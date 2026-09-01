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
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    /// The device grant's install key (D27d) — minted once per `~/.docli`, stable thereafter.
    pub install_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredsFile {
    #[serde(default)]
    servers: BTreeMap<String, ServerCreds>,
}

pub struct CredsStore {
    dir: PathBuf,
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

impl CredsStore {
    /// `~/.docli` (override: `DOCLI_HOME` — tests and odd setups).
    pub fn open_default() -> Result<Self> {
        let dir = match std::env::var_os("DOCLI_HOME") {
            Some(d) => PathBuf::from(d),
            None => std::env::home_dir()
                .context("cannot determine the home directory (set DOCLI_HOME)")?
                .join(".docli"),
        };
        Self::open(dir)
    }

    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict_dir(&dir)?;
        let store = CredsStore { dir };
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
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(store.lock_path())?;
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

    fn file_path(&self) -> PathBuf {
        self.dir.join("credentials.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("creds.lock")
    }

    fn read(&self) -> Result<CredsFile> {
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
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
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
    pub fn remove_if_current(&self, server: &str, refresh_token: &str) -> Result<bool> {
        let removed = std::cell::Cell::new(false);
        self.mutate(&|f| {
            let current_matches = f
                .servers
                .get(server)
                .is_some_and(|c| c.refresh_token == refresh_token);
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
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
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
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
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
        Ok(self.get(server)?.map(|c| c.access_token))
    }

    /// A valid bearer for `server`, refreshing single-flight when needed.
    /// `refresh` performs the network exchange: `(refresh_token) → (access, refresh, expires_in)`.
    pub fn bearer(
        &self,
        server: &str,
        refresh: &dyn Fn(&str) -> Result<RefreshOutcome>,
    ) -> Result<String> {
        let Some(c) = self.get(server)? else {
            bail!("not signed in to {server} - run `docli login`");
        };
        if c.expires_at > now_unix() + REFRESH_SKEW_SECS {
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
        // The advisory lock: the loser of a concurrent refresh would trip reuse detection and
        // revoke the LINEAGE — both processes lose the credential, recovery is a browser
        // round-trip an agent cannot perform.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
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
            let usable = c.expires_at > now_unix() + REFRESH_SKEW_SECS
                && rejected != Some(c.access_token.as_str());
            if usable {
                return Ok(c.access_token);
            }
            match refresh(&c.refresh_token)? {
                RefreshOutcome::Rotated {
                    access_token,
                    refresh_token,
                    expires_in,
                } => {
                    let fresh = ServerCreds {
                        access_token: access_token.clone(),
                        refresh_token,
                        expires_at: now_unix() + expires_in,
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

    fn seed(s: &CredsStore, server: &str, expires_at: i64) {
        s.put(
            server,
            ServerCreds {
                access_token: "old-access".into(),
                refresh_token: "r1".into(),
                expires_at,
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
        fs::create_dir_all(&home).unwrap();
        let f = home.join("credentials.json");
        fs::write(&f, "{\"servers\":{}}").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o666)).unwrap();
        let s = CredsStore::open(home.clone()).unwrap();
        let mode = fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        assert!(s.get("x").unwrap().is_none());
        // A symlinked credentials entry refuses outright.
        fs::remove_file(&f).unwrap();
        fs::write(home.join("outside"), "{}").unwrap();
        std::os::unix::fs::symlink(home.join("outside"), &f).unwrap();
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
        assert_eq!(s.get("srv").unwrap().unwrap().refresh_token, "r2");
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
                refresh_token: "r2".into(),
                expires_at: now_unix() + 3600,
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
