//! `flock sleep` and `flock wake`, and the one assertion that makes them mean anything.
//!
//! # The wipe is the test
//!
//! `flock sleep` deliberately does **not** delete the pool's pages (sleeping frees compute, not
//! disk — see `cmd::tier`). So a `wake` that read them straight back off local disk would pass a
//! naive test while never touching the tier at all, and we would ship a `sleep` that quietly
//! depended on the machine it slept on.
//!
//! So the test **deletes the pool's entire content-addressed store** between the two. `rm -rf
//! .flock/cas`. Not an eviction — every page, gone. After that there is exactly one place in the
//! universe the data can come from, and if `wake` returns the rows, it came from there.

use std::path::Path;
use std::process::Command;

fn flock(pool: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_flock"))
        .arg("--pool")
        .arg(pool)
        .args(args)
        .output()
        .expect("the flock binary must be runnable");
    assert!(
        out.status.success(),
        "flock {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn csv() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/trades.csv")
        .display()
        .to_string()
}

#[test]
fn a_database_survives_sleep_a_total_wipe_of_the_pool_and_wake() {
    let home = tempfile::tempdir().unwrap();
    let pool = home.path().join("pool");
    let cold = home.path().join("cold");

    flock(&pool, &["import", &csv()]);
    flock(&pool, &["sql", "DELETE FROM trades WHERE symbol = 'TSLA'"]);

    let out = flock(&pool, &["sleep", "--tier", &cold.display().to_string()]);
    assert!(out.contains("is asleep"), "{out}");

    // The branch's own directory — its private write-ahead log, which holds its head — is gone.
    assert!(
        !pool.join("dbs/main").exists(),
        "sleep should have released the branch"
    );
    assert!(
        pool.join("asleep/main.json").is_file(),
        "sleep should have left a WakeToken behind"
    );

    // **The wipe.** Every page this database has ever had, deleted from local disk.
    std::fs::remove_dir_all(pool.join("cas")).expect("wipe the pool's CAS");
    assert!(!pool.join("cas").exists());

    let out = flock(&pool, &["wake", "main"]);
    assert!(out.contains("is awake"), "{out}");

    // There is exactly one place these 8 rows can have come from.
    let out = flock(&pool, &["sql", "SELECT count(*) AS n FROM trades"]);
    assert!(out.contains('8'), "the data did not come back:\n{out}");

    // And the DELETE that happened before the sleep is still a DELETE.
    let out = flock(
        &pool,
        &[
            "sql",
            "SELECT count(*) AS n FROM trades WHERE symbol = 'TSLA'",
        ],
    );
    assert!(
        out.contains('0'),
        "the sleeping database woke up with stale rows:\n{out}"
    );
}

/// A sleeping branch is still a branch: it must show up, marked, and refuse to be queried with a
/// message that says how to fix that.
#[test]
fn a_sleeping_branch_is_listed_and_refuses_to_be_queried() {
    let home = tempfile::tempdir().unwrap();
    let pool = home.path().join("pool");

    flock(&pool, &["import", &csv()]);
    flock(&pool, &["sleep"]);

    let out = flock(&pool, &["branches"]);
    assert!(out.contains("asleep"), "{out}");

    let out = Command::new(env!("CARGO_BIN_EXE_flock"))
        .arg("--pool")
        .arg(&pool)
        .args(["sql", "SELECT 1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("flock wake main"), "{err}");
}

/// Waking must restore the branch as a normal, writable, forkable branch — not a read-only ghost.
#[test]
fn a_woken_branch_can_be_written_to_and_forked() {
    let home = tempfile::tempdir().unwrap();
    let pool = home.path().join("pool");

    flock(&pool, &["import", &csv()]);
    flock(&pool, &["sleep"]);
    std::fs::remove_dir_all(pool.join("cas")).unwrap();
    flock(&pool, &["wake", "main"]);

    flock(
        &pool,
        &["sql", "INSERT INTO trades VALUES ('AMD', 5, 1.0, 'XNAS')"],
    );
    flock(&pool, &["branch", "after-wake"]);
    flock(&pool, &["sql", "DELETE FROM trades WHERE symbol = 'AMD'"]);

    let fork = flock(&pool, &["sql", "SELECT count(*) AS n FROM trades"]);
    assert!(fork.contains("10"), "{fork}");

    let parent = flock(
        &pool,
        &[
            "--branch",
            "main",
            "sql",
            "SELECT count(*) AS n FROM trades",
        ],
    );
    assert!(
        parent.contains("11"),
        "a woken branch lost its fork isolation:\n{parent}"
    );
}
