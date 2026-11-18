//! A device for the preflight's tests: it answers from a script, so that no device is touched.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::ddi::Found;
use crate::ops::{DevError, Mounted};
use crate::preflight::Ops;

/// What the fake device answers, one answer for each question and the last one for every question after.
#[derive(Default)]
pub(crate) struct Script {
    pub(crate) developer_mode: VecDeque<Result<bool, DevError>>,
    pub(crate) mounted: VecDeque<Result<bool, DevError>>,
    pub(crate) mount: VecDeque<Result<Mounted, DevError>>,
    pub(crate) display_service: VecDeque<Result<bool, DevError>>,
    pub(crate) reveals: u32,
    pub(crate) mounts: u32,
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
        self.with(|s| next(&mut s.developer_mode, Ok(true)))
    }

    async fn reveal_developer_mode(&self) -> Result<(), DevError> {
        self.with(|s| s.reveals += 1);
        Ok(())
    }

    async fn image_mounted(&self) -> Result<bool, DevError> {
        self.with(|s| next(&mut s.mounted, Ok(true)))
    }

    async fn mount(&self, _: &Found, progress: &mut (dyn FnMut(u8) + Send)) -> Result<Mounted, DevError> {
        for percent in [0, 4, 12, 50, 99, 100] {
            progress(percent);
        }
        self.with(|s| {
            s.mounts += 1;
            next(&mut s.mount, Ok(Mounted::Now))
        })
    }

    async fn display_service_offered(&self) -> Result<bool, DevError> {
        self.with(|s| next(&mut s.display_service, Ok(true)))
    }
}
