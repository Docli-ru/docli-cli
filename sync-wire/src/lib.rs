// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The docli vault-sync WIRE — plain serde shapes, extracted from `apps/api/src/sync.rs`
//! (v0.28.0 D1) so the api server and the Rust CLI consume ONE definition and api↔cli drift
//! fails to COMPILE instead of at runtime.
//!
//! Contract rules, load-bearing:
//! - **Wire bytes are pinned.** Field ORDER inside each struct is the JSON key order serde emits;
//!   the snapshot tests below fix the serialized form, so a reorder is a wire change and fails.
//! - **No server internals.** `SyncCursor` (and everything behind docli-core's `db` feature)
//!   stays out; the api converts at its own call sites.
//! - **Optional fields carry `#[serde(default)]`.** The api serializes with `skip_serializing_if`,
//!   so a deserializing CLI must tolerate the absent field. For `capabilities` (a bare `Vec`)
//!   the default is deserialization-ESSENTIAL; for the `Option`s it is belt — pinned either way
//!   by the round-trip test.
//! - The plugin's TS mirror (`packages/sync-client/serverModel.ts` + `ports.ts`) remains the wire
//!   definition running on users' machines; this crate does nothing for THAT drift.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The most workspace ids one `/api/sync/search` request may carry (v0.28.0 D5). A contract line,
/// not a tuning knob: `search_ws` runs build-on-miss INLINE for small workspaces, so an unbounded
/// list would serialize N index builds in one request. A client with more mounts BATCHES.
pub const SEARCH_WORKSPACE_CAP: usize = 16;

fn is_false(b: &bool) -> bool {
    !*b
}

/// The keyset pull cursor: strictly-after `(node_rev, id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct WireCursor {
    pub rev: i64,
    pub id: Uuid,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub workspace_id: Uuid,
    /// Required in the shared type even for ephemeral pulls (v0.28.0 D2a) — the ephemeral arm
    /// accepts and ignores it, so the shape stays one.
    pub client_id: String,
    pub cursor: WireCursor,
    pub epoch: i64,
    /// Client-tunable page size (mobile shrinks it). Clamped server-side to `[1, 2000]`; an
    /// ephemeral client MUST send a value inside that range, because the head-reaching predicate
    /// (`nodes.len() < limit`) is computed against the value each side believes in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// v0.27.0 D7 — the client's DURABLE-APPLY frontier: the predecessor of its earliest-keyset
    /// quarantined position (or its durably-applied cursor when nothing is quarantined). When
    /// present, the server acks THIS instead of the paging cursor and stamps `acked_at` per the
    /// ack-lag rule; absent → the legacy `cursor` ack, no `acked_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<WireCursor>,
    /// v0.28.0 D2a — an EPHEMERAL pull: no `sync_clients` registration, no ack, no lazy purge;
    /// the response synthesizes `last_mutation_id = 0` / `resync_required = false` and carries
    /// `live_nodes` on the head-reaching page. Default false — absent for every existing client,
    /// and a pre-v0.28.0 server ignores it (no `deny_unknown_fields`), which is exactly the
    /// rollback shape the CLI's live-node-count detector exists to catch.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ephemeral: bool,
    /// v0.29.1 D4 — ask for the workspace GRAPH on the head-reaching page. **Opt-in, default
    /// false**: the api ships before the CLI half, so a default-on payload would land on the
    /// installed 0.1.4 fleet at every session start. `sync --check` and `doctor` leave it off —
    /// neither reads the graph, and both run where a multi-megabyte body buys nothing.
    ///
    /// Honored on the EPHEMERAL arm only. Setting it on a registered pull is accepted and
    /// ignored, so the shape stays one (the `client_id` precedent).
    #[serde(default, skip_serializing_if = "is_false")]
    pub graph: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireNode {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    /// The RAW DB kind string (v0.7.4 build 7): an unknown future kind reaches the client verbatim
    /// so its known-kind filter can ignore it (vs being coerced to a file).
    pub kind: String,
    pub name: String,
    pub path: String,
    pub rev: i64,
    pub trashed: bool,
    pub mime: Option<String>,
    pub content_bytes: i32,
    pub body: Option<String>,
    pub blob_url: Option<String>,
    /// Fractional manual-order key (v0.12.3) — additive; old clients ignore it (raw-JSON
    /// forward-compat, v0.7.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// v0.27.0 — the blob's plaintext digest (content IDENTITY) and the content GENERATION (the
    /// remote-changed signal + the putBlob CAS base). Additive; old clients ignore both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_generation: Option<i64>,
}

/// v0.7.4 build 7: a server-advertised, version-sensitive feature gate. Each entry names a feature
/// and the MINIMUM plugin version that may use it; an older client self-disables that feature while
/// basic sync continues.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    pub feature: String,
    pub min_client_version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullResponse {
    pub epoch: i64,
    pub cursor: WireCursor,
    pub nodes: Vec<WireNode>,
    /// v0.7.4 build 7 forward-compat: version-gated feature advertisements (omitted when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    /// v0.7.0 D5: the client was over-stale (its tombstones may have been purged) → it must enter
    /// full-reconcile mode. Always `false` on the ephemeral arm (the server can never order a full
    /// reconcile for a client it does not register — the CLI's prune arm compensates).
    pub resync_required: bool,
    /// v0.7.3: this client's server-side exactly-once high-water mark. Synthesized `0` on the
    /// ephemeral arm (no client row exists).
    pub last_mutation_id: i64,
    /// v0.28.0 D2a — the workspace's live-node count, emitted on the EPHEMERAL arm's HEAD-REACHING
    /// page only (`nodes.len() < limit`, response-derived on BOTH sides — never cursor-vs-head
    /// equality, which purges' row-less barrier revs make permanently unsatisfiable). The CLI
    /// compares it against its live-id ledger to bound the hard-purge staleness window; its
    /// ABSENCE on a head-reaching ephemeral response is the rollback detector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_nodes: Option<i64>,
    /// v0.29.1 D4 — the workspace graph, present only when the request set
    /// [`PullRequest::graph`] AND this page reached head. A new CLI against an older api gets
    /// `None` and must say so; an older client ignores the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<WireGraph>,
}

// ---- the note graph (v0.29.1 D3/D4) ---------------------------------------------------------

/// One node's IDENTITY row — and the interning dictionary: every edge and tag addresses a node by
/// its INDEX into [`WireGraph::nodes`].
///
/// It carries `title`/`aliases` because the `read_note` envelope needs them and the pull page's
/// [`WireNode`] does not have them (v0.20.0 re-homed both into typed columns). It carries
/// `trashed` because `links` deliberately RETAINS rows whose source or target is trashed, so
/// every predicate over the edge set has to filter liveness itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub id: Uuid,
    /// The RAW DB kind string, like [`WireNode::kind`] — an unknown future kind reaches the
    /// client verbatim rather than being coerced.
    pub kind: String,
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    pub content_bytes: i32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub trashed: bool,
}

/// One `links` row, interned. Both endpoint columns stay DISCRIMINATED (D3): a `kind='embed'`
/// row may resolve to a note (`dst`) or to an attachment (`att`), and collapsing them into one
/// index would make `forward_links` and `attachment_embeds` read the same column — which is the
/// divergence this payload exists to remove.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    /// Index into [`WireGraph::nodes`] — the note the reference is written in.
    pub src: u32,
    /// `links.dst_node_id`, interned; absent when the ref resolved to nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst: Option<u32>,
    /// `links.dst_attachment_id`, interned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub att: Option<u32>,
    /// `links.dst_ref` — the raw target text exactly as written. NOT interned: it is the one
    /// field whose whole point is to be unresolved.
    #[serde(rename = "ref")]
    pub dst_ref: String,
    /// `wikilink` | `embed` | `markdown`, raw.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

/// One `note_tags` row, interned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphTag {
    /// Index into [`WireGraph::nodes`].
    pub node: u32,
    pub tag: String,
}

/// The whole workspace graph as one coherent snapshot, built in the SAME transaction and
/// snapshot as the page it rides (D4). It has no freshness lifecycle of its own — it arrives with
/// the page, is stored against the `(epoch, cursor)` that page reported, and dies with it. That is
/// what makes its absence mean exactly one thing: an api that cannot serve one.
///
/// **Workspace-wide, not scope-limited**, deliberately: an edge's other endpoint is routinely
/// outside a folder scope, and a scope-clipped graph would report a note as unlinked because the
/// mount is narrow — the false-negative class D5 exists to forbid.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireGraph {
    pub nodes: Vec<GraphNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<GraphEdge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<GraphTag>,
}

// ---- /api/sync/search (v0.28.0 D5) ----------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    /// The workspaces to search (the CLI sends its mounted set). At most
    /// [`SEARCH_WORKSPACE_CAP`]; over the cap is a request-level rejection.
    pub workspace_ids: Vec<Uuid>,
    /// Rides the POST body — note-content queries never ride a query string (the standing
    /// log-pin class).
    pub query: String,
    /// Per-workspace hit limit; server-clamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// v0.29.0 D2d — the caller's mirror POSITION per workspace, so the answer can carry the
    /// [`SearchWorkspaceOutcome::delta`] signal without a second round trip or a second audit
    /// row. Present only for mounts the client considers a usable projection; ABSENCE means
    /// "we did not ask", never "nothing to fetch".
    ///
    /// A `BTreeMap`, not a `HashMap`: a multi-entry byte pin over a hash map is
    /// order-nondeterministic. Keys outside `workspaceIds` are ignored; a map larger than
    /// [`SEARCH_WORKSPACE_CAP`] is a request-level rejection (the cap gates `workspaceIds`, and
    /// this route carries no body limit). The field is OMITTED, never `{}`, when no mount in a
    /// chunk qualifies — `{}` and absence are byte-different and the pin freezes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions: Option<BTreeMap<Uuid, MirrorPosition>>,
}

/// One mount's position on the sync keyset, plus the comparand only the client holds (v0.29.0
/// D2a): its live-id LEDGER count. Sending it up is what lets the SERVER own the whole verdict —
/// having both counts, it resolves the D2c precedence in one place instead of splitting it
/// across two machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPosition {
    pub cursor: WireCursor,
    pub epoch: i64,
    pub ledger_count: i64,
}

/// One ranked note hit (the tantivy/BM25 arm).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHitWire {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub snippet: String,
    pub rank: f32,
}

/// One attachment NAME hit (the `search_notes` precedent: a plain SQL name/path match, never
/// mixed into the ranked note hits — a filename match and a BM25 body match are not comparable).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAttachmentWire {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub mime: Option<String>,
}

/// The per-workspace outcome shape (v0.28.0 D5): an id that fails its gate yields a REFUSAL entry
/// while the others return hits — a request-level error is reserved for malformed requests and
/// the cap, so one stale mount can never abort fifteen valid searches.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchWorkspaceOutcome {
    pub workspace_id: Uuid,
    /// `Some(code)` = this workspace refused (scope / fence / pin / entitlement / ownership);
    /// every other field is then empty/false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<SearchHitWire>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<SearchAttachmentWire>,
    /// v0.15.1 D9 passthrough — the NOTE index was knowingly incomplete for this call. A degraded
    /// answer is INCONCLUSIVE about absence; both existing consumers (MCP, GraphQL) disclose it
    /// and the CLI must too.
    #[serde(default, skip_serializing_if = "is_false")]
    pub degraded: bool,
    /// The attachment arm's two truncation disclosures (v0.23.0): a capped or superset list that
    /// doesn't say so reads as "these are all of them".
    #[serde(default, skip_serializing_if = "is_false")]
    pub attachments_truncated: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub attachments_query_truncated: bool,
    /// v0.29.0 — the MIRROR DELTA for the position the caller sent in
    /// [`SearchRequest::positions`]: `"none"`, `"pending"`, `"epoch_mismatch"` or
    /// `"rebuild_required"`, all server-derived.
    ///
    /// **Not a freshness verdict** (D5) — `docli sync --check` remains that authority. This
    /// answers cursor comparability, position, and cardinality agreement, and nothing else.
    ///
    /// A plain string rather than an enum, deliberately (D2b): an unknown value from a newer
    /// server reaches an older client verbatim and is treated as NO ANSWER — silence, never a
    /// wrong sentence. Absent whenever the caller did not ask, the workspace refused, or the
    /// derivation failed; absence is never `"none"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// One outcome per requested workspace id, in request order.
    pub workspaces: Vec<SearchWorkspaceOutcome>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_full() -> WireNode {
        WireNode {
            id: Uuid::from_u128(1),
            parent_id: Some(Uuid::from_u128(2)),
            kind: "file".into(),
            name: "a.md".into(),
            path: "f/a.md".into(),
            rev: 7,
            trashed: false,
            mime: None,
            content_bytes: 5,
            body: Some("hello".into()),
            blob_url: None,
            position: Some("a0".into()),
            sha256: Some("ab".into()),
            blob_generation: Some(3),
        }
    }

    /// The wire-bytes pin (v0.28.0 D1): the extraction must leave the serialized form of a pull
    /// page byte-identical to what `apps/api/src/sync.rs` emitted before it. Key ORDER is part of
    /// the pin — serde emits fields in declaration order.
    #[test]
    fn pull_response_bytes_are_pinned() {
        let resp = PullResponse {
            epoch: 1,
            cursor: WireCursor {
                rev: 7,
                id: Uuid::from_u128(1),
            },
            nodes: vec![node_full()],
            capabilities: vec![],
            resync_required: false,
            last_mutation_id: 4,
            live_nodes: None,
            graph: None,
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            concat!(
                r#"{"epoch":1,"cursor":{"rev":7,"id":"00000000-0000-0000-0000-000000000001"},"#,
                r#""nodes":[{"id":"00000000-0000-0000-0000-000000000001","#,
                r#""parentId":"00000000-0000-0000-0000-000000000002","kind":"file","name":"a.md","#,
                r#""path":"f/a.md","rev":7,"trashed":false,"mime":null,"contentBytes":5,"#,
                r#""body":"hello","blobUrl":null,"position":"a0","sha256":"ab","blobGeneration":3}],"#,
                r#""resyncRequired":false,"lastMutationId":4}"#
            )
        );
    }

    /// The `serde(default)` pin: a response WITHOUT the optional fields (position/sha256/
    /// blobGeneration skipped, capabilities omitted, liveNodes absent) round-trips.
    #[test]
    fn optional_fields_deserialize_when_absent() {
        let json = concat!(
            r#"{"epoch":1,"cursor":{"rev":0,"id":"00000000-0000-0000-0000-000000000000"},"#,
            r#""nodes":[{"id":"00000000-0000-0000-0000-000000000001","parentId":null,"#,
            r#""kind":"folder","name":"f","path":"f","rev":1,"trashed":false,"mime":null,"#,
            r#""contentBytes":0,"body":null,"blobUrl":null}],"#,
            r#""resyncRequired":false,"lastMutationId":0}"#
        );
        let resp: PullResponse = serde_json::from_str(json).unwrap();
        assert!(resp.capabilities.is_empty());
        assert!(resp.live_nodes.is_none());
        // A pre-v0.29.1 api serves no graph, and «no graph» must not be an empty graph — the
        // client's whole absent-is-not-empty contract rests on this being `None`.
        assert!(resp.graph.is_none());
        let n = &resp.nodes[0];
        assert!(n.position.is_none() && n.sha256.is_none() && n.blob_generation.is_none());
    }

    /// The request the PLUGIN sends today (no `ack`, no `ephemeral`) still parses, and the flag
    /// defaults false — absence IS the registered path.
    #[test]
    fn a_plugin_shaped_pull_request_parses_with_ephemeral_false() {
        let json = concat!(
            r#"{"workspaceId":"00000000-0000-0000-0000-000000000001","clientId":"c1","#,
            r#""cursor":{"rev":0,"id":"00000000-0000-0000-0000-000000000000"},"epoch":1}"#
        );
        let req: PullRequest = serde_json::from_str(json).unwrap();
        assert!(!req.ephemeral);
        assert!(req.ack.is_none());
    }

    /// The CLI's serialization of an ephemeral request carries the flag and omits what it
    /// doesn't use — and `clientId` stays present (required in the shared type).
    #[test]
    fn an_ephemeral_request_serializes_the_flag() {
        let req = PullRequest {
            workspace_id: Uuid::from_u128(1),
            client_id: "ephemeral".into(),
            cursor: WireCursor {
                rev: 0,
                id: Uuid::nil(),
            },
            epoch: 1,
            limit: Some(500),
            ack: None,
            ephemeral: true,
            graph: false,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            concat!(
                r#"{"workspaceId":"00000000-0000-0000-0000-000000000001","clientId":"ephemeral","#,
                r#""cursor":{"rev":0,"id":"00000000-0000-0000-0000-000000000000"},"epoch":1,"#,
                r#""limit":500,"ephemeral":true}"#
            )
        );
    }

    // ---- the search family's byte pins (v0.29.0 D2d) -----------------------------------------

    fn ws(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    /// The first byte pin on the SEARCH family. Two entries, so the `BTreeMap` choice is
    /// load-bearing here rather than stylistic: over a `HashMap` this string is
    /// order-nondeterministic and the pin would flake.
    #[test]
    fn a_search_request_with_positions_is_pinned() {
        let mut positions = BTreeMap::new();
        positions.insert(
            ws(1),
            MirrorPosition {
                cursor: WireCursor { rev: 7, id: ws(9) },
                epoch: 3,
                ledger_count: 12,
            },
        );
        positions.insert(
            ws(2),
            MirrorPosition {
                cursor: WireCursor {
                    rev: 0,
                    id: Uuid::nil(),
                },
                epoch: 1,
                ledger_count: 0,
            },
        );
        let req = SearchRequest {
            workspace_ids: vec![ws(1), ws(2)],
            query: "заметка".into(),
            limit: Some(20),
            positions: Some(positions),
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            concat!(
                r#"{"workspaceIds":["00000000-0000-0000-0000-000000000001","#,
                r#""00000000-0000-0000-0000-000000000002"],"query":"заметка","limit":20,"#,
                r#""positions":{"00000000-0000-0000-0000-000000000001":{"cursor":{"rev":7,"#,
                r#""id":"00000000-0000-0000-0000-000000000009"},"epoch":3,"ledgerCount":12},"#,
                r#""00000000-0000-0000-0000-000000000002":{"cursor":{"rev":0,"#,
                r#""id":"00000000-0000-0000-0000-000000000000"},"epoch":1,"ledgerCount":0}}}"#
            )
        );
    }

    /// No mount in the chunk qualified ⇒ the field is OMITTED, not `{}`. The two are
    /// byte-different and «absence means we did not ask» is the contract the CLI reads.
    #[test]
    fn a_search_request_without_positions_omits_the_field() {
        let req = SearchRequest {
            workspace_ids: vec![ws(1)],
            query: "q".into(),
            limit: None,
            positions: None,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"workspaceIds":["00000000-0000-0000-0000-000000000001"],"query":"q"}"#
        );
    }

    /// The request the v0.28.x CLI sends (no `positions`) still parses — the field defaults to
    /// absent, which is exactly "did not ask".
    #[test]
    fn a_pre_v0_29_search_request_parses_with_no_positions() {
        let json = concat!(
            r#"{"workspaceIds":["00000000-0000-0000-0000-000000000001"],"query":"q","#,
            r#""limit":5}"#
        );
        let req: SearchRequest = serde_json::from_str(json).unwrap();
        assert!(req.positions.is_none());
    }

    /// The response pin, both ways: an answered outcome carries `delta` LAST, and an outcome
    /// with no answer omits it entirely (absence is never `"none"`).
    #[test]
    fn a_search_outcome_pins_the_delta_field() {
        let answered = SearchWorkspaceOutcome {
            workspace_id: ws(1),
            refused: None,
            hits: vec![],
            attachments: vec![],
            degraded: false,
            attachments_truncated: false,
            attachments_query_truncated: false,
            delta: Some("pending".into()),
        };
        assert_eq!(
            serde_json::to_string(&SearchResponse {
                workspaces: vec![answered]
            })
            .unwrap(),
            concat!(
                r#"{"workspaces":[{"workspaceId":"00000000-0000-0000-0000-000000000001","#,
                r#""delta":"pending"}]}"#
            )
        );
        let unanswered = SearchWorkspaceOutcome {
            workspace_id: ws(1),
            refused: Some("FORBIDDEN".into()),
            hits: vec![],
            attachments: vec![],
            degraded: false,
            attachments_truncated: false,
            attachments_query_truncated: false,
            delta: None,
        };
        assert_eq!(
            serde_json::to_string(&SearchResponse {
                workspaces: vec![unanswered]
            })
            .unwrap(),
            concat!(
                r#"{"workspaces":[{"workspaceId":"00000000-0000-0000-0000-000000000001","#,
                r#""refused":"FORBIDDEN"}]}"#
            )
        );
    }

    // ---- the graph payload's byte pins (v0.29.1 D4) -----------------------------------------

    /// The graph rides the pull page, so its bytes are pinned like the rest of it. The shape
    /// under test is the one that actually exercises interning: a note, an attachment, one
    /// resolved note edge, one attachment embed, one dangling ref, and a tag.
    #[test]
    fn a_graph_payload_is_pinned() {
        let graph = WireGraph {
            nodes: vec![
                GraphNode {
                    id: ws(1),
                    kind: "file".into(),
                    name: "a.md".into(),
                    path: "a.md".into(),
                    title: Some("A".into()),
                    aliases: vec!["alias".into()],
                    mime: None,
                    content_bytes: 12,
                    trashed: false,
                },
                GraphNode {
                    id: ws(2),
                    kind: "attachment".into(),
                    name: "p.png".into(),
                    path: "p.png".into(),
                    title: None,
                    aliases: vec![],
                    mime: Some("image/png".into()),
                    content_bytes: 99,
                    trashed: false,
                },
            ],
            edges: vec![
                GraphEdge {
                    src: 0,
                    dst: Some(0),
                    att: None,
                    dst_ref: "a".into(),
                    kind: "wikilink".into(),
                    anchor: Some("h".into()),
                },
                GraphEdge {
                    src: 0,
                    dst: None,
                    att: Some(1),
                    dst_ref: "p.png".into(),
                    kind: "embed".into(),
                    anchor: None,
                },
                GraphEdge {
                    src: 0,
                    dst: None,
                    att: None,
                    dst_ref: "nowhere".into(),
                    kind: "wikilink".into(),
                    anchor: None,
                },
            ],
            tags: vec![GraphTag {
                node: 0,
                tag: "проект".into(),
            }],
        };
        assert_eq!(
            serde_json::to_string(&graph).unwrap(),
            concat!(
                r#"{"nodes":[{"id":"00000000-0000-0000-0000-000000000001","kind":"file","#,
                r#""name":"a.md","path":"a.md","title":"A","aliases":["alias"],"#,
                r#""contentBytes":12},{"id":"00000000-0000-0000-0000-000000000002","#,
                r#""kind":"attachment","name":"p.png","path":"p.png","mime":"image/png","#,
                r#""contentBytes":99}],"#,
                r#""edges":[{"src":0,"dst":0,"ref":"a","kind":"wikilink","anchor":"h"},"#,
                r#"{"src":0,"att":1,"ref":"p.png","kind":"embed"},"#,
                r#"{"src":0,"ref":"nowhere","kind":"wikilink"}],"#,
                r#""tags":[{"node":0,"tag":"проект"}]}"#
            )
        );
        // …and it round-trips, which is what the CLI's cache does to it on every read.
        let back: WireGraph =
            serde_json::from_str(&serde_json::to_string(&graph).unwrap()).unwrap();
        assert_eq!(back, graph);
    }

    /// An EMPTY graph is a real answer — a workspace with no links and no tags — and must stay
    /// byte-distinguishable from `graph: null`, which means «this api serves none».
    #[test]
    fn an_empty_graph_is_not_an_absent_one() {
        let empty = serde_json::to_string(&WireGraph::default()).unwrap();
        assert_eq!(empty, r#"{"nodes":[]}"#);
        let held: Option<WireGraph> = serde_json::from_str(&empty).unwrap();
        assert!(held.is_some());
        let none: Option<WireGraph> = serde_json::from_str("null").unwrap();
        assert!(none.is_none());
    }

    /// The request pin for the flag, and the forward-compat direction that matters: a 0.1.4-era
    /// CLI's request parses with `graph` false, so the api's opt-in gate reads «did not ask».
    #[test]
    fn the_graph_flag_is_opt_in_on_the_wire() {
        let req = PullRequest {
            workspace_id: ws(1),
            client_id: "ephemeral".into(),
            cursor: WireCursor {
                rev: 0,
                id: Uuid::nil(),
            },
            epoch: 1,
            limit: Some(500),
            ack: None,
            ephemeral: true,
            graph: true,
        };
        assert!(serde_json::to_string(&req)
            .unwrap()
            .ends_with(r#""limit":500,"ephemeral":true,"graph":true}"#));
        let old = concat!(
            r#"{"workspaceId":"00000000-0000-0000-0000-000000000001","clientId":"c","#,
            r#""cursor":{"rev":0,"id":"00000000-0000-0000-0000-000000000000"},"epoch":1,"#,
            r#""ephemeral":true}"#
        );
        let req: PullRequest = serde_json::from_str(old).unwrap();
        assert!(!req.graph);
    }

    /// D2b's whole mechanism: an UNKNOWN value from a newer server deserializes verbatim. The
    /// CLI's job is then to say nothing about it — a typed enum would either fail the parse or
    /// need `serde(other)`, which serde documents only for tagged enums.
    #[test]
    fn an_unknown_delta_value_survives_deserialization_verbatim() {
        let json = concat!(
            r#"{"workspaces":[{"workspaceId":"00000000-0000-0000-0000-000000000001","#,
            r#""delta":"reindex_required"}]}"#
        );
        let resp: SearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.workspaces[0].delta.as_deref(),
            Some("reindex_required")
        );
        // …and a pre-v0.29.0 server's answer leaves it absent.
        let old = r#"{"workspaces":[{"workspaceId":"00000000-0000-0000-0000-000000000001"}]}"#;
        let resp: SearchResponse = serde_json::from_str(old).unwrap();
        assert!(resp.workspaces[0].delta.is_none());
    }
}
