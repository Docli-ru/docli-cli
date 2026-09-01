// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The CLI's ONE output vocabulary (0.1.1).
//!
//! Before this module every command invented its own shape — bare `println!` lines, three
//! different prefixes, and a mix of Russian and English that made the tool read like several
//! tools. The rules below follow the established CLI-design guidance (clig.dev, Heroku's CLI
//! style guide, the 12-factor-CLI conventions) rather than taste:
//!
//! * **stdout is what the command was asked to PRODUCE; stderr is how it is going.** Search
//!   hits, status rows and `--json` go to stdout; headings, progress, successes and warnings go
//!   to stderr. So `docli search q | grep`, `docli status --json | jq` and `docli sync 2>/dev/null`
//!   all behave, and an agent parsing stdout never has to skip our chatter.
//! * **Message TEXT is ASCII; only DECORATION is extended.** A sentence reads the same with a
//!   hyphen as with an em dash, so message strings use ASCII punctuation unconditionally and
//!   cannot arrive as mojibake. The markers, the box rule, the prompt symbols and the identity
//!   block are the decorative layer, and each consults [`unicode`] — a UTF-8 terminal gets the
//!   nice interface, `LANG=C` gets a plain and complete one.
//! * **One marker per line class**: `✓` done, `!` attention, `✗` refusal, `→` what to do next.
//!   Each has an ASCII fallback for terminals that cannot render it (`console::Emoji`).
//! * **Colour is decoration, never information.** Every line reads the same stripped. Colour is
//!   off automatically for a non-TTY, `NO_COLOR`, `TERM=dumb`, and explicitly for `--no-color`.
//! * **`--quiet` silences the chatter, never the results or the refusals** — the flag exists so
//!   scripts can drop the narrative without `2>/dev/null` swallowing real errors too.
//! * **Prompts only at an attended terminal**, and never under `--no-input`: a wizard that
//!   blocks in CI is worse than no wizard.
//!
//! **The CLI speaks English.** It is a developer tool, and developer tools speak English —
//! including in this market: Yandex Cloud's `yc` and Timeweb's own `twc` both ship an English
//! CLI with Russian documentation. Two things follow. The output stays ASCII-safe, so it cannot
//! arrive as mojibake in a terminal that cannot render Cyrillic (`сервер` decoded as cp1251 is
//! `СЃРµСЂРІРµСЂ`), and extended characters are used only where [`unicode`] says they render.
//! docli's RUSSIAN surface is the product itself: the app, the site, and README.ru.md.

use std::sync::atomic::{AtomicBool, Ordering};

use console::{style, Term};

static QUIET: AtomicBool = AtomicBool::new(false);
static NO_INPUT: AtomicBool = AtomicBool::new(false);
static REPORT: AtomicBool = AtomicBool::new(false);
static MACHINE: AtomicBool = AtomicBool::new(false);

/// Applied once, from the global flags, before any command runs.
pub fn configure(quiet: bool, no_color: bool, no_input: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
    NO_INPUT.store(no_input, Ordering::Relaxed);
    if no_color {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// REPORT MODE — for commands whose entire output IS the product (`status`, human-readable
/// `doctor`, `search`). Everything then goes to stdout, so `docli status > file` keeps its
/// headings, and `--quiet` does not blank the very thing that was asked for. Off by default,
/// where the split holds: results on stdout, how-it-is-going on stderr.
pub fn report_mode() {
    REPORT.store(true, Ordering::Relaxed);
}

fn reporting() -> bool {
    REPORT.load(Ordering::Relaxed)
}

/// MACHINE MODE — this run's stdout is `--json` for a script. Nothing may prompt: the caller
/// cannot answer, so an offer becomes a hang. Set once by the `--json` arms, and consulted by
/// [`interactive`] so a prompt deep inside a shared helper (the `.gitignore` offer sits inside
/// `validate_geometry`'s caller) cannot escape the rule.
pub fn machine_mode() {
    MACHINE.store(true, Ordering::Relaxed);
}

/// Can this terminal render non-ASCII? Extended characters are used wherever they make the
/// output better — `✓`, `→`, `—`, box rules — and swapped for ASCII only where they would
/// arrive as mojibake instead.
///
/// The check is the environment's own declaration, because nothing else is knowable from here:
///
/// * **Windows**: yes. Rust's std writes to a console handle with `WriteConsoleW` (UTF-16), so
///   the active code page cannot mangle it, and a redirected stream is bytes either way.
/// * **Unix**: the first of `LC_ALL`, `LC_CTYPE`, `LANG` that is SET decides, by whether it
///   names UTF-8. `LANG=C`, an empty environment (cron, some CI, `env -i`) and a legacy charset
///   all answer no — which is exactly where a `✓` would arrive as `Ã¢Å"“`.
pub fn unicode() -> bool {
    if cfg!(windows) {
        return true;
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
        .unwrap_or(false)
}

/// Status markers, each with an ASCII fallback for a terminal that cannot render the first.
fn done() -> &'static str {
    if unicode() {
        "✓"
    } else {
        "+"
    }
}

fn refused() -> &'static str {
    if unicode() {
        "✗"
    } else {
        "x"
    }
}

fn next_marker() -> &'static str {
    if unicode() {
        "→"
    } else {
        ">"
    }
}

/// An arrow for INSIDE a sentence («mounted → dir»), gated like every other decoration. The
/// `next()` marker has its own; this one is for message text that genuinely reads better with
/// one, which is the only place message text is allowed a non-ASCII character.
pub fn arrow() -> &'static str {
    if unicode() {
        "→"
    } else {
        "->"
    }
}

/// Terminal width for rules, with a sane fallback for pipes.
fn width() -> usize {
    Term::stderr().size().1.clamp(40, 100) as usize
}

/// Chatter: suppressed by `--quiet`, on stderr — unless this command is a report, in which
/// case it is the product and goes to stdout regardless.
fn chatter(s: String) {
    if reporting() {
        println!("{s}");
    } else if !quiet() {
        eprintln!("{s}");
    }
}

/// A section heading.
pub fn heading(text: &str) {
    chatter(format!("\n{}", style(text).bold()));
}

/// A wizard step: `[2/6] Пространство`.
pub fn step(n: usize, total: usize, title: &str) {
    chatter(format!(
        "\n{} {}",
        style(format!("[{n}/{total}]")).dim(),
        style(title).bold()
    ));
}

/// Something finished.
pub fn ok(text: &str) {
    chatter(format!("{} {text}", style(done()).green().bold()));
}

/// A secondary detail under the line above it.
pub fn detail(text: &str) {
    chatter(format!("  {}", style(text).dim()));
}

/// What the reader should do next. Suggesting the following command is a documented pattern,
/// not decoration — it is how people discover the rest of a CLI.
pub fn next(text: &str) {
    chatter(format!("{} {text}", style(next_marker()).cyan()));
}

/// Something needs attention but nothing failed. Survives `--quiet`: a warning the reader
/// asked not to see is a warning that will be missed.
pub fn warn(text: &str) {
    let s = format!("{} {text}", style("!").yellow().bold());
    if reporting() {
        println!("{s}");
    } else {
        eprintln!("{s}");
    }
}

/// A refusal reported while the command carries on with its other work (a skipped mount, an
/// agent config it could not write). Survives `--quiet`.
pub fn refuse(text: &str) {
    let s = format!("{} {text}", style(refused()).red().bold());
    if reporting() {
        println!("{s}");
    } else {
        eprintln!("{s}");
    }
}

/// The heading OF a result block (a listing's title). Stdout, like the rows beneath it: a
/// heading on the other stream is lost by `> file` and only lines up on screen by luck.
pub fn result_heading(text: &str) {
    println!("\n{}", style(text).bold());
}

/// RESULT output — stdout, never suppressed. Search hits, status rows, listings.
pub fn line(text: &str) {
    println!("{text}");
}

/// An aligned `label   value` result row. `pad` comes from [`label_width`] so a block lines up.
pub fn field(label: &str, value: &str, pad: usize) {
    println!(
        "  {:<pad$}  {}",
        style(label).dim(),
        value,
        pad = pad.max(label.chars().count())
    );
}

/// The label column width for a block of [`field`] rows — measured in CHARACTERS, so Cyrillic
/// labels align like ASCII ones.
pub fn label_width<'a>(labels: impl IntoIterator<Item = &'a str>) -> usize {
    labels
        .into_iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
}

/// «1 node» / «2 nodes». Trivial in English — and that is part of why the CLI speaks it: the
/// Russian form needs three cases keyed on the last two digits, and getting it wrong is the
/// loudest possible tell that a string was written by a machine.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Strip control characters from text that came from OUTSIDE this program.
///
/// `docli.toml` is committed and teammate-editable, and so are workspace names on the server:
/// a `name = "\u{1b}[2Jforged"` would clear the screen and forge output on someone else's
/// terminal when they run `docli status`. Rendering foreign text is exactly where that has to
/// be stopped — validation refuses such values too, but `status` deliberately reports on
/// configurations it has not validated, which is precisely the case that matters.
///
/// TAB is kept (it is legitimate inside a name); everything else in the control range goes.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\t' || !c.is_control() {
                c
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

/// Inline styles for values quoted inside a sentence. Functions rather than raw `style()` calls
/// at the call sites, so the palette lives in ONE place.
pub fn path(p: &str) -> String {
    style(p).cyan().to_string()
}

/// A command the reader can type.
pub fn cmd(c: &str) -> String {
    style(c).yellow().to_string()
}

/// Dimmed secondary text inside a sentence.
pub fn dim(v: &str) -> String {
    style(v).dim().to_string()
}

/// May this run ask a question? An unattended terminal or an explicit `--no-input` means no —
/// a prompt in CI hangs a pipeline until it times out.
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    !NO_INPUT.load(Ordering::Relaxed)
        && !MACHINE.load(Ordering::Relaxed)
        && console::user_attended_stderr()
        && Term::stdout().is_term()
        // STDIN is the one the prompt actually READS: with `tail -f /dev/null | docli init`
        // the other two are still a terminal, and dialoguer would block forever on a pipe
        // that never delivers a line.
        && std::io::stdin().is_terminal()
}

/// A single-line, in-place progress report for work that takes long enough to look stalled.
///
/// Deliberately not a progress BAR: a sync's total is unknown until the last page arrives, and
/// a bar that cannot fill is worse than a count that keeps moving. Silent whenever the output
/// is not an attended terminal (no «Christmas tree» in CI logs) or `--quiet` is set, and it
/// always [`finish`](Progress::finish)es by clearing its line so the summary is not appended to
/// a half-drawn one.
pub struct Progress {
    term: Term,
    live: bool,
    label: String,
}

impl Progress {
    pub fn new(label: &str) -> Self {
        let term = Term::stderr();
        let live = !quiet() && term.is_term() && console::user_attended_stderr();
        Progress {
            term,
            live,
            label: label.to_string(),
        }
    }

    pub fn set(&self, detail: &str) {
        if !self.live {
            return;
        }
        let _ = self.term.clear_line();
        // Clip the PLAIN text and style afterwards: clipping the styled string counts escape
        // bytes toward the limit and can cut a sequence in half, which leaves the rest of the
        // terminal dimmed.
        let plain = format!("  {} {}", self.label, detail);
        let max = width().saturating_sub(1);
        let clipped: String = plain.chars().take(max).collect();
        let _ = self.term.write_str(&format!("\r{}", style(clipped).dim()));
    }

    pub fn finish(self) {
        if self.live {
            let _ = self.term.clear_line();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_degrade_to_ascii_without_a_utf8_locale() {
        // The check reads the environment's own declaration: a `✓` in a Latin-1 terminal
        // arrives as mojibake, and a `+` is the honest substitute.
        let saved: Vec<(String, Option<String>)> = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for k in ["LC_ALL", "LC_CTYPE", "LANG"] {
            std::env::remove_var(k);
        }
        std::env::set_var("LC_ALL", "en_US.UTF-8");
        assert!(unicode());
        assert_eq!(done(), "✓");
        std::env::set_var("LC_ALL", "C");
        assert_eq!(unicode(), cfg!(windows));
        if !cfg!(windows) {
            assert_eq!(done(), "+");
        }
        // An unset environment is the cron/CI case: assume nothing.
        std::env::remove_var("LC_ALL");
        assert_eq!(unicode(), cfg!(windows));
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
    }

    #[test]
    fn foreign_text_cannot_forge_terminal_output() {
        // A committed docli.toml or a server-side workspace name reaches someone else's
        // terminal; an escape sequence in it must not be executed there.
        let forged = "\u{1b}[2Jforged\u{7}";
        let safe = sanitize(forged);
        assert!(!safe.contains('\u{1b}'), "{safe:?}");
        assert!(!safe.contains('\u{7}'), "{safe:?}");
        assert!(
            safe.contains("forged"),
            "the readable part survives: {safe:?}"
        );
        // Ordinary text, including non-ASCII and tabs, is untouched.
        assert_eq!(sanitize("Документация\tдокли"), "Документация\tдокли");
    }

    #[test]
    fn plurals_agree() {
        assert_eq!(plural(0, "node", "nodes"), "0 nodes");
        assert_eq!(plural(1, "node", "nodes"), "1 node");
        assert_eq!(plural(2, "node", "nodes"), "2 nodes");
        assert_eq!(plural(442, "node", "nodes"), "442 nodes");
    }

    #[test]
    fn label_width_measures_characters_not_bytes() {
        // A Cyrillic label is 2 bytes per char; padding by bytes would over-indent every ASCII
        // row in the same block.
        assert_eq!(label_width(["сервер", "вход"]), 6);
        assert_eq!(label_width(["a", "abc"]), 3);
        assert_eq!(label_width([]), 0);
    }

    #[test]
    fn width_is_clamped_for_pipes() {
        let w = width();
        assert!((40..=100).contains(&w), "{w}");
    }

    #[test]
    fn machine_mode_closes_the_prompt_door_too() {
        // `docli doctor --json` reaches the same `.gitignore` offer as a plain run, through a
        // helper that cannot see the flag — so the flag has to live where `interactive` looks.
        configure(false, false, false);
        machine_mode();
        assert!(!interactive());
        MACHINE.store(false, Ordering::Relaxed);
    }

    #[test]
    fn no_input_closes_the_prompt_door() {
        // The wizard, the agent picker and uninstall's confirmation all gate on this; a stale
        // `true` here is a hung pipeline.
        configure(false, false, true);
        assert!(!interactive());
        configure(false, false, false);
    }

    #[test]
    fn report_mode_survives_quiet() {
        // `docli status --quiet` must still print the status: the report IS the product, and a
        // flag meant to drop narration must not blank it.
        configure(true, false, false);
        report_mode();
        assert!(reporting());
        assert!(quiet());
        REPORT.store(false, Ordering::Relaxed);
        configure(false, false, false);
    }

    #[test]
    fn quiet_is_scoped_to_chatter() {
        // Not an output assertion (the macros write to the real streams) — the pin is that the
        // flag reaches the one place the chatter helpers consult, and that warnings/refusals
        // never read it.
        configure(true, false, false);
        assert!(quiet());
        configure(false, false, false);
        assert!(!quiet());
    }

    #[test]
    fn progress_is_silent_when_no_one_is_watching() {
        // In a test run stderr is not a terminal, so the live flag must be off: a Progress that
        // wrote anyway would corrupt piped output with carriage returns.
        let p = Progress::new("sync");
        assert!(!p.live);
        p.set("372 nodes");
        p.finish();
    }
}
