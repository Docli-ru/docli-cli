// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Attachment MARKERS (v0.28.0 D6) — inert sidecars with provenance, never a stub at the
//! binary's own path (the A3 truncation class) and never a byte download (marker-only falls out
//! of not porting `attachmentSync`).
//!
//! Format: git-LFS-style sorted `key value` lines — deliberately NOT YAML frontmatter a wiki
//! parser would render. Everything a marker needs is already on the wire (`WireNode`: mime,
//! contentBytes, sha256). Extracted-text fields are RESERVED with DICOM-style provenance
//! semantics — `extracted_at` absent = never attempted / `result: empty` = attempted, empty /
//! `result: unsupported` = cannot — so the document-files slice lands additively and an agent
//! can never mistake «not yet processed» for «empty». Markers are inert: nothing materializes
//! on read (the hydration-storm lesson).

use docli_sync_wire::WireNode;

/// The mirror-root control files that always WIN their names (D6 relocation cause 1).
pub const CONTROL_FILES: [&str; 2] = ["MOUNT.docli", "CACHE_INCOMPLETE.docli"];

/// Render a marker's content — deterministic (sorted keys), so redelivery is byte-equal
/// adoptable like every other write (D2's crash-consistency rule).
pub fn render_marker(node: &WireNode) -> String {
    let mut out = String::new();
    out.push_str(&format!("id {}\n", node.id));
    out.push_str(&format!(
        "mime {}\n",
        node.mime.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!("path {}\n", node.path));
    // An absent wire digest is an HONEST unknown (a NULL blob digest arrives as an absent field —
    // `Option` + `skip_serializing_if`); the marker says so rather than inventing, and `doctor`
    // treats unknown as not-a-mismatch.
    match &node.sha256 {
        Some(d) => out.push_str(&format!("sha256 {d}\n")),
        None => out.push_str("sha256 unknown\n"),
    }
    out.push_str(&format!("size {}\n", node.content_bytes));
    // The api's `wikilink_for` NULL rule, mirrored (the four-character doc-twin): a `#|[]` path
    // cannot be named by any wikilink we could emit.
    if docli_rules::wikilink_expressible(&node.path) {
        out.push_str(&format!("wikilink ![[{}]]\n", node.path));
    } else {
        out.push_str("wikilink not-expressible\n");
    }
    out
}

/// The DERIVED marker path for an attachment's local path — or `None` when the sidecar must
/// RELOCATE to `.docli/markers/<ws>/<node-id>.docli` (D6, per-workspace subdir since Codex round 12). The collision decision is taken against
/// PRE-PAGE state (conservative): a node vacating the colliding path in the same page still
/// counts as a collision, so a marker can relocate when staying derived would have been legal —
/// correct (state resolves it everywhere) and self-healing on the node's next delivery, just
/// conservatively homed until then.
///
/// Relocation causes (D6): the derived name would collide with a
/// mirror-root control file (control files always win), with another node's local path (an
/// attachment literally named `x.png.docli` is legal server-side), or cannot take the suffix at
/// all (component length). The caller resolves relocated markers through STATE (`marker_path`),
/// never by re-deriving.
pub fn derived_marker_path(
    local_path: &str,
    rules: &crate::platform::FsRules,
    collides_with_node_or_control: impl Fn(&str) -> bool,
) -> Option<String> {
    let candidate = format!("{local_path}.docli");
    let leaf = candidate.rsplit('/').next().unwrap_or(&candidate);
    // The same platform-aware measure as `project` (Codex round 23).
    if crate::localpath::component_units(leaf, rules) > rules.max_component_bytes {
        return None;
    }
    // ASCII-case-insensitive (Codex round 4): on a case-folding filesystem a root attachment
    // named `mount` derives `mount.docli`, which ALIASES `MOUNT.docli` — an exact comparison
    // let it collide with the ownership marker and park forever instead of relocating.
    if CONTROL_FILES
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&candidate))
    {
        return None;
    }
    if collides_with_node_or_control(&candidate) {
        return None;
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rules() -> crate::platform::FsRules {
        crate::platform::FsRules {
            fold_case_insensitive: true,
            win_names: false,
            max_component_bytes: 255,
        }
    }
    use uuid::Uuid;

    fn att(path: &str, sha: Option<&str>) -> WireNode {
        WireNode {
            id: Uuid::from_u128(7),
            parent_id: None,
            kind: "attachment".into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            rev: 1,
            trashed: false,
            mime: Some("image/png".into()),
            content_bytes: 9,
            body: None,
            blob_url: Some("/api/attachments/x".into()),
            position: None,
            sha256: sha.map(|s| s.into()),
            blob_generation: Some(0),
        }
    }

    #[test]
    fn marker_says_sha256_unknown_on_an_absent_wire_digest() {
        let m = render_marker(&att("a/pic.png", None));
        assert!(m.contains("sha256 unknown\n"), "{m}");
        let m = render_marker(&att("a/pic.png", Some("abc")));
        assert!(m.contains("sha256 abc\n"), "{m}");
    }

    #[test]
    fn marker_wikilink_is_not_expressible_on_wikilink_syntax_paths() {
        let m = render_marker(&att("a/p#1.png", None));
        assert!(m.contains("wikilink not-expressible\n"), "{m}");
        let m = render_marker(&att("a/pic.png", None));
        assert!(m.contains("wikilink ![[a/pic.png]]\n"), "{m}");
    }

    #[test]
    fn marker_relocates_for_all_three_causes() {
        // Control-file collision: a root attachment named bare `MOUNT` — and the case alias
        // `mount`, which names the SAME file on a folding filesystem.
        assert_eq!(derived_marker_path("MOUNT", &test_rules(), |_| false), None);
        assert_eq!(derived_marker_path("mount", &test_rules(), |_| false), None);
        // Node collision: a sibling literally named `x.png.docli`.
        assert_eq!(
            derived_marker_path("x.png", &test_rules(), |c| c == "x.png.docli"),
            None
        );
        // Length overflow: the suffix pushes the leaf past the cap.
        let long = "a".repeat(250);
        assert_eq!(derived_marker_path(&long, &test_rules(), |_| false), None);
        // The ordinary case derives.
        assert_eq!(
            derived_marker_path("a/pic.png", &test_rules(), |_| false).as_deref(),
            Some("a/pic.png.docli")
        );
    }
}
