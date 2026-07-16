//! The seam between the *arithmetic* of the read boundary and the *machinery* that fetches a page.
//!
//! [`serve_read`](crate::read::serve_read) — the byte-exact, memory-safety-critical part of this
//! crate, and the part the fuzzer hammers — is written against this trait, not against a
//! [`TieredStore`](substrate_store::TieredStore). That separation is deliberate and load-bearing:
//!
//! - The dangerous code (offset → page arithmetic, the buffer copy) is proven correct against a
//!   trivial in-memory [`PageSource`] that can be *told* to misbehave (return a short page, an
//!   oversize page, a missing page) far faster and more adversarially than a real object store could
//!   be driven to. The fuzz oracle in `tests/proptest_read.rs` is that in-memory source.
//! - The real path — [`TieredPageSource`](crate::tiered::TieredPageSource) — is a thin, boring impl
//!   of the same trait over substrate. It adds no arithmetic of its own, so the fuzzed arithmetic is
//!   the arithmetic that ships.
//!
//! This is how the FFI seam's escape from substrate's in-tree fuzzing is bought back: the escape is
//! the translation done here, and it is tested in isolation, exhaustively.

use crate::error::Result;

/// A source of fixed-size pages backing a virtual file.
///
/// Logical page `i` covers file bytes `[i * page_size, (i + 1) * page_size)`. Every page is exactly
/// `page_size` bytes except possibly the last page of the file, which may be shorter. A source that
/// violates that contract (a zero page size, an oversize page, a page too short for a byte the file
/// length claims exists) is rejected by [`serve_read`](crate::read::serve_read) rather than trusted —
/// the point of this crate is that a misbehaving store produces an *error*, never wrong bytes.
///
/// # Why `Vec<u8>`, and not a borrowed slice
///
/// Returning an owned `Vec` copies the page once. The clever alternative — lending `&[u8]` straight
/// out of substrate's page cache — would save that copy, but it entangles the page's lifetime with the
/// borrow and, on the real path, a page fault is already ~7 ms of `block_on` + tier get + BLAKE3
/// verify; one `memcpy` of 64 KiB is lost in the noise. CLAUDE.md rule 7: take the boring one, and this
/// is the note saying what the clever one was.
pub trait PageSource {
    /// The page size the store was created with. Must be non-zero for any read to succeed.
    fn page_size(&self) -> usize;

    /// Fault a single logical page and return its bytes.
    ///
    /// On the real path this is a substrate page fault: a local-cache hit is a local read, a miss
    /// fetches from the object-storage tier and verifies the page's content hash before returning it.
    /// Either way only pages a read actually covers are ever requested.
    fn read_page(&self, page_no: u64) -> Result<Vec<u8>>;

    /// Warm a set of pages into the local cache, **coalescing their faults** so that N pages that
    /// each miss the object-storage tier cost roughly one round-trip of wall-clock instead of N.
    ///
    /// # Why this exists
    ///
    /// A woken database answers a selective query by faulting a small, flat set of pages (`docs/04`,
    /// the wake-one-of-many measurement: ~21 pages regardless of database size). On a local or
    /// same-runner tier those faults are ~free and their *order* does not matter. Against a
    /// **wide-area** object store each fault is a real network round-trip, and DuckDB issues its reads
    /// *serially* — one range, wait, the next — so faulting reactively turns those ~21 faults into ~21
    /// serial RTTs, which is where a sub-second wake dies. This method lets a scheduler fault the whole
    /// predicted set **at once, concurrently**, before DuckDB's serial reads arrive; by the time they
    /// do, the pages are resident and each read is a local-cache hit.
    ///
    /// # It is advisory, and that is deliberate — not a swallowed error
    ///
    /// Prefetch's *only* effect is residency. It changes no bytes and weakens no check: every page
    /// DuckDB actually reads still goes through [`serve_read`](crate::read::serve_read), which
    /// re-derives and verifies the page's content hash. So a page that fails to prefetch (a transient
    /// tier error, a page that turns out not to exist) is simply left cold and faults for real —
    /// correctly, or with a real error — when, and only if, DuckDB reads it. Propagating a prefetch
    /// failure would be worse than useless: it would fail a wake over a page the query may never touch.
    /// A prefetch error is therefore *dropped on purpose*, documented here rather than hidden.
    ///
    /// The default implementation warms serially — correct, but not coalesced. A tiered source
    /// overrides it to fan the faults out concurrently.
    fn prefetch(&self, pages: &[u64]) {
        for &page_no in pages {
            // Best-effort warm; see the "advisory" note above. A cold page faults for real on read.
            let _ = self.read_page(page_no);
        }
    }
}
