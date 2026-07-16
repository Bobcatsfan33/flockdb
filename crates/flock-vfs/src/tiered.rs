//! The real page source: substrate's [`TieredStore`], woken from a [`WakeToken`].
//!
//! This is the production implementation of [`PageSource`], and it is deliberately thin. It holds a
//! woken store and answers `read_page` by faulting that page through substrate's pager — a local-cache
//! hit is a local read, a miss fetches from the object-storage tier and verifies the page's content
//! hash. It adds **no arithmetic** of its own: all of the offset → page translation lives in
//! [`serve_read`](crate::read::serve_read), which is fuzzed. So the bytes the fuzzer proves correct are
//! the bytes that ship; this file only supplies the pages.

use crate::error::{Result, VfsError};
use crate::source::PageSource;
use std::sync::{Arc, Mutex};
use substrate_pager::PageStore;
use substrate_store::TieredStore;

/// How many independent fault gates guard concurrent first-faults (see [`TieredPageSource`]). A page is
/// gated by `page_no % FAULT_STRIPES`, so the same page always takes the same gate while distinct pages
/// almost always take different ones. A power of two keeps the modulo cheap; 1024 is far more than the
/// thread count of any query engine that faults through us, so genuine same-stripe contention between
/// *different* pages is negligible.
const FAULT_STRIPES: usize = 1024;

/// A [`PageSource`] backed by a woken substrate [`TieredStore`].
///
/// Construct one with [`TieredPageSource::new`] over a store you have already woken, or let
/// [`crate::ffi::flock_vfs_open`] wake and wrap one for the C++ extension.
pub struct TieredPageSource {
    store: Arc<TieredStore>,
    total_len: u64,
    /// Serializes concurrent *first* faults of the same page.
    ///
    /// **Why this exists.** Substrate's local CAS writes a faulted page by streaming it to a temp file
    /// whose name is the page's content hash and then `rename`-ing it into place. That temp name is
    /// therefore identical for every writer of the same page, so two threads faulting the *same*
    /// not-yet-cached page collide: one renames the temp into place, the other's `rename` then fails
    /// with `ENOENT` because the temp it was going to move is already gone. A parallel query engine
    /// (DuckDB's multi-threaded scan) faults concurrently, so at large sizes — more scan threads, more
    /// boundary pages touched by two threads at once — that collision is not rare, it is the normal
    /// failure. Gating the fault by page number turns the concurrent writers of one page into a queue:
    /// the first faults and fills the cache, the rest find it already resident. Distinct pages fault
    /// fully in parallel; only the same page waits, and only until it is cached once.
    ///
    /// The gate is held across the (blocking) fault deliberately — that blocking call *is* the fill it
    /// is protecting. It is a fixed-size striped array rather than a per-page map so it neither grows
    /// with the database nor takes a global lock on every read.
    fault_gates: Box<[Mutex<()>]>,
}

impl TieredPageSource {
    /// Wrap an already-woken store.
    ///
    /// `total_len` is the database file's byte length — what DuckDB `fstat`s and what bounds every read.
    /// It is **not** discoverable from the store alone (the pager knows how many pages exist, but the
    /// last page's exact length is a property of the file, not the page store), so it travels with the
    /// database's sleep record alongside the [`WakeToken`](substrate_store::WakeToken) and is supplied
    /// here. Getting it wrong does not corrupt anything — a too-large `total_len` surfaces as a
    /// [`VfsError::ShortPage`] on the read that runs past the real data, which is a refusal, not wrong
    /// bytes.
    pub fn new(store: Arc<TieredStore>, total_len: u64) -> Self {
        let fault_gates = (0..FAULT_STRIPES).map(|_| Mutex::new(())).collect();
        Self {
            store,
            total_len,
            fault_gates,
        }
    }

    /// The database file's length in bytes — what a read is clamped to.
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// The woken store this reads from.
    pub fn store(&self) -> &Arc<TieredStore> {
        &self.store
    }
}

impl PageSource for TieredPageSource {
    fn page_size(&self) -> usize {
        self.store.pager().page_size()
    }

    fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
        let pager = self.store.pager();
        let head = pager.head();
        // Gate this page's fault so two threads never fill the same not-yet-cached page at once — see
        // `fault_gates`. Poison recovery mirrors substrate: a gate is a bare `()`, so a thread that
        // panicked while holding it left no state to be inconsistent; take the guard and carry on rather
        // than propagate a panic (rule 2: no panics on the read path).
        let gate = &self.fault_gates[(page_no as usize) % FAULT_STRIPES];
        let _hold = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The fault. `PageStore::read` is synchronous — substrate-pager is sync on purpose (rule 7) —
        // and the tiered CAS bridges to its async object-storage fetch internally on a cache miss. So
        // from here a fault is one blocking call, exactly as the F5 spike measured it.
        let page = pager
            .read(&head, page_no)
            .map_err(|source| VfsError::Fault { page_no, source })?;
        // One copy out of the page cache. The zero-copy alternative and why we do not take it is in the
        // `PageSource` docs.
        Ok(page.as_bytes().to_vec())
    }
}
