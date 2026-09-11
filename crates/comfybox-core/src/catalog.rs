use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Catalog {
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default)]
    pub workflows: Vec<WorkflowDefinition>,
    #[serde(default)]
    pub custom_nodes: Vec<CustomNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub name: String,
    pub url: String,
    pub relative_path: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub gated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionalGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub custom_node_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub family: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub primary_artifact_ids: Vec<String>,
    #[serde(default)]
    pub dependency_artifact_ids: Vec<String>,
    #[serde(default)]
    pub custom_node_ids: Vec<String>,
    #[serde(default)]
    pub optional_groups: Vec<OptionalGroup>,
    #[serde(default)]
    pub discover_query: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub id: String,
    pub name: String,
    pub file: String,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub custom_node_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomNode {
    pub id: String,
    pub name: String,
    pub git_url: String,
    pub folder_name: String,
    #[serde(default)]
    pub node_types: Vec<String>,
}

impl Catalog {
    pub fn from_toml_str(raw: &str) -> Result<Self> {
        let cat: Self = toml::from_str(raw).context("parse catalog TOML")?;
        cat.validate()?;
        Ok(cat)
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("read catalog {}", path.display()))?;
        let cat = match path.extension().and_then(|x| x.to_str()) {
            Some("json") => serde_json::from_str(&raw).context("parse catalog JSON")?,
            _ => toml::from_str(&raw).context("parse catalog TOML")?,
        };
        let cat: Self = cat;
        cat.validate()?;
        Ok(cat)
    }

    pub fn merge(mut self, other: Self) -> Result<Self> {
        merge_by_id(&mut self.artifacts, other.artifacts, |x| &x.id);
        merge_by_id(&mut self.packages, other.packages, |x| &x.id);
        merge_by_id(&mut self.workflows, other.workflows, |x| &x.id);
        merge_by_id(&mut self.custom_nodes, other.custom_nodes, |x| &x.id);
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        ensure_unique(self.artifacts.iter().map(|x| x.id.as_str()), "artifact")?;
        ensure_unique(self.packages.iter().map(|x| x.id.as_str()), "package")?;
        ensure_unique(self.workflows.iter().map(|x| x.id.as_str()), "workflow")?;
        ensure_unique(
            self.custom_nodes.iter().map(|x| x.id.as_str()),
            "custom node",
        )?;
        let artifacts: HashSet<_> = self.artifacts.iter().map(|x| x.id.as_str()).collect();
        let nodes: HashSet<_> = self.custom_nodes.iter().map(|x| x.id.as_str()).collect();
        for p in &self.packages {
            for id in p
                .primary_artifact_ids
                .iter()
                .chain(&p.dependency_artifact_ids)
                .chain(p.optional_groups.iter().flat_map(|g| g.artifact_ids.iter()))
            {
                if !artifacts.contains(id.as_str()) {
                    bail!("package {} references unknown artifact {}", p.id, id);
                }
            }
            for id in p.custom_node_ids.iter().chain(
                p.optional_groups
                    .iter()
                    .flat_map(|g| g.custom_node_ids.iter()),
            ) {
                if !nodes.contains(id.as_str()) {
                    bail!("package {} references unknown custom node {}", p.id, id);
                }
            }
        }
        for w in &self.workflows {
            for id in &w.artifact_ids {
                if !artifacts.contains(id.as_str()) {
                    bail!("workflow {} references unknown artifact {}", w.id, id);
                }
            }
            for id in &w.custom_node_ids {
                if !nodes.contains(id.as_str()) {
                    bail!("workflow {} references unknown custom node {}", w.id, id);
                }
            }
        }
        Ok(())
    }

    pub fn artifact(&self, id: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|x| x.id == id)
    }
    pub fn package(&self, id: &str) -> Option<&Package> {
        self.packages.iter().find(|x| x.id == id)
    }
    pub fn workflow(&self, id: &str) -> Option<&WorkflowDefinition> {
        self.workflows.iter().find(|x| x.id == id)
    }
    pub fn custom_node(&self, id: &str) -> Option<&CustomNode> {
        self.custom_nodes.iter().find(|x| x.id == id)
    }
}

fn ensure_unique<'a>(ids: impl Iterator<Item = &'a str>, what: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            bail!("duplicate {what} id: {id}");
        }
    }
    Ok(())
}

fn merge_by_id<T, F>(base: &mut Vec<T>, incoming: Vec<T>, id: F)
where
    F: Fn(&T) -> &String,
{
    let mut map: HashMap<String, usize> = base
        .iter()
        .enumerate()
        .map(|(i, x)| (id(x).clone(), i))
        .collect();
    for item in incoming {
        if let Some(idx) = map.get(id(&item)).copied() {
            base[idx] = item;
        } else {
            map.insert(id(&item).clone(), base.len());
            base.push(item);
        }
    }
}
