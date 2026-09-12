# `fib` at 3.7x: the per-call budget, and three hypotheses it kills

**2026-09-12.** `where-the-cpu-gap-actually-is-20260912.md` §4 ranks `fib` first
once the collector is struck from the list: the largest gap on any `CratonBench`
row that is **pure compilation** — no allocation, no collections, no collector
involvement, one arithmetic expression and two calls. 12,028 ms against
HotSpot's 3,281 ms.

This page is that row opened up. It contains **no transform**: it is an
instruction-level budget and three hypotheses measured and discarded, filed
because each one is the obvious thing to try and each one is wrong.

`probes/FibCall.java` is the phase alone in `flag-ab.sh`'s `acc=/ms=` format.
Windows dev box under unrelated load from other worktrees; every timing below is
interleaved with a control arm and reports its own floor.

## 1. The arithmetic that sizes it

`fib(44)` makes about `2·fib(45)` ≈ **2.3 billion** calls.

| | per call |
|---|---:|
| HotSpot | ~1.4 ns |
| CratonVM | ~5.3 ns |

**About 12 extra cycles per call**, and the body is four bytecodes of real work.
So the question is not "what is slow in the loop" — there is no loop. It is
"what does a call cost", and that is answerable by counting.

## 2. The count

`fib` is compiled by the optimizing pipeline (`bg-compile … tier=C1
optimized=true`, `[ir] admission … admitted to the optimizing pipeline`,
published at **788 bytes**). Its per-call cost is in two places.

**The prologue — ~16 instructions, ten of them stores:**

```asm
push rbp ; mov rbp,rsp ; sub rsp,200h          ; a 512-BYTE frame for one int local
mov [rbp-0A8h],rbx                             ; callee-saved save
mov [rbp-10h],rcx ; mov [rbp-8],rdx            ; context + the parameter, spilled
mov qword [rbp-20h],0  ; ×5                    ; the reserved bookkeeping tail
mov [gs:14F0h],rbp ; mov dword [gs:14F8h],1    ; the precise-maps frame mirror
jmp short +29h                                 ; over a 41-byte NOP sled
test byte [rel …],0FFh ; je                    ; entry safepoint poll
```

**Each call site — ~15 instructions:**

```asm
mov qword [rbp-18h],2                          ; safepoint-id store
mov rax,rsp ; and eax,0FFFFh ; cmp eax,210h ; ja   ; inline stack-depth guard
mov rcx,[rbp-10h] ; mov rdx,[rbp-50h]          ; context + arg, RELOADED from the frame
call fib                                       ; direct self-call (the good part)
mov [gs:14F0h],rbp ; mov dword [gs:14F8h],1    ; mirror republish
mov r10,8000000000000000h ; cmp rax,r10 ; jne  ; exception/deopt sentinel
mov rbx,rax ; mov [rbp-58h],rax                ; result, to a register AND its home
```

**~36 instructions per call against HotSpot's handful**, and **~15 of them are
stores**. At roughly one sustained store per cycle that is the twelve cycles §1
asks about, without needing any other explanation.

Note the shape of it: almost none of this is *arithmetic the optimizer could
improve*. It is the VM's own per-frame contract — the reserved tail, the mirror,
the safepoint id, the stack guard, the sentinel, and a home word for every
value. That is why the three hypotheses below all fail: they each remove one
term from a sum whose terms are all small.

## 3. Hypothesis 1 — the precise-maps mirror. **Wrong.**

The natural suspect: four mirror stores per call (`RBP` + identity on the
callee's entry, the same pair again in the caller's republish), and
`JIT_OPTIMIZATION.md` records that precise maps shipped default-OFF over "a ~6x
throughput tax on call-heavy code", flipped ON when re-measurement found the tax
gone *because more aggressive inlining leaves far fewer real call safepoints*.
`fib` is the method that cannot be inlined. The tax should still be here.

It is not. `CRATONVM_NO_PRECISE_JIT_MAPS=1 CRATONVM_NO_MOVING_YOUNG=1` — and the
arm is real, the body shrinks **788 → 725 bytes**, which is the check that says
the flag was not silently ignored by the `moving_young` interlock it warns
about:

| | run 1 | run 2 | run 3 |
|---|---:|---:|---:|
| default | 752 ms | 796 ms | 796 ms |
| no precise maps | 747 ms | 756 ms | 776 ms |

Inside the noise. **Sixty-three bytes and four stores per call buy nothing
measurable**, which also retires the idea that the store count alone is the
whole story — it is the store count *plus* everything else, and no single term
dominates.

There is still a provably redundant store in there, and it is worth writing down
even though §3 says it is not worth taking: **a self-recursive call's post-call
identity republish is a no-op.** The callee of a direct self-call *is this same
compilation*, so the `compile_id` it published on entry is the identical value
the caller is about to write back. Only `RBP` genuinely changes. Both tiers emit
it (`ir_lower::emit_post_call_frame_record`,
`x64::frames::emit_post_call_rbp_republish`). It is sound to elide and it is
measurably worth nothing.

## 4. Hypothesis 2 — schedule-late at equal depth. **Wrong, and backwards.**

`fib`'s entry block computes both recursive arguments *before* testing the base
case:

```asm
lea eax,[rbx-2] ; mov [rbp-68h],rax     ; n-2, computed and spilled …
lea eax,[rbx-1] ; mov [rbp-50h],rax     ; n-1, computed and spilled …
cmp ebx,1 ; jg .recurse                 ; … and only NOW the base-case test
```

Roughly half of all `fib` calls are base cases, so that is four instructions
executed on half the calls for nothing. This is *exactly* the shape
`CRATONVM_JIT_IR_SINK_EQUAL_DEPTH`
([`c2-schedule-late-at-equal-depth-20260912.md`](c2-schedule-late-at-equal-depth-20260912.md))
was built for, and it works: with the flag on, the entry block goes straight
from `mov rbx,rax` to `cmp ebx,1`, and both `lea`+store pairs are gone.

**And it is 2.4% SLOWER.** Fifteen interleaved rounds, control arm, `n=30`:

```text
A (flag off)  median 291.0 ms   C (control) median 285.0 ms
B (flag on)   median 295.0 ms
noise floor 2.1%   effect +2.4%   ratio 1.024x
VERDICT: flag ON is SLOWER — above the floor
```

The body grows **788 → 823 bytes**: what leaves the base-case path reappears,
larger, on the recursive path. So the flag now has three measurements —
1.000x on the partial-unroll probe, 0.974x on the field-read loop, **1.024x
here** — and they average to nothing. That page left it default-OFF for want of
evidence; this is the second and better reason to leave it there, and it is the
kind of result that only shows up when a flag is tried on a shape it was not
tuned on.

## 5. Hypothesis 3 — "C2's body is worse than C1's here". **Not measurable, because there is no C1 body.**

The tiering inversion is on record for a field-read loop, so the obvious next
question is whether `fib` is another instance. It cannot be asked with the
levers that look like they ask it:

| arm | emitted body |
|---|---|
| default | `len=788` |
| `CRATONVM_C2_SUPERSEDE=0` | `len=788` |
| `CRATONVM_C2_ACCEPT=never` | `len=788` |
| `CRATONVM_C2_ACCEPT=always` | `len=788` |

**`fib` never gets a single-pass body.** Its first compile is already the
optimizing pipeline (`tier=C1 optimized=true`), and the supersede line says
`c1=none c2=788` — there is no baseline body to supersede and none to compare
against. `CRATONVM_C2_SUPERSEDE` gates the *republish*, not the pipeline.

That matters for anyone reading a `CRATONVM_C2_SUPERSEDE=0` arm as "the C1
tier": on this method it is not. What that arm *does* measure is the supersede
machinery itself, and it is not free — **1.4% over a 0.0% floor** across
thirteen interleaved rounds, on a method where the republish is a first-publish
that changes no code. Small, but it is pure overhead on a workload that gains
nothing from it.

## 6. What the acceptance gate can and cannot see

The census prints `refused as a cost regression: bodies=0`. That line is easy to
read as "no body was a regression". It means something narrower, and the
estimator says so:

```rust
pub fn added_ns_per_execution(self) -> i64 {
    const BLIND_DISPATCH_NS: i64 = 175;
    const DIRECT_CALL_NS: i64 = 4;
    i64::from(self.blind_dispatches_in_splice) * (BLIND_DISPATCH_NS - DIRECT_CALL_NS)
        - i64::from(self.spliced_frames) * DIRECT_CALL_NS
}
```

**One term.** Blind dispatches introduced *inside spliced bodies*, credited
against the frames the splice removed. A body that inlined nothing and is simply
larger and slower than the alternative prices at exactly **0**, passes the gate,
and is published. The gate is the right idea and it is structurally blind to the
commonest way a body can be worse.

The cheapest second term is already printed beside it and already non-zero: the
supersede line carries `c1=` and `c2=` **byte counts**, and on `CratonBenchC2`
three of fourteen methods publish a LARGER C2 body than the C1 it replaced
(`c1=2004 c2=2443`, `c1=7718 c2=8813`, `c1=278 c2=334`). Size is a poor proxy
for speed in general — an unrolled body is bigger and faster — which is exactly
why this is a lane with a measurement in front of it rather than a patch: the
question is whether `c2_bytes > c1_bytes ∧ no size-increasing transform fired`
predicts a regression often enough to gate on. `Transform` bits already say
which transforms fired, so the conjunct is available.

## 7. What is actually left on `fib`

Sized off §2 rather than off intuition, and none of it is small work:

* **The parameter round-trips through memory.** The prologue stores `rdx` to
  `[rbp-8]`; the body's first act is `mov rax,[rbp-8]`. Every value has a home
  word and the home is written whether or not anything reads it. This is the
  single largest term in §2 and it is an architectural property of the lowerer,
  not a missing peephole.
* **A 512-byte frame and a five-store reserved tail** for a method with one int
  local. Each zero store is individually justified (a stale word in that tail
  makes G1's band verifier refuse a whole collection) and collectively they are
  ten percent of the call.
* **The 41-byte NOP sled**, jumped over rather than executed — the `perf-02` fix
  working as designed, still costing one taken branch per call and 41 bytes of
  icache footprint per body.

The honest summary is that `fib` is 3.7x because a CratonVM call is ~36
instructions and a HotSpot call is ~10, and closing it means reducing the VM's
per-frame contract rather than optimising the two `lea`s in the middle of it.

## 8. Reproducing

```bash
javac -d probes/out probes/FibCall.java
EXE=./target/release/cratonvm

CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FibCall.fib \
  $EXE -cp probes/out -Dprobe.n=24 -Dprobe.reps=1 FibCall

bash tools/tier-ab/flag-ab.sh -Exe $EXE -Cp probes/out -Class FibCall \
  -Flag CRATONVM_JIT_IR_SINK_EQUAL_DEPTH -On 1 -Off 0 -D probe.n=30 -Rounds 15
```
