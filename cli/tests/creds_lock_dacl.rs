// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The credentials lock has to outlive the run that created it.
//!
//! Measured on Windows 2026-09-22: `restrict_dir` leaves `~/.docli/auth` with one ACE — Full to
//! OWNER RIGHTS — and NO `(OI)(CI)` inheritance flags, so a file BORN there lands with an EMPTY
//! DACL. Windows grants the CREATOR the access it asked for at creation time, which is what made
//! this invisible: the run that made `creds.lock` worked, and every run afterwards failed
//! `lock_for_write` and reported «the home directory is not writable … (outside an agent
//! sandbox)» about a home that was perfectly writable — a wrong diagnosis of a real breakage, on
//! the path that stores and REFRESHES tokens.
//!
//! These are INTEGRATION tests on purpose. The hole needs a SECOND opener of a lock an earlier
//! opener created, and a unit-level probe of the same sequence inside the creating process
//! passes — which is exactly how a green lib suite sat on top of it.

use std::fs::OpenOptions;
use std::path::Path;

use docli_cli::creds::{CredsStore, ServerCreds};

const SERVER: &str = "https://example.invalid";

fn creds() -> ServerCreds {
    ServerCreds {
        access_token: "docli_pat_test".into(),
        refresh_token: Some("r".into()),
        expires_at: Some(i64::MAX / 2),
        install_id: "i".into(),
    }
}

/// Can this file be opened for writing AT ALL? Deliberately an OPEN and not `exists()`: an
/// empty-DACL file is invisible to `exists()` too, because reading its attributes is denied.
fn writable(p: &Path) -> bool {
    OpenOptions::new().write(true).open(p).is_ok()
}

/// Leave the file the way an earlier version of the CLI left it: openable by nobody.
#[cfg(windows)]
fn break_permissions(p: &Path) {
    // Since `open_lock` hardens the lock it creates, the file carries an EXPLICIT owner ACE, and
    // `/inheritance:r` alone removes only INHERITED ones — the file would stay openable and the
    // test below would skip with nothing pinned. `/reset` first replaces the explicit ACEs with
    // the inherited set (nothing: `auth`'s one ACE carries no `(OI)(CI)`), then `/inheritance:r`
    // strips that — the empty `D:AI` state measured on the real install.
    for args in [&["/reset"][..], &["/inheritance:r"][..]] {
        let _ = std::process::Command::new("icacls")
            .arg(p)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[test]
fn a_lock_this_run_creates_is_openable_by_the_next_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".docli");

    let store = CredsStore::open(home.clone()).unwrap();
    store.put(SERVER, creds()).unwrap();
    let lock = home.join("auth").join("creds.lock");

    // A FRESH store is the second opener — the one that failed. `CredsStore::open` reaches the
    // lock only once a credentials file exists, which is why the `put` above comes first.
    let next = CredsStore::open(home.clone()).unwrap();
    next.put(SERVER, creds()).unwrap();

    assert!(
        writable(&lock),
        "{} must stay openable after the run that created it",
        lock.display()
    );
}

/// Windows only: the empty DACL is a Windows state, and on Unix an unopenable lock is a refusal
/// that must be EXPLAINED, never repaired (`creds::tests::a_write_refusal_on_the_lock_…`).
#[cfg(windows)]
#[test]
fn a_lock_an_earlier_version_left_unopenable_is_repaired_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".docli");

    let store = CredsStore::open(home.clone()).unwrap();
    store.put(SERVER, creds()).unwrap();
    let lock = home.join("auth").join("creds.lock");

    break_permissions(&lock);
    // The owner can always empty its own file's DACL, so a still-openable lock means the break
    // above failed — a skip here would pass with the repair unpinned.
    assert!(
        !writable(&lock),
        "could not construct an unopenable lock - the repair is not exercised"
    );

    // The OWNER keeps WRITE_DAC whatever the DACL says, which is
    // what makes an existing broken install repairable in place rather than only reinstallable.
    let store = CredsStore::open(home.clone()).unwrap();
    store
        .put(SERVER, creds())
        .expect("a lock left unopenable by an earlier version must be repaired, not diagnosed");

    assert!(writable(&lock), "the repair must leave the lock openable");
}

/// Nothing we spawn may narrate onto OUR stdout — that is where machine-readable output goes.
///
/// `icacls` prints a line per file it touches, in the console's OEM codepage, and an inherited
/// stdout put six of those lines AHEAD of the command's own output: `docli search --json` on
/// Windows emitted chatter and then the JSON, which is not JSON — and `search` is the one
/// command the contract says can establish that a note does not exist.
///
/// Asked of `status`, because it re-hardens the store (so `icacls` really does run) and prints
/// its own report to stdout, and asked as «the FIRST thing on stdout is ours» rather than by
/// matching the helper's text: that text is localized, and an English machine would pass a
/// substring check while broken.
#[cfg(windows)]
#[test]
fn no_spawned_helper_narrates_onto_our_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".docli");
    // The re-hardening path only runs once a credentials file EXISTS, so seed one first.
    CredsStore::open(home.clone())
        .unwrap()
        .put(SERVER, creds())
        .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_docli"))
        .arg("status")
        .env("DOCLI_HOME", &home)
        .current_dir(tmp.path())
        .output()
        .expect("the CLI runs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim_start().starts_with("docli-cli"),
        "stdout must begin with the command's own output, not a spawned helper's: {stdout:?}"
    );
}
