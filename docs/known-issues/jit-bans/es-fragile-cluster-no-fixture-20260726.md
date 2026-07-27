# ES fragile cluster (`is_elasticsearch_suite_jit_fragile_cluster`, whole `org/elasticsearch/` prefix) — BLOCKED, no fixture on this host

**Status: still banned, not re-tested this session — no fixture available.**
Part of the "remove app-specific JIT bans" sweep following the 2026-07-26 JIT
rework. Attempted to test the blanket `org/elasticsearch/` ban
(`vm/src/jit/skip_list.rs`, `is_elasticsearch_suite_jit_fragile_cluster`)
alongside the narrower `ES-HAMCREST.1` ban in the same Conservative block.

## Why untested

No Elasticsearch checkout exists on this host
(`victor@20.83.144.174`) as of 2026-07-26: `find / -iname craton-testcp.txt
-path '*elasticsearch*'` and `find / -maxdepth 3 -iname '*elasticsearch*'
-type d` both came back empty (a prior session's memory,
`es-suite-adhoc-junitcore-repro-recipe`, references
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
and a sibling `.../20260710-093821-es-tdigest-sortingdigest/apps/elasticsearch`
— neither directory exists anymore, presumably pruned by this host's periodic
space-reclaim sweep). `apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1`
is itself Windows-oriented (`C:\craton\CratonVM\apps\elasticsearch` default
root) and depends on a prebuilt `libvec.so` native fixture that also isn't
present here.

The only `org/elasticsearch/` classes findable on this host are
`elasticsearch-rest-client-8.12.2.jar` (a thin REST client, Apache
HttpClient-based — a completely different subsystem from the server/Lucene/
vector-search internals that originally motivated this ban) and a
`testcontainers/elasticsearch` module (container orchestration, no real ES
server code). Testing the ban with either would not exercise the actual
code paths implicated in the original bug reports (`ES-HANG-*`,
`ES-PERF-*`, `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests`,
etc.) and risks producing a false "safe to remove" signal from an
unfaithful repro — the established lesson from this same sweep's
`spb1-springframework-util-investigation.md` and the `ANTLR.1` item in
`jit-skip-list-open-bans-20260725.md` ("no fixture on this host" is grounds
to leave an item unclaimed/untested, not to force a synthetic substitute).

## Recommendation

**Leave `is_elasticsearch_suite_jit_fragile_cluster` banned.** This is the
single highest-blast-radius item in the whole sweep candidate list (an
entire framework's package prefix) and deserves a real ES checkout + the
Linux-native `run-elasticsearch-suite.ps1` fixture (or a Linux-side
equivalent, following the `es-suite-adhoc-junitcore-repro-recipe` pattern —
`org.junit.runner.JUnitCore` against `server/build/craton-testcp.txt`,
`-Dtests.asserts=false` required) before any lifting decision. Whoever picks
this up next should first re-clone/rebuild an Elasticsearch checkout on this
host (or locate a still-live prior worktree elsewhere) rather than
substitute a partial jar.

Note: `ES-HAMCREST.1` (`org/hamcrest/`, a narrower, separate ban in the same
block) WAS re-tested this session with the real `hamcrest-core`/`hamcrest`/
`hamcrest-library` 3.0 jars directly (no ES needed — Hamcrest's own matcher
codegen is what the ban targets) and came back clean under JIT, including
under `CRATONVM_JIT_THRESHOLD=1` aggressive compilation — see the sweep
summary doc for that result. That is an independent finding from this one;
do not conflate the two ES-adjacent bans.
