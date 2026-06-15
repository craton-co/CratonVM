# Spring Framework suite under CratonVM — complete results

**Suite:** `apps/spring-framework` 7.1.0-SNAPSHOT, **2927/2930** test classes executed (3 anomalies),
**18,940** test methods. JUnit Platform 6.1.0.
**CratonVM:** `c5644da4` (dev, the binary at run start — *before* this session's fixes).
**HotSpot baseline:** Temurin JDK 25.0.2.
**Method:** one `cratonvm.exe` per batch, crash-recovery to per-class isolation; HotSpot triage of
every non-OK class to drop failures HotSpot shares.

## Whole-suite totals (CratonVM)
| Status | Classes |
|--------|--------:|
| OK | 1065 |
| FAIL (assertions) | 1219 |
| TIMEOUT (hang/slow) | 362 |
| LOADERR | 103 |
| CRASH (SIGSEGV/abort) | 10 |
| ABEND | 2 |
| EMPTY (abstract/no @Test) | 153 |

Test-method level: **18,953 found · 11,584 passed · 7,112 failed** under CratonVM.

## CratonVM-UNIQUE failures (HotSpot passes, CratonVM doesn't) — FINAL (full triage, 4/4 shards)
**1,443 CV-unique classes** (270 more were dropped as SAME-AS-HotSpot — environmental/missing-dep):
| Category | Classes | Notes |
|----------|--------:|-------|
| VM-CORRECTNESS | 1011 | dominated by the **annotation** cluster ([[bugs/spring-bug-01]]) + reflection/generics |
| VM-HANG | 335 | **inflated** — see caveat; mostly the known interpreted-instance-method perf pathology + 4-way-run contention |
| VM-LOADERR | 78 | JUnit-platform dispatch on AspectJ-woven classes (bug-10) |
| VM-CRASH | 11 | 3 Groovy (one cause), scheduler (FAIL in isolation), EmptyMap SIGSEGV (FIXED) … |
| VM-OTHER | 8 | |

### ⚠️ Caveats on the numbers
- **TIMEOUT/VM-HANG is over-counted.** The run was 4-way parallel (+ concurrent rebuilds), so
  slow-but-OK classes crossed the 90s timeout and were classed TIMEOUT→VM-HANG. The *true* hang
  count needs a quiet-machine isolation pass; a large fraction are the documented
  ~1000× interpreted-instance-method slowdown, not infinite loops.
- The baseline is **c5644da4** — before this session's 3 fixes and before recent dev advances
  (kotlin built-ins, regex/JIT, getResourceAsStream …). A rerun on current dev will be lower.

## Perf pathology (the 362 TIMEOUTs) — root cause confirmed
Instance-method invocations only tier-up to the JIT behind `CRATONVM_JIT_VIRTUAL_TIERUP` (default
**OFF**, gated at `vm/src/runtime/interpreter.rs:17416`; flag at `env_cache.rs:135`). Short-loop
instance methods stay interpreted (~1000× slow on reflection/annotation-heavy code) → most timeouts.
A complete instance-tier-up path **already exists** behind that flag (proven ~76× speedup,
bit-identical to HotSpot), but is held OFF because enabling it exposes a deeper latent **JIT→JIT
call-boundary "lost receiver/value" codegen defect** (same family as the `Matcher.search` /
`ByteBuddyState.make` skip-list bans, `vm/src/jit/skip_list.rs:1416,1433`). **That codegen defect —
not the missing counter — is the load-bearing fix; it's architectural and out of scope for a quick
patch.** Actionable: re-running TIMEOUT classes with `CRATONVM_JIT_VIRTUAL_TIERUP=1` reclassifies
perf-timeouts vs true hangs (some, like `ResourceTests`, also need Mockito/ByteBuddy/ASM gen, which
is separately skip-listed). So the 362 TIMEOUT count is **mostly this known perf pathology**, not 362
distinct hang bugs.

## Fixes landed on dev this session (verified, no regression)
| Bug | Commit | Effect |
|-----|--------|--------|
| spring-bug-05 dynamic Proxy disabled | `16f832c0` | `Proxy.newProxyInstance` works |
| spring-bug-03 synthetic `IntStream.findFirst` AbstractMethodError | `4c5a4d09` | `BindingReflectionHintsRegistrarTests` FAIL→OK |
| spring-bug-09 `EmptyMap` OOB → SIGSEGV | `5941addd` | crash eliminated; maps still correct |

Merges → dev `12c22ec5`. Handed off (chips): spring-bug-02 (kotlin-reflect), spring-bug-04 (@Timeout).

## Root-cause clustering (the 1,443 are NOT 1,443 bugs) — refined by deep investigation
Most collapse into a few causes — see [bugs/README.md](bugs/README.md):
1. **Annotation subsystem** (bug-01) — biggest FAIL driver. **Foundational cause found:** the FIRST
   dynamic proxy per process (`$Proxy0`) fell back to a bare `Proxy$Instance` (one-line fix staged),
   breaking every synthesized-annotation accessor. Plus `@AliasFor`-mirror and param-array-rank
   sub-bugs (need tracing), and the `char[]→Integer[]` coercion (fix staged).
2. **Interpreted-instance-method perf** (~1000×) — **most TIMEOUTs.** Tier-up exists behind
   `CRATONVM_JIT_VIRTUAL_TIERUP` (OFF); held OFF by a deeper **JIT→JIT call-boundary codegen
   defect**. Architectural — the load-bearing fix.
3. **GC root-undercount race** (bug-10, was mislabeled "LOADERR dispatch") — under heavy
   multithreaded JUnit execution the young-gen collector zeroes live engine/listener/enum objects
   (root invisible on a parked worker; the `CRATONVM_SHADOW_STACK` register gap). Surfaces as
   LOADERR (getId/Status/NPE) and likely some CRASH/FAIL. One GC fix recovers ~all genuine LOADERRs.
   High-risk (needs uncontended validation; overlaps deferred B-K work).
4. **Synthetic-stream & dispatch gaps** (bug-03 pattern) — partly **fixed** (IntStream.findFirst).
5. **Groovy runtime + JIT crash** (bug-11) — JIT miscompile + Groovy `@Generated`-on-Object
   ClassNode resolution; deep.
6. **Dynamic Proxy + EmptyMap-OOB** (bug-05, bug-09) — **FIXED.**

> Tally caveats: ~14 of the 103 LOADERRs are **missing-TestNG-engine** (environmental, drop); several
> NoClassDefFound are absent optional deps (drop). The GC race + the 4-way-parallel run load means
> some LOADERR/CRASH counts are amplified by the run conditions, not all distinct single-class bugs.
