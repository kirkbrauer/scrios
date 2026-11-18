//! A command-line front for the H.264 SPS rewrite, to check it against other decoders and parsers (see README.md).
//!
//! usage:
//!   rewrite_sps patch <in.h264> <out.h264>   rewrite every SPS of an Annex-B stream (add the VUI bitstream restriction)
//!   rewrite_sps craft <0|1|2> <out.h264>     write a crafted stream: an SPS with no VUI (0), a VUI without (1) or
//!                                            with (2) a bitstream restriction, a PPS and a stub IDR slice
use scrios_fuzz::{avc, craft};
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["craft", kind, out] => {
            let kind: u32 = kind.parse().expect("the kind is 0, 1 or 2");
            let mut stream = vec![0, 0, 0, 1];
            stream.extend(craft::craft(kind));
            stream.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]);
            stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xFF]);
            std::fs::File::create(out).expect("create the output").write_all(&stream).expect("write the output");
        }
        ["patch", input, out] => {
            let data = std::fs::read(input).expect("read the input");
            let patched = patch_stream(&data);
            std::fs::write(out, patched).expect("write the output");
        }
        _ => {
            eprintln!("usage: rewrite_sps patch <in.h264> <out.h264> | rewrite_sps craft <0|1|2> <out.h264>");
            std::process::exit(2);
        }
    }
}

/// The NAL units of an Annex-B stream (3- or 4-byte start codes), without trailing zero bytes.
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(data.len(), |next| next - 3);
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            &data[s..e]
        })
        .collect()
}

fn patch_stream(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    for nal in split_annexb(data) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        if nal.first().map(|b| b & 0x1f) == Some(avc::NAL_SPS) {
            match avc::add_bitstream_restriction(nal) {
                Ok(avc::SpsRewrite::Patched(p)) => {
                    eprintln!("SPS {} -> {} bytes", nal.len(), p.len());
                    out.extend_from_slice(&p);
                }
                Ok(avc::SpsRewrite::AlreadyRestricted(r)) => {
                    eprintln!("SPS has a restriction already, kept: {r:?}");
                    out.extend_from_slice(nal);
                }
                Err(e) => {
                    eprintln!("SPS not rewritten: {e}");
                    out.extend_from_slice(nal);
                }
            }
        } else {
            out.extend_from_slice(nal);
        }
    }
    out
}
