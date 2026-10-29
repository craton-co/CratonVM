# Promoted loop bodies keep their values in stack homes and poll every trip: 4-9x HotSpot's instructions

> **j27 wave 8 (lane W8LOOP, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.**
> **(1) Wider unroll for a register-only body.** New opt-in switch `CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1`
> (`jit/src/ir_optimize/unroll_wide_pure.rs`, wired at the `main_post_unroll_loop` call in `ir_optimize.rs`).
> If no node of the body can deopt (`ir_lower::node_cannot_deopt`, the `frames == None` route), the main loop
> gets the largest factor up to 8 that keeps `copies * body <= UNROLL_MAX_BODY` (48), and never less than the
> base factor of 4. An explicit `CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR` overrides it. A body that can deopt keeps
> the base factor. HotSpot unrolls these kernels 8-16x. With the j27 default `CRATONVM_JIT_IR_LINEAR_REASSOC`, the
> eight copies of a promoted accumulation fold back to one add per cell.
> **(2) The poll.** The page form (`CRATONVM_JIT_IR_PAGE_POLL`) refuses every poll that has a mode exit. Every
> row in the table below has one: they are method-entry or OSR compiles. Filed as a proposal:
> `j27-page-poll-with-a-mode-exit-proposal-20261028.md`.
> **Tests.** `cratonvm-jit --lib ir_optimize::unroll_wide_pure::tests`, three tests:
> * `j27_w8_wide_pure_takes_the_widest_factor_the_budget_holds`
> * `j27_w8_wide_pure_never_narrows_and_leaves_a_deopting_body_alone`
> * `j27_w8_wide_pure_is_opt_in_and_yields_to_an_explicit_factor`
>
> **Probe.** `tools/probes/jit/j27/J27W8WideUnrollRows.java`. Its expected HotSpot 25 lines are in its header.
> They cover trip counts on both sides of every factor (0..17), and the licence refusing near
> `Integer.MAX_VALUE` and `MIN_VALUE`.
> **Verify (host, both modes).**
> * Facts: every `fact` line must equal HotSpot in the default arm and in the `=1` arm.
> * Engagement: under `CRATONVM_DBG_UNROLL=1`, look for `main/post factor 4 -> 8 (wide pure)` on `sumInt`,
>   `twoCells` and `sumSq`.
> * Decision: run the icount table below with default vs `CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1`, and also
>   `-Dj27.time=true`. Flip the default to ON only if every row improves and no row is
>   `[ir] acceptance ... refused` under `CRATONVM_DBG_JITC=1` in the `=1` arm that is accepted in the default arm.

> **j27 wave 5 (lane UNROLLRES, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Two changes.
> **(1) Residency on unrolled loops.** Under `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY=1`, the residency rule now has a
> defined loser on a main/post unroll: the main loop's carried phis keep their registers, and the remainder's
> phis yield (`regalloc::loop_residency::eviction_rank`). Details:
> `j27-loop-phi-residency-misses-unrolled-pressure-loop-FIXED-20261029.md`.
> **(2) The poll.** A two-instruction back-edge poll, `CRATONVM_JIT_IR_PAGE_POLL=1` (opt-in). It is the page form
> (`jit/src/ir_lower/ir_page_poll.rs`), and it replaces the five-instruction thread-word poll on back edges
> without a mode exit. That does NOT include the method-entry bodies' back edges in this table, which carry a
> mode exit. Details: item 3 of `j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md`.
> **Host.** The icount table below, default vs each `=1`. Check which rows carry a mode exit first
> (`CRATONVM_DBG_JIT_DISASM`: a page-form poll is `test dword ptr [r11+...], eax`).

> **j27 wave 2 (lane RESIDENCY, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** The register-residency lever is in
> code, OPT-IN: `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY=1` makes the IR linear scan's spill cost loop-aware
> (`jit/src/regalloc/loop_residency.rs`): a loop invariant no longer shares the loop-carried relief with the phis, and
> a value redefined every trip (phi, unrolled temporary) weighs its definition too and loses next-use ties, so under
> the five-register System V file the phis and cells of `run` / `runStatic` keep whole-range registers (homes then
> dropped by the existing `value_home_droppable` / `phi_home_droppable`) and the invariants go to memory operands.
> The `x + c` rematerialisation is not built. Description, unit tests, probe
> (`tools/probes/jit/j27/J27LoopResidencyRows.java`), expected lines and the flip rule:
> `j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md`'s RESIDENCY block. Host: the icount table below,
> default vs `=1`, plus `CRATONVM_DBG_IR_LINEAR_SCAN=1`'s phi census on `run` (are the cells `no_register` today?).

> **j27 wave 2 (lane STRIPMINE, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** The poll-every-trip lever is in
> code, OPT-IN: `CRATONVM_JIT_IR_STRIP_MINE=1` strip-mines the pure main loop of a main/post unroll so it polls once
> per `CRATONVM_JIT_IR_STRIP_MINE_ITER` (default 1000) main trips (`jit/src/ir_optimize/strip_mine.rs`; the lowering
> skips the inner latch's poll in `Lowerer::emit_safepoint_poll`). It is meant to reach the method-entry rows
> (`counterInt`, `twoCells`, `run`, `runStatic`; confirm with `CRATONVM_DBG_UNROLL=1` -- a carried memory phi at the
> main header is refused); NOT the OSR-body rows (`counterInt(1_000_000)`, `twoCells(1_000_000)`), whose loop
> is the OSR-entered post loop. Register residency (lever 2) is still not built. Full description, probe
> (`tools/probes/jit/j27/J27StripMineRows.java`), expected lines and the flip rule:
> `j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md`'s wave-2 status block. Host: the icount table
> below, default vs `CRATONVM_JIT_IR_STRIP_MINE=1`.

> **j27 wave 1 (lane LOOPCG, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Two of the page's three levers
> are in code, both default ON with an `=0` kill; register residency of the remaining temporaries and a
> strip-mined / every-N-trips poll are NOT built (proposal
> `j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md`).
> (1) `CRATONVM_JIT_IR_LINEAR_REASSOC` (`jit/src/ir_optimize/linear_reassoc.rs`, run in `optimize_passes`
> after the post-promotion unroll): an `int` / `long` add/sub/neg/constant-scale tree whose interior nodes
> have one reader and no snapshot name is rebuilt as `sum(k * leaf) + c` when that takes strictly fewer nodes --
> `acc + i + (i+1) + (i+2) + (i+3)` -> `acc + (i << 2) + 6`, `c + 3 + 3 + 3 + 3` -> `c + 12`, `run`'s
> `v += k*S + i` x4 -> `v + (kS << 2) + (i << 2) + 6`. Removes the three homed temporaries per main trip of
> `counterInt` outright. (2) `CRATONVM_JIT_IR_POLL_WORD_SELECT` (`jit/src/ir_lower/poll_word_select.rs`, wired in
> `Lowerer::emit_safepoint_poll`): the thread-word poll selects the byte's address into R11 (`JNZ rel8` over a
> `LEA R11,[rip+flag]` fallback) and tests once -- 5 instructions on the hot path instead of 6, same answer on
> both arms. Also `optimize_passes` now sets `promoted_cells` for the method-entry accumulator promotion, so its
> headers are offered to the after-promote static hoist / unroll too.
> Verify (host, `--jdk-only` and `--compatible`): `tools/probes/jit/j27/J27LoopcgReassocRows.java` -- every `fact`
> line equal to HotSpot 25 (expected lines below) in arms default, `CRATONVM_JIT_IR_LINEAR_REASSOC=0`,
> `CRATONVM_JIT_IR_POLL_WORD_SELECT=0`; engagement `CRATONVM_DBG_IR_COMPILES=1` prints `[ir] linear reassoc: N
> tree(s), A node(s) -> B` for `counterInt` / `twoCells` / `invariantScale` (if it never prints for the unrolled
> kernels, the main-body copies are snapshot-named -- per-copy frames -- and the pass leaves them; that is the
> next thing to look at). Decision: the table above through the icount harness, default vs each `=0`; unit tests
> `cargo test -p cratonvm-jit j27_loopcg`.
>
> Expected HotSpot 25 output of `J27LoopcgReassocRows` (deterministic: modelled from Java `int` / `long` wrap-around arithmetic):
> `fact warm -5615701443778440464`; for n = 0 / 1 / 3 / 7 / 1000 / 1000000:
> `sumInt` 0 / 0 / 3 / 21 / 499500 / 1783293664; `sumFrom` 5 / -2147483630 / -2147483607 / 5 / 5 / 5;
> `subChain` 1 / 0 / -8 / -48 / -999999 / 727379969; `invariantScale` 0 / 1592 / 4869 / 11795 / 17076500 / 1039528736;
> `sumLong` 9223372032559808512 / 9223372032559808513 / 6532028343110300738 / 8830710280122805696 /
> 8806117241942794884 / 8357281332585869984; `counterInt` = `sumInt`; `twoCells` 0 / 21 / 66 / 154 / 21000 / 21000000.

Status: OPEN (filed 2026-10-26 by the retirement of `docs/internal/jit/jr21-typecheck-lane-perf-rows-RETIRED-20261026.md`,
rows 4 and 5). Not a promotion defect: every loop below IS promoted (its cells live in registers, its stores are
sunk to the exit) and unrolled; what is left is the code the lowering emits for the promoted body.
Area: `jit/src/ir_lower.rs` (register residency of loop phis and unrolled temporaries, the back-edge poll),
`jit/src/ir_optimize.rs` (the unroller's factor; no reassociation of an unrolled accumulation).
Found by: the deterministic in-process instruction count (`tools/probes/icount2.sh`'s protocol, a `TcuCountQ` port of
the round-21 kernels), host 64.177.43.150.

## Numbers

User-mode instructions of the Java thread per call, compile-quiesced, median of 3 (spread 0.0 %), `--jdk-only`,
default switches, 1000-iteration calls unless named:

| kernel (probe it comes from) | CratonVM | HotSpot 25 | ratio |
|---|---|---|---|
| `run`: `a.v += k * SCALE + i; a.w += bias` on a fresh `Acc` (`R20PLoopStaticBarrier`) | 13 619 | 1 485 | 9.2x |
| `runStatic(n, kk, b)`, same body with parameters (`R21TypecheckStaticInit`) | 19 027 | 1 353 | 14x |
| `counterInt`: `COUNTER += i` (`R20PLoopStaticCells`) | 6 433 | 1 403 | 4.6x |
| `counterInt(1_000_000)`, the OSR body | 6 250 164 | 1 438 508 | 4.3x |
| `twoCells`: `COUNTER += 3; OTHER ^= i` | 8 705 | 2 287 | 3.8x |
| `twoCells(1_000_000)`, the OSR body | 8 500 178 | 2 251 186 | 3.8x |

`CRATONVM_JIT_IR_REG_ALLOC=1` (round 25 wave 7, opt-in) moves only `runStatic` (19 027 -> 17 035).

## What the body is

`counterInt`'s method-entry C2 body, main loop (unrolled by 4, one trip shown, `CRATONVM_DBG_JIT_DISASM`):

```
cmp ebx,r13d ; jge exit
lea eax,[r14+rbx] ; mov [rbp-60h],rax        ; acc + i            -> stack home
lea eax,[rbx+3]   ; mov [rbp-80h],rax        ; i + 3              -> stack home
lea eax,[rbx+2]   ; mov [rbp-88h],rax        ; i + 2              -> stack home
lea eax,[rbx+1]   ; mov rcx,rax
mov rax,[rbp-60h] ; add eax,ecx
mov rcx,[rbp-88h] ; add eax,ecx
mov rcx,[rbp-80h] ; add eax,ecx
mov r14,rax ; lea ebx,[rbx+4]
mov r11,fs:[poll word] ; test r11,r11 ; je .g ; test byte [r11],0FFh ; jmp .t ; .g: test byte [rip+flag],0FFh ; .t: jne slow
jmp head
```

About 27 instructions for four iterations. `run`'s body has the same shape with the two cells' values also in stack
homes (`[rbp-118h]`, `[rbp-120h]`), reloaded and stored every trip, and `w += bias` added four times rather than
`w += 4 * bias`. HotSpot's C2: the cells and temporaries in registers, a one-instruction poll (`test [r10], eax`) per
main trip, unrolled 8-16x with the accumulation reassociated (`counterInt` is ~1.4 instructions per iteration).

## What would make it go away

* The unrolled temporaries and the loop's carried values register-resident across the trip (the round-25 register
  allocator's increments 3, 4, 5 and 7, not built: `docs/internal/jit-proposals/r25-w7-ir-register-allocator-RETIRED-20261025.md`).
* A back-edge poll of one or two instructions: the thread word's TLS mirror (`CRATONVM_JIT_POLL_THREAD_WORD`) loads,
  null-tests and falls back to the global byte on every trip.
* Reassociation of an unrolled accumulation (`acc + i + (i+1) + (i+2) + (i+3)` -> `acc + 4i + 6`), and a wider
  unroll for a register-only body; both must get past the acceptance gate (`[ir] acceptance ... refused` under
  `CRATONVM_DBG_JITC`), which has discarded unrolled and reassociated bodies before.

Decision rows: the table above, through the same count harness; a fix is done when each kernel is within 1.5x of
HotSpot's count with every `fact` of `R20PLoopStaticBarrier`, `R20PLoopStaticCells` and `R21TypecheckStaticInit`
equal to HotSpot's.
