use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    pub children: HashMap<i32, Vec<i32>>,
    pub comm: HashMap<i32, String>,
    pub args: HashMap<i32, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMatch {
    pub name: String,
    pub pid: i32,
}

struct ProviderPattern {
    label: &'static str,
    needles: &'static [&'static str],
}

const PROVIDERS: &[ProviderPattern] = &[
    ProviderPattern {
        label: "smelt",
        needles: &["smelt"],
    },
    ProviderPattern {
        label: "claude",
        needles: &["claude"],
    },
    ProviderPattern {
        label: "codex",
        needles: &["codex"],
    },
    ProviderPattern {
        label: "gemini",
        needles: &["gemini"],
    },
    ProviderPattern {
        label: "opencode",
        needles: &["opencode"],
    },
    ProviderPattern {
        label: "kimi",
        needles: &["kimi", "kimi-code", "@moonshot-ai/kimi-code"],
    },
];

pub fn resolve(cmd: &str, shell_pid: i32, pt: &ProcessTable) -> Option<ProviderMatch> {
    let current = resolve_registered(cmd);
    if let Some(matched) = resolve_descendant(shell_pid, pt) {
        return Some(matched);
    }
    current.map(|matched| ProviderMatch {
        name: matched.to_string(),
        pid: shell_pid,
    })
}

fn resolve_descendant(root_pid: i32, pt: &ProcessTable) -> Option<ProviderMatch> {
    let mut queue = VecDeque::from([root_pid]);
    let mut seen = HashMap::new();
    while let Some(pid) = queue.pop_front() {
        if seen.insert(pid, true).is_some() {
            continue;
        }
        for child_pid in pt.children.get(&pid).into_iter().flatten() {
            if let Some(matched) = resolve_process(*child_pid, pt) {
                return Some(matched);
            }
            queue.push_back(*child_pid);
        }
    }
    None
}

fn resolve_process(pid: i32, pt: &ProcessTable) -> Option<ProviderMatch> {
    if let Some(comm) = pt.comm.get(&pid)
        && let Some(name) = resolve_registered(comm)
    {
        return Some(ProviderMatch {
            name: name.to_string(),
            pid,
        });
    }
    let args = pt.args.get(&pid)?;
    if let Some(name) = resolve_registered(args) {
        return Some(ProviderMatch {
            name: name.to_string(),
            pid,
        });
    }
    for arg in args.split_whitespace() {
        let base = arg.rsplit('/').next().unwrap_or(arg);
        if let Some(name) = resolve_registered(base) {
            return Some(ProviderMatch {
                name: name.to_string(),
                pid,
            });
        }
    }
    None
}

fn resolve_registered(cmd: &str) -> Option<&'static str> {
    let normalized = cmd.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    for provider in PROVIDERS {
        if provider
            .needles
            .iter()
            .any(|needle| normalized.contains(needle))
        {
            return Some(provider.label);
        }
    }
    if let Some(base) = normalized.rsplit('/').next() {
        for provider in PROVIDERS {
            if provider.needles.iter().any(|needle| base.contains(needle)) {
                return Some(provider.label);
            }
        }
    }
    None
}

pub fn parse_process_table(out: &str) -> ProcessTable {
    let mut pt = ProcessTable::default();
    for line in out.trim().lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<i32>() else {
            continue;
        };
        let Ok(ppid) = fields[1].parse::<i32>() else {
            continue;
        };
        let mut cmdline = line.trim_start_matches(fields[0]).trim_start();
        cmdline = cmdline.trim_start_matches(fields[1]).trim_start();
        pt.children.entry(ppid).or_default().push(pid);
        pt.comm.insert(pid, fields[2].to_string());
        pt.args.insert(pid, cmdline.to_string());
    }
    pt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_provider_descendant_pid_when_tmux_reports_shell() {
        let mut pt = ProcessTable::default();
        pt.children.insert(10, vec![20]);
        pt.children.insert(20, vec![30]);
        pt.comm.insert(20, "bash".to_string());
        pt.args.insert(20, "bash wrapper".to_string());
        pt.comm.insert(30, "smelt".to_string());
        pt.args.insert(30, "smelt".to_string());

        let matched = resolve("bash", 10, &pt).unwrap();

        assert_eq!(matched.name, "smelt");
        assert_eq!(matched.pid, 30);
    }

    #[test]
    fn falls_back_to_pane_pid_for_direct_provider_process() {
        let pt = ProcessTable::default();

        let matched = resolve("smelt", 10, &pt).unwrap();

        assert_eq!(matched.name, "smelt");
        assert_eq!(matched.pid, 10);
    }

    #[test]
    fn resolves_kimi_binary_from_tmux_command() {
        let matched = resolve("kimi", 10, &ProcessTable::default()).unwrap();

        assert_eq!(matched.name, "kimi");
        assert_eq!(matched.pid, 10);
    }

    #[test]
    fn resolves_kimi_code_package_command_line() {
        let pt = parse_process_table(
            "42 10 node /home/user/.local/share/pnpm/global/5/node_modules/@moonshot-ai/kimi-code/dist/main.mjs\n",
        );

        let matched = resolve("node", 10, &pt).unwrap();

        assert_eq!(matched.name, "kimi");
        assert_eq!(matched.pid, 42);
    }

    #[test]
    fn resolves_kimi_dev_entrypoint_path() {
        let pt = parse_process_table("42 10 tsx /tmp/kimi-code/apps/kimi-code/src/main.ts\n");

        let matched = resolve("tsx", 10, &pt).unwrap();

        assert_eq!(matched.name, "kimi");
        assert_eq!(matched.pid, 42);
    }
}
