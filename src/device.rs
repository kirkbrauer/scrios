//! Everything that touches the phone: usbmuxd, the `CoreDevice` tunnel, the display service and the
//! UDP sockets on the tunnel. The helpers that need no phone (address parsing, device choice, reading
//! the device's answer) are plain functions with tests.

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use idevice::{
    IdeviceError, IdeviceService, ReadWrite, RsdService,
    core_device::{
        ButtonState, CallInfoBlob, CoreDeviceServiceClient, DIGITIZER_SURFACE_MAIN_TOUCHSCREEN, DisplayServiceClient,
        HidSurface, IndigoHidClient, KeyboardUsage, MainKeyboardService, MediaNegotiationBlob,
        TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE, UniversalHidServiceClient, build_screen_audio_offer,
        build_start_audio_parameters, build_start_video_parameters,
    },
    core_device_proxy::CoreDeviceProxy,
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice},
};
use tracing::{debug, info, warn};

use crate::displayinfo::{DisplayInfo, DisplayInfoSource, DisplayQuery, parse_display_info};
use crate::hid::{Action, HidBackend};
use crate::offer::{OfferFlags, build_video_offer};
use crate::session::{Datagram, DatagramSocket};

/// The label usbmuxd sees for our connections.
const LABEL: &str = "scrios";
/// `clientSupportedFeatures` is what the controller supports (what the system's own screen-sharing
/// client sends), not the device's mask.
const CLIENT_SUPPORTED_FEATURES: u64 = 140;
/// The sender ports we hand the device in the start parameters. The device sends RTP from its
/// own source port; this is only the fallback RTCP destination.
const AUDIO_SENDER_PORT: u16 = 50000;
const VIDEO_SENDER_PORT: u16 = 50001;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Connecting a HID service while the stream already runs: short, so the device's 20 s without a
/// receiver report cannot run out meanwhile.
const HID_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the start waits for the display info before going on without it.
const DISPLAY_INFO_WAIT: Duration = Duration::from_secs(3);
/// A display query that has had no answer for this long is given up on, and the service with it.
const DISPLAY_QUERY_LIMIT: Duration = Duration::from_secs(10);
const DEVICE_INFO_SERVICE: &str = "com.apple.coredevice.deviceinfo";
const GET_DISPLAY_INFO: &str = "com.apple.coredevice.feature.getdisplayinfo";

#[derive(Debug, Clone)]
pub struct DeviceOptions {
    pub udid: Option<String>,
    pub usbmuxd: Option<String>,
    pub display_id: i64,
    /// Log the device's answer and its media support info (to find the stream's parameters).
    pub log_answer: bool,
    /// The offer's `allowRTCPFB` and long-term-reference settings.
    pub offer: OfferFlags,
}

/// What the device's answer told us about the stream (all optional: the first RTP packets confirm).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamInfo {
    /// `SourcePort` of the stream config: where the device sends RTP from, and listens for RTCP.
    pub source_port: Option<u16>,
    /// `LocalSSRC`: the device's SSRC.
    pub device_ssrc: Option<u32>,
    pub fps: Option<u32>,
    /// `rtp_sample_rate` of the negotiated HEVC payload.
    pub clock_hz: Option<u32>,
    pub rtcp_timeout_secs: Option<u32>,
}

// ---- pure helpers -----------------------------------------------------------------------------

/// The usbmuxd address to use when `--usbmuxd` is not given.
#[must_use]
pub fn default_usbmuxd_spec() -> String {
    match std::env::var("USBMUXD_SOCKET_ADDRESS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => UsbmuxdAddr::SOCKET_FILE.to_string(),
    }
}

/// `/path/to/socket` (or a relative `./path`) is a unix socket; `host:port` is TCP; a bare word
/// is taken as a socket path.
///
/// # Errors
///
/// A message for an empty address, a `host:port` whose port is not a number, and a host that does not resolve or has no
/// address.
pub fn parse_usbmuxd_addr(spec: &str) -> Result<UsbmuxdAddr, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty usbmuxd address".into());
    }
    let looks_like_path = spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../");
    if looks_like_path || !spec.contains(':') {
        return Ok(UsbmuxdAddr::UnixSocket(spec.to_string()));
    }
    if let Ok(a) = SocketAddr::from_str(spec) {
        return Ok(UsbmuxdAddr::TcpSocket(a));
    }
    // host:port with a name.
    let (host, port) = spec.rsplit_once(':').ok_or_else(|| format!("bad usbmuxd address {spec:?}"))?;
    port.parse::<u16>().map_err(|_| format!("bad port in usbmuxd address {spec:?}"))?;
    spec.to_socket_addrs()
        .map_err(|e| format!("cannot resolve usbmuxd host {host:?}: {e}"))?
        .next()
        .map(UsbmuxdAddr::TcpSocket)
        .ok_or_else(|| format!("usbmuxd host {host:?} has no address"))
}

/// Picks the device: the one named by `udid`, else the only one. A device that usbmuxd lists both
/// over USB and over the network is one device (USB wins).
///
/// # Errors
///
/// A message if no device is connected, none has that UDID (the message lists the ones usbmuxd knows), or several are
/// connected and no UDID tells them apart.
pub fn select_device<'a>(devices: &'a [UsbmuxdDevice], udid: Option<&str>) -> Result<&'a UsbmuxdDevice, String> {
    let candidates: Vec<&UsbmuxdDevice> = match udid {
        Some(u) => devices.iter().filter(|d| d.udid == u).collect(),
        None => devices.iter().collect(),
    };
    let mut udids: Vec<&str> = candidates.iter().map(|d| d.udid.as_str()).collect();
    udids.sort_unstable();
    udids.dedup();
    match udids.len() {
        0 => match udid {
            Some(u) => {
                let listed: Vec<&str> = devices.iter().map(|d| d.udid.as_str()).collect();
                Err(format!(
                    "no device with UDID {u} (usbmuxd lists: {})",
                    if listed.is_empty() { "nothing".to_string() } else { listed.join(", ") }
                ))
            }
            None => Err("no device is connected (usbmuxd lists none)".into()),
        },
        1 => Ok(candidates.iter().find(|d| d.connection_type == Connection::Usb).copied().unwrap_or(candidates[0])),
        _ => Err(format!("several devices are connected ({}): pass --udid", udids.join(", "))),
    }
}

/// A non-negative real as a whole number: the fraction is dropped, and a number too big for a `u64` is the largest.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a float-to-int `as` saturates; the caller has checked the sign"
)]
fn real_to_u64(r: f64) -> u64 {
    r as u64
}

/// A plist number, or a string of digits, or the `{"int": n}` wrapper `CoreDevice` uses.
fn value_u64(v: &plist::Value) -> Option<u64> {
    match v {
        plist::Value::Integer(_) => {
            v.as_unsigned_integer().or_else(|| v.as_signed_integer().and_then(|i| u64::try_from(i).ok()))
        }
        plist::Value::Real(r) if *r >= 0.0 => Some(real_to_u64(*r)),
        plist::Value::String(s) => s.trim().parse().ok(),
        plist::Value::Dictionary(d) if d.len() == 1 => d.iter().next().and_then(|(_, inner)| value_u64(inner)),
        _ => None,
    }
}

/// Finds `key` anywhere in `v`: in nested dictionaries and arrays, and inside binary plists carried
/// as data. First match, depth first.
fn find_key(v: &plist::Value, key: &str, depth: u32) -> Option<plist::Value> {
    if depth > 8 {
        return None;
    }
    match v {
        plist::Value::Dictionary(d) => {
            if let Some(found) = d.get(key) {
                return Some(found.clone());
            }
            d.iter().find_map(|(_, inner)| find_key(inner, key, depth + 1))
        }
        plist::Value::Array(a) => a.iter().find_map(|inner| find_key(inner, key, depth + 1)),
        plist::Value::Data(bytes) if bytes.starts_with(b"bplist") => {
            let inner: plist::Value = plist::from_bytes(bytes).ok()?;
            find_key(&inner, key, depth + 1)
        }
        _ => None,
    }
}

fn find_u32(v: &plist::Value, key: &str) -> Option<u32> {
    find_key(v, key, 0).as_ref().and_then(value_u64).and_then(|n| u32::try_from(n).ok())
}

/// The HEVC payload's `rtp_sample_rate` from the negotiation blob in the answer, if it says.
fn negotiated_clock_hz(answer: &plist::Value) -> Option<u32> {
    let plist::Value::Data(blob) = find_key(answer, "avcMediaStreamNegotiatorMediaBlob", 0)? else {
        return None;
    };
    // The wire form is zlib-compressed protobuf; take a raw protobuf only if it is not that.
    let parsed = MediaNegotiationBlob::from_negotiator_offer(&blob)
        .ok()
        .filter(|b| !b.stream_groups.is_empty())
        .unwrap_or_else(|| MediaNegotiationBlob::decode(&blob));
    parsed
        .stream_groups
        .iter()
        .flat_map(|g| g.payloads.iter())
        .find(|p| p.rtp_payload == u32::from(crate::engine::PAYLOAD_TYPE) && p.rtp_sample_rate > 0)
        .map(|p| p.rtp_sample_rate)
        .filter(|hz| (1000..=1_000_000).contains(hz))
}

/// Reads what the stream's parameters are out of the device's answer (or its server status, which
/// carries the same stream config).
#[must_use]
pub fn answer_info(answer: &plist::Value) -> StreamInfo {
    StreamInfo {
        source_port: find_u32(answer, "SourcePort").and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0),
        device_ssrc: find_u32(answer, "LocalSSRC").filter(|s| *s != 0),
        fps: find_u32(answer, "Framerate").filter(|f| (1..=240).contains(f)),
        clock_hz: negotiated_clock_hz(answer),
        rtcp_timeout_secs: find_u32(answer, "RTCPTimeoutInterval"),
    }
}

/// Words in a key that make its value worth showing when the answer is read for what it says of
/// the codec and the stream.
const SUMMARY_WORDS: [&str; 12] =
    ["payload", "codec", "ssrc", "port", "frame", "rtcp", "bitrate", "width", "height", "profile", "level", "ip"];

fn collect_summary(v: &plist::Value, path: &str, depth: u32, out: &mut Vec<String>) {
    if depth > 8 || out.len() >= 80 {
        return;
    }
    match v {
        plist::Value::Dictionary(d) => {
            for (k, inner) in d {
                let here = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                let lower = k.to_ascii_lowercase();
                let scalar =
                    !matches!(inner, plist::Value::Dictionary(_) | plist::Value::Array(_) | plist::Value::Data(_));
                if scalar && SUMMARY_WORDS.iter().any(|w| lower.contains(w)) {
                    out.push(format!("{here} = {inner:?}"));
                }
                collect_summary(inner, &here, depth + 1, out);
            }
        }
        plist::Value::Array(a) => {
            for (i, inner) in a.iter().enumerate() {
                collect_summary(inner, &format!("{path}[{i}]"), depth + 1, out);
            }
        }
        plist::Value::Data(bytes) if bytes.starts_with(b"bplist") => {
            if let Ok(inner) = plist::from_bytes::<plist::Value>(bytes) {
                collect_summary(&inner, &format!("{path}(plist)"), depth + 1, out);
            }
        }
        _ => {}
    }
}

/// What the negotiation blob in the answer says: for each stream group, its payloads (payload type,
/// codec type, sample rate, RTCP flags) and streams (SSRC).
fn negotiation_lines(answer: &plist::Value) -> Vec<String> {
    let Some(plist::Value::Data(blob)) = find_key(answer, "avcMediaStreamNegotiatorMediaBlob", 0) else {
        return vec!["the answer has no media negotiation blob".to_string()];
    };
    let parsed = MediaNegotiationBlob::from_negotiator_offer(&blob)
        .ok()
        .filter(|b| !b.stream_groups.is_empty())
        .unwrap_or_else(|| MediaNegotiationBlob::decode(&blob));
    if parsed.stream_groups.is_empty() {
        return vec!["the answer's media blob has no stream groups".to_string()];
    }
    let mut out = Vec::new();
    for (i, g) in parsed.stream_groups.iter().enumerate() {
        for p in &g.payloads {
            out.push(format!(
                "stream group {i} (type {}): payload type {}, codec type {}, sample rate {}, rtcp flags {}, media flags {}, profile level id {}",
                g.stream_group, p.rtp_payload, p.codec_type, p.rtp_sample_rate, p.rtcp_flags, p.media_flags, p.profile_level_id
            ));
        }
        for s in &g.streams {
            out.push(format!(
                "stream group {i}: stream ssrc {:#x}, max network bitrate {}, stream id {}",
                s.rtp_ssrc, s.max_network_bitrate, s.stream_id
            ));
        }
    }
    out
}

/// The parts of the device's answer that say which codec and stream it chose: the scalar values whose
/// key names a payload, codec, SSRC, port, frame rate, RTCP, bitrate, size, profile or level, and the
/// negotiation blob's payloads and streams.
#[must_use]
pub fn answer_summary(answer: &plist::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_summary(answer, "", 0, &mut out);
    out.extend(negotiation_lines(answer));
    out
}

impl StreamInfo {
    fn fill_from(&mut self, other: StreamInfo) {
        self.source_port = self.source_port.or(other.source_port);
        self.device_ssrc = self.device_ssrc.or(other.device_ssrc);
        self.fps = self.fps.or(other.fps);
        self.clock_hz = self.clock_hz.or(other.clock_hz);
        self.rtcp_timeout_secs = self.rtcp_timeout_secs.or(other.rtcp_timeout_secs);
    }
}

async fn timed<T, F: Future<Output = T>>(what: &str, limit: Duration, fut: F) -> Result<T, String> {
    tokio::time::timeout(limit, fut).await.map_err(|_| format!("timed out after {} s: {what}", limit.as_secs()))
}

// ---- display info ----------------------------------------------------------------------------

/// The `CoreDevice` `deviceinfo` service: read-only queries about the device.
#[derive(Debug)]
pub struct DeviceInfoClient {
    inner: CoreDeviceServiceClient<Box<dyn ReadWrite>>,
}

impl RsdService for DeviceInfoClient {
    fn rsd_service_name() -> Cow<'static, str> {
        Cow::Borrowed(DEVICE_INFO_SERVICE)
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> Result<Self, IdeviceError> {
        Ok(Self { inner: Box::pin(CoreDeviceServiceClient::new(stream)).await? })
    }
}

impl DeviceInfoClient {
    /// The device's displays (`getdisplayinfo`).
    ///
    /// # Errors
    ///
    /// A message if the request fails or the reply has no usable display (see [`parse_display_info`]).
    pub async fn get_display_info(&mut self) -> Result<Vec<DisplayInfo>, String> {
        let reply = Box::pin(self.inner.invoke_with_plist(GET_DISPLAY_INFO, plist::Dictionary::new()))
            .await
            .map_err(|e| format!("getdisplayinfo failed: {e:?}"))?;
        parse_display_info(&reply)
    }
}

/// Display queries over one `deviceinfo` connection, one at a time.
#[derive(Debug)]
pub struct DisplayInfoClient {
    inner: Arc<tokio::sync::Mutex<Option<DeviceInfoClient>>>,
}

impl DisplayInfoClient {
    fn new(client: Option<DeviceInfoClient>) -> Self {
        DisplayInfoClient { inner: Arc::new(tokio::sync::Mutex::new(client)) }
    }
}

impl DisplayInfoSource for DisplayInfoClient {
    fn query(&self) -> DisplayQuery {
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut guard = inner.lock().await;
            let Some(client) = guard.as_mut() else {
                return Err("the device info service is not available".to_string());
            };
            if let Ok(reply) = Box::pin(tokio::time::timeout(DISPLAY_QUERY_LIMIT, client.get_display_info())).await {
                reply
            } else {
                // An unanswered request leaves the connection out of step: do not use it again.
                *guard = None;
                Err(format!("no answer in {} s", DISPLAY_QUERY_LIMIT.as_secs()))
            }
        })
    }
}

// ---- the stream -------------------------------------------------------------------------------

/// The tunnel's UDP socket for the video stream.
#[derive(Debug)]
pub struct VideoSocket(UdpSocketHandle);

impl DatagramSocket for VideoSocket {
    // The trait asks for a `Send` future; spelled out so that the bound is checked here.
    #[allow(clippy::manual_async_fn)]
    fn recv(&self) -> impl Future<Output = io::Result<Datagram>> + Send {
        async move {
            let dg = self.0.recv().await?;
            Ok(Datagram { src_port: dg.source_port, data: dg.data })
        }
    }

    fn send_to(&self, port: u16, data: Vec<u8>) -> impl Future<Output = io::Result<()>> + Send {
        self.0.send_to(port, data)
    }
}

/// A display stream, from the first connection to the last cleanup.
///
/// The caller owns it, so it can be cleaned up whatever happens to [`Session::start`]: the future
/// may be dropped half way (a stop arrived while starting) or fail, and [`Session::shutdown`] still
/// stops whatever was started on the device and closes the tunnel.
#[derive(Default)]
pub struct Session {
    client: Option<DisplayServiceClient<Box<dyn ReadWrite>>>,
    adapter: Option<AdapterHandle>,
    video: Option<VideoSocket>,
    audio_drain: Option<tokio::task::JoinHandle<()>>,
    /// A stream was (or may have been) requested from the device: `stopMediaStream` is due.
    stream_requested: bool,
    /// The SSRC the offer declared.
    pub our_ssrc: u32,
    pub info: StreamInfo,
    /// The service list of the tunnel, kept for connecting more services to it.
    handshake: Option<RsdHandshake>,
    display: Option<DisplayInfoClient>,
    /// The answer to the display query made at start, if it came in time.
    first_display: Option<Result<Vec<DisplayInfo>, String>>,
    /// That query, if it is still waiting for its answer.
    pending_display: Option<DisplayQuery>,
}

// By hand: a query in flight is a `dyn Future`, which has no `Debug`.
impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("client", &self.client)
            .field("adapter", &self.adapter)
            .field("video", &self.video)
            .field("audio_drain", &self.audio_drain)
            .field("stream_requested", &self.stream_requested)
            .field("our_ssrc", &self.our_ssrc)
            .field("info", &self.info)
            .field("handshake", &self.handshake)
            .field("display", &self.display)
            .field("first_display", &self.first_display)
            .field("pending_display", &self.pending_display.as_ref().map(|_| "a query in flight"))
            .finish()
    }
}

impl Session {
    /// Where RTCP goes if neither the answer nor an RTP packet says: the sender port we named in
    /// the start parameters.
    pub const FALLBACK_RTCP_PORT: u16 = VIDEO_SENDER_PORT;

    /// The video socket, once the stream is up.
    pub fn video(&self) -> Option<&VideoSocket> {
        self.video.as_ref()
    }

    /// Where display queries go (the device has the service).
    pub fn display_source(&self) -> Option<&dyn DisplayInfoSource> {
        self.display.as_ref().map(|d| d as &dyn DisplayInfoSource)
    }

    /// The answer to the query made at start, if it arrived within the wait.
    pub fn take_first_display(&mut self) -> Option<Result<Vec<DisplayInfo>, String>> {
        self.first_display.take()
    }

    /// The query made at start, if it is still in flight (the answer was slow).
    pub fn take_pending_display(&mut self) -> Option<DisplayQuery> {
        self.pending_display.take()
    }

    /// Opens the tunnel and starts the display stream.
    ///
    /// usbmuxd and the pair record are only read: nothing is paired, mounted or written. The device
    /// gets an audio stream first (it needs one to establish the screen-sharing session), then the
    /// video stream on the same session, with an offer that declares our SSRC.
    ///
    /// # Errors
    ///
    /// A message that says which step failed or timed out: connecting to usbmuxd, picking the device, opening the tunnel,
    /// the RSD handshake, connecting the display service, or starting the audio or video stream. Whatever was
    /// started is stopped by [`Session::shutdown`], which the caller calls after a failure too.
    pub async fn start(&mut self, opts: &DeviceOptions) -> Result<(), String> {
        let spec = opts.usbmuxd.clone().unwrap_or_else(default_usbmuxd_spec);
        let addr = parse_usbmuxd_addr(&spec)?;
        info!("connecting to usbmuxd at {spec}");
        let mut mux = timed("connecting to usbmuxd", CONNECT_TIMEOUT, addr.connect(1))
            .await?
            .map_err(|e| format!("cannot connect to usbmuxd at {spec}: {e:?}"))?;
        let devices = timed("listing devices", CONNECT_TIMEOUT, mux.get_devices())
            .await?
            .map_err(|e| format!("usbmuxd device list failed: {e:?}"))?;
        drop(mux);
        let dev = select_device(&devices, opts.udid.as_deref())?;
        info!("using device {} ({:?})", dev.udid, dev.connection_type);
        let provider = dev.to_provider(addr, LABEL);

        let proxy = timed("opening the CoreDevice tunnel", CONNECT_TIMEOUT, CoreDeviceProxy::connect(&provider))
            .await?
            .map_err(|e| format!("cannot open the CoreDevice tunnel: {e:?}"))?;
        let rsd_port = proxy.tunnel_info().server_rsd_port;
        let adapter = proxy.create_software_tunnel().map_err(|e| format!("cannot create the tunnel adapter: {e:?}"))?;
        let mut adapter = adapter.to_async_handle();
        self.adapter = Some(adapter.clone());
        let stream = timed("connecting to RSD", CONNECT_TIMEOUT, adapter.connect(rsd_port))
            .await?
            .map_err(|e| format!("cannot connect to the RSD port {rsd_port}: {e}"))?;
        let mut handshake = Box::pin(timed("the RSD handshake", CONNECT_TIMEOUT, RsdHandshake::new(stream)))
            .await?
            .map_err(|e| format!("RSD handshake failed: {e:?}"))?;
        let client: DisplayServiceClient<Box<dyn ReadWrite>> = Box::pin(timed(
            "connecting the display service",
            CONNECT_TIMEOUT,
            DisplayServiceClient::connect_rsd(&mut adapter, &mut handshake),
        ))
        .await?
        .map_err(|e| {
            format!("this device has no display service (needs iOS 27 and a mounted developer image): {e:?}")
        })?;
        let client = self.client.insert(client);

        // The display's size and orientation, best effort: a slow or missing answer must not hold
        // the stream up (a slow one arrives later, a missing one means --visible-size or the SPS).
        let started = Box::pin(connect_display_info(&mut adapter, &mut handshake)).await;
        self.display = started.source;
        self.first_display = started.first;
        self.pending_display = started.pending;

        let audio_udp = adapter.bind_udp(0).await.map_err(|e| format!("cannot bind the audio socket: {e}"))?;
        let video_udp = adapter.bind_udp(0).await.map_err(|e| format!("cannot bind the video socket: {e}"))?;
        let receiver_ip = adapter.host_ip().to_string();
        let sender_ip = adapter.peer_ip().to_string();
        let audio_port = audio_udp.local_port();
        let video_port = video_udp.local_port();
        info!("video receiver {receiver_ip}:{video_port}, device {sender_ip}");

        // The endpoint description the system's own screen-sharing client sends.
        let call_info = CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        };
        let session_id = uuid::Uuid::new_v4();

        // The audio stream first, to establish the session.
        let audio_call_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let audio_offer = build_screen_audio_offer(&audio_call_id, &call_info)
            .map_err(|e| format!("cannot build the audio offer: {e:?}"))?;
        let audio_params = build_start_audio_parameters(
            &receiver_ip,
            audio_port,
            &sender_ip,
            AUDIO_SENDER_PORT,
            audio_offer,
            CLIENT_SUPPORTED_FEATURES,
            session_id,
        );
        // From here on the device may hold a stream for us, whatever happens to this call.
        self.stream_requested = true;
        Box::pin(timed("starting the audio stream", START_TIMEOUT, client.start_media_stream(audio_params)))
            .await?
            .map_err(|e| format!("the audio stream did not start: {e:?}"))?;
        // Nothing reads the audio; drain it so the tunnel's queue for that socket cannot grow.
        self.audio_drain = Some(tokio::spawn(async move { while audio_udp.recv().await.is_ok() {} }));

        // Then the video, on the same session, with our SSRC in the offer.
        self.our_ssrc = fresh_ssrc();
        let call_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let offer = build_video_offer(&call_id, &call_info, self.our_ssrc, opts.offer)?;
        info!(
            "offer: allowRTCPFB {}, long-term references {}",
            u8::from(opts.offer.allow_rtcp_fb),
            if opts.offer.ltr { "on" } else { "off" }
        );
        let params = build_start_video_parameters(
            &receiver_ip,
            video_port,
            &sender_ip,
            VIDEO_SENDER_PORT,
            offer,
            CLIENT_SUPPORTED_FEATURES,
            opts.display_id,
            session_id,
        );
        let answer = Box::pin(timed("starting the video stream", START_TIMEOUT, client.start_media_stream(params)))
            .await?
            .map_err(|e| format!("the video stream did not start: {e:?}"))?;
        info!("video stream started (session {session_id}, our SSRC {:#x})", self.our_ssrc);

        let mut info_from = answer_info(&answer);
        if opts.log_answer {
            info!("answer: {answer:?}");
            for line in answer_summary(&answer) {
                info!("answer says: {line}");
            }
        }
        if info_from.source_port.is_none() || info_from.device_ssrc.is_none() {
            // The same stream config is in the server status while the session is active.
            match Box::pin(timed("the server status", STOP_TIMEOUT, client.get_media_stream_server_status())).await {
                Ok(Ok(status)) => {
                    if opts.log_answer {
                        info!("server status: {status:?}");
                        for line in answer_summary(&status) {
                            info!("server status says: {line}");
                        }
                    }
                    info_from.fill_from(answer_info(&status));
                }
                Ok(Err(e)) => debug!("server status unavailable: {e:?}"),
                Err(e) => debug!("{e}"),
            }
        }
        if opts.log_answer {
            match Box::pin(timed("media support info", STOP_TIMEOUT, client.get_media_support_info())).await {
                Ok(Ok(v)) => info!("media support info: {v:?}"),
                Ok(Err(e)) => warn!("media support info unavailable: {e:?}"),
                Err(e) => warn!("{e}"),
            }
        }
        info!("stream parameters from the device: {info_from:?}");
        self.info = info_from;
        self.video = Some(VideoSocket(video_udp));
        self.handshake = Some(handshake);
        Ok(())
    }

    /// Connects the HID services (touch, and the hardware buttons) on the tunnel the stream
    /// already uses. HID only works while a display stream is open, which is this one.
    pub async fn connect_hid(&mut self) -> DeviceHid {
        let mut hid = DeviceHid::default();
        let (Some(adapter), Some(handshake)) = (self.adapter.as_mut(), self.handshake.as_mut()) else {
            hid.touch_error = Some("the stream is not up".into());
            return hid;
        };
        match Box::pin(timed(
            "connecting the touch service",
            HID_CONNECT_TIMEOUT,
            UniversalHidServiceClient::connect_rsd(adapter, handshake),
        ))
        .await
        {
            Ok(Ok(mut c)) => {
                // What the device says its HID surfaces are, to check the touch mapping against.
                match Box::pin(timed("listing the HID surfaces", HID_CONNECT_TIMEOUT, c.list_connected_services()))
                    .await
                {
                    Ok(Ok(surfaces)) => {
                        for s in &surfaces {
                            info!(
                                "HID surface {}: product {:?}, primary usage page {}, usage {}",
                                s.service_id,
                                s.product,
                                fmt_hex(s.primary_usage_page),
                                fmt_hex(s.primary_usage)
                            );
                        }
                        hid.warnings = surface_warnings(&surfaces);
                        hid.surfaces = surfaces;
                    }
                    Ok(Err(e)) => warn!("the HID surfaces could not be listed: {e:?}"),
                    Err(e) => warn!("{e}"),
                }
                // The virtual keyboard: created once here, taken down when the session ends. Touch and
                // buttons work without it.
                match Box::pin(timed("creating the keyboard", HID_CONNECT_TIMEOUT, c.create_main_keyboard())).await {
                    Ok(Ok(kb)) => hid.keyboard = Some(kb),
                    Ok(Err(e)) => hid.keyboard_error = Some(format!("the keyboard could not be created: {e}")),
                    Err(e) => hid.keyboard_error = Some(e),
                }
                hid.touch = Some(c);
            }
            Ok(Err(e)) => hid.touch_error = Some(format!("the touch service did not open: {e:?}")),
            Err(e) => hid.touch_error = Some(e),
        }
        match Box::pin(timed(
            "connecting the button service",
            HID_CONNECT_TIMEOUT,
            IndigoHidClient::connect_rsd(adapter, handshake),
        ))
        .await
        {
            Ok(Ok(c)) => hid.buttons = Some(c),
            Ok(Err(e)) => hid.buttons_error = Some(format!("the button service did not open: {e:?}")),
            Err(e) => hid.buttons_error = Some(e),
        }
        hid
    }

    /// Stops the stream on the device (if one was asked for) and closes the tunnel. Safe to call
    /// at any point, after a failed or abandoned start too, and more than once.
    pub async fn shutdown(&mut self) {
        if self.stream_requested {
            self.stream_requested = false;
            if let Some(client) = self.client.as_mut() {
                // stopAll: the audio stream goes too.
                match Box::pin(timed("stopMediaStream", STOP_TIMEOUT, client.stop_media_stream())).await {
                    Ok(Ok(_)) => info!("media stream stopped"),
                    Ok(Err(e)) => warn!("stopMediaStream failed: {e:?}"),
                    Err(e) => warn!("{e}"),
                }
            }
        }
        if let Some(h) = self.audio_drain.take() {
            h.abort();
        }
        self.video = None;
        self.client = None;
        self.display = None;
        self.pending_display = None;
        self.handshake = None;
        if let Some(mut adapter) = self.adapter.take()
            && let Err(e) = adapter.close().await
        {
            debug!("closing the tunnel: {e}");
        }
    }
}

/// The device's HID services: touch reports and hardware buttons.
#[derive(Debug, Default)]
pub struct DeviceHid {
    touch: Option<UniversalHidServiceClient<Box<dyn ReadWrite>>>,
    buttons: Option<IndigoHidClient<Box<dyn ReadWrite>>>,
    /// The virtual keyboard created on the universal HID service (it uses the same client as touch).
    keyboard: Option<MainKeyboardService>,
    pub touch_error: Option<String>,
    pub buttons_error: Option<String>,
    pub keyboard_error: Option<String>,
    /// The HID surfaces the device lists (id, product, usage: the crate reads nothing else of them).
    pub surfaces: Vec<HidSurface>,
    /// Where those surfaces differ from what the touch path expects.
    pub warnings: Vec<String>,
}

fn fmt_hex(v: Option<u64>) -> String {
    v.map_or_else(|| "?".to_string(), |v| format!("{v:#x}"))
}

/// What the listed HID surfaces say against what is assumed: touch reports go to the surface
/// [`DIGITIZER_SURFACE_MAIN_TOUCHSCREEN`]. The listing has no orientation or size to check the
/// mapping against (the crate reads only id, product and usage); a missing surface is worth a warning.
#[must_use]
pub fn surface_warnings(surfaces: &[HidSurface]) -> Vec<String> {
    let mut out = Vec::new();
    if surfaces.is_empty() {
        out.push("the device lists no HID surface".to_string());
    } else if !surfaces.iter().any(|s| s.service_id == DIGITIZER_SURFACE_MAIN_TOUCHSCREEN) {
        out.push(format!(
            "the device lists no touchscreen surface with id {DIGITIZER_SURFACE_MAIN_TOUCHSCREEN} (it lists: {})",
            surfaces
                .iter()
                .map(|s| format!("{} {:?}", s.service_id, s.product.as_deref().unwrap_or("?")))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

impl DeviceHid {
    /// Touch works (the buttons may not).
    #[must_use]
    pub fn ready(&self) -> bool {
        self.touch.is_some()
    }

    /// The keyboard was created and can be used.
    #[must_use]
    pub fn keyboard_ready(&self) -> bool {
        self.keyboard.is_some()
    }
}

/// A keyboard failure as a message.
fn kb_error(e: impl std::fmt::Display) -> String {
    format!("keyboard: {e}")
}

impl HidBackend for DeviceHid {
    fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
        Box::pin(async move {
            match action {
                Action::Touch { down, x, y } => {
                    let Some(client) = self.touch.as_mut() else {
                        return Err(self
                            .touch_error
                            .clone()
                            .unwrap_or_else(|| "the touch service is not connected".into()));
                    };
                    // One report type for a finger on the screen (0xC2) and one for its lift (0x02);
                    // a move is another contact sample. The timestamp is the wall clock.
                    let state = if down { TOUCHSCREEN_STATE_CONTACT } else { TOUCHSCREEN_STATE_RELEASE };
                    Box::pin(client.send_touchscreen(state, x, y, None))
                        .await
                        .map_err(|e| format!("touch report failed: {e:?}"))
                }
                Action::Key { usage, down } => {
                    let (Some(client), Some(kb)) = (self.touch.as_mut(), self.keyboard.as_mut()) else {
                        return Err(self
                            .keyboard_error
                            .clone()
                            .unwrap_or_else(|| "the keyboard is not connected".into()));
                    };
                    let usage = KeyboardUsage::new(u16::from(usage)).map_err(kb_error)?;
                    if down {
                        Box::pin(client.main_keyboard_key_down(kb, usage)).await.map_err(kb_error)
                    } else {
                        Box::pin(client.main_keyboard_key_up(kb, usage)).await.map_err(kb_error)
                    }
                }
                Action::KeyboardRelease => {
                    let (Some(client), Some(kb)) = (self.touch.as_mut(), self.keyboard.as_mut()) else {
                        return Ok(()); // no keyboard: nothing is down
                    };
                    // An empty report lets go of every key (the device's reset alone is not known to).
                    let emptied = Box::pin(client.set_main_keyboard_usages(kb, [])).await.map_err(kb_error);
                    let reset = Box::pin(client.reset_main_keyboard(kb)).await.map_err(kb_error);
                    emptied.and(reset)
                }
                Action::KeyboardRemove => {
                    let (Some(client), Some(kb)) = (self.touch.as_mut(), self.keyboard.as_mut()) else {
                        return Ok(());
                    };
                    let result = Box::pin(client.remove_main_keyboard(kb)).await.map_err(kb_error);
                    if result.is_ok() {
                        self.keyboard = None;
                    }
                    result
                }
                Action::Button { page, code, down } => {
                    let Some(client) = self.buttons.as_mut() else {
                        return Err(self
                            .buttons_error
                            .clone()
                            .unwrap_or_else(|| "the button service is not connected".into()));
                    };
                    let state = if down { ButtonState::Down } else { ButtonState::Up };
                    Box::pin(client.send_button(page, code, state))
                        .await
                        .map_err(|e| format!("button event failed: {e:?}"))
                }
            }
        })
    }
}

/// The result of the display info set-up at start.
#[derive(Default)]
struct DisplayStart {
    source: Option<DisplayInfoClient>,
    first: Option<Result<Vec<DisplayInfo>, String>>,
    pending: Option<DisplayQuery>,
}

/// Opens the `deviceinfo` service and makes the first display query, waiting for it for a few
/// seconds at most. Failures are logged, not returned: the size then comes from `--visible-size`
/// or the SPS.
async fn connect_display_info(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> DisplayStart {
    let service = match Box::pin(timed(
        "connecting the device info service",
        CONNECT_TIMEOUT,
        DeviceInfoClient::connect_rsd(adapter, handshake),
    ))
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            warn!(
                "no display info (the device info service did not open: {e:?}): the picture size comes from --visible-size or the SPS"
            );
            return DisplayStart::default();
        }
        Err(e) => {
            warn!("no display info ({e}): the picture size comes from --visible-size or the SPS");
            return DisplayStart::default();
        }
    };
    let source = DisplayInfoClient::new(Some(service));
    let mut query = source.query();
    let mut out = DisplayStart::default();
    if let Ok(reply) = tokio::time::timeout(DISPLAY_INFO_WAIT, &mut query).await {
        out.first = Some(reply);
    } else {
        warn!("the display info is slow ({} s): going on without it, it will follow", DISPLAY_INFO_WAIT.as_secs());
        out.pending = Some(query);
    }
    out.source = Some(source);
    out
}

/// A non-zero random SSRC.
fn fresh_ssrc() -> u32 {
    loop {
        // The low 32 bits of a random UUID.
        let [.., a, b, c, d] = *uuid::Uuid::new_v4().as_bytes();
        let s = u32::from_be_bytes([a, b, c, d]);
        if s != 0 {
            return s;
        }
    }
}

#[cfg(test)]
mod tests {
    use idevice::core_device::display_stream::negotiation::{StreamGroup, StreamGroupPayload};

    use super::*;

    fn dev(udid: &str, conn: Connection, id: u32) -> UsbmuxdDevice {
        UsbmuxdDevice { connection_type: conn, udid: udid.into(), device_id: id }
    }

    #[test]
    fn usbmuxd_addresses() {
        assert!(matches!(
            parse_usbmuxd_addr("/var/run/usbmuxd"),
            Ok(UsbmuxdAddr::UnixSocket(p)) if p == "/var/run/usbmuxd"
        ));
        assert!(matches!(parse_usbmuxd_addr("./run/usbmuxd"), Ok(UsbmuxdAddr::UnixSocket(_))));
        assert!(matches!(parse_usbmuxd_addr("usbmuxd.sock"), Ok(UsbmuxdAddr::UnixSocket(_))));
        match parse_usbmuxd_addr("127.0.0.1:27015") {
            Ok(UsbmuxdAddr::TcpSocket(a)) => assert_eq!(a.to_string(), "127.0.0.1:27015"),
            other => panic!("{other:?}"),
        }
        match parse_usbmuxd_addr("[::1]:27015") {
            Ok(UsbmuxdAddr::TcpSocket(a)) => assert_eq!(a.port(), 27015),
            other => panic!("{other:?}"),
        }
        match parse_usbmuxd_addr("localhost:27015") {
            Ok(UsbmuxdAddr::TcpSocket(a)) => assert_eq!(a.port(), 27015),
            other => panic!("{other:?}"),
        }
        assert!(parse_usbmuxd_addr("").is_err());
        assert!(parse_usbmuxd_addr("host:notaport").is_err());
    }

    #[test]
    fn the_only_device_is_chosen_and_usb_beats_network_for_one_udid() {
        let one = [dev("AAA", Connection::Usb, 1)];
        assert_eq!(select_device(&one, None).unwrap().device_id, 1);
        assert_eq!(select_device(&one, Some("AAA")).unwrap().device_id, 1);

        let both = [dev("AAA", Connection::Network("10.0.0.2".parse().unwrap()), 7), dev("AAA", Connection::Usb, 8)];
        assert_eq!(select_device(&both, None).unwrap().device_id, 8);
    }

    #[test]
    fn device_selection_errors_say_what_to_do() {
        assert!(select_device(&[], None).unwrap_err().contains("no device"));
        let two = [dev("AAA", Connection::Usb, 1), dev("BBB", Connection::Usb, 2)];
        let e = select_device(&two, None).unwrap_err();
        assert!(e.contains("--udid") && e.contains("AAA") && e.contains("BBB"), "{e}");
        assert_eq!(select_device(&two, Some("BBB")).unwrap().device_id, 2);
        let e = select_device(&two, Some("CCC")).unwrap_err();
        assert!(e.contains("CCC") && e.contains("AAA"), "{e}");
    }

    fn dict(entries: Vec<(&str, plist::Value)>) -> plist::Value {
        let mut d = plist::Dictionary::new();
        for (k, v) in entries {
            d.insert(k.to_string(), v);
        }
        plist::Value::Dictionary(d)
    }

    fn int(n: i64) -> plist::Value {
        plist::Value::Integer(n.into())
    }

    #[test]
    fn the_stream_config_is_found_wherever_it_is_nested() {
        let answer = dict(vec![
            ("something", int(1)),
            (
                "output",
                dict(vec![(
                    "streamConfig",
                    dict(vec![
                        ("SourcePort", int(50001)),
                        ("LocalSSRC", int(3_000_000_001)),
                        ("RemoteSSRC", int(5)),
                        ("Framerate", int(60)),
                        ("RTCPTimeoutInterval", int(20)),
                    ]),
                )]),
            ),
        ]);
        let info = answer_info(&answer);
        assert_eq!(info.source_port, Some(50001));
        assert_eq!(info.device_ssrc, Some(3_000_000_001));
        assert_eq!(info.fps, Some(60));
        assert_eq!(info.rtcp_timeout_secs, Some(20));
        assert_eq!(info.clock_hz, None);
    }

    #[test]
    fn numbers_may_be_wrapped_or_strings_and_a_config_may_sit_in_a_nested_plist() {
        let inner = dict(vec![
            ("SourcePort", dict(vec![("int", int(40000))])),
            ("LocalSSRC", plist::Value::String("123456".into())),
        ]);
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &inner).unwrap();
        assert!(bytes.starts_with(b"bplist"));
        let answer = dict(vec![("blob", plist::Value::Data(bytes))]);
        let info = answer_info(&answer);
        assert_eq!(info.source_port, Some(40000));
        assert_eq!(info.device_ssrc, Some(123_456));
    }

    #[test]
    fn nonsense_values_are_ignored() {
        let answer = dict(vec![("SourcePort", int(0)), ("LocalSSRC", int(-4)), ("Framerate", int(100_000))]);
        assert_eq!(answer_info(&answer), StreamInfo::default());
        assert_eq!(answer_info(&plist::Value::Boolean(true)), StreamInfo::default());
    }

    fn blob_with_rate(rate: u32) -> MediaNegotiationBlob {
        let mut blob = MediaNegotiationBlob::default();
        let mut group = StreamGroup::default();
        group.payloads.push(StreamGroupPayload { rtp_payload: 100, rtp_sample_rate: rate, ..Default::default() });
        blob.stream_groups.push(group);
        blob
    }

    #[test]
    fn the_clock_rate_comes_from_the_negotiation_blob_raw_or_compressed() {
        let raw = blob_with_rate(24_000).encode();
        let answer = dict(vec![("avcMediaStreamNegotiatorMediaBlob", plist::Value::Data(raw))]);
        assert_eq!(answer_info(&answer).clock_hz, Some(24_000));

        let zipped = blob_with_rate(90_000).to_negotiator_offer().unwrap();
        let answer =
            dict(vec![("answer", dict(vec![("avcMediaStreamNegotiatorMediaBlob", plist::Value::Data(zipped))]))]);
        assert_eq!(answer_info(&answer).clock_hz, Some(90_000));

        // A rate of zero says nothing.
        let zero = dict(vec![("avcMediaStreamNegotiatorMediaBlob", plist::Value::Data(blob_with_rate(0).encode()))]);
        assert_eq!(answer_info(&zero).clock_hz, None);
    }

    #[test]
    fn fill_from_keeps_what_is_already_known() {
        let mut a = StreamInfo { source_port: Some(1), ..Default::default() };
        a.fill_from(StreamInfo { source_port: Some(2), device_ssrc: Some(9), ..Default::default() });
        assert_eq!((a.source_port, a.device_ssrc), (Some(1), Some(9)));
    }

    fn surface(id: u64, product: Option<&str>) -> HidSurface {
        HidSurface {
            service_id: id,
            product: product.map(String::from),
            primary_usage: Some(4),
            primary_usage_page: Some(0x0D),
        }
    }

    #[test]
    fn surfaces_without_the_touchscreen_are_a_warning_not_an_error() {
        assert!(surface_warnings(&[surface(257, Some("CoreDevice touchscreen(nil)")), surface(1281, None)]).is_empty());
        let w = surface_warnings(&[surface(1281, Some("gesture")), surface(7, None)]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("257") && w[0].contains("1281") && w[0].contains("gesture"), "{w:?}");
        assert!(surface_warnings(&[])[0].contains("no HID surface"));
    }

    #[test]
    fn the_answer_summary_picks_out_the_codec_and_stream_values() {
        use idevice::core_device::display_stream::negotiation::{StreamGroup, StreamGroupPayload, StreamGroupStream};
        let mut blob = MediaNegotiationBlob::default();
        let mut group = StreamGroup { stream_group: 3, ..Default::default() };
        group.payloads.push(StreamGroupPayload {
            rtp_payload: 123,
            codec_type: 2,
            rtp_sample_rate: 90_000,
            rtcp_flags: 1,
            ..Default::default()
        });
        group.streams.push(StreamGroupStream { rtp_ssrc: 0xABCD, ..Default::default() });
        blob.stream_groups.push(group);
        let answer = dict(vec![
            ("something", int(1)),
            (
                "streamConfig",
                dict(vec![
                    ("PayloadType", int(123)),
                    ("SourcePort", int(50001)),
                    ("LocalSSRC", int(7)),
                    ("Framerate", int(60)),
                    ("RTCPTimeoutInterval", int(20)),
                    ("Unrelated", int(9)),
                    ("Codec", plist::Value::String("H264".into())),
                ]),
            ),
            ("avcMediaStreamNegotiatorMediaBlob", plist::Value::Data(blob.to_negotiator_offer().unwrap())),
        ]);
        let lines = answer_summary(&answer);
        let text = lines.join("\n");
        for want in [
            "streamConfig.PayloadType = ",
            "streamConfig.SourcePort",
            "streamConfig.LocalSSRC",
            "streamConfig.Framerate",
            "streamConfig.RTCPTimeoutInterval",
            "streamConfig.Codec = String(\"H264\")",
        ] {
            assert!(text.contains(want), "{want} missing from {text}");
        }
        assert!(!text.contains("Unrelated") && !text.contains("something"));
        assert!(text.contains("payload type 123, codec type 2, sample rate 90000, rtcp flags 1"), "{text}");
        assert!(text.contains("stream ssrc 0xabcd"), "{text}");
        // Nothing to say is still an answer.
        assert_eq!(answer_summary(&plist::Value::Boolean(true)), vec!["the answer has no media negotiation blob"]);
    }

    #[tokio::test]
    async fn a_missing_service_answers_queries_with_an_error() {
        let source = DisplayInfoClient::new(None);
        let err = source.query().await.unwrap_err();
        assert!(err.contains("not available"), "{err}");
    }

    #[test]
    fn fresh_ssrcs_are_never_zero() {
        for _ in 0..1000 {
            assert_ne!(fresh_ssrc(), 0);
        }
    }
}
