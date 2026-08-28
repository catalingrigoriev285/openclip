//! Streaming OpenDML (AVI 2.0) writer.
//!
//! Layout: `RIFF 'AVI '` (≤ 1 GiB: `hdrl` with `avih` / per-stream `strh` +
//! `strf` + `indx` super-index / `odml dmlh`, a `JUNK` pad, `LIST movi`, then
//! the legacy `idx1`) followed by as many `RIFF 'AVIX'` continuation chunks as
//! needed. Every RIFF's `movi` list ends with the standard `ix00` / `ix01`
//! indexes the super-index points at. Video is stored as Annex-B H.264 / HEVC
//! (`H264` / `HEVC` fourcc, parameter sets in-band on keyframes); audio is MP3
//! (`0x0055`), AAC (`0x00FF`) or 16-bit PCM (`0x0001`).
//!
//! AVI has a fixed frame rate: every frame occupies a slot; gaps (dropped
//! frames) are written as empty `00dc` chunks so timing stays correct, and a
//! frame whose slot is already taken is refused.
//!
//! Audio streams are sample based in the AVI sense: PCM ticks are sample
//! frames (`dwSampleSize` = block align); MP3 / AAC use the byte-based CBR
//! convention (`dwSampleSize` = 1, `dwRate` = bytes per second, lengths and
//! super-index durations in bytes), which is what Windows' AVI source, ffmpeg
//! and VLC all agree on.

use std::io::{Seek, SeekFrom, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::mp4::{AudioTrackConfig, VideoCodecConfig, VideoTrackConfig};
use crate::audio::encoder::AudioCodecConfig;

/// Largest RIFF chunk we write (players and the legacy index want ≤ 1 GiB).
const MAX_RIFF: u64 = 0x3F00_0000;
/// Super-index entries reserved per stream (each covers one RIFF chunk).
const SUPER_INDEX_ENTRIES: usize = 512;
/// The `movi` list starts at a multiple of this (leaves room to rewrite `hdrl`).
const HEADER_ALIGN: u64 = 4096;

const AVIF_HASINDEX: u32 = 0x10;
const AVIF_ISINTERLEAVED: u32 = 0x100;
const AVIF_TRUSTCKTYPE: u32 = 0x800;
const AVIIF_KEYFRAME: u32 = 0x10;
const AVI_INDEX_OF_INDEXES: u8 = 0;
const AVI_INDEX_OF_CHUNKS: u8 = 1;

const VIDEO_CHUNK: [u8; 4] = *b"00dc";
const AUDIO_CHUNK: [u8; 4] = *b"01wb";

/// Little-endian byte buffer with RIFF helpers.
#[derive(Default)]
struct Le(Vec<u8>);

impl Le {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i16(&mut self, v: i16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn fourcc(&mut self, f: &[u8; 4]) -> &mut Self {
        self.0.extend_from_slice(f);
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }
    /// `fourcc size payload` (+ pad byte for odd payloads).
    fn chunk(&mut self, kind: &[u8; 4], f: impl FnOnce(&mut Le)) -> &mut Self {
        self.fourcc(kind);
        let at = self.0.len();
        self.u32(0);
        f(self);
        let size = (self.0.len() - at - 4) as u32;
        self.0[at..at + 4].copy_from_slice(&size.to_le_bytes());
        if size % 2 == 1 {
            self.0.push(0);
        }
        self
    }
    /// `LIST size kind payload`.
    fn list(&mut self, kind: &[u8; 4], f: impl FnOnce(&mut Le)) -> &mut Self {
        self.chunk(b"LIST", |b| {
            b.fourcc(kind);
            f(b);
        })
    }
}

#[derive(Clone, Copy)]
struct IndexEntry {
    /// Absolute file offset of the chunk header.
    header_pos: u64,
    size: u32,
    keyframe: bool,
}

#[derive(Clone, Copy)]
struct SuperEntry {
    /// Absolute offset of the `ix##` chunk.
    offset: u64,
    /// Size of the `ix##` chunk including its header.
    size: u32,
    duration: u32,
}

#[derive(Default)]
struct StreamIndex {
    /// Chunks in the current RIFF.
    current: Vec<IndexEntry>,
    /// Stream ticks covered by the current RIFF: frames for video, PCM sample
    /// frames for audio (Windows' AVI source treats the super-index duration
    /// that way even for frame-based codecs like MP3).
    current_duration: u64,
    /// One entry per closed RIFF.
    supers: Vec<SuperEntry>,
    /// Chunks in the first RIFF (for `idx1`).
    legacy: Vec<IndexEntry>,
    max_chunk: u32,
}

pub struct AviWriter<W: Write + Seek> {
    out: W,
    pos: u64,
    video: VideoTrackConfig,
    audio: Option<AudioTrackConfig>,
    fps: u32,
    header_len: u64,
    riff_start: u64,
    /// Absolute position of the `movi` fourcc of the current RIFF.
    movi_pos: u64,
    riff_index: u32,
    max_riff: u64,
    streams: [StreamIndex; 2],
    first_pts: Option<Duration>,
    next_slot: u64,
    /// Video chunks in the first RIFF / in total (including empty ones).
    riff0_frames: u32,
    /// Size field of the first RIFF once closed (re-applied after the header rewrite).
    riff0_size: u32,
    total_frames: u64,
    audio_samples: u64,
    audio_bytes: u64,
    audio_chunks: u64,
    finalized: bool,
}

impl<W: Write + Seek> AviWriter<W> {
    pub fn new(out: W, video: VideoTrackConfig, audio: Option<AudioTrackConfig>) -> Result<Self> {
        if video.width == 0 || video.height == 0 {
            bail!("AVI needs video dimensions");
        }
        let fps = video.fps.round().max(1.0) as u32;
        let mut w = Self {
            out,
            pos: 0,
            video,
            audio,
            fps,
            header_len: 0,
            riff_start: 0,
            movi_pos: 0,
            riff_index: 0,
            max_riff: MAX_RIFF,
            streams: [StreamIndex::default(), StreamIndex::default()],
            first_pts: None,
            next_slot: 0,
            riff0_frames: 0,
            riff0_size: 0,
            total_frames: 0,
            audio_samples: 0,
            audio_bytes: 0,
            audio_chunks: 0,
            finalized: false,
        };
        let header = w.build_header();
        w.header_len = header.len() as u64;
        w.write(&header)?;
        w.open_movi()?;
        Ok(w)
    }

    /// Lowers the RIFF size limit so tests can exercise `AVIX` continuation chunks.
    #[doc(hidden)]
    pub fn set_max_riff_for_tests(&mut self, bytes: u64) {
        self.max_riff = bytes.max(64 * 1024);
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn bytes_written(&self) -> u64 {
        self.pos
    }

    pub fn video_frames(&self) -> u64 {
        self.total_frames
    }

    /// Everything before the first `movi` list: `RIFF AVI ` header, `hdrl`, `JUNK`.
    fn build_header(&self) -> Vec<u8> {
        let v = &self.video;
        let fourcc: [u8; 4] = match v.codec {
            VideoCodecConfig::H264 => *b"H264",
            VideoCodecConfig::Hevc => *b"HEVC",
        };
        let video_bps = self.video_bitrate_hint();
        let audio_bps = self.audio.as_ref().map(|a| a.bitrate_bps).unwrap_or(0);
        let streams = if self.audio.is_some() { 2 } else { 1 };
        let [vs, aus] = &self.streams;
        let mut b = Le::default();
        b.fourcc(b"RIFF").u32(0).fourcc(b"AVI ");
        b.list(b"hdrl", |b| {
            b.chunk(b"avih", |b| {
                b.u32(1_000_000 / self.fps);
                b.u32((video_bps + audio_bps) / 8);
                b.u32(0); // padding granularity
                b.u32(AVIF_HASINDEX | AVIF_ISINTERLEAVED | AVIF_TRUSTCKTYPE);
                b.u32(self.riff0_frames);
                b.u32(0); // initial frames
                b.u32(streams);
                b.u32(vs.max_chunk.max(aus.max_chunk));
                b.u32(v.width).u32(v.height);
                b.zeros(16);
            });
            b.list(b"strl", |b| {
                b.chunk(b"strh", |b| {
                    b.fourcc(b"vids").fourcc(&fourcc);
                    b.u32(0).u16(0).u16(0).u32(0); // flags, priority, language, initial frames
                    b.u32(1).u32(self.fps); // scale, rate
                    b.u32(0); // start
                    b.u32(self.total_frames.min(u32::MAX as u64) as u32);
                    b.u32(vs.max_chunk);
                    b.u32(u32::MAX); // quality: default
                    b.u32(0); // sample size
                    b.i16(0).i16(0).i16(v.width as i16).i16(v.height as i16);
                });
                b.chunk(b"strf", |b| {
                    b.u32(40).u32(v.width).u32(v.height);
                    b.u16(1).u16(24);
                    b.fourcc(&fourcc);
                    b.u32(v.width * v.height * 3);
                    b.u32(0).u32(0).u32(0).u32(0);
                });
                super_index(b, &VIDEO_CHUNK, &vs.supers);
            });
            if let Some(a) = &self.audio {
                b.list(b"strl", |b| {
                    let (scale, rate, sample_size, length) = match &a.codec {
                        AudioCodecConfig::Pcm { .. } => {
                            let align = a.channels as u32 * 2;
                            (align, a.sample_rate * align, align, self.audio_samples)
                        }
                        // Byte-based CBR: one tick per byte.
                        _ => (1, (a.bitrate_bps / 8).max(1), 1, self.audio_bytes),
                    };
                    b.chunk(b"strh", |b| {
                        b.fourcc(b"auds").u32(0);
                        b.u32(0).u16(0).u16(0).u32(0);
                        b.u32(scale).u32(rate);
                        b.u32(0);
                        b.u32(length.min(u32::MAX as u64) as u32);
                        b.u32(aus.max_chunk);
                        b.u32(u32::MAX);
                        b.u32(sample_size);
                        b.i16(0).i16(0).i16(0).i16(0);
                    });
                    b.chunk(b"strf", |b| wave_format(b, a));
                    super_index(b, &AUDIO_CHUNK, &aus.supers);
                });
            }
            b.list(b"odml", |b| {
                b.chunk(b"dmlh", |b| {
                    b.u32(self.total_frames.min(u32::MAX as u64) as u32);
                    b.zeros(244);
                });
            });
        });
        // Pad so the movi list starts aligned; the header keeps a fixed size.
        let used = b.0.len() as u64 + 8; // + JUNK header
        let pad = (HEADER_ALIGN - used % HEADER_ALIGN) % HEADER_ALIGN;
        b.chunk(b"JUNK", |b| {
            b.zeros(pad as usize);
        });
        b.0
    }

    fn video_bitrate_hint(&self) -> u32 {
        // Rough estimate from what was written so far, for `dwMaxBytesPerSec`.
        let frames = self.total_frames.max(1);
        let bytes: u64 = self.streams[0].legacy.iter().map(|e| e.size as u64).sum();
        ((bytes * 8 * self.fps as u64) / frames).min(u32::MAX as u64) as u32
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Opens `RIFF … LIST movi` for the current RIFF (sizes patched when it closes).
    fn open_movi(&mut self) -> Result<()> {
        let mut b = Le::default();
        if self.riff_index == 0 {
            // The first RIFF header was written with the `hdrl` block.
            self.riff_start = 0;
        } else {
            self.riff_start = self.pos;
            b.fourcc(b"RIFF").u32(0).fourcc(b"AVIX");
        }
        b.fourcc(b"LIST").u32(0).fourcc(b"movi");
        let movi_offset = b.0.len() as u64 - 4;
        let at = self.pos;
        self.write(&b.0)?;
        self.movi_pos = at + movi_offset;
        Ok(())
    }

    /// Writes one data chunk covering `ticks` of stream time, rolling over to
    /// a new RIFF first if it would not fit.
    fn write_chunk(&mut self, stream: usize, id: &[u8; 4], data: &[u8], keyframe: bool, ticks: u64) -> Result<()> {
        let needed = 8 + data.len() as u64 + (data.len() % 2) as u64;
        // Keep room for the two standard indexes (+ idx1 in the first RIFF).
        let index_reserve: u64 = self.streams.iter().map(|s| 32 + 8 * s.current.len() as u64).sum::<u64>()
            + if self.riff_index == 0 { 16 * (self.streams[0].legacy.len() + self.streams[1].legacy.len()) as u64 + 8 } else { 0 };
        if self.pos + needed + index_reserve + 1024 > self.riff_start + self.max_riff && !self.streams[stream].current.is_empty() {
            self.close_riff()?;
            self.riff_index += 1;
            self.open_movi()?;
        }
        let header_pos = self.pos;
        let mut hdr = Le::default();
        hdr.fourcc(id).u32(data.len() as u32);
        self.write(&hdr.0)?;
        self.write(data)?;
        if data.len() % 2 == 1 {
            self.write(&[0])?;
        }
        let entry = IndexEntry { header_pos, size: data.len() as u32, keyframe };
        let s = &mut self.streams[stream];
        s.current.push(entry);
        s.current_duration += ticks;
        if self.riff_index == 0 {
            s.legacy.push(entry);
        }
        s.max_chunk = s.max_chunk.max(data.len() as u32);
        Ok(())
    }

    /// Appends one Annex-B access unit. Returns `false` when its frame slot is
    /// already taken (the frame is dropped; timing is preserved).
    pub fn push_video(&mut self, annexb: &[u8], pts: Duration, keyframe: bool) -> Result<bool> {
        let first = *self.first_pts.get_or_insert(pts);
        let slot = (pts.saturating_sub(first).as_secs_f64() * self.fps as f64).round() as u64;
        if slot < self.next_slot {
            return Ok(false);
        }
        while self.next_slot < slot {
            self.write_chunk(0, &VIDEO_CHUNK, &[], false, 1)?;
            self.count_frame();
        }
        self.write_chunk(0, &VIDEO_CHUNK, annexb, keyframe, 1)?;
        self.count_frame();
        Ok(true)
    }

    fn count_frame(&mut self) {
        self.next_slot += 1;
        self.total_frames += 1;
        if self.riff_index == 0 {
            self.riff0_frames += 1;
        }
    }

    /// Appends one audio frame (`samples` PCM frames per channel).
    pub fn push_audio(&mut self, data: &[u8], samples: u32) -> Result<()> {
        let Some(a) = &self.audio else { bail!("no audio track") };
        let ticks = match a.codec {
            AudioCodecConfig::Pcm { .. } => samples as u64,
            _ => data.len() as u64,
        };
        self.write_chunk(1, &AUDIO_CHUNK, data, true, ticks)?;
        self.audio_samples += samples as u64;
        self.audio_bytes += data.len() as u64;
        self.audio_chunks += 1;
        Ok(())
    }

    /// Writes the standard indexes of the current RIFF, patches its sizes and,
    /// for the first RIFF, appends the legacy `idx1`.
    fn close_riff(&mut self) -> Result<()> {
        let stream_count = if self.audio.is_some() { 2 } else { 1 };
        for stream in 0..stream_count {
            let id = if stream == 0 { VIDEO_CHUNK } else { AUDIO_CHUNK };
            let entries = std::mem::take(&mut self.streams[stream].current);
            let duration = std::mem::take(&mut self.streams[stream].current_duration);
            if entries.is_empty() && stream == 1 {
                continue;
            }
            let ix_pos = self.pos;
            let mut b = Le::default();
            let ix_id = [b'i', b'x', id[0], id[1]];
            b.chunk(&ix_id, |b| {
                b.u16(2).u8(0).u8(AVI_INDEX_OF_CHUNKS);
                b.u32(entries.len() as u32);
                b.fourcc(&id);
                b.u64(self.movi_pos);
                b.u32(0);
                for e in &entries {
                    b.u32((e.header_pos + 8 - self.movi_pos) as u32);
                    b.u32(if e.keyframe { e.size } else { e.size | 0x8000_0000 });
                }
            });
            self.write(&b.0)?;
            self.streams[stream].supers.push(SuperEntry {
                offset: ix_pos,
                size: b.0.len() as u32,
                duration: duration.min(u32::MAX as u64) as u32,
            });
        }
        // Patch the movi LIST size (the size counts the `movi` type + payload).
        let movi_size = self.pos - self.movi_pos;
        self.patch_u32(self.movi_pos - 4, movi_size as u32)?;
        if self.riff_index == 0 {
            let mut legacy: Vec<(u64, [u8; 4], IndexEntry)> = Vec::new();
            for (stream, id) in [(0usize, VIDEO_CHUNK), (1usize, AUDIO_CHUNK)] {
                for e in std::mem::take(&mut self.streams[stream].legacy) {
                    legacy.push((e.header_pos, id, e));
                }
            }
            legacy.sort_by_key(|(pos, _, _)| *pos);
            let mut b = Le::default();
            b.chunk(b"idx1", |b| {
                for (_, id, e) in &legacy {
                    b.fourcc(id);
                    b.u32(if e.keyframe { AVIIF_KEYFRAME } else { 0 });
                    b.u32((e.header_pos - self.movi_pos) as u32);
                    b.u32(e.size);
                }
            });
            self.write(&b.0)?;
        }
        // Patch the RIFF size.
        let riff_size = (self.pos - self.riff_start - 8) as u32;
        if self.riff_index == 0 {
            self.riff0_size = riff_size;
        }
        self.patch_u32(self.riff_start + 4, riff_size)?;
        Ok(())
    }

    fn patch_u32(&mut self, at: u64, value: u32) -> Result<()> {
        self.out.seek(SeekFrom::Start(at))?;
        self.out.write_all(&value.to_le_bytes())?;
        self.out.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }

    /// Closes the last RIFF, rewrites the header with the final counts and flushes.
    pub fn finalize(mut self) -> Result<()> {
        if self.total_frames == 0 {
            bail!("no video frames were written");
        }
        self.close_riff()?;
        let header = self.build_header();
        if header.len() as u64 != self.header_len {
            bail!("AVI header size changed ({} → {})", self.header_len, header.len());
        }
        self.out.seek(SeekFrom::Start(0))?;
        self.out.write_all(&header)?;
        self.out.seek(SeekFrom::Start(self.pos))?;
        // The header template carries a placeholder RIFF size; restore the real one.
        self.patch_u32(4, self.riff0_size)?;
        self.out.flush().context("flushing AVI")?;
        self.finalized = true;
        Ok(())
    }
}

impl<W: Write + Seek> Drop for AviWriter<W> {
    fn drop(&mut self) {
        if !self.finalized {
            log::warn!("AviWriter dropped without finalize(); file will lack indexes");
        }
    }
}

/// `indx` super-index with a fixed number of reserved entries.
fn super_index(b: &mut Le, chunk_id: &[u8; 4], entries: &[SuperEntry]) {
    b.chunk(b"indx", |b| {
        b.u16(4).u8(0).u8(AVI_INDEX_OF_INDEXES);
        b.u32(entries.len().min(SUPER_INDEX_ENTRIES) as u32);
        b.fourcc(chunk_id);
        b.zeros(12);
        for i in 0..SUPER_INDEX_ENTRIES {
            match entries.get(i) {
                Some(e) => {
                    b.u64(e.offset).u32(e.size).u32(e.duration);
                }
                None => {
                    b.zeros(16);
                }
            }
        }
    });
}

/// `WAVEFORMATEX` (+ codec-specific extension) for the audio `strf`.
fn wave_format(b: &mut Le, a: &AudioTrackConfig) {
    match &a.codec {
        AudioCodecConfig::Pcm { bits } => {
            let align = a.channels * bits / 8;
            b.u16(0x0001).u16(a.channels).u32(a.sample_rate);
            b.u32(a.sample_rate * align as u32).u16(align).u16(*bits).u16(0);
        }
        AudioCodecConfig::Mp3 => {
            b.u16(0x0055).u16(a.channels).u32(a.sample_rate);
            b.u32(a.bitrate_bps / 8).u16(1).u16(0).u16(12);
            // MPEGLAYER3WAVEFORMAT
            b.u16(1); // wID = MPEGLAYER3_ID_MPEG
            b.u32(2); // fdwFlags = MPEGLAYER3_FLAG_PADDING_OFF
            b.u16((144 * a.bitrate_bps / a.sample_rate.max(1)) as u16); // nBlockSize
            b.u16(1); // nFramesPerBlock
            b.u16(0); // nCodecDelay
        }
        AudioCodecConfig::Aac { asc } => {
            b.u16(0x00FF).u16(a.channels).u32(a.sample_rate);
            b.u32(a.bitrate_bps / 8).u16(1).u16(0).u16(asc.len() as u16);
            b.bytes(asc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_chunks_pad_odd_sizes() {
        let mut b = Le::default();
        b.chunk(b"test", |b| {
            b.bytes(&[1, 2, 3]);
        });
        assert_eq!(b.0, [b't', b'e', b's', b't', 3, 0, 0, 0, 1, 2, 3, 0]);
        let mut l = Le::default();
        l.list(b"movi", |_| {});
        assert_eq!(l.0, [b'L', b'I', b'S', b'T', 4, 0, 0, 0, b'm', b'o', b'v', b'i']);
    }
}
