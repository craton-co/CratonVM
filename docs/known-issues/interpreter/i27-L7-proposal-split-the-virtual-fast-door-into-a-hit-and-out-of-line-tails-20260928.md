# Proposal: split the virtual/interface fast door into a hit and out-of-line tails

**Status: open, narrowed — filed 2026-09-28 by interpreter round i1 wave 27,
lane L7, for lane L4 (the door's owner). Wave 28 (lane L7) landed stages 1-2
of the staged plan for the VIRTUAL door (all four design steps), unmeasured;
stage 3 (the non-virtual and static doors) remains. Performance only.**

## Progress (wave 28) — lane L7

Three commits on `d8a690353`, each bisectable, none changing behaviour
(every census reason string, hand-over, count and the order of operations
inside each moved block are the old ones):

1. **Steps 2-4** (commit "the virtual fast door's declines, tier-up and
   compiled call out of line", `vm/src/runtime/interpreter/dispatch_virtual.rs`):
   * `door_decline_at_probe` (`#[cold]`, `#[inline(never)]`): the two
     declines at the probe (`DoorProbe::Found(other.clone())` / `Miss`);
   * `door_decline_late` / `door_decline_late_owned` (`#[cold]`,
     `#[inline(never)]`): the fifteen late declines. The borrowed variant
     clones for the hand-over as the sites did; the owned one moves the entry
     taken by `callee.into_owned()`, `after_inline_attempt` passed through
     (only the contended-monitor site passes `inline_attempted`);
   * `virtual_door_tier_up` (`#[inline(never)]`) returning
     `VirtualDoorTierUp::{Interpret, Compiled, Decline, DeclineUnprobed}`,
     called under the block's unchanged condition, so `--nojit` never
     enters it; `poly_gate` and `inline_attempted` threaded by `&mut`;
   * `virtual_door_call_compiled` (`#[inline(never)]`): the compiled call
     with its 16-`Value` buffer, off the door's frame.
2. **Step 1** (commit "the virtual fast door is never inlined into the
   dispatch loop"): `#[inline(never)]` on `execute_invokevirtual_fast_door`,
   separate so the host A/B can drop it alone.
3. Timing probe `tools/probes/interp/L7/L7W28VirtualDoorSplitBench.java`:
   `virtualMono`, `ifaceMono`, `virtualPoly`, `syncMono` (framed door calls,
   expected down a few ns with commit 1, flat with commit 2), `getterMono`,
   `emptyMono` (frameless, flat or down), `staticCall` (control, flat);
   also with the JIT on (must not move beyond the floor).

What remains: measure (the rows above and this page's own list, fat LTO,
interleaved, against the parent of commit 1 and commit 1 against commit 2),
then stage 3 below if the virtual door paid. For stage 3, note that the
non-virtual door's tier-up block is out of line already (`door_tier_up`,
wave 22) and the static door is short (its JIT half is
`callee_has_compiled_body` + `note_invocation_for_tierup`, both `#[inline]`,
under `!disable_jit()`); the candidate there is only that JIT half.

## Problem, with evidence

`execute_invokevirtual_fast_door`
(`vm/src/runtime/interpreter/dispatch_virtual.rs`, `#[inline]`, ~800 lines)
is called from two arms of `execute_frame_from_index` (`0xb6`
`invokevirtual`, `0xb9` `invokeinterface`) on every virtual call. A warm
monomorphic call to an interpreted callee -- nearly every call it serves --
runs: the redefinition latch test, the inline-cache probe, the
synchronized / static / stack-depth / receiver checks, the proxy, interface
memo, intercept-shape, force-native and descriptor-facts tests,
`read_args_verbatim`, the empty-body test, and `push_frame_verbatim`. Under
`--nojit` the tier-up block is skipped by its first test.

Everything else is in the same function:

* about fifteen late-decline tails, each `*door_probe =
  late_decline_probe(receiver_class_id, Arc::clone(cached), ...);
  note_virtual_decline(...); return None;` -- an `Arc` clone and an enum
  build inline at every site;
* the tier-up block (`if gate_generation == 0 && !disable_jit() &&
  jit_virtual_tierup()`: the native-call-site memo, the `java/util/` bitmap,
  the promotion census, the JIT cache read, the door counter, the background
  offer, `try_jit_upgrade_with_gate`), about 180 lines;
* the compiled-call block (`if let Some(compiled) = compiled_call`), which
  declares `let mut args_buf = [Value::Uninitialized; 16]` -- 256 bytes of the
  door's own stack frame -- plus `execute_jit_call_decoded`, the synchronized
  re-run and `install_cached_frame`.

So every interpreted virtual call pays the prologue, the stack frame and the
callee-saved register traffic of a function sized for its rarest paths, and
whether fat LTO inlines this function into the dispatch loop (twice), or the
door's small helpers into it, moves with the size of unrelated code. That is
the mechanism the wave-26 host measurements on
`i25-L7-the-interpreter-dispatch-loop-is-slower-than-on-wave-23-20260927.md`
point at: `TypeCheckBench`'s `classMono` / `ifaceMono` / `classPoly` (a
virtual or interface call per iteration) stepped 10-20% up and down as
unrelated lanes merged, while `classNegPoly` (no call) did not move; the slow
build's profile showed `HeapBitmap::contains` (this door's
`is_object_address` receiver check) out of line.

Wave 27 did the same split for the other functions those rows call
(`op_checkcast` / `op_instanceof`, `execute_ldc`, the reference-local
helpers, the back-edge recorders; see that page's "Progress (wave 27)").
This one is lane L4's file, and L4 is reworking `ArgSlots` in the doors this
wave, so it is proposed, not done.

## Design

Four mechanical steps, each behaviour-preserving and separately measurable:

1. **`#[inline(never)]` on `execute_invokevirtual_fast_door`** (it is
   `#[inline]`). The loop's two arms are then the same call in every build.
2. **One cold decline tail.**
   ```rust
   #[cold]
   #[inline(never)]
   fn decline_late(
       door_probe: &mut DoorProbe,
       receiver_class_id: ClassId,
       cached: &Arc<CachedBytecodeMethod>,
       poly_gate: Option<RedefineGate>,
       gate_generation: u32,
       after_inline_attempt: bool,
       why: &'static str,
   ) -> Option<Result<CachedCallResult, MethodCallFailed>> {
       *door_probe = late_decline_probe(
           receiver_class_id, Arc::clone(cached), poly_gate, gate_generation,
           after_inline_attempt,
       );
       invoke_fast::note_virtual_decline(why);
       None
   }
   ```
   and every late decline becomes `return decline_late(door_probe, ...)`.
   The early declines (`DoorProbe::Found` / `Miss`) can share a second cold
   helper. Where a site moves `cached` (after `callee.into_owned()`), pass a
   reference and let the helper clone, as the pre-commit sites already do.
3. **The tier-up block out of line**: `#[inline(never)] fn
   door_tier_up(shared, thread, cached: &Arc<CachedBytecodeMethod>,
   receiver_class_id, poly_gate: &mut Option<RedefineGate>, gate_generation,
   caller_class_id, cp_index, site_pc, verbatim) -> TierUp`, with `enum TierUp
   { Interpret, Compiled(RetainedCode), Decline(&'static str),
   DeclineUnprobed(&'static str) }` covering its three `return None` shapes
   (the bitmap with no answer, an argument slot needing coercion, a due
   inline tier-up without a gate) and `inline_attempted`. The door calls it
   only under the block's existing condition, so `--nojit` never enters it.
4. **The compiled call out of line**: `#[inline(never)] fn
   door_call_compiled(shared, thread, frame_idx, cached, compiled, total_args,
   site_pc, actual_class_id) -> Option<Result<CachedCallResult,
   MethodCallFailed>>`, moving `args_buf` (256 bytes) off the door's frame.

What stays in the door is its hit path: the probe, the checks, the getter and
empty-body answers, `read_args_verbatim`, the monitor acquire and
`push_frame_verbatim`.

The same four steps apply to `execute_nonvirtual_fast_door` /
`nonvirtual_door_prelude` / `nonvirtual_door_finish` and
`execute_invokestatic_fast_door` (`invoke_fast.rs`, `dispatch_static.rs`);
measure the virtual door first.

## Expected win and how to measure it

A smaller frame and fewer saved registers per virtual call, and the loop's
virtual arms fixed as a call: a few ns per call, and (the point) no step when
unrelated code changes size. Rows, `--nojit`, fat LTO, interleaved, pinned,
5 rounds, medians, against the build before the change AND against a build
with one unrelated lane merged on top (the wave-26 bisection's shape):
`TypeCheckBench` `classMono` / `ifaceMono` / `classPoly` / `ifacePoly`;
`InvokeDoorCostBench` `virtual-mono` / `empty-virtual` / `empty-iface`;
`L7W27DispatchLayoutBench` `castCall`; `L4W25FramelessDoorBench`. With the
JIT on, the tier-up rows of `InvokeDoorCostBench` must not move (step 3 adds
one call on a path that already calls `try_jit_upgrade_with_gate`). `perf
annotate` of the door: the prologue should no longer save six registers and
reserve a frame larger than a page (no stack probe).

## Cost and risk

Mechanical moves with no change of order: low risk, like wave 27's
`checkcast_full_path`. The one subtlety is `callee` (`Cow` of the primary
entry, borrowed from `thread.invoke_cache`): the tier-up helper must receive
the owned `Arc` the door already takes before that block
(`callee.into_owned()`), and must not hold it across the `&mut thread` calls
any more than the block does today. The `DoorProbe` hand-over contracts
(`late_decline_probe`'s `after_inline_attempt`) must be passed through
unchanged, and the decline reasons (`note_virtual_decline`'s
`[invoke-door]` census keys on them) kept byte-identical.

## Staged plan

1. Steps 1 and 2 (one commit each), measured.
2. Steps 3 and 4, measured, JIT-on rows included.
3. The non-virtual and static doors, if 1-2 paid.
