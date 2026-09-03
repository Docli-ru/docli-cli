// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The note graph, INVERTED locally (v0.29.1 D3).
//!
//! The server computes the graph; the client only holds it and reads it from the other end. That
//! distinction is the whole of D5's amendment to the no-client-derivation pin: **the client may
//! hold a server-computed graph and invert it, never derive one.** Nothing here parses a wikilink,
//! resolves a basename, or decides what an ambiguous `[[ref]]` points at — every resolution
//! arrived already made, in `links.dst_node_id` / `links.dst_attachment_id`.
//!
//! # The five predicates
//!
//! What IS reimplemented here is the read half: five `WHERE` clauses that `packages/core`'s
//! `db::link` states in SQL. They are small and they are not obvious, and one of them is
//! counter-intuitive in a way a natural implementation gets backwards: **`backlinks` KEEPS
//! self-edges and `forward_links` excludes them.** So all five are pinned by vector files shared
//! with the api's own test (`packages/docli-rules/vectors/graph.json`) — the v0.28.0 cross-train
//! precedent, for the same reason: the CLI and the api ship on different trains, so the vectors
//! are the contract.
//!
//! # ORDER is not part of the parity claim, deliberately
//!
//! Every one of those queries ends `ORDER BY path ASC` (or `tag ASC`), evaluated under the
//! database's collation — `en_US.UTF-8` in this deployment, which is a locale collation and not
//! byte order (measured: it sorts `Ё Е е ё` before `_x`, and `a b` before `B`). Reproducing it
//! client-side means shipping a collation engine, which is a second implementation of a locale
//! — the exact class D3 refuses, for a field that is presentation.
//!
//! So these functions order by Rust's byte order, which is stable and total, and the vectors pin
//! **membership**, not sequence. Membership is what an agent acts on; a set that differed would
//! be a wrong answer, while an order that differs is a differently-sorted right one.

use std::collections::HashMap;

use docli_sync_wire::{GraphNode, WireGraph};
use uuid::Uuid;

/// A held graph with its id index rebuilt. The payload interns by INDEX, so answering anything
/// about a node id needs the reverse map.
pub struct Graph {
    g: WireGraph,
    by_id: HashMap<Uuid, u32>,
}

/// A note reference as the `read_note` envelope renders it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NoteRef {
    pub id: Uuid,
    pub name: String,
    pub path: String,
}

/// An attachment edge, matching the MCP `read_note.embeds` shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileRef {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub mime: Option<String>,
    pub size: i32,
}

impl Graph {
    pub fn new(g: WireGraph) -> Self {
        let by_id = g
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id, i as u32))
            .collect();
        Graph { g, by_id }
    }

    fn idx(&self, id: Uuid) -> Option<u32> {
        self.by_id.get(&id).copied()
    }

    fn at(&self, i: u32) -> Option<&GraphNode> {
        self.g.nodes.get(i as usize)
    }

    /// The identity row for a node id, live or trashed.
    pub fn node(&self, id: Uuid) -> Option<&GraphNode> {
        self.idx(id).and_then(|i| self.at(i))
    }

    fn live(&self, i: u32) -> Option<&GraphNode> {
        self.at(i).filter(|n| !n.trashed)
    }

    fn note_ref(n: &GraphNode) -> NoteRef {
        NoteRef {
            id: n.id,
            name: n.name.clone(),
            path: n.path.clone(),
        }
    }

    /// `db::link::backlinks` — the distinct LIVE notes that link to `id`.
    ///
    /// **Self-edges are KEPT** (the SQL carries no `src <> dst` filter): Obsidian users pin
    /// deliberate self-mentions and the panel shows them.
    pub fn backlinks(&self, id: Uuid) -> Vec<NoteRef> {
        let Some(target) = self.idx(id) else {
            return Vec::new();
        };
        let mut out: Vec<NoteRef> = self
            .g
            .edges
            .iter()
            .filter(|e| e.dst == Some(target))
            .filter_map(|e| self.live(e.src))
            .map(Self::note_ref)
            .collect();
        dedup_notes(&mut out);
        out
    }

    /// `db::link::forward_links` — the distinct LIVE notes `id` links to, **self EXCLUDED**
    /// (`l.dst_node_id <> $2`). Only note targets: an attachment embed carries no node edge.
    ///
    /// Note what is NOT filtered, matching the SQL: the SOURCE's own liveness. The join is on the
    /// target only, so a trashed note still reports its forward links.
    pub fn forward_links(&self, id: Uuid) -> Vec<NoteRef> {
        let Some(src) = self.idx(id) else {
            return Vec::new();
        };
        let mut out: Vec<NoteRef> = self
            .g
            .edges
            .iter()
            .filter(|e| e.src == src && e.dst.is_some_and(|d| d != src))
            .filter_map(|e| self.live(e.dst.expect("filtered")))
            .map(Self::note_ref)
            .collect();
        dedup_notes(&mut out);
        out
    }

    /// `db::link::attachment_embeds` — the distinct LIVE attachments a LIVE note embeds. Both
    /// ends are liveness-filtered here (the SQL joins `s` and `a`, each `trashed_at IS NULL`).
    pub fn attachment_embeds(&self, id: Uuid) -> Vec<FileRef> {
        let Some(src) = self.idx(id) else {
            return Vec::new();
        };
        if self.live(src).is_none() {
            return Vec::new();
        }
        let mut out: Vec<FileRef> = self
            .g
            .edges
            .iter()
            .filter(|e| e.src == src)
            .filter_map(|e| self.live(e.att?))
            .map(|a| FileRef {
                id: a.id,
                name: a.name.clone(),
                path: a.path.clone(),
                mime: a.mime.clone(),
                size: a.content_bytes,
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }

    /// `db::link::attachment_embedders` — the distinct LIVE notes embedding an attachment. The
    /// attachment's OWN liveness is not a filter (the SQL joins only the source), which is what
    /// makes this usable as a trash warning.
    pub fn attachment_embedders(&self, id: Uuid) -> Vec<NoteRef> {
        let Some(att) = self.idx(id) else {
            return Vec::new();
        };
        let mut out: Vec<NoteRef> = self
            .g
            .edges
            .iter()
            .filter(|e| e.att == Some(att))
            .filter_map(|e| self.live(e.src))
            .map(Self::note_ref)
            .collect();
        dedup_notes(&mut out);
        out
    }

    /// `db::link::unresolved_refs` — the distinct dangling targets written in a note.
    ///
    /// Two rules are load-bearing and both are the SQL's: `embed` rows are EXCLUDED (an embed's
    /// null node edge usually means it resolved to an attachment, not that it is missing), and the
    /// test is on the NOTE edge alone — `dst_node_id IS NULL`, whatever `dst_attachment_id` holds.
    /// No liveness filter anywhere.
    ///
    /// That second rule is a FAITHFUL COPY of a case nothing can currently produce, and copying it
    /// anyway is deliberate. `resolve_link_meta` (`packages/core/src/markdown/resolve.rs:285`)
    /// gives `Wikilink`/`Markdown` a note target or none — only `Embed`/`Image` can yield an
    /// attachment — so no in-scope row has both a whitelisted kind and a non-null attachment
    /// target. The predicate mirrors the SQL rather than the reachable subset of it, because the
    /// two sides ship on different trains and the day the resolver changes is the day the copy
    /// that guessed diverges.
    pub fn unresolved_refs(&self, id: Uuid) -> Vec<String> {
        let Some(src) = self.idx(id) else {
            return Vec::new();
        };
        let mut out: Vec<String> = self
            .g
            .edges
            .iter()
            .filter(|e| {
                e.src == src
                    && e.dst.is_none()
                    && !e.dst_ref.is_empty()
                    && matches!(e.kind.as_str(), "wikilink" | "markdown")
            })
            .map(|e| e.dst_ref.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// `db::tag::tags_for_node` — a live note's tags, alphabetical. A trashed node surfaces none.
    pub fn tags(&self, id: Uuid) -> Vec<String> {
        let Some(i) = self.idx(id) else {
            return Vec::new();
        };
        if self.live(i).is_none() {
            return Vec::new();
        }
        let mut out: Vec<String> = self
            .g
            .tags
            .iter()
            .filter(|t| t.node == i)
            .map(|t| t.tag.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// `SELECT DISTINCT … ORDER BY path` over `(id, name, path)`: dedup by ID, since that is the
/// row's identity, and sort by path with the id as tiebreak so the sequence is total.
fn dedup_notes(v: &mut Vec<NoteRef>) {
    v.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
    v.dedup_by(|a, b| a.id == b.id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use docli_sync_wire::{GraphEdge, GraphTag};
    use std::collections::BTreeMap;

    /// The CLI half of the D7 cross-train pin. The api half seeds the SAME file into Postgres and
    /// asserts `db::link`/`db::tag` answer identically
    /// (`packages/core/src/db/link.rs::the_graph_vectors_pin_the_five_read_predicates`).
    /// Read through the CRATE that owns the file, not by relative path: the public mirror lays
    /// the three crates out flat, where `../../../packages/…` does not exist.
    const VECTORS: &str = docli_rules::vectors::GRAPH;

    struct Fixture {
        graph: Graph,
        /// key → the synthetic uuid this side minted for it.
        ids: BTreeMap<String, Uuid>,
        expect: serde_json::Value,
    }

    fn fixture() -> Fixture {
        let v: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        let mut ids = BTreeMap::new();
        let mut nodes = Vec::new();
        for (i, n) in v["nodes"].as_array().unwrap().iter().enumerate() {
            let key = n["key"].as_str().unwrap().to_string();
            // 100 + i, so a bug that confuses an index with an id has a visible smell.
            let id = Uuid::from_u128(100 + i as u128);
            ids.insert(key, id);
            nodes.push(GraphNode {
                id,
                kind: n["kind"].as_str().unwrap().into(),
                name: n["name"].as_str().unwrap().into(),
                path: n["path"].as_str().unwrap().into(),
                title: n["title"].as_str().map(Into::into),
                aliases: n["aliases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect(),
                mime: n["mime"].as_str().map(Into::into),
                content_bytes: n["contentBytes"].as_i64().unwrap() as i32,
                trashed: n["trashed"].as_bool().unwrap(),
            });
        }
        let index: BTreeMap<Uuid, u32> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id, i as u32))
            .collect();
        let at = |key: &serde_json::Value| -> Option<u32> {
            let k = key.as_str()?;
            Some(index[&ids[k]])
        };
        let edges = v["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| GraphEdge {
                src: at(&l["src"]).unwrap(),
                dst: at(&l["dst"]),
                att: at(&l["att"]),
                dst_ref: l["ref"].as_str().unwrap().into(),
                kind: l["kind"].as_str().unwrap().into(),
                anchor: l["anchor"].as_str().map(Into::into),
            })
            .collect();
        let tags = v["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| GraphTag {
                node: at(&t["node"]).unwrap(),
                tag: t["tag"].as_str().unwrap().into(),
            })
            .collect();
        Fixture {
            graph: Graph::new(WireGraph { nodes, edges, tags }),
            ids,
            expect: v["expect"].clone(),
        }
    }

    /// MEMBERSHIP, sorted canonically on both sides — the vectors pin the set, never the
    /// collation-dependent sequence (see this module's header).
    fn set(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn the_graph_vectors_pin_the_five_read_predicates() {
        let f = fixture();
        let keys: Vec<String> = f.ids.keys().cloned().collect();
        // key BY ID, so an answer can be turned back into the vector's own vocabulary.
        let key_of: BTreeMap<Uuid, String> = f.ids.iter().map(|(k, v)| (*v, k.clone())).collect();
        let want = |pred: &str, k: &str| -> Vec<String> {
            set(f.expect[pred][k]
                .as_array()
                .unwrap_or_else(|| panic!("vector {pred}.{k} is missing"))
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect())
        };

        for k in &keys {
            let id = f.ids[k];
            assert_eq!(
                set(f
                    .graph
                    .backlinks(id)
                    .into_iter()
                    .map(|r| key_of[&r.id].clone())
                    .collect()),
                want("backlinks", k),
                "backlinks({k})"
            );
            assert_eq!(
                set(f
                    .graph
                    .forward_links(id)
                    .into_iter()
                    .map(|r| key_of[&r.id].clone())
                    .collect()),
                want("forwardLinks", k),
                "forwardLinks({k})"
            );
            assert_eq!(
                set(f
                    .graph
                    .attachment_embeds(id)
                    .into_iter()
                    .map(|r| key_of[&r.id].clone())
                    .collect()),
                want("attachmentEmbeds", k),
                "attachmentEmbeds({k})"
            );
            assert_eq!(
                set(f
                    .graph
                    .attachment_embedders(id)
                    .into_iter()
                    .map(|r| key_of[&r.id].clone())
                    .collect()),
                want("attachmentEmbedders", k),
                "attachmentEmbedders({k})"
            );
            assert_eq!(
                set(f.graph.unresolved_refs(id)),
                want("unresolved", k),
                "unresolved({k})"
            );
            assert_eq!(set(f.graph.tags(id)), want("tags", k), "tags({k})");
        }
    }

    /// The identity row carries what the pull page does not (v0.20.0's typed columns) — the whole
    /// reason the payload has an identity table rather than joining to the client's node state.
    #[test]
    fn the_identity_row_carries_title_and_aliases_and_file_facts() {
        let f = fixture();
        let a = f.graph.node(f.ids["a"]).unwrap();
        assert_eq!(a.title.as_deref(), Some("Alpha note"));
        assert_eq!(a.aliases, vec!["Alpha", "Первая"]);
        let p = f.graph.node(f.ids["p"]).unwrap();
        assert_eq!(p.mime.as_deref(), Some("image/png"));
        assert_eq!(p.content_bytes, 99);
        // The attachment arm reads these off the same row.
        let embeds = f.graph.attachment_embeds(f.ids["a"]);
        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0].mime.as_deref(), Some("image/png"));
        assert_eq!(embeds[0].size, 99);
    }

    /// An id the graph has never heard of answers EMPTY, not a panic — the graph is workspace-wide
    /// while a mount can be scoped, so `read` will ask about ids on both sides of that line.
    #[test]
    fn an_unknown_id_answers_empty_everywhere() {
        let f = fixture();
        let ghost = Uuid::from_u128(999);
        assert!(f.graph.node(ghost).is_none());
        assert!(f.graph.backlinks(ghost).is_empty());
        assert!(f.graph.forward_links(ghost).is_empty());
        assert!(f.graph.attachment_embeds(ghost).is_empty());
        assert!(f.graph.attachment_embedders(ghost).is_empty());
        assert!(f.graph.unresolved_refs(ghost).is_empty());
        assert!(f.graph.tags(ghost).is_empty());
    }
}
