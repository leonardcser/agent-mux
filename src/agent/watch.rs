use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::agent::git::enrich_panes;
use crate::agent::ipc::{Request, Response, socket_path};
use crate::agent::persist::{
    Snapshot, cache_panes, load_snapshot, load_ui_state, panes_from_snapshot, state_dir,
    ui_pane_state_is_empty, update_ui_state_if_changed, write_heartbeat, write_snapshot_if_changed,
};
use crate::agent::{Pane, Reconciler, list_panes_fast};

type SharedSnapshot = Arc<Mutex<Option<Snapshot>>>;
type Subscribers = Arc<Mutex<Vec<mpsc::Sender<Response>>>>;

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

    let latest_snapshot = Arc::new(Mutex::new(None));
    let subscribers = Arc::new(Mutex::new(Vec::new()));
    start_socket_server(latest_snapshot.clone(), subscribers.clone());
    start_metadata_worker(latest_snapshot.clone(), subscribers.clone());

    let fast_interval = Duration::from_millis(250);
    while !stopped.load(Ordering::SeqCst) {
        let start = Instant::now();
        match refresh_once_with(&mut reconciler, Some(&latest_snapshot), Some(&subscribers)) {
            Ok(()) => {}
            Err(err) => log_error(&format!("refresh failed: {err:#}")),
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
    refresh_once_with(&mut reconciler, None, None)?;
    let _ = refresh_metadata_snapshot()?;
    Ok(())
}

fn refresh_once_with(
    reconciler: &mut Reconciler,
    latest_snapshot: Option<&SharedSnapshot>,
    subscribers: Option<&Subscribers>,
) -> Result<()> {
    write_heartbeat()?;

    let previous = load_snapshot();
    let ui_state = load_ui_state();

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

    if let Some(snapshot) = previous.as_ref() {
        apply_cached_metadata(&mut panes, snapshot);
    }

    reconciler.reconcile(&mut panes);
    let (snapshot, changed) = write_panes_snapshot(reconciler, &panes)?;
    publish_snapshot(latest_snapshot, subscribers, snapshot, changed);
    write_heartbeat()?;

    prune_ui_state(&panes)?;

    Ok(())
}

fn start_metadata_worker(latest_snapshot: SharedSnapshot, subscribers: Subscribers) {
    std::thread::spawn(move || {
        let interval = Duration::from_secs(3);
        loop {
            std::thread::sleep(interval);
            match refresh_metadata_snapshot() {
                Ok(Some(snapshot)) => {
                    publish_snapshot(Some(&latest_snapshot), Some(&subscribers), snapshot, true)
                }
                Ok(None) => {}
                Err(err) => log_error(&format!("metadata refresh failed: {err:#}")),
            }
        }
    });
}

fn refresh_metadata_snapshot() -> Result<Option<Snapshot>> {
    let Some(snapshot) = load_snapshot() else {
        return Ok(None);
    };
    let mut panes = panes_from_snapshot(&snapshot);
    enrich_panes(&mut panes);
    let metadata = cache_panes(&panes);
    merge_metadata_snapshot(&metadata)
}

fn merge_metadata_snapshot(
    metadata: &[crate::agent::persist::CachedPane],
) -> Result<Option<Snapshot>> {
    let Some(mut snapshot) = load_snapshot() else {
        return Ok(None);
    };
    let metadata: std::collections::HashMap<String, &crate::agent::persist::CachedPane> = metadata
        .iter()
        .map(|pane| (pane.pane_key().to_string(), pane))
        .collect();
    for pane in &mut snapshot.panes {
        let Some(meta) = metadata.get(pane.pane_key()) else {
            continue;
        };
        if pane.path != meta.path {
            continue;
        }
        pane.short_path = meta.short_path.clone();
        pane.project_root = meta.project_root.clone();
        pane.project_short = meta.project_short.clone();
        pane.project_branch = meta.project_branch.clone();
        pane.project_dirty = meta.project_dirty;
        pane.git_branch = meta.git_branch.clone();
        pane.git_dirty = meta.git_dirty;
    }
    let changed = write_snapshot_if_changed(snapshot)?;
    Ok(changed.then(load_snapshot).flatten())
}

fn publish_snapshot(
    latest_snapshot: Option<&SharedSnapshot>,
    subscribers: Option<&Subscribers>,
    snapshot: Snapshot,
    changed: bool,
) {
    let mut was_empty = false;
    if let Some(latest_snapshot) = latest_snapshot
        && let Ok(mut latest) = latest_snapshot.lock()
    {
        was_empty = latest.is_none();
        *latest = Some(snapshot.clone());
    }
    if changed || was_empty {
        if let Some(subscribers) = subscribers {
            broadcast_snapshot(subscribers, snapshot);
        }
    }
}

fn prune_ui_state(panes: &[Pane]) -> Result<()> {
    let alive: std::collections::HashMap<String, bool> = panes
        .iter()
        .flat_map(|p| [(p.pane_id.clone(), true), (p.target.clone(), true)])
        .collect();
    update_ui_state_if_changed(|state| {
        state
            .panes
            .retain(|id, ui| alive.contains_key(id) && !ui_pane_state_is_empty(ui));
    })?;
    Ok(())
}

fn write_panes_snapshot(reconciler: &Reconciler, panes: &[Pane]) -> Result<(Snapshot, bool)> {
    let mut cached = cache_panes(panes);
    reconciler.apply_to_cache(&mut cached);
    let changed = write_snapshot_if_changed(Snapshot {
        version: 1,
        generation: 0,
        panes: cached,
        updated_at: None,
    })?;
    let snapshot = load_snapshot().context("load written snapshot")?;
    Ok((snapshot, changed))
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

fn start_socket_server(latest_snapshot: SharedSnapshot, subscribers: Subscribers) {
    std::thread::spawn(move || {
        let path = socket_path();
        let _ = fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                log_error(&format!("bind daemon socket failed: {err:#}"));
                return;
            }
        };

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_socket_client(stream, &latest_snapshot, &subscribers),
                Err(err) => log_error(&format!("accept daemon socket failed: {err:#}")),
            }
        }
    });
}

fn handle_socket_client(
    mut stream: UnixStream,
    latest_snapshot: &SharedSnapshot,
    subscribers: &Subscribers,
) {
    let mut line = String::new();
    let request = match stream.try_clone() {
        Ok(read_stream) => {
            let read = BufReader::new(read_stream).read_line(&mut line);
            read.and_then(|_| {
                serde_json::from_str::<Request>(&line)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
            })
        }
        Err(err) => Err(err),
    };

    match request {
        Ok(Request::GetState) => {
            write_response(&mut stream, state_response(latest_snapshot));
        }
        Ok(Request::Subscribe) => subscribe_client(stream, latest_snapshot, subscribers),
        Err(err) => {
            write_response(
                &mut stream,
                Response::Error {
                    message: err.to_string(),
                },
            );
        }
    }
}

fn subscribe_client(
    mut stream: UnixStream,
    latest_snapshot: &SharedSnapshot,
    subscribers: &Subscribers,
) {
    write_response(&mut stream, state_response(latest_snapshot));
    let (tx, rx) = mpsc::channel();
    if let Ok(mut subscribers) = subscribers.lock() {
        subscribers.push(tx);
    }
    std::thread::spawn(move || {
        for response in rx {
            if !write_response(&mut stream, response) {
                break;
            }
        }
    });
}

fn state_response(latest_snapshot: &SharedSnapshot) -> Response {
    Response::State {
        snapshot: latest_snapshot
            .lock()
            .ok()
            .and_then(|snapshot| snapshot.clone()),
        ui_state: load_ui_state(),
    }
}

fn broadcast_snapshot(subscribers: &Subscribers, snapshot: Snapshot) {
    let response = Response::State {
        snapshot: Some(snapshot),
        ui_state: load_ui_state(),
    };
    if let Ok(mut subscribers) = subscribers.lock() {
        subscribers.retain(|tx| tx.send(response.clone()).is_ok());
    }
}

fn write_response(stream: &mut UnixStream, response: Response) -> bool {
    match serde_json::to_string(&response) {
        Ok(response) => writeln!(stream, "{response}").is_ok(),
        Err(err) => {
            log_error(&format!("encode daemon socket response failed: {err:#}"));
            false
        }
    }
}

pub fn is_running() -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(lock_path()) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => false,
        Err(err) => err.kind() == std::io::ErrorKind::WouldBlock,
    }
}

pub fn log_path() -> PathBuf {
    state_dir().join("watch.log")
}

fn log_error(message: &str) {
    let _ = fs::create_dir_all(state_dir());
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(file, "{} {message}", chrono::Utc::now().to_rfc3339());
    }
}

pub fn lock_path() -> PathBuf {
    state_dir().join("watch.lock")
}
