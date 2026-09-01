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
    let store = CredsStore::open_default()?;
    let targets: Vec<String> = if all {
        store.servers()?
    } else {
        vec![server.to_string()]
    };
    if targets.is_empty() {
        ui::ok("No active connections.");
        return Ok(0);
    }

    let mut any = false;
    // Tracked, not just warned about: the local copy is gone either way, so a token the server
    // never confirmed revoking is the one outcome a script (and `docli uninstall`) must be able
    // to notice.
    let mut unconfirmed = 0usize;
    let mut signed_out: Vec<String> = Vec::new();
    for target in targets {
        let Some(creds) = store.get(&target)? else {
            if !all {
                ui::ok(&format!("This device was not signed in to {target}."));
            }
            continue;
        };
        any = true;
        signed_out.push(target.clone());
        let revoked = revoke(&target, &creds.refresh_token);
        // Local state goes regardless — see the module note on ordering — but only while it is
        // still the credential we just revoked: a `docli login` finishing during the network
        // round-trip owns the entry now, and deleting it would leave a live token with no local
        // copy able to revoke it.
        if !store.remove_if_current(&target, &creds.refresh_token)? {
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
        if revoked {
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
    ui::detail("The connection stays listed in the access list until you disconnect it there.");
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
