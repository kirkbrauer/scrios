//! Getting from "a device is plugged in" to "the stream can start": finding the device through usbmuxd, checking that
//! it trusts this host, and the rest of what the display service needs.
//!
//! Each step that can be missing is a [`Blocker`], which says in a line what to do about it. [`open_device`] is the
//! first steps (the device, and its pair record in usbmuxd) in one pass: it either gives the device or says what
//! stops it ([`Stop`]), which the commands that manage a device print and exit on.

use idevice::usbmuxd::UsbmuxdDevice;

use crate::mux::{Choice, Mux, PairLookup, choose, usbmuxd_hint};

/// Which device to work with, and through which usbmuxd.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// The device's UDID (default: the only device usbmuxd lists).
    pub udid: Option<String>,
    /// The usbmuxd address (see [`Mux::resolve`]).
    pub usbmuxd: Option<String>,
}

/// Something that has to happen, on the device or at the host, before the next step can be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// No device is connected.
    NoDevice,
    /// A UDID was asked for and usbmuxd lists no such device (the UDIDs it lists).
    UnknownUdid { udid: String, listed: Vec<String> },
    /// The device does not trust this host (usbmuxd holds no pair record for it).
    NotTrusted { udid: String },
}

impl Blocker {
    /// The one line that says what is waited for, when waiting.
    #[must_use]
    pub fn wait_line(&self) -> String {
        match self {
            Blocker::NoDevice => "Plug in and unlock your iPhone or iPad".to_string(),
            Blocker::UnknownUdid { udid, listed } => format!(
                "Plug in and unlock the device {udid} (usbmuxd lists: {})",
                if listed.is_empty() { "nothing".to_string() } else { listed.join(", ") }
            ),
            Blocker::NotTrusted { .. } => "Unlock the device and tap Trust".to_string(),
        }
    }

    /// The error that says it, when not waiting.
    #[must_use]
    pub fn error(&self) -> String {
        match self {
            Blocker::NoDevice => {
                "no device is connected (usbmuxd lists none): plug in and unlock your iPhone or iPad".into()
            }
            Blocker::UnknownUdid { udid, listed } => format!(
                "no device with UDID {udid} (usbmuxd lists: {})",
                if listed.is_empty() { "nothing".to_string() } else { listed.join(", ") }
            ),
            Blocker::NotTrusted { udid } => format!(
                "this computer is not trusted by the device {udid} (usbmuxd has no pair record for it): unlock the device and tap Trust when it asks; if it does not ask, pair it with your system's tools first (Finder on macOS, `idevicepair pair` on Linux)"
            ),
        }
    }
}

/// Why a step did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It can be cured, usually by the user at the device.
    Blocked(Blocker),
    /// It failed in a way that waiting does not cure.
    Failed(String),
}

impl Stop {
    /// What to tell the user.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Stop::Blocked(blocker) => blocker.error(),
            Stop::Failed(why) => why.clone(),
        }
    }
}

/// The usbmuxd, and the device it lists for `target` (the one named by the UDID, else the only one), once the device
/// has a pair record there.
///
/// # Errors
///
/// [`Stop::Blocked`] if no such device is connected or it does not trust this host; [`Stop::Failed`] if usbmuxd
/// cannot be reached, several devices are connected and no UDID tells them apart, or the pair record is unreadable.
pub async fn open_device(target: &Target) -> Result<(Mux, UsbmuxdDevice), Stop> {
    let mux = Mux::resolve(target.usbmuxd.as_deref()).map_err(Stop::Failed)?;
    let devices = mux.devices().await.map_err(|e| Stop::Failed(format!("{e} ({})", usbmuxd_hint())))?;
    let device = match choose(&devices, target.udid.as_deref()) {
        Choice::Found(device) => device.clone(),
        Choice::NoDevice => return Err(Stop::Blocked(Blocker::NoDevice)),
        Choice::UnknownUdid(listed) => {
            let udid = target.udid.clone().unwrap_or_default();
            return Err(Stop::Blocked(Blocker::UnknownUdid { udid, listed }));
        }
        Choice::Several(udids) => {
            return Err(Stop::Failed(format!("several devices are connected ({}): pass --udid", udids.join(", "))));
        }
    };
    match mux.pair_record(&device.udid).await.map_err(Stop::Failed)? {
        PairLookup::Present => Ok((mux, device)),
        PairLookup::Missing => Err(Stop::Blocked(Blocker::NotTrusted { udid: device.udid })),
        PairLookup::Unreadable(why) => Err(Stop::Failed(format!(
            "usbmuxd's pair record for {} cannot be read ({why}): unlock the device, tap Trust again, and if that does not help remove the stale record with your system's pairing tool",
            device.udid
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testmux::{FakeMux, pair_record_bytes};

    const UDID: &str = "00008110-001234567890401E";
    const OTHER: &str = "00008120-000A1B2C3D4E5F60";

    fn target(fake: &FakeMux, udid: Option<&str>) -> Target {
        Target { udid: udid.map(str::to_string), usbmuxd: Some(fake.spec().to_string()) }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_trusted_device_is_opened_and_the_pair_record_comes_from_usbmuxd() {
        let fake = FakeMux::unix();
        fake.attach_usb(UDID, 1);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        let (mux, device) = open_device(&target(&fake, None)).await.unwrap();
        assert_eq!((device.udid.as_str(), device.device_id), (UDID, 1));
        assert_eq!(mux.spec(), fake.spec());
        assert!(fake.requests().iter().any(|m| m == "ReadPairRecord"));
    }

    #[tokio::test]
    async fn what_stops_a_device_is_a_blocker_that_says_what_to_do() {
        let fake = FakeMux::tcp();
        // Nothing connected.
        let stop = open_device(&target(&fake, None)).await.unwrap_err();
        assert_eq!(stop, Stop::Blocked(Blocker::NoDevice));
        assert_eq!(Blocker::NoDevice.wait_line(), "Plug in and unlock your iPhone or iPad");
        assert!(stop.message().contains("no device is connected"));
        // A device that has never been trusted.
        fake.attach_usb(UDID, 1);
        let stop = open_device(&target(&fake, None)).await.unwrap_err();
        assert_eq!(stop, Stop::Blocked(Blocker::NotTrusted { udid: UDID.into() }));
        assert!(stop.message().contains("Trust") && stop.message().contains(UDID));
        assert_eq!(Blocker::NotTrusted { udid: UDID.into() }.wait_line(), "Unlock the device and tap Trust");
        // The UDID asked for is not the one that is connected.
        let stop = open_device(&target(&fake, Some(OTHER))).await.unwrap_err();
        assert_eq!(stop, Stop::Blocked(Blocker::UnknownUdid { udid: OTHER.into(), listed: vec![UDID.into()] }));
        assert!(stop.message().starts_with(&format!("no device with UDID {OTHER} (usbmuxd lists: {UDID}")));
        // A pair record that is garbage is not the same as none.
        fake.set_pair_record(UDID, b"garbage".to_vec());
        let stop = open_device(&target(&fake, None)).await.unwrap_err();
        assert!(matches!(&stop, Stop::Failed(why) if why.contains("cannot be read")), "{stop:?}");
        // Trusted at last.
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        assert!(open_device(&target(&fake, None)).await.is_ok());
    }

    #[tokio::test]
    async fn several_devices_need_a_udid_and_the_message_lists_them() {
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.attach_usb(OTHER, 2);
        fake.set_pair_record(OTHER, pair_record_bytes(OTHER));
        let stop = open_device(&target(&fake, None)).await.unwrap_err();
        let Stop::Failed(why) = stop else { panic!("{stop:?}") };
        assert!(why.contains("--udid") && why.contains(UDID) && why.contains(OTHER), "{why}");
        let (_, device) = open_device(&target(&fake, Some(OTHER))).await.unwrap();
        assert_eq!(device.udid, OTHER);
    }

    #[tokio::test]
    async fn no_usbmuxd_is_a_failure_with_the_platforms_hint() {
        let stop = open_device(&Target { udid: None, usbmuxd: Some("127.0.0.1:1".into()) }).await.unwrap_err();
        let Stop::Failed(why) = stop else { panic!("{stop:?}") };
        assert!(why.contains("cannot connect to usbmuxd at 127.0.0.1:1") && why.contains(usbmuxd_hint()), "{why}");
        let stop = open_device(&Target { udid: None, usbmuxd: Some("host:notaport".into()) }).await.unwrap_err();
        assert!(matches!(stop, Stop::Failed(_)));
    }
}
