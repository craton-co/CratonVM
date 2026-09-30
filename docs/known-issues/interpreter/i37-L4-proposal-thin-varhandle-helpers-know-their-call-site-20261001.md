# Proposal: thin VarHandle helpers know their call site

**Status: proposal, filed 2026-10-01 by interpreter round i1 wave 37, lane
L4.** Not implemented.

## Why

The JIT's thin `VarHandle` helpers (`vm/src/jit/helpers.rs`
`jit_varhandle_{read,write,cas}_direct<SLOT>`, bound by the single-pass door
from the constant-pool triple: `jit/src/lib.rs` `varhandle_*_helper_slot`)
receive only the handle and the arguments. The call site's descriptor is
reduced to the slot (mode x value kind) at compile time, and their cold arm
dispatches through one static stand-in `JitInvokeInfo` per slot
(`VARHANDLE_{READ,WRITE,CAS}_INFOS`). Three things follow:

* an invoke-exact `VarHandle` (wave 37, item 3 of
  `interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`) cannot be
  judged at such a site, so a mistyped access converts where HotSpot throws
  `WrongMethodTypeException` (`L4W37VarHandleExact` `hot-wrong`, default mode);
* every bound site of a slot shares one native-site-cache key
  (`jit_invoke_dispatch` keys on the info address), so the cache cannot hold
  a per-site answer;
* the value-site classes armed for the cold arm are the stand-in's, not the
  site's (the reference slots were split into kinds to live with that, round
  11 wave 18).

## Direction

Emit, per bound site, a `'static` `JitInvokeInfo` built from the REAL
constant-pool triple (the site's own `info`, which the generic path already
has) and pass its address as one extra argument to the thin helper, the way
`jit_invoke_dispatch` receives `info`. The fast arm ignores it (no per-call
cost beyond one register); the cold arm dispatches with it instead of the
stand-in, and the exact check (`var_handle_exact_refusal`) then runs at the
native site cache like any generic site. `is_varhandle_stand_in_info` and
the stand-in arrays can go.

Measure first: the JIT-mode `VarHandle` benchmark rows (field reads, writes
and CAS loops through bound sites) with and without the extra argument,
interleaved, fat LTO.

## Progress (wave 46) — lane L4

Not built (emitted code; the round's last wave closed pages instead). This
proposal now carries the one remainder of the closed
`docs/internal/fixed-bugs/interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010.md`:
item 3 (an invoke-exact `VarHandle`) at a compiled site bound to a thin
helper, which is step 4's correctness half below. Before building step 3,
settle wave 41's open question with one host run:
`CRATONVM_DBG_JITC=1` on `L4W37VarHandleExact` and
`L4W41VarHandleExactThinHelper` says whether their loops are bound to a
thin helper at all (if not, the positive control needs a site the
single-pass door binds, and the probe must change first).

## Progress (wave 45) — lane L4

Not built (emitted code in two backends; this lane does not build). The
wave-43 trace below is the design; the order of work, so each step can be
built and measured on its own:

1. **Planner, no codegen change.** In the single-pass planner's three
   `varhandle_*_helper_slot` arms (`jit/src/lib.rs`) and the OSR door's
   (`vm/src/runtime/interpreter/jit_bridge.rs`, `compile_osr_artifact`),
   push the site's real `invoke_info` row beside the `JitDirectCall`, as the
   `FfmSegmentGetAtIndex` arm does. Nothing reads it yet; a jit unit test
   asserts the row is there for a bound site.
2. **Helper ABI with a decline value** (`vm/src/jit/helpers.rs`): write
   returns `1` done / `0` declined, CAS `0` / `1` / `-1` declined, read
   writes its value through an out pointer and returns `1` / `0`. The cold
   arm (the stand-in `VARHANDLE_*_INFOS` dispatch) stays as the declined
   path's body for now, so behaviour is unchanged.
3. **x64 emitter** (`jit/src/x64/op_invoke.rs`): a `VARHANDLE_THIN` region
   modelled on `INTRINSIC REGION: FFM_SEGMENT` — CALL the helper, test the
   decline value, and on decline build the argument buffer and call
   `invoke_dispatch` with the site's own `info`. A codegen unit test in the
   FFM region's style. The aarch64 backend (`jit/src/aarch64_backend.rs`)
   has no FFM region to copy (the `FfmSegmentGetAtIndex` names occur only in
   `jit/src/lib.rs`, `intrinsic_catalogue.rs` and `x64/op_invoke.rs`), so
   there the planner keeps binding the old shape behind a target `#[cfg]`
   until a twin is written.
4. **Retire the stand-ins:** the helpers' cold arm, `VARHANDLE_*_INFOS` and
   `is_varhandle_stand_in_info` go; the fast arm declines an exact receiver
   (the per-VM gate `NativeRealm::var_handle_exact_slot`, then the
   receiver's `exact` field). Positive control
   `L4W41VarHandleExactThinHelper` (HotSpot `20000` / `20000`) once
   `CRATONVM_DBG_JITC=1` shows its loop bound; the benchmark is
   `L4W43VarHandleThinHelperBench`, interleaved against the previous step.

Steps 1-2 are behaviour-neutral and can land first; step 3 is the one that
needs a crash-free host run in both backends.

## Progress (wave 41) — lane L4

Not built. A probe for it: `tools/probes/interp/L4/L4W41VarHandleExactThinHelper.java`
(a same-value-kind, other-coordinate site on an exact handle,
`(Ljava/lang/Object;)I`, which the thin read and write helpers serve; see
`interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`, Progress
(wave 41)): unlike `L4W37VarHandleExact`'s `hot-wrong` (whose value kind
differs from the field's, so the fast arm declines to the stand-in cold
arm), its access is one the fast arm itself serves. Why `hot-wrong` matched
on the host although the stand-in is not judged is an open question on
that page.

## Progress (wave 43) — lane L4

Not built. Wave 43's lane section asked for it; the lane traced the change
and judged it unsafe to write without a build: it is emitted-code work in
two backends, and a wrong register or stack slot there is a crash, not a
failed probe. What the trace found, so the next attempt starts from it:

* **Which doors bind the thin helpers.** The single-pass planner
  (`jit/src/lib.rs`, the three `varhandle_*_helper_slot` arms near the
  `Long.longValue` bind) and the OSR door (`vm/src/runtime/interpreter/jit_bridge.rs`,
  `compile_osr_artifact`, the three arms near line 4700). The IR tier does
  not: its planner routes any method with such a site to the single-pass
  tier (the `varhandle_*_helper_slot` terms of the IR eligibility predicate
  in `jit/src/lib.rs`). Neither door pushes an `invoke_info` row for the
  site, so the x64 direct arm (`jit/src/x64/op_invoke.rs`, the plain
  direct-call path after the intrinsic regions) has `info_ptr == None` and
  emits no site constant.
* **A better shape than "one extra argument": the FFM decline edge.**
  `INTRINSIC REGION: FFM_SEGMENT` in the same arm already solves "a thin
  helper that must fall back to the site's own dispatch": the planner pushes
  BOTH a sentinel `JitDirectCall` and the site's real `invoke_info` row
  (`jit/src/lib.rs`, the `FfmSegmentGetAtIndex` arm), and the emitter CALLs
  the helper, tests `RAX == 0` for "declined", and on decline builds the
  argument buffer and calls `invoke_dispatch` with the REAL `info`. Moved to
  `VarHandle`: the thin helpers stop calling `jit_invoke_dispatch`
  themselves (their cold arm and the stand-in `VARHANDLE_*_INFOS` go, and
  with them `is_varhandle_stand_in_info`), and the decline edge dispatches
  with the site's own info, where `try_jit_site_cached_native_dispatch`
  judges an exact receiver (`jit_var_handle_exact_refusal`) and the native
  site cache keys per site. The helper ABI needs a decline value:
  * write: `(vm, vh, recv, value) -> 1 done | 0 declined` (no new argument);
  * CAS: `(vm, vh, recv, expected, new) -> 0 | 1 | -1 declined`;
  * read: `(vm, vh, recv, *mut i64 out) -> 1 | 0`, the FFM get's shape (an
    out slot reserved before the decline buffer, as there).
  The fast arm must also decline an exact receiver: one relaxed load of the
  per-VM gate (`NativeRealm::var_handle_exact_slot`, `u32::MAX` until a VM
  mints its first exact handle), then the receiver's `exact` field only
  once the gate is open. No per-call cost otherwise beyond the `RAX` test.
* **What else must move with it:** the aarch64 backend's direct-call arm
  (the planner is shared, so a new region needs its aarch64 twin or an
  `#[cfg]` that keeps binding the old shape there); the OSR door's arms
  (`invoke_info` rows); the jit crate's unit tests of the three slot
  functions (they stay) and a codegen test in the style of the FFM region's.
* **Benchmark rows to compare** (interleaved, fat LTO, JIT mode): the new
  timing probe `tools/probes/interp/L4/L4W43VarHandleThinHelperBench.java`
  (`get-int`, `getvol-long`, `get-ref`, `set-int`, `setrel-long`, `set-ref`,
  `cas-int`, `cas-ref`, and `get-int-exact` with the exactness gate open),
  plus the engagement counts `CRATONVM_DBG=jit-method-stats` prints
  (`VarHandle.read=` / `write=` / `cas=` served/declined).
* **Positive control for the correctness half:**
  `tools/probes/interp/L4/L4W41VarHandleExactThinHelper.java` (HotSpot:
  `20000` and `20000`), once its loop is shown to be bound
  (`CRATONVM_DBG_JITC=1`).
