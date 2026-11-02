use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
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
    Axis, DividerStyles, HitRegistry, LayoutStyle, LayoutTree, Line, NoopSizer, PaintId,
    ResolvedLayout, Split, SplitInteraction, SplitOptions, SplitPane, SplitResizeMode, SplitSize,
    Surface, TerminalSession,
};

use crate::agent::ipc;
use crate::agent::persist::{
    Snapshot, UiState, apply_ui_state, load_snapshot, load_ui_state, panes_from_snapshot,
};
#[cfg(not(test))]
use crate::agent::persist::{ui_pane_state_is_empty, update_ui_state};
use crate::agent::{Pane, PaneId, PaneStatus, capture_pane, kill_pane, switch_to_pane};
use crate::config::{
    Config, SectionBehavior, SidebarConfig, SortMode, StartupSelection, WorkingSection,
};

const SIDEBAR: PaintId = PaintId(1);
const PREVIEW: PaintId = PaintId(3);
const MIN_SIDEBAR: u16 = 20;
const MIN_PREVIEW: u16 = 20;
const SYNCING_MSG: &str = "syncing agent-mux snapshot";

#[derive(Clone, Debug)]
enum Hit {
    Pane(String),
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
}

#[derive(Debug)]
enum Msg {
    PanesLoaded {
        panes: Vec<Pane>,
        snapshot_generation: u64,
        ui_state: UiState,
        err: Option<String>,
        live: bool,
    },
    PreviewLoaded {
        pane_id: String,
        content: String,
        preview_seq: u64,
    },
    PaneKilled {
        pane_id: String,
        err: Option<String>,
    },
    SubscriptionEnded,
    DaemonChecked(Option<String>),
}

pub fn run(tmux_session: String) -> Result<()> {
    let config = Config::load()?;
    let mut term = TerminalSession::builder()
        .buffer_capacity(128 * 1024)
        .focus_events(true)
        .enter_stdout()?;
    let (w, h) = term.size()?;
    let mut surface = Surface::new(w, h);

    let mut app = App::new(tmux_session, config.sidebar);
    app.resize(w, h);
    run_loop(&mut surface, term.writer(), &mut app).map_err(Into::into)
}

fn run_loop<W: Write>(surface: &mut Surface, writer: &mut W, app: &mut App) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_millis(33);
    let mut last_panes = Instant::now() - Duration::from_millis(500);
    let mut last_preview = Instant::now();
    let mut last_subscribe = Instant::now() - Duration::from_secs(1);
    let mut panes_pending = false;
    let mut preview_pending = false;
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
    load_preview(app);

    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::PanesLoaded {
                    mut panes,
                    snapshot_generation,
                    ui_state,
                    err,
                    live,
                } => {
                    if live {
                        subscribed = true;
                        subscribe_pending = false;
                    } else {
                        panes_pending = false;
                    }
                    let mut changed = false;
                    if let Some(err) = err {
                        if err == SYNCING_MSG && app.has_display_snapshot() {
                            if app.err.as_deref() == Some(SYNCING_MSG) {
                                app.err = None;
                                changed = true;
                            }
                        } else if app.err.as_deref() != Some(&err) {
                            app.err = Some(err);
                            changed = true;
                        }
                    } else {
                        if app.err.is_some() {
                            app.err = None;
                            changed = true;
                        }
                        app.hide_pending_kills(&mut panes);
                        let ui_is_older = ui_state_is_older_than(&ui_state, &app.ui_state);
                        let ui_changed =
                            !ui_is_older && ui_state.updated_at != app.ui_state.updated_at;
                        if ui_changed {
                            app.ui_state = ui_state;
                        } else if ui_is_older {
                            apply_ui_state(&mut panes, &app.ui_state);
                        }
                        if snapshot_generation != app.snapshot_generation || ui_changed {
                            app.snapshot_generation = snapshot_generation;
                            app.replace_panes(panes);
                            changed = true;
                        }
                    }
                    dirty |= changed;
                }
                Msg::PreviewLoaded {
                    pane_id,
                    content,
                    preview_seq,
                } => {
                    preview_pending = false;
                    if preview_seq == app.preview_gen
                        && app
                            .current_pane()
                            .is_some_and(|pane| pane.pane_id.as_str() == pane_id)
                    {
                        app.preview_lines = preview_lines(&content);
                        dirty = true;
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

        if last_preview.elapsed() >= Duration::from_millis(100)
            && !preview_pending
            && app.current_pane().is_some()
        {
            preview_pending = spawn_preview(&tx, app);
            last_preview = Instant::now();
        }

        if dirty || last_draw.elapsed() >= Duration::from_millis(250) {
            render(surface, app, writer)?;
            dirty = false;
            last_draw = Instant::now();
        }

        let poll_for = Duration::from_millis(33)
            .saturating_sub(last_draw.elapsed())
            .max(Duration::from_millis(1));
        if event::poll(poll_for)? {
            let action = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dirty |= app.split_interaction.active_id().is_some();
                    app.handle_key(key, &tx)
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(w, h) => {
                    surface.set_terminal_size(w, h);
                    app.resize(w, h);
                    Action::Redraw
                }
                Event::FocusLost => {
                    app.cancel_resize();
                    Action::Redraw
                }
                _ => Action::None,
            };
            match action {
                Action::Quit => return Ok(()),
                Action::Redraw => dirty = true,
                Action::Preview => {
                    preview_pending = spawn_preview(&tx, app);
                    dirty = true;
                }
                Action::None => {}
            }
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
    let request = serde_json::to_string(&ipc::Request::Subscribe)?;
    writeln!(stream, "{request}")?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        match serde_json::from_str::<ipc::Response>(&line)? {
            ipc::Response::State { snapshot, ui_state } => {
                send_panes_loaded(tx, snapshot, ui_state, true);
            }
            ipc::Response::Error { message } => {
                let _ = tx.send(Msg::PanesLoaded {
                    panes: Vec::new(),
                    snapshot_generation: 0,
                    ui_state: load_ui_state(),
                    err: Some(message),
                    live: true,
                });
            }
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
    let Some(snapshot) = snapshot else {
        let _ = tx.send(Msg::PanesLoaded {
            panes: Vec::new(),
            snapshot_generation: 0,
            ui_state,
            err: Some(SYNCING_MSG.into()),
            live,
        });
        return;
    };
    let snapshot_generation = snapshot.generation;
    let mut panes = panes_from_snapshot(&snapshot);
    apply_ui_state(&mut panes, &ui_state);
    let _ = tx.send(Msg::PanesLoaded {
        panes,
        snapshot_generation,
        ui_state,
        err: None,
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

fn load_preview(app: &mut App) {
    let Some(p) = app.current_pane() else { return };
    let pane_id = p.pane_id.clone();
    let lines = app.height.max(50) as usize;
    let content = capture_pane(&pane_id, lines).unwrap_or_else(|err| format!("error: {err}"));
    app.preview_lines = preview_lines(&content);
}

fn spawn_preview(tx: &mpsc::Sender<Msg>, app: &App) -> bool {
    let Some(p) = app.current_pane() else {
        return false;
    };
    let pane_id = p.pane_id.clone();
    let lines = app.height.max(50) as usize;
    let preview_seq = app.preview_gen;
    let tx = tx.clone();
    thread::spawn(move || {
        let content = capture_pane(&pane_id, lines).unwrap_or_else(|err| format!("error: {err}"));
        let _ = tx.send(Msg::PreviewLoaded {
            pane_id: pane_id.to_string(),
            content,
            preview_seq,
        });
    });
    true
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
    Preview,
    Quit,
}

impl SortMode {
    #[cfg(not(test))]
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
    project_win_width: HashMap<String, usize>,
    width: u16,
    height: u16,
    sidebar: Split,
    split_interaction: SplitInteraction,
    show_help: bool,
    pending_d: bool,
    pending_g: bool,
    sort_mode: SortMode,
    expanded_section: Option<Section>,
    sidebar_config: SidebarConfig,
    count: usize,
    err: Option<String>,
    daemon_err: Option<String>,
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
        let (snapshot, ui_state) = load_pane_state();
        let snapshot_generation = snapshot
            .as_ref()
            .map(|snapshot| snapshot.generation)
            .unwrap_or_default();
        let mut panes = snapshot
            .as_ref()
            .map(panes_from_snapshot)
            .unwrap_or_default();
        apply_ui_state(&mut panes, &ui_state);
        let mut app = Self {
            panes: panes
                .into_iter()
                .map(|p| (p.pane_id.to_string(), p))
                .collect(),
            items: Vec::new(),
            cursor: 0,
            preview_lines: Vec::new(),
            preview_gen: 1,
            snapshot_generation,
            project_win_width: HashMap::new(),
            width: 0,
            height: 0,
            sidebar: sidebar_split(ui_state.sidebar_width),
            split_interaction: SplitInteraction::default(),
            show_help: false,
            pending_d: false,
            pending_g: false,
            sort_mode: SortMode::default(),
            expanded_section: None,
            sidebar_config,
            count: 0,
            err: snapshot.is_none().then(|| SYNCING_MSG.to_string()),
            daemon_err: None,
            ui_state,
            pending_unread_changes: HashMap::new(),
            pending_kills: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: tmux_session,
        };
        app.focus_startup_session();
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

    fn replace_panes(&mut self, panes: Vec<Pane>) {
        let selected = self.current_pane().map(|p| p.pane_id.to_string());
        self.panes = panes
            .into_iter()
            .map(|p| (p.pane_id.to_string(), p))
            .collect();
        if let Some(pane) = selected.as_ref().and_then(|id| self.panes.get(id)) {
            self.expanded_section = self.pane_section(pane);
        }
        if selected.is_none() {
            self.focus_startup_session();
        } else {
            self.rebuild_items();
            self.cursor = selected
                .and_then(|id| self.find_pane_by_id(&id))
                .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
            self.collapse_unfocused_section();
            if self.current_pane().is_none() {
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

        let mut items = Vec::new();
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

    fn has_display_snapshot(&self) -> bool {
        self.snapshot_generation > 0 || !self.panes.is_empty() || !self.pending_kills.is_empty()
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
        } else if pane.status == PaneStatus::Busy
            && self.sidebar_config.working != WorkingSection::Inline
        {
            Some(Section::Working)
        } else {
            None
        }
    }

    fn section_is_always_expanded(&self, section: Section) -> bool {
        match section {
            Section::Working => self.sidebar_config.working == WorkingSection::Expand,
            Section::Stashed => self.sidebar_config.stashed == SectionBehavior::Expand,
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

    fn toggle_current_stash(&mut self) -> Option<bool> {
        let previous_cursor = self.cursor;
        let (pane_id, stashed) = {
            let pane = self.current_pane_mut()?;
            pane.stashed = !pane.stashed;
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
        self.cancel_resize();
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
            return Action::Preview;
        }
        if alt && matches!(key.code, KeyCode::Char('>')) {
            self.pending_g = false;
            self.select_cursor(last_pane(&self.items).unwrap_or(0), true);
            self.preview_gen += 1;
            return Action::Preview;
        }

        if key.code == KeyCode::Char('d') {
            if self.pending_d {
                self.pending_d = false;
                self.pending_g = false;
                if let Some(pane_id) = self.remove_current_pane() {
                    let tx = tx.clone();
                    thread::spawn(move || {
                        let err = kill_pane(&pane_id).err().map(|e| e.to_string());
                        let _ = tx.send(Msg::PaneKilled {
                            pane_id: pane_id.to_string(),
                            err,
                        });
                    });
                    return Action::Preview;
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
                return Action::Preview;
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
                Action::Preview
            }
            KeyCode::Char(' ') => {
                let mut changed = None;
                if let Some(p) = self.current_pane_mut() {
                    let unread = match p.status {
                        PaneStatus::Idle => true,
                        PaneStatus::Unread => false,
                        PaneStatus::Busy => return Action::None,
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
            KeyCode::Char('a') => {
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
                    return if stashed {
                        Action::Preview
                    } else {
                        Action::Redraw
                    };
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
                Action::Preview
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
                    Action::Preview
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
                Action::Preview
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
                Action::Preview
            }
            KeyCode::Enter => self.open_current_pane(),
            _ => Action::None,
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
            return Action::Preview;
        }
        if let Some(p) = self.current_pane() {
            let pane_id = p.pane_id.clone();
            // Opening a pane reads it, even if it was force-marked unread,
            // consistent with Space/`a`, which clear a manual Unread too.
            if p.status == PaneStatus::Unread {
                self.pending_unread_changes
                    .insert(pane_id.to_string(), false);
            }
            let _ = switch_to_pane(&pane_id);
        }
        self.save_state();
        Action::Quit
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        let layout = self.resolved_layout();
        let active = self
            .split_interaction
            .active_id()
            .and_then(|id| layout.split(id));
        let response = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => layout
                .divider_at(mouse.row, mouse.column)
                .and_then(|split| self.split_interaction.begin(split, mouse.row, mouse.column)),
            MouseEventKind::Drag(MouseButton::Left) => {
                self.split_interaction
                    .update(active, mouse.row, mouse.column)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.split_interaction
                    .release(active, mouse.row, mouse.column)
            }
            _ => None,
        };
        if let Some(response) = response {
            if response.ended() && response.changed {
                self.save_state();
            }
            return Action::Redraw;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.split_interaction.active_id().is_none()
            && let Some(hit) = self.hits.hit(mouse.row, mouse.column).cloned()
        {
            let (idx, section) = match hit {
                Hit::Pane(id) => (self.find_pane_by_id(&id), None),
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
                return Action::Preview;
            }
            self.select_cursor(idx, false);
            return self.open_current_pane();
        }
        Action::None
    }

    fn cancel_resize(&mut self) {
        if let Some(response) = self.split_interaction.cancel()
            && response.changed
        {
            self.save_state();
        }
    }

    #[cfg(not(test))]
    fn save_state(&mut self) {
        let panes = &self.panes;
        let pending = &self.pending_unread_changes;
        let sidebar_width = self.sidebar_width();
        let sort_mode = self.sort_mode;
        let selected_pane_id = self
            .current_pane()
            .map(|pane| pane.pane_id.to_string())
            .unwrap_or_default();
        if let Ok(state) = update_ui_state(|state| {
            for p in panes.values() {
                if !state.panes.contains_key(p.pane_id.as_str())
                    && let Some(ui) = state.panes.remove(&p.target)
                {
                    state.panes.insert(p.pane_id.to_string(), ui);
                }
            }
            state.panes.retain(|id, _| panes.contains_key(id));
            for p in panes.values() {
                let entry = state.panes.entry(p.pane_id.to_string()).or_default();
                entry.stashed = p.stashed;
                if let Some(unread) = pending.get(p.pane_id.as_str()) {
                    entry.forced_unread = *unread;
                    entry.read_content_hash = (!*unread).then(|| p.content_hash.clone());
                }
            }
            state.panes.retain(|_, ui| !ui_pane_state_is_empty(ui));
            state.selected_pane_id = selected_pane_id.clone();
            state.sidebar_width = sidebar_width;
            state.sort_mode = sort_mode.as_str().to_string();
        }) {
            self.ui_state = state;
            self.pending_unread_changes.clear();
        }
    }

    #[cfg(test)]
    fn save_state(&mut self) {
        self.ui_state.sidebar_width = self.sidebar_width();
        self.ui_state.selected_pane_id = self
            .current_pane()
            .map(|pane| pane.pane_id.to_string())
            .unwrap_or_default();
    }
}

fn render<W: Write>(surface: &mut Surface, app: &mut App, out: &mut W) -> io::Result<()> {
    app.hits.clear();
    surface.set_layout(app.layout());
    surface.set_layout_style(LayoutStyle {
        active_split: app.split_interaction.active_id(),
        ..LayoutStyle::default()
    });
    surface.render(out, |id, slice, _theme| {
        if id == SIDEBAR {
            render_sidebar(slice, app);
        } else if id == PREVIEW {
            render_preview(slice, app);
        }
    })
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
        let (message, style) = if err == SYNCING_MSG {
            (err.clone(), Style::new().fg(Color::DarkGrey))
        } else {
            (format!("Error: {err}"), Style::new().fg(Color::Red))
        };
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
        if list_height > 1 {
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
            TreeItem::SectionHeader(section, _) => Some(Hit::Section(*section)),
            _ => None,
        };
        if let Some(hit) = hit {
            app.hits
                .record_local(origin, Rect::new(row as u16, 0, width, 1), hit);
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
    }
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
            PaneStatus::Busy => Color::Rgb {
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
    let icon = if matches!(p.status, PaneStatus::Idle) {
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
    if app.show_help {
        render_help(slice);
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

fn render_empty_preview(slice: &mut GridSlice<'_>, app: &App) {
    let title = if app.err.as_deref() == Some(SYNCING_MSG) {
        "Looking for sessions"
    } else if !app.panes.is_empty() {
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

fn render_help(slice: &mut GridSlice<'_>) {
    let title = Style::new().fg(Color::White).bold();
    let key = Style::new().fg(Color::Yellow).bold();
    let dim = Style::new().fg(Color::DarkGrey);
    slice.put_str(2, 1, "Keybindings", title);
    let rows = [
        ("j/k", "move down/up"),
        ("h/left", "collapse section"),
        ("C-n/C-p", "move down/up"),
        ("[n]j/k", "move down/up n times"),
        ("enter", "switch to pane"),
        ("space", "toggle read/unread"),
        ("a", "mark all read"),
        ("s/u", "stash/unstash"),
        ("dd", "kill pane"),
        ("gg", "go to first"),
        ("M-<", "go to first"),
        ("G", "go to last"),
        ("M->", "go to last"),
        ("o", "toggle sort order"),
        ("H/L", "resize sidebar"),
        ("drag", "resize sidebar"),
        ("?", "toggle help"),
        ("q/esc", "quit"),
    ];
    for (i, (k, desc)) in rows.iter().enumerate() {
        let y = i as u16 + 3;
        slice.put_str(2, y, &format!("{k:<8}"), key);
        slice.put_str(12, y, desc, dim);
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
    matches!(item, TreeItem::Pane(_) | TreeItem::SectionHeader(_, _))
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
    if matches!(items[from], TreeItem::Pane(_)) {
        return from;
    }
    for offset in 1..items.len() {
        if from >= offset && matches!(items[from - offset], TreeItem::Pane(_)) {
            return from - offset;
        }
        if from + offset < items.len() && matches!(items[from + offset], TreeItem::Pane(_)) {
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
            project_win_width: HashMap::new(),
            width: 0,
            height: 0,
            sidebar: sidebar_split(0),
            split_interaction: SplitInteraction::default(),
            show_help: false,
            pending_d: false,
            pending_g: false,
            sort_mode: SortMode::default(),
            expanded_section: None,
            sidebar_config: SidebarConfig::default(),
            count: 0,
            err: None,
            daemon_err: None,
            ui_state: UiState::default(),
            pending_unread_changes: HashMap::new(),
            pending_kills: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: String::new(),
        };
        app.focus_startup_session();
        app
    }

    #[test]
    fn startup_key_without_session_does_not_leave_preview_pending() {
        let mut app = app_with_panes(Vec::new());
        let (tx, rx) = mpsc::channel();
        assert!(matches!(
            app.handle_key(KeyEvent::from(KeyCode::Char('o')), &tx),
            Action::Preview
        ));
        assert!(!spawn_preview(&tx, &app));
        assert!(rx.try_recv().is_err());
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
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 36)),
            Action::Redraw
        ));
        assert_eq!(app.split_interaction.active_id(), Some(app.sidebar.id()));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 50));
        assert_eq!(
            app.ui_state.sidebar_width, 36,
            "save only at gesture completion"
        );
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 54));
        assert_eq!(
            app.ui_state.sidebar_width, 54,
            "final pointer position is applied"
        );
        assert_eq!(app.split_interaction.active_id(), None);
        assert_eq!(app.cursor, cursor);
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 54));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX));
        app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE), &tx);
        assert_eq!(app.split_interaction.active_id(), None);
        assert_eq!(app.sidebar_width(), 97);
        assert!(matches!(
            app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 30)),
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
        assert!(matches!(app.handle_mouse(mouse), Action::None));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert_eq!(app.preview_gen, generation);
        assert!(matches!(app.handle_mouse(mouse), Action::None));
        assert_eq!(app.preview_gen, generation);
        assert!(matches!(
            app.handle_mouse(MouseEvent {
                column: 80,
                ..mouse
            }),
            Action::None
        ));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
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
        assert!(matches!(app.handle_mouse(mouse), Action::Redraw));
        assert!(app.split_interaction.active_id().is_some());
        assert!(matches!(
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 4,
                row: app.find_pane_by_id("%2").unwrap() as u16,
                ..mouse
            }),
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
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                ..mouse
            }),
            Action::None
        ));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        assert!(matches!(app.handle_mouse(mouse), Action::Preview));
        assert_eq!(app.expanded_section, Some(Section::Stashed));
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%3");
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        assert!(matches!(app.handle_mouse(mouse), Action::Preview));
        assert_eq!(app.expanded_section, None);
        assert_eq!(app.current_pane().unwrap().pane_id.as_str(), "%1");
        render(&mut surface, &mut app, &mut Vec::new()).unwrap();
        let busy_row = app
            .items
            .iter()
            .position(|item| matches!(item, TreeItem::SectionHeader(Section::Working, _)))
            .unwrap() as u16;
        assert!(matches!(
            app.handle_mouse(MouseEvent {
                row: busy_row,
                ..mouse
            }),
            Action::Preview
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
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            }),
            Action::Preview
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
                Action::Preview
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
        app.sidebar_config.working = WorkingSection::Expand;
        app.sidebar_config.stashed = SectionBehavior::Expand;
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
            app.sidebar_config.stashed = behavior;
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
        for working in [
            WorkingSection::Collapse,
            WorkingSection::Expand,
            WorkingSection::Inline,
        ] {
            for stashed_behavior in [SectionBehavior::Collapse, SectionBehavior::Expand] {
                let mut busy = pane("%2", 1);
                busy.status = PaneStatus::Busy;
                let mut stashed = pane("%3", 2);
                stashed.stashed = true;
                let mut app = app_with_panes(vec![pane("%1", 0), busy, stashed]);
                app.sidebar_config = SidebarConfig {
                    working,
                    stashed: stashed_behavior,
                    ..SidebarConfig::default()
                };
                app.focus_startup_session();
                assert_eq!(
                    app.find_pane_by_id("%2").is_some(),
                    working != WorkingSection::Collapse
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
                    working != WorkingSection::Collapse
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
                    working != WorkingSection::Collapse
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

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);

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
        assert_eq!(app.pending_unread_changes.get("%1"), Some(&true));

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        assert_eq!(app.panes["%1"].status, PaneStatus::Idle);
        assert_eq!(app.pending_unread_changes.get("%1"), Some(&false));

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
}
