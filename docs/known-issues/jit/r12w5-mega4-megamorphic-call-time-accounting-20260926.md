# Where a megamorphic call's time goes: dispatch is about a fifth of it

Status: OPEN
Area: JIT calls (inline caches, hashed stub, shared megamorphic table, OSR bodies, call protocol)
Severity: MEDIUM (performance; no wrong answer found)
Found by: round 12 wave 5 lane mega4

## The question

`R12Mega3VirtualSlots` on the wave-4 binary: mega24 100 ms, mega48 124, sites8
231, wide48 180, iface24 100, against HotSpot 25's 20 / 16 / 51 / 24 / 27. The
numbers are the same with `CRATONVM_JIT_MEGA_CLASS_SLOTS=0`, and
`CRATONVM_DBG=mic-prof` counts only 637 `jit_invoke_virtual_mic` entries and
`mic_mega_table=0` for the whole run. (That dump is also printed at exit, by
`vm-cli/src/main.rs`'s `mic_prof::dump_now()`, so it covers the whole run.) The
hot loops never reach the Rust helper, so the gap is in the machine code.

This page accounts for that machine code by reading. The short answer is that
the megamorphic dispatch path (the PIC cascade, the stub, the class cell or the
shared table) is roughly 40 of the roughly 185 instructions one iteration of
`mega` runs. Most of the rest is the compiled call protocol, paid twice per
iteration because the loop makes two compiled calls (`eval` and the static
`Node.want`), where HotSpot makes one call and inlines everything else.
Speeding up the probe the wave-4 work improved (class cell against shared
table) cannot show up in these timings, and it does not.

`C:\craton\jitr12-probes\src\R12Mega4OneSite.java` (this wave) measures each
part separately.

## 1. What runs the loop

`mega(Node[], int)` is invoked 10 times in total (5 reps × 2 phases), each time
with a 1 000 000-iteration loop. It never reaches a method-entry threshold. The
loop runs in an **OSR body**:

- The optimizing OSR door (`jit_bridge.rs::osr_optimizing_body_ready` /
  `build_osr_optimizing_artifact`, default on) is asked first. It needs the IR
  body to pass `ir_evidence::accept`. Because the loop contains a call, the body
  is not `ir_osr_sentinel_free`, so it also needs the exception-exit and
  guard-exit admissions (`CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS` and
  `_GUARD_EXITS`, both default on). Those need every guard exit to be
  single-scope, exact and `REEXECUTE`. A null check left inside the spliced
  `want` (`n.evalTag`) would fail that. Whether the check survives
  `null_check_elim` cannot be settled by reading.
- Otherwise the single-pass door runs (`jit_bridge.rs::compile_osr_body` →
  `x64::compile_with_param_slots`). That compile passes
  `std::collections::HashMap::new(), // inline_sites` (`jit_bridge.rs` ~5314).
  **A single-pass OSR body splices nothing**, so `Node.want(node, i)` is a real
  compiled call on every iteration.
- `CRATONVM_DBG_JITC=1` tells the two apart. The single-pass door prints
  `[cratonvm-jitc] OSR-compile …`, the optimizing route prints `osr optimizing …`
  lines, and `maybe_dump` tags the body `osr/sp` or `osr/ir`.

In the single-pass case, the static call to `want` is bound through an OSR
**retire cell** (`lib.rs::retirable_osr_direct_call`). A retire cell "admits
only the body its caller baked" (`JitCycleEdgeCell::admits`, `lib.rs` ~18113).
The OSR body therefore calls the `want` body that existed when it was compiled
(usually C1, from the door's eager callee compile) for the whole run. A later C2
`want` is never entered from it. A method-entry single-pass compile normally
splices a 20-byte static like `ref` into `want` (`MaxInlineSize` budget; not
verified for the eager-callee door's compile), so here the cost is mostly the
call itself rather than C1 against C2. For a larger callee it is both.

## 2. The per-call dispatch path, by arm (Win64, receiver + 1 int, no stack word)

Single-pass site with an inline PIC (`op_invoke.rs`, `pic_inline` needs
`n + 1 <= ARG_REGS.len()`). The PIC arm emits no MIC. The counts are
instructions executed on the path.

| receiver served by | path | ~instr |
|---|---|---|
| PIC way k (0..3) | `MOV R10,pic`; hoisted marshal (ctx + n loads); `TEST/JZ` null; `MOV EAX,[recv]`; (k+1)×`CMP/JNE`; `MOV R11/TEST/JZ/BTR`; `JC/JMP`; `CALL R11`; republish (2 TLS stores); sentinel `CMP/JNO`; `JMP .done` | 18 + 2k |
| per-site hashed way | all four PIC compares miss (15), then the stub (`runtime_lowering.rs::emit_hashed_vtable_stub_body`): receiver reload, null, kind screen, class-id reload (6); `IMUL/SHR/SHL` + `MOV R10,imm64` (4); way 0 or 1 compare/load (5-8); tail: `BTR/JNC`, marshal again from the frame (3), `CALL R11`, republish, sentinel, `JMP` to the identity restore (1 store) and `JMP .done` | ~45 |
| class cell (M3-1) | as above to the hashed miss (~30), then `emit_mega_class_slot_probe` (24: column, root, directory, bound, array, bound, cell address, selector, key, compare, word), `JNZ tail`, tail (~12) | ~70 |
| shared table (W2-1) | as above to the hashed miss (~30); an unbound column costs 3 in the class probe; `emit_mega_dispatch_table_probe`: selector, table (6), hash (6), key (2), 1-4 way compares (3 each); tail | ~65-75 |
| HotSpot vtable stub | load klass, load vtable entry, `JMP [Method::from_compiled]` | ~5 |

The optimizing tier (`ir_lower.rs::emit_inline_cache_call`) adds a MIC compare
before its PIC (3 instructions), a kind screen even at `invokevirtual` sites
whose static type cannot be an array (the single-pass PIC skips it:
`ic_receiver_may_be_array`), and an `n`-load/`n`-store staging loop before the
stub on every megamorphic miss.

Dependent loads matter more than the counts. A per-site hashed hit is class id
→ hash → one load. A class-cell hit is PIC word → root → directory row → array
→ cell key/word, a four-deep chain. The root and directory loads do not depend
on the receiver and can issue early, but the chain is not shorter than the
shared table's (two loads behind a six-op hash). **So a class-cell hit is not
cheaper than a shared-table hit.** That is why
`CRATONVM_JIT_MEGA_CLASS_SLOTS=0` measured the same. The cells' real gain is
capacity: no set conflicts and no full sets. That shows up only when the shared
table overflows, which a 24- or 48-class probe does not do.

## 3. Which probe actually hits

`JitPICSlot::install` publishes into the per-site hashed ways FIRST and the
inline ways second (`inline_cache_pic.rs::install`:
`install_megamorphic_locked` then `install_way_locked`). So the first receivers
occupy a PIC way AND a hashed way. The helper also offers every resolution to
the shared table and to the receiver's cell (`helpers.rs::install_mega_dispatch_way`).
Class ids of one probe's leaves are close to consecutive, so Fibonacci hashing
spreads them evenly over the 8 two-way sets.

| phase | PIC | per-site hashed | class cell | shared table | Rust helper |
|---|---|---|---|---|---|
| mega24 (single-pass OSR) | 4/24 | ~12/24 (16 ways minus the 4 PIC duplicates, minus set overflow) | ~8/24 | 0 (cells answer first) | fills only |
| mega48 | 4/48 | ~12/48 | ~32/48 | 0 | fills only |
| sites8 | per `S_k`: as mega48; the cells are shared (one selector per loader) | | | | 8× the fills |
| wide48 (`n+1 = 5 > 4`: MIC only, no inline PIC) | MIC 1/48 | ~16/48 | ~31/48 | 0 | fills only |
| iface24 (`invokeinterface`, PIC present: column from the first receiver, W4-1) | 4/24 | ~12/24 | ~8/24 (all `Q*` add `area` at one depth) | 0 | fills only |
| any of them, `MEGA_CLASS_SLOTS=0` | same | same | 0 | the cells' share | fills only |

The class cells serve a third of mega24's calls and two thirds of mega48's. By
§2, switching them off moves those calls to a path of the same length.

## 4. What else one iteration of `mega` pays

For the single-pass OSR body, per iteration:

| item | ~instr | HotSpot C2 |
|---|---|---|
| loop: `(i*7 + (i>>>5)) % ns.length` (an `IDIV`, ~20-26 cycles latency), `aaload` with bounds and null check, the compare, `sum +=`, back-edge poll | ~27 | same work, `IDIV` included |
| megamorphic `eval`: weighted over §3's split (4/24 × 20 + 12/24 × 45 + 8/24 × 70) | ~49 | ~5 |
| caller-side safepoint protocol at each of the 2 calls: argument staging, `emit_pre_safepoint_spill_args_published` (reference homes plus the blind callee-saved spill), `sp_id` store, shadow push and reload, `.done` oop-map reload | ~2 × 12 | C2 spills live values around a call too, ~2-4 |
| `eval` body: C2 prologue (frame, param homes, reserved-slot zeroing, frame record: 2 TLS stores), spliced `ref`, epilogue | ~31 | inlined `ref`, one frame |
| `want`: retire-cell load/test/jz, `CALL`, C1 prologue (stack bang, frame, homes, frame record, `sp_id` init, shadow-slot zeroing, thread-TLS load, entry counter `ADD [R10],1`, entry poll), `getfield evalTag`, spliced `ref`, epilogue | ~45 | inlined: ~6 |
| total | **~185** | **~25-30** |

At an IPC of 2.5-3 that is 60-75 cycles, 17-22 ns per iteration, which is the
measured 20 ns (100 ms / 5M). HotSpot's 4 ns is about 15 cycles, bounded by the
`IDIV` and one real call.

By share of the iteration: megamorphic dispatch ≈ 40 of 185 (22%, the part
above what a PIC-way-0 hit would cost). The **second call, `want`, ≈ 45 (25%)**:
it exists only because the single-pass OSR body splices nothing. The **call
protocol** of the two calls (caller-side safepoint work, callee prologue and
epilogue) ≈ 90 (50%), overlapping the two items before it. HotSpot pays the same
protocol for its one real call in about a tenth of the instructions.

The same reading explains the other phases:

- **mega48 against mega24** (+24 ms, about +5 ns per call): twice the share of
  calls on the 70-instruction cell path. On top of that, 48 distinct callee
  bodies (more code and BTB footprint) and 24 more `eval` bodies to tier up
  inside the timed reps.
- **iface24 = mega24**: the loop's static call is `ref` directly instead of
  `want → ref`, so the second call is the same size. HotSpot's itable stub is
  slower than its vtable stub (27 against 20). CratonVM's interface path is the
  same stub as its virtual one, so the ratio looks better.
- **sites8**: eight OSR compiles, eight sets of per-site fills and eight
  interpreted warm-ups, all inside the timed region. HotSpot's ratio against
  mega48 (3.2×) is worse than CratonVM's (1.9×), so there is no
  megamorphic-specific defect there.
- **wide48**: no inline PIC at a Win64 receiver + 3 site, so the path is MIC then
  stub, with the stack-word marshal. `mix → mixRef → ref` is also more work per
  call on both VMs (HotSpot 24 against 16). The ratio (7.5×) is mega48's.

## 5. Findings

1. **The single-pass OSR door splices nothing.**
   `vm/src/runtime/interpreter/jit_bridge.rs::compile_osr_body` passes empty
   `inline_sites` and `inline_guard_variants` to `x64::compile_with_param_slots`
   (~5314). Every static, private or final helper called from an OSR loop is a
   full compiled call per iteration. Method-entry single-pass compiles splice
   them (`lib.rs` ~37156, `MaxInlineSize` / `FreqInlineSize` budgets). Fix:
   proposal W5-1 in `jit-r12-calls-proposals.md` (replay3's file plus the
   `lib.rs` planner).
2. **OSR retire cells pin the baked callee body for the life of the OSR body.**
   `lib.rs::JitCycleEdgeCell::admits` refuses anything but `pinned_entry`, so a
   callee's C2 body is never called from an OSR body compiled before it (C12-1 /
   W2-5, OSR edition). Proposal W5-3.
3. **The per-site hashed ways duplicate the inline ways' receivers.**
   `inline_cache_pic.rs::JitPICSlot::install` fills the hashed set before the
   inline ways. At a site whose cascade probes the inline ways (every IR IC
   site, and every single-pass site that fits the registers), up to 4 of the 16
   hashed ways hold receivers the cascade already answers: 25% of the per-site
   capacity. The duplicates are NOT waste at a single-pass MIC-only site
   (`pic_inline == false`: over-wide sites), where machine code never reads the
   inline ways. A blind reorder would regress those sites and break
   `tests.rs::test_jit_pic_slot_recompiled_target_retires_the_stale_way` /
   `test_jit_pic_secondary_cache_serves_overflow_and_clears` and
   `runtime_lowering`'s "the site's own hashed way is probed first" assertion.
   Fix shape: a per-slot "inline ways are probed" bit set by the emitter that
   emits the cascade, read by `install` (proposal W5-6). Worth about one cell hit
   per 24 receivers today. It matters where the next tier down is the Rust
   helper: `CRATONVM_JIT_HASHED_STUB_MEGA_TABLE=0`, a full shared set, or a
   column-less interface site.
4. **The stub re-derives what the cascade already knows.** At the PIC's final
   miss EAX holds the receiver's class id, and the receiver was null- and
   kind-screened (or cannot be an array). The stub reloads the receiver, tests
   it for null, screens its kind again and reloads the class id (6
   instructions, 2 loads). It also re-marshals arguments the PIC arm hoisted
   into the argument registers. The kind screen is also unconditional in the IR
   IC prefix and in the stub, where the single-pass PIC skips it for
   `invokevirtual` on a non-`Object` class (`ic_receiver_may_be_array`). Worth
   about 6-8 instructions per megamorphic call. Proposal W5-5.
5. **The class-cell probe is not faster than the shared table's** (§2), so
   M3-1's gain is capacity, not latency. To pay on latency, a site known to be
   megamorphic has to probe the cell FIRST, before the four PIC compares and the
   per-site hash. Proposal W5-4.
6. **`R12Mega3VirtualSlots` times warm-up, an `IDIV` and a second call along
   with dispatch.** Its per-phase numbers cannot isolate dispatch. That is why
   `R12Mega4OneSite` exists.
7. **The compiled call protocol is the largest single share** (§4, about half
   the iteration). It is not in the calls lane's files: the caller side is
   `x64/safepoint.rs` / `x64/op_invoke.rs`'s staging and spill hoist, and the
   callee side is `x64/frames.rs::emit_prologue` and `ir_lower.rs::emit_prologue`.
   Proposal W5-2 says how to price each item with switches that already exist.

## 6. How the orchestrator can confirm

Run `R12Mega4OneSite` (see its header) in these arms: default,
`CRATONVM_C2_ACCEPT=never`, `CRATONVM_C2_ACCEPT=always`,
`CRATONVM_JIT_OSR_OPTIMIZING=0`, `CRATONVM_JIT_MEGA_CLASS_SLOTS=0`,
`CRATONVM_JIT_HASHED_STUB_MEGA_TABLE=0` with `CRATONVM_DBG=mic-prof`,
`CRATONVM_JIT_SP_INLINE_MEGA=0`, `CRATONVM_JIT_THRESHOLD=1`; and once with
`CRATONVM_DBG_JITC=1` for the tier lines. This page's reading predicts:

- `static24` against `nocall24`: one compiled static call costs CratonVM 3-5 ns
  (about 7-10 ms per 2^21 iterations per rep). HotSpot's two lines are equal (it
  inlines).
- `mega24` against `static24`: the dispatch adds 1-3 ns. `mega64` adds a little
  more (more cell hits). `poly4` sits between `static24` and `mega24`.
- `rnd24` against `mega24`: the prediction share. If it is large, conditional
  mispredicts in the cascade matter, and W5-4's cell-first shape (fewer
  data-dependent branches) is worth more than instruction counts suggest.
- `MEGA_CLASS_SLOTS=0`: `mega24` and `mega64` unchanged (§2).
- `HASHED_STUB_MEGA_TABLE=0` with mic-prof: `mic_pic` rises by about the class
  cells' share of §3's table, which counts per arm which probe was serving.
  `SP_INLINE_MEGA=0`: every call past the PIC goes to the helper, so the
  `-best` times show the helper's price per call.
- `osr-*` against `ent-*`: with the single-pass OSR door, the OSR shape of
  `static24` pays the call and the entry shape (C2) splices it. That shows up as
  `osr-static24` far above `ent-static24`, which is finding 1 measured.

For `R12Mega3VirtualSlots` itself: `CRATONVM_JIT_THRESHOLD=1` removes most of
the warm-up. `CRATONVM_DBG_JITC=1` says whether `mega` ran `osr/sp` or `osr/ir`.
If `osr/ir`, finding 1 does not apply to it (the optimizing tier splices
`want`), and its time is the dispatch plus one call protocol.

## Orchestrator measurement (w4 binary `bb1addd1a`, 2026-09-27)

`R12Mega4OneSite`, default arm, best of 5 (ms), HotSpot 25 / CratonVM:

| phase | osr HotSpot | osr CratonVM | ent HotSpot | ent CratonVM |
|---|---|---|---|---|
| mono | 2 | 9 | 3 | 9 |
| poly4 | 4 | 12 | 5 | 14 |
| mega8 | 4 | 15 | 4 | 26 |
| mega24 | 4 | 36 | 5 | 58 |
| mega64 | 4 | 45 | 4 | 58 |
| rnd24 | 20 | 52 | 22 | 51 |
| iface24 | 5 | 36 | 5 | 59 |
| static24 | 1 | 5 | 1 | 5 |
| nocall24 | 1 | 5 | 1 | 5 |

All checksums agree with HotSpot (`R12Mega4OneSite OK`).

Reading of the numbers:

- `static24` equals `nocall24`, so the compiled call protocol itself is not where the time goes
  in this shape (F7 is refuted for it; the 5 ms floor is the loop and the table walk).
- The cost is dispatch, and it grows with the number of receiver classes (mega8 15 -> mega24 36
  -> mega64 45 in the OSR shape; 26 -> 58 -> 58 in the method-entry shape), which is what a
  sequential probe chain (PIC ways, then per-site hashed ways, then class cells / shared table)
  does and what a vtable load does not. About 17 ns per megamorphic call against HotSpot's ~2 ns.
- The method-entry (`ent-`) shape is slower than the OSR shape for the megamorphic phases.

So W5-4 (probe the class cell first at a site the profile calls megamorphic, HotSpot's vtable-stub
order) and W5-6 (no duplicated hashed ways) are the next steps, measured with this probe.

## Round 12 wave 6 (lane mega5)

Landed W5-4 and W5-6 from the reading above: a site whose PIC overflows (a
fifth live receiver class) is flagged in its `JitPICSlot`; its cascade then
jumps from after inline way 0 (single-pass) or the MIC (optimizing) straight to
the hashed stub, which probes the receiver's class cell FIRST, then the site's
own hashed ways, then the shared table. At a single-pass site the first four
receivers no longer take duplicate hashed ways, and are copied into them at the
overflow. Switches `CRATONVM_JIT_MEGA_CELL_FIRST` and
`CRATONVM_JIT_PIC_INLINE_FIRST` (both default on). Details, soundness and the
executed tests: `r12w6-mega5-cell-first-at-megamorphic-sites-20260927.md`.

Left on this page: the measurement (`R12Mega4OneSite` default against
`CRATONVM_JIT_MEGA_CELL_FIRST=0`), findings 1 and 2 (the single-pass OSR door
splices nothing; OSR retire cells pin the baked callee), finding 4 (W5-5, the
stub's re-derivation), and finding 7's protocol pricing (W5-2). Finding 3 is
fixed for single-pass slots (the optimizing tier's slots need
`r12w6-mega5-ir-ic-slots-inline-first-patch-20260927.md`); finding 5 is what
W5-4 answers.

## Round 12 wave 7 (lane mega6)

The growth with the class count in the orchestrator's measurement (mega8 15
-> mega24 36 -> mega64 45, OSR shape) was not the probe chain's length as
such: the class cells were dead. A callee that tiered up while an OSR loop
ran lost its cell to the supersede and could not take it back before the
retirement was graced, which a thread inside the loop prevents; nothing
re-published it later. So §3's table ("class cell ~8/24") did not describe
what ran: nearly every overflow receiver took the site's hashed ways or the
shared table, behind a missed cell probe once W5-4 put the cell first. Fixed
by same-key refill (`CRATONVM_JIT_IC_SAME_KEY_REFILL`); reading and
prediction in `r12w7-mega6-retired-cells-never-refill-20260927.md`.

Still open on this page: finding 1 (the single-pass OSR door splices
nothing), finding 2 (OSR retire cells pin the baked callee), finding 4 (the
stub re-derives what the cascade knows: fixed for the single-pass tier by
W7-1, `CRATONVM_JIT_MEGA_GATE_FAST_ENTRY`; open for the optimizing tier), finding 7 (protocol
pricing, W5-2). Finding 3 is now fixed for the optimizing tier's slots too
(the W5-6 IR patch was applied).

## Round 13 wave 2 (lane mega)

No change to the accounting's open findings landed; this wave's code at the call
sites is the recursion stack check, which ADDS to the path this page counts:

| where | added per call | when |
|---|---|---|
| single-pass IC/stub site | `CMP RSP,[rbp-floor]; JBE` (1 fused pair) | always, with `CRATONVM_JIT_IC_STACK_GUARD` on |
| single-pass prologue of a method with an IC site | inherit proof (~7) + leaf `jit_native_stack_floor` CALL | per ENTRY (not per call); a same-method recursive child inherits |
| IR IC site | `MOVZX EAX,SP; CMP EAX,imm32; JA` (3) | always; the guard helper only when the frame straddles a 64 KiB boundary |

For `R12Mega4OneSite` the per-call part is 2-3 instructions of the ~60 the
megamorphic path runs; the per-entry part lands on the `ent-*` shapes' caller only
once per 256 iterations. `CRATONVM_JIT_IC_STACK_GUARD=0` restores round 12's code.

Open, unchanged: finding 1 (the single-pass OSR door splices nothing) and 2 (OSR
retire cells pin the baked callee) are in lane replay's / the planner's files;
finding 4 for the optimizing tier (the stub re-derives the receiver's class id on
the megamorphic edge, and the `.slow` staging loop runs before it) is proposal
M13-2 with the exact shape (split `.slow` into a screened and a raw entry; stage
through a register other than RAX; enter the stub's gate entry); finding 7
(protocol pricing) is still unmeasured.

The honest reading of the orchestrator's round-12 numbers (osr-mega24 37 ms against
HotSpot 4; poly4 12, mega8 15) is that nobody has yet measured where the extra
~45 cycles per megamorphic call between mega8 and mega24 go: the instruction count
of the cell-first path does not change with the class count, so the growth is
micro-architectural (indirect-target prediction over 24 bodies, i-cache/BTB
footprint of 24 callee bodies with their prologues) or a probe tier that is not the
one this page assumed. Proposal M13-1 is that measurement (`perf stat` / `perf
record` on the Linux host, per phase, both VMs), and it should come before any
further instruction trimming here.

## Round 13 wave 5 (lane callcost2)

**Landed: finding 4 for the optimizing tier (proposal M13-2),** switch
`CRATONVM_JIT_IR_MEGA_GATE_ENTRY` (default on; also off under
`CRATONVM_JIT_MEGA_GATE_FAST_ENTRY=0`). `jit/src/ir_lower.rs` `emit_inline_cache_call` now
splits its `.slow` edges in two:

* the edges that carry the receiver's class id in EAX -- the MIC's and each rung's zero-word
  `JZ`, the megamorphic gate's `JNE`, the last rung's miss -- stage the arguments through R11
  (so EAX survives) and `JMP` to the hashed stub's gate entry (`MOV EDX, EAX` and back into the
  stub past its receiver reload, null test, kind screen and class-id reload);
* the null and kind edges keep the RAX staging and fall into the stub's full entry, which sends
  both on to the resolving helper, as before.

The stub's gate entry is the one the single-pass cascade has used since round 12 wave 7
(`runtime_lowering.rs` `emit_hashed_vtable_stub_body`, `fast_entry`); the new
`emit_hashed_vtable_stub_reloading_with_gate_entry` asks for it on the optimizing tier's
reloading stub, and `hashed_stub_admitted` lets the caller decide before emitting the jump.
Per megamorphic call the six-instruction full entry (receiver reload, `TEST`/`JZ`, kind
`TEST`/`JNE`, class-id reload: three loads, two of them dependent) becomes `JMP`, `MOV EDX,EAX`,
`JMP` back (no load). Every
edge still stages the whole argument block, which the stub's homeless-argument loads, its
callee-deopt service (read as written when the site published no shadow set) and the
resolving helper all read; staging only the homeless arguments would need the helper's block
re-staged on the stub's miss edge and is left as proposal CC2-5. Test:
`ir_lower.rs` `r13w5_screened_megamorphic_edges_enter_the_stub_gate_entry` (one gate entry, one
jump into it, the R11 staging store just before the jump; nothing enters a gate entry with the
switch off). Probe: `C:\craton\jitr13-probes\src\R13Callcost2MegaGate.java` (`virt`, `iface`,
`wide`, `eq` with array and null receivers, `throwy`), default against
`CRATONVM_JIT_IR_MEGA_GATE_ENTRY=0`.

Still open on this page: findings 1 and 2 (the single-pass OSR door splices nothing; OSR retire
cells pin the baked callee; not this lane's files), finding 7 (protocol pricing: still
unmeasured), and M13-1 (the `perf` measurement the wave-2 section asks for before any further
instruction trimming). A smaller item found while reading, filed as proposal CC2-2: the
optimizing tier's cascade screens the receiver kind at EVERY virtual site, including an
`invokevirtual` whose owner is a class other than `Object` (no array can reach it), which the
single-pass cascade skips (`op_invoke.rs` `ic_receiver_may_be_array`) -- one load, test and
branch on every monomorphic call too.

## Round 13 wave 8 (lane mega7)

Nothing landed on the per-call path this wave, and nothing was measured (lanes do not run the VM).
What the accounting should know:

* The one new cost is per UNLOAD, not per call: the unload pass now walks every published body's MIC
  and PIC slots once (`r13w8-mega7-site-caches-keep-unloaded-receivers-FIXED-20260928.md`); a slot with no
  dead receiver costs twenty atomic loads and takes no lock.
* Under loader churn the per-call path this page counts was not the path that ran: a site whose ways
  filled with unloaded receivers sent every live receiver to the cell probe or past it (see the
  round-trips page, seventh shape). `C:\craton\jitr13-probes\src\R13Mega7LoaderChurnIface.java` times
  it (`churn`, `mono-after-churn`), default against `CRATONVM_JIT_IC_FORGET_DEAD_RECEIVERS=0`.
* The honest order of the remaining work is unchanged: M13-1 (a `perf stat`/`perf record` per phase of
  `R12Mega4OneSite` on the Linux host, both VMs) before any further instruction trimming, since the
  growth from mega8 to mega24 is not in the instruction count. Findings 1 and 2 (the single-pass OSR
  door splices nothing; OSR retire cells pin the baked callee) and 7 (protocol pricing) stay open in
  their owners' files.

## Round 13 wave 10 (lane mega8)

Nothing on the per-call path this page counts changed, and nothing was measured (lanes do not run
the VM). Two changes in `inline_cache_pic.rs` move costs this page does not count per call:

* per UNLOAD: the dead-receiver pass (wave 8) now queues a body's withdrawn owners in one batch
  instead of one retirement-queue trip, and one drain, per slot (`defer_jit_owners`,
  `CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS`); every other writer section batches the same way. Inside
  the collector's pause that was O(slots x queue) while any thread is in compiled code.
* under loader churn: ways freed by an unload are free for the next generation at once
  (`CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE`), so a churned polymorphic site keeps the PIC-way-0
  row of §2's table instead of falling to the hashed stub row (~20 against ~45 instructions) for the
  rest of a run in which some thread stays compiled. `C:\craton\jitr13-probes\src\R13Mega8ChurnWayReuse.java`
  times it (`poly`, `mega`) against the switch off.

Open, unchanged: findings 1, 2 and 7, and M13-1 (the `perf stat` / `perf record` per phase of
`R12Mega4OneSite` on the Linux host, both VMs, before any further instruction trimming). The new
`R13Mega8MegaShapes` adds a width axis (one megamorphic site per phase at 8/12/16 receivers); its
in-loop check calls the static `mix` once per iteration, so read it against its own widths, not
against `R12Mega4OneSite`'s absolute numbers.

## Round 13 wave 11 (lane mega9)

Landed on the per-call path this page counts:

* **The optimizing tier's kind screen at class-owned `invokevirtual` sites is gone** (M13-6, the
  CC2-2 item of the wave-5 section; `CRATONVM_JIT_IR_IC_KIND_SCREEN_ELIDE`, default on): one header
  byte load and one branch fewer on EVERY IR virtual call whose owner no array is assignable to,
  monomorphic included -- what the single-pass cascade has skipped since round 11. With it, finding
  4 has no open part left in either tier.
* **Profile-seeded megamorphic sites** (M13-9): a slot whose site's profile shows more than four
  classes (and a real overflow share) starts flagged, so the cell-first path of the round-12 wave-6
  section serves from the first call of a recompiled body instead of after the inline ways refill.
  Warm-up only; the steady state this page times is unchanged.

Not changed: findings 1, 2 and 7. Still nothing measured by a lane. M13-1 now has its probe and
plan: `C:\craton\jitr13-probes\src\R13Mega9CallAnatomy.java` separates the lookup chain (one
inherited target over 8/24/48 classes), indirect-target prediction (distinct targets, cyclic,
in runs, random) and callee footprint (4x bodies), per OSR and method-entry shape, and runs one
phase for N seconds for `perf stat`; the counter set, the attribution step and how to read each
difference are proposal M9-1 in `jit-r13-mega9-proposals-RETIRED-20260929.md`. That measurement should still come
before any further instruction trimming here.

## Round 13 wave 12 (lane osrdoor)

**Landed: finding 1, the single-pass OSR door splices** (proposal W5-1; switches
`CRATONVM_JIT_OSR_SPLICE` and `CRATONVM_JIT_OSR_SPLICE_CALLS`, both default ON).

* `vm/src/runtime/interpreter/jit_bridge.rs` `compile_osr_body`: after the invoke loop and the
  trivial-constructor pass, the statically bound sites in `pending_callee_compiles` are offered to
  `cratonvm_jit::plan_osr_door_splices` (`jit/src/lib.rs`, beside `retirable_osr_direct_call`)
  BEFORE their eager compile. The planner is `plan_inline` with the method-entry budgets and
  tiers (`MAX_INLINE_BUDGET[_HOT]`, `hot_loop_ranges` / `call_site_is_hot` over the method's
  profile), `InlineBackendCaps::single_pass_x64()`, no guarded variants. It takes a site only at
  the opcode whose single-pass arm splices it unguarded (`invokestatic` kind 3, `invokespecial`
  kind 1; a private `invokevirtual` pinned to kind 1 is left alone) and only inside a loop
  (`bytecode_pc_in_loop`): a site after the loop runs once per entry and keeps its retire-celled
  direct call, which is what sends the code after the loop to a redefined callee's new body.
  The body comes from `resolve_inline_site` (same admission as a method-entry splice: native
  shadow, `<clinit>` done, size, shape). Calls inside the body are direct-bound through
  `osr_splice_direct_bind`, the door's own four refusals (another loader's copy, an indy-trap
  body, a cycle that must dispatch, `osr_callee_bars_direct_call`), pinned until publication;
  with `CRATONVM_JIT_OSR_SPLICE_CALLS=0` a body whose calls are not themselves spliced is rolled
  back by the emitter and the site keeps its call.
* Every planned site still gets its eager compile, its direct bind or dispatch row and its
  `JitInvokeInfo`, so a splice the emitter rolls back falls to exactly the old lowering. The
  spliced bodies' own call targets are interned into the door's arenas and their entries go to
  `_direct_callee_entries` (`intern_osr_door_splice_targets`). The artifact records the plan's
  `inlined_methods` (so `put_osr`'s dependency check and the class-change / unload invalidations
  see it) and `inline_tally`.
* A backend refusal of a body that splices something is asked again with no splices before it is
  the permanent bail (`mark_bail_listed_with_site`): the OSR door is the only route out of the
  interpreter for a `main` or `@Test` loop, so a splice must not be able to cost the loop its
  compiled body. `CRATONVM_DBG_JITC=1` prints `osr-splice-planned`, `osr-splice-refused` and
  `osr-splice-retry` lines.
* Soundness (argued on `plan_osr_door_splices`): nothing is published from inside a splice
  (`x64/inlining.rs` postcondition), and a caller-chain point would go through
  `transfer_osr_exit_chain_into_live_frame` and `validate_osr_entry`'s admission; a method with
  an exception table compiles with `precise_exception_frames`, which plans nothing (the
  interlock), so every exception leaving a splice leaves the frame (`Propagate`); a redefinition
  of a spliced class takes the whole-cache path (the resolver marks it copied), which withdraws
  the OSR body and sends a running frame out at its next back-edge poll.
* Tests: `jit/src/lib.rs` `r13w12_osrdoor_splice_plan_tests` (a static leaf planned with its
  dependency; nothing planned or resolved under precise frames; only the unguarded arms inside a
  loop; an unresolved callee counted).
* Probe: `C:\craton\jitr13-probes\src\R13OsrdoorSplice.java` (leaves with int/long/double
  parameters, a side effect inside one splice and an NPE out of the next, a divisor splice whose
  zero throws from its miss edge, a constructor splice, the `want -> ref` nested shape of this
  page with a timing line, a spliced helper with a direct call inside, a class whose `<clinit>`
  first runs half-way through the OSR'd loop).

**Expected effect on this page's numbers.** `R12Mega4OneSite` `osr-static24` should drop to
`osr-nocall24` under `CRATONVM_JIT_OSR_OPTIMIZING=0`, and `R12Mega3VirtualSlots` `mega*` should
lose the second call (the `want` row of section 4, about a quarter of the iteration) whenever the
loop runs `osr/sp`. Measure default against `CRATONVM_JIT_OSR_SPLICE=0`.

**Still open on this page:** finding 2 (OSR retire cells pin the baked callee body; proposal
W5-3), finding 7 (protocol pricing) and M13-1 (the `perf` measurement). Not planned by the new
door, and left as proposals in `jit-r13-osrdoor-proposals-RETIRED-20260929.md`: guarded (PGO-02) splices of the
OSR body's virtual sites, and splices in a method with an exception table.

## Round 14 wave 1 (lane calls)

Nothing on the per-call path changed; this wave made M13-1 / M9-1 runnable instead of adding a
fourth round of unmeasured trimming.

* **The measurement is a script now:** `C:\craton\jitr14-probes\m91-mega-anatomy.sh` (Linux host;
  `EXE`, `JAVA`, `CP`, `OUT` from the environment). `step1` is the tier check (every `osr-*` phase
  of `R13Mega9CallAnatomy` in every arm: `osr/sp` or `osr/ir` from `CRATONVM_DBG_JITC=1`, and
  `mic_calls` from `CRATONVM_DBG=mic-prof`, which must stay at warm-up counts); `step2` is `perf
  stat` per phase, 5 runs, pinned with `taskset`, HotSpot and CratonVM, CratonVM in seven arms that
  each remove or reorder one tier of the lookup chain (`def`, `CRATONVM_JIT_MEGA_CELL_FIRST=0`,
  `CRATONVM_JIT_MEGA_CLASS_SLOTS=0`, `CRATONVM_JIT_HASHED_STUB_MEGA_TABLE=0`,
  `CRATONVM_JIT_SP_INLINE_MEGA=0`, `CRATONVM_C2_ACCEPT=never`, `CRATONVM_JIT_OSR_OPTIMIZING=0`);
  `step3` is `perf record` on `osr-shared24` / `osr-distinct24`. Divide each counter by the
  phase's printed `calls=`.
* **Correction to M9-1's step 3:** it named `CRATONVM_JIT_PERF_MAP=1` for `perf annotate`. A perf
  map carries names and ranges only; annotate needs the machine code, which only the jitdump sink
  writes (`CRATONVM_JIT_JITDUMP=1`, `jit/src/jitdump.rs`; `perf record -k 1`, then `perf inject
  --jit`). It matters here more than usual: the IC cascade, the hashed stub and its cell / hashed /
  shared-table probes are emitted INSIDE the caller's body (`code_events.rs`: stubs have no record
  of their own), so the samples land on the caller's jitted symbol and only an annotated listing
  says which tier's instructions take them. The script uses the jitdump.
* **Why no in-code tier counters:** counting which tier served each call needs an increment in
  every hit tail of the hashed stub (`runtime_lowering.rs` `emit_hashed_vtable_stub_body`,
  `emit_mega_class_slot_probe`, `emit_mega_dispatch_table_probe`) and of both cascades. That
  perturbs exactly the path being timed and lives in files outside this lane. The arms above
  answer the same question by difference (a tier switched off moves its share to the next one),
  and step 3 answers it by sample. Proposal C14-4 in `jit-r14-calls-proposals.md` keeps the
  counted variant (a debug-only stub arm) in case the arms disagree with the samples.

Open, unchanged: finding 2 (OSR retire cells pin the baked callee), finding 7 (protocol pricing),
and the M13-1 numbers themselves (the orchestrator's run of the script). Status stays OPEN.

## Round 14 wave 3 (lane calls)

Two changes touch rows this page counts; nothing was measured (lanes do not run the VM).

* **The optimizing tier's direct cross call** (the `want` row of section 4 when `mega` runs `osr/ir`
  or `ent-*` and `want` is not spliced): the merged post-call sequence used to be `CMP RAX,1; JNO
  .keep` taken on every call, jumping over the inline callee-deopt service and exception tail
  (~60-120 bytes in the hot span). It is now `CMP RAX,1; JO .cold` not taken, falling through, with
  the cold side after the body (CC3-1, `CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS`, default on; see
  `jit-r13-callcost3-proposals-RETIRED-20260929.md`). One taken branch fewer per such call. The inline-cache hit
  arms (the `eval` row) are unchanged; the census line
  (`CRATONVM_DBG_JITC=1`: `ir-call-cold-tails ... ic_hit_exits=<n> ic_admissible=<m>`) says how
  many of them the same deferral could take.
* **Finding 1's pricing** (the single-pass OSR door's splices): the loop the OSR trigger fired in
  is now hot evidence for the planner even with no branch profile (OD-1,
  `CRATONVM_JIT_OSR_SPLICE_ENTRY_LOOP_HOT`), so an in-loop helper up to `FreqInlineSize` (325
  bytes) is spliced where only 35 bytes were before, under the hot method budget.

Open, unchanged: finding 2 (OSR retire cells pin the baked callee; proposal OD-5, `lib.rs`
outside this lane's regions), finding 7 (protocol pricing) and the M13-1 numbers
(`C:\craton\jitr14-probes\m91-mega-anatomy.sh`). Read `R12Mega4OneSite`'s `ent-*` phases default
against `CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS=0` in the same run. Status stays OPEN.

## Round 14 wave 4 (lane calls2)

By reading; nothing measured.

* **The `ent-*` shape's static helpers** (C14W3-1, `CRATONVM_JIT_INLINE_LOOP_SITES_HOT`, default
  ON): a method-entry single-pass compile with no loop profile (every default run) now prices its
  in-loop static / `invokespecial` sites hot, as OD-1 did for the OSR door in wave 3. An in-loop
  helper up to `FreqInlineSize` is spliced in `ent-*` bodies that stay single-pass
  (`CRATONVM_C2_ACCEPT=never`); read `ent-static24` against `ent-nocall24` with the switch on and
  off. (The optimizing tier's planner had no cold tier to begin with.)
* **The IC hit's taken branch** (C14W3-2, the `eval` row): not landed. Every contained variant
  keeps one taken branch on the MIC hit; removing it needs the polymorphic region emitted out of
  line. Plan and decision procedure: `r14w4-calls2-ic-hit-cold-tail-needs-an-out-of-line-region-20260929.md`.
* **Finding 2** (OSR retire cells pin the baked callee): re-read. `JitCycleEdgeCell::admits`
  refuses a newer body on purpose -- "anything newer could close a cycle of cell edges with no
  stack guard" -- so OD-5 is a stack-safety design question (admit a newer body only if it carries
  the self-call stack-floor guard, or only a body of the same tier's cycle set), not a one-line
  relaxation. Unchanged, outside this lane's regions.

Open, unchanged: finding 2, finding 7 (protocol pricing) and the M13-1 numbers
(`C:\craton\jitr14-probes\m91-mega-anatomy.sh`). Status stays OPEN.

## Round 14 wave 7 (lane mega)

By reading and from the orchestrator's recorded numbers; nothing new measured.

* **Finding 7 (protocol pricing) is answered and should leave the open list.** The orchestrator's
  round-12 measurement on this page already refuted it for the shape it was filed on:
  `R12Mega4OneSite` `static24` equals `nocall24` (5 / 5 ms, osr and ent), so one compiled static
  call per iteration is not where the time goes; every later section kept listing it by carry-over.
  Round 14's own runs agree in the default, `thr1` and `c2always` arms (`C:\craton\jitr14-probes`
  `res-base-*.txt`, `res-w3a-*.txt`: `static24` and `nocall24` best reps within a millisecond).
  The exception is `CRATONVM_C2_ACCEPT=never`, where `static24` costs about an interpreted call
  per iteration (best 1222 ms against 5 on w3a) -- not the protocol this finding priced but a
  missing splice / bind in single-pass-only bodies; filed as
  `r14w7-mega-static-leaf-call-costs-an-interpreter-call-under-c2-never-20260929.md`. What remains of the protocol question is the second call of the
  `R12Mega3VirtualSlots` shape, which finding 1's OSR-door splice (round 13 wave 12) and OD-1 /
  C14W3-1 (round 14 waves 3-4) removed wherever the callee is spliced.
* **Finding 2** (OSR retire cells pin the baked callee): unchanged; a stack-safety design question
  in `lib.rs` `JitCycleEdgeCell::admits` (see the wave-4 section), outside this lane's regions.
* **M13-1** (where the ~45 cycles between mega8 and mega24 go): still the orchestrator's run of
  `C:\craton\jitr14-probes\m91-mega-anatomy.sh` on the Linux host. No trimming before it.
* Warm-up side, related: the round-trips page's shape 4 now has a count
  (`mic_unbound_table_held`, see that page's wave-7 section) and the grace catch-up no longer fails
  on the refusing thread's own stale stamp (`CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP`, see
  `r12w7-mega6-grace-starves-while-a-thread-stays-compiled-CLOSED-20260929.md`). Neither changes a
  steady-state row of this page.

Status stays OPEN (finding 2 and the M13-1 numbers).
