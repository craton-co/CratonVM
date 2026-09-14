#!/usr/bin/env python3
"""Fail only on formatting the diff is responsible for.

`rustfmt --check` answers "is this whole file formatted?". CI's `fmt` job asked
that of every changed file, and 171 of this repository's 978 tracked `.rs` files
answer "no" — so the job failed on 33 of the last 60 non-merge commits for
formatting those commits did not introduce (see
`the-fmt-ci-job-was-red-on-half-of-all-pushes-RESOLVED-20260906.md`).
This asks the question the job's name implies instead: **are the lines this diff
touched formatted?**

Two behaviours of `rustfmt` make that harder than it sounds, and both are the
reason this is a script rather than three lines of `bash`:

1. **It follows `mod` declarations.** `rustfmt --check vm/src/runtime/
   interpreter.rs` reports hunks in `vm/src/runtime/interpreter/
   dispatch_static.rs`, a file the diff may never have opened. So a hunk is
   attributed by the path in its OWN header, not by the file that was passed in.
2. **It exits 1 for two unrelated reasons** — a formatting difference and a
   parse failure. `ci.yml` says so itself, which is why a separate parse job
   exists beside `fmt`. A parse failure here is reported separately and always
   fails, because it is the one thing a formatting gate must never swallow.

Inherited hunks are counted and printed rather than dropped in silence: the debt
stays visible, it just stops being attributed to whoever touched the file next.

Usage:

    tools/check_changed_rust_formatting.py <base> <head>

Locally, before pushing:

    tools/check_changed_rust_formatting.py origin/dev HEAD
"""

from __future__ import annotations

import os
import re
import subprocess
import sys

VENDOR_PREFIX = "native-builtins/vendor/"
EDITION = "2021"

DIFF_HEADER = re.compile(r"^Diff in (?P<path>.+):(?P<line>\d+):$")
HUNK_HEADER = re.compile(r"^@@ -\d+(?:,\d+)? \+(?P<start>\d+)(?:,(?P<count>\d+))? @@")


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(args, capture_output=True, text=True, **kwargs)
    except FileNotFoundError:
        sys.exit(
            "%s is not on PATH. CI installs it as the `rustfmt` component of "
            "`dtolnay/rust-toolchain`; locally, `rustup component add rustfmt`."
            % args[0]
        )


def repo_root() -> str:
    out = run(["git", "rev-parse", "--show-toplevel"])
    if out.returncode != 0:
        sys.exit("not inside a git repository: %s" % out.stderr.strip())
    return out.stdout.strip()


def changed_rust_files(base: str, head: str) -> list[str]:
    """Added/copied/modified/renamed `.rs` files, vendor excluded."""
    out = run(
        ["git", "diff", "--name-only", "--diff-filter=ACMR", base, head, "--", "*.rs"]
    )
    if out.returncode != 0:
        sys.exit("git diff failed: %s" % out.stderr.strip())
    return [
        line
        for line in out.stdout.splitlines()
        if line and not line.startswith(VENDOR_PREFIX)
    ]


def touched_ranges(base: str, head: str, path: str) -> list[tuple[int, int]]:
    """The line ranges of `path` AT HEAD that this diff added or changed.

    `-U0` so the ranges are the changes themselves and not three lines of
    innocent neighbours. A pure deletion (`+c,0`) leaves no new lines but does
    join two that were not adjacent before, so the joint counts as touched.
    """
    out = run(["git", "diff", "-U0", "--no-color", base, head, "--", path])
    if out.returncode != 0:
        sys.exit("git diff -U0 failed for %s: %s" % (path, out.stderr.strip()))
    ranges: list[tuple[int, int]] = []
    for line in out.stdout.splitlines():
        m = HUNK_HEADER.match(line)
        if not m:
            continue
        start = int(m.group("start"))
        count = int(m.group("count")) if m.group("count") is not None else 1
        if count == 0:
            ranges.append((max(1, start), start + 1))
        else:
            ranges.append((start, start + count - 1))
    return ranges


class Hunk:
    __slots__ = ("path", "start", "end", "body")

    def __init__(self, path: str, start: int, end: int, body: list[str]):
        self.path = path
        self.start = start
        self.end = end
        self.body = body

    @property
    def key(self) -> tuple[str, int, int]:
        return (self.path, self.start, self.end)

    def render(self) -> str:
        return "Diff in %s:%d:\n%s" % (self.path, self.start, "\n".join(self.body))


def parse_check_output(stdout: str, root: str) -> list[Hunk]:
    """Split `rustfmt --check` output into hunks, spanned in ORIGINAL lines.

    A body line that does not begin with `+` occupies a line of the file being
    checked; `+` lines are rustfmt's proposed additions and occupy none. An
    empty body line is a blank context line, so it counts — erring toward a
    LONGER span, which can only make a hunk more likely to be reported, which is
    the safe direction for a gate.
    """
    hunks: list[Hunk] = []
    path: str | None = None
    start = 0
    body: list[str] = []

    def flush() -> None:
        if path is None:
            return
        original = sum(1 for line in body if not line.startswith("+"))
        end = start + original - 1 if original else start
        hunks.append(Hunk(path, start, end, body))

    for line in stdout.splitlines():
        m = DIFF_HEADER.match(line)
        if m:
            flush()
            raw = m.group("path")
            path = os.path.relpath(raw, root) if os.path.isabs(raw) else raw
            path = path.replace(os.sep, "/")
            start = int(m.group("line"))
            body = []
            continue
        if path is not None:
            body.append(line)
    flush()
    return hunks


def overlaps(hunk: Hunk, ranges: list[tuple[int, int]]) -> bool:
    return any(hunk.start <= hi and lo <= hunk.end for lo, hi in ranges)


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        sys.exit("usage: %s <base> <head>" % os.path.basename(argv[0]))
    base, head = argv[1], argv[2]

    root = repo_root()
    os.chdir(root)

    files = changed_rust_files(base, head)
    if not files:
        print("No Rust files changed; nothing to check.")
        return 0

    print("Checking %d changed Rust file(s) against the lines they touch:" % len(files))
    for f in files:
        print("  %s" % f)

    ranges = {f: touched_ranges(base, head, f) for f in files}

    proc = run(["rustfmt", "--check", "--edition", EDITION] + files)

    # A parse failure is not a formatting opinion and must never be filtered.
    # `rustfmt` writes those to stderr; a clean run leaves it empty.
    if proc.stderr.strip():
        print("\nrustfmt reported an error, which is not a formatting difference:")
        print(proc.stderr.rstrip())
        print(
            "\nThis is the second thing a non-zero `rustfmt --check` can mean — most "
            "often a file that no longer parses. Fix it; it is never inherited debt."
        )
        return 1

    hunks = parse_check_output(proc.stdout, root)

    seen: set[tuple[str, int, int]] = set()
    attributed: list[Hunk] = []
    inherited: dict[str, int] = {}
    for hunk in hunks:
        if hunk.key in seen:
            continue
        seen.add(hunk.key)
        if overlaps(hunk, ranges.get(hunk.path, [])):
            attributed.append(hunk)
        else:
            inherited[hunk.path] = inherited.get(hunk.path, 0) + 1

    if inherited:
        total = sum(inherited.values())
        print(
            "\n%d formatting hunk(s) in %d file(s) are NOT attributable to this diff "
            "and are not failing it:" % (total, len(inherited))
        )
        for path in sorted(inherited):
            print("  %4d  %s" % (inherited[path], path))
        print(
            "  (pre-existing debt, or a `mod` sibling `rustfmt` reached from a file "
            "this diff did change — see\n"
            "   docs/known-issues/"
            "the-fmt-ci-job-is-red-on-half-of-all-pushes-20260906.md)"
        )

    if not attributed:
        print("\nEvery line this diff touched is formatted. OK.")
        return 0

    print(
        "\n%d formatting hunk(s) overlap lines this diff touched:\n" % len(attributed)
    )
    for hunk in attributed:
        print(hunk.render())
        print()
    print(
        "Run `rustfmt --edition %s` on the file(s) above, or hand-format just these "
        "hunks. Only the lines you touched are being asked about." % EDITION
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
