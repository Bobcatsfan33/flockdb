//! `flock sleep` and `flock wake` — the tier, and the honesty about which tier.
//!
//! # `--tier` is a directory, and that is not a stand-in for S3
//!
//! `object_store`'s `LocalFileSystem` is the *same trait* the S3 client implements, so this path
//! exercises every line of substrate's tiering code: the upload, the manifest closure, `drop_local`,
//! the fetch on wake. What it does not exercise is **the network** — and the network is where the
//! latency, the retries, the credentials and the 503s live.
//!
//! So the CLI ships the filesystem backend and refuses `s3://` **by name**, rather than wiring an
//! `AmazonS3Builder` that no test has ever run against a real endpoint. A storage path that has
//! never been executed is not a feature; it is a rumour with a code path. The engine
//! (`flock_core::RemoteTier`) will take any `ObjectStore` you hand it today.
//!
//! # What `sleep` does not do
//!
//! It does not delete a single page. Pages live in the pool's shared content-addressed store, which
//! every branch in the pool reads from; `sleep` removes the branch's own directory — its private
//! write-ahead log, which is its *head* — and puts a durable copy of everything in the tier.
//! Sleeping frees **compute**, not disk. Reclaiming disk means garbage-collecting pages no live
//! manifest references, which is a fleet-level policy decision (F4) and not something a `sleep`
//! should do behind the caller's back while they are not looking.
//!
//! The consequence, said plainly: after `flock sleep`, the pages are in **two** places. The test
//! that proves `wake` really reads the tier is the one that deletes the pool's entire CAS in
//! between — see `tests/tier.rs`.

use crate::error::{CliError, Result};
use crate::workspace::{SleepRecord, Workspace};
use flock_core::object_store::local::LocalFileSystem;
use flock_core::{Flock, RemoteTier, WakeToken};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn sleep(ws: &Workspace, branch: Option<&str>, tier: Option<PathBuf>) -> Result<()> {
    let branch = ws.resolve(branch)?;

    let tier = tier.unwrap_or_else(|| ws.root().join("cold"));
    let tier = prepare_tier_dir(&tier)?;
    let pool = pool_name(ws);
    let remote = remote_tier(&tier, &pool)?;

    let db = Flock::open(ws.root(), &branch)?;

    // Multi-threaded, and this is not a preference. Substrate's page read path is synchronous (so
    // that crash injection can be deterministic), so a cache miss blocks the calling thread on an
    // async fetch. On a current-thread runtime that thread is the executor's only thread, and the
    // process deadlocks — silently, with no error, forever. `flock-core` says this in its docs and
    // this is the CLI honouring it.
    let rt = runtime()?;
    let token = rt.block_on(db.sleep(remote))?;

    ws.write_sleep_record(
        &branch,
        &SleepRecord {
            tier: tier.clone(),
            pool,
            token: token.to_json().map_err(|source| CliError::Store {
                op: "encode the WakeToken",
                source,
            })?,
        },
    )?;

    // Only now — after the pages are in the tier and the token is on disk — does the branch's head
    // go away. In that order, because the reverse order has a window in which a crash loses the
    // branch. Substrate's own commit protocol is built on exactly this discipline: make it durable
    // elsewhere, verify, and only then throw away the copy you have.
    ws.remove_branch_dir(&branch)?;

    println!("branch \"{branch}\" is asleep in {}", tier.display());
    println!(
        "what is left of it is a 32-byte manifest id in {}",
        ws.sleep_record_path(&branch).display()
    );
    println!("the pool's pages were NOT deleted — sleeping frees compute, not disk");
    println!("bring it back with: flock wake {branch}");
    Ok(())
}

pub fn wake(ws: &Workspace, branch: &str) -> Result<()> {
    if !ws.exists() {
        return Err(CliError::NoPool {
            pool: ws.root().to_path_buf(),
        });
    }
    let known = ws.branches()?;
    if !known.contains(&branch.to_string()) {
        return Err(CliError::UnknownBranch {
            name: branch.to_string(),
            known,
        });
    }
    if !ws.is_asleep(branch) {
        return Err(CliError::BranchAwake {
            name: branch.to_string(),
        });
    }

    let record = ws.read_sleep_record(branch)?;
    let remote = remote_tier(&record.tier, &record.pool)?;
    let token = WakeToken::from_json(&record.token).map_err(|source| CliError::Store {
        op: "decode the WakeToken",
        source,
    })?;

    // The cache starts EMPTY, in a directory of our own. Everything the woken database reads is a
    // miss, and every miss is served from the tier. That is what makes `tests/tier.rs` — which
    // deletes the pool's entire CAS before this runs — a test of the tier and not of the disk.
    let cache = tempfile::tempdir().map_err(CliError::io(
        "create a page cache for the wake",
        std::env::temp_dir(),
        "The wake fetches pages into a temporary directory before adopting them into the pool. \
         Check $TMPDIR.",
    ))?;

    let rt = runtime()?;
    let manifest = {
        // Opening the woken database hydrates DuckDB's file, which reads every page, which faults
        // every page in from the tier. That is F1's honest cost of a wake and it is written up in
        // `flock_core::Flock::wake`: substrate wakes lazily, DuckDB needs a whole file, so FlockDB
        // does not get to be lazy. Here it is also load-bearing — it is what guarantees the cache
        // holds the whole database by the time we adopt it.
        let db = rt.block_on(Flock::wake(cache.path(), branch, remote, &token))?;
        db.head()
    };

    // Adopt the fetched pages into the pool's own content-addressed store. Pages and manifests are
    // immutable and named by the hash of their bytes, so this is a plain file copy into the same
    // shard layout: `pages/aa/bb/<hex>`, `manifests/aa/<hex>`. There is no merge and no conflict —
    // a page that is already there is already correct.
    let cas = ws.cas_dir();
    let pages = copy_tree(&cache.path().join("pages"), &cas.join("pages"))?;
    copy_tree(&cache.path().join("manifests"), &cas.join("manifests"))?;

    // The branch again: a fresh directory, a private WAL, and a head moved to the manifest the token
    // named. `restore` is the library call that does the last part, and it refuses a manifest this
    // pool cannot resolve — which is exactly the check we want, because if the copy above had missed
    // a page, we would rather find out here than three queries later.
    let mut db = Flock::open(ws.root(), branch)?;
    db.restore(manifest)?;

    ws.remove_sleep_record(branch)?;

    println!(
        "branch \"{branch}\" is awake — {pages} pages fetched from {}",
        record.tier.display()
    );
    Ok(())
}

/// A local-filesystem tier. **Not** an S3 client wearing one as a hat.
fn remote_tier(dir: &Path, pool: &str) -> Result<RemoteTier> {
    let backend = LocalFileSystem::new_with_prefix(dir).map_err(|source| CliError::Tier {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(RemoteTier::new(Arc::new(backend), pool))
}

/// Create the tier directory, and refuse anything that is not one.
fn prepare_tier_dir(dir: &Path) -> Result<PathBuf> {
    // `--tier s3://bucket` is the mistake this catches, and it must not be caught by
    // `create_dir_all` making a literal directory called `s3:`.
    let text = dir.display().to_string();
    if text.contains("://") {
        return Err(CliError::TierNotSupported { uri: text });
    }

    std::fs::create_dir_all(dir).map_err(CliError::io(
        "create the tier directory",
        dir,
        "`--tier` is a directory on this machine. Check the path is writable.",
    ))?;

    // Absolute, because the pool can be opened from a different working directory tomorrow and the
    // sleep record has to still point at the right place.
    std::fs::canonicalize(dir).map_err(CliError::io(
        "resolve the tier directory",
        dir,
        "The tier path is stored absolutely, so that waking works from any working directory.",
    ))
}

/// The dedup pool name the tier is keyed by.
///
/// Derived from the pool directory's own name, so that two pools sharing one tier directory cannot
/// see each other's pages — substrate keys every object by the pool and *refuses* to wake a database
/// into a pool that is not the one it slept in. That refusal is the classification boundary from
/// docs/02 §9.1, and it only works if the name is stable, which is why it is also written into the
/// sleep record rather than re-derived at wake time.
fn pool_name(ws: &Workspace) -> String {
    let name: String = std::fs::canonicalize(ws.root())
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();

    if name.is_empty() {
        "flock".to_string()
    } else {
        name
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(CliError::io(
            "start the async runtime",
            ".",
            "Object storage is async and substrate's read path is not, so sleep and wake need a \
             multi-threaded runtime. This failure is the OS refusing to make threads.",
        ))
}

/// Copy a content-addressed tree, preserving its shard layout. Returns the number of files copied.
///
/// Immutable, hash-named files: a file that already exists at the destination has the right bytes in
/// it by construction, so it is skipped rather than rewritten.
fn copy_tree(src: &Path, dst: &Path) -> Result<usize> {
    if !src.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(dst).map_err(CliError::io(
        "create the pool's store",
        dst,
        "Check that the pool directory is writable.",
    ))?;

    let mut copied = 0;
    let entries = std::fs::read_dir(src).map_err(CliError::io(
        "read the page cache",
        src,
        "The wake fetched pages here; they could not be read back.",
    ))?;

    for entry in entries {
        let entry = entry.map_err(CliError::io(
            "read the page cache",
            src,
            "The wake fetched pages here; they could not be read back.",
        ))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());

        if from.is_dir() {
            copied += copy_tree(&from, &to)?;
        } else if !to.exists() {
            std::fs::copy(&from, &to).map_err(CliError::io(
                "adopt a page into the pool",
                &to,
                "Check that the pool directory is writable and has free space.",
            ))?;
            copied += 1;
        }
    }
    Ok(copied)
}
