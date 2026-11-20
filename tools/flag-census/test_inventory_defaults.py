#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
"""Check the effective collector defaults printed for flag users."""

import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location(
    "inventory", Path(__file__).with_name("render-inventory.py"))
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class CollectorDefaults(unittest.TestCase):
    def test_effective_defaults_include_all_shipping_collectors(self):
        rows, _ = inventory.rows(inventory.ROOT)
        for key, expected in (
            ("CRATONVM_GC_HOTSPOT_INITIAL_HEAP", "on"),
            ("CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS", "on"),
            ("CRATONVM_SOFTREF_HOTSPOT_LRU", "on"),
            ("CRATONVM_TLAB_SHARE_SIZER", "gen-g1-on"),
            ("CRATONVM_TL_INTRINSIC", "compatible-on"),
        ):
            with self.subTest(key=key):
                self.assertEqual(rows[key][3], expected)


if __name__ == "__main__":
    unittest.main()
