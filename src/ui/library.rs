//! File browser for the output folder (Videos / Images / Audios tabs).

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::t;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryTab {
    Videos,
    Images,
    Audios,
}

impl LibraryTab {
    pub const ALL: [LibraryTab; 3] = [LibraryTab::Videos, LibraryTab::Images, LibraryTab::Audios];

    /// Placeholder shown when the folder holds no file of this kind.
    pub fn empty_label(self) -> &'static str {
        match self {
            LibraryTab::Videos => t!(NO_VIDEOS_YET),
            LibraryTab::Images => t!(NO_IMAGES_YET),
            LibraryTab::Audios => t!(NO_AUDIOS_YET),
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            LibraryTab::Videos => &["mp4", "mkv", "mov", "webm", "avi"],
            LibraryTab::Images => &["png", "jpg", "jpeg", "bmp", "gif", "webp"],
            LibraryTab::Audios => &["mp3", "wav", "m4a", "flac", "ogg", "aac"],
        }
    }

    pub fn for_path(path: &Path) -> Option<LibraryTab> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        LibraryTab::ALL.into_iter().find(|t| t.extensions().contains(&ext.as_str()))
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: SystemTime,
}

pub struct Library {
    pub tab: LibraryTab,
    pub entries: Vec<Entry>,
    pub selected: Option<usize>,
    pub confirm_delete: Option<PathBuf>,
    last_scan: Option<Instant>,
    scanned_dir: Option<PathBuf>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            tab: LibraryTab::Videos,
            entries: Vec::new(),
            selected: None,
            confirm_delete: None,
            last_scan: None,
            scanned_dir: None,
        }
    }

    /// Rescans `dir` if the tab/folder changed, `force` is set, or the last
    /// scan is older than five seconds.
    pub fn refresh(&mut self, dir: &Path, force: bool) {
        let stale = self.last_scan.map(|t| t.elapsed().as_secs() >= 5).unwrap_or(true);
        let dir_changed = self.scanned_dir.as_deref() != Some(dir);
        if !(force || stale || dir_changed) {
            return;
        }
        let selected_path = self.selected.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
        self.entries = scan(dir, self.tab);
        self.selected = selected_path.and_then(|p| self.entries.iter().position(|e| e.path == p));
        self.last_scan = Some(Instant::now());
        self.scanned_dir = Some(dir.to_path_buf());
    }

    pub fn set_tab(&mut self, tab: LibraryTab, dir: &Path) {
        if self.tab != tab {
            self.tab = tab;
            self.selected = None;
            self.refresh(dir, true);
        }
    }

    /// Selects `path` (switching tab if needed) after a file was saved.
    pub fn select_path(&mut self, path: &Path, dir: &Path) {
        if let Some(tab) = LibraryTab::for_path(path) {
            self.tab = tab;
        }
        self.refresh(dir, true);
        self.selected = self.entries.iter().position(|e| e.path == path);
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.selected.and_then(|i| self.entries.get(i))
    }

    pub fn delete(&mut self, path: &Path, dir: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)?;
        self.selected = None;
        self.refresh(dir, true);
        Ok(())
    }
}

fn scan(dir: &Path, tab: LibraryTab) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Entry> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if LibraryTab::for_path(&path) != Some(tab) {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(Entry {
                name: path.file_name()?.to_string_lossy().into_owned(),
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                path,
            })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Opens `path` with the system default application.
pub fn open_with_default(path: &Path) {
    #[cfg(windows)]
    let cmd = std::process::Command::new("cmd").args(["/C", "start", "", &path.to_string_lossy()]).spawn();
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = cmd {
        log::warn!("open file: {e}");
    }
}

/// Reveals `path` in the file manager (selecting it where supported).
pub fn reveal_in_folder(path: &Path) {
    #[cfg(windows)]
    let cmd = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
    if let Err(e) = cmd {
        log::warn!("reveal file: {e}");
    }
}
