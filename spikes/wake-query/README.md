# Wake-query spike — wake → first QUERY RESULT, through the LOADED C++ extension

This is the successor to `spikes/wake-latency/` and the next measurement RISK-1 (`docs/wake-latency.md`)
needs. That spike clocked wake → first *page read* through a dyld interpose shim. This one goes all the
way to wake → first *query result*: **stock DuckDB opens a sleeping database at a `flock://` path,
plans, executes, and returns rows — through the real `FlockFileSystem` C++ extension** (`extension/
flock-vfs/`), with every page faulted from substrate's tier on demand.

> **The one number:** wall-clock time from *"wake a sleeping database"* to *"first row of a point query
> on a small table in a larger database returned"*, through the loaded extension — and that it stays
> **FLAT as the database grows**, against a full-scan control and an eager-hydration control that are
> both **O(database)**.

## What it is (and is NOT)

- It **loads the extension's FileSystem into a real DuckDB** and serves real queries. Two load paths:
  - `register`: installs `FlockFileSystem` via `VirtualFileSystem::RegisterSubSystem` — **the exact body
    of the extension's `LoadInternal`** (`flock_vfs_extension.cpp`). This carries the measurement.
  - `load`: additionally builds the packaged `flock_vfs.duckdb_extension` (metadata footer, unsigned) and
    `LOAD`s it with `allow_unsigned_extensions`, to prove the *packaged loadable* form loads and serves.
- Every faulted result is checked **byte-for-byte against stock DuckDB on the real file** (the `expected`
  mode), so these are correctness-verified, not merely fast.
- The fault count is read from the extension's new `flock_vfs_tier_misses` FFI — substrate's own
  `TierStats.misses`, i.e. **real object-storage GETs** — so "the pages were faulted from the tier, not
  the file hydrated" is *proven*, not asserted.
- **Two tiers, never conflated.** Every size is measured twice: against the `LocalFileSystem` **floor**
  (network at zero), and — when `FLOCK_VFS_S3_URL` is set — against a **same-runner MinIO** endpoint (a
  real object store, but a *low-latency, datacenter-local* one, stated as such). A wide-area bucket is
  the follow-on; this spike does not claim one.
- **No 250 ms number.** None is measured, none is quoted.

## Layout

```
seed/            Rust bin (substrate + flock-vfs, NO DuckDB): chunks a plain .duckdb into a TieredStore
                 and sleeps it, via flock-vfs's own `remote_tier` selector (so seed and wake agree).
measure.cpp      the DuckDB side: seed a file; run wake→first-row through the extension (register/load);
                 the eager control; the byte-correct ground truth. Links the crate's own libduckdb.a.
run.sh           builds everything and runs the seed→sleep→wake→query matrix across database sizes.
```

## Run it

It builds the bundled DuckDB static lib, so it wants a machine with disk headroom — it is driven by
`.github/workflows/wake-query.yml` on a GitHub Actions runner, not the dev Mac. Locally, with a release
`libduckdb.a` already built and (optionally) a MinIO endpoint exported:

```bash
bash run.sh                          # LocalFileSystem floor only
FLOCK_VFS_S3_URL=http://localhost:9000 \
FLOCK_VFS_S3_BUCKET=flockdb FLOCK_VFS_S3_KEY_ID=minioadmin FLOCK_VFS_S3_SECRET=minioadmin \
  bash run.sh                        # floor + MinIO pass
```

## What it does NOT build

Per RISK-1 scope: no `flockd`, no registry, no wake-on-query scheduler. Those wait on this number
existing and holding. This spike measures wake→query and proves it is flat; it does not build the plane
on top of it.
