// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli uninstall` (0.1.1) — the exit that the install script's entrance implies.
//!
//! The shape follows what a single-binary tool is expected to do (rustup's `self uninstall`,
//! deno, volta): the binary removes ITSELF and its per-user state, shows the exact list first,
//! and asks once. What it deliberately does NOT do is decide anything about the user's own
//! directories:
//!
//! * **`docli.toml` and a mirror the user PLACED are the project's, not ours.** A mirror is a
//!   rebuildable cache, but an explicit `dir` puts it inside a repository the user owns, and a
//!   tool that deletes files inside a checkout on its way out is a tool nobody installs twice.
//!   Those are PRINTED and left; `--purge` opts into removing the ones it can prove are ours.
//! * **The per-machine cache IS ours, and goes with the rest of `~/.docli`.** Since v0.29.2 a
//!   derived mount lives in our own home, not in anybody's checkout — and uninstall removes the
//!   BINARY, so no project can be reading that cache afterwards. It is announced in the list
//!   before the single confirmation, like everything else. `--purge` deliberately says nothing
//!   about it: that flag is scoped to ONE project, while the cache is shared by every project on
//!   the machine, so purging from project A must never take away what project B is using.
//! * **Agent configurations are shared files.** `.mcp.json` and friends usually carry other
//!   servers, so uninstall names them and leaves the editing to the reader.
//!
//! Credentials are the exception: they are ours, they are a live credential, and leaving them
//! behind after «uninstall» would be a leak. So the device is logged out first — the same
//! revoke-then-forget order as `docli logout`, for the same reason.
//!
//! On Windows the running executable cannot delete itself; it is renamed aside (`.old`) and
//! swept by `cleanup_stale_binary` on any later run — and if no later run ever happens, the
//! message says which file to delete by hand rather than claiming success it cannot verify.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dialoguer::Confirm;

use crate::creds::CredsStore;
use crate::{logout, ui};

/// The spelling to SHOW a human: resolved when the path exists, the raw one otherwise. Invoking
/// the binary through a relative path (`../bin/docli`) otherwise reproduces the `..` in a list of
/// things about to be deleted, which is exactly where a reader needs to be sure what is meant.
/// Display only — every removal still uses the original path.
fn shown(p: &Path) -> String {
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}

/// Where per-user state lives — the same resolution `CredsStore::open_default` uses.
pub fn home_dir() -> Option<PathBuf> {
    match std::env::var_os("DOCLI_HOME") {
        Some(d) => Some(PathBuf::from(d)),
        None => std::env::home_dir().map(|h| h.join(".docli")),
    }
}

/// The mirror directories and control plane of the project the command was run in, if any.
/// Reported always; removed only under `--purge`.
///
/// **Every path is proved to be a STRICT descendant of the project root before it is offered.**
/// `docli.toml` is committed and hand-editable, and nothing revalidates it here: a mount with
/// `dir = "."` would otherwise make `--purge` delete the whole project (including the
/// `docli.toml` this command promises to keep), and `dir = ".."` its parent. The check is on the
/// PHYSICAL path, so a symlinked mount cannot point out of the tree either.
fn project_paths(cwd: &Path) -> Result<Vec<PathBuf>> {
    let Some(root) = crate::config::find_project(cwd) else {
        return Ok(Vec::new());
    };
    // A config that will not parse is NOT «no mounts»: under `--purge` that silently left the
    // mirrors on disk while reporting a completed purge. The caller decides what to do with the
    // error — without `--purge` there is nothing to purge and it does not matter.
    let project = crate::config::load_project(&root)?;
    let root_phys = crate::config::physicalize(&root);
    let mut out = vec![root.join(".docli")];
    let mut skipped: Vec<String> = Vec::new();
    // A mount directory is only OURS if THAT DIRECTORY carries our marker: `MOUNT.docli` naming
    // this control plane as owner and this workspace. Workspace STATE is not proof — it is keyed
    // by workspace id, so re-pointing an already-synced workspace at an existing `src/` made
    // that directory look owned, and `--purge` would have deleted the project's own source.
    // `docli.toml` is committed and hand-editable, so this has to be checked against the disk.
    //
    // The project's REAL control root — since v0.29.2 that is `~/.docli`, not `<project>/.docli`.
    // This line still said the latter, so every marker was compared against a directory that no
    // longer exists, every mount came back «not this project's mirror», and `--purge` refused all
    // of them while saying so.
    let control_dir = project.control_root().dir;
    for m in &project.config.mounts {
        // A DERIVED mount is the per-machine cache in `~/.docli/mirror`, which the ordinary
        // uninstall already removes wholesale — it is not this project's to purge, and it is
        // not the reader's directory to be told about. `--purge` exists for the mount somebody
        // put inside their own project with an explicit `dir`; that is the same «a directory is
        // printed only when the USER chose it» split `docli list` learned in 0.1.14.
        if m.derived_dir {
            continue;
        }
        let abs = crate::config::mount_abs(&root, m);
        let phys = crate::config::physicalize(&abs);
        // Never the project itself or anything containing it: `dir = "."` or `".."` would make
        // `--purge` delete the checkout. That is a containment question, not an ownership one.
        if is_ancestor_or_self(&phys, &root_phys) {
            continue;
        }
        // OWNERSHIP is the marker, not location. A mount may legitimately live outside the
        // project (`/var/tmp/docli-mirror`); requiring a descendant path silently skipped it
        // while `--purge` still reported success and left the mirror on disk.
        if !crate::mountfs::verify_mount_identity(&abs, &control_dir, m.workspace) {
            if abs.exists() {
                skipped.push(shown(&abs));
            }
            continue;
        }
        out.push(abs);
    }
    out.retain(|p| p.exists());
    for s in skipped {
        // `warn`, not `detail`: `--quiet` drops narration, and «I did not delete what you asked
        // me to delete» is not narration.
        ui::warn(&format!("{s}: not this project's mirror - left untouched"));
    }
    Ok(out)
}

/// Is `a` the same path as `b`, or does it contain it?
fn is_ancestor_or_self(a: &Path, b: &Path) -> bool {
    b == a || b.starts_with(a)
}

/// Does `dir` STILL carry this project's marker for the workspace it is configured for? Asked
/// again immediately before deletion, because the answer can change after the list is built.
fn still_ours(cwd: &Path, dir: &Path) -> bool {
    let Some(root) = crate::config::find_project(cwd) else {
        return false;
    };
    let Ok(project) = crate::config::load_project(&root) else {
        return false;
    };
    project.config.mounts.iter().any(|m| {
        crate::config::physicalize(&crate::config::mount_abs(&root, m))
            == crate::config::physicalize(dir)
            && crate::mountfs::verify_mount_identity(dir, &project.control_root().dir, m.workspace)
    })
}

/// Delete the per-machine data docli itself wrote, so the directory can actually go.
///
/// `remove_dir` below is non-recursive on purpose — it FAILS if anything reappeared, which is
/// the honest outcome rather than a second race. That only works while nothing of OURS is left,
/// and since v0.29.2 moved the mirror into `~/.docli` that stopped being true: `mirror/`,
/// `state/` and `update-check.json` are written by ordinary use, so `remove_dir` always failed
/// and uninstall always refused — with a claim about a live sign-in that had not happened.
///
/// All three are disposable and ours: the contract already says the mirror can be deleted and
/// re-synced, `state/` only describes that mirror, and `update-check.json` is a cached notice.
/// The credential files are NOT here — step 1 removes those under the lock.
fn remove_our_leftovers(h: &Path) {
    for name in ["mirror", "state"] {
        let _ = std::fs::remove_dir_all(h.join(name));
    }
    let _ = std::fs::remove_file(h.join("update-check.json"));
}

/// Is everything still under `h` just a docli lock file?
///
/// The lock deliberately survives an uninstall (`revoke_all_and_clear` explains why: unlinking
/// it would let a waiter hold the now-unlinked inode while the next process creates a fresh
/// one). Since v0.29.1 it lives in `auth/`, so this accepts it at either place — and accepts
/// NOTHING else, because the whole point of the check is to notice a credential that came back.
fn only_locks_remain(h: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(h) else {
        return false;
    };
    entries.flatten().all(|e| {
        let name = e.file_name();
        if name == std::ffi::OsStr::new("creds.lock") {
            return true;
        }
        if name == std::ffi::OsStr::new("auth") {
            return std::fs::read_dir(e.path()).is_ok_and(|d| {
                d.flatten()
                    .all(|f| f.file_name() == std::ffi::OsStr::new("creds.lock"))
            });
        }
        false
    })
}

pub fn run(cwd: &Path, purge: bool, assume_yes: bool) -> Result<i32> {
    let exe = std::env::current_exe().context("locating the running binary")?;
    let home = home_dir();
    let project = match project_paths(cwd) {
        Ok(p) => p,
        // Only `--purge` depends on this list; without it the files stay anyway.
        Err(e) if purge => {
            return Err(e.context(
                "cannot tell what --purge should remove - repair docli.toml, or run without \
                 the flag",
            ))
        }
        Err(_) => Vec::new(),
    };

    ui::heading("Uninstalling docli-cli");
    ui::detail("This will be removed:");
    ui::line(&format!("  {}", ui::path(&shown(&exe))));
    if let Some(h) = &home {
        if h.exists() {
            ui::line(&format!(
                "  {}  {}",
                ui::path(&shown(h)),
                ui::dim("(credentials, mirrors and sync state)")
            ));
        }
    }
    if !project.is_empty() {
        if purge {
            ui::detail("And, because of --purge, in the current project:");
            for p in &project {
                ui::line(&format!("  {}", ui::path(&shown(p))));
            }
        } else {
            ui::detail(
                "Left in place (these are your files - remove them yourself if you want to):",
            );
            for p in &project {
                ui::line(&format!("  {}", ui::dim(&shown(p))));
            }
            ui::detail("docli.toml and your agent configurations are not touched either.");
        }
    }

    if !assume_yes {
        if !ui::interactive() {
            ui::refuse(
                "This needs an answer but the terminal is not interactive - re-run with --yes.",
            );
            return Ok(1);
        }
        if !Confirm::with_theme(&crate::wizard::prompt_theme())
            .with_prompt("Remove docli-cli from this device?")
            .default(false)
            .interact()?
        {
            ui::ok("Cancelled - nothing was removed.");
            return Ok(0);
        }
    }

    // 0. Is the store directory OURS at all? `DOCLI_HOME` is an environment variable, so it can
    // name the user's home directory — where `install_id` and `credentials.json` may exist as
    // somebody else's files. This has to be settled BEFORE anything is revoked or removed:
    // checking it later cannot prevent a deletion that already happened.
    if let Some(h) = &home {
        let phys = crate::config::physicalize(h);
        let user_home = std::env::home_dir().map(|p| crate::config::physicalize(&p));
        if phys.parent().is_none() || user_home.as_ref() == Some(&phys) {
            ui::refuse(&format!(
                "DOCLI_HOME points at {} - that is a home directory, not a docli store, and \
                 this command will not treat it as one",
                shown(h)
            ));
            ui::next("Unset DOCLI_HOME (or point it at a directory of its own) and try again");
            return Ok(1);
        }
    }

    // 1. Hand the credentials back FIRST, ATOMICALLY, and let the outcome decide the rest.
    //
    // Four review rounds went into this. Every check-then-act shape — log out, re-read, delete —
    // could still destroy a credential minted between the last check and the delete, and
    // building it out of `logout` was doubly wrong because `logout` DROPS what it could not
    // revoke (its own documented contract). So the whole thing happens inside the credential
    // store's advisory lock, the one `login` and `refresh` also take:
    // `revoke_all_and_clear` revokes each entry, removes only the ones the SERVER confirmed,
    // deletes the files only when nothing is left, and hands back whatever remains.
    //
    // The invariant this buys: **uninstall never deletes a credential that was not revoked**,
    // and it FAILS CLOSED — if the store cannot even be read, the command stops rather than
    // deleting on the strength of an answer it does not have.
    let had_store = home
        .as_ref()
        .is_some_and(|h| h.join("auth/credentials.json").exists());
    // Collected DURING the revoke pass rather than read before it: the pass runs under the
    // store's lock and is the only place that sees each entry, and a pre-read could name an
    // origin whose credential the pass then failed to remove.
    let minted_keys = std::sync::Mutex::new(Vec::<String>::new());
    let remaining = match CredsStore::open_stored().and_then(|store| {
        store.revoke_all_and_clear(&|server, creds| match &creds.refresh_token {
            Some(rt) => logout::revoke(server, rt),
            // A minted key is DELETED without a server confirmation, deliberately — the
            // «never delete a credential that was not revoked» invariant exists because a
            // device grant with no local copy left can never be revoked by anyone. A key the
            // user minted is listed under their own name in the access list, so they can
            // always retire it; we never had exclusive custody. It is NAMED below, because
            // deleting it quietly would still read as «revoked».
            None => {
                if let Ok(mut v) = minted_keys.lock() {
                    v.push(server.to_string());
                }
                true
            }
        })
    }) {
        Ok(remaining) => remaining,
        Err(e) => {
            ui::refuse(&format!(
                "cannot establish that no live credential is left ({e:#}) - stopping before \
                 anything is deleted"
            ));
            ui::next("Repair or remove ~/.docli yourself, then run uninstall again");
            return Ok(1);
        }
    };
    for origin in minted_keys.into_inner().unwrap_or_default() {
        ui::warn(&format!(
            "{origin}: the stored key was a token you minted, so it was removed from this \
             machine without being revoked - it is still live. Retire it in the access list on \
             {origin} if you no longer want it to work."
        ));
    }
    if !remaining.is_empty() {
        // The origins come from the STORE, never from the current project: with a single
        // `https://staging.example` credential and no project, pointing at docli.ru's access
        // list would send the reader somewhere else entirely.
        ui::refuse(
            "these sign-ins could NOT be revoked, so nothing was deleted - their tokens would \
             otherwise stay live with no local copy left to revoke them with:",
        );
        for origin in &remaining {
            ui::detail(origin);
        }
        ui::next(
            "Get back online and run uninstall again, or disconnect the device in the \
                  access list on that origin",
        );
        return Ok(1);
    }

    // 2. Per-user state. The FILES were removed inside the credential lock by
    // `revoke_all_and_clear` — doing it here, unlocked, let a login that was waiting on that
    // lock write a fresh credential into a file we were about to delete. What is left is the
    // directory itself, and `remove_dir` is exactly the right tool: it FAILS if anything
    // reappeared, which is the honest outcome rather than a second race.
    if let Some(h) = &home {
        if h.is_dir() {
            let label = shown(h);
            // Everything else docli itself put here, removed BEFORE the directory. `remove_dir`
            // is non-recursive by design (it fails if anything reappeared, which is the honest
            // outcome), but that only works if what remains is nothing of ours — and since the
            // mirror moved to `~/.docli` in v0.29.2 that stopped being true. The result was an
            // uninstall that could never finish on an install anyone had USED, refusing with a
            // claim about a live sign-in that had not happened.
            //
            // These are all disposable and ours: the mirror is a cache the contract already
            // says can be deleted and re-synced, `state/` describes that cache, and
            // `update-check.json` is the cached update notice. `creds.lock` deliberately stays
            // (see `revoke_all_and_clear`).
            remove_our_leftovers(h);
            // `auth/` only ever holds the credential files, which step 1 removed under the
            // lock, plus the lock itself — which STAYS, for the reason `revoke_all_and_clear`
            // gives. So this succeeds exactly when the lock is already gone, and its failure is
            // the benign case `only_locks_remain` recognises.
            let _ = std::fs::remove_dir(h.join("auth"));
            match std::fs::remove_dir(h) {
                Ok(()) => ui::ok(&format!("removed: {label}")),
                Err(_) => {
                    ui::ok(&format!("credentials removed from {label}"));
                    // Distinguish the two reasons, because they mean opposite things. Only the
                    // lock file left = nothing of yours is there, and it stays by design (see
                    // `revoke_all_and_clear`). Anything else = a file we did not put there, or
                    // one a concurrent process recreated.
                    let only_lock = only_locks_remain(h);
                    if only_lock {
                        ui::detail(&format!(
                            "{label} kept: only a lock file remains, safe to delete once no \
                             docli command is running"
                        ));
                    } else if h.join("credentials.json").exists()
                        || h.join("auth/credentials.json").exists()
                    {
                        // A CREDENTIAL is back — in practice a `docli login` that won the lock
                        // right after the credential step. Its token is live and unrevoked, so
                        // removing the binary would take away the only thing that can revoke
                        // it. This is the one case worth stopping for, and it is now decided by
                        // looking for the credential rather than by "the directory is not
                        // empty", which was true of every ordinary install.
                        ui::refuse(&format!(
                            "{label} is not empty - a sign-in most likely landed while \
                             uninstalling, and its token is live"
                        ));
                        ui::next("Run `docli logout --all`, then uninstall again");
                        return Ok(1);
                    } else {
                        // Something we did not write. Say so NEUTRALLY and carry on: the
                        // credential is gone, which is what this command is actually for, and
                        // asserting a live token we have no evidence of would be the same false
                        // alarm this branch used to raise.
                        ui::detail(&format!(
                            "{label} kept: it still holds files docli did not write - look, \
                             then remove it yourself"
                        ));
                    }
                }
            }
        }
    }

    // 3. The project's caches, only when asked — and each one RE-VERIFIED at the moment of
    // deletion. The list was built before the confirmation prompt and before the credential
    // step, so a mirror could have been renamed away and an ordinary directory left at the same
    // path in between; deleting from a stale ownership snapshot is how the wrong tree gets
    // removed. `.docli/` is ours by construction and needs no marker.
    if purge {
        let control_dir = crate::config::find_project(cwd).map(|r| r.join(".docli"));
        for p in &project {
            if !p.is_dir() {
                continue;
            }
            let label = shown(p);
            let is_control = control_dir
                .as_ref()
                .is_some_and(|c| crate::config::physicalize(c) == crate::config::physicalize(p));
            if !is_control && !still_ours(cwd, p) {
                ui::warn(&format!(
                    "{label} is no longer this project's mirror - left untouched"
                ));
                continue;
            }
            std::fs::remove_dir_all(p).with_context(|| format!("removing {label}"))?;
            ui::ok(&format!("removed: {label}"));
        }
    }

    // 3a. The hook entries, ALWAYS — not only under `--purge` (v0.28.6 Step 8a).
    //
    // This is a deliberate exception to «agent configurations are shared files», and the reason
    // is that these entries are not configuration for somebody else's server: they name THIS
    // binary, which is about to be gone. Leaving them is leaving litter that resolves to
    // nothing. The guarded command form makes a dangling entry harmless rather than broken,
    // which is what lets this be tidy-up instead of a rescue — and only OUR elements are
    // touched; the user's own hooks, and every other key in the file, are untouched.
    remove_hooks(cwd);

    // 4. The binary itself, last — everything above needs it running. A root-owned install
    // directory is the ordinary case, not an error, but it must not be reported as a completed
    // uninstall: the credentials are gone and the binary is still on PATH.
    if remove_self(&exe)? {
        ui::ok("docli-cli removed.");
        // The one case this command genuinely cannot close: a `docli login` already waiting on
        // its browser callback will write a fresh credential when the user finishes it, which
        // is AFTER every check here and after the binary is gone. Nothing detectable
        // distinguishes that from no login at all, so it is stated rather than pretended away.
        if had_store {
            ui::detail(
                "If a docli login was still waiting for its browser callback, finish or cancel \
                 it: a credential written after this point stays on disk unrevoked.",
            );
        }
        ui::detail("Thanks for using it. Come back any time: https://docli.ru");
        // A credential we could not revoke is the one thing left undone.
        return Ok(0);
    }
    ui::warn("Credentials removed, but the executable is still installed - remove it by hand.");
    Ok(1)
}

/// Take back the hook entries `docli init` wrote in the project we are standing in, reporting
/// what could not be removed rather than failing the uninstall over it.
///
/// Nothing here is fatal: an unwritable or hand-rewritten agent config is the user's file, and
/// «I could not tidy up» must not stop a credential from being revoked.
fn remove_hooks(cwd: &Path) {
    let Some(root) = crate::config::find_project(cwd) else {
        return;
    };
    for agent in crate::hooks::HookAgent::all() {
        let rel = agent.config_path();
        let abs = root.join(rel);
        let body = match std::fs::read_to_string(&abs) {
            Ok(b) => b,
            // Absent is the ordinary case: nothing of ours can be in a file that is not there.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Anything else — permissions, an ACL, a transient I/O error — means we do not KNOW
            // whether live entries are in there, and the binary they name is about to be
            // deleted. Silence would leave the user with hooks pointing at nothing and no way
            // to have learned it.
            Err(e) => {
                ui::warn(&format!(
                    "{rel} could not be read ({e}), so docli's hooks were not removed from it - \
                     if it has entries whose command contains `docli guard --agent` or \
                     `docli sync --check --agent`, delete them by hand (they name a binary that \
                     will not exist)."
                ));
                continue;
            }
        };
        let cleaned = match crate::hooks::remove(agent, &body) {
            crate::hooks::RemoveOutcome::Removed(c) => c,
            crate::hooks::RemoveOutcome::NothingOfOurs => continue,
            // «I could not look» is not «there was nothing there». The textual locator refuses
            // shapes it cannot be sure of — a duplicated key, or any backslash-escaped
            // top-level string, which a Windows path in `statusLine` is enough to produce — and
            // treating that as «nothing of ours» deleted the binary while leaving live entries
            // naming it, with nobody told.
            crate::hooks::RemoveOutcome::Refused => {
                ui::warn(&format!(
                    "docli's hooks in {rel} could not be removed safely - the file has a shape \
                     this cannot edit without guessing. Delete the two entries whose command \
                     contains `docli guard --agent` and `docli sync --check --agent` by hand \
                     (they name a binary that will not exist)."
                ));
                continue;
            }
        };
        match crate::agents::write_user_config(&abs, cleaned.as_bytes()) {
            Ok(()) => ui::ok(&format!("removed docli's hooks from {rel}")),
            // `warn`, not `detail`: «I left an entry pointing at a binary I am about to
            // delete» is not narration.
            // Names the strings that are ACTUALLY in the file. This message exists so a person
            // can finish what the CLI could not, and it pointed at an identity marker the CLI
            // stopped writing — they would have searched and found nothing.
            Err(e) => ui::warn(&format!(
                "could not remove docli's hooks from {rel} ({e:#}) - they name a binary that \
                 will not exist. Delete the two entries whose command contains \
                 `docli guard --agent` and `docli sync --check --agent`."
            )),
        }
    }
}

/// Returns whether the binary is actually gone.
#[cfg(unix)]
fn remove_self(exe: &Path) -> Result<bool> {
    match std::fs::remove_file(exe) {
        Ok(()) => Ok(true),
        // A system-wide install the user cannot write to: say the exact command instead of
        // failing with a bare permission error — and report FALSE, so the caller does not
        // announce an uninstall that did not happen.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            ui::warn(&format!(
                "no permission to remove {} - run: sudo rm {}",
                shown(exe),
                shown(exe)
            ));
            Ok(false)
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("removing {}", shown(exe)))),
    }
}

/// Returns whether the binary is actually gone (on Windows: moved aside, which is as far as a
/// running executable can go).
#[cfg(windows)]
fn remove_self(exe: &Path) -> Result<bool> {
    // A running .exe cannot delete itself; renaming it aside is allowed, and the next `docli`
    // run sweeps `.old` (`selfupdate::cleanup_stale_binary`). After an uninstall there IS no
    // next run, so the file is named for the reader rather than silently left behind.
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
    ui::warn(&format!(
        "Windows will not let a running file delete itself: remove {} by hand.",
        old.display()
    ));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_honours_the_override() {
        let _home = crate::creds::home_env_lock();
        std::env::set_var("DOCLI_HOME", "/tmp/docli-home-probe");
        assert_eq!(home_dir(), Some(PathBuf::from("/tmp/docli-home-probe")));
        std::env::remove_var("DOCLI_HOME");
    }

    #[test]
    fn project_paths_lists_only_what_exists_and_nothing_outside_the_project() {
        let _home = test_home();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("docli.toml"),
            "server = \"https://docli.ru\"\n\n[[mount]]\nworkspace = \
             \"00000000-0000-0000-0000-000000000001\"\ndir = \"cache\"\n",
        )
        .unwrap();
        // Neither path exists yet: nothing is offered for deletion.
        assert!(project_paths(root).unwrap().is_empty());

        std::fs::create_dir_all(root.join(".docli")).unwrap();
        std::fs::create_dir_all(root.join("cache")).unwrap();
        // A mount counts as ours only when THAT DIRECTORY carries our marker.
        claim(root, "cache", "00000000-0000-0000-0000-000000000001");
        let paths = project_paths(root).unwrap();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths.iter().all(|p| p.starts_with(root)), "{paths:?}");
        // docli.toml itself is the user's committed file — never on the list.
        assert!(
            !paths.iter().any(|p| p.ends_with("docli.toml")),
            "{paths:?}"
        );
    }

    #[test]
    fn a_hand_edited_mount_cannot_make_purge_delete_the_project_or_its_parent() {
        let _home = test_home();
        // `docli.toml` is committed and hand-editable, and nothing revalidates it here; a mount
        // of `.` or `..` must never reach the recursive delete.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        for dir in [".", "..", "../sibling"] {
            std::fs::write(
                root.join("docli.toml"),
                format!(
                    "server = \"https://docli.ru\"\n\n[[mount]]\nworkspace = \
                     \"00000000-0000-0000-0000-000000000001\"\ndir = \"{dir}\"\n"
                ),
            )
            .unwrap();
            std::fs::create_dir_all(tmp.path().join("sibling")).unwrap();
            let paths = project_paths(&root).unwrap();
            assert!(
                paths
                    .iter()
                    .all(|p| !p.starts_with(&root) || p.ends_with(".docli")),
                "{dir:?} must never offer the project or an ancestor: {paths:?}"
            );
        }
    }

    /// Point `DOCLI_HOME` at a temp directory for the body of one test.
    ///
    /// Without it these tests resolve the control root to the REAL `~/.docli`, write marker
    /// files naming it, and pass or fail on whatever the developer's machine happens to hold.
    /// The lock is the crate-wide one, because the variable is process-global.
    struct TestHome {
        _dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn test_home() -> TestHome {
        let guard = crate::creds::home_env_lock();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded within the guard; every reader takes the same lock.
        unsafe { std::env::set_var("DOCLI_HOME", dir.path()) };
        TestHome {
            _dir: dir,
            _guard: guard,
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("DOCLI_HOME") };
        }
    }

    /// Write the `MOUNT.docli` marker a real sync would leave in `dir`.
    ///
    /// The owner is the project's REAL control root, which since v0.29.2 is the per-machine home
    /// — not `<project>/.docli`. This helper still wrote the latter, so these fixtures were
    /// describing a layout the product had stopped using, and they went on passing because the
    /// production code was reading the same stale path. Both are fixed together; a fixture that
    /// agrees with a bug is not a test.
    fn claim(root: &Path, dir: &str, ws: &str) {
        let project = crate::config::load_project(root).unwrap();
        let control = project.control_root().dir;
        std::fs::create_dir_all(&control).unwrap();
        let owner = std::fs::canonicalize(&control)
            .unwrap()
            .display()
            .to_string();
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(
            root.join(dir).join("MOUNT.docli"),
            format!("{{\"owner\":\"{owner}\",\"workspace\":\"{ws}\"}}"),
        )
        .unwrap();
    }

    #[test]
    fn a_re_pointed_mount_does_not_make_someone_elses_directory_purgeable() {
        let _home = test_home();
        // State is keyed by WORKSPACE; the directory is what gets deleted. Syncing a workspace
        // once and then re-pointing it at an existing `src/` must not make `src/` ours.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".docli")).unwrap();
        let ws = "00000000-0000-0000-0000-000000000001";
        std::fs::write(
            root.join("docli.toml"),
            format!(
                "server = \"https://docli.ru\"\n\n[[mount]]\nworkspace = \"{ws}\"\ndir = \"src\"\n"
            ),
        )
        .unwrap();
        // The workspace HAS state (it was synced elsewhere) and `src/` exists…
        crate::state::ControlRoot::new(root)
            .save_state(ws.parse().unwrap(), &crate::state::WsState::fresh(None))
            .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        let paths = project_paths(root).unwrap();
        assert!(
            !paths.iter().any(|p| p.ends_with("src")),
            "state is not proof that THIS directory is ours: {paths:?}"
        );
        // …and once it really is claimed, it is offered.
        claim(root, "src", ws);
        let paths = project_paths(root).unwrap();
        assert!(paths.iter().any(|p| p.ends_with("src")), "{paths:?}");
    }

    #[test]
    fn a_never_synced_mount_directory_is_not_ours_to_delete() {
        // `docli.toml` is committed and hand-editable; `dir = "src"` with no sync behind it
        // names a directory docli has never written to.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("docli.toml"),
            "server = \"https://docli.ru\"\n\n[[mount]]\nworkspace = \
             \"00000000-0000-0000-0000-000000000001\"\ndir = \"src\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".docli")).unwrap();
        let paths = project_paths(root).unwrap();
        assert!(
            !paths.iter().any(|p| p.ends_with("src")),
            "a never-synced directory must not be purgeable: {paths:?}"
        );
        // The control directory is ours either way.
        assert!(paths.iter().any(|p| p.ends_with(".docli")), "{paths:?}");
    }

    #[test]
    fn outside_a_project_there_is_nothing_to_purge() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(project_paths(tmp.path()).unwrap().is_empty());
    }

    /// The v0.29.2 regression, pinned at both halves.
    ///
    /// `docli uninstall` could not finish on any install that had been USED. The mirror moved
    /// into `~/.docli`, `remove_dir` is non-recursive, and the leftover check tolerated exactly
    /// one filename — so an ordinary home always looked «not empty», and the refusal asserted
    /// that «a sign-in most likely landed while uninstalling, and its token is live», which had
    /// not happened. Reported from a real run, three releases after the move.
    /// `--purge` has nothing to say about the per-machine cache.
    ///
    /// Reported from a real run: three `~/.docli/mirror/<ws>` directories each announced as «not
    /// this project's mirror - left untouched». Two faults behind it — the identity check ran
    /// against `<project>/.docli`, a control root v0.29.2 retired, so NOTHING could match; and a
    /// derived mount is not the project's to purge in the first place, since the ordinary
    /// uninstall removes `~/.docli` wholesale. The reader was told about a directory they did
    /// not choose, about an outcome that was not true.
    #[test]
    fn purge_says_nothing_about_the_machine_cache() {
        let _home = test_home();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("docli.toml"),
            "server = \"https://docli.ru\"\n\n[[mount]]\nworkspace = \"cd2f1093-4219-4d68-8d2c-dfe7d5125b72\"\n",
        )
        .unwrap();
        // A derived mount resolves into the machine home, which is not under the project at all.
        let paths = project_paths(root).unwrap();
        assert!(
            paths.is_empty(),
            "a derived mount is the machine cache, not this project's to purge: {paths:?}"
        );
    }

    #[test]
    fn a_used_home_is_emptied_of_everything_docli_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        std::fs::create_dir_all(h.join("mirror/ws1")).unwrap();
        std::fs::create_dir_all(h.join("state")).unwrap();
        std::fs::create_dir_all(h.join("auth")).unwrap();
        std::fs::write(h.join("mirror/ws1/note.md"), "x").unwrap();
        std::fs::write(h.join("state/ws1.json"), "{}").unwrap();
        std::fs::write(h.join("update-check.json"), "{}").unwrap();
        std::fs::write(h.join("auth/creds.lock"), "").unwrap();

        remove_our_leftovers(h);

        assert!(!h.join("mirror").exists(), "the mirror is a cache and goes");
        assert!(!h.join("state").exists(), "state only describes the mirror");
        assert!(
            !h.join("update-check.json").exists(),
            "a cached notice goes"
        );
        // …and what is left is recognised as benign, so the command completes instead of
        // refusing with a claim about a credential that is not there.
        assert!(
            only_locks_remain(h),
            "the lock stays by design and must not read as «something reappeared»"
        );
    }

    /// The check must still catch the case it was written for.
    #[test]
    fn a_credential_that_came_back_is_not_mistaken_for_a_leftover_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        std::fs::create_dir_all(h.join("auth")).unwrap();
        std::fs::write(h.join("auth/creds.lock"), "").unwrap();
        assert!(only_locks_remain(h));
        // A `docli login` that won the lock right after the credential step.
        std::fs::write(h.join("auth/credentials.json"), "{}").unwrap();
        assert!(
            !only_locks_remain(h),
            "a credential under auth/ must NOT read as «only a lock remains»"
        );
    }

    #[test]
    fn an_unparseable_config_is_an_error_not_an_empty_purge_list() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("docli.toml"), "server = [not toml\n").unwrap();
        assert!(project_paths(tmp.path()).is_err());
    }
}
