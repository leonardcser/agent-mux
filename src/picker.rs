//! A list of a project's worktrees. The chosen one becomes the directory a key
//! action runs in.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use smelt_term::grid::{Color, GridSlice, Style};

use crate::actions::{Action, Target};
use crate::agent::git::Worktree;

pub struct Picker {
    action: Action,
    target: Target,
    worktrees: Vec<Worktree>,
    query: String,
    /// Indexes into `worktrees` of the entries matching `query`, best first.
    matches: Vec<usize>,
    /// Position in `matches`.
    selected: usize,
}

/// What a key press did to the picker.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Cancel,
    Choose(Worktree),
}

impl Picker {
    /// Starts on the worktree the target is already in.
    pub fn new(action: Action, target: Target, worktrees: Vec<Worktree>) -> Self {
        let mut picker = Self {
            action,
            target,
            worktrees,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    pub fn into_parts(self, worktree: &Worktree) -> (Action, Target) {
        let target = self.target.in_worktree(worktree);
        (self.action, target)
    }

    /// Typing filters the list; the best match is always selected.
    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc if self.query.is_empty() => Outcome::Cancel,
            KeyCode::Esc => self.edit(|query| query.clear()),
            KeyCode::Char('c' | 'd') if ctrl => Outcome::Cancel,
            KeyCode::Char('u') if ctrl => self.edit(|query| query.clear()),
            KeyCode::Char('n' | 'j') if ctrl => self.select(self.selected.saturating_add(1)),
            KeyCode::Char('p' | 'k') if ctrl => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down => self.select(self.selected.saturating_add(1)),
            KeyCode::Up => self.select(self.selected.saturating_sub(1)),
            KeyCode::Backspace => self.edit(|query| {
                query.pop();
            }),
            KeyCode::Enter => match self.matches.get(self.selected) {
                Some(&index) => Outcome::Choose(self.worktrees[index].clone()),
                None => Outcome::Pending,
            },
            KeyCode::Char(ch) if !ctrl && !alt => self.edit(|query| query.push(ch)),
            _ => Outcome::Pending,
        }
    }

    fn edit(&mut self, change: impl FnOnce(&mut String)) -> Outcome {
        change(&mut self.query);
        self.refilter();
        Outcome::Pending
    }

    fn select(&mut self, index: usize) -> Outcome {
        self.selected = index.min(self.matches.len().saturating_sub(1));
        Outcome::Pending
    }

    /// Without a query every worktree is listed in git order with the current
    /// one selected; with one, the matches are ranked and the best is selected.
    fn refilter(&mut self) {
        if self.query.is_empty() {
            self.matches = (0..self.worktrees.len()).collect();
            self.selected = self
                .worktrees
                .iter()
                .position(|worktree| worktree.path == self.target.path)
                .unwrap_or(0);
            return;
        }
        let mut scored: Vec<(i32, usize)> = self
            .worktrees
            .iter()
            .enumerate()
            .filter_map(|(index, worktree)| {
                let branch = fuzzy_score(&self.query, &worktree.branch);
                let path = fuzzy_score(&self.query, &worktree.path).map(|score| score - 10);
                branch.max(path).map(|score| (score, index))
            })
            .collect();
        scored.sort_by_key(|&(score, index)| (std::cmp::Reverse(score), index));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }

    pub fn render(&self, slice: &mut GridSlice<'_>) {
        let title = Style::new().fg(Color::White).bold();
        let dim = Style::new().fg(Color::DarkGrey);
        let key = Style::new().fg(Color::Yellow).bold();
        let current = Style::new().fg(Color::White).bold();
        let normal = Style::new().fg(Color::Grey);
        slice.put_str(2, 1, "Choose a worktree", title);
        slice.put_str(2, 2, &self.action.description, dim);
        slice.put_str(2, 4, ">", key);
        let col = slice.put_str(4, 4, &self.query, current);
        slice.put_str(col, 4, "█", dim);
        let width = slice.width() as usize;
        if self.matches.is_empty() {
            slice.put_str(4, 6, "no matching worktree", dim);
            return;
        }
        let branch_width = self
            .matches
            .iter()
            .map(|&index| self.worktrees[index].branch.chars().count())
            .max()
            .unwrap_or(0);
        for (i, &index) in self.matches.iter().enumerate() {
            let row = i as u16 + 6;
            if row >= slice.height() {
                break;
            }
            let worktree = &self.worktrees[index];
            let selected = i == self.selected;
            slice.put_str(2, row, if selected { "›" } else { " " }, key);
            let style = if selected { current } else { normal };
            let branch = format!("{:<branch_width$}", worktree.branch);
            slice.put_str(4, row, &branch, style);
            let path_x = 4 + branch_width + 2;
            if path_x < width {
                let path = shorten_path(&worktree.path, width - path_x);
                slice.put_str(path_x as u16, row, &path, dim);
            }
        }
    }
}

/// Case-insensitive subsequence match. Higher is better: runs of consecutive
/// characters and matches at the start of a word score more, and so do shorter
/// texts.
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0;
    let mut at = 0;
    let mut previous: Option<usize> = None;
    for wanted in query.chars().flat_map(char::to_lowercase) {
        let found = (at..text.len()).find(|&i| text[i] == wanted)?;
        score += 1;
        if previous.is_some_and(|p| p + 1 == found) {
            score += 4;
        }
        if found == 0 || !text[found - 1].is_alphanumeric() {
            score += 3;
        }
        previous = Some(found);
        at = found + 1;
    }
    Some(score * 4 - text.len().min(60) as i32 / 4)
}

/// Fits `path` into `room` columns by replacing the home directory with `~`
/// and cutting the front.
fn shorten_path(path: &str, room: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    };
    let count = path.chars().count();
    if count <= room {
        return path;
    }
    let keep = room.saturating_sub(1);
    let tail: String = path.chars().skip(count - keep).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ActionConfig, ActionMode, Config};

    fn worktree(path: &str, branch: &str) -> Worktree {
        Worktree {
            path: path.to_string(),
            branch: branch.to_string(),
        }
    }

    fn picker(current: &str) -> Picker {
        let config = Config {
            actions: [(
                "x".to_string(),
                ActionConfig {
                    mode: ActionMode::Run,
                    cmd: "true".to_string(),
                    ..ActionConfig::default()
                },
            )]
            .into(),
            ..Config::default()
        };
        let action = crate::actions::bind(&config)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let target = Target {
            session: None,
            path: current.to_string(),
            root: "/repo".to_string(),
            branch: String::new(),
        };
        Picker::new(
            action,
            target,
            vec![
                worktree("/repo", "main"),
                worktree("/repo-a", "a"),
                worktree("/repo-b", "b"),
            ],
        )
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn type_text(picker: &mut Picker, text: &str) {
        for ch in text.chars() {
            picker.handle_key(press(KeyCode::Char(ch)));
        }
    }

    fn chosen(outcome: Outcome) -> String {
        match outcome {
            Outcome::Choose(worktree) => worktree.branch,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn starts_on_the_current_worktree_and_moves_within_bounds() {
        let mut picker = picker("/repo-a");
        assert_eq!(picker.selected, 1);
        picker.handle_key(press(KeyCode::Down));
        picker.handle_key(press(KeyCode::Down));
        assert_eq!(picker.selected, 2);
        picker.handle_key(press(KeyCode::Up));
        picker.handle_key(press(KeyCode::Up));
        picker.handle_key(press(KeyCode::Up));
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn unknown_current_directory_starts_on_the_first_worktree() {
        assert_eq!(picker("/elsewhere").selected, 0);
    }

    #[test]
    fn typing_filters_and_enter_chooses_the_best_match() {
        let mut picker = picker("/repo");
        type_text(&mut picker, "b");
        assert_eq!(chosen(picker.handle_key(press(KeyCode::Enter))), "b");
        picker.handle_key(press(KeyCode::Backspace));
        // The empty query lists everything again, on the current worktree.
        assert_eq!(picker.matches.len(), 3);
        assert_eq!(chosen(picker.handle_key(press(KeyCode::Enter))), "main");
        type_text(&mut picker, "zzz");
        assert!(picker.matches.is_empty());
        assert_eq!(picker.handle_key(press(KeyCode::Enter)), Outcome::Pending);
    }

    #[test]
    fn control_keys_move_the_selection() {
        let mut picker = picker("/repo");
        for key in ['j', 'n'] {
            picker.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL));
        }
        assert_eq!(picker.selected, 2);
        for key in ['k', 'p'] {
            picker.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL));
        }
        assert_eq!(picker.selected, 0);
        assert!(picker.query.is_empty());
    }

    #[test]
    fn digits_and_q_are_text() {
        let mut picker = picker("/repo");
        type_text(&mut picker, "q1");
        assert_eq!(picker.query, "q1");
    }

    #[test]
    fn escape_clears_the_query_before_it_cancels() {
        let mut picker = picker("/repo");
        type_text(&mut picker, "fe");
        assert_eq!(picker.handle_key(press(KeyCode::Esc)), Outcome::Pending);
        assert!(picker.query.is_empty());
        assert_eq!(picker.handle_key(press(KeyCode::Esc)), Outcome::Cancel);
    }

    #[test]
    fn ranking_prefers_prefixes_and_branches_over_paths() {
        assert!(fuzzy_score("feat", "feature") > fuzzy_score("feat", "my-feature-x"));
        assert!(fuzzy_score("fx", "fix-x").is_some());
        assert_eq!(fuzzy_score("xf", "fix-x"), None);
        assert_eq!(fuzzy_score("zq", "fix-x"), None);
        assert!(fuzzy_score("FEAT", "feature").is_some());
    }

    #[test]
    fn long_paths_keep_their_tail() {
        assert_eq!(shorten_path("/a/very/long/path", 8), "…ng/path");
        assert_eq!(shorten_path("/short", 20), "/short");
    }
}
