#!/usr/bin/env python3
"""Validate local Markdown links and mdBook SUMMARY membership.

External URLs are intentionally not fetched. The check is deterministic and
requires only Python's standard library.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from urllib.parse import unquote


LINK_RE = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)|!\[[^\]]*\]\(([^)]+)\)")
FENCE_RE = re.compile(r"^\s*(```|~~~)")


def markdown_files(root: Path) -> list[Path]:
    excluded = {".git", "target", "node_modules"}
    return sorted(
        path
        for path in root.rglob("*.md")
        if not any(part in excluded for part in path.parts)
    )


def links_in(path: Path) -> list[tuple[int, str]]:
    links: list[tuple[int, str]] = []
    in_fence = False
    marker = ""
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        fence = FENCE_RE.match(line)
        if fence:
            current = fence.group(1)
            if not in_fence:
                in_fence = True
                marker = current
            elif current == marker:
                in_fence = False
            continue
        if in_fence:
            continue
        for match in LINK_RE.finditer(line):
            target = match.group(1) or match.group(2)
            links.append((number, target.strip()))
    return links


def is_external(target: str) -> bool:
    lowered = target.lower()
    return (
        lowered.startswith(("http://", "https://", "mailto:", "data:"))
        or target.startswith("#")
        or "{{" in target
    )


def validate_links(root: Path, paths: list[Path]) -> list[str]:
    errors: list[str] = []
    for path in paths:
        for line, raw_target in links_in(path):
            target = raw_target.split(maxsplit=1)[0].strip("<>")
            if not target or is_external(target):
                continue
            target = unquote(target.split("#", 1)[0])
            if not target:
                continue
            resolved = (path.parent / target).resolve()
            try:
                resolved.relative_to(root.resolve())
            except ValueError:
                errors.append(
                    f"{path.relative_to(root)}:{line}: link escapes repository: {raw_target}"
                )
                continue
            if not resolved.exists():
                errors.append(
                    f"{path.relative_to(root)}:{line}: missing local target: {raw_target}"
                )
    return errors


def validate_summary(root: Path) -> list[str]:
    book_root = root / "docs" / "book" / "src"
    summary = book_root / "SUMMARY.md"
    if not summary.exists():
        return ["docs/book/src/SUMMARY.md: missing"]

    listed: set[Path] = set()
    for _, raw_target in links_in(summary):
        target = raw_target.split("#", 1)[0]
        if target.endswith(".md"):
            listed.add((summary.parent / target).resolve())

    errors: list[str] = []
    for page in sorted(book_root.rglob("*.md")):
        if page == summary:
            continue
        if page.resolve() not in listed:
            errors.append(
                f"{page.relative_to(root)}: book page is not listed in SUMMARY.md"
            )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--all",
        action="store_true",
        help="check every Markdown file; default checks maintained public docs",
    )
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    if args.all:
        paths = markdown_files(root)
    else:
        paths = sorted(
            {
                root / "README.md",
                root / "ARCHITECTURE.md",
                root / "BENCHMARK.md",
                *markdown_files(root / "docs" / "book" / "src"),
                *[p for p in (root / "docs").glob("*.md")],
            }
        )

    errors = validate_links(root, paths)
    errors.extend(validate_summary(root))
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        print(f"markdown documentation check failed: {len(errors)} issue(s)", file=sys.stderr)
        return 1

    print(f"validated {len(paths)} Markdown files and mdBook SUMMARY membership")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
