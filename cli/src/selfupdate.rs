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
        crate::ui::ok(&format!("docli {current} is already the latest"));
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
        "updating docli {current} {} {} ({target})...",
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
