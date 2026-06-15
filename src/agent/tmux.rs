use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use crate::agent::Pane;
use crate::agent::git::enrich_panes;
use crate::agent::provider::{ProcessTable, parse_process_table, resolve};

#[derive(Debug, Clone)]
struct RawPane {
    pane_id: String,
    target: String,
    session: String,
    window: String,
    window_name: String,
    pane: String,
    path: String,
    cmd: String,
    pid: i32,
    window_focused: bool,
}

pub fn list_panes() -> Result<Vec<Pane>> {
    let mut panes = list_panes_fast()?;
    enrich_panes(&mut panes);
    Ok(panes)
}

pub fn list_panes_fast() -> Result<Vec<Pane>> {
    let mut panes = fetch_panes()?;
    capture_content(&mut panes);
    Ok(panes)
}

fn fetch_panes() -> Result<Vec<Pane>> {
    let tmux_out = list_tmux_panes()?;
    let pt = load_process_table();
    let raw = resolve_agent_panes(parse_tmux_panes(&tmux_out), &pt);
    Ok(raw
        .into_iter()
        .enumerate()
        .map(|(order, r)| Pane {
            pane_id: r.pane_id,
            target: r.target,
            session: r.session,
            window: r.window,
            window_name: r.window_name,
            pane: r.pane,
            path: r.path,
            pid: r.pid,
            window_active: r.window_focused,
            order,
            provider: r.cmd,
            ..Pane::default()
        })
        .collect())
}

fn list_tmux_panes() -> Result<String> {
    let out = Command::new("tmux")
        .arg("list-panes")
        .arg("-a")
        .arg("-F")
        .arg("#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{window_name}\t#{window_active}#{?session_attached,1,0}#{pane_active}\t#{pane_id}")
        .output()
        .context("tmux list-panes")?;
    if !out.status.success() {
        return Err(anyhow!("tmux list-panes exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_tmux_panes(out: &str) -> Vec<RawPane> {
    out.trim()
        .lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let fields: Vec<&str> = line.splitn(7, '\t').collect();
            if fields.len() < 7 {
                return None;
            }
            let (session, window, pane) = parse_target(fields[0]);
            Some(RawPane {
                target: fields[0].to_string(),
                cmd: fields[1].to_string(),
                path: fields[2].to_string(),
                pid: fields[3].parse().unwrap_or(0),
                window_name: fields[4].to_string(),
                window_focused: fields[5] == "111",
                pane_id: fields[6].to_string(),
                session,
                window,
                pane,
            })
        })
        .collect()
}

fn resolve_agent_panes(raw: Vec<RawPane>, pt: &ProcessTable) -> Vec<RawPane> {
    raw.into_iter()
        .filter_map(|mut r| {
            let cmd = resolve(&r.cmd, r.pid, pt);
            if cmd.is_empty() {
                None
            } else {
                r.cmd = cmd;
                Some(r)
            }
        })
        .collect()
}

fn load_process_table() -> ProcessTable {
    Command::new("ps")
        .arg("-eo")
        .arg("pid=,ppid=,command=")
        .output()
        .map(|out| parse_process_table(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

fn capture_content(panes: &mut [Pane]) {
    thread::scope(|scope| {
        for pane in panes {
            scope.spawn(move || {
                let (hash, moving, attention) = capture_pane_content(&pane.target);
                pane.content_hash = hash;
                pane.content_moving = moving;
                pane.heuristic_attention = attention;
            });
        }
    });
}

fn capture_pane_content(target: &str) -> (String, bool, bool) {
    let Ok(first) = Command::new("tmux")
        .arg("capture-pane")
        .arg("-t")
        .arg(target)
        .arg("-p")
        .arg("-S")
        .arg("-10")
        .output()
    else {
        return (String::new(), false, false);
    };
    let first = trim_trailing_newlines(first.stdout);
    let first_hash = short_hash(&first);
    let first_attention = attention_re().is_match(&String::from_utf8_lossy(&first));

    thread::sleep(Duration::from_millis(100));

    let Ok(second) = Command::new("tmux")
        .arg("capture-pane")
        .arg("-t")
        .arg(target)
        .arg("-p")
        .arg("-S")
        .arg("-10")
        .output()
    else {
        return (first_hash, false, first_attention);
    };
    let second = trim_trailing_newlines(second.stdout);
    let second_hash = short_hash(&second);
    let second_attention = attention_re().is_match(&String::from_utf8_lossy(&second));
    (
        second_hash,
        first != second,
        first_attention || second_attention,
    )
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

fn attention_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"Do you want to proceed\?|Do you want to allow|Allow once|press Enter to approve|Enter to select|Type something|Esc to cancel|I'll wait for your|waiting for your response|Let me know when|Please let me know|What would you like|How would you like|Should I proceed|Would you like me to|please provide|please specify|I need more information|Could you clarify|awaiting your|ready when you are|let me know if you'd like|Feel free to ask|Is there anything else|What else can I help|Want me to|Shall I|Do you want me to|Ready to proceed").expect("valid attention regex"))
}

pub fn capture_pane(target: &str, lines: usize) -> Result<String> {
    let out = Command::new("tmux")
        .arg("capture-pane")
        .arg("-t")
        .arg(target)
        .arg("-e")
        .arg("-p")
        .arg("-S")
        .arg(format!("-{lines}"))
        .output()
        .with_context(|| format!("capture-pane {target}"))?;
    if !out.status.success() {
        return Err(anyhow!("capture-pane {target} exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn switch_to_pane(target: &str) -> Result<()> {
    let (session, window, _) = parse_target(target);
    let session_window = format!("{session}:{window}");
    run_tmux(["switch-client", "-t", &session_window])?;
    run_tmux(["select-pane", "-t", target])
}

pub fn kill_pane(target: &str) -> Result<()> {
    let (session, window, _) = parse_target(target);
    let session_window = format!("{session}:{window}");
    let out = Command::new("tmux")
        .arg("list-panes")
        .arg("-t")
        .arg(&session_window)
        .output()
        .context("list-panes")?;
    let pane_count = String::from_utf8_lossy(&out.stdout).trim().lines().count();
    if pane_count <= 1 {
        run_tmux(["kill-window", "-t", &session_window])
    } else {
        run_tmux(["kill-pane", "-t", target])
    }
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
    let exe = std::env::current_exe().context("current executable")?;
    Command::new(exe)
        .arg("watch")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start watch")?;
    Ok(())
}

pub fn restart_watch() -> Result<()> {
    if let Ok(data) = std::fs::read_to_string(crate::agent::watch::lock_path())
        && let Ok(pid) = data.trim().parse::<i32>()
        && pid > 0
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        thread::sleep(Duration::from_millis(200));
    }
    start_watch()
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
