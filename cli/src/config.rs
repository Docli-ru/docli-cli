// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! `docli.toml` — the MOUNT TABLE (v0.28.0 D2). Committed and visible (an agent doing `ls` must
//! find it); it NAMES workspaces, never grants (designation without authority — login state
//! decides reach). Mounts key on workspace **id**, never handle (rename-at-will).
//!
//! The GEOMETRY rules live here and are re-checked by `sync` like the rest (not only offered by
//! `init` — hand-editing the committed `docli.toml` is the first-class way to add a mount and
//! never passes through `init`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CONFIG_NAME: &str = "docli.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocliToml {
    /// The docli origin. Defaults to production; a dev stack overrides it.
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default, rename = "mount")]
    pub mounts: Vec<Mount>,
    /// The MCP connection label `docli init --mcp` wired for this project (D12.4). Persisted so
    /// re-runs and renames of the project DIRECTORY keep pointing at the SAME connection —
    /// deriving from the dir name alone would silently fork the grant/persona/pin on a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_label: Option<String>,
}

fn default_server() -> String {
    "https://docli.ru".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    /// Workspace ID — never the handle (handles rename at will; ids don't).
    pub workspace: Uuid,
    /// Mount dir, relative to `docli.toml`'s directory (or absolute).
    pub dir: String,
    /// Optional folder scope: mirror only this server subtree, mapped to the mount root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The AUTHOR's name for this mount — what refusals report (never server-fetched titles:
    /// no existence oracle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Mount {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.dir)
    }
}

pub struct Project {
    pub root: PathBuf,
    pub config: DocliToml,
}

/// Find `docli.toml` in `start` or an ancestor (the git discovery shape).
pub fn find_project(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(CONFIG_NAME).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

pub fn load_project(start: &Path) -> Result<Project> {
    let Some(root) = find_project(start) else {
        bail!("no {CONFIG_NAME} here or in any parent directory - run `docli init` first");
    };
    let raw = fs::read_to_string(root.join(CONFIG_NAME)).context("reading docli.toml")?;
    // ONE origin normalization and ONE control-character refusal, both at this seam — see
    // `parse_config`, which `init` and the wizard share so neither can skip them.
    let config = parse_config(&raw)?;
    refuse_control_characters(&config)?;
    Ok(Project { root, config })
}

/// Parse `docli.toml` text — the ONE door for this file, so its refusals cannot be bypassed by
/// a caller that happens to read it itself. (`init` and the wizard both do, to inspect an
/// existing project before deciding what to write.)
pub fn parse_config(raw: &str) -> Result<DocliToml> {
    let mut config: DocliToml = toml::from_str(raw).context("parsing docli.toml")?;
    config.server = config.server.trim_end_matches('/').to_string();
    refuse_control_characters(&config)?;
    Ok(config)
}

/// `docli.toml` is committed and teammate-editable, so an escape sequence in it reaches a
/// colleague's terminal through whichever command renders it — and `status` deliberately
/// reports on configurations it has not otherwise validated. Refusing at INGESTION means no
/// renderer downstream can meet one, which is a property no amount of per-call-site escaping
/// can give. The vector is TOML's own `\uXXXX` escape; a raw control byte is already rejected
/// by the parser.
fn refuse_control_characters(config: &DocliToml) -> Result<()> {
    let dirty = [("server", config.server.as_str())]
        .into_iter()
        .chain(config.mounts.iter().flat_map(|m| {
            [
                Some(("dir", m.dir.as_str())),
                m.name.as_deref().map(|n| ("name", n)),
                m.folder.as_deref().map(|f| ("folder", f)),
            ]
            .into_iter()
            .flatten()
        }))
        .find(|(_, v)| v.chars().any(|c| c.is_control()));
    if let Some((field, _)) = dirty {
        bail!(
            "docli.toml: `{field}` contains a control character - remove it (it would be \
             interpreted by whatever terminal renders it)"
        );
    }
    Ok(())
}

/// Absolute mount dir for a mount (lexically normalized — `.`/`..` resolved without touching
/// the filesystem, so geometry holds for not-yet-created dirs too).
pub fn mount_abs(project_root: &Path, m: &Mount) -> PathBuf {
    let p = Path::new(&m.dir);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    lexical_normalize(&joined)
}

/// The PHYSICAL form of a possibly-not-yet-existing path (Codex round 1): canonicalize the
/// nearest EXISTING ancestor and re-append the missing suffix. Lexical paths alone let an
/// ancestor symlink smuggle a mount into a git work tree (or into overlap) the lexical
/// comparisons never see — `/outside/link → /repo` puts `/outside/link/m` physically inside
/// `/repo`, where `find_git_worktree` walking the lexical spelling never meets `/repo/.git`.
/// Falls back to the LEXICAL path when no ancestor resolves (a fully-nonexistent spelling, a
/// canonicalize error) — the same best-effort posture the rest of geometry takes.
pub fn physicalize(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                suffix.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut out = fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    for name in suffix.iter().rev() {
        out.push(name);
    }
    out
}

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The comparison spelling of a geometry path: on a case-folding platform (macOS/Windows)
/// each component is lowercased, because `.DOCLI` and `.docli` are ONE directory entry there
/// and a case-varied spelling of a not-yet-existing path sails past `physicalize` (which can
/// only canonicalize what exists) — letting a mount claim the control plane itself, or two
/// mounts alias one physical dir (Codex round 9). Comparison-only — never used for IO. On
/// case-sensitive platforms the path is untouched (`Notes` and `notes` are honestly distinct
/// mounts there).
///
/// Public since v0.28.6 D2: `guard.rs` answers «is this path inside a mirror?» and a guard that
/// skipped the fold would let `Docli-Mirror/x.md` through on APFS, and an NFD spelling through
/// on the platforms that fold. BOTH rules — `physicalize` for the symlink half, this for the
/// spelling half — or the deny is decorative.
pub fn geometry_key(p: &Path) -> PathBuf {
    if !docli_rules::platform_folds_case() {
        return p.to_path_buf();
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            // The FULL filesystem fold (`docli_rules::fold_path`: NFC + lowercase + sigma),
            // not bare lowercase — APFS also unifies composed/decomposed spellings, so
            // `É` and `E\u{301}` alias physically too (Codex round 10).
            std::path::Component::Normal(n) => {
                out.push(docli_rules::fold_path(&n.to_string_lossy(), true))
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Is `b` the same path as `a`, or inside it? Both spellings go through [`geometry_key`], so a
/// case- or NFD-varied `b` still lands inside `a` wherever the filesystem folds them together.
/// Public since v0.28.6 D2 — the guard asks exactly this question of a mount directory.
pub fn is_ancestor_or_self(a: &Path, b: &Path) -> bool {
    let (a, b) = (geometry_key(a), geometry_key(b));
    b == a || b.starts_with(&a)
}

/// The CONFIG-level half of validation — request-shaping rules only (mounts exist, one per
/// workspace, folder-scope shape). `search` runs THIS and nothing more (Codex round 24): the
/// disk-geometry rules below are mirror-WRITE safety, and blocking a server search on an
/// unignored/nonexistent mount dir violated the works-without-a-cache contract.
pub fn validate_config(config: &DocliToml) -> Result<()> {
    if config.mounts.is_empty() {
        bail!("docli.toml has no [[mount]] entries - add one with `docli init`");
    }
    for m in &config.mounts {
        // A control character in a mount dir cannot be expressed as a `.gitignore` pattern (a
        // NEWLINE becomes two patterns, one of which may hide something real), and no
        // legitimate path needs one.
        if m.dir.chars().any(|c| c.is_control())
            || m.name
                .as_deref()
                .is_some_and(|n| n.chars().any(|c| c.is_control()))
        {
            bail!(
                "mount `{}`: the path or name contains a control character - remove it",
                crate::ui::sanitize(m.display_name())
            );
        }
    }
    // One mount per workspace: `.docli/` state is per-workspace, and two mounts draining one
    // cursor would each hold half the tree.
    let mut seen = std::collections::HashSet::new();
    for m in &config.mounts {
        if !seen.insert(m.workspace) {
            bail!(
                "workspace {} is mounted more than once - each workspace may have only one \
                 mount",
                m.workspace
            );
        }
        if let Some(f) = &m.folder {
            // Per-segment (Codex round 25): a scope no server path can ever match
            // (`docs//x`, `../up`, control chars) makes `scope_relative` match NOTHING —
            // and re-scoping an existing mount to it would from-zero the mirror EMPTY.
            let bad_segment = f.split('/').any(|seg| {
                seg.is_empty()
                    || seg == "."
                    || seg == ".."
                    || seg.contains('\\')
                    || seg.chars().any(|c| c.is_control())
                    // Server names are trimmed and length-capped (`validate_node_name`), so
                    // boundary whitespace / >255-byte segments match nothing (round 26).
                    || seg.trim() != seg
                    || seg.len() > 255
            });
            if f.is_empty() || f.starts_with('/') || f.ends_with('/') || bad_segment {
                bail!(
                    "mount `{}`: the folder scope is a relative server folder path such as \
                     `docs/api` - nonempty segments of at most 255 bytes, with no `.`, `..`, \
                     backslashes, control characters, surrounding whitespace, or leading or \
                     trailing slashes",
                    m.display_name()
                );
            }
        }
    }
    Ok(())
}

/// The D2 geometry rules — a config-level HARD refusal (auth-reach failures are the
/// partial-success class; broken geometry is not). Checked at `init` AND every `sync`/`doctor`.
pub fn validate_geometry(project_root: &Path, config: &DocliToml) -> Result<()> {
    validate_geometry_inner(project_root, config, true)
}

/// The geometry rules WITHOUT the git-ignore requirement — for the interactive wizard, which
/// validates the mount the moment it is chosen and only offers to write `.gitignore` two steps
/// later. Never a substitute for [`validate_geometry`] on the sync path: the ignore rule is a
/// gate there, and this one deliberately does not enforce it.
pub fn validate_geometry_paths_only(project_root: &Path, config: &DocliToml) -> Result<()> {
    validate_geometry_inner(project_root, config, false)
}

fn validate_geometry_inner(
    project_root: &Path,
    config: &DocliToml,
    require_ignored: bool,
) -> Result<()> {
    validate_config(config)?;
    // Geometry runs on PHYSICAL paths: symlinked ancestors must not smuggle a mount past the
    // overlap/control/vault/git rules (Codex round 1).
    let project_root_phys = physicalize(project_root);
    let control = project_root_phys.join(".docli");
    let abs: Vec<(usize, PathBuf)> = config
        .mounts
        .iter()
        .enumerate()
        .map(|(i, m)| (i, physicalize(&mount_abs(project_root, m))))
        .collect();
    for (i, a) in &abs {
        let m = &config.mounts[*i];
        // Overlapping or nested mounts: two apply passes over shared paths would mutually
        // destroy files and turn `doctor` into an unfalsifiable discrepancy loop.
        for (j, b) in &abs {
            if i < j && (is_ancestor_or_self(a, b) || is_ancestor_or_self(b, a)) {
                bail!(
                    "mounts `{}` and `{}` overlap ({} vs {}) - mounts must be disjoint",
                    m.display_name(),
                    config.mounts[*j].display_name(),
                    a.display(),
                    b.display()
                );
            }
        }
        // Canonical disjointness from the CONTROL PLANE, in BOTH directions: a mount of `.`
        // would let a legal server node named `docli.toml` overwrite the configuration (the
        // `.docli` suffix guard does not cover that name), and a mount inside `.docli/` is the
        // inverse containment.
        if is_ancestor_or_self(a, &project_root_phys) {
            bail!(
                "mount `{}` contains the project's docli.toml or .docli/ directory - choose a \
                 mount directory that contains neither",
                m.display_name()
            );
        }
        if is_ancestor_or_self(&control, a) {
            bail!(
                "mount `{}` is inside .docli/ - choose a directory outside .docli/",
                m.display_name()
            );
        }
        // An Obsidian vault ancestor: the plugin scan-diffs untracked files into CREATE
        // mutations, so a mirror dropped into a synced vault pushes a full duplicate of every
        // mounted workspace (the server's blast-radius breaker gates deletes only).
        let mut anc = Some(a.as_path());
        while let Some(d) = anc {
            if d.join(".obsidian").is_dir() {
                bail!(
                    "mount `{}` sits inside an Obsidian vault ({}) - the plugin would push the \
                     whole mirror back as new notes; mount outside the vault",
                    m.display_name(),
                    d.display()
                );
            }
            anc = d.parent();
        }
        // Inside a git work tree, the mirror AND `.docli/` must be git-ignored: `.docli/` holds
        // the full note-path tree of every mounted workspace, which must not sweep into a git
        // REMOTE any more than bodies may (the §8 inheritance is about the agent's machine, not
        // arbitrary remotes; only docli.toml is committed).
        if require_ignored {
            require_gitignored(&project_root_phys, a, m)?;
        }
    }
    if require_ignored {
        require_gitignored_control(&project_root_phys, &control)?;
    }
    Ok(())
}

/// The git work tree governing `from`, if any — the NEAREST one, so a mount inside a vendored
/// repository resolves to that repository rather than the outer project.
pub(crate) fn find_git_worktree(from: &Path) -> Option<PathBuf> {
    let mut d = Some(from);
    while let Some(p) = d {
        if p.join(".git").exists() {
            return Some(p.to_path_buf());
        }
        d = p.parent();
    }
    None
}

fn git_check_ignore(worktree: &Path, path: &Path) -> Result<bool> {
    // A RELATIVE path: git canonicalizes the repo root, and an un-canonicalized absolute arg
    // (macOS /var vs /private/var) reads as "outside repository" — which would report a
    // correctly-ignored mount as unignored.
    let rel = path.strip_prefix(worktree).unwrap_or(path);
    // Trailing slash: the paths checked here are DIRECTORIES that may not exist yet (a mount
    // before its first sync, `.docli/` before init), and a dir-only `.gitignore` pattern
    // (`mirror/`) does not match a bare nonexistent name.
    let mut arg = rel.as_os_str().to_os_string();
    arg.push("/");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("check-ignore")
        .arg("-q")
        // `--` before the path: a mount named `-notes` is otherwise parsed as an OPTION, and
        // the guard then reports «git could not answer» for a directory git ignores perfectly
        // well — blocking sync on a legal name.
        .arg("--")
        .arg(arg)
        // git's own diagnostics are SILENCED: this CLI reports the condition itself, in its own
        // voice, and a raw `fatal: not a git repository` printed twice above our message just
        // makes the reader hunt for which line matters.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // `git check-ignore` documents THREE outcomes and they must stay three: 0 = ignored,
    // 1 = not ignored, anything else = git could not answer (a dubious-ownership repository, a
    // corrupt `.git`, exit 128). Folding «could not answer» into «not ignored» made
    // `--gitignore` append entries and the gate refuse a moment later on the same failure.
    match out {
        Ok(s) if s.code() == Some(0) => Ok(true),
        Ok(s) if s.code() == Some(1) => Ok(false),
        // Code 128 is git's catch-all refusal, and by far its commonest cause is the
        // safe.directory check on a repository owned by another user (shared volumes, Docker
        // mounts, a checkout made under sudo). Naming that fix turns a dead end into one line
        // of shell; a broken `.git` lands here too, hence the second suggestion.
        Ok(s) => bail!(
            "`git check-ignore` in {} exited with code {} - cannot verify that the mirror is \
             hidden from git.\nIf the repository belongs to another user:\n  git config --global \
             --add safe.directory {}\nOtherwise repair the repository, or put the mirror \
             outside it.",
            worktree.display(),
            s.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "a signal".to_string()),
            worktree.display()
        ),
        Err(e) => bail!(
            "a git work tree was detected at {}, but `git` could not be run ({e}) - cannot \
             verify the mirror is git-ignored; install git or mount outside the repository",
            worktree.display()
        ),
    }
}

/// The advisory answer to «does this path still need a `.gitignore` entry?» — THREE-valued,
/// because the third value is the one that matters.
///
/// Collapsing «git could not answer» into either boolean is wrong in a different way each time.
/// As «missing», `--gitignore` appends lines on the strength of an answer nobody has, and the
/// gate refuses a moment later on the same git failure. As «covered», `status` reports no
/// problem and exits 0 while `sync` refuses, and the wizard says «`.gitignore` уже закрывает…»
/// just before the gate disagrees. The value has to survive to whoever renders it.
///
/// Never a gate itself — `require_gitignored` is the one that fails closed.
pub(crate) enum IgnoreState {
    /// Not in a git work tree: nothing to ignore, no requirement at all.
    NotInRepo,
    /// git ignores it already.
    Covered,
    /// git says it is not ignored — an entry is genuinely missing.
    Missing,
    /// git could not answer (broken repository, `safe.directory`, exit 128).
    Unknown(String),
}

pub(crate) fn ignore_state(from: &Path, path: &Path) -> IgnoreState {
    let Some(wt) = find_git_worktree(from) else {
        return IgnoreState::NotInRepo;
    };
    match git_check_ignore(&wt, path) {
        Ok(true) => IgnoreState::Covered,
        Ok(false) => IgnoreState::Missing,
        Err(e) => IgnoreState::Unknown(format!("{e:#}")),
    }
}

fn require_gitignored(project_root: &Path, mount: &Path, m: &Mount) -> Result<()> {
    let Some(wt) = find_git_worktree(mount) else {
        return Ok(());
    };
    if !git_check_ignore(&wt, mount)? {
        bail!(
            "mount `{}` ({}) is inside the git work tree at {} but is not git-ignored - \
             `git add -A` would stage mirrored note contents, which could then be committed \
             and pushed to a remote.\nAdd to .gitignore:\n  {}/\n  .docli/\nOr let docli \
             append them: docli init --gitignore",
            m.display_name(),
            mount.display(),
            wt.display(),
            mount.strip_prefix(&wt).unwrap_or(mount).display()
        );
    }
    let _ = project_root; // control-root check runs once, in validate_geometry
    Ok(())
}

fn require_gitignored_control(project_root: &Path, control: &Path) -> Result<()> {
    let Some(wt) = find_git_worktree(project_root) else {
        return Ok(());
    };
    // `.docli/` may not exist yet on a fresh init — check-ignore works on hypothetical paths.
    if !git_check_ignore(&wt, control)? {
        bail!(
            ".docli/ is inside the git work tree at {} but is NOT git-ignored - it holds the \
             full note-path tree of every mounted workspace.\nAdd to .gitignore:\n  .docli/\n\
             Or let docli append it: docli init --gitignore",
            wt.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_label_round_trips_beside_a_mount_array() {
        // toml 0.9 emits the root table before [[mount]] element tables, so the scalar can't
        // be absorbed into the last mount on re-read — pinned here because a serializer swap
        // would break it silently (round-2 R11).
        let mut c = cfg(vec![Mount {
            workspace: Uuid::from_u128(7),
            dir: "mirror".into(),
            folder: None,
            name: None,
        }]);
        c.mcp_label = Some("stable".into());
        let body = toml::to_string_pretty(&c).unwrap();
        let back: DocliToml = toml::from_str(&body).unwrap();
        assert_eq!(back.mcp_label.as_deref(), Some("stable"));
        assert_eq!(back.mounts.len(), 1);
        assert_eq!(back.mounts[0].dir, "mirror");
    }

    fn cfg(mounts: Vec<Mount>) -> DocliToml {
        DocliToml {
            server: default_server(),
            mounts,
            mcp_label: None,
        }
    }

    fn mount(ws: u128, dir: &str) -> Mount {
        Mount {
            workspace: Uuid::from_u128(ws),
            dir: dir.into(),
            folder: None,
            name: None,
        }
    }

    #[test]
    fn duplicate_workspace_ids_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(vec![mount(1, "a"), mount(1, "b")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("mounted more than once"), "{err}");
    }

    #[test]
    fn overlapping_and_nested_mounts_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        for (a, b) in [("m", "m"), ("m", "m/sub"), ("m/sub", "m")] {
            let c = cfg(vec![mount(1, a), mount(2, b)]);
            let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
            assert!(err.contains("overlap"), "{a} vs {b}: {err}");
        }
        let c = cfg(vec![mount(1, "ma"), mount(2, "mb")]);
        validate_geometry(tmp.path(), &c).unwrap();
    }

    #[test]
    fn a_mount_containing_the_control_plane_is_refused_both_directions() {
        let tmp = tempfile::tempdir().unwrap();
        // A mount of `.` contains docli.toml — a legal server node named `docli.toml` would
        // overwrite the configuration.
        let c = cfg(vec![mount(1, ".")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("docli.toml or .docli/ directory"), "{err}");
        // …and a parent dir too.
        let sub = tmp.path().join("proj");
        std::fs::create_dir_all(&sub).unwrap();
        let c = cfg(vec![mount(1, "..")]);
        let err = validate_geometry(&sub, &c).unwrap_err().to_string();
        assert!(err.contains("docli.toml or .docli/ directory"), "{err}");
        // Inverse containment: a mount inside .docli/ itself.
        let c = cfg(vec![mount(1, ".docli/mirror")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("inside .docli/"), "{err}");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn a_case_varied_spelling_cannot_dodge_geometry_on_a_folding_platform() {
        // `.DOCLI` does not exist yet, so `physicalize` cannot canonicalize it to its on-disk
        // spelling — but on macOS/Windows it IS `.docli`, and passing it here would let the
        // mount claim the control plane itself (Codex round 9).
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(vec![mount(1, ".DOCLI/mirror")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("inside .docli/"), "{err}");
        // Case-varied overlap between mounts is the same alias.
        let c = cfg(vec![mount(1, "Notes"), mount(2, "notes/sub")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("overlap"), "{err}");
        // Composed vs decomposed spellings alias physically too (APFS normalizes) — the fold
        // must be the FULL filesystem fold, not bare lowercase (Codex round 10).
        let c = cfg(vec![mount(1, "\u{c9}"), mount(2, "E\u{301}/sub")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn a_mount_inside_an_obsidian_vault_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vault/.obsidian")).unwrap();
        std::fs::create_dir_all(tmp.path().join("vault/notes")).unwrap();
        let c = cfg(vec![mount(1, "vault/notes")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("Obsidian vault"), "{err}");
        // …but the CONFIG-level half passes: search must not be blocked by mirror-WRITE
        // safety rules (Codex round 24 — the works-without-a-cache contract).
        validate_config(&c).unwrap();
    }

    #[test]
    fn loading_refuses_control_characters_anywhere_in_the_file() {
        // `docli.toml` is committed and teammate-editable; an escape sequence in ANY rendered
        // field reaches a colleague's terminal through whichever command prints it. Refusing at
        // the LOAD seam is what makes every downstream renderer safe without knowing about it.
        //
        // The vector is TOML's own `BSu001B` escape: a RAW control byte is already refused by
        // the TOML parser, but the escape decodes to one after parsing.
        let tmp = tempfile::tempdir().unwrap();
        let esc = "BSu001B";
        let mount = "workspace = QQ00000000-0000-0000-0000-000000000001QQ";
        for field in [
            format!("server = QQhttps://docli.ru/{esc}[2JQQ"),
            format!(
                "server = QQhttps://docli.ruQQ\n\n[[mount]]\n{mount}\ndir = QQcacheQQ\n\
                 name = QQ{esc}[2JforgedQQ"
            ),
            format!(
                "server = QQhttps://docli.ruQQ\n\n[[mount]]\n{mount}\ndir = QQcacheQQ\n\
                 folder = QQdocs{esc}[2JQQ"
            ),
        ] {
            let toml = field.replace("QQ", "\"").replace("BS", "\\");
            std::fs::write(tmp.path().join(CONFIG_NAME), &toml).unwrap();
            let err = match load_project(tmp.path()) {
                Err(e) => format!("{e:#}"),
                Ok(_) => panic!("{toml:?} must not load"),
            };
            assert!(err.contains("control character"), "{toml:?}: {err}");
        }
        // …and an ordinary file still loads, including a non-ASCII name.
        let ok = format!(
            "server = QQhttps://docli.ruQQ\n\n[[mount]]\n{mount}\ndir = QQcacheQQ\n\
             name = QQДокументацияQQ"
        )
        .replace("QQ", "\"");
        std::fs::write(tmp.path().join(CONFIG_NAME), ok).unwrap();
        assert_eq!(
            load_project(tmp.path()).unwrap().config.mounts[0]
                .name
                .as_deref(),
            Some("Документация")
        );
    }

    #[test]
    fn a_control_character_in_a_mount_dir_is_refused() {
        // A NEWLINE cannot be expressed as one `.gitignore` pattern: `cache\n/src` writes two,
        // and the second can hide the project's real `src` while the actual mount stays
        // unignored and the gate keeps refusing.
        let c = cfg(vec![mount(1, "cache\n/src")]);
        let err = validate_config(&c).unwrap_err().to_string();
        assert!(err.contains("control character"), "{err}");
    }

    #[test]
    fn an_impossible_folder_scope_is_refused() {
        // Codex round 25: a scope no server path can match would from-zero the mirror EMPTY.
        for bad in [
            "docs//private",
            "../up",
            "a/./b",
            "back\\slash",
            "ctl\u{7}",
            " lead",
            "trail ",
            "a/ b",
        ] {
            let mut m = mount(1, "m");
            m.folder = Some(bad.to_string());
            let err = validate_config(&cfg(vec![m])).unwrap_err().to_string();
            assert!(err.contains("folder scope is a relative"), "{bad}: {err}");
        }
        let mut long = mount(1, "m");
        long.folder = Some("я".repeat(130)); // 260 bytes > the 255-byte server cap
        assert!(validate_config(&cfg(vec![long])).is_err());
        let mut ok = mount(1, "m");
        ok.folder = Some("docs/приватное".to_string());
        validate_config(&cfg(vec![ok])).unwrap();
    }

    #[test]
    fn init_refuses_to_shadow_an_ancestor_project() {
        // Codex round 24: a nested docli.toml silently takes over for everything below it.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(CONFIG_NAME), "").unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let err = crate::init_cmd::run(
            &sub,
            None,
            &crate::init_cmd::InitArgs {
                server: None,
                workspace: None,
                dir: None,
                folder: None,
                name: None,
                mcp: None,
                mcp_label: None,
                mcp_bare: false,
                allow_prompt: false,
                clear_folder: false,
                write_gitignore: false,
                skills: Some("none".into()),
                hooks: None,
                instructions: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("inside the docli project"), "{err}");
        // From the project root itself, init still works (edit-in-place).
        assert!(!sub.join(CONFIG_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_mount_into_a_git_worktree_is_still_gated() {
        // Codex round 1: `/outside/link → /repo/inner` puts `link/m` PHYSICALLY inside /repo,
        // where no lexical ancestor of the mount ever meets `/repo/.git` — bodies could sweep
        // into a remote past the mandatory git-ignore gate. `physicalize` closes it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("inner")).unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(repo.join("inner"), &link).unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let c = cfg(vec![Mount {
            workspace: Uuid::from_u128(1),
            dir: link.join("m").display().to_string(),
            folder: None,
            name: None,
        }]);
        let err = validate_geometry(&proj, &c).unwrap_err().to_string();
        assert!(err.contains("not git-ignored"), "{err}");
    }

    #[test]
    fn a_git_worktree_mount_must_be_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        run(&["init", "-q"]);
        let c = cfg(vec![mount(1, "mirror")]);
        let err = validate_geometry(tmp.path(), &c).unwrap_err().to_string();
        assert!(err.contains("not git-ignored"), "{err}");
        std::fs::write(tmp.path().join(".gitignore"), "mirror/\n.docli/\n").unwrap();
        validate_geometry(tmp.path(), &c).unwrap();
    }
}
