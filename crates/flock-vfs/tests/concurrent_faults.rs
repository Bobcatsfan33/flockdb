//! Regression: concurrent faults of the *same* page must not race in the local cache.
//!
//! A query engine that faults through `TieredPageSource` from many threads at once (DuckDB's
//! multi-threaded scan is the real case) will, at a page boundary, ask two threads to fault the *same*
//! not-yet-cached page simultaneously. Substrate's local CAS fills a faulted page through a temp file
//! named for the page's content hash and then renames it into place; two writers of the same page share
//! that temp name, so without serialization one thread's `rename` loses the temp to the other and fails
//! with `ENOENT` — surfacing as `flock_vfs_pread ... No such file or directory (os error 2)`. This is
//! exactly the failure the wake→query measurement hit at 30 MB, where the parallel `cold` scan first
//! touches every page. [`TieredPageSource`] gates faults per page to make it safe; this test drives the
//! race hard and asserts every byte still comes back correct.

use flock_vfs::{serve_read, TieredPageSource};
use object_store::local::LocalFileSystem;
use std::sync::Arc;
use substrate_pager::{PageStore, StoreConfig};
use substrate_store::{RemoteTier, TieredStore};

/// Distinct, incompressible-ish bytes so every 64 KiB page has unique content (no CAS dedupe) — the
/// same property the md5-seeded DuckDB file in the measurement has. splitmix64 mixes the high bits of
/// `i` into the low byte, so neighbouring pages differ.
fn distinct_bytes(total_len: usize) -> Vec<u8> {
    (0..total_len as u64)
        .map(|i| {
            let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as u8
        })
        .collect()
}

#[test]
fn concurrent_faults_of_the_same_page_do_not_race() {
    let page_size = 65536usize;
    // The size the measurement fails at: 1.2M cold rows ≈ 30 MB over 481 pages.
    let total_len = 31_469_568usize;
    let data = Arc::new(distinct_bytes(total_len));
    let pool = "vfs-concurrent";

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

    // Seed the pages, then sleep so the wake below must fault every page from the tier.
    let token = rt.block_on(async {
        let backend = LocalFileSystem::new_with_prefix(&remote_dir).unwrap();
        let remote = RemoteTier::new(Arc::new(backend), pool.to_string());
        let config = StoreConfig {
            page_size,
            pool: pool.to_string(),
            ..Default::default()
        };
        let store = TieredStore::open(&seed_cache, remote, config)
            .await
            .unwrap();
        let pager = store.pager();
        let mut txn = pager.begin().unwrap();
        for (i, chunk) in data.chunks(page_size).enumerate() {
            pager.write(&mut txn, i as u64, chunk.to_vec()).unwrap();
        }
        pager.commit(txn).unwrap();
        store.sleep().await.unwrap()
    });

    let backend = LocalFileSystem::new_with_prefix(&remote_dir).unwrap();
    let remote = RemoteTier::new(Arc::new(backend), pool.to_string());
    let store = rt
        .block_on(TieredStore::wake(&wake_cache, remote, &token))
        .unwrap();
    let source = Arc::new(TieredPageSource::new(Arc::new(store), total_len as u64));

    // Many threads, each reading the whole file from a staggered start, so their faults land on the same
    // pages at the same time — the pattern that triggers the temp-name collision.
    let nthreads = 16usize;
    let mut handles = Vec::new();
    for t in 0..nthreads {
        let source = Arc::clone(&source);
        let data = Arc::clone(&data);
        handles.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; page_size];
            let mut off = (t * page_size) % total_len;
            for _ in 0..(total_len / page_size + 1) {
                let n = serve_read(&*source, total_len as u64, off as u64, &mut buf)
                    .unwrap_or_else(|e| panic!("thread {t} fault at offset {off}: {e}"));
                let end = (off + n).min(total_len);
                assert_eq!(
                    &buf[..n],
                    &data[off..end],
                    "thread {t} wrong bytes at {off}"
                );
                off = (off + page_size) % total_len;
            }
        }));
    }
    for h in handles {
        h.join().expect("a faulting thread panicked");
    }

    // Every page faulted from the tier exactly once, no matter how many threads raced for it: the fault
    // count is a property of the DATA (481 pages), not of the concurrency.
    let stats = source.store().stats();
    assert_eq!(
        stats.misses,
        (total_len / page_size + 1) as u64,
        "each page should be faulted exactly once"
    );

    drop(rt);
}
