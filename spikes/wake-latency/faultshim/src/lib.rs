//! # faultshim — a page-faulting read path for DuckDB, backed by substrate's `TieredStore`
//!
//! This is the *measurement vehicle* for RISK-1 (`docs/wake-latency.md`). The F4 proof
//! (`proofs/lazy-wake/`) measured the *fault set*: how many bytes of the file DuckDB reads to wake
//! and to answer a selective query (flat ~268–780 KiB regardless of database size). This turns that
//! fault set into an end-to-end **wall-clock latency**, by actually serving DuckDB's file reads from
//! substrate pages on demand and timing wake→first-row.
//!
//! ## How it intercepts DuckDB
//!
//! `faultglue.c` (compiled into this same dylib) interposes `pread`/`read`/`open`/… via the dyld
//! `__interpose` mechanism — the identical interception point the F4 trace used. When DuckDB opens
//! the sparse database file and reads a byte range, [`flock_serve`] translates that range into the
//! substrate logical pages that cover it (logical page `i` = file bytes `[i*ps, (i+1)*ps)`, exactly
//! the layout `flock-kernel/src/paging.rs` defines), fetches each page from the [`TieredStore`]
//! — faulting it from the object-storage tier on a local-cache miss — and copies the requested slice
//! back. No hydration; DuckDB pulls only what it touches.
//!
//! ## Why this is not FUSE, and what that means for the number
//!
//! macFUSE is a privileged kernel extension and was not installable in the spike environment. This
//! shim sits at the *same boundary* a FUSE `read` handler would (a `pread` on the database file
//! becomes a substrate fault) but **in-process, with no kernel round-trip**. So its latency is a
//! *floor below* what FUSE would deliver — FUSE adds one user↔kernel↔user round-trip per fault — and
//! it is a faithful proxy for an in-process **C++ DuckDB FileSystem extension**, which is also
//! in-process. The spike README works this through.
//!
//! ## Boot vs wake (so the timed region is honest)
//!
//! [`flock_fault_boot`] pays the one-time process costs a production runtime would already have paid
//! (build the tokio runtime, open the object-store handle, create the sparse file) and is **not**
//! timed. [`flock_fault_wake`] does the actual wake — fetch the manifest, and in eager mode prefetch
//! every page (the O(database) control) — and IS timed, together with the DuckDB open and the query.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

use object_store::local::LocalFileSystem;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use substrate_pager::{ManifestId, PageStore};
use substrate_store::{RemoteTier, TieredStore, WakeToken};

/// Everything the shim needs, established at boot and (for `store`) at wake.
struct Shim {
    /// Kept alive for the whole process: dropping it would shut the runtime the `TieredCas` fetches
    /// on down under our feet.
    rt: tokio::runtime::Runtime,
    remote: RemoteTier,
    token: WakeToken,
    cache_dir: PathBuf,
    total_len: u64,
    eager: bool,
    /// Set by [`flock_fault_wake`]. `None` until then.
    store: RwLock<Option<Arc<TieredStore>>>,
    /// How many `pread`/`read` calls we served, for sanity against the fault count.
    serve_calls: AtomicU64,
}

static SHIM: OnceLock<Shim> = OnceLock::new();

fn env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("faultshim: env var {key} is not set"))
}

/// Read a required env var and parse it, with an actionable message on failure.
fn env_parse<T: std::str::FromStr>(key: &str) -> Result<T, String> {
    let raw = env(key)?;
    raw.parse::<T>()
        .map_err(|_| format!("faultshim: env var {key}={raw:?} did not parse"))
}

extern "C" {
    /// Defined in `faultglue.c`. Referenced here only so the linker keeps that object — and with it
    /// the `__DATA,__interpose` section that is the entire point of this dylib.
    fn flock_faultglue_anchor() -> i32;
}

fn boot_inner() -> Result<(), String> {
    // Force the interpose glue to be linked in. Without this the read path is never intercepted.
    // SAFETY: `flock_faultglue_anchor` is a pure C function returning a constant, no preconditions.
    if unsafe { flock_faultglue_anchor() } != 1 {
        return Err("faultshim: interpose glue anchor returned the wrong value".to_string());
    }
    let remote_dir: PathBuf = env("FLOCK_REMOTE_DIR")?.into();
    let cache_dir: PathBuf = env("FLOCK_CACHE_DIR")?.into();
    let pool = env("FLOCK_POOL")?;
    let manifest_hex = env("FLOCK_MANIFEST")?;
    let page_size: usize = env_parse("FLOCK_PAGE_SIZE")?;
    let total_len: u64 = env_parse("FLOCK_TOTAL_LEN")?;
    let db_path = env("FLOCK_DB_PATH")?;
    let eager = env("FLOCK_FAULT_MODE")
        .map(|m| m == "eager")
        .unwrap_or(false);

    let manifest = ManifestId::from_hex(&manifest_hex)
        .map_err(|e| format!("faultshim: FLOCK_MANIFEST is not a valid manifest id: {e}"))?;

    // A fresh, empty cache dir — the wake must pull from the tier, not from a warm local disk.
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("faultshim: could not create cache dir {cache_dir:?}: {e}"))?;

    let backend = LocalFileSystem::new_with_prefix(&remote_dir).map_err(|e| {
        format!("faultshim: could not open the local object-store tier at {remote_dir:?}: {e}")
    })?;
    let remote = RemoteTier::new(Arc::new(backend), pool.clone());

    // The sparse backing file: DuckDB opens it and `fstat`s its length; every byte it then reads is
    // served by us out of substrate. `set_len` makes it a hole — no real disk is used.
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&db_path)
        .map_err(|e| format!("faultshim: could not create sparse db file {db_path:?}: {e}"))?;
    f.set_len(total_len)
        .map_err(|e| format!("faultshim: could not size sparse db file to {total_len}: {e}"))?;
    drop(f);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("faultshim: could not build tokio runtime: {e}"))?;

    let shim = Shim {
        rt,
        remote,
        token: WakeToken {
            pool,
            manifest,
            page_size,
        },
        cache_dir,
        total_len,
        eager,
        store: RwLock::new(None),
        serve_calls: AtomicU64::new(0),
    };
    SHIM.set(shim)
        .map_err(|_| "faultshim: boot called twice".to_string())
}

fn wake_inner() -> Result<(), String> {
    let shim = SHIM.get().ok_or("faultshim: wake before boot")?;

    // The wake itself: fetch the manifest eagerly (one round trip), pages lazily. This is the whole
    // point — `TieredStore::wake` does NOT move the pages, only the manifest.
    let store = shim
        .rt
        .block_on(TieredStore::wake(
            &shim.cache_dir,
            shim.remote.clone(),
            &shim.token,
        ))
        .map_err(|e| format!("faultshim: TieredStore::wake failed: {e}"))?;
    let store = Arc::new(store);

    if shim.eager {
        // The O(database) control: prefetch every page, exactly as `paging::hydrate` does today.
        // After this, DuckDB's reads hit a warm local cache — but the wake paid for the whole file.
        let head = store.pager().head();
        let pages = store
            .pager()
            .resolve(&head)
            .map_err(|e| format!("faultshim: resolve failed during eager prefetch: {e}"))?;
        for &page_no in pages.keys() {
            store
                .pager()
                .read(&head, page_no)
                .map_err(|e| format!("faultshim: eager prefetch of page {page_no} failed: {e}"))?;
        }
    }

    let mut guard = store_write_guard(shim)?;
    *guard = Some(store);
    Ok(())
}

fn store_write_guard(
    shim: &Shim,
) -> Result<std::sync::RwLockWriteGuard<'_, Option<Arc<TieredStore>>>, String> {
    shim.store
        .write()
        .map_err(|_| "faultshim: store lock poisoned".to_string())
}

fn serve_inner(off: i64, buf: *mut c_void, n: usize) -> Result<usize, String> {
    let shim = SHIM.get().ok_or("faultshim: serve before boot")?;
    shim.serve_calls.fetch_add(1, Ordering::Relaxed);

    let guard = shim
        .store
        .read()
        .map_err(|_| "faultshim: store lock poisoned".to_string())?;
    let store = guard
        .as_ref()
        .ok_or("faultshim: serve before wake")?
        .clone();
    drop(guard);

    let off = if off < 0 { 0u64 } else { off as u64 };
    if off >= shim.total_len {
        return Ok(0); // read starts at or past EOF
    }
    let want = std::cmp::min(n as u64, shim.total_len - off) as usize;
    if want == 0 {
        return Ok(0);
    }

    let pager = store.pager();
    let head = pager.head();
    let ps = pager.page_size() as u64;

    // SAFETY: `buf` is the destination libc's `pread`/`read` contract guarantees is valid for `n`
    // bytes; we write only `want <= n` of them and never read from it.
    let out = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, want) };

    let mut done = 0usize;
    while (done as u64) < want as u64 {
        let file_pos = off + done as u64;
        let page_no = file_pos / ps;
        let page_off = (file_pos % ps) as usize;

        // The fault: on a local-cache miss this fetches the page from the object-storage tier and
        // verifies its hash. On a hit it is a local read. Either way, only pages the query's reads
        // actually cover are ever touched.
        let page = pager
            .read(&head, page_no)
            .map_err(|e| format!("faultshim: fault of page {page_no} failed: {e}"))?;
        let pbytes = page.as_bytes();
        if page_off >= pbytes.len() {
            // Only reachable if total_len disagreed with the pages; refuse rather than read garbage.
            return Err(format!(
                "faultshim: offset {file_pos} lands past the end of page {page_no} \
                 (page is {} bytes) — total_len is wrong",
                pbytes.len()
            ));
        }
        let take = std::cmp::min(pbytes.len() - page_off, want - done);
        out[done..done + take].copy_from_slice(&pbytes[page_off..page_off + take]);
        done += take;
    }
    Ok(done)
}

/// Boot: pay the one-time process costs (runtime, tier handle, sparse file). NOT timed. Returns 0
/// on success, -1 on failure (message on stderr).
///
/// # Safety
/// Exported for the C harness to call via `dlsym`. No arguments; safe to call once.
#[no_mangle]
pub extern "C" fn flock_fault_boot() -> i32 {
    match boot_inner() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            -1
        }
    }
}

/// Wake: fetch the manifest (and, in eager mode, prefetch every page). This IS timed by the harness.
///
/// # Safety
/// Exported for the C harness to call via `dlsym`, after [`flock_fault_boot`].
#[no_mangle]
pub extern "C" fn flock_fault_wake() -> i32 {
    match wake_inner() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            -1
        }
    }
}

/// Serve `n` bytes of the database file at `off` from substrate pages. Returns bytes filled, or -1.
///
/// # Safety
/// `buf` must be valid for writes of `n` bytes. Called by `faultglue.c` on an interposed
/// `pread`/`read` of the tracked database fd.
#[no_mangle]
pub unsafe extern "C" fn flock_serve(off: i64, buf: *mut c_void, n: usize) -> isize {
    match serve_inner(off, buf, n) {
        Ok(filled) => filled as isize,
        Err(e) => {
            eprintln!("{e}");
            -1
        }
    }
}

/// Distinct pages faulted from the object-storage tier (i.e. tier cache misses). This is the number
/// that ties back to the F4 fault-set proof.
///
/// # Safety
/// Exported for the C harness via `dlsym`.
#[no_mangle]
pub extern "C" fn flock_fault_pages_faulted() -> i64 {
    match SHIM.get().and_then(|s| {
        s.store
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|st| st.stats().misses))
    }) {
        Some(m) => m as i64,
        None => -1,
    }
}

/// How many `pread`/`read` calls the shim served — a sanity check on the fault count.
///
/// # Safety
/// Exported for the C harness via `dlsym`.
#[no_mangle]
pub extern "C" fn flock_fault_serve_calls() -> i64 {
    match SHIM.get() {
        Some(s) => s.serve_calls.load(Ordering::Relaxed) as i64,
        None => -1,
    }
}
