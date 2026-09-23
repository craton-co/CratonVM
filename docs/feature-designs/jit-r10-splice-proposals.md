# Round 10 wave 7, lane `splice` — proposals

Scope: `jit/src/x64.rs`, `jit/src/x64/inlining.rs`, `jit/src/x64/deopt_stubs.rs`.
Everything here was derived by READING. This lane was not permitted to build,
test, probe or run the regression suite, so no number below is a measurement and
none is presented as one; where a size or a cost is stated it is a count of call
sites or an inspection of the code, and says which.

The wave's landed change is recorded in
`docs/internal/fixed-bugs/r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`
(resolution section). The residuals it did not close are
`docs/internal/fixed-bugs/r10-splice-by-bci-deopt-maps-have-no-owner-FIXED-20260922.md`
and §1–§4 below.

---

## §1 A callee-side local kind/liveness analysis, so a spliced frame can be resumable

**Where it stands.** A deopt point published from inside a splice now describes
the callee's frame geometry with every slot `FrameValue::Unsupported`. That is
honest and fail-closed, and it is also useless for reach: one `Unsupported` slot
makes `frame_state_is_resumable` false, so any trap at such a point costs the
method a re-run (or, for a method with stores, an `InternalError: precise
deoptimization unavailable`). The first edit that publishes inside a splice
therefore gets correct metadata and no benefit.

**What already exists, and is easy to miss.** Two of the three inputs are there:

* **oop-ness.** `Compiler::inline_oop_scopes` runs `compute_local_oop_masks` over
  the CALLEE's own bytecode, seeded with the callee's reference parameters
  (`compute_param_oop_mask`), and `InlineOopScope::mask_at_cur()` answers at the
  callee pc being emitted. The safepoint machinery already trusts it to name
  callee locals as GC roots (`x64/safepoint.rs`: `collect_live_oop_homes`,
  `moving_young_safepoint_coverage_complete`), and refuses coverage when it cannot
  answer — a `None` from `mask_at_cur` is a refusal, not an empty mask.
  (The wave-6 known-issues page stated that no oop source exists for a callee's
  locals. That is not correct, and it matters, because it was the stated reason
  the "substantive half" of the fix was impossible.)
* **homes.** Callee local `k` lives at `[rbp - (local_base + k*8)]`, memory-homed
  for the whole splice — the splice never register-allocates a callee local
  (`try_emit_inline_body`'s `emit_store_local`/`load_slot_to_reg` go through the
  frame slot), which is also what makes `push_inline_scope`'s caller snapshot
  sound.

**What is missing.** A WIDTH source and a LIVENESS source for the callee:

* the root method gets `classify_local_kinds` (whole-method) plus
  `local_kinds_refined` (per-pc reaching kinds) and
  `regalloc::live_locals_per_pc_all`. None of them is run over callee bytecode.
* without a width, a non-reference callee local cannot be distinguished between a
  cat-1 `int`, a spilled `long` and a spilled `double`, and publishing the wrong
  one is a truncated value on resume — the same failure the operand stack's
  `wide_fp` gate exists to avoid.

**Shape of the work.** `compute_local_oop_masks` is already called per splice with
`(callee_code, callee_len, callee_locals_size, param_oop_mask)`. The same call
site can run `classify_local_kinds` over the same four inputs and park the result
in `InlineOopScope` (which then wants renaming — it would no longer be only about
oops). Cost: one extra analysis per splice, on a body the planner has already
size-capped, so it is bounded by the inline budget rather than by method size.
Liveness is the larger half and can be skipped at first: publishing a live-but-
`Ambiguous` slot as `Unsupported` is what the root path does.

**Do not do this before there is a consumer.** With no publisher inside a splice,
a callee kind table would be an analysis whose result nothing reads — the exact
defect class `scripts/check-orphan-instruments.sh` was written for this round to
stop growing. The order is: publisher first (a BCE guard or an OSR exit inside a
callee body, with a workload that shows the reach it buys), then this.

## §2 Teach the four `dbg_last_pc` publishers whose pc they are publishing

**The finding.** `emit_post_invoke_exception_check`, `emit_post_alloc_oom_check`,
`emit_precise_array_npe_check` and the reason-11 bounds arm all key their point on
`Compiler::dbg_last_pc`, which only the OUTER walk assigns
(`x64/bytecode_walk.rs`). Inside a splice that is the CALLER's invoke pc — which
is the correct attribution for the exception (the enclosing method's exception
table is the one that must be searched, and the site's own doc says so) — but
`current_bytecode_owner`/`resume_bci_for` would stamp it with the CALLEE's key and
publish it without `orig_bci`. Both halves wrong, in the opposite direction from
the wave-6 page's finding.

**Why it is not live.** `precise_exception_frames` arms all four, and inlining is
refused for such a compile three times over (`plan_inline`,
`inline_sites.clear()`, and — new this wave — `try_emit_inline_site`).

**The proposal.** Give the producer a way to say "this pc belongs to the enclosing
method": a `build_and_record_deopt_point_for_enclosing_pc(bci, reason)` that builds
the frame with the identity stack treated as empty. For a reason-9 frame that is
not a compromise but the correct answer: `first_unresumable_local`'s doc states
that a `PendingException` frame's recorded operand stack is NEVER READ (the
consumer builds `[exception]` per JVMS §2.10), so the only fields that matter —
`bci` and `locals` — are exactly the ones the root-method analyses answer
correctly at `dbg_last_pc`. The extra callee operands left on `self.stack` are
ignored.

Not done here because three of the four call sites live in files this lane does not
own (`arrays.rs` x3), and doing half of it would leave the two owned sites behaving
differently from their siblings for no stated reason.

## §3 `inline_caller_chain` has no depth cap

`deopt::caller_chain_depth`, `frame_state_is_resumable`,
`FrameStateInterner::materialize` and `DeoptVerifier::check_point` all bound their
walks by `MAX_SCOPE_CHAIN`, each with a comment saying that a chain deeper than
that is a metadata defect rather than a deep inline. `Compiler::inline_caller_chain`
builds the chain and bounds nothing. It cannot overrun today (only
`try_emit_inline_site` pushes a scope, and it cannot nest — `try_emit_nested_inline`
pushes none), so the stack's depth is at most 1 and a cap would be dead code that
reads as a live guard. Filed rather than added for exactly that reason: the wave
found nine instruments this round whose only defect was being unreachable. The cap
belongs in the same edit that makes nesting push scopes, and
`MAX_OSR_INLINE_RESUME_DEPTH` (9, the VM's own budget, deliberately defined as one
constant so the two cannot drift) is the figure to use — not `MAX_SCOPE_CHAIN`,
which bounds what may be WALKED rather than what may be admitted.

## §4 `force_inline_deopt_publication` only produces one of the two refusable shapes

The test-only hook (`#[cfg(test)] impl Compiler`) pushes a raw `deopt_stubs` entry,
which the postcondition catches as `published_unreadable_stub` — a stub with no
matching point. It cannot produce the shape the postcondition's other two clauses
exist for: a published POINT that is misidentified. So
`inline_publishing_a_deopt_point_is_refused` (in `x64/tests.rs`) exercises one
clause, and the other two are exercised only by the free function's own unit tests
on hand-built `DeoptimizationPoint`s.

A second hook that calls `build_and_record_deopt_point` from inside a live splice
would close that gap and would be the only way to test the rollback path (buffer,
stack, the three lockstep vectors and the by-bci purge) for a published point
rather than a published stub. `x64/tests.rs` is not owned by this lane, so the hook
would land with no caller — an orphan by construction — and it is filed instead.

---

## Rejected alternatives, and why

**Publish nothing from inside a splice (make `build_and_record_deopt_point` a
no-op, or assert the identity stack is empty).** This keeps today's guarantee with
one line and is tempting. Rejected: a silent no-op is how a future publisher gets a
guard with no snapshot, routed to the shared sentinel exit, with nothing saying
why; and an assert/refuse-the-compile turns a future edit into a compile bail whose
cause is one frame deeper than the code the author just wrote. The fail-closed
frame is louder (a `SPLICE FRAME` trace line under `CRATONVM_DBG_EXCFRAME`) and
degrades rather than vanishes.

**Revert the postcondition to "refuse every splice that published anything".** This
is what the wave-6 page argues against and it is right to: the check would stop
stating the property, and the edit that makes callee-identified metadata possible
would no longer be admitted automatically. The bci-range clause added this wave is
the narrower version of the same conservatism — it refuses the specific pair that
cannot be a callee pc.

**Publish precise `StackSlotRef`s for the reference slots a callee's
`mask_at_cur()` claims, leaving the rest `Unsupported`.** Correct as far as it
goes, and buys nothing: the frame stays unresumable because of the other slots, and
a wrong sign or base on a `StackSlotRef` hands the GC the caller's frame as an
object address. Sequenced behind §1 instead, where a resumable frame is the
deliverable and the ref slots are one part of it.

**Retire `Compiler::has_elided_monitor` (the field lives in `x64.rs`, owned).**
Reviewed and NOT done. Wave 6 took option 1 on
`r10-earelock-has-elided-monitor-is-never-set-FIXED-20260922.md` — keep the field as the
standing contract for an elision that leaves no `MonitorInfo`, pinned by
`jit/tests/r10_deoptverify_elided_monitor_contract.rs`, which is DESIGNED to fail
on the legitimate edit. Retiring it now would delete a contract, delete the only
statement in the tree that `can_deopt_resume` and `can_osr_exit` are coupled, and
break that test, in exchange for constant-folding a `&&` that the compiler already
constant-folds. The two comments in `x64.rs` that the page listed as still
misleading (the field's own doc and `sr_monitor_scalar_ops`') were already
corrected by `cad2e2339`; both were re-read this wave and both now state that
nothing sets the flag.
