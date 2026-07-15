//! The clock a follower replays through.
//!
//! # Why a follower needs a clock it controls
//!
//! Substrate bakes a wall-clock timestamp into every manifest, and a manifest id is the hash of its
//! bytes — timestamp included. So two commits with the same base and the same page writes but
//! *different* timestamps are two different manifests with two different ids.
//!
//! A follower must reproduce the primary's manifests **byte for byte** (that is the whole product:
//! a replica that is only *almost* the primary is a replica that serves a wrong answer). The
//! primary ships the exact `created_at_ms` it committed with; the follower installs a
//! [`ReplayClock`], sets it to that value immediately before replaying the commit, and then drives
//! the commit through substrate's ordinary [`DurableStore::commit`](substrate_wal::DurableStore::commit)
//! path — which reads the clock once and bakes it in. The result is the identical manifest, produced
//! by the identical, fuzz-tested commit protocol rather than by a second copy of it written here.
//!
//! This is the boring choice on purpose (CLAUDE.md rule 7). The clever alternative — reaching past
//! `DurableStore` into `Pager::derive_next` / `install` and hand-ordering the follower's commit —
//! would re-implement the one piece of substrate that survives 50,000 crash-injection cycles, so
//! that a follower could do without a settable clock. Not worth it.

use std::sync::atomic::{AtomicU64, Ordering};
use substrate_pager::Clock;

/// A [`Clock`] whose value is set explicitly before each replayed commit.
///
/// One `Replica` drives one `ReplayClock`, and applies commits one at a time under `&mut self`, so
/// the set-then-commit sequence is never interleaved. The `AtomicU64` is there so the type can be
/// shared as `Arc<dyn Clock>` with the store without a `Mutex`, not because two threads race on it.
#[derive(Debug, Default)]
pub struct ReplayClock {
    now_ms: AtomicU64,
}

impl ReplayClock {
    /// A new clock reading zero. The first `set` before the first apply gives it a real value.
    pub fn new() -> Self {
        ReplayClock {
            now_ms: AtomicU64::new(0),
        }
    }

    /// Set the timestamp the next commit will bake into its manifest.
    ///
    /// Call this with the primary's `created_at_ms` for the commit about to be replayed, and the
    /// manifest the follower derives will be byte-identical to the primary's.
    pub fn set(&self, now_ms: u64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for ReplayClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}
