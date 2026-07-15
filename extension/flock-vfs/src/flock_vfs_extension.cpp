//===----------------------------------------------------------------------===//
// flock_vfs_extension.cpp — the DuckDB loadable-extension entry point.
//
// This is the whole reason the read path is a C++ extension and not a C one: the single call
// `RegisterSubSystem` below exists only in DuckDB's C++ core (`VirtualFileSystem::RegisterSubSystem`,
// the same seam httpfs uses for s3://). The stock C API and the C loadable-extension API expose no
// equivalent — F4 verified this against DuckDB 1.10504 (flock-kernel/src/lib.rs) — so a page-faulting
// FileSystem can only be installed from C++. LOADING this into stock DuckDB additionally requires
// `SET allow_unsigned_extensions=true` (or a signed build) and a per-platform metadata footer; see
// README.md.
//===----------------------------------------------------------------------===//
#define DUCKDB_EXTENSION_MAIN

#include "duckdb.hpp"
#include "duckdb/common/helper.hpp"

#include "flock_file_system.hpp"

namespace duckdb {

//! Install the substrate-backed FileSystem on the database instance's VirtualFileSystem. After this,
//! any `flock://` path DuckDB opens (e.g. `ATTACH 'flock://pool/manifest?...' AS db (READ_ONLY)`) is
//! served page-by-page from substrate, through the fuzzed flock-vfs Rust boundary.
static void LoadInternal(DatabaseInstance &db) {
	auto &fs = db.GetFileSystem();
	fs.RegisterSubSystem(make_uniq<FlockFileSystem>());
}

} // namespace duckdb

extern "C" {

//! DuckDB calls `<name>_init` after loading the extension. The name must match the loaded file stem
//! (`flock_vfs`).
DUCKDB_EXTENSION_API void flock_vfs_init(duckdb::DatabaseInstance &db) {
	duckdb::LoadInternal(db);
}

//! The DuckDB version this extension was built against. The loader refuses to load an extension whose
//! version string does not match the running DuckDB, which is why it is derived, not hard-coded.
DUCKDB_EXTENSION_API const char *flock_vfs_version() {
	return duckdb::DuckDB::LibraryVersion();
}
}
