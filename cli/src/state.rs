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
    /// v0.29.7 D2 — the server's `content_changed_at` for this node, as the server last stated it.
    /// The comparand the mid-session gate uses. `None` = the server had none when we applied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_changed_at: Option<String>,
}

impl NodeState {
    /// Has this node's CONTENT moved since the mirror applied it?
    ///
    /// `served` is what the server says now. Both sides of the comparison are values THE SAME
    /// SERVER produced about the SAME node, which is why no clock is compared across machines and
    /// no two nodes' stamps are ever ordered.
    ///
    /// **A served `None` is proof of «unchanged», not a gap in what we know** — and that is what
    /// keeps this a two-line function instead of a tri-state. `content_changed_at` is written only
    /// by `set_body_author`, is never cleared, and every path that changes a note's content routes
    /// through it (the web editor, MCP, and the plugin's push via `merge_note_save`). So a node the
    /// server has no stamp for has not had its content change since `0052` — whatever this mirror
    /// did or did not record about it.
    ///
    /// That also means an older CLI's state (no stamp recorded) and an older api (no stamp served)
    /// both reach the right answer instead of needing to be told apart.
    pub fn content_moved(&self, served: Option<&str>) -> bool {
        served.is_some_and(|v| self.content_changed_at.as_deref() != Some(v))
    }
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
    /// Did the last completed sync ASK for the note graph (v0.29.1)? Paired with the presence of
    /// the graph file, this is what separates the three answers `docli read` has to give:
    ///
    /// - file present and stamped for this `(epoch, cursor)` → the graph is HELD.
    /// - no file, `graph_asked` → we asked and the server served none: an api too old to have one.
    /// - no file, `!graph_asked` → we NEVER asked. `#[serde(default)]` makes this what a
    ///   0.1.4-era state file says, which is the point: that mirror must get «run docli sync»,
    ///   not «your server cannot serve one».
    #[serde(default)]
    pub graph_asked: bool,
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
            graph_asked: false,
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
            // SATURATING, because this file is untrusted input: a hand-edited `i64::MIN` would
            // otherwise wrap in release and read as brand new. A stamp in the FUTURE is not
            // handled here — see `unusable_reason`; it makes the age unknown, which is a reason
            // to distrust the mirror but not to rebuild it from zero.
            Some(t) if now.saturating_sub(t) > MAX_HEAD_AGE_SECS => {
                Some("its cursor last reached head more than 30 days ago")
            }
            _ => None,
        }
    }

    /// How far ahead of `now` a head stamp may sit before its age stops meaning anything.
    ///
    /// Not zero, deliberately: a few seconds of skew between a sync and the next command is
    /// ordinary, and firing on it would make the notice fire constantly. This is the width of a
    /// clock CORRECTION, not of jitter.
    const MAX_CLOCK_SKEW_SECS: i64 = 5 * 60;

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
        // A head stamp in the FUTURE makes the age term above vacuous: `now - t` is negative, so
        // the mirror never ages out and an arbitrarily stale one reads as current. The realistic
        // cause is a clock CORRECTION — the machine synced while its clock ran ahead, then NTP
        // pulled it back — which `selfupdate`'s own «due» check already has an arm for.
        //
        // It belongs HERE and not in `rebuild_reason` on purpose: the age is unknown, which is a
        // reason to stop vouching for the mirror, not to re-download it. Forcing a from-zero
        // rebuild of a large mirror over two minutes of skew would be a worse bug than the one
        // it fixes, and the next ordinary `docli sync` re-stamps the field either way.
        if self
            .head_reached_at
            .is_some_and(|t| t.saturating_sub(now) > Self::MAX_CLOCK_SKEW_SECS)
        {
            return Some("its last head time is in the future, so its age cannot be read");
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

/// The note graph as CACHED (v0.29.1 D4), stamped with the position of the head-reaching page it
/// rode in on.
///
/// The stamp is the whole safety story. The graph has no freshness lifecycle of its own: it is
/// served only when `(epoch, cursor)` byte-equals the state's, so a state that moved on — one more
/// page, a new epoch, a from-zero — silently retires the cache instead of pairing a current mirror
/// with a stale graph. That is why the file is separate from the state rather than inside it: the
/// state is read by `search` and `status` on every invocation, and a multi-megabyte graph has no
/// business in that path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphCache {
    pub epoch: i64,
    pub cursor: WireCursor,
    pub graph: docli_sync_wire::WireGraph,
}

/// What the server has TOLD us about a node whose copy this mirror may be behind on (v0.29.7 D3).
///
/// **A claim is `(node id, node_rev)`** — the server's own monotonic position for the node at the
/// moment it named it. It is OBSOLETE once the mirror has applied that rev or a later one, so
/// nothing has to remove claims for them to stop firing: catching up retires them, and a claim for
/// a rev the mirror has already passed never fires at all.
///
/// # Why the rev, and not the content stamp or the file's order
///
/// `content_changed_at` decides WHETHER a node is claimed — that is what keeps a rename or a
/// reorder from firing the gate, which is the whole point of D2. It cannot ORDER claims: two
/// searches can append in the opposite order to the server snapshots they read, so «the last line
/// wins» lets an older observation suppress a newer one and the mirror then serves content the
/// server has already named. Ordering has to be a property of the SERVER, and `node_rev` is the one
/// the sync plane already keys on.
///
/// It also makes claims self-retiring, which removed the last piece of lifecycle machinery here.
/// Earlier drafts kept every claim and refused if any was unsatisfied (a note edited twice then
/// stranded forever), then kept the last line per node (unsound across concurrent appends), then
/// deleted the whole file on a from-zero rebuild to unstick them (which lost true claims, and for
/// a `gone` node followed by a hard purge lost them permanently — a stale serve).
///
/// # The file is APPEND-ONLY and nothing ever deletes it
///
/// One `<uuid> <rev>` per line. Adding is an `O_APPEND` write, so a concurrent writer cannot be
/// lost. Folding takes the GREATEST rev per node, which is order-independent — the property the
/// last-line rule lacked. It is a separate file from `WsState` because `save_state` writes the
/// whole state and nothing locks it: a `search` that did load→mutate→save would clobber a
/// concurrent `sync`'s cursor, ledger and parks wholesale.
#[derive(Debug, Clone, Default)]
pub struct StaleMarks {
    /// node id → the GREATEST rev claimed for it. Order-independent by construction.
    pub latest: BTreeMap<Uuid, i64>,
}

impl StaleMarks {
    /// Is the mirror behind on this node, by the server's own reckoning?
    ///
    /// `stored` is the node's `rev` as this mirror applied it. A claim at or below it has been
    /// caught up with and is permanently obsolete — including a claim made for a trashed node,
    /// which retires the moment the tombstone (a later rev) is applied.
    pub fn contradict(&self, id: Uuid, stored: i64) -> bool {
        self.latest
            .get(&id)
            .is_some_and(|claimed| *claimed > stored)
    }
}

impl ControlRoot {
    pub fn marks_path(&self, ws: Uuid) -> PathBuf {
        self.dir.join("state").join(format!("{ws}.stale"))
    }

    /// The claims for `ws`, or an empty set when there are none or the file will not read.
    ///
    /// An unreadable or partly-garbled file yields whatever lines DO parse, never an error: this
    /// is a derived projection whose only remedy is the next `search`, and failing a `docli read`
    /// over it would turn a cache miss into an outage (the `load_graph` precedent).
    pub fn load_marks(&self, ws: Uuid) -> StaleMarks {
        let Ok(raw) = fs::read_to_string(self.marks_path(ws)) else {
            return StaleMarks::default();
        };
        let mut latest: BTreeMap<Uuid, i64> = BTreeMap::new();
        for line in raw.lines() {
            let Some((id, rev)) = line.trim().split_once(' ') else {
                continue;
            };
            let (Ok(id), Ok(rev)) = (Uuid::parse_str(id), rev.parse::<i64>()) else {
                continue;
            };
            // GREATEST, not last: file order is append order, which is not server order.
            let e = latest.entry(id).or_insert(rev);
            *e = (*e).max(rev);
        }
        StaleMarks { latest }
    }

    /// APPEND `claims`, skipping any the file already covers with an equal or greater rev.
    ///
    /// The skip is a read-then-APPEND, never a read-modify-replace, so losing that race costs a
    /// duplicate line and never a claim. It exists because without it every search re-appends the
    /// same claim for as long as the mirror is behind.
    ///
    /// **Best-effort by contract**: where `$HOME` is read-only — an agent sandbox, the environment
    /// that broke v0.29.1's live gate — this cannot persist and MUST NOT fail the command (the
    /// `0.1.11` rule). The gate then learns nothing new there, though `read` still honours claims
    /// an earlier writable-home session left.
    pub fn merge_marks(&self, ws: Uuid, claims: &BTreeMap<Uuid, i64>) {
        if claims.is_empty() {
            return;
        }
        let held = self.load_marks(ws);
        let fresh: Vec<(Uuid, i64)> = claims
            .iter()
            .filter(|(id, rev)| held.latest.get(id).is_none_or(|h| *rev > h))
            .map(|(id, rev)| (*id, *rev))
            .collect();
        if fresh.is_empty() {
            return;
        }
        let _ = self.append_marks(ws, &fresh);
    }

    fn append_marks(&self, ws: Uuid, claims: &[(Uuid, i64)]) -> Result<()> {
        use std::io::Write;
        let p = self.marks_path(ws);
        fs::create_dir_all(p.parent().expect("state dir")).context("creating .docli/state")?;
        let mut buf = String::with_capacity(claims.len() * 48);
        for (id, rev) in claims {
            buf.push_str(&format!("{id} {rev}\n"));
        }
        // `O_APPEND` positions each underlying write atomically. A torn record — reachable only if
        // the kernel short-writes, which regular files do not do in practice — costs at most that
        // line: the parser skips anything that is not `<uuid> <i64>`, and the next `search`
        // re-derives the claim, because the node is still above the mirror's cursor.
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .with_context(|| format!("opening {}", p.display()))?
            .write_all(buf.as_bytes())
            .with_context(|| format!("appending to {}", p.display()))?;
        Ok(())
    }
}

/// The control root — `~/.docli` in production, since the mirror became per-MACHINE in v0.29.2
/// (see [`ControlRoot::at`]; the `<project>/.docli` shape survives only for tests and the legacy
/// layout). Holds `state/<ws>.json`, `state/<ws>.stale`, `markers/`, and gets the same containment
/// discipline as the mount (D2 — two validated roots, not one).
pub struct ControlRoot {
    pub dir: PathBuf,
}

impl ControlRoot {
    /// The control plane for a project. Since the mirror became per-MACHINE, this is
    /// `~/.docli` — the same home the credentials use — not `<project>/.docli`.
    ///
    /// State has to live wherever the mirror lives. Two projects linking one workspace share one
    /// cache; if each kept its own state for it, both would write the same directory believing
    /// different things about what is in it.
    pub fn at(dir: &Path) -> Self {
        ControlRoot {
            dir: dir.to_path_buf(),
        }
    }

    /// TESTS and the legacy project-local shape: `<root>/.docli`.
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

    pub fn graph_path(&self, ws: Uuid) -> PathBuf {
        self.dir.join("state").join(format!("{ws}.graph.json"))
    }

    /// The cached graph for `ws`, or `None` when there is none, it cannot be read, or its stamp
    /// does not match `(epoch, cursor)`.
    ///
    /// An unreadable or unparseable file is `None`, NOT an error: this is a derived cache whose
    /// only remedy is the next sync, and failing a `docli read` over it would turn a cache miss
    /// into an outage. `read` reports «not held» either way, which is true either way.
    pub fn load_graph(&self, ws: Uuid, epoch: i64, cursor: WireCursor) -> Option<GraphCache> {
        let raw = fs::read_to_string(self.graph_path(ws)).ok()?;
        let c: GraphCache = serde_json::from_str(&raw).ok()?;
        (c.epoch == epoch && c.cursor == cursor).then_some(c)
    }

    /// Persist the graph against the position of the page that carried it (tmp + rename, like
    /// the state itself).
    pub fn save_graph(
        &self,
        ws: Uuid,
        epoch: i64,
        cursor: WireCursor,
        graph: &docli_sync_wire::WireGraph,
    ) -> Result<()> {
        let p = self.graph_path(ws);
        fs::create_dir_all(p.parent().expect("state dir")).context("creating .docli/state")?;
        let tmp = p.with_extension("json.tmp");
        let cache = GraphCache {
            epoch,
            cursor,
            graph: graph.clone(),
        };
        // Compact, not pretty: nothing reads this by eye, and pretty-printing a 10 000-node
        // graph roughly doubles it on disk.
        fs::write(&tmp, serde_json::to_vec(&cache)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &p).with_context(|| format!("committing {}", p.display()))?;
        Ok(())
    }

    /// Drop the cache — the server served no graph, so keeping the previous one would pair a
    /// current mirror with whatever the last graph-serving api happened to say.
    pub fn clear_graph(&self, ws: Uuid) -> Result<()> {
        match fs::remove_file(self.graph_path(ws)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context("removing the cached note graph")),
        }
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

    fn claim(n: u128, rev: i64) -> BTreeMap<Uuid, i64> {
        BTreeMap::from([(Uuid::from_u128(n), rev)])
    }

    /// The whole model: a claim is OBSOLETE once the mirror has applied that rev or later.
    ///
    /// This is what retired the prune, the head-clear and the from-zero delete in turn — catching
    /// up retires a claim, so nothing has to remove one for it to stop firing.
    #[test]
    fn a_claim_retires_once_the_mirror_reaches_its_rev() {
        let marks = StaleMarks {
            latest: BTreeMap::from([(Uuid::from_u128(1), 10)]),
        };
        let id = Uuid::from_u128(1);
        assert!(marks.contradict(id, 9), "behind the claimed rev is stale");
        assert!(!marks.contradict(id, 10), "reaching it retires the claim");
        assert!(
            !marks.contradict(id, 11),
            "and passing it must not resurrect the claim - a trashed node's tombstone is a LATER \
             rev, so this is what retires a `gone` claim too"
        );
        assert!(
            !marks.contradict(Uuid::from_u128(2), 0),
            "claims do not bleed"
        );
    }

    /// Folding takes the GREATEST rev, not the last line — file order is APPEND order, which is not
    /// server order.
    ///
    /// Two searches can append in the opposite order to the snapshots they read. Under a
    /// last-line-wins rule an older observation then suppresses a newer one, and the mirror serves
    /// content the server has already named — the one unacceptable outcome.
    #[test]
    fn folding_takes_the_greatest_rev_whatever_order_it_was_written() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ControlRoot::new(tmp.path());
        let ws = Uuid::from_u128(1);
        // The NEWER observation lands first, the older one after it.
        root.merge_marks(ws, &claim(7, 20));
        root.append_marks(ws, &[(Uuid::from_u128(7), 12)]).unwrap();
        let loaded = root.load_marks(ws);
        assert!(
            loaded.contradict(Uuid::from_u128(7), 15),
            "an out-of-order older claim must not suppress the newer one"
        );
        assert!(!loaded.contradict(Uuid::from_u128(7), 20));
    }

    /// A claim the file already covers appends nothing — otherwise every search re-appends the same
    /// claim for as long as the mirror is behind.
    #[test]
    fn a_covered_claim_is_not_appended_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ControlRoot::new(tmp.path());
        let ws = Uuid::from_u128(1);
        root.merge_marks(ws, &claim(7, 20));
        let after_first = fs::read_to_string(root.marks_path(ws)).unwrap();
        for _ in 0..5 {
            root.merge_marks(ws, &claim(7, 20));
        }
        // …and an OLDER rev is covered too.
        root.merge_marks(ws, &claim(7, 12));
        assert_eq!(
            fs::read_to_string(root.marks_path(ws)).unwrap(),
            after_first
        );
        // A NEWER one is recorded.
        root.merge_marks(ws, &claim(7, 21));
        assert!(root.load_marks(ws).contradict(Uuid::from_u128(7), 20));
    }

    /// A torn tail costs that record alone, never the file.
    #[test]
    fn a_garbled_line_does_not_poison_the_other_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ControlRoot::new(tmp.path());
        let ws = Uuid::from_u128(1);
        root.merge_marks(ws, &claim(7, 20));
        let p = root.marks_path(ws);
        let mut raw = fs::read_to_string(&p).unwrap();
        raw.push_str("00000000-0000-0000-0000-0000000000 2"); // a half-written record
        fs::write(&p, raw).unwrap();
        let loaded = root.load_marks(ws);
        assert_eq!(loaded.latest.len(), 1);
        assert!(loaded.contradict(Uuid::from_u128(7), 19));
    }

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
