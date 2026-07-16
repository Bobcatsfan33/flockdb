//! Object-storage tier selection for the read path — the one place the backend is chosen.
//!
//! The read path ([`serve_read`](crate::read::serve_read)) is fuzzed and backend-agnostic: it turns a
//! byte range into page faults against a [`PageSource`](crate::source::PageSource), and it does not
//! care whether a fault lands on local disk or on S3. This module is the small, boring code that picks
//! which object store the tier faults *from*, and it is deliberately the crate's only knowledge of that
//! choice.
//!
//! - **Default (and every airgap build): a `LocalFileSystem` tier** rooted at `remote_dir`. This is the
//!   zero-network floor the fuzzed read path was measured against (`docs/wake-latency.md`), and it is
//!   what a plain `LOAD`/`ATTACH` of a `flock://` path uses.
//! - **With the `s3` feature *and* `FLOCK_VFS_S3_URL` set: an S3-compatible tier.** This is the backend
//!   swap RISK-1's object-storage measurement needs — the same `serve_read` boundary, now faulting each
//!   page across a real network — so the wake→query number can be clocked against MinIO on a CI runner
//!   or a real bucket. It is a choice made *below* the fuzzed arithmetic, never a change to it.
//!
//! **Airgap discipline (CLAUDE.md rule 5 / substrate rule 5).** The S3 client is compiled in **only**
//! under `--features s3`. The default build and `cargo test --workspace --features airgap` contain no
//! network client of their own here — the only networking on the whole path stays inside
//! substrate-store's object-storage client, which is where the airgap amputation is enforced.

use crate::error::{Result, VfsError};
use std::sync::Arc;
use substrate_store::RemoteTier;

/// Environment variable naming the S3 endpoint. When set (and the `s3` feature is on), the tier faults
/// from that S3-compatible object store instead of local disk. Its companions are read only if it is
/// present, so an unset `FLOCK_VFS_S3_URL` always yields the local-disk floor.
pub const S3_URL_ENV: &str = "FLOCK_VFS_S3_URL";

/// Build the object-storage tier for a wake, choosing the backend by build feature and environment.
///
/// `remote_dir` is the local tier root (used by the default `LocalFileSystem` backend); `pool` is the
/// [`WakeToken`](substrate_store::WakeToken) pool the database lives in, and is the key prefix under
/// which its pages and manifests are stored, whichever backend is chosen. Seeding tools and the FFI
/// wake path both call this so that what a seed *writes* is what a wake *reads* — a mismatch here would
/// wake a database into an empty bucket.
///
/// See the module docs for the selection rule and the airgap guarantee.
pub fn remote_tier(remote_dir: &str, pool: &str) -> Result<RemoteTier> {
    #[cfg(feature = "s3")]
    {
        if std::env::var(S3_URL_ENV).is_ok() {
            return s3_tier(pool);
        }
    }
    local_tier(remote_dir, pool)
}

/// The default backend: a `LocalFileSystem` object store rooted at `remote_dir`. Network-free, and the
/// only backend compiled into an airgap build.
fn local_tier(remote_dir: &str, pool: &str) -> Result<RemoteTier> {
    let backend =
        object_store::local::LocalFileSystem::new_with_prefix(remote_dir).map_err(|e| {
            VfsError::TierConfig {
                detail: format!(
                    "could not open '{remote_dir}' as a local object-store tier: {e}. Create the \
                 directory, or set {S3_URL_ENV} to fault from an S3-compatible store instead"
                ),
            }
        })?;
    Ok(RemoteTier::new(Arc::new(backend), pool.to_string()))
}

/// The S3-compatible backend, configured from the environment. Compiled in only under `--features s3`.
///
/// The variables mirror the ones the wake-latency workflow and `tests/s3_measure.rs` use, so one set of
/// env configures seeding and waking alike:
/// `FLOCK_VFS_S3_URL` (endpoint), `FLOCK_VFS_S3_BUCKET`, `FLOCK_VFS_S3_KEY_ID`, `FLOCK_VFS_S3_SECRET`,
/// `FLOCK_VFS_S3_REGION`. HTTP is allowed so a plain-HTTP MinIO on a runner works.
#[cfg(feature = "s3")]
fn s3_tier(pool: &str) -> Result<RemoteTier> {
    use object_store::aws::AmazonS3Builder;

    let env = |key: &str| std::env::var(key).ok();
    let url = env(S3_URL_ENV).ok_or_else(|| VfsError::TierConfig {
        detail: format!("{S3_URL_ENV} is not set; it is required to select the S3 tier"),
    })?;

    let mut builder = AmazonS3Builder::new()
        .with_endpoint(url)
        .with_allow_http(true)
        .with_bucket_name(env("FLOCK_VFS_S3_BUCKET").unwrap_or_else(|| "flockdb".to_string()))
        .with_region(env("FLOCK_VFS_S3_REGION").unwrap_or_else(|| "us-east-1".to_string()));
    if let Some(id) = env("FLOCK_VFS_S3_KEY_ID") {
        builder = builder.with_access_key_id(id);
    }
    if let Some(secret) = env("FLOCK_VFS_S3_SECRET") {
        builder = builder.with_secret_access_key(secret);
    }

    let backend = builder.build().map_err(|e| VfsError::TierConfig {
        detail: format!(
            "could not build the S3 tier from {S3_URL_ENV} and its companions: {e}. Check the \
             endpoint URL, bucket, and credentials"
        ),
    })?;
    Ok(RemoteTier::new(Arc::new(backend), pool.to_string()))
}
