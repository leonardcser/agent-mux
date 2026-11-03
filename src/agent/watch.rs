use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::agent::git::{dirty_statuses, enrich_panes_fast};
use crate::agent::ipc::{Request, Response, socket_path};
use crate::agent::persist::{
    Snapshot, UiPaneState, UiState, cache_panes, load_snapshot, load_ui_state, panes_from_snapshot,
    snapshot_path, state_dir, ui_pane_state_is_empty, update_snapshot, update_snapshot_at,
    update_ui_state_if_changed, write_heartbeat,
};
use crate::agent::{Pane, Reconciler, list_panes_fast};

type SharedSnapshot = Arc<Mutex<Option<Snapshot>>>;
type Subscribers = Arc<Mutex<Vec<mpsc::Sender<Response>>>>;

pub fn run() -> Result<()> {
    fs::create_dir_all(state_dir()).context("create state dir")?;
    let mut lock = OpenOptions::new()
        .create(true)
        .truncate(false)
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
    let last_refresh = Arc::new(Mutex::new(None));
    let (refresh_tx, refresh_rx) = mpsc::channel();
    start_socket_server(
        latest_snapshot.clone(),
        subscribers.clone(),
        stopped.clone(),
        last_refresh.clone(),
        refresh_tx,
    )?;
    start_metadata_worker(latest_snapshot.clone(), subscribers.clone());

    let fast_interval = Duration::from_millis(500);
    let mut waiting: Vec<UnixStream> = Vec::new();
    while !stopped.load(Ordering::SeqCst) {
        waiting.extend(refresh_rx.try_iter());
        let start = Instant::now();
        let result = refresh_once_with(&mut reconciler, Some(&latest_snapshot), Some(&subscribers))
            .map_err(|err| format!("{err:#}"));
        match &result {
            Ok(()) => {
                *last_refresh.lock().unwrap() = Some(Instant::now());
            }
            Err(err) => log_error(&format!("refresh failed: {err}")),
        }
        // Only initialize subscriptions requested before this status check started.
        for mut stream in waiting.drain(..) {
            match &result {
                Ok(()) => subscribe_client(stream, &latest_snapshot, &subscribers),
                Err(message) => {
                    let response = Response::Error {
                        message: message.clone(),
                    };
                    std::thread::spawn(move || write_response(&mut stream, response));
                }
            }
        }
        if let Ok(stream) = refresh_rx.recv_timeout(fast_interval.saturating_sub(start.elapsed())) {
            waiting.push(stream);
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

    let ui_state = load_ui_state();

    let mut panes = list_panes_fast()?;
    for p in &mut panes {
        if let Some(ui) = ui_state
            .panes
            .get(p.pane_id.as_str())
            .or_else(|| ui_state.panes.get(&p.target))
        {
            p.stashed = ui.stashed;
        }
    }

    reconciler.reconcile(&mut panes);
    let (snapshot, changed) = write_panes_snapshot(reconciler, &panes, snapshot_path())?;
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
    enrich_panes_fast(&mut panes);
    let dirty = dirty_statuses(&panes);
    let metadata = cache_panes(&panes);
    merge_metadata_snapshot(&metadata, &dirty)
}

fn merge_metadata_snapshot(
    metadata: &[crate::agent::persist::CachedPane],
    dirty: &std::collections::HashMap<String, Option<bool>>,
) -> Result<Option<Snapshot>> {
    let updated = update_snapshot(|previous| {
        let mut snapshot = previous?.clone();
        apply_metadata(&mut snapshot, metadata, dirty);
        Some(snapshot)
    })?;
    Ok(updated.and_then(|(snapshot, changed)| changed.then_some(snapshot)))
}

fn apply_metadata(
    snapshot: &mut Snapshot,
    metadata: &[crate::agent::persist::CachedPane],
    dirty: &std::collections::HashMap<String, Option<bool>>,
) {
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
        pane.git_branch = meta.git_branch.clone();
        if let Some(Some(value)) = dirty.get(&pane.project_root) {
            pane.project_dirty = *value;
        }
        if let Some(Some(value)) = dirty.get(&pane.path) {
            pane.git_dirty = *value;
        }
    }
}

fn publish_snapshot(
    latest_snapshot: Option<&SharedSnapshot>,
    subscribers: Option<&Subscribers>,
    snapshot: Snapshot,
    changed: bool,
) {
    // Keep publication ordered with the shared state update.
    let mut latest = latest_snapshot.and_then(|latest| latest.lock().ok());
    let mut needs_broadcast = changed;
    if let Some(latest) = latest.as_mut() {
        if latest
            .as_ref()
            .is_some_and(|current| current.revision > snapshot.revision)
        {
            return;
        }
        // A hook refresh can commit a newer snapshot without this poll writing it.
        needs_broadcast |= latest
            .as_ref()
            .is_none_or(|current| current.revision < snapshot.revision);
        **latest = Some(snapshot.clone());
    }
    if needs_broadcast && let Some(subscribers) = subscribers {
        broadcast_snapshot(subscribers, snapshot);
    }
}

fn prune_ui_state(panes: &[Pane]) -> Result<()> {
    let pane_state: std::collections::HashMap<&str, &Pane> = panes
        .iter()
        .flat_map(|p| [(p.pane_id.as_str(), p), (p.target.as_str(), p)])
        .collect();
    update_ui_state_if_changed(|state| {
        for (id, ui) in &mut state.panes {
            let Some(pane) = pane_state.get(id.as_str()) else {
                continue;
            };
            ui.parked_since = ui
                .parked_since
                .filter(|since| pane.status.is_working() && pane.park_epoch == Some(*since));
            update_pane_read_state(ui, &pane.content_hash, pane.window_active);
        }
        state
            .panes
            .retain(|id, ui| pane_state.contains_key(id.as_str()) && !ui_pane_state_is_empty(ui));
    })?;
    Ok(())
}

fn update_pane_read_state(ui: &mut UiPaneState, content_hash: &str, focused: bool) {
    if focused {
        ui.forced_unread = false;
        ui.read_content_hash = None;
    } else if ui.read_content_hash.as_deref() != Some(content_hash) {
        ui.read_content_hash = None;
    }
}

fn write_panes_snapshot(
    reconciler: &Reconciler,
    panes: &[Pane],
    path: PathBuf,
) -> Result<(Snapshot, bool)> {
    update_snapshot_at(path, |previous| {
        let mut panes = panes.to_vec();
        if let Some(previous) = previous {
            apply_cached_metadata(&mut panes, previous);
        }
        if panes
            .iter()
            .any(|pane| pane.short_path.is_empty() || pane.project_root.is_empty())
        {
            enrich_panes_fast(&mut panes);
        }
        let mut cached = cache_panes(&panes);
        reconciler.apply_to_cache(&mut cached);
        Some(Snapshot {
            version: 1,
            panes: cached,
            ..Snapshot::default()
        })
    })?
    .context("update pane snapshot")
}

fn apply_cached_metadata(panes: &mut [Pane], snapshot: &Snapshot) {
    let cached: std::collections::HashMap<String, &crate::agent::persist::CachedPane> = snapshot
        .panes
        .iter()
        .map(|cp| (cp.pane_key().to_string(), cp))
        .collect();

    for p in panes {
        let Some(cached) = cached
            .get(p.pane_id.as_str())
            .or_else(|| cached.get(&p.target))
        else {
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

fn start_socket_server(
    latest_snapshot: SharedSnapshot,
    subscribers: Subscribers,
    stopped: Arc<AtomicBool>,
    last_refresh: Arc<Mutex<Option<Instant>>>,
    refresh_tx: mpsc::Sender<UnixStream>,
) -> Result<()> {
    let path = socket_path();
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).context("bind daemon socket")?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_socket_client(
                    stream,
                    &latest_snapshot,
                    &subscribers,
                    &stopped,
                    &last_refresh,
                    &refresh_tx,
                ),
                Err(err) => log_error(&format!("accept daemon socket failed: {err:#}")),
            }
        }
    });
    Ok(())
}

fn handle_socket_client(
    mut stream: UnixStream,
    latest_snapshot: &SharedSnapshot,
    subscribers: &Subscribers,
    stopped: &AtomicBool,
    last_refresh: &Mutex<Option<Instant>>,
    refresh_tx: &mpsc::Sender<UnixStream>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
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
        Ok(Request::Status) => {
            let healthy = last_refresh.lock().ok().is_some_and(|last| {
                last.is_some_and(|time| time.elapsed() < Duration::from_secs(10))
            });
            write_response(
                &mut stream,
                Response::Status {
                    build: crate::agent::daemon::build_info().clone(),
                    healthy,
                },
            );
        }
        Ok(Request::Shutdown) => {
            stopped.store(true, Ordering::SeqCst);
            write_response(&mut stream, Response::Stopping);
        }
        Ok(Request::GetState) => {
            write_response(&mut stream, state_response(latest_snapshot));
        }
        Ok(Request::SubscribeFresh) => {
            if let Err(mut err) = refresh_tx.send(stream) {
                write_response(
                    &mut err.0,
                    Response::Error {
                        message: "watcher stopped".into(),
                    },
                );
            }
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
    let rx = register_subscriber(latest_snapshot, subscribers, load_ui_state());
    std::thread::spawn(move || {
        for response in rx {
            if !write_response(&mut stream, response) {
                break;
            }
        }
    });
}

fn register_subscriber(
    latest_snapshot: &SharedSnapshot,
    subscribers: &Subscribers,
    ui_state: UiState,
) -> mpsc::Receiver<Response> {
    let (tx, rx) = mpsc::channel();
    // Match publication lock order. Queue initial state before registering, with
    // no disk or socket I/O under either lock.
    let latest = latest_snapshot.lock().unwrap();
    let mut subscribers = subscribers.lock().unwrap();
    tx.send(Response::State {
        snapshot: latest.clone(),
        ui_state,
    })
    .expect("new subscriber receiver");
    subscribers.push(tx);
    rx
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_subscriber_does_not_block_publication_or_reorder_initial_state() {
        let latest = Arc::new(Mutex::new(Some(Snapshot {
            revision: 1,
            ..Snapshot::default()
        })));
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let rx = register_subscriber(&latest, &subscribers, UiState::default());
        assert!(latest.try_lock().is_ok());
        assert!(subscribers.try_lock().is_ok());
        // Publish while the subscriber has not consumed its initial state.
        publish_snapshot(
            Some(&latest),
            Some(&subscribers),
            Snapshot {
                revision: 2,
                ..Snapshot::default()
            },
            true,
        );
        for revision in [1, 2] {
            let Response::State {
                snapshot: Some(snapshot),
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("expected snapshot");
            };
            assert_eq!(snapshot.revision, revision);
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn fast_poll_preserves_metadata_committed_after_pane_discovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("snapshot.json");
        let panes = vec![Pane {
            path: "/repo".to_string(),
            ..Pane::new(crate::agent::PaneId::parse("%1").unwrap())
        }];
        let reconciler = Reconciler::new();
        write_panes_snapshot(&reconciler, &panes, path.clone()).unwrap();

        // The metadata worker commits after the fast poll has discovered clean panes.
        update_snapshot_at(path.clone(), |previous| {
            let mut snapshot = previous.unwrap().clone();
            snapshot.panes[0].git_dirty = true;
            snapshot.panes[0].project_dirty = true;
            Some(snapshot)
        })
        .unwrap();
        let (snapshot, changed) = write_panes_snapshot(&reconciler, &panes, path).unwrap();
        assert!(snapshot.panes[0].git_dirty);
        assert!(snapshot.panes[0].project_dirty);
        assert!(!changed);
        assert_eq!(snapshot.revision, 2);
    }

    #[test]
    fn metadata_merge_preserves_live_pane_state_and_skips_changed_paths() {
        let panes = vec![Pane {
            path: "/repo".to_string(),
            project_root: "/repo".to_string(),
            ..Pane::new(crate::agent::PaneId::parse("%1").unwrap())
        }];
        let metadata = cache_panes(&panes);
        let dirty = std::collections::HashMap::from([("/repo".to_string(), Some(true))]);
        let mut snapshot = Snapshot {
            version: 1,
            panes: cache_panes(&panes),
            ..Snapshot::default()
        };
        snapshot.panes[0].content_hash = "new content".to_string();
        apply_metadata(&mut snapshot, &metadata, &dirty);
        assert!(snapshot.panes[0].git_dirty);
        assert!(snapshot.panes[0].project_dirty);
        assert_eq!(snapshot.panes[0].content_hash, "new content");
        snapshot.panes[0].path = "/other".to_string();
        snapshot.panes[0].git_dirty = false;
        snapshot.panes[0].project_dirty = false;
        apply_metadata(&mut snapshot, &metadata, &dirty);
        assert!(!snapshot.panes[0].git_dirty);
        assert!(!snapshot.panes[0].project_dirty);
    }

    #[test]
    fn failed_dirty_check_does_not_restore_stale_values() {
        let stale = vec![Pane {
            path: "/repo".to_string(),
            project_root: "/repo".to_string(),
            ..Pane::new(crate::agent::PaneId::parse("%1").unwrap())
        }];
        let metadata = cache_panes(&stale);
        let dirty = std::collections::HashMap::from([("/repo".to_string(), None)]);
        let mut snapshot = Snapshot {
            panes: metadata.clone(),
            ..Snapshot::default()
        };
        // A newer successful check has already committed dirty status.
        snapshot.panes[0].git_dirty = true;
        snapshot.panes[0].project_dirty = true;
        apply_metadata(&mut snapshot, &metadata, &dirty);
        assert!(snapshot.panes[0].git_dirty);
        assert!(snapshot.panes[0].project_dirty);
        let clean = std::collections::HashMap::from([("/repo".to_string(), Some(false))]);
        apply_metadata(&mut snapshot, &metadata, &clean);
        assert!(!snapshot.panes[0].git_dirty);
        assert!(!snapshot.panes[0].project_dirty);
    }

    #[test]
    fn externally_written_snapshot_notifies_subscribers_once() {
        let latest = Arc::new(Mutex::new(Some(Snapshot {
            generation: 1,
            revision: 1,
            ..Snapshot::default()
        })));
        let (tx, rx) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(vec![tx]));
        let external = Snapshot {
            generation: 2,
            revision: 2,
            ..Snapshot::default()
        };
        publish_snapshot(Some(&latest), Some(&subscribers), external.clone(), false);
        let Response::State {
            snapshot: Some(snapshot),
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected external snapshot");
        };
        assert_eq!(snapshot.revision, 2);
        publish_snapshot(Some(&latest), Some(&subscribers), external, false);
        assert!(
            rx.try_recv().is_err(),
            "unchanged polls must not rebroadcast"
        );
    }

    #[test]
    fn publishing_an_older_snapshot_does_not_roll_back_state() {
        let latest = Arc::new(Mutex::new(Some(Snapshot {
            version: 1,
            generation: 1,
            revision: 2,
            updated_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            ..Snapshot::default()
        })));
        let (tx, rx) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(vec![tx]));
        publish_snapshot(
            Some(&latest),
            Some(&subscribers),
            Snapshot {
                version: 1,
                generation: 1,
                revision: 1,
                updated_at: Some(chrono::Utc::now()),
                ..Snapshot::default()
            },
            true,
        );
        assert_eq!(latest.lock().unwrap().as_ref().unwrap().revision, 2);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn focusing_a_pane_marks_forced_unread_state_read() {
        let mut ui = UiPaneState {
            forced_unread: true,
            ..UiPaneState::default()
        };

        update_pane_read_state(&mut ui, "current", true);

        assert!(!ui.forced_unread);
        assert_eq!(ui.read_content_hash, None);
    }

    #[test]
    fn new_content_invalidates_a_read_hash() {
        let mut ui = UiPaneState {
            read_content_hash: Some("old".to_string()),
            ..UiPaneState::default()
        };

        update_pane_read_state(&mut ui, "new", false);

        assert_eq!(ui.read_content_hash, None);
    }
}
