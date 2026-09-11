use crate::{
    catalog::{Artifact, Catalog},
    download::temp_paths,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStatus {
    Missing,
    Installed,
    Partial {
        downloaded_bytes: u64,
        expected_bytes: Option<u64>,
    },
    SizeMismatch {
        actual: u64,
        expected: u64,
    },
}

#[derive(Debug, Clone)]
pub struct Inventory<'a> {
    pub comfy_root: &'a Path,
    pub catalog: &'a Catalog,
}

impl<'a> Inventory<'a> {
    pub fn artifact_path(&self, artifact: &Artifact) -> PathBuf {
        self.comfy_root.join(&artifact.relative_path)
    }

    pub fn status(&self, artifact: &Artifact) -> ArtifactStatus {
        let final_path = self.artifact_path(artifact);
        if let Ok(meta) = fs::metadata(&final_path) {
            if let Some(expected) = artifact.size_bytes {
                if meta.len() != expected {
                    return ArtifactStatus::SizeMismatch {
                        actual: meta.len(),
                        expected,
                    };
                }
            }
            return ArtifactStatus::Installed;
        }
        let (_, part, ranges) = temp_paths(self.comfy_root, artifact);
        if part.exists() {
            let downloaded = ranges
                .and_then(|p| fs::read_to_string(p).ok())
                .and_then(|s| serde_json::from_str::<crate::download::RangeState>(&s).ok())
                .map(|r| {
                    r.completed
                        .iter()
                        .map(|x| x.end.saturating_sub(x.start) + 1)
                        .sum()
                })
                .unwrap_or(0);
            return ArtifactStatus::Partial {
                downloaded_bytes: downloaded,
                expected_bytes: artifact.size_bytes,
            };
        }
        ArtifactStatus::Missing
    }
}
