// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! docli — a read-only agent cache over the docli sync plane (v0.29.1 / docli-cli 0.1.5).
//!
//! A library target exists so the integration tests can drive the sync orchestrator against a
//! scripted stub server; the shipped artifact is the `docli` bin.

pub mod agents;
pub mod apply;
pub mod config;
pub mod creds;
pub mod doctor;
pub mod graph;
pub mod guard;
pub mod hooks;
pub mod http;
pub mod init_cmd;
pub mod instructions;
pub mod list_cmd;
pub mod localpath;
pub mod login;
pub mod logout;
pub mod markers;
pub mod mountfs;
pub mod platform;
pub mod read_cmd;
pub mod search_cmd;
pub mod selfupdate;
pub mod state;
pub mod status;
pub mod sync_cmd;
pub mod ui;
pub mod uninstall;
pub mod wizard;
