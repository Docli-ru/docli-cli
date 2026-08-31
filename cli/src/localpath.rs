// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Server path → LOCAL mirror path projection (v0.28.0 D3).
//!
//! Three layers, in order:
//! 1. scope stripping — a folder-scoped mount holds that subtree at its root;
//! 2. the winPath encoding (Windows targets; `docli_rules::winpath`, the plugin's D8 twin);
//! 3. the projection PARK rules the encoding itself cannot express: parity with `winPath.ts`
//!    alone is not representability — the encoding permits cross-domain projection collisions
//!    (`a<b.md` projects onto the same local name as a literal `a%3Cb.md`) and `%XX` expansion
//!    can push a valid component past filesystem length limits. Both GUARD-PARK after
//!    projection (the same park family as the fold guard), never write.

use crate::platform::FsRules;

/// Why a delivered path cannot be materialized (all STRUCTURAL parks — healable only by a
/// server-side rename; D2a's second park class).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProjectError {
    /// A component exceeds the filesystem's byte cap after `%XX` expansion.
    ComponentTooLong { component: String },
}

/// Strip a mount's folder scope off a server path. `None` = the path is OUT of scope.
/// The scope folder itself maps to the mount root and is not materialized as a subdirectory.
pub fn scope_relative<'a>(server_path: &'a str, scope: Option<&str>) -> Option<&'a str> {
    match scope {
        None => Some(server_path),
        Some(folder) => {
            if server_path == folder {
                Some("")
            } else {
                server_path.strip_prefix(&format!("{folder}/"))
            }
        }
    }
}

/// Project a scope-relative server path into its local spelling under the mount root.
pub fn project(rel_server_path: &str, rules: &FsRules) -> Result<String, ProjectError> {
    let encoded = if rules.win_names {
        docli_rules::winpath::encode_win_path(rel_server_path)
    } else {
        rel_server_path.to_string()
    };
    for component in encoded.split('/') {
        if component_units(component, rules) > rules.max_component_bytes {
            return Err(ProjectError::ComponentTooLong {
                component: component.to_string(),
            });
        }
    }
    Ok(encoded)
}

/// The unit a filename component is measured in on this filesystem (Codex round 23): NTFS caps
/// at 255 UTF-16 code units, so measuring UTF-8 bytes permanently structural-parked legal
/// Cyrillic-heavy names (`Ж` is 2 bytes but 1 unit); unix filesystems cap at 255 BYTES.
pub fn component_units(component: &str, rules: &FsRules) -> usize {
    if rules.win_names {
        component.encode_utf16().count()
    } else {
        component.len()
    }
}

/// The fold key for the twin guard (D3): two local paths with one key name ONE physical file.
pub fn fold_key(local_path: &str, rules: &FsRules) -> String {
    docli_rules::fold_path(local_path, rules.fold_case_insensitive)
}

/// True when ANY segment of a server path ends in `.docli` — the CLI's control-namespace park
/// rule (D2). Not just the leaf: a child of a `foo.docli/` folder has an ordinary name, but
/// materializing it would ancestor-create the parked folder, so the whole subtree stays
/// ledger-only.
pub fn in_docli_namespace(server_path: &str) -> bool {
    // ASCII-case-insensitive, like the server's `guard_segment`: a `MOUNT.DOCLI` folder would
    // otherwise dodge the STRUCTURAL park here and land in a never-healing TRANSIENT park when
    // it collides with the control file on a case-folding filesystem (round-2 finding).
    server_path.split('/').any(|seg| {
        let bytes = seg.as_bytes();
        bytes.len() >= 6 && bytes[bytes.len() - 6..].eq_ignore_ascii_case(b".docli")
    })
}

/// True when ANY segment is one of the reserved names the plugin filters push-side
/// (`.obsidian`/`.trash`/`.git` are creatable today via the unguarded web arm; a faithful
/// applyRemote port would materialize them, and a mirror growing an `.obsidian/` would then trip
/// the CLI's own vault-ancestor geometry rule against itself). ASCII-case-insensitive, like the
/// server's `guard_segment`.
pub fn has_reserved_segment(server_path: &str) -> bool {
    server_path.split('/').any(|seg| {
        [".obsidian", ".trash", ".git"]
            .iter()
            .any(|r| seg.eq_ignore_ascii_case(r))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win_rules() -> FsRules {
        FsRules {
            fold_case_insensitive: true,
            win_names: true,
            max_component_bytes: 255,
        }
    }

    #[test]
    fn scope_stripping_maps_the_subtree_to_the_mount_root() {
        assert_eq!(scope_relative("docs/a.md", Some("docs")), Some("a.md"));
        assert_eq!(scope_relative("docs", Some("docs")), Some(""));
        assert_eq!(scope_relative("elsewhere/a.md", Some("docs")), None);
        assert_eq!(scope_relative("docs2/a.md", Some("docs")), None);
        assert_eq!(scope_relative("a.md", None), Some("a.md"));
    }

    #[test]
    fn windows_measures_utf16_units_not_utf8_bytes() {
        // Codex round 23: NTFS caps components at 255 UTF-16 units. 130 `Ж` = 260 UTF-8 bytes
        // but 130 units — representable on Windows, so it must NOT park there; on unix
        // (255-BYTE caps) it honestly does.
        let name = format!("{}.md", "Ж".repeat(130));
        assert!(project(&name, &win_rules()).is_ok());
        let unix = FsRules {
            win_names: false,
            ..win_rules()
        };
        assert!(matches!(
            project(&name, &unix).unwrap_err(),
            ProjectError::ComponentTooLong { .. }
        ));
    }

    #[test]
    fn projection_parks_a_component_past_the_byte_cap() {
        // A 250-char name of `:` expands 3× under %XX — far past 255 bytes.
        let long = format!("{}.md", ":".repeat(120));
        let err = project(&long, &win_rules()).unwrap_err();
        matches!(err, ProjectError::ComponentTooLong { .. });
        // The same name is fine where no projection applies (byte length 124 < 255).
        assert!(project(
            &long,
            &FsRules {
                win_names: false,
                ..win_rules()
            }
        )
        .is_ok());
    }

    #[test]
    fn docli_namespace_covers_interior_segments_and_attachment_names() {
        assert!(in_docli_namespace(".docli"));
        assert!(in_docli_namespace("foo.docli/child.md"));
        assert!(in_docli_namespace("a/x.png.docli"));
        assert!(
            in_docli_namespace("MOUNT.DOCLI"),
            "case-insensitive like the server guard"
        );
        assert!(!in_docli_namespace("docli/notes.md"));
        assert!(!in_docli_namespace("a/doclix/b.md"));
    }
}
