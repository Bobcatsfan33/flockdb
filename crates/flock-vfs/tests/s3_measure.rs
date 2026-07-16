//! `#[ignore]`d: clock the read path against a REAL object-storage endpoint.
//!
//! RISK-1's open question is not "is wake lazy" (the F5 spike showed it is, flat, at a zero-network
//! floor) — it is "what does the object-storage round-trip cost per fault", because that is the ~60% of
//! a 250 ms budget the zero-network floor does not include. This test measures exactly that, for the
//! read path F4 built, WITHOUT DuckDB: it seeds a small database into a real bucket, sleeps it, wakes it
//! into a cold cache, and times wake→first-page-fault through `serve_read`. It is the flock-vfs analogue
//! of flock-core's `wake_latency_against_a_real_s3_endpoint`, but it needs no DuckDB build — so it can
//! run where the full engine's test binary cannot be linked.
//!
//! It measures **wake → first page read**, not wake → first *query* (there is no SQL engine here); the
//! per-fault object-storage latency it reports is the load-bearing part of the wake→first-query number.
//!
//! ## Run it
//!
//! ```sh
//! # start any S3-compatible endpoint with a bucket named `flockdb`, e.g. MinIO:
//! MINIO_ROOT_USER=minioadmin MINIO_ROOT_PASSWORD=minioadmin minio server /data --address :9000
//! MINIO_URL=http://127.0.0.1:9000 cargo test -p flock-vfs --test s3_measure -- --ignored --nocapture
//! ```
//!
//! ## Read this before believing the number
//!
//! It measures a **small** database (a handful of 4 KiB pages), so it isolates the per-fault round-trip,
//! not a full scan. Wake-one-of-many is flat in the fault SET (F4/F5); this puts a real network latency
//! on each of those faults. No 250 ms claim is made from it — it is one honest measurement of the piece
//! that was previously unmeasured.

use flock_vfs::{serve_read, PageSource, TieredPageSource};
use object_store::aws::AmazonS3Builder;
use std::sync::Arc;
use std::time::Instant;
use substrate_pager::{PageStore, StoreConfig};
use substrate_store::{RemoteTier, TieredStore, WakeToken};

async fn seed_and_sleep(
    remote: RemoteTier,
    seed_cache: &std::path::Path,
    pool: &str,
    page_size: usize,
    data: &[u8],
) -> (WakeToken, u64) {
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

fn s3_backend() -> RemoteTier {
    let url = std::env::var("MINIO_URL")
        .expect("MINIO_URL is not set — this test needs a real S3-compatible endpoint");
    let backend = AmazonS3Builder::new()
        .with_endpoint(url)
        .with_bucket_name(std::env::var("MINIO_BUCKET").unwrap_or_else(|_| "flockdb".into()))
        .with_access_key_id(std::env::var("MINIO_USER").unwrap_or_else(|_| "minioadmin".into()))
        .with_secret_access_key(
            std::env::var("MINIO_PASSWORD").unwrap_or_else(|_| "minioadmin".into()),
        )
        .with_allow_http(true)
        .build()
        .expect("build an S3 client");
    // A fresh pool per run so repeated runs do not read each other's warm objects.
    let pool = format!(
        "flockvfs-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    RemoteTier::new(Arc::new(backend), pool)
}

#[test]
#[ignore = "needs an S3-compatible endpoint; set MINIO_URL. Measures the read path's object-storage round-trip."]
fn wake_read_path_against_a_real_s3_endpoint() {
    let page_size = 4096usize;
    let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 + 3) as u8).collect();
    let total_len = data.len() as u64;

    let tmp = tempfile::tempdir().unwrap();
    let seed_cache = tmp.path().join("seed-cache");
    let wake_cache = tmp.path().join("wake-cache");
    std::fs::create_dir_all(&seed_cache).unwrap();
    std::fs::create_dir_all(&wake_cache).unwrap();

    let remote = s3_backend();
    let pool = remote.pool().to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // Seed + sleep into the real bucket.
    let (token, seeded_len) =
        rt.block_on(seed_and_sleep(remote, &seed_cache, &pool, page_size, &data));
    assert_eq!(seeded_len, total_len);

    // Wake into a COLD cache and time it. `wake` fetches only the head manifest (one round trip); pages
    // are faulted lazily below.
    let backend2 = s3_backend_same_pool(&pool);
    let t_wake = Instant::now();
    let store = rt
        .block_on(TieredStore::wake(&wake_cache, backend2, &token))
        .unwrap();
    let wake_ms = t_wake.elapsed().as_secs_f64() * 1000.0;
    let source = TieredPageSource::new(Arc::new(store), total_len);

    // First page read: faults the covering pages from S3. This is the round-trip the floor omitted.
    let t_read = Instant::now();
    let mut buf = vec![0u8; page_size];
    let n = serve_read(&source, total_len, 0, &mut buf).unwrap();
    let first_read_ms = t_read.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(n, page_size);
    assert_eq!(&buf[..], &data[..page_size]);

    // A point read further into the file, faulting another page.
    let t_point = Instant::now();
    let mut pbuf = vec![0u8; 256];
    let pn = serve_read(&source, total_len, 8000, &mut pbuf).unwrap();
    let point_ms = t_point.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(pn, 256);
    assert_eq!(&pbuf[..], &data[8000..8256]);

    let stats = source.store().stats();
    println!("--- flock-vfs read path vs REAL object storage (no DuckDB) ---");
    println!("wake (fetch head manifest)      : {wake_ms:.1} ms");
    println!("first page fault (4 KiB)        : {first_read_ms:.1} ms");
    println!("second point fault (256 B)      : {point_ms:.1} ms");
    println!(
        "wake + first read               : {:.1} ms",
        wake_ms + first_read_ms
    );
    println!("tier faults (S3 GETs)           : {}", stats.misses);
    println!("(measures wake -> first page read, NOT wake -> first query. No 250 ms claim.)");
}

/// The measurement that gates any latency claim and calibrates the prefetch fan-out: fault the SAME
/// warm set two ways against the SAME real bucket — **serially** (the reactive path: one fault, wait,
/// the next, as DuckDB drives it) and **coalesced** (`PageSource::prefetch`, which fans the faults out
/// concurrently) — and report both wall-clocks.
///
/// Why this is the number that matters: `docs/wake-latency.md` proved the fault SET is flat (~21 pages
/// regardless of database size) and that on a local/same-runner tier its faults are ~free. This puts a
/// REAL wide-area round-trip on each of those faults. Serially, a 21-page set costs ~21 × RTT — that is
/// where a sub-second wake dies. Coalesced, the same set costs ~1 × RTT plus fan-out overhead. The ratio
/// this prints is exactly how much the flockd wake scheduler's coalescing buys, and the per-fault RTT it
/// derives is what tells us whether the fan-out (currently 16) should grow.
///
/// **Scope, stated so no one over-reads it (CLAUDE.md rule 6):** the number is only a *wide-area* number
/// if `MINIO_URL` points at a bucket a real network away from this process. Against same-runner MinIO
/// (the `wake-latency.yml` control) the RTT is ~0 and serial≈coalesced — which is itself the honest
/// finding that the same-runner endpoint is NOT wide-area. **No 250 ms claim is made from this test**,
/// and none may be quoted until it is run against a genuinely remote bucket. It measures fault fetch, not
/// wake→first-*query* (no SQL engine here); it is the load-bearing input to that number, not the number.
#[test]
#[ignore = "needs an S3-compatible endpoint; set MINIO_URL. Measures serial vs coalesced fault fetch — the wide-area gate on any latency claim."]
fn serial_vs_coalesced_fault_fetch_against_a_real_s3_endpoint() {
    let page_size = 4096usize;
    // A warm set of ~21 pages — the wake-one-of-many fault set size. Seed enough distinct pages that
    // faulting them serially vs concurrently is a real difference against a real RTT.
    let warm_pages: u64 = 21;
    let n_pages: u64 = 40; // a little larger than the warm set, so the set is a subset, not the whole file
    let data: Vec<u8> = (0..(n_pages * page_size as u64))
        .map(|i| (i.wrapping_mul(7).wrapping_add(3)) as u8)
        .collect();
    let total_len = data.len() as u64;
    // Spread the warm set across the file so the faults are distinct objects, not one contiguous run.
    let warm_set: Vec<u64> = (0..warm_pages).map(|i| i * n_pages / warm_pages).collect();

    let tmp = tempfile::tempdir().unwrap();
    let seed_cache = tmp.path().join("seed-cache");
    std::fs::create_dir_all(&seed_cache).unwrap();

    let remote = s3_backend();
    let pool = remote.pool().to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let (token, seeded_len) =
        rt.block_on(seed_and_sleep(remote, &seed_cache, &pool, page_size, &data));
    assert_eq!(seeded_len, total_len);

    // --- Wake A (cold): fault the warm set SERIALLY, exactly as a reactive read path would.
    let cache_a = tmp.path().join("wake-serial");
    std::fs::create_dir_all(&cache_a).unwrap();
    let store_a = rt
        .block_on(TieredStore::wake(
            &cache_a,
            s3_backend_same_pool(&pool),
            &token,
        ))
        .unwrap();
    let source_a = TieredPageSource::new(Arc::new(store_a), total_len);
    let t_serial = Instant::now();
    for &p in &warm_set {
        // One fault, then the next — the serial pattern. (Default prefetch is exactly this loop, but
        // spelled out here so the comparison is unmistakable.)
        source_a.read_page(p).unwrap();
    }
    let serial_ms = t_serial.elapsed().as_secs_f64() * 1000.0;
    let serial_faults = source_a.store().stats().misses;

    // --- Wake B (cold, fresh cache): fault the SAME set COALESCED, via prefetch.
    let cache_b = tmp.path().join("wake-coalesced");
    std::fs::create_dir_all(&cache_b).unwrap();
    let store_b = rt
        .block_on(TieredStore::wake(
            &cache_b,
            s3_backend_same_pool(&pool),
            &token,
        ))
        .unwrap();
    let source_b = TieredPageSource::new(Arc::new(store_b), total_len);
    let t_coalesced = Instant::now();
    source_b.prefetch(&warm_set);
    let coalesced_ms = t_coalesced.elapsed().as_secs_f64() * 1000.0;
    let coalesced_faults = source_b.store().stats().misses;

    let per_fault_rtt = serial_ms / warm_pages as f64;
    let speedup = if coalesced_ms > 0.0 {
        serial_ms / coalesced_ms
    } else {
        f64::INFINITY
    };
    println!("--- serial vs coalesced fault fetch of a {warm_pages}-page warm set (REAL object storage) ---");
    println!(
        "serial   (reactive, one fault at a time) : {serial_ms:.1} ms  ({serial_faults} GETs)"
    );
    println!("coalesced (prefetch, concurrent)         : {coalesced_ms:.1} ms  ({coalesced_faults} GETs)");
    println!("derived per-fault RTT (serial / pages)   : {per_fault_rtt:.1} ms");
    println!("coalescing speedup                       : {speedup:.1}x");
    println!("(If speedup ≈ 1x, MINIO_URL is a low-latency/same-runner endpoint — NOT wide-area. No 250 ms claim.)");

    // Correctness, not just timing: coalesced must fault the same set. (Both wakes are cold, so both
    // pay the same GETs; only their overlap in time differs.)
    assert_eq!(
        serial_faults, coalesced_faults,
        "serial and coalesced faulted different numbers of pages — not a like-for-like comparison"
    );
    assert!(
        coalesced_faults >= warm_pages,
        "coalesced fault should have faulted the whole warm set from the cold tier"
    );

    drop(rt);
}

fn s3_backend_same_pool(pool: &str) -> RemoteTier {
    let url = std::env::var("MINIO_URL").unwrap();
    let backend = AmazonS3Builder::new()
        .with_endpoint(url)
        .with_bucket_name(std::env::var("MINIO_BUCKET").unwrap_or_else(|_| "flockdb".into()))
        .with_access_key_id(std::env::var("MINIO_USER").unwrap_or_else(|_| "minioadmin".into()))
        .with_secret_access_key(
            std::env::var("MINIO_PASSWORD").unwrap_or_else(|_| "minioadmin".into()),
        )
        .with_allow_http(true)
        .build()
        .expect("build an S3 client");
    RemoteTier::new(Arc::new(backend), pool.to_string())
}
