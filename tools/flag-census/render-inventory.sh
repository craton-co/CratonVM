#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Regenerate the Full inventory table in docs/config/flag-inventory.md from
# types/src/flag_groups.rs::INVENTORY, types/tests/flag-surface.txt and a scan
# of <crate>/src.
#
# Thin wrapper so this sits beside render-tokens.sh and is invoked the same way.
#
#   tools/flag-census/render-inventory.sh          # from anywhere
set -euo pipefail

ROOT="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
exec python3 "$(dirname "$0")/render-inventory.py" "$ROOT"
