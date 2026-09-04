// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli logout` (0.1.1) — hand the credential back and forget it locally.
//!
//! The order is deliberate and so is the failure posture: the refresh token is offered to the
//! server's RFC 7009 revocation endpoint FIRST, and the local file is cleared **whatever the
//! answer was**. A logout that leaves a live token on disk because the network was down is not
//! a logout; a logout that leaves a live grant on the server because the file was already gone
//! is a leak the user cannot see. Doing both, in that order, is the only combination where a
//! partial failure still ends with the weaker state on this machine.
//!
//! Revocation is TOKEN-scoped, not grant-scoped (the v0.25.x asymmetry): this retires the
//! device's tokens, and the connection keeps its row in «Доступ» until the owner disconnects
//! it there. The message says so rather than implying the grant is gone.
//!
//! The install id deliberately SURVIVES (`creds::install_id` persists it outside the entry):
//! logging back in from this machine re-uses the one device row instead of minting a second
//! one toward the grant cap.

use anyhow::Result;

use crate::creds::CredsStore;
use crate::http::CLI_CLIENT_ID;
use crate::ui;

/// Best-effort RFC 7009 revocation. Returns whether the server confirmed it.
///
/// Never fails the logout: a 4xx from an old server, a captive portal, or no network at all
/// must still let the credential be dropped locally.
pub fn revoke(server: &str, refresh_token: &str) -> bool {
    let http = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    http.post(format!("{server}/api/oauth/revoke"))
        .form(&[
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", CLI_CLIENT_ID),
        ])
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Say so when `DOCLI_TOKEN` still signs this device in.
///
/// Removing the stored credential does not touch the environment, so without this line a
/// successful-looking logout would be followed by a `docli search` that still works — and the
/// reader would have no way to find out why.
pub fn warn_env_token_still_in_force() {
    if let Ok(Some(env)) = crate::creds::env_token() {
        ui::warn(&format!(
            "{} is still set in this environment and still signs you in to {}. Unset it to \
             finish signing out; revoke the token itself in the access list on {}.",
            crate::creds::TOKEN_VAR,
            env.server(),
            env.server()
        ));
    }
}

/// Log out of EVERY server this machine holds a credential for (`--all`, and what
/// `docli uninstall` calls on its way out).
pub fn all() -> Result<i32> {
    run_inner("", true, true)
}

/// The same, without the «sign in again» closing hint — for `docli uninstall`, where suggesting
/// the next sign-in contradicts the operation that is about to delete the binary.
pub fn all_quiet() -> Result<i32> {
    run_inner("", true, false)
}

pub fn run(server: &str, all: bool) -> Result<i32> {
    run_inner(server, all, true)
}

fn run_inner(server: &str, all: bool, suggest_next: bool) -> Result<i32> {
    // The STORED credential, deliberately: `DOCLI_TOKEN` is not ours to revoke or remove, and
    // refusing to clear the file because one is set would leave a device signed in with no way
    // to sign it out. What we owe the reader is the sentence at the end saying it is still in
    // force — a `logout` that reports success while the next command still authenticates is
    // exactly the kind of quiet lie this CLI does not tell.
    let store = CredsStore::open_stored()?;
    let targets: Vec<String> = if all {
        store.servers()?
    } else {
        vec![server.to_string()]
    };
    if targets.is_empty() {
        ui::ok("No active connections.");
        warn_env_token_still_in_force();
        return Ok(0);
    }

    let mut any = false;
    // Tracked, not just warned about: the local copy is gone either way, so a token the server
    // never confirmed revoking is the one outcome a script (and `docli uninstall`) must be able
    // to notice.
    let mut unconfirmed = 0usize;
    let mut signed_out: Vec<String> = Vec::new();
    // «Connection» is grant language. A minted key is a KEY — it shows as one in the access
    // list, and the closing line below would be pointing at something the reader does not have.
    let mut any_grant = false;
    for target in targets {
        let Some(creds) = store.get(&target)? else {
            if !all {
                ui::ok(&format!("This device was not signed in to {target}."));
            }
            continue;
        };
        any = true;
        signed_out.push(target.clone());
        // A minted key has no refresh lineage to retire, and offering the ACCESS token to the
        // revocation endpoint would be guessing at a server behaviour we have not verified. It
        // is also not ours to retire: the user minted it and it is listed under their own name
        // in the access list. So the local copy goes and the closing line says the key is still
        // live — the one thing a reader must not have to work out for themselves.
        let minted_key = creds.refresh_token.is_none();
        any_grant |= !minted_key;
        let revoked = match &creds.refresh_token {
            Some(rt) => revoke(&target, rt),
            None => false,
        };
        let identity = creds
            .refresh_token
            .clone()
            .unwrap_or_else(|| creds.access_token.clone());
        // Local state goes regardless — see the module note on ordering — but only while it is
        // still the credential we just revoked: a `docli login` finishing during the network
        // round-trip owns the entry now, and deleting it would leave a live token with no local
        // copy able to revoke it.
        if !store.remove_if_current(&target, &identity)? {
            // Say what actually happened to the OLD token: claiming a revocation the server
            // never confirmed would leave a live credential the reader thinks is gone.
            let fate = if revoked {
                "the previous token was revoked"
            } else {
                unconfirmed += 1;
                "the previous token could NOT be revoked and may still be live"
            };
            ui::warn(&format!(
                "{target}: a new sign-in appeared while this one was running - {fate}, and the \
                 new one was kept. Run the command again to drop that one too."
            ));
            continue;
        }
        if minted_key {
            // NOT counted as unconfirmed: nothing failed. The credential we were holding is
            // gone from this machine, which is all a logout of a pasted key can mean.
            ui::ok(&format!(
                "Signed out of {target} - the stored key was removed from this device."
            ));
            ui::warn(&format!(
                "That key is still live on the server. Revoke it in the access list on {target} \
                 if you no longer want it to work."
            ));
        } else if revoked {
            ui::ok(&format!("Signed out of {target} - tokens revoked."));
        } else {
            unconfirmed += 1;
            ui::ok(&format!("Signed out of {target} - credentials removed."));
            ui::warn(&format!(
                "The server did not confirm the revocation. If this device is lost, \
                 disconnect it in the access list on {target}.",
            ));
        }
    }
    if !any {
        return Ok(0);
    }
    if any_grant {
        ui::detail("The connection stays listed in the access list until you disconnect it there.");
    }
    warn_env_token_still_in_force();
    if suggest_next {
        // Name the ORIGIN when it is not production: bare `docli login` outside a project signs
        // into docli.ru, which is not where this device was just signed out of.
        // Name the ORIGIN whenever it is not production: bare `docli login` outside a project
        // signs into docli.ru, which is not where this device was just signed out of. Under
        // `--all` that is knowable only when exactly one origin was involved.
        let origin = if all {
            match signed_out.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            }
        } else {
            Some(server.to_string())
        };
        let cmd = match origin {
            Some(o) if o != "https://docli.ru" => format!("docli login --server {o}"),
            _ => "docli login".to_string(),
        };
        ui::next(&format!("Sign in again: {}", ui::cmd(&cmd)));
    }
    Ok(if unconfirmed > 0 { 1 } else { 0 })
}
