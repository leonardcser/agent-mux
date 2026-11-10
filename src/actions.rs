//! Key actions that run in a session's or project's directory. The sidebar
//! ships a default set; the config adds to it and can override any key.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agent::git::Worktree;
use crate::config::{ActionConfig, ActionMode, Config, Pick};
use crate::keys;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    key: char,
    ctrl: bool,
    pub cmd: String,
    pub mode: ActionMode,
    pub description: String,
    pub pick: Option<Pick>,
    tag: String,
    send: Option<String>,
    focus: bool,
}

impl Action {
    /// Whether the action moves tmux to another window, so the list should close.
    pub fn leaves(&self) -> bool {
        match self.mode {
            ActionMode::Window | ActionMode::Agent => true,
            ActionMode::Terminal => self.focus,
            ActionMode::Popup | ActionMode::Run => false,
        }
    }

    /// Whether the action works on the project's main checkout rather than on
    /// the selected directory: agents and terminals, unless a worktree is picked.
    pub fn is_project_level(&self) -> bool {
        self.pick.is_none() && matches!(self.mode, ActionMode::Agent | ActionMode::Terminal)
    }

    pub fn key_label(&self) -> String {
        if self.ctrl {
            format!("C-{}", self.key)
        } else {
            self.key.to_string()
        }
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        matches!(key.code, KeyCode::Char(ch) if ch == self.key) && ctrl == self.ctrl && !alt
    }
}

/// Where an action runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// tmux session of the selected session row; none for a project row.
    pub session: Option<String>,
    pub path: String,
    pub root: String,
    pub branch: String,
}

impl Target {
    /// The same target, moved to another checkout of the project.
    pub fn in_worktree(&self, worktree: &Worktree) -> Target {
        Target {
            path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            ..self.clone()
        }
    }
}

pub fn find<'a>(actions: &'a [Action], key: &KeyEvent) -> Option<&'a Action> {
    actions.iter().find(|action| action.matches(key))
}

/// The keys every sidebar has before the config adds its own.
fn defaults(agent_cmd: Option<&str>) -> Vec<(&'static str, ActionConfig)> {
    let agent = ActionConfig {
        mode: ActionMode::Agent,
        cmd: agent_cmd.unwrap_or_default().to_string(),
        ..ActionConfig::default()
    };
    let terminal = ActionConfig {
        mode: ActionMode::Terminal,
        ..ActionConfig::default()
    };
    let pick = Some(Pick::Worktree);
    vec![
        ("a", agent.clone()),
        ("A", ActionConfig { pick, ..agent }),
        ("t", terminal.clone()),
        (
            "T",
            ActionConfig {
                pick,
                ..terminal.clone()
            },
        ),
        (
            "r",
            ActionConfig {
                send: Some("last".to_string()),
                focus: Some(false),
                ..terminal
            },
        ),
    ]
}

/// Binds the default actions and the configured ones. A configured action
/// replaces the default on the same key.
pub fn bind(config: &Config) -> Result<Vec<Action>> {
    let defaults = defaults(config.new_window_command.as_deref());
    let entries = defaults
        .iter()
        .filter(|(spec, _)| !config.actions.contains_key(*spec))
        .map(|(spec, action)| (*spec, action))
        .chain(
            config
                .actions
                .iter()
                .map(|(spec, action)| (spec.as_str(), action)),
        );
    let mut actions: Vec<Action> = Vec::new();
    for (spec, config) in entries {
        let action = bind_one(spec, config).with_context(|| format!("actions.{spec}"))?;
        if actions
            .iter()
            .any(|a| a.key == action.key && a.ctrl == action.ctrl)
        {
            bail!("actions.{spec}: key is bound twice");
        }
        actions.push(action);
    }
    Ok(actions)
}

fn bind_one(spec: &str, config: &ActionConfig) -> Result<Action> {
    let (key, ctrl) = parse_key(spec)?;
    let terminal = config.mode == ActionMode::Terminal;
    let terminal_options = config.tag.is_some() || config.send.is_some() || config.focus.is_some();
    match config.mode {
        ActionMode::Terminal => {
            if !config.cmd.trim().is_empty() {
                bail!("cmd is not used by terminal actions; use send");
            }
        }
        ActionMode::Agent => {}
        ActionMode::Popup | ActionMode::Window | ActionMode::Run => {
            if config.cmd.trim().is_empty() {
                bail!("cmd is empty");
            }
        }
    }
    if !terminal && terminal_options {
        bail!("tag, send and focus only apply to mode = \"terminal\"");
    }
    let focus = config.focus.unwrap_or(true);
    if terminal && !focus && config.send.is_none() {
        bail!("focus = false needs send, or the action does nothing");
    }
    let tag = config.tag.clone().unwrap_or_else(|| "terminal".to_string());
    if tag.trim().is_empty() {
        bail!("tag is empty");
    }
    let mut description = match (config.mode, config.send.as_deref()) {
        (ActionMode::Terminal, Some("last")) => "rerun last command".to_string(),
        (ActionMode::Terminal, Some(send)) => format!("run {send}"),
        (ActionMode::Terminal, None) => "go to terminal".to_string(),
        (ActionMode::Agent, _) => "new agent window".to_string(),
        _ => config.cmd.clone(),
    };
    if let Some(custom) = &config.description {
        description = custom.clone();
    } else if config.pick == Some(Pick::Worktree) {
        description.push_str(" in a worktree");
    }
    Ok(Action {
        key,
        ctrl,
        cmd: config.cmd.clone(),
        mode: config.mode,
        description,
        pick: config.pick,
        tag,
        send: config.send.clone(),
        focus,
    })
}

fn parse_key(spec: &str) -> Result<(char, bool)> {
    let (name, ctrl) = match spec
        .strip_prefix("ctrl-")
        .or_else(|| spec.strip_prefix("C-"))
    {
        Some(name) => (name, true),
        None => (spec, false),
    };
    let mut chars = name.chars();
    let (Some(key), None) = (chars.next(), chars.next()) else {
        bail!("key must be one character or ctrl-<character>");
    };
    if keys::is_reserved(key, ctrl) || key.is_control() {
        bail!("key is used by the sidebar");
    }
    Ok((key, ctrl))
}

/// Replaces `{path}`, `{root}` and `{branch}` with shell-quoted values.
pub fn expand(template: &str, target: &Target) -> String {
    template
        .replace("{path}", &shell_quote(&target.path))
        .replace("{root}", &shell_quote(&target.root))
        .replace("{branch}", &shell_quote(&target.branch))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn run(action: &Action, target: &Target) -> Result<()> {
    let command = expand(&action.cmd, target);
    match action.mode {
        ActionMode::Terminal => run_terminal(action, target),
        ActionMode::Agent => crate::agent::tmux::open_agent_window(
            target.session.as_deref(),
            &target.root,
            &target.path,
            Some(&command),
        ),
        ActionMode::Popup => tmux(&[
            "display-popup",
            "-E",
            "-w",
            "90%",
            "-h",
            "90%",
            "-d",
            &target.path,
            &command,
        ]),
        ActionMode::Window => tmux(&["new-window", "-a", "-c", &target.path, &command]),
        ActionMode::Run => {
            let output = Command::new("sh")
                .args(["-c", &command])
                .current_dir(&target.path)
                .stdin(Stdio::null())
                .output()
                .with_context(|| format!("run {}", action.cmd))?;
            if output.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(anyhow!("{}: {}", action.cmd, stderr.trim()))
            }
        }
    }
}

/// The tag of the terminal window for the target's checkout. The project root
/// uses the action's tag; each linked worktree gets a window of its own.
fn terminal_tag(tag: &str, target: &Target) -> String {
    if target.path == target.root {
        tag.to_string()
    } else {
        format!("{tag}:{}", target.path)
    }
}

fn run_terminal(action: &Action, target: &Target) -> Result<()> {
    use crate::agent::tmux::{
        create_terminal_session, default_shell_name, project_session, send_to_terminal,
        switch_to_pane, terminal_pane,
    };
    let tag = terminal_tag(&action.tag, target);
    let mut created = false;
    let session = match &target.session {
        Some(session) => session.clone(),
        None => match project_session(&target.root)? {
            Some(session) => session,
            None => {
                created = true;
                create_terminal_session(&target.root, &target.path, &tag)?
            }
        },
    };
    let (pane, mut command) = terminal_pane(&session, &tag, &target.path)?;
    if created {
        // A new shell may still be starting, which tmux reports as another command.
        command = default_shell_name();
    }
    if let Some(send) = &action.send {
        send_to_terminal(&pane, &command, &expand(send, target))?;
    }
    if action.focus {
        switch_to_pane(&pane)?;
    }
    Ok(())
}

fn tmux(args: &[&str]) -> Result<()> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .context("run tmux")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(actions: Vec<(&str, ActionConfig)>) -> Config {
        Config {
            actions: actions
                .into_iter()
                .map(|(spec, action)| (spec.to_string(), action))
                .collect::<BTreeMap<_, _>>(),
            ..Config::default()
        }
    }

    fn command(cmd: &str) -> ActionConfig {
        ActionConfig {
            cmd: cmd.to_string(),
            ..ActionConfig::default()
        }
    }

    fn bound(config: &Config, key: char) -> Action {
        let mut actions = bind(config).unwrap();
        let at = actions
            .iter()
            .position(|action| action.key == key && !action.ctrl)
            .unwrap();
        actions.remove(at)
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn target(path: &str, root: &str) -> Target {
        Target {
            session: None,
            path: path.to_string(),
            root: root.to_string(),
            branch: "feat/x".to_string(),
        }
    }

    #[test]
    fn keys_match_plain_shifted_and_control_presses() {
        let actions = bind(&config(vec![
            ("x", command("a")),
            ("P", command("b")),
            ("ctrl-x", command("c")),
        ]))
        .unwrap();
        let find_cmd = |key| find(&actions, &key).map(|action| action.cmd.as_str());
        assert_eq!(
            find_cmd(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some("a")
        );
        assert_eq!(
            find_cmd(press(KeyCode::Char('P'), KeyModifiers::SHIFT)),
            Some("b")
        );
        assert_eq!(
            find_cmd(press(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Some("c")
        );
        assert_eq!(
            find_cmd(press(KeyCode::Char('y'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(find_cmd(press(KeyCode::Char('x'), KeyModifiers::ALT)), None);
    }

    #[test]
    fn invalid_bindings_are_rejected() {
        for entries in [
            vec![("j", command("cmd"))],
            vec![("R", command("cmd"))],
            vec![("ctrl-n", command("cmd"))],
            vec![("xy", command("cmd"))],
            vec![("", command("cmd"))],
            vec![("x", command("  "))],
            vec![
                ("x", command("a")),
                ("C-x", command("b")),
                ("ctrl-x", command("c")),
            ],
        ] {
            let result = bind(&config(entries.clone()));
            assert!(result.is_err(), "{entries:?}");
        }
    }

    #[test]
    fn default_keys_open_agents_and_terminals() {
        let base = Config {
            new_window_command: Some("smelt".to_string()),
            ..Config::default()
        };
        let agent = bound(&base, 'a');
        assert_eq!(
            (agent.mode, agent.cmd.as_str()),
            (ActionMode::Agent, "smelt")
        );
        assert_eq!(agent.pick, None);
        assert!(agent.leaves());
        let pick_agent = bound(&base, 'A');
        assert_eq!(pick_agent.pick, Some(Pick::Worktree));
        assert_eq!(pick_agent.description, "new agent window in a worktree");
        assert_eq!(bound(&base, 'T').pick, Some(Pick::Worktree));
        let rerun = bound(&base, 'r');
        assert_eq!(rerun.description, "rerun last command");
        assert!(!rerun.leaves());
        assert!(bound(&base, 't').leaves());
        for (key, project_level) in [('a', true), ('A', false), ('t', true), ('T', false)] {
            assert_eq!(bound(&base, key).is_project_level(), project_level, "{key}");
        }
        let custom = config(vec![("x", command("ls"))]);
        assert!(!bound(&custom, 'x').is_project_level());
    }

    #[test]
    fn configured_actions_replace_default_keys() {
        let rebound = config(vec![("a", command("lazygit"))]);
        let action = bound(&rebound, 'a');
        assert_eq!(action.mode, ActionMode::Popup);
        assert_eq!(action.cmd, "lazygit");
        assert_eq!(
            bind(&rebound)
                .unwrap()
                .iter()
                .filter(|action| action.key == 'a')
                .count(),
            1
        );
    }

    fn terminal(send: Option<&str>, focus: Option<bool>) -> Config {
        config(vec![(
            "t",
            ActionConfig {
                mode: ActionMode::Terminal,
                send: send.map(str::to_string),
                focus,
                ..ActionConfig::default()
            },
        )])
    }

    #[test]
    fn terminal_actions_default_to_focusing_the_term_window() {
        let action = bound(&terminal(None, None), 't');
        assert!(action.leaves());
        assert_eq!(action.description, "go to terminal");
        let rerun = bound(&terminal(Some("last"), Some(false)), 't');
        assert!(!rerun.leaves());
        assert_eq!(rerun.description, "rerun last command");
        let typed = bound(&terminal(Some("cargo build"), None), 't');
        assert_eq!(typed.description, "run cargo build");
    }

    #[test]
    fn option_misuse_is_rejected() {
        assert!(bind(&terminal(None, Some(false))).is_err());
        let with_cmd = ActionConfig {
            mode: ActionMode::Terminal,
            cmd: "ls".to_string(),
            ..ActionConfig::default()
        };
        assert!(bind(&config(vec![("x", with_cmd)])).is_err());
        let mut popup = command("ls");
        popup.send = Some("last".to_string());
        assert!(bind(&config(vec![("x", popup)])).is_err());
        let agent = ActionConfig {
            mode: ActionMode::Agent,
            tag: Some("build".to_string()),
            ..ActionConfig::default()
        };
        assert!(bind(&config(vec![("x", agent)])).is_err());
    }

    #[test]
    fn terminals_are_per_checkout() {
        let root = target("/repo", "/repo");
        let linked = target("/repo-feature", "/repo");
        assert_eq!(terminal_tag("terminal", &root), "terminal");
        assert_eq!(terminal_tag("terminal", &linked), "terminal:/repo-feature");
    }

    #[test]
    fn picked_worktree_replaces_path_and_branch_only() {
        let base = Target {
            session: Some("repo".to_string()),
            ..target("/repo", "/repo")
        };
        let moved = base.in_worktree(&Worktree {
            path: "/repo-feature".to_string(),
            branch: "feature".to_string(),
        });
        assert_eq!(
            moved,
            Target {
                session: Some("repo".to_string()),
                path: "/repo-feature".to_string(),
                root: "/repo".to_string(),
                branch: "feature".to_string(),
            }
        );
    }

    #[test]
    fn placeholders_expand_to_quoted_values() {
        let target = Target {
            session: None,
            path: "/tmp/it's here".to_string(),
            root: "/tmp/root".to_string(),
            branch: "feat/x".to_string(),
        };
        assert_eq!(
            expand("git -C {path} push origin {branch} # {root}", &target),
            "git -C '/tmp/it'\\''s here' push origin 'feat/x' # '/tmp/root'"
        );
    }
}
