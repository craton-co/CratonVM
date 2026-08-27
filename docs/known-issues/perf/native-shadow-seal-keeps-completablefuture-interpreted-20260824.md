# The native-shadow seal keeps `CompletableFuture` composition INTERPRETED — and that is the 442-872x

## Status
**OPEN (2026-08-24, blast radius measured 2026-08-26). Root cause identified by
a native profile, not fixed.** The hibernate-reactive suite seals a median
**1 010 methods per class** this way — 3.6 for every method the JIT ever
tracks. But half of all seals are `<clinit>` (worthless to fix, including the
single biggest cause), and the 483 that hit real methods come from **217
distinct natives in a long tail** — 121 of them to reach 80%. De-registering
cannot fix this; see "Consequence for the fix".

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

## What to try next

1. **Count how many of the 217 targets already have a compiled-code fast
   path.** Those seals could be lifted with no new codegen — the bind exists,
   only the seal's opinion of it is stale.
2. **Then make the seal ask.** `native_skip` is a scan for "does this method
   call a shadowed native"; what it should ask is "does it call one the JIT
   cannot emit". The `VarHandle` read/write/CAS binds are the existence proof
   that the two questions differ.
3. Do NOT pursue de-registration further. §6's seven pure delegations were the
   whole population of that shape; the remaining head is `Map.get`,
   `Object.hashCode` and `Function.apply`, which are not delegations.
