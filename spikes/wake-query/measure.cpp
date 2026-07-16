//===----------------------------------------------------------------------===//
// measure.cpp — the DuckDB side of the wake → first-QUERY-RESULT measurement.
//
// Unlike spikes/wake-latency (which timed wake → first *page read* through a dyld interpose shim), this
// harness times wake → first *query result* through the LOADED C++ FileSystem extension: DuckDB opens a
// sleeping database at a `flock://` path, and every read it issues is served page-by-page from
// substrate's tier via the fuzzed flock-vfs boundary. It links the crate's own libduckdb.a (stock,
// unpatched DuckDB 1.10504) and the extension's real FlockFileSystem (extension/flock-vfs/src).
//
// Modes:
//   measure seed <path> <cold_rows>
//       Create hot(1000 rows) + cold(cold_rows) in one plain .duckdb file and CHECKPOINT. Same schema
//       as spikes/wake-latency/duckharness. This is the file the Rust `seed` bin chunks + sleeps.
//
//   measure version
//       Print DuckDB::LibraryVersion() — the version the .duckdb_extension footer must carry.
//
//   measure expected <which> <cold_rows> <real.duckdb>
//       Stock DuckDB on the REAL file, no extension — the byte-correct ground-truth RESULT.
//
//   measure register <which> <cold_rows> <flock_uri>
//       Install FlockFileSystem via RegisterSubSystem (what the extension's LoadInternal does), ATTACH
//       the flock:// database READ_ONLY, run one query, materialise the first value, and time
//       wake→first-row. Prints RESULT / LATENCY_US / FAULT_MISSES (tier GETs, from flock_vfs_tier_misses).
//
//   measure load <which> <cold_rows> <ext_path> <flock_uri>
//       Same, but installs the FileSystem by actually LOADing the built <name>.duckdb_extension file
//       (allow_unsigned_extensions), to prove the packaged loadable extension loads and serves.
//
//   measure eager <which> <cold_rows> <remote> <cache> <pool> <manifest_hex> <page_size> <total_len>
//       The CONTROL that today's FlockDB pays: hydrate the WHOLE file from the tier (flock_vfs_pread in a
//       loop), then open the hydrated temp file with stock DuckDB and query. Time wake→first-row — this
//       is O(database), the rising curve the flat one must be read against.
//
// `which`: open=SELECT 1 (pure wake); hot=selective aggregate on the small table; point=zonemap-pruned
// point lookup on the big table; cold=full-table aggregate (the scaling control). `v % 1000` keeps the
// aggregate exact-checkable in int64 while still forcing the whole operand column to be read.
//
// Not production code. A measurement instrument.
//===----------------------------------------------------------------------===//
#include "duckdb.hpp"

#include "flock_file_system.hpp"
#include "flock_vfs_ffi.h"

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

using namespace duckdb;

namespace {

int64_t now_us() {
	using namespace std::chrono;
	return duration_cast<microseconds>(steady_clock::now().time_since_epoch()).count();
}

// The query for `which`, qualified by `prefix` ("fdb." for an attached flock db, "" for a plain open).
std::string make_sql(const std::string &which, long cold_rows, const std::string &prefix) {
	if (which == "open") {
		return "SELECT 1";
	}
	if (which == "hot") {
		return "SELECT sum(v % 1000)::BIGINT FROM " + prefix + "hot";
	}
	if (which == "cold") {
		return "SELECT sum(v % 1000)::BIGINT FROM " + prefix + "cold";
	}
	if (which == "point") {
		return "SELECT sum(v % 1000)::BIGINT FROM " + prefix + "cold WHERE id = " +
		       std::to_string(cold_rows / 2);
	}
	return std::string();
}

// Run `sql`, materialise the first value as a string, or return false with the error on stderr.
bool run_first_value(Connection &con, const std::string &sql, std::string &out_value) {
	auto result = con.Query(sql);
	if (result->HasError()) {
		fprintf(stderr, "query failed: %s\n  sql: %s\n", result->GetError().c_str(), sql.c_str());
		return false;
	}
	if (result->RowCount() == 0 || result->ColumnCount() == 0) {
		out_value = "";
		return true;
	}
	out_value = result->GetValue(0, 0).ToString();
	return true;
}

int do_seed(const char *path, long cold_rows) {
	DuckDB db(path);
	Connection con(db);
	// Per-row-unique, effectively incompressible values (hash + md5), so the tables occupy real, distinct
	// blocks rather than collapsing under compression — otherwise we measure compression, not locality.
	std::vector<std::string> stmts = {
	    "CREATE TABLE hot (id BIGINT, v BIGINT, w VARCHAR)",
	    "INSERT INTO hot SELECT i, (hash(i*2+1) >> 1)::BIGINT, md5(i::VARCHAR || 'hot') FROM range(1000) t(i)",
	    "CREATE TABLE cold (id BIGINT, v BIGINT, w VARCHAR)",
	};
	for (auto &s : stmts) {
		auto r = con.Query(s);
		if (r->HasError()) {
			fprintf(stderr, "seed failed: %s\n", r->GetError().c_str());
			return 2;
		}
	}
	if (cold_rows > 0) {
		std::string ins = "INSERT INTO cold SELECT i, (hash(i) >> 1)::BIGINT, md5(i::VARCHAR || 'cold') "
		                  "FROM range(" +
		                  std::to_string(cold_rows) + ") t(i)";
		auto r = con.Query(ins);
		if (r->HasError()) {
			fprintf(stderr, "seed insert failed: %s\n", r->GetError().c_str());
			return 2;
		}
	}
	auto r = con.Query("CHECKPOINT");
	if (r->HasError()) {
		fprintf(stderr, "checkpoint failed: %s\n", r->GetError().c_str());
		return 2;
	}
	return 0;
}

int do_expected(const std::string &which, long cold_rows, const char *realpath) {
	// Stock DuckDB, READ_ONLY, on the real file. The ground truth every faulted result is checked against.
	DBConfig config;
	config.options.access_mode = AccessMode::READ_ONLY;
	DuckDB db(realpath, &config);
	Connection con(db);
	std::string value;
	if (!run_first_value(con, make_sql(which, cold_rows, ""), value)) {
		return 2;
	}
	printf("RESULT %s\n", value.c_str());
	return 0;
}

// An observing subclass so the harness can read the tier fault count of the handle DuckDB opened. It
// adds no behaviour — it records the last-opened *database* FlockVfs* so `flock_vfs_tier_misses` can be
// read while the handle is still open, right after the query. The production FlockFileSystem is left
// untouched.
//
// Crucially it records only handles that carry a real vfs. A READ_ONLY ATTACH also opens the
// `<db>.wal` sibling, and a sleeping snapshot has no WAL, so that open returns an EMPTY handle with
// `vfs == nullptr` (see FlockFileSystem::OpenFile). If we captured that one it would clobber the real
// database handle and `flock_vfs_tier_misses` would read -1 — which is exactly the bogus fault count
// the lazy rows reported before this guard. The `.wal` open happens after the db open, so filtering by
// a non-null vfs (not by open order) is what keeps `last_vfs` pointing at the handle that served the
// query's faults.
class MeasuringFS : public FlockFileSystem {
public:
	unique_ptr<FileHandle> OpenFile(const string &path, FileOpenFlags flags,
	                                optional_ptr<FileOpener> opener) override {
		auto handle = FlockFileSystem::OpenFile(path, flags, opener);
		if (handle) {
			if (FlockVfs *vfs = handle->Cast<FlockFileHandle>().vfs) {
				last_vfs = vfs;
			}
		}
		return handle;
	}
	FlockVfs *last_vfs = nullptr;
};

// Shared measured region: ATTACH the flock db READ_ONLY, run the query, materialise the first row, time
// it. `misses` is read after the query while the handle is still open, or -1 if unavailable.
int attach_and_measure(Connection &con, const std::string &uri, const std::string &which, long cold_rows,
                       MeasuringFS *fs) {
	std::string value;
	int64_t t0 = now_us();
	auto attach = con.Query("ATTACH '" + uri + "' AS fdb (READ_ONLY)");
	if (attach->HasError()) {
		fprintf(stderr, "ATTACH failed: %s\n  uri: %s\n", attach->GetError().c_str(), uri.c_str());
		return 2;
	}
	if (!run_first_value(con, make_sql(which, cold_rows, "fdb."), value)) {
		return 2;
	}
	int64_t t1 = now_us();

	int64_t misses = (fs && fs->last_vfs) ? flock_vfs_tier_misses(fs->last_vfs) : -1;
	int64_t hits = (fs && fs->last_vfs) ? flock_vfs_tier_hits(fs->last_vfs) : -1;
	printf("RESULT %s\n", value.c_str());
	printf("LATENCY_US %lld\n", (long long)(t1 - t0));
	printf("FAULT_MISSES %lld\n", (long long)misses);
	printf("FAULT_HITS %lld\n", (long long)hits);
	return 0;
}

int do_register(const std::string &which, long cold_rows, const std::string &uri) {
	DuckDB db(nullptr); // in-memory instance; the flock db is attached to it
	auto fs = make_uniq<MeasuringFS>();
	MeasuringFS *fsptr = fs.get();
	// This is exactly what the extension's LoadInternal(db) does: install FlockFileSystem on the
	// instance's VirtualFileSystem so any flock:// path is served page-by-page from substrate.
	db.instance->GetFileSystem().RegisterSubSystem(std::move(fs));
	Connection con(db);
	return attach_and_measure(con, uri, which, cold_rows, fsptr);
}

int do_load(const std::string &which, long cold_rows, const std::string &ext_path, const std::string &uri) {
	DBConfig config;
	// The packaged extension is unsigned (no DuckDB signing key here); allow it, then LOAD the real file.
	// `allow_unsigned_extensions` is a GLOBAL_ONLY setting, so it must be set on the config before the
	// instance starts, not via a runtime SET.
	config.SetOptionByName("allow_unsigned_extensions", Value::BOOLEAN(true));
	DuckDB db(nullptr, &config);
	Connection con(db);
	auto loaded = con.Query("LOAD '" + ext_path + "'");
	if (loaded->HasError()) {
		fprintf(stderr, "LOAD_FAILED: %s\n  ext: %s\n", loaded->GetError().c_str(), ext_path.c_str());
		return 4;
	}
	printf("LOADED %s\n", ext_path.c_str());
	// The FileSystem is now installed by DuckDB's own loader; we hold no handle to it, so the fault count
	// is not read here (the `register` mode reports it). This mode proves the packaged extension LOADs
	// and serves correct rows.
	return attach_and_measure(con, uri, which, cold_rows, nullptr);
}

int do_eager(const std::string &which, long cold_rows, const char *remote, const char *cache,
             const char *pool, const char *manifest_hex, size_t page_size, uint64_t total_len) {
	// The control today's FlockDB pays: hydrate the WHOLE file, then open it with stock DuckDB. O(database).
	int64_t t0 = now_us();
	FlockVfs *h = flock_vfs_open(remote, cache, pool, manifest_hex, page_size, total_len);
	if (!h) {
		fprintf(stderr, "eager: flock_vfs_open failed (see stderr above)\n");
		return 3;
	}
	std::string tmp = std::string(cache) + "/hydrated.duckdb";
	FILE *out = fopen(tmp.c_str(), "wb");
	if (!out) {
		fprintf(stderr, "eager: cannot open temp %s\n", tmp.c_str());
		flock_vfs_close(h);
		return 3;
	}
	std::vector<uint8_t> chunk(page_size);
	uint64_t off = 0;
	while (off < total_len) {
		ssize_t n = flock_vfs_pread(h, (int64_t)off, chunk.data(), chunk.size());
		if (n < 0) {
			fprintf(stderr, "eager: pread failed at %llu\n", (unsigned long long)off);
			fclose(out);
			flock_vfs_close(h);
			return 3;
		}
		if (n == 0) {
			break;
		}
		if (fwrite(chunk.data(), 1, (size_t)n, out) != (size_t)n) {
			fprintf(stderr, "eager: short write to temp\n");
			fclose(out);
			flock_vfs_close(h);
			return 3;
		}
		off += (uint64_t)n;
	}
	fclose(out);
	int64_t misses = flock_vfs_tier_misses(h);
	flock_vfs_close(h);

	DBConfig config;
	config.options.access_mode = AccessMode::READ_ONLY;
	DuckDB db(tmp.c_str(), &config);
	Connection con(db);
	std::string value;
	if (!run_first_value(con, make_sql(which, cold_rows, ""), value)) {
		return 2;
	}
	int64_t t1 = now_us();
	printf("RESULT %s\n", value.c_str());
	printf("LATENCY_US %lld\n", (long long)(t1 - t0));
	printf("FAULT_MISSES %lld\n", (long long)misses);
	remove(tmp.c_str());
	return 0;
}

} // namespace

int main(int argc, char **argv) {
	if (argc >= 2 && std::string(argv[1]) == "version") {
		printf("%s\n", DuckDB::LibraryVersion());
		return 0;
	}
	if (argc >= 4 && std::string(argv[1]) == "seed") {
		return do_seed(argv[2], strtol(argv[3], nullptr, 10));
	}
	if (argc >= 5 && std::string(argv[1]) == "expected") {
		return do_expected(argv[2], strtol(argv[3], nullptr, 10), argv[4]);
	}
	if (argc >= 5 && std::string(argv[1]) == "register") {
		return do_register(argv[2], strtol(argv[3], nullptr, 10), argv[4]);
	}
	if (argc >= 6 && std::string(argv[1]) == "load") {
		return do_load(argv[2], strtol(argv[3], nullptr, 10), argv[4], argv[5]);
	}
	if (argc >= 10 && std::string(argv[1]) == "eager") {
		return do_eager(argv[2], strtol(argv[3], nullptr, 10), argv[4], argv[5], argv[6], argv[7],
		                (size_t)strtoull(argv[8], nullptr, 10), strtoull(argv[9], nullptr, 10));
	}
	fprintf(stderr,
	        "usage:\n"
	        "  %s seed <path> <cold_rows>\n"
	        "  %s version\n"
	        "  %s expected <which> <cold_rows> <real.duckdb>\n"
	        "  %s register <which> <cold_rows> <flock_uri>\n"
	        "  %s load <which> <cold_rows> <ext_path> <flock_uri>\n"
	        "  %s eager <which> <cold_rows> <remote> <cache> <pool> <manifest_hex> <page_size> <total_len>\n",
	        argv[0], argv[0], argv[0], argv[0], argv[0], argv[0]);
	return 1;
}
