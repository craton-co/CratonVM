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


def gen_zip_entry() -> None:
    import io

    # Seed 1 — an ordinary JAR: a manifest, a class entry, and a resource.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(
            "META-INF/MANIFEST.MF",
            "Manifest-Version: 1.0\r\nMain-Class: com.example.Main\r\n"
            "Class-Path: lib/a.jar lib/b.jar\r\n\r\n",
        )
        z.writestr("com/example/Main.class", CLASS_MAGIC + JAVA_21 + u2(1) + b"\x00" * 16)
        z.writestr("com/example/data.txt", "hello\n")
    write("fuzz_zip_entry", "seed-plain-jar", buf.getvalue())

    # Seed 2 — traversal-shaped entry names. This is the seed the
    # enumeration oracle exists for: every one of these really is in the
    # central directory, so if the name filter regresses the target's
    # `find_class` / `find_resource` assertions fire immediately.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("../../../../etc/passwd.class", b"\x00")
        z.writestr("..\\..\\windows\\win.ini.class", b"\x00")
        z.writestr("/absolute/Escape.class", b"\x00")
        z.writestr("C:/drive/Letter.class", b"\x00")
        z.writestr("normal/./Dot.class", b"\x00")
        z.writestr("META-INF/../../outside.txt", b"\x00")
        # Placeholder for an entry name carrying invalid UTF-8; `zipfile`
        # refuses to encode one directly, so the two `XX` bytes are patched
        # out below. The replacement is the same length, so every local
        # header and central-directory offset stays valid.
        z.writestr("bad/XXname.class", b"\x00")
    raw = buf.getvalue().replace(b"bad/XXname.class", b"bad/\xff\xfename.class")
    write("fuzz_zip_entry", "seed-traversal-names", raw)

    # Seed 3 — a modest compression bomb: 4 MiB of zeros in one stored-name
    # entry, deflated down to a couple of kilobytes. Well under the
    # 512 MiB clamp, but it drives the streaming-inflate accounting.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        z.writestr("bomb/Zeros.class", b"\x00" * (4 * 1024 * 1024))
    write("fuzz_zip_entry", "seed-ratio-bomb", buf.getvalue())

    # Seed 4 — truncated archive: a valid central directory with its tail
    # cut off, so EOCD discovery must fail cleanly.
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("a/B.class", b"\x00" * 64)
        z.writestr("a/C.class", b"\x00" * 64)
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


def main() -> None:
    print(f"writing seeds under {ROOT}")
    gen_constant_pool()
    gen_attribute_nesting()
    gen_jni_descriptor()
    gen_jfr_chunk()
    gen_zip_entry()
    gen_signed_jar()
    print("done")


if __name__ == "__main__":
    main()
