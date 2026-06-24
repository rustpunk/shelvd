//! Completion sources for the owned editor (epic #19, #40): fetch candidates
//! from a shell's own engine and shape them into the protocol's
//! [`CompletionResponse`] for the completion menu.
//!
//! This first slice drives **fish** with a one-shot `fish -c "complete -C …"`.
//! Completion is Tab-triggered (a deliberate keypress), so a cold ~40 ms invoke
//! is imperceptible and avoids a persistent-subshell state machine; a warm path
//! is a later optimization if as-you-type menu refresh is ever added. The bash
//! and zsh engines are separate follow-ups — the app routes by
//! [`shelvd_pty::ShellKind`], so a non-fish shell simply gets no candidates here
//! and the caller falls back to readline.

use std::process::Command;

use shelvd_term::{CompletionItem, CompletionResponse};

/// Fetch fish completions for `line` with the caret at byte offset `cursor`.
/// Returns `None` when fish is unavailable, errors, or yields nothing — the
/// caller then falls back to readline. The replacement range is the in-progress
/// word (the last whitespace-delimited token up to the caret).
pub fn fish_complete(line: &str, cursor: usize) -> Option<CompletionResponse> {
    // Complete the line up to the caret; snap a stray offset to a char boundary.
    let cursor = floor_char_boundary(line, cursor.min(line.len()));
    let prefix = &line[..cursor];
    let script = format!("complete -C -- {}", fish_single_quote(prefix));
    let output = Command::new("fish").arg("-c").arg(script).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let items = parse_completions(&String::from_utf8_lossy(&output.stdout));
    if items.is_empty() {
        return None;
    }
    Some(CompletionResponse { replace_start: last_token_start(prefix), replace_end: cursor, items })
}

/// Parse fish's `complete -C` output: one candidate per line, the value and an
/// optional description separated by a tab.
fn parse_completions(stdout: &str) -> Vec<CompletionItem> {
    stdout
        .lines()
        .filter_map(|line| {
            let (value, description) = match line.split_once('\t') {
                Some((v, d)) => (v, Some(d.to_owned())),
                None => (line, None),
            };
            (!value.is_empty()).then(|| CompletionItem { value: value.to_owned(), description })
        })
        .collect()
}

/// Byte offset where the last whitespace-delimited token of `s` begins (0 if
/// there is no whitespace) — the in-progress word fish completes. A first-cut
/// boundary: quoting and escaped spaces are not yet handled.
fn last_token_start(s: &str) -> usize {
    s.rfind([' ', '\t']).map_or(0, |i| i + 1)
}

/// Quote `s` as a single fish string literal, escaping the two bytes special
/// inside fish single quotes (`'` and `\`), so an arbitrary line embeds safely
/// in the `complete -C` script.
fn fish_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Largest char boundary `<= i` (and `<= s.len()`).
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_token_start_finds_the_in_progress_word() {
        assert_eq!(last_token_start("git che"), 4);
        assert_eq!(last_token_start("ls"), 0, "no whitespace: the whole line is the token");
        assert_eq!(last_token_start("echo $HO"), 5);
        assert_eq!(last_token_start("git "), 4, "a trailing space leaves an empty token at the end");
    }

    #[test]
    fn fish_single_quote_escapes_quote_and_backslash() {
        assert_eq!(fish_single_quote("ls"), "'ls'");
        assert_eq!(fish_single_quote("a'b"), "'a\\'b'");
        assert_eq!(fish_single_quote("a\\b"), "'a\\\\b'");
        // A semicolon or space inside the quotes can't break out of the literal.
        assert_eq!(fish_single_quote("a; rm b"), "'a; rm b'");
    }

    #[test]
    fn parse_completions_splits_value_and_description() {
        let out = "checkout\tCheck out a branch\ncherry-pick\ncommit\tRecord changes\n";
        let items = parse_completions(out);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].value, "checkout");
        assert_eq!(items[0].description.as_deref(), Some("Check out a branch"));
        assert_eq!(items[1].value, "cherry-pick");
        assert_eq!(items[1].description, None);
        assert_eq!(items[2].description.as_deref(), Some("Record changes"));
    }

    #[test]
    fn parse_completions_skips_blank_lines() {
        assert!(parse_completions("\n\n").is_empty());
        assert_eq!(parse_completions("only\n").len(), 1);
    }

    #[test]
    fn fish_complete_lists_candidates_when_fish_is_present() {
        // Self-skips where fish isn't installed, so the suite stays green without it.
        let present = Command::new("fish")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !present {
            eprintln!("skipping fish_complete live check: fish not on PATH");
            return;
        }
        let resp = fish_complete("ech", 3).expect("fish returns candidates for 'ech'");
        assert!(resp.items.iter().any(|i| i.value == "echo"), "'echo' is among the candidates");
        assert_eq!((resp.replace_start, resp.replace_end), (0, 3), "the single token spans 0..3");
    }
}
