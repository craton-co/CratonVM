# VERIFY-01 — the harness every lane above needs

**Status:** not started. **Owns:** `apps/*-suite-runner/` drivers and a new
`scripts/` entry point. **Not a lane** — a prerequisite the other lanes keep
paying for individually.

## The problem

Every lane in this directory changes compiled-code behaviour. The only
mechanism this project has for noticing that a change was wrong is the app
suites (Spring, Spring Boot, Tomcat, Hibernate, H2), and today they are:

* driven by five different runners with five different CLIs — `.sh` on Linux,
  `.ps1` on Windows, one of them (`hib`) defaulting its binary path to a
  Windows drive letter;
* compared against baselines that live in dated `RESULTS-*.md` files rather
  than in a machine-readable form;
* fixture-dependent in ways that are not checked — the H2 runner failed
  *every* class once because a `target/` directory had been swept, and the
  Tomcat runner needs a `CATALINA_BASE` nobody validates before a run.

So "did this change regress anything" costs a person a day, and the answer
arrives as prose.

## What is already true and worth keeping

* Every runner already takes the binary path from an environment variable
  (`CRATONVM_BIN`, `CRATONVM_EXE`, `CV_BIN`), so a uniquely-named build can be
  probed without editing a shared checkout.
* The H2 and Tomcat runners already shard (`--shard I/M`, positional
  `<idx> <count>`) and write an append-only per-shard results file, so a run is
  resumable.
* The H2 runner emits `results.tsv` with a stable schema
  (`idx class status rc ms tests mode log note`). That is the format to
  standardise on.

## The first increment

1. **One machine-readable baseline per suite**, checked in — a TSV of
   `class → status`, generated from the current best run, replacing the prose
   `RESULTS-*.md` as the *comparison* artifact. Keep the prose as narrative.
2. **One `compare` command** that takes two results files and prints the
   three sets that matter: regressed (was PASS, now not), fixed (was not PASS,
   now PASS), and still-failing. Nothing else. Most of the analysis in the
   existing `RESULTS-*.md` files is this diff, computed by hand.
3. **A fixture precondition check** per suite that fails loudly before the run
   rather than producing 218 identical errors.

## Non-obvious requirements

* **A HotSpot control run is part of the answer, not a luxury.** The Tomcat
  analysis that produced "91 confirmed CratonVM-only regressions" got there by
  running the same classes under HotSpot and diffing — without that, a failing
  class is ambiguous between a VM bug and a broken fixture.
* **Timeouts are a status, not a failure.** The H2 baseline recorded 60 HANGs
  at a 60s timeout; re-running at 1500s showed most were merely slow. A
  comparison that treats HANG as FAIL will report regressions that are load
  artifacts of a contended host.
* **Do not add fixed wall-clock bounds to any check.** Both directions flake:
  an upper bound fails under contention, and a lower bound can pass while
  measuring nothing.

## What to refuse

A "green/red" summary. Every suite here has a substantial permanently-failing
set for fixture and environment reasons, and a boolean will be either always
red or quietly re-baselined until it is meaningless. The deliverable is a
diff against a named baseline commit.
