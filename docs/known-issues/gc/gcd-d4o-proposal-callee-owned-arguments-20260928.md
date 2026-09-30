# Proposal: a call's reference arguments belong to the callee once it runs

> **STATUS (2026-09-28, gcd d9/d, lane args9): BUILT for the one call shape
> the ArgPin census measured; the rest is a new defect page.** The measured
> holder was none of (1)-(3) below: it was the SINGLE-PASS tier's service
> copy of a baked direct `invokestatic`'s arguments (`tier=sp`, `caller`
> off=104, named in the call's map as a staged argument). That shape now
> keeps no copy when the callee's body declares no handlers and has no
> frameless trap stub (default on, `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` off;
> `jit/src/x64/op_invoke.rs` `sp_direct_call_args_owned_by_callee`,
> `jit/src/lib.rs` `direct_callee_owns_its_arguments`,
> `vm/src/jit/helpers.rs` `drop_declined_callee_stash`). The re-run arms were
> not rewritten to read the callee's frame, as (1) proposed: a re-run from
> entry needs the ENTRY arguments, which a callee that dropped one no longer
> has, so no frame can supply them. They hand the trap to the caller's
> caller instead (a frameless deopt of the caller), and the admission keeps
> them rare. Every other shape -- the IR tier's direct call (item 1 below,
> unmeasured: the IR tier is off under moving-young, the Generational
> default), the single-pass instance direct twin, the inline caches and the
> dispatch helper's argument buffer, retire-cell and spliced sites, the
> interpreter's `JitArgPinGuard` and the Rust doors' `CompileArgPins` (item
> 3) -- is listed with its exact reader in
> `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928.md`.
> Item 2 (the IR callee's parameter home) is unchanged and unmeasured. Keep
> this page until the orchestrator retires the defect page; then it can go
> to `docs/internal/` with it.

> **Earlier status (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 3
> of 54).** Not built. It is now the only fix left for
> `gengc-r5w4-jit8-call-argument-pins-outlive-the-callees-use-of-them-20260926.md`
> (channel (a), gcd d7/u): on d7 arg_pin_1..3 (`Gcd1ArgPinProbe -Xmx64m`)
> print `PASS arg-pin-cold-caller`, `FAIL arg-pin-warm-caller`, `FAIL 1 of 2`,
> 3/3, while the same `cleared` body passes in `GenR5W2OsrDeadSlotProbe`
> (osr_dead_1..3 `PASS all 3`). **Gate:** `Gcd1ArgPinProbe` `PASS all 2` 3/3
> with the census naming no holder at `caller` / `dropAndCheck` /
> `native-pin`, and the deopt / re-run battery clean. **Size:** M for (2)
> parameter homes; L for (1), the call-lowering redesign.

*Filed 2026-09-28 by gcd wave d4, lane o (frames4). Proposal, not a defect
page; the defect is
`gengc-r5w4-jit8-call-argument-pins-outlive-the-callees-use-of-them-20260926.md`.
Nothing changes until it is designed in full and priced.*

## Why

HotSpot passes an outgoing argument in the callee's frame. The caller's oop
map at the call leaves out a value that is dead after the call. The callee's
map drops a parameter once the callee no longer reads it. So `dropAndCheck(l)`
nulling `l` really drops the list.

CratonVM keeps the argument rooted for the whole call through three channels.
Each one keeps `Gcd1ArgPinProbe`'s warm case alive if nothing else does:

1. **The IR caller's home of the argument value.** It is an input of the call
   node, so `dead_ref_colours_at` sees a live range at the call, and no
   dead-home clear can zero it before the call reads it to stage it. The frame
   block and the band scan then root it for the whole call.
2. **The callee's parameter home** (`[rbp - (idx+1)*8]`). It is written by the
   prologue and never cleared (oomjit9 item 5 (a)).
3. **The Rust dispatch door's pins** (`JitArgPinGuard`, `CompileArgPins`).
   Every re-run arm reads them back after the call.

## The proposal

- **(2) first, because it is local:** give the IR's parameter homes the same
  treatment as colours. A parameter home is dead at a call when no range of
  its `Op::Param` node covers the call and no snapshot the keep set keeps
  names it. Slot 0 of an instance method is excluded (a synchronized
  receiver, stack traces). The stale-set walk treats the prologue store as a
  `Def` at entry. Single-pass: the same through its local liveness proposal
  (`gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md`).
- **(1) needs the call's lowering to cooperate:** zero the argument homes that
  the call's own after-call keep set does not name AFTER the arguments are in
  the ABI registers, and before the CALL instruction. That is only sound if
  nothing after the call reads them. The direct-call cold paths (re-stage and
  re-dispatch on the deopt sentinel, the exception-table service range)
  currently do read them. Those paths would have to re-stage from the callee's
  saved arguments, or from a service range the collector rewrites and the
  callee's liveness does not keep. That is the design work.
- **(3) is the page's own fix 1** (resume precisely instead of re-running),
  unchanged.

## How to verify

`Gcd1ArgPinProbe -Xmx64m` (JIT on): `PASS arg-pin-cold-caller`,
`PASS arg-pin-warm-caller`, `PASS all 2`. The census has no holder at `caller`
(`region=operand-spill`), at `dropAndCheck` (`region=java-local`) or in
`native-pin`. The deopt / re-run battery must stay clean: every re-run arm
reads arguments this proposal stops keeping.
