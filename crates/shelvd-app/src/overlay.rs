//! The command-palette / history-search overlay: its state and fuzzy filtering.
//!
//! This is pure interaction state — it never touches the terminal, PTY, or
//! clipboard. Pressing Enter yields an [`Action`] the event loop executes.
//! Filtering uses `nucleo-matcher` for the same smart fuzzy ranking editors use.

use std::sync::OnceLock;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use unicode_width::UnicodeWidthStr;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use shelvd_core::{Overlay, OverlayColors, OverlayItem};

/// A command in shelvd's vocabulary: the single thing a key chord and a palette
/// row both resolve to. The event loop runs it through `run_action` (it touches
/// subsystems the overlay deliberately knows nothing about).
#[derive(Clone, Debug)]
pub enum Action {
    /// Open the command palette.
    OpenPalette,
    ScrollToTop,
    ScrollToBottom,
    CopySelection,
    CopyBlock,
    /// Paste the clipboard into the PTY.
    Paste,
    PrevBlock,
    NextBlock,
    /// Scroll the viewport one page up / down through history.
    PageUp,
    PageDown,
    /// Open the history-search overlay.
    SearchHistory,
    Quit,
    /// Write this command into the PTY's input line (a history pick).
    InsertCommand(String),
    /// Add the band's current input to the type-ahead queue, to run on the next
    /// prompt (the global "queue this command" action).
    QueueInput,
}

/// The base key of a chord: a character (matched case-insensitively) or a named
/// key. Kept separate from winit's `Key` so the table stays a plain data literal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChordKey {
    Char(char),
    Named(NamedKey),
}

/// A key chord: a base key plus the modifiers that must be held. Only the named
/// modifiers are *required* — extra ones are ignored, matching the terminal
/// convention that Ctrl+Shift+X fires whether or not Alt is also down.
#[derive(Clone, Copy)]
struct Chord {
    ctrl: bool,
    shift: bool,
    alt: bool,
    key: ChordKey,
}

impl Chord {
    const fn new(ctrl: bool, shift: bool, alt: bool, key: ChordKey) -> Self {
        Self { ctrl, shift, alt, key }
    }

    /// Whether a key press with `mods` held satisfies this chord.
    fn matches(&self, key: &Key, mods: ModifiersState) -> bool {
        if (self.ctrl && !mods.control_key())
            || (self.shift && !mods.shift_key())
            || (self.alt && !mods.alt_key())
        {
            return false;
        }
        match (self.key, key) {
            (ChordKey::Char(c), Key::Character(s)) => {
                s.eq_ignore_ascii_case(c.encode_utf8(&mut [0u8; 4]))
            }
            (ChordKey::Named(n), Key::Named(k)) => n == *k,
            _ => false,
        }
    }

    /// A human-readable rendering, e.g. `Ctrl+Shift+X`. The palette shows this as
    /// each row's keybinding hint, so the hint is derived from the binding and
    /// can never drift from it.
    fn hint(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        match self.key {
            ChordKey::Char(c) => s.push(c.to_ascii_uppercase()),
            ChordKey::Named(NamedKey::ArrowUp) => s.push_str("Up"),
            ChordKey::Named(NamedKey::ArrowDown) => s.push_str("Down"),
            ChordKey::Named(NamedKey::PageUp) => s.push_str("PageUp"),
            ChordKey::Named(NamedKey::PageDown) => s.push_str("PageDown"),
            ChordKey::Named(NamedKey::Enter) => s.push_str("Enter"),
            ChordKey::Named(_) => {}
        }
        s
    }
}

/// One command's full definition: the [`Action`] it runs, its default key chord
/// (if any), and its palette label (if it appears as a palette row). This is the
/// single command table — both the keymap and the palette are derived from it,
/// so a command added here is reachable both ways and the two can't diverge.
struct Binding {
    action: Action,
    chord: Option<Chord>,
    label: Option<&'static str>,
}

/// The command table, built once. Order matters twice: the keymap takes the
/// first chord that matches, and the palette lists labelled rows in this order.
fn commands() -> &'static [Binding] {
    use ChordKey::{Char, Named};
    static TABLE: OnceLock<Vec<Binding>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let cs = |key| Some(Chord::new(true, true, false, key)); // Ctrl+Shift+…
        let sh = |key| Some(Chord::new(false, true, false, key)); // Shift+…
        vec![
            Binding { action: Action::OpenPalette, chord: cs(Char('p')), label: None },
            Binding {
                action: Action::SearchHistory,
                chord: cs(Char('r')),
                label: Some("Search command history"),
            },
            Binding {
                action: Action::PrevBlock,
                chord: cs(Named(NamedKey::ArrowUp)),
                label: Some("Jump to previous block"),
            },
            Binding {
                action: Action::NextBlock,
                chord: cs(Named(NamedKey::ArrowDown)),
                label: Some("Jump to next block"),
            },
            Binding {
                action: Action::CopyBlock,
                chord: cs(Char('x')),
                label: Some("Copy current block"),
            },
            Binding {
                action: Action::CopySelection,
                chord: cs(Char('c')),
                label: Some("Copy selection"),
            },
            Binding { action: Action::Paste, chord: cs(Char('v')), label: None },
            Binding {
                action: Action::QueueInput,
                chord: cs(Named(NamedKey::Enter)),
                label: Some("Add typed input to run next"),
            },
            Binding { action: Action::PageUp, chord: sh(Named(NamedKey::PageUp)), label: None },
            Binding { action: Action::PageDown, chord: sh(Named(NamedKey::PageDown)), label: None },
            Binding { action: Action::ScrollToTop, chord: None, label: Some("Scroll to top") },
            Binding { action: Action::ScrollToBottom, chord: None, label: Some("Scroll to bottom") },
            Binding { action: Action::Quit, chord: None, label: Some("Quit shelvd") },
        ]
    })
}

/// Resolve a key press to the [`Action`] it is bound to, if any. The single
/// keymap: every direct keybinding lives in [`commands`], the same table the
/// palette lists, so the two can never drift apart.
pub fn key_to_action(event: &KeyEvent, mods: ModifiersState) -> Option<Action> {
    commands().iter().find_map(|b| {
        let chord = b.chord?;
        chord.matches(&event.logical_key, mods).then(|| b.action.clone())
    })
}

/// A single editable text line: append-typed, backspace-deleted, with a caret
/// that always rests at the end. The shared input primitive behind both the
/// palette/history query and the compose-next band, so both edit text the same
/// way — control chars ignored, the caret measured in display columns (wide
/// glyphs count as two) rather than `char`s.
#[derive(Default)]
struct InputLine {
    text: String,
}

impl InputLine {
    /// Append a typed character (ignoring control chars).
    fn input_char(&mut self, c: char) {
        if !c.is_control() {
            self.text.push(c);
        }
    }

    /// Delete the last character.
    fn backspace(&mut self) {
        self.text.pop();
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Take the text, leaving the line empty (used to queue a composed command).
    fn take(&mut self) -> String {
        std::mem::take(&mut self.text)
    }

    /// Display column the caret rests on: the summed width of every glyph, so a
    /// wide/CJK char advances it by two columns, not one.
    fn caret_col(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }
}

/// The band's input line: the next command being typed at the bottom while a
/// command runs. Edits the same way the overlay query does (see [`InputLine`]);
/// the event loop takes it on Enter (send to the running command) or on
/// Ctrl+Shift+Enter ([`Action::QueueInput`], queue for the next prompt).
#[derive(Default)]
pub struct BandInput {
    line: InputLine,
}

impl BandInput {
    /// Append a typed character to the input line.
    pub fn input_char(&mut self, c: char) {
        self.line.input_char(c);
    }

    /// Delete the last character of the input line.
    pub fn backspace(&mut self) {
        self.line.backspace();
    }

    /// The text typed so far.
    pub fn text(&self) -> &str {
        self.line.text()
    }

    /// Whether the input line is empty.
    pub fn is_empty(&self) -> bool {
        self.line.is_empty()
    }

    /// Take the typed text, leaving the line empty.
    pub fn take(&mut self) -> String {
        self.line.take()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Palette,
    History,
}

struct Candidate {
    label: String,
    detail: Option<String>,
    /// Label plus detail, matched against so an action is findable by its
    /// keybinding hint, not only its name.
    haystack: String,
    action: Action,
}

impl Candidate {
    fn new(label: impl Into<String>, detail: Option<&str>, action: Action) -> Self {
        let label = label.into();
        let haystack = match detail {
            Some(d) => format!("{label} {d}"),
            None => label.clone(),
        };
        Self { label, detail: detail.map(str::to_owned), haystack, action }
    }
}

/// Live state of an open overlay.
pub struct OverlayState {
    kind: Kind,
    query: InputLine,
    candidates: Vec<Candidate>,
    /// Indices into `candidates`, best match first.
    filtered: Vec<usize>,
    /// Index into `filtered`.
    selected: usize,
    matcher: Matcher,
}

impl OverlayState {
    /// The command palette: the labelled rows of the single command table, each
    /// showing the keybinding hint derived from its own chord.
    pub fn palette() -> Self {
        let candidates = commands()
            .iter()
            .filter_map(|b| {
                let label = b.label?;
                let hint = b.chord.map(|c| c.hint());
                Some(Candidate::new(label, hint.as_deref(), b.action.clone()))
            })
            .collect();
        Self::build(Kind::Palette, candidates)
    }

    /// The history search: one row per recent command.
    pub fn history(commands: Vec<String>) -> Self {
        let candidates = commands
            .into_iter()
            .map(|cmd| Candidate::new(cmd.clone(), None, Action::InsertCommand(cmd)))
            .collect();
        Self::build(Kind::History, candidates)
    }

    fn build(kind: Kind, candidates: Vec<Candidate>) -> Self {
        let mut state = Self {
            kind,
            query: InputLine::default(),
            filtered: (0..candidates.len()).collect(),
            candidates,
            selected: 0,
            matcher: Matcher::new(Config::DEFAULT),
        };
        state.refilter();
        state
    }

    /// Append a typed character to the query (ignoring control chars).
    pub fn input_char(&mut self, c: char) {
        self.query.input_char(c);
        self.refilter();
    }

    /// Delete the last query character.
    pub fn backspace(&mut self) {
        self.query.backspace();
        self.refilter();
    }

    /// Move the highlight by `delta` rows, wrapping around.
    pub fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }

    /// The action of the highlighted row, if any.
    pub fn selected_action(&self) -> Option<Action> {
        let idx = *self.filtered.get(self.selected)?;
        Some(self.candidates[idx].action.clone())
    }

    fn refilter(&mut self) {
        // Remember the highlighted candidate so the cursor can stay on it.
        let previous = self.filtered.get(self.selected).copied();

        if self.query.is_empty() {
            self.filtered = (0..self.candidates.len()).collect();
        } else {
            let pattern =
                Pattern::parse(self.query.text(), CaseMatching::Ignore, Normalization::Smart);
            let candidates = &self.candidates;
            let matcher = &mut self.matcher;
            // One scratch buffer, reused per candidate (Utf32Str::new clears it).
            let mut buf = Vec::new();
            let mut scored: Vec<(usize, u32)> = candidates
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let hay = Utf32Str::new(&c.haystack, &mut buf);
                    pattern.score(hay, matcher).map(|score| (i, score))
                })
                .collect();
            // Higher score first; the sort is stable, so ties keep list order.
            scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }

        // Keep the highlight on the same item if it survived; else jump to top.
        self.selected = previous
            .and_then(|cand| self.filtered.iter().position(|&i| i == cand))
            .unwrap_or(0);
    }

    /// Build the render-ready overlay description for this frame, materializing
    /// only the `capacity` rows that fit below the query line. The window scrolls
    /// so the selection stays visible, so a long history clones a dozen strings
    /// per redraw rather than the whole filtered list.
    pub fn to_overlay(&self, colors: OverlayColors, capacity: usize) -> Overlay {
        let len = self.filtered.len();
        let visible = len.min(capacity);
        let mut first = if visible > 0 && self.selected >= visible {
            self.selected + 1 - visible
        } else {
            0
        };
        first = first.min(len.saturating_sub(visible));

        let items = self.filtered[first..first + visible]
            .iter()
            .map(|&i| {
                let c = &self.candidates[i];
                OverlayItem { label: c.label.clone(), detail: c.detail.clone() }
            })
            .collect();
        let selected_visible = (visible > 0
            && self.selected >= first
            && self.selected < first + visible)
            .then(|| self.selected - first);

        let prompt = match self.kind {
            Kind::Palette => ">".to_owned(),
            Kind::History => "history".to_owned(),
        };
        // Caret column = display width of "<prompt> <query>" (the renderer draws
        // the prompt, one space, then the query). Using display width — not a
        // char count — keeps the caret on the real cell when the query holds
        // wide/CJK glyphs that occupy two columns each.
        let query_caret_col =
            UnicodeWidthStr::width(prompt.as_str()) + 1 + self.query.caret_col();

        Overlay {
            prompt,
            query: self.query.text().to_owned(),
            items,
            selected_visible,
            // Only flag "no matches" once the user has actually typed — an empty
            // query shows the "type to search…" placeholder, which already
            // conveys the empty state, so a "no matches" row beside it (e.g. an
            // empty history with no query) would be a contradictory double-message.
            no_matches: !self.query.is_empty() && self.filtered.is_empty(),
            query_caret_col,
            colors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shelvd_core::Rgba;

    /// A throwaway palette — `to_overlay` only copies it into the `Overlay`, so
    /// the exact values don't matter for the windowing assertions.
    fn test_colors() -> OverlayColors {
        let c = Rgba::new(0, 0, 0, 255);
        OverlayColors { panel_bg: c, fg: c, dim: c, sel_bg: c, accent: c }
    }

    #[test]
    fn to_overlay_windows_to_capacity_around_the_selection() {
        // More commands than the window holds, selection driven near the end.
        let cmds: Vec<String> = (0..50).map(|i| format!("cmd-{i:02}")).collect();
        let mut h = OverlayState::history(cmds);
        let capacity = 12;
        // Select the second-to-last row (index 48 of 50).
        h.move_selection(-2);
        assert_eq!(h.selected, 48);

        let ov = h.to_overlay(test_colors(), capacity);

        // (a) Exactly `capacity` rows are materialized, not the whole list.
        assert_eq!(ov.items.len(), capacity);
        // (b) The window scrolled to include the selected command's label.
        let sv = ov.selected_visible.expect("selection is inside the window");
        assert_eq!(ov.items[sv].label, "cmd-48");
        // (c) `selected_visible` indexes the selected item within `items`.
        assert!(sv < ov.items.len());
    }

    #[test]
    fn to_overlay_keeps_every_row_when_under_capacity() {
        let h = OverlayState::history(vec!["one".into(), "two".into(), "three".into()]);
        let ov = h.to_overlay(test_colors(), 12);
        assert_eq!(ov.items.len(), 3);
        assert_eq!(ov.selected_visible, Some(0));
    }

    #[test]
    fn to_overlay_flags_no_matches() {
        let mut p = OverlayState::palette();
        // A matching query: results exist, so the flag is clear.
        for c in "quit".chars() {
            p.input_char(c);
        }
        assert!(!p.to_overlay(test_colors(), 12).no_matches);
        // Nonsense query: nothing matches, so the flag is set (and the renderer
        // draws a "no matches" row off it, not off an empty `items`).
        for c in "zzz".chars() {
            p.input_char(c);
        }
        assert!(p.filtered.is_empty());
        assert!(p.to_overlay(test_colors(), 12).no_matches);

        // An empty query never flags "no matches", even when nothing is listed:
        // a history with no recorded commands shows only the placeholder, not a
        // contradictory placeholder + "no matches" pair.
        let empty_history = OverlayState::history(vec![]);
        assert!(empty_history.filtered.is_empty());
        assert!(!empty_history.to_overlay(test_colors(), 12).no_matches);
    }

    #[test]
    fn to_overlay_caret_column_uses_display_width() {
        // Palette prompt ">" is one column; the separating space is another.
        let base = OverlayState::palette().to_overlay(test_colors(), 12).query_caret_col;
        assert_eq!(base, 2, "\"> \" places the caret in column 2");

        // A wide (double-width) CJK glyph advances the caret by two columns, not
        // one — a char count would have drifted it left by one.
        let mut h = OverlayState::history(vec!["世界".into()]);
        h.input_char('世'); // U+4E16, display width 2
        let ov = h.to_overlay(test_colors(), 12);
        // prompt "history" = 7 cols, + 1 space, + 2 for the wide glyph.
        assert_eq!(ov.query_caret_col, 7 + 1 + 2);
    }

    #[test]
    fn palette_starts_showing_everything() {
        let p = OverlayState::palette();
        assert_eq!(p.filtered.len(), p.candidates.len());
        assert!(p.candidates.len() >= 5);
    }

    #[test]
    fn fuzzy_query_narrows_to_the_match() {
        let mut p = OverlayState::palette();
        for c in "quit".chars() {
            p.input_char(c);
        }
        assert!(matches!(p.selected_action(), Some(Action::Quit)));
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut p = OverlayState::palette();
        let n = p.filtered.len();
        assert!(n > 1);
        p.move_selection(-1);
        assert_eq!(p.selected, n - 1);
        p.move_selection(1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn backspace_restores_earlier_matches() {
        let mut p = OverlayState::palette();
        for c in "zzz".chars() {
            p.input_char(c);
        }
        assert!(p.filtered.is_empty(), "nonsense query matches nothing");
        for _ in 0..3 {
            p.backspace();
        }
        assert_eq!(p.filtered.len(), p.candidates.len());
    }

    #[test]
    fn history_picks_insert_the_command() {
        let mut h = OverlayState::history(vec!["cargo build".into(), "ls -la".into()]);
        for c in "ls".chars() {
            h.input_char(c);
        }
        match h.selected_action() {
            Some(Action::InsertCommand(cmd)) => assert_eq!(cmd, "ls -la"),
            other => panic!("expected InsertCommand, got {other:?}"),
        }
    }

    #[test]
    fn actions_are_findable_by_their_keybinding() {
        // "Copy current block" is the only action whose hint ends in X.
        let mut p = OverlayState::palette();
        for c in "ctrl+shift+x".chars() {
            p.input_char(c);
        }
        assert!(matches!(p.selected_action(), Some(Action::CopyBlock)));
    }

    #[test]
    fn selection_is_preserved_across_narrowing() {
        let mut p = OverlayState::palette();
        for c in "scroll".chars() {
            p.input_char(c);
        }
        assert_eq!(p.filtered.len(), 2, "both scroll actions match");
        p.move_selection(1); // highlight "Scroll to bottom"
        assert!(matches!(p.selected_action(), Some(Action::ScrollToBottom)));
        // Both still match; the highlight must follow the item, not reset to top.
        p.input_char('o');
        assert!(matches!(p.selected_action(), Some(Action::ScrollToBottom)));
    }

    #[test]
    fn chord_matches_required_mods_case_insensitively() {
        let p = Chord::new(true, true, false, ChordKey::Char('p'));
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert!(p.matches(&Key::Character("p".into()), ctrl_shift));
        assert!(p.matches(&Key::Character("P".into()), ctrl_shift));
        // A required modifier missing: no match.
        assert!(!p.matches(&Key::Character("p".into()), ModifiersState::CONTROL));
        // Extra modifiers are ignored — only the required ones are checked.
        assert!(p.matches(&Key::Character("p".into()), ctrl_shift | ModifiersState::ALT));
        // Wrong key: no match.
        assert!(!p.matches(&Key::Character("q".into()), ctrl_shift));
    }

    #[test]
    fn chord_hint_reads_like_the_keybinding() {
        assert_eq!(Chord::new(true, true, false, ChordKey::Char('x')).hint(), "Ctrl+Shift+X");
        assert_eq!(
            Chord::new(true, true, false, ChordKey::Named(NamedKey::ArrowUp)).hint(),
            "Ctrl+Shift+Up"
        );
        assert_eq!(
            Chord::new(false, true, false, ChordKey::Named(NamedKey::PageDown)).hint(),
            "Shift+PageDown"
        );
        // The compose-next chord: the hint must name Enter, not stop at the mods.
        assert_eq!(
            Chord::new(true, true, false, ChordKey::Named(NamedKey::Enter)).hint(),
            "Ctrl+Shift+Enter"
        );
    }

    #[test]
    fn band_input_edits_like_a_line() {
        let mut c = BandInput::default();
        for ch in "hi".chars() {
            c.input_char(ch);
        }
        assert_eq!(c.text(), "hi");
        c.backspace();
        assert_eq!(c.text(), "h");
        // Taking the text yields it and clears the line for the next entry.
        assert_eq!(c.take(), "h");
        assert!(c.is_empty(), "take leaves the line empty");
    }

    #[test]
    fn queue_input_is_keybound_and_listed() {
        let queue = commands()
            .iter()
            .find(|b| matches!(b.action, Action::QueueInput))
            .expect("queue-input is in the command table");
        assert_eq!(queue.chord.unwrap().hint(), "Ctrl+Shift+Enter");
        assert!(queue.label.is_some(), "it surfaces in the palette");
    }

    #[test]
    fn open_palette_is_keybound_but_not_a_palette_row() {
        // The table is the single source of truth: open-palette is reachable by
        // its chord yet never lists itself as a palette row.
        let open = commands()
            .iter()
            .find(|b| matches!(b.action, Action::OpenPalette))
            .expect("open-palette is in the command table");
        assert!(open.chord.is_some());
        assert!(open.label.is_none());
    }

    #[test]
    fn a_keybound_palette_row_surfaces_its_chord_as_the_hint() {
        // A labelled row with a chord teaches the keybinding rather than carrying
        // a hand-written hint that could drift from the keymap.
        let copy_block = commands()
            .iter()
            .find(|b| matches!(b.action, Action::CopyBlock))
            .unwrap();
        assert_eq!(copy_block.label, Some("Copy current block"));
        assert_eq!(copy_block.chord.unwrap().hint(), "Ctrl+Shift+X");
    }
}
