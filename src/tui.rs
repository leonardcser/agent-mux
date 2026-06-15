use std::collections::HashMap;
use std::io::{self, BufWriter, Stdout, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use smelt_ansi::{AnsiSpan, parse_ansi_lines};
use smelt_term::grid::{Color, GridSlice, Style};
use smelt_term::{Constraint, HitRegistry, LayoutTree, PaintId, Surface};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::git::enrich_panes;
use crate::agent::persist::{
    LastPosition, State, cache_panes, has_status_override, load_state, panes_from_state,
    update_state,
};
use crate::agent::{
    Pane, PaneStatus, capture_pane, kill_pane, list_panes_basic, restart_watch, switch_to_pane,
};

const SIDEBAR: PaintId = PaintId(1);
const SEPARATOR: PaintId = PaintId(2);
const PREVIEW: PaintId = PaintId(3);
const MIN_SIDEBAR: u16 = 20;
const MIN_PREVIEW: u16 = 20;

#[derive(Clone, Debug)]
enum Hit {
    Separator,
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
        state: Option<State>,
        err: Option<String>,
    },
    PreviewLoaded {
        pane_id: String,
        content: String,
        preview_seq: u64,
    },
    PaneKilled {
        err: Option<String>,
    },
}

pub fn run(tmux_session: String) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let mut writer = BufWriter::with_capacity(128 * 1024, stdout);
    let (w, h) = crossterm::terminal::size()?;
    let mut surface = Surface::new(w, h);

    let mut app = App::new(tmux_session);
    app.resize(w, h);
    let result = run_loop(&mut surface, &mut writer, &mut app);

    disable_raw_mode()?;
    execute!(
        writer,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    writer.flush()?;
    result.map_err(Into::into)
}

fn run_loop(
    surface: &mut Surface,
    writer: &mut BufWriter<Stdout>,
    app: &mut App,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_millis(33);
    let mut last_panes = Instant::now() - Duration::from_millis(500);
    let mut last_preview = Instant::now() - Duration::from_millis(200);
    let mut panes_pending = false;
    let mut preview_pending = true;

    spawn_preview(&tx, app);

    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::PanesLoaded { panes, state, err } => {
                    panes_pending = false;
                    if let Some(err) = err {
                        app.err = Some(err);
                    } else {
                        app.err = None;
                        if let Some(state) = state {
                            app.state = state;
                        }
                        app.replace_panes(panes);
                    }
                    dirty = true;
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
                Msg::PaneKilled { err } => {
                    if let Some(err) = err {
                        app.err = Some(err);
                    }
                    spawn_load_panes(&tx);
                    panes_pending = true;
                    dirty = true;
                }
            }
        }

        if last_panes.elapsed() >= Duration::from_millis(500) && !panes_pending {
            spawn_load_panes(&tx);
            panes_pending = true;
            last_panes = Instant::now();
        }

        if last_preview.elapsed() >= Duration::from_millis(200) && !preview_pending {
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
                Event::Mouse(mouse) => {
                    if app.handle_mouse(mouse) {
                        dirty = true;
                    }
                }
                Event::Resize(w, h) => {
                    surface.set_terminal_size(w, h);
                    app.resize(w, h);
                    dirty = true;
                }
                _ => {}
            }
        }
    }
}

fn spawn_load_panes(tx: &mpsc::Sender<Msg>) {
    let tx = tx.clone();
    thread::spawn(move || {
        let state = load_state();
        if let Some(state_ref) = state.as_ref()
            && !state_ref.panes.is_empty()
        {
            let mut panes = panes_from_state(state_ref);
            enrich_panes(&mut panes);
            let _ = tx.send(Msg::PanesLoaded {
                panes,
                state,
                err: None,
            });
            return;
        }
        match list_panes_basic() {
            Ok(mut panes) => {
                enrich_panes(&mut panes);
                let _ = tx.send(Msg::PanesLoaded {
                    panes,
                    state,
                    err: None,
                });
            }
            Err(err) => {
                let _ = tx.send(Msg::PanesLoaded {
                    panes: Vec::new(),
                    state,
                    err: Some(err.to_string()),
                });
            }
        }
    });
}

fn spawn_preview(tx: &mpsc::Sender<Msg>, app: &App) {
    let Some(p) = app.current_pane() else { return };
    let target = p.target.clone();
    let pane_id = p.pane_id.clone();
    let lines = app.height.max(50) as usize;
    let preview_seq = app.preview_gen;
    let tx = tx.clone();
    thread::spawn(move || {
        let content = capture_pane(&target, lines).unwrap_or_else(|err| format!("error: {err}"));
        let _ = tx.send(Msg::PreviewLoaded {
            pane_id,
            content,
            preview_seq,
        });
    });
}

#[derive(Debug)]
enum Action {
    None,
    Redraw,
    Preview,
    LoadPanes,
    Quit,
}

struct App {
    panes: HashMap<String, Pane>,
    items: Vec<TreeItem>,
    cursor: usize,
    scroll_start: usize,
    preview_for: String,
    preview_lines: Vec<Vec<AnsiSpan>>,
    preview_gen: u64,
    preview_applied_gen: u64,
    project_win_width: HashMap<String, usize>,
    width: u16,
    height: u16,
    sidebar_width: u16,
    dragging: bool,
    show_help: bool,
    pending_d: bool,
    pending_g: bool,
    count: usize,
    err: Option<String>,
    state: State,
    pending_overrides: HashMap<String, PaneStatus>,
    hits: HitRegistry<Hit>,
    _tmux_session: String,
}

impl App {
    fn new(tmux_session: String) -> Self {
        let state = load_state().unwrap_or_default();
        let mut panes = if !state.panes.is_empty() {
            panes_from_state(&state)
        } else {
            list_panes_basic().unwrap_or_default()
        };
        enrich_panes(&mut panes);
        let mut app = Self {
            panes: panes.into_iter().map(|p| (p.pane_id.clone(), p)).collect(),
            items: Vec::new(),
            cursor: 0,
            scroll_start: 0,
            preview_for: String::new(),
            preview_lines: Vec::new(),
            preview_gen: 1,
            preview_applied_gen: 0,
            project_win_width: HashMap::new(),
            width: 0,
            height: 0,
            sidebar_width: state.sidebar_width,
            dragging: false,
            show_help: false,
            pending_d: false,
            pending_g: false,
            count: 0,
            err: None,
            state,
            pending_overrides: HashMap::new(),
            hits: HitRegistry::new(),
            _tmux_session: tmux_session,
        };
        app.rebuild_items();
        if let Some(att) = app.first_attention_pane() {
            app.cursor = att;
        } else if !app.state.last_position.pane_id.is_empty()
            || !app.state.last_position.pane_target.is_empty()
        {
            let id = if app.state.last_position.pane_id.is_empty() {
                app.state.last_position.pane_target.clone()
            } else {
                app.state.last_position.pane_id.clone()
            };
            app.cursor = app
                .find_pane_by_id(&id)
                .unwrap_or_else(|| first_pane(&app.items).unwrap_or(0));
            app.scroll_start = app.state.last_position.scroll_start;
        } else {
            app.cursor = first_pane(&app.items).unwrap_or(0);
        }
        app
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        if self.sidebar_width == 0 {
            self.sidebar_width = (width / 4).max(MIN_SIDEBAR);
        }
        self.sidebar_width = self
            .sidebar_width
            .clamp(MIN_SIDEBAR, width.saturating_sub(MIN_PREVIEW));
    }

    fn replace_panes(&mut self, panes: Vec<Pane>) {
        self.panes = panes.into_iter().map(|p| (p.pane_id.clone(), p)).collect();
        self.rebuild_items();
        self.cursor = nearest_pane(&self.items, self.cursor);
    }

    fn rebuild_items(&mut self) {
        let mut sorted: Vec<&Pane> = self.panes.values().collect();
        let mut grouped_projects = HashMap::new();
        for p in &sorted {
            if !p.project_root.is_empty() && p.path != p.project_root {
                grouped_projects.insert(p.project_root.clone(), true);
            }
        }
        sorted.sort_by(|a, b| {
            a.stashed
                .cmp(&b.stashed)
                .then(a.order.cmp(&b.order))
                .then(a.target.cmp(&b.target))
        });

        let mut project_win_width: HashMap<String, usize> = HashMap::new();
        for p in &sorted {
            if grouped_projects.contains_key(&p.project_root) {
                let label = pane_label(p);
                let width = display_width(&label);
                project_win_width
                    .entry(p.project_root.clone())
                    .and_modify(|current| *current = (*current).max(width))
                    .or_insert(width);
            }
        }
        self.project_win_width = project_win_width;

        let mut items = Vec::new();
        let mut prev_path = String::new();
        let mut prev_project = String::new();
        let mut in_stashed = false;
        for p in sorted {
            if p.stashed && !in_stashed {
                in_stashed = true;
                items.push(TreeItem::SectionHeader(None));
                items.push(TreeItem::SectionHeader(Some("stashed".into())));
                prev_path.clear();
                prev_project.clear();
            }
            if grouped_projects.contains_key(&p.project_root) {
                if p.project_root != prev_project {
                    items.push(TreeItem::ProjectGroup(p.pane_id.clone()));
                    prev_project = p.project_root.clone();
                }
                items.push(TreeItem::Pane(p.pane_id.clone()));
            } else {
                if p.path != prev_path {
                    items.push(TreeItem::Workspace(p.pane_id.clone()));
                    prev_path = p.path.clone();
                }
                items.push(TreeItem::Pane(p.pane_id.clone()));
                prev_project.clear();
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

    fn first_attention_pane(&self) -> Option<usize> {
        self.items.iter().enumerate().find_map(|(i, it)| {
            let TreeItem::Pane(id) = it else { return None };
            let p = self.panes.get(id)?;
            (!p.stashed && matches!(p.status, PaneStatus::NeedsAttention | PaneStatus::Unread))
                .then_some(i)
        })
    }

    fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::Sender<Msg>) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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

        if key.code == KeyCode::Char('d') {
            if self.pending_d {
                self.pending_d = false;
                self.pending_g = false;
                if let Some(p) = self.current_pane() {
                    let target = p.target.clone();
                    let tx = tx.clone();
                    thread::spawn(move || {
                        let err = kill_pane(&target).err().map(|e| e.to_string());
                        let _ = tx.send(Msg::PaneKilled { err });
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
                    match p.status {
                        PaneStatus::Idle => p.status = PaneStatus::Unread,
                        PaneStatus::NeedsAttention | PaneStatus::Unread => {
                            p.status = PaneStatus::Idle
                        }
                        PaneStatus::Busy => return Action::None,
                    }
                    changed = Some((p.pane_id.clone(), p.status));
                }
                if let Some((id, status)) = changed {
                    self.pending_overrides.insert(id, status);
                    self.save_state();
                }
                Action::Redraw
            }
            KeyCode::Char('s') => {
                if let Some(p) = self.current_pane_mut() {
                    p.stashed = !p.stashed;
                    self.rebuild_items();
                    self.cursor = nearest_pane(&self.items, self.cursor);
                    self.save_state();
                }
                Action::Redraw
            }
            KeyCode::Char('u') => {
                if let Some(p) = self.current_pane_mut()
                    && p.stashed
                {
                    p.stashed = false;
                    self.rebuild_items();
                    self.cursor = nearest_pane(&self.items, self.cursor);
                    self.save_state();
                    return Action::Redraw;
                }
                Action::None
            }
            KeyCode::Char('R') => {
                let _ = restart_watch();
                Action::LoadPanes
            }
            KeyCode::Char('H') => {
                self.sidebar_width = self
                    .sidebar_width
                    .saturating_sub((2 * count) as u16)
                    .max(MIN_SIDEBAR);
                self.resize(self.width, self.height);
                Action::Redraw
            }
            KeyCode::Char('L') => {
                self.sidebar_width = self.sidebar_width.saturating_add((2 * count) as u16);
                self.resize(self.width, self.height);
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
            KeyCode::Enter => {
                if let Some(p) = self.current_pane() {
                    let pane_id = p.pane_id.clone();
                    let target = p.target.clone();
                    let was_unread = p.status == PaneStatus::Unread
                        && !has_status_override(&self.state, &pane_id);
                    if was_unread {
                        self.pending_overrides.insert(pane_id, PaneStatus::Idle);
                    }
                    let _ = switch_to_pane(&target);
                }
                self.save_state();
                Action::Quit
            }
            _ => Action::None,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if matches!(self.hits.hit(mouse.row, mouse.column), Some(Hit::Separator)) {
                    self.dragging = true;
                    return true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging {
                    self.sidebar_width = mouse
                        .column
                        .clamp(MIN_SIDEBAR, self.width.saturating_sub(MIN_PREVIEW));
                    return true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.dragging {
                    self.dragging = false;
                    self.save_state();
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    fn save_state(&mut self) {
        let mut cursor = self.cursor;
        let mut scroll_start = self.scroll_start;
        if let Some(att) = self.first_attention_pane() {
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
            .map(|p| (p.pane_id.clone(), p.target.clone()))
            .unwrap_or_default();
        let panes: Vec<Pane> = self.panes.values().cloned().collect();
        let pending = self.pending_overrides.clone();
        let sidebar_width = self.sidebar_width;
        let _ = update_state(|state| {
            if state.panes.is_empty() {
                state.panes = cache_panes(&panes);
            }
            for cp in &mut state.panes {
                let id = cp.pane_key().to_string();
                if let Some(p) = panes.iter().find(|p| p.pane_id == id) {
                    cp.stashed = p.stashed;
                }
                if let Some(status) = pending.get(&id) {
                    cp.status_override = Some(status.as_i32());
                    cp.last_status = Some(status.as_i32());
                    if let Some(p) = panes.iter().find(|p| p.pane_id == id) {
                        cp.content_hash = p.content_hash.clone();
                    }
                }
            }
            state.last_position = LastPosition {
                pane_id: pane_id.clone(),
                pane_target: pane_target.clone(),
                cursor,
                scroll_start,
            };
            state.sidebar_width = sidebar_width;
            self.state = state.clone();
        });
        self.pending_overrides.clear();
    }
}

fn render<W: Write>(surface: &mut Surface, app: &mut App, out: &mut W) -> io::Result<()> {
    app.hits.clear();
    surface.set_layout(LayoutTree::hbox(vec![
        (
            Constraint::Length(app.sidebar_width),
            LayoutTree::leaf(SIDEBAR),
        ),
        (Constraint::Length(1), LayoutTree::leaf(SEPARATOR)),
        (Constraint::Fill, LayoutTree::leaf(PREVIEW)),
    ]));
    surface.render(out, |id, slice, _theme| {
        if id == SIDEBAR {
            render_sidebar(slice, app);
        } else if id == SEPARATOR {
            render_separator(slice, app);
        } else if id == PREVIEW {
            render_preview(slice, app);
        }
    })
}

fn render_separator(slice: &mut GridSlice<'_>, app: &mut App) {
    let style = Style::new().fg(if app.dragging {
        Color::Grey
    } else {
        Color::DarkGrey
    });
    for y in 0..slice.height() {
        slice.set(0, y, '│', style);
    }
    app.hits.record(slice.screen_rect(), Hit::Separator);
}

fn render_sidebar(slice: &mut GridSlice<'_>, app: &App) {
    if let Some(err) = &app.err {
        put_clipped(
            slice,
            0,
            0,
            &format!("Error: {err}"),
            Style::new().fg(Color::Red),
        );
        return;
    }
    if app.items.is_empty() {
        put_clipped(
            slice,
            2,
            1,
            "No active sessions",
            Style::new().fg(Color::DarkGrey),
        );
        return;
    }
    let h = slice.height() as usize;
    let start = visible_start(app.items.len(), app.cursor, h);
    let end = (start + h).min(app.items.len());
    for (row, idx) in (start..end).enumerate() {
        render_tree_item(
            slice,
            row as u16,
            slice.width(),
            &app.items[idx],
            idx == app.cursor,
            app,
        );
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
            put_clipped(
                slice,
                0,
                row,
                &text,
                Style::new().fg(Color::AnsiValue(242)).dim(),
            );
        }
        TreeItem::Workspace(id) => {
            if let Some(p) = app.panes.get(id) {
                render_header_row(
                    slice,
                    row,
                    width,
                    &p.short_path,
                    &p.git_branch,
                    p.git_dirty,
                    Style::new().fg(Color::White).bold(),
                    Style::new().fg(Color::Green),
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
                    name,
                    &p.project_branch,
                    p.project_dirty,
                    Style::new().fg(Color::White).bold(),
                    Style::new().fg(Color::Green),
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

fn render_header_row(
    slice: &mut GridSlice<'_>,
    row: u16,
    width: u16,
    name: &str,
    branch: &str,
    dirty: bool,
    style: Style,
    branch_style: Style,
) {
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
    let mut col = put_clipped(slice, 0, row, " ", style);
    col = put_clipped(slice, col, row, &name, style);
    if !branch.is_empty() {
        let pad = width
            .saturating_sub(col)
            .saturating_sub(display_width(&branch) as u16)
            .saturating_sub(1);
        fill_spaces(slice, col, row, pad, style);
        col += pad;
        col = put_clipped(slice, col, row, &branch, branch_style);
        let _ = put_clipped(slice, col, row, " ", branch_style);
    } else {
        fill_spaces(slice, col, row, width.saturating_sub(col), style);
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
    fill_spaces(slice, 0, row, width, fill_style);

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

    let icon_color = match p.status {
        PaneStatus::Busy => Color::Rgb {
            r: 217,
            g: 119,
            b: 6,
        },
        PaneStatus::NeedsAttention | PaneStatus::Unread => Color::Rgb {
            r: 155,
            g: 155,
            b: 245,
        },
        PaneStatus::Idle if selected => Color::White,
        PaneStatus::Idle if p.stashed => Color::AnsiValue(242),
        PaneStatus::Idle => Color::DarkGrey,
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
    col = put_clipped(
        slice,
        col,
        row,
        PREFIX,
        if selected { selected_style } else { dim_style },
    );
    slice.set(col, row, icon, fill_style.fg(icon_color));
    col += 1;
    col = put_clipped(slice, col, row, " ", fill_style);
    col = put_clipped(slice, col, row, &win_label, text_style);
    if !worktree_rendered.is_empty() {
        col = put_clipped(slice, col, row, &worktree_rendered, dim_style);
    }
    col = put_clipped(slice, col, row, &" ".repeat(gap), dim_style);
    let _ = put_clipped(slice, col, row, &elapsed, dim_style);
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
    if app.preview_lines.is_empty() {
        put_clipped(
            slice,
            1,
            1,
            "loading preview…",
            Style::new().fg(Color::DarkGrey),
        );
        return;
    }
    let h = slice.height() as usize;
    let start = app.preview_lines.len().saturating_sub(h);
    for (row, line) in app.preview_lines.iter().skip(start).take(h).enumerate() {
        put_ansi_spans(slice, 0, row as u16, line);
    }
}

fn put_ansi_spans(slice: &mut GridSlice<'_>, mut x: u16, y: u16, spans: &[AnsiSpan]) -> u16 {
    for span in spans {
        x = put_clipped(slice, x, y, &span.text, span.style);
        if x >= slice.width() {
            break;
        }
    }
    x
}

fn render_help(slice: &mut GridSlice<'_>) {
    let title = Style::new().fg(Color::White).bold();
    let key = Style::new().fg(Color::Yellow).bold();
    let dim = Style::new().fg(Color::DarkGrey);
    put_clipped(slice, 2, 1, "Keybindings", title);
    let rows = [
        ("j/k", "move down/up"),
        ("[n]j/k", "move down/up n times"),
        ("enter", "switch to pane"),
        ("space", "toggle attention"),
        ("s/u", "stash/unstash"),
        ("dd", "kill pane"),
        ("gg", "go to first"),
        ("G", "go to last"),
        ("R", "reload watch"),
        ("H/L", "resize sidebar"),
        ("drag", "resize sidebar"),
        ("?", "toggle help"),
        ("q/esc", "quit"),
    ];
    for (i, (k, desc)) in rows.iter().enumerate() {
        let y = i as u16 + 3;
        put_clipped(slice, 2, y, &format!("{k:<8}"), key);
        put_clipped(slice, 12, y, desc, dim);
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
        "ralph" => Color::Rgb {
            r: 244,
            g: 63,
            b: 94,
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
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn put_clipped(slice: &mut GridSlice<'_>, mut x: u16, y: u16, text: &str, style: Style) -> u16 {
    for ch in text.chars() {
        let w = ch.width().unwrap_or(1).max(1) as u16;
        if x + w > slice.width() || y >= slice.height() {
            break;
        }
        slice.set(x, y, ch, style);
        x += w;
    }
    x
}

fn fill_spaces(slice: &mut GridSlice<'_>, x: u16, y: u16, width: u16, style: Style) {
    for col in x..x.saturating_add(width).min(slice.width()) {
        slice.set(col, y, ' ', style);
    }
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn truncate_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(1).max(1);
        if width + w > max {
            break;
        }
        out.push(ch);
        width += w;
    }
    out
}

fn visible_start(len: usize, cursor: usize, height: usize) -> usize {
    if len <= height {
        0
    } else if cursor < height / 2 {
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
    for i in from + 1..items.len() {
        if matches!(items[i], TreeItem::Pane(_)) {
            return i;
        }
    }
    for i in 0..from.min(items.len()) {
        if matches!(items[i], TreeItem::Pane(_)) {
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
