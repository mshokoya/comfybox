use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    #[serde(default)]
    pub system_dependencies: Vec<SystemDependency>,
    #[serde(default)]
    pub python_dependencies: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSource {
    pub title: String,
    pub url: String,
    pub size: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    pub description: String,
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
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub sources: Vec<ArtifactSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemDependency {
    pub id: String,
    pub platform: String,
    pub package: String,
    pub detect: String,
    pub install: String,
    #[serde(default)]
    pub required_by: Vec<String>,
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
        let mut cat: Self = toml::from_str(raw).context("parse catalog TOML")?;
        cat.normalize_sources();
        cat.validate()?;
        Ok(cat)
    }

    pub fn from_json_str(raw: &str) -> Result<Self> {
        let mut cat: Self = serde_json::from_str(raw).context("parse catalog JSON")?;
        cat.normalize_sources();
        cat.validate()?;
        Ok(cat)
    }

    /// Parses one fragment of a split catalog. Cross-file references are validated
    /// by `merge` after the fragment has been combined with the preceding files.
    pub fn from_json_fragment(raw: &str) -> Result<Self> {
        let mut cat: Self = serde_json::from_str(raw).context("parse catalog JSON fragment")?;
        cat.normalize_sources();
        Ok(cat)
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("read catalog {}", path.display()))?;
        let cat = match path.extension().and_then(|x| x.to_str()) {
            Some("json") => serde_json::from_str(&raw).context("parse catalog JSON")?,
            _ => toml::from_str(&raw).context("parse catalog TOML")?,
        };
        let mut cat: Self = cat;
        cat.normalize_sources();
        cat.validate()?;
        Ok(cat)
    }

    fn normalize_sources(&mut self) {
        for artifact in &mut self.artifacts {
            if artifact.sources.is_empty() && !artifact.url.is_empty() {
                artifact.sources.push(ArtifactSource {
                    title: artifact.name.clone(),
                    url: artifact.url.clone(),
                    size: artifact.size_bytes.map(format_human_size),
                    size_bytes: artifact.size_bytes,
                    description: artifact
                        .description
                        .clone()
                        .unwrap_or_else(|| "Default catalog source".into()),
                });
            }
        }
    }

    pub fn merge(mut self, other: Self) -> Result<Self> {
        merge_by_id(&mut self.artifacts, other.artifacts, |x| &x.id);
        merge_by_id(&mut self.packages, other.packages, |x| &x.id);
        merge_by_id(&mut self.workflows, other.workflows, |x| &x.id);
        merge_by_id(&mut self.custom_nodes, other.custom_nodes, |x| &x.id);
        merge_by_id(
            &mut self.system_dependencies,
            other.system_dependencies,
            |x| &x.id,
        );
        if other.python_dependencies.is_some() {
            self.python_dependencies = other.python_dependencies;
        }
        self.enrich_shared_artifact_metadata();
        self.validate()?;
        Ok(self)
    }

    fn enrich_shared_artifact_metadata(&mut self) {
        let known_sizes = self
            .artifacts
            .iter()
            .filter_map(|artifact| {
                artifact
                    .size_bytes
                    .map(|size| (artifact.relative_path.clone(), size))
            })
            .collect::<HashMap<_, _>>();
        for artifact in &mut self.artifacts {
            if artifact.size_bytes.is_none()
                && let Some(size) = known_sizes.get(&artifact.relative_path).copied()
            {
                artifact.size_bytes = Some(size);
            }
            let single_source = artifact.sources.len() == 1;
            for source in &mut artifact.sources {
                if single_source && source.size_bytes.is_none() {
                    source.size_bytes = artifact.size_bytes;
                }
                if source.size.is_none() {
                    source.size = source.size_bytes.map(format_human_size);
                }
            }
        }
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

fn format_human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_artifact_url_becomes_a_visible_default_source() {
        let catalog = Catalog::from_toml_str(
            r#"
                [[artifacts]]
                id = "model"
                name = "Model"
                url = "https://example.com/model.safetensors"
                relative_path = "models/diffusion_models/model.safetensors"
                size_bytes = 1073741824
            "#,
        )
        .unwrap();
        let artifact = catalog.artifact("model").unwrap();
        assert_eq!(artifact.sources.len(), 1);
        assert_eq!(artifact.sources[0].title, "Model");
        assert_eq!(artifact.sources[0].size.as_deref(), Some("1.0 GiB"));
    }

    #[test]
    fn merge_shares_known_size_for_the_same_target_file() {
        let base = Catalog::from_toml_str(
            r#"
                [[artifacts]]
                id = "curated"
                name = "Curated"
                url = "https://example.com/model.safetensors"
                relative_path = "models/diffusion_models/model.safetensors"
                size_bytes = 2048
            "#,
        )
        .unwrap();
        let incoming = Catalog::from_toml_str(
            r#"
                [[artifacts]]
                id = "workflow-model"
                name = "Workflow model"
                url = "https://example.com/model.safetensors"
                relative_path = "models/diffusion_models/model.safetensors"
            "#,
        )
        .unwrap();
        let merged = base.merge(incoming).unwrap();
        let artifact = merged.artifact("workflow-model").unwrap();
        assert_eq!(artifact.size_bytes, Some(2048));
        assert_eq!(artifact.sources[0].size_bytes, Some(2048));
    }
}
