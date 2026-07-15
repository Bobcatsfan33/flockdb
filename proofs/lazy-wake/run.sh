#!/bin/bash
# run.sh — build the interpose shim + harness against the crate's own libduckdb.a,
# then measure DuckDB's read pattern for a selective query as the database grows.
#
# The result answers docs/wake-latency.md's open question: does DuckDB read the
# whole file to answer a query on part of it (=> no VFS helps, reprice the pitch),
# or only the touched ranges (=> a page-faulting VFS would deliver a lazy wake)?
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../.. && pwd)"
LIB="$(ls -1 "$ROOT"/target/release/build/libduckdb-sys-*/out/libduckdb.a | head -1)"
HDR_DIR="${DUCKDB_HDR_DIR:-/private/tmp/claude-501/-Users-rwallace/86c70964-2c56-4ca2-963d-b243f884863a/scratchpad/duckhdr/duckdb/src/include}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "libduckdb.a: $LIB"
echo "header dir : $HDR_DIR"
echo "workdir    : $WORK"

clang -O2 -dynamiclib readtrace.c -o "$WORK/readtrace.dylib"
# libduckdb.a is C++; link with clang++ and the C++ runtime.
clang++ -O2 -I"$HDR_DIR" harness.c "$LIB" -o "$WORK/harness" -lc++

echo
echo "cold_rows  file_bytes  q  uniq_touched  raw_read  ncalls  pct_of_file"
for cr in 0 100000 400000 1000000; do
  DB="$WORK/flock_probe.duckdb"
  rm -f "$DB" "$DB".wal
  "$WORK/harness" seed "$DB" "$cr"
  fsz=$(stat -f%z "$DB")
  for q in open hot point cold; do
    TR="$WORK/trace_${cr}_${q}.log"
    DYLD_INSERT_LIBRARIES="$WORK/readtrace.dylib" \
      READTRACE_MATCH=flock_probe READTRACE_OUT="$TR" \
      "$WORK/harness" query "$DB" "$q" "$cr"
    line=$(python3 analyze.py "$TR" "$fsz")
    printf "%-10s %s\t%s\n" "$cr" "$q" "$line"
  done
  rm -f "$DB" "$DB".wal
  echo
done
