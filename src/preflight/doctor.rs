//! `scrios doctor`: the checks of the preflight, run one after the other without waiting, each with the fix for what
//! it finds.
//!
//! The checks are the preflight's own (the same functions find the device and read its pair record, the same
//! device operations ask for Developer Mode and the mounted image), so what the doctor says is what a stream would
//! stop on. A check that needs one that failed is skipped. A program that has more to check (the viewer's decoder) adds
//! its own with [`Report::push`].

use std::fmt::Write as _;

use super::{Blocker, Ops, Target, listed_or_nothing};
use crate::ddi::DdiSources;
use crate::mux::{Choice, Mux, PairLookup, choose, usbmuxd_hint};
use crate::ops::{DevError, Device, Route};

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// What it looks at is in place.
    Ok,
    /// It is not (the check's `fix` says what to do).
    Fail,
    /// It could not be looked at because a check before it failed.
    Skipped,
}

/// One check: what was looked at, what was found, and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What it looks at: `usbmuxd`, `device`, `pairing`, `developer mode`, `developer disk image`, `image source`,
    /// `display service`.
    pub name: String,
    pub status: Status,
    /// What was found.
    pub detail: String,
    /// What to do about it, for a check that failed.
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Check {
        Check { name: name.into(), status: Status::Ok, detail: detail.into(), fix: None }
    }

    fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check { name: name.into(), status: Status::Fail, detail: detail.into(), fix: Some(fix.into()) }
    }

    fn skipped(name: &str, because: &str) -> Check {
        Check { name: name.into(), status: Status::Skipped, detail: format!("needs {because}"), fix: None }
    }
}

/// The checks, in the order they were made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Adds a check at the end: the hook for a check of the program's own (the viewer's decoder).
    pub fn push(&mut self, check: Check) {
        self.checks.push(check);
    }

    /// Whether no check failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.status != Status::Fail)
    }

    /// The report as text: a line for each check (`ok`, `FAIL` or `skip`), and under a failed one, its fix.
    #[must_use]
    pub fn render(&self) -> String {
        let width = self.checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        let mut out = String::new();
        for check in &self.checks {
            let tag = match check.status {
                Status::Ok => "ok",
                Status::Fail => "FAIL",
                Status::Skipped => "skip",
            };
            let _ = writeln!(out, "{tag:<5} {:<width$}  {}", check.name, check.detail);
            if let Some(fix) = &check.fix {
                let _ = writeln!(out, "{:<5} {:<width$}  fix: {fix}", "", "");
            }
        }
        out
    }
}

/// What the doctor looks at.
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    /// The device and the usbmuxd to find it through.
    pub target: Target,
    /// Where the developer disk image would come from.
    pub ddi: DdiSources,
}

/// Runs the checks against the device.
pub async fn run(opts: &DoctorOptions) -> Report {
    run_with(opts, Device::new).await
}

/// [`run`] with the device operations given.
pub(crate) async fn run_with<O: Ops>(
    opts: &DoctorOptions,
    make: impl Fn(&Mux, &idevice::usbmuxd::UsbmuxdDevice) -> O,
) -> Report {
    let mut report = Report::default();
    let Some(mux) = check_usbmuxd(&opts.target, &mut report).await else {
        for name in ["device", "pairing", "developer mode", "developer disk image"] {
            report.push(Check::skipped(name, "usbmuxd"));
        }
        report.push(image_source(&opts.ddi, None, None));
        report.push(Check::skipped("display service", "usbmuxd"));
        return report;
    };
    let device = check_device(&mux, opts.target.udid.as_deref(), &mut report).await;
    let Some(device) = device else {
        for name in ["pairing", "developer mode", "developer disk image"] {
            report.push(Check::skipped(name, "a device"));
        }
        report.push(image_source(&opts.ddi, None, None));
        report.push(Check::skipped("display service", "a device"));
        return report;
    };
    if !check_pairing(&mux, &device.udid, &mut report).await {
        for name in ["developer mode", "developer disk image"] {
            report.push(Check::skipped(name, "a device that trusts this computer"));
        }
        report.push(image_source(&opts.ddi, None, None));
        report.push(Check::skipped("display service", "a device that trusts this computer"));
        return report;
    }
    let on_device = make(&mux, &device);
    let mode_ok = check_developer_mode(&on_device, &device.udid, &mut report).await;
    // The display service is the sure sign that the image is in place (on recent iOS the image mounter may not
    // report a personalized image that is mounted): ask for it first, and the image mounter only when it is absent.
    let display = if mode_ok { Some(check_display_service(&on_device).await) } else { None };
    let offered = display.as_ref().is_some_and(|check| check.status == Status::Ok);
    // The cryptex that cryptexd lists as installed is the sure word on a cryptex install; it is told when there is one.
    let cryptex = on_device
        .installed_cryptex()
        .await
        .ok()
        .flatten()
        .map_or_else(String::new, |c| format!("; cryptex {} {} is installed", c.identifier, c.version));
    let mounted = if offered {
        report.push(Check::ok(
            "developer disk image",
            format!("mounted (the device offers the display service{cryptex})"),
        ));
        Some(true)
    } else {
        check_image_mounted(&on_device, &cryptex, &mut report).await
    };
    // Which form of the image the device takes decides which folder is any use.
    let route = if mode_ok { on_device.route().await.ok() } else { None };
    report.push(image_source(&opts.ddi, Some(mounted == Some(true)), route));
    report.push(display.unwrap_or_else(|| Check::skipped("display service", "Developer Mode")));
    report
}

async fn check_usbmuxd(target: &Target, report: &mut Report) -> Option<Mux> {
    let mux = match Mux::resolve(target.usbmuxd.as_deref()) {
        Ok(mux) => mux,
        Err(why) => {
            report.push(Check::fail("usbmuxd", why, format!("give --usbmuxd as {}", crate::mux::address_forms())));
            return None;
        }
    };
    match mux.devices().await {
        Ok(devices) => {
            report
                .push(Check::ok("usbmuxd", format!("reachable at {} (lists {} device(s))", mux.spec(), devices.len())));
            Some(mux)
        }
        Err(why) => {
            report.push(Check::fail("usbmuxd", why, usbmuxd_hint()));
            None
        }
    }
}

async fn check_device(mux: &Mux, udid: Option<&str>, report: &mut Report) -> Option<idevice::usbmuxd::UsbmuxdDevice> {
    let devices = match mux.devices().await {
        Ok(devices) => devices,
        Err(why) => {
            report.push(Check::fail("device", why, usbmuxd_hint()));
            return None;
        }
    };
    match choose(&devices, udid) {
        Choice::Found(device) => {
            let listed = crate::list::Listed::from(device);
            report.push(Check::ok("device", format!("{} ({})", listed.udid, listed.connection)));
            Some(device.clone())
        }
        Choice::NoDevice => {
            report.push(Check::fail("device", "no device is connected", Blocker::NoDevice.wait_line()));
            None
        }
        Choice::UnknownUdid(listed) => {
            let udid = udid.unwrap_or_default();
            report.push(Check::fail(
                "device",
                format!("no device with UDID {udid} (usbmuxd lists: {})", listed_or_nothing(&listed)),
                "plug in and unlock that device, or give the UDID of one that is listed (`scrios list`)",
            ));
            None
        }
        Choice::Several(udids) => {
            report.push(Check::fail(
                "device",
                format!("several devices are connected ({})", udids.join(", ")),
                "pass --udid with the one to use",
            ));
            None
        }
    }
}

async fn check_pairing(mux: &Mux, udid: &str, report: &mut Report) -> bool {
    match mux.pair_record(udid).await {
        Ok(PairLookup::Present) => {
            report.push(Check::ok("pairing", "usbmuxd holds the pair record of the device"));
            true
        }
        Ok(PairLookup::Missing) => {
            let blocker = Blocker::NotTrusted { udid: udid.to_string() };
            report.push(Check::fail("pairing", "usbmuxd has no pair record for the device", blocker.error()));
            false
        }
        Ok(PairLookup::Unreadable(why)) => {
            report.push(Check::fail(
                "pairing",
                format!("usbmuxd's pair record cannot be read ({why})"),
                "unlock the device and tap Trust again; if that does not help, remove the stale record with your system's pairing tool",
            ));
            false
        }
        Err(why) => {
            report.push(Check::fail("pairing", why, usbmuxd_hint()));
            false
        }
    }
}

/// The fix for a device operation that failed, and whether it is something to do with the device's trust.
fn device_failure(name: &str, e: &DevError, report: &mut Report) {
    let fix = match e {
        DevError::NotTrusted => Blocker::NotTrusted { udid: String::new() }.wait_line(),
        DevError::Locked => Blocker::Locked.wait_line(),
        DevError::Failed(_) => "unlock the device and run `scrios doctor` again".to_string(),
    };
    report.push(Check::fail(name, e.to_string(), fix));
}

async fn check_developer_mode<O: Ops>(on_device: &O, udid: &str, report: &mut Report) -> bool {
    match on_device.developer_mode().await {
        Ok(true) => {
            report.push(Check::ok("developer mode", "on"));
            true
        }
        Ok(false) => {
            let steps = Blocker::DeveloperModeOff { udid: udid.to_string(), after_restart: false }.wait_line();
            report.push(Check::fail(
                "developer mode",
                "off",
                format!("{steps} (`scrios developer-mode` shows the switch in Settings; scrios never turns it on unless `--enable --yes` is given)"),
            ));
            false
        }
        Err(e) => {
            device_failure("developer mode", &e, report);
            false
        }
    }
}

async fn check_image_mounted<O: Ops>(on_device: &O, cryptex: &str, report: &mut Report) -> Option<bool> {
    match on_device.image_mounted().await {
        Ok(true) => {
            let note = cryptex.strip_prefix("; ").map_or_else(String::new, |note| format!(" ({note})"));
            report.push(Check::ok("developer disk image", format!("mounted{note}")));
            Some(true)
        }
        Ok(false) => {
            report.push(Check::fail(
                "developer disk image",
                "not mounted",
                "run `scrios mount-ddi` (plain `scrios` mounts it too); the first mount after each restart of the device needs internet",
            ));
            Some(false)
        }
        Err(e) => {
            device_failure("developer disk image", &e, report);
            None
        }
    }
}

/// Where the image to install would come from. `mounted`: whether the device has it already (then not finding one is
/// no failure), if that is known. `route`: how the device takes the image, if that is known: it decides which form of
/// the image is any use.
fn image_source(ddi: &DdiSources, mounted: Option<bool>, route: Option<Route>) -> Check {
    let located = match route {
        Some(route) => ddi.locate_for(route.variant()),
        None => ddi.locate(),
    };
    match located {
        Ok(found) => {
            let form = route.map_or_else(String::new, |route| format!("{}: ", route.variant().label()));
            Check::ok("image source", format!("{form}{}", found.describe()))
        }
        Err(why) if mounted == Some(true) => {
            Check::ok("image source", format!("none found, not needed now: the image is mounted ({why})"))
        }
        Err(why) => {
            let explicit = ddi.dir.is_some() || ddi.env_dir.is_some();
            if explicit {
                Check::fail("image source", why, "point --ddi-dir (or $SCRIOS_DDI_DIR) at the iOS_DDI folder of Xcode")
            } else {
                Check::fail("image source", "none found", why)
            }
        }
    }
}

async fn check_display_service<O: Ops>(on_device: &O) -> Check {
    match on_device.display_service_offered().await {
        Ok(true) => Check::ok("display service", "the device offers it"),
        Ok(false) => Check::fail(
            "display service",
            "the device does not offer it",
            "it needs iOS 27 or later with Developer Mode on and the developer disk image mounted; restart the device if all of that is so",
        ),
        Err(e) => {
            let mut report = Report::default();
            device_failure("display service", &e, &mut report);
            report.checks.pop().unwrap_or_else(|| Check::skipped("display service", "an answer"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::testdir::TempDir;
    use crate::testmux::{FakeMux, pair_record_bytes};
    use crate::testops::FakeDevice;

    const UDID: &str = "00008110-001234567890401E";

    fn opts(fake: &FakeMux, ddi: DdiSources) -> DoctorOptions {
        DoctorOptions { target: Target { udid: None, usbmuxd: Some(fake.spec().to_string()) }, ddi }
    }

    fn statuses(report: &Report) -> Vec<(&str, Status)> {
        report.checks.iter().map(|c| (c.name.as_str(), c.status)).collect()
    }

    fn ddi_folder() -> (TempDir, DdiSources) {
        let dir = TempDir::new("doctor-ddi");
        let manifest = {
            let mut info = plist::Dictionary::new();
            info.insert("Path".into(), "a.dmg".into());
            let mut entry = plist::Dictionary::new();
            entry.insert("Info".into(), plist::Value::Dictionary(info));
            let mut inner = plist::Dictionary::new();
            inner.insert("PersonalizedDMG".into(), plist::Value::Dictionary(entry));
            let mut identity = plist::Dictionary::new();
            identity.insert("Manifest".into(), plist::Value::Dictionary(inner));
            let mut top = plist::Dictionary::new();
            top.insert("ProductBuildVersion".into(), "27B48".into());
            top.insert("BuildIdentities".into(), plist::Value::Array(vec![plist::Value::Dictionary(identity)]));
            let mut bytes = Vec::new();
            plist::to_writer_xml(&mut bytes, &plist::Value::Dictionary(top)).unwrap();
            bytes
        };
        dir.write("BuildManifest.plist", &manifest);
        dir.write("a.dmg", b"image");
        let ddi = DdiSources { dir: Some(dir.path().to_path_buf()), ..DdiSources::default() };
        (dir, ddi)
    }

    #[tokio::test]
    async fn a_device_that_is_all_set_passes_every_check() {
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        let (_dir, ddi) = ddi_folder();
        let device = FakeDevice::default();
        let report = run_with(&opts(&fake, ddi), |_, _| device.clone()).await;
        assert!(report.passed(), "{}", report.render());
        assert_eq!(
            statuses(&report),
            vec![
                ("usbmuxd", Status::Ok),
                ("device", Status::Ok),
                ("pairing", Status::Ok),
                ("developer mode", Status::Ok),
                ("developer disk image", Status::Ok),
                ("image source", Status::Ok),
                ("display service", Status::Ok),
            ]
        );
        let text = report.render();
        assert!(text.contains("ok    usbmuxd") && text.contains(UDID) && text.contains("build 27B48"), "{text}");
        assert!(!text.contains("fix:"), "{text}");
    }

    #[tokio::test]
    async fn no_usbmuxd_fails_the_first_check_and_skips_what_needs_it() {
        let report = run_with(
            &DoctorOptions {
                target: Target { udid: None, usbmuxd: Some("127.0.0.1:1".into()) },
                ddi: DdiSources { cache: Some("/nonexistent/cache/ddi".into()), ..DdiSources::default() },
            },
            |_, _| FakeDevice::default(),
        )
        .await;
        assert!(!report.passed());
        let by_name = statuses(&report);
        assert_eq!(by_name[0], ("usbmuxd", Status::Fail));
        assert!(report.checks[0].fix.as_deref().is_some_and(|f| f == usbmuxd_hint()));
        assert!(by_name[1..].iter().all(|(name, status)| *status == Status::Skipped || *name == "image source"));
        let text = report.render();
        assert!(text.starts_with("FAIL  usbmuxd") && text.contains("fix: "), "{text}");
    }

    #[tokio::test]
    async fn each_missing_thing_is_a_failed_check_with_its_fix() {
        let fake = FakeMux::tcp();
        let ddi = DdiSources { cache: Some("/nonexistent/cache/scrios/ddi".into()), ..DdiSources::default() };
        // No device.
        let report = run_with(&opts(&fake, ddi.clone()), |_, _| FakeDevice::default()).await;
        assert_eq!(report.checks[1].status, Status::Fail);
        assert_eq!(report.checks[1].fix.as_deref(), Some("Plug in and unlock your iPhone or iPad"));
        assert!(report.checks[2..5].iter().all(|c| c.status == Status::Skipped));
        // A device that is not trusted.
        fake.attach_usb(UDID, 1);
        let report = run_with(&opts(&fake, ddi.clone()), |_, _| FakeDevice::default()).await;
        assert_eq!(report.checks[2].status, Status::Fail);
        assert!(report.checks[2].fix.as_deref().is_some_and(|f| f.contains("tap Trust")));
        assert_eq!(report.checks[3].status, Status::Skipped);
        // Trusted, with Developer Mode off and nothing mounted, and no image to mount.
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        let device = FakeDevice::default();
        device.with(|s| {
            s.developer_mode = VecDeque::from([Ok(false)]);
            s.mounted = VecDeque::from([Ok(false)]);
        });
        let report = run_with(&opts(&fake, ddi), |_, _| device.clone()).await;
        assert_eq!(
            statuses(&report)[3..],
            [
                ("developer mode", Status::Fail),
                ("developer disk image", Status::Fail),
                ("image source", Status::Fail),
                ("display service", Status::Skipped),
            ]
        );
        assert!(
            report.checks[3]
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("Settings > Privacy & Security > Developer Mode"))
        );
        assert!(report.checks[4].fix.as_deref().is_some_and(|f| f.contains("scrios mount-ddi")));
        assert!(report.checks[5].fix.as_deref().is_some_and(|f| f.contains("copy the iOS_DDI folder")));
        let text = report.render();
        assert_eq!(text.matches("FAIL").count(), 3, "{text}");
        assert_eq!(text.matches("fix: ").count(), 3, "{text}");
    }

    #[tokio::test]
    async fn a_device_that_offers_the_display_service_has_its_image_whatever_the_image_mounter_says() {
        // iOS 27 can mount the personalized image as a MobileAsset cryptex that LookupImage does not report.
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        let device = FakeDevice::default();
        device.with(|s| {
            s.mounted = VecDeque::from([Ok(false)]);
            s.display_service = VecDeque::from([Ok(true)]);
        });
        let ddi = DdiSources { cache: Some("/nonexistent/cache/scrios/ddi".into()), ..DdiSources::default() };
        let report = run_with(&opts(&fake, ddi), |_, _| device.clone()).await;
        assert!(report.passed(), "{}", report.render());
        let by_name = statuses(&report);
        assert!(by_name.contains(&("developer disk image", Status::Ok)));
        assert!(by_name.contains(&("display service", Status::Ok)));
        assert_eq!(device.with(|s| s.mounts), 0);
    }

    #[tokio::test]
    async fn an_image_that_is_mounted_needs_no_source_and_the_service_can_still_be_missing() {
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        let device = FakeDevice::default();
        device.with(|s| s.display_service = VecDeque::from([Ok(false)]));
        let ddi = DdiSources { cache: Some("/nonexistent/cache/scrios/ddi".into()), ..DdiSources::default() };
        let report = run_with(&opts(&fake, ddi), |_, _| device.clone()).await;
        let by_name = statuses(&report);
        assert_eq!(by_name[5], ("image source", Status::Ok));
        assert_eq!(by_name[6], ("display service", Status::Fail));
        assert!(report.checks[5].detail.contains("not needed now"));
        assert!(report.checks[6].fix.as_deref().is_some_and(|f| f.contains("iOS 27")));
    }

    #[tokio::test]
    async fn on_the_cryptexd_route_the_cryptex_is_told_and_the_folder_that_counts_is_one_with_a_cryptex() {
        use crate::ops::Route;
        use crate::testddi;
        let fake = FakeMux::tcp();
        fake.attach_usb(UDID, 1);
        fake.set_pair_record(UDID, pair_record_bytes(UDID));
        let device = FakeDevice::default();
        device.with(|s| {
            s.route = VecDeque::from([Ok(Route::Cryptexd)]);
            s.display_service = VecDeque::from([Ok(true)]);
            s.installed = VecDeque::from([Ok(Some(crate::testops::fake_cryptex()))]);
        });
        let cryptex = TempDir::new("doctor-cryptex");
        testddi::cryptex_folder(&cryptex);
        let ddi = DdiSources { dir: Some(cryptex.path().to_path_buf()), ..DdiSources::default() };
        let report = run_with(&opts(&fake, ddi), |_, _| device.clone()).await;
        assert!(report.passed(), "{}", report.render());
        assert!(
            report.checks[4].detail.contains("cryptex com.apple.MobileAsset.DDI 27.2.52.0 is installed"),
            "{}",
            report.render()
        );
        assert!(report.checks[5].detail.starts_with("cryptex: "), "{}", report.render());
        // A folder with no cryptex, for a device that takes one and has none: the source check fails and says why.
        let device = FakeDevice::default();
        device.with(|s| {
            s.route = VecDeque::from([Ok(Route::Cryptexd)]);
            s.mounted = VecDeque::from([Ok(false)]);
        });
        let (_dir, personalized) = ddi_folder();
        let xcode = DdiSources { dir: None, xcode: vec![personalized.dir.clone().unwrap()], ..personalized };
        let report = run_with(&opts(&fake, xcode), |_, _| device.clone()).await;
        assert_eq!(report.checks[5].status, Status::Fail, "{}", report.render());
        assert!(report.checks[5].fix.as_deref().is_some_and(|f| f.contains("cryptex")), "{}", report.render());
    }

    #[tokio::test]
    async fn a_program_can_add_its_own_check() {
        let fake = FakeMux::tcp();
        let mut report = run_with(&opts(&fake, DdiSources::default()), |_, _| FakeDevice::default()).await;
        let before = report.checks.len();
        report.push(Check::ok("decoder", "H.264 hardware decoding is available"));
        assert_eq!(report.checks.len(), before + 1);
        assert!(report.render().contains("decoder"));
    }
}
