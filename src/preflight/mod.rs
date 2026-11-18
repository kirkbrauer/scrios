//! The preflight: getting from "a device is plugged in" to "the stream can start", the way scrcpy just works.
//!
//! [`ensure_ready`] walks the steps in order and, for each one that is missing, either waits for it (saying in one
//! line what it waits for) or fails with a message that says what to do:
//!
//! 1. **usbmuxd** is reached at the address resolved by [`Mux`] (usbmuxd is the only transport);
//! 2. a **device** is picked: the one named by the UDID, else the only one (none: "Plug in and unlock your iPhone or
//!    iPad", and wait);
//! 3. the device **trusts this host**: usbmuxd holds a pair record for it (none: "Unlock the device and tap Trust",
//!    and wait);
//! 4. **Developer Mode** is on (off: the switch is revealed in Settings, the steps are said, and it waits; scrios
//!    never turns it on by itself);
//! 5. the **developer disk image** is mounted (not: it is mounted from [`DdiSources`], which needs internet for
//!    Apple's signing server the first time after each restart of the device).
//!
//! It returns a [`ReadyDevice`], which carries what a stream needs ([`ReadyDevice::stream_options`]), and reports what
//! it does through a callback of [`PreflightStep`]s: the command line prints their [`line`](PreflightStep::line), a
//! window can show them. The same checks, run one by one without waiting, make the [`doctor`] report.

pub mod doctor;

use std::time::Duration;

use idevice::usbmuxd::UsbmuxdDevice;
use tracing::debug;

use crate::ddi::{DdiSources, Found};
use crate::mux::{Choice, Mux, PairLookup, choose, usbmuxd_hint};
use crate::ops::{DevError, Device, Mounted};
use crate::stream::StreamOptions;

/// How often a missing step is looked at again while waiting.
const POLL: Duration = Duration::from_secs(1);
/// How many times in a row a device that answered before may fail to, before that is an error and not a restart.
const MAX_UNRESPONSIVE: u32 = 60;

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
    /// The device is locked.
    Locked,
    /// Developer Mode is off. `after_restart`: the device has restarted since this was first said, so what is left
    /// is the confirmation that iOS asks for after the restart.
    DeveloperModeOff { udid: String, after_restart: bool },
    /// The device went away after Developer Mode was said to need a restart: it is restarting.
    Restarting,
    /// The device answered before and does not now (the reason): it may be restarting or locking.
    Unresponsive(String),
}

fn listed_or_nothing(listed: &[String]) -> String {
    if listed.is_empty() { "nothing".to_string() } else { listed.join(", ") }
}

impl Blocker {
    /// The one line that says what is waited for, when waiting.
    #[must_use]
    pub fn wait_line(&self) -> String {
        match self {
            Blocker::NoDevice => "Plug in and unlock your iPhone or iPad".to_string(),
            Blocker::UnknownUdid { udid, listed } => {
                format!("Plug in and unlock the device {udid} (usbmuxd lists: {})", listed_or_nothing(listed))
            }
            Blocker::NotTrusted { .. } => "Unlock the device and tap Trust".to_string(),
            Blocker::Locked => "Unlock the device".to_string(),
            Blocker::DeveloperModeOff { after_restart: false, .. } => {
                "Turn Developer Mode on: Settings > Privacy & Security > Developer Mode > On, then restart the device"
                    .to_string()
            }
            Blocker::DeveloperModeOff { after_restart: true, .. } => {
                "Developer Mode is still off: after the restart, tap Turn On on the device and enter its passcode"
                    .to_string()
            }
            Blocker::Restarting => "Waiting for the device to restart".to_string(),
            Blocker::Unresponsive(why) => format!("The device does not answer yet ({why}): waiting"),
        }
    }

    /// The error that says it, when not waiting.
    #[must_use]
    pub fn error(&self) -> String {
        match self {
            Blocker::NoDevice => {
                "no device is connected (usbmuxd lists none): plug in and unlock your iPhone or iPad".into()
            }
            Blocker::UnknownUdid { udid, listed } => {
                format!("no device with UDID {udid} (usbmuxd lists: {})", listed_or_nothing(listed))
            }
            Blocker::NotTrusted { udid } => format!(
                "this computer is not trusted by the device {udid} (usbmuxd has no pair record for it): unlock the device and tap Trust when it asks; if it does not ask, pair it with your system's tools first (Finder on macOS, `idevicepair pair` on Linux)"
            ),
            Blocker::Locked => "the device is locked: unlock it".into(),
            Blocker::DeveloperModeOff { udid, .. } => format!(
                "Developer Mode is off on the device {udid}: turn it on in Settings > Privacy & Security > Developer Mode, then restart the device and confirm (scrios never turns it on by itself; `scrios developer-mode --enable --yes` does, on a device without a passcode)"
            ),
            Blocker::Restarting => "the device is restarting".into(),
            Blocker::Unresponsive(why) => format!("the device does not answer: {why}"),
        }
    }
}

/// Why the preflight did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightError {
    /// Something has to be done first, and waiting for it was not asked for.
    Blocked(Blocker),
    /// It failed in a way that waiting does not cure.
    Failed(String),
}

impl PreflightError {
    /// What to tell the user.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            PreflightError::Blocked(blocker) => blocker.error(),
            PreflightError::Failed(why) => why.clone(),
        }
    }
}

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PreflightError {}

/// What the preflight does, as it does it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightStep {
    /// It waits for something that the user has to do (said once for each thing).
    Waiting(Blocker),
    /// The Developer Mode switch was revealed in the device's Settings (it is not turned on).
    DeveloperModeRevealed,
    /// The developer disk image is being downloaded (from this host: the rest of a URL can carry a secret).
    Downloading { host: String },
    /// The developer disk image is being mounted, from this folder.
    Mounting { from: String },
    /// The upload of the image is this far (percent, in tens).
    Uploading { percent: u8 },
    /// The image is mounted.
    Mounted,
    /// Everything is in place for the device with this UDID.
    Ready { udid: String },
}

impl PreflightStep {
    /// The line to show for the step, if it has one: plain, without a prefix, for the user to read.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        match self {
            PreflightStep::Waiting(blocker) => Some(blocker.wait_line()),
            PreflightStep::DeveloperModeRevealed => {
                Some("The Developer Mode switch is now in the device's Settings (Privacy & Security)".to_string())
            }
            PreflightStep::Downloading { host } => {
                Some(format!("Downloading the developer disk image from {host} (checked against the SHA-256 you gave)"))
            }
            PreflightStep::Mounting { from } => Some(format!(
                "Mounting the developer disk image from {from} (the first mount after each restart of the device needs internet, for Apple's signing server)"
            )),
            PreflightStep::Uploading { percent } => Some(format!("Uploading the image: {percent}%")),
            PreflightStep::Mounted => Some("The developer disk image is mounted".to_string()),
            PreflightStep::Ready { .. } => None,
        }
    }
}

/// What the preflight is asked to do.
#[derive(Debug, Clone)]
pub struct PreflightOptions {
    /// The device and the usbmuxd to find it through.
    pub target: Target,
    /// Wait for what is missing (and say so), instead of failing at once.
    pub wait: bool,
    /// Mount the developer disk image when it is not mounted (else that is an error).
    pub mount_ddi: bool,
    /// Where to find the image to mount.
    pub ddi: DdiSources,
    /// How often to look again while waiting.
    pub poll: Duration,
}

impl Default for PreflightOptions {
    /// What `scrios` does: wait, and mount.
    fn default() -> Self {
        PreflightOptions {
            target: Target::default(),
            wait: true,
            mount_ddi: true,
            ddi: DdiSources::default(),
            poll: POLL,
        }
    }
}

/// A device that is ready for the stream, and what it was found through.
#[derive(Debug, Clone)]
pub struct ReadyDevice {
    /// The device's UDID.
    pub udid: String,
    /// The usbmuxd it was found through, as text that [`Mux::from_spec`] and `--usbmuxd` take.
    pub usbmuxd: String,
    mux: Mux,
    device: UsbmuxdDevice,
}

impl ReadyDevice {
    /// The usbmuxd.
    #[must_use]
    pub fn mux(&self) -> &Mux {
        &self.mux
    }

    /// The device, for the operations of [`crate::ops`] (Developer Mode, the mounted image).
    #[must_use]
    pub fn handle(&self) -> Device {
        Device::new(&self.mux, &self.device)
    }

    /// `base` for this device: its UDID and its usbmuxd, so that the stream goes to the device that was checked.
    #[must_use]
    pub fn stream_options(&self, base: StreamOptions) -> StreamOptions {
        StreamOptions { udid: Some(self.udid.clone()), usbmuxd: Some(self.usbmuxd.clone()), ..base }
    }
}

/// The usbmuxd, and the device it lists for `target` (the one named by the UDID, else the only one), once the device
/// has a pair record there.
///
/// # Errors
///
/// [`PreflightError::Blocked`] if no such device is connected or it does not trust this host;
/// [`PreflightError::Failed`] if usbmuxd cannot be reached, several devices are connected and no UDID tells them
/// apart, or the pair record is unreadable.
pub async fn open_device(target: &Target) -> Result<(Mux, UsbmuxdDevice), PreflightError> {
    let mux = Mux::resolve(target.usbmuxd.as_deref()).map_err(PreflightError::Failed)?;
    let device = find_trusted_device(&mux, target.udid.as_deref()).await?;
    Ok((mux, device))
}

/// The steps that need no device connection but usbmuxd's: the device, and its pair record.
async fn find_trusted_device(mux: &Mux, udid: Option<&str>) -> Result<UsbmuxdDevice, PreflightError> {
    let devices = mux.devices().await.map_err(|e| PreflightError::Failed(format!("{e} ({})", usbmuxd_hint())))?;
    let device = match choose(&devices, udid) {
        Choice::Found(device) => device.clone(),
        Choice::NoDevice => return Err(PreflightError::Blocked(Blocker::NoDevice)),
        Choice::UnknownUdid(listed) => {
            let udid = udid.unwrap_or_default().to_string();
            return Err(PreflightError::Blocked(Blocker::UnknownUdid { udid, listed }));
        }
        Choice::Several(udids) => {
            return Err(PreflightError::Failed(format!(
                "several devices are connected ({}): pass --udid",
                udids.join(", ")
            )));
        }
    };
    match mux.pair_record(&device.udid).await.map_err(PreflightError::Failed)? {
        PairLookup::Present => Ok(device),
        PairLookup::Missing => Err(PreflightError::Blocked(Blocker::NotTrusted { udid: device.udid })),
        PairLookup::Unreadable(why) => Err(PreflightError::Failed(format!(
            "usbmuxd's pair record for {} cannot be read ({why}): unlock the device, tap Trust again, and if that does not help remove the stale record with your system's pairing tool",
            device.udid
        ))),
    }
}

/// What the preflight asks of a device. The real one is [`Device`]; the tests have their own.
pub(crate) trait Ops {
    async fn developer_mode(&self) -> Result<bool, DevError>;
    async fn reveal_developer_mode(&self) -> Result<(), DevError>;
    async fn image_mounted(&self) -> Result<bool, DevError>;
    async fn mount(&self, folder: &Found, progress: &mut (dyn FnMut(u8) + Send)) -> Result<Mounted, DevError>;
    async fn display_service_offered(&self) -> Result<bool, DevError>;
}

impl Ops for Device {
    async fn developer_mode(&self) -> Result<bool, DevError> {
        Device::developer_mode(self).await
    }

    async fn reveal_developer_mode(&self) -> Result<(), DevError> {
        Device::reveal_developer_mode(self).await
    }

    async fn image_mounted(&self) -> Result<bool, DevError> {
        Device::image_mounted(self).await
    }

    async fn mount(&self, folder: &Found, progress: &mut (dyn FnMut(u8) + Send)) -> Result<Mounted, DevError> {
        Device::mount_ddi(self, &folder.folder, progress).await
    }

    async fn display_service_offered(&self) -> Result<bool, DevError> {
        Device::display_service_offered(self).await
    }
}

/// What the waiting has gone through so far.
#[derive(Debug, Default)]
struct Waited {
    /// The last thing that was said to be waited for.
    last: Option<Blocker>,
    /// The device went away after Developer Mode was found off.
    restarted: bool,
    /// The Developer Mode switch was revealed.
    revealed: bool,
    /// Failures of a device that answered before, in a row.
    unresponsive: u32,
}

/// Makes sure the device is ready for the stream, waiting for what is missing (when [`PreflightOptions::wait`]) and
/// mounting the developer disk image (when [`PreflightOptions::mount_ddi`]). `progress` is told what is done and
/// what is waited for. The future can be dropped at any time to give up.
///
/// scrios never turns Developer Mode on here: it reveals the switch in the device's Settings and says the steps.
///
/// # Errors
///
/// [`PreflightError::Blocked`] for the first thing that is missing when waiting is off; [`PreflightError::Failed`]
/// for what waiting does not cure: usbmuxd cannot be reached, several devices and no UDID, no developer disk image
/// to mount (the message says where to put one), a mount that failed.
pub async fn ensure_ready(
    opts: &PreflightOptions,
    progress: &mut (dyn FnMut(&PreflightStep) + Send),
) -> Result<ReadyDevice, PreflightError> {
    run(opts, progress, Device::new).await
}

/// [`ensure_ready`] with the device operations given.
async fn run<O: Ops>(
    opts: &PreflightOptions,
    progress: &mut (dyn FnMut(&PreflightStep) + Send),
    make: impl Fn(&Mux, &UsbmuxdDevice) -> O,
) -> Result<ReadyDevice, PreflightError> {
    let mux = Mux::resolve(opts.target.usbmuxd.as_deref()).map_err(PreflightError::Failed)?;
    let mut waited = Waited::default();
    loop {
        match attempt(&mux, opts, &make, progress, &mut waited).await {
            Ok(ready) => {
                progress(&PreflightStep::Ready { udid: ready.udid.clone() });
                return Ok(ready);
            }
            Err(PreflightError::Blocked(blocker)) if opts.wait => {
                debug!("waiting: {blocker:?}");
                if waited.last.as_ref() != Some(&blocker) {
                    progress(&PreflightStep::Waiting(blocker.clone()));
                    waited.last = Some(blocker);
                }
                tokio::time::sleep(opts.poll).await;
            }
            Err(other) => return Err(other),
        }
    }
}

/// One pass over the steps: the first that is missing is the error.
async fn attempt<O: Ops>(
    mux: &Mux,
    opts: &PreflightOptions,
    make: &impl Fn(&Mux, &UsbmuxdDevice) -> O,
    progress: &mut (dyn FnMut(&PreflightStep) + Send),
    waited: &mut Waited,
) -> Result<ReadyDevice, PreflightError> {
    let device = match find_trusted_device(mux, opts.target.udid.as_deref()).await {
        Ok(device) => device,
        // The device that went away while Developer Mode was being turned on is restarting.
        Err(PreflightError::Blocked(Blocker::NoDevice | Blocker::UnknownUdid { .. }))
            if matches!(waited.last, Some(Blocker::DeveloperModeOff { .. } | Blocker::Restarting)) =>
        {
            waited.restarted = true;
            return Err(PreflightError::Blocked(Blocker::Restarting));
        }
        Err(other) => return Err(other),
    };
    let udid = device.udid.clone();
    let on_device = make(mux, &device);

    // Developer Mode.
    match on_device.developer_mode().await {
        Ok(true) => waited.unresponsive = 0,
        Ok(false) => {
            if !waited.revealed {
                waited.revealed = true;
                match on_device.reveal_developer_mode().await {
                    Ok(()) => progress(&PreflightStep::DeveloperModeRevealed),
                    Err(e) => debug!("could not reveal the Developer Mode switch: {e}"),
                }
            }
            return Err(PreflightError::Blocked(Blocker::DeveloperModeOff { udid, after_restart: waited.restarted }));
        }
        Err(e) => return Err(device_error(e, &udid, waited)),
    }

    // The developer disk image.
    match on_device.image_mounted().await {
        Ok(true) => {}
        Ok(false) => mount_image(&on_device, opts, progress, &udid).await?,
        Err(e) => return Err(device_error(e, &udid, waited)),
    }
    Ok(ReadyDevice { udid, usbmuxd: mux.spec().to_string(), mux: mux.clone(), device })
}

/// Mounts the developer disk image from the first source that has one.
async fn mount_image<O: Ops>(
    on_device: &O,
    opts: &PreflightOptions,
    progress: &mut (dyn FnMut(&PreflightStep) + Send),
    udid: &str,
) -> Result<(), PreflightError> {
    if !opts.mount_ddi {
        return Err(PreflightError::Failed(format!(
            "the developer disk image is not mounted on {udid}: run `scrios mount-ddi` (or `scrios`, which mounts it)"
        )));
    }
    let found = Box::pin(opts.ddi.find_or_download(&mut |host: &str| {
        progress(&PreflightStep::Downloading { host: host.to_string() });
    }))
    .await
    .map_err(PreflightError::Failed)?;
    progress(&PreflightStep::Mounting {
        from: format!(
            "{} ({}{})",
            found.folder.root().display(),
            found.origin.label(),
            found.folder.build().map_or_else(String::new, |b| format!(", build {b}"))
        ),
    });
    let mut last = u8::MAX;
    let mut report = |percent: u8| {
        let tenth = percent / 10;
        if tenth != last {
            last = tenth;
            progress(&PreflightStep::Uploading { percent: tenth.saturating_mul(10) });
        }
    };
    match on_device.mount(&found, &mut report).await {
        Ok(_) => {
            progress(&PreflightStep::Mounted);
            Ok(())
        }
        Err(DevError::NotTrusted) => Err(PreflightError::Blocked(Blocker::NotTrusted { udid: udid.to_string() })),
        Err(DevError::Locked) => Err(PreflightError::Blocked(Blocker::Locked)),
        Err(DevError::Failed(why)) => Err(PreflightError::Failed(why)),
    }
}

/// A device operation failed: a blocker the user can cure, or (for a device that answered before and may be
/// restarting) one to wait out, or a failure.
fn device_error(e: DevError, udid: &str, waited: &mut Waited) -> PreflightError {
    match e {
        DevError::NotTrusted => PreflightError::Blocked(Blocker::NotTrusted { udid: udid.to_string() }),
        DevError::Locked => PreflightError::Blocked(Blocker::Locked),
        DevError::Failed(why) if waited.last.is_some() && waited.unresponsive < MAX_UNRESPONSIVE => {
            waited.unresponsive += 1;
            PreflightError::Blocked(Blocker::Unresponsive(why))
        }
        DevError::Failed(why) => PreflightError::Failed(why),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use super::*;
    use crate::ddi::DdiSources;
    use crate::testdir::TempDir;
    use crate::testmux::{FakeMux, pair_record_bytes};
    use crate::testops::FakeDevice;

    const UDID: &str = "00008110-001234567890401E";
    const OTHER: &str = "00008120-000A1B2C3D4E5F60";

    /// A DDI folder that mounts (a manifest with an identity that names an image that is there).
    fn ddi_folder() -> (TempDir, DdiSources) {
        let dir = TempDir::new("preflight-ddi");
        let mut info = plist::Dictionary::new();
        info.insert("Path".into(), "a.dmg".into());
        let mut entry = plist::Dictionary::new();
        entry.insert("Info".into(), plist::Value::Dictionary(info));
        let mut manifest = plist::Dictionary::new();
        manifest.insert("PersonalizedDMG".into(), plist::Value::Dictionary(entry));
        let mut identity = plist::Dictionary::new();
        identity.insert("Manifest".into(), plist::Value::Dictionary(manifest));
        let mut top = plist::Dictionary::new();
        top.insert("BuildIdentities".into(), plist::Value::Array(vec![plist::Value::Dictionary(identity)]));
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &plist::Value::Dictionary(top)).unwrap();
        dir.write("BuildManifest.plist", &bytes);
        dir.write("a.dmg", b"image");
        let sources = DdiSources { dir: Some(dir.path().to_path_buf()), ..DdiSources::default() };
        (dir, sources)
    }

    fn options(fake: &FakeMux, wait: bool) -> PreflightOptions {
        PreflightOptions {
            target: Target { udid: None, usbmuxd: Some(fake.spec().to_string()) },
            wait,
            poll: Duration::from_millis(5),
            ..PreflightOptions::default()
        }
    }

    /// A usbmuxd with one trusted device.
    fn trusted() -> FakeMux {
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        fake
    }

    async fn go(
        opts: &PreflightOptions,
        device: &FakeDevice,
    ) -> (Result<ReadyDevice, PreflightError>, Vec<PreflightStep>) {
        let mut steps = Vec::new();
        let result = run(opts, &mut |step: &PreflightStep| steps.push(step.clone()), |_, _| device.clone()).await;
        (result, steps)
    }

    fn lines(steps: &[PreflightStep]) -> Vec<String> {
        steps.iter().filter_map(PreflightStep::line).collect()
    }

    #[tokio::test]
    async fn a_device_that_has_everything_is_ready_at_once_and_says_nothing() {
        let fake = trusted();
        let (result, steps) = go(&options(&fake, true), &FakeDevice::default()).await;
        let ready = result.unwrap();
        assert_eq!(ready.udid, UDID);
        assert_eq!(ready.usbmuxd, fake.spec());
        assert_eq!(steps, vec![PreflightStep::Ready { udid: UDID.into() }]);
        assert!(lines(&steps).is_empty());
        // What a stream needs is the device that was checked, through the usbmuxd it was found through.
        let o = ready.stream_options(StreamOptions { display_id: 7, ..StreamOptions::default() });
        assert_eq!((o.udid.as_deref(), o.usbmuxd.as_deref(), o.display_id), (Some(UDID), Some(fake.spec()), 7));
        assert_eq!(ready.handle().udid(), UDID);
    }

    #[tokio::test]
    async fn without_waiting_the_first_missing_step_is_the_error() {
        let fake = FakeMux::tcp();
        let device = FakeDevice::default();
        let (result, steps) = go(&options(&fake, false), &device).await;
        assert_eq!(result.unwrap_err(), PreflightError::Blocked(Blocker::NoDevice));
        assert!(steps.is_empty());
        fake.attach_usb(UDID, 1);
        let (result, _) = go(&options(&fake, false), &device).await;
        let why = result.unwrap_err();
        assert_eq!(why, PreflightError::Blocked(Blocker::NotTrusted { udid: UDID.into() }));
        assert!(why.to_string().contains("tap Trust"));
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        device.with(|s| s.developer_mode.push_back(Ok(false)));
        let (result, _) = go(&options(&fake, false), &device).await;
        assert!(matches!(result.unwrap_err(), PreflightError::Blocked(Blocker::DeveloperModeOff { .. })));
        // Not mounted, and mounting is off: the way to mount it is the message.
        device.with(|s| {
            s.developer_mode = VecDeque::from([Ok(true)]);
            s.mounted = VecDeque::from([Ok(false)]);
        });
        let (result, _) = go(&PreflightOptions { mount_ddi: false, ..options(&fake, false) }, &device).await;
        assert!(matches!(result.unwrap_err(), PreflightError::Failed(why) if why.contains("scrios mount-ddi")));
    }

    #[tokio::test]
    async fn no_device_is_waited_for_with_one_line_until_one_is_plugged_in_and_trusted() {
        let fake = Arc::new(FakeMux::tcp());
        let plug = fake.clone();
        let plugger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            plug.attach_usb(UDID, 1);
            std::thread::sleep(Duration::from_millis(100));
            plug.set_pair_record(UDID, pair_record_bytes(UDID));
        });
        let (result, steps) = go(&options(&fake, true), &FakeDevice::default()).await;
        plugger.join().unwrap();
        assert_eq!(result.unwrap().udid, UDID);
        // Each thing waited for is said once, however long the wait.
        assert_eq!(
            lines(&steps),
            vec!["Plug in and unlock your iPhone or iPad".to_string(), "Unlock the device and tap Trust".to_string()]
        );
    }

    #[tokio::test]
    async fn a_udid_that_is_not_connected_is_waited_for_by_name() {
        let fake = Arc::new(trusted());
        let mut opts = options(&fake, true);
        opts.target.udid = Some(OTHER.into());
        let plug = fake.clone();
        let plugger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            plug.attach_usb(OTHER, 2);
            plug.set_pair_record(OTHER, pair_record_bytes(OTHER));
        });
        let (result, steps) = go(&opts, &FakeDevice::default()).await;
        plugger.join().unwrap();
        assert_eq!(result.unwrap().udid, OTHER);
        let said = lines(&steps);
        assert_eq!(said.len(), 1);
        assert!(said[0].contains(OTHER) && said[0].contains(UDID), "{said:?}");
    }

    #[tokio::test]
    async fn several_devices_and_no_udid_fail_at_once_even_when_waiting() {
        let fake = trusted();
        fake.attach_usb(OTHER, 2);
        let (result, steps) = go(&options(&fake, true), &FakeDevice::default()).await;
        let PreflightError::Failed(why) = result.unwrap_err() else { panic!() };
        assert!(why.contains("--udid") && why.contains(UDID) && why.contains(OTHER), "{why}");
        assert!(steps.is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_usbmuxd_fails_at_once_even_when_waiting() {
        let opts = PreflightOptions {
            target: Target { udid: None, usbmuxd: Some("127.0.0.1:1".into()) },
            ..PreflightOptions::default()
        };
        let (result, steps) = go(&opts, &FakeDevice::default()).await;
        let PreflightError::Failed(why) = result.unwrap_err() else { panic!() };
        assert!(why.contains("cannot connect to usbmuxd at 127.0.0.1:1") && why.contains(usbmuxd_hint()), "{why}");
        assert!(steps.is_empty());
        let bad = PreflightOptions { target: Target { udid: None, usbmuxd: Some("host:notaport".into()) }, ..opts };
        assert!(matches!(go(&bad, &FakeDevice::default()).await.0, Err(PreflightError::Failed(_))));
    }

    #[tokio::test]
    async fn developer_mode_off_reveals_the_switch_says_the_steps_and_waits_but_never_turns_it_on() {
        let fake = trusted();
        let device = FakeDevice::default();
        device.with(|s| s.developer_mode = VecDeque::from([Ok(false), Ok(false), Ok(false), Ok(true)]));
        let (result, steps) = go(&options(&fake, true), &device).await;
        assert!(result.is_ok());
        assert_eq!(steps[0], PreflightStep::DeveloperModeRevealed);
        let said = lines(&steps);
        assert_eq!(said.len(), 2, "{said:?}");
        for want in ["Settings > Privacy & Security > Developer Mode > On", "restart"] {
            assert!(said[1].contains(want), "{want} missing from {said:?}");
        }
        // The switch is revealed once. (The fake has no way to turn Developer Mode on: the preflight has none either.)
        device.with(|s| assert_eq!(s.reveals, 1));
    }

    #[tokio::test]
    async fn a_restart_for_developer_mode_is_waited_out_and_the_confirmation_is_asked_for() {
        let fake = Arc::new(trusted());
        let device = FakeDevice::default();
        // Off until the device has been away and back and the Turn On prompt has been answered.
        device.with(|s| s.developer_mode = VecDeque::from([Ok(false)]));
        let (unplug, answers) = (fake.clone(), device.clone());
        let restarter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            unplug.detach_all();
            std::thread::sleep(Duration::from_millis(100));
            unplug.attach_usb(UDID, 1);
            std::thread::sleep(Duration::from_millis(80));
            answers.with(|s| s.developer_mode = VecDeque::from([Ok(true)]));
        });
        let (result, steps) = go(&options(&fake, true), &device).await;
        restarter.join().unwrap();
        assert!(result.is_ok());
        let said = lines(&steps);
        assert_eq!(said.len(), 4, "{said:?}");
        assert!(said[0].starts_with("The Developer Mode switch is now in"), "{said:?}");
        assert!(said[1].contains("Turn Developer Mode on"), "{said:?}");
        assert_eq!(said[2], "Waiting for the device to restart");
        assert!(said[3].contains("tap Turn On"), "{said:?}");
    }

    #[tokio::test]
    async fn a_device_that_is_not_mounted_gets_the_image_from_the_first_source_with_progress_in_tens() {
        let fake = trusted();
        let (_dir, ddi) = ddi_folder();
        let device = FakeDevice::default();
        device.with(|s| s.mounted = VecDeque::from([Ok(false)]));
        let opts = PreflightOptions { ddi, ..options(&fake, true) };
        let (result, steps) = go(&opts, &device).await;
        assert!(result.is_ok());
        device.with(|s| assert_eq!(s.mounts, 1));
        let percents: Vec<u8> = steps
            .iter()
            .filter_map(|s| if let PreflightStep::Uploading { percent } = s { Some(*percent) } else { None })
            .collect();
        assert_eq!(percents, vec![0, 10, 50, 90, 100]);
        let said = lines(&steps);
        assert!(
            said[0].starts_with("Mounting the developer disk image from ") && said[0].contains("--ddi-dir"),
            "{said:?}"
        );
        assert!(said[0].contains("needs internet"), "{said:?}");
        assert_eq!(said.last().unwrap(), "The developer disk image is mounted");
    }

    #[tokio::test]
    async fn with_no_image_to_mount_the_instruction_is_the_error_and_nothing_is_mounted() {
        let fake = trusted();
        let device = FakeDevice::default();
        device.with(|s| s.mounted = VecDeque::from([Ok(false)]));
        let ddi = DdiSources { cache: Some("/nonexistent/cache/scrios/ddi".into()), ..DdiSources::default() };
        let (result, _) = go(&PreflightOptions { ddi, ..options(&fake, true) }, &device).await;
        let PreflightError::Failed(why) = result.unwrap_err() else { panic!() };
        assert!(why.contains("no developer disk image") && why.contains("copy the iOS_DDI folder"), "{why}");
        device.with(|s| assert_eq!(s.mounts, 0));
    }

    #[tokio::test]
    async fn a_mount_that_fails_is_an_error_and_a_locked_device_is_waited_for() {
        let fake = trusted();
        let (_dir, ddi) = ddi_folder();
        let device = FakeDevice::default();
        device.with(|s| {
            s.mounted = VecDeque::from([Ok(false)]);
            s.mount = VecDeque::from([Err(DevError::Failed("TSS said no".into()))]);
        });
        let opts = PreflightOptions { ddi, ..options(&fake, true) };
        let (result, _) = go(&opts, &device).await;
        assert_eq!(result.unwrap_err(), PreflightError::Failed("TSS said no".into()));
        // A locked device is a wait, not a failure.
        device.with(|s| s.mount = VecDeque::from([Err(DevError::Locked), Ok(Mounted::Now)]));
        let (result, steps) = go(&opts, &device).await;
        assert!(result.is_ok());
        assert!(lines(&steps).contains(&"Unlock the device".to_string()));
    }

    #[tokio::test]
    async fn a_device_that_stops_answering_after_a_wait_is_waited_for_and_before_it_is_an_error() {
        let fake = trusted();
        let device = FakeDevice::default();
        device.with(|s| s.developer_mode = VecDeque::from([Err(DevError::Failed("connection reset".into()))]));
        let (result, _) = go(&options(&fake, true), &device).await;
        assert_eq!(result.unwrap_err(), PreflightError::Failed("connection reset".into()), "the first failure is real");
        // After a wait (Developer Mode off), the same failure is the device restarting.
        device.with(|s| {
            s.developer_mode = VecDeque::from([
                Ok(false),
                Err(DevError::Failed("connection reset".into())),
                Err(DevError::Failed("connection reset".into())),
                Ok(true),
            ]);
        });
        let (result, steps) = go(&options(&fake, true), &device).await;
        assert!(result.is_ok());
        let said = lines(&steps);
        assert!(said.iter().any(|l| l.contains("does not answer yet (connection reset)")), "{said:?}");
        assert_eq!(said.iter().filter(|l| l.contains("does not answer yet")).count(), 1, "said once: {said:?}");
    }

    #[tokio::test]
    async fn a_device_that_does_not_trust_us_when_asked_is_the_trust_wait() {
        let fake = trusted();
        let device = FakeDevice::default();
        device.with(|s| s.developer_mode = VecDeque::from([Err(DevError::NotTrusted), Ok(true)]));
        let (result, steps) = go(&options(&fake, true), &device).await;
        assert!(result.is_ok());
        assert_eq!(lines(&steps), vec!["Unlock the device and tap Trust".to_string()]);
    }

    #[test]
    fn the_preflight_can_run_on_any_thread() {
        fn is_send<T: Send>(_: &T) {}
        let opts = PreflightOptions::default();
        let mut progress = |_: &PreflightStep| {};
        let future = ensure_ready(&opts, &mut progress);
        is_send(&future);
    }

    #[test]
    fn every_blocker_has_a_line_to_wait_with_and_a_sentence_to_fail_with() {
        let blockers = [
            Blocker::NoDevice,
            Blocker::UnknownUdid { udid: UDID.into(), listed: vec![] },
            Blocker::NotTrusted { udid: UDID.into() },
            Blocker::Locked,
            Blocker::DeveloperModeOff { udid: UDID.into(), after_restart: false },
            Blocker::DeveloperModeOff { udid: UDID.into(), after_restart: true },
            Blocker::Restarting,
            Blocker::Unresponsive("x".into()),
        ];
        for b in blockers {
            assert!(!b.wait_line().is_empty() && !b.error().is_empty(), "{b:?}");
            assert_eq!(PreflightError::Blocked(b.clone()).message(), b.error());
        }
        let off = Blocker::DeveloperModeOff { udid: UDID.into(), after_restart: false };
        assert!(off.error().contains("never turns it on by itself"));
    }
}
