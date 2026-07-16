//! End-to-end proof of the scheduler's whole point: a database's SECOND cold wake pays one coalesced
//! prewarm for the fault set its FIRST wake had to discover — and the query then serves with **zero**
//! new tier faults.
//!
//! The shape mirrors flock-vfs's `roundtrip_tiered.rs` (a real local `TieredStore`, woken cold), but
//! drives the flockd scheduler:
//!
//! 1. Seed a database into a tier and sleep it.
//! 2. **First wake (cold):** wrap the woken source in a [`RecordingSource`], serve a point-like query
//!    (a couple of sub-ranges — a handful of pages, not the whole file), and `observe` the recorded
//!    fault set into the scheduler. Count the tier misses this wake paid.
//! 3. **Second wake (cold again, fresh cache):** `prewarm` the learned set, then serve the SAME query.
//!    Assert the query added zero tier misses — every page it needs was coalesced in by the prewarm.
//!
//! What this proves and what it does not: it proves the scheduler warms exactly the set a query needs,
//! so the query's serial reads become cache hits. It does NOT assert a latency — on a local tier a
//! fault is ~free, so wall-clock says nothing here; the coalescing payoff is a wide-area number,
//! measured separately and never quoted until it exists.
//!
//! Reads run on a non-runtime thread (substrate's pager bridges to the async tier with an internal
//! `block_on`, which cannot nest inside a runtime worker) — same as `roundtrip_tiered.rs`.

use flock_vfs::{serve_read, PageSource, TieredPageSource};
use flockd::{DbId, RecordingSource, WakeScheduler};
use object_store::local::LocalFileSystem;
use std::sync::Arc;
use substrate_pager::{PageStore, StoreConfig};
use substrate_store::{RemoteTier, TieredStore, WakeToken};

async fn seed_and_sleep(
    remote_dir: &std::path::Path,
    seed_cache: &std::path::Path,
    pool: &str,
    page_size: usize,
    data: &[u8],
) -> (WakeToken, u64) {
    let backend = LocalFileSystem::new_with_prefix(remote_dir).unwrap();
    let remote = RemoteTier::new(Arc::new(backend), pool.to_string());
    let config = StoreConfig {
        page_size,
        pool: pool.to_string(),
        ..Default::default()
    };
    let store = TieredStore::open(seed_cache, remote, config).await.unwrap();
    let pager = store.pager();
    let mut txn = pager.begin().unwrap();
    for (i, chunk) in data.chunks(page_size).enumerate() {
        pager.write(&mut txn, i as u64, chunk.to_vec()).unwrap();
    }
    pager.commit(txn).unwrap();
    let token = store.sleep().await.unwrap();
    (token, data.len() as u64)
}

/// Wake the sleeping database into a fresh, COLD cache directory. Each call is an independent cold wake.
fn wake_cold(
    rt: &tokio::runtime::Runtime,
    remote_dir: &std::path::Path,
    cache_dir: &std::path::Path,
    pool: &str,
    token: &WakeToken,
    total_len: u64,
) -> TieredPageSource {
    let backend = LocalFileSystem::new_with_prefix(remote_dir).unwrap();
    let remote = RemoteTier::new(Arc::new(backend), pool.to_string());
    let store = rt
        .block_on(TieredStore::wake(cache_dir, remote, token))
        .unwrap();
    TieredPageSource::new(Arc::new(store), total_len)
}

/// The "query": read two small sub-ranges — a point-query-like access touching only a few pages, not a
/// scan. Returns nothing; its effect is the faults it drives.
fn run_query<S: PageSource>(source: &S, total_len: u64) {
    for &(off, len) in &[(20_000u64, 64usize), (36_000, 40)] {
        let mut buf = vec![0u8; len];
        serve_read(source, total_len, off, &mut buf).unwrap();
    }
}

#[test]
fn second_wake_prewarms_the_learned_set_and_serves_with_zero_new_faults() {
    let page_size = 4096usize;
    let data: Vec<u8> = (0..60_000u32).map(|i| (i * 11 + 5) as u8).collect(); // ~15 pages
    let total_len = data.len() as u64;
    let pool = "flockd-wakewarm";

    let tmp = tempfile::tempdir().unwrap();
    let remote_dir = tmp.path().join("remote");
    let seed_cache = tmp.path().join("seed-cache");
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(&seed_cache).unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (token, _) = rt.block_on(seed_and_sleep(
        &remote_dir,
        &seed_cache,
        pool,
        page_size,
        &data,
    ));

    let sched = WakeScheduler::new();
    let db = DbId::new("acme/ledger");

    // ---- First wake (cold): nothing learned yet, so prewarm is a no-op; the query faults and teaches.
    let cache1 = tmp.path().join("wake-1");
    std::fs::create_dir_all(&cache1).unwrap();
    let source1 = wake_cold(&rt, &remote_dir, &cache1, pool, &token, total_len);

    assert_eq!(
        sched.prewarm(&db, &source1),
        0,
        "first wake of an unknown database has nothing to prewarm"
    );

    let rec = RecordingSource::new(source1);
    run_query(&rec, total_len);
    let learned = rec.touched();
    sched.observe(&db, &learned);
    let first_wake_misses = rec.into_inner().store().stats().misses;
    assert!(
        !learned.is_empty() && first_wake_misses >= 1,
        "the first wake should have faulted the query's pages from the cold tier \
         (learned {} pages, {} misses)",
        learned.len(),
        first_wake_misses
    );
    // The query is selective: it touched only a few pages, not the whole ~15-page file.
    assert!(
        learned.len() <= 6,
        "a point-like query should fault only a few pages, learned {}",
        learned.len()
    );

    // ---- Second wake (cold again, fresh cache): prewarm the learned set, then serve the same query.
    let cache2 = tmp.path().join("wake-2");
    std::fs::create_dir_all(&cache2).unwrap();
    let source2 = wake_cold(&rt, &remote_dir, &cache2, pool, &token, total_len);

    let warmed = sched.prewarm(&db, &source2);
    assert_eq!(
        warmed,
        learned.len(),
        "prewarm warms exactly the learned set"
    );
    let after_prewarm = source2.store().stats().misses;

    run_query(&source2, total_len);
    let after_query = source2.store().stats().misses;
    assert_eq!(
        after_query, after_prewarm,
        "the query re-faulted the tier ({} new misses) after prewarm — the learned warm set did not \
         cover the query's reads",
        after_query - after_prewarm
    );

    drop(rt);
}
