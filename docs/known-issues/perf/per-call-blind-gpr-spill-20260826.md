# The 14-store blind GPR spill at every compiled call — halved for oop-clean frames, OPEN for every other frame

**Status: OPEN, throughput.** Half of this is done and half is not, and the
open half is why this page is in `known-issues/` rather than in the internal
record: a compiled call still carries a blind 14-store copy of the whole GPR
file on **every frame that holds a reference in a register-homed local**, which
is most real code. What landed 2026-08-26
(`CRATONVM_JIT_CALL_SPILL_ELISION`, default `mic`, `=0` restores the old route)
elides that spill where the caller frame is provably oop-clean — **~2x on every
call shape** there — and, more usefully for whoever picks this up, it makes the
remaining refusals countable: they are **100% one clause**, and
[the next lever](#the-next-lever-therefore-is-to-narrow-the-spill-not-to-elide-it)
is named at the bottom of this page.

All numbers: **Azure Linux host `vm1`, quiet (load 3.9–4.7)**, 2026-08-26,
release build, real-JDK mode, ONE binary with the flag off and on, eight
interleaved (ABBA) readings per arm, minimum of each.

## What a compiled call was emitting

`probes/CallArgCostProbe.java` prices a compiled static call at ~4 ns against
HotSpot's ~0 — the number both netty exhaustive-loop pages and
`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md` end on. The
disassembler answers what it is spent on directly:

```bash
CRATONVM_DBG_JIT_DISASM=CallArgCostProbe.armInt1 cratonvm --java-home <jdk> -cp . CallArgCostProbe 2000000
```

The hot loop of `armInt1` — `s += int1(i)`, a `long` accumulator, an `int`
counter, **no reference anywhere in the frame** — emits 26 instructions of call
overhead, and 14 of them are one thing:

```asm
46f: mov r11,r12                ; stage arg
472: mov [rbp-50h],r11          ; operand-stack flush
476: mov rdi,r12                ; ABI arg
479: mov [rbp-8],r14            ; register-homed local flush  (x4)
...
489: mov [rbp-0E8h],rax         ; <-- the blind full-GPR spill: RAX, RCX, RDX,
490: mov [rbp-0F0h],rcx         ;     RBX, RSI, RDI, R8, R9, R10, R11,
...                             ;     R12, R13, R14, R15 -- 14 stores
4e4: mov [rbp-150h],r15
4eb: mov rax,0Bh                ; safepoint id
4f2: mov [rbp-30h],rax
4f6: call <int1>
```

That is `emit_pre_safepoint_spill`'s SB-CRASH-04 blind spill
(`safepoint_reg_spill_all`), and it is **on by default** — not through
`CRATONVM_JIT_SAFEPOINT_REG_SPILL`, which is opt-in, but through
`reg_spill_for_root_visibility = !precise_reg_spill_disabled()`, which is
opt-OUT. It exists so the conservative `[scanner_sp, entry_sp)` root scan can
see an oop that lives only in a register across a GC-capable call.

In `armInt1` there is no oop to see.

## The change

`can_elide_self_call_register_spill` already proved exactly the right thing —
"every live oop in this frame is frame-resident, so a conservative register copy
publishes nothing new" — and was consulted at **one** call site: the direct
self-recursive call. Nothing in that proof is about the callee, so it is now
`call_spill_elision_core` and is asked at three more sites:

* the `invokestatic` direct call to a compiled callee,
* the `invokespecial`/`invokevirtual` direct call to a compiled callee,
* the `jit_invoke_dispatch` helper call, and the MIC/PIC inline-dispatch
  cascade (the `mic` mode, which is the default — it is what
  `invokevirtual`/`invokeinterface` actually take).

On a hit, `emit_safepoint_metadata_only` publishes the safepoint id in **2
instructions** instead of 18.

Two things had to change for it to fire at all:

1. **The survivor test.** The self-call form requires every operand-stack
   survivor to be `StackSlot::Frame`. `s += int1(i)` leaves the `long`
   accumulator on the operand stack in a *callee-saved register*, which the
   callee preserves and whose saved copy is inside the callee's frame — so the
   relaxed form refuses only `Scratch`/`Xmm` survivors, which the `CALL`
   genuinely clobbers. With the strict test the counter read
   `elided=0 frame-not-clean=6`: wired, engaged, worth nothing.
2. **Arguments.** At the `CALL` the arguments are in ABI registers, which the
   caller-frame proof cannot see. Mode `1` refuses any reference argument
   outright. Modes `2`/`3` admit them only where they are already frame-resident
   — the direct sites copy every argument into the callee-sentinel service slots,
   and the dispatch/MIC sites stage them into the helper's args buffer AND name
   the oops among them in the safepoint map (`pending_staged_arg_oops`).

## What it is worth

`probes/CallArgCostProbe.java`, deltas over the `control` arm, minimum of eight
interleaved readings per arm, one binary:

| call shape | spill (`=0`) | elided (default) | ratio |
|---|---:|---:|---:|
| `int0()` static, no args | 3.19 ns | **1.51 ns** | 2.1x |
| `int1(int)` | 3.47 | **1.60** | 2.2x |
| `int2(int,int)` | 3.78 | **1.92** | 2.0x |
| `ref1(Object)` | 4.54 | **2.69** | 1.7x |
| `ref2(Object,Object)` | 4.43 | **2.66** | 1.7x |
| `refRet(Object)->Object` | 3.70 | **2.43** | 1.5x |
| `virtInt` virtual, int arg | 4.36 | **3.09** | 1.4x |
| `virtRef` virtual, ref arg | 5.21 | **3.68** | 1.4x |

Reproduced on two independently built binaries (`p5`, `p6`) with the same ABBA
protocol. HotSpot is ~0 on every row because it inlines all of them; this is a
cut into the non-inlined call cost, not a claim about the inlining gap.

Engagement counter, under `CRATONVM_DBG=jit-method-stats`:

```
[cratonvm] direct-call spill: elided=8 oop-arg=0 no-precise-maps=0 ref-local-in-reg=0 marks-inexact=0 survivor-in-scratch=0 moving-unpublishable=0
```

`elided=0` on a run means this changed nothing there, whatever the clock says.

## What it does NOT move, and the one clause that says why

**It does not move either netty exhaustive-loop class.** Measured on the same
quiet host, three interleaved rounds each, and recorded here so nobody re-runs
it:

| | `=0` | default |
|---|---:|---:|
| `HeaderValidationLoopRate` value loop | 652–685 ns/iter | 684–718 |
| `HeaderValidationLoopRate` name loop | 1144–1225 | 1168–1284 |
| `HttpStatusClassLoopRate` real loop | 106.5–121.2 | 105.8–112.1 |

The counter says exactly why, and it is a single clause:

```
HeaderValidationLoopRate:  elided=1 oop-arg=0 no-precise-maps=0 ref-local-in-reg=116 marks-inexact=0 survivor-in-scratch=0 moving-unpublishable=0
HttpStatusClassLoopRate:   elided=1 oop-arg=0 no-precise-maps=0 ref-local-in-reg=46  marks-inexact=0 survivor-in-scratch=0 moving-unpublishable=0
```

**100% of the refusals are `ref-local-in-reg`** — a register-homed local that
could hold an object reference. Not the operand-stack marks, not a scratch
survivor, not the moving-young publication proof: those are zero. Real
reference-manipulating code keeps a receiver or a `this` in a register-homed
local, and then the blind spill is doing work the elision cannot argue away.

### The next lever, therefore, is to NARROW the spill, not to elide it

The elision is all-or-nothing by construction. What the refusal census points at
is different: spill the registers that can hold an oop, instead of all fourteen.
The compiler already has the material —
`SafepointPublishPlan::register_homed_reference_locals` is a bitmask of exactly
the locals whose register residency can hide a root, and `local_assignments`
maps each to its register. A register hosting a *primitive* local, or hosting
nothing this method ever wrote, is a store per call for nothing.

Two cautions for whoever takes it:

* the spill is deliberately **blind**, and its own comment says why — "an oop
  can also live in a callee-saved register as an operand-stack temporary that
  survives the call, or via a value the per-slot oop tracker fails to tag." The
  operand-stack half of that is now covered separately and precisely by
  `flush_callee_saved_oops_enabled()` (default-on); what a narrowing gives up is
  the untracked-intermediate defence, which is what `=all` was added for
  (Keycloak Gap 9, a live oop in a caller-saved / argument / RAX register). Any
  narrowing must keep RAX and the ARG registers.
* the gate that matters is not a test suite. It is relocation under moving young
  — see below.

## Gates

This is a GC-root-visibility change, so the correctness argument is the gate
list, not the diff.

* **Relocation under moving young**, `-XX:+UseGenerationalGC`, three interleaved
  rounds per arm, checksums compared against HotSpot on the same host:
  `MovingYoungConcurrentProbe` checksum `3852744000` — HotSpot's, in all six
  CratonVM runs; `BinTreesClassic 16` → `[14985902]`, HotSpot's, in all six;
  `IdentityHashAcrossGcProbe` 0 hash changes / 0 map misses in all;
  `GcWalkProbe` `liveNodes=100212`, HotSpot's, in all. The
  `[moving-young] fallback` count is **identical between arms** (1 / 2–3 / 3 / 6
  per probe), i.e. the elision does not make the collector give up its precise
  coverage more often — which is the second reading, and the one a pure
  pass/fail would have missed.
* **netty `codec-http`, 93 classes**, flag off and on: identical result sets,
  88 `PASS` / 3 `HANG` / 2 `NOTESTS` in both arms.
* **`regression-suite/run.sh`**: 72 of 72 scheduled vectors pass in both arms.
* `cargo test --release`: `cratonvm-jit --lib` 2110, `cratonvm-vm --lib` 2623,
  `cratonvm-native-builtins --lib` 4167, `cratonvm-types --lib` 581,
  `stub_ratchet` 12, `doc_citation_paths` 3 — all pass, 0 failed.

`BinTreesClassic 16` wall, same runs: `=0` 467–736 ms, default 479–661 ms — no
regression on the allocation-heavy shape either.

## Repro

```bash
cratonvm --java-home <jdk> -cp . CallArgCostProbe 20000000
```

```bash
CRATONVM_JIT_CALL_SPILL_ELISION=0 cratonvm --java-home <jdk> -cp . CallArgCostProbe 20000000
```

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp . CallArgCostProbe 20000000 2>&1 | grep 'direct-call spill'
```

```bash
CRATONVM_DBG_JIT_DISASM=CallArgCostProbe.armInt1 cratonvm --java-home <jdk> -cp . CallArgCostProbe 2000000 2>&1 | grep -c 'mov \[rbp'
```

## Related

* [`../netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](../netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  and [its sibling](../netty/httpresponsestatustest-exhaustive-loop-timeout-20260816.md)
  — the two pages whose entire remainder is this wall. This narrows the wall for
  oop-free frames and, as measured above, not for theirs.
* [`../netty/fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](../netty/fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — the family page for the same wall.
* [`interpreted-invoke-cost-350ns-20260825.md`](interpreted-invoke-cost-350ns-20260825.md)
  — the same question one tier down, for calls that never reach compiled code.
* `performance/a-compiled-call-goes-out-to-rust-two-causes-RETIRED-20260817.md`
  (internal) — the previous per-call cost to be measured and closed.
