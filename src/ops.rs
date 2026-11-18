//! What scrios asks of a device besides streaming: its lockdown values, Developer Mode, and the developer disk image
//! (mounted images, mounting, unmounting). Every connection goes through usbmuxd and uses the pair record usbmuxd
//! holds ([`Mux::provider`](crate::mux::Mux::provider)).

use std::fmt;
use std::future::Future;
use std::time::Duration;

use idevice::amfi::AmfiClient;
use idevice::core_device::DisplayServiceClient;
use idevice::core_device_proxy::CoreDeviceProxy;
use idevice::lockdown::LockdownClient;
use idevice::mobile_image_mounter::ImageMounter;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::rsd::RsdHandshake;
use idevice::usbmuxd::UsbmuxdDevice;
use idevice::{IdeviceError, IdeviceService, ReadWrite, RsdService};

use crate::ddi::DdiFolder;
use crate::mux::Mux;

/// The label the device sees for our connections.
const LABEL: &str = "scrios";
/// How long one query of the device may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long opening the tunnel and its handshake may take.
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a mount may take: the image goes over USB, and Apple's signing server answers in between.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(300);
/// Apple's signing server, which mounting the personalized image asks for a ticket (over plain HTTP, port 80).
const TSS_HOST: &str = "gs.apple.com";

/// The first version of iOS that takes the personalized developer disk image.
const FIRST_PERSONALIZED_IOS: u32 = 17;

/// Why something that asked the device failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevError {
    /// The device does not accept the pair record usbmuxd holds: it was reset, or never trusted this host.
    NotTrusted,
    /// The device is locked.
    Locked,
    /// Anything else, in words.
    Failed(String),
}

impl fmt::Display for DevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DevError::NotTrusted => f.write_str(
                "the device does not trust this computer (its pair record is missing or stale): unlock it and tap Trust",
            ),
            DevError::Locked => f.write_str("the device is locked: unlock it"),
            DevError::Failed(why) => f.write_str(why),
        }
    }
}

/// An error and what caused it, in one line.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl DevError {
    /// `what` failed with `e`.
    #[must_use]
    pub fn from_idevice(what: &str, e: &IdeviceError) -> DevError {
        match e {
            IdeviceError::InvalidHostID => DevError::NotTrusted,
            IdeviceError::DeviceLocked => DevError::Locked,
            IdeviceError::DeveloperModeNotEnabled => DevError::Failed(format!("{what}: Developer Mode is off")),
            other => DevError::Failed(format!("{what}: {}", chain(other))),
        }
    }
}

/// Waits for `fut` at most `limit`.
async fn limited<T>(
    what: &str,
    limit: Duration,
    fut: impl Future<Output = Result<T, DevError>>,
) -> Result<T, DevError> {
    tokio::time::timeout(limit, fut).await.unwrap_or_else(|_| {
        Err(DevError::Failed(format!("{what}: the device did not answer in {} s (is it unlocked?)", limit.as_secs())))
    })
}

/// Whether Apple's signing server can be reached (the connection is made, nothing is sent).
///
/// # Errors
///
/// A message that says so if it cannot: the first mount after each reboot needs it.
pub async fn tss_reachable() -> Result<(), String> {
    let connect = tokio::net::TcpStream::connect((TSS_HOST, 80));
    match tokio::time::timeout(Duration::from_secs(8), connect).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("cannot reach Apple's signing server {TSS_HOST}: {e}")),
        Err(_) => Err(format!("cannot reach Apple's signing server {TSS_HOST}: no answer in 8 s")),
    }
}

/// What the mount found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mounted {
    /// The image was mounted by this call.
    Now,
    /// It was mounted already: nothing was done.
    Already,
}

/// The way to one device: its lockdown, Developer Mode and mounted images.
#[derive(Debug, Clone)]
pub struct Device {
    provider: UsbmuxdProvider,
}

impl Device {
    /// The device `device` of the usbmuxd `mux`.
    #[must_use]
    pub fn new(mux: &Mux, device: &UsbmuxdDevice) -> Device {
        Device { provider: mux.provider(device, LABEL) }
    }

    /// The device's UDID.
    #[must_use]
    pub fn udid(&self) -> &str {
        &self.provider.udid
    }

    /// A lockdown session (the pair record comes from usbmuxd).
    async fn lockdown(&self) -> Result<LockdownClient, DevError> {
        let mut lockdown = LockdownClient::connect(&self.provider)
            .await
            .map_err(|e| DevError::from_idevice("cannot connect to the device's lockdown", &e))?;
        let pairing = self
            .provider
            .get_pairing_file()
            .await
            .map_err(|e| DevError::from_idevice("cannot read the pair record from usbmuxd", &e))?;
        lockdown
            .start_session(&pairing)
            .await
            .map_err(|e| DevError::from_idevice("cannot start a lockdown session", &e))?;
        Ok(lockdown)
    }

    async fn mounter(&self) -> Result<ImageMounter, DevError> {
        ImageMounter::connect(&self.provider)
            .await
            .map_err(|e| DevError::from_idevice("cannot connect to the device's image mounter", &e))
    }

    /// The device's iOS version (`27.2`).
    ///
    /// # Errors
    ///
    /// Why the device could not be asked.
    pub async fn product_version(&self) -> Result<String, DevError> {
        limited("reading the iOS version", QUERY_TIMEOUT, async {
            let mut lockdown = self.lockdown().await?;
            let value = lockdown
                .get_value(Some("ProductVersion"), None)
                .await
                .map_err(|e| DevError::from_idevice("cannot read the iOS version", &e))?;
            value
                .as_string()
                .map(str::to_string)
                .ok_or_else(|| DevError::Failed("the device's iOS version is not a string".into()))
        })
        .await
    }

    /// Whether Developer Mode is on (the image mounter's `QueryDeveloperModeStatus`).
    ///
    /// # Errors
    ///
    /// Why the device could not be asked.
    pub async fn developer_mode(&self) -> Result<bool, DevError> {
        limited("reading Developer Mode", QUERY_TIMEOUT, async {
            let mut mounter = self.mounter().await?;
            mounter
                .query_developer_mode_status()
                .await
                .map_err(|e| DevError::from_idevice("cannot read the Developer Mode status", &e))
        })
        .await
    }

    /// The images the device has mounted (the image mounter's `CopyDevices`), as the device describes them.
    ///
    /// # Errors
    ///
    /// Why the device could not be asked.
    pub async fn mounted_images(&self) -> Result<Vec<plist::Value>, DevError> {
        limited("listing the mounted images", QUERY_TIMEOUT, async {
            let mut mounter = self.mounter().await?;
            mounter.copy_devices().await.map_err(|e| DevError::from_idevice("cannot list the mounted images", &e))
        })
        .await
    }

    /// Whether the developer disk image is mounted: the personalized image on iOS 17 and later, the older one before.
    ///
    /// # Errors
    ///
    /// Why the device could not be asked.
    pub async fn image_mounted(&self) -> Result<bool, DevError> {
        let version = self.product_version().await?;
        limited("looking up the developer image", QUERY_TIMEOUT, async {
            let mut mounter = self.mounter().await?;
            match mounter.lookup_image(image_type(major(&version))).await {
                Ok(_) => Ok(true),
                Err(IdeviceError::NotFound) => Ok(false),
                Err(e) => Err(DevError::from_idevice("cannot look up the developer image", &e)),
            }
        })
        .await
    }

    /// Mounts the personalized developer disk image of `folder` (iOS 17 and later), calling `progress` with the
    /// percent of the upload that is done. The device's ticket comes from Apple's signing server (TSS), which needs
    /// internet, unless the device already holds one.
    ///
    /// # Errors
    ///
    /// Why it could not be done: the device's iOS is older than 17, the folder has nothing for this device, the
    /// signing server cannot be reached (the message says so), or the device refused.
    pub async fn mount_ddi(
        &self,
        folder: &DdiFolder,
        progress: &mut (dyn FnMut(u8) + Send),
    ) -> Result<Mounted, DevError> {
        let version = self.product_version().await?;
        if major(&version) < FIRST_PERSONALIZED_IOS {
            return Err(DevError::Failed(format!(
                "this device runs iOS {version}, which takes the older developer image: scrios mounts only the personalized one of iOS {FIRST_PERSONALIZED_IOS} and later (the display service needs iOS 27 anyway); mount it with Xcode"
            )));
        }
        Box::pin(limited("mounting the developer image", MOUNT_TIMEOUT, self.mount_personalized(folder, progress)))
            .await
    }

    async fn mount_personalized(
        &self,
        folder: &DdiFolder,
        progress: &mut (dyn FnMut(u8) + Send),
    ) -> Result<Mounted, DevError> {
        let mut lockdown = self.lockdown().await?;
        let chip = lockdown
            .get_value(Some("UniqueChipID"), None)
            .await
            .map_err(|e| DevError::from_idevice("cannot read the device's chip ID", &e))?
            .as_unsigned_integer()
            .ok_or_else(|| DevError::Failed("the device's chip ID is not a number".into()))?;
        let mut mounter = self.mounter().await?;
        match mounter.lookup_image(image_type(FIRST_PERSONALIZED_IOS)).await {
            Ok(_) => return Ok(Mounted::Already),
            Err(IdeviceError::NotFound) => {}
            Err(e) => return Err(DevError::from_idevice("cannot look up the developer image", &e)),
        }
        let identifiers = mounter
            .query_personalization_identifiers(None)
            .await
            .map_err(|e| DevError::from_idevice("cannot read the device's personalization identifiers", &e))?;
        let number = |key: &str| {
            identifiers
                .get(key)
                .and_then(plist::Value::as_unsigned_integer)
                .ok_or_else(|| DevError::Failed(format!("the device's personalization identifiers have no {key}")))
        };
        let files = folder.files_for(number("BoardId")?, number("ChipID")?).map_err(DevError::Failed)?;
        let read = |path: &std::path::Path| {
            std::fs::read(path).map_err(|e| DevError::Failed(format!("cannot read {}: {e}", path.display())))
        };
        let (image, trust_cache) = (read(&files.image)?, read(&files.trust_cache)?);
        let (manifest, _) = folder.read_manifest().map_err(DevError::Failed)?;
        progress(0);
        // The upload calls back through a shared reference.
        let progress = std::sync::Mutex::new(progress);
        let outcome = Box::pin(mounter.mount_personalized_with_callback(
            &self.provider,
            image,
            trust_cache,
            &manifest,
            None,
            chip,
            |((sent, total), ())| {
                let (sent, total) = (u64::try_from(sent).unwrap_or(u64::MAX), u64::try_from(total).unwrap_or(1));
                let percent = sent.saturating_mul(100) / total.max(1);
                if let Ok(mut report) = progress.lock() {
                    (*report)(u8::try_from(percent.min(100)).unwrap_or(100));
                }
                std::future::ready(())
            },
            (),
        ))
        .await;
        match outcome {
            Ok(()) => Ok(Mounted::Now),
            Err(e) => match DevError::from_idevice("mounting the developer image failed", &e) {
                // The ticket comes from Apple's signing server: if that cannot be reached, say so, because it is the
                // one cause the user can act on.
                DevError::Failed(why) => Err(DevError::Failed(match tss_reachable().await {
                    Err(unreachable) => format!(
                        "{why}; {unreachable}. The first mount after each restart of the device needs internet (scrios connects to {TSS_HOST} directly: HTTP proxy settings are not used)"
                    ),
                    Ok(()) => why,
                })),
                other => Err(other),
            },
        }
    }

    /// Shows the Developer Mode switch in the device's Settings (Privacy & Security > Developer Mode), which iOS hides
    /// until a developer tool asks for it. It does not turn Developer Mode on (AMFI's `reveal` action).
    ///
    /// # Errors
    ///
    /// Why the device could not be asked, or refused.
    pub async fn reveal_developer_mode(&self) -> Result<(), DevError> {
        limited("revealing the Developer Mode switch", QUERY_TIMEOUT, async {
            let mut amfi = AmfiClient::connect(&self.provider)
                .await
                .map_err(|e| DevError::from_idevice("cannot connect to the device's AMFI service", &e))?;
            amfi.reveal_developer_mode_option_in_ui()
                .await
                .map_err(|e| DevError::from_idevice("cannot reveal the Developer Mode switch", &e))
        })
        .await
    }

    /// Turns Developer Mode on (AMFI's `enable` action): the device restarts, and works only on a device without a
    /// passcode. scrios only does this when it is asked to by name (`scrios developer-mode --enable --yes`).
    ///
    /// # Errors
    ///
    /// Why the device could not be asked, or refused.
    pub async fn enable_developer_mode(&self) -> Result<(), DevError> {
        limited("turning Developer Mode on", QUERY_TIMEOUT, async {
            let mut amfi = AmfiClient::connect(&self.provider)
                .await
                .map_err(|e| DevError::from_idevice("cannot connect to the device's AMFI service", &e))?;
            amfi.enable_developer_mode().await.map_err(|e| DevError::from_idevice("cannot turn Developer Mode on", &e))
        })
        .await
    }

    /// Whether the device offers the display service: the `CoreDevice` tunnel is opened, and the services its RSD
    /// handshake lists are looked through. This is what the stream needs, and the one answer to "is everything in
    /// place" (it needs iOS 27 or later, Developer Mode on and the developer disk image mounted).
    ///
    /// # Errors
    ///
    /// Why the tunnel could not be opened.
    pub async fn display_service_offered(&self) -> Result<bool, DevError> {
        limited("opening the CoreDevice tunnel", TUNNEL_TIMEOUT, async {
            let tunnel_error = |what: &str, e: &dyn fmt::Debug| DevError::Failed(format!("{what}: {e:?}"));
            let proxy = Box::pin(CoreDeviceProxy::connect(&self.provider))
                .await
                .map_err(|e| DevError::from_idevice("cannot open the CoreDevice tunnel", &e))?;
            let rsd_port = proxy.tunnel_info().server_rsd_port;
            let adapter =
                proxy.create_software_tunnel().map_err(|e| tunnel_error("cannot create the tunnel adapter", &e))?;
            let mut adapter = adapter.to_async_handle();
            let offered = async {
                let stream = adapter
                    .connect(rsd_port)
                    .await
                    .map_err(|e| tunnel_error(&format!("cannot connect to the RSD port {rsd_port}"), &e))?;
                let handshake = Box::pin(RsdHandshake::new(stream))
                    .await
                    .map_err(|e| DevError::from_idevice("the RSD handshake failed", &e))?;
                let name = <DisplayServiceClient<Box<dyn ReadWrite>> as RsdService>::rsd_service_name();
                Ok(handshake.services.contains_key(name.as_ref()))
            }
            .await;
            let _ = adapter.close().await;
            offered
        })
        .await
    }

    /// Unmounts the developer disk image (`/System/Developer` on iOS 17 and later, `/Developer` before).
    ///
    /// # Errors
    ///
    /// Why the device could not be asked, or refused.
    pub async fn unmount_ddi(&self) -> Result<(), DevError> {
        let version = self.product_version().await?;
        let path = if major(&version) < FIRST_PERSONALIZED_IOS { "/Developer" } else { "/System/Developer" };
        limited("unmounting the developer image", QUERY_TIMEOUT, async {
            let mut mounter = self.mounter().await?;
            mounter.unmount_image(path).await.map_err(|e| DevError::from_idevice(&format!("cannot unmount {path}"), &e))
        })
        .await
    }
}

/// The major number of an iOS version (`27.2` is 27); 0 for what is not a version.
#[must_use]
pub fn major(version: &str) -> u32 {
    version.split('.').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// The image type the device lists its developer image under, for an iOS of this major version.
fn image_type(major: u32) -> &'static str {
    if major < FIRST_PERSONALIZED_IOS { "Developer" } else { "Personalized" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_image_types() {
        assert_eq!(major("27.2"), 27);
        assert_eq!(major("16.7.10"), 16);
        assert_eq!(major(""), 0);
        assert_eq!(major("beta"), 0);
        assert_eq!(image_type(16), "Developer");
        assert_eq!(image_type(17), "Personalized");
        assert_eq!(image_type(major("27.0.1")), "Personalized");
    }

    #[test]
    fn the_device_errors_that_have_a_cure_are_told_apart() {
        assert_eq!(DevError::from_idevice("x", &IdeviceError::InvalidHostID), DevError::NotTrusted);
        assert_eq!(DevError::from_idevice("x", &IdeviceError::DeviceLocked), DevError::Locked);
        let other = DevError::from_idevice("reading", &IdeviceError::UnexpectedResponse("odd".into()));
        assert!(
            matches!(&other, DevError::Failed(why) if why.starts_with("reading: ") && why.contains("odd")),
            "{other}"
        );
        let io = DevError::from_idevice("connecting", &IdeviceError::Socket(std::io::Error::other("reset")));
        assert!(matches!(&io, DevError::Failed(why) if why.contains("reset")), "{io}");
        assert!(DevError::NotTrusted.to_string().contains("Trust"));
        assert!(DevError::Locked.to_string().contains("unlock"));
    }

    #[tokio::test]
    async fn a_query_that_never_answers_is_an_error_not_a_hang() {
        let never = std::future::pending::<Result<(), DevError>>();
        let why = limited("asking", Duration::from_millis(20), never).await.unwrap_err();
        assert!(matches!(&why, DevError::Failed(m) if m.contains("did not answer") && m.contains("unlocked")), "{why}");
    }
}
