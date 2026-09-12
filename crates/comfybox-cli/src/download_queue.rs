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
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
    task::AbortHandle,
};

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

struct Operation {
    root: PathBuf,
    pip_index_url: String,
    status: JobStatus,
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
    OperationFinished(std::result::Result<(), String>),
}

pub struct DownloadQueue {
    jobs: Vec<Job>,
    python_deps: Option<Operation>,
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
            python_deps: None,
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
                let result = install_custom_node(root, node, &sender).await;
                let message = match result {
                    Ok(()) => format!("completed custom-node download {name}"),
                    Err(error) => format!("failed custom-node download {name}: {error:#}"),
                };
                let _ = sender.send(QueueEvent::Log(message));
            });
        }
    }

    pub fn enqueue_python_deps(&mut self, root: &Path, pip_index_url: &str) -> Result<()> {
        if !root.join("main.py").is_file() || !root.join("requirements.txt").is_file() {
            anyhow::bail!("install or locate ComfyUI before installing Python dependencies");
        }
        self.python_deps = Some(Operation {
            root: root.to_path_buf(),
            pip_index_url: pip_index_url.to_owned(),
            status: JobStatus::Queued,
            error: None,
            abort: None,
        });
        self.log(format!(
            "queued ComfyUI Python dependencies via {pip_index_url}"
        ));
        Ok(())
    }

    pub fn tick(&mut self, max_concurrent: usize) -> bool {
        let mut changed = false;
        let mut persist_needed = false;
        while let Ok(event) = self.receiver.try_recv() {
            changed = true;
            match event {
                QueueEvent::Log(message) => self.log(message),
                QueueEvent::OperationFinished(result) => {
                    if let Some(operation) = &mut self.python_deps {
                        operation.abort = None;
                        match result {
                            Ok(()) => {
                                operation.status = JobStatus::Completed;
                                self.log("completed ComfyUI Python dependencies".into());
                            }
                            Err(error) => {
                                operation.status = JobStatus::Failed;
                                operation.error = Some(error.clone());
                                self.log(format!("failed ComfyUI Python dependencies: {error}"));
                            }
                        }
                    }
                }
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
        if self
            .python_deps
            .as_ref()
            .is_some_and(|operation| operation.status == JobStatus::Queued)
        {
            self.start_python_deps();
            changed = true;
        }
        if persist_needed {
            let _ = self.save();
        }
        changed
    }

    pub fn stop(&mut self, index: usize) -> Result<()> {
        if index >= self.jobs.len() {
            if let Some(operation) = &mut self.python_deps {
                if let Some(abort) = operation.abort.take() {
                    abort.abort();
                }
                operation.status = JobStatus::Paused;
                self.log("paused ComfyUI Python dependencies".into());
            }
            return Ok(());
        }
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
        if index >= self.jobs.len() {
            if let Some(operation) = &mut self.python_deps
                && operation.status == JobStatus::Paused
            {
                operation.status = JobStatus::Queued;
                operation.error = None;
            }
            return Ok(());
        }
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
        if index >= self.jobs.len() {
            if let Some(operation) = &mut self.python_deps
                && operation.status == JobStatus::Failed
            {
                operation.status = JobStatus::Queued;
                operation.error = None;
            }
            return Ok(());
        }
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
        let mut snapshots = self
            .jobs
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
            .collect::<Vec<_>>();
        if let Some(operation) = &self.python_deps {
            snapshots.push(JobSnapshot {
                artifact_id: "python-dependencies".into(),
                name: "ComfyUI Python dependencies".into(),
                relative_path: operation.root.join(".venv").display().to_string(),
                status: operation.status,
                downloaded_bytes: 0,
                total_bytes: None,
                bytes_per_second: 0.0,
                error: operation.error.clone(),
                endpoint: None,
            });
        }
        snapshots
    }

    pub fn logs(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.logs.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.jobs.len() + usize::from(self.python_deps.is_some())
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

    fn start_python_deps(&mut self) {
        let Some(operation) = &mut self.python_deps else {
            return;
        };
        let root = operation.root.clone();
        let pip_index_url = operation.pip_index_url.clone();
        let sender = self.sender.clone();
        operation.status = JobStatus::Downloading;
        self.log("started ComfyUI Python dependencies".into());
        let handle = tokio::spawn(async move {
            let result = install_python_dependencies(&root, &pip_index_url, &sender)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(QueueEvent::OperationFinished(result));
        });
        if let Some(operation) = &mut self.python_deps {
            operation.abort = Some(handle.abort_handle());
        }
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

async fn install_custom_node(
    root: PathBuf,
    node: CustomNode,
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    let base = root.join("custom_nodes");
    tokio::fs::create_dir_all(&base).await?;
    let target = find_equivalent_node_folder(&base, &node.folder_name)
        .unwrap_or_else(|| base.join(&node.folder_name));
    if !target.exists() {
        let temporary_base = base.join(".comfybox-tmp");
        tokio::fs::create_dir_all(&temporary_base).await?;
        let temporary = temporary_base.join(format!("{}-{}", node.id, uuid::Uuid::new_v4()));
        if let Err(error) = ComfyManager::clone_repository(&node.git_url, &temporary).await {
            let _ = tokio::fs::remove_dir_all(&temporary).await;
            return Err(error).with_context(|| format!("git clone failed for {}", node.name));
        }
        tokio::fs::rename(&temporary, &target).await?;
        let _ = tokio::fs::remove_dir(&temporary_base).await;
    }
    let requirements = target.join("requirements.txt");
    let python = if cfg!(windows) {
        root.join(".venv/Scripts/python.exe")
    } else {
        root.join(".venv/bin/python")
    };
    if !python.is_file() {
        anyhow::bail!("ComfyUI virtual environment is missing; install Python dependencies first");
    }
    if requirements.is_file() {
        let _ = sender.send(QueueEvent::Log(format!(
            "[PYTHON] Installing requirements for {}",
            node.name
        )));
        install_custom_node_python_requirements(&python, &node.folder_name, &requirements, sender)
            .await?;
    }
    ensure_known_node_runtime(&python, &node.folder_name, sender).await?;
    Ok(())
}

async fn install_python_dependencies(
    root: &Path,
    pip_index_url: &str,
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    let requirements = root.join("requirements.txt");
    if !requirements.is_file() {
        anyhow::bail!("requirements.txt not found in {}", root.display());
    }
    let venv = root.join(".venv");
    let python = if cfg!(windows) {
        venv.join("Scripts/python.exe")
    } else {
        venv.join("bin/python")
    };
    if !python.is_file() {
        let system_python = if cfg!(windows) { "python" } else { "python3" };
        run_logged(
            Command::new(system_python).arg("-m").arg("venv").arg(&venv),
            sender,
        )
        .await?;
    }
    let _ = sender.send(QueueEvent::Log(format!(
        "[PYTHON] Configuring pip index: {pip_index_url}"
    )));
    run_logged(
        Command::new(&python)
            .arg("-m")
            .arg("pip")
            .arg("config")
            .arg("set")
            .arg("global.index-url")
            .arg(pip_index_url)
            .env("PIP_INDEX_URL", pip_index_url),
        sender,
    )
    .await?;
    let wheelhouse = root
        .join(".comfybox-tmp")
        .join(format!("pip-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&wheelhouse).await?;
    let download = run_logged(
        Command::new(&python)
            .arg("-m")
            .arg("pip")
            .arg("download")
            .arg("-r")
            .arg(&requirements)
            .arg("-d")
            .arg(&wheelhouse)
            .env("PIP_INDEX_URL", pip_index_url),
        sender,
    )
    .await;
    if let Err(error) = download {
        let _ = tokio::fs::remove_dir_all(&wheelhouse).await;
        return Err(error);
    }
    let install = run_logged(
        Command::new(&python)
            .arg("-m")
            .arg("pip")
            .arg("install")
            .arg("--no-index")
            .arg("--find-links")
            .arg(&wheelhouse)
            .arg("-r")
            .arg(&requirements),
        sender,
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&wheelhouse).await;
    install?;
    let _ = sender.send(QueueEvent::Log(
        "[PYTHON] Installing/upgrading ComfyUI Node Manager".into(),
    ));
    run_logged(
        Command::new(&python)
            .arg("-m")
            .arg("pip")
            .arg("install")
            .arg("-U")
            .arg("--pre")
            .arg("comfyui-manager")
            .env("PIP_INDEX_URL", pip_index_url),
        sender,
    )
    .await?;
    let custom_nodes = root.join("custom_nodes");
    if custom_nodes.is_dir() {
        let mut node_folders = fs::read_dir(&custom_nodes)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        node_folders.sort();
        let mut node_errors = Vec::new();
        for node_folder in node_folders {
            let node_name = node_folder
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let requirements = node_folder.join("requirements.txt");
            if requirements.is_file() {
                let _ = sender.send(QueueEvent::Log(format!(
                    "[PYTHON] Installing requirements for {node_name}"
                )));
                if let Err(error) = install_custom_node_python_requirements(
                    &python,
                    &node_name,
                    &requirements,
                    sender,
                )
                .await
                {
                    let message = format!("{node_name}: {error:#}");
                    let _ = sender.send(QueueEvent::Log(format!("[PYTHON] FAILED {message}")));
                    node_errors.push(message);
                }
            }
            if let Err(error) = ensure_known_node_runtime(&python, &node_name, sender).await {
                let message = format!("{node_name} runtime verification: {error:#}");
                let _ = sender.send(QueueEvent::Log(format!("[PYTHON] FAILED {message}")));
                node_errors.push(message);
            }
        }
        if !node_errors.is_empty() {
            anyhow::bail!(
                "custom-node dependency failures: {}",
                node_errors.join("; ")
            );
        }
    }
    let marker_dir = root.join(".comfybox");
    tokio::fs::create_dir_all(&marker_dir).await?;
    let requirements_bytes = tokio::fs::read(&requirements).await?;
    use sha2::{Digest, Sha256};
    tokio::fs::write(
        marker_dir.join("python-deps.sha256"),
        format!("v6:{}", hex::encode(Sha256::digest(requirements_bytes))),
    )
    .await?;
    Ok(())
}

async fn install_custom_node_python_requirements(
    python: &Path,
    folder_name: &str,
    requirements: &Path,
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    let mut command = Command::new(python);
    command
        .arg("-m")
        .arg("pip")
        .arg("install")
        .env_remove("PIP_INDEX_URL");
    if normalized_node_name(folder_name) == "comfyuifaceanalysis" {
        command.args(["onnxruntime", "insightface", "color_matcher"]);
    } else {
        command.arg("-r").arg(requirements);
    }
    run_logged(&mut command, sender).await
}

fn normalized_node_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn find_equivalent_node_folder(base: &Path, expected: &str) -> Option<PathBuf> {
    let expected = normalized_node_name(expected);
    fs::read_dir(base)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            entry.file_type().ok()?.is_dir().then_some(())?;
            (normalized_node_name(&entry.file_name().to_string_lossy()) == expected)
                .then(|| entry.path())
        })
}

async fn ensure_known_node_runtime(
    python: &Path,
    folder_name: &str,
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    let Some((packages, import)) = known_node_runtime_spec(folder_name) else {
        return Ok(());
    };
    let _ = sender.send(QueueEvent::Log(format!(
        "[PYTHON] Verifying runtime for {folder_name}"
    )));
    let mut install = Command::new(python);
    install
        .arg("-m")
        .arg("pip")
        .arg("install")
        .args(packages)
        .env_remove("PIP_INDEX_URL");
    run_logged(&mut install, sender).await?;
    run_logged(Command::new(python).arg("-c").arg(import), sender).await
}

fn known_node_runtime_spec(folder_name: &str) -> Option<(&'static [&'static str], &'static str)> {
    match normalized_node_name(folder_name).as_str() {
        "comfyuifaceanalysis" => Some((
            &[
                "setuptools",
                "onnx",
                "onnxruntime",
                "insightface==0.7.3",
                "color_matcher",
            ],
            "from insightface.app import FaceAnalysis",
        )),
        "comfyuioutputlistscombiner" => Some((&["skia-python"], "import skia")),
        "comfyuieasyuse" => Some((&["opencv-python-headless"], "import cv2")),
        _ => None,
    }
}

async fn run_logged(
    command: &mut Command,
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    command
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().context("capture process stdout")?).lines();
    let mut stderr = BufReader::new(child.stderr.take().context("capture process stderr")?).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;
    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout.next_line(), if !stdout_done => match line? { Some(line) => { let _ = sender.send(QueueEvent::Log(format!("[PYTHON] {line}"))); }, None => stdout_done = true },
            line = stderr.next_line(), if !stderr_done => match line? { Some(line) => { let _ = sender.send(QueueEvent::Log(format!("[PYTHON] {line}"))); }, None => stderr_done = true },
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("process exited with {status}");
    }
    Ok(())
}

pub fn python_dependencies_ready(root: &Path) -> bool {
    let requirements = match fs::read(root.join("requirements.txt")) {
        Ok(value) => value,
        Err(_) => return false,
    };
    use sha2::{Digest, Sha256};
    let expected = format!("v6:{}", hex::encode(Sha256::digest(requirements)));
    if !fs::read_to_string(root.join(".comfybox/python-deps.sha256"))
        .is_ok_and(|value| value.trim() == expected)
    {
        return false;
    }
    let python = if cfg!(windows) {
        root.join(".venv/Scripts/python.exe")
    } else {
        root.join(".venv/bin/python")
    };
    [
        "ComfyUI_FaceAnalysis",
        "ComfyUI-outputlists-combiner",
        "ComfyUI-Easy-Use",
    ]
    .into_iter()
    .filter(|folder| find_equivalent_node_folder(&root.join("custom_nodes"), folder).is_some())
    .all(|folder| {
        let (_, import) = known_node_runtime_spec(folder).expect("known runtime");
        std::process::Command::new(&python)
            .arg("-c")
            .arg(import)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

impl Drop for DownloadQueue {
    fn drop(&mut self) {
        for job in &mut self.jobs {
            if let Some(abort) = job.abort.take() {
                abort.abort();
                job.status = JobStatus::Paused;
            }
        }
        if let Some(operation) = &mut self.python_deps
            && let Some(abort) = operation.abort.take()
        {
            abort.abort();
            operation.status = JobStatus::Paused;
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
