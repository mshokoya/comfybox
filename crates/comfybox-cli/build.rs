use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workflows_dir = manifest_dir.join("../../assets/workflows");
    println!("cargo:rerun-if-changed={}", workflows_dir.display());

    let mut files = fs::read_dir(&workflows_dir)
        .expect("read workflow directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    files.sort();

    let mut generated = String::from(
        "fn dependency_manifest_workflow(id: &str) -> Option<&'static str> {\n    match id {\n",
    );
    for path in files {
        let file = path.file_name().unwrap().to_string_lossy();
        let id = file.trim_end_matches(".json");
        generated.push_str(&format!(
            "        {id:?} => Some(include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../assets/workflows/{file}\"))),\n"
        ));
    }
    generated.push_str("        _ => None,\n    }\n}\n");
    let output =
        PathBuf::from(env::var("OUT_DIR").expect("output dir")).join("dependency_workflows.rs");
    fs::write(output, generated).expect("write generated workflow registry");
}
