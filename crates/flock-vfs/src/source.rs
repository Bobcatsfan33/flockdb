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
}
