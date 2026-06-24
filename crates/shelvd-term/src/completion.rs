//! The shelvd completion-delegation wire protocol (epic #19, issue #40).
//!
//! shelvd owns the prompt editor (Approach O) but delegates *completion* to the
//! shell's own engine, fetched out-of-band. This module defines the two halves
//! of that exchange:
//!
//! - **Request** (shelvd → a responder): the partial line + cursor byte offset.
//!   A shell does not parse OSC sequences on its stdin, and under Approach O the
//!   live shell sits at readline with an empty buffer, so the request is *not* an
//!   OSC — it is a transport-neutral serialization a responder consumes (typed as
//!   a function invocation to a warm subshell, or read from the line a bound
//!   widget was triggered over). [`encode_request`] produces that serialization.
//!
//! - **Response** (a responder → shelvd): the candidate list, emitted as a custom
//!   **OSC sequence** the shell prints to its PTY, so it rides the same byte
//!   stream the OSC-133 markers do and is framed by [`crate::osc133::Scanner`].
//!   The scanner surfaces it as [`crate::osc133::Marker::Completion`]; the parser
//!   lives here ([`parse_response_payload`]) and [`encode_response`] is the
//!   reference encoder the per-shell responders mirror.
//!
//! All free-text fields (the line, candidate values, descriptions) are
//! percent-encoded with a strict unreserved set ([`crate::osc133::percent_encode_strict`]),
//! so the framing bytes — `;` between fields, `,` between a value and its
//! description, the OSC terminators, and any control/non-ASCII byte — can never
//! appear inside a field. That also keeps a request a single shell-safe token,
//! so the transport layer never has to quote it.

use crate::osc133::{percent_decode, percent_encode_strict};

/// The OSC payload prefix that frames a completion **response**. `5379` is a
/// shelvd-private OSC command code (no standard assignment; chosen clear of the
/// known private codes 9 / 99 / 133 / 777 / 1337), and the `res` sub-tag makes a
/// stray collision on the number alone fail to match.
pub(crate) const RESPONSE_PREFIX: &str = "5379;res;";

/// Cap on candidates a single response carries. Responders should rank and
/// truncate to this; [`encode_response`] enforces it so the encoded payload stays
/// well under the scanner's completion-OSC buffer bound. The overlay windows to
/// the visible rows anyway, so more candidates would not earn their bytes.
pub(crate) const MAX_ITEMS: usize = 256;

/// A completion request: complete `line` with the cursor at byte offset `cursor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionRequest {
    /// The partial command line to complete.
    pub line: String,
    /// The cursor position as a byte offset into `line` (`0..=line.len()`).
    pub cursor: usize,
}

/// One completion candidate: the text to insert, plus an optional description
/// (e.g. fish's per-candidate hint; bash has none, so `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// The candidate text that replaces the in-progress word.
    pub value: String,
    /// A short description shown beside the candidate, if the engine supplies one.
    pub description: Option<String>,
}

/// A completion response: the candidates, plus the byte range of the request line
/// they replace. `replace_start..replace_end` is the in-progress word (the same
/// span for every candidate — shells complete one token at a time), so the app
/// splices a chosen `value` over that range rather than appending it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionResponse {
    /// Byte offset into the request line where the replaced word begins.
    pub replace_start: usize,
    /// Byte offset into the request line where the replaced word ends (usually
    /// the request cursor).
    pub replace_end: usize,
    /// The candidates, best-ranked first; at most [`MAX_ITEMS`].
    pub items: Vec<CompletionItem>,
}

/// Serialize a request to its transport-neutral form: `"<cursor> <line%>"`, where
/// the line is strict-percent-encoded (so it holds no spaces and the single space
/// is an unambiguous separator). The transport layer adds its own framing (a
/// responder function name, a newline) around this.
pub fn encode_request(req: &CompletionRequest) -> String {
    format!("{} {}", req.cursor, percent_encode_strict(&req.line))
}

/// Parse a request produced by [`encode_request`]. Returns `None` if the cursor
/// field is missing or not a number. Mirrors what a shell responder does, and is
/// the round-trip partner of [`encode_request`] in tests.
pub fn parse_request(s: &str) -> Option<CompletionRequest> {
    let (cursor, line) = s.split_once(' ')?;
    Some(CompletionRequest {
        cursor: cursor.parse().ok()?,
        line: percent_decode(line),
    })
}

/// Encode a response to the full OSC byte sequence a responder prints
/// (`ESC ] 5379;res;… ST`). The candidate count is capped at [`MAX_ITEMS`]. This
/// is the reference the per-shell responders reproduce, and the round-trip
/// partner of the scanner's response parsing in tests.
pub fn encode_response(res: &CompletionResponse) -> Vec<u8> {
    let mut payload = format!("{RESPONSE_PREFIX}{};{}", res.replace_start, res.replace_end);
    for item in res.items.iter().take(MAX_ITEMS) {
        payload.push(';');
        payload.push_str(&percent_encode_strict(&item.value));
        if let Some(desc) = &item.description {
            payload.push(',');
            payload.push_str(&percent_encode_strict(desc));
        }
    }
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(b"\x1b\\"); // ST
    out
}

/// Parse the bytes of a completion-response OSC payload *after* [`RESPONSE_PREFIX`]
/// — i.e. `"<start>;<end>[;<value%>[,<desc%>]]…"`. Returns `None` for a malformed
/// header (non-UTF-8, or a start/end that is not a number); a stray empty
/// candidate field is skipped rather than rejected. Called by the scanner; never
/// panics, so a truncated or corrupt payload is dropped, not fatal.
pub(crate) fn parse_response_payload(rest: &[u8]) -> Option<CompletionResponse> {
    let text = std::str::from_utf8(rest).ok()?;
    let mut fields = text.split(';');
    let replace_start = fields.next()?.parse().ok()?;
    let replace_end = fields.next()?.parse().ok()?;
    let mut items = Vec::new();
    for field in fields {
        if field.is_empty() {
            continue;
        }
        let (value, description) = match field.split_once(',') {
            Some((v, d)) => (percent_decode(v), Some(percent_decode(d))),
            None => (percent_decode(field), None),
        };
        items.push(CompletionItem { value, description });
    }
    Some(CompletionResponse { replace_start, replace_end, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_strict_encoding() {
        // A line full of bytes that would break framing if left raw: spaces, a
        // semicolon, a comma, a percent, and a non-ASCII glyph.
        let req = CompletionRequest { line: "git ch; echo 50%, café".into(), cursor: 6 };
        let wire = encode_request(&req);
        // The wire form is a single space-separated pair with no raw framing bytes.
        assert!(wire.starts_with("6 "));
        assert!(!wire[2..].contains(' '), "the encoded line holds no literal spaces");
        assert!(!wire.contains(';') && !wire.contains(','));
        assert_eq!(parse_request(&wire), Some(req));
    }

    #[test]
    fn request_round_trips_empty_line() {
        let req = CompletionRequest { line: String::new(), cursor: 0 };
        assert_eq!(parse_request(&encode_request(&req)), Some(req));
    }

    #[test]
    fn parse_request_rejects_a_missing_cursor() {
        assert_eq!(parse_request("not-a-number x"), None);
        assert_eq!(parse_request("no-space-at-all"), None);
    }

    #[test]
    fn response_round_trips_with_descriptions_and_framing_chars() {
        let res = CompletionResponse {
            replace_start: 4,
            replace_end: 7,
            items: vec![
                CompletionItem { value: "checkout".into(), description: Some("switch branches".into()) },
                // A value and description carrying the framing bytes themselves.
                CompletionItem { value: "a;b,c%d e".into(), description: Some("x; y, z".into()) },
                CompletionItem { value: "no-desc".into(), description: None },
            ],
        };
        let bytes = encode_response(&res);
        // It is a single OSC framed by ESC ] … ESC \ with no raw framing bytes leaking.
        assert!(bytes.starts_with(b"\x1b]") && bytes.ends_with(b"\x1b\\"));
        let payload = &bytes[2..bytes.len() - 2];
        let rest = payload.strip_prefix(RESPONSE_PREFIX.as_bytes()).expect("has the response prefix");
        assert_eq!(parse_response_payload(rest), Some(res));
    }

    #[test]
    fn response_round_trips_empty_candidate_list() {
        let res = CompletionResponse { replace_start: 2, replace_end: 2, items: vec![] };
        let bytes = encode_response(&res);
        let rest = bytes[2..bytes.len() - 2]
            .strip_prefix(RESPONSE_PREFIX.as_bytes())
            .expect("prefix");
        assert_eq!(parse_response_payload(rest), Some(res));
    }

    #[test]
    fn parse_response_rejects_a_nonnumeric_range() {
        assert_eq!(parse_response_payload(b"x;3;value"), None);
        assert_eq!(parse_response_payload(b"3"), None, "missing the end offset");
    }

    #[test]
    fn encode_response_caps_the_candidate_count() {
        let items = (0..MAX_ITEMS + 50)
            .map(|i| CompletionItem { value: format!("c{i}"), description: None })
            .collect();
        let res = CompletionResponse { replace_start: 0, replace_end: 0, items };
        let bytes = encode_response(&res);
        let rest = bytes[2..bytes.len() - 2]
            .strip_prefix(RESPONSE_PREFIX.as_bytes())
            .unwrap();
        let parsed = parse_response_payload(rest).unwrap();
        assert_eq!(parsed.items.len(), MAX_ITEMS, "the count is clamped to MAX_ITEMS");
    }
}
