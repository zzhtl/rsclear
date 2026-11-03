use crate::model::{EntryKind, FileEntry, Fingerprint};
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};
use std::{
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}
pub fn bytes_path(bytes: Vec<u8>) -> PathBuf {
    std::ffi::OsString::from_vec(bytes).into()
}

pub fn create(path: &Path) -> Result<Connection> {
    // The caller has stopped the previous writer and readers. Replacing a disposable
    // index avoids walking millions of old B-tree cells on every new scan.
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        match std::fs::remove_file(candidate) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(3))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-16384;
        DROP TABLE IF EXISTS entries; DROP TABLE IF EXISTS pending;
        CREATE TABLE entries (
            path BLOB NOT NULL, name TEXT NOT NULL, display TEXT NOT NULL,
            extension TEXT NOT NULL, kind INTEGER NOT NULL, logical INTEGER NOT NULL,
            allocated INTEGER NOT NULL, mtime INTEGER NOT NULL, fingerprint TEXT NOT NULL
        );
        CREATE INDEX entries_size ON entries(kind, logical DESC);
        CREATE INDEX entries_time ON entries(kind, mtime DESC);
        CREATE INDEX entries_manifests ON entries(name) WHERE name='Cargo.toml' AND kind=0;
        CREATE TABLE pending (path BLOB PRIMARY KEY, device INTEGER NOT NULL, inode INTEGER NOT NULL, state INTEGER NOT NULL DEFAULT 0, UNIQUE(device,inode));
        CREATE INDEX IF NOT EXISTS pending_state ON pending(state);
        CREATE TABLE IF NOT EXISTS inodes (dev INTEGER, ino INTEGER, PRIMARY KEY(dev, ino));
        CREATE TABLE IF NOT EXISTS errors (path TEXT, message TEXT);
        DELETE FROM entries; DELETE FROM pending; DELETE FROM inodes; DELETE FROM errors;")?;
    Ok(db)
}
pub fn read(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch("PRAGMA cache_size=-8192;")?;
    Ok(db)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Size,
    Modified,
}
#[derive(Clone)]
pub struct Query {
    pub min_bytes: u64,
    pub search: String,
    pub extension: String,
    pub modified_before: Option<i64>,
    pub sort: Sort,
    pub offset: u64,
}
impl Default for Query {
    fn default() -> Self {
        Self {
            min_bytes: 100 * 1024 * 1024,
            search: String::new(),
            extension: String::new(),
            modified_before: None,
            sort: Sort::Size,
            offset: 0,
        }
    }
}
pub const PAGE_SIZE: usize = 250;
pub fn query(path: &Path, q: &Query) -> Result<Vec<FileEntry>> {
    let db = read(path)?;
    let order = match q.sort {
        Sort::Size => "logical DESC, rowid",
        Sort::Modified => "mtime DESC, rowid",
    };
    let sql = format!(
        "SELECT path, fingerprint, allocated FROM entries WHERE kind=0 AND logical>=?1
        AND (?2='' OR instr(lower(display),lower(?2))>0) AND (?3='' OR extension=lower(?3))
        AND (?4 IS NULL OR mtime<=?4) ORDER BY {order} LIMIT ?5 OFFSET ?6"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            q.min_bytes.min(i64::MAX as u64) as i64,
            q.search,
            q.extension.trim_start_matches('.'),
            q.modified_before,
            PAGE_SIZE as i64,
            q.offset.min(i64::MAX as u64) as i64
        ],
        |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    let mut result = Vec::new();
    for row in rows {
        let (path, fp, allocated) = row?;
        result.push(FileEntry {
            path: bytes_path(path),
            fingerprint: serde_json::from_str::<Fingerprint>(&fp)?,
            allocated: allocated as u64,
            kind: EntryKind::File,
        });
    }
    Ok(result)
}
pub fn manifests(path: &Path) -> Result<Vec<PathBuf>> {
    let db = read(path)?;
    let mut stmt = db.prepare("SELECT path FROM entries WHERE name='Cargo.toml' AND kind=0")?;
    let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
    Ok(rows
        .map(|r| r.map(bytes_path))
        .collect::<Result<Vec<_>, _>>()?)
}
pub fn errors(path: &Path) -> Result<Vec<(String, String)>> {
    let db = read(path)?;
    let mut stmt = db.prepare("SELECT path,message FROM errors LIMIT 100")?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}
