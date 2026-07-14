# CLAUDE.md — FlockDB

**Read this, then read
[substrate's `docs/02`](https://github.com/Bobcatsfan33/substrate/blob/main/docs/02-embedded-single-node-engine-architecture.md)
and [`docs/04`](https://github.com/Bobcatsfan33/substrate/blob/main/docs/04-flockdb-loomdb-unified-roadmap.md).**
Those are the architecture of record. Code that contradicts them is a bug in the code, or a bug in
the docs — fix the docs first, then the code. Substrate's own `CLAUDE.md` binds here too; the rules
below are the ones that are *specifically* about this repository.

FlockDB is an embedded analytical database — DuckDB's SQL, on substrate's content-addressed page
store, so that forking a database is free and having ten thousand of them is affordable.

---

## The rules

Each one exists because breaking it produces a specific, expensive failure, named below.

### 1. `flock-*` may depend on `substrate-*`. It may NEVER depend on `loom-*`

Not "should not". *Cannot*. The dependency is pinned to a substrate **git tag**, never a branch.

> **Why:** the moment FlockDB depends on LoomDB code (or the reverse), the engine has forked to
> serve two masters, and a small team is maintaining two databases forever. Separate repositories
> make this structurally impossible rather than merely discouraged. And a *branch* dependency under
> a storage engine means the on-disk format can change without a version bump — a store written on
> Tuesday stops opening on Wednesday, and content addressing makes that silent.

### 2. No `unwrap()`, `expect()`, or `panic!()` in library code. Ever

`#![deny(clippy::unwrap_used)]`, `expect_used`, `panic` in every crate. Tests may panic; that is
what an assertion *is*.

> **Why:** a panic in a storage engine is an unplanned process death, and an unplanned process death
> during a commit is precisely the disaster crash recovery exists to survive. Do not manufacture the
> disaster you are defending against.

### 3. An error message states the corrective action

`thiserror` enum per crate, `#[non_exhaustive]`. And then:

```
GOOD:  "table 'foo' does not exist. Run `SHOW TABLES` to list what is there."
BAD:   ERR_NO_TABLE
```

> **Why:** the person reading it is very often a person under pressure at an unreasonable hour. What
> they need is not a diagnosis of our internals — it is the next thing to type. If a variant cannot
> name a next step, it is not finished.

### 4. `export_duckdb` is tested on every commit, against a *stock* DuckDB

Not against ourselves. Against a `duckdb::Connection` that has never heard of FlockDB.

> **Why:** the single largest objection to adopting a new storage engine is *"what if you disappear,
> or I hate you."* The answer must be one command that returns the data in a format with an
> ecosystem and no dependency on us. Anything less makes us a hostage-taker, and serious buyers can
> smell it. A test that only proves *we* can read our own export proves nothing and is exactly the
> test a vendor writes when they want the box ticked. **The escape hatch is not allowed to rot.**

### 5. Doc comments explain *why*, and name the failure the rule prevents

Every public item gets one. A comment that restates the function signature in English is worse than
no comment, because it looks like documentation.

### 6. Say where we are weak, in the repository, before a customer finds it

docs/04 §5: *"We will not ship a number we cannot reproduce"* and *"we will say where we are
weak."*

This is a hard rule and it has teeth. If the TPC-H overhead is 40 %, the README says 40 %. If
`fork()` is O(1) in substrate and O(database) in the kernel — **it is** — then that sentence appears
in the docs for `fork()`, not in a footnote. If a target is **not measured** — the < 250 ms wake in
docs/02 §7 **is not** — the docs say *not measured*, not "should be fine".

> **Why:** a storage engine earns trust exactly once, and it does it by being unembarrassed about
> its limits. A truthful 40 % is worth more than an unreproducible 12 %, because the second one gets
> found out in a POC and takes every other claim down with it.

### 7. Prefer boring

When there is a clever way and an obvious way, take the obvious one and leave a comment saying what
the clever one would have been. This is the most safety-critical code in the company.

### 8. Do not skip a test "for now"

If something genuinely cannot be tested, say so **in the docs, with the reason**. A skipped test in
a database is a lie with a timer on it.

---

## Layout

```
crates/
  flock-kernel/     SqlKernel trait + the DuckDB implementation, and the page marshalling
  flock-core/       Flock, Db — the public API (docs/02 §5.3). Durability, forks, the escape hatch.
    src/store.rs    WHERE THE COMMIT POINT IS. Commits route through substrate's DurableStore.
    src/pool.rs     on-disk layout: one shared CAS per pool, one private WAL per database
    tests/          SQL smoke, fork isolation, snapshot/restore, export-to-stock-DuckDB, tiering
    benches/tpch.rs TPC-H SF0.1, full stack vs raw DuckDB
```

Dependency direction is one-way: `flock-core → flock-kernel → substrate-*`. Never back up the chain.

## Working commands

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --features airgap        # must pass with no network
cargo bench -p flock-core --features tpch       # fetches DuckDB's TPC-H extension once (network)
```

## Definition of done

- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` green
- [ ] `export_duckdb` still opens in a stock DuckDB
- [ ] no `unwrap()` / `expect()` / `panic!()` added to library code
- [ ] every new public item has a doc comment that says *why*
- [ ] anything that got slower, or is not what its name implies, is **written down**

## The one thing to know before changing durability

**Do not order the commit yourself.** `flock-core/src/store.rs` routes every commit through
`substrate_wal::DurableStore`, which fsyncs a CRC-protected WAL record *before* installing the
manifest. That record IS the commit point, and substrate holds it to that across 50,000 randomized
crash-and-recover cycles.

An earlier version of this repo hand-ordered it (pages → install → log) and used `Wal::checkpoint()`
— which *truncates the log* — as if it were a commit. It worked, which is why it survived. If you
find yourself reaching for `Pager::commit` or `Wal::checkpoint` on the write path, stop.

## The one thing to know before changing `flock-kernel`

**DuckDB does not let us give it a filesystem.** Its C++ core has the seam
(`VirtualFileSystem::RegisterSubSystem`), the C API does not expose it, and `duckdb-rs` binds only
the C API. So DuckDB owns a temp file and we sync file ↔ pages at transaction boundaries.

That single fact is the cause of every limitation in this repository: `fork` being O(database) in
the kernel, `snapshot` reading the whole file, and writes between snapshots not being durable. It is
written up in full at the top of `crates/flock-kernel/src/lib.rs`. Read that before you propose a
change to the sync path, and **before you believe a claim in this README that sounds too good**.
