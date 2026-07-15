// harness — seeds a DuckDB database, then (in a separate, cold invocation) runs
// one query, so readtrace.dylib can record exactly which byte ranges of the file
// DuckDB reads to answer it.
//
// The experiment this serves: put a small "hot" table and a large "cold" table in
// ONE database file, grow the cold table across runs, and query only the hot
// table. If DuckDB reads a flat number of bytes as the file grows, then waking one
// thing inside a large sleeping database is O(thing) at the engine level, and the
// O(database) wake FlockDB has today is purely an artefact of eager hydration —
// which a page-faulting VFS would remove. If instead DuckDB reads the whole file,
// no VFS helps and the "<250 ms wake" pitch needs repricing. See docs/wake-latency.md.
//
// Modes:
//   harness seed  <path> <cold_rows>   create hot(1000 rows) + cold(cold_rows), CHECKPOINT
//   harness query <path> <which>       open cold, run one query; <which> in:
//                                         hot   -> SELECT sum(v) FROM hot         (selective)
//                                         cold  -> SELECT sum(v) FROM cold        (full scan, control)
//                                         open  -> SELECT 1                       (pure wake, no table)
//                                         point -> SELECT sum(v) FROM cold WHERE id = <mid>  (point lookup)
//
// Not production code. A measurement instrument that links the same libduckdb.a
// the crate builds, so the read pattern is DuckDB's real one, not a model of it.

#include "duckdb.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int die(duckdb_result *r, const char *what) {
    fprintf(stderr, "FAIL %s: %s\n", what, r ? duckdb_result_error(r) : "?");
    return 2;
}

static int run(duckdb_connection con, const char *sql) {
    duckdb_result r;
    if (duckdb_query(con, sql, &r) != DuckDBSuccess) {
        int rc = die(&r, sql);
        duckdb_destroy_result(&r);
        return rc;
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

    // A hot table of fixed, small size. Values are per-row unique and effectively
    // incompressible (hash + md5), so the table occupies real, distinct blocks
    // rather than collapsing into the catalog under RLE/dictionary compression —
    // otherwise the whole experiment measures compression, not read locality.
    if (run(con, "CREATE TABLE hot (id BIGINT, v BIGINT, w VARCHAR)")) return 2;
    if (run(con,
            "INSERT INTO hot SELECT i, (hash(i*2+1) >> 1)::BIGINT, md5(i::VARCHAR || 'hot') "
            "FROM range(1000) t(i)"))
        return 2;

    // A cold table whose size we vary run to run — this is the bulk of the file.
    // Same incompressible shape, so growing cold_rows genuinely grows the file.
    if (run(con, "CREATE TABLE cold (id BIGINT, v BIGINT, w VARCHAR)")) return 2;
    char sql[256];
    snprintf(sql, sizeof(sql),
             "INSERT INTO cold SELECT i, (hash(i) >> 1)::BIGINT, md5(i::VARCHAR || 'cold') "
             "FROM range(%ld) t(i)",
             cold_rows);
    if (cold_rows > 0 && run(con, sql)) return 2;

    // Fold the WAL into the main file, exactly as flock-kernel's checkpoint does,
    // so the file we trace is the whole database with no sidecar.
    if (run(con, "CHECKPOINT")) return 2;

    duckdb_disconnect(&con);
    duckdb_close(&db);
    return 0;
}

static int do_query(const char *path, const char *which, long cold_rows) {
    duckdb_database db;
    duckdb_connection con;
    // A fresh open => DuckDB's buffer manager is empty => this is a cold wake.
    if (duckdb_open(path, &db) != DuckDBSuccess) {
        fprintf(stderr, "open failed\n");
        return 2;
    }
    duckdb_connect(db, &con);

    const char *sql;
    char buf[128];
    if (strcmp(which, "hot") == 0) {
        sql = "SELECT sum(v) FROM hot";
    } else if (strcmp(which, "cold") == 0) {
        sql = "SELECT sum(v) FROM cold";
    } else if (strcmp(which, "open") == 0) {
        sql = "SELECT 1";
    } else if (strcmp(which, "point") == 0) {
        snprintf(buf, sizeof(buf), "SELECT sum(v) FROM cold WHERE id = %ld", cold_rows / 2);
        sql = buf;
    } else {
        fprintf(stderr, "unknown query %s\n", which);
        return 2;
    }

    duckdb_result r;
    if (duckdb_query(con, sql, &r) != DuckDBSuccess) {
        int rc = die(&r, sql);
        duckdb_destroy_result(&r);
        return rc;
    }
    // Force materialisation of at least one value so the query is not optimised
    // away before it touches storage.
    if (duckdb_row_count(&r) > 0 && duckdb_column_count(&r) > 0)
        (void)duckdb_value_int64(&r, 0, 0);
    duckdb_destroy_result(&r);

    duckdb_disconnect(&con);
    duckdb_close(&db);
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s seed|query <path> [arg]\n", argv[0]);
        return 1;
    }
    if (strcmp(argv[1], "seed") == 0 && argc == 4)
        return do_seed(argv[2], strtol(argv[3], NULL, 10));
    if (strcmp(argv[1], "query") == 0 && argc >= 4) {
        long cold_rows = argc >= 5 ? strtol(argv[4], NULL, 10) : 0;
        return do_query(argv[2], argv[3], cold_rows);
    }
    fprintf(stderr, "bad args\n");
    return 1;
}
