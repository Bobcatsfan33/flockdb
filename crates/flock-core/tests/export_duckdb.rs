//! **The escape hatch** (docs/02 §6.2), and the one test in this repository that is a *product*
//! test rather than an engineering one.
//!
//! The largest objection to adopting a new storage engine is not performance and it is not
//! features. It is: *"what if you disappear, or I hate you."* The only honest answer is one command
//! that hands the data back in a format with an ecosystem and no dependency on us. Anything less
//! makes us a hostage-taker, and serious buyers can smell it from across a room.
//!
//! So the test is deliberately hostile to ourselves. It does not check that FlockDB can read what
//! FlockDB wrote — that would prove nothing, and it is exactly the check a vendor writes when they
//! want the box ticked. It opens the exported file with a **stock `duckdb::Connection`** that has
//! never heard of FlockDB, and queries it.
//!
//! It runs in CI on every commit. It is not allowed to rot.

mod common;

use duckdb::Connection;
use flock_core::Flock;

/// Read a `count(*)` using nothing but vanilla DuckDB. No FlockDB types appear in this function,
/// and that is the entire point of it.
fn count_with_stock_duckdb(path: &std::path::Path, table: &str) -> i64 {
    let conn = Connection::open(path).expect("stock DuckDB could not open the exported file");
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("stock DuckDB could not query the exported file")
}

#[test]
fn export_produces_a_file_a_stock_duckdb_can_open_and_query() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Flock::open(dir.path(), "sales").unwrap();

    db.execute("CREATE TABLE t (id INTEGER, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'ada'), (2, 'grace')")
        .unwrap();

    let out = dir.path().join("exported.duckdb");
    db.export_duckdb(&out).unwrap();

    assert!(out.exists(), "export wrote nothing");
    assert_eq!(count_with_stock_duckdb(&out, "t"), 2);
}

#[test]
fn an_exported_file_carries_the_schema_not_just_the_rows() {
    // "We exported your data" is not the promise. The promise is that the file is a *database*:
    // types, constraints, multiple tables, the lot. A pile of rows with the column types guessed
    // is not something anyone can use, and it is what a lazy export produces.
    let dir = tempfile::tempdir().unwrap();
    let mut db = Flock::open(dir.path(), "sales").unwrap();

    db.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, region VARCHAR NOT NULL);
         CREATE TABLE orders (id INTEGER, customer INTEGER, amount DECIMAL(10, 2), placed_at TIMESTAMP);
         INSERT INTO customers VALUES (1, 'EMEA'), (2, 'AMER');
         INSERT INTO orders VALUES (1, 1, 99.50, TIMESTAMP '2026-01-01 10:00:00');",
    )
    .unwrap();

    let out = dir.path().join("exported.duckdb");
    db.export_duckdb(&out).unwrap();

    let conn = Connection::open(&out).unwrap();

    // Both tables are there.
    assert_eq!(count_with_stock_duckdb(&out, "customers"), 2);
    assert_eq!(count_with_stock_duckdb(&out, "orders"), 1);

    // And so are the types — a DECIMAL that came back as a DOUBLE would be a silently wrong export
    // of exactly the column an accountant cares about.
    let amount_type: String = conn
        .query_row(
            "SELECT data_type FROM information_schema.columns
             WHERE table_name = 'orders' AND column_name = 'amount'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        amount_type.starts_with("DECIMAL"),
        "the exported column type must survive, got {amount_type}"
    );

    // A join across the exported tables, in stock DuckDB. If this works, the file is a database.
    let total: f64 = conn
        .query_row(
            "SELECT CAST(sum(o.amount) AS DOUBLE)
             FROM orders o JOIN customers c ON o.customer = c.id
             WHERE c.region = 'EMEA'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!((total - 99.50).abs() < 1e-9);
}

#[test]
fn export_includes_writes_made_since_the_last_snapshot() {
    // The moment a user most wants an export is the moment they least trust the rest of the system.
    // An export that quietly gave them only the last *snapshot* — while their session showed the
    // newer rows — would hand them a file that is missing data they can see on their screen.
    let dir = tempfile::tempdir().unwrap();
    let mut db = Flock::open(dir.path(), "sales").unwrap();

    db.execute("CREATE TABLE t (id INTEGER)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.snapshot().unwrap();

    db.execute("INSERT INTO t VALUES (2)").unwrap(); // deliberately NOT snapshotted

    let out = dir.path().join("exported.duckdb");
    db.export_duckdb(&out).unwrap();

    assert_eq!(
        count_with_stock_duckdb(&out, "t"),
        2,
        "the export must contain what the user can see, not what we last committed"
    );
}

#[test]
fn a_fork_exports_the_forks_data_and_not_its_parents() {
    let dir = tempfile::tempdir().unwrap();
    let mut base = Flock::open(dir.path(), "sales").unwrap();
    base.execute("CREATE TABLE t (id INTEGER)").unwrap();
    base.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();

    let mut fork = base.fork("experiment").unwrap();
    fork.execute("DELETE FROM t WHERE id = 1").unwrap();

    let fork_out = dir.path().join("fork.duckdb");
    let base_out = dir.path().join("base.duckdb");
    fork.export_duckdb(&fork_out).unwrap();
    base.export_duckdb(&base_out).unwrap();

    assert_eq!(count_with_stock_duckdb(&fork_out, "t"), 2);
    assert_eq!(count_with_stock_duckdb(&base_out, "t"), 3);
}

#[test]
fn a_restored_database_exports_the_restored_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Flock::open(dir.path(), "sales").unwrap();

    db.execute("CREATE TABLE t (id INTEGER)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let one_row = db.snapshot().unwrap();

    db.execute("INSERT INTO t VALUES (2), (3)").unwrap();
    db.restore(one_row).unwrap();

    let out = dir.path().join("exported.duckdb");
    db.export_duckdb(&out).unwrap();
    assert_eq!(count_with_stock_duckdb(&out, "t"), 1);
}

#[test]
fn export_refuses_to_overwrite_and_the_original_file_is_untouched() {
    // An export is what people run when they are frightened about their data. Clobbering a file at
    // the destination is the worst conceivable moment to be helpful, and "it was in the docs" is
    // not a defence.
    let dir = tempfile::tempdir().unwrap();
    let mut db = Flock::open(dir.path(), "sales").unwrap();
    db.execute("CREATE TABLE t (id INTEGER)").unwrap();

    let out = dir.path().join("precious.duckdb");
    std::fs::write(&out, b"last year's numbers").unwrap();

    let err = db.export_duckdb(&out).unwrap_err();
    assert!(err.to_string().contains("will not overwrite"), "got: {err}");
    assert_eq!(std::fs::read(&out).unwrap(), b"last year's numbers");
}

#[test]
fn a_large_export_is_complete_and_not_merely_openable() {
    // A truncated file often still opens. Row counts and checksums are what catch a short write,
    // and this is the size at which a short write becomes possible at all.
    let dir = tempfile::tempdir().unwrap();
    let mut db = Flock::open(dir.path(), "sales").unwrap();

    db.execute(
        "CREATE TABLE t AS SELECT i AS id, repeat('y', 40) AS pad FROM range(200000) AS r(i)",
    )
    .unwrap();

    let out = dir.path().join("big.duckdb");
    db.export_duckdb(&out).unwrap();

    let conn = Connection::open(&out).unwrap();
    let (n, sum): (i64, i64) = conn
        .query_row("SELECT count(*), sum(id) FROM t", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();

    assert_eq!(n, 200_000);
    assert_eq!(sum, (0..200_000i64).sum::<i64>());
}
