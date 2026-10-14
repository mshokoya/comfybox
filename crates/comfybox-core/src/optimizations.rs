use std::{
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

/// Standard known paths where libtcmalloc shared objects are installed on Linux distributions.
const TCMALLOC_CANDIDATE_PATHS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu/libtcmalloc_minimal.so.4",
    "/usr/lib/x86_64-linux-gnu/libtcmalloc_minimal.so",
    "/usr/lib64/libtcmalloc_minimal.so.4",
    "/usr/lib64/libtcmalloc.so.4",
    "/usr/lib/libtcmalloc_minimal.so.4",
    "/usr/lib/libtcmalloc.so.4",
    "/usr/lib/libtcmalloc_minimal.so",
    "/usr/lib/libtcmalloc.so",
];

/// Finds a valid libtcmalloc shared library on the current Linux system.
pub fn find_tcmalloc_library() -> Option<PathBuf> {
    for candidate in TCMALLOC_CANDIDATE_PATHS {
        let p = Path::new(candidate);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }

    // Dynamic search in /usr/lib if standard paths are absent
    let Ok(output) = Command::new("find")
        .args(["/usr/lib", "-name", "libtcmalloc*minimal*.so*", "-type", "f"])
        .output()
    else {
        return None;
    };

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && Path::new(trimmed).is_file() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }

    None
}

/// Checks whether SageAttention package is available in the target ComfyUI / Python environment.
pub fn is_sage_attention_installed(python_bin: Option<&Path>, comfy_root: Option<&Path>) -> bool {
    is_python_package_installed("sageattention", python_bin, comfy_root)
}

/// Checks whether FlashAttention (`flash_attn`) package is available in the target ComfyUI / Python environment.
pub fn is_flash_attention_installed(python_bin: Option<&Path>, comfy_root: Option<&Path>) -> bool {
    is_python_package_installed("flash_attn", python_bin, comfy_root)
}

fn is_python_package_installed(package_name: &str, python_bin: Option<&Path>, comfy_root: Option<&Path>) -> bool {
    // 1. Check virtualenv site-packages directory directly (fastest, no subprocess)
    if let Some(root) = comfy_root {
        for venv_name in &[".venv", "venv"] {
            let lib_dir = root.join(venv_name).join("lib");
            if lib_dir.is_dir() {
                for entry in WalkDir::new(&lib_dir)
                    .max_depth(3)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.file_type().is_dir() {
                        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                        if name == package_name
                            || name.starts_with(&format!("{package_name}-"))
                            || name.starts_with(&format!("{package_name}."))
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }

    // 2. Fall back to executing python import test
    if let Some(python) = python_bin {
        let script = format!("import {package_name}");
        if let Ok(status) = Command::new(python)
            .args(["-c", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            if status.success() {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcmalloc_candidate_paths_are_not_empty() {
        assert!(!TCMALLOC_CANDIDATE_PATHS.is_empty());
    }

    #[test]
    fn check_package_installed_nonexistent() {
        assert!(!is_python_package_installed(
            "nonexistent_package_xyz_12345",
            None,
            None
        ));
    }
}
