# Tier-up / profiling / compile-gate proposals — round 10, lane `tier2`

**Status:** PROPOSALS ONLY. Written at the close of a review of
`jit/src/tiered.rs`, `jit/src/profile.rs`, `jit/src/profile_store.rs`,
`jit/src/pgo.rs`, `jit/src/entry_counter.rs`, `jit/src/compile_gate.rs`,
`jit/src/compile_request.rs` and `jit/src/bailout.rs`. Nothing below was built
or measured; each item states its own cost and risk. An earlier incarnation of
this lane had already landed the `tier_fail_count`/`osr_fail_count` split and
the two missing `tiering_is_disabled()` checks in `jit/src/tiered.rs` before it
was cut off — this file is the continuation, not a repeat, of that sweep.

## 1. `request_osr` refuses on a method-entry failure that has nothing to do
   with the OSR artifact

**Where:** `jit/src/tiered.rs`, `CompilerCore::admit`'s `out_of_retries` for an
OSR task (`state.osr_fail_count >= MAX_TIER_FAIL_RETRIES ||
state.tier_fail_count >= MAX_TIER_FAIL_RETRIES`) and the identical pair
`request_osr` checks before building a task at all.

**What is true today.** The split this round's earlier pass made
(`tier_fail_count` for method-entry attempts, `osr_fail_count` for OSR
attempts, each cleared only by its own pipeline's publish) is asymmetric by
design, and the code says so: *"The OSR door still honours `tier_fail_count`
as well ... so the coupling that existed before is kept in the one direction
it was ever asserted in."* Concretely: three consecutive method-entry compile
failures permanently close the OSR door too, for the life of the process (or
until a class redefinition resets both counters). Three consecutive OSR
failures do NOT close the method-entry door — that direction was the actual
bug the split fixed.

The asymmetry is a real design question, not an oversight the split missed.
Method-entry and OSR compile the same bytecode through the same front end
(`ir_lower`, the same bailout categories in `jit/src/bailout.rs`), so a
front-end defect that fails method-entry compilation deterministically (a
`GraphTooLarge`, an `UnsupportedOpcode`, a verifier rejection) is very likely
to fail an OSR compile of the same method identically — in which case the
coupling costs nothing, because `osr_fail_count` would reach the same verdict
on its own in three tries. The case the coupling actually changes the outcome
is a method-entry-ONLY failure: something that differs between the two
compiles (different entry conventions, different live-in sets at the OSR
entry point, a resolver failure specific to method-entry's constant-pool
reads) while the loop body itself would compile and run fine via OSR. For that
population — which this lane did not and could not measure the size of —
today's rule denies the interpreter's single sharpest tool for a hot loop
(background OSR) as a side effect of a completely different code path's
failure.

**Proposal.** Measure first: instrument (temporarily, behind a debug flag)
how often `request_osr`'s `tier_fail_count >= MAX_TIER_FAIL_RETRIES` arm
actually fires while `osr_fail_count == 0` — i.e., OSR was never itself tried
and failed, only refused by the method-entry budget. A workload where that
count is near zero says the coupling is free and the current comment's
reasoning holds without qualification. A workload where it is not says the
population above is real, and the fix is to drop `tier_fail_count` from
`request_osr`'s and `admit`'s OSR arms entirely, leaving OSR gated only by its
own `osr_fail_count` — completing the split in both directions instead of one.
The risk of over-rotating (an OSR pipeline retried forever against a front-end
defect that method-entry has already proven permanent) is bounded by
`osr_fail_count`'s own three-strike budget, which is the same bound
method-entry already trusts for itself.

**Cost.** The measurement is small (one counter, one log line, gated off by
default). The fix, if the measurement supports it, is a two-line change
(`admit`'s OSR arm and `request_osr`'s early check both drop the
`tier_fail_count` disjunct) plus updating whichever existing test in
`jit/src/tiered.rs::tests` currently pins the coupled behavior (grep for
`tier_fail_count` beside `osr_fail_count` in the same assertion — this lane's
sweep found the coupling documented but did not find a test that exercises
*this specific* cross-pipeline refusal path with real fail-then-OSR sequencing,
so there may be nothing to update, only something to add).

## 2. A generation stamp on `MethodProfile`, matching `RuntimeDespecRegistry`'s

**Where:** `jit/src/profile.rs` (`MethodProfile`, `ProfileStore::record_branch`
/ `record_receiver` / `record_backedge` / `record_trip_complete`).

**What is true today.** Filed in full as
`docs/internal/retired/r10-tier2-profile-store-survives-class-redefinition-20260921-RETIRED-20260922.md`:
a class redefinition purges every tiering verdict
(`TieredCompilationManager::on_class_redefined`) but not the interpreter
profile, because `ProfileStore` is a sibling component with no redefinition
hook at all, and `class_id` — the profile's key — is stable across a
redefine by construction. The known-issue page's suggested fix is the cheap
one: call `ProfileStore::invalidate_class` from the same five VM call sites
that already call `on_class_redefined`, dropping the WHOLE profile for the
redefined class.

**Proposal (the finer-grained alternative, for later).** Dropping the whole
profile means a hot method that keeps the same shape across a redefinition
(the overwhelmingly common case — most redefinitions touch a handful of
methods, not every call site's receiver mix) re-warms its profile from zero
for no reason. `RuntimeDespecRegistry` (`jit/src/compile_gate.rs`) already
solves the general version of this problem for a different verdict:
`RuntimeDespecStamp = (redefine_epoch, install_epoch)`, and a read compares the
stamp under which a verdict was recorded to the epoch pair *now*. The same
shape applied to `MethodProfile` — a `recorded_epoch: u32` set at first use
(mirroring the field, not the type, since a profile has no separate
"install"-epoch dependency the way a runtime despec verdict does) and checked
on read — would let a profile survive a redefinition of an UNRELATED class
(the common case: nothing changed about `crate::redefine_epoch()`'s scope for
this class) while still going stale the instant its OWN class's bytecode
changes.

**Why this lane did not attempt it.** `record_branch_borrowed` /
`record_backedge_borrowed` are this file's own module doc's example of a path
"AUDIT CRIT-3/CRIT-5/HIGH-7" already found expensive enough to gate behind
`PROFILING_ENABLED` and shard sixteen ways; adding an epoch compare to every
recording call is exactly the kind of hot-path change that comment's opening
paragraph says must be MEASURED, not asserted, and this lane cannot run a
benchmark. The known-issue's call-site fix (`invalidate_class`, already
public, already tested) is strictly safer and should land first regardless.

**Cost.** One `AtomicU32` per `MethodProfile` (cheap), one relaxed load on
every recording call (needs measurement — the whole reason this is a proposal
and not this round's fix), and the read-side check in `get_profile` /
`snapshot_all` needs the same "or_insert" race `RuntimeDespecStamp` doesn't
have to worry about (a profile is mutated far more often than a despec
verdict, so "stale, reset in place" needs its own small state machine rather
than a bare compare).

## 3. `pgo.rs` should be deleted, not merely narrowed

**Where:** `jit/src/pgo.rs` (whole file), `jit/src/lib.rs:191` (already
`pub(crate) mod pgo;` — the narrowing this file's own top-of-file REVIEW-NOTE
asked for has landed since the note was written; this lane updated the note in
place to say so rather than re-file it).

**What is true today.** The module's own doc is unambiguous: zero production
callers, every counter permanently zero, and a data model that actively
disagrees with the live `crate::profile` store on receiver-table capacity and
counter width (documented in the module's own "TWO PROFILES" table). It is
1 997 lines of serialization format and a profile model that
`docs/jit/pgo-inlining.md` §1 keeps as a design sketch.

**Proposal.** Now that visibility is `pub(crate)`, the remaining risk the
module posed to an OUT-of-tree consumer is gone; what is left is an in-tree
maintenance cost (1 997 lines a future contributor might read, half-understand,
and extend by accident, exactly the shape the module's own guard test
(`pgo_is_named_only_by_comments_outside_this_module`) exists to catch one step
too late — after the edit, not before it). If the serialization format is worth
keeping as a reference, it belongs in `docs/jit/pgo-inlining.md` as a worked
example (a format spec in prose plus the struct definitions, not 1 997 lines of
compiled Rust with its own test suite that must keep passing for content
nothing reads). Deletion needs the four-site cleanup the module's own header
comment already enumerates (the `pub mod pgo;` line — done — plus
`profile.rs` lines 294/2592, `ir_schedule.rs:119`, and the doc file), which is
exactly what `pgo_is_named_only_by_comments_outside_this_module` checks, so the
test that guards against silent adoption today would double as the deletion
checklist tomorrow.

**Cost/risk.** Low technical risk (the module's own test proves nothing
outside it depends on the types), but it is an architectural call about
whether the design-sketch value outweighs the maintenance cost, which is why
this stays a proposal rather than this round's fix — this lane owns the file
but not the decision to remove a design reference wholesale.
