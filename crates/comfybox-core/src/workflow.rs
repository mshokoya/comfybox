use crate::catalog::Catalog;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
};

#[derive(Debug, Clone, Default)]
pub struct WorkflowInspection {
    pub artifact_ids: BTreeSet<String>,
    pub custom_node_ids: BTreeSet<String>,
    pub unresolved_model_filenames: BTreeSet<String>,
    pub node_types: BTreeSet<String>,
}

pub fn inspect_workflow(path: &Path, catalog: &Catalog) -> Result<WorkflowInspection> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read workflow {}", path.display()))?;
    let json: Value = serde_json::from_str(&raw).context("parse workflow JSON")?;
    let by_name: HashMap<String, String> = catalog
        .artifacts
        .iter()
        .filter_map(|a| {
            Path::new(&a.relative_path)
                .file_name()
                .and_then(|x| x.to_str())
                .map(|n| (n.to_owned(), a.id.clone()))
        })
        .collect();
    let mut strings = Vec::new();
    if let Some(nodes) = json.get("nodes").and_then(Value::as_array) {
        for node in nodes {
            if let Some(node_type) = node.get("type").and_then(Value::as_str) {
                strings.push(node_type.to_owned());
            }
            if let Some(widgets) = node.get("widgets_values") {
                collect_strings(widgets, &mut strings);
            }
        }
    } else {
        collect_strings(&json, &mut strings);
    }
    let mut out = WorkflowInspection::default();
    for s in strings {
        let filename = s.replace('\\', "/");
        let filename = filename.rsplit('/').next().unwrap_or(&s);
        if let Some(id) = by_name.get(&s).or_else(|| by_name.get(filename)) {
            out.artifact_ids.insert(id.clone());
        } else if looks_like_model(&s) {
            out.unresolved_model_filenames.insert(s.clone());
        }
        for node in &catalog.custom_nodes {
            if node.node_types.iter().any(|t| t == &s) {
                out.custom_node_ids.insert(node.id.clone());
            }
        }
        if !s.contains('/') && !s.contains('\\') && s.len() < 100 {
            out.node_types.insert(s);
        }
    }
    Ok(out)
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(xs) => xs.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}
fn looks_like_model(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.ends_with(".safetensors")
        || l.ends_with(".ckpt")
        || l.ends_with(".pth")
        || l.ends_with(".pt")
        || l.ends_with(".onnx")
}
