//! One error enum for the crate (CLAUDE.md rule 6), and every message names the next thing to type.
//!
//! Replication fails in ways that are easy to misdiagnose — a follower that has quietly diverged
//! looks exactly like one that is merely behind — so each variant here says both *what* went wrong
//! and *what to do about it*, because the person reading it is deciding whether to promote a replica
//! during an incident.

use substrate_pager::LogicalPageNo;
use substrate_wal::Lsn;

/// The result type for this crate.
pub type Result<T> = std::result::Result<T, SyncError>;

/// Everything that can go wrong shipping, applying, or restoring from a WAL.
///
/// `#[non_exhaustive]`: new variants arrive in minor versions. Match with a `_` arm, or a release
/// that cannot possibly affect you will still stop your build.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The page store said no while staging or committing an applied record.
    #[error("page store failed during {op}: {source}")]
    Pager {
        /// What flock-sync was doing.
        op: &'static str,
        /// Substrate's account of it.
        source: substrate_pager::PagerError,
    },

    /// The write-ahead log said no — reading the primary's segments, or committing on the follower.
    #[error("write-ahead log failed during {op}: {source}")]
    Wal {
        /// What flock-sync was doing.
        op: &'static str,
        /// Substrate's account of it.
        source: substrate_wal::WalError,
    },

    /// A follower was handed a commit that does not build on the state it is currently at.
    ///
    /// This is the *good* kind of failure: it is caught before anything is applied. A follower must
    /// apply the primary's commits in order and with no gaps, because each commit is derived from
    /// the manifest before it. A shipment whose base is not the follower's head means the follower
    /// missed a commit, or is being fed another primary's log.
    #[error(
        "shipment at LSN {lsn} does not follow the replica's current state\n\
         The replica is at manifest {head}, but this commit builds on {expected_base}. Apply commits \
         in strict LSN order with no gaps — use `Replica::resume_after(head)` on the source to find \
         the exact LSN to stream from, and do not mix two primaries' logs into one follower."
    )]
    OutOfOrder {
        /// The commit LSN of the offending shipment.
        lsn: Lsn,
        /// The manifest the shipment expects the follower to be at (its base), in hex.
        expected_base: String,
        /// The manifest the follower is actually at, in hex.
        head: String,
    },

    /// A follower applied a commit and arrived at a *different* manifest than the primary did.
    ///
    /// Content addressing makes this detectable at all: the primary shipped the exact manifest id
    /// its commit produced, and the follower re-derived one that does not match. That is a real
    /// divergence — a different page hasher, a corrupted shipment, or a substrate version mismatch —
    /// and installing the follower's manifest anyway would be serving a wrong answer while reporting
    /// success. So we stop.
    #[error(
        "replaying LSN {lsn} produced a different database than the primary committed\n\
         Expected manifest {expected}, got {got}. The replica has NOT advanced. This means the \
         follower and primary disagree on how bytes hash — check they were built with the same \
         substrate version and the same page-hashing mode (keyed vs unkeyed) — or the shipment was \
         corrupted in transit. Re-seed this follower from a fresh copy of the primary rather than \
         trusting it."
    )]
    Diverged {
        /// The commit LSN at which replay diverged.
        lsn: Lsn,
        /// The manifest the primary committed, in hex.
        expected: String,
        /// The manifest the follower derived, in hex.
        got: String,
    },

    /// A shipment's bytes do not hash to the page id the shipment claims for them.
    ///
    /// The shipment carries page id → bytes pairs so the follower's content-addressed store can be
    /// seeded. If the bytes hash to something else, the shipment is corrupt and applying it would
    /// poison the follower's CAS with a page filed under the wrong name.
    #[error(
        "a shipped page for logical page {page_no} does not match its claimed content hash\n\
         Claimed {claimed}, computed {actual}. The shipment was corrupted in transit. Discard it and \
         re-fetch the commit from the primary."
    )]
    PageHashMismatch {
        /// The logical page whose bytes were wrong.
        page_no: LogicalPageNo,
        /// The page id the shipment claimed, in hex.
        claimed: String,
        /// The id the bytes actually hash to, in hex.
        actual: String,
    },

    /// A shipment references a page whose bytes are not in it (or not in the primary's CAS).
    #[error(
        "commit at LSN {lsn} references page {page_id} for logical page {page_no}, but its bytes are \
         not available\n\
         On the primary this means the page was garbage-collected out from under a follower that had \
         not yet caught up — a follower cannot be allowed to fall so far behind that GC reclaims the \
         history it still needs. Bound follower lag, or checkpoint less aggressively, and re-seed \
         this follower from a fresh primary copy."
    )]
    MissingPage {
        /// The commit that referenced the page.
        lsn: Lsn,
        /// The logical page being written.
        page_no: LogicalPageNo,
        /// The content hash whose bytes are missing, in hex.
        page_id: String,
    },

    /// A follower asked where to resume, but its head is nowhere in the primary's log.
    ///
    /// The primary re-derives the resume point by finding the follower's current manifest among its
    /// own committed manifests. If it is not there, the follower's history and the primary's have
    /// forked — different primaries, or a follower promoted and written to.
    #[error(
        "the replica's head {head} is not a commit in this primary's log\n\
         The two have forked: this follower did not come from this primary, or it was promoted and \
         written to and can no longer follow. Re-seed the follower from a fresh copy of the primary."
    )]
    FollowerUnknown {
        /// The follower head that could not be located, in hex.
        head: String,
    },

    /// Reading the primary's WAL directory failed.
    #[error(
        "could not read the primary's WAL at {path}: {source}\n\
         Check the path is the primary database's directory (the one holding `wal/`), and is \
         readable by this process."
    )]
    Io {
        /// The path involved.
        path: std::path::PathBuf,
        /// The OS's account of it.
        source: std::io::Error,
    },

    /// An operation that this store shape cannot support.
    #[error("{what} is not supported: {why}")]
    Unsupported {
        /// The operation.
        what: &'static str,
        /// Why not, and what the supported alternative is.
        why: &'static str,
    },
}

impl SyncError {
    pub(crate) fn pager(op: &'static str) -> impl Fn(substrate_pager::PagerError) -> SyncError {
        move |source| SyncError::Pager { op, source }
    }

    pub(crate) fn wal(op: &'static str) -> impl Fn(substrate_wal::WalError) -> SyncError {
        move |source| SyncError::Wal { op, source }
    }
}
