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

# `python3` is not a name that exists on a stock Windows install — the launcher
# is `python`, and the `python3` alias Microsoft ships is an App Execution
# Alias that prints "Python was not found; run without arguments to install
# from the Microsoft Store" and exits 9009. Under `set -e` that made this
# wrapper look like it had regenerated the file when it had written nothing, so
# the docs stayed stale and `flag_docs_generated` stayed red. Pick whichever
# interpreter is really there.
for PY in python3 python py; do
  if command -v "$PY" >/dev/null 2>&1 && "$PY" -c 'import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)' >/dev/null 2>&1; then
    exec "$PY" "$(dirname "$0")/render-inventory.py" "$ROOT"
  fi
done
echo "error: no Python 3 interpreter found (tried python3, python, py)" >&2
exit 1
