//! The devices usbmuxd knows: what `scrios list` prints.
//!
//! usbmuxd reports a device's UDID and how it is connected, and nothing else: the name and model are behind a
//! lockdown session, which needs the pair record and a TLS handshake, so they are not listed.

use std::time::Duration;

use idevice::usbmuxd::{Connection, UsbmuxdDevice};

use crate::device::{default_usbmuxd_spec, parse_usbmuxd_addr};

/// How long connecting to usbmuxd and listing may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// One entry of usbmuxd's device list. A device that is connected both over USB and over the network is listed
/// twice, once for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub udid: String,
    /// `usb`, `network <address>`, or what usbmuxd said.
    pub connection: String,
    /// usbmuxd's number for this connection of the device.
    pub device_id: u32,
}

impl From<&UsbmuxdDevice> for Listed {
    fn from(d: &UsbmuxdDevice) -> Self {
        let connection = match &d.connection_type {
            Connection::Usb => "usb".to_string(),
            Connection::Network(ip) => format!("network {ip}"),
            Connection::Unknown(what) => what.clone(),
        };
        Listed { udid: d.udid.clone(), connection, device_id: d.device_id }
    }
}

/// Asks usbmuxd (at `usbmuxd`, else `$USBMUXD_SOCKET_ADDRESS`, else the system's socket) for its devices.
///
/// # Errors
///
/// A message if the address is bad (see [`parse_usbmuxd_addr`]), usbmuxd cannot be
/// connected to, or it does not answer in time.
pub async fn list_devices(usbmuxd: Option<&str>) -> Result<Vec<Listed>, String> {
    let spec = usbmuxd.map(str::to_string).unwrap_or_else(default_usbmuxd_spec);
    let addr = parse_usbmuxd_addr(&spec)?;
    let listing = async {
        let mut mux = addr.connect(1).await.map_err(|e| format!("cannot connect to usbmuxd at {spec}: {e:?}"))?;
        mux.get_devices().await.map_err(|e| format!("usbmuxd device list failed: {e:?}"))
    };
    let devices = tokio::time::timeout(TIMEOUT, listing)
        .await
        .map_err(|_| format!("usbmuxd at {spec} did not answer in {} s", TIMEOUT.as_secs()))??;
    let mut listed: Vec<Listed> = devices.iter().map(Listed::from).collect();
    listed.sort_by(|a, b| (&a.udid, &a.connection).cmp(&(&b.udid, &b.connection)));
    Ok(listed)
}

/// The table `scrios list` prints: a header and a line per entry (nothing else for an empty list).
#[must_use]
pub fn format_table(devices: &[Listed]) -> String {
    let width = devices.iter().map(|d| d.udid.len()).max().unwrap_or(0).max("UDID".len());
    let mut out = format!("{:<width$}  CONNECTION\n", "UDID");
    for d in devices {
        out.push_str(&format!("{:<width$}  {}\n", d.udid, d.connection));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(udid: &str, connection_type: Connection) -> UsbmuxdDevice {
        UsbmuxdDevice { connection_type, udid: udid.into(), device_id: 7 }
    }

    #[test]
    fn a_connection_is_described_by_its_kind() {
        let usb = Listed::from(&device("A", Connection::Usb));
        assert_eq!(usb, Listed { udid: "A".into(), connection: "usb".into(), device_id: 7 });
        let net = Listed::from(&device("A", Connection::Network("192.168.1.20".parse().unwrap())));
        assert_eq!(net.connection, "network 192.168.1.20");
        assert_eq!(Listed::from(&device("A", Connection::Unknown("Wi-Fi".into()))).connection, "Wi-Fi");
    }

    #[test]
    fn the_table_lines_up_on_the_longest_udid() {
        let devices = [
            Listed { udid: "00008110-001234567890401E".into(), connection: "usb".into(), device_id: 1 },
            Listed { udid: "00008110-001234567890401E".into(), connection: "network 10.0.0.2".into(), device_id: 2 },
            Listed { udid: "SHORT".into(), connection: "usb".into(), device_id: 3 },
        ];
        assert_eq!(
            format_table(&devices),
            "UDID                       CONNECTION\n\
             00008110-001234567890401E  usb\n\
             00008110-001234567890401E  network 10.0.0.2\n\
             SHORT                      usb\n"
        );
        assert_eq!(format_table(&[]), "UDID  CONNECTION\n");
    }

    #[tokio::test]
    async fn no_usbmuxd_is_an_error_that_says_where() {
        let err = list_devices(Some("/nonexistent/usbmuxd")).await.unwrap_err();
        assert!(err.contains("cannot connect to usbmuxd at /nonexistent/usbmuxd"), "{err}");
        assert!(list_devices(Some("host:notaport")).await.is_err());
    }
}
