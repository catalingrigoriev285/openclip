<div align="center">

# openclip

[![Latest release](https://img.shields.io/github/v/release/catalingrigoriev285/openclip?style=flat-square&label=release&color=0a84ff)](https://github.com/catalingrigoriev285/openclip/releases/latest)
[![Stars](https://img.shields.io/github/stars/catalingrigoriev285/openclip?style=flat-square&logo=github&color=ffd60a)](https://github.com/catalingrigoriev285/openclip/stargazers)
[![Downloads](https://img.shields.io/github/downloads/catalingrigoriev285/openclip/total?style=flat-square&color=30d158)](https://github.com/catalingrigoriev285/openclip/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/catalingrigoriev285/openclip/release.yml?style=flat-square&label=build)](https://github.com/catalingrigoriev285/openclip/actions/workflows/release.yml)
[![License](https://img.shields.io/github/license/catalingrigoriev285/openclip?style=flat-square&color=8e8e93)](LICENSE)

[![Download](https://img.shields.io/badge/Download-latest%20release-0a84ff?style=for-the-badge&logo=github&logoColor=white)](https://github.com/catalingrigoriev285/openclip/releases/latest)

Windows · Linux · macOS — no installer, no runtime dependencies

</div>

![openclip — a self-contained screen recorder](docs/assets/945shots_so.png)

A small, self-contained screen recorder written in Rust with an [egui](https://github.com/emilk/egui) GUI.

- Records a **whole monitor**, a **single window**, or a **dragged region**
- **Game recording** (Windows, 64-bit games): openclip's hook takes frames from the game's own back buffer — so it works in exclusive fullscreen and costs the game less than desktop capture — and draws a frame-rate counter into the game's picture, **green** when armed and **red** while recording
- Captures **system audio** (what you hear) and/or a **microphone**
- **Live preview** of what is being recorded
- Writes standard **MP4** or **AVI** files that play in VLC, Windows Media Player / Movies & TV, Chrome/Edge and ffmpeg-based tools
- **GPU encoding** on Windows: H.264 and H.265/HEVC through NVIDIA NVENC, Intel Quick Sync or AMD AMF (Windows Media Foundation), plus Microsoft's software encoders; AAC audio the same way
- **No external runtime dependencies**: no ffmpeg, no codec packs. The bundled H.264 encoder ([OpenH264](https://www.openh264.org/)) and MP3 encoder ([LAME](https://lame.sourceforge.io/)) are compiled from source at build time, hardware/AAC encoders come from Media Foundation (part of Windows), and the MP4 and AVI muxers are in-house.

## Building

```sh
cargo build --release
./target/release/openclip
```

You need a Rust toolchain (edition 2024 → Rust 1.85+) and a C/C++ compiler for the bundled codecs:

| Platform | Requirements |
|---|---|
| Windows | Visual Studio Build Tools (MSVC). Nothing else. |
| macOS | Xcode command-line tools, plus `autoconf automake libtool` (LAME builds via autotools). macOS 13+ for capture; system-audio loopback needs macOS 14.6+. |
| Linux | `build-essential autoconf automake libtool` and the dev headers used by eframe / xcap / cpal: `libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev libxcb1-dev libxrandr-dev libdbus-1-dev libpipewire-0.3-dev libasound2-dev libegl1-mesa-dev libgl1-mesa-dev libwayland-dev libxkbcommon-x11-dev libx11-dev`. X11 only for now (see limitations). |

Installing [nasm](https://www.nasm.us/) before building enables OpenH264's assembly kernels and roughly doubles encoder throughput; without it the build silently falls back to C.

### Releasing

Pushing a `v*` tag that matches the version in `Cargo.toml` runs the [release workflow](.github/workflows/release.yml), which builds the executable for Windows, Linux and macOS (with nasm) and publishes a GitHub Release with a `.zip` / `.tar.gz` per platform:

```sh
# bump version in Cargo.toml, then refresh Cargo.lock and commit both
cargo update --workspace
git commit -am "Release v0.2.0"
git tag v0.2.0 && git push origin main v0.2.0
```

Running the workflow manually (Actions → Release → Run workflow) only builds and uploads the archives as artifacts, which is handy for checking a build before tagging.

The in-app updater relies on this layout: it looks for the asset named `openclip-<version>-<target>.<zip|tar.gz>` on the latest release and takes `openclip[.exe]` — and, on Windows, `openclip_hook64.dll` — out of the folder inside. The DLL is replaced first and a failure there abandons the update, because the two share a compiled-in ABI version and must never end up from different builds. An archive from before the hook existed simply has no DLL and still updates cleanly. `cargo run --example check_update` prints what the updater sees; with `OPENCLIP_UPDATE_PRETEND_VERSION=0.1.0` and `-- --install` it runs the whole download → verify → replace path on the example's own executable.

## Usage

The window is laid out like a classic recorder (about 800×640): a toolbar on top, a navigation on the left (Home / General / Video / Image / About) and pages on the right.

1. **Toolbar** – pick a recording mode (**Region** opens the on-screen selector: drag a rectangle, Esc cancels; **Monitor**; **Window**), toggle **system audio**, **microphone** and **cursor**, then press the round **REC** button. A **3-2-1 countdown** runs first (big number in the window / mini bar, never in the video; Esc or REC cancels; length and on/off under General → Recording). **⏸** pauses and resumes (paused time is cut out of the file, video and audio stay in sync); the camera button saves a PNG snapshot.
2. **Home** – a file browser for your output folder with **Videos / Images / Audios** tabs (newest first, with sizes; double-click or **Play** opens the file, **Folder** reveals it, **Delete** asks for confirmation). The **Preview** tab shows a live view of exactly what will be recorded — cursor and mouse effects included — together with the monitor/window/region selector; the preview capture only runs while that tab is open. The status strip's **Change…** button jumps there.
3. **Video** has two tabs: **Record** (cursor / click / highlight toggles, system audio, microphone and device, and the format summary boxes with a **Settings** button that opens the Format settings dialog below) and **Mouse** (mouse effects, below). **General** sets the output folder and file-name prefix.
4. **Mini bar** – the ▭ toolbar button collapses the window into a small always-on-top bar with the recording area, the input toggles, a ⚙ button for the Format settings, pause / REC and a restore button; drag it by its background to move it out of the recorded area.
5. Press **REC** again (it turns into a stop button) to finish. Files are written as `<prefix>-YYYYMMDD-HHMMSS.mp4` (or `.avi`) into the chosen folder (defaults to `~/Videos`).

The status strip shows elapsed (recorded) time, encoded fps, dropped frames and file size while recording, plus a note when an encoder had to be substituted. Video runs on a fixed frame clock: every slot of `1/fps` gets exactly one frame — the newest captured one, or a repeat of the previous frame when the screen did not change or the capture was late — so the file always has a perfectly regular frame rate and audio stays in sync. Only if encoding falls more than two frames behind are slots skipped (counted as "dropped").

All settings are remembered between runs in **`settings.json` next to the executable** (portable). If that folder is not writable (e.g. `Program Files`), the per-user location is used instead: `%APPDATA%\openclip\` on Windows, `~/.config/openclip/` on Linux, `~/Library/Application Support/openclip/` on macOS. A settings file from that per-user location is picked up automatically the first time. General → Sources shows the file in use.

### Updates

At start-up openclip asks GitHub once whether a newer release exists (a single unauthenticated request; nothing is installed without asking — turn it off under **General → Updates**, or check by hand from **About**). When there is one, an **Update to vX** button appears in the status strip; the dialog shows the release notes and offers **Download and install**: the archive for your platform is downloaded next to the executable, verified against the SHA-256 that GitHub publishes for the asset, the binary is swapped in place (`settings.json` and your recordings are untouched) and **Restart now** launches the new version. If the program folder is not writable (e.g. `Program Files`) or there is no build for your platform, the dialog only opens the release page.

### Format settings

The **Settings** button on the Video → Record page (and the ⚙ button on the mini bar) opens a dialog laid out like classic recorders:

- **File Type** – **MP4** or **AVI**. AVI is the crash-tolerant, "plays anywhere" option and the only one that can hold PCM audio; HEVC is written to MP4 only.
- **Size** – Full Size, Half Size, a preset (1920×1080, 1280×720, 854×480, 640×360 — fitted inside, aspect kept, never upscaled) or a custom **W% × H%**.
- **FPS** – 10 … 120 or a custom value.
- **Codec** – **Auto** (default: the first hardware H.264 encoder, else OpenH264), `H264 (OpenH264, CPU)`, and on Windows every Media Foundation encoder found on the machine, e.g. `H264 (NVIDIA® NVENC)`, `H264 (Intel® Quick Sync)`, `H264 (AMD AMF/VCE)`, `H264 (Microsoft software)`, `H265/HEVC (NVIDIA® NVENC)`, … The **…** button shows the encoder's details and can rescan. If the chosen encoder cannot start (driver gone, unsupported size), the next encoder of the same family is used, then OpenH264 — the status strip tells you. OpenH264 at 1080p needs roughly 35 ms per frame on a laptop CPU, so prefer a hardware encoder (or Half Size) for smooth 1080p30.
- **Quality** – 100 … 10; the bitrate is derived from the quality, the output size and the frame rate (≈ 10 Mbps for 1080p30 at 80). The **…** button switches to a constant bitrate and sets the keyframe interval.
- **Profile** – Auto / Baseline / Main / High for H.264, Auto / Main for HEVC.
- **Audio** – **MP3** (bundled LAME), **AAC** (Windows Media Foundation) or **PCM** (AVI only); bitrate (MP3 64–320 kbps, AAC 96/128/160/192 kbps), Mono/Stereo, 44100 or 48000 Hz.

On laptops with hybrid graphics the executable asks Windows to run it on the discrete GPU (it exports `NvOptimusEnablement` / `AmdPowerXpressRequestHighPerformance`), which is what lets the NVIDIA / AMD encoders activate. `cargo run --example list_encoders` prints every encoder Media Foundation offers and whether it activates.

### Mouse effects

The **Video → Mouse** tab mirrors the classic "mouse effects" panel:

- **Show mouse cursor** with a **Size** (50–300 %). At 100 % the real cursor is captured natively (exact shape); at any other size the app hides the native cursor and draws its own scalable arrow.
- **Click effect** – an expanding ring on every press, with separate **left / right click colours** and a size.
- **Highlight effect** – a translucent halo that follows the pointer, with colour, size and opacity.
- A checkerboard **preview** shows the current settings; click inside it to test the click effect.

Effects are painted onto the captured frames themselves (in the encode thread, and in the live preview), so the recording and the preview always match. The global pointer is read through `device_query`; on Linux this needs X11 (`libx11-dev` at build time).

### Headless examples

```sh
cargo run --release --example capture_to_mp4 -- 10 out.mp4      # record the primary monitor for 10 s
    # flags: --half --mic --no-audio --fx --region X,Y,W,H --window TITLE --pause-at S --resume-at S
    #        --codec openh264|h264-hw|h264-sw|hevc|nvenc|quick|… --audio mp3|aac|pcm --avi --fps N --quality Q
cargo run --release --example bench_encode -- 1920 1080 5 out.mp4 --codec nvenc   # encoder throughput on synthetic content
cargo run --example list_encoders                                 # what Media Foundation offers on this machine
cargo run --example probe_media -- out.mp4 out.avi                # decode files with Windows' own demuxers/decoders
```

## How it works

```
capture backend ──RawFrame──▶ scale ──▶ I420 / NV12 ──▶ OpenH264 | Media Foundation (NVENC / QSV / AMF / software) ──▶ MP4 | AVI muxer ──▶ file
                                                                                                                             ▲
cpal (mic / loopback) ──▶ mixer ──▶ LAME MP3 | MF AAC | PCM ────────────────────────────────────────────────────────────────┘
```

- **Capture** (`src/capture`): Windows uses Windows.Graphics.Capture via `windows-capture` (BGRA frames, GPU-side crop for regions). The GPU→CPU readback is double-buffered — each frame is copied into one staging texture while the previous one is mapped — and lands in pooled buffers, so a 1080p recording does not allocate per frame. macOS and Linux/X11 use `xcap`'s video recorder for monitors and screenshot polling for windows. Monitor/window enumeration and the region-picker backdrop use `xcap` everywhere.
- **Pipeline** (`src/pipeline.rs`): a fixed-cadence frame clock assigns captured frames to `1/fps` slots (newest wins, missing ones repeat the last frame), paints mouse effects in place (saving and restoring only the touched pixels), converts, encodes and muxes on one thread; previews are only produced while the Preview tab is visible.
- **Video** (`src/video`): frames are scaled (`fast_image_resize`) to the chosen size, converted to I420 or NV12 with the SIMD `yuv` crate and encoded by OpenH264 (screen-content real-time mode, low complexity, 2–4 threads) or by a Media Foundation transform (`src/video/mf`: enumeration, a sync/async transform session, NV12 upload from system memory, no B-frames so presentation order equals decode order). Encoders hand out Annex-B; parameter sets are harvested from the first keyframe.
- **Audio** (`src/audio`): each cpal stream pushes timestamped chunks; the mixer places them on the recording timeline (inserting silence for gaps such as WASAPI loopback going quiet), resamples to the chosen rate and feeds the encoder: LAME (one 1152-sample MP3 frame per container sample), the Media Foundation AAC encoder (1024-sample access units, AudioSpecificConfig from the transform) or 16-bit PCM.
- **Mux** (`src/mux`): a streaming, non-fragmented MP4 writer (`ftyp` / 64-bit `mdat` / `moov`, `avc1` or `hvc1`, `esds` for MP3/AAC) with per-sample durations from real timestamps, keyframe table, 0.5 s interleaved chunks and `co64` for files over 4 GiB; and an OpenDML AVI writer (`hdrl` with super-indexes, ≤ 1 GiB `RIFF` chunks with `AVIX` continuations, standard `ix##` indexes, legacy `idx1`, empty chunks for dropped frame slots). Integration tests round-trip both through independent readers.
- **Settings** (`src/settings.rs`): the format settings, output folder, prefix, input toggles and mouse effects, saved as JSON.

## Limitations (v1)

- **Wayland** is not supported (xcap limitation); use an X11 session.
- **Window capture on macOS/Linux** polls screenshots and is slower than monitor capture; on Windows it is native.
- **System audio on Linux** has no loopback API in cpal; select a PulseAudio/PipeWire monitor source as the input device instead.
- **Hardware encoders, HEVC and AAC are Windows-only** (Media Foundation). macOS and Linux record with OpenH264 + MP3/PCM. HEVC is written to MP4 only.
- Microsoft's **DX12 encoder** transforms are listed by Windows but need a D3D12 device manager; they are skipped during enumeration.
- If a captured **window** is resized mid-recording the encoder is not re-negotiated — the file keeps the size it started at. The new picture is fitted into that frame with its aspect ratio preserved, so a shape change shows up as black bars rather than a stretched picture or a failed recording; the status strip says so. A **region** may be moved freely while recording — only resizing it is disabled once recording starts.
- Without `nasm`, OpenH264 manages roughly 25–30 fps at 1080p on high-entropy content on a modern desktop CPU; typical desktop content is much cheaper. Use a hardware encoder, Half Size or a lower frame rate if you see drops.

## Licensing notes

The code is Apache-2.0. Bundled third-party encoders: OpenH264 (BSD-2-Clause) and LAME (LGPL-2.1+). UI icons are Font Awesome Free (SIL OFL 1.1 font, CC BY 4.0 icons — `assets/fonts/FONT-AWESOME-LICENSE.txt`). Note that Cisco's MPEG-LA H.264 patent coverage applies only to the binary they distribute, not to source builds like this one; whether that matters depends on your jurisdiction and use. Media Foundation encoders are provided by Windows / your GPU driver under their own terms.
