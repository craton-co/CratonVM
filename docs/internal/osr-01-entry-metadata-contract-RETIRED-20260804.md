# OSR-01 — the entry metadata contract · RETIRED 2026-08-04

> **Retired: every item in this brief is done.** The lane's answer lives in
> [`docs/feature-designs/jit-osr-entry-metadata.md`](../feature-designs/jit-osr-entry-metadata.md),
> which carries the closeout, the measurements and the injection results. The
> brief is kept verbatim below it because a retired brief is the only thing that
> records what a lane was *asked* for, as opposed to what it delivered — the
> failure `docs/known-issues/c2/archive/README.md` exists to prevent.
>
> Where it landed, item by item:
>
> | Brief item | Landed as |
> |---|---|
> | 1 — nothing typed the two pc spaces apart | `jit/src/osr_coords.rs`. The `OutPcIndexed` → `BciIndexed` conversion, with the **identity** direction checked against `orig_code_len`; it had been a bare `None =>` match arm resting on an unstated assumption. |
> | 2 — the `-1` refusal sentinel | Already enforced at the publication site before this lane; re-verified. |
> | 3 — the second compile door | `jit/src/compile_gate.rs`. There turned out to be **three** doors, not two, and neither direct one had ever checked the code-cache cap. The OSR door's compile-epoch witness was also ~1,000 lines too late to cover its own class loading. |
> | 4 — the two frame views, unchecked | `CompiledMethod::osr_home_disagreement`. The pair that can actually contradict is the precise `FrameState` versus the **register homes** — `osr_entry_frame_state` and "the deopt frame state" are literally the same object. |
> | "A test where they disagree must fail" | Six synthetic-vector tests in `osr_contract`, five in `osr_coords`, seven in `compile_gate`, seven for the frame-view check — plus `vm-cli/tests/jit_compile_gate_doors.rs`, and three injected defects each shown to trip a distinct check. |
>
> One thing the brief asked for that **cannot be written as stated**: "the three
> vectors are index-compatible". Two of the three are not in the same space —
> `osr_local_assignments` is indexed by local index, not by any pc. The design
> doc's coordinate table is the correction.

**Status:** not started as a lane; several one-off fixes have landed around it.
**Owns:** `jit/src/lib.rs` (the OSR region), `jit/src/x64.rs` (the OSR
publication site).

## Current state, verified

OSR entry works. The machinery is: `osr_pc_to_native` (per-bci entry table,
`-1` meaning refused), `osr_dead_mask` (same indexing), `osr_local_assignments`
(where the trampoline seeds each local), `can_osr_enter` /
`can_osr_enter_with`, `osr_trampoline`, `osr_entry_frame_state`, and
`osr_exit_points`.

What has no owner is the **contract between them**. Four things this campaign
found, each a separate near-miss:

1. `osr_pc_to_native` is indexed by *interpreter bci* by the runtime, but the
   emitter writes it at whatever pc it is emitting — which under a bytecode
   transform is an *output* pc. The coordinate change is one function call at
   the publication site. Nothing typed the two spaces apart, so the bug class
   is "a plausible integer in the wrong space".
2. The refusal sentinel is load-bearing and was not enforced as an invariant.
   A bci with no steady-state image must publish `-1`; entering there resumes a
   "back edge next" frame at the top of a fresh body and runs one extra
   iteration. That refusal is now enforced at the publication site regardless
   of which producer filled the vector.
3. The OSR compile path in `vm/src/runtime/interpreter/invoke.rs` calls the
   backend **directly**, not through `try_compile`, so it misses anything
   `try_compile` does at entry. That was found only because the code-cache lane
   needed a compile-epoch witness and noticed the second door.
4. `osr_entry_frame_state` and the deopt frame state are two views of the same
   thing and are not checked against each other.

## The first increment

Write the contract down as *executable* checks, not prose:

* A newtype (or at minimum a debug assertion at each boundary) separating
  interpreter-bci space from output-pc space. Every one of the four items above
  is a coordinate confusion.
* An assertion, at publication, that `osr_pc_to_native`, `osr_dead_mask` and
  `osr_local_assignments` agree on length and on which entries are refused. A
  test where they disagree must fail.
* A single entry point for "produce an OSR-capable artifact", so the direct
  path in `invoke.rs` and the `try_compile` path cannot drift again. If that
  means the direct path grows a witness or a wrapper, say so and do it.

## How to verify

Two properties that can be tested without running Java:

* For every bci the transform refuses, the published entry is `-1` — asserted
  against the *mapping function* over a synthetic vector, not against whatever
  the emitter happened to place there. (The version of this test that asserted
  against a real compile passed for the wrong reason and then failed for the
  wrong reason; see `docs/jit/loop-rewriter-wiring.md`.)
* The three vectors are index-compatible.

## What to refuse

An OSR entry for a bci whose frame state cannot be reconstructed exactly.
Over-refusal costs an optimisation; under-refusal re-runs loop iterations or
resumes with the wrong locals.
