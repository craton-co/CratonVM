# Proposal: a withdrawn compiled body leaves at its constant-pool helper calls

**Status: proposal — filed 2026-10-07 by interpreter round i1 wave 43, lane
L2, from the host evidence of
`docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md`
and the shapes of
`docs/known-issues/interpreter/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-20261005.md`.
Wave 44 (lane L2): built for the single-pass tier behind
`op_invoke.rs::CP_HELPER_EXITS_ENABLED` (on in the lane's last commit); the
optimizing tier and the holder-keyed type-check calls remain (see
"Progress (wave 44)"). Wave 45 (lane L2): the optimizing tier's half is not
built; what it needs is written out in "Progress (wave 45)".**

## Progress (wave 45) — lane L2: the optimizing tier's half, not built

Read at `69568bea6`; no code changed for it. What stands in the way is not
the site (the tier already emits post-call sites the same way,
`ir_lower.rs::maybe_emit_post_call_exit_site`) but the frame state:

* `Op::ConstString` and `Op::ConstClass` (`ir_lower.rs`, the "cov-01" arms)
  lower to `runtime_lowering::emit_ldc_class_cp_stub` behind a slot probe
  and a safepoint map, and carry NO snapshot: they cannot deopt (a failed
  helper takes the shared exception epilogue). A site before them needs the
  frame as the interpreter would stand at the `ldc`'s bci (every live local,
  the operand stack below the `ldc`), which the graph does not keep there.
  The node is pinned (`[ctrl, mem]` inputs, `ir.rs`), so the site would at
  least sit at the instruction's place in the schedule; the snapshot is
  what is missing.
* The `new` / `anewarray` / `multianewarray` nodes do carry snapshots for
  their allocation-failure exits; whether they are the state BEFORE the
  instruction (with its operands) or after, and whether `plan_slots` pins
  every value they name up to the site, was not traced.

**The next step, buildable:** in the IR builder, give `Op::ConstString` /
`Op::ConstClass` a before-state snapshot when the compile's polls read a
verdict (`IrPollModeExits::OsrDoor` / `MethodEntry`), as `Op::Call`'s is
kept (`ea_ir_bridge::ir_prune_unconsumable_snapshots` must keep it, and
`plan_slots` then colours what it names); then a twin of
`maybe_emit_post_call_exit_site` that builds the point from that snapshot
with `ResumeSemantics::REEXECUTE` at the node's own bci (the single-pass
site's rule: nothing of the instruction ran), counted against
`IR_POST_CALL_EXIT_SITES_MAX`, with a unit test in the style of the
post-call site tests at the end of `ir_lower.rs` (`door.post_call_exit_sites`).

**Why it is less pressing after wave 45:** an optimizing OSR body of the
redefined class's own bytecode now leaves at its next back-edge poll or
post-call site after a renumbering redefinition (the i44-L2 proposal's
"Progress (wave 45)"), and a body withdrawn for another class or by a
debugger already leaves at those. The sites would add an earlier exit
inside one iteration of such a loop, and an exit to an optimizing
METHOD-ENTRY body with no loop and no call.

## Progress (wave 44) — lane L2: the single-pass tier's sites

**Built.** `jit/src/x64/op_invoke.rs::emit_cp_helper_exit_site`, called by
the single-pass walk right before each run-time constant-pool helper call:
the `ldc` / `ldc_w` arms of a String or Class constant (`op_local_stack.rs`,
before `emit_ldc_class` / `emit_ldc_string`, i.e. before the compiled-`ldc`
slot probe too), the deferred `new` and `anewarray` arms and `multianewarray`
(`op_object.rs`, with the length / dimensions still on the simulated stack).
The site is the post-call exit's in every part but where its map is: the
aligned five-byte `NOP`, the 16-byte stub, the shared register-preserving
verdict tail (`POST_CALL_EXIT_VERDICT_ONLY`), the `POST_CALL_EXIT_SITES_MAX`
cap (shared), the force pass (`JitCache::force_withdrawn_exit_polls` reads
`CompiledMethod::post_call_exit_sites`, which lists both kinds), and the
tier rule (`post_call_exit_site_may_be_emitted`: an OSR-tier compile, or a
method-entry one that already filed a map or keeps a dispatch record). The
`OsrExit` map is at the instruction ITSELF with its operands on the stack
(`branch_mode_exit_target(pc, stack.len())`, after a `flush_scratch_registers`
every helper arm made anyway), so the interpreter re-executes the
instruction; nothing of it ran (the site is before the helper). Refused: a
branch target (a loop header's map would otherwise be this site's), and
whatever `branch_mode_exit_target` refuses (a point already at the bci, a
loop rewrite, an unresumable frame, an elided monitor or self-lock, a
splice).

*What was checked first*, as the proposal asked:

* every such site files at most one other point at its bci, the allocation
  guard's precise frame (`emit_post_alloc_oom_check`, protected ranges only):
  a `RETHROW` `PendingException` point in its own by-bci map
  (`exc_frame_box_ptr_by_bci`), filed after the site's `REEXECUTE` one, and
  the two by-bci reason lookups each skip the other kind
  (`osr_exit::deopt_reason_at_bci`, `exceptional_reason_at_bci`);
* re-execution is idempotent: the site precedes the helper call, so a `new`'s
  class initialisation and every allocation happen once, in the interpreter.

**Positive controls.** Unit test
`x64::tests::a_forced_cp_helper_exit_leaves_before_the_helper_runs` (a real
OSR-tier compile of `new int[2][3]` with a fake `multianewarray_n`: one site,
a `NOP` until forced, a stay verdict runs the helper, a leave verdict exits at
bci 2 with `[2, 3]` on the stack). `CompiledMethod::cp_helper_exit_sites`
counts the sites; the `[cratonvm-jitc] exit polls candidate:` line prints it
(`cp-helper-sites=N`) for every OSR body a redefinition sees. Probe
`tools/probes/interp/L2/L2W44CpHelperExitAfterTheCall.java` (agent jar): a
method-entry body parked in a call whose successor is an `ldc` (String, then
Class) and which then constructs an object whose elided constructor was
retransformed meanwhile. The post-call site is refused there (an `ldc`
successor files a point of its own, `post_call_exit_successor_admitted`), so
on the base under `CRATONVM_C2_SUPERSEDE=0` the body runs on to its return:
`false` on both rows. With the site: HotSpot's `true` on both rows, and
`CRATONVM_DBG_DEOPT=1` prints a `post-call exit verdict #k: ...stringBody...
named=true withdrawn=true verdict=0x..` (non-zero) line per row.

**Found while building it: an own-class body never leaves, so the proposal's
own control cannot fire.** `JitCache::force_withdrawn_exit_polls` spares a
body compiled from the redefined class's own bytecode
(`compiled_from_class`, `exits_spared_as_obsolete`): its frame is that
class's obsolete activation, which HotSpot lets finish too (JEP 109), and
`withdrawn_body_may_leave` answers "stay" for it. `L2W43CompiledConstantsAcrossRenumbering`'s
`Tgt.spin` is exactly such a body, so its `ldc` sites are not forced and the
index translation (`CpSite`) remains what keeps it right; its candidate line
shows `own-class=true ... cp-helper-sites=N`. The sites serve the bodies a
redefinition withdraws for ANOTHER class (a splice or an elided constructor
of it) and every body a debugger request withdraws
(`JitRealm::withdraw_every_body_for_the_interpreter`).

**Cost.** Per admitted site: five bytes of `NOP` (plus at most a four-byte
alignment `NOP`) on the fast path, a flush the helper arm made anyway moved
before the slot probe of a compiled `ldc`, a 16-byte stub, one reason-7 stub
and one `DeoptimizationPoint` out of line. Measure with the JIT rows the
post-call exits were measured with (`InvokeDoorCostBench`,
`L7W28VirtualDoorSplitBench`), plus a string-constant-heavy loop, this
branch against itself with `CP_HELPER_EXITS_ENABLED = false` (the last
commit reverted).

**What remains:**

* the optimizing tier: a helper `Op::Call` of a constant (`ir_lower.rs`) would
  need the site before the call with the INSTRUCTION's frame state
  (`post_call_exit_state` builds the state after the call);
* the holder-keyed type-check calls (`jit_typecheck_holder_site_target`'s
  sites), which the single-pass walk lowers through the `checkcast` /
  `instanceof` helpers; not given a site;
* a single-pass method-entry body with neither a filed map nor a dispatch
  record (the wave-39 rule keeps the first deopt exit off it; see
  `op_invoke.rs::METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`).

## The problem it removes

A compiled body its class's redefinition withdrew keeps running until it
reaches an exit: a back-edge poll with a mode-exit map, or (since waves
27-39) a patchable post-call exit after a real invoke. Bodies with neither --
i41-L1's shapes (a loop-free method-entry body with no call record, a loop
the single-pass admission refuses, an optimizing back edge whose header
state has no location) -- finish their activation compiled. While they do,
every constant they name goes through a runtime helper that must translate
the body's constant-pool index into the class's current pool
(`vm/src/jit/helpers.rs::CpSite`). Wave 43 found that translation racing the
redefinition (the host's `JIT ldc: cp#19 ... holds Utf8("length")`), and
`multianewarray` / type-check sites still judge it lock-free
(`docs/internal/fixed-bugs/interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008.md`).
Every such site is a place the body could have left instead.

## The idea

The constant-pool helper calls of a compiled body (`jit_ldc_string_cp`,
`jit_ldc_class_cp`, `jit_new_object_cp`, `jit_anewarray_object_cp`,
`jit_multianewarray`, the holder-keyed type checks) are calls whose failure
the single-pass tier already routes through a bail at the call
(`emit_post_alloc_oom_check` on the allocation sites; whether each site has
a resumable frame state is the first thing to check, below). Give each such
site, in a body that reserved a compile id, the post-call exit machinery the
invoke sites have (`x64/op_invoke.rs::emit_post_call_exit_site`): a patchable
5-byte site BEFORE the helper call whose stub asks the body's withdrawal
verdict (`POST_CALL_EXIT_VERDICT_ONLY`) and, when the body is withdrawn,
leaves through the reason-7 stub of an `OsrExit` map at the instruction
itself, so the interpreter -- which moves the frame onto its translated body
first -- executes the `ldc` / `new` against the right pool.

* Forced with the polls and the other post-call exits when the body is
  withdrawn (`JitRealm::withdraw_*`), so nothing is paid until then: a NOP
  on the fast path, as at the invoke sites.
* It gives the loop-free and refused-loop shapes of i41-L1 an exit wherever
  they read a constant, which also serves a debugger request that withdraws
  every body.
* With it, a withdrawn body never reaches a helper's translation at all; the
  translation stays for bodies that are not withdrawn (a redefinition of
  another class that only its splices copied).

## What it would cost

Five bytes per constant-pool helper site of a body that has a compile id,
plus a 16-byte stub each, bounded by `POST_CALL_EXIT_SITES_MAX` as today
(the cap would count both kinds). Nothing on a call path. The optimizing
tier needs a frame state at the helper call (`ir_lower.rs::post_call_exit_state`
already builds one per call; a helper `Op::Call` has one).

## What to check first

* Which helper sites already carry a deopt point whose state the exit map
  can reuse, per tier (the `emit_post_alloc_oom_check` sites do).
* That the interpreter's re-execution of the instruction is idempotent: the
  helper has not run yet when the site leaves (the site is BEFORE the call),
  so a `new`'s class initialization runs once, in the interpreter.
* A positive control: `CRATONVM_DBG_JITC`'s `post-call exit verdict` lines
  and counters (wave 28) extended with the site kind, and the probe
  `tools/probes/interp/L2/L2W43CompiledConstantsAcrossRenumbering.java`
  under a debugger-free agent redefinition, where the loop's `ldc` sites
  must then leave rather than translate.
