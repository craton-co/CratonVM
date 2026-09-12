# Schedule-late at equal depth: the spills went, and the time did not

**2026-09-12.** `c2-the-partial-unroller-20260911.md` §6 opened with one item and
said everything else on its list was worth nothing until that item landed:

> **Schedule-late at equal depth** (§5). Until the copies stop being computed
> above the first test, this transform trades back edges for spills and the
> trade is even.

It is built, it is behind `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` (default OFF), and
**it does exactly what that page predicted to the schedule and nothing at all to
the clock.** Both halves of that sentence are the result; the second is why this
page exists rather than a one-line changelog entry.

| | flag OFF | flag ON |
|---|---:|---:|
| `peak_live` | 15 | **13** |
| `scan_spills` | 7 | **5** |
| `scan_reloads` | 7 | **5** |
| interval `splits` | 18 | **16** |
| phi `reg_publishes` | 5 | **0** |
| frame references in the emitted body | 1,596 | **1,212 (−24%)** |

`bench/C2PartialUnrollProbe.java`'s shape under
`CRATONVM_JIT=force-c2 CRATONVM_JIT_IR_PARTIAL_UNROLL=1 CRATONVM_JIT_IR_PER_COPY_FRAMES=1`,
read off `[ir-ls]` under `CRATONVM_DBG_IR_LINEAR_SCAN=1`. Deterministic counters,
not wall clock — which is the only reason this page can state an effect at all.

## 1. What was wrong, in three places rather than one

§5 of the partial-unroller page named one: `ir_schedule::sink_pure_nodes` moves a
pure node **only when the loop depth strictly decreases**, every copy of an
unrolled body sits at the header's own depth, so no copy moves. That is true and
it is not sufficient. Lifting the depth rule alone moves nothing in a loop, and
two further things had to be true first.

**The phi edge rule.** A phi's value input is used **on its edge** — in the
matching predecessor block — not in the phi's own block. `sink_pure_nodes` built
its use map by attributing every input of a node to that node's block, so a loop
header was a use site for every carried value and the deepest common dominator of
any carried value's uses was the header. The depth rule was never the only thing
pinning them. §5 of the prior page says this in one clause; it is half the
change.

**The safepoint anchor was keyed by bci.** The obligation the pass owes is that a
value a deopt frame names must dominate every block that could anchor that
frame. The check built `bci -> [block]` and asked it per snapshot. After a clone
there are `factor` nodes at one bci, so **every copy's frame claimed every
copy's blocks**, and a value sunk into copy 1 read as failing to dominate copy 0.
The whole-method placement was then thrown away — silently, because the revert
is not a refusal.

That last one is the interesting one, because the fix already existed and was
built for this exact reason. `Node::frame_snapshot` is the per-copy identity
`c2-per-copy-deopt-frames-20260911.md` added; `ir_lower::resolve_frame_state_for_site`
prefers it and falls back to the by-bci scan. The scheduler's check was asking a
question the lowerer had stopped answering. It now mirrors that resolution
exactly, in `resolved_snapshot`.

**And the revert now falls back rather than down.** The equal-depth rule can
violate the obligation in strictly more ways than the depth rule alone, and the
revert is whole-method. Reverting straight to "no sinking" would let the new rule
cost the OLD one its wins on any method that happens to name a sunk value in a
frame. `sink_pure_nodes` now retries the placement *without* the equal-depth rule
and reverts only if the depth rule alone cannot satisfy it either.

## 2. The measurement, and the negative half

`tools/tier-ab/flag-ab.sh`, nine rounds, arms interleaved ABBA/BAAB, a control
arm identical to A every round so its spread is the noise floor. Windows dev box
with unrelated load, so the floors are real and are reported.

| arm | floor | effect | verdict |
|---|---:|---:|---|
| equal-depth sink, partial unroller armed (`UnrollLoop`) | 0.2% | +0.0% | **UNMEASURABLE** |
| equal-depth sink alone (`UnrollLoop`) | 1.7% | −0.7% | **UNMEASURABLE** |
| equal-depth sink alone (`FieldLoop.sum`) | 2.0% | −2.6% | 0.974x faster |
| the partial unroller, equal-depth ON | 1.0% | −2.0% | 0.980x faster |
| the partial unroller, equal-depth OFF | 0.7% | −1.7% | 0.983x faster |

Two things to read off it.

**The unroller pays about 2%, and it paid that before this change.** The prior
page reported 0.98 "against a run-to-run spread of ±8%" and called it no
difference. With a control arm and nine rounds the floor is 0.7–1.0% and the same
0.98 is *above* it. The transform was always worth ~2%; the instrument was not
sharp enough to say so. That is a correction to the prior page's headline, not to
its reasoning.

**Removing the spills did not add to it.** −29% spills, −24% frame references,
and 1.000x. The probe's body is `a += i ^ (a >>> 3)` — a serial dependency chain
on `a`, three dependent ops, and the loop runs at about three cycles an
iteration. The spilled values are `i` and the loop bound, which are **not on that
chain**, so their reloads issue in the shadow of the chain and cost nothing. This
is the same conclusion the prior page reached about instruction count (16.25
against 20 per iteration, buying nothing) arrived at from the register side, and
it means §6's ordering was wrong: the copies being computed above the first test
was not what stood between this transform and a win, because *nothing* did — the
win was already there and was 2%.

**What this does NOT say.** It does not say spilling is free. It says this probe
cannot see it, because this probe is latency-bound. A throughput-bound shape
(`FieldLoop -Dprobe.wide=true`, four independent accumulators, built for exactly
this distinction) measured −3.6% inside a 7.2% floor — the box was busy and the
arm is unreported rather than negative. Anyone re-running this should take that
arm on a quiet host first.

## 3. Green

* 2,393 `cratonvm-jit` unit tests, 145 `ir_vs_singlepass` differential tests, and
  every other `cratonvm-jit` test target — **with the flag off and with it on**.
* Regression suite **95/95** against Temurin 25, twice: default flags, and with
  `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` + `CRATONVM_JIT_IR_PER_COPY_FRAMES` +
  `CRATONVM_JIT_IR_PARTIAL_UNROLL` all armed.
* `CratonBench` (all seven phases) and `CratonBenchC2` (all three) — **every
  checksum identical** to the default arm and to HotSpot, flags on and off.

Two unit tests pin the behaviour, and the second is the one worth keeping:
`equal_depth_sink_moves_a_pure_node_to_its_only_reader` asserts the move, and
`a_value_a_frame_names_on_the_other_arm_does_not_sink` asserts the refusal. The
first draft of the first test used a local, which every later snapshot names, so
it asserted the refusal while reading as if it asserted the move. A third test
pins that the flag is read live rather than through a `OnceLock`, which is the
trap `ir_per_copy_frames_enabled` documents and which makes both arms report
byte-identical code.

## 3a. A third shape, measured the same day: 1.024x — SLOWER

`fib` is the shape this rule most obviously fits, and it was tried there the
same day. `FibCall.fib` computes BOTH recursive arguments before testing its
base case, so four instructions run on roughly half of all calls for nothing;
the flag removes them exactly as designed, and the entry block goes straight
from `mov rbx,rax` to `cmp ebx,1`.

It is **2.4% slower** — fifteen interleaved rounds, control arm, effect +2.4%
over a 2.1% floor. The body grows **788 → 823 bytes**: what leaves the base-case
path reappears, larger, on the recursive path.

So the flag has three measurements on three shapes — **1.000x**, **0.974x**,
**1.024x** — and they average to nothing. §4 left it OFF for want of evidence;
this is the second and better reason, and it is the one a soak on the first two
shapes would have missed.
[`c2-fib-per-call-budget-20260912.md`](c2-fib-per-call-budget-20260912.md) §4.

## 4. Why it is default OFF

Because the wall-clock evidence for it is one 2.6% arm over a 2.0% floor on one
shape, nothing on a second, and a 2.4% REGRESSION on a third (§3a). The allocator counters are unambiguous and the
correctness evidence is broad, but this repo's convention is that a flag flips on
a measurement and not on an argument, and the measurement here is "the schedule
got better and the clock did not move".

The honest next step is not to flip it. It is to take the throughput-bound arm on
a quiet host, because that is the one shape where the counters above should turn
into time — and if they do not there either, then register pressure is not what
this tier is losing to, and `c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`
was right about more than it claimed.

## 5. Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d probes/out probes/UnrollLoop.java probes/FieldLoop.java

# the counters
CRATONVM_JIT=force-c2 CRATONVM_JIT_IR_PER_COPY_FRAMES=1 \
CRATONVM_JIT_IR_PARTIAL_UNROLL=1 CRATONVM_DBG_IR_LINEAR_SCAN=1 \
CRATONVM_JIT_IR_SINK_EQUAL_DEPTH=1 \
  ./target/release/cratonvm -cp probes/out -Dprobe.reps=1 -Dprobe.n=100 UnrollLoop \
  2>&1 | grep 'ir-ls'

# the clock, with a control arm
bash tools/tier-ab/flag-ab.sh -Exe ./target/release/cratonvm -Cp probes/out \
  -Class UnrollLoop -Flag CRATONVM_JIT_IR_SINK_EQUAL_DEPTH -On 1 -Off 0 \
  -Base "CRATONVM_JIT_IR_PER_COPY_FRAMES=1,CRATONVM_JIT_IR_PARTIAL_UNROLL=1" -Rounds 9
```
