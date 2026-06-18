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

pub fn enrich_panes_fast(panes: &mut [Pane]) {
    let _g = smelt_perf::perf::begin("git.enrich_panes_fast");
    enrich_panes_with(panes, false);
}

pub fn enrich_panes(panes: &mut [Pane]) {
    let _g = smelt_perf::perf::begin("git.enrich_panes");
    enrich_panes_with(panes, true);
}

fn enrich_panes_with(panes: &mut [Pane], include_dirty: bool) {
    let mut unique: HashMap<String, WsInfo> = HashMap::new();
    for p in panes.iter() {
        unique.entry(p.path.clone()).or_insert_with(|| WsInfo {
            short_path: shorten(&p.path),
            project_root: String::new(),
            project_short: String::new(),
            git_branch: String::new(),
            git_dirty: None,
        });
    }

    smelt_perf::perf::record_value("git.unique_paths", unique.len() as u64);
    for (path, info) in unique.iter_mut() {
        info.git_branch = git_branch(path);
        if include_dirty {
            info.git_dirty = Some(git_dirty(path));
        }
        info.project_root = project_root(path);
        info.project_short = shorten(&info.project_root);
    }

    let mut projects: HashMap<String, (String, Option<bool>)> = HashMap::new();
    for info in unique.values() {
        projects
            .entry(info.project_root.clone())
            .or_insert_with(|| {
                (
                    git_branch(&info.project_root),
                    include_dirty.then(|| git_dirty(&info.project_root)),
                )
            });
    }

    for p in panes.iter_mut() {
        if let Some(info) = unique.get(&p.path) {
            p.short_path = info.short_path.clone();
            p.project_root = info.project_root.clone();
            p.project_short = info.project_short.clone();
            p.git_branch = info.git_branch.clone();
            if let Some(dirty) = info.git_dirty {
                p.git_dirty = dirty;
            }
            if let Some((branch, dirty)) = projects.get(&info.project_root) {
                p.project_branch = branch.clone();
                if let Some(dirty) = dirty {
                    p.project_dirty = *dirty;
                }
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
    git_dirty: Option<bool>,
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
    let _g = smelt_perf::perf::begin("git.branch");
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
    let _g = smelt_perf::perf::begin("git.dirty");
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

    let dirty = {
        let _g = smelt_perf::perf::begin("git.status");
        Command::new("git")
            .arg("status")
            .arg("--porcelain")
            .current_dir(dir)
            .output()
            .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
            .unwrap_or(false)
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent-mux-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn fast_enriches_worktree_structure() -> std::io::Result<()> {
        let root = temp_dir("worktree");
        let repo = root.join("repo");
        let worktree = root.join("repo-feature");
        let worktree_git_dir = repo.join(".git/worktrees/repo-feature");
        fs::create_dir_all(&worktree_git_dir)?;
        fs::create_dir_all(&worktree)?;
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n")?;
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )?;

        let mut panes = vec![Pane {
            path: worktree.to_string_lossy().to_string(),
            git_dirty: true,
            project_dirty: true,
            ..Pane::default()
        }];

        enrich_panes_fast(&mut panes);

        assert_eq!(panes[0].short_path, "repo-feature");
        assert_eq!(panes[0].project_root, repo.to_string_lossy());
        assert_eq!(panes[0].project_short, "repo");
        assert_eq!(panes[0].git_branch, "feature");
        assert_eq!(panes[0].project_branch, "main");
        assert!(panes[0].git_dirty);
        assert!(panes[0].project_dirty);

        fs::remove_dir_all(root)?;
        Ok(())
    }
}
