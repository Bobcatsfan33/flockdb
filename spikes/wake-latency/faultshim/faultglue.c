// faultglue — the __DATA,__interpose plumbing for the page-faulting read path.
//
// This is the same interception point the F4 read-trace shim used (proofs/lazy-wake/readtrace.c),
// turned from a *tracer* into a *server*: instead of recording the byte ranges DuckDB reads on the
// database file, it FILLS them, page by page, from substrate's TieredStore via `flock_serve`
// (defined in Rust, in this same dylib). A FUSE `read` handler would sit at exactly this boundary;
// this sits one layer lower, in-process, with no kernel round-trip — which is why the latency it
// measures is a *floor below* what FUSE would deliver, and a faithful proxy for an in-process C++
// DuckDB FileSystem extension. See the spike README.
//
// Only the database file is served: an open whose path contains FLOCK_DB_MATCH is "tracked", and
// every pread/read on a tracked fd is answered by `flock_serve`. Every other fd — the substrate
// cache files, the object-store tier, stdout — passes straight through to libc, so there is no
// recursion when `flock_serve` itself reads pages from the tier.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdarg.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/types.h>

#define MAXFD 8192

// Defined in Rust (faultshim/src/lib.rs), same dylib. Fills `buf` with `n` bytes of the database
// file starting at `off`, faulting the covering substrate pages on demand. Returns bytes filled,
// or -1 on error.
extern long flock_serve(long long off, void *buf, unsigned long n);

// A global the Rust side references so the linker pulls this object (and its __interpose section)
// into the dylib. Without a referenced global symbol, the archive member — whose interpose atoms
// are all `static` — would be dropped, and nothing would be interposed. See build.rs.
int flock_faultglue_anchor(void) { return 1; }

static char tracked[MAXFD];
static off_t seqpos[MAXFD];
static const char *match = NULL;

__attribute__((constructor)) static void fg_init(void) {
    match = getenv("FLOCK_DB_MATCH");
    memset(tracked, 0, sizeof(tracked));
    memset(seqpos, 0, sizeof(seqpos));
}

// The canonical Apple dyld interpose macro (identical to readtrace.c's). A reference to the real
// `pread` from inside the replacement resolves to libSystem, so there is no recursion.
#define DYLD_INTERPOSE(_replacement, _replacee)                                  \
    __attribute__((used)) static struct {                                        \
        const void *replacement;                                                 \
        const void *replacee;                                                    \
    } _interpose_##_replacee __attribute__((section("__DATA,__interpose"))) = {  \
        (const void *)(unsigned long)&_replacement,                              \
        (const void *)(unsigned long)&_replacee};

static void note_open(int fd, const char *path) {
    if (fd >= 0 && fd < MAXFD && match && path && strstr(path, match)) {
        tracked[fd] = 1;
        seqpos[fd] = 0;
    }
}

static int fg_open(const char *path, int flags, ...) {
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t)va_arg(ap, int);
        va_end(ap);
    }
    int fd = open(path, flags, mode);
    note_open(fd, path);
    return fd;
}
DYLD_INTERPOSE(fg_open, open)

static int fg_openat(int dirfd, const char *path, int flags, ...) {
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t)va_arg(ap, int);
        va_end(ap);
    }
    int fd = openat(dirfd, path, flags, mode);
    note_open(fd, path);
    return fd;
}
DYLD_INTERPOSE(fg_openat, openat)

static int fg_close(int fd) {
    if (fd >= 0 && fd < MAXFD) tracked[fd] = 0;
    return close(fd);
}
DYLD_INTERPOSE(fg_close, close)

static ssize_t fg_pread(int fd, void *buf, size_t n, off_t off) {
    if (fd >= 0 && fd < MAXFD && tracked[fd]) {
        long r = flock_serve((long long)off, buf, (unsigned long)n);
        if (r < 0) { errno = EIO; return -1; }
        return (ssize_t)r;
    }
    return pread(fd, buf, n, off);
}
DYLD_INTERPOSE(fg_pread, pread)

static ssize_t fg_read(int fd, void *buf, size_t n) {
    if (fd >= 0 && fd < MAXFD && tracked[fd]) {
        long r = flock_serve((long long)seqpos[fd], buf, (unsigned long)n);
        if (r < 0) { errno = EIO; return -1; }
        seqpos[fd] += r;
        return (ssize_t)r;
    }
    return read(fd, buf, n);
}
DYLD_INTERPOSE(fg_read, read)

static off_t fg_lseek(int fd, off_t off, int whence) {
    off_t r = lseek(fd, off, whence);
    if (fd >= 0 && fd < MAXFD && tracked[fd] && r >= 0) seqpos[fd] = r;
    return r;
}
DYLD_INTERPOSE(fg_lseek, lseek)

// If DuckDB ever mmap'd the database file we could not serve it lazily through this path, and the
// measurement would be silently wrong (it would read the sparse zero-file). The F4 trace showed
// DuckDB does NOT mmap the main file for these queries; this makes a violation LOUD rather than
// silent, so the number cannot rot into a lie. It does not attempt to serve the mapping.
static void *fg_mmap(void *addr, size_t len, int prot, int flags, int fd, off_t off) {
    if (fd >= 0 && fd < MAXFD && tracked[fd]) {
        fprintf(stderr,
                "faultshim: DuckDB mmap'd the tracked database file (off=%lld len=%zu) — the "
                "page-faulting path cannot serve an mmap, so this measurement is INVALID. "
                "Aborting rather than reporting a wrong number.\n",
                (long long)off, len);
        abort();
    }
    return mmap(addr, len, prot, flags, fd, off);
}
DYLD_INTERPOSE(fg_mmap, mmap)
