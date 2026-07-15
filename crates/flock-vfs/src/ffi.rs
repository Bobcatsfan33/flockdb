//! The C ABI the DuckDB C++ FileSystem extension (F4 step 2) links against.
//!
//! This is the FFI seam RISK-1 is nervous about, and rightly: it is C++ handing Rust an offset, a
//! length, and a raw buffer. Everything memory-safety-critical past this edge lives in
//! [`serve_read`](crate::read::serve_read), which is fuzzed; this file's job is only to (1) validate
//! every value the C caller passes before trusting it, and (2) turn the caller's raw buffer into a safe
//! `&mut [u8]` so `serve_read` never sees a pointer. The one `unsafe` block that builds that slice is
//! the whole crate's attack surface, and it does nothing but honour the caller's `pread` contract.
//!
//! # Lifecycle, from the extension's point of view
//!
//! ```c
//! FlockVfs *h = flock_vfs_open(remote_dir, cache_dir, pool, manifest_hex, page_size, total_len);
//! if (!h) { /* wake failed; message is on stderr */ }
//! int64_t len = flock_vfs_len(h);                 // what DuckDB fstat()s
//! ssize_t n  = flock_vfs_pread(h, offset, buf, n); // every DuckDB read on the db file
//! flock_vfs_close(h);
//! ```

use crate::error::{Result, VfsError};
use crate::read::serve_read;
use crate::tiered::TieredPageSource;
use object_store::local::LocalFileSystem;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Arc;
use substrate_pager::ManifestId;
use substrate_store::{RemoteTier, TieredStore, WakeToken};

/// An opaque, woken database handle. Created by [`flock_vfs_open`], read by [`flock_vfs_pread`], freed
/// by [`flock_vfs_close`]. Opaque to C on purpose — its layout is not ABI.
pub struct FlockVfs {
    // Field order is drop order: `source` (and the store inside it) is dropped BEFORE `_rt`. The tiered
    // CAS holds a handle to this runtime for its background uploader and its on-miss fetch, so the
    // runtime must outlive the store or an in-flight fault would be aborted from under us.
    source: TieredPageSource,
    _rt: tokio::runtime::Runtime,
}

/// Borrow a C string argument as `&str`, rejecting null and non-UTF-8 rather than trusting them.
///
/// # Safety
/// `ptr` must be null or a valid, NUL-terminated C string that stays alive for the call.
unsafe fn cstr<'a>(ptr: *const c_char, what: &'static str) -> Result<&'a str> {
    if ptr.is_null() {
        return Err(VfsError::BadFfiArgument {
            what,
            detail: "it was a null pointer — pass a valid NUL-terminated string".to_string(),
        });
    }
    // SAFETY: caller-guaranteed valid NUL-terminated string (documented above).
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| VfsError::BadFfiArgument {
            what,
            detail: "it was not valid UTF-8".to_string(),
        })
}

fn open_inner(
    remote_dir: *const c_char,
    cache_dir: *const c_char,
    pool: *const c_char,
    manifest_hex: *const c_char,
    page_size: usize,
    total_len: u64,
) -> Result<FlockVfs> {
    // SAFETY: each pointer is validated (null-checked, UTF-8-checked) inside `cstr`. The `&str`s live
    // only within this function, never past the C strings the caller owns.
    let remote_dir = unsafe { cstr(remote_dir, "remote directory path") }?;
    let cache_dir = unsafe { cstr(cache_dir, "cache directory path") }?;
    let pool = unsafe { cstr(pool, "pool name") }?;
    let manifest_hex = unsafe { cstr(manifest_hex, "manifest id") }?;

    if page_size == 0 {
        return Err(VfsError::BadFfiArgument {
            what: "page size",
            detail: "it was 0 — pass the `page_size` from the database's WakeToken".to_string(),
        });
    }

    let manifest = ManifestId::from_hex(manifest_hex).map_err(|e| VfsError::BadFfiArgument {
        what: "manifest id",
        detail: format!("{e}; expected the 64-char hex id from the sleep record"),
    })?;

    // A fresh cache dir; the wake must pull from the tier, not from a warm disk left by a prior run.
    std::fs::create_dir_all(cache_dir).map_err(|e| VfsError::BadFfiArgument {
        what: "cache directory path",
        detail: format!("could not create it: {e}"),
    })?;

    // The object-storage tier. This build wires a LocalFileSystem tier — the same real `object_store`
    // code path the F5 floor was measured against, and what the S3 test (step 3) swaps for an S3
    // backend. A production extension parameterizes this by an object-store URL; that is a backend
    // choice, not a change to the read path below it.
    let backend =
        LocalFileSystem::new_with_prefix(remote_dir).map_err(|e| VfsError::BadFfiArgument {
            what: "remote directory path",
            detail: format!("could not open it as a local object-store tier: {e}"),
        })?;
    let remote = RemoteTier::new(Arc::new(backend), pool.to_string());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| VfsError::Runtime {
            detail: format!("could not build the multi-threaded tokio runtime: {e}"),
        })?;

    let token = WakeToken {
        pool: pool.to_string(),
        manifest,
        page_size,
    };

    let store = rt
        .block_on(TieredStore::wake(cache_dir, remote, &token))
        .map_err(|source| VfsError::Wake { source })?;

    Ok(FlockVfs {
        source: TieredPageSource::new(Arc::new(store), total_len),
        _rt: rt,
    })
}

/// Wake a sleeping database and return a handle to read it, or null on failure (message on stderr).
///
/// `remote_dir` is the local object-store tier root, `cache_dir` a scratch dir for faulted pages,
/// `pool`/`manifest_hex`/`page_size` the fields of the database's `WakeToken`, and `total_len` the
/// database file's byte length (from the sleep record). The returned handle must be freed with
/// [`flock_vfs_close`].
///
/// # Safety
/// The four pointer arguments must each be null or a valid NUL-terminated C string alive for the call.
/// The returned pointer must be freed exactly once with [`flock_vfs_close`] and never otherwise.
#[no_mangle]
pub unsafe extern "C" fn flock_vfs_open(
    remote_dir: *const c_char,
    cache_dir: *const c_char,
    pool: *const c_char,
    manifest_hex: *const c_char,
    page_size: usize,
    total_len: u64,
) -> *mut FlockVfs {
    match open_inner(
        remote_dir,
        cache_dir,
        pool,
        manifest_hex,
        page_size,
        total_len,
    ) {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(e) => {
            eprintln!("flock_vfs_open: {e}");
            std::ptr::null_mut()
        }
    }
}

/// The database file's length in bytes — what DuckDB `fstat`s. Returns -1 on a null handle or if the
/// length does not fit a signed 64-bit value.
///
/// # Safety
/// `handle` must be null or a pointer returned by [`flock_vfs_open`] and not yet closed.
#[no_mangle]
pub unsafe extern "C" fn flock_vfs_len(handle: *const FlockVfs) -> i64 {
    // SAFETY: caller guarantees `handle` is null or a live open handle.
    match unsafe { handle.as_ref() } {
        Some(h) => i64::try_from(h.source.total_len()).unwrap_or(-1),
        None => -1,
    }
}

/// Serve `len` bytes of the database file at `offset` into `buf`, faulting pages on demand. Returns the
/// number of bytes read (0 at/past EOF, fewer than `len` on an end-of-file short read), or -1 on error
/// (message on stderr). A negative `offset`, a null handle, or a null `buf` with `len > 0` is an error.
///
/// # Safety
/// `handle` must be a live handle from [`flock_vfs_open`]. If `len > 0`, `buf` must point to at least
/// `len` writable bytes; this function writes only the returned number of bytes and reads none.
#[no_mangle]
pub unsafe extern "C" fn flock_vfs_pread(
    handle: *const FlockVfs,
    offset: i64,
    buf: *mut u8,
    len: usize,
) -> isize {
    // SAFETY: caller guarantees `handle` is null or a live open handle.
    let Some(h) = (unsafe { handle.as_ref() }) else {
        eprintln!("flock_vfs_pread: null handle");
        return -1;
    };
    // A real `pread` never has a negative offset. One here is a malformed request; refuse it rather
    // than sign-cast it into a gigantic unsigned offset.
    if offset < 0 {
        eprintln!("flock_vfs_pread: negative offset {offset}");
        return -1;
    }
    // A zero-length read touches no memory, and `buf` may legitimately be null for it.
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        eprintln!("flock_vfs_pread: null buffer with len {len}");
        return -1;
    }
    // SAFETY: the caller's `pread` contract guarantees `buf` is valid for `len` writable bytes (checked
    // non-null above). `serve_read` writes only `<= len` of them and never reads from `buf`. This slice
    // is the crate's entire unsafe surface, and it hands `serve_read` a safe `&mut [u8]`.
    let out = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    match serve_read(&h.source, h.source.total_len(), offset as u64, out) {
        Ok(n) => n as isize,
        Err(e) => {
            eprintln!("flock_vfs_pread: {e}");
            -1
        }
    }
}

/// Free a handle returned by [`flock_vfs_open`]. A null pointer is ignored.
///
/// # Safety
/// `handle` must be null, or a pointer from [`flock_vfs_open`] that has not already been closed. After
/// this call the pointer is dangling and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn flock_vfs_close(handle: *mut FlockVfs) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller guarantees `handle` came from `flock_vfs_open` and is closed at most once.
    drop(unsafe { Box::from_raw(handle) });
}
