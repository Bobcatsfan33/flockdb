# Wake-latency spike — turning the F4 fault SET into an end-to-end wall-clock LATENCY

This builds on `proofs/lazy-wake/` and RISK-1 (`docs/wake-latency.md`). F4 measured the *fault set*:
DuckDB reads a **flat, small** fraction of the database file to wake and to answer a selective query
(~268–780 KiB, independent of database size). That said a page-faulting read path *would* deliver a
lazy wake — but it did not build one or clock it. This spike builds one and clocks it:

> **The one number:** wall-clock time from *"wake a sleeping database"* to *"first row of a point/
> small query returned"*, and — the load-bearing part — that it stays **flat as the database grows**,
> against an eager-hydration control that is O(database).

## What it is (and what it is NOT)

- It is a **real page-faulting read path**: stock DuckDB opens a database file, and every `pread`
  DuckDB issues on that file is served, page by page, from substrate's `TieredStore` on demand —
  faulting each page from the object-storage tier on a local-cache miss. No hydration; DuckDB pulls
  only the pages its query touches.
- The tier is substrate's `LocalFileSystem` object store — a **real** `object_store` code path with
  the **network at exactly zero**. Every latency here is therefore a **FLOOR**: the honest lower
  bound with no S3 round-trips. It is **not** a real-object-storage number, and it is emphatically
  **not** the 250 ms target. No 250 ms number is quoted anywhere, because none has been measured.

### Why the read path is a DYLD interpose shim and not FUSE

The task named FUSE as the vehicle. **macFUSE was not installable in this environment** — it is a
privileged kernel extension (no macFUSE kext, no libfuse, no `pkg-config`, and installing a signed
kext needs admin + reboot approval a sandbox cannot give). So the read path is built at the *same
boundary a FUSE `read` handler would sit at* — a `pread` on the database file becomes a substrate
page fault — but **in-process, via the dyld `__interpose` mechanism** (the identical interception
point the F4 trace used, `proofs/lazy-wake/readtrace.c`), turned from a *tracer* into a *server*.

This substitution makes the number **more** conservative, not less, and it is a **better** proxy for
the production mechanism we actually recommend:

- FUSE would add one user→kernel→daemon→kernel→user round-trip **per fault** on top of everything
  measured here. So the FUSE floor is strictly **above** this in-process floor.
- The production recommendation (below) is an **in-process C++ DuckDB FileSystem extension**, which
  has the *same* in-process locality as this shim. So this shim's floor is a faithful proxy for the
  extension's, and a lower bound for FUSE's.

## Layout

```
faultshim/         Rust cdylib: the page-faulting server (flock_serve) + wake, backed by TieredStore
  faultglue.c      the __DATA,__interpose plumbing (open/pread/read/lseek/mmap), compiled in
seed/              Rust bin (substrate-only, NO DuckDB): chunks a plain .duckdb into a TieredStore
                   and sleep()s it — the same page layout flock-kernel/src/paging.rs uses
duckharness.c      the DuckDB side: `seed` a file; `wake` it read-only and time wake→first-row.
                   Links the crate's own libduckdb.a (stock DuckDB, unpatched)
run.sh             seeds, sleeps, and runs the lazy/eager/expected matrix across database sizes
```

The database file ↔ substrate page mapping is the dumb one `paging.rs` defines: logical page `i` =
file bytes `[i·ps, (i+1)·ps)`, 64 KiB pages. So a `pread(off, n)` faults pages `off/ps … (off+n)/ps`
and nothing else.

## Run it

```bash
bash run.sh      # needs the release libduckdb.a already built (cargo build --release, in the repo root)
```

Each `(size, query)` runs three ways: **lazy** (fault on demand — the candidate fix), **eager**
(prefetch every page — models today's `paging::hydrate`), and **expected** (the real file, no shim —
ground-truth result). Every lazy/eager result is checked byte-for-byte against the ground truth, so
these are correctness-verified, not just fast.

## Result (measured 2026-07, DuckDB 1.10504, macOS, substrate-v1.2.1, zero-network local tier)

Queries: `open` = `SELECT 1` (pure wake); `hot` = a selective aggregate on a small table; `point` =
a zonemap-pruned point lookup; `cold` = a full-table aggregate (the control). One database file holds
a small `hot` table and a large `cold` table; only `cold` grows across sizes.

### A. Fault set — pages faulted from the tier (deterministic; identical across every run)

This is the direct wall-clock-era confirmation of the F4 fault-set proof.

| database | `open` | `hot` | `point` | `cold` (full scan) | eager (hydrate-all) |
|---:|---:|---:|---:|---:|---:|
| 1.5 MB | 3 | 5 | 8 | 8 | 17 |
| 8 MB | 4 | 5 | 14 | 46 | 122 |
| 30 MB | 4 | 5 | 14 | 169 | 474 |
| 59 MB | 4 | 5 | **14** | **335** | **942** |
| **KiB faulted** | ~256, flat | 320, flat | **896, flat** | 512→21,440, **scales** | 1,088→60,288, **scales** |

`open`/`hot`/`point` fault a **flat, sub-MB** set no matter how big the database is — exactly F4's
finding, now on the real fault path. `cold` (full scan) and `eager` (whole file) **scale with the
database**. Faulting is genuinely lazy; a full scan is genuinely not.

### B. Wake → first-row latency — the FLOOR (ms, zero network, min of cold runs)

| database | `point`/lazy | `hot`/lazy | `open`/lazy | `cold`/lazy (full scan) | eager (hydrate-all) |
|---:|---:|---:|---:|---:|---:|
| 1.5 MB | 58 | 45 | 39 | 59 | 116 |
| 8 MB | 96 | 37 | 31 | 248 | 631 |
| 30 MB | 97 | 43 | 35 | 1,079 | 2,588 |
| 59 MB | **105** | **51** | **55** | **3,792** | **11,644** |
| shape | **FLAT** | **FLAT** | **FLAT** | **scales** | **scales O(database)** |

Read it the way the F4 tables are read:

1. **Wake-one-of-many is flat in wall-clock time.** A point lookup or a small-table query wakes its
   database and returns a row in **~40–105 ms regardless of whether the database is 1.5 MB or 59 MB**,
   because it faults a flat ~4–14 pages. That is the claim, and it holds.
2. **Today's wake (eager hydration) is O(database):** 0.12 s → **11.6 s** as the database grows
   1.5 → 59 MB, and it keeps climbing. This is the cost FlockDB pays now, and the spike reproduces it
   as the control — the flat curve above is meaningful only next to this rising one (the AT-011
   lesson: one number in isolation proves nothing).
3. **The full-scan control is NOT flat, on purpose.** `cold`/lazy scales 59 ms → 3.8 s. Being lazy
   does not make a full scan cheap, and no read path can. The honest contrast is part of the result.

### C. What this floor does and does not say

- The **~100 ms point-query floor is with the network at zero.** It is dominated by per-fault
  synchronous overhead (~7 ms/fault: `block_on` + a `LocalFileSystem` get + a BLAKE3 verify), not by
  the page count — which is an optimization target (batch/parallelize faults), not a wall. Real
  object storage adds first-byte latency (tens of ms) **per fault** on top of this.
- So the floor **already eats ~40 % of a 250 ms budget for the point case before a single network
  round-trip**, and the eager path blows past 250 ms entirely at any interesting size. This is
  exactly why RISK-1 forbids quoting 250 ms until it is measured against real object storage. **This
  spike does not measure that, and does not quote it.**

## Scoped claim (what this spike licenses)

- **Wake-one-of-many for POINT and SMALL queries is lazy** — a flat, sub-MB fault set and a flat
  wake→first-row latency independent of database size (measured 1.5 → 59 MB). The floor is ~100 ms at
  zero network for the point case.
- **A cold full-table scan is O(data), stated plainly** — no read path makes it flat, and this spike
  does not pretend one does.
- **No 250 ms number, anywhere.** The real-object-storage number is still unmeasured.

## FUSE vs a C++ DuckDB FileSystem extension — the production recommendation

Both deliver the *same lazy wake* (both intercept at the file-read boundary and fault substrate
pages); this spike measured the in-process floor that either would build on. The choice is
operational, and the spike + F4's mechanism finding settle it:

| | FUSE mount | **C++ loadable FileSystem extension** |
|---|---|---|
| Mechanism | userspace filesystem; DuckDB opens a path on it | `VirtualFileSystem::RegisterSubSystem` (how `httpfs` does `s3://`), read() → Rust |
| Locality | out-of-process daemon; **kernel round-trip per fault** | **in-process; no kernel hop** (this spike's floor is its proxy) |
| Deploy | **privileged mount** (macFUSE kext; Linux CAP_SYS_ADMIN / privileged pod); a mount to supervise | unprivileged; ship + **sign a per-platform extension** |
| Blast radius | well-defined kernel VFS boundary | most safety-critical bytes cross an **FFI seam** that substrate's in-tree fuzzing does not cover (F1's standing objection) |
| Fit for a dense multi-tenant fleet | poor — privileged mounts and a per-fault kernel hop are the wrong tax on "sleep a million databases" | good — in-process, unprivileged, container-friendly |

**Recommendation: production should target the C++ FileSystem extension, not FUSE.** The spike shows
the in-process fault path delivers the flat wake at a low floor; the extension keeps that in-process
locality without FUSE's privileged mount and per-fault kernel round-trip, which are disqualifying for
the fleet economics FlockDB is pitched on. FUSE remains a legitimate **dev/PoC vehicle** (faster to
stand up on a single node) and a single-tenant fallback.

**Cost to build the extension** (a bounded, known project — not a weekend):

1. A C++ shim registering a `FileSystem` subclass whose `Read`/`OpenFile` call a Rust cdylib — the
   productionized `flock_serve` measured here.
2. A per-platform **signed-extension** build + release pipeline (DuckDB extensions are versioned and
   signed per platform/arch).
3. **Non-negotiable:** a fuzz harness over the FFI read boundary, to buy back the escape from
   substrate's in-tree differential/crash fuzzing that the FFI seam creates. This is F1's objection,
   and it is the real price, not the C++.

The stock **C API still has no filesystem-registration hook** (F4 re-verified against DuckDB 1.10504
and `duckdb-rs`), so the extension must be **C++**, not a C extension. "Wait for the C API to grow
`duckdb_register_file_system`" stays the cheapest future option to watch.

## What this spike does NOT build

Per RISK-1 scope: no `flockd`, no registry, no wake-on-query scheduler. Those wait on a real read
path built against real object storage. This spike proves the wake *path* is lazy and flat, and
tells the human which read path to build — it does not build the plane on top of it.
