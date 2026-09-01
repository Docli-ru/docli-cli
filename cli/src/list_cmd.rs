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
    /// The mount directory in THIS project, when this workspace is mounted here.
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
                mounted_at: mount.map(|m| m.dir.clone()),
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
        // The marker column is TWO characters wide for every row, mounted or not, so the
        // handles stay in one column and the eye can run down them.
        let handle = format!("@{}", r.handle);
        let (mark, handle) = match &r.mounted_at {
            Some(_) => ("*", style(format!("{handle:<hw$}")).bold().to_string()),
            None => (" ", format!("{handle:<hw$}")),
        };
        let mut line = format!(
            "{mark} {handle}  {:<nw$}  {}",
            r.name,
            ui::dim(&r.id),
            nw = nw
        );
        if let Some(dir) = &r.mounted_at {
            let scope = match &r.folder {
                Some(f) => format!(" ({f})"),
                None => String::new(),
            };
            line.push_str(&format!(
                "  {}",
                ui::dim(&format!("{} {dir}{scope}", ui::arrow()))
            ));
        }
        ui::line(&line);
    }
    let mounted = rows.iter().filter(|r| r.mounted_at.is_some()).count();
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
