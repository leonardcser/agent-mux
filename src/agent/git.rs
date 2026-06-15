use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::agent::Pane;

#[derive(Clone, Debug)]
struct DirtyEntry {
    index_mtime: SystemTime,
    dirty: bool,
}

static DIRTY_CACHE: OnceLock<Mutex<HashMap<String, DirtyEntry>>> = OnceLock::new();

pub fn enrich_panes(panes: &mut [Pane]) {
    let mut unique: HashMap<String, WsInfo> = HashMap::new();
    for p in panes.iter() {
        unique.entry(p.path.clone()).or_insert_with(|| WsInfo {
            short_path: shorten(&p.path),
            project_root: String::new(),
            project_short: String::new(),
            git_branch: String::new(),
            git_dirty: false,
        });
    }

    for (path, info) in unique.iter_mut() {
        info.git_branch = git_branch(path);
        info.git_dirty = git_dirty(path);
        info.project_root = project_root(path);
        info.project_short = shorten(&info.project_root);
    }

    let mut projects: HashMap<String, (String, bool)> = HashMap::new();
    for info in unique.values() {
        projects
            .entry(info.project_root.clone())
            .or_insert_with(|| {
                (
                    git_branch(&info.project_root),
                    git_dirty(&info.project_root),
                )
            });
    }

    for p in panes.iter_mut() {
        if let Some(info) = unique.get(&p.path) {
            p.short_path = info.short_path.clone();
            p.project_root = info.project_root.clone();
            p.project_short = info.project_short.clone();
            p.git_branch = info.git_branch.clone();
            p.git_dirty = info.git_dirty;
            if let Some((branch, dirty)) = projects.get(&info.project_root) {
                p.project_branch = branch.clone();
                p.project_dirty = *dirty;
            }
        }
    }
}

#[derive(Debug)]
struct WsInfo {
    short_path: String,
    project_root: String,
    project_short: String,
    git_branch: String,
    git_dirty: bool,
}

fn shorten(path: &str) -> String {
    let p = Path::new(path);
    let base = p.file_name().and_then(|s| s.to_str()).unwrap_or(path);
    if base == "." || base == "/" || base.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            let home = home.to_string_lossy();
            if path.starts_with(home.as_ref()) {
                return format!("~{}", &path[home.len()..]);
            }
        }
        path.to_string()
    } else {
        base.to_string()
    }
}

fn project_root(dir: &str) -> String {
    let git_path = Path::new(dir).join(".git");
    let Ok(meta) = fs::symlink_metadata(&git_path) else {
        return dir.to_string();
    };
    if meta.is_dir() {
        return dir.to_string();
    }
    let Ok(data) = fs::read_to_string(&git_path) else {
        return dir.to_string();
    };
    let Some(gitdir) = data.trim().strip_prefix("gitdir:") else {
        return dir.to_string();
    };
    let mut gitdir = PathBuf::from(gitdir.trim());
    if !gitdir.is_absolute() {
        gitdir = Path::new(dir).join(gitdir);
    }
    let gitdir = clean_path(gitdir);
    let Some(parent) = gitdir.parent().and_then(|p| p.parent()) else {
        return dir.to_string();
    };
    if parent.file_name().and_then(|s| s.to_str()) != Some(".git") {
        return dir.to_string();
    }
    parent
        .parent()
        .unwrap_or(Path::new(dir))
        .to_string_lossy()
        .to_string()
}

fn resolve_git_dir(dir: &str) -> Option<PathBuf> {
    let git_path = Path::new(dir).join(".git");
    let meta = fs::symlink_metadata(&git_path).ok()?;
    if meta.is_dir() {
        return Some(git_path);
    }
    let data = fs::read_to_string(&git_path).ok()?;
    let gitdir = data.trim().strip_prefix("gitdir:")?.trim();
    let mut p = PathBuf::from(gitdir);
    if !p.is_absolute() {
        p = Path::new(dir).join(p);
    }
    Some(clean_path(p))
}

fn clean_path(path: PathBuf) -> PathBuf {
    path.components().collect()
}

fn git_branch(dir: &str) -> String {
    let Some(gitdir) = resolve_git_dir(dir) else {
        return String::new();
    };
    let Ok(data) = fs::read_to_string(gitdir.join("HEAD")) else {
        return String::new();
    };
    let head = data.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return branch.to_string();
    }
    if head.len() >= 8 {
        head[..8].to_string()
    } else {
        head.to_string()
    }
}

fn git_dirty(dir: &str) -> bool {
    let Some(gitdir) = resolve_git_dir(dir) else {
        return false;
    };
    let Ok(meta) = fs::metadata(gitdir.join("index")) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };

    let cache = DIRTY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(entry) = cache.get(dir)
        && entry.index_mtime == mtime
    {
        return entry.dirty;
    }

    let dirty = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .current_dir(dir)
        .output()
        .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
        .unwrap_or(false);

    if let Ok(mut cache) = cache.lock() {
        cache.insert(
            dir.to_string(),
            DirtyEntry {
                index_mtime: mtime,
                dirty,
            },
        );
    }
    dirty
}
