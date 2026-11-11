use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::agent::adapter::apply_provider_activity;
use crate::agent::git::enrich_panes;
use crate::agent::provider::{ProcessTable, resolve};
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
    /// The window name follows tmux's active pane instead of being chosen.
    auto_name: bool,
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
    let (tmux_out, focused) = list_tmux_panes()?;
    let pt = load_process_table();
    let raw = {
        let _g = smelt_perf::perf::begin("provider.resolve_panes");
        let mut panes = parse_tmux_panes(&tmux_out);
        for pane in &mut panes {
            pane.window_focused = focused.contains(&pane.pane_id);
        }
        resolve_agent_panes(panes, &pt)
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

fn list_tmux_panes() -> Result<(String, std::collections::HashSet<PaneId>)> {
    let _g = smelt_perf::perf::begin("tmux.list_panes");
    let out = Command::new("tmux")
        .arg("list-panes")
        .arg("-a")
        .arg("-F")
        .arg("#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{window_name}\t#{window_active}#{?session_attached,1,0}#{pane_active}\t#{pane_id}\t#{pane_width}x#{pane_height}\t#{automatic-rename}")
        .args([";", "list-clients", "-F", "@client\t#{&&:#{client_control_mode},#{m:*read-only*,#{client_flags}}}\t#{pane_id}"])
        .output()
        .context("tmux list-panes")?;
    if !out.status.success() {
        return Err(anyhow!("tmux list-panes exited with {}", out.status));
    }
    let output = String::from_utf8_lossy(&out.stdout).into_owned();
    // Read-only control clients observe panes. Interactive control clients (for
    // example iTerm2) still count as user focus.
    let focused = output
        .lines()
        .filter_map(|line| line.strip_prefix("@client\t0\t").and_then(PaneId::parse))
        .collect();
    Ok((output, focused))
}

const DELETED_SUFFIX: &str = " (deleted)";

/// The directory a pane works in. The system reports `<dir> (deleted)` for a
/// process whose directory was removed, and keeps doing so after the directory
/// is created again, because the process still holds the old one. A path that
/// exists again is the directory the user means.
fn live_path(path: &str) -> String {
    match path.strip_suffix(DELETED_SUFFIX) {
        Some(live) if std::path::Path::new(live).is_dir() => live.to_string(),
        _ => path.to_string(),
    }
}

fn parse_tmux_panes(out: &str) -> Vec<RawPane> {
    let panes: Vec<RawPane> = out
        .trim()
        .lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let fields: Vec<&str> = line.splitn(9, '\t').collect();
            if fields.len() < 8 {
                return None;
            }
            let (session, window, pane) = parse_target(fields[0]);
            let (width, height) = parse_dims(fields[7]);
            Some(RawPane {
                target: fields[0].to_string(),
                cmd: fields[1].to_string(),
                path: live_path(fields[2]),
                pid: fields[3].parse().unwrap_or(0),
                provider_pid: 0,
                window_name: fields[4].to_string(),
                auto_name: fields.get(8) == Some(&"1"),
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
            r.window_name = agent_window_name(r.auto_name, &r.window_name, &r.cmd);
            Some(r)
        })
        .collect()
}

/// The label for an agent's window. tmux names an unnamed window after its
/// active pane, which is not the agent once the window is split, so unnamed
/// windows take the agent's name. A name the user chose is kept.
fn agent_window_name(auto_named: bool, tmux_name: &str, agent: &str) -> String {
    if auto_named {
        agent.to_string()
    } else {
        tmux_name.to_string()
    }
}

fn load_process_table() -> ProcessTable {
    static CACHE: OnceLock<Mutex<Option<ProcessTableCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Ok(cache) = cache.lock()
        && let Some(entry) = cache.as_ref()
        && entry.loaded_at.elapsed() < PROCESS_TABLE_TTL
    {
        smelt_perf::perf::record_value("process.snapshot_cache_hit", 1);
        return entry.table.clone();
    }

    let _g = smelt_perf::perf::begin("process.snapshot");
    let table = ProcessTable::capture();

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
        // Keep trailing cells: a TUI that fills its background by erasing would otherwise lose it
        // after the last printed character of every row.
        .arg("-N")
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

/// Opens a window after the last window of `session` and switches to it.
pub fn new_window(session: &str, dir: &str, command: Option<&str>) -> Result<()> {
    let target = format!("={session}:$");
    let mut args = vec!["new-window", "-a", "-t", &target, "-c", dir];
    args.extend(["-P", "-F", "#{pane_id}"]);
    spawn_and_switch("new-window", args, command)
}

/// Opens a window in `dir` and switches to it. It goes in `session`, else in
/// the project's session, which is created when none exists.
pub fn open_agent_window(
    session: Option<&str>,
    root: &str,
    dir: &str,
    command: Option<&str>,
) -> Result<()> {
    let session = match session {
        Some(session) => Some(session.to_string()),
        None => project_session(root)?,
    };
    match session {
        Some(session) => new_window(&session, dir, command),
        None => {
            let name = session_name(root);
            let args = vec![
                "new-session",
                "-d",
                "-s",
                &name,
                "-c",
                dir,
                "-P",
                "-F",
                "#{pane_id}",
            ];
            spawn_and_switch("new-session", args, command)
        }
    }
}

/// The session for a project: one that starts in `root`, else one named like
/// the sessionizer names the directory.
pub fn project_session(root: &str) -> Result<Option<String>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}\t#{session_path}"])
        .output()
        .context("tmux list-sessions")?;
    let sessions = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        String::new()
    };
    Ok(find_project_session(&sessions, root, &session_name(root)))
}

/// Creates a detached session whose first window, started in `dir`, is the
/// terminal window `terminal`, and returns the session name.
pub fn create_terminal_session(root: &str, dir: &str, terminal: &str) -> Result<String> {
    let name = session_name(root);
    let window = tmux_stdout(&[
        "new-session",
        "-d",
        "-s",
        &name,
        "-c",
        dir,
        "-P",
        "-F",
        "#{window_id}",
    ])?;
    run_tmux([
        "set-option",
        "-w",
        "-t",
        window.trim(),
        TERMINAL_TAG,
        terminal,
    ])?;
    Ok(name)
}

/// Name of the shell tmux starts in new panes.
pub fn default_shell_name() -> String {
    let shell = tmux_stdout(&["show-options", "-gv", "default-shell"]).unwrap_or_default();
    std::path::Path::new(shell.trim()).file_name().map_or_else(
        || "sh".to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "nu"];

/// One pane of a session, as listed for terminal lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPane {
    window: String,
    tag: String,
    pane: String,
    command: String,
    window_panes: usize,
    path: String,
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalPick {
    /// A window already tagged with the terminal's name.
    Tagged(String),
    /// An idle single-shell window, to be tagged and reused.
    Adopt {
        window: String,
        pane: String,
    },
    Missing,
}

/// The window tagged `name`, else an untagged idle shell sitting in `dir`.
fn pick_terminal(panes: &[SessionPane], name: &str, dir: &str) -> TerminalPick {
    if let Some(pane) = panes.iter().find(|pane| pane.tag == name) {
        return TerminalPick::Tagged(pane.pane.clone());
    }
    panes
        .iter()
        .find(|pane| {
            pane.tag.is_empty()
                && pane.window_panes == 1
                && pane.path == dir
                && SHELLS.contains(&pane.command.as_str())
        })
        .map_or(TerminalPick::Missing, |pane| TerminalPick::Adopt {
            window: pane.window.clone(),
            pane: pane.pane.clone(),
        })
}

fn parse_session_panes(out: &str) -> Vec<SessionPane> {
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [window, tag, pane, command, window_panes, path] = f.as_slice() else {
                return None;
            };
            Some(SessionPane {
                window: (*window).to_string(),
                tag: (*tag).to_string(),
                pane: (*pane).to_string(),
                command: (*command).to_string(),
                window_panes: window_panes.parse().ok()?,
                path: live_path(path),
            })
        })
        .collect()
}

const TERMINAL_TAG: &str = "@agent-mux-terminal";

/// The pane of the session's terminal window named `name`. An untagged idle
/// shell window in `dir` is adopted first; otherwise a window is added to the
/// session. The window is tagged so later lookups find it even while a
/// command renames it.
pub fn terminal_pane(session: &str, name: &str, dir: &str) -> Result<(PaneId, String)> {
    let target = format!("={session}:");
    let out = tmux_stdout(&[
        "list-panes",
        "-s",
        "-t",
        &target,
        "-F",
        &format!(
            "#{{window_id}}\t#{{{TERMINAL_TAG}}}\t#{{pane_id}}\t#{{pane_current_command}}\t#{{window_panes}}\t#{{pane_current_path}}"
        ),
    ])?;
    let panes = parse_session_panes(&out);
    let (pane, command) = match pick_terminal(&panes, name, dir) {
        TerminalPick::Tagged(pane) => {
            let command = panes
                .iter()
                .find(|p| p.pane == pane)
                .map(|p| p.command.clone())
                .unwrap_or_default();
            (pane, command)
        }
        TerminalPick::Adopt { window, pane } => {
            run_tmux(["set-option", "-w", "-t", &window, TERMINAL_TAG, name])?;
            let command = panes
                .iter()
                .find(|p| p.pane == pane)
                .map(|p| p.command.clone())
                .unwrap_or_default();
            (pane, command)
        }
        TerminalPick::Missing => {
            // Before the first window, so the terminal keeps one fixed position
            // and agent windows, added at the end, never interleave with it.
            let created = tmux_stdout(&[
                "new-window",
                "-b",
                "-t",
                &format!("={session}:^"),
                "-c",
                dir,
                "-d",
                "-P",
                "-F",
                "#{window_id}\t#{pane_id}",
            ])?;
            let (window, pane) = created
                .trim()
                .split_once('\t')
                .ok_or_else(|| anyhow!("tmux new-window returned no window"))?;
            run_tmux(["set-option", "-w", "-t", window, TERMINAL_TAG, name])?;
            (pane.to_string(), default_shell_name())
        }
    };
    let pane = PaneId::parse(&pane).context("tmux returned invalid pane ID")?;
    Ok((pane, command))
}

/// Types `text` into an idle shell pane, or recalls the previous command when
/// `text` is `last`. Refuses while a program other than a shell is running.
pub fn send_to_terminal(pane: &PaneId, command: &str, text: &str) -> Result<()> {
    if !SHELLS.contains(&command) {
        return Err(anyhow!("terminal is busy running {command}"));
    }
    let id = pane.as_str();
    // Ctrl-C drops any half-typed line so recalled or typed text starts clean.
    run_tmux(["send-keys", "-t", id, "C-c"])?;
    if text == "last" {
        run_tmux(["send-keys", "-t", id, "Up", "Enter"])
    } else {
        run_tmux(["send-keys", "-t", id, "-l", "--", text])?;
        run_tmux(["send-keys", "-t", id, "Enter"])
    }
}

fn tmux_stdout(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .context("run tmux")?;
    if !output.status.success() {
        return Err(anyhow!(
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn find_project_session(sessions: &str, root: &str, name: &str) -> Option<String> {
    let sessions: Vec<(&str, &str)> = sessions
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    sessions
        .iter()
        .find(|(_, path)| *path == root)
        .or_else(|| sessions.iter().find(|(session, _)| *session == name))
        .map(|(session, _)| (*session).to_string())
}

/// The session name the tmux sessionizer uses: the directory name with `.`
/// replaced, since tmux reserves `.` and `:` in targets.
fn session_name(root: &str) -> String {
    let base = std::path::Path::new(root).file_name().map_or_else(
        || root.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    base.replace(['.', ':'], "_")
}

fn spawn_and_switch<'a>(
    what: &str,
    mut args: Vec<&'a str>,
    command: Option<&'a str>,
) -> Result<()> {
    if let Some(command) = command.filter(|command| !command.trim().is_empty()) {
        args.push(command);
    }
    let output = Command::new("tmux")
        .args(&args)
        .output()
        .with_context(|| format!("tmux {what}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "tmux {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let pane_id = PaneId::parse(String::from_utf8_lossy(&output.stdout).trim())
        .with_context(|| format!("tmux {what} returned invalid pane ID"))?;
    switch_to_pane(&pane_id)
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
    use super::{
        SessionPane, TerminalPick, agent_window_name, find_project_session, live_path,
        parse_session_panes, pick_terminal, session_name, short_hash,
    };

    #[test]
    fn project_session_prefers_matching_path_then_sessionizer_name() {
        let sessions = "dotfiles\t/home/me\n_dotfiles\t/home/me/.dotfiles\nrepo\t/elsewhere\n";
        assert_eq!(
            find_project_session(sessions, "/home/me/.dotfiles", "_dotfiles").as_deref(),
            Some("_dotfiles")
        );
        assert_eq!(
            find_project_session(sessions, "/home/me/dev/repo", "repo").as_deref(),
            Some("repo")
        );
        assert_eq!(
            find_project_session(sessions, "/home/me/dev/new", "new"),
            None
        );
    }

    const DIR: &str = "/repo";

    fn session_pane(window: &str, tag: &str, command: &str, window_panes: usize) -> SessionPane {
        SessionPane {
            window: window.to_string(),
            tag: tag.to_string(),
            pane: format!("%{}", &window[1..]),
            command: command.to_string(),
            window_panes,
            path: DIR.to_string(),
        }
    }

    #[test]
    fn terminal_lookup_prefers_tag_then_adopts_an_idle_shell() {
        let tagged = [
            session_pane("@1", "", "nvim", 1),
            session_pane("@2", "term", "cargo", 1),
            session_pane("@3", "", "zsh", 1),
        ];
        assert_eq!(
            pick_terminal(&tagged, "term", DIR),
            TerminalPick::Tagged("%2".to_string())
        );
        let untagged = [
            session_pane("@1", "", "nvim", 1),
            session_pane("@2", "", "smelt", 1),
            session_pane("@3", "", "zsh", 1),
        ];
        assert_eq!(
            pick_terminal(&untagged, "term", DIR),
            TerminalPick::Adopt {
                window: "@3".to_string(),
                pane: "%3".to_string()
            }
        );
        let other = [session_pane("@3", "build", "zsh", 1)];
        assert_eq!(pick_terminal(&other, "term", DIR), TerminalPick::Missing);
    }

    #[test]
    fn terminal_lookup_skips_split_and_busy_windows() {
        let panes = [
            session_pane("@1", "", "zsh", 2),
            session_pane("@2", "", "cargo", 1),
        ];
        assert_eq!(pick_terminal(&panes, "term", DIR), TerminalPick::Missing);
    }

    #[test]
    fn terminal_lookup_adopts_only_a_shell_in_the_wanted_directory() {
        let panes = [session_pane("@1", "", "zsh", 1)];
        assert_eq!(
            pick_terminal(&panes, "term", "/repo-feature"),
            TerminalPick::Missing
        );
    }

    #[test]
    fn session_pane_listing_parses_tab_separated_fields() {
        let panes = parse_session_panes("@1\tterm\t%4\tzsh\t1\t/repo\nbroken line\n");
        assert_eq!(
            panes,
            [session_pane("@1", "term", "zsh", 1)].map(|mut p| {
                p.pane = "%4".to_string();
                p
            })
        );
    }

    #[test]
    fn unnamed_windows_are_labelled_by_their_agent_not_the_active_pane() {
        assert_eq!(agent_window_name(true, "zsh", "smelt"), "smelt");
        assert_eq!(agent_window_name(false, "backend", "smelt"), "backend");
    }

    #[test]
    fn recreated_directories_lose_the_deleted_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = temporary.path().join("worktree");
        let reported = format!("{} (deleted)", dir.display());
        // Still gone: the marker stays.
        assert_eq!(live_path(&reported), reported);
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(live_path(&reported), dir.to_str().unwrap());
        assert_eq!(live_path(dir.to_str().unwrap()), dir.to_str().unwrap());
    }

    #[test]
    fn session_names_match_the_sessionizer() {
        assert_eq!(session_name("/home/me/.dotfiles"), "_dotfiles");
        assert_eq!(session_name("/home/me/dev/agent-mux"), "agent-mux");
        assert_eq!(session_name("/srv/app.v2:x"), "app_v2_x");
    }

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
