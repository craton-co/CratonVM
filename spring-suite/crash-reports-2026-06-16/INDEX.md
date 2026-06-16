# CratonVM × Spring Framework — crash/hang reports (run 2026-06-16)

One `.md` per **CratonVM crash (CRASH/ABEND) or hang (TIMEOUT, HotSpot-OK)** found running the
**complete** Spring suite (`apps/spring-framework`, ~2930 test classes) under CratonVM.
Assertion-only FAILs are **not** in scope here (they live in the broader triage).

- **CratonVM build under test:** dev `8e8e47d9` (suite run, pinned worktree `CratonVM-springrun`).
- **Fixes built/verified on:** current dev `0e3f0398` (worktree `CratonVM-oomfix`).
- **HotSpot baseline:** Temurin JDK 25.0.2.
- **Harness:** `spring-suite/run-all-shards.sh` (4-way, never stops on failure) + `triage-spring.sh`.

> **Run status: IN PROGRESS** — numbers below update as shards finish. Crash monitor is live.

## Crashes / hangs found

| # | Report | Class / API | CratonVM | HotSpot | Status | Owner |
|---|--------|-------------|----------|---------|--------|-------|
| 01 | [crash-01](crash-01-arraylist-capacity-oom-abend.md) | `ArrayConstructorTests` — `new ArrayList(Integer.MAX_VALUE)` | ABEND rc=127 (`abort()`) | OK (catchable OOME) | **FIXED** ✓ (`fix/oom-array-alloc-abend` `0aee90fc`) | **me** |
| 02 | [crash-02](crash-02-native-capacity-ctor-abort-family.md) | `HashMap/HashSet/LinkedHashMap/ArrayDeque/PriorityQueue/StringBuilder(int)` huge cap | ABEND (`abort()`) | no-throw / OOME | **OPEN** (sibling of 01; plumbing ready) | me / handoff |

## Live tallies (partial — 603/2930 classes at last update)
```
OK 320 · FAIL 209 · TIMEOUT 55 · LOADERR 4 · ABEND 1 · EMPTY 14 · CRASH 0
```
TIMEOUTs still need HotSpot triage to split genuine hangs from the known interpreted-perf pathology
(much reduced now that `CRATONVM_JIT_VIRTUAL_TIERUP` is default-on).

## Fix-vs-handoff at a glance
- **crash-01** — *fixed by me*, verified, committed on a branch ready to merge to dev.
- **crash-02** — recommend *me* for the trivial subset (deque/PQ/StringBuilder → catchable OOME via
  the new `try_new_*` plumbing) and *me-or-handoff* for the map lazy-table change (HotSpot doesn't
  throw there). Low real-world likelihood, but a genuine VM-robustness gap.
