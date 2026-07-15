//! Errors, and the sentence that follows each one.
//!
//! A CLI's error message is its documentation, read at the worst possible moment. Every variant
//! here answers **"what do I type now"** — because the alternative is that the user answers it by
//! guessing, or by leaving.
//!
//! This is also written for the *other* reader. A coding agent driving `flock` in a loop has no
//! intuition and no README: the error text is its entire feedback channel, and "invalid input" ends
//! the loop while "table `trades` already exists — use `--table other`, or drop it with
//! `flock sql 'DROP TABLE trades'`" continues it.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, CliError>;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The engine said no. Its messages already carry their own corrective action.
    #[error(transparent)]
    Flock(#[from] flock_core::FlockError),

    #[error(
        "no FlockDB pool at {pool}\n\
         Create one by importing a file:  flock import <file.csv>\n\
         Or point at an existing pool:    flock --pool <dir> ...   (or set $FLOCK_POOL)"
    )]
    NoPool { pool: PathBuf },

    #[error(
        "branch {name:?} does not exist in this pool\n\
         Branches here: {}\n\
         Create it:     flock branch {name}",
        known_or_none(.known)
    )]
    UnknownBranch { name: String, known: Vec<String> },

    #[error(
        "branch {name:?} is asleep in object storage, so there is nothing local to query\n\
         Bring it back:  flock wake {name}"
    )]
    BranchAsleep { name: String },

    #[error(
        "branch {name:?} is not asleep — it is right here\n\
         `flock wake` only applies to a branch that `flock sleep` put in object storage. \
         Query this one directly:  flock sql --branch {name} \"SELECT 1\""
    )]
    BranchAwake { name: String },

    #[error(
        "cannot import {path}: FlockDB's importer does not know the extension {ext:?}\n\
         Supported: .csv, .tsv, .csv.gz, .tsv.gz, .parquet.\n\
         Anything else, load with SQL — DuckDB's readers are all there:\n\
           flock sql \"CREATE TABLE t AS SELECT * FROM read_json_auto('{path}')\""
    )]
    UnknownFormat { path: String, ext: String },

    #[error(
        "cannot import {path}: no such file\n\
         Check the path. `flock import` takes a path on this machine, not a URL."
    )]
    NoSuchFile { path: String },

    #[error(
        "{name:?} is not a usable SQL table name\n\
         Use letters, digits and underscores, starting with a letter or underscore. \
         `flock import` derives the table name from the file name; override it with \
         `--table <name>`."
    )]
    BadTableName { name: String },

    #[error(
        "table {table:?} already exists on branch {branch:?}\n\
         Import under a different name:  flock import <file> --table <name>\n\
         Or import onto a fresh branch:  flock branch <name> && flock import <file>\n\
         Or drop it:                     flock sql \"DROP TABLE {table}\""
    )]
    TableExists { table: String, branch: String },

    /// S3 is not faked. See the message — it is the whole point of the variant.
    #[error(
        "the `flock` CLI cannot tier to {uri:?}\n\
         `--tier` takes a **directory on this machine**. FlockDB's engine speaks to any \
         `object_store` backend — S3, GCS, Azure — and `flock_core::RemoteTier` will take one \
         today; the CLI ships only the filesystem backend, because we have no S3 endpoint to test \
         it against and a storage path that has never been run is not a feature, it is a rumour. \
         See README, \"What is real, and what is not\"."
    )]
    TierNotSupported { uri: String },

    #[error("{op} failed at {path}: {source}\n{hint}")]
    Io {
        op: &'static str,
        path: PathBuf,
        hint: &'static str,
        source: std::io::Error,
    },

    #[error(
        "the sleep record for branch {name:?} at {path} is not readable: {source}\n\
         It is JSON that `flock sleep` wrote, holding the WakeToken and the tier it went to. If it \
         is gone or corrupt, the pages are still in the tier but FlockDB no longer knows where — \
         restore the file, or import the data again."
    )]
    BadSleepRecord {
        name: String,
        path: PathBuf,
        source: serde_json::Error,
    },

    #[error(
        "the tier directory {path} could not be opened as an object store: {source}\n\
         `--tier` takes a directory. Check it exists and is writable."
    )]
    Tier {
        path: PathBuf,
        source: flock_core::object_store::Error,
    },

    #[error("object storage failed while trying to {op}: {source}")]
    Store {
        op: &'static str,
        source: flock_core::StoreError,
    },

    #[error("could not format the result: {0}")]
    Render(#[from] flock_core::arrow::error::ArrowError),
}

impl CliError {
    pub fn io(
        op: &'static str,
        path: impl Into<PathBuf>,
        hint: &'static str,
    ) -> impl FnOnce(std::io::Error) -> CliError {
        let path = path.into();
        move |source| CliError::Io {
            op,
            path,
            hint,
            source,
        }
    }
}

fn known_or_none(known: &[String]) -> String {
    if known.is_empty() {
        "(none — this pool has no branches yet)".to_string()
    } else {
        known.join(", ")
    }
}
