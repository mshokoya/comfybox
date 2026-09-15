use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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

    pub fn from_dependency_manifest(raw: &str) -> Result<Self> {
        let manifest: DependencyManifest =
            serde_json::from_str(raw).context("parse dependency manifest JSON")?;
        let mut artifacts = Vec::with_capacity(manifest.artifacts.len());
        let mut artifact_by_path = HashMap::new();
        for (id, entry) in manifest.artifacts {
            let sources = entry
                .source
                .into_iter()
                .map(|(title, url, size, description)| {
                    let size_bytes = size.as_deref().and_then(parse_human_size);
                    ArtifactSource {
                        title,
                        url,
                        size,
                        size_bytes,
                        description,
                    }
                })
                .collect::<Vec<_>>();
            let primary = sources
                .first()
                .with_context(|| format!("artifact {id} has no download source"))?;
            artifact_by_path.insert(entry.relative_path.clone(), id.clone());
            artifacts.push(Artifact {
                id,
                name: primary.title.clone(),
                url: primary.url.clone(),
                relative_path: entry.relative_path,
                size_bytes: primary.size_bytes,
                sha256: entry.sha256,
                gated: false,
                description: Some(primary.description.clone()),
                sources,
            });
        }
        artifacts.sort_by(|a, b| a.name.cmp(&b.name));

        let mut custom_nodes = manifest
            .custom_nodes
            .into_iter()
            .map(|node| {
                let folder_name = node
                    .repository
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(&node.id)
                    .trim_end_matches(".git")
                    .to_string();
                CustomNode {
                    name: humanize_id(&node.id),
                    folder_name,
                    git_url: node.repository,
                    node_types: node.aliases,
                    id: node.id,
                }
            })
            .collect::<Vec<_>>();
        custom_nodes.sort_by(|a, b| a.name.cmp(&b.name));

        let mut workflows = Vec::with_capacity(manifest.workflows.len());
        for (file, workflow) in manifest.workflows {
            let id = file.trim_end_matches(".json").to_string();
            let artifact_ids = workflow
                .artifacts
                .iter()
                .map(|path| {
                    artifact_by_path.get(path).cloned().with_context(|| {
                        format!("workflow {file} references unknown artifact {path}")
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            workflows.push(WorkflowDefinition {
                name: humanize_id(&id),
                id,
                file,
                artifact_ids,
                custom_node_ids: workflow.custom_nodes,
            });
        }
        workflows.sort_by(|a, b| a.name.cmp(&b.name));

        let packages = artifacts
            .iter()
            .map(|artifact| Package {
                id: format!("artifact.{}", artifact.id),
                name: artifact.name.clone(),
                family: artifact_family(&artifact.relative_path).to_string(),
                description: artifact.description.clone(),
                primary_artifact_ids: vec![artifact.id.clone()],
                dependency_artifact_ids: Vec::new(),
                custom_node_ids: Vec::new(),
                optional_groups: Vec::new(),
                discover_query: None,
            })
            .collect();

        let mut system_dependencies = Vec::new();
        for (platform, dependencies) in manifest.system_dependencies {
            for dependency in dependencies {
                system_dependencies.push(SystemDependency {
                    id: dependency.id,
                    platform: platform.clone(),
                    package: dependency.debian_package,
                    detect: dependency.detect,
                    install: dependency.install_debian,
                    required_by: dependency.required_by,
                });
            }
        }

        let catalog = Self {
            artifacts,
            packages,
            workflows,
            custom_nodes,
            system_dependencies,
            python_dependencies: Some(manifest.python_dependencies),
        };
        catalog.validate()?;
        Ok(catalog)
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

#[derive(Deserialize)]
struct DependencyManifest {
    artifacts: BTreeMap<String, DependencyArtifact>,
    workflows: BTreeMap<String, DependencyWorkflow>,
    custom_nodes: Vec<DependencyCustomNode>,
    python_dependencies: Value,
    #[serde(default)]
    system_dependencies: BTreeMap<String, Vec<DependencySystemDependency>>,
}

#[derive(Deserialize)]
struct DependencyArtifact {
    relative_path: String,
    source: Vec<(String, String, Option<String>, String)>,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Deserialize)]
struct DependencyWorkflow {
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    custom_nodes: Vec<String>,
}

#[derive(Deserialize)]
struct DependencyCustomNode {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    repository: String,
}

#[derive(Deserialize)]
struct DependencySystemDependency {
    id: String,
    debian_package: String,
    #[serde(default)]
    required_by: Vec<String>,
    detect: String,
    install_debian: String,
}

fn parse_human_size(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number = parts.next()?.parse::<f64>().ok()?;
    let multiplier = match parts.next()?.to_ascii_uppercase().as_str() {
        "B" => 1.0,
        "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        _ => return None,
    };
    Some((number * multiplier) as u64)
}

fn humanize_id(id: &str) -> String {
    id.replace(['_', '-'], " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn artifact_family(path: &str) -> &str {
    path.strip_prefix("models/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("runtime models")
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
