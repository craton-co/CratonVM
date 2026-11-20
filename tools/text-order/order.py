#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
"""tools/text-order/order.py -- boot text-ordering template and its resolution.

  order.py template ALIGNED_BIN OUT TRAP [TRAP...]
      The functions a function-per-page build (`-C llvm-args=-align-all-functions=12`)
      executed, from texttrap.c's files, in first-execution order (the first
      TRAP's order, then each later TRAP's new functions). Written as FNV-1a-64
      hashes of the DEMANGLED names: the crate hashes inside mangled Rust names
      change with the build profile (LTO, codegen units) and the toolchain, the
      demangled path does not.

  order.py resolve TEMPLATE BIN OUT
      Map the template onto BIN's own symbols and write an LLD
      `--symbol-ordering-file` (mangled names, template order). Prints how much
      of the template and of BIN's text it covers.

Needs binutils `nm` and `c++filt` (Rust v0 demangling, binutils >= 2.36).
"""
import bisect
import struct
import subprocess
import sys

HEADER = "# tools/text-order template v1: FNV-1a-64 of demangled symbol names, first-execution order\n"


def fnv(s):
    h = 0xCBF29CE484222325
    for b in s.encode():
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def text_symbols(binary):
    """(address, size, mangled name) of every defined function, by address."""
    out = subprocess.run(["nm", "-n", "-S", "--defined-only", binary],
                         capture_output=True, text=True, check=True).stdout
    syms = []
    for line in out.splitlines():
        f = line.split(" ", 3)
        if len(f) == 4 and f[2] in "tTwW":
            syms.append((int(f[0], 16), int(f[1], 16), f[3]))
    return syms


def demangle(names):
    out = subprocess.run(["c++filt"], input="\n".join(names) + "\n",
                         capture_output=True, text=True, check=True).stdout.splitlines()
    if len(out) != len(names):
        sys.exit("c++filt returned a different number of lines")
    return out


def text_segment_vaddr(binary):
    out = subprocess.run(["readelf", "-lW", binary], capture_output=True, text=True,
                         check=True).stdout
    for line in out.splitlines():
        f = line.split()
        if f and f[0] == "LOAD" and "E" in "".join(f[6:-1]):
            return int(f[2], 16)
    sys.exit(f"no executable LOAD segment in {binary}")


def template(aligned, out, traps):
    syms = text_symbols(aligned)
    starts = [a for a, _, _ in syms]
    seg = text_segment_vaddr(aligned) // 4096 * 4096
    seen, order = set(), []
    for trap in traps:
        data = open(trap, "rb").read()
        words = struct.unpack(f"<{len(data) // 8}Q", data)
        for off in words[1:]:
            page = seg + off
            # The function covering the page start, and every one starting inside it.
            i = bisect.bisect_right(starts, page + 4095) - 1
            while i >= 0 and syms[i][0] + max(syms[i][1], 1) > page:
                name = syms[i][2]
                if name not in seen:
                    seen.add(name)
                    order.append(name)
                if syms[i][0] <= page:
                    break
                i -= 1
    keys, done = [], set()
    for d in demangle(order):
        k = fnv(d)
        if k not in done:
            done.add(k)
            keys.append(k)
    size = {n: s for _, s, n in syms}
    print(f"template: {len(keys)} functions, {sum(size[n] for n in order) / 1e6:.2f} MB "
          f"in the aligned build, from {len(traps)} trap file(s)")
    with open(out, "w") as f:
        f.write(HEADER)
        f.write("\n".join(keys) + "\n")


def resolve(tmpl, binary, out):
    keys = [l.strip() for l in open(tmpl) if l.strip() and not l.startswith("#")]
    syms = text_symbols(binary)
    by_key = {}
    for (_, s, n), d in zip(syms, demangle([n for _, _, n in syms])):
        by_key.setdefault(fnv(d), []).append((n, s))
    names, size, hit = [], 0, 0
    for k in keys:
        if k in by_key:
            hit += 1
            for n, s in by_key[k]:
                names.append(n)
                size += s
    total = sum(s for _, s, _ in syms)
    print(f"resolve: {hit}/{len(keys)} template functions found ({100 * hit / max(len(keys), 1):.1f} %), "
          f"{len(names)} symbols, {size / 1e6:.2f} MB of {total / 1e6:.2f} MB text")
    with open(out, "w") as f:
        f.write("\n".join(names) + "\n")
    return hit, len(keys)


if __name__ == "__main__":
    if len(sys.argv) >= 5 and sys.argv[1] == "template":
        template(sys.argv[2], sys.argv[3], sys.argv[4:])
    elif len(sys.argv) == 5 and sys.argv[1] == "resolve":
        resolve(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        sys.exit(__doc__)
