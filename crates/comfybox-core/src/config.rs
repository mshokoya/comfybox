use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub comfy_path: Option<PathBuf>,
    pub hf_endpoint: Option<String>,
    #[serde(default = "default_pypi_index_url")]
    pub pypi_index_url: String,
    #[serde(default = "default_parallelism")]
    pub download_parallelism: usize,
    #[serde(default = "default_chunk_size")]
    pub chunk_size_bytes: u64,
    #[serde(default = "default_concurrent_downloads")]
    pub max_concurrent_downloads: usize,
}

fn default_parallelism() -> usize {
    4
}
fn default_chunk_size() -> u64 {
    16 * 1024 * 1024
}
fn default_concurrent_downloads() -> usize {
    2
}
fn default_pypi_index_url() -> String {
    "https://pypi.org/simple".to_owned()
}

impl AppConfig {
    pub fn config_dir() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("dev", "comfybox", "ComfyBox")
            .context("unable to determine config directory")?;
        Ok(dirs.config_dir().to_path_buf())
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.json"))
    }
    pub fn state_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("state.json"))
    }
    pub fn download_queue_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("downloads.json"))
    }
    pub fn catalog_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("catalog.d"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self {
                download_parallelism: default_parallelism(),
                chunk_size_bytes: default_chunk_size(),
                max_concurrent_downloads: default_concurrent_downloads(),
                pypi_index_url: default_pypi_index_url(),
                ..Default::default()
            });
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Self =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if cfg.download_parallelism == 0 {
            cfg.download_parallelism = default_parallelism();
        }
        if cfg.chunk_size_bytes == 0 {
            cfg.chunk_size_bytes = default_chunk_size();
        }
        if cfg.max_concurrent_downloads == 0 {
            cfg.max_concurrent_downloads = default_concurrent_downloads();
        }
        if cfg.pypi_index_url.is_empty() {
            cfg.pypi_index_url = default_pypi_index_url();
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&path, self)
    }
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
