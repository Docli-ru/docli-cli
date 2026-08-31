// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! Filesystem rules, INJECTED rather than read off `cfg!` at use sites, so the whole apply
//! engine — winPath projection, fold-collision parking, length parking — is exercisable on any
//! development platform (the plugin's `foldPath` closure is the same shape: a platform rule
//! passed in, not consulted inline).

/// The rules the local filesystem imposes on mirror paths.
#[derive(Debug, Clone, Copy)]
pub struct FsRules {
    /// Fold case when comparing paths for the twin guard (macOS / Windows default filesystems).
    pub fold_case_insensitive: bool,
    /// Apply the winPath projection (`docli_rules::winpath`) — Windows only.
    pub win_names: bool,
    /// The per-component byte cap the projection must not exceed (255 on every mainstream fs).
    pub max_component_bytes: usize,
}

impl FsRules {
    pub fn native() -> Self {
        FsRules {
            fold_case_insensitive: docli_rules::platform_folds_case(),
            win_names: cfg!(target_os = "windows"),
            max_component_bytes: 255,
        }
    }
}
