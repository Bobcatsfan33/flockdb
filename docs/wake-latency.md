# RISK-1 — wake is O(database), and it gates the 250 ms claim

**Status: OPEN. Tracked, not solved.**
**Owner: F4 (wake-on-query scheduler).**
**Blocks: any public "wake one in 250 ms" claim, and any demo of one.**

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

## The candidate fix, and why it is the shape of the answer

**A page-faulting VFS.** DuckDB supports a pluggable filesystem/storage layer. If FlockDB implements one
that faults pages **from substrate on demand** — rather than materialising a file up front — then
DuckDB's reads become substrate reads, and FlockDB inherits substrate's laziness instead of defeating
it. Wake becomes O(pages the query touches), which is the shape the 250 ms target needs.

Open questions, none of them answered yet:
- Does DuckDB's storage extension surface give us page-granular reads, or does it re-materialise
  internally at some layer we do not control?
- DuckDB's plans may scan far more of the file than the answer needs, so "pages touched" could still be
  most of the database for a table scan. **Being lazy is not sufficient if the query is not.**
- A cold page fault mid-query means a query that blocks on object storage. That is a latency
  *distribution* problem, not just a mean problem, and p99 is what a user feels.

Until those are answered, **assume the fix is unproven.**

## What this permits, and what it forbids

**Permitted:** F1 and F2 ship with O(database) wake, **stated as a limitation** in the README, not
buried here. Sleep/wake works, is crash-safe, and is genuinely useful for archival — it is simply not
fast enough to make a sleeping database feel live.

**Forbidden until the lazy-wake path is proven:**
- Publishing, quoting, or demoing a 250 ms wake number.
- The F4 wake-on-query scheduler, whose entire premise is that a query can wake its own database inline.
  It cannot be built on an O(database) wake; it would be a scheduler for an operation that misses its
  deadline by construction.

## How this was found

Honestly, and before a customer found it — which is the standard. The F1 build measured the floor,
noticed that a sub-megabyte database already ate most of the budget with the network at zero, and said
so rather than reporting the tiering as done.
