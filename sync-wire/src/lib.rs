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
}
