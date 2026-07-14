//! [`Flock`] and [`Db`] — the public API from docs/02 §5.3.

use crate::error::{FlockError, Result};
use crate::pool::{db_dir, link_db_to_shared_cas, validate_name};
use crate::store::Store;
use flock_kernel::{ArrowStream, DuckDbKernel, KernelOpts, SqlKernel};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use substrate_pager::{std_vfs, ManifestId, PageStore, StoreConfig};
use substrate_store::{RemoteTier, TieredStore, WakeToken};
use substrate_wal::DurableStore;

/// A pool of databases sharing one content-addressed store.
///
/// A pool is a **boundary, not a namespace** (docs/02 §9.1). Two databases in the same pool share
/// pages — which is what makes ten thousand forks of one template cost one template — and two
/// databases in *different* pools never share a page even when their bytes are identical, so that
/// data cannot cross a classification boundary through the storage layer. There is no setting that
/// turns that off.
pub struct Flock;

impl Flock {
    /// Open (or create) the database `db_name` in the pool rooted at `root`.
    ///
    /// Creating a database is a directory and an empty manifest — there is nothing to "provision",
    /// which is the point of the whole design (docs/02 §1.1: *databases are cheap to have*).
    ///
    /// Opening an existing one replays its write-ahead log, which restores the head it had at its
    /// last [`Db::snapshot`]. Anything written after that snapshot and before the process died is
    /// gone: see [`Db::snapshot`] for exactly why, and the `flock-kernel` crate docs for the honest
    /// version of the durability boundary.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), flock_core::FlockError> {
    /// use flock_core::Flock;
    ///
    /// let mut sales = Flock::open("/var/lib/flock/tenants", "acme")?;
    /// sales.execute("CREATE TABLE t (region TEXT, amount DOUBLE)")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(root: impl AsRef<Path>, db_name: &str) -> Result<Db> {
        Flock::open_with(root, db_name, KernelOpts::default())
    }

    /// [`Flock::open`], with control over the SQL engine's threads and memory.
    ///
    /// This exists chiefly so that the TPC-H benchmark can give FlockDB and raw DuckDB *identical*
    /// engine settings. A comparison in which one side quietly gets more threads than the other is
    /// not a measurement, and we do not ship numbers we cannot reproduce (docs/04 §5).
    pub fn open_with(root: impl AsRef<Path>, db_name: &str, opts: KernelOpts) -> Result<Db> {
        let root = root.as_ref().to_path_buf();
        validate_name(db_name)?;

        // The database's own directory: a private WAL, and `pages`/`manifests` symlinked into the
        // pool's one shared CAS. See `pool.rs` for why that indirection has to exist.
        let dir = link_db_to_shared_cas(&root, db_name)?;

        let durable = Arc::new(
            DurableStore::open(std_vfs(), &dir, StoreConfig::default())
                .map_err(FlockError::wal("open the durable store"))?,
        );

        // Replay. This restores the head this database had at its last commit — or, for a database
        // that does not exist yet, leaves it at the canonical empty root.
        //
        // It must happen BEFORE the kernel opens, because the kernel hydrates its scratch file from
        // whatever the head is at the moment it is constructed. Open the kernel first and every
        // database in the pool comes up empty, and only the *second* run of the program notices.
        durable
            .recover()
            .map_err(FlockError::wal("replay the log"))?;

        let store = Store::Local(durable);
        let kernel = DuckDbKernel::open(store.page_store(), opts.clone())?;

        Ok(Db {
            name: db_name.to_string(),
            root,
            store,
            kernel,
            opts,
        })
    }

    /// **Wake a sleeping database** out of object storage.
    ///
    /// `cache_root` is a *cache*, not a home: it starts empty and fills with the pages the database
    /// actually reads. The data's home is the bucket.
    ///
    /// # This must run on a multi-threaded tokio runtime
    ///
    /// `#[tokio::main]`, or `#[tokio::test(flavor = "multi_thread")]`. Not the current-thread
    /// runtime, and this is not a preference. Substrate's page read path is *synchronous* (so that
    /// crash injection can be deterministic), so a cache miss blocks the calling thread on an async
    /// fetch. On a current-thread runtime that is the executor's only thread, and it deadlocks —
    /// silently, with no error, forever.
    ///
    /// # What waking actually costs in F1, which is not what docs/02 §7 hoped
    ///
    /// Substrate wakes **lazily**: fetch the manifest, then fetch pages only as queries touch them.
    /// That is what makes a 100 GB database wake in 250 ms without moving 100 GB.
    ///
    /// **FlockDB cannot use that**, and the reason is the same one behind every other limitation
    /// here: DuckDB needs a *file*. Before it can answer a single query the kernel must reconstruct
    /// the whole database file, which reads **every page**, which faults in **the entire database
    /// from object storage**. Waking a 100 GB FlockDB database moves 100 GB.
    ///
    /// The code below is nonetheless written against the lazy API, because it is correct and because
    /// the day DuckDB exposes a filesystem hook, this method becomes fast without being rewritten.
    /// Today it is honest to call it *"restore from object storage"* and not *"wake"*.
    pub async fn wake(
        cache_root: impl AsRef<Path>,
        db_name: &str,
        remote: RemoteTier,
        token: &WakeToken,
    ) -> Result<Db> {
        Flock::wake_with(cache_root, db_name, remote, token, KernelOpts::default()).await
    }

    /// [`Flock::wake`], with control over the SQL engine's threads and memory.
    pub async fn wake_with(
        cache_root: impl AsRef<Path>,
        db_name: &str,
        remote: RemoteTier,
        token: &WakeToken,
        opts: KernelOpts,
    ) -> Result<Db> {
        let root = cache_root.as_ref().to_path_buf();
        validate_name(db_name)?;
        std::fs::create_dir_all(&root).map_err(FlockError::pool("create cache root", &root))?;

        let tiered = TieredStore::wake(&root, remote, token)
            .await
            .map_err(FlockError::tier("wake"))?;

        let store = Store::Tiered(Arc::new(tiered));

        // Hydration reads every page, and every one of them is a cache miss against the bucket. It
        // blocks. On a multi-threaded runtime that costs one worker thread for the duration, which
        // is exactly what substrate's own synchronous read path does on a cache miss — so this is
        // consistent with the engine's design rather than a corner we cut.
        let kernel = DuckDbKernel::open(store.page_store(), opts.clone())?;

        Ok(Db {
            name: db_name.to_string(),
            root,
            store,
            kernel,
            opts,
        })
    }
}

/// One database.
///
/// The handle owns a SQL engine and a position in the manifest DAG. Everything interesting about
/// FlockDB is in what the last four methods cost:
///
/// | Method | Cost in substrate | Cost in the kernel (F1) |
/// | --- | --- | --- |
/// | [`snapshot`](Db::snapshot) | O(1) — remember a 32-byte id | O(database) — the file→pages sync |
/// | [`fork`](Db::fork) | O(1) — a new manifest, no bytes copied | O(database) — rehydrate a scratch file |
/// | [`restore`](Db::restore) | O(1) — move a pointer | O(database) — rehydrate a scratch file |
/// | [`sleep`](Db::sleep) | O(database) — upload | O(database) — one last sync |
///
/// The left column is the architecture. **The right column is the price of the fallback**, and it is
/// the price a DuckDB filesystem hook would abolish — see the `flock-kernel` crate docs for why that
/// hook is not reachable from Rust today.
pub struct Db {
    name: String,
    root: PathBuf,
    store: Store,
    kernel: DuckDbKernel,
    opts: KernelOpts,
}

/// Names the database and where it is, and *nothing else*.
///
/// Deliberately not derived. A derived `Debug` would reach into the store and print pages, and one
/// day someone would log a `Db` at debug level and put a page of a customer's data into a log
/// aggregator. The fields below are the ones a human debugging a fleet actually wants, and none of
/// them is data.
impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("name", &self.name)
            .field("root", &self.root)
            .field(
                "durability",
                match self.store {
                    Store::Local(_) => &"local WAL",
                    Store::Tiered(_) => &"object storage",
                },
            )
            .field("head", &self.store.head())
            .finish_non_exhaustive()
    }
}

impl Db {
    /// The database's name within its pool.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The pool this database lives in — or, for a woken database, its local page cache.
    pub fn pool_root(&self) -> &Path {
        &self.root
    }

    /// The manifest this database currently *is*.
    pub fn head(&self) -> ManifestId {
        self.store.head()
    }

    /// Run a query. Results come back as Arrow (docs/02 §6.1).
    ///
    /// See [`ArrowStream`] for the one thing that is not yet true about the name: F1 materialises the
    /// whole result rather than streaming it.
    pub fn query(&mut self, sql: &str) -> Result<ArrowStream> {
        Ok(self.kernel.query(sql)?)
    }

    /// Run a statement for its effect. Returns the number of rows it changed.
    ///
    /// **This does not make anything durable.** The write lands in DuckDB's scratch file; substrate
    /// hears about it at the next [`snapshot`](Db::snapshot). That boundary is the fallback's, it is
    /// real, and it is spelled out in full in the `flock-kernel` crate docs.
    pub fn execute(&mut self, sql: &str) -> Result<u64> {
        Ok(self.kernel.execute(sql)?)
    }

    /// Run several statements separated by `;`.
    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.kernel.execute_batch(sql)?)
    }

    /// Take a snapshot. **This is the commit point.**
    ///
    /// Returns a [`ManifestId`]: a 32-byte content hash that *is* the complete state of this database
    /// at this instant. Hand it to [`restore`](Db::restore) at any point in the future and you are
    /// back here exactly.
    ///
    /// # What happens, in the order it happens
    ///
    /// ```text
    ///   1. DuckDB folds its own WAL into its main file
    ///   2. the file is chunked into pages; changed pages go to the CAS and are fsync'd
    ///   3. a CRC-protected commit record is fsync'd to substrate's WAL   ← THE COMMIT POINT
    ///   4. the manifest is installed, and readers can see it
    /// ```
    ///
    /// **Steps 3 and 4 are in that order, and it matters.** FlockDB does not choose that order or
    /// implement it: [`substrate_wal::DurableStore`] does, and substrate's fuzz suite holds it to it
    /// across 50,000 randomized crash-and-recover cycles that kill the write path at every byte
    /// boundary in turn. A crash before step 3 leaves durable pages that nothing references —
    /// garbage, which GC sweeps — and the database opens at the previous snapshot, whole. A crash
    /// between 3 and 4 is a commit that happened: recovery re-derives the identical manifest from the
    /// log and installs it. There is no state in between.
    ///
    /// (An earlier version of FlockDB ordered these itself, and had them the other way round. See the
    /// `store` module for what was wrong with that and why it is not the sort of thing to eyeball.)
    ///
    /// # On a woken database, the commit point is somewhere else
    ///
    /// A database restored by [`Flock::wake`] has **no local WAL** — its durability is the bucket.
    /// `snapshot()` still returns a manifest id, and the pages are queued for upload, but *durable*
    /// means *in object storage*, and that has not necessarily happened yet when this returns. Call
    /// [`ensure_durable`](Db::ensure_durable) to wait for it. We would rather hand you a second
    /// method than let the word "commit" quietly mean two different things.
    ///
    /// # And the part that is *not* free
    ///
    /// The snapshot itself is O(1) — substrate measures it at 15 ns. Step 2 is not: it reads the
    /// whole database file. Snapshots in F1 are cheap in *manifests* and linear in *I/O*. Measured at
    /// 33 ms on a 26 MiB database. Do not take one per row.
    pub fn snapshot(&mut self) -> Result<ManifestId> {
        // The kernel commits through the store's `PageStore`, whose `commit` IS `DurableStore::commit`
        // for a local database — the real protocol, fsync'd log record and all. There is deliberately
        // no second WAL call here: the old code took a manifest id from `Pager::commit` (which writes
        // no log record) and then called `Wal::checkpoint`, which is not a commit — it persists the
        // head and *truncates the log behind it*. It worked by accident.
        Ok(self.kernel.checkpoint()?)
    }

    /// Wait until this database is durable in object storage. Tiered databases only.
    ///
    /// On a local database this is a no-op that returns immediately: a local database is already
    /// durable the moment [`snapshot`](Db::snapshot) returns, because its commit point is an fsync'd
    /// WAL record. Calling it anyway is harmless and lets fleet code stay uniform.
    pub async fn ensure_durable(&self) -> Result<()> {
        match &self.store {
            Store::Local(_) => Ok(()),
            Store::Tiered(t) => t
                .ensure_durable(&[t.pager().head()])
                .await
                .map_err(FlockError::tier("make durable in object storage")),
        }
    }

    /// Fork this database. **The headline property, and the one worth being blunt about.**
    ///
    /// The fork is a *real, separate database*. It shares every page with its parent by construction
    /// and copies none of them. Then:
    ///
    /// > **A write to the fork is never, under any circumstance, visible in the parent.**
    ///
    /// That is not enforced by a check we perform, which is a much weaker claim than it sounds. It is
    /// **structural**: a manifest is an immutable value, the fork holds a different one, and there is
    /// no code path — no bug, no race, no `WHERE` clause someone forgot — that could make the parent
    /// read the fork's bytes. `fork_isolation.rs` in this crate's tests asserts it at the SQL level,
    /// as bluntly as it can be written.
    ///
    /// # This checkpoints the parent first, and it must
    ///
    /// The parent's uncommitted writes live in a DuckDB scratch file substrate has never seen. A fork
    /// that did not checkpoint first would branch from the parent's *last snapshot*, silently dropping
    /// everything the caller has done since — and the caller, who has just watched their `INSERT`s
    /// succeed, would have no reason to suspect it.
    ///
    /// # What it costs
    ///
    /// O(1) in substrate — 98 ns, flat, no bytes copied. **O(database size) in the kernel**, because
    /// the fork needs its own DuckDB connection and DuckDB needs a file, so we hydrate one. The
    /// isolation is free; the file is not. See the `flock-kernel` crate docs.
    pub fn fork(&mut self, name: &str) -> Result<Db> {
        validate_name(name)?;

        let dir = db_dir(&self.root, name);
        if dir.exists() {
            // We will not adopt it, and adopting it is the *dangerous* option: recovery would set the
            // fork's head to that other database's manifest, and `fork` would return a handle full of
            // someone else's rows while reporting complete success.
            return Err(FlockError::NameTaken {
                name: name.to_string(),
                path: dir,
            });
        }

        let head = self.snapshot()?;

        // The fork's own directory — its own WAL, symlinked onto the same shared CAS as its parent.
        // *That* is the fork: same pages, different head. Nothing is copied.
        let dir = link_db_to_shared_cas(&self.root, name)?;
        let forked = Arc::new(
            DurableStore::open(std_vfs(), &dir, StoreConfig::default())
                .map_err(FlockError::wal("open the fork's durable store"))?,
        );
        forked
            .recover()
            .map_err(FlockError::wal("replay the fork's log"))?;

        // Position the fork at its parent's snapshot, and make that durable.
        //
        // `checkpoint()` is the right call here and the wrong call in `snapshot()` — it persists the
        // current head so recovery starts from it. Without it, the fork's log is empty, recovery
        // resets it to the empty root manifest, and reopening the fork after a restart hands back an
        // EMPTY database. That is caught only by a test that forks a non-empty database and then
        // reopens it, which is why there is one.
        forked
            .pager()
            .set_head_to(head)
            .map_err(FlockError::pager("position the fork at its parent"))?;
        forked
            .checkpoint()
            .map_err(FlockError::wal("record the fork's head"))?;

        let store = Store::Local(forked);
        let kernel = DuckDbKernel::open(store.page_store(), self.opts.clone())?;

        Ok(Db {
            name: name.to_string(),
            root: self.root.clone(),
            store,
            kernel,
            opts: self.opts.clone(),
        })
    }

    /// Go back to a snapshot. O(1) in substrate: a pointer moves.
    ///
    /// **Everything written since the snapshot is discarded**, including work that has not been
    /// snapshotted at all. That is what "restore" means, it is not recoverable through this API, and
    /// this sentence is the only warning you get — so take a [`snapshot`](Db::snapshot) first if you
    /// might want the current state back. Snapshots are cheap and this is exactly what they are for.
    ///
    /// The old state is *not destroyed*: it is still a manifest, still content-addressed, still
    /// restorable if you kept its id. Nothing is deleted until GC runs and finds no live manifest
    /// referencing it.
    pub fn restore(&mut self, manifest: ManifestId) -> Result<()> {
        self.rewind_to(manifest)?;
        // Rebuilding the kernel rehydrates the scratch file from the restored head, and drops the old
        // connection — and with it, the old scratch file. Assigning here rather than mutating in place
        // means a failure leaves the previous kernel untouched and usable.
        self.kernel = DuckDbKernel::open(self.store.page_store(), self.opts.clone())?;
        Ok(())
    }

    /// **Put the database to sleep in object storage**, and hand back the pointer that is all that
    /// remains of it.
    ///
    /// Every page and the manifest's whole ancestry go to the bucket; the SQL engine, its threads and
    /// its scratch file are released. What is left is a [`WakeToken`] — a pool, a 32-byte manifest id,
    /// and a page size. That is the entire database, as far as anything else is concerned, which is
    /// why a million sleeping databases fit in a registry on a laptop (docs/02 §9.3).
    ///
    /// Consumes the handle, because after this there is no database in memory.
    ///
    /// # It snapshots first
    ///
    /// So nothing written since the last snapshot is lost.
    ///
    /// # This must run on a multi-threaded tokio runtime
    ///
    /// See [`Flock::wake`] for why a current-thread runtime deadlocks rather than erroring.
    ///
    /// # What it does NOT do: delete your local pages
    ///
    /// The pool's CAS is left exactly as it was. Sleeping frees *compute*, and puts a durable copy in
    /// object storage; it does not reclaim local disk. Reclaiming it means garbage-collecting pages
    /// that no live manifest references, which is [`substrate_pager::PageStore::gc`]'s job and is a
    /// fleet-level policy decision (F4), not something a single `sleep()` call should take upon
    /// itself while the caller is not looking.
    ///
    /// # The trap underneath this, which cost an afternoon to find
    ///
    /// The obvious implementation is to open a [`TieredStore`] directly on the pool's CAS and call
    /// [`TieredStore::sleep`]. **That destroys the pool.**
    ///
    /// `TieredCas` tracks which pages are `pending` upload, and it learns that by watching pages go
    /// *through* it. Open one over a CAS that already has pages in it — pages a `DurableStore` wrote,
    /// which it never saw — and `pending` is empty. `flush()` then uploads *nothing*, believing there
    /// is nothing to upload; `drop_local()` checks that `pending` is empty, concludes everything is
    /// safely in the bucket, and **deletes every page in the pool**. The manifests upload fine, so you
    /// are left with a bucket full of manifests pointing at pages that exist nowhere, and a local CAS
    /// that has been emptied. Every database in the pool, gone, with a successful return code.
    ///
    /// So we never point a `TieredStore` at the pool. We open one on a throwaway staging directory and
    /// *copy* the pages into it — through `TieredCas::put`, which is what marks them `pending` and is
    /// therefore what makes `flush()` actually upload them. The staging directory is ours, it holds
    /// nothing unique, and `drop_local()` is welcome to it.
    pub async fn sleep(mut self, remote: RemoteTier) -> Result<WakeToken> {
        let head = self.snapshot()?;
        let page_size = self.store.pager().page_size();

        // Release the SQL engine before we do any I/O: the point of sleeping is to stop paying for
        // compute, and there is no reason to hold a DuckDB connection open through an upload.
        drop(self.kernel);

        // A staging store we own and will throw away. NOT the pool — see the trap above.
        let staging =
            tempfile::tempdir().map_err(FlockError::pool("stage for sleep", &self.root))?;
        let config = StoreConfig {
            page_size,
            pool: remote.pool().to_string(),
            ..StoreConfig::default()
        };
        let tiered = TieredStore::open(staging.path(), remote, config)
            .await
            .map_err(FlockError::tier("open the staging store"))?;

        let src = self.store.page_store();
        let dst_cas = tiered.pager().cas();
        let dst_manifests = tiered.pager().manifest_store();

        // Walk the head's ancestry: the overlay chain it resolves through (the STORAGE edge — without
        // it the manifest cannot be read at all) and the parents that are its history (the HISTORY
        // edge — without it, every snapshot id the caller is holding stops working after a wake).
        //
        // Copying the manifests *by value* keeps their ids: a manifest id is a hash of its bytes, and
        // these are the same bytes. That is what lets a `ManifestId` taken before a sleep still be a
        // valid argument to `restore` after the wake. Re-deriving the manifests instead would have
        // been easier and would have silently changed every id.
        let mut seen: HashSet<ManifestId> = HashSet::new();
        let mut stack = vec![head];
        let mut page_ids: HashSet<_> = HashSet::new();

        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let manifest = src
                .manifest(&id)
                .map_err(FlockError::pager("read manifest"))?;

            // Every page this manifest can see, not merely the ones it changed — so that restoring to
            // an older snapshot after a wake finds its pages there.
            for page_id in src
                .resolve(&id)
                .map_err(FlockError::pager("resolve manifest"))?
                .into_values()
            {
                page_ids.insert(page_id);
            }

            dst_manifests
                .put(&manifest)
                .map_err(FlockError::pager("stage manifest"))?;

            if let Some(base) = manifest.overlay_base() {
                stack.push(base);
            }
            if let Some(parent) = manifest.parent {
                stack.push(parent);
            }
        }

        // The pages, through `TieredCas::put` — which is the whole point. This is what marks them
        // `pending`, and `pending` is what `flush()` uploads.
        let src_cas = self.store.pager().cas();
        for page_id in page_ids {
            let page = src_cas
                .get(page_id)
                .map_err(FlockError::pager("read page for upload"))?;
            dst_cas
                .put(&page)
                .map_err(FlockError::pager("stage page for upload"))?;
        }

        tiered
            .pager()
            .set_head_to(head)
            .map_err(FlockError::pager("position the staging store"))?;

        // Flush the pages, upload the manifest ancestry, drop the staging cache. If the flush fails,
        // substrate drops nothing and errors — a sleep that loses data is a bug with good marketing.
        tiered.sleep().await.map_err(FlockError::tier("sleep"))
    }

    /// Export to a **plain, standard `.duckdb` file** with no dependency on FlockDB.
    ///
    /// This is the escape hatch, and it is a product decision rather than a feature. The largest
    /// objection to adopting a new storage engine is *"what if you disappear, or I hate you"*, and
    /// the answer has to be one command that hands the data back in a format with an ecosystem.
    /// Anything less makes us a hostage-taker, and serious buyers can smell it.
    ///
    /// Open the result with the `duckdb` CLI, with Python, with a BI tool. There is no import step and
    /// no compatibility mode, because there is nothing of ours in it. CLAUDE.md rule 4 tests this on
    /// every commit against a *stock* DuckDB connection that has never heard of FlockDB — a test that
    /// proved only *we* can read our own export would prove nothing.
    ///
    /// Refuses to overwrite an existing file.
    pub fn export_duckdb(&mut self, path: impl AsRef<Path>) -> Result<()> {
        Ok(self.kernel.export(path.as_ref())?)
    }

    fn rewind_to(&mut self, manifest: ManifestId) -> Result<()> {
        let store = self.store.page_store();

        // Refuse a manifest this pool has never seen, rather than discovering it three layers down as
        // a missing page. A `ManifestId` is 32 opaque bytes; the caller may well have got one from the
        // wrong pool, and the error should say so.
        store
            .manifest(&manifest)
            .map_err(|_| FlockError::UnknownSnapshot { manifest })?;

        store
            .rewind(&manifest)
            .map_err(FlockError::pager("restore"))?;

        // Make the new head durable, so a restart lands here and not back where we were. `checkpoint`
        // persists the current head — this is the one place it is the right call.
        if let Store::Local(d) = &self.store {
            d.checkpoint()
                .map_err(FlockError::wal("record the restored head"))?;
        }
        Ok(())
    }
}
