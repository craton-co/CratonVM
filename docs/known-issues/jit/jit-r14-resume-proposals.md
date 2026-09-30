# JIT round 14 wave 3, lane resume: proposals

Status: OPEN (proposal book; ideas, not work items, until the owner triages them)
Area: deopt charging and chain resumes: `vm/src/runtime/interpreter/deopt_resume.rs`,
`vm/src/jit/helpers.rs` (call-site service, `despeculate_trapped_method`),
`vm/src/runtime/interpreter/jit_bridge.rs` (OSR door charge), `jit/src/lib.rs` (IR inline planner)
Severity: proposals
Found by: round 14 wave 3 lane resume

Ranked by expected value per cost. Landed this wave: CH3-4 (OSR chain guard exits charged at their
call site), R14DP-1 (the call-site service takes a never-redefined body's own template), R14DP-3
(the service's restash keeps the template). The defect found on the way is a page:
`r14w3-resume-chain-trap-despec-never-withdraws-the-spliced-guard-FIXED-20260929.md`.

## RS-1. Keep a chain trap's history in the SPLICED method's own name too (MEDIUM value, low cost)

**What.** Every chain sink charges the outer method at the call site. HotSpot additionally keeps
the trap in the inlined method's own profile (its MDO's per-bci trap counts), so when that method
is compiled on its own -- or inlined elsewhere -- the failing speculation is already known. Here,
on a chain trap, also record `(innermost method key, innermost bci, reason)` in the deopt log as a
NON-evicting event (a new `DeoptLog` entry kind, or `record_deopt` without `invalidate`), and after
`PER_BCI_DESPEC_LIMIT` such records insert the derived speculation id for the callee at its own
bci. The callee's standalone body then drops that guard, and so does any other caller's splice of
it if the IR builder asks the registry with the callee's key for spliced guards (it does not today;
see RS-2).
**Benefit.** A hot callee whose `BoundsCheck` fails in one caller's loop stops paying the trap in
every other caller.
**Cost.** One helper in `deopt_resume.rs` called from `stash_charge_site`'s three callers and the
OSR chain charge; the log entry kind.
**Risk.** Low (additive; only charges the callee's registry row).
**First step.** Under `CRATONVM_DBG_DEOPT=1` count chain traps per `(innermost key, bci)` on the R13
chain battery in the chain arm.

## RS-2. Let the IR builder ask the de-spec registry about SPLICED guards by the callee's key (MEDIUM value, medium cost)

**What.** A spliced guard's point is published at the callee's own bci (`ir_lower.rs` ~18519,
`chain.bci`), but every registry consult in the planner / builder / `ir_optimize` uses the OUTER
method's key and the combined pc. Map a spliced node's pc through `IrInlineFrameSites` to
`(callee key, callee bci)` and ask the registry with that pair before planting a speculative guard
inside a splice. With RS-1 this closes the loop HotSpot has.
**Benefit.** The recompile after a chain trap actually differs from the body that trapped (today
it does not; see the page above).
**Cost.** A translation helper in `ir.rs` and the consults in the speculative-guard producers.
**Risk.** Medium (touches guard admission; a wrong translation only withdraws a guard, never adds
one).
**First step.** List every `contains_speculation` caller in `jit/src` and mark which can run on a
spliced pc.

## RS-3. One function for "charge a trap and maybe de-spec its site" (LOW value, low cost)

**What.** The per-bci de-spec rule (limit 4, count at `(reason, bci)` or at the whole bci for the
wildcard, insert derived id or wildcard) is written out three times: the doors
(`real_frame_deopt_resume_or_throw_and_despeculate`), the call-site service / OSR flat charge
(`helpers::despeculate_trapped_method`), and this wave's OSR chain charge
(`charge_osr_chain_guard_exit`). Fold them into one `deopt_resume::charge_trap_at_site(shared,
class_id, key parts, reason, bci, wildcard)`.
**Benefit.** The three have already drifted once (round 11 wave 4: one summed every reason at the
bci). One copy cannot drift.
**Cost.** A refactor across `deopt_resume.rs` and `helpers.rs` (the site-trap policy stays in the
helper).
**Risk.** Low; the three unit-test families pin each caller.
**First step.** Diff the three blocks and list their deliberate differences (the site-trap arm, the
superseded / left-for-interpreter guards).

## Round 14 wave 4 (lane resume2): RS-3 landed

The de-spec half (the part that drifted) is one function, `deopt_resume::despec_site_after_trap_limit`
(with `PER_BCI_DESPEC_LIMIT` as a module constant): the sentinel refusal, the limit, the count at
`(reason, bci)` or the whole bci, the insert. The doors and the OSR chain charge call it (pending
build); `helpers::despeculate_trapped_method` is an exact patch
(`r14w4-resume2-helpers-despec-rule-patch-FIXED-20260929.md`). The CHARGE itself stays per caller: the
doors and the OSR chain charge use `deoptimize_in`, the service `deoptimize_or_by_name` with its
site-trap arm, and the superseded / left-for-interpreter guards stay where they are -- those are
the deliberate differences. Test `r14w3_resume_tests::the_per_bci_despec_rule_counts_at_the_grain_it_is_asked`.

## RS-4. A door that receives a FOREIGN stash with a source charges the right body (LOW value, low cost)

**What.** Since R14DP-3 a frame the call-site service declined keeps its trapping body's template
when it travels on. A door that reads it as a foreign stash charges its owner by NAME
(`despeculate_stashed_frame_method_with_cause`), resolving the class through the door's loader. The
source names the class id exactly (`source.declaring_class_id`); use it for the charge when present.
**Benefit.** Correct attribution when two loaders define the owner's class name (the charge today
may hit another loader's body or none).
**Cost.** One `peek_last_deopt_trap_source` read before the door takes the stash, threaded to the
charge.
**Risk.** Low.
**First step.** Census how often a door's foreign-stash charge resolves no class
(`CRATONVM_DBG_DEOPT=1`).
