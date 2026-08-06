#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Rewrite one method's ``max_stack`` in a class file.

The positive control for a bytecode-verifier sweep. A sweep that reports "every
class verified" is indistinguishable from a sweep whose verifier never ran, so
before believing a green run, hand it a class file that must be rejected and
confirm it is.

Lowering ``max_stack`` below what the body needs is the cleanest such defect:
it changes exactly two bytes, leaves the StackMapTable and every offset intact,
and the rejection names the precise offset at which the declared limit is first
exceeded -- so the control also tells you what a real under-declared
``max_stack`` looks like in the VM's diagnostic.

Usage:
    patch_max_stack.py <in.class> <out.class> <method-name> <descriptor> <new-max-stack>
"""

import struct
import sys

# Constant-pool tag -> (payload byte length, index slots consumed).
# Utf8 (1) is variable-length and handled separately.
CP_SHAPES = {
    3: (4, 1),   # Integer
    4: (4, 1),   # Float
    5: (8, 2),   # Long
    6: (8, 2),   # Double
    7: (2, 1),   # Class
    8: (2, 1),   # String
    9: (4, 1),   # Fieldref
    10: (4, 1),  # Methodref
    11: (4, 1),  # InterfaceMethodref
    12: (4, 1),  # NameAndType
    15: (3, 1),  # MethodHandle
    16: (2, 1),  # MethodType
    17: (4, 1),  # Dynamic
    18: (4, 1),  # InvokeDynamic
    19: (2, 1),  # Module
    20: (2, 1),  # Package
}


def parse_constant_pool(b, pos):
    """Return (utf8_by_index, position just past the pool)."""
    count = struct.unpack_from(">H", b, pos)[0]
    pos += 2
    utf8 = {}
    idx = 1
    while idx < count:
        tag = b[pos]
        pos += 1
        if tag == 1:
            length = struct.unpack_from(">H", b, pos)[0]
            pos += 2
            utf8[idx] = b[pos:pos + length].decode("utf-8", "replace")
            pos += length
            slots = 1
        else:
            payload, slots = CP_SHAPES[tag]
            pos += payload
        idx += slots
    return utf8, pos


def skip_attributes(b, pos):
    count = struct.unpack_from(">H", b, pos)[0]
    pos += 2
    for _ in range(count):
        pos += 2  # name index
        length = struct.unpack_from(">I", b, pos)[0]
        pos += 4 + length
    return pos


def skip_fields(b, pos):
    count = struct.unpack_from(">H", b, pos)[0]
    pos += 2
    for _ in range(count):
        pos += 6  # access, name, descriptor
        pos = skip_attributes(b, pos)
    return pos


def patch(data, want_name, want_desc, new_max_stack):
    b = bytearray(data)
    assert b[0:4] == b"\xca\xfe\xba\xbe", "not a class file"
    pos = 8
    utf8, pos = parse_constant_pool(b, pos)
    pos += 6  # access_flags, this_class, super_class
    iface_count = struct.unpack_from(">H", b, pos)[0]
    pos += 2 + 2 * iface_count
    pos = skip_fields(b, pos)

    method_count = struct.unpack_from(">H", b, pos)[0]
    pos += 2
    for _ in range(method_count):
        pos += 2  # access_flags
        name = utf8[struct.unpack_from(">H", b, pos)[0]]
        pos += 2
        desc = utf8[struct.unpack_from(">H", b, pos)[0]]
        pos += 2
        attr_count = struct.unpack_from(">H", b, pos)[0]
        pos += 2
        for _ in range(attr_count):
            attr_name = utf8[struct.unpack_from(">H", b, pos)[0]]
            pos += 2
            attr_len = struct.unpack_from(">I", b, pos)[0]
            pos += 4
            if attr_name == "Code" and name == want_name and desc == want_desc:
                was = struct.unpack_from(">H", b, pos)[0]
                struct.pack_into(">H", b, pos, new_max_stack)
                return bytes(b), was
            pos += attr_len
    raise SystemExit("no Code attribute for {}{}".format(want_name, want_desc))


def main():
    if len(sys.argv) != 6:
        raise SystemExit(__doc__)
    src, dst, name, desc, new_max = sys.argv[1:]
    with open(src, "rb") as f:
        data = f.read()
    out, was = patch(data, name, desc, int(new_max))
    # Encode first, write second: a failed write must not leave a truncated
    # class file behind for the next run to load.
    with open(dst, "wb") as f:
        f.write(out)
    print("{}{}: max_stack {} -> {}".format(name, desc, was, new_max))


if __name__ == "__main__":
    main()
