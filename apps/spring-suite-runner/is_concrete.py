#!/usr/bin/env python3
"""Filter a list of candidate test classes down to the ones a JUnit engine can
actually run standalone: concrete (non-abstract, non-interface) classes.

Reads `module<TAB>fqcn` rows on stdin, writes the surviving rows to stdout.
`run-suite.sh discover` finds candidates by *filename* (`*Tests.class`), which
also picks up the `Abstract*Tests` base classes that exist only to be extended
-- they legitimately contain zero @Test methods, so every run reported them as
EMPTY and inflated the non-passed count.  Checking ACC_ABSTRACT/ACC_INTERFACE
in the class file itself is the cheap, exact test.
"""
import glob
import os
import struct
import sys

ACC_INTERFACE = 0x0200
ACC_ABSTRACT = 0x0400

# constant-pool tag -> fixed byte length of its info section (None = variable)
FIXED = {
    7: 2, 8: 2, 16: 2, 19: 2, 20: 2,          # Class, String, MethodType, Module, Package
    15: 3,                                     # MethodHandle
    9: 4, 10: 4, 11: 4, 12: 4, 17: 4, 18: 4,   # {Field,Method,InterfaceMethod}ref, NameAndType, Dynamic, InvokeDynamic
    3: 4, 4: 4,                                # Integer, Float
    5: 8, 6: 8,                                # Long, Double  (also take two slots)
}


def access_flags(path):
    with open(path, 'rb') as f:
        data = f.read()
    if len(data) < 10 or data[:4] != b'\xca\xfe\xba\xbe':
        raise ValueError('not a class file')
    n = struct.unpack_from('>H', data, 8)[0]
    off = 10
    i = 1
    while i < n:
        tag = data[off]
        off += 1
        if tag == 1:  # Utf8
            off += 2 + struct.unpack_from('>H', data, off)[0]
        else:
            size = FIXED.get(tag)
            if size is None:
                raise ValueError('unknown constant-pool tag %d' % tag)
            off += size
            if tag in (5, 6):
                i += 1  # long/double occupy two entries
        i += 1
    return struct.unpack_from('>H', data, off)[0]


def main():
    kept = dropped = unreadable = 0
    for line in sys.stdin:
        row = line.rstrip('\n')
        if not row:
            continue
        try:
            module, fqcn = row.split('\t')
        except ValueError:
            continue
        rel = fqcn.replace('.', '/') + '.class'
        path = None
        for cand in glob.glob(os.path.join(module, 'build', 'classes', '*', 'test', rel)):
            path = cand
            break
        if path is None:
            unreadable += 1
            print(row)          # can't tell -> keep it, same as before
            kept += 1
            continue
        try:
            flags = access_flags(path)
        except Exception as exc:
            print('WARN %s: %s' % (fqcn, exc), file=sys.stderr)
            unreadable += 1
            print(row)
            kept += 1
            continue
        if flags & (ACC_ABSTRACT | ACC_INTERFACE):
            dropped += 1
            continue
        print(row)
        kept += 1
    print('[is_concrete] kept=%d dropped_abstract=%d unresolved=%d'
          % (kept, dropped, unreadable), file=sys.stderr)


if __name__ == '__main__':
    main()
