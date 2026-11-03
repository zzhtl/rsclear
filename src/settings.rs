use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub roots: Vec<PathBuf>,
    pub exclusions: Vec<PathBuf>,
    pub min_mib: u64,
    pub threads: usize,
    pub retention_days: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            roots: directories::BaseDirs::new()
                .map(|d| vec![d.home_dir().to_path_buf()])
                .unwrap_or_default(),
            exclusions: Vec::new(),
            min_mib: 100,
            threads: 4,
            retention_days: 30,
        }
    }
}
#[derive(Clone)]
pub struct AppPaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}
impl AppPaths {
    pub fn discover() -> Result<Self> {
        if let Some(root) = std::env::var_os("RSCLEAR_DATA_DIR") {
            let root = PathBuf::from(root);
            return Ok(Self {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
            });
        }
        let dirs = directories::ProjectDirs::from("cn", "rsclear", "rsclear")
            .context("无法确定用户应用目录")?;
        Ok(Self {
            config: dirs.config_dir().into(),
            data: dirs.data_local_dir().into(),
            cache: dirs.cache_dir().into(),
        })
    }
    pub fn create(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for path in [&self.config, &self.data, &self.cache] {
            std::fs::create_dir_all(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    pub fn load(&self) -> Result<Settings> {
        match std::fs::read(self.config.join("settings.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, settings: &Settings) -> Result<()> {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(&self.config)?;
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.as_file().sync_all()?;
        file.persist(self.config.join("settings.json"))?;
        Ok(())
    }
}
