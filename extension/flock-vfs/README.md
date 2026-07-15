# flock-vfs — the DuckDB C++ FileSystem extension (F4 step 2)

This is the production mechanism RISK-1 (`docs/wake-latency.md`) settled on: a page-faulting read path
for DuckDB, delivered as a **C++ loadable extension**, not FUSE and not a C extension.

## Why C++, and why an extension at all

FlockDB's stock DuckDB C API has **no filesystem-registration hook**. F4 verified this against DuckDB
1.10504's C header, its C loadable-extension API struct, and `duckdb-rs` (the full write-up is at the
top of `crates/flock-kernel/src/lib.rs`). The only seam that registers a filesystem —
`VirtualFileSystem::RegisterSubSystem(unique_ptr<FileSystem>)`, the same one `httpfs` uses for `s3://`
— exists **only in DuckDB's C++ core**. So a page-faulting `FileSystem` can be installed only from C++.

This extension registers a `FlockFileSystem` that intercepts paths of the form:

```
flock://<pool>/<manifest_hex>?remote=<dir>&cache=<dir>&page_size=<n>&len=<n>
```

On open it wakes the database via the **fuzzed** `flock-vfs` Rust cdylib (`crates/flock-vfs`), and every
read DuckDB issues is forwarded to `flock_vfs_pread` — the exact boundary the F5 interpose spike
measured (`spikes/wake-latency/`) and the proptest + libFuzzer harness hardened. All offset/length
arithmetic and the only unsafe buffer handling live on the Rust side, behind the fuzz gate. The C++ does
no safety-critical work of its own: it parses a URI, holds a handle, and forwards reads.

## Files

| file | what it is |
|---|---|
| `include/flock_vfs_ffi.h` | the C ABI of the Rust cdylib (`flock_vfs_open/len/pread/close`) |
| `src/flock_file_system.hpp/.cpp` | `FlockFileSystem : FileSystem` + `FlockFileHandle : FileHandle`, read-only |
| `src/flock_vfs_extension.cpp` | the loadable-extension entry point — the `RegisterSubSystem` call |
| `CMakeLists.txt` | a standalone build recipe |

## Status in this sandbox — verified compiles, NOT a signed load

**What was done here, honestly:**

- Both C++ translation units **compile cleanly** (`clang++ -std=c++17 -fsyntax-only`) against the *real*
  DuckDB 1.10504 headers (extracted from the `libduckdb-sys` bundle's `duckdb.tar.gz`). Every override
  signature — `OpenFile`, both `Read` overloads, `GetFileSize`, `Seek`, `CanHandleFile`, `GetName`,
  `RegisterSubSystem` — matches the actual C++ API. The mechanism is real, not sketched.
- The Rust boundary it calls (`crates/flock-vfs`) is fuzzed green: 300k proptest cases + ~3.0M libFuzzer
  iterations, zero findings, plus a real substrate seed→sleep→wake→read round-trip that matches
  byte-for-byte.

**What was NOT done here, and why — stated plainly (CLAUDE.md rule 6):**

- **No signed `.duckdb_extension` was built or loaded in-sandbox.** A loadable-extension *build+load*
  needs (1) DuckDB's `extension-ci-tools` to append the per-platform metadata footer, (2) a DuckDB built
  with `allow_unsigned_extensions` (or a real signing key), and (3) linking the full DuckDB C++ library
  — a pipeline that is not present here, on a host with <2 GB free disk. Faking a "working load" would
  violate rule 6, so this ships as the compiled-and-verified shim + recipe instead.
- **No wake→first-query was re-measured through the extension.** Because it was not loaded. The F5
  **interpose floor stands as the faithful proxy**: it intercepts at the identical file-read boundary,
  **in-process with the same locality** this extension has (no kernel hop), so the ~40–105 ms flat
  wake-one-of-many floor it measured is what this extension would deliver at the same zero-network floor.
  That equivalence is argued in `docs/wake-latency.md` and `spikes/wake-latency/README.md`.
- **No 250 ms number** is claimed anywhere. The floor is a zero-network lower bound; the real
  object-storage number is still unmeasured (F4 step 3).

## Production pipeline — exactly what a real build needs

1. **Toolchain & version pin.** A C++17 compiler and the DuckDB source/headers for the **exact** version
   FlockDB links (currently `1.10504`, pinned in the root `Cargo.toml`). The extension's version string
   (`flock_vfs_version`) is derived from `DuckDB::LibraryVersion()`, and the loader refuses a mismatch.
2. **Build.** Use DuckDB's official **extension template** + `extension-ci-tools` (preferred, handles the
   footer/signing), or the standalone `CMakeLists.txt` here for a local build. Build the Rust cdylib
   first: `cargo build -p flock-vfs --release`.
3. **Per-platform signed build.** DuckDB extensions are versioned and **signed per (platform, arch)**.
   Ship one artifact per target (`osx_arm64`, `osx_amd64`, `linux_amd64`, `linux_arm64`, …). Either sign
   with a key DuckDB trusts, or append the unsigned metadata footer and require operators to
   `SET allow_unsigned_extensions=true`.
4. **Load & use.**
   ```sql
   SET allow_unsigned_extensions=true;   -- unsigned builds only
   LOAD 'flock_vfs';
   ATTACH 'flock://pool/<manifest_hex>?remote=/tier&cache=/scratch&page_size=65536&len=1500000'
     AS db (READ_ONLY);
   SELECT count(*) FROM db.main.some_table;   -- served by substrate page faults
   ```
5. **Non-negotiable, and already paid.** The fuzz harness over the FFI read boundary
   (`crates/flock-vfs`) — the price of the FFI seam that substrate's in-tree fuzzing does not cover
   (F1's standing objection). It is green; keep it in CI.

## Object-storage backend

This build wires a `LocalFileSystem` tier inside `flock_vfs_open` (the zero-network floor the spike
measured). A production extension parameterizes the object store by URL (S3 via `object_store`'s AWS
backend). That is a backend swap **below** the read path — the fuzzed `serve_read` boundary is unchanged
— and it is what F4 step 3 (`wake_latency_against_a_real_s3_endpoint`) exists to measure.
