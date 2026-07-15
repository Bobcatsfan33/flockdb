# FlockDB

**A DuckDB you can fork in a millisecond and snapshot for free.**

```rust
let mut sales = Flock::open("/var/lib/flock/tenants", "acme")?;
sales.execute("CREATE TABLE t AS SELECT * FROM 'sales.parquet'")?;

let mut experiment = sales.fork("what-if")?;        // no bytes copied
experiment.execute("DELETE FROM t WHERE region = 'EMEA'")?;

sales.query("SELECT count(*) FROM t")?;             // untouched. two databases now.
```

FlockDB is an embedded analytical database: **DuckDB's SQL, on a content-addressed page store**
([substrate](https://github.com/Bobcatsfan33/substrate)), so that a database is cheap to have and a
fork costs nothing. The target workload is *many small databases* — a database per tenant, per
backtest, per service, per agent session — rather than one big one.

This repository is **F1** (the engine) and **F2** (the `flock` CLI, and `import flockdb` for Python).
Replication (F3) and the fleet plane that manages ten thousand of these (F4–F5) are later.

**Fork a database in five commands** — the [Quick start](#quick-start) below is extracted from this
README and run verbatim in CI, so it cannot drift from what the CLI actually does.

---

## What is real, and what is not

We would rather you read this here than find it in a POC. This section is the point of the README.

| Claim | Status |
| --- | --- |
| **Fork isolation at the SQL level** | **Real, and structural.** Not "we check" — a manifest is an immutable value and the fork holds a different one. 9 tests. |
| **Snapshot / restore round-trip** | **Real.** O(1) in substrate — a snapshot is a 32-byte content hash. |
| **`export_duckdb` writes a vanilla `.duckdb` file** | **Real.** Tested every commit by opening it with a *stock* `duckdb::Connection` that has never heard of us. |
| **A snapshot survives a crash** | **Real.** The commit point is an fsync'd WAL record, written by substrate's `DurableStore` — the path with 50,000 randomized crash-and-recover cycles behind it. We do not order the commit ourselves. (We used to. See below.) |
| **`sleep()` puts a database in object storage** | **Real.** Pages and the manifest's whole ancestry go to the bucket. Every tiering test **deletes the entire pool directory** between `sleep()` and `wake()`, so the data cannot be coming from anywhere else. |
| **A snapshot id still works after a sleep/wake** | **Real.** `sleep()` copies manifests by value, so their content hashes — their ids — survive the round trip. |
| **TPC-H SF0.1 < 15 % over raw DuckDB** (docs/02 §7) | **Measured.** See below. |
| *Writes between snapshots* are durable | **No.** They live in a DuckDB scratch file. A crash takes them. |
| `fork()` is O(1) end to end | **No.** O(1) in substrate; **O(database) in the kernel**, which must hydrate a file for DuckDB. |
| `query()` streams results | **No.** F1 materialises them. The type is named `ArrowStream` because the API is frozen; the laziness is F2. |
| **Wake is lazy** — fetch only the pages a query touches | **No, and it cannot be in F1.** Substrate's wake *is* lazy. FlockDB defeats it: DuckDB needs a whole file, so waking reconstructs the database and therefore downloads **all of it**. Waking a 100 GB database moves 100 GB. |
| **Wake from S3 in < 250 ms (docs/02 §7)** | **NOT MEASURED**, and the floor below suggests it is in trouble. Not "measured and passing". *Not measured.* |

Every "No" in that table has the same root cause, and it is worth stating once.

## The other thing we got wrong, and fixed

The first version of this engine used substrate's `Pager` directly and **hand-ordered the commit
protocol**: pages into the CAS, install the manifest, then append a WAL record. Substrate's actual
protocol puts the fsync'd WAL record *before* the install — that record **is** the commit point.
Worse, the "append" was `Wal::checkpoint()`, which is not a commit at all: it persists the head and
*truncates the log behind it*. Every snapshot silently threw the log away.

It worked. That is the problem with it. It was "probably safe, by my reading, with no crash-injection
harness" — and that is not a sentence anyone should accept about a storage engine.

It now commits through **`substrate_wal::DurableStore`**, the path with **50,000 randomized
crash-and-recover cycles** behind it, which kill the write path at every byte boundary in turn and
which found three real bugs when substrate ran them. We do not order the commit. It does.

`crates/flock-core/src/store.rs` is the adapter, and `many_snapshots_in_a_row_all_replay_after_the_process_dies`
is the test that keeps it honest.

## The one engineering decision that explains everything

**DuckDB will not let us give it a filesystem.**

The ideal design is a DuckDB *virtual filesystem* backed by substrate's `PageStore`: DuckDB thinks
it has a file, we hand it pages, nothing is ever copied. We went looking for that seam. What we
found:

- **DuckDB's C++ core has it.** `virtual_file_system.hpp` declares
  `VirtualFileSystem::RegisterSubSystem(unique_ptr<FileSystem>)`. It is how `httpfs` teaches DuckDB
  to read `s3://`.
- **The C API does not expose it.** `duckdb.h` has a `duckdb_file_system` handle, but it is
  *consumption only* (`duckdb_file_system_open`, `duckdb_file_handle_read`…). The complete set of
  registration hooks is scalar/aggregate/table/copy/cast functions, logical types, config options,
  and log storage. **There is no `duckdb_register_file_system`.**
- **`duckdb-rs` binds the C API and nothing else.** Grepping its entire `src/` for `filesystem`,
  `file_system`, or `vfs` returns one hit: a comment in a unit test about invalid Unicode.

So we took the fallback that substrate's docs/02 §5.2 names: **DuckDB owns a temp file, and we sync
file ↔ pages at transaction boundaries.**

The full write-up, including what a C++ loadable extension would cost and why we declined it for
F1, is at the top of [`crates/flock-kernel/src/lib.rs`](crates/flock-kernel/src/lib.rs).

### What the fallback costs

**Reads pay nothing.** A `SELECT` is DuckDB reading a local file it opened itself. FlockDB is not in
the call stack.

**Writes and forks pay O(database size).** `snapshot()` reads the whole file back and byte-compares
every page. `fork()` hydrates a fresh file for the fork's DuckDB. The *isolation* is free; the
*file* is not.

| Database size | Bytes read per `snapshot()` |
| --- | --- |
| 10 MiB | 10 MiB |
| 1 GiB | 1 GiB |
| 100 GiB | 100 GiB — **at this size FlockDB is the wrong tool**, and we would rather say so |

---

## Performance

Reproduce with `cargo bench -p flock-core --features tpch`. Measured on an 8-core Apple laptop —
which is to say, on a noisy machine, and we will get to that.

### TPC-H SF0.1 — the target from docs/02 §7

21 of the 22 queries (q15 defines a view and cannot be run twice in one connection; we exclude it
and say so rather than quietly calling 21 queries "TPC-H"). Both engines are DuckDB, on a real file,
with identical thread and memory settings.

| | |
| --- | --- |
| **Overhead, median of paired rounds** | **+0.3 %**, **+1.1 %**, **−4.0 %** across three independent runs |
| Target (docs/02 §7) | < 15 % |
| Per-round spread | up to **±20 %** |

**The honest reading of that table is "the overhead is indistinguishable from zero, and our
measuring rig cannot resolve anything finer."** One of the three runs came out *negative*, which is
not a claim that FlockDB is faster than DuckDB — it is the noise floor of a laptop.

We are not embarrassed by the near-zero result, because it is exactly what the architecture
predicts: **on the read path FlockDB is not in the call stack.** DuckDB opened a local file and is
reading it. There is nothing of ours in between, so there is nothing to be slow.

> **How we got here, because the first version of this benchmark was lying.** It took the mean of
> five raw runs and the mean of five FlockDB runs and divided. Run twice, it reported **+0.7 %** and
> then **+8.2 %**. Criterion, on the same code, reported FlockDB *faster than raw DuckDB* — which is
> impossible. All three numbers were measuring the laptop, not the code. The fix was to **pair** the
> measurements — time both engines back to back, so a slow second makes both slow and the ratio is
> unmoved — and to report the median ratio and the full range. We are writing this down because the
> tempting thing was to publish the +0.7 % and move on.

### Snapshot — where the fallback actually charges

The number the target does not name, and the one that will matter to you. A 26.3 MiB SF0.1 database
(600,572 `lineitem` rows):

| Operation | Time |
| --- | --- |
| `snapshot()`, nothing changed | **33.1 ms** |
| `snapshot()`, one row changed | **58.5 ms** |

That is ~800 MB/s, and it is **linear in database size, not in what you changed**. A `snapshot()` of
a 1 GiB database reads 1 GiB, because DuckDB owns the file and will not tell us which parts of it
moved. Only the *writes* track the delta — substrate stores just the changed pages, so ten thousand
forks of a template still cost one template.

A benchmark suite that only measured the operation we are fast at would be marketing, so
`benches/tpch.rs` measures both and prints both.

### Wake latency — the target we did NOT measure, and why we think it is in trouble anyway

docs/02 §7 wants **p99 wake-to-first-query < 250 ms**. We could not measure it: this machine had no
S3-compatible endpoint (the Docker daemon would not start, and pulling the MinIO binary was
refused). So the number does not exist, and we are not going to imply otherwise. There is an
`#[ignore]`d test, `wake_latency_against_a_real_s3_endpoint`, that produces it in one command the
moment anyone has a bucket — a mechanism, rather than a promise to measure it later.

What we *can* measure is the **floor**: the same operation against an in-process object store, which
is to say with the network latency set to exactly zero.

| Database | `Flock::open` from local pool → first query | **`Flock::wake` → first query** (zero-latency object store) |
| --- | --- | --- |
| 1 k rows (~210 KB) | 20 ms | **95 ms** |
| 100 k rows (~210 KB) | 17 ms | **79 ms** |
| 1 M rows (~800 KB) | 31 ms | **199 ms** |

Read the right-hand column again. On a **sub-megabyte** database, with an object store that responds
instantly, waking already costs 79–199 ms of a 250 ms budget. Every real network round trip is on
top of that — and because FlockDB's wake is **O(database)** rather than O(pages touched), there is
one such fetch for *every page in the database*, not for the few a query needs.

We do not think this target is reachable in F1, and the reason is not substrate: substrate's wake is
lazy and would meet it. It is the same fallback as everything else on this page — **DuckDB needs a
whole file before it will answer anything.** We would rather say that now than discover it in a POC.

---

## Quick start

Clone the repo and build the CLI once. This compiles DuckDB from source, so it takes a few minutes —
that cost is real and we are not going to hide it behind the number below.

```console
$ git clone https://github.com/Bobcatsfan33/flockdb && cd flockdb
$ cargo install --path crates/flock-cli    # builds `flock` (compiles DuckDB — minutes, once)
```

Then, from the repo root, these five commands import a table, **fork it without copying a byte**,
change the fork, and show the original is untouched. Every line of output below is what you will
actually see — this block is parsed out of the README and asserted byte-for-byte in CI, so if the CLI
ever prints something different, the build goes red until this document is fixed.

<!-- QUICKSTART:BEGIN -->
```console
$ flock import examples/trades.csv
imported 10 rows into table "trades" on branch "main"

$ flock sql "SELECT venue, count(*) AS fills, sum(qty) AS shares FROM trades GROUP BY venue ORDER BY shares DESC"
+-------+-------+--------+
| venue | fills | shares |
+-------+-------+--------+
| XNAS  | 6     | 590    |
| ARCX  | 2     | 260    |
| BATS  | 2     | 180    |
+-------+-------+--------+

$ flock branch what-if
forked "main" → "what-if" (no pages copied)
switched to branch "what-if"

$ flock sql "DELETE FROM trades WHERE venue = 'XNAS'"
+-------+
| Count |
+-------+
| 6     |
+-------+

$ flock --branch main sql "SELECT count(*) AS still_here FROM trades"
+------------+
| still_here |
+------------+
| 10         |
+------------+
```
<!-- QUICKSTART:END -->

The fork deleted six rows and `main` still has all ten — two separate databases, and the fork cost no
copied pages. The build takes minutes; **the five commands, once `flock` is on your path, take well
under ninety seconds** (`scripts/quickstart.sh` runs them and fails if they do not).

## Using FlockDB as a library

```rust
use flock_core::Flock;

let mut db = Flock::open("/tmp/pool", "sales")?;

db.execute("CREATE TABLE t (id INTEGER, region TEXT)")?;
db.execute("INSERT INTO t VALUES (1, 'EMEA'), (2, 'AMER')")?;

let snap = db.snapshot()?;          // the commit point. 32 bytes that ARE the database.

let mut fork = db.fork("what-if")?; // a real, separate database
fork.execute("DELETE FROM t WHERE region = 'EMEA'")?;

db.restore(snap)?;                  // O(1). back to exactly there.
db.export_duckdb("/tmp/mine.duckdb")?;   // and here is your data, with no strings attached.
```

Results are [Arrow](https://arrow.apache.org/) `RecordBatch`es, not a bespoke row format.

### Sleep and wake

```rust
use flock_core::{Flock, RemoteTier, object_store::aws::AmazonS3Builder};

let bucket = AmazonS3Builder::new().with_bucket_name("tenants").build()?;
let tier = RemoteTier::new(std::sync::Arc::new(bucket), "acme");   // "acme" is the POOL

// Everything goes to object storage. What's left is a token: a pool, 32 bytes, a page size.
let token: WakeToken = db.sleep(tier.clone()).await?;

// …a month later, on a different machine, into an empty directory:
let mut db = Flock::wake("/var/cache/flock", "sales", tier, &token).await?;
db.query("SELECT count(*) FROM t")?;
```

A `WakeToken` is the whole database as far as anything else is concerned, which is why a million
sleeping databases fit in a registry on a laptop. **`sleep`/`wake` need a multi-threaded tokio
runtime** — substrate's read path is synchronous by design, so a cache miss blocks on an async fetch,
and a current-thread runtime deadlocks rather than erroring. And read the wake-latency section above
before you plan around this: waking is O(database), not O(what you query.)

## The escape hatch

`Db::export_duckdb(path)` writes a **vanilla, standard `.duckdb` file** with no dependency on
FlockDB, on substrate, or on anything we ship. Open it with the `duckdb` CLI, with Python, with a BI
tool. There is no import step and no compatibility mode, because there is nothing of ours in it.

This is a product decision, not a feature. The largest objection to adopting a new storage engine is
*"what if you disappear, or I hate you"* — and the answer has to be one command that hands the data
back in a format with an ecosystem. Anything less makes us a hostage-taker.

**It is tested on every commit against a stock DuckDB connection.** It is not allowed to rot.

## Layout

```
crates/
  flock-kernel/   the SqlKernel trait, the DuckDB implementation, and the page marshalling
  flock-core/     Flock, Db — the public API. Durability, forks, snapshots, the escape hatch.
    store.rs      WHERE THE COMMIT POINT IS. Routes commits through substrate's DurableStore.
    pool.rs       the on-disk layout, and why there are symlinks in a storage engine
    tests/        SQL smoke · fork isolation · snapshot/restore · export-to-stock-DuckDB · tiering
    benches/      TPC-H SF0.1, full stack vs raw DuckDB
```

**On the symlinks.** A pool keeps one shared CAS (`cas/pages`, `cas/manifests`) and gives each
database a private WAL (`dbs/<name>/wal`); each database's `pages`/`manifests` are symlinks into the
shared CAS. That shape is forced: substrate's `DurableStore` takes **one directory** and puts the CAS
and the WAL both inside it, which cannot express *"many databases, one CAS, a WAL each"* — and that
is the only shape FlockDB has. Give each database its own directory and a fork copies every page;
point two of them at one directory and they share a WAL and corrupt each other's head. The proper fix
is a `DurableStore::from_parts(pager, wal)` upstream. Until then, a symlink is the boring answer, and
it beats hand-rolling the commit protocol a second time.

`flock-*` may depend on `substrate-*`. It may **never** depend on `loom-*`
([LoomDB](https://github.com/Bobcatsfan33/loomdb) is the other product on the same engine). The
substrate dependency is pinned to a git **tag**, never a branch: a moving dependency under a storage
engine means the on-disk format can change without a version bump.

## Building

```bash
cargo test --workspace                      # DuckDB is compiled from source (bundled). No system lib.
cargo test --workspace --features airgap    # must pass with no network
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p flock-core --features tpch   # fetches DuckDB's TPC-H extension once (network)
```

The first build compiles DuckDB from source and takes several minutes.

**The library never touches the network**, and `cargo test --workspace --features airgap` is a
separate CI job that holds us to it. **The benchmark does**, once: it fetches DuckDB's official
TPC-H extension (`INSTALL tpch`), which DuckDB then caches. We tried to avoid that — duckdb-rs
advertises a `tpch` cargo feature that links the extension statically, and it **does not work from
crates.io**: it implies `bundled-cmake`, whose build script needs a `duckdb-sources/` directory that
only exists in a git checkout. That is an upstream packaging bug we cannot configure around, so we
say what we actually do instead of what we wish we did.

## License

Apache-2.0. The engine is open because durability claims that cannot be audited are worthless.
