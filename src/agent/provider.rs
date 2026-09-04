use std::collections::{HashMap, HashSet, VecDeque};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    children: HashMap<i32, Vec<i32>>,
    processes: HashMap<i32, ProcessInfo>,
}

#[derive(Debug, Clone)]
struct ProcessInfo {
    name: String,
    executable: String,
    argv: Vec<String>,
}

impl ProcessTable {
    pub fn capture() -> Self {
        let refresh = ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .without_tasks();
        let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
        let script_pids: Vec<_> = system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                let executable_is_runtime = process
                    .exe()
                    .and_then(|path| path.file_name())
                    .is_some_and(|name| is_script_runtime(&name.to_string_lossy()));
                let name_is_runtime = is_script_runtime(&process.name().to_string_lossy());
                (executable_is_runtime || name_is_runtime).then_some(*pid)
            })
            .collect();
        if !script_pids.is_empty() {
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&script_pids),
                false,
                ProcessRefreshKind::nothing()
                    .with_cmd(UpdateKind::OnlyIfNotSet)
                    .without_tasks(),
            );
        }

        let mut table = Self::default();
        for process in system.processes().values() {
            let Ok(pid) = i32::try_from(process.pid().as_u32()) else {
                continue;
            };
            let parent = process
                .parent()
                .and_then(|parent| i32::try_from(parent.as_u32()).ok());
            if let Some(parent) = parent {
                table.children.entry(parent).or_default().push(pid);
            }

            let name = process.name().to_string_lossy().into_owned();
            let executable = process
                .exe()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            let argv = process
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            table.processes.insert(
                pid,
                ProcessInfo {
                    name,
                    executable,
                    argv,
                },
            );
        }

        table
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMatch {
    pub name: String,
    pub pid: i32,
}

enum ProviderSignature {
    Executable(&'static [&'static str]),
    Script {
        runtimes: &'static [&'static str],
        entrypoints: &'static [&'static str],
    },
}

struct ProviderPattern {
    label: &'static str,
    signatures: &'static [ProviderSignature],
}

const NODE_RUNTIMES: &[&str] = &["node", "nodejs"];

const PROVIDERS: &[ProviderPattern] = &[
    ProviderPattern {
        label: "smelt",
        signatures: &[ProviderSignature::Executable(&["smelt"])],
    },
    ProviderPattern {
        label: "claude",
        signatures: &[
            ProviderSignature::Executable(&["claude", "claude.exe"]),
            ProviderSignature::Script {
                runtimes: NODE_RUNTIMES,
                entrypoints: &[
                    "claude",
                    "@anthropic-ai/claude-code/cli.mjs",
                    "@anthropic-ai/claude-code/cli.js",
                ],
            },
        ],
    },
    ProviderPattern {
        label: "codex",
        signatures: &[
            ProviderSignature::Executable(&["codex"]),
            ProviderSignature::Script {
                runtimes: NODE_RUNTIMES,
                entrypoints: &["codex", "@openai/codex/bin/codex.js"],
            },
        ],
    },
    ProviderPattern {
        label: "gemini",
        signatures: &[
            ProviderSignature::Executable(&["gemini"]),
            ProviderSignature::Script {
                runtimes: NODE_RUNTIMES,
                entrypoints: &[
                    "gemini",
                    "@google/gemini-cli/dist/index.js",
                    "@google/gemini-cli/bundle/gemini.js",
                ],
            },
        ],
    },
    ProviderPattern {
        label: "opencode",
        signatures: &[ProviderSignature::Executable(&["opencode", "opencode.exe"])],
    },
    ProviderPattern {
        label: "kimi",
        signatures: &[
            ProviderSignature::Executable(&["kimi", "kimi-code", "kimi code"]),
            ProviderSignature::Script {
                runtimes: NODE_RUNTIMES,
                entrypoints: &["kimi", "kimi-code", "@moonshot-ai/kimi-code/dist/main.mjs"],
            },
            ProviderSignature::Script {
                runtimes: &["tsx"],
                entrypoints: &["kimi-code/apps/kimi-code/src/main.ts"],
            },
        ],
    },
];

pub fn resolve(cmd: &str, shell_pid: i32, pt: &ProcessTable) -> Option<ProviderMatch> {
    let current = resolve_executable(cmd);
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
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
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
    let process = pt.processes.get(&pid)?;
    let name = resolve_executable(&process.executable)
        .or_else(|| resolve_executable(&process.name))
        .or_else(|| resolve_script(process))?;
    Some(ProviderMatch {
        name: name.to_string(),
        pid,
    })
}

fn resolve_executable(command: &str) -> Option<&'static str> {
    let executable = command_basename(command);
    if executable.is_empty() {
        return None;
    }
    PROVIDERS.iter().find_map(|provider| {
        provider
            .signatures
            .iter()
            .any(|signature| match signature {
                ProviderSignature::Executable(candidates) => candidates
                    .iter()
                    .any(|candidate| executable.eq_ignore_ascii_case(candidate)),
                ProviderSignature::Script { .. } => false,
            })
            .then_some(provider.label)
    })
}

fn is_script_runtime(command: &str) -> bool {
    let executable = command_basename(command);
    PROVIDERS.iter().any(|provider| {
        provider.signatures.iter().any(|signature| match signature {
            ProviderSignature::Script { runtimes, .. } => runtimes
                .iter()
                .any(|runtime| executable.eq_ignore_ascii_case(runtime)),
            ProviderSignature::Executable(_) => false,
        })
    })
}

fn resolve_script(process: &ProcessInfo) -> Option<&'static str> {
    let entrypoint = process.argv.get(1)?.replace('\\', "/").to_ascii_lowercase();
    PROVIDERS.iter().find_map(|provider| {
        provider
            .signatures
            .iter()
            .any(|signature| match signature {
                ProviderSignature::Script {
                    runtimes,
                    entrypoints,
                } => {
                    process_matches_any_name(process, runtimes)
                        && entrypoints
                            .iter()
                            .any(|candidate| entrypoint_matches(&entrypoint, candidate))
                }
                ProviderSignature::Executable(_) => false,
            })
            .then_some(provider.label)
    })
}

fn process_matches_any_name(process: &ProcessInfo, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| {
        process.name.eq_ignore_ascii_case(candidate)
            || process.executable.eq_ignore_ascii_case(candidate)
    })
}

fn entrypoint_matches(entrypoint: &str, candidate: &str) -> bool {
    entrypoint == candidate
        || entrypoint
            .strip_suffix(candidate)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

fn command_basename(command: &str) -> &str {
    command
        .trim()
        .trim_matches(['\'', '"'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_process(
        table: &mut ProcessTable,
        pid: i32,
        parent: i32,
        executable: &str,
        argv: &[&str],
    ) {
        table.children.entry(parent).or_default().push(pid);
        table.processes.insert(
            pid,
            ProcessInfo {
                name: executable.to_string(),
                executable: executable.to_string(),
                argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            },
        );
    }

    #[test]
    fn resolves_provider_descendant_pid_when_tmux_reports_shell() {
        let mut table = ProcessTable::default();
        add_process(&mut table, 20, 10, "bash", &["bash", "wrapper"]);
        add_process(&mut table, 30, 20, "smelt", &["smelt"]);

        let matched = resolve("bash", 10, &table).unwrap();

        assert_eq!(matched.name, "smelt");
        assert_eq!(matched.pid, 30);
    }

    #[test]
    fn falls_back_to_pane_pid_for_direct_provider_process() {
        let matched = resolve("smelt", 10, &ProcessTable::default()).unwrap();

        assert_eq!(matched.name, "smelt");
        assert_eq!(matched.pid, 10);
    }

    #[test]
    fn resolves_exact_native_executables() {
        for (command, provider) in [
            ("smelt", "smelt"),
            ("claude.exe", "claude"),
            ("codex", "codex"),
            ("gemini", "gemini"),
            ("opencode.exe", "opencode"),
            ("Kimi Code", "kimi"),
        ] {
            let matched = resolve(command, 10, &ProcessTable::default()).unwrap();

            assert_eq!(matched.name, provider);
            assert_eq!(matched.pid, 10);
        }
    }

    #[test]
    fn resolves_provider_from_exact_process_name_when_executable_differs() {
        let mut table = ProcessTable::default();
        add_process(&mut table, 42, 10, "node", &["node"]);
        table.processes.get_mut(&42).unwrap().name = "Kimi Code".to_string();

        let matched = resolve("node", 10, &table).unwrap();

        assert_eq!(matched.name, "kimi");
        assert_eq!(matched.pid, 42);
    }

    #[test]
    fn resolves_documented_script_entrypoints() {
        for (runtime, entrypoint, provider) in [
            (
                "node",
                "/home/user/node_modules/@anthropic-ai/claude-code/cli.mjs",
                "claude",
            ),
            (
                "node",
                "/home/user/node_modules/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
            (
                "node",
                "/home/user/node_modules/@openai/codex/bin/codex.js",
                "codex",
            ),
            (
                "node",
                "/home/user/node_modules/@google/gemini-cli/dist/index.js",
                "gemini",
            ),
            (
                "node",
                "/home/user/node_modules/@google/gemini-cli/bundle/gemini.js",
                "gemini",
            ),
            (
                "node",
                "/home/A User/node_modules/@moonshot-ai/kimi-code/dist/main.mjs",
                "kimi",
            ),
        ] {
            let mut table = ProcessTable::default();
            add_process(&mut table, 42, 10, runtime, &[runtime, entrypoint]);

            let matched = resolve(runtime, 10, &table).unwrap();

            assert_eq!(matched.name, provider);
            assert_eq!(matched.pid, 42);
        }
    }

    #[test]
    fn resolves_script_symlink_by_exact_entrypoint_name() {
        let mut table = ProcessTable::default();
        add_process(
            &mut table,
            42,
            10,
            "node",
            &["node", "/opt/homebrew/bin/gemini"],
        );

        let matched = resolve("node", 10, &table).unwrap();

        assert_eq!(matched.name, "gemini");
        assert_eq!(matched.pid, 42);
    }

    #[test]
    fn resolves_kimi_development_entrypoint() {
        let mut table = ProcessTable::default();
        add_process(
            &mut table,
            42,
            10,
            "tsx",
            &["tsx", "/tmp/kimi-code/apps/kimi-code/src/main.ts"],
        );

        let matched = resolve("tsx", 10, &table).unwrap();

        assert_eq!(matched.name, "kimi");
        assert_eq!(matched.pid, 42);
    }

    #[test]
    fn ignores_other_scripts_inside_provider_packages() {
        for entrypoint in [
            "/tmp/node_modules/@anthropic-ai/claude-code/scripts/build.js",
            "/tmp/node_modules/@openai/codex/bin/install.js",
            "/tmp/node_modules/@google/gemini-cli/scripts/postinstall.js",
            "/tmp/node_modules/@moonshot-ai/kimi-code/dist/install.mjs",
        ] {
            let mut table = ProcessTable::default();
            add_process(&mut table, 42, 10, "node", &["node", entrypoint]);

            assert_eq!(resolve("node", 10, &table), None);
        }
    }

    #[test]
    fn ignores_smelt_names_in_cargo_compiler_arguments() {
        let mut table = ProcessTable::default();
        add_process(
            &mut table,
            20,
            10,
            "cargo",
            &["cargo", "install", "--path", "."],
        );
        add_process(
            &mut table,
            30,
            20,
            "rustc",
            &[
                "rustc",
                "--crate-name",
                "smelt_store",
                "--out-dir",
                "/home/user/smelt/target/release/deps",
            ],
        );

        assert_eq!(resolve("cargo", 10, &table), None);
    }

    #[test]
    fn finds_smelt_after_cargo_run_starts_the_binary() {
        let mut table = ProcessTable::default();
        add_process(&mut table, 20, 10, "cargo", &["cargo", "run"]);
        add_process(
            &mut table,
            30,
            20,
            "rustc",
            &[
                "rustc",
                "--crate-name",
                "smelt_agent",
                "--out-dir",
                "/home/user/smelt/target/debug",
            ],
        );
        add_process(
            &mut table,
            40,
            20,
            "smelt",
            &["/home/user/smelt/target/debug/smelt"],
        );

        let matched = resolve("cargo", 10, &table).unwrap();

        assert_eq!(matched.name, "smelt");
        assert_eq!(matched.pid, 40);
    }

    #[test]
    fn ignores_provider_names_in_unrelated_executable_names() {
        for command in ["cargo-smelt", "claude-helper", "codex-build", "not-gemini"] {
            assert_eq!(resolve(command, 10, &ProcessTable::default()), None);
        }
    }

    #[test]
    fn ignores_provider_entrypoints_outside_script_position() {
        let mut table = ProcessTable::default();
        add_process(
            &mut table,
            42,
            10,
            "node",
            &[
                "node",
                "/tmp/build.js",
                "--output",
                "/tmp/node_modules/@google/gemini-cli/bundle/gemini.js",
            ],
        );

        assert_eq!(resolve("node", 10, &table), None);
    }
}
