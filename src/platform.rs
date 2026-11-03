use crate::model::Cancellation;
use anyhow::{Context, Result, bail};
use std::{
    ffi::{OsStr, OsString},
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn executable(name: &str) -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    if let Some(home) = directories::BaseDirs::new() {
        paths.push(home.home_dir().join(".cargo/bin"));
        paths.push(home.home_dir().join(".local/bin"));
    }
    paths
        .into_iter()
        .map(|p| p.join(name))
        .find(|p| p.is_file())
}
pub fn run(
    program: &Path,
    args: &[OsString],
    cwd: Option<&Path>,
    cancel: &Cancellation,
    timeout: Duration,
) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // Discovery must never trigger a rustup toolchain download.
    command.env("RUSTUP_AUTO_INSTALL", "0");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("无法启动 {}", program.display()))?;
    let stdout = child.stdout.take().context("无法读取命令输出")?;
    let stderr = child.stderr.take().context("无法读取命令错误")?;
    fn capture(mut input: impl Read) -> Vec<u8> {
        let mut result = Vec::new();
        let mut buffer = [0u8; 8192];
        while let Ok(n) = input.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let remaining = (4 * 1024 * 1024usize).saturating_sub(result.len());
            result.extend_from_slice(&buffer[..n.min(remaining)]);
        }
        result
    }
    let out = thread::spawn(move || capture(stdout));
    let err = thread::spawn(move || capture(stderr));
    let start = Instant::now();
    let mut interrupted = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.is_cancelled() || start.elapsed() > timeout {
            interrupted = Some(if cancel.is_cancelled() {
                "操作已取消，已完成的改动不会回滚"
            } else {
                "命令超时"
            });
            // SAFETY: the child owns this freshly created process group; no user-supplied pid is used.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let output = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let error = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if let Some(message) = interrupted {
        bail!("{message}");
    }
    if !status.success() {
        bail!("{}: {} {}", program.display(), status, error.trim());
    }
    Ok(if output.trim().is_empty() {
        error
    } else {
        output
    })
}
pub fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
pub fn reveal(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/open").arg("-R").arg(path).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn()?;
    }
    Ok(())
}
pub fn trash_file(path: &Path) -> Result<()> {
    trash::delete(path)
        .with_context(|| format!("移入废纸篓失败（不会永久删除）: {}", path.display()))
}

pub fn privileged(
    request: &crate::system::PrivilegedRequest,
    cancel: &Cancellation,
) -> Result<String> {
    request.validate()?;
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "linux")]
    let helper = if Path::new("/usr/libexec/rsclear-helper").is_file() {
        PathBuf::from("/usr/libexec/rsclear-helper")
    } else {
        executable.with_file_name("rsclear-helper")
    };
    #[cfg(target_os = "macos")]
    let helper = executable.with_file_name("rsclear-helper");
    if !helper.is_file() {
        bail!("找不到权限辅助程序，请安装完整应用包");
    }
    let request = serde_json::to_string(request)?;
    #[cfg(target_os = "linux")]
    {
        run(
            Path::new("/usr/bin/pkexec"),
            &[
                OsString::from("--disable-internal-agent"),
                helper.into_os_string(),
                request.into(),
            ],
            None,
            cancel,
            Duration::from_secs(3600),
        )
    }
    #[cfg(target_os = "macos")]
    {
        // AppleScript quotes the argv values; JSON and paths are never interpolated into script code.
        let script = "on run argv\nset cmd to quoted form of (item 1 of argv) & \" \" & quoted form of (item 2 of argv)\ndo shell script cmd with administrator privileges\nend run";
        run(
            Path::new("/usr/bin/osascript"),
            &[
                OsStr::new("-e").into(),
                script.into(),
                helper.into_os_string(),
                request.into(),
            ],
            None,
            cancel,
            Duration::from_secs(3600),
        )
    }
}

pub fn os_arg(value: &OsStr) -> OsString {
    value.to_os_string()
}
