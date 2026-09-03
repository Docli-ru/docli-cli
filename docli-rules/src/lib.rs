// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! docli's shared PURE client rules (v0.28.0 D1) — the licensable home for logic that must hold
//! the SAME answer on both ends of the sync wire and ships in the public CLI mirror.
//!
//! Four rule families live here:
//! - the **note-name rule** (`is_note_name`, moved from docli-core's `path.rs`; core re-exports
//!   it so the rule keeps ONE definition) — the A3 truncation guard's shared half;
//! - the **name-fold rule** (`fold_path`) — NFC everywhere, case-fold only on default
//!   case-insensitive filesystems; the plugin's `foldPath` closure is its JS twin, pinned by the
//!   shared vector file `vectors/fold.json` (both sides consume the SAME file — the twins ship
//!   on different release trains, so the vectors are the contract);
//! - the **Windows path projection** (`winpath`) — the Rust twin of the plugin's `winPath.ts`
//!   (v0.27.1 D8), pinned by `vectors/winpath.json` the same way;
//! - the **connection-label grammar** (`valid_label`, moved from docli-core's `models.rs` in
//!   v0.28.0 D12; core re-exports it) — the CLI validates the labels it writes into agent MCP
//!   configs against the same rule the server's labeled routes enforce.
//!
//! Plus one four-character doc-twin: [`wikilink_expressible`], mirroring the api's
//! `upload.rs wikilink_for` NULL rule.
//!
//! This crate must never depend on docli-core (UNLICENSED — it cannot ship in the MIT mirror).

/// The file extension of a node name — everything after the LAST interior `.`, lowercased,
/// WITHOUT the dot (`note.md` → `md`, `pic.PNG` → `png`). Returns `None` when there's no
/// extension: no dot, a leading dot (`.gitignore`, the literal `.md` name), or a trailing dot.
pub fn file_extension(name: &str) -> Option<String> {
    let i = name.rfind('.')?;
    if i == 0 || i + 1 == name.len() {
        return None;
    }
    Some(name[i + 1..].to_ascii_lowercase())
}

/// True when a node NAME is a valid NOTE (`kind='file'`) name: extension exactly `md`,
/// case-insensitive, on [`file_extension`]'s interior-dot rule (so the extension-only name `.md`
/// is NOT a note name).
///
/// **Parity family** (the same question on every end of the sync wire): the server's create
/// funnel (`create_node_core`, via docli-core's re-export), the plugin scanner
/// (`vaultPort.ts classifyFile`), the sync-client apply guard (`quarantine.ts isNoteName`), and
/// the CLI's mirror guard — a split is either the A3 binary truncation or an unconvergeable node.
pub fn is_note_name(name: &str) -> bool {
    file_extension(name).as_deref() == Some("md")
}

/// The name-fold rule (the NFC/NFD e2e finding, v0.27.1): two paths whose folded forms are equal
/// name ONE physical file on the local filesystem. NFC everywhere; lowercase only when the
/// filesystem is case-insensitive by default (macOS / Windows — an unconditional lowercase on
/// Linux would park legitimate twins).
///
/// JS twin: the plugin's `foldPath` closure in `main.ts` (`p.normalize("NFC")` +
/// `.toLowerCase()` on mac/win/iOS). `to_lowercase()` and `toLowerCase()` are both the
/// unconditional-Unicode + SpecialCasing mapping, but the agreement is MEASURED by
/// `vectors/fold.json`, not assumed (the 2026-08-27 case-folding research: residues here are
/// small, systematic, and silent).
#[cfg(feature = "fold")]
pub fn fold_path(path: &str, case_insensitive: bool) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfc: String = path.nfc().collect();
    if case_insensitive {
        // Lowercase + ς→σ. The sigma post-map is load-bearing (Codex round 1; rationale
        // CORRECTED by measurement, review round 4): the guard's job is to predict what the
        // FILESYSTEM folds together, and Unicode case folding — what case-insensitive
        // filesystems implement — maps BOTH sigmas to σ. Rust's `to_lowercase()` and V8's
        // `toLowerCase()` AGREE with each other (both spec-compliant, Final_Sigma included —
        // measured 2026-08-30 on «ΟΔΟΣ»→«οδος» and «ΟΔΟΣ.md»→«οδοσ.md», where the dot's
        // case-ignorability blocks the rule), but both emit word-final ς where the filesystem
        // folds to σ. The post-map aligns the twins with the FILESYSTEM, so «ΑΣ» and «ασ» — one
        // physical file on APFS/NTFS — fold equal here too.
        nfc.to_lowercase().replace('ς', "σ")
    } else {
        nfc
    }
}

/// True when the platform's default filesystem folds case (the plugin's
/// `Platform.isMacOS || Platform.isWin || Platform.isIosApp` test, minus iOS — the CLI does not
/// run there).
pub fn platform_folds_case() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// The api's `upload.rs wikilink_for` NULL rule, mirrored (v0.28.0 D6): a path containing
/// wikilink syntax (`#` anchor, `|` alias, brackets) cannot be named by any wikilink we could
/// emit — those parse as structure, so any string would point somewhere else. The CLI's marker
/// then says `wikilink not-expressible` instead of inventing a wrong link.
pub fn wikilink_expressible(path: &str) -> bool {
    !path.contains(['#', '|', '[', ']'])
}

/// The connection-label GRAMMAR (v0.25.7 D1, moved here in v0.28.0 D12 so the CLI can validate
/// the labels it writes into agent MCP configs) — **a compatibility surface, not a convenience**:
/// lowercase ASCII `[a-z0-9-]`, 1–64 bytes, matched byte-for-byte against the raw path segment.
/// Widening it is a DESIGN change, not a tweak — every stored `resource` string, every client's
/// pasted URL, and the ledger's rendered tags are written against exactly this alphabet.
/// Off-grammar and over-long labels are REJECTED, never truncated — truncating a label changes
/// which connection is being named. docli-core re-exports both names so the server side keeps
/// ONE definition.
pub const CONNECTION_LABEL_MAX_BYTES: usize = 64;

/// Is `label` a well-formed connection label? Byte-exact over the raw segment: percent-encodings,
/// uppercase, dots, slashes and query characters all simply fail the alphabet, which is what
/// makes «no normalization» a property of the grammar rather than a list of cases.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= CONNECTION_LABEL_MAX_BYTES
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub mod winpath;

/// The shared vector FILES, exposed as crate constants.
///
/// `fold.json` and `winpath.json` are consumed by this crate's own tests, where
/// `include_str!("../vectors/…")` resolves in any layout. `graph.json` is different: its Rust
/// consumers are OUTSIDE this crate (`apps/cli/src/graph.rs` and `packages/core/src/db/link.rs`),
/// and a path relative to THOSE files only resolves in the monorepo — the public CLI mirror lays
/// the three crates out flat, so the same `include_str!` there fails to compile. Reading the file
/// where it lives and handing it out as a constant is what makes the twin's contract portable.
///
/// Caught by the mirror's own `cargo test`, which is the whole point of running it there.
pub mod vectors {
    /// v0.29.1 D7 — the note graph's five read predicates.
    pub const GRAPH: &str = include_str!("../vectors/graph.json");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_note_name_matches_the_scanner_twin_vectors() {
        for ok in ["note.md", "Note.MD", "a.b.md", "Привет.md"] {
            assert!(is_note_name(ok), "{ok} is a note name");
        }
        for bad in [
            "Map.canvas",
            "notes.txt",
            "README",
            ".md",
            "trailing.",
            "archive.tar.gz",
        ] {
            assert!(!is_note_name(bad), "{bad} is NOT a note name");
        }
    }

    /// Both sides of the cross-train fold twin consume THIS file; the plugin-side test lives in
    /// `apps/obsidian-plugin` and reads it by path.
    #[cfg(feature = "fold")]
    #[test]
    fn fold_agrees_with_the_shared_vectors() {
        #[derive(serde::Deserialize)]
        struct V {
            input: String,
            ci: String,
            cs: String,
        }
        let vs: Vec<V> = serde_json::from_str(include_str!("../vectors/fold.json")).unwrap();
        assert!(vs.len() >= 8, "the fold vector file has thinned out");
        for v in vs {
            assert_eq!(fold_path(&v.input, true), v.ci, "ci fold of {:?}", v.input);
            assert_eq!(fold_path(&v.input, false), v.cs, "cs fold of {:?}", v.input);
        }
    }

    #[test]
    fn label_grammar_is_byte_exact_and_never_truncates() {
        for ok in ["blog", "a-1", "x", &"a".repeat(64)] {
            assert!(valid_label(ok), "{ok:?}");
        }
        for bad in ["", "Blog", "a.b", "a b", "%62log", "мой", &"a".repeat(65)] {
            assert!(!valid_label(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn wikilink_expressibility_mirrors_wikilink_for() {
        assert!(wikilink_expressible("docs/photo.png"));
        for bad in ["a#b.png", "a|b.png", "a[.png", "a].png"] {
            assert!(!wikilink_expressible(bad), "{bad}");
        }
    }
}
