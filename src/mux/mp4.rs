//! Streaming MP4 writer.
//!
//! Layout: `ftyp` | `mdat` (64-bit largesize, samples interleaved in ~0.5 s
//! chunks) | `moov`. The `moov` box is assembled from in-memory sample tables
//! at [`Mp4Writer::finalize`]. Video is `avc1` (H.264, AVCC length-prefixed
//! samples); audio is `mp4a` with an MPEG-1 Layer III (`objectTypeIndication`
//! 0x6B) `esds`, one MP3 frame per sample.

use std::io::{Seek, SeekFrom, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::avc;
use super::boxes::{mp4_now, unity_matrix, Buf};

/// Timescale of the video track (ticks per second).
pub const VIDEO_TIMESCALE: u32 = 90_000;
/// Timescale of the movie header.
const MOVIE_TIMESCALE: u32 = 1_000;
/// Samples are grouped into chunks covering about this much time.
const CHUNK_DURATION: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct VideoTrackConfig {
    pub width: u32,
    pub height: u32,
    /// Used only for the duration of the final sample.
    pub fps: f64,
}

#[derive(Debug, Clone)]
pub struct AudioTrackConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate_bps: u32,
    /// PCM samples per encoded frame (1152 for MPEG-1 Layer III).
    pub samples_per_frame: u32,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    size: u32,
    /// Presentation time in track timescale units, relative to the first sample.
    pts: u64,
    keyframe: bool,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    offset: u64,
    sample_count: u32,
}

#[derive(Debug, Default)]
struct Track {
    timescale: u32,
    samples: Vec<Sample>,
    chunks: Vec<Chunk>,
    /// Samples buffered for the chunk currently being assembled.
    pending: Vec<u8>,
    pending_count: u32,
    pending_start_pts: u64,
    first_pts: Option<Duration>,
}

impl Track {
    fn new(timescale: u32) -> Self {
        Self { timescale, ..Default::default() }
    }

    fn chunk_ticks(&self) -> u64 {
        CHUNK_DURATION.as_millis() as u64 * self.timescale as u64 / 1000
    }

    fn duration_ticks(&self, last_delta: u32) -> u64 {
        match self.samples.last() {
            None => 0,
            Some(s) => s.pts + last_delta as u64,
        }
    }

    /// Per-sample deltas; the final sample borrows the previous delta (or `fallback`).
    fn deltas(&self, fallback: u32) -> Vec<u32> {
        let n = self.samples.len();
        let mut out: Vec<u32> = Vec::with_capacity(n);
        for i in 0..n {
            let d = if i + 1 < n {
                (self.samples[i + 1].pts - self.samples[i].pts) as u32
            } else if i > 0 {
                out[i - 1]
            } else {
                fallback
            };
            out.push(d.max(1));
        }
        out
    }
}

pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    pos: u64,
    mdat_start: u64,
    video: Option<(Track, VideoTrackConfig)>,
    audio: Option<(Track, AudioTrackConfig)>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    finalized: bool,
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Starts a new file with the given tracks. At least one track is required.
    pub fn new(
        mut out: W,
        video: Option<VideoTrackConfig>,
        audio: Option<AudioTrackConfig>,
    ) -> Result<Self> {
        if video.is_none() && audio.is_none() {
            bail!("MP4 needs at least one track");
        }
        let mut ftyp = Buf::new();
        ftyp.atom(b"ftyp", |b| {
            b.fourcc(b"isom").u32(0x200);
            for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
                b.fourcc(brand);
            }
        });
        out.write_all(&ftyp.0)?;
        let mdat_start = ftyp.0.len() as u64;
        // 64-bit mdat header: size=1 + largesize (patched at finalize).
        let mut hdr = Buf::new();
        hdr.u32(1).fourcc(b"mdat").u64(0);
        out.write_all(&hdr.0)?;
        Ok(Self {
            out,
            pos: mdat_start + 16,
            mdat_start,
            video: video.map(|c| (Track::new(VIDEO_TIMESCALE), c)),
            audio: audio.map(|c| (Track::new(c.sample_rate), c)),
            sps: None,
            pps: None,
            finalized: false,
        })
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Registers SPS/PPS (start-code-free NALs). Only the first pair is kept.
    pub fn set_parameter_sets(&mut self, sps: &[u8], pps: &[u8]) {
        if self.sps.is_none() {
            self.sps = Some(sps.to_vec());
        }
        if self.pps.is_none() {
            self.pps = Some(pps.to_vec());
        }
    }

    /// Appends one video access unit already in AVCC form (4-byte length prefixes).
    pub fn push_video(&mut self, data: &[u8], pts: Duration, keyframe: bool) -> Result<()> {
        let (track, _) = self.video.as_mut().context("no video track")?;
        let first = *track.first_pts.get_or_insert(pts);
        let rel = pts.saturating_sub(first);
        let mut ticks = duration_to_ticks(rel, track.timescale);
        if let Some(last) = track.samples.last()
            && ticks <= last.pts
        {
            ticks = last.pts + 1;
        }
        let sample = Sample { size: data.len() as u32, pts: ticks, keyframe };
        Self::push_sample(&mut self.out, &mut self.pos, track, sample, data)
    }

    /// Appends one MP3 frame. Audio samples are assumed contiguous.
    pub fn push_audio(&mut self, frame: &[u8]) -> Result<()> {
        let (track, cfg) = self.audio.as_mut().context("no audio track")?;
        let ticks = track.samples.len() as u64 * cfg.samples_per_frame as u64;
        let sample = Sample { size: frame.len() as u32, pts: ticks, keyframe: true };
        Self::push_sample(&mut self.out, &mut self.pos, track, sample, frame)
    }

    fn push_sample(
        out: &mut W,
        pos: &mut u64,
        track: &mut Track,
        sample: Sample,
        data: &[u8],
    ) -> Result<()> {
        if track.pending_count == 0 {
            track.pending_start_pts = sample.pts;
        }
        track.pending.extend_from_slice(data);
        track.pending_count += 1;
        track.samples.push(sample);
        if sample.pts - track.pending_start_pts >= track.chunk_ticks() {
            Self::flush_chunk(out, pos, track)?;
        }
        Ok(())
    }

    fn flush_chunk(out: &mut W, pos: &mut u64, track: &mut Track) -> Result<()> {
        if track.pending_count == 0 {
            return Ok(());
        }
        out.write_all(&track.pending)?;
        track.chunks.push(Chunk { offset: *pos, sample_count: track.pending_count });
        *pos += track.pending.len() as u64;
        track.pending.clear();
        track.pending_count = 0;
        Ok(())
    }

    /// Bytes written to the file so far.
    pub fn bytes_written(&self) -> u64 {
        self.pos
    }

    /// Number of video samples pushed so far.
    pub fn video_samples(&self) -> usize {
        self.video.as_ref().map(|(t, _)| t.samples.len()).unwrap_or(0)
    }

    /// Number of audio samples (MP3 frames) pushed so far.
    pub fn audio_samples(&self) -> usize {
        self.audio.as_ref().map(|(t, _)| t.samples.len()).unwrap_or(0)
    }

    /// Writes the remaining chunks and the `moov` box, then flushes the writer.
    pub fn finalize(mut self) -> Result<()> {
        if let Some((track, _)) = self.video.as_mut() {
            Self::flush_chunk(&mut self.out, &mut self.pos, track)?;
        }
        if let Some((track, _)) = self.audio.as_mut() {
            Self::flush_chunk(&mut self.out, &mut self.pos, track)?;
        }
        // Patch mdat largesize.
        let mdat_size = self.pos - self.mdat_start;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&mdat_size.to_be_bytes())?;
        self.out.seek(SeekFrom::Start(self.pos))?;

        let moov = self.build_moov()?;
        self.out.write_all(&moov)?;
        self.pos += moov.len() as u64;
        self.out.flush()?;
        self.finalized = true;
        Ok(())
    }

    fn build_moov(&self) -> Result<Vec<u8>> {
        let now = mp4_now();
        let mut tracks: Vec<(Vec<u8>, u64)> = Vec::new(); // (trak box, duration in movie ticks)
        let mut next_track_id = 1u32;

        if let Some((track, cfg)) = &self.video {
            if track.samples.is_empty() {
                bail!("video track has no samples");
            }
            let sps = self.sps.as_deref().context("no SPS recorded for video track")?;
            let pps = self.pps.as_deref().context("no PPS recorded for video track")?;
            let fallback = (track.timescale as f64 / cfg.fps.max(1.0)).round() as u32;
            let deltas = track.deltas(fallback);
            let dur_ticks = track.duration_ticks(*deltas.last().unwrap());
            let movie_dur = rescale(dur_ticks, track.timescale, MOVIE_TIMESCALE);
            let stsd = build_avc1(cfg, sps, pps);
            let trak = build_trak(&TrakParams {
                track_id: next_track_id,
                now,
                track,
                deltas: &deltas,
                movie_duration: movie_dur,
                handler: b"vide",
                handler_name: "VideoHandler",
                dims: Some((cfg.width, cfg.height)),
                stsd_entry: stsd,
                is_video: true,
            });
            tracks.push((trak, movie_dur));
            next_track_id += 1;
        }
        if let Some((track, cfg)) = &self.audio
            && !track.samples.is_empty()
        {
            let deltas = track.deltas(cfg.samples_per_frame);
            let dur_ticks = track.duration_ticks(cfg.samples_per_frame);
            let movie_dur = rescale(dur_ticks, track.timescale, MOVIE_TIMESCALE);
            let stsd = build_mp4a(cfg);
            let trak = build_trak(&TrakParams {
                track_id: next_track_id,
                now,
                track,
                deltas: &deltas,
                movie_duration: movie_dur,
                handler: b"soun",
                handler_name: "SoundHandler",
                dims: None,
                stsd_entry: stsd,
                is_video: false,
            });
            tracks.push((trak, movie_dur));
            next_track_id += 1;
        }
        if tracks.is_empty() {
            bail!("no samples were written");
        }
        let movie_duration = tracks.iter().map(|t| t.1).max().unwrap_or(0);

        let mut moov = Buf::new();
        moov.atom(b"moov", |b| {
            b.full_atom(b"mvhd", 0, 0, |b| {
                b.u32(now as u32).u32(now as u32);
                b.u32(MOVIE_TIMESCALE).u32(movie_duration.min(u32::MAX as u64) as u32);
                b.u32(0x0001_0000).u16(0x0100).zeros(10);
                unity_matrix(b);
                b.zeros(24);
                b.u32(next_track_id);
            });
            for (trak, _) in &tracks {
                b.bytes(trak);
            }
        });
        Ok(moov.into_vec())
    }
}

impl<W: Write + Seek> Drop for Mp4Writer<W> {
    fn drop(&mut self) {
        if !self.finalized {
            log::warn!("Mp4Writer dropped without finalize(); file will lack a moov box");
        }
    }
}

fn duration_to_ticks(d: Duration, timescale: u32) -> u64 {
    (d.as_nanos() * timescale as u128 / 1_000_000_000) as u64
}

fn rescale(v: u64, from: u32, to: u32) -> u64 {
    (v as u128 * to as u128 / from as u128) as u64
}

struct TrakParams<'a> {
    track_id: u32,
    now: u64,
    track: &'a Track,
    deltas: &'a [u32],
    movie_duration: u64,
    handler: &'a [u8; 4],
    handler_name: &'a str,
    dims: Option<(u32, u32)>,
    stsd_entry: Vec<u8>,
    is_video: bool,
}

fn build_trak(p: &TrakParams<'_>) -> Vec<u8> {
    let track = p.track;
    let now = p.now;
    let media_duration = track.duration_ticks(*p.deltas.last().unwrap_or(&0));
    let mut b = Buf::new();
    b.atom(b"trak", |b| {
        b.full_atom(b"tkhd", 0, 0x3, |b| {
            b.u32(now as u32).u32(now as u32).u32(p.track_id).u32(0);
            b.u32(p.movie_duration.min(u32::MAX as u64) as u32);
            b.zeros(8);
            b.u16(0).u16(0);
            b.u16(if p.is_video { 0 } else { 0x0100 }).u16(0);
            unity_matrix(b);
            let (w, h) = p.dims.unwrap_or((0, 0));
            b.u32(w << 16).u32(h << 16);
        });
        b.atom(b"mdia", |b| {
            b.full_atom(b"mdhd", 0, 0, |b| {
                b.u32(now as u32).u32(now as u32);
                b.u32(track.timescale).u32(media_duration.min(u32::MAX as u64) as u32);
                b.u16(0x55C4).u16(0); // language 'und'
            });
            b.full_atom(b"hdlr", 0, 0, |b| {
                b.u32(0).fourcc(p.handler).zeros(12);
                b.bytes(p.handler_name.as_bytes()).u8(0);
            });
            b.atom(b"minf", |b| {
                if p.is_video {
                    b.full_atom(b"vmhd", 0, 1, |b| {
                        b.u16(0).u16(0).u16(0).u16(0);
                    });
                } else {
                    b.full_atom(b"smhd", 0, 0, |b| {
                        b.u16(0).u16(0);
                    });
                }
                b.atom(b"dinf", |b| {
                    b.full_atom(b"dref", 0, 0, |b| {
                        b.u32(1);
                        b.full_atom(b"url ", 0, 1, |_| {});
                    });
                });
                b.atom(b"stbl", |b| {
                    b.full_atom(b"stsd", 0, 0, |b| {
                        b.u32(1).bytes(&p.stsd_entry);
                    });
                    build_stts(b, p.deltas);
                    if p.is_video {
                        build_stss(b, track);
                    }
                    build_stsc(b, track);
                    build_stsz(b, track);
                    build_stco(b, track);
                });
            });
        });
    });
    b.into_vec()
}

fn build_stts(b: &mut Buf, deltas: &[u32]) {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in deltas {
        match runs.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => runs.push((1, d)),
        }
    }
    b.full_atom(b"stts", 0, 0, |b| {
        b.u32(runs.len() as u32);
        for (count, delta) in runs {
            b.u32(count).u32(delta);
        }
    });
}

fn build_stss(b: &mut Buf, track: &Track) {
    let keys: Vec<u32> = track
        .samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.keyframe)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    b.full_atom(b"stss", 0, 0, |b| {
        b.u32(keys.len() as u32);
        for k in keys {
            b.u32(k);
        }
    });
}

fn build_stsc(b: &mut Buf, track: &Track) {
    let mut runs: Vec<(u32, u32)> = Vec::new(); // (first_chunk, samples_per_chunk)
    for (i, c) in track.chunks.iter().enumerate() {
        match runs.last() {
            Some(&(_, n)) if n == c.sample_count => {}
            _ => runs.push((i as u32 + 1, c.sample_count)),
        }
    }
    b.full_atom(b"stsc", 0, 0, |b| {
        b.u32(runs.len() as u32);
        for (first, n) in runs {
            b.u32(first).u32(n).u32(1);
        }
    });
}

fn build_stsz(b: &mut Buf, track: &Track) {
    b.full_atom(b"stsz", 0, 0, |b| {
        b.u32(0).u32(track.samples.len() as u32);
        for s in &track.samples {
            b.u32(s.size);
        }
    });
}

fn build_stco(b: &mut Buf, track: &Track) {
    let needs_64 = track.chunks.iter().any(|c| c.offset > u32::MAX as u64);
    if needs_64 {
        b.full_atom(b"co64", 0, 0, |b| {
            b.u32(track.chunks.len() as u32);
            for c in &track.chunks {
                b.u64(c.offset);
            }
        });
    } else {
        b.full_atom(b"stco", 0, 0, |b| {
            b.u32(track.chunks.len() as u32);
            for c in &track.chunks {
                b.u32(c.offset as u32);
            }
        });
    }
}

fn build_avc1(cfg: &VideoTrackConfig, sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let (profile, compat, level) = avc::sps_profile_info(sps).unwrap_or((66, 0xC0, 31));
    let mut b = Buf::new();
    b.atom(b"avc1", |b| {
        b.zeros(6).u16(1); // reserved, data_reference_index
        b.u16(0).u16(0).zeros(12); // pre_defined, reserved, pre_defined
        b.u16(cfg.width as u16).u16(cfg.height as u16);
        b.u32(0x0048_0000).u32(0x0048_0000); // 72 dpi
        b.u32(0);
        b.u16(1); // frame_count
        let name = b"openclip";
        b.u8(name.len() as u8).bytes(name).zeros(31 - name.len());
        b.u16(0x0018); // depth
        b.u16(0xFFFF); // pre_defined = -1
        b.atom(b"avcC", |b| {
            b.u8(1).u8(profile).u8(compat).u8(level);
            b.u8(0xFC | 3); // lengthSizeMinusOne = 3
            b.u8(0xE0 | 1);
            b.u16(sps.len() as u16).bytes(sps);
            b.u8(1);
            b.u16(pps.len() as u16).bytes(pps);
            if matches!(profile, 100 | 110 | 122 | 144) {
                b.u8(0xFC | 1).u8(0xF8).u8(0xF8).u8(0);
            }
        });
    });
    b.into_vec()
}

fn build_mp4a(cfg: &AudioTrackConfig) -> Vec<u8> {
    let mut b = Buf::new();
    b.atom(b"mp4a", |b| {
        b.zeros(6).u16(1);
        b.u16(0).u16(0).u32(0); // version, revision, vendor
        b.u16(cfg.channels).u16(16);
        b.u16(0).u16(0);
        b.u32(cfg.sample_rate << 16);
        b.full_atom(b"esds", 0, 0, |b| {
            b.descriptor(0x03, |b| {
                b.u16(0).u8(0); // ES_ID, flags
                b.descriptor(0x04, |b| {
                    b.u8(0x6B); // objectTypeIndication: MPEG-1 audio (Layer III)
                    b.u8(0x15); // streamType audio (5) << 2 | reserved 1
                    b.u24(0); // bufferSizeDB
                    b.u32(cfg.bitrate_bps).u32(cfg.bitrate_bps);
                });
                b.descriptor(0x06, |b| {
                    b.u8(0x02);
                });
            });
        });
    });
    b.into_vec()
}
