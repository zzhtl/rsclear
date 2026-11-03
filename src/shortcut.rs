//! User-owned desktop launchers. Existing entries are never replaced.
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub enum Outcome {
    Created { path: PathBuf, needs_trust: bool },
    Present,
    NoDesktop,
}

pub fn ensure() -> Result<Outcome> {
    let Some(user) = directories::UserDirs::new() else {
        return Ok(Outcome::NoDesktop);
    };
    let Some(desktop) = user.desktop_dir() else {
        return Ok(Outcome::NoDesktop);
    };
    // XDG allows disabling the desktop by pointing it at the home directory.
    if desktop == user.home_dir() || !desktop.is_absolute() || !desktop.is_dir() {
        return Ok(Outcome::NoDesktop);
    }
    let executable = std::env::current_exe().context("无法确定当前程序路径")?;
    let data = crate::settings::AppPaths::discover()?.data;
    let Some(path) = install(&executable, desktop, &data)? else {
        return Ok(Outcome::Present);
    };
    #[cfg(target_os = "linux")]
    let needs_trust = crate::platform::run(
        Path::new("/usr/bin/gio"),
        &[
            "set".into(),
            "-t".into(),
            "string".into(),
            path.as_os_str().into(),
            "metadata::trusted".into(),
            "true".into(),
        ],
        None,
        &crate::model::Cancellation::default(),
        std::time::Duration::from_secs(2),
    )
    .is_err();
    #[cfg(target_os = "macos")]
    let needs_trust = false;
    Ok(Outcome::Created { path, needs_trust })
}

fn occupied(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true), // Includes broken links: they still belong to the user.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn write_new(path: &Path, contents: &[u8], mode: u32) -> Result<bool> {
    let parent = path.parent().context("快捷方式路径没有父目录")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error.into()),
    }
}

#[cfg(target_os = "linux")]
fn install(executable: &Path, desktop: &Path, data: &Path) -> Result<Option<PathBuf>> {
    let destination = desktop.join("rsclear.desktop");
    if occupied(&destination)? {
        return Ok(None);
    }
    let executable = path_text(executable)?;
    let icon = data.join("rsclear-desktop.svg");
    let icon_value = path_text(&icon)?.replace('\\', "\\\\");
    let argument = exec_argument(executable);
    // Some launchers check Exec's program before expanding literal percent signs;
    // the specification also forbids '=' in that program. Use a fixed shell
    // command with the executable as a positional argument for these names only.
    let command = if executable.contains(['%', '=']) {
        format!(
            "/bin/sh -c {} rsclear-shortcut {argument}",
            exec_argument("exec \"$1\"")
        )
    } else {
        argument
    };
    let contents = include_str!("../packaging/rsclear.desktop")
        .replace("Exec=rsclear\n", &format!("Exec={command}\n"))
        .replace("Icon=rsclear\n", &format!("Icon={icon_value}\n"));
    fs::create_dir_all(data)?;
    if !occupied(&icon)? {
        write_new(&icon, include_bytes!("../packaging/rsclear.svg"), 0o644)?;
    }
    Ok(write_new(&destination, contents.as_bytes(), 0o755)?.then_some(destination))
}

fn path_text(path: &Path) -> Result<&str> {
    let value = path.to_str().context("快捷方式不支持非 UTF-8 路径")?;
    if !path.is_absolute() || value.chars().any(char::is_control) {
        bail!("快捷方式需要不含控制字符的绝对路径");
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
fn exec_argument(path: &str) -> String {
    let mut argument = String::from("\"");
    for ch in path.chars() {
        match ch {
            '"' | '`' | '$' | '\\' => {
                argument.push('\\');
                argument.push(ch);
            }
            '%' => argument.push_str("%%"),
            _ => argument.push(ch),
        }
    }
    argument.push('"');
    // Desktop string unescaping happens before Exec argument unquoting.
    argument.replace('\\', "\\\\")
}

#[cfg(any(target_os = "macos", test))]
fn app_bundle(executable: &Path) -> Option<&Path> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app"
        && contents.join("Info.plist").is_file())
    .then_some(bundle)
}

#[cfg(any(target_os = "macos", test))]
fn write_mac_launcher(bundle: &Path, executable: &Path) -> Result<()> {
    let executable = path_text(executable)?;
    let macos = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos)?;
    let script = format!("#!/bin/sh\nexec '{}'\n", executable.replace('\'', "'\\''"));
    write_new(&macos.join("launch"), script.as_bytes(), 0o755)?;
    write_new(
        &bundle.join("Contents/Info.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>launch</string>
<key>CFBundleIdentifier</key><string>cn.rsclear.desktop.shortcut</string>
<key>CFBundleName</key><string>rsclear</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
"#,
        0o644,
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install(executable: &Path, desktop: &Path, data: &Path) -> Result<Option<PathBuf>> {
    let destination = desktop.join("rsclear.app");
    if occupied(&destination)? {
        return Ok(None);
    }
    let mut wrapper = None;
    let target = if let Some(bundle) = app_bundle(executable) {
        bundle.to_path_buf()
    } else {
        // Finder needs an application bundle even for a directly launched Mach-O binary.
        fs::create_dir_all(data)?;
        let temporary = tempfile::Builder::new()
            .prefix("desktop-launcher-")
            .tempdir_in(data)?;
        let bundle = temporary.path().join("rsclear.app");
        write_mac_launcher(&bundle, executable)?;
        wrapper = Some(temporary);
        bundle
    };
    match std::os::unix::fs::symlink(target, &destination) {
        Ok(()) => {
            if let Some(wrapper) = wrapper {
                let _ = wrapper.keep();
            }
            Ok(Some(destination))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::symlink, process::Command};

    fn temporary() -> tempfile::TempDir {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".build/tmp");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn startup_creation_is_idempotent_and_preserves_existing_entries() {
        let tmp = temporary();
        let executable = tmp.path().join("rsclear");
        fs::write(&executable, b"fixture").unwrap();
        let desktop = tmp.path().join("桌面");
        fs::create_dir(&desktop).unwrap();
        let data = tmp.path().join("data");
        let shortcut = install(&executable, &desktop, &data).unwrap().unwrap();
        let metadata = fs::symlink_metadata(&shortcut).unwrap();
        assert!(install(&executable, &desktop, &data).unwrap().is_none());
        assert_eq!(
            metadata.modified().unwrap(),
            fs::symlink_metadata(&shortcut).unwrap().modified().unwrap()
        );
        fs::remove_file(&shortcut).unwrap();
        fs::write(&shortcut, b"user custom entry").unwrap();
        assert!(install(&executable, &desktop, &data).unwrap().is_none());
        assert_eq!(fs::read(&shortcut).unwrap(), b"user custom entry");
        fs::remove_file(&shortcut).unwrap();
        symlink(tmp.path().join("missing"), &shortcut).unwrap();
        assert!(install(&executable, &desktop, &data).unwrap().is_none());
        assert_eq!(
            fs::read_link(&shortcut).unwrap(),
            tmp.path().join("missing")
        );
    }

    #[test]
    fn recognizes_app_bundles_and_builds_a_launchable_wrapper_for_bare_binaries() {
        let tmp = temporary();
        let executable = tmp.path().join("rsclear 空格 '\"$`% binary");
        fs::write(&executable, b"#!/bin/sh\nprintf '%s' \"$0\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let bundle = tmp.path().join("rsclear.app");
        write_mac_launcher(&bundle, &executable).unwrap();
        let launcher = bundle.join("Contents/MacOS/launch");
        assert_eq!(app_bundle(&launcher), Some(bundle.as_path()));
        assert_eq!(app_bundle(&executable), None);
        let output = Command::new(&launcher).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, executable.as_os_str().as_encoded_bytes());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_entry_uses_absolute_paths_and_rejects_unrepresentable_paths() {
        use std::os::unix::ffi::OsStringExt;
        let tmp = temporary();
        let data = tmp.path().join("data");
        let path = install(&tmp.path().join("rsclear 空格"), tmp.path(), &data)
            .unwrap()
            .unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains(&format!("Exec=\"{}/rsclear 空格\"", tmp.path().display())));
        assert!(contents.contains(&format!("Icon={}/rsclear-desktop.svg", data.display())));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_file(&path).unwrap();
        for name in [
            std::ffi::OsString::from("bad\nExec=other"),
            std::ffi::OsString::from_vec(vec![0xff]),
        ] {
            assert!(install(&tmp.path().join(name), tmp.path(), &data).is_err());
            assert!(!path.exists());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires desktop-file-validate and Python GObject Gio bindings"]
    fn generated_entry_launches_literal_special_character_path_using_gio() {
        for name in [
            "rsclear 空格",
            "rsclear'quote",
            "rsclear\"quote",
            "rsclear$dollar",
            "rsclear`tick",
            "rsclear\\slash",
            "rsclear%%percent",
            "rsclear%f=path",
            "rsclear 空格 '\"\\$`%%;binary",
        ] {
            let tmp = temporary();
            let executable = tmp.path().join(name);
            fs::write(
                &executable,
                b"#!/bin/sh\nprintf '%s' \"$0\" > \"$RSCLEAR_SHORTCUT_TEST_RESULT\"\n",
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let shortcut = install(&executable, tmp.path(), &tmp.path().join("data"))
                .unwrap()
                .unwrap();
            assert!(
                Command::new("desktop-file-validate")
                    .arg(&shortcut)
                    .status()
                    .unwrap()
                    .success()
            );
            let result = tmp.path().join("result");
            let script = "import sys,time\nfrom gi.repository import Gio\nfrom pathlib import Path\napp=Gio.DesktopAppInfo.new_from_filename(sys.argv[1])\nassert app is not None\nassert app.launch([],None)\nfor _ in range(100):\n if Path(sys.argv[2]).exists(): break\n time.sleep(0.02)\nassert Path(sys.argv[2]).exists()\n";
            let output = Command::new("/usr/bin/python3")
                .args(["-c", script])
                .arg(&shortcut)
                .arg(&result)
                .env("RSCLEAR_SHORTCUT_TEST_RESULT", &result)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "case {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                fs::read(result).unwrap(),
                executable.as_os_str().as_encoded_bytes()
            );
        }
    }
}
