// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Credentials (v0.28.0 D4): `~/.docli/credentials.json`, mode 0600 on Unix and an owner-only
//! DACL on Windows (a full-sync-authority credential on a first-class platform cannot ship with
//! an unspecified inherited-ACL default; OS keychain deferred — no native deps in the static
//! binary).
//!
//! **Refresh is rotating and single-use, and a double-spend is TERMINAL** (reuse detection
//! revokes the whole lineage): an agent-driven CLI hits the concurrent shape routinely, so
//! refresh runs SINGLE-FLIGHT — an advisory file lock around the critical section (lock →
//! re-read creds → re-check expiry → refresh only if still needed → write → unlock). Two
//! failure classes are handled distinctly: `invalid_grant` ⇒ creds are dead, say «run
//! `docli login`», never retry; `503 + Retry-After` (the SUSPENDED-grant answer — a different
//! class than reuse) ⇒ wait and retry with re-read creds, never discard.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// How early before nominal expiry a token is refreshed (skew + request time).
const REFRESH_SKEW_SECS: i64 = 60;
const SUSPEND_RETRIES: u32 = 3;
const MAX_RETRY_AFTER_SECS: u64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCreds {
    pub access_token: String,
    /// `None` for a KEY the user minted and handed us (`docli login --token`): it has no refresh
    /// lineage, and that absence is the whole of why it survives a read-only home — nothing to
    /// rotate means nothing to persist means no lock and no write.
    ///
    /// Read as `#[serde(default)]`, so a credentials file written by any earlier CLI still
    /// parses: the field was mandatory and every entry carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. `None` for a minted key: its lifetime is the server's business, and a
    /// number we invented would be read as knowledge we do not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The device grant's install key (D27d) — minted once per `~/.docli`, stable thereafter.
    pub install_id: String,
}

impl ServerCreds {
    /// Is this credential due for renewal? A key with no expiry never is — the server answers
    /// that question by accepting or refusing the request.
    pub fn needs_refresh(&self, skew: i64) -> bool {
        self.expires_at.is_some_and(|at| at <= now_unix() + skew)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredsFile {
    #[serde(default)]
    servers: BTreeMap<String, ServerCreds>,
}

pub struct CredsStore {
    dir: PathBuf,
    /// `DOCLI_TOKEN`, when the environment supplied one. `None` is the ordinary stored-file
    /// mode; `Some` means nothing under `dir` is read or written for credentials.
    env: Option<EnvToken>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serializes every test that overrides `DOCLI_HOME`, across the whole crate.
///
/// The variable is PROCESS-GLOBAL and `cargo test` runs one binary with parallel threads, so
/// tests in `status`, `sync_cmd` and `uninstall` were each setting and clearing it under one
/// another — one thread's `remove_var` landing between another's `set_var` and the read it was
/// meant to cover. That is a flake whose symptom is an unrelated assertion, which is the worst
/// kind to chase. This module owns the resolution, so it owns the lock.
#[cfg(test)]
pub(crate) fn home_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Poison-tolerant: one failing test must not cascade into «every test that touches the
    // credential home also failed», which hides the one that actually broke.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `~/.docli` (override: `DOCLI_HOME` — tests and odd setups) — the CLI's per-MACHINE home.
///
/// One definition, because more than one thing lives here now: the credentials, and since the
/// mirror moved, the cache and its state as well. Two copies of this path would be two answers
/// to «where does this machine keep its docli data».
pub fn cli_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("DOCLI_HOME") {
        Some(d) => PathBuf::from(d),
        None => std::env::home_dir()
            .context("cannot determine the home directory (set DOCLI_HOME)")?
            .join(".docli"),
    })
}

/// The directory holding this machine's credentials: `~/.docli/auth`.
///
/// Named separately from [`cli_home`] because it is the ONE path an agent sandbox has to be able
/// to write for a token refresh to work, and `docli init` writes it into Codex's
/// `writable_roots`. Keeping it a level below the home is what stops that grant from reaching
/// the mirror — `writable_roots` is recursive.
pub fn auth_dir() -> Result<PathBuf> {
    Ok(cli_home()?.join("auth"))
}

/// The environment-supplied bearer.
///
/// The problem it solves was MEASURED, not imagined (the v0.29.1 live-agent gate): a coding
/// agent's sandbox leaves `$HOME` read-only while the workspace stays writable, and the stored
/// credential cannot be refreshed there by any arrangement of ours — taking the single-flight
/// lock needs a writable home, and persisting the ROTATED refresh token needs one too, while
/// refreshing WITHOUT persisting burns the stored token and locks the user out of a credential
/// only a browser round-trip restores. So the CLI worked until the access token lapsed and then
/// stopped, intermittently and without explanation. A token handed in by the environment
/// sidesteps the whole knot: nothing is locked, nothing is rotated, nothing is written.
///
/// The convention is `gh`'s (`GH_TOKEN` / `GH_ENTERPRISE_TOKEN`), including the part that
/// matters most here: **the token is bound to ONE origin**. `docli.toml` is committed and
/// teammate-editable, and its `server` line is what decides where the CLI sends its bearer — so
/// an unbound environment token would let a repository choose where a credential travels. The
/// binding turns that into a refusal instead of an exfiltration.
pub const TOKEN_VAR: &str = "DOCLI_TOKEN";
/// The origin [`TOKEN_VAR`] belongs to. Defaults to production; a dev stack or a self-hosted
/// server names itself here.
pub const TOKEN_SERVER_VAR: &str = "DOCLI_TOKEN_SERVER";
const DEFAULT_TOKEN_SERVER: &str = "https://docli.ru";
/// Refuse rather than truncate, the `client_id`/`state` rule (v0.25.6 D8). Real tokens are
/// nowhere near this; anything longer is a paste accident or a file read into the variable.
const MAX_TOKEN_LEN: usize = 4096;

#[derive(Debug, Clone)]
pub struct EnvToken {
    token: String,
    /// Normalized (no trailing slash), so it compares byte-for-byte with a config origin.
    server: String,
}

impl EnvToken {
    pub fn server(&self) -> &str {
        &self.server
    }
}

/// Resolve [`TOKEN_VAR`], or `None` when it is unset or empty.
///
/// Empty is UNSET (gh's rule): `DOCLI_TOKEN=` in a shell profile or a CI matrix that did not
/// populate it must not shadow the stored sign-in with a credential that cannot work.
pub fn env_token() -> Result<Option<EnvToken>> {
    let Some(raw) = std::env::var_os(TOKEN_VAR) else {
        return Ok(None);
    };
    let raw = raw.to_str().with_context(|| {
        format!("{TOKEN_VAR} is not valid UTF-8 - it cannot be an access token")
    })?;
    // Trimmed, because the overwhelmingly common way this variable is filled is from a file or
    // a command substitution, and a trailing newline in an HTTP header is an error message
    // about header syntax rather than about the token.
    let token = raw.trim();
    if token.is_empty() {
        return Ok(None);
    }
    if token.len() > MAX_TOKEN_LEN {
        bail!(
            "{TOKEN_VAR} is {} bytes long - that is not a token",
            token.len()
        );
    }
    // A bearer is sent verbatim in a header. Anything outside printable ASCII cannot be, and
    // saying so here beats a transport-level complaint at the first request.
    if let Some(bad) = token.chars().find(|c| !matches!(c, '\x21'..='\x7e')) {
        bail!(
            "{TOKEN_VAR} contains a character that cannot appear in an access token ({:?}) - \
             check for stray whitespace or quotes",
            bad
        );
    }
    let server = match std::env::var_os(TOKEN_SERVER_VAR) {
        Some(s) => {
            let s = s
                .to_str()
                .with_context(|| format!("{TOKEN_SERVER_VAR} is not valid UTF-8"))?
                .trim()
                .trim_end_matches('/')
                .to_string();
            if s.is_empty() {
                bail!("{TOKEN_SERVER_VAR} is empty - name the origin {TOKEN_VAR} belongs to, or unset it");
            }
            s
        }
        None => DEFAULT_TOKEN_SERVER.to_string(),
    };
    Ok(Some(EnvToken {
        token: token.to_string(),
        server,
    }))
}

impl CredsStore {
    /// `~/.docli` (override: `DOCLI_HOME` — tests and odd setups), or [`TOKEN_VAR`] when the
    /// environment supplies a bearer.
    ///
    /// In environment mode the credentials FILE is never opened, hardened, read or written —
    /// which is the point: a read-only home stops being a problem to solve rather than a
    /// problem to survive.
    pub fn open_default() -> Result<Self> {
        Self::open_default_in(cli_home()?)
    }

    fn open_default_in(home: PathBuf) -> Result<Self> {
        match env_token()? {
            Some(env) => Ok(CredsStore {
                dir: home.join("auth"),
                env: Some(env),
            }),
            None => Self::open(home),
        }
    }

    /// The stored credential, IGNORING [`TOKEN_VAR`] — for `logout` and `uninstall`, which
    /// manage what is on this machine. An environment token is not ours to remove, and refusing
    /// to clear the file because one happens to be set would leave a device signed in with no
    /// way to sign it out.
    pub fn open_stored() -> Result<Self> {
        Self::open(cli_home()?)
    }

    /// A store that serves ONE bearer from memory and touches no file at all.
    ///
    /// It reuses the environment-token machinery because the semantics are identical: a bearer
    /// bound to one origin, never written, never refreshed. `docli login --token` uses it to
    /// VERIFY a pasted key against the server before storing it — writing an unusable
    /// credential and letting the next command discover it would put the failure a long way
    /// from its cause.
    pub fn in_memory(server: &str, token: &str) -> Self {
        CredsStore {
            // Never read: every path that touches `dir` is a write, and writes refuse while
            // `env` is set.
            dir: PathBuf::new(),
            env: Some(EnvToken {
                token: token.to_string(),
                server: server.trim_end_matches('/').to_string(),
            }),
        }
    }

    /// `home` is the CLI's per-machine directory; the credentials live in `home/auth`.
    ///
    /// The subdirectory exists so that the ONE thing a coding agent's sandbox has to be granted
    /// — a writable path, for the token refresh — can be granted without also handing over the
    /// mirror. Codex's `writable_roots` is RECURSIVE (measured 2026-09-04), so a grant of
    /// `~/.docli` would make the mirror writable to shell commands, which is precisely the gap
    /// the guard hook cannot cover ("shell writes are not covered on either agent"). With the
    /// credentials one level down, the grant is `~/.docli/auth` and the mirror stays refused.
    pub fn open(home: PathBuf) -> Result<Self> {
        let dir = home.join("auth");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let store = CredsStore { dir, env: None };
        // Re-harden an EXISTING credentials file before anything reads it (Codex rounds
        // 20–21): a file carrying a broad ACE/mode from outside our writes would otherwise
        // stay exposed for as long as the access token stays fresh. The entry must be a
        // REGULAR file (no-follow stat — a symlink would chmod its target and read a file we
        // never hardened), and instead of patching ACLs in place (unenumerable foreign ACEs
        // survive a denylist) the bytes are REWRITTEN through the born-restricted temp +
        // rename path, which replaces the security descriptor wholesale. Fail closed.
        // …UNDER the credentials lock (Codex round 22): an unlocked rewrite could resume
        // with a stale snapshot after a concurrent refresh rotated the tokens, and the next
        // refresh would then replay a consumed refresh token into terminal lineage revocation.
        // The REFUSAL is unconditional and comes first: a credentials entry that is not a
        // regular file (a symlink pointing anywhere) must never be used, and that question is a
        // no-follow stat needing no lock at all. Gating it behind the lock meant a second
        // concurrent command skipped the check entirely and read through the symlink.
        match fs::symlink_metadata(store.file_path()) {
            Ok(md) if md.file_type().is_file() => {}
            Ok(_) => bail!(
                "{} is not a regular file - refusing to use it for credentials",
                store.file_path().display()
            ),
            Err(_) => return Ok(store),
        }
        // Everything from here on is HARDENING, and hardening has to WRITE. A coding agent's
        // sandbox routinely leaves $HOME read-only while the workspace stays writable — Codex's
        // `workspace-write` does exactly that — and treating a write refusal as a failure took
        // out every verb that merely READS. Measured on the v0.29.1 live-agent gate: `search`,
        // `list` and `status` all exited «Operation not permitted» inside the sandbox while
        // `docli read`, which never opens this store, worked fine. An agent that cannot search
        // cannot establish absence, which is the one thing the contract promises.
        //
        // So a write refusal degrades to OBSERVING what we cannot fix. The property being
        // protected was never «rewrite the file» — it is «never read a credentials file carrying
        // a broad mode», and that question can be answered with a stat.
        if let Err(e) = restrict_dir(&store.dir) {
            return store.finish_unwritable(e);
        }
        let lock = match OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(store.lock_path())
        {
            Ok(l) => l,
            Err(e) if is_write_refusal(&e) => {
                return store.finish_unwritable(anyhow::Error::new(e))
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "opening {} to verify the credential file's permissions",
                    store.lock_path().display()
                )))
            }
        };
        // TRY, don't wait, for the REWRITE half only. Re-hardening is best-effort maintenance,
        // while the holder of this lock may be a concurrent refresh sleeping out a
        // `503 Retry-After` — up to six minutes. Blocking here made merely OPENING the store
        // (every `docli status`) inherit that wait.
        // CONTENTION and FAILURE are different answers. `WouldBlock` means another docli
        // command holds the lock — skip the best-effort re-hardening and carry on. Any other
        // error (a filesystem without advisory locks, an I/O failure) means we cannot tell, and
        // silently reading a possibly mode-0644 credentials file on that basis is the wrong
        // trade for a file holding a refresh token.
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(store),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "cannot lock {} to verify the credential file's permissions",
                    store.lock_path().display()
                )))
            }
        }
        let rehard = (|| {
            let existing = store.file_path();
            match fs::symlink_metadata(&existing) {
                Ok(md) if md.file_type().is_file() => {
                    let raw = fs::read(&existing)
                        .with_context(|| format!("reading {}", existing.display()))?;
                    let tmp = existing.with_extension("json.tmp");
                    let mut fh = create_restricted(&tmp)?;
                    std::io::Write::write_all(&mut fh, &raw)?;
                    drop(fh);
                    fs::rename(&tmp, &existing)?;
                    restrict_file(&existing)?;
                }
                Ok(_) => bail!(
                    "{} is not a regular file - refusing to use it for credentials",
                    existing.display()
                ),
                Err(_) => {}
            }
            Ok(())
        })();
        let _ = lock.unlock();
        rehard?;
        Ok(store)
    }

    /// The read-only-home path: we cannot re-harden, so CHECK instead of fix.
    ///
    /// Fails closed if the mode really is broad. A sandboxed agent cannot repair that, but
    /// silently reading a world-readable refresh token is the worse of the two answers — and the
    /// message names the fix, which the human running outside the sandbox can apply.
    ///
    /// A non-write error is re-raised untouched: «we could not tell» has never been a reason to
    /// carry on here.
    fn finish_unwritable(self, cause: anyhow::Error) -> Result<Self> {
        if !cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(is_write_refusal)
        {
            return Err(cause);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::symlink_metadata(self.file_path())
                .with_context(|| format!("stat {}", self.file_path().display()))?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                bail!(
                    "{} is mode {mode:o} and {} is not writable, so it cannot be re-secured - \
                     tighten it with `chmod 600`, or run this where the home directory is writable",
                    self.file_path().display(),
                    self.dir.display()
                );
            }
        }
        Ok(self)
    }

    fn file_path(&self) -> PathBuf {
        self.dir.join("credentials.json")
    }

    /// The environment token, when there is one — whatever origin it is bound to. For callers
    /// that must SAY where the sign-in came from (`status`) or refuse an operation that would
    /// be shadowed by it (`login`).
    pub fn env_source(&self) -> Option<&EnvToken> {
        self.env.as_ref()
    }

    /// The environment token IF it is the credential for `server`.
    ///
    /// A mismatch is an ERROR, deliberately, and not a quiet fall-through to the stored file.
    /// Falling through would mean the same variable shadows the store on one origin and does
    /// nothing on another — the intermittent-and-unexplained class this CLI keeps paying for.
    /// It also silently drops the binding that makes the variable safe to set at all.
    fn env_for(&self, server: &str) -> Result<Option<&str>> {
        let Some(env) = &self.env else {
            return Ok(None);
        };
        if server.trim_end_matches('/') == env.server {
            return Ok(Some(&env.token));
        }
        bail!(
            "{TOKEN_VAR} is set for {}, but this command is talking to {server} - unset \
             {TOKEN_VAR}, or set {TOKEN_SERVER_VAR} to the origin the token belongs to",
            env.server
        )
    }

    /// Is there a credential for `server` at all — from the environment or from the file?
    ///
    /// The «not signed in» gates ask this, never `get()`: in environment mode the file holds
    /// nothing, and reading that as signed-out would send an agent to `docli login`, which is
    /// the one thing it cannot do.
    pub fn signed_in(&self, server: &str) -> Result<bool> {
        if self.env_for(server)?.is_some() {
            return Ok(true);
        }
        Ok(self.get(server)?.is_some())
    }

    /// Can this sign-in LAPSE — i.e. does anything ever need to refresh it?
    ///
    /// Only a stored OAuth grant can. A minted key has no expiry and `DOCLI_TOKEN` is handed in
    /// per process, so for either of those there is nothing to renew and nothing a writable
    /// credentials directory would buy. `docli init` asks it before offering to relax Codex's
    /// sandbox: asking someone to give up a restriction for no gain is worse than not asking.
    pub fn can_lapse(&self, server: &str) -> Result<bool> {
        if self.env_source().is_some() {
            return Ok(false);
        }
        Ok(self.get(server)?.is_some_and(|c| c.refresh_token.is_some()))
    }

    /// Refuse a write while an environment token is in force.
    ///
    /// Not merely tidy: a `docli login` here would open a browser, mint a device grant, write
    /// it — and then be shadowed by the variable on the very next command, so the user would
    /// have granted authority they cannot see in use. gh refuses the same way and for the same
    /// reason. (`logout`/`uninstall` reach the file through [`Self::open_stored`], which has no
    /// environment token to be in force.)
    fn refuse_if_env(&self) -> Result<()> {
        if let Some(env) = &self.env {
            bail!(
                "{TOKEN_VAR} is set (for {}), so this machine's stored sign-in is not the one \
                 in use - unset {TOKEN_VAR} first to store credentials here",
                env.server
            );
        }
        Ok(())
    }

    /// Take the credentials lock for a path that is going to WRITE.
    ///
    /// The one thing this adds over opening the file directly is a sentence. A read-only home
    /// cannot take the lock, so none of the four writers can run — and every one of them was
    /// surfacing that as a bare «Operation not permitted (os error 1)», which names neither the
    /// cause nor a remedy.
    ///
    /// The v0.29.1 live-agent gate measured why that matters: inside an agent sandbox the CLI
    /// works while the access token is still valid and starts failing the moment it expires.
    /// Intermittent and unexplained is worse than plainly broken, because the agent cannot tell
    /// it from a transient fault and will retry forever.
    ///
    /// **It already fails SAFE and must keep doing so.** The lock is taken before any network
    /// call, so a refusal here consumes no rotating refresh token and leaves the stored
    /// credential usable. Deliberately NOT «refresh anyway and skip the write»: the server
    /// rotates the refresh token (`RefreshOutcome::Rotated`), so a refresh we cannot persist
    /// burns the stored one and locks the user out of a credential only a browser round-trip
    /// restores — which is exactly what an agent cannot do.
    fn lock_for_write(&self) -> Result<std::fs::File> {
        match OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())
        {
            Ok(l) => Ok(l),
            Err(e) if is_write_refusal(&e) => bail!(
                "{} is not writable, so the sign-in cannot be updated here - run the command \
                 where the home directory is writable (outside an agent sandbox). Nothing was \
                 lost: the stored sign-in is untouched.",
                self.dir.display()
            ),
            Err(e) => Err(anyhow::Error::new(e).context("opening the credentials lock")),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("creds.lock")
    }

    fn read(&self) -> Result<CredsFile> {
        // The file is NOT in use in environment mode, so it must not be visible through the
        // store either. Two things depend on this: `status` would otherwise report a stored
        // grant's expiry beside a sign-in that is not it, and `in_memory` (whose `dir` is
        // deliberately empty) would resolve `credentials.json` against the working directory.
        if self.env.is_some() {
            return Ok(CredsFile::default());
        }
        let p = self.file_path();
        if !p.exists() {
            return Ok(CredsFile::default());
        }
        let raw = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))
    }

    fn write(&self, f: &CredsFile) -> Result<()> {
        let p = self.file_path();
        let tmp = p.with_extension("json.tmp");
        // The temp file must be born restricted — `fs::write` + chmod-after leaves a
        // umask-default window with the refresh token world-readable (D4 says 0600,
        // unconditionally).
        let mut fh = create_restricted(&tmp)?;
        std::io::Write::write_all(&mut fh, &serde_json::to_vec_pretty(f)?)?;
        drop(fh);
        fs::rename(&tmp, &p)?;
        restrict_file(&p)?;
        Ok(())
    }

    pub fn get(&self, server: &str) -> Result<Option<ServerCreds>> {
        Ok(self.read()?.servers.get(server).cloned())
    }

    /// Every read-modify-write on the creds file runs under the same advisory lock the
    /// refresh path takes (Codex round 18): two concurrent logins for different servers each
    /// read the old map, and the later write would silently drop the other's entry — and a
    /// login racing a refresh would clobber the freshly rotated tokens.
    fn mutate(&self, op: &dyn Fn(&mut CredsFile)) -> Result<()> {
        self.refuse_if_env()?;
        let lock = self.lock_for_write()?;
        lock.lock().context("waiting for the credentials lock")?;
        let result = (|| {
            let mut f = self.read()?;
            op(&mut f);
            self.write(&f)
        })();
        let _ = lock.unlock();
        result
    }

    pub fn put(&self, server: &str, creds: ServerCreds) -> Result<()> {
        self.mutate(&|f| {
            f.servers.insert(server.to_string(), creds.clone());
        })
    }

    /// Every server this machine holds a credential for — `docli logout --all` and
    /// `docli uninstall` both need the list, and neither may guess it from a project file (a
    /// dev stack's origin lives only here).
    pub fn servers(&self) -> Result<Vec<String>> {
        Ok(self.read()?.servers.keys().cloned().collect())
    }

    pub fn remove(&self, server: &str) -> Result<()> {
        self.mutate(&|f| {
            f.servers.remove(server);
        })
    }

    /// Remove the entry for `server` ONLY while it still holds `refresh_token` — the compare
    /// happens under the same lock as the write. `docli logout` reads a credential, revokes it
    /// over the network, and then deletes; a `docli login` finishing inside that window would
    /// otherwise have its fresh lineage deleted locally while staying live on the server, with
    /// no local copy left to revoke it with. Returns false when the entry had moved on.
    ///
    /// A minted KEY has no refresh token, so identity is the ACCESS token for it — the same
    /// question («is this still the credential I just read?»), asked of the only field it has.
    pub fn remove_if_current(&self, server: &str, token: &str) -> Result<bool> {
        let removed = std::cell::Cell::new(false);
        self.mutate(&|f| {
            let current_matches = f
                .servers
                .get(server)
                .is_some_and(|c| c.refresh_token.as_deref().unwrap_or(&c.access_token) == token);
            if current_matches {
                f.servers.remove(server);
                removed.set(true);
            }
        })?;
        Ok(removed.get())
    }

    /// For call sites ALREADY holding the credentials lock (`refresh_locked`): re-acquiring
    /// through `mutate` would deadlock against our own file description.
    fn put_unlocked(&self, server: &str, creds: ServerCreds) -> Result<()> {
        let mut f = self.read()?;
        f.servers.insert(server.to_string(), creds);
        self.write(&f)
    }

    fn remove_unlocked(&self, server: &str) -> Result<()> {
        let mut f = self.read()?;
        f.servers.remove(server);
        self.write(&f)
    }

    /// The install_id for a server — reused if any (revoking + re-logging-in keeps ONE device
    /// row rather than minting grants toward the cap), minted otherwise.
    pub fn install_id(&self, server: &str) -> Result<String> {
        self.refuse_if_env()?;
        if let Some(c) = self.get(server)? {
            return Ok(c.install_id);
        }
        // Persist the minted id OUTSIDE the creds entry too, so a logout+login keeps it.
        let p = self.dir.join("install_id");
        if let Ok(existing) = fs::read_to_string(&p) {
            let t = existing.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
        // Mint under the shared lock (Codex round 18): two concurrent first logins must
        // agree on ONE install id, or the loser registers a phantom device row.
        let lock = self.lock_for_write()?;
        lock.lock().context("waiting for the credentials lock")?;
        let result = (|| {
            if let Ok(existing) = fs::read_to_string(&p) {
                let t = existing.trim().to_string();
                if !t.is_empty() {
                    return Ok(t);
                }
            }
            let id = uuid::Uuid::new_v4().to_string();
            fs::write(&p, &id)?;
            restrict_file(&p)?;
            Ok(id)
        })();
        let _ = lock.unlock();
        result
    }

    /// Revoke every stored credential and clear the store, ATOMICALLY with respect to any other
    /// docli process — the whole operation runs under the same advisory lock `login` and
    /// `refresh` take, so a credential cannot appear halfway through.
    ///
    /// This exists because `docli uninstall` cannot be built out of `logout`: three review
    /// rounds went into check-then-act variants, and every one of them could still delete a
    /// credential minted a microsecond after the last check. The rule it enforces is **never
    /// delete a credential that was not revoked** — an entry whose revocation the server did
    /// not confirm STAYS in the file, and its origin is returned so the caller can stop and say
    /// so. (`logout` deliberately does the opposite: it drops what it could not revoke, because
    /// leaving a live token on the disk of a machine you are signing out of is worse. Only
    /// `uninstall`, which also deletes the binary, needs this stricter shape.)
    ///
    /// `revoke` reports whether the SERVER confirmed. Returns the origins still present.
    pub fn revoke_all_and_clear(
        &self,
        revoke: &dyn Fn(&str, &ServerCreds) -> bool,
    ) -> Result<Vec<String>> {
        self.refuse_if_env()?;
        let lock = self.lock_for_write()?;
        lock.lock().context("waiting for the credentials lock")?;
        let result = (|| {
            let mut f = self.read()?;
            let entries: Vec<(String, ServerCreds)> = f
                .servers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (server, creds) in entries {
                if revoke(&server, &creds) {
                    f.servers.remove(&server);
                }
            }
            let remaining: Vec<String> = f.servers.keys().cloned().collect();
            if remaining.is_empty() {
                // Removed under the SAME lock: a login waiting on it cannot slip a credential
                // into a file we are about to delete.
                //
                // The LOCK FILE deliberately STAYS. Unlinking it would leave a waiter holding
                // the now-unlinked inode while the next process creates a fresh one — two
                // processes each believing they hold the credential lock, which is worse than
                // an empty file left behind. The caller reports the leftover instead.
                let _ = fs::remove_file(self.file_path());
                let _ = fs::remove_file(self.dir.join("install_id"));
            } else {
                self.write(&f)?;
            }
            Ok(remaining)
        })();
        let _ = lock.unlock();
        result
    }

    /// The stored access token AS IT IS — no refresh, no network, no lock. For readers that
    /// must be bounded in time (`docli status`): refreshing can meet a `503 Retry-After` and
    /// sleep for minutes, which is the wrong trade for a screen that mostly reports local state.
    pub fn stored_token(&self, server: &str) -> Result<Option<String>> {
        if let Some(t) = self.env_for(server)? {
            return Ok(Some(t.to_string()));
        }
        Ok(self.get(server)?.map(|c| c.access_token))
    }

    /// A valid bearer for `server`, refreshing single-flight when needed.
    /// `refresh` performs the network exchange: `(refresh_token) → (access, refresh, expires_in)`.
    pub fn bearer(
        &self,
        server: &str,
        refresh: &dyn Fn(&str) -> Result<RefreshOutcome>,
    ) -> Result<String> {
        // An environment token is used AS IT IS: there is no refresh token beside it, no
        // expiry we can read, and nothing of ours to write. Whether it still works is the
        // server's answer, and it gives it on the next request.
        if let Some(t) = self.env_for(server)? {
            return Ok(t.to_string());
        }
        let Some(c) = self.get(server)? else {
            bail!("not signed in to {server} - run `docli login`");
        };
        if !c.needs_refresh(REFRESH_SKEW_SECS) {
            return Ok(c.access_token);
        }
        self.refresh_single_flight(server, refresh, None)
    }

    /// A refresh forced by the CALLER's evidence (the 401 path) — still single-flight.
    /// `rejected` is the access token the server just refused: the under-lock freshness check
    /// must not hand it straight back (locally unexpired ≠ server-accepted — a revocation, a
    /// server-side rotation, clock skew), so under the lock the stored token short-circuits
    /// ONLY when it DIFFERS from the rejected one (some other process already rotated);
    /// otherwise the rotation runs regardless of local expiry (Codex round 2).
    pub fn refresh_single_flight(
        &self,
        server: &str,
        refresh: &dyn Fn(&str) -> Result<RefreshOutcome>,
        rejected: Option<&str>,
    ) -> Result<String> {
        // There is nothing to rotate. The only caller that reaches here in environment mode is
        // the 401 retry, so this IS the report of a refused token — and it must name the
        // credential that was refused, because the fix is somewhere the CLI cannot see.
        if self.env_for(server)?.is_some() {
            bail!(
                "{server} refused the token in {TOKEN_VAR} - it may have expired, been revoked, \
                 or be missing the `sync` scope. Mint a new one on {server} and set \
                 {TOKEN_VAR} again."
            );
        }
        // The advisory lock: the loser of a concurrent refresh would trip reuse detection and
        // revoke the LINEAGE — both processes lose the credential, recovery is a browser
        // round-trip an agent cannot perform.
        let lock = self.lock_for_write()?;
        lock.lock().context("waiting for the credentials lock")?;
        let result = self.refresh_locked(server, refresh, rejected);
        let _ = lock.unlock();
        result
    }

    fn refresh_locked(
        &self,
        server: &str,
        refresh: &dyn Fn(&str) -> Result<RefreshOutcome>,
        rejected: Option<&str>,
    ) -> Result<String> {
        let mut attempt = 0;
        loop {
            // RE-READ under the lock: the winner of a concurrent race already rotated the
            // tokens, and the re-check makes this invocation a no-op reader of its work. With
            // `rejected` set, "still fresh" is NOT enough — the stored token short-circuits
            // only when it is a DIFFERENT one than the server refused.
            let Some(c) = self.get(server)? else {
                bail!("not signed in to {server} - run `docli login`");
            };
            let usable =
                !c.needs_refresh(REFRESH_SKEW_SECS) && rejected != Some(c.access_token.as_str());
            if usable {
                return Ok(c.access_token);
            }
            // A minted key cannot be renewed by us, and the only caller that reaches here with
            // one is the 401 retry — so this IS the report that the server refused it. Saying
            // «run docli login» would be wrong twice: it is not what created this credential,
            // and it would replace a key the user is still managing on the server.
            //
            // The entry is KEPT, unlike the `invalid_grant` arm below. That arm acts on the
            // token endpoint saying the lineage is dead; this one has only a single 401 from a
            // sync request, which a momentary server-side fault also produces. Nothing was
            // consumed by the failure — no rotation happened — so keeping the key costs
            // nothing, while deleting it would force a re-paste on exactly the population that
            // cannot do one on demand: a sandbox, a CI job, an unattended container.
            let Some(refresh_token) = c.refresh_token.clone() else {
                bail!(
                    "{server} refused the access token stored for this device - it may have \
                     expired, been revoked, or be missing the `sync` scope. Mint a new key on \
                     {server} and run `docli login --token`, or `docli login` for a browser \
                     sign-in."
                );
            };
            match refresh(&refresh_token)? {
                RefreshOutcome::Rotated {
                    access_token,
                    refresh_token,
                    expires_in,
                } => {
                    let fresh = ServerCreds {
                        access_token: access_token.clone(),
                        refresh_token: Some(refresh_token),
                        expires_at: Some(now_unix() + expires_in),
                        install_id: c.install_id,
                    };
                    self.put_unlocked(server, fresh)?;
                    return Ok(access_token);
                }
                RefreshOutcome::InvalidGrant => {
                    // Terminal: never retry (a dead lineage stays dead). The creds entry goes,
                    // so the next command says «run docli login» immediately.
                    self.remove_unlocked(server)?;
                    bail!("your sign-in to {server} is no longer valid - run `docli login`");
                }
                RefreshOutcome::Suspended { retry_after_secs } => {
                    attempt += 1;
                    if attempt > SUSPEND_RETRIES {
                        bail!(
                            "{server} says the connection is suspended - try again later \
                             (your credentials were kept)"
                        );
                    }
                    let wait = retry_after_secs.min(MAX_RETRY_AFTER_SECS);
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                    // Loop re-reads creds — never discard on this class.
                }
            }
        }
    }
}

pub enum RefreshOutcome {
    Rotated {
        access_token: String,
        refresh_token: String,
        expires_in: i64,
    },
    /// `400 invalid_grant` — creds are dead.
    InvalidGrant,
    /// `503 + Retry-After` — entitlement off / persona archived; wait, never discard.
    Suspended { retry_after_secs: u64 },
}

/// Create (or truncate) a file that is RESTRICTED from its first byte.
#[cfg(unix)]
fn create_restricted(p: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // create_new (Codex round 21): a PRE-EXISTING temp (planted 0666, or a symlink) would
    // keep its own mode/target — `mode(0o600)` applies only to files this open creates. The
    // HANDLE is returned and the caller writes through it (round 22): re-opening by path can
    // recreate the file with default permissions if a concurrent open removed it first.
    let _ = fs::remove_file(p);
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(p)
        .with_context(|| format!("creating {}", p.display()))
}

#[cfg(windows)]
fn create_restricted(p: &Path) -> Result<fs::File> {
    // No mode-at-open on Windows; create_new then DACL-restrict before any secret bytes land
    // (a pre-existing file would keep its own DACL — Codex round 21). Handle returned, same
    // rule as unix (round 22).
    let _ = fs::remove_file(p);
    let f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(p)
        .with_context(|| format!("creating {}", p.display()))?;
    restrict_file(p)?;
    Ok(f)
}

#[cfg(unix)]
fn restrict_file(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Did the filesystem refuse us WRITE access, as opposed to failing for some other reason?
///
/// The distinction is the whole of the read-only-home arm: «not allowed to write here» is an
/// ordinary, expected condition inside an agent sandbox and must not take out the read paths,
/// while every other error still means we could not tell and must stop.
fn is_write_refusal(e: &std::io::Error) -> bool {
    // EPERM and EACCES both arrive as `PermissionDenied`. EROFS does not: older Rust maps it to
    // `Uncategorized`, so it is matched by its raw code — 30 on both macOS and Linux.
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(30)
}

/// Owner-only DACL via `icacls` with the OWNER_RIGHTS SID (`*S-1-3-4`) — locale-independent
/// (no `%USERNAME%` parsing), the same effect as gh's config-file fallback hardened: strip
/// inheritance, grant only the object owner.
#[cfg(windows)]
fn restrict_windows(p: &Path) -> Result<()> {
    // `/inheritance:r` strips INHERITED ACEs only — a pre-existing file/dir can carry
    // EXPLICIT ACEs for other principals that survive it (Codex round 19), so the broad
    // well-known SIDs are removed explicitly: Everyone (*S-1-1-0), Users (*S-1-5-32-545),
    // Authenticated Users (*S-1-5-11), Guests (*S-1-5-32-546). `/remove` of an absent SID is
    // a no-op, so the single invocation is idempotent.
    let status = std::process::Command::new("icacls")
        .arg(p)
        .args([
            "/inheritance:r",
            "/remove",
            "*S-1-1-0",
            "/remove",
            "*S-1-5-32-545",
            "/remove",
            "*S-1-5-11",
            "/remove",
            "*S-1-5-32-546",
            "/grant:r",
            "*S-1-3-4:F",
        ])
        .status()
        .context("running icacls to restrict the credentials file")?;
    if !status.success() {
        bail!(
            "could not set an owner-only DACL on {} (icacls exit {status}) - refusing to store \
             credentials world-readable",
            p.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_file(p: &Path) -> Result<()> {
    restrict_windows(p)
}

#[cfg(windows)]
fn restrict_dir(p: &Path) -> Result<()> {
    restrict_windows(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store() -> (tempfile::TempDir, CredsStore) {
        let tmp = tempfile::tempdir().unwrap();
        let s = CredsStore::open(tmp.path().join("home")).unwrap();
        (tmp, s)
    }

    /// The v0.29.1 live-agent gate's finding, pinned at the DECISION rather than by faking a
    /// sandbox: inside Codex's `workspace-write` the home is refused at the syscall level, which
    /// mode bits cannot reproduce (chmod succeeds on a directory you own, and an existing file
    /// stays writable in a mode-0500 parent). So the tests drive `finish_unwritable` with the
    /// error the kernel actually returns.
    ///
    /// What it protects: `search`, `list` and `status` all exited «Operation not permitted»
    /// inside the sandbox, while `docli read` — which never opens this store — worked. An agent
    /// that cannot search cannot establish absence, which is the one thing the contract promises.
    #[cfg(unix)]
    #[test]
    fn a_write_refused_home_still_opens_when_the_credentials_are_already_tight() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, s) = store();
        seed(&s, "https://docli.ru", i64::MAX / 2);
        fs::set_permissions(s.file_path(), fs::Permissions::from_mode(0o600)).unwrap();

        let refusal =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let reopened = s
            .finish_unwritable(refusal)
            .expect("a home we cannot write must still open for reading");
        assert!(reopened.get("https://docli.ru").unwrap().is_some());
    }

    /// …and the property the rewrite was protecting survives: a BROAD mode we cannot fix is
    /// refused rather than read. Checking replaces fixing; it does not replace caring.
    #[cfg(unix)]
    #[test]
    fn a_write_refused_home_refuses_a_world_readable_credentials_file() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, s) = store();
        seed(&s, "https://docli.ru", i64::MAX / 2);
        fs::set_permissions(s.file_path(), fs::Permissions::from_mode(0o644)).unwrap();

        let refusal =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let msg = match s.finish_unwritable(refusal) {
            Ok(_) => panic!("a broad mode must still be refused"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains("644"), "{msg}");
        assert!(
            msg.contains("chmod 600"),
            "the refusal must name the fix: {msg}"
        );
    }

    /// A non-write error is re-raised untouched — «we could not tell» was never a reason to
    /// carry on, and folding every failure into the tolerant arm is how a security check quietly
    /// stops being one.
    #[test]
    fn a_non_write_error_is_not_treated_as_a_read_only_home() {
        let (_tmp, s) = store();
        let other = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::InvalidData));
        assert!(s.finish_unwritable(other).is_err());
    }

    /// The second half of the v0.29.1 gate's finding. The first run fixed OPENING the store on a
    /// read-only home; this is what remained — every path that WRITES still refused with a bare
    /// errno, so inside a sandbox the CLI worked until the access token expired and then failed
    /// with a message naming neither cause nor remedy.
    #[cfg(unix)]
    #[test]
    fn a_write_refusal_on_the_lock_explains_itself_and_keeps_the_sign_in() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let s = CredsStore::open(home.clone()).unwrap();
        seed(&s, "https://docli.ru", i64::MAX / 2);
        // Make the lock itself unopenable for writing — the one part of this a mode bit CAN do,
        // since it is the FILE's own permission rather than the directory's.
        fs::set_permissions(s.lock_path(), fs::Permissions::from_mode(0o400)).unwrap();

        let msg = match s.lock_for_write() {
            Ok(_) => panic!("a write-refused lock must not be handed out"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains("not writable"), "{msg}");
        assert!(
            msg.contains("agent sandbox"),
            "the refusal must name the cause: {msg}"
        );
        assert!(
            !msg.contains("os error"),
            "a bare errno is what this replaces: {msg}"
        );
        // Fail-safe: the stored credential is untouched and still readable.
        assert!(s.get("https://docli.ru").unwrap().is_some());

        fs::set_permissions(s.lock_path(), fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn write_refusals_are_told_apart_from_other_io_errors() {
        use std::io::{Error, ErrorKind};
        assert!(is_write_refusal(&Error::from(ErrorKind::PermissionDenied)));
        // EROFS, which older Rust leaves `Uncategorized`.
        assert!(is_write_refusal(&Error::from_raw_os_error(30)));
        assert!(!is_write_refusal(&Error::from(ErrorKind::NotFound)));
        assert!(!is_write_refusal(&Error::from(ErrorKind::InvalidData)));
    }

    /// Set `DOCLI_TOKEN` (+ optionally `DOCLI_TOKEN_SERVER`) for the body of one test, under the
    /// same process-global lock `DOCLI_HOME` uses — these are the same class of variable and a
    /// test that clears one between another's write-and-read is the flake nobody can chase.
    fn with_env_token<T>(token: &str, server: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = home_env_lock();
        // SAFETY: single-threaded within the guard; every reader of these variables in this
        // crate takes the same lock.
        unsafe {
            std::env::set_var(TOKEN_VAR, token);
            match server {
                Some(s) => std::env::set_var(TOKEN_SERVER_VAR, s),
                None => std::env::remove_var(TOKEN_SERVER_VAR),
            }
        }
        let out = body();
        unsafe {
            std::env::remove_var(TOKEN_VAR);
            std::env::remove_var(TOKEN_SERVER_VAR);
        }
        out
    }

    /// gh's rule, and the one that keeps a half-populated CI matrix from breaking every
    /// developer machine that inherits its profile: `DOCLI_TOKEN=` is UNSET, not «a credential
    /// that cannot work».
    #[test]
    fn an_empty_token_variable_is_not_a_sign_in() {
        with_env_token("", None, || assert!(env_token().unwrap().is_none()));
        with_env_token("   \n", None, || assert!(env_token().unwrap().is_none()));
    }

    /// The overwhelmingly common way this variable gets filled is `$(cat token)` or a `.env`
    /// line, and both bring a newline. Trimming it here means the failure is never a complaint
    /// about HTTP header syntax.
    #[test]
    fn a_token_is_trimmed() {
        with_env_token("  abc123 \n", None, || {
            let t = env_token().unwrap().unwrap();
            assert_eq!(t.token, "abc123");
            assert_eq!(t.server, "https://docli.ru");
        });
    }

    #[test]
    fn a_token_that_cannot_be_a_bearer_is_refused_by_name() {
        with_env_token("abc\u{7}def", None, || {
            let e = format!("{:#}", env_token().unwrap_err());
            assert!(e.contains(TOKEN_VAR), "{e}");
        });
        with_env_token(&"x".repeat(MAX_TOKEN_LEN + 1), None, || {
            assert!(env_token().is_err());
        });
    }

    #[test]
    fn the_token_server_variable_names_the_origin_and_is_normalized() {
        with_env_token("abc", Some("http://docli.localhost/"), || {
            assert_eq!(
                env_token().unwrap().unwrap().server,
                "http://docli.localhost"
            );
        });
        with_env_token("abc", Some("  "), || assert!(env_token().is_err()));
    }

    /// The binding is the whole reason this variable is safe to set. `docli.toml` is committed
    /// and teammate-editable, and its `server` line decides where the bearer is sent — so a
    /// repository naming another origin must get a REFUSAL, never a silent fall-through to the
    /// stored file (which would also make the shadowing intermittent).
    #[test]
    fn the_environment_token_is_bound_to_one_origin() {
        let tmp = tempfile::tempdir().unwrap();
        with_env_token("t0k", None, || {
            let s = CredsStore::open_default_in(tmp.path().join("home")).unwrap();
            assert_eq!(
                s.bearer("https://docli.ru", &|_| unreachable!()).unwrap(),
                "t0k"
            );
            assert!(s.signed_in("https://docli.ru").unwrap());

            let e = format!(
                "{:#}",
                s.bearer("https://evil.example", &|_| unreachable!())
                    .unwrap_err()
            );
            assert!(e.contains("https://evil.example"), "{e}");
            assert!(e.contains(TOKEN_SERVER_VAR), "{e}");
            assert!(s.signed_in("https://evil.example").is_err());
        });
    }

    /// The point of the whole feature: a read-only home is not a problem to survive, it is a
    /// problem that does not arise. Nothing under `dir` may be created, read or written.
    #[test]
    fn the_environment_token_never_touches_the_credentials_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("never-created");
        with_env_token("t0k", None, || {
            let s = CredsStore::open_default_in(home.clone()).unwrap();
            assert_eq!(
                s.bearer("https://docli.ru", &|_| unreachable!()).unwrap(),
                "t0k"
            );
            assert_eq!(
                s.stored_token("https://docli.ru").unwrap().as_deref(),
                Some("t0k")
            );
        });
        assert!(
            !home.exists(),
            "environment mode created {}",
            home.display()
        );
    }

    /// A 401 in environment mode is the SERVER refusing the token, and there is nothing to
    /// rotate. It must say which credential was refused, because the fix is somewhere the CLI
    /// cannot see — and it must not call the refresh function at all.
    #[test]
    fn a_refused_environment_token_is_reported_not_refreshed() {
        let tmp = tempfile::tempdir().unwrap();
        with_env_token("t0k", None, || {
            let s = CredsStore::open_default_in(tmp.path().join("home")).unwrap();
            let e = format!(
                "{:#}",
                s.refresh_single_flight("https://docli.ru", &|_| unreachable!(), Some("t0k"))
                    .unwrap_err()
            );
            assert!(e.contains(TOKEN_VAR), "{e}");
        });
    }

    /// A `docli login` that completes and is then shadowed on every later command is worse than
    /// no login: authority granted, invisibly unused. Refuse at the store, so no caller can
    /// route around it.
    #[test]
    fn storing_a_credential_is_refused_while_the_environment_signs_us_in() {
        let tmp = tempfile::tempdir().unwrap();
        with_env_token("t0k", None, || {
            let s = CredsStore::open_default_in(tmp.path().join("home")).unwrap();
            for e in [
                s.put(
                    "https://docli.ru",
                    ServerCreds {
                        access_token: "a".into(),
                        refresh_token: Some("r".into()),
                        expires_at: Some(0),
                        install_id: "i".into(),
                    },
                )
                .unwrap_err(),
                s.install_id("https://docli.ru").unwrap_err(),
            ] {
                let e = format!("{e:#}");
                assert!(e.contains(TOKEN_VAR), "{e}");
            }
        });
    }

    /// The two questions `docli init` asks after a `--token` sign-in, pinned together because
    /// the right answers are opposite and both matter.
    ///
    /// `signed_in` must be TRUE, or the wizard's step 2 would offer a browser round to someone
    /// who just handed us a working credential — on a machine that may well have no browser,
    /// which is why they used a key. And `can_lapse` must be FALSE, or the same run would go on
    /// to ask them to relax Codex's sandbox for a refresh that will never happen.
    #[test]
    fn a_key_signin_needs_neither_a_browser_round_nor_a_writable_sandbox() {
        let (_t, s) = store();
        seed_key(&s, "https://docli.ru");
        assert!(
            s.signed_in("https://docli.ru").unwrap(),
            "a stored key IS a sign-in - init must not offer to log in again"
        );
        assert!(
            !s.can_lapse("https://docli.ru").unwrap(),
            "a key never refreshes - init must not offer to widen the sandbox for it"
        );
    }

    /// …and the OAuth grant is the one case where the sandbox offer has something to fix.
    #[test]
    fn an_oauth_grant_is_the_only_sign_in_that_can_lapse() {
        let (_t, s) = store();
        seed(&s, "https://docli.ru", i64::MAX / 2);
        assert!(s.can_lapse("https://docli.ru").unwrap());
        // Nothing stored at all is not «can lapse» either — there is no sign-in to renew.
        assert!(!s.can_lapse("https://other.example").unwrap());
    }

    fn seed_key(s: &CredsStore, server: &str) {
        s.put(
            server,
            ServerCreds {
                access_token: "minted".into(),
                refresh_token: None,
                expires_at: None,
                install_id: "i1".into(),
            },
        )
        .unwrap();
    }

    /// The property that makes a minted key work where a device grant cannot: it is never DUE,
    /// so no command ever takes the credentials lock to renew it — and taking that lock is what
    /// fails on a read-only home.
    #[test]
    fn a_minted_key_is_never_due_for_refresh() {
        let (_t, s) = store();
        seed_key(&s, "srv");
        assert_eq!(s.bearer("srv", &|_| unreachable!()).unwrap(), "minted");
        // …including when a device grant with the same clock WOULD be due.
        assert!(!s.get("srv").unwrap().unwrap().needs_refresh(i64::MAX / 2));
    }

    /// A 401 on a minted key is the server refusing it, and there is nothing to rotate. The
    /// message must name what to do — and `docli login` alone would be wrong guidance, since it
    /// is not what created this credential.
    #[test]
    fn a_refused_minted_key_says_what_to_do_and_never_refreshes() {
        let (_t, s) = store();
        seed_key(&s, "srv");
        let e = format!(
            "{:#}",
            s.refresh_single_flight("srv", &|_| unreachable!(), Some("minted"))
                .unwrap_err()
        );
        assert!(e.contains("--token"), "{e}");
        // …and the key is KEPT. A single 401 from a sync request is weaker evidence than the
        // token endpoint's `invalid_grant`, nothing was consumed by the failure, and the people
        // this credential serves are the ones who cannot re-paste one on demand.
        assert_eq!(
            s.get("srv").unwrap().unwrap().access_token,
            "minted",
            "a 401 must not destroy a key that may still be good"
        );
    }

    /// Identity for `remove_if_current` is the refresh token when there is one and the access
    /// token otherwise — one question, asked of the only field the credential has.
    #[test]
    fn a_minted_key_is_removed_only_while_it_is_still_the_stored_one() {
        let (_t, s) = store();
        seed_key(&s, "srv");
        assert!(!s.remove_if_current("srv", "some-other-key").unwrap());
        assert!(s.get("srv").unwrap().is_some());
        assert!(s.remove_if_current("srv", "minted").unwrap());
        assert!(s.get("srv").unwrap().is_none());
    }

    /// A credentials file written by any CLI up to 0.1.14 has both fields as bare values. It
    /// must keep parsing — an upgrade that silently signs the user out would send them to a
    /// browser they may not have.
    #[test]
    fn a_pre_0_1_15_credentials_file_still_parses() {
        let f: CredsFile = serde_json::from_str(
            r#"{"servers":{"https://docli.ru":{"access_token":"a","refresh_token":"r",
               "expires_at":123,"install_id":"i"}}}"#,
        )
        .expect("the old shape must still parse");
        let c = &f.servers["https://docli.ru"];
        assert_eq!(c.refresh_token.as_deref(), Some("r"));
        assert_eq!(c.expires_at, Some(123));
    }

    fn seed(s: &CredsStore, server: &str, expires_at: i64) {
        s.put(
            server,
            ServerCreds {
                access_token: "old-access".into(),
                refresh_token: Some("r1".into()),
                expires_at: Some(expires_at),
                install_id: "i1".into(),
            },
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn open_rehardens_a_loose_existing_file_and_refuses_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        // A 0666 credentials file from outside our writes is rewritten to 0600 on open, with
        // its bytes intact (Codex rounds 20-21).
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // The credentials live in `home/auth` — a level below the mirror, so an agent sandbox
        // can be granted the refresh path without being granted the cache.
        let auth = home.join("auth");
        fs::create_dir_all(&auth).unwrap();
        let f = auth.join("credentials.json");
        fs::write(&f, "{\"servers\":{}}").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o666)).unwrap();
        let s = CredsStore::open(home.clone()).unwrap();
        let mode = fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        assert!(s.get("x").unwrap().is_none());
        // A symlinked credentials entry refuses outright.
        fs::remove_file(&f).unwrap();
        fs::write(auth.join("outside"), "{}").unwrap();
        std::os::unix::fs::symlink(auth.join("outside"), &f).unwrap();
        let err = match CredsStore::open(home) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a symlinked credentials entry must refuse"),
        };
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn creds_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, s) = store();
        seed(&s, "https://docli.ru", 0);
        let mode = fs::metadata(s.file_path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
    }

    #[test]
    fn fresh_token_is_returned_without_refreshing() {
        let (_t, s) = store();
        seed(&s, "srv", now_unix() + 3600);
        let calls = AtomicUsize::new(0);
        let tok = s
            .bearer("srv", &|_| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("must not refresh a fresh token")
            })
            .unwrap();
        assert_eq!(tok, "old-access");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn single_flight_two_sequential_expiries_refresh_once() {
        // The lock's semantic half testable in-process: the second caller re-reads under the
        // lock and becomes a no-op reader of the first refresh's work.
        let (_t, s) = store();
        seed(&s, "srv", 0);
        let calls = AtomicUsize::new(0);
        let refresh = |_rt: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(RefreshOutcome::Rotated {
                access_token: "new-access".into(),
                refresh_token: "r2".into(),
                expires_in: 3600,
            })
        };
        assert_eq!(s.bearer("srv", &refresh).unwrap(), "new-access");
        assert_eq!(s.bearer("srv", &refresh).unwrap(), "new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly ONE refresh");
        // The rotated refresh token is persisted (single-use lineage).
        assert_eq!(
            s.get("srv").unwrap().unwrap().refresh_token.as_deref(),
            Some("r2")
        );
    }

    #[test]
    fn invalid_grant_is_terminal_and_never_retried() {
        let (_t, s) = store();
        seed(&s, "srv", 0);
        let calls = AtomicUsize::new(0);
        let err = s
            .bearer("srv", &|_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(RefreshOutcome::InvalidGrant)
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("docli login"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(s.get("srv").unwrap().is_none(), "dead creds are removed");
    }

    #[test]
    fn suspended_retries_with_reread_creds_and_keeps_them() {
        let (_t, s) = store();
        seed(&s, "srv", 0);
        let calls = AtomicUsize::new(0);
        let tok = s
            .bearer("srv", &|_| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(RefreshOutcome::Suspended {
                        retry_after_secs: 0,
                    })
                } else {
                    Ok(RefreshOutcome::Rotated {
                        access_token: "after-suspend".into(),
                        refresh_token: "r2".into(),
                        expires_in: 3600,
                    })
                }
            })
            .unwrap();
        assert_eq!(tok, "after-suspend");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_server_rejected_token_forces_rotation_despite_local_freshness() {
        // Codex round 2: a 401 means the server refused THIS token (revocation, server-side
        // rotation, clock skew) — the under-lock freshness check must not hand the same token
        // straight back for an identical second 401.
        let (_t, s) = store();
        seed(&s, "srv", now_unix() + 3600); // locally FRESH
        let calls = AtomicUsize::new(0);
        let tok = s
            .refresh_single_flight(
                "srv",
                &|_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(RefreshOutcome::Rotated {
                        access_token: "rotated".into(),
                        refresh_token: "r2".into(),
                        expires_in: 3600,
                    })
                },
                Some("old-access"),
            )
            .unwrap();
        assert_eq!(tok, "rotated");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the rotation genuinely ran"
        );
        // …while a DIFFERENT stored token (another process already rotated) short-circuits.
        let tok = s
            .refresh_single_flight(
                "srv",
                &|_| panic!("must not rotate twice"),
                Some("old-access"),
            )
            .unwrap();
        assert_eq!(tok, "rotated");
    }

    #[test]
    fn a_logout_does_not_delete_a_credential_a_concurrent_login_replaced() {
        let (_t, s) = store();
        seed(&s, "srv", 0);
        // The logout read `r1`; meanwhile a login stored a fresh lineage.
        s.put(
            "srv",
            ServerCreds {
                access_token: "new-access".into(),
                refresh_token: Some("r2".into()),
                expires_at: Some(now_unix() + 3600),
                install_id: "i1".into(),
            },
        )
        .unwrap();
        assert!(!s.remove_if_current("srv", "r1").unwrap());
        assert!(s.get("srv").unwrap().is_some(), "the new lineage survives");
        // …and the honest case still removes.
        assert!(s.remove_if_current("srv", "r2").unwrap());
        assert!(s.get("srv").unwrap().is_none());
    }

    #[test]
    fn install_id_is_minted_once_and_survives_logout() {
        let (_t, s) = store();
        let a = s.install_id("srv").unwrap();
        let b = s.install_id("srv").unwrap();
        assert_eq!(a, b);
        seed(&s, "srv", 0);
        s.remove("srv").unwrap();
        assert_eq!(s.install_id("srv").unwrap(), a, "survives a logout");
    }
}
