# The 14-store blind GPR spill at every compiled call — halved for oop-clean frames, OPEN for every other frame

**Status: OPEN, throughput — all three of this page's levers are now spent.**
Three changes against the same 14-store blind copy of the whole GPR file that
`emit_pre_safepoint_spill` emitted at every GC-capable call. Together, on
`CallArgCostProbe` with all three off against all three on — one binary, six
interleaved readings per arm, **idle host (load 2.2–2.3)**, minimum of each,
deltas over `control`:

| call shape | none of the three | all three | |
|---|---:|---:|---|
| `int0()` static | 3.05 ns | **1.42 ns** | **−53%** |
| `int1(int)` | 3.33 | **1.30** | **−61%** |
| `int2(int,int)` | 3.62 | **1.71** | −53% |
| `ref1(Object)` | 5.09 | **2.13** | **−58%** |
| `ref2(Object,Object)` | 4.23 | **2.38** | −44% |
| `refRet(Object)->Object` | 3.56 | **1.59** | −55% |
| `virtInt` | 5.02 | **3.64** | −27% |
| `virtRef` | 6.68 | **4.72** | −29% |

Every row separates completely — the `all` arm's *maximum* `int0` reading (1.92)
is far below the `none` arm's *minimum* (3.51) — and the blind spill itself goes
**476 → 198 stores (−58%)**.

* **2026-08-26, elision** (`CRATONVM_JIT_CALL_SPILL_ELISION`, default `mic`) —
  where the caller frame is provably oop-clean the spill is replaced by a
  2-instruction safepoint-id publication. **~2x on every call shape.** It
  refuses on most real code, and the refusal census says so in one clause.
* **2026-08-27, narrowing** (`CRATONVM_JIT_SPILL_NARROW`, default ON) — where it
  does NOT refuse the spill outright, it is now cut to the registers that can
  hold an oop: **476 → 260 stores** on `CallArgCostProbe`, **1344 → 830** on
  netty's own loop, and **8–24% off a compiled call's overhead** on top of the
  elision, on frames the elision cannot touch.
* **2026-08-27, the staging IS the publication**
  (`CRATONVM_JIT_SPILL_ARGS_PUBLISHED`, default ON) — the last of the three, and
  the only one that touches the population the full-file spill was introduced
  for. At the four sites that stage their arguments to the frame before the
  safepoint, the register copy of those same values is a duplicate: **260 → 210
  stores** and a further **16–26%** off a call of any shape.

The page stays open because the wall is narrowed, not closed: what a compiled
call still pays is the frame-slot writes it genuinely needs plus the call
sequence itself, and the two netty classes this family is named for still miss
their walls by roughly an order of magnitude. There is no fourth lever of this
shape left — see [What is still there](#what-is-still-there).

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

### DONE 2026-08-27: the spill is narrowed to the registers that can hold an oop

The elision is all-or-nothing by construction, so the answer to a refusal that
is 100% one clause is not a better elision — it is to keep the spill and make it
smaller. `CRATONVM_JIT_SPILL_NARROW` (default ON, `=0` restores the full copy)
selects the registers from four sources, and the first two are precisely the
reason `=all` exists at all — `Compiler::new`: *"a receiver/args staged into
ARG_REGS immediately before a GC-capable call … which the callee-saved-only
spill never covers"*:

1. `RAX`, where the emitter materialises every loaded / allocated / returned
   reference before it is pushed or stored;
2. every `ARG_REGS` member — a staged receiver or reference argument;
3. the register home of any local the method-wide reference mask says can hold a
   reference (`SafepointPublishPlan::register_homed_reference_locals` indexed
   through `local_assignments`);
4. any register currently holding an operand-stack entry, oop-marked or not —
   cheaper to keep than to reason about, and `self.stack` is short.

Dropped: a register hosting a local the mask says is *primitive*, and a register
this method's model never put anything in (`R10`/`R11` on SysV, plus unused
local homes). The mask is conservative in the safe direction —
`find_reference_locals` ORs in every `aload`/`astore` across the whole method, so
javac's cross-scope slot reuse only makes it name MORE locals, never fewer.

Fails closed to the full fourteen on: no publish plan (the legacy `compile` test
wrapper), more than 64 locals (the mask's unrepresented tail), or an explicit
`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`. The slot LAYOUT is unchanged —
`emit_blind_reg_spill` indexes by position in `ALL_SPILL_GPRS` — so a skipped
store leaves a **stale** slot, which the scanner re-validates through
`heap.is_object_address` and which can therefore only over-retain, never
under-report. The inline-TLAB `new` site splits its spill across two program
points, and the deferred half reads the selection the safepoint captured
(`pending_narrow_spill`) rather than recomputing it, because the operand-stack
model has moved on by the slow-path label.

**What it is worth.** Measured with the ELISION PINNED OFF
(`CRATONVM_JIT_CALL_SPILL_ELISION=0`), so every call keeps a spill and the
narrowing is the only difference — ten interleaved readings per arm, load ~11,
minimum of each, deltas over `control`:

| call shape | full 14 | narrowed | |
|---|---:|---:|---|
| `int0()` | 4.01 ns | **3.33 ns** | −17% |
| `int1(int)` | 4.39 | **3.44** | −22% |
| `int2(int,int)` | 5.11 | **4.44** | −13% |
| `ref1(Object)` | 6.28 | **5.07** | −19% |
| `ref2(Object,Object)` | 6.57 | **5.61** | −15% |
| `refRet(Object)->Object` | 5.72 | **4.37** | −24% |
| `virtInt` | 9.12 | **8.29** | −9% |
| `virtRef` | 10.80 | **9.91** | −8% |

The separation is complete on the static rows: the **maximum** of the ten
narrowed `int1` readings (4.74) is below the **minimum** of the ten full ones
(5.05). Width, from the compile-time census, which is load-independent and is
the engagement proof: `safepoint spill width: stores-emitted=260
stores-if-full=476 full-refused=1` against `476 / 476 / 34` unnarrowed.

**What it is NOT worth: anything visible on the two netty loops.** Their store
counts drop (`1344 → 830` on `HttpStatusClassLoopRate`), but ~5 saved stores per
safepoint against a ~100 ns iteration is ~2–3%, and twelve interleaved readings
per arm on this host could not separate the arms in either direction. Recorded
so nobody re-runs it expecting otherwise.

### DONE 2026-08-27: the argument staging IS the publication

The narrowing's residual was RAX + the six `ARG_REGS` + the reference-local
homes, and the first two are exactly the population `=all` was introduced for.
They are also the two the call site has *already written to the frame* by the
time the spill runs, so the register copy is a duplicate of a publication that
already happened. `CRATONVM_JIT_SPILL_ARGS_PUBLISHED` (default ON, `=0` keeps
copying them) drops them — **per site, never globally**, and as **two separate
claims**, because different code publishes each:

| site | `ARG_REGS` published | `RAX` published |
|---|---|---|
| `invokestatic` direct | when `service_args_base.is_some()` | **no** — that site stages through `R11` |
| `invokespecial`/`virtual` direct | when `service_args_base.is_some()` | **no** — same |
| `jit_invoke_dispatch` helper | always (they carry the helper ABI, never a Java oop) | when `n > 0` |
| MIC/PIC cascade | always | when `n > 0` |

The `service_args_base.is_some()` condition is the one that matters: with no
service slots reserved the arguments live in `ARG_REGS` **and nowhere else**,
and the claim would be false. The first cut of this change asserted both claims
unconditionally at all four sites and was rebuilt before it was ever gated —
the tell was reading the staging loops and noticing that two of them use `R11`
while two use `RAX`, which is the difference between "RAX holds a staged
argument" and "RAX holds whatever it held".

The one-shot carrying the claim is set by
`emit_pre_safepoint_spill_args_published` and taken at the top of the spill, in
the same breath, so a site that stages and then does *not* reach the spill
(because the elision won) cannot leak the claim into the next safepoint.

**What it is worth**, with the ELISION PINNED OFF so every call keeps a spill
and this cut is the only difference — ten interleaved readings per arm, **idle
host (load 2.3–2.4)**, minimum of each, deltas over `control`:

| call shape | keeping RAX+ARG_REGS | published |  |
|---|---:|---:|---|
| `int0()` | 2.41 ns | **1.89 ns** | −22% |
| `int1(int)` | 2.53 | **1.90** | −25% |
| `int2(int,int)` | 2.84 | **2.10** | −26% |
| `ref1(Object)` | 3.24 | **2.57** | −21% |
| `ref2(Object,Object)` | 3.57 | **2.93** | −18% |
| `refRet(Object)->Object` | 2.89 | **2.24** | −22% |
| `virtInt` | 4.43 | **3.62** | −18% |
| `virtRef` | 5.61 | **4.71** | −16% |

`int0`, `int1`, `int2`, `virtInt` and `virtRef` separate completely — `int0`
`on` max 2.44 against `off` min 2.87. Width: 260 → 210 stores; on netty's own
loop 830 → 522, i.e. **1344 → 522 cumulative**.

*A first pass at this measurement ran at load ~11 and read −1% to −13%, with
`virtRef` apparently unmoved. Same binary, same protocol; the host was simply
not quiet. The load figure is printed with every reading on this page for that
reason — a per-call cost of a couple of nanoseconds is not measurable against a
loaded machine's noise, and reporting it as a small effect rather than an
unmeasured one is the error to avoid.*

## What is still there

No fourth lever of this shape. What a call still writes is the reference-local
homes it genuinely has to publish, plus the operand-stack registers, plus the
call sequence itself; the remaining gap to HotSpot on these rows is that HotSpot
*inlines* them, which is a different page. The two netty classes this family is
named for still miss their walls by roughly an order of magnitude, and this
work did not measurably move either of them — their loops are dominated by
per-iteration costs that are not the spill.

The caution, unchanged: **the gate that matters is not a test suite. It is
relocation under moving young** — see below.

## Gates

All three are GC-root-visibility changes, so the correctness argument is the
gate list, not the diff. Everything below was run for the elision (2026-08-26),
re-run for the narrowing and re-run again for the staging claim (2026-08-27,
each flag off and on); the numbers quoted are the staging claim's, and the
earlier two were the same.

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
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp . CallArgCostProbe 20000000 2>&1 | grep -E 'direct-call spill|spill width'
```

The narrowing on its own, with the elision pinned off so it is the only
difference — this is the arm the −8..−24% above comes from:

```bash
CRATONVM_JIT_CALL_SPILL_ELISION=0 cratonvm --java-home <jdk> -cp . CallArgCostProbe 40000000
```

```bash
CRATONVM_JIT_CALL_SPILL_ELISION=0 CRATONVM_JIT_SPILL_NARROW=0 cratonvm --java-home <jdk> -cp . CallArgCostProbe 40000000
```

The staging claim on its own, and all three against none of them:

```bash
CRATONVM_JIT_CALL_SPILL_ELISION=0 CRATONVM_JIT_SPILL_ARGS_PUBLISHED=0 cratonvm --java-home <jdk> -cp . CallArgCostProbe 40000000
```

```bash
CRATONVM_JIT_CALL_SPILL_ELISION=0 CRATONVM_JIT_SPILL_NARROW=0 CRATONVM_JIT_SPILL_ARGS_PUBLISHED=0 cratonvm --java-home <jdk> -cp . CallArgCostProbe 40000000
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
* [`interpreted-invoke-cost-350ns-RETIRED-20260911.md`](interpreted-invoke-cost-350ns-RETIRED-20260911.md)
  — the same question one tier down, for calls that never reach compiled code.
* `a-compiled-call-goes-out-to-rust-two-causes-RETIRED-20260817.md`
  (internal) — the previous per-call cost to be measured and closed.
