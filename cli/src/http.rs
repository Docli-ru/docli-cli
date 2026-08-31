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
        Ok(Api {
            server: server.trim_end_matches('/').to_string(),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
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

    /// Workspace enumeration for `docli init` — `viewer.workspaces`, deliberately exempt from
    /// `deny_scoped_pat_via_graphql` and filtered to the PAT's pin set.
    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let mut token = self.bearer()?;
        // Same 401 discipline as the sync surface (Codex round 3): a locally fresh but
        // server-rejected token must rotate once, and any other failure must SAY so — a
        // swallowed error here reads as «you have no workspaces», which is worse than an error.
        let mut rotated = false;
        let resp = loop {
            let resp = self
                .http
                .post(format!("{}/api/graphql", self.server))
                .header("authorization", format!("Bearer {token}"))
                .header("x-docli-cli-version", env!("CARGO_PKG_VERSION"))
                .json(&serde_json::json!({
                    "query": "{ viewer { workspaces { id handle name } } }"
                }))
                .send()
                .context("listing workspaces")?;
            let status = resp.status();
            if status.as_u16() == 401 && !rotated {
                rotated = true;
                token = self.creds.refresh_single_flight(
                    &self.server,
                    &self.refresh_fn(),
                    Some(&token),
                )?;
                continue;
            }
            if !status.is_success() {
                bail!("listing workspaces failed ({status})");
            }
            break resp;
        };
        let v: serde_json::Value = resp.json().context("parsing the workspace list")?;
        if let Some(errs) = v.get("errors").and_then(|e| e.as_array()) {
            if !errs.is_empty() {
                bail!(
                    "listing workspaces failed: {}",
                    errs[0]["message"].as_str().unwrap_or("GraphQL error")
                );
            }
        }
        let list = v["data"]["viewer"]["workspaces"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|w| {
                Some(WorkspaceInfo {
                    id: w["id"].as_str()?.parse().ok()?,
                    handle: w["handle"].as_str()?.to_string(),
                    name: w["name"].as_str()?.to_string(),
                })
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: Uuid,
    pub handle: String,
    pub name: String,
}
