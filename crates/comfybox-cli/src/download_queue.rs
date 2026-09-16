use anyhow::{Context, Result};
use comfybox_core::{
    auth,
    catalog::{Artifact, Catalog, CustomNode, SystemDependency},
    comfy::ComfyManager,
    config::{AppConfig, atomic_write_json},
    download::{DownloadManager, DownloadOptions, DownloadProgress, temp_paths},
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
    Resolving,
    Downloading,
    Processing,
    Installing,
    Paused,
    Failed,
    Completed,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Resolving => "RESOLVING",
            Self::Downloading => "DOWNLOADING",
            Self::Processing => "PROCESSING",
            Self::Installing => "INSTALLING",
            Self::Paused => "PAUSED",
            Self::Failed => "FAILED",
            Self::Completed => "DONE",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Resolving
                | Self::Downloading
                | Self::Processing
                | Self::Installing
        )
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

struct SystemOperation {
    dependencies: Vec<SystemDependency>,
    status: JobStatus,
    error: Option<String>,
    abort: Option<AbortHandle>,
}

struct CustomNodeOperation {
    node: CustomNode,
    root: PathBuf,
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
    Stage {
        artifact_id: String,
        status: JobStatus,
    },
    Finished {
        artifact_id: String,
        result: std::result::Result<(), String>,
    },
    Log(String),
    OperationFinished(std::result::Result<(), String>),
    SystemOperationFinished(std::result::Result<(), String>),
    CustomNodeFinished {
        node_id: String,
        result: std::result::Result<(), String>,
    },
}

pub struct DownloadQueue {
    jobs: Vec<Job>,
    python_deps: Option<Operation>,
    system_deps: Option<SystemOperation>,
    custom_nodes: Vec<CustomNodeOperation>,
    logs: VecDeque<String>,
    sender: mpsc::UnboundedSender<QueueEvent>,
    receiver: mpsc::UnboundedReceiver<QueueEvent>,
    state_path: PathBuf,
    log_file: Option<fs::File>,
    server_log_path: Option<PathBuf>,
    server_log_offset: u64,
    server_log_partial: String,
}

impl DownloadQueue {
    pub fn load(cfg: &AppConfig, catalog: &Catalog) -> Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let state_path = AppConfig::download_queue_path()?;
        let log_path = AppConfig::config_dir()?.join("comfybox.log");
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let log_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        let persisted = if state_path.is_file() {
            let state = fs::read_to_string(&state_path)?;
            // Older releases exposed a destructive `Stopped` lifecycle. Treat any
            // such persisted jobs as paused so their resumable data remains usable.
            let state = state.replace("\"Stopped\"", "\"Paused\"");
            serde_json::from_str::<Vec<PersistedJob>>(&state)
                .with_context(|| format!("parse {}", state_path.display()))?
        } else {
            Vec::new()
        };
        let mut jobs = Vec::new();
        for saved in persisted {
            let Some(artifact) = catalog.artifact(&saved.artifact_id).cloned() else {
                continue;
            };
            let status = if saved.status.is_active() {
                JobStatus::Paused
            } else {
                saved.status
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
                    log: None,
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
            system_deps: None,
            custom_nodes: Vec::new(),
            logs,
            sender,
            receiver,
            state_path,
            log_file,
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
            if self
                .jobs
                .iter()
                .any(|job| job.artifact.id == artifact.id && job.status.is_active())
            {
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
            if self
                .custom_nodes
                .iter()
                .any(|operation| operation.node.id == node.id && operation.status.is_active())
            {
                self.log(format!("{} is already queued or installing", node.name));
                continue;
            }
            self.log(format!("queued custom-node download {}", node.name));
            if let Some(existing) = self
                .custom_nodes
                .iter_mut()
                .find(|operation| operation.node.id == node.id)
            {
                existing.node = node;
                existing.root = root.to_path_buf();
                existing.status = JobStatus::Queued;
                existing.error = None;
                existing.abort = None;
            } else {
                self.custom_nodes.push(CustomNodeOperation {
                    node,
                    root: root.to_path_buf(),
                    status: JobStatus::Queued,
                    error: None,
                    abort: None,
                });
            }
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

    pub fn enqueue_system_deps(&mut self, dependencies: Vec<SystemDependency>) -> Result<()> {
        if !cfg!(target_os = "linux") {
            anyhow::bail!("automatic system dependency installation is only supported on Linux");
        }
        if !command_exists("apt-get") {
            anyhow::bail!("apt-get is unavailable; install the manifest system packages manually");
        }
        let dependencies = dependencies
            .into_iter()
            .filter(|dependency| !system_package_installed(&dependency.package))
            .collect::<Vec<_>>();
        if dependencies.is_empty() {
            self.log("all manifest system dependencies are already installed".into());
            return Ok(());
        }
        let names = dependencies
            .iter()
            .map(|dependency| dependency.package.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.system_deps = Some(SystemOperation {
            dependencies,
            status: JobStatus::Queued,
            error: None,
            abort: None,
        });
        self.log(format!("queued system dependencies: {names}"));
        Ok(())
    }

    pub fn tick(&mut self, max_concurrent: usize) -> bool {
        let mut changed = false;
        let mut persist_needed = false;
        // Bound work per UI frame so noisy pip/apt processes cannot starve input
        // handling or terminal redraws. Remaining events are consumed next tick.
        for _ in 0..512 {
            let Ok(event) = self.receiver.try_recv() else {
                break;
            };
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
                QueueEvent::SystemOperationFinished(result) => {
                    if let Some(operation) = &mut self.system_deps {
                        operation.abort = None;
                        match result {
                            Ok(()) => {
                                operation.status = JobStatus::Completed;
                                self.log("completed system dependencies".into());
                            }
                            Err(error) => {
                                operation.status = JobStatus::Failed;
                                operation.error = Some(error.clone());
                                self.log(format!("failed system dependencies: {error}"));
                            }
                        }
                    }
                }
                QueueEvent::CustomNodeFinished { node_id, result } => {
                    if let Some(index) = self
                        .custom_nodes
                        .iter()
                        .position(|operation| operation.node.id == node_id)
                    {
                        let name = self.custom_nodes[index].node.name.clone();
                        self.custom_nodes[index].abort = None;
                        match result {
                            Ok(()) => {
                                self.custom_nodes[index].status = JobStatus::Completed;
                                self.log(format!("completed custom-node download {name}"));
                            }
                            Err(error) => {
                                self.custom_nodes[index].status = JobStatus::Failed;
                                self.custom_nodes[index].error = Some(error.clone());
                                self.log(format!("failed custom-node download {name}: {error}"));
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
                QueueEvent::Stage {
                    artifact_id,
                    status,
                } => {
                    if let Some(job) = self
                        .jobs
                        .iter_mut()
                        .find(|job| job.artifact.id == artifact_id)
                    {
                        job.status = status;
                        if status != JobStatus::Downloading {
                            job.bytes_per_second = 0.0;
                        }
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
            .filter(|job| job.status.is_active())
            .count()
            + self
                .custom_nodes
                .iter()
                .filter(|operation| operation.status.is_active())
                .count();
        while active < limit {
            if let Some(index) = self
                .jobs
                .iter()
                .position(|job| job.status == JobStatus::Queued)
            {
                self.start(index);
            } else if let Some(index) = self
                .custom_nodes
                .iter()
                .position(|operation| operation.status == JobStatus::Queued)
            {
                self.start_custom_node(index);
            } else {
                break;
            }
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
        if self
            .system_deps
            .as_ref()
            .is_some_and(|operation| operation.status == JobStatus::Queued)
        {
            self.start_system_deps();
            changed = true;
        }
        if persist_needed {
            let _ = self.save();
        }
        changed
    }

    pub fn pause(&mut self, index: usize) -> Result<()> {
        if index >= self.jobs.len() {
            let mut operation_index = index - self.jobs.len();
            if operation_index < self.custom_nodes.len() {
                let operation = &mut self.custom_nodes[operation_index];
                if let Some(abort) = operation.abort.take() {
                    abort.abort();
                }
                if operation.status.is_active() {
                    operation.status = JobStatus::Paused;
                    let name = operation.node.name.clone();
                    self.log(format!("paused custom-node install {name}"));
                }
                return Ok(());
            }
            operation_index -= self.custom_nodes.len();
            if operation_index == 0
                && let Some(operation) = &mut self.python_deps
            {
                if let Some(abort) = operation.abort.take() {
                    abort.abort();
                }
                operation.status = JobStatus::Paused;
                self.log("paused ComfyUI Python dependencies".into());
            } else if operation_index == usize::from(self.python_deps.is_some())
                && self.system_deps.is_some()
            {
                self.log(
                    "system dependency installation cannot be paused safely; let apt-get finish"
                        .into(),
                );
            }
            return Ok(());
        }
        let Some(job) = self.jobs.get_mut(index) else {
            return Ok(());
        };
        if let Some(abort) = job.abort.take() {
            abort.abort();
        }
        if job.status.is_active() {
            job.status = JobStatus::Paused;
            job.bytes_per_second = 0.0;
            let name = job.artifact.name.clone();
            self.log(format!("paused {name}; verified chunks were retained"));
            self.save()?;
        }
        Ok(())
    }

    /// Remove a queue record, optionally discarding its resumable temporary data.
    pub fn remove_download(&mut self, index: usize, delete_temp: bool) -> Result<()> {
        if index < self.jobs.len() {
            if let Some(abort) = self.jobs[index].abort.take() {
                abort.abort();
            }
            let job = self.jobs.remove(index);
            if delete_temp {
                let (temp_dir, _, _) = temp_paths(&job.root, &job.artifact);
                if temp_dir.exists() {
                    fs::remove_dir_all(&temp_dir).with_context(|| {
                        format!("delete temporary download {}", temp_dir.display())
                    })?;
                }
            }
            self.log(format!(
                "removed {} from download manager{}",
                job.artifact.name,
                if delete_temp {
                    " and deleted temporary data"
                } else {
                    "; installed artifact and temporary data were left untouched"
                }
            ));
            return self.save();
        }
        let mut operation_index = index - self.jobs.len();
        if operation_index < self.custom_nodes.len() {
            if let Some(abort) = self.custom_nodes[operation_index].abort.take() {
                abort.abort();
            }
            let operation = self.custom_nodes.remove(operation_index);
            if delete_temp {
                delete_custom_node_temp(&operation.root, &operation.node.id)?;
            }
            self.log(format!(
                "removed {} from download manager",
                operation.node.name
            ));
            return Ok(());
        }
        operation_index -= self.custom_nodes.len();
        if operation_index == 0 && self.python_deps.is_some() {
            if let Some(mut operation) = self.python_deps.take()
                && let Some(abort) = operation.abort.take()
            {
                abort.abort();
            }
            self.log("removed Python dependency operation from download manager".into());
            return Ok(());
        }
        operation_index = operation_index.saturating_sub(usize::from(self.python_deps.is_some()));
        if operation_index == 0 && self.system_deps.is_some() {
            if self
                .system_deps
                .as_ref()
                .is_some_and(|operation| operation.status.is_active())
            {
                anyhow::bail!("active system package installation cannot be removed safely");
            }
            self.system_deps = None;
            self.log("removed system dependency operation from download manager".into());
            return Ok(());
        }
        anyhow::bail!("download record no longer exists")
    }

    pub fn forget_artifacts<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let ids = ids.into_iter().collect::<std::collections::HashSet<_>>();
        for job in &mut self.jobs {
            if ids.contains(job.artifact.id.as_str())
                && let Some(abort) = job.abort.take()
            {
                abort.abort();
            }
        }
        self.jobs
            .retain(|job| !ids.contains(job.artifact.id.as_str()));
        self.save()
    }

    pub fn resume(&mut self, index: usize) -> Result<()> {
        if index >= self.jobs.len() {
            let mut operation_index = index - self.jobs.len();
            if operation_index < self.custom_nodes.len() {
                let operation = &mut self.custom_nodes[operation_index];
                if operation.status == JobStatus::Paused {
                    operation.status = JobStatus::Queued;
                    operation.error = None;
                }
                return Ok(());
            }
            operation_index -= self.custom_nodes.len();
            if operation_index == 0
                && let Some(operation) = &mut self.python_deps
                && operation.status == JobStatus::Paused
            {
                operation.status = JobStatus::Queued;
                operation.error = None;
            } else if operation_index == usize::from(self.python_deps.is_some())
                && let Some(operation) = &mut self.system_deps
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
            let mut operation_index = index - self.jobs.len();
            if operation_index < self.custom_nodes.len() {
                let operation = &mut self.custom_nodes[operation_index];
                if operation.status == JobStatus::Failed {
                    operation.status = JobStatus::Queued;
                    operation.error = None;
                }
                return Ok(());
            }
            operation_index -= self.custom_nodes.len();
            if operation_index == 0
                && let Some(operation) = &mut self.python_deps
                && operation.status == JobStatus::Failed
            {
                operation.status = JobStatus::Queued;
                operation.error = None;
            } else if operation_index == usize::from(self.python_deps.is_some())
                && let Some(operation) = &mut self.system_deps
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
        snapshots.extend(self.custom_nodes.iter().map(|operation| {
            JobSnapshot {
                artifact_id: format!("custom-node:{}", operation.node.id),
                name: operation.node.name.clone(),
                relative_path: operation
                    .root
                    .join("custom_nodes")
                    .join(&operation.node.folder_name)
                    .display()
                    .to_string(),
                status: operation.status,
                downloaded_bytes: 0,
                total_bytes: None,
                bytes_per_second: 0.0,
                error: operation.error.clone(),
                endpoint: Some(operation.node.git_url.clone()),
            }
        }));
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
                endpoint: Some(operation.pip_index_url.clone()),
            });
        }
        if let Some(operation) = &self.system_deps {
            snapshots.push(JobSnapshot {
                artifact_id: "system-dependencies".into(),
                name: "System dependencies".into(),
                relative_path: operation
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.package.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                status: operation.status,
                downloaded_bytes: 0,
                total_bytes: None,
                bytes_per_second: 0.0,
                error: operation.error.clone(),
                endpoint: Some("apt-get".into()),
            });
        }
        snapshots
    }

    pub fn artifact_status(&self, artifact_id: &str) -> Option<JobStatus> {
        self.jobs
            .iter()
            .find(|job| job.artifact.id == artifact_id)
            .map(|job| job.status)
    }

    pub fn custom_node_status(&self, node_id: &str) -> Option<JobStatus> {
        self.custom_nodes
            .iter()
            .find(|operation| operation.node.id == node_id)
            .map(|operation| operation.status)
    }

    pub fn operation_status(&self, id: &str) -> Option<JobStatus> {
        match id {
            "python-dependencies" => self.python_deps.as_ref().map(|operation| operation.status),
            "system-dependencies" => self.system_deps.as_ref().map(|operation| operation.status),
            _ => None,
        }
    }

    pub fn has_active_operations(&self) -> bool {
        self.jobs.iter().any(|job| job.status.is_active())
            || self
                .python_deps
                .as_ref()
                .is_some_and(|operation| operation.status.is_active())
            || self
                .system_deps
                .as_ref()
                .is_some_and(|operation| operation.status.is_active())
            || self
                .custom_nodes
                .iter()
                .any(|operation| operation.status.is_active())
    }

    pub fn active_descriptions(&self) -> Vec<String> {
        self.snapshots()
            .into_iter()
            .filter(|job| job.status.is_active())
            .map(|job| format!("{}: {}", job.name, job.status.label()))
            .collect()
    }

    pub fn logs(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.logs.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
            + self.custom_nodes.len()
            + usize::from(self.python_deps.is_some())
            + usize::from(self.system_deps.is_some())
    }

    pub fn update_chunk_parallelism(&mut self, parallelism: usize) {
        for job in &mut self.jobs {
            if job.status != JobStatus::Downloading {
                job.options.parallelism = parallelism.max(1);
            }
        }
        for operation in &mut self.custom_nodes {
            if let Some(abort) = operation.abort.take() {
                abort.abort();
                operation.status = JobStatus::Paused;
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
        let log_sender = self.sender.clone();
        let stage_id = artifact.id.clone();
        options.log = Some(Arc::new(move |message| {
            let status = match message.as_str() {
                "phase:resolving" => Some(JobStatus::Resolving),
                "phase:downloading" => Some(JobStatus::Downloading),
                "phase:processing" => Some(JobStatus::Processing),
                "phase:installing" => Some(JobStatus::Installing),
                _ => None,
            };
            if let Some(status) = status {
                let _ = log_sender.send(QueueEvent::Stage {
                    artifact_id: stage_id.clone(),
                    status,
                });
            }
            let _ = log_sender.send(QueueEvent::Log(format!("[DOWNLOAD] {message}")));
        }));
        let sender = self.sender.clone();
        let finished_id = artifact.id.clone();
        self.jobs[index].status = JobStatus::Resolving;
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

    fn start_custom_node(&mut self, index: usize) {
        let node = self.custom_nodes[index].node.clone();
        let node_id = node.id.clone();
        let name = node.name.clone();
        let root = self.custom_nodes[index].root.clone();
        let sender = self.sender.clone();
        self.custom_nodes[index].status = JobStatus::Installing;
        self.custom_nodes[index].error = None;
        self.log(format!("started custom-node download {name}"));
        let task_sender = sender.clone();
        let handle = tokio::spawn(async move {
            let result = install_custom_node(root, node, &task_sender)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(QueueEvent::CustomNodeFinished { node_id, result });
        });
        self.custom_nodes[index].abort = Some(handle.abort_handle());
    }

    fn start_python_deps(&mut self) {
        let Some(operation) = &mut self.python_deps else {
            return;
        };
        let root = operation.root.clone();
        let pip_index_url = operation.pip_index_url.clone();
        let sender = self.sender.clone();
        operation.status = JobStatus::Installing;
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

    fn start_system_deps(&mut self) {
        let Some(operation) = &mut self.system_deps else {
            return;
        };
        let dependencies = operation.dependencies.clone();
        let sender = self.sender.clone();
        operation.status = JobStatus::Installing;
        self.log("started system dependencies".into());
        let handle = tokio::spawn(async move {
            let result = install_system_dependencies(&dependencies, &sender)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(QueueEvent::SystemOperationFinished(result));
        });
        if let Some(operation) = &mut self.system_deps {
            operation.abort = Some(handle.abort_handle());
        }
    }

    fn log(&mut self, message: String) {
        let line = format!("[{}] {message}", timestamp());
        self.push_log_line(line.clone());
        if let Some(file) = &mut self.log_file {
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
        let _ = sender.send(QueueEvent::Log(format!(
            "[GIT] Cloning {} from {}",
            node.name, node.git_url
        )));
        if let Err(error) = ComfyManager::clone_repository_quiet(&node.git_url, &temporary).await {
            let _ = tokio::fs::remove_dir_all(&temporary).await;
            return Err(error).with_context(|| format!("git clone failed for {}", node.name));
        }
        tokio::fs::rename(&temporary, &target).await?;
        let _ = sender.send(QueueEvent::Log(format!(
            "[GIT] Installed {} at {}",
            node.name,
            target.display()
        )));
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

fn delete_custom_node_temp(root: &Path, node_id: &str) -> Result<()> {
    let temp_base = root.join("custom_nodes/.comfybox-tmp");
    let Ok(entries) = fs::read_dir(&temp_base) else {
        return Ok(());
    };
    let prefix = format!("{node_id}-");
    for entry in entries.filter_map(std::result::Result::ok) {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            fs::remove_dir_all(entry.path())?;
        }
    }
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
        format!("v7:{}", hex::encode(Sha256::digest(requirements_bytes))),
    )
    .await?;
    Ok(())
}

async fn install_system_dependencies(
    dependencies: &[SystemDependency],
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    let packages = dependencies
        .iter()
        .map(|dependency| dependency.package.as_str())
        .collect::<Vec<_>>();
    let _ = sender.send(QueueEvent::Log(format!(
        "[SYSTEM] Installing {}",
        packages.join(", ")
    )));
    run_logged(Command::new("apt-get").arg("update"), sender).await?;
    run_logged(
        Command::new("apt-get")
            .arg("install")
            .arg("-y")
            .args(&packages)
            .env("DEBIAN_FRONTEND", "noninteractive"),
        sender,
    )
    .await?;
    for dependency in dependencies {
        if !system_package_installed(&dependency.package) {
            anyhow::bail!(
                "{} was not detected after apt-get completed",
                dependency.package
            );
        }
    }
    Ok(())
}

fn command_exists(command: &str) -> bool {
    std::process::Command::new(command)
        .arg("--version")
        .output()
        .is_ok()
}

fn system_package_installed(package: &str) -> bool {
    std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Status}", package])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("install ok installed")
        })
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

pub(crate) fn find_equivalent_node_folder(base: &Path, expected: &str) -> Option<PathBuf> {
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
    ensure_known_linux_runtime(folder_name, sender).await?;
    run_logged(Command::new(python).arg("-c").arg(import), sender).await
}

async fn ensure_known_linux_runtime(
    folder_name: &str,
    sender: &mpsc::UnboundedSender<QueueEvent>,
) -> Result<()> {
    if !cfg!(target_os = "linux")
        || normalized_node_name(folder_name) != "comfyuioutputlistscombiner"
        || linux_has_libegl()
    {
        return Ok(());
    }
    let _ = sender.send(QueueEvent::Log(
        "[SYSTEM] libEGL.so.1 is missing; installing Debian/Ubuntu package libegl1".into(),
    ));
    let mut install = Command::new("apt-get");
    install
        .args(["install", "-y", "libegl1"])
        .env("DEBIAN_FRONTEND", "noninteractive");
    if run_logged(&mut install, sender).await.is_err() {
        let _ = sender.send(QueueEvent::Log(
            "[SYSTEM] Refreshing apt package metadata before retrying libegl1".into(),
        ));
        let mut update = Command::new("apt-get");
        update
            .arg("update")
            .env("DEBIAN_FRONTEND", "noninteractive");
        run_logged(&mut update, sender).await.with_context(
            || "libEGL.so.1 is missing and apt metadata refresh failed; install `libegl1` manually",
        )?;
        let mut retry = Command::new("apt-get");
        retry
            .args(["install", "-y", "libegl1"])
            .env("DEBIAN_FRONTEND", "noninteractive");
        run_logged(&mut retry, sender).await.with_context(
            || "libEGL.so.1 is missing; install Debian/Ubuntu package `libegl1` manually",
        )?;
    }
    if !linux_has_libegl() {
        anyhow::bail!(
            "installed libegl1 but libEGL.so.1 is still unavailable to the dynamic linker"
        );
    }
    Ok(())
}

fn linux_has_libegl() -> bool {
    [
        "/usr/lib/x86_64-linux-gnu/libEGL.so.1",
        "/lib/x86_64-linux-gnu/libEGL.so.1",
        "/usr/lib/aarch64-linux-gnu/libEGL.so.1",
        "/lib/aarch64-linux-gnu/libEGL.so.1",
    ]
    .iter()
    .any(|path| Path::new(path).exists())
        || std::process::Command::new("ldconfig")
            .arg("-p")
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("libEGL.so.1")
            })
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
    let expected = format!("v7:{}", hex::encode(Sha256::digest(requirements)));
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
