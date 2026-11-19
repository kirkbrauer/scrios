//! usbmuxd: scrios's only way to a device, the way adb is scrcpy's.
//!
//! Everything that reaches a device goes through usbmuxd: the device list, the pair record (usbmuxd's
//! `ReadPairRecord`: scrios reads no lockdown directory of the host), the lockdown and service connections, and
//! the `CoreDevice` tunnel. [`Mux`] is the address of the usbmuxd to ask, resolved in one order for every command:
//!
//! 1. the address given (`--usbmuxd`),
//! 2. else `$USBMUXD_SOCKET_ADDRESS`,
//! 3. else the platform's own: `/var/run/usbmuxd` on unix, `127.0.0.1:27015` on Windows.
//!
//! An address is a unix socket (`UNIX:/path` as libimobiledevice writes it, or a bare `/path`) or a TCP endpoint
//! (`host:port`, or `TCP:host:port`).

use std::net::{SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::time::Duration;

use idevice::IdeviceError;
use idevice::provider::UsbmuxdProvider;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice};

/// How long connecting to usbmuxd and asking it one thing may take.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The environment variable libimobiledevice, go-ios and idevice read for the address.
pub const ADDRESS_ENV: &str = "USBMUXD_SOCKET_ADDRESS";

/// Where a usbmuxd is: the address, and the text it was given as (which is what messages show).
#[derive(Debug, Clone)]
pub struct Mux {
    addr: UsbmuxdAddr,
    spec: String,
}

/// What usbmuxd says of the pair record of a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairLookup {
    /// usbmuxd holds a pair record for the device, and it can be read.
    Present,
    /// usbmuxd holds none: the host has not been trusted by the device.
    Missing,
    /// usbmuxd answered with something that is not a pair record (the reason).
    Unreadable(String),
}

/// The text of an address, as `--usbmuxd` would take it.
#[must_use]
pub fn describe(addr: &UsbmuxdAddr) -> String {
    match addr {
        #[cfg(unix)]
        UsbmuxdAddr::UnixSocket(path) => path.clone(),
        UsbmuxdAddr::TcpSocket(a) => a.to_string(),
    }
}

/// `UNIX:/path` and a bare `/path` (or `./path`, or a word without a colon) are a unix socket; `host:port` and
/// `TCP:host:port` are TCP. The prefixes are not case sensitive.
///
/// # Errors
///
/// A message for an empty address, a unix socket on a platform without them, a `host:port` whose port is not a
/// number, and a host that does not resolve or has no address.
pub fn parse_usbmuxd_addr(spec: &str) -> Result<UsbmuxdAddr, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty usbmuxd address".into());
    }
    if let Some(path) = after_prefix(spec, "unix:") {
        return unix_socket(path.trim());
    }
    if let Some(hostport) = after_prefix(spec, "tcp:") {
        return tcp_socket(hostport.trim());
    }
    let looks_like_path = spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../");
    if looks_like_path || !spec.contains(':') {
        return unix_socket(spec);
    }
    tcp_socket(spec)
}

/// What follows `prefix` in `text`, if `text` starts with it (ASCII case ignored).
fn after_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| text.get(prefix.len()..)).flatten()
}

#[cfg(unix)]
fn unix_socket(path: &str) -> Result<UsbmuxdAddr, String> {
    if path.is_empty() {
        return Err("empty unix socket path in the usbmuxd address".into());
    }
    Ok(UsbmuxdAddr::UnixSocket(path.to_string()))
}

#[cfg(not(unix))]
fn unix_socket(path: &str) -> Result<UsbmuxdAddr, String> {
    Err(format!("{path:?} is a unix socket, which this platform does not have: give a host:port"))
}

fn tcp_socket(hostport: &str) -> Result<UsbmuxdAddr, String> {
    if let Ok(a) = SocketAddr::from_str(hostport) {
        return Ok(UsbmuxdAddr::TcpSocket(a));
    }
    // host:port with a name.
    let (host, port) = hostport.rsplit_once(':').ok_or_else(|| format!("bad usbmuxd address {hostport:?}"))?;
    port.parse::<u16>().map_err(|_| format!("bad port in usbmuxd address {hostport:?}"))?;
    hostport
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve usbmuxd host {host:?}: {e}"))?
        .next()
        .map(UsbmuxdAddr::TcpSocket)
        .ok_or_else(|| format!("usbmuxd host {host:?} has no address"))
}

/// The forms of usbmuxd address this platform takes, for a message: a unix socket is only on unix.
#[must_use]
pub fn address_forms() -> &'static str {
    if cfg!(unix) { "UNIX:/path, /path, host:port or TCP:host:port" } else { "host:port or TCP:host:port" }
}

/// What to try when usbmuxd cannot be reached, for this platform.
#[must_use]
pub fn usbmuxd_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "usbmuxd is part of macOS: check the address (--usbmuxd, $USBMUXD_SOCKET_ADDRESS) if you gave one"
    } else if cfg!(windows) {
        "install the Apple Devices app or iTunes (their Apple Mobile Device Service is usbmuxd), or give --usbmuxd host:port"
    } else {
        "start the usbmuxd service (for example `sudo systemctl start usbmuxd`), or point --usbmuxd at a remote one"
    }
}

impl Mux {
    /// The usbmuxd to ask: `explicit` (the `--usbmuxd` option) if there is one, else `$USBMUXD_SOCKET_ADDRESS` if it
    /// is set and not empty, else the platform's own.
    ///
    /// # Errors
    ///
    /// A message if the address cannot be parsed (see [`parse_usbmuxd_addr`]); one that comes from the
    /// environment says so.
    pub fn resolve(explicit: Option<&str>) -> Result<Mux, String> {
        let env = std::env::var(ADDRESS_ENV).ok();
        Mux::resolve_with(explicit, env.as_deref())
    }

    /// [`Mux::resolve`] with the environment's value given.
    ///
    /// # Errors
    ///
    /// The same as [`Mux::resolve`].
    pub fn resolve_with(explicit: Option<&str>, env: Option<&str>) -> Result<Mux, String> {
        if let Some(spec) = explicit {
            return Mux::from_spec(spec);
        }
        if let Some(spec) = env.map(str::trim).filter(|v| !v.is_empty()) {
            return Mux::from_spec(spec).map_err(|e| format!("${ADDRESS_ENV}: {e}"));
        }
        let addr = UsbmuxdAddr::default();
        Ok(Mux { spec: describe(&addr), addr })
    }

    /// The usbmuxd at `spec` (see [`parse_usbmuxd_addr`]).
    ///
    /// # Errors
    ///
    /// The same as [`parse_usbmuxd_addr`].
    pub fn from_spec(spec: &str) -> Result<Mux, String> {
        Ok(Mux { addr: parse_usbmuxd_addr(spec)?, spec: spec.trim().to_string() })
    }

    /// The address.
    #[must_use]
    pub fn addr(&self) -> &UsbmuxdAddr {
        &self.addr
    }

    /// The address as it was given: what messages show.
    #[must_use]
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// Connects to usbmuxd.
    ///
    /// # Errors
    ///
    /// A message that names the address if usbmuxd cannot be connected to or does not accept in time.
    pub async fn connect(&self, tag: u32) -> Result<UsbmuxdConnection, String> {
        match tokio::time::timeout(TIMEOUT, self.addr.connect(tag)).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(format!("cannot connect to usbmuxd at {}: {e:?}", self.spec)),
            Err(_) => Err(self.unanswered()),
        }
    }

    fn unanswered(&self) -> String {
        format!("usbmuxd at {} did not answer in {} s", self.spec, TIMEOUT.as_secs())
    }

    /// The devices usbmuxd knows. A device that is attached over USB and over the network is listed twice.
    ///
    /// # Errors
    ///
    /// A message if usbmuxd cannot be connected to, does not answer in time, or fails the request.
    pub async fn devices(&self) -> Result<Vec<UsbmuxdDevice>, String> {
        let mut connection = self.connect(1).await?;
        match tokio::time::timeout(TIMEOUT, connection.get_devices()).await {
            Ok(Ok(devices)) => Ok(devices),
            Ok(Err(e)) => Err(format!("usbmuxd device list failed: {e:?}")),
            Err(_) => Err(self.unanswered()),
        }
    }

    /// Whether usbmuxd holds a pair record for `udid` (its `ReadPairRecord`). The record is the proof that the
    /// device trusts this host; scrios reads it from nowhere else.
    ///
    /// # Errors
    ///
    /// A message if usbmuxd cannot be connected to, does not answer in time, or fails the request.
    pub async fn pair_record(&self, udid: &str) -> Result<PairLookup, String> {
        let mut connection = self.connect(1).await?;
        match tokio::time::timeout(TIMEOUT, connection.get_pair_record(udid)).await {
            Ok(Ok(_)) => Ok(PairLookup::Present),
            // idevice answers these for a reply without `PairRecordData` (usbmuxd has no record) and for one that
            // does not parse as a pair record.
            Ok(Err(IdeviceError::UnexpectedResponse(why))) if why.contains("PairRecordData") => Ok(PairLookup::Missing),
            Ok(Err(IdeviceError::UnexpectedResponse(why))) => Ok(PairLookup::Unreadable(why)),
            Ok(Err(e)) => Err(format!("usbmuxd pair record lookup failed: {e:?}")),
            Err(_) => Err(self.unanswered()),
        }
    }

    /// The provider of `device` for idevice's services: its connections and its pair record both go through this
    /// usbmuxd.
    #[must_use]
    pub fn provider(&self, device: &UsbmuxdDevice, label: &str) -> UsbmuxdProvider {
        device.to_provider(self.addr.clone(), label)
    }
}

/// The outcome of picking a device from usbmuxd's list.
#[derive(Debug, Clone)]
pub enum Choice<'a> {
    /// The device to use.
    Found(&'a UsbmuxdDevice),
    /// No device is connected (and none was asked for by UDID).
    NoDevice,
    /// A UDID was given and usbmuxd lists no such device (the UDIDs it lists).
    UnknownUdid(Vec<String>),
    /// Several devices are connected and no UDID tells them apart (their UDIDs).
    Several(Vec<String>),
}

/// Picks the device: the one named by `udid`, else the only one. A device that usbmuxd lists both over USB and over
/// the network is one device (USB wins).
#[must_use]
pub fn choose<'a>(devices: &'a [UsbmuxdDevice], udid: Option<&str>) -> Choice<'a> {
    let candidates: Vec<&UsbmuxdDevice> = match udid {
        Some(u) => devices.iter().filter(|d| d.udid == u).collect(),
        None => devices.iter().collect(),
    };
    let mut udids: Vec<&str> = candidates.iter().map(|d| d.udid.as_str()).collect();
    udids.sort_unstable();
    udids.dedup();
    match (udids.as_slice(), udid) {
        ([], Some(_)) => {
            let mut listed: Vec<String> = devices.iter().map(|d| d.udid.clone()).collect();
            listed.sort_unstable();
            listed.dedup();
            Choice::UnknownUdid(listed)
        }
        ([], None) => Choice::NoDevice,
        ([_], _) => {
            let usb = candidates.iter().find(|d| d.connection_type == Connection::Usb);
            usb.or_else(|| candidates.first()).map_or(Choice::NoDevice, |d| Choice::Found(d))
        }
        (several, _) => Choice::Several(several.iter().map(ToString::to_string).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testmux::{FakeMux, pair_record_bytes};

    fn dev(udid: &str, conn: Connection, id: u32) -> UsbmuxdDevice {
        UsbmuxdDevice { connection_type: conn, udid: udid.into(), device_id: id }
    }

    const UDID: &str = "00008110-001234567890401E";

    #[test]
    fn addresses_are_unix_sockets_or_tcp_endpoints() {
        #[cfg(unix)]
        {
            for spec in
                ["/var/run/usbmuxd", "UNIX:/var/run/usbmuxd", "unix:/var/run/usbmuxd", " UNIX: /var/run/usbmuxd "]
            {
                assert!(
                    matches!(parse_usbmuxd_addr(spec), Ok(UsbmuxdAddr::UnixSocket(p)) if p == "/var/run/usbmuxd"),
                    "{spec}"
                );
            }
            assert!(matches!(parse_usbmuxd_addr("./run/usbmuxd"), Ok(UsbmuxdAddr::UnixSocket(_))));
            assert!(matches!(parse_usbmuxd_addr("usbmuxd.sock"), Ok(UsbmuxdAddr::UnixSocket(_))));
            assert!(parse_usbmuxd_addr("UNIX:").is_err());
        }
        for spec in ["127.0.0.1:27015", "TCP:127.0.0.1:27015", "tcp:127.0.0.1:27015", "localhost:27015"] {
            match parse_usbmuxd_addr(spec) {
                Ok(UsbmuxdAddr::TcpSocket(a)) => assert_eq!(a.port(), 27015, "{spec}"),
                other => panic!("{spec}: {other:?}"),
            }
        }
        match parse_usbmuxd_addr("[::1]:27015") {
            Ok(UsbmuxdAddr::TcpSocket(a)) => assert!(a.is_ipv6() && a.port() == 27015),
            other => panic!("{other:?}"),
        }
        assert!(parse_usbmuxd_addr("").is_err());
        assert!(parse_usbmuxd_addr("   ").is_err());
        assert!(parse_usbmuxd_addr("host:notaport").is_err());
        assert!(parse_usbmuxd_addr("TCP:host").is_err());
        assert!(parse_usbmuxd_addr("TCP:127.0.0.1:99999").is_err());
    }

    #[test]
    fn the_address_is_the_option_then_the_environment_then_the_platform_default() {
        let given = Mux::resolve_with(Some("127.0.0.1:1"), Some("127.0.0.1:2")).unwrap();
        assert_eq!(given.spec(), "127.0.0.1:1");
        let from_env = Mux::resolve_with(None, Some("127.0.0.1:2")).unwrap();
        assert_eq!(from_env.spec(), "127.0.0.1:2");
        assert!(matches!(from_env.addr(), UsbmuxdAddr::TcpSocket(a) if a.port() == 2));
        // An environment variable that is empty says nothing.
        for env in [None, Some(""), Some("  ")] {
            let default = Mux::resolve_with(None, env).unwrap();
            assert_eq!(default.spec(), describe(&UsbmuxdAddr::default()));
        }
        #[cfg(unix)]
        assert_eq!(Mux::resolve_with(None, None).unwrap().spec(), "/var/run/usbmuxd");
        #[cfg(not(unix))]
        assert_eq!(Mux::resolve_with(None, None).unwrap().spec(), "127.0.0.1:27015");
        // A bad address is an error whichever way it came, and the environment is named.
        assert!(Mux::resolve_with(Some(""), None).is_err());
        let e = Mux::resolve_with(None, Some("host:notaport")).unwrap_err();
        assert!(e.contains("USBMUXD_SOCKET_ADDRESS") && e.contains("port"), "{e}");
        #[cfg(unix)]
        assert_eq!(Mux::resolve_with(Some("UNIX:/tmp/mux"), None).unwrap().spec(), "UNIX:/tmp/mux");
    }

    #[test]
    fn the_only_device_is_chosen_and_usb_beats_network_for_one_udid() {
        let one = [dev("AAA", Connection::Usb, 1)];
        assert!(matches!(choose(&one, None), Choice::Found(d) if d.device_id == 1));
        assert!(matches!(choose(&one, Some("AAA")), Choice::Found(d) if d.device_id == 1));
        let both = [dev("AAA", Connection::Network("10.0.0.2".parse().unwrap()), 7), dev("AAA", Connection::Usb, 8)];
        assert!(matches!(choose(&both, None), Choice::Found(d) if d.device_id == 8));
        let net = [dev("AAA", Connection::Network("10.0.0.2".parse().unwrap()), 7)];
        assert!(matches!(choose(&net, None), Choice::Found(d) if d.device_id == 7));
    }

    #[test]
    fn no_device_a_wrong_udid_and_several_devices_are_told_apart() {
        assert!(matches!(choose(&[], None), Choice::NoDevice));
        assert!(matches!(choose(&[], Some("AAA")), Choice::UnknownUdid(listed) if listed.is_empty()));
        let two = [dev("BBB", Connection::Usb, 2), dev("AAA", Connection::Usb, 1)];
        assert!(matches!(choose(&two, None), Choice::Several(u) if u == ["AAA", "BBB"]));
        assert!(matches!(choose(&two, Some("BBB")), Choice::Found(d) if d.device_id == 2));
        assert!(matches!(choose(&two, Some("CCC")), Choice::UnknownUdid(u) if u == ["AAA", "BBB"]));
    }

    /// The same checks against a fake usbmuxd on whatever transport.
    async fn exercise(fake: &FakeMux) {
        let mux = Mux::from_spec(fake.spec()).unwrap();
        // The device list: one device over USB and over the network.
        let devices = mux.devices().await.unwrap();
        assert_eq!(devices.len(), 2);
        assert!(matches!(choose(&devices, None), Choice::Found(d) if d.connection_type == Connection::Usb));
        // The pair record.
        assert_eq!(mux.pair_record(UDID).await.unwrap(), PairLookup::Present);
        assert_eq!(mux.pair_record("00008110-000000000000000A").await.unwrap(), PairLookup::Missing);
        assert!(matches!(mux.pair_record("GARBAGE").await.unwrap(), PairLookup::Unreadable(_)));
        // The provider carries the same address.
        let provider = mux.provider(&devices[0], "scrios-test");
        assert!(format!("{provider:?}").contains(&devices[0].udid));
        // Nothing but the device list and pair records was asked of usbmuxd (no lockdown directory, no connection).
        let asked = fake.requests();
        assert!(asked.iter().all(|m| m == "ListDevices" || m == "ReadPairRecord"), "{asked:?}");
        assert!(asked.iter().any(|m| m == "ReadPairRecord"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_unix_socket_usbmuxd_lists_devices_and_reads_pair_records() {
        let fake = FakeMux::unix();
        fake.attach_usb(UDID, 1);
        fake.attach_network(UDID, 2, [192, 168, 1, 5]);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        fake.set_pair_record("GARBAGE", b"not a plist".to_vec());
        exercise(&fake).await;
        // The `UNIX:` spelling reaches the same socket.
        let mux = Mux::from_spec(&format!("UNIX:{}", fake.spec())).unwrap();
        assert_eq!(mux.devices().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_tcp_usbmuxd_lists_devices_and_reads_pair_records() {
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.attach_network(UDID, 2, [192, 168, 1, 5]);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        fake.set_pair_record("GARBAGE", b"not a plist".to_vec());
        exercise(&fake).await;
        let mux = Mux::from_spec(&format!("TCP:{}", fake.spec())).unwrap();
        assert_eq!(mux.devices().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_usbmuxd_that_is_not_there_is_an_error_that_says_where() {
        let mux = Mux::from_spec("127.0.0.1:1").unwrap();
        let e = mux.devices().await.unwrap_err();
        assert!(e.contains("cannot connect to usbmuxd at 127.0.0.1:1"), "{e}");
        assert!(mux.pair_record(UDID).await.is_err());
        assert!(!usbmuxd_hint().is_empty());
        assert!(address_forms().contains("TCP:host:port") && address_forms().contains("UNIX:") == cfg!(unix));
    }
}
