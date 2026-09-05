#!/usr/bin/env python3
"""Attribute an `hs_err` jump-table SIGSEGV to a source-level `match`, without
the crashing binary.

# Why this exists

A crash of the form

    EXCEPTION_ACCESS_VIOLATION ... at pc=<rip>
    Faulting access: read at address <addr>          where addr == r10 + rax*4

is the x86-64 jump-table idiom LLVM emits for a dense `match`: `r10` is the
table, `rax` the index, and the fault means `rax` was not a valid index --
i.e. an enum was constructed from memory that did not hold a valid
discriminant. See
`internal/fixed-bugs/hib-orm-json-xml-function-tests-segfault-g1-zgc-FIXED-20260901.md`.

Naming the function normally needs the exact binary: an RVA means nothing
without it, and symbolizing against a near-miss build produces a
plausible-looking and entirely wrong answer. When the crashing build is gone,
that route is closed.

This tool takes the other route. The crash handler dumps the memory around
`r10`, which *is the jump table*, and a jump table stores **table-relative**
displacements. So the differences between its entries are exactly the
differences between the code addresses they target -- and those are a property
of the function's own layout, not of where the image was based or how it was
linked. Two of the eight `hs_err` files this was written for come from
different builds with every RVA shifted, and their entry deltas are
byte-identical.

That delta vector is the fingerprint. Scan any *other* build's `.rdata` for a
run of dwords with the same deltas, and the hit names the same `match` -- in a
binary you still have, with a `.pdb` beside it.

# Usage

    python scripts/hs-err-jumptable-fingerprint.py \
        --hs-err apps/hib-suite-runner/hs_err_pid3512.log \
        --scan target/release/cratonvm.exe [more.exe ...]

`--fingerprint` takes a comma-separated delta vector directly, for a dump this
script cannot parse. `--min-run` trims the fingerprint to its last N entries
when the full vector does not hit (the entries before the table base belong to
a neighbouring table and can move independently).

Report a hit's `target RVA` to `CRATONVM_SYMBOLIZE` on the scanned binary, or
feed it to any PDB symbolizer, to get the function name.
"""

from __future__ import annotations

import argparse
import os
import re
import struct
import sys

# `[R10+0x8] = 0xFDF60E0DFDF60E38` -- one 64-bit qword, printed high-dword
# first, at signed offset `+0x8` from R10.
_MEM_LINE = re.compile(
    r"^\s*\[R10\+(0x[0-9a-fA-F]+)\]\s*=\s*0x([0-9a-fA-F]{16})\s*$"
)
_R10_LINE = re.compile(r"^Memory around R10 \(0x([0-9a-fA-F]+)\):")
_PC_LINE = re.compile(r"at pc=0x([0-9a-fA-F]+)")
_FAULT_LINE = re.compile(r"Faulting access: \w+ at address 0x([0-9a-fA-F]+)")


def _sign64(v: int) -> int:
    return v - (1 << 64) if v >= (1 << 63) else v


def _sign32(v: int) -> int:
    return v - (1 << 32) if v >= (1 << 31) else v


def parse_hs_err(path: str) -> dict:
    """Pull the R10 memory dump out of an `hs_err` log as an ordered dword run.

    Returns `{"r10", "pc", "fault", "dwords": [(offset_from_r10, value), ...]}`.
    """
    r10 = pc = fault = None
    dwords: dict[int, int] = {}
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if r10 is None:
                m = _R10_LINE.match(line)
                if m:
                    r10 = int(m.group(1), 16)
                    continue
            if pc is None:
                m = _PC_LINE.search(line)
                if m:
                    pc = int(m.group(1), 16)
            if fault is None:
                m = _FAULT_LINE.search(line)
                if m:
                    fault = int(m.group(1), 16)
            m = _MEM_LINE.match(line)
            if m:
                off = _sign64(int(m.group(1), 16))
                qword = int(m.group(2), 16)
                # Little-endian: the low dword lives at the lower address.
                dwords[off] = qword & 0xFFFFFFFF
                dwords[off + 4] = (qword >> 32) & 0xFFFFFFFF
    if r10 is None or not dwords:
        raise SystemExit(f"{path}: no 'Memory around R10' dump to read")
    ordered = sorted(dwords.items())
    return {"r10": r10, "pc": pc, "fault": fault, "dwords": ordered}


def fingerprint(dwords: list[tuple[int, int]], anchor_off: int = 0) -> list[int]:
    """Deltas of each dumped dword from the entry at `anchor_off` (the table
    base, i.e. index 0 of the faulting `match`).

    Table entries are table-relative displacements, so subtracting one from
    another cancels the base: the result is the distance between two code
    addresses inside one function, which survives rebasing and relinking.
    """
    by_off = dict(dwords)
    if anchor_off not in by_off:
        raise SystemExit(f"no dword at R10+{anchor_off:#x} in the dump")
    anchor = _sign32(by_off[anchor_off])
    return [_sign32(v) - anchor for _, v in dwords]


def pe_sections(data: bytes) -> list[dict]:
    """Minimal PE section table read: enough to map file offsets to RVAs."""
    if data[:2] != b"MZ":
        raise SystemExit("not a PE image (no MZ)")
    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_off : pe_off + 4] != b"PE\0\0":
        raise SystemExit("not a PE image (no PE signature)")
    coff = pe_off + 4
    num_sections = struct.unpack_from("<H", data, coff + 2)[0]
    opt_size = struct.unpack_from("<H", data, coff + 16)[0]
    sec_off = coff + 20 + opt_size
    out = []
    for i in range(num_sections):
        base = sec_off + i * 40
        name = data[base : base + 8].rstrip(b"\0").decode("ascii", "replace")
        virt_size = struct.unpack_from("<I", data, base + 8)[0]
        virt_addr = struct.unpack_from("<I", data, base + 12)[0]
        raw_size = struct.unpack_from("<I", data, base + 16)[0]
        raw_ptr = struct.unpack_from("<I", data, base + 20)[0]
        out.append(
            {
                "name": name,
                "rva": virt_addr,
                "vsize": virt_size,
                "raw": raw_ptr,
                "rsize": raw_size,
            }
        )
    return out


def scan(path: str, deltas: list[int], anchor_index: int) -> list[dict]:
    """Every place in `path` where a dword run has exactly these deltas."""
    with open(path, "rb") as fh:
        data = fh.read()
    sections = pe_sections(data)
    n = len(deltas)
    hits = []
    for sec in sections:
        # Jump tables live in read-only data; `.text` is scanned too because
        # some link configurations park them beside the code they serve.
        if sec["name"] not in (".rdata", ".text", ".data"):
            continue
        start = sec["raw"]
        end = min(sec["raw"] + sec["rsize"], len(data))
        if end - start < 4 * n:
            continue
        # One pass, keeping a rolling window of decoded dwords.
        count = (end - start) // 4
        vals = struct.unpack_from("<%dI" % count, data, start)
        signed = [_sign32(v) for v in vals]
        for i in range(count - n + 1):
            anchor = signed[i + anchor_index]
            ok = True
            for k in range(n):
                if signed[i + k] - anchor != deltas[k]:
                    ok = False
                    break
            if ok:
                table_file_off = start + (i + anchor_index) * 4
                table_rva = sec["rva"] + (table_file_off - sec["raw"])
                targets = [
                    (table_rva + signed[i + k]) & 0xFFFFFFFF for k in range(n)
                ]
                hits.append(
                    {
                        "section": sec["name"],
                        "table_rva": table_rva,
                        "file_off": table_file_off,
                        "targets": targets,
                    }
                )
    return hits


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--hs-err", help="hs_err log carrying the 'Memory around R10' dump")
    ap.add_argument(
        "--fingerprint",
        help="comma-separated delta vector, e.g. '0,0,0x2b,0,-0x106' "
        "(used instead of --hs-err)",
    )
    ap.add_argument(
        "--anchor-index",
        type=int,
        default=None,
        help="index within the vector that is the table base (default: the "
        "entry at R10+0 for --hs-err, 0 for --fingerprint)",
    )
    ap.add_argument(
        "--min-run",
        type=int,
        default=0,
        help="if the full vector misses, retry with its last N entries "
        "(0 = do not retry)",
    )
    ap.add_argument("--scan", nargs="+", required=True, help="PE images to search")
    args = ap.parse_args(argv)

    if args.fingerprint:
        deltas = [int(x, 0) for x in args.fingerprint.split(",")]
        anchor_index = 0 if args.anchor_index is None else args.anchor_index
        print(f"fingerprint: {len(deltas)} entries, anchor index {anchor_index}")
    elif args.hs_err:
        info = parse_hs_err(args.hs_err)
        deltas = fingerprint(info["dwords"])
        offs = [off for off, _ in info["dwords"]]
        anchor_index = offs.index(0)
        if args.anchor_index is not None:
            anchor_index = args.anchor_index
        print(f"{args.hs_err}:")
        print(f"  r10   = {info['r10']:#018x}")
        if info["pc"] is not None:
            print(f"  pc    = {info['pc']:#018x}")
        if info["fault"] is not None:
            print(f"  fault = {info['fault']:#018x}")
        print(
            "  table entries dumped: "
            + ", ".join(
                f"R10{off:+#x}={v:#010x}" for off, v in info["dwords"]
            )
        )
        print(
            "  fingerprint (deltas from R10+0): "
            + ", ".join(f"{d:+#x}" for d in deltas)
        )
    else:
        ap.error("one of --hs-err or --fingerprint is required")

    attempts = [(deltas, anchor_index)]
    if args.min_run and args.min_run < len(deltas):
        trimmed = deltas[-args.min_run :]
        trimmed_anchor = anchor_index - (len(deltas) - args.min_run)
        if trimmed_anchor >= 0:
            attempts.append((trimmed, trimmed_anchor))

    any_hit = False
    for vec, anchor in attempts:
        print(f"\n--- scanning with {len(vec)} entries (anchor {anchor}) ---")
        for image in args.scan:
            if not os.path.exists(image):
                print(f"  {image}: MISSING")
                continue
            try:
                hits = scan(image, vec, anchor)
            except SystemExit as exc:
                print(f"  {image}: {exc}")
                continue
            if not hits:
                print(f"  {image}: no match")
                continue
            any_hit = True
            print(f"  {image}: {len(hits)} match(es)")
            for h in hits:
                uniq = sorted(set(h["targets"]))
                print(
                    f"    section {h['section']} table_rva={h['table_rva']:#x} "
                    f"file_off={h['file_off']:#x}"
                )
                print(
                    "      targets: "
                    + ", ".join(f"{t:#x}" for t in h["targets"])
                )
                print(
                    f"      distinct arm bodies: {len(uniq)} -> "
                    + ", ".join(f"{t:#x}" for t in uniq)
                )
                print(
                    "      symbolize with: CRATONVM_SYMBOLIZE="
                    + ",".join(f"{t:#x}" for t in uniq)
                )
        if any_hit:
            break

    return 0 if any_hit else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
