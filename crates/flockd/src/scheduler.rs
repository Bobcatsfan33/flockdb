//! The scheduler and the two pieces it needs: a way to *record* a query's fault set
//! ([`RecordingSource`]), and a place to *remember* it per database ([`WarmSet`], held by
//! [`WakeScheduler`]).
//!
//! Everything here is pure — no I/O of its own, no async, no `unwrap`/`expect`/`panic` (rule 2). It
//! decides *which pages to warm and when*; the faulting itself belongs to `PageSource`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use flock_vfs::{PageSource, Result};

/// Identifies a database whose warm set the scheduler tracks. A newtype so a database id can never be
/// swapped for some other string at a call site (the newtype-for-type-safety pattern).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DbId(pub String);

impl DbId {
    /// Wrap a database identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// The most pages a single database's warm set will hold.
///
/// A selective query's fault set is tiny (~21 pages, `docs/wake-latency.md`), so this cap is a
/// runaway guard, not a normal limit: a workload that faults its way past it (a full-scan masquerading
/// as a point query, or a pathological access pattern) has its warm set *bounded* rather than allowed
/// to grow until prewarming the "warm" set is just eager hydration under another name. When the cap is
/// hit, new pages are dropped and the fact is observable via [`WarmSet::saturated`]. 8192 four-KiB
/// pages is 32 MiB — comfortably above any real point-query set, well below "the whole database".
const WARM_SET_CAP: usize = 8192;

/// The set of pages a database's queries have been seen to fault — the set to prewarm on its next cold
/// wake. Held sorted and deduplicated so it is cheap to prefetch and trivial to persist later (it is
/// just a list of `u64`).
#[derive(Debug, Clone, Default)]
pub struct WarmSet {
    pages: Vec<u64>,
    saturated: bool,
}

impl WarmSet {
    /// An empty warm set — what a database that has never been woken starts with.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a query's fault trace into the set: union in the newly-touched pages, keeping the result
    /// sorted and deduplicated, up to [`WARM_SET_CAP`]. Idempotent — observing the same trace twice
    /// changes nothing, which is why re-warming a stable workload converges instead of growing.
    pub fn learn(&mut self, touched: &[u64]) {
        for &page in touched {
            if let Err(idx) = self.pages.binary_search(&page) {
                if self.pages.len() >= WARM_SET_CAP {
                    // Bounded on purpose: see WARM_SET_CAP. Drop the page rather than let the warm set
                    // grow into an eager hydrate; record that we did so.
                    self.saturated = true;
                    continue;
                }
                self.pages.insert(idx, page);
            }
        }
    }

    /// The pages to prewarm, sorted ascending.
    pub fn pages(&self) -> &[u64] {
        &self.pages
    }

    /// How many pages are in the set.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// Whether the set is empty (a database never yet observed).
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Whether the cap was ever hit — i.e. this database's observed fault set is larger than a warm set
    /// is meant to hold, so prewarming it is deliberately incomplete. A caller can surface this rather
    /// than silently under-warm.
    pub fn saturated(&self) -> bool {
        self.saturated
    }
}

/// A [`PageSource`] decorator that records every page faulted through it.
///
/// This is how the scheduler learns a warm set from *reality* instead of a heuristic: wrap the real
/// source, serve the query through the wrapper, and the wrapper has the exact set of pages the query
/// touched. It adds nothing to the read but an append under a lock, and it forwards
/// [`prefetch`](PageSource::prefetch) unchanged so wrapping a source never disables coalescing.
pub struct RecordingSource<S> {
    inner: S,
    touched: Mutex<Vec<u64>>,
}

impl<S> RecordingSource<S> {
    /// Wrap a source and begin recording.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            touched: Mutex::new(Vec::new()),
        }
    }

    /// The pages faulted so far, sorted and deduplicated — the query's fault set, ready to hand to
    /// [`WakeScheduler::observe`].
    pub fn touched(&self) -> Vec<u64> {
        // A poisoned lock means a reader panicked mid-record; the Vec is still a valid list of pages
        // (an append is atomic w.r.t. the lock), so recover it rather than propagate a panic (rule 2).
        let guard = self
            .touched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pages = guard.clone();
        pages.sort_unstable();
        pages.dedup();
        pages
    }

    /// Unwrap, returning the inner source.
    pub fn into_inner(self) -> S {
        self.inner
    }

    fn record(&self, page_no: u64) {
        let mut guard = self
            .touched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.push(page_no);
    }
}

impl<S: PageSource> PageSource for RecordingSource<S> {
    fn page_size(&self) -> usize {
        self.inner.page_size()
    }

    fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
        self.record(page_no);
        self.inner.read_page(page_no)
    }

    fn prefetch(&self, pages: &[u64]) {
        // A prewarm is also a fault of these pages; record it so a warm set stays accurate across
        // wakes even when the query is served entirely from a prewarmed cache (its reads would
        // otherwise not be seen as faults). Then forward to the inner source's real (coalesced) prefetch.
        for &page_no in pages {
            self.record(page_no);
        }
        self.inner.prefetch(pages);
    }
}

/// The wake scheduler: it remembers each database's warm set and, on a cold wake, coalesces that set
/// into a single concurrent prefetch before the query's serial reads arrive.
///
/// Construct one per process (it is `Send + Sync`; share it behind an `Arc`). It holds no I/O and no
/// tier — a caller wakes a store and builds a [`PageSource`], and hands it here to be warmed.
#[derive(Default)]
pub struct WakeScheduler {
    warm: Mutex<BTreeMap<DbId, WarmSet>>,
}

impl WakeScheduler {
    /// A scheduler with no learned warm sets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Prewarm a freshly-woken source with what this database's queries have taught us, **coalesced**:
    /// the warm set is handed to [`PageSource::prefetch`], which fans the faults out concurrently. Call
    /// this immediately after waking, before serving the query, so the query's serial reads land on
    /// resident pages. Returns the number of pages prewarmed (0 for a database never yet observed —
    /// the honest cold-start case, where there is nothing to warm and the query simply faults and
    /// *teaches* the set via [`observe`](Self::observe)).
    pub fn prewarm<S: PageSource>(&self, db: &DbId, source: &S) -> usize {
        let pages = {
            let guard = self
                .warm
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.get(db) {
                Some(set) => set.pages().to_vec(),
                None => Vec::new(),
            }
        };
        // Prefetch outside the lock: it blocks on the tier, and holding the warm-set lock across a
        // network fault would serialize every database's wake behind one slow bucket.
        source.prefetch(&pages);
        pages.len()
    }

    /// Fold a query's fault trace (from a [`RecordingSource`]) into this database's warm set, so the
    /// next wake can prewarm it. Idempotent for a stable workload.
    pub fn observe(&self, db: &DbId, touched: &[u64]) {
        let mut guard = self
            .warm
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.entry(db.clone()).or_default().learn(touched);
    }

    /// A snapshot of a database's current warm set (for persistence, inspection, or tests). Empty for
    /// an unknown database.
    pub fn warm_set(&self, db: &DbId) -> WarmSet {
        let guard = self
            .warm
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.get(db).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory `PageSource` that counts how many real faults it served — the same kind of trivial
    /// source the fuzzer uses, here used to check the scheduler's *logic* without a tier.
    struct CountingSource {
        page_size: usize,
        n_pages: u64,
        faults: Mutex<u64>,
    }

    impl PageSource for CountingSource {
        fn page_size(&self) -> usize {
            self.page_size
        }
        fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
            *self.faults.lock().unwrap() += 1;
            Ok(vec![
                (page_no & 0xff) as u8;
                self.page_size.min(self.n_pages as usize).max(1)
            ])
        }
    }

    #[test]
    fn warm_set_unions_and_dedupes_and_stays_sorted() {
        let mut set = WarmSet::new();
        set.learn(&[5, 1, 3]);
        set.learn(&[3, 1, 9]); // 3 and 1 are repeats
        assert_eq!(set.pages(), &[1, 3, 5, 9]);
        assert!(!set.saturated());
    }

    #[test]
    fn warm_set_is_bounded_and_reports_saturation() {
        let mut set = WarmSet::new();
        let many: Vec<u64> = (0..(WARM_SET_CAP as u64 + 100)).collect();
        set.learn(&many);
        assert_eq!(set.len(), WARM_SET_CAP, "the warm set must be capped");
        assert!(set.saturated(), "hitting the cap must be observable");
    }

    #[test]
    fn recording_source_captures_the_exact_fault_set() {
        let inner = CountingSource {
            page_size: 8,
            n_pages: 100,
            faults: Mutex::new(0),
        };
        let rec = RecordingSource::new(inner);
        for p in [7u64, 2, 7, 40, 2] {
            rec.read_page(p).unwrap();
        }
        assert_eq!(
            rec.touched(),
            vec![2, 7, 40],
            "sorted, deduplicated fault set"
        );
    }

    #[test]
    fn scheduler_learns_then_prewarms_the_learned_set() {
        let sched = WakeScheduler::new();
        let db = DbId::new("acme/ledger");

        // Cold start: nothing learned, prewarm does nothing.
        let cold = CountingSource {
            page_size: 8,
            n_pages: 100,
            faults: Mutex::new(0),
        };
        assert_eq!(
            sched.prewarm(&db, &cold),
            0,
            "an unknown database has nothing to warm"
        );

        // Serve a query through a recorder; teach the scheduler its fault set.
        let rec = RecordingSource::new(cold);
        for p in [3u64, 8, 15, 3] {
            rec.read_page(p).unwrap();
        }
        sched.observe(&db, &rec.touched());
        assert_eq!(sched.warm_set(&db).pages(), &[3, 8, 15]);

        // Next wake: prewarm now warms exactly that set, in one coalesced prefetch.
        let warm = CountingSource {
            page_size: 8,
            n_pages: 100,
            faults: Mutex::new(0),
        };
        assert_eq!(
            sched.prewarm(&db, &warm),
            3,
            "prewarm warms the learned set"
        );
        assert_eq!(
            *warm.faults.lock().unwrap(),
            3,
            "exactly the 3 learned pages were faulted"
        );
    }
}
