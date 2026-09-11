use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
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
    let mut out: Vec<_> = disks
        .list()
        .iter()
        .map(|d| StorageCandidate {
            mount_point: d.mount_point().to_path_buf(),
            available_bytes: d.available_space(),
            total_bytes: d.total_space(),
            file_system: d.file_system().to_string_lossy().into_owned(),
            removable: d.is_removable(),
        })
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.available_bytes));
    Ok(out)
}

pub fn recommend_storage() -> Result<Option<StorageCandidate>> {
    Ok(storage_candidates()?.into_iter().find(|disk| {
        !disk.removable
            && disk.available_bytes > 2 * 1024 * 1024 * 1024
            && directory_is_writable(&disk.mount_point)
    }))
}

pub fn directory_is_writable(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let probe = path.join(format!(".comfybox-write-probe-{}", uuid::Uuid::new_v4()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            fs::remove_file(probe).is_ok()
        }
        Err(_) => false,
    }
}
