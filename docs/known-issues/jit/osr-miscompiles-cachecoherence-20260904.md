# OSR: a strided loop's stored VALUE is computed once and reused

**Status:** PARTIALLY FIXED 2026-09-04. One of two causes is closed; the
original `cacheCoherence` reproducer still diverges, with a *different*
wrong value than before.
**Reproducer:** `test_classes/jit/OsrStridedValue.java` (self-contained,
no GPU, no `--gpu`).
**Found:** 2026-09-04, chasing what looked like a GPU offload defect. It
was not one.

## The defect, in one line

In an OSR-compiled body, a strided loop whose stored value is an
expression over the induction variable emits that expression **once**;
subsequent iterations advance the index and re-read the value from the
frame slot the first iteration wrote.

## Repro

```bash
javac -d test_classes/jit test_classes/jit/OsrStridedValue.java
cratonvm --java-home <jdk25> -cp test_classes/jit --nojit OsrStridedValue 65536
cratonvm --java-home <jdk25> -cp test_classes/jit         OsrStridedValue 65536
```

```java
for (int round = 0; round < 12; round++) {
    scale(in, out);                       // a call is REQUIRED to trigger it
    h = mix(h, sum(out));
    for (int i = round; i < n; i += 1024) {
        justRound[i]  = round;            // correct
        iPlusRound[i] = i + round;        // WRONG
        in[i]         = i ^ round;        // WRONG
    }
}
```

Sampling the addresses the last round (`round == 11`) wrote:

```
            addresses 11, 1035, 2059, 3083, 4107, 5131
--nojit   round    11 11 11 11 11 11        <- correct
          i+round  22 1046 2070 3094 4118 5142
          i^round  0 1024 2048 3072 4096 5120
jit       round    11 11 11 11 11 11        <- correct
          i+round  22 22 22 22 22 22        <- frozen at i = 11
          i^round  0 0 0 0 0 0              <- frozen at i = 11
```

The **addresses are right** and the **values are frozen at the first
iteration's `i`**. `round` is right because it genuinely is
loop-invariant.

## In the emitted code

`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=OsrStridedValue.body`.
`r12` is `i`, `r13` is `round`:

```
138d  mov  rax, r13              ; round
139a  mov  rax, [rbp-80h]        ; the value expression's `i` operand
139e  xor  eax, ecx              ;   i ^ round
13a3  mov  [rbp-80h], rax        ; -> slot
13a7  mov  [rbp-78h], rax        ; -> slot
      ... guard ...
13cc  mov  rdx, r13              ; justRound[i] = round      RECOMPUTED
13d3  mov  rax, [rbp-70h]        ; iPlusRound value FROM SLOT
1401  mov  rax, [rbp-78h]        ; in value FROM SLOT
142f  add  r12d, 400h            ; i += 1024
      ... guard ...
145a  mov  rdx, r13              ; justRound[i] = round      RECOMPUTED
1461  mov  rax, [rbp-70h]        ; iPlusRound value FROM SLOT  <-- stale
148f  mov  rax, [rbp-78h]        ; in value FROM SLOT          <-- stale
14bd  add  r12d, 400h
```

### The back edge names the bug

```
16bc  jmp 0x...13AB
```

The loop's back edge targets **`13ab`** — the guard — which is *after*
the value computation at `138d`. So this is **not** an unroll that
dropped a copy: the value expressions are emitted **above the loop's
back-edge target**, i.e. outside the loop, and the body from `13ab`
onward never recomputes them. Every iteration replays the slots the
pre-loop code wrote once.

That makes it a **label placement** problem: the back-edge target for
this loop is bound past the first expression of the loop body, so the
first body expression is executed exactly once, on fall-through.

Note also `139a`/`13a3`: the `xor` reads and writes the *same* slot, so
that slot is not a stable home for `i` either.

`iPlusRound`'s slot (`-70h`) is never written inside the loop at all —
its value is produced before the loop and only read within it.

## What it is NOT — every one of these was tested and refuted

| suspect | switch | result |
|---|---|---|
| GPU offload / the residency cache | `--gpu` absent entirely | still wrong |
| the compiled-tier array barrier | `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` | still wrong |
| IR loop-invariant code motion | `CRATONVM_JIT_LICM=0` | still wrong |
| IR loop unrolling | `CRATONVM_JIT_UNROLL=0` | still wrong |
| IR affine strength reduction | `CRATONVM_JIT_REASSOC=0` | still wrong |
| OSR frame-slot seeding | `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS=0/1` | still wrong |
| OSR dead-local masking | `CRATONVM_JIT_OSR_DEAD_LOCALS=0`, `..._DEAD_MASK_BLANKET=0` | still wrong |
| OSR single-pc binding | `CRATONVM_JIT_OSR_SINGLE_PC=1` | still wrong |
| **OSR itself** | `CRATONVM_JIT_OSR=0` | **correct** |
| **that one method** | `CRATONVM_JIT_DENY=...body` | **correct** |

No IR-tier optimisation switch moves it, and the emitted idiom
(`add r12d,400h`, simulated-stack spills through `[rbp-0B0h]`) is the
single-pass backend's. So this is the **single-pass OSR** path.

Start at whatever binds the back-edge target for a counted loop in
`jit/src/x64/bytecode_walk.rs` under an OSR compile, and ask why it
lands one expression late. The three-way trigger (a call in the OUTER
body, stride > 1, a value expression over the IV) most likely selects
which pc the loop head is recorded at.

## Minimal trigger

Bisected from `GpuRuntimeStress.cacheCoherence` by deletion. All three
are required:

* a **call** in the outer loop body (`scale(in, out)`) — remove it and
  the answer is correct;
* a **stride greater than 1** — `i += 1` is correct, `i += 1024` is not;
* a stored value that is an **expression over the induction variable** —
  `in[i] = 5` is correct, `in[i] = i` is correct, `in[i] = i + round` and
  `in[i] = i ^ round` are not.

Neither the loop's start value (`i = round` vs `i = 0` vs `i = h & 7`)
nor its bound (`n` vs a literal) matters; both were tested.

## Why no suite caught it

`bench-gpu/runtime-stress.sh` ran three arms and **none of them compiled
the method**: HotSpot, `cratonvm --nojit` (interpreted by construction),
and `cratonvm --gpu`, where `runtime::offload_jit_gate` refuses to
compile every scenario in that file — they all write a primitive array
or call an offload-eligible kernel, which is both of that gate's
reasons.

A fourth **compiled CPU arm** was added on 2026-09-04 and is what now
reports this. The general lesson is worth more than the defect: a gate
that keeps methods interpreted removes them from every differential
suite that reaches the compiled tier only through that gate.


## Cause 1 — FIXED: OSR entry with a live expression stack

`jit/src/x64/bytecode_walk.rs` marks a pc OSR-ineligible for several
reasons (hoisted-loop interiors, synthetic guards, handler-only pcs).
It did not require the **abstract operand stack to be empty**.

Entering part-way through an expression means the prologue materialises
the pending operands — correctly, for the entering iteration. What it
cannot do is make the LOOP recompute them: the back edge targets the
header, the operand pushes live *above* the entry point, and every later
iteration replays the slots the prologue filled once.

`operand_stack_live` now joins that rejection set. HotSpot has the same
rule. It costs nothing in practice — javac gives every loop header an
empty expression stack, so the newly-refused pcs are mid-expression ones
the interpreter reaches again a few bytecodes later at the header.

**Evidence it is the right rule:** the minimal reproducer is fixed.

    Min.body, a[i] = i + r      before  11 11 11 11 11 11
                                after   11 1035 2059 3083 4107 5131

Regression suite 89/90 with the rule in — the one red is
`RJitLambdaNpeSupersede`, which a pristine dev binary fails identically
(landed by `4b6437eaf wip:`).

## Cause 2 — STILL OPEN

`OsrStridedValue` / `GpuRuntimeStress.cacheCoherence` still diverge, and
`i+round` is still frozen at the entering iteration's `i`:

    OsrStridedValue  i+round  22 22 22 22 22 22     (unchanged)
    OsrCoh           8859114794901677457 expected
                     6931349745872807313 before the rule
                     3454959152174638481 after it

The value CHANGED, which is itself information: the rule moved which pc
OSR enters at, and the loop is still wrong from the new one. So there is
a second way for the loop body's value computation to end up above the
back-edge target that does not involve a live expression stack at the
entry pc.

### Minimised: `test_classes/jit/OsrStridedValueMin.java`

Two nested loops and **no calls at all**:

```java
for (int i = 0; i < n; i++) a[i] = 0;      // hot -> OSR
for (int r = 0; r < 12; r++)
    for (int i = 0; i < n; i += 1024) a[i] = i + r;
```

    --nojit   11 1035 2059 3083 4107
    jit       11   11   11   11   11

Three hypotheses tested and refuted while minimising:

* **"three stores in one body"** — no. A three-store version
  (`a[i]=r; b[i]=i+r; c[i]=i^r;`) with `i = 0` and a trivial call is
  **correct**.
* **"the inner loop starts at the outer IV"** — no. `i = 0` and
  `i = r` both fail, sampled at the indices each actually writes. (An
  earlier `i = 0` arm reported a vacuous "same" because it sampled
  `11 + k*1024`, which that loop never touches.)
* **"a call in the outer body is required"** — no longer. It was, before
  cause 1 was fixed; the call-free shape fails now.

### What the code shows

`Min4.zeroStart`, inner loop bytecode `27: iload_3 … 44: goto 27`:

```
44e  xor eax,eax ; mov r12,rax      ; i = 0            (pc 25/26)
457  mov rax,r12 ; mov [rbp-30h],rax ; push i          (pc 34)
45e  mov rax,r13 ; mov [rbp-38h],rax ; push r          (pc 35)
468  mov rax,[rbp-30h] ; add eax,ecx ; i + r           (pc 36)
475  mov [rbp-28h],rax               ; -> value slot
479  cmp r12d,r15d ; jge exit        ; the test        (pc 29)
48a  mov rax,r14                     ; aload a         (pc 32)
4a6  mov [rax+rcx*4+10h],edx         ; iastore, value from the slot
4aa  add r12d,400h
6ca  jmp 479                         ; BACK EDGE
```

The emitted order is pcs **34, 35, 36, then 29, then 32** — the value
expression is emitted *before* the loop test and the array load, and the
loop body from `479` onward **contains no code for pcs 33–36 at all**.
The back edge re-enters at `479`, so those pcs never execute again.

So this is not "the entry pc had a live stack" (cause 1, fixed): the
compiled loop body is *missing* the value computation outright, and
would be wrong from any entry point. The operands appear once, in what
looks like an OSR-entry reconstruction of the expression stack for a
resume at bci 37 (`iastore`), and the loop body then reads the slots
that reconstruction filled.

The question for the fix: why does the walk emit pcs 34–36 ahead of pc
29, and why does the loop body it lays down afterwards skip them? Start
from whatever seeds the simulated operand stack for an OSR resume bci
and whether the subsequent walk treats those slots as already populated.

`CRATONVM_JIT_OSR=0` remains the workaround for both causes.
