# RISK-1 — wake is O(database), and it gates the 250 ms claim

**Status: OPEN, but the direction is now settled and the upside is measured.**
**Owner: F4 (wake-on-query scheduler).**
**Blocks: any public "wake one in 250 ms" claim, and any demo of one.**

> **F4 update (2026-07).** Two things are now known that were open before.
> **(1) The mechanism.** The in-process fix is *impossible* through the stock DuckDB
> C API — verified against the shipped header, the C loadable-extension API struct,
> and `duckdb-rs`, not assumed (see "The mechanism" below). A page-faulting read path
> exists only outside that API: a C++ loadable extension, a patched DuckDB, or a FUSE
> mount. **(2) The upside.** It is now *measured* — not assumed — that DuckDB reads a
> **flat, small fraction** of the file to wake and to answer a selective query
> (`proofs/lazy-wake/`). So the O(database) wake FlockDB has today is an artefact of
> eager hydration, and a page-faulting read path would in fact deliver a lazy wake
> for the wake-one-of-many case. The full-table-scan caveat below is also confirmed.
> **Still not measured, and still forbidden to quote: any 250 ms number.**

---

## The claim this risk is about

FlockDB's economic pitch is **sleep a million databases, wake one cheaply**. Not "sleeping is cheap" —
everything sleeps cheaply — but that the *wake* is fast enough that a sleeping database is
indistinguishable from a live one at query time. The target is **< 250 ms to first query.**

That number is the product. If waking takes two seconds, nobody sleeps anything, the tiering is dead
weight, and FlockDB is a slower DuckDB with extra steps.

## Why it does not hold today

**Substrate's wake is lazy.** It faults pages in on demand — you wake a database, you touch a few pages,
you pay for a few pages. Cost is O(pages touched). This is the right shape, and it is already built.

**FlockDB defeats it.** DuckDB needs a **whole database file**. So waking a FlockDB database
materialises *every* page, whether the query touches it or not. Cost is **O(database)**, not O(pages
touched) — and O(database) does not have a 250 ms bound at any interesting size, no matter how fast the
network is.

## What is actually measured

**Wake against a real S3 endpoint: NOT MEASURED.** No S3 endpoint was reachable during F1 (Docker would
not start headlessly; downloading MinIO was denied by the sandbox). An `#[ignore]`d test,
`wake_latency_against_a_real_s3_endpoint`, yields the number in one command for anyone with a bucket.
That is a mechanism, not a promise, and it is not a number.

**What we could measure is the floor** — against a **zero-latency, in-process** object store, i.e. with
the network at exactly zero:

| database size | local open → first query | **wake → first query** |
|---:|---:|---:|
| ~210 KB | 20 ms | **95 ms** |
| ~210 KB | 17 ms | **79 ms** |
| ~800 KB | 31 ms | **199 ms** |

Read that table carefully. On a **sub-megabyte** database, with **zero network latency**, waking already
consumes 79–199 ms of a 250 ms budget. Real object storage has a first-byte latency of tens of
milliseconds *before* it transfers anything. The budget is gone before the data moves.

**This is a floor, not an estimate of the real number.** The real number is worse and we have not taken
it.

## What F4 measured: DuckDB's fault set is flat, so the fix would work

The candidate fix is a **page-faulting read path**: serve DuckDB's file reads from substrate pages on
demand instead of materialising the whole file up front. That only pays off if DuckDB *itself* reads a
small fraction of the file for a selective query. **That is now measured** — `proofs/lazy-wake/`
records the exact byte ranges DuckDB reads (via a `DYLD_INTERPOSE` shim over the crate's own
`libduckdb.a`) as a database grows from 0.5 MB to 26 MB, holding a small `hot` table and a large
`cold` table in one file.

| query | what it does | bytes DuckDB reads | as the file grows 0.5 → 26 MB |
|---|---|---:|---|
| `SELECT 1` | pure wake, no table | **~268 KiB, flat** | 51 % → **1.0 %** of the file |
| `sum(v) FROM hot` | selective — a small table | **~524 KiB, flat** | 17 % → **2.0 %** of the file |
| `… FROM cold WHERE id = ?` | point lookup, zonemap-pruned | **~780 KiB, flat** | 25 % → **3.0 %** of the file |
| `sum(v) FROM cold` | full scan (control) | **1.3 → 8.9 MB, scales** | ~34 % of the file, always |

Read that table the way the floor table above it should be read:

- **Wake is not O(database) at the engine.** Opening the database reads a *flat* ~268 KiB regardless of
  file size. FlockDB's O(database) wake is caused entirely by `paging::hydrate` writing the whole file
  before DuckDB opens — DuckDB never asked for those bytes. **This is the strongest evidence the fix is
  real:** the cost we are paying is one we invented.
- **Waking one of many is genuinely lazy.** A query on a small table inside a large database reads a
  flat ~524 KiB — 2 % of a 26 MB file, and a smaller fraction the bigger the file gets. A page-faulting
  read path would fault only those pages.
- **The caveat survives, exactly as written.** The full scan (`cold`) is *not* flat — it reads O(the
  column it scans). **Being lazy is not sufficient if the query is not.** The fix makes wake and
  selective queries cheap; it does not make a full-table aggregate cheap, and no read path can.

**What this does not measure, and what therefore stays forbidden:** this is the *fault set* (how many
pages), not the *fault latency* (object-storage round-trips per 256 KiB block, and their p99). A cold
page fault mid-query blocks on object storage — a latency-distribution problem, and p99 is what a user
feels. **No 250 ms number until a real read path is built and clocked against real object storage.**

## The mechanism: verified, and the stock in-process path is closed

F1 claimed the C API cannot register a filesystem. F4 re-verified it against the actual DuckDB 1.10504
sources, because the claim gates a real build decision:

- **C++ core has the seam.** `duckdb/common/virtual_file_system.hpp` declares
  `VirtualFileSystem::RegisterSubSystem(unique_ptr<FileSystem>)` — how `httpfs` teaches DuckDB `s3://`.
- **The C API does not expose it.** `duckdb.h`'s filesystem surface is *consumption only*
  (`duckdb_client_context_get_file_system`, `duckdb_file_system_open`, `duckdb_file_handle_read/…`).
  The complete set of registration entry points is
  `duckdb_register_{scalar,aggregate,cast,copy,table}_function`, `_function_set`, `_logical_type`,
  `_config_option`, `_log_storage`. **There is no `duckdb_register_file_system`.**
- **The C loadable-extension API cannot do it either.** The `duckdb_ext_api_v1` struct
  (`capi/extension_api.hpp`) exposes the *same* consumption-only filesystem functions and the *same*
  register set — so even a C extension gets no filesystem-registration hook.
- **`duckdb-rs` binds only the C API.** One grep hit for "filesystem" in its `src/`, a test comment.

So the in-process, stock-API fix is genuinely **impossible today**, and this is the reason FlockDB uses
the temp-file fallback. The remaining routes, cheapest first:

1. **FUSE mount.** Mount a userspace filesystem backed by substrate; open the database at a path on it.
   Stock DuckDB, no patching, and its `pread`s become substrate faults — the measured read set says this
   would be lazy. **Cost:** a FUSE dependency (macFUSE / libfuse), a mount to manage, and a kernel
   round-trip per fault (latency, and a scheduling boundary), on the hottest path in the product.
2. **C++ loadable extension** that registers a `FileSystem` subclass calling back into Rust. **Cost:**
   shipping and signing a C++ extension per platform, and putting the most safety-critical bytes in the
   system behind an FFI boundary we cannot fuzz with the rest of the engine (F1's objection, still valid).
3. **Patched DuckDB.** Heaviest — a fork to maintain against every DuckDB release.
4. **Wait for the C API to grow the hook.** A future DuckDB C API could add filesystem registration,
   which would reopen the cheap in-process path. Not something to plan around, but worth watching.

None of these is "just write an extension." Each is a real project with a real maintenance bill, and the
choice between them is a decision for a human, not a default. **What is no longer in doubt is that one of
them would work** — the fault set is small and flat, so a lazy read path yields a lazy wake.

## What this permits, and what it forbids

**Permitted:** F1 and F2 ship with O(database) wake, **stated as a limitation** in the README, not
buried here. Sleep/wake works, is crash-safe, and is genuinely useful for archival — it is simply not
fast enough to make a sleeping database feel live.

**Forbidden until the lazy-wake path is *built*, not merely shown to be possible:**
- Publishing, quoting, or demoing a 250 ms wake number. F4 proved the fault set is small; it did **not**
  build a read path or clock one against object storage. Possible is not measured.
- The F4 wake-on-query scheduler, whose entire premise is that a query can wake its own database inline.
  It still cannot be built on the *current* O(database) wake — that wake would miss its deadline by
  construction. The scheduler waits on a real page-faulting read path (FUSE or C++ extension), not on
  this proof. The proof unblocks the *decision to build that path*; it does not replace it.

## How this was found, and then narrowed

Honestly, and before a customer found it — which is the standard. The F1 build measured the floor,
noticed that a sub-megabyte database already ate most of the budget with the network at zero, and said
so rather than reporting the tiering as done.

F4 then did the one thing F1 left open: it measured whether the *fix* would even work, before anyone
spent weeks building it. `proofs/lazy-wake/` traces DuckDB's real read syscalls and shows the fault set
is flat and small for wake and selective queries — so a page-faulting read path is worth building — and
confirms it stays O(scan) for a full table scan — so the pitch must stay honest about what "wake" makes
fast. The mechanism question ("can we even intercept those reads") was re-verified against the shipped
DuckDB sources rather than trusted from F1. What remains is a human's call on *which* read path to build.
