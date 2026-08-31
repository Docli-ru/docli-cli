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
