use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::agent::persist::{cache_panes, load_state, state_dir, update_state};
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
    if let Some(state) = load_state() {
        reconciler.seed_from_state(&state);
    }

    let interval = Duration::from_millis(500);
    while !stopped.load(Ordering::SeqCst) {
        let start = Instant::now();
        let state = load_state().unwrap_or_default();
        reconciler.merge_overrides(&state);

        if let Ok(mut panes) = list_panes() {
            reconciler.reconcile(&mut panes);
            let _ = update_state(|current| {
                reconciler.merge_new_overrides(&state, current);
                let stashed: std::collections::HashMap<String, bool> = current
                    .panes
                    .iter()
                    .filter(|cp| cp.stashed)
                    .map(|cp| (cp.pane_key().to_string(), true))
                    .collect();
                for p in &mut panes {
                    p.stashed = stashed.get(&p.pane_id).copied().unwrap_or(false);
                }
                current.panes = cache_panes(&panes);
                reconciler.apply_to_cache(&mut current.panes);
            });
        }

        let elapsed = start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    Ok(())
}

pub fn lock_path() -> PathBuf {
    state_dir().join("watch.lock")
}
