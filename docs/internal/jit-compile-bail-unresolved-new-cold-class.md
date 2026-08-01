# JIT coverage gap: a hot method never compiled if it contained `new <not-yet-loaded class>`

**Status:** FIXED 2026-07-31 (`fix/jit-new-unresolved-cp-20260731`). Was never a
correctness bug — affected methods stayed interpreted, which is safe but can be
very slow. Found on 2026-07-27 while re-testing the json-smart JIT ban (since
retired). Repro: `docs/internal/repros/jit-cold-new-cp-20260731/`.

## Symptom

A method that is unambiguously hot never left the interpreter.
`CRATONVM_DBG=jit-method-stats` names them (`tier_fail_count=3` = the compiler
gave up permanently):

```
[cratonvm] JIT method stats: 65 distinct methods tracked, 64 ever invoked, 14874779 total invocations
           | still-interpreted=14 c1=20 c2=31 | c1_threshold=500
           | hot_but_stuck_in_interpreter=13 (of which ineligible-by-policy=0, compile-failures=13)
[cratonvm]  2939956 queued=false tier_fail_count=3  net/minidev/json/parser/JSONParserBase.readMain(...)
[cratonvm]  2459956 queued=false tier_fail_count=3  net/minidev/json/parser/JSONParserMemory.readString()V
[cratonvm]  2309940 queued=false tier_fail_count=3  net/minidev/json/parser/JSONParserBase.checkControleChar()V
...
```

## Cause

Every one of those methods contains `new net/minidev/json/parser/ParseException`
on an error path. `resolve_jit_new_site` (`vm/src/runtime/interpreter/invoke.rs`)
resolves a `new`'s CP index with `find_class_by_name_for_class`, which only sees
**already-loaded** classes. When no parse ever fails, `ParseException` is never
loaded, so the resolver returned `None`, `try_compile_inner` bailed the WHOLE
compile (`cp_new_resolver` / `new_resolve` site), and after
`MAX_TIER_FAIL_RETRIES` (3) attempts the method was never retried.

The bail was silent by design (it looks "transient"), which is what made this
hard to spot. `CRATONVM_DBG_JITC=1` names the resolver responsible:

```
[cratonvm-jitc] resolver-bail site=new_resolve net/minidev/json/parser/JSONParserBase.readMain(...)
[cratonvm-jitc] compile-bail   net/minidev/json/parser/JSONParserBase.readMain(...) backend_attempted=false
```

The shape generalises far beyond json-smart: **any hot method whose only
un-taken branch does `throw new SomeException(...)`** was uncompilable until
something else in the process loaded that exception class. Cold `new` of a
lazily-used helper class had the same effect, and `anewarray` of a not-yet-loaded
component class bailed through the same resolver (`anewarray_resolve`).

## Fix — resolve at run time, not compile time

The third option from the original writeup, and the one it recommended. The
compiler no longer needs a class id for a `new` it cannot resolve: it bakes the
*referencing* class id + the constant-pool index and calls a helper that resolves
and initialises the class on first execution.

Sound because it is exactly what the interpreter's own `0xbb` handler already
does — same thread, same program point, same loader-faithful resolver, same JVMS
5.4.4 access check, same `<clinit>`. Free on the hot path: a `new` whose class IS
loaded at compile time still takes the inline-TLAB / `jit_new_object` path
untouched, and even a deferred site takes a lock-only `find_class_by_name_for_class`
fast path from its second execution onward.

The two rejected alternatives stay rejected:

- **Resolving (loading) the class at compile time** would run a user-defined
  `ClassLoader.loadClass` from inside the JIT compile path with the class-manager
  lock in play — a deadlock/reentrancy hazard — and would make the VM load
  classes the program never would.
- **An uncommon trap at the unresolved `new`** (HotSpot's answer) needs the
  precise frame-deopt path; CratonVM's default deopt re-runs from method entry,
  which double-executes side effects already committed before the trap — unsound
  for exactly the methods that motivate this (a parser that has already advanced
  its cursor).

### What changed

- `cratonvm_jit::JitNewSite` (new): the `cp_new_resolver` contract is now
  `Resolved { class_id, num_fields, has_prim_init, has_finalizer }` /
  `Deferred { holder_class_id, cp_idx }` / `None`. `None` is reserved for a site
  no runtime resolution can rescue (holder class gone, CP entry is not a class
  reference) and is still a permanent bail.
- `JitRuntimeHelpers::new_object_cp` / `anewarray_object_cp` (new, `OptionalPtr`,
  appended so every prior golden offset is unchanged; `NUM_FIELDS` 58 → 60).
  `vm/src/jit/helpers.rs::jit_new_object_cp` resolves + access-checks and falls
  into `jit_new_object`'s body; `jit_anewarray_object_cp` is the `anewarray`
  sibling. Unwired (0, hand-built test tables) ⇒ the deferred site keeps the
  historical bail rather than emitting a `CALL` to address 0.
- `x64::compile_with_param_slots` takes `new_deferred_info` /
  `anewarray_deferred_info` (`(pc, holder_class_id, cp_idx)`), disjoint from the
  resolved lists. Deferred sites are excluded from the inline-TLAB bump, scalar
  replacement (no `new_info` entry ⇒ no plan) and `gc_inert_selfrec`, and they
  force `has_dispatch` — resolution itself needs the `JIT_THREAD` TLS.
- The IR builder skips deferred sites, so an allocation-bearing method with one
  falls back to the single-pass backend (which compiles it) instead of bailing
  both.
- **Bonus fix in the two OSR compile paths** (`interpreter.rs` and
  `invoke.rs`): a `new` whose class failed to load, or failed the access check,
  was baked as the sentinel entry `(pc, class_id 0, 0 fields, true, true)`. The
  comment claimed this "makes the JIT skip this site and defer to the
  interpreter"; it did not — the codegen found an entry at that pc and compiled
  an allocation against **class id 0**. Those sites are now deferred, so the real
  resolution and `IllegalAccessError` happen at the site.

## Sibling resolvers checked (and why they are not the same gap)

- **`anewarray` (0xbd)** — WAS the same gap, through the same resolver
  (`anewarray_resolve`). Fixed here too, via `anewarray_object_cp`.
- **`checkcast` / `instanceof`** — resolve by NAME (`cp_class_name_resolver`);
  the helper does the class lookup at run time already. No gap.
- **`multianewarray`** — its compile-time metadata is `(pc, dimensions)` only.
  No class resolution, no gap.
- **`getstatic` / `putstatic`** — `cp_static_field_resolver` calls
  `resolve_field_ref`, which DOES resolve the owning class at compile time, but
  through the flat loader-blind `load_class_concurrent` (never a user
  `loadClass`), and returns `Err` outright for a loader-sensitive referencing
  class — so a cold `getstatic` on a user-loader-owned class still bails the
  whole compile at `static_field_resolve`. That is a real, structurally similar
  residual, but it belongs to the field-resolution/loader-fidelity area rather
  than to this doc, and closing it means changing what the compile path is
  allowed to load — deliberately NOT in scope here.
- **`ldc` of a String/Class/MethodHandle constant** — permanent bail by
  design (RBC.7), a property of the class file, not a resolution-timing miss.

## Verification

`docs/internal/repros/jit-cold-new-cp-20260731/ColdNewJitProbe.java`, 2,000,000
iterations, release build, Azure Linux host, JDK 25:

| arm | pre-fix | post-fix |
|---|---|---|
| `Cold` never loaded | **8689 ms** — `hot` `tier_fail_count=3`, 1,999,988 interpreted invocations | **776 ms** — `hot` compiles, 1,140 interpreted invocations |
| `-Dcold.trip=true` (control: `Cold` loaded first) | 765 ms | 862 ms |

`acc=931093952` in all four arms. The control arm is what makes this
non-vacuous: pre-fix, loading one otherwise-irrelevant exception class was worth
11x on that method.

json-smart 2.6.0 (`JsonSmartProbeCold`, 30,000 rounds over 10 mixed documents):

| | pre-fix | post-fix |
|---|---|---|
| total interpreted invocations | 14,874,779 | 108,840 |
| `hot_but_stuck_in_interpreter` | 13 | 0 |
| compile-failures | 13 | 0 |
| `resolver-bail site=new_resolve` | 40 | 0 |
| errors | 0 | 0 |

Its **wall clock does not move** (≈218 s either way, and ≈37 s at 5,000 rounds
with base-cold ≈ base-warmed ≈ fix-cold across 3 interleaved rounds) — those
parser methods are not what bounds that workload, and the original writeup's
"very slow" framing was never actually demonstrated on it. The microprobe above
is where the throughput consequence is real. Host load averaged 16-45 on 16
cores throughout, so treat any sub-10% wall-clock difference here as noise.

### Regression tests added

- `jit/src/lib.rs::deferred_new_site_compiles_when_cp_helper_is_wired` — the
  COMPILE half. The same bytecode compiled three ways (resolved /
  deferred+wired / deferred+unwired); the resolved arm is a control, so a shape
  that stops compiling for an unrelated reason cannot make it pass vacuously.
- `vm/tests/jit_cold_new_cp.rs` — the RUNTIME half, end-to-end against a real
  JDK. A hot `hot(int)` carrying a cold `throw new Cold(...)` and a hot
  `hotArr(int)` carrying a cold `new Elem[3]`. On `origin/dev` BOTH are
  `tier_fail_count=3 compile-failed`; with the fix neither is, and every output
  matches HotSpot byte for byte:

  ```
  clinitBeforeCold=false   coldClass=ColdNewCpProbe$Cold   coldMsg=cold-taken
  clinitAfterCold=true     arrClass=[LColdNewCpProbe$Elem;  arrLen=3  arrElem0Null=true
  ```

  The `<clinit>`-timing pair is what keeps it honest: `clinitBeforeCold=false`
  fails if anything loaded `Cold` early (⇒ the site was never deferred and the
  compile-failure assertion would prove nothing), and `clinitAfterCold=true`
  fails if the helper allocated without running JVMS §5.5 initialisation.

### Suites

`cargo test -p cratonvm-jit -p cratonvm-jit-api` green (1064 lib + 35 jit-api +
12 integration suites).

`cargo test -p cratonvm-vm --no-fail-fast`: four suites fail, all accounted for
against a pristine `origin/dev` checkout of the same worktree —

| suite | `origin/dev` | with fix |
|---|---|---|
| `class_loader_unload_regression` | 2 FAIL | 2 FAIL (identical `checksum=-232543299289`; also fails its `--nojit` arm, which a JIT-only change cannot reach) |
| `wp4_6_chm_basic` | 2 FAIL | 2 FAIL (siblings already `#[ignore]`d for "WP4.6-FOLLOWUP-A: CHM transfer() data-loss") |
| `tier1_tests::t1_gc_pause_budget_100k_objects_under_200ms` | FAIL | passes on rerun — a pause-time budget, failed only at host load 386 |
| `url_classloader_resource_delegation` | ok | ok — the one failing run said "fixture was not compiled", i.e. `build.rs`'s `javac` was absent from a non-interactive `PATH` |

Apache Tomcat, all 645 test classes, one process per class, 6 shards, fix arm
then baseline arm (`apps/tomcat-suite-runner/run-tomcat-suite.sh`):

| | PASS | FAIL | HANG | NOSUMMARY |
|---|---|---|---|---|
| fix | 533 | 74 | 36 | 2 |
| baseline | 524 | 69 | 50 | 2 |

**Read this diff with care** — the arms could not be interleaved (they share
`tomcat.test.temp` and bind the same ports), and this shared host swung between
load 35 and 386 on 16 cores between them, with the *baseline* arm taking the
worse half. 69 classes differ. Splitting them:

- **`TestHttpServletDoHead*` (65 classes in the suite, 38 of the 69 diffs)** —
  16 flips toward the fix, 22 against. This family is documented as straddling
  the runner's 300 s wall
  (`reference_tomcat_dohead_family_straddles_300s_timeout`); flipping in both
  directions at roughly the same rate is its normal signature, not a signal.
- **Everything else (30 diffs)** — 25 toward the fix, 5 against. The 25 are
  dominated by baseline HANG → fix PASS across the Jasper compiler and HTTP/2
  clusters, which is the shape you would predict from "13-ish hot methods per
  workload that used to interpret forever now compile".

#### Per-class back-to-back rerun of the 5 non-`DoHead` candidates

Each class run under the fix binary and then immediately under the baseline
binary — adjacent in time, so both see the same host conditions — at a 900 s
timeout:

| class | fix | baseline | verdict |
|---|---|---|---|
| `TestFormAuthenticatorC` | PASS (164 s) | PASS (163 s) | not a regression |
| `TestTomcatStandalone` | PASS (9 s) | PASS (8 s) | not a regression |
| `TestWebSocketFrameClient` | PASS (50 s) | FAIL (310 s) | not a regression — the verdict *reversed* |
| `TestWsRemoteEndpointImplServerDeadlock` | HANG (900 s) | HANG (900 s) | not a regression — both hang, with an identical 13 moving-young fallbacks |
| `TestCharChunkLargeHeap` | rc=137 | PASS | host OOM kill, see below |

`rc=137` is SIGKILL. `dmesg` names it directly:
`Out of memory: Killed process … (cratonvm-jitnew) … anon-rss:9896720kB` — the
`-Xmx10g` `LargeHeap` class on a box where another session's `rustfmt` (15.5 GB
RSS) had been OOM-killed 20 minutes earlier. Not a VM defect; the baseline's
PASS 10 s earlier simply landed before memory pressure peaked.

#### Per-class back-to-back rerun of ALL 27 candidates

The decisive measurement. Every one of the 27 classes that looked like a
regression in the full suite, re-run with the two binaries adjacent in time:

| round 1 | PASS | FAIL | HANG | NOSUMMARY |
|---|---|---|---|---|
| fix | **15** | 10 | 1 | 1 |
| baseline | 12 | 14 | 1 | 0 |

On the *worst-case-selected* subset — the classes hand-picked because the fix
looked worse — the fix passes MORE of them than the baseline does. 11 classes
disagree: 7 toward the fix, 4 against (3 `DoHead` flips plus the OOM-killed
`LargeHeap`). There is no regression here; these classes are flaky, and the
full-suite diff was measuring host load, not the change.

#### `TestWsRemoteEndpointImplServerDeadlock` — the one that needed chasing

This is where a regression would have been easiest to believe. Across the full
suite and two rerun rounds the fix hung it 3/3 while the baseline passed it
2/3, and its log shows the JIT-frame-driven moving-young fallback
(`xt-helper-window-conservative-scan`, `compiled-frame-oop-not-published`,
`active-safepoint-map-incomplete`) firing thousands of times — a plausible
mechanism for "more methods compile ⇒ more live JIT frames at GC ⇒ GC thrash".

Run in isolation on a quiet box (load 6), one binary immediately after the
other, 20 rounds each with the order alternated so neither arm always goes
first:

| | PASS | HANG | hang rate |
|---|---|---|---|
| fix | 10 | 10 | 50% |
| baseline | 13 | 7 | 35% |

Two-proportion z ≈ 0.96, p ≈ 0.34 — not distinguishable from noise at n=20.
**This test hangs 35-50% of the time under CratonVM on either binary.** It is
badly flaky, and the full-suite 3/3-vs-2/3 split was a short unlucky streak.

`--stack-dump-on-timeout=240` shows both arms' hangs are the same hang: `main`
blocked in `TestWsRemoteEndpointImplServerDeadlock$Bug66508Client.onMessage` ←
`PojoMessageHandlerWholeBase.onMessage` ← `WsFrameBase.sendMessageText`, with
every `http-nio-*-exec-*` worker parked in `LinkedBlockingQueue.take` — the
test's own *intentional* temporary deadlock (Tomcat bug 66508) failing to
resolve. Pre-existing, and worth its own investigation independent of this
change.

The moving-young fallbacks turned out to be a **consequence** of the hang, not
a cause. Counting the last fallback number per run: passing runs sit at 16-64
on both arms (fix 32/64/16/32, baseline 32/32/32/16/32/32); only hanging runs
reach 512+, because the hang itself spends 240 s GC-cycling. So the tempting
"more methods compile ⇒ more live JIT frames ⇒ more non-moving fallbacks ⇒ GC
thrash" story is refuted: the fix does not run more fallbacks when the test
passes.
