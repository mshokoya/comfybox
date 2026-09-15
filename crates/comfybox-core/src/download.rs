use crate::catalog::Artifact;
use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use futures::{StreamExt, stream::FuturesUnordered};
use reqwest::{
    Client,
    header::{ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_LENGTH, RANGE},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs as stdfs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use sysinfo::Disks;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::Semaphore,
};

#[derive(Clone)]
pub struct DownloadOptions {
    pub parallelism: usize,
    pub chunk_size_bytes: u64,
    pub force: bool,
    pub hf_endpoint: Option<String>,
    pub hf_token: Option<String>,
    pub progress: Option<Arc<dyn Fn(DownloadProgress) + Send + Sync>>,
    pub log: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            parallelism: 4,
            chunk_size_bytes: 16 * 1024 * 1024,
            force: false,
            hf_endpoint: None,
            hf_token: None,
            progress: None,
            log: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RangeState {
    pub expected_size: u64,
    #[serde(default)]
    pub completed: Vec<CompletedRange>,
}

#[derive(Debug, Clone)]
pub enum InstallOutcome {
    AlreadyInstalled,
    Installed(PathBuf),
    Resumed(PathBuf),
}

#[derive(Clone)]
pub struct DownloadManager {
    client: Client,
}

impl DownloadManager {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("comfybox/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(12))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { client })
    }

    pub async fn install_artifact(
        &self,
        comfy_root: &Path,
        artifact: &Artifact,
        opts: &DownloadOptions,
    ) -> Result<InstallOutcome> {
        report_log(opts, "phase:resolving".into());
        let final_path = comfy_root.join(&artifact.relative_path);
        let existing_size = fs::metadata(&final_path).await.ok().map(|meta| meta.len());
        if existing_size.is_some()
            && let Some(expected_hash) = &artifact.sha256
        {
            if verify_sha256(final_path.clone(), expected_hash.clone()).await? {
                return Ok(InstallOutcome::AlreadyInstalled);
            }
            if !opts.force {
                bail!(
                    "{} already exists but does not match its SHA-256; use --force to replace",
                    final_path.display()
                );
            }
        }
        if let Some(existing) = existing_size
            && artifact.sha256.is_none()
            && artifact.size_bytes == Some(existing)
        {
            return Ok(InstallOutcome::AlreadyInstalled);
        }

        let (url, head) = self.resolve_download_url(artifact, opts).await?;
        let remote_size = head
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|x| x.to_str().ok())
            .and_then(|x| x.parse::<u64>().ok());
        let expected = remote_size
            .or(artifact.size_bytes)
            .context("remote did not provide Content-Length and catalog has no size")?;
        if let (Some(catalog), Some(remote)) = (artifact.size_bytes, remote_size)
            && catalog != remote
        {
            report_log(
                opts,
                format!(
                    "catalog size is approximate for {}; using remote size {} instead of {}",
                    artifact.name, remote, catalog
                ),
            );
        }
        if let Some(existing) = existing_size {
            if artifact.sha256.is_none() && existing == expected && remote_size.is_some() {
                return Ok(InstallOutcome::AlreadyInstalled);
            }
            if !opts.force {
                bail!(
                    "{} already exists but does not match catalog; use --force to replace",
                    final_path.display()
                );
            }
        }
        let supports_ranges = head
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|x| x.to_str().ok())
            .map(|x| x.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);

        let (temp_dir, part_path, ranges_path) = temp_paths(comfy_root, artifact);
        let already_downloaded = ranges_path
            .as_ref()
            .and_then(|p| stdfs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<RangeState>(&raw).ok())
            .filter(|r| r.expected_size == expected)
            .map(|r| {
                r.completed
                    .iter()
                    .map(|x| x.end.saturating_sub(x.start) + 1)
                    .sum::<u64>()
            })
            .unwrap_or(0);
        report_progress(opts, already_downloaded, expected);
        let missing = expected.saturating_sub(already_downloaded);
        ensure_free_space(
            final_path.parent().unwrap_or(comfy_root),
            missing.saturating_add((expected / 50).max(1024 * 1024 * 1024)),
        )?;
        fs::create_dir_all(&temp_dir).await?;
        let resumed = part_path.exists() && already_downloaded > 0;
        report_log(opts, "phase:downloading".into());
        if supports_ranges {
            self.download_ranged(
                &url,
                &part_path,
                ranges_path.as_ref().expect("range path"),
                expected,
                opts,
            )
            .await?;
        } else {
            self.download_sequential(&url, &part_path, expected, opts)
                .await?;
        }
        let actual = fs::metadata(&part_path).await?.len();
        if actual != expected {
            bail!(
                "downloaded size mismatch for {}: got {actual}, expected {expected}",
                artifact.name
            );
        }
        report_log(opts, "phase:processing".into());
        if let Some(hash) = &artifact.sha256 {
            if !verify_sha256(part_path.clone(), hash.clone()).await? {
                bail!("SHA-256 mismatch for {}", artifact.name);
            }
        }

        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        report_log(opts, "phase:installing".into());
        publish_atomically(&part_path, &final_path, opts.force).await?;
        let _ = fs::remove_dir_all(&temp_dir).await;
        Ok(if resumed {
            InstallOutcome::Resumed(final_path)
        } else {
            InstallOutcome::Installed(final_path)
        })
    }

    async fn resolve_download_url(
        &self,
        artifact: &Artifact,
        opts: &DownloadOptions,
    ) -> Result<(String, reqwest::Response)> {
        let candidates = download_url_candidates(&artifact.url, opts.hf_endpoint.as_deref());
        let mut last_error = None;
        for (candidate_index, url) in candidates.iter().enumerate() {
            for attempt in 1..=2 {
                let mut request = self.client.head(url).header(ACCEPT_ENCODING, "identity");
                if let Some(token) = &opts.hf_token {
                    request = request.bearer_auth(token);
                }
                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        if candidate_index > 0 {
                            report_log(
                                opts,
                                format!(
                                    "using fallback download endpoint for {}: {url}",
                                    artifact.name
                                ),
                            );
                        }
                        return Ok((url.clone(), response));
                    }
                    Ok(response)
                        if matches!(
                            response.status(),
                            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
                        ) =>
                    {
                        return Err(http_status_error(
                            "remote metadata request",
                            &artifact.name,
                            response.status(),
                            opts.hf_token.is_some(),
                        ));
                    }
                    Ok(response) => {
                        last_error =
                            Some(anyhow::anyhow!("HEAD {url} returned {}", response.status()));
                    }
                    Err(error) => {
                        last_error = Some(anyhow::Error::new(error).context(format!("HEAD {url}")));
                    }
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
            if candidate_index + 1 < candidates.len() {
                report_log(
                    opts,
                    format!(
                        "{} unreachable at {}; trying Hugging Face fallback endpoint",
                        artifact.name, url
                    ),
                );
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no download URL for {}", artifact.name)))
    }

    async fn download_sequential(
        &self,
        url: &str,
        part: &Path,
        expected: u64,
        opts: &DownloadOptions,
    ) -> Result<()> {
        if part.exists() {
            fs::remove_file(part).await?;
        }
        let mut req = self.client.get(url).header(ACCEPT_ENCODING, "identity");
        if let Some(token) = &opts.hf_token {
            req = req.bearer_auth(token);
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            return Err(http_status_error(
                "download",
                url,
                res.status(),
                opts.hf_token.is_some(),
            ));
        }
        let mut stream = res.bytes_stream();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(part)
            .await?;
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            report_progress(opts, written, expected);
            if written > expected {
                bail!("remote sent more data than expected");
            }
        }
        file.sync_all().await?;
        Ok(())
    }

    async fn download_ranged(
        &self,
        url: &str,
        part: &Path,
        ranges_path: &Path,
        expected: u64,
        opts: &DownloadOptions,
    ) -> Result<()> {
        let mut state = if ranges_path.exists() {
            serde_json::from_slice::<RangeState>(&fs::read(ranges_path).await?).unwrap_or_default()
        } else {
            RangeState::default()
        };
        if state.expected_size != 0 && state.expected_size != expected {
            state = RangeState::default();
        }
        state.expected_size = expected;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(part)
            .await?;
        file.set_len(expected).await?;
        let file = Arc::new(tokio::sync::Mutex::new(file));
        let completed: BTreeSet<(u64, u64)> =
            state.completed.iter().map(|r| (r.start, r.end)).collect();
        let initially_downloaded = state
            .completed
            .iter()
            .map(|range| range.end.saturating_sub(range.start) + 1)
            .sum::<u64>();
        let live_downloaded = Arc::new(AtomicU64::new(initially_downloaded));
        let chunk = opts.chunk_size_bytes.max(1024 * 1024);
        let mut missing = Vec::new();
        let mut start = 0u64;
        while start < expected {
            let end = (start + chunk - 1).min(expected - 1);
            if !completed.contains(&(start, end)) {
                missing.push((start, end));
            }
            start = end + 1;
        }
        let sem = Arc::new(Semaphore::new(opts.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();
        for (start, end) in missing {
            let permit = sem.clone().acquire_owned().await?;
            let client = self.client.clone();
            let url = url.to_owned();
            let token = opts.hf_token.clone();
            let live_downloaded = Arc::clone(&live_downloaded);
            let progress = opts.progress.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                let want = end - start + 1;
                let mut last_error: Option<anyhow::Error> = None;
                for attempt in 0..8u32 {
                    let mut req = client
                        .get(&url)
                        .header(ACCEPT_ENCODING, "identity")
                        .header(RANGE, format!("bytes={start}-{end}"));
                    if let Some(token) = &token {
                        req = req.bearer_auth(token);
                    }
                    match req.send().await {
                        Ok(res) if res.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                            let mut stream = res.bytes_stream();
                            let mut bytes = BytesMut::with_capacity(want as usize);
                            let mut attempt_bytes = 0u64;
                            let mut unreported = 0u64;
                            let mut stream_error = None;
                            while let Some(item) = stream.next().await {
                                match item {
                                    Ok(part) => {
                                        let count = part.len() as u64;
                                        attempt_bytes += count;
                                        unreported += count;
                                        bytes.extend_from_slice(&part);
                                        let total = live_downloaded.fetch_add(count, Ordering::Relaxed) + count;
                                        if unreported >= 512 * 1024 || attempt_bytes == want {
                                            if let Some(callback) = &progress { callback(DownloadProgress { downloaded_bytes: total.min(expected), total_bytes: expected }); }
                                            unreported = 0;
                                        }
                                    }
                                    Err(error) => { stream_error = Some(error); break; }
                                }
                            }
                            if attempt_bytes == want && stream_error.is_none() {
                                return Ok::<_, anyhow::Error>((start, end, bytes.freeze()));
                            }
                            live_downloaded.fetch_sub(attempt_bytes, Ordering::Relaxed);
                            if let Some(callback) = &progress { callback(DownloadProgress { downloaded_bytes: live_downloaded.load(Ordering::Relaxed), total_bytes: expected }); }
                            last_error = Some(stream_error.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("range {start}-{end} returned {attempt_bytes} bytes, expected {want}")));
                        }
                        Ok(res)
                            if matches!(
                                res.status(),
                                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
                            ) =>
                        {
                            return Err(http_status_error(
                                "range download",
                                &url,
                                res.status(),
                                token.is_some(),
                            ));
                        }
                        Ok(res) => {
                            last_error = Some(anyhow::anyhow!(
                                "range request {start}-{end} failed with {}",
                                res.status()
                            ))
                        }
                        Err(e) => last_error = Some(e.into()),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs((attempt + 1).min(5) as u64))
                        .await;
                }
                Err(last_error.unwrap_or_else(|| anyhow::anyhow!("range {start}-{end} failed")))
            }));
        }
        while let Some(joined) = tasks.next().await {
            let (start, end, bytes) = joined??;
            {
                let mut f = file.lock().await;
                f.seek(std::io::SeekFrom::Start(start)).await?;
                f.write_all(&bytes).await?;
            }
            state.completed.push(CompletedRange { start, end });
            state.completed.sort_by_key(|r| r.start);
            atomic_write_range_state(ranges_path, &state).await?;
        }
        file.lock().await.sync_all().await?;
        Ok(())
    }
}

pub fn temp_paths(comfy_root: &Path, artifact: &Artifact) -> (PathBuf, PathBuf, Option<PathBuf>) {
    let final_path = comfy_root.join(&artifact.relative_path);
    let parent = final_path.parent().unwrap_or(comfy_root);
    let safe_id = artifact.id.replace(['/', '\\', ':'], "_");
    let dir = parent.join(".comfybox-tmp").join(safe_id);
    let filename = final_path.file_name().unwrap_or_default().to_string_lossy();
    let part = dir.join(format!("{filename}.part"));
    let ranges = Some(dir.join("ranges.json"));
    (dir, part, ranges)
}

async fn atomic_write_range_state(path: &Path, state: &RangeState) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    fs::write(&tmp, serde_json::to_vec_pretty(state)?).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

async fn publish_atomically(part: &Path, final_path: &Path, force: bool) -> Result<()> {
    if !final_path.exists() {
        fs::rename(part, final_path).await?;
        return Ok(());
    }
    if !force {
        bail!("target already exists: {}", final_path.display());
    }
    let backup = final_path.with_extension(format!("comfybox-backup-{}", uuid::Uuid::new_v4()));
    fs::rename(final_path, &backup).await?;
    match fs::rename(part, final_path).await {
        Ok(()) => {
            let _ = fs::remove_file(backup).await;
            Ok(())
        }
        Err(e) => {
            let _ = fs::rename(&backup, final_path).await;
            Err(e.into())
        }
    }
}

async fn verify_sha256(path: PathBuf, expected: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let mut f = stdfs::File::open(path)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(hex::encode(h.finalize()).eq_ignore_ascii_case(expected.trim()))
    })
    .await?
}

fn ensure_free_space(path: &Path, required: u64) -> Result<()> {
    let disks = Disks::new_with_refreshed_list();
    let best = disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());
    if let Some(disk) = best {
        if disk.available_space() < required {
            bail!(
                "insufficient disk space on {}: need about {:.1} GiB, have {:.1} GiB free",
                disk.mount_point().display(),
                required as f64 / 1073741824.0,
                disk.available_space() as f64 / 1073741824.0
            );
        }
    }
    Ok(())
}

fn rewrite_hf_url(url: &str, endpoint: Option<&str>) -> String {
    let Some(endpoint) = endpoint else {
        return url.to_owned();
    };
    if let Some(rest) = url.strip_prefix("https://huggingface.co") {
        return format!("{}{}", endpoint.trim_end_matches('/'), rest);
    }
    url.to_owned()
}

fn download_url_candidates(url: &str, endpoint: Option<&str>) -> Vec<String> {
    let preferred = rewrite_hf_url(url, endpoint);
    let mut candidates = vec![preferred.clone()];
    if url.starts_with("https://huggingface.co/") {
        let fallback_endpoint = if preferred.starts_with("https://hf-mirror.com/") {
            "https://huggingface.co"
        } else {
            "https://hf-mirror.com"
        };
        let fallback = rewrite_hf_url(url, Some(fallback_endpoint));
        if fallback != preferred {
            candidates.push(fallback);
        }
    }
    candidates
}

fn report_progress(opts: &DownloadOptions, downloaded_bytes: u64, total_bytes: u64) {
    if let Some(callback) = &opts.progress {
        callback(DownloadProgress {
            downloaded_bytes,
            total_bytes,
        });
    }
}

fn report_log(opts: &DownloadOptions, message: String) {
    if let Some(callback) = &opts.log {
        callback(message);
    }
}

fn http_status_error(
    operation: &str,
    target: &str,
    status: reqwest::StatusCode,
    token_present: bool,
) -> anyhow::Error {
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        if token_present {
            anyhow::anyhow!(
                "{operation} was rejected for {target}: {status}; HF_TOKEN may be invalid or may not have accepted the repository terms"
            )
        } else {
            anyhow::anyhow!(
                "{operation} requires authentication for {target}: {status}; set HF_TOKEN and accept any repository terms before retrying"
            )
        }
    } else {
        anyhow::anyhow!("{operation} failed for {target}: {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::{download_url_candidates, rewrite_hf_url};

    #[test]
    fn hugging_face_urls_are_rewritten_to_the_selected_mirror() {
        assert_eq!(
            rewrite_hf_url(
                "https://huggingface.co/Comfy-Org/z_image/resolve/main/split_files/vae/ae.safetensors",
                Some("https://hf-mirror.com/"),
            ),
            "https://hf-mirror.com/Comfy-Org/z_image/resolve/main/split_files/vae/ae.safetensors"
        );
    }

    #[test]
    fn non_hugging_face_urls_are_not_rewritten() {
        assert_eq!(
            rewrite_hf_url(
                "https://example.com/model.safetensors",
                Some("https://hf-mirror.com")
            ),
            "https://example.com/model.safetensors"
        );
    }

    #[test]
    fn official_hugging_face_downloads_fall_back_to_the_mirror() {
        let urls = download_url_candidates(
            "https://huggingface.co/owner/repo/resolve/main/model.safetensors",
            Some("https://huggingface.co"),
        );
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("https://huggingface.co/"));
        assert!(urls[1].starts_with("https://hf-mirror.com/"));
    }

    #[test]
    fn mirror_downloads_fall_back_to_official_hugging_face() {
        let urls = download_url_candidates(
            "https://huggingface.co/owner/repo/resolve/main/model.safetensors",
            Some("https://hf-mirror.com"),
        );
        assert!(urls[0].starts_with("https://hf-mirror.com/"));
        assert!(urls[1].starts_with("https://huggingface.co/"));
    }
}
