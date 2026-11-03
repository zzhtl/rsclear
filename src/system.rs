use crate::{
    model::{Cancellation, Fingerprint},
    operations, platform,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SystemRule {
    AptPackages,
    Journal,
    TemporaryFiles,
    MacApplicationCache,
    MacDiagnosticReport,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivilegedRequest {
    pub version: u32,
    pub rule: SystemRule,
    pub retention_days: u32,
    pub path: Option<PathBuf>,
    pub fingerprint: Option<Fingerprint>,
}
impl PrivilegedRequest {
    pub fn new(rule: SystemRule, retention_days: u32) -> Self {
        Self {
            version: 1,
            rule,
            retention_days,
            path: None,
            fingerprint: None,
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || !(7..=3650).contains(&self.retention_days) {
            bail!("不支持的协议版本或保留天数");
        }
        match self.rule {
            SystemRule::AptPackages | SystemRule::Journal | SystemRule::TemporaryFiles => {
                if !cfg!(target_os = "linux") || self.path.is_some() || self.fingerprint.is_some() {
                    bail!("无效的系统清理请求");
                }
            }
            SystemRule::MacApplicationCache | SystemRule::MacDiagnosticReport => {
                if !cfg!(target_os = "macos") {
                    bail!("此规则仅适用于 macOS");
                }
                let Some(path) = self.path.as_deref() else {
                    bail!("缺少清理路径");
                };
                if self.fingerprint.is_none() {
                    bail!("缺少清理对象身份");
                }
                let root = if self.rule == SystemRule::MacApplicationCache {
                    Path::new("/Library/Caches")
                } else {
                    Path::new("/Library/Logs/DiagnosticReports")
                };
                if path.parent() != Some(root)
                    || path.file_name().and_then(|n| n.to_str()).is_none()
                {
                    bail!("清理路径越界");
                }
                if self.rule == SystemRule::MacApplicationCache && !known_mac_cache(path) {
                    bail!("不是已识别的第三方应用缓存");
                }
                if self.rule == SystemRule::MacDiagnosticReport
                    && !old_report(path, self.retention_days)?
                {
                    bail!("报告类型或保留时间不符合规则");
                }
            }
        }
        Ok(())
    }
}
pub fn known_mac_cache(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(
            "com.google.Chrome"
                | "org.mozilla.firefox"
                | "com.microsoft.VSCode"
                | "com.jetbrains.intellij"
                | "com.jetbrains.rustrover"
        )
    )
}
pub fn old_report(path: &Path, days: u32) -> Result<bool> {
    if !matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ips" | "crash" | "diag" | "hang")
    ) {
        return Ok(false);
    }
    let m = fs::symlink_metadata(path)?;
    Ok(m.is_file()
        && !m.is_symlink()
        && m.modified()?
            < SystemTime::now()
                .checked_sub(Duration::from_secs(u64::from(days) * 86400))
                .unwrap_or(UNIX_EPOCH))
}
pub fn execute(request: &PrivilegedRequest, cancel: &Cancellation) -> Result<String> {
    request.validate()?;
    let (program, args) = match request.rule {
        SystemRule::AptPackages => ("/usr/bin/apt-get", platform::args(&["clean"])),
        SystemRule::Journal => (
            "/usr/bin/journalctl",
            platform::args(&[
                "--system",
                &format!("--vacuum-time={}d", request.retention_days),
            ]),
        ),
        SystemRule::TemporaryFiles => (
            "/usr/bin/systemd-tmpfiles",
            platform::args(&["--clean", "--prefix=/tmp", "--prefix=/var/tmp"]),
        ),
        SystemRule::MacApplicationCache | SystemRule::MacDiagnosticReport => {
            let (Some(path), Some(fingerprint)) = (&request.path, &request.fingerprint) else {
                bail!("缺少清理对象");
            };
            operations::remove_tree(path, fingerprint, cancel)?;
            return Ok("清理完成".into());
        }
    };
    platform::run(
        Path::new(program),
        &args,
        None,
        cancel,
        Duration::from_secs(3600),
    )
}
