//! Shared helpers. Compiled into each test binary, so some helpers are unused in some of them.
#![allow(dead_code)]

use flock_core::arrow::array::{Array, Int32Array, Int64Array, StringArray};
use flock_core::ArrowStream;

/// Pull a single `BIGINT` out of a one-row, one-column result.
///
/// Panics on any surprise, which is correct: in a test, "the query did not return what I said it
/// would" is a failed assertion, not a condition to handle. (CLAUDE.md rule 6 bans panics in
/// *library* code; an assertion is what a test is made of.)
pub fn scalar_i64(rows: &ArrowStream) -> i64 {
    let batch = rows
        .batches()
        .first()
        .expect("query returned no batches at all");
    assert_eq!(batch.num_rows(), 1, "expected exactly one row");

    // `count(*)` is BIGINT but `max(id)` over an INTEGER column is INTEGER — DuckDB does not widen
    // a column just because the test would find it convenient, and it is right not to. Accept both
    // rather than making every test declare BIGINT columns that no real schema has.
    let col = batch.column(0);
    if let Some(a) = col.as_any().downcast_ref::<Int64Array>() {
        a.value(0)
    } else if let Some(a) = col.as_any().downcast_ref::<Int32Array>() {
        i64::from(a.value(0))
    } else {
        panic!("column 0 is {:?}, not an integer", col.data_type());
    }
}

/// Every value of a single integer column, in order.
///
/// Accepts `INTEGER` (Arrow `Int32`) as well as `BIGINT` (`Int64`), because DuckDB — quite
/// correctly — does not widen a column just because the test would find it convenient. A helper
/// that only handled `Int64` would make every test declare `BIGINT` columns, which is not what
/// anyone's schema looks like.
pub fn column_i64(rows: &ArrowStream) -> Vec<i64> {
    rows.batches()
        .iter()
        .flat_map(|b| {
            let col = b.column(0);
            if let Some(a) = col.as_any().downcast_ref::<Int64Array>() {
                (0..a.len()).map(|i| a.value(i)).collect::<Vec<_>>()
            } else if let Some(a) = col.as_any().downcast_ref::<Int32Array>() {
                (0..a.len())
                    .map(|i| i64::from(a.value(i)))
                    .collect::<Vec<_>>()
            } else {
                panic!("column 0 is {:?}, not an integer", col.data_type());
            }
        })
        .collect()
}

/// Every value of a single `VARCHAR` column, in order.
pub fn column_str(rows: &ArrowStream) -> Vec<String> {
    rows.batches()
        .iter()
        .flat_map(|b| {
            let col = b
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("column 0 is not a VARCHAR");
            (0..col.len())
                .map(|i| col.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}
