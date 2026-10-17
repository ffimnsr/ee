//! Vim-style registers.
//!
//! Register storage is owned here. The system clipboard transport behind the
//! `+` and `*` registers lives in [`crate::clipboard`].

use std::collections::HashMap;

use crate::clipboard::{ClipboardHandle, ClipboardRouting};

pub(crate) use crate::clipboard::ClipboardSelection;

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
    /// `+` — system clipboard.
    Clipboard,
    /// `*` — system primary clipboard.
    PrimaryClipboard,
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
            '+' => Some(Self::Clipboard),
            '*' => Some(Self::PrimaryClipboard),
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
    /// Transport behind the `+` and `*` registers.
    clipboard: ClipboardHandle,
    /// How plain (prefix-less) yank/delete/paste reach the system clipboard.
    routing: ClipboardRouting,
}

impl Default for RegisterStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RegisterStore {
    pub(crate) fn new() -> Self {
        Self::with_routing(ClipboardRouting::default())
    }

    /// Build a store whose plain operations follow `routing`.
    pub(crate) fn with_routing(routing: ClipboardRouting) -> Self {
        Self::with_clipboard(routing, ClipboardHandle::new())
    }

    /// Build a store around an existing clipboard transport.
    ///
    /// `App` owns the same `Arc`-backed handle so `:set` and shutdown reach the
    /// worker through one shared configuration.
    pub(crate) fn with_clipboard(routing: ClipboardRouting, clipboard: ClipboardHandle) -> Self {
        Self {
            unnamed: String::new(),
            zero: String::new(),
            numbered: vec![String::new(); 9],
            named: HashMap::new(),
            search: String::new(),
            clipboard,
            routing,
        }
    }

    /// Shared clipboard transport behind `+` and `*`.
    pub(crate) fn clipboard(&self) -> &ClipboardHandle {
        &self.clipboard
    }

    /// Current clipboard routing.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn routing(&self) -> ClipboardRouting {
        self.routing
    }

    /// Update clipboard routing (`:set clipboard=...`).
    pub(crate) fn set_routing(&mut self, routing: ClipboardRouting) {
        self.routing = routing;
    }

    /// Register a plain (prefix-less) paste reads from.
    ///
    /// With `clipboard=unnamedplus` a plain `p` reads the system clipboard, so
    /// the fallback follows the configured routing.
    pub(crate) fn default_paste_register(&self) -> RegisterName {
        match self.routing.mirror_selection() {
            Some(ClipboardSelection::Clipboard) => RegisterName::Clipboard,
            Some(ClipboardSelection::Primary) => RegisterName::PrimaryClipboard,
            None => RegisterName::Unnamed,
        }
    }

    /// Map a read of `reg` to the register it actually reads.
    ///
    /// The unnamed register is aliased to the routed selection under
    /// `clipboard=unnamedplus`/`unnamed`, so an explicit `""` (`""p`,
    /// `<C-r>"`) reads the system clipboard exactly like a plain `p`. Every
    /// other register is returned unchanged.
    pub(crate) fn read_register(&self, reg: RegisterName) -> RegisterName {
        match reg {
            RegisterName::Unnamed => self.default_paste_register(),
            other => other,
        }
    }

    /// Mirror the unnamed register to the system clipboard when routing is on.
    ///
    /// Called after every plain yank/delete so the OS clipboard tracks the
    /// unnamed register without bypassing numbered-register shifting.
    fn mirror_unnamed(&self) {
        let Some(selection) = self.routing.mirror_selection() else {
            return;
        };
        self.clipboard.write_text(selection, self.unnamed.clone());
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
            RegisterName::Clipboard => {
                self.clipboard.read_text(ClipboardSelection::Clipboard).unwrap_or_default()
            }
            RegisterName::PrimaryClipboard => {
                self.clipboard.read_text(ClipboardSelection::Primary).unwrap_or_default()
            }
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
            self.mirror_unnamed();
            return;
        }
        if *target == RegisterName::Clipboard {
            self.clipboard.write_text(ClipboardSelection::Clipboard, text.clone());
            self.unnamed = text;
            return;
        }
        if *target == RegisterName::PrimaryClipboard {
            self.clipboard.write_text(ClipboardSelection::Primary, text.clone());
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
        self.mirror_unnamed();
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
            self.mirror_unnamed();
            return;
        }
        if *target == RegisterName::Clipboard {
            self.clipboard.write_text(ClipboardSelection::Clipboard, text.clone());
            self.unnamed = text;
            return;
        }
        if *target == RegisterName::PrimaryClipboard {
            self.clipboard.write_text(ClipboardSelection::Primary, text.clone());
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
        self.mirror_unnamed();
    }

    /// Update the search register (`/`).  Does not touch unnamed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_search(&mut self, pattern: String) {
        self.search = pattern;
    }

    pub(crate) fn clear(&mut self, reg: Option<&RegisterName>) {
        match reg {
            None => self.reset(),
            Some(RegisterName::Unnamed) => self.unnamed.clear(),
            Some(RegisterName::Zero) => self.zero.clear(),
            Some(RegisterName::Numbered(n)) => {
                let idx = n.saturating_sub(1).min(8) as usize;
                self.numbered[idx].clear();
            }
            Some(RegisterName::Named(c)) => {
                self.named.remove(c);
            }
            Some(RegisterName::BlackHole) => {}
            Some(RegisterName::Search) => self.search.clear(),
            Some(RegisterName::Clipboard) => {
                self.clipboard.write_text(ClipboardSelection::Clipboard, String::new());
            }
            Some(RegisterName::PrimaryClipboard) => {
                self.clipboard.write_text(ClipboardSelection::Primary, String::new());
            }
        }
    }

    /// Clear every register without replacing the clipboard transport.
    ///
    /// Dropping the transport would close the worker channel, and the worker
    /// dropping its `arboard::Clipboard` releases this process's selection
    /// ownership on X11 and Wayland — which wipes the system clipboard for every
    /// other application. `:clear` must not do that.
    fn reset(&mut self) {
        self.unnamed.clear();
        self.zero.clear();
        self.numbered = vec![String::new(); 9];
        self.named.clear();
        self.search.clear();
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
        assert_eq!(RegisterName::from_char('+'), Some(RegisterName::Clipboard));
        assert_eq!(RegisterName::from_char('*'), Some(RegisterName::PrimaryClipboard));
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

    #[test]
    fn clipboard_registers_use_distinct_selections() {
        let mut store = RegisterStore::new();

        store.yank(&RegisterName::Clipboard, "clip".into(), false);
        store.yank(&RegisterName::PrimaryClipboard, "primary".into(), false);

        assert_eq!(store.get(&RegisterName::Clipboard), "clip");
        assert_eq!(store.get(&RegisterName::PrimaryClipboard), "primary");
    }

    #[test]
    fn empty_clipboard_does_not_fall_back_to_the_cached_value() {
        use crate::clipboard::set_test_clipboard;

        let mut store = RegisterStore::new();
        store.yank(&RegisterName::Clipboard, "cached".into(), false);

        // An externally emptied clipboard is authoritative: the cached value
        // must not be pasted in its place.
        set_test_clipboard(ClipboardSelection::Clipboard, "");
        assert_eq!(store.get(&RegisterName::Clipboard), "");
    }

    #[test]
    fn unnamedplus_mirrors_plain_yank_to_system_clipboard() {
        use crate::clipboard::set_test_clipboard;

        set_test_clipboard(ClipboardSelection::Clipboard, "");
        let mut store = RegisterStore::with_routing(ClipboardRouting::UnnamedPlus);
        store.yank(&RegisterName::Unnamed, "hello".into(), false);

        assert_eq!(store.get(&RegisterName::Clipboard), "hello");
        assert_eq!(store.default_paste_register(), RegisterName::Clipboard);
    }

    #[test]
    fn unnamedplus_mirrors_named_yank_and_delete_to_clipboard() {
        use crate::clipboard::set_test_clipboard;

        set_test_clipboard(ClipboardSelection::Clipboard, "");
        let mut store = RegisterStore::with_routing(ClipboardRouting::UnnamedPlus);
        store.yank(&RegisterName::Named('a'), "alpha".into(), false);
        assert_eq!(store.get(&RegisterName::Clipboard), "alpha");

        store.delete(&RegisterName::Unnamed, "deleted".into(), false);
        assert_eq!(store.get(&RegisterName::Clipboard), "deleted");
    }

    #[test]
    fn unnamed_routing_mirrors_to_primary_and_pastes_primary() {
        use crate::clipboard::set_test_clipboard;

        set_test_clipboard(ClipboardSelection::Primary, "");
        let mut store = RegisterStore::with_routing(ClipboardRouting::Unnamed);
        store.yank(&RegisterName::Unnamed, "hello".into(), false);

        assert_eq!(store.get(&RegisterName::PrimaryClipboard), "hello");
        assert_eq!(store.default_paste_register(), RegisterName::PrimaryClipboard);
    }

    #[test]
    fn off_routing_keeps_operations_internal() {
        use crate::clipboard::set_test_clipboard;

        set_test_clipboard(ClipboardSelection::Clipboard, "");
        let mut store = RegisterStore::with_routing(ClipboardRouting::Off);
        store.yank(&RegisterName::Unnamed, "hello".into(), false);

        // No OS write, and a plain paste stays on the internal register.
        assert_eq!(store.get(&RegisterName::Clipboard), "");
        assert_eq!(store.get(&RegisterName::Unnamed), "hello");
        assert_eq!(store.default_paste_register(), RegisterName::Unnamed);
    }

    #[test]
    fn routing_does_not_skip_numbered_shift_on_delete() {
        use crate::clipboard::set_test_clipboard;

        set_test_clipboard(ClipboardSelection::Clipboard, "");
        let mut store = RegisterStore::with_routing(ClipboardRouting::UnnamedPlus);
        store.delete(&RegisterName::Unnamed, "first".into(), false);
        store.delete(&RegisterName::Unnamed, "second".into(), false);

        assert_eq!(store.get(&RegisterName::Numbered(1)), "second");
        assert_eq!(store.get(&RegisterName::Numbered(2)), "first");
        assert_eq!(store.get(&RegisterName::Clipboard), "second");
    }

    #[test]
    fn set_routing_switches_paste_default() {
        let mut store = RegisterStore::new();
        assert_eq!(store.routing(), ClipboardRouting::UnnamedPlus);
        assert_eq!(store.default_paste_register(), RegisterName::Clipboard);

        store.set_routing(ClipboardRouting::Off);
        assert_eq!(store.routing(), ClipboardRouting::Off);
        assert_eq!(store.default_paste_register(), RegisterName::Unnamed);
    }

    #[test]
    fn read_register_aliases_only_the_unnamed_register() {
        let mut store = RegisterStore::with_routing(ClipboardRouting::UnnamedPlus);
        // Explicit `""` reads the routed selection, like a plain `p`.
        assert_eq!(store.read_register(RegisterName::Unnamed), RegisterName::Clipboard);
        // Every other register is untouched.
        assert_eq!(store.read_register(RegisterName::Named('a')), RegisterName::Named('a'));
        assert_eq!(store.read_register(RegisterName::Zero), RegisterName::Zero);
        assert_eq!(store.read_register(RegisterName::Numbered(1)), RegisterName::Numbered(1));
        assert_eq!(store.read_register(RegisterName::Search), RegisterName::Search);

        store.set_routing(ClipboardRouting::Unnamed);
        assert_eq!(store.read_register(RegisterName::Unnamed), RegisterName::PrimaryClipboard);

        store.set_routing(ClipboardRouting::Off);
        assert_eq!(store.read_register(RegisterName::Unnamed), RegisterName::Unnamed);
    }
}
