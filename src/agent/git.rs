use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent::Pane;

pub fn enrich_panes(panes: &mut [Pane]) {
    let _g = smelt_perf::perf::begin("git.enrich_panes");
    enrich_panes_fast(panes);
    let dirty = dirty_statuses(panes);
    for pane in panes {
        if let Some(Some(value)) = dirty.get(&pane.path) {
            pane.git_dirty = *value;
        }
        if let Some(Some(value)) = dirty.get(&pane.project_root) {
            pane.project_dirty = *value;
        }
    }
}

pub fn dirty_statuses(panes: &[Pane]) -> HashMap<String, Option<bool>> {
    let mut dirty = HashMap::new();
    for pane in panes {
        for path in [&pane.path, &pane.project_root] {
            dirty.entry(path.clone()).or_insert_with(|| git_dirty(path));
        }
    }
    dirty
}

pub fn enrich_panes_fast(panes: &mut [Pane]) {
    let _g = smelt_perf::perf::begin("git.enrich_panes_fast");
    let mut unique: HashMap<String, WsInfo> = HashMap::new();
    for p in panes.iter() {
        unique.entry(p.path.clone()).or_insert_with(|| {
            let project_root = project_root(&p.path);
            WsInfo {
                short_path: shorten(&p.path),
                project_short: shorten(&project_root),
                project_root,
                git_branch: git_branch(&p.path),
            }
        });
    }

    smelt_perf::perf::record_value("git.unique_paths", unique.len() as u64);

    let mut projects = HashMap::new();
    for info in unique.values() {
        projects
            .entry(info.project_root.clone())
            .or_insert_with(|| git_branch(&info.project_root));
    }

    for p in panes.iter_mut() {
        if let Some(info) = unique.get(&p.path) {
            p.short_path = info.short_path.clone();
            p.project_root = info.project_root.clone();
            p.project_short = info.project_short.clone();
            p.git_branch = info.git_branch.clone();
            if let Some(branch) = projects.get(&info.project_root) {
                p.project_branch = branch.clone();
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

fn git_dirty(dir: &str) -> Option<bool> {
    let _g = smelt_perf::perf::begin("git.dirty");
    if resolve_git_dir(dir).is_none() {
        return Some(false);
    }
    let _g = smelt_perf::perf::begin("git.status");
    let output = Command::new("git")
        .args(["--no-optional-locks", "status", "--porcelain"])
        .current_dir(dir)
        .output()
        .ok()?;
    output.status.success().then_some(!output.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PaneId;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent-mux-{name}-{}-{nanos}", std::process::id()))
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn dirty_status_tracks_changes_without_index_updates() -> std::io::Result<()> {
        let repo = temp_dir("dirty");
        fs::create_dir_all(&repo)?;
        git(&repo, &["init"]);
        fs::write(repo.join("tracked"), "original")?;
        git(&repo, &["add", "tracked"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "initial",
            ],
        );
        let path = repo.to_str().unwrap();
        let mut panes = vec![Pane {
            path: path.to_string(),
            ..Pane::new(PaneId::parse("%1").unwrap())
        }];
        let index_mtime = fs::metadata(repo.join(".git/index"))?.modified()?;
        for (file, contents, dirty) in [
            ("tracked", Some("original"), false),
            ("tracked", Some("modified"), true),
            ("tracked", Some("original"), false),
            ("untracked", Some("new"), true),
            ("untracked", None, false),
        ] {
            if let Some(contents) = contents {
                fs::write(repo.join(file), contents)?;
            } else {
                fs::remove_file(repo.join(file))?;
            }
            enrich_panes(&mut panes);
            assert_eq!(panes[0].git_dirty, dirty);
            assert_eq!(panes[0].project_dirty, dirty);
            assert_eq!(
                fs::metadata(repo.join(".git/index"))?.modified()?,
                index_mtime
            );
        }
        fs::remove_dir_all(repo)
    }

    #[test]
    fn dirty_status_detects_untracked_files_before_first_commit() -> std::io::Result<()> {
        let repo = temp_dir("unborn");
        fs::create_dir_all(&repo)?;
        git(&repo, &["init"]);
        fs::write(repo.join("untracked"), "new")?;
        assert_eq!(git_dirty(repo.to_str().unwrap()), Some(true));
        fs::remove_dir_all(repo)
    }

    #[test]
    fn failed_git_status_preserves_dirty_metadata() -> std::io::Result<()> {
        let repo = temp_dir("invalid");
        fs::create_dir_all(repo.join(".git"))?;
        let mut panes = vec![Pane {
            path: repo.to_string_lossy().to_string(),
            git_dirty: true,
            project_dirty: true,
            ..Pane::new(PaneId::parse("%1").unwrap())
        }];
        enrich_panes(&mut panes);
        assert!(panes[0].git_dirty);
        assert!(panes[0].project_dirty);
        fs::remove_dir_all(repo)
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
            ..Pane::new(PaneId::parse("%1").unwrap())
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
