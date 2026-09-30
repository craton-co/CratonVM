# Proposal: let a virtual thread unmount under the method-handle natives

**Status: proposal — filed 2026-09-28 by interpreter round i1 wave 26, lane
L4. Stage 0 (the identity case) landed in the same wave; stage 1's
`primitive -> reference` half, with stage 3's widening, landed in wave 28
(see its note); `V -> reference` needs lane L7's `return` arm.**

## Problem, with evidence

Wave 26 fixed
`docs/internal/fixed-bugs/interpreter-L4-a-virtual-thread-that-unmounts-under-a-native-loses-the-natives-return-conversion-FIXED-20260928.md`
by PINNING: a virtual thread that parks or sleeps while a native that
re-entered Java is beneath it (`native_callee_memo::continuation_pinned_by_native`)
blocks its carrier instead of unmounting. That is HotSpot's rule for a real
native frame. But on HotSpot the method-handle and reflection paths have NO
native frame (JDK 25 reflection is `DirectMethodHandleAccessor` ->
`LambdaForm`s, all Java), so HotSpot unmounts there.

Stage 0 (landed, wave 26): where the door's post-call work is the identity --
a reference leaf result handed back unchanged through adapters that only
reshape arguments -- the door declares itself transparent (`lang_invoke.rs`
`MH_TAIL`) and the thread unmounts, as on HotSpot and as it did before wave
26. What still pins, and did not on HotSpot:

* a `void` or primitive leaf reached through `Method.invoke` (`--jdk-only`:
  `invokeImpl` -> `invokeExact`, then boxing / `void -> null`) or
  `MethodHandle.invoke` / `invokeExact` / `invokeWithArguments` -- e.g. a
  Spring MVC controller method returning `void` or `int`, an `@Async void`
  body, a `@Scheduled` method, on virtual threads;
* a constructor handle (`findConstructor`, `newInstance` in `--jdk-only`);
* anything under `filterReturnValue` / `catchException` / `tryFinally` / the
  loops, and a guard's test, a filter or a combiner (their result is read
  after the call).

Each such blocked virtual thread holds a carrier (the starvation watchdog adds
carriers, so nothing hangs, but N blocked requests hold N OS threads);
`jdk.VirtualThreadPinned` (reason `NativeMethod`) shows it.

## Design

The remaining conversions are exactly what wave 25's `ContinuationReturnAdapter`
models for the lambda door (`try_lambda_dispatch` records one when a yield
leaves it; the interpreter's value-return arms apply it on the remounted
frame's return; owner: lane L7).

1. Extend `ContinuationReturnAdapter` with the method-handle conversions:
   `V -> reference` (push `null`), `primitive -> reference` (box with the
   primitive's wrapper, `box_value_canonical`), and `primitive -> primitive`
   widening (`box_return_against_target`'s `widen_return_value`); and a
   constructor variant that carries the new object as a GC root and pushes it
   on the `void` return of `<init>` (the lambda door's constructor reference,
   which wave 26 pins, needs the same).
2. Let the leaf arms, in tail position (`MH_TAIL` already computes it), when
   the door's conversion is one of those, declare the door transparent AND
   record the adapter for the leaf frame's depth if the nested call yields
   (the lambda door's `note_continuation_return_boundary` discipline: record
   on the way out of a yield, never on a normal return).
3. Keep every adapter with work after its call opaque.

## Expected win and how to measure it

A virtual-thread server whose handlers are reached reflectively and return
`void` / primitives keeps its carrier count at the scheduler's parallelism.
Measure: 1,000 virtual threads each calling `Method.invoke` on a `void`
method that sleeps 100 ms; print wall time and
`ThreadMXBean.getPeakThreadCount`. HotSpot: ~100-200 ms, peak ~= carriers.
CratonVM wave 26: one pinned carrier per sleeping thread. After: HotSpot's
shape. `tools/probes/interp/L4/L4W26VirtualNativePin.java` must keep
printing HotSpot's values, and its `jdk.VirtualThreadPinned` events must
disappear for the method-handle rows.

## Cost and risk

The adapter must reproduce the door's conversion exactly, or a remounted
return pushes a wrongly typed value (the defect class waves 25 and 26 closed).
It touches the interpreter's return arms (lane L7's hot code): the check there
must stay one `is_empty()` test.

## Staged plan

0. (Landed, wave 26.) Transparent doors for the reference identity case.
1. `V -> reference` and `primitive -> reference` for direct handles through
   `invokeExact` (the `--jdk-only` reflection path), with the measurement above.
2. The constructor variant (method-handle `CONSTRUCTOR` arm and the lambda
   door's constructor reference, which then stops pinning).
3. `invoke` / `invokeWithArguments` and the widening conversions.

## Wave 27 note (lane L4) — stage 1 not implemented; what it needs, exactly

Read against the code at `faa212874`; no stage landed, because stage 1 is
neither small nor contained in L4's files:

* **`primitive -> reference` needs no new conversion.** The value-return arms
  already apply `ContinuationReturnAdapter` through
  `interpreter.rs::push_return_across_continuation_boundary`, whose
  `lambda.rs::coerce_return` boxes a primitive callee return for a reference
  caller token with `invoke.rs::box_primitive` (`valueOf`, the same identity
  `box_value_canonical` gives the door's `auto_box_return`). What is missing
  is (a) a way for a `native-builtins` leaf arm to RECORD the adapter -- a new
  `NativeContext` method (say `note_continuation_return_boundary(frame_index,
  caller_return)`, delegating to `interpreter::note_continuation_return_boundary`),
  plus the leaf's frame index (`thread.frames.len()` before the nested call,
  which the context does not expose today); (b) the yield signal at the leaf:
  the nested `ctx.invoke_*` returns the yield as an error value, which the arm
  would have to recognise before its own post-call work runs; and (c)
  `transparent_leaf_call` admitting a primitive leaf when the door's return
  (the call site's, or `invokeExact`'s declared type) is a reference -- which
  `MH_TAIL` does not carry today (it is one bit: "reference result handed back
  unchanged"). (c) means widening `MH_TAIL` to the door's return token.
* **`V -> reference` cannot be done in L4's files.** A `void` callee returns
  through the interpreter's `return` arm, which does not consult
  `continuation_return_adapters` (only the value-return arms do; an adapter
  whose frame leaves by a `void` return is pruned). Pushing `null` there is a
  change to lane L7's hottest arm and must stay behind the existing
  `thread_is_virtual && !continuation_return_adapters.is_empty()` test.
* **Order of work if taken up:** L7 adds the `void`-return consult (cold,
  behind the existing test); L4 adds the `NativeContext` hook, widens
  `MH_TAIL` to a return token, and records in the `STATIC` / `VIRTUAL` /
  `SPECIAL` arms; measure with the 1,000-thread `Method.invoke` benchmark
  above and `L4W26VirtualNativePin.java`.

Wave 27 also changed what an invoker handle's `MH_DESC` holds (the target's
descriptor, `mh_alloc_invoker`); an invoker's leaf is its target's, so the
`MH_TAIL` bit crosses it unchanged and nothing here moves.

## Wave 28 note (lane L4) — stage 1's primitive half landed, with stage 3's widening

Landed (`native-builtins/src/lang_invoke.rs`, `native-api/src/registry.rs`,
`vm/src/vm/vm_exec.rs`):

* **`MH_TAIL` is the door's return token**, not a bit: `b'L'` for any
  reference, a primitive's (or `void`'s) descriptor byte, `0` for none
  (`door_tail_token`). `invoke` arms its call site's return, `invokeExact`
  its call site's when the declared type's agrees, `invokeWithArguments`
  `b'L'`; all still `0` when `return_cast_may_apply`. The re-arming adapter
  arms pass it through unchanged; the invoker arm passes it to its target
  doors only for a reference `t` return under a reference token.
* **The leaf rule** (`tail_leaf_conversion`): a reference leaf under `L`, the
  same primitive, `void` under `V` -- transparent with nothing owed (stage 0,
  plus the primitive and `void` identities, which were right before wave 26);
  a primitive leaf under `L` (boxed with its own wrapper) or under a wider
  primitive (widened) -- transparent, and the conversion is recorded if the
  nested call yields. Everything else stays opaque: a `void` leaf behind a
  value, a reference leaf behind a primitive, a narrowing, a constructor, and
  (when a conversion is owed) a lambda-proxy receiver, whose lambda door
  records its own conversion for the same frame.
* **The hook** the wave-27 note asked for: `NativeThreadAccess::
  continuation_frame_mark` (the frame count, taken before the leaf's call)
  and `note_continuation_return(frame_index, caller_return)` (default no-op;
  the VM's delegates to `interpreter::note_continuation_return_boundary`).
  `transparent_leaf_call` / `transparent_leaf_call_done` wrap the four leaf
  calls (`STATIC`, `SPECIAL`, both `VIRTUAL` arms) and record on the way out
  of a `ContinuationYield`, never on a normal return.

Probe: `tools/probes/interp/L4/L4W28VirtualMhReturnUnmount.java` (values as
HotSpot 25.0.3; the positive control is `CRATONVM_DBG_MH_DISPATCH=1`'s
`[MH_TAIL_ADAPTER]` lines, one per converting row that unmounted) and
`L4W26VirtualNativePin.java`, whose rows 2-6 now unmount and must keep
printing HotSpot's values.

**What remains.** `V -> reference` (the `void` leaf behind `Method.invoke`,
`invokeWithArguments`, an `Object` site -- the proposal's headline Spring
case): the interpreter's `return` arm (lane L7) must consult
`continuation_return_adapters` behind the existing `thread_is_virtual &&
!is_empty()` test and push `null` for an adapter whose `caller_return` is a
reference; `coerce_return` drops a `void` callee's value today (its `None`
arm), so it needs that case too. Then `tail_leaf_conversion` admits `(b'L',
b'V')` with `Some(DESC_OBJECT)`. Stage 2 (constructors) unchanged. The
measurement (1,000 virtual threads, `Method.invoke` of a sleeping method)
applies once the `void` half lands; for the `int` half it is the same
benchmark with an `int` method.
