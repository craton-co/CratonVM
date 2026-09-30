# The interpreter's dispatch loop is 5–9% slower than on wave 23

**Status: open, narrowed — filed 2026-09-27 by the orchestrator of interpreter
round i1 wave 25 from the fat-LTO bisection below. Wave 26's lane L7 commits
measured: they recover the loop (3–16% faster than `dev` on almost every
`--nojit` row). What remains is a fat-LTO code-generation step on the
type-check rows that call through the cast object, which the rest of the wave
switches on and off without touching that path (see "Wave 26 host
measurements"). Wave 27 (lane L7) found the loop-top byte spill's source (the
opcode itself, kept in memory by a diagnostic's borrow) and made the arms
those rows run independent of the unit's inlining decisions; unmeasured (see
"Progress (wave 27)"). Wave 28: JIT round 12's merge measured no slower on
any row; lane L7 split the virtual door and made the constructor rows'
frameless answers engage (see "Progress (wave 28)"), unmeasured. Wave 37
(lane L7): two source changes that take work the unit's inlining decided out
of the per-bytecode and per-return paths -- the loop-top reborrow and a
plain-return head in front of an out-of-line full pop -- unmeasured, with the
exact measurement written down (see "Progress (wave 37)").
Performance only; no wrong answer.**

## Progress (wave 37) — lane L7

The lane cannot build or time. The step this page is left with is a fat-LTO
code-generation knife edge in `execute_frame_from_index`: the same source
moves the type-check and call rows 8-19% depending on what else the unit
contains. The robust answer is to leave the unit fewer decisions on the hot
paths, so two commits remove one per-bytecode computation and one inlining
decision; each is independent and revertable.

### `45d214692` — the match takes its frame from `hot_fp`

The loop top already computes `hot_fp = thread.frames.frame_ptr(frame_idx)`
for its preamble; the match then took `&mut thread.frames[frame_idx]` again,
which through `FrameStack`'s `IndexMut` is two bounds checks (the live-prefix
slice, then the index) and an `imul` by `size_of::<Frame>()` (240) on every
bytecode that reaches the match -- the "`hot_fp` reborrow at the loop top"
of "What remains" below, whose base load was the loop's heaviest single
instruction in the wave-26 profile. `FrameStack::frame_mut_from_ptr` (now
lane L7's file) turns the hoisted pointer back into a `&mut Frame` that
borrows the whole stack exactly as the indexed borrow did, so every arm's
`let _ = frame;` discipline keeps its compile-time check. The two cold hooks
that invalidate the pointer re-derive it out of line
(`rederive_hot_frame_ptr`): the JVMTI single-step hook (an indexed shared
reborrow of the stack) and the breakpoint park (a debugger's method
invocation runs Java on this thread and can push, i.e. move, frames).

### `75000901b` — a plain return does not call the full pop

Every interpreted return called `pop_and_recycle_frame_with_reason`, a
~200-line function with no inline attribute, from three sites in the loop
(value return, void return, the decoded `Return` arm) -- whether it was
inlined there was the unit's choice. Its plain case (no held block monitor,
no door monitor, no `FramePop` request, no loop work to credit, no frame
trace, no invoke-phase census) only bumps the caller's `exec_epoch` and
retires the frame. `pop_and_recycle_frame` now tests those gates (the full
function's own, in its order) and does the two things inline; the full
function is `#[inline(never)]`. A new duty added to the full function must
add its gate to the head (said at both).

Also on the call path this wave (the contiguous-stack page's lane; listed
here because they move the same rows): `c9e785452` keeps a retired slot's
code `Arc` when it is rebuilt for the same method (two locked
read-modify-writes fewer per recursive or same-callee call), and stage 2 of
the contiguous stack adds one bit test per value return with its switch off.

### Exactly what to measure

Fat LTO (`CARGO_TARGET_DIR=target-lto cargo build --release -p cratonvm-cli
--bin cratonvm`), `--nojit`, five builds: the base `54182e717`, then each of
`c9e785452`, `45d214692`, `75000901b` cumulatively, and the lane head.
Interleaved, pinned (`taskset -c 5`, then the same run on core 3), 5 rounds,
medians, in ONE run per core (the floor between runs is 8-40%):

* `TypeCheckBench` all rows (`classMono`, `ifaceMono`, `classPoly`,
  `castCall`, `arrObject`, `ladder3`, `classNegPoly`);
* `L7W27DispatchLayoutBench`, `BackEdgeCounterBench` (per-bytecode rows: the
  reborrow is on every bytecode, the return head on none);
* `InvokeDoorCostBench` (`static-call`, `private-same`, `super-call`,
  `virtual-mono`, `ctor`), `L7W28VirtualDoorSplitBench`,
  `L7W29ContiguousStackBench` (call and return rows: all three commits).

Expected: every row flat or down; the per-bytecode rows move only with
`45d214692`, the call rows with all three. Then `perf record` (timer
sampling) of `TypeCheckBench` on the base and the head, and compare
`perf report --sort symbol` self time (the wave-32 lesson: diff the
per-symbol profiles before reading annotations) --
`pop_and_recycle_frame_with_reason` should vanish from the head's profile on
these benches, and `perf annotate execute_frame_from_index` should show no
`imul $0xf0` between the opcode byte loads and the match's jump. A commit
that moves any row UP beyond the floor in both cores' runs is reverted on its
own. `perf stat` for the indirect-branch question still needs a host with a
PMU (below).

### What remains

Unchanged below except that the `hot_fp` reborrow item is done: measure; the
`perf stat` comparison on a host with a PMU; the unfused `*aload` general
path. Close this page only on the orchestrator's measurement of the
type-check rows against wave 23.

## Progress (wave 28) — lane L7

**JIT round 12 cost the loop nothing measurable.** The orchestrator's fat-LTO
interleaved `--nojit` A/B (6 runs over 2 cores, medians), the wave-27 landing
`8e837ef15` against `dev` after JIT round 12 (`d8a690353`): no row slower
beyond the floor; `TypeCheckBench` 4-11% faster (`arrNeg` -11.5%, `arrPoly`
-9%, `ladder3` +3.1%), `InvokeDoorCostBench` `ctor` -8.4%, `private-same`
-8.2%, `L4W25FramelessDoorBench` `new-default` -12.2%, `new-field-init`
-11.0%, `new-object` -6.3%, `empty-virtual` +4.4%, the array and
dispatch-layout rows ±3%. So the wave-27 "`ctor` / `new-field-init` about 8%
slower" reading did not persist on the new base.

The code audit agrees. What round 12 put on the interpreter's hot paths:

| where | runs | cost |
|---|---|---|
| `record_branch_for_frame`, `record_backedge_for_frame`, `record_loop_exit_for_frame`: `runs_obsolete_method()` first | per recorded branch (profile armed / PGO) | one bit test on the frame |
| `goto_arm!`: `offset > 0 && pgo_enabled` → `record_loop_exit_for_frame` | every forward `goto` | one compare on a register-held offset and a hoisted bool |
| the receiver / call-site profile records in the doors and `execute_invokestatic_cached` / `execute_invoke_kind`: `&& !runs_obsolete_method()`, and `record_call_site_borrowed(.., code.len())` | per call, only while receiver profiling is armed (JIT on) | one bit test, one length load |
| `execute_invokevirtual_vtable_fast`: `vtable_shadows_a_signature` under the read guard it already took | inline-cache miss path only | one `is_empty` |
| `execute`: the eager first-call door routes, `note_door_rerun_of_method` | uncached first call / a compiled body's deopt or stash outcome | nothing on a cached call |
| `jit_bridge` door outcomes (`note_door_rerun_from_entry`, `#[cold]`) | a compiled body's `Deopt` / trap outcome | nothing on a normal return |
| `gc_and_alloc`: `wake_lazy_parkers` | per pause | nothing per allocation |

Lane L7 made the two recorders it touched free of refcount traffic (commit
"profile recorders take no Arc clone per record"): `record_branch_for_frame`
records through its memo's handle instead of cloning the `BranchCounters`
`Arc` out and dropping it (two atomic RMWs per recorded branch while a
nominated method runs interpreted with the JIT on), round 12's
`loop_extents_for_frame` became `with_loop_extents_for_frame` (the same, per
PGO back edge and loop exit), and `record_loop_exit_for_frame` is `#[cold]`
like its sibling, since round 12 added a call to it in `goto_arm!`.

**The virtual door split** (this page's third "What remains" item) landed:
see `i27-L7-proposal-split-the-virtual-fast-door-into-a-hit-and-out-of-line-tails-20260928.md`,
"Progress (wave 28)", and `L7W28VirtualDoorSplitBench`.

**The constructor rows**: `new-default` / `new-sub-default` never had the
wave-25 frameless trivial constructor (it never engaged; see
`i24-L4-proposal-frameless-trivial-constructor-in-the-special-door-20260927.md`,
"Progress (wave 28)"), and `ctor` / `new-field-init` are field-store
constructors, answered without a frame since this wave. Expect those rows to
drop well beyond the floor; `L7W28FramelessCtorBench`.

**What remains** (unchanged from wave 27 except as above): measure; the
`perf stat` comparison on a host with a PMU; the `hot_fp` reborrow at the loop
top; the unfused `*aload` general path. Close this page only on the
orchestrator's measurement of the type-check rows against wave 23.

## Progress (wave 27) — lane L7

The lane cannot build or time. Eight code commits on `faa212874`, each
bisectable, none changing behaviour; the timing probe is
`tools/probes/interp/L7/L7W27DispatchLayoutBench.java` (rows and expected
directions in its header), next to the page's own rows.

### The spilled byte is the opcode, and a borrow put it there

`mov %cl,0x347(%rsp)` (fast build) / `mov %r14b,0x14f(%rsp)` (slow build)
store the register the opcode was just loaded into (`movzbl
(%r14,%rbx,1),%ecx`), and the slow build's later `cmpb $0x32,0x14f(%rsp)` /
`cmpb $0xb4,0x14f(%rsp)` are the `*aload` and `getfield` arms re-testing it;
the fast build also has `lea 0x347(%rsp),%rax`: the slot's ADDRESS is taken.
No hoisted bool is involved. The value-return arm's operand-stack-underflow
diagnostic did `eprintln!("... opcode=0x{:x}", ..., saved_pc, opcode)`, and a
`format_args!` argument is a borrow. rustc gives any borrowed local a stack
slot for the whole function instead of an SSA value, and because the pointer
escapes into the formatter LLVM cannot promote it back, so every bytecode
stored its opcode there and the arms that test `opcode` again read memory.
`saved_pc` had the same problem through five format sites on the decoded path
(`0x50(%rsp)`, reloaded at every loop top in both profiles).

`d0ca42cec` moves every such diagnostic out of line with the values passed
BY VALUE (`value_return_underflow`, `decoded_frame_out_of_range`,
`no_instruction_decoded`, `decoded_error_tripwires`), makes the quickened
`resolve` closure `move`, formats a copy in `trace!`, and turns
`(0x2e..=0x35).contains(&b1)` into `matches!`. The rule is written on
`value_return_underflow`: nothing in `execute_frame_from_index` may borrow
`saved_pc`, `opcode`, `b1` or `b2`. `af61a55e6` applies it to `value` in the
same arm (the `CRATONVM_TRACE_SB_FILTER` block formatted it; `&Some(value)`
was built before the MethodExit listener test on every return).

`frame_idx` is address-taken too, by design (`&mut frame_idx` goes to
`unwind_to_handler`, `try_osr_with_backoff`), and stays in memory
(`0x70(%rsp)`); returning the new index instead is a larger change for one
load that is store-forwarded anyway.

### What `classMono` / `ifaceMono` run, and what changed on each arm

Per iteration (javac): `iload_2; ldc 1000000; if_icmpge; aload_0; iload_2;
iconst_1; iand; aaload; astore_3; aload_3; instanceof; ifeq; iload_1;
aload_3; checkcast; invokevirtual|invokeinterface; [callee: iconst_1;
ireturn]; iadd; istore_1; iinc+goto` — two frame switches (call, return).

| arm / call | before | wave 27 |
|---|---|---|
| every bytecode | opcode stored to a stack slot, `saved_pc` reloaded | both SSA (`d0ca42cec`) |
| `ldc` (loop bound) | call into a ~470-line `execute_ldc` for a per-thread cache hit | `execute_ldc` is the probe, the resolver is `execute_ldc_resolve`, both `#[inline(never)]` (`df4c07192`) |
| `instanceof`, `checkcast` | shared arm with getstatic/putstatic/new/anewarray, then a second `match opcode` | own arms (`68ab0d7f0`) |
| `op_instanceof`, `op_checkcast` | `#[inline]`, 200 / 700 lines, a memo hit sharing frame and inlining budget with resolution and CCE forensics | probe + hit only, `#[inline(never)]`; the rest moved verbatim to `instanceof_full_path` / `checkcast_full_path` (`856aec764`) |
| `aload_N`, `astore_N`, `areturn`, reference fallbacks | `inline(always)` helpers with the validated-`L` coerce (and its heap probe) inlined at eight sites | cold halves `#[cold] #[inline(never)]` (`8be2ae163`) |
| `ireturn` | `value` in memory (trace borrow), `Some(value)` built for MethodExit | neither (`af61a55e6`) |
| `ifeq`, `if_icmpge`, `iinc+goto` (18 + 2 macro expansions) | `record_branch_for_frame`, the PGO back-edge record and `try_osr_with_backoff` (with `try_osr_offer`, its only caller) inlinable at every expansion | `#[inline(never)]` / one cold call (`61a8c8296`) |
| `getfield` / `putfield` after a quickened decline | `op_getfield` / `op_putfield` `#[inline]`, 360 / 420 lines | `#[inline(never)]` (`87e5cdcb3`) |
| `invokevirtual` / `invokeinterface` door | `#[inline]`, ~800 lines, lane L4's file | not edited; see `i27-L7-proposal-split-the-virtual-fast-door-into-a-hit-and-out-of-line-tails-20260928.md` |

Rows each commit should move are in its message; all expected flat or down.
If `856aec764` steps the type-check rows UP, fat LTO used to inline all of
`op_instanceof` into the loop: drop only its two `#[inline(never)]` on
`op_instanceof` / `op_checkcast` and keep the split.

### A measurement that would settle the "layout step"

The step is 45 ns per `classMono` iteration (240 → 285), about 150-200
cycles for roughly 25 dispatches and two calls. A spill or two cannot make
that; the single indirect `jmp *%rax` whose predictor history holds every
opcode transition can, if a layout change costs it a handful of
mispredictions per iteration. `perf stat -e
cycles,instructions,branch-misses,br_misp_retired.indirect` (the last on
Intel; `-e branch-misses` alone elsewhere) of `TypeCheckBench` on the fast
(L7 alone) and the slow (whole wave-26) fat-LTO builds tells the two apart:
equal instruction counts with ~5+ more indirect misses per iteration on the
slow build means the dispatch site, not the code in the arms, and then the
next step is Stage 0/6 of
`i1-L1-proposal-dispatch-loop-register-state-20260923.md` (replicated
dispatch for the hot transitions), not more outlining.

### What remains (after wave 27)

* Measure the eight commits on fat LTO against `faa212874` and against
  L7-alone of wave 26, with the pinned 5-round protocol; revert any that does
  not pay (each is independent).
* The `perf stat` comparison above, on the fast and slow builds of wave 26
  and on this lane's build.
* The virtual/interface fast door's split (lane L4's file; the proposal page
  above says exactly where to cut).
* `let frame = &mut thread.frames[frame_idx]` after the loop top recomputes
  what `hot_fp` already holds (a second bounds check and `imul`, and the
  profile's heaviest single instruction, `add 0x1e8(%rax),%r9`, 7.3%, is its
  base load). A lifetime-bound `FrameStack` accessor that reborrows `hot_fp`
  under `&mut self` would keep the borrow checker's `let _ = frame;`
  discipline; `FrameStack` is lane L3's file, and the loop-top hooks that can
  run Java (`deliver_breakpoint_if_set`, `fire_jvmti_single_step`) must
  re-derive it. See the Wave 27 note on
  `i1-L1-proposal-dispatch-loop-register-state-20260923.md`.
* The unfused `*aload` arm's general path (the decoded operands,
  `get_array_element`, the AIOOBE) is inline; it is the ONLY path when
  `fast_field_zgc` is `None` (a non-ZGC heap), so it was left alone rather
  than marked cold.

## Progress (wave 26) — lane L7

The lane cannot build or time; the four changes below are separate commits
so a fat-LTO A/B can bisect them. None changes behaviour.

### (a) What waves 24–25 added to `execute_frame_from_index`'s hot paths

`git diff 175faad5f 4a816af32` over the interpreter, `frame.rs` and
`quickened.rs`, sorted by how often it runs:

| added in | where | runs | cost |
|---|---|---|---|
| w24 L3 | loop top: `code_moves` compare moved out of the gate's `\|\|` into its own branch, with a `conversion_pending` test, a retry call and a `continue` in its body; the snapshot starts one behind | every bytecode | same load count as wave 23 (the `code_moves` load was already per bytecode), but a separate compare-and-branch plus a larger inline body at the loop top |
| w24 L7 | `aload_N` arm: `aload_N; iload_M; <x>aload` test | every `aload_0..3` | two range compares on bytes already loaded; a match inlined `array_load_prim` + `array_load_ref` |
| w25 L7 | `iload_N` and `iload n` arms: `iload; <x>aload` test | every `iload` | one range compare; a match inlined `array_load_top_with_index` (both readers again) |
| w25 L7 | both value-return arms: `continuation_return_adapters.is_empty()` | every interpreted value return | one load from the `JvmThread` (a line the return path touches for nothing else) + branch |
| w24 L3 | gate refresh: `gate_saw_move` swap and the `moved` walk of the frames below | every frame switch (call, return) | a stack-slot load/store and a predicted branch; the walk runs only after a move |
| w24 | loop-top hooks: `ThreadReference.Stop` result | only with `loop_top_hooks` (debugger / pair census) | nothing unarmed |
| w25 L3 | `quickened_for_frame`: `replaced_body()` instead of `widened_stream()` | decoded-path frame switch only | nothing on the fast path |
| w24/25 L2 | `held_monitors` rewrites in `try_osr_offer`, `deopt_resume`, `jit_bridge` | OSR / deopt frame rebuilds | not on any `--nojit` path; the per-call `held_monitors` work (`reset_cached_tail`'s `clear`, the return arms' `is_empty`) is wave 23's and unchanged |
| w24/25 L4/L5 | `invoke_fast.rs` / `dispatch_static.rs`: `slots` hoisted into the door caller, the trivial-constructor shape test (`is_special` only), the recorded owner failure (resolution slow path only) | per call through the non-virtual door | `is_special &&` short-circuits for `invokestatic`; the 144-byte `empty_arg_slots()` fill per door call predates wave 23 |

So no single per-bytecode LOAD was added: what grew is the loop top's code
(a second compare-and-branch with its own inline body) and the loop's size
(about six inlined copies of the array readers), which fits the page's
layout/register-allocation hypothesis and the no-LTO build not showing it.

### (b) The commits, and what each should move (`--nojit`, fat LTO)

1. `069cbb656` — **one per-thread poll word** (stages 1-2 of
   `i24-L3-proposal-one-per-thread-poll-word-for-safepoints-and-frame-moves-20260927.md`):
   the `stw_requested` load and the `code_moves` compare become one relaxed
   load of the OS thread's `threading::gc_barrier::LoopPollWord` and one
   compare against a register; the STW handling, the move-count compare, the
   conversion retry and the gate refresh moved into the `#[cold]`
   `loop_top_poll`. The barrier counts its pause in every registered word in
   the same critical section that raises the flag, and uncounts it in the
   one that lowers it; a frame move bumps the word of the OS thread it runs
   on; the slow path re-reads the flag before it parks and compares the
   stack's own move count. Expected: every row
   down a little (`classMono`, `ifaceMono`, `arrObject`, `ladder3`,
   `L3W23GateBench` `arith` most). Correctness gate: the four collectors'
   STW tests, and `tools/probes/interp/L7/L7W26PollWordSafepointBench.java`
   (`gc max ms` must not step up).
2. `3c97c93bd` — value returns test a hoisted `thread_is_virtual` before
   `continuation_return_adapters` (every yield producer requires a virtual
   thread). Expected: call rows (`static-call`, `classMono`) flat to
   slightly down.
3. `580c3b123` — the fused element reads call their array readers out of
   line (`field_fast::array_load_fused`, `array_load_top_with_index`
   `#[inline(never)]`); the unfused `*aload` arm keeps its inlined copy.
   Expected: the non-array rows down or flat; `L7W24ArrayElementBench` /
   `L7W25StackedElementBench` fused rows up by at most a call. Revert it if
   only the latter moves.
4. `4730377b8` — back edges stop loading the flag: each back-edge site
   `continue`s into the loop top, whose poll word answers the same request
   (the call stays unconditional with the gate off). Expected: one load and
   branch fewer per loop trip — `BackEdgeCounterBench`, every
   `TypeCheckBench` row.

### What remains

* Measure the four commits on fat LTO (interleaved, medians of 3) and
  `perf annotate` `execute_frame_from_index` against `175faad5f`; revert
  any commit that does not pay.
* If the rows are still above wave 23 after them, the remaining suspects
  are the per-frame-switch gate refresh (every call and return:
  `frame_fast_path_admitted`'s memo probe, `replaced_body()`,
  `frame_branch_profile_armed`, `refresh_debugger_gate!`) and the invoke
  doors (lane L4; one concrete per-call waste was filed as
  `i26-L7-every-invoke-door-call-fills-all-nine-argument-slots-20260928.md`,
  removed in wave 27:
  `docs/internal/fixed-bugs/interpreter-L4-every-invoke-door-call-fills-all-nine-argument-slots-FIXED-20260928.md`).
* Older than wave 23 but on the same per-return path: the fast value-return
  arm decodes the returned slot into a `Value` (`decode_arg_kind_aware`)
  on every `ireturn` / `lreturn` / `freturn` / `dreturn`, although only
  `areturn` pushes it (`push_fast_return_value` pushes the raw slot for the
  others); the `Value` is otherwise read only by the `MethodExit` hook (one
  flag load when no agent listens), `trace_sb_filter` and the continuation
  adapter. Building it only on those paths would take a 16-byte enum
  construction (and its spill, since `&Some(value)` is passed out) off
  every primitive return.

## Wave 27 host measurements (orchestrator)

Fat LTO, `--nojit`, pinned to one core, interleaved, medians of 5–6 rounds,
against the wave-26 landing `faa212874` (ns per operation).

| Row | landing | lane L7 alone, run 1 (core 5) | lane L7 alone, run 2 (core 3) | whole wave 27, run 2 |
|---|---|---|---|---|
| `TypeCheckBench` `classMono` | 244–246 | 244 | 265 | 239 |
| `TypeCheckBench` `classPoly` | 272–274 | 266 | 282 | 294 |
| `InvokeDoorCostBench` `ctor` | 276–291 | 295 | 305 | 300 |
| `InvokeDoorCostBench` `static-call` | 115–132 | 117 | 115 | 114 |
| `L7W24ArrayElementBench` `int` | 38–54 | 41 | — | 38 |
| `BackEdgeCounterBench` | 47–54 | 47 | — | 48 |

**The measurement floor on this host.** Within one run the samples of one
binary agree to about 1% (`classMono` of the L7 build: 264.96, 265.12, 265.10,
265.24, 265.01, 266.72). Between runs the SAME binary moved by 8% on
`classMono` (244 → 265) and the landing binary by 40% on the array rows
(53.8 → 38.1), with nothing changed but the pinned core and the time. The
host is a shared 8-vCPU VM whose PMU is not exposed (`perf stat` reports
every hardware event `<not supported>`; `perf record` samples by timer), so
the cause cannot be counted; a busy SMT sibling hurting the layout-sensitive
rows is the likely one. So steps below ~10% on these rows cannot be settled
from one session: compare builds only inside one interleaved run, and repeat
the run on a different core before calling a step.

With that floor: lane L7's wave-27 commits are neutral on the type-check rows
(the loop-top opcode spill is gone from the source; its effect is inside the
floor), and the whole wave is flat except `ctor` / `new-field-init` about 8%
slower in both of its runs, which lane L7 alone also showed in run 2 — so it
is not attributable to lane L4's argument-slot change. What remains of this
page is that the type-check and constructor rows sit on a layout knife-edge;
a measurement that settles steps under 10% needs a quiet machine with
hardware counters.

## Wave 26 host measurements (orchestrator)

Fat-LTO builds (the shipped profile), `--nojit`, pinned to one core
(`taskset -c 5`), five interleaved rounds, medians; ns per operation. The
host carried another session's `rustc` (load 2–4), so steps under ~4% are
noise.

**Lane L7's four commits, each built on `dev` `ebdc885de`** (bisectable, as
the lane asked): every one is at worst neutral, and together (`c4`,
`4730377b8`) they are the win the lane predicted.

| Row | `dev` | L7 `c1` poll word | `c4` all four |
|---|---|---|---|
| `TypeCheckBench` `classMono` | 237 | 246 | 240 |
| `TypeCheckBench` `ladder3` | 161 | 145 | 148 |
| `InvokeDoorCostBench` `private-same` | 200 | 177 | 174 |
| `InvokeDoorCostBench` `ctor` | 315 | 290 | 264 |
| `InvokeDoorCostBench` `static-call` | 124 | 120 | 113 |
| `L7W24ArrayElementBench` `int` | 41.9 | 40.8 | 38.0 |
| `L4W25FramelessDoorBench` `new-default` | 231 | 207 | 202 |
| `BackEdgeCounterBench` (per back edge) | 55.4 | 47.2 | 47.2 |
| `L3W23GateBench` `arith` (ms) | 1948 | 1606 | 1602 |

Commits 1–2 without 3 (`c2`, the hoisted `thread_is_virtual`) cost the `int` / `long`
element rows 19–24% on its own build and nothing once commit 3 (the fused reads out of line) was in; the
page's earlier caution about isolated layout steps applies.

**The merged wave.** With lanes L1–L6 merged as well, the invoke, array and
back-edge rows keep the win, but the type-check rows that CALL through the
cast object step up:

| Row | `dev` | L7 alone | all but L7 and L4 (`d8e6a7db0`) | all but L4 (`ff74be34a`) | whole wave |
|---|---|---|---|---|---|
| `classMono` | 252 | 240 | 276 | 234 | 285 |
| `ifaceMono` | 242 | 239 | 273 | 240 | 292 |
| `classPoly` | 284 | 267 | 314 | 258 | 315 |
| `classNegPoly` (no call) | 99 | 91 | 100 | 100 | 99 |
| `InvokeDoorCostBench` `ctor` | 326 | 292 | 315 | 294 | 279 |

None of lanes L1–L6 touches the `instanceof` / `checkcast` / `invokevirtual`
path these rows run (their interpreter changes are debug-build only, on
resolution misses, or in the deopt and OSR sinks), and the step switches on
without L7, off with L7, and on again with L4, whose interpreter change is
one arm of the constructor-reference door. So it is a code-generation
artefact of the one fat-LTO codegen unit (inlining and register allocation in
`execute_frame_from_index`), not work on the path. Forcing the per-reference
validation helpers inline (`HeapBitmap::contains`, `site_stats::bump`,
`object_ref_payload_is_known`, which the slow build's profile shows out of
line) did not remove it (`32a009d51`, reverted). Against wave 25's landing
the type-check rows are about flat (`classMono` 267 on wave 25's own fat-LTO
build).

`perf annotate` of `execute_frame_from_index` on both builds is kept on the
host (`/data/wt-interp-w22-logs/perf26/lto-l7c4.annot`, `lto-w26.annot`,
with the `perf record` data). Both spill a byte-sized value to the stack at
the loop top on every iteration (`mov %cl,0x347(%rsp)` / `mov
%r14b,0x14f(%rsp)`, 4.8% / 6.0% of the loop's samples): a flag the loop
keeps live across the whole match. Keeping it in a register, or not needing
it per iteration, is the first thing to try.

**Next step.** Make the invoke arms that the type-check rows reach robust to
the unit's layout: find the loop-top byte spill's source variable, move the
virtual/interface fast door's cold tails out of the loop's inlining budget
(`#[cold]` / `#[inline(never)]` helpers for its refusal and fill paths), and
measure with the same pinned 5-round protocol against `dev` and against L7
alone.

## What was measured

Timing probes run `--nojit` on fat-LTO builds (the shipped profile, not the
no-LTO build the per-wave probes use), interleaved, medians of 3, on the
shared 8-core Linux host. Stdout was identical across builds.

| Row | wave 22 `0c410b878` | `dev` `9e252c8b2` | wave 23 `175faad5f` | wave 24 `005dd305d` | wave 25 (fixed) `1055df23b` |
|---|---|---|---|---|---|
| `TypeCheckBench` `classMono` | 240 | 279 | 241 | 265 | 267 |
| `TypeCheckBench` `ifaceMono` | 241 | 288 | 244 | 264 | 264 |
| `TypeCheckBench` `arrObject` | 147 | 167 | 165 | 170 | 182 |
| `TypeCheckBench` `ladder3` | 139 | 160 | 153 | 162 | 166 |
| `InvokeDoorCostBench` `static-call` | 107 | 116 | 118 | 119 | 128 |
| `InvokeDoorCostBench` `ctor` | 258 | 280 | 290 | 344 | 319 |

(ns per operation. The wave-25 column comes from a second run, interleaved with
a wave-23 control that read `classMono` 247, `ctor` 294.)

Three separate steps:

1. **`dev` between the wave-22 and wave-23 landings (`9e252c8b2`: JIT round 11
   waves 16–19 and a gc round) cost 5–18% on almost every row.** It is not
   interpreter-round work. Wave 23 won most of it back for the type-check and
   invoke rows (`classMono` 279 → 241).
2. **Wave 24 cost 5–25%.** Most of it, on `invokespecial` and `new`, was the
   non-virtual door's 144-byte `ArgSlots`, which was copied twice per call once
   the door was split into prelude and finish. That is fixed in wave 25
   (`1055df23b`): `private-same` 211 → 183 (wave 23: 183), `super-call` 150 →
   135 (135), `new-sub-default` 346 → 311. What is left is spread over the
   dispatch loop: `perf` of `TypeCheckBench` on the two builds gives the same
   profile, except that `execute_frame_from_index` itself rises from 42.4% to
   44.3% of the samples. No other function moves by more than 0.4 points.
3. **Wave 25 moved `static-call` (+8%) and the array casts (+7%)** and nothing
   else outside noise.

## What to look at

* The dispatch loop's per-bytecode and per-frame work added in waves 24–25:
  - the deferred-conversion retry at the loop top (`conversion_pending`, wave
    24 lane L3);
  - the `FrameStack::code_moves` gate refresh that marks the frames below
    (wave 24 lane L3);
  - the `aload/iload/xaload` superinstruction arms (wave 24) and the stacked
    `iload; xaload` fusion (wave 25, lane L7);
  - the continuation-return-adapter test on every value return (wave 25, lane
    L7);
  - the `held_monitors` bookkeeping on frame rebuild (waves 24–25, lane L2).
  Each is one load or one branch. Together they may push the loop's hot
  arms over a layout or register-allocation edge in the LTO build, which is
  also why a no-LTO build does not show the same step (see the retired
  wave-22 page on the no-LTO layout artefact).
* Method: fat-LTO builds of each wave-24 lane commit in isolation (the
  bisection above only has wave granularity), then `perf annotate` of
  `execute_frame_from_index` on the first lane that moves `classMono`.

## Reproduce

```sh
# on the Linux host, fat LTO (the default release profile)
CARGO_TARGET_DIR=target-lto cargo build --release -p cratonvm-cli --bin cratonvm
javac -d /tmp/tc tools/probes/interp/L2/TypeCheckBench.java
target-lto/release/cratonvm --nojit -cp /tmp/tc TypeCheckBench
```

## Wave 28 host measurement (orchestrator)

Fat LTO, `--nojit`, interleaved `dev` `d8a690353` (JIT round 12 merged) →
the merged wave 28 → the same without `#[inline(never)]` on
`execute_invokevirtual_fast_door` (`e94f84385` reverted).

* **Before the wave.** JIT round 12 did not slow the loop: the wave-27 landing
  → `d8a690353` moved every row −12% … +4% (6 samples over 2 cores).
* **First run, clean core 5** (core 3 had a busy neighbour: `dev` samples of
  121 → 377 ns within one row), 3 samples each:
  - the constructor rows fell: `ctor` 272 → 194 ns, `new-default` 207 → 139 ns
    (wave 25's trivial-constructor elision engages for the first time, and
    field-store constructors are frameless);
  - the virtual-call rows were flat;
  - `TypeCheckBench` `classMono` went 218 → 237–260 ns and `castCall`
    218 → 237 ns;
  - `new-long-param` rose 204 → 236 ns, a real per-call cost that L7b
    removed.
* **Second run**, the final head, under another session's Spring suite (load
  average 9–17; absolute times about 1.7× the first run):
  - the constructor rows are −20 … −32% again;
  - `new-long-param` is about +9% in the cleanest samples;
  - `classMono` and `castCall` are indistinguishable from `dev`;
  - the build without `#[inline(never)]` is no better on the type-check rows,
    so the attribute stays.

Lane L7b found no mechanism in the `checkcast` / `instanceof` arms: nothing
in wave 28 touches `typecheck.rs` or those arms. The rows stay the page's
knife-edge. The next step, if anyone takes it, is `perf record` (timer
sampling) of `classMono` on the two builds on an idle host.

