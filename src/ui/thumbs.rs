//! Poster frames for the library list.
//!
//! Decoding a frame out of a finished recording takes long enough to be felt
//! (Media Foundation has to open the file and run a decoder), so the work goes
//! to one background thread and the GUI paints a placeholder until the texture
//! arrives. Entries are keyed by path *and* by size + mtime, so a file that is
//! still growing — the recording in progress — is re-probed once it settles.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime};

use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};

use crate::video::thumbnail::{self, MediaInfo};

use super::library::Entry;

/// Longest side of a poster frame, in pixels. Twice the widest the list draws
/// it, so the thumbnail stays sharp on a 2× display.
const MAX_SIDE: u32 = 256;

/// Identity of a file version: a growing recording keeps its path but not this.
type Version = (u64, Option<Duration>);

/// A finished probe, ready to draw.
pub struct Thumb {
    pub texture: Option<TextureHandle>,
    pub duration: Option<Duration>,
}

enum Slot {
    Pending(Version),
    Done(Version, Thumb),
}

impl Slot {
    fn version(&self) -> Version {
        match self {
            Slot::Pending(v) | Slot::Done(v, _) => *v,
        }
    }
}

pub struct Thumbs {
    cache: HashMap<PathBuf, Slot>,
    jobs: Sender<(PathBuf, Version)>,
    done: Receiver<(PathBuf, Version, MediaInfo)>,
}

impl Thumbs {
    pub fn new() -> Self {
        let (jobs, job_rx) = mpsc::channel::<(PathBuf, Version)>();
        let (done_tx, done) = mpsc::channel();
        std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || {
                // Media Foundation needs COM on this thread for as long as it
                // is used; `probe` also guards itself, which only bumps the
                // reference count.
                #[cfg(windows)]
                let _com = crate::video::mf::ComGuard::new();
                while let Ok((path, version)) = job_rx.recv() {
                    let info = thumbnail::probe(&path, MAX_SIDE);
                    if done_tx.send((path, version, info)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self { cache: HashMap::new(), jobs, done }
    }

    /// Collects finished probes and uploads their images as GUI textures.
    /// Returns `true` when something new arrived, so the caller can repaint.
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let mut changed = false;
        while let Ok((path, version, info)) = self.done.try_recv() {
            let texture = info.poster.map(|p| {
                let image = ColorImage::from_rgba_unmultiplied([p.width as usize, p.height as usize], &p.rgba);
                ctx.load_texture(format!("thumb:{}", path.display()), image, TextureOptions::LINEAR)
            });
            self.cache.insert(path, Slot::Done(version, Thumb { texture, duration: info.duration }));
            changed = true;
        }
        changed
    }

    /// The poster and duration of `entry`, queueing a probe the first time the
    /// row is drawn. `None` while the answer is still on its way.
    pub fn get(&mut self, entry: &Entry) -> Option<&Thumb> {
        let version = version_of(entry);
        match self.cache.get(&entry.path) {
            Some(slot) if slot.version() == version => {}
            _ => {
                self.cache.insert(entry.path.clone(), Slot::Pending(version));
                let _ = self.jobs.send((entry.path.clone(), version));
            }
        }
        match self.cache.get(&entry.path) {
            Some(Slot::Done(_, thumb)) => Some(thumb),
            _ => None,
        }
    }

    /// Drops textures for files that are no longer listed, so browsing a large
    /// folder does not grow the cache without bound.
    pub fn retain(&mut self, entries: &[Entry]) {
        const KEEP: usize = 128;
        if self.cache.len() <= KEEP {
            return;
        }
        let live: std::collections::HashSet<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        self.cache.retain(|p, _| live.contains(p.as_path()));
    }
}

fn version_of(entry: &Entry) -> Version {
    (entry.size, entry.modified.duration_since(SystemTime::UNIX_EPOCH).ok())
}
