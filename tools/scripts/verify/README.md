# scripts/verify — the differential harness for app-suite results

Implements the "first increment" of
`docs/known-issues/c2/verify-01-differential-harness.md` (now retired to
`` — read it first for the *why*). This directory does not
replace the five suite runners under `apps/*-suite-runner/`; it standardises
what happens *after* one of them produces results.

## The one compare command

```
scripts/verify/compare.py <baseline> <current>
```

Prints four sets and nothing else — no green/red verdict:

* **REGRESSED** — was PASS in the baseline, is FAIL/CRASH/etc. now.
* **REGRESSED-HANG** — was PASS, is HANG now. Kept separate from REGRESSED
  on purpose: a timeout at the runner's configured per-class limit is not
  proof of a real hang (see `RESULTS-20260721-hang-rootcause.md` for H2 —
  most "hangs" at 60s turned out to just be slow at 1500s). Confirm with a
  longer-timeout rerun on an idle host before treating this as a regression.
* **FIXED** — was not PASS, is PASS now.
* **STILL-FAILING** — not PASS in both, with the status transition shown
  (e.g. `FAIL -> HANG` is informative; collapsing it to one bucket isn't).

Plus **MISSING** (class dropped out of the current run — could be a crash
before that class even started, don't ignore it) and **NEW** (class not in
the baseline — a new test, not a fix).

Exit code: `0` normally, `1` if REGRESSED or MISSING is non-empty (so it can
gate a script), `2` on a usage/parse error. The exit code is a convenience
for scripting, not a substitute for reading the sets — this tool refuses to
print a single boolean summary, per the doc's "what to refuse" section.

### Generating a baseline from a run

```
scripts/verify/compare.py --emit-baseline <raw-results-file> > baseline.tsv
```

Reads a suite runner's native output and prints the canonical
`class<TAB>status` form, sorted by class name. This is how a checked-in
`baseline.tsv` gets produced — from an actual run's raw output, not
transcribed by hand out of a prose `RESULTS-*.md` table.

### Format sniffing

`compare.py` accepts the checked-in canonical format AND the native output
of the runners that exist today, without needing a format flag:

| Shape | Suite | class col | status col |
|---|---|---:|---:|
| 2 fields, tab, no header | canonical `baseline.tsv` | 0 | 1 |
| 4 fields, comma, no header | Tomcat `results.csv`, runs before 2026-08-23 | 0 | 3 |
| 5 fields, comma, no header | Tomcat `results.csv` (`class,rc,secs,status,loadavg1`) | 0 | 3 |
| 9 fields, tab, no header | H2 (`run-h2-suite.sh` `results.tsv`) | 1 | 2 |
| any width, has a header row naming a `class`/`cls`/`test` and a `status`/`result` column | e.g. Spring Boot's 16-column `results.tsv` | sniffed from header | sniffed from header |

Anything else: pass `--class-col`/`--status-col`/`--sep` explicitly rather
than teaching a runner to change its native format. The five runners keep
their own shapes; this tool adapts to them, not the other way round — that
was true before this existed (five CLIs, five formats) and forcing a
rewrite of every runner's output format was explicitly out of scope for the
first increment.

Rows are last-write-wins per class (a resumable runner's results file is
append-only, so a retried class has two rows — the later one is the real
one).

## Fixture precondition checks

Added directly to the two Linux drivers this increment touched:

* `apps/h2database-suite-runner/run-h2-suite.sh` — `ensure_built()`, called
  from `run_mode()`, checks `$H2_ROOT/target/{classes,test-classes}` are
  non-empty and `$CP_FILE` is non-empty before running anything. This is
  the exact "target/ got swept" failure mode the doc named.
* `apps/tomcat-suite-runner/run-tomcat-suite.sh` — a preflight block before
  `OUTDIR` is created, checking `$TC_ROOT/output/testclasses`,
  `$TC_ROOT/output/build/{conf,webapps}` (the CATALINA_BASE-equivalent the
  doc named explicitly), `$CP_FILE`, and `$CLASSLIST`.

Both die loudly (`ERROR: ... missing or empty: <list>`) before touching any
test class, instead of producing N identical wrong results.

**Not done in this increment** (scope was the two Linux drivers the doc
used as its concrete examples): Spring, Hibernate, and Spring Boot's
runners have no equivalent check yet, and Spring Boot/Elasticsearch/
Keycloak's Windows `.ps1` drivers weren't touched at all. Same pattern
applies if picked up later — see the doc's own "first increment" list.

## Checked-in baselines

* `apps/h2database-suite-runner/baseline.tsv` — 218 classes, generated
  2026-08-03 from a fresh full run on this host (dev @ the commit this
  worktree branched from), craton JIT-on/real-JDK mode, `--class-to 300`.
* `apps/tomcat-suite-runner/baseline.tsv` — 646 classes, craton. Generated
  2026-08-03 by merging two already-on-host raw runs with
  `--emit-baseline`: `full-suite-20260721` (all 646 classes) overridden by
  `cwdfix-craton-20260724` (195 classes rerun after the CWD fix documented
  in `RESULTS-20260724-cwdfix.md` — the later, corrected data wins).
  Machine-merged, not hand-transcribed; may differ by a small amount (~1
  class) from that doc's hand tally, which further hand-split the non-PASS
  set into "confirmed regression" vs "true fixture gap" using a HotSpot
  diff — this baseline doesn't encode that split, the paired HotSpot
  baseline below does the same job by direct comparison instead.
* `apps/tomcat-suite-runner/baseline-hotspot-partial.tsv` — 196 of 646
  classes (only the ones that were non-PASS under craton in the
  2026-07-24 rerun), from the paired `hotspot-control` run on the same
  fixture. Partial by construction — HotSpot was never run against the
  full 646, only the reruns. Use `compare.py` between the two `baseline*`
  files to reproduce the "confirmed regression vs fixture gap" split
  directly instead of trusting a hand count.
* Spring, Hibernate, Spring Boot: no checked-in `baseline.tsv` yet. Spring
  Boot has real recent full-suite raw data at
  `apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-azure-20260802/all-jit/results.tsv`
  (1975 classes, 2026-08-02, already checked in) — running it through
  `--emit-baseline` is a five-minute follow-up, just not done in this pass.
