#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""Validate local Markdown links and mdBook SUMMARY membership.

External URLs are intentionally not fetched. The check is deterministic and
requires only Python's standard library.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import unquote


LINK_RE = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)|!\[[^\]]*\]\(([^)]+)\)")
FENCE_RE = re.compile(r"^\s*(```|~~~)")
DATE_RE = re.compile(r"(?<!\d)(?:20\d{2}-\d{2}-\d{2}|20\d{6})(?!\d)")


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
        or "{{" in target
    )


def heading_anchors(path: Path) -> set[str]:
    anchors: set[str] = set()
    in_fence = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if FENCE_RE.match(line):
            in_fence = not in_fence
        if in_fence:
            continue
        heading = re.match(r"^#{1,6}\s+(.+?)\s*#*\s*$", line)
        if heading:
            label = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", heading.group(1))
            slug = re.sub(r"[^\w\- ]", "", label.lower()).replace(" ", "-")
            candidate = slug
            suffix = 1
            while candidate in anchors:
                candidate = f"{slug}-{suffix}"
                suffix += 1
            anchors.add(candidate)
        anchors.update(re.findall(r'<a\s+(?:id|name)=["\x27]([^"\x27]+)', line))
    return anchors


def validate_links(root: Path, paths: list[Path]) -> list[str]:
    errors: list[str] = []
    for path in paths:
        for line, raw_target in links_in(path):
            target = raw_target.split(maxsplit=1)[0].strip("<>")
            if not target or is_external(target):
                continue
            target, _, fragment = unquote(target).partition("#")
            resolved = (path.parent / target).resolve() if target else path.resolve()
            try:
                resolved.relative_to(root.resolve())
            except ValueError:
                errors.append(
                    f"{path.relative_to(root)}:{line}: link escapes repository: {raw_target}"
                )
                continue
            if resolved.is_relative_to((root / "docs" / "internal").resolve()):
                errors.append(
                    f"{path.relative_to(root)}:{line}: public link reaches private archive: {raw_target}"
                )
                continue
            if not resolved.exists():
                errors.append(
                    f"{path.relative_to(root)}:{line}: missing local target: {raw_target}"
                )
            elif fragment and resolved.suffix.lower() == ".md" and fragment not in heading_anchors(resolved):
                errors.append(
                    f"{path.relative_to(root)}:{line}: missing heading: {raw_target}"
                )
    return errors


def validate_public_text(root: Path, paths: list[Path]) -> list[str]:
    errors = []
    for path in paths:
        relative = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        if "--jdk" + "-only" in text:
            errors.append(f"{relative}: use the canonical --real-jdk spelling")
        # Third-party legal notices must retain their original text.
        if relative.startswith("docs/known-issues/") or path.name == "THIRD-PARTY-NOTICES.md":
            continue
        if DATE_RE.search(relative):
            errors.append(f"{relative}: dated public document name")
        for line, value in enumerate(text.splitlines(), 1):
            if DATE_RE.search(value):
                errors.append(f"{relative}:{line}: date outside an actionable known issue")
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
        help="also check untracked public Markdown; private archives and vendored trees stay excluded",
    )
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    names = subprocess.check_output(
        ["git", "-C", str(root), "ls-files", "-z"], text=True
    ).split("\0")
    paths = sorted({
        root / name for name in names
        if name.lower().endswith(".md")
        and not name.startswith(("docs/internal/", "apps/"))
        and "/vendor/" not in name
        and (root / name).is_file()
    })
    if args.all:
        paths = sorted(set(paths) | set(markdown_files(
            root, excluded={"internal", "vendor", "apps", ".bench-cache"}
        )))

    errors = validate_links(root, paths)
    errors.extend(validate_public_text(root, paths))
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
