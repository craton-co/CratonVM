# The w4b "kill arms beat the default" rows: the predicates cannot fail there; the split is per-process

> **j27 wave 8 (lane W8LOOP, 2026-10-28): PARTLY (race B's evidence line), PENDING HOST TRACE.**
> **The gap.** Race B's census (item 3 of the wave-5 recipe below) could not be read. The `[ir] never-taken feed`
> line prints only when a prune is OFFERED. So exactly the thin-row compiles race B is about printed nothing.
> **The change.** Every compile that has branch rows now prints this line, under the same channels
> (`CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_IR_BAILOUT`):
> `[ir] never-taken rows in <method>: rows=R full=F (floor 2000) max_total=M offered=K`
> Code: new module `jit/src/never_taken_row_census.rs`, called at the end of `lib.rs` `ir_never_taken_branches`.
> It is debug output only, and no decision reads it.
> **Tests.** `cratonvm-jit --lib never_taken_row_census::tests`, three tests: `j27_w8_row_census_*` and
> `j27_w8_census_line_names_the_fill_and_the_offer`.
> **Recipe.** Run the 16-process `Nio21DecodeLoop mixed` census below, and add
> `grep 'never-taken rows in .*decodeArrayLoop'` per C2 compile.
> * If the slow and fast processes split on `full` (`full=0` / `offered=0` in one mode, `full>0` in the other), race
>   B is the cause. The fix is then a minimum row fill before a C2 compile reads the rows, or C1 branch profiling.
> * If `full` is the same in both modes, race B is excluded, and race A's counts decide.

> **j27 wave 5 (lane TRAPPROF, 2026-10-28): NOT FIXED, PENDING HOST TRACE. Two races of the seglongloops shape found by reading. One has an opt-in fix.**
> Each race below makes a once-only decision from a profile that is not yet filled.
> - **Race A (fixed opt-in): trap, eager C2 re-queue, re-prune.**
>   - After a pruned branch traps, `deoptimize_in` re-queues C2 (`CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2`, ON since j27). The trap's de-spec is recorded later, by the resume sink, and the trapped edge later still, by the resumed interpreter.
>   - An idle worker can therefore re-prune the same branch. Four such charges at one `(UncommonTrap, bci)` set `c2_bailout` for the rest of the process. That is a permanent C1 state, which fits a ~2.5x slow mode for a kernel that should be C2.
>   - Opt-in fix: `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT=1`. See `j27-trap-recompile-reads-the-profile-before-the-resume-records-FIXED-20261029.md`, and `j27-per-bci-trap-limit-bans-c2-for-the-life-of-the-method-FIXED-20261029.md` for the permanence.
> - **Race B (not fixed): the branch rows a C2 compile reads are filled only during a timing-dependent stretch.**
>   - The stretch runs from the method's first `Interpreter -> C1` nomination (`BranchCounters::arm`) until it leaves the interpreter. Branch recording is interpreter-only (`jit/src/profile.rs`).
>   - A kernel that a fast C1 worker or an early OSR body takes out of the interpreter reaches C2 with rows below `ir::MIN_OBSERVATIONS_TO_PRUNE` (2 000). Such a kernel gets no prunes. A slower process gets full rows and its prunes, and those can then trap (race A).
>   - Not fixed: which mode is the fast one is not provable by reading.
> - **Exact host recipe** (`Nio21DecodeLoop mixed`, 16 separate processes per arm, `--jdk-only`, `JAVA_HOME`=jdk25):
>   - Arms: the default, `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT=1`, and `CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2=0`.
>   - Environment for every arm: `CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1 CRATONVM_DBG_IR_COMPILES=1 CRATONVM_DBG_TIER_ENQUEUE=1`.
>   - Per process, record the row's ns/op and these counts for `decodeArrayLoop`:
>     1. `grep -c 'reason=\(UncommonTrap\|UnstableIf\) bci=' ` on the `[cratonvm-deopt] ...decodeArrayLoop` lines, split by bci.
>     2. `grep -c 'retry REFUSED .*decodeArrayLoop.*c2_bailout'`.
>     3. The `[ir] never-taken feed in ...decodeArrayLoop: N profiled branch(es), offered [...]` lines. N per compile is race B's evidence.
>     4. `never-taken branch PRUNED at bytecode pc` and `... WITHDRAWN (de-spec)` lines.
>     5. The `enqueue .. tier=C2 invocations=` line.
>   - **How to read it:**
>     - If every slow process has `c2_bailout`, or 2-4 deopts at one bci, and the `=1` arm is unimodal fast, race A is the cause. Then flip `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT` and fix the permanence.
>     - If instead the slow and fast processes split on the size of N, or on prunes being offered, race B is the cause. Then the fix is a minimum row count before a C2 compile reads the rows, which HotSpot's MDO gets for free by profiling in C1 too.

> **j27 wave 1 (lane TIER, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION.** No deterministic cause can
> be proven from code alone; two per-process timing sources in this lane's area were found, and both now
> have a lever. (1) Mechanism 1 above (a refused C2 publication charged to the budget) already has an
> opt-in refund, `CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND` (r23 w9 L6; `tiered.rs`
> `refund_refused_publication`, up to 6 per method). (2) WHEN a C1 body's C2 request is queued is decided by
> the drain clocks (interpreter stride, background compile starts, a 200 ms idle tick, OSR entries at most
> every 50 ms), i.e. by wall time and by which other compiles happen to run, not by the count -- a
> per-process compile ORDER source for exactly the kernels this page lists (all reached from compiled
> callers). `CRATONVM_JIT_ENTRY_TRIP=1` (this wave, page
> `jr21-conf-compiled-bodies-reach-c2-only-at-a-drain-clock-FIXED-20261029.md`) queues it at the bar instead.
> What the host must run: the page's 16-process `Nio21DecodeLoop mixed` census under
> `CRATONVM_DBG_IR_COMPILES=1 CRATONVM_DBG_TIER_ENQUEUE=1`, default vs `CRATONVM_JIT_ENTRY_TRIP=1` vs
> `+ CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND=1`; if an arm is unimodal, the fast/slow split was the C2
> request clock (resp. the budget) and that switch is the fix; tabulate the `enqueue .. tier=C2
> invocations=` of `decodeArrayLoop` per process against its mode.

Status: OPEN (round-21 host check, 2026-10-15) -- host census done (section "Host census"): no predicate switch causes the bimodality; it follows `CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS` (a per-process compile-order outcome), and every predicate switch stays ON. Missing: the 16-process `CRATONVM_DBG_IR_COMPILES=1` compile-sequence diff, in no round-21 log. Previous status: OPEN
Area: `jit/src/ir_optimize/{monotone_predicate,class_predicate}.rs`, `jit/src/ir_optimize.rs`
(`plant_length_predicates`, `install_range_predicates`), `jit/src/ir_lower.rs` (`emit_osr_entry_stubs`,
`op_guard_is_range_predicate`, `osr_recheck_nodes`), `jit/src/tiered.rs` (`request_c2_retry`),
`vm/src/runtime/interpreter/jit_bridge.rs` (`note_refused_optimizing_publication`)
Found by: JIT round 21 lane strings (handoff carried item 3; books STR20-6, LOOPOPT20-7, SPLICE20-4)

## Question

w4b (5 interleaved reps, loaded host) measured kill arms faster than the default:
`CRATONVM_JIT_IR_MONOTONE_PREDICATE=0` (`Nio21DecodeLoop` asciitail / mixed -34 / -31 %),
`CRATONVM_JIT_IR_OFFSET_PREDICATE=0` (`Nio21EncodeLoop` buffer-ascii -29 %, buffer-mixed -36 %),
`CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE=0` (buffer-ascii -22 %), `CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0` under
`--compatible` (cyrillic 28332 -> 18669), and on `Nio20BufferTyped`'s direct rows
`CRATONVM_JIT_IR_BUFFER_TRAP_MISS=0` / `CRATONVM_JIT_IR_LENGTH_PREDICATE=0` /
`CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS=0` (about half of 234 / 233 / 251 ns). The brief asks WHY,
suspecting (a) predicate failure -> deopt -> recompile without, or (b) a guard in the hot loop.

## What reading excludes

* **(b) a per-iteration guard.** Every one of the four predicate families is planted in the loop
  PRE-HEADER and runs once per loop entry (`plant_monotone_predicates`, `plant_class_predicates`,
  `plant_length_predicates`, the offset predicate), and `ir_check_elim` only removes in-body checks. Nothing
  these switches add executes per byte; the per-byte body can only get smaller.
* **(a) for the decode rows.** `UTF_8$Decoder.decodeArrayLoop` (JDK 25, `jdk25src`) computes
  `sl = soff + src.limit()`, `dl = doff + dst.limit()` from heap buffers, so `sl <= sa.length` and
  `dl <= da.length` always hold; `sp`, `dp` start at `arrayOffset + position >= 0`; the only `x - p < k` test
  with an invariant `x` other than the header bound is `dl - dp < k`, and `dl >= 0`. Every monotone guard
  therefore PASSES on every entry for every input `Nio21DecodeLoop` makes; no deopt, no de-spec, no
  recompile can come from them. The room-offset arm (`CRATONVM_JIT_IR_BCE_ROOM_OFFSET`) plants no guard at
  all (a proof rule in `ir_check_elim`), so it cannot deopt either.
* **(a) for the encode buffer rows.** `encodeBufferLoop`'s receivers are one `StringCharBuffer` and one
  `HeapByteBuffer` per row (`CharBuffer.wrap(String)`, heap destination); the JDK's own stdout path takes
  `encodeArrayLoop` (both buffers have arrays), so the class predicate's single-class speculation does not
  fail in that probe either.
* **(a) for `Nio20BufferTyped`.** Each row has its OWN kernel copy (`getIntsDirect`, `putIntsDirect`,
  `longsDirect`, ...; `tools/probes/Nio20BufferTyped.java` lines 112-119), so a direct kernel's receiver
  profile names `DirectByteBuffer` alone and its trap-form splice never misses on that row.

So none of the six switches buys its loss through a failing speculation or a per-byte cost on these rows.
This agrees with lane xUtf8's wave-18 reading of the m7 pass (an older tree with the same guards): the
DEFAULT measured asciitail 26 999 / mixed 28 665 and buffer-ascii 42 970 there, the same as the w4b kill arms;
w4b's default sat ~50 % above. Every arm's value equals the default's FAST mode.

## What remains: a per-process choice of body

The rows are bimodal per PROCESS. The mechanisms by which one process can keep a slower body for the loop
method, ranked by how directly the code shows them:

1. **A refused C2 publication.** `JitCache::put` drops an optimizing body whose baked direct-call target was
   retired before publication (`[jit-retired-callee]`), and the by-name door then serves the C1 body
   (`note_refused_optimizing_publication`, `jit_bridge.rs`). `tiered.rs` re-asks C2
   (`request_c2_retry(.., "refused-publication")`), charged to the tier-failure budget; a method whose callees
   keep superseding during its compile window can exhaust it and stay C1 for the process. lane splice's
   round-20 census already saw "a refused `c2-publication` of `decodeArrayLoop`" (SPLICE20-5). The predicate
   switches change the C2 compile's size and duration, i.e. the odds of that race -- which would make an arm
   LOOK faster on a 5-rep median without the arm costing anything itself.
2. **A refused OSR entry.** `emit_osr_entry_stubs` refuses an entry past a header guard the stub cannot
   re-check (`CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`; `op_guard_is_range_predicate` admits only
   `Cmp(Le|Lt)[x, ArrayLength]`, so the monotone `0 <= v` and the class guards always refuse it). That costs
   only an activation already inside the loop when the body is published (one long decode at warm-up), not the
   steady state, unless the steady-state caller keeps entering through OSR.
3. **A lowering-ladder retry** (`ir-deopt-image dropped and the attempt was refused`) producing a heavier body.

## What decides it (orchestrator, host)

`Nio21DecodeLoop rows asciitail,mixed`, `Nio21EncodeLoop rows buffer-ascii`, `Nio20BufferTyped rows
directget,directput,directlong`: 8+ separate PROCESSES per arm (default and each kill arm), per-rep values,
each with `CRATONVM_DBG_JITC=1 CRATONVM_DBG_IR_COMPILES=1`, tabulating slow / fast against

* `grep -c 'c2-publication REFUSED .*decodeArrayLoop'` (resp. `encodeBufferLoop`, the row's kernel) and
  `[jit-retired-callee]` lines naming it;
* `[ir] acceptance .. decodeArrayLoop` accepted / REFUSED;
* `osr optimizing REUSE .. pc=<header>` counts and the OSR refusal reasons at that header;
* `ir-deopt-image dropped and the attempt was refused` with the method's node count.

If the slow mode coincides with (1) in every arm, the fix is in the publication path (re-offer a refused body
after re-resolving its baked targets instead of charging the budget), not in the predicates, and no
predicate switch should be flipped. If it coincides with (2), extend `op_guard_is_range_predicate` /
`osr_recheck_nodes` to the `Cmp(Le)[Const 0, v]` and `ExactClassIs` guards (the re-check program
`osr_check_compile` already compiles compares and constants; a value it cannot rebuild keeps today's refusal,
so the extension is sound by construction). Neither was built in round 21 because neither is proven, and the
kill arms are not shown to win anything but the race.

## Host census (orchestrator, w1g binary, 8 processes per arm, loaded host)

`pr.sh w1g` / `prsum.sh w1g` (raw per-process values, ns/op):

* `Nio21DecodeLoop mixed` is bimodal (~40k / ~100k) in the default arm AND in every predicate kill arm
  (MONOTONE, OFFSET, LOOP_CLASS, BUFFER_TRAP_MISS, LENGTH all show both modes). No single predicate switch is the
  cause, so the w4b "kill arm wins" were which mode the 5 reps happened to land in, not the switch.
* Only `CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS=0` is unimodal (mixed 55-73k), but it pays for it on
  asciitail (59-80k vs 44-55k default). The mode split therefore follows the back-edge virtual-object exit shape
  (a per-process compile-order outcome), not a predicate. Keep every predicate switch ON.
* `Nio21EncodeLoop buffer-ascii` and `Nio20BufferTyped direct*`: arm spreads overlap (load noise, 2-3x within one
  arm); no arm separates.

Next step: run `Nio21DecodeLoop mixed` 16x under `CRATONVM_DBG_IR_COMPILES=1` and diff the compile sequence of a
40k process against a 100k process (which kernel publishes first, and whether the back-edge exit materialises).
