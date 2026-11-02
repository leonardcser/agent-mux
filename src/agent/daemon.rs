use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

use super::{ipc, persist::state_dir, watch};

pub const PROTOCOL_VERSION: u32 = 1;
static BUILD: OnceLock<BuildInfo> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub build_id: String,
    pub protocol_version: u32,
}

impl BuildInfo {
    pub fn check_compatibility(&self) -> Result<()> {
        if self.protocol_version != PROTOCOL_VERSION {
            bail!("agent-mux was updated; reopen this TUI to use its new protocol");
        }
        Ok(())
    }
}

pub fn initialize() -> Result<()> {
    // Hash the running image, not a replacement installed at the same path.
    #[cfg(target_os = "linux")]
    let executable = PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe().context("locate executable")?;
    let _ = BUILD.set(BuildInfo {
        build_id: fingerprint(&executable)?,
        protocol_version: PROTOCOL_VERSION,
    });
    Ok(())
}

pub fn build_info() -> &'static BuildInfo {
    BUILD.get().expect("running build initialized")
}

pub struct Manager {
    launch_path: PathBuf,
    registered: bool,
}

fn fingerprint(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("open installed executable")?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl Manager {
    pub fn new() -> Result<Self> {
        let invocation = PathBuf::from(std::env::args_os().next().context("missing launch path")?);
        let directory = std::env::current_dir()?;
        let launch_path = if invocation.components().count() > 1 {
            directory.join(invocation)
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|entry| directory.join(entry).join(&invocation))
                .find(|path| {
                    fs::metadata(path).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
                .context("cannot resolve executable in PATH")?
        };
        Ok(Self {
            launch_path,
            registered: false,
        })
    }

    pub fn ensure_running(&mut self) -> Result<BuildInfo> {
        fs::create_dir_all(state_dir()).context("create state dir")?;
        let lifecycle = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir().join("lifecycle.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match lifecycle.try_lock_exclusive() {
                Ok(()) => break,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("timed out waiting for daemon lifecycle lock");
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(err.into()),
            }
        }

        // A new TUI selects the installation once. Older open TUIs follow that
        // selection instead of repeatedly restoring their own executable.
        let selection = state_dir().join("daemon-executable.json");
        if !self.registered {
            super::persist::write_json_file(selection.clone(), &self.launch_path)?;
            self.registered = true;
        }
        let executable: PathBuf = serde_json::from_slice(&fs::read(selection)?)?;
        let expected = fingerprint(&executable)?;
        if let Some(info) = ready_build(&expected) {
            return Ok(info);
        }
        if watch::is_running() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Some(info) = ready_build(&expected) {
                    return Ok(info);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        // Never stop a working watcher for a replacement that cannot identify itself.
        validate(&executable, &expected).context("validate installed executable")?;
        if watch::is_running() {
            stop(&executable)?;
        }

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(watch::log_path())
            .context("open watch log")?;
        let mut child = Command::new(executable)
            .arg("watch")
            .process_group(0)
            .stdin(Stdio::null())
            .stderr(Stdio::from(log.try_clone()?))
            .stdout(Stdio::from(log))
            .spawn()
            .context("start watcher")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(info) = ready_build(&expected) {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(info);
            }
            if let Some(status) = child.try_wait()? {
                bail!(
                    "watcher exited with {status}; see {}",
                    watch::log_path().display()
                );
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "watcher did not become ready; see {}",
                    watch::log_path().display()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn ready_build(expected: &str) -> Option<BuildInfo> {
    match ipc::request(&ipc::Request::Status).ok()? {
        ipc::Response::Status { build, healthy } if build.build_id == expected && healthy => {
            Some(build)
        }
        _ => None,
    }
}

fn validate(executable: &Path, expected: &str) -> Result<()> {
    let (output, child_output) = UnixStream::pair()?;
    let timeout = Duration::from_secs(3);
    // Darwin rejects socket options after the peer closes. Configure this before
    // spawning; it also bounds reads when a descendant inherits stdout.
    output.set_read_timeout(Some(timeout))?;
    let mut command = Command::new(executable);
    command
        .arg("--daemon-info")
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(child_output)))
        .stderr(Stdio::null());
    let deadline = Instant::now() + timeout;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            // An installer or a forked child can briefly retain a writable fd.
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    // Close the parent's stdout writer so the reader can observe EOF.
    drop(command);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("replacement validation timed out");
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        bail!("replacement validation exited with {status}");
    }
    let mut bytes = Vec::new();
    output
        .take(4096)
        .read_to_end(&mut bytes)
        .context("read replacement validation output")?;
    let info: BuildInfo = serde_json::from_slice(&bytes).context("invalid build information")?;
    if info.build_id != expected || fingerprint(executable)? != expected {
        bail!("installed executable changed during validation");
    }
    Ok(())
}

fn stop(executable: &Path) -> Result<()> {
    let _ = ipc::request(&ipc::Request::Shutdown);
    if wait_stopped(Duration::from_secs(2)) {
        return Ok(());
    }

    // Legacy or unresponsive daemons cannot acknowledge Shutdown. Never trust
    // the PID file alone: verify the executable and watch subcommand first.
    let pid: u32 = fs::read_to_string(watch::lock_path())?.trim().parse()?;
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .without_tasks(),
    );
    if let Some(process) = system.process(pid) {
        let path = process.exe().context("cannot verify watcher executable")?;
        let path = path.to_string_lossy();
        let path = Path::new(path.strip_suffix(" (deleted)").unwrap_or(&path));
        let launched_from = process.cmd().first().map(Path::new);
        let same_image = fs::canonicalize(executable).is_ok_and(|expected| path == expected);
        if (!same_image && launched_from != Some(executable))
            || !process.cmd().iter().skip(1).any(|arg| arg == "watch")
        {
            bail!("refusing to stop unverified watcher PID {pid}");
        }
        let start_time = process.start_time();
        process.kill_with(Signal::Term);
        if !wait_stopped(Duration::from_secs(2)) {
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().without_tasks(),
            );
            if let Some(process) = system.process(pid) {
                if process.start_time() != start_time {
                    bail!("refusing to stop reused watcher PID {pid}");
                }
                process.kill_with(Signal::Kill);
            }
        }
    }
    if !wait_stopped(Duration::from_secs(2)) {
        bail!("watcher did not stop");
    }
    Ok(())
}

fn wait_stopped(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while watch::is_running() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(source: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("replacement");
        fs::write(&path, source).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (root, path)
    }

    #[test]
    fn validation_accepts_identified_builds_without_assuming_protocol() {
        let (_root, path) = executable("#!/bin/sh\ncat \"$0.json\"\n");
        let expected = fingerprint(&path).unwrap();
        for protocol_version in [PROTOCOL_VERSION, 999] {
            let info = BuildInfo {
                build_id: expected.clone(),
                protocol_version,
            };
            fs::write(
                path.with_extension("json"),
                serde_json::to_vec(&info).unwrap(),
            )
            .unwrap();
            validate(&path, &expected).unwrap();
        }
    }

    #[test]
    fn validation_rejects_failed_malformed_or_misidentified_builds() {
        for (source, message) in [
            ("#!/bin/sh\nexit 1\n", "replacement validation exited"),
            (
                "#!/bin/sh\nprintf 'not-json'\n",
                "invalid build information",
            ),
            ("#!/bin/sh\nprintf '{}'\n", "invalid build information"),
            (
                "#!/bin/sh\nprintf '{\"build_id\":\"wrong\",\"protocol_version\":1}'\n",
                "installed executable changed",
            ),
        ] {
            let (_root, path) = executable(source);
            let error = validate(&path, &fingerprint(&path).unwrap()).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        let (root, path) = executable("invalid executable\n");
        assert!(validate(&path, &fingerprint(&path).unwrap()).is_err());
        assert!(fingerprint(&root.path().join("missing")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validation_waits_for_transient_executable_writer() {
        let (_root, path) = executable("#!/bin/sh\ncat \"$0.json\"\n");
        let expected = fingerprint(&path).unwrap();
        fs::write(
            path.with_extension("json"),
            serde_json::to_vec(&BuildInfo {
                build_id: expected.clone(),
                protocol_version: PROTOCOL_VERSION,
            })
            .unwrap(),
        )
        .unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(writer);
        });
        let result = validate(&path, &expected);
        release.join().unwrap();
        result.unwrap();
    }

    #[test]
    fn validation_bounds_hung_executable() {
        let (_root, path) = executable("#!/bin/sh\nexec sleep 60\n");
        let error = validate(&path, &fingerprint(&path).unwrap()).unwrap_err();
        assert!(error.to_string().contains("validation timed out"));
    }

    #[test]
    fn validation_bounds_stdout_inherited_by_descendant() {
        let (_root, path) =
            executable("#!/bin/sh\nsleep 60 &\necho $! > \"$0.pid\"\nprintf '{}'\n");
        let result = validate(&path, &fingerprint(&path).unwrap());
        // This PID was created by this fixture, not taken from daemon state.
        let pid = fs::read_to_string(path.with_extension("pid")).unwrap();
        let _ = Command::new("kill").args(["-TERM", pid.trim()]).status();
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("read replacement validation output")
        );
    }
}
