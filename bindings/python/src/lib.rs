//! `import flockdb` — FlockDB from Python.
//!
//! ```python
//! import flockdb
//!
//! db = flockdb.open("./warehouse")                    # a pool, and a branch called "main"
//! db.sql("CREATE TABLE t AS SELECT * FROM 'sales.parquet'")
//!
//! what_if = db.branch("what-if")                      # no bytes copied
//! what_if.sql("DELETE FROM t WHERE region = 'EMEA'")
//!
//! print(db.sql("SELECT count(*) FROM t"))             # the parent never noticed
//! ```
//!
//! # Why the surface is this small
//!
//! `open`, `sql`, `branch`, `checkout`, `branches`, `snapshot`, `export_duckdb`. That is the whole
//! API, and the omissions are deliberate: anything DuckDB can already do — types, functions,
//! `COPY`, extensions — you do in SQL, because a second, worse Python wrapper around DuckDB's SQL is
//! not what FlockDB is for. What FlockDB adds is the four verbs underneath the SQL, and those are
//! the ones that are here.
//!
//! # pyarrow and pandas are optional, and that is why they are not imported here
//!
//! [`Rows`] is printable, sized, and knows its columns with no third-party package at all. Ask it
//! for `.to_arrow()` or `.df()` and it imports pyarrow (and pandas) *at that moment*, and tells you
//! how to install them if they are not there. A binding that cannot be imported on a machine without
//! pandas is a binding that fails at deploy time in a container someone else built.
//!
//! # Every write commits
//!
//! `sql()` takes a snapshot before it returns, for the same reason the CLI does: a Python process
//! that exits without one loses the write in DuckDB's scratch file. `snapshot()` is still exposed
//! because it hands back the id — but you never have to *remember* to call it.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow::ipc::writer::StreamWriter;
use arrow::util::pretty::pretty_format_batches_with_schema;
use flock_core::{Flock, FlockError};
use pyo3::exceptions::{PyImportError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::path::PathBuf;
use std::sync::Mutex;

// FlockDB's error, as a Python exception.
//
// The message is the engine's own, and the engine's messages name the next thing to do — see
// `flock_core::FlockError`. Flattening them into a bare `RuntimeError("failed")` would throw away
// the only part of an error that a user (or an agent driving this API in a loop) can act on.
pyo3::create_exception!(flockdb, FlockDbError, PyRuntimeError);

fn to_py(e: FlockError) -> PyErr {
    FlockDbError::new_err(e.to_string())
}

/// One branch of one database.
///
/// The handle owns a DuckDB connection, so it is wrapped in a mutex: two Python threads may hold a
/// reference to the same `Db`, and DuckDB's connection is not safe to use from two at once. The
/// mutex makes the second thread wait rather than corrupt.
#[pyclass(module = "flockdb")]
pub struct Db {
    inner: Mutex<flock_core::Db>,
    root: PathBuf,
}

#[pymethods]
impl Db {
    /// The branch this handle is on.
    #[getter]
    fn name(&self) -> String {
        self.lock().name().to_string()
    }

    /// The pool this branch lives in.
    #[getter]
    fn pool(&self) -> PathBuf {
        self.root.clone()
    }

    /// Run SQL. Returns [`Rows`], and takes a snapshot before it returns.
    fn sql(&self, py: Python<'_>, query: &str) -> PyResult<Rows> {
        // `allow_threads`: a query is a long, blocking, CPU-bound call into DuckDB, and holding the
        // GIL across it would freeze every other Python thread in the process for its duration.
        py.detach(|| {
            let mut db = self.lock();
            let rows = db.query(query).map_err(to_py)?;
            db.snapshot().map_err(to_py)?;
            Ok(Rows {
                schema: rows.schema().clone(),
                batches: rows.into_batches(),
            })
        })
    }

    /// Fork this branch. No bytes are copied, and a write to the fork is never visible here.
    fn branch(&self, py: Python<'_>, name: &str) -> PyResult<Db> {
        py.detach(|| {
            let forked = self.lock().fork(name).map_err(to_py)?;
            Ok(Db {
                inner: Mutex::new(forked),
                root: self.root.clone(),
            })
        })
    }

    /// Open another branch of the same pool. Returns a new handle; this one is unaffected.
    ///
    /// Unlike the CLI's `flock checkout`, this changes no state on disk — a *library* whose
    /// behaviour depends on a hidden "current branch" file is a library that surprises the second
    /// process to use it.
    fn checkout(&self, py: Python<'_>, name: &str) -> PyResult<Db> {
        let root = self.root.clone();
        py.detach(move || open_at(root, name))
    }

    /// Every branch in this pool.
    fn branches(&self) -> PyResult<Vec<String>> {
        let dir = self.root.join("dbs");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(FlockDbError::new_err(format!(
                    "could not list the branches in {}: {e}",
                    dir.display()
                )))
            }
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Take a snapshot, and return its id: 32 bytes of hash that *are* this database, right now.
    ///
    /// Hand it back to `restore()` at any point in the future and you are here again.
    fn snapshot(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| self.lock().snapshot().map(|id| id.to_hex()).map_err(to_py))
    }

    /// Go back to a snapshot. Everything written since is discarded.
    fn restore(&self, py: Python<'_>, snapshot: &str) -> PyResult<()> {
        let id = parse_manifest(snapshot)?;
        py.detach(|| self.lock().restore(id).map_err(to_py))
    }

    /// Write a plain `.duckdb` file with nothing of ours in it. The escape hatch.
    ///
    /// Open it with the `duckdb` CLI, with `duckdb.connect()`, with a BI tool. There is no import
    /// step, because there is nothing to import.
    fn export_duckdb(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        py.detach(|| self.lock().export_duckdb(&path).map_err(to_py))
    }

    fn __repr__(&self) -> String {
        let db = self.lock();
        format!(
            "<flockdb.Db branch={:?} pool={:?}>",
            db.name(),
            self.root.display().to_string()
        )
    }
}

impl Db {
    /// A poisoned mutex means another thread panicked *while holding a DuckDB connection*. The
    /// connection itself is fine — DuckDB's state does not depend on our Rust stack — so recovering
    /// the guard is correct, and it is certainly better than a `panic!` inside a Python extension,
    /// which takes the interpreter with it.
    fn lock(&self) -> std::sync::MutexGuard<'_, flock_core::Db> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn parse_manifest(hex: &str) -> PyResult<flock_core::ManifestId> {
    flock_core::ManifestId::from_hex(hex).map_err(|e| {
        FlockDbError::new_err(format!(
            "{hex:?} is not a snapshot id: {e}\n\
             A snapshot id is the hex string that `snapshot()` returned."
        ))
    })
}

/// The rows a query returned. Printable without pyarrow; convertible with it.
#[pyclass(module = "flockdb")]
pub struct Rows {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
}

#[pymethods]
impl Rows {
    /// The column names, in order. Available even when the query matched nothing.
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    fn __len__(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    fn __repr__(&self) -> PyResult<String> {
        if self.schema.fields().is_empty() {
            return Ok("OK".to_string());
        }
        pretty_format_batches_with_schema(self.schema.clone(), &self.batches)
            .map(|t| t.to_string())
            .map_err(|e| FlockDbError::new_err(format!("could not format the result: {e}")))
    }

    fn __str__(&self) -> PyResult<String> {
        self.__repr__()
    }

    /// The result as Arrow IPC stream bytes. The zero-dependency way out of this object.
    fn to_ipc<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut buf, &self.schema)
                .map_err(|e| FlockDbError::new_err(format!("could not encode Arrow IPC: {e}")))?;
            for batch in &self.batches {
                writer.write(batch).map_err(|e| {
                    FlockDbError::new_err(format!("could not encode Arrow IPC: {e}"))
                })?;
            }
            writer
                .finish()
                .map_err(|e| FlockDbError::new_err(format!("could not encode Arrow IPC: {e}")))?;
        }
        Ok(PyBytes::new(py, &buf))
    }

    /// A `pyarrow.Table`.
    ///
    /// pyarrow is imported here and not at module load, so that `import flockdb` works on a machine
    /// that does not have it.
    fn to_arrow<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ipc = py.import("pyarrow.ipc").map_err(|_| {
            PyImportError::new_err(
                "to_arrow() needs pyarrow, which is not installed.\n  pip install pyarrow\n\
                 Or use to_ipc(), which returns Arrow IPC bytes and needs nothing.",
            )
        })?;
        let pa = py.import("pyarrow").map_err(|_| {
            PyImportError::new_err("to_arrow() needs pyarrow.\n  pip install pyarrow")
        })?;

        let buffer = pa.call_method1("py_buffer", (self.to_ipc(py)?,))?;
        let reader = ipc.call_method1("open_stream", (buffer,))?;
        reader.call_method0("read_all")
    }

    /// A `pandas.DataFrame`. Needs pyarrow and pandas.
    fn df<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("pandas").map_err(|_| {
            PyImportError::new_err(
                "df() needs pandas (and pyarrow), which are not installed.\n  \
                 pip install pandas pyarrow",
            )
        })?;
        self.to_arrow(py)?.call_method0("to_pandas")
    }
}

/// Open (or create) a database.
///
/// `path` is a **pool**: a directory that can hold many branches, all sharing one copy of every page
/// they have in common. It is created if it is not there — there is nothing to provision, which is
/// the whole point.
#[pyfunction]
#[pyo3(signature = (path, branch = "main"))]
fn open(py: Python<'_>, path: PathBuf, branch: &str) -> PyResult<Db> {
    let branch = branch.to_string();
    py.detach(move || open_at(path, &branch))
}

fn open_at(root: PathBuf, branch: &str) -> PyResult<Db> {
    let db = Flock::open(&root, branch).map_err(to_py)?;
    Ok(Db {
        inner: Mutex::new(db),
        root,
    })
}

#[pymodule]
fn flockdb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("FlockDbError", m.py().get_type::<FlockDbError>())?;
    m.add_class::<Db>()?;
    m.add_class::<Rows>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    Ok(())
}
