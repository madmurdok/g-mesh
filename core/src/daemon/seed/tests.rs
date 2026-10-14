//! `daemon::seed` over temp dirs and real git repositories
//! (docs/design/GM-564-seed-worktree-index.md, "Behaviours the tests must
//! pin", 1-4). The source index lives under a temp g-mesh home
//! ([`seed_if_eligible_under`]), so nothing here reads or writes the user's.
//!
//! Controls (code reverts, never the test):
//! - in `check_source`, accept any generation: the mismatch case seeds
//!   (`no_seed_when_the_generation_differs` fails);
//! - replace the `Backup` in `seed_from` with `fs::copy` of `index.db`: the
//!   WAL-only row is missing (`a_seed_holds_commits_still_only_in_the_sources_wal`);
//! - in `main_checkout`, use the gitdir itself as the common dir when it has
//!   no `commondir`: the separate-git-dir checkout maps to a "main"
//!   (`main_checkout_is_none_without_a_commondir`).

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{Connection, OpenFlags};

use super::*;
use crate::daemon::indexing_status::ColdCause;

const VERSION: &str = "seed-test-generation";

/// Runs git in `dir`, with an identity and no signing, so the user's global
/// config cannot change what a commit needs.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=g-mesh test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A main checkout with one commit (`src/a.ts`, `sub/b.ts`) and a linked
/// worktree of it, both under one temp dir, canonical.
struct Repos {
    _dir: tempfile::TempDir,
    main: PathBuf,
    worktree: PathBuf,
}

fn repos() -> Repos {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let main = base.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    fs::create_dir_all(main.join("sub")).unwrap();
    fs::write(main.join("src/a.ts"), "export const a = 1;\n").unwrap();
    fs::write(main.join("sub/b.ts"), "export const b = 1;\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let worktree = base.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "wt-branch", worktree.to_str().unwrap()]);
    Repos { _dir: dir, main, worktree }
}

/// Creates an index at `path` of generation `version`, its walk recorded
/// complete when `walked`, in WAL mode as the daemon keeps it.
fn create_index(path: &Path, version: &str, walked: bool) -> Connection {
    vectors::register_extension();
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    schema::ensure_current(&conn, version).unwrap();
    if walked {
        conn.execute("UPDATE meta SET bulkIndexedAt = CURRENT_TIMESTAMP WHERE id = 1", []).unwrap();
    }
    conn
}

/// The source index of `main` under `home`, written and closed.
fn source_index(home: &Path, main: &Path, version: &str, walked: bool) -> PathBuf {
    let dir = connection::project_dir_under(home, main).unwrap();
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("index.db");
    let conn = create_index(&db, version, walked);
    schema::set_resolution_facts(&conn, "typescript", Some("source facts")).unwrap();
    drop(conn);
    db
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn main_checkout_maps_a_linked_worktree_and_its_subdirectories_to_main() {
    let repos = repos();
    assert_eq!(main_checkout(&repos.worktree), Some(repos.main.clone()));
    assert_eq!(main_checkout(&repos.worktree.join("sub")), Some(repos.main.join("sub")));
}

#[test]
fn main_checkout_is_none_for_the_main_checkout_itself() {
    let repos = repos();
    assert_eq!(main_checkout(&repos.main), None);
    assert_eq!(main_checkout(&repos.main.join("sub")), None);
}

/// A `.git` file whose gitdir has no `commondir`: a submodule-style checkout
/// (gitdir under the parent's `.git/modules`), and a separate-git-dir
/// checkout whose gitdir is another tree's `.git`, which only the
/// `commondir` rule rejects.
#[test]
fn main_checkout_is_none_without_a_commondir() {
    let repos = repos();
    let submodule = repos.main.join("module");
    let modules = repos.main.join(".git/modules/module");
    fs::create_dir_all(&submodule).unwrap();
    fs::create_dir_all(modules.parent().unwrap()).unwrap();
    git(&submodule, &["init", "-q", "--separate-git-dir", modules.to_str().unwrap()]);
    assert!(submodule.join(".git").is_file());
    assert_eq!(main_checkout(&submodule), None, "a submodule");

    let base = repos.main.parent().unwrap();
    let elsewhere = base.join("elsewhere");
    let separate = base.join("separate");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(&separate).unwrap();
    git(&separate, &["init", "-q", "--separate-git-dir", elsewhere.join(".git").to_str().unwrap()]);
    assert!(separate.join(".git").is_file() && elsewhere.join(".git").is_dir());
    assert_eq!(main_checkout(&separate), None, "a separate git dir named .git");
}

/// The common dir is named `.git` and is its parent's `.git` directory, but
/// a bare repository's: not a working tree.
#[test]
fn main_checkout_is_none_for_a_bare_repos_worktree() {
    let repos = repos();
    let base = repos.main.parent().unwrap();
    let bare_parent = base.join("bare");
    fs::create_dir_all(&bare_parent).unwrap();
    let bare = bare_parent.join(".git");
    git(base, &["clone", "-q", "--bare", repos.main.to_str().unwrap(), bare.to_str().unwrap()]);
    let worktree = base.join("bare-wt");
    git(&bare, &["worktree", "add", "-q", "-b", "bare-branch", worktree.to_str().unwrap()]);
    assert!(worktree.join(".git").is_file());
    assert_eq!(main_checkout(&worktree), None);
}

#[test]
fn seeding_is_switched_by_g_mesh_seed() {
    assert!(!seed_enabled_from(Some("0")));
    assert!(!seed_enabled_from(Some(" 0 ")));
    assert!(seed_enabled_from(Some("1")));
    assert!(seed_enabled_from(None), "on by default");
}

/// The eligible case, from a source a cleanly exited daemon left (no `-wal`):
/// the copy is in place, records where it came from, and is pending its
/// reconcile.
#[test]
fn a_worktree_is_seeded_from_a_source_with_no_wal_file() {
    let repos = repos();
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let source = source_index(home.path(), &repos.main, VERSION, true);
    assert!(!PathBuf::from(format!("{}-wal", source.display())).exists(), "the source was closed cleanly");

    let origin = seed_if_eligible_under(home.path(), &repos.worktree, state.path(), VERSION)
        .expect("an eligible worktree is seeded");
    assert_eq!(origin.source_root, repos.main);
    assert_eq!(entries(state.path()), ["index.db"], "no temp file or side file is left");
    let seeded = Connection::open(state.path().join("index.db")).unwrap();
    assert_eq!(schema::resolution_facts(&seeded, "typescript").unwrap().as_deref(), Some("source facts"));
    assert!(schema::seed_pending(&seeded).unwrap());
    let recorded = schema::seed_source_root(&seeded).unwrap().expect("the seed records its source");
    assert_eq!(Path::new(&recorded), repos.main);
    assert!(schema::bulk_index_completed(&seeded).unwrap());
}

#[test]
fn no_seed_when_the_target_index_exists() {
    let repos = repos();
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    source_index(home.path(), &repos.main, VERSION, true);
    let target = state.path().join("index.db");
    fs::write(&target, b"the worktree's own index").unwrap();

    assert!(seed_if_eligible_under(home.path(), &repos.worktree, state.path(), VERSION).is_none());
    assert_eq!(fs::read(&target).unwrap(), b"the worktree's own index");
    assert_eq!(entries(state.path()), ["index.db"]);
}

#[test]
fn no_seed_when_the_source_never_finished_its_walk() {
    let repos = repos();
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    source_index(home.path(), &repos.main, VERSION, false);

    assert!(seed_if_eligible_under(home.path(), &repos.worktree, state.path(), VERSION).is_none());
    assert!(entries(state.path()).is_empty(), "{:?}", entries(state.path()));
}

#[test]
fn no_seed_when_the_generation_differs() {
    let repos = repos();
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    source_index(home.path(), &repos.main, "another-generation", true);

    assert!(seed_if_eligible_under(home.path(), &repos.worktree, state.path(), VERSION).is_none());
    assert!(entries(state.path()).is_empty(), "{:?}", entries(state.path()));
}

/// A row a second connection committed with checkpoints off is only in the
/// source's `-wal` (a copy of `index.db` alone lacks it, checked here); the
/// seed has it.
#[test]
fn a_seed_holds_commits_still_only_in_the_sources_wal() {
    let source_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("index.db");
    let first = create_index(&source, VERSION, true);
    first.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
    let writer = Connection::open(&source).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    schema::set_resolution_facts(&writer, "typescript", Some("wal only")).unwrap();

    let main_file_only = source_dir.path().join("main-file-only.db");
    fs::copy(&source, &main_file_only).unwrap();
    let copy = Connection::open_with_flags(&main_file_only, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        schema::resolution_facts(&copy, "typescript").unwrap(),
        None,
        "the row must be in the -wal only for this test to mean anything"
    );
    drop(copy);

    let origin = seed_from(&source, state.path(), Path::new("/main"), VERSION).unwrap();
    assert!(origin.is_some());
    let seeded = Connection::open(state.path().join("index.db")).unwrap();
    assert_eq!(schema::resolution_facts(&seeded, "typescript").unwrap().as_deref(), Some("wal only"));
    drop((first, writer));
}

/// `reset` (the wipe a generation change or `g-mesh reindex` runs) drops
/// the seed's origin: a reset index is no longer a seed.
#[test]
fn a_reset_index_is_no_longer_a_seed() {
    let dir = tempfile::tempdir().unwrap();
    let conn = create_index(&dir.path().join("index.db"), VERSION, true);
    schema::record_seed_origin(&conn, "/main").unwrap();
    assert!(schema::seed_pending(&conn).unwrap());

    schema::reset(&conn, VERSION).unwrap();
    assert!(!schema::seed_pending(&conn).unwrap());
    assert_eq!(schema::seed_source_root(&conn).unwrap(), None);
}

#[test]
fn a_stale_seed_temp_file_is_removed_and_the_index_kept() {
    let state = tempfile::tempdir().unwrap();
    for name in ["index.db", "index.db.seed-tmp", "index.db.seed-tmp-wal", "index.db.seed-tmp-shm"] {
        fs::write(state.path().join(name), name).unwrap();
    }
    remove_stale_seed_tmp(state.path());
    assert_eq!(entries(state.path()), ["index.db"]);
}

/// The gate keeps a seeded start at `Walking` until it opens (or drops),
/// then `Structural`, and tells the activation once; it never moves a phase
/// that is no longer `Walking`.
#[test]
fn the_seed_gate_holds_walking_until_it_opens() {
    let indexing = IndexingStatus::walking_because(ColdCause::Seeded);
    assert!(
        indexing.progress_detail().contains("copied from the main checkout"),
        "{}",
        indexing.progress_detail()
    );
    let (gate, reconciled) = SeedGate::new(indexing.clone());
    assert_eq!(indexing.phase(), Phase::Walking);
    assert!(reconciled.try_recv().is_err());
    gate.open();
    assert_eq!(indexing.phase(), Phase::Structural);
    assert!(reconciled.try_recv().is_ok());

    let indexing = IndexingStatus::walking_because(ColdCause::Seeded);
    let (gate, reconciled) = SeedGate::new(indexing.clone());
    drop(gate);
    assert_eq!(indexing.phase(), Phase::Structural, "a dropped gate (a panicking reconcile) opens too");
    assert!(reconciled.try_recv().is_ok());

    let indexing = IndexingStatus::walking_because(ColdCause::Seeded);
    let (gate, _reconciled) = SeedGate::new(indexing.clone());
    indexing.set_phase(Phase::Ready);
    gate.open();
    assert_eq!(indexing.phase(), Phase::Ready, "the phase never moves backwards");
}
