use crate::model::{Cancellation, FileEntry, Fingerprint, ItemStatus, OperationResult};
use anyhow::{Context, Result, bail};
use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};
use xattr::FileExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Skip,
    Number,
}
#[derive(Clone)]
pub enum Action {
    Trash,
    Move {
        destination: PathBuf,
        conflict: ConflictPolicy,
    },
}
#[derive(Clone)]
pub struct OperationPlan {
    pub entries: Vec<FileEntry>,
    pub action: Action,
}

pub fn verify(entry: &FileEntry) -> Result<()> {
    let metadata = fs::symlink_metadata(&entry.path)?;
    if metadata.is_symlink() || Fingerprint::from(&metadata) != entry.fingerprint {
        bail!("文件已变化，请重新扫描");
    }
    let _parent = open_directory(entry.path.parent().context("路径没有父目录")?)?;
    Ok(())
}
pub fn execute(
    plan: &OperationPlan,
    cancel: &Cancellation,
    mut progress: impl FnMut(OperationResult),
) {
    for entry in &plan.entries {
        if cancel.is_cancelled() {
            progress(OperationResult {
                item: entry.path.display().to_string(),
                status: ItemStatus::Cancelled,
                message: "未执行".into(),
                processed_bytes: 0,
            });
            continue;
        }
        let result = verify(entry).and_then(|()| match &plan.action {
            Action::Trash => {
                crate::platform::trash_file(&entry.path)?;
                Ok(true)
            }
            Action::Move {
                destination,
                conflict,
            } => move_file(entry, destination, *conflict, cancel),
        });
        let (status, message, bytes) = match result {
            Ok(true) => (
                ItemStatus::Succeeded,
                match plan.action {
                    Action::Trash => "已移入废纸篓；清空废纸篓后才会释放空间",
                    Action::Move { .. } => "移动完成",
                }
                .into(),
                entry.fingerprint.size,
            ),
            Ok(false) => (ItemStatus::Skipped, "目标已存在，已跳过".into(), 0),
            Err(e) => (
                if cancel.is_cancelled() {
                    ItemStatus::Cancelled
                } else {
                    ItemStatus::Failed
                },
                format!("{e:#}"),
                0,
            ),
        };
        progress(OperationResult {
            item: entry.path.display().to_string(),
            status,
            message,
            processed_bytes: bytes,
        });
    }
}
fn c_name(name: &OsStr) -> Result<CString> {
    Ok(CString::new(name.as_bytes())?)
}
fn owned_fd(fd: i32) -> Result<OwnedFd> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fd is a new, valid descriptor returned by open/openat; ownership is transferred once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
pub fn open_directory(path: &Path) -> Result<OwnedFd> {
    if !path.is_absolute() {
        bail!("路径必须为绝对路径");
    }
    // SAFETY: the string is NUL terminated and flags require a directory.
    let mut fd = owned_fd(unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    for part in path.components() {
        match part {
            Component::RootDir => (),
            Component::Normal(name) => {
                let name = c_name(name)?;
                // SAFETY: fd and name remain valid throughout the call. NOFOLLOW rejects path substitutions.
                fd = owned_fd(unsafe {
                    libc::openat(
                        fd.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                })?;
            }
            _ => bail!("路径不能包含相对组件"),
        }
    }
    Ok(fd)
}
fn rename_exclusive(from: RawFd, source: &CStr, to: RawFd, target: &CStr) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    // SAFETY: both descriptors are owned by the caller and strings are valid C strings.
    let result = unsafe {
        libc::renameat2(
            from,
            source.as_ptr(),
            to,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both descriptors are owned by the caller and strings are valid C strings.
    let result = unsafe {
        libc::renameatx_np(
            from,
            source.as_ptr(),
            to,
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
pub fn move_file(
    entry: &FileEntry,
    destination: &Path,
    conflict: ConflictPolicy,
    cancel: &Cancellation,
) -> Result<bool> {
    verify(entry)?;
    let parent = entry.path.parent().context("缺少父目录")?;
    let source_dir = open_directory(parent)?;
    let destination = fs::canonicalize(destination)?;
    let target_dir = open_directory(&destination)?;
    let source_name = c_name(entry.path.file_name().context("缺少文件名")?)?;
    let mut target_name = source_name.clone();
    let mut index = 0;
    loop {
        if cancel.is_cancelled() {
            bail!("移动已取消");
        }
        let outcome = if entry_exists(target_dir.as_raw_fd(), &target_name)? {
            Err(std::io::Error::from_raw_os_error(libc::EEXIST))
        } else {
            rename_exclusive(
                source_dir.as_raw_fd(),
                &source_name,
                target_dir.as_raw_fd(),
                &target_name,
            )
        };
        match outcome {
            Ok(()) => return Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                if conflict == ConflictPolicy::Skip {
                    return Ok(false);
                }
                index += 1;
                let mut name = entry.path.file_stem().unwrap_or_default().to_os_string();
                name.push(format!(" ({index})"));
                if let Some(extension) = entry.path.extension() {
                    name.push(".");
                    name.push(extension);
                }
                target_name = c_name(&name)?;
            }
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                return copy_then_remove(
                    entry,
                    &source_dir,
                    &source_name,
                    &target_dir,
                    &target_name,
                    &destination,
                    cancel,
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn copy_then_remove(
    entry: &FileEntry,
    source_dir: &OwnedFd,
    source_name: &CStr,
    target_dir: &OwnedFd,
    target_name: &CStr,
    destination: &Path,
    cancel: &Cancellation,
) -> Result<bool> {
    // SAFETY: directory fd and filename are valid; O_NOFOLLOW prevents a substituted symlink.
    let fd = owned_fd(unsafe {
        libc::openat(
            source_dir.as_raw_fd(),
            source_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    let mut source = File::from(fd);
    if Fingerprint::from(&source.metadata()?) != entry.fingerprint {
        bail!("源文件已变化");
    }
    let mut temp = tempfile::Builder::new()
        .prefix(".rsclear-move-")
        .tempfile_in(destination)?;
    #[cfg(target_os = "macos")]
    {
        // Native macOS copyfile uses uncached destination I/O for sparse copies.
        // SAFETY: the descriptor is owned and valid; F_NOCACHE takes an integer flag.
        if unsafe { libc::fcntl(temp.as_file().as_raw_fd(), libc::F_NOCACHE, 1) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    // Establish holes before writing: extending past EOF while copying can
    // materialize skipped zero ranges on macOS.
    temp.as_file().set_len(entry.fingerprint.size)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hash = blake3::Hasher::new();
    let mut length = 0u64;
    loop {
        if cancel.is_cancelled() {
            bail!("复制已取消，保留源文件");
        }
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        write_sparse_chunk(temp.as_file_mut(), &buffer[..count])?;
        length += count as u64;
    }
    if length != entry.fingerprint.size {
        bail!("源文件大小在复制期间变化，保留源文件");
    }
    temp.as_file()
        .set_permissions(source.metadata()?.permissions())?;
    for name in source.list_xattr()? {
        if let Some(value) = source.get_xattr(&name)? {
            temp.as_file().set_xattr(&name, &value)?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: both fds remain valid. Copy only ACL metadata, never file contents.
        if unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                temp.as_file().as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_ACL,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let metadata = source.metadata()?;
    temp.as_file().set_times(
        fs::FileTimes::new()
            .set_accessed(metadata.accessed()?)
            .set_modified(metadata.modified()?),
    )?;
    temp.as_file().sync_all()?;
    temp.as_file_mut().seek(SeekFrom::Start(0))?;
    let mut verify_hash = blake3::Hasher::new();
    loop {
        if cancel.is_cancelled() {
            bail!("校验已取消，保留源文件");
        }
        let count = temp.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        verify_hash.update(&buffer[..count]);
    }
    if hash.finalize() != verify_hash.finalize()
        || Fingerprint::from(&source.metadata()?) != entry.fingerprint
    {
        bail!("复制校验失败或源文件已变化，保留源文件");
    }
    verify(entry)?;
    let temp_name = c_name(temp.path().file_name().context("缺少临时文件名")?)?;
    rename_exclusive(
        target_dir.as_raw_fd(),
        &temp_name,
        target_dir.as_raw_fd(),
        target_name,
    )?;
    // A committed copy survives cancellation or failure after this point.
    File::from(target_dir.try_clone()?).sync_all()?;
    verify(entry).context("目标副本已保存，但源文件变化，未删除源文件")?;
    remove_tree(&entry.path, &entry.fingerprint, cancel)
        .context("副本已保存，源文件未能完全移除")?;
    File::from(source_dir.try_clone()?).sync_all()?;
    Ok(true)
}

fn write_sparse_chunk(file: &mut File, bytes: &[u8]) -> Result<()> {
    // Coalesce equal runs: dense buffers still use one write, while a short
    // data prefix must not materialize an otherwise empty megabyte on APFS.
    let mut start = 0;
    let mut zero_run = bytes[..bytes.len().min(4096)].iter().all(|b| *b == 0);
    for offset in (4096..bytes.len()).step_by(4096) {
        let zero = bytes[offset..(offset + 4096).min(bytes.len())]
            .iter()
            .all(|b| *b == 0);
        if zero != zero_run {
            if zero_run {
                file.seek(SeekFrom::Current((offset - start) as i64))?;
            } else {
                file.write_all(&bytes[start..offset])?;
            }
            start = offset;
            zero_run = zero;
        }
    }
    if zero_run {
        file.seek(SeekFrom::Current((bytes.len() - start) as i64))?;
    } else {
        file.write_all(&bytes[start..])?;
    }
    Ok(())
}

/// Deletes only beneath an already validated directory, with fd-relative traversal.
pub fn remove_tree(path: &Path, expected: &Fingerprint, cancel: &Cancellation) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("清理已取消");
    }
    let parent_path = path.parent().context("不能删除根目录")?;
    let parent = open_directory(parent_path)?;
    let name = c_name(path.file_name().context("不能删除根目录")?)?;
    let metadata = fs::symlink_metadata(path)?;
    if &Fingerprint::from(&metadata) != expected || metadata.is_symlink() {
        bail!("清理对象已变化，请重新扫描");
    }
    // Detach the reviewed entry into a private sibling before deleting it. A name
    // replaced between the first check and rename is verified and restored, never deleted.
    let stage = tempfile::Builder::new()
        .prefix(".rsclear-pending-")
        .tempdir_in(parent_path)?;
    let stage_fd = open_directory(stage.path())?;
    rename_exclusive(parent.as_raw_fd(), &name, stage_fd.as_raw_fd(), c"entry")?;
    // Disable recursive Drop: an unexpected or partly cleaned object must survive errors.
    let stage = stage.keep();
    let result = (|| -> Result<()> {
        // SAFETY: stage_fd is owned and entry is fixed; refuse substituted links or blocking FIFOs.
        let fd = owned_fd(unsafe {
            libc::openat(
                stage_fd.as_raw_fd(),
                c"entry".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        })?;
        let actual = Fingerprint::from(&File::from(fd).metadata()?);
        let mut compared = actual.clone();
        // Rename itself changes ctime, but must not change identity, size, mtime or mode.
        compared.ctime = expected.ctime;
        compared.ctime_nsec = expected.ctime_nsec;
        if &compared != expected {
            bail!("清理对象在执行时被替换或修改");
        }
        remove_at(
            stage_fd.as_raw_fd(),
            c"entry",
            expected.dev,
            Some(&actual),
            cancel,
        )
    })();
    if let Err(error) = result {
        if entry_exists(stage_fd.as_raw_fd(), c"entry")?
            && rename_exclusive(stage_fd.as_raw_fd(), c"entry", parent.as_raw_fd(), &name).is_err()
        {
            bail!(
                "{error:#}；剩余内容保留在 {}",
                stage.join("entry").display()
            );
        }
        let _ = fs::remove_dir(&stage);
        return Err(error);
    }
    fs::remove_dir(stage)?;
    Ok(())
}
fn remove_at(
    parent: RawFd,
    name: &CStr,
    device: u64,
    expected: Option<&Fingerprint>,
    cancel: &Cancellation,
) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("清理已取消，已删除的内容不会恢复");
    }
    // SAFETY: valid parent and name; opening directories only avoids devices/FIFOs and symlinks.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if ![Some(libc::ENOTDIR), Some(libc::ELOOP)].contains(&error.raw_os_error()) {
            return Err(error.into());
        }
        if let Some(expected) = expected {
            let actual = stat_at(parent, name)?;
            if actual.st_dev as u64 != expected.dev
                || actual.st_ino != expected.ino
                || actual.st_mode as u64 != u64::from(expected.mode)
            {
                bail!("清理对象已被替换");
            }
        }
        // SAFETY: unlinkat does not follow the final entry; removing a link never removes its target.
        if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        return Ok(());
    }
    let directory = owned_fd(fd)?;
    let file = File::from(directory.try_clone()?);
    if let Some(expected) = expected {
        let actual = Fingerprint::from(&file.metadata()?);
        if &actual != expected {
            bail!("清理目录已被替换或修改");
        }
    }
    if file.metadata()?.dev() != device {
        bail!("拒绝跨挂载点删除");
    }
    // SAFETY: fdopendir takes ownership of dup's descriptor, not directory's original fd.
    let stream = unsafe { libc::fdopendir(libc::dup(directory.as_raw_fd())) };
    if stream.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            /* SAFETY: this object uniquely owns the directory stream. */
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    loop {
        // SAFETY: stream is live, used by this thread only. Copy the name before the next readdir.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir returned a non-null dirent with a NUL-terminated d_name.
        let child = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
        if child.as_bytes() == b"." || child.as_bytes() == b".." {
            continue;
        }
        remove_at(directory.as_raw_fd(), &child, device, None, cancel)?;
    }
    // SAFETY: remove the directory entry itself; non-empty/replaced directories fail normally.
    if unsafe { libc::unlinkat(parent, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn stat_at(parent: RawFd, name: &CStr) -> std::io::Result<libc::stat> {
    let mut result = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat initializes result on success; name and parent are valid.
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            result.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized all fields of the stat structure.
    Ok(unsafe { result.assume_init() })
}
fn entry_exists(parent: RawFd, name: &CStr) -> Result<bool> {
    match stat_at(parent, name) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn append_history(path: &Path, result: &OperationResult) -> Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > 10 * 1024 * 1024) {
        fs::rename(path, path.with_extension("previous.jsonl"))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(&mut file, result)?;
    writeln!(file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryKind;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> tempfile::TempDir {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".build/tmp");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    // Exercises the actual EXDEV copy implementation on one filesystem; routing across
    // two physical filesystems is a separate platform acceptance check.
    #[test]
    fn copy_commit_preserves_content_permissions_xattrs_and_sparse_size() {
        let tmp = fixture();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let mut file = File::create(&source).unwrap();
        file.write_all(b"content").unwrap();
        file.set_len(4 * 1024 * 1024).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o640))
            .unwrap();
        file.set_xattr("user.rsclear-test", b"metadata").unwrap();
        // Compare committed allocation on both sides, not delayed source writes
        // against a fully synced destination.
        file.sync_all().unwrap();
        let meta = file.metadata().unwrap();
        let mut probe = File::create(tmp.path().join("sparse-probe")).unwrap();
        probe.write_all(b"content").unwrap();
        probe.seek(SeekFrom::Start(4 * 1024 * 1024 - 1)).unwrap();
        probe.write_all(&[0]).unwrap();
        probe.sync_all().unwrap();
        let probe_allocated = probe.metadata().unwrap().blocks() * 512;
        let item = FileEntry {
            path: source.clone(),
            fingerprint: Fingerprint::from(&meta),
            allocated: meta.blocks() * 512,
            kind: EntryKind::File,
        };
        let source_dir = open_directory(tmp.path()).unwrap();
        let target_dir = open_directory(&dest).unwrap();
        copy_then_remove(
            &item,
            &source_dir,
            c"source",
            &target_dir,
            c"moved",
            &dest,
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!source.exists());
        let target = dest.join("moved");
        let bytes = fs::read(&target).unwrap();
        assert_eq!(&bytes[..7], b"content");
        assert!(bytes[7..].iter().all(|b| *b == 0));
        let metadata = fs::metadata(&target).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
        assert_eq!(metadata.mtime(), meta.mtime());
        assert_eq!(
            xattr::get(&target, "user.rsclear-test").unwrap(),
            Some(b"metadata".to_vec())
        );
        println!(
            "::notice title=Committed file allocation::source={}, copy={}, seek/write probe={}, logical={}",
            meta.blocks() * 512,
            metadata.blocks() * 512,
            probe_allocated,
            metadata.len()
        );
        if meta.blocks() * 512 < meta.len() {
            assert!(
                metadata.blocks() * 512 < metadata.len(),
                "a committed sparse source must stay sparse"
            );
        } else {
            assert!(
                metadata.blocks() * 512 <= meta.blocks() * 512,
                "a dense copy must not increase allocated blocks"
            );
        }
    }

    #[test]
    fn copy_preserves_alternating_zero_and_data_runs_across_buffers() {
        let tmp = fixture();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let mut payload = vec![0; 1024 * 1024 + 5003];
        payload[4100..8201].fill(17);
        payload[32768..65536].fill(29);
        payload[1024 * 1024 - 2..1024 * 1024 + 3].fill(41);
        let length = payload.len();
        payload[length - 3..].fill(53);
        fs::write(&source, &payload).unwrap();
        let meta = fs::metadata(&source).unwrap();
        let item = FileEntry {
            path: source.clone(),
            fingerprint: Fingerprint::from(&meta),
            allocated: meta.blocks() * 512,
            kind: EntryKind::File,
        };
        copy_then_remove(
            &item,
            &open_directory(tmp.path()).unwrap(),
            c"source",
            &open_directory(&dest).unwrap(),
            c"moved",
            &dest,
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(dest.join("moved")).unwrap(), payload);
    }

    #[test]
    fn cancelled_copy_preserves_source_and_does_not_publish_destination() {
        let tmp = fixture();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, b"content").unwrap();
        let meta = fs::metadata(&source).unwrap();
        let item = FileEntry {
            path: source.clone(),
            fingerprint: Fingerprint::from(&meta),
            allocated: meta.blocks() * 512,
            kind: EntryKind::File,
        };
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(
            copy_then_remove(
                &item,
                &open_directory(tmp.path()).unwrap(),
                c"source",
                &open_directory(&dest).unwrap(),
                c"moved",
                &dest,
                &cancel
            )
            .is_err()
        );
        assert_eq!(fs::read(source).unwrap(), b"content");
        assert_eq!(fs::read_dir(dest).unwrap().count(), 0);
    }
}
