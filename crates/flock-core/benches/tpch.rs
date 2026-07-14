//! TPC-H SF0.1 — the full FlockDB stack against raw DuckDB.
//!
//! docs/02 §7 sets the target at **< 15 % overhead**, and it is the right target: our storage layer
//! must not make the query engine we host look bad. A user who could get their answer 30 % faster
//! by deleting us will, and they would be right to.
//!
//! # Run it
//!
//! ```sh
//! cargo bench -p flock-core --features tpch
//! ```
//!
//! # This benchmark uses the network, and the library does not. Both facts matter
//!
//! `dbgen` and the 22 queries come from DuckDB's official TPC-H extension, which the benchmark
//! **downloads on its first run** (`INSTALL tpch`) and DuckDB caches in `~/.duckdb/extensions`.
//!
//! We wanted to avoid that. duckdb-rs advertises a `tpch` cargo feature that links the extension
//! statically — no network, ever — and **it does not work from crates.io**: the feature implies
//! `bundled-cmake`, whose build script requires `duckdb-sources/CMakeLists.txt`, a directory that
//! exists only in a git checkout of duckdb-rs. The published crate does not ship it, and the build
//! script panics. That is an upstream packaging bug and there is no configuration around it.
//!
//! So the honest statement is: **the benchmark needs the network once; the library never does.**
//! `cargo test --workspace --features airgap` is a separate CI job, and it is what actually holds
//! CLAUDE.md rule 5 — not a claim in a comment. A benchmark is a development tool, and no byte of
//! it is linked into anything a user runs.
//!
//! # What is being compared, and why it is a fair fight
//!
//! Both sides are DuckDB, on a real file, with **identical thread and memory settings**. The only
//! difference is that FlockDB's file was hydrated out of a content-addressed page store, and
//! DuckDB's was written by `dbgen` straight into place. A comparison in which one side quietly
//! gets more threads than the other is not a measurement, it is a press release (docs/04 §5: *we
//! will not ship a number we cannot reproduce*).
//!
//! # The second benchmark, which is the one that will hurt
//!
//! Queries are the number the target names, and they are the number FlockDB will look good on —
//! because on the read path **FlockDB is not in the call stack at all**. DuckDB opened a local file
//! and is reading it.
//!
//! The cost of the fallback lands somewhere else entirely: on `snapshot()`, which reads the whole
//! database back out of the file and diffs it against the pages. `bench_snapshot` measures that and
//! reports it in the same run, because a benchmark suite that only measures the operation you are
//! fast at is marketing.

#[cfg(not(feature = "tpch"))]
fn main() {
    eprintln!(
        "The TPC-H benchmark is behind a feature flag. Run:\n\
         \n\
             cargo bench -p flock-core --features tpch\n\
         \n\
         It is gated because `cargo test --all-targets` runs bench targets, and a multi-minute\n\
         TPC-H pass in the middle of the unit tests is the fastest way to teach a team to stop\n\
         running them. It downloads DuckDB's official TPC-H extension on first use."
    );
}

#[cfg(feature = "tpch")]
fn main() {
    tpch::run();
}

#[cfg(feature = "tpch")]
mod tpch {
    use criterion::Criterion;
    use duckdb::{Config, Connection};
    use flock_core::{Db, Flock, KernelOpts};
    use std::hint::black_box;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Both engines get exactly this. Four threads, not "however many the machine has", because a
    /// benchmark whose result depends on what else was running is not a benchmark.
    const THREADS: i64 = 4;
    const SCALE_FACTOR: f64 = 0.1;

    /// TPC-H query 15 defines a view, so running it twice in one connection fails on the second
    /// pass — which is exactly what a benchmark does. We exclude it and say so here and in the
    /// output, rather than quietly reporting a 21-query result as "TPC-H".
    const SKIPPED: &[i64] = &[15];

    pub fn run() {
        let mut c = Criterion::default().configure_from_args();

        let queries = load_queries();
        eprintln!(
            "\nTPC-H SF{SCALE_FACTOR}: {} queries (q15 excluded — it defines a view and cannot be \
             run twice in one connection)\n",
            queries.len()
        );

        // The headline number, measured plainly, before criterion's statistics get involved — so
        // that the figure quoted in the README can be reproduced by reading one line of output
        // rather than by interpreting a distribution.
        headline(&queries);

        bench_queries(&mut c, &queries);
        bench_snapshot(&mut c);

        c.final_summary();
    }

    // ── fixtures ────────────────────────────────────────────────────────────────────────────────

    /// Fetch (once) and load DuckDB's official TPC-H extension, then generate SF0.1.
    ///
    /// `INSTALL` is a network call on a cold machine and a no-op thereafter. If it fails, the
    /// message says so plainly rather than surfacing as "dbgen: no such function", which is what a
    /// missing extension actually looks like and which sends people hunting in the wrong place.
    const DBGEN: &str = "\
        INSTALL tpch; \
        LOAD tpch; \
        CALL dbgen(sf = 0.1);";

    fn config() -> Config {
        Config::default()
            .threads(THREADS)
            .unwrap_or_else(|e| panic!("could not configure DuckDB threads: {e}"))
    }

    /// Returns `()` rather than `!` so it can be passed straight to `unwrap_or_else`, which wants a
    /// `FnOnce(E) -> T`. It never actually returns.
    fn dbgen_failed<E: std::fmt::Display>(e: E) {
        panic!(
            "could not generate TPC-H data: {e}\n\n\
             This benchmark downloads DuckDB's official TPC-H extension on first use, so it needs \
             network access once (it is then cached in ~/.duckdb/extensions). The library itself \
             never touches the network — see `cargo test --workspace --features airgap`."
        )
    }

    /// Raw DuckDB, on a file, with SF0.1 generated into it. The control.
    fn raw_duckdb(dir: &Path) -> Connection {
        let path = dir.join("raw.duckdb");
        let conn = Connection::open_with_flags(&path, config())
            .unwrap_or_else(|e| panic!("could not open raw DuckDB: {e}"));
        conn.execute_batch(DBGEN).unwrap_or_else(dbgen_failed);
        conn
    }

    /// FlockDB, with SF0.1 generated into it, then **snapshotted, closed, and reopened** — so the
    /// database being queried is one that was rebuilt out of pages, not one DuckDB happens to have
    /// warm from having just written it. Skipping the reopen would benchmark a code path no user is
    /// ever on.
    fn flock(dir: &Path) -> Db {
        let opts = KernelOpts::default().threads(THREADS);
        {
            let mut db = Flock::open_with(dir, "tpch", opts.clone())
                .unwrap_or_else(|e| panic!("could not open FlockDB: {e}"));
            db.execute_batch(DBGEN).unwrap_or_else(dbgen_failed);
            db.snapshot()
                .unwrap_or_else(|e| panic!("could not snapshot: {e}"));
        }
        Flock::open_with(dir, "tpch", opts)
            .unwrap_or_else(|e| panic!("could not reopen FlockDB from pages: {e}"))
    }

    /// The queries, straight from DuckDB's own TPC-H extension. We do not hand-copy them: a
    /// benchmark against queries we transcribed ourselves is a benchmark against our typing.
    fn load_queries() -> Vec<(i64, String)> {
        let conn = Connection::open_in_memory()
            .unwrap_or_else(|e| panic!("could not open DuckDB to read the query set: {e}"));
        conn.execute_batch("INSTALL tpch; LOAD tpch;")
            .unwrap_or_else(dbgen_failed);
        let mut stmt = conn
            .prepare("SELECT query_nr, query FROM tpch_queries() ORDER BY query_nr")
            .unwrap_or_else(|e| panic!("could not read tpch_queries(): {e}"));
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap_or_else(|e| panic!("could not read the TPC-H query set: {e}"));

        rows.filter_map(|r| r.ok())
            .filter(|(nr, _)| !SKIPPED.contains(nr))
            .collect()
    }

    // ── the headline: a PAIRED ratio, because the machine is noisier than the effect ─────────────

    /// How many paired rounds. Each round times both engines back to back.
    const ROUNDS: u32 = 15;

    /// Report the overhead as the **median of per-round ratios**, not as the ratio of the means.
    ///
    /// # Why this is not statistical fussiness
    ///
    /// The first version of this benchmark took the mean of five raw runs and the mean of five
    /// FlockDB runs and divided. Run twice, it reported **+0.7 %** and then **+8.2 %**; criterion,
    /// on the same code, reported FlockDB *faster than raw DuckDB* — which is impossible, since on
    /// the read path FlockDB is literally DuckDB reading a local file it opened itself.
    ///
    /// The measurement was picking up the laptop, not the code. Thermal state, the page cache, and
    /// whatever else the machine was doing swamp a difference that is structurally near zero.
    ///
    /// Pairing fixes it. Each round times raw and FlockDB **back to back**, so a second in which
    /// the machine is slow makes *both* slow and the ratio is unmoved. We report the median of
    /// those ratios, plus the full range, so a reader can see the noise rather than take our word
    /// about it. Publishing a single flattering mean from a noisy rig is exactly the kind of number
    /// docs/04 §5 says we will not ship.
    fn headline(queries: &[(i64, String)]) {
        let raw_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("no temp dir: {e}"));
        let flock_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("no temp dir: {e}"));

        let conn = raw_duckdb(raw_dir.path());
        let mut db = flock(flock_dir.path());

        // Warm both. We are measuring steady-state query cost, not the cost of DuckDB faulting a
        // fresh file into the page cache — unwarmed, we would be measuring which of the two the OS
        // happened to have cached.
        for _ in 0..2 {
            run_all_raw(&conn, queries);
            run_all_flock(&mut db, queries);
        }

        let mut ratios: Vec<f64> = Vec::with_capacity(ROUNDS as usize);
        let mut raw_total = Duration::ZERO;
        let mut flock_total = Duration::ZERO;

        for _ in 0..ROUNDS {
            let t = Instant::now();
            run_all_raw(&conn, queries);
            let raw = t.elapsed();

            let t = Instant::now();
            run_all_flock(&mut db, queries);
            let flk = t.elapsed();

            raw_total += raw;
            flock_total += flk;
            ratios.push(flk.as_secs_f64() / raw.as_secs_f64());
        }

        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |r: f64| (r - 1.0) * 100.0;
        let median = pct(ratios[ratios.len() / 2]);
        let best = pct(ratios[0]);
        let worst = pct(ratios[ratios.len() - 1]);

        eprintln!("────────────────────────────────────────────────────────────────────");
        eprintln!(
            "  TPC-H SF{SCALE_FACTOR} · {} queries · {THREADS} threads · {ROUNDS} paired rounds",
            queries.len()
        );
        eprintln!("    raw DuckDB, mean : {:>12.2?}", raw_total / ROUNDS);
        eprintln!("    FlockDB,    mean : {:>12.2?}", flock_total / ROUNDS);
        eprintln!("    overhead, MEDIAN of paired ratios : {median:>+7.1} %");
        eprintln!("    overhead, range  (best … worst)   : {best:>+7.1} % … {worst:>+.1} %");
        eprintln!("    docs/02 §7 target                 :   < 15.0 %");
        eprintln!("────────────────────────────────────────────────────────────────────\n");
    }

    fn run_all_raw(conn: &Connection, queries: &[(i64, String)]) {
        for (nr, sql) in queries {
            let mut stmt = conn
                .prepare(sql)
                .unwrap_or_else(|e| panic!("raw DuckDB could not prepare TPC-H q{nr}: {e}"));
            let batches: Vec<_> = stmt
                .query_arrow([])
                .unwrap_or_else(|e| panic!("raw DuckDB failed TPC-H q{nr}: {e}"))
                .collect();
            black_box(batches);
        }
    }

    fn run_all_flock(db: &mut Db, queries: &[(i64, String)]) {
        for (nr, sql) in queries {
            let rows = db
                .query(sql)
                .unwrap_or_else(|e| panic!("FlockDB failed TPC-H q{nr}: {e}"));
            black_box(rows);
        }
    }

    // ── criterion ───────────────────────────────────────────────────────────────────────────────

    fn bench_queries(c: &mut Criterion, queries: &[(i64, String)]) {
        let raw_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("no temp dir: {e}"));
        let flock_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("no temp dir: {e}"));

        let conn = raw_duckdb(raw_dir.path());
        let mut db = flock(flock_dir.path());

        let mut group = c.benchmark_group("tpch_sf0.1");
        group.sample_size(10);

        group.bench_function("duckdb_raw", |b| b.iter(|| run_all_raw(&conn, queries)));
        group.bench_function("flockdb", |b| b.iter(|| run_all_flock(&mut db, queries)));

        group.finish();
    }

    /// The cost of the fallback, measured rather than described.
    ///
    /// `snapshot()` reads the whole database file back and byte-compares every page against the
    /// store. On the read path FlockDB is invisible; **here** is where the DuckDB-owns-a-temp-file
    /// design charges its bill, and a benchmark suite that only measured the fast path would be
    /// telling half the truth on purpose.
    fn bench_snapshot(c: &mut Criterion) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("no temp dir: {e}"));
        let mut db = flock(dir.path());

        let mut group = c.benchmark_group("snapshot_sf0.1");
        group.sample_size(10);

        // An unchanged database: the pure diff cost. Every page is read and compared and nothing is
        // written. This is the floor, and it is O(database size) no matter how little changed.
        group.bench_function("unchanged", |b| {
            b.iter(|| {
                black_box(
                    db.snapshot()
                        .unwrap_or_else(|e| panic!("snapshot failed: {e}")),
                )
            })
        });

        // One row changed. Substrate writes a page or two; we still read the entire file to work
        // out which. The gap between this and `unchanged` is the write cost — the part that is
        // *shared* between them is the fallback's tax, and it is the part a filesystem hook would
        // delete outright.
        //
        // `iter_custom` rather than `iter_batched` because the INSERT must not be inside the timed
        // region — it is the setup, not the thing being measured — and `iter_batched`'s setup
        // closure would need a second mutable borrow of `db`. Timing the region by hand is the
        // boring way, and it is exact about what is counted.
        db.execute("CREATE TABLE bench_marker (i INTEGER)")
            .unwrap_or_else(|e| panic!("setup failed: {e}"));

        group.bench_function("one_row_changed", |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    db.execute("INSERT INTO bench_marker VALUES (1)")
                        .unwrap_or_else(|e| panic!("setup failed: {e}"));

                    let start = Instant::now();
                    let id = db
                        .snapshot()
                        .unwrap_or_else(|e| panic!("snapshot failed: {e}"));
                    elapsed += start.elapsed();

                    black_box(id);
                }
                elapsed
            })
        });

        group.finish();
    }
}
