#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Regenerate the hand-built seed corpora under ``fuzz/corpus/``.

Every seed this script writes is a *structurally valid* example of the
input layout its target expects, so libFuzzer starts from a shape that
already reaches deep code rather than from random noise. The seeds are
committed, so this script only needs re-running when a target's per-input
layout changes.

Usage (from anywhere):

    python fuzz/corpus/gen_seeds.py

It writes into ``<this file's directory>/<target>/`` and is idempotent.
No third-party imports — only the standard library.
"""

from __future__ import annotations

import pathlib
import struct
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

CLASS_MAGIC = b"\xca\xfe\xba\xbe"
JAVA_21 = b"\x00\x00\x00\x41"  # minor = 0, major = 65
JFR_MAGIC = b"FLR\x00"


def write(target: str, name: str, data: bytes) -> None:
    d = ROOT / target
    d.mkdir(parents=True, exist_ok=True)
    (d / name).write_bytes(data)
    print(f"  {target}/{name}  ({len(data)} bytes)")


def u1(v: int) -> bytes:
    return struct.pack(">B", v & 0xFF)


def u2(v: int) -> bytes:
    return struct.pack(">H", v & 0xFFFF)


def u4(v: int) -> bytes:
    return struct.pack(">I", v & 0xFFFFFFFF)


def utf8(s: str) -> bytes:
    """CONSTANT_Utf8_info: tag 1, u2 length, bytes."""
    b = s.encode("utf-8")
    return b"\x01" + u2(len(b)) + b


def varint(v: int) -> bytes:
    """JFR compressed int (LEB128 variant, 7 payload bits per byte)."""
    out = bytearray()
    while True:
        byte = v & 0x7F
        v >>= 7
        if v:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def zigzag(v: int) -> bytes:
    """JFR compressed long: zigzag then LEB128."""
    return varint(((v << 1) ^ (v >> 63)) & 0xFFFFFFFFFFFFFFFF)


# ---------------------------------------------------------------------------
# fuzz_constant_pool
#
# Layout: this_class u2 | super_class u2 | raw constant pool.
# ---------------------------------------------------------------------------


def gen_constant_pool() -> None:
    # Seed 1 — a coherent pool whose this_class/super_class resolve, so
    # `read_class` succeeds and every ConstantPool accessor assertion runs.
    #   #1 Utf8 "Seed"        #2 Class -> #1
    #   #3 Utf8 "java/lang/Object"   #4 Class -> #3
    #   #5 Utf8 "value"       #6 Utf8 "I"    #7 NameAndType #5:#6
    #   #8 Fieldref #2:#7
    pool = (
        u2(9)
        + utf8("Seed")
        + b"\x07" + u2(1)
        + utf8("java/lang/Object")
        + b"\x07" + u2(3)
        + utf8("value")
        + utf8("I")
        + b"\x0c" + u2(5) + u2(6)
        + b"\x09" + u2(2) + u2(7)
    )
    write("fuzz_constant_pool", "seed-coherent-pool", u2(2) + u2(4) + pool)

    # Seed 2 — every category-2 and cross-reference shape in one pool,
    # including a Long/Double pair (two slots each), MethodHandle,
    # InvokeDynamic, Module and Package. Exercises the tombstone rule and
    # the widest part of `validate()`.
    #   #1 Utf8 "Seed2"  #2 Class -> #1
    #   #3 Long (occupies #3,#4)   #5 Double (occupies #5,#6)
    #   #7 Utf8 "m"  #8 Utf8 "()V"  #9 NameAndType #7:#8
    #   #10 Methodref #2:#9   #11 MethodHandle kind=6 -> #10
    #   #12 InvokeDynamic bsm=0 -> #9   #13 Module -> #1  #14 Package -> #1
    #   #15 String -> #1  #16 MethodType -> #8  #17 Integer  #18 Float
    pool = (
        u2(19)
        + utf8("Seed2")
        + b"\x07" + u2(1)
        + b"\x05" + struct.pack(">q", -1)
        + b"\x06" + struct.pack(">d", 1.5)
        + utf8("m")
        + utf8("()V")
        + b"\x0c" + u2(7) + u2(8)
        + b"\x0a" + u2(2) + u2(9)
        + b"\x0f" + u1(6) + u2(10)
        + b"\x12" + u2(0) + u2(9)
        + b"\x13" + u2(1)
        + b"\x14" + u2(1)
        + b"\x08" + u2(1)
        + b"\x10" + u2(8)
        + b"\x03" + struct.pack(">i", -2147483648)
        + b"\x04" + struct.pack(">f", 0.5)
    )
    write("fuzz_constant_pool", "seed-category2-and-refs", u2(2) + u2(2) + pool)

    # Seed 3 — a declared count of 65535 backed by three bytes. This is the
    # resource-exhaustion shape: the reader must fail without allocating a
    # 65535-entry pool.
    write("fuzz_constant_pool", "seed-absurd-count", u2(1) + u2(1) + u2(0xFFFF) + b"\x01\x00")


# ---------------------------------------------------------------------------
# fuzz_attribute_nesting
#
# Layout: selector byte | raw attribute body.
# ---------------------------------------------------------------------------


def code_body(inner: bytes | None, name_index: int = 1) -> bytes:
    body = u2(1) + u2(1) + u4(1) + b"\xb1" + u2(0)
    if inner is None:
        return body + u2(0)
    return body + u2(1) + u2(name_index) + u4(len(inner)) + inner


def gen_attribute_nesting() -> None:
    # Selector 0 -> "Code". A three-deep Code-in-Code body: valid, well
    # inside MAX_ATTRIBUTE_DEPTH, and exactly the shape the fuzzer should
    # deepen.
    body = code_body(None)
    for _ in range(3):
        body = code_body(body)
    write("fuzz_attribute_nesting", "seed-code-in-code", b"\x00" + body)

    # Selector 2 -> "RuntimeVisibleAnnotations". num_annotations = 1, then
    # four levels of `@`-nested element values.
    ann = bytearray(u2(1))  # num_annotations
    for _ in range(4):
        ann += u2(1) + u2(1) + u2(1) + b"@"
    ann += u2(1) + u2(0)  # innermost annotation, no pairs
    write("fuzz_attribute_nesting", "seed-nested-annotations", b"\x02" + bytes(ann))

    # Selector 4 -> "RuntimeVisibleTypeAnnotations". target_type 0x13 has an
    # empty target_info; a 3-entry type_path precedes the annotation.
    ta = bytearray(u2(1))  # num_annotations
    ta += b"\x13"  # target_type
    ta += b"\x03" + b"\x00\x00" + b"\x01\x00" + b"\x02\x01"  # type_path
    ta += u2(1) + u2(1) + u2(1) + b"["  # element_value: array
    ta += u2(2) + b"I" + u2(1) + b"@"  # two values: const int, nested annotation
    ta += u2(1) + u2(0)  # the nested annotation
    write("fuzz_attribute_nesting", "seed-type-annotation-path", b"\x04" + bytes(ta))


# ---------------------------------------------------------------------------
# fuzz_jni_descriptor
#
# Layout: selector byte | scale byte | free-form descriptor text.
# Both leading bytes are chosen printable so the file stays readable.
# ---------------------------------------------------------------------------


def gen_jni_descriptor() -> None:
    write(
        "fuzz_jni_descriptor",
        "seed-method-descriptor",
        b"\x09\x10(Ljava/lang/String;[[IJZ)Ljava/util/Map;",
    )
    write(
        "fuzz_jni_descriptor",
        "seed-generic-signature",
        b"\x01\x08<T:Ljava/lang/Object;>Ljava/util/List<TT;>;Ljava/io/Serializable;",
    )
    # Deep-but-legal array nesting plus an unterminated L-descriptor tail.
    write(
        "fuzz_jni_descriptor",
        "seed-array-nesting",
        b"\x0b\xff" + b"[" * 200 + b"Ljava/lang/Object;",
    )


# ---------------------------------------------------------------------------
# fuzz_jfr_chunk
#
# Layout: the chunk body; the harness prepends the 4-byte FLR\0 magic.
# So a seed is a whole .jfr file MINUS its magic.
# ---------------------------------------------------------------------------

JFR_HEADER_SIZE = 72


def jfr_body(checkpoint_offset: int, metadata_offset: int, events: bytes,
             minor: int = 1) -> bytes:
    """Everything after the 4 magic bytes, laid out per jfr/src/dump.rs."""
    head = bytearray()
    head += u2(2) + u2(minor)                     # major, minor
    head += struct.pack(">Q", JFR_HEADER_SIZE + len(events))  # file_size
    head += struct.pack(">Q", checkpoint_offset)
    head += struct.pack(">Q", metadata_offset)
    head += struct.pack(">Q", 1_000_000_000)      # start_time_ns
    head += struct.pack(">Q", 1_000_000)          # duration_ns
    head += struct.pack(">Q", 0)                  # start_ticks
    head += struct.pack(">Q", 1_000_000_000)      # ticks_per_second
    head += b"\x00"                               # file_state
    # Pad out to HEADER_SIZE (minus the 4 magic bytes the harness adds).
    head += b"\x00" * (JFR_HEADER_SIZE - 4 - len(head))
    return bytes(head) + events


def gen_jfr_chunk() -> None:
    # Seed 1 — one well-formed event record of registry type id 1
    # ("FuzzScalar": long, int, boolean, float, double). Record layout is
    # size | type_id | start_time | duration | thread_id | fields.
    fields = (
        zigzag(1234)                # long
        + zigzag(-7)                # int
        + b"\x01"                   # boolean
        + struct.pack(">f", 2.5)    # float
        + struct.pack(">d", -0.25)  # double
    )
    inner = varint(1) + zigzag(0) + zigzag(500) + zigzag(9) + fields
    # `size` counts itself; 1 byte is enough for any record below 128 bytes.
    record = varint(len(inner) + 1) + inner
    body = jfr_body(
        checkpoint_offset=JFR_HEADER_SIZE + len(record),
        metadata_offset=JFR_HEADER_SIZE + len(record),
        events=record,
    )
    write("fuzz_jfr_chunk", "seed-one-scalar-event", body)

    # Seed 2 — header whose checkpoint/metadata offsets point past EOF.
    # Both must be rejected before any slice is taken.
    body = jfr_body(
        checkpoint_offset=0xFFFF_FFFF_FFFF_FFFF,
        metadata_offset=0xFFFF_FFFF_FFFF_FFFF,
        events=b"",
    )
    write("fuzz_jfr_chunk", "seed-offsets-past-eof", body)

    # Seed 3 — a record whose declared size is a 10-byte varint of u64::MAX,
    # the integer-overflow shape the `checked_add` guard exists for.
    record = varint(0xFFFF_FFFF_FFFF_FFFF) + varint(1) + zigzag(0) + zigzag(0) + zigzag(0)
    body = jfr_body(
        checkpoint_offset=JFR_HEADER_SIZE + len(record),
        metadata_offset=JFR_HEADER_SIZE + len(record),
        events=record,
    )
    write("fuzz_jfr_chunk", "seed-record-size-overflow", body)


# ---------------------------------------------------------------------------
# fuzz_zip_entry
#
# Layout: the raw archive bytes.
# ---------------------------------------------------------------------------


# A fixed DOS timestamp for every generated ZIP entry.
#
# `ZipFile.writestr(name, data)` builds its `ZipInfo` from
# `time.localtime()`, which makes the output differ on every run — the
# seeds were being rewritten byte-for-byte-differently each time the
# generator ran, contradicting this script's own idempotency claim and
# making "regenerate and `git diff --exit-code`" unusable as a CI check.
# Constructing the `ZipInfo` explicitly pins it.
ZIP_EPOCH = (2026, 1, 1, 0, 0, 0)


def zip_entry(name: str, compress_type: int = zipfile.ZIP_DEFLATED) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(name, date_time=ZIP_EPOCH)
    info.compress_type = compress_type
    info.create_system = 0  # MS-DOS, so the host OS does not leak in either.
    return info


def gen_zip_entry() -> None:
    import io

    # Seed 1 — an ordinary JAR: a manifest, a class entry, and a resource.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(
            zip_entry("META-INF/MANIFEST.MF"),
            "Manifest-Version: 1.0\r\nMain-Class: com.example.Main\r\n"
            "Class-Path: lib/a.jar lib/b.jar\r\n\r\n",
        )
        z.writestr(zip_entry("com/example/Main.class"), CLASS_MAGIC + JAVA_21 + u2(1) + b"\x00" * 16)
        z.writestr(zip_entry("com/example/data.txt"), "hello\n")
    write("fuzz_zip_entry", "seed-plain-jar", buf.getvalue())

    # Seed 2 — traversal-shaped entry names. This is the seed the
    # enumeration oracle exists for: every one of these really is in the
    # central directory, so if the name filter regresses the target's
    # `find_class` / `find_resource` assertions fire immediately.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(zip_entry("../../../../etc/passwd.class"), b"\x00")
        z.writestr(zip_entry("..\\..\\windows\\win.ini.class"), b"\x00")
        z.writestr(zip_entry("/absolute/Escape.class"), b"\x00")
        z.writestr(zip_entry("C:/drive/Letter.class"), b"\x00")
        z.writestr(zip_entry("normal/./Dot.class"), b"\x00")
        z.writestr(zip_entry("META-INF/../../outside.txt"), b"\x00")
        # Placeholder for an entry name carrying invalid UTF-8; `zipfile`
        # refuses to encode one directly, so the two `XX` bytes are patched
        # out below. The replacement is the same length, so every local
        # header and central-directory offset stays valid.
        z.writestr(zip_entry("bad/XXname.class"), b"\x00")
    raw = buf.getvalue().replace(b"bad/XXname.class", b"bad/\xff\xfename.class")
    write("fuzz_zip_entry", "seed-traversal-names", raw)

    # Seed 3 — a modest compression bomb: 4 MiB of zeros in one stored-name
    # entry, deflated down to a couple of kilobytes. Well under the
    # 512 MiB clamp, but it drives the streaming-inflate accounting.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        z.writestr(zip_entry("bomb/Zeros.class"), b"\x00" * (4 * 1024 * 1024))
    write("fuzz_zip_entry", "seed-ratio-bomb", buf.getvalue())

    # Seed 4 — truncated archive: a valid central directory with its tail
    # cut off, so EOCD discovery must fail cleanly.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(zip_entry("a/B.class"), b"\x00" * 64)
        z.writestr(zip_entry("a/C.class"), b"\x00" * 64)
    raw = buf.getvalue()
    write("fuzz_zip_entry", "seed-truncated", raw[: len(raw) - 24])


# ---------------------------------------------------------------------------
# fuzz_signed_jar
#
# Layout: selector | manifest_len u2 | sf_len u2 | manifest | sf | block.
# ---------------------------------------------------------------------------


def signed_jar_seed(selector: int, manifest: bytes, sf: bytes, block: bytes) -> bytes:
    return u1(selector) + u2(len(manifest)) + u2(len(sf)) + manifest + sf + block


def gen_signed_jar() -> None:
    manifest = (
        b"Manifest-Version: 1.0\r\n"
        b"Created-By: 21 (Craton)\r\n"
        b"\r\n"
        b"Name: com/example/Main.class\r\n"
        b"SHA-256-Digest: 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\r\n"
        b"\r\n"
        b"Name: com/example/data.txt\r\n"
        b"SHA1-Digest: 2jmj7l5rSw0yVb/vlWAYkK/YBwk=\r\n"
        b"\r\n"
    )
    sf = (
        b"Signature-Version: 1.0\r\n"
        b"SHA-256-Digest-Manifest: 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\r\n"
        b"Created-By: 21 (Craton)\r\n"
        b"\r\n"
        b"Name: com/example/Main.class\r\n"
        b"SHA-256-Digest: 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\r\n"
        b"\r\n"
    )
    # A minimal, deliberately-truncated PKCS#7 ContentInfo: SEQUENCE {
    # OID 1.2.840.113549.1.7.2 (signedData), [0] EXPLICIT { SEQUENCE { ...
    # } } }. It parses as far as the SignedData body and then runs out —
    # exactly the boundary the fuzzer should push on.
    signed_data_oid = bytes([0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02])
    inner = bytes([0x30, 0x06, 0x02, 0x01, 0x01, 0x31, 0x01, 0x00])  # SEQUENCE{INTEGER,SET}
    ctx0 = bytes([0xA0, len(inner)]) + inner
    payload = signed_data_oid + ctx0
    block = bytes([0x30, len(payload)]) + payload
    write("fuzz_signed_jar", "seed-pkcs7-signeddata", signed_jar_seed(0, manifest, sf, block))

    # Seed 2 — unsupported digest algorithm plus a malformed base64 value.
    # `parse_manifest_entry_digests` must drop both without producing an
    # entry, and `verify_sf_binds_manifest` must stay fail-closed.
    manifest2 = (
        b"Manifest-Version: 1.0\r\n"
        b"\r\n"
        b"Name: a/B.class\r\n"
        b"MD5-Digest: not+valid+base64===\r\n"
        b"SHA-999-Digest: AAAA\r\n"
        b"\r\n"
        b"Name: " + b"x" * 5000 + b"\r\n"       # past the 4 KiB name cap
        b"SHA-256-Digest: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n"
        b"\r\n"
    )
    sf2 = (
        b"Signature-Version: 1.0\r\n"
        b"SHA-256-Digest-Manifest-Main-Attributes: AAAA\r\n"  # NOT a substitute
        b"\r\n"
    )
    write("fuzz_signed_jar", "seed-bad-algorithms", signed_jar_seed(1, manifest2, sf2, b"\x30\x00"))

    # Seed 3 — a PEM-shaped anchor bundle in the signer-block slot
    # (selector odd routes to `load_pem_bundle`).
    pem = (
        b"-----BEGIN CERTIFICATE-----\n"
        b"MIIBhTCCASugAwIBAgIQAAAAAAAAAAAAAAAAAAAAADAKBggqhkjOPQQDAjAA\n"
        b"-----END CERTIFICATE-----\n"
        b"-----BEGIN CERTIFICATE-----\n"
        b"bm90IGEgY2VydGlmaWNhdGU=\n"
        b"-----END CERTIFICATE-----\n"
    )
    write("fuzz_signed_jar", "seed-pem-bundle", signed_jar_seed(1, manifest, sf, pem))


# ---------------------------------------------------------------------------
# Whole `.class` files
#
# Four targets take a raw class file with no framing at all:
#   fuzz_classfile, fuzz_read_class, fuzz_verifier, difftest_bytecode.
#
# The builder below mirrors `reader/tests/mutation_harness.rs::valid_class`
# byte for byte in structure (constant pool, one `Code` method with a
# non-empty exception table, nested LineNumberTable + StackMapTable, a
# class-level SourceFile). That fixture is already known to parse, and the
# mutation harness drives it through `force_decode_all` + `verified_code`
# on every commit — so a seed built the same way is known-good rather than
# hoped-good.
# ---------------------------------------------------------------------------


def class_attr(name_index: int, body: bytes) -> bytes:
    return u2(name_index) + u4(len(body)) + body


def valid_class(major: int = 52, extra_class_attrs: bytes = b"",
                extra_attr_count: int = 0, code_bytes: bytes = b"\xb1\xb1\xb1\xb1",
                stack_map: bytes | None = None) -> bytes:
    """A structurally valid class file `T` with one method `void m()`.

    Constant pool (14 slots declared, 13 real):
      #1 Utf8 "java/lang/Object"     #2 Class -> #1
      #3 Utf8 "m"                    #4 Utf8 "()V"
      #5 Utf8 "Code"                 #6 Utf8 "java/lang/Exception"
      #7 Class -> #6                 #8 Utf8 "LineNumberTable"
      #9 Utf8 "StackMapTable"        #10 Utf8 "SourceFile"
      #11 Utf8 "T.java"              #12 Utf8 "T"   #13 Class -> #12
    """
    d = bytearray(CLASS_MAGIC)
    d += u2(0) + u2(major)
    d += u2(14)
    d += utf8("java/lang/Object")          # 1
    d += b"\x07" + u2(1)                   # 2
    d += utf8("m")                         # 3
    d += utf8("()V")                       # 4
    d += utf8("Code")                      # 5
    d += utf8("java/lang/Exception")       # 6
    d += b"\x07" + u2(6)                   # 7
    d += utf8("LineNumberTable")           # 8
    d += utf8("StackMapTable")             # 9
    d += utf8("SourceFile")                # 10
    d += utf8("T.java")                    # 11
    d += utf8("T")                         # 12
    d += b"\x07" + u2(12)                  # 13

    d += u2(0x0021)  # ACC_PUBLIC | ACC_SUPER
    d += u2(13)      # this_class  -> T
    d += u2(2)       # super_class -> java/lang/Object
    d += u2(0)       # interfaces_count
    d += u2(0)       # fields_count

    lnt = u2(1) + u2(0) + u2(1)
    smt = stack_map if stack_map is not None else (u2(1) + b"\x00")

    code = bytearray()
    code += u2(2) + u2(1)                  # max_stack, max_locals
    code += u4(len(code_bytes)) + code_bytes
    code += u2(1) + u2(0) + u2(2) + u2(2) + u2(7)   # one exception-table row
    code += u2(2)
    code += class_attr(8, lnt)
    code += class_attr(9, smt)

    d += u2(1)                             # methods_count
    d += u2(0x0001) + u2(3) + u2(4)        # ACC_PUBLIC, name "m", desc "()V"
    d += u2(1)                             # method attributes_count
    d += class_attr(5, bytes(code))

    d += u2(1 + extra_attr_count)
    d += class_attr(10, u2(11))            # SourceFile -> "T.java"
    d += extra_class_attrs
    return bytes(d)


def gen_class_file_targets() -> None:
    baseline = valid_class()

    # A switch-heavy body: `tableswitch` and `lookupswitch` are the two
    # alignment-padded instructions, and their padding depends on the
    # instruction's absolute pc — the classic off-by-one site.
    #   pc 0 : tableswitch, 3 pad bytes, default/low/high, 2 jump offsets
    #   pc 24: lookupswitch, 3 pad bytes, default/npairs, 1 pair
    ts = b"\xaa" + b"\x00\x00\x00" + u4(20) + u4(0) + u4(1) + u4(16) + u4(20)
    ls = b"\xab" + b"\x00\x00\x00" + u4(12) + u4(1) + u4(7) + u4(12)
    switchy = ts + ls + b"\xb1"
    assert len(ts) == 24, len(ts)

    # StackMapTable exercising all five frame families plus verification
    # types Object (cp index) and Uninitialized (bytecode offset).
    smt = bytearray(u2(5))
    smt += b"\x00"                                   # same_frame, delta 0
    smt += b"\x40" + b"\x07" + u2(13)                # same_locals_1_stack_item, Object #13
    smt += b"\xf8" + u2(3)                           # chop_frame (251+3-k), delta 3
    smt += b"\xfc" + u2(1) + b"\x01"                 # append_frame (+1 local: Integer)
    smt += (b"\xff" + u2(2)                          # full_frame
            + u2(2) + b"\x01" + b"\x08" + u2(0)      #   locals: Integer, Uninitialized@0
            + u2(1) + b"\x07" + u2(2))               #   stack:  Object #2

    # fuzz_classfile: raw class bytes, eager + every lazy attribute decoder.
    write("fuzz_classfile", "seed-valid-class", baseline)
    write("fuzz_classfile", "seed-switch-code", valid_class(code_bytes=switchy))
    write("fuzz_classfile", "seed-stack-map-families",
          valid_class(stack_map=bytes(smt)))
    # A Java-21 class file: the version gate is a distinct branch and the
    # mutation harness only ever exercises major 52.
    write("fuzz_classfile", "seed-java21", valid_class(major=65))

    # fuzz_read_class: same grammar, different entry point (`read_class_arc`).
    # Kept as its own copy so `cargo fuzz cmin` can prune the two corpora
    # independently — they diverge as soon as either target grows.
    write("fuzz_read_class", "seed-valid-class", baseline)
    write("fuzz_read_class", "seed-switch-code", valid_class(code_bytes=switchy))

    # fuzz_verifier: `define_class` bails immediately unless `this_class`
    # resolves, so an unparseable seed teaches the fuzzer nothing.
    write("fuzz_verifier", "seed-valid-class", baseline)
    write("fuzz_verifier", "seed-switch-code", valid_class(code_bytes=switchy))

    # difftest_bytecode: the mutator walks the constant pool looking for
    # Integer/Float/Long/Double entries to rewrite in place. The baseline
    # pool has none, so a seed with all four is what makes the target's
    # length-preservation assertion reachable at all.
    numeric_pool = (
        u2(9)
        + utf8("T")
        + b"\x07" + u2(1)
        + b"\x03" + struct.pack(">i", 42)          # Integer
        + b"\x04" + struct.pack(">f", 1.5)         # Float
        + b"\x05" + struct.pack(">q", -1)          # Long   (2 slots: #5,#6)
        + b"\x06" + struct.pack(">d", 0.25)        # Double (2 slots: #7,#8)
    )
    numeric_class = (
        CLASS_MAGIC + u2(0) + u2(52) + numeric_pool
        + u2(0x0021) + u2(2) + u2(2) + u2(0) + u2(0) + u2(0) + u2(0)
    )
    write("difftest_bytecode", "seed-numeric-constants", numeric_class)
    write("difftest_bytecode", "seed-valid-class", baseline)


# ---------------------------------------------------------------------------
# fuzz_stack_map
#
# Layout: the raw `StackMapTable` attribute *body* — everything after
# `attribute_name_index` and `attribute_length`.
# ---------------------------------------------------------------------------


def gen_stack_map() -> None:
    frames = bytearray(u2(5))
    frames += b"\x00"                                 # same_frame
    frames += b"\x40" + b"\x07" + u2(1)               # same_locals_1_stack_item
    frames += b"\xf8" + u2(3)                         # chop_frame
    frames += b"\xfc" + u2(1) + b"\x01"               # append_frame
    frames += (b"\xff" + u2(2) + u2(2) + b"\x01" + b"\x08" + u2(0)
               + u2(1) + b"\x07" + u2(2))             # full_frame
    write("fuzz_stack_map", "seed-all-frame-kinds", bytes(frames))

    # `absolute_offsets` sums `offset_delta + 1` across frames. Four
    # near-maximal deltas overshoot u16 — the additive-overflow site the
    # target's docstring names. Must be `Err`, never a wrap or a panic.
    overflow = bytearray(u2(4))
    for _ in range(4):
        overflow += b"\xfb" + u2(0xFFFF)              # chop_frame, delta 65535
    write("fuzz_stack_map", "seed-offset-delta-overflow", bytes(overflow))

    # A declared count of 65535 backed by two bytes: the anti-OOM shape.
    write("fuzz_stack_map", "seed-absurd-entry-count", u2(0xFFFF) + b"\x00\x00")

    # `append_frame` tag 252..254 declares 1..3 extra locals; declaring three
    # with none behind them must fail rather than over-read.
    write("fuzz_stack_map", "seed-unbacked-append", u2(1) + b"\xfe" + u2(0))


# ---------------------------------------------------------------------------
# fuzz_instruction
#
# Layout: the whole input is one bytecode array, walked from pc 0.
# ---------------------------------------------------------------------------


def gen_instruction() -> None:
    # Both alignment-padded switches, at pcs whose padding differs.
    ts = b"\xaa" + b"\x00\x00\x00" + u4(20) + u4(0) + u4(1) + u4(16) + u4(20)
    ls = b"\xab" + b"\x00\x00\x00" + u4(12) + u4(1) + u4(7) + u4(12)
    write("fuzz_instruction", "seed-switches", ts + ls + b"\xb1")

    # `wide` in both of its forms: the 4-byte local-index form and the
    # 6-byte `iinc` form. Different operand widths behind the same prefix.
    wide = (b"\xc4\x15" + u2(300)          # wide iload  #300
            + b"\xc4\x36" + u2(300)        # wide istore #300
            + b"\xc4\x84" + u2(300) + u2(0xFFFF)   # wide iinc #300, -1
            + b"\xb1")
    write("fuzz_instruction", "seed-wide-forms", wide)

    # A spread of fixed-width operand shapes: constants, branches, field and
    # method references, `invokedynamic` (with its two reserved zero bytes),
    # `invokeinterface` (count + reserved byte), `multianewarray`, `newarray`.
    mixed = (b"\x10\x2a"                   # bipush 42
             + b"\x11" + u2(1000)          # sipush 1000
             + b"\x12\x01"                 # ldc #1
             + b"\x13" + u2(1)             # ldc_w #1
             + b"\x14" + u2(1)             # ldc2_w #1
             + b"\xbc\x0a"                 # newarray int
             + b"\xc5" + u2(1) + b"\x03"   # multianewarray #1, 3
             + b"\xb4" + u2(1)             # getfield #1
             + b"\xb6" + u2(1)             # invokevirtual #1
             + b"\xb9" + u2(1) + b"\x01\x00"        # invokeinterface #1, 1, 0
             + b"\xba" + u2(1) + b"\x00\x00"        # invokedynamic #1, 0, 0
             + b"\xa7" + u2(0xFFFD)        # goto -3
             + b"\xc8" + u4(0xFFFFFFF9)    # goto_w -7
             + b"\xb1")
    write("fuzz_instruction", "seed-operand-shapes", mixed)

    # A truncated `tableswitch`: the header claims 65536 jump offsets that
    # are not there. The decoder must not believe `high - low`.
    write("fuzz_instruction", "seed-truncated-tableswitch",
          b"\xaa" + b"\x00\x00\x00" + u4(8) + u4(0) + u4(0xFFFF))


# ---------------------------------------------------------------------------
# fuzz_descriptor
#
# Layout: the whole input, lossily decoded as UTF-8, fed to every
# descriptor / signature parser.
# ---------------------------------------------------------------------------


def gen_descriptor() -> None:
    write("fuzz_descriptor", "seed-method-descriptor",
          b"(Ljava/lang/String;[[IJZDF)Ljava/util/Map;")
    write("fuzz_descriptor", "seed-class-signature",
          b"<K:Ljava/lang/Object;V::Ljava/lang/Comparable<TV;>;>"
          b"Ljava/util/AbstractMap<TK;TV;>;Ljava/io/Serializable;")
    write("fuzz_descriptor", "seed-method-signature",
          b"<T:Ljava/lang/Object;>(Ljava/util/List<+TT;>;[TT;)"
          b"Ljava/util/Map<TT;*>;^Ljava/lang/Exception;^TT;")
    write("fuzz_descriptor", "seed-field-signature",
          b"Ljava/util/Map<Ljava/lang/String;Ljava/util/List<[TT;>;>.Entry;")
    # Modified-UTF8 surrogate pair and a lone high surrogate: the CESU-8
    # shapes that reach `from_utf8_lossy`'s replacement path.
    write("fuzz_descriptor", "seed-modified-utf8",
          b"L\xed\xa0\xbd\xed\xb8\x80/\xed\xa0\x80a;")


# ---------------------------------------------------------------------------
# fuzz_jimage
#
# Layout: a whole jimage container. Mirrors
# `reader/src/jimage::test_builder::build_simple` — 28-byte little-endian
# header, redirect table, offset table, locations buffer (offset 0 is a
# reserved sentinel), NUL-terminated strings buffer, resource data.
# ---------------------------------------------------------------------------

JIMAGE_MAGIC_LE = struct.pack("<I", 0xCAFEDADA)
JIMAGE_VERSION = (1 << 16) | 0
JIMAGE_HEADER_SIZE = 28
JIMAGE_HASH_MULTIPLIER = 0x0100_0193


def jimage_hash(path: str, seed: int) -> int:
    """`ImageStrings::hash_code` — wrapping i32 multiply-then-xor FNV."""
    h = seed
    for b in path.encode("utf-8"):
        h = ((h * JIMAGE_HASH_MULTIPLIER) & 0xFFFFFFFF) ^ b
    return h & 0x7FFFFFFF


def jimage_attr(kind: int, value: int) -> bytes:
    """`header_byte = (kind << 3) | (len - 1)`, then big-endian value."""
    raw = value.to_bytes(8, "big").lstrip(b"\x00") or b"\x00"
    return bytes([(kind << 3) | (len(raw) - 1)]) + raw


def build_jimage(resources: list[tuple[str, str, str, str, bytes]]) -> bytes:
    strings = bytearray(b"\x00")          # offset 0 = the empty string
    interned: dict[str, int] = {"": 0}

    def sref(s: str) -> int:
        if s in interned:
            return interned[s]
        off = len(strings)
        strings.extend(s.encode("utf-8"))
        strings.append(0)
        interned[s] = off
        return off

    locations = bytearray(b"\x00")        # offset 0 is the reserved sentinel
    rdata = bytearray()
    entries: list[tuple[str, int]] = []
    for module, parent, base, ext, payload in resources:
        mo, po, bo, eo = sref(module), sref(parent), sref(base), sref(ext)
        res_off, res_len = len(rdata), len(payload)
        rdata += payload
        loc_start = len(locations)
        locations += jimage_attr(1, mo)
        locations += jimage_attr(2, po)
        locations += jimage_attr(3, bo)
        locations += jimage_attr(4, eo)
        locations += jimage_attr(5, res_off)
        locations += jimage_attr(7, res_len)
        locations += b"\x00"              # END
        path = ""
        if module:
            path += "/" + module
        if parent:
            path += "/" + parent
        if base:
            path += "/" + base
        if ext:
            path += "." + ext
        entries.append((path, loc_start))

    # Single-step (direct) assignment: `redirect[primary] = -(slot + 1)`,
    # which the reader decodes as `slot = -redirect - 1`. Distinct primaries
    # are asserted rather than searched for — the seeds are fixed, so a
    # collision would be a build-time failure here, not a silent bad seed.
    table_len = max(len(entries) * 4, 16)
    redirect = [0] * table_len
    offsets = [0] * table_len
    for slot, (path, loc_start) in enumerate(entries):
        primary = jimage_hash(path, JIMAGE_HASH_MULTIPLIER) % table_len
        assert redirect[primary] == 0, f"seed hash collision on {path!r}"
        redirect[primary] = -(slot + 1)
        offsets[slot] = loc_start

    head = bytearray(JIMAGE_MAGIC_LE)
    head += struct.pack("<I", JIMAGE_VERSION)
    head += struct.pack("<I", 0)                  # flags
    head += struct.pack("<I", len(entries))       # resource_count
    head += struct.pack("<I", table_len)
    head += struct.pack("<I", len(locations))
    head += struct.pack("<I", len(strings))
    assert len(head) == JIMAGE_HEADER_SIZE, len(head)

    body = bytearray()
    for v in redirect:
        body += struct.pack("<i", v)
    for v in offsets:
        body += struct.pack("<I", v)
    body += locations
    body += strings
    body += rdata
    return bytes(head) + bytes(body)


def gen_jimage() -> None:
    # A two-resource image whose paths the reader can round-trip. This is
    # the seed that makes `iter_entries` and the *positive* `find_resource`
    # lookup reachable — from random bytes the fuzzer would essentially
    # never build a header that survives `from_bytes`.
    img = build_jimage([
        ("java.base", "java/lang", "Object", "class",
         CLASS_MAGIC + JAVA_21 + u2(1) + b"\x00" * 16),
        ("java.base", "java/util", "Map", "class", b"\x00" * 8),
    ])
    write("fuzz_jimage", "seed-two-resources", img)

    # The same image with `table_length` rewritten to 0xFFFFFFFF: the
    # section-offset arithmetic must fail closed instead of computing a
    # redirect table larger than the file.
    absurd = bytearray(img)
    absurd[16:20] = struct.pack("<I", 0xFFFFFFFF)
    write("fuzz_jimage", "seed-absurd-table-length", bytes(absurd))

    # Header only, everything after it cut away.
    write("fuzz_jimage", "seed-header-only", img[:JIMAGE_HEADER_SIZE])


# ---------------------------------------------------------------------------
# fuzz_asn1
#
# Layout: selector byte (`% 5` picks the decoder) | DER payload.
#   0 read_header  1 read_oid  2 decode_algorithm_identifier
#   3 decode_subject_public_key_info + decode_extensions
#   4 x509_manager::parse_certificate
# ---------------------------------------------------------------------------


def der(tag: int, content: bytes) -> bytes:
    """DER TLV with the short/long length forms picked as the encoder must."""
    n = len(content)
    if n < 0x80:
        length = bytes([n])
    else:
        raw = n.to_bytes((n.bit_length() + 7) // 8, "big")
        length = bytes([0x80 | len(raw)]) + raw
    return bytes([tag]) + length + content


# `ecPublicKey` (1.2.840.10045.2.1) and `prime256v1` (1.2.840.10045.3.1.7).
OID_EC_PUBLIC_KEY = bytes([0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01])
OID_PRIME256V1 = bytes([0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07])
# `sha256WithRSAEncryption` (1.2.840.113549.1.1.11).
OID_SHA256_RSA = bytes(
    [0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B]
)


def gen_asn1() -> None:
    # Selector 0 — a long-form length header (two length bytes).
    write("fuzz_asn1", "seed-long-form-length", b"\x00" + der(0x04, b"A" * 300))

    # Selector 1 — an OID whose first arc packs two components into one byte
    # and whose later arcs are multi-byte base-128.
    write("fuzz_asn1", "seed-oid", b"\x01" + OID_SHA256_RSA[2:])

    # Selector 2 — AlgorithmIdentifier ::= SEQUENCE { OID, params }.
    alg = der(0x30, OID_EC_PUBLIC_KEY + OID_PRIME256V1)
    write("fuzz_asn1", "seed-algorithm-identifier", b"\x02" + alg)

    # Selector 3 — SubjectPublicKeyInfo ::= SEQUENCE { alg, BIT STRING }.
    spki = der(0x30, alg + der(0x03, b"\x00\x04" + b"\x2a" * 64))
    write("fuzz_asn1", "seed-spki", b"\x03" + spki)

    # Selector 3 again — Extensions: a SEQUENCE OF Extension carrying
    # basicConstraints (critical) and keyUsage.
    basic = der(0x30, bytes([0x06, 0x03, 0x55, 0x1D, 0x13])
                + der(0x01, b"\xff")
                + der(0x04, der(0x30, der(0x01, b"\xff"))))
    key_usage = der(0x30, bytes([0x06, 0x03, 0x55, 0x1D, 0x0F])
                    + der(0x04, der(0x03, b"\x01\x86")))
    write("fuzz_asn1", "seed-extensions", b"\x03" + der(0x30, basic + key_usage))

    # Selector 4 — a Certificate shell: SEQUENCE { tbsCertificate,
    # signatureAlgorithm, signatureValue }, with a tbs carrying version,
    # serial, issuer/subject RDNs, validity and an SPKI. Deliberately
    # unsigned garbage in `signatureValue`: the parser must get all the way
    # through the structure and then reject, which is the longer path.
    rdn = der(0x31, der(0x30, bytes([0x06, 0x03, 0x55, 0x04, 0x03])
                        + der(0x0C, b"seed")))
    name = der(0x30, rdn)
    validity = der(0x30, der(0x17, b"260101000000Z") + der(0x17, b"360101000000Z"))
    tbs = der(0x30,
              der(0xA0, der(0x02, b"\x02"))          # [0] version v3
              + der(0x02, b"\x01")                   # serialNumber
              + alg + name + validity + name + spki
              + der(0xA3, der(0x30, basic + key_usage)))  # [3] extensions
    cert = der(0x30, tbs + alg + der(0x03, b"\x00" + b"\x5a" * 64))
    write("fuzz_asn1", "seed-certificate", b"\x04" + cert)


# ---------------------------------------------------------------------------
# fuzz_keystore
#
# Layout: selector byte (`% 3`) | password-length byte | password | body.
#   0 load_jks   1 load_pkcs12   2 load_keystore (magic sniffer)
# ---------------------------------------------------------------------------


def keystore_seed(selector: int, password: bytes, body: bytes) -> bytes:
    return u1(selector) + u1(len(password)) + password + body


def gen_keystore() -> None:
    # A JKS body: magic FEEDFEED, version 2, one private-key alias record,
    # then a 20-byte HMAC-SHA1 trailer that will not verify. Reaching the
    # HMAC check means every record above it parsed.
    jks = bytearray(struct.pack(">I", 0xFEEDFEED) + u4(2) + u4(1))
    jks += u4(1)                                     # tag: private key entry
    alias = b"seed"
    jks += u2(len(alias)) + alias
    jks += struct.pack(">q", 1_700_000_000_000)      # creation date (ms)
    encrypted = b"\x30\x0d" + b"\x00" * 11
    jks += u4(len(encrypted)) + encrypted
    jks += u4(1)                                     # chain length
    certtype = b"X.509"
    jks += u2(len(certtype)) + certtype
    certbody = b"\x30\x03\x02\x01\x00"
    jks += u4(len(certbody)) + certbody
    jks += b"\x00" * 20                              # HMAC-SHA1 trailer
    write("fuzz_keystore", "seed-jks", keystore_seed(0, b"changeit", bytes(jks)))
    # The same bytes through the sniffer, which must route on FEEDFEED.
    write("fuzz_keystore", "seed-jks-sniffed", keystore_seed(2, b"", bytes(jks)))

    # A PKCS#12 shell: PFX ::= SEQUENCE { version, authSafe ContentInfo,
    # macData }. Structurally a p12 down to the MAC, then empty.
    data_oid = bytes([0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x01])
    auth_safe = der(0x30, data_oid + der(0xA0, der(0x04, der(0x30, b""))))
    mac_alg = der(0x30, bytes([0x06, 0x05, 0x2B, 0x0E, 0x03, 0x02, 0x1A]) + b"\x05\x00")
    mac_data = der(0x30,
                   der(0x30, mac_alg + der(0x04, b"\x00" * 20))
                   + der(0x04, b"\x01" * 8)
                   + der(0x02, b"\x08\x00"))
    pfx = der(0x30, der(0x02, b"\x03") + auth_safe + mac_data)
    write("fuzz_keystore", "seed-pkcs12", keystore_seed(1, b"changeit", pfx))
    write("fuzz_keystore", "seed-pkcs12-sniffed", keystore_seed(2, b"changeit", pfx))

    # Neither magic: the sniffer must reject rather than guess.
    write("fuzz_keystore", "seed-unknown-magic",
          keystore_seed(2, b"", b"\xde\xad\xbe\xef" + b"\x00" * 32))


# ---------------------------------------------------------------------------
# fuzz_tls_record
#
# Layout: raw bytes off the wire — `TLSPlaintext` is
# `u1 content_type | u2 legacy_version | u2 length | fragment`.
# ---------------------------------------------------------------------------


def tls_record(content_type: int, fragment: bytes, version: int = 0x0303,
               declared_len: int | None = None) -> bytes:
    n = len(fragment) if declared_len is None else declared_len
    return bytes([content_type]) + u2(version) + u2(n) + fragment


def gen_tls_record() -> None:
    # A handshake record carrying a ClientHello-shaped body. Content type 22
    # is the one that matters: it is the first thing a server ever decodes.
    hello = (b"\x01" + b"\x00\x00\x2c"          # handshake: client_hello, len
             + u2(0x0303) + b"\x11" * 32        # legacy_version, random
             + b"\x00"                          # legacy_session_id (empty)
             + u2(2) + b"\x13\x01"              # cipher_suites
             + b"\x01\x00"                      # compression_methods
             + u2(0))                           # extensions (empty)
    write("fuzz_tls_record", "seed-handshake", tls_record(22, hello))

    # Application data at exactly the 16384-byte spec ceiling: the
    # boundary the default record layer clamps at.
    write("fuzz_tls_record", "seed-max-fragment",
          tls_record(23, b"\x5a" * 16384))

    # A declared length of 0xFFFF with nothing behind it — the framing
    # over-read shape, and past the 16 KiB clamp in both configurations.
    write("fuzz_tls_record", "seed-length-overrun",
          tls_record(23, b"", declared_len=0xFFFF))

    # Two records back to back; the decoder must consume exactly the first.
    write("fuzz_tls_record", "seed-two-records",
          tls_record(21, b"\x01\x00") + tls_record(23, b"payload"))

    # An unknown content type with a legal frame: must be rejected on the
    # type, not on the framing.
    write("fuzz_tls_record", "seed-unknown-content-type",
          tls_record(0xFF, b"\x00\x01\x02\x03"))


def main() -> None:
    print(f"writing seeds under {ROOT}")
    gen_constant_pool()
    gen_attribute_nesting()
    gen_jni_descriptor()
    gen_jfr_chunk()
    gen_zip_entry()
    gen_signed_jar()
    gen_class_file_targets()
    gen_stack_map()
    gen_instruction()
    gen_descriptor()
    gen_jimage()
    gen_asn1()
    gen_keystore()
    gen_tls_record()
    print("done")


if __name__ == "__main__":
    main()
