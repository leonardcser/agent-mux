use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::agent::persist::{
    Snapshot, cache_panes, load_snapshot, load_ui_state, state_dir, update_ui_state, write_snapshot,
};
use crate::agent::{Reconciler, list_panes};

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

    let interval = Duration::from_millis(500);
    while !stopped.load(Ordering::SeqCst) {
        let start = Instant::now();
        let _ = refresh_once_with(&mut reconciler);

        let elapsed = start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    Ok(())
}

pub fn refresh_once() -> Result<()> {
    let mut reconciler = Reconciler::new();
    if let Some(snapshot) = load_snapshot() {
        reconciler.seed_from_snapshot(&snapshot);
    }
    refresh_once_with(&mut reconciler)
}

fn refresh_once_with(reconciler: &mut Reconciler) -> Result<()> {
    let ui_state = load_ui_state();
    reconciler.merge_overrides(&ui_state);

    let mut panes = list_panes()?;
    for p in &mut panes {
        if let Some(ui) = ui_state
            .panes
            .get(&p.pane_id)
            .or_else(|| ui_state.panes.get(&p.target))
        {
            p.stashed = ui.stashed;
        }
    }
    reconciler.reconcile(&mut panes);

    let mut cached = cache_panes(&panes);
    reconciler.apply_to_cache(&mut cached);
    write_snapshot(Snapshot {
        version: 1,
        panes: cached,
        updated_at: None,
    })?;

    let overrides = reconciler.override_entries();
    update_ui_state(|state| {
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

pub fn lock_path() -> PathBuf {
    state_dir().join("watch.lock")
}
