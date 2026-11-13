#!/usr/bin/env python3
"""Join an IR location census to one captured JIT instruction trace.

The capture directory contains summary.json, trace.txt and err, produced by
an address-level trace while CRATONVM_DBG_JITC and CRATONVM_DBG_IR_GRAPH are
set. Attempt markers separate abandoned compilations from accepted bodies.
If multiple accepted attempts reused an address, the capture cannot identify
which was executed and is rejected. Legacy captures without markers can be
read with --allow-legacy but cannot prove compilation identity. Byte ranges
distinguish data-node arms, block edges, other block code and code outside
blocks; prologues and deferred stubs remain outside the recorded blocks.
"""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re

LINE = re.compile(
    r"^\[ir-location\] base=(0x[0-9a-f]+) method=(.*?) block=(.*?) "
    r"node=(\d+) bci=(.*?) op=(.*?) rule=(.*?) bytes=(\d+)\.\.(\d+) "
    r"inputs=(.*?) result_slot=(.*?) result_reg=(.*?) result_live=(\w+) "
    r"home_dropped=(\w+)$"
)


MARKER = re.compile(r"^\[ir-location-(begin|end)\] base=(0x[0-9a-f]+) method=(.*)$")
EDGE = re.compile(r"^\[ir-location-edge\] base=(0x[0-9a-f]+) method=(.*?) block=(\d+) term=(.*?) op=(.*?) bytes=(\d+)\.\.(\d+)$")
BLOCK = re.compile(r"^\[ir-location-block\] base=(0x[0-9a-f]+) method=(.*?) block=(\d+) bytes=(\d+)\.\.(\d+)$")


def read_capture(directory, contains, allow_legacy=False):
    summary = json.loads((directory / "summary.json").read_text())
    ips = Counter(int(line, 16) for line in (directory / "trace.txt").read_text().splitlines())
    if sum(ips.values()) != summary["instructions"]:
        raise ValueError("trace length does not match its capture summary")
    records = defaultdict(list)
    active = {}
    accepted = defaultdict(list)
    reused_after_acceptance = set()
    lines = (directory / "err").read_text(errors="replace").splitlines()
    marked = any(line.startswith("[ir-location-begin]") for line in lines)
    if not marked and not allow_legacy:
        raise ValueError("capture has no attempt markers; pass --allow-legacy to accept ambiguity")
    for line in lines:
        marker = MARKER.fullmatch(line)
        if marker:
            kind, raw_base, method = marker.groups()
            base = int(raw_base, 16)
            if kind == "begin":
                # A new allocation can reuse an abandoned attempt's address.
                # Even a later failed attempt makes an earlier body's address
                # ambiguous without time-correlated code-load/unload records.
                if accepted[base]:
                    reused_after_acceptance.add(base)
                active[base] = (method, [])
            else:
                attempt = active.pop(base, None)
                if attempt is None or attempt[0] != method:
                    raise ValueError(f"unmatched attempt end at {base:#x}: {method}")
                accepted[base].append(attempt)
            continue
        if line.startswith("[ir-location-begin]") or line.startswith("[ir-location-end]"):
            raise ValueError(f"unrecognized attempt marker: {line[:200]}")
        edge = EDGE.fullmatch(line)
        block_span = BLOCK.fullmatch(line)
        if edge or block_span:
            fields = (edge or block_span).groups()
            raw_base, method, block = fields[:3]
            begin, end = map(int, fields[-2:])
            base = int(raw_base, 16)
            attempt = active.get(base)
            if not marked or attempt is None or attempt[0] != method:
                raise ValueError(f"block range outside its attempt at {base:#x}: {method}")
            if begin > end:
                raise ValueError(f"invalid block or edge range for {method}")
            row = {"kind": "edge" if edge else "block", "block": int(block),
                   "begin": begin, "end": end}
            if edge:
                row.update(term=fields[3], op=fields[4])
            attempt[1].append(row)
            continue
        if line.startswith("[ir-location-edge]") or line.startswith("[ir-location-block]"):
            raise ValueError(f"unrecognized block or edge record: {line[:200]}")
        match = LINE.fullmatch(line)
        if not match:
            if line.startswith("[ir-location]"):
                raise ValueError(f"unrecognized census record: {line[:200]}")
            continue
        base, method, block, node, bci, op, rule, begin, end, inputs, slot, reg, live, dropped = match.groups()
        begin, end = int(begin), int(end)
        if begin > end:
            raise ValueError(f"invalid range for {method} node {node}")
        base = int(base, 16)
        if marked:
            attempt = active.get(base)
            if attempt is None or attempt[0] != method:
                raise ValueError(f"census record outside its attempt at {base:#x}: {method}")
            rows = attempt[1]
        else:
            rows = records[base]
        rows.append({
            "kind": "node", "method": method, "block": block, "node": int(node), "bci": bci,
            "op": op, "rule": rule, "begin": begin, "end": end,
            "inputs": inputs, "result_slot": slot, "result_reg": reg,
            "result_live": live == "true", "home_dropped": dropped == "true",
        })
    bodies = []
    for region in summary["code"]:
        name, base, size = region["name"], int(region["start"], 16), region["size"]
        if contains not in name:
            continue
        if marked:
            attempts = accepted.get(base, [])
            if len(attempts) != 1:
                raise ValueError(f"expected one accepted attempt at {base:#x}, found {len(attempts)}: {name}")
            if base in reused_after_acceptance:
                raise ValueError(f"address reused after acceptance at {base:#x}: {name}")
            rows = attempts[0][1]
        else:
            rows = records.get(base, [])
        if not rows:
            raise ValueError(f"no census at captured body address {base:#x}: {name}")
        nodes = [r for r in rows if r["kind"] == "node"]
        edges = [r for r in rows if r["kind"] == "edge"]
        blocks = [r for r in rows if r["kind"] == "block"]
        spans = sorted((r["begin"], r["end"]) for r in nodes + edges if r["begin"] < r["end"])
        block_spans = sorted((r["begin"], r["end"]) for r in blocks if r["begin"] < r["end"])
        if any(end > size for _, end in spans + block_spans):
            raise ValueError(f"range exceeds captured body: {name}")
        if any(left[1] > right[0] for left, right in zip(spans, spans[1:])):
            raise ValueError(f"overlapping node or edge ranges: {name}")
        if any(left[1] > right[0] for left, right in zip(block_spans, block_spans[1:])):
            raise ValueError(f"overlapping block ranges: {name}")
        if blocks and any(not any(lo <= begin and end <= hi for lo, hi in block_spans)
                          for begin, end in spans):
            raise ValueError(f"node or edge range outside a block: {name}")
        executed = sum(count for ip, count in ips.items() if base <= ip < base + size)
        grouped = Counter()
        hot = []
        for row in nodes:
            count = sum(count for ip, count in ips.items() if base + row["begin"] <= ip < base + row["end"])
            if count:
                grouped[(row["op"], row["rule"])] += count
                hot.append({**row, "instructions": count})
        mapped = sum(grouped.values())
        edge_counts = Counter()
        for row in edges:
            edge_counts[row["op"]] += sum(count for ip, count in ips.items()
                                           if base + row["begin"] <= ip < base + row["end"])
        edge_total = sum(edge_counts.values())
        in_blocks = sum(count for ip, count in ips.items()
                        if any(base + lo <= ip < base + hi for lo, hi in block_spans))
        if blocks and mapped + edge_total > in_blocks:
            raise ValueError(f"mapped count exceeds block count: {name}")
        if mapped + edge_total > executed:
            raise ValueError(f"mapped count exceeds body count: {name}")
        bodies.append({
            "name": name, "base": hex(base), "size": size,
            "executed_instructions": executed, "mapped_instructions": mapped,
            "unmapped_instructions": executed - mapped,
            "edge_instructions": edge_total,
            "block_other_instructions": in_blocks - mapped - edge_total if blocks else None,
            "outside_blocks_instructions": executed - in_blocks if blocks else None,
            "by_edge": dict(edge_counts),
            "by_operation": [{"op": op, "rule": rule, "instructions": count}
                             for (op, rule), count in grouped.most_common()],
            "nodes": sorted(hot, key=lambda row: (-row["instructions"], row["node"])),
        })
    if not bodies:
        raise ValueError(f"no captured body matched {contains!r}")
    return {"capture": str(directory), "attempts_verified": marked,
            "trace_instructions": sum(ips.values()), "bodies": bodies}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path)
    parser.add_argument("--method", default="", help="substring of captured method name")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--allow-legacy", action="store_true", help="accept captures without attempt identity")
    args = parser.parse_args()
    result = read_capture(args.capture, args.method, args.allow_legacy)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    for body in result["bodies"]:
        print(f"{body['name']}: {body['executed_instructions']} total; "
              f"nodes={body['mapped_instructions']} edges={body['edge_instructions']} "
              f"other_block={body['block_other_instructions']} outside_blocks={body['outside_blocks_instructions']}")


if __name__ == "__main__":
    main()
