//! **The headline property, asserted as bluntly as it can be written.**
//!
//! FlockDB's entire pitch is that a fork is a *real, separate database* that cost nothing to make.
//! If a write to a fork can be seen in its parent, there is no product — not a degraded one, not a
//! slower one: none. A per-tenant database whose isolation leaks is a `tenant_id` column with extra
//! steps and worse marketing.
//!
//! So these tests are deliberately unclever. They insert a row into a fork and then ask the parent
//! to count its rows. There is no property-based generator here and no abstraction to admire —
//! the assertion a customer will make in their POC is exactly this one, and it should be exactly
//! this legible.

mod common;

use common::{column_i64, scalar_i64};
use flock_core::Flock;

fn pool() -> tempfile::TempDir {
    tempfile::tempdir().expect("could not make a temp pool")
}

#[test]
fn a_write_to_a_fork_is_not_visible_in_the_base() {
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();

    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    base.snapshot().unwrap();

    let mut fork = base.fork("experiment").unwrap();
    fork.execute("INSERT INTO t VALUES (4)").unwrap();

    assert_eq!(
        scalar_i64(&fork.query("SELECT count(*) FROM t").unwrap()),
        4
    );
    assert_eq!(
        scalar_i64(&base.query("SELECT count(*) FROM t").unwrap()),
        3,
        "THE headline property: the base must not have seen the fork's INSERT"
    );
}

#[test]
fn a_delete_in_a_fork_does_not_delete_from_the_base() {
    // The quickstart in docs/04 §3 ends on this exact line, so it had better be true.
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();

    base.execute("CREATE TABLE t AS SELECT * FROM range(1000) AS r(id)")
        .unwrap();
    base.snapshot().unwrap();

    let mut fork = base.fork("experiment").unwrap();
    fork.execute("DELETE FROM t WHERE id >= 500").unwrap();

    assert_eq!(
        scalar_i64(&fork.query("SELECT count(*) FROM t").unwrap()),
        500
    );
    assert_eq!(
        scalar_i64(&base.query("SELECT count(*) FROM t").unwrap()),
        1000,
        "the base is untouched. two databases now."
    );
}

#[test]
fn a_write_to_the_base_after_forking_is_not_visible_in_the_fork() {
    // Isolation runs in both directions, and it is the *less* obvious direction that a naive
    // "the fork gets a copy" implementation would still pass. A shared mutable page store would
    // fail here and nowhere else.
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();

    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (1)").unwrap();

    let mut fork = base.fork("experiment").unwrap();

    base.execute("INSERT INTO t VALUES (2)").unwrap();
    base.snapshot().unwrap();

    assert_eq!(
        scalar_i64(&base.query("SELECT count(*) FROM t").unwrap()),
        2
    );
    assert_eq!(
        scalar_i64(&fork.query("SELECT count(*) FROM t").unwrap()),
        1,
        "the fork must not have seen a write its parent made after the fork"
    );
}

#[test]
fn a_fork_sees_writes_the_parent_had_not_snapshotted_yet() {
    // `fork` checkpoints the parent first, and this is why it must. Without that, the fork would
    // branch from the parent's LAST SNAPSHOT and silently lose every row the caller inserted since
    // — while every one of those INSERTs reported success. Of every bug this design can have, a
    // fork that quietly returns stale data is the one a user is least equipped to notice.
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();

    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (1)").unwrap();
    base.snapshot().unwrap();

    // Written AFTER the last snapshot, and never snapshotted.
    base.execute("INSERT INTO t VALUES (2), (3)").unwrap();

    let mut fork = base.fork("experiment").unwrap();
    assert_eq!(
        column_i64(&fork.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![1, 2, 3],
        "a fork must carry the parent's un-snapshotted writes, not silently drop them"
    );
}

#[test]
fn a_fork_of_a_fork_is_isolated_from_both_of_its_ancestors() {
    // Branch trees to arbitrary depth (docs/02 §3.1). If isolation only held one level deep, the
    // "try three hypotheses and keep the one that worked" story that FlockDB shares with LoomDB
    // would be a story about exactly one hypothesis.
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();
    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (1)").unwrap();

    let mut child = base.fork("child").unwrap();
    child.execute("INSERT INTO t VALUES (2)").unwrap();

    let mut grandchild = child.fork("grandchild").unwrap();
    grandchild.execute("INSERT INTO t VALUES (3)").unwrap();

    assert_eq!(
        scalar_i64(&grandchild.query("SELECT count(*) FROM t").unwrap()),
        3
    );
    assert_eq!(
        scalar_i64(&child.query("SELECT count(*) FROM t").unwrap()),
        2
    );
    assert_eq!(
        scalar_i64(&base.query("SELECT count(*) FROM t").unwrap()),
        1
    );
}

#[test]
fn two_forks_of_the_same_parent_cannot_see_each_other() {
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();
    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (0)").unwrap();

    let mut h1 = base.fork("hypothesis-1").unwrap();
    let mut h2 = base.fork("hypothesis-2").unwrap();

    h1.execute("INSERT INTO t VALUES (1)").unwrap();
    h2.execute("INSERT INTO t VALUES (2)").unwrap();

    assert_eq!(
        column_i64(&h1.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![0, 1]
    );
    assert_eq!(
        column_i64(&h2.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![0, 2]
    );
    assert_eq!(
        column_i64(&base.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![0]
    );
}

#[test]
fn a_fork_can_change_the_schema_without_the_base_noticing() {
    // Per-tenant schema evolution is one of the two things a `tenant_id` column structurally
    // cannot do (docs/02 §1), so it is worth an assertion of its own rather than trusting that
    // "DDL is just another write".
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();
    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (1)").unwrap();

    let mut fork = base.fork("migration-canary").unwrap();
    fork.execute("ALTER TABLE t ADD COLUMN region TEXT")
        .unwrap();
    fork.execute("DROP TABLE IF EXISTS t2").unwrap();
    fork.execute("CREATE TABLE t2 (x INTEGER)").unwrap();
    fork.snapshot().unwrap();

    // The fork has a column and a table the base does not.
    assert_eq!(
        scalar_i64(
            &fork
                .query("SELECT count(*) FROM information_schema.columns WHERE table_name = 't'")
                .unwrap()
        ),
        2
    );
    assert_eq!(
        scalar_i64(
            &base
                .query("SELECT count(*) FROM information_schema.columns WHERE table_name = 't'")
                .unwrap()
        ),
        1,
        "the base's schema must be untouched by a migration applied to a fork"
    );
    assert!(
        base.query("SELECT * FROM t2").is_err(),
        "the base must not have a table that only the fork created"
    );
}

#[test]
fn forking_onto_a_name_that_already_exists_is_refused_rather_than_silently_adopted() {
    // The dangerous failure is not "overwrite". It is "adopt": recover the existing database's WAL
    // into the fork's handle and return, successfully, holding some other tenant's rows.
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();
    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.snapshot().unwrap();

    base.fork("taken").unwrap();
    let err = base.fork("taken").unwrap_err();

    assert!(
        matches!(err, flock_core::FlockError::NameTaken { .. }),
        "expected NameTaken, got {err:?}"
    );
}

#[test]
fn a_fork_named_with_a_path_traversal_is_refused() {
    // A pool is a security boundary (docs/02 §9.1), and a name is a directory. `../../evil` would
    // put a database's durable head outside its own pool.
    let dir = pool();
    let mut base = Flock::open(dir.path(), "sales").unwrap();
    let err = base.fork("../evil").unwrap_err();
    assert!(matches!(err, flock_core::FlockError::BadName { .. }));
}
