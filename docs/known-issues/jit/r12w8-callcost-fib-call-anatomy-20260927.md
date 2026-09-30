# `CratonBench fib`: what one compiled call executes, against HotSpot

Status: OPEN (reading page; two items landed this wave, the rest is listed under "What is left")
Area: optimizing tier call protocol (`jit/src/ir_lower.rs`: `emit_prologue`, `emit_self_recursive_call`, `emit_epilogue`, `emit_safepoint_poll`)
Severity: MEDIUM (performance: `fib` 5.5 s against HotSpot 2.1 s; no wrong answer)
Found by: round 12 wave 8 lane callcost

## Which tier runs `fib`

`fib`'s first compile is already the optimizing pipeline (`tier=C1 optimized=true`,
`docs/internal/performance/c2-fib-per-call-budget-20260912.md` section 5), and every
recursive call in that body is a kind-4 direct `CALL rel32` to its own offset 0
(`Lowerer::patch_self_calls`), so a recursion that enters the IR body stays in it.
The interpreted top frames of `fib(44)` enter the newest body at each call, so all
but a few milliseconds of the 5.5 s run in the IR body. That body splices
`IR_RECURSIVE_INLINE_MAX_COPIES = 2` levels of `fib` into itself (6 copies, 7 nodes
of the call tree per activation, 8 leaf self-calls).

About half of the compiled ACTIVATIONS are base cases: activations are entered at
tree depth 3k, and about half of the nodes at any depth band of a Fibonacci tree
are leaves (`n <= 1`).

## Today (before this wave), read from the emitters, Win64

Offsets are the IR tier's; `r` is a register from the five-register file.

**Callee prologue** (`lower_inner_attempt` pad + `emit_prologue` + the entry poll):

```text
nop dword [rax+rax]          ; ENTRY_PATCH_PAD (not-entrant patch site)
push rbp ; mov rbp,rsp ; sub rsp,F
mov [rbp-..],rbx ... x5      ; callee-saved GP saves (ls_saved_gp: every register
                             ; the body's linear scan hands out; fib uses all five)
mov [rbp-ctx],rcx            ; the VM context
mov [rbp-8],rdx              ; n's prologue word (its home since F13-2)
mov qword [rbp-thr],0        ; thread slot (kept for a lean body: null-guarded readers)
mov qword [rbp-spid],0       ; safepoint-id slot (F15-4, not landed)
mov gs:[rbp_mirror],rbp      ; frame record, RBP half
mov dword gs:[cm_mirror],id  ; frame record, identity half
test byte [rip+flag],0FFh    ; entry poll ...
jz .clear                    ; ... TAKEN on every call, over the inline slow path
mov r,[rbp-8]                ; Op::Param (alias: one load, no copy)
```

About 20 instructions, 11 of them stores, and one taken branch.

**Each leaf self-call** (`Op::Call` arm + `emit_self_recursive_call`):

```text
movzx eax,sp ; cmp eax,F+16 ; ja .ok     ; sampled stack guard, TAKEN (guard inline)
mov rcx,[rbp-ctx]                        ; context
lea edx,[r-1]                            ; the argument (or a move)
call fib                                 ; offset 0 of this body
mov gs:[rbp_mirror],rbp                  ; RBP republish (identity elided, X1)
cmp rax,1 ; jno .keep                    ; sentinel test, TAKEN (cold path inline)
mov r',rax                               ; result into its register (home dropped)
```

About 10 instructions and two taken branches. The id store before the call is gone
(F14-1, `CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID`).

**Epilogue** (`Op::Return` + `emit_epilogue`): `mov rax,r`, five restores,
`add rsp,F`, `pop rbp`, `ret` -- 9 instructions.

**Totals.** A full activation (large `n`: 7 nodes, 8 leaf calls) is about
20 + 30 (splice compares, arguments, phi moves, adds) + 8 x 10 + 9 = ~140
instructions. A BASE-CASE activation (`n <= 1`) pays the whole prologue, one
compare, `mov rax`, and the whole epilogue: ~32 instructions and 11 stores to
return its argument.

## HotSpot 25 (C2, `MaxRecursiveInlineLevel=1`)

```text
mov [rsp-0x14000],eax        ; stack bang
push rbp ; sub rsp,0x10
cmp esi,1 ; jle .base
...                          ; fib(n-1) inlined once: 2-3 calls per activation
mov [rsp],esi ; lea esi,[rsi-1] ; call fib ; mov esi,[rsp] ...
add rsp,0x10 ; pop rbp
cmp rsp,[r15+poll] ; ja .slow ; ret
```

About 10-12 instructions per call: no callee-saved file (C2 spills the two live
values around its calls), no frame record, no sentinel test (exceptions unwind by
return address), no per-call stack sample (the bang is in the callee), no entry poll
(it polls at the return).

## Landed this wave

1. **The entry fast return** (`r12w8-callcost-entry-fast-return-FIXED-20260927.md`,
   `CRATONVM_JIT_IR_ENTRY_FAST_RETURN`): after the pad, `CMP edx, 1; JLE
   .fast_return`, and `.fast_return: MOV RAX, RDX; RET` after the body. A base-case
   activation drops from ~32 instructions (11 stores) to 5 (none). The full
   activation pays one extra, macro-fused, not-taken `CMP/Jcc`.
2. **Dominated self-call samples** (`r12w8-callcost-dominated-self-call-samples-FIXED-20260927.md`,
   `CRATONVM_JIT_IR_SELF_CALL_SAMPLE_DOMINATED`): the second call of each spliced
   `fib` copy follows the first on every path at the same RSP, so it emits no sample:
   4 of 8 leaf samples per full activation, 12 instructions and 4 taken branches.

Expected by counting: a base-case activation saves ~27 instructions and a full one
~10, over roughly equal counts of each: 15-25% fewer instructions per `fib` call and
about half the stores. Expected `CratonBench fib`: 5.5 s -> about 4.3-4.7 s. The
first measurement to take is `R12CallcostFib`'s `fib32` line with each switch at 0.

## What is left (ranked; each is a proposal in `jit-r11-fib-proposals.md`, "Round 12 wave 8")

* **F8-1** the callee-saved file on a full activation (5 saves + 5 restores): save
  only on the paths that make a call (shrink-wrap to the first call-bearing block),
  or a caller-saved allocation for values not live across calls.
* **F8-2** the entry poll's taken `JZ` (outline the method-entry poll only; the
  outline machinery is `CRATONVM_JIT_IR_POLL_OUTLINE`, which also moves back-edge
  polls and so stays off).
* **F8-3** the sentinel test's taken `JNO` after every self-call (an out-of-line
  sentinel path; the callee-deopt service reads live register state, so it must be
  re-emitted, not moved).
* **F8-4** F14-2 (the parameter straight into its register in the prologue) and
  F15-4 (no sp-id zeroing in a pure recursive body; blocked on the not-entrant
  finding below).
* A lazy post-self-call RBP republish is NOT sound:
  `r12w8-callcost-not-entrant-stub-needs-the-callers-frame-record-FIXED-20260928.md`.

## How to confirm

* `CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=CratonBench.fib`: after the
  five-byte NOP, `cmp edx,1` (`cmp esi,1` on SysV) and `jle`, then `push rbp`; one
  `movzx eax,sp` per spliced copy, not two.
* `jit/src/ir_lower.rs` `r12w8_callcost_tests` (executed bodies).
* `R12CallcostFib` in the default arm and with each switch at 0: identical count
  lines, `bad 0`, and the `fib32` time.

## Round 13 wave 1 (lane callcost)

**Landed: F8-2, the method-entry poll outlined** (`jit/src/ir_lower.rs`
`emit_safepoint_poll`, switch `CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE`, default on). The entry
poll (`back_edge_block == None`, no mode exit) now takes the existing outlined path
(`outlined_polls` / `emit_outlined_polls`): `TEST byte [rip+flag]; JNZ .slow` falls through
on every call, and the ~40-byte slow path (map, body id, `CALL`, shadow reload, `JMP` back)
moves after the body. The back-edge polls keep their inline shape (their FP reloads are not
emitted by the outlined block, which is why `CRATONVM_JIT_IR_POLL_OUTLINE` stays off). Every
IR body's entry changes: one taken branch fewer per call and a shorter prologue span. Tests:
`r13_the_entry_poll_is_outlined_and_stops_exactly_as_the_inline_one` (census, set flag stops
once in both shapes, clear flag never stops); `an_outlined_safepoint_poll_stops_when_the_inline_one_does_and_not_otherwise`
now holds the new switch at 0 in both arms so its all-inline comparison is unchanged.

**What is left** (unchanged in substance; each is a proposal in
`jit-r13-callcost-proposals-RETIRED-20260929.md`): F8-1 (shrink-wrap the callee-saved file), F8-3 (out-of-line
sentinel test after a self-call), F8-4 (parameter straight into its register; F15-4 still
blocked -- the line-number half of the not-entrant finding has an exact patch in
`r13w1-callcost-not-entrant-innermost-native-pc-patch-FIXED-20260928.md`, the mirror half stands).
Measure first: `R12CallcostFib` `fib32` with `CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE=0` against the
default, interleaved.

## Round 13 wave 3 (lane loopcall)

No code landed for F8-1 / F8-3 / F8-4 this wave; each was taken as far as reading can take it
and turned into an exact design (proposals LC-3, LC-4, LC-5 in `jit-r13-loopcall-proposals-RETIRED-20260929.md`).
Why each stopped where it did, so the next round does not re-derive it:

* **F8-1 (shrink-wrap the callee-saved file) buys `fib` nothing on its own.** With the entry
  fast return, an activation that reaches the prologue has `n >= 2`. Those with `n >= 4` make
  leaf self-calls on every path, so every save is needed. Those with `n` in `{2, 3}` (the
  spliced copies bottom out in base cases, no call) are the only call-free activations -- but
  their paths still WRITE the callee-saved registers the linear scan handed the spliced values,
  so the saves cannot move past them. What would help is the other half of the item: allocate
  values that are not live across a call to caller-saved registers (`regalloc.rs`,
  `ir_gp_file`), after which a call-free path writes no callee-saved register and the saves can
  sink into the first call-bearing block. That is a register-allocator change (not this lane's
  file) plus a per-exit "were the saves made" discipline in every exit this file emits (the
  epilogue, `emit_deopt_stub`, `emit_call_exc_stub`, the rethrow pads, the entry fast return):
  proposal LC-4 lists the exits.
* **F8-3 (out-of-line sentinel after a self-call) is a deferred emission with captured
  state, not a move.** The cold side of `emit_self_call_marshal_and_call` reads lowering
  state that is only valid AT the site: `deferred_self_call_sp_id` (a value -- capturable),
  the identity republish (state-free), the callee-deopt service's argument staging
  (`gp_load_value(RAX, arg)` for each argument: register residency / home of each node AT the
  call), `emit_pending_shadow_copy_back` (the site's pending publication set, and it calls
  `invalidate_ref_residency`, which changes how the rest of the body is lowered), and
  `push_self_call_exc_patch` (reads `cur_bci`, and on a protected bci resolves a frame state
  from the current node's locations). The outlined-poll machinery (`outlined_polls`,
  `emit_outlined_polls`) shows the pattern (capture `spill_high_water`, `defined_nodes`,
  `cur_bci` at the site; reinstate them around the deferred emission). A safe first cut
  (proposal LC-3) admits only a reference-free body (`lean_ref_free_prologue()`: no pending
  shadow words, no ref residency), a site whose bci is unprotected, and arguments that are
  `int`/`long` with a HOME word (staged from the home, not from a register), and records the
  call-exception patch at the site with the site's bci; everything else keeps the inline
  shape. It must also stay ahead of the OSR facts `lower_inner_attempt` computes from
  `call_exc_patches` (~35102): the site's own sentinel exit already records the same bci, so
  the fact is unchanged, but the patch row itself has to be pushed only once the code exists.
* **F8-4 / F14-2 (the parameter straight into its register)** is the `Op::Param` arm, which
  is not in this lane's regions, and it is not a two-line change: proposal LC-5 has the design
  and the one condition that makes it sound (the linear scan may hand the parameter's register
  to a value defined EARLIER in block 0, so a prologue-filled register is only still the
  parameter if nothing lowered before the `Op::Param` node wrote it). The saving is one
  store-forwarded reload per activation; the entry poll's slow path (a helper `CALL`, now
  outlined) and, in a body that publishes roots, the thread fetch's helper both clobber the
  ABI register between the prologue and the `Op::Param` node, which is why the fix has to fill
  the register in the PROLOGUE and have the arm skip its load, not the other way round.
* A smaller taken branch this lane found on the way: the sampled stack guard `MOVZX EAX, SP;
  CMP EAX, span; JA .ok` is TAKEN on every sampled self-call (4 of 8 per full activation after
  wave 8's domination elision). Its slow side has the same state dependencies as the sentinel
  path minus the argument staging, so it is the natural first user of LC-3's deferred
  emission.

Measure first (unchanged): `R12CallcostFib` `fib32`, interleaved, each round-12/13 switch at 0.

## Round 13 wave 5 (lane callcost2)

No fib code landed this wave; what was read and why each remaining item stopped:

* **F8-3 (sentinel after a self-call) and the sampled stack guard's taken `JA`.** Both are
  "move the cold side after the body". Re-read against wave 3's LC-3 design: the cold side
  cannot be emitted later from inside `emit_self_call_marshal_and_call` alone. It needs a
  per-compile list of deferred cold blocks (a new `Lowerer` field) and a flush after the body
  in `lower_inner`, next to `emit_outlined_polls` -- both outside this lane's regions -- and
  `push_self_call_exc_patch` reads `cur_bci` at emission time, so the deferred block needs a
  bci-taking variant. Proposal CC2-1 in `jit-r13-callcost2-proposals-RETIRED-20260929.md` is the three-part
  change (field, flush, emitter) with the admission LC-3 already worked out. Beyond the taken
  branches it moves ~100 bytes of cold code per self-call site out of the hot span (8 sites in
  `fib`'s body), which is likely worth as much as the branches.
* **F15-4 (no sp-id zeroing in a lean prologue)** lost its only named blocker:
  `r13w1-callcost-not-entrant-innermost-native-pc-patch` is in the tree and now has its unit
  test (this wave). It still owes an audit of every Rust call a lean body can make before its
  first safepoint (proposal CC2-4); one prologue store per non-base activation is not worth a
  wrong line in a trace without it.
* **F8-1 / F8-4** are unchanged (register allocator; the `Op::Param` arm), see wave 3's
  section.
* **The post-self-call RBP republish** stays, for a second reason recorded on
  `r12w8-callcost-not-entrant-stub-needs-the-callers-frame-record-FIXED-20260928.md` (the Rust doors
  walk from the mirror too).

Measure first, unchanged: `R12CallcostFib` `fib32`, interleaved, each round-12/13 switch at 0
(`CRATONVM_JIT_IR_ENTRY_FAST_RETURN`, `CRATONVM_JIT_IR_SELF_CALL_SAMPLE_DOMINATED`,
`CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE`, `CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID`), so the next
round knows which of the landed items the 3.1-3.3 s already includes.

## Round 13 wave 8 (lane callcost3)

**Landed: CC2-1, the deferred cold tails of a direct self-call** (`jit/src/ir_lower.rs`,
switch `CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS`, default on, read per site). Both cold halves of
`emit_self_recursive_call` now sit after the body, entered by an inverted branch that is NOT
taken on the hot path:

```text
movzx eax,sp ; cmp eax,F+16 ; jbe .guard_tail      ; was: ja .ok (taken) over the guard call
mov rcx,[rbp-ctx] ; lea edx,[r-1] ; call fib
mov gs:[rbp_mirror],rbp
cmp rax,1 ; jo .sentinel_tail                      ; was: jno .keep (taken) over ~150 bytes
mov r',rax                                         ; .keep
...
.guard_tail:    [sp id] ; mov rcx,[rbp-ctx] ; call guard ; test ; jne exc ; jmp .ok
.sentinel_tail: [sp id] ; [identity] ; <callee-deopt service, staged from the sources
                captured at the call> ; cmp rax,1 ; jno .keep ; <dispatch_threw peek> ;
                jne exc ; jmp .keep
```

How it was made safe (the obstacles waves 3 and 5 recorded):

* **Admission** (`self_call_cold_tail_admitted`, wave 3's LC-3): a body that publishes no roots,
  no pending shadow publication at the site (so the inline copy-back would emit no code; its
  residency bookkeeping is replayed at the site), an unprotected bci under
  `ir_self_call_states_unread_enabled` (so the exit is a plain call-exception row, no reason-9 pad),
  and no level-2 machine list. Anything else keeps the inline shape, byte for byte.
* **Argument staging** for the callee-deopt service: the source each `gp_load_value(RAX, arg)`
  would read at the call (a constant, a resident register of the callee-saved file, or a home word)
  is captured at the site (`deferred_self_call_service_args`), and the tail replays exactly that
  instruction. An argument a carry still names declines the deferral. Nothing between the site's
  `Jcc` and the tail writes a register or a frame word, so the tail runs in the inline code's state.
* **Call-exception rows**: the site's bci and monitor fact are taken at the site
  (`note_deferred_self_call_exit`); the rows are pushed by the flush
  (`emit_deferred_self_call_tails`), which runs right after the block loop in `lower_inner_attempt`,
  BEFORE the OSR facts and `emit_call_exc_stub` read `call_exc_patches`. The facts see the same
  (bci) set as before.
* `StackOverflowError` is raised exactly where it was (the guard helper at the self-call's bci,
  with the deferred sp id stored first); GC maps at the call are unchanged (the map, the shadow
  push and the reload are all still emitted at the site); a callee deopt is serviced by the same
  sequence and resumes at the same `.keep`.

Tests (`r12w8_callcost_tests`, executed bodies, both arms of the switch):
`r13w8_self_call_cold_tails_leave_both_branches_falling_through` (the `JBE`/`JO` shapes, fib 0..24),
`r13w8_a_deferred_guard_runs_and_returns_to_the_call` (20 000 frames on a 64 MiB thread: the guard
runs from the tail and returns to the call), `r13w8_a_deferred_guard_trip_propagates_through_every_deferred_sentinel_path`
(a trip deep in the recursion unwinds through the deferred sentinel path of every frame above it),
`r13w8_the_deferred_callee_deopt_service_stages_the_calls_argument` (a service answering from the
staged argument: `d(n) = n + 1000` proves the captured source was the call's argument).

**Expected**: the 8 leaf self-calls of a full `fib` activation each lose a taken `JNO`, the 4
undominated samples each lose a taken `JA`, and ~1.2-1.5 KB of cold code (≈150 bytes per sentinel
path, ≈40 per guard path) leaves the hot span, which now fits the uop cache far better. Estimate
3-8% on `CratonBench fib` (2.95 s -> about 2.75-2.85 s). Measure: `bench13.sh` fib, default
against `CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS=0`, interleaved; `R13Callcost3Fib` `fib32`.

**Read and not changed** (what the brief's other candidates would buy, from the emitted shape):

* *One stack check per frame against the floor word.* The per-site sample is now 3 instructions,
  macro-fused and not taken (4 per full activation). Moving it into the callee's prologue (LC-8)
  would leave the SOE raised with the CALLER innermost but without the caller's safepoint id: a
  reference-free body's self-call defers that store to its cold paths (`CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID`),
  and `jit_self_call_stack_guard` builds the error (and its trace) inside the helper. An exact
  floor compare (`CMP RSP,[thread+floor]`) costs a TLS load and a memory compare, more than the
  sample. Proposal CC3-2 in `jit-r13-callcost3-proposals-RETIRED-20260929.md`.
* *Redundant spills across a self-call*: none. Values live across the call are in the callee-saved
  GP file (saved once per activation), the result's home is dropped (`home_is_one_store_rax`), and
  the argument is read from its register.
* *The callee's argument registers passed directly*: the marshal is `mov rcx,[rbp-ctx]` plus one
  register move per argument (eliminated at rename); the callee side is F8-4 / LC-5 (unchanged).
* *Frameless base case*: already the entry fast return (round 12 wave 8).

**What is left**: F8-1 (callee-saved file; register allocator), F8-4 / LC-5 (parameter straight
into its register), F15-4 / CC2-4 (sp-id zeroing, needs its audit), CC3-1..3 in
`jit-r13-callcost3-proposals-RETIRED-20260929.md` (the same deferral for cross-call / inline-cache exits and for
bodies with roots). Status stays OPEN (performance gap: fib 1.4x HotSpot before this wave).

## Round 13 wave 10 (lane callcost4)

No fib code landed this wave, and none could land in this lane's files: `fib`'s time is in its
IR body (`jit/src/ir_lower.rs` prologue, self-call and epilogue emitters, and the linear scan's
register handout), and this lane owns the single-pass tier's call emitters
(`jit/src/x64/op_invoke.rs`, `frames.rs`), which `fib` leaves after its first compile. What
was checked and what it leaves:

* **Wave 8's cold tails have not been timed.** The last `bench13.sh` fib numbers are w6c
  (2.94-3.05 s against HotSpot 2.10-2.18 s, `bench-w6c.txt`); CC2-1
  (`CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS`) landed in w8 and no bench run since includes it.
  That is the first thing to measure, before any further call-protocol work (arms below).
* **Largest remaining item, by counting: F8-1, the callee-saved file.** A full activation saves
  and restores five GPRs (10 memory operations of the ~130 instructions) because the linear
  scan hands out all five registers of `ir_gp_file()`, and `saved_gpr_regs` saves every
  register the scan ever handed out (`ls_saved_gp`). HotSpot keeps two values live across
  its calls. Two cheaper cuts than the full "caller-saved allocation for call-free values" of
  wave 3's LC-4 are recorded as proposal CC4-3 (`jit-r13-callcost4-proposals-RETIRED-20260929.md`): a
  handout order that reuses an already-saved register before opening a new one, and
  counting how many distinct registers `fib`'s body actually needs at once (the first step,
  a disassembly read, costs nothing).
* F8-4 / LC-5 (parameter straight into its register) and F15-4 / CC2-4 (sp-id zeroing) are
  unchanged: one store-forwarded reload and one store per non-base activation, both in
  `ir_lower.rs`, the second still owing its audit.
* The static and virtual call chains a `fib`-shaped program also makes (non-self calls) go
  through `emit_direct_cross_call` / the inline caches in the IR tier and through
  `walk_invokestatic` / `walk_invoke_instance` in the single-pass tier; `R13Callcost4CallChains`
  (new) times a static chain, a virtual chain and an interface chain next to `fib` so a
  regression in either tier's call protocol shows up beside the self-call numbers.

**Measure** (orchestrator; interleaved, `$TAG` = the wave binary's tag):
```
cd /c/craton/jitr13-probes
./bench13.sh 3 bench-w10-fib.txt $TAG $TAG:CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS=0 \
  $TAG:CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS=0,CRATONVM_JIT_IR_ENTRY_FAST_RETURN=0,CRATONVM_JIT_IR_SELF_CALL_SAMPLE_DOMINATED=0,CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE=0,CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID=0 hs
./prep13.sh R13Callcost4CallChains
PROBES="R13Callcost4CallChains R13Callcost3Fib R12CallcostFib" ./run13.sh <exe> cc4-chains-def
PROBES="R13Callcost4CallChains R13Callcost3Fib R12CallcostFib" CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS=0 ./run13.sh <exe> cc4-chains-ct0
```
Read `fib=` per arm (median of three) and the probes' `fib32` / chain timing lines; answers
(`sum-*`, `bad 0`) identical across arms. Status stays OPEN (performance gap: fib ~1.4x
HotSpot at w6c).

## Round 13 wave 11 (lane callcost5)

**Landed: CC4-3, the kept values take the first registers of the file** (`jit/src/ir_lower.rs`
`ls_certain_refusals`, switch `CRATONVM_JIT_IR_LS_PREPIN_REFUSED`, default ON, read per compile).
This goes at F8-1 from the allocator's side, without changing the save protocol. The
prologue saves every register a KEPT value holds (`ls_saved_gp`). The scan's handout was
already first fit (lowest free register in file order, intervals in start order), so it never
opened a new register while a used one was free. But it handed registers to values
`plan_register_residency` then refuses: every constant (`skip_const`), every value read once
that no carry or crossblock rule keeps (`skip_single_use`), and references (the bank match).
In `fib` that covers the leaf calls' `n-1` / `n-2` arguments, the call results, the adds and
the constants. Each one held a low register while the kept values (`n`, the spliced copies'
parameters, the splice phis) were defined, and pushed them higher. Those values are now
pinned before the scan, which is exactly the refusal they were going to get, made before the
scan instead of after it. The kept values then pack into the lowest registers. The saves
drop to the number of kept values live at once, which is what the proposal's first step
counts.

Expected: `fib`'s prologue saves fewer than five GPRs, and each register dropped saves one
store and one load per full activation (the base case already skips the prologue). Not
measured (lanes do not run). **Measure** (orchestrator):
```
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=CratonBench.fib   # prologue saves, both arms
CRATONVM_DBG_IR_LINEAR_SCAN=1                                      # [ir-ls] resident= / skipped=, both arms
./bench13.sh 3 bench-w11-fib.txt $TAG $TAG:CRATONVM_JIT_IR_LS_PREPIN_REFUSED=0 hs
PROBES="R13Callcost5CalleeSaved R13Callcost3Fib R12CallcostFib" ./run13.sh $EXE cc5-def
PROBES="R13Callcost5CalleeSaved R13Callcost3Fib R12CallcostFib" CRATONVM_JIT_IR_LS_PREPIN_REFUSED=0 ./run13.sh $EXE cc5-pp0
```
The answers (`sum-*`, `bad 0`) must be identical in both arms. The `fib30` / `fib32` lines and
`fib=` are the timing.

**What is left** (unchanged otherwise):
* F8-1, the rest of it: a call-free path still writes the callee-saved registers its values
  hold, so the saves cannot sink (wave 3's LC-4: caller-saved allocation for values not live
  across a call, plus a "were the saves made" discipline in every exit).
* F8-4 / LC-5: the parameter goes straight into its register (the `Op::Param` arm and the
  prologue, which are not this lane's regions).
* F15-4 / CC2-4: no sp-id zeroing, which still owes its audit.
* The call result read once after the NEXT self-call (`fib(n-1)` across `fib(n-2)`) is refused
  by the pays rule unless `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1`. So it takes a home store
  after its call and a reload at the add. See proposal CC5-1 in `jit-r13-callcost5-proposals-RETIRED-20260929.md`.
Status stays OPEN (performance gap: `fib` ~3.2 s against HotSpot ~2.4 s at w9b).

## Round 14 wave 1 (lane calls)

**Landed: CC5-1, a register for a single-use value that crosses a call** (`jit/src/ir_lower.rs`
`single_use_crosses_a_call` / `ls_call_positions`, used by `ls_certain_refusals` and
`plan_register_residency_with`; switch `CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING`, default ON, read
once per residency plan). A value read once, of type `int`/`long`, whose home is one `store_rax`
(`op_home_is_one_store_rax`, or a direct self-call's result under
`ir_self_call_states_unread_enabled`) and whose live range has an `Op::Call` strictly inside it is
no longer refused by the pays rule (`skip_single_use`) and is not pre-pinned by CC4-3. In `fib`
that is `fib(n-1)`'s result, read by the add after `fib(n-2)`: it took a home store after its call
and a reload at the add; with a register the home drops (`value_home_droppable`) and the pair is
one register move. Admissions are charged to the same carried-reservation price as the crossblock
arm (`carried_displacement_price`, `crossblock_taken`), so a loop kernel's carried values are not
displaced by them -- the reason `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` itself stays off
(`mixNarrow`). The census line `[ir-ls] skipped:` gains `call_crossing=<k>`.

Soundness: this is a placement decision only. Every register of the IR GP file is callee-saved and
saved by the prologue, the publish and home-drop rules are the ordinary ones (a value with two
reads across calls has always been admitted by the same machinery), and a refused admission keeps
the home word as before. Test: `r14w1_a_single_use_value_is_a_call_crossing_candidate_only_across_a_call`
(the candidate predicate: crossing, twice-read, call at the range end, FP, no call, kill switch).

Cost side (why it is a switch and must be measured): a crossing value may need one more
callee-saved register while it is live, i.e. one more save/restore pair per full activation; the
saving is a store and a reload per crossing value per activation (in `fib`'s spliced body, one per
spliced call pair, ~4 per full activation). Expected neutral to a few percent on `fib`.

**Measure** (orchestrator, interleaved):
```
CRATONVM_DBG_IR_LINEAR_SCAN=1   # [ir-ls] skipped: ... call_crossing=  (>0 default, 0 with =0)
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=CratonBench.fib   # prologue saves, both arms
./bench14.sh 3 bench-w1-fib.txt $TAG $TAG:CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING=0 hs
PROBES="R14CallsCallCrossing R13Callcost5CalleeSaved R13Callcost3Fib R12CallcostFib" ./run14.sh $EXE calls-def
PROBES="R14CallsCallCrossing R13Callcost5CalleeSaved R13Callcost3Fib R12CallcostFib" CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING=0 ./run14.sh $EXE calls-cc0
```
Answers identical in both arms; `R11W15`-style `mixNarrow` / `RegPressure` must not move (the
budget keeps them byte-identical when their carried set fills the file).

**CC5-5 (shrink-wrap the call-free activations)**: not started; its first step is still the
disassembly of `CratonBench.fib` (which registers the `n in {2,3}` path writes), now to be read in
both arms of the new switch, since CC5-1 can change which registers the spliced copies hold. No
code is owed before that reading. **What is left**: F8-1 (the rest: caller-saved allocation for
values not live across a call, and a per-exit "were the saves made" discipline), F8-4 / LC-5
(parameter straight into its register), F15-4 / CC2-4 (sp-id zeroing, audit owed), CC5-5.
Status stays OPEN (performance gap: `fib` ~1.3x HotSpot).

## Round 14 wave 5 (lane callcost)

**CC5-5 as written (shrink-wrap the saves of the call-free activations) cannot pay on `fib`,
by construction.** With two spliced levels the call-free activations are `n` in `{2, 3}`.
Every call-free path of the spliced body joins a call-bearing path at a splice's result phi
(`r1 = (n-1 < 2) ? n-1 : fib''(..) + fib''(..)`) before the method's single `Op::Return`, so the
one epilogue must restore the full save set on both paths. A save sunk past the entry has to
be re-inserted on the call-free edge into that phi, which is exactly the path it was meant to
spare; `n`'s own register is written in block 0 on every path anyway. The only way through is
duplicating the post-merge tail per save state (most of the body) or the register-allocator
half of LC-4. Neither is contained.

**Landed instead: the entry fold** (`jit/src/ir_lower.rs` `emit_entry_fold` /
`plan_entry_fold` / `entry_fold_eval` / `emit_entry_fold_stub`, switch
`CRATONVM_JIT_IR_ENTRY_FOLD`, default ON, read per compile). A call-free activation is a pure
function of its argument, so the compiler evaluates it: for the first (up to four) arguments
past the entry fast return, it walks the blocks the body would run (block 0, each `If` on its
condition, phi edges as parallel assignments, to an `Op::Return`), with Java `int` semantics
(`entry_fold_int_op`: wrapping `+ - *`, masked shifts, signed `Cmp`), and gives up at the first
node that is not pure (`entry_fast_return_may_skip`: any call, load, store, division, guard,
allocation) or not evaluable. The values it gets are answered before the prologue:

```text
nop5 ; cmp edx,2 ; jl .fast_return          ; round 12 wave 8
cmp edx,3 ; jle .fold                       ; new: 2 <= n <= 3
push rbp ; ...                              ; n >= 4: unchanged
.fold: mov rax,2 ; mov r11,1 ; cmp edx,2 ; cmove rax,r11 ; ret
```

Admission: an entry fast return on `<=`/`<` (so every argument below the range already left),
a direct self-call in the body (the frames removed are a recursion's), a loop-free CFG, no MIR
mode / int poisoning (inherited from the fast return). Why nothing observable is skipped is the
fast return's argument extended from one block to a path (the doc comment of `emit_entry_fold`):
only pure nodes, no loop so no skipped poll, the stub writes RAX/R11/flags only, offset 0 is still
the not-entrant pad. A static-field read, a division, or any call on the path declines it (the
probe's `stat` and `divz` phases).

Expected on `fib`: in the call tree of `fib(N)` the arguments 1, 0, 2, 3 occur F(N), F(N-1),
F(N-1), F(N-2) times out of about 2F(N+1), so `n` in `{2, 3}` is about 30% of the calls (the
compiled activations sample every third tree level, which keeps roughly that mix). Each drops from the full prologue (5 saves, context and parameter
stores, frame record, zeroing, entry poll), the spliced compares and adds, and the epilogue -- about
45 instructions and 12 stores -- to 9 instructions (the two compares, their branches and the stub) and no store. The `n >= 4` activations pay one
macro-fused, not-taken `CMP/JLE`. By counting, 15-25% fewer instructions per `fib` call. Not
measured (lanes do not run).

Tests (`r12w8_callcost_tests`, executed bodies): `r14w5_the_call_free_activations_past_the_fast_return_are_folded`
(`fib` with its call-free pair written out: `CMP n,3; JLE` after the fast return, one `CMOVE`,
fib(-3..24) in both arms), `r14w5_the_fold_stops_at_its_cap_and_answers_past_it` (four values,
three selects, answers on both sides of the cap), `r14w5_a_body_whose_first_value_calls_folds_nothing`
(plain `fib` without splices: unchanged), `r14w5_the_folded_int_ops_have_java_semantics`.

**Measure** (orchestrator, interleaved):
```
CRATONVM_DBG_IR_ENTRY_FOLD=1          # [ir-entry-fold] folded n in [2, 3] -> [1, 2] for CratonBench.fib
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=CratonBench.fib   # the second CMP/JLE, the .fold stub
./bench14.sh 3 bench-w5-fib.txt $TAG $TAG:CRATONVM_JIT_IR_ENTRY_FOLD=0 hs
PROBES="R14CallcostEntryFold R12CallcostFib R13Callcost3Fib" ./run14.sh $EXE cc-def
PROBES="R14CallcostEntryFold R12CallcostFib R13Callcost3Fib" CRATONVM_JIT_IR_ENTRY_FOLD=0 ./run14.sh $EXE cc-fold0
```
If the census says `declined` for `CratonBench.fib`, the reason names the first node on the
`n = 2` path that is not pure: that is the next thing to read (a splice may carry a node the
unit-test bodies do not).

**What is left**: F8-4 / LC-5 (parameter straight into its register), F15-4 / CC2-4 (sp-id
zeroing, audit owed), the register-allocator half of F8-1 / LC-4, and a caller-side fold
(proposal CE-1 in `jit-r14-callcost-proposals.md`: answer a direct self-call whose argument is
in the fast-return or fold range without the `CALL`). Status stays OPEN (performance gap).

## Round 14 wave 6 (lane callcost2)

**Landed: CE-1, a direct self-call answered in the CALLER** (`jit/src/ir_lower.rs`
`emit_self_call_answer` / `plan_self_call_answer_site` / `self_call_answer_source`, the plan
recorded by `emit_entry_fast_return` in the new `Lowerer::self_call_answer`; switch
`CRATONVM_JIT_IR_SELF_CALL_ANSWER`, default ON, read per compile). After a site's stack sample
and before its marshal, the site asks the question the callee's entry would ask, on the same
32 bits of the same argument, and when the entry would answer without a frame, answers here:

```text
movzx eax,sp ; cmp eax,F+16 ; jbe .guard_tail    ; the sample, unchanged (kept: it may dominate others)
cmp c,3 ; jg .call                               ; c = the argument's register, or R10 from its home
mov rax,2 ; mov r11,1 ; cmp c,2 ; cmove rax,r11  ; the fold's table (n in {2, 3})
mov r11,c ; cmp c,2 ; cmovl rax,r11              ; the fast return (n < 2 returns n)
jmp .keep
.call: mov rcx,[rbp-ctx] ; mov edx,c ; call fib ; mov gs:[mirror],rbp ; cmp rax,1 ; jo .sentinel_tail
.keep: <store the result>
```

Without a fold the answer is `CMP c, imm; J<not cc> .call; MOV RAX, <value>; JMP .keep` (any
fast-return condition, a constant or ANY parameter's argument as the value).

Why it is the call's result, and why the join is sound, is the doc comment of
`emit_self_call_answer`; in short: the callee is this body, and for these arguments its first
instructions are these compares and moves into RAX followed by `RET`; the answer path has no
safepoint, helper, trap or store; at `.keep` the call path has clobbered every caller-saved
register, so the lowering state claims none of them, and the answer path writes only RAX, R10,
R11 and the flags; the state updates the call path makes before `.keep` only forget
(`invalidate_ref_residency`, `rcx_twin`, `note_rcx_written`) or widen (the monitor fact). The
sample stays BEFORE the answer: a site whose sample dominates another site
(`plan_self_call_sample_elision`) must sample on its answer path too, or the dominated call would
run unchecked. Admission: a reference-free body (no shadow publication spans the call), no
carried argument (an RAX carry must meet the marshal with nothing emitted in between), the
compared argument in a register or a home word (a constant declines: rare, and it would want a
compile-time answer instead), a context-taking body, no MIR mode, arity equal to the
parameter count. A not-entrant patch at offset 0 is not consulted on the answer path, exactly as
it is not by the spliced copies of the callee the body already contains.

Expected on `fib`: every leaf self-call with `n - 1` or `n - 2` in `{0..3}` (most of them: the
call tree's arguments 0..3 are ~80% of its nodes) drops the context load, the argument move,
`CALL`, the callee's two `CMP/Jcc`, the stub, `RET`, the RBP republish and the `CMP/JO`: about
14 instructions, a call/return pair and one store, for ~9 instructions (one taken `JMP`). The
calls that remain pay one macro-fused `CMP/JG`, taken. Estimate 10-20% on `CratonBench fib`; not
measured (lanes do not run).

Tests (`r12w8_callcost_tests`, executed bodies, both arms): `r14w6_a_self_call_the_entry_would_fold_is_answered_in_the_caller`
(one `CMOVL` per site, fib(-3..24)), `r14w6_the_answer_covers_the_fold_up_to_its_cap_and_calls_past_it`
(the four-value fold, arguments past it still call), `r14w6_the_answer_is_the_argument_the_fast_return_names`
(`acc(n, a)`: the fast return answers the SECOND parameter; wrapping sums). The wave-5 fold
tests now hold the switch at 0 so their counts stay the entry stub's.

**Measure** (orchestrator, interleaved):
```
CRATONVM_DBG_IR_SELF_CALL_ANSWER=1   # [ir-self-call-answer] bci N: answered n <= 3 (...) for CratonBench.fib
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=CratonBench.fib   # the CMP/JG + CMOV block before each marshal
./bench14.sh 3 bench-w6-fib.txt $TAG $TAG:CRATONVM_JIT_IR_SELF_CALL_ANSWER=0 hs
PROBES="R14Callcost2SelfCallAnswer R14CallcostEntryFold R12CallcostFib" ./run14.sh $EXE cc2-def
PROBES="R14Callcost2SelfCallAnswer R14CallcostEntryFold R12CallcostFib" CRATONVM_JIT_IR_SELF_CALL_ANSWER=0 ./run14.sh $EXE cc2-ans0
```
If the census says `declined: an argument is carried` or `... resident in a scratch register`
for fib, that is the next thing to read.

**F8-4 / LC-5 (the parameter straight into its register): not landed.** Its two halves are the
`Op::Param` arm of `lower_data_node` and `emit_prologue`, neither in this lane's regions, and its
one soundness condition (nothing lowered before the `Op::Param` node in block 0 writes the
parameter's assigned register -- the linear scan may hand it to a value whose interval ends
first) needs either the per-register write tracking or a planner pin, i.e. the register
allocator. What it saves is one store-forwarded reload per non-base activation; CE-1 removes most
of the activations that paid it. Recorded as proposal CC6-2 in `jit-r14-callcost2-proposals.md`.

**What is left**: the register-allocator half of F8-1 / LC-4, F8-4 / LC-5, F15-4 / CC2-4 (sp-id
zeroing, audit owed), CE-2..4 (`jit-r14-callcost-proposals.md`), and the new book's items.
Status stays OPEN (performance gap).

## Round 14 wave 7 (lane callcost3)

**Landed: the entry fold evaluates THROUGH direct self-calls** (`jit/src/ir_lower.rs`
`entry_fold_self_call`, called from `entry_fold_eval`; memo and budgets in `EntryFoldCalls`,
`ENTRY_FOLD_MAX_DEPTH` = 8, `ENTRY_FOLD_MAX_ACTIVATIONS` = 64; switch
`CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS`, default ON, read per compile). Until now the fold's
evaluation stopped at the first node that is not pure, and every `Op::Call` is not pure, so
plain `fib` folded nothing (`n = 2` already calls `fib(1)` and `fib(0)`), and with two spliced
levels it folded at most `n` in `{2, 3}`. A direct self-call whose compared argument is a known
`int` (and whose every other argument is the caller's own parameter in the same position) is
now evaluated as this same body for that argument, memoized per compile. So `fib` folds
`n` in `2..=5` -> `[1, 2, 3, 5]` (the cap of four values), the entry stub answers those
without a frame, and CE-1 (wave 6) answers every direct self-call whose argument is `<= 5` at
the call site, because it reads the same table (`SelfCallAnswer::fold`).

Why that is the call's result: the call enters this same body at offset 0, and the evaluation
walks exactly the path the callee would run, under the same purity rule; a folded activation
is therefore still a pure function of its argument. What it drops besides what the wave-5 fold
already drops (the entry poll of a loop-free body) is the callee's frames and their sampled
stack checks, i.e. a possible `StackOverflowError` a few frames earlier -- the same freedom
every inlining takes. A recursion that never returns (`k(n)` calling `k(n)`, or a climbing
argument) gives up (argument in progress / depth past 8), as does any impure node on a callee
path (the probe's `thrower`, `stat`).

Expected on `fib` (not measured, lanes do not run): the real CALLs are now made only with
arguments `>= 6` (before: `>= 4`). In the call tree that removes about `phi^2 ~ 2.6x` of the
full activations (prologue, 5 saves, frame record, entry poll, epilogue); each answered site
grows from one select to three (`CMOVE` per folded value). Estimate 25-40% on `CratonBench fib`.

**Landed: CC6-2 / F8-4 / LC-5, the parameter straight into its register**
(`emit_prologue` + `plan_param_register_fills` + `param_register_filled`, and the
`Op::Param` arm of `lower_data_node`; switch `CRATONVM_JIT_IR_PARAM_REGISTER_FILL`, default ON,
read per compile). After storing the ABI registers to the prologue words, the prologue emits
`MOV dst, abi` for each `int`/`long` register parameter with an assigned GP register and marks
it live (`mark_gp_reg_live`); the `Op::Param` arm then skips `publish_gp_from_slot`'s reload
while `dst` still holds it (`resident_gpr == dst` and `gp_reg_owner[dst] == id`). No planner pin
was needed: every write of a GP-file register by the lowering is a publish through
`mark_gp_reg_live` / `ls_publish` / `ls_drop_residency`, which takes the previous owner's bit
away, so a value the linear scan hands `dst` before the parameter's interval starts clears the
fill and the arm reloads as before. The admission keeps that bookkeeping trivially complete:
the parameter is in block 0 and only `Op::Param` / `Op::Const` nodes precede it there; between
the fill and the arm only the rest of the prologue (RAX/R10/R11, memory) and the entry poll
(C-ABI helper, callee-saved file preserved) run. Off under a MIR mode, the int poisoning mode,
and `CRATONVM_JIT_IR_PARAM_HOME_ALIAS=0`. Saves one store-forwarded `MOV r, [rbp-8]` per
activation that reaches the prologue.

**CC6-1 (one compare for a sibling pair of answered self-calls): not landed; not contained as
written.** At the dominated site the compare can be skipped only on a path that KNOWS the
dominating site answered, and that knowledge ends at the dominating site's `.keep`, which is
a join with its call path. Carrying it past the join means duplicating everything lowered
between the two sites on the answer path -- the first site's `store_rax(slot)` publish, the
second site's `emit_safepoint_map` / `alloc_slot` / deferred sp-id bookkeeping, any node
scheduled in between -- with the lowering state (residency, home stores, deopt names) valid at
both joins: the same hazard review7 is examining at CE-1's `.keep`, doubled. The saving is one
macro-fused, predicted `CMP/JG` per answered pair. The fold through self-calls above takes the
calls themselves instead (it widens what each site answers from `<= 3` to `<= 5`); the answered
pairs that remain (the spliced copies' leaves) still pay the one extra compare. Recorded as
CC7-3 in `jit-r14-callcost3-proposals.md` with what a contained form would need.

Tests (`r12w8_callcost_tests`, executed bodies, both arms):
`r14w7_the_fold_evaluates_self_calls_and_folds_fib_to_its_cap` (plain `fib`: `CMP n, 5; JLE`,
three stub selects, and with CE-1 three per site; fib(-3..24)),
`r14w7_a_self_call_with_its_own_argument_or_a_growing_one_folds_nothing`,
`r14w7_the_prologue_fills_the_parameter_register_and_the_reload_goes` (one `MOV r, [rbp-8]`
fewer; fib(-3..24)). The wave-5/6 tests that count the fold's selects on bodies whose calls
are now evaluated hold `CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS=0`.

**Measure** (orchestrator, interleaved):
```
CRATONVM_DBG_IR_ENTRY_FOLD=1        # [ir-entry-fold] folded n in [2, 5] -> [1, 2, 3, 5] (...; self-calls answered: K) for fib
CRATONVM_DBG_IR_PARAM_FILL=1        # [ir-param-fill] filled (node, reg): [(n, r)]
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=CratonBench.fib   # CMP n,5/JLE; MOV r,rdx after the stores
./bench14.sh 3 bench-w7-fib.txt $TAG $TAG:CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS=0 $TAG:CRATONVM_JIT_IR_PARAM_REGISTER_FILL=0 hs
PROBES="R14Callcost3FoldCalls R14Callcost2SelfCallAnswer R14CallcostEntryFold R12CallcostFib" ./run14.sh $EXE cc3-def
PROBES="R14Callcost3FoldCalls R14Callcost2SelfCallAnswer R14CallcostEntryFold R12CallcostFib" CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS=0 ./run14.sh $EXE cc3-fc0
PROBES="R14Callcost3FoldCalls R14Callcost2SelfCallAnswer R12CallcostFib" CRATONVM_JIT_IR_PARAM_REGISTER_FILL=0 ./run14.sh $EXE cc3-pf0
```
If the census says `declined` or stops at `n=2` for `CratonBench.fib`, its reason names the
first impure node on a spliced path (a splice may carry a node the unit bodies do not).

**What is left**: the register-allocator half of F8-1 / LC-4, F15-4 / CC2-4 (sp-id zeroing,
audit owed), CE-2..4 (`jit-r14-callcost-proposals.md`), CC6-1 in the contained form CC7-3
describes, and the new book's items (a wider fold table, CC7-1). Status stays OPEN (performance
gap).
