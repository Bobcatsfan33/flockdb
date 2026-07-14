//! Sleep and wake: does the database actually get to object storage, and does it come back?
//!
//! # The wipe is the whole test
//!
//! Every test here **deletes the entire pool directory** between `sleep()` and `wake()` — not evicts
//! a cache, not clears a temp file: `remove_dir_all` on the pool. If a single byte the woken database
//! needs were still on local disk, these tests would pass for the wrong reason and we would ship a
//! `sleep()` that quietly depended on the machine it slept on. With the wipe, there is exactly one
//! place the data can be coming from.
//!
//! # What backend these run against, and why that is not a dodge
//!
//! `object_store::memory::InMemory` and `object_store::local::LocalFileSystem` — the same
//! `ObjectStore` trait the real S3 client implements, and the same one substrate's own suite uses.
//! They exercise every line of FlockDB's and substrate's tiering path: the upload, the manifest
//! closure, `drop_local`, the lazy fetch on wake.
//!
//! What they do **not** exercise is the network. So they prove *correctness* and they do not prove
//! *latency*: the < 250 ms wake target in docs/02 §7 is **not measured** by these tests and is not
//! measured anywhere else in this repository either. See the README. A number from these tests would
//! be a floor with the interesting part removed, and quoting it as if it were the target would be
//! precisely the sort of unreproducible claim docs/04 §5 exists to forbid.

mod common;

use common::{column_i64, scalar_i64};
use flock_core::{object_store::memory::InMemory, Db, Flock, RemoteTier};
use std::path::Path;
use std::sync::Arc;

/// A pool with a database in it holding `1..=n`.
fn seeded(root: &Path, n: i64) -> Db {
    let mut db = Flock::open(root, "sales").expect("open");
    db.execute("CREATE TABLE t (id INTEGER)").expect("create");
    for i in 1..=n {
        db.execute(&format!("INSERT INTO t VALUES ({i})"))
            .expect("insert");
    }
    db
}

fn tier(pool: &str) -> RemoteTier {
    RemoteTier::new(Arc::new(InMemory::new()), pool)
}

/// **The headline.** Write, sleep, delete every local byte, wake into a different directory, query.
#[tokio::test(flavor = "multi_thread")]
async fn a_database_survives_sleep_a_total_wipe_and_wake() {
    let remote = tier("acme");
    let home = tempfile::tempdir().expect("tempdir");

    let token = {
        let db = seeded(home.path(), 3);
        db.sleep(remote.clone()).await.expect("sleep")
    };

    // The wipe. Not an eviction — the pool is gone.
    std::fs::remove_dir_all(home.path()).expect("wipe the pool");
    assert!(!home.path().exists());

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let mut db = Flock::wake(elsewhere.path(), "sales", remote, &token)
        .await
        .expect("wake");

    assert_eq!(
        column_i64(&db.query("SELECT id FROM t ORDER BY id").expect("query")),
        vec![1, 2, 3],
        "the rows did not come back — which means they were never in object storage, or the wake \
         read them off a disk that is supposed to be empty"
    );
}

/// Sleep must capture writes made since the last snapshot, or it silently truncates the database.
#[tokio::test(flavor = "multi_thread")]
async fn sleep_snapshots_first_so_unsnapshotted_writes_are_not_lost() {
    let remote = tier("acme");
    let home = tempfile::tempdir().expect("tempdir");

    let token = {
        let mut db = seeded(home.path(), 3);
        db.snapshot().expect("snapshot");
        // Written AFTER the last explicit snapshot. These live only in DuckDB's scratch file; if
        // `sleep()` did not checkpoint, substrate would never have heard of them.
        db.execute("INSERT INTO t VALUES (4), (5)").expect("insert");
        db.sleep(remote.clone()).await.expect("sleep")
    };

    std::fs::remove_dir_all(home.path()).expect("wipe");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let mut db = Flock::wake(elsewhere.path(), "sales", remote, &token)
        .await
        .expect("wake");

    assert_eq!(
        column_i64(&db.query("SELECT id FROM t ORDER BY id").expect("query")),
        vec![1, 2, 3, 4, 5],
        "sleep() must snapshot before it uploads, or everything since the last snapshot is gone"
    );
}

/// A snapshot id taken *before* the sleep must still work *after* the wake.
///
/// This is the test that forced `sleep()` to copy manifests by value rather than re-derive them. A
/// manifest id is a hash of its bytes; re-deriving would have produced different ids, every
/// `ManifestId` the caller was holding would have become garbage, and nothing would have failed
/// loudly — `restore` would just have started saying "not in this pool" about the pool it was in.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_taken_before_the_sleep_can_be_restored_after_the_wake() {
    let remote = tier("acme");
    let home = tempfile::tempdir().expect("tempdir");

    let (token, before) = {
        let mut db = seeded(home.path(), 3);
        let before = db.snapshot().expect("snapshot");

        db.execute("INSERT INTO t VALUES (4)").expect("insert");
        let token = db.sleep(remote.clone()).await.expect("sleep");
        (token, before)
    };

    std::fs::remove_dir_all(home.path()).expect("wipe");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let mut db = Flock::wake(elsewhere.path(), "sales", remote, &token)
        .await
        .expect("wake");

    assert_eq!(scalar_i64(&db.query("SELECT count(*) FROM t").unwrap()), 4);

    // The id from before the sleep. Its pages and its manifest have made a round trip through
    // object storage, and it still names exactly the state it named.
    db.restore(before).expect("restore a pre-sleep snapshot");
    assert_eq!(
        column_i64(&db.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![1, 2, 3],
        "a ManifestId taken before the sleep must still mean what it meant"
    );
}

/// Waking into the wrong pool is refused. Pools are a security boundary (docs/02 §9.1).
#[tokio::test(flavor = "multi_thread")]
async fn a_database_cannot_be_woken_into_a_different_pool() {
    let acme = tier("acme");
    let home = tempfile::tempdir().expect("tempdir");

    let token = seeded(home.path(), 2)
        .sleep(acme)
        .await
        .expect("sleep into acme");

    // A *different* tier, naming a different pool. Even with the right token, this must not work:
    // the pool is the classification boundary, and waking across it is exactly the thing the
    // boundary exists to prevent.
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let err = Flock::wake(elsewhere.path(), "sales", tier("other-tenant"), &token)
        .await
        .expect_err("waking into another pool must be refused");

    let msg = err.to_string();
    assert!(
        msg.contains("pool"),
        "the error must say the pool is the problem, not fail with something cryptic: {msg}"
    );
}

/// **`sleep()` must not touch the pool it slept from.**
///
/// The trap documented on `Db::sleep`: a `TieredStore` opened over a CAS it did not write believes
/// nothing is pending, uploads nothing, and then `drop_local()` — seeing an empty `pending` set —
/// deletes every page in it. If `sleep()` ever regresses to pointing a `TieredStore` at the real
/// pool, this test fails, and it fails by finding the pool empty.
#[tokio::test(flavor = "multi_thread")]
async fn sleeping_does_not_delete_the_pool_it_slept_from() {
    let remote = tier("acme");
    let home = tempfile::tempdir().expect("tempdir");

    let db = seeded(home.path(), 3);
    db.sleep(remote).await.expect("sleep");

    // The pool's CAS must still hold its pages, and the database must still open and read.
    let pages = home.path().join("cas").join("pages");
    let count = std::fs::read_dir(&pages)
        .expect("the pool's page directory must still exist after a sleep")
        .count();
    assert!(
        count > 0,
        "sleep() emptied the pool's CAS at {pages:?}. That is the `drop_local` trap, and it means \
         every OTHER database in this pool has just lost its pages too."
    );

    let mut still_there = Flock::open(home.path(), "sales").expect("reopen after sleep");
    assert_eq!(
        column_i64(&still_there.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![1, 2, 3],
        "the database we slept is still on local disk and must still be readable"
    );
}

/// Sleep and wake through a real filesystem-backed object store, not just the in-memory one.
///
/// `LocalFileSystem` goes through `object_store`'s real code path — paths, prefixes, byte streams —
/// so this catches key-construction bugs that `InMemory` would forgive. It is still not the network.
#[tokio::test(flavor = "multi_thread")]
async fn sleep_and_wake_through_a_filesystem_backed_object_store() {
    let bucket = tempfile::tempdir().expect("tempdir");
    let backend = object_store::local::LocalFileSystem::new_with_prefix(bucket.path())
        .expect("open a filesystem-backed object store");
    let remote = RemoteTier::new(Arc::new(backend), "acme");

    let home = tempfile::tempdir().expect("tempdir");
    let token = seeded(home.path(), 4).sleep(remote.clone()).await.unwrap();

    std::fs::remove_dir_all(home.path()).expect("wipe");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let mut db = Flock::wake(elsewhere.path(), "sales", remote, &token)
        .await
        .unwrap();

    assert_eq!(
        column_i64(&db.query("SELECT id FROM t ORDER BY id").unwrap()),
        vec![1, 2, 3, 4]
    );
}

/// **Wake-to-first-query against a real S3 API — the docs/02 §7 target of p99 < 250 ms.**
///
/// # This test has never been run, and that is the point of it existing
///
/// It needs an S3-compatible endpoint. In the environment this was written in there was none: the
/// Docker daemon would not start, and fetching the MinIO binary was refused. So the number is **NOT
/// MEASURED** — not "measured and passing", not "measured and close". Not measured.
///
/// The alternative to this test was a paragraph in the README promising to measure it later, which
/// is a promise with no mechanism. This is a mechanism: one `docker run` and one command, and the
/// number exists.
///
/// ```sh
/// docker run -d -p 9000:9000 -e MINIO_ROOT_USER=minioadmin \
///     -e MINIO_ROOT_PASSWORD=minioadmin minio/minio server /data
/// # create a bucket named `flockdb`, then:
/// MINIO_URL=http://localhost:9000 cargo test -p flock-core --test tiering -- --ignored --nocapture
/// ```
///
/// # Read this before you believe the number it prints
///
/// Even against real S3, this measures a **tiny** database. FlockDB's wake is **O(database)** in
/// network transfer, not O(pages the query touches), because DuckDB needs a whole file before it
/// will answer anything (see [`Flock::wake`]). Substrate's wake is lazy and would hit the 250 ms
/// target on a 100 GB database; FlockDB's cannot, and would move 100 GB. A green tick here on a
/// four-row table would say almost nothing about that, and it must not be quoted as though it did.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an S3-compatible endpoint; set MINIO_URL. Never yet run — see the doc comment."]
async fn wake_latency_against_a_real_s3_endpoint() {
    let Ok(url) = std::env::var("MINIO_URL") else {
        panic!("MINIO_URL is not set. This test cannot be run without an S3-compatible endpoint.");
    };

    let backend = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(url)
        .with_bucket_name(std::env::var("MINIO_BUCKET").unwrap_or_else(|_| "flockdb".into()))
        .with_access_key_id(std::env::var("MINIO_USER").unwrap_or_else(|_| "minioadmin".into()))
        .with_secret_access_key(
            std::env::var("MINIO_PASSWORD").unwrap_or_else(|_| "minioadmin".into()),
        )
        .with_allow_http(true)
        .build()
        .expect("build an S3 client");

    let remote = RemoteTier::new(Arc::new(backend), "acme");
    let home = tempfile::tempdir().expect("tempdir");
    let token = seeded(home.path(), 4).sleep(remote.clone()).await.unwrap();

    std::fs::remove_dir_all(home.path()).expect("wipe");
    let elsewhere = tempfile::tempdir().expect("tempdir");

    // Wake-to-first-QUERY, not wake-to-first-page-read. The kernel has to reconstruct the database
    // file and start DuckDB before a single row can come back, and the user waits for all of it.
    let started = std::time::Instant::now();
    let mut db = Flock::wake(elsewhere.path(), "sales", remote, &token)
        .await
        .expect("wake");
    let rows = column_i64(&db.query("SELECT id FROM t ORDER BY id").unwrap());
    let latency = started.elapsed();

    assert_eq!(rows, vec![1, 2, 3, 4]);
    println!(
        "wake-to-first-query against a real S3 API: {latency:?}  (docs/02 §7 target: < 250 ms)"
    );
    assert!(
        latency.as_millis() < 250,
        "wake took {latency:?}, over the 250 ms budget in docs/02 §7"
    );
}
