//! The shared-memory contract between openclip and the injected game hook.
//!
//! Both sides compile *this* module, so the layout is structurally identical by
//! construction rather than by promise. [`HOOK_ABI_VERSION`] only has to catch
//! the case where the DLL on disk came from a different openclip build — a
//! half-finished self-update, or a portable install someone copied a new exe
//! into. On mismatch the hook writes [`HookError::AbiMismatch`] and hooks
//! nothing, which is much better than reading a struct with the wrong shape out
//! of someone's game.
//!
//! Nothing here depends on the `windows` crate: handles are `u64` and names are
//! ASCII byte arrays, so the layout tests run on every platform.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `"OCHK"`, so a mapping that is not ours is never parsed as one.
pub const HOOK_MAGIC: u32 = u32::from_le_bytes(*b"OCHK");
/// Bump on **any** change to [`Control`]'s layout or field meanings.
pub const HOOK_ABI_VERSION: u32 = 1;
/// Shared textures are double-buffered: the hook writes one while openclip
/// reads the other, so a slow readback never stalls a game's present.
pub const TEX_SLOTS: usize = 2;
/// Enough for `Local\openclip.hook.<pid>.<pid>.tex.<slot>.<gen>` with room spare.
pub const NAME_MAX: usize = 128;
/// Size of [`Control`], asserted below. Changing it means bumping the version.
pub const CONTROL_SIZE: usize = 768;

/// Which graphics API the hook attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GfxApi {
    Unknown = 0,
    D3D11 = 1,
    D3D12 = 2,
    OpenGl = 3,
    Vulkan = 4,
}

impl GfxApi {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => GfxApi::D3D11,
            2 => GfxApi::D3D12,
            3 => GfxApi::OpenGl,
            4 => GfxApi::Vulkan,
            _ => GfxApi::Unknown,
        }
    }

    /// The label shown in the UI. Not translated: these are API names.
    pub fn label(self) -> &'static str {
        match self {
            GfxApi::Unknown => "—",
            GfxApi::D3D11 => "Direct3D 11",
            GfxApi::D3D12 => "Direct3D 12",
            GfxApi::OpenGl => "OpenGL",
            GfxApi::Vulkan => "Vulkan",
        }
    }
}

/// How frames cross the process boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Transport {
    /// Nothing is being published yet.
    None = 0,
    /// A keyed-mutex shared texture: the frame never leaves the GPU.
    SharedTexture = 1,
    /// A shared-memory ring. The slow path, used where an API cannot hand us a
    /// shareable texture; reported to the user because it costs real frame time.
    SharedMemory = 2,
}

impl Transport {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Transport::SharedTexture,
            2 => Transport::SharedMemory,
            _ => Transport::None,
        }
    }
}

/// Why the hook gave up. Each maps to one localized message on the openclip side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HookError {
    None = 0,
    /// The DLL and the exe disagree about [`Control`] — see the module docs.
    AbiMismatch = 1,
    /// The function the vtable slot pointed at was not in the module it should
    /// have been in, so it was left alone rather than patched blindly.
    VtableUnexpected = 2,
    /// No device could be obtained from the swapchain.
    NoDevice = 3,
    /// The back buffer is in a format the pipeline cannot take yet (HDR/10-bit).
    FormatUnsupported = 4,
    /// The shared texture could not be created or opened.
    ShareFailed = 5,
    /// Repeated panics inside the hook; it reverted itself and went inert.
    SelfDisarmed = 6,
}

impl HookError {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => HookError::AbiMismatch,
            2 => HookError::VtableUnexpected,
            3 => HookError::NoDevice,
            4 => HookError::FormatUnsupported,
            5 => HookError::ShareFailed,
            6 => HookError::SelfDisarmed,
            _ => HookError::None,
        }
    }
}

/// The counter's appearance, packed into one `u64`.
///
/// One atomic store means the hook can never observe a half-applied settings
/// change mid-present — the same trick `capture::LiveRect` uses for a region
/// being dragged while recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlaySettings {
    pub enabled: bool,
    /// 0..=3, matching [`crate::layout::Corner::ALL`].
    pub corner: u8,
    /// Percent of natural size, clamped to 10..=400.
    pub size: u16,
    /// Percent, clamped to 0..=100.
    pub opacity: u8,
    /// Draw into the frame openclip records as well as the one on screen.
    pub burn_in: bool,
}

impl OverlaySettings {
    pub fn pack(self) -> u64 {
        let size = self.size.clamp(10, 400) as u64;
        let opacity = self.opacity.min(100) as u64;
        (self.enabled as u64) | ((self.corner as u64 & 0b11) << 1) | (size << 3) | (opacity << 13) | ((self.burn_in as u64) << 20)
    }

    pub fn unpack(v: u64) -> Self {
        Self {
            enabled: v & 1 != 0,
            corner: ((v >> 1) & 0b11) as u8,
            size: (((v >> 3) & 0x3ff) as u16).clamp(10, 400),
            opacity: (((v >> 13) & 0x7f) as u8).min(100),
            burn_in: (v >> 20) & 1 != 0,
        }
    }
}

/// The shared control block, mapped into both processes.
///
/// Field ownership is strict and one-way — the comments below say who writes
/// what. Nothing here is a lock: the hook runs on a game's render thread and
/// must never block on openclip.
#[repr(C, align(64))]
pub struct Control {
    // ----- header: written by openclip before injection, read-only after -----
    pub magic: u32,
    pub abi_version: u32,
    pub struct_size: u32,
    pub host_pid: u32,
    pub target_pid: u32,
    _pad_hwnd: u32,
    pub target_hwnd: u64,
    /// `QueryPerformanceFrequency`, so the hook's timestamps can be mapped onto
    /// the recording timeline without each side sampling it separately.
    pub qpc_freq: i64,

    // ----- openclip -> hook -----
    /// Draw the counter (green). Set when Game mode is armed.
    pub armed: AtomicU32,
    /// Also publish frames, and draw the counter red.
    pub capturing: AtomicU32,
    /// Unhook and go inert.
    pub stop: AtomicU32,
    /// Publish decimation target; the game may present far faster.
    pub capture_fps: AtomicU32,
    /// [`OverlaySettings::pack`].
    pub overlay: AtomicU64,

    // ----- hook -> openclip -----
    /// `major << 16 | minor << 8 | patch`. Non-zero means the hook is alive.
    pub hook_version: AtomicU32,
    /// [`GfxApi`].
    pub api: AtomicU32,
    /// [`Transport`].
    pub transport: AtomicU32,
    /// [`HookError`].
    pub error_code: AtomicU32,
    /// The adapter the shared texture lives on. openclip must open it on the
    /// same one — on a hybrid laptop the game is on the dGPU and openclip may
    /// not be, and a cross-adapter shared texture simply does not open.
    pub adapter_luid: AtomicU64,
    /// Every present, including the ones not published.
    pub present_count: AtomicU64,
    /// The game's own frame rate ×1000; this is what the counter shows.
    pub present_fps_milli: AtomicU32,
    _pad_beat: u32,
    /// Bumped every present. A stale value means the game hung or exited.
    pub heartbeat_qpc: AtomicU64,

    // ----- frame publication -----
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// The `DXGI_FORMAT` of the shared texture.
    pub dxgi_format: AtomicU32,
    /// 1 when rows run bottom-up (OpenGL).
    pub flip_y: AtomicU32,
    /// Bumped whenever the textures are recreated (resize, format or swapchain
    /// change). openclip reopens by name when it moves.
    pub generation: AtomicU64,
    /// Bumped after a slot is filled.
    pub frame_seq: AtomicU64,
    /// Which slot `frame_seq` refers to.
    pub slot: AtomicU32,
    _pad_slot: u32,
    /// Present time per slot, in `QueryPerformanceCounter` ticks.
    pub qpc: [AtomicU64; TEX_SLOTS],
    /// NUL-terminated ASCII names of the shared textures.
    pub tex_name: [[u8; NAME_MAX]; TEX_SLOTS],
    /// NUL-terminated ASCII name of the shared-memory fallback mapping.
    pub shm_name: [u8; NAME_MAX],
    pub shm_stride: AtomicU32,
    pub shm_bytes: AtomicU32,
    /// NUL-terminated ASCII detail for [`HookError`], written once.
    pub error_text: [u8; 160],
}

const _: () = assert!(size_of::<Control>() == CONTROL_SIZE);
const _: () = assert!(align_of::<Control>() == 64);

impl Control {
    /// Whether this mapping is ours and speaks our version of the contract.
    pub fn is_compatible(&self) -> bool {
        self.magic == HOOK_MAGIC
            && self.abi_version == HOOK_ABI_VERSION
            && self.struct_size as usize == size_of::<Control>()
    }

    pub fn overlay_settings(&self) -> OverlaySettings {
        OverlaySettings::unpack(self.overlay.load(Ordering::Relaxed))
    }

    pub fn set_overlay_settings(&self, s: OverlaySettings) {
        self.overlay.store(s.pack(), Ordering::Relaxed);
    }

    pub fn api(&self) -> GfxApi {
        GfxApi::from_u32(self.api.load(Ordering::Relaxed))
    }

    pub fn transport(&self) -> Transport {
        Transport::from_u32(self.transport.load(Ordering::Relaxed))
    }

    pub fn error(&self) -> HookError {
        HookError::from_u32(self.error_code.load(Ordering::Relaxed))
    }

    /// The game's present rate, for the counter and the status strip.
    pub fn present_fps(&self) -> f32 {
        self.present_fps_milli.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn error_detail(&self) -> Option<&str> {
        read_cstr(&self.error_text)
    }

    pub fn tex_name_at(&self, slot: usize) -> Option<&str> {
        self.tex_name.get(slot).and_then(|n| read_cstr(n))
    }

    pub fn shm(&self) -> Option<&str> {
        read_cstr(&self.shm_name)
    }
}

// The four `DXGI_FORMAT` values the recording pipeline can take, as raw numbers
// because this module deliberately does not depend on the `windows` crate — it
// has to compile and be tested on every platform. openclip's own
// `capture::windows::pixel_format` maps the same set through the typed
// constants, and a test there asserts the two agree.
const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 28;
const DXGI_FORMAT_R8G8B8A8_UNORM_SRGB: u32 = 29;
const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
const DXGI_FORMAT_B8G8R8A8_UNORM_SRGB: u32 = 91;

/// Whether a back buffer in this format can be recorded.
///
/// HDR and 10-bit surfaces (`R10G10B10A2_UNORM`, `R16G16B16A16_FLOAT`) are
/// common in recent titles and are **not** in this set: `crate::video::PixelFormat`
/// has no 16-bit variant, so recording one would mean silently mangling its
/// colour. The hook refuses instead and says so.
pub fn format_supported(dxgi_format: u32) -> bool {
    matches!(
        dxgi_format,
        DXGI_FORMAT_R8G8B8A8_UNORM
            | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
            | DXGI_FORMAT_B8G8R8A8_UNORM
            | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
    )
}

/// Writes `text` into a fixed NUL-terminated ASCII field, truncating to fit.
pub fn write_cstr(dst: &mut [u8], text: &str) {
    dst.fill(0);
    let room = dst.len().saturating_sub(1);
    let bytes = text.as_bytes();
    // Truncate on a character boundary so the result is still valid UTF-8.
    let mut end = bytes.len().min(room);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    dst[..end].copy_from_slice(&bytes[..end]);
}

fn read_cstr(src: &[u8]) -> Option<&str> {
    let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    (end > 0).then(|| core::str::from_utf8(&src[..end]).ok()).flatten()
}

// ----- named objects ---------------------------------------------------------

// Every name is keyed on the **target** pid alone, because that is the only
// number the hook knows about itself. `SetWindowsHookEx` carries no payload, so
// there is no way to tell the DLL which openclip started it; it finds its
// mapping by its own process id and reads [`Control::host_pid`] from inside.
// Two openclip instances therefore share one hook per game rather than
// colliding — `host_pid` is what lets the second one notice and stand down.

/// The shared control block.
pub fn control_name(target_pid: u32) -> String {
    format!("Local\\openclip.hook.{target_pid}.ctl")
}

/// Signalled by the hook when a slot has been filled.
pub fn ready_event_name(target_pid: u32) -> String {
    format!("Local\\openclip.hook.{target_pid}.ready")
}

/// Signalled by openclip to ask the hook to unhook.
pub fn stop_event_name(target_pid: u32) -> String {
    format!("Local\\openclip.hook.{target_pid}.stop")
}

/// Created by the hook; its existence means the DLL is already live in that
/// process, so a second injection would be pointless.
pub fn instance_mutex_name(target_pid: u32) -> String {
    format!("Local\\openclip.hook.{target_pid}.instance")
}

/// The NT name of a shared texture. `generation` changes on every recreate, so
/// a stale name can never be reopened by accident.
pub fn texture_name(target_pid: u32, slot: usize, generation: u64) -> String {
    format!("Local\\openclip.hook.{target_pid}.tex.{slot}.{generation}")
}

/// The shared-memory fallback transport.
pub fn shmem_name(target_pid: u32, generation: u64) -> String {
    format!("Local\\openclip.hook.{target_pid}.shm.{generation}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_settings_round_trip() {
        let s = OverlaySettings { enabled: true, corner: 2, size: 150, opacity: 75, burn_in: true };
        assert_eq!(OverlaySettings::unpack(s.pack()), s);
        let off = OverlaySettings { enabled: false, corner: 0, size: 10, opacity: 0, burn_in: false };
        assert_eq!(OverlaySettings::unpack(off.pack()), off);
        // Out-of-range values are clamped rather than wrapping into another field.
        let wild = OverlaySettings { enabled: true, corner: 3, size: 4000, opacity: 250, burn_in: false };
        let back = OverlaySettings::unpack(wild.pack());
        assert_eq!((back.size, back.opacity, back.corner), (400, 100, 3));
    }

    #[test]
    fn every_corner_survives_packing() {
        for corner in 0..4u8 {
            let s = OverlaySettings { enabled: true, corner, size: 100, opacity: 100, burn_in: false };
            assert_eq!(OverlaySettings::unpack(s.pack()).corner, corner);
        }
    }

    #[test]
    fn cstr_fields_round_trip_and_truncate() {
        let mut buf = [0u8; 16];
        write_cstr(&mut buf, "hello");
        assert_eq!(read_cstr(&buf), Some("hello"));
        // Always NUL-terminated, even when the text does not fit.
        write_cstr(&mut buf, "0123456789abcdefghij");
        assert_eq!(read_cstr(&buf).unwrap().len(), 15);
        assert_eq!(buf[15], 0);
        // Multi-byte characters are cut on a boundary, never mid-sequence.
        write_cstr(&mut buf, "ăăăăăăăăăă");
        assert!(read_cstr(&buf).is_some());
        // An empty field reads as absent, not as "".
        write_cstr(&mut buf, "");
        assert_eq!(read_cstr(&buf), None);
    }

    #[test]
    fn names_are_distinct_and_fit_the_field() {
        let names = [
            control_name(20),
            ready_event_name(20),
            stop_event_name(20),
            instance_mutex_name(20),
            texture_name(20, 0, 7),
            texture_name(20, 1, 7),
            shmem_name(20, 7),
        ];
        for n in &names {
            assert!(n.len() < NAME_MAX, "{n} must fit a NAME_MAX field");
            assert!(n.starts_with("Local\\openclip.hook."));
        }
        let mut sorted = names.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "names must not collide");
        // Different games never share a name...
        assert_ne!(control_name(20), control_name(21));
        // ...and a regenerated texture never reuses the old one.
        assert_ne!(texture_name(20, 0, 7), texture_name(20, 0, 8));
        assert_ne!(texture_name(20, 0, 7), texture_name(20, 1, 7));
        // A pid long enough to be implausible still fits the field.
        assert!(control_name(u32::MAX).len() < NAME_MAX);
    }

    #[test]
    fn only_eight_bit_bgra_and_rgba_are_recordable() {
        for f in [28, 29, 87, 91] {
            assert!(format_supported(f), "format {f} should be recordable");
        }
        // R10G10B10A2_UNORM and R16G16B16A16_FLOAT — the common HDR back buffers.
        for f in [24, 10, 0] {
            assert!(!format_supported(f), "format {f} should be refused");
        }
    }

    #[test]
    fn enums_round_trip_through_their_wire_values() {
        for a in [GfxApi::Unknown, GfxApi::D3D11, GfxApi::D3D12, GfxApi::OpenGl, GfxApi::Vulkan] {
            assert_eq!(GfxApi::from_u32(a as u32), a);
        }
        for t in [Transport::None, Transport::SharedTexture, Transport::SharedMemory] {
            assert_eq!(Transport::from_u32(t as u32), t);
        }
        for e in [HookError::None, HookError::AbiMismatch, HookError::VtableUnexpected, HookError::SelfDisarmed] {
            assert_eq!(HookError::from_u32(e as u32), e);
        }
        // An unknown wire value degrades instead of panicking.
        assert_eq!(GfxApi::from_u32(99), GfxApi::Unknown);
        assert_eq!(HookError::from_u32(99), HookError::None);
    }
}
