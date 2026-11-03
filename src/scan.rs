use crate::{
    index::{bytes_path, path_bytes},
    model::*,
};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct ScanRequest {
    pub roots: Vec<PathBuf>,
    pub exclusions: Vec<PathBuf>,
    pub threads: usize,
    pub database: PathBuf,
}
pub struct ScanHandle {
    pub cancel: Cancellation,
    pub events: Receiver<ScanEvent>,
    pub join: thread::JoinHandle<()>,
}
struct Directory {
    path: PathBuf,
    device: u64,
}
enum Message {
    Batch(Vec<FileEntry>),
    Error(PathBuf, String),
    Done(PathBuf),
}

pub fn start(request: ScanRequest) -> ScanHandle {
    let cancel = Cancellation::default();
    let token = cancel.clone();
    let (tx, events) = bounded(16);
    let join = thread::spawn(move || {
        if let Err(e) = run(&request, &token, &tx) {
            let _ = tx.send(ScanEvent::Failed(format!("{e:#}")));
        }
    });
    ScanHandle {
        cancel,
        events,
        join,
    }
}
fn blocked(path: &Path, exclusions: &[PathBuf]) -> bool {
    ["/proc", "/sys", "/dev", "/run"]
        .iter()
        .any(|p| path.starts_with(p))
        || exclusions.iter().any(|p| path.starts_with(p))
}
pub fn run(
    request: &ScanRequest,
    cancel: &Cancellation,
    events: &Sender<ScanEvent>,
) -> Result<ScanProgress> {
    if request.roots.is_empty() {
        bail!("请先添加扫描目录");
    }
    let mut db = crate::index::create(&request.database)?;
    let exclusions: Vec<_> = request
        .exclusions
        .iter()
        .map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    for root in &request.roots {
        let root = fs::canonicalize(root)
            .with_context(|| format!("扫描目录不可访问: {}", root.display()))?;
        if blocked(&root, &exclusions) {
            continue;
        }
        let meta = fs::metadata(&root)?;
        if !meta.is_dir() {
            bail!("不是目录: {}", root.display());
        }
        db.execute(
            "INSERT OR IGNORE INTO pending(path,device,inode) VALUES(?1,?2,?3)",
            params![path_bytes(&root), meta.dev() as i64, meta.ino() as i64],
        )?;
    }
    let workers = request.threads.clamp(1, 8);
    let readers = workers.min(2);
    let (jobs_tx, jobs_rx) = bounded::<Directory>(readers);
    let (metadata_tx, metadata_rx) = bounded::<(Vec<PathBuf>, u64)>(16);
    let pending_metadata = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = bounded(16);
    let mut handles = Vec::new();
    for _ in 0..workers {
        let (jobs, tx, token) = (metadata_rx.clone(), tx.clone(), cancel.clone());
        handles.push(thread::spawn(move || {
            while let Ok((paths, device)) = jobs.recv() {
                let entries = read_metadata(paths, device, &token, &tx);
                if tx.send(Message::Batch(entries)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(metadata_rx);
    for _ in 0..readers {
        let (jobs, tx, token) = (jobs_rx.clone(), tx.clone(), cancel.clone());
        let exclusions = exclusions.clone();
        let metadata = metadata_tx.clone();
        let pending = pending_metadata.clone();
        handles.push(thread::spawn(move || {
            while let Ok(job) = jobs.recv() {
                if !token.is_cancelled() {
                    walk_directory(&job, &exclusions, &token, &tx, &metadata, &pending);
                }
                if tx.send(Message::Done(job.path)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(metadata_tx);
    drop(tx);
    let mut progress = ScanProgress::default();
    let begin = Instant::now();
    let mut published = Instant::now();
    let mut active = 0;
    let result = (|| -> Result<()> {
        loop {
            while active < readers && !cancel.is_cancelled() {
                let next = db
                    .query_row(
                        "SELECT path,device FROM pending WHERE state=0 LIMIT 1",
                        [],
                        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                let Some((path, dev)) = next else {
                    break;
                };
                db.execute("UPDATE pending SET state=1 WHERE path=?1", params![path])?;
                jobs_tx.send(Directory {
                    path: bytes_path(path),
                    device: dev as u64,
                })?;
                active += 1;
            }
            if active == 0 && pending_metadata.load(Ordering::SeqCst) == 0 {
                break;
            }
            match rx.recv()? {
                Message::Batch(entries) => {
                    pending_metadata.fetch_sub(1, Ordering::SeqCst);
                    write_batch(&mut db, entries, &mut progress)?;
                }
                Message::Error(path, message) => {
                    progress.errors += 1;
                    if progress.errors <= 1000 {
                        db.execute(
                            "INSERT INTO errors VALUES(?1,?2)",
                            params![path.to_string_lossy(), message],
                        )?;
                    }
                }
                Message::Done(path) => {
                    active -= 1;
                    progress.directories += 1;
                    db.execute(
                        "UPDATE pending SET state=2 WHERE path=?1",
                        params![path_bytes(&path)],
                    )?;
                }
            }
            if published.elapsed() >= Duration::from_millis(100) {
                progress.elapsed_ms = begin.elapsed().as_millis();
                let _ = events.try_send(ScanEvent::Progress(progress.clone()));
                published = Instant::now();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        cancel.cancel();
    }
    drop(jobs_tx);
    // Disconnect results before joining so a database error cannot strand producers.
    drop(rx);
    for handle in handles {
        let _ = handle.join();
    }
    result?;
    progress.elapsed_ms = begin.elapsed().as_millis();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let _ = events.send(ScanEvent::Finished {
        progress: progress.clone(),
        cancelled: cancel.is_cancelled(),
    });
    Ok(progress)
}
fn walk_directory(
    job: &Directory,
    exclusions: &[PathBuf],
    cancel: &Cancellation,
    tx: &Sender<Message>,
    metadata_tx: &Sender<(Vec<PathBuf>, u64)>,
    pending: &AtomicUsize,
) {
    let iterator = match fs::read_dir(&job.path) {
        Ok(it) => it,
        Err(e) => {
            let _ = tx.send(Message::Error(job.path.clone(), e.to_string()));
            return;
        }
    };
    let mut batch = Vec::with_capacity(256);
    let submit = |batch| {
        pending.fetch_add(1, Ordering::SeqCst);
        metadata_tx.send((batch, job.device)).is_ok()
    };
    for entry in iterator {
        if cancel.is_cancelled() {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Message::Error(job.path.clone(), e.to_string()));
                continue;
            }
        };
        let path = entry.path();
        if blocked(&path, exclusions) {
            continue;
        }
        batch.push(path);
        if batch.len() == 256 && !submit(std::mem::replace(&mut batch, Vec::with_capacity(256))) {
            return;
        }
    }
    if !batch.is_empty() {
        submit(batch);
    }
}
fn read_metadata(
    paths: Vec<PathBuf>,
    device: u64,
    cancel: &Cancellation,
    tx: &Sender<Message>,
) -> Vec<FileEntry> {
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        if cancel.is_cancelled() {
            break;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(Message::Error(path, e.to_string()));
                continue;
            }
        };
        if metadata.dev() != device {
            continue;
        }
        let kind = if metadata.is_symlink() {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        result.push(FileEntry {
            path,
            fingerprint: Fingerprint::from(&metadata),
            allocated: metadata.blocks().saturating_mul(512),
            kind,
        });
    }
    result
}
fn write_batch(
    db: &mut Connection,
    entries: Vec<FileEntry>,
    progress: &mut ScanProgress,
) -> Result<()> {
    let transaction = db.transaction()?;
    {
        let mut insert = transaction
            .prepare_cached("INSERT OR IGNORE INTO entries VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
        let mut queue = transaction
            .prepare_cached("INSERT OR IGNORE INTO pending(path,device,inode) VALUES(?1,?2,?3)")?;
        let mut inode = transaction.prepare_cached("INSERT OR IGNORE INTO inodes VALUES(?1,?2)")?;
        for entry in entries {
            let fp = &entry.fingerprint;
            let added = insert.execute(params![
                path_bytes(&entry.path),
                entry.path.file_name().unwrap_or_default().to_string_lossy(),
                entry.path.to_string_lossy(),
                entry
                    .path
                    .extension()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
                entry.kind as i64,
                fp.size.min(i64::MAX as u64) as i64,
                entry.allocated.min(i64::MAX as u64) as i64,
                fp.mtime,
                serde_json::to_string(fp)?
            ])?;
            if added == 0 {
                continue;
            }
            if entry.kind == EntryKind::Directory {
                queue.execute(params![
                    path_bytes(&entry.path),
                    fp.dev as i64,
                    fp.ino as i64
                ])?;
            }
            if entry.kind == EntryKind::File {
                progress.files += 1;
                progress.logical = progress.logical.saturating_add(fp.size);
                if fp.links <= 1 || inode.execute(params![fp.dev as i64, fp.ino as i64])? != 0 {
                    progress.allocated = progress.allocated.saturating_add(entry.allocated);
                }
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

/// Streams a directory without following links, for per-candidate size estimates.
/// Results are occupancy estimates, not guaranteed reclaimed bytes (hard links/APFS).
pub fn measure(path: &Path, cancel: &Cancellation) -> Result<(u64, i64)> {
    let root = fs::symlink_metadata(path)?;
    if root.is_symlink() {
        bail!("不扫描符号链接");
    }
    let device = root.dev();
    let mut total: u64 = 0;
    let mut newest = root.mtime();
    let mut linked = std::collections::HashSet::new();
    let mut stack = Vec::new();
    if root.is_file() {
        return Ok((root.blocks().saturating_mul(512), root.mtime()));
    }
    stack.push(fs::read_dir(path)?);
    while let Some(iter) = stack.last_mut() {
        if cancel.is_cancelled() {
            bail!("操作已取消");
        }
        let Some(entry) = iter.next() else {
            stack.pop();
            continue;
        };
        let entry = entry?;
        let m = fs::symlink_metadata(entry.path())?;
        if m.dev() != device || m.is_symlink() {
            continue;
        }
        newest = newest.max(m.mtime());
        if m.is_dir() {
            stack.push(fs::read_dir(entry.path())?);
        } else if m.is_file() && (m.nlink() <= 1 || linked.insert((m.dev(), m.ino()))) {
            total = total.saturating_add(m.blocks().saturating_mul(512));
        }
    }
    Ok((total, newest))
}
