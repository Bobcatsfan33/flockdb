//! One error enum for the crate (CLAUDE.md rule 6).
//!
//! # Why every message here names a corrective action
//!
//! The person reading a storage-engine error is very often a person under pressure at an
//! unreasonable hour, and what they need is not a diagnosis of our internals — it is the next
//! thing to type. `ERR_NO_TABLE` tells them we noticed. *"table 'foo' does not exist. Run
//! `SHOW TABLES` to list what is there."* tells them what to do.
//!
//! So the rule for this file: **if a variant cannot name a next step, it is not finished.**

use std::path::PathBuf;

/// The result type for this crate.
pub type Result<T> = std::result::Result<T, KernelError>;

/// Everything that can go wrong in the kernel.
///
/// `#[non_exhaustive]`: new variants will arrive in minor versions, and a consumer who matched
/// exhaustively would stop compiling for a change that cannot affect them. Match with a `_` arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KernelError {
    /// DuckDB refused a statement. Almost always a SQL error, and almost always the user's to fix.
    ///
    /// We pass DuckDB's own message through unedited. It is a good message — it names the column,
    /// the position, and usually the fix — and replacing it with one of ours would be strictly
    /// worse while looking more professional.
    #[error(
        "SQL failed: {source}\n\
         The statement was:\n  {sql}\n\
         If the object is missing, `SHOW TABLES` lists what this database actually contains."
    )]
    Sql {
        /// The statement we were asked to run.
        sql: String,
        /// DuckDB's own account of what was wrong with it.
        source: duckdb::Error,
    },

    /// DuckDB could not be opened over the temp file we hydrated for it.
    ///
    /// This is not a SQL error and it is not the user's fault. It means the hydration produced
    /// bytes DuckDB will not accept as a database — which, because pages are content-verified on
    /// every read, points at a bug in *our* page marshalling, not at a corrupt disk.
    #[error(
        "could not open DuckDB over the hydrated database at {path}: {source}\n\
         The page store verifies every page's hash on read, so this is unlikely to be disk \
         corruption — it is more likely a FlockDB bug. Please report it with this message, and \
         note that `Db::export_duckdb` on the last good snapshot will still get your data out."
    )]
    Open {
        /// Where the temp file was.
        path: PathBuf,
        /// DuckDB's complaint.
        source: duckdb::Error,
    },

    /// The scratch filesystem let us down: no space, no permission, or no temp directory.
    ///
    /// The kernel needs a real file because DuckDB needs a real file (see the crate docs). If it
    /// cannot have one, it cannot run at all, and saying so plainly beats failing later and
    /// mysteriously.
    #[error(
        "temp file I/O failed at {path} ({op}): {source}\n\
         DuckDB requires a real file on a real filesystem; FlockDB gives it a scratch one. Check \
         free space and permissions on the temp directory, or set TMPDIR to somewhere writable."
    )]
    Scratch {
        /// What we were doing.
        op: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The OS's account of it.
        source: std::io::Error,
    },

    /// The page store refused a read, a write, or a commit.
    #[error("page store failed during {op}: {source}")]
    Store {
        /// What the kernel was trying to do — hydrating, or checkpointing.
        op: &'static str,
        /// Substrate's account of it.
        source: substrate_pager::PagerError,
    },

    /// A manifest whose logical pages do not form a contiguous file.
    ///
    /// The kernel writes pages `0..n` with no gaps, so it cannot produce one of these. Seeing one
    /// means the manifest was written by something else — and reconstructing a file from it anyway
    /// would hand DuckDB a database with a hole silently punched through the middle of it, which
    /// DuckDB would either reject as corruption or, far worse, accept.
    #[error(
        "manifest {manifest} is not a FlockDB database: logical page {page_no} should begin at \
         byte {expected_offset} but only {actual_offset} bytes precede it.\n\
         FlockDB's kernel only ever writes a contiguous run of pages, so this manifest was not \
         written by it. Refusing to reconstruct a database with a hole in it. If you reached this \
         by passing a ManifestId from another tool into `Db::restore`, don't — restore only \
         accepts ids that `Db::snapshot` returned."
    )]
    CorruptLayout {
        /// The manifest we were asked to reconstruct.
        manifest: substrate_pager::ManifestId,
        /// The logical page that was not where it should have been.
        page_no: substrate_pager::LogicalPageNo,
        /// Where that page should have started.
        expected_offset: usize,
        /// Where it actually would have started.
        actual_offset: usize,
    },

    /// The export path is not somewhere we can write, or something is already there.
    ///
    /// We refuse to overwrite. An export is the thing a user reaches for when they are frightened
    /// about their data, and clobbering a file they may have meant to keep is a spectacularly bad
    /// time to be helpful.
    #[error(
        "cannot export to {path}: {reason}\n\
         Choose a path that does not exist yet — FlockDB will not overwrite a file during an \
         export, because an export is what people run when they are worried about their data and \
         that is the worst possible moment to destroy some."
    )]
    Export {
        /// Where we were asked to write.
        path: PathBuf,
        /// Why we would not.
        reason: String,
    },
}

impl KernelError {
    /// Wrap a substrate failure with the kernel operation that provoked it.
    pub(crate) fn store(op: &'static str) -> impl Fn(substrate_pager::PagerError) -> KernelError {
        move |source| KernelError::Store { op, source }
    }

    /// Wrap an I/O failure against the scratch file.
    pub(crate) fn scratch(
        op: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> KernelError {
        let path = path.into();
        move |source| KernelError::Scratch { op, path, source }
    }
}
