#!/usr/bin/env python3
# analyze — collapse a readtrace.log into "how much of the file did DuckDB
# actually touch". Prints unique bytes touched (union of all read/mmap ranges),
# raw bytes requested, number of read calls, and the file size for reference.
import sys

def merge(ranges):
    ranges.sort()
    out = []
    for a, b in ranges:
        if out and a <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], b))
        else:
            out.append((a, b))
    return out

def main():
    log, size = sys.argv[1], int(sys.argv[2])
    ranges, raw, ncalls = [], 0, 0
    with open(log) as f:
        for line in f:
            p = line.split()
            if not p or p[0] not in ("R", "M"):
                continue
            off, ln = int(p[1]), int(p[2])
            ranges.append((off, off + ln))
            raw += ln
            ncalls += 1
    uniq = sum(b - a for a, b in merge(ranges))
    pct = 100.0 * uniq / size if size else 0.0
    print(f"{size}\t{uniq}\t{raw}\t{ncalls}\t{pct:.1f}")

if __name__ == "__main__":
    main()
