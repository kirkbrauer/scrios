//! Windows' own decoder, Media Foundation: **not written yet**.
//!
//! On Windows the platform decoder is asked for, says it is not there yet, and [`create`](super::create) falls back to the
//! software decoder (`OpenH264`), which is what a Windows build decodes with today. The type is here so that the day the
//! decoder is written (an `MFT` H.264 decoder with D3D11 output, behind the same [`Decoder`] trait) it
//! changes this file and nothing else. On other hosts nothing refers to it.

#![cfg_attr(not(windows), allow(dead_code))]

use super::{DecodeError, Decoder, Picture};
use crate::stream::AccessUnit;

/// The Media Foundation decoder (a stub).
#[derive(Debug)]
pub struct MediaFoundation;

impl MediaFoundation {
    /// Always an error for now: "not yet".
    ///
    /// # Errors
    ///
    /// Always.
    pub fn new() -> Result<MediaFoundation, String> {
        Err("the Media Foundation decoder is not written yet".to_owned())
    }
}

impl Decoder for MediaFoundation {
    fn name(&self) -> &'static str {
        "Media Foundation"
    }

    fn decode(&mut self, _unit: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
        Err(DecodeError::Fatal("the Media Foundation decoder is not written yet".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_says_it_is_not_there_yet() {
        assert!(MediaFoundation::new().unwrap_err().contains("not written yet"));
    }
}
