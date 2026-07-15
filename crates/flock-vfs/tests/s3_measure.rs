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

use flock_vfs::{serve_read, TieredPageSource};
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
