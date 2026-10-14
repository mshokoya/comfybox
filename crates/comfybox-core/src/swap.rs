use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSwap {
    pub path: PathBuf,
    pub total_kib: u64,
    pub used_kib: u64,
}

pub fn parse_proc_swaps(content: &str) -> Vec<ActiveSwap> {
    let mut result = Vec::new();
    for line in content.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let path = PathBuf::from(parts[0]);
            let total_kib = parts[2].parse::<u64>().unwrap_or(0);
            let used_kib = parts[3].parse::<u64>().unwrap_or(0);
            result.push(ActiveSwap {
                path,
                total_kib,
                used_kib,
            });
        }
    }
    result
}

pub fn list_active_swaps() -> Vec<ActiveSwap> {
    let Ok(content) = fs::read_to_string("/proc/swaps") else {
        return Vec::new();
    };
    parse_proc_swaps(&content)
}

pub fn find_active_swap_by_path(path: &Path) -> Option<ActiveSwap> {
    let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    list_active_swaps().into_iter().find(|s| {
        let s_canon = fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
        s_canon == canon || s.path == path
    })
}

pub fn is_swap_active(path: &Path) -> bool {
    find_active_swap_by_path(path).is_some()
}

pub fn enable_swap_file(swap_file: &Path, size_gb: u32) -> Result<()> {
    if is_swap_active(swap_file) {
        return Ok(());
    }
    if let Some(parent) = swap_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create swap directory {}", parent.display()))?;
    }

    let need_allocation = match fs::metadata(swap_file) {
        Ok(meta) => {
            let expected_bytes = (size_gb as u64) * 1024 * 1024 * 1024;
            meta.len() < expected_bytes.saturating_sub(64 * 1024 * 1024)
        }
        Err(_) => true,
    };

    if need_allocation {
        let fallocate_status = Command::new("fallocate")
            .arg("-l")
            .arg(format!("{size_gb}G"))
            .arg(swap_file)
            .status();

        let fallocate_success = fallocate_status.map(|s| s.success()).unwrap_or(false);
        if !fallocate_success {
            let dd_status = Command::new("dd")
                .arg("if=/dev/zero")
                .arg(format!("of={}", swap_file.display()))
                .arg("bs=1M")
                .arg(format!("count={}", size_gb * 1024))
                .status()
                .context("run dd to allocate swap file")?;
            if !dd_status.success() {
                let _ = fs::remove_file(swap_file);
                bail!("failed to allocate swap file using fallocate and dd (check available disk space)");
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(swap_file, fs::Permissions::from_mode(0o600));
    }

    let mkswap_output = Command::new("mkswap")
        .arg(swap_file)
        .output()
        .context("run mkswap")?;
    if !mkswap_output.status.success() {
        let _ = fs::remove_file(swap_file);
        bail!(
            "mkswap failed for {}: {}",
            swap_file.display(),
            String::from_utf8_lossy(&mkswap_output.stderr).trim()
        );
    }

    let swapon_output = Command::new("swapon")
        .arg(swap_file)
        .output()
        .context("run swapon")?;
    if !swapon_output.status.success() {
        let err = String::from_utf8_lossy(&swapon_output.stderr).trim().to_string();
        // Remove the allocated file so it does not waste disk space
        let _ = fs::remove_file(swap_file);
        if err.contains("Operation not permitted") {
            bail!(
                "swapon failed: Operation not permitted.\n\
                 AutoDL and Docker containers restrict low-level kernel memory management (CAP_SYS_ADMIN).\n\
                 Custom swap files cannot be activated inside container environments. To avoid OOM, use TCMalloc and the --lowvram preset."
            );
        }
        bail!("swapon failed for {}: {err}", swap_file.display());
    }

    // Best-effort swappiness adjustment to prefer RAM before swap
    let _ = Command::new("sysctl").arg("vm.swappiness=10").output();

    Ok(())
}

pub fn disable_swap_file(swap_file: &Path) -> Result<()> {
    let status = Command::new("swapoff")
        .arg(swap_file)
        .status()
        .context("run swapoff")?;
    if !status.success() {
        bail!("swapoff failed for {}", swap_file.display());
    }
    Ok(())
}

pub fn remove_swap_file(swap_file: &Path) -> Result<()> {
    if is_swap_active(swap_file) {
        let _ = disable_swap_file(swap_file);
    }
    if swap_file.exists() {
        fs::remove_file(swap_file)
            .with_context(|| format!("remove swap file {}", swap_file.display()))?;
    }
    if let Some(parent) = swap_file.parent() {
        if parent.file_name().and_then(|n| n.to_str()) == Some("comfybox-swap") {
            let _ = fs::remove_dir(parent);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_swaps_example() {
        let sample = "Filename				Type		Size		Used		Priority\n/root/autodl-tmp/comfybox-swap/swapfile  file		33554428	1024		-2\n";
        let parsed = parse_proc_swaps(sample);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].path,
            PathBuf::from("/root/autodl-tmp/comfybox-swap/swapfile")
        );
        assert_eq!(parsed[0].total_kib, 33554428);
        assert_eq!(parsed[0].used_kib, 1024);
    }

    #[test]
    fn parse_empty_proc_swaps() {
        let sample = "Filename				Type		Size		Used		Priority\n";
        let parsed = parse_proc_swaps(sample);
        assert!(parsed.is_empty());
    }
}
