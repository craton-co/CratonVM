# OSR-01 — the entry metadata contract

**Status:** not started as a lane; several one-off fixes have landed around it.
**Owns:** `../../../jit/src/lib.rs` (the OSR region), `../../../jit/src/x64.rs` (the OSR
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
3. The OSR compile path in `../../../vm/src/runtime/interpreter/invoke.rs` calls the
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
  wrong reason; see `../../jit/loop-rewriter-wiring.md`.)
* The three vectors are index-compatible.

## What to refuse

An OSR entry for a bci whose frame state cannot be reconstructed exactly.
Over-refusal costs an optimisation; under-refusal re-runs loop iterations or
resumes with the wrong locals.
