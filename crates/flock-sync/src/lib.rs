//! # flock-sync
//!
//! WAL shipping, read replicas, and point-in-time restore for FlockDB — built on **substrate's WAL**,
//! the same fsync'd, CRC-protected commit records that are FlockDB's commit point. A follower does
//! not re-derive a diff; it applies the durable commit records the primary already wrote, through
//! substrate's own commit protocol, and converges to the primary byte for byte.
//!
//! ```no_run
//! # fn main() -> Result<(), flock_sync::SyncError> {
//! use std::sync::Arc;
//! use flock_sync::{Replica, WalSource};
//! use substrate_pager::{std_vfs, StoreConfig};
//!
//! // A follower somewhere else. It is a real, crash-durable database of its own.
//! let mut replica = Replica::open(std_vfs(), "/var/lib/flock-replica/acme", StoreConfig::default())?;
//!
//! // The primary side: a view of the primary's committed history as shippable transactions.
//! // (flock-core hands you one of these from a local `Db` via `Db::wal_source`.)
//! let source: WalSource = unimplemented!("built from the primary's WAL dir + CAS");
//!
//! // Pull everything new and converge. Safe to call again and again to keep following.
//! let n = replica.catch_up(&source)?;
//! println!("applied {n} commits; replica head = {}", replica.head().to_hex());
//! # Ok(()) }
//! ```
//!
//! # The consistency model, stated so it cannot be oversold
//!
//! flock-sync gives you **asynchronous, log-shipped read replicas with manual failover.** Precisely:
//!
//! - **Eventually consistent reads.** A follower applies the primary's fsync'd commit records in
//!   order and lags the primary by whatever it has not yet pulled. A read on a follower is served
//!   from a *pinned manifest* — a whole, committed snapshot — so it is never half-applied, but it may
//!   be stale. A client that reads the primary then the follower can see the follower behind; a
//!   client reading only the follower always sees a self-consistent, monotonically advancing view.
//! - **Byte-identical at every applied commit.** When a follower has applied through primary LSN *N*,
//!   its manifest id equals the primary's manifest id at *N*. Content addressing makes that literal:
//!   equal ids mean equal bytes on every page. This is the load-bearing property, and `tests/oracle.rs`
//!   hammers it differentially across thousands of randomized runs.
//! - **Point-in-time restore is exact.** Replaying to LSN *N* lands on precisely the primary's
//!   manifest at *N*, reproducibly.
//! - **Writes stay on the primary.** Followers are read-only. Reads scale out; there is one writer.
//!
//! # Where it is weak — read this before a customer finds it (flockdb `CLAUDE.md` rule 6)
//!
//! - **No automatic failover.** If the primary dies, promoting a follower to primary is a *manual*
//!   operation you perform; flock-sync does not detect the failure or elect a new primary. F3 ships
//!   the mechanism (a follower is a fully-formed durable database, ready to be opened for writes),
//!   not the orchestration.
//! - **No split-brain protection.** Nothing here stops two processes from each believing they are the
//!   primary and both accepting writes. There is no lease, no fencing token, no quorum. If you
//!   promote a follower while the old primary is still up, you have two primaries and divergent
//!   histories, and flock-sync will not stop you — it will only *notice* afterwards
//!   ([`SyncError::FollowerUnknown`]/[`SyncError::Diverged`]) once you try to make one follow the
//!   other. Preventing it is a fencing layer above this crate, and it is not built yet.
//! - **A follower can fall arbitrarily behind, and that has a floor.** Catch-up re-reads the log from
//!   its start each time (substrate exposes no streaming reader), and if the primary checkpoints and
//!   truncates history a lagging follower still needs, that follower cannot catch up and must be
//!   re-seeded ([`SyncError::MissingPage`]). Bound follower lag relative to the primary's checkpoint
//!   cadence. There is no built-in backpressure from follower to primary yet.
//! - **Shipping is a pull, and transport is yours.** A `WalSource` reads a local WAL directory and a
//!   `Shipment` is a serializable message; moving shipments between machines (a socket, a queue, mTLS)
//!   is above this crate. F3 is the correctness core, not the network daemon.
//!
//! # No `async`
//!
//! Like substrate-pager and substrate-wal underneath it, this crate is synchronous. Applying a
//! shipped commit is deterministic replay, and deterministic replay is the one thing this whole
//! engine refuses to make non-deterministic (substrate `CLAUDE.md` rule 7).

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(rust_2018_idioms)]
// Tests are the one place a panic is the correct response to the impossible: a failing assertion
// *should* stop the run. The ban is on panics in library code, not in the assertions that prove it.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod clock;
mod error;
mod replica;
mod shipment;
mod source;

pub use clock::ReplayClock;
pub use error::{Result, SyncError};
pub use replica::{Applied, Replica};
pub use shipment::{PageBlob, PageWrite, Shipment};
pub use source::WalSource;

// Re-exported so a caller can name a shipment's coordinate and a replica's head without separately
// depending on substrate or pinning the same versions — a duplicate in the dependency graph produces
// a type error that names the same type twice and explains nothing.
pub use substrate_pager::ManifestId;
pub use substrate_wal::Lsn;
