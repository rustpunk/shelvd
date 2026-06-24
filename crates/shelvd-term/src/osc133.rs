//! A byte-stream tee that recognizes shell-integration escape sequences the
//! locked `alacritty_terminal`/`vte` drop before any handler can see them.
//!
//! [`Scanner`] runs over each PTY chunk *before* it reaches alacritty, framing
//! OSC sequences (`ESC ] … (BEL | ST)`) and surfacing the two we care about:
//!
//! - **OSC 133** semantic prompts (`133;A` / `B` / `C` / `D[;exit]`) — the
//!   command-block boundaries.
//! - **OSC 7** working-directory reports (`7;file://host/path`).
//!
//! The framing state persists across calls, so a sequence split across PTY
//! reads is still recognized — the documented correctness risk for blocks. Each
//! recognized marker is returned with the byte offset just past its terminator,
//! so the caller can feed alacritty up to that point and read the cursor to
//! anchor the marker to an absolute grid line.

/// An OSC 133 semantic-prompt marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticKind {
    /// `133;A` — a fresh prompt is about to be drawn.
    PromptStart,
    /// `133;B` — end of the prompt; command input begins here.
    PromptEnd,
    /// `133;C` — the command was submitted; its output begins here.
    OutputStart,
    /// `133;D[;exit]` — the command finished, optionally with its exit code.
    CommandFinished(Option<i32>),
}

/// A recognized shell-integration marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marker {
    /// An OSC 133 semantic prompt.
    Semantic(SemanticKind),
    /// An OSC 7 working-directory report, decoded to a filesystem path.
    Cwd(String),
    /// A shelvd completion response (private OSC 5379), carrying the parsed
    /// candidate list for the owned editor's completion menu.
    Completion(crate::completion::CompletionResponse),
}

/// Upper bound on an OSC payload we will buffer; a longer sequence is abandoned
/// rather than grown without limit (alacritty still parses it on its own path).
const MAX_OSC: usize = 4096;

/// A completion response (OSC 5379) can list many candidates, so it is allowed to
/// grow past [`MAX_OSC`] up to this larger bound; anything longer is abandoned.
const MAX_COMPLETION_OSC: usize = 64 * 1024;

/// Where the scanner is in the middle of recognizing an OSC sequence. The buffer
/// holds the payload accumulated after `ESC ]`, so a split read resumes cleanly.
#[derive(Debug, Default)]
enum State {
    /// Outside any escape sequence.
    #[default]
    Ground,
    /// Saw `ESC`; awaiting `]` to begin an OSC.
    Esc,
    /// Inside an OSC payload.
    Osc(Vec<u8>),
    /// Inside an OSC payload, just saw `ESC`; awaiting `\` (the ST terminator).
    OscEsc(Vec<u8>),
}

/// Stateful OSC framer. One per terminal; [`Scanner::scan`] each PTY chunk.
#[derive(Debug, Default)]
pub struct Scanner {
    state: State,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan one chunk, returning each recognized marker paired with the byte
    /// offset **just past** its terminator within `bytes`. Framing state carries
    /// over to the next call, so a sequence straddling chunk boundaries is
    /// recognized when its terminator finally arrives.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, Marker)> {
        let mut hits = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            // Take ownership of the current state so payload buffers can move.
            let state = std::mem::take(&mut self.state);
            self.state = match (state, b) {
                (State::Ground, 0x1b) => State::Esc,
                (State::Ground, _) => State::Ground,

                (State::Esc, b']') => State::Osc(Vec::new()),
                (State::Esc, 0x1b) => State::Esc,
                (State::Esc, _) => State::Ground,

                // BEL terminates the OSC.
                (State::Osc(buf), 0x07) => {
                    push_hit(&mut hits, i, &buf);
                    State::Ground
                }
                // ESC may begin the two-byte ST terminator.
                (State::Osc(buf), 0x1b) => State::OscEsc(buf),
                (State::Osc(mut buf), b) => {
                    buf.push(b);
                    // Most OSC payloads are abandoned past MAX_OSC, but a
                    // completion response may legitimately be larger, so it gets a
                    // bigger bound. The prefix check only runs once a payload has
                    // already exceeded MAX_OSC, so the common path stays a length
                    // compare.
                    let over_general = buf.len() > MAX_OSC;
                    let completion = over_general
                        && buf.starts_with(crate::completion::RESPONSE_PREFIX.as_bytes());
                    if (over_general && !completion) || buf.len() > MAX_COMPLETION_OSC {
                        State::Ground
                    } else {
                        State::Osc(buf)
                    }
                }

                // `ESC \` is ST: the OSC terminates here.
                (State::OscEsc(buf), b'\\') => {
                    push_hit(&mut hits, i, &buf);
                    State::Ground
                }
                // `ESC ]` aborts this OSC and starts a fresh one.
                (State::OscEsc(_), b']') => State::Osc(Vec::new()),
                // `ESC ESC` aborts the OSC; the second ESC starts a new sequence.
                (State::OscEsc(_), 0x1b) => State::Esc,
                // `ESC <other>` aborts the OSC entirely.
                (State::OscEsc(_), _) => State::Ground,
            };
        }
        hits
    }
}

/// Parse a completed OSC payload and, if it is one we track, record it with the
/// offset just past its terminator byte (at index `term_index`).
fn push_hit(hits: &mut Vec<(usize, Marker)>, term_index: usize, payload: &[u8]) {
    if let Some(marker) = parse_marker(payload) {
        hits.push((term_index + 1, marker));
    }
}

/// Recognize an OSC 133 semantic prompt, an OSC 7 cwd report, or a shelvd
/// completion response (OSC 5379) in `payload` (the bytes between `ESC ]` and the
/// terminator).
fn parse_marker(payload: &[u8]) -> Option<Marker> {
    if let Some(rest) = payload.strip_prefix(crate::completion::RESPONSE_PREFIX.as_bytes()) {
        return crate::completion::parse_response_payload(rest).map(Marker::Completion);
    }
    if let Some(rest) = payload.strip_prefix(b"133;") {
        let mut parts = rest.split(|&b| b == b';');
        let kind = match parts.next()? {
            b"A" => SemanticKind::PromptStart,
            b"B" => SemanticKind::PromptEnd,
            b"C" => SemanticKind::OutputStart,
            b"D" => {
                let exit = parts
                    .next()
                    .and_then(|p| std::str::from_utf8(p).ok())
                    .and_then(|s| s.parse::<i32>().ok());
                SemanticKind::CommandFinished(exit)
            }
            _ => return None,
        };
        return Some(Marker::Semantic(kind));
    }
    if let Some(rest) = payload.strip_prefix(b"7;") {
        let url = std::str::from_utf8(rest).ok()?;
        return file_url_to_path(url).map(Marker::Cwd);
    }
    None
}

/// Extract the filesystem path from a `file://host/path` URL, percent-decoding
/// it. Returns `None` if it is not a `file://` URL with a path.
fn file_url_to_path(url: &str) -> Option<String> {
    let authority_and_path = url.strip_prefix("file://")?;
    // The authority (host) runs up to the first '/', which begins the path.
    let slash = authority_and_path.find('/')?;
    Some(percent_decode(&authority_and_path[slash..]))
}

/// Decode `%XX` escapes; bytes that are not valid escapes pass through unchanged.
/// The inverse of [`percent_encode_strict`], and also used to decode OSC 7 paths.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode every byte outside the RFC 3986 unreserved set (`A-Za-z0-9` and
/// `-._~`). Strict on purpose: the completion framing bytes `;`, `,`, `%`, the OSC
/// terminators, spaces, and all non-ASCII bytes are escaped, so an encoded field
/// is a single token safe to drop into an OSC payload or a shell word.
pub(crate) fn percent_encode_strict(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// The uppercase hex digit for a nibble `0..=15`.
fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a single chunk through a fresh scanner.
    fn scan_once(bytes: &[u8]) -> Vec<(usize, Marker)> {
        Scanner::new().scan(bytes)
    }

    fn markers(bytes: &[u8]) -> Vec<Marker> {
        scan_once(bytes).into_iter().map(|(_, m)| m).collect()
    }

    #[test]
    fn parses_each_semantic_kind() {
        use SemanticKind::*;
        assert_eq!(markers(b"\x1b]133;A\x07"), vec![Marker::Semantic(PromptStart)]);
        assert_eq!(markers(b"\x1b]133;B\x07"), vec![Marker::Semantic(PromptEnd)]);
        assert_eq!(markers(b"\x1b]133;C\x07"), vec![Marker::Semantic(OutputStart)]);
        assert_eq!(markers(b"\x1b]133;D\x07"), vec![Marker::Semantic(CommandFinished(None))]);
        assert_eq!(markers(b"\x1b]133;D;0\x07"), vec![Marker::Semantic(CommandFinished(Some(0)))]);
        assert_eq!(markers(b"\x1b]133;D;42\x07"), vec![Marker::Semantic(CommandFinished(Some(42)))]);
    }

    #[test]
    fn accepts_st_terminator_and_extra_params() {
        use SemanticKind::*;
        // ST (ESC \) terminator instead of BEL.
        assert_eq!(markers(b"\x1b]133;A\x1b\\"), vec![Marker::Semantic(PromptStart)]);
        // Trailing params after the kind letter are ignored.
        assert_eq!(markers(b"\x1b]133;A;cl=m\x07"), vec![Marker::Semantic(PromptStart)]);
        assert_eq!(markers(b"\x1b]133;D;1;extra\x07"), vec![Marker::Semantic(CommandFinished(Some(1)))]);
    }

    #[test]
    fn parses_osc7_cwd() {
        assert_eq!(
            markers(b"\x1b]7;file://host/home/u\x07"),
            vec![Marker::Cwd("/home/u".to_string())]
        );
        // Percent-encoded spaces decode.
        assert_eq!(
            markers(b"\x1b]7;file://host/a%20b\x1b\\"),
            vec![Marker::Cwd("/a b".to_string())]
        );
    }

    #[test]
    fn ignores_unrelated_osc_and_text() {
        // A window-title OSC must not match, even if its text mentions 133;A.
        assert!(markers(b"\x1b]0;a 133;A title\x07").is_empty());
        assert!(markers(b"plain text, no escapes").is_empty());
        assert!(markers(b"\x1b]133;Z\x07").is_empty());
    }

    #[test]
    fn reports_offset_just_past_terminator() {
        // "hi" + OSC(BEL) + "yo": the offset must point at 'y'.
        let bytes = b"hi\x1b]133;B\x07yo";
        let hits = scan_once(bytes);
        assert_eq!(hits.len(), 1);
        let (offset, ref marker) = hits[0];
        assert_eq!(*marker, Marker::Semantic(SemanticKind::PromptEnd));
        assert_eq!(&bytes[offset..], b"yo");
    }

    #[test]
    fn finds_multiple_markers_in_one_chunk() {
        let out = markers(b"\x1b]133;A\x07prompt$ \x1b]133;B\x07ls\x1b]133;C\x07");
        assert_eq!(
            out,
            vec![
                Marker::Semantic(SemanticKind::PromptStart),
                Marker::Semantic(SemanticKind::PromptEnd),
                Marker::Semantic(SemanticKind::OutputStart),
            ]
        );
    }

    #[test]
    fn sequence_split_across_reads_is_recognized() {
        let mut s = Scanner::new();
        // The whole sequence arrives one byte per chunk; the marker surfaces only
        // when its terminator does.
        let full = b"\x1b]133;D;7\x07";
        for (n, _) in full.iter().enumerate() {
            let chunk = &full[n..n + 1];
            let hits = s.scan(chunk);
            if n + 1 == full.len() {
                assert_eq!(
                    hits,
                    vec![(1, Marker::Semantic(SemanticKind::CommandFinished(Some(7))))]
                );
            } else {
                assert!(hits.is_empty(), "no marker before the terminator (byte {n})");
            }
        }
    }

    #[test]
    fn split_mid_payload_and_at_st() {
        // Split in the middle of "133".
        let mut s = Scanner::new();
        assert!(s.scan(b"\x1b]13").is_empty());
        assert_eq!(s.scan(b"3;C\x07"), vec![(4, Marker::Semantic(SemanticKind::OutputStart))]);

        // Split between the ESC and the '\' of an ST terminator.
        let mut s = Scanner::new();
        assert!(s.scan(b"\x1b]133;A\x1b").is_empty());
        assert_eq!(s.scan(b"\\"), vec![(1, Marker::Semantic(SemanticKind::PromptStart))]);
    }

    #[test]
    fn aborted_osc_then_fresh_osc() {
        // An ESC that is not part of ST aborts the OSC; a following ESC ] starts
        // a new one that is recognized.
        let out = markers(b"\x1b]999\x1bX\x1b]133;A\x07");
        assert_eq!(out, vec![Marker::Semantic(SemanticKind::PromptStart)]);
    }

    #[test]
    fn esc_bracket_inside_osc_restarts() {
        // `ESC ]` while mid-OSC discards the partial and begins fresh.
        let out = markers(b"\x1b]garbage\x1b]133;B\x07");
        assert_eq!(out, vec![Marker::Semantic(SemanticKind::PromptEnd)]);
    }

    #[test]
    fn parses_a_completion_response_round_trip() {
        use crate::completion::{encode_response, CompletionItem, CompletionResponse};
        let res = CompletionResponse {
            replace_start: 4,
            replace_end: 7,
            items: vec![
                CompletionItem { value: "checkout".into(), description: Some("switch".into()) },
                CompletionItem { value: "cherry-pick".into(), description: None },
            ],
        };
        // The scanner frames the emitted OSC and surfaces the parsed response.
        assert_eq!(markers(&encode_response(&res)), vec![Marker::Completion(res)]);
    }

    #[test]
    fn a_large_completion_response_within_the_bigger_cap_is_recognized() {
        use crate::completion::{encode_response, CompletionItem, CompletionResponse};
        // ~200 candidates: well past the general OSC cap, under the completion cap.
        // A non-completion OSC this long would be abandoned (see below).
        let items = (0..200)
            .map(|i| CompletionItem {
                value: format!("candidate-number-{i:04}-with-some-length"),
                description: Some("a description that adds bytes".into()),
            })
            .collect();
        let res = CompletionResponse { replace_start: 0, replace_end: 3, items };
        let bytes = encode_response(&res);
        assert!(bytes.len() > MAX_OSC, "the payload exceeds the general cap");
        assert!(bytes.len() <= MAX_COMPLETION_OSC, "but stays within the completion cap");
        assert_eq!(markers(&bytes), vec![Marker::Completion(res)]);
    }

    #[test]
    fn a_completion_response_past_the_bigger_cap_is_abandoned() {
        // The response prefix followed by enough bytes to blow the completion cap
        // is dropped rather than buffered without bound — no marker, no panic.
        let mut bytes = b"\x1b]5379;res;0;0".to_vec();
        bytes.extend(std::iter::repeat_n(b';', MAX_COMPLETION_OSC + 16));
        bytes.extend_from_slice(b"\x1b\\");
        assert!(markers(&bytes).is_empty());
    }

    #[test]
    fn a_general_osc_is_still_capped_at_max_osc() {
        // A valid-but-over-long OSC 7 is abandoned at the general cap, so it yields
        // no marker — proving the bigger cap is gated on the completion prefix, not
        // applied to every OSC.
        let mut bytes = b"\x1b]7;file://h/".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_OSC));
        bytes.extend_from_slice(b"\x1b\\");
        assert!(markers(&bytes).is_empty(), "an over-long OSC 7 is dropped, not parsed");
    }

    #[test]
    fn a_completion_response_split_across_chunks_is_recognized() {
        use crate::completion::{encode_response, CompletionItem, CompletionResponse};
        let res = CompletionResponse {
            replace_start: 1,
            replace_end: 2,
            items: vec![CompletionItem { value: "value".into(), description: None }],
        };
        let bytes = encode_response(&res);
        let mut s = Scanner::new();
        let mid = bytes.len() / 2;
        assert!(s.scan(&bytes[..mid]).is_empty(), "no marker before the terminator");
        let hits: Vec<Marker> = s.scan(&bytes[mid..]).into_iter().map(|(_, m)| m).collect();
        assert_eq!(hits, vec![Marker::Completion(res)]);
    }
}
