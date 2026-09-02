# The bounds-checked array element read is 44x HotSpot, and the bounds check is not why

**Status: OPEN, throughput. Read from the emitter, not measured.** The witness
numbers below are other people's measurements; the instruction sequence and
every attribution on this page is a source trace of the single-pass x64 backend
on `claude/audit-impl-20260901` @ `19812fecc`, working tree otherwise clean. No
binary was built and no disassembly was taken for this page. Where that
matters, it is said in the row.

## The witness

One bounds-checked array element read in a compiled counted loop. Nothing else
in the body.

| workload | HotSpot 25 | CratonVM | ratio |
|---|---:|---:|---:|
| `char[]` element read, `probes/CharAtCostCurve.java` `scanArr` | 0.12 ns/char | **5.2** | **44x** |
| `int[]` sum in a counted loop, `Bench.array_sum` | 3 ms | 206 ms | **69x** |

For scale: this VM's dispatch, invoke, exception handling and integer codegen
all sit at **2-8x** HotSpot on the same runs. This is not the general codegen
gap. It is roughly ten times worse than this VM's own baseline, and it is the
**floor** under every array-touching workload measured — `sort` at 564x and
`string_scan` at 211x are both dominated by element reads.

Both probes are already checked in. `scanArr` is the isolated measurement: it is
`CharAtCostCurve`'s control arm, the identical loop with the `String` call taken
out, and
[`string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.md`](string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.md)
explicitly parks the row as "its own question about baseline codegen quality".
This is that question.

## What the emitter actually emits

`scanArr`'s inner loop, `javac -g` (this is the exact bytecode, taken with
`javap -c -p -l`):

```
  12: iload         4        <- loop header
  14: aload_0
  15: arraylength
  16: if_icmpge     37
  19: aload_0
  20: iload         4
  22: caload
  23: bipush        97
  25: if_icmpne     31
  28: iinc          2, 1
  31: iinc          4, 1
  34: goto          12       <- back edge
```

Walked through `jit/src/x64/bytecode_walk.rs`'s opcode arms and the emitters
they call. `Ra` = the callee-saved home of local 0 (`a`), `Ri` = local 4 (`i`),
`Rc` = local 2 (`c`). Register homes are the graph-colouring allocator's output;
they are enabled by default here because `precise_jit_maps_enabled()` is
default-ON, which satisfies `callee_saved_gpr_local_homes_enabled()`'s
`precise_maps || moving_young` conjunction — so reference locals are **not**
masked back to frame slots (that masking is the narrow `kernel_reg_homes` path,
and it is skipped precisely when the general one is on).

| # | bci | emitted | why | HotSpot |
|--:|--:|---|---|---|
| — | 12 | *(nothing)* | `iload` of a register-homed local is a zero-cost `StackSlot::CalleeSaved` push | — |
| — | 14 | *(nothing)* | same, `aload_0` | — |
| 1 | 15 | `MOV RAX, Ra` | the `emit_*_regs` family takes the array in RAX and the index in RCX, fixed | — |
| 2 | 15 | `TEST RAX, RAX` | `emit_null_check_arraylength`, **not** elided — see below | — |
| 3 | 15 | `JZ rel32` | to the shared NPE stub | — |
| 4 | 15 | `MOV EAX, [RAX+4]` | `emit_arraylength_regs` — **the length, re-loaded from the header every iteration** | hoisted to a register in the pre-header |
| 5 | 15 | `MOV R8, RAX` | `push_from_rax` into the pure-kernel deferred operand cache | — |
| 6 | 16 | `MOV [rbp-o], R8` | `flush_scratch_registers()` at the top of every branch arm: R8/R9 are caller-saved and not valid across a block boundary | — |
| 7 | 16 | `MOV RCX, R8` | the reload of `[rbp-o]`, elided to a reg-reg move by `slot_mirror` (the store still stands) | — |
| 8 | 16 | `CMP Ri_d, ECX` | the loop test | `cmp r_i, r_len` |
| 9 | 16 | `JGE rel32` | exit | `jl` |
| 10 | 22 | `MOV RAX, Ra` | the fixed-register convention again | — |
| 11 | 22 | `MOV RCX, Ri` | " | — |
| — | 22 | *(nothing)* | null check **elided** — `null_check_elim` proves local 0 non-null from the `arraylength` at 15 | — |
| — | 22 | *(nothing)* | bounds check **elided** — `bounds_safe_pcs` contains 22 | — |
| 12 | 22 | `MOVZX EAX, WORD [RAX+RCX*2+16]` | `emit_char_aload_regs`. **The entire useful work.** | `movzx eax, word [r_a+r_i*2+16]` |
| 13 | 22 | `MOV R8, RAX` | `push_from_rax` again | — |
| 14 | 23/25 | `CMP R8D, 97` | `bipush; if_icmp*` **is** fused by `try_const_compare_peephole` — no spill, no reload | `cmp eax, 97` |
| 15 | 25 | `JNE rel32` | | `jne` |
| (16) | 28 | `ADD Rc_d, 1` + `MOVSXD Rc, Rc_d` | ~1 iteration in 26 | `inc` |
| 16 | 31 | `ADD Ri_d, 1` | `emit_iinc_local` | `inc r_i` |
| 17 | 31 | `MOVSXD Ri, Ri_d` | the register home is 64-bit; every `iinc` re-canonicalises it | — |
| 18 | 34 | `MOV R11, imm64` | safepoint poll: the flag address, **re-materialised every iteration**, 10 bytes | — |
| 19 | 34 | `TEST BYTE [R11], 0xFF` | | `test dword [rip+poll], eax`, once per unrolled group |
| 20 | 34 | `JZ rel32` | over the slow path | — |
| 21 | 34 | `JMP rel32` | back edge | `jl` (fused with #8/#9) |

**~21 instructions and ~66 bytes per element**, one frame store, one array-header
load, one element load, four branches. The essential loop is **seven**:
`movzx / cmp / jne / inc / inc / cmp / jl`.

At 5.2 ns/char that is roughly 13-17 cycles for 21 instructions — an IPC around
1.3, which is what a body with four branches and a store-to-compare hop looks
like. That arithmetic is a sanity check on the instruction count, not an
independent measurement, and no cycle count on this page came from a counter.

## Attribution

| cost | instructions/element | confidence |
|---|---:|---|
| the un-hoisted `arraylength` and everything it drags (#1, #4, #5, #6, #7) | **5** | **seen in the emitter** |
| its null check (#2, #3) | **2** | **seen in the emitter** |
| the fixed RAX/RCX convention at the element load (#10, #11) | **2** | **seen in the emitter** |
| `push_from_rax` on a value the very next instruction consumes (#13) | **1** | **seen in the emitter** |
| the back-edge safepoint poll's address materialisation (#18, #20) | **2** | **seen in the emitter** |
| `iinc`'s 64-bit re-canonicalisation (#17) | **1** | **seen in the emitter** |
| bounds check | **0** | **traced end to end — see below** |
| null check on the element load | **0** | **traced end to end** |
| address recomputation | **0** | it is a single SIB addressing mode; there is nothing to strength-reduce |
| spill/reload around the access | **1 store** | the operand-cache flush at #6 only |

Sum of the removable rows: **13 of the 21**. A perfect scalar body would be
~7 instructions, i.e. about **3x** off the measured 5.2 ns, landing near
1.7 ns/char.

**HotSpot is at 0.12 ns/char, which is ~0.35 cycles per character.** That is
below one instruction per element, so C2 is not running a scalar element read at
all — it is unrolled and vectorised. So of the 44x:

* **~3x is per-instruction codegen quality** — the rows above. Real, and
  entirely inside this VM's existing structure.
* **the remaining ~13x is vector width and unroll depth**, which this backend
  does not have for a `char[]` scan with a conditional counter. `jit/src/x64/simd.rs`
  detects `int` array sum, `double` array sum, element-wise, matrix-dot and
  byte-sieve shapes; a `caload` compare-and-count is none of them.

Anyone reading this page as "fix the emitter and the 44x goes away" is reading
it wrong. Fixing the emitter takes 44x to about 14x. That is still the largest
single lever available, because it is the **floor** under `sort` and
`string_scan` too, and because the vector work is a different project.

## The bounds check is already eliminated — traced

This was the page's first hypothesis and it is false for this shape. The chain,
each link read in source:

1. `detect_loops` (`jit/src/x64/driver.rs`) reports `(12, 34)` from the backward
   `goto`.
2. `locate_exit_test` (`jit/src/x64/bce.rs`) Pattern A matches at the header:
   `extract_iload_local` gives iv = 4, then `decode_bound_expr` ->
   `decode_atom` (`jit/src/loop_analysis.rs`) sees `aload_0` followed by
   `arraylength` and returns **`BoundSource::ArrayLength(0)`**. The
   `if_icmpge`'s target 37 is at `back_edge_end`, so the branch leaves the loop.
3. `analyze_counted_loop_at` agrees on the same `(iv, bound)` pair and
   `find_iv_stride` gives `Const(1)`.
4. `constant_iv_init` finds exactly one out-of-body `istore 4` at bci 10,
   preceded by `iconst_0`, neither of them a branch target, dominating the
   header — so `iv.init` is the constant **0**, not `unknown`. (Had it been
   `unknown`, the proof would have demanded a non-negativity guard, and the
   `Guarded` arm refuses outright when `bound_local` is `None`, which it is
   here. The constant init is load-bearing.)
5. `analyze_array_access_operands` simulates the producer stack from the header
   and reports `22 -> (array 0, index 4)`. The `if_icmpne`'s target 31 is a join
   inside the loop, but it is *after* the `caload`, so the walk records the
   access before it stops.
6. `prove_index_in_bounds_of_array` (`jit/src/scev.rs`): the index is
   non-negative from the constant init, and the upper demand normalises to
   `ArrayLength(0) + 0`, which matches the `denoted` array exactly — the
   tautology arm — so no guard is left and the verdict is
   **`BoundsProof::Static`**.
7. `safe_pcs` gets 22; `emit_bounds_check` returns at its first line.

`bound_local` is `None` here (the limit is an `arraylength`, not a local), so the
*speculative* half never engages and nothing can de-spec it back on. The static
proof stands for the whole life of the compiled body.

Two consequences worth writing down:

* **`jit/src/range_analysis.rs` and `jit/src/scev.rs` are on this path**, not
  only the IR tier's. `scev` supplies `CountedLoop::prove_index_in_bounds_of_array`,
  which is what returns `Static` above. The second, *range*-based reason
  (`range_safe_pcs`, the `if (i >= 0 && i < a.length)` shape) is **default OFF**
  behind `CRATONVM_JIT_RANGE_BCE=1` and contributes nothing here — but it is not
  what this loop needed.
* **The three-instruction bounds check is not free where it does fire**, and it
  fires on every array access outside a proved counted loop, which is most of
  them. See "What was ruled out" below.

## The null check that does survive, and why

`emit_null_check_arraylength` emits `TEST RAX,RAX; JZ rel32` at bci 15 on every
iteration, on a receiver that the previous iteration's own `arraylength`
dereferenced.

It is not a bug in the elision. `crate::null_check_elim::analyze` meets over
paths with a bitwise AND and forces `IN[0] = 0`. Local 0 is a parameter, and
nothing in `scanArr` dereferences it before the header. So at bci 12 the meet is
`{back edge: a non-null} AND {pre-header: nothing proven}` = nothing proven, and
the fact the loop body establishes is destroyed at the header on every pass.
The first iteration genuinely has no proof; iterations 2..n pay for that.

HotSpot does not have this problem because it does not emit an explicit null
check at all — it lets the load fault and translates the signal. CratonVM
cannot: `vm/src/runtime/crash_handler.rs` dumps an `hs_err` and re-raises,
killing the VM instead of throwing, which is exactly why the round-8/round-9
inline checks exist at all.

## What was ruled out

* **The element address is not recomputed.** `emit_char_aload_regs` emits one
  `MOVZX EAX, WORD [RAX+RCX*2+<data offset>]`. Base, scaled index and header
  displacement are a single x86 addressing mode; there is no
  `base + header + index*scale` sequence to strength-reduce, and a
  pointer-increment form would buy nothing on x86-64. The only cost near the
  load is the two `MOV`s that put the array and index into the emitters' fixed
  RAX/RCX.
* **Nothing is spilled around the access itself.** The one frame store per
  iteration (#6) is `flush_scratch_registers()` at a branch arm, and its reload
  is already elided to a register move by `slot_mirror`. This is a *branch*
  artefact, not a register-allocation failure at the load.
* **`CMP ECX, [RAX+len]` is not a safe local peephole.** Folding
  `emit_bounds_check`'s two-instruction `MOV R10D,[RAX+len]; CMP ECX,R10D` into
  one memory-operand compare saves an instruction and four bytes and macro-fuses
  with the `JAE` — and it silently breaks the exception message, because
  `emit_bounds_check_stubs` in `jit/src/x64/deopt_stubs.rs` reads **R10D as
  `jit_throw_aioobe`'s `length` argument**. R10D is a live output of that
  sequence, not a scratch temporary. The correct version moves the length load
  into the cold stub; that is a two-file change (`arrays.rs` +
  `deopt_stubs.rs`) and was outside this task's scope. It is worth ~1
  instruction and 4 bytes on **every emitted bounds check**, i.e. everywhere the
  elision above does not fire — which is where `sort` and `string_scan` live.

## What was changed

**No codegen.** Two comment corrections in `jit/src/x64/arrays.rs`, and no kill
switch, because nothing behavioural was touched:

* `emit_bounds_check`'s doc said "header offset 12". The constant is **4** and
  has been since the header shrank to 16 bytes; the emitted bytes always took
  the named constant, so only the prose was stale. The doc now also records the
  R10D contract, so the next person to see the obvious peephole finds the reason
  it is wrong before they write it. (While confirming that value: the constant's
  own comment in `types/src/heap_types.rs` reads "8, not 12" directly above a
  value of `4`. That one is still wrong and was not in scope to fix.)
* `emit_null_check_arraylength`'s doc now records the loop-header meet above.

The header-offset inventory tripwire
(`header_offset_emission_site_inventory_matches_the_doc`) counts the constant
names as literal substrings across the whole backend, comments included, so both
comments were written to avoid the counted spellings. `arrays.rs`'s needle
counts are byte-identical to `HEAD`'s.

## What would close it, and what it is worth

In descending order of value per unit of risk. All four are **outside**
`arrays.rs`/`bce.rs`.

1. **Hoist the loop-invariant `arraylength`** — ~5 instructions of 21, the
   single largest item. The scaffold exists and is *deliberately inert*:
   `jit/src/x64/driver.rs` calls `crate::loop_analysis::find_invariant_loads` and
   throws the result away with a `TODO(round-12+)` saying the hoist needs
   safepoint / oop-map / regalloc participation. Note the scaffold targets
   `getfield`/`getstatic`; `arraylength` is not in it and would have to be added.
   The existing `LoopHoist` (aaload) and `FpLoopHoist` (dload) mechanisms are the
   shape to copy. Sizing: this is the real work and it is not small — a
   pre-header slot, the null check moved to the pre-header, and a
   bypassable-header veto (`find_bypassable_loop_headers` already computes that
   veto for the other speculating transforms).
2. **Seed the loop header's non-null IN set from the pre-header** — ~2
   instructions, and it composes with (1): once the length is hoisted the
   pre-header holds the only `arraylength`, so the header inherits the fact by
   construction. Lives in `jit/src/null_check_elim.rs`.
3. **Hoist the safepoint flag address** — ~2 instructions and 10 bytes per back
   edge. `emit_safepoint_poll` materialises a 64-bit absolute address with
   `MOV R11, imm64` at every poll. A RIP-relative `TEST BYTE [rip+disp32], 0xFF`
   is one instruction and 7 bytes and needs no register at all. Lives in
   `jit/src/x64/safepoint.rs`. Smallest change on this list, and it is paid by
   every loop in the VM, not only array loops.
4. **The vector/unroll gap** — the other ~13x. Out of reach of any of the above.

Doing 1-3 takes the body from ~21 instructions to ~9 and should move the
`char[]` row from 5.2 ns to somewhere near 2. That is **44x -> ~15x** on this
probe and, because this is the floor, a proportional move on `sort` and
`string_scan`. It is not parity, and nothing on this list reaches parity.

## Reproducers

`probes/CharAtCostCurve.java` — the `char[]` rows are the isolated arm.

```sh
java     -cp probes CharAtCostCurve    # the oracle
cratonvm -cp probes CharAtCostCurve

# the bounds check IS eliminated on this shape; this is the control that proves it
CRATONVM_JIT_NO_BCE=1 cratonvm -cp probes CharAtCostCurve
```

That last arm is the first measurement this page owes. If `CRATONVM_JIT_NO_BCE=1`
makes the `char[]` row materially worse, the elision traced above is firing and
the trace is confirmed from the outside. If it changes nothing, the trace is
wrong somewhere and the rest of this page's attribution needs re-reading before
it is acted on.

## Not determined

* **Which tier compiled the measured `scanArr`.** Everything above is the
  single-pass backend (`jit/src/x64/`). `jit/src/ir_lower.rs` is a second x64
  emitter with its own array element emitters and its own header-offset
  inventory, and the sibling `charAt` page establishes that this probe's methods
  reach the backend through `CompileDoor::Osr`. BCE does run on that door — the
  analysis lives in `compile_with_param_slots`, which OSR reaches directly — but
  the register homes may differ (`jit/src/x64/osr.rs` masks some assignments back
  to frame slots), and every "zero-cost `CalleeSaved` push" row above becomes a
  frame load if `a` or `i` loses its home.
* **Whether the loop is natively unrolled.** Body size is 22 bytes, which the
  static heuristic bands at 2x, but `plan_native_unroll`'s legality gate was not
  traced. 2x would halve #18-#21 per element and change nothing else.
* **The colouring itself** — which of R12-R15/RBX each local actually gets, and
  whether all three fit.
* **`Bench.array_sum`'s own bytecode** was not read. Its 69x is quoted from the
  audit; whether its `iaload` also reaches `BoundsProof::Static`, and whether
  `detect_int_array_sum` diverts it into the AVX2 reduction in `simd.rs`
  instead, are both open. The `char[]` trace does not transfer to it for free.
