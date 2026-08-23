# A compiled caller calling an INTERPRETED callee costs 1900 ns — 5x more than never compiling the caller at all

**Status: PARTLY FIXED 2026-08-22. `invokestatic` — 89% of the JIT's dispatch
calls — now enters an uncompiled callee through the call site's own cached
interpreter frame template: 1310 -> 259 ns/op, 5.06x, six interleaved pairs,
and now BELOW the both-interpreted cost, so compiling the caller is no longer a
pessimisation. `invokevirtual` / `invokeinterface` / `invokespecial` are still
on the by-name path and this page stays open for them.**

This is not a lambda bug, not a reactive bug and not a `java.time` bug: it is
the JIT's interpreter-fallback path, and it applies to EVERY compiled method
that calls a callee the JIT did not compile — which, on any real application, is
most callees.

It is the reason the JIT is worth nothing on
`known-issues/perf/webclient-integration-tests-reactive-exchange-gap-20260822.md`
(JIT on 24.6 ms/op, `--nojit` 25.8 ms/op — a wash), and the reason reactive code
is hit hardest is a corollary recorded in §3.

## The measurement

`probes/XferProbe.java` — a hot loop calling a one-line `static int callee(int
x) { return x + 1; }`. Three arms, same binary, same probe:

| configuration | ns/op |
|---|---:|
| compiled caller -> compiled callee | **32.5** |
| both interpreted (`--nojit`) | **385** |
| compiled caller -> **interpreted** callee (`CRATONVM_JIT_DENY=XferProbe.callee`) | **1902** |

Compiling the caller and not the callee is **5x slower than compiling
neither**, and 58x slower than compiling both. The middle row is the control
that matters: the JIT is not merely failing to help here, it is actively
losing to the interpreter it replaced.

`CRATONVM_JIT_DENY` is not the cause — `probes/IndyScopeProbe.java` reproduces
the same shape with no flag at all (§3), where the callee is uncompilable on
its own merits.

## Why

`perf` on the denied arm, steady state. A compiled caller's call to an
uncompiled callee falls out of `jit_invoke_dispatch`'s fast arms into
`crate::vm::invoke_or_native`, i.e. the **fully name-keyed generic dispatch**:

| symbol | self% |
|---|---:|
| `vm_exec::invoke_on_class_shared_inner` | 14.72% |
| `interpreter::execute` | 8.04% |
| `RawEntryBuilder<(ClassLoaderId, Arc<str>)>::search` (class lookup BY NAME) | 3.41% |
| `__memcmp_evex_movbe` | 3.04% |
| `jit::helpers::jit_invoke_dispatch` | 3.06% |
| `vm_exec::invoke_or_native` | 2.43% |
| `native_override::should_force_registered_native_over_bytecode` | 1.93% |
| `Arc<[u8]>::drop_slow` + `frame::padded_bytecode_for_method` + `CodeAttribute::clone` | 4.50% |
| mimalloc (`_mi_page_malloc_zero`, `mi_free`, `mi_malloc_aligned`, VecPool) | ~11% |

Every one of those is *re-derivation of a constant*. The call site is
monomorphic and the callee never changes, yet each call re-resolves the class
by name, re-runs the native-override arbitration, re-clones the `CodeAttribute`
and re-pads the bytecode (`padded_bytecode_for_method` memoizes, but its hit
path still takes a global `Mutex` and verifies with a **full body memcmp**).

The interpreter does none of this: its call sites hold a
`CachedInvokeTarget::Bytecode` with a prebuilt `Arc<CachedBytecodeMethod>`, so
the same call costs 385 ns. **The JIT's dispatch helper has a cache for a
COMPILED callee (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`) and no cache at
all for an interpreted one.**

## §3 — why this lands on reactive code hardest: `invokedynamic`

A method containing an unbridged `invokedynamic` cannot run compiled, so it
becomes exactly the interpreted callee above. Two separate mechanisms, both
named by the VM itself under `CRATONVM_DBG_JITC=1`:

* **OSR is refused outright and permanently.**
  `osr-DENY (unbridged invokedynamic) …` then `OSR-compile FAILED … — method
  marked OSR-denied for the rest of this process`. The guard
  (`jit_bridge.rs`, "RBC.7 (relaxed)") admits only `StringConcatFactory` sites,
  because every other bootstrap lowers to an unconditional frame-deopt
  (reason 8) and an OSR frame cannot take that trap safely. It is **method-wide**
  — one indy anywhere denies every loop in the method.
* **Whole-method compiles are undone at runtime.** The method DOES compile
  (`full-compile IndyScopeProbe.makeAndCall(I)I … len=2191`), then executes the
  indy, hits the reason-8 stub, and `DeoptimizationController::deoptimize` with
  `action=MakeNotCompilable` retires it permanently — correctly, since the trap
  is unconditional and would reproduce.

`probes/IndyScopeProbe.java` isolates it with four arms that differ only in
where the lambda is created:

| arm | HotSpot | CratonVM |
|---|---:|---:|
| loop with the lambda created **outside** the loop method | 3.6 ns | **35.2 ns** |
| identical loop, lambda created **inside** it (method has an indy) | 2.6 ns | **1045.3 ns** |
| SAM call in a small hot method | 3.5 ns | **32.6 ns** |
| same method, but it creates the lambda | 5.7 ns | **2792.4 ns** |

**The SAM call itself is fine** — 32-35 ns once the calling method is compiled.
What is broken is that a method *containing* an `invokedynamic` never stays
compiled, and then everything that calls it pays the 1900 ns transition. Reactor
and WebFlux assembly is nothing but methods that create lambdas, which is why
`probes/ReactorProbe.java` reads 72-181x against HotSpot on operator assembly
while its own no-reactive-types control arm reads 1.3x.

## What this is NOT

* Not lambda *dispatch*. `[LAMBDA-PROF]` and `[LAMBDA-JIT]` both show the lambda
  machinery working: `compiled_hits=1017675`, `declines=0`.
* Not the lambda inline-cache thunk being inert, though it is
  (`site_calls=0 site_adapters=0` over 1 M dispatches, with
  `CRATONVM_JIT_LAMBDA_SITE`/`_ADAPTER` both default-ON). That arm is never
  reached because the compiled caller never exists.
* Not tier-up thresholds: `CRATONVM_JIT_LAMBDA_TIERUP=0` moves the lambda arm 4%.
* Not the field-site cache. Its hit rate on the WebClient exchange is 83.9% at
  the default 1024 slots and saturates at 92.6% by 32768 — but buying those
  95 000 misses back moved neither the exchange (30.2 -> 30.2 ms/op) nor
  `ReactorProbe` (118k -> 115k ns/op, inside noise). Sized and rejected; see
  `site_cache.rs`'s own "that number does not generalise" note, now answered.

## What was fixed: the `invokestatic` half

`jit::helpers::try_jit_static_bytecode_callee` gives the dispatch tail the cache
the interpreter always had — a site-keyed
`(Arc<CachedBytecodeMethod>, RedefineGate)` memo, entered with the same frame
template `lambda.rs::try_invoke_cached_lambda_impl` uses to run an interpreted
callee from a Rust helper. Its refusals are in its doc comment; the two that
carry the safety argument are:

* **no native anywhere has this `(name, descriptor)`**
  (`might_have_method_descriptor`), which removes the native-override,
  `SyntheticStub`-yield and redefine-shadow questions rather than reproducing
  them; and
* **`jit_static_owner_override` declines**, which keeps the entire
  loader-identity surface — BUG-JIT-INVOKESPECIAL-LOADER-20260726, the
  `AotIntegrationTests` static-owner hang — on the untouched path.

`invokestatic` was taken first because it is where the volume is
(`kind_static=2_986_402` of `disp_calls=3_356_461`) and because it has no
receiver, hence no virtual retarget, no interface rules and no receiver guard.

Measured, six interleaved pairs, `CRATONVM_JIT_DENY=XferProbe.callee`:
**1310 -> 259 ns/op**, `sink` byte-identical to the baseline and to HotSpot.

`CRATONVM_DBG=mic-prof` proves it fires rather than merely existing — the two
new census slots read `out_static_bc=1048575 out_static_bc_refused=0` against
`out_tail=1048575` on `XferProbe`, i.e. it serves 100% of that probe's tail.

**One correctness hole was found by the tree's own guard and is worth recording
because the same shape will catch the next memo added here.** The new memo
resolves its owner through `get_loaded_class_id(info.class_name)` — a class
NAME — so a second loader defining that name makes the template describe the
wrong class's method, and that event publishes no compiled code, redefines
nothing and supersedes no tier. It therefore had to join
`flush_class_identity_dispatch_memos`, not just the generation flush.
`a_jit_generation_change_clears_every_site_keyed_memo` failed until the memo was
given its population line, which is exactly what that assertion exists for.

## Still open: the virtual / interface / special half

`out_static_bc=98_201` of `out_tail=342_867` on `probes/ReactorProbe.java` — the
fix serves 29% of that workload's tail calls, and the remaining 71% are kinds
0/1/2. Their correctness surface is the one this pass deliberately did not open:
a receiver-class guard to re-test per hit, the interface/abstract retarget, and
the `globally_named` loader term that `virtual_dispatch_target_cached` already
computes but that nothing yet feeds to a bytecode-callee memo. The MIC is where
the volume for those is: `mic_calls=9_437_184` with `hit_entry=879_584` and
`hit_noentry=1_954_260`, i.e. 1.95 M dispatches that found the receiver and had
no compiled callee to enter.

Because that half is still by-name, neither `ReactorProbe` nor the WebClient
exchange moves measurably yet (both inside noise across interleaved rounds) —
the improved population is ~3% of dispatch calls there. The isolated 5x is real;
do not quote it as a suite number.

## Reproducing

```
CRATONVM_BIN=<bin> bash probes/wcit-exchange-ab.sh run cv XferProbe 2000000
CRATONVM_JIT_DENY=XferProbe.callee CRATONVM_BIN=<bin> \
  bash probes/wcit-exchange-ab.sh run cv XferProbe 2000000
CV_EXTRA=--nojit CRATONVM_BIN=<bin> \
  bash probes/wcit-exchange-ab.sh run cv XferProbe 2000000
```

`IndyScopeProbe` / `ReactorProbe` / `IndyProbe` / `HoistedLambdaProbe` take the
same driver. All are pure CPU with no sockets, so unlike the WebClient exchange
probe they are stable on a loaded shared host — `XferProbe` read 1902.5 and
1901.4 ns/op on consecutive runs at load 25.

Related: [[jit-entries-per-call-cost-is-the-call-dense-wall]],
`known-issues/perf/webclient-integration-tests-reactive-exchange-gap-20260822.md`.
