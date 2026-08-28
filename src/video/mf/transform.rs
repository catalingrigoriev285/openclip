//! A synchronous-style wrapper around an `IMFTransform` that also drives the
//! event-based protocol of asynchronous (hardware) transforms.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFMediaBuffer, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    MEError, METransformDrainComplete, METransformHaveOutput, METransformNeedInput, MFCreateMemoryBuffer,
    MFCreateSample, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_EVENT_FLAG_NO_WAIT, MF_E_BUFFERTOOSMALL,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK,
};

use super::{variant_bool, variant_u32};

/// How long to wait for an asynchronous transform to accept input before giving up.
const STALL_TIMEOUT: Duration = Duration::from_secs(2);

pub struct MftSession {
    mft: IMFTransform,
    /// Present for asynchronous transforms (hardware encoders).
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    in_id: u32,
    out_id: u32,
    provides_samples: bool,
    out_buf_size: u32,
    /// Outstanding `METransformNeedInput` credits.
    need_input: u32,
    started: bool,
}

impl MftSession {
    pub fn from_activate(activate: &IMFActivate) -> Result<Self> {
        let mft: IMFTransform = unsafe { activate.ActivateObject() }.context("ActivateObject")?;
        Self::from_transform(mft)
    }

    pub fn from_transform(mft: IMFTransform) -> Result<Self> {
        let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
        // E_NOTIMPL means the streams are simply numbered from zero.
        let _ = unsafe { mft.GetStreamIDs(&mut in_ids, &mut out_ids) };
        let codec_api = mft.cast::<ICodecAPI>().ok();
        let mut events = None;
        if let Ok(attrs) = unsafe { mft.GetAttributes() }
            && unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1
        {
            unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.context("MF_TRANSFORM_ASYNC_UNLOCK")?;
            events = Some(mft.cast::<IMFMediaEventGenerator>().context("async MFT without event generator")?);
        }
        Ok(Self {
            mft,
            events,
            codec_api,
            in_id: in_ids[0],
            out_id: out_ids[0],
            provides_samples: false,
            out_buf_size: 0,
            need_input: 0,
            started: false,
        })
    }

    pub fn is_async(&self) -> bool {
        self.events.is_some()
    }

    pub fn transform(&self) -> &IMFTransform {
        &self.mft
    }

    pub fn has_codec_api(&self) -> bool {
        self.codec_api.is_some()
    }

    pub fn set_output_type(&self, t: &IMFMediaType) -> Result<()> {
        unsafe { self.mft.SetOutputType(self.out_id, t, 0) }.context("SetOutputType")
    }

    pub fn set_input_type(&self, t: &IMFMediaType) -> Result<()> {
        unsafe { self.mft.SetInputType(self.in_id, t, 0) }.context("SetInputType")
    }

    pub fn output_available_types(&self) -> Vec<IMFMediaType> {
        let mut v = Vec::new();
        for i in 0.. {
            match unsafe { self.mft.GetOutputAvailableType(self.out_id, i) } {
                Ok(t) => v.push(t),
                Err(_) => break,
            }
        }
        v
    }

    pub fn input_available_types(&self) -> Vec<IMFMediaType> {
        let mut v = Vec::new();
        for i in 0.. {
            match unsafe { self.mft.GetInputAvailableType(self.in_id, i) } {
                Ok(t) => v.push(t),
                Err(_) => break,
            }
        }
        v
    }

    pub fn output_type(&self) -> Result<IMFMediaType> {
        unsafe { self.mft.GetOutputCurrentType(self.out_id) }.context("GetOutputCurrentType")
    }

    /// Sets an `ICodecAPI` property; unsupported properties are logged, not fatal.
    pub fn set_u32(&self, key: &GUID, value: u32) -> bool {
        let Some(api) = &self.codec_api else { return false };
        match unsafe { api.SetValue(key, &variant_u32(value)) } {
            Ok(()) => true,
            Err(e) => {
                log::debug!("codec API {key:?} = {value} rejected: {e}");
                false
            }
        }
    }

    pub fn set_bool(&self, key: &GUID, value: bool) -> bool {
        let Some(api) = &self.codec_api else { return false };
        match unsafe { api.SetValue(key, &variant_bool(value)) } {
            Ok(()) => true,
            Err(e) => {
                log::debug!("codec API {key:?} = {value} rejected: {e}");
                false
            }
        }
    }

    /// Begins streaming. Output buffers are allocated by us unless the
    /// transform provides its own samples.
    pub fn start(&mut self, min_out_buf: u32) -> Result<()> {
        let info = unsafe { self.mft.GetOutputStreamInfo(self.out_id) }.context("GetOutputStreamInfo")?;
        self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        self.out_buf_size = info.cbSize.max(min_out_buf);
        log::debug!(
            "MFT start: async={} out_flags={:#x} cbSize={} provides_samples={} streams in={} out={}",
            self.is_async(),
            info.dwFlags,
            info.cbSize,
            self.provides_samples,
            self.in_id,
            self.out_id
        );
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("NOTIFY_BEGIN_STREAMING")?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).context("NOTIFY_START_OF_STREAM")?;
        }
        self.started = true;
        Ok(())
    }

    /// Feeds one sample and returns every output sample that became available.
    pub fn process(&mut self, sample: &IMFSample) -> Result<Vec<IMFSample>> {
        let mut out = Vec::new();
        if self.events.is_some() {
            self.pump_events(&mut out, true)?;
            unsafe { self.mft.ProcessInput(self.in_id, sample, 0) }.context("ProcessInput")?;
            self.need_input = self.need_input.saturating_sub(1);
            self.pump_events(&mut out, false)?;
        } else {
            unsafe { self.mft.ProcessInput(self.in_id, sample, 0) }.context("ProcessInput")?;
            while self.pull_output(&mut out)? {}
        }
        Ok(out)
    }

    /// Flushes everything still buffered.
    pub fn drain(&mut self) -> Result<Vec<IMFSample>> {
        let mut out = Vec::new();
        if !self.started {
            return Ok(out);
        }
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0).ok();
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).context("COMMAND_DRAIN")?;
        }
        if let Some(events) = self.events.clone() {
            let deadline = Instant::now() + STALL_TIMEOUT;
            loop {
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(ev) => {
                        let t = unsafe { ev.GetType() }.unwrap_or(0);
                        if t == METransformHaveOutput.0 as u32 {
                            self.pull_output(&mut out)?;
                        } else if t == METransformDrainComplete.0 as u32 {
                            break;
                        } else if t == METransformNeedInput.0 as u32 {
                            self.need_input += 1;
                        } else if t == MEError.0 as u32 {
                            bail!("encoder reported an error while draining");
                        }
                    }
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        if Instant::now() > deadline {
                            log::warn!("encoder did not complete draining in time");
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => return Err(anyhow!("GetEvent: {e}")),
                }
            }
        } else {
            while self.pull_output(&mut out)? {}
        }
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0) }.ok();
        self.started = false;
        Ok(out)
    }

    /// Handles pending events. With `wait_for_input`, blocks (bounded) until
    /// the transform is ready to accept a sample.
    fn pump_events(&mut self, out: &mut Vec<IMFSample>, wait_for_input: bool) -> Result<()> {
        let Some(events) = self.events.clone() else { return Ok(()) };
        let deadline = Instant::now() + STALL_TIMEOUT;
        loop {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    let t = unsafe { ev.GetType() }.unwrap_or(0);
                    log::trace!("MFT event {t}");
                    if t == METransformNeedInput.0 as u32 {
                        self.need_input += 1;
                    } else if t == METransformHaveOutput.0 as u32 {
                        self.pull_output(out)?;
                    } else if t == MEError.0 as u32 {
                        let status = unsafe { ev.GetStatus() }.map(|h| format!("{h:?}")).unwrap_or_default();
                        bail!("encoder reported an error {status}");
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if !wait_for_input || self.need_input > 0 {
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        bail!("encoder stalled (no input request for {STALL_TIMEOUT:?})");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(anyhow!("GetEvent: {e}")),
            }
        }
    }

    /// Calls `ProcessOutput` once. Returns `false` when the transform needs more input.
    fn pull_output(&mut self, out: &mut Vec<IMFSample>) -> Result<bool> {
        loop {
            let sample = if self.provides_samples { None } else { Some(new_sample(self.out_buf_size)?) };
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_id,
                pSample: std::mem::ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let result = unsafe { self.mft.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status) };
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut buf.pSample) };
            unsafe { std::mem::ManuallyDrop::drop(&mut buf.pEvents) };
            if let Err(e) = &result {
                log::debug!("ProcessOutput → {e} (status={status:#x} dwStatus={:#x} provides_samples={})", buf.dwStatus, self.provides_samples);
            }
            match result {
                Ok(()) => {
                    if let Some(s) = sample {
                        out.push(s);
                    }
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Renegotiate the output type (same codec, refreshed attributes).
                    let t = unsafe { self.mft.GetOutputAvailableType(self.out_id, 0) }.context("stream change")?;
                    self.set_output_type(&t)?;
                    let info = unsafe { self.mft.GetOutputStreamInfo(self.out_id) }?;
                    self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                    self.out_buf_size = self.out_buf_size.max(info.cbSize);
                    if self.events.is_some() {
                        // Asynchronous transforms raise a new HaveOutput event for
                        // the pending sample; calling ProcessOutput again now fails.
                        return Ok(true);
                    }
                }
                Err(e) if e.code() == MF_E_BUFFERTOOSMALL => {
                    self.out_buf_size = self.out_buf_size.saturating_mul(2).max(1 << 16);
                }
                Err(e) => return Err(anyhow!("ProcessOutput: {e}")),
            }
        }
    }
}

impl Drop for MftSession {
    fn drop(&mut self) {
        if self.started {
            unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0) }.ok();
        }
    }
}

fn new_sample(capacity: u32) -> Result<IMFSample> {
    unsafe {
        let sample = MFCreateSample().context("MFCreateSample")?;
        let buffer = MFCreateMemoryBuffer(capacity.max(1)).context("MFCreateMemoryBuffer")?;
        sample.AddBuffer(&buffer).context("AddBuffer")?;
        Ok(sample)
    }
}

/// Wraps `data` in a sample with the given time/duration (100-ns units).
pub fn make_sample(data: &[u8], time_100ns: i64, duration_100ns: i64) -> Result<IMFSample> {
    unsafe {
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(data.len().max(1) as u32).context("MFCreateMemoryBuffer")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None).context("Lock")?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().context("Unlock")?;
        buffer.SetCurrentLength(data.len() as u32)?;
        let sample = MFCreateSample().context("MFCreateSample")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
        Ok(sample)
    }
}

/// Copies a sample's payload into a `Vec`.
pub fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().context("ConvertToContiguousBuffer")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).context("Lock")?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock().ok();
        Ok(data)
    }
}
