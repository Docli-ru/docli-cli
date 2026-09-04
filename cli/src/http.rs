// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The API transport (v0.28.0): blocking reqwest over rustls, bearer from [`crate::creds`],
//! `X-Docli-Sync-Version` / `X-Docli-Cli-Version` / `X-Docli-Client-Platform` on every request
//! (D9 — version is logged server-side, stored nowhere), 401 → one single-flight refresh →
//! retry.

use anyhow::{bail, Context, Result};
use docli_sync_wire::{PullRequest, PullResponse, SearchRequest, SearchResponse};
use serde::Deserialize;
use uuid::Uuid;

use crate::creds::{CredsStore, RefreshOutcome};

pub const SYNC_PROTOCOL_VERSION: &str = "1";
pub const CLI_CLIENT_ID: &str = "docli-cli";

pub struct Api {
    pub server: String,
    http: reqwest::blocking::Client,
    creds: CredsStore,
}

/// A typed non-2xx from the sync surface.
#[derive(Debug)]
pub enum ApiFailure {
    /// 409 EPOCH_CHANGED — the workspace was resynced; pull from START at the new epoch.
    EpochChanged { epoch: i64 },
    /// Any other refusal (code + message as the server said them).
    Refused {
        status: u16,
        code: String,
        message: String,
    },
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiFailure::EpochChanged { epoch } => write!(f, "workspace resynced (epoch {epoch})"),
            ApiFailure::Refused {
                status,
                code,
                message,
            } => write!(f, "{code} ({status}): {message}"),
        }
    }
}

#[derive(Deserialize)]
struct ErrBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    epoch: Option<i64>,
}

impl Api {
    pub fn new(server: &str, creds: CredsStore) -> Result<Self> {
        Self::with_timeout(server, creds, std::time::Duration::from_secs(120))
    }

    /// A client with a SHORT timeout, for probes whose answer is a nicety rather than the
    /// command's purpose. `docli status` is offline-by-default, and a captive portal that
    /// blackholes packets would otherwise make it sit for the full two minutes before printing
    /// state it already had on disk.
    pub fn with_timeout(
        server: &str,
        creds: CredsStore,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        Ok(Api {
            server: server.trim_end_matches('/').to_string(),
            http: reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .context("building the HTTP client")?,
            creds,
        })
    }

    pub fn creds(&self) -> &CredsStore {
        &self.creds
    }

    fn refresh_fn(&self) -> impl Fn(&str) -> Result<RefreshOutcome> + '_ {
        move |refresh_token: &str| {
            let resp = self
                .http
                .post(format!("{}/api/oauth/token", self.server))
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", CLI_CLIENT_ID),
                ])
                .send()
                .context("refreshing the access token")?;
            let status = resp.status();
            if status.is_success() {
                #[derive(Deserialize)]
                struct TokenResp {
                    access_token: String,
                    refresh_token: String,
                    expires_in: i64,
                }
                let t: TokenResp = resp.json().context("parsing the token response")?;
                return Ok(RefreshOutcome::Rotated {
                    access_token: t.access_token,
                    refresh_token: t.refresh_token,
                    expires_in: t.expires_in,
                });
            }
            if status.as_u16() == 503 {
                let retry_after_secs = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30);
                return Ok(RefreshOutcome::Suspended { retry_after_secs });
            }
            #[derive(Deserialize)]
            struct OauthErr {
                #[serde(default)]
                error: String,
            }
            let e: OauthErr = resp.json().unwrap_or(OauthErr {
                error: String::new(),
            });
            if e.error == "invalid_grant" {
                return Ok(RefreshOutcome::InvalidGrant);
            }
            bail!("token refresh failed ({status}): {}", e.error)
        }
    }

    fn bearer(&self) -> Result<String> {
        self.creds.bearer(&self.server, &self.refresh_fn())
    }

    /// POST a JSON body to a sync-plane path; one refresh-and-retry on 401.
    fn post_sync<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<std::result::Result<T, ApiFailure>> {
        let mut token = self.bearer()?;
        for attempt in 0..2 {
            let resp = self
                .http
                .post(format!("{}{path}", self.server))
                .header("authorization", format!("Bearer {token}"))
                .header("x-docli-sync-version", SYNC_PROTOCOL_VERSION)
                .header("x-docli-cli-version", env!("CARGO_PKG_VERSION"))
                .header("x-docli-client-platform", std::env::consts::OS)
                .json(body)
                .send()
                .with_context(|| format!("POST {path}"))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(Ok(resp
                    .json()
                    .with_context(|| format!("parsing {path}"))?));
            }
            if status.as_u16() == 401 && attempt == 0 {
                token = self.creds.refresh_single_flight(
                    &self.server,
                    &self.refresh_fn(),
                    Some(&token),
                )?;
                continue;
            }
            let e: ErrBody = resp.json().unwrap_or(ErrBody {
                code: String::new(),
                message: String::new(),
                epoch: None,
            });
            if e.code == "EPOCH_CHANGED" {
                return Ok(Err(ApiFailure::EpochChanged {
                    epoch: e.epoch.unwrap_or(0),
                }));
            }
            return Ok(Err(ApiFailure::Refused {
                status: status.as_u16(),
                code: e.code,
                message: e.message,
            }));
        }
        unreachable!("the loop returns on both branches");
    }

    pub fn pull(&self, req: &PullRequest) -> Result<std::result::Result<PullResponse, ApiFailure>> {
        self.post_sync("/api/sync/pull", req)
    }

    pub fn bootstrap(
        &self,
        req: &PullRequest,
    ) -> Result<std::result::Result<PullResponse, ApiFailure>> {
        self.post_sync("/api/sync/bootstrap", req)
    }

    pub fn search(
        &self,
        req: &SearchRequest,
    ) -> Result<std::result::Result<SearchResponse, ApiFailure>> {
        self.post_sync("/api/sync/search", req)
    }

    /// Who this device is signed in as, for `docli status` — the display name when the account
    /// has one, else the email. `viewer` self-introspection is open to a sync-scoped PAT by
    /// design (`deny_scoped_pat_via_graphql` deliberately does not gate it), so this needs no
    /// scope the login round did not already take.
    ///
    /// Errors are the CALLER's to swallow: status renders offline, and a failed identity probe
    /// must degrade one line rather than fail the command.
    pub fn viewer_label(&self) -> Result<String> {
        let v: serde_json::Value =
            self.graphql_no_refresh("{ viewer { email displayName } }", "reading the account")?;
        let viewer = self.viewer_of(&v)?;
        let viewer = &viewer;
        let name = viewer["displayName"].as_str().unwrap_or("").trim();
        let email = viewer["email"].as_str().unwrap_or("").trim();
        let (name, email) = (crate::ui::sanitize(name), crate::ui::sanitize(email));
        let (name, email) = (name.trim(), email.trim());
        match (name.is_empty(), email.is_empty()) {
            (false, false) => Ok(format!("{name} <{email}>")),
            (true, false) => Ok(email.to_string()),
            (false, true) => Ok(name.to_string()),
            (true, true) => bail!("the server returned no account identity"),
        }
    }

    /// Workspace enumeration for `docli init` — `viewer.workspaces`, deliberately exempt from
    /// `deny_scoped_pat_via_graphql` and filtered to the PAT's pin set.
    /// One GraphQL round-trip with the sync surface's 401 discipline (Codex round 3): a
    /// locally fresh but server-rejected token rotates ONCE, and any other failure SAYS so —
    /// a swallowed error here reads as «you have no workspaces», which is worse than an error.
    ///
    /// Both GraphQL callers share this: a second hand-rolled copy of the rotation loop is the
    /// «two readers of the same question» shape that has bitten this codebase before.
    fn graphql(&self, query: &str, what: &str) -> Result<serde_json::Value> {
        self.graphql_inner(query, what, true)
    }

    /// The same round-trip with the 401 ROTATION disabled — for callers that must be bounded in
    /// time. A refresh can meet `503 Retry-After`, and the shared path then sleeps up to three
    /// times (two minutes each): fine for a command that needs the credential, wrong for
    /// `docli status`, which is a reader with a five-second budget.
    fn graphql_no_refresh(&self, query: &str, what: &str) -> Result<serde_json::Value> {
        self.graphql_inner(query, what, false)
    }

    fn graphql_inner(
        &self,
        query: &str,
        what: &str,
        allow_refresh: bool,
    ) -> Result<serde_json::Value> {
        // `bearer()` refreshes on its own when the stored token is near expiry, which re-opens
        // the very sleep loop `allow_refresh: false` exists to avoid: the pre-check upstream can
        // see 61 seconds of life and `bearer` see 59. A no-refresh caller reads the stored token
        // as it is and fails honestly if it will not do.
        let mut token = if allow_refresh {
            self.bearer()?
        } else {
            self.creds
                .stored_token(&self.server)?
                .context("no usable token - run `docli login`")?
        };
        let mut rotated = false;
        let resp = loop {
            let resp = self
                .http
                .post(format!("{}/api/graphql", self.server))
                .header("authorization", format!("Bearer {token}"))
                .header("x-docli-cli-version", env!("CARGO_PKG_VERSION"))
                .json(&serde_json::json!({ "query": query }))
                .send()
                .with_context(|| what.to_string())?;
            let status = resp.status();
            if status.as_u16() == 401 && !rotated && allow_refresh {
                rotated = true;
                token = self.creds.refresh_single_flight(
                    &self.server,
                    &self.refresh_fn(),
                    Some(&token),
                )?;
                continue;
            }
            if !status.is_success() {
                bail!("{what} failed ({status})");
            }
            break resp;
        };
        let v: serde_json::Value = resp.json().with_context(|| format!("parsing: {what}"))?;
        if let Some(errs) = v.get("errors").and_then(|e| e.as_array()) {
            if !errs.is_empty() {
                bail!(
                    "{what} failed: {}",
                    errs[0]["message"].as_str().unwrap_or("GraphQL error")
                );
            }
        }
        Ok(v)
    }

    /// `data.viewer`, or a refusal saying the credential was not accepted.
    ///
    /// **The server answers an unauthenticated GraphQL request with 200 and `data.viewer: null`**,
    /// not a 401 — measured against prod, not inferred. Read naively that is indistinguishable
    /// from a signed-in account holding nothing, and `workspaces()` said exactly that: «This
    /// account has no workspaces you can reach» for a token the server had refused. An emptiness
    /// claim made on a refusal is the worst shape an answer can take, because the reader acts
    /// on it.
    fn viewer_of<'a>(&self, v: &'a serde_json::Value) -> Result<&'a serde_json::Value> {
        let viewer = &v["data"]["viewer"];
        if viewer.is_null() {
            return Err(anyhow::Error::new(CredentialRefused {
                server: self.server.clone(),
            }));
        }
        Ok(viewer)
    }

    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let v = self.graphql(
            "{ viewer { workspaces { id handle name } } }",
            "listing workspaces",
        )?;
        let list = self.viewer_of(&v)?["workspaces"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|w| {
                // Sanitized AT INGESTION: a workspace name is set by its owner and rendered on
                // every member's terminal, including inside interactive pickers. Escaping at
                // each render site is a rule someone forgets; this is a place they cannot.
                Some(WorkspaceInfo {
                    id: w["id"].as_str()?.parse().ok()?,
                    handle: crate::ui::sanitize(w["handle"].as_str()?),
                    name: crate::ui::sanitize(w["name"].as_str()?),
                })
            })
            .collect())
    }
}

/// The server ANSWERED and said this credential is not one — as distinct from a request that
/// never arrived.
///
/// `status` needs the difference. It is offline-first, so a probe that fails because there is no
/// network must degrade one line; a probe that fails because the server refused the credential
/// is the whole answer, and for a minted key it is the ONLY way to learn it — a key carries no
/// expiry, so «signed in» would otherwise stand forever while every other command failed.
#[derive(Debug)]
pub struct CredentialRefused {
    pub server: String,
}

impl std::fmt::Display for CredentialRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} did not accept this sign-in - it may have expired or been revoked",
            self.server
        )
    }
}
impl std::error::Error for CredentialRefused {}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: Uuid,
    pub handle: String,
    pub name: String,
}
