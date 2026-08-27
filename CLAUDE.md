# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

openclip is a cross-platform screen recorder in Rust (edition 2024, Rust 1.85+) with an egui/eframe GUI. It writes standard MP4 (H.264 + MP3) with **no external runtime dependencies**: OpenH264 and LAME are compiled from bundled sources by their crates at build time, and the MP4 muxer is in-house. A C/C++ toolchain is required to build (MSVC on Windows; autotools on macOS/Linux — see README for the full package list). Installing `nasm` before building enables OpenH264's assembly kernels (~2× encoder throughput); without it the build silently falls back to C.

## Commands

```sh
cargo build --release                 # first build compiles OpenH264 + LAME from source (slow)
cargo run --release                   # launch the GUI
cargo test                            # unit tests (src/audio, src/mux) + tests/mp4_roundtrip.rs
cargo test --test mp4_roundtrip       # just the MP4 round-trip integration test
cargo test --lib avc                  # run unit tests matching a name
cargo clippy --all-targets

# Headless examples (useful for testing the pipeline without the GUI)
cargo run --release --example capture_to_mp4 -- 10 out.mp4   # record primary monitor for 10 s
#   flags: --half --mic --no-audio --fx --region X,Y,W,H --window TITLE --pause-at S --resume-at S
cargo run --release --example bench_encode -- 1920 1080 5    # encoder throughput on synthetic content
```

`profile.dev` uses `opt-level = 1` for the crate and `opt-level = 3` for dependencies, so debug builds are usable for real-time capture. In release builds `main.rs` sets `windows_subsystem = "windows"` (no console); use a debug build or `RUST_LOG` via `env_logger` when you need log output.

`.gitignore` excludes `*.mp4`, `*.h264`, `*.wav`, `*.mp3` — examples and manual tests can write scratch media into the repo root safely.

## Architecture

The crate is a library (`src/lib.rs`) plus a thin binary (`src/main.rs`) that only builds the eframe window and instantiates `openclip::ui::App`. Examples and the integration test link against the library API, so keep pipeline/codec code out of `ui`.

```
capture backend ──RawFrame──▶ encode thread ──▶ OpenH264 ──▶ MP4 muxer ──▶ file
                                   ▲                             ▲
cpal (mic / loopback) ──▶ mixer ───┴──▶ LAME MP3 ────────────────┘
```

### `src/pipeline.rs` — the recording session (`Recorder`)

The central orchestrator. `Recorder::start(RecordConfig, on_preview)` spawns:

- a **capture backend** (`capture::start`) whose sink pushes `RawFrame`s into a bounded `sync_channel(4)`; when full, frames are dropped and counted in `Stats::frames_dropped` (never queued late — sync is preserved by timestamps, not by frame count);
- an **encode thread** (`encode_loop`): optional half-res downscale, mouse-effects painting (on a copy, so the clean frame can be reused), BGRA/RGBA→I420, OpenH264, muxing. When no frame arrives within one frame interval it re-encodes the last frame as a **heartbeat** so static screens keep a steady cadence. The first video pts becomes the shared `audio_origin` timeline origin;
- an **audio thread** (if `wants_audio()`): cpal streams → `Mixer` → `Mp3Encoder` → `Mp3Frame` channel consumed by the encode thread's muxer.

**Timeline model:** every frame/chunk carries a timestamp relative to `epoch`. Pause is implemented by subtracting accumulated `paused_total_us` from timestamps (frames arriving while paused are discarded), so paused time is cut out of the file with A/V still in sync. Audio is mixed `AUDIO_LAG` (150 ms) behind wall-clock so device latency never creates gaps. `Stats` (atomics + `Mutex<Option<String>>` error/note) is the only channel back to the GUI besides `PreviewSlot`.

### `src/capture/` — platform backends behind one interface

`capture::start(CaptureConfig, epoch, FrameSink) -> CaptureHandle`. `Source` is `Monitor{id}` / `Window{id}` / `Region{monitor_id, rect}` with physical-pixel `Rect`s. Backend selection is `cfg(windows)` → `windows.rs` (Windows.Graphics.Capture via `windows-capture`, GPU-side crop for regions, native cursor) vs. `xcap_backend.rs` (xcap video recorder for monitors, screenshot polling for windows). `monitors.rs` uses `xcap` on **every** platform for enumeration, one-shot screenshots (picker backdrop, previews) and `source_origin` (needed to map global mouse coords into the frame). `FpsLimiter` is the shared wall-clock throttle.

### `src/video/`

- `convert.rs`: `RawFrame` (BGRA/RGBA + pts + optional mouse snapshot) and `Converter` (→ I420 via the SIMD `yuv` crate, plus half-res downscale).
- `encoder.rs`: OpenH264 in screen-content real-time mode. Annex-B output is converted to AVCC length-prefixed samples; SPS/PPS are extracted for the `avcC` box (see `mux/avc.rs`).
- `mouse_fx.rs`: `MouseFx` settings, `MouseSampler` (global pointer via `device_query`), and the painters for cursor sprite / click ripples / highlight halo. Effects are painted onto frames in the encode thread **and** in the live preview so the two always match. At `cursor_size == 100` the native cursor from the capture API is used; any other size hides it and draws the scalable arrow.
- `preview.rs`: downscaled `PreviewImage` for the GUI.

### `src/audio/`

`capture.rs` opens cpal streams (mic, or WASAPI loopback for system audio) that push timestamped `Chunk`s into a `SharedQueue`. `mixer.rs` places chunks on the recording timeline by arrival time, fills gaps > 60 ms with silence (WASAPI loopback goes quiet when the system is silent), resamples (`resample.rs`, linear) to 48 kHz stereo and sums sources. `mp3.rs` wraps LAME and splits its output so **each MP4 sample is exactly one 1152-sample MP3 frame**.

### `src/mux/`

Streaming, non-fragmented MP4: `ftyp` | 64-bit `mdat` (samples interleaved in ~0.5 s chunks) | `moov` written at `finalize`. Per-sample durations come from real pts (video timescale 90 000), keyframe table (`stss`), `co64` offsets. `boxes.rs` is the box-writing helper layer, `avc.rs` handles Annex-B ↔ AVCC and SPS/PPS. `tests/mp4_roundtrip.rs` validates output with the independent `mp4-atom` parser — extend it when changing box layout.

### `src/ui/` — egui application

`App` (in `mod.rs`, the largest file) owns all settings and a `State` enum: `Idle` / `Picking(Picker)` / `Recording(Recorder)`. Layout is toolbar + status strip on top, left nav (Home / General / Video / Image / About), pages in the centre. Sub-modules extend `App` via `impl App` blocks:

- `picker.rs`: region selector — one undecorated always-on-top viewport per monitor showing a fresh screenshot; drag a rect, Esc cancels.
- `minibar.rs`: **compact mode** — the main viewport is resized into a small always-on-top bar (`enter_compact` / restore). Closing the bar window restores the full window rather than quitting (`intercept_close`). The bar is placed next to a picked region and the region is *docked* to the bar (`follow_bar`, `bar_anchor`, `bar_settle_until` to ignore our own position commands).
- `region_frame.rs`: border around the selected region while compact. Child viewports can't be transparent with wgpu/DX12 on Windows, so the frame is four opaque click-through strip viewports placed a `GAP_PX` *outside* the rect (so they're never captured). DWM styling on Windows is applied once (`frame_styled`).
- `library.rs`: file browser for the output folder (Videos / Images / Audios tabs); `theme.rs` colours and `apply_theme`; `icons.rs` Font Awesome glyphs (font in `assets/fonts`).

`LivePreview` runs a separate low-fps capture (`20` fps) only while the Home → Preview tab is visible; it is stopped on compact mode and on exit. `MouseFx` is shared with the encode thread as `Arc<RwLock<MouseFx>>` so edits in the Mouse tab apply live.

## Conventions and gotchas

- All coordinates in `capture` are **physical pixels**; egui works in points. Convert via the monitor's scale factor (see `region_frame.rs` / `minibar.rs`) when placing viewports around a `Rect`.
- Windows-only code lives under `#[cfg(windows)]` in `capture/windows.rs`, `build.rs` (icon embedding via `winresource`) and a few DWM calls in `ui`; keep non-Windows builds compiling (`xcap_backend.rs` path) when touching shared interfaces.
- If a captured window is resized mid-recording, frames of the new size are dropped rather than re-negotiating the encoder.
- Output files are `<prefix>-YYYYMMDD-HHMMSS.mp4` in the output folder (default `~/Videos`); snapshots are PNG alongside.
- `docs/` is the GitHub Pages site (`index.html` + screenshots), not developer documentation.
