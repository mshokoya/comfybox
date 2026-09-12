mod dashboard;
mod download_queue;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use comfybox_core::{
    auth::{self, HfTokenSource},
    catalog::Catalog,
    comfy::{ComfyManager, ComfySource, StartOptions, configured_instance},
    config::AppConfig,
    download::{DownloadManager, DownloadOptions},
    inventory::{ArtifactStatus, Inventory},
    state::{ManagedInstall, ManagedState},
    storage,
    workflow::inspect_workflow,
};
use console::style;
use inquire::{Confirm, MultiSelect, Password, PasswordDisplayMode, Select};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
};
use tokio::process::Command;

const BUILTIN_CATALOG: &str = include_str!("../../../assets/catalog/builtin.toml");
const MINIMAX_WORKFLOW: &str = include_str!("../../../assets/workflows/minimax-h3-reference.json");
const DATASET_QWEN_2509_BASIC_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-qwen-edit-2509-basic-angles.json");
const DATASET_QWEN_2509_FACE_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-qwen-edit-2509-face.json");
const DATASET_QWEN_2509_LIFESTYLE_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-qwen-edit-2509-lifestyle.json");
const DATASET_QWEN_2511_BODY_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-qwen-edit-2511-body-angles.json");
const DATASET_FLUX2_KLEIN_BODY_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-flux2-klein-9b-body-angles.json");
const DATASET_KREA2_LIFESTYLE_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-krea2-lifestyle.json");
const DATASET_POST_PRODUCTION_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-post-production.json");
const DATASET_AUTO_CAPTION_WORKFLOW: &str =
    include_str!("../../../assets/workflows/dataset-auto-caption.json");
const KREA2_GENERATE_WORKFLOW: &str = include_str!("../../../assets/workflows/krea2-generate.json");
const FACE_SWAP_COMPARISON_WORKFLOW: &str =
    include_str!("../../../assets/workflows/face-swap-krea2-qwen2511-flux2-klein.json");
const FLUX2_KLEIN_FACE_SWAP_WORKFLOW: &str =
    include_str!("../../../assets/workflows/flux2-klein-face-swap.json");
const KREA2_FACE_SWAP_WORKFLOW: &str =
    include_str!("../../../assets/workflows/krea2-face-swap.json");
const MINIMAX_H3_CHARACTER_SWAP_WORKFLOW: &str =
    include_str!("../../../assets/workflows/minimax-h3-character-swap-bf16-24to60.json");
const QWEN_2511_FACE_SWAP_WORKFLOW: &str =
    include_str!("../../../assets/workflows/qwen-edit-2511-face-swap.json");
const PYPI_OFFICIAL: &str = "https://pypi.org/simple";
const PYPI_ALIBABA: &str = "https://mirrors.aliyun.com/pypi/simple/";

#[derive(Parser, Debug)]
#[command(
    name = "comfybox",
    version,
    about = "Reliable ComfyUI installer and model/workflow package manager"
)]
struct Cli {
    #[arg(long, global = true)]
    catalog: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<CommandTop>,
}

#[derive(Subcommand, Debug)]
enum CommandTop {
    Doctor,
    Locate {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Install {
        #[arg(short = 'd', long)]
        destination: Option<PathBuf>,
        #[arg(long, value_enum)]
        source: Option<ComfySourceArg>,
    },
    PythonDeps,
    Uninstall {
        #[arg(long)]
        yes: bool,
    },
    Start {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8188)]
        port: u16,
    },
    Stop,
    Status,
    Models {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Catalog {
        #[command(subcommand)]
        command: CatalogCommand,
    },
    TempClean,
}

#[derive(Subcommand, Debug)]
enum ModelCommand {
    List,
    Install(ModelInstallArgs),
    Remove {
        id: String,
        #[arg(long)]
        with_deps: bool,
        #[arg(long)]
        yes: bool,
    },
    Discover {
        query: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ComfySourceArg {
    Github,
    Gitee,
}

impl From<ComfySourceArg> for ComfySource {
    fn from(value: ComfySourceArg) -> Self {
        match value {
            ComfySourceArg::Github => Self::GitHub,
            ComfySourceArg::Gitee => Self::Gitee,
        }
    }
}

#[derive(Args, Debug)]
struct ModelInstallArgs {
    id: String,
    #[arg(long)]
    no_deps: bool,
    #[arg(long)]
    force: bool,
    #[arg(long = "optional")]
    optional_groups: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum WorkflowCommand {
    List,
    Inspect {
        id: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    InstallDeps {
        id: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    RemoveDeps {
        id: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    Install {
        id: String,
        #[arg(long)]
        with_deps: bool,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Show,
    SetHfEndpoint {
        endpoint: String,
    },
    SetComfyPath {
        path: PathBuf,
    },
    SetHfToken {
        #[arg(long, conflicts_with = "stdin")]
        from_env: bool,
        #[arg(long)]
        stdin: bool,
    },
    ClearHfToken,
}
#[derive(Subcommand, Debug)]
enum CatalogCommand {
    Validate,
}

fn main() -> Result<()> {
    install_hf_token_for_process()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let mut cfg = AppConfig::load()?;
    let catalog = load_catalog(&cli.catalog)?;
    match cli.command {
        None => interactive(&mut cfg, &catalog).await,
        Some(c) => run(c, &mut cfg, &catalog).await,
    }
}

fn install_hf_token_for_process() -> Result<()> {
    let Some(credential) = auth::resolve_hf_token()? else {
        return Ok(());
    };
    if credential.source() != HfTokenSource::Environment {
        // SAFETY: this runs at the first line of synchronous main, before the
        // Tokio runtime or any application threads are created.
        unsafe { std::env::set_var("HF_TOKEN", credential.token()) };
    }
    Ok(())
}

fn load_catalog(extra: &[PathBuf]) -> Result<Catalog> {
    let mut cat = Catalog::from_toml_str(BUILTIN_CATALOG)?;
    if let Ok(dir) = AppConfig::catalog_dir() {
        if dir.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(&dir)?
                .filter_map(|e| e.ok())
                .map(|x| x.path())
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|x| x.to_str()),
                        Some("toml" | "json")
                    )
                })
                .collect();
            entries.sort();
            for p in entries {
                cat = cat.merge(Catalog::load_file(&p)?)?;
            }
        }
    }
    for p in extra {
        cat = cat.merge(Catalog::load_file(p)?)?;
    }
    Ok(cat)
}

async fn run(cmd: CommandTop, cfg: &mut AppConfig, cat: &Catalog) -> Result<()> {
    match cmd {
        CommandTop::Doctor => doctor(cfg, cat).await,
        CommandTop::Locate { path } => locate(cfg, path),
        CommandTop::Install {
            destination,
            source,
        } => install_comfy(cfg, destination, source).await,
        CommandTop::PythonDeps => {
            let i = configured_instance(cfg)?;
            let py = ComfyManager::install_python_deps(&i).await?;
            println!(
                "{} {}",
                style("✓ Python deps installed:").green(),
                py.display()
            );
            Ok(())
        }
        CommandTop::Uninstall { yes } => uninstall_comfy(cfg, yes),
        CommandTop::Start { host, port } => {
            let i = configured_instance(cfg)?;
            if !download_queue::python_dependencies_ready(&i.root) {
                bail!(
                    "Python dependencies are not installed or requirements.txt changed; run `comfybox python-deps`"
                );
            }
            let mut s = ManagedState::load()?;
            let pid = ComfyManager::start(&i, StartOptions { host: &host, port }, &mut s).await?;
            println!(
                "{} PID {pid}; http://{host}:{port}",
                style("✓ ComfyUI started").green()
            );
            Ok(())
        }
        CommandTop::Stop => {
            let i = configured_instance(cfg)?;
            let mut s = ManagedState::load()?;
            ComfyManager::stop(&i, &mut s)?;
            println!("{}", style("✓ ComfyUI stopped").green());
            Ok(())
        }
        CommandTop::Status => {
            let i = configured_instance(cfg)?;
            let mut s = ManagedState::load()?;
            let up = ComfyManager::status(&i, &mut s)?;
            println!(
                "{}",
                if up {
                    style("running").green()
                } else {
                    style("stopped").yellow()
                }
            );
            if let Some(log) = s.comfy_log {
                println!("log: {log}");
            }
            Ok(())
        }
        CommandTop::Models { command } => models(command, cfg, cat).await,
        CommandTop::Workflow { command } => workflows(command, cfg, cat).await,
        CommandTop::Config { command } => config_cmd(command, cfg),
        CommandTop::Catalog {
            command: CatalogCommand::Validate,
        } => {
            cat.validate()?;
            println!(
                "{} {} artifacts, {} packages, {} workflows",
                style("✓ catalog valid").green(),
                cat.artifacts.len(),
                cat.packages.len(),
                cat.workflows.len()
            );
            Ok(())
        }
        CommandTop::TempClean => temp_clean(cfg),
    }
}

async fn doctor(cfg: &AppConfig, cat: &Catalog) -> Result<()> {
    println!("{}", style("ComfyBox doctor").bold());
    println!(
        "catalog: {} artifacts / {} packages / {} workflows",
        cat.artifacts.len(),
        cat.packages.len(),
        cat.workflows.len()
    );
    println!(
        "ComfyUI: {}",
        cfg.comfy_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not configured".into())
    );
    let env_hf = std::env::var("HF_ENDPOINT").ok();
    println!(
        "HF endpoint: {}",
        cfg.hf_endpoint
            .as_deref()
            .or(env_hf.as_deref())
            .unwrap_or("https://huggingface.co")
    );
    println!(
        "HF token: {}",
        if auth::resolve_hf_token()?.is_some() {
            "present"
        } else {
            "not set"
        }
    );
    for bin in ["git", if cfg!(windows) { "python" } else { "python3" }] {
        let ok = Command::new(bin)
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!("{bin}: {}", if ok { "ok" } else { "missing" });
    }
    if let Some(d) = storage::recommend_storage()? {
        println!(
            "largest recommended volume: {} ({:.1} GiB free)",
            d.mount_point.display(),
            gib(d.available_bytes)
        );
    }
    Ok(())
}

fn locate(cfg: &mut AppConfig, path: Option<PathBuf>) -> Result<()> {
    let selected = if let Some(p) = path {
        p
    } else {
        let found = ComfyManager::discover(&[], 5);
        if found.is_empty() {
            browse_directory(std::env::current_dir()?)?
        } else {
            Select::new(
                "Select ComfyUI installation",
                found.iter().map(|x| x.root.display().to_string()).collect(),
            )
            .prompt()
            .map(PathBuf::from)?
        }
    };
    if !ComfyManager::is_comfy_root(&selected) {
        bail!("not a ComfyUI root: {}", selected.display());
    }
    cfg.comfy_path = Some(fs::canonicalize(selected)?);
    cfg.save()?;
    println!(
        "{} {}",
        style("✓ configured").green(),
        cfg.comfy_path.as_ref().unwrap().display()
    );
    Ok(())
}

async fn install_comfy(
    cfg: &mut AppConfig,
    destination: Option<PathBuf>,
    source: Option<ComfySourceArg>,
) -> Result<()> {
    let source = choose_comfy_source(source)?;
    println!("source: {} — {}", source.name(), source.url());
    let parent = match destination {
        Some(p) => p,
        None => {
            if let Some(d) = storage::recommend_storage()? {
                println!(
                    "recommended: {} ({:.1} GiB free)",
                    d.mount_point.display(),
                    gib(d.available_bytes)
                );
                browse_directory(d.mount_point)?
            } else {
                browse_directory(std::env::current_dir()?)?
            }
        }
    };
    let i = ComfyManager::install(&parent, source).await?;
    cfg.comfy_path = Some(i.root.clone());
    cfg.save()?;
    println!(
        "{} {}",
        style("✓ installed ComfyUI").green(),
        i.root.display()
    );
    Ok(())
}

fn choose_comfy_source(source: Option<ComfySourceArg>) -> Result<ComfySource> {
    if let Some(source) = source {
        return Ok(source.into());
    }
    let gitee = "China mirror (Gitee) — faster and more reliable inside China";
    let github = "Official ComfyUI repository (GitHub)";
    match Select::new("Download ComfyUI from", vec![gitee, github]).prompt()? {
        selected if selected == gitee => Ok(ComfySource::Gitee),
        _ => Ok(ComfySource::GitHub),
    }
}

fn download_options(cfg: &AppConfig, force: bool) -> Result<DownloadOptions> {
    let mut endpoint = cfg
        .hf_endpoint
        .clone()
        .or_else(|| std::env::var("HF_ENDPOINT").ok());
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let mirror = "China mirror — https://hf-mirror.com";
        let official = "Official Hugging Face — https://huggingface.co";
        let selected = Select::new(
            "Download Hugging Face models and dependencies from",
            vec![mirror, official],
        )
        .prompt()?;
        endpoint = Some(
            if selected == mirror {
                "https://hf-mirror.com"
            } else {
                "https://huggingface.co"
            }
            .to_owned(),
        );
    }
    println!(
        "Hugging Face endpoint: {}",
        endpoint.as_deref().unwrap_or("https://huggingface.co")
    );
    Ok(DownloadOptions {
        parallelism: cfg.download_parallelism,
        chunk_size_bytes: cfg.chunk_size_bytes,
        force,
        hf_endpoint: endpoint,
        hf_token: auth::hf_token()?,
        progress: None,
    })
}

fn dashboard_download_options(cfg: &AppConfig, force: bool) -> Result<DownloadOptions> {
    Ok(DownloadOptions {
        parallelism: cfg.download_parallelism,
        chunk_size_bytes: cfg.chunk_size_bytes,
        force,
        hf_endpoint: cfg
            .hf_endpoint
            .clone()
            .or_else(|| std::env::var("HF_ENDPOINT").ok()),
        hf_token: auth::hf_token()?,
        progress: None,
    })
}

fn uninstall_comfy(cfg: &mut AppConfig, yes: bool) -> Result<()> {
    let i = configured_instance(cfg)?;
    let confirmed = yes
        || Confirm::new(&format!(
            "Delete {} and everything inside it?",
            i.root.display()
        ))
        .with_default(false)
        .prompt()?;
    if !confirmed {
        println!("cancelled");
        return Ok(());
    }
    fs::remove_dir_all(&i.root)?;
    cfg.comfy_path = None;
    cfg.save()?;
    println!("{}", style("✓ ComfyUI deleted").green());
    Ok(())
}

async fn models(cmd: ModelCommand, cfg: &AppConfig, cat: &Catalog) -> Result<()> {
    let i = configured_instance(cfg)?;
    let inv = Inventory {
        comfy_root: &i.root,
        catalog: cat,
    };
    match cmd {
        ModelCommand::List => {
            for p in &cat.packages {
                let statuses: Vec<_> = p
                    .primary_artifact_ids
                    .iter()
                    .filter_map(|id| cat.artifact(id))
                    .map(|a| inv.status(a))
                    .collect();
                let mark = if p.primary_artifact_ids.is_empty() {
                    "?"
                } else if statuses
                    .iter()
                    .all(|s| matches!(s, ArtifactStatus::Installed))
                {
                    "✓"
                } else if statuses
                    .iter()
                    .any(|s| matches!(s, ArtifactStatus::Partial { .. }))
                {
                    "◐"
                } else {
                    "○"
                };
                println!("{} {:<26} {}", style(mark).cyan(), p.id, p.name);
            }
            Ok(())
        }
        ModelCommand::Install(args) => {
            install_package(
                &i.root,
                cfg,
                cat,
                &args.id,
                !args.no_deps,
                &args.optional_groups,
                args.force,
            )
            .await
        }
        ModelCommand::Remove { id, with_deps, yes } => {
            remove_package(&i.root, cat, &id, with_deps, yes)
        }
        ModelCommand::Discover { query, limit } => discover_hf(&query, limit).await,
    }
}

async fn install_package(
    root: &Path,
    cfg: &AppConfig,
    cat: &Catalog,
    id: &str,
    with_deps: bool,
    optional: &[String],
    force: bool,
) -> Result<()> {
    let p = cat
        .package(id)
        .with_context(|| format!("unknown package {id}"))?;
    if p.primary_artifact_ids.is_empty() {
        bail!(
            "{} is discovery-only; use `comfybox models discover {}` until a verified manifest is added",
            p.name,
            p.discover_query.as_deref().unwrap_or(&p.id)
        );
    }
    let mut ids = p.primary_artifact_ids.clone();
    if with_deps {
        ids.extend(p.dependency_artifact_ids.clone());
    }
    for gid in optional {
        let g = p
            .optional_groups
            .iter()
            .find(|g| &g.id == gid)
            .with_context(|| format!("unknown optional group {gid}"))?;
        ids.extend(g.artifact_ids.clone());
    }
    ids.sort();
    ids.dedup();
    println!("{}", style(format!("Install plan: {}", p.name)).bold());
    let inv = Inventory {
        comfy_root: root,
        catalog: cat,
    };
    for id in &ids {
        let a = cat.artifact(id).unwrap();
        println!(
            "  {} {:<42} {}",
            status_mark(&inv.status(a)),
            a.name,
            a.relative_path
        );
    }
    let dm = DownloadManager::new()?;
    let opts = download_options(cfg, force)?;
    let mut installed = BTreeSet::new();
    for id in &ids {
        let a = cat.artifact(id).unwrap();
        println!("{} {}", style("→").cyan(), a.name);
        dm.install_artifact(root, a, &opts).await?;
        installed.insert(id.clone());
    }
    let mut node_ids: BTreeSet<String> = p.custom_node_ids.iter().cloned().collect();
    for gid in optional {
        if let Some(g) = p.optional_groups.iter().find(|g| &g.id == gid) {
            node_ids.extend(g.custom_node_ids.iter().cloned());
        }
    }
    for nid in &node_ids {
        install_custom_node(root, cat, nid).await?;
    }
    let mut state = ManagedState::load()?;
    state.packages.insert(
        p.id.clone(),
        ManagedInstall {
            artifact_ids: installed,
            custom_node_ids: node_ids,
        },
    );
    state.save()?;
    println!("{} {}", style("✓ installed").green(), p.name);
    Ok(())
}

fn remove_package(root: &Path, cat: &Catalog, id: &str, with_deps: bool, yes: bool) -> Result<()> {
    let p = cat
        .package(id)
        .with_context(|| format!("unknown package {id}"))?;
    let mut state = ManagedState::load()?;
    let recorded = state.packages.get(id).cloned();
    let mut ids: BTreeSet<String> = if let Some(r) = &recorded {
        r.artifact_ids.clone()
    } else {
        p.primary_artifact_ids.iter().cloned().collect()
    };
    if !with_deps {
        ids.retain(|x| p.primary_artifact_ids.contains(x));
    }
    let confirmed = yes
        || Confirm::new(&format!("Remove {} managed files?", p.name))
            .with_default(false)
            .prompt()?;
    if !confirmed {
        return Ok(());
    }
    for aid in ids {
        let shared_in_state = state.artifact_referenced_elsewhere(&aid, Some(id), None);
        let shared_on_disk =
            catalog_artifact_needed_by_other_installed_package(root, cat, &aid, id);
        if with_deps && (shared_in_state || shared_on_disk) {
            println!("{} shared dependency {aid}", style("keep").yellow());
            continue;
        }
        if let Some(a) = cat.artifact(&aid) {
            let path = root.join(&a.relative_path);
            if path.exists() {
                fs::remove_file(&path)?;
                println!("removed {}", path.display());
            }
        }
    }
    if with_deps {
        if let Some(rec) = &recorded {
            for nid in &rec.custom_node_ids {
                if state.custom_node_referenced_elsewhere(nid, Some(id), None) {
                    println!("{} shared custom node {nid}", style("keep").yellow());
                    continue;
                }
                if let Some(n) = cat.custom_node(nid) {
                    let path = root.join("custom_nodes").join(&n.folder_name);
                    if path.exists() {
                        fs::remove_dir_all(&path)?;
                        println!("removed {}", path.display());
                    }
                }
            }
        }
    }
    state.packages.remove(id);
    state.save()?;
    Ok(())
}

fn catalog_artifact_needed_by_other_installed_package(
    root: &Path,
    cat: &Catalog,
    artifact_id: &str,
    except_id: &str,
) -> bool {
    cat.packages.iter().filter(|p| p.id != except_id).any(|p| {
        let installed = p
            .primary_artifact_ids
            .iter()
            .filter_map(|id| cat.artifact(id))
            .any(|a| root.join(&a.relative_path).exists());
        installed
            && (p
                .primary_artifact_ids
                .iter()
                .chain(&p.dependency_artifact_ids)
                .any(|id| id == artifact_id)
                || p.optional_groups
                    .iter()
                    .flat_map(|g| g.artifact_ids.iter())
                    .any(|id| id == artifact_id))
    })
}

async fn workflows(cmd: WorkflowCommand, cfg: &AppConfig, cat: &Catalog) -> Result<()> {
    let i = configured_instance(cfg)?;
    match cmd {
        WorkflowCommand::List => {
            for w in &cat.workflows {
                println!("{:<28} {}", w.id, w.name)
            }
            Ok(())
        }
        WorkflowCommand::Inspect { id, file } => {
            let p = resolve_workflow_path(id.as_deref(), file, cat)?;
            let r = inspect_workflow(&p, cat)?;
            print_workflow_inspection(&r);
            Ok(())
        }
        WorkflowCommand::InstallDeps { id, file, force } => {
            let p = resolve_workflow_path(id.as_deref(), file, cat)?;
            let r = inspect_workflow(&p, cat)?;
            if !r.unresolved_model_filenames.is_empty() {
                println!(
                    "{} unresolved: {:?}",
                    style("warning:").yellow(),
                    r.unresolved_model_filenames
                )
            }
            let dm = DownloadManager::new()?;
            let opts = download_options(cfg, force)?;
            for aid in &r.artifact_ids {
                dm.install_artifact(&i.root, cat.artifact(aid).unwrap(), &opts)
                    .await?;
            }
            for nid in &r.custom_node_ids {
                install_custom_node(&i.root, cat, nid).await?;
            }
            Ok(())
        }
        WorkflowCommand::RemoveDeps { id, file, yes } => {
            let p = resolve_workflow_path(id.as_deref(), file, cat)?;
            let r = inspect_workflow(&p, cat)?;
            let confirmed = yes
                || Confirm::new("Remove unshared known workflow dependencies?")
                    .with_default(false)
                    .prompt()?;
            if !confirmed {
                return Ok(());
            }
            let state = ManagedState::load()?;
            for aid in r.artifact_ids {
                if state.artifact_referenced_elsewhere(&aid, None, None) {
                    println!("keep shared {aid}");
                    continue;
                }
                if let Some(a) = cat.artifact(&aid) {
                    let f = i.root.join(&a.relative_path);
                    if f.exists() {
                        fs::remove_file(f)?;
                    }
                }
            }
            Ok(())
        }
        WorkflowCommand::Install {
            id,
            with_deps,
            force,
        } => {
            let w = cat
                .workflow(&id)
                .with_context(|| format!("unknown workflow {id}"))?;
            let dest = i.root.join("user/default/workflows");
            fs::create_dir_all(&dest)?;
            let target = dest.join(Path::new(&w.file).file_name().unwrap_or_default());
            fs::write(&target, bundled_workflow(&id)?)?;
            println!(
                "{} {}",
                style("✓ workflow installed").green(),
                target.display()
            );
            if with_deps {
                install_named_workflow_deps(&i.root, &id, cfg, cat, force).await?;
            }
            Ok(())
        }
    }
}

async fn install_named_workflow_deps(
    root: &Path,
    id: &str,
    cfg: &AppConfig,
    cat: &Catalog,
    force: bool,
) -> Result<()> {
    let w = cat
        .workflow(id)
        .with_context(|| format!("unknown workflow {id}"))?;
    let dm = DownloadManager::new()?;
    let opts = download_options(cfg, force)?;
    for aid in &w.artifact_ids {
        dm.install_artifact(
            root,
            cat.artifact(aid)
                .with_context(|| format!("unknown artifact {aid}"))?,
            &opts,
        )
        .await?;
    }
    for nid in &w.custom_node_ids {
        install_custom_node(root, cat, nid).await?;
    }
    let mut state = ManagedState::load()?;
    state.workflows.insert(
        id.to_string(),
        ManagedInstall {
            artifact_ids: w.artifact_ids.iter().cloned().collect(),
            custom_node_ids: w.custom_node_ids.iter().cloned().collect(),
        },
    );
    state.save()?;
    Ok(())
}

fn resolve_workflow_path(
    id: Option<&str>,
    file: Option<PathBuf>,
    cat: &Catalog,
) -> Result<PathBuf> {
    if let Some(f) = file {
        return Ok(f);
    };
    let id = id.context("provide workflow id or --file")?;
    let w = cat
        .workflow(id)
        .with_context(|| format!("unknown workflow {id}"))?;
    let temp = std::env::temp_dir().join(format!("comfybox-{id}.json"));
    fs::write(&temp, bundled_workflow(id)?)?;
    let _ = &w.file;
    Ok(temp)
}
fn bundled_workflow(id: &str) -> Result<&'static str> {
    match id {
        "minimax-h3-reference" => Ok(MINIMAX_WORKFLOW),
        "dataset-qwen-edit-2509-basic-angles" => Ok(DATASET_QWEN_2509_BASIC_WORKFLOW),
        "dataset-qwen-edit-2509-face" => Ok(DATASET_QWEN_2509_FACE_WORKFLOW),
        "dataset-qwen-edit-2509-lifestyle" => Ok(DATASET_QWEN_2509_LIFESTYLE_WORKFLOW),
        "dataset-qwen-edit-2511-body-angles" => Ok(DATASET_QWEN_2511_BODY_WORKFLOW),
        "dataset-flux2-klein-9b-body-angles" => Ok(DATASET_FLUX2_KLEIN_BODY_WORKFLOW),
        "dataset-krea2-lifestyle" => Ok(DATASET_KREA2_LIFESTYLE_WORKFLOW),
        "dataset-post-production" => Ok(DATASET_POST_PRODUCTION_WORKFLOW),
        "dataset-auto-caption" => Ok(DATASET_AUTO_CAPTION_WORKFLOW),
        "krea2-generate" => Ok(KREA2_GENERATE_WORKFLOW),
        "face-swap-krea2-qwen2511-flux2-klein" => Ok(FACE_SWAP_COMPARISON_WORKFLOW),
        "flux2-klein-face-swap" => Ok(FLUX2_KLEIN_FACE_SWAP_WORKFLOW),
        "krea2-face-swap" => Ok(KREA2_FACE_SWAP_WORKFLOW),
        "minimax-h3-character-swap-bf16-24to60" => Ok(MINIMAX_H3_CHARACTER_SWAP_WORKFLOW),
        "qwen-edit-2511-face-swap" => Ok(QWEN_2511_FACE_SWAP_WORKFLOW),
        _ => bail!("workflow {id} is cataloged but not bundled in this build"),
    }
}
fn print_workflow_inspection(r: &comfybox_core::workflow::WorkflowInspection) {
    println!("artifacts: {:?}", r.artifact_ids);
    println!("custom nodes: {:?}", r.custom_node_ids);
    println!("unresolved models: {:?}", r.unresolved_model_filenames)
}

async fn install_custom_node(root: &Path, cat: &Catalog, id: &str) -> Result<()> {
    let node = cat
        .custom_node(id)
        .with_context(|| format!("unknown custom node {id}"))?;
    let base = root.join("custom_nodes");
    fs::create_dir_all(&base)?;
    let target = base.join(&node.folder_name);
    if target.exists() {
        return Ok(());
    }
    let tmpbase = base.join(".comfybox-tmp");
    fs::create_dir_all(&tmpbase)?;
    let tmp = tmpbase.join(format!("{}-{}", node.id, uuid_like()));
    if let Err(error) = ComfyManager::clone_repository(&node.git_url, &tmp).await {
        let _ = fs::remove_dir_all(&tmp);
        return Err(error).with_context(|| format!("git clone failed for {}", node.name));
    }
    fs::rename(&tmp, &target)?;
    let _ = fs::remove_dir(&tmpbase);
    Ok(())
}

async fn discover_hf(query: &str, limit: usize) -> Result<()> {
    let url = format!(
        "https://huggingface.co/api/models?search={}&limit={}",
        urlencoding::encode(query),
        limit
    );
    let v: Value = reqwest::get(url).await?.error_for_status()?.json().await?;
    if let Some(items) = v.as_array() {
        for x in items {
            if let Some(id) = x.get("id").and_then(Value::as_str) {
                println!("{id}")
            }
        }
    }
    Ok(())
}

fn config_cmd(cmd: ConfigCommand, cfg: &mut AppConfig) -> Result<()> {
    match cmd {
        ConfigCommand::Show => println!("{}", serde_json::to_string_pretty(cfg)?),
        ConfigCommand::SetHfEndpoint { endpoint } => {
            cfg.hf_endpoint = Some(endpoint);
            cfg.save()?;
        }
        ConfigCommand::SetComfyPath { path } => {
            if !ComfyManager::is_comfy_root(&path) {
                bail!("not a ComfyUI root")
            }
            cfg.comfy_path = Some(fs::canonicalize(path)?);
            cfg.save()?;
        }
        ConfigCommand::SetHfToken { from_env, stdin } => {
            save_hf_token(from_env, stdin)?;
        }
        ConfigCommand::ClearHfToken => {
            if auth::clear_saved_hf_token()? {
                println!("{}", style("✓ saved HF token removed").green());
            } else {
                println!("no ComfyBox-saved HF token was present");
            }
        }
    }
    Ok(())
}

fn save_hf_token(from_env: bool, stdin: bool) -> Result<()> {
    let token = if from_env {
        std::env::var("HF_TOKEN").context("HF_TOKEN is not set")?
    } else if stdin {
        let mut token = String::new();
        std::io::stdin().read_to_string(&mut token)?;
        token
    } else {
        Password::new("Hugging Face token")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .prompt()?
    };
    let path = auth::save_hf_token(&token)?;
    println!(
        "{} {}",
        style("✓ HF token saved securely at").green(),
        path.display()
    );
    println!("It is available to ComfyBox immediately and on future runs.");
    Ok(())
}
fn temp_clean(cfg: &AppConfig) -> Result<()> {
    let i = configured_instance(cfg)?;
    for e in walkdir::WalkDir::new(&i.root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.file_type().is_dir() && e.file_name() == ".comfybox-tmp" {
            println!("removing {}", e.path().display());
            let _ = fs::remove_dir_all(e.path());
        }
    }
    Ok(())
}

async fn interactive(cfg: &mut AppConfig, cat: &Catalog) -> Result<()> {
    let mut queue = download_queue::DownloadQueue::load(cfg, cat)?;
    loop {
        let action = dashboard::run(cfg, cat, &mut queue)?;
        if matches!(action, dashboard::DashboardAction::Quit) {
            return Ok(());
        }
        let action_label = format!("{action:?}");
        queue.record(format!("action requested: {action_label}"));
        let queues_downloads = matches!(
            action,
            dashboard::DashboardAction::InstallPackage(_)
                | dashboard::DashboardAction::InstallWorkflow(_)
                | dashboard::DashboardAction::InstallPythonDeps
        );
        if let Err(error) = execute_dashboard_action(action, cfg, cat, &mut queue).await {
            queue.record(format!("action failed: {action_label}: {error:#}"));
            eprintln!("{} {error:#}", style("Action failed:").red().bold());
        } else {
            queue.record(format!("action completed: {action_label}"));
        }
        if queues_downloads {
            continue;
        }
        println!("\nPress Enter to return to the dashboard…");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
    }
}

fn prompt_pypi_source(current: &str) -> Result<&'static str> {
    let china = "China mirror (Alibaba Cloud) — faster inside China";
    let official = "Official PyPI — pypi.org";
    let choices = if current == PYPI_ALIBABA {
        vec![china, official]
    } else {
        vec![official, china]
    };
    let selected = Select::new("Python package source", choices)
        .with_help_message("Saved for ComfyBox, pip, ComfyUI, and Node Manager installs")
        .prompt()?;
    Ok(if selected == china {
        PYPI_ALIBABA
    } else {
        PYPI_OFFICIAL
    })
}

async fn apply_pip_index_to_comfy(cfg: &AppConfig, pip_index_url: &str) -> Result<()> {
    let Some(root) = cfg.comfy_path.as_deref() else {
        return Ok(());
    };
    let python = if cfg!(windows) {
        root.join(".venv/Scripts/python.exe")
    } else {
        root.join(".venv/bin/python")
    };
    if !python.is_file() {
        return Ok(());
    }
    let output = Command::new(&python)
        .args([
            "-m",
            "pip",
            "config",
            "set",
            "global.index-url",
            pip_index_url,
        ])
        .env("PIP_INDEX_URL", pip_index_url)
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "pip configuration failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn execute_dashboard_action(
    action: dashboard::DashboardAction,
    cfg: &mut AppConfig,
    cat: &Catalog,
    queue: &mut download_queue::DownloadQueue,
) -> Result<()> {
    match action {
        dashboard::DashboardAction::Quit => Ok(()),
        dashboard::DashboardAction::InstallComfyUi => install_comfy(cfg, None, None).await,
        dashboard::DashboardAction::LocateComfyUi => {
            println!("Searching for ComfyUI installations…");
            locate(cfg, None)
        }
        dashboard::DashboardAction::InstallPythonDeps => {
            let instance = configured_instance(cfg)?;
            let pip_index_url = prompt_pypi_source(&cfg.pypi_index_url)?;
            cfg.pypi_index_url = pip_index_url.to_owned();
            cfg.save()?;
            queue.enqueue_python_deps(&instance.root, pip_index_url)
        }
        dashboard::DashboardAction::ConfigurePypi => {
            let pip_index_url = prompt_pypi_source(&cfg.pypi_index_url)?;
            cfg.pypi_index_url = pip_index_url.to_owned();
            cfg.save()?;
            apply_pip_index_to_comfy(cfg, pip_index_url).await?;
            println!(
                "{} {pip_index_url}",
                style("✓ Python package source set to").green()
            );
            Ok(())
        }
        dashboard::DashboardAction::SetHfToken => save_hf_token(false, false),
        dashboard::DashboardAction::ToggleServer => {
            let instance = configured_instance(cfg)?;
            if !download_queue::python_dependencies_ready(&instance.root) {
                bail!(
                    "install the ComfyUI Python dependencies before starting or stopping the server"
                );
            }
            let mut state = ManagedState::load()?;
            if ComfyManager::status(&instance, &mut state)? {
                ComfyManager::stop(&instance, &mut state)?;
                println!("{}", style("✓ ComfyUI stopped").green());
            } else {
                let pid = ComfyManager::start(
                    &instance,
                    StartOptions {
                        host: "127.0.0.1",
                        port: 8188,
                    },
                    &mut state,
                )
                .await?;
                println!(
                    "{} PID {pid}; http://127.0.0.1:8188",
                    style("✓ ComfyUI started").green()
                );
            }
            Ok(())
        }
        dashboard::DashboardAction::InstallPackage(id) => {
            let instance = configured_instance(cfg)?;
            let package = cat
                .package(&id)
                .with_context(|| format!("unknown package {id}"))?;
            let labels = package
                .optional_groups
                .iter()
                .map(|group| format!("{} — {}", group.id, group.name))
                .collect::<Vec<_>>();
            let optional = if labels.is_empty() {
                Vec::new()
            } else {
                MultiSelect::new("Optional components", labels)
                    .prompt()?
                    .into_iter()
                    .filter_map(|label| label.split(" — ").next().map(str::to_owned))
                    .collect()
            };
            let mut artifact_ids = package.primary_artifact_ids.clone();
            artifact_ids.extend(package.dependency_artifact_ids.iter().cloned());
            for group in &package.optional_groups {
                if optional.contains(&group.id) {
                    artifact_ids.extend(group.artifact_ids.iter().cloned());
                }
            }
            let mut custom_node_ids = package.custom_node_ids.clone();
            for group in &package.optional_groups {
                if optional.contains(&group.id) {
                    custom_node_ids.extend(group.custom_node_ids.iter().cloned());
                }
            }
            let artifacts = artifact_ids
                .iter()
                .map(|artifact_id| {
                    cat.artifact(artifact_id)
                        .cloned()
                        .with_context(|| format!("unknown artifact {artifact_id}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let nodes = custom_node_ids
                .iter()
                .map(|node_id| {
                    cat.custom_node(node_id)
                        .cloned()
                        .with_context(|| format!("unknown custom node {node_id}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let options = dashboard_download_options(cfg, false)?;
            queue.enqueue(&instance.root, artifacts, &options)?;
            queue.enqueue_custom_nodes(&instance.root, nodes);
            let mut state = ManagedState::load()?;
            state.packages.insert(
                id,
                ManagedInstall {
                    artifact_ids: artifact_ids.into_iter().collect(),
                    custom_node_ids: custom_node_ids.into_iter().collect(),
                },
            );
            state.save()
        }
        dashboard::DashboardAction::InstallWorkflow(id) => {
            let instance = configured_instance(cfg)?;
            let workflow = cat
                .workflow(&id)
                .with_context(|| format!("unknown workflow {id}"))?;
            let dest = instance.root.join("user/default/workflows");
            fs::create_dir_all(&dest)?;
            fs::write(
                dest.join(Path::new(&workflow.file).file_name().unwrap_or_default()),
                bundled_workflow(&id)?,
            )?;
            let nodes = workflow
                .custom_node_ids
                .iter()
                .map(|node_id| {
                    cat.custom_node(node_id)
                        .cloned()
                        .with_context(|| format!("unknown custom node {node_id}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let artifacts = workflow
                .artifact_ids
                .iter()
                .map(|artifact_id| {
                    cat.artifact(artifact_id)
                        .cloned()
                        .with_context(|| format!("unknown artifact {artifact_id}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let options = dashboard_download_options(cfg, false)?;
            queue.enqueue(&instance.root, artifacts, &options)?;
            queue.enqueue_custom_nodes(&instance.root, nodes);
            let mut state = ManagedState::load()?;
            state.workflows.insert(
                id,
                ManagedInstall {
                    artifact_ids: workflow.artifact_ids.iter().cloned().collect(),
                    custom_node_ids: workflow.custom_node_ids.iter().cloned().collect(),
                },
            );
            state.save()
        }
    }
}

fn status_mark(s: &ArtifactStatus) -> String {
    match s {
        ArtifactStatus::Installed => style("✓ installed").green().to_string(),
        ArtifactStatus::Partial {
            downloaded_bytes, ..
        } => style(format!("◐ {:.1} GiB partial", gib(*downloaded_bytes)))
            .yellow()
            .to_string(),
        ArtifactStatus::SizeMismatch { .. } => style("! mismatch").red().to_string(),
        ArtifactStatus::Missing => style("○ missing").dim().to_string(),
    }
}
fn browse_directory(start: PathBuf) -> Result<PathBuf> {
    let mut cur = start;
    loop {
        let mut dirs: Vec<PathBuf> = fs::read_dir(&cur)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        let mut labels = vec!["[select this directory]".to_string()];
        if cur.parent().is_some() {
            labels.push("[..]".into())
        }
        labels.extend(
            dirs.iter()
                .map(|p| format!("[{}]", p.file_name().unwrap_or_default().to_string_lossy())),
        );
        let choice =
            Select::new(&format!("Directory: {}", cur.display()), labels.clone()).prompt()?;
        if choice == "[select this directory]" {
            return Ok(cur);
        }
        if choice == "[..]" {
            cur = cur.parent().unwrap().to_path_buf();
            continue;
        }
        let name = choice.trim_start_matches('[').trim_end_matches(']');
        cur = cur.join(name)
    }
}
fn gib(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0 / 1024.0
}
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
