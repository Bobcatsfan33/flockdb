//! # flock-core
//!
//! A DuckDB you can fork in a millisecond and snapshot for free.
//!
//! ```no_run
//! use flock_core::Flock;
//!
//! # fn main() -> Result<(), flock_core::FlockError> {
//! let mut sales = Flock::open("/var/lib/flock/tenants", "acme")?;
//! sales.execute("CREATE TABLE t AS SELECT * FROM range(1000) AS r(id)")?;
//! sales.snapshot()?;
//!
//! let mut experiment = sales.fork("what-if")?;          // O(1) in storage. No bytes copied.
//! experiment.execute("DELETE FROM t WHERE id > 500")?;
//!
//! // Two databases now. The base never noticed.
//! let base = sales.query("SELECT count(*) FROM t")?;
//! let fork = experiment.query("SELECT count(*) FROM t")?;
//! # let _ = (base, fork);
//! # Ok(())
//! # }
//! ```
//!
//! # What this is
//!
//! The engine half of FlockDB (docs/02 §2): a library that gives you a real, isolated, forkable
//! analytical database *in-process*, on top of substrate's content-addressed page store. The fleet
//! plane that manages ten thousand of them is a separate, later thing; this is the one database.
//!
//! FlockDB does not implement SQL — it hosts DuckDB (`flock-kernel`) and adds **storage
//! semantics** underneath it: fork, snapshot, restore, sleep, and an export that hands your data
//! back with no strings attached.
//!
//! # What is real in F1, and what is not
//!
//! We would rather you read this here than find it out in a POC. docs/04 §5: *"we will say where we
//! are weak."*
//!
//! | Claim | Status in F1 |
//! | --- | --- |
//! | Fork isolation at the SQL level | **Real, and structural.** Tested bluntly. See [`Db::fork`]. |
//! | Snapshot / restore round-trip | **Real.** O(1) in substrate. |
//! | `export_duckdb` writes a vanilla file | **Real.** Tested every commit against a stock DuckDB. |
//! | TPC-H SF0.1 under 15 % overhead | **Measured: +0.3 % / +1.1 % / −4.0 %** over three paired runs — i.e. indistinguishable from zero. See the README. |
//! | A snapshot is durable across a crash | **Real.** The commit point is an fsync'd WAL record. |
//! | *Writes between snapshots* are durable | **NO.** They live in a DuckDB scratch file. A crash takes them. |
//! | `fork` is O(1) end to end | **NO.** O(1) in substrate; O(database) in the kernel, which must hydrate a file for DuckDB. |
//! | `query` streams results | **NO.** F1 materialises them. The type is called [`ArrowStream`] because the API is frozen; the laziness is F2. |
//! | `sleep` puts a database in object storage | **Real.** Pages and the manifest's whole ancestry go to the bucket; [`Flock::wake`] brings it back into an empty directory. Tested by deleting the entire pool between the two. |
//! | Wake is *lazy* — fetch only the pages a query reads | **NO**, and it cannot be in F1. DuckDB needs a file, so waking reconstructs the whole database and therefore downloads **all of it**. See [`Flock::wake`]. |
//! | Wake from S3 in < 250 ms (docs/02 §7) | **NOT MEASURED.** No S3-compatible endpoint was reachable to measure against — not Docker/MinIO, not a bucket. Not "measured and passing": *not measured*. The in-process figure in the README excludes the network and is a floor, not the number. |
//!
//! Every one of those "NO"s has the same root cause, and it is worth naming once: **DuckDB will
//! not let us give it a filesystem.** The `flock-kernel` crate docs carry the full investigation —
//! what we looked for, what we found in `duckdb-rs` and in DuckDB's C API, and what the fallback
//! costs, with numbers.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(rust_2018_idioms)]
// Tests are the one place a panic is the correct response to the impossible: a failing assertion
// *should* stop the run. CLAUDE.md rule 6 bans panics in library code, not in the assertions that
// prove the library is right.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod db;
mod error;
mod pool;
mod replica;
mod store;

pub use db::{Db, Flock};
pub use error::{FlockError, Result};
pub use replica::ReadReplica;

// Re-exported so a caller can drive replication — build a `Db::wal_source`, hand `Shipment`s to a
// `ReadReplica` — without separately depending on `flock-sync` and pinning the same versions.
pub use flock_sync::{self, Shipment, SyncError, WalSource};

// Re-exported so a caller can name a snapshot's type, and the Arrow types a query returns, without
// separately depending on substrate or on the exact `arrow` version DuckDB was built against. A
// duplicate `arrow` in the dependency graph produces a type error that names the same type twice.
pub use flock_kernel::{arrow, ArrowStream, KernelOpts};
pub use substrate_pager::ManifestId;

// Sleep and wake speak in substrate's own types, deliberately. A `WakeToken` is what a fleet
// registry stores — a pool, a manifest id, and a page size, and nothing else — and wrapping it in a
// FlockDB newtype would buy nothing and cost the caller the ability to hand it to any other tool
// built on the same engine. `RemoteTier` is likewise just "a bucket, and which pool it holds".
//
// `StoreError` comes with them, because `WakeToken::to_json` returns one: a caller that can build a
// token but cannot name the error that building it produces is a caller that has to `unwrap()`.
pub use substrate_store::{RemoteTier, StoreError, WakeToken};

/// The object-storage backends `RemoteTier` accepts (S3, GCS, Azure, in-memory, local filesystem).
///
/// Re-exported so callers can build one without separately depending on `object_store` and pinning
/// the exact same version — a duplicate in the dependency graph produces a type error that names the
/// same type twice and explains nothing.
pub use object_store;
