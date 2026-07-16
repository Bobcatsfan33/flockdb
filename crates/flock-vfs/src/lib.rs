//! # flock-vfs — the page-faulting read path for FlockDB
//!
//! DuckDB believes it holds a database *file*. This crate is what makes that belief cheap when the
//! file is a sleeping database in object storage: every byte range DuckDB reads is translated into
//! substrate page faults and served on demand, so waking a database and answering a selective query
//! moves only the pages the query touches — not the whole file (`docs/wake-latency.md`, RISK-1).
//!
//! ## Where this sits
//!
//! FlockDB's stock DuckDB C API has **no filesystem-registration hook** — verified against DuckDB
//! 1.10504, its C loadable-extension API, and `duckdb-rs` (see the top of `flock-kernel/src/lib.rs`).
//! So the production read path is a **C++ DuckDB `FileSystem` subclass** (`RegisterSubSystem`) whose
//! `Read` calls into this crate over its [C ABI](ffi). This crate is the Rust half of that path: it is
//! the productionised, hardened form of the F5 interpose spike (`spikes/wake-latency/faultshim/`),
//! which measured the flat, ~40–105 ms wake-one-of-many floor this path delivers.
//!
//! ## The shape of the crate, and why
//!
//! - [`read::serve_read`] is the boundary: `(total_len, offset, buf)` → bytes, faulting pages via a
//!   [`source::PageSource`]. It is pure, uses **only safe slices**, and is the crate's whole
//!   correctness burden. It is fuzzed (`tests/proptest_read.rs` and `fuzz/`) because it is a new trust
//!   surface substrate's own fuzzing does not cover — F1's standing objection, bought back here.
//! - [`source::PageSource`] is the seam that lets the fuzzer drive `serve_read` with a hostile,
//!   in-memory page source instead of a real object store.
//! - [`tiered::TieredPageSource`] is the real, boring implementation over substrate's `TieredStore`.
//! - [`ffi`] is the C ABI, and the crate's only `unsafe` — it validates every C-supplied value and
//!   hands `serve_read` a safe slice, never a pointer.
//!
//! ## What this crate does NOT contain
//!
//! No `flockd`, no registry, no wake-on-query scheduler (RISK-1 scope: those wait on this read path
//! being clocked against *real object storage*). And it quotes **no 250 ms number** — the measured
//! floor is a zero-network lower bound, and the real-object-storage number is still unmeasured.

// CLAUDE.md rule 2 / substrate rule 6: no panics in a storage engine's read path. These denials make
// an `unwrap`/`expect`/`panic!` a compile error, not a code-review miss — this is the fuzzed boundary,
// so it is the last place a panic is acceptable.
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(missing_docs)]
// Tests may panic — that is what an assertion is (rust testing rule; CLAUDE.md rule 8 forbids
// *skipping* a test, not asserting in one). The denials above stay in force for all library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backend;
pub mod error;
pub mod ffi;
pub mod read;
pub mod source;
pub mod tiered;

pub use backend::remote_tier;
pub use error::{Result, VfsError};
pub use read::serve_read;
pub use source::PageSource;
pub use tiered::TieredPageSource;
