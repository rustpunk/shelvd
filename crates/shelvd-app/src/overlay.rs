//! The command-palette / history-search overlay: its state and fuzzy filtering.
//!
//! This is pure interaction state — it never touches the terminal, PTY, or
//! clipboard. Pressing Enter yields an [`Action`] the event loop executes.
//! Filtering uses `nucleo-matcher` for the same smart fuzzy ranking editors use.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use shelvd_core::{Overlay, OverlayColors, OverlayItem};

/// What pressing Enter on a selected row does. The event loop runs it (it
/// touches subsystems the overlay deliberately knows nothing about).
#[derive(Clone, Debug)]
pub enum Action {
    ScrollToTop,
    ScrollToBottom,
    CopySelection,
    CopyBlock,
    PrevBlock,
    NextBlock,
    /// Open the history-search overlay.
    SearchHistory,
    Quit,
    /// Write this command into the PTY's input line (a history pick).
    InsertCommand(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Palette,
    History,
}

struct Candidate {
    label: String,
    detail: Option<String>,
    action: Action,
}

/// Live state of an open overlay.
pub struct OverlayState {
    kind: Kind,
    query: String,
    candidates: Vec<Candidate>,
    /// Indices into `candidates`, best match first.
    filtered: Vec<usize>,
    /// Index into `filtered`.
    selected: usize,
    matcher: Matcher,
}

impl OverlayState {
    /// The command palette: a fixed list of actions.
    pub fn palette() -> Self {
        let candidates = vec![
            Candidate {
                label: "Search command history".into(),
                detail: Some("Ctrl+Shift+R".into()),
                action: Action::SearchHistory,
            },
            Candidate {
                label: "Jump to previous block".into(),
                detail: Some("Ctrl+Shift+Up".into()),
                action: Action::PrevBlock,
            },
            Candidate {
                label: "Jump to next block".into(),
                detail: Some("Ctrl+Shift+Down".into()),
                action: Action::NextBlock,
            },
            Candidate {
                label: "Copy current block".into(),
                detail: Some("Ctrl+Shift+X".into()),
                action: Action::CopyBlock,
            },
            Candidate {
                label: "Copy selection".into(),
                detail: Some("Ctrl+Shift+C".into()),
                action: Action::CopySelection,
            },
            Candidate { label: "Scroll to top".into(), detail: None, action: Action::ScrollToTop },
            Candidate {
                label: "Scroll to bottom".into(),
                detail: None,
                action: Action::ScrollToBottom,
            },
            Candidate { label: "Quit shelvd".into(), detail: None, action: Action::Quit },
        ];
        Self::build(Kind::Palette, candidates)
    }

    /// The history search: one row per recent command.
    pub fn history(commands: Vec<String>) -> Self {
        let candidates = commands
            .into_iter()
            .map(|cmd| Candidate {
                label: cmd.clone(),
                detail: None,
                action: Action::InsertCommand(cmd),
            })
            .collect();
        Self::build(Kind::History, candidates)
    }

    fn build(kind: Kind, candidates: Vec<Candidate>) -> Self {
        let mut state = Self {
            kind,
            query: String::new(),
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
        if c.is_control() {
            return;
        }
        self.query.push(c);
        self.refilter();
    }

    /// Delete the last query character.
    pub fn backspace(&mut self) {
        self.query.pop();
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
        if self.query.is_empty() {
            self.filtered = (0..self.candidates.len()).collect();
            self.selected = 0;
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        let candidates = &self.candidates;
        let matcher = &mut self.matcher;
        let mut scored: Vec<(usize, u32)> = candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let mut buf = Vec::new();
                let hay = Utf32Str::new(&c.label, &mut buf);
                pattern.score(hay, matcher).map(|score| (i, score))
            })
            .collect();
        // Higher score first; the sort is stable, so ties keep list order.
        scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
        self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        self.selected = 0;
    }

    /// Build the render-ready overlay description for this frame.
    pub fn to_overlay(&self, colors: OverlayColors) -> Overlay {
        let items = self
            .filtered
            .iter()
            .map(|&i| {
                let c = &self.candidates[i];
                OverlayItem { label: c.label.clone(), detail: c.detail.clone() }
            })
            .collect();
        Overlay {
            prompt: match self.kind {
                Kind::Palette => ">".into(),
                Kind::History => "history".into(),
            },
            query: self.query.clone(),
            items,
            selected: self.selected,
            colors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
