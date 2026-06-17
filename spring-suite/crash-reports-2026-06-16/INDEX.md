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
| 02 | [crash-02](crash-02-native-capacity-ctor-abort-family.md) | `HashMap/HashSet/LinkedHashMap/ArrayDeque/PriorityQueue/StringBuilder(int)` huge cap | ABEND (`abort()`) | no-throw / OOME | **FIXED** ✓ (`da58ff4e`) | **me** |

## Live tallies (partial — 603/2930 classes at last update)
```
OK 320 · FAIL 209 · TIMEOUT 55 · LOADERR 4 · ABEND 1 · EMPTY 14 · CRASH 0
```
TIMEOUTs still need HotSpot triage to split genuine hangs from the known interpreted-perf pathology
(much reduced now that `CRATONVM_JIT_VIRTUAL_TIERUP` is default-on).

## Fix-vs-handoff at a glance
- **crash-01** — *fixed by me*, verified, committed (`0aee90fc`) on `fix/oom-array-alloc-abend`.
- **crash-02** — *fixed by me*, verified vs HotSpot, committed (`da58ff4e`) on the same branch.

Both fixes live on branch `fix/oom-array-alloc-abend` (base current dev `0e3f0398`), ready to merge.

## Side-findings (correctness bugs surfaced alongside the crashes)
- **[bug-03](bug-03-priorityqueue-boxed-ordering.md)** — native `PriorityQueue` didn't order boxed
  elements (not a min-heap). **FIXED** ✓ (`936b8e19`), verified vs HotSpot.
- **multi-dim array element type** — `new String[2][2]` reports type `[Ljava.lang.String;` (1-D)
  instead of `[[Ljava.lang.String;` (surfaced in `ArrayConstructorTests` after the crash-01 fix).
  OPEN — not yet investigated.
- **[bug-04](bug-04-string-constant-corrupted-to-object-under-load.md)** — under batched load a live
  String constant reads back as a bare `java.lang.Object` (KRun's `"OK"`/`"FAIL"`). Load-dependent
  (clean in isolation); a non-crashing symptom of the known GC-root-undercount race (spring-bug-10).
  OPEN / handoff.
