use rsclear::{
    cargo,
    index::{self, Query},
    model::*,
    operations::{self, ConflictPolicy},
    scan::{self, ScanRequest},
    system::{PrivilegedRequest, SystemRule},
};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn temp() -> tempfile::TempDir {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".build/tmp");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn entry(path: &Path) -> FileEntry {
    let m = fs::symlink_metadata(path).unwrap();
    FileEntry {
        path: path.into(),
        fingerprint: Fingerprint::from(&m),
        allocated: m.blocks() * 512,
        kind: if m.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
    }
}
fn scan_root(root: &Path, database: &Path) -> ScanProgress {
    let handle = scan::start(ScanRequest {
        roots: vec![root.into(), root.join("sub")],
        exclusions: vec![root.join("excluded")],
        threads: 4,
        database: database.into(),
    });
    let result = loop {
        match handle.events.recv_timeout(Duration::from_secs(30)).unwrap() {
            ScanEvent::Finished { progress, .. } => break progress,
            ScanEvent::Failed(e) => panic!("{e}"),
            _ => (),
        }
    };
    handle.join.join().unwrap();
    result
}
#[test]
fn scan_real_files_links_sparse_raw_names_and_overlapping_roots() {
    let temp = temp();
    let root = temp.path().join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::create_dir(root.join("excluded")).unwrap();
    fs::write(root.join(".hidden"), b"hidden").unwrap();
    fs::write(root.join("sub/data"), [1u8; 4096]).unwrap();
    fs::hard_link(root.join("sub/data"), root.join("linked")).unwrap();
    fs::write(root.join("excluded/secret"), b"excluded").unwrap();
    symlink(&root, root.join("sub/cycle")).unwrap();
    #[cfg(target_os = "linux")]
    let raw = root.join(std::ffi::OsString::from_vec(vec![0xff, 0xfe]));
    // APFS rejects invalid UTF-8 names before the scanner can encounter them.
    #[cfg(target_os = "macos")]
    let raw = root.join("文件-扫描");
    fs::write(&raw, b"raw").unwrap();
    let sparse = root.join("sparse");
    fs::File::create(&sparse)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let db = temp.path().join("scan.sqlite");
    let progress = scan_root(&root, &db);
    assert_eq!(progress.files, 5);
    assert_eq!(progress.directories, 2);
    assert_eq!(progress.errors, 0);
    let expected = [&raw, &sparse, &root.join(".hidden"), &root.join("sub/data")]
        .iter()
        .map(|p| fs::metadata(p).unwrap().blocks() * 512)
        .sum::<u64>();
    assert_eq!(progress.allocated, expected);
    let rows = index::query(
        &db,
        &Query {
            min_bytes: 0,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(rows.iter().any(|r| r.path == raw));
    assert_eq!(rows[0].path, sparse);
}
#[test]
fn scan_cancellation_does_not_strand_workers() {
    let temp = temp();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    for i in 0..2000 {
        fs::write(root.join(format!("{i}")), b"a").unwrap();
    }
    let handle = scan::start(ScanRequest {
        roots: vec![root],
        exclusions: vec![],
        threads: 4,
        database: temp.path().join("scan.sqlite"),
    });
    let begin = Instant::now();
    handle.cancel.cancel();
    loop {
        if let ScanEvent::Finished { cancelled, .. } =
            handle.events.recv_timeout(Duration::from_secs(3)).unwrap()
        {
            assert!(cancelled);
            break;
        }
    }
    handle.join.join().unwrap();
    assert!(begin.elapsed() < Duration::from_secs(3));
}

#[test]
#[ignore = "requires a generated million-file fixture at RSCLEAR_BENCH_ROOT"]
fn running_million_file_scan_cancels_within_one_second() {
    let tmp = temp();
    let root =
        PathBuf::from(std::env::var_os("RSCLEAR_BENCH_ROOT").expect("set RSCLEAR_BENCH_ROOT"));
    assert!(root.starts_with(env!("CARGO_MANIFEST_DIR")));
    let handle = scan::start(ScanRequest {
        roots: vec![root],
        exclusions: vec![],
        threads: 4,
        database: tmp.path().join("cancel.sqlite"),
    });
    loop {
        match handle.events.recv_timeout(Duration::from_secs(10)).unwrap() {
            ScanEvent::Progress(p) if p.files > 0 => break,
            ScanEvent::Failed(e) => panic!("{e}"),
            ScanEvent::Finished { .. } => {
                panic!("fixture too small to test an active cancellation")
            }
            _ => (),
        }
    }
    let start = Instant::now();
    handle.cancel.cancel();
    loop {
        if let ScanEvent::Finished { cancelled, .. } =
            handle.events.recv_timeout(Duration::from_secs(1)).unwrap()
        {
            assert!(cancelled);
            break;
        }
    }
    handle.join.join().unwrap();
    println!("active_scan_cancel_ms={}", start.elapsed().as_millis());
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[test]
fn move_does_not_overwrite_and_numbering_preserves_both_files() {
    let temp = temp();
    let dest = temp.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let source = temp.path().join("file.txt");
    fs::write(&source, b"new").unwrap();
    fs::write(dest.join("file.txt"), b"old").unwrap();
    let item = entry(&source);
    assert!(
        !operations::move_file(&item, &dest, ConflictPolicy::Skip, &Cancellation::default())
            .unwrap()
    );
    assert!(source.exists());
    assert_eq!(fs::read(dest.join("file.txt")).unwrap(), b"old");
    assert!(
        operations::move_file(
            &item,
            &dest,
            ConflictPolicy::Number,
            &Cancellation::default()
        )
        .unwrap()
    );
    assert!(!source.exists());
    assert_eq!(fs::read(dest.join("file (1).txt")).unwrap(), b"new");
}
#[test]
fn modified_and_symlink_substituted_sources_are_rejected() {
    let temp = temp();
    let file = temp.path().join("file");
    fs::write(&file, b"original").unwrap();
    let item = entry(&file);
    fs::write(&file, b"changed").unwrap();
    assert!(operations::verify(&item).is_err());
    fs::remove_file(&file).unwrap();
    symlink("/etc/passwd", &file).unwrap();
    assert!(operations::verify(&item).is_err());
}
#[test]
fn recursive_cleanup_never_follows_links() {
    let temp = temp();
    let cache = temp.path().join("cache");
    let keep = temp.path().join("keep");
    fs::create_dir_all(cache.join("nested")).unwrap();
    fs::create_dir(&keep).unwrap();
    fs::write(keep.join("important"), b"keep").unwrap();
    fs::write(cache.join("nested/data"), b"cache").unwrap();
    symlink(&keep, cache.join("link")).unwrap();
    let item = entry(&cache);
    operations::remove_tree(&cache, &item.fingerprint, &Cancellation::default()).unwrap();
    assert!(!cache.exists());
    assert_eq!(fs::read(keep.join("important")).unwrap(), b"keep");
}
#[test]
fn changed_cleanup_root_and_symlink_ancestor_are_rejected() {
    let temp = temp();
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    let item = entry(&cache);
    fs::rename(&cache, temp.path().join("old")).unwrap();
    fs::create_dir(&cache).unwrap();
    assert!(operations::remove_tree(&cache, &item.fingerprint, &Cancellation::default()).is_err());
    symlink(&cache, temp.path().join("alias")).unwrap();
    assert!(operations::open_directory(&temp.path().join("alias")).is_err());
}
#[test]
fn helper_protocol_rejects_paths_versions_and_unsafe_retention() {
    let mut request = PrivilegedRequest::new(SystemRule::AptPackages, 30);
    if cfg!(target_os = "linux") {
        assert!(request.validate().is_ok());
    }
    request.path = Some("/home".into());
    assert!(request.validate().is_err());
    request.path = None;
    request.version = 2;
    assert!(request.validate().is_err());
    request.version = 1;
    request.retention_days = 0;
    assert!(request.validate().is_err());
    assert!(serde_json::from_str::<PrivilegedRequest>(r#"{"version":1,"rule":"AptPackages","retention_days":30,"path":null,"fingerprint":null,"command":"rm"}"#).is_err());
}

#[test]
fn cancellation_preserves_cache_and_measure_deduplicates_hard_links() {
    let tmp = temp();
    let cache = tmp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("one"), [1; 8192]).unwrap();
    fs::hard_link(cache.join("one"), cache.join("two")).unwrap();
    let measured = scan::measure(&cache, &Cancellation::default()).unwrap();
    assert_eq!(
        measured.0,
        fs::metadata(cache.join("one")).unwrap().blocks() * 512
    );
    let token = Cancellation::default();
    token.cancel();
    assert!(operations::remove_tree(&cache, &entry(&cache).fingerprint, &token).is_err());
    assert!(cache.join("one").exists());
    assert!(cache.join("two").exists());
}

#[test]
fn scan_reports_unreadable_directories_without_losing_accessible_files() {
    use std::os::unix::fs::PermissionsExt;
    // Root is not subject to ordinary mode-bit access checks.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let tmp = temp();
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("visible"), b"keep").unwrap();
    fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
    let progress = scan_root(&root, &tmp.path().join("scan.sqlite"));
    fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(progress.files, 1);
    assert_eq!(progress.errors, 1);
    assert_eq!(
        index::errors(&tmp.path().join("scan.sqlite"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn real_cargo_separate_build_directory_is_discovered_and_cleaned() {
    let tmp = temp();
    let root = tmp.path().join("workspace");
    fs::create_dir_all(root.join("a/src")).unwrap();
    fs::create_dir_all(root.join("b/src")).unwrap();
    fs::create_dir(root.join(".cargo")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['a','b']\nresolver='3'\n",
    )
    .unwrap();
    for name in ["a", "b"] {
        fs::write(
            root.join(name).join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n"),
        )
        .unwrap();
        fs::write(root.join(name).join("src/main.rs"), "fn main() {}\n").unwrap();
    }
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\ntarget-dir='products'\nbuild-dir='intermediate'\n",
    )
    .unwrap();
    let exe = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(exe)
        .args(["build", "--offline", "--workspace"])
        .current_dir(&root)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut projects = vec![];
    cargo::discover(
        vec![
            root.join("Cargo.toml"),
            root.join("a/Cargo.toml"),
            root.join("b/Cargo.toml"),
        ],
        &Cancellation::default(),
        |p| projects.push(p),
    );
    assert_eq!(projects.len(), 1, "{projects:?}");
    let project = &projects[0];
    assert!(project.error.is_none(), "{project:?}");
    assert_eq!(project.build, root.join("intermediate"));
    cargo::clean(project, &Cancellation::default()).unwrap();
    assert!(!root.join("products/debug/a").exists());
    assert!(!root.join("intermediate/debug/deps").exists());
    assert!(root.join("a/src/main.rs").exists());
    assert!(root.join("b/src/main.rs").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn real_trash_roundtrip_uses_isolated_user_directories() {
    const CHILD: &str = "RSCLEAR_TRASH_TEST_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let source = root.join("original.bin");
        fs::write(&source, b"recoverable").unwrap();
        rsclear::platform::trash_file(&source).unwrap();
        assert!(!source.exists());
        let items: Vec<_> = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|i| i.original_path() == source)
            .collect();
        assert_eq!(items.len(), 1);
        trash::os_limited::restore_all(items).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"recoverable");
        return;
    }
    let tmp = temp();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "real_trash_roundtrip_uses_isolated_user_directories",
            "--nocapture",
        ])
        .env(CHILD, tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn real_cargo_workspace_custom_target_cleanup_preserves_source_and_rebuilds() {
    let temp = temp();
    // Legitimate source paths must not be rejected just because an ancestor has
    // a conventional cache name.
    let root = temp.path().join("registry/target/project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir(root.join(".cargo")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"fixture\"); }",
    )
    .unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\ntarget-dir='custom-target'\n",
    )
    .unwrap();
    let cargo_exe = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let build = || {
        let out = Command::new(&cargo_exe)
            .args(["build", "--offline"])
            .current_dir(&root)
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    build();
    let before = fs::read(root.join("src/main.rs")).unwrap();
    let lock = fs::read(root.join("Cargo.lock")).unwrap();
    let mut projects = vec![];
    cargo::discover(
        vec![root.join("Cargo.toml")],
        &Cancellation::default(),
        |p| projects.push(p),
    );
    assert_eq!(projects.len(), 1);
    assert!(projects[0].error.is_none(), "{:?}", projects[0].error);
    assert_eq!(projects[0].target, root.join("custom-target"));
    cargo::clean(&projects[0], &Cancellation::default()).unwrap();
    assert!(!root.join("custom-target/debug/fixture").exists());
    assert_eq!(fs::read(root.join("src/main.rs")).unwrap(), before);
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock);
    assert!(cargo::validate_build_path(&root, &root).is_err());
    build();
}

#[test]
#[ignore = "requires a project-local mount at RSCLEAR_TEST_DEST on a different filesystem"]
fn real_cross_device_move_conflicts_metadata_and_disk_full() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = temp();
    let mount =
        PathBuf::from(std::env::var_os("RSCLEAR_TEST_DEST").expect("set RSCLEAR_TEST_DEST"));
    assert!(mount.starts_with(Path::new(env!("CARGO_MANIFEST_DIR"))));
    let destination = tempfile::tempdir_in(mount).unwrap();
    assert_ne!(
        fs::metadata(tmp.path()).unwrap().dev(),
        fs::metadata(destination.path()).unwrap().dev()
    );
    let source = tmp.path().join("data.bin");
    fs::write(&source, vec![13u8; 1024 * 1024]).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    xattr::set(&source, "user.rsclear", b"attribute").unwrap();
    fs::write(destination.path().join("data.bin"), b"existing").unwrap();
    let original = entry(&source);
    assert!(
        !operations::move_file(
            &original,
            destination.path(),
            ConflictPolicy::Skip,
            &Cancellation::default()
        )
        .unwrap()
    );
    assert!(
        operations::move_file(
            &original,
            destination.path(),
            ConflictPolicy::Number,
            &Cancellation::default()
        )
        .unwrap()
    );
    assert!(!source.exists());
    let moved = destination.path().join("data (1).bin");
    assert_eq!(fs::read(&moved).unwrap(), vec![13u8; 1024 * 1024]);
    assert_eq!(
        fs::metadata(&moved).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        xattr::get(&moved, "user.rsclear").unwrap(),
        Some(b"attribute".to_vec())
    );
    assert_eq!(
        fs::read(destination.path().join("data.bin")).unwrap(),
        b"existing"
    );
    let large = tmp.path().join("too-large");
    fs::write(&large, vec![37u8; 16 * 1024 * 1024]).unwrap();
    let result = operations::move_file(
        &entry(&large),
        destination.path(),
        ConflictPolicy::Skip,
        &Cancellation::default(),
    );
    assert!(result.is_err(), "use an 8 MiB destination filesystem");
    assert_eq!(fs::metadata(&large).unwrap().len(), 16 * 1024 * 1024);
    assert!(!destination.path().join("too-large").exists());
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
}
