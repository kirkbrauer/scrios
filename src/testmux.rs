//! A usbmuxd for the tests: threads that answer what scrios asks of usbmuxd (the device list, pair records) on a
//! unix socket or on TCP, with idevice's own server-side message types. No device and no real usbmuxd is touched.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use idevice::usbmuxd::RawPacket;
use idevice::usbmuxd::server::{UsbmuxdServerRequest, UsbmuxdServerResponse};

#[derive(Default)]
struct State {
    devices: Vec<plist::Value>,
    records: HashMap<String, Vec<u8>>,
    requests: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

enum Where {
    #[cfg(unix)]
    Unix(std::path::PathBuf),
    Tcp(SocketAddr),
}

/// A running fake usbmuxd. It stops (and removes its socket file) when dropped.
pub(crate) struct FakeMux {
    spec: String,
    state: Shared,
    stop: Arc<AtomicBool>,
    at: Where,
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

impl FakeMux {
    /// On a unix socket in the temporary directory; [`FakeMux::spec`] is its path.
    #[cfg(unix)]
    pub(crate) fn unix() -> FakeMux {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("scrios-fm-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (state, stop) = (Shared::default(), Arc::new(AtomicBool::new(false)));
        let (s, st) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(stream) = stream {
                    let s = s.clone();
                    std::thread::spawn(move || serve(stream, &s));
                }
            }
        });
        FakeMux { spec: path.display().to_string(), state, stop, at: Where::Unix(path) }
    }

    /// On TCP, on a free port of 127.0.0.1; [`FakeMux::spec`] is `127.0.0.1:port`.
    pub(crate) fn tcp() -> FakeMux {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (state, stop) = (Shared::default(), Arc::new(AtomicBool::new(false)));
        let (s, st) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(stream) = stream {
                    let s = s.clone();
                    std::thread::spawn(move || serve(stream, &s));
                }
            }
        });
        FakeMux { spec: addr.to_string(), state, stop, at: Where::Tcp(addr) }
    }

    /// The address to give `--usbmuxd`.
    pub(crate) fn spec(&self) -> &str {
        &self.spec
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn attach(&self, udid: &str, id: u32, connection: &str, address: Option<[u8; 4]>) {
        let mut props = plist::Dictionary::new();
        props.insert("ConnectionType".into(), connection.into());
        props.insert("DeviceID".into(), u64::from(id).into());
        props.insert("SerialNumber".into(), udid.into());
        if let Some([a, b, c, d]) = address {
            // A BSD sockaddr_in: its length, AF_INET, the port, the address, and padding.
            let mut raw = vec![0x10, 0x02, 0, 0, a, b, c, d];
            raw.extend_from_slice(&[0; 8]);
            props.insert("NetworkAddress".into(), plist::Value::Data(raw));
        }
        let mut entry = plist::Dictionary::new();
        entry.insert("DeviceID".into(), u64::from(id).into());
        entry.insert("MessageType".into(), "Attached".into());
        entry.insert("Properties".into(), plist::Value::Dictionary(props));
        self.with(|s| s.devices.push(plist::Value::Dictionary(entry)));
    }

    /// A device attached over USB.
    pub(crate) fn attach_usb(&self, udid: &str, id: u32) {
        self.attach(udid, id, "USB", None);
    }

    /// A device attached over the network.
    pub(crate) fn attach_network(&self, udid: &str, id: u32, ip: [u8; 4]) {
        self.attach(udid, id, "Network", Some(ip));
    }

    /// The pair record usbmuxd holds for `udid`.
    pub(crate) fn set_pair_record(&self, udid: &str, data: Vec<u8>) {
        self.with(|s| {
            s.records.insert(udid.to_string(), data);
        });
    }

    /// The `MessageType`s of the requests so far, in order.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.with(|s| s.requests.clone())
    }
}

impl Drop for FakeMux {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the acceptor so that it sees the flag.
        match &self.at {
            #[cfg(unix)]
            Where::Unix(path) => {
                let _ = std::os::unix::net::UnixStream::connect(path);
                let _ = std::fs::remove_file(path);
            }
            Where::Tcp(addr) => {
                let _ = TcpStream::connect(addr);
            }
        }
    }
}

/// A pair record in the form usbmuxd stores it: the certificates are PEM (placeholder bytes, which is all idevice
/// checks), the keys and the rest are plain.
pub(crate) fn pair_record_bytes(udid: &str) -> Vec<u8> {
    let pem =
        |what: &str| plist::Value::Data(format!("-----BEGIN {what}-----\nAAAA\n-----END {what}-----\n").into_bytes());
    let mut d = plist::Dictionary::new();
    d.insert("DeviceCertificate".into(), pem("CERTIFICATE"));
    d.insert("HostCertificate".into(), pem("CERTIFICATE"));
    d.insert("RootCertificate".into(), pem("CERTIFICATE"));
    d.insert("HostPrivateKey".into(), plist::Value::Data(b"host key".to_vec()));
    d.insert("RootPrivateKey".into(), plist::Value::Data(b"root key".to_vec()));
    d.insert("SystemBUID".into(), "00000000-0000-0000-0000-000000000000".into());
    d.insert("HostID".into(), "11111111-1111-1111-1111-111111111111".into());
    d.insert("WiFiMACAddress".into(), "02:00:00:00:00:00".into());
    d.insert("UDID".into(), udid.into());
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &plist::Value::Dictionary(d)).unwrap();
    out
}

fn serve<S: Read + Write>(mut stream: S, state: &Shared) {
    loop {
        let mut head = [0u8; 16];
        if stream.read_exact(&mut head).is_err() {
            return;
        }
        let size = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let mut packet = head.to_vec();
        packet.resize(size, 0);
        if stream.read_exact(&mut packet[16..]).is_err() {
            return;
        }
        let Ok(request) = RawPacket::try_from(packet.as_slice()) else { return };
        let tag = request.tag;
        let kind = request.plist.get("MessageType").and_then(plist::Value::as_string).unwrap_or("?").to_string();
        let reply = {
            let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
            s.requests.push(kind);
            match UsbmuxdServerRequest::decode(&request) {
                Ok(UsbmuxdServerRequest::ListDevices) => UsbmuxdServerResponse::DeviceList(s.devices.clone()),
                Ok(UsbmuxdServerRequest::ReadPairRecord { pair_record_id }) => match s.records.get(&pair_record_id) {
                    Some(data) => UsbmuxdServerResponse::PairRecord(data.clone()),
                    // What usbmuxd says of a record it does not have.
                    None => UsbmuxdServerResponse::Result(2),
                },
                Ok(UsbmuxdServerRequest::ReadBuid) => UsbmuxdServerResponse::Buid("fake-buid".into()),
                Ok(_) | Err(_) => UsbmuxdServerResponse::Result(1),
            }
        };
        let bytes: Vec<u8> = reply.into_packet(tag).into();
        if stream.write_all(&bytes).is_err() {
            return;
        }
    }
}
