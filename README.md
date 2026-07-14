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

This repository is **F1**: the engine. The `flock` CLI (F2), replication (F3), and the fleet plane
that manages ten thousand of these (F4–F5) are later.

---

## What is real, and what is not

We would rather you read this here than find it in a POC. This section is the point of the README.

| Claim | Status |
| --- | --- |
| **Fork isolation at the SQL level** | **Real, and structural.** Not "we check" — a manifest is an immutable value and the fork holds a different one. 9 tests. |
| **Snapshot / restore round-trip** | **Real.** O(1) in substrate — a snapshot is a 32-byte content hash. |
| **`export_duckdb` writes a vanilla `.duckdb` file** | **Real.** Tested every commit by opening it with a *stock* `duckdb::Connection` that has never heard of us. |
| **A snapshot survives a crash** | **Real.** The commit point is an fsync'd WAL record. |
| **TPC-H SF0.1 < 15 % over raw DuckDB** (docs/02 §7) | **Measured.** See below. |
| *Writes between snapshots* are durable | **No.** They live in a DuckDB scratch file. A crash takes them. |
| `fork()` is O(1) end to end | **No.** O(1) in substrate; **O(database) in the kernel**, which must hydrate a file for DuckDB. |
| `query()` streams results | **No.** F1 materialises them. The type is named `ArrowStream` because the API is frozen; the laziness is F2. |
| `sleep()` hibernates a database into object storage | **No.** F1's sleep releases *compute*. Nothing is uploaded. |
| Wake from S3 in < 250 ms (docs/02 §7) | **Not measured.** The substrate API that would do it is not published yet. Not "measured and passing" — *not measured*. |

Every "No" in that table has the same root cause, and it is worth stating once.

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

---

## Quick start

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
    tests/        SQL smoke · fork isolation · snapshot/restore · export-to-stock-DuckDB
    benches/      TPC-H SF0.1, full stack vs raw DuckDB
```

`flock-*` may depend on `substrate-*`. It may **never** depend on `loom-*`
([LoomDB](https://github.com/Bobcatsfan33/loomdb) is the other product on the same engine). The
substrate dependency is pinned to a git **tag**, never a branch: a moving dependency under a storage
engine means the on-disk format can change without a version bump.

## Building

```bash
cargo test --workspace                      # DuckDB is compiled from source (bundled). No system lib.
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p flock-core --features tpch   # slow: CMake build of DuckDB. No network.
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
