use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::agent::PaneId;

pub(crate) const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub pane_id: PaneId,
    pub session: String,
    pub lines: usize,
    pub generation: u64,
}

enum Event {
    Select(Option<Target>),
    Output(u64),
    Block(u64, bool, Vec<String>),
    Closed(u64),
    Stop,
}

pub struct Preview {
    tx: mpsc::Sender<Event>,
    worker: Option<JoinHandle<()>>,
    selected: Option<Target>,
}

impl Preview {
    pub fn new(deliver: impl Fn(Target, String) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_tx = tx.clone();
        let worker = thread::spawn(move || run(rx, worker_tx, deliver));
        Self {
            tx,
            worker: Some(worker),
            selected: None,
        }
    }

    pub fn select(&mut self, target: Option<Target>) {
        if self.selected != target {
            self.selected = target.clone();
            let _ = self.tx.send(Event::Select(target));
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        let _ = self.tx.send(Event::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug)]
enum Phase {
    Disconnected { retry_at: Instant },
    Attaching { deadline: Instant },
    Ready,
    Capturing { target: Target, deadline: Instant },
}

#[derive(Debug)]
enum Action {
    Connect { target: Target, epoch: u64 },
    Capture(Target),
    TimedOut(Target),
}

struct Scheduler {
    selected: Option<Target>,
    session: Option<String>,
    epoch: u64,
    phase: Phase,
    dirty: bool,
    next_frame: Instant,
}

impl Scheduler {
    fn new(now: Instant) -> Self {
        Self {
            selected: None,
            session: None,
            epoch: 0,
            phase: Phase::Disconnected { retry_at: now },
            dirty: false,
            next_frame: now,
        }
    }

    fn select(&mut self, target: Option<Target>, now: Instant) {
        if self.selected == target {
            return;
        }
        if self.session.as_ref().is_some_and(|session| {
            target
                .as_ref()
                .is_none_or(|target| &target.session != session)
        }) {
            self.disconnect(now);
        }
        self.selected = target;
        self.dirty = self.selected.is_some();
        if let Phase::Disconnected { retry_at } = &mut self.phase {
            *retry_at = now;
        }
    }

    fn output(&mut self, epoch: u64) {
        if epoch == self.epoch && self.session.is_some() {
            self.dirty = true;
        }
    }

    fn disconnect(&mut self, retry_at: Instant) {
        self.epoch += 1;
        self.session = None;
        self.phase = Phase::Disconnected { retry_at };
    }

    fn closed(&mut self, epoch: u64, now: Instant) {
        if epoch == self.epoch {
            self.disconnect(now + RETRY_INTERVAL);
        }
    }

    fn next_action(&mut self, now: Instant) -> Option<Action> {
        let target = self.selected.as_ref()?;
        match &self.phase {
            Phase::Disconnected { retry_at } if now >= *retry_at => {
                self.epoch += 1;
                self.session = Some(target.session.clone());
                self.phase = Phase::Attaching {
                    deadline: now + RESPONSE_TIMEOUT,
                };
                self.dirty = true;
                Some(Action::Connect {
                    target: target.clone(),
                    epoch: self.epoch,
                })
            }
            Phase::Attaching { deadline } | Phase::Capturing { deadline, .. }
                if now >= *deadline =>
            {
                let target = target.clone();
                self.disconnect(now + RETRY_INTERVAL);
                Some(Action::TimedOut(target))
            }
            Phase::Ready if self.dirty && now >= self.next_frame => {
                let target = target.clone();
                self.phase = Phase::Capturing {
                    target: target.clone(),
                    deadline: now + RESPONSE_TIMEOUT,
                };
                self.dirty = false;
                self.next_frame = now + FRAME_INTERVAL;
                Some(Action::Capture(target))
            }
            _ => None,
        }
    }

    fn wait(&self, now: Instant) -> Option<Duration> {
        self.selected.as_ref()?;
        let deadline = match &self.phase {
            Phase::Disconnected { retry_at } => *retry_at,
            Phase::Attaching { deadline } | Phase::Capturing { deadline, .. } => *deadline,
            Phase::Ready if self.dirty => self.next_frame,
            Phase::Ready => return None,
        };
        Some(deadline.saturating_duration_since(now))
    }

    fn complete(
        &mut self,
        epoch: u64,
        success: bool,
        lines: Vec<String>,
        now: Instant,
    ) -> Option<(Target, String)> {
        if epoch != self.epoch || self.session.is_none() {
            return None;
        }
        match std::mem::replace(&mut self.phase, Phase::Ready) {
            Phase::Attaching { .. } => {
                if success {
                    None
                } else {
                    self.disconnect(now + RETRY_INTERVAL);
                    self.selected.clone().map(|target| {
                        (
                            target,
                            format!("preview reconnecting: {}", lines.join("\n")),
                        )
                    })
                }
            }
            Phase::Capturing { target, .. } => {
                (self.selected.as_ref() == Some(&target)).then(|| {
                    let content = if success {
                        lines.join("\n")
                    } else {
                        format!("preview unavailable: {}", lines.join("\n"))
                    };
                    (target, content)
                })
            }
            phase => {
                self.phase = phase;
                None
            }
        }
    }
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    reader: Option<JoinHandle<()>>,
    output_pending: Arc<AtomicBool>,
    observed: Arc<Mutex<PaneId>>,
}

impl Connection {
    fn open(target: &Target, epoch: u64, tx: mpsc::Sender<Event>) -> std::io::Result<Self> {
        let mut child = Command::new("tmux")
            .args([
                "-C",
                "attach-session",
                "-E",
                "-f",
                "ignore-size,read-only",
                "-t",
            ])
            .arg(format!("={}", target.session))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let output_pending = Arc::new(AtomicBool::new(false));
        let pending = output_pending.clone();
        let observed = Arc::new(Mutex::new(target.pane_id.clone()));
        let reader_observed = observed.clone();
        let reader = thread::spawn(move || {
            let mut parser = Parser::default();
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some((success, lines)) = parser.push(&line) {
                    if tx.send(Event::Block(epoch, success, lines)).is_err() {
                        break;
                    }
                } else if !parser.in_block()
                    && invalidates_preview(&line, &reader_observed.lock().unwrap())
                    && !pending.swap(true, Ordering::AcqRel)
                    && tx.send(Event::Output(epoch)).is_err()
                {
                    break;
                }
            }
            let _ = tx.send(Event::Closed(epoch));
        });
        Ok(Self {
            child,
            stdin,
            reader: Some(reader),
            output_pending,
            observed,
        })
    }

    fn capture(&mut self, target: &Target) -> std::io::Result<()> {
        // Output arriving during capture must schedule another frame.
        self.output_pending.store(false, Ordering::Release);
        writeln!(
            self.stdin,
            "capture-pane -p -e -N -S -{} -t {}",
            target.lines, target.pane_id
        )?;
        self.stdin.flush()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn run(rx: mpsc::Receiver<Event>, tx: mpsc::Sender<Event>, deliver: impl Fn(Target, String)) {
    let mut scheduler = Scheduler::new(Instant::now());
    let mut connection: Option<Connection> = None;
    loop {
        match scheduler.next_action(Instant::now()) {
            Some(Action::Connect { target, epoch }) => {
                match Connection::open(&target, epoch, tx.clone()) {
                    Ok(client) => connection = Some(client),
                    Err(err) => {
                        scheduler.closed(epoch, Instant::now());
                        deliver(target, format!("preview reconnecting: {err}"));
                    }
                }
            }
            Some(Action::Capture(target)) => {
                if connection
                    .as_mut()
                    .expect("connected preview")
                    .capture(&target)
                    .is_err()
                {
                    scheduler.closed(scheduler.epoch, Instant::now());
                }
            }
            Some(Action::TimedOut(target)) => {
                deliver(
                    target,
                    "preview reconnecting: tmux response timed out".into(),
                );
            }
            None => {}
        }
        if scheduler.session.is_none() {
            connection = None;
        }
        let event = match scheduler.wait(Instant::now()) {
            Some(timeout) => rx.recv_timeout(timeout),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match event {
            Ok(Event::Select(target)) => {
                scheduler.select(target, Instant::now());
                if let (Some(client), Some(target)) = (&connection, &scheduler.selected) {
                    *client.observed.lock().unwrap() = target.pane_id.clone();
                }
            }
            Ok(Event::Output(epoch)) => scheduler.output(epoch),
            Ok(Event::Block(epoch, success, lines)) => {
                if let Some((target, content)) =
                    scheduler.complete(epoch, success, lines, Instant::now())
                {
                    deliver(target, content);
                }
            }
            Ok(Event::Closed(epoch)) => scheduler.closed(epoch, Instant::now()),
            Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if scheduler.session.is_none() {
            connection = None;
        }
    }
}

fn invalidates_preview(line: &str, selected: &PaneId) -> bool {
    for prefix in ["%output ", "%extended-output ", "%pane-mode-changed "] {
        if let Some(value) = line.strip_prefix(prefix) {
            return value.split_whitespace().next() == Some(selected.as_str());
        }
    }
    // A resize or pane switch can change screen contents without application output.
    [
        "%layout-change ",
        "%window-pane-changed ",
        "%session-window-changed ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

#[derive(Default)]
struct Parser {
    block: Option<(String, Vec<String>)>,
    attached: bool,
}

impl Parser {
    fn in_block(&self) -> bool {
        self.block.is_some()
    }

    fn push(&mut self, line: &str) -> Option<(bool, Vec<String>)> {
        if let Some((id, lines)) = &mut self.block {
            let end = line
                .strip_prefix("%end ")
                .map(|value| (true, value))
                .or_else(|| line.strip_prefix("%error ").map(|value| (false, value)));
            if let Some((success, value)) = end
                && value == id
            {
                let (id, lines) = self.block.take().unwrap();
                // Hooks also emit response blocks, but only commands from control
                // input have flag 1. The initial attach response is the exception.
                let requested = !self.attached || id.split_whitespace().last() == Some("1");
                self.attached = true;
                return requested.then_some((success, lines));
            }
            lines.push(line.to_string());
        } else if let Some(id) = line.strip_prefix("%begin ") {
            self.block = Some((id.to_string(), Vec::new()));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, session: &str, generation: u64) -> Target {
        Target {
            pane_id: PaneId::parse(id).unwrap(),
            session: session.into(),
            lines: 50,
            generation,
        }
    }

    fn ready(now: Instant) -> (Scheduler, u64) {
        let mut scheduler = Scheduler::new(now);
        scheduler.select(Some(target("%1", "one", 1)), now);
        let Some(Action::Connect { epoch, .. }) = scheduler.next_action(now) else {
            panic!("expected connection");
        };
        assert!(scheduler.complete(epoch, true, vec![], now).is_none());
        (scheduler, epoch)
    }

    #[test]
    fn idle_has_no_capture_deadline_and_output_after_idle_is_immediate() {
        let now = Instant::now();
        let mut empty = Scheduler::new(now);
        assert!(empty.next_action(now).is_none());
        assert_eq!(empty.wait(now), None);
        let (mut scheduler, epoch) = ready(now);
        assert!(matches!(
            scheduler.next_action(now),
            Some(Action::Capture(_))
        ));
        assert!(
            scheduler
                .complete(epoch, true, vec!["screen".into()], now)
                .is_some()
        );
        let later = now + Duration::from_secs(100);
        assert!(scheduler.next_action(later).is_none());
        assert_eq!(scheduler.wait(later), None);
        scheduler.output(epoch);
        assert!(matches!(
            scheduler.next_action(later),
            Some(Action::Capture(_))
        ));
    }

    #[test]
    fn bursts_coalesce_with_one_capture_in_flight_and_frame_rate_limit() {
        let now = Instant::now();
        let (mut scheduler, epoch) = ready(now);
        assert!(matches!(
            scheduler.next_action(now),
            Some(Action::Capture(_))
        ));
        for _ in 0..1000 {
            scheduler.output(epoch);
            assert!(scheduler.next_action(now).is_none());
        }
        let before = now + FRAME_INTERVAL - Duration::from_nanos(1);
        assert!(scheduler.next_action(before).is_none());
        scheduler.complete(epoch, true, vec![], before);
        assert!(scheduler.next_action(before).is_none());
        assert_eq!(scheduler.wait(before), Some(Duration::from_nanos(1)));
        assert!(matches!(
            scheduler.next_action(now + FRAME_INTERVAL),
            Some(Action::Capture(_))
        ));
        scheduler.complete(epoch, true, vec![], now + FRAME_INTERVAL);
        assert_eq!(scheduler.wait(now + FRAME_INTERVAL), None);
    }

    #[test]
    fn slow_capture_never_builds_a_queue_of_obsolete_frames() {
        let now = Instant::now();
        let (mut scheduler, epoch) = ready(now);
        scheduler.next_action(now);
        scheduler.output(epoch);
        let later = now + FRAME_INTERVAL * 3;
        assert!(scheduler.next_action(later).is_none());
        scheduler.complete(epoch, true, vec![], later);
        assert!(matches!(
            scheduler.next_action(later),
            Some(Action::Capture(_))
        ));
        scheduler.complete(epoch, true, vec![], later);
        assert_eq!(scheduler.wait(later), None);
    }

    #[test]
    fn selection_change_discards_in_flight_frame_without_bypassing_rate_limit() {
        let now = Instant::now();
        let (mut scheduler, epoch) = ready(now);
        scheduler.next_action(now);
        let selected = target("%2", "one", 2);
        scheduler.select(Some(selected.clone()), now);
        assert!(
            scheduler
                .complete(epoch, true, vec!["old pane".into()], now)
                .is_none()
        );
        assert!(scheduler.next_action(now).is_none());
        let Some(Action::Capture(request)) = scheduler.next_action(now + FRAME_INTERVAL) else {
            panic!("expected capture");
        };
        assert_eq!(request, selected);
        // A new selection generation must also invalidate the same pane's old frame.
        scheduler.select(Some(target("%2", "one", 3)), now + FRAME_INTERVAL);
        assert!(
            scheduler
                .complete(
                    epoch,
                    true,
                    vec!["old generation".into()],
                    now + FRAME_INTERVAL
                )
                .is_none()
        );
    }

    #[test]
    fn timed_out_capture_retries_without_accepting_old_connection_events() {
        let now = Instant::now();
        let (mut scheduler, old_epoch) = ready(now);
        scheduler.next_action(now);
        let expired = now + RESPONSE_TIMEOUT;
        assert!(matches!(
            scheduler.next_action(expired),
            Some(Action::TimedOut(_))
        ));
        assert!(
            scheduler
                .complete(old_epoch, true, vec!["late".into()], expired)
                .is_none()
        );
        assert_eq!(scheduler.wait(expired), Some(RETRY_INTERVAL));
        assert!(scheduler.next_action(expired).is_none());
        let retry = expired + RETRY_INTERVAL;
        let Some(Action::Connect { epoch, .. }) = scheduler.next_action(retry) else {
            panic!("expected reconnect");
        };
        assert_ne!(old_epoch, epoch);
        scheduler.closed(old_epoch, retry);
        scheduler.output(old_epoch);
        assert!(scheduler.complete(old_epoch, true, vec![], retry).is_none());
        assert!(matches!(scheduler.phase, Phase::Attaching { .. }));
        scheduler.complete(epoch, true, vec![], retry);
        assert!(matches!(
            scheduler.next_action(retry),
            Some(Action::Capture(_))
        ));
        scheduler.complete(epoch, true, vec!["recovered".into()], retry);
        scheduler.output(old_epoch);
        assert_eq!(scheduler.wait(retry), None);
    }

    #[test]
    fn attachment_failure_retries_and_deselection_cancels_retry() {
        let now = Instant::now();
        let mut scheduler = Scheduler::new(now);
        scheduler.select(Some(target("%1", "one", 1)), now);
        let Some(Action::Connect { epoch, .. }) = scheduler.next_action(now) else {
            panic!("expected connection")
        };
        let (_, error) = scheduler
            .complete(epoch, false, vec!["no session".into()], now)
            .unwrap();
        assert!(error.contains("no session"));
        assert!(scheduler.next_action(now).is_none());
        assert!(matches!(
            scheduler.next_action(now + RETRY_INTERVAL),
            Some(Action::Connect { .. })
        ));
        scheduler.select(None, now + RETRY_INTERVAL);
        assert!(
            scheduler
                .next_action(now + Duration::from_secs(10))
                .is_none()
        );
        assert_eq!(scheduler.wait(now), None);
    }

    #[test]
    fn session_switch_reconnects_and_discards_previous_session_response() {
        let now = Instant::now();
        let (mut scheduler, old_epoch) = ready(now);
        scheduler.next_action(now);
        let selected = target("%3", "two", 2);
        scheduler.select(Some(selected.clone()), now);
        let Some(Action::Connect { target, epoch }) = scheduler.next_action(now) else {
            panic!("expected connection")
        };
        assert_eq!(target, selected);
        assert!(
            scheduler
                .complete(old_epoch, true, vec!["old session".into()], now)
                .is_none()
        );
        scheduler.complete(epoch, true, vec![], now);
        assert!(scheduler.next_action(now).is_none());
        assert!(matches!(
            scheduler.next_action(now + FRAME_INTERVAL),
            Some(Action::Capture(_))
        ));
    }

    #[test]
    fn capture_preserves_ansi_blank_rows_and_notification_looking_text() {
        let mut parser = Parser::default();
        parser.push("%begin 123 4 1");
        for line in [
            "\x1b[31mred\x1b[0m",
            "",
            "%output %1 not a notification",
            "%end 999 5 1",
        ] {
            assert!(parser.push(line).is_none());
        }
        let (success, lines) = parser.push("%end 123 4 1").unwrap();
        assert!(success);
        assert_eq!(
            lines,
            [
                "\x1b[31mred\x1b[0m",
                "",
                "%output %1 not a notification",
                "%end 999 5 1"
            ]
        );
        assert!(!parser.in_block());
    }

    #[test]
    fn hook_responses_do_not_consume_pending_capture() {
        let mut parser = Parser::default();
        parser.push("%begin 123 1 0");
        assert_eq!(parser.push("%end 123 1 0"), Some((true, vec![])));
        parser.push("%begin 123 2 0");
        parser.push("hook output");
        assert!(parser.push("%end 123 2 0").is_none());
        parser.push("%begin 123 3 1");
        parser.push("screen");
        assert_eq!(
            parser.push("%end 123 3 1"),
            Some((true, vec!["screen".into()]))
        );
    }

    #[test]
    fn only_selected_pane_output_invalidates_preview() {
        let selected = PaneId::parse("%1").unwrap();
        assert!(invalidates_preview("%output %1 bytes", &selected));
        assert!(invalidates_preview(
            "%extended-output %1 10 : bytes",
            &selected
        ));
        assert!(!invalidates_preview("%output %10 bytes", &selected));
        assert!(!invalidates_preview("%output %2 bytes", &selected));
        assert!(invalidates_preview("%layout-change @1 layout", &selected));
    }

    #[test]
    fn capture_errors_do_not_desynchronize_next_response() {
        let mut parser = Parser::default();
        parser.push("%begin 123 4 1");
        parser.push("can't find pane");
        assert_eq!(
            parser.push("%error 123 4 1"),
            Some((false, vec!["can't find pane".into()]))
        );
        parser.push("%output %1 ignored");
        parser.push("%begin 123 5 1");
        assert_eq!(parser.push("%end 123 5 1"), Some((true, vec![])));
    }
}
