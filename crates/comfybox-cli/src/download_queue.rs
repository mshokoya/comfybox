use anyhow::{Context, Result};
use comfybox_core::{
    auth,
    catalog::{Artifact, Catalog, CustomNode},
    comfy::ComfyManager,
    config::{AppConfig, atomic_write_json},
    download::{DownloadManager, DownloadOptions, DownloadProgress},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::mpsc, task::AbortHandle};

const MAX_LOG_LINES: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Queued,
    Downloading,
    Paused,
    Failed,
    Completed,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Downloading => "ACTIVE",
            Self::Paused => "PAUSED",
            Self::Failed => "FAILED",
            Self::Completed => "DONE",
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobSnapshot {
    pub artifact_id: String,
    pub name: String,
    pub relative_path: String,
    pub status: JobStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_second: f64,
    pub error: Option<String>,
    pub endpoint: Option<String>,
}

struct Job {
    artifact: Artifact,
    root: PathBuf,
    options: DownloadOptions,
    status: JobStatus,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    bytes_per_second: f64,
    last_progress: Instant,
    last_bytes: u64,
    error: Option<String>,
    abort: Option<AbortHandle>,
}

#[derive(Serialize, Deserialize)]
struct PersistedJob {
    artifact_id: String,
    root: PathBuf,
    status: JobStatus,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    endpoint: Option<String>,
    force: bool,
}

enum QueueEvent {
    Progress {
        artifact_id: String,
        progress: DownloadProgress,
    },
    Finished {
        artifact_id: String,
        result: std::result::Result<(), String>,
    },
    Log(String),
}

pub struct DownloadQueue {
    jobs: Vec<Job>,
    logs: VecDeque<String>,
    sender: mpsc::UnboundedSender<QueueEvent>,
    receiver: mpsc::UnboundedReceiver<QueueEvent>,
    state_path: PathBuf,
    log_path: PathBuf,
    server_log_path: Option<PathBuf>,
    server_log_offset: u64,
    server_log_partial: String,
}

impl DownloadQueue {
    pub fn load(cfg: &AppConfig, catalog: &Catalog) -> Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let state_path = AppConfig::download_queue_path()?;
        let log_path = AppConfig::config_dir()?.join("comfybox.log");
        let persisted = if state_path.is_file() {
            serde_json::from_slice::<Vec<PersistedJob>>(&fs::read(&state_path)?)
                .with_context(|| format!("parse {}", state_path.display()))?
        } else {
            Vec::new()
        };
        let mut jobs = Vec::new();
        for saved in persisted {
            let Some(artifact) = catalog.artifact(&saved.artifact_id).cloned() else {
                continue;
            };
            let status = match saved.status {
                JobStatus::Downloading | JobStatus::Queued => JobStatus::Paused,
                other => other,
            };
            jobs.push(Job {
                artifact,
                root: saved.root,
                options: DownloadOptions {
                    parallelism: cfg.download_parallelism,
                    chunk_size_bytes: cfg.chunk_size_bytes,
                    force: saved.force,
                    hf_endpoint: saved.endpoint,
                    hf_token: auth::hf_token()?,
                    progress: None,
                },
                status,
                downloaded_bytes: saved.downloaded_bytes,
                total_bytes: saved.total_bytes,
                bytes_per_second: 0.0,
                last_progress: Instant::now(),
                last_bytes: saved.downloaded_bytes,
                error: None,
                abort: None,
            });
        }
        let logs = fs::read_to_string(&log_path)
            .ok()
            .map(|text| {
                text.lines()
                    .rev()
                    .take(MAX_LOG_LINES)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            jobs,
            logs,
            sender,
            receiver,
            state_path,
            log_path,
            server_log_path: None,
            server_log_offset: 0,
            server_log_partial: String::new(),
        })
    }

    pub fn enqueue(
        &mut self,
        root: &Path,
        artifacts: impl IntoIterator<Item = Artifact>,
        options: &DownloadOptions,
    ) -> Result<usize> {
        let mut added = 0;
        for artifact in artifacts {
            if self.jobs.iter().any(|job| {
                job.artifact.id == artifact.id
                    && matches!(job.status, JobStatus::Queued | JobStatus::Downloading)
            }) {
                self.log(format!(
                    "{} is already queued or downloading",
                    artifact.name
                ));
                continue;
            }
            if let Some(existing) = self
                .jobs
                .iter_mut()
                .find(|job| job.artifact.id == artifact.id)
            {
                existing.root = root.to_path_buf();
                existing.options = options.clone();
                existing.status = JobStatus::Queued;
                existing.error = None;
                existing.bytes_per_second = 0.0;
                existing.abort = None;
            } else {
                self.jobs.push(Job {
                    total_bytes: artifact.size_bytes,
                    artifact,
                    root: root.to_path_buf(),
                    options: options.clone(),
                    status: JobStatus::Queued,
                    downloaded_bytes: 0,
                    bytes_per_second: 0.0,
                    last_progress: Instant::now(),
                    last_bytes: 0,
                    error: None,
                    abort: None,
                });
            }
            added += 1;
        }
        if added > 0 {
            self.log(format!("queued {added} artifact download(s)"));
            self.save()?;
        }
        Ok(added)
    }

    pub fn enqueue_custom_nodes(
        &mut self,
        root: &Path,
        nodes: impl IntoIterator<Item = CustomNode>,
    ) {
        for node in nodes {
            let root = root.to_path_buf();
            let sender = self.sender.clone();
            self.log(format!("queued custom-node download {}", node.name));
            tokio::spawn(async move {
                let name = node.name.clone();
                let result = install_custom_node(root, node).await;
                let message = match result {
                    Ok(()) => format!("completed custom-node download {name}"),
                    Err(error) => format!("failed custom-node download {name}: {error:#}"),
                };
                let _ = sender.send(QueueEvent::Log(message));
            });
        }
    }

    pub fn tick(&mut self, max_concurrent: usize) -> bool {
        let mut changed = false;
        let mut persist_needed = false;
        while let Ok(event) = self.receiver.try_recv() {
            changed = true;
            match event {
                QueueEvent::Log(message) => self.log(message),
                QueueEvent::Progress {
                    artifact_id,
                    progress,
                } => {
                    if let Some(job) = self
                        .jobs
                        .iter_mut()
                        .find(|job| job.artifact.id == artifact_id)
                    {
                        let elapsed = job.last_progress.elapsed().as_secs_f64();
                        if elapsed > 0.05 {
                            let delta = progress.downloaded_bytes.saturating_sub(job.last_bytes);
                            job.bytes_per_second = delta as f64 / elapsed;
                            job.last_progress = Instant::now();
                            job.last_bytes = progress.downloaded_bytes;
                        }
                        job.downloaded_bytes = progress.downloaded_bytes;
                        job.total_bytes = Some(progress.total_bytes);
                    }
                }
                QueueEvent::Finished {
                    artifact_id,
                    result,
                } => {
                    if let Some(index) = self
                        .jobs
                        .iter()
                        .position(|job| job.artifact.id == artifact_id)
                    {
                        persist_needed = true;
                        let name = self.jobs[index].artifact.name.clone();
                        self.jobs[index].abort = None;
                        self.jobs[index].bytes_per_second = 0.0;
                        match result {
                            Ok(()) => {
                                self.jobs[index].status = JobStatus::Completed;
                                if let Some(total) = self.jobs[index].total_bytes {
                                    self.jobs[index].downloaded_bytes = total;
                                }
                                self.log(format!("completed {name}"));
                            }
                            Err(error) => {
                                self.jobs[index].status = JobStatus::Failed;
                                self.jobs[index].error = Some(error.clone());
                                self.log(format!("failed {name}: {error}"));
                            }
                        }
                    }
                }
            }
        }

        let limit = max_concurrent.max(1);
        let mut active = self
            .jobs
            .iter()
            .filter(|job| job.status == JobStatus::Downloading)
            .count();
        while active < limit {
            let Some(index) = self
                .jobs
                .iter()
                .position(|job| job.status == JobStatus::Queued)
            else {
                break;
            };
            self.start(index);
            active += 1;
            changed = true;
            persist_needed = true;
        }
        if persist_needed {
            let _ = self.save();
        }
        changed
    }

    pub fn stop(&mut self, index: usize) -> Result<()> {
        let Some(job) = self.jobs.get_mut(index) else {
            return Ok(());
        };
        if let Some(abort) = job.abort.take() {
            abort.abort();
        }
        if matches!(job.status, JobStatus::Downloading | JobStatus::Queued) {
            job.status = JobStatus::Paused;
            job.bytes_per_second = 0.0;
            let name = job.artifact.name.clone();
            self.log(format!("paused {name}; verified chunks were retained"));
            self.save()?;
        }
        Ok(())
    }

    pub fn resume(&mut self, index: usize) -> Result<()> {
        let Some(job) = self.jobs.get_mut(index) else {
            return Ok(());
        };
        if job.status == JobStatus::Paused {
            job.status = JobStatus::Queued;
            job.error = None;
            let name = job.artifact.name.clone();
            self.log(format!("continued {name} from retained download state"));
            self.save()?;
        }
        Ok(())
    }

    pub fn retry(&mut self, index: usize) -> Result<()> {
        let Some(job) = self.jobs.get_mut(index) else {
            return Ok(());
        };
        if job.status == JobStatus::Failed {
            job.status = JobStatus::Queued;
            job.error = None;
            let name = job.artifact.name.clone();
            self.log(format!("retrying failed download {name}"));
            self.save()?;
        }
        Ok(())
    }

    pub fn snapshots(&self) -> Vec<JobSnapshot> {
        self.jobs
            .iter()
            .map(|job| JobSnapshot {
                artifact_id: job.artifact.id.clone(),
                name: job.artifact.name.clone(),
                relative_path: job.artifact.relative_path.clone(),
                status: job.status,
                downloaded_bytes: job.downloaded_bytes,
                total_bytes: job.total_bytes,
                bytes_per_second: job.bytes_per_second,
                error: job.error.clone(),
                endpoint: job.options.hf_endpoint.clone(),
            })
            .collect()
    }

    pub fn logs(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.logs.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn update_chunk_parallelism(&mut self, parallelism: usize) {
        for job in &mut self.jobs {
            if job.status != JobStatus::Downloading {
                job.options.parallelism = parallelism.max(1);
            }
        }
        self.log(format!(
            "per-file chunk parallelism set to {}",
            parallelism.max(1)
        ));
    }

    pub fn record(&mut self, message: impl Into<String>) {
        self.log(message.into());
    }

    pub fn tail_comfyui_log(&mut self, path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        let changed_file = self.server_log_path.as_deref() != Some(path);
        if changed_file {
            self.server_log_path = Some(path.to_path_buf());
            self.server_log_offset = metadata.len().saturating_sub(64 * 1024);
            self.server_log_partial.clear();
        } else if metadata.len() < self.server_log_offset {
            self.server_log_offset = 0;
            self.server_log_partial.clear();
        }
        if metadata.len() == self.server_log_offset {
            return false;
        }
        let Ok(mut file) = fs::File::open(path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(self.server_log_offset)).is_err() {
            return false;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return false;
        }
        self.server_log_offset += bytes.len() as u64;
        let mut text = std::mem::take(&mut self.server_log_partial);
        text.push_str(&String::from_utf8_lossy(&bytes));
        let complete = text.ends_with('\n');
        let mut lines = text.split('\n').map(str::to_owned).collect::<Vec<_>>();
        if !complete {
            self.server_log_partial = lines.pop().unwrap_or_default();
        } else {
            lines.pop();
        }
        let mut changed = false;
        for line in lines.into_iter().filter(|line| !line.is_empty()) {
            self.push_log_line(format!("[COMFYUI] {line}"));
            changed = true;
        }
        changed
    }

    fn start(&mut self, index: usize) {
        let artifact = self.jobs[index].artifact.clone();
        let root = self.jobs[index].root.clone();
        let mut options = self.jobs[index].options.clone();
        let sender = self.sender.clone();
        let progress_id = artifact.id.clone();
        options.progress = Some(Arc::new(move |progress| {
            let _ = sender.send(QueueEvent::Progress {
                artifact_id: progress_id.clone(),
                progress,
            });
        }));
        let sender = self.sender.clone();
        let finished_id = artifact.id.clone();
        self.jobs[index].status = JobStatus::Downloading;
        self.jobs[index].last_progress = Instant::now();
        self.jobs[index].last_bytes = self.jobs[index].downloaded_bytes;
        self.jobs[index].error = None;
        self.log(format!("started {}", artifact.name));
        let handle = tokio::spawn(async move {
            let result = async {
                let manager = DownloadManager::new()?;
                manager.install_artifact(&root, &artifact, &options).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(QueueEvent::Finished {
                artifact_id: finished_id,
                result,
            });
        });
        self.jobs[index].abort = Some(handle.abort_handle());
    }

    fn log(&mut self, message: String) {
        let line = format!("[{}] {message}", timestamp());
        self.push_log_line(line.clone());
        if let Some(parent) = self.log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn push_log_line(&mut self, line: String) {
        if self.logs.len() == MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let jobs = self
            .jobs
            .iter()
            .map(|job| PersistedJob {
                artifact_id: job.artifact.id.clone(),
                root: job.root.clone(),
                status: job.status,
                downloaded_bytes: job.downloaded_bytes,
                total_bytes: job.total_bytes,
                endpoint: job.options.hf_endpoint.clone(),
                force: job.options.force,
            })
            .collect::<Vec<_>>();
        atomic_write_json(&self.state_path, &jobs)
    }
}

async fn install_custom_node(root: PathBuf, node: CustomNode) -> Result<()> {
    let base = root.join("custom_nodes");
    tokio::fs::create_dir_all(&base).await?;
    let target = base.join(&node.folder_name);
    if target.exists() {
        return Ok(());
    }
    let temporary_base = base.join(".comfybox-tmp");
    tokio::fs::create_dir_all(&temporary_base).await?;
    let temporary = temporary_base.join(format!("{}-{}", node.id, uuid::Uuid::new_v4()));
    if let Err(error) = ComfyManager::clone_repository(&node.git_url, &temporary).await {
        let _ = tokio::fs::remove_dir_all(&temporary).await;
        return Err(error).with_context(|| format!("git clone failed for {}", node.name));
    }
    tokio::fs::rename(&temporary, &target).await?;
    let _ = tokio::fs::remove_dir(&temporary_base).await;
    Ok(())
}

impl Drop for DownloadQueue {
    fn drop(&mut self) {
        for job in &mut self.jobs {
            if let Some(abort) = job.abort.take() {
                abort.abort();
                job.status = JobStatus::Paused;
            }
        }
        let _ = self.save();
    }
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
}
