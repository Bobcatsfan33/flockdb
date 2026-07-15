//! A **read replica**: a follower that applies a primary's shipped commits and answers SQL against
//! them at a consistent snapshot.
//!
//! # What this adds over `flock_sync::Replica`
//!
//! `flock_sync::Replica` converges a follower's *page store* to the primary. A [`ReadReplica`] puts
//! a DuckDB engine on top of it, so the follower answers queries — the point of a read replica being
//! that reads scale out onto followers while writes stay on the one primary.
//!
//! # The snapshot a query sees, and why it advances only on `refresh`
//!
//! The kernel is hydrated from **one manifest** — a whole, committed snapshot. Applying shipped
//! commits advances the follower's durable head, but **not** the manifest the kernel is showing:
//! reads keep seeing the snapshot they were hydrated from until [`refresh`](ReadReplica::refresh)
//! re-hydrates the engine at the new head. So a query is always a manifest-consistent view and never
//! a half-applied one, and the moment its data jumps forward is a moment you choose, not one that
//! happens under a running query.
//!
//! This mirrors [`Db::restore`](crate::Db::restore), which re-hydrates the same way: "which version
//! of the database am I looking at" stays in exactly one place (the kernel's hydration point), which
//! is the rule the `flock-kernel` docs insist on.
//!
//! # It is eventually consistent, and reads are read-only
//!
//! A `ReadReplica` lags the primary by whatever it has not pulled and re-hydrated. It exposes no
//! `execute`: a follower does not take writes. Promoting one to a writable primary is a manual
//! operation and is not automated here — see `flock_sync`'s consistency-model notes for the honest
//! account of failover and split-brain.

use crate::error::{FlockError, Result};
use crate::pool::{link_db_to_shared_cas, validate_name};
use crate::store::Store;
use flock_kernel::{ArrowStream, DuckDbKernel, KernelOpts, SqlKernel};
use flock_sync::{ReplayClock, Replica, Shipment, WalSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use substrate_pager::{std_vfs, Clock, ManifestId, StoreConfig};
use substrate_wal::{DurableStore, Lsn};

/// A follower database that serves SQL reads of a replicated primary.
pub struct ReadReplica {
    name: String,
    root: PathBuf,
    replica: Replica,
    store: Store,
    kernel: DuckDbKernel,
    /// The manifest the kernel is currently showing — the snapshot queries see.
    visible: ManifestId,
    opts: KernelOpts,
}

impl std::fmt::Debug for ReadReplica {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadReplica")
            .field("name", &self.name)
            .field("root", &self.root)
            .field("applied_head", &self.replica.head())
            .field("visible", &self.visible)
            .finish_non_exhaustive()
    }
}

impl ReadReplica {
    /// Open (creating if absent) a read replica named `db_name` under `root`, and recover it.
    ///
    /// `root` is the follower's *own* storage — its own pool, its own CAS — not the primary's. A
    /// follower that already applied commits before a restart comes back at its durable head, ready
    /// to keep following.
    pub fn open(root: impl AsRef<Path>, db_name: &str) -> Result<ReadReplica> {
        ReadReplica::open_with(root, db_name, KernelOpts::default())
    }

    /// [`ReadReplica::open`], with control over the SQL engine's threads and memory.
    pub fn open_with(
        root: impl AsRef<Path>,
        db_name: &str,
        opts: KernelOpts,
    ) -> Result<ReadReplica> {
        let root = root.as_ref().to_path_buf();
        validate_name(db_name)?;
        let dir = link_db_to_shared_cas(&root, db_name)?;

        // A replay clock, and a durable store opened on it, so applied commits reproduce the
        // primary's manifests byte-for-byte (see `flock_sync::ReplayClock`).
        let clock = Arc::new(ReplayClock::new());
        let durable = Arc::new(
            DurableStore::open_with_clock(
                std_vfs(),
                &dir,
                StoreConfig::default(),
                Arc::clone(&clock) as Arc<dyn Clock>,
            )
            .map_err(FlockError::wal("open the replica's durable store"))?,
        );
        durable
            .recover()
            .map_err(FlockError::wal("replay the replica's log"))?;

        // The same Arc backs both the SQL kernel (via `Store::Local`) and the sync replica.
        let store = Store::Local(Arc::clone(&durable));
        let kernel = DuckDbKernel::open(store.page_store(), opts.clone())?;
        let visible = store.head();
        let replica = Replica::new(durable, clock);

        Ok(ReadReplica {
            name: db_name.to_string(),
            root,
            replica,
            store,
            kernel,
            visible,
            opts,
        })
    }

    /// The replica's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The manifest the follower has durably applied through — which may be ahead of what queries
    /// see until the next [`refresh`](Self::refresh).
    pub fn applied_head(&self) -> ManifestId {
        self.replica.head()
    }

    /// The manifest queries currently see — the hydrated snapshot.
    pub fn visible_head(&self) -> ManifestId {
        self.visible
    }

    /// The primary LSN applied through in this process, if any.
    pub fn applied_lsn(&self) -> Option<Lsn> {
        self.replica.applied_lsn()
    }

    /// Apply one shipped commit. Advances the follower's durable head; does **not** move the snapshot
    /// queries see — call [`refresh`](Self::refresh) for that.
    pub fn apply(&mut self, shipment: &Shipment) -> Result<()> {
        self.replica
            .apply(shipment)
            .map(|_| ())
            .map_err(FlockError::sync("apply a shipped commit"))
    }

    /// Pull every commit `source` has that this follower does not, and apply them. Returns how many
    /// commits it advanced. Does not refresh the visible snapshot.
    pub fn catch_up(&mut self, source: &WalSource) -> Result<u64> {
        self.replica
            .catch_up(source)
            .map_err(FlockError::sync("catch up to the primary"))
    }

    /// **Point-in-time restore** to `target_lsn`: apply up to it and stop. Refreshes the visible
    /// snapshot to the restored state, because a restore is a deliberate "show me it as of then".
    pub fn restore_to(&mut self, source: &WalSource, target_lsn: Lsn) -> Result<()> {
        self.replica
            .restore_to(source, target_lsn)
            .map_err(FlockError::sync("restore to a point in time"))?;
        self.refresh()
    }

    /// Advance the snapshot queries see to the follower's current applied head.
    ///
    /// Re-hydrates the SQL engine from the new head. Assigning the new kernel rather than mutating in
    /// place means a failure leaves the old kernel — and the old, whole snapshot — usable.
    pub fn refresh(&mut self) -> Result<()> {
        let kernel = DuckDbKernel::open(self.store.page_store(), self.opts.clone())?;
        self.kernel = kernel;
        self.visible = self.store.head();
        Ok(())
    }

    /// Run a read query against the visible snapshot. Results come back as Arrow.
    ///
    /// There is deliberately no `execute`: a read replica does not take writes.
    pub fn query(&mut self, sql: &str) -> Result<ArrowStream> {
        Ok(self.kernel.query(sql)?)
    }
}
