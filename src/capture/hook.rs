//! Game capture: reading the frames openclip's hook publishes from inside a game.
//!
//! The counterpart to `crates/openclip-hook/src/publish.rs`. The hook copies the
//! game's back buffer into a shared texture and signals an event; this reads that
//! texture back through the same double-buffered staging path
//! [`Readback`](super::windows::Readback) uses for Windows.Graphics.Capture, so
//! the two backends produce identical [`RawFrame`]s and everything downstream is
//! none the wiser.
//!
//! The hook does its own rate limiting, so most of a fast game's presents never
//! reach us at all — skipping the GPU copy is where the saving is, not the
//! readback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use openclip_overlay::abi::Transport;
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_SDK_VERSION,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIKeyedMutex, DXGI_ERROR_NOT_FOUND, DXGI_SHARED_RESOURCE_READ,
};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

use super::windows::Readback;
use super::{CaptureConfig, CaptureHandle, FramePool, FrameSink, Source};
use crate::game::shared::{qpc_frequency, qpc_now, HookSession};

/// How long to wait on the hook's frame event before checking on the game.
const FRAME_WAIT: Duration = Duration::from_millis(100);
/// A game that has not presented for this long is hung, minimised or gone.
const ALIVE_WITHIN: Duration = Duration::from_secs(5);

pub fn start(config: CaptureConfig, epoch: Instant, sink: FrameSink) -> Result<CaptureHandle> {
    let Source::Game { pid } = config.source else {
        bail!("game capture called with a source that is not a game");
    };

    let session = HookSession::open(pid).with_context(|| format!("attaching to the hook in process {pid}"))?;
    if !session.is_hooked() {
        bail!("the hook is not loaded in that game yet");
    }
    if !session.is_alive(ALIVE_WITHIN) {
        bail!("that game is not presenting frames");
    }

    let mut notes = Vec::new();
    if session.control().transport() == Transport::SharedMemory {
        notes.push(crate::t!(NOTE_GAME_SLOW_TRANSPORT).to_string());
    }

    // The shared texture lives on the adapter the *game* renders with. On a
    // hybrid laptop that is the discrete GPU while openclip may have landed on
    // the integrated one, and a cross-adapter shared texture simply does not
    // open — this is the difference between working and mystifying.
    let luid = session.control().adapter_luid.load(Ordering::Relaxed);
    let (device, context, same_adapter) = open_device(luid)?;
    if !same_adapter {
        notes.push(crate::t!(NOTE_GAME_DIFFERENT_ADAPTER).to_string());
    }

    session.set_capture_fps(config.fps.max(1));
    session.set_capturing(true);

    let stop = Arc::new(AtomicBool::new(false));
    let pool = config.pool.clone().unwrap_or_else(|| FramePool::new(6));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("openclip-hookcap".into())
            .spawn(move || pump(session, device, context, epoch, stop, sink, pool))
            .context("starting the game-capture thread")?
    };

    let thread = std::sync::Mutex::new(Some(thread));
    let stopper = {
        let stop = stop.clone();
        Box::new(move || -> Result<()> {
            stop.store(true, Ordering::SeqCst);
            match thread.lock().unwrap().take() {
                Some(h) => h.join().map_err(|_| anyhow!("the game-capture thread panicked"))?,
                None => Ok(()),
            }
        })
    };
    Ok(CaptureHandle::new(stop, stopper).with_note((!notes.is_empty()).then(|| notes.join("; "))))
}

/// Reads published frames until asked to stop or the game goes away.
fn pump(
    session: HookSession,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    mut sink: FrameSink,
    pool: Arc<FramePool>,
) -> Result<()> {
    // The readback must not be starved by the encoder's worker threads, the same
    // reasoning as the WGC delivery thread.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };

    let clock = QpcClock::anchor();
    let mut readback = Readback::new(device.clone(), context);
    let device1: ID3D11Device1 = device.cast().context("this GPU does not support shared textures by name")?;

    let control = session.control();
    let mut textures: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)> = Vec::new();
    let mut generation = 0u64;
    let mut last_seq = 0u64;

    while !stop.load(Ordering::SeqCst) {
        if !session.wait_for_frame(FRAME_WAIT) {
            if !session.is_alive(ALIVE_WITHIN) {
                log::info!("game capture: the game stopped presenting; ending the recording");
                break;
            }
            continue;
        }

        // Acquire: pairs with the hook's release of `frame_seq`, so the slot's
        // pixels and its timestamp are both visible before it is read.
        let seq = control.frame_seq.load(Ordering::Acquire);
        if seq == last_seq {
            continue;
        }
        last_seq = seq;

        let current = control.generation.load(Ordering::Acquire);
        if current != generation || textures.is_empty() {
            match open_textures(&device1, control) {
                Ok(t) => {
                    textures = t;
                    generation = current;
                    log::debug!("game capture: opened generation {current}");
                }
                Err(e) => {
                    log::warn!("game capture: cannot open the shared textures: {e:#}");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
        }

        let slot = control.slot.load(Ordering::Relaxed) as usize;
        let Some((texture, mutex)) = textures.get(slot) else { continue };
        let (w, h) = (control.width.load(Ordering::Relaxed), control.height.load(Ordering::Relaxed));
        if w == 0 || h == 0 {
            continue;
        }
        let format = DXGI_FORMAT(control.dxgi_format.load(Ordering::Relaxed) as i32);
        let pts = clock.pts(control.qpc[slot].load(Ordering::Relaxed), epoch);

        // Key 1 is what the hook released the slot with. A short timeout rather
        // than forever: the hook only holds it for one copy, and blocking here
        // would be a deadlock waiting to happen if it ever died mid-write.
        // SAFETY: `mutex` guards `texture`, both opened by us above.
        if unsafe { mutex.AcquireSync(1, 16) }.is_err() {
            continue;
        }
        let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        let delivered = readback.submit(texture, format, bx, pts, &pool);
        // SAFETY: as above; key 0 hands the slot back to the hook.
        let _ = unsafe { mutex.ReleaseSync(0) };

        match delivered {
            Ok(Some(frame)) => {
                if !sink(frame) {
                    break;
                }
            }
            Ok(None) => {} // the readback is one frame deep; nothing yet
            Err(e) => {
                log::warn!("game capture: readback failed: {e:#}");
                // Most likely the textures were recreated under us; reopen.
                textures.clear();
            }
        }
    }
    Ok(())
}

/// Opens both of the hook's shared textures by name.
fn open_textures(
    device: &ID3D11Device1,
    control: &openclip_overlay::abi::Control,
) -> Result<Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>> {
    let mut out = Vec::new();
    for slot in 0..openclip_overlay::abi::TEX_SLOTS {
        let name = control.tex_name_at(slot).ok_or_else(|| anyhow!("the hook has not published slot {slot} yet"))?;
        let wide = HSTRING::from(name);
        // SAFETY: opening a named shared resource the hook created; the name is
        // NUL-terminated by the ABI helper that wrote it.
        let texture: ID3D11Texture2D =
            unsafe { device.OpenSharedResourceByName(PCWSTR(wide.as_ptr()), DXGI_SHARED_RESOURCE_READ.0) }
                .with_context(|| format!("opening shared texture {name}"))?;
        let mutex: IDXGIKeyedMutex = texture.cast().context("the shared texture has no keyed mutex")?;
        out.push((texture, mutex));
    }
    Ok(out)
}

/// A D3D11 device on the adapter with `luid`, falling back to the default one.
///
/// The `bool` is false when the fallback was taken, so the caller can say so
/// rather than leaving the user with a game mode that silently never works.
fn open_device(luid: u64) -> Result<(ID3D11Device, ID3D11DeviceContext, bool)> {
    let adapter = (luid != 0).then(|| adapter_with_luid(luid)).flatten();
    let matched = adapter.is_some();
    let mut device = None;
    let mut context = None;
    // SAFETY: standard device creation. `D3D_DRIVER_TYPE_UNKNOWN` is required
    // (and only valid) when an adapter is supplied.
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            if matched { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE },
            HMODULE::default(),
            Default::default(),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("creating a Direct3D device for game capture")?;
    }
    Ok((
        device.ok_or_else(|| anyhow!("no Direct3D device"))?,
        context.ok_or_else(|| anyhow!("no Direct3D context"))?,
        matched,
    ))
}

fn adapter_with_luid(luid: u64) -> Option<IDXGIAdapter> {
    // SAFETY: plain DXGI enumeration; the loop ends on DXGI_ERROR_NOT_FOUND.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for index in 0.. {
            match factory.EnumAdapters1(index) {
                Ok(adapter) => {
                    if let Ok(desc) = adapter.GetDesc1() {
                        let found = ((desc.AdapterLuid.HighPart as u64) << 32) | desc.AdapterLuid.LowPart as u64;
                        if found == luid {
                            return adapter.cast().ok();
                        }
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(_) => break,
            }
        }
    }
    None
}

/// Maps the hook's `QueryPerformanceCounter` stamps onto the recording timeline.
///
/// The hook timestamps a frame when the game presented it. Using that rather
/// than the moment this thread woke up is what keeps recorded motion matched to
/// the game's real pacing instead of to readback jitter.
struct QpcClock {
    qpc0: i64,
    freq: i64,
    t0: Instant,
}

impl QpcClock {
    fn anchor() -> Self {
        // Sampled back to back, so the two clocks are tied at one instant.
        let qpc0 = qpc_now();
        let t0 = Instant::now();
        Self { qpc0, freq: qpc_frequency(), t0 }
    }

    fn pts(&self, qpc: u64, epoch: Instant) -> Duration {
        Self::pts_from(self.qpc0, self.freq, self.t0, qpc, epoch)
    }

    /// Split out so the arithmetic can be tested without a clock.
    fn pts_from(qpc0: i64, freq: i64, t0: Instant, qpc: u64, epoch: Instant) -> Duration {
        if qpc == 0 || freq <= 0 {
            return t0.saturating_duration_since(epoch);
        }
        let delta = qpc as i64 - qpc0;
        let at = if delta >= 0 {
            t0 + Duration::from_secs_f64(delta as f64 / freq as f64)
        } else {
            // A frame presented before we anchored; clamp rather than wrap.
            t0.checked_sub(Duration::from_secs_f64(-delta as f64 / freq as f64)).unwrap_or(t0)
        };
        at.saturating_duration_since(epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_stamps_map_onto_the_recording_timeline() {
        const FREQ: i64 = 10_000_000;
        let epoch = Instant::now();
        let t0 = epoch + Duration::from_millis(500);
        let qpc0 = 1_000_000_000;

        // A frame presented exactly when we anchored is half a second in.
        let at_anchor = QpcClock::pts_from(qpc0, FREQ, t0, qpc0 as u64, epoch);
        assert!((at_anchor.as_secs_f64() - 0.5).abs() < 1e-6, "{at_anchor:?}");

        // A quarter of a second later.
        let later = QpcClock::pts_from(qpc0, FREQ, t0, (qpc0 + FREQ / 4) as u64, epoch);
        assert!((later.as_secs_f64() - 0.75).abs() < 1e-6, "{later:?}");

        // Before the anchor, but still after the epoch.
        let earlier = QpcClock::pts_from(qpc0, FREQ, t0, (qpc0 - FREQ / 4) as u64, epoch);
        assert!((earlier.as_secs_f64() - 0.25).abs() < 1e-6, "{earlier:?}");
    }

    #[test]
    fn a_missing_or_broken_stamp_falls_back_to_the_anchor() {
        let epoch = Instant::now();
        let t0 = epoch + Duration::from_millis(200);
        // No timestamp yet, and a nonsense frequency: neither may panic or wrap.
        assert_eq!(QpcClock::pts_from(1_000, 10_000_000, t0, 0, epoch), Duration::from_millis(200));
        assert_eq!(QpcClock::pts_from(1_000, 0, t0, 5_000, epoch), Duration::from_millis(200));
    }

    #[test]
    fn a_frame_from_before_the_epoch_clamps_to_zero() {
        let epoch = Instant::now();
        // Anchored at the epoch; a frame from well before it cannot go negative.
        let pts = QpcClock::pts_from(1_000_000, 10_000_000, epoch, 1u64, epoch);
        assert_eq!(pts, Duration::ZERO);
    }
}
