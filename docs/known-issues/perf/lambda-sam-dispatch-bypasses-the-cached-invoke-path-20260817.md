# A lambda SAM call is 40x the same call on an ordinary class — and it is not the JIT, not interface dispatch, and not class resolution

**Status: OPEN, ATTRIBUTED to a mechanism, with one plausible fix already built
and REFUTED by measurement.** Filed 2026-08-17, following
`known-issues/hibernate-reactive/residual-seven-after-the-afc-fix-20260817.md`,
which showed the five remaining Windows hibernate-reactive classes are correct
but 12–60x slow and put ~55% of their samples in `CompletableFuture`
composition.

## 1. The ladder — `probes/SamDispatchDecompositionProbe.java`

Every row is one `int -> int` call, same loop shape in a called method, no
boxing except the last. 500 000 ops, ABBA-interleaved, quiet box. Read against
HotSpot `-Xint`, never C2 — this repo's yardstick is "2.5x versus `-Xint` is
the statement about this VM".

| shape | HotSpot `-Xint` | CratonVM | vs `-Xint` |
|---|---|---|---|
| `staticCall` — `invokestatic` | 14.3–16.8 ns | **6.5–7.0 ns** | **0.4x** |
| `virtualCall` — `invokevirtual` | 16.4–18.5 ns | **8.1–8.8 ns** | 0.5x |
| `ifaceClass` — `invokeinterface`, NAMED class | 16.0–18.9 ns | **8.5–9.3 ns** | **0.5x** |
| `ifaceAnon` — `invokeinterface`, ANONYMOUS class | 15.0–16.5 ns | **8.8–9.6 ns** | 0.6x |
| `ifaceLambda` — same call, LAMBDA receiver | 25.2–27.8 ns | **354.8–370.2 ns** | **13–14x** |
| `ifaceMref` — METHOD REFERENCE receiver | 25.3–28.2 ns | 376.5–380.2 ns | 14x |
| `ifaceCap` — CAPTURING lambda | 27.0–30.0 ns | 494.5–504.0 ns | 17x |
| `boxedLambda` — `Function<Integer,Integer>` | 159.6–170.9 ns | 1339.7–1411.3 ns | 8x |

**Three things this rules out at once.**

* **Not the JIT.** `staticCall`, `virtualCall` and `ifaceClass` are all 2x
  FASTER on CratonVM than on HotSpot's interpreter. The compiler works.
* **Not `invokeinterface`.** An interface call on an ordinary named class is
  8.5–9.3 ns. The identical call site, identical interface, identical body —
  with a lambda as the receiver — is **355 ns. A ~40x penalty for the receiver
  being a lambda proxy, and nothing else changed.**
* **Not "lambdas are inherently slow".** HotSpot's own interpreter charges only
  ~1.6x for the same substitution (16.0 → 25.2 ns).

Capturing adds ~140 ns on top (`ifaceCap` vs `ifaceLambda`) and boxing adds
~1000 ns (`boxedLambda`) — both additive, neither the main term. `boxedLambda`
is the shape `CompletableFuture` actually uses.

## 2. Inside the lambda path — `CRATONVM_DBG=lambda-prof`

The in-tree instrument splits `try_lambda_dispatch`. Read the SPLIT, not the
absolutes: the instrument's own `Instant::now` pairs inflate the total from
~355 ns to ~530 ns.

```
[LAMBDA-PROF] calls=1200000 total=523ns/call  lookup=46  prep=73  target=232  other=171
```

* `lookup` (the `lambda_proxies` read + `Arc` clone) — **46 ns, ~9%.** The map
  value is already `Arc<LambdaCallSite>`, so this is a lock and a hash.
* `prep` (descriptor splits, capture prepending, `coerce_lambda_args`) — 73 ns.
* **`target` (the impl invoke) — 232 ns, ~45%** — for an impl body that
  `staticCall` executes in 7 ns.
* `other` (guards, descriptor comparisons, arm selection) — 171 ns, ~32%.

## 3. The hypothesis that looked obvious, and the measurement that killed it

`try_lambda_dispatch`'s `InvokeStatic` arm reaches its impl through

```rust
invoke_shared(shared, thread,
    &lcs.impl_handle.class_name,      // by NAME
    &lcs.impl_handle.member_name,
    &lcs.impl_handle.descriptor, &full_args)
```

and `invoke_shared` begins with `load_class_concurrent(class_name)` — a
per-class-NAME lock plus a string hash — on **every SAM invocation**, for a
value that is a per-call-site constant. That is this project's "never bind by
NAME" rule in its performance form, and the file even carries a sibling fix
with the same shape (`lambda_impl_dispatch_override`, whose comment records
hoisting a per-proxy constant that had been 11.3% of a run).

**It was built and it moved nothing.** A `OnceLock<ClassId>` memo on
`LambdaCallSite` plus an `invoke_shared_on_class` entry point (`invoke_shared`
minus the name resolution), A/B'd ABBA on the same box:

| | before | after |
|---|---|---|
| `ifaceLambda` | 356.3 / 318.2 ns | 360.7 / 310.4 ns |
| `ifaceCap` | 464.2 / 450.9 ns | 389.4 / 502.5 ns |
| `lambda-prof target` | 226–348 ns | 219–294 ns |

Inside the noise on the wall clock, and `target` — the term it was aimed at —
did not move. The change was reverted rather than shipped: it would have cost a
memo that must be invalidated if class unloading is ever wired up (today
`ClassUnloader::unload_classes` has no callers outside its own tests), in
exchange for nothing.

This is the third time this VM's flat profile has offered a structural lead
that did not convert; see `vm-per-call-dispatch-cost-20260813.md`
§3 for the other two.

## 4. What the evidence actually points at

`load_class_concurrent` is cheap because the class is already loaded. What is
left in `target` is everything AFTER it — `invoke_on_class_shared_inner` — and
that is the generic dispatch entry point, which per call:

* copies the argument slice (`let mut rooted_args = args.to_vec()`) — a heap
  allocation per invocation;
* pins them (`PinnedDispatchArgs::new`);
* takes the `lambda_proxies` read lock **again** to ask whether the receiver is
  itself a lambda proxy (the third such lookup in one SAM call — once in
  `invoke.rs`'s `is_lambda_proxy_receiver`, once in `try_lambda_dispatch`, once
  here);
* then resolves the method by name+descriptor and applies retarget logic.

An ordinary `invokeinterface` on a named class reaches none of it: it goes
through the resolved-callsite cache (`InvokeCache` / `CachedInvokeTarget`) and
the JIT's monomorphic inline cache, which is why it is 9 ns.

**So the shape of the defect is: a lambda SAM call permanently bypasses the
cached-invoke fast path and funnels into the generic, allocating, name-keyed
dispatch machinery.** The fix is therefore not another memo inside that
machinery — it is giving lambda call sites a cached invoke target of their own,
so the second and subsequent invocations of a given proxy skip
`try_lambda_dispatch` → `invoke_shared` → `invoke_on_class_shared_inner`
entirely. That is real JIT/interpreter work, not a one-line change, which is
why this page stops here.

Whoever takes it:

* A/B on `MultithreadedInsertionTest`'s wall clock (219 s today, HotSpot
  18.5 s), **not** on the probe — the 2026-08-09 lambda fast-path widening was
  4x on a microbenchmark and 0% on the Spring workload.
* `probes/SamDispatchDecompositionProbe.java` is the unit-level check, and its
  `ifaceClass` row is the target: 9 ns is what this VM already achieves for the
  same call on a non-lambda receiver.
* Watch `ifaceCap` separately — capturing lambdas carry an extra ~140 ns that a
  call-site cache alone will not remove.
