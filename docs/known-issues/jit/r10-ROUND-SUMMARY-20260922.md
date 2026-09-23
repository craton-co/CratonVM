# Round 10 (JIT design and performance) - what closed, what is deliberately still open

**Ran:** 2026-09-20 to 2026-09-22, eight waves, on `jit/perf-round-20260920`,
merged to `dev` after each wave.
**Shape:** review lanes investigated and wrote code but never built; the
orchestrator owned every build, probe, suite run and merge, and enforced disjoint
file ownership so no two lanes could touch the same file in one wave.

## The one result worth leading with

**The same defect was found independently by twelve lanes**: an instrument, API
or code path with **no production caller** - it reads zero forever, or can never
fire, and zero is indistinguishable from "the thing it watches never happened".
It appeared as a counter nobody reads, a `record_*` nobody calls, a metric row
whose name no call site passes, a verifier lane nothing arms, a relock path no
analysis can reach, and a whole 2065-line interned representation no producer
builds.

After the fifth instance, instance-hunting was abandoned for a **mechanical
ratchet**: `scripts/check-orphan-instruments.sh`, a frozen allowlist, and
`docs/ci/orphan-instrument-gate.md`. It has since made its own catches, including
two `gc/src/g1.rs` counters that arrived from another session - caught at the
merge boundary by a gate rather than by a person reading a file. The allowlist
went 112 -> 103 over the round, and the last re-freeze added nothing for the
first time.

The gate has four known blind spots, all documented rather than quietly lived
with: it cannot see a same-file production caller (it produced a false positive
on a 30000-line `lib.rs`), it censuses no `pub static ...Atomic...`, check C2
skips `&self` methods (which hid the round's single largest instance), and a
`#[cfg(test)] pub fn` would still read as a candidate.

## Two gate scanners were themselves wrong, in the same way

Both the JIT's `panic_free_compile_ratchet.rs` and the VM's
`hot_files_have_no_production_panics` had too-narrow rules for what "opens test
code" means, and both produced confident wrong verdicts:

* The JIT one tested the boundary BEFORE dropping comment lines, so a `///`
  comment that merely MENTIONED the attribute truncated the scan. It reported
  `ir.rs: 5 -> 0 (all removed)` and invited lowering the frozen row - which would
  have discarded five real panic sites. Fixing it then exposed a second row whose
  frozen 9 was an artefact of the same bug; the comment responsible even said
  "frozen at the older tests' total", describing itself.
* The VM one matched only the literal attribute, so three
  `#[cfg(all(test, target_arch = "x86_64"))]` modules added on `dev` by another
  session were scanned as production and their test assertions reported as
  production panics. **That gate was red on `dev` for the whole round** until it
  was fixed here.

A gate that is red on a clean tree is a gate somebody switches off. Both are
green and neither frozen count moved.

## Lanes overturning the page they were given

This is the round's second-order result: a page is a hypothesis, not a work
order. Lanes disproved the brief they were handed at least eight times.

* A page's confirmation grep was printed with "(no other hits)" as captured
  output; running it shows the opposite. The counter had had a production reader
  for three and a half weeks before the page was filed - CLOSED as not a defect.
* A page's suggested fix would have introduced a regression (clearing a cursor
  unconditionally, when only one of its two refusal causes is permanent).
* A page's proposed key was wrong: splice DEPTH fails the page's own stated
  requirement, since two splices of one callee at two caller pcs are both
  depth 1.
* A page's suggested call could not be written at all - `jit` cannot depend on
  `vm`, so the data had to flow the other way.
* A "cheapest win" proposal mis-grouped `long +` with `&`/`*`; a different
  proposal was a TRAP that would have restored an arithmetically impossible
  reading with a producer behind it.
* A design doc's headline 88% saving turned out to be a CONSTRUCTION measurement
  from a test that derives snapshots; no producer derives, so the real figure was
  never knowable - and the representation was retired.
* The orchestrator relayed a publisher count of "four" without checking it; it
  was wrong in both directions, and propagated into three files before a lane
  enumerated the call surface and found two.

## Measured, not assumed

* ~~`R10SelfRecCatch`, the round's reproducer: **19 optimizing-arm runs at the
  oracle** on the wave-5 binary, and clean on every binary since. Pristine `dev`
  had died with an uncaught exception in both optimizing arms. Repeats, not
  single runs, because the family was bimodal.~~
  **STRUCK 2026-09-22. All 19 of those runs were reading the INTERPRETER.** The
  probe warms three shapes in a hot `drive` loop and then prints four values
  from `main`, which this VM never compiles — so the columns the claim rests on
  never exercised the compiled call path. Re-measured by counting results
  *inside* the warm loop: 9,941 of 10,000 calls wrong for the implicit-NPE
  shape and 9,940 for AIOOBE, in both optimizing arms, at the moment this
  bullet was written. Root cause and fix:
  `docs/internal/retired/r10-self-recursive-catch-of-its-own-call-is-miscompiled-20260921-RETIRED-20260922.md`.
  The repeats were right; the instrument was not, and repeats of a blind
  measurement are still blind. The successor guard,
  `regression-suite/src/RJitSelfRecCatch.java`, asserts only values produced
  inside `drive` and is falsified against the pre-fix binary.
* Regression suite **101/101 on every wave binary** from wave 5 onward.
* Wave 6 armed `DeoptVerifier` over the single-pass backend with a refusal that
  DISCARDS the artifact. **Zero `deopt-metadata-violation` across 101 vectors and
  all four arms**, on every binary since - evidence it refuses nothing today, and
  stated as exactly that rather than as a proof that nothing can fire.
* Wave 7's splice fix: **zero `SPLICE FRAME` lines**, confirming the new
  callee-geometry branch is unreached, which is what "byte-identical in practice"
  predicted.

## Still open, and why - this list is deliberate

> **Closed since this summary was written (2026-09-22, wave 9):**
> `r10-producers-ir-tier-instanceof-arm-still-has-no-census` - the optimizing
> tier's `instanceof` arm now has its own four-bucket census, printed beside the
> single-pass five on one line whose "single-pass door only" caveat is deleted.
> Retired to `docs/internal/fixed-bugs/`, together with the whole `earelock`
> chain it hung off: the two `checkcast`/`instanceof` census pages, the three
> `has_elided_monitor` descriptions, the single-pass `DeoptVerifier` page and its
> splice / by-bci / publisher-count residuals. Eight pages, nothing left open in
> that chain.
>
> `r10-deoptverify-find-deopt-point-has-no-production-caller` and
> `r10-deoptverify-frame-state-interner-has-no-production-user` — the last two
> `deoptverify` pages — are retired to `docs/internal/fixed-bugs/` with the
> `offsetkey` page that hung off the first. Lane `deoptretire` owned every file
> the two earlier lanes were blocked by, so it took the option both had said was
> right and could not reach:
>
> * `CompiledMethod::find_deopt_point` is DELETED, with its `debug_assert` and
>   the three `#[cfg(test)]` assertions in `ir_lower.rs` that were its only
>   users. Their replacement asserts "this offset names EXACTLY ONE point",
>   which the binary search it went through could not say — a deleted oracle
>   leaving a stronger test behind.
> * `ir::InlineScopeTable` and `ir_lower::Lowerer::caller_chain_for` are DELETED
>   (option B). This was the round's defect class in its worst form: unlike the
>   interner, whose absence a `rg` reported honestly, this table's READ side was
>   on the production lowering path, so a `rg` found production hits and told a
>   reader the optimizing tier described inlined scope chains — which it never
>   has, on any compile. **An orphan whose reference list lies is worse than one
>   with no references**, and that is the sharper statement of the round's lead
>   result.
> * The `ResumeSemantics` consumer half of
>   `docs/jit/deopt-frame-state-interning.md` §5.2 is closed: both trampoline
>   entries, the OSR exception-exit provenance check, both rebuild sinks and the
>   new `scope_resume_pc` all read `semantics` instead of inferring from
>   `DeoptReason`. `ReconstructedFrame` now carries per-scope semantics, which is
>   what the retired interned form did and §7.5 recorded as unsolved.
>
> **Wave 9 also built and ran the tests**, which none of waves 6-8 were permitted
> to do for these files: `cargo test -p cratonvm-jit` (3 375 lib + 63 integration
> binaries), `cargo test -p cratonvm-vm --lib --all-features` (4 522). One test
> changed BEHAVIOUR rather than prose, and it is the interesting one: routing on
> `semantics.rethrow_exception` instead of `reason == PendingException` means a
> flagged point now reaches `LAST_EXCEPTIONAL` with its precise handler locals
> instead of being replaced by the re-run sentinel — the same defect the tree
> already recorded for `ir_deopt_entry`'s old shape, closed for the other
> direction too.

| page | why it is open |
|---|---|
| `r10-ea-single-pass-monitor-scalar-relock-is-unreachable` (**FIXED 2026-09-22**, `../../internal/fixed-bugs/r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md`) | Was "argued twice and deliberately not fixed", because the arm is SOUND but INERT behind two independent gates. Both gates are gone and the three-step ordering the page ends with is the one that was followed: the `goto` barrier admits straight-line forward jumps, the monitor arms are written, and the exceptional-frame sinks accept virtual objects so the relock has something to relock. Phase C now fires (`elided_monitor_ops` non-zero) AND is consumed (materialisations at the handler), with every checksum equal to HotSpot. |
| `r10-gate-two-g1-censuses-have-no-reader` (**FIXED 2026-09-22**, `../../internal/fixed-bugs/r10-gate-two-g1-censuses-have-no-reader-FIXED-20260922.md`) | Two different things wearing one output. `block_offset_no_jump_census` was a real orphan -- every reader it had was under `gc/tests/`, so the lever could be turned on against a shipped binary and produce no reading -- and it and both siblings now feed a `[GC] g1 block-offset:` line on the report `vm-cli` emits on BOTH shutdown arms. `g1_evac_copy_span` was never an orphan: its reader is in its own defining file, which the gate treats as a non-caller by design. That is the gate's L8 limit, and it produced the checked `read-by` annotation the debt-register retirement below cites. |
| `r10-bybci-deopt-box-map-conflates-two-deopt-reasons` (**FIXED 2026-09-22**, `../../internal/fixed-bugs/r10-bybci-deopt-box-map-conflates-two-deopt-reasons-20260921-FIXED-20260922.md`) | The probe the page said should come first was built first and answered YES: `Compiler::note_by_bci_reason_collision` prints under `CRATONVM_DBG_DEOPT`, and `apps/probes/DeoptReasonCollisionProbe.java` collides four times at one pc under `CRATONVM_DEOPT_EAGER`. The page's guessed shape -- a BCE-guarded loop header that is also an invoke -- is unreachable and always was (`bce::analyze_array_access_operands`' simulator stops at the first `invoke*`); the producer that reaches it is `deopt_eager_bci`. Zero collisions under default flags, with the method still compiled, so the bound is stated as a bound. Key is now `(bci, DeoptReason)`, option *(a)* of the proposals page, across the five files it sized. |
| `r10-diagread-orphan-instrument-debt-register` (**RETIRED 2026-09-22**, `../../internal/retired/r10-diagread-orphan-instrument-debt-register-20260921-RETIRED-20260922.md`) | A REGISTER, not a defect -- and now a two-entry one, down from 112. The 96 remaining entries were dispositioned one at a time: 54 wired into an opt-in per-subsystem census (`CRATONVM_INSTRUMENT_CENSUS=1`), 33 narrowed to `pub(crate)` (over-broad visibility, never orphans), 7 deleted with their statics and tests, and one -- `note_bb_dispatcher_cap_hit` -- was a live defect whose call had been lost in the H6 exec-depth rewrite. The two survivors are fed in-file and `pub` only for an integration test; both say so at the definition, and neither takes the `read-by` annotation, because its cross-file half would verify against unrelated code. The gate is now wired into CI. |
| `r10-readers-orphaned-jit-accessors` | PARTIAL by design. Some accessors still have no production reader; each is either allowlisted with a reason or filed. |

## Closed after the round, 2026-09-22 (wave 9)

Five pages the round left in a partial state were finished and retired to
`docs/internal/retired/`. Both were the last residue of their family, and both
were finished by *executing* what three lanes had only been able to read:

* **`r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode`**, with
  `r10-vecplan-dependence-capped-widths-...` and
  `r10-vecclasses-emitter-doc-contradicts-three-new-encoders`. Class B closed: `&`
  reductions emit at both integer widths. The instruction was never the obstacle —
  `VPCMPEQD acc, acc, acc` is `&`'s all-ones identity and `vec_rr` already emitted
  that form. The obstacle was the shape of the admission test, and it is the
  round's own lesson one more time: `scalar_fold_opcode` alone decided which
  reductions were emitted, so adding the obvious `And => 0x21` row gives every `&`
  reduction a `VPXOR`-zeroed accumulator and the answer `0` — silent wrong code
  from one line, invisible to a suite whose reductions all sum. The fix is a
  *second* table read together with the first, pinned against it over the whole
  operator vocabulary. `*` stays refused with the reason recorded (its identity
  needs an operand layout `Asm` does not have). The page's real deliverable, the
  list of shapes the gate admits and the emitter refuses, moved into
  `docs/jit/vectorization-emitter.md` so it outlives the page.
* **`r10-report-seven-code-cache-fields-still-have-no-producer`**, with
  `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields`. The
  compilation census got a producer, so five of the seven fields are now measured
  and the second arithmetically impossible reading is impossible by construction
  rather than by suppression: the census and `installed_bodies` are bumped from the
  same `mark_published` call, and the pull samples the census *first* so a racing
  publish can only make the denominator stale-low. Not wired from
  `nominate_to_first_body`, the lower-bound counter that page named as a trap. The
  two remaining fields stay suppressed because their UNIT is missing, not their
  measurement — they count sweeps and the queue counts withdrawals — which is the
  recorded decision half of that page's own closing condition.
  And one finding the pages could not have had: a *fed* census with nothing yet
  compiled still prints `versions_per_method=0.0000` through a zero denominator, so
  `Display` grew a third arm rather than print the one string readers had been
  taught to distrust.

## Proposals

Sixteen `docs/feature-designs/jit-r10-*-proposals.md` documents. The ones worth
reading first: the escape-analysis note that scalar replacement has THREE
independent sufficient refusals (which is why fixing one at a time moves
nothing), the vectorization ordering in which every intermediate state is still a
refusal (adding one obvious opcode alone yields `0 & x == 0`, silent wrong code),
and the argument for teaching the orphan gate about `&self` readers and
`#[cfg(test)]`.
