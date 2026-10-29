# `matchonly` (and `regex-groups`) still land in two or three levels per process

Status: OPEN. Area: JIT, optimizing tier, nested profile-guarded splice caps. Severity: perf (5-15 % on
`matchonly`, one row; no wrong answer). Found by round rx27 wave 2, lane bimodal, 2026-10-28.

## Evidence

After the keep-loop fix of lane bimodal (`CRATONVM_JIT_IR_KEEP_LOOP_STRING_PREFIX`,
`CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW`; see "Round rx27 wave 2 (lane bimodal)" on
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`) the other affected rows sit in one level, but
`matchonly` still reads 27 075 / 28 959 / 31 337 main/op across processes of one binary (icount2, 5 reps, every
arm, `/root/rx27/bimodal/ic2-bim-ab2.log`), and `regex-groups` 25.5-29.2k (3 reps, both arms).

`matchonly`, `cvm-bimodal-3`, `CRATONVM_DBG_JITC=1` (`/root/rx27/bimodal/m1` = 27.1k, `m4` = 29.0k): the one
difference is `Pattern$Start.match`'s nested row at `BmpCharPropertyGreedy.match` pc 70 (`next.match`,
`LastNode`, class 922, 45 bytes):

* low: `nest-profiled ... callee_pc=70 depth=1 -> guard on class 922 (body 45 bytes, cap 325)`;
* high: `nest-profiled ... class 922 refused by the resolver (body 45 bytes: over cap 35 ...; samples 248)`.

The cap comes from `ir_nested_guarded_body_cap`: hot (325) when the row has at least
`GUARDED_SPLICE_HOT_MIN_CALLS` = 256 samples, cold (35) otherwise. The row is filled only while
`BmpCharPropertyGreedy.match` runs interpreted, i.e. until its C1 body publishes, so its sample count is set by
compile-thread timing and sits right at the bar (248 in the high run; ~190 per mille of ~1 000-1 300
invocations in every run). An absolute count over a timing-bounded window is a race, as the keep-loop rule's
artifact read was.

`regex-groups` was not diagnosed.

## What would close it

A hot / cold decision for nested rows that does not depend on how long the enclosing body stayed interpreted:
a frequency (samples per invocation, which is stable at ~190 per mille here) instead of an absolute count, or
the count scaled to the window. Note that HotSpot's own rule (`InlineFrequencyRatio` 0.25) calls this site
infrequent and would keep the 35-byte cap, i.e. the slower mode; the faster one splices `LastNode.match`. Pick
one deliberately and measure `matchonly`, `cached`, `split-ws`, `regex-split`, `regex-find-email` with
icount2 (5 reps, every value in one level). Then diagnose `regex-groups` the same way (KEEP=1 runs with
`CRATONVM_DBG_JITC=1`, `bm_state.py`-style per-method compile diff of a low and a high process).

## Round rx27 wave 3 (lane bimodal, 2026-10-28; built as `cvm-bimodal-5`, measured against `cvm-rx27b3`)

**pc 70 hot cap: made order-free, behind an opt-in.** `CRATONVM_JIT_IR_NESTED_HOT_CAP_BY_FREQUENCY=1`
(`ir_nested_guarded_body_cap_in`, `jit/src/lib.rs`): with the enclosing body's interpreted invocation count
(the same number `InlineSite::callee_invocations` carries; the VM resolver now reads it once, `cap_invocations`,
so resolver and planner agree) the nested hot cap asks the row's frequency, C2's `InlineFrequencyRatio`
(250 per mille), instead of 256 samples. pc 70 is ~194 per mille in every process, so it is cold every time
(`nest-profiled ... cap 35; samples 259 invocations 1329`). Confirmed on 8 DBG processes. Opt-in because it is
not a win: the deterministic side is the slower one for `matchonly` (27.1k spliced vs 29.0k kept), HotSpot's own
rule.

**`matchonly` is still spread with it** (icount2 `ic2-ab5.log`, 3 reps: base 28.0-31.3k, freq 28.96-31.3k,
nofreq 27.1-28.6k; `ic2-ab4.log` fix arm 27.9-32.4k). Other timing-bounded decisions remain in the same chain
(8 DBG processes with pc 70 pinned: 7 x 28 977, 1 x 28 067): a never-taken branch in the spliced `LastNode`
body pruned only when the window missed it (`spliced never-taken rows ... 2 admitted of 2` vs `1 of 1`), and a
second C2 compile of the row lambda (outside-C1 drain / late-bind rebinding) in some processes. Each is a
profile read over a window whose length compile timing sets; closing the row needs those windows normalised
the same way (or a C1 tier that keeps profiling), not one more switch.

**`regex-groups`** on rx27b3 reads 21.9k / 22.8k / 23.6k. Not ASLR (`setarch -R` wrapper arm: same three
levels, `ic2-norand.log`). A low and a high DBG process take identical splice / guard / cap decisions; they
differ in a second C2 compile of the row lambda (`lambda$row$5` C2 twice in the high one), an extra compiled
`Pattern.lambda$Single$0`, and code size 0.5-2 % larger in nearly every body, C1 ones included. Not diagnosed
further.

**regex-matches** (4.3x): perf on rx27b3 (`prec.sh` + `CRATONVM_DBG_JIT_DISASM=Pattern`): 50 % in a CALLED
`BmpCharPropertyGreedy.match` C2 body (19 KB, 976-byte frame: per call ~30 % of its samples are prologue,
frame clear and home stores), 22 % `Pattern.matcher` (Matcher allocation and reset), 17 % the spliced row,
8 % a called `LastNode.match`. The call exists because `Matcher.match` pc 121's nested row is a `nest-exact`
row for `Pattern$Node` that can never prove its receiver. `CRATONVM_JIT_IR_NESTED_EXACT_YIELDS_TO_PROFILE=1`
(opt-in, `jit_bridge.rs`) drops such a contradicted row at every site; the profiled route then also needs
`CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1` (pc 70 is LastNode / Bmp) and
`CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF=0` (Bmp is not a leaf) to splice. All three together:
regex-matches 15.8k vs 16.5k (-4 %), but matchonly 35.6k (+23 %) and regex-groups 24.3k (+11 %). Not a lever as
is; the per-call cost is the called body's frame and spill traffic, which is code quality, not a splice
decision. R24 facts equal HotSpot 25 in both modes on `cvm-bimodal-5`, default switches and the three opt-ins.
