# `CompletableFuture` composition runs INTERPRETED — `UniCompose.tryFire` is refused by the compiler with `reason=unrecorded`

## Status
**OPEN. The title's causal claim is RETRACTED (2026-08-26).** The seal is
real and large — the hibernate-reactive suite seals a median **1 010 methods
per class**, 3.6 for every method the JIT ever tracks — but it is **not** why
`CompletableFuture` composition is slow. Turning the seal off with
`CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0` changes the compile count by one
method and the runtime by nothing.

The real constraint is that the two hottest composition methods are ASKED and
REFUSED: `CompletableFuture$UniCompose.tryFire` (97 716 invocations) and
`UniRelay.tryFire` (58 612), both `compile-failed` with **`reason=unrecorded`**.
See the CORRECTION at the bottom, which supersedes the causal argument in the
sections above; those are kept because the blast-radius measurement and the
`<clinit>` breakdown stand on their own.

## Severity
**HIGH.** It is the reason `CompletableFuture` composition measures 442-872x
HotSpot, and it reaches every reactive workload in the tree: the two frames it
seals in Vert.x are the ones in every hibernate-reactive stack trace on the
`MultithreadedInsertionWithLazyConnectionTest` page.

## The measurement that found it

A Java-frame profile put 34% of composition in `CompletableFuture.tryPushStack`
and sent three days of work into `VarHandle` primitives. That reading was
**wrong**, and only a NATIVE profile could say so — a Java-frame sampler
attributes the whole of a native call to the Java frame that made it, so "34% in
tryPushStack" was never evidence about which part of that call was expensive.

`perf record -F 999` on `HibfixComposeProbe2` (Linux, `/data/vhprof`):

| DSO | share |
|---|---:|
| **`cratonvm`** (VM runtime + interpreter) | **92.35%** |
| `libc` | 4.95% |
| **`[JIT]` compiled code** | **0.32%** |

Composition runs almost entirely INTERPRETED. Confirmed independently on the
current Windows binary, where the JIT makes no difference at all:

```
jit:    @@COMPOSE2 threads=2 chains=40000 ms=6142
nojit:  @@COMPOSE2 threads=2 chains=40000 ms=5988
```

`--nojit` is marginally FASTER. There is nothing for the JIT to do because the
code was never compiled.

## Why it is not compiled

`CRATONVM_DBG=jit-method-stats` on the same probe:

```
JIT skip-seal census: 63 method(s) sealed before any compile
  | calls-native-shadowed-method=31 clinit=31 forkjointask-subclass=1
```

A method that calls a JDK method CratonVM has registered a native over is
sealed — permanently, before any compile is attempted
(`interpreter.rs`, `seal_site = "calls-native-shadowed-method"`).
`CompletableFuture.complete` IS such a native
(`native-collections/src/lib.rs`, `r.register(cf, "complete", …)`), and it is
NOT in the `skip_delegating_cf` set that §6 of the hibernate-reactive page
de-registered under a real JDK.

`CRATONVM_DBG_JITC=1` names the casualties. Ignoring `<clinit>` (one-shot,
harmless), from the Vert.x bridge probe:

```
io/vertx/core/Future.lambda$toCompletionStage$5(CompletableFuture, AsyncResult)V
io/vertx/core/impl/future/FutureBase.emitResult(Object, Throwable, Completable)V
java/util/concurrent/CompletableFuture.uniComposeStage(Executor, Function)
java/util/concurrent/ScheduledThreadPoolExecutor.schedule / delayedExecute / triggerTime
java/util/concurrent/AbstractExecutorService.submit(Runnable)
```

The first two are the exact frames in every hibernate-reactive stack trace on
the `MultithreadedInsertionWithLazyConnectionTest` page. The third is the CORE
of `thenCompose`. None of them can ever be compiled.

## Why this is not the §6 fix repeated

§6 de-registered `thenApply`/`thenAccept`/`thenRun`/`thenCompose`/
`exceptionally`/`handle`/`whenComplete` because over a real JDK each was a
**pure delegation** back to the method it shadowed — deleting the native
changed nothing but the funnel.

`native_cf_complete` is not that. It writes `CF_FIELD_RESULT` directly,
substitutes the JDK's `NIL` sentinel for a null value, and distinguishes a
SYNTHETIC CompletableFuture (slot 1 is an Int `done` marker) from a real-JDK one
(slot 1 is the lock-free `stack` reference) by the type of slot 1. Removing it
requires the real JDK's `complete` — and with it `postComplete`, the `stack`
Treiber chain and the `Signaller` path — to work end to end under CratonVM.
That is a project, not a guard.

## What this explains

* **The 442-872x itself.** Every primitive optimised underneath it — the
  `VarHandle` CAS fast path (4 396 000 served, 0.5% on composition), the `set`
  bind, the global mutex — was rearranging furniture inside an interpreter.
* **Why `--nojit` was CLEAN on the hibernate test** (8/8, against 6/8 with the
  JIT) while being no faster: if the reactive path is interpreted either way,
  the JIT's only contribution to that workload is timing VARIANCE, which is
  exactly the shape the cut-loop investigation kept running into.

## Blast radius, measured on the hibernate-reactive suite (2026-08-26)

27 classes of the non-PASS union, run sequentially against an external MySQL
(`shards=1` deliberately: the classes share one database under
`HBM2DDL_AUTO=create`, and a class that dies early on a schema race seals fewer
methods, which would silently UNDERCOUNT the thing being measured).

`CRATONVM_DBG=jit-method-stats` gives a census per fork:

| | min | median | max | total |
|---|---:|---:|---:|---:|
| `calls-native-shadowed-method` seals | 440 | **1 010** | 1 194 | **25 594** |
| `clinit` seals | 832 | 1 757 | 1 835 | 44 223 |
| distinct methods the JIT ever TRACKED | 37 | **227** | 884 | 7 035 |
| compiles (c1+c2) | 57 | 397 | 1 571 | 12 362 |

**3.6 methods sealed for every method the JIT ever tracks**, and the band is
uniform across classes (the three at 440-717 are the short ones). This is a
property of loading Hibernate + Vert.x + netty, not of any one test — which
makes §6's note that a Spring context seals 856 methods the same phenomenon
seen from another workload.

(The `compiles` column double-counts a method compiled at both tiers, so the
2.1:1 it yields understates. The TRACKED comparison is the honest one. And the
`clinit` row is reported separately rather than folded in: quoting the ~2 900
combined total would overstate the problem threefold.)

### Half the seals are worthless to fix, including the single biggest cause

`CRATONVM_DBG_JITC=1` on one class pairs each seal with the shadowed native
that caused it — 1018 seals, and the split is the finding:

```
<clinit> = 535     real methods = 483
```

A sealed `<clinit>` costs almost nothing: class initializers run once.

And the largest single cause is entirely in that half:

| shadowed native | seals | of which real methods |
|---|---:|---:|
| `java/lang/Class.desiredAssertionStatus()Z` | **353** | **0** |

That is javac's `$assertionsDisabled` idiom, emitted into the `<clinit>` of
every class compiled with assertions. It is 35% of all seals and **worth
nothing** to unseal. A blast-radius number that had stopped at "353, the top
cause" would have pointed the next fix at the one target that cannot pay.

### What actually seals real methods is a LONG TAIL

The 483 real-method seals come from **217 distinct shadowed targets**:

| coverage | targets needed |
|---|---:|
| 17.4% | top 5 |
| 28.8% | top 10 |
| 43.1% | top 20 |
| **80.1%** | **top 121** |

The head is ubiquitous and mostly INTERFACE dispatch — `Function.apply` (24),
`Map.get` (20), `Supplier.get` (14), `Object.getClass` (14), `Object.hashCode`
(12), `List.size` / `List.get` (12 each), `StringBuilder.append` (11),
`Consumer.accept` (11), `Iterator.hasNext` (9). **38% of real seals arrive
through a `java.util` interface or functional-interface call.** A native
registered on an interface method seals every caller that dispatches through
it, which is why lambdas seal their callers everywhere.

### Consequence for the fix

**De-registering natives cannot fix this.** It worked for §6's seven
`CompletionStage` methods because those were seven pure delegations; here it
would take 121 of them to reach 80%, on methods like `Map.get` and
`Object.hashCode` that are not delegations at all.

The fix has to be structural, and step 2 of this page's original plan is the
one that survives: **ask whether "shadowed" must imply "uncompilable"**. It
already does not — the `VarHandle` READ modes are served from compiled code
today via `VARHANDLE_READ_DIRECT_FNS`, and `VarHandle.set` and the in-funnel
CAS joined them on 2026-08-24. A native the JIT can emit a direct call to need
not seal its callers, and the seal predates those binds.

The cheap first probe: count how many of the 217 targets already HAVE a
compiled-code fast path. Those are seals that could be lifted today with no new
codegen at all.

## CORRECTION (2026-08-26): the seal is real but it is NOT why composition is slow

This page's original claim — that composition is interpreted BECAUSE of the
native-shadow seal — is **wrong**, and the lever the seal's own comment asks
for is what disproves it.

`CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0` prices the ceiling. Interleaved on
the compose probe:

| seal | ms | ms |
|---|---:|---:|
| on | 12 482 | 11 574 |
| off | 13 666 | 10 305 |

Means 12.0 s against 12.0 s, with the off arm alone spanning 10.3-13.7 s.
**Worth ~0**, which is exactly the outcome the seal's comment says should stop
the per-SITE rewrite from being attempted.

And it is not that the seal removal failed to take effect — it took effect and
changed nothing that matters:

| seal | methods tracked | compiles |
|---|---:|---:|
| on | 10 | c1=6 c2=3 osr=1 |
| off | 9 | c1=5 c2=3 osr=1 |

Removing the seal causes **no more methods to compile**. It was never the
binding constraint on this workload.

### What IS the constraint: the two hottest CF methods are REFUSED by the compiler

`CRATONVM_DBG=jit-method-stats` names them:

```
99956  ineligible-by-policy  HibfixComposeProbe2.chain(...)
       reason=singlepass-codegen/dup_x2-unprovable-form(pc=15,op=0x5b)
97716  compile-failed  java/util/concurrent/CompletableFuture$UniCompose.tryFire(I)
       tier_fail_count=3  reason=unrecorded
58612  compile-failed  java/util/concurrent/CompletableFuture$UniRelay.tryFire(I)
       tier_fail_count=3  reason=unrecorded
```

`UniCompose.tryFire` and `UniRelay.tryFire` ARE composition — they run every
dependent stage. They are invoked ~98 000 and ~59 000 times, the compiler was
ASKED and refused three times each, and **the reason is not recorded**.

That is the whole story of the 872x, and it is a different defect from the seal:
the seal excludes methods before asking; these two were asked and refused.

### The cost structure underneath, for scale

721 103 native invocations for 40 000 chains — **18 native crossings per
chain**:

| native | calls | per chain |
|---|---:|---:|
| `java/lang/Object.<init>` | 360 805 | **9** |
| `java/lang/invoke/VarHandle.compareAndSet` | 158 180 | 4 |
| `jdk/internal/misc/Unsafe.compareAndSetInt` | 100 000 | 2.5 |
| `java/util/concurrent/CompletableFuture.complete` | 100 000 | 2.5 |

At ~300 ns a crossing that is ~5 ns of 54 us per chain — a few percent, not the
gap. The gap is the interpreter running `tryFire` because the compiler refused
it.

### What to do

1. **Record the refusal reason.** `reason=unrecorded` on a method invoked
   97 716 times is the diagnostic gap that matters; every hypothesis below it is
   guesswork until the refusing site names itself.
2. Then fix whatever it names, for `UniCompose.tryFire` first.
3. `singlepass-codegen/dup_x2-unprovable-form` is a real codegen gap too,
   though the method carrying it here is the probe's own.
4. Do **not** pursue the seal's per-SITE rewrite on this evidence. The blast
   radius above is real (1 010 methods per class) but the ceiling is ~0 here,
   and the seal's own comment sets that as the bar.
