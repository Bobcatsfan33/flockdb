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
use std::sync::Arc;
use substrate_pager::PageStore;
use substrate_store::TieredStore;

/// A [`PageSource`] backed by a woken substrate [`TieredStore`].
///
/// Construct one with [`TieredPageSource::new`] over a store you have already woken, or let
/// [`crate::ffi::flock_vfs_open`] wake and wrap one for the C++ extension.
pub struct TieredPageSource {
    store: Arc<TieredStore>,
    total_len: u64,
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
        Self { store, total_len }
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
