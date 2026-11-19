//! The commands that manage the device rather than stream it: `ddi-status`, `mount-ddi`, `unmount-ddi`, `doctor`,
//! `developer-mode`.
//! They print what they find on standard output and what they are doing on standard error.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use clap::Args;
use tracing::error;

use scrios::ddi::DdiSources;
use scrios::ops::{DevError, Device, Mounted, Removed};
use scrios::preflight::doctor::{self, DoctorOptions};
use scrios::preflight::{
    Blocker, PreflightOptions, PreflightStep, Target, ensure_ready, install_developer_image, open_device,
};
use scrios::stream::EXIT_START_FAILED;

/// The exit status of `ddi-status` when the device is reachable and something is missing.
const EXIT_MISSING: i32 = 1;

/// The help of `--usbmuxd`: the forms of address this host takes, and its own default.
pub const USBMUXD_HELP: &str = if cfg!(windows) {
    "usbmuxd address: host:port or TCP:host:port (default: $USBMUXD_SOCKET_ADDRESS, else 127.0.0.1:27015, the Apple \
Mobile Device Service)"
} else {
    "usbmuxd address: UNIX:/path (or a bare /path), host:port or TCP:host:port (default: $USBMUXD_SOCKET_ADDRESS, \
else /var/run/usbmuxd)"
};

/// Which device to work with.
#[derive(Args, Debug, Clone, Default)]
pub struct TargetArgs {
    /// UDID of the device (default: the only device usbmuxd lists; see `scrios list`)
    #[arg(short = 's', long, value_name = "UDID")]
    pub udid: Option<String>,

    #[arg(
        long,
        value_name = "ADDR",
        help = USBMUXD_HELP
    )]
    pub usbmuxd: Option<String>,
}

impl TargetArgs {
    fn target(&self) -> Target {
        Target { udid: self.udid.clone(), usbmuxd: self.usbmuxd.clone() }
    }
}

/// Where to find the developer disk image.
#[derive(Args, Debug, Clone, Default)]
pub struct DdiArgs {
    #[arg(
        long = "ddi-dir",
        value_name = "DIR",
        help = "The developer disk image folder to mount: the iOS_DDI folder of Xcode (or a copy of it)",
        long_help = "The developer disk image folder to mount: the iOS_DDI folder of Xcode (or a copy of it), with its \
BuildManifest.plist in it or in Restore/. Default: $SCRIOS_DDI_DIR, then Xcode's folders (macOS), then scrios's cache \
folder (~/Library/Caches/scrios/ddi on macOS, $XDG_CACHE_HOME/scrios/ddi or ~/.cache/scrios/ddi on Linux, \
%LOCALAPPDATA%\\scrios\\ddi on Windows)"
    )]
    pub dir: Option<PathBuf>,

    #[arg(
        long = "ddi-download-url",
        value_name = "URL",
        help = "Download the image from this URL if none is found (a .tar.gz of the iOS_DDI folder; needs --ddi-sha256)",
        long_help = "Where to download the image from if none is found: the URL of a .tar.gz of the iOS_DDI folder, which \
goes into the cache folder. Needs --ddi-sha256. There is no default URL: scrios does not host Apple's image (also \
$SCRIOS_DDI_URL)"
    )]
    pub download_url: Option<String>,

    #[arg(
        long = "ddi-sha256",
        value_name = "HEX",
        help = "The SHA-256 (64 hex digits) of the archive at --ddi-download-url (also $SCRIOS_DDI_SHA256)"
    )]
    pub sha256: Option<String>,
}

impl DdiArgs {
    /// The places to look for the image, with the environment's say where the options leave it open.
    pub fn sources(&self) -> Result<DdiSources, String> {
        DdiSources::from_host(self.dir.clone(), self.download_url.as_deref(), self.sha256.as_deref())
    }
}

/// Writes to standard output (a closed pipe is not an error).
fn out(text: &str) {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

/// Writes a progress line to standard error.
fn say(line: &str) {
    eprintln!("{line}");
}

fn run_async<F: std::future::Future<Output = i32>>(fut: F) -> i32 {
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(fut),
        Err(e) => {
            error!("cannot build the async runtime: {e}");
            EXIT_START_FAILED
        }
    }
}

/// One entry of the device's list of mounted images as a line: the keys that are plain values.
fn entry_line(entry: &plist::Value) -> String {
    let Some(dict) = entry.as_dictionary() else { return format!("{entry:?}") };
    let mut parts: Vec<String> = dict
        .iter()
        .filter_map(|(key, value)| {
            let text = match value {
                plist::Value::String(s) => s.clone(),
                plist::Value::Boolean(b) => b.to_string(),
                plist::Value::Integer(i) => i.to_string(),
                plist::Value::Data(d) => format!("<{} bytes>", d.len()),
                _ => return None,
            };
            Some(format!("{key}={text}"))
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

/// The `label: value` lines of a status.
#[derive(Default)]
struct Report(String);

impl Report {
    fn line(&mut self, label: &str, value: &str) {
        let _ = writeln!(self.0, "{:<17} {value}", format!("{label}:"));
    }

    /// A line that continues the one above it.
    fn more(&mut self, value: &str) {
        let _ = writeln!(self.0, "{:<17} {value}", "");
    }
}

fn dev_error(what: &str, e: &DevError) {
    error!("{what}: {e}");
}

/// `scrios ddi-status`: Developer Mode, the mounted images and installed cryptex, how the image would be installed, and
/// the folder scrios would install it from. Exit status 0 if the developer disk image is in place and Developer Mode is
/// on, 1 if one of them is not, 2 if the device could not be asked.
pub fn run_ddi_status(target: &TargetArgs, ddi: &DdiArgs) -> i32 {
    let sources = match ddi.sources() {
        Ok(s) => s,
        Err(why) => {
            error!("{why}");
            return EXIT_START_FAILED;
        }
    };
    run_async(async {
        let (mux, found) = match open_device(&target.target()).await {
            Ok(opened) => opened,
            Err(stop) => {
                error!("{}", stop.message());
                return EXIT_START_FAILED;
            }
        };
        let device = Device::new(&mux, &found);
        let mut report = Report::default();
        let listed = scrios::list::Listed::from(&found);
        report.line("Device", &format!("{} ({})", listed.udid, listed.connection));
        let mut complete = true;
        let mut failed = false;
        match device.product_version().await {
            Ok(version) => report.line("iOS", &version),
            Err(e) => {
                failed = true;
                dev_error("cannot read the iOS version", &e);
            }
        }
        match device.developer_mode().await {
            Ok(on) => {
                complete &= on;
                report.line("Developer Mode", if on { "on" } else { "off" });
            }
            Err(e) => {
                failed = true;
                dev_error("cannot read Developer Mode", &e);
            }
        }
        // The cryptex that cryptexd lists is the sure word on a cryptex install: the image mounter's list does not always
        // show it. A device that cannot be asked for it (older iOS, no cryptexd) simply has none.
        let route = device.route().await.ok();
        let cryptex = match route {
            Some(scrios::ops::Route::Cryptexd) => device.installed_ddi().await.ok().flatten(),
            _ => None,
        };
        match device.image_mounted().await {
            Ok(mounted) => {
                let mounted = mounted || cryptex.is_some();
                complete &= mounted;
                report.line("Developer image", if mounted { "mounted" } else { "not mounted" });
            }
            Err(e) => {
                failed = true;
                dev_error("cannot look up the developer image", &e);
            }
        }
        if let Ok(images) = device.mounted_images().await {
            for image in &images {
                report.more(&entry_line(image));
            }
        }
        if let Some(image) = &cryptex {
            report.line("Cryptex", &format!("{} {} (installed through cryptexd)", image.identifier, image.version));
        }
        if let Some(route) = route {
            report.line("Install route", route.describe());
        }
        let located = match route {
            Some(route) => sources.locate_for(route.variant()),
            None => sources.locate(),
        };
        match located {
            Ok(found) => report.line("Image to install", &found.describe()),
            Err(why) => report.line("Image to install", &format!("none ({why})")),
        }
        out(&report.0);
        if failed {
            EXIT_START_FAILED
        } else if complete {
            0
        } else {
            EXIT_MISSING
        }
    })
}

/// `scrios mount-ddi`: puts the developer disk image on the device, the way the device takes it: as a cryptex through
/// cryptexd (iOS 17.4 and later), else through the image mounter, from the folder the sources name.
pub fn run_mount_ddi(target: &TargetArgs, ddi: &DdiArgs) -> i32 {
    let sources = match ddi.sources() {
        Ok(s) => s,
        Err(why) => {
            error!("{why}");
            return EXIT_START_FAILED;
        }
    };
    run_async(async {
        let (mux, found) = match open_device(&target.target()).await {
            Ok(opened) => opened,
            Err(stop) => {
                error!("{}", stop.message());
                return EXIT_START_FAILED;
            }
        };
        let device = Device::new(&mux, &found);
        match device.image_mounted().await {
            Ok(true) => {
                say("The developer disk image is already mounted.");
                return 0;
            }
            Ok(false) => {}
            Err(e) => {
                dev_error("cannot look up the developer image", &e);
                return EXIT_START_FAILED;
            }
        }
        let mut print = |step: &PreflightStep| {
            if let Some(line) = step.line() {
                say(&line);
            }
        };
        match Box::pin(install_developer_image(&device, &sources, &mut print)).await {
            Ok(Mounted::Now) => {
                say("Done.");
                0
            }
            Ok(Mounted::Already) => 0,
            Err(stop) => {
                error!("cannot install the developer disk image: {}", stop.message());
                EXIT_START_FAILED
            }
        }
    })
}

/// `scrios unmount-ddi`: takes the developer disk image off the device: a cryptex is uninstalled through cryptexd, an
/// image that was mounted is unmounted through the image mounter.
pub fn run_unmount_ddi(target: &TargetArgs) -> i32 {
    run_async(async {
        let (mux, found) = match open_device(&target.target()).await {
            Ok(opened) => opened,
            Err(stop) => {
                error!("{}", stop.message());
                return EXIT_START_FAILED;
            }
        };
        match Device::new(&mux, &found).remove_ddi().await {
            Ok(Removed::Cryptex(image)) => {
                say(&format!("Uninstalled the developer disk image ({} {}).", image.identifier, image.version));
                0
            }
            Ok(Removed::Unmounted(path)) => {
                say(&format!("Unmounted the developer disk image ({path})."));
                0
            }
            Err(e) => {
                dev_error("cannot unmount the developer disk image", &e);
                EXIT_START_FAILED
            }
        }
    })
}

/// `scrios doctor`: every check of the preflight, with its fix. Exit status 0 if all passed, 1 if one failed.
/// The viewer's own check for `scrios doctor`: the decoder that the window would decode H.264 with.
#[cfg(feature = "viewer")]
fn viewer_decoder_check() -> doctor::Check {
    use scrios::codec::CodecKind;
    use scrios::viewer::DecoderChoice;
    use scrios::viewer::decode;
    let name = "h264 decoder";
    match decode::create(DecoderChoice::Auto, CodecKind::H264) {
        Ok(decoder) => {
            let fallback = if decode::has_platform_decoder() { ", OpenH264 in software as the fallback" } else { "" };
            doctor::Check {
                name: name.into(),
                status: doctor::Status::Ok,
                detail: format!("{} decodes the picture of the window{fallback}", decoder.name()),
                fix: None,
            }
        }
        Err(why) => doctor::Check {
            name: name.into(),
            status: doctor::Status::Fail,
            detail: why,
            fix: Some("the window needs a decoder: `scrios stream` shows nothing and needs none".into()),
        },
    }
}

pub fn run_doctor(target: &TargetArgs, ddi: &DdiArgs) -> i32 {
    let sources = match ddi.sources() {
        Ok(s) => s,
        Err(why) => {
            error!("{why}");
            return EXIT_START_FAILED;
        }
    };
    run_async(async {
        let opts = DoctorOptions { target: target.target(), ddi: sources };
        let report = Box::pin(doctor::run(&opts)).await;
        // A build with the viewer also checks that it has an H.264 decoder to show the picture with.
        #[cfg(feature = "viewer")]
        let report = {
            let mut report = report;
            report.push(viewer_decoder_check());
            report
        };
        out(&report.render());
        if report.passed() { 0 } else { EXIT_MISSING }
    })
}

/// What `scrios developer-mode` is asked to do.
#[derive(Args, Debug, Clone, Default)]
pub struct DeveloperModeArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    #[arg(
        long,
        help = "Turn Developer Mode on: the device RESTARTS (needs --yes). Works only on a device without a passcode"
    )]
    pub enable: bool,

    #[arg(long, requires = "enable", help = "Confirm --enable: yes, restart the device")]
    pub yes: bool,
}

/// `scrios developer-mode`: says whether Developer Mode is on and, if it is off, reveals the switch in the device's
/// Settings and says the steps. With `--enable --yes` it turns Developer Mode on (AMFI), which restarts the device and
/// works only without a passcode; `--enable` without `--yes` only says what it would do. Exit status 0 if Developer
/// Mode is on (or is being turned on), 1 if it is off, 2 if it could not be done.
pub fn run_developer_mode(args: &DeveloperModeArgs) -> i32 {
    if args.enable && !args.yes {
        error!(
            "--enable turns Developer Mode on and RESTARTS the device; it works only on a device without a passcode. Add --yes to do it"
        );
        return EXIT_START_FAILED;
    }
    run_async(async {
        let (mux, found) = match open_device(&args.target.target()).await {
            Ok(opened) => opened,
            Err(stop) => {
                error!("{}", stop.message());
                return EXIT_START_FAILED;
            }
        };
        let device = Device::new(&mux, &found);
        let on = match device.developer_mode().await {
            Ok(on) => on,
            Err(e) => {
                dev_error("cannot read Developer Mode", &e);
                return EXIT_START_FAILED;
            }
        };
        if on {
            say("Developer Mode is on.");
            return 0;
        }
        if args.enable {
            return match device.enable_developer_mode().await {
                Ok(()) => {
                    say(
                        "Developer Mode is being turned on: the device restarts now. When it is back, tap Turn On and enter its passcode if it asks.",
                    );
                    0
                }
                Err(e) => {
                    dev_error("cannot turn Developer Mode on", &e);
                    EXIT_START_FAILED
                }
            };
        }
        match device.reveal_developer_mode().await {
            Ok(()) => say("The Developer Mode switch is now in the device's Settings (Privacy & Security)."),
            Err(e) => dev_error("cannot reveal the Developer Mode switch", &e),
        }
        let off = Blocker::DeveloperModeOff { udid: found.udid, after_restart: false };
        say(&format!("Developer Mode is off. {}", off.wait_line()));
        say(
            "scrios does not turn it on by itself: `scrios developer-mode --enable --yes` does, on a device without a passcode.",
        );
        EXIT_MISSING
    })
}

/// What the stream's preflight ended with.
pub enum PreflightEnd {
    /// The device is ready: the options of the stream to go to it.
    Ready(Box<scrios::preflight::ReadyDevice>),
    /// The user gave up (Ctrl-C, or SIGTERM on unix) while it waited.
    Interrupted,
    /// It could not be done (the message).
    Failed(String),
}

/// Runs the preflight for a stream, printing its steps (one plain line each) on standard error.
pub fn run_preflight(opts: &PreflightOptions) -> PreflightEnd {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return PreflightEnd::Failed(format!("cannot build the async runtime: {e}")),
    };
    rt.block_on(async {
        let mut print = |step: &PreflightStep| {
            if let Some(line) = step.line() {
                say(&line);
            }
        };
        tokio::select! {
            ready = Box::pin(ensure_ready(opts, &mut print)) => match ready {
                Ok(ready) => PreflightEnd::Ready(Box::new(ready)),
                Err(why) => PreflightEnd::Failed(why.message()),
            },
            () = interrupted() => PreflightEnd::Interrupted,
        }
    })
}

/// Completes when a stop signal arrives (SIGINT or SIGTERM; Ctrl-C or Ctrl-Break on Windows).
async fn interrupted() {
    match crate::signals::StopSignals::install() {
        Ok(mut signals) => signals.recv().await,
        Err(_) => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mounted_image_is_one_line_of_its_plain_values() {
        let mut d = plist::Dictionary::new();
        d.insert("ImageType".into(), "Personalized".into());
        d.insert("MountPath".into(), "/System/Developer".into());
        d.insert("Signature".into(), plist::Value::Data(vec![0; 48]));
        d.insert("Nested".into(), plist::Value::Dictionary(plist::Dictionary::new()));
        assert_eq!(
            entry_line(&plist::Value::Dictionary(d)),
            "ImageType=Personalized MountPath=/System/Developer Signature=<48 bytes>"
        );
        assert!(entry_line(&plist::Value::Boolean(true)).contains("true"));
    }

    #[test]
    fn the_ddi_options_become_sources() {
        assert!(DdiArgs::default().sources().is_ok());
        let dir = DdiArgs { dir: Some("/x/iOS_DDI".into()), ..DdiArgs::default() }.sources().unwrap();
        assert_eq!(dir.dir.as_deref(), Some(std::path::Path::new("/x/iOS_DDI")));
        let half = DdiArgs { download_url: Some("https://example.com/x".into()), ..DdiArgs::default() };
        assert!(half.sources().is_err());
        let both = DdiArgs {
            download_url: Some("https://example.com/x".into()),
            sha256: Some("0".repeat(64)),
            ..DdiArgs::default()
        };
        assert!(both.sources().unwrap().download.is_some());
    }
}
