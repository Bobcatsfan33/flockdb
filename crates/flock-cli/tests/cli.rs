//! What the CLI must do, asserted by running the actual binary.
//!
//! Not by calling the command functions — by *spawning the program*, because the two most dangerous
//! bugs this CLI can have are only visible from outside it:
//!
//! 1. **A write that does not survive the process exiting.** Every `flock` command is a separate
//!    process, and DuckDB's scratch file dies with it. An in-process test that opened a `Db`, wrote,
//!    and read back would pass while the shipped binary lost every row. So every assertion here that
//!    reads data reads it from a **new process**.
//! 2. **A panic instead of an error.** `unwrap()` on a missing file prints a backtrace and exits
//!    101. That is not a message, and the test that catches it has to see the exit code.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary, as cargo built it. No `assert_cmd`, no `PATH` games.
fn flock(pool: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_flock"));
    cmd.arg("--pool").arg(pool);
    cmd
}

fn run(pool: &Path, args: &[&str]) -> Output {
    flock(pool)
        .args(args)
        .output()
        .expect("the flock binary must be runnable")
}

fn ok(pool: &Path, args: &[&str]) -> String {
    let out = run(pool, args);
    assert!(
        out.status.success(),
        "flock {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn fails(pool: &Path, args: &[&str]) -> String {
    let out = run(pool, args);
    assert!(
        !out.status.success(),
        "flock {args:?} was supposed to fail, and did not.\nstdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // A CLI that panics has not reported an error, it has crashed. This assertion is in the shared
    // helper on purpose: it applies to *every* failing path in the program, not to the handful
    // someone remembered to check.
    assert!(
        !stderr.contains("panicked"),
        "flock {args:?} panicked instead of erroring:\n{stderr}"
    );
    assert!(
        stderr.starts_with("error: "),
        "flock {args:?} must fail with a message, not a Debug dump:\n{stderr}"
    );
    stderr
}

fn trades_csv() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/flock-cli.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/trades.csv")
}

fn csv() -> String {
    trades_csv().display().to_string()
}

// ── The five commands of the quickstart, one at a time ───────────────────────────────────────────

/// **The commit test.** `import` writes rows; a *different process* must be able to see them.
///
/// Fails if `import` does not snapshot: DuckDB's scratch file lives in a `TempDir` that is deleted
/// when the process exits, so without the snapshot this prints `imported 10 rows` and then hands the
/// next command an empty database.
#[test]
fn an_import_survives_the_process_that_did_it() {
    let pool = tempfile::tempdir().unwrap();

    let out = ok(pool.path(), &["import", &csv()]);
    assert!(out.contains("imported 10 rows"), "{out}");
    assert!(out.contains("\"trades\""), "{out}");
    assert!(out.contains("\"main\""), "{out}");

    // New process. New DuckDB. The only way these rows exist is substrate.
    let out = ok(pool.path(), &["sql", "SELECT count(*) AS n FROM trades"]);
    assert!(
        out.contains("10"),
        "the rows did not survive the import:\n{out}"
    );
}

/// **The other commit test**, and the one that would have bitten a real user first: a `DELETE` run
/// by `flock sql` must still be gone in the next process.
#[test]
fn a_write_survives_the_process_that_did_it() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    ok(
        pool.path(),
        &["sql", "DELETE FROM trades WHERE symbol = 'AAPL'"],
    );

    let out = ok(pool.path(), &["sql", "SELECT count(*) AS n FROM trades"]);
    assert!(
        out.contains('7'),
        "the DELETE did not survive the process:\n{out}"
    );
}

/// **The headline.** A fork is a real, separate database: writing to it cannot touch its parent.
///
/// This is `flock-core`'s property, asserted through the CLI — because the CLI is where it can be
/// broken by something as dull as writing HEAD at the wrong moment.
#[test]
fn a_write_to_a_fork_is_never_visible_in_the_parent() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let out = ok(pool.path(), &["branch", "what-if"]);
    assert!(out.contains("forked \"main\" → \"what-if\""), "{out}");
    assert!(out.contains("switched to branch \"what-if\""), "{out}");

    // `branch` checked out the fork, so this lands on the fork.
    ok(
        pool.path(),
        &["sql", "DELETE FROM trades WHERE symbol = 'AAPL'"],
    );

    let fork = ok(pool.path(), &["sql", "SELECT count(*) AS n FROM trades"]);
    assert!(
        fork.contains('7'),
        "the fork should have lost 3 rows:\n{fork}"
    );

    let parent = ok(
        pool.path(),
        &[
            "--branch",
            "main",
            "sql",
            "SELECT count(*) AS n FROM trades",
        ],
    );
    assert!(
        parent.contains("10"),
        "THE PARENT WAS MODIFIED BY A WRITE TO ITS FORK:\n{parent}"
    );
}

#[test]
fn branch_no_checkout_leaves_you_where_you_were() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let out = ok(pool.path(), &["branch", "aside", "--no-checkout"]);
    assert!(out.contains("still on \"main\""), "{out}");

    let out = ok(pool.path(), &["branches"]);
    assert!(out.contains("* main"), "HEAD should still be main:\n{out}");
    assert!(out.contains("aside"), "{out}");
}

#[test]
fn branches_marks_the_checked_out_one() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);
    ok(pool.path(), &["branch", "b"]);

    let out = ok(pool.path(), &["branches"]);
    assert!(out.contains("* b"), "{out}");
    assert!(out.contains("  main"), "{out}");

    ok(pool.path(), &["checkout", "main"]);
    let out = ok(pool.path(), &["branches"]);
    assert!(out.contains("* main"), "{out}");
}

// ── The errors. These are the product, not the exception path. ───────────────────────────────────

/// Every error message must name the next thing to type. This is the test that says so out loud.
#[test]
fn an_empty_directory_tells_you_how_to_make_a_database() {
    let pool = tempfile::tempdir().unwrap();
    let err = fails(pool.path(), &["sql", "SELECT 1"]);
    assert!(
        err.contains("flock import"),
        "no way forward offered:\n{err}"
    );
}

#[test]
fn an_unknown_branch_lists_the_ones_that_exist() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let err = fails(pool.path(), &["checkout", "nope"]);
    assert!(
        err.contains("main"),
        "the message must list what IS there:\n{err}"
    );
    assert!(err.contains("flock branch nope"), "{err}");
}

#[test]
fn a_table_that_already_exists_offers_the_three_ways_out() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let err = fails(pool.path(), &["import", &csv()]);
    assert!(err.contains("--table"), "{err}");
    assert!(err.contains("flock branch"), "{err}");
    assert!(err.contains("DROP TABLE"), "{err}");
}

#[test]
fn a_second_import_under_a_new_name_works() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);
    ok(pool.path(), &["import", &csv(), "--table", "trades_backup"]);

    let out = ok(
        pool.path(),
        &["sql", "SELECT count(*) AS n FROM trades_backup"],
    );
    assert!(out.contains("10"), "{out}");
}

#[test]
fn an_unknown_file_format_names_the_extension_and_the_way_round_it() {
    let pool = tempfile::tempdir().unwrap();
    let path = pool.path().join("data.xlsx");
    std::fs::write(&path, b"not a spreadsheet").unwrap();

    let err = fails(pool.path(), &["import", &path.display().to_string()]);
    assert!(err.contains(".xlsx"), "{err}");
    assert!(err.contains("read_json_auto"), "the escape hatch:\n{err}");
}

#[test]
fn a_missing_file_is_an_error_and_not_a_panic() {
    let pool = tempfile::tempdir().unwrap();
    let err = fails(pool.path(), &["import", "/no/such/file.csv"]);
    assert!(err.contains("no such file"), "{err}");
}

#[test]
fn broken_sql_is_duckdbs_error_not_a_crash() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let err = fails(pool.path(), &["sql", "SELECT * FROM nonexistent_table"]);
    assert!(err.to_lowercase().contains("nonexistent_table"), "{err}");
}

/// A name that would walk out of the pool is refused, not sanitised. `flock-core` enforces this
/// (a pool is a security boundary); the CLI must not swallow the error on its way out.
#[test]
fn a_branch_name_that_escapes_the_pool_is_refused() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let err = fails(pool.path(), &["branch", "../escape"]);
    assert!(err.contains("is not usable"), "{err}");
}

/// We do not fake S3, and the message has to say what we *do* support rather than "invalid value".
#[test]
fn an_s3_url_is_refused_by_name_and_not_turned_into_a_directory() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let err = fails(pool.path(), &["sleep", "--tier", "s3://bucket/prefix"]);
    assert!(err.contains("s3://bucket/prefix"), "{err}");
    assert!(err.contains("directory"), "{err}");
    assert!(
        !pool.path().join("s3:").exists(),
        "a directory called `s3:` was created — the URL was taken literally"
    );
}

/// The lint bans `unwrap` in this crate's code, but the thing that actually matters to a user is
/// that no input produces a backtrace. So: throw junk at it and check the exit code.
#[test]
fn no_input_makes_it_panic() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let junk: &[&[&str]] = &[
        &["sql", ""],
        &["sql", "'"],
        &["sql", "SELECT * FROM"],
        &["import", ""],
        &["import", "/"],
        &["branch", ""],
        &["branch", "."],
        &["checkout", ""],
        &["wake", "main"],
        &["wake", "does-not-exist"],
        &["--branch", "", "sql", "SELECT 1"],
    ];

    for args in junk {
        let out = run(pool.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("panicked"),
            "flock {args:?} panicked:\n{stderr}"
        );
        // 101 is Rust's panic exit code. A clean error is 1.
        assert_ne!(
            out.status.code(),
            Some(101),
            "flock {args:?} exited with the panic code:\n{stderr}"
        );
    }
}

/// `flock wake` on a branch that is not asleep must say so, rather than doing something surprising.
#[test]
fn waking_a_branch_that_is_awake_says_so() {
    let pool = tempfile::tempdir().unwrap();
    ok(pool.path(), &["import", &csv()]);

    let err = fails(pool.path(), &["wake", "main"]);
    assert!(err.contains("not asleep"), "{err}");
}

/// The pool is where we say it is. `$FLOCK_POOL` is the only stateful thing about this CLI besides
/// `HEAD`, and a bug in it means writing someone's data into the wrong directory.
#[test]
fn flock_pool_env_var_is_honoured() {
    let pool = tempfile::tempdir().unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_flock"))
        .env("FLOCK_POOL", pool.path())
        .args(["import", &csv()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(pool.path().join("HEAD").is_file());
    assert!(pool.path().join("dbs/main").is_dir());
    assert!(pool.path().join("cas/pages").is_dir());
}
