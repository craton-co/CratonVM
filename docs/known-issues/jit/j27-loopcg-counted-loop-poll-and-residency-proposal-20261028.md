# Proposal: a strip-mined counted-loop poll and register-resident unrolled temporaries

> **j27 wave 8 (lane W8CHA, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION; item 2 "Schedule for pressure" is ALREADY IN CODE (no new work).** No code change.
> * **Item 2, schedule lever.** "Place a single-reader pure node right before its reader" is `ir_schedule::pair_single_use_operands` (`CRATONVM_JIT_IR_PAIR_OPERANDS`, default ON; at most `CRATONVM_JIT_IR_PAIR_MAX_CROSS` = 2 nodes crossed, round 17), and the pressure-aware pick is `priority_sort_block_pressure` (`CRATONVM_JIT_IR_LIST_SCHED_PRESSURE_SOURCE`, default ON since round 17: bytecode order once 5 block-local values are live). The page's residual shape (an `i + k` read by a load AND a compare) has two readers, so neither lever applies to it. What remains for it is the `x + c` rematerialisation (REMAT block, opt-in). Triage: `CRATONVM_DBG_IR_LINEAR_SCAN=1` on `J27LoopResidencyRows` `pressureInt`, arms default vs `CRATONVM_JIT_IR_PAIR_OPERANDS=0` vs `CRATONVM_JIT_IR_LS_REMAT_ADD=1`; only the last should lower `load=` for the two-reader values.
> * **Item 3 review** (`jit/src/ir_lower/ir_page_poll.rs`, `Lowerer::emit_safepoint_poll`): the page-form slow path reaches the byte poll's slow path byte for byte (argument 0 is the baked `safepoint_flag_addr` immediate, not a register the prefix left); R11 is the only register the prefix writes; the outlined key is the faulting site, which no byte-form `JNZ` field can share; a declined registration refuses the body. Still not built: a forced-exit route to a page-form poll, so every mode-exit poll keeps the byte form.
> * **Item 1** unchanged (STRIPMINE block).

> **j27 wave 5 (lane UNROLLRES, 2026-10-28): item 3 "A one-instruction poll" PARTLY (the back-edge page form
> is in code, opt-in), PENDING HOST VERIFICATION.**
> **Switch.** `CRATONVM_JIT_IR_PAGE_POLL=1` (opt-in). It was written without a build, and no unit test can
> drive its fault path. Module: `jit/src/ir_lower/ir_page_poll.rs`.
> **Hot path.** An IR back-edge poll runs `MOV R11, <seg>:[cell]; TEST [R11 + pair_base], EAX`, the single-pass
> page form's bytes. Outlined (the default `CRATONVM_JIT_IR_BACK_EDGE_POLL_OUTLINE`), that is the whole hot path:
> two instructions, no flag test, no `Jcc`. The byte poll took five plus the `JNZ`.
> **Slow path.** The fault's recovery pc is the outlined slow block (`emit_outlined_polls` binds it in
> `bind_outlined_poll_slow`). Inline, a `JMP rel32` takes the `JZ`'s place, and the recovery pc is the slow
> path's first byte. The slow path is byte for byte the byte poll's: the fault leaves every register but R11,
> and the flags.
> **Registration.** The `(site, slow)` pairs are registered in the implicit-null table right after
> `CompiledMethod::try_new` in `lower_inner_attempt`. A declined site refuses the body (`ir-page-poll-declined` /
> `ir-page-poll-entry-not-base`).
> **The byte poll, unchanged, for:**
> * a poll with a mode exit (the forced-exit patch needs the `Jcc`), which covers every admitted method-entry
>   and OSR body's back edge;
> * the stress countdown;
> * the owed-cycle latch;
> * the method-entry poll;
> * no pair in disp32 reach, or `poll_page::emit_enabled()` false (`CRATONVM_JIT_PAGE_POLL=0`, or no
>   implicit-null table).
>
> **Not built.** A route for the forced exit to reach a page-form poll, for example arming the thread's cell, so
> mode-exit polls could take the form too.
> **Unit tests.** `cargo test -j 5 -p cratonvm-jit --lib ir_page_poll` covers the admission, the bytes against
> the single-pass encoding, the inline skip opcode, and the opt-in reader.
> **Verify (host, `--jdk-only` and `--compatible`).**
> * `tools/probes/jit/j27/J27StripMineRows.java` and `J27LoopResidencyRows.java`: every `fact` line equal to
>   their expected lines, in arms default and `CRATONVM_JIT_IR_PAGE_POLL=1`. The `safepoint-*` rows must finish
>   and answer `true`; a hang means a page-form poll that no pause reaches.
> * Engagement: `CRATONVM_DBG_JIT_DISASM` shows `test dword ptr [r11+...], eax` at an IR back edge.
> * The GC battery's time-to-safepoint must be unchanged.
>
> **Decision (flip default ON).** The parent page's icount rows (`counterInt`, `twoCells`, `run`) default vs `=1`.

> **j27 wave 4 (lane REMAT, 2026-10-28): item 2 "Rematerialise `x + c`" PARTLY (the reload half is in code,
> opt-in; skipping the home store is NOT BUILT), PENDING HOST VERIFICATION.**
> `CRATONVM_JIT_IR_LS_REMAT_ADD=1` (opt-in, written without a build; inert unless `CRATONVM_JIT_IR_LS_SPLITS`
> splits a value). The split plan's planned `SpillKind::Load` of an `int`/`long` `Add(x, Const)` (either side) or
> `Sub(x, Const)` becomes `LsTransition::RematAdd` (`ir_lower::ls_split_transitions` via
> `ls_remat_add::ls_reload_transition`; shape `regalloc::remat_add_of`, the addend normalised and wrapped at the width:
> `x - MIN` is `x + MIN`). At emission (`Lowerer::ls_emit_remat_add`) it is `LEA r32, [base + disp32]` for `int`
> (32-bit sum, zero-extended, the image `emit_add_lea` writes), `LEA r64, [base + disp32]` for a `long` whose addend
> fits, `MOV r64, imm64; ADD r64, base` for a wider `long` addend only while the flags are dead
> (`remat_add_form`; a transition sits at a node position, where no lowering leaves flags live --
> `FLAGS_LIVE_AT_NODE_POSITION`), and the planned home load whenever the base is not provably in a register at that
> moment (`resident_gpr` answers `None`, the base is carried, or the base is in the destination), whenever a
> reachable deopt names the value (deopt naming wins), and never for a `Ref` (the shape is `int`/`long` only, so
> GC maps do not move). Unchanged: the home store (managed homes are never dropped, so deopt / OSR / edge copies read
> what they read before); admission (`split_register_benefit` prices a `RematAdd` as the load it may degrade to);
> sequencing (its source is `Slot(node)`, like a load); the fired census counts a remat under `remat=` and a
> degraded one under `load=`. Not built: dropping the home store of a value every reader can recompute (the base's
> residency would have to be proved at every reader, not checked at emission) -- that is where the deopt refusal
> starts to matter. Note for the decision: the deopt refusal is conservative for this increment (the home is still
> written) and may leave little engaged in loops whose back-edge poll names the value.
> Verify (host, `--jdk-only` and `--compatible`): `cargo test -p cratonvm-jit j27_remat` (`regalloc::remat_add::tests`
> shape; `ir_lower::ls_remat_add::tests` bytes, the int wrap at MAX executed, the flags-live refusal of `ADD`, LEA
> keeping ZF executed; `ir_lower::ls_remat_add_tests` the planner arm on/off, the emission on an installed plan --
> LEA bytes, deopt-named / non-resident / base-in-destination fall back to the home load -- and the RESIDENCY
> pressure loop's Java answers in every arm); `tools/probes/jit/j27/J27RematRows.java` every `fact` line equal to the
> lines below in arms default, `CRATONVM_JIT_IR_LS_REMAT_ADD=1`, and `=1` with `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY=1`.
> Engagement: `CRATONVM_DBG_IR_LINEAR_SCAN=1` -- `[ir-ls] splits emitted: ... remat=` must rise under the switch on
> `pressureInt` / `pressureLong` (and `load=` fall by the same count). Decision (flip default ON): the parent page's
> icount rows and `-Dj27.time=1` `time pressureInt-1e8` / `pressureLong-1e8` default vs `=1`.
>
> Expected HotSpot 25 output of `J27RematRows` (modelled from Java int/long arithmetic), each prefixed `fact `:
> `warm 7028371784424783496`; `pressureInt-0-5 10`, `-1-5 56`, `-2-5 136`, `-7-5 21869`, `-1000-5 -1646835881`,
> `-1000000-5 1688274130`, `pressureInt-1000-max500 -126679533`, `pressureInt-1000-min -933158970`;
> `pressureLong-0-5 10`, `-1-5 3435973836832`, `-2-5 10033043603526`, `-7-5 1127961491151195`,
> `-1000-5 1529148870705792673`, `-1000000-5 2113506839693599970`, `pressureLong-1000-max500 -5067456112786380853`;
> `wrapInt-max -2`, `wrapInt-max1 -2`, `wrapInt-min 2`, `wrapInt-m1 -2`, `wrapInt-0 2`; `wrapLong-max -2`,
> `wrapLong-min -8589934590`, `wrapLong-m1 -2`, `wrapLong-0 -8589934590`, `wrapLong-2p31 2`;
> `throwInside-5000-2500 caught i=2500 acc=9385581 / by zero`, `throwInside-10-3 caught i=3 acc=142 / by zero`,
> `throwInside-20-100 done acc=1024`.

> **j27 wave 2 (lane RESIDENCY, 2026-10-28): item 2 PARTLY (the pressure-aware assignment is in code, opt-in; the
> `x + c` rematerialisation is NOT BUILT), PENDING HOST VERIFICATION.**
> What the code already did: the IR linear scan gives `int`/`long` loop phis and temporaries callee-saved GPRs and, by
> default, drops the home store of a resident value no reachable deopt names (`value_home_droppable` /
> `phi_home_droppable`), so a phi WITH a register is already store-free in the body. The loss is in WHO gets the five
> registers: `ls_pick_victim` divides the next-use distance of every carried-and-read value by 64, which scores a loop
> INVARIANT (defined before the loop, one definition for every trip) the same zero as a phi, so under pressure the
> tie went to next-use distance and a body temporary re-arriving at its read could evict a phi mid-loop (a split phi
> is refused by `plan_register_residency`, so it then lives in its home for the whole method).
> `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY=1` (opt-in; `jit/src/regalloc/loop_residency.rs`, one rule `loop_def_role`:
> loop depth of the definition against the deepest read): a loop invariant keeps no carry relief; a value redefined
> every trip weighs its reads PLUS one definition per trip (`loop_spill_weight`, the C2/LLVM spill-weight shape) and,
> when carried, loses every next-use-distance tie (`ls_pick_victim_with`); the loop-carried reservation
> (`ir_lower::carried_reservation`) ranks by the same weight. Only the choice of victim changes: `Ref` is still
> refused by type, a split value still refused, deopt naming / home dropping / `verify_allocation` unchanged, the
> termination guard (`a.lo >= current.lo`) untouched. Off = the pre-j27 score exactly (`ls_pick_victim` is now the
> test-only `false` form). Not built: rematerialising `Add(x, Const)` at its readers -- the IR consumer of the split
> plan takes only `Remat::Const` (`ls_split_transitions`), `LsTransition` has no `lea` form, and a remat that skips
> the home store needs the base's residency proved at every reader; that is the next increment. Not changed: the
> crossblock admission price (`carried_w`) stays read-weighted, so `CRATONVM_JIT_IR_REG_ALLOC` single-use admissions
> are not made harder by this switch.
> Verify (host, `--jdk-only` and `--compatible`): `cargo test -p cratonvm-jit j27_residency` (regalloc
> `loop_residency::tests` -- the OFF arm splits the phi, the ON arm keeps it whole and evicts the invariant -- and
> `ir_lower::loop_phi_residency_tests` -- every carried phi of a 4-phi/2-invariant loop resident under the switch, the
> executed answer equal to Java's in both arms); `tools/probes/jit/j27/J27LoopResidencyRows.java` every `fact` line
> equal to the lines below in arms default and `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY=1` (and once more with
> `CRATONVM_JIT_IR_REG_ALLOC=1` added). Engagement: `CRATONVM_DBG_IR_LINEAR_SCAN=1`'s `[ir-ls]` phi census on
> `pressureInt` / `cellsLong` -- `no_register` must fall under the switch. Decision (flip default ON): the parent
> page's icount rows (`run`, `runStatic`, `twoCells`) and `time pressureInt-1e8` / `cellsLong-1e8` default vs `=1`,
> `probes/RegPressure.java` not slower (the shape that sank `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK`).
>
> Expected HotSpot 25 output of `J27LoopResidencyRows` (modelled from Java int/long arithmetic), each prefixed `fact `:
> `warm -2163733899907927777`; `pressureInt-0 3`, `-1 28`, `-2 107`, `-5 621`, `-10 3231`, `-100 3556516`,
> `-1000 -1415282113`, `-100000 -1170721005`, `-10000000 -1169861181` (rows `pressureInt-<n>`);
> `cellsLong-0-5-3 0`, `cellsLong-1000-5-3 -7073999394286606408`,
> `cellsLong-1000000-123456789--77 1404844454358779678`, `cellsLong-10000000--1-1099511627776 8504361719791957118`;
> `callLoop-0 33`, `callLoop-9 36336`, `callLoop-10 44205`, `callLoop-1000 1733509017`, `callLoop-100000 -784111949303`;
> `throwInside-5000-2500 caught i=2500 a=7069 b=6366 c=615454 / by zero`, `throwInside-5000-6000 done a=0 b=1 c=5002`,
> `throwInside-10-3 caught i=3 a=1833 b=1316 c=2175 / by zero`; `chaLoop-switch-100000-50000 804748458129855`,
> `chaLoop-tri-1000 191030902`, `chaLoop-square-1000 10320934686`; `allocLoop-0 1`, `allocLoop-16 4473700099`,
> `allocLoop-3000000 320309852319`.

> **j27 wave 2 (lane STRIPMINE, 2026-10-28): PARTLY (item 1 FIXED IN CODE, opt-in; items 2 and 3 NOT BUILT), PENDING HOST VERIFICATION.**
> (Partial host evidence, orchestrator log, snapshot s4: the J27 probes are SAME in both modes in the strip-mine
> arms `=1` / `ITER=1` / `ITER=7`; the engagement lines and the flip A/B are not in the log yet.)
> Item 1: `jit/src/ir_optimize/strip_mine.rs` (run last in `optimize_with`, after every loop pass) strip-mines the
> MAIN loop of a main/post unroll (`Graph::partially_unrolled_headers`, the pc-less header) whose body is pure
> (arithmetic, compares, conversions, constant divisions; no memory phi, no load/store/call/guard, no snapshot naming
> a loop value) and whose exit still reaches its post loop: `lim2 = min(lim, MAX - S + 1)`, outer `O: q = phi(i0, i)`,
> `slim = (int) min((long) q + N*S, (long) lim2)`, the inner test `i < slim`, the outer test `i < lim2` on the inner exit.
> `lim2` keeps the poll-free inner loop from ever wrapping `i`: a trip that could wrap leaves the main loop early and its
> post loop (the original loop, polling every trip) runs it, which is exact (main/post's own argument). Inner headers are
> recorded in `Graph::strip_mined_inner_headers`; `Lowerer::emit_safepoint_poll` emits no poll on a latch whose every
> retreating successor is one (`ir_optimize::latch_enters_only_strip_inner`, the one place that decides). OSR: neither the
> inner nor the outer header carries a bci, so no OSR entry or poll mode exit can land in them; the OSR door keeps
> entering the original header (the post loop). Switches: `CRATONVM_JIT_IR_STRIP_MINE` (opt-in `=1`),
> `CRATONVM_JIT_IR_STRIP_MINE_ITER` (inner trips per strip, default 1000 = C2's `LoopStripMiningIter`, clamp 1..2^20).
> Not taken: a loop with a bci (an ordinary loop, a post loop, an OSR-door body -- so the `counterInt(1_000_000)` OSR
> row does not move), a body with array/field reads (`ir_check_elim` reads the main test `i < n - d`, which `slim`
> would hide), a `long` induction; see `j27-stripmine-osr-and-array-bodies-proposal-20261028.md`.
> Verify (host, `--jdk-only` and `--compatible`): `cargo test -p cratonvm-jit j27_stripmine`;
> `tools/probes/jit/j27/J27StripMineRows.java` -- every `fact` line equal to the expected lines below in arms default,
> `CRATONVM_JIT_IR_STRIP_MINE=1`, `CRATONVM_JIT_IR_STRIP_MINE=1 CRATONVM_JIT_IR_STRIP_MINE_ITER=1` and `=7`; the probe
> must finish (a hang in `safepoint-*` = a worker that never reaches a poll). Engagement: `CRATONVM_DBG_IR_COMPILES=1`
> prints `[ir] strip mine: N loop(s), K trip(s) per poll`, `CRATONVM_DBG_UNROLL=1` names each refusal. Decision (flip
> default ON): the parent page's icount rows (`counterInt`, `twoCells`, `run`) and `time mix-1e8` default vs `=1`, the GC
> battery's time-to-safepoint unchanged.
>
> Expected HotSpot 25 output of `J27StripMineRows` (modelled from Java int/long arithmetic):
> `fact warm 8040375334766268280`; `mix-0-0 1`, `mix-0-1 31`, `mix-0-7 1773379906`, `mix-0-1000 133786869`,
> `mix-0-1000000 -1656710879`, `mix--500-500 123083509`, `mix-2147482647-2147483647 413006453`,
> `mix--2147483648--2147482648 133786869`, `mix-2147483642-2147482647 1`, `mix-0-100000000 -1508126591`,
> `mix-1847483647-2147483647 230900097`; `mixStep3-0-3001 376456827`, `mixStep3-2147482648-2147483647 1123731433`,
> `mixStep3-2147480647-2147483647 1495475805`, `mixStep3--2147483648--2147453648 281774233`;
> `mixStep7-2146783647-2147483647 2059761969`, `mixStep7--2147483648--2147483641 -2147483617`,
> `mixStep7--7000-7000 -503752359`; `sumSq-0 0`, `sumSq-1000 332833500`, `sumSq-1000000 333332833333500000`,
> `sumSq-50000000 -4529445843202100544`; `twoAcc-0 1`, `twoAcc-7 854583`, `twoAcc-1000 592523609`,
> `twoAcc-1000000 -1781274111`; `divLoop-zero java.lang.ArithmeticException: / by zero`, `divLoop-ok 870`,
> `throwAt-2500 java.lang.IllegalStateException: acc -2068156991 at 2500`,
> `arraySum-oob java.lang.ArrayIndexOutOfBoundsException: Index 3000 out of bounds for length 3000`, `arraySum-ok 0`,
> `safepoint-gc-returned true`, `safepoint-stack-walked true`, `safepoint-worker-finished true`,
> `safepoint-worker-answers true`, `safepoint-mix-0-200000000 -784883455` (each line prefixed `fact `).

Status: PARTLY, PENDING HOST VERIFICATION (item 1 built opt-in in j27 wave 2, lane STRIPMINE; item 2 partly
built opt-in in wave 2, lane RESIDENCY, and wave 4, lane REMAT; item 3 not built; see the blocks above).
Proposed j27 wave 1, lane LOOPCG, 2026-10-28.
Area: `jit/src/ir_optimize.rs` (unroller, a new strip-mining pass), `jit/src/ir_lower.rs`
(`emit_safepoint_poll`, the linear scan's register file), `jit/src/ir_schedule.rs` (intra-block priority).
Parent: `docs/known-issues/jit/jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`. j27 built the
reassociation (`CRATONVM_JIT_IR_LINEAR_REASSOC`) and a 5-instruction thread-word poll
(`CRATONVM_JIT_IR_POLL_WORD_SELECT`); these are the parts left.

## 1. Poll once per strip, as C2's loop strip mining does

C2 (`UseCountedLoopSafepoints` + `LoopStripMiningIter=1000`) splits a counted `int` loop into an inner loop of
at most 1000 trips with no safepoint and an outer loop that polls once per strip. The IR's main/post form polls
once per unrolled main trip (every 4 iterations).

Shape: for a counted loop `for (i = i0; i < n; i += s)` with a pure (deopt-free, call-free) body, build
`outer: lim = min(n, i + 1000*s) (computed in long, so no overflow); inner: while (i < lim) body; poll; if (i < n) goto outer`.
What must hold:

* **Time to safepoint.** One strip of a register-only body is ~1000 x a few ns: well under the stop-the-world
  latency the barrier tolerates; a body with a slow op (division, a helper) needs a smaller strip or no strip
  (cap the strip by a body cost estimate).
* **Deopt.** The inner loop has no poll, so no mode exit there; a guard inside it deopts with its own frame as
  today. The outer poll's mode exit names the header state exactly as the back-edge poll does now
  (`back_edge_mode_exit_state`), so it needs the outer header to carry the original header's bci.
* **Trip math.** `lim` in `long` (`(long) i + 1000L * s`, then `min` with `n`), so `i + 1000*s` cannot wrap past
  `n` near `Integer.MAX_VALUE`; a negative stride mirrors it with `max`.
* **OSR.** An OSR entry stub enters at the original header; it must enter the OUTER loop (it re-computes `lim`).

Decision rule: `R20PLoopStaticCells` / `R21TypecheckStaticInit` facts = HotSpot, and the icount rows of the parent
page; GC battery (time-to-safepoint) unchanged.

## 2. Register-resident unrolled temporaries

After the reassociation, what is left of the page's homed temporaries is any loop value with more than one
reader inside a trip (an `i + k` index read by a load and a compare). The accumulator lowering homes a value the
linear scan does not give one of its five callee-saved registers. Two cheaper levers than increment 3:

* **Schedule for pressure.** `ir_schedule`'s priority sort places independent `lea`s first (latency), which makes
  their ranges overlap; a pressure-aware tie-break (place a single-reader pure node right before its reader)
  lets the RAX carry (`op_home_is_one_store_rax`) take it without a home.
* **Rematerialise `x + c`.** An `Add(x, Const)` whose `x` is register-resident can be recomputed (`lea`) at each
  reader instead of homed; one instruction either way, no store / reload pair.

Decision rule: the parent page's table, default vs each lever.

## 3. A one-instruction poll

`CRATONVM_JIT_PAGE_POLL` (opt-in, single-pass only today, `jit/src/poll_page.rs`) is the branch-free page form.
Porting it to `Lowerer::emit_safepoint_poll` needs the IR lowerer to register `(site, slow)` in an
implicit-null table and is refused for a poll with a mode exit (the forced-exit patch rewrites a `Jcc` the page
form does not have); a back-edge poll of an admitted method-entry body has one. So the port pays only on bodies
without mode exits unless the forced exit learns another route (e.g. arming the thread's page cell).
