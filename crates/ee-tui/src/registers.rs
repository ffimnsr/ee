use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Identifies a Vim-style register by its designator character.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RegisterName {
    /// `"` — default yank/delete destination.
    Unnamed,
    /// `0` — last explicit yank (not touched by delete).
    Zero,
    /// `1`–`9` — numbered delete history; `1` is most recent.
    Numbered(u8),
    /// `a`–`z` — named registers; uppercase variant (`A`–`Z`) appends.
    Named(char),
    /// `_` — black hole (discards all writes).
    BlackHole,
    /// `/` — last search pattern.
    Search,
    /// `*` or `+` — system clipboard.
    Clipboard,
}

impl RegisterName {
    /// Map a char to the corresponding register, or `None` for unknown chars.
    pub(crate) fn from_char(c: char) -> Option<Self> {
        match c {
            '"' => Some(Self::Unnamed),
            '0' => Some(Self::Zero),
            '1'..='9' => Some(Self::Numbered(c as u8 - b'0')),
            'a'..='z' | 'A'..='Z' => Some(Self::Named(c.to_ascii_lowercase())),
            '_' => Some(Self::BlackHole),
            '/' => Some(Self::Search),
            '*' | '+' => Some(Self::Clipboard),
            _ => None,
        }
    }

    /// Returns `true` when `c` is an uppercase named register designator,
    /// which means "append to the lowercase register" in Vim.
    pub(crate) fn is_append_char(c: char) -> bool {
        c.is_ascii_uppercase()
    }
}

/// Frontend-owned register storage.
#[derive(Debug, Clone)]
pub(crate) struct RegisterStore {
    unnamed: String,
    zero: String,
    /// Indices 0..8 correspond to registers `1`..`9`.
    numbered: Vec<String>,
    named: HashMap<char, String>,
    search: String,
}

impl Default for RegisterStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RegisterStore {
    pub(crate) fn new() -> Self {
        Self {
            unnamed: String::new(),
            zero: String::new(),
            numbered: vec![String::new(); 9],
            named: HashMap::new(),
            search: String::new(),
        }
    }

    /// Return the current content of `reg`.
    pub(crate) fn get(&self, reg: &RegisterName) -> String {
        match reg {
            RegisterName::Unnamed => self.unnamed.clone(),
            RegisterName::Zero => self.zero.clone(),
            RegisterName::Numbered(n) => {
                let idx = n.saturating_sub(1).min(8) as usize;
                self.numbered[idx].clone()
            }
            RegisterName::Named(c) => self.named.get(c).cloned().unwrap_or_default(),
            RegisterName::BlackHole => String::new(),
            RegisterName::Search => self.search.clone(),
            RegisterName::Clipboard => read_clipboard(),
        }
    }

    /// Record a yank (copy without shifting numbered registers).
    /// `append` is only honoured for named registers (e.g. `"Ayw` appends to `a`).
    pub(crate) fn yank(&mut self, target: &RegisterName, text: String, append: bool) {
        if *target == RegisterName::BlackHole {
            return;
        }
        if let RegisterName::Named(c) = target {
            if append {
                self.named.entry(*c).or_default().push_str(&text);
            } else {
                self.named.insert(*c, text.clone());
            }
            self.unnamed = self.named[c].clone();
            return;
        }
        if *target == RegisterName::Clipboard {
            write_clipboard(&text);
            self.unnamed = text;
            return;
        }
        // Numbered target: update that slot; also update zero and unnamed.
        if let RegisterName::Numbered(n) = target {
            let idx = n.saturating_sub(1).min(8) as usize;
            self.numbered[idx] = text.clone();
        }
        self.zero = text.clone();
        self.unnamed = text;
    }

    /// Record a deletion.  Shifts numbered registers `1`←`2`←…←`9` like Vim.
    pub(crate) fn delete(&mut self, target: &RegisterName, text: String, append: bool) {
        if *target == RegisterName::BlackHole {
            return;
        }
        if let RegisterName::Named(c) = target {
            if append {
                self.named.entry(*c).or_default().push_str(&text);
            } else {
                self.named.insert(*c, text.clone());
            }
            self.unnamed = self.named[c].clone();
            return;
        }
        if *target == RegisterName::Clipboard {
            write_clipboard(&text);
            self.unnamed = text;
            return;
        }
        // Shift: old[1]→[2], …, old[8]→[9], new text→[1].
        for i in (1..9).rev() {
            let prev = self.numbered[i - 1].clone();
            self.numbered[i] = prev;
        }
        self.numbered[0] = text.clone();
        self.unnamed = text;
    }

    /// Update the search register (`/`).  Does not touch unnamed.
    pub(crate) fn set_search(&mut self, pattern: String) {
        self.search = pattern;
    }
}

// ── Clipboard helpers (best-effort subprocess on Linux/X11) ─────────────────

fn read_clipboard() -> String {
    // Try xclip first, then xsel.
    let try_read = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if out.status.success() {
            String::from_utf8(out.stdout).ok()
        } else {
            None
        }
    };
    try_read("xclip", &["-selection", "clipboard", "-o"])
        .or_else(|| try_read("xsel", &["--clipboard", "--output"]))
        .unwrap_or_default()
}

fn write_clipboard(text: &str) {
    let try_write = |cmd: &str, args: &[&str]| -> bool {
        let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() else {
            return false;
        };
        let Some(stdin) = child.stdin.as_mut() else {
            return false;
        };
        let _ = stdin.write_all(text.as_bytes());
        child.wait().map(|s| s.success()).unwrap_or(false)
    };
    if !try_write("xclip", &["-selection", "clipboard"]) {
        let _ = try_write("xsel", &["--clipboard", "--input"]);
    }
}

// ── LastChange ───────────────────────────────────────────────────────────────

/// The last buffer-modifying change; used by `.` to repeat.
#[derive(Debug, Clone)]
pub(crate) enum LastChange {
    /// Sequence of xi edit RPC calls recorded during an operator application.
    Commands(Vec<(&'static str, serde_json::Value)>),
    /// Characters that were typed during one insert-mode session.
    Insert(String),
}

// ── BlockInsert ──────────────────────────────────────────────────────────────

/// Deferred block-insert/append: applied when leaving insert mode.
#[derive(Debug, Clone)]
pub(crate) struct BlockInsert {
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    /// Column at which to insert or append on each line.
    pub(crate) col: usize,
    /// `false` = insert before column (`I`), `true` = append after column (`A`).
    pub(crate) append: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_name_from_char_covers_all_variants() {
        assert_eq!(RegisterName::from_char('"'), Some(RegisterName::Unnamed));
        assert_eq!(RegisterName::from_char('0'), Some(RegisterName::Zero));
        assert_eq!(RegisterName::from_char('5'), Some(RegisterName::Numbered(5)));
        assert_eq!(RegisterName::from_char('a'), Some(RegisterName::Named('a')));
        // Uppercase maps to lowercase named register.
        assert_eq!(RegisterName::from_char('A'), Some(RegisterName::Named('a')));
        assert_eq!(RegisterName::from_char('_'), Some(RegisterName::BlackHole));
        assert_eq!(RegisterName::from_char('/'), Some(RegisterName::Search));
        assert_eq!(RegisterName::from_char('*'), Some(RegisterName::Clipboard));
        assert_eq!(RegisterName::from_char('?'), None);
    }

    #[test]
    fn register_name_is_append_char() {
        assert!(RegisterName::is_append_char('A'));
        assert!(!RegisterName::is_append_char('a'));
    }

    #[test]
    fn yank_updates_zero_and_unnamed() {
        let mut store = RegisterStore::new();
        store.yank(&RegisterName::Unnamed, "hello".into(), false);
        assert_eq!(store.get(&RegisterName::Unnamed), "hello");
        assert_eq!(store.get(&RegisterName::Zero), "hello");
    }

    #[test]
    fn yank_named_register() {
        let mut store = RegisterStore::new();
        store.yank(&RegisterName::Named('a'), "foo".into(), false);
        assert_eq!(store.get(&RegisterName::Named('a')), "foo");
        assert_eq!(store.get(&RegisterName::Unnamed), "foo");
    }

    #[test]
    fn yank_named_register_append() {
        let mut store = RegisterStore::new();
        store.yank(&RegisterName::Named('a'), "foo".into(), false);
        store.yank(&RegisterName::Named('a'), "bar".into(), true);
        assert_eq!(store.get(&RegisterName::Named('a')), "foobar");
    }

    #[test]
    fn delete_shifts_numbered_registers() {
        let mut store = RegisterStore::new();
        store.delete(&RegisterName::Unnamed, "first".into(), false);
        store.delete(&RegisterName::Unnamed, "second".into(), false);
        assert_eq!(store.get(&RegisterName::Numbered(1)), "second");
        assert_eq!(store.get(&RegisterName::Numbered(2)), "first");
    }

    #[test]
    fn black_hole_discards_write() {
        let mut store = RegisterStore::new();
        store.yank(&RegisterName::BlackHole, "discarded".into(), false);
        assert_eq!(store.get(&RegisterName::BlackHole), "");
        assert_eq!(store.get(&RegisterName::Unnamed), "");
    }

    #[test]
    fn search_register_roundtrip() {
        let mut store = RegisterStore::new();
        store.set_search("pattern".into());
        assert_eq!(store.get(&RegisterName::Search), "pattern");
    }
}
