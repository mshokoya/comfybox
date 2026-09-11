use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sysinfo::Disks;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCandidate {
    pub mount_point: PathBuf,
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub file_system: String,
    pub removable: bool,
}

pub fn storage_candidates() -> Result<Vec<StorageCandidate>> {
    let disks = Disks::new_with_refreshed_list();
    let mut out: Vec<_> = disks.list().iter().map(|d| StorageCandidate {
        mount_point: d.mount_point().to_path_buf(),
        available_bytes: d.available_space(),
        total_bytes: d.total_space(),
        file_system: d.file_system().to_string_lossy().into_owned(),
        removable: d.is_removable(),
    }).collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.available_bytes));
    Ok(out)
}

pub fn recommend_storage() -> Result<Option<StorageCandidate>> {
    Ok(storage_candidates()?.into_iter().find(|d| !d.removable && d.available_bytes > 2 * 1024 * 1024 * 1024))
}
