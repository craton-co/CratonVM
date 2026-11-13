#!/usr/bin/env python3
"""Bucket leaf samples from `perf script -G --no-inline -F comm,period,ip,sym,dso`."""
import bisect
from collections import Counter
import json
from pathlib import Path
import re
import sys
import xml.etree.ElementTree as ET

SAMPLE = re.compile(r"^\s*(.*?)\s+(\d+)\s+([0-9a-f]+)\s+(.*)\s+\(([^()]*)\)\s*$")


def read_hotspot_code(path):
    ranges, counts = [], Counter()
    for _, element in ET.iterparse(path):
        if element.tag == "nmethod":
            tier = {"c1": "C1", "c2": "C2"}.get(element.get("compiler"), "runtime/native")
            start = int(element.attrib["address"], 16)
            ranges.append((start, start + int(element.attrib["size"]), tier))
            counts[tier] += 1
        element.clear()
    ranges.sort()
    return ranges, counts


class HotspotCode:
    def __init__(self, ranges):
        self.ranges = sorted(ranges)
        self.starts = [start for start, _, _ in self.ranges]
        self.ends = []
        for _, end, _ in self.ranges:
            self.ends.append(max(end, self.ends[-1] if self.ends else 0))

    def tier(self, ip):
        index = bisect.bisect_right(self.starts, ip) - 1
        tiers = set()
        while index >= 0 and self.ends[index] > ip:
            start, end, tier = self.ranges[index]
            if start <= ip < end:
                tiers.add(tier)
            index -= 1
        # Reused addresses of different tiers cannot be priced without lifetimes.
        return next(iter(tiers)) if len(tiers) == 1 else None


def thread_class(comm):
    if "Compiler" in comm or comm.startswith("cratonvm-jit-co"):
        return "compile"
    if "gc" in comm.lower() or comm.startswith("G1 "):
        return "GC"
    if comm in {"main-vm", "java", "cratonvm-fp", "env"}:
        return "main"
    if comm.startswith(("ForkJoinPool", "Thread-", "pool-")):
        return "test"
    return "other"


def execution_tier(comm, ip, symbol, dso, code):
    if thread_class(comm) == "compile":
        return "compiler"
    if "cratonvm_gc::" in symbol or thread_class(comm) == "GC":
        return "GC/barriers"
    if symbol.endswith(" [c1]"):
        return "C1"
    if symbol.endswith(" [c2]"):
        return "C2"
    if code is not None and (tier := code.tier(ip)) is not None:
        return tier
    if symbol == "Interpreter" or ("cratonvm_vm::runtime::interpreter::" in symbol
            and "::jit_bridge::" not in symbol):
        return "interpreter/dispatch"
    if "cratonvm_classloading::" in symbol or symbol.startswith((
            "ClassFileParser::", "SystemDictionary::", "Verifier::")):
        return "class loading/resolution"
    if symbol == "[unknown]" or dso == "[unknown]":
        return "unknown"
    if "perf-" in dso and dso.endswith(".map"):
        return "JIT/stub, tier unknown"
    return "runtime/native"


def attribute(lines, code=None):
    buckets, samples = Counter(), 0
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        match = SAMPLE.fullmatch(line.rstrip("\n"))
        if match is None:
            raise ValueError(f"Unparsed sample at line {line_number}: {line[:120]!r}")
        comm, period, ip, symbol, dso = match.groups()
        key = thread_class(comm), execution_tier(comm, int(ip, 16), symbol, dso, code)
        buckets[key] += int(period)
        samples += 1
    total = sum(buckets.values())
    if not total:
        raise ValueError("No weighted samples")
    return {"samples": samples, "sampled_instructions": total, "buckets": [
        {"thread": thread, "execution": tier, "instructions": count,
         "percent": round(100 * count / total, 4)}
        for (thread, tier), count in sorted(buckets.items())]}


def self_test():
    code = HotspotCode([(16, 32, "C1"), (24, 40, "C2")])
    assert [code.tier(ip) for ip in [15, 16, 24, 32, 40]] == [None, "C1", None, "C2", None]
    rows = [" Thread-1 7 10 body [c1] (/tmp/perf-1.map)\n",
            " cratonvm-jit-co 3 22 compiler::work (vm)\n"]
    result = attribute(rows)
    assert result["samples"] == 2 and result["sampled_instructions"] == 10
    assert sum(row["percent"] for row in result["buckets"]) == 100
    assert {row["execution"] for row in result["buckets"]} == {"C1", "compiler"}
    try:
        attribute(["not a perf sample"])
        raise AssertionError("malformed sample accepted")
    except ValueError:
        pass


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif len(sys.argv) in (2, 3):
        ranges, counts = read_hotspot_code(sys.argv[2]) if len(sys.argv) == 3 else ([], {})
        with Path(sys.argv[1]).open() as source:
            result = attribute(source, HotspotCode(ranges) if ranges else None)
        result["hotspot_nmethod_publications"] = counts
        print(json.dumps(result, indent=2))
    else:
        raise SystemExit("Usage: attribute-perf.py SAMPLES [HOTSPOT_LOG_XML] | --self-test")
