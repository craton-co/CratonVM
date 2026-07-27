#!/usr/bin/env python3
"""Filter a list of candidate test classes down to the ones a JUnit engine can
actually run: concrete (non-abstract, non-interface) classes that are not
class-level `@Disabled`.

Reads `module<TAB>fqcn` rows on stdin, writes the surviving rows to stdout.
`run-suite.sh discover` finds candidates by *filename* (`*Tests.class`), which
also picks up the `Abstract*Tests` base classes that exist only to be extended,
JUnit meta-annotation interfaces such as `@PathPatternsParameterizedTest`, and
classes the suite itself disables. All of those legitimately contain zero
runnable tests, so every run reported them as EMPTY and inflated the non-passed
count -- 74 of them in the 2026-07-27 full-suite run, none a CratonVM bug.
Reading the class file directly is the cheap, exact test.
"""
import glob
import os
import struct
import sys

ACC_INTERFACE = 0x0200
ACC_ABSTRACT = 0x0400

DISABLED_DESC = b'Lorg/junit/jupiter/api/Disabled;'

# constant-pool tag -> fixed byte length of its info section
FIXED = {
    7: 2, 8: 2, 16: 2, 19: 2, 20: 2,          # Class, String, MethodType, Module, Package
    15: 3,                                     # MethodHandle
    9: 4, 10: 4, 11: 4, 12: 4, 17: 4, 18: 4,   # {Field,Method,InterfaceMethod}ref, NameAndType, Dynamic, InvokeDynamic
    3: 4, 4: 4,                                # Integer, Float
    5: 8, 6: 8,                                # Long, Double (also take two slots)
}


def _skip_attributes(data, off):
    count = struct.unpack_from('>H', data, off)[0]
    off += 2
    for _ in range(count):
        length = struct.unpack_from('>I', data, off + 2)[0]
        off += 6 + length
    return off


def _skip_members(data, off):
    """Skip a fields[] or methods[] table."""
    count = struct.unpack_from('>H', data, off)[0]
    off += 2
    for _ in range(count):
        off += 6                       # access_flags, name_index, descriptor_index
        off = _skip_attributes(data, off)
    return off


def inspect(path):
    """Return (access_flags, class_level_disabled)."""
    with open(path, 'rb') as f:
        data = f.read()
    if len(data) < 10 or data[:4] != b'\xca\xfe\xba\xbe':
        raise ValueError('not a class file')
    n = struct.unpack_from('>H', data, 8)[0]
    off = 10
    utf8 = {}
    i = 1
    while i < n:
        tag = data[off]
        off += 1
        if tag == 1:  # Utf8
            length = struct.unpack_from('>H', data, off)[0]
            utf8[i] = data[off + 2:off + 2 + length]
            off += 2 + length
        else:
            size = FIXED.get(tag)
            if size is None:
                raise ValueError('unknown constant-pool tag %d' % tag)
            off += size
            if tag in (5, 6):
                i += 1  # long/double occupy two entries
        i += 1

    flags = struct.unpack_from('>H', data, off)[0]
    off += 6                                   # access_flags, this_class, super_class
    ifaces = struct.unpack_from('>H', data, off)[0]
    off += 2 + 2 * ifaces
    off = _skip_members(data, off)             # fields
    off = _skip_members(data, off)             # methods

    # Class-level attributes: look for @Disabled inside RuntimeVisibleAnnotations.
    disabled = False
    count = struct.unpack_from('>H', data, off)[0]
    off += 2
    for _ in range(count):
        name_idx = struct.unpack_from('>H', data, off)[0]
        length = struct.unpack_from('>I', data, off + 2)[0]
        body = data[off + 6:off + 6 + length]
        if utf8.get(name_idx) == b'RuntimeVisibleAnnotations':
            # Each annotation starts with a type_index into the constant pool.
            # Rather than decode element_value trees, check whether any Utf8 the
            # attribute's leading type indices point at is @Disabled's descriptor.
            num = struct.unpack_from('>H', body, 0)[0] if len(body) >= 2 else 0
            pos = 2
            for _a in range(num):
                if pos + 4 > len(body):
                    break
                type_idx = struct.unpack_from('>H', body, pos)[0]
                if utf8.get(type_idx) == DISABLED_DESC:
                    disabled = True
                    break
                # Skip this annotation's element_value_pairs by walking them.
                pos = _skip_annotation(body, pos)
                if pos is None:
                    break
        off += 6 + length
    return flags, disabled


def _skip_annotation(body, pos):
    """Advance past one `annotation` structure starting at `pos`."""
    try:
        pos += 2                                   # type_index
        num_pairs = struct.unpack_from('>H', body, pos)[0]
        pos += 2
        for _ in range(num_pairs):
            pos += 2                               # element_name_index
            pos = _skip_element_value(body, pos)
        return pos
    except Exception:
        return None


def _skip_element_value(body, pos):
    tag = body[pos:pos + 1]
    pos += 1
    if tag in (b'B', b'C', b'D', b'F', b'I', b'J', b'S', b'Z', b's', b'c'):
        return pos + 2
    if tag == b'e':
        return pos + 4
    if tag == b'@':
        return _skip_annotation(body, pos)
    if tag == b'[':
        count = struct.unpack_from('>H', body, pos)[0]
        pos += 2
        for _ in range(count):
            pos = _skip_element_value(body, pos)
        return pos
    raise ValueError('unknown element_value tag %r' % tag)


def main():
    kept = dropped_shape = dropped_disabled = unresolved = 0
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
            unresolved += 1
            print(row)          # can't tell -> keep it, same as before
            kept += 1
            continue
        try:
            flags, disabled = inspect(path)
        except Exception as exc:
            print('WARN %s: %s' % (fqcn, exc), file=sys.stderr)
            unresolved += 1
            print(row)
            kept += 1
            continue
        if flags & (ACC_ABSTRACT | ACC_INTERFACE):
            dropped_shape += 1
            continue
        if disabled:
            dropped_disabled += 1
            continue
        print(row)
        kept += 1
    print('[is_concrete] kept=%d dropped_abstract_or_interface=%d dropped_disabled=%d '
          'unresolved=%d' % (kept, dropped_shape, dropped_disabled, unresolved),
          file=sys.stderr)


if __name__ == '__main__':
    main()
