//! The codec of the display stream, and what differs between HEVC and H.264 for the engine.

use crate::avc::parse_avc_sps;
use crate::depack::{Assembler, Depacketizer, H264Assembler};
use crate::sps::parse_sps;

/// The codec the stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecKind {
    #[default]
    Hevc,
    H264,
}

/// What an SPS says of the stream, in the terms of the `config` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsDescription {
    /// The WebCodecs / RFC 6381 codec string: `hev1.*` or `avc1.PPCCLL`.
    pub codec: String,
    /// The picture size the SPS codes.
    pub coded: (u32, u32),
    /// The size after the SPS's own cropping (HEVC's conformance window, H.264's frame cropping).
    pub visible: (u32, u32),
}

impl CodecKind {
    /// The RTP payload type the stock offer gives the codec.
    #[must_use]
    pub fn payload_type(self) -> u8 {
        match self {
            CodecKind::Hevc => 100,
            CodecKind::H264 => 123,
        }
    }

    #[must_use]
    pub fn depacketizer(self) -> Box<dyn Depacketizer> {
        match self {
            CodecKind::Hevc => Box::new(Assembler::new()),
            CodecKind::H264 => Box::new(H264Assembler::new()),
        }
    }

    /// Reads an SPS NAL unit (with its NAL header, no start code).
    pub fn describe_sps(self, nal: &[u8]) -> Result<SpsDescription, String> {
        match self {
            CodecKind::Hevc => {
                let sps = parse_sps(nal).map_err(|e| e.to_string())?;
                Ok(SpsDescription {
                    codec: sps.codec_string("hev1"),
                    coded: (sps.width, sps.height),
                    visible: sps.cropped_size().map_err(|e| e.to_string())?,
                })
            }
            CodecKind::H264 => {
                let sps = parse_avc_sps(nal).map_err(|e| e.to_string())?;
                Ok(SpsDescription {
                    codec: sps.codec_string(),
                    coded: (sps.coded_width, sps.coded_height),
                    visible: (sps.width, sps.height),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::avc::testutil::AvcSpsSpec;
    use crate::sps::testutil::SpsSpec;

    #[test]
    fn the_payload_types_are_the_stock_offers() {
        assert_eq!(CodecKind::Hevc.payload_type(), 100);
        assert_eq!(CodecKind::H264.payload_type(), 123);
        assert_eq!(CodecKind::default(), CodecKind::Hevc);
    }

    #[test]
    fn an_hevc_sps_is_described_with_its_conformance_window() {
        let mut spec = SpsSpec::main_l150(1216, 2656);
        spec.conf_window = Some([0, 5, 0, 17]);
        let d = CodecKind::Hevc.describe_sps(&spec.nal()).unwrap();
        assert_eq!(d.codec, "hev1.1.6.L150.B0");
        assert_eq!(d.coded, (1216, 2656));
        assert_eq!(d.visible, (1206, 2622));
        assert!(CodecKind::Hevc.describe_sps(&[0x67, 66, 0, 30]).is_err());
    }

    #[test]
    fn an_h264_sps_is_described_with_its_cropping() {
        let d = CodecKind::H264.describe_sps(&AvcSpsSpec::high_1080p().nal()).unwrap();
        assert_eq!(d.codec, "avc1.640028");
        assert_eq!(d.coded, (1920, 1088));
        assert_eq!(d.visible, (1920, 1080));
        // An H.264 SPS is no HEVC SPS and the other way round.
        assert!(CodecKind::Hevc.describe_sps(&AvcSpsSpec::high_1080p().nal()).is_err());
        assert!(CodecKind::H264.describe_sps(&SpsSpec::main_l150(1216, 2656).nal()).is_err());
    }

    #[test]
    fn each_codec_gets_its_own_depacketizer() {
        // The H.264 one makes a unit of a slice; the HEVC one drops a payload whose temporal id is zero.
        let slice = [0x41u8, 0x9A, 0x01];
        let meta = crate::depack::PacketMeta { seq: 1, timestamp: 1, marker: true };
        let mut out = Vec::new();
        CodecKind::H264.depacketizer().push(meta, &slice, &mut out);
        assert!(matches!(out[0], crate::depack::Output::Unit(_)));
        let mut out = Vec::new();
        CodecKind::Hevc.depacketizer().push(meta, &[0x42, 0x00], &mut out);
        assert!(matches!(out[0], crate::depack::Output::Dropped { .. }));
    }
}
