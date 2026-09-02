// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Per-workspace durable state under `.docli/` (v0.28.0 D2) — cursor + the id-keyed node map
//! (`pathForId` is what turns a remote rename into a real MOVE; losing it re-creates the
//! identity-churn defect), the live-id LEDGER (the D2a count comparand — wire-derived, so scope,
//! the D3 guards, and unknown kinds cannot produce false mismatches), the parked deliveries
//! (durable — the cursor advances past a parked node and the server never redelivers an
//! unchanged rev, so an unrecorded park would read as healthy forever), and the completeness
//! manifest fields (head time, scope key, the from-zero flag).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use docli_sync_wire::WireCursor;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackedKind {
    Folder,
    Note,
    Attachment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeState {
    /// The SERVER path (scope-inclusive) — what the wire speaks.
    pub server_path: String,
    /// The local path relative to the mount root (scope-stripped, win-projected).
    pub local_path: String,
    pub kind: TrackedKind,
    pub rev: i64,
    /// hex sha256 of the bytes the CLI wrote (note body / marker content) — the adoption
    /// comparand and doctor's digest baseline. Empty for folders.
    pub content_sha256: String,
    /// Where the attachment's sidecar actually lives when it RELOCATED (D6) — resolved through
    /// state by search and doctor, never by re-deriving. `None` = the derived `<local>.docli`.
    pub marker_path: Option<String>,
}

/// The two park classes fail differently (D2a): TRANSIENT keeps `sync --check` failing and
/// `CACHE_INCOMPLETE.docli` present, heals via `sync --full` once the occupant is gone;
/// STRUCTURAL is reported by `doctor` and in `sync`'s summary instead — a signal that cannot
/// stop firing stops informing (the D8 rule read in both directions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParkClass {
    Transient,
    Structural,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Park {
    pub class: ParkClass,
    pub reason: String,
    pub server_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsState {
    pub epoch: i64,
    pub cursor: WireCursor,
    /// Unix seconds when the cursor last REACHED HEAD (not merely the last pull) — the manifest's
    /// retention bound (head-age > 30 days hard-forces from-zero).
    pub head_reached_at: Option<i64>,
    /// True while the last pull run has not reached head — with `from_zero` and transient parks,
    /// one of the three `CACHE_INCOMPLETE.docli` predicates.
    pub at_head: bool,
    /// The mount's folder scope this state was built under; a change forces from-zero (the
    /// cursor advanced past out-of-scope nodes, so a widened scope must backfill — the plugin's
    /// own load-bearing rule).
    pub scope_key: Option<String>,
    /// The durable from-zero-in-progress flag (D3): while set, the next sync replays from (0,0)
    /// again (an interrupted from-zero RESTARTS, never resumes), `--check` FAILS and
    /// `CACHE_INCOMPLETE.docli` is PRESENT.
    pub from_zero: bool,
    /// id → tracked materialization.
    pub nodes: BTreeMap<Uuid, NodeState>,
    /// The live-id LEDGER: every delivered non-trashed id, whether or not it materialized
    /// (out-of-scope, guard-parked, and unknown-kind nodes included; a tombstone removes its
    /// id). Exists solely to make the D2a count comparison well-defined.
    pub ledger: BTreeSet<Uuid>,
    /// Parked deliveries, by node id.
    pub parks: BTreeMap<Uuid, Park>,
    /// Directories whose removal is OWED but blocked (an untracked occupant kept a tombstoned/
    /// moved folder alive) — a SET of mount-relative paths. DURABLE (Codex round 2: an
    /// in-memory list dies with a crash between pages, and `--full`'s prune walks `nodes`, so
    /// a lost entry meant a stray directory no CLI verb could ever remove) and deliberately
    /// DECOUPLED from `parks` (Codex round 3: keying a debt by node id clobbered a structural
    /// park on the same id, and one node CAN owe several dirs across pages). Retried on every
    /// sync; `--check` and `CACHE_INCOMPLETE.docli` consult this set DIRECTLY.
    #[serde(default)]
    pub pending_removals: BTreeSet<String>,
}

/// How long a cursor may go without reaching head before the mirror stops being a projection of
/// anything (the manifest's retention bound). Lives here rather than in `sync_cmd` because
/// v0.29.0 D4 made it one of the shared readiness terms — `search` asks the same question
/// `sync`'s invalidator does, and two thresholds would be two answers.
pub const MAX_HEAD_AGE_SECS: i64 = 30 * 24 * 60 * 60;

impl WsState {
    pub fn fresh(scope_key: Option<String>) -> Self {
        WsState {
            epoch: 0,
            cursor: WireCursor {
                rev: 0,
                id: Uuid::nil(),
            },
            head_reached_at: None,
            at_head: false,
            scope_key,
            from_zero: true,
            nodes: BTreeMap::new(),
            ledger: BTreeSet::new(),
            parks: BTreeMap::new(),
            pending_removals: BTreeSet::new(),
        }
    }

    pub fn has_transient_parks(&self) -> bool {
        self.parks.values().any(|p| p.class == ParkClass::Transient)
    }

    /// The four terms `CACHE_INCOMPLETE.docli` is written for — the codebase's own definition of
    /// an incomplete mirror, in ONE place (v0.29.0 D4). `sync` writes the marker from it and
    /// `status` renders its row from it; a screen disagreeing with the marker in the mirror
    /// would be worse than no screen.
    ///
    /// Deliberately NOT a term here: the mount's configured folder scope, which is a property of
    /// `docli.toml` rather than of this state — callers holding a `Mount` add it explicitly.
    pub fn incomplete(&self) -> bool {
        self.from_zero
            || !self.at_head
            || self.has_transient_parks()
            || !self.pending_removals.is_empty()
    }

    /// Why this state forces a REBUILD FROM ZERO — the cheap half of `sync`'s invalidator, which
    /// adds the expensive mirror-vs-manifest walk on top (v0.29.0 D4).
    ///
    /// One home for the three terms, because they are genuinely the same question asked by two
    /// commands. Left as two hand-copied lists they would drift the silent way: `sync --check`
    /// would start calling a mount stale for a term `search` never learned, and `search` would go
    /// on asking and reporting `none` over it — the exact defect D4 exists to close, one layer up.
    ///
    /// `scope` is the mount's configured folder (state cannot see `docli.toml`); `now` is unix
    /// seconds, injected so the age term is testable.
    pub fn rebuild_reason(&self, scope: Option<&str>, now: i64) -> Option<&'static str> {
        if self.from_zero {
            return Some("a full rebuild is pending");
        }
        if self.scope_key.as_deref() != scope {
            // The cursor advanced past out-of-scope nodes, so a widened scope must backfill.
            return Some("the mount's folder scope changed");
        }
        match self.head_reached_at {
            None => Some("it has never reached the server's head"),
            Some(t) if now - t > MAX_HEAD_AGE_SECS => {
                Some("its cursor last reached head more than 30 days ago")
            }
            _ => None,
        }
    }

    /// Why this state is not a usable read-only projection right now, or `None` when it is
    /// (v0.29.0 D4 — the shared readiness predicate `search` asks).
    ///
    /// [`Self::rebuild_reason`]'s three terms, plus the three that make a mirror incomplete
    /// without forcing a rebuild. The set is deliberately BROADER than the render filter: a
    /// parked or pending-removal mount that asked would receive `none` and print nothing, over a
    /// mirror the `CACHE_INCOMPLETE.docli` contract already calls incomplete.
    ///
    /// **Cheap terms only**: no disk walk, no network. `search` must stay lock-less and must not
    /// pay a manifest walk, so content removed under an intact `MOUNT.docli` — a shell `rm -rf`,
    /// a `git clean -fd`, an agent's own `rm` — is invisible here and stays `docli doctor`'s
    /// question. So is a STRUCTURAL park: it leaves a node unmaterialized while its id stays in
    /// the ledger, and it is durable by nature, so treating it as unusable would fire forever
    /// over one unmaterializable path — a signal that cannot stop firing stops informing.
    pub fn unusable_reason(&self, scope: Option<&str>, now: i64) -> Option<&'static str> {
        if let Some(r) = self.rebuild_reason(scope, now) {
            return Some(r);
        }
        if !self.at_head {
            return Some("the last sync did not finish");
        }
        if self.has_transient_parks() {
            return Some("deliveries are parked behind untracked files");
        }
        if !self.pending_removals.is_empty() {
            return Some("directory removals are blocked by untracked occupants");
        }
        None
    }

    /// Node id currently tracked at a LOCAL path (linear — the maps are small enough, and an
    /// index would be one more thing to keep consistent).
    pub fn id_at_local(&self, local: &str) -> Option<Uuid> {
        self.nodes
            .iter()
            .find(|(_, n)| n.local_path == local)
            .map(|(id, _)| *id)
    }
}

/// The control root: `.docli/` next to `docli.toml`. Holds `state/<ws>.json`, `markers/`, and
/// gets the same containment discipline as the mount (D2 — two validated roots, not one).
pub struct ControlRoot {
    pub dir: PathBuf,
}

impl ControlRoot {
    pub fn new(project_root: &Path) -> Self {
        ControlRoot {
            dir: project_root.join(".docli"),
        }
    }

    pub fn state_path(&self, ws: Uuid) -> PathBuf {
        self.dir.join("state").join(format!("{ws}.json"))
    }

    pub fn markers_dir(&self) -> PathBuf {
        self.dir.join("markers")
    }

    pub fn load_state(&self, ws: Uuid) -> Result<Option<WsState>> {
        let p = self.state_path(ws);
        if !p.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let st = serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
        Ok(Some(st))
    }

    /// Atomic persist (tmp + rename): a crash between an FS mutation and this commit leaves the
    /// PREVIOUS state, which the byte-equal-adoption rule makes convergent on the next cycle.
    pub fn save_state(&self, ws: Uuid, state: &WsState) -> Result<()> {
        let p = self.state_path(ws);
        fs::create_dir_all(p.parent().expect("state dir")).context("creating .docli/state")?;
        let tmp = p.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &p).with_context(|| format!("committing {}", p.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ControlRoot::new(tmp.path());
        let ws = Uuid::from_u128(1);
        assert!(root.load_state(ws).unwrap().is_none());
        let mut st = WsState::fresh(Some("docs".into()));
        st.ledger.insert(Uuid::from_u128(2));
        st.parks.insert(
            Uuid::from_u128(3),
            Park {
                class: ParkClass::Structural,
                reason: "docli-namespace".into(),
                server_path: "x.docli".into(),
            },
        );
        root.save_state(ws, &st).unwrap();
        let back = root.load_state(ws).unwrap().unwrap();
        assert!(back.from_zero);
        assert_eq!(back.ledger.len(), 1);
        assert!(!back.has_transient_parks());
        assert_eq!(back.scope_key.as_deref(), Some("docs"));
    }
}
