# Proposal: make an unreserved VM-context slot unloadable in both JIT tiers, and let helpers check the VM pointer they get

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 25
> of 54).** Not built (no `load_context` in either tier). JIT round's file
> set; the ratchet tests pinning the audit exist. **Gate:** the grep ratchet
> (no raw `context_slot_off` / `heap_local_offset` load outside the loader)
> and one unit test per context-taking node. **Size:** S (item 1), M (item 2),
> S (item 3).

- **Status:** PROPOSAL (gcd d1/mon, 2026-09-27). Not a defect. The one
  known instance, the monitor ops, is fixed by JIT round 13
  (`../../internal/gc/gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928.md`).
  The rest of the IR tier was audited clean. Two tests pin that audit
  (`ir_lower.rs`: `every_op_whose_lowering_reads_the_context_reserves_it`
  and the source ratchet `the_context_slot_readers_are_the_audited_set`).
- **Owner:** the JIT round (lane b of this wave owns the files).

## Why

A missing context reservation fails silently, and the crash surfaces far
from the cause. The VM-context pointer lives in a frame slot that exists
only when the method "needs context":

- in the optimizing tier, `context_slot_off`, decided by
  `ir_lower::scan_frame_needs`;
- in the single-pass tier, `heap_local_offset`, decided by the `needs_heap`
  scan in `x64/bytecode_compat.rs`.

The offset of an unreserved slot is `0`, and `[rbp - 0]` is the saved
caller RBP. A lowering that loads the context without the matching scan
entry compiles cleanly and passes that RBP to a helper as a `SharedVm`. The
crash then lands wherever the helper first dereferences it: in
`flush_thread_satb`, `LockSlots::lease`, or a hashbrown table on a Rust
stack.

This bug has now happened three times:

- the single-pass `needs_heap` putfield bug
  (`Catalina.setParentClassLoader`);
- the IR `Op::Store(Ref)` / `ArrayStore(Ref)` reservations (COV-03, the
  comments in `scan_frame_needs`);
- the monitor ops (gcd d1/mon).

Every time, the fix was one more line in a hand-kept list, far from the
load it guards.

## What

1. **One loader per tier.** Add `Lowerer::load_context(reg)` in
   `ir_lower.rs` and `Compiler::load_context(reg)` in `x64`. Each refuses
   the compile when the slot is unreserved:
   - IR: `latch_bailout(Internal("context load from an unreserved slot"))`;
   - single-pass: `fail("…")`.

   Convert every `load_reg_from_frame(_, self.context_slot_off)` (about 30
   IR sites) and every `heap_local_offset` load (about 100 single-pass sites)
   to it. `emit_monitor_stub` and the `InlineTlabPlan { context_off, .. }`
   consumers then take the checked value. A future scan omission becomes a
   named bail in the `CRATONVM_DBG_JITC` census instead of a wild pointer.
2. **Derive the scan from the loads.** Better still, drop the hand-kept
   list. Lower once with the context reserved, record whether any
   `load_context` ran, and publish `needs_context` from that. This costs a
   second lowering only for bodies that turn out not to need it, and those
   are the small ones. The frame-size estimate would then have to assume the
   slot, one word.
3. **A VM identity the helpers can check cheaply.** Every compiled frame
   runs on a thread whose `JvmThread` is installed in `JIT_THREAD`. Adding
   the owning `*const SharedVm` to `JvmThread` (lane b's
   `vm/src/threading/jvm_thread.rs`, set where the thread is attached to a
   VM) would let `jit_monitor_screen_vm_ptr` and its future siblings compare
   `vm_ptr` against it: one TLS load and one compare. That catches the
   variant the stack-band screen cannot: a Java heap address or other
   non-stack garbage left in RBP by a Rust caller. It is also the precondition
   for the helpers not needing `vm_ptr` at all.

## How to verify

- A unit test per tier that lowers a body whose only context-taking node is
  X, for each X the loader serves, and asserts `needs_context()`.
  `ir_lower.rs::a_monitor_only_body_takes_the_context_and_hands_it_to_the_helpers`
  is the monitor instance.
- A grep ratchet: no `load_reg_from_frame(.*context_slot_off` and no raw
  `heap_local_offset` load outside `load_context`.
