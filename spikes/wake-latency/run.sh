#!/bin/bash
# run.sh — measure end-to-end wake→first-query latency for the wake-one-of-many case, and show it
# is FLAT across database size (O(pages touched)) versus an eager-hydration control that is
# O(database). This turns the F4 fault-SET proof (proofs/lazy-wake/) into a wall-clock LATENCY.
#
# For each database size and each query it runs three times:
#   expected : duckharness on the REAL file, no shim  — the ground-truth result AND a local-disk
#              open baseline (network + fault path both absent).
#   lazy     : duckharness on a SPARSE file, faultshim in LAZY mode — the candidate fix: wake fetches
#              the manifest, then only the pages the query touches fault from the (local) tier.
#   eager    : same, faultshim in EAGER mode — models today's paging::hydrate: prefetch every page.
#
# The tier is substrate's LocalFileSystem object store: a REAL object-store code path with the
# NETWORK AT ZERO. Every latency here is therefore a FLOOR — the honest lower bound with no S3
# round-trips — NOT a real-object-storage number, and emphatically NOT the 250 ms target. See
# docs/wake-latency.md.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

ROOT="$(cd ../.. && pwd)"
LIB="$(ls -1 "$ROOT"/target/release/build/libduckdb-sys-*/out/libduckdb.a | head -1)"
HDR_DIR="$(dirname "$LIB")/duckdb/src/include"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "libduckdb.a : $LIB"
echo "header dir  : $HDR_DIR"
echo "workdir     : $WORK"
echo

echo "building faultshim (cdylib + interpose glue) and seed, release ..."
cargo build --release -p faultshim -p seed
SHIM="$PWD/target/release/libfaultshim.dylib"
SEED="$PWD/target/release/seed"
[ -f "$SHIM" ] || { echo "no shim dylib at $SHIM"; exit 1; }

echo "compiling duckharness against libduckdb.a ..."
clang++ -O2 -I"$HDR_DIR" duckharness.c "$LIB" -o "$WORK/duckharness" -lc++

# cold_rows tuned to file sizes (~26–32 bytes/row on disk, per F4). Small on purpose: disk is tight.
SIZES=(30000 300000 1200000 2400000)
QUERIES=(open hot point cold)

extract() { echo "$2" | awk -v k="$1" '$1==k{print $2; exit}'; }

printf '%-9s %-9s %-6s %14s %10s %12s %10s\n' \
    cold_rows file_MB query latency_us fault_pgs faults_KiB result
echo "--------------------------------------------------------------------------------"

for cr in "${SIZES[@]}"; do
  REAL="$WORK/real_${cr}.duckdb"
  SPARSE="$WORK/flock_sparse_${cr}.duckdb"      # name contains flock_sparse => the glue tracks it
  REMOTE="$WORK/remote_${cr}"                    # the local object-store tier (the "bucket")
  SEEDCACHE="$WORK/seedcache_${cr}"
  POOL="acme"

  rm -f "$REAL" "$REAL".wal
  "$WORK/duckharness" seed "$REAL" "$cr"
  fbytes=$(stat -f%z "$REAL")
  fmb=$(awk -v b="$fbytes" 'BEGIN{printf "%.1f", b/1048576}')

  META="$("$SEED" "$REAL" "$REMOTE" "$SEEDCACHE" "$POOL")"
  MANIFEST=$(echo "$META" | awk -F= '/^MANIFEST=/{print $2}')
  PAGE_SIZE=$(echo "$META" | awk -F= '/^PAGE_SIZE=/{print $2}')
  TOTAL_LEN=$(echo "$META" | awk -F= '/^TOTAL_LEN=/{print $2}')
  # Seed cache is emptied by drop_local at sleep; remove the dir shell too.
  rm -rf "$SEEDCACHE"

  for q in "${QUERIES[@]}"; do
    # expected: real file, no shim.
    exp_out="$("$WORK/duckharness" wake "$REAL" "$q" "$cr")"
    exp_res=$(extract RESULT "$exp_out")

    for mode in lazy eager; do
      # Best-of-3: the fault count is deterministic, the latency is not — a cold process is subject
      # to scheduler/allocator jitter, so the MIN of a few runs is the honest floor (the standard
      # way to report a latency floor). The flat-vs-scaling SHAPE is the result, not any one cell.
      min_lat=""
      pgs=""
      for _ in 1 2 3; do
        CACHE="$WORK/wakecache_${cr}_${q}_${mode}"
        rm -rf "$CACHE" "$SPARSE"
        out="$(DYLD_INSERT_LIBRARIES="$SHIM" \
               FLOCK_REMOTE_DIR="$REMOTE" FLOCK_CACHE_DIR="$CACHE" FLOCK_POOL="$POOL" \
               FLOCK_MANIFEST="$MANIFEST" FLOCK_PAGE_SIZE="$PAGE_SIZE" FLOCK_TOTAL_LEN="$TOTAL_LEN" \
               FLOCK_DB_PATH="$SPARSE" FLOCK_DB_MATCH="flock_sparse" FLOCK_FAULT_MODE="$mode" \
               "$WORK/duckharness" wake "$SPARSE" "$q" "$cr")"
        res=$(extract RESULT "$out")
        lat=$(extract LATENCY_US "$out")
        pgs=$(extract FAULT_PAGES "$out")
        if [ "$res" != "$exp_res" ]; then
          echo "CORRECTNESS FAIL: cr=$cr q=$q mode=$mode got '$res' expected '$exp_res'"
          echo "$out"
          exit 1
        fi
        if [ -z "$min_lat" ] || [ "$lat" -lt "$min_lat" ]; then min_lat="$lat"; fi
        rm -rf "$CACHE"
      done
      kib=$(awk -v p="$pgs" -v ps="$PAGE_SIZE" 'BEGIN{printf "%.0f", p*ps/1024}')
      printf '%-9s %-9s %-6s %14s %10s %12s %10s\n' \
          "$cr" "$fmb" "$q/$mode" "$min_lat" "$pgs" "$kib" "$res"
    done
  done
  rm -f "$REAL" "$REAL".wal "$SPARSE"
  rm -rf "$REMOTE"
  echo "--------------------------------------------------------------------------------"
done
echo "all results matched the ground truth (lazy and eager both returned the correct value)."
