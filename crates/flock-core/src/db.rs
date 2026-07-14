//! [`Flock`] and [`Db`] — the public API from docs/02 §5.3.

use crate::error::{FlockError, Result};
use crate::pool::{pages_dir, validate_name, wal_dir};
use crate::wake::WakeToken;
use flock_kernel::{ArrowStream, DuckDbKernel, KernelOpts, SqlKernel};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use substrate_pager::{std_vfs, ManifestId, PageStore, Pager, StoreConfig};
use substrate_wal::Wal;

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

        std::fs::create_dir_all(&root).map_err(FlockError::pool("create pool root", &root))?;

        let pager = Arc::new(open_pager(&root)?);

        let mut wal =
            Wal::open(std_vfs(), wal_dir(&root, db_name)).map_err(FlockError::wal("open"))?;

        // Replay. This sets the pager's head to the last checkpointed manifest — or, for a database
        // that does not exist yet, to the canonical empty root. It must happen BEFORE the kernel
        // opens, because the kernel hydrates its scratch file from whatever the head is at the
        // moment it is constructed. Open the kernel first and every database in the pool would come
        // up empty, and only the *second* run of the program would notice.
        wal.recover(&pager).map_err(FlockError::wal("recover"))?;

        let kernel = DuckDbKernel::open(pager.clone(), opts.clone())?;

        Ok(Db {
            name: db_name.to_string(),
            root,
            pager,
            wal,
            kernel,
            opts,
        })
    }

    /// Bring a sleeping database back.
    ///
    /// Cheap in the sense that matters — no bytes move between machines — but **not** cheap in the
    /// sense docs/02 §7 targets, because it rehydrates a scratch file from local pages. See the
    /// `wake` module for exactly what F1's sleep does and does not do; the summary is that the
    /// compute half is real and the object-storage half is not built yet.
    pub fn wake(token: &WakeToken) -> Result<Db> {
        Flock::wake_with(token, KernelOpts::default())
    }

    /// [`Flock::wake`], with control over the SQL engine's threads and memory.
    pub fn wake_with(token: &WakeToken, opts: KernelOpts) -> Result<Db> {
        let db = Flock::open_with(token.pool_root(), token.db_name(), opts)?;
        // The token names an exact state, and it wins over whatever the WAL last checkpointed.
        // Ordinarily they agree — `sleep()` checkpoints before it returns the token — but a token
        // is a *value*, and honouring it exactly is what makes it safe for a fleet registry to hold
        // one for a year and hand it back.
        db.restore_into(token.manifest())
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
/// | [`sleep`](Db::sleep) | O(1) | O(database) — one last sync |
///
/// The left column is the architecture. **The right column is the price of the fallback**, and it
/// is the price a DuckDB filesystem hook would abolish — see the `flock-kernel` crate docs for why
/// that hook is not reachable from Rust today.
pub struct Db {
    name: String,
    root: PathBuf,
    /// Concrete `Pager`, not `Box<dyn PageStore>`, because forking needs `Pager::at` — the inherent
    /// method that hands back a *typed* store positioned at a manifest. `PageStore::fork` returns a
    /// `Box<dyn PageStore>`, and a trait object cannot be forked again with a private WAL.
    pager: Arc<Pager>,
    wal: Wal,
    kernel: DuckDbKernel,
    opts: KernelOpts,
}

/// Names the database and where it is, and *nothing else*.
///
/// Deliberately not derived. A derived `Debug` would reach into the `Pager` and the `Wal`, and one
/// day someone would log a `Db` at debug level and put a page of a customer's data into a log
/// aggregator. The three fields below are the three a human debugging a fleet actually wants, and
/// none of them is data.
impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("name", &self.name)
            .field("pool", &self.root)
            .field("head", &self.pager.head())
            .finish_non_exhaustive()
    }
}

impl Db {
    /// The database's name within its pool.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The pool this database lives in.
    pub fn pool_root(&self) -> &Path {
        &self.root
    }

    /// Run a query. Results come back as Arrow (docs/02 §6.1).
    ///
    /// See [`ArrowStream`] for the one thing that is not yet true about the name: F1 materialises
    /// the whole result rather than streaming it.
    pub fn query(&mut self, sql: &str) -> Result<ArrowStream> {
        Ok(self.kernel.query(sql)?)
    }

    /// Run a statement for its effect. Returns the number of rows it changed.
    ///
    /// **This does not make anything durable.** The write lands in DuckDB's scratch file; substrate
    /// hears about it at the next [`snapshot`](Db::snapshot). That boundary is the fallback's, it
    /// is real, and it is spelled out in full in the `flock-kernel` crate docs.
    pub fn execute(&mut self, sql: &str) -> Result<u64> {
        Ok(self.kernel.execute(sql)?)
    }

    /// Take a snapshot. **This is the commit point.**
    ///
    /// Returns a [`ManifestId`]: a 32-byte content hash that *is* the complete state of this
    /// database at this instant. Hand it to [`restore`](Db::restore) at any point in the future and
    /// you are back here exactly.
    ///
    /// # What happens, in the order it happens
    ///
    /// ```text
    ///   1. DuckDB folds its own WAL into its main file
    ///   2. the file is chunked into pages; changed pages go to the CAS and are fsync'd
    ///   3. substrate derives and installs the manifest
    ///   4. the manifest id is written to substrate's WAL and fsync'd   ← THE COMMIT POINT
    /// ```
    ///
    /// A crash before step 4 leaves durable pages that nothing references and a manifest nothing
    /// points at — garbage, which GC sweeps, and the database opens at the previous snapshot,
    /// whole. A crash after step 4 is a snapshot that happened. There is no state in between, and
    /// that is the durability guarantee (docs/02 §3.1).
    ///
    /// # And the part that is *not* free
    ///
    /// The snapshot itself is O(1) — substrate measures it at 15 ns. Step 2 is not: it reads the
    /// whole database file. Snapshots in F1 are cheap in *manifests* and linear in *I/O*. Do not
    /// take one per row.
    pub fn snapshot(&mut self) -> Result<ManifestId> {
        let id = self.kernel.checkpoint()?;
        self.wal
            .checkpoint(id)
            .map_err(FlockError::wal("commit snapshot"))?;
        Ok(id)
    }

    /// Fork this database. **The headline property, and the one worth being blunt about.**
    ///
    /// The fork is a *real, separate database*. It shares every page with its parent by
    /// construction and copies none of them. Then:
    ///
    /// > **A write to the fork is never, under any circumstance, visible in the parent.**
    ///
    /// That is not enforced by a check we perform, which is a much weaker claim than it sounds.
    /// It is **structural**: a manifest is an immutable value, the fork holds a different one, and
    /// there is no code path — no bug, no race, no `WHERE` clause someone forgot — that could make
    /// the parent read the fork's bytes. `fork_isolation.rs` in this crate's tests asserts it at
    /// the SQL level, as bluntly as it can be written.
    ///
    /// # This checkpoints the parent first, and it must
    ///
    /// The parent's uncommitted writes live in a DuckDB scratch file substrate has never seen. A
    /// fork that did not checkpoint first would branch from the parent's *last snapshot*, silently
    /// dropping everything the caller has done since — and the caller, who has just watched their
    /// `INSERT`s succeed, would have no reason to suspect it.
    ///
    /// # What it costs
    ///
    /// O(1) in substrate — 98 ns, flat, no bytes copied. **O(database size) in the kernel**, because
    /// the fork needs its own DuckDB connection and DuckDB needs a file, so we hydrate one. The
    /// isolation is free; the file is not. See the `flock-kernel` crate docs.
    pub fn fork(&mut self, name: &str) -> Result<Db> {
        validate_name(name)?;

        let dir = wal_dir(&self.root, name);
        if dir.exists() {
            // We will not adopt it, and adopting it is the *dangerous* option: `Wal::recover` would
            // set the fork's head to that other database's manifest, and `fork` would return a
            // handle full of someone else's rows while reporting complete success.
            return Err(FlockError::NameTaken {
                name: name.to_string(),
                path: dir,
            });
        }

        let head = self.snapshot()?;

        // O(1). `Pager::at` shares the CAS and the manifest store — this is the fork — and gives
        // the new database a private head.
        let forked = Arc::new(self.pager.at(head).map_err(FlockError::pager("fork"))?);

        // The fork's head is durable from birth. Note we do NOT call `recover()` here: the WAL is
        // brand new, so recovery would reset the head to the empty root manifest and hand back an
        // empty database. That is a genuinely easy mistake — it is the same call that is mandatory
        // in `Flock::open` — and it would be caught only by a test that forks a *non-empty*
        // database, which is why there is one.
        let mut wal = Wal::open(std_vfs(), &dir).map_err(FlockError::wal("open fork's log"))?;
        wal.checkpoint(head)
            .map_err(FlockError::wal("record fork's head"))?;

        let kernel = DuckDbKernel::open(forked.clone(), self.opts.clone())?;

        Ok(Db {
            name: name.to_string(),
            root: self.root.clone(),
            pager: forked,
            wal,
            kernel,
            opts: self.opts.clone(),
        })
    }

    /// Go back to a snapshot. O(1) in substrate: a pointer moves.
    ///
    /// **Everything written since the snapshot is discarded**, including work that has not been
    /// snapshotted at all. That is what "restore" means, it is not recoverable through this API,
    /// and this sentence is the only warning you get — so take a [`snapshot`](Db::snapshot) first
    /// if you might want the current state back. Snapshots are cheap and this is exactly what they
    /// are for.
    ///
    /// The old state is *not destroyed*: it is still a manifest, still content-addressed, still
    /// restorable if you kept its id. Nothing is deleted until GC runs and finds no live manifest
    /// referencing it.
    pub fn restore(&mut self, manifest: ManifestId) -> Result<()> {
        self.rewind_to(manifest)?;
        // Rebuilding the kernel rehydrates the scratch file from the restored head, and drops the
        // old connection — and with it, the old scratch file. Assigning here rather than mutating
        // in place means a failure leaves the previous kernel untouched and usable.
        self.kernel = DuckDbKernel::open(self.pager.clone(), self.opts.clone())?;
        Ok(())
    }

    /// Put the database to sleep, releasing its SQL engine, its threads, and its scratch file.
    ///
    /// Consumes the handle, because after this there is no database in memory — only a
    /// [`WakeToken`], which is a name and 32 bytes. Bring it back with [`Flock::wake`].
    ///
    /// It checkpoints first, so nothing is lost.
    ///
    /// # Read the `wake` module before you believe this does what you think
    ///
    /// F1's sleep releases **compute**. It does **not** upload anything to object storage — the
    /// pages stay in the local CAS, so a sleeping F1 database still occupies its bytes on local
    /// disk. The <250 ms cold wake from S3 in docs/02 §7 is F3 work and is **not measured here**,
    /// because the substrate API that would do it is not published yet. Details, including exactly
    /// which substrate types are missing, are in the `wake` module docs.
    pub fn sleep(mut self) -> Result<WakeToken> {
        let manifest = self.snapshot()?;
        Ok(WakeToken::new(self.root, self.name, manifest))
    }

    /// Write a vanilla `.duckdb` file. **The escape hatch** (docs/02 §6.2).
    ///
    /// This is a product decision, not a feature. The largest objection to adopting a new storage
    /// engine is *"what if you disappear, or I hate you"* — and the answer has to be one command
    /// that hands over the data in a format with an ecosystem and no dependency on us. Anything
    /// less makes us a hostage-taker, and serious buyers can smell it.
    ///
    /// The file it writes has never heard of FlockDB. Open it with the `duckdb` CLI, with
    /// `import duckdb` in Python, with a BI tool. There is no import step and no "FlockDB
    /// compatibility mode", because there is nothing of ours in it.
    ///
    /// It exports **what you can see right now**, including writes since the last snapshot — it
    /// reads through the live DuckDB connection, so it does not depend on the page sync having run.
    /// That matters: the moment you most want an export is the moment you least trust the rest of
    /// the system.
    ///
    /// It refuses to overwrite an existing file. Tested on every commit, against a fresh DuckDB
    /// connection that has never heard of us, so that it cannot rot into a claim we stopped
    /// checking.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), flock_core::FlockError> {
    /// # let mut db = flock_core::Flock::open("/tmp/pool", "sales")?;
    /// db.export_duckdb("/tmp/sales-i-can-take-with-me.duckdb")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn export_duckdb(&mut self, path: impl AsRef<Path>) -> Result<()> {
        Ok(self.kernel.export(path.as_ref())?)
    }

    /// Run several statements as one script.
    ///
    /// Outside docs/02 §5.3's frozen surface, and here because the alternative is every caller
    /// splitting SQL on semicolons — which breaks the first time a string literal contains one.
    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.kernel.execute_batch(sql)?)
    }

    /// The manifest this database is currently sitting on, without taking a snapshot.
    ///
    /// Note this is the head as substrate knows it — i.e. as of the last [`snapshot`](Db::snapshot),
    /// **not** including writes still sitting in DuckDB's scratch file.
    pub fn head(&self) -> ManifestId {
        self.pager.head()
    }

    /// Move the store's head, checking the manifest exists and recording the move durably.
    fn rewind_to(&mut self, manifest: ManifestId) -> Result<()> {
        self.pager.rewind(&manifest).map_err(|e| match e {
            // Substrate says "missing manifest"; the caller needs to hear "that is not a snapshot
            // of a database in this pool", which is a different and much more actionable sentence.
            substrate_pager::PagerError::MissingManifest(_) => {
                FlockError::UnknownSnapshot { manifest }
            }
            source => FlockError::Pager {
                op: "restore",
                source,
            },
        })?;
        self.wal
            .checkpoint(manifest)
            .map_err(FlockError::wal("record restored head"))?;
        Ok(())
    }

    /// `restore`, but consuming and returning `self` — for [`Flock::wake`], which has a `Db` it has
    /// just built and wants positioned at a token's manifest before anyone sees it.
    fn restore_into(mut self, manifest: ManifestId) -> Result<Db> {
        self.restore(manifest)?;
        Ok(self)
    }
}

/// Open the pool's shared page store.
///
/// The pool name is the *directory name*, which makes it visible in a path, in a mount, and in an
/// `ls` — an operator who needs to know which classification boundary a directory belongs to should
/// not have to run a query to find out.
fn open_pager(root: &Path) -> Result<Pager> {
    let pages = pages_dir(root);
    let pool = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());

    Pager::open(
        &pages,
        StoreConfig {
            pool,
            ..StoreConfig::default()
        },
    )
    .map_err(FlockError::pager("open pool"))
}
