/*
 * flock_vfs_ffi.h — the C ABI exported by the `flock-vfs` Rust cdylib (crates/flock-vfs).
 *
 * These four functions ARE the read boundary RISK-1 is about, and the Rust behind them is the code the
 * proptest + libFuzzer harness proves cannot be crashed or confused by a malformed request. The C++
 * FileSystem subclass in this directory does nothing safety-critical itself: it parses a `flock://` URI,
 * calls flock_vfs_open, and forwards each DuckDB read to flock_vfs_pread. All the offset/length
 * arithmetic and the only unsafe buffer handling live on the Rust side, behind the fuzz gate.
 *
 * Link against libflock_vfs.{dylib,so} (built with `cargo build -p flock-vfs --release`).
 */
#ifndef FLOCK_VFS_FFI_H
#define FLOCK_VFS_FFI_H

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h> /* ssize_t */

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque woken-database handle. Layout is not ABI; only the pointer crosses the boundary. */
typedef struct FlockVfs FlockVfs;

/*
 * Wake a sleeping database and return a handle, or NULL on failure (a diagnostic is written to stderr).
 *   remote_dir   : local object-store tier root (this build wires a LocalFileSystem tier).
 *   cache_dir    : scratch dir for faulted pages.
 *   pool         : the WakeToken pool.
 *   manifest_hex : the WakeToken manifest id, 64-char hex.
 *   page_size    : the WakeToken page size (bytes).
 *   total_len    : the database file length (bytes), from the sleep record.
 * The returned handle must be freed exactly once with flock_vfs_close.
 */
FlockVfs *flock_vfs_open(const char *remote_dir, const char *cache_dir, const char *pool,
                         const char *manifest_hex, size_t page_size, uint64_t total_len);

/* The database file length in bytes (what DuckDB fstat()s). -1 on a NULL handle or i64 overflow. */
int64_t flock_vfs_len(const FlockVfs *handle);

/*
 * Observability: pages this handle faulted from the object-storage tier (tier GETs / cache misses),
 * and pages served from the local cache (hits). -1 on a NULL handle. These read substrate's own
 * TierStats and perform no I/O. They exist so a host can PROVE the lazy-wake claim end to end: a point
 * query on a small table in a large database faults a flat, small number of pages regardless of size.
 */
int64_t flock_vfs_tier_misses(const FlockVfs *handle);
int64_t flock_vfs_tier_hits(const FlockVfs *handle);

/*
 * Serve `len` bytes at `offset` into `buf`, faulting pages on demand. Returns the number of bytes read
 * (0 at/past EOF, fewer than `len` on an end-of-file short read), or -1 on error. A negative `offset`,
 * a NULL handle, or a NULL `buf` with `len > 0` is an error. This is the fuzzed boundary.
 */
ssize_t flock_vfs_pread(const FlockVfs *handle, int64_t offset, uint8_t *buf, size_t len);

/* Free a handle from flock_vfs_open. A NULL pointer is ignored. */
void flock_vfs_close(FlockVfs *handle);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* FLOCK_VFS_FFI_H */
