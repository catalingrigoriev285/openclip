//! H.265 / HEVC bitstream helpers: NAL types and the SPS fields needed for `hvcC`.

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;
pub const NAL_SEI_PREFIX: u8 = 39;
pub const NAL_SEI_SUFFIX: u8 = 40;

/// `nal_unit_type` of a start-code-free HEVC NAL.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0)
}

pub fn is_parameter_set(t: u8) -> bool {
    matches!(t, NAL_VPS | NAL_SPS | NAL_PPS)
}

/// Intra random access point (IDR / CRA / BLA) NAL types.
pub fn is_irap(t: u8) -> bool {
    (16..=23).contains(&t)
}

/// Fields of the SPS `profile_tier_level` (and chroma/bit depth) copied into `hvcC`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcProfileInfo {
    pub profile_space: u8,
    pub tier: u8,
    pub profile_idc: u8,
    pub compat_flags: u32,
    pub constraint_flags: [u8; 6],
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
}

impl Default for HevcProfileInfo {
    /// Main profile, level 5.1, 4:2:0 8-bit — a safe guess when parsing fails.
    fn default() -> Self {
        Self {
            profile_space: 0,
            tier: 0,
            profile_idc: 1,
            compat_flags: 0x6000_0000,
            constraint_flags: [0x90, 0, 0, 0, 0, 0],
            level_idc: 153,
            chroma_format_idc: 1,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
        }
    }
}

/// Removes emulation-prevention bytes (`00 00 03` → `00 00`).
fn to_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = if zeros == 0 { 0 } else { self.bits(zeros)? };
        Some((1u32 << zeros) - 1 + rest)
    }
}

/// Parses the profile / tier / level and format fields from an HEVC SPS NAL
/// (start-code-free, including the 2-byte NAL header).
pub fn parse_sps_profile(sps: &[u8]) -> Option<HevcProfileInfo> {
    if sps.len() < 3 || nal_type(sps) != NAL_SPS {
        return None;
    }
    let rbsp = to_rbsp(&sps[2..]);
    let mut r = BitReader::new(&rbsp);
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as usize;
    r.bit()?; // sps_temporal_id_nesting_flag

    // profile_tier_level(1, max_sub_layers_minus1)
    let profile_space = r.bits(2)? as u8;
    let tier = r.bit()? as u8;
    let profile_idc = r.bits(5)? as u8;
    let compat_flags = r.bits(32)?;
    let mut constraint_flags = [0u8; 6];
    for c in &mut constraint_flags {
        *c = r.bits(8)? as u8;
    }
    let level_idc = r.bits(8)? as u8;
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 {
        profile_present[i] = r.bit()? == 1;
        level_present[i] = r.bit()? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.bits(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }

    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()? as u8;
    if chroma_format_idc == 3 {
        r.bit()?; // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.bit()? == 1 {
        // conformance_window offsets
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let bit_depth_luma = r.ue()? as u8 + 8;
    let bit_depth_chroma = r.ue()? as u8 + 8;
    Some(HevcProfileInfo {
        profile_space,
        tier,
        profile_idc,
        compat_flags,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS from an x265 Main-profile 4:2:0 8-bit stream (level 3.1).
    const SPS: [u8; 41] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5d,
        0xa0, 0x02, 0x80, 0x80, 0x2d, 0x16, 0x59, 0x59, 0xa4, 0x93, 0x2b, 0x9a, 0x02, 0x00, 0x00, 0x03, 0x00, 0x02,
        0x00, 0x00, 0x03, 0x00, 0x3c,
    ];

    #[test]
    fn parses_main_profile_sps() {
        let p = parse_sps_profile(&SPS).expect("parses");
        assert_eq!(p.profile_space, 0);
        assert_eq!(p.tier, 0);
        assert_eq!(p.profile_idc, 1);
        assert_eq!(p.compat_flags, 0x6000_0000);
        assert_eq!(p.constraint_flags, [0x90, 0, 0, 0, 0, 0]);
        assert_eq!(p.level_idc, 93);
        assert_eq!(p.chroma_format_idc, 1);
        assert_eq!(p.bit_depth_luma, 8);
        assert_eq!(p.bit_depth_chroma, 8);
    }

    #[test]
    fn nal_types_and_rbsp() {
        assert_eq!(nal_type(&SPS), NAL_SPS);
        assert_eq!(nal_type(&[0x40, 0x01]), NAL_VPS);
        assert_eq!(nal_type(&[0x44, 0x01]), NAL_PPS);
        assert!(is_irap(19));
        assert!(!is_irap(1));
        assert_eq!(to_rbsp(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
        assert!(parse_sps_profile(&[0x40, 0x01, 0x0c]).is_none());
    }
}
