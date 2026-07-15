//! The fuzz gate for `serve_read` — the FFI read boundary — driven by proptest against an
//! obviously-correct ground-truth oracle.
//!
//! This is RISK-1's "buy back the FFI seam" made concrete. `serve_read` is a new trust surface that
//! substrate's in-tree differential/crash fuzzing does not cover (F1's standing objection). Here it is
//! hammered with malformed and hostile input — huge/overflowing offsets, zero-length and past-EOF
//! reads, page-straddling reads, and a page source that returns short, oversize, or missing pages — and
//! every result is checked against a trivial in-memory model of the file. The properties asserted are
//! the four invariants from `read.rs`:
//!
//! 1. **Never panics** — a proptest case that panics is a failure, so the whole suite is a panic check.
//! 2. **Never over-reports** — `Ok(n)` implies `n <= buf.len()` and `n <= total_len - offset`.
//! 3. **Never returns wrong bytes** — `Ok(n)` implies the `n` bytes returned equal the ground-truth
//!    file's `[offset, offset+n)` exactly. This is the "never the wrong page's bytes" invariant, and it
//!    is checked on *every* Ok, hostile pages included.
//! 4. **Fails closed, not open** — when no page in the covered range misbehaves, the read must succeed
//!    with the full clamped length; it may only return fewer bytes or an error when a page it needs is
//!    genuinely bad.
//!
//! The mock models a page source the way substrate's contract makes real: a returned page's bytes ARE
//! that page's bytes (substrate hash-verifies every fault, so content corruption is caught below this
//! boundary and surfaces as a fault *error*, not wrong bytes). So the hostile behaviours modelled here
//! are the ones `serve_read` alone is responsible for: a page that is *missing*, *oversize*, or *short*
//! — i.e. disagreements between the file length and the pages, which are exactly what a wrong
//! `total_len` or a truncated store produce.

use flock_vfs::error::Result;
use flock_vfs::{serve_read, PageSource, VfsError};
use proptest::prelude::*;
use std::collections::HashMap;

/// How a single page misbehaves when faulted.
#[derive(Clone, Debug)]
enum Behavior {
    /// Return the page's real bytes (correct store).
    Normal,
    /// Fail the fault (a lost/missing page).
    Missing,
    /// Return the real bytes plus `extra` junk bytes, so the page exceeds `page_size`.
    Oversize(usize),
    /// Return only the first `keep` real bytes of the page (a short/truncated page). The bytes are
    /// still *correct* — a short store has fewer bytes, not wrong ones — which is why an Ok result off
    /// a short page must still match ground truth.
    Short(usize),
}

/// An obviously-correct page source over a ground-truth byte vector, with per-page hostility knobs.
struct MockSource {
    page_size: usize,
    data: Vec<u8>,
    behaviors: HashMap<u64, Behavior>,
}

impl MockSource {
    /// The real bytes of page `page_no` (its slice of `data`, possibly a short final page).
    fn real_page(&self, page_no: u64) -> Vec<u8> {
        let start = (page_no as usize).saturating_mul(self.page_size);
        if start >= self.data.len() {
            return Vec::new();
        }
        let end = (start + self.page_size).min(self.data.len());
        self.data[start..end].to_vec()
    }
}

impl PageSource for MockSource {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
        match self.behaviors.get(&page_no) {
            // A lost/missing page: the fault fails. On the real path this is a `VfsError::Fault`
            // wrapping substrate's `PagerError` (which is `#[non_exhaustive]` and so cannot be built
            // here); the oracle only cares that it is an `Err`, so a crate-local error stands in.
            Some(Behavior::Missing) => Err(VfsError::ShortPage {
                page_no,
                needed_offset: 0,
                page_len: 0,
            }),
            Some(Behavior::Oversize(extra)) => {
                let mut p = self.real_page(page_no);
                p.extend(std::iter::repeat_n(0xEE, *extra + 1)); // +1 guarantees len > page_size
                Ok(p)
            }
            Some(Behavior::Short(keep)) => {
                let p = self.real_page(page_no);
                let k = (*keep).min(p.len());
                Ok(p[..k].to_vec())
            }
            _ => Ok(self.real_page(page_no)),
        }
    }
}

/// The clamped number of bytes an honest read of `[offset, offset+buf_len)` should return.
fn expected_clamp(total_len: u64, offset: u64, buf_len: usize) -> usize {
    if offset >= total_len {
        return 0;
    }
    (total_len - offset).min(buf_len as u64) as usize
}

/// Does the covered range touch any page whose behaviour could legitimately make the read fail or
/// short? If not, the read MUST fully succeed (invariant 4).
fn covered_range_is_all_clean(src: &MockSource, total_len: u64, offset: u64, want: usize) -> bool {
    if want == 0 {
        return true;
    }
    let ps = src.page_size as u64;
    if ps == 0 {
        return false; // a zero page size is itself a clean-failure case
    }
    let first = offset / ps;
    let last = (offset + want as u64 - 1) / ps;
    for page_no in first..=last {
        match src.behaviors.get(&page_no) {
            None | Some(Behavior::Normal) => {}
            Some(Behavior::Missing) | Some(Behavior::Oversize(_)) => return false,
            Some(Behavior::Short(keep)) => {
                // The last byte this read needs within the page. If the page is short of it, the read
                // legitimately fails; if the page keeps enough, it is effectively clean for this read.
                let page_start = page_no * ps;
                let need_end_in_page =
                    ((offset + want as u64).min(total_len) - page_start) as usize;
                if *keep < need_end_in_page {
                    return false;
                }
            }
        }
    }
    total_len != 0 && offset < total_len
}

proptest! {
    // Hard run: 8192 independent cases per invocation, each an adversarial (data, page_size, offset,
    // len, behaviours) tuple. `cargo test` runs this every time; the coverage-guided libFuzzer target
    // in `fuzz/` drives the same `serve_read` for millions of iterations on nightly.
    #![proptest_config(ProptestConfig { cases: 8192, ..ProptestConfig::default() })]

    #[test]
    fn serve_read_upholds_its_invariants(
        data in proptest::collection::vec(any::<u8>(), 0..4096),
        page_size in 1usize..=256,
        // Offsets: mostly in range, but deliberately includes huge and max values to exercise the
        // past-EOF and overflow guards.
        offset in prop_oneof![
            0u64..8192,
            Just(u64::MAX),
            Just(u64::MAX - 1),
            (u64::MAX - 4096)..u64::MAX,
        ],
        buf_len in 0usize..4096,
        // A handful of hostile page behaviours keyed by (small) page number.
        behavior_seeds in proptest::collection::vec(
            (0u64..64, prop_oneof![
                Just(Behavior::Normal),
                Just(Behavior::Missing),
                (0usize..300).prop_map(Behavior::Oversize),
                (0usize..300).prop_map(Behavior::Short),
            ]),
            0..8,
        ),
    ) {
        let behaviors: HashMap<u64, Behavior> = behavior_seeds.into_iter().collect();
        let total_len = data.len() as u64;
        let src = MockSource { page_size, data: data.clone(), behaviors };

        let mut buf = vec![0xA5u8; buf_len];
        let result = serve_read(&src, total_len, offset, &mut buf);

        match result {
            Ok(n) => {
                // Invariant 2: never over-reports.
                prop_assert!(n <= buf_len, "returned {n} > buffer {buf_len}");
                let clamp = expected_clamp(total_len, offset, buf_len);
                prop_assert!(
                    n <= clamp,
                    "returned {n} bytes but only {clamp} exist before EOF (offset {offset}, total {total_len})"
                );
                // Invariant 3: the bytes returned are the RIGHT bytes — never another page's.
                let off = offset as usize; // n>0 implies offset<total_len<=data.len, so this fits usize
                if n > 0 {
                    prop_assert_eq!(
                        &buf[..n],
                        &data[off..off + n],
                        "returned bytes do not match the ground-truth file at offset {}", offset
                    );
                }
                // Invariant 4: a fully-clean covered range must not short-read.
                if covered_range_is_all_clean(&src, total_len, offset, clamp) {
                    prop_assert_eq!(n, clamp, "clean read short-changed: got {} want {}", n, clamp);
                }
            }
            Err(_) => {
                // An error is only permitted when the read genuinely needed a bad page (or a zero page
                // size, which cannot happen here since page_size >= 1). A clean read must never error.
                let clamp = expected_clamp(total_len, offset, buf_len);
                prop_assert!(
                    !covered_range_is_all_clean(&src, total_len, offset, clamp),
                    "a clean, in-bounds read returned an error (offset {offset}, len {buf_len}, total {total_len})"
                );
            }
        }
    }
}
