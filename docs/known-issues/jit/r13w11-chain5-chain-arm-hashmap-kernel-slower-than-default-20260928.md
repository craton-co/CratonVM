# The chain arm splices `putVal` / `getNode` into the `hashmap` kernel and runs it SLOWER than the default arm

Status: OPEN
Area: `jit/src/ir.rs` (chain snapshots, `push_splice_state`), `jit/src/ea_ir_bridge.rs` (`ir_prune_dead_snapshot_locals`), the IR linear-scan allocator (phi residency), `jit/src/lib.rs` (the IR inline planner's budget)
Severity: MEDIUM (performance; it is what keeps `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` / `_CHAIN_FENCES` / `_MULTI_RETURN` from being flipped, not soundness)
Found by: round 13 wave 11 lane chain5 (by reading the orchestrator's w10f / w11a traces and bench)

## What was measured

`bench13.sh` on w11a, five interleaved reps, medians (ORCH-LOG 20:41): `CratonBench hashmap`
default 1957 ms, chain arm (`FRAME_STATES=1 CHAIN_FENCES=1 MULTI_RETURN=1`, "CM") 2090 ms (+7%),
CM with `CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_RECEIVER=0` 2155 ms; HotSpot 635 ms. The arm does
what it exists for -- `C:\craton\jitr13-probes\hs2-CM.log`: `inline-plan
CratonBench.hashMapPutGet(I)J: 3 site(s), 13 spliced bodies, 613 bytes appended`, HS-2 engaged,
7 guards publish a chain, 0 fall back, the OSR body is entered (`osr optimizing REUSE ... pc=10`)
-- and the kernel still gets slower, although it makes two compiled calls per put+get pair fewer
(`putVal`, `getNode`; about 14 ns of ~140 on the round-12 numbers).

## What differs between the two kernel compiles, by the traces

| | default (`hs2-D.log` 1509-1560) | CM (`hs2-CM.log` 1652-1863) |
|---|---|---|
| spliced bodies / bytes | 4 / 63 | 13 / 613 |
| linear scan | 6 values resident, **3 of 4 phis** | 18 values resident, **3 of 14 phis** |
| `putVal` / `getNode` | own compiles (`putVal`: 11 resident, 4 of 17 phis) | in the kernel's graph |
| loops in the kernel graph | the kernel's own two | + `getNode`'s chain walk, `putVal`'s `binCount` loop |

So 11 of the kernel graph's 14 phis live in memory. Which ones is not in the trace; if the
kernel's own loop-carried values (`i`, `sum`, `map`) are among them, every iteration of the hot
loop pays loads and stores that the default arm's kernel keeps in registers, and the spliced
`getNode` walk (one iteration per get on these sequential keys) pays its phis' spills too.

## Hypotheses, each with the way to confirm it (by reading; nothing was run by this lane)

1. **Phi residency.** The allocator gives 3 of 14 phis a register (item above). Confirm with
   `CRATONVM_DBG_IR_LINEAR_SCAN=1` on the CM kernel compile: list which phis are demoted; if the
   outer loop's are, a loop-depth priority for phi residency (outer hot loop first, or the
   innermost loop by its profile count) is the fix -- allocator code, lane callcost's region.
2. **Chain snapshots keep dead locals alive.** `ea_ir_bridge::ir_prune_dead_snapshot_locals`
   clears snapshot locals the bytecode cannot read again, but only for snapshots whose `bci` is
   the compiling method's (`bci < code_len`): a chain snapshot's `bci` is a combined-buffer pc,
   so neither its innermost locals nor any caller scope's locals (carried in `stack`, never pruned)
   are ever cut. Every chain point therefore keeps EVERY local of every scope alive up to it, where
   the flat frame at the outermost `invoke` had its dead ones cleared. Proposal CH5-1 in
   `jit-r13-chain5-proposals-RETIRED-20260929.md` (per-scope liveness: callee scopes on their relocated bodies,
   caller scopes at their `invoke`'s successor). Confirm by counting chain-snapshot slots that
   the per-scope liveness would clear on the kernel (a debug census in the prune).
3. **One big loop body instead of three small compiles.** 613 appended bytes put the cold paths
   of `putVal` (resize branch, `treeifyBin` call, the collision loop) and `getNode` (the chain walk,
   the `TreeNode` test) inside the kernel's loop: code size and branch density in the hot loop,
   and more values competing for the same registers (hypothesis 1). Confirm by timing CM with
   `CRATONVM_JIT_IR_SPLICE_COLD_NESTED=0` / `=1` (already default on) and with a smaller
   `IR_INLINE_MAX_TOTAL_BYTES` if a switch exists for it.
4. **Not the cause, checked by reading:** escape analysis (the kernel's boxes and `Node` escape in
   both arms; under chain fences EA keeps what a chain names, but nothing here was removable), the
   OSR entry (entered in both arms), the guard count (7 chain guards, none failing on this
   kernel), CH-1 (this wave's forced chain deopts emit nothing with `CRATONVM_DEOPT_EAGER_CHAINS`
   off).

## What to do

Take hypothesis 1's census first (one run with `CRATONVM_DBG_IR_LINEAR_SCAN=1`). If the kernel's
loop phis are demoted, the allocator fix is the lever; hypothesis 2's pruning helps every chain
compile regardless and is small (proposal CH5-1). Re-time CM against default after each, and flip
the chain arm only when CM is at least neutral on `hashmap` and `R13Iropt8HashKernels`.

## Round 13 wave 12 (lane snapliv)

**Hypothesis 2 is fixed in code (CH5-1), unmeasured.** Checked by reading first: the page's
reading was right. `ir_prune_dead_snapshot_locals` skipped every snapshot at `bci >= code_len`,
and a chain snapshot's `bci` is a combined-buffer pc, so no chain snapshot was ever cut -- not
its callee locals, not the caller scopes riding in `stack`. In the DEFAULT arm the builder records
no snapshot inside a splice at all, so a spliced body's dead temps (and the eager φs of its loop
headers: `getNode`'s chain walk, `putVal`'s `binCount` loop) die in DCE; in the chain arm every
chain snapshot at those headers named every callee local, which kept those φs, their homes and
their edge copies alive. That is exactly the difference between the two arms' φ counts in the
table above, though whether it explains all of the 11 demoted φs is for the census below.

What landed (`jit/src/ea_ir_bridge.rs`, `ir_prune_dead_snapshot_locals_scoped`, called from
`lib.rs` in place of the old prune):

* each chain snapshot is checked against the build's spliced-body table
  (`ir::IrInlineFrameSites::bodies_enclosing`: method key, body length, the bci each scope is
  parked at, and the `stack` layout of `SpliceScopeChain::split`); any disagreement prunes
  nothing in that snapshot;
* scope 0's locals are cut by the liveness of its own relocated body
  (`combined[base .. base + code_len]`) at its own bci; every caller scope's locals by its own
  body's liveness at its `invoke` (the compiling method's includes its exception edges, so a
  throw out of the callee into a caller handler still finds what the handler reads); operand
  stacks and monitors are never cut;
* kill switch `CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS=0` (default on; inert outside the
  chain arm, where no chain snapshot exists).

Unit tests: `ea_ir_bridge::r13w12_snapliv_tests` (one- and two-level chains, a chain that
disagrees with the table, the kill switch).

**Still open.** Nothing was timed by this lane. For the orchestrator:

1. `CRATONVM_DBG_IR_COMPILES=1` on the chain arm prints `[ir] snapshot liveness: N slot(s)
   cleared -- ... M in chain snapshots` for `CratonBench.hashMapPutGet` (the census CH5-1 asked
   for); `CRATONVM_DBG_IR_LINEAR_SCAN=1` then shows the φ residency (was 3 of 14).
2. Re-time CM against default and against CM + `CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS=0`
   (`R13SnaplivHashKernel`, or `bench13.sh` `hashmap`). If CM is still slower, hypothesis 1
   (allocator phi priority by loop depth) and 3 (613 bytes of cold code in the hot loop) remain.
3. The compiling method's own loop headers are still not pruned by default (see
   `r13w12-snapliv-loop-header-snapshot-doubles-as-the-osr-seed-list-FIXED-20260929.md`); the kernel's
   own loop-carried values are live anyway, so this matters little here.

## Round 14 wave 1 (lane chain)

**The gap is bigger than the bench said, and it is not the kernel alone.** The w12a probe matrix
(already carrying CH5-1), default vs chain arm (`res-w12a-def.txt` / `res-w12a-cm.txt` in
`C:\craton\jitr13-probes`): `R13FramestateHashmapKernel` 377/380/268 ms against 715/761/534 (the
`hashmap-field` row, whose map is a static field so `put`/`get` stay CALLS, is 2x slower too),
`R13SnaplivHashKernel` ~405 against ~520, `R13Iropt8HashKernels` 217/231/215/591 against
350/316/298/820, `R11IrcoreHashMapGetNode` 728 against 1012. So the chain arm makes the separately
compiled `HashMap.put`/`get` (which splice `putVal`/`getNode` under `MULTI_RETURN`) slower, not
only the kernel.

**Hypothesis 1 (phi residency) is not the lever, by reading.** A `Ref` phi is refused a register
by TYPE (`plan_register_residency`, `PhiRefusal::RefType`, "a hard refusal, not a heuristic"), and
the phis `putVal`/`getNode` add are reference phis (`p`, `e`, `tab`, `k`, the multi-return
result). The 3 resident phis are the kernel's `int`/`long` ones in both arms. Confirm with
`CRATONVM_DBG_IR_LINEAR_SCAN=1`: the `[ir-ls] phis=14: resident=3 ref_type=N` line.

**What the traces do show: code size.** `CRATONVM_DBG_JITC` `full-compile ... len=` (actual bytes):
`HashMap.putVal` **76 847** in the chain arm (`hs2-CM.log`) against **30 035** in the default
(`hs2-D.log`); `HashMap.put` 32 807 in the chain arm; and every chain-arm compile of these methods is
lowered TWICE (two identical `[ir] linear scan` / `splice-frame-states` line pairs; the one retry path
for an identical graph is the code-buffer overflow retry of `lower_inner_with_array_list`,
`CRATONVM_DBG_IR_BUFSIZE=1` says), where the default lowers once. 110 more appended
bytecodes cannot make 46 KB; something emitted per point scales with the chain arm.

**The mechanism, by reading (`ir_lower.rs`).** `plan_slots` PINS every value any snapshot names
(`SlotClass::Pinned`: a frame word nothing else ever takes, for the whole method). The chain arm
records a chain snapshot at EVERY instruction of every spliced body, naming the callee's locals
and operand stack and every caller scope's; in the default arm a spliced body has no snapshot, so
its temporaries share words. Every reference temporary of `putVal`/`getNode`/`newNode`/
`treeifyBin` therefore got its own home. That matters because of how an IR body publishes its GC
roots: with at most `FRAME_BLOCK_MAX_HOMES` (32) distinct reference homes it publishes a frame
block ONCE per activation (`plan_frame_block`, `emit_frame_block_ensure`); past 32 it falls back to
`emit_shadow_push` + `emit_shadow_reload` at EVERY GC point (every call, the slow path of every
`new`): a load/store/lea per defined reference home before the call and a copy back after it.
That is quadratic code (GC points x homes: the 46 KB) and linear per-call cost at run time (the
kernel's `Integer.valueOf` calls, 3 per put+get pair, each pushing and restoring every reference
home the spliced bodies defined). It fits every row above, including the field-map row.

**Landed (pending build): `CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS`, default ON, inert without
chain snapshots.** `plan_slots` no longer pins a value named only by CHAIN snapshots (locals and
stack; their monitors stay pinned). `chain_read_colour_ranges` instead treats every scheduled node
that could publish such a snapshot as a USE of every value it names -- any node at the snapshot's
raw pc, an `Op::Guard` whose `bci` is that pc, a node naming it (`frame_snapshot`), a superset of
`Lowerer::splice_chain_snapshot_for_site` -- and runs `plan_slots`' own backward dataflow over
those reads, so the value's word is held from its definition through every such guard, loops
included, and only then shared. Sound because a chain snapshot has exactly one reader: the box of
a guard lowered at its raw pc (`splice_chain_for_site`, from `emit_deopt_unless` and the
`Op::Guard` arm; `build_deopt_points` skips every chain snapshot; every `emit_deopt_*` caller
passes the lowered node's own pc). The published `SlotPlan::range` is unchanged (residency's
second opinions and the dead-home clears read exactly what they read before); the colouring and
`verify_slot_colouring` use the wider `SlotPlan::colour_range`. A dataflow that does not settle
pins, as before. Unit test `ir_lower::..::r14w1_chain_named_value_is_ranged_to_its_reader_not_pinned`.
`CRATONVM_DBG_IR_SLOTS=1` prints `chain_ranged=N` on each compile's `[ir-slots]` line.

Also landed: CH5-4 (`ir_splice_multi_return_enabled` read per call, no `OnceLock`), so the flip is
one word there.

**Left open, for the orchestrator (nothing was run by this lane):**

1. Confirm the mechanism on the pre-wave binary and the fix on the new one, chain arm,
   `CratonBench hashmap`:
   `CRATONVM_DBG_JITC=1` (the `full-compile java/util/HashMap.putVal ... len=` and `put` lengths,
   and whether the kernel/putVal are lowered once), `CRATONVM_DBG_IR_RELOC=1 | grep -c
   "frame_block=\[\]"` for `hashMapPutGet`/`putVal` (an empty frame block with long `slots=` lists
   = the per-call path), `CRATONVM_DBG_IR_SLOTS=1` (`slots=` and `chain_ranged=`). Then the same
   with `CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS=0` (the round-13 chain arm).
2. Soundness: the chain arm + `CRATONVM_DEOPT_EAGER_CHAINS=1` over the R13 chain battery and
   `C:\craton\jitr14-probes\src\R14ChainHashShapes.java` (answers must match HotSpot; a word handed
   out too early shows as a changed `sum-*` line or `bad > 0`).
3. Time default / chain / chain + `RANGE_PINS=0` interleaved (`bench14.sh`, `R14ChainHashShapes`,
   `R13FramestateHashmapKernel`, `R13Iropt8HashKernels`). If the chain arm is still over the cap
   in the kernel (`frame_block=[]` in step 1), the frame-block cap is the next lever:
   `r14w1-chain-frame-block-home-cap-patch-FIXED-20260929.md`.

Status stays OPEN until step 3 reads the chain arm at least neutral; then flip the three switches
(`runtime_flag_default_on` in `ir_splice_frame_states_enabled`, `ir_splice_chain_fences_enabled`,
`ir_splice_multi_return_enabled`) and re-take the tests pinned to the old default.

### Round 14 wave 1 (lane chain), hand-back: the cap knob and the lowered-twice retry

Applied `r14w1-chain-frame-block-home-cap-patch-FIXED-20260929.md` (FIXED, pending verification):
`CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` (numeric, default 32, ceiling 1024) with a
`CRATONVM_DBG_IR_RELOC` refusal line; and `CRATONVM_JIT_IR_BUFFER_REF_HOME_RESERVE` (default ON),
which sizes the first code buffer of a body over 32 reference homes for its per-GC-point
publication, so such a body (the chain arm's `putVal`, the kernel) should no longer be lowered
twice. Timing arm to add to step 3: chain + `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES=128`.

## Round 14 wave 2 (lane guard)

CH5-5 (the guarded splice's miss edge as a trap) landed default OFF
(`CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS=1`; see the note under CH5-5 in
`jit-r13-chain5-proposals-RETIRED-20260929.md`). By reading it is NOT a lever for this page's rows:
`CratonBench.hashMapPutGet`'s `map` is a single-store local of `new HashMap` and is already
proven exact (W3-1), and the `hashmap-field` row reads the field at each use (two `Load` nodes, so
a fact on the first never reaches the second). It helps a map held in a LOCAL whose class the
profile alone names (`Map m = this.m; m.put(..); m.get(..)`). The field shape needs load
forwarding across the spliced `put` (`jit-r14-guard-proposals.md` G14-1). Nothing else here
changed; status unchanged.

## Round 14 wave 3 (lane chain)

Re-read against the tree: the wave-1 levers are in (`CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS`,
`chain_read_colour_ranges` in `ir_lower.rs`; `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES`;
`CRATONVM_JIT_IR_BUFFER_REF_HOME_RESERVE`), and every remaining step on this page is a measurement
(steps 1-3 of the wave-1 section, plus the `MAX_HOMES=128` arm). The page does not name CHN-1
(range pins for FLAT snapshots, `jit-r14-chain-proposals.md`), so it was not taken: its first step
is itself a census (`CRATONVM_DBG_IR_SLOTS` Pinned homes vs `peak_live`), and without the step-1
numbers it is not known whether the kernel is still over the frame-block cap.

One change this wave that bears on the arm's timing rather than on this kernel: a chain trap that
keeps failing at a call site now withdraws the splice there instead of recompiling the same splice
until the caller is retired (`r14w3-resume-chain-trap-despec-never-withdraws-the-spliced-guard`).
`CratonBench hashmap` should not trap in steady state, so no change is expected in its row; the
`R14ResumeOsrChainExit` / `R14ChainDespecSplice` timing lines are where it shows. Status unchanged.
