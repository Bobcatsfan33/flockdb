# flock-vfs DuckDB extension — build recipe.
#
# This builds the C++ FileSystem subclass (src/) into a DuckDB loadable extension that calls the
# `flock-vfs` Rust cdylib (crates/flock-vfs) over its C ABI. It is a STANDALONE recipe: it does not
# vendor the DuckDB build system. A production, signed, per-platform build should instead use DuckDB's
# official extension template + extension-ci-tools (see README.md "Production pipeline"); this recipe
# exists so the shim can be compiled and linked against a DuckDB the operator points it at.
#
# Usage:
#   cargo build -p flock-vfs --release                 # builds libflock_vfs.{dylib,so}
#   cmake -B build -DDUCKDB_INCLUDE_DIR=<duckdb/src/include> \
#                  -DDUCKDB_LIB=<path/to/libduckdb.a-or-.dylib> \
#                  -DFLOCK_VFS_LIB=<repo>/target/release/libflock_vfs.dylib
#   cmake --build build
# then append the metadata footer and load (README.md).

cmake_minimum_required(VERSION 3.16)
project(flock_vfs_extension CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Where DuckDB's C++ headers live. For the `bundled` duckdb-rs build these ship inside the crate's
# duckdb.tar.gz at duckdb/src/include (extract it once). For a source checkout, it is duckdb/src/include.
if(NOT DEFINED DUCKDB_INCLUDE_DIR)
  message(FATAL_ERROR "Set -DDUCKDB_INCLUDE_DIR to DuckDB 1.10504's src/include directory.")
endif()

# The DuckDB library to link the extension against (the full engine — static .a or shared .dylib/.so).
if(NOT DEFINED DUCKDB_LIB)
  message(FATAL_ERROR "Set -DDUCKDB_LIB to the DuckDB library (e.g. the bundled libduckdb.a).")
endif()

# The flock-vfs Rust cdylib (crates/flock-vfs). Its four flock_vfs_* symbols are the fuzzed boundary.
if(NOT DEFINED FLOCK_VFS_LIB)
  message(FATAL_ERROR "Set -DFLOCK_VFS_LIB to the built libflock_vfs.{dylib,so} from `cargo build -p flock-vfs --release`.")
endif()

# DuckDB names loadable extensions `<name>.duckdb_extension`; the entry symbols must be
# flock_vfs_init / flock_vfs_version, matching this stem.
add_library(flock_vfs SHARED
  src/flock_vfs_extension.cpp
  src/flock_file_system.cpp
)
set_target_properties(flock_vfs PROPERTIES
  PREFIX ""
  SUFFIX ".duckdb_extension"
  CXX_VISIBILITY_PRESET hidden
)
target_include_directories(flock_vfs PRIVATE
  ${DUCKDB_INCLUDE_DIR}
  ${CMAKE_CURRENT_SOURCE_DIR}/include
)
target_link_libraries(flock_vfs PRIVATE ${DUCKDB_LIB} ${FLOCK_VFS_LIB})

# On macOS an extension resolves DuckDB's symbols from the host process at load time; allow undefined.
if(APPLE)
  target_link_options(flock_vfs PRIVATE -undefined dynamic_lookup)
endif()
