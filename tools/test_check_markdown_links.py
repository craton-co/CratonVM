# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright Craton Software Company

import tempfile
import unittest
from pathlib import Path

from tools.check_markdown_links import validate_links, validate_public_text


class PublicDocsTests(unittest.TestCase):
    def test_private_links_fail_even_when_archive_exists(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "docs/internal/note.md"
            archive.parent.mkdir(parents=True)
            archive.write_text("private", encoding="utf-8")
            public = root / "README.md"
            public.write_text("[note](docs/internal/note.md)", encoding="utf-8")
            self.assertIn("private archive", validate_links(root, [public])[0])
            archive.unlink()
            self.assertIn("private archive", validate_links(root, [public])[0])

    def test_dates_are_limited_to_known_issues(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            issue = root / "docs/known-issues/bug.md"
            issue.parent.mkdir(parents=True)
            issue.write_text("Observed 2026-01-01", encoding="utf-8")
            public = root / "README.md"
            public.write_text("Updated 20260101", encoding="utf-8")
            self.assertEqual([], validate_public_text(root, [issue]))
            self.assertEqual(1, len(validate_public_text(root, [public])))

    def test_heading_links_are_checked(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            public = root / "README.md"
            public.write_text("# Current status\n[status](#current-status)\n[old](#history)", encoding="utf-8")
            errors = validate_links(root, [public])
            self.assertEqual(1, len(errors))
            self.assertIn("missing heading", errors[0])


if __name__ == "__main__":
    unittest.main()
