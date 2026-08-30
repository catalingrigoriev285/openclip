//! A tiny file log, opened on the worker thread.
//!
//! `log` + `env_logger` are not used here. Installing a global logger inside
//! someone else's process is not ours to do — the game may have its own — and
//! the whole point of this file is to be openable from the worker thread only,
//! never from `DllMain`.
//!
//! openclip surfaces the log through an "Open hook log" button, because when a
//! hook misbehaves inside a fullscreen game there is nowhere else for a message
//! to go.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// `%LOCALAPPDATA%\openclip\hook-<pid>.log`, or `None` if it cannot be opened —
/// in which case logging silently does nothing rather than failing the hook.
fn path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let dir = PathBuf::from(base).join("openclip");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("hook-{}.log", std::process::id())))
}

fn file() -> Option<&'static Mutex<File>> {
    LOG.get_or_init(|| {
        let path = path()?;
        // Truncate: one file per process, so a game relaunched twenty times does
        // not leave a log nobody will ever read the top of.
        OpenOptions::new().create(true).write(true).truncate(true).open(path).ok().map(Mutex::new)
    })
    .as_ref()
}

pub fn write(args: std::fmt::Arguments<'_>) {
    let Some(f) = file() else { return };
    let Ok(mut f) = f.lock() else { return };
    let _ = writeln!(f, "{args}");
    let _ = f.flush();
}

/// The hook's equivalent of `log::info!`.
macro_rules! hlog {
    ($($arg:tt)*) => { $crate::logging::write(format_args!($($arg)*)) };
}

pub(crate) use hlog;
