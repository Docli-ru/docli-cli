// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The `docli` binary — argument parsing + dispatch; everything real lives in the lib.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use docli_cli::{
    config, creds, doctor, guard, hooks, http, init_cmd, list_cmd, login, logout, read_cmd,
    search_cmd, selfupdate, status, sync_cmd, ui, uninstall, wizard,
};

/// D12.5 - the identity block: name, version, site, copyright.
///
/// The NAME comes from clap's `display_name`, not from these strings, so `-V` prints
/// `docli-cli 0.1.3` on one line — the shape the neighbouring tool uses (`codex-cli 0.144.6`) and
/// the reason the hyphen was adopted. `Usage:` still says `docli`, because that is what you type:
/// the product has a name and the command has a spelling, and they are allowed to differ.
///
/// The version is repeated in the HELP footer deliberately. Help output is what people paste into
/// a bug report, and a paste with no version in it costs a round trip to find out.
const LONG_VERSION_UNICODE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nread-only docli workspace mirrors for coding agents",
    "\nhttps://docli.ru \u{b7} \u{a9} 2026 Agitek \u{b7} MIT License"
);
const LONG_VERSION_ASCII: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nread-only docli workspace mirrors for coding agents",
    "\nhttps://docli.ru | (c) 2026 Agitek | MIT License"
);
const AFTER_HELP_UNICODE: &str = concat!(
    "docli-cli ",
    env!("CARGO_PKG_VERSION"),
    " \u{b7} https://docli.ru \u{b7} \u{a9} 2026 Agitek \u{b7} MIT License"
);
const AFTER_HELP_ASCII: &str = concat!(
    "docli-cli ",
    env!("CARGO_PKG_VERSION"),
    " | https://docli.ru | (c) 2026 Agitek | MIT License"
);
const ABOUT_UNICODE: &str =
    "docli-cli \u{2014} read-only docli workspace mirrors for coding agents";
const ABOUT_ASCII: &str = "docli-cli - read-only docli workspace mirrors for coding agents";

#[derive(Parser)]
#[command(
    name = "docli",
    display_name = "docli-cli",
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
        /// Copy the mirror contract into each agent's own skills directory: `auto`
        /// (detected here - the default), `none`, or a comma-separated list of agent keys
        #[arg(long)]
        skills: Option<String>,
        /// Install docli's hooks - a PreToolUse hook that refuses writes into the mirror and a
        /// SessionStart hook that reports its freshness: `auto` (detected here), `none` (the
        /// default), or a list of claude,codex. Never written without this flag under
        /// --no-input
        #[arg(long)]
        hooks: Option<String>,
        /// Write the docli section of AGENTS.md, and a CLAUDE.md importing it when none
        /// exists (an existing CLAUDE.md is never edited)
        #[arg(long)]
        instructions: bool,
    },
    /// Bring every mount to the server's head (one-shot; never pushes)
    Sync {
        /// Freshness gate only: exit 0 confirms the mirror is current
        #[arg(long, conflicts_with = "full")]
        check: bool,
        /// Rebuild the mirror from server state and prune stale files
        #[arg(long)]
        full: bool,
        /// Report freshness on stdout in an agent's SessionStart hook schema, and always
        /// exit 0 (`claude` or `codex`). Only with --check
        #[arg(long, requires = "check", conflicts_with = "full")]
        agent: Option<String>,
    },
    /// Server search across all mounts; pass a result to `docli read`
    Search {
        /// The query
        #[arg(value_name = "QUERY")]
        query: Vec<String>,
        /// Machine-readable output (JSON)
        #[arg(long)]
        json: bool,
    },
    /// Print a mirrored note, or a file's metadata, by server path or node id
    Read {
        /// The server path - the address search, wikilinks and the MCP tools all use
        #[arg(value_name = "PATH")]
        path: Option<String>,
        /// Address by node id instead of path
        #[arg(long, conflicts_with = "path")]
        id: Option<uuid::Uuid>,
        /// Which mount to read from - a mount name or a workspace id
        #[arg(long)]
        mount: Option<String>,
        /// Line range, 1-based and inclusive: `40-80`, `40-` for the rest, `40` for one line
        #[arg(long, value_name = "A-B")]
        lines: Option<String>,
        /// Machine-readable output (JSON) - the read_note envelope
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
    /// The PreToolUse hook's decision, in the agent's own JSON schema. Invoked by agents,
    /// never by people - see `docli init --hooks`.
    #[command(hide = true)]
    Guard {
        /// Which agent's hook schema to answer in
        #[arg(long)]
        agent: String,
        /// Where to read the hook payload: `-` for stdin
        #[arg(long, default_value = "-")]
        tool_input: String,
    },
    /// Remove docli-cli from this device
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
    // The update notice (D11), surface 1 of three. Deliberately AFTER the command: a notice
    // above the thing the reader asked for is noise, and one on stderr never interleaves into
    // another command's stdout product, so piping stays parseable.
    //
    // Three commands are exempt and each for its own reason: `guard` and a hook-mode `sync`
    // must emit nothing but their machine output; `status` renders the notice ITSELF, as a
    // field on stdout, because there the screen is the product; and `self-update` has just
    // finished answering this exact question.
    let announce_update = !matches!(
        cli.command,
        Command::Guard { .. }
            | Command::Status { .. }
            | Command::SelfUpdate
            // …and `uninstall`, which is not about noise: the notice WRITES its cache into
            // `~/.docli`, so announcing after a successful uninstall recreates the very
            // directory the command just removed — and does it even when the fetch fails,
            // because the attempt is stamped either way.
            | Command::Uninstall { .. }
    ) && !matches!(cli.command, Command::Sync { agent: Some(_), .. });
    match run(cli) {
        Ok(code) => {
            if announce_update {
                if let Some(n) = selfupdate::notice() {
                    ui::update_notice(&n);
                }
            }
            std::process::exit(code)
        }
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
            skills,
            hooks,
            instructions,
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
                skills,
                hooks,
                instructions,
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
        Command::Sync { check, full, agent } => {
            // The HOOK path resolves everything itself and always exits 0. It cannot go through
            // the branch below: `api_for` bails before `sync` is reached at all, so «not signed
            // in» would leave a SessionStart hook emitting an anyhow error to stderr and
            // NOTHING to the channel the agent actually reads (D3).
            if let Some(agent) = agent {
                let agent = hooks::HookAgent::parse(&agent)?;
                // The PROCESS cwd, deliberately — unlike `guard`, which reads `cwd` out of its
                // payload. The asymmetry is real and neither side is guessing: both agents
                // document that a hook command runs in the session's working directory, so
                // this is correct; `guard` reads the payload's copy because it is already
                // parsing that payload and the field costs nothing. What this path must NOT do
                // is read stdin for it — a person typing `docli sync --check --agent claude`
                // in a terminal would hang on a pipe that never delivers a line.
                return Ok(sync_cmd::hook_check(&cwd, agent));
            }
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
        Command::Read {
            path,
            id,
            mount,
            lines,
            json,
        } => {
            // Deliberately NOT `api_for`: `read` answers off the mirror, which is the whole
            // point of having one (latency and egress — v0.29.1 D1). A signed-out device with a
            // synced mirror still reads.
            let project = config::load_project(&cwd)?;
            read_cmd::run(
                &project,
                &read_cmd::ReadArgs {
                    path,
                    id,
                    mount,
                    lines,
                    json,
                },
            )
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
        Command::Guard { agent, tool_input } => {
            let agent = hooks::HookAgent::parse(&agent)?;
            guard::run(agent, &tool_input)
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
        // The brand is lowercase «docli», never «Docli» (user ruling 2026-09-01), and the entity
        // is written «Agitek» in user-facing copy.
        assert!(long.contains("docli-cli"), "{long}");
        assert!(!long.contains("Docli"), "{long}");
        assert!(!long.contains("OOO"), "{long}");
        assert!(long.contains("docli.ru"), "{long}");
        assert!(long.contains("(c) 2026 Agitek"), "{long}");
        assert!(long.contains("MIT"), "{long}");
        // The NAME and the VERSION share the first line — the shape the neighbouring tool uses
        // (`codex-cli 0.144.6`), which is the evidence the hyphenated spelling rests on. It comes
        // from clap's `display_name`, so a rename that only touched these constants would not
        // move it; asserting the rendered line is what makes the two agree.
        let first = long.lines().next().unwrap_or_default();
        assert_eq!(
            first,
            concat!("docli-cli ", env!("CARGO_PKG_VERSION")),
            "name and version belong on one line: {long}"
        );
        // …while `Usage:` still names the COMMAND. The product has a name and the command has a
        // spelling, and conflating them would tell people to type something that does not exist.
        let help = cmd.render_long_help().to_string();
        assert!(help.contains("Usage: docli "), "{help}");
        assert!(help.contains("docli.ru"), "{help}");
        assert!(help.contains("Agitek"), "{help}");
        // The version is in the HELP footer too, deliberately: help output is what people paste
        // into a bug report, and a paste with no version costs a round trip to establish one.
        assert!(
            help.contains(concat!("docli-cli ", env!("CARGO_PKG_VERSION"))),
            "the help footer must carry the version: {help}"
        );
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
            "read",
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
