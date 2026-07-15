//===----------------------------------------------------------------------===//
// flock_file_system.hpp — a DuckDB FileSystem subclass backed by substrate page faults.
//
// This is F4 step 2: the production mechanism RISK-1 (docs/wake-latency.md) settled on. The stock
// DuckDB C API has NO filesystem-registration hook (verified against 1.10504; see
// flock-kernel/src/lib.rs), so a page-faulting read path must be a C++ loadable extension that calls
// VirtualFileSystem::RegisterSubSystem with a FileSystem subclass. That subclass is this.
//
// It intercepts paths of the form:
//
//     flock://<pool>/<manifest_hex>?remote=<dir>&cache=<dir>&page_size=<n>&len=<n>
//
// On OpenFile it wakes the database via the fuzzed flock-vfs Rust cdylib (flock_vfs_open) and returns a
// handle whose every Read is forwarded to flock_vfs_pread — the exact boundary the F5 spike measured and
// the proptest/libFuzzer harness hardened. It is READ-ONLY: a woken database is served, never mutated.
//===----------------------------------------------------------------------===//
#pragma once

#include "duckdb/common/file_system.hpp"

#include "flock_vfs_ffi.h"

namespace duckdb {

//! A handle to one woken database file. Owns the Rust-side FlockVfs and tracks the sequential read
//! position (DuckDB uses both positional and sequential reads).
class FlockFileHandle : public FileHandle {
public:
	FlockFileHandle(FileSystem &fs, string path, FileOpenFlags flags, FlockVfs *vfs, int64_t size);
	~FlockFileHandle() override;

	//! FileHandle's one pure-virtual. Frees the Rust handle.
	void Close() override;

	FlockVfs *vfs;      //! The woken database, owned by this handle.
	idx_t position;     //! Sequential-read cursor.
	int64_t file_size;  //! Cached total length (bytes).
};

//! The page-faulting filesystem. Registered on the DuckDB instance's VirtualFileSystem so that any
//! `flock://` path DuckDB opens is served from substrate pages on demand.
class FlockFileSystem : public FileSystem {
public:
	//! Route `flock://…` paths here. This is how RegisterSubSystem dispatches (like httpfs for s3://).
	bool CanHandleFile(const string &fpath) override;

	//! Wake the database named by the URI and return a read handle.
	unique_ptr<FileHandle> OpenFile(const string &path, FileOpenFlags flags,
	                                optional_ptr<FileOpener> opener) override;

	//! Positional read — must fill exactly nr_bytes or throw (DuckDB's storage layer relies on it).
	void Read(FileHandle &handle, void *buffer, int64_t nr_bytes, idx_t location) override;
	//! Sequential read — fills up to nr_bytes from the cursor, advances it, returns the count.
	int64_t Read(FileHandle &handle, void *buffer, int64_t nr_bytes) override;

	int64_t GetFileSize(FileHandle &handle) override;
	FileType GetFileType(FileHandle &handle) override;
	bool FileExists(const string &filename, optional_ptr<FileOpener> opener) override;

	void Seek(FileHandle &handle, idx_t location) override;
	idx_t SeekPosition(FileHandle &handle) override;
	void Reset(FileHandle &handle) override;
	bool CanSeek() override;
	bool OnDiskFile(FileHandle &handle) override;

	//! Read-only: every mutating entry point refuses with an actionable message (CLAUDE.md rule 3).
	void Write(FileHandle &handle, void *buffer, int64_t nr_bytes, idx_t location) override;
	int64_t Write(FileHandle &handle, void *buffer, int64_t nr_bytes) override;
	void Truncate(FileHandle &handle, int64_t new_size) override;
	void FileSync(FileHandle &handle) override;

	std::string GetName() const override {
		return "FlockFileSystem";
	}
};

} // namespace duckdb
