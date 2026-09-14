# OSR, VM side: the validated entry, and the exit that must not replay

Companion to [`on-stack-replacement.md`](./on-stack-replacement.md), which
describes the `jit`-crate half of the change and ends (§5, §7) with two edits it
could not make because `vm/src/runtime/interpreter.rs` was not in its scope.
This document records those two edits, now landed, and what they close.

Everything here lives in exactly two files:

* `vm/src/runtime/interpreter.rs` — `transfer_osr_exit_into_live_frame` and its
  new `_checked` body.
* `vm/src/runtime/interpreter/invoke.rs` — `try_osr`.

---

## 1. The defect this closes

`jit/src/deopt.rs` split `FrameValue::MaterializationRequired` out of
`FrameValue::Unsupported` so that a value an optimization *deleted* refuses to
be reconstructed, instead of arriving as a silent null. The VM's in-place
OSR-exit transfer honoured that only **by accident**: the variant was not
special-cased, so it fell into `fv_to_value`'s catch-all `None` arm and the
transfer bailed with `"unmappable local"`.

The direction was right; the moment and the name were not.

* **The name.** "unmappable local" is the label for a slot the coarse
  whole-method classifier could not *type*. `MaterializationRequired` is a slot
  whose value was *deleted*. Those are opposite verdicts — the first is
  tolerated (the live frame's current value is provably safe to leave in place),
  the second must refuse — and reporting both under one label threw away the
  entire reason the variant exists.
* **The moment.** The bail propagated to `try_osr`'s
  `can_osr_exit && transfer(...)` check, which fell through to the **safe
  reject**: "continue interpreting THIS frame from where it was". That is
  correct only when the bail *precedes* any committed iteration. Here it does
  not — the OSR'd body ran — so every iteration since entry executed a second
  time. That is the recorded `jit-osr-bail-reruns-loop-iterations` defect
  (20 000 requested, 20 008 executed; 12 346 requested, 42 730 executed once
  the exception escaped the OSR'd method), and it is a correctness bug, not a
  slowdown.

---

## 2. Edit 1 — the refusal names itself

`transfer_osr_exit_into_live_frame` is now a thin wrapper over
`transfer_osr_exit_into_live_frame_checked`, which returns the refusal *reason*
rather than a bare `None`. The wrapper traces it under `CRATONVM_DBG_DEOPT` and
returns `Option<()>` exactly as before, so no caller changed shape; the split
exists so every refusal has a name a test can assert on.

`MaterializationRequired` joins the Phase-A scope guard, next to the
`caller_frames` / `monitors` / `VirtualObject` refusals, and covers **both**
locals and the operand stack (the latter used to reach the equally-wrong
"unmappable stack slot"). It names the region and index:

```
materialization required (local 1: eliminated store (producer n7))
```

Ordering matters: it is decided *before* any mapping and before any write, so a
`MaterializationRequired` slot can never half-write the live frame, and the
generic `"unmappable local"` arm below it no longer stands in for it.

### What did not change

`docs/jit/deopt-frame-state-interning.md` records that all three resume sinks
refuse a `ReconstructedFrame` with a non-empty `caller_frames`
(`resume_from_ir_deopt`, `build_deopt_frame_inner`,
`transfer_osr_exit_into_live_frame`). That is still correct and still in force.
The new guard is a **per-slot** verdict and says nothing about scope depth, so
the caller-chain rule keeps exactly the meaning it had — asserted directly by
`materialization_guard_does_not_disturb_the_caller_chain_rule`.

---

## 3. Edit 2 — the validated entry

`try_osr` used to build a bare `Vec<i64>` and call `osr_enter`: raw words, no
types attached, nothing comparing the interpreter's idea of a slot against the
compiled entry's. `Frame::get_local_tag` had existed since forever, documented
"for JIT/OSR interop", and was never passed to the JIT.

It is now built in the same loop as `jit_locals`, and the two feed an
`OsrEntryState` through `CompiledMethod::validate_osr_entry` →
`osr_enter_planned`.

```rust
let frame = &thread.frames[frame_idx];
for i in 0..num_locals {
    jit_locals.push(frame.get_local_raw(i) as i64);
    jit_local_tags.push(frame.get_local_tag(i));      // same snapshot
}
let osr_state = cratonvm_jit::OsrEntryState {
    pc: entry_pc, locals: &jit_locals, local_tags: &jit_local_tags,
    stack: &[], stack_tags: &[],
};
let plan = compiled.validate_osr_entry(&osr_state)?;   // (memo-then-refuse)
unsafe { compiled.osr_enter_planned(vm_ptr, &osr_state, &plan, thread_ptr) }
```

Both invariants `on-stack-replacement.md` §6 states are now enforced rather than
assumed:

| Invariant | How it is held |
| --- | --- |
| `state.pc` is the frame's **current** pc | `try_osr` refuses outright when `entry_pc != frame.pc`. Every back-edge site captures `entry_pc = frame.pc` after the branch is taken and nothing between there and here moves it — but a future trigger that passes some other pc is now refused, not silently entered at a bci the interpreter is not standing on. |
| tags come from the **same** snapshot as the words, **before** `set_jit_thread` | Both are read in one loop from one `&thread.frames[frame_idx]` borrow, which ends before `set_jit_thread`. No safepoint can run between a word and its tag. |

The operand stack is passed explicitly as empty rather than omitted. A
back-edge entry has an empty stack by construction, and `osr_trampoline` seeds
locals only — so a future trigger at a bci with live operands is **refused**
(`osr-entry-operand-stack`) instead of silently truncated.

### The memoization split

`validate_osr_entry` returns a `bailout::Bailout` carrying a refusal tag.
`cratonvm_jit::osr_refusal_is_permanent` is the predicate that decides whether
the VM may cache it:

* **Memoed** via `crate::jit::mark_osr_entry_rejected` — the nine
  `OSR_PERMANENT_REFUSAL_TAGS`: `osr-entry-no-table`,
  `osr-entry-pc-not-an-entry`, `osr-entry-dead-local-mask`,
  `osr-entry-undescribable-slot`, `osr-entry-inlined-scope`,
  `osr-entry-unconditional-trap`, `osr-entry-unresumable-exit`,
  `osr-entry-ambiguous-exit-image`, `osr-entry-contract-disagreement`. Each is a
  pure function of the artifact, so it reproduces for every future back-edge
  over the same pc and re-running the pipeline can only reach it again.
* **Not memoed** — `osr-entry-local-count`, `osr-entry-operand-stack`,
  `osr-entry-slot-type-mismatch`, `osr-entry-returnaddress-slot`. These depend on the
  *incoming interpreter state*; the next trip over the back-edge carries
  different locals and may well be admissible. Memoing one would permanently
  deny OSR at that pc on the strength of one unlucky iteration.

The per-pc exponential backoff (`record_osr_rejection`, driven from
`try_osr_with_backoff`) is what throttles the non-memoable ones, exactly as it
already throttled every other rejection.

---

## 4. Why the post-commit replay is now unreachable

Structurally, in this order:

1. **`can_osr_exit` implies a non-empty `deopt_points`.** `osr_exit_points` are
   emitted *into* `deopt_points` (`jit/src/x64.rs`, `emit_osr_exit_map_at_reason`)
   and `can_osr_exit = !osr_exit_points.is_empty()`. So an artifact that cannot
   reach the transfer at all is the `OsrExitPolicy::PropagateOnly` case, and the
   transfer is never called for it.
2. **An admitted entry is `ExactTransfer`.** `validate_osr_entry` ends in
   `osr_exit_policy`, which walks **every** deopt point of the artifact and
   refuses the entry (`osr-entry-unresumable-exit`, permanent, memoed) if any of
   them holds monitors, has non-`REEXECUTE` semantics, or fails
   `deopt::frame_state_is_resumable` — which is exactly where
   `MaterializationRequired` and `Unsupported` are caught. A caller scope is no
   longer refused as such: `osr_exit_policy` refuses (`osr-entry-inlined-scope`)
   only a chain deeper than `deopt::MAX_OSR_INLINE_RESUME_DEPTH` (9), and it
   also refuses two resume images at one bci whose `ResumeSemantics` disagree
   (`osr-entry-ambiguous-exit-image`). This runs **before**
   the trampoline, when nothing has executed, so the refusal costs no replay.
3. **The exit is the same set.** Every refusal
   `transfer_osr_exit_into_live_frame` can raise for a *reconstructed* frame is
   the runtime image of a refusal `osr_exit_policy` already screened at
   admission. There is no exit shape that passes admission and then fails here.
4. **The resume bci is exact or absent.** The bare `frame.pc = rframe.bci` is
   replaced by `plan.resume_after_exit(&compiled, &rframe)`, which re-checks that
   the bci is a deopt point *this* artifact recorded and that its
   `ResumeSemantics` is `REEXECUTE`, and returns the reconstructed frame's own
   bci — never the entry bci. A mis-routed stash, or a `RESUME`/`RETHROW` point,
   refuses instead of parking the interpreter on a guess. It is consulted before
   the locals/stack are overwritten, so a refusal leaves the frame untouched.

The safe-reject branch in `try_osr` therefore keeps its one legitimate meaning —
"`can_osr_exit` is false, so no compiled iteration was ever committed through an
exit map" — and its comment now says so. **A future reader must not add a resume
path there.** If that branch is ever reached after a committed body, the
invariant above broke, and the fix belongs at admission, where nothing has run
yet.

---

## 5. Tests

In `vm/src/runtime/interpreter/deopt_resume.rs`, `mod deopt_step3_tests` (the
module moved there from `vm/src/runtime/interpreter.rs`). The last row's test
no longer exists under that name. Since the multi-frame transfer landed, the
module's caller-chain test is
`a_caller_chain_refuses_for_a_named_reason_not_for_being_a_chain`.

| Test | Asserts |
| --- | --- |
| `osr_exit_transfer_names_materialization_required_local` | the refusal starts with `materialization required`, names `local 1`, does **not** say `unmappable local`, and leaves the live frame untouched |
| `osr_exit_transfer_names_materialization_required_stack_slot` | same for an operand-stack slot (`stack 0`) |
| `unsupported_is_tolerated_where_materialization_required_refuses` | the two variants get opposite verdicts on the same fixture |
| `osr_exit_transfer_refuses_a_bci_the_artifact_never_recorded` | a bci with no matching deopt point refuses (`unresumable exit`) and moves neither the pc nor a local |
| `validated_resume_lands_where_the_body_stopped_not_at_the_entry_pc` | the frame parks at the bail's bci, never at `plan.entry_pc`, with the JIT-advanced locals intact |
| `materialization_guard_does_not_disturb_the_caller_chain_rule` | an inlined chain still refuses on its own older reason |

The eight pre-existing transfer tests now build a plan (`osr_plan_for`) and an
artifact carrying a `REEXECUTE` deopt point at the frame's bci, so they exercise
`resume_after_exit` rather than the bare `rframe.bci` they used to trust.

The memoization split itself is asserted on the `jit` side
(`osr_refusal_taxonomy_is_closed_and_counted`, `jit/src/lib.rs`), which is where
`osr_refusal_is_permanent` and the two tag constants live.

---

## 6. To reconcile

* **`validate_osr_entry` is a strictly narrower admission gate than
  `can_osr_enter`.** Three of its refusals can deny an entry that succeeded
  before:
  * `osr-entry-unconditional-trap` (`has_indy_trap`). `compile_osr_artifact`'s
    relaxed RBC.7 already refuses methods with an *unbridged* indy, and
    `has_indy_trap` is computed the same way (a fully StringConcatFactory-bridged
    method carries no trap), so the two should agree. If they ever drift, this
    gate is the stricter one and will silently cost those methods their OSR.
  * `osr-entry-slot-type-mismatch` under the inferred (`RegisterHomes`) contract.
    `osr_local_assignments[i].is_some()` is a **whole-method** home, not per-pc
    liveness, so a harmlessly-dead local that still carries a stale `float` tag
    while the artifact GPR-homes that slot for a later `int` use will be refused
    at every trip over that back-edge (non-memoed, so backoff-throttled rather
    than permanent). This is the one place worth watching for an OSR-entry-rate
    regression; `CRATONVM_DBG_JITC` prints the tag and slot. Closing it properly
    means a per-entry-pc slot-type table from the codegen — a `jit/src/x64.rs`
    edit, out of scope here and already listed in `on-stack-replacement.md` §7.
  * `osr-entry-local-count`. The background-OSR compile path takes `max_locals`
    from class metadata while the inline path takes `frame.max_locals`
    (`effective_max_locals`, which can exceed the declared value when a caller
    supplied extra argument slots). They agree today; where they would not, the
    old code fed `osr_trampoline` a short `jit_locals` slice, so the refusal is a
    fix, not a regression.
* **The permanent memo does not short-circuit the reuse path.**
  `compile_osr_artifact` consults `is_osr_entry_rejected` only inside its
  `!osr_reused` arm, so a cached `compiled_via_osr` artifact re-runs
  `validate_osr_entry` on every admitted back-edge even after a permanent
  refusal was memoed. That costs one metadata walk plus one `Vec`, throttled by
  the per-pc backoff — worth folding into the reuse arm if the walk ever shows
  up in a profile.
* **Lifted, 2026-08-18:** the transfer is no longer single-frame.
  `transfer_osr_exit_into_live_frame_checked` hands a frame with caller frames
  to `transfer_osr_exit_chain_into_live_frame`, and `osr_exit_policy` admits a
  chain up to `deopt::MAX_OSR_INLINE_RESUME_DEPTH` (9), refusing only deeper
  ones. The original bullet follows for the record.
* **The single-frame transfer still refuses a described caller chain**, and
  `osr_exit_policy` refuses such an artifact at admission for the same reason.
  Those two lift together, not separately — see `on-stack-replacement.md` §7 and
  `deopt-inline-scopes.md`.
