//! Proof that `TieredPageSource::prefetch` actually warms the cache — the coalesced-fault primitive
//! the wake scheduler schedules against (`docs/wake-latency.md`, the wide-area design input).
//!
//! Coalescing's *only* observable effect is residency: after a prefetch, the pages must be served from
//! the local cache without touching the tier again. This test proves exactly that against the **real**
//! `TieredPageSource` over a real (local) `TieredStore`, using substrate's own tier-miss counter as the
//! instrument — a prefetch that warmed the set means a subsequent read of that set adds **zero** misses.
//!
//! It deliberately does NOT try to prove the *speedup*. On a local tier a fault is nearly free, so
//! wall-clock says nothing; the value of firing the faults concurrently only appears against a
//! wide-area bucket, and that is measured separately (`tests/s3_measure.rs`, and the wide-area workflow)
//! — never quoted as a latency number until it is. Here the contract is correctness: prefetch warms
//! what it is asked to warm, and is safe when asked to warm nothing or a page past the end.
//!
//! Read from a non-runtime thread for the same reason as `roundtrip_tiered.rs`: substrate's pager is
//! synchronous and bridges to the async tier with an internal `block_on`, which cannot nest inside a
//! runtime worker.

use flock_vfs::{serve_read, TieredPageSource};
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

#[test]
fn prefetch_warms_the_set_so_reads_do_not_re_fault() {
    let page_size = 4096usize;
    // Enough bytes for several pages so "warm a subset, read it, no new misses" is a real claim.
    let data: Vec<u8> = (0..40_000u32).map(|i| (i * 13 + 1) as u8).collect();
    let total_len = data.len() as u64;
    let n_pages = data.len().div_ceil(page_size) as u64; // 10 pages, last one short
    let pool = "vfs-prefetch";

    let tmp = tempfile::tempdir().unwrap();
    let remote_dir = tmp.path().join("remote");
    let seed_cache = tmp.path().join("seed-cache");
    let wake_cache = tmp.path().join("wake-cache");
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(&seed_cache).unwrap();
    std::fs::create_dir_all(&wake_cache).unwrap();

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

    // Wake into a COLD cache — every page must be faulted from the tier on first touch.
    let backend = LocalFileSystem::new_with_prefix(&remote_dir).unwrap();
    let remote = RemoteTier::new(Arc::new(backend), pool.to_string());
    let store = rt
        .block_on(TieredStore::wake(&wake_cache, remote, &token))
        .unwrap();
    let source = TieredPageSource::new(Arc::new(store), total_len);

    use flock_vfs::PageSource;
    let all_pages: Vec<u64> = (0..n_pages).collect();

    // Empty and past-the-end prefetches must be safe: no panic, no propagated error. A page past the
    // end is best-effort — it may or may not touch the tier, but it must not blow up the wake.
    source.prefetch(&[]);
    source.prefetch(&[n_pages + 100]);

    // Warm the whole file in one coalesced call, from a non-runtime thread.
    source.prefetch(&all_pages);
    let after_prefetch = source.store().stats().misses;
    // It must have faulted the cold tier — but NOT necessarily once per logical page. This is a
    // content-addressed store: identical pages share one object, so the miss count is the number of
    // distinct *objects*, which is `<= n_pages` and content-dependent. (The old `>= n_pages` assertion
    // was wrong on that, and flaky on top of it, because this per-page fan-out races when several
    // logical pages resolve to the same object — exactly the redundancy substrate's `get_batch`
    // dedupe-by-object fixes. This primitive is superseded by `get_batch`; the F4 wake redesign will
    // consume it, and this test's load-bearing half is the zero-re-fault check below.)
    assert!(
        (1..=n_pages).contains(&after_prefetch),
        "prefetch should have faulted between 1 and {n_pages} distinct objects from the cold tier, \
         saw {after_prefetch}"
    );

    // The payoff: reading the whole file back now must add ZERO tier misses — every page the read
    // touches was coalesced in by the prefetch and is served from the warm local cache.
    let mut whole = vec![0u8; data.len()];
    let got = serve_read(&source, total_len, 0, &mut whole).unwrap();
    assert_eq!(got, data.len());
    assert_eq!(whole, data, "warm read did not match the seeded bytes");

    let after_read = source.store().stats().misses;
    assert_eq!(
        after_read, after_prefetch,
        "a read after a full prefetch re-faulted the tier ({} new misses) — prefetch did not warm the \
         set it was asked to warm",
        after_read - after_prefetch
    );

    drop(rt);
}
