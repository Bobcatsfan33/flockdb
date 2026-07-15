//! Turning a result into something a person can read.
//!
//! Arrow's own pretty-printer, and not one of ours. It already knows how a `Decimal128(18,2)`, a
//! `Timestamp(us, "Europe/London")` and a NULL should look, and every one of those is a place to be
//! quietly wrong about someone's money.

use crate::error::Result;
use flock_core::arrow::util::pretty::pretty_format_batches_with_schema;
use flock_core::ArrowStream;

/// A result, as a bordered table.
///
/// A statement with no result columns at all — `SET`, `PRAGMA`, some DDL — prints `OK`. It is not
/// an empty table: an empty table means *the query ran and matched nothing*, which is a different
/// fact and one people act on.
pub fn table(rows: &ArrowStream) -> Result<String> {
    if rows.schema().fields().is_empty() {
        return Ok("OK".to_string());
    }
    // `_with_schema` and not the bare `pretty_format_batches`: a query that matched no rows still
    // has columns, and printing nothing at all for `SELECT * FROM t WHERE false` loses the one piece
    // of information that result carries.
    Ok(pretty_format_batches_with_schema(rows.schema().clone(), rows.batches())?.to_string())
}
