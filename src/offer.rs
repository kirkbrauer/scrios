//! The display offer.
//!
//! `idevice`'s `build_screen_video_offer` writes the offer the system's screen-sharing client sends,
//! with `allowRTCPFB` off and long-term reference pictures (LTR) on. **That stock offer is the
//! default**: it is what was run on a real device, where a keyframe request (PLI) was answered with
//! an IDR in about 50 ms. Two settings can be changed, each by its own opt-in flag, to compare
//! behaviour on a device: this module takes the stock offer, opens its media blob, and edits exactly
//! those fields of the video settings message (field 5 of the blob):
//!
//! | field | meaning | stock (default) | opt-in |
//! | --- | --- | --- | --- |
//! | 5.1 | SSRC (session id) | ours | ours |
//! | 5.2 | `allowRTCPFB` | 0 | `--rtcp-fb`: 1 |
//! | 5.3 (HEVC, payload type 100), string field 3 | codec feature string | contains `LTR;` | `--no-ltr`: `LTR;` removed |
//! | 5.7 | `ltrpEnabled` | 1 | `--no-ltr`: 0 |
//!
//! Everything else is left byte for byte as the builder wrote it: the blob is parsed into protobuf
//! fields, the fields above are edited, and it is written back (the default flags reproduce the
//! builder's blob exactly, which a test checks).

use std::io::{Read, Write};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use idevice::core_device::{CallInfoBlob, build_screen_video_offer};

const BLOB_KEY: &str = "avcMediaStreamNegotiatorMediaBlob";
/// Field of the blob that holds the video settings message.
const VIDEO_SETTINGS: u32 = 5;
#[cfg(test)]
const F_SSRC: u32 = 1;
const F_ALLOW_RTCP_FB: u32 = 2;
const F_CODEC_BANK: u32 = 3;
const F_LTRP_ENABLED: u32 = 7;
/// Inside a codec bank: its RTP payload type, and its feature string.
const F_BANK_PAYLOAD: u32 = 1;
const F_BANK_FEATURES: u32 = 3;
const HEVC_PAYLOAD_TYPE: u64 = 100;
const LTR_FEATURE: &[u8] = b"LTR;";

/// Which codec banks the offer names. The stock offer names both; the device picks HEVC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// The stock offer: an H.264 bank (payload type 123) and an HEVC bank (payload type 100).
    Hevc,
    /// Only the H.264 bank: the HEVC bank is taken out, so a device that can do H.264 has to answer with it.
    H264,
}

/// What this module decides about the offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfferFlags {
    /// `allowRTCPFB`: the device honours RTCP feedback (PLI/FIR).
    pub allow_rtcp_fb: bool,
    /// Long-term reference pictures (`ltrpEnabled` and the `LTR;` feature).
    pub ltr: bool,
    pub codec: Codec,
}

impl Default for OfferFlags {
    /// The stock offer: `allowRTCPFB` 0, long-term references on.
    fn default() -> Self {
        OfferFlags {
            allow_rtcp_fb: false,
            ltr: true,
            codec: Codec::Hevc,
        }
    }
}

// ---- a minimal protobuf reader/writer ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    Varint(u64),
    Fixed64([u8; 8]),
    Len(Vec<u8>),
    Fixed32([u8; 4]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub num: u32,
    pub wire: Wire,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or("truncated varint")?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("varint too long".into())
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn parse(buf: &[u8]) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let num = u32::try_from(key >> 3).map_err(|_| "field number too large")?;
        let wire = match key & 7 {
            0 => Wire::Varint(read_varint(buf, &mut pos)?),
            1 => {
                let b = buf.get(pos..pos + 8).ok_or("truncated fixed64")?;
                pos += 8;
                Wire::Fixed64(b.try_into().expect("8 bytes"))
            }
            2 => {
                let len = usize::try_from(read_varint(buf, &mut pos)?).map_err(|_| "length")?;
                let end = pos.checked_add(len).ok_or("length")?;
                let b = buf.get(pos..end).ok_or("truncated length-delimited field")?;
                pos = end;
                Wire::Len(b.to_vec())
            }
            5 => {
                let b = buf.get(pos..pos + 4).ok_or("truncated fixed32")?;
                pos += 4;
                Wire::Fixed32(b.try_into().expect("4 bytes"))
            }
            t => return Err(format!("unsupported wire type {t}")),
        };
        fields.push(Field { num, wire });
    }
    Ok(fields)
}

pub fn encode(fields: &[Field]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        let wt = match f.wire {
            Wire::Varint(_) => 0,
            Wire::Fixed64(_) => 1,
            Wire::Len(_) => 2,
            Wire::Fixed32(_) => 5,
        };
        write_varint(&mut out, (u64::from(f.num) << 3) | wt);
        match &f.wire {
            Wire::Varint(v) => write_varint(&mut out, *v),
            Wire::Fixed64(b) => out.extend_from_slice(b),
            Wire::Len(b) => {
                write_varint(&mut out, b.len() as u64);
                out.extend_from_slice(b);
            }
            Wire::Fixed32(b) => out.extend_from_slice(b),
        }
    }
    out
}

fn set_varint(fields: &mut [Field], num: u32, value: u64) -> Result<(), String> {
    match fields.iter_mut().find(|f| f.num == num) {
        Some(Field {
            wire: wire @ Wire::Varint(_),
            ..
        }) => {
            *wire = Wire::Varint(value);
            Ok(())
        }
        _ => Err(format!("the offer has no varint field {num} where expected")),
    }
}

// ---- the offer --------------------------------------------------------------------------------

fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("the offer's media blob is not zlib data: {e}"))?;
    Ok(out)
}

fn deflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(data).map_err(|e| format!("zlib: {e}"))?;
    z.finish().map_err(|e| format!("zlib: {e}"))
}

/// Edits the raw (uncompressed) media blob.
pub fn patch_blob(raw: &[u8], flags: OfferFlags) -> Result<Vec<u8>, String> {
    let mut top = parse(raw)?;
    let video = top
        .iter_mut()
        .find(|f| f.num == VIDEO_SETTINGS && matches!(f.wire, Wire::Len(_)))
        .ok_or("the offer has no video settings (field 5)")?;
    let Wire::Len(video_bytes) = &video.wire else {
        unreachable!("checked above")
    };
    let mut settings = parse(video_bytes)?;

    set_varint(&mut settings, F_ALLOW_RTCP_FB, u64::from(flags.allow_rtcp_fb))?;
    set_varint(&mut settings, F_LTRP_ENABLED, u64::from(flags.ltr))?;
    if !flags.ltr {
        let mut removed = false;
        for bank in settings.iter_mut().filter(|f| f.num == F_CODEC_BANK) {
            removed |= remove_ltr_feature(bank)?;
        }
        if !removed {
            tracing::debug!("the HEVC feature string had no `LTR;` to remove");
        }
    }
    if flags.codec == Codec::H264 {
        let before = settings.len();
        let mut hevc_banks = Vec::new();
        for (i, f) in settings.iter().enumerate() {
            if f.num == F_CODEC_BANK && bank_payload_type(f)? == Some(HEVC_PAYLOAD_TYPE) {
                hevc_banks.push(i);
            }
        }
        for i in hevc_banks.into_iter().rev() {
            settings.remove(i);
        }
        if settings.len() == before {
            return Err("the offer has no HEVC bank to take out".into());
        }
        let left = settings.iter().filter(|f| f.num == F_CODEC_BANK).count();
        if left == 0 {
            return Err("the offer has no H.264 bank to keep".into());
        }
    }
    video.wire = Wire::Len(encode(&settings));
    Ok(encode(&top))
}

/// The RTP payload type of a codec bank (field 1 of the bank message).
fn bank_payload_type(bank: &Field) -> Result<Option<u64>, String> {
    let Wire::Len(bytes) = &bank.wire else {
        return Ok(None);
    };
    Ok(parse(bytes)?
        .iter()
        .find_map(|f| match (f.num, &f.wire) {
            (F_BANK_PAYLOAD, Wire::Varint(v)) => Some(*v),
            _ => None,
        }))
}

/// Drops `LTR;` from the feature string of the HEVC codec bank; other banks are not touched.
fn remove_ltr_feature(bank: &mut Field) -> Result<bool, String> {
    let Wire::Len(bytes) = &bank.wire else {
        return Ok(false);
    };
    let mut inner = parse(bytes)?;
    let is_hevc = inner
        .iter()
        .any(|f| f.num == F_BANK_PAYLOAD && f.wire == Wire::Varint(HEVC_PAYLOAD_TYPE));
    if !is_hevc {
        return Ok(false);
    }
    let mut changed = false;
    for f in inner.iter_mut().filter(|f| f.num == F_BANK_FEATURES) {
        if let Wire::Len(s) = &mut f.wire {
            if let Some(i) = s.windows(LTR_FEATURE.len()).position(|w| w == LTR_FEATURE) {
                s.drain(i..i + LTR_FEATURE.len());
                changed = true;
            }
        }
    }
    if changed {
        bank.wire = Wire::Len(encode(&inner));
    }
    Ok(changed)
}

/// Rewrites the stock offer (a binary plist) with `flags`.
pub fn patch_offer(stock: &[u8], flags: OfferFlags) -> Result<Vec<u8>, String> {
    let mut value: plist::Value =
        plist::from_bytes(stock).map_err(|e| format!("the offer is not a plist: {e}"))?;
    let dict = value.as_dictionary_mut().ok_or("the offer is not a dictionary")?;
    let blob = dict
        .get(BLOB_KEY)
        .and_then(|v| v.as_data())
        .ok_or("the offer has no media blob")?
        .to_vec();
    let patched = deflate(&patch_blob(&inflate(&blob)?, flags)?)?;
    dict.insert(BLOB_KEY.into(), plist::Value::Data(patched));
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &value).map_err(|e| format!("offer plist encode: {e}"))?;
    Ok(out)
}

/// The video offer with our SSRC and `flags`.
pub fn build_video_offer(
    call_id: &str,
    call_info: &CallInfoBlob,
    ssrc: u32,
    flags: OfferFlags,
) -> Result<Vec<u8>, String> {
    let stock = build_screen_video_offer(call_id, call_info, ssrc)
        .map_err(|e| format!("cannot build the video offer: {e:?}"))?;
    patch_offer(&stock, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_info() -> CallInfoBlob {
        CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        }
    }

    const SSRC: u32 = 0xDEAD_BEEF;

    fn plist_blob(offer: &[u8]) -> Vec<u8> {
        let v: plist::Value = plist::from_bytes(offer).unwrap();
        let data = v.as_dictionary().unwrap().get(BLOB_KEY).unwrap().as_data().unwrap().to_vec();
        inflate(&data).unwrap()
    }

    fn stock_blob() -> Vec<u8> {
        let stock = build_screen_video_offer("CALL-ID", &call_info(), SSRC).unwrap();
        plist_blob(&stock)
    }

    fn video_settings(blob: &[u8]) -> Vec<Field> {
        let top = parse(blob).unwrap();
        let Wire::Len(v) = &top.iter().find(|f| f.num == VIDEO_SETTINGS).unwrap().wire else {
            panic!("field 5 is not a message")
        };
        parse(v).unwrap()
    }

    fn varint(fields: &[Field], num: u32) -> u64 {
        match fields.iter().find(|f| f.num == num).unwrap().wire {
            Wire::Varint(v) => v,
            ref w => panic!("field {num} is {w:?}"),
        }
    }

    /// The banks of the video settings: (payload type, fields).
    fn banks(settings: &[Field]) -> Vec<(u64, Vec<Field>)> {
        settings
            .iter()
            .filter(|f| f.num == F_CODEC_BANK)
            .map(|f| {
                let Wire::Len(b) = &f.wire else { panic!("bank") };
                let fields = parse(b).unwrap();
                (varint(&fields, F_BANK_PAYLOAD), fields)
            })
            .collect()
    }

    fn features(bank: &[Field]) -> String {
        let Wire::Len(s) = &bank.iter().find(|f| f.num == F_BANK_FEATURES).unwrap().wire else {
            panic!("features")
        };
        String::from_utf8(s.clone()).unwrap()
    }

    #[test]
    fn protobuf_round_trips_every_wire_type() {
        let fields = vec![
            Field { num: 1, wire: Wire::Varint(300) },
            Field { num: 2, wire: Wire::Len(vec![1, 2, 3]) },
            Field { num: 3, wire: Wire::Fixed32([1, 2, 3, 4]) },
            Field { num: 4, wire: Wire::Fixed64([9; 8]) },
            Field { num: 20, wire: Wire::Varint(u64::MAX) },
        ];
        assert_eq!(parse(&encode(&fields)).unwrap(), fields);
        assert!(parse(&[0x0a, 0x05, 1]).is_err());
        assert!(parse(&[0x80]).is_err());
        assert!(parse(&[0x0b]).is_err()); // wire type 3 (start group)
    }

    /// `allowRTCPFB` 1 and LTR off: the configuration a bare PLI was measured to work in.
    const RTCP_FB_NO_LTR: OfferFlags = OfferFlags {
        allow_rtcp_fb: true,
        ltr: false,
        codec: Codec::Hevc,
    };

    #[test]
    fn the_default_flags_reproduce_the_builders_blob_byte_for_byte() {
        let raw = stock_blob();
        assert_eq!(patch_blob(&raw, OfferFlags::default()).unwrap(), raw);
        // The whole offer too (one stock offer, patched: each build stamps the current time in it).
        let stock = build_screen_video_offer("CALL-ID", &call_info(), SSRC).unwrap();
        let ours = patch_offer(&stock, OfferFlags::default()).unwrap();
        assert_eq!(plist_blob(&stock), plist_blob(&ours));
        // And the stock blob is what the contract says: our SSRC, allowRTCPFB 0, LTR on.
        let vs = video_settings(&raw);
        assert_eq!(varint(&vs, F_SSRC), u64::from(SSRC));
        assert_eq!(varint(&vs, F_ALLOW_RTCP_FB), 0);
        assert_eq!(varint(&vs, F_LTRP_ENABLED), 1);
        let banks = banks(&vs);
        assert_eq!(banks.len(), 2);
        assert!(features(&banks[1].1).contains("LTR;"));
    }

    #[test]
    fn with_both_flags_the_offer_allows_rtcp_feedback_has_ltr_off_and_our_ssrc() {
        let offer = build_video_offer("CALL-ID", &call_info(), SSRC, RTCP_FB_NO_LTR).unwrap();
        let vs = video_settings(&plist_blob(&offer));
        assert_eq!(varint(&vs, F_SSRC), u64::from(SSRC), "the SSRC is ours");
        assert_eq!(varint(&vs, F_ALLOW_RTCP_FB), 1, "allowRTCPFB");
        assert_eq!(varint(&vs, F_LTRP_ENABLED), 0, "ltrpEnabled");
        assert_eq!(varint(&vs, 8), 63);
        assert_eq!(varint(&vs, 12), 1);
        let banks = banks(&vs);
        let hevc = banks.iter().find(|b| b.0 == 100).unwrap();
        assert!(!features(&hevc.1).contains("LTR"), "{}", features(&hevc.1));
        assert!(features(&hevc.1).starts_with("FLS;MS:-1;LF:-1;CABAC;"));
    }

    #[test]
    fn nothing_else_in_the_blob_changes() {
        // One stock offer and its patched copy (every build stamps the current time in the blob).
        let stock_offer = build_screen_video_offer("CALL-ID", &call_info(), SSRC).unwrap();
        let stock = plist_blob(&stock_offer);
        let ours = plist_blob(&patch_offer(&stock_offer, RTCP_FB_NO_LTR).unwrap());

        // Top level: everything but field 5 is identical, in the same order.
        let (st, ot) = (parse(&stock).unwrap(), parse(&ours).unwrap());
        assert_eq!(st.len(), ot.len());
        for (a, b) in st.iter().zip(&ot) {
            assert_eq!(a.num, b.num);
            if a.num != VIDEO_SETTINGS {
                assert_eq!(a, b, "top-level field {}", a.num);
            }
        }
        // Video settings: all but 2 and 7 and the HEVC bank are identical.
        let (sv, ov) = (video_settings(&stock), video_settings(&ours));
        assert_eq!(sv.len(), ov.len());
        for (a, b) in sv.iter().zip(&ov) {
            assert_eq!(a.num, b.num);
            let hevc_bank = |f: &Field| {
                f.num == F_CODEC_BANK
                    && matches!(&f.wire, Wire::Len(x) if parse(x).unwrap().iter().any(|y| y.num == 1 && y.wire == Wire::Varint(100)))
            };
            if a.num != F_ALLOW_RTCP_FB && a.num != F_LTRP_ENABLED && !hevc_bank(a) {
                assert_eq!(a, b, "video settings field {}", a.num);
            }
        }
        // The HEVC bank: only the feature string differs, and only by `LTR;`.
        let (sb, ob) = (banks(&sv), banks(&ov));
        let (s_h, o_h) = (&sb.iter().find(|b| b.0 == 100).unwrap().1, &ob.iter().find(|b| b.0 == 100).unwrap().1);
        assert_eq!(s_h.len(), o_h.len());
        for (a, b) in s_h.iter().zip(o_h) {
            if a.num != F_BANK_FEATURES {
                assert_eq!(a, b);
            }
        }
        assert_eq!(features(s_h).replacen("LTR;", "", 1), features(o_h));
        // The H.264 bank is untouched.
        assert_eq!(sb.iter().find(|b| b.0 == 123), ob.iter().find(|b| b.0 == 123));
    }

    const H264_ONLY: OfferFlags = OfferFlags {
        allow_rtcp_fb: false,
        ltr: true,
        codec: Codec::H264,
    };

    #[test]
    fn the_h264_offer_has_only_the_h264_bank_and_nothing_else_changes() {
        let stock_offer = build_screen_video_offer("CALL-ID", &call_info(), SSRC).unwrap();
        let stock = plist_blob(&stock_offer);
        let ours = plist_blob(&patch_offer(&stock_offer, H264_ONLY).unwrap());

        // Only payload type 123 is left, and it is the bank the stock offer had, byte for byte.
        let (sv, ov) = (video_settings(&stock), video_settings(&ours));
        let (sb, ob) = (banks(&sv), banks(&ov));
        assert_eq!(sb.iter().map(|b| b.0).collect::<Vec<_>>(), vec![123, 100]);
        assert_eq!(ob.iter().map(|b| b.0).collect::<Vec<_>>(), vec![123]);
        assert_eq!(sb[0], ob[0]);
        assert_eq!(features(&ob[0].1), "FLS;SW:1;");
        // Our SSRC, and every other field of the video settings as the builder wrote it.
        assert_eq!(varint(&ov, F_SSRC), u64::from(SSRC));
        let others = |v: &[Field]| v.iter().filter(|f| f.num != F_CODEC_BANK).cloned().collect::<Vec<_>>();
        assert_eq!(others(&sv), others(&ov));
        // Top level: everything but field 5 is identical.
        let (st, ot) = (parse(&stock).unwrap(), parse(&ours).unwrap());
        assert_eq!(st.len(), ot.len());
        for (a, b) in st.iter().zip(&ot) {
            if a.num != VIDEO_SETTINGS {
                assert_eq!(a, b, "top-level field {}", a.num);
            }
        }
        // The plist around the blob is unchanged too.
        let a: plist::Value = plist::from_bytes(&stock_offer).unwrap();
        let b: plist::Value = plist::from_bytes(&patch_offer(&stock_offer, H264_ONLY).unwrap()).unwrap();
        for (k, v) in a.as_dictionary().unwrap() {
            if k != BLOB_KEY {
                assert_eq!(b.as_dictionary().unwrap().get(k), Some(v), "key {k}");
            }
        }
    }

    #[test]
    fn the_codec_combines_with_the_other_flags() {
        let both = OfferFlags { allow_rtcp_fb: true, ltr: false, codec: Codec::H264 };
        let offer = build_video_offer("C", &call_info(), SSRC, both).unwrap();
        let vs = video_settings(&plist_blob(&offer));
        assert_eq!(varint(&vs, F_ALLOW_RTCP_FB), 1);
        assert_eq!(varint(&vs, F_LTRP_ENABLED), 0);
        assert_eq!(banks(&vs).iter().map(|b| b.0).collect::<Vec<_>>(), vec![123]);
        // Taking the HEVC bank out twice is an error (nothing to take out), not a silent no-op.
        let once = patch_blob(&stock_blob(), H264_ONLY).unwrap();
        assert!(patch_blob(&once, H264_ONLY).unwrap_err().contains("no HEVC bank"));
    }

    #[test]
    fn the_flags_are_independent() {
        for (fb, ltr) in [(false, false), (false, true), (true, false), (true, true)] {
            let offer = build_video_offer("C", &call_info(), SSRC, OfferFlags { allow_rtcp_fb: fb, ltr, ..OfferFlags::default() }).unwrap();
            let vs = video_settings(&plist_blob(&offer));
            assert_eq!(varint(&vs, F_ALLOW_RTCP_FB), u64::from(fb));
            assert_eq!(varint(&vs, F_LTRP_ENABLED), u64::from(ltr));
            let banks = banks(&vs);
            let hevc = banks.iter().find(|b| b.0 == 100).unwrap();
            assert_eq!(features(&hevc.1).contains("LTR;"), ltr, "fb={fb} ltr={ltr}");
        }
    }

    #[test]
    fn the_plist_around_the_blob_is_unchanged() {
        let stock = build_screen_video_offer("CALL-ID", &call_info(), SSRC).unwrap();
        let ours = patch_offer(&stock, RTCP_FB_NO_LTR).unwrap();
        let a: plist::Value = plist::from_bytes(&stock).unwrap();
        let b: plist::Value = plist::from_bytes(&ours).unwrap();
        let (a, b) = (a.as_dictionary().unwrap(), b.as_dictionary().unwrap());
        assert_eq!(a.len(), b.len());
        for (k, v) in a {
            if k != BLOB_KEY {
                assert_eq!(b.get(k), Some(v), "key {k}");
            }
        }
        assert_ne!(a.get(BLOB_KEY), b.get(BLOB_KEY));
    }

    #[test]
    fn a_blob_of_another_shape_is_an_error() {
        assert!(patch_blob(&[], OfferFlags::default()).is_err());
        // Field 5 without the varints we edit.
        let odd = encode(&[Field { num: 5, wire: Wire::Len(encode(&[Field { num: 1, wire: Wire::Varint(1) }])) }]);
        assert!(patch_blob(&odd, OfferFlags::default()).unwrap_err().contains("varint field 2"));
        assert!(patch_offer(b"not a plist", OfferFlags::default()).is_err());
    }
}
