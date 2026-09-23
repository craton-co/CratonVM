# Round 10, lane `offsetkey`: proposals

**Lane:** round 10 wave 8, `offsetkey`.
**Owned:** `jit/src/osr_exit.rs`, `jit/src/x64/loop_rewrite.rs`.
**Executed:** `rustfmt --check --edition 2021` on both owned files plus the new
test (clean), and `scripts/check-orphan-instruments.sh` (exit 1, one name — see
§4). **Nothing else was run**: no `cargo build`, `test`, `check` or `clippy`, no
regression suite, no probe. Every claim below that is not attributed to one of
those two commands is from reading, and says so.

Ordered by what a reviewer should look at first, not by size.

---

## 1. Teach the orphan gate about verdict functions (blind spot 4)

**STATUS 2026-09-22: still a proposal, deliberately.** The instance that
motivated it is closed (see §2's status and the retired page below), but C4
itself needs the scope-aware same-file exclusion this section argues for and
`docs/ci/orphan-instrument-gate.md`'s wave-6 false positive
(`note_long_long_value_direct_site`) also wants. Landing C4 on top of a
same-file rule that is wrong for a 30,000-line `lib.rs` produces a census
nobody can read, which is what the third bullet below predicts.

**Problem.** `docs/ci/orphan-instrument-gate.md` records three blind spots. Lane
`offsetkey`'s largest finding fell outside all of them, and outside all three
checks, for a reason that is structural rather than incidental:
`jit/src/osr_exit.rs::exceptional_reason_at_bci` is a `pub fn` with no production
caller anywhere, but it takes two arguments, returns an enum, and touches no
atomic. C1 wants `record_*`/`note_*`; C2 wants zero arguments, an integer-ish
return and an atomic; C3 wants an `*_EVENTS` row.

The gate's framing is "an instrument nobody reads cannot warn anyone". The class
is wider than instruments. A **verdict function** — a refusal, a classifier, an
admission test — with no production caller is the same hazard with the same
signature: it reads as a guard that is in place and holding, and it has never run.
`docs/internal/retired/r10-offsetkey-exceptional-reason-at-bci-is-unwired-and-cannot-refuse-20260921-RETIRED-20260922.md`
is the instance.

**Proposal: a check C4.** `pub fn NAME(...)` in a `jit/src` or `vm/src` production
region whose return type is an enum declared in the same crate *and* which has no
call site outside its defining file. Deliberately narrow, because the false-positive
risk is the whole question:

* **Enum return only.** A verdict is a decision between named outcomes. Restricting
  to a same-crate enum return excludes the large population of `pub fn` helpers
  returning `bool`, integers, `Option<T>` and collections, which are ordinary API
  and would flood the census.
* **The same-file exclusion must be scope-aware before C4 ships**, not after. This
  is the wave-6 `note_long_long_value_direct_site` false positive, and C4 will hit
  it harder than C1/C2 do: verdict functions cluster in large modules. The remedy
  is the one `docs/feature-designs/jit-r10-wrappers-proposals.md` already argues
  for from the other side — distinguish a reference inside the defining item or a
  `#[cfg(test)]` block from one in an unrelated function in the same file. C4
  should be the change that finally pays for that scope awareness, since it needs
  it to be usable at all.
* **Expect a large initial allowlist and freeze it in a separate commit** from the
  check itself, so the census is reviewable as a census.

**Cost if not done.** Three independent mechanisms missed one item
(`check-orphan-instruments.sh` by shape, `vm/tests/no_test_only_public_api.rs` by
crate scope, `rustc`'s `dead_code` by `pub` + own-test reference). Whether there
are more like it is currently unknown, and "unknown" is the answer C4 would change.

## 2. Extend `no_test_only_public_api.rs` beyond `vm/src`

**STATUS 2026-09-22: DONE.** `vm/tests/no_test_only_public_api.rs` now runs its
three passes a second time with `jit/src` supplying the declarations, under
`BASELINE_OFFENDERS_JIT` and `MIN_DECLARATIONS_SCANNED_JIT`. It is a SECOND scan
with its own baseline rather than an addition to the `vm` declaration set, which
is the one place this section's plan was changed: merging the two maps would
have made the cross-crate collision leniency worse in a way invisible from the
count, because `decls.entry(name).or_insert(..)` keeps the first declaration of
a bare name and a `jit` item masked by a same-named `vm` item would then never
be reportable at all. The `vm` baseline is untouched at 281. See
`docs/internal/retired/r10-offsetkey-exceptional-reason-at-bci-is-unwired-and-cannot-refuse-20260921-RETIRED-20260922.md`.

**Problem.** That ratchet's offender condition — "no production reference beyond
its own declaration *and* at least one reference from test code" — describes
`exceptional_reason_at_bci` exactly. It does not fire because the scanner's
declaration pass reads `vm/src` only, while its *reference* pass already reads
every workspace member. So the machinery to cover `jit/src` is largely present;
what is missing is the declaration sweep.

**Proposal.** Add `jit/src` to the declaration pass, freeze the resulting offender
set as a baseline in the same commit, and expect that baseline to be substantial —
`jit` is the crate this round has been auditing, and it exports a wide surface to
`vm`. Two cautions carried over from that file's own header, both of which apply
more strongly to `jit`:

* the "cross-crate name collisions make this LENIENT" limitation gets *worse*, not
  better, with more crates in the declaration set — a `jit` item masked by a
  same-named `vm` item is a new miss;
* `jit`'s public surface is genuinely consumed by `vm` at a scale `vm`'s is not
  consumed by anything, so a high offender count is expected and is not evidence
  the change is wrong.

Lower confidence than §1 on cost/benefit, and worth doing second: §1's C4 catches
a class no ratchet covers, whereas this extends a ratchet that already works.

## 3. Split `loop_xform_deopt_frames_diverge` into kind and shape

**Problem.** `x64::loop_rewrite::deopt_point_difference` classifies four different
disagreements between two copies of one bytecode as
`PointDifference::Divergent`, and one counter records all four:

| disagreement | what it means |
|---|---|
| a slot's `OsrSlotType` differs | the documented, expected case — `IndyDeoptProbe.concatLoop`'s `Register` vs `RegisterRef` for local 3, a forward-dataflow artefact of unrolling |
| `locals.len()` differs | — |
| `stack.len()` differs | the emitter's simulated operand stack disagreed between two images of ONE bytecode |
| `monitors.len()` differs | likewise for monitor depth |

The metric row's comment says "Non-zero is normal", and for the first row that is
true and measured. For the last three it is a much stronger claim than the evidence
supports: two copies of the same bytecode are at the same abstract stack depth and
the same lock depth by construction, so a count difference says the replication
desynced — a rewriter defect — not that the dataflow widened a type.

Both are *safe*, and that is why they were grouped: `try_osr_entry` re-verifies
every slot expectation, and the depth disagreement is exactly what
`OSR_REFUSE_OPERAND_STACK` reports. The problem is not soundness, it is that a
defect signal is being summed with an expected one under a row labelled "normal".
A rewriter bug that desyncs the operand stack would show up as a slightly larger
number in a counter nobody investigates.

**Proposal.** Two rows: keep `loop_xform_deopt_frames_diverge` for the slot-KIND
case (where "non-zero is normal" is the measured truth) and add
`loop_xform_deopt_frames_disagree_on_shape` for the three count cases, documented
as expected-zero. `PointDifference::Divergent` would carry which of the two it is.

**Why not done here.** It needs a new row in `jit/src/metrics.rs`, which this lane
did not own, and `record_loop_xform_event` silently ignores an undeclared name — so
the `loop_rewrite.rs` half alone produces a counter that no-ops forever. Same
constraint as
`docs/known-issues/jit/r10-offsetkey-provenance-not-total-refusal-has-no-counter-20260921.md`,
and the two changes should land together since they touch the same table.

## 4. One reader for `unsafe_accessor_census` (gate is red on it now)

`scripts/check-orphan-instruments.sh` exits 1 on this tree with one name:
`jit/src/lib.rs:11229::unsafe_accessor_census`. Verified by hand rather than taken
on the gate's word, because a C2 hit in `lib.rs` is the case the gate's same-file
blind spot makes unreliable — `rg` returns exactly one line workspace-wide, the
definition. Its five counters all have production feeders. Four lines in
`vm-cli/src/main.rs` fix it, and it must not be allowlisted; full write-up and the
exact patch in
`docs/known-issues/jit/r10-offsetkey-unsafe-accessor-census-has-no-reader-20260921.md`.

## 5. Fuse the two admission-time walks over `deopt_points`

**Problem.** `jit/src/osr_entry.rs`'s OSR admission calls
`osr_exit::has_reason_ambiguous_bci` and then
`osr_exit::first_ambiguous_resume_bci` back to back (lines 1009 and 1012). Each
builds its own `distinct_bci_images` iterator, which is quadratic by construction —
a `Vec<u32>` of seen bcis scanned with `contains`, and a fresh `resume_image`
rescan of the whole point list per distinct bci. So the same O(n²) walk runs twice
per admission to answer two questions about one traversal.

`distinct_bci_images`' own doc already concedes the cost and argues it is
acceptable: "artifacts carry tens of points and this runs once per OSR admission,
not per iteration. A map would be faster and would allocate on a path that today
does not." That argument is sound for the complexity and silent about the
duplication, which is the cheaper thing to fix.

**Proposal.** One function in `osr_exit.rs` returning both answers from a single
walk — `fn resume_image_survey(&[DeoptimizationPoint]) -> (Option<(u32, usize, usize)>, bool)`
or a small struct. The allocation profile is unchanged (one `Vec<u32>` instead of
two), and the caller reads more honestly: the two facts come from one traversal of
one immutable list, so they cannot disagree about what they saw.

**Why not done here.** The caller edit is in `jit/src/osr_entry.rs`, which lane
`offsetkey` did not own, and landing the fused function without its caller would
add an API with no production caller — the defect this round has found ten times.
The two halves must land together. **Lowest-value item on this list**, and listed
only because it is a real duplicated walk on a real path: admission runs once per
OSR entry, artifacts carry tens of points, so the measured saving is likely
unobservable. Do it for the readability, or not at all.

---

## Not proposed, and why

* **Tightening `DeoptMetadataError::DeoptPointsUnsorted` to `>=`.** Already argued
  down in that variant's doc and in
  `docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md`.
  It would refuse a well-formed artifact to prove a hypothesis, and the verifier
  cannot tell a legitimate same-offset pair from a mistaken one.
  `jit/tests/r10_interner_deopt_point_keys.rs::the_sortedness_lane_still_tolerates_the_equal_pair`
  fails if someone reverses it.
* **Wiring `CompiledMethod::find_deopt_point` into anything.** The order of work,
  if a consumer ever needs an offset-keyed lookup (a signal-delivered trap at a pc
  with no baked pointer is the plausible one), is fixed and is not negotiable by
  convenience: make the three recorder families agree on one identity first — a
  single `recorded_at_pc` set, or a `(pc, reason)` key — then make the verifier
  enforce it, then write the lookup.
  `jit/tests/r10_offsetkey_deopt_point_identity.rs::find_deopt_point_still_has_no_production_call_site`
  makes wiring it a visible decision.
* **Deleting `exceptional_reason_at_bci`.** A defensible call, and the orchestrator's
  rather than this lane's; the trade-off (two useful assertions lose their second
  half) is set out in that function's known-issues page rather than decided here.
  **Decided 2026-09-22: not deleted, WIRED.** The sink at
  `vm/src/runtime/interpreter/deopt_resume.rs` was already asking this
  function's exact question through an inline `any(..)` that duplicated the
  predicate, so "it would compute the same answer by a longer route" is the
  reason the wiring is free rather than a reason against it. The tests keep
  their second half and the item stops being a verdict function with no
  production caller.
* **Giving `LoopRewriteRefusal::ProvenanceNotTotal` a `tally` call now.** It would
  silently no-op until `metrics.rs` declares the row, which is the same defect in
  new clothing. See §3's constraint.
