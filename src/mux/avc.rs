//! H.264 Annex-B bitstream helpers: start-code splitting and NAL inspection.

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Removes a leading `00 00 00 01` / `00 00 01` start code if present.
pub fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

/// NAL unit type of a start-code-free NAL (`nal_unit_type` field).
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// Splits an Annex-B byte stream into NAL units (without start codes).
pub fn split_annexb(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut end = if n + 1 < starts.len() { starts[n + 1] - 3 } else { stream.len() };
        // Trailing zero bytes before the next start code belong to the start code.
        while end > s && stream[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&stream[s..end]);
        }
    }
    out
}

/// Profile / compatibility / level bytes from an SPS NAL (without start code).
pub fn sps_profile_info(sps: &[u8]) -> Option<(u8, u8, u8)> {
    if sps.len() >= 4 && nal_type(sps) == NAL_SPS {
        Some((sps[1], sps[2], sps[3]))
    } else {
        None
    }
}

/// Appends `nal` to `out` in AVCC form (4-byte big-endian length prefix).
pub fn push_avcc(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0xCC, 0, 0, 0, 1, 0x65, 1, 2];
        let nals = split_annexb(&stream);
        assert_eq!(nals, vec![&[0x67, 0xAA][..], &[0x68, 0xBB, 0xCC][..], &[0x65, 1, 2][..]]);
        assert_eq!(nal_type(nals[0]), NAL_SPS);
        assert_eq!(nal_type(nals[1]), NAL_PPS);
        assert_eq!(nal_type(nals[2]), NAL_IDR);
    }

    #[test]
    fn strips_start_codes() {
        assert_eq!(strip_start_code(&[0, 0, 0, 1, 9]), &[9]);
        assert_eq!(strip_start_code(&[0, 0, 1, 9]), &[9]);
        assert_eq!(strip_start_code(&[9]), &[9]);
    }
}
