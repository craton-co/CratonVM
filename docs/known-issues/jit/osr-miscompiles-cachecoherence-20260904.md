# OSR: a strided loop's stored VALUE is computed once and reused

**Status:** open, root-caused to the emitted code, not yet fixed.
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

The body is duplicated per back edge. The **first** copy computes the
value expressions into frame slots; the **second** advances `i` and
replays the stores from those slots without recomputing. Note also
`139a`/`13a3`: the `xor` reads and writes the *same* slot, so the slot is
not a stable home for `i` either.

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
