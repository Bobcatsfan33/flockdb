#!/usr/bin/env bash
# run.sh — measure wake → first-QUERY-RESULT through the LOADED C++ FileSystem extension, and show it is
# FLAT across database size for the wake-one-of-many case (a point query on a small table in a large
# database), against an eager-hydration control that is O(database).
#
# This is spikes/wake-latency's successor: that spike stopped at wake → first *page read* (through a dyld
# interpose shim); this one goes all the way to wake → first *query result*, with DuckDB opening the
# database THROUGH the real FlockFileSystem extension and faulting pages from substrate's tier on demand.
#
# It builds heavy artifacts (the bundled DuckDB static lib), so it is meant to run on a CI runner with
# disk headroom (.github/workflows/wake-query.yml), NOT the dev Mac. See README.md.
#
# Tier: LocalFileSystem (the zero-network FLOOR) always; and additionally, if FLOCK_VFS_S3_URL is
# exported (the workflow points it at same-runner MinIO), a second pass faulting from that S3 endpoint —
# a real-object-storage, low-latency-endpoint number, stated as such. The two are NEVER conflated.
#
# No 250 ms number is produced or quoted. The result is the SHAPE: flat point-query wake→query vs a
# scaling full-scan and a scaling eager control.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

ROOT="$(cd ../.. && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "== building the flock-vfs cdylib (release, with s3) and the Rust seeder =="
# The cdylib carries the fuzzed read boundary AND, with --features s3, the S3 tier the MinIO pass uses.
( cd "$ROOT" && cargo build --release -p flock-vfs --features s3 )
cargo build --release --features s3            # the seeder (this spike's workspace)

CDYLIB_DIR="$ROOT/target/release"
if [ -f "$CDYLIB_DIR/libflock_vfs.so" ]; then
  CDYLIB="$CDYLIB_DIR/libflock_vfs.so"
elif [ -f "$CDYLIB_DIR/libflock_vfs.dylib" ]; then
  CDYLIB="$CDYLIB_DIR/libflock_vfs.dylib"
else
  echo "no libflock_vfs cdylib in $CDYLIB_DIR" >&2; exit 1
fi
SEED="$PWD/target/release/seed"
echo "cdylib : $CDYLIB"
echo "seed   : $SEED"

echo "== building the bundled DuckDB static lib (release; this is the heavy step) =="
# Any crate that depends on `duckdb` (bundled) triggers the libduckdb-sys build. flock-kernel is the
# smallest. In release it is a fraction of the debug footprint (root Cargo.toml explains why).
( cd "$ROOT" && cargo build --release -p flock-kernel )
LIB="$(ls -1 "$ROOT"/target/release/build/libduckdb-sys-*/out/libduckdb.a 2>/dev/null | head -1)"
[ -n "$LIB" ] || { echo "libduckdb.a not found under target/release/build" >&2; exit 1; }
HDR="$(dirname "$LIB")/duckdb/src/include"
[ -d "$HDR" ] || { echo "DuckDB headers not found at $HDR" >&2; exit 1; }
echo "libduckdb.a : $LIB"
echo "headers     : $HDR"

echo "== compiling the measurement harness (links libduckdb.a whole + flock-vfs) =="
# -rdynamic + --whole-archive so the DuckDB symbols the dlopened .duckdb_extension needs are present and
# exported from this process at LOAD time. Register mode does not need that, but one binary serves both.
"${CXX:-g++}" -std=c++17 -O2 \
  -I"$HDR" -I"$ROOT/extension/flock-vfs/include" -I"$ROOT/extension/flock-vfs/src" \
  "$PWD/measure.cpp" "$ROOT/extension/flock-vfs/src/flock_file_system.cpp" \
  -Wl,--whole-archive "$LIB" -Wl,--no-whole-archive \
  -L"$CDYLIB_DIR" -lflock_vfs -Wl,-rpath,"$CDYLIB_DIR" \
  -rdynamic -ldl -lpthread -lm \
  -o "$WORK/measure"
MEASURE="$WORK/measure"
DUCKDB_VERSION="$("$MEASURE" version)"
echo "DuckDB version (for the extension footer): $DUCKDB_VERSION"

echo "== building the loadable .duckdb_extension (hidden visibility, DuckDB symbols undefined) =="
# No libduckdb linked: its symbols resolve from the harness process at dlopen. flock-vfs IS linked so the
# extension and harness share one cdylib instance (one tier state).
EXT="$WORK/flock_vfs.duckdb_extension"
"${CXX:-g++}" -std=c++17 -O2 -fPIC -fvisibility=hidden -shared \
  -I"$HDR" -I"$ROOT/extension/flock-vfs/include" \
  "$ROOT/extension/flock-vfs/src/flock_vfs_extension.cpp" \
  "$ROOT/extension/flock-vfs/src/flock_file_system.cpp" \
  -L"$CDYLIB_DIR" -lflock_vfs -Wl,-rpath,"$CDYLIB_DIR" \
  -o "$EXT" && echo "extension built: $EXT" || { echo "extension link FAILED (non-fatal)"; EXT=""; }

# Best-effort: append the per-platform metadata footer so `LOAD '<file>'` accepts it. Prefer a script
# bundled with the exact DuckDB source (guaranteed-matching footer format); else fetch extension-ci-tools.
EXT_LOADABLE=0
if [ -n "$EXT" ]; then
  SCRIPT="$(find "$(dirname "$LIB")/duckdb" -name append_extension_metadata.py 2>/dev/null | head -1 || true)"
  if [ -z "$SCRIPT" ]; then
    curl -fsSL https://raw.githubusercontent.com/duckdb/extension-ci-tools/main/scripts/append_extension_metadata.py \
      -o "$WORK/append_extension_metadata.py" 2>/dev/null && SCRIPT="$WORK/append_extension_metadata.py" || true
  fi
  PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]')_$(uname -m)"
  case "$PLATFORM" in linux_x86_64) PLATFORM=linux_amd64;; linux_aarch64) PLATFORM=linux_arm64;; esac
  if [ -n "$SCRIPT" ]; then
    # Flag spellings have drifted across versions; try the common ones. Non-fatal on failure.
    if python3 "$SCRIPT" -l "$EXT" -o "$WORK/flock_vfs.signed.duckdb_extension" \
         -n flock_vfs -dv "$DUCKDB_VERSION" -ev 0.1.0 -pf "$PLATFORM" >/dev/null 2>&1; then
      EXT="$WORK/flock_vfs.signed.duckdb_extension"; EXT_LOADABLE=1
    elif python3 "$SCRIPT" --library "$EXT" --out "$WORK/flock_vfs.signed.duckdb_extension" \
         --name flock_vfs --duckdb-version "$DUCKDB_VERSION" --extension-version 0.1.0 --platform "$PLATFORM" >/dev/null 2>&1; then
      EXT="$WORK/flock_vfs.signed.duckdb_extension"; EXT_LOADABLE=1
    else
      echo "footer append FAILED with both flag conventions (non-fatal): LOAD path not validated"
    fi
  else
    echo "no append_extension_metadata.py available (non-fatal): LOAD path not validated"
  fi
fi

SIZES=(30000 300000 1200000 2400000)
QUERIES=(open hot point cold)

extract() { echo "$2" | awk -v k="$1" '$1==k{print $2; exit}'; }

# One LOAD-path correctness proof, done once (the register pass carries the numbers): actually LOAD the
# packaged .duckdb_extension and check it serves a correct row.
load_check() {
  local uri="$1" cr="$2" exp="$3"
  if [ "$EXT_LOADABLE" != "1" ]; then
    echo "LOAD check SKIPPED: no metadata-footer'd extension to LOAD (register-mode load succeeded instead)"
    return 0
  fi
  local out res
  if out="$("$MEASURE" load point "$cr" "$EXT" "$uri" 2>&1)"; then
    res="$(extract RESULT "$out")"
    if [ "$res" = "$exp" ]; then
      echo "LOAD check PASSED: LOAD '$(basename "$EXT")' served a correct point-query row ($res)"
    else
      echo "LOAD check WRONG ROW: got '$res' expected '$exp'"; echo "$out"; exit 1
    fi
  else
    echo "LOAD check could not LOAD the packaged extension (non-fatal):"; echo "$out"
  fi
}

run_matrix() {
  local tier="$1"   # "local" (floor) or "s3" (MinIO)
  echo
  echo "################################################################################"
  echo "# TIER = $tier   (wake → first QUERY RESULT, through the loaded FlockFileSystem)"
  echo "################################################################################"
  printf '%-9s %-8s %-7s %12s %10s %10s\n' cold_rows file_MB query latency_us faults result
  echo "--------------------------------------------------------------------------------"

  local did_load_check=0
  for cr in "${SIZES[@]}"; do
    local REAL="$WORK/real_${cr}.duckdb"
    local REMOTE="$WORK/remote_${cr}_${tier}"
    local SEEDCACHE="$WORK/seedcache_${cr}_${tier}"
    local POOL="acme"
    rm -f "$REAL" "$REAL".wal
    "$MEASURE" seed "$REAL" "$cr"
    local fbytes fmb
    fbytes=$(stat -c%s "$REAL" 2>/dev/null || stat -f%z "$REAL")
    fmb=$(awk -v b="$fbytes" 'BEGIN{printf "%.1f", b/1048576}')

    # Sleep to the tier. For the s3 pass, FLOCK_VFS_S3_URL is exported by the caller so both seed and
    # wake pick the S3 backend; for local it is unset so both pick LocalFileSystem.
    local META MANIFEST PAGE_SIZE TOTAL_LEN
    META="$("$SEED" "$REAL" "$REMOTE" "$SEEDCACHE" "$POOL")"
    MANIFEST=$(echo "$META" | awk -F= '/^MANIFEST=/{print $2}')
    PAGE_SIZE=$(echo "$META" | awk -F= '/^PAGE_SIZE=/{print $2}')
    TOTAL_LEN=$(echo "$META" | awk -F= '/^TOTAL_LEN=/{print $2}')
    rm -rf "$SEEDCACHE"

    for q in "${QUERIES[@]}"; do
      local exp
      exp="$(extract RESULT "$("$MEASURE" expected "$q" "$cr" "$REAL")")"

      # Fresh wake cache each run so faults are genuinely cold. Min of 3 for the latency floor; the fault
      # count is deterministic. The flat-vs-scaling SHAPE is the result, not any single cell.
      local min_lat="" faults="" res=""
      local WAKECACHE="$WORK/wakecache_${cr}_${q}_${tier}"
      local URI="flock://${POOL}/${MANIFEST}?remote=${REMOTE}&cache=${WAKECACHE}&page_size=${PAGE_SIZE}&len=${TOTAL_LEN}"
      for _ in 1 2 3; do
        rm -rf "$WAKECACHE"
        local out lat
        out="$("$MEASURE" register "$q" "$cr" "$URI")"
        res="$(extract RESULT "$out")"
        lat="$(extract LATENCY_US "$out")"
        faults="$(extract FAULT_MISSES "$out")"
        if [ "$res" != "$exp" ]; then
          echo "CORRECTNESS FAIL: tier=$tier cr=$cr q=$q got '$res' expected '$exp'"; echo "$out"; exit 1
        fi
        if [ -z "$min_lat" ] || [ "$lat" -lt "$min_lat" ]; then min_lat="$lat"; fi
      done
      rm -rf "$WAKECACHE"
      printf '%-9s %-8s %-7s %12s %10s %10s\n' "$cr" "$fmb" "$q" "$min_lat" "$faults" "$res"

      # Do the packaged-LOAD correctness proof once (first query of the first size).
      if [ "$did_load_check" = "0" ]; then load_check "$URI" "$cr" "$exp"; did_load_check=1; fi
    done

    # The control today's FlockDB pays: eager hydration then query. O(database).
    local EAGERCACHE="$WORK/eagercache_${cr}_${tier}"
    rm -rf "$EAGERCACHE"
    local eout elat efaults eres eexp
    eexp="$(extract RESULT "$("$MEASURE" expected point "$cr" "$REAL")")"
    eout="$("$MEASURE" eager point "$cr" "$REMOTE" "$EAGERCACHE" "$POOL" "$MANIFEST" "$PAGE_SIZE" "$TOTAL_LEN")"
    eres="$(extract RESULT "$eout")"; elat="$(extract LATENCY_US "$eout")"; efaults="$(extract FAULT_MISSES "$eout")"
    if [ "$eres" != "$eexp" ]; then echo "EAGER CORRECTNESS FAIL cr=$cr: got '$eres' exp '$eexp'"; echo "$eout"; exit 1; fi
    printf '%-9s %-8s %-7s %12s %10s %10s\n' "$cr" "$fmb" "point/EAGER" "$elat" "$efaults" "$eres"
    rm -rf "$EAGERCACHE" "$REMOTE"
    rm -f "$REAL" "$REAL".wal
    echo "--------------------------------------------------------------------------------"
  done
  echo "All faulted results matched stock-DuckDB ground truth for tier=$tier."
}

# Pass 1: the zero-network LocalFileSystem floor. Force the local backend for the children.
( unset FLOCK_VFS_S3_URL; run_matrix local )

# Pass 2: same-runner MinIO, if configured. A real-object-storage, low-latency-endpoint number.
if [ -n "${FLOCK_VFS_S3_URL:-}" ]; then
  run_matrix s3
else
  echo
  echo "S3/MinIO pass SKIPPED: FLOCK_VFS_S3_URL not set. Only the LocalFileSystem floor was measured."
fi

echo
echo "Reminder (docs/wake-latency.md): no 250 ms number is quoted. The result is the SHAPE — a flat"
echo "point-query wake→query vs a scaling full-scan and a scaling eager control."
