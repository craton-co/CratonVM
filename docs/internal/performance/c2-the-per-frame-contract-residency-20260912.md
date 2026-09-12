# The per-frame contract, lane 1: who actually pays for a home word

**2026-09-12.** [`c2-fib-per-call-budget-20260912.md`](c2-fib-per-call-budget-20260912.md)
§7 closes by naming the largest term in `fib`'s per-call budget and declining to
fix it:

> **The parameter round-trips through memory.** […] Every value has a home word
> and the home is written whether or not anything reads it. This is the single
> largest term in §2 and it is an architectural property of the lowerer, not a
> missing peephole.

This page takes that lane and reaches two results.

* **`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` — a win.** Single-use values that
  `plan_carries` provably cannot reach are refused a register by a rule that
  assumes the carry took them. Admitting exactly that complement is **7.6%
  faster on `probes/FieldLoop.java` over a 0.0% floor**, with a *smaller*
  optimizing body, and free (1.000x, inside a 1.1% floor) on `FibCall.fib`.
  Default OFF pending broader shapes; §6 says what it needs.
* **A retraction.** Admitting *constants* looked equally obvious, was built,
  compiled and passed 2 393 tests, and is **structurally incapable** of paying.
  §2 is why, and the reason was already written in the tree.

The two together give a cost model the current rule does not have (§5), and it
is the useful output of the page: a promoted value costs one save plus one
restore **per epilogue**, and repays it **per execution of the reload it
removes** — so residency pays in loops and washes on straight-line code.

Windows dev box under unrelated load from other worktrees; every timing is
interleaved with a CONTROL arm and reports its own floor. Checksums identical on
every row of every table.

## 1. Where the home words come from

`plan_register_residency` decides which IR values live in a register. It refuses
in two places that matter here, and the census names both:

```text
[ir-ls] skipped: … const=2 single_use=13 …
```

* **`const`** — `Op::Const` is refused unconditionally, before the pays-check
  runs at all.
* **`single_use`** — `ir_residency_pays_here` reduces to `static_uses >= 2` in a
  default build, so a value read exactly once never gets a register.

Both look like missed opportunities on a method whose budget is dominated by
home stores and reloads. Exactly one of them is.

## 2. The constant arm is right, and the reason is one flag away

**Retracted hypothesis.** The reasoning that produced it was: `fib` and
`FieldLoop` both show `const=2`, a constant read every iteration reaches its use
through memory, and a constant is the *safest* value in the graph to promote —
no inputs, no aliasing, and a deopt frame already names it as a literal
(`Op::Const(v) => FrameValue::Int(v)`). Supporting evidence looked strong:
lifting the residency refusals wholesale (`CRATONVM_JIT_IR_RESIDENCY_PAYS=0`) is
5.7% faster on `FieldLoop`, and the census attributed the whole of that to one
value going `const` 2 → 0.

It was built, gated on the loop-weighted pays-check so straight-line code would
be unaffected. Both halves of that prediction were wrong.

**On `FieldLoop` it is a no-op.** The constants are admitted and then handed
straight back:

| `CRATONVM_JIT_IR_CONST_RESIDENCY` | `const` | `resident` | `split_or_spilled` | `carried_reserved` | `FieldLoop.sum` |
|---|---:|---:|---:|---:|---:|
| 0 | 2 | 5 | 1 | 4 | **len=1635** |
| 1 | **0** | 5 | 2 | 3 | **len=1635** |

`resident` does not move and the emitted body is **byte-identical**. The
allocator spills one of them and the carry takes the other. The census line that
pointed at a constant was reporting a value that was never going to reach a
register by this route.

**On `fib` it is measurably slower.** The prediction was byte-identity, because
the loop-weighted form reduces to `static_uses >= 2` at depth 0. But `fib`'s
constant `1` is read twice — by `n-1` and by the `n <= 1` test — so it clears
that bar, is admitted, and the body **grows 788 → 827 bytes**:

```asm
mov [rbp-0B0h],r12      ; NEW: a callee-saved register saved in the prologue
mov r12,[rbp-48h]       ; the constant LOADED FROM ITS HOME WORD, not materialised
mov r12,[rbp-0B0h]      ; NEW: and restored
```

```text
A (flag off)  median 107.0 ms   C (control) median 107.0 ms
B (flag on)   median 109.0 ms   n=26
noise floor 0.0%   effect +1.9%   ratio 1.019x
VERDICT: flag ON is SLOWER — above the floor
```

**The premise was wrong, and one existing flag says so.**
`CRATONVM_JIT_IR_CONST_IMM` is **default ON**: a constant is already read as an
immediate rather than from its home word. So a constant's baseline cost is
**zero memory operations**, and residency cannot improve on zero — it can only
add the register's own prologue save and restore, which is what the disassembly
above shows it doing. The `const=` skip line is not a missed opportunity; it is
the correct answer, and the `const` arm has been correct since it was written.

**The tree already said so, in the one place that had to get it right.**
`ir_reserve_carried_enabled`'s candidate filter — the *other* pass that hands
out registers from this file — excludes constants with the reason spelled out:

```rust
// A constant is materialised as an immediate by every
// reader, so its register could never be read.
&& !matches!(node.op, Op::Const(_))
```

That comment is the whole of §2, written before the experiment and by the
author of the competing pass. A grep for `Op::Const(_)` across the two
register-handout sites would have retired the hypothesis before it was built,
and that is the cheap check this page exists to recommend.

The transform is **reverted**. What survives is this section and the rule under
it: **before promoting a value, price what it costs today, not what a home word
usually costs.** A value with a rematerialisation cheaper than a load is already
free.

## 3. The single-use arm IS wrong, and the carry window says exactly where

The `single_use` line is the larger population — 13 of 20 nodes on `fib`, 15 of
23 on `FieldLoop` — and unlike the constants these values genuinely reach their
use through memory. The refusal that produces them is `static_uses >= 2`, and
the argument for it is sound *where it applies*: a single-use value costs one
publish and saves one reload, a wash — **and `Lowerer::plan_carries` already
serves those for free**, by handing the producer's RAX straight to the consumer.

But the carry scans a window exactly one node wide:

```rust
for w in 1..block.nodes.len() {
    let prod = block.nodes[w - 1];
    let cons = block.nodes[w];
```

so it reaches a single-use value only when the consumer is the very next node in
the same block. Every other single-use value gets neither mechanism: not the
carry, because the consumer is not adjacent; not residency, because the rule
that refuses it assumes the carry took it.

`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1` admits exactly the complement of that
window, so the two partition the population rather than competing for it —
`plan_carries` already declines a value residency took
(`assigned_gpr(prod).is_some()`, `carry_skips[3]`), and
`the_carry_window_is_exactly_the_next_node_in_the_same_block` pins the two
definitions of the window against each other.

**On the loop it pays: 7.6% over a 0.0% floor.** Thirteen interleaved rounds,
control arm, `probe.reps=3000 probe.n=20000`:

```text
A (flag off)  median  99.0 ms    C (control) median 99.0 ms
B (flag on)   median  91.5 ms    n=26
noise floor 0.0%   effect -7.6%   ratio 0.924x
VERDICT: flag ON is FASTER — above the floor
```

and the optimizing body gets **smaller**, 1052 → 1039 bytes.

**The reason is one value, and it is the induction variable.** Normalising
addresses and diffing the two `full/ir` bodies leaves this, inside the loop:

```asm
; flag OFF                            ; flag ON
mov r14,rax
lea eax,[r15+1]                       lea r14d,[r15+1]
mov [rbp-90h],rax   ; store i+1  →    (gone)
cmp r15d,r12d                         cmp r15d,[rbp-60h]
…                                     …
mov rbx,r14                           mov rbx,r12
mov r15,[rbp-90h]   ; reload i+1 →    mov r15,r14
```

`i + 1` was computed, stored to its home word, and reloaded — **twenty thousand
times per call** — because it is read exactly once, by the phi at the loop back
edge, and that read is not the adjacent node. This is the budget page's "the
parameter round-trips through memory", in a loop, removed.

## 4. On `fib` it is free, and the reason is the epilogue count

Same flag, same binary, the shape the budget page opened up:

```text
A (flag off)  median 179.0 ms   C (control) median 177.0 ms
B (flag on)   median 178.0 ms   n=26
noise floor 1.1%   effect +0.0%   ratio 1.000x
VERDICT: UNMEASURABLE (0.0% effect inside a 1.1% floor)
```

It engages hard — `single_use` 13 → 6, `resident` 2 → 6 — and the body grows
**788 → 883 bytes**. The full memory-operation accounting, counted off the two
disassemblies rather than estimated:

| | stores to `[rbp]` | loads from `[rbp]` | epilogues |
|---|---:|---:|---:|
| flag off | 19 | 21 | 4 |
| flag on | **19** | **30** | 4 |

* **Stores are a wash.** Three home stores genuinely disappear (`-78h`, `-68h`,
  `-50h` — home-dropping works), and three callee-saved *saves* replace them
  (`mov [rbp-0B8h],r13`, `r14`, `r15`).
* **Loads go up by nine.** The three reloads the transform set out to remove do
  disappear, and are paid for with twelve epilogue restores — three registers
  across four exits.

Fifteen extra memory operations for three removed, and it costs **nothing
measurable**, because all fifteen are on the prologue and the four exit paths:
once per call, never in a loop, and `fib` has no loop. The transform is neutral
here rather than harmful, which is the second-best outcome and worth having.

## 5. The rule this gives, and why the register file makes it that shape

**The residency register file is entirely callee-saved.** Five registers by
default — RBX, R12–R15 — and `plan_register_residency` says so itself: *"every
register in it is callee-saved and no fixed x86 operand names one"*. So a
promoted value takes a register that must be saved on entry and **restored on
every exit**. `fib` has four epilogues, each restoring the whole set:

```asm
mov rbx,[rbp-0A8h] ; mov r13,[rbp-0B8h] ; mov r14,[rbp-0C0h] ; mov r15,[rbp-0C8h]
add rsp,200h ; pop rbp ; ret          ; … and again, ×4
```

Put the two measurements together and the trade has a shape:

> **cost** = one save plus `N_epilogues` restores, paid **once per call**.
> **benefit** = reloads removed, times **how often they execute**.

The `static_uses >= 2` rule sees neither side. It counts static graph edges, so
it cannot see the 20 000 executions of `FieldLoop`'s back edge, and it cannot
see `fib`'s four exits either. That is why lifting it wholesale
(`CRATONVM_JIT_IR_RESIDENCY_PAYS=0`) produced opposite signs on the two shapes —
+4.8% on `fib`, −5.7% on `FieldLoop` — and why restricting the lift to the
carry's complement gets the loop win **without** the `fib` loss.

`FieldLoop` is the confirming case for the cost side too: its optimizing body has
**two** epilogues and already saves all five registers in *both* arms, so
crossblock adds no new save at all and the cost term is zero.

| | epilogues | new callee-saved saves | body | verdict |
|---|---:|---:|---:|---|
| `FieldLoop.sum` | 2 | **0** | 1052 → 1039 | **0.924x — faster** |
| `FibCall.fib` | 4 | 3 | 788 → 883 | 1.000x — free |

## 6. Status, and the obvious next step

`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` ships **default OFF**: one win and one
neutral on two probes is evidence for the mechanism, not yet evidence for a
default. What it needs before flipping is the regression suite and a
`CratonBenchC2` checksum-parity run, and a third and fourth shape — in
particular a loop-free method with *more* than four epilogues, which is where §5
predicts it should finally lose.

The next step §5 hands over is sharper than "try it on more shapes", though.
The cost side is one save plus `N_epilogues` restores and the benefit side is
loop-weighted, and **both quantities are already computed in this file**:
`live.weight` is the loop-weighted use count `residency_pays_loop_weighted`
reads, and the epilogue count is a property of the schedule. A rule of the form

```text
admit when  live.weight[id]  >  1 + epilogue_count
```

subsumes `static_uses >= 2`, the crossblock arm, and
`CRATONVM_JIT_IR_LS_LOOP_WEIGHT` in one predicate that is actually counting the
thing that decides the answer.

## 7. What this says about the `fib` lane

§4 is a negative result on `fib` and it belongs with the budget page's other
two: the precise-maps mirror bought nothing, the equal-depth sink was 2.4%
slower, and admitting six values to registers instead of two is free. Four
hypotheses measured on that method now, and the only movement came from a
*different* method's loop.

That is consistent with the budget page's conclusion rather than a challenge to
it: `fib` is 3.7x because a CratonVM call is ~36 instructions to HotSpot's ~10,
and every term in that sum is small. §3's win is the shape of what does work —
it did not remove a term from the per-call contract, it removed a memory
round-trip from something executed twenty thousand times inside one call.

The one genuinely encouraging number for the per-call budget is still in §4's
table: **three home stores disappeared.** `value_home_droppable` /
`op_home_is_one_store_rax` already exist and already fire, gated on
`deopt_nameable[id]`, which requires `holders[reg] == 1` — exclusive register
ownership across the whole method. Widening what can satisfy that is the lane
that attacks the per-frame contract directly.

## 8. Reproducing

```bash
javac -d probes/out probes/FibCall.java probes/FieldLoop.java
EXE=./target/release/cratonvm

# the census, and the optimizing body (NOT the osr/sp one at these settings)
CRATONVM_DBG_IR_LINEAR_SCAN=1 $EXE -cp probes/out -Dprobe.n=26 -Dprobe.reps=1 FibCall
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FieldLoop.sum \
  $EXE -cp probes/out -Dprobe.reps=3000 -Dprobe.n=20000 FieldLoop | grep full/ir

bash tools/tier-ab/flag-ab.sh -Exe $EXE -Cp probes/out -Class FieldLoop \
  -Flag CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK -On 1 -Off 0 \
  -D probe.reps=3000 -D probe.n=20000 -Rounds 13
```
