//! Windows Media Foundation encoders: hardware H.264 / HEVC (NVENC, AMF,
//! Quick Sync, Microsoft's DX12 encoders), Microsoft's software H.264, and
//! (in `audio::mf_aac`) AAC. Media Foundation ships with Windows, so this
//! keeps the "no external runtime dependencies" promise while giving GPU
//! encoding.

pub mod transform;
pub mod video;

use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_BOOL};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, MFStartup, MFTEnumEx, MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_URL_Attribute, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFT_TRANSFORM_CLSID_Attribute, MFSTARTUP_NOSOCKET,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_I8, VT_UI4};

use super::encoder::{EncoderInfo, Vendor};

pub use transform::MftSession;

/// Initializes COM (multithreaded) for the current thread for as long as the
/// guard lives. Threads that already run an STA (the GUI thread) are left alone.
pub struct ComGuard {
    initialized: bool,
}

impl ComGuard {
    pub fn new() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK / S_FALSE: we own a reference. RPC_E_CHANGED_MODE: already
        // initialized in another mode, still usable, nothing to release.
        let initialized = hr.is_ok();
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            log::warn!("CoInitializeEx failed: {hr:?}");
        }
        Self { initialized }
    }
}

impl Default for ComGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

/// Starts Media Foundation once per process (never shut down).
pub fn startup() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!("MFStartup failed: {e}"))
}

pub fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

pub fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: VARIANT_BOOL(if v { -1 } else { 0 }) },
            }),
        },
    }
}

/// A `VT_I8` property variant, which is what `IMFSourceReader::SetCurrentPosition`
/// wants for a seek target (in 100 ns units). `VT_I8` owns no allocation, so the
/// value can simply be dropped — no `PropVariantClear` needed.
pub fn propvariant_i64(v: i64) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: v },
            }),
        },
    }
}

/// Reads a string attribute (e.g. the friendly name) from an MF attribute store.
pub fn attr_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut ptr = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        attrs.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
        let s = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as *const _));
        s
    }
}

/// Enumerates transforms of `category` producing `subtype`, in MF's preferred order.
pub fn enumerate_activates(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    major: GUID,
    subtype: GUID,
) -> Result<Vec<IMFActivate>> {
    let out_type = MFT_REGISTER_TYPE_INFO { guidMajorType: major, guidSubtype: subtype };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(category, flags, None, Some(&out_type), &mut ptr, &mut count).context("MFTEnumEx")?;
        let mut out = Vec::with_capacity(count as usize);
        if !ptr.is_null() {
            for i in 0..count as usize {
                // Take ownership of each entry, then free the array itself.
                if let Some(a) = std::ptr::read(ptr.add(i)) {
                    out.push(a);
                }
            }
            CoTaskMemFree(Some(ptr as *const _));
        }
        Ok(out)
    }
}

fn hardware_flags() -> MFT_ENUM_FLAG {
    // HARDWARE alone hides Microsoft's DX12 GPU encoders; ASYNCMFT includes them.
    MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
}

fn software_flags() -> MFT_ENUM_FLAG {
    MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
}

fn vendor_from(name: &str, vendor_id: Option<&str>) -> Vendor {
    let n = name.to_ascii_lowercase();
    if n.contains("nvidia") {
        Vendor::Nvidia
    } else if n.contains("amd") || n.contains("advanced micro") || n.contains("radeon") {
        Vendor::Amd
    } else if n.contains("intel") {
        Vendor::Intel
    } else if n.contains("microsoft") {
        Vendor::Microsoft
    } else {
        match vendor_id.map(|v| v.to_ascii_uppercase()) {
            Some(v) if v.contains("10DE") => Vendor::Nvidia,
            Some(v) if v.contains("1002") => Vendor::Amd,
            Some(v) if v.contains("8086") => Vendor::Intel,
            Some(v) if v.contains("1414") => Vendor::Microsoft,
            _ => Vendor::Other,
        }
    }
}

fn label_for(hevc: bool, vendor: Vendor, hardware: bool, friendly: &str) -> String {
    let family = if hevc { "H265/HEVC" } else { "H264" };
    let who = match (vendor, hardware) {
        (Vendor::Nvidia, true) => "NVIDIA® NVENC".to_string(),
        (Vendor::Amd, true) => "AMD AMF/VCE".to_string(),
        (Vendor::Intel, true) => "Intel® Quick Sync".to_string(),
        (Vendor::Microsoft, true) => "Microsoft DX12 GPU".to_string(),
        (Vendor::Microsoft, false) => "Microsoft software".to_string(),
        (_, true) => format!("{} hardware", friendly.trim()),
        (_, false) => format!("{} software", friendly.trim()),
    };
    format!("{family} ({who})")
}

/// Transform identity as stored in settings: the CLSID as 32 lowercase hex
/// digits, or — for transforms registered without one (Microsoft's DX12
/// encoders) — the hardware URL / friendly name.
pub fn activate_key(attrs: &IMFAttributes) -> Option<String> {
    if let Ok(g) = unsafe { attrs.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) } {
        return Some(format!("{:032x}", g.to_u128()));
    }
    let name = attr_string(attrs, &MFT_ENUM_HARDWARE_URL_Attribute)
        .or_else(|| attr_string(attrs, &MFT_FRIENDLY_NAME_Attribute))?;
    Some(format!("name:{name}"))
}

fn describe_activate(act: &IMFActivate, hevc: bool, hardware: bool) -> Option<(EncoderInfo, String)> {
    let attrs: IMFAttributes = act.cast().ok()?;
    let friendly = attr_string(&attrs, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "Unknown encoder".into());
    let vendor_id = attr_string(&attrs, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
    let url = attr_string(&attrs, &MFT_ENUM_HARDWARE_URL_Attribute).unwrap_or_default();
    let key = activate_key(&attrs)?;
    let mut vendor = vendor_from(&friendly, vendor_id.as_deref());
    if vendor == Vendor::Other && !hardware && vendor_id.is_none() {
        // Windows-shipped software transforms ("H264 Encoder MFT", "HEVCVideoExtensionEncoder").
        vendor = Vendor::Microsoft;
    }
    let info = EncoderInfo {
        hevc,
        label: label_for(hevc, vendor, hardware, &friendly),
        friendly_name: friendly.clone(),
        vendor,
        hardware,
        clsid: key,
    };
    Some((info, format!("{friendly}|{url}")))
}

fn enumerate() -> Vec<EncoderInfo> {
    let _com = ComGuard::new();
    if let Err(e) = startup() {
        log::warn!("{e:#}");
        return Vec::new();
    }
    let mut out: Vec<EncoderInfo> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (subtype, hevc) in [(MFVideoFormat_H264, false), (MFVideoFormat_HEVC, true)] {
        for (flags, hardware) in [(hardware_flags(), true), (software_flags(), false)] {
            let acts = match enumerate_activates(MFT_CATEGORY_VIDEO_ENCODER, flags, MFMediaType_Video, subtype) {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("enumerating {} encoders: {e:#}", if hevc { "HEVC" } else { "H.264" });
                    continue;
                }
            };
            for a in &acts {
                let Some((info, key)) = describe_activate(a, hevc, hardware) else { continue };
                // The same device is often registered twice (legacy + current CLSID).
                if seen.contains(&key) || out.iter().any(|e| e.clsid == info.clsid) {
                    continue;
                }
                seen.push(key);
                // Transforms that cannot even be activated / configured from
                // system memory (e.g. the DX12 encoders, which want a D3D12
                // device manager) would only ever fall back; leave them out.
                match probe(a, hevc) {
                    Ok(()) => out.push(info),
                    Err(e) => log::info!("skipping {}: {e:#}", info.label),
                }
            }
        }
    }
    // Disambiguate identical labels (e.g. two Intel GPUs) with the transform name.
    let labels: Vec<String> = out.iter().map(|e| e.label.clone()).collect();
    for e in &mut out {
        if labels.iter().filter(|l| **l == e.label).count() > 1 {
            e.label = format!("{} – {}", e.label, e.friendly_name);
        }
    }
    out
}

/// Activates the transform and negotiates a typical 720p30 output type, which
/// is exactly what recording will do; failures mean the encoder is unusable here.
fn probe(act: &IMFActivate, hevc: bool) -> Result<()> {
    let session = MftSession::from_activate(act)?;
    let t = video::output_media_type(hevc, 1280, 720, 30, 4_000_000, None)?;
    session.set_output_type(&t)
}

static CACHE: Mutex<Option<Vec<EncoderInfo>>> = Mutex::new(None);

/// Cached list of Media Foundation encoders on this machine.
pub fn available_encoders() -> Vec<EncoderInfo> {
    if let Some(list) = CACHE.lock().unwrap().as_ref() {
        return list.clone();
    }
    refresh_encoders()
}

pub fn refresh_encoders() -> Vec<EncoderInfo> {
    let list = enumerate();
    *CACHE.lock().unwrap() = Some(list.clone());
    list
}

/// Finds the activate object for an enumerated encoder again (the objects
/// themselves are thread-affine, so only the CLSID is cached).
pub(crate) fn activate_for(info: &EncoderInfo) -> Result<IMFActivate> {
    let subtype = if info.hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
    let flags = if info.hardware { hardware_flags() } else { software_flags() };
    let acts = enumerate_activates(MFT_CATEGORY_VIDEO_ENCODER, flags, MFMediaType_Video, subtype)?;
    acts.into_iter()
        .find(|a| a.cast::<IMFAttributes>().ok().and_then(|at| activate_key(&at)).as_deref() == Some(info.clsid.as_str()))
        .ok_or_else(|| anyhow!("{} is no longer available", info.label))
}
