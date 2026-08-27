//! Low-level helpers for building ISO BMFF boxes in memory.
//!
//! `moov` and its children are small, so they are assembled as byte vectors and
//! written once at finalize time. Only `mdat` is streamed.

/// Growable big-endian byte buffer with box-building helpers.
#[derive(Default, Debug, Clone)]
pub struct Buf(pub Vec<u8>);

impl Buf {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u24(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes()[1..]);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }

    pub fn zeros(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }

    pub fn fourcc(&mut self, f: &[u8; 4]) -> &mut Self {
        self.0.extend_from_slice(f);
        self
    }

    /// Appends a plain box `kind` whose payload is produced by `f`.
    pub fn atom(&mut self, kind: &[u8; 4], f: impl FnOnce(&mut Buf)) -> &mut Self {
        let start = self.0.len();
        self.u32(0).fourcc(kind);
        f(self);
        let size = (self.0.len() - start) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_be_bytes());
        self
    }

    /// Appends a full box (version + 24-bit flags) whose payload is produced by `f`.
    pub fn full_atom(
        &mut self,
        kind: &[u8; 4],
        version: u8,
        flags: u32,
        f: impl FnOnce(&mut Buf),
    ) -> &mut Self {
        self.atom(kind, |b| {
            b.u8(version).u24(flags);
            f(b);
        })
    }

    /// Appends an MPEG-4 descriptor (tag + 4-byte expandable length + payload),
    /// as used inside `esds`.
    pub fn descriptor(&mut self, tag: u8, f: impl FnOnce(&mut Buf)) -> &mut Self {
        let mut payload = Buf::new();
        f(&mut payload);
        let len = payload.0.len() as u32;
        self.u8(tag);
        // 4-byte expandable size, same shape ffmpeg writes.
        self.u8(0x80 | ((len >> 21) & 0x7F) as u8);
        self.u8(0x80 | ((len >> 14) & 0x7F) as u8);
        self.u8(0x80 | ((len >> 7) & 0x7F) as u8);
        self.u8((len & 0x7F) as u8);
        self.bytes(&payload.0)
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

/// Seconds between 1904-01-01 (MP4 epoch) and 1970-01-01 (Unix epoch).
pub const MP4_EPOCH_OFFSET: u64 = 2_082_844_800;

/// Current time in the MP4 epoch (seconds since 1904).
pub fn mp4_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + MP4_EPOCH_OFFSET
}

/// Identity transformation matrix used by `mvhd`/`tkhd`.
pub fn unity_matrix(b: &mut Buf) {
    b.u32(0x0001_0000).u32(0).u32(0);
    b.u32(0).u32(0x0001_0000).u32(0);
    b.u32(0).u32(0).u32(0x4000_0000);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_sizes_are_patched() {
        let mut b = Buf::new();
        b.atom(b"free", |b| {
            b.u32(0xDEAD_BEEF);
        });
        assert_eq!(b.0, [0, 0, 0, 12, b'f', b'r', b'e', b'e', 0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn descriptor_uses_expandable_length() {
        let mut b = Buf::new();
        b.descriptor(0x06, |b| {
            b.u8(0x02);
        });
        assert_eq!(b.0, [0x06, 0x80, 0x80, 0x80, 0x01, 0x02]);
    }
}
