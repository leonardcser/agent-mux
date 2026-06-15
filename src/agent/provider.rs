use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    pub children: HashMap<i32, Vec<i32>>,
    pub comm: HashMap<i32, String>,
    pub args: HashMap<i32, String>,
}

const PROVIDERS: &[&str] = &[
    "smelt", "claude", "codex", "gemini", "opencode", "ralph", "kimi",
];

pub fn resolve(cmd: &str, shell_pid: i32, pt: &ProcessTable) -> String {
    if let Some(matched) = resolve_registered(cmd) {
        return matched.to_string();
    }

    for child_pid in pt.children.get(&shell_pid).into_iter().flatten() {
        if let Some(comm) = pt.comm.get(child_pid)
            && let Some(matched) = resolve_registered(comm)
        {
            return matched.to_string();
        }
        if let Some(args) = pt.args.get(child_pid) {
            if let Some(matched) = resolve_registered(args) {
                return matched.to_string();
            }
            for arg in args.split_whitespace() {
                let base = arg.rsplit('/').next().unwrap_or(arg);
                if let Some(matched) = resolve_registered(base) {
                    return matched.to_string();
                }
            }
        }
    }

    String::new()
}

fn resolve_registered(cmd: &str) -> Option<&'static str> {
    let normalized = cmd.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    for provider in PROVIDERS {
        if normalized.contains(provider) {
            return Some(provider);
        }
    }
    if let Some(base) = normalized.rsplit('/').next() {
        for provider in PROVIDERS {
            if base.contains(provider) {
                return Some(provider);
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
