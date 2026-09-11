# The optimizing tier's frame traffic is one carry slot, not a scheduling order — FIXED

**2026-09-10, extended 2026-09-11.** A counted loop whose entire live state is
two `long`s and an `int` spent **55 of 161 instructions** on `[rbp-*]` traffic,
and two of its three intermediate stores were never read back.

The obvious lever — scheduling a single-use operand next to the consumer that
reads it, so the existing carry can take it — was built, engaged, and **measured
a wash** (§4). It is a wash by construction: the emitter held ONE carry, so
pairing could only move the carry between operands, never add one.

The fix was therefore in the emitter and not the scheduler: a second carry slot,
with its own soundness proof, so a consumer can take both its operands in
registers (§6). **1.090x on the kernel, ranges non-overlapping.** The scheduling
pass ships with it because the two slots need the order it produces — but on its
own it does nothing, which is why §4 is still in this page rather than deleted.

**Then the second slot turned out to fire once in 1129 compiled methods**, and
the reason was that its RCX-preservation rule asks an OP a question only the ARM
can answer: every binary arm reaches RCX through the register form of its own
second operand, and an operand folded into an immediate never takes that branch
(§7). Asking it per node instead takes the probe set from 1 deferred carry to
11, is worth **1.009x** against a 0.1% floor on the kernel that has the shape
(§8), and — the reason this page can be retired — the census that widening
made possible says where the remaining traffic is, and it is not here: **82% of
candidates fail on operand POSITION** (§9, §10).

Sibling of `c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`,
and the same shape of answer: the thing that looked like the constraint was not.

## 1. The traffic, from the emitted bytes

`PollReach.hotLoop` — `long s; for (int i = 0; i < n; i++) s += i ^ (s >>> 3);`
— driven to the OSR/optimizing tier, release binary, `CRATONVM_DBG_JIT_DISASM`:

```asm
mov rax,rbx / movsxd rax,eax / mov [rbp-78h],rax    ; I2L    -> home
mov rax,r15 / shr rax,3      / mov [rbp-88h],rax    ; UShr   -> home
mov rcx,rax / mov rax,[rbp-78h]                     ; reload the I2L
xor rax,rcx                  / mov [rbp-90h],rax    ; Xor    -> home
mov rcx,rax / mov rax,r15 / add rax,rcx / mov r13,rax   ; Add -> r13, no home
```

Slot by slot, over the whole body:

| slot | stores | loads |
|---|---:|---:|
| `[rbp-88h]` | 1 | **0** |
| `[rbp-90h]` | 1 | **0** |
| `[rbp-78h]` | 1 | 1 — reloaded into the register that already held it |

Whole body: 19 stores and 13 loads out of 161 instructions. The `Add` **did**
get its home dropped, so the home-dropping machinery works; what did not happen
is that the three intermediates were never in registers to begin with.

## 2. The census names the cause, and it is deliberate

`CRATONVM_DBG_IR_LINEAR_SCAN=1` on that method:

```text
nodes=25 positions=19 peak_live=8 scan_promoted=13 resident=5 (fp=0 gp=5)
skipped: ... const=4 single_use=16 ... no_alloc=3 carried_reserved=3
homes kept: switch=0 deopt=0 type=0 op=1
homes: dropped_values=6 stores_skipped=6 read_refusals=0
carries: planned=2 taken=2 read=2 refused=0 stores_dropped=2
carry skips: multi_use=2 wrong_type=0 producer_arm=4 already_resident=2 consumer_arm=0 operand_position=1
```

`homes kept: switch=0 deopt=0 type=0 op=1` — the home-drop conjunction is
essentially fully effective **on the values it is asked about**. It is not the
problem.

`single_use=16` of 25 nodes is. `plan_register_residency` will not spend a
callee-saved register plus its prologue save on a value read once, which is
correct. `ir_carry_single_use` exists to cover exactly those, and took 2.

## 3. The hypothesis: they were not ineligible, they were not ADJACENT

`plan_carries` inspects strictly adjacent scheduled pairs
(`block.nodes[w - 1]`, `block.nodes[w]`). In this loop `I2L` and the `Xor` that
reads it have the other operand's `UShr` scheduled between them, so the planner
sees the pair `(I2L, UShr)`, finds `UShr` does not read `I2L`, and files it
under `operand_position=1`.

So: sink a single-use, carry-eligible operand to sit immediately before its
consumer. Sinking both in reverse order leaves `[input1, input0, cons]`, which
is the order the carry wants — a consumer's arm reads its first operand from
RAX and its second from RCX.

Sinking a definition later is not safe in general: the value is undefined at any
deopt arriving in between, and `graph.safepoints` names the full operand stack
at every bci. The condition is about what is crossed, not about the value —
every node strictly between the old and new position must be one no deopt can
arrive at (`op_cannot_deopt`, the same enumeration
`compute_deopt_named_reachable` builds its trapping-bci set from).

That was built, with three tests (node-set preservation, definitions before
uses, no sinking past a trapping node). 2339 jit unit tests and the 145
`ir_vs_singlepass` differential tests passed with it default-ON.

## 4. Measured, and rejected on its own

Same release binary, `CRATONVM_JIT_IR_PAIR_OPERANDS` as the A/B:

| arm | bytes | instrs | `mov reg,reg` | `[rbp]` ops | carries taken |
|---|---:|---:|---:|---:|---:|
| pairing **on** | 777 | 161 | 22 | 55 | 2 |
| pairing **off** | 774 | 162 | 23 | 55 | 2 |

−1 instruction, **+3 bytes**, identical memory traffic. Both arms print the same
result.

**And it is a wash by construction, not by bad luck.** `taken=2` in both arms.
The pairing cannot raise the carry count: it changes WHICH operand is carried,
from the RCX form to the stronger RAX form, and pushes the other operand's round
trip into the frame in its place. One in, one out.

## 5. What the ceiling actually is

Carrying *both* operands is what would remove the traffic, and two things stop
it. Neither is a scheduling decision:

* **`Lowerer::live_carry` is a single `Option`**, not a set. The emitter holds
  one carry at a time, with a soundness proof per form — an RAX carry is proved
  by `buf.pos()` (nothing emitted since the producer left the value there), an
  RCX carry rests on `op_reads_rax_then_rcx`. A second simultaneous carry needs
  a second slot and its own proof.
* **Every binary arm writes RCX.** `Op::Add` and its family do
  `gp_load_value(RCX, node.inputs[1])` for a non-immediate second operand, so an
  RCX carry cannot survive one of them sitting in between. `Op::Neg` writes RCX
  too, in its `IrType::Double` arm.

That second point is worth stating plainly because an allowlist written from the
op names rather than from the arms gets it wrong: a first attempt at
`op_preserves_rcx` listed `Add`, `Sub`, `And`, `Or`, `Xor` and `Neg` as
RCX-preserving, and all six write RCX. The failure would have been a dropped
home whose carry is never honoured — which fails closed (a dropped home refuses
the read and bails the compile) rather than miscompiling, but the full jit suite
and the differential suite both pass with it in.

## 6. The emitter grew a second slot, and it is worth 1.090x

Route 1 below was taken. `Lowerer::deferred_rcx` is a second carry slot holding
`(producer, consumer, allowance)`; the adjacent `live_carry` is unchanged. It is
RCX-only by construction, because the consumer reads operand 0 from RAX — where
the adjacent producer left it — so the only value that can still be in flight
across an arm is operand 1.

The adjacent carry proves itself with `pos == buf.pos()`: nothing was emitted
since the producer left the value there. A deferred carry cannot say that, and
what stands in for it is [`op_preserves_rcx`], re-checked in
`lower_data_node_tracked` against what was actually LOWERED rather than trusted
from the plan. The allowance is 1, spent by the arm in between; running out
refuses the compile.

`op_preserves_rcx` is two ops — `I2L` and `L2I` — for the reasons in §5.
`every_rcx_preserving_arm_leaves_rcx_alone` is what keeps it honest: it scans
each claimed arm's source for any `RCX` identifier AND pins the exact
`buf.emit(&[..])` byte literals, because a raw ModRM can name RCX where no
identifier scan would see it. Adding `Op::Add` to the list makes it fail.

`ir_schedule::pair_single_use_operands` supplies the `[input1, input0, cons]`
order the two slots need. On its own it is inert — see §4, which is why it is
not a separate change.

### Measured

`probes/PollBench.java` (`s += i ^ (s >>> 3)`, the same kernel
`probes/PollReach.java` carries as `hotLoop`), release binary, one flag
(`CRATONVM_JIT_IR_CARRY_2ND`), arms interleaved ABBA x3, 6 samples each, best-of-7
inner rounds per sample:

| arm | median ns/iter | min | max | spread |
|---|---:|---:|---:|---:|
| `=0` | 1.0399 | 1.0345 | 1.0937 | 5.7% |
| **`=1`** | **0.9537** | 0.9115 | 0.9745 | 6.9% |

**1.090x**, and the ranges do not overlap — every `on` sample beats every `off`
sample. Checksums identical across arms and against HotSpot
(`791133517366464198`). HotSpot on the same source and host is 0.777 ns/iter, so
the gap on this kernel goes from 1.34x to **1.23x**.

Emitted code, same method:

| arm | bytes | instrs | `[rbp]` ops | carries | deferred |
|---|---:|---:|---:|---|---|
| `=0` | 777 | 161 | 55 | planned=2 taken=2 dropped=2 | 0/0 |
| `=1` | 766 | 160 | 53 | planned=3 taken=3 dropped=3 | **1/1** |

`refused=0`. The two `[rbp]` operations removed are exactly the `[rbp-78h]`
store and its reload two instructions later — the store-to-load forwarding pair
`docs/JIT_OPTIMIZATION.md` traced this tier's residual to.

Green: 2340 jit unit tests, 145 `ir_vs_singlepass` differential tests, 2641 vm
unit tests.

## 7. `op_preserves_rcx` asks the OP a question only the ARM can answer

§5 and §6 are right about the op and wrong about the arm, and the gap between
those two statements is worth 10 of the 11 deferred carries this probe set
takes.

> Every binary arm loads its own second operand with `gp_load_value(RCX,
> node.inputs[1])`.

True of the arm. **False of the path the arm takes when that operand is a
constant.** Every one of them is shaped like this:

```rust
self.gp_load_value(RAX, node.inputs[0]);
if !self.emit_alu_acc_imm(node.inputs[1], 0x35, true) {
    self.gp_load_value(RCX, node.inputs[1]);   // <- the only route to RCX
    self.buf.emit(&[0x48, 0x31, 0xC8]);        //    xor rax,rcx
}
self.store_rax(slot);
```

`x + 1`, `x & 0xFF`, `x >>> 3` and `x * 31` take the folded branch — that is
what `ir_alu_imm` is for, and it has been default-ON since 2026-09-05 — and the
folded branch never reaches RCX at all. So the arm CAN carry a deferred value
across itself; it just cannot say so as an op.

`Lowerer::node_preserves_rcx` is `op_preserves_rcx` plus that, asked per node:

```rust
node.inputs.get(1).is_some_and(|&b| self.alu_imm32(b).is_some())
```

which is the arm's own question rather than a proxy for it — all three fold
helpers (`emit_alu_acc_imm`, `emit_imul_imm`, `emit_shift_imm`) open with
exactly `let Some(imm) = self.alu_imm32(id) else { return false; };`.
`CRATONVM_JIT_IR_CARRY_RCX_FOLDED=0` restores the two-op rule; with it off,
`node_preserves_rcx` is `op_preserves_rcx(&node.op)` and nothing else runs.

### Widening an audit needs a net under it, so the audit became an observation

§5's warning is the reason this could not simply be a longer list: the first
attempt at `op_preserves_rcx` claimed six ops that all write RCX, and **the full
unit suite and the differential suite both passed with it in**, because a
deferred carry that is never honoured fails closed. Nothing said so. A list that
now depends on a *branch prediction* rather than on an op name is a bigger
target of the same kind.

So the claim stopped being predicted and started being observed.
`Lowerer::rcx_writes` counts every route in `ir_lower.rs` that reaches RCX —
`gp_load_value`, `load_to_rcx`, `emit_mov_reg_reg64`, and the two immediate
forms all call `note_rcx_written` first — and `lower_data_node_tracked` compares
it across the node the carry crosses. "Did this arm write RCX" is therefore a
fact about the emission rather than a prediction about the op. The one write
that is deliberate, `store_rax` moving a carried value out of RAX, goes through
`emit_mov_reg_reg64_raw` so it is not counted against the carry it is
installing.

A wrong audit now costs the METHOD, at the node that did it, and never a wrong
answer at run time.

### Three tests, each mutation-checked

* **`every_folded_arm_reaches_rcx_only_in_its_register_form`** pins all three
  source facts: the fold helpers decline on exactly `alu_imm32`; every mention
  of RCX in each claimed arm is inside that fold's `else` branch, and there is
  exactly one such branch per arm; and nothing outside it can reach RCX by
  another route — no raw `buf.emit`, and only calls from a closed list whose
  sole register-bearing member is `gp_load_value(RAX, node.inputs[0])`. Adding
  `Op::Neg` to the list fails it; naming RCX outside the guard fails it.
* **`a_folded_middle_arm_carries_both_operands_and_still_computes_the_answer`**
  is the half no source audit can do. It lowers `(a + 1) ^ (b * 3)` twice from
  one graph — paired and unpaired — and RUNS both, so a divergence is
  attributable to the pairing and nothing else. It also asserts the carry fired
  rather than assuming it: `ir_carry_deferred_census()` has to move.
* `every_rcx_preserving_arm_leaves_rcx_alone` is unchanged and still governs the
  op-level list.

## 8. Measured

### The emitted code, which is deterministic

`CRATONVM_DBG=jit-disasm`, first optimizing-tier body of each method, both arms
from one binary:

| method | arm | bytes | instrs | `[rbp]` ops | loads | stores |
|---|---|---:|---:|---:|---:|---:|
| `FoldCarryBench.kernel` | **on** | **948** | **187** | **68** | **16** | **24** |
| (= `OsrTierProbe.kernel`) | off | 970 | 189 | 72 | 18 | 26 |
| `PhiSwapLoop.swap2` | **on** | **1058** | **200** | **78** | **20** | **24** |
| | off | 1080 | 202 | 82 | 22 | 26 |
| `PhiSwapLoop.rot3` | **on** | **1295** | **230** | **100** | **27** | **32** |
| | off | 1317 | 232 | 104 | 29 | 34 |
| `PollReach.hotLoop` | on | 1190 | 200 | 79 | 25 | 30 |
| | off | 1190 | 200 | 79 | 25 | 30 |

**The accounting closes exactly, which is the part worth checking.** Each
converted carry removes a 7-byte home store and a 7-byte reload and adds a
3-byte `mov rcx,rax`: −11 bytes, −1 instruction, −2 frame operations. Every row
above is two carries — −22 bytes, −2 instructions, −4 frame operations — and the
census independently reports 2, 2 and 2 for those three methods (`rot3` and
`swap2` are `PhiSwapLoop`'s 4). `PollReach.hotLoop` is byte-identical because
its crossing arm is an `I2L`, which `op_preserves_rcx` already covered: §6's
kernel is exactly the case this does not touch.

The removed pair, from the `off` arm's diff, is the store-to-load forwarding
shape this whole page has been about:

```asm
mov [rbp-0C8h],rax       ; home store
mov rax,r13
imul eax,1Fh
mov rcx,[rbp-0C8h]       ; reloaded three instructions later
add eax,ecx
```

### Wall clock could not measure it, and said so twice

`tools/tier-ab/flag-ab.sh` on `FoldCarryBench`, host load 40–90:

| run | rounds | A (off) | C (control) | B (on) | floor | effect |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 6 | 119.0 ms | 112.0 ms | 111.0 ms | 6.1% | −3.9% |
| 2 | 12 | 353.5 ms | 407.0 ms | 388.5 ms | 14.1% | +2.2% |

Both **UNMEASURABLE**, and the sign flips between them. Those two runs describe
the machine, not the change, and they are recorded so nobody reads their silence
as disagreement with what follows.

### User CPU could, and it reproduced

`tools/tier-ab/cpu-ab.sh` — the same ABBA/BAAB interleave with a control arm,
measuring `%U` instead of wall clock, because a descheduled process stops
accumulating user CPU and therefore stops charging other tenants to the result.
12 rounds, `-Dprobe.reps=120000` (≈29 s of user CPU per sample, so `%U`'s 10 ms
resolution is not in the answer):

| run | A (off) | C (control) | B (on) | floor (A vs C) | effect |
|---|---:|---:|---:|---:|---:|
| 1 | 29.010 s | 28.990 s | **28.735 s** | **0.1%** | **−0.9%** |
| 2 | 29.330 s | 29.300 s | **29.045 s** | **0.1%** | **−0.9%** |

**1.009x**, an effect nine times the floor, reproduced to the digit on an
independent run. Checksums identical across every sample in both runs.

The size is the right size: four frame operations out of 68 in a 187-instruction
body, on a loop whose other 183 instructions are unchanged.

## 9. The census, across the probe set

§7 of the version of this page that shipped the second slot ended *"the census
wants running across the probe set first"*. It has been run.

Every probe in `probes/` that compiles alone (194 of 218), driven with
`CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG_IR_LINEAR_SCAN=1`, **paired**: each probe
runs under both arms and is counted only if both produced output, because a
probe that times out in one arm and not the other moves every number in the
totals. 94 probes paired; ~1130 optimizing-tier method compiles per arm.

| | `RCX_FOLDED=1` | `=0` |
|---|---:|---:|
| methods compiled | 1139 | 1129 |
| single-use carries planned = taken = read | 738 | 861 |
| carries **refused** | **0** | **0** |
| carry home stores dropped | 625 | 669 |
| **deferred carries taken** | **11** | **1** |
| candidate windows | 444 | 549 |
| declined — operand position | **363 (82%)** | **467 (85%)** |
| declined — producer's arm or type | 35 (8%) | 35 (6%) |
| declined — producer multi-use | 31 (7%) | 32 (6%) |
| declined — producer already carrying | 0 | 0 |
| declined — producer resident | 0 | 0 |
| declined — consumer | 0 | 0 |
| declined — middle arm can write RCX | 4 (0.9%) | 14 (2.6%), **10 foldable** |

A **candidate** is a consumer already taking its FIRST operand in RAX — the
shape the second slot exists for, and the only honest denominator. The raw
triple count (`non_candidate_windows`, ~7900) is every other three consecutive
scheduled nodes and no decision depends on it; the first cut of this census
reported it as the dominant cause, which said nothing.

Which probes take one, ON: `PhiSwapLoop` 4, `FoldCarryBench` 2, `JmxBlast` 2,
`OsrTierProbe` 2, `PollReach` 1. OFF: `PollReach` 1. Per-probe counts are
stable run to run; the aggregates are not, because a probe's compile set varies
with timing (`JmxBlast` is multithreaded), which is why `carries` reads 738 vs
861 across arms that differ in nothing else. **Read the per-probe column, and
the cause distribution as percentages.**

Three things this settles:

1. **The widening is most of the mechanism's reach.** 10 of 11 deferred carries
   exist only because of it. Without it the second carry slot fires **once in
   1129 compiled methods** — and that once is `PollReach.hotLoop`, the kernel it
   was built on.
2. **`op_preserves_rcx` was never the binding constraint anyway**, and now
   demonstrably is not: 0.9% of candidates. Even at its unwidened value it was
   2.6%. This retires §7's second bullet, below.
3. **`refused=0` across ~2300 method compiles in both arms.** The deferred
   carry's fail-closed net never fired, which is what a widened audit backed by
   an observation was supposed to buy.

## 10. What is still open, re-pointed

**Retired: "widen `op_preserves_rcx` by making the arms deserve it."** The
previous version of this page proposed giving every binary arm a caller-supplied
scratch register instead of RCX — "a mechanical change to ~50 arms rather than a
design question". It is a change to ~50 arms for **0.9% of candidate windows**.
Do not start it from this page.

**The lever is operand POSITION: 82% of candidates.** The node two positions
back is not the consumer's second operand, so the triple
`[input1, input0, cons]` that both slots need was never formed.
`ir_schedule::pair_single_use_operands` is what would form it, and it declines
for reasons it already enumerates: the operand is used more than once, its op is
not one `op_home_is_one_store_rax` certifies, it is in another block, or there
is a node between it and the consumer that a deopt can arrive at. **Which of
those four dominates is not yet counted** — the pairing pass has no census, and
that is the next thing to build, not the next thing to fix. It is one counter
per `continue` in a 60-line function.

**Still standing, unchanged: promote single-use values into caller-saved scratch
registers.** They cannot repay a callee-saved register's prologue save, which is
why `plan_register_residency` declines them; a caller-saved register has no save
to repay. This subsumes carries rather than extending them. Larger blast radius:
it changes register pressure and interacts with every existing residency
invariant. Note that it does NOT depend on the operand-position finding above —
it is the other route out of the same `single_use` skip.

## 11. Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d /tmp/pr probes/FoldCarryBench.java probes/PollReach.java

# §8 — the emitted code, both arms from one binary
for F in 1 0; do
  CRATONVM_JIT_IR_CARRY_RCX_FOLDED=$F CRATONVM_JIT_FORCE_C2=1 \
  CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FoldCarryBench.kernel \
    ./target/release/cratonvm -cp /tmp/pr FoldCarryBench 2>&1 \
    | awk '/osr-optimizing\/ir/{f=1} f&&/^\[cratonvm-jit-disasm\] osr\/sp/{f=0} f' \
    | grep -c 'rbp-'
done

# §8 — the throughput, on a host too busy for wall clock
bash tools/tier-ab/cpu-ab.sh ./target/release/cratonvm /tmp/pr \
    FoldCarryBench CRATONVM_JIT_IR_CARRY_RCX_FOLDED 12 -Dprobe.reps=120000

# §9 — the census, one method at a time
CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG_IR_LINEAR_SCAN=1 \
  ./target/release/cratonvm -cp /tmp/pr -Dprobe.reps=3 PollReach 2>&1 \
  | grep -E 'carries:|deferred'

# §9 — the process-wide totals, for a workload rather than a kernel
CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG=jitc \
  ./target/release/cratonvm -cp /tmp/pr FoldCarryBench 2>&1 \
  | grep 'ir deferred carries'
```

Measurements were taken on a contended 8-core Linux host (load average 40–90
throughout, other tenants building). That is why §8 has a wall-clock table that
says nothing and a user-CPU table that says something: on a quiet host the
wall-clock arm is the better instrument and should be preferred.

## 12. Green

2345 jit unit tests, 145 `ir_vs_singlepass` differential tests, 2641 vm unit
tests, `flag_declaration_guard` and `flag_docs_generated` (which were red on
`dev` — the two flags the second carry slot landed with were declared nowhere,
so every branch's pre-push hook refused).
