//! Coverage-guided libFuzzer target for `serve_read` — the FFI read boundary.
//!
//! This drives the *same* function `tests/proptest_read.rs` gates, but with libFuzzer's coverage
//! feedback instead of proptest's random sampling, so it discovers edge cases by exploring branches.
//! A panic (invariant 1) or a violated assertion (invariants 2–4) is a libFuzzer crash. Run it with:
//!
//! ```bash
//! cargo +nightly fuzz run serve_read -- -runs=5000000
//! ```

#![no_main]

use arbitrary::Arbitrary;
use flock_vfs::error::Result;
use flock_vfs::{serve_read, PageSource, VfsError};
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

/// How a single page misbehaves — mirrors the proptest oracle's `Behavior`.
#[derive(Clone, Debug)]
enum Behavior {
    Normal,
    Missing,
    Oversize(usize),
    Short(usize),
}

struct MockSource {
    page_size: usize,
    data: Vec<u8>,
    behaviors: HashMap<u64, Behavior>,
}

impl MockSource {
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
            Some(Behavior::Missing) => Err(VfsError::ShortPage {
                page_no,
                needed_offset: 0,
                page_len: 0,
            }),
            Some(Behavior::Oversize(extra)) => {
                let mut p = self.real_page(page_no);
                p.extend(std::iter::repeat_n(0xEE, *extra + 1));
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

fn expected_clamp(total_len: u64, offset: u64, buf_len: usize) -> usize {
    if offset >= total_len {
        return 0;
    }
    (total_len - offset).min(buf_len as u64) as usize
}

fn covered_range_is_all_clean(src: &MockSource, total_len: u64, offset: u64, want: usize) -> bool {
    if want == 0 {
        return true;
    }
    let ps = src.page_size as u64;
    if ps == 0 {
        return false;
    }
    let first = offset / ps;
    let last = (offset + want as u64 - 1) / ps;
    for page_no in first..=last {
        match src.behaviors.get(&page_no) {
            None | Some(Behavior::Normal) => {}
            Some(Behavior::Missing) | Some(Behavior::Oversize(_)) => return false,
            Some(Behavior::Short(keep)) => {
                let page_start = page_no * ps;
                let need_end_in_page = ((offset + want as u64).min(total_len) - page_start) as usize;
                if *keep < need_end_in_page {
                    return false;
                }
            }
        }
    }
    total_len != 0 && offset < total_len
}

/// The structured fuzz input; libFuzzer's raw bytes are parsed into this by `arbitrary`.
#[derive(Arbitrary, Debug)]
struct Input {
    data: Vec<u8>,
    page_size: u16,
    offset_kind: u8,
    offset_raw: u64,
    buf_len: u16,
    behaviors: Vec<(u8, u8, u16)>,
}

fuzz_target!(|input: Input| {
    // Bound the sizes so a single case stays small (disk is tight; the interesting bugs are at the
    // boundaries, not at scale). page_size in 1..=256.
    let page_size = (input.page_size % 256) as usize + 1;
    let mut data = input.data;
    data.truncate(8192);
    let total_len = data.len() as u64;
    let buf_len = (input.buf_len % 4096) as usize;

    // Offsets: a spread across in-range, just-past-EOF, and the u64 extremes that exercise the
    // past-EOF and overflow guards.
    let offset = match input.offset_kind % 5 {
        0 => input.offset_raw % (total_len + 1).max(1),
        1 => input.offset_raw % 8192,
        2 => u64::MAX,
        3 => u64::MAX - (input.offset_raw % 4096),
        _ => input.offset_raw,
    };

    let mut behaviors: HashMap<u64, Behavior> = HashMap::new();
    for (page_no, kind, param) in input.behaviors.into_iter().take(16) {
        let b = match kind % 4 {
            0 => Behavior::Normal,
            1 => Behavior::Missing,
            2 => Behavior::Oversize((param % 300) as usize),
            _ => Behavior::Short((param % 300) as usize),
        };
        behaviors.insert((page_no % 64) as u64, b);
    }

    let src = MockSource {
        page_size,
        data: data.clone(),
        behaviors,
    };

    let mut buf = vec![0xA5u8; buf_len];
    let result = serve_read(&src, total_len, offset, &mut buf);
    let clamp = expected_clamp(total_len, offset, buf_len);

    match result {
        Ok(n) => {
            // Invariant 2: never over-reports.
            assert!(n <= buf_len, "returned {n} > buffer {buf_len}");
            assert!(n <= clamp, "returned {n} > available {clamp}");
            // Invariant 3: never the wrong bytes.
            if n > 0 {
                let off = offset as usize;
                assert_eq!(&buf[..n], &data[off..off + n], "wrong bytes at offset {offset}");
            }
            // Invariant 4: a clean read must not short-change.
            if covered_range_is_all_clean(&src, total_len, offset, clamp) {
                assert_eq!(n, clamp, "clean read short-changed");
            }
        }
        Err(_) => {
            // Invariant 4 (other direction): a clean, in-bounds read must never error.
            assert!(
                !covered_range_is_all_clean(&src, total_len, offset, clamp),
                "clean read errored (offset {offset}, len {buf_len}, total {total_len})"
            );
        }
    }
});
