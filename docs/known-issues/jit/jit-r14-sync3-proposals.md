# JIT round 14 wave 3, lane sync -- proposals

Follow-ups to the synchronized splice after wave 3 (SS-2 static mirror without the row, SS-3 splice
inside a held region, SS-4 no mixed coarsening, SS-5 no store fence for admitted bodies; see
`jit-r14-syncsplice-proposals.md`). Ranked by expected benefit over cost. Nothing here was built or
measured by the lane.

## SY3-1. Measure the wave-3 population before anything else

**What.** `CRATONVM_DBG_JITC=1` on `R14SyncSpliceStatic`, `R14SyncspliceLeaf` and `bench14.sh`
`SyncM`, default arm against `CRATONVM_JIT_IR_SYNC_SPLICE_STATIC_MIRROR=0`,
`CRATONVM_JIT_IR_SYNC_SPLICE_IN_REGION=0` and `CRATONVM_JIT_IR_SYNC_SPLICE_STORE_FENCE_SKIP=0`:
count `ir-splice-refused ir-sync-splice-*` by rule and any `ir-sync-splice-window REFUSED`.
**Benefit.** Says whether `staticStep` is spliced now that its body self-locks (it could not be in
wave 2 with the static flip on), and whether any SS-3 site reaches the lowerer's refusal (each one a
defect). **Cost.** Four runs. **Risk.** None. **First step.** The arms above, interleaved, 5 reps.

## SY3-2. Elide a synchronized splice's pair inside a region on the same object

**What.** In `synchronized (o) { o.leaf(); }` the splice's enter/exit on `o` is a recursive level
the region already holds (JMM: no other thread can acquire `o` inside the region; `wait`/`notify`
and `holdsLock` answer the same at depth 1). `elide_nested_monitors` now skips window ops entirely
(SS-3's safety patch). A window is frameless, so deleting BOTH halves needs no snapshot marks: add a
second arm that deletes a window pair when every snapshot at the invoke pc holds the window's
monitor NODE (strip trivial φs) and both halves are spliceable (`nested_monitor_op_spliceable`), and
drop the window from `sync_splice_windows` (or mark it elided so `ir_lower` skips it). **Benefit.**
Two inline CASes (the thin-lock recursion arm) per call in `Hashtable`/`Vector`-style code that
locks around its own calls. **Cost.** Small. **Risk.** Low-medium: both halves or neither, and the
lowerer's pairing must not see a half-deleted window. **First step.** The arm plus a unit test on
the `region_around_a_splice` fixture of `r14w3_sync_monitor_pass_tests`.

## Round 14 wave 4 (lane sync4): SY3-2 landed

Pending build. `ir_optimize.rs` `elide_region_nested_sync_windows`, run by `optimize` after
`elide_nested_monitors` under `CRATONVM_JIT_IR_NESTED_LOCK_ELIM` and its own
`CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_ELIM` (default ON): a window whose two halves are live, name
one object at one bci, and splice out of the memory chain (`nested_monitor_op_spliceable`) is
deleted WHOLE when every snapshot at that bci (the caller's `invoke`, whose monitor stack is the
caller's) names its object through trivial φs, and at least one exists; the window is then
dropped from `Graph::sync_splice_windows`, so `ir_lower` never sees a half. No snapshot marks (a
window is on no frame state; the region's own entry is the hold that remains). Tests
`ir_optimize::r14w4_sync4_window_elision_tests`; probes
`C:\craton\jitr14-probes\src\R14Sync4HoldsLock.java` (`region`), `R14Sync4RegionDeopt.java`.

## SY3-3. Constant-divisor division inside a synchronized splice

**What.** `ir_sync_splice_body_scan` refuses every `idiv`/`irem`/`ldiv`/`lrem` and the window rule
refuses `Op::Div`/`Op::Rem`, because a zero divisor traps. A divisor that is an `iconst_*` /
`bipush` / `sipush` / `ldc` integer constant other than 0 cannot trap (`MIN_VALUE / -1` wraps in
Java and must not trap in the lowering either -- check `ir_lower`'s division arm first). Admit
exactly that shape in both predicates (the scan tracks "constant non-zero" on the slot it pushes;
the graph rule admits an `Op::Div`/`Op::Rem` whose divisor node is `Op::Const(k != 0)`).
**Benefit.** Counters and hash buckets (`(h & 0x7fffffff) % 16` is `irem` by constant).
**Cost.** Small. **Risk.** Low, provided the lowering of a constant divisor has no trap path.
**First step.** Read the `Op::Div`/`Op::Rem` lowering for a `Const` divisor, then the two predicates.

## SY3-4. One lookup pass for the caller-held row and the splice's mirror

**What.** Since SS-2 a refused static site runs `sync_direct_monitor` twice (inside
`sync_direct_target`, then again in `sync_direct_monitor_answer`), and the splice planner may ask the
lookup a second time for a site the caller-held arm already asked (the direct bind to a
self-locking body skips that arm, so usually it does not). Return an enum from one pass
(`Bound(target, pin) | MonitorOnly(target)`) and memo the answer per `cp_idx` for the compile.
**Benefit.** Compile time only (two class-manager reads per refused static synchronized site).
**Cost.** Small. **Risk.** Low (the text pins on `sync_direct_target` must keep their strings).
**First step.** The enum in `jit_bridge.rs`, both closures matching on it.

## SY3-5. A splice census line and counter

**What.** No counter says how many synchronized splices a run built, of which kind (instance,
static via row, static via mirror, inside a region). Add one `ir_evidence`-style tally at
`end_splice`'s window push and print it with the existing JIT census at exit. **Benefit.** SY3-1
without `DBG_JITC` parsing, and a regression signal for the four switches. **Cost.** Small (must
have a production reader: the exit census). **Risk.** None. **First step.** The tally in `ir.rs`
next to `ir_evidence::note(Inlined)`.

## Round 14 wave 4 (lane sync4): SY3-5 landed

Pending build. `ir.rs` `IR_SYNC_SPLICE_CENSUS` (one static array, rows `instance`, `static-row`,
`static-mirror`, `in-region`, `holdslock-folded-in-splice`, `holdslock-folded-in-region`,
`window-elided-in-region`), counted at `end_splice`'s window push
(`IrBuilder::note_sync_splice_built`), at each SS-7 fold and at each SY3-2 elision. Readers: a
per-event `[cratonvm-jitc] ir-sync-splice built <kind> at pc=<n>[ in-region]` line under
`CRATONVM_DBG_JITC`, and `ir_sync_splice_census_line()` for the exit summary, whose
`interp_census.rs` call is the exact patch
`r14w4-sync4-interp-census-sync-splice-line-patch-FIXED-20260929.md`. No kill switch (a counter).
`jit/tests/process_global_statics_ratchet.rs` moves +1 (the census array).
