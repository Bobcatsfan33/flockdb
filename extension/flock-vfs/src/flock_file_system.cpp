//===----------------------------------------------------------------------===//
// flock_file_system.cpp — implementation of the substrate-backed DuckDB FileSystem.
//
// Every safety-critical byte of this file's read path is a forward to flock_vfs_pread, which is the
// fuzzed Rust boundary. The C++ here parses a URI, holds a handle, and translates DuckDB's read calls
// into that one FFI call. It deliberately does no offset arithmetic of its own beyond advancing a
// sequential cursor — the arithmetic that could be wrong is the arithmetic that was fuzzed.
//===----------------------------------------------------------------------===//
#include "flock_file_system.hpp"

#include "duckdb/common/exception.hpp"
#include "duckdb/common/helper.hpp"

#include <cstdlib>
#include <string>
#include <unordered_map>

namespace duckdb {

//===----------------------------------------------------------------------===//
// URI parsing: flock://<pool>/<manifest_hex>?remote=<dir>&cache=<dir>&page_size=<n>&len=<n>
//===----------------------------------------------------------------------===//
namespace {

// The canonical URI a caller writes is `flock://<pool>/<manifest>?...`, but DuckDB NORMALISES a path
// before it reaches the FileSystem — it collapses consecutive slashes, so `flock://pool/…` arrives as
// `flock:/pool/…` (ONE slash). So we match on the bare `flock:` scheme and then skip however many
// slashes remain; the original double-slash and the normalised single-slash both parse. Matching the
// literal `flock://` instead made every ATTACH fail with "database does not exist", because the
// normalised path was never routed to this subsystem at all.
constexpr const char *kSchemeBare = "flock:";

struct FlockUri {
	std::string pool;
	std::string manifest_hex;
	std::string remote_dir;
	std::string cache_dir;
	size_t page_size = 0;
	uint64_t total_len = 0;
};

//! Parse the `flock://` URI, throwing an actionable error if any field is missing or malformed.
FlockUri ParseFlockUri(const string &path) {
	if (path.rfind(kSchemeBare, 0) != 0) {
		throw InvalidInputException("flock-vfs: '%s' is not a flock:// path. Expected "
		                            "flock://<pool>/<manifest_hex>?remote=<dir>&cache=<dir>&"
		                            "page_size=<n>&len=<n>",
		                            path);
	}
	// Strip `flock:` and then ALL leading slashes, so both `flock://pool/...` and DuckDB's normalised
	// `flock:/pool/...` yield the same `pool/manifest?params`.
	std::string rest = path.substr(std::string(kSchemeBare).size());
	while (!rest.empty() && rest.front() == '/') {
		rest.erase(rest.begin());
	}

	auto qpos = rest.find('?');
	if (qpos == std::string::npos) {
		throw InvalidInputException("flock-vfs: '%s' has no query string; the wake parameters "
		                            "(remote, cache, page_size, len) travel in it. Expected "
		                            "flock://<pool>/<manifest_hex>?remote=...&cache=...&page_size=...&len=...",
		                            path);
	}
	std::string authority = rest.substr(0, qpos); // <pool>/<manifest_hex>
	std::string query = rest.substr(qpos + 1);

	auto slash = authority.find('/');
	if (slash == std::string::npos) {
		throw InvalidInputException("flock-vfs: '%s' is missing the '<pool>/<manifest_hex>' segment", path);
	}
	FlockUri uri;
	uri.pool = authority.substr(0, slash);
	uri.manifest_hex = authority.substr(slash + 1);

	std::unordered_map<std::string, std::string> kv;
	size_t i = 0;
	while (i < query.size()) {
		auto amp = query.find('&', i);
		std::string pair = query.substr(i, amp == std::string::npos ? std::string::npos : amp - i);
		auto eq = pair.find('=');
		if (eq != std::string::npos) {
			kv[pair.substr(0, eq)] = pair.substr(eq + 1);
		}
		if (amp == std::string::npos) {
			break;
		}
		i = amp + 1;
	}

	auto require = [&](const char *key) -> std::string {
		auto it = kv.find(key);
		if (it == kv.end() || it->second.empty()) {
			throw InvalidInputException("flock-vfs: '%s' is missing required query parameter '%s'", path, key);
		}
		return it->second;
	};

	uri.remote_dir = require("remote");
	uri.cache_dir = require("cache");
	uri.page_size = static_cast<size_t>(std::stoull(require("page_size")));
	uri.total_len = static_cast<uint64_t>(std::stoull(require("len")));
	return uri;
}

//! Whether a path is a probe for the `<db>.wal` sibling. The query string (which carries the wake
//! params) is stripped first, so both `<db>.wal` and `<db>.wal?params` are recognised.
bool IsWalPath(const string &path) {
	std::string bare = path;
	if (auto q = bare.find('?'); q != std::string::npos) {
		bare.resize(q);
	}
	const std::string wal_suffix = ".wal";
	return bare.size() >= wal_suffix.size() &&
	       bare.compare(bare.size() - wal_suffix.size(), wal_suffix.size(), wal_suffix) == 0;
}

FlockFileHandle &AsFlock(FileHandle &handle) {
	return handle.Cast<FlockFileHandle>();
}

} // namespace

//===----------------------------------------------------------------------===//
// FlockFileHandle
//===----------------------------------------------------------------------===//
FlockFileHandle::FlockFileHandle(FileSystem &fs, string path, FileOpenFlags flags, FlockVfs *vfs_p, int64_t size)
    : FileHandle(fs, std::move(path), flags), vfs(vfs_p), position(0), file_size(size) {
}

FlockFileHandle::~FlockFileHandle() {
	FlockFileHandle::Close();
}

void FlockFileHandle::Close() {
	if (vfs) {
		flock_vfs_close(vfs);
		vfs = nullptr;
	}
}

//===----------------------------------------------------------------------===//
// FlockFileSystem
//===----------------------------------------------------------------------===//
bool FlockFileSystem::CanHandleFile(const string &fpath) {
	return fpath.rfind(kSchemeBare, 0) == 0;
}

unique_ptr<FileHandle> FlockFileSystem::OpenFile(const string &path, FileOpenFlags flags,
                                                 optional_ptr<FileOpener> opener) {
	if (flags.OpenForWriting()) {
		throw NotImplementedException("flock-vfs: '%s' is a sleeping database and is read-only. Open it "
		                              "with READ_ONLY; to write, wake it through flock-core, not the VFS.",
		                              path);
	}
	// A sleeping snapshot has NO write-ahead log. Some DuckDB READ_ONLY ATTACH paths OPEN the `<db>.wal`
	// sibling to replay it *without* first honouring FileExists (which reports it absent). Hand back an
	// EMPTY, zero-length handle (vfs=nullptr, size=0) — there is nothing to replay — rather than trying
	// to wake a substrate manifest whose id would be `<manifest>.wal` and does not exist. DuckDB checks
	// GetFileSize (0) and skips replay; the Read overrides also treat a null-vfs handle as immediate EOF.
	if (IsWalPath(path)) {
		return make_uniq<FlockFileHandle>(*this, path, flags, nullptr, 0);
	}
	FlockUri uri = ParseFlockUri(path);

	FlockVfs *vfs = flock_vfs_open(uri.remote_dir.c_str(), uri.cache_dir.c_str(), uri.pool.c_str(),
	                               uri.manifest_hex.c_str(), uri.page_size, uri.total_len);
	if (!vfs) {
		throw IOException("flock-vfs: failed to wake '%s'. A diagnostic was written to stderr; check "
		                  "that the remote/cache dirs exist and the manifest id and page_size match the "
		                  "database's WakeToken.",
		                  path);
	}
	int64_t size = flock_vfs_len(vfs);
	if (size < 0) {
		flock_vfs_close(vfs);
		throw IOException("flock-vfs: woke '%s' but its length is unavailable", path);
	}
	return make_uniq<FlockFileHandle>(*this, path, flags, vfs, size);
}

void FlockFileSystem::Read(FileHandle &handle, void *buffer, int64_t nr_bytes, idx_t location) {
	auto &fh = AsFlock(handle);
	if (!fh.vfs) {
		// The empty-WAL handle. A zero-length read is a no-op; any positive read is past the end of a
		// zero-byte file.
		if (nr_bytes == 0) {
			return;
		}
		throw IOException("flock-vfs: read past the end of an empty write-ahead log '%s'", fh.path);
	}
	// DuckDB's positional Read must deliver EXACTLY nr_bytes. Loop until satisfied, because a single
	// pread may stop at a page boundary; a genuine short read (past EOF, or a corrupt store the Rust
	// side refused) is a hard error here.
	auto *out = static_cast<uint8_t *>(buffer);
	int64_t filled = 0;
	while (filled < nr_bytes) {
		ssize_t n = flock_vfs_pread(fh.vfs, static_cast<int64_t>(location) + filled, out + filled,
		                            static_cast<size_t>(nr_bytes - filled));
		if (n < 0) {
			throw IOException("flock-vfs: read of '%s' failed at offset %lld (see stderr)", fh.path,
			                  static_cast<long long>(location + filled));
		}
		if (n == 0) {
			throw IOException("flock-vfs: read of '%s' hit end of file %lld bytes short at offset %lld",
			                  fh.path, static_cast<long long>(nr_bytes - filled),
			                  static_cast<long long>(location + filled));
		}
		filled += n;
	}
}

int64_t FlockFileSystem::Read(FileHandle &handle, void *buffer, int64_t nr_bytes) {
	auto &fh = AsFlock(handle);
	if (!fh.vfs) {
		return 0; // empty-WAL handle: immediate EOF
	}
	ssize_t n = flock_vfs_pread(fh.vfs, static_cast<int64_t>(fh.position), static_cast<uint8_t *>(buffer),
	                            static_cast<size_t>(nr_bytes));
	if (n < 0) {
		throw IOException("flock-vfs: sequential read of '%s' failed at offset %lld (see stderr)", fh.path,
		                  static_cast<long long>(fh.position));
	}
	fh.position += static_cast<idx_t>(n);
	return n;
}

int64_t FlockFileSystem::GetFileSize(FileHandle &handle) {
	return AsFlock(handle).file_size;
}

FileType FlockFileSystem::GetFileType(FileHandle &handle) {
	(void)handle;
	return FileType::FILE_TYPE_REGULAR;
}

bool FlockFileSystem::FileExists(const string &filename, optional_ptr<FileOpener> opener) {
	(void)opener;
	// A flock:// path denotes a sleeping database that exists by construction of its WakeToken; the
	// wake in OpenFile is the real existence check. Report true for any flock:// path so DuckDB opens
	// it rather than short-circuiting — OpenFile does the real validation and throws a clear error if
	// the wake parameters are missing or wrong.
	//
	// **Do NOT require the query string here.** DuckDB's READ_ONLY `ATTACH` existence probe calls
	// `FileExists` with the query string *stripped* (`flock://<pool>/<manifest>`, no `?`), while the
	// full URI (with the wake params) is what reaches `OpenFile`. An earlier version demanded a `?`,
	// which made that stripped probe report "database does not exist" and the attach failed. The scheme
	// is the existence signal; the params are OpenFile's problem.
	//
	// BUT report the SIBLING paths DuckDB probes as absent — chiefly `<db>.wal`. A sleeping database is
	// a single immutable snapshot with no separate write-ahead log; claiming its `.wal` existed would
	// make DuckDB (even READ_ONLY) try to open and replay a WAL that is not there. Anything ending
	// ".wal" — with or without a trailing query string — is a probe for a log this snapshot lacks.
	if (!CanHandleFile(filename)) {
		return false;
	}
	// The `.wal` sibling is reported absent (a sleeping snapshot has no write-ahead log). Everything
	// else that is a flock:// path is reported present — OpenFile does the real validation.
	return !IsWalPath(filename);
}

void FlockFileSystem::Seek(FileHandle &handle, idx_t location) {
	AsFlock(handle).position = location;
}

idx_t FlockFileSystem::SeekPosition(FileHandle &handle) {
	return AsFlock(handle).position;
}

void FlockFileSystem::Reset(FileHandle &handle) {
	AsFlock(handle).position = 0;
}

bool FlockFileSystem::CanSeek() {
	return true;
}

bool FlockFileSystem::OnDiskFile(FileHandle &handle) {
	// Faulted from object storage, not a local plain file — this tells DuckDB random reads are not as
	// cheap as local disk, which is exactly true of a page fault.
	(void)handle;
	return false;
}

void FlockFileSystem::Write(FileHandle &handle, void *buffer, int64_t nr_bytes, idx_t location) {
	(void)handle;
	(void)buffer;
	(void)nr_bytes;
	(void)location;
	throw NotImplementedException("flock-vfs is read-only: a sleeping database is served, never written. "
	                              "Wake it through flock-core to modify it.");
}

int64_t FlockFileSystem::Write(FileHandle &handle, void *buffer, int64_t nr_bytes) {
	(void)handle;
	(void)buffer;
	(void)nr_bytes;
	throw NotImplementedException("flock-vfs is read-only: a sleeping database is served, never written. "
	                              "Wake it through flock-core to modify it.");
}

void FlockFileSystem::Truncate(FileHandle &handle, int64_t new_size) {
	(void)handle;
	(void)new_size;
	throw NotImplementedException("flock-vfs is read-only: a sleeping database cannot be truncated.");
}

void FlockFileSystem::FileSync(FileHandle &handle) {
	// A read-only VFS has nothing to flush; make sync a no-op rather than an error so callers that sync
	// defensively are not broken.
	(void)handle;
}

} // namespace duckdb
