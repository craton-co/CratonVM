# A fused compare can read its operands where they are — RETIRED 2026-09-11

**Retires `docs/known-issues/perf/c2-a-fused-compare-can-read-its-operands-where-they-are-20260910.md`.**
The page shipped two of four compare forms on 2026-09-10 and named two
residuals: the immediate form of the compare, and the `LEA` for `i + 1`. **Both
are now built, measured and shipping**, and there is nothing left in the residue
this page was tracking. §1-§5 below are the original page, unchanged except
where a number was re-taken; §6-§9 are the residuals.

| | |
|---|---|
| **Verdict** | Fixed and shipping, default ON, two kill switches. Four compare forms and two `LEA` forms, all six engaging on real probes. |
| **What is claimed** | Instructions and bytes, exactly, from the emitted code. |
| **What is NOT claimed** | A speedup. The timing is a **null** on this host and §8 shows why in numbers rather than in a hedge. |
| **Where** | Azure host 2 (`20.80.105.49`), AMD EPYC 9V45 (Zen 5), 8 cores, branch `claude/c2-cmp-imm-lea-residue-20260910` off `origin/dev` `6f507bb48`, JDK 25.0.4. Host load 40-111 throughout, which is the whole of §8. |
| **Flags** | `CRATONVM_JIT_IR_CMP_IN_PLACE=0`, `CRATONVM_JIT_IR_ADD_LEA=0` |

---

## 1. What it emitted

Every counted loop's back edge spent three instructions on its comparison when
one was enough.

`PollReach.hotLoop`, optimizing tier, `i` resident in RBX and `n` in R12:

```asm
mov rax,rbx        ; i -> RAX
mov rcx,r12        ; n -> RCX
cmp eax,ecx
```

Two of the seventeen instructions in the loop body copying values that were
already in registers. `LoopCtl.spin`, where register pressure is higher
(`peak_live=13` against a five-register file), is worse — the bound lost its
register, so the second operand comes from the frame:

```asm
mov rax,rbx        ; i -> RAX
mov rcx,[rbp-60h]  ; n -> RCX, from its slot
cmp eax,ecx
```

Found by re-reading the same loop as
`c2-one-carry-slot-is-the-frame-traffic-ceiling-20260910.md` after the carry work
landed: when the memory traffic went away, what was left at the top of the
residue was the loop control.

## 2. Why a fused compare in particular

Because it owes nothing else. Every other arm that loads operands into RAX/RCX
also has to put a result somewhere: a home word, a resident register, a carry.
A fused compare defines no value, writes no home and publishes no register — the
only thing that outlives it is the flags, and those are identical whichever
registers the comparison names.

Nothing between it and the `Jcc` touches flags, and nothing downstream may
assume RAX holds the first operand: the non-fused path already overwrites AL
with `SETcc` on the phi-copy layout, so no reader could ever have relied on it.

## 3. The four forms

`pick_cmp_form` tries them in this order, which is the order of how much each
saves and also — not by coincidence — the order of how little each needs.

| operands | encoding | emitted | census |
|---|---|---|---|
| first resident, second a constant | `83 /7 ib` or `81 /7 id` | `cmp ebx,64h` | `cmp_imm=n+0` |
| first in its slot, second a constant | the same `/7`, memory `r/m` | `cmp [rbp-60h],64h` | `cmp_imm=0+n` |
| both resident | `39 /r` (`CMP r/m, r`) | `cmp ebx,r12d` | `cmp_in_place=n+0` |
| first resident, second in its slot | `3B /r` (`CMP r, r/m`) | `cmp ebx,[rbp-60h]` | `cmp_in_place=0+n` |

The immediate forms come first because they need the least: a constant folds
into the instruction, so only the FIRST operand needs a place to be read from,
and a first operand that lost its register is served as well as one that kept
it. They are also the common case — `i < 100` is the shape of most Java loops,
and `i < n` is not.

The frame forms are not fallbacks either: `peak_live` routinely exceeds the
five-register GP file, and a loop bound is exactly the long-lived value that
loses its register. Three instructions become one in all four rows.

The 32-bit frame forms read four bytes where the `MOV` they replace read eight.
That is the same comparison — the slot holds a sign-extended `int` in its low
word, and `CMP EAX, ECX` only ever looked at those four bytes either.

## 4. The guards, and that each fails closed

* `carry_names()` declines any value a carry is holding. A carried value must be
  read through `gp_load_value` or the carry strands and the compile is refused.
* the frame forms go through `slot_of_checked`, so a dropped home declines the
  form rather than latching a bailout on a path with a perfectly good fallback.
* the immediate forms gate on `alu_imm32`, the same gate the arithmetic folds
  use. It declines a constant too wide for `i32` — every immediate form here
  SIGN-EXTENDS, so such a constant has no immediate encoding at all and the
  register form is not a fallback but the only correct answer — and it is off
  under a MIR mode, where a tiled node is emitted by the selector and a fold
  here would leave the byte-equality lane comparing two different programs.

## 5. Measured: instructions and bytes

Release binary, the two flags as the A/B, first optimizing-tier compile of each
method, instructions / bytes:

| probe | both off | cmp only | lea only | **both on** | forms that fired |
|---|---:|---:|---:|---:|---|
| `CmpImm.wide` | 186 / 923 | 184 / 919 | 185 / 918 | **183 / 914** | `cmp_imm=1+0 add_lea=0+1` |
| `CmpImm.down` | 186 / 919 | 184 / 912 | 185 / 914 | **183 / 907** | `cmp_imm=1+0 add_lea=0+1` |
| `LoopCtl.spin` | 189 / 933 | 187 / 927 | 188 / 928 | **186 / 922** | `cmp_in_place=0+1 add_lea=0+1` |
| `PollReach.hotLoop` | 200 / 1190 | 198 / 1185 | 198 / 1183 | **196 / 1178** | `cmp_in_place=1+0 add_lea=1+0` |
| `PollReach.wideLoop` | 268 / 1637 | 266 / 1631 | 267 / 1632 | **265 / 1626** | `cmp_in_place=0+1 add_lea=0+1` |

The four arms are additive to the instruction, which is what says the two levers
are disjoint rather than two spellings of one effect. Bytes fall in every arm —
worth contrasting with the operand-pairing pass in the sibling page, which
bought its one instruction for three extra bytes and was rejected for it.

All four compare forms engage somewhere: register-immediate on `CmpImm.wide`,
frame-immediate on `CmpImmProbe` (`cmp_imm=0+2` on one compile) and on
`CmpImm.tight` (`1+1`), register-register on `PollReach.hotLoop`,
register-frame on `LoopCtl.spin`.

The original page's numbers for the two shipped forms were 160→158 instructions
on `PollReach.hotLoop` and 190→188 on `LoopCtl.spin`. Those are not comparable
to the table above: `dev` moved between the two sessions (the carry work landed)
and both bodies grew. `LoopCtl.spin`'s **byte** count at the off arm is 933 in
both sessions, which is the check that the off arm is still the same off arm.

## 6. Residual one, done: the immediate form

`i < 100` used to load the constant into RCX first. It no longer does. Two
encoders, both free functions so their bytes can be tested without standing up a
`Lowerer`, and both worth testing for the same reason `cmp_reg_reg_bytes` was:

**the `/digit` is a silent failure.** `/7` is CMP, but `/0` in the same two
opcodes is ADD and `/5` is SUB, and both of those WRITE the register. A
transposed digit would not fault — it would increment the loop counter
underneath the comparison and produce a plausible wrong branch.

* `cmp_reg_imm_encodes_the_known_forms` — `CMP EBX,100` = `83 FB 64`,
  `CMP EAX,0` = `83 F8 00` (no REX), `CMP RBX,100` = `48 83 FB 64`,
  `CMP R12D,100` = `41 83 FC 64` (REX.**B**, because the sole operand is the
  `r/m` and `/7` occupies `reg`, so REX.R is never set), `CMP EAX,4096` =
  `81 F8 00 10 00 00`, and the `imm8` boundary pinned at both −128 and −129.
* `cmp_frame_imm_encodes_the_known_forms` — `CMP DWORD [RBP-60h],100` =
  `83 7D A0 64`, its REX.W twin, and the `disp32` form for a slot out of `disp8`
  reach.

## 7. Residual two, done: the `LEA` — and the shape it was actually in

The page asked for this:

```asm
mov rax,rbx        ; i -> RAX
add eax,1
mov r14,rax        ; i+1 -> r14
```

→ `lea r14d,[rbx+1]`. That form is built and it fires — `add_lea=1+0` on
`PollReach.hotLoop`, worth two instructions and seven bytes there.

**But the first version of it engaged NOWHERE on the probe written for it**, and
the reason is the useful part of this section. `CmpImm.wide`'s census:

```text
[ir-ls] homes: dropped_values=7 stores_skipped=10 def_publishes=0
[ir-ls] phi copies: reg_reads=0 reg_publishes=10
```

`def_publishes=0`. In a counted loop the increment is not itself given a
register — the PHI is, and `emit_phi_copies` publishes it on the back edge. So
`i + 1` writes a home word and `assigned_gpr` answers `None`, and a `LEA` that
requires a destination register has nothing to write into. The emitted shape was
two instructions, not three:

```asm
mov rax,rbx        ; i -> RAX
add eax,1
mov [rbp-0B0h],rax ; its home; the phi copy reloads it at the back edge
```

So there are two forms, and the weaker one is the common case:

* **`AddForm::Done`** — the result has a register. `lea r14d,[rbx+1]`, and the
  arm owes nothing further.
* **`AddForm::InRax`** — it does not. `lea eax,[rbx+1]`, and `store_rax`
  finishes exactly as it would have, because RAX holds precisely what
  `mov rax,x; add eax,k` would have left there. One instruction out of two, and
  nothing else about the arm changes.

`x - k` reaches the same encoder with the constant negated, except at
`Integer.MIN_VALUE`, whose negation is not an `int`. One constant in the
language, and it declines rather than wrapping into a silent `+ MIN`.

**The safety argument that had to be extended.** Unlike the compare, this
DEFINES a value. `every_droppable_op_writes_its_home_once_through_store_rax`
exists because `op_home_is_one_store_rax` claims a property of SOURCE the
compiler cannot check — "the arm writes its home exactly once, through
`store_rax`, with RAX holding the value" — and it is the whole safety argument
for `CRATONVM_JIT_IR_DROP_HOME`. `AddForm::Done` writes a home word that
`publish_def_at_store` never sees, so:

* the arms were restructured so `self.store_rax(slot);` still appears exactly
  once in each, and the test's original assertion stands unchanged;
* the test now NAMES `emit_add_lea` as the one certified exception, and refuses
  a new arm that reaches for it;
* `the_lea_add_form_publishes_what_it_does_not_store` certifies it, against
  source: the direct form must contain `mark_gp_reg_live`, `cur_def_published =
  true` and `store_abi_reg`, the publish must PRECEDE the branch that decides
  whether to skip the home store, and the accumulator form must NOT publish
  (its value is in RAX and `store_rax` is about to publish it from there).

Two encoding traps in `lea_reg_base_disp_bytes`, both silent rather than
faulting, and both live rather than theoretical because **R12 and R13 are in
this backend's GP file**:

* a base whose low three bits are `100` (RSP, R12) means "SIB follows", so it
  needs a `24` SIB byte naming itself with no index;
* a base whose low three bits are `101` (RBP, R13) has no `mod=00` form — that
  encoding is RIP-relative. The encoder always emits a displacement, `disp8` of
  zero if that is what it takes.

`lea_base_disp_encodes_the_known_forms` pins both, plus the `imm8` boundary;
`lea_puts_the_destination_in_reg_and_the_base_in_rm` pins the transposition
separately, because `LEA` assigns `reg` and `r/m` the OPPOSITE way round from
`cmp_reg_imm_bytes` and every vector in the first test would still pass if the
function and its expectations were transposed together.

## 8. The timing is a null, and this is what a null looks like

Three arms interleaved ABCCBA, A and C the SAME build, so the A-C spread is the
noise floor and nothing about the change:

| probe | rounds | floor (A vs C) | effect (B→A) |
|---|---:|---:|---:|
| `CmpImm.wide` | 12 | 1.43% median / 1.18% min | −2.62% median / +3.05% min |
| `LoopCtl.spin` | 12 | 5.69% median / 1.55% min | +3.19% median / +3.49% min |
| `CmpImm.wide` | 20 | **14.63%** median / 0.07% min | +9.35% median / **−5.28%** min |
| `LoopCtl.spin` | 20 | 3.11% median / 2.74% min | +22.22% median / +0.40% min |

Host load ran 40-111 on 8 cores across those runs. Two identical builds come out
14.63% apart; the measured "effect" ranges from −5.28% to +22.22%; and the
median and the minimum disagree about its SIGN on both probes. **That is a null,
not a small win.**

Adding rounds made it worse rather than better, which is worth recording because
the instinct is the opposite: between the 12-round and the 20-round runs the
host's load rose faster than the averaging could compensate, so the longer run
has the wider floor. On a shared box, more samples taken over a longer window is
not more signal.

`PollReach` was already known not to resolve this, for a second reason the
original page recorded and which still holds: its body is a serial recurrence on
`s`, so the comparison and the increment are off the critical path and execute
in parallel with it. Deleting instructions that are not on the dependence chain
cannot make a latency-bound loop faster. `LoopCtl` and `CmpImm` were written
with four independent accumulators for exactly that reason — and they did not
resolve it either, on this host, on this day.

## 9. Why both ship anyway, default ON

**The compare: no mechanism by which it loses.** It strictly removes
instructions and bytes from every qualifying compare. `cmp ebx,[rbp-60h]` is one
micro-fused load-and-compare uop against three instructions and two uops;
`cmp ebx,64h` removes a `mov ecx,imm`, which is a real uop that no renamer
eliminates. There is no port or latency story on the other side.

**The `LEA`: there IS an argument against it, and it is written down rather than
argued away.** `mov rax,rbx` is eliminated at rename on every current x86-64, so
the instruction the accumulator form removes was very likely already free. On
Intel, `LEA` also issues on fewer ports than `ADD` (1 and 5, against 0/1/5/6),
so in a port-1/5-bound loop it could in principle cost a cycle it does not
spend. **Not on this host** — an AMD EPYC 9V45 (Zen 5), where the simple
base-plus-displacement form runs on all four ALUs — but the flag is not
host-specific and the next machine may be.

So the honest summary of the `LEA` is: it removes an instruction and five bytes
but probably not a uop. It ships ON for the decode and I-cache saving, which is
not in doubt, and `CRATONVM_JIT_IR_ADD_LEA=0` is both the kill switch and the
A/B arm.

What cannot be claimed from this host is a speedup, so none is claimed. A
quieter box, or a compare-dense workload, is what would price either of them.

## 10. Verified

* **2366** `cratonvm-jit` unit tests, **2645** `cratonvm-vm` unit tests, both in
  debug so `debug_assert` is live; **145** `ir_vs_singlepass` differential tests.
* Regression suite **92/92 with the new defaults and 92/92 with both kill
  switches** — both directions, because a switch nobody exercises is not a
  switch.
* `probes/CmpImmProbe.java` agrees with HotSpot to the checksum
  (`CMPIMM ck=-428570987440006`, n=200000) under the defaults, under each kill
  switch alone, under both, under `CRATONVM_JIT_IR_ALU_IMM=0`, under
  `CRATONVM_JIT_IR_LINEAR_SCAN=0`, and — at n=20000, where HotSpot gives
  `-41855999783126` — under `--nojit`. It covers the `imm8`/`imm32` boundary in
  both signs, negative bounds, a `long` constant outside `i32` that no immediate
  can express, `Integer.MIN_VALUE` as a bound and as an addend, a first operand
  forced out of its register by pressure, and a reference against `null`.
* Four new encoder tests and one new source-property test, described in §6 and
  §7.

## 11. What is left in this residue, and it is not this page's

The compare and the increment are done. Two things next to them were seen and
deliberately not taken:

* **The first operand never folds.** `1 + i` and `100 > i` go to the
  accumulator path. `Op::Add` is commutative and nothing canonicalises it; a
  compare would additionally need its condition inverted. Both are missed folds,
  never wrong ones, and javac emits neither order for a counted loop. A
  canonicalisation pass in `ir_optimize` is the right place for it, not four
  more cases in two emitters.
* **`i + 1` gets no register because the phi has it.** §7's census is the
  finding, and the `LEA` works around it rather than fixing it. Whether the
  increment and its phi should share one register — making the back-edge copy
  disappear entirely rather than shortening what feeds it — is a residency
  question, and belongs with
  `c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`.

New pages if anyone wants them. Nothing here is blocked on either.
