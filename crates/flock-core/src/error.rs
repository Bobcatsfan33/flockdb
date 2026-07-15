//! One error enum for the crate (CLAUDE.md rule 6), and every message names the next thing to type.
//!
//! The person reading this is often mid-incident. `ERR_NO_SNAPSHOT` tells them we noticed
//! something. *"…run `SHOW TABLES`"* tells them what to do. Only the second one is worth printing.

use std::path::PathBuf;
use substrate_pager::ManifestId;

/// The result type for this crate.
pub type Result<T> = std::result::Result<T, FlockError>;

/// Everything that can go wrong in FlockDB's public API.
///
/// `#[non_exhaustive]`: new variants arrive in minor versions. Match with a `_` arm, or a release
/// that cannot possibly affect you will still stop your build.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FlockError {
    /// The SQL engine said no. Usually a SQL error, and usually the caller's to fix.
    #[error(transparent)]
    Kernel(#[from] flock_kernel::KernelError),

    /// The page store said no.
    #[error("page store failed during {op}: {source}")]
    Pager {
        /// What FlockDB was doing.
        op: &'static str,
        /// Substrate's account of it.
        source: substrate_pager::PagerError,
    },

    /// The write-ahead log said no — which, on a commit, means the commit did not happen.
    ///
    /// That is the *correct* outcome of a failed commit and not a disaster: the WAL record is the
    /// commit point (docs/02 §3.1), so a commit that could not write one has not occurred, and
    /// what is on disk is the previous snapshot, whole.
    #[error(
        "write-ahead log failed during {op}: {source}\n\
         If this happened during `snapshot()`, the snapshot did NOT happen — the WAL record IS \
         the commit. The database is intact as of the previous snapshot. Check free space and \
         permissions under the pool's `wal/` directory, then retry."
    )]
    Wal {
        /// What FlockDB was doing.
        op: &'static str,
        /// Substrate's account of it.
        source: substrate_wal::WalError,
    },

    /// A database name that cannot safely be a directory.
    ///
    /// Names become directory components under the pool root. A name containing `/` or `..` would
    /// put a database's write-ahead log outside its own pool — and pools are a **security
    /// boundary**, not a namespace (docs/02 §9.1: two stores in different pools never share a
    /// page, so that data cannot cross a classification boundary through the storage layer). A
    /// name that escapes its pool directory is a hole straight through that guarantee, so it is
    /// rejected at the door.
    #[error(
        "database name {name:?} is not usable: {reason}\n\
         Use letters, digits, '-', '_', and '.' — and not '.' or '..' alone. Names become \
         directories inside the pool, and a pool is a security boundary, so a name that could \
         escape it is refused rather than sanitised into something you did not ask for."
    )]
    BadName {
        /// The name we were given.
        name: String,
        /// Precisely what is wrong with it.
        reason: &'static str,
    },

    /// A fork was asked to land on a name that is already a database.
    ///
    /// We do not overwrite it and we do not adopt it. Adopting it would be worse than overwriting:
    /// the fork would come back holding *someone else's data* while reporting complete success.
    #[error(
        "cannot fork to {name:?}: a database of that name already exists in this pool ({path})\n\
         Pick a different name, or open the existing database with `Flock::open`. FlockDB will \
         neither overwrite it nor silently adopt it — a fork that returns another database's rows \
         and reports success is the worst failure this API can have."
    )]
    NameTaken {
        /// The name that was asked for.
        name: String,
        /// Where its state already lives.
        path: PathBuf,
    },

    /// `restore` was handed a manifest this pool has never seen.
    #[error(
        "snapshot {manifest} is not in this pool\n\
         `restore` only accepts a ManifestId that `snapshot()` returned from a database in this \
         same pool. Pools never share pages (docs/02 §9.1), so a snapshot from another pool \
         cannot be restored here even if you have its id — that is the boundary working, not a \
         bug. If the id is right, check you opened the pool you meant to."
    )]
    UnknownSnapshot {
        /// The id we could not resolve.
        manifest: ManifestId,
    },

    /// The pool directory could not be created or read.
    #[error(
        "pool I/O failed at {path} ({op}): {source}\n\
         Check that the path exists, is a directory, and is writable by this process."
    )]
    Pool {
        /// What FlockDB was doing.
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The OS's account of it.
        source: std::io::Error,
    },

    /// Object storage said no — during a [`sleep`](crate::Db::sleep) or a [`wake`](crate::Flock::wake).
    ///
    /// A failed `sleep` has **not** dropped anything: substrate uploads, verifies, and only then
    /// discards, so a database that could not be put to sleep is still awake and still whole.
    #[error(
        "object storage failed during {op}: {source}\n\
         Nothing has been discarded — `sleep` uploads and verifies BEFORE it drops anything, so the \
         database is intact. Check the bucket's credentials, region, and network reachability, then \
         retry.\n\
         If this happened on `wake`, also check that the WakeToken's pool matches the tier you \
         opened: pools are a security boundary (docs/02 §9.1) and substrate refuses to wake a \
         database into the wrong one."
    )]
    Tier {
        /// What FlockDB was doing.
        op: &'static str,
        /// Substrate's account of it.
        source: substrate_store::StoreError,
    },

    /// Replication said no — shipping a primary's WAL, or applying it on a replica.
    ///
    /// The source is boxed: `SyncError` carries several hex-string fields, and inlining it here would
    /// bloat `FlockError` — and therefore every `Result` in the crate, and the CLI's `CliError` on
    /// top of it — past the size at which `clippy::result_large_err` (rightly) complains. A box keeps
    /// the common, non-error `Ok` path small, which is the path that runs.
    #[error("replication failed during {op}: {source}")]
    Sync {
        /// What FlockDB was doing.
        op: &'static str,
        /// flock-sync's account of it.
        source: Box<flock_sync::SyncError>,
    },

    /// A thing this platform cannot do.
    #[error("{what} is not supported: {why}")]
    Unsupported {
        /// The operation.
        what: &'static str,
        /// Why not, and what the supported alternative is.
        why: &'static str,
    },
}

impl FlockError {
    pub(crate) fn pager(op: &'static str) -> impl Fn(substrate_pager::PagerError) -> FlockError {
        move |source| FlockError::Pager { op, source }
    }

    pub(crate) fn wal(op: &'static str) -> impl Fn(substrate_wal::WalError) -> FlockError {
        move |source| FlockError::Wal { op, source }
    }

    pub(crate) fn tier(op: &'static str) -> impl Fn(substrate_store::StoreError) -> FlockError {
        move |source| FlockError::Tier { op, source }
    }

    pub(crate) fn sync(op: &'static str) -> impl Fn(flock_sync::SyncError) -> FlockError {
        move |source| FlockError::Sync {
            op,
            source: Box::new(source),
        }
    }

    pub(crate) fn pool(
        op: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> FlockError {
        let path = path.into();
        move |source| FlockError::Pool { op, path, source }
    }
}
