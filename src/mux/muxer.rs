//! Container-agnostic front for the MP4 and AVI writers.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use super::mp4::{AudioTrackConfig, Mp4Writer, VideoTrackConfig};
use crate::audio::encoder::AudioFrame;
use crate::settings::Container;
use crate::video::encoder::CodecParams;

pub enum Muxer {
    Mp4(Mp4Writer<BufWriter<File>>),
}

impl Muxer {
    pub fn create(
        container: Container,
        path: &Path,
        video: VideoTrackConfig,
        audio: Option<AudioTrackConfig>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).ok();
        }
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let out = BufWriter::with_capacity(1 << 20, file);
        match container {
            Container::Mp4 => Ok(Muxer::Mp4(Mp4Writer::new(out, Some(video), audio)?)),
            Container::Avi => anyhow::bail!("AVI output is not available yet"),
        }
    }

    pub fn has_audio(&self) -> bool {
        match self {
            Muxer::Mp4(m) => m.has_audio(),
        }
    }

    /// Registers the parameter sets (needed by MP4; ignored by AVI).
    pub fn set_codec_params(&mut self, params: &CodecParams) {
        match self {
            Muxer::Mp4(m) => m.set_codec_params(params),
        }
    }

    /// Appends one Annex-B access unit. Returns `false` when the container
    /// could not place the frame (AVI: a frame slot already taken).
    pub fn push_video(&mut self, annexb: &[u8], pts: Duration, keyframe: bool) -> Result<bool> {
        match self {
            Muxer::Mp4(m) => m.push_video(annexb, pts, keyframe).map(|_| true),
        }
    }

    pub fn push_audio(&mut self, frame: &AudioFrame) -> Result<()> {
        match self {
            Muxer::Mp4(m) => m.push_audio(&frame.data),
        }
    }

    pub fn bytes_written(&self) -> u64 {
        match self {
            Muxer::Mp4(m) => m.bytes_written(),
        }
    }

    pub fn finalize(self) -> Result<()> {
        match self {
            Muxer::Mp4(m) => m.finalize().context("finalizing MP4"),
        }
    }
}
