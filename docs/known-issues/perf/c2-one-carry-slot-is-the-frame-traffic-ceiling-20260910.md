# The optimizing tier's frame traffic is one carry slot, not a scheduling order — FIXED 2026-09-10

**2026-09-10.** A counted loop whose entire live state is two `long`s and an
`int` spent **55 of 161 instructions** on `[rbp-*]` traffic, and two of its
three intermediate stores were never read back.

The obvious lever — scheduling a single-use operand next to the consumer that
reads it, so the existing carry can take it — was built, engaged, and **measured
a wash** (§4). It is a wash by construction: the emitter held ONE carry, so
pairing could only move the carry between operands, never add one.

The fix was therefore in the emitter and not the scheduler: a second carry slot,
with its own soundness proof, so a consumer can take both its operands in
registers (§6). **1.090x on the kernel, ranges non-overlapping.** The scheduling
pass ships with it because the two slots need the order it produces — but on its
own it does nothing, which is why §4 is still in this page rather than deleted.

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

## 6. FIXED — the emitter grew a second slot, and it is worth 1.090x

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

`probes`-style kernel `s += i ^ (s >>> 3)`, release binary, one flag
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

## 7. What is still open

`deferred=1/1` on this kernel: the mechanism fires once, because it needs the
consumer's FIRST operand to come from a width conversion. That covers mixed
`int`/`long` arithmetic and nothing else.

What is left of the `single_use=16` skip is the bigger half:

* **Promote single-use values into caller-saved scratch registers.** They cannot
  repay a callee-saved register's prologue save, which is why residency declines
  them — but a caller-saved register has no save to repay. This subsumes carries
  entirely rather than extending them. Larger blast radius: it changes register
  pressure and interacts with every existing residency invariant.
* **Widen `op_preserves_rcx` by making the arms deserve it.** Every binary arm
  loads its operand 1 into RCX. An arm that took a caller-supplied scratch
  register instead would be eligible, and that is a mechanical change to ~50
  arms rather than a design question.

Neither should be started from one kernel. The census wants running across the
probe set first — `deferred=N/M` is now the line that says how often the shape
occurs in real code.
