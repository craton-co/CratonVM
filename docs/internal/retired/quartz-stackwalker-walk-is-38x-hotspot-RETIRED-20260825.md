# RETIRED — `QuartzEndpointWebIntegrationTests` passes 45/45; the hang was gone from pristine dev, and this page's "full fix" is a regression

**Status: RETIRED 2026-08-25. The class passes 45/45 under the shipped default,
and it does so on PRISTINE dev — this page's symptom was already gone before any
of the work below was written. Three real VM defects were found underneath it
and are fixed; the stack-walk throughput residual is 3.8x better on the shape
that pays it. Both of this page's nominated fixes were built and measured, and
the one it called "the only one with the right ceiling" is a REGRESSION.**

## What the class actually does now

`MEMCAP=8G /data/sb4.sh … 600`, one process per arm, ABBA-interleaved, on a
quiet box (load 27). The loop count is this page's own instrument
(`grep -c 'QuartzEndpoint.triggerQuartzJob'`):

| arm | result | wall | `triggerQuartzJob` frames |
|---|---|---:|---:|
| HotSpot 25 | ✓ 45/45, 0 failed | 42 s | — |
| CratonVM, pristine dev | ✓ 45/45, 0 failed | 189 / 234 s | **8** |
| CratonVM, this branch | ✓ 45/45, 0 failed | 163 / 150 s | **8** |
| CratonVM, `--nojit` | ✓ 45/45, 0 failed | 224 s | **8** |

This page records the class as OOM-killed at ~24 s with 22 GB RSS, driven by
~23,700 complete server dispatches for ONE client request, with `--nojit` the
only passing arm. Measured now: **8** frames, not 47,362. No spin, no OOM, no
hang, and **the JIT arm is the FASTER one** — which inverts this page's central
thesis ("it is not that `--nojit` avoids a bug, it is that `--nojit` is *faster*
on the operation the workload is bottlenecked on").

Something between 2026-08-18 and 2026-08-25 closed it. This page does not know
what, and did not find out: by the time it was re-measured the symptom was
already absent from pristine dev, so there was nothing left to bisect against.

**A warning about this class, which cost a full re-measurement.** An earlier run
of the same arms reported `tests=45 failed=27` for pristine dev and two 420 s
`NO-RESULT` timeouts. That run climbed from load 52 to load 113 while it
executed. At load 27 the identical binaries are clean. This class's verdict is
decided by host load as much as by the binary — exactly as the "Run it under a
cap" note below says about memory, and it applies to PASS/FAIL too. Do not
report an arm from this class without the load beside it.

## What was fixed, and how it was found

The path switch in item 3 is what exposed items 1 and 2: `p59_sw_walk`
intercepts `StackWalker.walk` in real-JDK mode, so `lang_stackwalker.rs`'s
`callStackWalk` / `fetchStackFrames` — and everything they call — had no reachable
consumer for `walk()` at all. `CRATONVM_DEBUG_STACKWALK=1` on a walk logged
**zero** `callStackWalk capture` lines. Three defects had accumulated behind that.

1. **`ClassFrameInfo.declaringClass()` enforced RETAIN_CLASS_REFERENCE.** The
   JDK says the opposite on the declaration itself:

   ```java
   // package-private called by StackStreamFactory to skip
   // the capability check
   Class<?> declaringClass() { return (Class<?>) classOrMemberName; }
   ```

   and `ClassFrameInfo.getClassName()` is `declaringClass().getName()`. The guard
   blocked `StackStreamFactory$StackFrameBuffer.at(int)`, which the JDK runs for
   EVERY populated frame inside `setBatch()`, so a plain
   `StackWalker.getInstance().walk(…)` threw `UnsupportedOperationException` out
   of its first batch. The capability check belongs to the public
   `getDeclaringClass()` (`ensureRetainClassRefEnabled(); return declaringClass();`),
   which now has its own wrapper.

2. **`populate_sfi` wrote `StackTraceElement` at synthetic-stub offsets.** Slots
   `0..3` are `[class, method, file, line]` on the stub; real JDK 25 declares
   `classLoaderName, moduleName, moduleVersion, declaringClass, methodName,
   fileName, lineNumber, declaringClassObject`, so every write landed a
   field-group early. `getClassName()` read a null loader name, `getLineNumber()`
   read a `String` slot, and `StackFrameInfo.toString()` NPE'd inside
   `StackTraceElement.computeFormat()` on a null `declaringClass`. Now resolved
   by name and memoized, `declaringClassObject` included — `computeFormat()`
   dereferences it with no null guard.

3. **`frame_class_ids` skipped JIT-compiled frames.** It mapped `thread.frames`
   alone, and a compiled method pushes no interpreter `Frame`, so every
   caller-attribution site blamed the next frame down. It fails in BOTH
   directions: a `java.base` caller that tiered up disappears and a classpath
   frame is blamed — `StackFrameBuffer.fill` constructing `StackFrameInfo` was
   denied with `module java.base does not "opens java.lang" to unnamed module`,
   **and only with the JIT on**, which is the discriminator that named it — and
   symmetrically a compiled APPLICATION frame disappears behind a JDK frame and
   is granted access it should not have. The same list backs `Class.forName`'s
   caller-loader resolution.

## Both nominated fixes, built and measured

This page names two. Neither survives in the form it was written.

### "The full fix, and the only one with the right ceiling" — a REGRESSION

Routing real-JDK `walk`/`forEach` through the JDK's own batched
`StackStreamFactory` is **slower at every depth**. ABBA in one binary,
`probes/StackWalkerTerminationProbe.java`, 2000 iterations, two runs per arm:

| depth | eager native | JDK batched |
|---:|---|---|
| 2 | **706 / 740** | 3,039 / 4,386 |
| 40 | **4,748 / 6,400** | 12,453 / 14,609 |
| 120 | **25,384 / 30,024** | 37,459 / 41,980 |

Both arms byte-identical to HotSpot, so this is a clean throughput comparison.
The reason is that the JDK's laziness is written in **Java**: a reflective
`Constructor.newInstance` per buffer slot, an `Array.newInstance`, a
spliterator, the `doStackWalk` re-entry, and a native call per frame for
`StackFrameBuffer.at`. Interpreted, that fixed cost (~300 µs/walk at depth 2
against the eager native's ~88 µs) is larger than the per-frame cost batching
saves, and its per-frame slope is worse too.

It is kept behind `CRATONVM_SW_JDK_WALK=1`, because it is the arm that proves
the above and the only thing that exercises `callStackWalk`/`fetchStackFrames`
at all — which is how items 1–3 went unnoticed.

### "The smaller change" — real, bounded, and right for a reason this page does not give

Making the frame's strings and mirror lazy is worth ~40% on the early-match
shape (depth 40, 326 → 195 ms). But this page's profile cannot justify it:
`populate_stack_frame`, `create_string`, `try_alloc_concurrent_synthetic` and
`get_class_mirror` **do not appear in the profile at all** above 0.7%. It works
because the walk's cost is the object COUNT, not any one symbol — the `near`
profile is flat with ~24% in mimalloc's `mmap` and no resolvable Rust caller.

Deferring the mirror does NOT reintroduce the `<clinit>` failure the eager
resolve exists to avoid: the carrier stores the same frame-captured `ClassId`,
so only the LOOKUP moved. `StackWalkerLog4jCallerProbe` and
`StackWalkerLog4jStressProbe` are the fixtures for that case and both pass.

## The ceiling argument was aimed at the wrong layer

This page prices the residual from `native_stack_has_jit_frame` at 17.9% and
concludes "deleting it entirely buys 1.2x against a 38x gap", then that the rest
is "a native call made from a JIT frame pays a per-call conservative root
deposit". The caller census disagrees:

```
scan_active_jit_frames by caller: gc-roots=0 safepoint=0 blocked-deposit=86,700
jitprobe calls=86,081 words=637,620,424
```

Not GC, not the safepoint, **not the native call**. It is
`NativeContext::refresh_root_snapshot()`, which the synthetic-stream drain loops
call once per ELEMENT, each call a full deposit — 638 million words, 5.1 GB of
native stack read, for 500 walks over a 120-frame stack, at a constant ~7,400
words per call with the call COUNT scaling linearly in depth because a
`StackWalker` stream has one element per frame.

The pins that refresh exists to publish are pushed ONCE before the loop and
never change, so all 86,700 deposits republished a bit-identical snapshot.
`refresh_root_snapshot`'s own contract asks for a publish "right after
establishing such a batch of pins (**and optionally again periodically**)" — the
per-iteration half is the optional one. Now every 32nd element
(`CRATONVM_GC_STREAM_REFRESH_EACH=1` restores the old cadence in the same
binary):

| shape | depth | HotSpot | per-element | periodic |
|---|---:|---:|---:|---:|
| full drain | 40 | 159 | 1,234 | **615** |
| full drain | 120 | 221 | 5,681 | **1,508** |
| early match | 40 | 30 | 178 | 165 |
| early match | 120 | 20 | 476 | 449 |

**3.8x at depth 120, and 25.7x → 6.8x against HotSpot.** The early-match control
does not move, correctly: it already made ~1.7 deposits per walk, flat in depth,
against 37 (depth 40) and 105 (depth 120) for the full drain. That split is also
what makes the two shapes worth separating at all —
`StackWalkerTerminationProbe` cannot see it, because three of its four arms
traverse the whole stack by construction, so batching can only ADD round trips
to them. `probes/StackWalkerFindFirstProbe.java` was added for that.

**Not done, deliberately:** skipping `scan_active_jit_frames` on the
non-blocking deposit, which would remove the remaining scans.
`collect_all_root_snapshots` consumes every ALIVE thread's snapshot, not only
blocked ones, so that is a GC-safety change and being wrong there is a reclaimed
live object, not a slow one.

## What is left

`StackWalker.walk` over a stack it fully drains is still ~7x HotSpot at depth
120 (1,508 ms against 221). That residual is the per-object-returning-native-call
conservative root scan — one scan per frame the stream touches, ~59 KB of native
stack each — and it is genuinely architectural. It is NOT specific to
`StackWalker`, has no failing witness left on this page, and the memo route into
it is closed by construction (see "the memo is 100% cold" below, which still
stands).

## Verification

`probes/StackWalkerCrossVmProbe.java` (added with this) is the differential
oracle: 17 rungs — collect, count, findFirst hit and miss, skip, limit, frame
fields, `toStackTraceElement`, RETAIN_CLASS_REFERENCE including the negative
half, `forEach`, `getCallerClass`, nested walks, 200x repeat stability, depth-40,
reflect frames — byte-identical to HotSpot 25 in BOTH walk arms. All four
`vm/tests/wp1_9_stackwalker.rs` integration probes pass in both arms.

---

*Everything below is the original page as it stood on 2026-08-18, kept because
its refuted hypotheses and its four inert memo attempts are still the record
that stops them being re-run. Read the status above first: the class passes, the
"full fix" it recommends is a regression, and its ceiling argument names the
wrong caller.*


Found re-measuring the four classes on
retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md. That
page records this class as green under the shipped default. It is not, on Linux,
on current dev — and the failure is not that page's mechanism: there are **zero**
`[moving-young]` fallback lines in the run.

## What it actually is

The class runs its request against the Quartz actuator endpoint over and over,
forever. Counted by instrumenting the throwable/frame capture
(`CRATONVM_DBG_STTRACE=1`) and tallying `QuartzEndpoint.triggerQuartzJob`
frames:

| arm | `triggerQuartzJob` frames | window | outcome |
|---|---:|---|---|
| `--nojit` — **the whole passing run** | **66** | ~110 s | ✓45/45 |
| JIT on | **47,362** | 22 s | OOM-killed |

23,681 separate captured traces carry exactly **three** `triggerQuartzJob`
frames (`QuartzEndpointWebExtension:97` → `QuartzEndpoint:227` →
`QuartzEndpoint:231`) and none carry more, so this is **not** runaway recursion —
it is ~24,000 *separate, complete HTTP requests* through a three-deep call nest
that should run about 45 times.

The logs are quiet: 4 `IllegalStateException`s in the whole run and no repeated
exception. The requests are not failing and being retried by an error handler;
something is polling for a condition it never sees. `--nojit` sees it.

**The 22 GB is a symptom, not the bug.** Each iteration captures a stack trace
(up to 240 frames), so RSS climbs ~350 MB/s until the kernel OOM-killer takes
the process — 21.9 GB unconstrained, ~24 s under an 8 GB cgroup cap. That is why
`--Xmx` has no effect (the Java heap reads ~1 MB throughout) and why all three
collectors behave identically. Stop the loop and the memory goes with it.

**Run it under a cap.** This is a shared box and an unconstrained arm will OOM
other people's work:
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0 …`

## Measured

One class per process, `--Xmx 2g`, real JDK 25 backend, the three load-bearing
suite env vars, RSS sampled every 2 s.

| arm | peak RSS | outcome |
|---|---:|---|
| HotSpot 25 | **385 MB** | ✓45/45 in 8 s |
| CratonVM, ZGC (shipped default) | 7,867 MB | OOM-killed @ ~24 s |
| CratonVM, `--XX:UseGc G1` | 7,491 MB | OOM-killed @ ~24 s |
| CratonVM, `-XX:+UseGenerationalGC` | 7,998 MB | OOM-killed @ ~24 s |
| CratonVM, `--nojit` | 2,741 MB | **✓45/45** in ~110 s |

Collector-independent, so not a GC bug. `--nojit` clears it, so it is the JIT —
which is the same thesis the retired page reached for these classes by a
different route.

## 2026-08-18, later: the exception traffic is NOT the signal, and a warning about the instrument

An earlier pass on this page reported "18,069 `NullPointerException`s, 18,066 of
them thrown at `TomcatEmbeddedWebappClassLoader.loadClass`". **That number is an
artefact and both halves of it are wrong.** It came from pairing a debug line
that printed the throwable's class with a *separate* debug line that printed
frames. Several threads capture concurrently, `eprintln!` interleaves, and
pairing "the last class line" with "the next frame line" attributes frames to
the wrong throwable. The same defect in the opposite direction made the frame
list read outermost-first, so "the deepest frame" was the thread entry point.

With class and frames emitted on ONE line (`STTRACE_DBG_TOP`, three commits on
this branch), the real distribution over 25 s is:

| throwable | count | where |
|---|---:|---|
| `NoSuchMethodException` | 1,324 | Jersey `AnnotatedMethod.findAnnotatedMethod`, Spring `DisposableBeanAdapter.inferDestroyMethodsIfNecessary` |
| `ClassNotFoundException` | 85 | ordinary optional-dependency probing |
| `NoSuchBeanDefinitionException` | 70 | ordinary Spring wiring |
| `NullPointerException` | **2** | `ModuleDescriptor.modsHashCode` during a `<clinit>` |

Every one of those is **ordinary framework reflection** that HotSpot also
performs. There is no exception storm and nothing is failing in a loop. The
stack-trace capture cost is real (it is what turns the spin into 22 GB of RSS)
but it is a consequence of the iteration count, not a cause.

**A correct-looking number from a wrongly-paired log is worse than no number.**
Both mis-pairings produced plausible, confident, specific answers — a named
class and a named throw site — and sent the investigation at
`getClassLoadingLock`, which a 20-line probe then cleared on all three arms
(`ClassLoadingLockProbe`: 400,000 iterations, zero null locks on HotSpot,
CratonVM+JIT and CratonVM `--nojit`).

### What the watchdog dump adds

`--stack-dump-on-timeout 20` while spinning: all four `reactor-http-nio-N`
threads — the `WebTestClient`'s own Netty event loops — are `blocked=true` in
`NioIoHandler.select`, i.e. **the client is idle, waiting for I/O**, while the
server side keeps executing. Whatever is iterating is on the server, not a
client retry loop.

## ROOT CAUSE, 2026-08-18: `StackWalker.walk` throughput

The request thread is `RUNNABLE` and making progress, in the same place in two
watchdog dumps three seconds apart:

```
org.mockito.internal.debugging.LocationImpl.lambda$getStackFrame$2(LocationImpl.java:96)
org.mockito.internal.debugging.LocationImpl.stackWalk(LocationImpl.java:134)
org.mockito.internal.debugging.LocationImpl.getStackFrame(LocationImpl.java:90)
org.mockito.internal.debugging.LocationImpl.<init>(LocationImpl.java:65)
org.mockito.internal.debugging.LocationFactory$DefaultLocationFactory.create
org.mockito.internal.creation.bytebuddy.access.MockMethodInterceptor.doIntercept
org.mockito.internal.creation.bytebuddy.codegen.Scheduler$MockitoMock.getJobDetail
org.springframework.boot.quartz.actuate.endpoint.QuartzEndpoint.triggerQuartzJob
```

Mockito records a `Location` for **every invocation on a mock**, and building
one runs a `StackWalker.walk`. Meanwhile `main` sits in
`reactor.core.publisher.Mono.block` inside `WebTestClient.exchange` — waiting
for a response the server has not finished producing. Nothing is looping;
the server is grinding.

### Measured, isolated

`probes/StackWalkerTerminationProbe.java` — a walk per iteration at depth 40,
with arms for `count`, a matching `findFirst`, a `findFirst` that matches
nothing, and a `skip` past the end of the stack. **All arms terminate correctly
on all three VMs**, so this is throughput, not a non-terminating stream:

| | HotSpot 25 | CratonVM `--nojit` | CratonVM JIT |
|---|---:|---:|---:|
| per `StackWalker.walk` | **61 µs** | 1,461 µs | **2,327 µs** |
| vs HotSpot | 1x | 24x | **38x** |

**The JIT arm is slower than the interpreter arm.** That inverts the usual
picture and is exactly why `--nojit` completes this class and the shipped
default does not — it is not that `--nojit` avoids a bug, it is that `--nojit`
is *faster* on the operation the workload is bottlenecked on.

### Where the time goes

`perf record -F 1999 -g --call-graph fp` on the isolated probe:

| symbol | share |
|---|---:|
| `jit::conservative_roots::native_stack_has_jit_frame` | **17.9%** |
| `ZObjectStarts::contains` | 9.1% |
| `Frame::scan_local_objects_inner` | 7.0% |
| `NativeContextImpl::deposit_root_snapshot_inner` | 6.9% |
| `VmHeap::is_object_address` | 4.9% |
| `drop_glue::<Vec<StackTraceEntry>>` | 4.4% |
| `Frame::scan_locals_conservative` | 4.3% |

**Roughly half the cost is GC root machinery, not stack walking.** Each walk
pays a root-snapshot deposit and a conservative scan of the frame's locals, and
the single largest symbol is `native_stack_has_jit_frame` — the raw word scan
over the stack band above the registered JIT entry chain. That is the same A5
probe the retired moving-young page measured at an **87% false-positive rate**,
here showing up as throughput rather than as a fallback reason. It scans more
when there are compiled frames to scan, which is the mechanism behind the JIT
arm being the slow one.

The memory growth this page originally reported follows from the same place:
`Vec<StackTraceEntry>` per walk, at ~24,000 walks, is the 22 GB.

### ATTEMPTED AND REVERTED: the memo route buys nothing

Two changes were written, built, measured interleaved, and reverted
(`b1ec1981a`). Recorded so the next pass does not spend the same day.

1. **Route the coverage probe through the existing memo.**
   `refresh_moving_young_coverage_for_current_thread` calls
   `native_stack_has_jit_frame` with no memo, and `UnregMemo`'s own doc comment
   names that as the site whose probes "dominate this workload" — so it read as
   the binding inefficiency. Interleaved on the isolated probe:
   4,965 / 4,970 / 5,082 ms before, 4,924 / 4,976 / 4,961 ms after, and
   `native_stack_has_jit_frame` **17.90% → 17.45%** of the profile. The memo
   never engaged.
2. **Lift the memo's compilation-invalidation rule**, which (1)'s inert result
   implicated — every observation was falling through to a full scan. Also
   nothing, A/B'd in ONE binary through its own kill switch: 4,994 vs 6,113 ms,
   then 7,903 vs 7,892 ms. Noise either way.

3. **Turn on the existing `rootsnap-cache`.** The Spring suite sets
   `CRATONVM_JIT=rootsnap-cache` and the isolated probe did not, so the obvious
   suspicion was that the probe simply ran without a cache the real workload
   has. Interleaved, same binary: off 5,007 / 4,972 / 5,014 ms, on
   4,911 / 5,009 / 5,001 ms. **Inert.** (It also means the Spring suite was
   already getting whatever this buys, which is nothing here.)

4. **Make the per-frame method-slot memo thread-local.**
   `find_method_index_memoized` takes a shared `RwLock` read once per FRAME of
   every capture and is the largest symbol in the Quartz profile (18.1%), so it
   looked like the thing that explains the depth scaling. Interleaved: depth 40
   3,028 / 3,921 ms base vs 3,982 / 3,873 ms thread-local; depth 120
   13,816 / 12,200 vs 11,969 / 15,114. **Inert** — an uncontended `parking_lot`
   read is nanoseconds, so that 18% is the hashing and the per-hit verification,
   not the lock. Reverted.

### The mechanism, named exactly

`p59_sw_walk` (`native-builtins/src/phases_late/reflect_invoke.rs:2581`):

```rust
let raw_trace = ctx.capture_stack_trace(0);
let frames = ordered_stack_walk_frames(&raw_trace);
let arr = ctx.new_ref_array(ClassId::new(0), frames.len());
for (i, entry) in frames.iter().enumerate() {
    let sf = populate_stack_frame(ctx, entry, retain_class_ref)?;  // a Java object PER FRAME
    ...
}
```

**Every `walk` materialises a Java `StackFrame` object for every frame on the
stack before the caller's `Function` runs**, so the cost is
`O(depth)` in Java allocations no matter how many frames the consumer reads.
HotSpot fetches frames in BATCHES (8 by default) and only materialises more if
the stream demands them — which is why its line in the table below is flat and
ours is not, and why `findFirst` (what Mockito uses) is nearly free there and
full price here.

And `populate_stack_frame` (`reflect_invoke.rs:2456`) is not cheap per frame. It
does, for EVERY frame:

* `try_alloc_concurrent_synthetic("java/lang/StackWalker$StackFrame", 8)` — the
  by-name class resolution funnel, per frame (the same per-allocation name
  lookup that was worth ~6% when it was memoized out of the bignum natives);
* **four** `create_string` calls — `class_name.replace('/', ".")` (a Rust
  `String` too), `method_name`, `source_file`, and the internal-form class name
  again for `toStackTraceElement()`'s fallback;
* `get_class_mirror(cid)`, eagerly, with a comment explaining that it must be
  eager *at population time* to avoid a by-name lookup failing later;
* five pins, five pin re-reads, eight `set_field`s.

At the Quartz stack depth (~53) that is **~200 Java string allocations per mock
invocation**, and Mockito reads at most a couple of frames before `findFirst`
short-circuits. HotSpot builds the strings in the getters, on demand.

So there are two independent lazinesses to recover, and the second is the
smaller change: make the frame's Strings and mirror lazy (store `class_id` /
`method_index` / bci in the slots and build the derived values in the getter
natives that already exist) even while keeping the eager array. That alone
should take the common `filter(..).findFirst()` shape from `O(depth)` string
allocations to `O(frames actually inspected)`.

The full fix, and the only one on this page with the right ceiling: a
lazy `Stream<StackFrame>` — a spliterator that pulls a batch at a time through a
`fetchFrames(from, count)` native — instead of an eagerly populated array. It is
a real change (a new synthetic spliterator class, a batching native, and the
`forEach`/`getCallerClass` siblings share the same eager path) and it lands in
the code path every exception in the VM traverses, so it wants its own task with
its own tests rather than being bolted on at the end of this one.

### The scaling, measured — and why it is not one symbol

| stack depth | CratonVM | HotSpot | ratio |
|---:|---:|---:|---:|
| 2 | 527 ms | 57 ms | 9x |
| 10 | 893 ms | 80 ms | 11x |
| 40 | 3,752 ms | 108 ms | 35x |
| 120 | 17,098 ms | 187 ms | **91x** |

CratonVM's capture cost is **linear in stack depth**; HotSpot's is nearly flat
(3.3x for 60x the depth, because its walk is lazy and `findFirst` stops at the
first match while ours materialises every frame). So the gap is not a fixed
per-call tax that one memo can remove — it is per-frame work, spread across
`entry_from_frame`'s Arc clones, class lookup, memo probe and line-number scan,
with no member big enough to matter alone. That is why four separate attempts to
remove one member each measured zero.

### MEASURED 2026-08-18: attempt 1 changed a call site that never runs

`CRATONVM_DBG=a5-engagement` (added with this, declared in all four flag files)
counts, per call of the coverage probe, what the memo would have answered.
On `probes/StackWalkerTerminationProbe`:

```
[a5-engagement] calls=0 (probe never ran)
```

**Zero.** `refresh_moving_young_coverage_for_current_thread`'s
`native_stack_has_jit_frame` call — the one `UnregMemo`'s doc comment names as
dominating, and the one attempt 1 memoized — **does not execute on this
workload at all.** The 17.9% comes from the OTHER caller, the detection scan
inside the root-snapshot deposit, which already has the memo.

So attempt 1 was inert because it changed code that never ran, not because
memoizing does not help. It was judged from a profile that did not move, and a
profile cannot distinguish "changed the wrong site" from "the change does not
help" — which is exactly what an engagement counter is for, and why this
codebase's own rule is to print one beside the number. Four attempts were
judged without one.

### And at the site that DOES run, the memo is 100% cold — by construction

Same counter, moved to the detection scan inside the root-snapshot deposit:

```
depth  20:  calls= 37,976   memo_clean=0  memo_banded=0  full_rescan= 37,976   (100%)
depth 120:  calls=188,115   memo_clean=0  memo_banded=0  full_rescan=188,115   (100%)
```

**Not one engagement in 188,115 calls.** `full_rescan` is the
`code_ranges != self.verified_ranges` arm, and `verified_ranges` is only ever
written by `mark_clean` — which is reached ONLY when the probe comes back with
no hit. On a workload with compiled frames the probe hits (the retired
moving-young page measured A5's false-positive rate at **87%**), so `mark_clean`
never runs, `verified_ranges` keeps its initial value, and every observation
falls through to a full rescan **forever**.

That closes the whole memo route, and explains all four attempts at once:

* attempt 1 memoized a call site that never runs (`calls=0`);
* attempt 2 lifted the range-invalidation rule, but with `verified_lo` still at
  its initial `usize::MAX` — `mark_clean` having never run — the `floor`
  comparison still forces a full-width scan, so the lift was neutered by the
  same cause;
* attempts 3 and 4 were unrelated knobs on the same cold path.

**The memo is not under-tuned, it is inapplicable.** It caches "this stack is
free of return-addresses-into-JIT", and on this workload that is simply false
most of the time. No amount of memo work fixes a cache whose predicate is
usually false — which is why the profile never moved and why an engagement
counter, not another profile, was the thing that settled it.

The previously-suspected hypothesis for this site — its
band is `[scanner_sp.max(cover_hi), stack_high)`, and with an empty JIT entry
chain `cover_hi == scanner_sp`, so it scans the whole native stack above the
scanner. `UnregMemo::mark_clean` sets `hiwater = search_lo` on every clean
verdict, so a stack that OSCILLATES — recurse, return, recurse, which is what
every Java workload does and what this probe does 500 times — re-scans instead
of reusing the verdict. That is testable with the same counter: a workload with
a flat stack should show `memo_clean` climbing and an oscillating one should
show `full_rescan` or `memo_banded` dominating. **Measure that before writing
the fifth attempt.**

**The arithmetic that should have come first.**
`native_stack_has_jit_frame` is ~17.9% of this workload, so deleting it
*entirely* buys **1.2x against a 38x gap**. No amount of memoizing that symbol
closes this page, and the profile said so before either change was written. The
lesson is the one the retired moving-young page already taught and this run
re-learned: **price the ceiling from the profile before writing the fix.**

### What is actually left

The gap is not one symbol. Grouped, the isolated probe's profile is roughly half
GC-root machinery (`native_stack_has_jit_frame`, `ZObjectStarts::contains`,
`scan_local_objects_inner`, `deposit_root_snapshot_inner`,
`is_object_address`, `scan_locals_conservative`) and the rest stack-walk and
`Vec<StackTraceEntry>` churn. Closing 38x means making a native call from a JIT
frame stop paying a per-call conservative root deposit at all — the same
"make native→heap interaction cheap in general" conclusion
`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot` and
`bobyqa-numeric-kernel-is-80x-slower-than-hotspot` both reach. This page is a
third witness, not a separate problem.

One structural detail worth carrying: `capture_full_trace` has a cheap path and
an expensive one, and the JIT arm takes the expensive one.

```rust
let jit = active_compiled_frames();
if jit.is_empty() {                       // <- the --nojit path
    return frames.iter().map(|f| entry_from_frame(class_store, f)).collect();
}
interleave_compiled_frames(class_store, frames, &jit)   // <- the JIT path
```

`active_compiled_frames` returns `Vec<(u32, String, u32)>` and does
`cm.method_label.clone()` — a heap allocation per compiled frame per capture,
then a second conversion into the `Arc<str>` a `StackTraceEntry` actually holds.
That is a real inefficiency and it is a plausible-looking lead. **It was not
pursued, on the ceiling argument this page now exists to make**: neither
`active_compiled_frames` nor any string/allocation symbol appears in the
profile's top ten, so it is a low-single-digit item against 38x. Anyone picking
it up should price it from a profile first.

The honest summary for planning: **there is no contained fix on this page.**
Three were tried and measured inert, and the remaining candidates are all
1-5% items. The gap is that a native call made from a JIT frame pays a per-call
conservative root deposit whose cost scales with stack depth, and HotSpot pays
nothing equivalent because it has precise oop maps. That is architectural work
on native->heap interaction, it is the same conclusion
`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot` and
`bobyqa-numeric-kernel-is-80x-slower-than-hotspot` reach from unrelated
workloads, and it should be scoped as its own task rather than as a fix to any
one of the three pages that witness it.

## NAMED, 2026-08-18: `quartzTriggerJobWithUnknownJobKey`, the WebMvc variant only

`@WebEndpointTest` is a parameterized template — 15 methods x 3 web-server
variants = the 45 tests — so `DiscoverySelectors.selectMethod(fqcn, name)`
cannot address one (it returns `tests=0 containersFailed=1` for every method).
`probes/SbRunnerTrace.java` (added with this; the fixture copy lives in `sb-runner/`) registers a
`TestExecutionListener` that prints every test as it starts and finishes,
flushed per line so a kill still leaves the last `@@START` behind:

```
@@DONE  SUCCESSFUL WebFlux
@@START Jersey  | …[test-template:quartzTriggerJobWithUnknownJobKey(WebTestClient)]/[test-template-invocation:#1]
@@DONE  SUCCESSFUL Jersey
@@START WebMvc  | …[test-template:quartzTriggerJobWithUnknownJobKey(WebTestClient)]/[test-template-invocation:#2]
                                    <- never finishes; 5 started, 4 finished
```

HotSpot runs all 45. **CratonVM hangs on the WebMvc invocation of
`quartzTriggerJobWithUnknownJobKey`, and the WebFlux and Jersey invocations of
the SAME method pass.**

The test is four lines, and so is the server path it drives:

```java
client.post().uri("/actuator/quartz/jobs/samples/does-not-exist")
      .contentType(MediaType.APPLICATION_JSON).bodyValue(Map.of("state","running"))
      .exchange().expectStatus().isNotFound();
```
```java
// QuartzEndpointWebExtension:97 -> QuartzEndpoint:227 -> :231
JobDetail jobDetail = this.scheduler.getJobDetail(jobKey);   // mock -> null
if (jobDetail == null) { return null; }                      // -> handleNull -> 404
```

So the request that spins is: **a POST carrying a JSON body, whose handler
returns 404 without ever reading that body, under Spring MVC.**

### What the counts say about the loop

From the same run: `DispatcherServlet.doService` **23,683**,
`RequestMappingHandlerAdapter.invokeHandlerMethod` **23,683**,
`Http11Processor.service` **24,870**, `NioEndpoint$SocketProcessor.doRun`
**24,871** — while the client's four `reactor-http-nio` event loops sit
`blocked=true` in `NioIoHandler.select`. One client request, ~23,700 complete
server-side dispatches. Tomcat is being handed the same socket over and over and
parsing a request from it each time.

**It is not an error dispatch.** `ErrorPageFilter`, `RequestDispatcher`,
`ApplicationDispatcher`, `forward`, `BasicErrorController` and `/error` all
appear **zero** times in the captured frames.

**And it is not raw selector readiness.** `probes/SelectorReadinessProbe.java`
(one client, one 5-byte write, then silence) reports
`readableWakeups=1 reads=1 bytesTotal=5 zeroByteReads=0` identically on HotSpot,
CratonVM+JIT and CratonVM `--nojit`. A drained non-blocking socket stops being
reported readable, correctly, on every arm.

### And the spinning thread is `main`, not a server thread

`--stack-dump-on-timeout 20` reports `tid=0 name="main"` as
`deposit=STALE — thread is RUNNING (blocked=false)`, while every
`reactor-http-nio` client loop is `deposit=live` and parked in
`NioIoHandler.select`. So the thread burning CPU is the **test** thread, and the
~23,700 server dispatches are being *driven* by it, not spontaneously generated
by Tomcat.

That matters because it redirects the search: the loop is on the JUnit/
`WebTestClient` side of `exchange()`, not inside the servlet container. The
watchdog cannot show where — a RUNNING thread's printed chain is its last
*blocking* site, which is stale by definition, and `--stack-sample-ms 25`
produced no sample records on this build, so the sampler needs checking before
it can be relied on here.

The untested hypothesis that fits what is left: an **unconsumed request body**.
Tomcat must swallow the bytes a handler never read before it can treat the
connection as ready for the next request; if that drain does not advance, the
leftover body is re-parsed as a new request forever. It explains the
WebMvc-only failure (Jersey and WebFlux consume the entity), the body-carrying
POST, and the 404-without-reading-the-body path. Next step: drive that exact
shape — POST with `Content-Length`, handler returns without reading — against a
socket and check whether the leftover bytes are drained.

## Refuted — do not re-run these

Each hypothesis was tested with a standalone probe reproducing the suspected
shape in isolation. All stay flat, so none is the mechanism.

1. **"The throwable stack-trace side table is never pruned."** The natural
   suspect once the trace-capture symbols showed up in the profile: the frames
   are parked in a VM-wide identity-hash-keyed store that only a collection
   prunes, and Throwables are tiny so the heap-occupancy trigger never fires.
   `ThrowNoGcProbe` — 50,000 throws at depth 200 with **no** `System.gc()` —
   holds flat at **317 MB**. The store is swept.
2. **"Classloader churn leaks metadata."** `LoaderChurnProbe` — 240 throwaway
   `URLClassLoader`s over the same 186 real jars — grows 464 → 508 MB, ~0.2 MB
   per loader. Real, small, two orders of magnitude short.
3. **"A JIT recompile loop."** `CRATONVM_DBG_JITC=1` over 30 s: **1,358 compiles
   of 1,280 distinct methods**, most-recompiled method 25 times. Ordinary
   warm-up volume.
4. **"Per-thread VM state is retained at thread exit."** Real but **bounded**:
   `ThreadOnlyProbe` shows ~85 KB per exited-and-joined thread (HotSpot flat at
   48 MB over 2,000 threads, CratonVM 268 → 435 MB) — but it **plateaus**, with
   18,000 threads levelling in a 500-800 MB band and falling back. Worth its own
   look; not 5 GB.

Three JIT kill switches were tried and none clears it (all still spin past a
200 s cap): `CRATONVM_DISABLE_AALOAD_LICM`, `CRATONVM_DISABLE_ARITH_LICM`,
`CRATONVM_JIT_GETFIELD_HELPER`.

Two dead ends on instrumentation, recorded to save the next person the time:
`heaptrack` cannot see this — the binary uses **mimalloc** as its global
allocator and mimalloc goes to `mmap` directly, so libc-malloc interception
records nothing. And a `perf record -e page-faults` profile attributes 21.6% to
mimalloc internals on the request thread with **no resolvable Rust caller**;
frame-pointer unwinding does not reach past them. Sampling says where allocation
happens, which was the wrong question anyway.

## Reproduction

```bash
# fixture: apps/spring-boot, built test classes + per-module cratonvm-test-cp.txt
# /data/sb4.sh and /data/sbrss.sh on the Azure Linux box wrap the exact launch
# run-spring-boot-suite.ps1 uses (three env vars, --stack-dump-on-timeout 0,
# --add-opens=java.base/java.net).

MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz \
  org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests q 400

# the passing control
MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz <same class> q 400 --nojit

# count the loop rather than watching the memory — this is the load-bearing number
CRATONVM_DBG_STTRACE=1 <cratonvm> … 2>&1 | grep -c 'QuartzEndpoint.triggerQuartzJob'
```

## Related

- retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md —
  the page this was found from. Same class, different mechanism: that one is a
  `[moving-young]` fallback spiral under Generational, and this run logs none.
  Its thesis that the JIT is what breaks these classes survives.
- retired/generational-young-relocation-nulls-live-string-references-RETIRED-20260818.md
  — the other finding from the same re-measurement, RETIRED and FIXED
  2026-08-18. It was the proxy-dispatch `Method` cache being rooted without
  being remapped, so it says nothing about this page's spin loop.
