// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli ls [folder]`, `docli tree`, `docli tags`, `docli tagged <tag>` (v0.29.9 Part B) — the
//! MCP reads `list_notes`, `read_vault`, `list_tags` and `notes_by_tag`, answered from the held
//! workspace graph.
//!
//! Every row comes from the graph the server computed, so the answer is complete for the WHOLE
//! workspace even under a folder-scoped mount; rows this mirror does not hold are MARKED and the
//! remedy named (widen the mount, or `read_note` over MCP). None of this is an absence authority:
//! `docli search` stays the only thing that establishes a note does not exist.
//!
//! Like `list` and `status`, the whole screen is the product (`ui::report_mode`), and `--json`
//! gives the same rows for scripts.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;
use uuid::Uuid;

use crate::config::{validate_config, Mount, Project};
use crate::graph::Graph;
use crate::read_cmd::{graph_slot, select_mounts, GraphSlot};
use crate::ui;

pub enum Verb {
    Ls { folder: Option<String> },
    Tree,
    Tags,
    Tagged { tag: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Does THIS mirror hold the node? False for a node outside the mount's folder scope, a
    /// kind the mirror does not store, or a delivery that was parked.
    pub mirrored: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagRow {
    pub tag: String,
    pub notes: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountAnswer {
    pub mount: String,
    pub workspace: Uuid,
    /// Absent when the graph is not held; `absent` then names why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Row>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent: Option<String>,
    /// `read`'s `mirror_not_usable` disclosure, when `WsState::unusable_reason` names one — the
    /// rows are a projection of this mirror and share its currency.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disclosures: Vec<crate::read_cmd::Disclosure>,
}

pub fn run(project: &Project, verb: &Verb, mount: Option<&str>, json: bool) -> Result<i32> {
    if json {
        ui::machine_mode();
    } else {
        ui::report_mode();
    }
    validate_config(&project.config)?;
    let mounts = match select_mounts(project, mount) {
        Ok(m) => m,
        Err(r) => return Ok(crate::read_cmd::render_refusal(&r, json)),
    };
    let control = project.control_root();
    let answers: Vec<MountAnswer> = mounts.iter().map(|m| answer(&control, m, verb)).collect();
    // The exit code answers the same question in both modes (the `list`/`status` rule): 1 when a
    // mount could not be listed (never synced, no graph held), for a script as for a person.
    let exit = if answers.iter().any(|a| a.absent.is_some()) {
        1
    } else {
        0
    };
    if json {
        return Ok(crate::read_cmd::write_json(&answers, exit));
    }
    let show_mount = project.config.mounts.len() > 1;
    for a in &answers {
        render(a, verb, show_mount);
    }
    Ok(exit)
}

fn answer(control: &crate::state::ControlRoot, m: &Mount, verb: &Verb) -> MountAnswer {
    let mut out = MountAnswer {
        mount: m.display_name().to_string(),
        workspace: m.workspace,
        rows: None,
        tags: None,
        absent: None,
        disclosures: Vec::new(),
    };
    let st = match control.load_state(m.workspace) {
        Ok(Some(st)) => st,
        Ok(None) => {
            out.absent = Some("this mount has never been synced - run `docli sync`".into());
            return out;
        }
        Err(e) => {
            out.absent = Some(format!(
                "the local mirror state could not be read ({e:#}) - `docli sync --full` rebuilds it"
            ));
            return out;
        }
    };
    let graph = match graph_slot(control, m.workspace, &st) {
        GraphSlot::Held(g) => g,
        GraphSlot::Absent(why) => {
            out.absent = Some(why.to_string());
            return out;
        }
    };
    if let Some(reason) = st.unusable_reason(m.folder.as_deref(), crate::sync_cmd::now_unix()) {
        out.disclosures
            .push(crate::related_cmd::mirror_not_usable(reason));
    }
    // «No such folder» and «an empty folder» are different answers, and the graph can tell them
    // apart: it holds the folder nodes. Said as an absence (exit 1), not as `nothing here`.
    if let Verb::Ls {
        folder: Some(folder),
    } = verb
    {
        let wanted = folder.trim_matches('/');
        let exists = graph
            .live_nodes()
            .any(|n| n.kind == "folder" && n.path == wanted);
        if !wanted.is_empty() && !exists {
            out.absent = Some(format!(
                "no folder `{}` in the held graph (paths are exact; `docli tree` lists them)",
                ui::sanitize(wanted)
            ));
            return out;
        }
    }
    let mirrored = |id: Uuid| st.nodes.contains_key(&id);
    match verb {
        Verb::Tags => out.tags = Some(tag_rows(&graph)),
        _ => out.rows = Some(rows(&graph, verb, mirrored)),
    }
    out
}

/// The live rows a verb selects, in byte order of path — the same ordering rule the graph module
/// states (the server sorts under its collation, which a client cannot reproduce; the SET is the
/// contract).
fn rows(graph: &Graph, verb: &Verb, mirrored: impl Fn(Uuid) -> bool) -> Vec<Row> {
    let mut out: Vec<Row> = graph
        .live_nodes()
        .filter(|n| match verb {
            Verb::Ls { folder } => {
                parent_of(&n.path) == folder.as_deref().unwrap_or("").trim_matches('/')
            }
            Verb::Tree => true,
            Verb::Tagged { tag } => graph.tags(n.id).iter().any(|t| t == tag),
            Verb::Tags => false,
        })
        .map(|n| Row {
            id: n.id,
            kind: n.kind.clone(),
            name: n.name.clone(),
            path: n.path.clone(),
            tags: match verb {
                Verb::Tagged { .. } => graph.tags(n.id),
                _ => Vec::new(),
            },
            mirrored: mirrored(n.id),
        })
        .collect();
    // By path COMPONENTS, not by the whole string: `a/child.md` must precede the sibling
    // directory `a-b/` (`'/'` sorts after `'-'`), or the tree's indentation shows the child under
    // the wrong parent (Loop B, Codex round 1). Still byte order within a component.
    out.sort_by(|a, b| {
        a.path
            .split('/')
            .collect::<Vec<_>>()
            .cmp(&b.path.split('/').collect::<Vec<_>>())
    });
    out
}

/// Tags with their live-note counts, most-used first then tag-ascending — MCP `list_tags`'s
/// order (`count(*) DESC, tag ASC`), so the two surfaces read alike.
fn tag_rows(graph: &Graph) -> Vec<TagRow> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in graph.live_nodes() {
        for t in graph.tags(n.id) {
            *counts.entry(t).or_insert(0) += 1;
        }
    }
    let mut out: Vec<TagRow> = counts
        .into_iter()
        .map(|(tag, notes)| TagRow { tag, notes })
        .collect();
    out.sort_by(|a, b| b.notes.cmp(&a.notes).then_with(|| a.tag.cmp(&b.tag)));
    out
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

fn render(a: &MountAnswer, verb: &Verb, show_mount: bool) {
    if show_mount {
        ui::result_heading(&format!("[{}]", ui::sanitize(&a.mount)));
    }
    for d in &a.disclosures {
        ui::warn(&d.message);
    }
    if let Some(why) = &a.absent {
        ui::warn(why);
        return;
    }
    if let Some(tags) = &a.tags {
        if tags.is_empty() {
            ui::detail("no tags");
        }
        let w = tags
            .iter()
            .map(|t| t.tag.chars().count())
            .max()
            .unwrap_or(0);
        for t in tags {
            ui::line(&format!(
                "{:<w$}  {}",
                ui::sanitize(&t.tag),
                ui::dim(&ui::plural(t.notes, "note", "notes"))
            ));
        }
        return;
    }
    let rows = a.rows.as_deref().unwrap_or(&[]);
    if rows.is_empty() {
        ui::detail("nothing here");
        return;
    }
    let mut unmirrored = 0usize;
    for r in rows {
        let shown = match verb {
            // A tree indents by depth and shows the leaf; everything else shows the full path,
            // which is the address `docli read` takes.
            Verb::Tree => {
                let depth = r.path.matches('/').count();
                format!("{}{}", "  ".repeat(depth), ui::sanitize(&r.name))
            }
            _ => ui::sanitize(&r.path),
        };
        let kind = match r.kind.as_str() {
            "folder" => "/",
            "attachment" => "  [file]",
            _ => "",
        };
        let mark = if r.mirrored {
            String::new()
        } else {
            unmirrored += 1;
            format!("  {}", ui::dim("(not mirrored here)"))
        };
        let tags = if r.tags.is_empty() {
            String::new()
        } else {
            format!("  {}", ui::dim(&format!("#{}", r.tags.join(" #"))))
        };
        ui::line(&format!("{shown}{kind}{tags}{mark}"));
    }
    if unmirrored > 0 {
        ui::detail(&format!(
            "{} not mirrored here (outside the mount's folder scope, or a kind this mirror does \
             not store) - widen the mount with `docli init --folder`, or read them with \
             `read_note` over the docli MCP connection",
            ui::plural(unmirrored, "row", "rows")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docli_sync_wire::{GraphNode, GraphTag, WireGraph};

    /// The exit code is the same question in both modes: a mount whose graph is not held is 1 for
    /// a script exactly as for a person (round 2 of the v0.29.9 review found `--json` answering 0).
    #[test]
    fn the_exit_code_does_not_depend_on_the_output_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("mirror")).unwrap();
        let project = Project {
            root: root.clone(),
            config: crate::config::DocliToml {
                server: "https://docli.ru".into(),
                mounts: vec![Mount {
                    workspace: Uuid::from_u128(1),
                    dir: "mirror".into(),
                    folder: None,
                    name: Some("m".into()),
                    derived_dir: false,
                    workspace_label: String::new(),
                }],
                mcp_label: None,
            },
            control: root.join(".docli"),
        };
        // Never synced ⇒ absent ⇒ 1, whichever way it is rendered.
        let plain = run(&project, &Verb::Tree, None, false).unwrap();
        let machine = run(&project, &Verb::Tree, None, true).unwrap();
        assert_eq!((plain, machine), (1, 1));
    }

    fn node(n: u128, kind: &str, path: &str, trashed: bool) -> GraphNode {
        GraphNode {
            id: Uuid::from_u128(n),
            kind: kind.into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            title: None,
            aliases: vec![],
            mime: None,
            content_bytes: 0,
            trashed,
        }
    }

    fn graph() -> Graph {
        Graph::new(WireGraph {
            nodes: vec![
                node(1, "folder", "docs", false),
                node(2, "file", "docs/a.md", false),
                node(3, "file", "docs/b.md", false),
                node(4, "file", "root.md", false),
                node(5, "file", "docs/gone.md", true),
                node(6, "attachment", "docs/p.png", false),
            ],
            edges: vec![],
            // Indices into `nodes`: 1 = docs/a.md, 2 = docs/b.md, 3 = root.md, 4 = the trashed
            // docs/gone.md.
            tags: vec![
                GraphTag {
                    node: 1,
                    tag: "work".into(),
                },
                GraphTag {
                    node: 2,
                    tag: "work".into(),
                },
                GraphTag {
                    node: 2,
                    tag: "проект".into(),
                },
                GraphTag {
                    node: 3,
                    tag: "work".into(),
                },
                GraphTag {
                    node: 4,
                    tag: "work".into(),
                },
            ],
        })
    }

    #[test]
    fn ls_lists_a_folders_live_children_and_marks_what_is_not_mirrored() {
        let g = graph();
        let held = |id: Uuid| id != Uuid::from_u128(3);
        let docs = rows(
            &g,
            &Verb::Ls {
                folder: Some("docs/".into()),
            },
            held,
        );
        let paths: Vec<&str> = docs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["docs/a.md", "docs/b.md", "docs/p.png"],
            "no trashed, no root"
        );
        assert!(docs[0].mirrored && !docs[1].mirrored);
        let root = rows(&g, &Verb::Ls { folder: None }, held);
        let paths: Vec<&str> = root.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["docs", "root.md"]);
    }

    /// `a/child.md` before `a-b/`: whole-string byte order puts `'/'` after `'-'` and would show
    /// the child under the wrong parent in the indented tree.
    #[test]
    fn rows_order_by_path_components_so_a_tree_nests_correctly() {
        let g = Graph::new(WireGraph {
            nodes: vec![
                node(1, "folder", "a", false),
                node(2, "folder", "a-b", false),
                node(3, "file", "a/child.md", false),
                node(4, "file", "a-b/other.md", false),
            ],
            edges: vec![],
            tags: vec![],
        });
        let paths: Vec<String> = rows(&g, &Verb::Tree, |_| true)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, vec!["a", "a/child.md", "a-b", "a-b/other.md"]);
    }

    #[test]
    fn tree_tags_and_tagged_answer_from_the_graph() {
        let g = graph();
        let all = rows(&g, &Verb::Tree, |_| true);
        assert_eq!(all.len(), 5, "every live node, trashed excluded");
        let tags = tag_rows(&g);
        assert_eq!(tags.len(), 2);
        // Most-used first (`list_tags`'s order): `work` sits on a.md, b.md and root.md — the
        // trashed gone.md carries none.
        assert_eq!((tags[0].tag.as_str(), tags[0].notes), ("work", 3));
        assert_eq!((tags[1].tag.as_str(), tags[1].notes), ("проект", 1));
        let tagged = rows(&g, &Verb::Tagged { tag: "work".into() }, |_| true);
        let paths: Vec<&str> = tagged.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["docs/a.md", "docs/b.md", "root.md"]);
        assert_eq!(tagged[1].tags, vec!["work", "проект"]);
    }
}
