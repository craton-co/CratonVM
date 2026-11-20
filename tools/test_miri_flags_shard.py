# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

import unittest
from unittest.mock import patch

from miri_flags_shard import run_one, selected_tests


class ShardsTest(unittest.TestCase):
    def test_shards_cover_every_skipped_name_once(self):
        names = [f"flags::tests::case_{n}" for n in range(19)] + [
            "access_flags::tests::case", "unrelated::case"
        ]
        listing = "\n".join(f"{name}: test" for name in reversed(names))
        shards = [selected_tests(listing, n, 4) for n in range(4)]
        selected = [name for shard in shards for name in shard]
        self.assertEqual(sorted(selected), sorted(name for name in names if "flags::" in name))
        self.assertEqual(len(selected), len(set(selected)))

    @patch("miri_flags_shard.subprocess.Popen")
    def test_a_shard_rejects_a_vacuous_or_failed_test(self, popen):
        process = popen.return_value
        process.stdout = iter(["running 1 test\n"])
        process.wait.return_value = 0
        self.assertTrue(run_one("flags::tests::case"))
        self.assertEqual(popen.call_args.args[0][-3:], ["flags::tests::case", "--", "--exact"])
        process.stdout = iter(["running 0 tests\n"])
        self.assertFalse(run_one("flags::tests::case"))
        process.stdout = iter(["running 1 test\n"])
        process.wait.return_value = 1
        self.assertFalse(run_one("flags::tests::case"))

    def test_missing_duplicate_and_invalid_shards_fail(self):
        with self.assertRaises(ValueError):
            selected_tests("unrelated::case: test", 0, 1)
        with self.assertRaises(ValueError):
            selected_tests("flags::case: test\nflags::case: test", 0, 1)
        with self.assertRaises(ValueError):
            selected_tests("flags::case: test", 1, 1)


if __name__ == "__main__":
    unittest.main()
