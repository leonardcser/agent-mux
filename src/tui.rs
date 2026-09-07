use std::collections::{HashMap, HashSet};
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
#[cfg(not(test))]
use crate::agent::persist::{LastPosition, ui_pane_state_is_empty, update_ui_state};
use crate::agent::persist::{
    Snapshot, UiState, apply_ui_state, load_snapshot, load_ui_state, panes_from_snapshot,
};
use crate::agent::{
    Pane, PaneId, PaneStatus, capture_pane, kill_pane, restart_watch, switch_to_pane,
};

const SIDEBAR: PaintId = PaintId(1);
const PREVIEW: PaintId = PaintId(3);
const MIN_SIDEBAR: u16 = 20;
const MIN_PREVIEW: u16 = 20;
const SYNCING_MSG: &str = "syncing agent-mux snapshot";

#[derive(Clone, Debug)]
enum Hit {
    /// A clickable sidebar row, carrying its index into `items`.
    Row(usize),
}

#[derive(Clone, Debug)]
enum TreeItem {
    SectionHeader(Option<String>),
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
}

pub fn run(tmux_session: String) -> Result<()> {
    let mut term = TerminalSession::builder()
        .buffer_capacity(128 * 1024)
        .focus_events(true)
        .enter_stdout()?;
    let (w, h) = term.size()?;
    let mut surface = Surface::new(w, h);

    let mut app = App::new(tmux_session);
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
                    if preview_seq >= app.preview_applied_gen {
                        app.preview_applied_gen = preview_seq;
                        app.preview_for = pane_id;
                        app.preview_lines = parse_ansi_lines(content.trim_end_matches('\n'));
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

        if last_preview.elapsed() >= Duration::from_millis(100) && !preview_pending {
            app.preview_for.clear();
            spawn_preview(&tx, app);
            preview_pending = true;
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
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dirty |= app.split_interaction.active_id().is_some();
                    match app.handle_key(key, &tx) {
                        Action::Quit => return Ok(()),
                        Action::Redraw => dirty = true,
                        Action::Preview => {
                            spawn_preview(&tx, app);
                            preview_pending = true;
                            dirty = true;
                        }
                        Action::LoadPanes => {
                            if !panes_pending {
                                spawn_load_panes(&tx);
                                panes_pending = true;
                            }
                            dirty = true;
                        }
                        Action::None => {}
                    }
                }
                Event::Mouse(mouse) => match app.handle_mouse(mouse) {
                    Action::Quit => return Ok(()),
                    Action::Redraw => dirty = true,
                    _ => {}
                },
                Event::Resize(w, h) => {
                    surface.set_terminal_size(w, h);
                    app.resize(w, h);
                    dirty = true;
                }
                Event::FocusLost => {
                    app.cancel_resize();
                    dirty = true;
                }
                _ => {}
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
        let result = subscribe_panes(&tx);
        if result.is_err() {
            let _ = tx.send(Msg::SubscriptionEnded);
        }
    });
}

fn subscribe_panes(tx: &mpsc::Sender<Msg>) -> Result<()> {
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

fn load_preview(app: &mut App) {
    let Some(p) = app.current_pane() else { return };
    let pane_id = p.pane_id.clone();
    let lines = app.height.max(50) as usize;
    let content = capture_pane(&pane_id, lines).unwrap_or_else(|err| format!("error: {err}"));
    app.preview_for = pane_id.to_string();
    app.preview_applied_gen = app.preview_gen;
    app.preview_lines = parse_ansi_lines(content.trim_end_matches('\n'));
}

fn spawn_preview(tx: &mpsc::Sender<Msg>, app: &App) {
    let Some(p) = app.current_pane() else { return };
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
    LoadPanes,
    Quit,
}

/// How sessions are ordered in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SortMode {
    /// Stable creation order (the default), grouped by folder.
    #[default]
    Order,
    /// Most recently changed first: folders are ordered by their most
    /// recently active session, and sessions within a folder likewise.
    Recent,
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
            "recent" => SortMode::Recent,
            _ => SortMode::Order,
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
    scroll_start: usize,
    preview_for: String,
    preview_lines: Vec<Line<'static>>,
    preview_gen: u64,
    preview_applied_gen: u64,
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
    count: usize,
    err: Option<String>,
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
    fn new(tmux_session: String) -> Self {
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
            scroll_start: 0,
            preview_for: String::new(),
            preview_lines: Vec::new(),
            preview_gen: 1,
            preview_applied_gen: 0,
            snapshot_generation,
            project_win_width: HashMap::new(),
            width: 0,
            height: 0,
            sidebar: sidebar_split(ui_state.sidebar_width),
            split_interaction: SplitInteraction::default(),
            show_help: false,
            pending_d: false,
            pending_g: false,
            sort_mode: SortMode::from_str(&ui_state.sort_mode),
            count: 0,
            err: snapshot.is_none().then(|| SYNCING_MSG.to_string()),
            ui_state,
            pending_unread_changes: HashMap::new(),
            pending_kills: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: tmux_session,
        };
        app.rebuild_items();
        if let Some(att) = app.first_unread_pane() {
            app.cursor = att;
        } else if !app.ui_state.last_position.pane_id.is_empty()
            || !app.ui_state.last_position.pane_target.is_empty()
        {
            let id = if app.ui_state.last_position.pane_id.is_empty() {
                app.ui_state.last_position.pane_target.clone()
            } else {
                app.ui_state.last_position.pane_id.clone()
            };
            app.cursor = app
                .find_pane_by_id(&id)
                .unwrap_or_else(|| first_pane(&app.items).unwrap_or(0));
            app.scroll_start = app.ui_state.last_position.scroll_start;
        } else {
            app.cursor = first_pane(&app.items).unwrap_or(0);
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

    fn replace_panes(&mut self, panes: Vec<Pane>) {
        let selected = self.current_pane().map(|p| p.pane_id.to_string());
        self.panes = panes
            .into_iter()
            .map(|p| (p.pane_id.to_string(), p))
            .collect();
        self.rebuild_items();
        self.cursor = selected
            .and_then(|id| self.find_pane_by_id(&id))
            .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
        if self.current_pane().is_none() {
            self.preview_for.clear();
            self.preview_lines.clear();
            self.preview_gen += 1;
        }
    }

    fn rebuild_items(&mut self) {
        let panes: Vec<&Pane> = self.panes.values().collect();
        let mut grouped_projects = HashSet::new();
        for p in &panes {
            if !p.project_root.is_empty() && p.path != p.project_root {
                grouped_projects.insert(p.project_root.clone());
            }
        }

        let mut project_win_width: HashMap<String, usize> = HashMap::new();
        for p in &panes {
            if grouped_projects.contains(&p.project_root) {
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
        for stashed in [false, true] {
            let mut groups: Vec<Group<'_>> = Vec::new();
            let mut group_index: HashMap<GroupKey, usize> = HashMap::new();
            for p in panes.iter().copied().filter(|p| p.stashed == stashed) {
                let key = if grouped_projects.contains(&p.project_root) {
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

            if groups.is_empty() {
                continue;
            }
            if stashed {
                items.push(TreeItem::SectionHeader(None));
                items.push(TreeItem::SectionHeader(Some("stashed".into())));
            }

            // Most recent change first: the newest last_active in a group.
            let group_recency = |g: &Group<'_>| g.panes.iter().filter_map(|p| p.last_active).max();
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
                        b.last_active
                            .cmp(&a.last_active)
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

    fn first_unread_pane(&self) -> Option<usize> {
        self.items.iter().enumerate().find_map(|(i, it)| {
            let TreeItem::Pane(id) = it else { return None };
            let p = self.panes.get(id)?;
            (!p.stashed && p.status == PaneStatus::Unread).then_some(i)
        })
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
        self.cursor = nearest_pane(&self.items, self.cursor);
        if self.preview_for == pane_key {
            self.preview_for.clear();
            self.preview_lines.clear();
        }
        self.preview_gen += 1;
        Some(pane_id)
    }

    fn restore_pending_kill(&mut self, pane_id: &str) {
        let Some(pane) = self.pending_kills.remove(pane_id) else {
            return;
        };
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

    fn toggle_current_stash(&mut self) -> Option<bool> {
        let previous_cursor = self.cursor;
        let (pane_id, stashed) = {
            let pane = self.current_pane_mut()?;
            pane.stashed = !pane.stashed;
            (pane.pane_id.to_string(), pane.stashed)
        };

        self.rebuild_items();
        self.cursor = if stashed {
            nearest_pane(&self.items, previous_cursor)
        } else {
            self.find_pane_by_id(&pane_id)
                .unwrap_or_else(|| nearest_pane(&self.items, previous_cursor))
        };
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
            self.cursor = first_pane(&self.items).unwrap_or(0);
            self.preview_gen += 1;
            return Action::Preview;
        }
        if alt && matches!(key.code, KeyCode::Char('>')) {
            self.pending_g = false;
            self.cursor = last_pane(&self.items).unwrap_or(0);
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
                self.cursor = first_pane(&self.items).unwrap_or(0);
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
                self.cursor = last_pane(&self.items).unwrap_or(0);
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
                let mut selected = None;
                if let Some(p) = self.current_pane_mut()
                    && p.stashed
                {
                    p.stashed = false;
                    selected = Some(p.pane_id.to_string());
                }
                if let Some(id) = selected {
                    self.rebuild_items();
                    self.cursor = self
                        .find_pane_by_id(&id)
                        .unwrap_or_else(|| nearest_pane(&self.items, self.cursor));
                    self.save_state();
                    return Action::Redraw;
                }
                Action::None
            }
            KeyCode::Char('R') => {
                let _ = restart_watch();
                Action::LoadPanes
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
            KeyCode::Char('j') | KeyCode::Down => {
                for _ in 0..count {
                    let next = next_pane(&self.items, self.cursor);
                    if next == self.cursor {
                        break;
                    }
                    self.cursor = next;
                }
                self.preview_gen += 1;
                Action::Preview
            }
            KeyCode::Char('k') | KeyCode::Up => {
                for _ in 0..count {
                    let prev = prev_pane(&self.items, self.cursor);
                    if prev == self.cursor {
                        break;
                    }
                    self.cursor = prev;
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
        // Clicking a row selects it and opens it, like pressing Enter.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(Hit::Row(idx)) = self.hits.hit(mouse.row, mouse.column).cloned()
        {
            self.cursor = idx;
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
        let mut cursor = self.cursor;
        let mut scroll_start = self.scroll_start;
        if let Some(att) = self.first_unread_pane() {
            cursor = att;
            scroll_start = 0;
        }
        let (pane_id, pane_target) = self
            .items
            .get(cursor)
            .and_then(|it| match it {
                TreeItem::Pane(id) => self.panes.get(id),
                _ => None,
            })
            .map(|p| (p.pane_id.to_string(), p.target.clone()))
            .unwrap_or_default();
        let pane_ids: std::collections::HashMap<String, bool> = self
            .items
            .iter()
            .filter_map(|it| match it {
                TreeItem::Pane(id) => Some((id.clone(), true)),
                _ => None,
            })
            .collect();
        let panes: Vec<Pane> = pane_ids
            .keys()
            .filter_map(|id| self.panes.get(id).cloned())
            .collect();
        let pending = self.pending_unread_changes.clone();
        let sidebar_width = self.sidebar_width();
        let sort_mode = self.sort_mode;
        if update_ui_state(|state| {
            for p in &panes {
                if !state.panes.contains_key(p.pane_id.as_str())
                    && let Some(ui) = state.panes.remove(&p.target)
                {
                    state.panes.insert(p.pane_id.to_string(), ui);
                }
            }
            state.panes.retain(|id, _| pane_ids.contains_key(id));
            for p in &panes {
                let entry = state.panes.entry(p.pane_id.to_string()).or_default();
                entry.stashed = p.stashed;
                if let Some(unread) = pending.get(p.pane_id.as_str()) {
                    entry.forced_unread = *unread;
                    entry.read_content_hash = (!*unread).then(|| p.content_hash.clone());
                }
            }
            state.panes.retain(|_, ui| !ui_pane_state_is_empty(ui));
            state.last_position = LastPosition {
                pane_id: pane_id.clone(),
                pane_target: pane_target.clone(),
                cursor,
                scroll_start,
            };
            state.sidebar_width = sidebar_width;
            state.sort_mode = sort_mode.as_str().to_string();
        })
        .is_ok()
        {
            self.ui_state = load_ui_state();
            self.pending_unread_changes.clear();
        }
    }

    #[cfg(test)]
    fn save_state(&mut self) {
        self.ui_state.sidebar_width = self.sidebar_width();
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
    if let Some(err) = &app.err {
        let (message, style) = if err == SYNCING_MSG {
            (err.clone(), Style::new().fg(Color::DarkGrey))
        } else {
            (format!("Error: {err}"), Style::new().fg(Color::Red))
        };
        slice.put_str(0, 0, &message, style);
        return;
    }
    if app.items.is_empty() {
        slice.put_str(2, 1, "No active sessions", Style::new().fg(Color::DarkGrey));
        return;
    }
    let h = slice.height() as usize;
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
        // Register the row as a click target so a click opens it like Enter.
        if matches!(app.items[idx], TreeItem::Pane(_)) {
            app.hits
                .record_local(origin, Rect::new(row as u16, 0, width, 1), Hit::Row(idx));
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
        TreeItem::SectionHeader(None) => {}
        TreeItem::SectionHeader(Some(title)) => {
            let label = format!(" {title} ");
            let mut text = format!("─{label}");
            let fill = width.saturating_sub(display_width(&text) as u16);
            text.push_str(&"─".repeat(fill as usize));
            slice.put_str(0, row, &text, Style::new().fg(Color::AnsiValue(242)).dim());
        }
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
    } else {
        "No active sessions"
    };
    let detail = "Start a supported agent in tmux and it will appear here.";
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
        ("R", "reload watch"),
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

fn first_pane(items: &[TreeItem]) -> Option<usize> {
    items.iter().position(|it| matches!(it, TreeItem::Pane(_)))
}

fn last_pane(items: &[TreeItem]) -> Option<usize> {
    items.iter().rposition(|it| matches!(it, TreeItem::Pane(_)))
}

fn next_pane(items: &[TreeItem], from: usize) -> usize {
    for (i, item) in items.iter().enumerate().skip(from + 1) {
        if matches!(item, TreeItem::Pane(_)) {
            return i;
        }
    }
    for (i, item) in items.iter().enumerate().take(from.min(items.len())) {
        if matches!(item, TreeItem::Pane(_)) {
            return i;
        }
    }
    from
}

fn prev_pane(items: &[TreeItem], from: usize) -> usize {
    for i in (0..from).rev() {
        if matches!(items[i], TreeItem::Pane(_)) {
            return i;
        }
    }
    for i in ((from + 1)..items.len()).rev() {
        if matches!(items[i], TreeItem::Pane(_)) {
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
    0
}

#[cfg(test)]
mod tests {
    use super::*;

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
            scroll_start: 0,
            preview_for: String::new(),
            preview_lines: Vec::new(),
            preview_gen: 0,
            preview_applied_gen: 0,
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
            count: 0,
            err: None,
            ui_state: UiState::default(),
            pending_unread_changes: HashMap::new(),
            pending_kills: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: String::new(),
        };
        app.rebuild_items();
        app
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
    fn stashing_keeps_cursor_row_instead_of_following_pane() {
        let mut app = app_with_panes(vec![pane("%1", 0), pane("%2", 1), pane("%3", 2)]);
        app.cursor = app.find_pane_by_id("%2").unwrap();
        let previous_cursor = app.cursor;

        assert_eq!(app.toggle_current_stash(), Some(true));

        assert_eq!(app.cursor, previous_cursor);
        assert_eq!(
            app.current_pane().map(|pane| pane.pane_id.as_str()),
            Some("%3")
        );
        assert!(app.panes["%2"].stashed);
    }

    fn pane_at(id: &str, order: usize, path: &str, last_active_secs: i64) -> Pane {
        Pane {
            target: format!("session:1.{order}"),
            path: path.to_string(),
            order,
            last_active: chrono::DateTime::from_timestamp(last_active_secs, 0),
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
    fn recent_sort_orders_folders_and_sessions_by_last_active() {
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_panes(vec![
            pane_at("%1", 0, "/a", 100),
            pane_at("%2", 1, "/a", 300),
            pane_at("%3", 2, "/b", 500),
        ]);

        // Default order: grouped by folder, in creation order.
        assert_eq!(app.sort_mode, SortMode::Order);
        assert_eq!(pane_ids(&app), ["%1", "%2", "%3"]);

        // Toggle to recent: folder /b (500) sorts ahead of /a (300), and the
        // newer session within /a (300) sorts ahead of the older one (100).
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
        assert_eq!(app.sort_mode, SortMode::Recent);
        assert_eq!(pane_ids(&app), ["%3", "%2", "%1"]);

        // Toggling again returns to the stable order.
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
        assert_eq!(app.sort_mode, SortMode::Order);
        assert_eq!(pane_ids(&app), ["%1", "%2", "%3"]);
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
