//! A device for the preflight's tests: it answers from a script, so that no device is touched.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use idevice::cryptexd::Cryptex1Assets;

use crate::ddi::Found;
use crate::ops::{DevError, InstalledImage, Mounted, MounterWhy, Route};
use crate::preflight::Ops;

/// What the fake device answers, one answer for each question and the last one for every question after.
#[derive(Default)]
pub(crate) struct Script {
    pub(crate) developer_mode: VecDeque<Result<bool, DevError>>,
    pub(crate) mounted: VecDeque<Result<bool, DevError>>,
    pub(crate) mount: VecDeque<Result<Mounted, DevError>>,
    pub(crate) display_service: VecDeque<Result<bool, DevError>>,
    /// How the image gets onto the device (default: the image mounter, as on iOS 17.0 to 17.3.1).
    pub(crate) route: VecDeque<Result<Route, DevError>>,
    /// The developer disk image cryptex that is installed (default: none).
    pub(crate) installed: VecDeque<Result<Option<InstalledImage>, DevError>>,
    /// What an install of a cryptex answers (default: it is installed now).
    pub(crate) install: VecDeque<Result<(Mounted, InstalledImage), DevError>>,
    /// What each install was asked to install, in order.
    pub(crate) installs: Vec<Installed>,
    pub(crate) reveals: u32,
    /// The device operations that were asked for, in order (Developer Mode is left out: it is asked for in every pass).
    pub(crate) calls: Vec<&'static str>,
    /// How many times Developer Mode was asked for.
    pub(crate) developer_mode_asked: u32,
    pub(crate) mounts: u32,
    /// A mount here succeeded.
    pub(crate) mounted_here: bool,
}

/// The payloads an install was given: what scrios passes to idevice's `install_ddi` (as sizes and the identity's keys).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Installed {
    pub(crate) image: Vec<u8>,
    pub(crate) trustcache: Vec<u8>,
    pub(crate) info: Vec<u8>,
    pub(crate) volumehash: Vec<u8>,
    pub(crate) nonce_domain: Option<u64>,
}

/// The cryptex a fake install makes.
pub(crate) fn fake_cryptex() -> InstalledImage {
    InstalledImage { identifier: "com.apple.MobileAsset.DDI".into(), version: "27.2.52.0".into() }
}

#[derive(Clone, Default)]
pub(crate) struct FakeDevice(Arc<Mutex<Script>>);

impl FakeDevice {
    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut Script) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

fn next<T: Clone>(queue: &mut VecDeque<T>, if_empty: T) -> T {
    if queue.len() > 1 { queue.pop_front().unwrap_or(if_empty) } else { queue.front().cloned().unwrap_or(if_empty) }
}

impl Ops for FakeDevice {
    async fn developer_mode(&self) -> Result<bool, DevError> {
        self.with(|s| {
            s.developer_mode_asked += 1;
            next(&mut s.developer_mode, Ok(true))
        })
    }

    async fn reveal_developer_mode(&self) -> Result<(), DevError> {
        self.with(|s| s.reveals += 1);
        Ok(())
    }

    async fn image_mounted(&self) -> Result<bool, DevError> {
        self.with(|s| {
            s.calls.push("image_mounted");
            next(&mut s.mounted, Ok(true))
        })
    }

    async fn mount(&self, _: &Found, progress: &mut (dyn FnMut(u8) + Send)) -> Result<Mounted, DevError> {
        for percent in [0, 4, 12, 50, 99, 100] {
            progress(percent);
        }
        self.with(|s| {
            s.calls.push("mount");
            s.mounts += 1;
            let result = next(&mut s.mount, Ok(Mounted::Now));
            s.mounted_here |= result.is_ok();
            result
        })
    }

    /// Unless scripted, the display service is offered exactly when the image is mounted (or was mounted here).
    async fn display_service_offered(&self) -> Result<bool, DevError> {
        self.with(|s| {
            s.calls.push("display_service_offered");
            if s.display_service.is_empty() {
                let mounted = s.mounted_here || matches!(s.mounted.front(), None | Some(Ok(true)));
                return Ok(mounted);
            }
            next(&mut s.display_service, Ok(true))
        })
    }

    /// Unless scripted, the image mounter's route.
    async fn route(&self) -> Result<Route, DevError> {
        self.with(|s| {
            s.calls.push("route");
            next(&mut s.route, Ok(Route::ImageMounter(MounterWhy::OlderIos)))
        })
    }

    async fn installed_cryptex(&self) -> Result<Option<InstalledImage>, DevError> {
        self.with(|s| {
            s.calls.push("installed_cryptex");
            next(&mut s.installed, Ok(None))
        })
    }

    async fn install_cryptex(&self, assets: &Cryptex1Assets) -> Result<(Mounted, InstalledImage), DevError> {
        self.with(|s| {
            s.calls.push("install_cryptex");
            s.installs.push(Installed {
                image: assets.image.clone(),
                trustcache: assets.trustcache.clone(),
                info: assets.info.clone(),
                volumehash: assets.volumehash.clone(),
                nonce_domain: assets.nonce_domain().ok(),
            });
            let result = next(&mut s.install, Ok((Mounted::Now, fake_cryptex())));
            if let Ok((_, image)) = &result {
                s.installed = VecDeque::from([Ok(Some(image.clone()))]);
                s.mounted_here = true;
            }
            result
        })
    }
}
