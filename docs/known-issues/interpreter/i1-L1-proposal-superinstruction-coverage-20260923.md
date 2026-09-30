# Proposal: superinstructions that match what javac emits, chosen by a census

**Status: open, partly landed — filed 2026-09-23 by interpreter round i1, lane L1; stages landed by waves 2-5, lane L1 (see Progress); census landed by wave 4; the array-length loop test by wave 5; an offline pair census (`tools/interp-pair-census`) and the stacked-array element read `iload{_N, n}; <x>aload` by wave 25, lane L7.** Proposal.

## What exists

All fusion lives in the `iload_0..3` arm of `execute_frame_from_index`
(`vm/src/runtime/interpreter.rs`), decided by peeking at the following bytes
on every execution:

| fused sequence | bytes | notes |
|---|---|---|
| `iload_X; iload_Y; iadd` | 3 | `a + b` |
| `iload_X; iload_Y; if_icmplt` | 5 | with PGO/back-edge/OSR handling |
| `iload_X; iconst_1; iadd; istore_X` | 4 | `x = x + 1` spelled without `iinc` |

This round (lane L1) removed a fourth, `iload_X; arraylength`, that could not
fire on verified code (an `int` is never an arrayref), switched the
`if_icmplt` fusion to raw-slot int reads, and made all fusion decline while a
JVMTI single-step listener is armed (fused tails used to lose their SingleStep
events).

Correctness of the existing fusions checks out: none of the fused bytecodes can
throw, the branch pc used for profiling and for the target is the `if_icmplt`'s
own (`saved_pc + 2`), and a branch landing in the middle of a fused group simply
dispatches that instruction unfused.

## The coverage problem

* **javac does not emit the fused loop shape.** `for (int i = 0; i < n; i++)`
  compiles to a *top-tested* loop: `iload i; iload n; if_icmpge exit; …;
  iinc i 1; goto top`. The fused `if_icmplt` form is the *bottom-tested* shape
  ECJ, Kotlin and Scala emit. The javac loop pays three dispatches for the test
  and two (`iinc`, `goto`) for the step.
* **Only `iload_0..3` operands fuse.** `iload 4+`, `bipush`/`iconst` right-hand
  sides (`i < 10`), and the `aload` family never do.
* **The most frequent pair in object-oriented bytecode is not fused at all:**
  `aload_0; getfield` (every field read in an instance method), followed by
  `aload_N; getfield`, `aload_N; arraylength`, `aload_N; iload_M; iaload`.
* **Runtime peeking costs every unfused `iload_N`** a byte-range compare or two.

## Proposed design

1. **Census first (instrument).** Add `CRATONVM_DBG=opcode-pairs`: a per-thread
   `Box<[[u32; 256]; 256]>` incremented on the fast path behind a hoisted bool
   (zero cost unarmed, like the other gates in the preamble), dumped as the top
   50 pairs and triples at exit. Run it on the Spring Boot, Tomcat, ES and
   CratonBench suites. Choose fusions by dynamic frequency, not by reading.
2. **Top-tested compare-and-branch, all six conditions:**
   `iload{_N, n}; {iload{_N, n} | iconst_* | bipush}; if_icmp{lt,ge,gt,le,eq,ne}`.
   Reuse `cond_branch_arm!` (it already owns PGO, back-edge, OSR and the poll)
   by giving it the fused instruction's own pc, as the `if_icmplt` fusion does
   by hand today — which would also delete that fusion's 60-line copy of the
   back-edge block.
3. **`iinc; goto` (the javac loop step)** — one dispatch for the back edge.
4. **`aload_N; getfield`** routed through `field_fast::getfield_fast` with the
   receiver taken straight from the local (no push/pop); on a miss, fall back
   to plain `aload_N` so the full handler sees the normal stack. Exceptions:
   the NPE must report the `getfield` pc (`saved_pc + 1`), not the `aload` —
   pass it explicitly into `pending_runtime_error`.
5. **Move the decision to quickening time** once there are more than a handful
   of fusions: `QuickenedCode` (`reader/src/quickened.rs`) already walks each
   method once; it can emit a per-pc "fusion id" byte array, so the fast path
   reads one byte instead of peeking at up to four. Branch targets are known
   at that point, so a fusion whose *interior* is a branch target or a handler
   start can be refused outright (today it is harmless only because a branch
   into the interior dispatches unfused — a quickening-time rewrite must keep
   that property).

## Rules every new fusion must keep

* Skip it while `single_step_active` (JVMTI SingleStep per bytecode).
* Any throw reports the pc of the bytecode that throws, not the group's start.
* PGO branch records and back-edge accounting use the branch's own pc.
* A branch into the middle of the group must still execute correctly.

## How to verify

* The `l1_dispatch_tests` module in `interpreter.rs` (added this round) runs a
  fused counted loop and a fused increment end to end; extend it with each new
  fusion's unfused-equivalent answer, a mid-group branch target, and (for
  `aload; getfield`) a null receiver.
* `difftest`'s `interp-decoded` axis (the decoded path never fuses) must stay
  green.
* `tools/probes/Dispatch.java` / `BackEdge.java` with a javac-compiled loop,
  interleaved arms, kill switch on vs off.

## Expected benefit and risk

Each fused pair removes one full iteration of the ~7.5 ns preamble (see
`i1-L1-proposal-dispatch-loop-register-state-20260923.md`) from its hot sites;
on javac-shaped loops the compare-and-branch and `iinc; goto` fusions together
remove 3 of ~5 loop-overhead dispatches. Risk is per fusion and low if the
rules above are tested; the `aload; getfield` fusion carries the NPE-pc risk.

## Progress (wave 2)

Landed in `vm/src/runtime/interpreter.rs` (`execute_frame_from_index` and the
helpers after the `mod` declarations), all declined while
`single_step_active`, as the rules above require:

* **Top-tested compare-and-branch, all six conditions** (item 2). First
  operand `iload_0..3` or `iload n`; second operand `iload_0..3`, `iload n`,
  `iconst_m1..5`, `bipush` or `sipush` (`fused_int_operand`); then any
  `if_icmpeq..if_icmple` (`if_icmp_taken`). The new `fused_icmp_arm!` macro
  hands the branch to `cond_branch_arm!` at the BRANCH's own pc, so PGO,
  back-edge accounting, OSR and the poll are the unfused arm's; the old
  `if_icmplt`-only fusion and its 60-line copy of the back-edge block are gone.
  The fused path also sets `last_instr_pc` to the branch pc, so a back-edge
  safepoint sees exactly the state the unfused `if_icmp` would leave
  (liveness reads `[pc, last_instr_pc]`). A local that is not int-tagged
  declines, keeping the unfused answer for that slot.
* **`iinc; goto`** (item 3), through a new `goto_arm!` macro that the plain
  `goto` arm now shares (back edge accounted at the `goto`'s pc).
* **`aload_N; getfield`** (item 4), as the simpler variant: `aload_N` pushes,
  then `field_fast::getfield_fast` is tried on the following `getfield`. A
  miss (null receiver, cold site, field watchpoint) leaves the stack as
  `aload_N` left it and the `getfield` dispatches on its own pc, so the NPE pc
  and JVMTI FieldAccess are unchanged by construction.

Tests (`l1_dispatch_tests`): `compare_and_branch_fusions_match_every_condition`
(every condition x every operand shape, against the plain predicate),
`javac_shaped_counted_loops_fuse_and_keep_their_answer` (low and high locals,
`iinc; goto` back edge), `branch_into_a_fused_group_runs_it_unfused`. The
`aload_N; getfield` fusion has no unit test (it needs a populated field site
over a real class); `difftest`'s `interp-decoded` axis and the suites cover
it. Probe: `tools/probes/interp/L1/L1FusionBurn.java` prices all three.

Not done: the `opcode-pairs` census (item 1 — a new `CRATONVM_*` flag needs a
`types/src/flag_groups.rs` declaration and regenerated flag docs), the
`aload_N; arraylength` / `aload; iload; iaload` shapes, and moving the
decision to quickening time (item 5). Unfused `iload_N` now pays an int-tag
test and a few byte compares for the second-operand decode; measure with the
probe before adding more peeks to that arm.

A fifth rule for every fusion, found while landing these: breakpoints. The
dispatch loop does not deliver breakpoints at all today
(`docs/internal/fixed-bugs/interpreter-L1-breakpoints-are-never-checked-by-the-dispatch-loop-FIXED-20260925.md`;
it has since wave 3, and every fusion declines while one is armed);
whoever adds that check must make every fusion decline while a breakpoint is
armed, or a breakpoint on a group's interior pc is skipped.

## Progress (wave 3)

The fifth rule is now enforced: the dispatch loop delivers JDWP breakpoints
(see the breakpoints page's Progress (wave 3)), and every fusion site tests
one hoisted `fusion_off` (`single_step_active || breakpoints_armed`) instead
of `single_step_active` alone. A new fusion must test `fusion_off`.

## Progress (wave 4)

* **Census (item 1) landed**, as `CRATONVM_QUICKEN_STATS=pairs` (a value of
  the existing flag, not a new `CRATONVM_DBG` token): fall-through pairs that
  still dispatch separately, top 60 at exit. See the dispatch-loop page's
  Progress (wave 4) for the mechanics; it costs nothing unarmed.
* **`aload_N; arraylength`** (javac's `a.length`): the `aload_0..3` arm tries
  the `arraylength` arm's own `field_fast::arraylength_fast` on the pushed
  reference. A null / non-array / corrupt-header receiver declines and leaves
  the stack exactly as `aload_N` did, so the NPE comes from the `arraylength`
  at its own pc. Same `fast_field_zgc` admission as the unfused arm.
* **`iload_X; iload_Y; iadd; istore_Z`** (`s += i`, `c = a + b` over locals
  0-3): once the existing `iload; iload; iadd` fusion has matched, a following
  `istore_0..3` stores the sum with the `istore` arm's own
  `set_local_compact_unchecked` instead of pushing it. Only groups that
  already matched pay the extra byte test.

Both decline under `fusion_off` and contain no throwing bytecode (the
`arraylength` is executed only on the non-throwing hit). Test:
`l1_dispatch_tests::wave4_fusions_keep_the_unfused_answers` (sum into another
local and into an operand local, the plain pushed sum, the array length, and
`arraylength` of null still failing).

Not done, and the census should decide before any of them is built:
`iload n` / `istore n` operands for the `iadd` fusion; the loop test
`iload i; aload a; arraylength; if_icmp<cond>` as a second-operand shape of
`fused_icmp_arm!` (needs an `arraylength_fast` variant that reads a local
without pushing — belongs in `field_fast.rs`); `aload_N; iload_M; iaload`;
moving the decision to quickening time (item 5).

## Progress (wave 5)

Measured by the orchestrator, 2026-09-24, `--compatible --nojit`, three
interleaved rounds (wave-3 / wave-4 / wave-5 release binaries), ns/iter,
medians: `L1Wave4Fusions` arrayLoop 63.6 / 72.2 / 60.8, sumLocals
19.8 / 20.7 / 19.4; `L1Wave5ArrayLengthTest` low 63.6 / 72.1 / 62.0, high
62.5 / 75.3 / 63.4. The wave-4 `aload_N; arraylength` fusion was ~13%
slower than no fusion; wave 5's local-reading fusion and fused loop test are
back at or below wave 3.

* **The loop test `iload i; aload a; arraylength; if_icmp<cond>`** (javac's
  `i < a.length`) is a second-operand shape of `fused_icmp_arm!`:
  `fused_int_operand` accepts `aload_0..3; arraylength` and
  `aload n; arraylength` (index under `max_locals`), for every `if_icmp`
  condition and behind both `iload_N` and `iload n` first operands. The
  length comes from the new `field_fast::arraylength_of_local` (an edit in
  lane L4's `field_fast.rs`): the header read `arraylength_fast` does, on the
  reference still in the local, nothing pushed or popped. A null, non-array
  or non-`Object` local declines, so the unfused `arraylength` raises its NPE
  at its own pc and no bytecode of a fused group throws. The javac array loop
  (`L1Wave4Fusions.arrayLoop`) now runs its whole test in one dispatch
  instead of three.
* **The wave-4 `aload_N; arraylength` pair reads the local too**, and
  `aload n; arraylength` joins it. The wave-4 form pushed the reference and
  then ran `arraylength_fast` over the stack, which re-read its kill switch,
  re-checked the depth and popped what it had just seen pushed.
* **Admission is hoisted**: `arraylength_fusion_on = fast_field_zgc.is_some()
  && !no_arraylength_fast()`, once per `execute_frame_from_index` entry, so
  every arraylength fusion tests one bool (the unfused arm still re-reads the
  switch per call, unchanged).
* Measured regression this answers: the orchestrator's interleaved
  `L1Wave4Fusions` runs had `arrayLoop` ~12% SLOWER on the wave-4 build
  (61-62 -> 68-71 ns/iter) with `sumLocals` flat. Two wave-4 changes sit on
  that loop's path: the `aload_0; arraylength` pair (above), and
  `refresh_debugger_gate!` at every back edge, which stored three loop
  locals per back edge and — because a workspace-wide `cargo build` unifies
  `libcratonvm`'s `experimental-debug` feature into `cratonvm-vm` — also
  loads the JDWP gate in the measured `cratonvm` binary. The refresh now
  compares first and writes only on a change. Re-measure `arrayLoop` and
  `sumLocals` against the wave-4 binary; if `arrayLoop` is still not faster,
  build `-p cratonvm-cli` alone (no `experimental-debug`) to separate the
  gate load from the fusion.

Tests (`l1_dispatch_tests`):
`array_length_compare_and_branch_fuses_for_every_condition` (six conditions,
three lengths, five indices, short and `n` spellings) and
`javac_array_loops_fuse_their_length_test` (the javac loop over low and high
locals, the `aload n; arraylength` pair, null arrays throwing from all three
shapes, the local reader declining null / int slots).

Still not done: `iload n` / `istore n` operands for the `iadd` fusion;
`aload_N; iload_M; iaload`; the reverse test `aload a; arraylength; iload i;
if_icmp` (rare in javac output); moving the decision to quickening time
(item 5). Take the next shape from a `CRATONVM_QUICKEN_STATS=pairs` run on
the suites, not from reading.

## Progress (wave 24) — lane L7

* **`aload_N; iload_M; <x>aload`** (javac's `a[i]`, both in locals 0-3), in
  the `aload_0..3` arm of `execute_frame_from_index`: the array and the index
  are read straight out of their locals and the element goes through the
  `*aload` arm's own quickened readers (`field_fast::array_load_prim`, and
  `array_load_ref` for `aaload`), so the admission is the unfused arm's
  (`fast_field_zgc`; `CRATONVM_JIT_NO_FIELD_FAST_PATH=1` turns both off). A
  null / non-`Object` array local, a non-int index local, a negative or
  out-of-range index, or an element type the opcode does not name pushes
  nothing and runs the plain `aload_N`, so the `*aload` throws its NPE /
  AIOOBE at its own pc; nothing in the group throws; declined under
  `fusion_off`. Chosen without the census (none has run): it is the element
  read of every javac array loop, the same loops the wave-5 length test
  fused, and it removes two of the three dispatches of each element read
  plus a push/pop pair.
* Test: `l1_dispatch_tests::array_element_reads_fuse_and_keep_their_answers`
  (an int-array sum loop, a `long[]` element, a null array, an index past the
  end and a negative index still throwing).
* Bench: `tools/probes/interp/L7/L7W24ArrayElementBench.java` — rows `int`,
  `long`, `byte`, `char`, `ref` should drop under `--nojit` against the
  wave-23 build, `control` (the same loop without an element read) flat.

Still not done: the `iload n` / `aload n` spellings of this group (locals 4+),
`iload n` / `istore n` operands for the `iadd` fusion, moving the decision to
quickening time (item 5). The census remains the way to choose the next one.

## Progress (wave 25) — lane L7: the census, and the fusion it picked

**The census.** No `CRATONVM_QUICKEN_STATS=pairs` run had been recorded, and
lanes cannot run CratonVM, so wave 25 built an offline one that needs only a
JDK 25: `tools/interp-pair-census/PairCensus.java` (`java.lang.classfile`).
`static` counts every fall-through pair / triple in the methods of a class
root (`jrt:java.base` by default), with a loop-weighted column (8x per
enclosing backward-branch range); `run <dir> <Main>` loads a program's own
classes through a loader that puts a counter after every instruction that
falls through and prints the pairs and triples actually EXECUTED (JDK classes
are not instrumented, so it is for benchmarks whose hot loops are their own).
Pairs are counted on raw opcodes (what the fast path dispatches) and by
family (`iload_1` and `iload 5` are both `iload`). The host's own
`CRATONVM_QUICKEN_STATS=pairs` is still the ground truth for what CratonVM
dispatches SEPARATELY (fused pairs drop out of it); run it on the suites to
confirm.

Dynamic, CratonBench's five interpreted-code kernels (`fib`, `sieve`,
`matrix`, `bintrees`, `arithmetic`), each kernel weighted equally (share of
its own executed pairs, averaged), raw opcodes; `F` = already fused on the
fast path, `F25` = fused this wave:

| # | pair | share | | # | pair | share |
|---|---|---|---|---|---|---|
| 1 | `iload_0 -> iconst_1` | 4.80% | F (`if_icmp`), not `isub` | 11 | `invokestatic -> iadd` | 2.13% | |
| 2 | `isub -> invokestatic` | 3.46% | | 12 | `iload_1 -> if_icmpgt` | 2.12% | F |
| 3 | `lload -> ldc2_w` | 2.73% | | 13 | `if_icmpgt -> aload_0` | 2.08% | |
| 4 | `iconst_1 -> if_icmpgt` | 2.67% | F | 14 | `iinc -> goto` | 2.02% | F |
| 5 | `iadd -> istore` | 2.32% | only after `iload_X; iload_Y` | 15 | `aload_0 -> getfield` | 1.59% | F |
| 6 | `aload_0 -> iload` | 2.28% | | 16 | `if_icmpgt -> iload_0` | 1.33% | |
| 7 | `aaload -> iload` | 2.22% | | 17 | `iload_0 -> ireturn` | 1.33% | |
| 8 | `iload -> aaload` | 2.22% | F25 | 18 | `iadd -> ireturn` | 1.33% | |
| 9 | `iload -> iaload` | 2.22% | F25 | 19 | `iconst_2 -> isub` | 1.33% | |
| 10 | `iconst_1 -> isub` | 2.13% | | 20 | `iload_0 -> iconst_2` | 1.33% | |

Top triples, same weighting: `iload_0; iconst_1; if_icmpgt` 3.33% (F),
`iconst_1; isub; invokestatic` 2.61%, `iload_0; iconst_1; isub` 2.61%,
`iload_1; if_icmpgt; aload_0` 2.38%, `iload n; aaload; iload n` 2.35% (F25
twice), `aaload; iload n; iaload` 2.35% (F25), `invokestatic; iadd; ireturn`
2.14%. In the matrix kernel alone `aload_N -> iload n`, `iload n -> aaload`,
`aaload -> iload n` and `iload n -> iaload` are the top four pairs (44% of its
executed pairs): the wave-24 `aload_N; iload_M; <x>aload` group never fires
there, because javac numbers `i, j, k` past local 3.

Static, `java.base` (61 344 methods), loop-weighted, by family: `aload ->
getfield` 4.50% (F for `aload_N`), `aload -> invokevirtual` 3.23%,
`aload -> iload` 3.23%, `aload -> aload` 2.81%, `astore -> aload` 2.67%,
`iload -> iconst` 2.53% (F before `if_icmp`), `iload -> iload` 2.19% (F
before `iadd` / `if_icmp`), `istore -> iload` 2.19%, `iconst -> istore`
1.88%, `istore -> goto` 1.59%, `aload -> iconst` 1.42%, `ifeq -> aload`
1.34%, `aload -> invokeinterface` 1.31%, `iload -> invokevirtual` 1.24%,
`invokevirtual -> aload` 1.20%, `getfield -> aload` 0.93%, `invokevirtual ->
goto` 0.90%, `istore -> aload` 0.89%, `iconst -> if_icmpne` 0.89% (F),
`ldc -> invokevirtual` 0.86%.

**Landed: `iload{_N, n}; <x>aload` with the array already on the stack**
(`vm/src/runtime/interpreter.rs`, the `iload_0..3` arm after its other
fusions and the `iload n` arm after `fused_icmp_arm!`; the reader is the new
`field_fast::array_load_top_with_index`). It covers the inner dimension of
`m[i][k]` (`aaload; iload k; iaload`), every `this.a[i]` (`getfield a;
iload i; iaload`) and `a[i]` with `i` past local 3 (`aload_0; iload 5;
iaload`, where the wave-24 group does not match). The array slot is peeked,
not decoded; the element goes through the `*aload` arm's own
`array_load_prim` / `array_load_ref`, so the admission (`fast_field_zgc`,
`CRATONVM_JIT_NO_FIELD_FAST_PATH`) is the unfused arm's; a null or
non-`Object` array slot, a non-int or negative index, an index past the end
or an element type the opcode does not name leaves the stack exactly as it
was (the array slot restored bit- and kind-exact) and the plain `iload`
runs, so the `*aload` throws at its own pc. Declined under `fusion_off`.
Cost to every other `iload_N`: one range test of the byte already in hand.

* Test: `l1_dispatch_tests::stacked_array_element_reads_fuse_and_keep_their_answers`
  (short and wide index, a `long[]`, a null array, a negative and an
  out-of-range index still throwing).
* Bench: `tools/probes/interp/L7/L7W25StackedElementBench.java` — `--nojit`,
  A/B against the wave-24 build: `matrix`, `field`, `wide` should drop,
  `control` stay flat.

**Next, by the census:** `iload{_N, n}; iconst; {iadd, isub}` pushing the
result (fib's `n - 1` / `n - 2`, bintrees' `depth - 1`: 2.6% + 1.7% of the
triples), then the long loop of `arithmetic` (`lload; ldc2_w; lcmp` and
`lload; lload; ladd; lstore`), then `iadd; istore` after any producer.
