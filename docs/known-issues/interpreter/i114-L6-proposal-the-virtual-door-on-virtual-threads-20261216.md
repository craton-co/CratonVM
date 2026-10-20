# i114-L6 proposal: the virtual invoke door and the sig-poly site cache on JDK virtual threads

Interpreter round i5, wave 114, lane L6. Status: proposal, not built.

## Background

`invoke_fast_door_on` is false on every virtual thread (`interpreter.rs`,
`execute_frame_from_index`). Wave 114 added `CRATONVM_VTHREAD_NONVIRTUAL_DOORS` (default off),
which lets the static and special doors run on a mounted JDK virtual thread. Their tier-up blocks
then follow the general arms' virtual-thread answer (`invoke_fast::vthread_door_gate`). Page:
`i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled`, "Wave 114".

Three paths still stay off on a virtual thread:

* the virtual / interface door (`execute_invokevirtual_fast_door`);
* the lambda door's compiled arm (it serves interpreted, and `lambda_door_compiled_body` refuses
  `Virtual`);
* the signature-polymorphic site cache (`sig_poly_site`).

So every `invokevirtual` and `invokeinterface` of a virtual thread takes
`execute_invokevirtual_cached_probed`. On a platform thread a warm monomorphic site is a class-id
compare, an argument check and a frame push. On a request handler written against interfaces, that
is most of its calls.

## What a change needs

1. **Admission.** Add a `DOOR_INVOKE_FAST` arm for JDK virtual threads, as
   `nonvirtual_fast_door_on` now has. `door_gates` is a full `u8`, so either widen it (the
   dispatch-loop frame ratchet, L7) or reuse `DOOR_INVOKE_FAST` with the virtual-thread condition
   folded into its admission.
2. **`virtual_door_tier_up`.** It can answer `Compiled(..)` and enter a compiled body
   (`virtual_door_call_compiled`), and with background compilation off it can run an inline
   upgrade (`try_jit_upgrade_with_gate`). On a virtual thread it needs the general `VirtualBytecode`
   arm's gate (`vthread_refused`: the three switches plus `vthread_may_enter_compiled_method`):
   * refused: `Interpret`, without the `jit_cache` probe and without counting, as the general arm
     neither probes nor counts a refused site;
   * admitted: the platform answer, but never the inline upgrade (the general arm's
     `!on_virtual_thread` guard).
3. **The private hand-off** (`nonvirtual_handoff_on`, `nonvirtual_door_finish`). It already asks
   `vthread_door_gate` since wave 114.
4. **`door_record_receiver` and the receiver profile.** Same as on a platform thread. PGO keeps
   every door off anyway.
5. **The sig-poly site cache** (L4's area). Confirm that a cached route never enters compiled code
   or a linker that can yield under Rust frames before admitting it.

## A second per-call cost, for the same proposal

The static door's `Jit` arm (`dispatch_static.rs`, `continuation_interpreted && matches!(&target,
CachedInvokeTarget::Jit ..)`) calls `vthread_may_enter_compiled_method` on every hit. That
re-check is needed for the learned and app arms, whose answer can move to "refuse" when the deny
set grows. For a leaf admitted by `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY`, it re-runs
`bytecode_cannot_reach_a_yield` over the whole body (up to 256 bytes) on every call. A leaf has no
invoke, so the scan never stops early. The answer is a pure function of the code when
`CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS` is off. A one-bit memo per `CachedBytecodeMethod` would
remove the scan. That needs a field and about 50 struct literals, most in L2's jit tests, or a
spare bit in an existing cell; `intercept_shape_cache` has five spare bits but belongs to another
question.

## Measure

`tools/probes/interp/L6/L6W114VirtualThreadRequestBench.java` (its route call is an
`invokeinterface`, its session call an `invokevirtual`), and the `helpers` rows of
`L6W108VirtualThreadLearnedRepsBench`.

## Wave 115 (interpreter round i6, lane L6)

SWITCH-OFF. Built behind `CRATONVM_VTHREAD_VIRTUAL_DOOR` (`VmConfig::vthread_virtual_door`, flag
group JIT, token `vthread-virtual-door`, `runtime_flag_on`, default OFF, performance only). Not built
or run.

What changed:

* **Admission** (`vm/src/runtime/interpreter.rs`, `execute_frame_from_index`; L7's file). The old
  `invoke_fast_door_on` is now `platform_fast_doors_on`; a new `jdk_vthread_doors_observable` is a
  mounted JDK virtual thread under the same observability conditions. `invoke_fast_door_on` (the
  `DOOR_INVOKE_FAST` bit) is the platform answer OR the JDK virtual thread under the new switch, so
  the gate byte is not widened. `nonvirtual_fast_door_on` and `sig_poly_site_on` are now computed
  from `platform_fast_doors_on`, so this switch does not admit them: the nonvirtual doors keep
  `CRATONVM_VTHREAD_NONVIRTUAL_DOORS`, and the sig-poly cache stays off on a virtual thread
  (item 5 is not done; L4 has to review it first).
* **The door** (`dispatch_virtual::execute_invokevirtual_fast_door`). Once the callee is owned
  (`DoorCallee`), it asks `invoke_fast::vthread_door_gate(.., static_door = false)`. That is
  exactly the general `VirtualBytecode` arm's `vthread_refused`: the three instance switches plus
  `vthread_may_enter_compiled_method`, asked on every call, so a deny sticks.
  * `DeclineSynchronized` declines to the general arm (`door_decline_late_owned`, reason
    `synchronized callee on a virtual thread`), whose monitor path may unmount a contended
    virtual thread. The trivial getter and the empty-body elision ahead of it already refuse
    `synchronized` callees.
  * `Interpret` skips the tier-up block, so there is no `jit_cache` probe and no count, as in the
    general arm.
  * `Platform` keeps the tier-up block.
* **No inline compile on a virtual thread** (`virtual_door_tier_up`). With background compilation
  off, a due stride returns `Interpret` on a virtual thread, as the general arm's
  `!on_virtual_thread` guard does.
* **No argument overlap on a virtual thread** (`in_place`), as the static door since wave 114.
* **The lambda door** (`invoke_fast::execute_lambda_door`) declines on a virtual thread. Its
  interpreted tier-up block was never reviewed against the general lambda arm on a virtual
  thread. One `kind` compare after the door's `lambda_door_live` test. A follow-up can admit it
  with the same gate.
* **The private hand-off** is unchanged. It runs only with `nonvirtual_handoff_on`, which is
  `DOOR_NONVIRTUAL_FAST`, and asks `vthread_door_gate` since wave 114.
* **Config test**: `the_wave_114_l6_vthread_app_entry_local_count_and_doors_follow_their_words` also
  checks the new word, and that it does not imply the nonvirtual doors.

The second per-call cost of this page (the leaf re-scan on every `Jit` hit) is not done. It still
needs a memo bit per `CachedBytecodeMethod`, and the struct literals of that type in L2's jit tests
would all change.

Probe: `tools/probes/interp/L6/L6W115VirtualThreadVirtualDoor.java` (HotSpot block filled from
HotSpot 25.0.3). Its rows:

* a monomorphic `invokevirtual`;
* a four-receiver `invokeinterface` (poly entries);
* a contended `synchronized` instance method;
* a getter, an empty override and a default method;
* an interface method that parks inside itself;
* an exception two virtual calls down;
* a lambda;
* a private nestmate call.

Run it with `-Djdk.virtualThreadScheduler.parallelism=2` in both modes, JIT and `--nojit`, in
three arms. All arms must be SAME:

* (a) no switch;
* (b) `CRATONVM_VTHREAD_VIRTUAL_DOOR=1`;
* (c) (b) plus `CRATONVM_VTHREAD_NONVIRTUAL_DOORS=1 CRATONVM_VTHREAD_COMPILED_APP_ENTRY=1`.

Also run `L6W114VirtualThreadDoorsAndAppEntry`, `L6W86`, `L6W89`, `L6W92`, `L6W93`, `L6W100` and
`L2W101` under (b) and (c). Under (b), `CRATONVM_DBG_FIELD_SITE=1` should name `synchronized callee
on a virtual thread` among the virtual door's declines.

A/B. Run interleaved, at least 5 runs per arm, medians, both modes:

    (A) cratonvm L6W114VirtualThreadRequestBench           (and L6W108VirtualThreadLearnedRepsBench)
    (B) CRATONVM_VTHREAD_VIRTUAL_DOOR=1 ...
    (C) CRATONVM_VTHREAD_VIRTUAL_DOOR=1 CRATONVM_VTHREAD_NONVIRTUAL_DOORS=1 ...

Expected, by reading:

* `request-virtual` and `blocking-virtual`: 0.85-0.95x of A for B, and less for C. The route call
  is an `invokeinterface` and the session call an `invokevirtual`, both warm and monomorphic.
* W108 `helpers-virtual`: 0.9-1.0x.
* Every `*-platform` row: 1.00x.

Flip rule: no virtual row worse than 1.05x in either mode, and the platform rows at 1.00x.

## Wave 118 (interpreter round i6, lane L6)

SWITCH-OFF. This is the "second per-call cost", the leaf re-scan on every `Jit` hit. It is built
behind `CRATONVM_VTHREAD_LEAF_VERDICT_MEMO` (`VmConfig::vthread_leaf_verdict_memo`, group JIT,
token `vthread-leaf-verdict-memo`, `runtime_flag_on`, default OFF, performance only). Not built or
run.

**What it does.** In `jdk_continuation::vthread_body_cannot_yield`, when
`CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS` is off, the scan is a pure function of the code. With the
switch on, it is answered from `ContinuationRunners::leaf_verdicts`, a `LeafVerdictMemo`:

* 512 direct-mapped slots keyed by the body's `Arc<[u8]>` address.
* Each slot keeps an `Arc` of the code it answers for, so the address cannot be reused and the
  answer cannot go stale. At most 512 bodies of at most 256 bytes stay mapped.
* The first body to claim a slot keeps it. A body whose slot another body holds is scanned as
  before.
* A hit costs one acquire load, `Arc::ptr_eq` and a byte load.
* The memo is per VM.

**No new field on `CachedBytecodeMethod`.** That struct has 50 struct literals, most of them in L2's
jit tests, so no literal changed. The sites a per-entry bit would have touched are listed here for
whoever wants to move the memo onto the entry later:

* `jit/src/lib.rs`: 11
* `jit/src/tests.rs`: 19
* `jit/src/r19_sync5_tests.rs`, `jit/src/r19_syncdoor3_tests.rs`: 1 each
* `jit/tests/*.rs`: 14 files, 1 each
* `jit-api/src/lib.rs`: 2, plus the `Clone` impl
* `vm/src/runtime/interpreter/deopt_resume.rs`: 2

You can find them all with `grep -rn "branch_profile_armed: std::sync::atomic::AtomicBool::new(false),"`.

**Unit tests.** `jdk_continuation::i118_l6_leaf_verdict_memo_tests`, and the config test in
`the_wave_114_l6_vthread_app_entry_local_count_and_doors_follow_their_words`.

**A/B.** Interleaved, at least 7 reps, both modes:

    cratonvm L6W114VirtualThreadRequestBench   vs   CRATONVM_VTHREAD_LEAF_VERDICT_MEMO=1 ...
    (also L6W108VirtualThreadLearnedRepsBench and L6W92VirtualThreadCpuBench)

**Expected.** Only the virtual rows that call static leaves from interpreted frames should move.
`L6W92`'s leaf rows and the `helpers` row should come in at 0.9-0.98x. Everything else should be
1.00x.

**Correctness.** `L6W92VirtualThreadCompiledLeaf`, `L6W100VirtualThreadLeafOsr` and
`L6W114VirtualThreadDoorsAndAppEntry` should be SAME with the switch on.

## Wave 119 (interpreter round i6, lane L6)

SWITCH-OFF (`CRATONVM_VTHREAD_LEAF_VERDICT_MEMO`, default off). The memo now checks the
redefinition epoch.

**How the epoch check works.** Each slot's verdict word carries the VM's
`class_redefinition_count` at store time (`(epoch << 2) | v`). A hit stored under an older count is
not answered: the body is rescanned and the slot re-stamped.

**Why stale verdicts were already impossible.** The memo is keyed by the body's `Arc<[u8]>`
identity, and each slot keeps that `Arc` alive. A redefinition installs a new body, which is a new
`Arc`, so it gets a new key. The verdict is a function of the body's bytes alone, and an `Arc<[u8]>`
never changes them; quickening and the widened-`ldc` streams live in separate cells. The epoch
check therefore only guards against a future in-place rewrite, and costs one acquire load per hit.

**Known cost.** A slot claimed by a body that a redefinition or unload retired stays claimed.
Bodies that hash to that slot are scanned as before.

**Commands.** As in "Wave 118", plus the `L6/Redefine*` probes with the switch on:
`RedefineRunningSpliceProbe` and `RedefineBakedCallProbe`, SAME.

## Wave 122 (interpreter round i6, lane L6): the lambda door on virtual threads

SWITCH-OFF. The switch is `CRATONVM_VTHREAD_LAMBDA_DOOR` (`VmConfig::vthread_lambda_door`, group JIT,
token `vthread-lambda-door`, `runtime_flag_on`, default OFF, performance only). Not built or run.

### What changed

All in `invoke_fast::execute_lambda_door`.

* **Admission.** The door is reached on a virtual thread only behind the virtual door, which has
  been default ON since the w115 triage. It used to decline every virtual thread. With the switch
  on, it now serves a mounted JDK continuation's thread (`jdk_continuation.jdk()`). A bound virtual
  thread still declines.
* **No tier-up block on a virtual thread.** On a virtual thread the door skips
  `if shared.config.jit_enabled { .. }` completely, so it does no compiled-body probe or entry
  (`lambda_door_compiled_body` already refused `Virtual`), no `callee_has_compiled_body` decline,
  no count and no offer. That mirrors the general arm, `lambda::try_invoke_cached_lambda_impl`,
  whose whole compiled block is guarded by `!matches!(thread.kind, Virtual)`. So a virtual thread's
  lambda call gets the same answer through either path: an interpreted push of the impl, with the
  same frame.
* **The rest of the door is unchanged on every thread:**
  * the `lambda_door_live` site table;
  * the replayed `checkcast`s, which decline when a cast would fail, so the general path throws
    the `ClassCastException`;
  * the verbatim argument checks and the frame limit;
  * `push_frame_stage1`.
* **What is not admitted.** Lambda impls are never `synchronized` door sites. The site table is
  per OS thread, but it holds site facts, not thread state, so a continuation that migrates between
  carriers reads the same answers.

### Probe

`tools/probes/interp/L6/L6W122VirtualThreadLambdaDoor.java` (HotSpot block filled from HotSpot
25.0.3). Its rows:

* plain and capturing lambdas;
* a long capture;
* a lambda that parks in its body;
* a failing replayed `checkcast`;
* a throwing body;
* a method reference.

Each must be SAME in both modes, JIT and `--nojit`, with the switch off and on:

    cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W122VirtualThreadLambdaDoor
    CRATONVM_VTHREAD_LAMBDA_DOOR=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W122VirtualThreadLambdaDoor

Also run `L6W115VirtualThreadVirtualDoor` (its `lambda` row) and
`L6W114VirtualThreadDoorsAndAppEntry` with the switch on.

### Bench

Run interleaved, at least 7 reps, both modes:

    cratonvm L6W114VirtualThreadRequestBench   vs   CRATONVM_VTHREAD_LAMBDA_DOOR=1 ...

Expected, by reading: 0.95-1.0x on the virtual rows, a little more where lambdas dominate, and
1.00x on the platform rows. Flip when no virtual row is worse than 1.05x.

## Wave 122 (interpreter round i6, lane L4): item 5, the sig-poly site cache

**L4 review: sound.** It is now built behind `CRATONVM_VTHREAD_SIG_POLY_SITES`
(`VmConfig::vthread_sig_poly_sites`, group JIT, token `vthread-sig-poly-sites`, `runtime_flag_on`,
default OFF, performance only). Not built or run by the lane.

### The review

The question was whether a cached route can enter compiled code, or a linker that can yield under
Rust frames, where the slow path would not.

* **A route replays exactly what the slow path ran.** It is the registered native that the slow
  path dispatched, recorded at the dispatch point (`note_route`), called through the same
  `safe_native_call` / `safe_native_call_synchronized`, with the slow path's monitor fact. The
  native makes every per-call decision on both paths, including any compiled entry and the
  continuation-yield discipline of the method-handle doors (`transparent_leaf_call`,
  `CRATONVM_MH_VOID_TAIL_UNMOUNT`). So the cache adds no compiled entry and no Rust frame that the
  slow path does not already have.
* **The slow path has no virtual-thread arm to skip.** `execute_invoke_kind`,
  `try_stackless_invoke`'s erased `MethodHandle` arm and `invoke_on_class_shared_inner`'s
  signature-polymorphic block have no `ThreadKind::Virtual` / `jdk_continuation` test. The hit
  skips only `mh_site_link_checked`, which ran when the site was first linked.
* **A yield inside the native** returns `Err(ContinuationYield)` from `run_route`. The arm routes it
  through `route_fast_path_error!`, which returns it as `Fatal` without unwinding, the same as the
  other doors. The caller's pc was set to `saved_pc + 3` before the probe, as on the slow path.
* **A lent site** (`CRATONVM_SIG_POLY_SITE_LEND`) is given back before the hit returns, on every
  exit including a yield's `Err`. No continuation can mount on the carrier while a site is out.
* **The extra arms** are interpreted only:
  * the VarHandle CAS/CAE field arms are plain field operations with no call;
  * the wave-120/121 frame pushes (`try_basic_frame_push`, `linkTo*`, `--nojit` only) push
    interpreted frames into this loop. That makes a LambdaForm chain yieldable where the native
    pinned it, which is HotSpot's shape.
* **The cache is a per-`JvmThread` table of site facts** (caller class, cp index, pc, receiver
  class, native slot), not thread state. A continuation that migrates reads the same answers.

### Defect found and fixed (any thread)

Since wave 120 a hit can push a frame (`try_basic_frame_push` returns `FramePushed`), but both
`invokevirtual` arms that probe the cache (`try_hit_first`, `try_hit`) answered `Some(Ok(_)) =>
continue` without moving `frame_idx` to the new frame. With `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1`
under `--nojit`, the loop then ran the caller at a stale index, past its invoke, with the callee
frame on top. Both arms now set `frame_idx = thread.frames.len() - 1` on `FramePushed`, as the
fast door's arm does (`vm/src/runtime/interpreter.rs`). There is no switch: it is a plain defect of
the wave-120 switch-on path, and the switch-off path never returns `FramePushed` there.

### The change

In `vm/src/runtime/interpreter.rs` (`execute_frame_from_index`), `sig_poly_site_on` is now
`(platform_fast_doors_on || (jdk_vthread_doors_observable && vthread_sig_poly_sites)) &&
sig_poly_site_cache`. `sig_poly_site_first_on` follows it.

### Probe

`tools/probes/interp/L4/L4W122VirtualThreadSigPolySites.java` (HotSpot 25.0.3 block, recorded by
default and with `-Xint`). Its rows run on virtual threads:

* VarHandle get/set/getAndAdd, CAS and CAE;
* `invokeExact`: static, virtual over two receivers, a sleeping target, a throwing target;
* a retargeted `MutableCallSite`;
* a wrong-type `invokeExact`;
* a null VarHandle receiver.

Run with `-Djdk.virtualThreadScheduler.parallelism=2`, both modes, JIT and `--nojit`. All arms must
be SAME:

* (a) no switch;
* (b) `CRATONVM_VTHREAD_SIG_POLY_SITES=1`;
* (c) (b) plus `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1`, `--nojit`. Run this arm on a platform thread
  too, since it covers the `frame_idx` fix: `L7W120ReflectSplitBench` and
  `L7W114NativeReentryBench` (stdout) with `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1 --nojit`.

Also run `L4W115VirtualVoidLeafUnmount` and `L4W28VirtualMhReturnUnmount` under (b), with and
without `CRATONVM_MH_VOID_TAIL_UNMOUNT=1`.

A/B: `L6W114VirtualThreadRequestBench` and a VarHandle-heavy virtual row (the probe's `vh-*` rows
in a loop), (a) vs (b), interleaved, medians. Expected 0.9-1.0x on virtual rows, 1.00x on platform
rows.
