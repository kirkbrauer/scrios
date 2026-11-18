//! Annex-B byte streams: splitting them into NAL units, and the length-prefixed form (AVCC) that platform decoders take.

/// The NAL unit type of a NAL unit's first byte.
#[must_use]
pub fn nal_type(first_byte: u8) -> u8 {
    first_byte & 0x1F
}

/// Where each NAL unit of an Annex-B stream starts (the byte after its start code), `00 00 01` or `00 00 00 01`.
fn starts(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some(rest) = data.get(from..) {
        let Some(at) = rest.windows(3).position(|w| w == [0, 0, 1]) else { break };
        from += at + 3;
        starts.push(from);
    }
    starts
}

/// The NAL units of an Annex-B stream, without their start codes and without the zero bytes that may pad the end of one
/// (a NAL unit never ends in a zero byte). Bytes before the first start code are not a NAL unit and are dropped.
#[must_use]
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let starts = starts(data);
    let mut units = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        // The next start code begins three bytes before the next unit does.
        let end = starts.get(i + 1).map_or(data.len(), |next| next.saturating_sub(3));
        let Some(unit) = data.get(start..end) else { continue };
        let len = unit.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
        if let Some(unit) = unit.get(..len)
            && !unit.is_empty()
        {
            units.push(unit);
        }
    }
    units
}

/// The slice NAL units of an access unit as length-prefixed (4 bytes, big-endian) units, which is what a sample for a
/// platform decoder holds. The parameter sets (they are in the decoder's configuration), access unit delimiters, SEI
/// and filler are left out.
#[must_use]
pub fn avcc_slices(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for unit in nal_units(data) {
        let Some(&first) = unit.first() else { continue };
        // 1..=5 are the coded slices (and the data partitions), the only units that carry a picture.
        if !(1..=5).contains(&nal_type(first)) {
            continue;
        }
        let Ok(len) = u32::try_from(unit.len()) else { continue };
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

/// The first SPS and the first PPS of an access unit (the parameter sets a key unit carries), if it has both.
#[must_use]
pub fn parameter_sets(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let units = nal_units(data);
    let find = |kind: u8| units.iter().copied().find(|u| u.first().is_some_and(|&b| nal_type(b) == kind));
    Some((find(7)?, find(8)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5, 6];
        assert_eq!(nal_units(&data), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5, 6]]);
    }

    #[test]
    fn trailing_zero_bytes_and_leading_junk_are_not_part_of_a_unit() {
        let data = [9, 9, 0, 0, 1, 0x41, 7, 0, 0, 0, 0, 0, 1, 0x41, 8, 0, 0];
        assert_eq!(nal_units(&data), vec![&[0x41, 7][..], &[0x41, 8]]);
        assert!(nal_units(&[]).is_empty());
        assert!(nal_units(&[1, 2, 3, 4]).is_empty());
        assert!(nal_units(&[0, 0, 1]).is_empty());
        assert!(nal_units(&[0, 0, 1, 0, 0, 1, 0x65, 1]).len() == 1);
    }

    #[test]
    fn an_access_unit_becomes_length_prefixed_slices_only() {
        let data = [
            0, 0, 0, 1, 0x09, 0xF0, // access unit delimiter
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 0, 1, 0x68, 3, // PPS
            0, 0, 0, 1, 0x06, 4, // SEI
            0, 0, 0, 1, 0x65, 5, 6, 7, // IDR slice
            0, 0, 0, 1, 0x41, 8, // non-IDR slice
        ];
        assert_eq!(avcc_slices(&data), vec![0, 0, 0, 4, 0x65, 5, 6, 7, 0, 0, 0, 2, 0x41, 8]);
        assert!(avcc_slices(&[0, 0, 0, 1, 0x67, 1]).is_empty());
    }

    #[test]
    fn a_key_units_parameter_sets_are_found() {
        let data = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 5];
        let (sps, pps) = parameter_sets(&data).expect("both");
        assert_eq!((sps, pps), (&[0x67, 1, 2][..], &[0x68, 3][..]));
        // A delta unit has none; a unit with only one has not both.
        assert_eq!(parameter_sets(&[0, 0, 0, 1, 0x41, 1]), None);
        assert_eq!(parameter_sets(&[0, 0, 0, 1, 0x67, 1, 2]), None);
    }
}
