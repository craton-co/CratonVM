> **j27 wave 9 (lane W9VER, 2026-10-29): VERIFIED (Windows) for facts and unit tests; the host-number rows stay open (perf, out of scope this wave).** Windows, release LTO-off build of the tree at `47479dc6e0` plus the other wave-9 lanes' WIP (`C:\craton\j27-ver-bin\cvm-ver-2.exe`, test exes from the same tree), HotSpot 25.0.3 as the reference. `cratonvm-jit --lib`: `j27_sdb17_3` 1, `monitor_rows` 14, `r11w9_mid_tests` 14, `r16w16_monitor4` 9, `r17_monitor5` 9, `r19_syncdoor3` 4, `self_lock_census` 3 passed; `cratonvm-vm --lib j27_sdr17` 1 passed. `J27MonitorRows` (24 facts) equals HotSpot in `--jdk-only` and `--compatible` at the default and with `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS=1 CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE=1`. `R24CfCatchLateArm` not run (needs its native library). The MON5-5 / SDB17-4 / MON3-4 / SDB17-3 / MON17-5 / MON17W2-2 rows wait on host counts and timings, so the page is not a retirement candidate yet.

# j27 MONROWS: the JIT monitor / sync-door rows of the round-18 backlog

> **j27 wave 8 (lane W8DSP, 2026-10-28): REVIEWED (no change), PENDING HOST VERIFICATION.** Wave 7 re-read every
> row and found every piece of wave 4's code present; nothing was added since. Every remaining row waits on a
> host number (MON5-5 / SDB17-4 counts, MON3-4 disassembly count, SDB17-3 census ratio, MON17-5 and
> MON17W2-2 flip runs), so no code is written here. The wave 4 verify recipe at the end of this page is the
> complete host run, in order; flip MON17-5 / MON17W2-2 only by their stated flip rules.

> **j27 wave 7 (lane T1IC, 2026-10-28): PARTLY (unchanged), PENDING HOST VERIFICATION. No row is open in code.**
> I re-read every row against the tree and built nothing. Every piece of code the wave-4 block below names is
> present:
> * MON17-5: `monitor_rows::block_admits_array_op` at `ir_optimize.rs` `plan_loop_block_coarsening`.
> * MON17W2-2: the planner arm, `note_field_monitor_base_proof` at both rewrites, and the `ir_lower` block-start
>   seed `dominated_monitor_bases`.
> * SY16-3: `note_sync_call_pair` at both `ir.rs` call arms, and `sync_call_halves_agree` in
>   `coarsenable_monitor_pair`.
> * SDB17-3: `releasing_exit_admitted`, `self_lock_handler_body_refused_only_for_shared_exits`, census row
>   `body-ref-handler-shared-exits`.
> * SDR17-1: `CalleeHandlerMiss::Unresolved`, `callee_rerun_refusal_what` and its test.
> * Probe `J27MonitorRows.java` / `.c`.
>
> What is left waits on the host, not on code:
> * MON5-5 and SDB17-4: their host readings, as the orchestrator asked.
> * MON3-4: the disassembly count.
> * SDB17-3: its census ratio, which decides whether the hand-over twin is worth building.
> * MON17-5 and MON17W2-2: their flip runs.
>
> Review notes (read, no defect):
> * **MON17W2-2's seed is sound on exceptional paths.** It is not confined to the loop: every block that the proof
>   read's block strictly dominates is seeded, loop exits included. That holds only if no exceptional edge can
>   leave that block before the read completes. The planner guarantees it, because it refuses a body node that
>   could call, and a body whose implicit-exception pad lies outside the body. So a path into a strictly
>   dominated block has run the read to completion, and a null base left at copy 0.
> * **SY16-3's records survive the pipeline.** No pass renumbers node ids. Neither unroller (`unroll_in`,
>   `cfg_unroll`) clones a `Call` or a monitor, so no copy of a caller-held pair goes unrecorded. A pair spliced
>   later would at worst stay unmerged or hit the old SB1 compile refusal: a performance cost, never a wrong
>   answer.

> **j27 wave 4 (lane MONROWS, 2026-10-28): PARTLY FIXED IN CODE, PENDING HOST VERIFICATION.** Four rows have code (MON17-5 and MON17W2-2 opt-in, SY16-3, SDR17-1), one has a census row so it can be measured (SDB17-3), two are not live as written (MON3-4, SDB17-5), and two wait on a host reading (MON5-5, SDB17-4). Probe `tools/probes/jit/j27/J27MonitorRows.java` (+ the JVMTI shim `J27MonitorRows.c`), whose header gives the expected HotSpot 25 lines. Verify recipe at the end.

Status: OPEN (until the host run below)
Area: JIT monitors / sync doors (single-pass monitor emission, block coarsening, compiled local handlers, call-site handler service)
Found by: j27 wave 4, lane MONROWS (code-only re-check of the nine CODE rows in `j27-r18-backlog-live-rows-20261028.md`; originals in `jr21-r18-backlog-retriage-20261015.md` and the r17 proposal pages quoted from git `6258e0f64e`)

## Row by row

### MON17-5 -- block coarsening takes no array ops: FIXED IN CODE (opt-in)

`plan_loop_block_coarsening` (`jit/src/ir_optimize.rs`) admitted only `Load`, `Store`, a direct `LoadStatic` / `StoreStatic` and a `Guard` at its own bci as block nodes that can leave. Now `jit/src/ir_optimize/monitor_rows.rs` `block_admits_array_op` also admits `ArrayLength` and a PRIMITIVE `ArrayLoad` / `ArrayStore` under `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS=1`. Their null and bounds checks deopt at their own bci (`ir_lower` arms, deopt-on-fault), so each copy stamps its clone with a per-copy frame that holds the monitor once, exactly as for a `Load`. A reference element store (`jit_aastore`) and a reference element load (ZGC barrier helper call) stay refused. An access the implicit-exception edges armed never reaches the rule: its pad and the block's catch-any handler lie outside the loop body and the planner refuses that loop ("a body value is read outside the body").

Opt-in because the executed half (a lowered, run coarsened block with an element access that faults in copy k) has not run. Flip rule: the unit tests below green, `J27MonitorRows` HotSpot-identical with the switch on (in particular `ring-aioobe*`), the battery SAME, and a DBG_JITC census showing coarsened array blocks.

### MON17W2-2 -- no cross-block null proof for non-receiver field-monitor bases: FIXED IN CODE (opt-in, follow-up after the ir_lower lanes closed)

Was live: `plan_loop_block_coarsening` refused a `BlockMonitor::Field` whose base is not the seeded receiver ("the monitor's getfield base is not the receiver (its re-read would need a null check)"). The re-read in copies 1..K has no honest frame, so it must be unable to leave, and only the receiver got a per-block null seed in the lowering.

Now, under `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE=1` (opt-in):

* the planner admits any invariant base (the receiver rule stays as it was with the switch off);
* both rewrites record `(base, copy 0's read)` in the new `Graph::monitor_base_proofs` (`monitor_rows::note_field_monitor_base_proof`; forked shape: the fast path's read `l0`; in-place shape: the original read);
* `ir_lower` seeds `base` non-null at the start of every block that the proof's block STRICTLY dominates (`monitor_rows::dominated_monitor_bases`, called right after `seed_block_null_proofs`), so the re-reads of copies 1..K lose their null test. A proof that a later pass killed or rewrote seeds nothing (`proof_still_reads`). The read's own block keeps its test: the existing block-local proof (`note_receiver_non_null`) covers the rest of that block.

Why the NPE behaviour cannot move:

* Copy 0's read is never seeded: its block is not strictly dominated by itself, and the loop header is not dominated by the body. So a null holder still fails at copy 0's read: the slow helper raises the NPE at bci of the `getfield`, with JEP 358's message, at copy 0's pre-enter frame, and no monitor of the group has been entered.
* A null `lock` still fails at copy 0's guard on the read, which deopts to the pre-enter frame; the interpreter's `monitorenter` raises "Cannot enter synchronized block because ... is null".
* The slow (non-group) path is the original body, which is untouched.
* A `lock` changed to null by another thread mid-group fails the copy's identity test. The group releases and the header re-runs copy 0, which then throws as above.

Probe rows: `holder-loop`, `holder-loop-null-holder`, `field-monitor-null-lock`, `field-monitor-null-holder`. Unit tests: `monitor_rows::tests::{switched_off_a_non_receiver_field_monitor_is_refused_as_before, switched_on_a_non_receiver_field_monitor_is_planned, switched_on_copy_zeros_read_is_recorded_as_the_base_proof, switched_off_nothing_is_recorded, a_base_is_seeded_only_below_its_proofs_block}`. Flip rule: those green; `J27MonitorRows` HotSpot-identical with the switch on; `CRATONVM_DBG_JITC` shows `holderLoop` "coarsened" with the switch on and "getfield base is not the receiver" with it off; the battery SAME.

### MON3-4 -- the single-pass monitor flushes registers eagerly: OBSOLETE (no payoff), PENDING HOST CONFIRMATION

`x64/op_object.rs` `emit_monitor_on_stack_top_with` still calls `flush_scratch_registers` before the inline thin-lock path, but that flush (`x64/operand_stack.rs`) stores only OPERAND-STACK entries held in scratch / low XMM registers. At a javac `monitorenter` / `monitorexit` the operand stack holds exactly the receiver (`dup; astore; monitorenter` and `aload; monitorexit`), and the very next block of the same function must give that receiver a frame home anyway (it is what the stub and the shadow map read). So for javac-shaped code the "eager flush" emits the one store the site needs regardless; there is nothing for a slow-path-only flush to save. Only non-javac bytecode that leaves operands beneath a `monitorenter` would pay, and none is known.

Host confirmation: `CRATONVM_DBG=jit-disasm,jitc cratonvm -cp /tmp/j27mon J27MonitorRows` and, in the single-pass body of `ring`, count the `MOV [rbp-..], reg` stores between the monitor site's start (`[cratonvm-jitc] monitor-inline ... pc=..`) and the inline `LOCK CMPXCHG`: expected at most one per site.

### MON5-5 -- the "base varies in the loop" refusal is not counted: LIVE (HOST)

The refusal (`"the monitor's getfield base varies in the loop"`, `plan_loop_block_coarsening`) is printed under `CRATONVM_DBG_JITC` with its header node and nothing else counts it. The row asks for the count before any code; a per-VM counter would add JIT state for a census the log already answers. Recipe on the host battery (not Quarkus, where a global DBG_JITC hangs):

    CRATONVM_DBG_JITC=1 cratonvm ... 2>&1 | grep -c "the monitor's getfield base varies in the loop"

and, for the instance-block population it competes with, `grep -c "coarsened x4"` on the same log. A count of 0 on the battery retires the row.

### SDB17-3 -- shared sentinel exits are still refused: LIVE, MEASURABLE NOW

Verified live: `x64/deopt_stubs.rs` `self_lock_handler_exits_admitted` refuses, under `CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER` (default ON), a self-locking body with a protected shared sentinel exit or reason-3 throw, even one its local-handler site offers to the in-frame handlers first (such an exit releases instead of handing over). The proposal's value was "unknown until counted" and nothing counted it apart from the other handler-exit refusals. Now the body check records `self-lock-handler-shared-exits` for a body refused ONLY for such exits (`self_lock_handler_body_refused_only_for_shared_exits`; reporting only, admission unchanged), counted on the `[cratonvm] self-lock census:` line as `body-ref-handler-shared-exits` (`jit/src/self_lock_census.rs`). The per-exit rule is one pure function, `releasing_exit_admitted`.

Host: `CRATONVM_DBG=jit-method-stats` on the server battery / Spring Boot sample; read `body-ref-handler-shared-exits=` against `body-ref-handler-exits=`. A hand-over twin for the shared exit (the proposal's option 2) is worth building only if the first is a real share of `entry-compiles`.

### SDB17-4 -- the lookup maps LOCAL_HANDLER_DECLINED (-2) to -1: LIVE, PERF ONLY (not a parity defect)

Verified live: `vm/src/jit/helpers/typecheck.rs` `jit_local_handler_lookup_body` answers `-1` for a definitive miss and for every decline (an armed exception client, a user-loader catch type, a no-collect site's unloaded catch type, `LOCAL_HANDLER_DECLINED`), and the stubs (`x64/deopt_stubs.rs` `emit_local_handler_stubs`, `JS propagate`) branch on the sign only. Checked as a possible defect: a decline is resolved by the route it takes -- the reason-9 exceptional frame or the shared exit -- whose VM sink (`run_jit_callee_handler`, `route_jit_signal_exception`) re-runs the handler search loader-faithfully, resumes the handler in an interpreter frame and posts `Exception` / `ExceptionCatch` there (`execute_resumed_handler_frame` -> `report_exception_caught_by_compiled_door`). Under the self-lock hand-over the handed method monitor is adopted by that frame or released on a propagate. So the answer is right either way; what the proposal saves is one VM sink round trip per escaping exception of a self-locking body (`te` / `rc` shapes), at the cost of a second point per protected site and a helper ABI change. Not attempted without a build. Host recipe before any code: count escaping exceptions of self-locking bodies on the battery (`hold-frames-taken=` and `door-drain-handovers=` on the self-lock census line, `CRATONVM_DBG=jit-method-stats`); only a non-trivial count justifies the two-point sites.

### SDB17-5 -- no JVMTI exception-event arm on `compiled_catch_events_armed`: NOT A DEFECT BY CODE READING; COVERAGE ADDED

Checked as a possible HotSpot-parity defect (a JVMTI `Exception` / `ExceptionCatch` missed by a compiled catch). Every compiled catch route is gated:

* the compiled local-handler lookup declines while `compiled_catch_events_armed` holds (`typecheck.rs` `jit_local_handler_lookup_body`, before both arms, so the implicit arm too), and the declined throw reaches a door that posts both events (`report_exception_caught_by_compiled_door`, unit tests `interpreter.rs` ~34000);
* a spliced site that must answer marks the catch and the chain push posts it (`note_spliced_catch_needs_report`, `report_spliced_catch_if_pending`; `R24CfCatchLateArm`);
* the optimizing tier builds no compiled local throw edge when a client is armed at compile time or the VM serves the events from compiled code (`jit_bridge.rs` `exception_events_armed_at_compile`), and a listener armed later withdraws such code (`local_throw_edges_need_the_interpreter`).

What was missing is the battery arm the row names: `J27MonitorRows.c` arms `Exception` + `ExceptionCatch` AFTER `tcb` (a `catch` inside `synchronized (LOCK)`) and `tcm` (a `static synchronized` method catching a callee's throw, the R17SyncDoor `tc` shape) are compiled. Expected HotSpot: `jvmti-exception-events 2000`, `jvmti-catch-events 2000`, `jvmti-catch-in-tcb 1000`, `jvmti-catch-in-tcm 1000`. A short count on CratonVM is a parity defect: file it with the failing method.

### SDR17-1 -- CalleeHandlerMiss has no Unresolved variant: FIXED IN CODE

`vm/src/runtime/interpreter/exception_dispatch.rs` `CalleeHandlerMiss::Unresolved` is new; `vm/src/jit/helpers/thread.rs` `try_run_callee_handler` answers it when `resolve_callee_cached` finds no template (was `Declined`). Every caller treats it as before (they match `NotCaught` and fall through otherwise); the call-site service's replay refusal (`refuse_callee_rerun`, its `CRATONVM_DBG_DEOPT` line and its `InternalError` text) now says "after an exception whose callee handler template did not resolve" instead of "after an exception its handler could not resume" for that case (`callee_rerun_refusal_what`). The `InternalError` is CratonVM-only (HotSpot never raises it), so no parity line moves. Diagnosis only; no switch.

### SY16-3 -- no caller-held pairs recorded on the graph: FIXED IN CODE

The builder now records the `MonitorEnter` / `MonitorExit` of every caller-held synchronized direct CALL (`jit/src/ir.rs`, both the `0xb7` and the `0xb6 | 0xb8` arms, via `Graph::note_sync_call_pair` in `ir_optimize/monitor_rows.rs`) in the new `Graph::sync_call_monitor_ops`. `coarsenable_monitor_pair` merges an exit with an enter only when both are such halves or neither is (`sync_call_halves_agree`), as it already did for synchronized splice windows. Before, a bytecode block's exit could merge with a caller-held CALL's enter; `ir_lower`'s `sync_direct_call_monitor` then found no enter beneath the call and refused the whole compile (r16 SB1). Two adjacent caller-held pairs still merge (the lowering's chain walk serves them). Refusing a merge only keeps the builder's graph, so no switch.

## Unit tests (all new)

* `cratonvm-jit`: `ir_optimize::monitor_rows::tests::{only_deopt_only_array_ops_are_candidates, the_array_switch_is_opt_in, switched_off_an_array_block_is_refused_as_before, switched_on_the_element_accesses_leave_with_a_frame_of_their_own, switched_on_each_copy_accesses_the_element_once_under_one_hold, the_builder_record_names_both_halves_of_the_call_pair, unrecorded_the_block_exit_merges_with_the_call_enter, recorded_a_block_half_never_merges_with_a_call_half, two_call_pairs_still_merge_with_each_other}`
* `cratonvm-jit`: `x64::deopt_stubs::j27_sdb17_3_releasing_exit_tests::an_offered_protected_exit_is_refused_only_under_the_hand_over`; `self_lock_census::tests::the_driver_bail_sites_map_to_their_rows` (extended)
* `cratonvm-vm`: `jit::helpers::j27_sdr17_callee_rerun_refusal_tests::an_unresolved_template_is_named_apart_from_a_frame_decline`

## Verify recipe (host)

    cargo test -j 5 -p cratonvm-jit --lib monitor_rows
    cargo test -j 5 -p cratonvm-jit --lib j27_sdb17_3
    cargo test -j 5 -p cratonvm-jit --lib self_lock_census
    cargo test -j 5 -p cratonvm-jit --lib r11w9_mid_tests
    cargo test -j 5 -p cratonvm-jit --lib r16w16_monitor4
    cargo test -j 5 -p cratonvm-jit --lib r17_monitor5
    cargo test -j 5 -p cratonvm-jit --lib r19_syncdoor3
    cargo test -j 5 -p cratonvm-vm --lib j27_sdr17
    javac -d /tmp/j27mon tools/probes/jit/j27/J27MonitorRows.java
    java -cp /tmp/j27mon J27MonitorRows > /tmp/j27mon.hs
    for m in "" --compatible; do cratonvm $m -cp /tmp/j27mon J27MonitorRows | diff /tmp/j27mon.hs -; done
    CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS=1 cratonvm -cp /tmp/j27mon J27MonitorRows | diff /tmp/j27mon.hs -
    CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE=1 cratonvm -cp /tmp/j27mon J27MonitorRows | diff /tmp/j27mon.hs -
    CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE=1 CRATONVM_DBG_JITC=1 cratonvm -cp /tmp/j27mon J27MonitorRows 2>&1 >/dev/null | grep "loop lock coarsening.*field monitor"
    gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" -o /tmp/libj27mon.so tools/probes/jit/j27/J27MonitorRows.c
    java -agentpath:/tmp/libj27mon.so -cp /tmp/j27mon J27MonitorRows /tmp/libj27mon.so > /tmp/j27mon-ti.hs
    cratonvm -agentpath:/tmp/libj27mon.so -cp /tmp/j27mon J27MonitorRows /tmp/libj27mon.so | diff /tmp/j27mon-ti.hs -

Every diff must be empty. The `fact` lines in the probe header are computed off-line: take the HotSpot run as the reference, and correct the header if the two NPE message lines differ from it.
