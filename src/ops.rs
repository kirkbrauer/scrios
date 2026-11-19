//! What scrios asks of a device besides streaming: its lockdown values, Developer Mode, and the developer disk image
//! (mounted images, installing, removing). Every connection goes through usbmuxd and uses the pair record usbmuxd
//! holds ([`Mux::provider`](crate::mux::Mux::provider)).
//!
//! The developer disk image goes onto a device by one of two [`Route`]s: as a cryptex through `cryptexd` (iOS 17.4
//! and later, when the device's RSD handshake offers `com.apple.security.cryptexd.remote`: what Xcode does), or
//! through the image mounter (iOS 17.0 to 17.3.1, and the fallback where `cryptexd` is not offered).

use std::fmt;
use std::future::Future;
use std::time::Duration;

use idevice::amfi::AmfiClient;
use idevice::core_device::DisplayServiceClient;
use idevice::core_device_proxy::CoreDeviceProxy;
use idevice::cryptexd::{Cryptex1Assets, CryptexdClient, InstalledCryptex, install_ddi, installed_ddi};
use idevice::lockdown::LockdownClient;
use idevice::mobile_image_mounter::ImageMounter;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::rsd::RsdHandshake;
use idevice::tcp::handle::AdapterHandle;
use idevice::usbmuxd::UsbmuxdDevice;
use idevice::{IdeviceError, IdeviceService, ReadWrite, RsdService};

use tracing::debug;

use crate::ddi::{DdiFolder, Variant};
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
/// The first version of iOS (major, minor) whose developer disk image is a cryptex that `cryptexd` installs; 17.0 to
/// 17.3.1 take the image mounter's personalized image.
const FIRST_CRYPTEXD_IOS: (u32, u32) = (17, 4);

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

/// Why the image mounter is the way to install the image on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MounterWhy {
    /// The device runs iOS 17.3.1 or older: `cryptexd` takes the image from iOS 17.4.
    OlderIos,
    /// The device is new enough, and its RSD handshake does not list `cryptexd`.
    NotOffered,
    /// The device is new enough, and the `CoreDevice` tunnel that would tell could not be opened.
    NoTunnel,
}

/// How the developer disk image gets onto a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// As a cryptex through `cryptexd`, over the `CoreDevice` tunnel (iOS 17.4 and later, where RSD offers the service).
    Cryptexd,
    /// Through the image mounter, as the personalized image (the fallback).
    ImageMounter(MounterWhy),
}

impl Route {
    /// The form of the image this route takes.
    #[must_use]
    pub fn variant(self) -> Variant {
        match self {
            Route::Cryptexd => Variant::Cryptex,
            Route::ImageMounter(_) => Variant::Personalized,
        }
    }

    /// The route in a few words, and why, for a report.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Route::Cryptexd => "cryptexd (the image is installed as a cryptex, as Xcode does)",
            Route::ImageMounter(MounterWhy::OlderIos) => {
                "the image mounter (iOS 17.3.1 and older have no cryptexd install)"
            }
            Route::ImageMounter(MounterWhy::NotOffered) => {
                "the image mounter (the device does not offer cryptexd over RSD)"
            }
            Route::ImageMounter(MounterWhy::NoTunnel) => {
                "the image mounter (the CoreDevice tunnel that tells whether cryptexd is offered could not be opened)"
            }
        }
    }
}

/// The route for a device that runs `version` (`27.2`), given whether its RSD handshake lists `cryptexd` (`None`:
/// not known, the tunnel could not be opened): `cryptexd` from iOS 17.4 when it is offered, else the image mounter.
#[must_use]
pub fn route_for(version: &str, cryptexd_offered: Option<bool>) -> Route {
    if major_minor(version) < FIRST_CRYPTEXD_IOS {
        return Route::ImageMounter(MounterWhy::OlderIos);
    }
    match cryptexd_offered {
        Some(true) => Route::Cryptexd,
        Some(false) => Route::ImageMounter(MounterWhy::NotOffered),
        None => Route::ImageMounter(MounterWhy::NoTunnel),
    }
}

/// What the device's RSD handshake lists that the developer disk image is about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RsdOffer {
    /// The display service (`com.apple.coredevice.displayservice`): what the stream needs.
    pub display_service: bool,
    /// `cryptexd` (`com.apple.security.cryptexd.remote`): what installs the image as a cryptex.
    pub cryptexd: bool,
}

impl RsdOffer {
    /// What the RSD services `names` offer.
    #[must_use]
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> RsdOffer {
        let display = <DisplayServiceClient<Box<dyn ReadWrite>> as RsdService>::rsd_service_name();
        let cryptexd = <CryptexdClient<Box<dyn ReadWrite>> as RsdService>::rsd_service_name();
        let mut offer = RsdOffer::default();
        for name in names {
            offer.display_service |= name == display;
            offer.cryptexd |= name == cryptexd;
        }
        offer
    }

    fn of(handshake: &RsdHandshake) -> RsdOffer {
        RsdOffer::from_names(handshake.services.keys().map(String::as_str))
    }
}

/// A cryptex that is installed on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledImage {
    /// The cryptex's identifier (`com.apple.MobileAsset.DDI` for the developer disk image).
    pub identifier: String,
    /// Its version (`27.2.52.0`).
    pub version: String,
}

impl From<InstalledCryptex> for InstalledImage {
    fn from(c: InstalledCryptex) -> InstalledImage {
        InstalledImage { identifier: c.identifier, version: c.version }
    }
}

/// What taking the developer disk image off a device did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removed {
    /// The cryptex was uninstalled through `cryptexd`.
    Cryptex(InstalledImage),
    /// The image was unmounted through the image mounter, from this path.
    Unmounted(String),
}

/// What removing the image does: uninstall the cryptex, or unmount through the image mounter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovePlan {
    /// Uninstall this cryptex through `cryptexd`.
    UninstallCryptex(InstalledImage),
    /// Unmount through the image mounter.
    Unmount,
}

/// How to take the image off a device with this `route`, given the cryptex that `cryptexd` lists as installed (`None`:
/// none, or not asked): a cryptex is uninstalled, anything else is unmounted through the image mounter.
#[must_use]
pub fn remove_plan(route: Route, installed: Option<InstalledImage>) -> RemovePlan {
    match (route, installed) {
        (Route::Cryptexd, Some(image)) => RemovePlan::UninstallCryptex(image),
        _ => RemovePlan::Unmount,
    }
}

/// An open `CoreDevice` tunnel, and the RSD handshake over it.
struct Tunnel {
    adapter: AdapterHandle,
    handshake: RsdHandshake,
}

impl Tunnel {
    async fn close(mut self) {
        let _ = self.adapter.close().await;
    }
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

    /// Whether Developer Mode is on: the image mounter's `QueryDeveloperModeStatus`, and if the device does not answer
    /// that, AMFI's own status query.
    ///
    /// # Errors
    ///
    /// Why the device could not be asked.
    pub async fn developer_mode(&self) -> Result<bool, DevError> {
        limited("reading Developer Mode", QUERY_TIMEOUT, async {
            let from_mounter = async {
                let mut mounter = self.mounter().await?;
                mounter
                    .query_developer_mode_status()
                    .await
                    .map_err(|e| DevError::from_idevice("cannot read the Developer Mode status", &e))
            }
            .await;
            match from_mounter {
                Err(first @ DevError::Failed(_)) => {
                    // Another way to the same answer, before giving up.
                    let from_amfi = async {
                        let mut amfi = AmfiClient::connect(&self.provider)
                            .await
                            .map_err(|e| DevError::from_idevice("cannot connect to the device's AMFI service", &e))?;
                        amfi.get_developer_mode_status()
                            .await
                            .map_err(|e| DevError::from_idevice("cannot read the Developer Mode status", &e))
                    }
                    .await;
                    from_amfi.map_err(|_| first)
                }
                other => other,
            }
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
    /// The mounted images (`CopyDevices`) are looked at first: on recent iOS the personalized image can arrive as a
    /// `MobileAsset` cryptex, which `LookupImage` does not report although it is mounted (see
    /// [`entries_show_developer_image`]). `LookupImage` is the second opinion.
    ///
    /// # Errors
    ///
    /// Why the device could not be asked.
    pub async fn image_mounted(&self) -> Result<bool, DevError> {
        match self.mounted_images().await {
            Ok(entries) if entries_show_developer_image(&entries) => return Ok(true),
            Ok(_) => {}
            Err(e) => debug!("cannot list the mounted images, looking the image up instead: {e}"),
        }
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

    /// Mounts the personalized developer disk image of `folder` through the image mounter (iOS 17 and later; see
    /// [`Device::route`] for when `cryptexd` takes the image instead), calling `progress` with the
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
        // Never mount over an image that is there: the mounted images say so even when LookupImage does not.
        if self.image_mounted().await? {
            return Ok(Mounted::Already);
        }
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
                // The ticket comes from Apple's signing server: if that cannot be reached, say so.
                DevError::Failed(why) => Err(DevError::Failed(with_signing_hint(why).await)),
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

    /// Opens the `CoreDevice` tunnel and its RSD handshake.
    async fn tunnel(&self) -> Result<Tunnel, DevError> {
        let tunnel_error = |what: &str, e: &dyn fmt::Debug| DevError::Failed(format!("{what}: {e:?}"));
        let proxy = Box::pin(CoreDeviceProxy::connect(&self.provider))
            .await
            .map_err(|e| DevError::from_idevice("cannot open the CoreDevice tunnel", &e))?;
        let rsd_port = proxy.tunnel_info().server_rsd_port;
        let adapter =
            proxy.create_software_tunnel().map_err(|e| tunnel_error("cannot create the tunnel adapter", &e))?;
        let mut adapter = adapter.to_async_handle();
        let handshake = async {
            let stream = adapter
                .connect(rsd_port)
                .await
                .map_err(|e| tunnel_error(&format!("cannot connect to the RSD port {rsd_port}"), &e))?;
            Box::pin(RsdHandshake::new(stream))
                .await
                .map_err(|e| DevError::from_idevice("the RSD handshake failed", &e))
        }
        .await;
        match handshake {
            Ok(handshake) => Ok(Tunnel { adapter, handshake }),
            Err(e) => {
                let _ = adapter.close().await;
                Err(e)
            }
        }
    }

    /// What the device's RSD handshake offers: the `CoreDevice` tunnel is opened, and the services its handshake lists
    /// are looked through.
    ///
    /// # Errors
    ///
    /// Why the tunnel could not be opened.
    pub async fn rsd_offer(&self) -> Result<RsdOffer, DevError> {
        limited("opening the CoreDevice tunnel", TUNNEL_TIMEOUT, async {
            let tunnel = self.tunnel().await?;
            let offer = RsdOffer::of(&tunnel.handshake);
            tunnel.close().await;
            Ok(offer)
        })
        .await
    }

    /// Whether the device offers the display service. This is what the stream needs, and the one answer to "is
    /// everything in place" (it needs iOS 27 or later, Developer Mode on and the developer disk image installed).
    ///
    /// # Errors
    ///
    /// Why the tunnel could not be opened.
    pub async fn display_service_offered(&self) -> Result<bool, DevError> {
        Ok(self.rsd_offer().await?.display_service)
    }

    /// How the developer disk image gets onto this device ([`route_for`]): the iOS version decides up to 17.3.1, and from
    /// 17.4 the device's RSD handshake does (the tunnel is opened for it). A tunnel that cannot be opened for another
    /// reason than trust or a locked device means the image mounter, which does not need it.
    ///
    /// # Errors
    ///
    /// Why the device could not be asked: the iOS version, trust, a locked device.
    pub async fn route(&self) -> Result<Route, DevError> {
        let version = self.product_version().await?;
        if major_minor(&version) < FIRST_CRYPTEXD_IOS {
            return Ok(route_for(&version, None));
        }
        let offered = match self.rsd_offer().await {
            Ok(offer) => Some(offer.cryptexd),
            Err(e @ (DevError::NotTrusted | DevError::Locked)) => return Err(e),
            Err(e) => {
                debug!("cannot ask the device whether it offers cryptexd: {e}");
                None
            }
        };
        Ok(route_for(&version, offered))
    }

    /// Runs `work` on a thread of its own, with a runtime of its own, and waits for its answer.
    ///
    /// idevice's `cryptexd` calls (`installed_ddi`, `install_ddi`, and the client they reach through
    /// `RsdService::connect_rsd`) are futures that rustc cannot prove `Send`, and the preflight's must be (it may run on
    /// any thread of a program). They are made and run here, on the thread, with a device of their own; only the
    /// answer crosses over. The stream does the same with its own thread.
    async fn off_thread<T, F, Fut>(&self, what: &'static str, work: F) -> Result<T, DevError>
    where
        T: Send + 'static,
        F: FnOnce(Device) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, DevError>> + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let device = self.clone();
        let spawned = std::thread::Builder::new().name("scrios-cryptexd".into()).spawn(move || {
            let outcome = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(work(device)),
                Err(e) => Err(DevError::Failed(format!("{what}: cannot build the async runtime: {e}"))),
            };
            let _ = tx.send(outcome);
        });
        if let Err(e) = spawned {
            return Err(DevError::Failed(format!("{what}: cannot start a thread: {e}")));
        }
        rx.await.unwrap_or_else(|_| Err(DevError::Failed(format!("{what}: the thread ended without an answer"))))
    }

    /// The developer disk image cryptex that `cryptexd` lists as installed (`copy-installed`), if there is one: `None`
    /// also where the device does not offer `cryptexd`.
    ///
    /// # Errors
    ///
    /// Why the tunnel could not be opened, or `cryptexd` could not be asked.
    pub async fn installed_ddi(&self) -> Result<Option<InstalledImage>, DevError> {
        self.off_thread("listing the installed cryptexes", |device| async move {
            Box::pin(limited("listing the installed cryptexes", TUNNEL_TIMEOUT, async {
                let mut tunnel = device.tunnel().await?;
                let listed = if RsdOffer::of(&tunnel.handshake).cryptexd {
                    Box::pin(installed_ddi(&mut tunnel.adapter, &mut tunnel.handshake))
                        .await
                        .map_err(|e| DevError::from_idevice("cannot list the installed cryptexes", &e))
                } else {
                    Ok(None)
                };
                tunnel.close().await;
                Ok(listed?.map(InstalledImage::from))
            }))
            .await
        })
        .await
    }

    /// Installs the developer disk image as a cryptex through `cryptexd` (idevice's `install_ddi`): the device's chip
    /// identifiers and the cryptex nonce are read, Apple's signing server (TSS) signs a ticket for them (which needs
    /// internet), and the image, trust cache, ticket, info and volume hash are pushed. Never over a cryptex that is
    /// installed: then nothing is done ([`Mounted::Already`]).
    ///
    /// # Errors
    ///
    /// Why it could not be done: the device does not offer `cryptexd`, the signing server cannot be reached (the
    /// message says so), or the device refused.
    pub async fn install_ddi_cryptex(&self, assets: &Cryptex1Assets) -> Result<(Mounted, InstalledImage), DevError> {
        let assets = assets.clone();
        self.off_thread("installing the developer image", |device| async move {
            Box::pin(limited("installing the developer image", MOUNT_TIMEOUT, async {
                let mut tunnel = device.tunnel().await?;
                let outcome = Box::pin(install_in(&mut tunnel, &assets)).await;
                tunnel.close().await;
                outcome
            }))
            .await
        })
        .await
    }

    /// Takes the developer disk image off the device: on a device whose route is `cryptexd` and that has the cryptex
    /// installed, the cryptex is uninstalled through `cryptexd`; else the image is unmounted through the image mounter
    /// (`/System/Developer` on iOS 17 and later, `/Developer` before). See [`remove_plan`].
    ///
    /// # Errors
    ///
    /// Why the device could not be asked, or refused.
    pub async fn remove_ddi(&self) -> Result<Removed, DevError> {
        let route = self.route().await?;
        let installed = if route == Route::Cryptexd { self.installed_ddi().await? } else { None };
        match remove_plan(route, installed) {
            RemovePlan::UninstallCryptex(image) => {
                let identifier = image.identifier.clone();
                self.off_thread("uninstalling the developer image", |device| async move {
                    Box::pin(limited("uninstalling the developer image", QUERY_TIMEOUT, async {
                        let mut tunnel = device.tunnel().await?;
                        let outcome = Box::pin(uninstall_in(&mut tunnel, &identifier)).await;
                        tunnel.close().await;
                        outcome
                    }))
                    .await
                })
                .await?;
                Ok(Removed::Cryptex(image))
            }
            RemovePlan::Unmount => self.unmount_ddi_path().await.map(Removed::Unmounted),
        }
    }

    /// Unmounts the developer disk image through the image mounter (`/System/Developer` on iOS 17 and later,
    /// `/Developer` before). [`Device::remove_ddi`] uninstalls the cryptex instead where the image is one.
    ///
    /// # Errors
    ///
    /// Why the device could not be asked, or refused.
    pub async fn unmount_ddi(&self) -> Result<(), DevError> {
        self.unmount_ddi_path().await.map(|_| ())
    }

    async fn unmount_ddi_path(&self) -> Result<String, DevError> {
        let version = self.product_version().await?;
        let path = if major(&version) < FIRST_PERSONALIZED_IOS { "/Developer" } else { "/System/Developer" };
        limited("unmounting the developer image", QUERY_TIMEOUT, async {
            let mut mounter = self.mounter().await?;
            mounter
                .unmount_image(path)
                .await
                .map_err(|e| DevError::from_idevice(&format!("cannot unmount {path}"), &e))?;
            Ok(path.to_string())
        })
        .await
    }
}

/// The uninstall itself, on an open tunnel.
async fn uninstall_in(tunnel: &mut Tunnel, identifier: &str) -> Result<(), DevError> {
    let client: CryptexdClient<Box<dyn ReadWrite>> = Box::pin(tunnel.handshake.connect(&mut tunnel.adapter))
        .await
        .map_err(|e| DevError::from_idevice("cannot connect to the device's cryptexd", &e))?;
    Box::pin(client.uninstall(identifier, None))
        .await
        .map_err(|e| DevError::from_idevice(&format!("cannot uninstall {identifier}"), &e))
}

/// The install itself, on an open tunnel: refused where `cryptexd` is not offered, skipped where the cryptex is there.
async fn install_in(tunnel: &mut Tunnel, assets: &Cryptex1Assets) -> Result<(Mounted, InstalledImage), DevError> {
    if !RsdOffer::of(&tunnel.handshake).cryptexd {
        return Err(DevError::Failed(
            "the device does not offer cryptexd over RSD (com.apple.security.cryptexd.remote): the cryptex cannot be installed".into(),
        ));
    }
    // Never install over an image that is there.
    match Box::pin(installed_ddi(&mut tunnel.adapter, &mut tunnel.handshake)).await {
        Ok(Some(installed)) => return Ok((Mounted::Already, installed.into())),
        Ok(None) => {}
        Err(e) => return Err(DevError::from_idevice("cannot list the installed cryptexes", &e)),
    }
    match Box::pin(install_ddi(&mut tunnel.adapter, &mut tunnel.handshake, assets)).await {
        Ok(installed) => Ok((Mounted::Now, installed.into())),
        Err(e) => match DevError::from_idevice("installing the developer image failed", &e) {
            DevError::Failed(why) => Err(DevError::Failed(with_signing_hint(why).await)),
            other => Err(other),
        },
    }
}

/// `why`, and if Apple's signing server cannot be reached, that too: the ticket comes from there, and it is the one cause
/// of a failed mount or install that the user can act on.
async fn with_signing_hint(why: String) -> String {
    match tss_reachable().await {
        Err(unreachable) => format!(
            "{why}; {unreachable}. The first install after each restart of the device needs internet (scrios connects to {TSS_HOST} directly: HTTP proxy settings are not used)"
        ),
        Ok(()) => why,
    }
}

/// Whether the image mounter's `CopyDevices` entries show a developer disk image: an entry whose `DiskImageType` is
/// `Personalized` (iOS 17 and later) or `Developer` (before), whose `BackingImage` is the `MobileAsset` DDI cryptex
/// (`.../com.apple.MobileAsset.DDI/...`), or whose `MountPath` is `/System/Developer` (or `/Developer`).
#[must_use]
pub fn entries_show_developer_image(entries: &[plist::Value]) -> bool {
    entries.iter().filter_map(plist::Value::as_dictionary).any(|entry| {
        let text = |key: &str| entry.get(key).and_then(plist::Value::as_string);
        matches!(text("DiskImageType"), Some("Personalized" | "Developer"))
            || text("BackingImage").is_some_and(|path| path.contains("com.apple.MobileAsset.DDI"))
            || text("MountPath").is_some_and(|path| {
                path == "/System/Developer" || path == "/Developer" || path.starts_with("/System/Developer/")
            })
    })
}

/// The major number of an iOS version (`27.2` is 27); 0 for what is not a version.
#[must_use]
pub fn major(version: &str) -> u32 {
    version.split('.').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// The major and minor numbers of an iOS version (`17.4.1` is `(17, 4)`, `27` is `(27, 0)`); `(0, 0)` for what is not a
/// version.
#[must_use]
pub fn major_minor(version: &str) -> (u32, u32) {
    let mut numbers = version.split('.').map(|part| {
        let digits: String = part.trim().chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    let major = numbers.next().flatten().unwrap_or(0);
    let minor = if major == 0 { 0 } else { numbers.next().flatten().unwrap_or(0) };
    (major, minor)
}

/// The image type the device lists its developer image under, for an iOS of this major version.
fn image_type(major: u32) -> &'static str {
    if major < FIRST_PERSONALIZED_IOS { "Developer" } else { "Personalized" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pairs: &[(&str, &str)]) -> plist::Value {
        plist::Value::Dictionary(pairs.iter().map(|(k, v)| ((*k).to_string(), plist::Value::from(*v))).collect())
    }

    #[test]
    fn a_personalized_image_from_the_mobileasset_cryptex_counts_as_mounted() {
        // The CopyDevices entry an iPhone on iOS 27 reported while its display service was offered (LookupImage
        // said "not found").
        let cryptex = entry(&[
            (
                "BackingImage",
                "/private/var/db/com.apple.security.cryptexd/codex.system/cryptex/com.apple.MobileAsset.DDI/gdmg",
            ),
            ("DeviceNode", "/dev/disk5s1"),
            ("DeviceType", "DiskImage"),
            ("DiskImageType", "Personalized"),
        ]);
        assert!(entries_show_developer_image(std::slice::from_ref(&cryptex)));
        // Each sign on its own is enough.
        assert!(entries_show_developer_image(&[entry(&[
            ("DeviceType", "DiskImage"),
            ("DiskImageType", "Personalized")
        ])]));
        assert!(entries_show_developer_image(&[entry(&[(
            "BackingImage",
            "/private/var/db/com.apple.security.cryptexd/codex.system/cryptex/com.apple.MobileAsset.DDI/gdmg",
        )])]));
        assert!(entries_show_developer_image(&[entry(&[("MountPath", "/System/Developer")])]));
        assert!(entries_show_developer_image(&[entry(&[("DiskImageType", "Developer")])]));
        // Other disk images, and nothing at all, do not.
        assert!(!entries_show_developer_image(&[]));
        assert!(!entries_show_developer_image(&[entry(&[
            ("BackingImage", "/private/var/mobile/Media/other.dmg"),
            ("DeviceType", "DiskImage"),
            ("DiskImageType", "Cryptex"),
        ])]));
        assert!(!entries_show_developer_image(&[plist::Value::from("not a dictionary")]));
    }

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
    fn versions_have_a_major_and_a_minor() {
        assert_eq!(major_minor("17.4.1"), (17, 4));
        assert_eq!(major_minor("27.2"), (27, 2));
        assert_eq!(major_minor("27"), (27, 0));
        assert_eq!(major_minor("17.3.1"), (17, 3));
        assert_eq!(major_minor("17.4 beta 2"), (17, 4));
        assert_eq!(major_minor(""), (0, 0));
        assert_eq!(major_minor("beta.4"), (0, 0));
    }

    #[test]
    fn the_route_is_cryptexd_from_ios_17_4_when_it_is_offered_and_the_image_mounter_otherwise() {
        use MounterWhy::{NoTunnel, NotOffered, OlderIos};
        // 17.0 to 17.3.1, and before: the image mounter, whatever RSD says (it is not asked).
        for old in ["16.7.10", "17.0", "17.2", "17.3.1"] {
            assert_eq!(route_for(old, None), Route::ImageMounter(OlderIos), "{old}");
            assert_eq!(route_for(old, Some(true)), Route::ImageMounter(OlderIos), "{old}");
        }
        // From 17.4: cryptexd when RSD lists it, else the image mounter, and the reason is told.
        for new in ["17.4", "17.4.1", "18.0", "26.5", "27.2"] {
            assert_eq!(route_for(new, Some(true)), Route::Cryptexd, "{new}");
            assert_eq!(route_for(new, Some(false)), Route::ImageMounter(NotOffered), "{new}");
            assert_eq!(route_for(new, None), Route::ImageMounter(NoTunnel), "{new}");
        }
        // What is not a version is not new enough.
        assert_eq!(route_for("", Some(true)), Route::ImageMounter(OlderIos));
        // The form of the image each route takes, and words for a report.
        assert_eq!(Route::Cryptexd.variant(), Variant::Cryptex);
        assert_eq!(Route::ImageMounter(NotOffered).variant(), Variant::Personalized);
        assert!(Route::Cryptexd.describe().contains("cryptexd"));
        for why in [OlderIos, NotOffered, NoTunnel] {
            assert!(Route::ImageMounter(why).describe().starts_with("the image mounter"), "{why:?}");
        }
    }

    #[test]
    fn the_rsd_offer_is_read_from_the_names_of_the_services_the_handshake_lists() {
        // The names idevice asks RSD for: the display stream and the cryptex daemon.
        let both = RsdOffer::from_names(["com.apple.coredevice.displayservice", "com.apple.security.cryptexd.remote"]);
        assert_eq!(both, RsdOffer { display_service: true, cryptexd: true });
        assert_eq!(
            RsdOffer::from_names(["com.apple.security.cryptexd.remote", "com.apple.other"]),
            RsdOffer { display_service: false, cryptexd: true }
        );
        assert_eq!(RsdOffer::from_names(["com.apple.other"]), RsdOffer::default());
        assert_eq!(RsdOffer::from_names([]), RsdOffer::default());
        // The two names are the ones the clients of idevice use: a bump of idevice that renames one is seen here.
        let display = <DisplayServiceClient<Box<dyn ReadWrite>> as RsdService>::rsd_service_name();
        let cryptexd = <CryptexdClient<Box<dyn ReadWrite>> as RsdService>::rsd_service_name();
        assert_eq!(
            (&*display, &*cryptexd),
            ("com.apple.coredevice.displayservice", "com.apple.security.cryptexd.remote")
        );
    }

    #[test]
    fn removing_the_image_uninstalls_a_cryptex_and_unmounts_anything_else() {
        let image = InstalledImage { identifier: "com.apple.MobileAsset.DDI".into(), version: "27.2.52.0".into() };
        assert_eq!(remove_plan(Route::Cryptexd, Some(image.clone())), RemovePlan::UninstallCryptex(image.clone()));
        // No cryptex installed (the image was mounted by the image mounter): unmount it.
        assert_eq!(remove_plan(Route::Cryptexd, None), RemovePlan::Unmount);
        // A device that takes the image mounter's image is never asked for a cryptex.
        assert_eq!(remove_plan(Route::ImageMounter(MounterWhy::OlderIos), Some(image)), RemovePlan::Unmount);
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
