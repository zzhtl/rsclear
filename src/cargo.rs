use crate::{
    model::{Cancellation, Fingerprint},
    platform, scan,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct RustProject {
    pub manifest: PathBuf,
    pub workspaces: Vec<PathBuf>,
    pub target: PathBuf,
    pub build: PathBuf,
    pub allocated: u64,
    pub newest: i64,
    pub snapshots: Vec<(PathBuf, Fingerprint)>,
    pub error: Option<String>,
}
#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    build_directory: Option<PathBuf>,
}
fn metadata(cargo: &Path, manifest: &Path, cancel: &Cancellation) -> Result<Metadata> {
    let mut args = platform::args(&[
        "metadata",
        "--no-deps",
        "--format-version",
        "1",
        "--frozen",
        "--manifest-path",
    ]);
    args.push(manifest.as_os_str().into());
    let output = platform::run(
        cargo,
        &args,
        manifest.parent(),
        cancel,
        Duration::from_secs(30),
    )?;
    serde_json::from_str(&output).context("Cargo 元数据解析失败")
}
pub fn validate_build_path(path: &Path, workspace: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("构建目录不存在或不可访问: {}", path.display()))?;
    let workspace = fs::canonicalize(workspace)?;
    if workspace.starts_with(&path) || path.components().count() < 3 {
        bail!("构建目录与源码或系统根目录重叠");
    }
    if [
        "/etc", "/usr", "/bin", "/sbin", "/lib", "/System", "/Library", "/proc", "/sys", "/dev",
        "/run",
    ]
    .iter()
    .any(|p| path.starts_with(p))
    {
        bail!("拒绝清理受保护目录");
    }
    if let Some(home) = directories::BaseDirs::new()
        && (home.home_dir().starts_with(&path)
            || path.starts_with(home.home_dir().join(".cargo"))
            || path.starts_with(home.home_dir().join(".ssh"))
            || path.starts_with(home.home_dir().join(".rustup")))
    {
        bail!("拒绝清理用户根目录或工具链");
    }
    if path.join("Cargo.toml").exists() {
        bail!("构建目录内含项目清单，拒绝整目录清理");
    }
    if !path.join("CACHEDIR.TAG").is_file() && !path.join(".rustc_info.json").is_file() {
        bail!("目录缺少 Cargo 构建标记，不能确认归属");
    }
    Ok(path)
}
pub fn discover(manifests: Vec<PathBuf>, cancel: &Cancellation, mut emit: impl FnMut(RustProject)) {
    let cargo = platform::executable("cargo");
    let mut projects: HashMap<PathBuf, RustProject> = HashMap::new();
    let mut seen_workspaces = HashSet::new();
    for manifest in manifests {
        if cancel.is_cancelled() {
            break;
        }
        // Cache markers identify disposable trees; a directory merely named
        // "target" or "registry" can still contain a real source workspace.
        if manifest
            .components()
            .any(|p| matches!(p.as_os_str().to_str(), Some(".git" | "node_modules")))
            || manifest
                .ancestors()
                .skip(1)
                .any(|p| p.join("CACHEDIR.TAG").is_file())
        {
            continue;
        }
        let found = (|| -> Result<Option<RustProject>> {
            let cargo = cargo
                .as_deref()
                .context("未安装 Cargo，不能确认构建目录归属")?;
            let metadata = metadata(cargo, &manifest, cancel)?;
            if !seen_workspaces.insert(metadata.workspace_root.clone()) {
                return Ok(None);
            }
            let target = validate_build_path(&metadata.target_directory, &metadata.workspace_root)?;
            let build = metadata
                .build_directory
                .as_deref()
                .map(|p| validate_build_path(p, &metadata.workspace_root))
                .transpose()?
                .unwrap_or_else(|| target.clone());
            if let Some(previous) = projects.get_mut(&target) {
                if previous.build != build {
                    previous.error =
                        Some("共享 target 的中间构建目录不一致，请单独扫描对应工作区".into());
                    bail!("共享 target 的中间构建目录不一致，请单独扫描对应工作区");
                }
                previous.workspaces.push(metadata.workspace_root);
                return Ok(None);
            }
            let mut snapshots = vec![(
                target.clone(),
                Fingerprint::from(&fs::symlink_metadata(&target)?),
            )];
            let (mut allocated, mut newest) = scan::measure(&target, cancel)?;
            if build != target && !build.starts_with(&target) {
                let (bytes, time) = scan::measure(&build, cancel)?;
                if target.starts_with(&build) {
                    allocated = bytes;
                } else {
                    allocated += bytes;
                }
                newest = newest.max(time);
                snapshots.push((
                    build.clone(),
                    Fingerprint::from(&fs::symlink_metadata(&build)?),
                ));
            }
            Ok(Some(RustProject {
                manifest: metadata.workspace_root.join("Cargo.toml"),
                workspaces: vec![metadata.workspace_root],
                target,
                build,
                allocated,
                newest,
                snapshots,
                error: None,
            }))
        })();
        match found {
            Ok(Some(project)) => {
                projects.insert(project.target.clone(), project);
            }
            Ok(None) => (),
            Err(e) => emit(RustProject {
                manifest: manifest.clone(),
                workspaces: vec![],
                target: manifest.parent().unwrap_or(Path::new("/")).join("target"),
                build: PathBuf::new(),
                allocated: 0,
                newest: 0,
                snapshots: vec![],
                error: Some(format!("{e:#}")),
            }),
        }
    }
    for (_, project) in projects {
        emit(project);
    }
}
pub fn clean(project: &RustProject, cancel: &Cancellation) -> Result<String> {
    if project.error.is_some() {
        bail!("无法确认的构建目录不能清理");
    }
    let cargo = platform::executable("cargo").context("找不到 Cargo")?;
    let current = metadata(&cargo, &project.manifest, cancel)?;
    let target = validate_build_path(&current.target_directory, &current.workspace_root)?;
    let build = current
        .build_directory
        .as_deref()
        .map(|p| validate_build_path(p, &current.workspace_root))
        .transpose()?
        .unwrap_or_else(|| target.clone());
    if target != project.target || build != project.build {
        bail!("Cargo 配置已变化，请重新扫描");
    }
    for (path, expected) in &project.snapshots {
        let actual = Fingerprint::from(&fs::symlink_metadata(path)?);
        if actual.dev != expected.dev || actual.ino != expected.ino {
            bail!("构建目录已被替换");
        }
    }
    let mut args = platform::args(&["clean", "--frozen", "--manifest-path"]);
    args.push(project.manifest.as_os_str().into());
    args.push("--target-dir".into());
    args.push(target.into_os_string());
    // Pin both destinations to the reviewed plan, even if a config changes after metadata.
    args.push("--config".into());
    args.push(
        format!(
            "build.build-dir={}",
            serde_json::to_string(&build.to_string_lossy())?
        )
        .into(),
    );
    platform::run(
        &cargo,
        &args,
        project.manifest.parent(),
        cancel,
        Duration::from_secs(3600),
    )
}
