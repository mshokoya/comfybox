use crate::config::{atomic_write_json, AppConfig};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManagedInstall {
    #[serde(default)] pub artifact_ids: BTreeSet<String>,
    #[serde(default)] pub custom_node_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManagedState {
    #[serde(default)] pub packages: BTreeMap<String, ManagedInstall>,
    #[serde(default)] pub workflows: BTreeMap<String, ManagedInstall>,
    pub comfy_pid: Option<u32>,
    pub comfy_log: Option<String>,
}

impl ManagedState {
    pub fn load() -> Result<Self> {
        let path = AppConfig::state_path()?;
        if !path.exists() { return Ok(Self::default()); }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    pub fn save(&self) -> Result<()> { atomic_write_json(&AppConfig::state_path()?, self) }

    pub fn artifact_referenced_elsewhere(&self, artifact_id: &str, except_package: Option<&str>, except_workflow: Option<&str>) -> bool {
        self.packages.iter().any(|(id, x)| Some(id.as_str()) != except_package && x.artifact_ids.contains(artifact_id)) ||
        self.workflows.iter().any(|(id, x)| Some(id.as_str()) != except_workflow && x.artifact_ids.contains(artifact_id))
    }

    pub fn custom_node_referenced_elsewhere(&self, node_id: &str, except_package: Option<&str>, except_workflow: Option<&str>) -> bool {
        self.packages.iter().any(|(id, x)| Some(id.as_str()) != except_package && x.custom_node_ids.contains(node_id)) ||
        self.workflows.iter().any(|(id, x)| Some(id.as_str()) != except_workflow && x.custom_node_ids.contains(node_id))
    }
}
