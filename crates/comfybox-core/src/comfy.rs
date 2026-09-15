use crate::{config::AppConfig, state::ManagedState, storage};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use sysinfo::{Pid, ProcessesToUpdate, Signal, System};
use tokio::process::Command;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComfyInstance {
    pub root: PathBuf,
    pub python: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct StartOptions<'a> {
    pub host: &'a str,
    pub port: u16,
}

pub struct ComfyManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComfySource {
    GitHub,
    Gitee,
}

impl ComfySource {
    pub fn name(self) -> &'static str {
        match self {
            Self::GitHub => "Official GitHub",
            Self::Gitee => "China mirror (Gitee)",
        }
    }

    pub fn url(self) -> &'static str {
        match self {
            Self::GitHub => "https://github.com/Comfy-Org/ComfyUI.git",
            Self::Gitee => "https://gitee.com/mirrors/comfyui",
        }
    }
}

impl ComfyManager {
    pub fn is_comfy_root(path: &Path) -> bool {
        path.join("main.py").is_file()
            && path.join("comfy").is_dir()
            && path.join("models").is_dir()
    }

    pub fn discover(extra_roots: &[PathBuf], max_depth: usize) -> Vec<ComfyInstance> {
        let mut roots = vec![];
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd);
        }
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home));
        }
        roots.extend(extra_roots.iter().cloned());
        if let Ok(disks) = storage::storage_candidates() {
            roots.extend(disks.into_iter().map(|x| x.mount_point));
        }
        let mut found = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut scanned_roots = std::collections::HashSet::new();
        for root in roots {
            let root = fs::canonicalize(&root).unwrap_or(root);
            if !scanned_roots.insert(root.clone()) {
                continue;
            }
            for e in WalkDir::new(root)
                .max_depth(max_depth)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    if entry.depth() == 0 {
                        return true;
                    }
                    !matches!(
                        entry.file_name().to_str(),
                        Some(
                            ".git"
                                | ".venv"
                                | "venv"
                                | "models"
                                | "node_modules"
                                | "target"
                                | ".cache"
                                | ".comfybox-tmp"
                        )
                    )
                })
                .filter_map(|e| e.ok())
            {
                if !e.file_type().is_dir() {
                    continue;
                }
                let p = e.path();
                if p.file_name()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("ComfyUI"))
                    .unwrap_or(false)
                    && Self::is_comfy_root(p)
                {
                    if let Ok(canon) = fs::canonicalize(p) {
                        if seen.insert(canon.clone()) {
                            found.push(ComfyInstance {
                                python: find_python(&canon),
                                root: canon,
                            });
                        }
                    }
                }
            }
        }
        found
    }

    pub async fn install(destination_parent: &Path, source: ComfySource) -> Result<ComfyInstance> {
        fs::create_dir_all(destination_parent)?;
        if !storage::directory_is_writable(destination_parent) {
            bail!(
                "destination is not writable: {}",
                destination_parent.display()
            );
        }
        let final_dir = destination_parent.join("ComfyUI");
        if final_dir.exists() {
            bail!("{} already exists", final_dir.display());
        }
        let tmp_root = destination_parent.join(".comfybox-tmp");
        fs::create_dir_all(&tmp_root)?;
        let tmp = tmp_root.join(format!("comfyui-{}", uuid::Uuid::new_v4()));
        if let Err(error) = Self::clone_repository(source.url(), &tmp).await {
            let _ = fs::remove_dir_all(&tmp);
            let _ = fs::remove_dir(&tmp_root);
            return Err(error);
        }
        if let Err(e) = fs::rename(&tmp, &final_dir) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e.into());
        }
        let _ = fs::remove_dir(&tmp_root);
        Ok(ComfyInstance {
            python: find_python(&final_dir),
            root: final_dir,
        })
    }

    pub async fn clone_repository(url: &str, target: &Path) -> Result<()> {
        const ATTEMPTS: usize = 3;
        for attempt in 1..=ATTEMPTS {
            if target.exists() {
                fs::remove_dir_all(target)?;
            }
            let mut command = Command::new("git");
            command.kill_on_drop(true);
            if attempt > 1 {
                command.arg("-c").arg("http.version=HTTP/1.1");
            }
            let status = command
                .arg("clone")
                .arg("--depth")
                .arg("1")
                .arg(url)
                .arg(target)
                .status()
                .await
                .context("failed to launch git; install Git first")?;
            if status.success() {
                return Ok(());
            }
            let _ = fs::remove_dir_all(target);
            if attempt < ATTEMPTS {
                tracing::warn!(attempt, url, "git clone failed; retrying with HTTP/1.1");
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
            }
        }
        bail!("git clone failed after {ATTEMPTS} attempts: {url}")
    }

    pub async fn install_python_deps(instance: &ComfyInstance) -> Result<PathBuf> {
        let python = if let Some(p) = &instance.python {
            p.clone()
        } else {
            let python_cmd = if cfg!(windows) { "python" } else { "python3" };
            let venv = instance.root.join(".venv");
            let status = Command::new(python_cmd)
                .arg("-m")
                .arg("venv")
                .arg(&venv)
                .status()
                .await?;
            if !status.success() {
                bail!("failed to create Python virtualenv")
            }
            find_python(&instance.root).context("venv created but Python executable not found")?
        };
        let req = instance.root.join("requirements.txt");
        if !req.exists() {
            bail!("requirements.txt not found")
        }
        let wheelhouse = instance
            .root
            .join(".comfybox-tmp")
            .join(format!("pip-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&wheelhouse)?;
        let dl = Command::new(&python)
            .arg("-m")
            .arg("pip")
            .arg("download")
            .arg("-r")
            .arg(&req)
            .arg("-d")
            .arg(&wheelhouse)
            .status()
            .await?;
        if !dl.success() {
            let _ = fs::remove_dir_all(&wheelhouse);
            bail!("pip download failed")
        }
        let ins = Command::new(&python)
            .arg("-m")
            .arg("pip")
            .arg("install")
            .arg("--no-index")
            .arg("--find-links")
            .arg(&wheelhouse)
            .arg("-r")
            .arg(&req)
            .status()
            .await?;
        let _ = fs::remove_dir_all(&wheelhouse);
        if !ins.success() {
            bail!("pip install failed")
        }
        Ok(python)
    }

    pub async fn start(
        instance: &ComfyInstance,
        opts: StartOptions<'_>,
        state: &mut ManagedState,
    ) -> Result<u32> {
        if Self::status(instance, state)? {
            bail!("ComfyUI is already running; stop it before starting another instance")
        }
        let python = instance
            .python
            .clone()
            .or_else(|| find_python(&instance.root))
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }));
        let log_dir = instance.root.join(".comfybox");
        fs::create_dir_all(&log_dir)?;
        let log_path = log_dir.join("comfyui.log");
        let stdout = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let stderr = stdout.try_clone()?;
        let mut child = Command::new(&python)
            .current_dir(&instance.root)
            .arg("main.py")
            .arg("--listen")
            .arg(opts.host)
            .arg("--port")
            .arg(opts.port.to_string())
            .arg("--enable-manager")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        if let Some(status) = child.try_wait()? {
            bail!(
                "ComfyUI exited immediately with {status}; see {}",
                log_path.display()
            );
        }
        let pid = child.id().context("ComfyUI started without PID")?;
        state.comfy_pid = Some(pid);
        state.comfy_log = Some(log_path.to_string_lossy().into_owned());
        state.save()?;
        Ok(pid)
    }

    pub fn status(instance: &ComfyInstance, state: &mut ManagedState) -> Result<bool> {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        if let Some(pid_u32) = state.comfy_pid
            && sys
                .process(Pid::from_u32(pid_u32))
                .is_some_and(|process| process_looks_like_comfy(process, &instance.root))
        {
            return Ok(true);
        }
        if let Some(pid) = discover_comfy_pid(&instance.root) {
            state.comfy_pid = Some(pid);
            state.save()?;
            return Ok(true);
        }
        if state.comfy_pid.take().is_some() {
            state.save()?;
        }
        Ok(false)
    }

    pub fn stop(instance: &ComfyInstance, state: &mut ManagedState) -> Result<()> {
        if !Self::status(instance, state)? {
            bail!("no running ComfyUI process found")
        }
        let Some(pid_u32) = state.comfy_pid else {
            bail!("no managed ComfyUI process")
        };
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let Some(proc_) = sys.process(Pid::from_u32(pid_u32)) else {
            state.comfy_pid = None;
            state.save()?;
            return Ok(());
        };
        if !process_looks_like_comfy(proc_, &instance.root)
            && !pid_looks_like_comfy_from_commands(pid_u32, &instance.root)
        {
            bail!("PID {pid_u32} no longer looks like this ComfyUI process; refusing to kill it")
        }
        let sent = proc_
            .kill_with(Signal::Term)
            .unwrap_or_else(|| proc_.kill());
        if !sent {
            bail!("failed to signal ComfyUI process {pid_u32}")
        }
        state.comfy_pid = None;
        state.save()?;
        Ok(())
    }
}

fn discover_comfy_pid(root: &Path) -> Option<u32> {
    let output = std::process::Command::new("pgrep")
        .args(["-af", "python.*main.py"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    for line in listing.lines() {
        let pid = line.split_whitespace().next()?.parse::<u32>().ok()?;
        if pid_looks_like_comfy_from_commands(pid, root) {
            return Some(pid);
        }
    }
    None
}

fn pid_looks_like_comfy_from_commands(pid: u32, root: &Path) -> bool {
    let Ok(ps) = std::process::Command::new("ps")
        .args(["-ww", "-o", "pid=,ppid=,args=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let args = String::from_utf8_lossy(&ps.stdout);
    if !ps.status.success() || !args.contains("main.py") {
        return false;
    }
    let proc_cwd = PathBuf::from(format!("/proc/{pid}/cwd"));
    fs::read_link(proc_cwd).is_ok_and(|cwd| paths_refer_to_same_location(&cwd, root))
        || args.contains(&root.join("main.py").to_string_lossy().into_owned())
        || command_uses_comfy_python(&args, root)
}

fn find_python(root: &Path) -> Option<PathBuf> {
    let candidates = if cfg!(windows) {
        vec![
            root.join(".venv/Scripts/python.exe"),
            root.join("venv/Scripts/python.exe"),
        ]
    } else {
        vec![root.join(".venv/bin/python"), root.join("venv/bin/python")]
    };
    candidates.into_iter().find(|p| p.is_file())
}

fn process_looks_like_comfy(p: &sysinfo::Process, root: &Path) -> bool {
    let cmd = p
        .cmd()
        .iter()
        .map(|x| x.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    if !cmd.contains("main.py") {
        return false;
    }
    let absolute_main = root.join("main.py").to_string_lossy().into_owned();
    if cmd.contains(&absolute_main) {
        return true;
    }
    command_uses_comfy_python(&cmd, root)
        || p.cwd()
            .is_some_and(|cwd| paths_refer_to_same_location(cwd, root))
}

fn command_uses_comfy_python(command: &str, root: &Path) -> bool {
    let candidates = if cfg!(windows) {
        [
            root.join(".venv/Scripts/python.exe"),
            root.join("venv/Scripts/python.exe"),
        ]
    } else {
        [root.join(".venv/bin/python"), root.join("venv/bin/python")]
    };
    candidates
        .iter()
        .any(|python| command.contains(&python.to_string_lossy().into_owned()))
}

fn paths_refer_to_same_location(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub fn configured_instance(cfg: &AppConfig) -> Result<ComfyInstance> {
    let root = cfg
        .comfy_path
        .clone()
        .context("ComfyUI path is not configured; run `comfybox locate` first")?;
    if !ComfyManager::is_comfy_root(&root) {
        bail!(
            "configured path is not a valid ComfyUI root: {}",
            root.display()
        )
    }
    Ok(ComfyInstance {
        python: find_python(&root),
        root,
    })
}

#[cfg(test)]
mod process_tests {
    use super::{command_uses_comfy_python, paths_refer_to_same_location};

    #[test]
    fn identical_paths_match_without_canonicalization() {
        let path = std::path::Path::new("/nonexistent/comfyui");
        assert!(paths_refer_to_same_location(path, path));
    }

    #[test]
    fn relative_main_is_owned_when_it_uses_the_comfy_venv() {
        let root = std::path::Path::new("/autodl-fs/data/ComfyUI");
        let command = "/autodl-fs/data/ComfyUI/.venv/bin/python main.py --listen 127.0.0.1 --port 8188 --enable-manager";
        assert!(command_uses_comfy_python(command, root));
    }

    #[test]
    fn another_comfy_install_is_not_owned() {
        let root = std::path::Path::new("/autodl-fs/data/ComfyUI");
        let command = "/other/ComfyUI/.venv/bin/python main.py --port 8188";
        assert!(!command_uses_comfy_python(command, root));
    }
}
