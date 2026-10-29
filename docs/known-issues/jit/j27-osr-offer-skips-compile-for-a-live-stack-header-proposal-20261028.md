> **j27 wave 5 (lane OSRRETIRE, 2026-10-28): FIXED IN CODE, PENDING HOST VERIFICATION.**
> `try_osr_offer` (vm/src/runtime/interpreter.rs) now calls
> `osr_live_stack::retire_live_stack_osr_header` (new
> `vm/src/runtime/interpreter/osr_live_stack.rs`) right after the method-wide
> budget and BEFORE both compile arms: a frame whose operand stack is non-empty
> is retired with the existing `charge_osr_rejection(.., OSR_MAX_ATTEMPTS)`
> (same state as the `published_but_unenterable` arm, incl. the method-wide
> refused-activation charge), counted as `osr_refused_entry` (no new metrics
> row), and the offer returns `Skip`. Switch `CRATONVM_JIT_OSR_LIVE_STACK_RETIRE`
> (token `osr-live-stack-retire`, default ON, `=0` restores the old offers; read
> only when the stack is live). **Stack at the offer = stack at the header:**
> every back-edge site offers after the branch landed (the `if*`/`if_icmp*`
> arms pop their operands before `cond_branch_arm!`, the fused compare forms
> never push them, `goto`/decoded handlers carry none). **Sync path
> (`CRATONVM_BG_COMPILE=0`):** it used to spend five charged offers on
> `try_osr`'s `!frame.stack.is_empty()` refusal (no compile); it now retires on
> the first offer too, so both modes take one decision in one place; `try_osr`'s
> check stays as the door's own contract. Tests (crate `cratonvm-vm`):
> `runtime::interpreter::osr_live_stack::tests::{only_a_live_stack_retires_and_an_empty_one_never_reads_the_switch,
> a_live_stack_header_is_retired_by_its_first_due_offer,
> the_offer_retires_a_live_stack_header_before_both_compile_arms}`. Verify: the
> unit tests, then `tools/probes/jit/j27/J27OsrAsmRows.java` in `--jdk-only`,
> `--compatible`, `CRATONVM_BG_COMPILE=0`: facts unchanged vs HotSpot; under
> `CRATONVM_DBG_JITC=1` stderr has `osr live-stack retire J27OsrAsm.<m>` lines
> (depth=1,1,2) and no `osr bg request J27OsrAsm.` line; with `=0` the request
> lines return.

# Proposal: retire a live-operand-stack loop header before the background OSR request

Status: PROPOSAL (lane OSRASM, j27 wave 5, 2026-10-28; not implemented, no build)
Area: `vm/src/runtime/interpreter.rs` `try_osr_offer` (background arm)

## Evidence (read, not measured)

A back-edge target whose operand stack is non-empty (`tools/probes/jit/j27/J27OsrAsmRows.java`:
`refOnStack`, `handlerHeader`, `mixedStack`; javac never emits one, but other bytecode producers can)
can never be OSR-entered: both tiers seed locals only. Every door says so:
`try_osr` refuses at `!frame.stack.is_empty()`, `validate_osr_entry` refuses
`OSR_REFUSE_OPERAND_STACK`, the single-pass emitter writes `-1` at the pc
(`bytecode_walk.rs`, operand-stack-live), and `emit_osr_entry_stubs` refuses the bci
("operand stack not empty at this bci").

Under the default `CRATONVM_BG_COMPILE`, `try_osr_offer` asks none of them before it queues the
background OSR task. The first due offer therefore runs a whole OSR task (the optimizing route's
graph build, then a full single-pass compile, published) to learn what the frame already shows; the
compile's `None` (`can_osr_enter` false) also charges one `osr_fail_count`. The next due offer then
retires the header through `published_but_unenterable`. If the single-pass compile bails outright on
the shape instead, up to `MAX_TIER_FAIL_RETRIES` (3) such tasks run before `Refused` retires it.

Bounded (one to three wasted compiles per method and install epoch), so not a defect.

## What would fix it

In the background arm of `try_osr_offer`, before `request_osr_classified`: if
`!thread.frames[*frame_idx].stack.is_empty()`, record `osr_published_but_unenterable`-style metrics
(say `osr_live_stack_header`), `charge_osr_rejection(.., OSR_MAX_ATTEMPTS)` and return `Skip`. The
stack depth at a pc is fixed by verified bytecode, so the verdict holds for every arrival at that
header. Default ON with an `=0` kill (`CRATONVM_JIT_OSR_LIVE_STACK_RETIRE`); verify with
`J27OsrAsmRows` under `CRATONVM_DBG_JITC=1`: no `osr bg request` line for the three `J27OsrAsm`
methods, facts unchanged.
