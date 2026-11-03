use serde::{Deserialize, Serialize};
use std::{
    fs::Metadata,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Default)]
pub struct Cancellation(pub Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
    pub mode: u32,
    pub links: u64,
}
impl From<&Metadata> for Fingerprint {
    fn from(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
            mode: m.mode(),
            links: m.nlink(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub fingerprint: Fingerprint,
    pub allocated: u64,
    pub kind: EntryKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File = 0,
    Directory = 1,
    Symlink = 2,
    Other = 3,
}

#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub files: u64,
    pub directories: u64,
    pub logical: u64,
    pub allocated: u64,
    pub errors: u64,
    pub elapsed_ms: u128,
}
#[derive(Debug, Clone)]
pub enum ScanEvent {
    Progress(ScanProgress),
    Finished {
        progress: ScanProgress,
        cancelled: bool,
    },
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ItemStatus {
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationResult {
    pub item: String,
    pub status: ItemStatus,
    pub message: String,
    pub processed_bytes: u64,
}

pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
