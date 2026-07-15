// readtrace — a DYLD interpose shim that records every syscall-level read DuckDB
// issues against a chosen file, so we can see whether DuckDB reads the *whole*
// database file to answer a query or only the byte ranges the query touches.
//
// Why this exists: FlockDB's wake is O(database) today because `paging::hydrate`
// materialises the entire file before DuckDB opens it (see
// crates/flock-kernel/src/lib.rs). The open question in docs/wake-latency.md is
// whether a page-faulting VFS would fix that — and that is only true if DuckDB
// *itself* reads a small fraction of the file for a selective query. A VFS
// intercepts exactly the reads this shim records: `pread`, `read`, `mmap`. So
// the set of ranges logged here is precisely the set of pages a lazy wake would
// have to fault in. We measure it rather than assume it.
//
// Build:  clang -dynamiclib -O2 readtrace.c -o readtrace.dylib
// Use:    DYLD_INSERT_LIBRARIES=./readtrace.dylib \
//         READTRACE_MATCH=flock READTRACE_OUT=trace.log ./harness ...
//
// Only files whose path contains READTRACE_MATCH are traced. Every pread/read
// on a traced fd is logged as "R <offset> <len>". mmap of a traced fd is logged
// as "M <offset> <len>" — an mmap makes the whole mapped range faultable, so we
// count it as touched. Not production code; a measurement instrument.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdarg.h>
#include <sys/mman.h>
#include <sys/types.h>

#define MAXFD 8192

static char tracked[MAXFD];
static FILE *out = NULL;
static const char *match = NULL;
static off_t seqpos[MAXFD]; // position for plain read()/lseek() tracking

__attribute__((constructor)) static void rt_init(void) {
    match = getenv("READTRACE_MATCH");
    const char *o = getenv("READTRACE_OUT");
    if (o) {
        out = fopen(o, "w");
        if (out) setvbuf(out, NULL, _IOLBF, 0);
    }
    memset(tracked, 0, sizeof(tracked));
    memset(seqpos, 0, sizeof(seqpos));
}

// The canonical Apple dyld interpose macro. A reference to `pread` (etc.) from
// *inside* a replacement resolves to the real libSystem symbol, so there is no
// recursion — this is the documented behaviour that makes the pattern usable.
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
        if (out) fprintf(out, "O %d %s\n", fd, path);
    }
}

static int rt_open(const char *path, int flags, ...) {
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
DYLD_INTERPOSE(rt_open, open)

static int rt_openat(int dirfd, const char *path, int flags, ...) {
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
DYLD_INTERPOSE(rt_openat, openat)

static int rt_close(int fd) {
    if (fd >= 0 && fd < MAXFD) tracked[fd] = 0;
    return close(fd);
}
DYLD_INTERPOSE(rt_close, close)

static ssize_t rt_pread(int fd, void *buf, size_t n, off_t off) {
    ssize_t r = pread(fd, buf, n, off);
    if (fd >= 0 && fd < MAXFD && tracked[fd] && out && r > 0)
        fprintf(out, "R %lld %zd\n", (long long)off, (size_t)r);
    return r;
}
DYLD_INTERPOSE(rt_pread, pread)

static ssize_t rt_read(int fd, void *buf, size_t n) {
    off_t off = (fd >= 0 && fd < MAXFD) ? seqpos[fd] : 0;
    ssize_t r = read(fd, buf, n);
    if (fd >= 0 && fd < MAXFD && tracked[fd]) {
        if (out && r > 0) fprintf(out, "R %lld %zd\n", (long long)off, (size_t)r);
        if (r > 0) seqpos[fd] += r;
    }
    return r;
}
DYLD_INTERPOSE(rt_read, read)

static off_t rt_lseek(int fd, off_t off, int whence) {
    off_t r = lseek(fd, off, whence);
    if (fd >= 0 && fd < MAXFD && tracked[fd] && r >= 0) seqpos[fd] = r;
    return r;
}
DYLD_INTERPOSE(rt_lseek, lseek)

static void *rt_mmap(void *addr, size_t len, int prot, int flags, int fd, off_t off) {
    void *p = mmap(addr, len, prot, flags, fd, off);
    if (fd >= 0 && fd < MAXFD && tracked[fd] && out && p != MAP_FAILED)
        fprintf(out, "M %lld %zd\n", (long long)off, len);
    return p;
}
DYLD_INTERPOSE(rt_mmap, mmap)
