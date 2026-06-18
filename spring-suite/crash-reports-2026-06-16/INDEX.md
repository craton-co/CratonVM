# CratonVM × Spring Framework — crash/hang reports (run 2026-06-16)

One `.md` per **CratonVM crash (CRASH/ABEND), hang, or distinct correctness bug** found running the
**complete** Spring suite (`apps/spring-framework`, ~2930 test classes) under CratonVM.

- **CratonVM build under test:** dev `8e8e47d9` (suite run, pinned `CratonVM-springrun`).
- **Fixes built/verified on:** current dev `0e3f0398`; **merged to dev `44a8ae56`** (worktree `CratonVM-oomfix`, branch `fix/oom-array-alloc-abend`).
- **HotSpot baseline:** Temurin JDK 25.0.2.
- **Harness:** `spring-suite/run-all-shards.sh` (4-way, never stops on failure) + `triage-spring.sh`.

> **Run status: COMPLETE — all 2,930 classes executed.**

## Final whole-suite totals (CratonVM)
| Status | Classes | | Status | Classes |
|--------|--------:|---|--------|--------:|
| OK | **1,212** | | LOADERR | 60 |
| FAIL (assertions) | 1,172 | | EMPTY (abstract/no @Test) | 117 |
| TIMEOUT (hang/slow) | 367 | | **ABEND** | **1** (fixed) |
| | | | **CRASH (SIGSEGV)** | **1** (fixed) |
| **Total** | **2,930** | | | |

Test-method level: **19,803 found · 13,133 passed · 6,340 failed.** (10 classes had GC-race
`Object@hash`-corrupted status — reclassified by their real pass/fail counts.)

**Only 2 classes crash deterministically in the whole suite — both now fixed.** The TIMEOUT/LOADERR
tail is dominated by the interpreted-perf pathology + the one GC-root-undercount race (load-dependent),
not distinct crash bugs. (HotSpot triage of the 427 crash/hang/loaderr classes in progress —
CV-unique counts appended on completion.)

## Crashes found
| # | Report | Class / API | CratonVM | Status |
|---|--------|-------------|----------|--------|
| 01 | [crash-01](crash-01-arraylist-capacity-oom-abend.md) | `ArrayConstructorTests` — `new ArrayList(Integer.MAX_VALUE)` | ABEND rc=127 (`abort()`) | **FIXED** ✓ `0aee90fc` |
| 02 | [crash-02](crash-02-native-capacity-ctor-abort-family.md) | 6 native capacity ctors (HashMap/Deque/PriorityQueue/StringBuilder…) | ABEND (`abort()`) | **FIXED** ✓ `da58ff4e` |
| 03 | [crash-03](crash-03-rsocket-payloadutils-jit-sigsegv.md) | `PayloadUtilsTests` — Netty direct `ByteBuf` (all off-heap) | **SIGSEGV** rc=139 | **FIXED** ✓ `957270c8` |

**Every deterministic crash found in the run is fixed and verified.** (The only two classes that crash
deterministically in isolation — `ArrayConstructorTests` ABEND, `PayloadUtilsTests` SIGSEGV — both
now pass.)

## Correctness bugs surfaced alongside
| Report | Issue | Status |
|--------|-------|--------|
| [bug-03](bug-03-priorityqueue-boxed-ordering.md) | native `PriorityQueue` not a min-heap (boxed elements) | **FIXED** ✓ `936b8e19` |
| [bug-05](bug-05-generics-fieldtypesignature-cce.md) | generics reflection arrays `Object[]` not `Type[]` → CCE (12 classes) | **PARTIAL** `d01345d1` (wildcard bounds fixed; real-`ParameterizedTypeImpl` `actualTypeArguments` still `Object[]`) |
| [bug-04](bug-04-string-constant-corrupted-to-object-under-load.md) | live constant → bare `Object` under batch load | OPEN — GC-root-undercount race (spring-bug-10 family) |
| (untracked) | multi-dim array element type: `new String[2][2]` → `[Ljava.lang.String;` (1-D) | OPEN |

See [FAIL-ANALYSIS.md](FAIL-ANALYSIS.md) for the full FAIL-clustering (the other big CV-unique family
is the interface "no Code attribute" itable bug = spring-bug-03).

## The dominant remaining issue — the GC-root-undercount race
The batch-only SIGSEGVs (spring-webflux, varying class each run), the `java.lang.Object@hash` corrupted
statuses, bug-04, and part of bug-05's "12 classes" are **all one architectural bug**: under heavy
batched JUnit + GC pressure the young collector treats a live object as dead and reuses its slot
(spring-bug-10 / memory `jit-junit-discovery-reflection-corruption`). It is **load-dependent** (clean
in isolation), so it never reproduces as a single-class deterministic crash — but it is the
**highest-leverage remaining fix**. Architectural; recommend a dedicated GC handoff.

## Fixes shipped (all on `fix/oom-array-alloc-abend`, base dev `0e3f0398`, ready to merge)
| Commit | Fix |
|--------|-----|
| `0aee90fc` | crash-01 — ArrayList(int) huge cap → catchable OOME |
| `da58ff4e` | crash-02 — capacity-ctor family (maps lazy-cap, deque/PQ/SB OOME) |
| `936b8e19` | bug-03 — PriorityQueue natural ordering |
| `d01345d1` | bug-05 (partial) — generics arrays `Type[]` |
| `957270c8` | crash-03 — `Unsafe` `Buffer.address` returns real address (all Netty direct buffers) |

## Fix-vs-handoff
- **Mine, done:** crash-01, crash-02, crash-03, bug-03 (all verified). bug-05 partial.
- **Recommend handoff (architectural):** the **GC-root-undercount race** (bug-04 + the batch-only
  crashes) — single highest-leverage bug; and the interface "no Code attribute" itable bug (spring-bug-03).
- **Small follow-ups:** bug-05 remainder (real-`ParameterizedTypeImpl` `actualTypeArguments`), multi-dim
  array element type.
