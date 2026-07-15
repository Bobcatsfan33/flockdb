//! `serve_read` — the read boundary. This is the whole point of the crate, and the whole risk.
//!
//! DuckDB believes it holds a `total_len`-byte database file. Every `pread(offset, len)` it issues
//! becomes a call here, and this function turns that byte range into substrate page faults and copies
//! the bytes back. It is the exact boundary the F5 interpose spike proved
//! (`spikes/wake-latency/faultshim/`), lifted out of that spike's global-state measurement harness
//! into a pure, testable function so it can be *fuzzed* — because on the production path this is C++
//! calling Rust with an offset, a length, and a raw buffer, and a page-faulting read path that a
//! malformed request can crash or confuse is a remote-code-execution surface, not a database feature.
//!
//! # The invariants this function guarantees, and why each matters
//!
//! For *any* inputs — including malformed, negative-turned-huge, overflowing, or a [`PageSource`] that
//! actively misbehaves — `serve_read`:
//!
//! 1. **Never panics.** It is library code in a storage engine (CLAUDE.md rule 2). Every arithmetic
//!    step is checked; every slice index is proven in-bounds before it is taken.
//! 2. **Never reads or writes out of bounds.** It operates entirely on safe slices (`buf: &mut [u8]`
//!    and `&page[..]`). There is no `unsafe` in this file; a bad index would panic, not corrupt — and
//!    invariant 1 proves it never even indexes badly. The one `unsafe` in the crate is at the FFI edge
//!    ([`crate::ffi`]), wrapping the caller's raw pointer, and it does nothing but hand this function a
//!    safe slice.
//! 3. **Never returns uninitialised memory.** It returns the count of bytes it actually copied from
//!    real pages. On any failure it returns `Err` — the caller (a `pread`) treats that as a short/failed
//!    read and never trusts the buffer. It never reports more bytes than it wrote.
//! 4. **Never returns the wrong page's bytes.** Byte `p` of the file comes from page `p / page_size` at
//!    offset `p % page_size`, and from nowhere else. A page that is the wrong size to satisfy that is
//!    refused (`OversizePage` / `ShortPage`), not stretched to fit.

use crate::error::{Result, VfsError};
use crate::source::PageSource;

/// Serve the byte range `[offset, offset + buf.len())` of a virtual file of length `total_len` into
/// `buf`, faulting pages from `src` on demand. Returns the number of bytes filled — which is fewer than
/// `buf.len()` exactly when the range runs past `total_len` (an ordinary end-of-file short read), and is
/// `0` for a zero-length read or a read starting at or past the end of the file.
///
/// See the module docs for the four invariants this upholds for arbitrary — including hostile — input.
pub fn serve_read<S>(src: &S, total_len: u64, offset: u64, buf: &mut [u8]) -> Result<usize>
where
    S: PageSource + ?Sized,
{
    // A read starting at or past EOF returns nothing. `pread` semantics, and it disposes of the whole
    // class of "huge offset" requests (including an i64 -1 that the FFI edge turned into u64::MAX)
    // before any arithmetic can go wrong.
    if offset >= total_len {
        return Ok(0);
    }

    // How many bytes we will actually serve: the request, clamped to what is left of the file. `offset
    // < total_len` above makes `total_len - offset` a positive, non-wrapping value; the `min` with the
    // buffer length keeps `want <= buf.len()`, so every write into `buf` below is in bounds by
    // construction. A zero-length buffer makes `want == 0` and we return early.
    let remaining = total_len - offset;
    let want = remaining.min(buf.len() as u64) as usize;
    if want == 0 {
        return Ok(0);
    }

    // A zero page size cannot address any byte, and dividing by it would panic. Refuse.
    let page_size = src.page_size();
    if page_size == 0 {
        return Err(VfsError::ZeroPageSize);
    }
    let ps = page_size as u64;

    let mut done = 0usize;
    while done < want {
        // The absolute file position we are about to serve. `offset < total_len` and `done < want` with
        // `offset + want <= total_len`, so this cannot exceed `total_len` — but a malformed near-`u64::MAX`
        // offset that slipped through could still overflow the add, so it is checked, not assumed.
        let file_pos = offset
            .checked_add(done as u64)
            .ok_or(VfsError::OffsetOverflow { offset })?;

        let page_no = file_pos / ps;
        let page_off = (file_pos % ps) as usize;

        // The fault. On the real path this is a substrate page fetch (tier get + hash verify on a miss);
        // here it is whatever the `PageSource` returns, including a deliberately corrupt one under fuzz.
        let page = src.read_page(page_no)?;

        // A page must be exactly `page_size` bytes (shorter only if it is the file's last page). A page
        // LARGER than that is bytes we cannot account for — refuse rather than index into them.
        if page.len() > page_size {
            return Err(VfsError::OversizePage {
                page_no,
                page_len: page.len(),
                page_size,
            });
        }

        // The file length claimed byte `file_pos` exists, so it must fall within this page. If the page
        // is too short to contain `page_off`, the file length and the pages disagree — refuse rather
        // than read whatever is past the page's end.
        if page_off >= page.len() {
            return Err(VfsError::ShortPage {
                page_no,
                needed_offset: page_off,
                page_len: page.len(),
            });
        }

        // Copy as much of this page as the request still needs. `take >= 1` (page_off < page.len() gives
        // at least one available byte, and done < want gives at least one wanted byte), so the loop
        // always makes progress and cannot spin. Both slices below are `take` bytes and both are proven
        // in bounds: `done + take <= want <= buf.len()`, and `page_off + take <= page.len()`.
        let available = page.len() - page_off;
        let needed = want - done;
        let take = available.min(needed);

        buf[done..done + take].copy_from_slice(&page[page_off..page_off + take]);
        done += take;
    }

    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::VfsError;

    /// A trivial, obviously-correct page source: a virtual file cut into `page_size` pages, plus knobs
    /// to make individual pages misbehave exactly as a corrupt store would. This is the same shape the
    /// proptest oracle uses; the unit tests here pin the specific adversarial cases by hand.
    struct MockSource {
        page_size: usize,
        /// The ground-truth file bytes. Pages are carved from this unless overridden below.
        data: Vec<u8>,
        /// Pages whose fault must fail (a lost/missing page).
        missing: Vec<u64>,
        /// A page forced to come back oversize (page_no -> byte length > page_size).
        oversize: Option<(u64, usize)>,
        /// A page forced to come back short (page_no -> byte length).
        short: Option<(u64, usize)>,
    }

    impl MockSource {
        fn new(page_size: usize, data: Vec<u8>) -> Self {
            Self {
                page_size,
                data,
                missing: Vec::new(),
                oversize: None,
                short: None,
            }
        }
    }

    impl PageSource for MockSource {
        fn page_size(&self) -> usize {
            self.page_size
        }

        fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
            if self.missing.contains(&page_no) {
                // Model a lost page as a fault error via the crate's own error type. (We cannot easily
                // build a real PagerError here, so a ShortPage-shaped refusal stands in — the point of
                // the test is that serve_read propagates the Err, whatever its kind.)
                return Err(VfsError::ShortPage {
                    page_no,
                    needed_offset: 0,
                    page_len: 0,
                });
            }
            if let Some((no, len)) = self.oversize {
                if no == page_no {
                    return Ok(vec![0xAB; len]);
                }
            }
            if let Some((no, len)) = self.short {
                if no == page_no {
                    return Ok(vec![0xCD; len]);
                }
            }
            let start = (page_no as usize).saturating_mul(self.page_size);
            if start >= self.data.len() {
                return Ok(Vec::new());
            }
            let end = (start + self.page_size).min(self.data.len());
            Ok(self.data[start..end].to_vec())
        }
    }

    #[test]
    fn reads_a_whole_small_file() {
        let data: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let src = MockSource::new(64, data.clone());
        let mut buf = vec![0u8; data.len()];
        let n = serve_read(&src, data.len() as u64, 0, &mut buf).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(buf, data);
    }

    #[test]
    fn read_straddling_a_page_boundary_stitches_two_pages() {
        let data: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        let src = MockSource::new(64, data.clone());
        // 64-byte pages: a read of [60, 70) crosses the page 0 / page 1 boundary.
        let mut buf = vec![0u8; 10];
        let n = serve_read(&src, data.len() as u64, 60, &mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(buf, &data[60..70]);
    }

    #[test]
    fn read_past_eof_is_clamped_and_read_at_eof_is_zero() {
        let data: Vec<u8> = (0..100u32).map(|i| i as u8).collect();
        let src = MockSource::new(64, data.clone());
        // Straddle EOF: ask for 50 bytes starting at 80, only 20 exist.
        let mut buf = vec![0u8; 50];
        let n = serve_read(&src, 100, 80, &mut buf).unwrap();
        assert_eq!(n, 20);
        assert_eq!(&buf[..20], &data[80..100]);
        // Exactly at EOF, and well past it: zero.
        assert_eq!(serve_read(&src, 100, 100, &mut buf).unwrap(), 0);
        assert_eq!(serve_read(&src, 100, 1_000_000, &mut buf).unwrap(), 0);
        assert_eq!(serve_read(&src, 100, u64::MAX, &mut buf).unwrap(), 0);
    }

    #[test]
    fn zero_length_read_returns_zero() {
        let src = MockSource::new(64, vec![1, 2, 3, 4]);
        let mut buf: [u8; 0] = [];
        assert_eq!(serve_read(&src, 4, 0, &mut buf).unwrap(), 0);
    }

    #[test]
    fn zero_page_size_is_refused_not_divided_by() {
        let src = MockSource::new(0, vec![1, 2, 3, 4]);
        let mut buf = vec![0u8; 4];
        assert!(matches!(
            serve_read(&src, 4, 0, &mut buf),
            Err(VfsError::ZeroPageSize)
        ));
    }

    #[test]
    fn an_oversize_page_is_refused() {
        let mut src = MockSource::new(64, vec![7u8; 128]);
        src.oversize = Some((0, 65)); // page 0 comes back one byte too long
        let mut buf = vec![0u8; 32];
        assert!(matches!(
            serve_read(&src, 128, 0, &mut buf),
            Err(VfsError::OversizePage { page_no: 0, .. })
        ));
    }

    #[test]
    fn a_short_page_the_read_needs_is_refused() {
        let mut src = MockSource::new(64, vec![7u8; 128]);
        // File says 128 bytes (2 full pages), but page 1 comes back with only 10 bytes; a read into
        // page 1 past byte 10 must refuse rather than read garbage.
        src.short = Some((1, 10));
        let mut buf = vec![0u8; 64];
        let err = serve_read(&src, 128, 64, &mut buf).unwrap_err();
        assert!(matches!(err, VfsError::ShortPage { page_no: 1, .. }));
    }

    #[test]
    fn a_missing_page_propagates_an_error_not_wrong_bytes() {
        let mut src = MockSource::new(64, vec![7u8; 128]);
        src.missing = vec![1];
        let mut buf = vec![0u8; 128];
        assert!(serve_read(&src, 128, 0, &mut buf).is_err());
    }

    #[test]
    fn a_short_final_page_is_allowed() {
        // A file whose length is not a multiple of page_size: the last page is legitimately short.
        let data: Vec<u8> = (0..100u32).map(|i| i as u8).collect(); // 100 bytes, 64-byte pages
        let src = MockSource::new(64, data.clone());
        let mut buf = vec![0u8; 100];
        let n = serve_read(&src, 100, 0, &mut buf).unwrap();
        assert_eq!(n, 100);
        assert_eq!(buf, data);
    }
}
