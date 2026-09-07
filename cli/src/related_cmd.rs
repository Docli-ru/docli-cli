// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli related` (v0.29.9 D7) — the server's `related_notes` answer, evaluated offline from the
//! artifact the mirror holds.
//!
//! The CLI re-implements NOTHING that ranks: the graph-arm and tag-arm lists in the artifact ARE
//! the server's own top-20 per subject (D5), the lexical arm is `docli_rules::related::cosine`
//! over the server's own vectors (D3), and the fusion below is core's `fuse` — RRF k=60, the
//! 150-word demote, the `path` tie-break under Rust `String::cmp` — carried here in fifteen lines
//! and pinned against the shared fixture `docli_rules::vectors::RELATED`, which the api's arm
//! test and core's `fuse` test consume too.
//!
//! What the CLI applies that is FRESHER than the artifact: its own exclusion set and liveness, from
//! the held graph — at the same point in each arm as the server does. For the COMPUTED lexical arm,
//! exclusion BEFORE truncation (the server over-fetches by the exclusion size, filters, then
//! truncates), liveness after (the server's `names_for_ids` drop). For the two HELD arms, both
//! after — those lists are already the server's top-20, so exclusion and liveness can only SHRINK
//! them; a link made since the build drops that candidate with no 21st to promote. That asymmetry
//! is the one place this answer is MORE current than the artifact. What the CLI does NOT do is
//! re-check a held row against the fresher graph (a mediator unlinked or a tag removed since the
//! build leaves its row standing, with the `why` the server gave it) — that check would be the
//! re-derivation D5 deleted — so the two held arms are exactly «the server's answer at
//! `covers_rev`, minus what this mirror has since excluded», and the disclosures say so.
//!
//! # The parity claim, stated exactly
//!
//! `docli related` = the server's answer as computed by the build that covers `covers_rev`, arm
//! for arm, for a subject whose own row is unchanged. The server's live SQL arms may have moved
//! since (a new backlink, a trashed mediator, anywhere in the workspace); the CLI discloses that
//! by the HEAD DISTANCE `head_rev − covers_rev` — two server-side numbers, never the mirror's
//! keyset cursor — and, per subject, when the subject's own `node_rev` passed `covers_rev` (the
//! server would then compute a fresher vector; `node_rev` and not `content_changed_at`, because a
//! rename or a new link genuinely moves the vector and the arms).
//!
//! # Without the artifact there is no `related` on the CLI — say so, never improvise one
//!
//! A subject's own backlinks and tags are its EXCLUSION set, not its relatives, and deriving
//! co-citation from the held graph's edges is the re-implementation D5 deleted. So when the
//! artifact is unavailable the answer is `related: null` plus a named reason and remedy — the
//! v0.29.1 invariant that every null key is named. `related` needs the held GRAPH too (identity,
//! liveness, the fresher exclusion), so a missing graph is the fourth absence, reported in the
//! graph's own words.
//!
//! # A limit stated: the subject must be a node this mirror holds
//!
//! `related` addresses its subject through `read`'s `locate`, so under a folder-scoped mount a
//! note outside the scope — one `docli tree` lists as «not mirrored here» — is refused with exit 3
//! although the workspace-wide artifact and graph both hold it. The one addressing surface across
//! `read`/`related`/`ls` is worth more than that case; `related_notes` over MCP answers it.
//!
//! # What `related` borrows from `read`, and what it does not
//!
//! It does NOT refuse (no exit 4): that refusal is about a note's BYTES, and this answer reads
//! none. It DOES carry `read`'s `mirror_not_usable` disclosure, from the same
//! `WsState::unusable_reason` — the held graph and artifact are projections of this mirror, and a
//! `sync --check` that marked the mirror behind moves that flag without moving the cursor, so the
//! graph's `(epoch, cursor)` gate cannot see it; `read` and `search` both speak over that state,
//! and silence here would be the two-readers-of-one-question defect. And it borrows the fact
//! behind a v0.29.7 mark: a note the server has named as changed above the mirror's rev is, by
//! that same fact, a subject the generation may predate — so a standing mark fires
//! `subject_changed` exactly as a mirror-applied `node_rev` past `covers_rev` does. Without it the
//! disclosure would be blind to precisely the notes the CLI had been told about, since a marked
//! node's mirror rev is behind by definition.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use docli_sync_wire::{RelatedSubject, WireRelated};
use serde::Serialize;
use uuid::Uuid;

use crate::config::{validate_config, Project};
use crate::graph::Graph;
use crate::read_cmd::{
    across_mounts, graph_slot, locate, parse_target, render_refusal, unverified_disclosure,
    write_json, Disclosure, GraphSlot, MountKey, SCOPE_DISCLOSURE,
};
use crate::state::{ControlRoot, WsState};

/// The same constants as core's `fuse` — RRF k, the short-note threshold and the demoted weight.
/// Duplicated by design: the CLI never links docli-core, and the shared fixture is what keeps the
/// two copies honest.
const RRF_K: f64 = 60.0;
const SHORT_NOTE_WORDS: u32 = 150;
const LEXICAL_DEMOTE: f64 = 0.4;
/// Per-arm depth (the server's `PER_SOURCE_K`).
const PER_SOURCE_K: usize = 20;
/// `why.terms` depth.
const WHY_TERMS: usize = 3;

pub const LIMIT_DEFAULT: usize = 10;
pub const LIMIT_MAX: usize = 25;

const NOT_SYNCED: &str = "not held - this mirror was last synced before the related artifact \
                          existed; `docli sync` fetches it";
const NOT_SERVED: &str = "not held - no related artifact has arrived from the server (an api \
                          before v0.29.9, a workspace not yet reindexed since, or a fetch that \
                          failed); `docli sync` asks again, and `related_notes` over the docli MCP \
                          connection answers directly";
const SUBJECT_MISSING: &str = "not in the held artifact - this note or file was created after the \
                               generation the mirror holds was built; `docli sync` fetches the \
                               next generation once the server has rebuilt it, and `related_notes` \
                               over the docli MCP connection answers now";

/// `read`'s `mirror_not_usable` disclosure, word for word — one sentence for the three verbs that
/// answer from a mirror they cannot vouch for.
pub(crate) fn mirror_not_usable(reason: &str) -> Disclosure {
    Disclosure {
        code: "mirror_not_usable",
        message: format!(
            "the local mirror cannot be vouched for right now - {reason}; \
             `docli sync --check` either clears the condition or names the fix"
        ),
    }
}

pub struct RelatedArgs {
    pub path: Option<String>,
    pub id: Option<Uuid>,
    pub mount: Option<String>,
    pub limit: usize,
    pub json: bool,
}

/// One ranked relative — the `related_notes` shape (id, name, path, kind, mime, score, why).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hit {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Rounded to 4 decimals like the MCP tool: RRF scores live in ~0.001..0.05.
    pub score: f64,
    #[serde(skip_serializing_if = "Why::is_empty")]
    pub why: Why,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Why {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_neighbors: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl Why {
    fn is_empty(&self) -> bool {
        self.shared_neighbors.is_none() && self.terms.is_empty() && self.tags.is_empty()
    }
}

/// The envelope: `related` is the list, or `null` with its reason under `absent` — never `[]`
/// standing in for «unknown».
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub kind: &'static str,
    pub id: Uuid,
    pub path: String,
    pub mount: String,
    pub workspace: Uuid,
    /// The generation the answer was computed from, when one is held.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers_rev: Option<i64>,
    pub related: Option<Vec<Hit>>,
    pub absent: BTreeMap<String, String>,
    pub disclosures: Vec<Disclosure>,
}

pub fn run(project: &Project, args: &RelatedArgs) -> Result<i32> {
    if args.json {
        crate::ui::machine_mode();
    }
    validate_config(&project.config)?;
    let control = project.control_root();
    let target = match parse_target(
        args.path.as_deref(),
        args.id,
        "usage: docli related <server-path>   (the path `docli search` prints)",
    ) {
        Ok(t) => t,
        Err(r) => return Ok(render_refusal(&r, args.json)),
    };
    let located = across_mounts(project, args.mount.as_deref(), |m| {
        locate(project, &control, m, &target).map(|l| {
            (
                MountKey {
                    name: m.display_name().to_string(),
                    workspace: m.workspace,
                },
                (
                    m.display_name().to_string(),
                    m.workspace,
                    m.folder.clone(),
                    l,
                ),
            )
        })
    });
    let ((mount, ws, folder, l), unverified) = match located {
        Ok(v) => v,
        Err(r) => return Ok(render_refusal(&r, args.json)),
    };
    let limit = args.limit.clamp(1, LIMIT_MAX);
    let scoped = folder.is_some();
    // The subject counts as changed if the MIRROR applied a rev past the generation, or if the
    // server NAMED it as changed above the mirror's rev (a v0.29.7 mark) — either way the
    // generation may predate the subject's current text, name or links.
    let subject_rev = if l.marks.contradict(l.id, l.node.rev) {
        l.marks.latest[&l.id]
    } else {
        l.node.rev
    };
    let mut env = answer(&control, ws, &l.st, l.id, subject_rev, scoped, limit);
    // `read`'s own currency disclosure, over the same state (`WsState::unusable_reason`, the one
    // readiness predicate): the held graph and artifact are projections of this mirror, so a
    // `sync --check` that marked it behind speaks here as it does in `read`.
    if let Some(reason) =
        l.st.unusable_reason(folder.as_deref(), crate::sync_cmd::now_unix())
    {
        env.disclosures.push(mirror_not_usable(reason));
    }
    env.mount = mount;
    env.path = l.node.server_path.clone();
    if let Some(d) = unverified_disclosure(&unverified) {
        env.disclosures.push(d);
    }
    Ok(render(&env, args.json))
}

/// Resolve the two held artifacts for `ws` and compute — or name why not.
fn answer(
    control: &ControlRoot,
    ws: Uuid,
    st: &WsState,
    id: Uuid,
    node_rev: i64,
    scoped: bool,
    limit: usize,
) -> Envelope {
    let mut env = Envelope {
        kind: "related",
        id,
        path: String::new(),
        mount: String::new(),
        workspace: ws,
        covers_rev: None,
        related: None,
        absent: BTreeMap::new(),
        disclosures: Vec::new(),
    };
    // The graph first: identity, liveness and the fresher exclusion all come from it, and it
    // keeps its own stricter `(epoch, cursor)` gate — its reason is reported verbatim.
    let graph = match graph_slot(control, ws, st) {
        GraphSlot::Held(g) => g,
        GraphSlot::Absent(why) => {
            env.absent.insert("related".into(), why.to_string());
            return env;
        }
    };
    let Some(artifact) = control.load_related(ws) else {
        env.absent.insert(
            "related".into(),
            if st.related_asked {
                NOT_SERVED
            } else {
                NOT_SYNCED
            }
            .to_string(),
        );
        return env;
    };
    env.covers_rev = Some(artifact.covers_rev);
    let Some(subject) = artifact.subjects.iter().find(|s| s.id == id) else {
        env.absent
            .insert("related".into(), SUBJECT_MISSING.to_string());
        return env;
    };
    let excluded = exclusion_set(&graph, id, subject.note.is_none());
    env.related = Some(rank(&graph, &artifact, subject, &excluded, limit));

    if node_rev > artifact.covers_rev {
        env.disclosures.push(Disclosure {
            code: "subject_changed",
            message: "this note or file changed after the held artifact was built (its name, \
                      links or text), so the server would rank from a fresher vector - \
                      `docli sync` fetches the next generation once the server has rebuilt it"
                .into(),
        });
    }
    match st.head_rev {
        Some(head) if head > artifact.covers_rev => {
            env.disclosures.push(Disclosure {
                code: "workspace_moved",
                message: format!(
                    "the workspace has moved {} since the held artifact was built, so the \
                     link-graph and tag lists here are the server's answer at that generation \
                     (minus what this mirror has since excluded) - a neighbour linked or tagged \
                     since is missing, one unlinked since may still be listed; `docli sync` \
                     fetches the next generation once the server has rebuilt it",
                    crate::ui::plural(
                        (head - artifact.covers_rev) as usize,
                        "revision",
                        "revisions"
                    )
                ),
            });
        }
        _ => {}
    }
    if scoped {
        env.disclosures.push(Disclosure {
            code: "graph_wider_than_mount",
            message: SCOPE_DISCLOSURE.into(),
        });
    }
    env
}

/// The server's `exclusion_set`, read off the held graph: self, and for a note its live
/// out-targets, in-linkers and embedded files; for a file, the notes that embed it.
fn exclusion_set(graph: &Graph, id: Uuid, is_attachment: bool) -> HashSet<Uuid> {
    let mut out: HashSet<Uuid> = HashSet::from([id]);
    if is_attachment {
        out.extend(graph.attachment_embedders(id).into_iter().map(|r| r.id));
        return out;
    }
    out.extend(graph.forward_links(id).into_iter().map(|r| r.id));
    out.extend(graph.backlinks(id).into_iter().map(|r| r.id));
    out.extend(graph.attachment_embeds(id).into_iter().map(|r| r.id));
    out
}

/// The three arms and the fusion. Pure over the held data, so the shared fixture can drive it.
pub(crate) fn rank(
    graph: &Graph,
    artifact: &WireRelated,
    subject: &RelatedSubject,
    excluded: &HashSet<Uuid>,
    limit: usize,
) -> Vec<Hit> {
    let live = |id: Uuid| graph.node(id).is_some_and(|n| !n.trashed);
    let dict = |i: u32| artifact.dict.get(i as usize).cloned().unwrap_or_default();
    let at = |i: u32| artifact.subjects.get(i as usize).map(|s| s.id);

    // Lexical: every other note row with cosine > 0, (cosine desc, id asc); exclusion BEFORE the
    // truncation, liveness after — the server's own order.
    let mut lexical: Vec<(Uuid, Vec<String>)> = Vec::new();
    if let Some(v) = &subject.note {
        type Scored<'a> = (f32, Uuid, &'a [(u32, f32)]);
        let mut scored: Vec<Scored<'_>> = artifact
            .subjects
            .iter()
            .filter_map(|s| {
                let c = s.note.as_ref()?;
                let score = docli_rules::related::cosine(&v.terms, &c.terms);
                (score > 0.0).then_some((score, s.id, c.terms.as_slice()))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        lexical = scored
            .into_iter()
            .filter(|(_, id, _)| !excluded.contains(id))
            .take(PER_SOURCE_K)
            .filter(|(_, id, _)| live(*id))
            .map(|(_, id, terms)| {
                let why = docli_rules::related::top_shared_terms(&v.terms, terms, WHY_TERMS)
                    .into_iter()
                    .map(dict)
                    .collect();
                (id, why)
            })
            .collect();
    }
    // The two held arms only ever shrink.
    let graph_arm: Vec<(Uuid, i64)> = subject
        .graph
        .iter()
        .filter_map(|(i, shared)| at(*i).map(|id| (id, *shared as i64)))
        .filter(|(id, _)| !excluded.contains(id) && live(*id))
        .collect();
    let tag_arm: Vec<(Uuid, Vec<String>)> = subject
        .tags
        .iter()
        .filter_map(|(i, tags)| at(*i).map(|id| (id, tags.iter().map(|t| dict(*t)).collect())))
        .filter(|(id, _)| !excluded.contains(id) && live(*id))
        .collect();

    // RRF — core's `fuse`, verbatim in rule: Σ w / (k + rank), rank 1-based per arm, the lexical
    // arm demoted for a short subject, ties by path.
    let w_lexical = match &subject.note {
        Some(v) if v.wc < SHORT_NOTE_WORDS => LEXICAL_DEMOTE,
        _ => 1.0,
    };
    let mut acc: HashMap<Uuid, Hit> = HashMap::new();
    let fold = |acc: &mut HashMap<Uuid, Hit>, id: Uuid, w: f64, rank: usize| -> Option<()> {
        let n = graph.node(id)?;
        let e = acc.entry(id).or_insert_with(|| Hit {
            id,
            name: n.name.clone(),
            path: n.path.clone(),
            kind: n.kind.clone(),
            mime: n.mime.clone(),
            score: 0.0,
            why: Why::default(),
        });
        e.score += w / (RRF_K + rank as f64);
        Some(())
    };
    for (i, (id, shared)) in graph_arm.iter().enumerate() {
        if fold(&mut acc, *id, 1.0, i + 1).is_some() {
            // The MCP tool's rule verbatim: the key appears only for a positive count.
            acc.get_mut(id).unwrap().why.shared_neighbors = (*shared > 0).then_some(*shared);
        }
    }
    for (i, (id, terms)) in lexical.iter().enumerate() {
        if fold(&mut acc, *id, w_lexical, i + 1).is_some() {
            acc.get_mut(id).unwrap().why.terms = terms.clone();
        }
    }
    for (i, (id, tags)) in tag_arm.iter().enumerate() {
        if fold(&mut acc, *id, 1.0, i + 1).is_some() {
            acc.get_mut(id).unwrap().why.tags = tags.clone();
        }
    }
    let mut out: Vec<Hit> = acc.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    out.truncate(limit);
    for h in &mut out {
        h.score = (h.score * 10000.0).round() / 10000.0;
    }
    out
}

fn render(env: &Envelope, json: bool) -> i32 {
    for d in &env.disclosures {
        crate::ui::warn(&d.message);
    }
    // An ABSENT answer exits 1 in both modes — `ls`/`tree`/`tags`/`tagged`'s rule for a mount
    // whose graph is not held, and unlike `read`'s absent graph fields: there the product (the
    // note) was delivered and one field was missing; here the whole product is. A ranked answer,
    // empty included, is 0.
    let exit = if env.related.is_some() { 0 } else { 1 };
    if json {
        return write_json(env, exit);
    }
    match &env.related {
        None => {
            let why = env.absent.get("related").cloned().unwrap_or_default();
            crate::ui::warn(&format!("related: {why}"));
            exit
        }
        Some(hits) if hits.is_empty() => {
            crate::ui::detail("no related notes or files");
            0
        }
        Some(hits) => {
            for h in hits {
                let mut why: Vec<String> = Vec::new();
                if let Some(n) = h.why.shared_neighbors {
                    why.push(format!("neighbors {n}"));
                }
                if !h.why.terms.is_empty() {
                    why.push(format!("terms {}", h.why.terms.join(", ")));
                }
                if !h.why.tags.is_empty() {
                    why.push(format!("tags {}", h.why.tags.join(", ")));
                }
                let tail = if why.is_empty() {
                    String::new()
                } else {
                    format!("  {}", crate::ui::dim(&why.join(" \u{b7} ")))
                };
                let kind = if h.kind == "attachment" {
                    "  [file]"
                } else {
                    ""
                };
                // The rows are the product, on stdout through the pipe-safe vocabulary
                // (`docli related x.md | head` is an ordinary way to read one).
                crate::ui::line(&format!(
                    "{:.4}  {}{kind}{tail}",
                    h.score,
                    crate::ui::sanitize(&h.path)
                ));
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docli_sync_wire::{GraphNode, RelatedVector, WireGraph};

    struct Fixture {
        graph: Graph,
        artifact: WireRelated,
        ids: BTreeMap<String, Uuid>,
        excluded: BTreeMap<String, Vec<String>>,
        expect: serde_json::Value,
    }

    /// The shared fixture, materialised: ids in `subjects` order (the file's own rule), the graph
    /// holding one identity row per subject.
    fn fixture() -> Fixture {
        let v: serde_json::Value = serde_json::from_str(docli_rules::vectors::RELATED).unwrap();
        let subjects_v = v["subjects"].as_array().unwrap();
        let ids: BTreeMap<String, Uuid> = subjects_v
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    s["key"].as_str().unwrap().to_string(),
                    Uuid::from_u128(i as u128 + 1),
                )
            })
            .collect();
        let idx = |key: &str| subjects_v.iter().position(|s| s["key"] == key).unwrap() as u32;
        let nodes: Vec<GraphNode> = subjects_v
            .iter()
            .map(|s| GraphNode {
                id: ids[s["key"].as_str().unwrap()],
                kind: s["kind"].as_str().unwrap().into(),
                name: s["path"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .into(),
                path: s["path"].as_str().unwrap().into(),
                title: None,
                aliases: vec![],
                mime: (s["kind"] == "attachment").then(|| "image/png".to_string()),
                content_bytes: 1,
                trashed: s["trashed"].as_bool().unwrap(),
            })
            .collect();
        let subjects: Vec<RelatedSubject> = subjects_v
            .iter()
            .map(|s| RelatedSubject {
                id: ids[s["key"].as_str().unwrap()],
                note: s["note"].as_object().map(|n| RelatedVector {
                    wc: n["wc"].as_u64().unwrap() as u32,
                    terms: n["terms"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| (t[0].as_u64().unwrap() as u32, t[1].as_f64().unwrap() as f32))
                        .collect(),
                }),
                graph: s["graph"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|g| (idx(g[0].as_str().unwrap()), g[1].as_u64().unwrap() as u32))
                    .collect(),
                tags: s["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| {
                        (
                            idx(t[0].as_str().unwrap()),
                            t[1].as_array()
                                .unwrap()
                                .iter()
                                .map(|d| d.as_u64().unwrap() as u32)
                                .collect(),
                        )
                    })
                    .collect(),
            })
            .collect();
        let artifact = WireRelated {
            covers_rev: 1,
            covers_id: Uuid::from_u128(1),
            dict: v["dict"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_str().unwrap().to_string())
                .collect(),
            subjects,
        };
        let excluded = v["excluded"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, list)| {
                (
                    k.clone(),
                    list.as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect();
        Fixture {
            graph: Graph::new(WireGraph {
                nodes,
                edges: vec![],
                tags: vec![],
            }),
            artifact,
            ids,
            excluded,
            expect: v["expect"].clone(),
        }
    }

    /// The cross-train contract: the CLI's fusion over the held artifact reproduces the fixture's
    /// SEQUENCE for every subject — the same file the api's arm test and core's `fuse` test read.
    #[test]
    fn the_fusion_agrees_with_the_shared_vectors() {
        let f = fixture();
        let key_of: BTreeMap<Uuid, String> = f.ids.iter().map(|(k, v)| (*v, k.clone())).collect();
        for (key, want) in f.expect["fused"].as_object().unwrap() {
            let id = f.ids[key];
            let subject = f.artifact.subjects.iter().find(|s| s.id == id).unwrap();
            let mut excluded: HashSet<Uuid> = HashSet::from([id]);
            for e in &f.excluded[key] {
                excluded.insert(f.ids[e]);
            }
            let got: Vec<String> = rank(&f.graph, &f.artifact, subject, &excluded, 10)
                .into_iter()
                .map(|h| key_of[&h.id].clone())
                .collect();
            let want: Vec<String> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect();
            assert_eq!(got, want, "fused({key})");
        }
    }

    /// `why` carries what each arm contributed, with the server's own vocabulary.
    #[test]
    fn why_names_each_arms_contribution() {
        let f = fixture();
        let s = f.ids["s"];
        let subject = f.artifact.subjects.iter().find(|x| x.id == s).unwrap();
        let excluded = HashSet::from([s, f.ids["x"]]);
        let hits = rank(&f.graph, &f.artifact, subject, &excluded, 10);
        let a = hits.iter().find(|h| h.id == f.ids["a"]).unwrap();
        assert_eq!(a.why.shared_neighbors, Some(1));
        assert_eq!(a.why.terms, vec!["alpha"]);
        let c = hits.iter().find(|h| h.id == f.ids["c"]).unwrap();
        assert_eq!(c.why.tags, vec!["proj"]);
        assert_eq!(c.why.terms, vec!["beta"]);
        // An attachment relative carries its kind and mime; the score is 4-decimal like MCP.
        let p = f
            .artifact
            .subjects
            .iter()
            .find(|x| x.id == f.ids["p"])
            .unwrap();
        let hits = rank(&f.graph, &f.artifact, p, &HashSet::from([f.ids["p"]]), 10);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].why.terms.is_empty(),
            "no lexical arm for a file subject"
        );
        assert_eq!(hits[0].score, (1.0f64 / 61.0 * 10000.0).round() / 10000.0);
    }

    /// The four absences each name themselves, and none of them is an empty list.
    #[test]
    fn absence_is_named_never_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let control = ControlRoot::new(tmp.path());
        let ws = Uuid::from_u128(7);
        let mut st = WsState::fresh(None);
        st.from_zero = false;
        st.at_head = true;
        // No graph held at all → the graph's own sentence.
        let env = answer(&control, ws, &st, Uuid::from_u128(1), 1, false, 10);
        assert!(env.related.is_none());
        assert!(
            env.absent["related"].contains("note graph"),
            "{:?}",
            env.absent
        );
        // Graph held, artifact never asked for.
        control
            .save_graph(ws, st.epoch, st.cursor, &WireGraph::default())
            .unwrap();
        let env = answer(&control, ws, &st, Uuid::from_u128(1), 1, false, 10);
        assert_eq!(env.absent["related"], NOT_SYNCED);
        // Asked, and the server served none.
        st.related_asked = true;
        let env = answer(&control, ws, &st, Uuid::from_u128(1), 1, false, 10);
        assert_eq!(env.absent["related"], NOT_SERVED);
        // Held, but the subject postdates it.
        control
            .save_related(
                ws,
                &WireRelated {
                    covers_rev: 5,
                    covers_id: Uuid::from_u128(9),
                    dict: vec![],
                    subjects: vec![],
                },
            )
            .unwrap();
        let env = answer(&control, ws, &st, Uuid::from_u128(1), 6, false, 10);
        assert_eq!(env.absent["related"], SUBJECT_MISSING);
        assert_eq!(env.covers_rev, Some(5));
    }

    /// The two disclosures compare server-side numbers: the subject's rev against the
    /// generation, and the observed head against the generation — never the mirror's cursor.
    #[test]
    fn disclosures_compare_server_side_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let control = ControlRoot::new(tmp.path());
        let ws = Uuid::from_u128(7);
        let id = Uuid::from_u128(1);
        let mut st = WsState::fresh(None);
        st.from_zero = false;
        st.at_head = true;
        st.related_asked = true;
        st.head_rev = Some(12);
        control
            .save_graph(
                ws,
                st.epoch,
                st.cursor,
                &WireGraph {
                    nodes: vec![GraphNode {
                        id,
                        kind: "file".into(),
                        name: "a.md".into(),
                        path: "a.md".into(),
                        title: None,
                        aliases: vec![],
                        mime: None,
                        content_bytes: 1,
                        trashed: false,
                    }],
                    edges: vec![],
                    tags: vec![],
                },
            )
            .unwrap();
        control
            .save_related(
                ws,
                &WireRelated {
                    covers_rev: 10,
                    covers_id: Uuid::from_u128(9),
                    dict: vec![],
                    subjects: vec![RelatedSubject {
                        id,
                        note: Some(RelatedVector {
                            wc: 200,
                            terms: vec![],
                        }),
                        graph: vec![],
                        tags: vec![],
                    }],
                },
            )
            .unwrap();
        // Subject at rev 10 (unchanged), head at 12 → only the head distance is disclosed.
        let env = answer(&control, ws, &st, id, 10, false, 10);
        assert_eq!(
            env.related.as_deref(),
            Some(&[][..]),
            "an empty answer is a real answer"
        );
        let codes: Vec<&str> = env.disclosures.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec!["workspace_moved"]);
        assert!(env.disclosures[0].message.contains("2 revisions"));
        // Subject at rev 11 → the subject changed too; a scoped mount adds the third.
        let env = answer(&control, ws, &st, id, 11, true, 10);
        let codes: Vec<&str> = env.disclosures.iter().map(|d| d.code).collect();
        assert_eq!(
            codes,
            vec![
                "subject_changed",
                "workspace_moved",
                "graph_wider_than_mount"
            ]
        );
        // Head AT the generation: zero distance, nothing to disclose.
        st.head_rev = Some(10);
        let env = answer(&control, ws, &st, id, 10, false, 10);
        assert!(env.disclosures.is_empty(), "{:?}", env.disclosures);
    }
}
