# A lambda SAM call is 40x the same call on an ordinary class — and it is not the JIT, not interface dispatch, and not class resolution

**Status: OPEN. A second, DIFFERENT mechanism was found and confirmed
(section 5): a lambda body never contributes to JIT invocation counting, so it
is never compiled. A fix for that was built, measured to help on the happy
path, and found to CRASH the VM on a lambda body that throws — not shipped.
See section 5.4 for what the next attempt needs.** Filed 2026-08-17, following
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
that did not convert; see `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md`
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

## 5. The missing tier-up, found and confirmed — a fix was built, measured to help, and found to crash on exception. Not shipped.

Filed as a continuation, same day. Sections 1 to 4 above characterise the SAM-call cost and refute the obvious per-invoke memo. This section names a different, independently verified mechanism and reports why the fix for it is not landed.

### 5.1 The root cause: a lambda impl method never contributes to JIT tier-up

`CRATONVM_DBG_JITC=1` against `probes/SamDispatchDecompositionProbe.java` shows every non-lambda receiver's SAM body compiling normally:

```
[cratonvm-jitc] bg-compile SamDispatchDecompositionProbe$NamedOp.applyAsInt(I)I tier=C1 optimized=false
[cratonvm-jitc] bg-compile SamDispatchDecompositionProbe$NamedOp.applyAsInt(I)I tier=C2 optimized=true
[cratonvm-jitc] bg-compile SamDispatchDecompositionProbe$1.applyAsInt(I)I tier=C1 optimized=false
```

while grepping the same log for `lambda$` (the synthetic name javac gives a lambda's implementation method) returns zero lines, in any run, at any iteration count. `try_invoke_cached_lambda_impl` (`vm/src/runtime/interpreter/lambda.rs`) resolves the impl method into a `CachedBytecodeMethod` and runs it via `Frame::new_pooled_cached` plus `execute_prebuilt_frame`, the plain interpreter loop, and never once calls `shared.jit.profile_store.increment_invocation` nor consults `shared.jit.jit_cache`, unlike every other cached-dispatch arm (`execute_invokestatic_cached`, `execute_invokevirtual_cached`). A lambda body reached only through lambda dispatch therefore cannot be nominated for JIT compilation no matter how many times it is called. The one narrow exception already in the file is the TDigest `get(int)D` special case, which checks `jit.jit_cache` for a body compiled via some other call path and enters it directly, but does nothing to get it compiled in the first place.

This is a different defect from section 3's refuted memo: that one targeted class-name resolution cost inside an already-running dispatch; this one is that the dispatch never runs the JIT-eligible counting at all.

### 5.2 The fix, and what it measured

Branch `fix/lambda-sam-dispatch-cache-20260817` off `dev`, not merged, not pushed to `dev`. Two parts, both scoped to `try_invoke_cached_lambda_impl`:

1. Warmup counter, unconditional, mirroring `execute_invokestatic_cached`'s `Bytecode` arm exactly: `cached.invoc_key()`, `profile_store.increment_invocation`, and, at the threshold, `tiered_manager.on_method_invocation_observed`. Safe from any caller context; the only cost is a counter bump.
2. Direct-compiled-call fast path, gated on a new `caller_frame_idx: Option<usize>` parameter threaded through a new `try_lambda_dispatch_with_frame` (the plain `try_lambda_dispatch` wraps it with `None`, unchanged for its nine other call sites). Only `invoke.rs`'s main `invokeinterface`/`invokevirtual` handler passes `Some(frame_idx)`, because it is the one caller that can prove `thread.frames[frame_idx]` is the frame currently executing the bytecode that reached lambda dispatch. When `Some` and the impl has no exception table of its own (mirroring the existing safety gate `dispatch_virtual.rs`'s poly-cache arm applies before the same call), it probes `jit.jit_cache` and, on a hit, calls the existing `execute_jit_call_decoded`, the same primitive the poly-cache arm already uses for a receiver whose class the monomorphic cache missed.

`execute_jit_call_decoded` was built to serve the interpreter's own dispatch loop: it pushes its result onto `thread.frames[frame_idx].stack`, where that loop expects to find it. `try_invoke_cached_lambda_impl`'s contract is different: return a `Value` directly. So the fast path records the stack depth before the call, pops back exactly what was just pushed, and hands that back as its own `Ok(Some(Some(value)))`.

Confirmed working, mechanically, via new `CRATONVM_DBG_JITC` diagnostics added alongside it:

```
[cratonvm-jitc] lambda-warmup SamDispatchDecompositionProbe.lambda$static$0(I)I invoc_count=2 threshold=500
[cratonvm-jitc] lambda-tiered-enqueue SamDispatchDecompositionProbe.lambda$static$0(I)I tier=Some(C1) invoc_count=500
[cratonvm-jitc] lambda-fast-path SamDispatchDecompositionProbe.lambda$static$0(I)I
```

The counter climbs, nomination fires at exactly the configured threshold, and the compiled entry is found and entered on later calls, all for a method name that never appeared in this log before. Clean runs (flag off, since the flag's own `eprintln` on every fast-path hit dominates the timing) show a real, if partial, improvement: `ifaceLambda` 435 to about 370-380 ns, `ifaceCap` 702 to about 440-475 ns, `boxedLambda` 2438 to about 1250-1465 ns (n=2, this loaded box). Not the ~9 ns `ifaceClass`/`ifaceAnon` already achieve; the gap to that is unexplained (possibly C1-only, since the probe's short runtime may not reach C2 before it exits, or residual per-call overhead in the fast path's own gates and the push/pop round-trip).

### 5.3 The crash — why this is not shipped

`probes/LambdaCorrectnessProbe.java` (new; exercises capturing lambdas, method references, `andThen`-composed default-method dispatch, and, the case that matters, a static lambda body that conditionally throws after warmup):

```java
private static final IntUnaryOperator maybeThrow = v -> {
    if (v == 999) throw new RuntimeException("boom-" + v);
    return v * 2;
};
```

called 2000 times with two throwing inputs, inside a try/catch at the call site, crashes the VM, reproducibly, on repeated runs:

```
thread 'main-vm' panicked at vm\src\runtime\value_stack.rs:827:19:
[PANIC_IN] LambdaCorrectnessProbe.lambda$static$0(I)I pc=12 max_stack=3 :: index out of bounds: the len is 24 but the index is 18446744073709551615
```

18446744073709551615 is `usize::MAX`, a `0usize - 1` underflow. The panic site's own `[PANIC_IN]` tag (from `execute_prebuilt_frame`'s handler) shows it firing inside the interpreted re-run of the same lambda body, not inside the new fast-path code directly, meaning the fast path's compiled-call attempt for an earlier throwing call left `thread`'s pooled frame/stack state corrupted, and a later call (interpreted, because the fast path had already declined or this was a different invocation) inherited the corruption.

The suspected mechanism: `execute_jit_call_decoded`'s exception handling (`route_jit_signal_exception`) is written for its one existing caller, which runs inside the interpreter's own per-instruction stepping loop. A `CachedCallResult::FramePushed` result there means a new interpreter frame was pushed for the exception path, and the outer loop will pick it up and run it to completion as part of normal control flow. `try_invoke_cached_lambda_impl` is not that loop; it is a one-shot subroutine that must return a complete `Value` synchronously. If a `FramePushed` (or similar) result comes back, this fast path's `debug_assert!(matches!(ccr, CachedCallResult::Handled))` is compiled out in release, so nothing catches the mismatch; the fast path just reads whatever is at `thread.frames[frame_idx].stack.len()`, which may no longer describe what happened, and returns as if all were normal while a now-orphaned frame, or corrupted pool state, sits behind it.

This is exactly the risk section 4 above named: giving lambda call sites a cached invoke target of their own is real JIT/interpreter work, not a one-line change. Reusing `execute_jit_call_decoded` as-is gets the happy path right and the exception path wrong, because that primitive's contract assumes integration with the interpreter's own loop that a one-shot dispatch helper does not have.

### 5.4 What is proven and what is not

* Proven: the root cause (5.1) is real and independent of sections 1-4's refuted memo. The warmup-counter plus jit_cache-check plus direct-call shape of a fix is the right one; it activates, nominates, compiles, and finds its own compiled entry correctly (5.2's diagnostics), and the happy-path speedup is real, if partial.
* Not proven safe: exception propagation out of a lambda body invoked via this fast path. Section 5.3's crash is deterministic and was found by the third correctness probe written for this page, on the specific case (a throw after the impl is already compiled) the first two happy-path-only probes could not have caught. Do not ship this shape of fix without solving that.
* What the next attempt needs: either (a) after `execute_jit_call_decoded` returns something other than a plain value-pushed `Handled`, drive the interpreter's own frame-stepping loop from the caller's `frame_idx` until back at the entry depth, so a pushed exception-handling frame actually runs instead of being silently dropped; or (b) skip `execute_jit_call_decoded` for this caller entirely and build a narrower direct-call primitive that, on any exceptional JIT signal, constructs and returns a Rust-level `MethodCallFailed::ExceptionThrown` synchronously, matching how the existing interpreted lambda path already propagates exceptions correctly through ordinary `?`-propagation, rather than touching `thread.frames` at all. Option (b) is probably the more tractable of the two for a one-shot subroutine and is the more promising next step.
* The branch (`fix/lambda-sam-dispatch-cache-20260817`, off `dev`) is pushed as a reference for whoever continues this. Its `HEAD` is exactly the crashing state described above, kept for the diagnostics (`CRATONVM_DBG_JITC`'s `lambda-warmup`/`lambda-tiered-enqueue`/`lambda-fast-path` lines) and the two probes (`probes/SamDispatchDecompositionProbe.java` already existed; `probes/LambdaCorrectnessProbe.java` is new and is the one that caught this).
