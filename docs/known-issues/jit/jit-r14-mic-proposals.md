# JIT round 14 wave 2, lane mic: proposals (inline-cache grace, miss-handler quiescence)

Status: PROPOSALS (ideas for triage, not work items)
Area: JIT inline caches, retirement grace (`jit/src/lib.rs` quiescence), the VM's helper doors
Found by: round 14 wave 2 lane mic

Ranked by expected benefit over cost. Each names its first concrete step.

## MIC-1. Stamp the inline-cache quiescence at the one helper-entry door every helper passes

**Benefit.** M8-1 (landed this wave) stamps `ic_quiescent_gen` only at `jit_invoke_virtual_mic` and
`jit_invoke_dispatch`. A compiled loop that never misses a call but allocates through the TLAB-refill
helper, reads a VarHandle through its funnel, or takes a checkcast/instanceof/array-store helper is
equally outside every probe when it enters those helpers, and today still pins the grace. Every such
helper already opens with `jit_safepoint_flush_satb(vm_ptr)` (`vm/src/jit/helpers.rs`; the
`helper_entry_doors` text pin lists `jit_newarray_body`, `jit_new_object_body`,
`jit_anewarray_object_body`, `jit_multianewarray_n_body` and the two dispatch bodies). Stamping there
covers them all with one line.
**Cost.** One call in `jit_safepoint_flush_satb` (the stamp is a TLS read, a load and a store), or in
a new wrapper the door calls.
**Risk.** Medium. The argument needs "no caller of the door is between a compare and a load"; the
door is also called from Rust code paths that are not compiled-code entries (grep its callers: a Rust
helper that calls it after a `lookup` and before using the result is fine, one between a class compare
and the entry load is not; none was found for the two dispatch bodies). The door is also the GC's
(`drain_native_return_at_compiled_helper_entry`): coordinate.
**First step.** `rg -n "jit_safepoint_flush_satb\(" vm/src` and classify each caller; then measure with
`R13Mega8ChurnWayReuse` with its spinner changed to allocate one small object per iteration.

## MIC-2. Count what the M8-1 catch-up lifts

**Benefit.** `mic_grace_lag_refused` now counts only refusals the per-thread catch-up did not lift, so
the census cannot say how much M8-1 did without an A/B run. A `mic_grace_catch_up_lifted` column in
the `[DISP_CENSUS]` line (`CRATONVM_DBG=mic-prof`) makes one run answer it, and tells the owner
whether the handshake default (`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`) still matters.
**Cost.** `JitPICSlot::install` would have to return a small enum (`Installed`, `RefusedGraceLag`,
`LiftedByCatchUp`) instead of a bool, and its three callers in `helpers.rs` note the new column (the
orphan-instruments script requires the reader, which the census line is).
**Risk.** Low (diagnostics only; the three callers are in the interpreter round's hot file).
**First step.** Change the return type and the three call sites together; keep `bool` semantics for
`RefusedGraceLag` so `request_ic_grace_if_due` is untouched.

## MIC-3. Make the inline-cache grace per VM

**Benefit.** `JIT_RETIRE_GENERATION` / `JIT_GRACED_GENERATION` are process statics, so one VM's
spinning thread holds the grace of every other VM in the process, and unit tests of grace behaviour
must hold an execution token to stay deterministic (`r13w10_mega8_dead_way_tests`,
`r13w11_mega9_grace_lag_refusal_tests`). The ways themselves are per VM (`MegaDispatchTable`) or per
compiled body; only the generation pair and the thread registry are global.
**Cost.** Large: the stamp would be read through the table or the slot's holder, and
`JitThreadQuiescence` would need a VM id; the retirement queue is global too.
**Risk.** Medium (the lowering of two process statics is a ratchet win, but every grace reader moves).
**First step.** List every `bump_retire_generation` / `retire_generation_is_graced` caller and whether
it can reach its VM's table; if all can, the pair moves onto `MegaDispatchTable`.

## MIC-4. Retry a refused install from the catch-up for the shared table too

**Benefit.** M8-1's retry covers `JitPICSlot::install` (inline and site-hashed ways). A refusal in
`MegaDispatchTable::install` / `install_with_class_slot` (a shared set or a class cell another
selector's retired column holds) has no retry: the receiver waits for the next miss after some pump
graces. With the stamp in place the same `jit_ic_grace_catch_up()` could run there.
**Cost.** Small: those installs must first say whether they refused for a lagging grace (they return
nothing today), then the same two-attempt wrapper.
**Risk.** Low (same proof, same lock order: the catch-up runs outside the table's writer lock).
**First step.** Add a `grace_lags` answer to `MegaDispatchTable::install_with_class_slot` and count it
beside `mic_grace_lag_refused` under `CRATONVM_DBG=mic-prof` on `R14MicSpinnerMisses`.
