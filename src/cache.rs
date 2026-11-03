use crate::{
    model::{Cancellation, Fingerprint},
    operations, platform, scan,
    system::{PrivilegedRequest, SystemRule},
};
use anyhow::{Context, Result, bail};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Npm,
    Pnpm,
    Pip,
    Homebrew,
}
#[derive(Debug, Clone)]
pub enum CacheAction {
    Directory {
        root: PathBuf,
        fingerprint: Fingerprint,
    },
    Tool {
        tool: Tool,
        path: PathBuf,
    },
    System(PrivilegedRequest),
    Unavailable,
}
#[derive(Debug, Clone)]
pub struct CleanupCandidate {
    pub name: String,
    pub path: PathBuf,
    pub bytes: Option<u64>,
    pub description: String,
    pub action: CacheAction,
    pub error: Option<String>,
}
fn home() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("无法确定主目录")?
        .home_dir()
        .to_path_buf())
}
fn tool_location(tool: Tool, cancel: &Cancellation) -> Result<(PathBuf, PathBuf)> {
    let (name, args) = match tool {
        Tool::Npm => ("npm", platform::args(&["config", "get", "cache"])),
        Tool::Pnpm => ("pnpm", platform::args(&["store", "path"])),
        Tool::Pip => ("python3", platform::args(&["-m", "pip", "cache", "dir"])),
        Tool::Homebrew => ("brew", platform::args(&["--cache"])),
    };
    let executable = platform::executable(name).with_context(|| format!("未安装 {name}"))?;
    let output = platform::run(
        &executable,
        &args,
        Some(&home()?),
        cancel,
        Duration::from_secs(20),
    )?;
    let path = PathBuf::from(output.trim());
    if !path.is_absolute() || path.components().count() < 3 {
        bail!("工具返回的缓存路径无效");
    }
    if home()?.starts_with(&path)
        || path.join("Cargo.toml").is_file()
        || path.join("package.json").is_file()
    {
        bail!("工具缓存路径与用户数据根目录重叠");
    }
    let path = fs::canonicalize(path)?;
    if tool == Tool::Homebrew {
        let base = directories::BaseDirs::new().context("无法确定缓存目录")?;
        let standard = fs::canonicalize(base.cache_dir().join("Homebrew"))?;
        if path != standard {
            bail!("自定义 Homebrew 缓存目录暂不支持自动清理");
        }
    }
    Ok((executable, path))
}
fn known_user_cache(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(
            "thumbnails"
                | "fontconfig"
                | "mesa_shader_cache"
                | "mesa_shader_cache_db"
                | "google-chrome"
                | "chromium"
                | "mozilla"
                | "JetBrains"
                | "com.google.Chrome"
                | "org.mozilla.firefox"
                | "com.microsoft.VSCode"
                | "com.jetbrains.rustrover"
                | "com.jetbrains.intellij"
        )
    )
}
pub fn discover(
    days: u32,
    cancel: &Cancellation,
    mut emit: impl FnMut(CleanupCandidate),
) -> Result<()> {
    let base = directories::BaseDirs::new().context("无法确定用户目录")?;
    let cache_root = fs::canonicalize(base.cache_dir()).unwrap_or_else(|_| base.cache_dir().into());
    let mut claimed = HashSet::new();
    for tool in [Tool::Npm, Tool::Pnpm, Tool::Pip, Tool::Homebrew] {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let result = tool_location(tool, cancel).and_then(|(_, path)| {
            let (bytes, _) = scan::measure(&path, cancel)?;
            Ok((path, bytes))
        });
        match result {
            Ok((path, bytes)) => {
                claimed.insert(path.clone());
                emit(CleanupCandidate {
                    name: format!("{tool:?} 下载缓存"),
                    path: path.clone(),
                    bytes: Some(bytes),
                    description: if tool == Tool::Pnpm {
                        "仅裁剪无引用包；占用不等于可释放量"
                    } else {
                        "清理后需要重新下载缓存；不卸载软件"
                    }
                    .into(),
                    action: CacheAction::Tool { tool, path },
                    error: None,
                });
            }
            Err(e) => emit(CleanupCandidate {
                name: format!("{tool:?}"),
                path: PathBuf::new(),
                bytes: None,
                description: "开发工具缓存".into(),
                action: CacheAction::Unavailable,
                error: Some(format!("{e:#}")),
            }),
        }
    }
    if let Ok(entries) = fs::read_dir(&cache_root) {
        for entry in entries {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let entry = entry?;
            let path = entry.path();
            if claimed
                .iter()
                .any(|p| p.starts_with(&path) || path.starts_with(p))
                || path.file_name().and_then(|n| n.to_str()) == Some("rsclear")
            {
                continue;
            }
            let recognized = known_user_cache(&path);
            let measure = scan::measure(&path, cancel);
            let (bytes, error) = match measure {
                Ok((bytes, _)) => (Some(bytes), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            };
            let action = if recognized && error.is_none() {
                CacheAction::Directory {
                    root: cache_root.clone(),
                    fingerprint: Fingerprint::from(&fs::symlink_metadata(&path)?),
                }
            } else {
                CacheAction::Unavailable
            };
            emit(CleanupCandidate {
                name: entry.file_name().to_string_lossy().into(),
                path,
                bytes,
                description: if recognized {
                    "已识别应用缓存；清理前请关闭对应应用"
                } else {
                    "未识别内容，仅展示占用"
                }
                .into(),
                action,
                error,
            });
        }
    }
    #[cfg(target_os = "linux")]
    for (name, rule, path, program, description) in [
        (
            "APT 下载包缓存",
            SystemRule::AptPackages,
            "/var/cache/apt/archives",
            "/usr/bin/apt-get",
            "使用 apt-get clean；不卸载软件",
        ),
        (
            "系统归档日志",
            SystemRule::Journal,
            "/var/log/journal",
            "/usr/bin/journalctl",
            "仅清理已归档日志；不强制轮转活动日志",
        ),
        (
            "系统过期临时文件",
            SystemRule::TemporaryFiles,
            "/tmp 和 /var/tmp",
            "/usr/bin/systemd-tmpfiles",
            "遵循系统已有过期规则；执行后报告结果",
        ),
    ] {
        let available = Path::new(program).is_file();
        emit(CleanupCandidate {
            name: name.into(),
            path: path.into(),
            bytes: None,
            description: description.into(),
            action: if available {
                CacheAction::System(PrivilegedRequest::new(rule, days))
            } else {
                CacheAction::Unavailable
            },
            error: (!available).then(|| "系统未提供对应工具".into()),
        });
    }
    #[cfg(target_os = "macos")]
    for (root, rule) in [
        ("/Library/Caches", SystemRule::MacApplicationCache),
        (
            "/Library/Logs/DiagnosticReports",
            SystemRule::MacDiagnosticReport,
        ),
    ] {
        match fs::read_dir(root) {
            Ok(entries) => {
                for entry in entries {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    let path = entry?.path();
                    if (rule == SystemRule::MacApplicationCache
                        && !crate::system::known_mac_cache(&path))
                        || (rule == SystemRule::MacDiagnosticReport
                            && !crate::system::old_report(&path, days)?)
                    {
                        continue;
                    }
                    let mut request = PrivilegedRequest::new(rule.clone(), days);
                    request.fingerprint = Some(Fingerprint::from(&fs::symlink_metadata(&path)?));
                    request.path = Some(path.clone());
                    emit(CleanupCandidate {
                        name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into(),
                        path: path.clone(),
                        bytes: scan::measure(&path, cancel).ok().map(|m| m.0),
                        description: "系统应用缓存或过期诊断报告；按需管理员授权".into(),
                        action: CacheAction::System(request),
                        error: None,
                    });
                }
            }
            Err(e) => emit(CleanupCandidate {
                name: "系统缓存/报告".into(),
                path: root.into(),
                bytes: None,
                description: "请在系统设置中检查文件访问权限".into(),
                action: CacheAction::Unavailable,
                error: Some(e.to_string()),
            }),
        }
    }
    Ok(())
}
pub fn clean(candidate: &CleanupCandidate, cancel: &Cancellation) -> Result<String> {
    if candidate.error.is_some() {
        bail!("此项目存在扫描错误，请重新检查");
    }
    match &candidate.action {
        CacheAction::Directory { root, fingerprint } => {
            if candidate.path.parent() != Some(root) || !known_user_cache(&candidate.path) {
                bail!("缓存目录越界或未识别");
            }
            operations::remove_tree(&candidate.path, fingerprint, cancel)?;
            Ok("缓存已清理；占用变化可能受打开文件、硬链接及快照影响".into())
        }
        CacheAction::Tool { tool, path } => {
            let (executable, actual) = tool_location(*tool, cancel)?;
            if &actual != path {
                bail!("工具缓存位置已变化，请重新扫描");
            }
            let mut args = match tool {
                Tool::Npm => platform::args(&["cache", "clean", "--force"]),
                Tool::Pnpm => platform::args(&["store", "prune"]),
                Tool::Pip => platform::args(&["-m", "pip", "cache", "purge"]),
                Tool::Homebrew => {
                    let fp = Fingerprint::from(&fs::symlink_metadata(path)?);
                    operations::remove_tree(path, &fp, cancel)?;
                    return Ok("Homebrew 下载缓存已清理，已安装软件保持不变".into());
                }
            };
            let pinned_path = if *tool == Tool::Pnpm {
                // `store path` includes pnpm's store-version suffix. Passing that
                // path as store-dir can append the suffix again. Verify the base
                // with pnpm itself before invoking a destructive command.
                let base = path.parent().context("pnpm store 路径没有父目录")?;
                let mut check = platform::args(&["store", "path", "--store-dir"]);
                check.push(base.as_os_str().into());
                let resolved = platform::run(
                    &executable,
                    &check,
                    Some(&home()?),
                    cancel,
                    Duration::from_secs(20),
                )?;
                if fs::canonicalize(resolved.trim())? != *path {
                    bail!("此 pnpm 版本无法固定已确认的 store 路径，未执行清理");
                }
                base
            } else {
                path.as_path()
            };
            args.push(
                match tool {
                    Tool::Npm => "--cache",
                    Tool::Pnpm => "--store-dir",
                    Tool::Pip => "--cache-dir",
                    Tool::Homebrew => unreachable!("Homebrew returns before invoking a command"),
                }
                .into(),
            );
            args.push(pinned_path.as_os_str().into());
            platform::run(
                &executable,
                &args,
                Some(&home()?),
                cancel,
                Duration::from_secs(3600),
            )
        }
        CacheAction::System(request) => platform::privileged(request, cancel),
        CacheAction::Unavailable => bail!("此项目暂不支持自动清理"),
    }
}
