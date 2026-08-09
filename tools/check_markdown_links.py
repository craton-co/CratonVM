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


def markdown_files(root: Path, excluded: set[str] | None = None) -> list[Path]:
    excluded = {".git", "target", "node_modules"} | (excluded or set())
    return sorted(
        path
        for path in root.rglob("*.md")
        if not any(part in excluded for part in path.parts)
    )


def links_in(path: Path) -> list[tuple[int, str]]:
    links: list[tuple[int, str]] = []
    in_fence = False
    marker = ""
    # The internal archive contains a few pre-migration Windows-1252 bytes.
    # Replacement decoding keeps --all useful for link auditing without making
    # archived encoding cleanup a prerequisite for checking maintained docs.
    text = path.read_text(encoding="utf-8", errors="replace")
    for number, line in enumerate(text.splitlines(), 1):
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
        help=(
            "audit every Markdown file in the repo, including the dated "
            "internal archive and vendored trees; the default scope is "
            "root *.md plus docs/** excluding the internal archive"
        ),
    )
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    if args.all:
        paths = markdown_files(root)
    else:
        # Default (CI) scope: root-level Markdown plus everything under docs/
        # recursively, minus the internal archive (`docs` + `/internal`).
        #
        # That archive is dated material (~396 broken links), and
        # most of those links point at documents that were intentionally
        # deleted once the work they described landed. Gating CI on it would
        # produce pure noise and would pressure people into resurrecting dead
        # files just to make the check green. Every other subtree under docs/
        # is maintained and is gated here. Use --all for the
        # everything-including-internal audit mode.
        paths = sorted(
            {
                *[p for p in root.glob("*.md")],
                *markdown_files(root / "docs", excluded={"internal"}),
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
