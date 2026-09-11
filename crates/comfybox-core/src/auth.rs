use crate::config::AppConfig;
use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

static SESSION_TOKEN: OnceLock<RwLock<Option<String>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HfTokenSource {
    Session,
    Environment,
    ExplicitTokenPath,
    ComfyBoxCredential,
    HuggingFaceCache,
}

impl HfTokenSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "current session",
            Self::Environment => "environment",
            Self::ExplicitTokenPath => "HF_TOKEN_PATH",
            Self::ComfyBoxCredential => "ComfyBox credential",
            Self::HuggingFaceCache => "Hugging Face cache",
        }
    }
}

pub struct HfCredential {
    token: String,
    source: HfTokenSource,
}

impl HfCredential {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn source(&self) -> HfTokenSource {
        self.source
    }
}

pub fn resolve_hf_token() -> Result<Option<HfCredential>> {
    if let Some(token) = session_token() {
        return Ok(Some(HfCredential {
            token,
            source: HfTokenSource::Session,
        }));
    }
    if let Some(token) = env_token("HF_TOKEN")? {
        return Ok(Some(HfCredential {
            token,
            source: HfTokenSource::Environment,
        }));
    }

    if let Some(path) = env::var_os("HF_TOKEN_PATH").map(PathBuf::from) {
        if path.is_file() {
            return read_credential(&path, HfTokenSource::ExplicitTokenPath).map(Some);
        }
    }

    let saved = token_path()?;
    if saved.is_file() {
        return read_credential(&saved, HfTokenSource::ComfyBoxCredential).map(Some);
    }

    for path in huggingface_cache_paths() {
        if path.is_file() {
            return read_credential(&path, HfTokenSource::HuggingFaceCache).map(Some);
        }
    }
    Ok(None)
}

pub fn hf_token() -> Result<Option<String>> {
    Ok(resolve_hf_token()?.map(|credential| credential.token))
}

pub fn token_path() -> Result<PathBuf> {
    Ok(AppConfig::config_dir()?.join("hf_token"))
}

pub fn save_hf_token(token: &str) -> Result<PathBuf> {
    let token = normalize_token(token)?.context("HF token cannot be empty")?;
    let path = token_path()?;
    let parent = path.parent().context("HF token path has no parent")?;
    fs::create_dir_all(parent)?;
    secure_directory(parent)?;

    let temporary = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    let result =
        write_secret(&temporary, token.as_bytes()).and_then(|()| replace_file(&temporary, &path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    set_session_token(Some(token));
    Ok(path)
}

pub fn clear_saved_hf_token() -> Result<bool> {
    let path = token_path()?;
    if !path.exists() {
        set_session_token(None);
        return Ok(false);
    }
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    set_session_token(None);
    Ok(true)
}

fn env_token(name: &str) -> Result<Option<String>> {
    let Some(raw) = env::var_os(name) else {
        return Ok(None);
    };
    let raw = raw
        .into_string()
        .map_err(|_| anyhow::anyhow!("{name} is not valid Unicode"))?;
    normalize_token(&raw).with_context(|| format!("invalid {name}"))
}

fn session_token() -> Option<String> {
    SESSION_TOKEN
        .get()
        .and_then(|token| token.read().ok()?.clone())
}

fn set_session_token(token: Option<String>) {
    let value = SESSION_TOKEN.get_or_init(|| RwLock::new(None));
    if let Ok(mut current) = value.write() {
        *current = token;
    }
}

fn read_credential(path: &Path, source: HfTokenSource) -> Result<HfCredential> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read token from {}", path.display()))?;
    let token = normalize_token(&raw)?
        .with_context(|| format!("token file is empty: {}", path.display()))?;
    Ok(HfCredential { token, source })
}

fn normalize_token(raw: &str) -> Result<Option<String>> {
    let token = raw.trim();
    if token.is_empty() {
        return Ok(None);
    }
    if token.len() > 4_096 || token.chars().any(char::is_whitespace) {
        bail!("HF token contains whitespace or is unexpectedly long");
    }
    Ok(Some(token.to_owned()))
}

fn huggingface_cache_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = env::var_os("HF_HOME") {
        paths.push(PathBuf::from(home).join("token"));
    }
    if let Some(base) = BaseDirs::new() {
        paths.push(base.home_dir().join(".cache/huggingface/token"));
        paths.push(base.home_dir().join(".huggingface/token"));
    }
    paths
}

fn write_secret(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn secure_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(temporary, destination)
        .with_context(|| format!("publish credential to {}", destination.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_token;

    #[test]
    fn token_is_trimmed_without_being_logged() {
        assert_eq!(
            normalize_token("  hf_example\n").unwrap().as_deref(),
            Some("hf_example")
        );
    }

    #[test]
    fn blank_or_whitespace_tokens_are_rejected() {
        assert!(normalize_token("   ").unwrap().is_none());
        assert!(normalize_token("hf_bad token").is_err());
    }
}
