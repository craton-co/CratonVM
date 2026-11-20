#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
"""Run one exact-test shard of the types Miri tests skipped by `--skip flags::`."""

import argparse
import subprocess
import sys

CARGO = ["cargo", "+nightly", "miri", "test", "--locked", "-p", "cratonvm-types", "--lib"]


def selected_tests(listing: str, shard: int, shards: int) -> list[str]:
    names = [line.removesuffix(": test") for line in listing.splitlines() if line.endswith(": test")]
    selected = sorted(name for name in names if "flags::" in name)
    if not selected or len(selected) != len(set(selected)):
        raise ValueError("Miri listed no flag tests or duplicate test names")
    if not 0 <= shard < shards <= len(selected):
        raise ValueError("invalid shard number or too many shards")
    return selected[shard::shards]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("shard", type=int)
    parser.add_argument("shards", type=int)
    args = parser.parse_args()
    listing = subprocess.run(CARGO + ["--", "--list"], text=True, capture_output=True)
    if listing.returncode:
        sys.stderr.write(listing.stderr)
        return listing.returncode
    try:
        tests = selected_tests(listing.stdout, args.shard, args.shards)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 1
    failed = False
    for name in tests:
        print(f"Miri shard {args.shard}/{args.shards}: {name}", flush=True)
        failed |= not run_one(name)
    return int(failed)


def run_one(name: str) -> bool:
    process = subprocess.Popen(
        CARGO + [name, "--", "--exact"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    assert process.stdout is not None
    ran_one = False
    for line in process.stdout:
        print(line, end="", flush=True)
        ran_one |= line.strip() == "running 1 test"
    status = process.wait()
    if not ran_one:
        print(f"Miri did not select exactly one test: {name}", file=sys.stderr)
    return status == 0 and ran_one


if __name__ == "__main__":
    sys.exit(main())
