# Page-form back-edge poll for polls that carry a mode exit (proposal)

**Status:** PROPOSAL (filed by j27 wave 8, lane W8LOOP, 2026-10-28). Not implemented.
**Area:** JIT, IR lowering (`jit/src/ir_lower/ir_page_poll.rs` `shape_admits`,
`jit/src/ir_lower.rs` `Lowerer::emit_safepoint_poll`), forced exits
(`jit/src/not_entrant.rs` `force_exit_polls`, `CompiledMethod::exit_poll_sites`).

## Why

`CRATONVM_JIT_IR_PAGE_POLL=1` (j27 wave 5) turns a back-edge poll into two
instructions (`MOV R11, seg:[cell]; TEST [R11+base], EAX`). It refuses every poll
with a mode exit, because a redefinition that withdraws the body forces its exit
by rewriting the poll's `Jcc` condition byte (`exit_poll_sites`, `JNZ` -> `JNO`),
and the page form has no `Jcc`. Every admitted method-entry compile carries a mode
exit on its back edges (interpreter round i1 wave 18, `TEST AL, 2`), and so does
every OSR-tier compile. That includes all six rows of the table in
`jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`. So the page
form reaches none of them, and they keep the five-instruction word-select poll.

## Options

1. **A patchable two-byte NOP in front of the `TEST`.** Emit `66 90` before the
   `TEST`. A forced exit rewrites it to `EB rel8`, a jump to a nearby `JMP rel32`
   to the slow block. The hot path is then three instructions. Costs: the
   trampoline must sit within 127 bytes, and the two-byte store must be atomic
   (it must not cross an 8-byte boundary: align it). The cross-modifying-code
   argument is the same one the condition-byte patch already relies on.
2. **Exit by arming the page.** The forced exit arms the stop page for the
   threads that may run the withdrawn body. The slow path's helper then answers
   "leave" for a withdrawn body. The patch is permanent, but an armed page is
   not: a thread that has not reached its poll when the page is disarmed would
   keep running the withdrawn body. So the page would have to stay armed until
   no activation of the body remains. That is a VM-side lifetime question.

Option 1 is local to the lowerer and `not_entrant.rs`. Option 2 touches the
safepoint protocol.

## Triage recipe (before building either)

Count how many back-edge polls each row has, and how many of them carry a mode
exit. Use `CRATONVM_DBG_JIT_DISASM=1` on `R20PLoopStaticCells counterInt` and
`R20PLoopStaticBarrier run`, under `CRATONVM_JIT_IR_PAGE_POLL=1`. A page-form
poll shows as `test dword ptr [r11+...], eax`. If none of the rows has a
page-form poll, the poll is still in each row's instruction count, and option 1
is worth building. Also time a forced exit (`J27DeoptLoopRows`, class
redefinition mid-loop) before and after the change.
