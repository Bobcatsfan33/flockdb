//! Read replicas end to end: a follower that serves SQL of a primary it is streaming from.
//!
//! These exercise the whole stack — DuckDB → substrate pages → shipped WAL → a follower's DuckDB —
//! rather than the page-level machinery `flock-sync`'s own oracle already hammers. The point here is
//! that the *SQL* a replica answers matches the primary's, and that a query sees a whole snapshot
//! that advances only when asked.

mod common;
use common::{column_str, scalar_i64};

use flock_core::{Flock, ReadReplica};

/// A follower catches up to a primary and answers the same queries.
#[test]
fn a_read_replica_serves_the_primarys_data() {
    let proot = tempfile::tempdir().unwrap();
    let mut primary = Flock::open(proot.path(), "sales").unwrap();
    primary
        .execute("CREATE TABLE t (id INTEGER, name TEXT)")
        .unwrap();
    primary
        .execute("INSERT INTO t VALUES (1, 'alpha'), (2, 'beta')")
        .unwrap();
    // A FlockDB write is durable — and shippable — only once it is snapshotted: that is the commit
    // point (the fsync'd WAL record). Until then it is in a DuckDB scratch file substrate never saw.
    primary.snapshot().unwrap();

    // The follower is its OWN pool, on its own disk — not sharing the primary's CAS. Cross-machine.
    let rroot = tempfile::tempdir().unwrap();
    let mut replica = ReadReplica::open(rroot.path(), "sales").unwrap();

    let applied = replica.catch_up(&primary.wal_source().unwrap()).unwrap();
    assert!(
        applied >= 1,
        "at least the CREATE+INSERT snapshot should ship"
    );
    replica.refresh().unwrap();

    assert_eq!(
        scalar_i64(&replica.query("SELECT count(*) FROM t").unwrap()),
        2
    );
    assert_eq!(
        column_str(&replica.query("SELECT name FROM t ORDER BY id").unwrap()),
        vec!["alpha".to_string(), "beta".to_string()]
    );
    // Byte-for-byte at the storage layer: the follower's applied head equals the primary's head.
    assert_eq!(replica.applied_head(), primary.head());
}

/// The visible snapshot advances only on `refresh`, never under a running read.
#[test]
fn a_replicas_reads_advance_only_on_refresh() {
    let proot = tempfile::tempdir().unwrap();
    let mut primary = Flock::open(proot.path(), "sales").unwrap();
    primary.execute("CREATE TABLE t (id INTEGER)").unwrap();
    primary.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    primary.snapshot().unwrap();

    let rroot = tempfile::tempdir().unwrap();
    let mut replica = ReadReplica::open(rroot.path(), "sales").unwrap();
    replica.catch_up(&primary.wal_source().unwrap()).unwrap();
    replica.refresh().unwrap();
    assert_eq!(
        scalar_i64(&replica.query("SELECT count(*) FROM t").unwrap()),
        2
    );

    // The primary moves on.
    primary
        .execute("INSERT INTO t VALUES (3), (4), (5)")
        .unwrap();
    primary.snapshot().unwrap();

    // The follower applies the new commit — its durable head advances…
    replica.catch_up(&primary.wal_source().unwrap()).unwrap();
    assert_eq!(
        replica.applied_head(),
        primary.head(),
        "durable head caught up"
    );

    // …but queries still see the old, whole snapshot until we refresh. Never a half-applied view.
    assert_eq!(
        scalar_i64(&replica.query("SELECT count(*) FROM t").unwrap()),
        2,
        "the visible snapshot must not move under a reader"
    );

    replica.refresh().unwrap();
    assert_eq!(
        scalar_i64(&replica.query("SELECT count(*) FROM t").unwrap()),
        5,
        "after refresh the newer snapshot is visible"
    );
}

/// Point-in-time restore, at the SQL level: rewind a follower to an earlier commit and see the
/// database exactly as it was then.
#[test]
fn point_in_time_restore_shows_an_earlier_state() {
    let proot = tempfile::tempdir().unwrap();
    let mut primary = Flock::open(proot.path(), "ledger").unwrap();
    primary.execute("CREATE TABLE t (id INTEGER)").unwrap();
    primary.execute("INSERT INTO t VALUES (1)").unwrap();
    primary.snapshot().unwrap();

    // Remember the LSN of this early state, then keep writing.
    let early_lsn = {
        let src = primary.wal_source().unwrap();
        let shipments = src.shipments_from(0).unwrap();
        shipments.last().unwrap().commit_lsn
    };

    primary.execute("INSERT INTO t VALUES (2), (3)").unwrap();
    primary.snapshot().unwrap();

    // A fresh follower restored to the early LSN sees only the first row.
    let rroot = tempfile::tempdir().unwrap();
    let mut replica = ReadReplica::open(rroot.path(), "ledger").unwrap();
    replica
        .restore_to(&primary.wal_source().unwrap(), early_lsn)
        .unwrap();

    assert_eq!(
        scalar_i64(&replica.query("SELECT count(*) FROM t").unwrap()),
        1,
        "restored to the early LSN, only the first insert is present"
    );
}
