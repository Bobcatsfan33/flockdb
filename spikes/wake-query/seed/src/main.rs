//! Seed a sleeping FlockDB database for the wake → first-QUERY-RESULT measurement.
//!
//! Takes a plain `.duckdb` file (produced by the `measure seed` C++ harness, which links the real
//! libduckdb.a), chunks it into substrate logical pages — logical page `i` = file bytes
//! `[i*ps, (i+1)*ps)`, the identical dumb layout `flock-kernel/src/paging.rs::persist` uses — commits
//! them into a `TieredStore`, and `sleep()`s it: every page and the manifest go to the object-storage
//! tier, and all local state is dropped. What remains is a `WakeToken` the extension wakes from.
//!
//! The tier is whatever [`flock_vfs::remote_tier`] selects — local disk by default, or S3/MinIO under
//! `--features s3` with `FLOCK_VFS_S3_URL` set. That is the SAME selector the extension's wake path
//! (`flock_vfs_open`) uses, so a page written here is a page the extension reads back, byte for byte.
//!
//! It prints the manifest id, page size and total length as `KEY=VALUE` lines for `run.sh`.

use std::error::Error;
use std::path::PathBuf;
use substrate_pager::{PageStore, StoreConfig, DEFAULT_PAGE_SIZE};
use substrate_store::TieredStore;

fn usage() -> String {
    "usage: seed <plain.duckdb> <remote_dir> <seed_cache_dir> <pool>".to_string()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let realfile = PathBuf::from(args.next().ok_or_else(usage)?);
    let remote_dir = args.next().ok_or_else(usage)?;
    let seed_cache_dir = PathBuf::from(args.next().ok_or_else(usage)?);
    let pool = args.next().ok_or_else(usage)?;

    let page_size = DEFAULT_PAGE_SIZE; // 64 KiB, the FlockDB default (flock-kernel docs).

    std::fs::create_dir_all(&remote_dir)?;
    std::fs::create_dir_all(&seed_cache_dir)?;

    // The tier the wake path will read from — chosen by flock-vfs, so it cannot drift from wake.
    let remote = flock_vfs::remote_tier(&remote_dir, &pool)?;

    let config = StoreConfig {
        page_size,
        pool: pool.clone(),
        ..Default::default()
    };
    let store = TieredStore::open(&seed_cache_dir, remote, config).await?;

    // pages ← file, exactly as persist() does for a fresh store: every chunk is new, so every one is
    // written.
    let bytes = std::fs::read(&realfile)?;
    let total_len = bytes.len() as u64;

    let pager = store.pager();
    let mut txn = pager.begin()?;
    for (i, chunk) in bytes.chunks(page_size).enumerate() {
        pager.write(&mut txn, i as u64, chunk.to_vec())?;
    }
    let head = pager.commit(txn)?;

    // Sleep: flush every page to the tier, upload the manifest's whole ancestry, drop local state.
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
