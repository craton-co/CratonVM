# A compiled caller calling an INTERPRETED callee cost 1900 ns — 5x more than never compiling the caller at all

**Status: FIXED 2026-08-23. All four invoke kinds now enter an uncompiled
callee through the call site's own cached interpreter frame template.
`invokestatic` landed 2026-08-22 (1310 -> 259 ns/op);
`invokevirtual` / `invokeinterface` / `invokespecial` landed here.
Measured on one binary with the kill switch as the A/B, four interleaved
rounds, 2 000 000 iterations, `probes/XferProbe2.java`:**

| kind | compiled -> compiled | both interpreted (`--nojit`) | compiled -> INTERPRETED, memo OFF | memo ON | ratio |
|---|---:|---:|---:|---:|---:|
| `invokestatic` | 25 | 283 | 238 | 240 | (landed 08-22) |
| `invokevirtual` | 25 | 333-384 | **1705-2012** | **275-285** | **6.3-7.1x** |
| `invokeinterface` | 27 | 501-572 | **2148-2733** | **440-493** | **4.9-5.5x** |
| `invokespecial` | — | 634-639 | **2643** | **458** | **5.8x** |

Re-measured on the final binary, three further interleaved rounds on a busier
window (absolute values inflated on BOTH arms, ratios unchanged): virtual
2724-4058 -> 319-521, interface 3815-5408 -> 587-706.

**Every kind is now BELOW its both-interpreted control, so compiling the caller
is no longer a pessimisation for any of them.** That is the number that
mattered: before this, compiling the caller and not the callee was 4.9x slower
(virtual) and 3.4x slower (interface) than compiling neither.

This was never a lambda bug, a reactive bug or a `java.time` bug: it was the
JIT's interpreter-fallback path, and it applied to EVERY compiled method
calling a callee the JIT did not compile — which, on any real application, is
most callees.

## The measurement

`probes/XferProbe2.java` — a hot loop calling a one-line callee, one arm per
invoke kind, with `CRATONVM_JIT_DENY` keeping the callee interpreted while the
caller stays compiled. Three configurations per arm, same binary:

```
CRATONVM_JIT_DENY=.calleeVirtual  <bin> -cp . XferProbe2 2000000 virtual
CRATONVM_JIT_DENY=.calleeVirtual  CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0 \
                                  <bin> -cp . XferProbe2 2000000 virtual
                                  <bin> --nojit -cp . XferProbe2 2000000 virtual
```

**Two traps in that command line, both of which silently measure the wrong
thing:**

* `CRATONVM_JIT_DENY` is a SUBSTRING match on `Class.method`, and the
  virtual/interface callees live on `XferProbe2$Impl`, not on `XferProbe2`.
  The probe's own header used to suggest `CRATONVM_JIT_DENY=XferProbe2.calleeVirtual`,
  which matches nothing — the arm then measures the UNDENIED configuration and
  reads ~25 ns/op, i.e. it looks like there is no problem at all.
* A denied callee can still be INLINED into the compiled caller: the deny stops
  a separate compile, not a splice. The `special` arm is bimodal for exactly
  this reason — 24 ns/op in the rounds where `Base.calleeSpecial` was spliced,
  ~2600 (OFF) / ~460 (ON) in the rounds where it dispatched. Only rounds in
  which BOTH arms dispatch are comparable, and `CRATONVM_JIT_INLINE_CALLS=0`
  is what makes that reproducible.

## Why it cost what it did

`perf` on the denied arm, steady state. A compiled caller's call to an
uncompiled callee fell out of `jit_invoke_dispatch`'s fast arms into
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
monomorphic and the callee never changes, yet each call re-resolved the class
by name, re-ran the native-override arbitration, re-cloned the `CodeAttribute`
and re-padded the bytecode.

The interpreter does none of this: its call sites hold a
`CachedInvokeTarget::Bytecode` with a prebuilt `Arc<CachedBytecodeMethod>`.
**The JIT's dispatch helper had a cache for a COMPILED callee (`DISPATCH_CACHE`,
`VIRTUAL_DISPATCH_CACHE`) and none at all for an interpreted one.**

## The fix

Two functions, one per shape, both giving the dispatch tail the cache the
interpreter always had — a site-keyed `(Arc<CachedBytecodeMethod>,
RedefineGate)` memo entered with the same frame template
`lambda.rs::try_invoke_cached_lambda_impl` uses:

* `jit::helpers::try_jit_static_bytecode_callee` — `invokestatic`, keyed by the
  call site alone (a static call has no receiver, so the target is a pure
  function of the site). Landed 2026-08-22.
* `jit::helpers::try_jit_virtual_bytecode_callee` — `invokevirtual` /
  `invokeinterface` / `invokespecial`, keyed by **`(call site, receiver
  ClassId)`**. That extra key term is the whole difference: a virtual target is
  not a function of the site, and the receiver's runtime class is what selects
  the override. Making it part of the KEY rather than a per-hit guard lets a
  bimorphic site keep both templates instead of evicting one on every
  alternation, and removes the guard from the hot path entirely — a hit IS a
  proof that the receiver's class is the one the template was resolved against.

Both are default-ON with a kill switch (`CRATONVM_JIT_STATIC_BYTECODE_CALLEE=0`,
`CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0`), so the A/B is inside one binary.

### What the virtual resolution refuses, and why each refusal is the point

Every obligation is discharged ONCE, at resolution; a site that cannot
discharge them caches a refusal and never asks again.

* **`cacheable_receiver && globally_named`.** This pair is the entire
  loader-identity surface of a virtual dispatch: `cacheable` means the dispatch
  class IS the receiver's runtime class (not an array, not `ClassId(0)`, not an
  interface/bare-`Object` fallback to the CP class), and `globally_named` means
  that name resolves back to that same class id. It is the virtual counterpart
  of the static twin's `jit_static_owner_override` refusal, and the same term
  `publish_mic_rust_cached_entry` and the by-name compile probe already gate on
  — see the `ApplicationContextAotGeneratorTests` note at those sites for what
  eight copies of one class name does without it.
* **no native anywhere has this `(name, descriptor)`**
  (`might_have_method_descriptor`), which removes the native-override,
  `SyntheticStub`-yield and redefine-shadow questions rather than reproducing
  them, and subsumes `force_native_over_real_jdk_bytecode` (whose triples all
  name a REGISTERED native).
* **the receiver's class is not a lambda proxy.** Every caller already routes
  proxies away above this point; the resolver refuses them anyway, because a
  memo that is correct only because of where it is called from is one move away
  from being wrong.
* **the declaring class is already initialised.** `invoke_shared` would run
  `<clinit>` on the way in. Unlike every other refusal this one is NOT cached —
  it is the single condition that becomes true on its own, and a cached refusal
  would deny a site its fast path for the life of the process over a race with
  class initialisation.
* **`build_lambda_impl_cached` accepts the method found by walking UP from the
  receiver's own class**: not native, not `synchronized`, not abstract (an
  abstract method has no `Code`), no native shadow on the declaring class.
  Rooting the walk at the receiver is what makes this override-correct rather
  than a call to the call site's static type — the `Object.equals` /
  `Long.equals` defect the MIC's own "VIRTUAL DISPATCH FIX" comment records.

`invokespecial` differs in one term: its resolution root is the loader-resolved
CP owner (`resolve_class_loader_aware`), not the receiver, because
invokespecial must NOT re-target onto the receiver's runtime class — that turns
a super-call into a self-call and recurses forever (the picocli
`AbstractParseResultHandler.execute` `StackOverflowError`).

Per hit, only what can change is re-tested: the declaring class's
`RedefineGate`, the process-wide `any_class_redefined` latch, the arity, and
the template's own `(name, descriptor)` against the site's.

### `apply` — the name that made the interface half read zero

The first cut of the virtual memo deferred to `site_name_is_special_cased`, the
name-only list the leaf-native path uses. `apply` is on it, and `apply` is the
SAM of `java.util.function.Function` — the single most common interface method
in reactive code. `CRATONVM_DBG=mic-prof` said so exactly:

```
virtual  out_virt_bc=1048575 out_virt_bc_refused=0          <- 100% served
iface    out_virt_bc=0       out_virt_bc_refused=1048575    <- 100% refused
```

`apply` earns its place on that list through ONE rescue: `invoke_or_native`
redirects `apply(Ljava/lang/Object;)Ljava/lang/Object;` to
`applyAsInt`/`applyAsLong`/`applyAsDouble` when the dispatch class is
`java/util/function/To{Int,Long,Double}Function`, because those interfaces
declare no `apply` at all and naive dispatch raises `NoSuchMethodError`
(Spring/Eureka). `virtual_site_name_is_special_cased` narrows the refusal to
that triple. Two independent things already make it unreachable from this path
— an interface that declares no `apply` has no `Code` for it, and a receiver
whose class is an interface different from the call site's fails
`cacheable_receiver` — and the explicit test is there so a later change to
either does not quietly re-open it. After the narrowing the interface arm reads
`out_virt_bc=1048575 out_virt_bc_refused=0`.

**A finding worth keeping from the same probe:** `iface` and `iface2` differ
only in the SAM's NAME (`apply` vs `step`) and take completely different
routes. `step` is devirtualized and INLINED outright — the arm reads ~23 ns/op
even with all of `XferProbe2$Impl` denied, and the dispatch helper is never
entered (no `[DISP_CENSUS]` line at all). `apply` is not inlined. Both arms are
kept in the probe because the pair is the evidence that the name, not the
shape, decides.

## Correctness

* `cargo test -p cratonvm-vm --lib`: **2600 passed, 0 failed**, including
  `a_jit_generation_change_clears_every_site_keyed_memo`.
* `regression-suite/run.sh`: **69 of 69 scheduled vectors passed, 0 failed.**


* The memo joins `site_keyed_memos!`, so the JIT-generation and
  class-identity flushes drop it like every other dispatch memo, and
  `a_jit_generation_change_clears_every_site_keyed_memo` fails if a new memo is
  added without a population line — which is what forced the static twin into
  `flush_class_identity_dispatch_memos` when it was first written.
* It re-tests the template's `(name, descriptor)` against the site's on every
  hit. `flush_raw_entry_dispatch_caches` is the mechanism that closes
  `JitInvokeInfo` address reuse, and it runs before every consult; the extra
  test is kept because this memo is also consulted from
  `jit_invoke_virtual_mic`, where a site key has already been observed once to
  name a different site (the `OffsetDateTimeTest` discovery crash).
* It does not starve tier-up. In `jit_invoke_dispatch` the invocation counter
  and compile attempt run BEFORE `out_tail`, i.e. before this hook; in
  `jit_invoke_virtual_mic` the `try_jit_compile_callee` probe runs before it in
  both the entryless-hit and the miss arm. A callee that becomes hot is still
  nominated and still tiers up — it simply stops paying a by-name resolution
  while it waits.

## Hooked at four sites

`jit_invoke_dispatch`'s `0 | 2` arm and its `1` arm, and
`jit_invoke_virtual_mic`'s entryless-hit and cache-miss arms. The MIC is where
the volume for virtual/interface lives: `mic_calls=9_437_184` with
`hit_entry=879_584` and `hit_noentry=1_954_260`, i.e. 1.95 M dispatches that
found the receiver and had no compiled callee to enter.

The `0 | 2` arm also switched from `virtual_dispatch_target_for_receiver` to
`virtual_dispatch_target_cached` — same class-name answer by the same
conditions, plus the `globally_named` round-trip the memo gates on, and
`flush_class_identity_dispatch_memos` has already run for that call.

## What this is NOT

* Not lambda *dispatch*. `[LAMBDA-PROF]` and `[LAMBDA-JIT]` both show the lambda
  machinery working: `compiled_hits=1017675`, `declines=0`.
* Not tier-up thresholds: `CRATONVM_JIT_LAMBDA_TIERUP=0` moves the lambda arm 4%.
* Not the field-site cache. Its hit rate on the WebClient exchange is 83.9% at
  the default 1024 slots and saturates at 92.6% by 32768 — but buying those
  95 000 misses back moved neither the exchange (30.2 -> 30.2 ms/op) nor
  `ReactorProbe` (118k -> 115k ns/op, inside noise). Sized and rejected.

## What stays open, and where

Two things this page carried that are NOT this defect and do not close with it:

* **`invokedynamic` keeps a method from staying compiled.** A method containing
  an unbridged indy is permanently denied OSR (`jit_bridge.rs`, the "RBC.7
  (relaxed)" guard admits only `StringConcatFactory` sites), and a whole-method
  compile that does succeed is retired at runtime by
  `DeoptimizationController::deoptimize` with `action=MakeNotCompilable` when
  the reason-8 stub fires. That is why reactive assembly is hit hardest: it is
  nothing but methods that create lambdas. **This page's fix changes the PRICE
  of that, not the fact** — such a method is still interpreted, but calling it
  now costs ~285-490 ns instead of ~1900-3200. The indy rule itself is tracked
  in the JIT notes, not here.
* **`known-issues/perf/webclient-integration-tests-reactive-exchange-gap-20260822.md`**
  is its own page and stays open on its own terms.

## Reproducing

```
CRATONVM_BIN=<bin> bash probes/wcit-exchange-ab.sh run cv XferProbe2 2000000 virtual
```

or directly, which is what the numbers above were taken with:

```
<bin> -cp <probes> XferProbe2 2000000 <static|virtual|iface|iface2|special>
```

`XferProbe2` / `IndyScopeProbe` / `ReactorProbe` / `IndyProbe` /
`HoistedLambdaProbe` are pure CPU with no sockets, so unlike the WebClient
exchange probe they are stable on a loaded shared host.

Related: [[jit-entries-per-call-cost-is-the-call-dense-wall]],
`known-issues/perf/webclient-integration-tests-reactive-exchange-gap-20260822.md`.
