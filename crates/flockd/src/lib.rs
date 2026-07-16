//! **flockd — the wake scheduler for sleeping, tiered databases.**
//!
//! FlockDB's promise is that you can have ten thousand databases and wake the one a query needs
//! without paying for the other 9,999. `docs/wake-latency.md` proved the mechanism: a woken database
//! answers a selective query by faulting a small, **flat** set of pages (~21 regardless of database
//! size), not the whole file. That measurement is what this crate is built on.
//!
//! But it also named the thing that measurement did *not* settle: latency against a **wide-area**
//! object store. Those ~21 faults cost nothing on a local tier; against a real bucket each is a network
//! round-trip, and DuckDB issues its reads **serially** — one range, wait, the next. Faulted reactively,
//! ~21 faults become ~21 serial round-trips, and that is where a sub-second wake dies. So the scheduler
//! is designed around one assumption, stated by the roadmap and honored here: **faults may need to be
//! fetched in a batch.**
//!
//! # What this crate does
//!
//! It turns a database's fault set into a single coalesced prefetch, and it *learns* that set instead of
//! guessing it:
//!
//! 1. [`RecordingSource`] wraps the real [`PageSource`](flock_vfs::PageSource) and records every page a
//!    query faults through it — the query's actual fault trace, not a heuristic window.
//! 2. [`WakeScheduler::observe`] folds that trace into the database's **warm set**.
//! 3. On the next cold wake, [`WakeScheduler::prewarm`] hands the warm set to
//!    [`PageSource::prefetch`](flock_vfs::PageSource::prefetch), which fans the faults out
//!    concurrently — so N serial round-trips collapse toward one, and by the time DuckDB's serial reads
//!    arrive the pages are resident.
//!
//! The result is that the *second* wake of a database — the common case for a hot tenant — pays one
//! coalesced round-trip for the set the first wake had to discover.
//!
//! # What this crate does NOT do yet — said plainly (CLAUDE.md rule 6)
//!
//! - **No daemon socket / ATTACH protocol.** The crate name ends in `d` for the daemon it will become;
//!   this increment is the *scheduler* the daemon is built on, not the serve loop. The network surface,
//!   connection handling, and the DuckDB-extension wiring that makes [`RecordingSource`] observe the
//!   *extension's* fault stream (rather than a `serve_read` driven directly) are the next milestone.
//! - **No latency claim.** The scheduler makes the fault set *coalesceable*; whether coalescing brings
//!   a wide-area wake under any particular budget is a measurement (`tests/s3_measure.rs` and the
//!   wide-area workflow), and **no number is quoted until that measurement exists**. In particular the
//!   250 ms target in substrate `docs/02 §7` remains **unmeasured** and unclaimed.
//! - **No cross-restart warm-set persistence.** The learned warm sets live for the scheduler's
//!   lifetime. Persisting them beside the database's sleep record is a small follow-on; the shape
//!   ([`WarmSet`] is a plain sorted page list) is chosen so that persisting it is trivial when it lands.

// CLAUDE.md rule 2: no `unwrap`/`expect`/`panic!` in a storage engine's library code — a panic is an
// unplanned process death, and this schedules the wakes. The denials make it a compile error.
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(missing_docs)]
// Tests may panic — that is what an assertion is. The denials stay in force for all library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod scheduler;

pub use scheduler::{DbId, RecordingSource, WakeScheduler, WarmSet};
