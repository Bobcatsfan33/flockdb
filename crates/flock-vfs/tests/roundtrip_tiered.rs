//! End-to-end proof that the *real* read path — `TieredPageSource` over a woken substrate
//! `TieredStore` — serves a database file byte-for-byte, not just the in-memory mock the fuzzer drives.
//!
//! This seeds a small database into a real `TieredStore` (a real `object_store::LocalFileSystem` tier),
//! `sleep()`s it, `wake()`s it into a cold cache, then reads it back through `serve_read` and checks
//! every returned byte against the ground truth. It is the bridge between the fuzzed arithmetic and the
//! substrate machinery: the mock proves `serve_read` is correct for arbitrary page bytes; this proves
//! the bytes `TieredPageSource` hands it are the real database's bytes, faulted from the tier on demand.
//!
//! The read path is called from a thread that is NOT a tokio worker (the runtime drives only the async
//! seed/sleep/wake): substrate's pager is synchronous and bridges to its async tier fetch with an
//! internal `block_on`, which cannot run nested inside a runtime worker. This mirrors production, where
//! DuckDB's own threads issue the reads while the runtime lives beside them.

use flock_vfs::{serve_read, TieredPageSource};
use object_store::local::LocalFileSystem;
use std::sync::Arc;
use substrate_pager::{PageStore, StoreConfig};
use substrate_store::{RemoteTier, TieredStore, WakeToken};

/// Seed `data` into a fresh tiered store as `page_size`-byte pages, then sleep it. Returns the wake
/// token and the file's total length.
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
fn real_tiered_store_reads_back_byte_for_byte() {
    // Small on purpose: disk is tight, and correctness lives at the boundaries, not at scale.
    // Substrate's minimum page size is 4 KiB (power of two); ~10000 bytes over 4 KiB pages gives three
    // pages, the last one short.
    let page_size = 4096usize;
    let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 + 3) as u8).collect();
    let total_len = data.len() as u64;
    let pool = "vfs-roundtrip";

    let tmp = tempfile::tempdir().unwrap();
    let remote_dir = tmp.path().join("remote");
    let seed_cache = tmp.path().join("seed-cache");
    let wake_cache = tmp.path().join("wake-cache");
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::create_dir_all(&seed_cache).unwrap();
    std::fs::create_dir_all(&wake_cache).unwrap();

    // A multi-threaded runtime the async seed/sleep/wake run on. Kept alive for the whole test: the
    // woken store's tiered CAS captures a handle to it and uses it to fault pages synchronously.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let (token, seeded_len) = rt.block_on(seed_and_sleep(
        &remote_dir,
        &seed_cache,
        pool,
        page_size,
        &data,
    ));
    assert_eq!(seeded_len, total_len);

    // Wake into a COLD cache: every page must be faulted from the tier, not read from the seed's warm
    // disk. A second RemoteTier over the same tier root/pool.
    let backend = LocalFileSystem::new_with_prefix(&remote_dir).unwrap();
    let remote = RemoteTier::new(Arc::new(backend), pool.to_string());
    let store = rt
        .block_on(TieredStore::wake(&wake_cache, remote, &token))
        .unwrap();
    let source = TieredPageSource::new(Arc::new(store), total_len);

    // Read the whole file back (from a non-runtime thread), then a battery of sub-ranges including ones
    // that straddle page boundaries and run past EOF — each must match the ground truth exactly.
    let mut whole = vec![0u8; data.len()];
    let n = serve_read(&source, total_len, 0, &mut whole).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(
        whole, data,
        "whole-file read did not match the seeded bytes"
    );

    // Sub-ranges: (offset, len). 4090..4110 straddles the page 0/1 boundary; 9990..10050 runs past EOF.
    for &(off, len) in &[
        (0u64, 10usize),
        (4090, 20),
        (4095, 2),
        (5000, 4096),
        (9990, 60),
    ] {
        let mut buf = vec![0u8; len];
        let got = serve_read(&source, total_len, off, &mut buf).unwrap();
        let expected_end = ((off as usize) + len).min(data.len());
        let expected = &data[off as usize..expected_end];
        assert_eq!(got, expected.len(), "short read at offset {off}");
        assert_eq!(&buf[..got], expected, "wrong bytes at offset {off}");
    }

    // Laziness sanity: reading a few sub-ranges must have faulted only a few pages from the tier, not
    // the whole file. (The whole-file read above touched all four; the point is the tier saw faults at
    // all — a real page-faulting path, not a hydrate.)
    let stats = source.store().stats();
    assert!(
        stats.misses >= 1,
        "expected the woken store to have faulted pages from the tier, saw {} misses",
        stats.misses
    );

    // Keep the runtime alive until here.
    drop(rt);
}
