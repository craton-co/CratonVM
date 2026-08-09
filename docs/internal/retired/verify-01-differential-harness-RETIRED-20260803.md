# VERIFY-01 — the differential harness every lane needs — IMPLEMENTED

**Status: first increment shipped 2026-08-03.** All three concrete asks in
the original doc's "first increment" exist and were exercised against real
data on the Azure host, not just unit-tested: a canonical baseline format
with a working compare command (`scripts/verify/compare.py`), checked-in
`baseline.tsv` files for three suites generated from real runs, and fixture
precondition checks in the two Linux drivers the doc named explicitly (H2,
Tomcat). Original problem statement preserved below for context.

## What shipped

* **`scripts/verify/compare.py`** — the one compare command. Reads two
  results files and prints REGRESSED / REGRESSED-HANG / FIXED /
  STILL-FAILING / MISSING / NEW, nothing else — no green/red verdict. Sniffs
  the native format of any suite runner that existed as of 2026-08 (H2's
  9-column TSV, Tomcat's 4-column CSV, any format with a header naming a
  `class`/`status` column, e.g. Spring Boot's 16-column TSV) as well as the
  canonical 2-column `baseline.tsv`, so the five runners keep their own
  output shapes — teaching this tool a sixth shape is one table row, not a
  rewrite of a runner. `--emit-baseline` turns any of those into the
  canonical form. Full design notes: `scripts/verify/README.md`.
* **Fixture precondition checks**, added directly to the two drivers:
  `apps/h2database-suite-runner/run-h2-suite.sh` (`ensure_built()` — the
  exact "target/ got swept" case named below) and
  `apps/tomcat-suite-runner/run-tomcat-suite.sh` (checks
  `output/testclasses` and `output/build/{conf,webapps}` — the exact
  "nobody validates CATALINA_BASE" case named below). Both verified live:
  fail loudly with a one-line diagnostic against a deliberately broken
  fixture, stay silent against the real one.
* **Checked-in baselines**, all generated from real runs via
  `--emit-baseline`, none hand-transcribed:
  * `apps/h2database-suite-runner/baseline.tsv` — 218/218, a fresh full run
    on this host 2026-08-03 (PASS 165 / HANG 30 / FAIL 23, `--class-to 300`
    default — a materially cleaner split than the 2026-08-01 probe's 60s
    timeout, which is exactly the "HANG at a short timeout is often just
    slow" effect this doc already called out). Diffing it against the
    2026-08-01 probe's `docs/known-issues/c2/h2-results-20260801.tsv` with
    `compare.py` found 27 FIXED, 52 STILL-FAILING, and 1 genuine REGRESSED
    (`org.h2.test.store.TestTransactionStore` — `testConcurrentAdd`'s
    `assertTrue` failing, PASS on 08-01 to FAIL now). Not chased down as
    part of this pass — it's a concurrency-timing test on a busy shared
    host, and confirming it needs the isolated rerun this tool doesn't
    replace, only points at.
  * `apps/tomcat-suite-runner/baseline.tsv` — 646/646, mechanically merged
    from two already-on-host raw runs (`full-suite-20260721` overridden by
    the CWD-fix rerun `cwdfix-craton-20260724`).
  * `apps/tomcat-suite-runner/baseline-hotspot-partial.tsv` — the paired
    196-class HotSpot control for the same rerun.
  * `apps/spring-boot-suite-runner/baseline.tsv` — 1975/1975, from the
    already-checked-in `craton-fullsuite-azure-20260802` raw results.
* **One incidental bug found and fixed while validating this**:
  `run-h2-suite.sh`'s `log()` wrote to stdout, which corrupted
  `list_for_category()`'s `$(...)`-captured return value with log lines
  whenever `discover()` ran for the first time on a fresh checkout (any
  worktree that hasn't run `discover` yet) — every class silently vanished
  ("nothing to run") instead of erroring. Now writes to stderr.

## What did not ship (left for whoever picks this up next)

* Spring, Hibernate, and Spring Boot's own runners have no fixture
  precondition check yet — only the two the doc used as concrete examples
  were touched. Same pattern (a handful of `[ -d ... ] || missing+=(...)`
  lines before the run loop) applies directly.
* Windows `.ps1` drivers (Spring Boot, Elasticsearch, Keycloak) weren't
  touched — this pass only had a Linux host to validate against.
* No CI wiring. The compare command exists and its exit code (`1` on a hard
  regression) is meant to make that possible later, but nothing calls it
  automatically yet.

## The original problem (2026-07, still accurate)

Every lane in `docs/known-issues/c2/` changes compiled-code behaviour. The
only mechanism this project had for noticing a change was wrong was the app
suites (Spring, Spring Boot, Tomcat, Hibernate, H2), and they were:

* driven by five different runners with five different CLIs — `.sh` on
  Linux, `.ps1` on Windows, one of them (`hib`) defaulting its binary path
  to a Windows drive letter;
* compared against baselines living in dated `RESULTS-*.md` files rather
  than in a machine-readable form;
* fixture-dependent in ways that went unchecked — the H2 runner failed
  *every* class once because a `target/` directory had been swept, and the
  Tomcat runner needed a `CATALINA_BASE` nobody validated before a run.

"Did this change regress anything" cost a person a day, and the answer
arrived as prose.

### What was already true and worth keeping

* Every runner already took the binary path from an environment variable
  (`CRATONVM_BIN`, `CRATONVM_EXE`, `CV_BIN`), so a uniquely-named build could
  be probed without editing a shared checkout.
* H2 and Tomcat already sharded and wrote an append-only per-shard results
  file, so a run was resumable.
* H2's `results.tsv` schema (`idx class status rc ms tests mode log note`)
  was already the shape worth standardising the *reading* side on (see
  `compare.py`'s format table above) — this increment did not ask every
  runner to rewrite its *writing* side to match.

### Non-obvious requirements this increment honored

* **A HotSpot control run is part of the answer, not a luxury** — hence the
  paired `baseline-hotspot-partial.tsv` for Tomcat, not just a craton-only
  baseline.
* **Timeouts are a status, not a failure** — `compare.py` puts a PASS→HANG
  transition in its own REGRESSED-HANG bucket with an explicit "confirm with
  a longer timeout before trusting this" note, separate from a real
  PASS→FAIL regression.
* **No fixed wall-clock bounds anywhere in the check** — `compare.py` never
  looks at the `ms`/`seconds` columns at all, only status.

### What was refused, and still is

A green/red summary. `compare.py`'s exit code exists for scripting
convenience but the tool always prints the full class-level diff regardless
— every suite here has a substantial permanently-failing set for fixture and
environment reasons, and collapsing that to a boolean would be either always
red or quietly meaningless.
