# Diagnostics/observability proposals — round 10, lane `obs`

**Status:** PROPOSALS ONLY. Written at the close of a hard review of
`jit/src/metrics.rs`, `jit/src/jfr_compile_decision.rs`, `jit/src/code_events.rs`,
`jit/src/bytecode_analysis.rs`, `jit/src/gdb_jit.rs`, `jit/src/jitdump.rs`,
`jit/src/perf_map.rs`, `jit/src/platform.rs`, `jit/src/layout_const_inventory.rs`,
`jit/src/offload_hook.rs`, `jit/src/gpu_barrier.rs`, `vm/src/jit/disasm.rs` and
`jit-api/src/lib.rs`. Nothing below was built or measured; each item states its
own cost.

## 1. A compile-time check that every `record_osr_event`/`record_loop_xform_event`
   call site names a real row

**Where:** `jit/src/metrics.rs` (`OSR_EVENTS`, `LOOP_XFORM_EVENTS`,
`record_osr_event`, `record_loop_xform_event`), plus every call site across
`vm/src/runtime/interpreter/jit_bridge.rs`, `jit/src/osr_entry.rs`,
`jit/src/x64/driver.rs` and `jit/src/x64/loop_rewrite.rs`.

**What is true today.** This round found exactly the bug class the round is
looking for: `jit_bridge.rs`'s optimizing-tier OSR door recorded
`"osr_optimizing_artifact_reused"`, a string that had no matching row in
`OSR_EVENTS`, so `record_osr_event`'s `.position()` lookup silently found
nothing and the event was dropped on every call, forever (fixed this round by
adding the row — see the round's report). `record_osr_event` and
`record_loop_xform_event` both take a bare `&str` and both silently ignore an
unknown name **by design**, because they are called from hot paths (an
interpreter back-edge, a compile-time planner) where a panic on a typo would be
strictly worse than a dropped count. That design choice is correct for
*production*, but it means a typo introduced today has no feedback loop at all
short of a human noticing a counter that never moves — which is exactly how the
bug above went unnoticed.

**Proposal.** A `#[cfg(test)]` (or `debug_assertions`-only) integration test
that does NOT rely on grepping call sites (fragile, and this round's find came
from a manual cross-reference, not a mechanical one) but instead makes the
*runtime* check itself in test/debug builds: `record_osr_event` and
`record_loop_xform_event` gain a `debug_assert!` that the name resolves,
compiled out entirely in release (so the hot-path/no-panic argument for
production is untouched) but active under `cargo test` and any debug-assertions
build. A single `cargo test -p cratonvm-jit` run that exercises the OSR and
loop-transform paths (several integration tests already do) would then catch a
future rename or a new call site with a fat-fingered string the moment it is
compiled and run — turning "silently zero forever" into "panics on the very
first `cargo test`".

**Why it is worth doing.** The whole point of `OSR_EVENTS`/`LOOP_XFORM_EVENTS`
being a closed, named vocabulary — the module docs are explicit about this — is
that a sink can trust the row set. A string that does not resolve breaks that
trust invisibly, and the only reason this round's instance was found is that a
lane reviewer happened to diff the call sites against the array by hand. That is
not a repeatable safeguard.

**Cost.** Small. `debug_assert!(OSR_EVENTS.contains(&event), "unknown OSR event {event:?}")`
is one line per function; the assertion is free in release builds (the
condition and the `contains` call are both compiled out). The risk is a false
positive if a legitimate caller intentionally passes an ad-hoc string expecting
it to no-op (grep found none of this shape today), so the change should ship
with a full-repo `cargo test` before merging, which this lane could not run.

## 2. A generation counter on `GETFIELD_ARM_EMITS`/`INLINE_CALL_ARM_EMITS`/
   `IR_GETFIELD_DECLINE` etc. that flags a name/index drift automatically

**Where:** `jit/src/metrics.rs` (the `note_*_arm(usize)` family: `note_getfield_arm`,
`note_inline_call_arm`, `note_ir_getfield_decline`, `note_local_handler`, and
their `*_NAMES` arrays), plus every call site in `jit/src/x64/op_field.rs`,
`jit/src/x64/op_invoke.rs`, `jit/src/x64/inlining.rs`, `jit/src/x64/deopt_stubs.rs`
and `jit/src/ir_lower.rs`.

**What is true today.** Unlike `OSR_EVENTS`, these census tables are indexed by
a bare `usize` at the call site (`note_inline_call_arm(4)`), with the
correspondence to a name documented only in a doc comment
(`"Index: 0 = ..., 1 = ..."`). This round cross-checked every call site by hand
against the documented mapping and found them all correct, but the mechanism
has no structural protection against the next edit: reordering
`INLINE_CALL_ARM_NAMES` without updating every `note_inline_call_arm(N)` call
site (or vice versa) compiles cleanly and silently relabels every subsequent
count. `OSR_EVENTS`/`LOOP_XFORM_EVENTS`/`SCHEDULING_EVENTS` avoid this class of
bug entirely by taking the name itself (string lookup, not a raw index); the
five `usize`-indexed tables in the arm-census family do not.

**Proposal.** Give each of these families a small `#[repr(usize)] enum` (e.g.
`InlineCallArm { SplicedCallDirect = 0, SplicedCallDispatch = 1, ... }`) and
change `note_inline_call_arm` (etc.) to take the enum instead of `usize`. The
name array is then built from the enum via a `match` (or `strum`-style
`as_str`), so a reordering is a compile-time exhaustiveness question rather
than a silent index drift, and a call site names its intent
(`note_inline_call_arm(InlineCallArm::NestedSplice)`) instead of a magic
number a reader has to look up in a doc comment three screens away.

**Why it is worth doing.** These are exactly the counters the round's FOCUS
calls out — "a metric whose name no longer matches what it counts" — and the
current mechanism has no defense against that drift beyond a reviewer
re-deriving the mapping from the doc comment, which is what this round did
manually. An enum makes the compiler do that cross-check on every future edit.

**Cost.** Moderate and entirely mechanical: five call-site families
(`GETFIELD_ARM_EMITS`/6, `INLINE_CALL_ARM_EMITS`/7, `IR_GETFIELD_DECLINE`/7,
`LOCAL_HANDLER_COUNTS`/6, `CALL_SPILL_COUNTS`/7, `RECEIVER_DESPEC_COUNTS`/4,
`SPILL_WIDTH_COUNTS`/3) across roughly two dozen call sites in files this lane
does not own (`x64/op_field.rs`, `x64/op_invoke.rs`, `x64/inlining.rs`,
`x64/deopt_stubs.rs`, `ir_lower.rs`, `x64/objects.rs`), so it is cross-lane work,
not a single-file change. No behavior changes; the JSON/census output is
unaffected since the name arrays already exist and would just move from a
parallel array to an enum's `Display`/`as_str`.

## 3. Emit a `JIT_CODE_MOVE` (or equivalent) jitdump record when a block is
   recycled by the code arena

**Where:** `jit/src/jitdump.rs` (`record_load`, which only ever emits
`JIT_CODE_LOAD`), `jit/src/platform.rs` (`JitCodeArena::release`, which — per
the arena's own `REVIEW-NOTE 10` — now flips a recycled block back to `RW`
before returning it to the free list), `jit/src/code_events.rs` (`publish`,
which fires again when the recycled block is reused for a new body).

**What is true today.** jitdump version 1, which is all this module speaks, has
no unload record, and the module doc is explicit that this is by design:
"Freed and reused addresses are resolved by perf from the record timestamps."
That is sound for `perf record`'s own post-hoc resolution, which does exactly
this. It is not sound for every OTHER consumer of a jitdump file: a `perf
inject --jit` pass that pre-builds a merged ELF image for `perf report
--symfs` bakes in the LAST `JIT_CODE_LOAD` at a given address as of injection
time, and two live bodies recorded at the same address (one retired, one
freshly compiled into the recycled block) are today indistinguishable from a
single long-lived body that happened to get two load records — there is
nothing in the file that says "the first one died". This is not a bug (the
format has no better answer, and the module's own docs say so), but it is a
place where the JIT's own code-arena recycling (new this round's sibling work
on `platform.rs`) makes address reuse routine rather than occasional, and a
denser recycling pattern makes the ambiguity bite sooner.

**Proposal.** `perf`'s jitdump reader is documented to accept an unbounded
`JIT_CODE_LOAD` stream and resolve by nearest-preceding-timestamp, so nothing
here is actually actionable without a version-2 jitdump extension upstream
(there is no unload record to emit into a v1 file). The concrete, buildable
half is: have `code_events::retire` (today GDB-only, by design, since perf and
jitdump have no take-back record) additionally count retirements-under-arena-
reuse in a new `jitdump_addr_reuse_total` counter in this file, so a person
staring at a confusing `perf report` symbol has a number to check ("has this
address been recycled N times in this run?") instead of having to guess from
the dump's raw record count.

**Why it is worth doing.** Cheap, and it turns a silent ambiguity into a
visible one — which is this round's whole mandate. It does not fix the
ambiguity (that needs an upstream format change this crate does not control).

**Cost.** Small: one counter, one increment at the one call site in
`code_events::retire` that already knows whether the retiring range was an
arena block (via `BlockOrigin`, which is `platform.rs`'s and not visible from
`code_events.rs` today, so the increment would need to move to
`JitCodeArena::release` or take a bool parameter through `retire`).

## 4. `CompilationReport::planned_spills`/`planned_reloads` producer wiring

**Where:** `jit/src/metrics.rs` (`CompileRecorder::set_planned_spills`,
documented at length as having **no production caller** on any path today),
`jit/src/ir_lower.rs` (`plan_register_residency`, the only function that holds
an `Allocation` and could supply the numbers).

**What is true today.** This is not this lane's bug to fix — `ir_lower.rs` is
owned by another lane, and `metrics.rs`'s own module doc already documents the
gap in exhaustive, self-aware detail (down to naming the deleted function that
used to almost-wire this and why it was wrong to keep). It is included here
because it is precisely the failure class the round's FOCUS names — a producer
that does not exist, so the field reads `NotMeasured` on literally every
compile — and it has been sitting unaddressed since before this round.

**Proposal.** Restated from the module doc's own open question, for the record:
`plan_register_residency` needs either (a) a `CompileRecorder` threaded down
through `lower_inner_with_scopes`, (b) the two numbers carried back up in
`RegResidency` and reported by the existing pinned-signature ambient hook, or
(c) a third `note_current_*` ambient pair mirroring `note_current_spills`. The
module doc argues against (c) already (see `set_planned_spills`'s doc), leaving
(a) or (b) as live options for whichever lane next touches `ir_lower.rs`'s
register allocation wiring.

**Why it is worth doing.** Every reader of this report today sees
`planned_spills: null` unconditionally and has no way to learn how much of the
allocator's plan the backend actually executed — the exact gap
`R4`/`NOTES-regalloc.md` describes.

**Cost.** Not assessed here; it is inside `ir_lower.rs`'s register-allocation
machinery, out of this lane's owned-file set, and the module doc already states
the three candidate shapes without picking one.
