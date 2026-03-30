use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn app_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Headroom")
}

pub fn ensure_data_dirs(base_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("creating app data dir {}", base_dir.display()))?;
    std::fs::create_dir_all(base_dir.join("telemetry"))
        .with_context(|| format!("creating telemetry dir under {}", base_dir.display()))?;
    std::fs::create_dir_all(base_dir.join("config"))
        .with_context(|| format!("creating config dir under {}", base_dir.display()))?;
    Ok(())
}

pub fn config_file(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("config").join(name)
}

pub fn telemetry_file(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("telemetry").join(name)
}
