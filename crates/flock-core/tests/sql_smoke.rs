//! Does the SQL work?
//!
//! FlockDB hosts a proven kernel and adds storage semantics under it (docs/02 §2.1). We are not
//! re-testing DuckDB — DuckDB has its own suite and it is far better than anything we would write.
//! What these tests check is that **nothing we do underneath breaks what DuckDB does on top**: that
//! a join still joins after the rows have been through a content-addressed page store and back.
//!
//! The interesting cases are the ones that write a *lot* of pages, or that make DuckDB restructure
//! its file — because that is where a page-marshalling bug would surface, and it would surface as a
//! wrong answer rather than as a crash.

mod common;

use common::{column_i64, column_str, scalar_i64};
use flock_core::{Db, Flock};

fn db(dir: &tempfile::TempDir) -> Db {
    Flock::open(dir.path(), "smoke").unwrap()
}

#[test]
fn create_insert_and_select_round_trip_through_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    db.execute("CREATE TABLE t (id INTEGER, name TEXT)")
        .unwrap();
    let n = db
        .execute("INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'kay')")
        .unwrap();
    assert_eq!(n, 3, "INSERT must report the rows it inserted");

    // Snapshot and restore forces every byte through the page store and back, which is the point:
    // a plain SELECT would never touch our code at all.
    let snap = db.snapshot().unwrap();
    db.restore(snap).unwrap();

    assert_eq!(
        column_str(&db.query("SELECT name FROM t ORDER BY id").unwrap()),
        vec!["ada", "grace", "kay"]
    );
}

#[test]
fn a_join_still_joins_after_a_snapshot_restore_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    db.execute_batch(
        "CREATE TABLE orders (id INTEGER, customer INTEGER, amount INTEGER);
         CREATE TABLE customers (id INTEGER, region TEXT);
         INSERT INTO orders VALUES (1, 10, 100), (2, 10, 50), (3, 20, 70);
         INSERT INTO customers VALUES (10, 'EMEA'), (20, 'AMER');",
    )
    .unwrap();

    let snap = db.snapshot().unwrap();
    db.restore(snap).unwrap();

    let rows = db
        .query(
            "SELECT CAST(sum(o.amount) AS BIGINT) AS total
             FROM orders o JOIN customers c ON o.customer = c.id
             WHERE c.region = 'EMEA'",
        )
        .unwrap();
    assert_eq!(scalar_i64(&rows), 150);
}

#[test]
fn aggregates_and_group_by_survive_a_snapshot_restore_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    db.execute("CREATE TABLE t AS SELECT i % 4 AS bucket, i AS v FROM range(10000) AS r(i)")
        .unwrap();
    let snap = db.snapshot().unwrap();
    db.restore(snap).unwrap();

    let rows = db
        .query("SELECT count(*) AS n FROM t GROUP BY bucket ORDER BY bucket")
        .unwrap();
    assert_eq!(column_i64(&rows), vec![2500, 2500, 2500, 2500]);

    let total = db.query("SELECT CAST(sum(v) AS BIGINT) FROM t").unwrap();
    assert_eq!(scalar_i64(&total), (0..10_000i64).sum::<i64>());
}

#[test]
fn window_functions_survive_a_snapshot_restore_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    db.execute("CREATE TABLE t AS SELECT i FROM range(5) AS r(i)")
        .unwrap();
    let snap = db.snapshot().unwrap();
    db.restore(snap).unwrap();

    let rows = db
        .query("SELECT CAST(sum(i) OVER (ORDER BY i) AS BIGINT) AS running FROM t ORDER BY i")
        .unwrap();
    assert_eq!(column_i64(&rows), vec![0, 1, 3, 6, 10]);
}

#[test]
fn a_database_large_enough_to_span_many_pages_round_trips_exactly() {
    // Two hundred thousand rows is comfortably more than one 64 KiB page, which is the whole point:
    // a marshalling bug that only bites at a page boundary is invisible in a three-row test, and it
    // would corrupt data rather than crash.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    db.execute(
        "CREATE TABLE t AS
           SELECT i AS id, repeat('x', 50) AS pad, i * 7 AS v
           FROM range(200000) AS r(i)",
    )
    .unwrap();

    let snap = db.snapshot().unwrap();
    db.restore(snap).unwrap();

    assert_eq!(
        scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()),
        200_000
    );
    assert_eq!(
        scalar_i64(&db.query("SELECT CAST(sum(v) AS BIGINT) FROM t").unwrap()),
        (0..200_000i64).map(|i| i * 7).sum::<i64>()
    );
    assert_eq!(
        scalar_i64(
            &db.query("SELECT count(*) FROM t WHERE pad <> repeat('x', 50)")
                .unwrap()
        ),
        0,
        "not one byte of padding may have changed on the way through the page store"
    );
}

#[test]
fn dropping_a_table_shrinks_the_database_and_the_table_stays_gone() {
    // The shrink path: DuckDB compacts, the file gets smaller, and if we failed to REMOVE the
    // logical pages past the new end, `hydrate` would weld the dead tail back on and the table
    // would rise from the grave on the next restore. There is a unit test for the paging layer;
    // this is the same bug, asserted where a user would notice it.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    db.execute("CREATE TABLE doomed AS SELECT i FROM range(100000) AS r(i)")
        .unwrap();
    db.snapshot().unwrap();

    db.execute("DROP TABLE doomed").unwrap();
    let snap = db.snapshot().unwrap();
    db.restore(snap).unwrap();

    assert!(
        db.query("SELECT count(*) FROM doomed").is_err(),
        "a dropped table must not come back after a restore"
    );
}

#[test]
fn a_sql_error_names_the_statement_and_suggests_the_next_command() {
    // CLAUDE.md: an error message must state the corrective action. The reader is often a person
    // under pressure, and what they need is the next thing to type.
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(&dir);

    let err = db.query("SELECT * FROM nonexistent").unwrap_err();
    let message = err.to_string();

    assert!(
        message.contains("nonexistent"),
        "the message must name the object: {message}"
    );
    assert!(
        message.contains("SHOW TABLES"),
        "the message must tell the reader what to run next: {message}"
    );
}
