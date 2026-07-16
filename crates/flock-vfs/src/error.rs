//! One error enum for the crate (CLAUDE.md rule 6), and every message names the next thing to type.
//!
//! This crate is the read boundary DuckDB's file reads cross to become substrate page faults. When it
//! fails it fails during a query, so each variant says both *what* is wrong with the store or the
//! request and *what to do about it* — because the alternative to a clear error here is the thing this
//! whole crate exists to forbid: reading the wrong bytes, or reading past a page and returning
//! uninitialised memory. A refusal with a reason is always correct; a plausible-looking wrong answer
//! never is.

/// The result type for this crate.
pub type Result<T> = std::result::Result<T, VfsError>;

/// Everything that can go wrong translating a `(offset, length)` file read into substrate page faults.
///
/// `#[non_exhaustive]`: new variants arrive in minor versions. Match with a `_` arm, or a release that
/// cannot possibly affect you will still stop your build.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VfsError {
    /// The store reported a page size of zero, which cannot address any byte.
    ///
    /// This is a corrupt or mis-constructed store, never a bad request. There is no page layout with a
    /// zero-byte page, so the read path refuses rather than divide by it.
    #[error(
        "page store reported a page size of 0, which cannot map file offsets to pages. \
         The store is corrupt or was opened with the wrong `WakeToken` — re-check the token's \
         `page_size` against the one the database was sealed with."
    )]
    ZeroPageSize,

    /// A read offset plus its progress overflowed `u64` before reaching the requested length.
    ///
    /// Only reachable from a malformed request (an offset near `u64::MAX`). Refused rather than wrapped,
    /// because a wrapped offset would address the wrong page.
    #[error(
        "read offset {offset} is too large to address — offset + length overflows a 64-bit file \
         position. Clamp reads to the file length reported by `flock_vfs_len` before calling."
    )]
    OffsetOverflow {
        /// The offending file offset.
        offset: u64,
    },

    /// A page came back larger than the store's declared page size.
    ///
    /// A page must be exactly `page_size` bytes (the final page of the file may be shorter). A page
    /// *longer* than that means the store handed back bytes that do not belong to this logical page, so
    /// the read path refuses it rather than risk serving another page's data.
    #[error(
        "page {page_no} came back with {page_len} bytes, larger than the store's page size of \
         {page_size}. The store is corrupt (a page must be exactly one page_size, or shorter if it is \
         the last page) — run substrate's integrity scrub (`TieredStore::repair`) on this pool."
    )]
    OversizePage {
        /// The logical page number.
        page_no: u64,
        /// How many bytes the store returned for it.
        page_len: usize,
        /// The store's declared page size.
        page_size: usize,
    },

    /// The read needed a byte inside a page, but the page was too short to contain that byte.
    ///
    /// The file's declared length said this offset exists; the page backing it did not have it. That is
    /// a disagreement between the file length and the pages — a corrupt store or a wrong `total_len` —
    /// and the read path refuses rather than return whatever bytes happen to be past the page.
    #[error(
        "read of page {page_no} needs byte {needed_offset} within it, but the page is only {page_len} \
         bytes. The file length disagrees with the pages behind it — the `WakeToken`/`total_len` is \
         wrong for this manifest, or the store is corrupt. Re-derive `total_len` from the sleep record, \
         and run `TieredStore::repair` if it persists."
    )]
    ShortPage {
        /// The logical page number.
        page_no: u64,
        /// The byte offset within the page the read needed.
        needed_offset: usize,
        /// How many bytes the page actually held.
        page_len: usize,
    },

    /// Faulting a page from substrate failed — a cache miss that could not be served from the tier.
    ///
    /// This is substrate's account of a real fault failure (object storage unreachable, a hash
    /// mismatch, a lost page). It is wrapped verbatim so its corrective action survives.
    #[error("faulting page {page_no} from the tier failed: {source}")]
    Fault {
        /// The logical page number that could not be faulted.
        page_no: u64,
        /// Substrate's account of it.
        source: substrate_pager::PagerError,
    },

    /// Waking the database from its `WakeToken` failed — the store could not be opened.
    ///
    /// Reachable only from `flock_vfs_open`, never from a read: if the wake fails there is nothing to
    /// read from. Wrapped verbatim so substrate's corrective action survives.
    #[error("waking the database failed: {source}")]
    Wake {
        /// Substrate's account of it.
        source: substrate_store::StoreError,
    },

    /// The async runtime the tiered store needs could not be started.
    ///
    /// Reachable only from `flock_vfs_open`. Substrate's object-storage fetch is async and requires a
    /// multi-threaded runtime (a current-thread one deadlocks on a cache-miss fault), so if the runtime
    /// will not build there is nothing to read from.
    #[error(
        "could not start the async runtime the tiered store needs: {detail}. This is an OS resource \
         limit, not a database fault — check the process's thread and file-descriptor ulimits."
    )]
    Runtime {
        /// What went wrong building the runtime.
        detail: String,
    },

    /// The object-storage tier could not be constructed from the paths/environment it was given.
    ///
    /// Reachable only from `flock_vfs_open` (via [`crate::backend::remote_tier`]): the local tier
    /// directory could not be opened, or — under `--features s3` — the S3 endpoint/credentials were
    /// malformed. It is a configuration fault, not a read fault; there is nothing to read from yet.
    #[error(
        "could not construct the object-storage tier: {detail}. This is a wake-time configuration \
         error, not a corrupt database."
    )]
    TierConfig {
        /// What was wrong with the tier configuration and how to fix it.
        detail: String,
    },

    /// A value handed across the C ABI was malformed — a null pointer, non-UTF-8 string, or bad hex.
    ///
    /// Reachable only from the FFI boundary, where the caller is C++ we do not control. Every such value
    /// is validated before use, because trusting it is exactly the remote-code-execution surface this
    /// crate is fuzzed to close.
    #[error(
        "the C caller passed a malformed {what}. This is a bug in the DuckDB extension linking \
         flock-vfs, not in the database — {detail}."
    )]
    BadFfiArgument {
        /// Which argument was malformed (e.g. "manifest id", "cache directory path").
        what: &'static str,
        /// What specifically was wrong and how to fix it.
        detail: String,
    },
}
