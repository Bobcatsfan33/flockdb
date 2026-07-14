//! Query results, as Arrow.
//!
//! Arrow and not a bespoke row format, because Arrow is the format the Python bindings, the CLI,
//! the fan-out service, and every BI tool downstream already speak (docs/02 §6.1). A row type of
//! our own would mean a conversion at every one of those hops, and each conversion is a place to
//! be subtly wrong about a timestamp.

use duckdb::arrow::array::RecordBatch;
use duckdb::arrow::datatypes::SchemaRef;

/// The rows a query produced.
///
/// # It is called a stream and in F1 it is not one, and here is why
///
/// This type collects every batch before it returns. It is named `ArrowStream` because that is the
/// API docs/02 §5.3 freezes and the API we intend to keep — but **F1 materialises the whole result
/// in memory**, and a `SELECT *` over a database larger than RAM will fail the way any such
/// program fails.
///
/// The reason is unglamorous. `duckdb-rs` hands back an iterator that borrows the `Statement` it
/// came from, which borrows the `Connection`. Returning it from a method that owns the connection
/// means a self-referential struct — `ouroboros`, or `unsafe`, or a lifetime on `SqlKernel` that
/// infects `flock-core`'s entire public API. CLAUDE.md rule 10 says take the obvious way and leave
/// a note saying what the clever way would have been.
///
/// This is the note. Real streaming is F2 work, it changes no signature here, and until it lands
/// this doc comment is the only thing standing between a user and an unpleasant surprise — which
/// is precisely why it says so instead of saying nothing.
#[derive(Debug, Clone)]
pub struct ArrowStream {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
}

impl ArrowStream {
    /// Build a stream from batches DuckDB has already produced.
    pub(crate) fn new(schema: SchemaRef, batches: Vec<RecordBatch>) -> Self {
        ArrowStream { schema, batches }
    }

    /// The Arrow schema of the result — available even when there are no rows.
    ///
    /// A query that matched nothing still has columns, and a caller building a table header should
    /// not have to guess at them from an empty `Vec`.
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// The batches, borrowed.
    pub fn batches(&self) -> &[RecordBatch] {
        &self.batches
    }

    /// The batches, owned — for handing straight to Arrow IPC, Polars, or pyarrow with no copy.
    pub fn into_batches(self) -> Vec<RecordBatch> {
        self.batches
    }

    /// How many rows the query returned, across all batches.
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    /// Whether the query returned no rows at all.
    pub fn is_empty(&self) -> bool {
        self.num_rows() == 0
    }
}

impl IntoIterator for ArrowStream {
    type Item = RecordBatch;
    type IntoIter = std::vec::IntoIter<RecordBatch>;

    fn into_iter(self) -> Self::IntoIter {
        self.batches.into_iter()
    }
}
