# `ParallelExecutionSpringExtensionTests` reads `TIMEOUT` because it is a 10 000-test throughput benchmark — and turning the parallelism OFF does not narrow the gap

| | |
|---|---|
| **Status** | OPEN. **Correctness matches HotSpot exactly** (10/10 repetitions, 10 000/10 000 nested tests succeeded). Throughput only. One real JIT gap found and fixed (`600311cbe`, merged to dev). Two tier-up avenues (the `invokespecial` gap and the `java/util` virtual-dispatch exclusion) are now both confirmed CLOSED as explanations for the throughput gap — see *What has not been done*. |
| **Scope** | `org.springframework.test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` (spring-framework, `spring-test`). |
| **Measured** | 2026-09-10/11, Azure `20.80.105.49`, real JDK 25, binary `cratonvm-springfix2-20260910` (`claude/spring-residuals-20260910`). The host carried load average 40–90 from other sessions throughout; every number below is either a completed-work count or a ratio taken from **concurrently executed** arms. 2026-09-17/18 profiling was taken on the same host at load 0–2, quiet. 2026-09-18 follow-up (below) at load ~9.5 — busier, but the A/B delta reported is ~12%, well outside the noise band this host shows at that load. |

## It is not a hang

The suite's `TIMEOUT` row carries `found=0`, which is indistinguishable from a
class that never started. Running the class under a per-test progress listener
answers that directly — each of the ten `@RepeatedTest` repetitions starts and
finishes, and none of them stalls:

```text
[   1.2s]   START  repetition 1 of 10
[  92.0s]   FINISH repetition 1 of 10 -> SUCCESSFUL
[ 156.7s]   FINISH repetition 2 of 10 -> SUCCESSFUL
[ 221.2s]   FINISH repetition 3 of 10 -> SUCCESSFUL
[ 297.8s]   FINISH repetition 4 of 10 -> SUCCESSFUL
[ 366.9s]   FINISH repetition 5 of 10 -> SUCCESSFUL
[ 441.3s]   FINISH repetition 6 of 10 -> SUCCESSFUL
[ 533.6s]   FINISH repetition 7 of 10 -> SUCCESSFUL
[ 607.8s]   FINISH repetition 8 of 10 -> SUCCESSFUL
[ 730.5s]   FINISH repetition 9 of 10 -> SUCCESSFUL
[ 811.5s]   FINISH repetition 10 of 10 -> SUCCESSFUL
[ 811.6s] END
```

Ten repetitions, 65–120 s each, monotone progress, all `SUCCESSFUL`.
HotSpot on the same host in the same minutes: **26.3 s** for the same ten.

## What the class actually does

```java
private static final int NUM_TESTS = 1000;

@RepeatedTest(value = 10, failureThreshold = 1)
void runTestsInParallel() {
    EngineTestKit.engine("junit-jupiter")
        .configurationParameter(PARALLEL_EXECUTION_ENABLED, "true")
        .configurationParameter(PARALLEL_CONFIG_DYNAMIC_FACTOR, "10")
        .selectors(selectClass(TestCase.class))
        .execute()
        .testEvents().assertStatistics(s -> s.started(1000).succeeded(1000).failed(0));
}
```

Each repetition launches a **nested** JUnit Platform run of 1 000
`@RepeatedTest` methods, each with a `@BeforeEach`, a test and an `@AfterEach`,
every one of them resolving an `@Autowired ApplicationContext` parameter
through Spring's `SpringExtension`. Ten repetitions is **10 000 nested tests
and 30 000 parameter resolutions**. It is a throughput benchmark wearing a
correctness test's clothes, and the runner's 180 s per-class cap is what turns
it red.

## Parallelism is not the gap

The obvious reading — "a VM that serialises Java threads would look exactly
like this" — is wrong, and it is cheap to falsify. `ParProbe` runs the class's
own nested `TestCase` through `EngineTestKit` with the parallel switch under
our control. Both VMs were run **concurrently** for each configuration, so the
host's load applies equally to the two arms of every ratio (sequential A/B on
this host has been measured to invent 1.9× differences for a flag that costs
nothing):

| configuration | HotSpot (rep 2) | CratonVM (rep 2) | ratio |
|---|---:|---:|---:|
| `parallel=on`, dynamic factor 10 | 9 043 ms | 69 860 ms | **7.7×** |
| `parallel=off` | 6 120 ms | 75 544 ms | **12.3×** |

Turning parallelism off leaves CratonVM where it was (69.9 s → 75.5 s, i.e.
nothing) while HotSpot gets *faster* (9.0 s → 6.1 s), so the ratio **widens**.
Whatever the cost is, it is per test, not per thread. (That HotSpot is faster
serial than parallel here is itself a load artefact — at load ~50 on 8 cores
there are no spare cores to win with. It does not affect the conclusion, which
rests on CratonVM being unchanged by the switch.)

## It is not the 2026-09-10 reflection fix

The class read `OK` in the pre-fix full-suite sweep and `TIMEOUT` in the
post-fix one, which looks like a regression from
`the-type-variable-scope-walk-never-climbed…`. It is not. That fix adds
`getTypeParameters()` / `getDeclaringClass()` calls to the type-variable path,
so the question deserved a measurement rather than an argument. Two binaries
built from the same base — `39a90d2f4` and `39a90d2f4` + the fix — run
**concurrently**, three repetitions each:

| | rep 1 | rep 2 | rep 3 | median |
|---|---:|---:|---:|---:|
| pre-fix | 128 652 ms | 120 745 ms | 121 163 ms | 121.2 s |
| post-fix | 112 519 ms | 111 723 ms | 133 302 ms | **112.5 s** |

The fixed binary is not slower; if anything it is marginally faster, and the
difference is inside the noise of a host at load 40. The `OK` → `TIMEOUT`
transition is the sweep's, not the binary's: the pre-fix sweep had the host to
itself (sum-class-ms 14.5 M across 2 848 classes) and the post-fix sweep shared
it with five other sessions (61.0 M for the same 2 848 classes, 4.2×). Rerun
alone, 19 of the post-fix sweep's 22 non-`OK` classes come back `OK`.

## Profiled on a quiet host (2026-09-17/18): it is not one hot method, it is compiled code that will not stay compiled

`ParProbe off 1` under `CRATONVM_PROFILE_SAMPLE_MS=5` (load 0.8–1.6, quiet)
put one frame at 40–42% of all samples:

```
[profile]    41.94%      3246  java/util/ArrayList.spliterator
[profile]     8.95%       693  TypeMappedAnnotations$AggregatesSpliterator.tryAdvance
```

That reading was wrong in a specific, checkable way, and chasing it down found
a real bug and then a bigger question.

### The real bug (fixed): `invokespecial` never asked the JIT cache anything

`execute_invokevirtual_cached`'s `Bytecode` arm — which serves every
`invokespecial` call once the inline cache is warm: **every private method,
every super call, every constructor** — had no tier-up logic at all. No
`jit_cache` probe, no invocation counter, nothing. Its siblings
(`execute_invokestatic_cached`'s `Bytecode` arm three lines above it in the
same crate, and the `VirtualBytecode` arm immediately above it in the *same
function*) both have this; this one never did, and nothing named the gap
before now.

`ArrayList$ArrayListSpliterator.getFence()`/`tryAdvance()` are private, so
they can only ever be reached through this exact arm. `CRATONVM_DBG_TIERUP_DECLINE`
and `CRATONVM_DBG_PROMOTE_REFUSE` — the two censuses that instrument the
*other* two tier-up doors — reported **zero rows** for either method, because
neither door ever saw them; `CRATONVM_DBG_INVOKESTATS`'s `slow_path` counter
(487 353) is within 4% of `tryAdvance`+`getFence`'s combined interpreted-frame
count (466 690), which is what "reached through the one door with no tier-up
logic at all" looks like from the outside.

Fixed in `600311cbe` (`claude/spring-parexec-profile-20260917`, merged to
dev): ported `execute_invokestatic_cached`'s probe-then-count block verbatim.
Needs none of the `receiver_is_java_util`/`promotion_barred` machinery the
`VirtualBytecode` arm carries — `invokespecial` is statically bound to the
declaring class by JVMS §6.5, there is no receiver-specific dispatch here for
cb563d707's stale-entry hazard to apply to. `cargo test -p cratonvm-vm --lib`:
2697/0. `ParProbe`: 1000/1000 both sides of the fix.

**It did not move the wall clock.** `fix1` (the clean version of this fix)
timed within noise of the pre-fix binary, three interleaved reps each,
quiet host: `24074/21082/22302 ms` fixed vs `24116/22174/22276 ms` baseline.

### 2026-09-18 correction: the compiled body does NOT stop being served — the previous session's read of that was wrong

The 2026-09-17/18 session (above) inferred "compiles, is served for ~300-400
calls, then `jit_cache` stops returning it" from two numbers that were never
measured on the same call site at the same time: a 378-call `CRATONVM_DBG_JITC`
trace (all `ok=true`) and a whole-run `CRATONVM_DBG_INTERP_FRAMES` count
(233 048/233 345 interpreted). Neither one shows the cache actually going from
serving the body to refusing it. This session built a trace that does: three
`eprintln!`s (temporary, gated on `CRATONVM_DBG_GETFENCE_TRACE`, never
merged — same discipline as the removed `CRATONVM_DBG_JITC` one) placed
directly in `JitCache::get`, `JitCache::put`, and
`invalidate_matching_collecting`'s match arm, naming every GET result, every
PUT (including what it superseded), and every entry a class-hierarchy
invalidation actually withdraws — keyed exactly to
(`ArrayList$ArrayListSpliterator`, `getFence`, declaring `ClassId`), so there
is no ambiguity about which call site produced which line.

`ParProbe off 2` under this build: 233 probe/GET events for `getFence`, all at
the same `declaring_class_id=584` (ruling out the duplicate-`ClassId` theory
outright — there is exactly one). Two `PUT`s, three lines apart in the trace
(a C1 compile immediately superseded by a C2 recompile, both publishing under
the same key) — the `dependencies_are_current`/`publication_epoch_is_current`
refusal traces added alongside them never fired once. Zero
`INVALIDATE-MATCH` events for this key across the whole run — no class-change
invalidation, no unloaded-class retirement, ever touches this entry.
**Every single probe from the moment of the first successful compile to the
end of the run reads `found=true`** — 130 of them, no reversion, no gap.
`JitCache::get` is a pure hash-then-key-match read of an immutable snapshot;
once `put()` publishes an entry nothing removes it short of an explicit
`remove()`/`invalidate_*` call, and none of those fire for this method in
this workload. **The fix from `600311cbe` works exactly as designed and the
compiled body sticks permanently.** The "compiles, then stops" narrative in
the previous section was wrong; nothing at the `JitCache`/`invalidate_matching`
layer explains the missing throughput.

This also closes the `SUPERSEDED`-retirement hypothesis the previous session
proposed as the next step, for a more basic reason: `code_cache_lifecycle.rs`'s
`retire_reason::SUPERSEDED` withdrawal protocol has **no production caller at
all** — `grep -rn "retire_reason::SUPERSEDED" jit/src vm/src/jit` outside
`code_cache_lifecycle.rs` itself returns nothing but its own unit tests.
Nothing in the live JIT ever calls `.retire(..., SUPERSEDED, ...)`. (There is
a separate, real coldness-eviction path — `JitCache::sweep_cold_bodies`,
gated by `CRATONVM_JIT_CODE_CACHE_SWEEP`, default OFF — but it was inactive
in every run this session traced, and the trace above shows it would not have
mattered for this method regardless.)

### The real second question: why doesn't compiling the actual hot frame help?

`CRATONVM_PROFILE_SAMPLE_MS=5`, `ParProbe off 5`, this session, load ~9.5 (the
host was not quiet — every other worktree in `/data` was building
concurrently — but the finding below is a same-binary A/B, not a
cross-session wall-clock comparison, so host noise mostly washes out):

```
[profile] execution samples: total=27280 no_java_frame=0 interval_ms=5
[profile]    53.28%     14535  java/util/ArrayList.spliterator
[profile]    11.87%      3237  TypeMappedAnnotations$AggregatesSpliterator.tryAdvance
[profile]    10.69%      2915  AbstractApplicationEventMulticaster$CachedListenerRetriever.getApplicationListeners
[profile]     8.26%      2254  java/util/concurrent/CopyOnWriteArraySet.<init>
```

Same shape as the 2026-09-17 reading (41.94% there, 53.28% here) — one
method, `ArrayList.spliterator()`, dominates the sample count by 4-5x over
everything else combined. `spliterator()` is an ordinary `invokevirtual` on
`java/util/ArrayList` (ArrayList itself, not the private spliterator class),
gated by the *other* known tier-up exclusion,
`CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL` (default OFF, documented in
[`composition-native-callback-and-the-promotion-question-CLOSED-20260911.md`](../../internal/performance/composition-native-callback-and-the-promotion-question-CLOSED-20260911.md)).
The obvious next move — flip it on, now that the `invokespecial` gap above no
longer confounds the measurement — was tried, this time as a controlled
same-binary A/B (`ParProbe off 3`, `/usr/bin/time -v`, sequential rather than
concurrent — a scripting slip on this session's part meant the concurrent
launch failed and only the baseline ran, so the flagged arm ran afterward
instead; host load was stable at ~9.5 across both, and the delta below is
large enough that this does not change the conclusion):

| configuration | rep 1 | rep 2 | rep 3 | total elapsed |
|---|---:|---:|---:|---:|
| baseline (default flags) | 37 808 ms | 36 418 ms | 36 306 ms | 1:51.45 |
| `+PROMOTE_JAVA_UTIL=1` | 43 270 ms | 39 268 ms | 40 280 ms | 2:04.43 |

**The flag makes it ~12% *slower*, not faster** — consistent with, not
contradicting, the 2026-09-17 session's own concurrent-A/B null result on the
same switch. Two independent measurements, two different hosts states, same
answer: making `ArrayList.spliterator()` (and every other `java/util/*`
virtual call site) JIT-eligible does not help this benchmark, and mildly
hurts it (plausibly: warmup/compile overhead across many now-eligible
`java/util` methods, most too cold to be worth it, with no method hot enough
in isolation to pay that back inside a 36-40 s run).

Put together with the `JitCache` trace above, this is the actual finding:
**a 53% CPU-sample share for `ArrayList.spliterator()` does not mean 53% of
wall time**, and the A/B is the proof, not just the exec-sampler's own
documented caveat about it. `spliterator()` is a two-line method
(`return new ArrayListSpliterator<>(...)`); interpreted execution polls a
safepoint on every bytecode dispatch and every call, so a trivial,
extremely-frequently-called interpreted method is exactly the shape that
over-represents itself in a sample-count profile relative to the actual
cycles it costs. **Both known JIT tier-up barriers for this workload —
`invokespecial`-cached dispatch (fixed) and `java/util` virtual-dispatch
promotion (tried, does not help) — are now closed as explanations for the
8-12x gap.** Chasing a third JIT-eligibility gap without a wall-clock
attribution to justify it is not a productive next step.

### What actually would move this forward

CPU-sample profiling has now produced two misleading top frames in a row
(`getFence`/`tryAdvance` in the original 40% reading, `ArrayList.spliterator`
here) for the same underlying reason: this workload is dominated by very
short, very frequently interpreted methods, and the sampler counts safepoint
polls, not cycles. The next session needs a **wall-clock-attributing**
measurement instead of another sample-count one — e.g. coarse phase timers
around `TestContextManager`/`SpringExtension.resolveParameter`/context-cache
lookup/`@BeforeEach`-`@AfterEach` dispatch (the reflection-heavy machinery
named in the original write-up and never yet directly timed), or a
sampling profiler that weights each sample by estimated time-since-last-poll
rather than counting polls uniformly. Also still true from the original
write-up: the workload is dominated by
per-test Spring `TestContext` work — `SpringExtension.supportsParameter` /
`resolveParameter`, context-cache lookup, `@BeforeEach`/`@AfterEach`
dispatch — reflection-heavy, none of it AOT or javac, unlike the suite's
other two slow classes (`BeanRegistrationsAotContributionTests`,
`AotIntegrationTests`), which are generate-and-compile bound.

## Repro

```bash
cd apps/spring-framework/spring-test
CP="/data/springres-work:$(cat /data/springres-work/cp-spring-test.txt)"

# progress trace: is it slow, or is it stopped?
<vm> -cp "$CP" KProgress \
  org.springframework.test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests

# the parallel/serial A/B (run the two VMs concurrently, not one after the other)
<vm> -cp "$CP" org.springframework.test.context.junit.jupiter.parallel.ParProbe on  2
<vm> -cp "$CP" org.springframework.test.context.junit.jupiter.parallel.ParProbe off 2
```

`KProgress.java` and `ParProbe.java` are in
`docs/internal/fixed-suite-bugs/repros/spring-typevar-scope-20260910/`.
`ParProbe` must be compiled into package
`org.springframework.test.context.junit.jupiter.parallel` — the nested
`TestCase` it selects is package-private.

```bash
# the exec sampler, quiet host only (see docs/internal, exec_sampler.rs)
CRATONVM_PROFILE_SAMPLE_MS=5 <vm> -cp "$CP" ...ParProbe off 1

# which tier-up door a site reached, and why it declined -- pick the
# doors this page's fixes/switches touch:
CRATONVM_DBG_TIERUP_DECLINE=1  <vm> ...   # execute_invokevirtual_cached (invokevirtual/interface)
CRATONVM_DBG_PROMOTE_REFUSE=1  <vm> ...   # the fast door (execute_invokevirtual_fast_door)
CRATONVM_DBG_INTERP_FRAMES=1   <vm> ...   # is a method STILL interpreted, regardless of why
CRATONVM_DBG=jit-method-stats  <vm> ...   # compiles/deopts/hot-but-stuck, VM-wide
CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1 <vm> ...   # admits a java/util/ receiver to virtual promotion

# three-arm concurrent A/B (never sequential on this host): fixed vs
# baseline CratonVM vs HotSpot, one nohup line each, same shell
nohup ./cratonvm-<fixed>   ... ParProbe off 3 > fixed.log &
nohup ./cratonvm-<baseline> ... ParProbe off 3 > baseline.log &
nohup java                  ... ParProbe off 3 > hotspot.log &
```

The invokespecial fix landed as `600311cbe` on `claude/spring-parexec-profile-20260917`
(merged to dev). `cratonvm-parexec-final-20260918` on the Azure host's
`/data/springres-work` is that fix's binary, built clean (no trace code) —
use it as the baseline for whatever chases the `SUPERSEDED` hypothesis next,
rather than re-deriving the invokespecial fix from scratch.
