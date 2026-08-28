//! Exercises the updater without the GUI.
//!
//! ```sh
//! cargo run --example check_update                     # print the latest release vs. this build
//! OPENCLIP_UPDATE_PRETEND_VERSION=0.1.0 cargo run --example check_update -- --install
//! ```
//!
//! `--install` downloads, verifies and extracts the release archive and then
//! replaces **this example's executable** (`target/…/examples/check_update.exe`)
//! with the released openclip binary — the full self-update path, minus the
//! relaunch. Run it again afterwards and you get the released GUI; `cargo run`
//! rebuilds the example.

use std::sync::atomic::Ordering;
use std::time::Duration;

use openclip::update::{self, Progress};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let install = std::env::args().any(|a| a == "--install");

    println!("running version: {}", update::local_version());
    println!("platform asset:  {}", update::TARGET_SUFFIX.unwrap_or("(none for this target)"));
    println!("exe dir writable: {}", update::install_dir_writable());
    let Some(release) = update::check()? else {
        println!("up to date");
        return Ok(());
    };
    println!("newer release:   {} — {}", release.version, release.html_url);
    match &release.asset {
        Some(a) => println!(
            "asset:           {} ({} bytes, sha256 {})",
            a.name,
            a.size,
            if a.sha256.is_some() { "published" } else { "missing" }
        ),
        None => println!("asset:           none for this platform"),
    }
    if !install {
        return Ok(());
    }

    let progress = std::sync::Arc::new(Progress::default());
    let reporter = {
        let progress = progress.clone();
        let total = release.asset.as_ref().map(|a| a.size).unwrap_or(0);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let done = progress.downloaded.load(Ordering::Relaxed);
                println!("  {done} / {total} bytes");
                if total > 0 && done >= total {
                    break;
                }
            }
        })
    };
    let exe = update::download_and_install(&release, &progress)?;
    let _ = reporter.join();
    println!("installed {} at {}", release.version, exe.display());
    Ok(())
}
