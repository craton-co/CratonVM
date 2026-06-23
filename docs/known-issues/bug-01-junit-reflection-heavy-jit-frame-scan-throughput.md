---
name: bug-01-junit-reflection-heavy-jit-frame-scan-throughput
description: BUG-01 "JUnit discovery hangs on reflection-heavy test classes" is MISDIAGNOSED. Reflection/reflectionData caching works fine. The real cause is the conservative JIT-frame GC-root scan (scan_active_jit_frames) making JIT-on ~6x SLOWER than the interpreter on call-heavy JUnit execution, overshooting the 120s default watchdog. nojit=20s, default JIT=123s. Same documented mechanism as the WS1/kafka-throughput + springrepos-extension-hang issues; the real fix is precise oop maps (in-flight, default-off, only ~26% effective today).
metadata:
  type: known-issue
  area: jit, gc, throughput
---

# BUG-01 — "JUnit discovery hangs on reflection-heavy test classes" — REAL cause: conservative JIT-frame root scan

**Severity:** High (suite-wide; every reflection/call-heavy JUnit test class overshoots the
120 s default watchdog and is reported as a hang).

**TL;DR — the original BUG-01 report is misdiagnosed.** It blamed
`Class.reflectionData()` not being cached. That is **wrong**: the reflection-data
cache works perfectly (≈50 declared-member native calls for the whole run). The
class does **not** hang — it **completes in ~123 s** with the JIT on, but the
built-in **120 s watchdog aborts it first**, which surfaces as a "hang." With the
JIT **disabled** the same class completes in **~20 s**. So the JIT makes this
workload **~6× slower than the interpreter**. This is the same mechanism already
documented in `vm/src/jit/conservative_roots.rs` (the "WS1 kafka JIT throughput"
comment) and in [[springrepos-extension-hang-jit-throughput-and-deep-recursion]].

Investigated on a worktree off `dev` `99510377` (binary `cratonvm-bug01*.exe`).
Repro classes: `org.springframework.util.ClassUtilsTests`,
`org.springframework.util.ObjectUtilsTests`.

## Hard measurements (ClassUtilsTests; `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, heap 2 GB)

| config | wall | note |
|---|---|---|
| HotSpot JDK 25 | **3.2 s** | 106 tests |
| CratonVM, **JIT off** (`CRATONVM_DISABLE_JIT=1`) | **20 s** | correct results |
| CratonVM, JIT default | **123 s** | overshoots 120 s watchdog → "hang" |
| CratonVM, JIT, lower threshold `CRATONVM_JIT_THRESHOLD=50` | 141 s | more compiles → *slower* |
| CratonVM, JIT, high threshold `=50000` | **18–23 s** | ≈ nojit (nothing crosses threshold) |
| CratonVM, JIT, `CRATONVM_PRECISE_JIT_MAPS=1` | 91 s | precise maps help only ~26 % |
| CratonVM, JIT, **skip the 13 hot methods** (see below) | **25 s** | ≈ nojit |

(The 63-vs-106 test-count gap and the 10 failures in CratonVM runs are **separate,
pre-existing bugs** — `forName` array types, primitive-class identity, `isCacheSafe`,
`UnmodifiableList`-public — NOT part of BUG-01. JIT-on and JIT-off produce the
*same* `found=63 succ=53 fail=10`, so the throughput fix is behaviour-neutral.)

## What was ruled out (with data)

- **Reflection-data / `reflectionData()` caching (the report's hypothesis).** REFUTED.
  Instrumented `getDeclaredMethods0`/`getDeclaredFields0`/`getDeclaredConstructors0`
  and all the class/method annotation natives. A microbench loop of
  `getDeclaredMethods()` ×500 fires the native **exactly once** (the JDK
  `reflectionData` SoftReference cache sticks). During the real ClassUtilsTests run
  the declared-member natives fire **~49 times total in 75 s**, and the annotation
  natives fire **zero** times in 40 s. Reflection is not the bottleneck.
- **JIT compile *time*.** Only **13 methods** ever compile (no recompiles/deopt
  churn), and 0 GCs occur — so it is not compilation cost.
- **Instance-method virtual tier-up** (`CRATONVM_JIT_VIRTUAL_TIERUP=0` → 115 s, ~no
  change).
- **OSR** (`CRATONVM_TIER_OSR_BACKEDGE=2e9` → 112 s; OSR is only ~+28 s).
- **`on_method_invocation` per-call frequency.** Throttling the tiered-manager
  consult with a `JIT_RETRY_STRIDE` gate in the `execute()` warmup path did **not**
  help (140 s) — reverted.

## Root cause (confirmed)

The whole overhead comes from JIT-compiling a handful of tiny, ultra-hot
leaf/utility methods, then paying CratonVM's **conservative JIT-frame GC-root scan**
for them. The mechanism is documented verbatim in
`vm/src/jit/conservative_roots.rs` (search "WS1 (kafka JIT throughput)"):

> `scan_active_jit_frames` runs on **every object-returning native call** (via
> `update_root_snapshot`). For a conservative chain entry it scans the band
> `[scanner_sp, entry_sp]` word-by-word. When a compiled frame sits low on a deep
> interpreter stack (e.g. a compiled JUnit lambda that transitively runs the test
> plan), that band spans the entire interpreter recursion above it, so every native
> call pays an **O(megabytes)** stack scan. *"This was the dominant mechanism behind
> 'JIT-on is slower than the interpreter' on call-heavy suites."*

The conservative path is taken because **precise oop maps are not emitted during
codegen by default** (`JitEntryGuard::enter_with_compiled`,
`conservative_roots.rs:540` — `if !cm.has_precise_oop_maps() { return Self::enter() }`).

### The 13 methods that compile (and cause it)

`org/junit/platform/commons/util/ReflectionUtils.{lambda$findAllFieldsInHierarchy$0,
defaultMethodSorter}`, `org/junit/platform/commons/util/Preconditions.{notNull,
condition,lambda$containsNoNullElements$2}`,
`org/junit/jupiter/engine/extension/MutableExtensionRegistry.lambda$stream$0`,
`java/util/regex/Pattern.lambda$DOT$0`, `java/util/Objects.requireNonNull`,
`java/lang/Integer.compare`, `java/lang/Class.cast`,
`java/lang/Character.{isHighSurrogate,codePointAt,charCount}`,
`java/lang/Enum.valueOf`.

Forcing all 13 to skip the JIT (`CRATONVM_JIT_BISECT_SKIP=…`) drops the run from
**140 s → 25 s** — proving these compiled frames are the entire cost.

## Why the obvious mitigations don't generalise

- **A bytecode-size gate cannot separate culprits from genuinely-hot methods.**
  `Objects.requireNonNull` is 32 bytecodes and `Character.codePointAt` is 49 —
  *larger* than `fib` (16). A size gate that excludes them also excludes `fib`,
  which **needs** compilation (`fib` benefits because it is called tens of millions
  of times and recurses JIT→JIT, so its interpreter→JIT transition is paid once).
- **Raising the invocation threshold** to ~50000 fixes ClassUtilsTests (the leaves
  are called 10 k–50 k times here, so they stop crossing it) but is a band-aid: a
  longer/larger test class pushes the same leaves past any fixed threshold, and a
  high default starves legitimately-hot medium methods of compilation.
- **`fib` is the adversarial case**: tiny, no loop, recursive, and *benefits* from
  JIT — so neither size nor "has-no-loop" nor "is-a-leaf" cleanly distinguishes the
  regressive methods. The true distinguisher is *caller context* (called
  predominantly from the interpreter, on a deep call-heavy stack), which is not
  known at compile time without profiling.

## Why "emit oop maps for the leaf methods" does NOT work (investigated 2026-06-23)

Three measured facts kill the leaf-map idea:

1. **Leaves are not the expensive frames.** Splitting the 13 and skipping only the
   pure leaves (`Integer.compare`, `Character.*`, …) leaves the run at 138 s (≈
   default); skipping only the methods-*with-calls*/lambdas drops it to 96 s. A leaf
   has no callee subtree below it, so its conservative band is tiny — cheap. The
   expensive frames are the call-wrapping lambdas whose band spans a big interpreted
   subtree.
2. **Precise maps are already emitted, and don't avoid the band scan.**
   `scan_one_frame_precise` (`conservative_roots.rs:1472-1485`) scans the mapped
   slots **and then STILL runs the full conservative backstop**
   `scan_one_frame(scanner_sp, info.frame_base)`. That backstop band scan is the
   actual O(stack-band) cost; precision is purely additive on top of it.
3. **No frame is `fully_oop_covered`, and the band covers callee oops.** Running the
   repro under `CRATONVM_PRECISE_COVERAGE_PIN=1 CRATONVM_DBG_VERIFY_OOP_MAPS=1` shows
   **64/64 precise frames `covered=false`**, and the unmapped in-band oops sit at
   deep offsets the oracle flags as *nested-JIT-callee slots*. So the backstop is
   load-bearing: it covers real roots in callees / unpinned native Rust locals that
   the frame's own maps cannot describe.

The codegen scaffolding for the fix exists (`CompiledMethod.fully_oop_covered`,
x64.rs:24277-24296) but is **inert by default** (`sp_id_slot_off == 0` →
`fully_oop_covered` is always false), and per its own comment the backstop may be
suppressed only once the runtime `CRATONVM_DBG_VERIFY_OOP_MAPS` oracle *proves*
coverage — i.e. it needs whole-stack precise coverage, not a per-leaf map. A naive
backstop-skip would drop the callee/native-local roots → heap corruption.

## The real fix (deep / cross-cutting — handoff)

The fix is **precise-maps "Stage B": let `scan_one_frame_precise` skip the
conservative backstop band scan for a `fully_oop_covered` frame** (the exact lever
named in x64.rs:24283-24289). That requires, in order:

1. **Full safepoint coverage + the precise inline gate** so methods actually become
   `fully_oop_covered` at runtime (today 0 % are). Needs `sp_id_slot_off != 0`,
   every `safepoint_pcs` mapped, no inlined-callee safepoints.
2. **Whole-stack coverage (the callee/native-local roots).** The backstop band also
   covers oops in nested callees and unpinned native Rust locals (proven by the
   VERIFY oracle above). Suppressing it is sound only when those are rooted some
   other way — every JIT callee precise+covered, and every allocating native pinning
   via `pin_native_root`.
3. **The runtime `CRATONVM_DBG_VERIFY_OOP_MAPS` oracle as the gate** before flipping
   backstop-suppression on, per the staged-rollout design.

This is the keystone of the in-flight precise-maps work
([[precise-maps-inline-frame-record-steps12]]) and is the same root cause as
[[springrepos-extension-hang-jit-throughput-and-deep-recursion]]; it is **not** a
per-leaf map and **not** a contained single-session change.

### (superseded note) earlier framing
The keystone is precise oop maps emitted during codegen — but emission is *already*
default-on, and on its own it does **not** help because the backstop band scan still
runs (see "Why ... does NOT work" above). The actionable missing piece is the
backstop suppression (Stage B), not the emission. `CRATONVM_PRECISE_JIT_MAPS` is a
no-op (read nowhere).
   Today it is default-off and only ~26 % effective here (91 s), suggesting maps are
   not yet emitted for these method shapes (lambdas / tiny leaves). Closing that gap
   should bring this workload to ≈ nojit.
2. **Or** bound the conservative band to the compiled frame's own size rather than
   `[scanner_sp, entry_sp]` — but that is exactly the GC-soundness tradeoff the WS1
   cache documents (the band covers interpreted callees that may hold the only live
   root), so it needs care.

This is the same root cause as [[springrepos-extension-hang-jit-throughput-and-deep-recursion]]
and the WS1 kafka-throughput note; fixing precise maps fixes all three.

## Verified NOT fixed by precise maps as landed (dev `814158ae`, 2026-06-23)

Precise-map **emission is already default-on** (`x64::precise_jit_maps_enabled()` =
opt-out `CRATONVM_NO_PRECISE_JIT_MAPS`; Steps 1–8 are in dev history). The
`CRATONVM_PRECISE_JIT_MAPS` env var is **read nowhere** — an earlier "91s with it
on" reading was run-to-run noise on this box, not the flag. Merging current dev and
rebuilding did **not** fix BUG-01:

| class | default (watchdog ON) | watchdog OFF | nojit |
|---|---|---|---|
| ClassUtilsTests | 112 s (barely under 120 s — fragile) | 105 s | 15 s |
| ObjectUtilsTests | **rc=127, aborts at 120 s (still hangs)** | 127 s (`found=140 succ=140`, all pass) | — |

`ObjectUtilsTests` has **no** separate correctness bugs (140/140 pass with the
watchdog off) — it is *purely* the throughput hang, and it still overshoots the
watchdog with precise maps on. **Why precise maps don't help here:**
`emit_oop_map_for_safepoint` only fires at **safepoints (call sites)**, so a pure
leaf method with no calls (`Integer.compare`, `Character.charCount`, …) emits **no**
oop maps → `has_precise_oop_maps()==false` → it still falls to the conservative
band scan. Closing the gap needs oop-map coverage (or a cheap entry-frame bound)
for the leaf/lambda shapes that compile on these workloads — still the open fix.

## Repro

```bash
cd /c/craton/CratonVM-refldata/spring-suite   # or reuse CratonVM-springsuite0622/spring-suite
# default watchdog ON → reproduces the "hang" (aborts at 120 s):
VM=.../target/release/cratonvm-bug01.exe
"$VM" --java-home "$JDK25" "@/tmp/af_sc.txt" KRun org.springframework.util.ClassUtilsTests
# completes in ~20 s and proves it is not a hang:
CRATONVM_DISABLE_JIT=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$VM" ... KRun org.springframework.util.ClassUtilsTests
# isolates the cost to the 13 compiled methods (~25 s):
CRATONVM_JIT_BISECT_SKIP="java/lang/Integer.compare,java/util/Objects.requireNonNull,..." "$VM" ...
```
