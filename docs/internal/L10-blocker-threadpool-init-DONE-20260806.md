# L10 — Real `ThreadPoolExecutor` field initialisation — **DONE 2026-08-06**

> **Retired** from `docs/feature-designs/jdk-only-wave2/`. `Executors.new*` no
> longer returns anything CratonVM built: in real-JDK mode the registry drops
> every `Executors` pool factory, so the real
> `java.util.concurrent.Executors` bytecode constructs every executor and a
> factory-made pool is real **by construction** rather than by a fallback that
> happened not to be taken. `probes/L10ThreadPoolInitProbe` is byte-identical to
> HotSpot 25 in both modes, and `CRATONVM_DBG_TPE_SHAPE` reports the
> receiver-shape predicate `true` on every call and `false` on none.
>
> **Read *Verification* §2 before quoting that last number.** The predicate
> answered `true` on every call *before* this change too. What the lane changed
> is not the predicate's answer but its **domain**: there is no longer an input
> it can be false for. `false=0` on two workloads was never evidence for the
> universal claim, and mistaking it for one is exactly how the eight dispatch
> sites would get deleted on the wrong grounds.
>
> **L11's item 7 is unblocked.** Its step 1 was this lane; steps 2–4 (reclassify
> `native_es_execute`, delete the ninth site, delete the eight) are unchanged
> and still have to happen together in that order.
>
> **Evidence record:**
> [`threadpoolexecutor-execute-receiver-shape-special-case-copies.md`](../known-issues/jdk-only/threadpoolexecutor-execute-receiver-shape-special-case-copies.md)
> — still **OPEN**, and stays open: it is item 7's record and item 7 is L11's.
> Its step 1 is struck through and dated.

## The brief was wrong about where the defect was, and that is the finding

The lane doc said **Owns: `native-collections/src/lib.rs` (whole file)** and
expected the work to be "establish what real `ThreadPoolExecutor.<init>` needs
that it does not get — `ctl`, and expect `mainLock`, `workers`, `workQueue` to
be in the same family".

**None of that was still true, and `native-collections/src/lib.rs` was not
touched at all.** By the time this lane ran, `Executors.newFixedThreadPool` /
`newCachedThreadPool` / `newSingleThreadExecutor` already drove the real
`ThreadPoolExecutor(int,int,long,TimeUnit,BlockingQueue[,ThreadFactory])V`
constructor through `initialize_real_thread_pool_executor`
(`native-builtins/src/phases_early.rs`), and `newScheduledThreadPool` /
`newSingleThreadScheduledExecutor` — which do *not* — were already dropped in
real-JDK mode by an existing arm of `NativeMethodRegistry::register`. Measured
before changing anything: the census probe's `concurrent` section is
byte-identical to HotSpot in both modes on the pre-fix binary, and so are 60 of
the 62 lines of the new L10 probe.

This is the third lane in this wave to find that ([`the README's own
lesson`](../feature-designs/jdk-only-wave2/README.md) — L3 found the Scanner
writer in a different file than its brief named; L1 and L3 both found their
brief under-reported the defect). Here it went the other way: the brief
*over*-reported it. **Read the code and take the measurement before sizing a
lane from its brief.**

## What was actually wrong

Two things, and only one of them was visible in a transcript.

### 1. `newSingleThreadExecutor()` returned the wrong kind of object

The real JDK does not hand back a `ThreadPoolExecutor` here. It returns
`Executors$AutoShutdownDelegatedExecutorService` wrapping one, specifically so
the pool cannot be reconfigured. CratonVM returned the bare pool:

```
                    HotSpot 25                                          CratonVM (both modes)
single.class=       java.util.concurrent.Executors$AutoShutdown...      java.util.concurrent.ThreadPoolExecutor
single.isTpe=       false                                               true
```

Every `instanceof ThreadPoolExecutor` a caller writes flipped, and
`setCorePoolSize` on a "single-thread" executor silently worked. These two lines
were the **only** divergence in the whole probe, in either mode.

### 2. The construction path kept two fallbacks that fabricate

This is the one that mattered for L11, and it is invisible to a transcript
because on the happy path it never fires.

`initialize_real_thread_pool_executor` allocates with
`alloc_concurrent_synthetic(.., "java/util/concurrent/ThreadPoolExecutor", 2)`
and then `invoke_special`s the real `<init>`. But if the `BlockingQueue` or the
`TimeUnit` constant cannot be built it calls `stpe_legacy_slot_init` — writing
the historical two-slot shape onto a real-layout object — and **returns it as if
construction had succeeded**. That object is precisely the receiver shape the
eight `ThreadPoolExecutor.execute` dispatch sites exist to detect.

So the predicate those sites consult was *conditionally* true, and
"conditionally true" is exactly what blocks deleting them. A lane that had only
fixed `newSingleThreadExecutor` would have produced a green transcript and left
L11 blocked for a reason nothing measured.

## The fix

One arm, in `NativeMethodRegistry::register` (`native-api/src/registry.rs`),
extending the `Executors` drop that already covered the scheduled pair:

```rust
if self.drop_real_layout_synthetic
    && class_name == "java/util/concurrent/Executors"
    && matches!(method_name,
        "newScheduledThreadPool" | "newSingleThreadScheduledExecutor"
            | "newFixedThreadPool" | "newCachedThreadPool" | "newSingleThreadExecutor")
{ return; }
```

**Registration, not dispatch, and that is the point.** Item 3's outcome record
(`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`) is
that a policy expressed at dispatch has to be restated once per dispatch path —
and the `String` lists turned out to decide nothing because `resolve_step1_native`
dispatches on the triple before any of them runs. A policy expressed at
registration is invisible to every dispatch path at once. With no native, real
`Executors` bytecode runs `new ThreadPoolExecutor(...)` — or, for the
single-thread case, builds the delegating wrapper — and there is no code path
left that can mint a half-constructed executor.

**Why this was safe rather than merely desirable:**
`Executors.newSingleThreadExecutor(ThreadFactory)` was never registered, so that
real path — including the `CleanerFactory.cleaner()` / `Cleaner.register`
machinery `AutoShutdownDelegatedExecutorService`'s constructor uses — has always
run under CratonVM. The drop puts the no-arg overload on the same path its own
one-arg sibling was already on.

### Nothing is deleted, and which axis this is on

The wave's standing rule (`docs/feature-designs/jdk-only-wave2/README.md`, *The
end state is two modes, and it is a rename*): the three modes collapse to two by
renaming — today's `--jdk-only` becomes `--real-jdk`, today's `--real-jdk`
becomes `--synthetic-jdk` — and **no synthetic method used by either surviving
mode may be removed.**

This change removes no code. `native_new_fixed_pool`,
`native_new_cached_pool`, `native_new_single_thread`,
`initialize_real_thread_pool_executor` and the four `phases_early` closures all
still exist and still call `register(...)`; the registry declines the
registration. The `--features synthetic-jdk` build never sets
`drop_real_layout_synthetic` and keeps every one of them, which is the build
that has no real `ThreadPoolExecutor` bytecode to fall back to.

**And the axis matters, because this is easy to misread as a mode decision.**
`set_drop_real_layout_synthetic` is a *correctness* gate, not a policy gate: it
answers "the real class is loaded, and this synthetic surface writes a layout
that corrupts it". It already drops `StringJoiner`, `Cleaner`,
`ReferenceQueue`, `Permissions`, `LinkedBlockingDeque`, the legacy regex
natives and `ScheduledThreadPoolExecutor` — all of that predates `--jdk-only`.
The `Executors` factories are the eighth entry on a list that is about
corruption, not about strictness. They were not serving `Compatible` mode; one
of them was measurably breaking it.

The alternative — keying the drop on `CompatibilityMode::JdkOnly` so
`Compatible` keeps the factories — was considered and **does not work**. The
eight dispatch sites are not mode-scoped; they run in `Compatible` too. If
`Compatible` can still mint a fabricated executor, the sites cannot be deleted,
and the lane's entire purpose (unblocking item 7) is unmet.

**What the drop does not reach**, asserted by a negative control in the gate:
`Executors.callable`, `defaultThreadFactory` and the `unconfigurable*` wrappers
fabricate no executor and keep whatever registration they have. And
`drop_real_layout_synthetic` is never set in the `--features synthetic-jdk`
build, whose two-slot executor model is what those factories are for.

## What shipped

| File | What |
|---|---|
| `native-api/src/registry.rs` | the drop arm, and `real_jdk_mode_registers_no_executors_pool_factory` — the hermetic gate over it. |
| `vm/src/runtime/env_cache.rs` | `CRATONVM_DBG_TPE_SHAPE`. |
| `vm/src/runtime/interpreter/native_override.rs` | the instrument inside `threadpool_executor_has_real_workers`; the ninth site's comment corrected (it claimed the factories "never run the real `<init>`", which had stopped being true); the census constant's step 1 struck through and dated for L11. |
| `probes/L10ThreadPoolInitProbe.java` | 62 deterministic lines, the oracle for this lane. |
| `probes/L10ShapeInstrumentControlProbe.java` | the negative control: a constructor-less executor, so the instrument's `false` branch is shown to fire. Not a correctness probe; diverges from HotSpot by design. |
| `apps/executor_probe/ExecProbe.java` | **restored** — `vm/tests/wave1_c_executor.rs`'s missing fixture. See below. |

## The instrument, and why it prints the successes too

`CRATONVM_DBG_TPE_SHAPE=1` prints one line per call of
`threadpool_executor_has_real_workers`:

```
[tpe-shape] real=true  reason=populated          class=java/util/concurrent/ThreadPoolExecutor
[tpe-shape] real=false reason=no-workers-field   class=<some other receiver>
[tpe-shape] real=false reason=null-workers       class=java/util/concurrent/ThreadPoolExecutor
```

"The predicate is universally true" is **two** claims — no `real=false`, and at
least one `real=true`. An instrument that printed only failures would make "the
probe never ran" (a workload that built no executor, or a refactor that stopped
reaching the sites) read exactly like "the probe ran and always said yes". That
is the shape of a guard that is green because it is dead, and this directory has
produced several.

`reason=` distinguishes the two ways `false` arises, because they mean different
things: **no `workers` field in the hierarchy at all** is a receiver that is not
a `ThreadPoolExecutor` (the dispatch sites still key on the resolved declaring
class, so this is rare), while **a null `workers` on a class that declares one**
is the fabricated-executor case this lane removed.

### The one receiver that can still answer `false`, and why it does not matter

A `ThreadPoolExecutor` created without running any constructor — an Objenesis /
Mockito mock — has a null `workers` and reports `real=false`. That is correct
and load-bearing rather than a residual: for a redefined class,
`should_force_registered_native_over_bytecode` already declines *before* the
receiver-shape check runs, so the mock's woven advice wins either way. The
lane's claim is the doc's claim — the predicate is true for every executor **the
factories produce** — and after this change the factories produce nothing
CratonVM built.

## Verification

Protocol per the wave README: three arms, exit status checked, both modes plus a
HotSpot control, and every guard shown to fail.

Two release binaries built from this worktree, differing **only** in the five
names in that `matches!` arm: `cratonvm-l10-abase` (pre-L10 policy) and
`cratonvm-l10-fix`. Both carry the instrument, so the A arm is a real
measurement rather than a binary in which the instrument does not exist — see
*The one thing that had to be built twice* below.

### 1. Behavioural transcripts — three arms, exit status checked

Every arm exited 0. `--jdk-only` and `--real-jdk` diffed against a real HotSpot
25 run of the same class files; the VM's own banner and `tracing` lines are
excluded exactly as `scripts/jdk-only-strict-probes.sh` excludes them.

| Probe | pre-fix `--real-jdk` | pre-fix `--jdk-only` | post-fix `--real-jdk` | post-fix `--jdk-only` |
|---|---|---|---|---|
| `L10ThreadPoolInitProbe` (62 lines) | **2 diverge** | **2 diverge** | **0** | **0** |
| `JdkOnlyCensusLoadProbe` | 0 | 0 | 0 | 0 |
| `JdkOnlyBreadthProbe` | 1 | 3 | 1 | 3 |
| `JdkOnlyPlatformProbe` | 2 | 2 | 2 | 1–2 |

The two that moved are `single.class` and `single.isTpe`. Everything the
`JdkOnlyBreadthProbe` / `JdkOnlyPlatformProbe` rows carry is the strict corpus's
four pre-existing filed defects (`textformat`'s decimal separators,
`serialization`'s `NoClassDefFoundError: cratonvm/internal/SystemLogger`,
`vthreads`' `terminated=false`, `agent`'s `attach=throw-UnsatisfiedLinkError`),
identical in both binaries. **No new divergent section key in any arm** — that
is what the strict-corpus ratchet scores, and it does not move.

`JdkOnlyPlatformProbe`'s strict arm printed one fewer divergent line after the
fix, because `vthreads … terminated=` happened to come out right on that run.
**That is not a claim.** It is the intermittent
`ConcurrentHashMap.newKeySet` race the record for it says it is, it is a
`newVirtualThreadPerTaskExecutor` and not a `ThreadPoolExecutor`, and nothing
here touched it.

The census probe's `concurrent` section — the lane doc's headline signal —
matched HotSpot **before** the change as well as after. That is worth stating
plainly rather than quietly banking: this lane did not turn that section green,
it was already green, and a verification that had stopped at that bullet would
have concluded the lane was a no-op.

### 2. The receiver-shape predicate

`CRATONVM_DBG_TPE_SHAPE=1`, both workloads, both modes:

| | pre-fix | post-fix |
|---|---|---|
| `L10ThreadPoolInitProbe` `--real-jdk` | `true=62 false=0` | `true=62 false=0` |
| `L10ThreadPoolInitProbe` `--jdk-only` | `true=62 false=0` | `true=62 false=0` |
| `JdkOnlyCensusLoadProbe` `--real-jdk` | `true=38 false=0` | `true=38 false=0` |
| `JdkOnlyCensusLoadProbe` `--jdk-only` | `true=38 false=0` | `true=38 false=0` |

Both halves of the claim hold post-fix: **no `false`, and `true` is not zero.**

**And the A arm is identical, which is the most important number in this
document.** A first draft of it claimed the fix "made the predicate universally
true". It did not: the predicate already answered `true` on every call, on both
workloads, in both modes, *before* the change — because the happy path of
`initialize_real_thread_pool_executor` does build a real executor, and neither
workload made a `BlockingQueue` or a `TimeUnit` lookup fail.

So the honest statement of what this lane did to the predicate is:

> The predicate's **answer** did not change. Its **domain** did. Before, it was
> true for the inputs these two workloads happen to produce; after, there is no
> input it can be false for, because real-JDK mode has no code path that
> constructs an executor.

That distinction is the whole justification for the lane, and it is worth being
blunt about how easily it is lost: a lane that had run only these probes,
observed `false=0`, and declared step 3 satisfied would have reached the right
conclusion by the wrong route — concluding a property holds universally because
the sampled inputs satisfied it. The eight dispatch sites cannot be deleted on
that evidence. They can be deleted on "the fallback that produces a false
receiver is not reachable", which is a claim about the code, and which is what
the drop delivers.

### The `false` branch, shown to fire

`false=0` means nothing until the failure path is known to work.
`probes/L10ShapeInstrumentControlProbe` (a **negative control**, not a
correctness probe, and deliberately in no strict-corpus list) builds the one
receiver that must answer `false` — a `ThreadPoolExecutor` from
`Unsafe.allocateInstance`, no constructor run, `workers` null, which is exactly
how Objenesis and therefore every mocking framework builds one:

```
control.allocated=true class=java.util.concurrent.ThreadPoolExecutor
[tpe-shape] real=false reason=null-workers class=java/util/concurrent/ThreadPoolExecutor
[tpe-shape] real=true  reason=populated    class=java/util/concurrent/ThreadPoolExecutor
[tpe-shape] real=false reason=null-workers class=java/util/concurrent/ThreadPoolExecutor
control.execute=task-ran
```

Both branches fire, and the `real=true` line in between is the VM's own internal
async pool servicing the forced native's fallback — so the control also shows
the two receiver classes being told apart at one call site. That probe diverges
from HotSpot by design (a real JVM throws on a constructor-less executor) and
must never be diffed against it.

### 3. The registry, and what it says about who built the executors

`--explain-jdk-only --dump-native-registry`, `--real-jdk`, the L10 probe as the
workload:

| | pre-fix | post-fix |
|---|---|---|
| `java/util/concurrent/Executors` registrations | **8** | **0** |
| …of which invoked, running this probe | 12 invocations across 4 rows | — |
| registry total | 11,876 | 11,868 |
| `intrinsic` / `bridge` / `synthetic-stub` | 687 / 10,434 / **755** | 683 / 10,430 / **755** |

The pre-fix invocation counts are the direct evidence that this was not
theoretical: `newFixedThreadPool` ×8, `newCachedThreadPool` ×2,
`newCachedThreadPool(ThreadFactory)` ×1, `newSingleThreadExecutor` ×1 —
CratonVM built twelve of the probe's executors itself. Post-fix it builds none,
because there is no registration left to dispatch. (The eight rows were four
duplicate pairs: `native-builtins/src/util_concurrent_ext.rs` registers the
triples first and `phases_early.rs` overwrites them, so only the `intrinsic`
half of each pair ever ran.)

**`synthetic-stub` is unmoved at 755**, and the `stub_ratchet` baseline
(553 in its own boot-path census) with it — the dropped rows were `Intrinsic`
and `Bridge`. Nothing needed re-freezing. `bridge-ratchet.sh` is a `<=` ratchet,
so four fewer `Bridge` rows passes and asks for a re-freeze; it is keyed
`25/linux` and refuses to adjudicate on this Windows host, so the re-freeze is
CI's to take.

`ThreadPoolExecutor.execute`'s own invocation count is **2 in both arms**, and
that is the expected answer rather than a leftover: the eight dispatch sites
decline to *force* the native for a real receiver, but `resolve_step1_native`
still dispatches a registered native on the triple alone, and `native_es_execute`
then re-checks the receiver itself and forwards to real bytecode via
`invoke_special_bytecode_only`. That callee-side backstop is deliberately *not*
part of this lane (the evidence record's *Not in this list*), and it is what
L11's step 2 removes.

### 4. Gates

| Gate | Result |
|---|---|
| `cargo test -p cratonvm-native-api --lib` | 285 passed — includes the new `real_jdk_mode_registers_no_executors_pool_factory` |
| `cargo test -p cratonvm-vm --lib threadpool_receiver_shape` | 2 passed — the eight-site census gate and the no-hand-inlined-probe gate, both still green |
| `cargo test --release -p cratonvm-native-collections --lib` | **105** passed (the lane doc says 94; the suite has grown since it was written) |
| `cargo test -p cratonvm-native-builtins --lib` | 3,287 passed |
| `cargo test -p cratonvm-native-builtins --test stub_ratchet` | 5 passed; baseline 553, slack 0, unmoved |
| `cargo test -p cratonvm-vm --test wave1_c_executor` | 5 passed — **and actually ran for the first time**; see the residual below |

**The new gate was shown to fail, in both directions.** Reverting the arm to its
pre-L10 name set fails the real-JDK half naming `newFixedThreadPool`; making the
arm unconditional fails the synthetic-JDK half, which is the assertion that stops
somebody "simplifying" the drop into one that would leave the `--features
synthetic-jdk` build with no executor factory at all.

### 5. The "watch for" — the socket/executor hang

The lane doc warns that executors are where the socket-registry lock cycle used
to surface, about one run in five, and that a single clean run proves nothing.
`JdkOnlyCensusLoadProbe` (whose `net` section is the one that hung) run 6× per
mode per binary: **24 runs, 24 exit-0**, 12 on each binary. No attribution to
this lane is needed because nothing was observed to attribute.

### 6. The one thing that had to be built twice

The first A-arm attempt reported `true=0 false=0` on the pre-fix binary — which
reads exactly like "the predicate was never consulted", and would have made a
tidy story about the sites being dead. It was wrong: that binary was linked
*before* `CRATONVM_DBG_TPE_SHAPE` existed, so the instrument was not in it. A
third binary was built with the instrument and the pre-L10 policy to get the
real A arm.

This is the instrument's own design rule turned on its author, and it is the
reason the flag prints successes as well as failures: **an absent instrument and
a satisfied predicate produce the same silence.** Check that the instrument is
in the binary before reading a zero from it.

## Residuals

Filed here rather than fixed, because each is a different lane's or a different
change's:

* **`initialize_real_thread_pool_executor`'s two `stpe_legacy_slot_init`
  fallbacks are now unreachable in real-JDK mode, and still live under
  `--features synthetic-jdk`** — where writing the two-slot shape is correct,
  because there is no real `ThreadPoolExecutor` class behind it. They are not
  deleted. Whoever removes `native_es_execute` in L11 should re-check whether
  the whole `native_new_*_pool` / `initialize_real_thread_pool_executor` family
  still earns its place in the synthetic build, which is a question about that
  build and not about `--jdk-only`.
* **`native-collections/src/lib.rs`'s `tp_is_real` and the `TP_FIELD_*` reads
  behind it** are now, in real-JDK mode, a branch that is never taken —
  `getPoolSize`, `getCorePoolSize`, `getMaximumPoolSize`, `isShutdown`,
  `shutdownNow` and friends all forward. Same disposition, same reason: it is
  the callee-side backstop the evidence record explicitly says not to delete in
  the same change as the dispatch-side probes.
* **The strict corpus's four filed divergences are untouched**, which is the
  expected result — none is executor-shaped. `vthreads`' `terminated=false` is
  the closest, and it is a `newVirtualThreadPerTaskExecutor` and a
  `ConcurrentHashMap.newKeySet` race.
* ~~`vm/tests/wave1_c_executor.rs` cannot run~~ — **fixed here.** It drives
  `apps/executor_probe/ExecProbe.java`, and that file **has never existed in the
  repository**: `git log -- apps/executor_probe` is empty, and `apps/` is
  `.gitignore`d with individual fixtures force-added (`apps/cglib_probe/`,
  `apps/h2database-suite-runner/probes/`), so it was simply never added. Five
  assertions over `newFixedThreadPool(4).submit(...).get()`, a 4×4000
  throughput run, a custom `ThreadFactory`, and `UncaughtExceptionHandler`
  dispatch have been returning green without executing since wave 1.
  Demonstrated rather than asserted: with the fixture removed the five tests
  "pass" in **0.00s**; with it present they take 2.31s and actually run.
  Restored and `git add -f`'d. It belongs to this lane because it is the
  executor regression test for exactly this surface — the one thing that should
  have caught `newSingleThreadExecutor` returning the wrong class, and could
  not.
* **`bridge-ratchet.sh` wants a re-freeze** (four fewer `Bridge` rows). It
  passes as-is — the ratchet is `<=` — and the baseline is keyed `25/linux`, so
  it can only be re-taken on the Linux CI leg.

---

## The lane brief as written

Kept verbatim so the difference between the plan and what was executed is
readable. Three of its statements did not survive contact with the code, and
they are called out above: the owned file, the `ctl`/`mainLock`/`workers`
"family" (all already initialised), and the assumption that step 2 was
unstarted.

> # L10 — Blocker: real `ThreadPoolExecutor` field initialisation
>
> **Owns:** `native-collections/src/lib.rs` (whole file)
> **Gated on:** nothing — but **it gates L11**.
> **Conflicts:** L2 owns the same file. **L2 lands first**; it is smaller.
> **Effort:** L
> **Note:** not a jdk-only change. Same caveat as L9 — staff it as a VM defect.
> **Evidence:** [`threadpoolexecutor-execute-receiver-shape-special-case-copies.md`](../../known-issues/jdk-only/threadpoolexecutor-execute-receiver-shape-special-case-copies.md)
>
> ## Goal
>
> `Executors.new*ThreadPool()` must return objects built by the **real**
> `ThreadPoolExecutor.<init>`, with real field initialisation. Until then,
> reclassifying `native_es_execute` drops it under `--jdk-only` and **strict mode
> loses thread pools**.
>
> That is what makes it a blocker rather than cleanup: the eight
> receiver-shape dispatch sites in L11 exist precisely to detect "this executor
> was not built by real bytecode" and route around it.
>
> ## Current state
>
> The marker undercount that made item 7 tier-1 is gone: a census constant names
> all eight sites plus the ninth unconditional `force_native` arm they exist to
> override, the receiver-shape probe has **one** implementation instead of three,
> and a gate fails on a partial sweep. **No site is deleted** — that needs this
> lane first.
>
> ## Steps
>
> 1. Establish what real `ThreadPoolExecutor.<init>` needs that it does not get.
>    The `ctl` `AtomicInteger` is the known one — a synthetic executor NPEs
>    immediately on it (see `fixed-suite-bugs/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`).
>    Expect `mainLock`, `workers`, `workQueue` to be in the same family.
> 2. Make `Executors.new*ThreadPool()` run the real constructor chain rather than
>    allocating a synthetic shape.
> 3. Confirm the receiver-shape probe (`threadpool_executor_has_real_workers`)
>    returns `true` for every executor the factories produce. That predicate is
>    the one all eight sites consult; when it is universally true, the sites are
>    dead and L11 can delete them.
>
> ## Verification
>
> * `JdkOnlyCensusLoadProbe`'s `concurrent` section under `--jdk-only`: fixed and
>   cached pools, 24 submitted tasks, a latch, and bounded `Future.get`. It must
>   match HotSpot exactly. This section was **dead** before the thread-start fix,
>   so it is a genuine new signal.
> * Both probes vs HotSpot, both modes, exit status checked.
> * `cargo test --release -p cratonvm-native-collections --lib` (94 tests).
> * The eight-site census gate must still pass — this lane does not delete sites,
>   it makes deleting them possible.
>
> ## Watch for
>
> Executors are where the **socket/executor hang** shows up
> (the socket-registry lock cycle — the retired `bounded-socket-operations-hang-about-one-run-in-five` record, fixed 2026-08-05).
> That hang is pre-existing and mode-independent; do not attribute it to this
> lane's changes without an A/B against the pre-fix binary and a HotSpot control.
>
> ## Done when
>
> `Executors.new*ThreadPool()` returns real-constructed executors,
> `threadpool_executor_has_real_workers` is true for all of them, and L11 is
> unblocked.
