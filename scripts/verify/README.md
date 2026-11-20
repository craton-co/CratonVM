# Comparing app-suite results

`compare.py` compares per-class results from operator-supplied framework suites.
It uses only Python's standard library.

```sh
python scripts/verify/compare.py baseline.tsv current.tsv
python scripts/verify/compare.py --emit-baseline results.tsv > baseline.tsv
python scripts/verify/compare.py --help
```

Reports distinguish REGRESSED, REGRESSED-HANG, FIXED, STILL-FAILING, MISSING,
and NEW classes. Exit status is 1 for REGRESSED or MISSING classes, 2 for
usage/parse errors, and 0 otherwise. REGRESSED-HANG alone does not produce
exit status 1. A timeout is not proof of deadlock; reproduce it with an
appropriate cap and an idle host before classifying it as a runtime regression.

The parser detects these layouts without a format option. Column numbers are
zero-based:

| Input shape | Class column | Status column |
|---|---:|---:|
| Two fields, tab-separated, no header (canonical baseline) | 0 | 1 |
| Four or five fields, comma-separated, no header (Tomcat) | 0 | 3 |
| Nine fields, tab-separated, no header (H2) | 1 | 2 |
| Header naming `class` / `cls` / `test` and `status` / `result` | From header | From header |

For other formats, supply `--class-col`, `--status-col`, and `--sep`.
Repeated classes use the last row, so an appended retry replaces its earlier
result. MISSING means a class disappeared from the run; NEW means a class was
absent from the baseline and must not be counted as a fix.

Generate baselines from complete raw runs, retain their source
revision and run configuration with local evidence, and compare the same
classpath and workload. External application checkouts are not distributed
with the repository.
