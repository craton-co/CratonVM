# round-10 `deopt2` lane — proposals and follow-up directions

**Filed:** 2026-09-21, round 10, lane `deopt2` (W4b).
**Scope:** `jit/src/deopt.rs`, `jit/src/osr_entry.rs`, `jit/src/osr_exit.rs`,
`jit/src/osr_coords.rs`, `jit/src/osr_contract.rs`, `jit/src/x64/osr.rs`,
`jit/src/x64/deopt_stubs.rs`.

This lane continued a sweep an earlier `deopt2` agent started and died on a
rate limit after three findings (the frame-scan-misses-monitors GC-root bug,
the exceptional-deopt-routed-to-the-wrong-stash bug, and the
`ReasonAtBci::Ambiguous`-picks-first-in-list-order bug — all already landed on
this branch). This document is the required "propose directions" writeup: one
concrete fix landed this pass (below), several suspected shapes were run down
and REFUTED as not bugs (also below, so nobody re-opens them), and a handful of
structural proposals for future rounds close it out.

## What landed this pass

**`OSR_REFUSE_INLINED_SCOPE` and `OSR_REFUSE_UNDESCRIBABLE_SLOT` were missing
from `OSR_COMPILE_STATE_REFUSAL_TAGS`** (`jit/src/osr_entry.rs`). Full
reasoning is inline at the constant's doc comment; summary: both refusals are
reachable ONLY through `deopt_points` / `osr_entry_frame_state`, which is
exactly the profile/speculation-shaped metadata the four already-listed tags
(`OSR_REFUSE_UNCONDITIONAL_TRAP`, `OSR_REFUSE_UNRESUMABLE_EXIT`,
`OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE`, `OSR_REFUSE_CONTRACT_DISAGREEMENT`) were
already classified as depending on. Before the fix, a refusal under either tag
was memoed with `install_epoch: None` — permanent for the life of the process,
surviving a code-cache flush that would otherwise have produced a fresh
artifact without the problem (different inlining decisions, different register
allocation). This is the round's FOCUS item 6 ("a cached OSR body that
outlives the speculation it was compiled under... on a code-cache flush")
applied one layer up: not the artifact outliving its speculation, but a
*refusal about* an artifact outliving it. Covered by a new test file,
`jit/tests/r10_deopt2_osr_compile_state_tags.rs` (a new external test crate
file, since `jit/src/tests.rs` is off-limits to this lane).

No other confirmed, fixable defect was found in this sweep. The owned files
are unusually well-hardened — the module docs read like a changelog of
already-fixed miscompiles for nearly every FOCUS shape the round brief lists,
each with its own regression test. That is a description of the code as
found, not a caveat on the review: the rest of this document is what a careful
pass through it actually turned up.

## Claims refuted this pass — do not re-open

### 1. `x64_deopt_entry`'s unused `epoch_guard` parameter is not a bug

`jit/src/deopt.rs::x64_deopt_entry` takes an `epoch_guard: *const DeoptEpochGuard`
and immediately does `let _ = epoch_guard;`, discarding it. This looks exactly
like FOCUS item 6's shape ("what happens on a code-cache flush... between
entry and exit") at first read. It is not a live gap: the doc comment at the
call site explains that the staleness short-circuit this parameter used to
feed was deleted *along with* the only mode that needed it
(`CRATONVM_JIT_FREE_CODE=1`, which freed compiled-code boxes out from under
still-running frames). Under the current retirement discipline a trapping
frame's own artifact is still alive by construction (an executing artifact
owns its deopt boxes until no thread can be inside it), so the box is always
valid and a superseded epoch only versions the SPECULATION, not the frame
layout being read. Reinstating the short-circuit was tried and reverted for a
recorded reason (a stale-guard short-circuit stashed an identity-less
`bci == u32::MAX` sentinel that forced every post-supersession trap onto the
imprecise whole-method re-run — the `jit-invokedynamic-groovy-regression`
fix). The parameter stays in the ABI only because the stub still passes it.
**Do not restore a staleness check here.**

### 2. The x64 single-pass backend never recording `relock: false` `MonitorInfo` for a real (non-scalar) `synchronized` block is by design, not a gap

`jit/src/x64/deopt_stubs.rs::build_frame_state_at`'s "Phase C" only emits
`MonitorInfo` entries from `sr_monitor_at` (scalar-replaced, always
`relock: true`); there is no path in this file that records a REAL, held
monitor with `relock: false`. That looked like a FOCUS item 7 candidate ("are
monitors correctly re-locked/released when a frame is rebuilt") — specifically
a lock leak on exception unwind through a freshly-synthesized interpreter
frame that never itself executed the `monitorenter`.

It is not a gap, because monitor ownership in this VM is NOT per-frame at all:
`vm/src/runtime/interpreter/deopt_resume.rs`'s own comment states it plainly —
*"the MonitorTable IS the interpreter's record of block monitors, there is no
per-frame list"*. A real monitor the compiled code locked through the monitor
helper is already reflected in the thread-global `MonitorTable`
(`shared.threads.monitors`) at the moment the helper ran; nothing about
resuming into a new interpreter frame needs to re-derive that fact, and
un-locking on exception unwind or normal exit reads the same global table
rather than a per-frame list. `relock: false` `MonitorInfo` entries DO exist
elsewhere (`vm/src/runtime/deopt_materialize.rs`, the optimizing tier's
`deopt_resume.rs` fixtures) but their only consumer-side effect is a no-op
continue in the `for &(obj, depth, relock) in &monitors_fwd { if !relock {
continue; } ...}` loop — i.e. recording one is optional documentation, not
a correctness requirement, for a backend that has no other reason to produce
one. **Do not add `relock: false` monitor recording to the x64 single-pass
backend to "fix" this.**

One legitimate residual question this left open (see Proposal 3 below): is the
real monitor's object still reachable as a GC root during the narrow window
between `resolve_frame_state_machine` (which does not walk any global monitor
table) and the frame being fully installed, for a monitor object that is *not*
otherwise referenced by any live local/stack slot at the trap? This was not
resolved in this pass — `vm/src/memory/gc.rs`'s root-scan coverage of
`MonitorTable` is outside every file this lane owns, and confirming it needs
a lane that owns the GC root-scan code.

### 3. `materialize_virtual_objects` in `jit/src/deopt.rs` is dead by design, not an unreachable uncommon trap

FOCUS item 5 asks about "an uncommon trap with no sink that can resume it."
`deopt::materialize_virtual_objects` (and its `_impl`) looked like a candidate:
its own doc says it is *"NOT WIRED TO A LIVE DEOPT PATH"* and returns
placeholder addresses in test builds. It is not the FOCUS shape, though — it
is explicitly gated to test builds in production
(`VirtualObjectMaterializationError::GcMaterializerUnavailable` otherwise), it
is never called from a real deopt path (only from its own unit test), and the
REAL materializer that a live deopt path actually calls is
`vm/src/runtime/deopt_materialize.rs::materialize_virtual_objects` — a
different function, owned by a different lane, that does the real
GC-coordinated allocation. The `jit/src/deopt.rs` copy is inert scaffolding
kept for its slot-index-extraction logic and its own test; nothing routes a
real trap through it. **Leave it as documented.**

## Proposals for a future round

### 1. Fold `OSR_PERMANENT_REFUSAL_TAGS` / `OSR_COMPILE_STATE_REFUSAL_TAGS` into one table

The bug this pass fixed happened because "is this refusal artifact-pure" and
"does its artifact-pure answer also depend on compile-time state that a flush
can change" are two independent booleans per tag, kept as two separately
hand-maintained `[&str; N]` arrays that a reader has to cross-reference by eye.
A third tag with the same shape (reachable only through `deopt_points`) added
in a future round can trivially repeat exactly this gap — appearing in the
first array and being forgotten from the second, because nothing enforces the
implication "every compile-state tag must be a permanent tag" other than a
test that already existed and did not catch THIS gap (it only checks the
listed compile-state tags against the permanent list, not the reverse: it
never asked "should a permanent tag that reads `deopt_points` also be here").
A single `enum` or a table of `(tag, Permanence)` where `Permanence` is
`NotMemoable | Permanent | CompileStateDependent` would make "compile-state
dependent but not permanent" a type error instead of a silent
`osr_refusal_depends_on_compile_state` under-count, and would make it
mechanical to audit: grep every `OSR_REFUSE_*` construction site for whether
its guard condition reads `deopt_points`/`osr_entry_frame_state`, and check
that classification matches the table.

### 2. A structural cross-check: "does this refusal's guard read `deopt_points`?"

Related to (1) but sharper: every `osr_refusal(TAG, ...)` call site in
`osr_entry.rs` could be audited (by a test, or by a doc-comment convention
checked in review) for whether the code path leading to it consults
`self.deopt_points` / `osr_entry_frame_state` / anything else gated by
`deopt_real_enabled()`. If it does, the tag MUST be in
`OSR_COMPILE_STATE_REFUSAL_TAGS`. This pass answered that question by hand for
the existing six tags; a lint or a test that walks the call sites structurally
(even just a `grep`-based CI check, since the actual data-flow analysis is
overkill here) would catch a regression cheaply.

### 3. Confirm — or close — the real-monitor GC-root-during-the-transient-window question from refuted claim 2

Not a confirmed bug (this lane could not read `vm/src/memory/gc.rs`'s
`MonitorTable` root-scan coverage without exceeding its file ownership, and
did not attempt an edit there), but worth a dedicated look by whichever lane
owns GC roots: does a *root scan* that runs strictly between
"`reconstruct_frame_from_machine_state` captured a real monitor's object
address" and "the object becomes reachable again through the reinstalled
interpreter frame's locals/monitor bookkeeping" see that object as a root
through `MonitorTable`, for a monitor object that is not independently
referenced by any live local or operand-stack slot at the trap bci (e.g.
`synchronized(makeLock()) { ...trap here... }` where the temporary is never
stored)? If `MonitorTable` is unconditionally part of every root scan (which
the deopt-resume comment strongly implies — "the MonitorTable IS the
interpreter's record of block monitors" — the implication being that IT, not
any per-frame list, is what keeps such objects alive), this is nothing; if
there is any root-scan path that skips it (e.g. a fast conservative scan that
only walks frames), it is a real use-after-free window and belongs in
`docs/known-issues/jit/`. This lane files it here as an open question rather
than a known issue because it could not verify either way without reading
outside its ownership boundary, and does not want to manufacture a defect
report it cannot back with evidence.

### 4. `has_elided_monitor` is a whole-method flag; a per-deopt-point version would recover admissible OSR entries

`x64/driver.rs` sets `cm.can_deopt_resume = !cm.deopt_points.is_empty() &&
!compiler.has_elided_monitor` and the mirror for `can_osr_exit` — both
whole-artifact gates. A method with TWO independent `synchronized` regions,
one of which had its lock elided by escape analysis and one of which did not,
has every one of its deopt points refused resume, including ones nowhere near
the elided lock's live range. This is over-refusal (safe, not a soundness
bug — matches the codebase's stated preference for refusing over guessing),
so it is a performance proposal rather than a correctness finding: track
`has_elided_monitor` per-deopt-point (the emitter already knows the bci range
an elided lock's scope covers via `sr_monitor_at`) rather than per-method, and
gate `can_deopt_resume`/`can_osr_exit` per point instead of once for the whole
artifact. Left as a proposal rather than attempted here because it is a
`can_deopt_resume`/`driver.rs` change outside this lane's owned files.

## Verification note

Every claim above was checked by reading, not by running `cargo test` or
`cargo build` — this lane's hard rules forbid any build/test invocation (the
parent orchestrator builds once for all five concurrent lanes). The new test
file (`jit/tests/r10_deopt2_osr_compile_state_tags.rs`) has been checked
against the actual public signatures it calls
(`cratonvm_jit::{osr_refusal_depends_on_compile_state, osr_refusal_is_permanent,
OSR_COMPILE_STATE_REFUSAL_TAGS, OSR_PERMANENT_REFUSAL_TAGS,
OSR_REFUSE_INLINED_SCOPE, OSR_REFUSE_UNDESCRIBABLE_SLOT,
OSR_REFUSE_PC_NOT_AN_ENTRY}` and `cratonvm_jit::bailout::{Bailout,
BailoutReason}`, all confirmed `pub` and glob-re-exported from `jit/src/lib.rs`)
but has not itself been compiled. The orchestrator should build it along with
the rest of the round.
