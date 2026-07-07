---
name: bug-01-junit-reflection-heavy-jit-frame-scan-throughput
description: BUG-01 "JUnit discovery hangs on reflection-heavy test classes" is MISDIAGNOSED. reflectionData caching works fine. The REAL cause is precise-JIT-oop-maps default-on (commit 5b8864a0) — its per-call/per-safepoint codegen makes hot JIT'd JUnit code ~6x SLOWER than the interpreter (nojit 15-25s vs JIT 105-123s), overshooting the 120s watchdog. CRATONVM_NO_PRECISE_JIT_MAPS=1 restores ~18s but reintroduces the A2/A3/A4 GC-root corruption that 5b8864a0 fixed. Ruled out: reflection cache, the conservative scan (chain mostly empty), compile time. The fix lives in the precise-maps codegen (owned by concurrent sessions) — reduce its per-call overhead or make emission selective.
metadata:
  type: known-issue
  area: jit, gc, throughput
---

# BUG-01 — "JUnit discovery hangs on reflection-heavy test classes" — REAL cause: precise-JIT-maps codegen overhead

> **⏫ SUPERSEDED 2026-07-07 — precise JIT oop maps re-flipped to DEFAULT-ON; this
> throughput regression is GONE.** The ~6× tax was measured on the doc's era build.
> On current dev (~600 commits later) it no longer reproduces: intervening JIT
> improvements (notably more inlining → far fewer real call safepoints in the hot
> reflection/framework methods) cut the precise per-safepoint cost to noise.
> Re-measured on the Linux spring-core suite with a default-on build:
> `ObjectUtilsTests`/`ClassUtilsTests` are the same wall time precise-on vs -off even
> at `JIT_THRESHOLD=50`; a 40-class spring-util reflection batch is **69.46 s
> default-on vs 69.33 s with `CRATONVM_NO_PRECISE_JIT_MAPS=1`** (noise) with
> **identical pass counts (1059/1061)**. GC-root coverage restored by default and
> verified: all A-family repros (`binarytrees`/`VAAload`/`VArgLen`/`VStatic`
> `14 @GC_STRESS=4096`) → `3222190` clean; the Fork6 GC_STRESS outcome A/B
> (interleaved, load-controlled, 25 each) is **23/25 ALL-OK on vs 22/25 off** —
> equivalent. So the "accepted trade-off" below (losing A2/A3/A4 register-root
> coverage for throughput) is no longer necessary and has been reversed:
> `precise_jit_maps_enabled()` is default-on, opt out with
> `CRATONVM_NO_PRECISE_JIT_MAPS=1`. The original analysis below is retained as history.


**Severity:** High (suite-wide; every reflection/call-heavy JUnit test class overshoots
the 120 s default watchdog and is reported as a hang).

**STATUS: FIXED** by flipping precise JIT oop maps to **default-OFF** (`jit/src/x64.rs`
`precise_jit_maps_enabled()` → opt-in `CRATONVM_PRECISE_JIT_MAPS`; `CRATONVM_NO_PRECISE_JIT_MAPS`
still forces off). After the flip, default build:
`ObjectUtilsTests` **18 s `found=140 succ=140 OK`** and `ClassUtilsTests` **15 s** under the
real 120 s watchdog (were both aborting at 120 s). **Accepted trade-off** (residuals tracked,
owner to revisit): default-off drops back to the conservative JIT-frame root scan, so the
GC-root-coverage-under-JIT family (`SB-CRASH-04`/A3, `ReflRepro`/A2, `Fork6`/A4) that
`5b8864a0` fixed can recur; opt back in with `CRATONVM_PRECISE_JIT_MAPS=1` (restores coverage,
restores the ~6× slowdown). The proper long-term fix keeps the coverage while cutting the
per-call cost (see "The fix" below).

**TL;DR.** The original report blamed `Class.reflectionData()` not being cached — that
is **wrong** (the reflection cache works perfectly). The class does not hang: it
**completes**, but slowly enough that the built-in **120 s watchdog aborts it**, which
surfaces as a "hang." The real cause is that **precise JIT oop maps were default-on**
(commit `5b8864a0`) and their **per-call / per-safepoint codegen makes hot JIT-compiled
code ~6× slower than the interpreter** on call-heavy JUnit execution.

Investigated on a worktree off `dev` `99510377`…`814158ae`. Repro classes:
`org.springframework.util.ClassUtilsTests`, `org.springframework.util.ObjectUtilsTests`.

## Hard measurements (`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, heap 2 GB)

ClassUtilsTests:

| config | wall | note |
|---|---|---|
| HotSpot JDK 25 | **3.2 s** | 106 tests |
| CratonVM, **JIT off** (`CRATONVM_DISABLE_JIT=1`) | **15–20 s** | correct results |
| CratonVM, JIT default (precise maps on) | **105–123 s** | overshoots 120 s watchdog → "hang" |
| CratonVM, JIT, **`CRATONVM_NO_PRECISE_JIT_MAPS=1`** | **18–25 s** | ≈ nojit — **precise maps are the cost** |
| CratonVM, JIT, `CRATONVM_JIT_THRESHOLD=50` (more compiles) | 141 s | more precise-compiled methods → *slower* |
| CratonVM, JIT, `=50000` (≈ nothing compiles) | 18–23 s | ≈ nojit |
| CratonVM, JIT, skip the ~13 hot compiled methods | 25 s | ≈ nojit (no precise codegen runs) |

ObjectUtilsTests: JIT default **aborts at the 120 s watchdog**; watchdog-off it finishes
in **127 s** with **`found=140 succ=140 fail=0`** — so it is *purely* the throughput hang,
no separate bugs. (ClassUtilsTests' `found=63 succ=53 fail=10` vs HotSpot's 106 — the gap
and the 10 fails are **separate pre-existing bugs**: `forName` array types, primitive-class
identity, `isCacheSafe`, `UnmodifiableList`-public. JIT-on and JIT-off produce the *same*
results, so any throughput fix here is behaviour-neutral.)

## Root cause (confirmed by elimination)

`precise_jit_maps_enabled()` (`jit/src/x64.rs`) is **default-ON** as of commit
`5b8864a0` ("fix(jit/gc): default-on CRATONVM_PRECISE_JIT_MAPS — fixes the
GC-root-coverage-under-JIT family (SB-CRASH-04 A3, ReflRepro A2, Fork6 A4)") — opt-out
`CRATONVM_NO_PRECISE_JIT_MAPS`. With it on, every compiled method carries the precise-maps
codegen: a per-invocation prologue **frame-record** (innermost-RBP mirror) and a
per-safepoint **sp-id store** plus the register-local flush + oop-map metadata. On
call-heavy framework code (JUnit's `findAnnotation`/`executeRecursively`/Stream-lambda
machinery, executed an enormous number of times) this per-call/per-safepoint tax is the
~6× overhead. `5b8864a0` lands in the report's own suspected regression window
(`0c904c04..3cc11e0c`), matching "the regression predates the annotation merge."

The precise maps are **correctness-load-bearing**: their per-safepoint register/local
flush is what makes a register-resident GC root visible to the root scan (the A2/A3/A4
family). So the overhead cannot simply be removed — the fix must make the *codegen*
cheaper (or selective) while preserving that coverage.

## Ruled out with data (do not re-investigate these)

- **`reflectionData()` caching (the report's hypothesis).** REFUTED. A `getDeclaredMethods()`
  ×500 microbench fires the backing native **exactly once** (the JDK SoftReference cache
  sticks). During the real run the declared-member natives fire **~49× total in 75 s** and
  the class/method annotation natives fire **zero** times in 40 s.
- **The conservative JIT-frame scan / `scan_active_jit_frames`.** RULED OUT. With
  `CRATONVM_DBG_SCAN_TIMING` the scan's miss-path counter **never reached 500 k calls** —
  the JIT entry chain is *mostly empty* (`chain_len == 0` early-return), so the scan barely
  runs. Coalescing the per-entry backstop band scans, the WS1 scan cache
  (`CRATONVM_NO_JIT_SCAN_CACHE`), and an sp-id active-map scan all changed wall time only
  within box noise. The band size is identical precise-on vs precise-off, so the scan cannot
  explain the 18 s→105 s gap. (This corrects an earlier draft of this doc that blamed the
  scan and proposed precise-maps "Stage B" backstop suppression — that is NOT the cost.)
- **JIT compile *time* / churn.** RULED OUT. Fewer than ~200 methods compile, no recompile
  churn; `CRATONVM_DBG_COMPILE_*` thresholds were never hit. The cost is **runtime execution
  of the precise-codegen'd methods**, not building them.
- **Inline vs CALL frame-record.** `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` changed wall
  time only within noise — the inline-vs-CALL choice is not the dominant term.
- **`CRATONVM_PRECISE_JIT_MAPS` env (note).** This var is **read nowhere** (a no-op); an
  earlier "91 s with it on" reading was box noise. The live gate is the opt-out
  `CRATONVM_NO_PRECISE_JIT_MAPS`.

## The fix (precise-maps codegen — owned by the precise-maps work, not contained)

Make precise maps cheap or selective without losing the A2/A3/A4 register-root coverage:

1. **Cut the per-call/per-safepoint cost.** The frame-record + sp-id + per-safepoint flush
   run on every invocation/call-site of hot methods. Options: an RBP-chain *walk at GC time*
   instead of an eager per-invocation frame-record; emit the per-safepoint flush only for
   safepoints that actually have a live register-resident oop; coalesce/cheapen the sp-id
   store.
2. **Selective emission.** Only emit precise codegen for methods that can actually hold a
   register-resident GC root across a safepoint (the A2/A3/A4 shape), leaving call-heavy
   framework glue on the cheaper conservative path. Needs a sound "can this method's roots be
   register-resident across a call" predicate.
3. **Until then:** the only lever is the global opt-out `CRATONVM_NO_PRECISE_JIT_MAPS=1`
   (6× faster) — unsafe as a default because it reintroduces the A2/A3/A4 GC-root corruption.

This is the precise-maps keystone ([[precise-maps-inline-frame-record-steps12]]); memory
flags that codegen as owned by concurrent sessions. Same throughput family as
[[springrepos-extension-hang-jit-throughput-and-deep-recursion]].

## Repro

```bash
cd /c/craton/CratonVM-refldata
VM=./target/release/cratonvm.exe   # any default (precise-maps-on) build
CPF=/c/craton/cratonvm/apps/spring-framework/spring-core/build/cratonvm-testcp.txt
KRUN=/c/craton/CratonVM-springsuite0622/spring-suite
AF=/tmp/af.txt; { echo -cp; echo "$(cygpath -m $KRUN);$(tr -d '\r' < $CPF)"; } > $AF
# precise on (default) — overshoots the 120s watchdog:
"$VM" --java-home "<jdk25>" --stack-dump-on-timeout 50 "@$(cygpath -m $AF)" KRun org.springframework.util.ObjectUtilsTests
# precise off — ~18s, ≈ nojit (BUT reintroduces A2/A3/A4 GC-root corruption risk):
CRATONVM_NO_PRECISE_JIT_MAPS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$VM" --java-home "<jdk25>" "@$(cygpath -m $AF)" KRun org.springframework.util.ObjectUtilsTests
```
