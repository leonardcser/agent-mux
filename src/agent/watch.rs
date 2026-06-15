use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::agent::git::enrich_panes;
use crate::agent::persist::{
    Snapshot, cache_panes, load_snapshot, load_ui_state, state_dir, update_ui_state_if_changed,
    write_heartbeat, write_snapshot_if_changed,
};
use crate::agent::{Pane, Reconciler, list_panes_fast};

pub fn run() -> Result<()> {
    fs::create_dir_all(state_dir()).context("create state dir")?;
    let mut lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path())
        .context("open watch lock")?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    lock.set_len(0).ok();
    lock.seek(SeekFrom::Start(0)).ok();
    write!(lock, "{}", std::process::id()).ok();

    let stopped = Arc::new(AtomicBool::new(false));
    let stop_flag = stopped.clone();
    ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::SeqCst);
    })
    .ok();

    let mut reconciler = Reconciler::new();
    if let Some(snapshot) = load_snapshot() {
        reconciler.seed_from_snapshot(&snapshot);
    }

    let fast_interval = Duration::from_millis(500);
    let slow_interval = Duration::from_secs(3);
    let mut last_slow = Instant::now() - slow_interval;
    while !stopped.load(Ordering::SeqCst) {
        let start = Instant::now();
        let enrich_git = last_slow.elapsed() >= slow_interval;
        if refresh_once_with(&mut reconciler, enrich_git).is_ok() && enrich_git {
            last_slow = Instant::now();
        }

        let elapsed = start.elapsed();
        if elapsed < fast_interval {
            std::thread::sleep(fast_interval - elapsed);
        }
    }

    Ok(())
}

pub fn refresh_once() -> Result<()> {
    let mut reconciler = Reconciler::new();
    if let Some(snapshot) = load_snapshot() {
        reconciler.seed_from_snapshot(&snapshot);
    }
    refresh_once_with(&mut reconciler, true)
}

fn refresh_once_with(reconciler: &mut Reconciler, enrich_git: bool) -> Result<()> {
    let previous = load_snapshot();
    let ui_state = load_ui_state();
    reconciler.merge_overrides(&ui_state);

    let mut panes = list_panes_fast()?;
    for p in &mut panes {
        if let Some(ui) = ui_state
            .panes
            .get(&p.pane_id)
            .or_else(|| ui_state.panes.get(&p.target))
        {
            p.stashed = ui.stashed;
        }
    }

    let should_enrich_git = enrich_git || metadata_missing_or_changed(&panes, previous.as_ref());
    if let Some(snapshot) = previous.as_ref() {
        apply_cached_metadata(&mut panes, snapshot);
    }

    reconciler.reconcile(&mut panes);
    write_panes_snapshot(reconciler, &panes)?;
    write_heartbeat()?;

    if should_enrich_git {
        enrich_panes(&mut panes);
        write_panes_snapshot(reconciler, &panes)?;
        write_heartbeat()?;
    }

    let overrides = reconciler.override_entries();
    update_ui_state_if_changed(|state| {
        let alive: std::collections::HashMap<String, bool> =
            panes.iter().map(|p| (p.pane_id.clone(), true)).collect();
        state.panes.retain(|id, _| alive.contains_key(id));
        for (id, status, content_hash) in &overrides {
            let ui = state.panes.entry(id.clone()).or_default();
            ui.status_override = Some(status.as_i32());
            ui.content_hash = content_hash.clone();
        }
        for (id, ui) in &mut state.panes {
            if !overrides
                .iter()
                .any(|(override_id, _, _)| override_id == id)
            {
                ui.status_override = None;
                ui.content_hash.clear();
            }
        }
    })?;

    Ok(())
}

fn write_panes_snapshot(reconciler: &Reconciler, panes: &[Pane]) -> Result<()> {
    let mut cached = cache_panes(panes);
    reconciler.apply_to_cache(&mut cached);
    let _ = write_snapshot_if_changed(Snapshot {
        version: 1,
        generation: 0,
        panes: cached,
        updated_at: None,
    })?;
    Ok(())
}

fn metadata_missing_or_changed(panes: &[Pane], snapshot: Option<&Snapshot>) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };
    let cached: std::collections::HashMap<String, &crate::agent::persist::CachedPane> = snapshot
        .panes
        .iter()
        .map(|cp| (cp.pane_key().to_string(), cp))
        .collect();

    panes.iter().any(|p| {
        let cached = cached.get(&p.pane_id).or_else(|| cached.get(&p.target));
        cached.is_none_or(|cached| cached.path != p.path || cached.project_root.is_empty())
    })
}

fn apply_cached_metadata(panes: &mut [Pane], snapshot: &Snapshot) {
    let cached: std::collections::HashMap<String, &crate::agent::persist::CachedPane> = snapshot
        .panes
        .iter()
        .map(|cp| (cp.pane_key().to_string(), cp))
        .collect();

    for p in panes {
        let Some(cached) = cached.get(&p.pane_id).or_else(|| cached.get(&p.target)) else {
            continue;
        };
        if cached.path != p.path {
            continue;
        }
        p.short_path = cached.short_path.clone();
        p.project_root = cached.project_root.clone();
        p.project_short = cached.project_short.clone();
        p.project_branch = cached.project_branch.clone();
        p.project_dirty = cached.project_dirty;
        p.git_branch = cached.git_branch.clone();
        p.git_dirty = cached.git_dirty;
    }
}

pub fn lock_path() -> PathBuf {
    state_dir().join("watch.lock")
}
