// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli read` (v0.29.1 D2) — the CLI's own read verb.
//!
//! Until this slice `docli search` returned a per-note LOCAL path and the agent opened the file
//! itself. That split — the CLI finds, something else reads — is where grep reaching the mirror,
//! false absence, and every hide-it/move-it question came from. D1 closes it by handing out no
//! per-note address at all, which only works if there is a verb that reads. This is that verb.
//!
//! Addressing is the ONE address space everything else already speaks: the **server path**
//! (`search` prints it, wikilinks resolve to it, the MCP tools take it), or `--id`.
//!
//! # The trust property is the INVERSE of `search`'s (D8)
//!
//! `search` is server-authoritative: its results are unaffected by the state of the mirror, and
//! its notice says exactly that. **`read`'s answer IS the mirror**, so that guardrail is false
//! here. Left unstated, `read` would launder a stale or hand-edited file into an envelope shaped
//! like server truth. So the contract is: serve what is held, and **disclose** what cannot be
//! vouched for — a mirror it cannot vouch for, a file whose digest no longer matches
//! what the server sent, a decode that was not clean. Disclosure goes to stderr and to a `--json`
//! field, **never into `content`**.
//!
//! And a note the mirror does not hold is **refused, naming the case** — never rendered as
//! absence. Only a non-degraded `docli search` establishes that a note does not exist; an agent
//! that reads exit 3 as "no such note" is the false negative this whole train exists to prevent,
//! which is why it has its own exit code rather than folding into the generic failure.
//!
//! # Graph absence is not graph emptiness
//!
//! The `read_note` envelope carries `links`/`backlinks`/`tags`/`title`/`aliases`. This build
//! holds none of them: the graph rides the pull payload in the slice after this one. They render
//! **absent** — `null` plus a named reason — never `[]`, because an empty `backlinks` over a note
//! that has plenty is indistinguishable from truth, which is D5's false negative inside the verb
//! built to replace grep. Half 1 names no remedy for it: `docli sync` cannot fetch a graph no api
//! serves yet, and an instruction that cannot succeed is worse than silence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use uuid::Uuid;

use crate::config::{mount_abs, validate_config, Mount, Project};
use crate::state::{ControlRoot, NodeState, Park, ParkClass, TrackedKind, WsState};

/// **This mirror does not hold it.** Deliberately its own code (open item 4): collapsed into the
/// generic failure, an agent turns "not here" into "does not exist", which is precisely the
/// conclusion only `docli search` may reach.
pub const EXIT_NOT_IN_MIRROR: i32 = 3;

/// **The server told us this note's content changed since the mirror applied it** (v0.29.7 D4).
///
/// Its OWN code, never shared with exit 3 or with the generic failure. The research this slice
/// rests on named the condition precisely: a hard failure is survivable only if it can never fire
/// on something the agent cannot itself fix, and only if the reader can tell it apart from «not
/// here» — an agent that read a staleness refusal as absence would conclude the note does not
/// exist, which is the one conclusion this verb must never permit.
pub const EXIT_STALE: i32 = 4;

/// Everything else. `main` uses the same code for an `Err` return, and that is the point: these
/// are the outcomes that are not answers ABOUT THE MIRROR'S CONTENTS — every miss that could not
/// look, plus the caller's own mistakes (a bad range, an ambiguous selector, and the one
/// declared oddity, a folder addressed as a note, which we DID look at and which is still not a
/// statement about whether the note exists). [`Miss::code`] is what separates the two.
const EXIT_FAILED: i32 = 2;

/// The three reasons the graph can be missing, each with the remedy that actually applies.
///
/// Half 1 had one string and named no command, because no api served a graph and an instruction
/// that cannot succeed is worse than silence. Half 2 makes the cause knowable, so the sentences
/// split — and the split is the whole point of `WsState::graph_asked` existing. What survives in
/// all three is the distinction itself: **not held is not empty.**
const GRAPH_NOT_SYNCED: &str = "not held - this mirror was last synced before the note graph \
                                existed, so absence here is not emptiness; `docli sync` fetches \
                                it";

const GRAPH_NOT_SERVED: &str = "not held - this workspace's server serves no note graph, so \
                                absence here is not emptiness; `read_note` over the docli MCP \
                                connection answers graph questions directly";

const GRAPH_STALE: &str = "not held - the cached graph belongs to an earlier sync of this mirror, \
                           so absence here is not emptiness; `docli sync` refreshes it";

/// The graph is workspace-wide while a mount can be a folder of it, so a held graph names paths
/// this mirror does not hold. A constant so the whitespace check below can reach it.
const SCOPE_DISCLOSURE: &str = "this mount is scoped to a folder while the note graph covers the \
                                whole workspace, so a linked path may not be mirrored here - \
                                `read` exits 3 on those, which says nothing about whether they \
                                exist";

/// A rebuild is in flight, so the mirror's bytes and its stored graph describe different moments —
/// the one case where a MATCHING stamp still must not be served.
const GRAPH_REBUILDING: &str = "not held - a full rebuild of this mirror is pending, so a stored \
                                graph would not describe the notes now on disk; `docli sync` \
                                completes it";

/// A note that simply declares no `title:`. The graph IS held, so this is knowledge, not a gap —
/// but the field still renders `null`, and the envelope's invariant is that every null says why.
const NO_TITLE: &str = "the note declares no `title:` of its own - `name` is its filename";

const FRONTMATTER_ABSENT: &str =
    "not parsed by the CLI - if the note has a raw YAML block, a whole-note read carries it \
     inside `content`, verbatim unless a `not_utf8` disclosure says otherwise";

const RELATED_ABSENT: &str =
    "server-scored per query, never cached here - call `related_notes` over the docli MCP \
     connection";

/// Exit 4's sentence.
///
/// **It says what the server NAMED, not what the server DID**, and the difference is not pedantry:
/// two cases this design names itself can fire over bytes that are perfectly current — a note
/// edited since `0052` whose stamp this mirror predates, and a claim an unlucky interleaving of
/// `search` and `sync` left behind. «The server has changed this note» would be false in both, and
/// re-collapsing the `node_rev`-churn-vs-content-change distinction is precisely what D2 exists to
/// avoid. `v0.29.5` minted the rule this follows: a claim may only assert what the surface it
/// feeds can support.
///
/// **One command, and it is `docli sync`.** MCP `read_note` would also serve the current text —
/// faster, even — but naming both is exactly the enumeration `0.1.19` MEASURED as harmful: listing
/// failure modes beside a remedy made three of six agents stop rather than act. `docli sync` is
/// also the only one of the two that RESOLVES the refusal: it delivers the current bytes AND
/// re-seeds the stamp. `read_note` would leave this note refusing.
///
/// Note what it does NOT promise: that the next read SUCCEEDS. A `gone` mark is resolved by a sync
/// applying the tombstone, after which the note is untracked and `read` answers exit 3; a node that
/// left the mount's scope resolves the same way. «Brings the mirror up to date» is true in every
/// one of those; «the next read succeeds» would not be.
const STALE_REFUSAL: &str = "the server listed this note as changed relative to this mirror's \
                             position, so what is held here cannot be served as current - \
                             `docli sync` brings the mirror up to date";

const ATTACHMENT_BYTES_ABSENT: &str =
    "an attachment's bytes are not mirrored - `read_attachment` over the docli MCP connection \
     fetches them";

pub struct ReadArgs {
    pub path: Option<String>,
    pub id: Option<Uuid>,
    pub mount: Option<String>,
    pub lines: Option<String>,
    pub json: bool,
}

pub fn run(project: &Project, args: &ReadArgs) -> Result<i32> {
    if args.json {
        // Nothing may prompt under `--json`, and the body is the product either way: `read` is
        // deliberately NOT in `report_mode`. Its stdout is the note, so warnings belong on
        // stderr, where they cannot corrupt a redirect into a file.
        crate::ui::machine_mode();
    }
    validate_config(&project.config)?;
    Ok(render(
        resolve(project, args, crate::sync_cmd::now_unix()),
        args.json,
    ))
}

// ---------------------------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Disclosure {
    pub code: &'static str,
    pub message: String,
}

/// 1-based, inclusive, and `total` is the note's own line count so a caller can page without a
/// second read. An EMPTY note is `0-0` of `0` — `1-0` would read as a malformed range.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LineSpan {
    pub from: usize,
    pub to: usize,
    pub total: usize,
}

/// The `read_note` envelope (D2), with every field this build cannot fill rendered as `null` and
/// **named in `absent`** with the reason. That pairing is the invariant — a null with no entry is
/// a silent absence, which is the shape D8 forbids — and it is pinned by a test.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteEnvelope {
    pub kind: &'static str,
    pub id: Uuid,
    /// The SERVER path. The CLI publishes no per-note local address (D1), and this is the
    /// address every other docli surface already speaks.
    pub path: String,
    pub name: String,
    /// The mount this came from — what `--mount` accepts and what `search` tags its hits with.
    pub mount: String,
    pub workspace: Uuid,
    /// Byte-verbatim for a whole-note read; the selected slice under `--lines`.
    pub content: Option<String>,
    pub lines: LineSpan,
    pub title: Option<String>,
    pub aliases: Option<Vec<String>>,
    pub links: Option<Vec<serde_json::Value>>,
    pub unresolved: Option<Vec<String>>,
    pub embeds: Option<Vec<serde_json::Value>>,
    pub backlinks: Option<Vec<serde_json::Value>>,
    pub tags: Option<Vec<String>>,
    pub frontmatter: Option<serde_json::Value>,
    pub related_hint: Option<serde_json::Value>,
    pub absent: BTreeMap<String, String>,
    pub disclosures: Vec<Disclosure>,
}

/// The attachment arm, which is not optional (D2): the marker sidecar was the CLI's only surface
/// for a file's id/mime/size/sha256/wikilink, and D1 removes the path that reached it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEnvelope {
    pub kind: &'static str,
    pub id: Uuid,
    pub path: String,
    pub name: String,
    pub mount: String,
    pub workspace: Uuid,
    pub mime: Option<String>,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    pub wikilink: Option<String>,
    /// The live notes that embed this file (`db::link::attachment_embedders`) — the fifth
    /// predicate, and the only one whose subject is a file.
    pub embedded_in: Option<Vec<serde_json::Value>>,
    /// Always `null`: the bytes are on the server.
    pub content: Option<String>,
    /// The marker file itself, verbatim — nothing the sidecar carries is lost to our parse.
    pub marker: String,
    pub absent: BTreeMap<String, String>,
    pub disclosures: Vec<Disclosure>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Envelope {
    Note(NoteEnvelope),
    File(FileEnvelope),
}

impl Envelope {
    fn disclosures(&self) -> &[Disclosure] {
        match self {
            Envelope::Note(n) => &n.disclosures,
            Envelope::File(f) => &f.disclosures,
        }
    }

    /// Add a disclosure the ENVELOPE could not know about — one about the other mounts, decided
    /// above `serve`, which only ever sees the mount it is serving from.
    fn disclose(&mut self, d: Disclosure) {
        match self {
            Envelope::Note(n) => n.disclosures.push(d),
            Envelope::File(f) => f.disclosures.push(d),
        }
    }
}

pub struct Served {
    pub envelope: Envelope,
    /// What goes to stdout in human mode, exactly.
    pub body: String,
    /// One optional STDERR line beside the disclosures — the graph, summarized. Never stdout:
    /// stdout is the note, byte for byte.
    pub note: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
    #[serde(skip)]
    pub exit: i32,
}

pub enum Outcome {
    Served(Box<Served>),
    Refused(Refusal),
}

fn refuse(code: &'static str, message: impl Into<String>, exit: i32) -> Outcome {
    Outcome::Refused(Refusal {
        code,
        message: message.into(),
        exit,
    })
}

// ---------------------------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------------------------

enum Target {
    Path(String),
    Id(Uuid),
}

/// Why one mount could not answer. Every variant is a SENTENCE the reader can act on; none of
/// them is ever rendered as "the note does not exist".
enum Miss {
    NeverSynced,
    /// The directory carries our `MOUNT.docli` — so this mirror WAS synced — but its state file
    /// is gone. `doctor` already has a name for it: `state-missing`.
    ///
    /// Not `NeverSynced`, which is an ANSWER («this mount holds nothing»). Here the mirror may
    /// hold the note perfectly well and the record of what it holds is what we lost, so the honest
    /// report is that we could not look.
    StateLost,
    StateUnreadable(String),
    NotThisMirror,
    Parked(ParkClass, String),
    /// The workspace delivered this id, but this mount does not materialize it. Reachable only
    /// through `--id`: the ledger is ids-only, so a PATH cannot be told apart from `NotHeld`
    /// (v0.29.1 open item 5).
    LedgerOnly,
    NotHeld,
    Folder,
    /// State tracks it; nothing is at that path any more. A fact ABOUT the mirror.
    Gone(String),
    /// Something IS at that path, but it resolves outside the mirror — a link leading out.
    ///
    /// Its own variant rather than a `Gone` carrying a different string, because `Gone`'s
    /// sentence asserts that the file vanished and this one has not: composing them produced
    /// «nothing is there any more (what is at that path no longer resolves inside the mirror)»,
    /// which contradicts itself in one breath.
    Escaped,
    /// State tracks it and something may well be there, but it could not be read — a permission
    /// error, an I/O fault, a directory where a file belongs, or a marker path the state records
    /// unusably.
    ///
    /// Deliberately NOT `Gone`. Folding an operational failure into «this mirror does not hold
    /// it» would put exit 3 on a file nobody ever looked at, which is the one conclusion this
    /// verb must never let a caller reach by accident — and it would let the mount count as
    /// having ANSWERED, silently suppressing the `mounts_unresolved` caveat over a sibling.
    Unreadable(String),
}

impl Miss {
    fn exit(&self) -> i32 {
        match self {
            // Not answers about the mirror's contents — failures to answer at all. The set is
            // exactly `!answered()` plus `Folder`; a variant that says «we could not look» while
            // exiting 3 would be claiming both halves of a contradiction.
            Miss::StateUnreadable(_)
            | Miss::StateLost
            | Miss::Unreadable(_)
            | Miss::NotThisMirror
            | Miss::Folder => EXIT_FAILED,
            _ => EXIT_NOT_IN_MIRROR,
        }
    }

    /// Did this mount actually ANSWER the question, or merely fail to?
    ///
    /// The distinction decides whether a hit elsewhere may be served as unambiguous. `NotHeld`,
    /// `NeverSynced`, a park, a folder and a vanished file are all answers: this mount does not
    /// hold it, and a hit in a sibling is the only one there is. `StateUnreadable`,
    /// `NotThisMirror` and `Unreadable` are NOT — the mount may hold that path and we could not
    /// look — so a bare multi-mount read that serves over one of them is claiming a uniqueness
    /// it never established.
    fn answered(&self) -> bool {
        !matches!(
            self,
            Miss::StateUnreadable(_) | Miss::StateLost | Miss::NotThisMirror | Miss::Unreadable(_)
        )
    }

    /// The machine code, which is NOT derivable from the exit code. A folder addressed as a note
    /// and a state file that would not read share exit 2 and share nothing else: one is the
    /// caller's mistake, the other is ours. A consumer branching on `code` has to be able to tell
    /// them apart, so `unavailable` is never used for a usage error.
    ///
    /// **`unavailable` is exactly `!answered()`.** The three lines have to agree or the answer
    /// contradicts itself: `NotThisMirror` said «we could not look» to one caller and «we looked
    /// and it is not here» to another, and the second is the conclusion this verb must never let
    /// anyone reach by accident.
    fn code(&self) -> &'static str {
        match self {
            Miss::StateUnreadable(_)
            | Miss::StateLost
            | Miss::Unreadable(_)
            | Miss::NotThisMirror => "unavailable",
            Miss::Folder => "usage",
            _ => "not_in_mirror",
        }
    }

    fn sentence(&self) -> String {
        match self {
            Miss::NeverSynced => {
                "this mount has never been synced, so it holds nothing yet - run `docli sync`"
                    .into()
            }
            Miss::StateLost => "this mirror's record of what it holds is gone (.docli/state) - \
                                `docli sync --full` rebuilds it, and `docli doctor` reports it \
                                as `state-missing`"
                .into(),
            Miss::StateUnreadable(e) => format!(
                "the local mirror state could not be read ({e}), so nothing can be served from \
                 this mount - `docli sync --full` rebuilds it"
            ),
            Miss::NotThisMirror => {
                "this directory is not this workspace's mirror - `docli init` re-points it".into()
            }
            // A TRANSIENT park heals; a STRUCTURAL one does not, and promising a fix for it
            // would send the reader to a command that names none. `sync --check` renders the
            // exact remedy for every transient cause, so it is one authority rather than a
            // second copy of seven of them.
            Miss::Parked(ParkClass::Transient, r) => format!(
                "the latest version of this note or file could not be written to the mirror \
                 ({r}) - `docli sync --check` names the fix"
            ),
            Miss::Parked(ParkClass::Structural, r) => format!(
                "this one cannot be stored in this mirror at all ({r}) - read it over the docli \
                 MCP connection"
            ),
            Miss::LedgerOnly => "this workspace holds it, but this mount does not mirror it \
                                 (outside the mount's folder scope, or a type this mirror does \
                                 not store) - read it over the docli MCP connection"
                .into(),
            Miss::NotHeld => "this mirror does not hold it - it may be outside the mount's \
                              folder scope, not synced yet, or not on the server; \
                              `docli sync --check` tests the mirror, and a `docli search` that \
                              does not report an incomplete index tests the server"
                .into(),
            Miss::Folder => "that is a folder - give `docli read` a note or file path, or an \
                             `--id` that names a note or a file"
                .into(),
            Miss::Gone(e) => format!(
                "the mirror tracks it but nothing is there any more ({e}) - `docli sync --full` \
                 re-derives the mirror"
            ),
            Miss::Escaped => "what is at that path resolves outside the mirror, so it will not \
                              be served - read it over the docli MCP connection; `docli doctor` \
                              reports the discrepancy"
                .into(),
            Miss::Unreadable(e) => format!(
                "the mirror tracks it but could not read its local copy ({e}) - this says \
                 nothing about whether it exists; read it over the docli MCP connection"
            ),
        }
    }
}

enum Probe {
    Hit(Box<Loaded>),
    Miss(Miss),
}

struct Loaded {
    mount: String,
    workspace: Uuid,
    id: Uuid,
    node: NodeState,
    bytes: Vec<u8>,
    /// `WsState::unusable_reason` — the shared readiness predicate `search` asks. `Some` does
    /// NOT stop the read (D8: serve and disclose); it becomes a disclosure.
    unusable: Option<&'static str>,
    /// The note graph for this workspace, or the reason there is none (v0.29.1 Half 2).
    graph: GraphSlot,
    /// Does this mount carry a folder scope? The graph is WORKSPACE-wide by design, so under a
    /// scope it names notes this mirror does not hold — which `read` has to disclose rather than
    /// let the reader discover through an exit 3.
    scoped: bool,
    /// v0.29.7 D4 — the server named this node as content-changed and no sync has applied it yet.
    ///
    /// Carried on the HIT rather than turned into a [`Miss`] on purpose. As a miss it would empty
    /// `hits`, and the two-mounts-hold-one-path case would stop being an ambiguity refusal and
    /// start being resolved BY staleness — picking whichever copy happened to be fresh, which is
    /// exactly the «answer depends on something the output never reveals» the ambiguity refusal
    /// exists to prevent.
    stale: bool,
}

/// Held, or absent with a sentence. There is deliberately no third state: a graph we cannot read
/// and a graph nobody fetched are both «not held», and the sentence is the only thing that
/// differs.
enum GraphSlot {
    Held(Box<crate::graph::Graph>),
    Absent(&'static str),
}

/// Resolve the graph for a workspace against the state the mirror is currently at.
///
/// The stamp does the work: `load_graph` returns the cache only when its `(epoch, cursor)`
/// byte-equals the state's, so a mirror that moved on gets `GRAPH_STALE` rather than a graph
/// describing an earlier shape of itself. The file's mere PRESENCE is what tells a stale cache
/// apart from no cache — which is why this reads `graph_path` after the miss instead of treating
/// every miss the same.
///
/// The stamp is not sufficient on its own, though, and `from_zero` is the reason. A pending
/// rebuild leaves `state.cursor` at its last durable value while `apply_page` rewrites the mirror
/// toward head — so the stamp keeps MATCHING while the bytes on disk stop being the ones the
/// graph describes. `read` would then pair new content with an old graph and answer
/// `backlinks: []` over a link the rebuild had already delivered. That is a false negative in a
/// relational claim, which is the one failure mode D5 exists to forbid, so it outranks the
/// disclose-don't-refuse rule: a `mirror_not_usable` disclosure fires here too, but a caveat
/// beside a confidently wrong list is not the same as not answering.
fn graph_slot(control: &ControlRoot, ws: Uuid, st: &WsState) -> GraphSlot {
    if st.from_zero {
        return GraphSlot::Absent(GRAPH_REBUILDING);
    }
    if let Some(c) = control.load_graph(ws, st.epoch, st.cursor) {
        return GraphSlot::Held(Box::new(crate::graph::Graph::new(c.graph)));
    }
    if control.graph_path(ws).exists() {
        return GraphSlot::Absent(GRAPH_STALE);
    }
    GraphSlot::Absent(if st.graph_asked {
        GRAPH_NOT_SERVED
    } else {
        GRAPH_NOT_SYNCED
    })
}

pub fn resolve(project: &Project, args: &ReadArgs, now: i64) -> Outcome {
    let target = match (args.path.as_deref(), args.id) {
        (Some(p), None) => {
            let p = normalize_server_path(p);
            if p.is_empty() {
                return refuse(
                    "usage",
                    "usage: docli read <server-path>   (the path `docli search` prints)",
                    EXIT_FAILED,
                );
            }
            Target::Path(p)
        }
        (None, Some(id)) => Target::Id(id),
        _ => {
            return refuse(
                "usage",
                "give exactly one address: a server path, or `--id <uuid>`",
                EXIT_FAILED,
            )
        }
    };
    let mounts = match select_mounts(project, args.mount.as_deref()) {
        Ok(m) => m,
        Err(r) => return Outcome::Refused(r),
    };
    let control = project.control_root();

    let mut hits: Vec<Loaded> = Vec::new();
    let mut misses: Vec<(String, Miss)> = Vec::new();
    for m in &mounts {
        match probe(project, &control, m, &target, now) {
            Probe::Hit(l) => hits.push(*l),
            Probe::Miss(miss) => misses.push((m.display_name().to_string(), miss)),
        }
    }

    // Two mounts CAN hold one server path (D2). Refuse rather than pick: picking would make the
    // answer depend on mount order in `docli.toml`, which nothing in the output reveals.
    if hits.len() > 1 {
        let tokens: Vec<String> = hits.iter().map(|h| selector_token(project, h)).collect();
        return refuse(
            "ambiguous",
            format!(
                "{} mounts hold the requested note or file - select one with `--mount`: {}",
                hits.len(),
                tokens.join(", ")
            ),
            EXIT_FAILED,
        );
    }
    if let Some(hit) = hits.pop() {
        // The staleness gate sits HERE — past the ambiguity refusal, so a second mount holding
        // the same path is still an ambiguity rather than being silently resolved by which copy
        // is fresh, and ahead of `serve`, because a refusal must print no body.
        if hit.stale {
            return refuse("stale", STALE_REFUSAL, EXIT_STALE);
        }
        // A sibling that could not answer at all leaves the uniqueness UNVERIFIED (Codex round
        // 1). Serving is still right — the bytes we have are the bytes we have, and refusing
        // would make one broken mount hide every other mount's notes — but D8's rule is that
        // what cannot be vouched for is disclosed, and «this is the only copy» is exactly such a
        // claim. Silent, it is the ambiguity refusal defeated by a mount that happens to be
        // broken instead of by one that happens to hold the path.
        let unverified: Vec<&str> = misses
            .iter()
            .filter(|(_, m)| !m.answered())
            .map(|(name, _)| name.as_str())
            .collect();
        return match serve(hit, args) {
            Ok(mut s) => {
                if !unverified.is_empty() {
                    s.envelope.disclose(Disclosure {
                        code: "mounts_unresolved",
                        message: format!(
                            "{} could not be consulted, so another mount may also hold the \
                             requested note or file - select the intended mount with `--mount`; \
                             `docli status` lists them",
                            unverified
                                .iter()
                                .map(|n| format!("`{}`", crate::ui::sanitize(n)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
                Outcome::Served(Box::new(s))
            }
            Err(r) => Outcome::Refused(r),
        };
    }

    // Nothing held it. Report every mount's own reason — a single aggregated "not found" would
    // hide the one case that matters (a mount whose state would not read said nothing about its
    // contents at all).
    // A mount that could not answer AT ALL dominates one that answered «not here»: the second is
    // a fact about the mirror, the first is a gap in what we know. Its code travels with it.
    //
    // The precedence is TOTAL and mount-order-independent, deliberately: `unavailable` (our
    // outage) outranks `usage` (the caller's mistake) outranks `not_in_mirror`. Taking the first
    // exit-2 miss in `docli.toml` order would make the headline code depend on the order of a
    // config file — the very thing the ambiguity refusal above refuses to let decide an answer.
    let rank = |m: &Miss| match m.code() {
        "unavailable" => 2,
        "usage" => 1,
        _ => 0,
    };
    let (exit, code) = misses
        .iter()
        .max_by_key(|(_, m)| rank(m))
        .map(|(_, m)| (m.exit(), m.code()))
        .unwrap_or((EXIT_NOT_IN_MIRROR, "not_in_mirror"));
    let message = if misses.len() == 1 {
        misses[0].1.sentence()
    } else {
        misses
            .iter()
            .map(|(name, m)| format!("[{}] {}", crate::ui::sanitize(name), m.sentence()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    refuse(code, message, exit)
}

/// A `--mount` token that really would select this hit.
///
/// The workspace id always works. The readable display name is offered instead only when
/// [`select_mounts`] — the actual resolver, asked here rather than re-implemented — resolves it
/// to exactly this mount. Re-deriving the condition (uniqueness among names, say) is the «two
/// readers of one question» defect: a name can also collide with another mount's workspace id,
/// and a hand-written uniqueness check would happily print a token the resolver then refuses.
fn selector_token(project: &Project, l: &Loaded) -> String {
    match select_mounts(project, Some(&l.mount)) {
        Ok(m) if m.len() == 1 && m[0].workspace == l.workspace => crate::ui::sanitize(&l.mount),
        _ => l.workspace.to_string(),
    }
}

/// Server paths arrive from agents and from our own output. Accept the shapes that mean the same
/// thing (a stray leading `/` or `./`, surrounding whitespace) and nothing more — a fuzzier match
/// would be a second resolver, which D3 refuses.
fn normalize_server_path(p: &str) -> String {
    p.trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Resolve `--mount`. Accepts what `search` PRINTS (the mount's display name) and the workspace
/// id (what `docli list` and `docli.toml` carry).
///
/// Display names are not unique — `validate_config` enforces uniqueness on `workspace` only — so
/// an ambiguous name is REFUSED with the ids that disambiguate it, rather than resolved by
/// position (open item 3).
fn select_mounts<'a>(project: &'a Project, sel: Option<&str>) -> Result<Vec<&'a Mount>, Refusal> {
    let all: Vec<&Mount> = project.config.mounts.iter().collect();
    let Some(sel) = sel else {
        return Ok(all);
    };
    // A flag given EXPLICITLY is never silently discarded — the rule `docli init` already
    // applies to its stray mount-shaping flags. `--mount "  "` used to fall through to «no
    // selector», which searches every mount: the caller asked to be scoped and was not, and
    // said nothing about it.
    let sel = sel.trim();
    if sel.is_empty() {
        return Err(Refusal {
            code: "usage",
            message: "`--mount` needs a mount name or a workspace id - `docli status` lists them"
                .into(),
            exit: EXIT_FAILED,
        });
    }
    // NAME first, then workspace id. The two selector spaces cannot collide — `validate_config`
    // refuses a mount named with another mount's workspace id, at the door, because that one
    // string would otherwise mean two mounts across every surface that prints a mount tag — so
    // the order here decides nothing and is simply the cheaper test first.
    // Both sides TRIMMED. The selector is already trimmed above, so comparing it against a raw
    // label is an asymmetry, and asymmetric normalization is how a token stops meaning what it
    // named: `search` tags a mount `" notes "`, the reader passes that tag back, it is trimmed to
    // `notes`, and a DIFFERENT mount named `notes` answers. `validate_config` trims the same way.
    let by_name: Vec<&Mount> = all
        .iter()
        .copied()
        .filter(|m| m.display_name().trim() == sel)
        .collect();
    if by_name.len() > 1 {
        return Err(Refusal {
            code: "ambiguous_mount",
            message: format!(
                "`{}` names {} mounts - use the workspace id instead: {}",
                crate::ui::sanitize(sel),
                by_name.len(),
                by_name
                    .iter()
                    .map(|m| m.workspace.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            exit: EXIT_FAILED,
        });
    }
    if !by_name.is_empty() {
        return Ok(by_name);
    }
    // One mount per workspace is a config invariant, so this is at most one mount.
    if let Ok(ws) = Uuid::parse_str(sel) {
        let found: Vec<&Mount> = all.into_iter().filter(|m| m.workspace == ws).collect();
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Err(Refusal {
        code: "no_such_mount",
        message: format!(
            "no mount matches `{}` - `docli status` lists mount names and workspace ids",
            crate::ui::sanitize(sel)
        ),
        exit: EXIT_FAILED,
    })
}

fn probe(
    project: &Project,
    control: &ControlRoot,
    mount: &Mount,
    target: &Target,
    now: i64,
) -> Probe {
    let mount_root = mount_abs(&project.root, mount);
    // STATE FIRST, then identity — the same order `search_cmd::read_local` takes, and for the
    // reason that made it right there. State lives in the CONTROL ROOT — `~/.docli/state/<ws>.json`
    // since the mirror went per-machine in v0.29.2 — and nowhere near the mount, so reading it
    // needs no claim on the mount directory; identity has to precede any OPEN, which is below it.
    //
    // Reversed, the two commands answer a fresh `docli init` differently. Nothing but
    // `sync`/`doctor` CREATES a mount directory (`claim_mount`), so between `init` and the
    // first sync the directory does not exist, `verify_mount_identity` is false, and an
    // identity-first probe replies «this directory is not this workspace's mirror - `docli
    // init` re-points it» — naming the command just run, over the one state that has its own
    // sentence and its own remedy.
    // State is keyed by WORKSPACE and says nothing about the directory configured NOW; the
    // marker in the directory is the only thing that binds the two. Measured here, applied below
    // — it is also what tells a mount that was never synced apart from one whose RECORD was lost.
    let claimed = crate::mountfs::verify_mount_identity(&mount_root, &control.dir, mount.workspace);
    let st = match control.load_state(mount.workspace) {
        Err(e) => return Probe::Miss(Miss::StateUnreadable(format!("{e:#}"))),
        // No state and no claim: nothing was ever synced here. That IS an answer.
        Ok(None) if !claimed => return Probe::Miss(Miss::NeverSynced),
        // No state but the directory carries our marker: it was synced, and we lost the record
        // of what it holds — the mirror may hold the note perfectly well. Not an answer.
        Ok(None) => return Probe::Miss(Miss::StateLost),
        Ok(Some(st)) => st,
    };
    if !claimed {
        return Probe::Miss(Miss::NotThisMirror);
    }
    // MARKS BEFORE BYTES, and the order is the whole of this gate's correctness (Codex round 1).
    //
    // Observed after the read, the check answers about a DIFFERENT moment than the bytes: a
    // concurrent sync can advance this node's stored rev in between, retiring the claim, so `read`
    // would find nothing and serve the older bytes it had already captured — defeating a claim that
    // genuinely applied to the bytes when the file was opened. Nothing locks either file, so the fix is
    // ordering, not exclusion: a claim seen here means we refuse before reading anything, and one
    // that appears AFTER this point describes a change we could not have known about when we
    // looked, which is the temporal blind spot D5 already accepts.
    //
    // An absent or unreadable file is simply NO marks — `read` stays offline, and the gate
    // degrades to the v0.29.0 behaviour rather than to a refusal.
    let marked = control.load_marks(mount.workspace);
    // PARKS FIRST (open item 1). A parked node is absent from `state.nodes` BY CONSTRUCTION, so
    // a nodes-miss checked first makes every park case unreachable and the reader gets the
    // generic "this mirror does not hold it" over a node whose exact reason we know.
    if let Some(park) = find_park(&st, target) {
        return Probe::Miss(Miss::Parked(park.class, park.reason.clone()));
    }
    let Some((id, node)) = find_node(&st, target) else {
        return Probe::Miss(match target {
            Target::Id(id) if st.ledger.contains(id) => Miss::LedgerOnly,
            _ => Miss::NotHeld,
        });
    };
    if node.kind == TrackedKind::Folder {
        return Probe::Miss(Miss::Folder);
    }
    let (abs, containment_root) = match file_abs(&project.root, control, &mount_root, mount, node) {
        Ok(v) => v,
        // A state record we cannot turn into a path at all — an attachment with no marker
        // recorded, a stored path that escapes containment, a project root that will not
        // resolve — is a failure to look, never a statement about what the mirror holds.
        Err(e) => return Probe::Miss(Miss::Unreadable(e)),
    };
    // Canonical containment: `contained_join` is lexical, so a symlink planted inside the mirror
    // would otherwise let `docli read` print any file the user can open. Canonicalizing also
    // refuses a path that does not exist, which is the "gone" answer.
    //
    // The RESOLVED path is what gets read, not the one we checked. `read` is deliberately
    // lock-free — unlike the write path, which holds the mount claim and has already walked the
    // tree with `refuse_symlinks` — so a check against one path followed by an open of another
    // leaves a window for a swap between them. Reading what `canonicalize` returned closes it
    // for every link on the way in; only the resolved leaf itself remains, and that is a local
    // attacker who can already write inside the mirror.
    let real = match crate::mountfs::canonical_within(&containment_root, &abs) {
        crate::mountfs::Containment::Inside(real) => real,
        crate::mountfs::Containment::Missing => {
            return Probe::Miss(Miss::Gone("nothing is at that path".into()))
        }
        // It exists — it is just not ours. A fact about this mirror, so exit 3; and a sentence
        // that does not claim the file vanished, because it did not.
        crate::mountfs::Containment::Escaped => return Probe::Miss(Miss::Escaped),
        // We could not look. Same rule as the read below: never exit 3, never «answered».
        crate::mountfs::Containment::Unresolvable(e) => {
            return Probe::Miss(Miss::Unreadable(e.to_string()))
        }
    };
    let bytes = match std::fs::read(&real) {
        Ok(b) => b,
        // Only a VANISHED file is «gone». A permission error, an I/O fault, or a directory where
        // the note belongs are all failures to LOOK, and each keeps this mount unanswered.
        // (`NotFound` is still reachable despite the resolve above — the file can go between
        // resolving it and opening it.)
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Probe::Miss(Miss::Gone(e.to_string()))
        }
        Err(e) => return Probe::Miss(Miss::Unreadable(e.to_string())),
    };
    Probe::Hit(Box::new(Loaded {
        mount: mount.display_name().to_string(),
        workspace: mount.workspace,
        id,
        node: node.clone(),
        bytes,
        unusable: st.unusable_reason(mount.folder.as_deref(), now),
        graph: graph_slot(control, mount.workspace, &st),
        scoped: mount.folder.is_some(),
        // From the snapshot taken ABOVE, before the bytes were read — and resolved against the
        // stamp this mirror actually holds, so a mark the mirror has since caught up with is
        // simply satisfied rather than needing to have been removed.
        stale: marked.contradict(id, node.rev),
    }))
}

fn find_park<'a>(st: &'a WsState, t: &Target) -> Option<&'a Park> {
    match t {
        Target::Id(id) => st.parks.get(id),
        Target::Path(p) => st.parks.values().find(|park| park.server_path == *p),
    }
}

fn find_node<'a>(st: &'a WsState, t: &Target) -> Option<(Uuid, &'a NodeState)> {
    match t {
        Target::Id(id) => st.nodes.get(id).map(|n| (*id, n)),
        Target::Path(p) => st
            .nodes
            .iter()
            .find(|(_, n)| n.server_path == *p)
            .map(|(id, n)| (*id, n)),
    }
}

/// The absolute path of what this node materializes, plus the root its canonical form must stay
/// under. An attachment's marker may have RELOCATED into the control root, which is a different
/// root from the mount — resolved through STATE (`marker_path`), never by re-deriving.
fn file_abs(
    project_root: &Path,
    control: &ControlRoot,
    mount_root: &Path,
    mount: &Mount,
    node: &NodeState,
) -> Result<(PathBuf, PathBuf), String> {
    if node.kind == TrackedKind::Attachment {
        let mp = node
            .marker_path
            .as_deref()
            .ok_or_else(|| "the mirror recorded no marker for it".to_string())?;
        let abs = crate::apply::marker_abs(control, mount_root, mount.workspace, mp)
            .map_err(|e| format!("{e:#}"))?;
        if !mp.starts_with(".docli/") {
            return Ok((abs, canonical_root(mount_root)?));
        }
        // A RELOCATED marker's containment root is the workspace's OWN marker namespace — the
        // same boundary `relocated_leaf` enforces lexically, so the canonical check cannot be
        // looser than the parse that produced the path.
        //
        // It is built by joining onto the CANONICAL PROJECT ROOT rather than canonicalized
        // itself. `.docli/`, `markers/` and the per-workspace subdir are ordinary directories the
        // CLI creates, with no ownership marker and no symlink refusal of their own — unlike the
        // mount, which `verify_mount_identity` has already established is not a link. Canonicalizing
        // that root would resolve any link in the chain and then compare the file against its own
        // target, which always matches: a `markers/<ws> -> /elsewhere` link would serve
        // /elsewhere's marker and pass containment. Anchoring lexically means a link anywhere
        // in the chain lands the file outside the expected prefix, and it is refused.
        let root = canonical_root(project_root)?
            .join(".docli")
            .join("markers")
            .join(mount.workspace.to_string());
        return Ok((abs, root));
    }
    let abs = crate::mountfs::contained_join(mount_root, &node.local_path)
        .map_err(|e| format!("{e:#}"))?;
    Ok((abs, canonical_root(mount_root)?))
}

/// The canonical form of a root `canonical_within` will be compared against. The mount root is
/// safe to canonicalize because `verify_mount_identity` has already refused it if it is a link;
/// the project root is the directory holding `docli.toml`, which is where we were invoked.
fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|e| format!("{} could not be resolved ({e})", root.display()))
}

// ---------------------------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------------------------

fn serve(mut l: Loaded, args: &ReadArgs) -> Result<Served, Refusal> {
    let mut disclosures = Vec::new();
    // The digest DESCRIBES WHAT IT OBSERVED and asserts nothing about how it got that way
    // (open item 1): a concurrent `docli sync` writing this very file produces the same
    // observation as innocently as a hand edit does.
    let observed = crate::apply::sha_hex(&l.bytes);
    // An ABSENT baseline is not a passing one. `.docli/state` is untrusted input, and only a
    // FOLDER is written with an empty digest (`apply.rs` — and `probe` refuses folders before
    // this), so an empty one here means the check cannot be made rather than that it passed.
    // Skipping it silently would let exactly the file whose provenance is unknown be the one
    // served with no caveat.
    if l.node.content_sha256.is_empty() {
        disclosures.push(Disclosure {
            code: "digest_unknown",
            message: "the mirror recorded no digest for this file, so these bytes cannot be \
                      checked against what the server sent; `docli sync --full` re-derives the \
                      mirror"
                .into(),
        });
    } else if observed != l.node.content_sha256 {
        disclosures.push(Disclosure {
            code: "digest_mismatch",
            message: "the bytes on disk are not the bytes the mirror recorded writing - the \
                      content may differ from what the server holds; `docli sync --full` \
                      re-derives the mirror"
                .into(),
        });
    }
    // The graph covers the whole workspace while this mount may be a folder of it — deliberately,
    // since an edge's other endpoint is routinely outside the scope and a clipped graph would
    // report a note as unlinked because the mount is narrow. Stated ONCE here, at the level of
    // the whole answer: per-reference annotation would mean a state lookup per edge, and the
    // reader's actual next step is already unambiguous (`docli read` on such a path exits 3,
    // which is a fact about this mirror and never about the server).
    if l.scoped && matches!(l.graph, GraphSlot::Held(_)) {
        disclosures.push(Disclosure {
            code: "graph_wider_than_mount",
            message: SCOPE_DISCLOSURE.into(),
        });
    }
    if let Some(reason) = l.unusable {
        disclosures.push(Disclosure {
            code: "mirror_not_usable",
            message: format!(
                "the local mirror cannot be vouched for right now - {reason}; \
                 `docli sync --check` either clears the condition or names the fix"
            ),
        });
    }
    let text = match String::from_utf8(std::mem::take(&mut l.bytes)) {
        Ok(t) => t,
        Err(e) => {
            disclosures.push(Disclosure {
                code: "not_utf8",
                message: "the file is not valid UTF-8 - invalid sequences were replaced; \
                          `docli sync --full` re-derives the mirror"
                    .into(),
            });
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
    };
    let name = leaf_of(&l.node.server_path);
    match l.node.kind {
        TrackedKind::Attachment => {
            if args.lines.is_some() {
                return Err(Refusal {
                    code: "usage",
                    message: "`--lines` reads a note; this is a file - drop `--lines` for its \
                              metadata, or fetch its bytes with `read_attachment` over the \
                              docli MCP connection"
                        .into(),
                    exit: EXIT_FAILED,
                });
            }
            Ok(serve_attachment(l, name, text, disclosures))
        }
        // `probe` refuses folders before anything is read.
        TrackedKind::Folder => unreachable!("a folder never reaches serve"),
        TrackedKind::Note => serve_note(l, name, text, disclosures, args.lines.as_deref()),
    }
}

fn serve_note(
    l: Loaded,
    name: String,
    text: String,
    disclosures: Vec<Disclosure>,
    lines: Option<&str>,
) -> Result<Served, Refusal> {
    let span = match lines {
        Some(spec) => parse_lines(spec).map_err(|message| Refusal {
            code: "usage",
            message,
            exit: EXIT_FAILED,
        })?,
        None => (1, None),
    };
    let (body, lines) = slice_lines(&text, span.0, span.1).map_err(|message| Refusal {
        code: "usage",
        message,
        exit: EXIT_FAILED,
    })?;
    let mut absent = BTreeMap::new();
    // The graph fields are filled TOGETHER or absent together. They come from one snapshot, so a
    // half-filled envelope would be a set of answers about different moments.
    let mut g = GraphFields::default();
    match &l.graph {
        GraphSlot::Held(graph) => {
            g.title = graph.node(l.id).and_then(|n| n.title.clone());
            if g.title.is_none() {
                absent.insert("title".into(), NO_TITLE.into());
            }
            g.aliases = Some(
                graph
                    .node(l.id)
                    .map(|n| n.aliases.clone())
                    .unwrap_or_default(),
            );
            g.links = Some(json_list(graph.forward_links(l.id)));
            g.backlinks = Some(json_list(graph.backlinks(l.id)));
            g.embeds = Some(json_list(graph.attachment_embeds(l.id)));
            g.unresolved = Some(graph.unresolved_refs(l.id));
            g.tags = Some(graph.tags(l.id));
        }
        GraphSlot::Absent(why) => {
            for f in [
                "title",
                "aliases",
                "links",
                "unresolved",
                "embeds",
                "backlinks",
                "tags",
            ] {
                absent.insert(f.to_string(), (*why).to_string());
            }
        }
    }
    absent.insert("frontmatter".into(), FRONTMATTER_ABSENT.into());
    absent.insert("relatedHint".into(), RELATED_ABSENT.into());
    // The human surface keeps stdout EXACTLY the note (`docli read a.md > a.md` must round-trip),
    // so the graph goes to stderr as one line beside the disclosures — visible without being in
    // the product. Under `--json` it is the envelope that carries it.
    let summary = g.summary();
    Ok(Served {
        envelope: Envelope::Note(NoteEnvelope {
            kind: "note",
            id: l.id,
            path: l.node.server_path.clone(),
            name,
            mount: l.mount,
            workspace: l.workspace,
            content: Some(body.clone()),
            lines,
            title: g.title,
            aliases: g.aliases,
            links: g.links,
            unresolved: g.unresolved,
            embeds: g.embeds,
            backlinks: g.backlinks,
            tags: g.tags,
            frontmatter: None,
            related_hint: None,
            absent,
            disclosures,
        }),
        body,
        note: summary,
    })
}

/// The note arm's graph fields, gathered before the envelope so the stderr summary and the JSON
/// can be built from one value.
#[derive(Default)]
struct GraphFields {
    title: Option<String>,
    aliases: Option<Vec<String>>,
    links: Option<Vec<serde_json::Value>>,
    backlinks: Option<Vec<serde_json::Value>>,
    embeds: Option<Vec<serde_json::Value>>,
    unresolved: Option<Vec<String>>,
    tags: Option<Vec<String>>,
}

impl GraphFields {
    /// One stderr line, or nothing when the graph is not held (the absence already has a
    /// disclosure; repeating it as a counts line would say it twice).
    fn summary(&self) -> Option<String> {
        let links = self.links.as_ref()?.len();
        let backlinks = self.backlinks.as_ref()?.len();
        let embeds = self.embeds.as_ref()?.len();
        let unresolved = self.unresolved.as_ref()?.len();
        let mut parts = vec![
            format!("links {links}"),
            format!("backlinks {backlinks}"),
            format!("files {embeds}"),
        ];
        if unresolved > 0 {
            parts.push(format!("unresolved {unresolved}"));
        }
        if let Some(tags) = &self.tags {
            if !tags.is_empty() {
                parts.push(format!("tags {}", tags.join(", ")));
            }
        }
        Some(parts.join(" · "))
    }
}

fn json_list<T: serde::Serialize>(v: Vec<T>) -> Vec<serde_json::Value> {
    v.into_iter()
        .map(|x| serde_json::to_value(x).expect("a graph ref serializes"))
        .collect()
}

fn serve_attachment(
    l: Loaded,
    name: String,
    marker: String,
    disclosures: Vec<Disclosure>,
) -> Served {
    let fields = parse_marker(&marker);
    let mut absent = BTreeMap::new();
    absent.insert("content".into(), ATTACHMENT_BYTES_ABSENT.into());
    // The marker's two DECLARED unknowns, kept apart from a parse failure: `sha256 unknown` is
    // the server saying it has no digest for these bytes, and `wikilink not-expressible` is the
    // api's own `wikilink_for` NULL rule mirrored — neither is a missing line.
    let sha256 = match fields.get("sha256").map(String::as_str) {
        Some("unknown") | None => {
            absent.insert(
                "sha256".into(),
                "the server reported no digest for these bytes".into(),
            );
            None
        }
        Some(v) => Some(v.to_string()),
    };
    let wikilink = match fields.get("wikilink").map(String::as_str) {
        Some("not-expressible") | None => {
            absent.insert(
                "wikilink".into(),
                "no wikilink can name this path - use `path` with the MCP tools".into(),
            );
            None
        }
        Some(v) => Some(v.to_string()),
    };
    let mime = match fields.get("mime").map(String::as_str) {
        Some("unknown") | None => {
            absent.insert(
                "mime".into(),
                "the server reported no MIME type for it".into(),
            );
            None
        }
        Some(v) => Some(v.to_string()),
    };
    let bytes = match fields.get("size").and_then(|s| s.parse::<u64>().ok()) {
        Some(v) => Some(v),
        None => {
            absent.insert(
                "bytes".into(),
                "the marker carries no readable size - `docli sync --full` rewrites it".into(),
            );
            None
        }
    };
    // The fifth predicate, whose subject is a file: which live notes embed it.
    let embedded_in = match &l.graph {
        GraphSlot::Held(graph) => Some(json_list(graph.attachment_embedders(l.id))),
        GraphSlot::Absent(why) => {
            absent.insert("embeddedIn".into(), (*why).to_string());
            None
        }
    };
    let note = embedded_in.as_ref().map(|v| {
        format!(
            "embedded in {}",
            crate::ui::plural(v.len(), "note", "notes")
        )
    });
    Served {
        envelope: Envelope::File(FileEnvelope {
            kind: "attachment",
            id: l.id,
            path: l.node.server_path.clone(),
            name,
            mount: l.mount,
            workspace: l.workspace,
            mime,
            bytes,
            sha256,
            wikilink,
            embedded_in,
            content: None,
            marker: marker.clone(),
            absent,
            disclosures,
        }),
        body: marker,
        note,
    }
}

fn leaf_of(server_path: &str) -> String {
    server_path
        .rsplit('/')
        .next()
        .unwrap_or(server_path)
        .to_string()
}

/// `A-B`, `A-` (to the end) or a bare `A` (one line). All 1-based and inclusive.
fn parse_lines(spec: &str) -> Result<(usize, Option<usize>), String> {
    let bad = || {
        format!(
            "`--lines {}` is not a range - use `40-80`, `40-` for the rest, or `40` for one line",
            crate::ui::sanitize(spec)
        )
    };
    let spec = spec.trim();
    let num = |s: &str| -> Result<usize, String> {
        s.trim().parse::<usize>().map_err(|_| bad()).and_then(|n| {
            if n == 0 {
                Err("line numbers start at 1".to_string())
            } else {
                Ok(n)
            }
        })
    };
    match spec.split_once('-') {
        None => {
            let a = num(spec)?;
            Ok((a, Some(a)))
        }
        Some((a, "")) => Ok((num(a)?, None)),
        Some((a, b)) => {
            let (a, b) = (num(a)?, num(b)?);
            if b < a {
                return Err(format!("`--lines {a}-{b}` ends before it starts"));
            }
            Ok((a, Some(b)))
        }
    }
}

/// Slice by lines, keeping every terminator, so a whole-note read is byte-identical to the file
/// and a range is byte-identical to that stretch of it.
fn slice_lines(
    content: &str,
    from: usize,
    to: Option<usize>,
) -> Result<(String, LineSpan), String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    if total == 0 {
        // An empty note is a real thing server-side (the D7 empty-note contract). Asking for
        // line 1 of it is answered with nothing; asking for line 9 is the same past-the-end
        // mistake as anywhere else. The span reads `0-0 of 0`, because `1-0` would look like a
        // malformed range rather than an empty note.
        if from > 1 {
            return Err("the note is empty (0 lines)".to_string());
        }
        return Ok((
            String::new(),
            LineSpan {
                from: 0,
                to: 0,
                total: 0,
            },
        ));
    }
    if from > total {
        return Err(format!(
            "line {from} is past the end - the note has {total} lines"
        ));
    }
    let to = to.unwrap_or(total).min(total);
    Ok((lines[from - 1..to].concat(), LineSpan { from, to, total }))
}

/// The marker's `key value` lines (`markers.rs` renders them sorted, one space, no quoting).
fn parse_marker(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

/// Should stdout get a closing newline the body does not have?
///
/// The missing final newline is a TERMINAL courtesy and nothing more, so it is gated on stdout
/// actually being one. A note without a trailing newline is an ordinary thing (Obsidian writes
/// plenty), and `docli read x.md > file` has to BE the file — silently adding a byte to a
/// redirect would make `read` the one command here that does not return what the mirror holds.
///
/// Split out as a pure function ON PURPOSE: `render` writes to process stdout and cannot be
/// asserted, so the byte-exactness claim would otherwise be the one guarantee in this slice
/// resting on a manual check that a later refactor could drop in silence.
fn needs_closing_newline(body: &str, stdout_is_a_terminal: bool) -> bool {
    stdout_is_a_terminal && !body.is_empty() && !body.ends_with('\n')
}

/// Write the product to stdout, treating a CLOSED PIPE as a normal end.
///
/// `print!` PANICS when stdout will not take the bytes, and the documented flow for this verb is
/// `docli read … | head` — a reader that closes the pipe on purpose, after which the next write
/// fails. Panicking there would exit 101 with a Rust backtrace over a pipeline that did exactly
/// what it was asked to. Rust masks `SIGPIPE`, so this is the only place it can be handled.
///
/// A note is often larger than a pipe buffer, so this is an ordinary outcome, not a corner: the
/// bytes the reader wanted were produced, and the command succeeded.
fn write_product(body: &str, add_newline: bool) -> Result<(), std::io::Error> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(body.as_bytes())?;
    if add_newline {
        out.write_all(b"\n")?;
    }
    out.flush()
}

/// A closed pipe is the reader's decision, not our failure.
fn broken_pipe(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::BrokenPipe
}

fn render(outcome: Outcome, json: bool) -> i32 {
    match outcome {
        Outcome::Refused(r) => {
            if json {
                // A machine caller gets a parseable answer even on a refusal — an agent that has
                // to fall back to reading stderr prose is an agent that will guess.
                let body = serde_json::json!({"error": {"code": r.code, "message": r.message}});
                match write_product(&body.to_string(), true) {
                    // The reader hung up. The answer was produced; the refusal's own code stands.
                    Ok(()) => r.exit,
                    Err(e) if broken_pipe(&e) => r.exit,
                    // Anything else and the JSON never landed, so `r.exit` would be a LIE: exit 3
                    // over an undelivered answer is indistinguishable from «not in this mirror»,
                    // which is the one conclusion this verb must never let a caller draw by
                    // accident.
                    Err(e) => {
                        crate::ui::refuse(&format!("could not write the refusal: {e}"));
                        EXIT_FAILED
                    }
                }
            } else {
                for line in r.message.lines() {
                    crate::ui::refuse(line);
                }
                r.exit
            }
        }
        Outcome::Served(s) => {
            // Disclosures go to STDERR in both modes (D8: never into `content`, and never into
            // the stdout product). Under `--json` they also ride the envelope, which is the
            // surface a machine reads.
            for d in s.envelope.disclosures() {
                crate::ui::warn(&d.message);
            }
            if let Some(n) = &s.note {
                crate::ui::detail(n);
            }
            let written = if json {
                match serde_json::to_string_pretty(&s.envelope) {
                    Ok(text) => write_product(&text, true),
                    Err(e) => {
                        crate::ui::refuse(&format!("could not render the envelope: {e}"));
                        return EXIT_FAILED;
                    }
                }
            } else {
                // The body is bytes-as-asked, not a UI line: no prefix, no styling, no
                // `--quiet` suppression.
                write_product(
                    &s.body,
                    needs_closing_newline(&s.body, console::Term::stdout().is_term()),
                )
            };
            match written {
                Ok(()) => 0,
                // The reader hung up. It got what it asked for; so did we.
                Err(e) if broken_pipe(&e) => 0,
                Err(e) => {
                    crate::ui::refuse(&format!("could not write the result: {e}"));
                    EXIT_FAILED
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocliToml;
    use crate::state::NodeState;

    struct Fx {
        _tmp: tempfile::TempDir,
        project: Project,
    }

    fn fx(mounts: &[(&str, u128, Option<&str>)]) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let control_dir = root.join(".docli");
        std::fs::create_dir_all(root.join(".docli")).unwrap();
        let owner = std::fs::canonicalize(root.join(".docli"))
            .unwrap()
            .display()
            .to_string();
        let mut table = Vec::new();
        for (dir, ws, name) in mounts {
            let ws = Uuid::from_u128(*ws);
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(
                root.join(dir).join("MOUNT.docli"),
                serde_json::json!({"owner": owner, "workspace": ws}).to_string(),
            )
            .unwrap();
            table.push(Mount {
                workspace: ws,
                dir: (*dir).to_string(),
                folder: None,
                name: name.map(str::to_string),
                derived_dir: false,
                workspace_label: String::new(),
            });
        }
        Fx {
            _tmp: tmp,
            project: Project {
                root,
                config: DocliToml {
                    server: "https://docli.ru".into(),
                    mounts: table,
                    mcp_label: None,
                },
                control: control_dir.clone(),
            },
        }
    }

    /// A synced mount: write the file, track it in state with the digest it really has.
    fn put_note(f: &Fx, dir: &str, ws: u128, id: u128, server_path: &str, body: &str) {
        put_file(
            f,
            dir,
            ws,
            id,
            server_path,
            server_path,
            body,
            TrackedKind::Note,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn put_file(
        f: &Fx,
        dir: &str,
        ws: u128,
        id: u128,
        server_path: &str,
        local_path: &str,
        body: &str,
        kind: TrackedKind,
        marker_path: Option<&str>,
    ) {
        let ws = Uuid::from_u128(ws);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap_or_else(|| {
            let mut s = WsState::fresh(None);
            s.from_zero = false;
            s.at_head = true;
            s.head_reached_at = Some(1);
            s
        });
        let on_disk = marker_path.unwrap_or(local_path);
        let abs = f.project.root.join(dir).join(on_disk);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, body).unwrap();
        st.nodes.insert(
            Uuid::from_u128(id),
            NodeState {
                server_path: server_path.to_string(),
                local_path: local_path.to_string(),
                kind,
                rev: 1,
                content_sha256: crate::apply::sha_hex(body.as_bytes()),
                marker_path: marker_path.map(str::to_string),
                content_changed_at: Some("2026-09-05T10:00:00.000000Z".to_string()),
            },
        );
        st.ledger.insert(Uuid::from_u128(id));
        control.save_state(ws, &st).unwrap();
    }

    /// Claim `id` the way `search` does. The fixtures store `rev: 1`, so any higher rev is a
    /// server position this mirror has not reached.
    fn mark(f: &Fx, ws: u128, id: u128) {
        claim(f, ws, id, 9);
    }

    fn claim(f: &Fx, ws: u128, id: u128, rev: i64) {
        ControlRoot::new(&f.project.root).merge_marks(
            Uuid::from_u128(ws),
            &std::collections::BTreeMap::from([(Uuid::from_u128(id), rev)]),
        );
    }

    fn args(path: &str) -> ReadArgs {
        ReadArgs {
            path: Some(path.into()),
            id: None,
            mount: None,
            lines: None,
            json: false,
        }
    }

    /// The whole point of the slice: a note the server named is REFUSED, with its own code, and
    /// no body is printed.
    #[test]
    fn a_marked_note_exits_four_and_serves_nothing() {
        let f = fx(&[("m", 1, None)]);
        put_note(&f, "m", 1, 7, "a.md", "old body");
        mark(&f, 1, 7);
        let r = refused(resolve(&f.project, &args("a.md"), 1));
        assert_eq!(r.exit, EXIT_STALE);
        assert_eq!(r.code, "stale");
        // Its own code, distinct from «not in this mirror» — an agent that confused the two would
        // conclude the note does not exist, which only `docli search` may ever establish.
        assert_ne!(r.exit, EXIT_NOT_IN_MIRROR);
        // ONE remedy, and it is the one that both delivers the current bytes and RETIRES the
        // claim — by advancing this node's stored rev past it; the record itself is never removed.
        assert!(r.message.contains("docli sync"), "{}", r.message);
        assert!(
            !r.message.contains("read_note"),
            "enumerating a second path is what 0.1.19 measured as harmful: {}",
            r.message
        );
    }

    /// The PER-NODE claim, which is what separates this from mount-marking: a different note
    /// changing must not cost this one its read.
    #[test]
    fn an_untouched_note_still_reads_while_a_sibling_is_marked() {
        let f = fx(&[("m", 1, None)]);
        put_note(&f, "m", 1, 7, "a.md", "body a");
        put_note(&f, "m", 1, 8, "b.md", "body b");
        mark(&f, 1, 8);
        assert_eq!(served(resolve(&f.project, &args("a.md"), 1)).body, "body a");
        assert_eq!(
            refused(resolve(&f.project, &args("b.md"), 1)).exit,
            EXIT_STALE
        );
    }

    /// No marks at all — no file, nothing searched yet, or a read-only `$HOME` where `search`
    /// could not persist — leaves `read` exactly as v0.29.0 left it: it SERVES and discloses.
    /// This is D5's fallback, and pinning it is what stops the gate drifting into an outage.
    #[test]
    fn an_unmarked_mirror_serves_as_before() {
        let f = fx(&[("m", 1, None)]);
        put_note(&f, "m", 1, 7, "a.md", "body");
        let control = ControlRoot::new(&f.project.root);
        assert!(!control.marks_path(Uuid::from_u128(1)).exists());
        assert_eq!(served(resolve(&f.project, &args("a.md"), 1)).body, "body");
    }

    /// A mark naming an id this mirror does not hold is INERT. It cannot promote an exit 3 into
    /// an exit 4 — «not here» stays the honest answer, and a park keeps its own sentence.
    #[test]
    fn a_mark_for_an_unheld_node_changes_nothing() {
        let f = fx(&[("m", 1, None)]);
        put_note(&f, "m", 1, 7, "a.md", "body");
        mark(&f, 1, 99);
        let r = refused(resolve(&f.project, &args("nope.md"), 1));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert_eq!(r.code, "not_in_mirror");
        assert_eq!(served(resolve(&f.project, &args("a.md"), 1)).body, "body");
    }

    /// A mark must not resolve an AMBIGUITY. Two mounts holding one path stays a `--mount`
    /// refusal even when one copy is marked — otherwise the answer would depend on which mirror
    /// happened to be stale, which nothing in the output reveals.
    #[test]
    fn a_marked_copy_does_not_break_a_tie_between_two_mounts() {
        let f = fx(&[("m1", 1, None), ("m2", 2, None)]);
        put_note(&f, "m1", 1, 7, "a.md", "one");
        put_note(&f, "m2", 2, 8, "a.md", "two");
        mark(&f, 1, 7);
        let r = refused(resolve(&f.project, &args("a.md"), 1));
        assert_eq!(r.code, "ambiguous");
        assert_eq!(r.exit, EXIT_FAILED);
    }

    /// A claim retires when the mirror reaches its rev — nothing removes it.
    ///
    /// This is the property that made the prune, the head-clear and the from-zero delete all
    /// unnecessary, and it is what makes the two snapshot races unreachable: a sync delivering an
    /// OLDER rev leaves the claim standing, and a search naming a rev already applied never fires.
    #[test]
    fn a_claim_retires_when_the_mirror_reaches_its_rev() {
        let f = fx(&[("m", 1, None)]);
        put_note(&f, "m", 1, 7, "a.md", "body"); // stored at rev 1
        claim(&f, 1, 7, 9);
        assert_eq!(
            refused(resolve(&f.project, &args("a.md"), 1)).exit,
            EXIT_STALE
        );
        // A claim at or below what the mirror already holds never fires — the case a search that
        // started before a sync produces, which an id-keyed mark stranded forever.
        let f2 = fx(&[("m", 1, None)]);
        put_note(&f2, "m", 1, 7, "a.md", "body");
        claim(&f2, 1, 7, 1);
        assert_eq!(served(resolve(&f2.project, &args("a.md"), 1)).body, "body");
    }

    fn served(o: Outcome) -> Served {
        match o {
            Outcome::Served(s) => *s,
            Outcome::Refused(r) => panic!("expected a served note, got {r:?}"),
        }
    }

    fn refused(o: Outcome) -> Refusal {
        match o {
            Outcome::Refused(r) => r,
            Outcome::Served(_) => panic!("expected a refusal"),
        }
    }

    fn note(e: &Envelope) -> &NoteEnvelope {
        match e {
            Envelope::Note(n) => n,
            Envelope::File(_) => panic!("expected a note envelope"),
        }
    }

    #[test]
    fn a_mirrored_note_is_served_byte_verbatim() {
        let f = fx(&[("mirror", 1, Some("notes"))]);
        let body = "---\ntitle: A\n---\n\nтекст\n";
        put_note(&f, "mirror", 1, 9, "docs/a.md", body);
        let s = served(resolve(&f.project, &args("docs/a.md"), 100));
        assert_eq!(s.body, body);
        let n = note(&s.envelope);
        assert_eq!(n.content.as_deref(), Some(body));
        assert_eq!(n.path, "docs/a.md");
        assert_eq!(n.name, "a.md");
        assert_eq!(n.mount, "notes");
        assert_eq!(n.id, Uuid::from_u128(9));
        assert_eq!(
            n.lines,
            LineSpan {
                from: 1,
                to: 5,
                total: 5
            }
        );
        assert!(n.disclosures.is_empty(), "{:?}", n.disclosures);
    }

    #[test]
    fn a_leading_slash_or_dot_slash_addresses_the_same_note() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "docs/a.md", "x\n");
        for spelling in ["/docs/a.md", "./docs/a.md", "  docs/a.md  "] {
            let s = served(resolve(&f.project, &args(spelling), 100));
            assert_eq!(s.body, "x\n", "{spelling}");
        }
    }

    #[test]
    fn lines_select_an_inclusive_one_based_range() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "one\ntwo\nthree\nfour\n");
        let mut a = args("a.md");
        a.lines = Some("2-3".into());
        let s = served(resolve(&f.project, &a, 100));
        assert_eq!(s.body, "two\nthree\n");
        assert_eq!(
            note(&s.envelope).lines,
            LineSpan {
                from: 2,
                to: 3,
                total: 4
            }
        );
        // Open-ended, and a bare single line.
        a.lines = Some("3-".into());
        assert_eq!(served(resolve(&f.project, &a, 100)).body, "three\nfour\n");
        a.lines = Some("1".into());
        assert_eq!(served(resolve(&f.project, &a, 100)).body, "one\n");
    }

    #[test]
    fn a_range_past_the_end_refuses_and_names_the_total() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "one\ntwo\n");
        let mut a = args("a.md");
        a.lines = Some("9-10".into());
        let r = refused(resolve(&f.project, &a, 100));
        assert_eq!(r.exit, EXIT_FAILED);
        assert!(r.message.contains("has 2 lines"), "{}", r.message);
        // …and a range that ends before it starts, and a zero.
        a.lines = Some("3-2".into());
        assert!(refused(resolve(&f.project, &a, 100))
            .message
            .contains("ends before it starts"));
        a.lines = Some("0-2".into());
        assert!(refused(resolve(&f.project, &a, 100))
            .message
            .contains("start at 1"));
        // A past-the-end range is NEVER the not-in-mirror code: the note is right here.
        a.lines = Some("9-10".into());
        assert_ne!(
            refused(resolve(&f.project, &a, 100)).exit,
            EXIT_NOT_IN_MIRROR
        );
    }

    #[test]
    fn an_empty_note_serves_as_empty_not_as_a_malformed_range() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "");
        let s = served(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(s.body, "");
        assert_eq!(
            note(&s.envelope).lines,
            LineSpan {
                from: 0,
                to: 0,
                total: 0
            }
        );
    }

    #[test]
    fn a_path_the_mirror_does_not_hold_is_refused_never_rendered_as_absence() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let r = refused(resolve(&f.project, &args("nowhere.md"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert_eq!(r.code, "not_in_mirror");
        // The sentence must not let a reader conclude the note does not exist.
        assert!(r.message.contains("docli search"), "{}", r.message);
        assert!(
            !r.message.contains("does not exist") || r.message.contains("only `docli search`"),
            "{}",
            r.message
        );
    }

    #[test]
    fn a_never_synced_mount_says_so_rather_than_reporting_a_miss() {
        let f = fx(&[("mirror", 1, None)]);
        // GENUINELY never synced: no state AND no claim on the directory. The fixture writes a
        // `MOUNT.docli` for convenience, and with one present the same missing state means
        // something else entirely — see the test below.
        std::fs::remove_file(f.project.root.join("mirror/MOUNT.docli")).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert!(r.message.contains("never been synced"), "{}", r.message);
    }

    /// A missing state file over a CLAIMED mirror is not «this mount holds nothing» — the mirror
    /// may hold the note perfectly well and the RECORD of what it holds is what was lost.
    /// `doctor` already has the name for it (`state-missing`); reporting it as never-synced
    /// would put an answered exit 3 on a mount nobody looked inside (Codex round 7).
    #[test]
    fn a_lost_state_file_over_a_claimed_mirror_is_not_never_synced() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let control = ControlRoot::new(&f.project.root);
        std::fs::remove_file(control.state_path(Uuid::from_u128(1))).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_FAILED);
        assert_eq!(r.code, "unavailable");
        assert!(r.message.contains("state-missing"), "{}", r.message);
        assert!(!r.message.contains("never been synced"), "{}", r.message);
    }

    #[test]
    fn a_parked_node_reports_its_park_and_the_check_happens_before_the_nodes_miss() {
        // The ordering IS the test (open item 1): a parked node is absent from `state.nodes` by
        // construction, so a nodes-first lookup makes this case unreachable.
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.parks.insert(
            Uuid::from_u128(77),
            Park {
                class: ParkClass::Transient,
                reason: "an untracked file occupies its path".into(),
                server_path: "parked.md".into(),
            },
        );
        control.save_state(ws, &st).unwrap();
        let r = refused(resolve(&f.project, &args("parked.md"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        // The park's OWN reason has to surface — that is what makes this reachable at all, and
        // a nodes-first lookup would have replaced it with the generic «does not hold it».
        assert!(r.message.contains("untracked file"), "{}", r.message);
        assert!(r.message.contains("sync --check"), "{}", r.message);
    }

    #[test]
    fn a_structural_park_names_no_remedy_that_cannot_work() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.parks.insert(
            Uuid::from_u128(77),
            Park {
                class: ParkClass::Structural,
                reason: "not representable on this filesystem".into(),
                server_path: "aux.md".into(),
            },
        );
        control.save_state(ws, &st).unwrap();
        let r = refused(resolve(&f.project, &args("aux.md"), 100));
        assert!(!r.message.contains("docli sync"), "{}", r.message);
        assert!(r.message.contains("MCP"), "{}", r.message);
    }

    #[test]
    fn a_ledger_only_id_is_distinguished_from_one_that_was_never_delivered() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.ledger.insert(Uuid::from_u128(42));
        control.save_state(ws, &st).unwrap();
        let by_id = |id: u128| ReadArgs {
            path: None,
            id: Some(Uuid::from_u128(id)),
            mount: None,
            lines: None,
            json: false,
        };
        let known = refused(resolve(&f.project, &by_id(42), 100));
        assert!(
            known.message.contains("does not mirror it"),
            "{}",
            known.message
        );
        let unknown = refused(resolve(&f.project, &by_id(43), 100));
        assert!(
            unknown.message.contains("does not hold it"),
            "{}",
            unknown.message
        );
        // Both are the same class of answer: not-in-mirror, never absence.
        assert_eq!(known.exit, EXIT_NOT_IN_MIRROR);
        assert_eq!(unknown.exit, EXIT_NOT_IN_MIRROR);
    }

    #[test]
    fn an_id_addresses_the_same_note_as_its_path() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "docs/a.md", "body\n");
        let s = served(resolve(
            &f.project,
            &ReadArgs {
                path: None,
                id: Some(Uuid::from_u128(9)),
                mount: None,
                lines: None,
                json: false,
            },
            100,
        ));
        assert_eq!(s.body, "body\n");
        assert_eq!(note(&s.envelope).path, "docs/a.md");
    }

    #[test]
    fn a_digest_mismatch_discloses_and_still_serves() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "original\n");
        std::fs::write(f.project.root.join("mirror/a.md"), "edited\n").unwrap();
        let s = served(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(
            s.body, "edited\n",
            "the answer IS the mirror - serve what is held"
        );
        let d = &note(&s.envelope).disclosures;
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].code, "digest_mismatch");
        // It describes what it OBSERVED and asserts no cause — a concurrent sync produces the
        // same observation as innocently as a hand edit does — so it states the CONSEQUENCE and
        // names no culprit.
        assert!(
            d[0].message
                .contains("may differ from what the server holds"),
            "{}",
            d[0].message
        );
        for culprit in ["edited", "changed in place", "by hand"] {
            assert!(
                !d[0].message.contains(culprit),
                "names a cause it never observed: {}",
                d[0].message
            );
        }
    }

    #[test]
    fn an_unusable_mirror_still_serves_and_discloses() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.at_head = false;
        control.save_state(ws, &st).unwrap();
        let s = served(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(s.body, "x\n");
        let d = &note(&s.envelope).disclosures;
        assert!(d.iter().any(|d| d.code == "mirror_not_usable"), "{d:?}");
    }

    #[test]
    fn a_missing_file_is_reported_as_gone_not_as_absence() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        std::fs::remove_file(f.project.root.join("mirror/a.md")).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert!(r.message.contains("tracks it"), "{}", r.message);
    }

    /// A fresh `docli init` has written `docli.toml` and nothing else — only `sync`/`doctor`
    /// create the mount directory (`claim_mount`). So the most common first-contact state has
    /// NO mount dir and NO state, and it must answer «run `docli sync`», not «this directory is
    /// not this workspace's mirror - `docli init` re-points it», which names the command just
    /// run. That is why state is read before identity is judged.
    #[test]
    fn a_freshly_initialised_mount_says_run_sync_not_re_point_it() {
        let f = fx(&[("mirror", 1, None)]);
        std::fs::remove_dir_all(f.project.root.join("mirror")).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert!(r.message.contains("never been synced"), "{}", r.message);
        assert!(!r.message.contains("docli init"), "{}", r.message);
    }

    /// …and the same state with a state file present IS the re-point case: the mirror was synced
    /// once and the directory has since stopped being it.
    /// The three classifications must agree. A variant that reports «we could not look»
    /// (`!answered`) while exiting 3 / `not_in_mirror` claims both halves of a contradiction —
    /// and the half that escapes is the one an agent turns into «the note does not exist»
    /// (Codex round 6).
    #[test]
    fn a_mount_that_could_not_be_consulted_never_codes_as_not_in_mirror() {
        let unanswered = [
            Miss::StateUnreadable("x".into()),
            Miss::StateLost,
            Miss::NotThisMirror,
            Miss::Unreadable("x".into()),
        ];
        for m in &unanswered {
            assert!(!m.answered(), "{}", m.sentence());
            assert_eq!(m.exit(), EXIT_FAILED, "{}", m.sentence());
            assert_eq!(m.code(), "unavailable", "{}", m.sentence());
        }
        // …and the converse: everything that DID answer keeps the not-in-mirror code, except the
        // one usage mistake.
        for m in [
            Miss::NeverSynced,
            Miss::NotHeld,
            Miss::LedgerOnly,
            Miss::Gone("x".into()),
            Miss::Escaped,
            Miss::Parked(ParkClass::Transient, "x".into()),
        ] {
            assert!(m.answered(), "{}", m.sentence());
            assert_eq!(m.exit(), EXIT_NOT_IN_MIRROR, "{}", m.sentence());
            assert_eq!(m.code(), "not_in_mirror", "{}", m.sentence());
        }
        assert!(Miss::Folder.answered());
        assert_eq!(Miss::Folder.code(), "usage");
    }

    #[test]
    fn a_directory_that_is_not_this_workspaces_mirror_refuses_and_names_docli_init() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        std::fs::remove_file(f.project.root.join("mirror/MOUNT.docli")).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert!(r.message.contains("docli init"), "{}", r.message);
    }

    #[test]
    fn a_symlink_planted_in_the_mirror_never_serves_a_file_outside_it() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let outside = f.project.root.join("outside.txt");
        std::fs::write(&outside, "secret\n").unwrap();
        let target = f.project.root.join("mirror/a.md");
        std::fs::remove_file(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &target).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, &target).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert!(r.message.contains("outside the mirror"), "{}", r.message);
        // It EXISTS — it is just not ours. The sentence must not also claim the file vanished;
        // an earlier draft composed exactly that contradiction in one breath.
        assert!(!r.message.contains("nothing is there"), "{}", r.message);
    }

    /// Exit 2 covers two unrelated things, and a machine branching on `code` has to tell them
    /// apart: a folder addressed as a note is the caller's mistake, a state file that will not
    /// read is ours.
    #[test]
    fn the_two_exit_two_cases_carry_different_machine_codes() {
        let f = fx(&[("mirror", 1, None)]);
        let control = ControlRoot::new(&f.project.root);
        let p = control.state_path(Uuid::from_u128(1));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(
            refused(resolve(&f.project, &args("a.md"), 100)).code,
            "unavailable"
        );
    }

    #[test]
    fn a_folder_is_refused_as_a_usage_error_never_as_a_missing_note() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        std::fs::create_dir_all(f.project.root.join("mirror/docs")).unwrap();
        st.nodes.insert(
            Uuid::from_u128(10),
            NodeState {
                server_path: "docs".into(),
                local_path: "docs".into(),
                kind: TrackedKind::Folder,
                rev: 1,
                content_sha256: String::new(),
                marker_path: None,
                content_changed_at: None,
            },
        );
        control.save_state(ws, &st).unwrap();
        let r = refused(resolve(&f.project, &args("docs"), 100));
        assert_eq!(r.exit, EXIT_FAILED);
        assert_eq!(
            r.code, "usage",
            "a folder is the caller's mistake, not our outage"
        );
        assert!(r.message.contains("folder"), "{}", r.message);
    }

    #[test]
    fn an_attachment_prints_the_marker_fields_the_mirror_path_used_to_expose() {
        let f = fx(&[("mirror", 1, None)]);
        let marker = "id 00000000-0000-0000-0000-000000000007\nmime image/png\n\
                      path a/pic.png\nsha256 abc123\nsize 4096\nwikilink ![[a/pic.png]]\n";
        put_file(
            &f,
            "mirror",
            1,
            7,
            "a/pic.png",
            "a/pic.png",
            marker,
            TrackedKind::Attachment,
            Some("a/pic.png.docli"),
        );
        let s = served(resolve(&f.project, &args("a/pic.png"), 100));
        assert_eq!(s.body, marker);
        let Envelope::File(fe) = &s.envelope else {
            panic!("expected a file envelope");
        };
        assert_eq!(fe.kind, "attachment");
        assert_eq!(fe.mime.as_deref(), Some("image/png"));
        assert_eq!(fe.bytes, Some(4096));
        assert_eq!(fe.sha256.as_deref(), Some("abc123"));
        assert_eq!(fe.wikilink.as_deref(), Some("![[a/pic.png]]"));
        assert_eq!(fe.content, None);
        assert!(fe.absent.contains_key("content"), "{:?}", fe.absent);
    }

    #[test]
    fn the_markers_two_declared_unknowns_stay_declared() {
        let f = fx(&[("mirror", 1, None)]);
        let marker = "id 00000000-0000-0000-0000-000000000007\nmime unknown\n\
                      path a/p#1.png\nsha256 unknown\nsize 9\nwikilink not-expressible\n";
        put_file(
            &f,
            "mirror",
            1,
            7,
            "a/p#1.png",
            "a/p#1.png",
            marker,
            TrackedKind::Attachment,
            Some("a/p#1.png.docli"),
        );
        let s = served(resolve(&f.project, &args("a/p#1.png"), 100));
        let Envelope::File(fe) = &s.envelope else {
            panic!("expected a file envelope");
        };
        assert_eq!(fe.sha256, None);
        assert_eq!(fe.wikilink, None);
        assert_eq!(fe.mime, None);
        for k in ["sha256", "wikilink", "mime"] {
            assert!(
                fe.absent.contains_key(k),
                "{k} must be NAMED absent: {:?}",
                fe.absent
            );
        }
        // …and the sidecar itself rides along, so nothing our parse skipped is lost.
        assert_eq!(fe.marker, marker);
    }

    #[test]
    fn a_relocated_marker_resolves_through_state_under_the_control_root() {
        let f = fx(&[("mirror", 1, None)]);
        let ws = Uuid::from_u128(1);
        let id = Uuid::from_u128(7);
        let marker = "id x\nmime image/png\npath MOUNT\nsha256 unknown\nsize 3\n\
                      wikilink ![[MOUNT]]\n";
        let rel = format!(".docli/markers/{ws}/{id}.docli");
        let abs = f.project.root.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, marker).unwrap();
        let control = ControlRoot::new(&f.project.root);
        let mut st = WsState::fresh(None);
        st.from_zero = false;
        st.at_head = true;
        st.head_reached_at = Some(1);
        st.nodes.insert(
            id,
            NodeState {
                server_path: "MOUNT".into(),
                local_path: "MOUNT".into(),
                kind: TrackedKind::Attachment,
                rev: 1,
                content_sha256: crate::apply::sha_hex(marker.as_bytes()),
                marker_path: Some(rel),
                content_changed_at: None,
            },
        );
        control.save_state(ws, &st).unwrap();
        let s = served(resolve(&f.project, &args("MOUNT"), 100));
        assert_eq!(s.body, marker);
    }

    #[test]
    fn a_sibling_workspaces_relocated_marker_never_resolves() {
        let f = fx(&[("mirror", 1, None)]);
        let ws = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        let id = Uuid::from_u128(7);
        let rel = format!(".docli/markers/{other}/{id}.docli");
        let abs = f.project.root.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "id x\n").unwrap();
        let control = ControlRoot::new(&f.project.root);
        let mut st = WsState::fresh(None);
        st.from_zero = false;
        st.at_head = true;
        st.head_reached_at = Some(1);
        st.nodes.insert(
            id,
            NodeState {
                server_path: "MOUNT".into(),
                local_path: "MOUNT".into(),
                kind: TrackedKind::Attachment,
                rev: 1,
                content_sha256: String::new(),
                marker_path: Some(rel),
                content_changed_at: None,
            },
        );
        control.save_state(ws, &st).unwrap();
        let r = refused(resolve(&f.project, &args("MOUNT"), 100));
        // It must not SERVE, which is the security property. And it is exit 2, not 3: we refused
        // to resolve a path that escapes this workspace's namespace, so «this mirror does not
        // hold it» would be a verdict we never earned — we declined to look.
        assert_eq!(r.exit, EXIT_FAILED);
        assert_eq!(r.code, "unavailable");
    }

    #[test]
    fn an_ambiguous_bare_path_refuses_rather_than_picking() {
        let f = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&f, "m1", 1, 9, "a.md", "from one\n");
        put_note(&f, "m2", 2, 10, "a.md", "from two\n");
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_FAILED);
        assert_eq!(r.code, "ambiguous");
        assert!(r.message.contains("--mount"), "{}", r.message);
        assert!(r.message.contains("one"), "{}", r.message);
        assert!(r.message.contains("two"), "{}", r.message);
        // …and naming one resolves it. Both tokens `search` can print must work.
        let mut a = args("a.md");
        a.mount = Some("two".into());
        assert_eq!(served(resolve(&f.project, &a, 100)).body, "from two\n");
        a.mount = Some(Uuid::from_u128(1).to_string());
        assert_eq!(served(resolve(&f.project, &a, 100)).body, "from one\n");
    }

    #[test]
    fn a_duplicate_mount_name_refuses_and_offers_the_workspace_ids() {
        // `validate_config` enforces uniqueness on `workspace` only, so display names CAN
        // collide (open item 3) — resolving one by position would make the answer depend on
        // the order of `docli.toml`.
        let f = fx(&[("m1", 1, Some("notes")), ("m2", 2, Some("notes"))]);
        put_note(&f, "m1", 1, 9, "a.md", "one\n");
        let mut a = args("a.md");
        a.mount = Some("notes".into());
        let r = refused(resolve(&f.project, &a, 100));
        assert_eq!(r.code, "ambiguous_mount");
        assert!(
            r.message.contains(&Uuid::from_u128(1).to_string()),
            "{}",
            r.message
        );
        assert!(
            r.message.contains(&Uuid::from_u128(2).to_string()),
            "{}",
            r.message
        );
    }

    #[test]
    fn an_unknown_mount_selector_refuses_without_searching() {
        let f = fx(&[("mirror", 1, Some("notes"))]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let mut a = args("a.md");
        a.mount = Some("elsewhere".into());
        let r = refused(resolve(&f.project, &a, 100));
        assert_eq!(r.code, "no_such_mount");
    }

    #[test]
    fn several_mounts_each_report_their_own_reason() {
        let f = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&f, "m1", 1, 9, "a.md", "x\n");
        // m2 was never synced (no state, no claim); m1 simply does not hold the path.
        std::fs::remove_file(f.project.root.join("m2/MOUNT.docli")).unwrap();
        let r = refused(resolve(&f.project, &args("b.md"), 100));
        assert!(r.message.contains("[one]"), "{}", r.message);
        assert!(r.message.contains("[two]"), "{}", r.message);
        assert!(r.message.contains("never been synced"), "{}", r.message);
    }

    #[test]
    fn a_state_file_that_will_not_read_is_a_failure_to_answer_not_an_answer() {
        let f = fx(&[("mirror", 1, None)]);
        let control = ControlRoot::new(&f.project.root);
        let p = control.state_path(Uuid::from_u128(1));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.exit, EXIT_FAILED, "never the not-in-mirror code");
        assert!(r.message.contains("could not be read"), "{}", r.message);
    }

    /// The `absent` invariant, asserted over ANY envelope — `Envelope` is `#[serde(untagged)]`,
    /// so a nullable field added to either arm has to be caught by the same rule or it escapes
    /// silently.
    fn assert_absent_is_total(e: &Envelope) -> serde_json::Value {
        let value = serde_json::to_value(e).unwrap();
        let obj = value.as_object().unwrap();
        let absent = obj["absent"].as_object().unwrap();
        for (k, v) in obj {
            if v.is_null() {
                assert!(
                    absent.contains_key(k),
                    "`{k}` is null but not named in `absent`: {value:#}"
                );
            }
        }
        value.clone()
    }

    #[test]
    fn the_absent_invariant_is_total_over_the_file_envelope_too() {
        let f = fx(&[("mirror", 1, None)]);
        // Every optional field unfillable at once: no mime, no digest, no wikilink, no size.
        let marker = "id x\npath a.png\nsha256 unknown\nwikilink not-expressible\n";
        put_file(
            &f,
            "mirror",
            1,
            7,
            "a.png",
            "a.png",
            marker,
            TrackedKind::Attachment,
            Some("a.png.docli"),
        );
        let s = served(resolve(&f.project, &args("a.png"), 100));
        let v = assert_absent_is_total(&s.envelope);
        // …and `content` is the one that is null BY DESIGN rather than by absence of data.
        assert!(v["content"].is_null());
        assert!(v["absent"]["content"]
            .as_str()
            .unwrap()
            .contains("read_attachment"));
    }

    #[test]
    fn every_null_field_in_the_envelope_is_named_in_absent() {
        // The invariant D8 rests on: a null with no entry beside it is a SILENT absence, and a
        // silent absence in `backlinks` is D5's false negative inside the verb built to replace
        // grep.
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let s = served(resolve(&f.project, &args("a.md"), 100));
        let value = assert_absent_is_total(&s.envelope);
        let obj = value.as_object().unwrap();
        let absent = obj["absent"].as_object().unwrap();
        // …and the graph fields are absent, never EMPTY.
        for k in [
            "title",
            "aliases",
            "links",
            "unresolved",
            "embeds",
            "backlinks",
            "tags",
        ] {
            assert!(obj[k].is_null(), "`{k}` must be null, never []: {value:#}");
            assert!(absent.contains_key(k), "`{k}` must be named absent");
        }
        // A mirror synced before the graph existed gets the remedy that WILL work — which is
        // what Half 2 changed: in Half 1 no api served a graph, so naming `docli sync` would
        // have been an instruction that could not succeed.
        assert!(
            absent["backlinks"].as_str().unwrap().contains("docli sync"),
            "a never-asked mirror must name the sync that would fetch a graph: {value:#}"
        );
        // `frontmatter` and `relatedHint` are DECLARED absent, never silently missing (D2).
        for k in ["frontmatter", "relatedHint"] {
            assert!(obj.contains_key(k), "`{k}` must be present as a key");
            assert!(absent.contains_key(k), "`{k}` must be named absent");
        }
    }

    /// Seed a graph cache for `ws`, stamped at whatever position its state currently holds — the
    /// only stamp `graph_slot` will accept.
    fn put_graph(f: &Fx, ws: u128, graph: docli_sync_wire::WireGraph) {
        let ws = Uuid::from_u128(ws);
        let control = ControlRoot::new(&f.project.root);
        let st = control.load_state(ws).unwrap().unwrap();
        control.save_graph(ws, st.epoch, st.cursor, &graph).unwrap();
    }

    fn gnode(id: u128, kind: &str, path: &str, title: Option<&str>) -> docli_sync_wire::GraphNode {
        docli_sync_wire::GraphNode {
            id: Uuid::from_u128(id),
            kind: kind.into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            title: title.map(Into::into),
            aliases: vec![],
            mime: (kind == "attachment").then(|| "image/png".to_string()),
            content_bytes: 4,
            trashed: false,
        }
    }

    /// The Half-2 payoff: with the graph held, the five predicates render as REAL answers — and
    /// an empty one renders as `[]`, which is now a statement rather than a silence.
    #[test]
    fn a_held_graph_fills_the_envelope_and_empty_means_empty() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        put_note(&f, "mirror", 1, 10, "b.md", "y\n");
        put_graph(
            &f,
            1,
            docli_sync_wire::WireGraph {
                nodes: vec![
                    gnode(9, "file", "a.md", Some("Alpha")),
                    gnode(10, "file", "b.md", None),
                    gnode(11, "attachment", "p.png", None),
                ],
                edges: vec![
                    docli_sync_wire::GraphEdge {
                        src: 0,
                        dst: Some(1),
                        att: None,
                        dst_ref: "b".into(),
                        kind: "wikilink".into(),
                        anchor: None,
                    },
                    docli_sync_wire::GraphEdge {
                        src: 0,
                        dst: None,
                        att: Some(2),
                        dst_ref: "p.png".into(),
                        kind: "embed".into(),
                        anchor: None,
                    },
                    docli_sync_wire::GraphEdge {
                        src: 0,
                        dst: None,
                        att: None,
                        dst_ref: "nowhere".into(),
                        kind: "wikilink".into(),
                        anchor: None,
                    },
                ],
                tags: vec![docli_sync_wire::GraphTag {
                    node: 0,
                    tag: "work".into(),
                }],
            },
        );

        let s = served(resolve(&f.project, &args("a.md"), 100));
        let n = note(&s.envelope);
        assert_eq!(n.title.as_deref(), Some("Alpha"));
        assert_eq!(n.links.as_ref().unwrap().len(), 1);
        assert_eq!(n.embeds.as_ref().unwrap().len(), 1);
        assert_eq!(n.unresolved.as_deref(), Some(&["nowhere".to_string()][..]));
        assert_eq!(n.tags.as_deref(), Some(&["work".to_string()][..]));
        // Nothing links TO a.md — and that is now `[]` with no `absent` entry, which is the
        // whole difference between Half 1 and Half 2.
        assert_eq!(n.backlinks.as_ref().unwrap().len(), 0);
        assert!(!n.absent.contains_key("backlinks"), "{:?}", n.absent);
        // The counts ride stderr, never stdout: stdout is the note.
        assert_eq!(s.body, "x\n");
        assert!(s.note.as_deref().unwrap().contains("backlinks 0"));
        assert!(s.note.as_deref().unwrap().contains("tags work"));

        // b.md declares no title — knowledge, not a gap, but still a null, so it still says why.
        let s = served(resolve(&f.project, &args("b.md"), 100));
        let n = note(&s.envelope);
        assert!(n.title.is_none());
        assert!(n.absent["title"].contains("declares no"), "{:?}", n.absent);
        assert_eq!(n.backlinks.as_ref().unwrap().len(), 1);
        assert_absent_is_total(&s.envelope);
    }

    /// The graph is workspace-wide and a mount can be a folder of it, so a held graph over a
    /// scoped mount names paths this mirror does not hold. Disclosed, not discovered.
    #[test]
    fn a_scoped_mount_discloses_that_the_graph_is_wider_than_it_is() {
        let mut f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "docs/a.md", "x\n");
        put_graph(&f, 1, docli_sync_wire::WireGraph::default());
        // Unscoped: no such caveat — the graph and the mount cover the same ground.
        let s = served(resolve(&f.project, &args("docs/a.md"), 100));
        assert!(!s
            .envelope
            .disclosures()
            .iter()
            .any(|d| d.code == "graph_wider_than_mount"));

        f.project.config.mounts[0].folder = Some("docs".into());
        let s = served(resolve(&f.project, &args("docs/a.md"), 100));
        assert!(
            s.envelope
                .disclosures()
                .iter()
                .any(|d| d.code == "graph_wider_than_mount"),
            "{:?}",
            s.envelope.disclosures()
        );
    }

    /// The three absences are three different sentences, and picking the wrong one sends the
    /// reader to a command that cannot help.
    #[test]
    fn the_three_graph_absences_are_told_apart() {
        // 1. Never asked — a mirror synced by a pre-graph CLI. `docli sync` is the fix.
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let s = served(resolve(&f.project, &args("a.md"), 100));
        let n = note(&s.envelope);
        assert!(n.absent["links"].contains("before the note graph existed"));

        // 2. Asked and got none — the server predates the payload. Sending the reader to `docli
        // sync` here would loop them forever, so the sentence names the MCP connection instead.
        let control = ControlRoot::new(&f.project.root);
        let ws = Uuid::from_u128(1);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.graph_asked = true;
        control.save_state(ws, &st).unwrap();
        let s = served(resolve(&f.project, &args("a.md"), 100));
        let n = note(&s.envelope);
        assert!(
            n.absent["links"].contains("serves no note graph"),
            "{:?}",
            n.absent
        );

        // 3. Held but STALE — the cache exists and its stamp does not match. It must never be
        // served: a graph describing an earlier shape of this mirror is exactly the pairing the
        // stamp exists to refuse.
        put_graph(&f, 1, docli_sync_wire::WireGraph::default());
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.cursor.rev += 1;
        control.save_state(ws, &st).unwrap();
        let s = served(resolve(&f.project, &args("a.md"), 100));
        let n = note(&s.envelope);
        assert!(n.links.is_none());
        assert!(n.absent["links"].contains("earlier sync"), "{:?}", n.absent);
    }

    /// A pending from-zero rebuild leaves the cursor put while the mirror is rewritten under it,
    /// so a MATCHING stamp is not enough — the graph must stand down until the rebuild lands.
    #[test]
    fn a_pending_rebuild_withholds_the_graph_even_though_the_stamp_matches() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        put_graph(&f, 1, docli_sync_wire::WireGraph::default());
        // Held, to prove the stamp is good before the rebuild flag is what changes the answer.
        let s = served(resolve(&f.project, &args("a.md"), 100));
        assert!(note(&s.envelope).backlinks.is_some());

        let control = ControlRoot::new(&f.project.root);
        let ws = Uuid::from_u128(1);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.from_zero = true;
        control.save_state(ws, &st).unwrap();
        assert!(
            control.load_graph(ws, st.epoch, st.cursor).is_some(),
            "the cache is still stamped for this position - the flag is doing the work"
        );

        let s = served(resolve(&f.project, &args("a.md"), 100));
        let n = note(&s.envelope);
        assert!(n.backlinks.is_none(), "absent, never an empty list");
        assert!(
            n.absent["backlinks"].contains("full rebuild"),
            "{:?}",
            n.absent
        );
    }

    /// Every sentence `read` puts in front of a reader is checked for the collapsed
    /// string-continuation defect — a Rust `\`-continued literal whose backslash was eaten before
    /// it reached the file leaves a run of alignment spaces mid-sentence. One shipped that way in
    /// this slice, and no test noticed, because the disclosure tests all match on `code`.
    #[test]
    fn no_reader_facing_sentence_carries_collapsed_whitespace() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        put_graph(&f, 1, docli_sync_wire::WireGraph::default());
        let s = served(resolve(&f.project, &args("a.md"), 100));
        let v = assert_absent_is_total(&s.envelope);
        let mut sentences: Vec<String> = Vec::new();
        for (_, m) in v["absent"].as_object().unwrap() {
            sentences.push(m.as_str().unwrap().to_string());
        }
        for d in s.envelope.disclosures() {
            sentences.push(d.message.clone());
        }
        // …plus the constants a served envelope cannot show all of at once.
        for c in [
            GRAPH_NOT_SYNCED,
            GRAPH_NOT_SERVED,
            GRAPH_STALE,
            GRAPH_REBUILDING,
            NO_TITLE,
            FRONTMATTER_ABSENT,
            RELATED_ABSENT,
            ATTACHMENT_BYTES_ABSENT,
            SCOPE_DISCLOSURE,
            STALE_REFUSAL,
        ] {
            sentences.push(c.to_string());
        }
        for m in sentences {
            assert!(
                !m.contains("   "),
                "a collapsed line continuation left a gap in: {m:?}"
            );
        }
    }

    /// The fifth predicate's own surface: a file says which notes embed it.
    #[test]
    fn a_file_reports_the_notes_that_embed_it() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        put_file(
            &f,
            "mirror",
            1,
            11,
            "p.png",
            "p.png",
            "id 00000000-0000-0000-0000-00000000000b\nmime image/png\nsize 4\n",
            TrackedKind::Attachment,
            Some("p.png.docli"),
        );
        put_graph(
            &f,
            1,
            docli_sync_wire::WireGraph {
                nodes: vec![
                    gnode(9, "file", "a.md", None),
                    gnode(11, "attachment", "p.png", None),
                ],
                edges: vec![docli_sync_wire::GraphEdge {
                    src: 0,
                    dst: None,
                    att: Some(1),
                    dst_ref: "p.png".into(),
                    kind: "embed".into(),
                    anchor: None,
                }],
                tags: vec![],
            },
        );
        let s = served(resolve(&f.project, &args("p.png"), 100));
        let v = assert_absent_is_total(&s.envelope);
        assert_eq!(v["embeddedIn"][0]["path"], "a.md");
        assert!(s.note.as_deref().unwrap().contains("embedded in 1 note"));
    }

    #[test]
    fn the_envelope_publishes_no_local_mirror_path() {
        // D1's spine, asserted on the new verb too: `read` opens the file so the caller does not
        // have to, and it must not hand the address back out on the way. Run against BOTH mount
        // shapes — the SECOND is what ships, since `docli init` writes no `name` and
        // `display_name()` then falls back to the directory.
        for name in [Some("notes"), None] {
            let f = fx(&[("mirror", 1, name)]);
            put_note(&f, "mirror", 1, 9, "docs/a.md", "x\n");
            let s = served(resolve(&f.project, &args("docs/a.md"), 100));
            let text = serde_json::to_string(&s.envelope).unwrap();
            assert!(!text.contains("mirror/docs/a.md"), "{name:?} {text}");
            assert!(!text.contains("localPath"), "{name:?} {text}");
            // …and the honest limit, asserted so the documents cannot drift into claiming the
            // address is unobtainable: with no `name` the `mount` field IS the directory. The
            // mirror's LOCATION is public by design (D1 is affordance removal, not
            // impossibility); the per-note HANDOUT is what this slice retires.
            let mount = &serde_json::from_str::<serde_json::Value>(&text).unwrap()["mount"];
            assert_eq!(mount.as_str(), Some(name.unwrap_or("mirror")));
        }
    }

    /// A redirect is the file; a terminal gets a tidy prompt. `render` writes to process stdout
    /// and cannot be asserted, so this is where the byte-exactness claim is actually pinned.
    #[test]
    fn a_redirect_never_gains_a_byte_the_mirror_does_not_hold() {
        assert!(!needs_closing_newline("no trailing newline", false));
        assert!(needs_closing_newline("no trailing newline", true));
        // Already terminated, or nothing to terminate: never, on either stream.
        for is_term in [true, false] {
            assert!(!needs_closing_newline("ends with one\n", is_term));
            assert!(!needs_closing_newline("", is_term));
        }
    }

    /// Every token the ambiguity refusal prints must be one `--mount` accepts, or the remedy
    /// leads to a second refusal. A display name shared with ANY other mount — a hit or not — is
    /// refused by `select_mounts`, so the id is offered instead.
    #[test]
    fn the_ambiguity_refusal_offers_only_tokens_that_resolve() {
        let f = fx(&[("m1", 1, Some("notes")), ("m2", 2, Some("notes"))]);
        put_note(&f, "m1", 1, 9, "a.md", "one\n");
        put_note(&f, "m2", 2, 10, "a.md", "two\n");
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(r.code, "ambiguous");
        // The shared name is useless here, so neither hit is offered it.
        assert!(!r.message.contains("notes"), "{}", r.message);
        for ws in [Uuid::from_u128(1), Uuid::from_u128(2)] {
            assert!(r.message.contains(&ws.to_string()), "{}", r.message);
            // …and each offered token really resolves.
            let mut a = args("a.md");
            a.mount = Some(ws.to_string());
            served(resolve(&f.project, &a, 100));
        }
        // With distinct names, the readable token is the one offered — and it resolves too.
        let g = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&g, "m1", 1, 9, "a.md", "one\n");
        put_note(&g, "m2", 2, 10, "a.md", "two\n");
        let r = refused(resolve(&g.project, &args("a.md"), 100));
        for name in ["one", "two"] {
            assert!(r.message.contains(name), "{}", r.message);
            let mut a = args("a.md");
            a.mount = Some(name.into());
            served(resolve(&g.project, &a, 100));
        }
    }

    /// The headline code must not depend on the order of `docli.toml` — the same objection the
    /// ambiguity refusal raises against picking a hit by position.
    #[test]
    fn the_aggregate_code_is_independent_of_mount_order() {
        for order in [[("m1", 1u128), ("m2", 2u128)], [("m2", 2), ("m1", 1)]] {
            let f = fx(&[
                (order[0].0, order[0].1, None),
                (order[1].0, order[1].1, None),
            ]);
            let control = ControlRoot::new(&f.project.root);
            // One mount holds a FOLDER at that path (a usage mistake); the other's state will
            // not parse (our outage). The outage is the honest headline either way.
            let mut st = WsState::fresh(None);
            st.from_zero = false;
            st.at_head = true;
            st.head_reached_at = Some(1);
            std::fs::create_dir_all(f.project.root.join("m1/docs")).unwrap();
            st.nodes.insert(
                Uuid::from_u128(10),
                NodeState {
                    server_path: "docs".into(),
                    local_path: "docs".into(),
                    kind: TrackedKind::Folder,
                    rev: 1,
                    content_sha256: String::new(),
                    marker_path: None,
                    content_changed_at: None,
                },
            );
            control.save_state(Uuid::from_u128(1), &st).unwrap();
            let p = control.state_path(Uuid::from_u128(2));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "{ not json").unwrap();
            let r = refused(resolve(&f.project, &args("docs"), 100));
            assert_eq!(r.code, "unavailable", "order {order:?}: {}", r.message);
            // …and neither mount's own sentence is lost to the headline.
            assert!(r.message.contains("folder"), "{}", r.message);
            assert!(r.message.contains("could not be read"), "{}", r.message);
        }
    }

    /// `.docli/state` is untrusted, and only a FOLDER is written with an empty digest — which
    /// `probe` refuses before `serve`. So an empty one here means the check CANNOT be made, and
    /// skipping it silently would leave the one file whose provenance is unknown as the one
    /// served with no caveat (Codex round 1).
    #[test]
    fn an_absent_digest_discloses_rather_than_passing_silently() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        st.nodes
            .get_mut(&Uuid::from_u128(9))
            .unwrap()
            .content_sha256 = String::new();
        control.save_state(ws, &st).unwrap();
        let s = served(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(s.body, "x\n", "it still serves what it holds");
        let d = &note(&s.envelope).disclosures;
        assert!(d.iter().any(|d| d.code == "digest_unknown"), "{d:?}");
        // …and it is NOT reported as a mismatch, which would assert something we did not observe.
        assert!(!d.iter().any(|d| d.code == "digest_mismatch"), "{d:?}");
    }

    /// A sibling mount that could not be consulted leaves «this is the only copy» UNVERIFIED.
    /// Serving is right; claiming uniqueness silently is not (Codex round 1).
    #[test]
    fn an_unconsultable_sibling_mount_is_disclosed_not_ignored() {
        let f = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&f, "m1", 1, 9, "a.md", "from one\n");
        let control = ControlRoot::new(&f.project.root);
        let p = control.state_path(Uuid::from_u128(2));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        let s = served(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(s.body, "from one\n");
        let d = s.envelope.disclosures();
        let found = d
            .iter()
            .find(|d| d.code == "mounts_unresolved")
            .unwrap_or_else(|| panic!("{d:?}"));
        assert!(found.message.contains("two"), "{}", found.message);

        // …but a sibling that genuinely ANSWERED «not here» is not a caveat: it established
        // exactly what the disclosure exists to doubt.
        let g = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&g, "m1", 1, 9, "a.md", "from one\n");
        put_note(&g, "m2", 2, 10, "b.md", "other\n");
        let s = served(resolve(&g.project, &args("a.md"), 100));
        assert!(
            !s.envelope
                .disclosures()
                .iter()
                .any(|d| d.code == "mounts_unresolved"),
            "{:?}",
            s.envelope.disclosures()
        );
    }

    /// A mount NAME is free text from an untrusted `docli.toml`, so nothing stops one mount
    /// being named with another mount's workspace id — and then that ONE string means two
    /// mounts: `search` tags mount A with it, and `--mount` hands back mount B's note (Codex
    /// round 1). It is refused at the config DOOR, which is what makes every mount-tag surface
    /// safe rather than just this one, and which keeps the invariant that a workspace id always
    /// selects its own mount.
    #[test]
    fn a_mount_named_with_another_mounts_workspace_id_never_reaches_the_resolver() {
        let collide = Mount {
            workspace: Uuid::from_u128(1),
            dir: "m1".into(),
            folder: None,
            name: Some(Uuid::from_u128(2).to_string()),
            derived_dir: false,
            workspace_label: String::new(),
        };
        let other = Mount {
            workspace: Uuid::from_u128(2),
            dir: "m2".into(),
            folder: None,
            name: None,
            derived_dir: false,
            workspace_label: String::new(),
        };
        let cfg = |mounts: Vec<Mount>| DocliToml {
            server: "https://docli.ru".into(),
            mounts,
            mcp_label: None,
        };
        let e = crate::config::validate_config(&cfg(vec![collide.clone(), other.clone()]))
            .expect_err("the collision must be refused");
        assert!(
            format!("{e:#}").contains("another mount's workspace id"),
            "{e:#}"
        );
        // A mount named after its OWN workspace is harmless — one string, one mount.
        let mut selfnamed = other.clone();
        selfnamed.name = Some(Uuid::from_u128(2).to_string());
        crate::config::validate_config(&cfg(vec![selfnamed])).expect("self-naming is fine");
        // The tag is `display_name()`, which falls back to the DIRECTORY — so a mount with no
        // name sitting in a directory called `<some-uuid>` collides identically (Codex round 2).
        let by_dir = Mount {
            workspace: Uuid::from_u128(1),
            dir: Uuid::from_u128(2).to_string(),
            folder: None,
            name: None,
            derived_dir: false,
            workspace_label: String::new(),
        };
        let e = crate::config::validate_config(&cfg(vec![by_dir, other.clone()]))
            .expect_err("a nameless mount collides through its directory");
        assert!(format!("{e:#}").contains("directory named after"), "{e:#}");
        // …and the comparison is on the PARSED uuid: `Uuid::parse_str` takes braced and
        // unhyphenated spellings that a byte comparison would walk past.
        for spelling in [
            Uuid::from_u128(2).simple().to_string(),
            format!("{{{}}}", Uuid::from_u128(2)),
        ] {
            let mut alt = collide.clone();
            alt.name = Some(spelling.clone());
            crate::config::validate_config(&cfg(vec![alt, other.clone()]))
                .expect_err(&format!("spelling {spelling} must be refused too"));
        }
        // And with the door shut, a workspace id still selects its own mount, always.
        let f = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&f, "m1", 1, 9, "a.md", "from one\n");
        put_note(&f, "m2", 2, 10, "a.md", "from two\n");
        for (ws, want) in [(1u128, "from one\n"), (2, "from two\n")] {
            let mut a = args("a.md");
            a.mount = Some(Uuid::from_u128(ws).to_string());
            assert_eq!(served(resolve(&f.project, &a, 100)).body, want);
        }
    }

    /// The marker namespace `.docli/markers/<ws>/` has no ownership marker and no symlink
    /// refusal of its own — unlike the mount, which `verify_mount_identity` has already cleared.
    /// So its containment root is ANCHORED to the canonical project root rather than
    /// canonicalized: canonicalizing it would resolve the link and then compare the file against
    /// its own target, which always matches (Codex round 2).
    #[cfg(unix)]
    #[test]
    fn a_symlinked_marker_namespace_never_serves_what_it_points_at() {
        let f = fx(&[("mirror", 1, None)]);
        let ws = Uuid::from_u128(1);
        let id = Uuid::from_u128(7);
        let marker = "id x\nmime image/png\npath a.png\nsha256 unknown\nsize 3\n\
                      wikilink ![[a.png]]\n";
        // The real marker lives OUTSIDE the control root, reached only through a planted link.
        let outside = f.project.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join(format!("{id}.docli")), marker).unwrap();
        let markers = ControlRoot::new(&f.project.root).markers_dir();
        std::fs::create_dir_all(&markers).unwrap();
        std::os::unix::fs::symlink(&outside, markers.join(ws.to_string())).unwrap();

        let rel = format!(".docli/markers/{ws}/{id}.docli");
        let control = ControlRoot::new(&f.project.root);
        let mut st = WsState::fresh(None);
        st.from_zero = false;
        st.at_head = true;
        st.head_reached_at = Some(1);
        st.nodes.insert(
            id,
            NodeState {
                server_path: "a.png".into(),
                local_path: "a.png".into(),
                kind: TrackedKind::Attachment,
                rev: 1,
                content_sha256: crate::apply::sha_hex(marker.as_bytes()),
                marker_path: Some(rel),
                content_changed_at: None,
            },
        );
        control.save_state(ws, &st).unwrap();
        let r = refused(resolve(&f.project, &args("a.png"), 100));
        assert_eq!(r.exit, EXIT_NOT_IN_MIRROR);
        assert!(r.message.contains("outside the mirror"), "{}", r.message);
    }

    /// An operational read failure is a failure to LOOK, never «this mirror does not hold it»
    /// (Codex round 3). Exit 3 over a file nobody could open is the one conclusion this verb
    /// must never let a caller reach by accident — and, as an «answered» miss, it would also
    /// suppress the `mounts_unresolved` caveat over a sibling that IS served.
    #[test]
    fn a_file_that_cannot_be_read_is_not_reported_as_not_held() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        // A DIRECTORY where the note belongs: a deterministic non-NotFound read error that
        // needs no permission games (and so behaves the same running as root).
        let at = f.project.root.join("mirror/a.md");
        std::fs::remove_file(&at).unwrap();
        std::fs::create_dir(&at).unwrap();
        let r = refused(resolve(&f.project, &args("a.md"), 100));
        assert_eq!(
            r.exit, EXIT_FAILED,
            "never the not-in-mirror code: {}",
            r.message
        );
        assert_eq!(r.code, "unavailable");
        assert!(
            r.message.contains("says nothing about whether it exists"),
            "{}",
            r.message
        );

        // …and it leaves the mount UNANSWERED, so a hit in a sibling still carries the caveat.
        let g = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&g, "m1", 1, 9, "a.md", "from one\n");
        put_note(&g, "m2", 2, 10, "a.md", "from two\n");
        let broken = g.project.root.join("m2/a.md");
        std::fs::remove_file(&broken).unwrap();
        std::fs::create_dir(&broken).unwrap();
        let s = served(resolve(&g.project, &args("a.md"), 100));
        assert_eq!(s.body, "from one\n");
        assert!(
            s.envelope
                .disclosures()
                .iter()
                .any(|d| d.code == "mounts_unresolved"),
            "{:?}",
            s.envelope.disclosures()
        );
    }

    /// The selector is trimmed on the way in, so the LABELS must be trimmed too — otherwise a
    /// tag `search` printed for one mount resolves to another (Codex round 3). Both the
    /// resolver and `validate_config`'s collision door normalize the same way.
    #[test]
    fn selector_matching_trims_both_sides_so_a_printed_tag_cannot_change_meaning() {
        // `" notes "` and `"notes"` are one selector, so they are an ambiguity, not a silent
        // hand-off to whichever mount the resolver reached first.
        let f = fx(&[("m1", 1, Some(" notes ")), ("m2", 2, Some("notes"))]);
        put_note(&f, "m1", 1, 9, "a.md", "from one\n");
        put_note(&f, "m2", 2, 10, "a.md", "from two\n");
        for spelling in [" notes ", "notes"] {
            let mut a = args("a.md");
            a.mount = Some(spelling.into());
            let r = refused(resolve(&f.project, &a, 100));
            assert_eq!(r.code, "ambiguous_mount", "{spelling:?}: {}", r.message);
        }
        // And a padded name that is another mount's workspace id is caught at the door, which
        // an untrimmed parse would have sailed past.
        let padded = Mount {
            workspace: Uuid::from_u128(1),
            dir: "m1".into(),
            folder: None,
            name: Some(format!("  {}  ", Uuid::from_u128(2))),
            derived_dir: false,
            workspace_label: String::new(),
        };
        let other = Mount {
            workspace: Uuid::from_u128(2),
            dir: "m2".into(),
            folder: None,
            name: None,
            derived_dir: false,
            workspace_label: String::new(),
        };
        crate::config::validate_config(&DocliToml {
            server: "https://docli.ru".into(),
            mounts: vec![padded, other],
            mcp_label: None,
        })
        .expect_err("a padded collision must be refused too");
    }

    /// The clock-skew arm, and the overflow guard beneath it. A head stamp in the FUTURE makes
    /// the age term vacuous — `now - t` is negative, so an arbitrarily stale mirror reads as
    /// current and `read` serves it with no `mirror_not_usable` (Codex round 4). The realistic
    /// cause is a clock CORRECTION, which is why it does not force a rebuild.
    #[test]
    fn a_head_time_in_the_future_stops_the_mirror_vouching_for_itself() {
        let f = fx(&[("mirror", 1, None)]);
        put_note(&f, "mirror", 1, 9, "a.md", "x\n");
        let ws = Uuid::from_u128(1);
        let control = ControlRoot::new(&f.project.root);
        let mut st = control.load_state(ws).unwrap().unwrap();
        let now = 1_800_000_000;
        st.head_reached_at = Some(now + 3600);
        control.save_state(ws, &st).unwrap();
        let s = served(resolve(&f.project, &args("a.md"), now));
        assert_eq!(s.body, "x\n", "it still serves what it holds");
        assert!(
            note(&s.envelope)
                .disclosures
                .iter()
                .any(|d| d.code == "mirror_not_usable"),
            "{:?}",
            note(&s.envelope).disclosures
        );
        // …but it must NOT force a rebuild: a two-minute NTP correction re-downloading a large
        // mirror would be a worse bug than the one this closes.
        assert_eq!(st.rebuild_reason(None, now), None);
        // Ordinary skew is not an event. Firing on it would make the notice fire constantly.
        let mut small = st.clone();
        small.head_reached_at = Some(now + 30);
        assert_eq!(small.unusable_reason(None, now), None);
        // And an untrusted state file cannot make the arithmetic wrap.
        let mut hostile = st.clone();
        hostile.head_reached_at = Some(i64::MIN);
        assert!(hostile.rebuild_reason(None, now).is_some());
        hostile.head_reached_at = Some(i64::MAX);
        assert!(hostile.unusable_reason(None, now).is_some());
    }

    /// An explicitly-given flag is never silently discarded — `--mount "  "` used to fall
    /// through to «no selector» and search every mount (Codex round 4).
    #[test]
    fn a_blank_mount_selector_is_a_usage_error_not_a_silent_widening() {
        let f = fx(&[("m1", 1, Some("one")), ("m2", 2, Some("two"))]);
        put_note(&f, "m1", 1, 9, "a.md", "from one\n");
        for blank in ["", "   ", "\t"] {
            let mut a = args("a.md");
            a.mount = Some(blank.into());
            let r = refused(resolve(&f.project, &a, 100));
            assert_eq!(r.code, "usage", "{blank:?}: {}", r.message);
        }
        // …and a blank LABEL cannot exist to be printed in the first place.
        let e = crate::config::validate_config(&DocliToml {
            server: "https://docli.ru".into(),
            mounts: vec![Mount {
                workspace: Uuid::from_u128(1),
                dir: "m1".into(),
                folder: None,
                name: Some("   ".into()),
                derived_dir: false,
                workspace_label: String::new(),
            }],
            mcp_label: None,
        })
        .expect_err("a blank name must be refused");
        assert!(format!("{e:#}").contains("blank display name"), "{e:#}");
    }

    /// A closed pipe is the reader's decision. `docli read … | head` is a documented flow, and
    /// `print!` panics when stdout will not take the bytes (Codex round 1).
    #[test]
    fn a_closed_pipe_is_a_normal_end_not_a_panic() {
        assert!(broken_pipe(&std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed"
        )));
        assert!(!broken_pipe(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope"
        )));
    }

    #[test]
    fn addressing_needs_exactly_one_form() {
        let f = fx(&[("mirror", 1, None)]);
        let r = refused(resolve(
            &f.project,
            &ReadArgs {
                path: None,
                id: None,
                mount: None,
                lines: None,
                json: false,
            },
            100,
        ));
        assert_eq!(r.code, "usage");
    }

    #[test]
    fn lines_are_refused_on_an_attachment_rather_than_slicing_a_marker() {
        let f = fx(&[("mirror", 1, None)]);
        let marker = "id x\nmime image/png\npath a.png\nsha256 abc\nsize 1\nwikilink ![[a.png]]\n";
        put_file(
            &f,
            "mirror",
            1,
            7,
            "a.png",
            "a.png",
            marker,
            TrackedKind::Attachment,
            Some("a.png.docli"),
        );
        let mut a = args("a.png");
        a.lines = Some("1-2".into());
        let r = refused(resolve(&f.project, &a, 100));
        assert_eq!(r.code, "usage");
    }
}
