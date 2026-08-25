use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::agent::adapter::apply_provider_activity;
use crate::agent::git::enrich_panes;
use crate::agent::provider::{ProcessTable, parse_process_table, resolve};
use crate::agent::{Pane, PaneId};

const PROCESS_TABLE_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct ProcessTableCache {
    loaded_at: Instant,
    table: ProcessTable,
}

#[derive(Debug, Clone)]
struct RawPane {
    pane_id: PaneId,
    target: String,
    session: String,
    window: String,
    window_name: String,
    pane: String,
    path: String,
    cmd: String,
    pid: i32,
    provider_pid: i32,
    window_focused: bool,
    width: u16,
    height: u16,
}

pub fn list_panes() -> Result<Vec<Pane>> {
    let _g = smelt_perf::perf::begin("agent.list_panes");
    let mut panes = list_panes_fast()?;
    enrich_panes(&mut panes);
    Ok(panes)
}

pub fn list_panes_fast() -> Result<Vec<Pane>> {
    let _g = smelt_perf::perf::begin("agent.list_panes_fast");
    let mut panes = fetch_panes()?;
    capture_content(&mut panes);
    apply_provider_activity(&mut panes);
    Ok(panes)
}

fn fetch_panes() -> Result<Vec<Pane>> {
    let _g = smelt_perf::perf::begin("tmux.fetch_panes");
    let tmux_out = list_tmux_panes()?;
    let pt = load_process_table();
    let raw = {
        let _g = smelt_perf::perf::begin("provider.resolve_panes");
        resolve_agent_panes(parse_tmux_panes(&tmux_out), &pt)
    };
    smelt_perf::perf::record_value("tmux.agent_panes", raw.len() as u64);
    Ok(raw
        .into_iter()
        .enumerate()
        .map(|(order, r)| Pane {
            target: r.target,
            session: r.session,
            window: r.window,
            window_name: r.window_name,
            pane: r.pane,
            path: r.path,
            pid: r.pid,
            window_active: r.window_focused,
            width: r.width,
            height: r.height,
            order,
            provider: r.cmd,
            provider_pid: r.provider_pid,
            ..Pane::new(r.pane_id)
        })
        .collect())
}

fn list_tmux_panes() -> Result<String> {
    let _g = smelt_perf::perf::begin("tmux.list_panes");
    let out = Command::new("tmux")
        .arg("list-panes")
        .arg("-a")
        .arg("-F")
        .arg("#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{window_name}\t#{window_active}#{?session_attached,1,0}#{pane_active}\t#{pane_id}\t#{pane_width}x#{pane_height}")
        .output()
        .context("tmux list-panes")?;
    if !out.status.success() {
        return Err(anyhow!("tmux list-panes exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_tmux_panes(out: &str) -> Vec<RawPane> {
    let panes: Vec<RawPane> = out
        .trim()
        .lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let fields: Vec<&str> = line.splitn(8, '\t').collect();
            if fields.len() < 8 {
                return None;
            }
            let (session, window, pane) = parse_target(fields[0]);
            let (width, height) = parse_dims(fields[7]);
            Some(RawPane {
                target: fields[0].to_string(),
                cmd: fields[1].to_string(),
                path: fields[2].to_string(),
                pid: fields[3].parse().unwrap_or(0),
                provider_pid: 0,
                window_name: fields[4].to_string(),
                window_focused: fields[5] == "111",
                pane_id: PaneId::parse(fields[6])?,
                width,
                height,
                session,
                window,
                pane,
            })
        })
        .collect();
    smelt_perf::perf::record_value("tmux.raw_panes", panes.len() as u64);
    panes
}
fn resolve_agent_panes(raw: Vec<RawPane>, pt: &ProcessTable) -> Vec<RawPane> {
    raw.into_iter()
        .filter_map(|mut r| {
            let matched = resolve(&r.cmd, r.pid, pt)?;
            r.cmd = matched.name;
            r.provider_pid = matched.pid;
            Some(r)
        })
        .collect()
}

fn load_process_table() -> ProcessTable {
    static CACHE: OnceLock<Mutex<Option<ProcessTableCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Ok(cache) = cache.lock()
        && let Some(entry) = cache.as_ref()
        && entry.loaded_at.elapsed() < PROCESS_TABLE_TTL
    {
        smelt_perf::perf::record_value("process.ps_cache_hit", 1);
        return entry.table.clone();
    }

    let _g = smelt_perf::perf::begin("process.ps");
    let table = Command::new("ps")
        .arg("-eo")
        .arg("pid=,ppid=,command=")
        .output()
        .map(|out| parse_process_table(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default();

    if let Ok(mut cache) = cache.lock() {
        *cache = Some(ProcessTableCache {
            loaded_at: Instant::now(),
            table: table.clone(),
        });
    }
    table
}

fn capture_content(panes: &mut [Pane]) {
    let _g = smelt_perf::perf::begin("tmux.capture_content_all");
    thread::scope(|scope| {
        for pane in panes {
            scope.spawn(move || {
                pane.content_hash = capture_pane_content(&pane.pane_id);
            });
        }
    });
}

fn capture_pane_content(pane_id: &PaneId) -> String {
    let _g = smelt_perf::perf::begin("tmux.capture_pane_content");
    let Ok(out) = Command::new("tmux")
        .arg("capture-pane")
        .arg("-t")
        .arg(pane_id.as_str())
        .arg("-p")
        .arg("-e")
        .arg("-S")
        .arg("-10")
        .output()
    else {
        return String::new();
    };
    let content = trim_trailing_newlines(out.stdout);
    smelt_perf::perf::record_value("tmux.capture_bytes", content.len() as u64);
    short_hash(&content)
}

fn trim_trailing_newlines(mut data: Vec<u8>) -> Vec<u8> {
    while data.last().is_some_and(|b| *b == b'\n') {
        data.pop();
    }
    data
}

fn short_hash(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn capture_pane(pane_id: &PaneId, lines: usize) -> Result<String> {
    let _g = smelt_perf::perf::begin("tmux.capture_preview");
    let out = Command::new("tmux")
        .arg("capture-pane")
        .arg("-t")
        .arg(pane_id.as_str())
        .arg("-e")
        .arg("-p")
        .arg("-S")
        .arg(format!("-{lines}"))
        .output()
        .with_context(|| format!("capture-pane {pane_id}"))?;
    if !out.status.success() {
        return Err(anyhow!("capture-pane {pane_id} exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn switch_to_pane(pane_id: &PaneId) -> Result<()> {
    run_tmux(["switch-client", "-t", pane_id.as_str()])
}

pub fn kill_pane(pane_id: &PaneId) -> Result<()> {
    run_tmux(["kill-pane", "-t", pane_id.as_str()])
}

fn run_tmux<const N: usize>(args: [&str; N]) -> Result<()> {
    let status = Command::new("tmux").args(args).status().context("tmux")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("tmux exited with {status}"))
    }
}

pub fn start_watch() -> Result<()> {
    if crate::agent::watch::is_running() {
        for _ in 0..5 {
            if crate::agent::ipc::get_state().is_ok_and(|(snapshot, _)| snapshot.is_some()) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop_watch_process();
        thread::sleep(Duration::from_millis(200));
    }

    std::fs::create_dir_all(crate::agent::persist::state_dir()).context("create state dir")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::agent::watch::log_path())
        .context("open watch log")?;
    let stderr = log.try_clone().context("clone watch log")?;

    let exe = std::env::current_exe().context("current executable")?;
    Command::new(exe)
        .arg("watch")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("start watch")?;

    for _ in 0..10 {
        if crate::agent::ipc::get_state().is_ok_and(|(snapshot, _)| snapshot.is_some()) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Ok(())
}

pub fn restart_watch() -> Result<()> {
    stop_watch_process();
    thread::sleep(Duration::from_millis(200));
    start_watch()
}

fn stop_watch_process() {
    if let Ok(data) = std::fs::read_to_string(crate::agent::watch::lock_path())
        && let Ok(pid) = data.trim().parse::<i32>()
        && pid > 0
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
}

fn parse_dims(s: &str) -> (u16, u16) {
    let mut parts = s.trim().splitn(2, 'x');
    let width = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let height = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (width, height)
}

pub fn parse_target(s: &str) -> (String, String, String) {
    let Some(colon_idx) = s.rfind(':') else {
        return (s.to_string(), String::new(), String::new());
    };
    let session = s[..colon_idx].to_string();
    let rest = &s[colon_idx + 1..];
    let Some(dot_idx) = rest.rfind('.') else {
        return (session, rest.to_string(), String::new());
    };
    (
        session,
        rest[..dot_idx].to_string(),
        rest[dot_idx + 1..].to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::short_hash;

    #[test]
    fn ansi_only_change_produces_activity_hash_change() {
        let red = b"\x1b[31mWorking\x1b[0m";
        let blue = b"\x1b[34mWorking\x1b[0m";

        assert_ne!(short_hash(red), short_hash(blue));
    }

    #[test]
    fn stable_ansi_content_produces_stable_hash() {
        let content = b"\x1b[32mCompleted\x1b[0m";

        assert_eq!(short_hash(content), short_hash(content));
    }
}
