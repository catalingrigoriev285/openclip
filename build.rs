fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/favicon.ico");

    // Embed the application icon into the Windows executable so it shows in
    // Explorer, the taskbar and the title bar. Other platforms set the window
    // icon at runtime (see `src/main.rs`).
    #[cfg(windows)]
    {
        // Export the hybrid-graphics hints defined in `src/lib.rs` from every
        // executable so laptops run openclip (and its hardware encoders) on the
        // discrete GPU.
        for sym in ["NvOptimusEnablement", "AmdPowerXpressRequestHighPerformance"] {
            println!("cargo:rustc-link-arg-bins=/EXPORT:{sym}");
            println!("cargo:rustc-link-arg-examples=/EXPORT:{sym}");
        }

        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/favicon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon resource: {e}");
        }
    }
}
