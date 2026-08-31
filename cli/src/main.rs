// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The `docli` binary — argument parsing + dispatch; everything real lives in the lib.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use docli_cli::{config, creds, doctor, http, init_cmd, login, search_cmd, selfupdate, sync_cmd};

/// D12.5 — the identity block: name, version, site, copyright. `-V` prints the short line,
/// `--version` the full block; help carries the site + copyright in the footer.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nDocli CLI \u{2014} https://docli.ru\n\u{a9} 2026 OOO Agitek. MIT License."
);
const AFTER_HELP: &str =
    "Docli CLI \u{b7} https://docli.ru \u{b7} \u{a9} 2026 OOO Agitek \u{b7} MIT License";

#[derive(Parser)]
#[command(
    name = "docli",
    version,
    long_version = LONG_VERSION,
    about = "Docli CLI \u{2014} read-only workspace mirrors for coding agents",
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign this device in (loopback OAuth; opens a browser)
    Login {
        /// The docli server URL (default: docli.toml's `server`, otherwise https://docli.ru)
        #[arg(long)]
        server: Option<String>,
    },
    /// Create or extend docli.toml and install the agent SKILL.md
    Init {
        /// Workspace ID to mount (see the list `docli init` prints)
        #[arg(long)]
        workspace: Option<uuid::Uuid>,
        /// Local mount directory (relative to docli.toml)
        #[arg(long)]
        dir: Option<String>,
        /// Optional server folder scope (mirror only this subtree)
        #[arg(long)]
        folder: Option<String>,
        /// Display name for this mount (shown in refusal messages)
        #[arg(long)]
        name: Option<String>,
        /// The docli server URL for a new docli.toml
        #[arg(long)]
        server: Option<String>,
        /// Add this project's MCP connection to agent configurations: `auto` (detected
        /// agents), `none`, or a comma-separated list
        /// (claude,codex,gemini,cursor,vscode,opencode,qwen,cline,trae,zed,windsurf,sourcecraft,junie,amp)
        #[arg(long)]
        mcp: Option<String>,
        /// Connection label for the MCP URL (default: derived from the directory name)
        #[arg(long)]
        mcp_label: Option<String>,
        /// Wire the bare unlabeled MCP URL (for clients that don't send RFC 8707 `resource`)
        #[arg(long, conflicts_with = "mcp_label")]
        mcp_bare: bool,
    },
    /// Pull every mount to the server's head (one-shot; never pushes)
    Sync {
        /// Cheap freshness gate: exit 0 confirms freshness; non-zero means freshness was not confirmed
        #[arg(long, conflicts_with = "full")]
        check: bool,
        /// Rebuild the mirror from server state and prune stale files
        #[arg(long)]
        full: bool,
    },
    /// Server search across all mounts (results carry local paths)
    Search {
        /// The query (docli's BM25 + RU/EN stemming)
        query: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Three-way reconciliation of server, disk, and state (read-only)
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Update this binary from the signed release manifest
    SelfUpdate,
}

fn resolve_server(explicit: Option<&str>, cwd: &std::path::Path) -> String {
    if let Some(s) = explicit {
        return s.trim_end_matches('/').to_string();
    }
    config::find_project(cwd)
        .and_then(|root| config::load_project(&root).ok())
        .map(|p| p.config.server)
        .unwrap_or_else(|| "https://docli.ru".to_string())
}

fn main() {
    selfupdate::cleanup_stale_binary();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("docli: {e:#}");
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    match cli.command {
        Command::Login { server } => {
            let server = resolve_server(server.as_deref(), &cwd);
            let creds = creds::CredsStore::open_default()?;
            login::run_login(&server, &creds)?;
            Ok(0)
        }
        Command::Init {
            workspace,
            dir,
            folder,
            name,
            server,
            mcp,
            mcp_label,
            mcp_bare,
        } => {
            let origin = resolve_server(server.as_deref(), &cwd);
            let api = creds::CredsStore::open_default().ok().and_then(|c| {
                c.get(&origin).ok().flatten()?;
                http::Api::new(&origin, c).ok()
            });
            init_cmd::run(
                &cwd,
                api.as_ref(),
                &init_cmd::InitArgs {
                    workspace,
                    dir,
                    folder,
                    name,
                    server,
                    mcp,
                    mcp_label,
                    mcp_bare,
                    allow_prompt: true,
                },
            )
        }
        Command::Sync { check, full } => {
            let project = config::load_project(&cwd)?;
            let api = api_for(&project)?;
            sync_cmd::run(&project, &api, &sync_cmd::SyncOptions { check, full })
        }
        Command::Search { query, json } => {
            let project = config::load_project(&cwd)?;
            let api = api_for(&project)?;
            let q = query.join(" ");
            if q.trim().is_empty() {
                anyhow::bail!("usage: docli search <query>");
            }
            search_cmd::run(&project, &api, &q, json)
        }
        Command::Doctor { json } => {
            let project = config::load_project(&cwd)?;
            let api = api_for(&project)?;
            doctor::run(&project, &api, json)
        }
        Command::SelfUpdate => selfupdate::run(),
    }
}

fn api_for(project: &config::Project) -> Result<http::Api> {
    let creds = creds::CredsStore::open_default()?;
    if creds.get(&project.config.server)?.is_none() {
        anyhow::bail!(
            "not signed in to {} — run `docli login`",
            project.config.server
        );
    }
    http::Api::new(&project.config.server, creds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_help_carry_the_identity_block() {
        // D12.5: name, version, site, copyright — on both --version and --help.
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let long = cmd.render_long_version().to_string();
        assert!(long.contains(env!("CARGO_PKG_VERSION")), "{long}");
        assert!(long.contains("Docli CLI"), "{long}");
        assert!(long.contains("docli.ru"), "{long}");
        assert!(long.contains("\u{a9} 2026 OOO Agitek"), "{long}");
        assert!(long.contains("MIT"), "{long}");
        let help = cmd.render_long_help().to_string();
        assert!(help.contains("docli.ru"), "{help}");
        assert!(help.contains("OOO Agitek"), "{help}");
    }

    #[test]
    fn the_mcp_help_list_names_every_agent_key() {
        // The --mcp help text hand-writes the key list (clap wants a static str); this pin
        // makes adding a table entry without updating it a test failure, not silent drift.
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let init = cmd
            .find_subcommand_mut("init")
            .expect("init exists")
            .render_long_help()
            .to_string();
        // The EXACT joined list must appear (R17): additions AND removals both fail — a
        // one-directional contains() would let a removed table entry leave stale help.
        let joined = docli_cli::agents::AGENTS
            .iter()
            .map(|a| a.key)
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            init.contains(&joined),
            "--mcp help must carry the exact key list {joined:?}"
        );
    }
}
