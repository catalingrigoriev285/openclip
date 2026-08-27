//! Audio capture through `cpal`: microphone input and system-audio loopback.
//!
//! Streams deliver interleaved f32 chunks (at the device's native rate and
//! channel count) into a shared queue together with their arrival time, which
//! the mixer uses to place them on the recording timeline.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};

/// A block of interleaved samples with its arrival time.
#[derive(Debug)]
pub struct Chunk {
    pub at: Instant,
    pub data: Vec<f32>,
}

#[derive(Debug, Default)]
pub struct Queue {
    pub chunks: VecDeque<Chunk>,
    pub overflow: u64,
}

pub type SharedQueue = Arc<Mutex<Queue>>;

const MAX_QUEUED_CHUNKS: usize = 512;

/// An open input stream; drop to close. Not `Send` on every platform, so keep
/// it on the thread that created it.
pub struct AudioSource {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub queue: SharedQueue,
    _stream: Stream,
}

/// Names of available microphone / input devices.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Some(name) = device_name(&d) {
                names.push(name);
            }
        }
    }
    names
}

fn device_name(d: &Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_string())
}

/// Opens the microphone (`name`, or the default input device).
pub fn open_microphone(name: Option<&str>) -> Result<AudioSource> {
    let host = cpal::default_host();
    let device = match name {
        Some(n) => host
            .input_devices()?
            .find(|d| device_name(d).as_deref() == Some(n))
            .ok_or_else(|| anyhow!("input device '{n}' not found"))?,
        None => host.default_input_device().context("no default input device")?,
    };
    let config = device.default_input_config().context("microphone default config")?;
    build(device, config, "microphone")
}

/// Opens a loopback capture of the default output device (what the speakers
/// play). Supported on Windows (WASAPI) and macOS 14.6+; on Linux, set
/// `PULSE_SOURCE=<sink>.monitor` or similar for the default input instead.
pub fn open_system_loopback() -> Result<AudioSource> {
    let host = cpal::default_host();
    let device = host.default_output_device().context("no default output device")?;
    let config = device.default_output_config().context("output device default config")?;
    build(device, config, "system audio")
}

fn build(device: Device, supported: cpal::SupportedStreamConfig, what: &str) -> Result<AudioSource> {
    let name = device_name(&device).unwrap_or_else(|| what.to_string());
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let queue: SharedQueue = Arc::new(Mutex::new(Queue::default()));
    let err_fn = {
        let what = what.to_string();
        move |e| log::warn!("{what} stream: {e}")
    };
    let q = queue.clone();
    let push = move |data: Vec<f32>| {
        let mut q = q.lock().unwrap();
        if q.chunks.len() >= MAX_QUEUED_CHUNKS {
            q.chunks.pop_front();
            q.overflow += 1;
        }
        q.chunks.push_back(Chunk { at: Instant::now(), data });
    };
    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |d: &[f32], _| push(d.to_vec()),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |d: &[i16], _| push(d.iter().map(|&s| s as f32 / 32768.0).collect()),
            err_fn,
            None,
        )?,
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |d: &[u16], _| push(d.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect()),
            err_fn,
            None,
        )?,
        SampleFormat::I32 => device.build_input_stream(
            config,
            move |d: &[i32], _| push(d.iter().map(|&s| s as f32 / 2147483648.0).collect()),
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported sample format {other:?} for {what}")),
    };
    stream.play().with_context(|| format!("starting {what} stream"))?;
    log::info!(
        "{what}: '{name}' {} Hz, {} ch, {sample_format:?}",
        config.sample_rate,
        config.channels
    );
    Ok(AudioSource {
        name,
        sample_rate: config.sample_rate,
        channels: config.channels,
        queue,
        _stream: stream,
    })
}
