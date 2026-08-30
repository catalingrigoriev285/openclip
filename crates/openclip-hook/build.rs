fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // A full version resource is part of the safety posture, not decoration:
    // this DLL is loaded into other people's games, and anyone inspecting the
    // process should be able to see at a glance what it is, who ships it and
    // which build it came from. See the game-capture notes in README.md.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "openclip");
        res.set("FileDescription", "openclip in-game capture hook");
        res.set("OriginalFilename", "openclip_hook64.dll");
        res.set("InternalName", "openclip_hook64");
        res.set("CompanyName", "openclip");
        res.set("LegalCopyright", "Apache-2.0");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed the hook version resource: {e}");
        }
    }
}
