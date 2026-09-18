use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    generate_catalog_registry(&manifest_dir);
    generate_workflow_registry(&manifest_dir);
}

fn generate_catalog_registry(manifest_dir: &Path) {
    let catalogs_dir = manifest_dir.join("../../assets/catalog");
    println!("cargo:rerun-if-changed={}", catalogs_dir.display());

    let mut files = json_files(&catalogs_dir);
    files.sort_by_key(|path| {
        let name = path.file_name().unwrap().to_string_lossy();
        let order = match name.as_ref() {
            "custom_nodes.json" => 1,
            "packages.json" => 2,
            "workflows.json" => 3,
            "dependencies.json" => 4,
            _ => 0,
        };
        (order, name.into_owned())
    });

    let mut generated =
        String::from("fn builtin_catalogs() -> &'static [&'static str] {\n    &[\n");
    for path in files {
        let file = path.file_name().unwrap().to_string_lossy();
        generated.push_str(&format!(
            "        include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../assets/catalog/{file}\")),\n"
        ));
    }
    generated.push_str("    ]\n}\n");
    let output =
        PathBuf::from(env::var("OUT_DIR").expect("output dir")).join("builtin_catalogs.rs");
    fs::write(output, generated).expect("write generated catalog registry");
}

fn generate_workflow_registry(manifest_dir: &Path) {
    let workflows_dir = manifest_dir.join("../../assets/workflows");
    println!("cargo:rerun-if-changed={}", workflows_dir.display());

    let mut files = json_files(&workflows_dir);
    files.sort();

    let mut generated =
        String::from("fn embedded_workflow(id: &str) -> Option<&'static str> {\n    match id {\n");
    for path in files {
        let file = path.file_name().unwrap().to_string_lossy();
        let id = file.trim_end_matches(".json");
        generated.push_str(&format!(
            "        {id:?} => Some(include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../assets/workflows/{file}\"))),\n"
        ));
    }
    generated.push_str("        _ => None,\n    }\n}\n");
    let output =
        PathBuf::from(env::var("OUT_DIR").expect("output dir")).join("bundled_workflows.rs");
    fs::write(output, generated).expect("write generated workflow registry");
}

fn json_files(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .expect("read workflow directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect()
}
