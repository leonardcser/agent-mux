//! The sidebar's built-in keys: one table drives the help screen and the keys
//! user actions cannot rebind.

pub struct Builtin {
    /// How the key is shown in help.
    pub label: &'static str,
    pub help: &'static str,
    /// Plain characters the key handler consumes.
    pub chars: &'static str,
    /// Characters the key handler consumes together with Ctrl.
    pub ctrl_chars: &'static str,
}

const fn key(
    label: &'static str,
    help: &'static str,
    chars: &'static str,
    ctrl_chars: &'static str,
) -> Builtin {
    Builtin {
        label,
        help,
        chars,
        ctrl_chars,
    }
}

pub const BUILTINS: &[Builtin] = &[
    key("j/k", "move down/up", "jk", ""),
    key("h/left", "collapse section", "h", ""),
    key("C-n/C-p", "move down/up", "", "np"),
    key("[n]j/k", "move down/up n times", "0123456789", ""),
    key("enter", "switch to pane, or run a", "", ""),
    key("space", "toggle read/unread", " ", ""),
    key("R", "mark all read", "R", ""),
    key("s/u", "stash/unstash", "su", ""),
    key("C-ret/w", "park/unpark (manual mode)", "w", ""),
    key("dd", "kill pane, or forget project", "d", ""),
    key("gg", "go to first", "g", ""),
    key("M-<", "go to first", "", ""),
    key("G", "go to last", "G", ""),
    key("M->", "go to last", "", ""),
    key("o", "toggle sort order", "o", ""),
    key("H/L", "resize sidebar", "HL", ""),
    key("drag", "resize sidebar", "", ""),
    key("?", "toggle help", "?", ""),
    key("q/esc", "quit", "q", ""),
    key("C-c/C-d", "quit", "", "cd"),
];

/// Whether the sidebar already handles this key.
pub fn is_reserved(key: char, ctrl: bool) -> bool {
    BUILTINS.iter().any(|builtin| {
        if ctrl {
            builtin.ctrl_chars.contains(key)
        } else {
            builtin.chars.contains(key)
        }
    })
}
