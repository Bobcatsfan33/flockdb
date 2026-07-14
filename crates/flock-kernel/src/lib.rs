//! # flock-kernel
//!
//! The SQL engine FlockDB hosts, behind one trait.
//!
//! FlockDB does not have a SQL dialect. It has *storage semantics* — fork, snapshot, rewind,
//! sleep — and it rents the SQL from DuckDB, which is very good at it (docs/02 §2.1: *"Not a new
//! SQL dialect. We host a proven kernel."*). [`SqlKernel`] is the seam between the two, and
//! [`DuckDbKernel`] is the only implementation there is or is likely to be.
//!
//! ---
//!
//! # THE INTEGRATION DECISION, AND WHAT IT COSTS
//!
//! This is the single most consequential engineering choice in FlockDB, so it is written down at
//! the top of the crate rather than buried in a commit message.
//!
//! ## What we wanted: a DuckDB filesystem hook backed by `PageStore`
//!
//! The ideal, named in docs/02 §5.2, is a **virtual filesystem**. DuckDB believes it has a file;
//! every read it issues is served from a substrate page, every write it issues becomes a page.
//! Nothing is ever copied. `fork()` would then be genuinely O(1) *end to end* — not merely O(1)
//! in the storage layer — because the forked database would be a new manifest and zero bytes of
//! work.
//!
//! ## What actually exists: the seam is real, and it is not reachable from Rust
//!
//! We looked, because the difference is worth a great deal. The finding, from reading the sources
//! of `duckdb` 1.10504 and its `libduckdb-sys` backend:
//!
//! - **DuckDB's C++ core does have the seam.** `duckdb/common/virtual_file_system.hpp` declares
//!   `VirtualFileSystem::RegisterSubSystem(unique_ptr<FileSystem>)`. This is how the `httpfs`
//!   extension teaches DuckDB to read `s3://` URLs. The capability is real.
//!
//! - **The C API does not expose it.** `duckdb.h` ships a `duckdb_file_system` handle, but it is
//!   *consumption only*: `duckdb_client_context_get_file_system`, `duckdb_file_system_open`,
//!   `duckdb_file_handle_read` / `_write` / `_seek` / `_sync`. Those let an extension **use**
//!   DuckDB's filesystem. There is no inverse. The complete set of registration entry points in
//!   the C API is `duckdb_register_{scalar,aggregate,table,copy,cast}_function`,
//!   `duckdb_register_logical_type`, `duckdb_register_config_option`, and
//!   `duckdb_register_log_storage`. **There is no `duckdb_register_file_system`.**
//!
//! - **`duckdb-rs` binds the C API and nothing else.** A grep for `filesystem`, `file_system`, or
//!   `vfs` across `duckdb-rs`'s entire `src/` returns exactly one hit, and it is a comment in a
//!   unit test about invalid Unicode. There is no seam to use, because from Rust there is no seam.
//!
//! The only route to the C++ hook is to write a DuckDB **loadable extension in C++** that
//! registers a `FileSystem` subclass calling back into Rust. That is a real option and it may be
//! the right one later. It is not F1: it means shipping and signing a C++ extension per platform,
//! and it would put the most safety-critical bytes in the system behind an FFI boundary we cannot
//! fuzz with the rest of the engine. We are not doing that to save a file copy before we have
//! measured what the file copy costs.
//!
//! **So: the FS-hook path does not work today, and this crate does not pretend that it does.**
//!
//! ## What we built: the documented fallback
//!
//! docs/02 §5.2 names the fallback, and we took it:
//!
//! > *DuckDB owns a temp file, and `flock-core` syncs file ↔ pages at transaction boundaries.*
//!
//! ```text
//!   open:        pages ──────► temp file ──────► DuckDB opens it
//!   query:                     temp file ◄─────► DuckDB      (we are not in the path at all)
//!   checkpoint:  pages ◄────── temp file        (chunk, diff, write only what changed)
//! ```
//!
//! ## The bill, in full
//!
//! **Reads pay nothing.** A `SELECT` is DuckDB reading a local file it opened itself. FlockDB is
//! not in the loop, not in the call stack, and not in the profile.
//!
//! Measured, TPC-H SF0.1, 21 queries, paired rounds: **+0.3 % / +1.1 % / −4.0 %** across three
//! runs, against a docs/02 §7 target of < 15 %. The honest reading is *"indistinguishable from
//! zero, and the rig cannot resolve finer"* — one run came out negative, which is the noise floor
//! of a laptop and not a claim that we are faster than the engine we are hosting. See the README
//! for how the first version of that benchmark was lying and what it took to fix.
//!
//! **`checkpoint()` reads the whole database.** Every sync reads the entire temp file, splits it
//! into `page_size` chunks (64 KiB by default), and byte-compares each chunk against the page
//! already in the store. Unchanged chunks are skipped, so the *write* volume tracks what actually
//! changed — but the *read* volume is the whole file, every time:
//!
//! | Database size | Bytes read per `checkpoint()` | Pages compared |
//! | --- | --- | --- |
//! | 10 MiB | 10 MiB | 160 |
//! | 1 GiB | 1 GiB | 16,384 |
//! | 100 GiB | 100 GiB | 1,638,400 |
//!
//! At 1 GiB this is a fraction of a second and nobody notices. At 100 GiB it is *minutes*, and
//! FlockDB is the wrong tool — which is consistent with the product's shape (docs/02 §1: many
//! *small* databases), but it is a ceiling and we name it here rather than let a customer find it
//! in a POC.
//!
//! **`fork()` is O(1) in substrate and O(database size) in the kernel.** This is the honest
//! headline, and the one it would be most tempting to fudge. The substrate fork *is* free — a new
//! manifest, 98 ns, no bytes copied — and the isolation it gives is total. But the forked `Db`
//! needs a DuckDB connection, and DuckDB needs a *file*, so we hydrate a fresh temp file from the
//! fork's pages. **The isolation guarantee is completely real; the constant factor is not free.**
//! The FS hook is exactly what would erase this, and that is the strongest argument for eventually
//! paying for the C++ extension.
//!
//! > *The clever thing we did not do:* on APFS, and on Linux with `FICLONE`, the hydration copy
//! > could be a copy-on-write clone of the parent's temp file — O(1) again, at the price of
//! > platform-specific code in the one part of the system that must never be surprising. CLAUDE.md
//! > rule 10 says prefer boring and leave a note saying what the clever thing would have been.
//! > This is the note.
//!
//! ## The durability boundary, stated bluntly
//!
//! **Between checkpoints, your data is in a temp file, not in FlockDB.** DuckDB commits an
//! `INSERT` to its own file and its own WAL; it does not tell us, and we do not watch. Substrate
//! learns what you wrote only when [`SqlKernel::checkpoint`] runs — which `flock-core` does on
//! `Db::snapshot()`, `Db::fork()`, and `Db::sleep()`.
//!
//! So: **a crash loses everything written since the last `snapshot()`.** That is not a bug, it is
//! the fallback's shape, and it is the second thing the FS hook would fix. Snapshot when you mean
//! it — they cost 15 ns in substrate, and the sync above in the kernel.
//!
//! # Why this crate is allowed to touch the filesystem
//!
//! Substrate's CLAUDE.md rule 2 says all durable state goes through `PageStore`, and no other
//! crate writes a file. This crate writes two, deliberately:
//!
//! 1. **The temp file.** It is not durable state. It is scratch, it lives in a `TempDir`, it is
//!    deleted on drop, and the durable copy of every byte in it is in the `PageStore`. Rule 2
//!    exists so that encryption, tiering, integrity scrubbing, and air-gap enforcement have
//!    exactly one place to live; a scratch file destroyed with the process escapes none of them.
//! 2. **The export file** ([`SqlKernel::export`]). Writing a file *outside* our storage layer is
//!    the entire point of it (docs/02 §6.2). A rule against leaving cannot apply to the exit.
//!
//! Both are named here so that a reader who greps for `File::create` finds a reason, not a
//! surprise.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(rust_2018_idioms)]
// Tests are the one place a panic is the correct response to the impossible: a failing assertion
// *should* stop the run. CLAUDE.md rule 6 bans panics in library code, not in the code that proves
// the library is right.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod duck;
mod error;
mod paging;
mod stream;

pub use duck::{DuckDbKernel, KernelOpts};
pub use error::{KernelError, Result};
pub use stream::ArrowStream;

// Re-exported so that a consumer never has to independently guess which `arrow` version we agreed
// with DuckDB on. A duplicate `arrow` in the dependency graph produces a type error that names the
// same type twice and runs to two hundred lines; this re-export is much cheaper than that.
pub use duckdb::arrow;

use std::path::Path;
use std::sync::Arc;
use substrate_pager::{ManifestId, PageStore};

/// A SQL engine that keeps its bytes in a [`PageStore`].
///
/// The trait exists so that FlockDB's storage semantics are not welded to DuckDB's internals.
/// Today there is exactly one implementation ([`DuckDbKernel`]) — the trait is what makes that a
/// *choice* rather than an assumption baked through `flock-core`.
///
/// Defined in docs/02 §5.2. The shape is frozen there; the honest account of how faithfully we
/// implement it is at the top of this crate.
pub trait SqlKernel: Send {
    /// Open a kernel over `store`, hydrating from the store's current head.
    ///
    /// The head is the caller's business: `flock-core` positions the store — by forking, by
    /// rewinding, or by WAL recovery — and *then* opens a kernel on it. That keeps "which version
    /// of the database am I looking at" in exactly one place. Two components each holding an
    /// opinion about the current head is how a fork silently reads its parent.
    fn open(store: Arc<dyn PageStore>, opts: KernelOpts) -> Result<Self>
    where
        Self: Sized;

    /// Run a SQL statement and return its rows as Arrow.
    ///
    /// Arrow, not a bespoke row format, because Arrow is how the Python bindings, the CLI, and the
    /// fan-out service speak to this without paying a conversion at every hop (docs/02 §6.1).
    fn query(&mut self, sql: &str) -> Result<ArrowStream>;

    /// Run a SQL statement for its effect. Returns the number of rows it changed.
    fn execute(&mut self, sql: &str) -> Result<u64>;

    /// Flush every byte DuckDB owns into pages, and return the manifest that now describes them.
    ///
    /// **This is the moment FlockDB learns what you wrote.** See "The durability boundary" in the
    /// crate docs: nothing between one `checkpoint` and the next survives a crash, because until
    /// this runs the data exists only in a temp file DuckDB never told us about.
    ///
    /// Returns the store's unchanged head when nothing changed, so calling it repeatedly does not
    /// litter the manifest DAG with identical states.
    fn checkpoint(&mut self) -> Result<ManifestId>;

    /// Write a vanilla `.duckdb` file at `path`. **The escape hatch** (docs/02 §6.2).
    ///
    /// The file has no dependency on FlockDB, on substrate, or on anything we ship. Open it with
    /// the `duckdb` CLI, with Python, with a BI tool. That is the point: the largest objection to
    /// adopting a new storage engine is *"what if you disappear, or I hate you"*, and the only
    /// honest answer is one command that returns the data in a format with an ecosystem.
    ///
    /// It is tested on every commit against a *fresh, vanilla* DuckDB connection that has never
    /// heard of us, precisely so it cannot rot into a claim we have stopped checking.
    fn export(&mut self, path: &Path) -> Result<()>;
}
