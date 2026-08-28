//! Lists the video encoders openclip can use on this machine and, on Windows,
//! every Media Foundation encoder transform with its activation result.
//!
//! Usage: cargo run --example list_encoders

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    println!("openclip encoders:");
    println!("  H264 (OpenH264, CPU)  [bundled]");
    for e in openclip::video::refresh_encoders() {
        println!(
            "  {:<34} vendor {:<9} hw {:<5} clsid {}  \"{}\"",
            e.label,
            e.vendor.label(),
            e.hardware,
            e.clsid,
            e.friendly_name
        );
    }
    #[cfg(windows)]
    windows_details();
}

#[cfg(windows)]
fn windows_details() {
    use openclip::video::mf::{attr_string, enumerate_activates, ComGuard};
    use windows::core::Interface;
    use windows::Win32::Media::MediaFoundation::{
        IMFAttributes, IMFTransform, MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
        MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_URL_Attribute,
        MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_HEVC,
    };

    let _com = ComGuard::new();
    openclip::video::mf::startup().unwrap();
    println!("\nMedia Foundation video encoder transforms:");
    for (name, subtype) in [("H264", MFVideoFormat_H264), ("HEVC", MFVideoFormat_HEVC)] {
        for (flag_name, flags) in [
            ("HARDWARE", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER),
            ("HARDWARE|ASYNC", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER),
            ("SYNC", MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER),
            ("ASYNC", MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER),
        ] {
            let acts = match enumerate_activates(MFT_CATEGORY_VIDEO_ENCODER, flags, MFMediaType_Video, subtype) {
                Ok(a) => a,
                Err(e) => {
                    println!("  {name} [{flag_name}]: enumeration failed: {e:#}");
                    continue;
                }
            };
            println!("  {name} [{flag_name}]: {} transform(s)", acts.len());
            for a in &acts {
                let attrs: IMFAttributes = a.cast().unwrap();
                let friendly = attr_string(&attrs, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
                let vendor = attr_string(&attrs, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).unwrap_or_default();
                let url = attr_string(&attrs, &MFT_ENUM_HARDWARE_URL_Attribute).unwrap_or_default();
                let activation = match unsafe { a.ActivateObject::<IMFTransform>() } {
                    Ok(_) => "activates OK".to_string(),
                    Err(e) => format!("ActivateObject failed: {e}"),
                };
                println!("      \"{friendly}\" vendor={vendor} url={url}\n         → {activation}");
            }
        }
    }
}
