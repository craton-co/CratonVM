#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""Make a longer division chain from one of the committed bench sources.

    gen-chain.py <source.java> <statements> <out-dir>

`GpuDivChain` / `GpuFloatDivChain` and their TornadoVM twins hand-unroll
`x = x / d + C;` 48 or 64 times. The brief wants longer chains to widen
the GPU/CPU gap, and all three arms (HotSpot, CratonVM, TornadoVM) must
run the SAME chain or the comparison is meaningless. So this rewrites the
committed file rather than keeping a second, hand-edited copy that can
drift from it: every line made only of `x = x / d + C;` statements is
replaced by one block of exactly <statements> of them, and the class is
renamed `<Class><statements>`.

Refuses a file with no such lines, or with two different constants,
instead of guessing.
"""
import pathlib
import re
import sys

STMT = re.compile(r"x = x / d \+ ([0-9.]+);")


def rewrite(text: str, k: int) -> str:
    lines = text.splitlines()
    chain = [i for i, ln in enumerate(lines) if ln.strip() and not STMT.sub("", ln).strip()]
    if not chain:
        raise SystemExit("no `x = x / d + C;` lines: not a div-chain source")
    consts = {m for i in chain for m in STMT.findall(lines[i])}
    if len(consts) != 1:
        raise SystemExit(f"expected one constant in the chain, found {sorted(consts)}")
    c = consts.pop()
    indent = re.match(r"\s*", lines[chain[0]]).group(0)
    stmt = f"x = x / d + {c};"
    block = [
        indent + "  ".join([stmt] * min(3, k - i))
        for i in range(0, k, 3)
    ]
    # Replace the first chain line with the block and drop the rest. The
    # chain is contiguous in every committed source; assert that rather
    # than silently splice across unrelated code.
    assert chain == list(range(chain[0], chain[0] + len(chain))), "chain is not contiguous"
    return "\n".join(lines[: chain[0]] + block + lines[chain[-1] + 1 :]) + "\n"


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    src, k, out = pathlib.Path(sys.argv[1]), int(sys.argv[2]), pathlib.Path(sys.argv[3])
    if k < 1:
        raise SystemExit("statements must be >= 1")
    cls = src.stem
    text = rewrite(src.read_text(), k)
    new = f"{cls}{k}"
    text = re.sub(rf"\b{cls}\b", new, text)
    out.mkdir(parents=True, exist_ok=True)
    (out / f"{new}.java").write_text(text)
    print(f"{out / (new + '.java')}  ({k} statements)")


if __name__ == "__main__":
    main()
