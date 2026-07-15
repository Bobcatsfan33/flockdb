//! Seed a sleeping FlockDB database for the wake-latency measurement.
//!
//! Takes a plain `.duckdb` file (produced by `duckharness seed`, which links the real libduckdb.a),
//! chunks it into substrate logical pages — logical page `i` = file bytes `[i*ps, (i+1)*ps)`, the
//! identical dumb layout `flock-kernel/src/paging.rs::persist` uses — commits them into a
//! `TieredStore`, and `sleep()`s it: every page and the manifest go to the local object-store tier,
//! and all local state is dropped. What remains is a `WakeToken` (pool + manifest id + page size),
//! which the shim wakes from.
//!
//! It then prints the manifest id, page size and total length as `KEY=VALUE` lines for `run.sh`.

use object_store::local::LocalFileSystem;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use substrate_pager::{PageStore, StoreConfig, DEFAULT_PAGE_SIZE};
use substrate_store::{RemoteTier, TieredStore};

fn usage() -> String {
    "usage: seed <plain.duckdb> <remote_dir> <seed_cache_dir> <pool>".to_string()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let realfile = PathBuf::from(args.next().ok_or_else(usage)?);
    let remote_dir = PathBuf::from(args.next().ok_or_else(usage)?);
    let seed_cache_dir = PathBuf::from(args.next().ok_or_else(usage)?);
    let pool = args.next().ok_or_else(usage)?;

    let page_size = DEFAULT_PAGE_SIZE; // 64 KiB, the FlockDB default (flock-kernel docs).

    std::fs::create_dir_all(&remote_dir)?;
    std::fs::create_dir_all(&seed_cache_dir)?;

    let backend = LocalFileSystem::new_with_prefix(&remote_dir)?;
    let remote = RemoteTier::new(Arc::new(backend), pool.clone());

    let config = StoreConfig {
        page_size,
        pool: pool.clone(),
        ..Default::default()
    };
    let store = TieredStore::open(&seed_cache_dir, remote, config).await?;

    // pages ← file, exactly as persist() does for a fresh store: every chunk is new, so every one
    // is written.
    let bytes = std::fs::read(&realfile)?;
    let total_len = bytes.len() as u64;

    let pager = store.pager();
    let mut txn = pager.begin()?;
    for (i, chunk) in bytes.chunks(page_size).enumerate() {
        pager.write(&mut txn, i as u64, chunk.to_vec())?;
    }
    let head = pager.commit(txn)?;

    // Sleep: flush every page to the tier, upload the manifest's whole ancestry, drop local state.
    // After this the tier holds the entire database and the local cache holds nothing.
    let token = store.sleep().await?;
    if token.manifest != head {
        return Err(format!(
            "seed: sleep changed the head from {} to {} — the seeded manifest is not what we slept",
            head.to_hex(),
            token.manifest.to_hex()
        )
        .into());
    }

    println!("MANIFEST={}", token.manifest.to_hex());
    println!("PAGE_SIZE={page_size}");
    println!("TOTAL_LEN={total_len}");
    println!("POOL={pool}");
    Ok(())
}
