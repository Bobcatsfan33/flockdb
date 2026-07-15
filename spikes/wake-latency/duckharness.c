// duckharness — the DuckDB side of the wake-latency spike. Links the crate's own libduckdb.a, so
// the read pattern is DuckDB's real one, not a model of it (same libduckdb.a the F4 trace used).
//
// Modes:
//   duckharness seed <path> <cold_rows>
//       Create hot(1000 rows) + cold(cold_rows) in one file and CHECKPOINT — the plain .duckdb the
//       Rust seeder chunks into substrate pages. Same schema as proofs/lazy-wake/harness.c.
//
//   duckharness wake <path> <which> <cold_rows>
//       Open <path> READ-ONLY, run one query, materialise the first value, and time
//       wake→first-row. If the faultshim dylib is loaded (DYLD_INSERT_LIBRARIES), its
//       flock_fault_boot/wake are called around the timed region so the timer spans a real page
//       fault path; the fault count is read back. Without the shim, this times a plain local-disk
//       open of a real file — the control / expected-value run.
//
//       <which>:
//         open  -> SELECT 1                                   (pure wake, no table)
//         hot   -> SELECT sum(v % 1000)::BIGINT FROM hot      (selective, small table)
//         point -> SELECT sum(v % 1000)::BIGINT FROM cold WHERE id = mid   (zonemap-pruned point)
//         cold  -> SELECT sum(v % 1000)::BIGINT FROM cold     (full scan, control — must scale)
//
//       `v % 1000` keeps the aggregate in int64 for a cheap exact correctness check; it still
//       forces reading the whole operand column, so the read locality is unchanged from F4's
//       sum(v). max()/count() were rejected: DuckDB can answer them from row-group zonemaps without
//       reading the column, which would make the full-scan control falsely flat.
//
// Not production code. A measurement instrument.

#include "duckdb.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <dlfcn.h>

static int run(duckdb_connection con, const char *sql) {
    duckdb_result r;
    if (duckdb_query(con, sql, &r) != DuckDBSuccess) {
        fprintf(stderr, "FAIL %s: %s\n", sql, duckdb_result_error(&r));
        duckdb_destroy_result(&r);
        return 2;
    }
    duckdb_destroy_result(&r);
    return 0;
}

static int do_seed(const char *path, long cold_rows) {
    duckdb_database db;
    duckdb_connection con;
    if (duckdb_open(path, &db) != DuckDBSuccess) {
        fprintf(stderr, "open failed\n");
        return 2;
    }
    duckdb_connect(db, &con);

    // Per-row-unique, effectively incompressible values (hash + md5), so the tables occupy real,
    // distinct blocks rather than collapsing into the catalog under compression — otherwise the
    // experiment measures compression, not read locality.
    if (run(con, "CREATE TABLE hot (id BIGINT, v BIGINT, w VARCHAR)")) return 2;
    if (run(con,
            "INSERT INTO hot SELECT i, (hash(i*2+1) >> 1)::BIGINT, md5(i::VARCHAR || 'hot') "
            "FROM range(1000) t(i)"))
        return 2;

    if (run(con, "CREATE TABLE cold (id BIGINT, v BIGINT, w VARCHAR)")) return 2;
    char sql[256];
    snprintf(sql, sizeof(sql),
             "INSERT INTO cold SELECT i, (hash(i) >> 1)::BIGINT, md5(i::VARCHAR || 'cold') "
             "FROM range(%ld) t(i)",
             cold_rows);
    if (cold_rows > 0 && run(con, sql)) return 2;

    if (run(con, "CHECKPOINT")) return 2;
    duckdb_disconnect(&con);
    duckdb_close(&db);
    return 0;
}

typedef int (*int_fn)(void);
typedef long (*long_fn)(void);

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long)ts.tv_sec * 1000000L + ts.tv_nsec / 1000L;
}

static int do_wake(const char *path, const char *which, long cold_rows) {
    // The page-faulting read path, if the shim is loaded.
    int_fn boot = (int_fn)dlsym(RTLD_DEFAULT, "flock_fault_boot");
    int_fn wake = (int_fn)dlsym(RTLD_DEFAULT, "flock_fault_wake");
    long_fn faulted = (long_fn)dlsym(RTLD_DEFAULT, "flock_fault_pages_faulted");
    long_fn serve_calls = (long_fn)dlsym(RTLD_DEFAULT, "flock_fault_serve_calls");
    int have_shim = (boot != NULL && wake != NULL);

    // Boot is the one-time process cost a production runtime would already have paid: it is NOT in
    // the timed region.
    if (have_shim && boot() != 0) {
        fprintf(stderr, "flock_fault_boot failed\n");
        return 3;
    }

    const char *sql;
    char buf[160];
    if (strcmp(which, "open") == 0) {
        sql = "SELECT 1";
    } else if (strcmp(which, "hot") == 0) {
        sql = "SELECT sum(v % 1000)::BIGINT FROM hot";
    } else if (strcmp(which, "cold") == 0) {
        sql = "SELECT sum(v % 1000)::BIGINT FROM cold";
    } else if (strcmp(which, "point") == 0) {
        snprintf(buf, sizeof(buf),
                 "SELECT sum(v %% 1000)::BIGINT FROM cold WHERE id = %ld", cold_rows / 2);
        sql = buf;
    } else {
        fprintf(stderr, "unknown query %s\n", which);
        return 2;
    }

    // ── the timed region: wake → first row ──────────────────────────────────────────────────────
    long t0 = now_us();
    if (have_shim && wake() != 0) {
        fprintf(stderr, "flock_fault_wake failed\n");
        return 3;
    }

    duckdb_config config;
    if (duckdb_create_config(&config) != DuckDBSuccess) {
        fprintf(stderr, "create_config failed\n");
        return 2;
    }
    duckdb_set_config(config, "access_mode", "READ_ONLY");
    duckdb_set_config(config, "threads", "1");

    duckdb_database db;
    char *err = NULL;
    if (duckdb_open_ext(path, &db, config, &err) != DuckDBSuccess) {
        fprintf(stderr, "open failed: %s\n", err ? err : "?");
        duckdb_free(err);
        duckdb_destroy_config(&config);
        return 2;
    }
    duckdb_destroy_config(&config);

    duckdb_connection con;
    duckdb_connect(db, &con);

    duckdb_result r;
    if (duckdb_query(con, sql, &r) != DuckDBSuccess) {
        fprintf(stderr, "query failed: %s\n", duckdb_result_error(&r));
        duckdb_destroy_result(&r);
        return 2;
    }
    long long value = 0;
    if (duckdb_row_count(&r) > 0 && duckdb_column_count(&r) > 0)
        value = (long long)duckdb_value_int64(&r, 0, 0);
    long t1 = now_us();
    // ────────────────────────────────────────────────────────────────────────────────────────────

    duckdb_destroy_result(&r);
    duckdb_disconnect(&con);
    duckdb_close(&db);

    printf("RESULT %lld\n", value);
    printf("LATENCY_US %ld\n", t1 - t0);
    if (have_shim) {
        long pages = faulted ? faulted() : -1;
        long calls = serve_calls ? serve_calls() : -1;
        printf("FAULT_PAGES %ld\n", pages);
        printf("FAULT_SERVE_CALLS %ld\n", calls);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 4 && strcmp(argv[1], "seed") == 0)
        return do_seed(argv[2], strtol(argv[3], NULL, 10));
    if (argc >= 4 && strcmp(argv[1], "wake") == 0) {
        long cold_rows = argc >= 5 ? strtol(argv[4], NULL, 10) : 0;
        return do_wake(argv[2], argv[3], cold_rows);
    }
    fprintf(stderr, "usage:\n  %s seed <path> <cold_rows>\n  %s wake <path> <which> <cold_rows>\n",
            argv[0], argv[0]);
    return 1;
}
