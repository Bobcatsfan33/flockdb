//! The page store behind a [`Db`](crate::Db) — and, crucially, **where the commit point is**.
//!
//! # The mistake this module exists to undo
//!
//! The first version of FlockDB used [`substrate_pager::Pager`] directly and hand-ordered the commit
//! protocol itself: pages into the CAS, install the manifest, *then* append a WAL record. That is
//! not substrate's protocol. Substrate's is:
//!
//! ```text
//!   1. page bytes → CAS, fsync         durable, but nothing references them yet
//!   2. WAL commit record, fsync        ◄── THE COMMIT POINT
//!   3. install the manifest            now readers can see it
//! ```
//!
//! Note where 2 and 3 sit relative to each other, and then note that we had them the other way
//! round. Our ordering was *probably* safe — a manifest installed before its commit record is
//! unreferenced garbage that GC sweeps, so the failure mode is a lost commit rather than a corrupt
//! one — but "probably safe, by my reading, with no crash-injection harness" is not a sentence
//! anybody should accept about a storage engine, and it was ours.
//!
//! [`substrate_wal::DurableStore`] implements that protocol, and `testing/fuzz` earns it: **50,000
//! randomized crash-and-recover cycles**, killing the write path at every byte boundary in turn. It
//! found three real bugs. It is not a thing to reimplement for fun.
//!
//! So: we do not order the commit. `DurableStore` does. This module is the adapter that lets the
//! kernel — which knows only [`PageStore`] — commit through it.
//!
//! # Two kinds of durable
//!
//! A [`Db`](crate::Db) is backed by one of two stores, and **they have different commit points**.
//! This is not an implementation detail; it is the durability contract, and it differs:
//!
//! | Store | The commit point is… | Set up by |
//! | --- | --- | --- |
//! | [`Store::Local`] | an **fsync'd WAL record** on local disk | [`Flock::open`](crate::Flock::open) |
//! | [`Store::Tiered`] | **pages + manifests confirmed in object storage** | [`Flock::wake`](crate::Flock::wake) |
//!
//! A tiered store has no WAL — substrate does not offer a durable-*and*-tiered store, and we are not
//! going to invent one. Its durability is object storage, which is why [`Db::snapshot`](crate::Db::snapshot)
//! on a woken database pushes to object storage before it returns, and says so.

use std::sync::Arc;
use substrate_pager::{
    GcStats, LogicalPageNo, Manifest, ManifestId, Page, PageDiff, PageId, PageMap, PageStore,
    Pager, Result as PagerResult, ThreeWayDiff, Txn,
};
use substrate_store::TieredStore;
use substrate_wal::DurableStore;

/// Which store a database is sitting on.
///
/// Deliberately an enum and not a trait object: the two have genuinely different durability
/// contracts (see the module docs), and a trait would paper over that by making them look
/// interchangeable at every call site. They are not interchangeable. They are two answers to
/// "what does it mean that this commit returned `Ok`".
pub(crate) enum Store {
    /// Local disk, crash-durable through substrate's WAL.
    Local(Arc<DurableStore>),
    /// Object storage, with local disk as a cache. Durable when the bytes are in the bucket.
    Tiered(Arc<TieredStore>),
}

impl Store {
    /// The pager underneath, for the operations that are pure page algebra — fork, diff, resolve.
    ///
    /// These do not commit, so they do not need the protocol, so they can go straight to the pager.
    pub(crate) fn pager(&self) -> &Arc<Pager> {
        match self {
            Store::Local(d) => d.pager(),
            Store::Tiered(t) => t.pager(),
        }
    }

    /// The current head — the manifest this database *is*.
    pub(crate) fn head(&self) -> ManifestId {
        self.pager().head()
    }

    /// A [`PageStore`] view for the kernel, whose `commit` goes through the real protocol.
    ///
    /// The kernel must not be handed the raw `Pager`. `Pager::commit` installs a manifest and writes
    /// no log record, so a database that committed through it would come back from a crash at
    /// whatever head it had when it was last *opened* — silently discarding every snapshot taken
    /// since. That is the bug this whole module is a fix for.
    pub(crate) fn page_store(&self) -> Arc<dyn PageStore> {
        match self {
            Store::Local(d) => Arc::new(DurableHandle(Arc::clone(d))),
            Store::Tiered(t) => Arc::new(TieredHandle(Arc::clone(t))),
        }
    }
}

/// A [`PageStore`] whose `commit` is [`DurableStore::commit`] — i.e. substrate's commit protocol.
struct DurableHandle(Arc<DurableStore>);

impl DurableHandle {
    fn pager_ref(&self) -> &Pager {
        self.0.pager()
    }
}

/// A [`PageStore`] over a tiered store. Commits reach object storage via the background uploader;
/// [`Db::snapshot`](crate::Db::snapshot) is what waits for them.
struct TieredHandle(Arc<TieredStore>);

impl TieredHandle {
    fn pager_ref(&self) -> &Pager {
        self.0.pager()
    }
}

/// Delegate the page algebra to the pager, and nothing else.
///
/// Every method here is read-only or pure — none of them is the commit point — so going straight to
/// the pager is correct. The macro exists so that the ONE method that is different, `commit`, is
/// impossible to miss when reading either impl: it is the only one written out by hand.
macro_rules! delegate_to_pager {
    () => {
        fn read(&self, manifest: &ManifestId, page_no: LogicalPageNo) -> PagerResult<Page> {
            self.pager_ref().read(manifest, page_no)
        }
        fn read_head(&self, page_no: LogicalPageNo) -> PagerResult<Page> {
            self.pager_ref().read_head(page_no)
        }
        fn begin(&self) -> PagerResult<Txn> {
            self.pager_ref().begin()
        }
        fn write(
            &self,
            txn: &mut Txn,
            page_no: LogicalPageNo,
            bytes: Vec<u8>,
        ) -> PagerResult<PageId> {
            self.pager_ref().write(txn, page_no, bytes)
        }
        fn remove(&self, txn: &mut Txn, page_no: LogicalPageNo) -> PagerResult<()> {
            self.pager_ref().remove(txn, page_no)
        }
        fn head(&self) -> ManifestId {
            self.pager_ref().head()
        }
        fn snapshot(&self) -> PagerResult<ManifestId> {
            self.pager_ref().snapshot()
        }
        fn fork(&self, from: &ManifestId) -> PagerResult<Box<dyn PageStore>> {
            self.pager_ref().fork(from)
        }
        fn rewind(&self, to: &ManifestId) -> PagerResult<()> {
            self.pager_ref().rewind(to)
        }
        fn diff(&self, a: &ManifestId, b: &ManifestId) -> PagerResult<PageDiff> {
            self.pager_ref().diff(a, b)
        }
        fn diff3(
            &self,
            base: &ManifestId,
            a: &ManifestId,
            b: &ManifestId,
        ) -> PagerResult<ThreeWayDiff> {
            self.pager_ref().diff3(base, a, b)
        }
        fn gc(&self, live: &[ManifestId]) -> PagerResult<GcStats> {
            self.pager_ref().gc(live)
        }
        fn manifest(&self, id: &ManifestId) -> PagerResult<Manifest> {
            self.pager_ref().manifest(id)
        }
        fn resolve(&self, id: &ManifestId) -> PagerResult<PageMap> {
            self.pager_ref().resolve(id)
        }
        fn lookup(&self, id: &ManifestId, page_no: LogicalPageNo) -> PagerResult<Option<PageId>> {
            self.pager_ref().lookup(id, page_no)
        }
        fn merge_base(&self, a: &ManifestId, b: &ManifestId) -> PagerResult<Option<ManifestId>> {
            self.pager_ref().merge_base(a, b)
        }
        fn page_size(&self) -> usize {
            self.pager_ref().page_size()
        }
        fn pool(&self) -> &str {
            self.pager_ref().pool()
        }
    };
}

impl PageStore for DurableHandle {
    delegate_to_pager!();

    /// **The commit point.** Substrate's, not ours.
    ///
    /// `DurableStore::commit` fsyncs a CRC-protected record *before* installing the manifest, which
    /// is what makes a crash land on a transaction boundary instead of inside one. We do not
    /// reorder it, we do not "optimise" it, and we do not add a step.
    fn commit(&self, txn: Txn) -> PagerResult<ManifestId> {
        self.0
            .commit(txn)
            .map_err(substrate_pager::PagerError::backend)
    }
}

impl PageStore for TieredHandle {
    delegate_to_pager!();

    /// Commit into the tiered store.
    ///
    /// There is no WAL here. The pages are in the local cache and queued for upload, and the commit
    /// is durable once they land in object storage — which is what [`Db::snapshot`](crate::Db::snapshot)
    /// waits for on a woken database. Between this returning and that upload completing, the data is
    /// exactly as durable as the local disk, and we say so rather than letting the word "commit"
    /// imply more than it is worth.
    fn commit(&self, txn: Txn) -> PagerResult<ManifestId> {
        self.0.pager().commit(txn)
    }
}
