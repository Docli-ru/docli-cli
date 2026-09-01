// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli self-update` (v0.28.0 D9) — swap the binary against the artifacts host + the SIGNED
//! version manifest.
//!
//! Authenticity, not just integrity: SHA-256 sums co-hosted with the binaries prove only that
//! two mutable objects match, so releases are SIGNED (minisign/ed25519). The release key is
//! held OFFLINE (Bitwarden escrow, the KEK precedent — never in the server keyring); its PUBLIC
//! key is pinned INSIDE this binary; the manifest is signed. A bucket compromise can poison a
//! FIRST install (the accepted residual, shared with every curl|sh installer) but never an
//! UPDATE — verification here uses the verifier and key already installed.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The artifacts host. The default binds to step 0's recorded infrastructure (the public
/// `docli-artifacts` bucket); `/release-cli` verifies the LIVE value against the applied
/// bucket's `full_name` (timeweb prefixes bucket names randomly) before the first publish —
/// this constant is corrected there if the prefix differs. `DOCLI_ARTIFACTS_BASE` overrides
/// for testing.
pub const ARTIFACTS_BASE: &str = "https://s3.twcstorage.ru/docli-artifacts";

/// The pinned minisign PUBLIC key (base64 body of the `.pub` file). EMPTY until the operator
/// mints the release keypair (offline, escrowed) — and an empty pin REFUSES self-update rather
/// than verifying nothing.
pub const RELEASE_PUBKEY_B64: &str = include_str!("../keys/release.pub.b64");

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// target triple-ish key (`{os}-{arch}`) → artifact.
    pub targets: std::collections::BTreeMap<String, ManifestTarget>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestTarget {
    pub file: String,
    pub sha256: String,
}

/// `major.minor.patch` → a comparable tuple; `None` on anything else (the caller then treats
/// the versions as incomparable and allows the swap — release manifests are ours and well-formed).
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    // No trim (Codex round 3): fail-closed means EXACTLY `x.y.z` — release manifests are ours
    // and never carry whitespace, so decoration is itself a red flag.
    let mut it = v.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    let pat = it.next()?.parse().ok()?;
    it.next().is_none().then_some((maj, min, pat))
}

pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn artifacts_base() -> String {
    std::env::var("DOCLI_ARTIFACTS_BASE").unwrap_or_else(|_| ARTIFACTS_BASE.to_string())
}

/// Verify + parse a manifest against the pinned key. Public for tests.
pub fn verify_manifest(manifest_bytes: &[u8], sig: &str, pubkey_b64: &str) -> Result<Manifest> {
    let key = pubkey_b64.trim();
    if key.is_empty() {
        bail!(
            "this build carries no pinned release key - self-update is disabled until the \
             release keypair is minted (reinstall via docli.ru/install.sh instead)"
        );
    }
    let pk = minisign_verify::PublicKey::from_base64(key)
        .context("the pinned release key is malformed")?;
    let signature =
        minisign_verify::Signature::decode(sig).context("the manifest signature is malformed")?;
    pk.verify(manifest_bytes, &signature, false)
        .context("the manifest SIGNATURE does not verify - refusing to update")?;
    serde_json::from_slice(manifest_bytes).context("parsing the verified manifest")
}

pub fn run() -> Result<i32> {
    let base = artifacts_base();
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let get = |path: &str| -> Result<Vec<u8>> {
        let resp = http
            .get(format!("{base}/{path}"))
            .send()
            .with_context(|| format!("fetching {path}"))?;
        if !resp.status().is_success() {
            bail!("GET {path}: {}", resp.status());
        }
        Ok(resp.bytes()?.to_vec())
    };

    let manifest_bytes = get("manifest.json")?;
    let sig = String::from_utf8(get("manifest.json.minisig")?)
        .context("the manifest signature is not UTF-8")?;
    // The signature verifies BEFORE anything is trusted — a bucket compromise cannot push an
    // update.
    let manifest = verify_manifest(&manifest_bytes, &sig, RELEASE_PUBKEY_B64)?;

    let current = env!("CARGO_PKG_VERSION");
    if manifest.version == current {
        crate::ui::ok(&format!("docli-cli {current} is already the latest"));
        return Ok(0);
    }
    // A validly SIGNED old manifest is still a downgrade an artifacts-host attacker can replay
    // (Codex round 1) — "a bucket compromise cannot push an update" must cover pushing
    // YESTERDAY's update too. Downgrades are a manual reinstall, never self-update. FAIL
    // CLOSED on anything unparseable (Codex round 2): our manifests are plain `x.y.z` by
    // construction, so a `0.2.0-beta.1`-shaped version reaching this comparison is itself
    // suspect — skipping the check would let it bypass the refusal.
    match (parse_semver(&manifest.version), parse_semver(current)) {
        (Some(m), Some(c)) if m > c => {}
        (Some(_), Some(_)) => bail!(
            "the manifest offers {} but this binary is {current} - refusing a downgrade \
             (reinstall via docli.ru/install.sh if you really want an older version)",
            manifest.version
        ),
        _ => bail!(
            "the manifest version {} is not a plain x.y.z (this binary: {current}) - refusing \
             (release manifests are always plain semver)",
            manifest.version
        ),
    }
    let target = current_target();
    let Some(t) = manifest.targets.get(&target) else {
        bail!("the manifest has no artifact for {target}");
    };
    crate::ui::detail(&format!(
        "updating docli-cli {current} {} {} ({target})...",
        crate::ui::arrow(),
        manifest.version
    ));
    let bin = get(&t.file)?;
    let got = hex::encode(Sha256::digest(&bin));
    if got != t.sha256.to_lowercase() {
        bail!(
            "downloaded artifact digest mismatch (manifest {}, got {got}) - refusing to \
             install",
            t.sha256
        );
    }
    swap_binary(&bin)?;
    crate::ui::ok(&format!("updated to {}", manifest.version));
    Ok(0)
}

/// Atomic-ish self-replace: write next to the running binary, then rename over it (on Windows a
/// running exe cannot be renamed OVER, but CAN be renamed aside — the standard two-step).
fn swap_binary(bytes: &[u8]) -> Result<()> {
    let exe = std::env::current_exe().context("locating the running binary")?;
    let staging = exe.with_extension("new");
    std::fs::write(&staging, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&staging, &exe).context("swapping the binary")?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).context("renaming the running binary aside")?;
        if let Err(e) = std::fs::rename(&staging, &exe) {
            // Roll the runnable binary back (Codex round 19): an AV lock or I/O error here
            // would otherwise leave NO docli.exe at all.
            let restored = std::fs::rename(&old, &exe).is_ok();
            return Err(anyhow::Error::new(e).context(if restored {
                "installing the new binary (the previous binary was restored)"
            } else {
                "installing the new binary - AND restoring the previous one failed; \
                 re-run the installer to recover"
            }));
        }
        // The .old file is cleaned up on the NEXT run (Windows keeps it mapped until exit).
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The update NOTICE (v0.28.6 D11) — a new version made visible, to the human and to the agent
// ─────────────────────────────────────────────────────────────────────────────────────────────
//
// Today a new version is invisible unless someone runs `self-update` on a hunch: 0.1.0 → 0.1.1 →
// 0.1.2 all shipped inside three days and nobody on 0.1.0 had any reason to know. The check
// compares the local version against the SIGNED manifest we already publish and already verify —
// no new endpoint, no new trust root, and RU-resident S3, so it is reachable without a VPN.
//
// Three rules, and each of them is what keeps a version check from becoming a nuisance:
//
// * **Cached, not per-command.** The network is touched at most once per 24 h. A CLI that hits
//   the network on every invocation is a CLI people stop putting in scripts.
// * **Never blocks and never fails a command.** Short timeout; any error, offline included, is
//   silence. A version check must not be able to break `docli sync`.
// * **Only ever announces strictly newer.** A manifest older than the local build — a rollback,
//   a developer build — says nothing.
//
// And the message NAMES THE COMMAND. Not «a new version is available» but
// «docli-cli 0.1.2 -> 0.1.3 - update: docli self-update». An agent that reads a state and a verb can
// act; an agent that reads a complaint cannot. We announce; we never replace the binary on our
// own — an update that happens by itself is a supply-chain event the user did not schedule, and
// «the agent decided to» is not consent from the person whose machine it is.

/// One network call per this long.
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// Short enough that a blackholed network cannot hold a command hostage; the freshness hook
/// never pays it at all (it reads the cache only).
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Default, serde::Serialize, Deserialize)]
struct UpdateCache {
    /// When the network was last consulted — stamped on failure too, so an offline machine
    /// retries once a day rather than on every invocation.
    #[serde(default)]
    checked_at: i64,
    /// The newest version the manifest offered, whatever it was.
    #[serde(default)]
    latest: Option<String>,
}

fn cache_path() -> Option<std::path::PathBuf> {
    crate::uninstall::home_dir().map(|h| h.join("update-check.json"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A poisoned or absent cache file is an empty cache, never an error: this whole subsystem is
/// allowed to know nothing.
fn read_cache() -> UpdateCache {
    cache_path().map(|p| read_cache_at(&p)).unwrap_or_default()
}

/// The pure half. Separated so its pin does not depend on `DOCLI_HOME`, which is a
/// PROCESS-GLOBAL env var that several other tests in this crate set and clear — a test that
/// reached through it passed alone and raced under `cargo test`.
fn read_cache_at(path: &std::path::Path) -> UpdateCache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_cache(c: &UpdateCache) {
    let Some(p) = cache_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string(c) {
        let _ = std::fs::write(p, body);
    }
}

/// Is the network due? The 24 h bound as a DECISION, so it can be pinned without a network.
///
/// A clock that moved backwards (a laptop waking, an NTP correction) must not park the check
/// forever on a `checked_at` in the FUTURE — hence the negative arm. It is `< 0`, not `<= 0`:
/// an age of exactly zero is a cache written THIS SECOND, and two invocations inside one
/// wall-clock second must not each hit the network.
fn due(checked_at: i64, now: i64) -> bool {
    let age = now - checked_at;
    !(0..CHECK_INTERVAL_SECS).contains(&age)
}

/// The notice for a known-newer version, or `None`. The comparison is [`parse_semver`]'s, so a
/// manifest version that is not plain `x.y.z` announces nothing — the same fail-closed posture
/// the update path itself takes.
pub fn notice_for(latest: &str, current: &str) -> Option<String> {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) if l > c => Some(format!(
            "docli-cli {current} -> {latest} - update: docli self-update"
        )),
        _ => None,
    }
}

/// The notice from the CACHE only — no network, no timeout, no failure mode.
///
/// This is what the `SessionStart` hook uses: the freshness probe owns that hook's 2 s budget
/// (D3), so a cold cache is skipped there and left for the next hand-run `docli` invocation to
/// warm. The limitation that follows is real and named rather than papered over: an agent-only
/// user who never runs `docli` by hand learns about a new version on the first session AFTER any
/// hand invocation.
pub fn cached_notice() -> Option<String> {
    let cache = read_cache();
    notice_for(cache.latest.as_deref()?, env!("CARGO_PKG_VERSION"))
}

/// The notice, refreshing the cache over the network at most once per 24 h.
///
/// Every failure — offline, a bad signature, a malformed manifest, an unwritable cache — is
/// silence. Nothing here can make a command fail.
pub fn notice() -> Option<String> {
    let mut cache = read_cache();
    if due(cache.checked_at, now_unix()) {
        // Stamped BEFORE the attempt, so a machine that is offline for a week makes one call a
        // day rather than one per invocation.
        cache.checked_at = now_unix();
        if let Some(v) = fetch_latest_version() {
            cache.latest = Some(v);
        }
        write_cache(&cache);
    }
    notice_for(cache.latest.as_deref()?, env!("CARGO_PKG_VERSION"))
}

/// Fetch + VERIFY the manifest, and return the version it advertises. The signature check is the
/// same one `run` performs and is not optional here either: an unsigned answer would be a
/// bucket-controlled string we then print as advice.
fn fetch_latest_version() -> Option<String> {
    let base = artifacts_base();
    let http = reqwest::blocking::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .ok()?;
    let fetch = |path: &str| -> Option<Vec<u8>> {
        let resp = http.get(format!("{base}/{path}")).send().ok()?;
        if !resp.status().is_success() {
            return None;
        }
        Some(resp.bytes().ok()?.to_vec())
    };
    let manifest_bytes = fetch("manifest.json")?;
    let sig = String::from_utf8(fetch("manifest.json.minisig")?).ok()?;
    let manifest = verify_manifest(&manifest_bytes, &sig, RELEASE_PUBKEY_B64).ok()?;
    Some(manifest.version)
}

/// Windows leftover cleanup, called at startup (harmless elsewhere).
pub fn cleanup_stale_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpinned_key_refuses_rather_than_verifying_nothing() {
        let err = verify_manifest(b"{}", "sig", "").unwrap_err().to_string();
        assert!(err.contains("no pinned release key"), "{err}");
        let err = verify_manifest(b"{}", "sig", "  \n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no pinned release key"), "{err}");
    }

    #[test]
    fn semver_parse_is_strict_three_component() {
        // The downgrade refusal FAILS CLOSED on anything else (Codex round 2): a signed
        // `0.2.0-beta.1` replay must not bypass the comparison.
        assert_eq!(parse_semver("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_semver("1.10.3"), Some((1, 10, 3)));
        for bad in ["0.2.0-beta.1", "0.2", "0.2.0.1", "v0.2.0", " 9.0.0 ", ""] {
            assert_eq!(parse_semver(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_notice_announces_strictly_newer_and_names_the_command() {
        // «docli 0.1.2 -> 0.1.3 - update: docli self-update». An agent that reads a state and a
        // VERB can act; an agent that reads a complaint cannot.
        let n = notice_for("0.1.3", "0.1.2").expect("newer announces");
        assert!(n.contains("0.1.2") && n.contains("0.1.3"), "{n}");
        assert!(n.contains("docli self-update"), "{n}");
        // ONE naming rule across every message the CLI prints: it names ITSELF `docli-cli`, and
        // a command you type stays the bare `docli`. Both halves are in this single line, which
        // is why it is the one pinned — «docli-cli 0.1.2 -> 0.1.3 - update: docli self-update».
        assert!(
            n.starts_with("docli-cli "),
            "the product names itself hyphenated: {n}"
        );
        assert!(
            n.contains(" update: docli self-update"),
            "…and the command stays bare: {n}"
        );
        // Equal and OLDER stay silent: a rollback or a developer build is not news.
        assert_eq!(notice_for("0.1.2", "0.1.2"), None);
        assert_eq!(notice_for("0.1.1", "0.1.2"), None);
        assert_eq!(notice_for("0.9.9", "1.0.0"), None);
        // …and anything that is not plain x.y.z announces nothing, the same fail-closed
        // posture the update path itself takes.
        for bad in ["0.2.0-beta.1", "", "v9.9.9", "9.9"] {
            assert_eq!(notice_for(bad, "0.1.2"), None, "{bad}");
        }
    }

    #[test]
    fn the_network_is_touched_at_most_once_a_day() {
        // Pinned against the CACHE, never the network — a CLI that hits the network on every
        // invocation is a CLI people stop putting in scripts.
        let day = CHECK_INTERVAL_SECS;
        assert!(due(0, day), "a never-checked cache is due");
        assert!(
            !due(1_000_000, 1_000_000),
            "a cache written THIS second is not due - two invocations inside one second must \
             not each hit the network"
        );
        assert!(!due(1_000_000, 1_000_000 + 60), "a minute later: not due");
        assert!(
            !due(1_000_000, 1_000_000 + day - 1),
            "one second short of a day: still not due"
        );
        assert!(due(1_000_000, 1_000_000 + day), "a day later: due");
        // A stamp in the FUTURE (a clock correction, a copied home directory) must not park
        // the check forever.
        assert!(due(2_000_000, 1_000_000));
    }

    #[test]
    fn a_poisoned_cache_is_ignored_rather_than_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("update-check.json");
        // Absent, and then unreadable: both read as an EMPTY cache — no panic, no error, no
        // notice. This whole subsystem is allowed to know nothing.
        assert_eq!(read_cache_at(&p).latest, None);
        std::fs::write(&p, "not json at all{{").unwrap();
        assert_eq!(read_cache_at(&p).latest, None);
        assert_eq!(read_cache_at(&p).checked_at, 0);
        // A well-formed one round-trips, and the 24 h bound is measured against the FILE, not
        // the network: a fresh `checked_at` is what keeps `notice()` from building a client.
        std::fs::write(&p, r#"{"checked_at": 9999999999, "latest": "99.0.0"}"#).unwrap();
        let c = read_cache_at(&p);
        assert_eq!(c.latest.as_deref(), Some("99.0.0"));
        assert!(
            now_unix() - c.checked_at < CHECK_INTERVAL_SECS,
            "still fresh"
        );
        assert!(notice_for(c.latest.as_deref().unwrap(), "0.1.2").is_some());
    }

    #[test]
    fn a_tampered_manifest_is_refused_by_the_signature() {
        // A structurally valid minisign key that signed NOTHING we present: decode fails or
        // verification fails — either way the manifest is refused before parsing.
        let some_key = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"; // minisign's doc example
        let err = verify_manifest(
            b"{\"version\":\"9.9.9\",\"targets\":{}}",
            "not-a-signature",
            some_key,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("malformed") || err.contains("does not verify"),
            "{err}"
        );
    }
}
