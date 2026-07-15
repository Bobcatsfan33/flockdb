//! The follower side: apply shipped commits and converge to the primary, byte for byte.
//!
//! A [`Replica`] wraps its own crash-durable [`DurableStore`] — a follower is a real, independently
//! recoverable database, not a cache — and applies [`Shipment`]s through substrate's ordinary commit
//! protocol under a [`ReplayClock`]. It re-derives each commit's manifest and refuses to advance if
//! it does not match the one the primary shipped.
//!
//! # The property this whole crate is judged on
//!
//! > A follower that has applied a prefix of the primary's WAL **equals** the primary at that same
//! > prefix.
//!
//! Not "is consistent with". *Equals* — the same manifest id, which (content addressing) means the
//! same bytes on every page. `tests/oracle.rs` asserts it differentially under thousands of
//! randomized commit sequences and randomized catch-up points, because a replica that is only
//! *almost* the primary is a replica that serves a wrong answer.

use crate::clock::ReplayClock;
use crate::error::{Result, SyncError};
use crate::shipment::Shipment;
use crate::source::WalSource;
use std::path::Path;
use std::sync::Arc;
use substrate_pager::{LogicalPageNo, ManifestId, Page, StoreConfig, Vfs};
use substrate_wal::{DurableStore, Lsn};

/// What applying a shipment did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The commit was applied and the replica advanced to a new head.
    Advanced,
    /// The replica was already at this commit's manifest; nothing changed. Idempotent re-delivery.
    AlreadyPresent,
}

/// A follower that applies a primary's shipped commits and serves consistent reads of them.
///
/// One `Replica` is a single writer of its own store: [`apply`](Self::apply) takes `&mut self` and
/// runs commits one at a time, which is what lets the [`ReplayClock`] be set-then-committed without
/// a race. Reads are taken against a pinned manifest (see [`read`](Self::read)), so a reader never
/// sees a half-applied commit even while a stream is being applied.
pub struct Replica {
    store: Arc<DurableStore>,
    clock: Arc<ReplayClock>,
    applied_lsn: Option<Lsn>,
}

impl Replica {
    /// Build a replica over an already-opened, already-recovered durable store and its replay clock.
    ///
    /// The store MUST have been opened with `this` clock
    /// ([`DurableStore::open_with_clock`](substrate_wal::DurableStore::open_with_clock)) and already
    /// [`recover`](substrate_wal::DurableStore::recover)ed, so that its head reflects whatever it had
    /// applied before the process last stopped. This constructor exists so a caller that also needs
    /// the store for something else — flock-core hands the same `Arc` to a SQL kernel — can share it.
    pub fn new(store: Arc<DurableStore>, clock: Arc<ReplayClock>) -> Self {
        Replica {
            store,
            clock,
            applied_lsn: None,
        }
    }

    /// Open (creating if absent) a follower store at `dir` and recover it.
    ///
    /// The convenience path for a follower that only replicates and does not also host a SQL engine.
    pub fn open(vfs: Arc<dyn Vfs>, dir: impl AsRef<Path>, config: StoreConfig) -> Result<Self> {
        let clock = Arc::new(ReplayClock::new());
        let store = Arc::new(
            DurableStore::open_with_clock(vfs, dir, config, Arc::clone(&clock) as Arc<_>)
                .map_err(SyncError::wal("open the follower store"))?,
        );
        store
            .recover()
            .map_err(SyncError::wal("recover the follower store"))?;
        Ok(Replica::new(store, clock))
    }

    /// The manifest the replica currently *is* — its durable, applied head.
    pub fn head(&self) -> ManifestId {
        self.store.head()
    }

    /// The primary LSN this replica has applied through in this process, if any.
    ///
    /// `None` until the first `apply`. This is an in-memory hint only — after a restart, ask the
    /// source where to resume with [`resume_after`](WalSource::resume_after), which recovers the
    /// position from the durable head rather than trusting a counter (substrate `CLAUDE.md` rule 9).
    pub fn applied_lsn(&self) -> Option<Lsn> {
        self.applied_lsn
    }

    /// The underlying durable store, for a caller that must read pages or share it with an engine.
    pub fn store(&self) -> &Arc<DurableStore> {
        &self.store
    }

    /// Read a page as of a pinned manifest — a whole, committed snapshot.
    ///
    /// Pin a manifest with [`head`](Self::head) and read every page of a query against *that* id.
    /// Because a commit installs its manifest with a single atomic head swap, a read pinned to a
    /// manifest is a manifest-consistent view: it can never observe some pages from before a commit
    /// and some from after.
    pub fn read(&self, manifest: &ManifestId, page_no: LogicalPageNo) -> Result<Page> {
        self.store
            .read(manifest, page_no)
            .map_err(SyncError::wal("read a page from the replica"))
    }

    /// Apply one shipped commit, advancing the replica to the primary's next state.
    ///
    /// Order of operations, and every one of them is load-bearing:
    ///
    /// 1. If the replica is already at this commit's manifest, it is a duplicate delivery — return
    ///    without touching the store. Idempotence, so a re-sent shipment is harmless.
    /// 2. If the replica's head is not this commit's `base`, refuse: a gap or a foreign log
    ///    ([`SyncError::OutOfOrder`]). Caught before anything is staged.
    /// 3. Stage each write, seeding the follower's CAS from the shipped bytes, and verify each byte
    ///    blob hashes to the id it claims ([`SyncError::PageHashMismatch`]).
    /// 4. Set the replay clock to the primary's timestamp and drive substrate's real commit.
    /// 5. Verify the manifest substrate derived equals the one the primary shipped
    ///    ([`SyncError::Diverged`]); the replica has advanced only if it does.
    pub fn apply(&mut self, shipment: &Shipment) -> Result<Applied> {
        let head = self.store.head();

        if head == shipment.manifest {
            self.applied_lsn = Some(match self.applied_lsn {
                Some(prev) => prev.max(shipment.commit_lsn),
                None => shipment.commit_lsn,
            });
            return Ok(Applied::AlreadyPresent);
        }
        if head != shipment.base {
            return Err(SyncError::OutOfOrder {
                lsn: shipment.commit_lsn,
                expected_base: shipment.base.to_hex(),
                head: head.to_hex(),
            });
        }

        let mut txn = self
            .store
            .begin()
            .map_err(SyncError::wal("begin the replica transaction"))?;

        for write in &shipment.writes {
            match write.page {
                Some(id) => {
                    let bytes = shipment
                        .bytes_for(id)
                        .ok_or_else(|| SyncError::MissingPage {
                            lsn: shipment.commit_lsn,
                            page_no: write.page_no,
                            page_id: id.to_hex(),
                        })?;
                    // `write` returns the content hash of the staged bytes. If it is not the id the
                    // shipment claims, the shipment is corrupt and we stop before it poisons the CAS.
                    let staged = self
                        .store
                        .write(&mut txn, write.page_no, bytes.to_vec())
                        .map_err(SyncError::wal("stage a replicated page"))?;
                    if staged != id {
                        return Err(SyncError::PageHashMismatch {
                            page_no: write.page_no,
                            claimed: id.to_hex(),
                            actual: staged.to_hex(),
                        });
                    }
                }
                None => {
                    self.store
                        .remove(&mut txn, write.page_no)
                        .map_err(SyncError::wal("stage a replicated removal"))?;
                }
            }
        }

        // The clock is set immediately before the commit that reads it. Because `apply` is `&mut`,
        // nothing runs between here and the commit that could set it to a different value.
        self.clock.set(shipment.created_at_ms);
        let got = self
            .store
            .commit(txn)
            .map_err(SyncError::wal("commit a replicated transaction"))?;

        if got != shipment.manifest {
            return Err(SyncError::Diverged {
                lsn: shipment.commit_lsn,
                expected: shipment.manifest.to_hex(),
                got: got.to_hex(),
            });
        }
        self.applied_lsn = Some(shipment.commit_lsn);
        Ok(Applied::Advanced)
    }

    /// Apply every commit the source has that this replica has not, in order.
    ///
    /// Resumes from the durable head (via [`WalSource::resume_after`]), so it is correct to call
    /// after a restart, after a crash, or repeatedly to poll for new commits. Returns how many
    /// commits it advanced through (duplicates already present do not count).
    pub fn catch_up(&mut self, source: &WalSource) -> Result<u64> {
        let from = source.resume_after(self.store.head())?;
        let mut advanced = 0;
        for shipment in source.shipments_from(from)? {
            if self.apply(&shipment)? == Applied::Advanced {
                advanced += 1;
            }
        }
        Ok(advanced)
    }

    /// **Point-in-time restore.** Replay up to `target_lsn` and stop.
    ///
    /// Applies every commit with LSN `<= target_lsn` that the replica does not already have, and no
    /// commit beyond it. When it returns, the replica's head equals the manifest the primary had as
    /// of `target_lsn` ([`WalSource::manifest_at`]) — an exact, reproducible state, because the
    /// manifest is content-addressed and the derivation deterministic. Restoring a fresh follower to
    /// the same `target_lsn` twice lands on the identical head both times.
    pub fn restore_to(&mut self, source: &WalSource, target_lsn: Lsn) -> Result<()> {
        let from = source.resume_after(self.store.head())?;
        for shipment in source.shipments_from(from)? {
            if shipment.commit_lsn > target_lsn {
                break;
            }
            self.apply(&shipment)?;
        }
        Ok(())
    }
}
