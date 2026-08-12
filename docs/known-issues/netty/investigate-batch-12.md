# netty — investigate batch 12 of 13

**TRIAGED 2026-08-12 — 10 of 15 now pass** (was 4), on branch
`fix/netty-util-batch1213-20260812`. Three CratonVM defects were root-caused
and fixed; the rest are characterised below, one filed separately and one an
accepted divergence. See "Resolution" at the bottom.

Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md (FIXED 2026-08-12)`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.util.NettyRuntimeTests` | FAIL/HANG | default=HANG, g1=FAIL, zgc=FAIL |
| `io.netty.util.RecyclerFastThreadLocalTest` | FAIL | default=FAIL, zgc=FAIL |
| `io.netty.util.RecyclerTest` | FAIL | default=FAIL, zgc=FAIL |
| `io.netty.util.ResourceLeakDetectorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.ThreadDeathWatcherTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.AutoScalingEventExecutorChooserFactoryTest` | FAIL | default=FAIL, g1=FAIL |
| `io.netty.util.concurrent.DefaultPromiseTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.util.concurrent.DefaultThreadFactoryTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.FastThreadLocalTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.util.concurrent.NonStickyEventExecutorGroupTest` | FAIL/HANG | default=FAIL, g1=HANG, zgc=FAIL |
| `io.netty.util.concurrent.PromiseAggregatorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.PromiseCombinerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.PromiseNotifierTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.internal.JfrEventSafeTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.internal.TypeParameterMatcherTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

## Resolution (2026-08-12, Linux/Azure host, JDK 25, real-jdk mode)

Measured against stock HotSpot JDK 25 on the same classpath, one VM per class.
**Read the HotSpot column first** — three of these classes do not pass on
HotSpot either, so "green" was never the target for them.

| class | HotSpot | CratonVM before | CratonVM after | cause |
|---|---|---|---|---|
| `NettyRuntimeTests` | 7/7 | HANG (240s) | **7/7 in 0.7s** | synthetic `CyclicBarrier` (fixed) |
| `RecyclerFastThreadLocalTest` | 67/67 | 67/67 | 67/67 | already fixed on dev |
| `RecyclerTest` | 59 ok, 8 aborted | same | same | matches HotSpot |
| `ResourceLeakDetectorTest` | **2 FAIL** | 1 fail | 1 fail | disjoint from HotSpot's — see below |
| `ThreadDeathWatcherTest` | 3/3 | 3/3 | 3/3 | already fixed on dev |
| `AutoScalingEventExecutorChooserFactoryTest` | **1 FAIL** | 7/7 | 7/7 | CratonVM better than the oracle |
| `DefaultPromiseTest` | 20/20 | 18/20 | 18/20 | JIT-only, not root-caused — see below |
| `DefaultThreadFactoryTest` | 3 ok, **2 aborted** | 1 fail | 1 fail | accepted divergence — see below |
| `FastThreadLocalTest` | 13 ok, 3 skipped | HANG | HANG | filed: JIT drops ctor side effects |
| `NonStickyEventExecutorGroupTest` | 10/10 | 10/10 | 10/10 | flaky under host load only |
| `PromiseAggregatorTest` | 6/6 | 2/6 | **6/6** | `StackWalker$Option` (fixed) |
| `PromiseCombinerTest` | 12/12 | 2/12 | **12/12** | `StackWalker$Option` (fixed) |
| `PromiseNotifierTest` | 5/5 | 3/5 | **5/5** | `StackWalker$Option` (fixed) |
| `JfrEventSafeTest` | 3/3 | 1/3 | 1/3 | JFR event delivery — see below |
| `TypeParameterMatcherTest` | 9/9 | 8/9 | **9/9** | `TypeVariable` scope walk (fixed) |

### Fixed in this branch

1. **`StackWalker$Option` built nameless enum constants** — `Enum.valueOf`
   threw for every name, killing Mockito's `Java9PlusLocationImpl.<clinit>`
   and with it every Mockito-based test. Full write-up in
   [investigate-batch-13.md](investigate-batch-13.md). Accounts for the three
   `Promise*Test` classes here.

2. **Synthetic `CyclicBarrier` loses a waiter's release.** CratonVM replaced
   `java.util.concurrent.CyclicBarrier` with a native implementation that has
   **no generation**: a trip resets one shared `count` to 0 and notifies, and
   each waiter decides it was released by re-reading `count == 0`. That test is
   only valid while nobody re-enters the barrier. In a tight loop a released
   waiter can be preempted before it re-reads; a faster party then bumps
   `count` back above 0, so the waiter concludes it was *not* released and
   waits again — with its wake-up already spent. The barrier is permanently one
   party short and every later trip deadlocks.

   Reproduced with no netty at all: a 4-party × 20-round `CyclicBarrier` hung
   **10/10 runs with the JIT on and 10/10 with `--nojit`**, while the identical
   loop over a hand-written `ReentrantLock`+`Condition` barrier, over
   `synchronized`/`wait`/`notifyAll`, and over a verbatim copy of the JDK's own
   `dowait` in an application class all passed. The timed `await` also threw
   `IllegalStateException: TimeoutException: …` instead of `TimeoutException`,
   which is what exposed the native.

   Fixed by gating the `CyclicBarrier` natives behind synthetic-AQS mode, the
   way `ReentrantLock`/`Lock`/`Condition` and `Semaphore` already were when
   real AQS became the default — the comment in
   `util_concurrent_ext::register_concurrent_natives` even recorded that
   `CyclicBarrier` had been left behind. The two constructors are gated with
   the `await` natives rather than separately: `native_cb_init` stores its
   `int[3]` state holder in the receiver's slot 0, which on the real JDK layout
   is the `lock` field, so registering only the constructors would hand real
   bytecode a barrier whose `lock` is an `int[]`. After: 0/5 hangs.

3. **`TypeVariable.getGenericDeclaration()` was wrong for anonymous/local
   classes.** The lexical scope walk in `native-builtins/src/generics.rs`
   climbed via `getDeclaringClass()`, which is **null** for anonymous and local
   classes (they are not members of their enclosing class), so it stopped at
   the anonymous class and attributed the variable to it. For
   `new U<E>() { }` inside `class V<E>`, HotSpot reports `E`'s declaration as
   `V`; CratonVM reported `V$1`. netty's `ReflectionUtil.resolveTypeParameter`
   then asks `V$1.isAssignableFrom(V$1)` — true instead of false — loops, walks
   off the end of the superclass chain and throws "cannot determine the type of
   the type parameter 'E'". Fixed by falling back to `getEnclosingClass()`,
   consulted only when `getDeclaringClass()` yields nothing so a
   `Method`/`Constructor` scope never reaches it.

### Filed separately

4. **`FastThreadLocalTest` — the JIT discards constructor side effects.** See
   [jit-elided-constructor-side-effects-20260812.md](jit-elided-constructor-side-effects-20260812.md).
   `is_trivial_void_init` in `jit/src/x64/driver.rs` is computed from the
   constructor's *signature* alone, so any no-arg `()V` constructor is treated
   as empty and elided along with its writes to global state. Minimal repro: a
   loop of 1 000 000 `new` whose ctor does `++someStaticInt` leaves the counter
   at **0** with the JIT on and at 1 000 000 with `--nojit` and on HotSpot.
   `testConstructionWithIndex` loops until a counter advanced *by the
   constructor* reaches its limit, so the increments stop and the loop never
   terminates. Not fixed here: the correct fix threads the existing
   `cp_elidable_init_resolver` (which the IR backend already uses to check the
   ctor *body*) into the single-pass backend, and needs performance validation.
   Note that fix alone likely will not green this class — the loop is ~2.1
   billion iterations by construction.

### Open, JIT-attributable, not root-caused

9. **`DefaultPromiseTest` fails only with the JIT on.** Six runs of the class
   alone on the fixed binary:

   | mode | run 1 | run 2 | run 3 |
   |---|---|---|---|
   | JIT on | 2 failed | 2 failed | 3 failed |
   | `--nojit` | **0 failed** | **0 failed** | **0 failed** |

   So this is a JIT defect, not host-load flakiness — worth stating explicitly
   because the failing assertion *is* a wall-clock one
   (`assertTrue(latch.await(2, TimeUnit.SECONDS))` at
   `DefaultPromiseTest.java:453`, reached from
   `testNoStackOverflowWithDefaultEventExecutorA/B` and
   `testStackOverFlowChainedFuturesB`), which on a shared host would normally
   be the first thing to suspect. Three clean `--nojit` runs rule that out.

   **Not** obviously the same defect as the constructor-elision bug filed in
   [jit-elided-constructor-side-effects-20260812.md](jit-elided-constructor-side-effects-20260812.md):
   the promises here are built with a **1-arg** constructor
   (`new DefaultPromise<Void>(executor)`) whose result is stored into an array,
   so the receiver escapes and the `()V` trivial-init path does not apply.
   The symptom is that the chained-listener cascade does not drive
   `latch.countDown()` to zero within 2s. Next step for whoever picks this up:
   raise the latch deadline to prove it is a *lost* notification rather than a
   slow one, then bisect with the JIT tier/inlining levers.

### Not CratonVM defects / accepted divergences

5. **`DefaultThreadFactoryTest` — accepted divergence, do not "fix".** The
   failing test calls `System.setSecurityManager(...)`, which on JDK 24+
   (JEP 486) always throws `UnsupportedOperationException`; the test catches
   that and skips, which is why HotSpot reports `aborted=2`. CratonVM
   deliberately does **not** adopt JEP 486 — the decision and its reasoning are
   recorded in `native-builtins/src/security_manager.rs` (2026-07-29): the
   installed manager really does gate `Runtime.exec`/`ProcessBuilder.start` and
   the Panama host-call path, so throwing here would remove the only sandbox
   the VM has. netty 4.2's `DefaultThreadFactory` no longer consults the
   SecurityManager at all (`this.threadGroup = threadGroup;`, null for the
   1-arg constructor), so the test can only pass by being skipped. Matching
   HotSpot therefore requires adopting JEP 486 and nothing else — revisit only
   together with a replacement for the exec/Panama gating.

6. **`ResourceLeakDetectorTest` — failure sets are disjoint from HotSpot's.**
   HotSpot fails `testLeakSetupHints` and `testLeakBrokenHint`; CratonVM
   **passes both** and fails only `testConcurrentUsage`, which times out at its
   own 60s `@Timeout` (same with `--nojit`, so not JIT-related). A throughput
   story, not a correctness one; not yet root-caused.

7. **`JfrEventSafeTest`** — `enableDefaults()` and `simple()` fail because a
   `jdk.jfr.consumer.RecordingStream` never delivers the committed event, so
   the test's `CompletableFuture.get(10s)` times out. Identical with `--nojit`.
   This is a missing-JFR-machinery gap, not a regression.

8. **`NonStickyEventExecutorGroupTest`** — recorded 8/10 in one sweep, but 4/4
   clean when run alone on the fixed binary (and HotSpot is 4/4 alone too). Host
   load under the sweep, not a defect and not a regression from these fixes.

