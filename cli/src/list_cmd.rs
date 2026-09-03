// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli list` (0.1.1) — every workspace this account can reach, and which of them this
//! project mirrors.
//!
//! The list existed before, buried inside `docli init`'s output, where it was only reachable
//! by running a setup command you might not want to run. It is its own verb now, and it
//! answers the question people actually have in a project directory: *which of these am I
//! working with here?* Mounted workspaces are marked `*` and bold, with the local directory
//! beside them.
//!
//! The whole screen is the product, so it goes to stdout (`ui::report_mode`), and `--json`
//! gives the same data for scripts. Nothing here prompts, so it behaves identically in a
//! terminal and in a pipe.

use std::path::Path;

use anyhow::Result;
use console::style;
use serde::Serialize;

use crate::config;
use crate::http::Api;
use crate::ui;

#[derive(Serialize)]
pub struct Row {
    pub id: String,
    pub handle: String,
    pub name: String,
    /// Is this workspace mounted in THIS project?
    pub mounted: bool,
    /// Where — but only when the USER chose the directory. A DERIVED mount lives in the
    /// per-machine cache, and printing that hands out an address nobody needs: it is one level
    /// from the credentials, and `docli status` stopped printing it for exactly that reason
    /// after the v0.29.1 live-agent gate watched an agent take the path out of a report and grep
    /// the mirror. An explicit `dir` is different — it is the user's own choice and already
    /// sits in the committed `docli.toml`.
    ///
    /// `docli doctor` still prints real paths: reconciling the filesystem is its job.
    pub mounted_at: Option<String>,
    pub folder: Option<String>,
}

pub fn rows(cwd: &Path, api: &Api, server: &str) -> Result<Vec<Row>> {
    let spaces = api.workspaces()?;
    // A docli.toml that will not parse must SAY so: swallowing the error renders every row as
    // «not mounted here», which is a confident wrong answer to the one question this command
    // exists to answer.
    let project = match config::find_project(cwd) {
        Some(root) => Some(config::load_project(&root)?),
        None => None,
    };
    // …and mounts are only joined when the project belongs to the SERVER being listed. Workspace
    // ids are server-scoped, so against a staging server cloned from production the same id
    // exists on both, and marking it «mounted here» attributes production's mirror to staging.
    let project = project.filter(|p| p.config.server.trim_end_matches('/') == server);
    Ok(spaces
        .into_iter()
        .map(|w| {
            let mount = project
                .as_ref()
                .and_then(|p| p.config.mounts.iter().find(|m| m.workspace == w.id));
            Row {
                id: w.id.to_string(),
                // Both sources are sanitized/refused at INGESTION (`http.rs` for server text,
                // `config::load_project` for the committed file), so nothing here needs escaping.
                handle: w.handle,
                name: w.name,
                mounted: mount.is_some(),
                mounted_at: mount.filter(|m| !m.derived_dir).map(|m| m.dir.clone()),
                folder: mount.and_then(|m| m.folder.clone()),
            }
        })
        .collect())
}

pub fn run(cwd: &Path, api: &Api, server: &str, json: bool) -> Result<i32> {
    if json {
        ui::machine_mode();
    } else {
        ui::report_mode();
    }
    let rows = rows(cwd, api, server)?;
    render(rows, json)
}

/// The listing WITHOUT touching global output mode — what `docli init` embeds. `run` sets report
/// mode process-wide, and `init` calling it left every later line (the MCP wiring progress) on
/// stdout and printing through `--quiet`.
/// One rendered row. Pure, so the D1 rule below can be asserted on the STRING a reader sees
/// rather than on the fields that feed it.
fn row_line(r: &Row, hw: usize, nw: usize) -> String {
    // The marker column is TWO characters wide for every row, mounted or not, so the handles
    // stay in one column and the eye can run down them.
    let handle = format!("@{}", r.handle);
    let (mark, handle) = if r.mounted {
        ("*", style(format!("{handle:<hw$}")).bold().to_string())
    } else {
        (" ", format!("{handle:<hw$}"))
    };
    let mut line = format!(
        "{mark} {handle}  {:<nw$}  {}",
        r.name,
        ui::dim(&r.id),
        nw = nw
    );
    if r.mounted {
        let scope = match &r.folder {
            Some(f) => format!(" ({f})"),
            None => String::new(),
        };
        // The folder SCOPE is a property of the mount and worth seeing; the directory is an
        // address and is shown only when the user picked it.
        let tail = match &r.mounted_at {
            Some(dir) => format!("{} {dir}{scope}", ui::arrow()),
            None if scope.is_empty() => String::new(),
            None => format!("{}{scope}", ui::arrow()),
        };
        if !tail.is_empty() {
            line.push_str(&format!("  {}", ui::dim(&tail)));
        }
    }
    line
}

pub fn render_rows(cwd: &Path, api: &Api, server: &str) -> Result<i32> {
    let rows = rows(cwd, api, server)?;
    render(rows, false)
}

fn render(rows: Vec<Row>, json: bool) -> Result<i32> {
    // The exit code must not depend on the OUTPUT FORMAT: an account with no reachable
    // workspaces is the same answer whether a human or a script asked.
    let empty = rows.is_empty();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(if empty { 1 } else { 0 });
    }
    if empty {
        ui::warn("This account has no workspaces you can reach.");
        return Ok(1);
    }
    ui::result_heading("Workspaces");
    // Column widths measured in CHARACTERS — handles are ASCII but names are not.
    let hw = rows
        .iter()
        .map(|r| r.handle.chars().count() + 1)
        .max()
        .unwrap_or(0);
    let nw = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    for r in &rows {
        ui::line(&row_line(r, hw, nw));
    }
    let mounted = rows.iter().filter(|r| r.mounted).count();
    if mounted == 0 {
        ui::next(&format!(
            "Nothing is mounted here - set it up: {}",
            ui::cmd("docli init")
        ));
    } else {
        ui::detail(&format!(
            "* - mirrored in this project ({mounted} of {})",
            rows.len()
        ));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(mounted: bool, at: Option<&str>, folder: Option<&str>) -> Row {
        Row {
            id: "cd2f1093-4219-4d68-8d2c-dfe7d5125b72".into(),
            handle: "docli".into(),
            name: "docli".into(),
            mounted,
            mounted_at: at.map(Into::into),
            folder: folder.map(Into::into),
        }
    }

    /// D1's rule, asserted on `list` because it was MISSED here once. `status` stopped printing
    /// the cache directory after the v0.29.1 live-agent gate watched an agent take the path out
    /// of a report and grep the mirror; `list` went on printing the absolute derived path — one
    /// level from the credentials — which made the contract's silence about that directory a
    /// half-measure.
    ///
    /// A DERIVED mount is ours and its location is not an address to hand out. An explicit one
    /// is the user's own and already sits in the committed `docli.toml`.
    #[test]
    fn a_derived_mount_publishes_no_directory_and_an_explicit_one_does() {
        let derived = row_line(&row(true, None, None), 8, 8);
        assert!(
            !derived.contains(".docli"),
            "a derived mount must not print its cache path: {derived}"
        );
        assert!(
            derived.contains('*'),
            "…but it is still marked mounted: {derived}"
        );

        // The folder SCOPE survives — it describes the mount, it is not a path.
        let scoped = row_line(&row(true, None, Some("docs")), 8, 8);
        assert!(scoped.contains("(docs)"), "{scoped}");
        assert!(!scoped.contains(".docli"), "{scoped}");

        let explicit = row_line(&row(true, Some("mirror"), None), 8, 8);
        assert!(explicit.contains("mirror"), "{explicit}");

        let unmounted = row_line(&row(false, None, None), 8, 8);
        assert!(!unmounted.contains('*'), "{unmounted}");
    }
}
