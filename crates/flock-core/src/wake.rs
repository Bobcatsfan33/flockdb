//! Sleep and wake — and an honest account of how much of it F1 actually does.
//!
//! # What sleeping is supposed to be
//!
//! docs/02 §1.1 makes two claims the product cannot exist without:
//!
//! > **Databases are cheap to have.** An idle one costs no compute.
//! > **Databases are fast to wake.** A query to a sleeping database returns in **under 250 ms**,
//! > cold, **from object storage**.
//!
//! A sleeping database is meant to be *bytes in S3 and a manifest in a registry*, and nothing else
//! — no container, no connection pool, no monthly bill. That is the economic argument, and it is
//! the entire reason to prefer FlockDB to forty thousand DuckDB containers.
//!
//! # What F1 actually delivers, precisely
//!
//! **[`Db::sleep`](crate::Db::sleep) releases the local compute, and it does not touch object
//! storage.**
//!
//! What it genuinely does:
//! - checkpoints, so the database's state is durable and has a `ManifestId`;
//! - drops the DuckDB connection, its threads, its buffer pool, and its scratch file;
//! - leaves behind a [`WakeToken`] — a pool root, a name, and 32 bytes.
//!
//! What it genuinely does **not** do:
//! - **it does not upload anything.** The pages stay in the local CAS. A "sleeping" F1 database
//!   still occupies its bytes on the local disk.
//!
//! So the *compute* half of the claim in §1.1 is real in F1 and the *storage* half is not. A
//! laptop can hold ten thousand F1 databases asleep and pay no CPU for the idle ones — which is
//! genuinely most of the value — but "an idle database costs the price of its bytes in object
//! storage" is not yet true, because there is no object storage in the picture.
//!
//! # Why not, exactly
//!
//! Because the thing that would do it is not there to call. `docs/substrate-api.md` advertises a
//! `TieredStore` and a `WakeToken` in `substrate-store`, but the crate as published at tag
//! `substrate-v1.1` exports `RemoteTier`, `TieredCas`, and `TierStats` — and no `WakeToken`, and
//! no `PageStore` implementation that tiers to S3. The API document is ahead of the code.
//!
//! We could have shipped a `sleep()` that returns `Err(Unimplemented)`, or one that pretends. We
//! did neither: F1 implements the half that substrate v1.1 can actually support, and this comment
//! is here so that nobody reads `Db::sleep` and believes the S3 half is done.
//!
//! The remaining half is **F3** work, and when substrate exports a real `WakeToken`, [`WakeToken`]
//! here becomes a wrapper around it rather than a replacement for it. **The p99-wake-under-250 ms
//! target in docs/02 §7 is therefore unmeasured in F1**, because the thing it measures does not
//! exist yet. It is not "measured and passing". It is not measured.

use std::path::{Path, PathBuf};
use substrate_pager::ManifestId;

/// A sleeping database: everything needed to bring it back, and nothing else.
///
/// It is deliberately tiny and deliberately *inert* — no file handles, no connection, no threads.
/// A fleet registry is expected to hold millions of these (docs/02 §3.2), so anything that made
/// one expensive would make the fleet plane impossible.
///
/// # It is not a serialization format yet
///
/// substrate's `WakeToken` is promised as a stable, serializable ~20 bytes that "a token written
/// by 1.0 must wake in 1.9". This one is **not that**. It has accessors and a `Display` for logs,
/// and it does not have a wire format, because inventing one now would mean inventing a *second*
/// one later when substrate ships the real thing — and a registry holding millions of rows in a
/// format we intend to abandon is not a thing to do casually. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeToken {
    pool_root: PathBuf,
    db_name: String,
    manifest: ManifestId,
}

impl WakeToken {
    pub(crate) fn new(pool_root: PathBuf, db_name: String, manifest: ManifestId) -> Self {
        WakeToken {
            pool_root,
            db_name,
            manifest,
        }
    }

    /// The pool this database sleeps in.
    ///
    /// A pool is a boundary, not a namespace: a token cannot be woken in a different pool, because
    /// pools never share pages (docs/02 §9.1) and the token's manifest simply is not there.
    pub fn pool_root(&self) -> &Path {
        &self.pool_root
    }

    /// The database's name within its pool.
    pub fn db_name(&self) -> &str {
        &self.db_name
    }

    /// The exact state the database was in when it went to sleep.
    ///
    /// This is the whole of a sleeping database. Content-addressed, so it can never be stale and
    /// can never be ambiguous: a different content would be a different id.
    pub fn manifest(&self) -> ManifestId {
        self.manifest
    }
}

impl std::fmt::Display for WakeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}@{} ({})",
            self.db_name,
            self.manifest,
            self.pool_root.display()
        )
    }
}
