# The bounds-checked array element read — FIXED 2026-09-02, 5.4x on the page's own witness

**Was `docs/known-issues/perf/array-element-load-baseline-codegen-20260901.md`,
opened 2026-09-01 as "OPEN, throughput. Read from the emitter, not measured."**

The page named three removable rows and asked for one measurement it had not
taken. All three are done. The measurement is done, and it says the page's
central claim was right about the *bound* and wrong about almost everything
else: the loop-invariant `arraylength` was not a ~5-instruction tax on a
21-instruction body, it was worth **5.4x**, and the body the page traced
instruction by instruction was not the body that ran.

`probes/CharAtCostCurve.java`'s `char[]` rows — `scanArr`, the page's own
witness, the exact loop its 21-instruction table walks. Three interleaved
rounds of every arm, steady-state reps only, 9 samples per arm,
`/proc/loadavg` 4.5-11.9 and recorded beside each run:

| arm | median ns/char | range |
|---|---:|---|
| before | 3.46 | 3.10-3.94 |
| **after** | **0.64** | 0.54-0.95 |
| after, `CRATONVM_JIT_LICM=0` | 3.48 | 3.07-5.86 |
| HotSpot 25, same host, same run | 0.14 | 0.11-0.54 |

**5.4x**, and the ratio to HotSpot goes from **24.7x to 4.6x**. The kill switch
reproduces the before column, so the A/B is one binary and one flag; the ranges
are the price of a shared host and are why medians over interleaved rounds are
quoted rather than a best run.

The same shape on the new probe, where the hand-hoisted control runs beside it
(`probes/ArrayElemLoadCost.java`, 15 interleaved runs, median):

| row | before | after |
|---|---:|---:|
| `char[]` | 3.49 ns/elem | **0.62** |
| `char[]+len` — `a.length` hoisted BY HAND | 0.58 | 0.58 |

The fixed row sits **on** its hand-hoisted control. That is what "hoisted the
invariant" is supposed to mean, and it is the only evidence that separates a
complete transform from a partial one.

## The measurement the page owed

> *"That last arm is the first measurement this page owes. If
> `CRATONVM_JIT_NO_BCE=1` makes the `char[]` row materially worse, the elision
> traced above is firing and the trace is confirmed from the outside."*

It does, and the arm the page proposed is the weakest of the three ways to see
it — worth writing down, because the flag does not engage where the page
reasons.

**Decisively, from the emitted bytes.** `CRATONVM_DBG_DUMP_JIT=charScanLen`
plus `objdump -D -b binary -m i386:x86-64 -M intel` on the single-pass artifact:
the inner loop is

```
mov  rax,rbx                          ; array
mov  rcx,r12                          ; index
movzx eax,WORD PTR [rax+rcx*2+0x10]   ; the element
```

with no `mov r10d,[rax+0x4]`, no compare against a length and no `jae` anywhere
between the index materialisation and the load. The check is not cheap, it is
**absent**. That settles it without a stopwatch.

**From the switch, where it reaches.** `CRATONVM_JIT_NO_BCE` is read in
`x64/driver.rs` and `x64/single_pass_only.rs` — the single-pass backend's
elimination and the ROUTING — and not by the optimizing tier's own bounds
reasoning. Pinned to the single-pass backend so the flag governs the body being
timed, three interleaved rounds: 0.71 ns/elem with elimination, 0.81 without,
**+14%**.

**From the default routing**, which is what the page actually proposed:
`CharAtCostCurve` at its quietest run, 3.70 ns/char default (load 8.08) against
5.21 with `NO_BCE=1` (load 6.95) — **+41% at a LOWER load**. Corroboration
rather than proof, because on that path the flag also changes which backend
compiles the method.

The page's seven-step source trace of `bce.rs` / `scev.rs` is confirmed. That
part of it was right and is now checked from outside.

## What was actually wrong

### 1. The traced emitter is not the emitter that ran

The page's own "Not determined" list opened with *"Which tier compiled the
measured `scanArr`"*, and the answer invalidates the 21-row instruction table
that is most of the page. The table is a correct reading of `jit/src/x64/`, the
single-pass backend. The 5.2 ns row is `ir_lower.rs`'s output.

One switch separates them, and it is not subtle:

| `char[]` element read, ns/elem | default routing | `CRATONVM_NO_IR_BRANCHY=1` |
|---|---:|---:|
| `for (i = 0; i < a.length; i++)` | 3.87 | **1.20** |
| the same loop, length in a local | 0.63 | 0.94 |

The optimizing tier is *better* than the single-pass backend on the local-bound
loop and **3x worse** on the `arraylength`-bound one, and the default routing
sends the second shape to it. Disassembling both artifacts
(`CRATONVM_DBG_DUMP_JIT=charScan`, then `objdump -D -b binary`) shows why: the
single-pass body is register-homed, 2x-unrolled, with both checks elided and
the constant compare fused; the IR body round-trips every value through a frame
slot, materialises the `if_icmpne` as a boolean with `setne`/`movzx`/`test`, and
emits both a null check and a bounds check.

### 2. The invariant `arraylength` was worth 5.4x, not 25%

The page sized the hoist by counting instructions: 5 of 21, "the single largest
item", inside an attribution that concluded *"Fixing the emitter takes 44x to
about 14x"*. The arithmetic is right and the conclusion is wrong, because an
instruction count is not a cost model for a body whose exit branch waits on a
dependent load.

The page never ran the control that would have said so. It is one method:

```java
for (int r = 0; r < reps; r++) {
    int n = a.length;                    // hand-hoisted
    for (int i = 0; i < n; i++) …
}
```

On the *unfixed* binary that arm ran at 0.58-0.69 ns/elem against the same
loop's 3.49-5.07. **The hoist was the whole gap**, and the page's own
`char[]`-vs-`charAt` framing had the same control sitting one method away.
`probes/ArrayElemLoadCost.java` is that probe, checked in.

### 3. The IR tier's LICM was disqualified by the node it needed to hoist

`CRATONVM_DBG_LICM=1` named it in one line, on every method:

```
[DBG_LICM] header 5: body 18 node(s), 0 load(s), hard_barrier=true
```

`loop_has_hard_barrier` refuses a loop containing any node that is not pure,
not control, and not `Load`/`Store`/`Phi` — because such a node may read or
write arbitrary memory. `Op::ArrayLength` is impure (it raises NPE on null), so
a javac counted loop **disqualified its own LICM by the very node the loop most
needed hoisted**, and took every other load in the body down with it.

### 4. …and it never saw an inner loop at all

`loop_headers` classified a header's control inputs by asking which are
reachable *forward from the header*. For an inner header that question has no
useful answer: the walk leaves through the inner exit, goes round the OUTER
back edge, and arrives at the inner loop's own pre-header, so every input reads
as a back edge and the loop yields no pre-header. The code knew and said so —
*"safe, just not optimized (same limitation as the unroll pass)"*.

It is not a corner. Every `for (r…) for (i…)` in the tree offered this pass only
its outer header, and the inner loop is the one running 100,000 times.

## What was fixed

Five commits on `perf/array-elem-load-codegen-20260902`.

**The page's three named items.**

1. **The loop-invariant `arraylength` is hoisted into the pre-header**, in
   *both* emitters. `ArrayLenHoist` in `jit/src/x64/licm.rs` for the single-pass
   backend, beside the existing aaload and arith hoists; `Op::ArrayLength`
   support in `ir_optimize::licm` for the optimizing tier. The cached value is a
   PRIMITIVE, which is why this is tractable where the general `getfield` LICM
   scaffold (`loop_analysis::find_invariant_loads`, still inert with its
   `TODO(round-12+)`) is not: an int in a frame slot is no GC root, needs no oop
   map, survives every safepoint, and cannot be invalidated by relocation.

   Neither hoist needs a deopt. Both take only sites that execute
   unconditionally on the first pass through the header — the single-pass
   matcher requires the header's straight-line prefix, the IR one requires the
   control input to BE the header region — so a null receiver simply throws the
   NPE the body would have thrown, through the same stub and the same JEP-358
   action. Routing it to a deopt stub instead would work and was the first shape
   of the single-pass code; it bakes a `Box` ADDRESS into the instruction
   stream, and `corpus_is_deterministic_within_a_process` fails on the spot.

2. **Seeding the loop header's non-null IN set** was item 2, "and it composes
   with (1)". It composes so completely that it is subsumed: once the
   `arraylength` is in the pre-header the null check goes with it and the header
   emits none. `emit_null_check_arraylength`'s doc records both the dataflow
   reasoning (still live for a header the hoist declines) and the fact that the
   hoist is what closed it.

3. **The safepoint poll is one RIP-relative instruction.** It read
   `MOV R11, imm64 ; TEST BYTE [R11], 0xFF` — 15 bytes, two instructions, a
   register — on the page's premise that x86-64 has no absolute-address
   `TEST [m64], imm`. It has a RIP-relative one: `TEST BYTE [rip+d32], 0xFF` is
   7 bytes and needs no register. Both backends; the old form stays as the
   out-of-reach fallback.

   **This one measured zero, and the reason is worth keeping.** The flag is at
   `0x2_0018_105d_c0` and the JIT code cache mmaps at `0x7eee_1db4_5000` — 130
   TB apart, so `emit_test_mem8_abs_imm8`'s ±2GB test refuses at every site and
   the fallback is what runs. The code is correct and inert on this platform.
   Bringing the code cache within rel32 reach of the VM's data is a separate
   change with a much wider payoff (every helper `CALL` is paying the same
   12-byte fallback) and is not made here.

**Two more the page named but did not take.**

4. **The bounds check's length load moved into its cold stub.**
   `MOV R10D,[RAX+len] ; CMP ECX,R10D ; JAE` becomes `CMP ECX,[RAX+len] ; JAE` —
   one instruction and four bytes fewer on **every** emitted bounds check, i.e.
   everywhere BCE does not fire, which is where `sort` and `string_scan` live.
   The page ruled this out as a local peephole for a correct reason: R10D is a
   live OUTPUT, read by `emit_bounds_check_stubs` as `jit_throw_aioobe`'s
   `length` argument, so folding the load alone leaves the exception message
   reporting whatever R10 last held — a wrong number, not a crash. The stub now
   re-loads it. `bounds_check_length_is_reloaded_in_the_cold_stub` pins both
   halves.

5. **`ARRAY_LENGTH_OFFSET`'s own comment** said "8, not 12" above a value of
   `4`. Both numbers wrong, and uncatchable, because every emission site takes
   the constant while the prose drifts. It is now derived: a const assert pins
   it to `offset_of!(ObjectHeader, shape)`, so a field reorder is a build error
   rather than another stale sentence. `ObjectHeader`'s own doc still described
   the pre-shrink 32-byte header with five fields that no longer exist;
   corrected too. Both new emission sites go through the build-checked
   `disp::disp8_const` rather than a raw `as u8`.

**And the two IR-tier defects the measurement exposed** (3 and 4 above):
`Op::ArrayLength` hoisting, and dominance-based inner-loop header discovery.
The second is one reachability walk with the header deleted — the definition of
dominance read directly, no dominator tree — and is applied only where the
existing test finds zero pre-headers, so every loop the pass already classified
keeps exactly the answer it had.

## Measured

`probes/ArrayElemLoadCost.java`, `reps=2000`, host load ~6, one run per arm.
`+len` rows are the hand-hoisted controls and are the ceiling.

| row | before | after | `CRATONVM_JIT_LICM=0` | HotSpot 25 |
|---|---:|---:|---:|---:|
| `char[]` | 5.07 | **0.95** | 4.23 | 0.21 |
| `char[]+len` | 0.69 | 0.87 | 0.74 | 0.20 |
| `int[] sum` | 2.22 | **1.08** | 2.47 | 0.25 |
| `int[] sum+len` | 0.68 | 0.87 | 0.67 | 0.26 |
| `int[] cnt` | 3.83 | **0.86** | 4.26 | 0.54 |
| `int[] cnt+len` | 0.66 | 0.81 | 0.85 | 0.52 |

Every `arraylength`-bound row collapses onto its hand-hoisted sibling; every
hand-hoisted row is unchanged; the kill switch restores the before column.
That is the shape a complete transform has, and it is why the interleaved
15-run median in the header table (3.49 → 0.62) is quoted rather than these
single runs — the single-run spread on this shared host is ±25%.

Single-pass arm alone (`CRATONVM_NO_IR_BRANCHY=1`, 5 interleaved rounds,
`char[]` median): base 0.90, fixed 0.74, hoist disabled 1.18. **~18%** — which
is what an instruction count predicts, and is the number the page's attribution
would have been right about if the traced emitter had been the one running.

`regression-suite/run.sh`, both binaries, same host: base 82 passed / 3 failed,
fixed 81 / 5. The two extra are `RNetIfaceScope` and its harness error, whose
message says the **HotSpot oracle** run failed; re-run alone both binaries pass
it. The three real failures (`RMapGcStress`, `RJdkIntrinsics3`,
`RBufferPoolCount`) are identical on both and predate this work.

## Still open

* **The remaining ~4.6x to HotSpot is vectorisation**, as the page said — but
  the size was wrong in both halves. The page split 44x as 3x codegen and 13x
  vector. Measured on one host in one run it was 24.7x, split as **5.4x the
  invariant bound** and **4.6x vector**, and the vector half is what is left. `jit/src/x64/simd.rs` detects `int` array
  sum, `double` array sum, element-wise, matrix-dot and byte-sieve; a `caload`
  compare-and-count is none of them.

* **The RIP-relative safepoint poll cannot reach its flag** (item 3 above). The
  code is in and correct; the payoff needs the code cache placed within ±2GB of
  the VM's data segment, which would also retire the 12-byte
  `MOV RAX, imm64 ; CALL RAX` fallback that every helper call currently takes.
  That is the next measurement worth taking on this path.

* **The optimizing tier is 3x worse than the single-pass backend on an
  `arraylength`-bound loop** was the routing finding (item 1 above). This change
  removes the *cause* rather than the routing, so the tiers now agree on this
  shape — but the finding was made on one probe, and whether other shapes route
  the same way is unmeasured. `CRATONVM_NO_IR_BRANCHY=1` is the arm.

* **`loop_body` over-approximates an inner loop's body** — the pinned-node
  sweep follows a data edge out through the inner exit and drags the enclosing
  loop in behind it, so both headers of a nested pair report the same body. The
  `arraylength` hoist is ordered ahead of the guard that reads it (an over-large
  body is conservative for every test that hoist makes), but the general load
  hoist is still blocked in every inner loop by a barrier that is not in it.
  Bounding the body by dominance is the obvious fix and is NOT taken here:
  `test_loop_body_pins_a_node_whose_only_link_sorts_after_it` catches the first
  attempt immediately, and the over-approximation is load-bearing in the
  direction that matters.

## Reproducers

```sh
javac -g -d probes probes/ArrayElemLoadCost.java

java     -cp probes ArrayElemLoadCost 2000    # the oracle
cratonvm -cp probes ArrayElemLoadCost 2000

CRATONVM_JIT_LICM=0              cratonvm -cp probes ArrayElemLoadCost 2000  # IR-tier hoist off
CRATONVM_DISABLE_ARRAYLEN_LICM=1 cratonvm -cp probes ArrayElemLoadCost 2000  # single-pass hoist off
CRATONVM_NO_IR_BRANCHY=1         cratonvm -cp probes ArrayElemLoadCost 2000  # single-pass backend only

CRATONVM_DBG_LICM=1     cratonvm -cp probes ArrayElemLoadCost 100 2>&1 | grep arraylength
CRATONVM_DBG_JIT_GEN=1  cratonvm -cp probes ArrayElemLoadCost 100 2>&1 | grep arraylength-LICM
```

`probes/CharAtCostCurve.java` is the page's original witness; its `char[]` rows
are `scanArr`, the same loop.

## Flags added

All default-ON, each reaching the level its change is at, so one binary A/Bs
all of it:

| flag | off arm |
|---|---|
| `CRATONVM_DISABLE_ARRAYLEN_LICM=1` | single-pass `arraylength` hoist |
| `CRATONVM_JIT_RIP_SAFEPOINT_POLL=0` | RIP-relative poll → `MOV R11, imm64` |
| `CRATONVM_JIT_FUSED_BOUNDS_LOAD=0` | fused bounds compare → `MOV`/`CMP` pair |
| `CRATONVM_JIT_LICM=0` (existing) | the whole IR-tier LICM pass |
