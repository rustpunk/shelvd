//! Completion sources for the owned editor (epic #19, #40): fetch candidates
//! from a shell's own engine and shape them into the protocol's
//! [`CompletionResponse`] for the completion menu.
//!
//! Each engine drives a **cold one-shot** subshell: **fish** via
//! `fish -c "complete -C …"`, **bash** via a one-shot `bash -c` that loads the
//! system completion infrastructure and replays the partial line through the
//! registered compspec, **zsh** via a one-shot `zsh` that hosts the real
//! completion system inside a `zpty` child and captures its matches. Completion is
//! Tab-triggered (a deliberate keypress), so a cold ~tens-of-ms invoke is
//! imperceptible and avoids a persistent-subshell state machine; a warm/live path
//! is a later optimization (#85) for full session-state fidelity. The app routes by
//! [`shelvd_pty::ShellKind`], so a shell without a cold engine here simply gets no
//! candidates and the caller falls back to readline.
//!
//! Every engine shells out under [`output_within_timeout`], so a wedged shell or a
//! pathological completion function can never freeze the owned editor on Tab.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shelvd_term::{CompletionItem, CompletionResponse};

/// What the event loop should do with a completion worker's result, decided by
/// [`CompletionState::resolve`]. Keeping the decision in a pure value — no winit,
/// no app state — makes the cancellation policy exhaustively unit-testable.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionOutcome {
    /// Stale (the owned line moved on) or owned editing is off — ignore the result.
    Discard,
    /// Open the completion menu with these candidates.
    Open(CompletionResponse),
    /// The engine found nothing — hand the line to readline.
    FallBack,
}

/// Generation bookkeeping for owned-editor completion. A request captures the
/// current [`CompletionState::dispatch`] generation; when its result arrives it is
/// applied only if the generation still matches — otherwise an owned-editor
/// keypress or a fresh prompt has [`CompletionState::invalidate`]d it. Pure and
/// single-threaded (it lives on the loop thread; workers only carry a generation
/// back), so there is no shared state and the whole policy is testable in isolation.
#[derive(Default)]
pub struct CompletionState {
    generation: u64,
}

impl CompletionState {
    /// Invalidate any in-flight request: a new owned keypress or fresh prompt makes
    /// a pending result stale.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// The generation to tag a request dispatched now. Call after [`Self::invalidate`].
    pub fn dispatch(&self) -> u64 {
        self.generation
    }

    /// Decide what to do with the result of request `generation`. `owned` is whether
    /// owned editing is still in effect. A result is applied only when it is both
    /// current and still owned; otherwise it is discarded.
    pub fn resolve(
        &self,
        generation: u64,
        owned: bool,
        response: Option<CompletionResponse>,
    ) -> CompletionOutcome {
        if !owned || generation != self.generation {
            CompletionOutcome::Discard
        } else if let Some(response) = response {
            CompletionOutcome::Open(response)
        } else {
            CompletionOutcome::FallBack
        }
    }
}

/// Carries a completion worker's result back to the event loop. Abstracts the
/// winit `EventLoopProxy` so [`spawn_completion_with`] is testable with a fake.
pub trait CompletionSink: Send + 'static {
    fn deliver(&self, generation: u64, response: Option<CompletionResponse>);
}

/// Run `engine` for `line`/`cursor` on a worker thread and deliver the result,
/// tagged with `generation`, through `sink`. Both the engine and the sink are
/// injected, so the worker round-trip is testable without spawning a shell or a
/// winit loop.
pub fn spawn_completion_with(
    engine: fn(&str, usize) -> Option<CompletionResponse>,
    line: String,
    cursor: usize,
    generation: u64,
    sink: impl CompletionSink,
) {
    std::thread::spawn(move || {
        let response = engine(&line, cursor);
        sink.deliver(generation, response);
    });
}

/// Wall-clock backstop for a completion subprocess. The engines run on a worker
/// thread (the caller delivers the result asynchronously), so this no longer gates
/// UI responsiveness — a legitimately slow completion (a networked CLI, a cold
/// `compinit`) returns late rather than being clipped. The cap exists only so a
/// truly wedged shell can't leak a thread and subprocess forever; a request the
/// user has already moved past is discarded by generation regardless.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(15);

/// Run `cmd` and return its stdout, killing the child and returning `None` if it
/// does not finish within `timeout`. A reader thread drains stdout so a chatty
/// child can't deadlock against a full pipe while the timer runs. `None` also
/// covers a failed spawn (shell absent) or a non-zero exit — every caller then
/// falls back to readline.
fn output_within_timeout(mut cmd: Command, timeout: Duration) -> Option<Vec<u8>> {
    let mut child =
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    // Drain stdout on a thread: a chatty child blocks writing once the pipe buffer
    // fills, and we'd then never observe its exit.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = rx.recv().ok()?; // the reader finishes as the pipe closes
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(8)),
            Err(_) => return None,
        }
    }
}

/// Fetch fish completions for `line` with the caret at byte offset `cursor`.
/// Returns `None` when fish is unavailable, errors, or yields nothing — the
/// caller then falls back to readline. The replacement range is the in-progress
/// word (the last whitespace-delimited token up to the caret).
pub fn fish_complete(line: &str, cursor: usize) -> Option<CompletionResponse> {
    // Complete the line up to the caret; snap a stray offset to a char boundary.
    let cursor = floor_char_boundary(line, cursor.min(line.len()));
    let prefix = &line[..cursor];
    let script = format!("complete -C -- {}", fish_single_quote(prefix));
    let mut cmd = Command::new("fish");
    cmd.arg("-c").arg(script);
    let stdout = output_within_timeout(cmd, COMPLETION_TIMEOUT)?;
    let items = parse_completions(&String::from_utf8_lossy(&stdout));
    if items.is_empty() {
        return None;
    }
    Some(CompletionResponse { replace_start: last_token_start(prefix), replace_end: cursor, items })
}

/// A self-contained bash script that replays a partial command line through
/// bash's own programmable completion and prints the candidates, one per line.
///
/// `$1` is the line prefix to complete (everything up to the caret). The script
/// loads the system completion infrastructure (best-effort), reconstructs the
/// `COMP_*` environment bash hands a compspec, lazily loads the spec for the
/// command, then either calls the registered `-F` function and dumps `COMPREPLY`,
/// or falls back to `compgen` (command names for the first word, filenames
/// otherwise). Word splitting is whitespace-only — a first-cut boundary matching
/// [`last_token_start`]; `COMP_WORDBREAKS` punctuation (`:`, `=`) is not honored.
/// The line arrives as an argv argument, never interpolated into the script, so
/// an arbitrary line cannot inject shell.
const BASH_DRIVER: &str = r#"
line="$1"
for f in /usr/share/bash-completion/bash_completion /etc/bash_completion; do
  if [[ -r "$f" ]]; then source "$f" >/dev/null 2>&1; break; fi
done
COMP_LINE="$line"
COMP_POINT="${#COMP_LINE}"
read -ra COMP_WORDS <<< "$COMP_LINE"
if [[ "$COMP_LINE" == *[[:space:]] || ${#COMP_WORDS[@]} -eq 0 ]]; then
  COMP_WORDS+=("")
fi
COMP_CWORD=$(( ${#COMP_WORDS[@]} - 1 ))
cmd="${COMP_WORDS[0]}"
cur="${COMP_WORDS[COMP_CWORD]}"
prev=""
(( COMP_CWORD > 0 )) && prev="${COMP_WORDS[COMP_CWORD-1]}"
# The first word names a command: complete from PATH/builtins/aliases.
if (( COMP_CWORD == 0 )); then
  compgen -c -- "$cur"
  exit 0
fi
# Trigger the on-demand loader so the command's compspec is registered.
if declare -F _completion_loader >/dev/null 2>&1; then
  _completion_loader "$cmd" >/dev/null 2>&1
fi
spec="$(complete -p "$cmd" 2>/dev/null)"
if [[ "$spec" == *" -F "* ]]; then
  func="${spec#* -F }"
  func="${func%% *}"
  COMPREPLY=()
  "$func" "$cmd" "$cur" "$prev" >/dev/null 2>&1
  printf '%s\n' "${COMPREPLY[@]}"
else
  # No completion function registered: fall back to filename completion.
  compgen -f -- "$cur"
fi
"#;

/// Fetch bash completions for `line` with the caret at byte offset `cursor`.
/// Returns `None` when bash is unavailable, errors, or yields nothing — the
/// caller then falls back to readline. Drives bash's own programmable completion
/// via a one-shot [`BASH_DRIVER`]; the replacement range is the in-progress word
/// (the last whitespace-delimited token up to the caret).
///
/// This is the cold engine: a fresh `bash -c` cannot see the live session's
/// interactively-defined aliases/functions/vars (that is #85's live transport),
/// but it reflects every compspec the system installs.
pub fn bash_complete(line: &str, cursor: usize) -> Option<CompletionResponse> {
    // Complete the line up to the caret; snap a stray offset to a char boundary.
    let cursor = floor_char_boundary(line, cursor.min(line.len()));
    let prefix = &line[..cursor];
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(BASH_DRIVER).arg("shelvd-complete").arg(prefix);
    let stdout = output_within_timeout(cmd, COMPLETION_TIMEOUT)?;
    let items = parse_bash_completions(&String::from_utf8_lossy(&stdout));
    if items.is_empty() {
        return None;
    }
    Some(CompletionResponse { replace_start: last_token_start(prefix), replace_end: cursor, items })
}

/// A self-contained zsh script that captures what zsh's own completion system
/// would offer for a partial line and prints the candidates, one per line as
/// `value<TAB>description` (the description present only when zsh supplies one).
///
/// zsh has no stdout-printing `compgen`/`complete -C`, so the script hosts a real
/// interactive zsh inside a `zpty` child and turns completion into a capture: it
/// neutralizes execution (Enter is unbound; Tab runs `complete-word`) and replaces
/// the `compadd` builtin with a hook that diverts matches into an array and prints
/// them instead of inserting them — the established "zsh-capture-completion"
/// technique. `compprefuncs`/`comppostfuncs` frame one completion pass with private
/// sentinels, and the post hook `exit`s the child so the pty closes and the read
/// loop ends. `$1` is the line prefix; it is fed to the child as keystrokes (never
/// executed), so an arbitrary line cannot inject shell.
///
/// `setopt rcquotes` makes `''` denote a literal single quote inside the
/// single-quoted setup that is sourced into the child.
const ZSH_DRIVER: &str = r#"
setopt rcquotes
zmodload zsh/zpty 2>/dev/null || exit 1
zpty CAP zsh -f -i
_shelvd_wait() {
  local want=$1 line acc=
  integer n=0
  while zpty -r CAP line; do
    acc+=$line
    [[ $acc == *$want* ]] && return 0
    (( ++n > 4000 )) && return 1
  done
  return 1
}
() { zpty -w CAP "source $1"; _shelvd_wait SHELVD_READY || exit 2 } =( <<< '
PROMPT=
RPROMPT=
autoload -Uz compinit && compinit -u -d "${TMPDIR:-/tmp}/shelvd-zcompdump-$UID" >/dev/null 2>&1
zmodload zsh/zutil
# Never run a command: Enter is inert, Tab completes.
bindkey ''^M'' undefined
bindkey ''^J'' undefined
bindkey ''^I'' complete-word
zstyle '':completion:*'' list-grouped false
zstyle '':completion:*'' insert-tab false
# Frame one completion pass; the post hook exits so the pty closes.
_shelvd_pre()  { print -r -- SHELVD_BEGIN }
_shelvd_post() { print -r -- SHELVD_END; exit }
compprefuncs=( _shelvd_pre )
comppostfuncs=( _shelvd_post )
# Divert matches instead of inserting them.
compadd () {
  # Calls that already capture (-O/-A/-D) must pass straight through.
  if [[ ${@[1,(i)(-|--)]} == *-(O|A|D)\ * ]]; then
    builtin compadd "$@"
    return $?
  fi
  typeset -a __hits __dscr
  if (( $@[(I)-d] )); then
    local __t=${@[$[${@[(i)-d]}+1]]}
    if [[ $__t == \(* ]]; then
      eval "__dscr=$__t"
    else
      __dscr=( "${(@P)__t}" )
    fi
  fi
  builtin compadd -A __hits -D __dscr "$@"
  setopt localoptions norcexpandparam extendedglob
  typeset -A apre hpre hsuf asuf
  zparseopts -E P:=apre p:=hpre S:=asuf s:=hsuf
  [[ -n $__hits ]] || return
  local i dscr
  for i in {1..$#__hits}; do
    (( $#__dscr >= i )) && dscr=$''\t''${${__dscr[i]}##$__hits[i] #} || dscr=
    print -r -- $IPREFIX$apre$hpre$__hits[i]$hsuf$asuf$dscr
  done
}
print -r -- SHELVD_READY')
zpty -w CAP "$1"$'\t'
integer infr=0
while zpty -r CAP; do :; done | while IFS= read -r line; do
  line=${line%$'\r'}
  [[ $line == *SHELVD_BEGIN* ]] && { infr=1; continue; }
  [[ $line == *SHELVD_END* ]] && break
  (( infr )) && print -r -- $line
done
exit 0
"#;

/// Fetch zsh completions for `line` with the caret at byte offset `cursor`.
/// Returns `None` when zsh is unavailable, errors, times out, or yields nothing —
/// the caller then falls back to readline. Captures zsh's own completion via a
/// one-shot [`ZSH_DRIVER`]; the replacement range is the in-progress word (the last
/// whitespace-delimited token up to the caret).
///
/// This is the cold engine: a fresh `zsh` cannot see the live session's
/// interactively-defined aliases/functions/vars (that is #85's live transport),
/// but it reflects every completion the system installs, descriptions included.
pub fn zsh_complete(line: &str, cursor: usize) -> Option<CompletionResponse> {
    // Complete the line up to the caret; snap a stray offset to a char boundary.
    let cursor = floor_char_boundary(line, cursor.min(line.len()));
    let prefix = &line[..cursor];
    let mut cmd = Command::new("zsh");
    cmd.arg("-f").arg("-c").arg(ZSH_DRIVER).arg("shelvd-complete").arg(prefix);
    let stdout = output_within_timeout(cmd, COMPLETION_TIMEOUT)?;
    let items = parse_zsh_completions(&String::from_utf8_lossy(&stdout));
    if items.is_empty() {
        return None;
    }
    Some(CompletionResponse { replace_start: last_token_start(prefix), replace_end: cursor, items })
}

/// Parse the bash driver's output: one candidate per line, no descriptions
/// (bash carries none). Compspecs append a trailing-space suffix to a complete
/// match, so trailing spaces are stripped; command completion lists the same name
/// once per source (builtin, alias, PATH), so duplicates are dropped, first
/// occurrence wins.
fn parse_bash_completions(stdout: &str) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    stdout
        .lines()
        .filter_map(|line| {
            let value = line.trim_end_matches(' ');
            (!value.is_empty() && seen.insert(value.to_owned()))
                .then(|| CompletionItem { value: value.to_owned(), description: None })
        })
        .collect()
}

/// Parse the zsh driver's output: one candidate per line, `value<TAB>description`
/// where present. zsh formats descriptions as `-- text`, so a leading `-- ` is
/// stripped; matches can repeat across completion tags, so duplicate values are
/// dropped, first occurrence wins.
fn parse_zsh_completions(stdout: &str) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    stdout
        .lines()
        .filter_map(|line| {
            let (value, description) = match line.split_once('\t') {
                Some((v, d)) => {
                    let d = d.trim();
                    let d = d.strip_prefix("-- ").unwrap_or(d).trim();
                    (v, (!d.is_empty()).then(|| d.to_owned()))
                }
                None => (line, None),
            };
            if value.is_empty() || !seen.insert(value.to_owned()) {
                return None;
            }
            Some(CompletionItem { value: value.to_owned(), description })
        })
        .collect()
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

    #[test]
    fn parse_bash_completions_trims_suffix_and_dedups() {
        // git's compspec appends a trailing space to each match; command
        // completion repeats a name once per source (builtin + PATH).
        let out = "checkout \ncherry-pick \ncherry \n";
        let items = parse_bash_completions(out);
        assert_eq!(items.iter().map(|i| i.value.as_str()).collect::<Vec<_>>(), [
            "checkout",
            "cherry-pick",
            "cherry"
        ]);
        assert!(items.iter().all(|i| i.description.is_none()), "bash carries no descriptions");

        let deduped = parse_bash_completions("echo\necho\necho\n");
        assert_eq!(deduped.len(), 1, "duplicate command names collapse to one");
        assert_eq!(deduped[0].value, "echo");
    }

    #[test]
    fn parse_bash_completions_skips_blank_lines() {
        assert!(parse_bash_completions("\n \n").is_empty(), "blank and all-space lines drop out");
        assert_eq!(parse_bash_completions("only\n").len(), 1);
    }

    #[test]
    fn bash_complete_lists_command_names_when_bash_is_present() {
        // Self-skips where bash isn't installed, so the suite stays green without
        // it. Command completion (the first word) needs only the `compgen` builtin,
        // not the bash-completion package, so this stays deterministic across hosts.
        let present = Command::new("bash")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !present {
            eprintln!("skipping bash_complete live check: bash not on PATH");
            return;
        }
        let resp = bash_complete("ech", 3).expect("bash returns candidates for 'ech'");
        assert!(resp.items.iter().any(|i| i.value == "echo"), "'echo' is among the candidates");
        assert_eq!((resp.replace_start, resp.replace_end), (0, 3), "the single token spans 0..3");
    }

    #[test]
    fn parse_zsh_completions_splits_dedups_and_strips_dashes() {
        // zsh repeats `echo` across tags and prefixes descriptions with `-- `.
        let out = "echo\nechotc\necho\n--color\t-- control use of color\n--help\tshow help\n";
        let items = parse_zsh_completions(out);
        assert_eq!(items.iter().map(|i| i.value.as_str()).collect::<Vec<_>>(), [
            "echo", "echotc", "--color", "--help"
        ]);
        assert_eq!(items[0].description, None);
        assert_eq!(items[2].description.as_deref(), Some("control use of color"), "'-- ' stripped");
        assert_eq!(items[3].description.as_deref(), Some("show help"));
    }

    #[test]
    fn parse_zsh_completions_skips_blank_lines() {
        assert!(parse_zsh_completions("\n\n").is_empty());
        assert_eq!(parse_zsh_completions("only\n").len(), 1);
    }

    #[test]
    fn zsh_complete_lists_candidates_when_zsh_is_present() {
        // Self-skips where zsh isn't installed, so the suite stays green without it.
        let present = Command::new("zsh")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !present {
            eprintln!("skipping zsh_complete live check: zsh not on PATH");
            return;
        }
        let resp = zsh_complete("ech", 3).expect("zsh returns candidates for 'ech'");
        assert!(resp.items.iter().any(|i| i.value == "echo"), "'echo' is among the candidates");
        assert_eq!((resp.replace_start, resp.replace_end), (0, 3), "the single token spans 0..3");
    }

    #[cfg(unix)]
    #[test]
    fn output_within_timeout_returns_a_fast_child_stdout() {
        let mut cmd = Command::new("printf");
        cmd.arg("hi");
        let out = output_within_timeout(cmd, Duration::from_secs(5)).expect("printf exits in budget");
        assert_eq!(out, b"hi");
    }

    #[cfg(unix)]
    #[test]
    fn output_within_timeout_kills_a_slow_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let start = Instant::now();
        let out = output_within_timeout(cmd, Duration::from_millis(200));
        assert!(out.is_none(), "a child past the budget yields no output");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "returns promptly after the timeout, not after the child exits"
        );
    }

    fn a_response() -> CompletionResponse {
        CompletionResponse {
            replace_start: 0,
            replace_end: 2,
            items: vec![CompletionItem { value: "echo".to_owned(), description: None }],
        }
    }

    #[test]
    fn resolve_opens_a_current_owned_some_result() {
        let cs = CompletionState::default();
        let gen = cs.dispatch();
        assert_eq!(cs.resolve(gen, true, Some(a_response())), CompletionOutcome::Open(a_response()));
    }

    #[test]
    fn resolve_falls_back_when_the_engine_is_empty() {
        let cs = CompletionState::default();
        let gen = cs.dispatch();
        assert_eq!(cs.resolve(gen, true, None), CompletionOutcome::FallBack);
    }

    #[test]
    fn resolve_discards_a_result_made_stale_by_a_keypress() {
        let mut cs = CompletionState::default();
        let gen = cs.dispatch(); // request dispatched
        cs.invalidate(); // a keypress happened before the result arrived
        assert_eq!(cs.resolve(gen, true, Some(a_response())), CompletionOutcome::Discard);
    }

    #[test]
    fn resolve_discards_when_owned_editing_is_off() {
        let cs = CompletionState::default();
        let gen = cs.dispatch();
        assert_eq!(cs.resolve(gen, false, Some(a_response())), CompletionOutcome::Discard);
        assert_eq!(cs.resolve(gen, false, None), CompletionOutcome::Discard);
    }

    #[test]
    fn only_the_newest_of_two_in_flight_requests_resolves() {
        // Two dispatches without a result in between (out-of-order workers / a
        // double Tab): the older generation must be discarded, the newer applied.
        let mut cs = CompletionState::default();
        cs.invalidate();
        let first = cs.dispatch();
        cs.invalidate();
        let second = cs.dispatch();
        assert_ne!(first, second);
        assert_eq!(cs.resolve(first, true, Some(a_response())), CompletionOutcome::Discard);
        assert_eq!(cs.resolve(second, true, None), CompletionOutcome::FallBack);
    }

    #[test]
    fn invalidate_wraps_without_panicking() {
        let mut cs = CompletionState { generation: u64::MAX };
        cs.invalidate();
        assert_eq!(cs.dispatch(), 0, "generation wraps rather than overflowing");
    }

    #[test]
    fn spawn_completion_with_delivers_the_tagged_engine_result() {
        // A fake sink + fake engine exercise the worker round-trip with no winit
        // loop and no shell: the engine runs with the given line/cursor and its
        // result is delivered tagged with the request generation.
        struct ChanSink(mpsc::Sender<(u64, Option<CompletionResponse>)>);
        impl CompletionSink for ChanSink {
            fn deliver(&self, generation: u64, response: Option<CompletionResponse>) {
                let _ = self.0.send((generation, response));
            }
        }
        fn echo_engine(line: &str, cursor: usize) -> Option<CompletionResponse> {
            Some(CompletionResponse {
                replace_start: 0,
                replace_end: cursor,
                items: vec![CompletionItem { value: line.to_owned(), description: None }],
            })
        }
        fn empty_engine(_: &str, _: usize) -> Option<CompletionResponse> {
            None
        }

        let (tx, rx) = mpsc::channel();
        spawn_completion_with(echo_engine, "git".to_owned(), 3, 42, ChanSink(tx.clone()));
        let (generation, response) = rx.recv_timeout(Duration::from_secs(5)).expect("worker delivered");
        assert_eq!(generation, 42, "the result carries the request generation");
        let response = response.expect("echo_engine returned Some");
        assert_eq!(response.items[0].value, "git", "the engine ran with the given line");
        assert_eq!(response.replace_end, 3, "the engine ran with the given cursor");

        spawn_completion_with(empty_engine, "x".to_owned(), 1, 7, ChanSink(tx));
        let (generation, response) = rx.recv_timeout(Duration::from_secs(5)).expect("worker delivered");
        assert_eq!((generation, response), (7, None), "an empty engine delivers a tagged None");
    }
}
