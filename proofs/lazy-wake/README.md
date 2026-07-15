# Lazy-wake proof — does DuckDB read the whole file to answer a query?

This is the F4 first-task investigation into RISK-1 (`docs/wake-latency.md`): FlockDB's
pitch is "sleep a million databases, wake one in < 250 ms," which assumes wake is
O(pages the query touches). Today FlockDB's wake is O(database) because
`crates/flock-kernel/src/paging.rs::hydrate` materialises the *entire* file before
DuckDB opens it. The candidate fix is a page-faulting VFS so DuckDB's reads become
substrate page reads. That fix only pays off if **DuckDB itself reads a small
fraction of the file** for a selective query. This measures whether it does.

## What it measures, and why this way

A page-faulting VFS would intercept exactly the `pread`/`read`/`mmap` calls DuckDB
issues against the database file, and fault only those byte ranges from substrate.
So the set of ranges DuckDB reads *is* the set of pages a lazy wake would fault.
`readtrace.c` is a `DYLD_INTERPOSE` shim that records those ranges against the
crate's own `libduckdb.a` — the real DuckDB, not a model. `harness.c` seeds one
database file holding a small `hot` table and a large, incompressible `cold` table,
then (in a fresh, cold process) runs one query while the shim traces it.

The shim is a **measurement instrument, not a shippable mechanism**:
`DYLD_INSERT_LIBRARIES` is not a way to serve pages from substrate, it just proves
the read set is small.

## Run it

```bash
bash run.sh      # needs the release libduckdb.a already built (cargo build --release)
```

Header path defaults to a scratch extraction of `duckdb.h` from the `libduckdb-sys`
crate tarball; override with `DUCKDB_HDR_DIR` if needed.

## Result (measured 2026-07, DuckDB 1.10504, macOS, in-memory OS cache)

`uniq_touched` = union of all byte ranges DuckDB read to answer the query.
DuckDB reads in 256 KiB blocks.

| cold_rows | file size | query | uniq touched | % of file |
|---:|---:|---|---:|---:|
| 0       | 0.5 MB  | open (`SELECT 1`)              | 268 KiB | 51.1 % |
| 100 000 | 3.2 MB  | open                           | 268 KiB | 8.7 % |
| 400 000 | 11 MB   | open                           | 268 KiB | 2.5 % |
| 1 000 000 | 26 MB | open                           | 268 KiB | **1.0 %** |
| 100 000 | 3.2 MB  | hot (`sum(v) FROM hot`)        | 524 KiB | 17.0 % |
| 400 000 | 11 MB   | hot                            | 524 KiB | 4.9 % |
| 1 000 000 | 26 MB | hot                            | 524 KiB | **2.0 %** |
| 100 000 | 3.2 MB  | point (`… FROM cold WHERE id=`)| 780 KiB | 25.3 % |
| 400 000 | 11 MB   | point                          | 780 KiB | 7.2 % |
| 1 000 000 | 26 MB | point                          | 780 KiB | **3.0 %** |
| 100 000 | 3.2 MB  | cold (`sum(v) FROM cold`)      | 1.3 MB  | 41.9 % |
| 400 000 | 11 MB   | cold                           | 3.7 MB  | 33.4 % |
| 1 000 000 | 26 MB | cold                           | 8.9 MB  | 34.0 % |

## What it shows

1. **Wake is not O(database) at the engine level.** Opening a database and running
   `SELECT 1` reads a **flat ~268 KiB** whether the file is 0.5 MB or 26 MB. The
   O(database) wake FlockDB has today is *entirely* an artefact of eager hydration.
2. **A selective query on a small table in a large database reads a flat ~524 KiB**
   — 2 % of a 26 MB file, and it would keep shrinking as a fraction as the file
   grows. This is the wake-one-of-many case, and it is genuinely lazy at the engine.
3. **A point lookup is zonemap-pruned to a flat ~780 KiB** regardless of table size —
   DuckDB skips the row groups whose min/max cannot match.
4. **A full-table aggregate is NOT flat** — `cold` scales with the column it scans
   (1.3 → 3.7 → 8.9 MB). This confirms `docs/wake-latency.md`'s caveat: *being lazy
   is not sufficient if the query is not.* A page-faulting VFS makes wake and
   selective queries cheap; it does not make a full scan cheap.

## What it does NOT show

- **No 250 ms number.** This measures the *fault set* (how many pages), not fault
  *latency* (object-storage round-trips per 256 KiB block × p99). The wake-latency
  budget still has to be measured against real object storage once a VFS exists.
- **The catalog read (~268 KiB) is for two tables.** A database with thousands of
  objects reads a larger catalog on wake; catalog cost scales with schema size, not
  data size, but it is not zero.
