# CratonVM — Elasticsearch full unit-test suite triage

**Suite:** Elasticsearch 9.5.0 (`apps/elasticsearch`, source tree), scope = **`libs/*` + `:server` unit tests** (the JUnit test classes, run per-class).
**VM:** `C:\craton\CratonVM-esrun\target\release\cratonvm.exe` — built from `dev` @ `16b69363`, worktree branch `suite/elasticsearch-run`.
**Baseline:** HotSpot JDK 25.0.1 (same classpath, same flags, same 600 s wall-budget).
**Method:** each compiled test class is run standalone via `org.junit.runner.JUnitCore <FQCN>` under both VMs; a class is a **CratonVM bug** only when HotSpot completes (pass or test-failure) but CratonVM crashes or hangs. Two configs are run: **default (JIT on)** and **`--nojit`** (JIT workaround, to surface the bugs hidden behind the dominant JIT hang).
**Date:** 2026-06-18

How the suite was made compilable/runnable, the exact launcher flags, and the per-class harness live under `apps/elasticsearch/cratonvm-suite/` (`run-one.sh`, `run-all.sh`, `run-all-nojit.sh`, `summarize.sh`). The gradle `settings.gradle`/`build.gradle` trims (to compile only the in-scope subgraph of a partial x-pack checkout) are commented `// CRATONVM SUITE TRIM`.

---

## Headline numbers

Total in-scope classes: **2701** (2477 `:server` + 224 across `libs/*`, `client/rest`, `modules/transport-netty4`). Numbers below are the **partial** totals at ~76% through both runs; final totals will be updated when both runs reach 2701 (the pattern is fully deterministic — see "Key facts").

| Config | classes run | clean pass (cv=0) | ran-but-failed (cv=1) | HANG | CRASH |
|--------|--------:|---------:|-----:|------:|------:|
| **default (JIT on)** | 884 | 20¹ | 0 | **864** (ES-HANG-01) | 0 |
| **`--nojit`** | 1178 | 11² | **1168** (ES-FAIL-03) | 3 (ES-HANG-02) | 0 |

¹ the 20 default-config non-hangs are all plain-JUnit (non-`ESTestCase`) classes: `client/rest`, `libs/geo`, `libs/native`, `libs/tdigest`.
² the 11 `--nojit` clean passes are likewise the plain-JUnit classes. The 1168 `cv=1` are `ESTestCase`-derived classes that *run* (no hang, no crash) but fail at `ESTestCase.<clinit>` for ES-FAIL-03.

**No segfault-class crashes were observed in either config** — CratonVM hangs (ES-HANG-01/02) or mis-dispatches an exception (ES-FAIL-03); it does not crash.

---

## Distinct CratonVM defects (one doc each)

| ID | Kind | Config | Title | Fix vs handoff |
|----|------|--------|-------|----------------|
| ES-HANG-01 *(doc removed — resolved)* | HANG (JIT livelock) | default | Every `LuceneTestCase`/`ESTestCase` suite hangs in setup. Pinned to a JIT miscompile of `WeakHashMap$ValueSpliterator.tryAdvance`. | ✅ **FIXED on dev** by `1cd0ab26` (same bug as kafka-bug-C); suite binary `16b69363` predated it. Doc deleted 2026-06-18 (stale). |
| ES-FAIL-03 *(doc removed — resolved)* | exception-dispatch correctness | `--nojit` | `NativeAccessHolder`'s `catch (LinkageError)` not honored → every `ESTestCase` fails at `<clinit>` | ✅ **RESOLVED on current dev** — re-verified 2026-06-18: the catch *is* honored, bootstrap continues to `NoopNativeAccess`, tests run. Doc deleted (stale). (Residual: `BuildTests` still reports 1 unrelated failure — separate, untracked.) |
| [ES-HANG-02](ES-HANG-02-restclient-integ-http-server.md) | HANG (socket/HTTP) | both | `RestClient*IntegTests` hang against an embedded HTTP server (not Lucene, not JIT) | **HANDOFF** (socket layer, low blast radius) |
| [ES-FAIL-04](ES-FAIL-04-arraylist-sublist-toarray-missing.md) | linkage (synthetic gap) | `--nojit` | synthetic `cratonvm/internal/ArrayListSubList` missing `toArray(T[])` → 7 search/sort tests (recorded as `rc=127` under parallel load; deterministically a `NoSuchMethodError` in isolation) | **HANDOFF** (add the overload) |

---

## Key facts for triage

- The **default-config picture is dominated by one bug**, ES-HANG-01: a JIT miscompile that infinite-loops in Lucene's test class-env setup. It is **permanent** (process still hung at 650 s) and hits essentially every `ESTestCase`. So under default settings the vast majority of the 2701 classes `HANG` for this single reason.
- Tests that do **not** extend `LuceneTestCase` (e.g. `client/rest`, some `libs/geo`) run fine under default settings — proving the hang is specific to the Lucene test base, not the launcher.
- The **`--nojit` run is the one that exposes the breadth of distinct defects**, because tests actually execute. Those are written up individually above.
