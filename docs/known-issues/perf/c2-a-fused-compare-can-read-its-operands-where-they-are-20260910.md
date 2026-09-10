# A fused compare can read its operands where they already are

**2026-09-10.** Every counted loop's back edge spent three instructions on its
comparison when one was enough. That is fixed and shipping. **The timing does
not resolve it on this host, and this page says so rather than quoting a
speedup.**

Found by re-reading the same loop as
`c2-one-carry-slot-is-the-frame-traffic-ceiling-20260910.md` after the carry work
landed: when the memory traffic went away, what was left at the top of the
residue was the loop control.

## What it emitted

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

## Why a fused compare in particular

Because it owes nothing else. Every other arm that loads operands into RAX/RCX
also has to put a result somewhere: a home word, a resident register, a carry.
A fused compare defines no value, writes no home and publishes no register — the
only thing that outlives it is the flags, and those are identical whichever
registers the comparison names.

Nothing between it and the `Jcc` touches flags, and nothing downstream may
assume RAX holds the first operand: the non-fused path already overwrites AL
with `SETcc` on the phi-copy layout, so no reader could ever have relied on it.

## The two forms

| operands | encoding | emitted |
|---|---|---|
| both resident | `39 /r` (`CMP r/m, r`) | `cmp ebx,r12d` |
| first resident, second in its slot | `3B /r` (`CMP r, r/m`) | `cmp ebx,[rbp-60h]` |

The second is the more valuable one and is not a fallback: `peak_live` routinely
exceeds the five-register GP file, and a loop bound is exactly the long-lived
value that loses its register. Three instructions become one.

The 32-bit form reads four bytes where the `MOV` it replaces read eight. That is
the same comparison — the slot holds a sign-extended `int` in its low word, and
`CMP EAX, ECX` only ever looked at those four bytes either.

Two guards: `carry_names()` declines any value a carry is holding (that has to be
read through `gp_load_value` or the carry strands), and the frame form goes
through `slot_of_checked`, so a dropped home declines this form rather than
latching a bailout on a path with a perfectly good fallback.

## Measured: instructions yes, time no

| probe | arm | instrs | bytes | census |
|---|---|---:|---:|---|
| `PollReach.hotLoop` | off | 160 | 766 | `cmp_in_place=0+0` |
| | **on** | **158** | **761** | `cmp_in_place=1+0` |
| `LoopCtl.spin` | off | 190 | 933 | `cmp_in_place=0+0` |
| | **on** | **188** | **927** | `cmp_in_place=0+1` |

Both forms engage, one per probe, and the results are unchanged.

Timing, `LoopCtl` (four independent accumulators, so it is throughput-bound
rather than latency-bound), release binary, one flag
(`CRATONVM_JIT_IR_CMP_IN_PLACE`), **three arms** interleaved ABCCBA x6 where A
and C are the SAME build:

| arm | median ns/iter | min | max |
|---|---:|---:|---:|
| A — on | 1.4112 | 1.2967 | 1.5905 |
| C — control, identical to A | 1.3571 | 1.2549 | 1.6610 |
| B — off | 1.4104 | 1.2657 | 1.5662 |

**Noise floor 3.99%** (A against C, identical binaries). **Effect 1.72%.** The
effect is inside the floor, and A and B are within 0.06% of each other — the
apparent gap comes entirely from C drawing a fast sample. This is a null result,
not a small win.

`PollReach` is the same story for a second reason worth writing down: its body
is a serial recurrence on `s`, so the comparison and the increment are already
off the critical path and execute in parallel with it. Deleting instructions
that are not on the dependence chain cannot make a latency-bound loop faster.
That is why `LoopCtl` was written — and it did not resolve either.

## Why it ships anyway, default ON

It is not a wash by construction, which is the test that sank the scheduling
pass in the sibling page. It strictly removes instructions and bytes from every
qualifying compare, and there is no mechanism by which it loses: `cmp
ebx,[rbp-60h]` is one micro-fused load-and-compare uop against three
instructions and two uops for the sequence it replaces.

What cannot be claimed from this host is a speedup, so none is claimed. A
quieter box — the noise floor here was 6% earlier in the same session and 4% by
the end — or a compare-dense workload would be needed to price it.
`CRATONVM_JIT_IR_CMP_IN_PLACE=0` is the kill switch and the A/B.

## Guarded by

`cmp_reg_reg_bytes` is a free function so its encoding can be tested against
hand-checked vectors without standing up a `Lowerer`, and it is worth testing:
a swapped ModRM field compares two registers that both exist, so the failure is
a plausible wrong branch and not a fault.

* `cmp_reg_reg_encodes_the_known_forms` — `CMP EAX,ECX` = `39 C8` (byte-identical
  to what it replaced), `CMP RAX,RCX` = `48 39 C8`, `CMP EBX,R12D` = `44 39 E3`
  (REX.R extends the SECOND operand), `CMP R13,RBX` = `49 39 DD` (REX.B extends
  the FIRST), and both-extended `4D 39 FE`.
* `cmp_reg_reg_puts_the_first_operand_in_rm` — a separate test, because the one
  above would still pass if the function and its vectors were transposed
  together. `CMP ECX,EAX` must be `39 C1`, not `39 C8`.

Green: 2345 jit unit tests, 145 `ir_vs_singlepass` differential tests, 2641 vm
unit tests.

## Next, in the same residue

```asm
mov rax,rbx        ; i -> RAX
add eax,1
mov r14,rax        ; i+1 -> r14
```

Three instructions for `i + 1` with both the source and the destination
resident; `lea r14d,[rbx+1]` is one. That is `Op::Add` with a constant operand
and a resident result home, so unlike the compare it touches result publishing
and not just flags — a bigger change, and one to price separately.

The compare's own unfinished half is the immediate form: `i < 100` still loads
the constant into RCX first, where `cmp ebx, 100` (`83 /7 ib`) would do. Cheap,
and common in Java.
