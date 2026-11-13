#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
"""Bucket `perf script -F comm,tid,ip,sym,dso` samples by thread class and code module.

    perf record -e instructions:u -c 10000000 -o p.data -- <vm> ...
    perf script -i p.data -F comm,tid,ip,sym,dso | instr_buckets.py [--syms N] [--depth N]

Works on CratonVM (Rust crate::module symbols, JIT bodies resolve to a perf-*.map
dso when CRATONVM_JIT_PERF_MAP=1 and the map is still on disk) and on HotSpot
(libjvm.so symbols, JIT code is anonymous). Rows are shares of ALL samples.
"""
import collections, re, sys

LINE = re.compile(r'\s*(.+?)\s+(\d+)\s+([0-9a-f]+)\s+(.*)\s+\((.*)\)\s*$')
CLASSES = [  # first match wins
    ('compile', r'^(cratonvm-jit-co|C[12] CompilerThre|Compile Broker)'),
    ('gc', r'(GC|G1 |ZGC|cratonvm-gc|VM Thread)'),
    ('main', r'^(main-vm|main)$'),
    ('test', r'^(ForkJoinPool|Thread-|pool-)'),
]


def thread_class(comm):
    for name, rx in CLASSES:
        if re.search(rx, comm):
            return name
    return 'other'


def module(sym, dso, depth=2):
    if 'perf-' in dso and dso.endswith('.map'):
        return 'JIT code'
    if dso in ('[unknown]', '') or dso.startswith('//anon'):
        return 'JIT code/unknown'
    if '/libc.' in dso or '/ld-linux' in dso:
        return 'libc'
    if dso.endswith('libjvm.so'):
        return 'libjvm:' + sym.split('::')[0][:40]
    s = re.sub(r'^<?&?(mut )?', '', sym)
    return '::'.join(s.split('::')[:depth])[:80]


def main():
    syms = int(sys.argv[sys.argv.index('--syms') + 1]) if '--syms' in sys.argv else 0
    depth = int(sys.argv[sys.argv.index('--depth') + 1]) if '--depth' in sys.argv else 2
    mods = collections.defaultdict(collections.Counter)
    names = collections.defaultdict(collections.Counter)
    total = 0
    for line in sys.stdin:
        m = LINE.match(line)
        if not m:
            continue
        comm, _tid, _ip, sym, dso = m.groups()
        c = thread_class(comm)
        total += 1
        mods[c][module(sym, dso, depth)] += 1
        if syms:
            names[c][sym[:110]] += 1
    assert total, 'no samples parsed'
    for c in sorted(mods, key=lambda k: -sum(mods[k].values())):
        n = sum(mods[c].values())
        print(f'== {c}: {100 * n / total:.1f}% of all samples ({n})')
        for k, v in mods[c].most_common(18 if depth > 2 else 12):
            print(f'   {100 * v / total:5.1f}%  {k}')
        for k, v in names[c].most_common(syms):
            print(f'   {100 * v / total:5.2f}%  sym {k}')
    print(f'total samples {total}')


if __name__ == '__main__':
    main()
