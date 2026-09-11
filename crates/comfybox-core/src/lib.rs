pub mod auth;
pub mod catalog;
pub mod comfy;
pub mod config;
pub mod download;
pub mod inventory;
pub mod state;
pub mod storage;
pub mod workflow;

pub use catalog::{Artifact, Catalog, CustomNode, Package, WorkflowDefinition};
pub use comfy::{ComfyInstance, ComfyManager, ComfySource};
pub use config::AppConfig;
pub use download::{DownloadManager, DownloadOptions, InstallOutcome};
pub use inventory::{ArtifactStatus, Inventory};
pub use state::ManagedState;
