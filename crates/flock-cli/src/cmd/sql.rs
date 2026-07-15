//! `flock sql "<query>"` — and the snapshot nobody asked for.
//!
//! # Why every `flock sql` commits
//!
//! In the library, `execute()` writes to DuckDB's scratch file and `snapshot()` is what makes it
//! durable. That is a good split for a program that holds a `Db` open across a thousand statements.
//!
//! A CLI is not that program. Each command is a **process**: it opens the database, does one thing,
//! and dies — and when it dies the kernel's `TempDir` is deleted and the scratch file with it. A
//! `flock sql "INSERT ..."` that did not snapshot would report success, print a row count, and lose
//! the row. Silently. Every time.
//!
//! So `sql` snapshots, unconditionally, including after a `SELECT` (where the page diff is empty and
//! the cost is one pass over the database file). "Only snapshot if it was a write" needs FlockDB to
//! parse SQL well enough to know what a write *is* — including `CREATE TABLE ... AS SELECT`, macros,
//! and whatever DuckDB adds next release. Being wrong about that once costs a user their data, and
//! being right about it costs them nothing they can perceive. So we do not guess.

use crate::error::Result;
use crate::render;
use crate::workspace::Workspace;
use flock_core::arrow::array::Int64Array;
use flock_core::{ArrowStream, Flock};

pub fn run(ws: &Workspace, branch: Option<&str>, query: &str) -> Result<()> {
    let branch = ws.resolve(branch)?;
    let mut db = Flock::open(ws.root(), &branch)?;

    let rows = db.query(query)?;

    // Commit BEFORE printing. If the commit fails, the user must not have already read a table of
    // results that are not going to be there next time — the last thing they see should be the
    // error, not the rows.
    db.snapshot()?;

    println!("{}", render::table(&rows)?);
    Ok(())
}

/// The first cell of a result, as an i64 — for `count(*)`, and for nothing else.
///
/// Returns `None` rather than panicking on an empty or wrongly-typed result. A helper that
/// `unwrap()`s the shape of a query someone else wrote is a helper that turns a typo into a crash.
pub fn first_i64(rows: &ArrowStream) -> Option<i64> {
    let batch = rows.batches().first()?;
    let column = batch.columns().first()?;
    let values = column.as_any().downcast_ref::<Int64Array>()?;
    values.values().first().copied()
}
