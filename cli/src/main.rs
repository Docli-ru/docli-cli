// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The `docli` binary — argument parsing + dispatch; everything real lives in the lib.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use docli_cli::{
    config, creds, doctor, http, init_cmd, list_cmd, login, logout, search_cmd, selfupdate, status,
    sync_cmd, ui, uninstall, wizard,
};

/// D12.5 - the identity block: name, version, site, copyright. `-V` prints the short line,
/// `--version` the full block; help carries the site + copyright in the footer.
const LONG_VERSION_UNICODE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nDocli CLI \u{2014} https://docli.ru\n\u{a9} 2026 OOO Agitek. MIT License."
);
const LONG_VERSION_ASCII: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nDocli CLI - https://docli.ru\n(c) 2026 OOO Agitek. MIT License."
);
const AFTER_HELP_UNICODE: &str =
    "Docli CLI \u{b7} https://docli.ru \u{b7} \u{a9} 2026 OOO Agitek \u{b7} MIT License";
const AFTER_HELP_ASCII: &str = "Docli CLI | https://docli.ru | (c) 2026 OOO Agitek | MIT License";
const ABOUT_UNICODE: &str =
    "Docli CLI \u{2014} read-only docli workspace mirrors for coding agents";
const ABOUT_ASCII: &str = "Docli CLI - read-only docli workspace mirrors for coding agents";

#[derive(Parser)]
#[command(
    name = "docli",
    version,
    long_version = LONG_VERSION_ASCII,
    about = ABOUT_ASCII,
    after_help = AFTER_HELP_ASCII,
    // clig.dev: show help when run with no arguments, rather than a bare parse error.
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Print less: drops the narration, never results or warnings
    #[arg(long, short = 'q', global = true)]
    quiet: bool,
    /// No colour (NO_COLOR, TERM=dumb and a non-TTY stdout do the same)
    #[arg(long, global = true)]
    no_color: bool,
    /// Never prompt - for scripts and CI
    #[arg(long, global = true)]
    no_input: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Sign this device in (browser OAuth)
    Login {
        /// docli server URL (default: docli.toml's `server`, else https://docli.ru)
        #[arg(long)]
        server: Option<String>,
    },
    /// Set the project up - guided, or by flags (docli.toml + the agent SKILL.md)
    Init {
        /// Workspace ID to mount (with no flags at a terminal, the guided setup runs)
        #[arg(long)]
        workspace: Option<uuid::Uuid>,
        /// Mirror directory, relative to docli.toml
        #[arg(long)]
        dir: Option<String>,
        /// Mirror only this folder of the workspace
        #[arg(long)]
        folder: Option<String>,
        /// Display name for this mount - what refusals report
        #[arg(long)]
        name: Option<String>,
        /// docli server URL for docli.toml
        #[arg(long)]
        server: Option<String>,
        /// Add this project's MCP connection to agent configurations: `auto` (detected
        /// here), `none`, or a comma-separated list
        /// (claude,codex,gemini,cursor,vscode,opencode,qwen,cline,trae,zed,windsurf,sourcecraft,junie,amp)
        #[arg(long)]
        mcp: Option<String>,
        /// Connection label for the MCP URL (default: derived from the directory name)
        #[arg(long)]
        mcp_label: Option<String>,
        /// Wire the unlabeled MCP URL (for clients that omit RFC 8707 `resource`)
        #[arg(long, conflicts_with = "mcp_label")]
        mcp_bare: bool,
        /// Drop the folder scope - mirror the whole workspace
        #[arg(long, conflicts_with = "folder")]
        clear_folder: bool,
        /// Append the missing .gitignore lines (otherwise they are only named)
        #[arg(long)]
        gitignore: bool,
    },
    /// Bring every mount to the server's head (one-shot; never pushes)
    Sync {
        /// Freshness gate only: exit 0 confirms the mirror is current
        #[arg(long, conflicts_with = "full")]
        check: bool,
        /// Rebuild the mirror from server state and prune stale files
        #[arg(long)]
        full: bool,
    },
    /// Server search across all mounts (results carry local paths)
    Search {
        /// The query
        #[arg(value_name = "QUERY")]
        query: Vec<String>,
        /// Machine-readable output (JSON)
        #[arg(long)]
        json: bool,
    },
    /// Three-way reconciliation of server, disk and state (read-only)
    Doctor {
        /// Machine-readable output (JSON)
        #[arg(long)]
        json: bool,
    },
    /// Update this binary from the signed release manifest
    SelfUpdate,
    /// Disconnect this device and drop its credentials
    Logout {
        /// Server URL (default: docli.toml's `server`)
        #[arg(long)]
        server: Option<String>,
        /// Log out of every server this device is signed in to
        #[arg(long, conflicts_with = "server")]
        all: bool,
    },
    /// List every workspace; the ones mounted here are marked *
    List {
        /// Server URL (default: docli.toml's `server`)
        #[arg(long)]
        server: Option<String>,
        /// Machine-readable output (JSON)
        #[arg(long)]
        json: bool,
    },
    /// Who you are, what is mounted, and how fresh the mirrors are
    Status {
        /// Server URL (default: docli.toml's `server`)
        #[arg(long)]
        server: Option<String>,
        /// Machine-readable output (JSON)
        #[arg(long)]
        json: bool,
    },
    /// Remove docli from this device
    Uninstall {
        /// Also remove this project's mirrors and .docli/
        #[arg(long)]
        purge: bool,
        /// Skip the confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

fn resolve_server(explicit: Option<&str>, cwd: &std::path::Path) -> Result<String> {
    if let Some(s) = explicit {
        return Ok(s.trim_end_matches('/').to_string());
    }
    // A docli.toml that will not parse must SAY so, not quietly fall back to production. The
    // fallback made `docli logout` in a self-hosted project revoke the PRODUCTION credential
    // and report success while the intended one stayed live.
    match config::find_project(cwd) {
        Some(root) => Ok(config::load_project(&root)?.config.server),
        None => Ok("https://docli.ru".to_string()),
    }
}

/// Colour has to be settled BEFORE clap runs: `--help`, a bare invocation and any parse error
/// are rendered and exited from inside `get_matches`, long before `ui::configure` would see the
/// flag. Reading argv directly is the only order that works. (`NO_COLOR`, `CLICOLOR` and the
/// non-TTY case are handled by `console` itself, and need no help here.)
fn preconfigure_color() {
    if std::env::args().any(|a| a == "--no-color") {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }
}

/// Ctrl-C at a prompt is an ANSWER, not a crash: `dialoguer` reports it as an interrupted I/O
/// error, and printing `docli: operation interrupted` under it reads as a bug in the tool. The
/// shell convention is a silent exit with 128+SIGINT.
fn interrupted(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::Interrupted)
    })
}

fn main() {
    preconfigure_color();
    selfupdate::cleanup_stale_binary();
    // Extended characters where the terminal can render them; the derive carries the ASCII
    // spelling so a plain `--help` is safe even if this swap is ever skipped.
    let cli = {
        use clap::{CommandFactory, FromArgMatches};
        let mut cmd = Cli::command();
        // clap keeps its OWN `ColorChoice`, which `console::set_colors_enabled` does not reach:
        // without this, `docli --no-color --help` and parse errors stayed styled on a TTY.
        if std::env::args().any(|a| a == "--no-color") {
            cmd = cmd.color(clap::ColorChoice::Never);
        }
        if ui::unicode() {
            cmd = cmd
                .about(ABOUT_UNICODE)
                .long_version(LONG_VERSION_UNICODE)
                .after_help(AFTER_HELP_UNICODE);
        }
        match Cli::from_arg_matches(&cmd.get_matches()) {
            Ok(c) => c,
            Err(e) => e.exit(),
        }
    };
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if interrupted(&e) {
                std::process::exit(130);
            }
            eprintln!("docli: {e:#}");
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    ui::configure(cli.quiet, cli.no_color, cli.no_input);
    let cwd = std::env::current_dir().context("reading the working directory")?;
    match cli.command {
        Command::Login { server } => {
            let server = resolve_server(server.as_deref(), &cwd)?;
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
            clear_folder,
            gitignore,
        } => {
            let origin = resolve_server(server.as_deref(), &cwd)?;
            let args = init_cmd::InitArgs {
                workspace,
                dir,
                folder,
                name,
                server,
                mcp,
                mcp_label,
                mcp_bare,
                allow_prompt: true,
                clear_folder,
                write_gitignore: gitignore,
            };
            // A bare `docli init` at a terminal is the guided journey; any flag, or a pipe,
            // keeps the scriptable path an agent can drive.
            if wizard::should_run(&args) {
                return wizard::run(&cwd, args.server.as_deref());
            }
            let api = creds::CredsStore::open_default().ok().and_then(|c| {
                c.get(&origin).ok().flatten()?;
                http::Api::new(&origin, c).ok()
            });
            init_cmd::run(&cwd, api.as_ref(), &args)
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
        Command::Logout { server, all } => {
            // `--all` names no project: reading docli.toml for a value it will not use turned a
            // malformed config into «cannot log out at all».
            if all {
                return logout::all();
            }
            let server = resolve_server(server.as_deref(), &cwd)?;
            logout::run(&server, false)
        }
        Command::List { server, json } => {
            // Deliberately NOT `api_for`: listing workspaces is the one thing that has to work
            // before a project exists — that is when people need it most, and `--server` is how
            // a self-hosted origin is named when there is no docli.toml to read it from.
            let server = resolve_server(server.as_deref(), &cwd)?;
            let store = creds::CredsStore::open_default()?;
            if store.get(&server)?.is_none() {
                anyhow::bail!("not signed in to {server} - run `docli login`");
            }
            let api = http::Api::new(&server, store)?;
            list_cmd::run(&cwd, &api, &server, json)
        }
        Command::Status { server, json } => {
            let server = resolve_server(server.as_deref(), &cwd)?;
            status::run(&cwd, &server, json)
        }
        Command::Uninstall { purge, yes } => uninstall::run(&cwd, purge, yes),
    }
}

fn api_for(project: &config::Project) -> Result<http::Api> {
    let creds = creds::CredsStore::open_default()?;
    if creds.get(&project.config.server)?.is_none() {
        anyhow::bail!(
            "not signed in to {} - run `docli login`",
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
        assert!(long.contains("(c) 2026 OOO Agitek"), "{long}");
        assert!(long.contains("MIT"), "{long}");
        let help = cmd.render_long_help().to_string();
        assert!(help.contains("docli.ru"), "{help}");
        assert!(help.contains("OOO Agitek"), "{help}");
    }

    #[test]
    fn the_cli_speaks_one_language_and_it_is_english() {
        // The CLI is a developer tool, and developer tools speak English — the convention this
        // segment already follows (Yandex Cloud's `yc` and Timeweb's own `twc` both ship an
        // English CLI with Russian documentation). It also keeps the output pure ASCII, which
        // is what makes it safe in a terminal that cannot render Cyrillic. The RUSSIAN surface
        // is the product: README.ru.md, the site, the app.
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        let mut rendered = vec![help];
        for name in [
            "init",
            "sync",
            "search",
            "status",
            "list",
            "logout",
            "uninstall",
        ] {
            rendered.push(
                cmd.find_subcommand_mut(name)
                    .unwrap_or_else(|| panic!("{name} exists"))
                    .render_long_help()
                    .to_string(),
            );
        }
        for text in rendered {
            assert!(
                text.is_ascii(),
                "the fallback help must stay ASCII so it renders anywhere:\n{text}"
            );
        }
        // …and the richer spelling is available for terminals that can show it.
        assert!(!ABOUT_UNICODE.is_ascii());
        assert!(ABOUT_UNICODE.contains("read-only docli workspace mirrors"));
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
