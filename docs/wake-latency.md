# RISK-1 — wake is O(database), and it gates the 250 ms claim

**Status: OPEN, but the direction is settled, the upside is measured, and the read path is now
built and clocked (against a zero-network floor, not real object storage).**
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

> **F5 update (2026-07): the fault set is now a wall-clock latency, and it is flat.** F4 measured how
> many pages wake faults; F5 built a **real page-faulting read path** and clocked wake→first-row on
> stock DuckDB across databases from 1.5 MB to 59 MB (`spikes/wake-latency/`). **(1) Wake-one-of-many
> is flat in time, not just in bytes.** A point lookup wakes its database and returns a row in
> **~40–105 ms regardless of database size**, faulting a flat ~14 pages; today's eager-hydration wake
> is **O(database)** on the same rig — 0.12 s at 1.5 MB, **11.6 s at 59 MB** — and the full-scan
> control scales too, exactly as the caveat requires. **(2) The read path was built without FUSE.**
> macFUSE (a privileged kernel extension) was not installable, so the path was built at the *same
> file-read boundary a FUSE handler sits at* but in-process, via the dyld `__interpose` mechanism —
> which makes the floor a *lower bound* on FUSE's (FUSE adds a kernel hop per fault) and a faithful
> proxy for an in-process **C++ FileSystem extension**, the recommended production mechanism (see
> "Which read path to build", below). **(3) The floor is a floor.** ~100 ms for the point case **with
> the network at zero** — it already eats ~40 % of a 250 ms budget before a single S3 round-trip.
> **Still not measured against real object storage, and still forbidden to quote: any 250 ms number.**
> F5 quotes none.

> **F4 update (2026-07): the read path is now productionized and its FFI boundary is fuzzed; the C++
> extension is verified to compile against the real DuckDB API; the S3 number is still unmeasured — now
> blocked by disk, not network.** Three things changed.
> **(1) The read path is a real crate, and its trust surface is bought back.** The F5 spike's read logic
> is lifted into `crates/flock-vfs/` — `serve_read((total_len, offset, buf)) → bytes via a PageSource`,
> pure and safe-slice-only, with the substrate-backed `TieredPageSource` and the C ABI the extension
> links. It is **fuzzed hard**, because it is the new FFI trust surface F1 objected to: a proptest oracle
> ran **300,000** adversarial cases (malformed/huge/overflowing offsets, past-EOF, zero-length,
> page-straddling reads, short/oversize/missing pages) and a coverage-guided **libFuzzer** target ran
> **3,028,283** iterations — **zero panics, zero wrong-byte results, zero findings** — plus a real
> substrate seed→sleep→wake→read round-trip that matches byte-for-byte. The boundary never panics, never
> reads out of bounds, and refuses a corrupt store rather than serve the wrong page.
> **(2) The C++ FileSystem extension is real, not sketched.** `extension/flock-vfs/` is the
> `FlockFileSystem : FileSystem` subclass + the `RegisterSubSystem` entry point, and **both translation
> units compile cleanly against the actual DuckDB 1.10504 C++ headers** (extracted from the
> `libduckdb-sys` bundle). Every override signature matches. It was **not** built or loaded as a signed
> extension in-sandbox (needs `extension-ci-tools`, per-platform signing, and a full DuckDB link — not
> available here), so wake was **not** re-measured through it; the F5 in-process interpose floor stands
> as the faithful proxy (same locality, no kernel hop), and this now adds compile-verification that the
> C++ matches the real API.
> **(3) The object-storage number is STILL unmeasured — and the reason moved from network to disk.**
> Unlike F1 (where the network was wholly blocked), a real **MinIO S3 endpoint was stood up here**
> (`/minio/health/live` → 200). But the measurement could not complete on this near-full shared machine:
> flock-core's DuckDB-linked test binary could not be linked within the disk headroom, and MinIO itself
> returned **HTTP 507 Insufficient Storage** on writes because its backing store shares the same
> near-full volume. A DuckDB-free read-path measurement (`crates/flock-vfs/tests/s3_measure.rs`) is now
> committed for anyone with a bucket and adequate disk. **Still forbidden, still absent: any 250 ms
> number.**
>
> **(4) The measurement now has a home: CI, not the dev Mac.** The disk is a *host* problem — DuckDB's
> debug build refills whatever `cargo clean` frees — so the object-storage number is taken by a GitHub
> Actions workflow (`.github/workflows/wake-latency.yml`): it reclaims ~30 GB of preinstalled runner
> toolchains, stands MinIO up as a service container, and runs `s3_measure` **in release** (a fraction
> of the debug footprint). That puts the honesty-critical number in-repo and reproducible — where it
> belongs — rather than on one laptop. **The number is not quoted here until that workflow has produced
> it end-to-end.**

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

**The read path's object-storage round-trip: NOW MEASURED (CI).** The
`.github/workflows/wake-latency.yml` workflow stood MinIO up on a GitHub Actions runner and ran the
DuckDB-free `s3_measure` test against it. The result — wake → first **page read**, through a real
S3-compatible endpoint:

| step | time | S3 GETs |
|---|---:|---:|
| wake (fetch head manifest) | 3.0 ms | — |
| first page fault (4 KiB) | 1.4 ms | — |
| second point fault (256 B) | 1.3 ms | — |
| **wake + first read** | **4.3 ms** | **2** |

Read this precisely, because the scope is the whole point:
- **This is wake → first page *read*, NOT wake → first *query*.** It measures the object-storage
  round-trip the read path pays — the piece the zero-network floor could not — not the full path
  through DuckDB's open + plan + execute. That is the honest thing this workflow can measure without a
  DuckDB build, and it is exactly the piece RISK-1 was uncertain about.
- **Only 2 S3 GETs** for wake + a point read, which is the flat, small fault set (the F4/F5 proof) now
  confirmed end-to-end against real object storage: the page-faulting read path fetches the pages it
  needs and nothing else.
- **It is a same-runner MinIO, not wide-area S3.** The network hop is a datacenter-local one, so 4.3 ms
  is a *low-latency-endpoint* number, not a cross-region one. A geographically distant bucket adds real
  first-byte latency per GET on top.
- **Still no 250 ms claim.** The full wake → first-*query* number, and a wide-area bucket, remain
  unmeasured. What this establishes is narrower and solid: the read path's object-storage overhead is
  **two small GETs, not a whole-file download** — which is the load-bearing question, and the answer is
  the one the fault-set proof predicted.

**The earlier blocker, for the record — disk, not network.** During F1 no S3 endpoint was
reachable at all (Docker would not start headlessly; downloading MinIO was denied). During **F4, a real
MinIO S3 endpoint *was* stood up** — downloaded, started, `/minio/health/live` returned 200 — so the
network was no longer the wall. What blocked the number on the *dev host* was **disk**, on a near-full
shared machine:
(a) flock-core's DuckDB-linked test binary (`wake_latency_against_a_real_s3_endpoint`) could not be
*linked* within the available headroom, and (b) MinIO returned **HTTP 507 Insufficient Storage** on
writes because its backing store shared the same near-full volume. Two `#[ignore]`d tests yield the
number in one command for anyone with a bucket and adequate disk: the wake→first-*query* one in
`flock-core` (`crates/flock-core/tests/tiering.rs`), and a DuckDB-free wake→first-*page-read* one for the
read path (`crates/flock-vfs/tests/s3_measure.rs`). Those are mechanisms, not promises, and neither is a
number yet.

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

## What F5 measured: the fault set, now a wall-clock latency, and it is flat

F4 stopped at "how many pages". F5 built a **real page-faulting read path** — stock DuckDB opens a
database file, and every `pread` it issues on that file is served, page by page, from substrate's
`TieredStore` on demand — and clocked **wake→first-row** across databases from 1.5 MB to 59 MB
(`spikes/wake-latency/`). The tier is substrate's `LocalFileSystem` object store: a real
`object_store` code path with the **network at exactly zero**, so every number is a **floor**.

macFUSE was not installable in the spike environment (a privileged kernel extension), so the read
path was built at the *same file-read boundary a FUSE handler would sit at* but **in-process**, via
the dyld `__interpose` mechanism — the same interception point the F4 trace used, turned from a
tracer into a server. That makes this floor a **lower bound on FUSE's** (FUSE adds a kernel round-trip
per fault) and a faithful **proxy for an in-process C++ FileSystem extension** (same locality).

Every lazy/eager result was checked byte-for-byte against the ground truth, so these are
correctness-verified, not merely fast.

**Wake→first-row latency (ms, zero network, min of cold runs):**

| database | `point`/lazy | `hot`/lazy | `open`/lazy | `cold`/lazy (full scan) | eager = today's hydrate |
|---:|---:|---:|---:|---:|---:|
| 1.5 MB | 58 | 45 | 39 | 59 | 116 |
| 8 MB | 96 | 37 | 31 | 248 | 631 |
| 30 MB | 97 | 43 | 35 | 1,079 | 2,588 |
| 59 MB | **105** | **51** | **55** | **3,792** | **11,644** |
| shape | **FLAT** | **FLAT** | **FLAT** | **scales** | **scales O(database)** |

And the fault counts behind them — deterministic, identical across every run, the direct wall-clock
confirmation of F4's fault-set table:

| database | `open` | `hot` | `point` | `cold` | eager |
|---:|---:|---:|---:|---:|---:|
| 1.5 MB | 3 | 5 | 8 | 8 | 17 |
| 59 MB | 4 | 5 | **14** | **335** | **942** |
| KiB | ~256, flat | 320, flat | **896, flat** | scales | scales |

Read it against the floor table in "What is actually measured":

- **Wake-one-of-many is flat in wall-clock time, not just in bytes.** A point or small-table query
  wakes its database and returns a row in **~40–105 ms whether the database is 1.5 MB or 59 MB**,
  because it faults a flat ~4–14 pages. That is the claim RISK-1 exists to test, and it holds.
- **Today's wake is O(database), and the spike reproduces it as the control:** eager hydration goes
  0.12 s → **11.6 s** across the same 1.5 → 59 MB, and keeps climbing. The flat curve means nothing
  without this rising one beside it (the AT-011 lesson: a single number in isolation proves nothing).
- **The full-scan caveat survives as a measured fact.** `cold`/lazy is *not* flat — 59 ms → 3.8 s.
  Being lazy does not make a full scan cheap, and no read path can. The honest contrast is the result.
- **The floor is a floor.** ~100 ms for the point case is with the network at zero, dominated by
  per-fault synchronous overhead (~7 ms/fault: `block_on` + a tier get + a BLAKE3 verify), not by page
  count. Real object storage adds first-byte latency **per fault** on top. It **already eats ~40 % of
  a 250 ms budget before a single S3 round-trip.** This is precisely why no 250 ms number is quoted.

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
the temp-file fallback. The remaining routes:

1. **FUSE mount.** Mount a userspace filesystem backed by substrate; open the database at a path on it.
   Stock DuckDB, no patching, and its `pread`s become substrate faults. **Cost:** a FUSE dependency
   (macFUSE / libfuse), a **privileged mount** (macFUSE kext; Linux CAP_SYS_ADMIN / privileged pod), a
   daemon and mount to supervise, and a **kernel round-trip per fault** on the hottest path.
2. **C++ loadable extension** that registers a `FileSystem` subclass calling back into Rust. In-process,
   unprivileged, no kernel hop — the *same* locality the F5 spike measured. **Cost:** shipping and
   **signing a C++ extension per platform**, and putting the most safety-critical bytes behind an FFI
   boundary substrate's in-tree fuzzing does not cover (F1's objection, still valid — it is bought back
   with a fuzz harness over the FFI read boundary, and that is the real price, not the C++).
3. **Patched DuckDB.** Heaviest — a fork to maintain against every DuckDB release.
4. **Wait for the C API to grow the hook.** A future DuckDB C API could add filesystem registration,
   which would reopen the cheap in-process path. Not something to plan around, but worth watching.

### Which read path to build (F5's recommendation)

**Production should target the C++ FileSystem extension (route 2), not FUSE.** Both deliver the same
lazy wake — F5 clocked the in-process floor either would build on — so the choice is operational, and
it is not close for FlockDB's shape. FUSE's privileged mount and per-fault kernel round-trip are the
wrong tax on a dense multi-tenant *"sleep a million databases"* fleet: privileged pods and a
context-switch pair on every page fault. The extension keeps the in-process locality the floor was
measured at, in an unprivileged container. **FUSE is a legitimate dev/PoC vehicle** (faster to stand
up on one node) and a single-tenant fallback — it is not the fleet mechanism. The stock **C API still
has no filesystem hook** (route 2 must be C++, not a C extension). **What is no longer in doubt is
that route 2 works and is flat** — F5 built the equivalent read path and clocked it.

## What this permits, and what it forbids

**Permitted:** F1 and F2 ship with O(database) wake, **stated as a limitation** in the README, not
buried here. Sleep/wake works, is crash-safe, and is genuinely useful for archival — it is simply not
fast enough to make a sleeping database feel live.

**Forbidden until the lazy-wake path is clocked against *real object storage*, not merely built:**
- Publishing, quoting, or demoing a 250 ms wake number. F5 built a page-faulting read path and clocked
  it, but **against a zero-network local tier** — a floor, and one that already spends ~100 ms on the
  point case (~40 % of the budget) before a single S3 round-trip. The number that decides the claim is
  the same measurement against a real object store, which is still the `#[ignore]`d
  `wake_latency_against_a_real_s3_endpoint` test, and is still **not measured**.
- The F4 wake-on-query scheduler, whose premise is that a query can wake its own database inline. F5
  shows a real read path *is* flat and fast at the floor — so the scheduler is no longer blocked on
  *whether* a lazy wake exists, only on that read path being **productionized** (route 2 above) and
  clocked against real object storage. It still cannot be built on the *current* O(database) wake,
  which the F5 control clocked at 11.6 s for a 59 MB database — it would miss its deadline by
  construction.

## How this was found, and then narrowed

Honestly, and before a customer found it — which is the standard. The F1 build measured the floor,
noticed that a sub-megabyte database already ate most of the budget with the network at zero, and said
so rather than reporting the tiering as done.

F4 then did the one thing F1 left open: it measured whether the *fix* would even work, before anyone
spent weeks building it. `proofs/lazy-wake/` traces DuckDB's real read syscalls and shows the fault set
is flat and small for wake and selective queries — so a page-faulting read path is worth building — and
confirms it stays O(scan) for a full table scan — so the pitch must stay honest about what "wake" makes
fast. The mechanism question ("can we even intercept those reads") was re-verified against the shipped
DuckDB sources rather than trusted from F1.

F5 then built the read path F4 argued for — a real page-faulting server, not a trace — and clocked
wake→first-row on stock DuckDB (`spikes/wake-latency/`): wake-one-of-many is flat in wall-clock time
(~40–105 ms, 1.5 → 59 MB), today's eager hydration is O(database) (up to 11.6 s), and a full scan
still scales. It did this *without* FUSE (uninstallable), at the same file-read boundary FUSE uses but
in-process — which also settled *which* read path to build: the in-process C++ FileSystem extension,
not FUSE. Each step measured the next step's premise before paying for it, and each stopped at the
zero-network floor rather than quote a number it had not earned. **The 250 ms line is still unmeasured
against real object storage, and still unquoted.** That is the whole discipline this risk enforces.
