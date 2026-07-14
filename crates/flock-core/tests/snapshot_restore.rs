//! Snapshot, restore, and the durability boundary — including the part of it that is not durable.
//!
//! A snapshot is a `ManifestId`: 32 bytes that *are* the whole database at an instant. Restoring is
//! pointing at those bytes again. Both are O(1) in substrate, and the tests here are about the
//! semantics rather than the speed — that a snapshot means what it says, that restoring gets you
//! precisely back, and that a database survives its own process dying.
//!
//! The last test in this file asserts a **limitation**. It is not a bug being tolerated; it is the
//! documented shape of the DuckDB fallback (see `flock-kernel`'s crate docs), and a test that pins
//! it down is worth more than a paragraph promising to remember it.

mod common;

use common::scalar_i64;
use flock_core::{Db, Flock};

fn seeded(dir: &tempfile::TempDir) -> Db {
    let mut db = Flock::open(dir.path(), "sales").unwrap();
    db.execute("CREATE TABLE t (id INTEGER)").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    db
}

#[test]
fn many_snapshots_in_a_row_all_replay_after_the_process_dies() {
    // ── This test exists because of a bug that was in this repository ─────────────────────────────
    //
    // FlockDB used to take the manifest id from `Pager::commit` — which installs a manifest and
    // writes NO log record — and then call `Wal::checkpoint(id)`. `checkpoint` is not a commit: it
    // persists the current head and *truncates the log behind it*. So every snapshot threw away the
    // log, and the commit protocol was being approximated by its own garbage-collection call.
    //
    // It happened to work, which is the worst outcome. Now the kernel commits through
    // `DurableStore::commit` — an fsync'd CRC-protected record before the manifest is installed —
    // and the log is a log. Twenty commits in a row, a process death, and a replay is the cheapest
    // way to keep it that way.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = seeded(&dir);
        for i in 4..=23 {
            db.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
            db.snapshot().unwrap();
        }
    } // the process "dies" — nothing is closed cleanly, nothing is flushed on the way out

    let mut db = Flock::open(dir.path(), "sales").unwrap();
    assert_eq!(
        scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()),
        23,
        "recovery landed on the wrong commit — the log did not replay every snapshot"
    );
    assert_eq!(
        scalar_i64(&db.query("SELECT max(id) FROM t").unwrap()),
        23,
        "recovery must restore the LAST committed head, not an earlier one"
    );
}

#[test]
fn a_restore_survives_a_restart() {
    // Restoring moves the head backwards. If that move is not made durable, the next open replays
    // the log, lands on the newest commit, and the restore is silently undone — the caller having
    // been told it succeeded.
    let dir = tempfile::tempdir().unwrap();
    let three_rows = {
        let mut db = seeded(&dir);
        let three_rows = db.snapshot().unwrap();
        db.execute("INSERT INTO t VALUES (4), (5)").unwrap();
        db.snapshot().unwrap();

        db.restore(three_rows).unwrap();
        assert_eq!(scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()), 3);
        three_rows
    };

    let mut db = Flock::open(dir.path(), "sales").unwrap();
    assert_eq!(
        scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()),
        3,
        "a restore must still be in effect after a restart, or it was never really a restore"
    );
    assert_eq!(db.head(), three_rows);
}

#[test]
fn restoring_a_snapshot_returns_the_database_to_exactly_that_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = seeded(&dir);

    let three_rows = db.snapshot().unwrap();

    db.execute("INSERT INTO t VALUES (4), (5)").unwrap();
    assert_eq!(scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()), 5);

    db.restore(three_rows).unwrap();
    assert_eq!(
        scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()),
        3,
        "restore must put back exactly the state the snapshot named"
    );
}

#[test]
fn a_snapshot_of_an_unchanged_database_is_the_same_snapshot() {
    // Content addressing, working. Identical content is identical identity — there is no clock, no
    // counter, and no nonce that could make two snapshots of the same bytes differ. It is also what
    // makes a pre-migration snapshot of ten thousand databases free enough to take unconditionally
    // (docs/02 §3.2).
    let dir = tempfile::tempdir().unwrap();
    let mut db = seeded(&dir);

    let a = db.snapshot().unwrap();
    let b = db.snapshot().unwrap();
    assert_eq!(a, b);
}

#[test]
fn you_can_go_forward_again_because_a_restore_destroys_nothing() {
    // Rewinding is a pointer move, not a deletion. The state you left is still a manifest, still
    // content-addressed, and still restorable — which is what makes "try it and see" safe.
    let dir = tempfile::tempdir().unwrap();
    let mut db = seeded(&dir);

    let three = db.snapshot().unwrap();
    db.execute("INSERT INTO t VALUES (4), (5)").unwrap();
    let five = db.snapshot().unwrap();

    db.restore(three).unwrap();
    assert_eq!(scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()), 3);

    db.restore(five).unwrap();
    assert_eq!(
        scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()),
        5,
        "the state we rewound past was not destroyed, so we can return to it"
    );
}

#[test]
fn restoring_a_snapshot_this_pool_has_never_seen_is_an_error_that_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = seeded(&dir);

    let fabricated = flock_core::ManifestId::from_bytes([0xab; 32]);
    let err = db.restore(fabricated).unwrap_err();

    assert!(matches!(
        err,
        flock_core::FlockError::UnknownSnapshot { .. }
    ));
    assert!(
        err.to_string().contains("is not in this pool"),
        "the message must explain the boundary, not just refuse: {err}"
    );
}

#[test]
fn a_snapshotted_database_survives_the_process_that_made_it() {
    // Dropping the `Db` destroys the DuckDB connection and its scratch file. Everything that is
    // left is pages, manifests, and a WAL — and if reopening cannot rebuild the database from
    // those, then FlockDB is a cache with delusions.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = seeded(&dir);
        db.execute("INSERT INTO t VALUES (4)").unwrap();
        db.snapshot().unwrap();
    } // <- connection closed, scratch file deleted, handle gone

    let mut reopened = Flock::open(dir.path(), "sales").unwrap();
    assert_eq!(
        scalar_i64(&reopened.query("SELECT count(*) FROM t").unwrap()),
        4,
        "a snapshotted database must come back from disk"
    );
}

#[test]
fn a_fork_survives_the_process_that_made_it_and_is_still_isolated() {
    // A fork is a first-class database, not a temporary view of one. It gets its own write-ahead
    // log at birth, so it comes back on its own after a restart — and it comes back holding *its*
    // rows, not its parent's.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut base = Flock::open(dir.path(), "sales").unwrap();
        base.execute("CREATE TABLE t (id INTEGER)").unwrap();
        base.execute("INSERT INTO t VALUES (1), (2)").unwrap();

        let mut fork = base.fork("experiment").unwrap();
        fork.execute("INSERT INTO t VALUES (3)").unwrap();
        fork.snapshot().unwrap();
        base.snapshot().unwrap();
    }

    let mut base = Flock::open(dir.path(), "sales").unwrap();
    let mut fork = Flock::open(dir.path(), "experiment").unwrap();

    assert_eq!(
        scalar_i64(&fork.query("SELECT count(*) FROM t").unwrap()),
        3
    );
    assert_eq!(
        scalar_i64(&base.query("SELECT count(*) FROM t").unwrap()),
        2
    );
}

// Sleep and wake are tested in `tiering.rs` — they are async, they need object storage, and they
// need the entire pool deleted between the two halves to prove anything.

#[test]
fn writes_made_after_the_last_snapshot_do_not_survive_a_crash() {
    // ── THIS TEST ASSERTS A LIMITATION, ON PURPOSE ────────────────────────────────────────────
    //
    // DuckDB commits an INSERT to its own scratch file. Substrate never hears about it until
    // `snapshot()` runs. So a process that dies between snapshots loses everything since the last
    // one — and this test pins that down, at the SQL level, so that nobody has to take the
    // documentation's word for it and nobody can quietly change it without noticing.
    //
    // It is the direct cost of the DuckDB fallback (see the `flock-kernel` crate docs), and it is
    // the second of the two things a real filesystem hook would fix. When that lands, this test
    // should FAIL — and the correct response will be to delete it and celebrate.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = seeded(&dir);
        db.snapshot().unwrap(); // three rows, durable

        db.execute("INSERT INTO t VALUES (99)").unwrap(); // never snapshotted
        assert_eq!(scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()), 4);

        // Dropping the handle without a snapshot is the best a test can do at simulating a crash:
        // the scratch file goes, and substrate has never seen row 99.
    }

    let mut reopened = Flock::open(dir.path(), "sales").unwrap();
    assert_eq!(
        scalar_i64(&reopened.query("SELECT count(*) FROM t").unwrap()),
        3,
        "un-snapshotted writes are lost — documented, deliberate, and pinned here so it stays known"
    );
}
