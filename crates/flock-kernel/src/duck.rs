//! [`DuckDbKernel`] — DuckDB, over a temp file, over a `PageStore`.
//!
//! The *why* of this arrangement, and everything it costs, is at the top of the crate. This file
//! is the mechanism.

use crate::error::{KernelError, Result};
use crate::paging;
use crate::stream::ArrowStream;
use crate::SqlKernel;
use duckdb::arrow::array::RecordBatch;
use duckdb::{Config, Connection};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use substrate_pager::{ManifestId, PageStore};
use tempfile::TempDir;

/// The scratch file's name is fixed, and that is load-bearing.
///
/// DuckDB names the default catalog after the file's stem, so a file called `flock.duckdb` gives a
/// catalog called `flock`. We do not rely on that — [`DuckDbKernel::export`] asks DuckDB for
/// `current_database()` rather than assuming — but a stable name means a stack trace, a `lsof`, or
/// a stray core file says *flock* rather than a random hex string, and at 3am that is worth
/// something.
const SCRATCH_FILE: &str = "flock.duckdb";

/// How a kernel is configured.
///
/// Deliberately tiny. Every knob here is a knob someone will turn wrongly, and the two that exist
/// are the two the benchmark needs in order to compare FlockDB against raw DuckDB *fairly* — a
/// comparison where one side gets eight threads and the other gets one is not a measurement, it is
/// a press release.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct KernelOpts {
    /// How many threads DuckDB may use. `None` leaves DuckDB's own default alone.
    pub threads: Option<i64>,
    /// DuckDB's memory limit, e.g. `"2GB"`. `None` leaves DuckDB's own default alone.
    pub max_memory: Option<String>,
}

impl KernelOpts {
    /// Pin DuckDB to `n` threads.
    pub fn threads(mut self, n: i64) -> Self {
        self.threads = Some(n);
        self
    }

    /// Cap DuckDB's memory, e.g. `"2GB"`.
    pub fn max_memory(mut self, limit: impl Into<String>) -> Self {
        self.max_memory = Some(limit.into());
        self
    }
}

/// DuckDB, hosted on a substrate [`PageStore`].
///
/// # Lifecycle
///
/// ```text
///   open()        pages ──► scratch file ──► DuckDB connection
///   query()/execute()      DuckDB ◄──► scratch file          (FlockDB is not in this path)
///   checkpoint()  DuckDB CHECKPOINT ──► scratch file ──► pages ──► ManifestId
///   export()      DuckDB ATTACH + COPY FROM DATABASE ──► a vanilla .duckdb file
///   drop          the scratch file and its TempDir are deleted
/// ```
///
/// The scratch file is not durable state — the `PageStore` holds the durable copy of every byte in
/// it, as of the last `checkpoint()`. Everything written since then lives *only* in the scratch
/// file, and a crash takes it with it. That boundary is the fallback's, not a defect in this file,
/// and it is spelled out in the crate docs.
pub struct DuckDbKernel {
    store: Arc<dyn PageStore>,
    conn: Connection,
    path: PathBuf,
    /// Held purely so the directory outlives the connection. Dropping it deletes the scratch file,
    /// which is exactly what we want and exactly what must not happen a moment early — hence the
    /// field, and hence this comment, because a reader will otherwise "clean up" an unused member
    /// and turn every query into a use-after-unlink.
    _scratch: TempDir,
}

impl DuckDbKernel {
    /// The scratch file DuckDB is actually using. Exposed for tests and diagnostics only.
    ///
    /// It is *not* a durable artifact and copying it is not a backup: use
    /// [`SqlKernel::export`](crate::SqlKernel::export), which produces a file with no dependency on
    /// us and which we test on every commit.
    pub fn scratch_path(&self) -> &Path {
        &self.path
    }

    /// Run several statements as a script. Not part of [`SqlKernel`] — a batch has no single row
    /// count to return, so it does not fit `execute`'s contract — but setup scripts and extension
    /// loads need it, and hand-splitting SQL on semicolons is a well-known way to break a string
    /// literal in half.
    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        self.conn
            .execute_batch(sql)
            .map_err(|source| KernelError::Sql {
                sql: sql.to_string(),
                source,
            })
    }

    /// Fold DuckDB's own write-ahead log into its main file, so that the main file alone is the
    /// whole database.
    ///
    /// This is the assumption the entire fallback rests on: after `CHECKPOINT`, everything DuckDB
    /// knows is in the one file we page up. If DuckDB were to leave committed data behind in a
    /// `.wal` sidecar, we would page up a database missing its most recent writes and never notice
    /// — a silent, unbounded data loss, and by a long way the most dangerous bug this design can
    /// have.
    ///
    /// So we do not merely assume it. We check, every time, and we refuse loudly if it is not true.
    /// The check costs one `stat` per checkpoint; the bug it forecloses costs a customer.
    fn fold_wal_into_main_file(&mut self) -> Result<()> {
        self.execute_batch("CHECKPOINT")?;

        let wal = self.path.with_extension("duckdb.wal");
        match std::fs::metadata(&wal) {
            // Gone, which is DuckDB's usual behaviour after a checkpoint.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(KernelError::Scratch {
                op: "stat DuckDB's WAL after CHECKPOINT",
                path: wal,
                source,
            }),
            // Present but empty: also fine — the checkpoint truncated it.
            Ok(m) if m.len() == 0 => Ok(()),
            Ok(m) => Err(KernelError::Scratch {
                op: "verify DuckDB's WAL is empty after CHECKPOINT",
                path: wal.clone(),
                source: std::io::Error::other(format!(
                    "DuckDB left {} bytes in {} after a CHECKPOINT. FlockDB pages up the main \
                     database file only, so committing now would durably lose whatever is in that \
                     WAL. Refusing. This is a FlockDB bug against this DuckDB version, not \
                     something you can fix from the outside — please report it. Your data is \
                     intact: `Db::export_duckdb` reads through the live DuckDB connection and \
                     will still write a complete file.",
                    m.len(),
                    wal.display()
                )),
            }),
        }
    }
}

impl SqlKernel for DuckDbKernel {
    fn open(store: Arc<dyn PageStore>, opts: KernelOpts) -> Result<Self> {
        let scratch =
            TempDir::new().map_err(KernelError::scratch("create scratch dir", "$TMPDIR"))?;
        let path = scratch.path().join(SCRATCH_FILE);

        // Pages → file. For a brand-new database this writes zero bytes, and DuckDB initialises an
        // empty database in the empty file, which is precisely the behaviour we want on `open` of a
        // name that does not exist yet.
        paging::hydrate(store.as_ref(), &store.head(), &path)?;

        let mut config = Config::default();
        if let Some(n) = opts.threads {
            config = config.threads(n).map_err(|source| KernelError::Open {
                path: path.clone(),
                source,
            })?;
        }
        if let Some(limit) = &opts.max_memory {
            config = config
                .max_memory(limit)
                .map_err(|source| KernelError::Open {
                    path: path.clone(),
                    source,
                })?;
        }

        let conn =
            Connection::open_with_flags(&path, config).map_err(|source| KernelError::Open {
                path: path.clone(),
                source,
            })?;

        Ok(DuckDbKernel {
            store,
            conn,
            path,
            _scratch: scratch,
        })
    }

    fn query(&mut self, sql: &str) -> Result<ArrowStream> {
        let mut stmt = self.conn.prepare(sql).map_err(|source| KernelError::Sql {
            sql: sql.to_string(),
            source,
        })?;

        let arrow = stmt.query_arrow([]).map_err(|source| KernelError::Sql {
            sql: sql.to_string(),
            source,
        })?;

        // The schema is taken *before* the batches are drained: an empty result still has columns,
        // and a caller drawing a table header should not have to guess them from an empty Vec.
        let schema = arrow.get_schema();
        let batches: Vec<RecordBatch> = arrow.collect();

        Ok(ArrowStream::new(schema, batches))
    }

    fn execute(&mut self, sql: &str) -> Result<u64> {
        self.conn
            .execute(sql, [])
            .map(|n| n as u64)
            .map_err(|source| KernelError::Sql {
                sql: sql.to_string(),
                source,
            })
    }

    fn checkpoint(&mut self) -> Result<ManifestId> {
        self.fold_wal_into_main_file()?;
        paging::persist(self.store.as_ref(), &self.path)
    }

    fn export(&mut self, path: &Path) -> Result<()> {
        // Refuse to overwrite. An export is what a person runs when they are worried about their
        // data; silently destroying a file at the destination is the worst imaginable way to be
        // helpful, and "it was in the docs" is not a defence.
        if path.exists() {
            return Err(KernelError::Export {
                path: path.to_path_buf(),
                reason: "a file already exists there".to_string(),
            });
        }

        // DuckDB writes the file, not us. That is what makes the result *vanilla by construction*
        // rather than vanilla because we were careful: there is no FlockDB code anywhere in the
        // path that produces those bytes, so there is no FlockDB bug that can make them
        // non-standard. `COPY FROM DATABASE` copies schema and data both.
        let target = sql_string_literal(&path.to_string_lossy());
        let source = self.current_database()?;

        // The source catalog name is quoted as an identifier because DuckDB will happily name a
        // catalog something that needs quoting, and a broken export is exactly the failure the
        // escape hatch cannot be allowed to have.
        let script = format!(
            "ATTACH {target} AS flock_export (TYPE DUCKDB); \
             COPY FROM DATABASE {} TO flock_export; \
             DETACH flock_export;",
            sql_identifier(&source),
        );

        self.execute_batch(&script)
            .map_err(|e| KernelError::Export {
                path: path.to_path_buf(),
                reason: format!("DuckDB refused to write it: {e}"),
            })
    }
}

impl DuckDbKernel {
    /// Ask DuckDB what it calls the catalog we are attached to, rather than deducing it from the
    /// filename. The deduction is *usually* right, and `export` is the one call in the product that
    /// is not allowed to be usually right.
    fn current_database(&self) -> Result<String> {
        self.conn
            .query_row("SELECT current_database()", [], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|source| KernelError::Sql {
                sql: "SELECT current_database()".to_string(),
                source,
            })
    }
}

/// Quote a value as a SQL string literal, doubling any embedded quote.
///
/// The export path comes from the caller and lands in a `format!`-built statement, which is the
/// classic shape of an injection. A path is allowed to contain an apostrophe — `/Users/o'brien/`
/// is a perfectly ordinary home directory — so this is a correctness fix before it is a security
/// fix, and it is both.
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote a value as a SQL identifier, doubling any embedded double-quote.
fn sql_identifier(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use substrate_pager::{Pager, StoreConfig};

    fn kernel() -> DuckDbKernel {
        let store = Arc::new(Pager::in_memory(StoreConfig::default()).unwrap());
        DuckDbKernel::open(store, KernelOpts::default()).unwrap()
    }

    #[test]
    fn a_fresh_kernel_opens_over_an_empty_store_and_answers_sql() {
        let mut k = kernel();
        let rows = k.query("SELECT 42 AS answer").unwrap();
        assert_eq!(rows.num_rows(), 1);
        assert_eq!(rows.schema().field(0).name(), "answer");
    }

    #[test]
    fn a_checkpoint_that_changes_nothing_returns_the_same_manifest() {
        let mut k = kernel();
        let a = k.checkpoint().unwrap();
        let b = k.checkpoint().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_checkpoint_after_a_write_returns_a_different_manifest() {
        let mut k = kernel();
        let before = k.checkpoint().unwrap();
        k.execute("CREATE TABLE t (i INTEGER)").unwrap();
        k.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        let after = k.checkpoint().unwrap();
        assert_ne!(before, after, "a write must move the manifest");
    }

    #[test]
    fn checkpoint_leaves_no_committed_data_behind_in_duckdbs_own_wal() {
        // The silent-data-loss guard from `fold_wal_into_main_file`, exercised directly: if a
        // future DuckDB stops truncating its WAL on CHECKPOINT, this fails here rather than in
        // production, quietly, six months later.
        let mut k = kernel();
        k.execute("CREATE TABLE t (i INTEGER)").unwrap();
        k.execute("INSERT INTO t SELECT * FROM range(10000)")
            .unwrap();
        k.checkpoint().unwrap();

        let wal = k.scratch_path().with_extension("duckdb.wal");
        let len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            len, 0,
            "DuckDB left data in its WAL that FlockDB would not have paged up"
        );
    }

    #[test]
    fn export_refuses_to_overwrite_an_existing_file() {
        let mut k = kernel();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taken.duckdb");
        std::fs::write(&path, b"someone else's data").unwrap();

        let err = k.export(&path).unwrap_err();
        assert!(matches!(err, KernelError::Export { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else's data");
    }

    #[test]
    fn a_path_containing_a_quote_is_escaped_and_not_injected() {
        assert_eq!(
            sql_string_literal("/Users/o'brien/db"),
            "'/Users/o''brien/db'"
        );
        assert_eq!(sql_identifier(r#"we"ird"#), r#""we""ird""#);
    }
}
