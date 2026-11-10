use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use smelt_term::ansi::parse_ansi_lines;
use smelt_term::geometry::Rect;
use smelt_term::grid::{Color, GridSlice, Style};
use smelt_term::{
    Axis, DividerStyles, HitRegistry, LayoutTree, Line, NoopSizer, PaintId, ResolvedLayout, Split,
    SplitOptions, SplitPane, SplitResizeMode, SplitResponse, SplitSize, Surface, TerminalSession,
};

use crate::actions::{self, Action as UserAction};
use crate::agent::ipc;
#[cfg(not(test))]
use crate::agent::persist::update_ui_state;
use crate::agent::persist::{
    CachedProject, Snapshot, UiState, apply_ui_state, load_snapshot, load_ui_state,
    panes_from_snapshot, ui_pane_state_is_empty,
};
use crate::agent::{Pane, PaneId, PaneStatus, kill_pane, switch_to_pane};
use crate::config::{
    Config, SectionBehavior, SidebarConfig, SortMode, StartupSelection, WorkingSection,
};
use crate::keys;
use crate::picker::{Outcome, Picker};
use crate::preview::{FRAME_INTERVAL, Preview, Target as PreviewTarget};

const SIDEBAR: PaintId = PaintId(1);
const PREVIEW: PaintId = PaintId(3);
const MIN_SIDEBAR: u16 = 20;
const MIN_PREVIEW: u16 = 20;
const LOADING_DELAY: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
enum Hit {
    Pane(String),
    Project(String),
    Section(Section),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Working,
    Stashed,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Stashed => "stashed",
        }
    }
}

#[derive(Clone, Debug)]
enum TreeItem {
    SectionSpacer,
    SectionHeader(Section, usize),
    Workspace(String),
    ProjectGroup(String),
    Pane(String),
    /// A known project without sessions, keyed by its root.
    Project(String),
}

#[derive(Debug)]
enum Msg {
    PanesLoaded {
        snapshot: Option<Snapshot>,
        ui_state: UiState,
        live: bool,
    },
    PreviewReady,
    PaneKilled {
        pane_id: String,
        err: Option<String>,
    },
    ActionDone {
        err: Option<String>,
        leaves: bool,
    },
    SubscriptionEnded,
    DaemonChecked(Option<String>),
    Input(io::Result<Event>),
}

#[derive(Default)]
struct LatestPreview {
    frame: Mutex<Option<(PreviewTarget, String)>>,
}

impl LatestPreview {
    fn replace(&self, target: PreviewTarget, content: String) -> bool {
        let mut frame = self.frame.lock().unwrap();
        let notify = frame.is_none();
        *frame = Some((target, content));
        notify
    }

    fn take(&self) -> Option<(PreviewTarget, String)> {
        self.frame.lock().unwrap().take()
    }
}

pub fn run(tmux_session: String) -> Result<()> {
    let config = Config::load();
    let mut term = TerminalSession::builder()
        .buffer_capacity(128 * 1024)
        .focus_events(true)
        .enter_stdout()?;
    let (w, h) = term.size()?;
    let mut surface = Surface::new(w, h);
    let config = match config {
        Ok(config) => config,
        Err(err) => {
            return run_config_error(&mut surface, term.writer(), &format!("{err:#}"))
                .map_err(Into::into);
        }
    };

    let mut app = App::new(tmux_session, config.sidebar);
    app.actions = actions::bind(&config)?;
    app.resize(w, h);
    run_loop(&mut surface, term.writer(), &mut app).map_err(Into::into)
}

fn run_config_error<W: Write>(
    surface: &mut Surface,
    writer: &mut W,
    error: &str,
) -> io::Result<()> {
    surface.set_layout(LayoutTree::leaf(PREVIEW));
    let mut scroll = 0usize;
    loop {
        surface.render(writer, |_, slice| {
            render_config_error(slice, error, &mut scroll);
        })?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                _ => {}
            },
            Event::Resize(width, height) => surface.set_terminal_size(width, height),
            _ => {}
        }
    }
}

fn config_error_lines(error: &str, width: u16) -> Vec<String> {
    let mut lines = Vec::new();
    for mut remaining in error.lines() {
        if remaining.is_empty() {
            lines.push(String::new());
        }
        while !remaining.is_empty() {
            let line = smelt_term::grid::truncate_width(remaining, width.max(2));
            remaining = &remaining[line.len()..];
            lines.push(line);
        }
    }
    lines
}

fn render_config_error(slice: &mut GridSlice<'_>, error: &str, scroll: &mut usize) {
    let lines = config_error_lines(error, slice.width().saturating_sub(4));
    let height = slice.height().saturating_sub(6) as usize;
    *scroll = (*scroll).min(lines.len().saturating_sub(height));
    slice.put_str(
        2,
        1,
        "Configuration error",
        Style::new().fg(Color::Red).bold(),
    );
    for (row, line) in lines.iter().skip(*scroll).take(height).enumerate() {
        slice.put_str(2, row as u16 + 3, line, Style::new().fg(Color::White));
    }
    slice.put_str(
        2,
        slice.height().saturating_sub(2),
        "q / Esc: quit    j / k: scroll",
        Style::new().fg(Color::DarkGrey),
    );
}

fn run_loop<W: Write>(surface: &mut Surface, writer: &mut W, app: &mut App) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut last_panes = Instant::now() - Duration::from_millis(500);
    let mut last_subscribe = Instant::now() - Duration::from_secs(1);
    let mut panes_pending = false;
    let preview_tx = tx.clone();
    let latest_preview = Arc::new(LatestPreview::default());
    let preview_mailbox = latest_preview.clone();
    let mut preview = Preview::new(move |target, content| {
        if preview_mailbox.replace(target, content) {
            let _ = preview_tx.send(Msg::PreviewReady);
        }
    });
    let _input = InputReader::start(tx.clone());
    let mut pending = None;
    let mut subscribed = false;
    let mut subscribe_pending = true;

    let mut daemon = crate::agent::daemon::Manager::new().map_err(io::Error::other)?;
    let daemon_tx = tx.clone();
    thread::spawn(move || {
        loop {
            let err = daemon
                .ensure_running()
                .and_then(|info| info.check_compatibility())
                .err()
                .map(|err| format!("{err:#}"));
            if daemon_tx.send(Msg::DaemonChecked(err)).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(3));
        }
    });
    spawn_subscribe_panes(&tx);

    loop {
        let batch_start = Instant::now();
        for msg in pending.take().into_iter().chain(rx.try_iter()) {
            match msg {
                Msg::PanesLoaded {
                    snapshot,
                    ui_state,
                    live,
                } => {
                    if live {
                        dirty |= app.loading_since.take().is_some();
                        dirty |= app.err.take().is_some();
                        subscribed = true;
                        subscribe_pending = false;
                    } else {
                        panes_pending = false;
                    }
                    if let Some(snapshot) = snapshot {
                        dirty |= app.apply_snapshot(snapshot, ui_state);
                    }
                }
                Msg::PreviewReady => {
                    if let Some((target, content)) = latest_preview.take()
                        && target.generation == app.preview_gen
                        && app
                            .current_pane()
                            .is_some_and(|pane| pane.pane_id == target.pane_id)
                    {
                        let lines = preview_lines(&content);
                        if app.preview_lines != lines {
                            app.preview_lines = lines;
                            dirty = true;
                        }
                    }
                }
                Msg::PaneKilled { pane_id, err } => {
                    if let Some(err) = err {
                        app.err = Some(err);
                        app.restore_pending_kill(&pane_id);
                    } else {
                        spawn_load_panes(&tx);
                        panes_pending = true;
                    }
                    dirty = true;
                }
                Msg::ActionDone { err, leaves } => {
                    if err.is_none() && leaves {
                        app.save_state();
                        return Ok(());
                    }
                    if err.is_some() {
                        app.err = err;
                    }
                    spawn_load_panes(&tx);
                    panes_pending = true;
                    dirty = true;
                }
                Msg::DaemonChecked(err) => {
                    if app.daemon_err != err {
                        app.daemon_err = err;
                        dirty = true;
                    }
                }
                Msg::SubscriptionEnded => {
                    subscribed = false;
                    subscribe_pending = false;
                }
                Msg::Input(event) => {
                    let action = match event? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            dirty |= app.cancel_resize(surface);
                            app.handle_key(key, &tx)
                        }
                        Event::Mouse(mouse) => app.handle_mouse(surface, mouse),
                        Event::Resize(w, h) => {
                            surface.set_terminal_size(w, h);
                            app.resize(w, h);
                            Action::Redraw
                        }
                        Event::FocusLost => {
                            app.cancel_resize(surface);
                            Action::Redraw
                        }
                        _ => Action::None,
                    };
                    match action {
                        Action::Quit => return Ok(()),
                        Action::Redraw => dirty = true,
                        Action::None => {}
                    }
                }
            }
            // Give rendering a turn even while producers keep the queue nonempty.
            if batch_start.elapsed() >= FRAME_INTERVAL {
                break;
            }
        }

        if !subscribed && !subscribe_pending && last_subscribe.elapsed() >= Duration::from_secs(1) {
            spawn_subscribe_panes(&tx);
            subscribe_pending = true;
            last_subscribe = Instant::now();
        }

        if !subscribed && last_panes.elapsed() >= Duration::from_millis(500) && !panes_pending {
            spawn_load_panes(&tx);
            panes_pending = true;
            last_panes = Instant::now();
        }

        preview.select(app.current_pane().map(|pane| PreviewTarget {
            pane_id: pane.pane_id.clone(),
            session: pane.session.clone(),
            lines: app.height.max(50) as usize,
            generation: app.preview_gen,
        }));

        if let Some(since) = app.loading_since {
            dirty |= last_draw < since + LOADING_DELAY && since.elapsed() >= LOADING_DELAY;
        }
        if (dirty && last_draw.elapsed() >= FRAME_INTERVAL)
            || last_draw.elapsed() >= Duration::from_secs(1)
        {
            render(surface, app, writer)?;
            dirty = false;
            last_draw = Instant::now();
        }
        let wait = if dirty {
            FRAME_INTERVAL.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_millis(250)
        };
        pending = rx.recv_timeout(wait).ok();
    }
}

struct InputReader {
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputReader {
    fn start(tx: mpsc::Sender<Msg>) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stopped = std::sync::Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Ok(true) => {
                        if tx.send(Msg::Input(event::read())).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Msg::Input(Err(err)));
                        break;
                    }
                }
            }
        });
        Self {
            stopped,
            thread: Some(thread),
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn load_pane_state() -> (Option<Snapshot>, UiState) {
    match ipc::get_state() {
        Ok((Some(snapshot), ui_state)) => (Some(snapshot), ui_state),
        Ok((None, ui_state)) => (load_snapshot(), ui_state),
        Err(_) => (load_snapshot(), load_ui_state()),
    }
}

fn spawn_subscribe_panes(tx: &mpsc::Sender<Msg>) {
    let tx = tx.clone();
    thread::spawn(move || {
        let _ = subscribe_panes(&tx);
        let _ = tx.send(Msg::SubscriptionEnded);
    });
}

fn subscribe_panes(tx: &mpsc::Sender<Msg>) -> Result<()> {
    ipc::check_compatibility()?;
    let mut stream = UnixStream::connect(ipc::socket_path())?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = serde_json::to_string(&ipc::Request::SubscribeFresh)?;
    writeln!(stream, "{request}")?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        match serde_json::from_str::<ipc::Response>(&line)? {
            ipc::Response::State {
                snapshot: Some(snapshot),
                ui_state,
            } => {
                reader.get_ref().set_read_timeout(None)?;
                send_panes_loaded(tx, Some(snapshot), ui_state, true);
            }
            ipc::Response::Error { message } => anyhow::bail!(message),
            _ => anyhow::bail!("unexpected subscription response"),
        }
    }
    Ok(())
}

fn send_panes_loaded(
    tx: &mpsc::Sender<Msg>,
    snapshot: Option<Snapshot>,
    ui_state: UiState,
    live: bool,
) {
    let _ = tx.send(Msg::PanesLoaded {
        snapshot,
        ui_state,
        live,
    });
}

fn spawn_load_panes(tx: &mpsc::Sender<Msg>) {
    let tx = tx.clone();
    thread::spawn(move || {
        let (snapshot, ui_state) = load_pane_state();
        send_panes_loaded(&tx, snapshot, ui_state, false);
    });
}

/// Parses a captured pane. Captures keep trailing cells, so empty rows are padded rather than bare
/// newlines; the rows at the bottom that show nothing are dropped so the preview ends at the content.
fn preview_lines(content: &str) -> Vec<Line<'static>> {
    let mut lines = parse_ansi_lines(content.trim_end_matches('\n'));
    while lines.last().is_some_and(shows_nothing) {
        lines.pop();
    }
    lines
}

/// A row with no visible text, no background and no reverse video.
fn shows_nothing(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .all(|span| span.text.trim().is_empty() && span.style.bg.is_none() && !span.style.reverse)
}

fn ui_state_is_older_than(incoming: &UiState, current: &UiState) -> bool {
    match (incoming.updated_at, current.updated_at) {
        (Some(incoming), Some(current)) => incoming < current,
        (None, Some(_)) => true,
        _ => false,
    }
}

#[derive(Debug)]
enum Action {
    None,
    Redraw,
    Quit,
}

impl SortMode {
    fn as_str(self) -> &'static str {
        match self {
            SortMode::Order => "order",
            SortMode::Recent => "recent",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "order" => SortMode::Order,
            _ => SortMode::Recent,
        }
    }

    fn toggled(self) -> Self {
        match self {
            SortMode::Order => SortMode::Recent,
            SortMode::Recent => SortMode::Order,
        }
    }
}

struct App {
    panes: HashMap<String, Pane>,
    items: Vec<TreeItem>,
    cursor: usize,
    preview_lines: Vec<Line<'static>>,
    preview_gen: u64,
    snapshot_generation: u64,
    snapshot_revision: u64,
    project_win_width: HashMap<String, usize>,
    width: u16,
    height: u16,
    sidebar: Split,
    show_help: bool,
    pending_d: bool,
    pending_g: bool,
    sort_mode: SortMode,
    expanded_section: Option<Section>,
    sidebar_config: SidebarConfig,
    actions: Vec<UserAction>,
    picker: Option<Picker>,
    projects: Vec<CachedProject>,
    pending_forgotten: Vec<String>,
    count: usize,
    err: Option<String>,
    daemon_err: Option<String>,
    loading_since: Option<Instant>,
    ui_state: UiState,
    pending_unread_changes: HashMap<String, bool>,
    pending_kills: HashMap<String, Pane>,
    hits: HitRegistry<Hit>,
    _tmux_session: String,
}

fn sidebar_split(width: u16) -> Split {
    Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(width),
            minimum: [MIN_SIDEBAR, MIN_PREVIEW],
            resize_mode: SplitResizeMode::Cells,
            styles: Some(DividerStyles {
                normal: Style::new().fg(Color::DarkGrey),
                active: Style::new().fg(Color::Grey),
            }),
        },
    )
}

impl App {
    fn new(tmux_session: String, sidebar_config: SidebarConfig) -> Self {
        let snapshot = load_snapshot();
        let ui_state = load_ui_state();
        let mut app = Self {
            panes: HashMap::new(),
            items: Vec::new(),
            cursor: 0,
            preview_lines: Vec::new(),
            preview_gen: 1,
            snapshot_generation: 0,
            snapshot_revision: 0,
            project_win_width: HashMap::new(),
            width: 0,
            height: 0,
            sidebar: sidebar_split(ui_state.sidebar_width),
            show_help: false,
            pending_d: false,
            pending_g: false,
            sort_mode: SortMode::default(),
            expanded_section: None,
            sidebar_config,
            actions: Vec::new(),
            picker: None,
            projects: Vec::new(),
            pending_forgotten: Vec::new(),
            count: 0,
            err: None,
            daemon_err: None,
            loading_since: Some(Instant::now()),
            ui_state,
            pending_unread_changes: HashMap::new(),
            pending_kills: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: tmux_session,
        };
        app.focus_startup_session();
        if let Some(snapshot) = snapshot {
            app.apply_snapshot(snapshot, app.ui_state.clone());
        }
        app
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        if self.sidebar.preferred_size() == SplitSize::Cells(0) {
            self.sidebar
                .set_preferred_size(SplitSize::Cells((width / 4).max(MIN_SIDEBAR)));
        }
    }

    fn layout(&self) -> LayoutTree {
        LayoutTree::split(
            self.sidebar.clone(),
            LayoutTree::leaf(SIDEBAR),
            LayoutTree::leaf(PREVIEW),
        )
    }

    fn resolved_layout(&self) -> ResolvedLayout {
        self.layout()
            .resolve(Rect::new(0, 0, self.width, self.height), &NoopSizer)
    }

    fn sidebar_width(&self) -> u16 {
        let layout = self.resolved_layout();
        let available = layout
            .split(self.sidebar.id())
            .map_or(0, |split| split.geometry.available);
        self.sidebar.preferred_size().resolve(available)
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, ui_state: UiState) -> bool {
        if snapshot.revision < self.snapshot_revision {
            return false;
        }
        self.snapshot_revision = snapshot.revision;
        let ui_is_older = ui_state_is_older_than(&ui_state, &self.ui_state);
        let ui_changed = !ui_is_older && ui_state.updated_at != self.ui_state.updated_at;
        if !ui_is_older {
            self.ui_state = ui_state;
        }
        if snapshot.generation == self.snapshot_generation && !ui_changed {
            return false;
        }
        self.projects = snapshot.projects.clone();
        let mut panes = panes_from_snapshot(&snapshot);
        apply_ui_state(&mut panes, &self.ui_state);
        self.hide_pending_kills(&mut panes);
        self.snapshot_generation = snapshot.generation;
        self.replace_panes(panes);
        true
    }

    fn replace_panes(&mut self, panes: Vec<Pane>) {
        let selected = self.current_pane().map(|p| p.pane_id.to_string());
        let selected_project = self.current_project().map(|p| p.root.clone());
        self.panes = panes
            .into_iter()
            .map(|p| (p.pane_id.to_string(), p))
            .collect();
        if let Some(pane) = selected.as_ref().and_then(|id| self.panes.get(id)) {
            self.expanded_section = self.pane_section(pane);
        }
        if selected.is_none() && selected_project.is_none() {
            self.focus_startup_session();
        } else {
            self.rebuild_items();
            self.cursor = selected
                .and_then(|id| self.find_pane_by_id(&id))
                .or_else(|| selected_project.and_then(|root| self.find_project(&root)))
                .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
            self.collapse_unfocused_section();
            if self.current_pane().is_none() && self.current_project().is_none() {
                self.focus_first_session();
            }
        }
        if self.current_pane().is_none() {
            self.preview_lines.clear();
            self.preview_gen += 1;
        }
    }

    fn rebuild_items(&mut self) {
        let panes: Vec<&Pane> = self.panes.values().collect();
        let mut project_win_width: HashMap<String, usize> = HashMap::new();
        for p in &panes {
            if !p.project_root.is_empty() {
                let label = pane_label(p);
                let width = display_width(&label);
                project_win_width
                    .entry(p.project_root.clone())
                    .and_modify(|current| *current = (*current).max(width))
                    .or_insert(width);
            }
        }
        self.project_win_width = project_win_width;

        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        enum GroupKey {
            Project(String),
            Workspace(String),
        }

        struct Group<'a> {
            key: GroupKey,
            header_id: String,
            sort_order: usize,
            panes: Vec<&'a Pane>,
        }

        let mut items: Vec<TreeItem> = self
            .idle_projects()
            .map(|project| TreeItem::Project(project.root.clone()))
            .collect();
        for section in [None, Some(Section::Working), Some(Section::Stashed)] {
            if let Some(section) = section {
                let count = panes
                    .iter()
                    .filter(|p| self.pane_section(p) == Some(section))
                    .count();
                if count == 0 {
                    continue;
                }
                if !items.is_empty() {
                    items.push(TreeItem::SectionSpacer);
                }
                items.push(TreeItem::SectionHeader(section, count));
                if !self.section_is_expanded(section) {
                    continue;
                }
            }
            let mut groups: Vec<Group<'_>> = Vec::new();
            let mut group_index: HashMap<GroupKey, usize> = HashMap::new();
            for p in panes
                .iter()
                .copied()
                .filter(|p| self.pane_section(p) == section)
            {
                let key = if !p.project_root.is_empty() {
                    GroupKey::Project(p.project_root.clone())
                } else {
                    GroupKey::Workspace(p.path.clone())
                };
                if let Some(&idx) = group_index.get(&key) {
                    let group = &mut groups[idx];
                    if p.order < group.sort_order {
                        group.sort_order = p.order;
                        group.header_id = p.pane_id.to_string();
                    }
                    group.panes.push(p);
                } else {
                    group_index.insert(key.clone(), groups.len());
                    groups.push(Group {
                        key,
                        header_id: p.pane_id.to_string(),
                        sort_order: p.order,
                        panes: vec![p],
                    });
                }
            }

            let group_recency =
                |g: &Group<'_>| g.panes.iter().filter_map(|p| p.status_changed_at).max();
            match self.sort_mode {
                SortMode::Order => {
                    groups.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.key.cmp(&b.key)));
                }
                SortMode::Recent => {
                    groups.sort_by(|a, b| {
                        group_recency(b)
                            .cmp(&group_recency(a))
                            .then(a.key.cmp(&b.key))
                    });
                }
            }
            for mut group in groups {
                match self.sort_mode {
                    SortMode::Order => group
                        .panes
                        .sort_by(|a, b| a.order.cmp(&b.order).then(a.target.cmp(&b.target))),
                    SortMode::Recent => group.panes.sort_by(|a, b| {
                        b.status_changed_at
                            .cmp(&a.status_changed_at)
                            .then(a.order.cmp(&b.order))
                    }),
                }
                if section.is_none() && self.sidebar_config.working == WorkingSection::InlineLast {
                    group
                        .panes
                        .sort_by_key(|pane| pane.status == PaneStatus::Busy);
                }
                if matches!(&group.key, GroupKey::Project(_)) {
                    items.push(TreeItem::ProjectGroup(group.header_id));
                } else {
                    items.push(TreeItem::Workspace(group.header_id));
                }
                items.extend(
                    group
                        .panes
                        .into_iter()
                        .map(|p| TreeItem::Pane(p.pane_id.to_string())),
                );
            }
        }
        self.items = items;
    }

    fn current_pane(&self) -> Option<&Pane> {
        match self.items.get(self.cursor)? {
            TreeItem::Pane(id) => self.panes.get(id),
            _ => None,
        }
    }

    fn current_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = match self.items.get(self.cursor)? {
            TreeItem::Pane(id) => id.clone(),
            _ => return None,
        };
        self.panes.get_mut(&id)
    }

    fn current_project(&self) -> Option<&CachedProject> {
        match self.items.get(self.cursor)? {
            TreeItem::Project(root) => self.projects.iter().find(|p| &p.root == root),
            _ => None,
        }
    }

    fn find_project(&self, root: &str) -> Option<usize> {
        self.items
            .iter()
            .position(|it| matches!(it, TreeItem::Project(r) if r == root))
    }

    /// Known projects that no session runs in, in the daemon's order.
    fn idle_projects(&self) -> impl Iterator<Item = &CachedProject> {
        self.projects.iter().filter(|project| {
            self.ui_state.projects.contains(&project.root)
                && !self.pending_forgotten.contains(&project.root)
                && !self
                    .panes
                    .values()
                    .any(|pane| pane.project_root == project.root)
        })
    }

    /// Where actions run for the selected row: its own directory, or the
    /// project's main checkout when `at_root`.
    fn action_target(&self, at_root: bool) -> Option<actions::Target> {
        if let Some(pane) = self.current_pane() {
            let root = if pane.project_root.is_empty() {
                pane.path.clone()
            } else {
                pane.project_root.clone()
            };
            let (path, branch) = if at_root {
                (root.clone(), pane.project_branch.clone())
            } else {
                (pane.path.clone(), pane.git_branch.clone())
            };
            return Some(actions::Target {
                session: Some(pane.session.clone()),
                path,
                root,
                branch,
            });
        }
        let project = self.current_project()?;
        Some(actions::Target {
            session: None,
            path: project.root.clone(),
            root: project.root.clone(),
            branch: project.branch.clone(),
        })
    }

    /// Runs `action` on the selected row, after asking for a worktree when the
    /// action wants one and the project has several.
    fn run_action(&mut self, action: UserAction, tx: &mpsc::Sender<Msg>) -> Action {
        let Some(target) = self.action_target(action.is_project_level()) else {
            return Action::None;
        };
        if action.pick.is_some() {
            let worktrees = crate::agent::git::worktrees(&target.root);
            if worktrees.len() > 1 {
                self.picker = Some(Picker::new(action, target, worktrees));
                return Action::Redraw;
            }
        }
        self.spawn_action(action, target, tx);
        Action::None
    }

    fn handle_picker_key(&mut self, key: KeyEvent, tx: &mpsc::Sender<Msg>) -> Action {
        let Some(picker) = self.picker.as_mut() else {
            return Action::None;
        };
        match picker.handle_key(key) {
            Outcome::Pending => {}
            Outcome::Cancel => self.picker = None,
            Outcome::Choose(worktree) => {
                if let Some(picker) = self.picker.take() {
                    let (action, target) = picker.into_parts(&worktree);
                    self.spawn_action(action, target, tx);
                }
            }
        }
        Action::Redraw
    }

    fn spawn_action(&self, action: UserAction, target: actions::Target, tx: &mpsc::Sender<Msg>) {
        let tx = tx.clone();
        let leaves = action.leaves();
        thread::spawn(move || {
            let err = actions::run(&action, &target)
                .err()
                .map(|err| format!("{err:#}"));
            // Actions often change git state; refresh before the next draw. A
            // list that is about to close skips it and quits at once.
            if err.is_some() || !leaves {
                let _ = crate::agent::watch::refresh_once();
            }
            let _ = tx.send(Msg::ActionDone { err, leaves });
        });
    }

    fn forget_current_project(&mut self) -> bool {
        let Some(root) = self.current_project().map(|p| p.root.clone()) else {
            return false;
        };
        self.pending_forgotten.push(root);
        self.rebuild_items();
        self.select_cursor(nearest_pane(&self.items, self.cursor), false);
        self.preview_gen += 1;
        self.save_state();
        true
    }

    fn find_pane_by_id(&self, pane_id: &str) -> Option<usize> {
        self.items
            .iter()
            .position(|it| matches!(it, TreeItem::Pane(id) if id == pane_id))
    }

    fn focus_startup_session(&mut self) {
        self.sort_mode = self
            .sidebar_config
            .sort
            .unwrap_or_else(|| SortMode::from_str(&self.ui_state.sort_mode));
        self.expanded_section = None;
        self.rebuild_items();
        let selected = match self.sidebar_config.select {
            StartupSelection::Unread => self
                .items
                .iter()
                .find_map(|item| {
                    let TreeItem::Pane(id) = item else {
                        return None;
                    };
                    let pane = &self.panes[id];
                    (!pane.stashed && pane.status == PaneStatus::Unread).then(|| id.clone())
                })
                .unwrap_or_else(|| self.ui_state.selected_pane_id.clone()),
            StartupSelection::Last => self.ui_state.selected_pane_id.clone(),
            StartupSelection::First => String::new(),
        };
        if let Some(section) = self
            .panes
            .get(&selected)
            .and_then(|pane| self.pane_section(pane))
            && !self.section_is_expanded(section)
        {
            self.expanded_section = Some(section);
            self.rebuild_items();
        }
        if let Some(cursor) = self.find_pane_by_id(&selected) {
            self.cursor = cursor;
        } else {
            self.focus_first_session();
        }
    }

    fn focus_first_session(&mut self) {
        self.select_cursor(first_pane(&self.items).unwrap_or(0), false);
    }

    fn remove_current_pane(&mut self) -> Option<PaneId> {
        let pane = self.current_pane()?.clone();
        let pane_id = pane.pane_id.clone();
        let pane_key = pane_id.to_string();
        self.pending_unread_changes.remove(&pane_key);
        self.pending_kills.insert(pane_key.clone(), pane);
        self.panes.remove(&pane_key);
        self.rebuild_items();
        self.select_cursor(nearest_pane(&self.items, self.cursor), false);
        self.preview_lines.clear();
        self.preview_gen += 1;
        Some(pane_id)
    }

    fn restore_pending_kill(&mut self, pane_id: &str) {
        let Some(pane) = self.pending_kills.remove(pane_id) else {
            return;
        };
        self.expanded_section = self.pane_section(&pane);
        self.panes.insert(pane_id.to_string(), pane);
        self.rebuild_items();
        self.cursor = self
            .find_pane_by_id(pane_id)
            .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
        self.preview_gen += 1;
    }

    fn hide_pending_kills(&mut self, panes: &mut Vec<Pane>) {
        let alive: HashMap<String, bool> = panes
            .iter()
            .map(|pane| (pane.pane_id.to_string(), true))
            .collect();
        self.pending_kills.retain(|id, _| alive.contains_key(id));
        panes.retain(|pane| !self.pending_kills.contains_key(pane.pane_id.as_str()));
    }

    fn pane_section(&self, pane: &Pane) -> Option<Section> {
        if pane.stashed {
            Some(Section::Stashed)
        } else if (pane.status == PaneStatus::Busy
            && self.sidebar_config.working == WorkingSection::Section)
            || (pane.status.is_working()
                && self.sidebar_config.working == WorkingSection::Manual
                && pane.parked_since.is_some())
        {
            Some(Section::Working)
        } else {
            None
        }
    }

    fn section_is_always_expanded(&self, section: Section) -> bool {
        match section {
            Section::Working => self.sidebar_config.working_section == SectionBehavior::Expand,
            Section::Stashed => self.sidebar_config.stashed_section == SectionBehavior::Expand,
        }
    }

    fn section_is_expanded(&self, section: Section) -> bool {
        self.section_is_always_expanded(section) || self.expanded_section == Some(section)
    }

    fn collapse_section(&mut self, section: Section) -> bool {
        if self.section_is_always_expanded(section) || !self.section_is_expanded(section) {
            return false;
        }
        let destination = self
            .items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                TreeItem::Pane(id) if self.pane_section(&self.panes[id]) != Some(section) => {
                    Some(index)
                }
                TreeItem::SectionHeader(other, _) if *other != section => Some(index),
                _ => None,
            });
        if let Some(destination) = destination {
            self.select_cursor(destination, false);
            true
        } else {
            false
        }
    }

    fn select_cursor(&mut self, cursor: usize, backwards: bool) {
        self.cursor = cursor;
        // Skip expanded headers when navigating between sessions.
        if let Some(TreeItem::SectionHeader(section, _)) = self.items.get(self.cursor)
            && self.section_is_expanded(*section)
        {
            self.cursor = if backwards {
                prev_pane(&self.items, self.cursor)
            } else {
                next_pane(&self.items, self.cursor)
            };
        }
        if let Some(TreeItem::SectionHeader(section, _)) = self.items.get(self.cursor) {
            let section = *section;
            self.expanded_section = Some(section);
            self.rebuild_items();
            let in_section = |item: &TreeItem| matches!(item, TreeItem::Pane(id) if self.pane_section(&self.panes[id]) == Some(section));
            self.cursor = if backwards {
                self.items.iter().rposition(in_section)
            } else {
                self.items.iter().position(in_section)
            }
            .unwrap_or(self.cursor);
        }
        self.collapse_unfocused_section();
    }

    fn collapse_unfocused_section(&mut self) {
        let section = self.current_pane().and_then(|pane| self.pane_section(pane));
        if let Some(expanded) = self.expanded_section
            && section != Some(expanded)
        {
            self.expanded_section = None;
            if self.section_is_expanded(expanded) {
                return;
            }
            let selected = self.current_pane().map(|p| p.pane_id.to_string());
            self.rebuild_items();
            self.cursor = selected
                .and_then(|id| self.find_pane_by_id(&id))
                .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
        }
    }

    fn toggle_current_parking(&mut self) -> Action {
        if self.sidebar_config.working != WorkingSection::Manual {
            return Action::None;
        }
        let previous_cursor = self.cursor;
        let Some(pane) = self.current_pane_mut() else {
            return Action::None;
        };
        if pane.stashed || !pane.status.is_working() {
            return Action::None;
        }
        let parked = pane.parked_since.is_none();
        if parked && pane.park_epoch.is_none() {
            return Action::None;
        }
        pane.parked_since = if parked { pane.park_epoch } else { None };
        let id = pane.pane_id.to_string();
        self.rebuild_items();
        self.cursor = if parked {
            nearest_pane(&self.items, previous_cursor)
        } else {
            self.find_pane_by_id(&id)
                .unwrap_or_else(|| nearest_pane(&self.items, previous_cursor))
        };
        self.select_cursor(self.cursor, false);
        self.preview_gen += 1;
        self.save_state();
        Action::Redraw
    }

    fn toggle_current_stash(&mut self) -> Option<bool> {
        let previous_cursor = self.cursor;
        let (pane_id, stashed) = {
            let pane = self.current_pane_mut()?;
            pane.stashed = !pane.stashed;
            pane.parked_since = None;
            (pane.pane_id.to_string(), pane.stashed)
        };
        if !stashed {
            self.expanded_section = self.current_pane().and_then(|pane| self.pane_section(pane));
        }
        self.rebuild_items();
        self.cursor = if stashed {
            nearest_pane(&self.items, previous_cursor)
        } else {
            self.find_pane_by_id(&pane_id)
                .unwrap_or_else(|| nearest_pane(&self.items, previous_cursor))
        };
        self.select_cursor(self.cursor, false);
        Some(stashed)
    }

    fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::Sender<Msg>) -> Action {
        if self.picker.is_some() {
            return self.handle_picker_key(key, tx);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Emacs navigation: alias C-n/C-p to down/up so they flow through the
        // existing vim handling below.
        let key = match key.code {
            KeyCode::Char('n') if ctrl => KeyEvent::from(KeyCode::Down),
            KeyCode::Char('p') if ctrl => KeyEvent::from(KeyCode::Up),
            _ => key,
        };
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Char('q')
            || (ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')))
        {
            self.save_state();
            return Action::Quit;
        }
        if let KeyCode::Char(ch) = key.code
            && ch.is_ascii_digit()
            && (self.count > 0 || ch != '0')
        {
            self.count = self
                .count
                .saturating_mul(10)
                .saturating_add((ch as u8 - b'0') as usize);
            return Action::None;
        }
        let count = self.count.max(1);
        self.count = 0;

        // Emacs go-to-top / go-to-bottom: M-< and M->.
        if alt && matches!(key.code, KeyCode::Char('<')) {
            self.pending_g = false;
            self.select_cursor(first_pane(&self.items).unwrap_or(0), false);
            self.preview_gen += 1;
            return Action::Redraw;
        }
        if alt && matches!(key.code, KeyCode::Char('>')) {
            self.pending_g = false;
            self.select_cursor(last_pane(&self.items).unwrap_or(0), true);
            self.preview_gen += 1;
            return Action::Redraw;
        }

        if key.code == KeyCode::Char('d') {
            if self.pending_d {
                self.pending_d = false;
                self.pending_g = false;
                if self.forget_current_project() {
                    return Action::Redraw;
                }
                if let Some(pane_id) = self.remove_current_pane() {
                    let tx = tx.clone();
                    thread::spawn(move || {
                        let err = kill_pane(&pane_id).err().map(|e| e.to_string());
                        let _ = tx.send(Msg::PaneKilled {
                            pane_id: pane_id.to_string(),
                            err,
                        });
                    });
                    return Action::Redraw;
                }
                return Action::None;
            }
            self.pending_d = true;
            self.pending_g = false;
            return Action::None;
        }
        self.pending_d = false;

        if key.code == KeyCode::Char('g') {
            if self.pending_g {
                self.pending_g = false;
                self.select_cursor(first_pane(&self.items).unwrap_or(0), false);
                self.preview_gen += 1;
                return Action::Redraw;
            }
            self.pending_g = true;
            return Action::None;
        }
        self.pending_g = false;

        match key.code {
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                Action::Redraw
            }
            KeyCode::Char('G') => {
                self.select_cursor(last_pane(&self.items).unwrap_or(0), true);
                self.preview_gen += 1;
                Action::Redraw
            }
            KeyCode::Char(' ') => {
                let mut changed = None;
                if let Some(p) = self.current_pane_mut() {
                    let unread = match p.status {
                        PaneStatus::Idle => true,
                        PaneStatus::Unread => false,
                        PaneStatus::Busy | PaneStatus::Waiting => return Action::None,
                    };
                    p.status = if unread {
                        PaneStatus::Unread
                    } else {
                        PaneStatus::Idle
                    };
                    changed = Some((p.pane_id.to_string(), unread));
                }
                if let Some((id, unread)) = changed {
                    self.pending_unread_changes.insert(id, unread);
                    self.save_state();
                }
                Action::Redraw
            }
            KeyCode::Char('R') => {
                let mut changed = Vec::new();
                for p in self.panes.values_mut() {
                    if p.status == PaneStatus::Unread {
                        p.status = PaneStatus::Idle;
                        changed.push(p.pane_id.to_string());
                    }
                }
                if changed.is_empty() {
                    return Action::None;
                }
                for id in changed {
                    self.pending_unread_changes.insert(id, false);
                }
                self.save_state();
                Action::Redraw
            }
            KeyCode::Char('s') => {
                if let Some(stashed) = self.toggle_current_stash() {
                    if stashed {
                        self.preview_gen += 1;
                    }
                    self.save_state();
                    return Action::Redraw;
                }
                Action::None
            }
            KeyCode::Char('u') => {
                if self.current_pane().is_some_and(|pane| pane.stashed) {
                    self.toggle_current_stash();
                    self.save_state();
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            KeyCode::Char('o') => {
                let selected = self.current_pane().map(|p| p.pane_id.to_string());
                self.sort_mode = self.sort_mode.toggled();
                self.rebuild_items();
                self.cursor = selected
                    .and_then(|id| self.find_pane_by_id(&id))
                    .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
                self.preview_gen += 1;
                self.save_state();
                Action::Redraw
            }
            KeyCode::Char('H' | 'L') => {
                let delta = i32::try_from(count).unwrap_or(i32::MAX).saturating_mul(2);
                let delta = if key.code == KeyCode::Char('H') {
                    -delta
                } else {
                    delta
                };
                if let Some(split) = self.resolved_layout().split(self.sidebar.id())
                    && split.resize(SplitPane::First, delta)
                {
                    self.save_state();
                }
                Action::Redraw
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let section = self.current_pane().and_then(|pane| self.pane_section(pane));
                if let Some(section) = section
                    && self.collapse_section(section)
                {
                    self.preview_gen += 1;
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                for _ in 0..count {
                    let next = next_pane(&self.items, self.cursor);
                    let previous = self.cursor;
                    self.select_cursor(next, false);
                    if next == previous {
                        break;
                    }
                }
                self.preview_gen += 1;
                Action::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                for _ in 0..count {
                    let prev = prev_pane(&self.items, self.cursor);
                    let previous = self.cursor;
                    self.select_cursor(prev, true);
                    if prev == previous {
                        break;
                    }
                }
                self.preview_gen += 1;
                Action::Redraw
            }
            KeyCode::Enter if ctrl => self.toggle_current_parking(),
            KeyCode::Char('w') => self.toggle_current_parking(),
            KeyCode::Enter if self.current_project().is_some() => self.open_new_agent(tx),
            KeyCode::Enter => self.open_current_pane(),
            _ => match actions::find(&self.actions, &key).cloned() {
                Some(action) => self.run_action(action, tx),
                None => Action::None,
            },
        }
    }

    /// Runs the action bound to `a`, which opens an agent in the selected project.
    fn open_new_agent(&mut self, tx: &mpsc::Sender<Msg>) -> Action {
        let key = KeyEvent::from(KeyCode::Char('a'));
        match actions::find(&self.actions, &key).cloned() {
            Some(action) => self.run_action(action, tx),
            None => Action::None,
        }
    }

    /// Read the pane under the cursor (if unread), switch tmux to it, and quit.
    fn open_current_pane(&mut self) -> Action {
        if matches!(
            self.items.get(self.cursor),
            Some(TreeItem::SectionHeader(_, _))
        ) {
            self.select_cursor(self.cursor, false);
            self.preview_gen += 1;
            return Action::Redraw;
        }
        if let Some(p) = self.current_pane() {
            let pane_id = p.pane_id.clone();
            let unread = p.status == PaneStatus::Unread;
            if let Err(err) = switch_to_pane(&pane_id) {
                self.err = Some(format!("{err:#}"));
                return Action::Redraw;
            }
            // Opening a pane reads it, even if it was force-marked unread,
            // consistent with Space/`A`, which clear a manual Unread too.
            if unread {
                self.pending_unread_changes
                    .insert(pane_id.to_string(), false);
            }
        }
        self.save_state();
        Action::Quit
    }

    fn handle_mouse(&mut self, surface: &mut Surface, mouse: MouseEvent) -> Action {
        if let Some(response) = surface.handle_mouse(mouse) {
            self.apply_split_response(&response);
            return Action::Redraw;
        }
        if self.picker.is_some() {
            return Action::None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && surface.active_split().is_none()
            && let Some(hit) = self.hits.hit(mouse.column, mouse.row).cloned()
        {
            let (idx, section) = match hit {
                Hit::Pane(id) => (self.find_pane_by_id(&id), None),
                Hit::Project(root) => {
                    if let Some(idx) = self.find_project(&root) {
                        self.select_cursor(idx, false);
                        self.preview_gen += 1;
                        return Action::Redraw;
                    }
                    return Action::None;
                }
                Hit::Section(section) => (
                    self.items.iter().position(
                        |item| matches!(item, TreeItem::SectionHeader(s, _) if *s == section),
                    ),
                    Some(section),
                ),
            };
            let Some(idx) = idx else { return Action::None };
            if let Some(section) = section {
                if !self.section_is_expanded(section) || !self.collapse_section(section) {
                    self.select_cursor(idx, false);
                }
                self.preview_gen += 1;
                return Action::Redraw;
            }
            self.select_cursor(idx, false);
            return self.open_current_pane();
        }
        Action::None
    }

    fn apply_split_response(&mut self, response: &SplitResponse) {
        if response.ended() && response.changed {
            self.save_state();
        }
    }

    /// Ends a divider drag in progress. Returns whether one was active.
    fn cancel_resize(&mut self, surface: &mut Surface) -> bool {
        let Some(response) = surface.cancel_split_drag() else {
            return false;
        };
        if response.changed {
            self.save_state();
        }
        true
    }

    fn update_saved_state(&self, state: &mut UiState) {
        for p in self.panes.values() {
            if !state.panes.contains_key(p.pane_id.as_str())
                && let Some(ui) = state.panes.remove(&p.target)
            {
                state.panes.insert(p.pane_id.to_string(), ui);
            }
        }
        state.panes.retain(|id, _| self.panes.contains_key(id));
        for p in self.panes.values() {
            let entry = state.panes.entry(p.pane_id.to_string()).or_default();
            entry.stashed = p.stashed;
            entry.parked_since = p.parked_since;
            if let Some(unread) = self.pending_unread_changes.get(p.pane_id.as_str()) {
                entry.forced_unread = *unread;
                entry.read_content_hash = (!*unread).then(|| p.content_hash.clone());
            }
        }
        state.panes.retain(|_, ui| !ui_pane_state_is_empty(ui));
        state.selected_pane_id = self
            .current_pane()
            .map(|pane| pane.pane_id.to_string())
            .unwrap_or_default();
        state.sidebar_width = self.sidebar_width();
        state
            .projects
            .retain(|root| !self.pending_forgotten.contains(root));
        state.sort_mode = self.sort_mode.as_str().to_string();
    }

    #[cfg(not(test))]
    fn save_state(&mut self) {
        if let Ok(state) = update_ui_state(|state| self.update_saved_state(state)) {
            self.ui_state = state;
            self.pending_unread_changes.clear();
            self.pending_forgotten.clear();
        }
    }

    #[cfg(test)]
    fn save_state(&mut self) {
        let mut state = std::mem::take(&mut self.ui_state);
        self.update_saved_state(&mut state);
        self.ui_state = state;
        self.pending_unread_changes.clear();
        self.pending_forgotten.clear();
    }
}

fn render<W: Write>(surface: &mut Surface, app: &mut App, out: &mut W) -> io::Result<()> {
    app.hits.clear();
    surface.set_layout(app.layout());
    surface.render(out, |id, slice| {
        if id == SIDEBAR {
            render_sidebar(slice, app);
        } else if id == PREVIEW {
            render_preview(slice, app);
        }
    })?;
    Ok(())
}

fn render_sidebar(slice: &mut GridSlice<'_>, app: &mut App) {
    let err = app.daemon_err.as_ref().or(app.err.as_ref());
    let status_height = if err.is_some() {
        slice.height().saturating_sub(1).min(3)
    } else {
        0
    };
    let list_height = slice.height().saturating_sub(status_height);
    if let Some(err) = err {
        let message = format!("Error: {err}");
        let style = Style::new().fg(Color::Red);
        let mut remaining = message.as_str();
        for row in list_height..slice.height() {
            let line = smelt_term::grid::truncate_width(remaining, slice.width());
            if line.is_empty() {
                break;
            }
            slice.put_str(0, row, &line, style);
            remaining = &remaining[line.len()..];
        }
    }
    if app.items.is_empty() {
        if list_height > 1 && app.loading_since.is_none() {
            slice.put_str(2, 1, "No active sessions", Style::new().fg(Color::DarkGrey));
        }
        return;
    }
    let h = list_height as usize;
    let start = visible_start(app.items.len(), app.cursor, h);
    let end = (start + h).min(app.items.len());
    let origin = slice.grid_rect();
    let width = slice.width();
    for (row, idx) in (start..end).enumerate() {
        render_tree_item(
            slice,
            row as u16,
            width,
            &app.items[idx],
            idx == app.cursor,
            app,
        );
        let hit = match &app.items[idx] {
            TreeItem::Pane(id) => Some(Hit::Pane(id.clone())),
            TreeItem::Project(root) => Some(Hit::Project(root.clone())),
            TreeItem::SectionHeader(section, _) => Some(Hit::Section(*section)),
            _ => None,
        };
        if let Some(hit) = hit {
            app.hits
                .record_local(origin, Rect::new(0, row as u16, width, 1), hit);
        }
    }
}

fn render_tree_item(
    slice: &mut GridSlice<'_>,
    row: u16,
    width: u16,
    item: &TreeItem,
    selected: bool,
    app: &App,
) {
    match item {
        TreeItem::SectionHeader(section, count) => {
            let text = format!("─ {} ({count}) ", section.title());
            let fill = width.saturating_sub(display_width(&text) as u16);
            let style = if selected {
                Style::new().fg(Color::White).bg(Color::DarkGrey).bold()
            } else {
                Style::new().fg(Color::AnsiValue(242)).dim()
            };
            slice.fill_row(row, style);
            slice.put_str(
                0,
                row,
                &format!("{text}{}", "─".repeat(fill as usize)),
                style,
            );
        }
        TreeItem::SectionSpacer => {}
        TreeItem::Workspace(id) => {
            if let Some(p) = app.panes.get(id) {
                render_header_row(
                    slice,
                    row,
                    width,
                    HeaderRow {
                        name: &p.short_path,
                        branch: &p.git_branch,
                        dirty: p.git_dirty,
                        style: if p.stashed {
                            Style::new().fg(Color::DarkGrey)
                        } else {
                            Style::new().fg(Color::White).bold()
                        },
                        branch_style: if p.stashed {
                            Style::new().fg(Color::AnsiValue(242))
                        } else {
                            Style::new().fg(Color::Green)
                        },
                    },
                );
            }
        }
        TreeItem::ProjectGroup(id) => {
            if let Some(p) = app.panes.get(id) {
                let name = if p.project_short.is_empty() {
                    &p.short_path
                } else {
                    &p.project_short
                };
                render_header_row(
                    slice,
                    row,
                    width,
                    HeaderRow {
                        name,
                        branch: &p.project_branch,
                        dirty: p.project_dirty,
                        style: if p.stashed {
                            Style::new().fg(Color::DarkGrey)
                        } else {
                            Style::new().fg(Color::White).bold()
                        },
                        branch_style: if p.stashed {
                            Style::new().fg(Color::AnsiValue(242))
                        } else {
                            Style::new().fg(Color::Green)
                        },
                    },
                );
            }
        }
        TreeItem::Pane(id) => {
            if let Some(p) = app.panes.get(id) {
                render_pane_row(slice, row, width, p, selected, app);
            }
        }
        TreeItem::Project(root) => {
            if let Some(project) = app.projects.iter().find(|p| &p.root == root) {
                render_project_row(slice, row, width, project, selected);
            }
        }
    }
}

fn render_project_row(
    slice: &mut GridSlice<'_>,
    row: u16,
    width: u16,
    project: &CachedProject,
    selected: bool,
) {
    let mut style = Style::new().fg(Color::AnsiValue(250));
    if selected {
        style = style.bg(Color::DarkGrey).bold();
    }
    let branch_style = if selected {
        Style::new().fg(Color::Green).bg(Color::DarkGrey)
    } else {
        Style::new().fg(Color::Green)
    };
    let mut branch = project.branch.clone();
    if project.dirty {
        branch.push('*');
    }
    let badge = distance_badge(project);
    let right = [branch, badge].join(if project.ahead + project.behind > 0 {
        " "
    } else {
        ""
    });
    let name_avail = (width as usize).saturating_sub(display_width(&right) + 3);
    let name = truncate_width(&project.short, name_avail.max(1));
    slice.fill_row(row, style);
    let mut col = slice.put_str(0, row, " ", style);
    col = slice.put_str(col, row, &name, style);
    let pad = width
        .saturating_sub(col)
        .saturating_sub(display_width(&right) as u16)
        .saturating_sub(1);
    if display_width(&right) < width as usize {
        col = slice.put_padded(col, row, pad, "", style);
        slice.put_str(col, row, &right, branch_style);
    }
}

fn distance_badge(project: &CachedProject) -> String {
    let mut badge = String::new();
    if project.ahead > 0 {
        badge.push_str(&format!("↑{}", project.ahead));
    }
    if project.behind > 0 {
        badge.push_str(&format!("↓{}", project.behind));
    }
    badge
}

struct HeaderRow<'a> {
    name: &'a str,
    branch: &'a str,
    dirty: bool,
    style: Style,
    branch_style: Style,
}

fn render_header_row(slice: &mut GridSlice<'_>, row: u16, width: u16, header: HeaderRow<'_>) {
    let HeaderRow {
        name,
        branch,
        dirty,
        style,
        branch_style,
    } = header;
    let avail = width.saturating_sub(2) as usize;
    let mut branch = branch.to_string();
    if !branch.is_empty() && dirty {
        branch.push('*');
    }
    let mut name = name.to_string();
    if !branch.is_empty() {
        let needed = display_width(&name) + 1 + display_width(&branch);
        if needed > avail {
            let branch_avail = avail.saturating_sub(display_width(&name) + 1);
            if branch_avail >= 4 {
                branch = truncate_width(&branch, branch_avail);
            } else {
                branch.clear();
            }
        }
    }
    if branch.is_empty() {
        name = truncate_width(&name, avail);
    }
    let mut col = slice.put_str(0, row, " ", style);
    col = slice.put_str(col, row, &name, style);
    if !branch.is_empty() {
        let pad = width
            .saturating_sub(col)
            .saturating_sub(display_width(&branch) as u16)
            .saturating_sub(1);
        col = slice.put_padded(col, row, pad, "", style);
        col = slice.put_str(col, row, &branch, branch_style);
        let _ = slice.put_str(col, row, " ", branch_style);
    } else {
        slice.put_padded(col, row, width.saturating_sub(col), "", style);
    }
}

fn render_pane_row(
    slice: &mut GridSlice<'_>,
    row: u16,
    width: u16,
    p: &Pane,
    selected: bool,
    app: &App,
) {
    const PREFIX: &str = "   ";
    const ELAPSED_SLOT_W: usize = 5;

    let selected_style = Style::new().fg(Color::White).bg(Color::DarkGrey).bold();
    let stashed_style = Style::new().fg(Color::DarkGrey);
    let normal_dim = Style::new().fg(Color::DarkGrey);
    let fill_style = if selected {
        selected_style
    } else if p.stashed {
        stashed_style
    } else {
        Style::default()
    };
    slice.fill_row(row, fill_style);

    let mut win_label = pane_label(p);
    let mut worktree = if !p.short_path.is_empty() && p.path != p.project_root {
        p.short_path.clone()
    } else {
        String::new()
    };

    let mut elapsed = String::new();
    if p.status != PaneStatus::Busy {
        elapsed = elapsed_label(p);
        if !elapsed.is_empty() {
            elapsed = format!(" {elapsed} ");
            if display_width(&elapsed) > ELAPSED_SLOT_W {
                elapsed = truncate_width(&elapsed, ELAPSED_SLOT_W);
            }
            let pad = ELAPSED_SLOT_W.saturating_sub(display_width(&elapsed));
            elapsed = format!("{}{elapsed}", " ".repeat(pad));
        }
    }
    if elapsed.is_empty() {
        elapsed = " ".repeat(ELAPSED_SLOT_W);
    }

    let prefix_w = display_width(PREFIX);
    let middle_avail = (width as usize)
        .saturating_sub(prefix_w)
        .saturating_sub(2)
        .saturating_sub(ELAPSED_SLOT_W);
    if display_width(&win_label) > middle_avail {
        win_label = truncate_width(&win_label, middle_avail);
    }
    let remaining = middle_avail.saturating_sub(display_width(&win_label));

    let mut sep_w = 2usize;
    if let Some(target_w) = app.project_win_width.get(&p.project_root)
        && *target_w > display_width(&win_label)
    {
        let aligned = 2 + *target_w - display_width(&win_label);
        if remaining >= aligned + 2 {
            sep_w = aligned;
        }
    }

    let mut worktree_rendered = String::new();
    if !worktree.is_empty() && remaining >= sep_w + 2 {
        let avail = remaining - sep_w;
        if display_width(&worktree) > avail {
            worktree = truncate_width(&worktree, avail);
        }
        worktree_rendered = format!("{}{}", " ".repeat(sep_w), worktree);
    }
    let gap = remaining.saturating_sub(display_width(&worktree_rendered));

    let icon_color = if p.stashed && !selected {
        Color::AnsiValue(242)
    } else {
        match p.status {
            PaneStatus::Busy | PaneStatus::Waiting => Color::Rgb {
                r: 217,
                g: 119,
                b: 6,
            },
            PaneStatus::Unread => Color::Rgb {
                r: 155,
                g: 155,
                b: 245,
            },
            PaneStatus::Idle if selected => Color::White,
            PaneStatus::Idle => Color::DarkGrey,
        }
    };
    let icon = if p.completed {
        '✓'
    } else if matches!(p.status, PaneStatus::Idle | PaneStatus::Waiting) {
        '○'
    } else {
        '●'
    };

    let text_style = if selected {
        selected_style
    } else if p.stashed {
        stashed_style
    } else {
        provider_style(&p.provider)
    };
    let dim_style = if selected {
        selected_style
    } else if p.stashed {
        Style::new().fg(Color::AnsiValue(242))
    } else {
        normal_dim
    };

    let mut col = 0;
    col = slice.put_str(
        col,
        row,
        PREFIX,
        if selected { selected_style } else { dim_style },
    );
    slice.set(col, row, icon, fill_style.fg(icon_color));
    col += 1;
    col = slice.put_str(col, row, " ", fill_style);
    col = slice.put_str(col, row, &win_label, text_style);
    if !worktree_rendered.is_empty() {
        col = slice.put_str(col, row, &worktree_rendered, dim_style);
    }
    col = slice.put_str(col, row, &" ".repeat(gap), dim_style);
    let _ = slice.put_str(col, row, &elapsed, dim_style);
}

fn pane_label(p: &Pane) -> String {
    let mut label = if p.window_name.is_empty() {
        format!("{}:{}", p.session, p.window)
    } else {
        format!("{}:{}", p.window, p.window_name)
    };
    if !p.pane.is_empty() {
        label.push('.');
        label.push_str(&p.pane);
    }
    label
}

fn render_preview(slice: &mut GridSlice<'_>, app: &App) {
    if let Some(picker) = &app.picker {
        picker.render(slice);
        return;
    }
    if app.show_help {
        render_help(slice, app);
        return;
    }
    if let Some(project) = app.current_project() {
        render_project_preview(slice, app, project);
        return;
    }
    if app.current_pane().is_none() {
        render_empty_preview(slice, app);
        return;
    }
    if app.preview_lines.is_empty() {
        slice.put_str(1, 1, "loading preview…", Style::new().fg(Color::DarkGrey));
        return;
    }
    let h = slice.height() as usize;
    let start = app.preview_lines.len().saturating_sub(h);
    for (row, line) in app.preview_lines.iter().skip(start).take(h).enumerate() {
        slice.put_line(0, row as u16, line);
    }
}

fn render_project_preview(slice: &mut GridSlice<'_>, app: &App, project: &CachedProject) {
    let title = Style::new().fg(Color::White).bold();
    let dim = Style::new().fg(Color::DarkGrey);
    let key = Style::new().fg(Color::Yellow).bold();
    slice.put_str(2, 1, &project.short, title);
    slice.put_str(2, 2, &project.root, dim);
    let mut state = vec![format!("branch {}", project.branch)];
    if project.dirty {
        state.push("uncommitted changes".to_string());
    }
    if project.ahead > 0 {
        state.push(format!("{} to push", project.ahead));
    }
    if project.behind > 0 {
        state.push(format!("{} to pull", project.behind));
    }
    slice.put_str(2, 4, &state.join("  "), Style::new().fg(Color::Green));
    let mut row = 6;
    let mut line = |label: &str, text: &str, key_style| {
        slice.put_str(2, row, &format!("{label:<8}"), key_style);
        slice.put_str(12, row, text, dim);
        row += 1;
    };
    line("dd", "forget project", key);
    for action in &app.actions {
        line(&action.key_label(), &action.description, key);
    }
}

fn render_empty_preview(slice: &mut GridSlice<'_>, app: &App) {
    if app.panes.is_empty()
        && let Some(since) = app.loading_since
    {
        if since.elapsed() >= LOADING_DELAY {
            slice.put_str(
                2,
                1,
                "Loading sessions...",
                Style::new().fg(Color::DarkGrey),
            );
        }
        return;
    }
    let title = if !app.panes.is_empty() {
        "Select a session"
    } else {
        "No active sessions"
    };
    let detail = if app.panes.is_empty() {
        "Start a supported agent in tmux and it will appear here."
    } else {
        "Move into a section or press Enter to expand it."
    };
    slice.put_str(2, 1, title, Style::new().fg(Color::White).bold());
    slice.put_str(2, 3, detail, Style::new().fg(Color::DarkGrey));
}

fn render_help(slice: &mut GridSlice<'_>, app: &App) {
    let title = Style::new().fg(Color::White).bold();
    let key = Style::new().fg(Color::Yellow).bold();
    let dim = Style::new().fg(Color::DarkGrey);
    slice.put_str(2, 1, "Keybindings", title);
    let builtins = keys::BUILTINS.iter().map(|b| (b.label.to_string(), b.help));
    let actions = app
        .actions
        .iter()
        .map(|a| (a.key_label(), a.description.as_str()));
    for (i, (label, help)) in builtins.chain(actions).enumerate() {
        // Action rows follow the built-in ones after a blank row.
        let gap = u16::from(i >= keys::BUILTINS.len());
        let y = i as u16 + 3 + gap;
        slice.put_str(2, y, &format!("{label:<8}"), key);
        slice.put_str(12, y, help, dim);
    }
}

fn provider_style(provider: &str) -> Style {
    let color = match provider {
        "claude" => Color::Rgb {
            r: 217,
            g: 119,
            b: 6,
        },
        "codex" => Color::Rgb {
            r: 209,
            g: 213,
            b: 219,
        },
        "gemini" => Color::Rgb {
            r: 16,
            g: 185,
            b: 129,
        },
        "kimi" => Color::Rgb {
            r: 0,
            g: 119,
            b: 182,
        },
        "opencode" => Color::Rgb {
            r: 6,
            g: 182,
            b: 212,
        },
        "smelt" => Color::Rgb {
            r: 234,
            g: 179,
            b: 8,
        },
        _ => Color::DarkGrey,
    };
    Style::new().fg(color)
}

fn elapsed_label(p: &Pane) -> String {
    if p.status == PaneStatus::Busy {
        return String::new();
    }
    let Some(t) = p.last_active else {
        return String::new();
    };
    let secs = (chrono::Utc::now() - t).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn display_width(s: &str) -> usize {
    usize::from(smelt_term::display_width(s))
}

fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let limit = u16::try_from(max.saturating_sub(1)).unwrap_or(u16::MAX);
    let mut out = smelt_term::truncate_width(s, limit);
    if max >= 1 {
        out.push('…');
    }
    out
}

fn visible_start(len: usize, cursor: usize, height: usize) -> usize {
    if len <= height || cursor < height / 2 {
        0
    } else if cursor + height / 2 >= len {
        len - height
    } else {
        cursor - height / 2
    }
}

fn selectable(item: &TreeItem) -> bool {
    matches!(
        item,
        TreeItem::Pane(_) | TreeItem::Project(_) | TreeItem::SectionHeader(_, _)
    )
}

fn first_pane(items: &[TreeItem]) -> Option<usize> {
    items.iter().position(selectable)
}

fn last_pane(items: &[TreeItem]) -> Option<usize> {
    items.iter().rposition(selectable)
}

fn next_pane(items: &[TreeItem], from: usize) -> usize {
    for (i, item) in items.iter().enumerate().skip(from + 1) {
        if selectable(item) {
            return i;
        }
    }
    for (i, item) in items.iter().enumerate().take(from.min(items.len())) {
        if selectable(item) {
            return i;
        }
    }
    from
}

fn prev_pane(items: &[TreeItem], from: usize) -> usize {
    for i in (0..from).rev() {
        if selectable(&items[i]) {
            return i;
        }
    }
    for i in ((from + 1)..items.len()).rev() {
        if selectable(&items[i]) {
            return i;
        }
    }
    from
}

fn nearest_pane(items: &[TreeItem], from: usize) -> usize {
    if items.is_empty() {
        return 0;
    }
    let from = from.min(items.len() - 1);
    let session = |item: &TreeItem| matches!(item, TreeItem::Pane(_) | TreeItem::Project(_));
    if session(&items[from]) {
        return from;
    }
    for offset in 1..items.len() {
        if from >= offset && session(&items[from - offset]) {
            return from - offset;
        }
        if from + offset < items.len() && session(&items[from + offset]) {
            return from + offset;
        }
    }
    first_pane(items).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keeps_erase_painted_rows_and_drops_empty_trailing_ones() {
        let painted = "\x1b[48;2;10;10;10mab    \n      \n\x1b[0m\n      \n";
        let lines = preview_lines(painted);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].width(), 6);
        assert!(lines[1].spans.iter().all(|s| s.style.bg.is_some()));
    }

    #[test]
    fn preview_drops_blank_rows_with_the_default_background() {
        assert_eq!(preview_lines("one\ntwo\n    \n    \n").len(), 2);
        assert!(preview_lines("   \n   ").is_empty());
    }

    fn pane(id: &str, order: usize) -> Pane {
        Pane {
            target: format!("session:1.{order}"),
            path: "/workspace".to_string(),
            order,
            ..Pane::new(PaneId::parse(id).unwrap())
        }
    }

    fn app_with_panes(panes: Vec<Pane>) -> App {
        let mut app = App {
            panes: panes
                .into_iter()
                .map(|pane| (pane.pane_id.to_string(), pane))
                .collect(),
            items: Vec::new(),
            cursor: 0,
            preview_lines: Vec::new(),
            preview_gen: 0,
            snapshot_generation: 0,
            snapshot_revision: 0,
            project_win_width: HashMap::new(),
            width: 0,
            height: 0,
            sidebar: sidebar_split(0),
            show_help: false,
            pending_d: false,
            pending_g: false,
            sort_mode: SortMode::default(),
            expanded_section: None,
            sidebar_config: SidebarConfig {
                working: WorkingSection::Section,
                ..SidebarConfig::default()
            },
            picker: None,
            actions: Vec::new(),
            projects: Vec::new(),
            pending_forgotten: Vec::new(),
            count: 0,
            err: None,
            daemon_err: None,
            loading_since: None,
            ui_state: UiState::default(),
            pending_unread_changes: HashMap::new(),
            pending_kills: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: String::new(),
        };
        app.focus_startup_session();
        app
    }

    fn snapshot_with_pane(revision: u64, generation: u64, pane: Pane) -> Snapshot {
        let mut cached = crate::agent::persist::cache_panes(std::slice::from_ref(&pane));
        cached[0].last_status = Some(pane.status.as_i32());
        Snapshot {
            revision,
            generation,
            panes: cached,
            ..Snapshot::default()
        }
    }

    #[test]
    fn saved_state_merges_current_changes_without_losing_read_state() {
        use crate::agent::persist::UiPaneState;

        let mut first = pane("%1", 0);
        first.stashed = true;
        first.content_hash = "current".into();
        let mut app = app_with_panes(vec![first, pane("%2", 1)]);
        app.cursor = app.find_pane_by_id("%2").unwrap();
        app.sort_mode = SortMode::Order;
        app.pending_unread_changes.insert("%1".into(), false);
        let mut state = UiState::default();
        state.panes.insert(
            app.panes["%1"].target.clone(),
            UiPaneState {
                forced_unread: true,
                ..UiPaneState::default()
            },
        );
        state.panes.insert(
            "%2".into(),
            UiPaneState {
                read_content_hash: Some("other-client-read".into()),
                ..UiPaneState::default()
            },
        );
        state.panes.insert("%99".into(), UiPaneState::default());

        app.update_saved_state(&mut state);
        assert_eq!(state.panes.len(), 2);
        assert!(state.panes["%1"].stashed);
        assert!(!state.panes["%1"].forced_unread);
        assert_eq!(
            state.panes["%1"].read_content_hash.as_deref(),
            Some("current")
        );
        assert_eq!(
            state.panes["%2"].read_content_hash.as_deref(),
            Some("other-client-read")
        );
        assert_eq!(state.selected_pane_id, "%2");
        assert_eq!(state.sort_mode, "order");
    }

    #[test]
    fn saved_state_removes_empty_entries_after_unstash() {
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        let mut state = UiState::default();
        app.panes.get_mut("%1").unwrap().stashed = true;
        app.update_saved_state(&mut state);
        assert!(state.panes["%1"].stashed);
        app.panes.get_mut("%1").unwrap().stashed = false;
        app.update_saved_state(&mut state);
        assert!(state.panes.is_empty());
        app.pending_unread_changes.insert("%1".into(), true);
        app.update_saved_state(&mut state);
        assert!(state.panes["%1"].forced_unread);
        assert!(state.panes["%1"].read_content_hash.is_none());
    }

    #[test]
    fn config_error_wrap_preserves_diagnostic_and_unicode() {
        let lines = config_error_lines("path/配置.toml\n\nunknown option", 8);
        assert!(lines.iter().all(|line| display_width(line) <= 8));
        assert!(lines.iter().any(String::is_empty));
        assert_eq!(lines.concat(), "path/配置.tomlunknown option");
        assert_eq!(config_error_lines("配置", 0), vec!["配", "置"]);
    }

    #[test]
    fn config_error_scroll_is_clamped_after_resize() {
        let error = "first\nsecond\nthird\nfourth";
        let mut scroll = usize::MAX;
        let mut grid = smelt_term::grid::Grid::new(50, 8);
        render_config_error(
            &mut grid.slice_mut(Rect::new(0, 0, 50, 8)),
            error,
            &mut scroll,
        );
        assert_eq!(scroll, 2);
        let mut grid = smelt_term::grid::Grid::new(80, 24);
        render_config_error(
            &mut grid.slice_mut(Rect::new(0, 0, 80, 24)),
            error,
            &mut scroll,
        );
        assert_eq!(scroll, 0);
        for size in [0, 1, 2] {
            let mut grid = smelt_term::grid::Grid::new(size, size);
            render_config_error(
                &mut grid.slice_mut(Rect::new(0, 0, size, size)),
                error,
                &mut scroll,
            );
        }
    }

    #[test]
    fn add_without_selected_session_does_nothing() {
        let mut app = app_with_panes(Vec::new());
        let (tx, _rx) = mpsc::channel();
        assert!(matches!(
            app.handle_key(KeyEvent::from(KeyCode::Char('a')), &tx),
            Action::None
        ));
        assert!(app.err.is_none());
    }

    #[test]
    fn slow_ui_receives_latest_preview_with_one_wakeup() {
        let latest = LatestPreview::default();
        let mut wakeups = 0;
        for generation in 0..1000 {
            wakeups += usize::from(latest.replace(
                PreviewTarget {
                    pane_id: PaneId::parse("%1").unwrap(),
                    session: "test".into(),
                    lines: 50,
                    generation,
                },
                format!("frame {generation}"),
            ));
        }
        assert_eq!(wakeups, 1);
        let (target, content) = latest.take().unwrap();
        assert_eq!(target.generation, 999);
        assert_eq!(content, "frame 999");
        assert!(latest.take().is_none());
        assert!(latest.replace(target, "next frame".into()));
        assert_eq!(latest.take().unwrap().1, "next frame");
    }

    #[test]
    fn late_cached_snapshot_cannot_undo_fresh_working_layout() {
        let mut app = app_with_panes(Vec::new());
        let mut busy = pane("%1", 0);
        busy.status = PaneStatus::Busy;
        assert!(app.apply_snapshot(snapshot_with_pane(5, 3, busy), UiState::default()));
        assert!(!app.apply_snapshot(snapshot_with_pane(4, 2, pane("%1", 0)), UiState::default()));
        assert_eq!(app.current_pane().unwrap().status, PaneStatus::Busy);
        assert_eq!(app.expanded_section, Some(Section::Working));
        assert_eq!(app.snapshot_revision, 5);
    }

    #[test]
    fn revision_advances_even_when_display_generation_does_not() {
        let mut app = app_with_panes(Vec::new());
        assert!(app.apply_snapshot(snapshot_with_pane(10, 5, pane("%1", 0)), UiState::default()));
        assert!(!app.apply_snapshot(snapshot_with_pane(12, 5, pane("%1", 0)), UiState::default()));
        assert_eq!(app.snapshot_revision, 12);
        assert!(!app.apply_snapshot(snapshot_with_pane(11, 6, pane("%2", 0)), UiState::default()));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.snapshot_generation, 5);
    }

    #[test]
    fn old_ui_state_cannot_reapply_a_removed_stash_entry() {
        let mut app = app_with_panes(Vec::new());
        let now = chrono::Utc::now();
        let current = UiState {
            updated_at: Some(now),
            ..UiState::default()
        };
        assert!(app.apply_snapshot(snapshot_with_pane(1, 1, pane("%1", 0)), current));
        let stale = UiState {
            updated_at: Some(now - chrono::Duration::seconds(1)),
            panes: HashMap::from([(
                "%1".into(),
                crate::agent::persist::UiPaneState {
                    stashed: true,
                    ..Default::default()
                },
            )]),
            ..UiState::default()
        };
        assert!(app.apply_snapshot(snapshot_with_pane(2, 2, pane("%1", 0)), stale));
        assert!(!app.panes["%1"].stashed);
        assert!(app.ui_state.panes.is_empty());
    }

    #[test]
    fn empty_sidebar_accepts_sort_key() {
        let mut app = app_with_panes(Vec::new());
        let (tx, rx) = mpsc::channel();
        assert!(matches!(
            app.handle_key(KeyEvent::from(KeyCode::Char('o')), &tx),
            Action::Redraw
        ));
        assert!(app.current_pane().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn empty_startup_delays_loading_until_needed() {
        let mut app = app_with_panes(Vec::new());
        for (loading_since, expected) in [
            (Some(Instant::now() + Duration::from_secs(1)), ""),
            (Some(Instant::now() - LOADING_DELAY), "Loading sessions..."),
            (None, "No active sessions"),
        ] {
            app.loading_since = loading_since;
            let mut grid = smelt_term::grid::Grid::new(100, 12);
            render_sidebar(&mut grid.slice_mut(Rect::new(0, 0, 40, 12)), &mut app);
            render_empty_preview(&mut grid.slice_mut(Rect::new(40, 0, 60, 12)), &app);
            let text = (0..12)
                .map(|row| {
                    (0..100)
                        .map(|col| grid.cell(col, row).symbol.as_str())
                        .collect::<String>()
                })
                .collect::<String>();
            if expected.is_empty() {
                assert!(text.trim().is_empty());
            } else {
                assert!(text.contains(expected));
            }
            assert!(!text.contains("Error:"));
            assert!(!text.contains("Refreshing"));
            if loading_since.is_some() {
                assert!(!text.contains("No active sessions"));
                assert!(!text.contains("Start a supported agent"));
            }
        }
    }

    #[test]
    fn recovery_error_keeps_sessions_visible_above_bounded_status_area() {
        let mut cached = pane("%1", 0);
        cached.window_name = "cached".into();
        let mut app = app_with_panes(vec![cached]);
        app.daemon_err =
            Some("validate installed executable: replacement validation timed out".into());
        let mut grid = smelt_term::grid::Grid::new(25, 8);
        render_sidebar(&mut grid.slice_mut(Rect::new(0, 0, 25, 8)), &mut app);
        let text = |start, end| {
            (start..end)
                .map(|row| {
                    (0..25)
                        .map(|column| grid.cell(column, row).symbol.as_str())
                        .collect::<String>()
                })
                .collect::<String>()
        };
        assert!(text(0, 5).contains("cached"));
        assert!(!text(0, 5).contains("Error:"));
        assert!(text(5, 8).starts_with("Error:"));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn sidebar_survives_terminal_shrink_and_restore() {
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        let mut surface = Surface::new(120, 24);
        app.resize(120, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let preferred = surface.paint_rect(SIDEBAR).unwrap().width;
        for width in [60, 39, 20, 3, 2, 1, 0, 120] {
            surface.set_terminal_size(width, 24);
            app.resize(width, 24);
            render(&mut surface, &mut app, &mut Vec::new()).unwrap();
            assert!(surface.paint_rect(SIDEBAR).unwrap().width <= width);
        }
        assert_eq!(surface.paint_rect(SIDEBAR).unwrap().width, preferred);
    }

    #[test]
    fn sidebar_restores_preference_after_temporary_clamp() {
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        let mut surface = Surface::new(200, 24);
        app.resize(200, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        assert_eq!(surface.paint_rect(SIDEBAR).unwrap().width, 50);
        surface.set_terminal_size(60, 24);
        app.resize(60, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        surface.set_terminal_size(200, 24);
        app.resize(200, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        assert_eq!(surface.paint_rect(SIDEBAR).unwrap().width, 50);
    }

    #[test]
    fn sidebar_keys_mouse_and_saved_width_share_bounds() {
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        let mut surface = Surface::new(120, 24);
        let (tx, _rx) = mpsc::channel();
        app.resize(120, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let cursor = app.cursor;
        for ch in ['3', 'L'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
        }
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        assert_eq!(surface.paint_rect(SIDEBAR).unwrap().width, 36);
        assert_eq!(app.ui_state.sidebar_width, 36);
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                mouse(MouseEventKind::Down(MouseButton::Left), 36)
            ),
            Action::Redraw
        ));
        assert_eq!(surface.active_split(), Some(app.sidebar.id()));
        app.handle_mouse(
            &mut surface,
            mouse(MouseEventKind::Drag(MouseButton::Left), 50),
        );
        assert_eq!(
            app.ui_state.sidebar_width, 36,
            "save only at gesture completion"
        );
        app.handle_mouse(
            &mut surface,
            mouse(MouseEventKind::Up(MouseButton::Left), 54),
        );
        assert_eq!(
            app.ui_state.sidebar_width, 54,
            "final pointer position is applied"
        );
        assert_eq!(surface.active_split(), None);
        assert_eq!(app.cursor, cursor);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        app.handle_mouse(
            &mut surface,
            mouse(MouseEventKind::Down(MouseButton::Left), 54),
        );
        app.handle_mouse(
            &mut surface,
            mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX),
        );
        assert!(app.cancel_resize(&mut surface));
        app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE), &tx);
        assert_eq!(surface.active_split(), None);
        assert_eq!(app.sidebar_width(), 97);
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                mouse(MouseEventKind::Drag(MouseButton::Left), 30)
            ),
            Action::None
        ));
        surface.set_terminal_size(39, 24);
        app.resize(39, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        app.save_state();
        assert_eq!(app.ui_state.sidebar_width, 97);
        surface.set_terminal_size(160, 24);
        app.resize(160, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        assert_eq!(surface.paint_rect(SIDEBAR).unwrap().width, 97);
    }

    #[test]
    fn preview_ansi_graphemes_survive_resize_and_clip_at_pane_boundary() {
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        app.cursor = app.find_pane_by_id("%1").unwrap();
        app.preview_lines = parse_ansi_lines("\x1b[31me\x1b[32m\u{301}界 tail");
        let mut surface = Surface::new(80, 8);
        app.resize(80, 8);
        let mut output = Vec::new();
        render(&mut surface, &mut app, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("e\u{301}"),
            "combining mark stays on its base across ANSI spans"
        );
        assert!(output.contains('界'));
        for width in [22, 3, 2, 80] {
            surface.set_terminal_size(width, 8);
            app.resize(width, 8);
            render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        }
        assert_eq!(truncate_width("e\u{301}界tail", 4), "e\u{301}界…");
        assert_eq!(truncate_width("界tail", 1), "…");
        assert_eq!(truncate_width("界tail", 0), "");
        assert_eq!(display_width("e\u{301}界"), 3);
    }

    #[test]
    fn removing_pane_returns_stable_tmux_id() {
        let mut selected = pane("%42", 0);
        selected.target = "session:9.1".to_string();
        let mut app = app_with_panes(vec![selected]);
        app.cursor = app.find_pane_by_id("%42").unwrap();

        assert_eq!(
            app.remove_current_pane().as_ref().map(PaneId::as_str),
            Some("%42")
        );
    }

    #[test]
    fn stashing_selects_next_main_session() {
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1), pane("%3", 2)]);
        app.cursor = app.find_pane_by_id("%2").unwrap();

        assert_eq!(app.toggle_current_stash(), Some(true));
        assert_eq!(
            app.current_pane().map(|pane| pane.pane_id.as_str()),
            Some("%3")
        );
        assert!(app.panes["%2"].stashed);
    }

    #[test]
    fn stashing_sessions_keeps_selection_on_available_session() {
        let mut busy = pane("%3", 2);
        busy.status = PaneStatus::Busy;
        let mut other_busy = pane("%4", 3);
        other_busy.status = PaneStatus::Busy;
        let mut stashed = pane("%5", 4);
        stashed.stashed = true;
        let mut app = app_with_panes(vec![
            pane("%1", 0),
            pane("%2", 1),
            busy,
            other_busy,
            stashed,
        ]);
        let (tx, _rx) = mpsc::channel();
        for (id, section) in [
            ("%2", None),
            ("%3", Some(Section::Working)),
            ("%4", Some(Section::Working)),
            ("%1", Some(Section::Stashed)),
        ] {
            app.handle_key(KeyEvent::from(KeyCode::Char('s')), &tx);
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
            assert_eq!(app.expanded_section, section);
        }
    }

    #[test]
    fn failed_kill_restores_selected_session_inside_working_section() {
        let mut busy = pane("%2", 1);
        busy.status = PaneStatus::Busy;
        let mut app = app_with_panes(vec![pane("%1", 0), busy]);
        let (tx, _rx) = mpsc::channel();
        app.handle_key(KeyEvent::from(KeyCode::Char('j')), &tx);
        app.remove_current_pane();
        app.restore_pending_kill("%2");
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%2");
        assert_eq!(app.expanded_section, Some(Section::Working));
    }

    #[test]
    fn unstashing_busy_session_preserves_selection_in_working_section() {
        let mut busy = pane("%2", 1);
        busy.status = PaneStatus::Busy;
        let mut stashed = pane("%3", 2);
        stashed.status = PaneStatus::Busy;
        stashed.stashed = true;
        let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
        let header = app
            .items
            .iter()
            .position(|item| matches!(item, TreeItem::SectionHeader(Section::Stashed, _)))
            .unwrap();
        app.select_cursor(header, false);
        let (tx, _rx) = mpsc::channel();
        app.handle_key(KeyEvent::from(KeyCode::Char('u')), &tx);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%3");
        assert_eq!(app.expanded_section, Some(Section::Working));
        assert_eq!(pane_ids(&app), ["%1", "%2", "%3"]);
    }

    fn pane_at(id: &str, order: usize, path: &str, status_changed_secs: i64) -> Pane {
        Pane {
            target: format!("session:1.{order}"),
            path: path.to_string(),
            order,
            status_changed_at: chrono::DateTime::from_timestamp(status_changed_secs, 0),
            park_epoch: chrono::DateTime::from_timestamp(status_changed_secs, 0),
            ..Pane::new(PaneId::parse(id).unwrap())
        }
    }

    fn pane_ids(app: &App) -> Vec<String> {
        app.items
            .iter()
            .filter_map(|it| match it {
                TreeItem::Pane(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn default_sort_orders_projects_and_sessions_by_state_change() {
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_panes(vec![
            pane_at("%1", 0, "/a", 100),
            pane_at("%2", 1, "/a", 300),
            pane_at("%3", 2, "/b", 500),
        ]);

        assert_eq!(SortMode::from_str(""), SortMode::Recent);
        assert_eq!(SortMode::from_str("order"), SortMode::Order);
        assert_eq!(app.sort_mode, SortMode::Recent);
        assert_eq!(pane_ids(&app), ["%3", "%2", "%1"]);

        app.panes.get_mut("%1").unwrap().last_active = chrono::DateTime::from_timestamp(1000, 0);
        app.rebuild_items();
        assert_eq!(pane_ids(&app), ["%3", "%2", "%1"]);

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
        assert_eq!(app.sort_mode, SortMode::Order);
        assert_eq!(pane_ids(&app), ["%1", "%2", "%3"]);

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
        assert_eq!(app.sort_mode, SortMode::Recent);
        assert_eq!(pane_ids(&app), ["%3", "%2", "%1"]);
    }

    #[test]
    fn project_group_keeps_worktrees_together_in_recent_order() {
        let mut main = pane_at("%1", 0, "/repo", 100);
        main.project_root = "/repo".into();
        let mut worktree = pane_at("%2", 1, "/repo-feature", 500);
        worktree.project_root = "/repo".into();
        let mut other = pane_at("%3", 2, "/other", 300);
        other.project_root = "/other".into();
        let app = app_with_panes(vec![main, worktree, other]);
        assert_eq!(pane_ids(&app), ["%2", "%1", "%3"]);
        assert_eq!(
            app.items
                .iter()
                .filter(|item| matches!(item, TreeItem::ProjectGroup(_)))
                .count(),
            2
        );
    }

    fn project(root: &str) -> CachedProject {
        CachedProject {
            root: root.to_string(),
            short: root.rsplit('/').next().unwrap().to_string(),
            branch: "main".to_string(),
            ahead: 1,
            ..CachedProject::default()
        }
    }

    fn app_with_projects(panes: Vec<Pane>, roots: &[&str]) -> App {
        let mut app = app_with_panes(panes);
        app.projects = roots.iter().map(|root| project(root)).collect();
        app.ui_state.projects = roots.iter().map(|root| (*root).to_string()).collect();
        app.rebuild_items();
        app.focus_first_session();
        app
    }

    fn project_roots(app: &App) -> Vec<String> {
        app.items
            .iter()
            .filter_map(|item| match item {
                TreeItem::Project(root) => Some(root.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn projects_without_sessions_stay_listed_and_selectable() {
        let mut busy = pane("%1", 1);
        busy.project_root = "/repo/a".to_string();
        let mut app = app_with_projects(vec![busy], &["/repo/a", "/repo/b", "/repo/c"]);
        assert_eq!(project_roots(&app), ["/repo/b", "/repo/c"]);

        assert_eq!(app.current_project().unwrap().root, "/repo/b");
        assert!(app.current_pane().is_none());
        app.select_cursor(next_pane(&app.items, app.cursor), true);
        assert_eq!(app.current_project().unwrap().root, "/repo/c");
        app.select_cursor(next_pane(&app.items, app.cursor), true);
        assert!(app.current_pane().is_some());
    }

    #[test]
    fn project_selection_survives_snapshot_updates() {
        let mut app = app_with_projects(Vec::new(), &["/repo/a", "/repo/b"]);
        app.select_cursor(next_pane(&app.items, app.cursor), false);
        assert_eq!(app.current_project().unwrap().root, "/repo/b");
        app.projects[1].dirty = true;
        app.replace_panes(Vec::new());
        assert_eq!(app.current_project().unwrap().root, "/repo/b");
    }

    #[test]
    fn forgetting_a_project_removes_it_and_persists() {
        let mut app = app_with_projects(Vec::new(), &["/repo/a", "/repo/b"]);
        assert!(app.forget_current_project());
        assert_eq!(project_roots(&app), ["/repo/b"]);
        assert_eq!(app.ui_state.projects, ["/repo/b"]);
        assert_eq!(app.current_project().unwrap().root, "/repo/b");
        assert!(!app.pending_forgotten.contains(&"/repo/a".to_string()));
        assert!(app.forget_current_project());
        assert!(project_roots(&app).is_empty());
        assert!(!app.forget_current_project());
    }

    #[test]
    fn action_targets_follow_the_selected_row() {
        let mut selected = pane("%1", 1);
        selected.path = "/repo/a-feature".to_string();
        selected.project_root = "/repo/a".to_string();
        selected.git_branch = "feature".to_string();
        selected.project_branch = "main".to_string();
        let mut app = app_with_projects(vec![selected], &["/repo/b"]);
        let target = app.action_target(false).unwrap();
        assert_eq!(
            (
                target.path.as_str(),
                target.root.as_str(),
                target.branch.as_str()
            ),
            ("/repo/b", "/repo/b", "main")
        );
        app.select_cursor(last_pane(&app.items).unwrap(), true);
        let target = app.action_target(false).unwrap();
        assert_eq!(
            (
                target.path.as_str(),
                target.root.as_str(),
                target.branch.as_str()
            ),
            ("/repo/a-feature", "/repo/a", "feature")
        );
        let target = app.action_target(true).unwrap();
        assert_eq!(
            (
                target.path.as_str(),
                target.root.as_str(),
                target.branch.as_str()
            ),
            ("/repo/a", "/repo/a", "main")
        );
    }

    #[test]
    fn pointer_motion_preserves_selection_and_preview() {
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1)]);
        let mut surface = Surface::new(120, 24);
        app.resize(120, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let mouse = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 4,
            row: app.find_pane_by_id("%2").unwrap() as u16,
            modifiers: KeyModifiers::NONE,
        };
        let generation = app.preview_gen;
        assert!(matches!(
            app.handle_mouse(&mut surface, mouse),
            Action::None
        ));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.preview_gen, generation);
        assert!(matches!(
            app.handle_mouse(&mut surface, mouse),
            Action::None
        ));
        assert_eq!(app.preview_gen, generation);
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                MouseEvent {
                    column: 80,
                    ..mouse
                }
            ),
            Action::None
        ));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn completed_rows_keep_checkmark_when_read() {
        for status in [PaneStatus::Unread, PaneStatus::Idle] {
            let mut completed = pane("%1", 0);
            completed.status = status;
            completed.completed = true;
            let app = app_with_panes(vec![completed]);
            let mut grid = smelt_term::grid::Grid::new(40, 1);
            render_pane_row(
                &mut grid.slice_mut(Rect::new(0, 0, 40, 1)),
                0,
                40,
                &app.panes["%1"],
                false,
                &app,
            );
            assert_eq!(grid.cell(3, 0).symbol.as_str(), "✓");
        }
    }

    #[test]
    fn working_rows_use_provider_and_busy_status_colors() {
        let mut busy = pane("%1", 0);
        busy.status = PaneStatus::Busy;
        busy.provider = "smelt".into();
        let app = app_with_panes(vec![busy]);
        let busy = &app.panes["%1"];
        let mut grid = smelt_term::grid::Grid::new(40, 1);
        render_pane_row(
            &mut grid.slice_mut(Rect::new(0, 0, 40, 1)),
            0,
            40,
            busy,
            false,
            &app,
        );
        assert_eq!(
            grid.cell(3, 0).style.fg,
            Some(Color::Rgb {
                r: 217,
                g: 119,
                b: 6
            })
        );
        assert_eq!(grid.cell(5, 0).style.fg, provider_style("smelt").fg);
    }

    #[test]
    fn pointer_motion_during_resize_preserves_session_selection() {
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1)]);
        let mut surface = Surface::new(120, 24);
        app.resize(120, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.sidebar_width(),
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            app.handle_mouse(&mut surface, mouse),
            Action::Redraw
        ));
        assert!(surface.active_split().is_some());
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 4,
                    row: app.find_pane_by_id("%2").unwrap() as u16,
                    ..mouse
                }
            ),
            Action::None
        ));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn title_clicks_toggle_sections_and_select_first_session() {
        let mut busy = pane("%2", 1);
        busy.status = PaneStatus::Busy;
        let mut stashed = pane("%3", 2);
        stashed.stashed = true;
        let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
        let mut surface = Surface::new(120, 24);
        app.resize(120, 24);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let header = app
            .items
            .iter()
            .position(|item| matches!(item, TreeItem::SectionHeader(Section::Stashed, _)))
            .unwrap();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: header as u16,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                MouseEvent {
                    kind: MouseEventKind::Moved,
                    ..mouse
                }
            ),
            Action::None
        ));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert!(matches!(
            app.handle_mouse(&mut surface, mouse),
            Action::Redraw
        ));
        assert_eq!(app.expanded_section, Some(Section::Stashed));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%3");
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        assert!(matches!(
            app.handle_mouse(&mut surface, mouse),
            Action::Redraw
        ));
        assert_eq!(app.expanded_section, None);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let busy_row = app
            .items
            .iter()
            .position(|item| matches!(item, TreeItem::SectionHeader(Section::Working, _)))
            .unwrap() as u16;
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                MouseEvent {
                    row: busy_row,
                    ..mouse
                }
            ),
            Action::Redraw
        ));
        assert_eq!(app.expanded_section, Some(Section::Working));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%2");
        assert_eq!(pane_ids(&app), ["%1", "%2"]);
    }

    fn click_title(app: &mut App, section: Section) {
        let mut surface = Surface::new(120, 24);
        app.resize(120, 24);
        render(&mut surface, app, &mut Vec::new()).unwrap();
        let row = app
            .items
            .iter()
            .position(|item| matches!(item, TreeItem::SectionHeader(found, _) if *found == section))
            .unwrap();
        assert!(matches!(
            app.handle_mouse(
                &mut surface,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 4,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE,
                }
            ),
            Action::Redraw
        ));
    }

    #[test]
    fn collapse_requires_destination_for_mouse_and_keyboard() {
        let (tx, _rx) = mpsc::channel();
        for section in [Section::Working, Section::Stashed] {
            let mut first = pane("%1", 0);
            first.status = PaneStatus::Busy;
            first.stashed = section == Section::Stashed;
            let mut second = first.clone();
            second.pane_id = pane("%2", 1).pane_id;
            second.order = 1;
            let mut app = app_with_panes(vec![first, second]);
            for key in [
                KeyCode::Char('j'),
                KeyCode::Left,
                KeyCode::Char('h'),
                KeyCode::Char('k'),
            ] {
                app.handle_key(KeyEvent::from(key), &tx);
                assert!(app.current_pane().is_some());
                assert!(app.section_is_expanded(section));
            }
            for _ in 0..3 {
                click_title(&mut app, section);
                assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
                assert!(app.section_is_expanded(section));
            }
        }
    }

    #[test]
    fn collapse_moves_into_other_closed_section() {
        let (tx, _rx) = mpsc::channel();
        let mut busy = pane("%1", 0);
        busy.status = PaneStatus::Busy;
        let mut stashed = pane("%2", 1);
        stashed.stashed = true;
        let mut app = app_with_panes(vec![busy, stashed]);
        for _ in 0..3 {
            click_title(&mut app, Section::Working);
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%2");
            assert_eq!(app.expanded_section, Some(Section::Stashed));
            assert!(matches!(
                app.handle_key(KeyEvent::from(KeyCode::Left), &tx),
                Action::Redraw
            ));
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
            assert_eq!(app.expanded_section, Some(Section::Working));
        }
    }

    #[test]
    fn always_expanded_sections_refuse_collapse_with_other_sessions() {
        let (tx, _rx) = mpsc::channel();
        let mut busy = pane("%2", 1);
        busy.status = PaneStatus::Busy;
        let mut stashed = pane("%3", 2);
        stashed.stashed = true;
        let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
        app.sidebar_config.working_section = SectionBehavior::Expand;
        app.sidebar_config.stashed_section = SectionBehavior::Expand;
        app.focus_startup_session();
        for (section, id) in [(Section::Working, "%2"), (Section::Stashed, "%3")] {
            click_title(&mut app, section);
            click_title(&mut app, section);
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
            assert!(matches!(
                app.handle_key(KeyEvent::from(KeyCode::Char('h')), &tx),
                Action::None
            ));
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
        }
    }

    #[test]
    fn manual_parking_ignores_idle_stashed_and_other_modes() {
        use PaneStatus::{Busy, Idle, Unread};
        use WorkingSection::{Inline, InlineLast, Manual, Section};
        for (mode, status, stashed) in [
            (Manual, Idle, false),
            (Manual, Unread, false),
            (Manual, Busy, true),
            (Section, Busy, false),
            (Inline, Busy, false),
            (InlineLast, Busy, false),
        ] {
            let mut p = pane_at("%1", 0, "/repo", 100);
            p.status = status;
            p.stashed = stashed;
            let mut app = app_with_panes(vec![p]);
            app.sidebar_config.working = mode;
            assert!(matches!(app.toggle_current_parking(), Action::None));
            assert!(app.panes["%1"].parked_since.is_none());
        }
    }

    #[test]
    fn parking_only_session_keeps_it_accessible() {
        let mut p = pane_at("%1", 0, "/repo", 100);
        p.status = PaneStatus::Busy;
        let mut app = app_with_panes(vec![p]);
        app.sidebar_config.working = WorkingSection::Manual;
        assert!(matches!(app.toggle_current_parking(), Action::Redraw));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.pane_section(&app.panes["%1"]), Some(Section::Working));
        assert!(matches!(app.toggle_current_parking(), Action::Redraw));
        assert_eq!(app.pane_section(&app.panes["%1"]), None);
    }

    #[test]
    fn waiting_session_stays_out_of_working_until_parked_manually() {
        let mut p = pane_at("%1", 0, "/repo", 100);
        p.status = PaneStatus::Waiting;
        for mode in [WorkingSection::Section, WorkingSection::Inline] {
            let mut app = app_with_panes(vec![p.clone()]);
            app.sidebar_config.working = mode;
            assert_eq!(app.pane_section(&app.panes["%1"]), None);
            assert!(matches!(app.toggle_current_parking(), Action::None));
        }
        let mut app = app_with_panes(vec![p]);
        app.sidebar_config.working = WorkingSection::Manual;
        assert!(matches!(app.toggle_current_parking(), Action::Redraw));
        assert_eq!(app.pane_section(&app.panes["%1"]), Some(Section::Working));
    }

    #[test]
    fn waiting_rows_use_orange_outline_circle() {
        let mut waiting = pane("%1", 0);
        waiting.status = PaneStatus::Waiting;
        let app = app_with_panes(vec![waiting]);
        let mut grid = smelt_term::grid::Grid::new(40, 1);
        render_pane_row(
            &mut grid.slice_mut(Rect::new(0, 0, 40, 1)),
            0,
            40,
            &app.panes["%1"],
            false,
            &app,
        );
        assert_eq!(grid.cell(3, 0).symbol.as_str(), "○");
        assert_eq!(
            grid.cell(3, 0).style.fg,
            Some(Color::Rgb {
                r: 217,
                g: 119,
                b: 6
            })
        );
    }

    #[test]
    fn saved_parking_expires_even_if_completion_snapshot_was_missed() {
        let mut p = pane_at("%1", 0, "/repo", 100);
        p.status = PaneStatus::Busy;
        let mut app = app_with_panes(vec![p.clone()]);
        app.sidebar_config.working = WorkingSection::Manual;
        app.toggle_current_parking();
        let state = app.ui_state.clone();
        crate::agent::persist::apply_pane_ui_state(&mut p, &state.panes["%1"]);
        assert!(p.parked_since.is_some());
        p.park_epoch = Some(p.park_epoch.unwrap() + chrono::Duration::seconds(1));
        crate::agent::persist::apply_pane_ui_state(&mut p, &state.panes["%1"]);
        assert!(p.parked_since.is_none());
    }

    #[test]
    fn inline_working_keeps_busy_sessions_in_main_but_respects_stash() {
        let mut busy = pane("%2", 1);
        busy.status = PaneStatus::Busy;
        let mut stashed = pane("%3", 2);
        stashed.status = PaneStatus::Busy;
        stashed.stashed = true;
        let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
        app.sidebar_config.working = WorkingSection::Inline;
        app.focus_startup_session();
        assert_eq!(pane_ids(&app), ["%1", "%2"]);
        assert_eq!(app.pane_section(&app.panes["%2"]), None);
        assert_eq!(app.pane_section(&app.panes["%3"]), Some(Section::Stashed));
        assert!(
            !app.items
                .iter()
                .any(|item| matches!(item, TreeItem::SectionHeader(Section::Working, _)))
        );
    }

    #[test]
    fn inline_last_preserves_sort_order_within_each_project() {
        let panes = (0..6)
            .map(|i| {
                let root = if i < 4 { "/a" } else { "/b" };
                let mut p = pane_at(&format!("%{i}"), i, root, i as i64);
                p.project_root = root.to_string();
                if i % 2 == 0 {
                    p.status = PaneStatus::Busy;
                }
                p
            })
            .collect();
        let mut app = app_with_panes(panes);
        app.sidebar_config.working = WorkingSection::InlineLast;
        for (sort, expected) in [
            (SortMode::Order, ["%1", "%3", "%0", "%2", "%5", "%4"]),
            (SortMode::Recent, ["%5", "%4", "%3", "%1", "%2", "%0"]),
        ] {
            app.sort_mode = sort;
            app.rebuild_items();
            assert_eq!(pane_ids(&app), expected);
        }
    }

    #[test]
    fn inline_last_preserves_selection_when_status_changes() {
        let mut busy = pane("%1", 0);
        busy.status = PaneStatus::Busy;
        let idle = pane("%2", 1);
        let mut app = app_with_panes(vec![busy.clone(), idle.clone()]);
        app.sidebar_config.working = WorkingSection::InlineLast;
        app.sort_mode = SortMode::Order;
        app.rebuild_items();
        app.cursor = app.find_pane_by_id("%1").unwrap();
        assert_eq!(pane_ids(&app), ["%2", "%1"]);
        busy.status = PaneStatus::Unread;
        app.replace_panes(vec![busy.clone(), idle.clone()]);
        assert_eq!(pane_ids(&app), ["%1", "%2"]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        busy.status = PaneStatus::Busy;
        app.replace_panes(vec![busy, idle]);
        assert_eq!(pane_ids(&app), ["%2", "%1"]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn startup_selects_first_session_in_sidebar_order() {
        let first = pane_at("%1", 0, "/repo", 200);
        let mut unread = pane_at("%2", 1, "/repo", 100);
        unread.status = PaneStatus::Unread;
        let mut app = app_with_panes(vec![first, unread]);
        app.sidebar_config.select = StartupSelection::First;
        app.focus_startup_session();
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn startup_prioritizes_unread_without_marking_it_read() {
        let first = pane_at("%1", 0, "/repo", 200);
        let mut unread = pane_at("%2", 1, "/repo", 100);
        unread.status = PaneStatus::Unread;
        let mut app = app_with_panes(vec![first, unread]);
        app.ui_state.selected_pane_id = "%1".into();
        app.focus_startup_session();
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%2");
        assert_eq!(app.panes["%2"].status, PaneStatus::Unread);
        assert!(app.pending_unread_changes.is_empty());
    }

    #[test]
    fn startup_selection_modes_control_priority_and_fallback() {
        for (mode, expected, missing_expected) in [
            (StartupSelection::Unread, "%2", "%2"),
            (StartupSelection::Last, "%3", "%1"),
            (StartupSelection::First, "%1", "%1"),
        ] {
            let first = pane_at("%1", 0, "/repo", 300);
            let mut unread = pane_at("%2", 1, "/repo", 200);
            unread.status = PaneStatus::Unread;
            let saved = pane_at("%3", 2, "/repo", 100);
            let mut app = app_with_panes(vec![first, unread, saved]);
            app.sidebar_config.select = mode;
            for (id, selected) in [("%3", expected), ("%99", missing_expected)] {
                app.ui_state.selected_pane_id = id.into();
                app.focus_startup_session();
                assert_eq!(app.current_pane().unwrap().pane_id.as_str(), selected);
            }
        }
    }

    #[test]
    fn startup_unread_selection_excludes_stashed_sessions() {
        for behavior in [SectionBehavior::Collapse, SectionBehavior::Expand] {
            let mut deferred = pane_at("%2", 1, "/repo", 300);
            deferred.status = PaneStatus::Unread;
            deferred.stashed = true;
            let mut app = app_with_panes(vec![pane_at("%1", 0, "/repo", 100), deferred]);
            app.sidebar_config.stashed_section = behavior;
            app.focus_startup_session();
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        }
    }

    #[test]
    fn startup_sort_uses_config_or_saved_order() {
        for configured in [None, Some(SortMode::Recent), Some(SortMode::Order)] {
            for (saved, saved_mode) in [
                ("", SortMode::Recent),
                ("recent", SortMode::Recent),
                ("order", SortMode::Order),
            ] {
                let mut app = app_with_panes(vec![
                    pane_at("%1", 0, "/repo", 100),
                    pane_at("%2", 1, "/repo", 200),
                ]);
                app.sidebar_config.sort = configured;
                app.ui_state.sort_mode = saved.into();
                app.focus_startup_session();
                let expected = configured.unwrap_or(saved_mode);
                assert_eq!(app.sort_mode, expected);
                assert_eq!(
                    pane_ids(&app),
                    if expected == SortMode::Order {
                        ["%1", "%2"]
                    } else {
                        ["%2", "%1"]
                    }
                );
            }
        }
    }

    #[test]
    fn unread_completion_does_not_steal_selection_after_startup() {
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1)]);
        let mut completed = pane("%2", 1);
        completed.status = PaneStatus::Unread;
        app.replace_panes(vec![pane("%1", 0), completed]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn first_loaded_snapshot_prioritizes_unread_session() {
        let mut app = app_with_panes(Vec::new());
        let mut unread = pane("%2", 1);
        unread.status = PaneStatus::Unread;
        app.replace_panes(vec![pane("%1", 0), unread]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%2");
    }

    #[test]
    fn first_loaded_snapshot_selects_first_stashed_session() {
        let mut app = app_with_panes(Vec::new());
        let mut stashed = pane("%1", 0);
        stashed.stashed = true;
        app.replace_panes(vec![stashed]);
        assert_eq!(app.expanded_section, Some(Section::Stashed));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn busy_sessions_automatically_appear_in_collapsed_working_section() {
        let mut busy = pane("%2", 1);
        busy.status = PaneStatus::Busy;
        let app = app_with_panes(vec![pane("%1", 0), busy]);
        assert_eq!(pane_ids(&app), ["%1"]);
        assert_eq!(app.pane_section(&app.panes["%2"]), Some(Section::Working));
        assert!(
            app.items
                .iter()
                .any(|item| matches!(item, TreeItem::SectionHeader(Section::Working, 1)))
        );
    }

    #[test]
    fn startup_restores_session_inside_section_before_first_render() {
        for (id, section) in [("%2", Section::Working), ("%3", Section::Stashed)] {
            let mut busy = pane("%2", 1);
            busy.status = PaneStatus::Busy;
            let mut stashed = pane("%3", 2);
            stashed.stashed = true;
            let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
            app.ui_state.selected_pane_id = id.into();
            app.focus_startup_session();
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
            assert_eq!(app.expanded_section, Some(section));
            assert_eq!(pane_ids(&app), ["%1", id]);
            let mut surface = Surface::new(120, 24);
            app.resize(120, 24);
            render(&mut surface, &mut app, &mut Vec::new()).unwrap();
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
            assert_eq!(app.expanded_section, Some(section));
        }
    }

    #[test]
    fn startup_falls_back_to_first_session_when_saved_session_is_unavailable() {
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        app.ui_state.selected_pane_id = "%99".into();
        app.focus_startup_session();
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
    }

    #[test]
    fn startup_expands_only_working_and_selects_first_session() {
        let mut busy = pane("%1", 0);
        busy.status = PaneStatus::Busy;
        let app = app_with_panes(vec![busy]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.expanded_section, Some(Section::Working));
    }

    #[test]
    fn busy_stashed_sessions_remain_in_stash() {
        let mut stashed = pane("%2", 1);
        stashed.status = PaneStatus::Busy;
        stashed.stashed = true;
        let app = app_with_panes(vec![pane("%1", 0), stashed]);
        assert_eq!(app.pane_section(&app.panes["%2"]), Some(Section::Stashed));
        assert_eq!(pane_ids(&app), ["%1"]);
    }

    #[test]
    fn section_configuration_controls_visibility_without_selecting_headers() {
        for (working, working_section) in [
            (WorkingSection::Section, SectionBehavior::Collapse),
            (WorkingSection::Section, SectionBehavior::Expand),
            (WorkingSection::Inline, SectionBehavior::Collapse),
            (WorkingSection::InlineLast, SectionBehavior::Collapse),
        ] {
            for stashed_behavior in [SectionBehavior::Collapse, SectionBehavior::Expand] {
                let mut busy = pane("%2", 1);
                busy.status = PaneStatus::Busy;
                let mut stashed = pane("%3", 2);
                stashed.stashed = true;
                let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
                app.sidebar_config = SidebarConfig {
                    working,
                    working_section,
                    stashed_section: stashed_behavior,
                    ..SidebarConfig::default()
                };
                app.focus_startup_session();
                assert_eq!(
                    app.find_pane_by_id("%2").is_some(),
                    working != WorkingSection::Section
                        || working_section == SectionBehavior::Expand
                );
                assert_eq!(
                    app.find_pane_by_id("%3").is_some(),
                    stashed_behavior == SectionBehavior::Expand
                );
                let (tx, _rx) = mpsc::channel();
                for id in ["%2", "%3", "%1"] {
                    app.handle_key(KeyEvent::from(KeyCode::Char('j')), &tx);
                    assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
                }
                assert_eq!(
                    app.find_pane_by_id("%2").is_some(),
                    working != WorkingSection::Section
                        || working_section == SectionBehavior::Expand
                );
                assert_eq!(
                    app.find_pane_by_id("%3").is_some(),
                    stashed_behavior == SectionBehavior::Expand
                );
                for id in ["%3", "%2", "%1"] {
                    app.handle_key(KeyEvent::from(KeyCode::Char('k')), &tx);
                    assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
                }
                assert_eq!(
                    app.find_pane_by_id("%2").is_some(),
                    working != WorkingSection::Section
                        || working_section == SectionBehavior::Expand
                );
                assert_eq!(
                    app.find_pane_by_id("%3").is_some(),
                    stashed_behavior == SectionBehavior::Expand
                );
            }
        }
    }

    #[test]
    fn sections_expand_on_entry_and_collapse_on_exit_in_both_directions() {
        let (tx, _rx) = mpsc::channel();
        let mut working = pane("%2", 1);
        working.status = PaneStatus::Busy;
        let mut stashed = pane("%3", 2);
        stashed.stashed = true;
        let mut app = app_with_panes(vec![pane("%1", 0), working, stashed]);
        app.cursor = app.find_pane_by_id("%1").unwrap();
        assert_eq!(pane_ids(&app), ["%1"]);
        for (key, id, section) in [
            ('j', "%2", Some(Section::Working)),
            ('j', "%3", Some(Section::Stashed)),
            ('j', "%1", None),
            ('k', "%3", Some(Section::Stashed)),
            ('k', "%2", Some(Section::Working)),
            ('k', "%1", None),
        ] {
            app.handle_key(KeyEvent::from(KeyCode::Char(key)), &tx);
            assert_eq!(app.current_pane().unwrap().pane_id.as_str(), id);
            assert_eq!(app.expanded_section, section);
            assert_eq!(pane_ids(&app).len(), if section.is_some() { 2 } else { 1 });
        }
    }

    #[test]
    fn startup_expands_only_stash_and_selects_first_session() {
        let (tx, _rx) = mpsc::channel();
        let mut stashed = pane("%1", 0);
        stashed.stashed = true;
        let mut app = app_with_panes(vec![stashed]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.expanded_section, Some(Section::Stashed));
        app.handle_key(KeyEvent::from(KeyCode::Char('u')), &tx);
        assert!(!app.panes["%1"].stashed);
        assert_eq!(app.expanded_section, None);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(pane_ids(&app), ["%1"]);
    }

    #[test]
    fn selected_session_keeps_focus_when_it_starts_working() {
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1)]);
        let mut busy = pane("%1", 0);
        busy.status = PaneStatus::Busy;
        app.replace_panes(vec![busy, pane("%2", 1)]);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.expanded_section, Some(Section::Working));
        assert_eq!(pane_ids(&app), ["%2", "%1"]);
    }

    #[test]
    fn completed_working_session_returns_to_main_list() {
        let mut working = pane("%2", 1);
        working.status = PaneStatus::Busy;
        let mut app = app_with_panes(vec![pane("%1", 0), working.clone()]);
        app.cursor = app.find_pane_by_id("%1").unwrap();
        let (tx, _rx) = mpsc::channel();
        app.handle_key(KeyEvent::from(KeyCode::Char('j')), &tx);
        assert_eq!(
            app.pane_section(app.current_pane().unwrap()),
            Some(Section::Working)
        );
        working.status = PaneStatus::Unread;
        app.replace_panes(vec![pane("%1", 0), working]);
        assert_eq!(app.pane_section(&app.panes["%2"]), None);
        assert_eq!(app.panes["%2"].status, PaneStatus::Unread);
        assert_eq!(app.expanded_section, None);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%2");
        assert_eq!(pane_ids(&app), ["%1", "%2"]);
    }

    #[test]
    fn mark_all_read_clears_unread_and_leaves_busy_untouched() {
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1)]);
        app.panes.get_mut("%1").unwrap().status = PaneStatus::Unread;
        app.panes.get_mut("%2").unwrap().status = PaneStatus::Busy;

        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), &tx);

        assert_eq!(app.panes["%1"].status, PaneStatus::Idle);
        assert_eq!(app.panes["%2"].status, PaneStatus::Busy);
    }

    #[test]
    fn space_toggles_idle_and_unread_but_not_busy() {
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_panes(vec![pane("%1", 0)]);
        app.cursor = app.find_pane_by_id("%1").unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        assert_eq!(app.panes["%1"].status, PaneStatus::Unread);
        assert!(app.ui_state.panes["%1"].forced_unread);
        assert!(app.pending_unread_changes.is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        assert_eq!(app.panes["%1"].status, PaneStatus::Idle);
        assert!(!app.ui_state.panes["%1"].forced_unread);
        assert_eq!(
            app.ui_state.panes["%1"].read_content_hash.as_deref(),
            Some("")
        );
        assert!(app.pending_unread_changes.is_empty());

        app.panes.get_mut("%1").unwrap().status = PaneStatus::Busy;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        assert_eq!(app.panes["%1"].status, PaneStatus::Busy);
    }

    #[test]
    fn emacs_navigation_matches_vim_motions() {
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1), pane("%3", 2)]);
        let cur = |app: &App| app.current_pane().map(|p| p.pane_id.to_string());

        app.cursor = app.find_pane_by_id("%1").unwrap();

        // C-n moves down like j.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(cur(&app).as_deref(), Some("%2"));

        // C-p moves up like k.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(cur(&app).as_deref(), Some("%1"));

        // M-> jumps to the last session like G.
        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::ALT), &tx);
        assert_eq!(cur(&app).as_deref(), Some("%3"));

        // M-< jumps to the first session like gg.
        app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::ALT), &tx);
        assert_eq!(cur(&app).as_deref(), Some("%1"));
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn worktree_keys_open_a_picker_that_escape_closes() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path().join("repo");
        let linked = temporary.path().join("repo-feature");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        let root = repo.to_str().unwrap();
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_projects(Vec::new(), &[root]);
        app.actions = actions::bind(&Config::default()).unwrap();
        app.select_cursor(app.find_project(root).unwrap(), false);

        for key in ['A', 'T'] {
            let opened =
                app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::SHIFT), &tx);
            assert!(matches!(opened, Action::Redraw), "{key}");
            assert!(app.picker.is_some(), "{key}");
            // Quit keys close the picker without leaving the sidebar.
            let closed = app.handle_key(KeyEvent::from(KeyCode::Esc), &tx);
            assert!(matches!(closed, Action::Redraw), "{key}");
            assert!(app.picker.is_none(), "{key}");
        }
    }
}
