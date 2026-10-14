use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttentionBackend {
    #[default]
    Off,
    SageAttention,
    FlashAttention,
}

impl AttentionBackend {
    pub const ALL: [Self; 3] = [Self::Off, Self::SageAttention, Self::FlashAttention];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::SageAttention => "SageAttention",
            Self::FlashAttention => "FlashAttention",
        }
    }

    pub fn cli_arg(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::SageAttention => Some("--use-sage-attention"),
            Self::FlashAttention => Some("--use-flash-attention"),
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::SageAttention,
            Self::SageAttention => Self::FlashAttention,
            Self::FlashAttention => Self::Off,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// GiB of VRAM ComfyUI should leave available for transient CUDA/VAE work.
    #[serde(default = "default_comfy_reserve_vram_gb")]
    pub comfy_reserve_vram_gb: f32,
    #[serde(default)]
    pub swap_file_path: Option<PathBuf>,
    #[serde(default)]
    pub swap_size_gb: Option<u32>,
    #[serde(default)]
    pub pytorch_expandable_segments: bool,
    #[serde(default)]
    pub preload_tcmalloc: bool,
    #[serde(default)]
    pub attention_backend: AttentionBackend,
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
fn default_comfy_reserve_vram_gb() -> f32 {
    4.0
}
fn default_pypi_index_url() -> String {
    "https://pypi.org/simple".to_owned()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            comfy_path: None,
            hf_endpoint: None,
            pypi_index_url: default_pypi_index_url(),
            download_parallelism: default_parallelism(),
            chunk_size_bytes: default_chunk_size(),
            max_concurrent_downloads: default_concurrent_downloads(),
            comfy_reserve_vram_gb: default_comfy_reserve_vram_gb(),
            swap_file_path: None,
            swap_size_gb: None,
            pytorch_expandable_segments: false,
            preload_tcmalloc: false,
            attention_backend: AttentionBackend::Off,
        }
    }
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
                comfy_reserve_vram_gb: default_comfy_reserve_vram_gb(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reserve_four_gib_for_comfyui() {
        assert_eq!(AppConfig::default().comfy_reserve_vram_gb, 4.0);
    }

    #[test]
    fn legacy_config_without_reserve_gets_safe_default() {
        let config: AppConfig = serde_json::from_str(
            r#"{"comfy_path":null,"hf_endpoint":null,"pypi_index_url":"https://pypi.org/simple","download_parallelism":4,"chunk_size_bytes":16777216,"max_concurrent_downloads":2}"#,
        )
        .unwrap();
        assert_eq!(config.comfy_reserve_vram_gb, 4.0);
        assert!(!config.pytorch_expandable_segments);
        assert!(!config.preload_tcmalloc);
        assert_eq!(config.attention_backend, AttentionBackend::Off);
        assert!(config.swap_file_path.is_none());
    }

    #[test]
    fn attention_backend_cycling() {
        assert_eq!(AttentionBackend::Off.next(), AttentionBackend::SageAttention);
        assert_eq!(AttentionBackend::SageAttention.next(), AttentionBackend::FlashAttention);
        assert_eq!(AttentionBackend::FlashAttention.next(), AttentionBackend::Off);
    }
}
