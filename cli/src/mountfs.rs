// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Mount-root discipline (v0.28.0 D2): the ownership marker + claim rule, the cross-config
//! advisory lock, symlink/reparse refusal, write containment, the FS read-only attribute, and
//! the `CACHE_INCOMPLETE.docli` marker.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

pub const MOUNT_MARKER: &str = "MOUNT.docli";
pub const INCOMPLETE_MARKER: &str = "CACHE_INCOMPLETE.docli";

/// Refuse a mount that IS a symlink or that CONTAINS symlinked entries (re-checked each run):
/// a `notes → ../outside` link inside the mirror would let a state-driven write escape the
/// canonical root. On Windows this also covers junctions/reparse points (they read as symlinks
/// through `symlink_metadata`).
///
/// Deliberate narrowing of the plan's "ancestors" wording, with the argument stated: ancestors
/// ABOVE the mount are not policed, because the mount root is CANONICALIZED before use and every
/// write goes through [`contained_join`] (Normal components only) under that canonical root — an
/// ancestor symlink (macOS's own `/var → /private/var`) cannot redirect a write, while refusing
/// it would refuse half of every real filesystem. What CAN redirect a write is a link at or
/// below the root; those are what this refuses.
pub fn refuse_symlinks(mount: &Path) -> Result<()> {
    let md = fs::symlink_metadata(mount).with_context(|| format!("stat {}", mount.display()))?;
    if md.file_type().is_symlink() {
        bail!(
            "{} is a symlink - deliveries could escape the mount; refuse to sync through links",
            mount.display()
        );
    }
    let mut stack = vec![mount.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("scanning {}", dir.display()))? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                bail!(
                    "{} is a symlink inside the mirror - deliveries could escape the mount; \
                     remove it",
                    entry.path().display()
                );
            }
            if ft.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Validate that a RELATIVE mirror path resolves INSIDE the canonical root before any write —
/// applied paths include state-driven moves/deletes, which trust stored paths (D2). The paths
/// we build are always `/`-joined relative strings; this refuses absolute paths, `..`, and (on
/// Windows) drive/UNC shapes.
pub fn contained_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        bail!("path {rel:?} is absolute - refusing (containment)");
    }
    for c in p.components() {
        match c {
            std::path::Component::Normal(_) => {}
            _ => bail!("path {rel:?} escapes the mount root - refusing (containment)"),
        }
    }
    Ok(root.join(p))
}

/// CANONICAL containment, for a consumer that is about to OPEN the path (`docli read`) — and it
/// returns the RESOLVED path rather than a yes/no, deliberately.
///
/// [`contained_join`] is lexical, and a read holds no mount claim, so a symlink planted inside a
/// mirror would otherwise let a mirror-relative address name any file the user can open.
/// Canonicalizing both ends resolves every link first; it also refuses a path that does not
/// exist, which is the caller's «gone» answer rather than a separate stat.
///
/// **Handing back the resolved path is what makes the check worth having.** A caller that
/// verified one path and then opened the original would leave a swap window between the two, and
/// a lock-free reader has nothing else closing it. Opening what was actually verified removes
/// every intermediate link from that window.
///
/// **`root` must ALREADY be canonical, or anchored to something that is** — this function does
/// not canonicalize it, deliberately. Canonicalizing both ends makes a symlinked ROOT vacuous:
/// root and file resolve through the same link, `starts_with` holds, and the containment check
/// passes over a directory that is somewhere else entirely. That is the round-17 defect
/// («a swapped root canonicalizes consistently against itself») one level down, and the
/// mount arm only escapes it because `verify_mount_identity` refuses a symlinked mount root
/// first. A caller with no such anchor must build `root` by joining onto a canonical path it
/// trusts, so that a link anywhere in the chain lands the file outside the expected prefix.
///
/// It returns a THREE-way answer rather than an `Option`, because the caller's three answers are
/// genuinely different: nothing is there, something is there but not ours, and we could not
/// find out. Collapsing the third into either of the first two puts «this mirror does not hold
/// it» on a file nobody was able to look at.
pub fn canonical_within(root: &Path, abs: &Path) -> Containment {
    let a = match abs.canonicalize() {
        Ok(a) => a,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Containment::Missing,
        Err(e) => return Containment::Unresolvable(e),
    };
    if a.starts_with(root) {
        Containment::Inside(a)
    } else {
        Containment::Escaped
    }
}

/// What [`canonical_within`] found.
pub enum Containment {
    /// Resolved, and inside the root. Carries the RESOLVED path — open this one, not the
    /// original, or the check and the open are about different files.
    Inside(PathBuf),
    /// A component of the path does not exist.
    Missing,
    /// It resolves, but outside the root — a link leading out of the mirror.
    Escaped,
    /// The resolve itself failed: a permission, an ACL, an I/O fault. NOT an answer about
    /// whether the file is there.
    Unresolvable(std::io::Error),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct MountMarker {
    /// Absolute path of the owning `.docli/` — TWO DISTINCT configs resolving to one canonical
    /// mount are what this catches (the per-`docli.toml` geometry rules cannot see a second
    /// repo/worktree mounting the same physical dir).
    owner: String,
    workspace: Uuid,
}

/// The mount handle: the claimed + locked mount root. Holds the advisory lock for the run
/// (dropping it unlocks). The lock lives ON `MOUNT.docli` itself — keyed on the mount's
/// canonical physical path by construction, so two configs serialize on the same file.
pub struct MountHandle {
    pub root: PathBuf,
    _lock: File,
}

/// Claim a mount, retrying the lock briefly. TESTS ONLY.
///
/// `try_lock` fails fast by design (a clear message beats a silent queue), and that is right for
/// the product. It makes a `drop`-then-reclaim assertion racy inside a MULTI-THREADED process
/// that also forks, though, and the test binary is exactly that: an advisory lock lives on the
/// open file description, which `fork` DUPLICATES, so a subprocess spawned by an unrelated test
/// on another thread holds the lock until it `exec`s and CLOEXEC closes the copy. Microseconds,
/// but enough to fail a reclaim that must succeed.
///
/// Worth knowing beyond the tests: **nothing in the product may spawn a subprocess while holding
/// a mount lock**, or the lock outlives its `MountHandle` by the same mechanism. Today nothing
/// does — `validate_geometry`'s `git` call happens before any claim, and `status`'s shell probe
/// holds no mount.
#[cfg(test)]
fn claim_mount_eventually(mount: &Path, owner: &Path, ws: Uuid) -> Result<MountHandle> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match claim_mount(mount, owner, ws) {
            Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

/// Typed marker for «somebody else holds this mount's lock» (v0.28.6 D3). `try_lock` fails fast
/// by design, so this is a routine outcome, not a fault: it joins the partial-success class on
/// the `--check` path, and the `SessionStart` hook reports it as its own branch rather than as
/// staleness.
#[derive(Debug)]
pub struct MountBusy;

impl std::fmt::Display for MountBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another docli run holds this mount")
    }
}
impl std::error::Error for MountBusy {}

/// Is this error a lock contention?
pub fn is_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<MountBusy>().is_some()
}

impl std::fmt::Debug for MountHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountHandle")
            .field("root", &self.root)
            .finish()
    }
}

/// Claim or re-open a mount root for `owner`/`ws`.
///
/// First claim requires an EMPTY dir (a mirror starts empty by definition; refusing
/// pre-existing files is what makes every later write occupant-free — the read-only CLI has no
/// keep-both machinery to protect an untracked occupant). A mount claimed by a DIFFERENT owner
/// refuses. A concurrent invocation fails fast on the lock (two apply passes over one mirror
/// would recreate the mutual-destruction class the geometry rules exclude, with the prune
/// raising the stakes from churn to deletion).
pub fn claim_mount(mount: &Path, owner_docli_dir: &Path, ws: Uuid) -> Result<MountHandle> {
    if fs::symlink_metadata(mount)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!(
            "{} is a symlink - refuse to mount through links",
            mount.display()
        );
    }
    fs::create_dir_all(mount).with_context(|| format!("creating {}", mount.display()))?;
    refuse_symlinks(mount)?;
    let root = fs::canonicalize(mount).context("canonicalizing the mount root")?;
    let marker_path = root.join(MOUNT_MARKER);
    let owner = fs::canonicalize(owner_docli_dir)
        .unwrap_or_else(|_| owner_docli_dir.to_path_buf())
        .display()
        .to_string();

    if !marker_path.exists() {
        // First claim: the dir must be EMPTY (nothing but nothing).
        let occupied = fs::read_dir(&root)?.next().is_some();
        if occupied {
            bail!(
                "{} is not empty - a mirror starts empty; point the mount at a fresh directory \
                 (or delete the leftovers deliberately)",
                root.display()
            );
        }
        let marker = MountMarker {
            owner: owner.clone(),
            workspace: ws,
        };
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker_path)
            .with_context(|| format!("claiming {}", marker_path.display()))?;
        f.write_all(serde_json::to_string_pretty(&marker)?.as_bytes())?;
    }

    // Open + verify + LOCK the marker (fail fast — a clear message beats a silent queue).
    let f = OpenOptions::new()
        .read(true)
        .open(&marker_path)
        .with_context(|| format!("opening {}", marker_path.display()))?;
    if f.try_lock().is_err() {
        // TYPED, not just a message (v0.28.6 D3): two wired agents starting together — or an
        // agent starting while `docli sync` runs in a terminal — is an ordinary session-start
        // state, and it is neither fresh, stale, nor a timeout. The freshness path has to be
        // able to tell it apart, and one mount's contention must not abort the others.
        return Err(anyhow::Error::new(MountBusy).context(format!(
            "another docli sync/doctor is running on {} - one run per mount at a time",
            root.display()
        )));
    }
    let raw = fs::read_to_string(&marker_path)?;
    let marker: MountMarker = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not a docli mount marker", marker_path.display()))?;
    if marker.owner != owner || marker.workspace != ws {
        bail!(
            "{} is already claimed by another project ({} / workspace {}) - a mirror has exactly \
             one owner; pick a different mount dir or delete that mirror deliberately",
            root.display(),
            marker.owner,
            marker.workspace
        );
    }
    Ok(MountHandle { root, _lock: f })
}

/// Lock-free mount-identity check for READ-ONLY consumers (search — Codex round 17): the
/// root must not itself be a symlink (no-follow stat) and must carry OUR `MOUNT.docli`
/// (owner + workspace match). A swapped root is either a symlink (refused here) or a foreign
/// directory whose marker cannot match. No lock is taken and no file is created — a running
/// sync is not disturbed and an unsynced mount simply fails the check.
pub fn verify_mount_identity(mount: &Path, owner_docli_dir: &Path, ws: Uuid) -> bool {
    if fs::symlink_metadata(mount)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
    {
        return false;
    }
    let owner = fs::canonicalize(owner_docli_dir)
        .unwrap_or_else(|_| owner_docli_dir.to_path_buf())
        .display()
        .to_string();
    // The MARKER itself must be a regular file. `read_to_string` follows symlinks, so
    // `src/MOUNT.docli -> ../real-mirror/MOUNT.docli` would let any directory borrow another
    // mirror's identity — and this check is what `uninstall --purge` deletes on.
    let marker_path = mount.join(MOUNT_MARKER);
    if !fs::symlink_metadata(&marker_path)
        .map(|m| m.file_type().is_file())
        .unwrap_or(false)
    {
        return false;
    }
    let Ok(raw) = fs::read_to_string(&marker_path) else {
        return false;
    };
    serde_json::from_str::<MountMarker>(&raw)
        .map(|m| m.owner == owner && m.workspace == ws)
        .unwrap_or(false)
}

/// Set/clear the FS read-only attribute. Advisory (editors unlink-and-recreate — D3's honest
/// contract says so out loud), but it makes the accidental `>>` fail. Lifted on ALL mutating
/// arms: on Windows the attribute also blocks delete and rename-over, so the trashed-removal,
/// move, AND prune arms lift it too.
pub fn set_readonly(path: &Path, ro: bool) -> Result<()> {
    let md = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mut perms = md.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(ro);
    fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

/// The transient names [`write_atomic`] uses. One recognizer shared by the writer, the
/// `sync --full` sweep, and doctor's classifier — a crash between temp-write and rename leaves
/// one of these behind, and all three must agree on what it looks like. Exactly the writer's
/// shape (16 hex chars — round-3 F6): the sweep DELETES matches, so the recognizer must not be
/// looser than the generator; a server note named `.docli-write-x.tmp` is not park-protected
/// (it doesn't end in `.docli`), and tightness is what keeps such a name out of the blast
/// radius unless it matches the full 16-hex form.
pub fn is_write_temp(name: &str) -> bool {
    // LOWERCASE hex only (Codex round 2): `hex::encode` never emits A-F, and the sweep
    // deletes matches — an uppercase spelling is an untracked occupant, not our residue.
    name.strip_prefix(".docli-write-")
        .and_then(|r| r.strip_suffix(".tmp"))
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Remove crash residue ([`is_write_temp`] files) anywhere under `root` — the `sync --full`
/// half of the write_atomic crash story: the error path below cleans up a FAILED swap, but
/// process death between temp-write and rename can't clean up after itself, and the temp is
/// read-only (a naive `rm` fails on Windows), so the authoritative resync owns the removal.
/// BEST-EFFORT by design (round-3 F5): a cleanup must never be able to fail the authoritative
/// repair it rides on — unreadable dirs and stuck removals are skipped, and doctor still
/// names whatever survives.
pub fn sweep_write_temps(root: &Path) -> usize {
    let mut removed = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_write_temp)
                && remove_owned_file(&p).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

/// Write mirror bytes ATOMICALLY (D12.1): a temp file in the SAME directory, made read-only,
/// then renamed over the target — a racing agent reader sees the old bytes or the new bytes,
/// never a truncated middle (`fs::write` truncates in place). Same-dir keeps the rename on one
/// filesystem; the read-only bit rides the temp's permissions through the rename. On Windows a
/// rename over a READ-ONLY target fails, so callers overwriting an existing tracked file still
/// lift the target's read-only bit first (the established shape). The temp name is transient
/// and random — it deliberately does NOT end in `.docli` (that would be the control namespace).
///
/// Windows caveat (round-2 R1, unverifiable on this build host): `MoveFileExW` needs DELETE
/// access on the destination, which a reader holding the file without `FILE_SHARE_DELETE`
/// blocks — a strictly STRONGER requirement than the plain write it replaced, and a write
/// failure here aborts the page (never advances the cursor). So a failed swap over an
/// EXISTING target falls back to the pre-D12 direct write: atomic when the OS allows it,
/// never a larger failure set than before. The release smoke's Windows leg is the check.
pub fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    use rand::RngCore;
    let dir = target
        .parent()
        .with_context(|| format!("no parent dir for {}", target.display()))?;
    let mut suffix = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut suffix);
    let tmp = dir.join(format!(".docli-write-{}.tmp", hex::encode(suffix)));
    let write = (|| -> Result<()> {
        fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        set_readonly(&tmp, true)?;
        if let Err(rename_err) = fs::rename(&tmp, target) {
            if target.exists() {
                // The share-blocked-rename fallback (doc above). The temp is removed by the
                // error-path cleanup below if this write fails too.
                fs::write(target, bytes)
                    .with_context(|| format!("writing {} (rename fallback)", target.display()))?;
                set_readonly(target, true)?;
                let _ = set_readonly(&tmp, false);
                let _ = fs::remove_file(&tmp);
            } else {
                return Err(rename_err)
                    .with_context(|| format!("renaming into {}", target.display()));
            }
        }
        Ok(())
    })();
    if write.is_err() {
        // Clean up a FAILED swap. Process death can't reach this arm — that residue is owned
        // by `sweep_write_temps` (sync --full) and named by doctor as `crash-residue`.
        let _ = set_readonly(&tmp, false);
        let _ = fs::remove_file(&tmp);
    }
    write
}

/// Remove a file the CLI owns, lifting the read-only attribute first (the Windows shape).
pub fn remove_owned_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(()); // idempotent - a crashed previous cycle may have removed it already
    }
    let _ = set_readonly(path, false);
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}

/// Maintain the visible incompleteness marker: present exactly when the cursor has not reached
/// head OR the from-zero flag is set OR a TRANSIENT park exists (a STRUCTURAL park alone leaves
/// it absent — the two park classes fail differently, D2a).
pub fn set_incomplete_marker(mount_root: &Path, incomplete: bool) -> Result<()> {
    let p = mount_root.join(INCOMPLETE_MARKER);
    if incomplete {
        if !p.exists() {
            fs::write(
                &p,
                "This mirror is INCOMPLETE (mid-sync, mid-repair, or holding parked deliveries).\n\
                 Run `docli sync`; `docli sync --check` explains. Do not treat absence of a file\n\
                 here as absence on the server.\n",
            )?;
        }
    } else if p.exists() {
        fs::remove_file(&p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn a_symlinked_marker_cannot_borrow_another_mirrors_identity() {
        // `--purge` deletes on the strength of this check: a directory that merely POINTS at a
        // real mirror's marker must not be able to claim it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let control = root.join(".docli");
        fs::create_dir_all(&control).unwrap();
        let ws = uuid::Uuid::from_u128(7);
        let owner = fs::canonicalize(&control).unwrap().display().to_string();
        let real = root.join("real-mirror");
        fs::create_dir_all(&real).unwrap();
        fs::write(
            real.join(MOUNT_MARKER),
            format!("{{\"owner\":\"{owner}\",\"workspace\":\"{ws}\"}}"),
        )
        .unwrap();
        assert!(verify_mount_identity(&real, &control, ws));

        let borrowed = root.join("src");
        fs::create_dir_all(&borrowed).unwrap();
        std::os::unix::fs::symlink(real.join(MOUNT_MARKER), borrowed.join(MOUNT_MARKER)).unwrap();
        assert!(
            !verify_mount_identity(&borrowed, &control, ws),
            "a symlinked marker must not confer ownership"
        );
    }

    use super::*;

    #[test]
    fn first_claim_requires_an_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tmp.path().join("m");
        fs::create_dir_all(&mount).unwrap();
        fs::write(mount.join("pre-existing.txt"), "x").unwrap();
        let owner = tmp.path().join(".docli");
        fs::create_dir_all(&owner).unwrap();
        let err = claim_mount(&mount, &owner, Uuid::from_u128(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not empty"), "{err}");
        // Empty dir claims fine, and re-claim by the same owner succeeds.
        let mount2 = tmp.path().join("m2");
        let h = claim_mount(&mount2, &owner, Uuid::from_u128(1)).unwrap();
        drop(h);
        claim_mount(&mount2, &owner, Uuid::from_u128(1)).unwrap();
    }

    #[test]
    fn a_second_owner_is_refused_by_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tmp.path().join("m");
        let owner_a = tmp.path().join("proj-a/.docli");
        let owner_b = tmp.path().join("proj-b/.docli");
        fs::create_dir_all(&owner_a).unwrap();
        fs::create_dir_all(&owner_b).unwrap();
        drop(claim_mount(&mount, &owner_a, Uuid::from_u128(1)).unwrap());
        // `claim_mount_eventually`: the OWNERSHIP refusal is what this test is about, and a
        // concurrently-forked sibling test can still hold the released lock for a moment (see
        // that helper) — which would surface as the LOCK refusal instead.
        let err = claim_mount_eventually(&mount, &owner_b, Uuid::from_u128(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already claimed"), "{err}");
        // Same .docli but a DIFFERENT workspace is a different owner too.
        let err = claim_mount_eventually(&mount, &owner_a, Uuid::from_u128(2))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already claimed"), "{err}");
    }

    #[test]
    fn concurrent_claims_fail_fast_on_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tmp.path().join("m");
        let owner = tmp.path().join(".docli");
        fs::create_dir_all(&owner).unwrap();
        let held = claim_mount(&mount, &owner, Uuid::from_u128(1)).unwrap();
        let err = claim_mount(&mount, &owner, Uuid::from_u128(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("another docli sync"), "{err}");
        drop(held);
        // Releasing lets the next claim succeed — EVENTUALLY. See `claim_mount_eventually`.
        claim_mount_eventually(&mount, &owner, Uuid::from_u128(1)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_mount_or_interior_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let owner = tmp.path().join(".docli");
        fs::create_dir_all(&owner).unwrap();
        // The mount itself is a link.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = claim_mount(&link, &owner, Uuid::from_u128(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        // A link INSIDE the mirror (re-checked each run — created after the claim).
        let mount = tmp.path().join("m");
        drop(claim_mount(&mount, &owner, Uuid::from_u128(1)).unwrap());
        std::os::unix::fs::symlink(tmp.path(), mount.join("escape")).unwrap();
        let err = claim_mount(&mount, &owner, Uuid::from_u128(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside the mirror"), "{err}");
    }

    #[test]
    fn containment_refuses_escapes() {
        let root = Path::new("/tmp/x");
        assert!(contained_join(root, "a/b.md").is_ok());
        assert!(contained_join(root, "../evil").is_err());
        assert!(contained_join(root, "/abs").is_err());
        assert!(contained_join(root, "a/../../evil").is_err());
    }

    #[test]
    fn incomplete_marker_toggles() {
        let tmp = tempfile::tempdir().unwrap();
        set_incomplete_marker(tmp.path(), true).unwrap();
        assert!(tmp.path().join(INCOMPLETE_MARKER).exists());
        set_incomplete_marker(tmp.path(), false).unwrap();
        assert!(!tmp.path().join(INCOMPLETE_MARKER).exists());
    }

    #[test]
    fn write_atomic_leaves_no_temp_and_carries_readonly() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a.md");
        write_atomic(&target, b"one").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one");
        assert!(fs::metadata(&target).unwrap().permissions().readonly());
        // Overwrite follows the established shape: lift, swap, still read-only after.
        set_readonly(&target, false).unwrap();
        write_atomic(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert!(fs::metadata(&target).unwrap().permissions().readonly());
        // No transient temp survives a successful swap.
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".docli-write-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn sweep_removes_readonly_crash_residue_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let stray = sub.join(".docli-write-deadbeef01020304.tmp");
        fs::write(&stray, b"partial").unwrap();
        set_readonly(&stray, true).unwrap();
        fs::write(tmp.path().join("keep.md"), b"user").unwrap();
        // The writer's generated names and the recognizer agree — and the recognizer is
        // EXACTLY the writer's 16-hex shape (the sweep deletes matches, so near-misses must
        // not match: round-4 F-H).
        assert!(is_write_temp(".docli-write-0011223344556677.tmp"));
        assert!(!is_write_temp("note.md"));
        assert!(!is_write_temp(".docli"));
        assert!(!is_write_temp(".docli-write-0011223344556677.tmp2"));
        assert!(!is_write_temp(".docli-write-001122334455667.tmp"), "15 hex");
        assert!(
            !is_write_temp(".docli-write-00112233445566778.tmp"),
            "17 hex"
        );
        assert!(
            !is_write_temp(".docli-write-00112233445566zz.tmp"),
            "non-hex"
        );
        assert!(!is_write_temp(".docli-write-.tmp"));
        assert!(
            !is_write_temp(".docli-write-DEADBEEFDEADBEEF.tmp"),
            "uppercase is not ours (Codex round 2)"
        );
        let removed = sweep_write_temps(tmp.path());
        assert_eq!(removed, 1);
        assert!(!stray.exists());
        assert!(tmp.path().join("keep.md").exists());
    }
}
