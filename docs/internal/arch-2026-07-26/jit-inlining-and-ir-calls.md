# JIT inlining and IR call lowering

**Slug:** `jit-inlining-and-ir-calls`
**Date:** 2026-07-26
**Basis:** merged from `dev` at `6495a191c34fcf701c7c168517e6a13f462fc3dc`.
**Files changed:** `jit/src/ir.rs`, `jit/src/ir_lower.rs`, `jit/src/lib.rs`,
`jit/src/tiered.rs`.

> **Basis note.** This work was briefed and started against `origin/main`
> (`e4e4053bb`). Dev had moved substantially underneath it: in particular
> `ir_lower::emit_direct_cross_call` and `lib.rs`'s `ir_direct_calls` planning
> **already existed on dev**, and `ir_compatible`'s invoke cap had already been
> raised 5 → 32 by that slice. The work below was rebased onto dev's design
> rather than duplicating it, so "what this change adds" is stated relative to
> dev, not to the brief.

---

## 1. The problem, restated against dev

BENCHMARK.md attributes two separate JIT gaps to the same root cause: the
optimizing IR pipeline declined ordinary application methods, because for a
call-bearing method an "optimizing" recompile was a net **regression** against
the single-pass body.

Dev had already closed half of that. `emit_direct_cross_call` binds a resolved
`invokestatic` / non-`<init>` `invokespecial` straight to its callee's compiled
entry, and the invoke cap moved to 32 accordingly. What remained:

| gap | state on dev | state after this change |
|---|---|---|
| direct call, statically bound | **done** (`emit_direct_cross_call`) | unchanged |
| direct self-recursive call | **done** (`invoke_kind == 4`) | unchanged |
| monomorphic / polymorphic inline cache | **missing** — a virtual site paid a full `jit_invoke_dispatch` round trip *plus* a dynamic vtable/itable lookup, per call | **done** (`emit_inline_cache_call`) |
| virtual-call lowering enabled by default | **no** — `CRATONVM_JIT_IR_CALL_VIRTUAL` was opt-**in** | **yes** — inverted to opt-out |
| field / static-field / allocation / size caps | still 5 / 5 / 3 / 3 / 200 | 64 / 64 / 16 / 16 / 8000 |
| inlining budget shape | flat 35-byte callee cap, 250 total | HotSpot three-tier (6 / 35 / 325), 750 cold / 2000 hot |
| tier-4 compile-cost bound | none | `IR_MAX_GRAPH_NODES` + `MAX_C2_COMPILE_TIME_MS` |
| IR lowerer buffer-overflow check | **absent** (latent silent truncation) | added |

---

## 2. What the IR can now lower that it could not

### 2.1 Virtual and interface calls, through a real inline cache

`ir_lower::emit_inline_cache_call` emits the same three-tier cascade
`jit/src/x64.rs` emits for the single-pass backend:

```text
  MOV  RAX, [rbp - recv_slot]                  ; receiver = arg0
  TEST RAX, RAX ; JZ .slow                     ; null → helper raises the NPE
  MOV  EAX, dword [RAX]                        ; class_id (ObjectHeader + 0)
  ; ── monomorphic (JitMICSlot) ──
  MOV  R10, imm64 mic
  CMP  EAX, [R10 + CACHED_CLASS_ID_OFFSET]     ; JNE .pic
  CMP  BYTE [R10 + CACHED_NEEDS_CONTEXT], 0    ; JE  .pic
  <marshal entry ABI> ; MOV R11,[R10+8] ; CALL R11 ; JMP .done
  ; ── polymorphic (JitPICSlot), 3-way ──
.pic:
  MOV  R10, imm64 pic
  for i in 0..JIT_PIC_ENTRIES:
    CMP EAX, [R10+CLASS_ID_OFFSETS[i]]   ; JNE .pic_{i+1}  (last: .slow)
    CMP BYTE [R10+NEEDS_CONTEXT[i]], 0   ; JE  .slow
    <marshal> ; MOV R11,[R10+ENTRY_PTR_OFFSETS[i]] ; CALL R11 ; JMP .done
  ; ── megamorphic / cold ──
.slow:
  <marshal args into the frame staging region>
  jit_invoke_virtual_mic(vm, info, args_ptr, num_args, mic, pic)
.done:
  <emit_call_return_check>                     ; shared sentinel + result spill
```

Design points worth recording:

* **One class-id load.** EAX carries the receiver class id from the single
  header load through every guard. No guard writes RAX, and the ABI marshalling
  touches only `ENTRY_ABI_REGS`, which excludes RAX/R10/R11 on both platforms —
  so both the class id and the slot base survive to their uses. A test pins
  `count_seq(code, [0x8B, 0x00]) == 1`.
* **`R10 → R11 → CALL` invariant.** Inherited verbatim from `x64.rs`: nothing
  may be emitted between the cached-entry load and its paired `CALL R11`,
  because R10 is a general scratch register and keeping an indirect-call target
  addressed through it across other instructions would let an R10 clobber
  redirect native control flow. `emit_call_cached_entry` is the only place that
  pair is emitted.
* **All forward branches are `rel32`.** The single-pass inline caches have had
  two independent CRIT bugs from `rel8` displacement overflow (CRIT-3, and the
  inter-slot `jne`). A few extra bytes per site is the right trade; a test
  scans the emitted stream for any unpatched `00 00 00 00` displacement.
* **Cold sites cost nothing.** An unpopulated slot holds `class_id == 0`, which
  no real receiver matches, so every guard falls straight through to the helper
  — which then populates both caches, after which later invocations hit inline
  with no recompile. This is the same eager-allocation strategy (HIGH-7) the
  single-pass planner already uses.
* **Slot ownership.** `lib.rs` allocates one `Box<JitMICSlot>` + one
  `Box<JitPICSlot>` per site, seeds the MIC from the receiver-type profile
  (`dominant_receiver(counts, 80)`) and the PIC from the MIC, and moves both
  onto `CompiledMethod::_jit_mic_slots` / `_jit_pic_slots` — the same contract
  the single-pass path uses, so the baked imm64s cannot outlive their storage.
  On the non-emittable-invoke bail the plan and the boxes are cleared
  **together**; never one without the other.

`JitMICSlot` / `JitPICSlot` live in `jit/src/lib.rs`, which is in scope for this
change, so no cross-file coordination was needed for the slot types.

### 2.2 Virtual-call lowering is now default-ON

`lib.rs::ir_virtual_calls_enabled()` (opt-out via
`CRATONVM_JIT_IR_CALL_VIRTUAL=0`, with a thread-local test override) is OR-ed
into the caller-supplied `ir_emit_virtual_calls` parameter. The parameter can
still force the capability on; it can no longer force it off.

**Required follow-up, outside this change's file ownership:**
`vm/src/runtime/env_cache.rs:797` still computes its own copy as

```rust
*CACHE.get_or_init(|| std::env::var_os("CRATONVM_JIT_IR_CALL_VIRTUAL").is_some())
```

That copy is now redundant (the OR in `try_compile_inner` makes the capability
active either way) but it is inconsistent with every sibling gate in that file.
It should become `map_or(true, |v| v != "0")`, after which the local
`ir_virtual_calls_enabled()` gate can be deleted and the parameter threaded
straight through.

---

## 3. The new `ir_compatible()` rule

The old caps were never lowering limits — the IR builder bails cleanly on any
opcode it cannot lower, and the caller falls back to single-pass on `build()`
or `lower()` returning `None`. They were a blunt proxy for *profitability*.
With calls lowering as well as single-pass does, the rule splits cleanly into
**budgets** (tunable guard rails) and **exclusions** (real missing lowerings).

### Budgets — a method past one is still compiled, by single-pass

| constant | was | now | why this number |
|---|---|---|---|
| `IR_MAX_INVOKES` | 5 → 32 | **64** | An inline-cache site emits ~250 bytes, so 64 sites ≈ 16 KiB of dispatch code — the same order single-pass produces for the same method. Also still bounds (a) the argument staging region / `needs_context` frame growth and the `1 + num_params > abi_len` lowering bail, and (b) wasted compile time on a method whose callees are mostly unresolvable. |
| `IR_MAX_FIELD_OPS` | 5 | **64** | `Op::Load`/`Op::Store` (optionally via the checked `jit_getfield` helper). Nothing scales worse than single-pass; 5 was part of the same "keep real methods off the IR path" posture as the invoke cap. |
| `IR_MAX_STATIC_FIELD_OPS` | 5 | **64** | As above. |
| `IR_MAX_ALLOCATIONS` (`new`, `anewarray`, each) | 3 | **16** | Deliberately the most conservative raise — see §3.1. |
| `IR_MAX_BYTECODE_SIZE` (`ir_compatible_sized`) | 200 | **8000** | HotSpot's `HugeMethodLimit`, the point at which HotSpot itself refuses to compile (`DontCompileHugeMethods`). 200 excluded essentially every real application method, which made every other cap moot. |
| `IR_MAX_GRAPH_NODES` (new) | — | **20 000** | The real tier-4 compile-time guard; see §5. |

### 3.1 The allocation budget is NOT like the others

The IR has **no allocation lowering**. An `Op::New` that survives escape
analysis bails the whole method to single-pass (`has_live_new` in
`try_compile_inner`), *precisely so the single-pass inline TLAB bump-pointer
fast path keeps serving every real allocation*. Raising `IR_MAX_ALLOCATIONS`
only lets more allocations be **scalar-replaced away**; it must never become a
licence to lower a surviving allocation in the IR.

Losing the inline TLAB bump is one of the two independent causes of the July
2026 Binary Trees 4× regression documented in BENCHMARK.md. The raise from 3 to
16 changes only how many allocations escape analysis may *attempt* to
eliminate. `c2_upgrade_would_engage` still refuses every allocation-bearing
method outright, for the same reason. Both facts are recorded in comments at
the code sites, not just here.

### Surviving exclusions — and why each survives

| exclusion | why it is still a hard `false` |
|---|---|
| `athrow` | No IR lowering exists. Only the single-pass backend emits the stash-pending-exception sequence (RBC.6). |
| `invokedynamic` | The IR builder has no `0xba` arm; single-pass lowers it to an uncommon trap. |
| `multianewarray` | Needs a resolver-shaped helper call sequence the IR lowerer does not synthesize. |
| `checkcast` / `instanceof` | The runtime type check needs its own guard shape (class-id compare plus a subtype-check helper fallback). **The inline caches added here do not help**: they cache a call *target*, not a subtype answer. |
| non-empty exception table (checked in `try_compile_inner`, not `ir_compatible`) | STUB-S8 — the IR builder has no exception-table-aware codegen; a handler entry is not a registered merge target, so the builder would walk handler bytecode with stale control/locals/stack state. |

---

## 4. The new inlining budgets

The old model was a flat `MAX_INLINE_BYTECODE_SIZE = 35` applied to every
callee. 35 is HotSpot's `MaxInlineSize`, which HotSpot applies **only to cold
callees**; a callee reached from a hot site gets `FreqInlineSize = 325`.
CratonVM had the cold constant and no hot tier, so a 40-byte accessor called a
million times in a loop was treated exactly like one called twice.

### Three tiers, matching HotSpot's shape

| tier | callee cap | expansion cap | HotSpot name | fires when |
|---|---|---|---|---|
| trivial | `MAX_TRIVIAL_INLINE_SIZE` = 6 | tier's expansion cap still applies | `MaxTrivialSize` | always — smaller than the call sequence it replaces |
| cold | `MAX_INLINE_SIZE_COLD` = 35 | `MAX_INLINE_EXPANSION_COST` = 64 | `MaxInlineSize` | no profile evidence of hotness |
| hot | `MAX_INLINE_BYTECODE_SIZE` = 325 | `MAX_INLINE_EXPANSION_COST_HOT` = 512 | `FreqInlineSize` | site in a hot loop, or a hot receiver profile |

Whole-method budget: `MAX_INLINE_BUDGET` 250 → **750** (cold caller),
`MAX_INLINE_BUDGET_HOT` = **2000** (a caller executing any hot loop).

### Why those totals, specifically

The budget is not a free parameter. `x64::compile` reserves
`callee_code_len * 64` **buffer bytes** and `callee_max_locals +
callee_code_len` **spill slots** per inlined site, both linear in the total. So
the ceiling directly bounds committed executable memory (~64 bytes per budgeted
bytecode) and JIT frame size (~8 bytes per budgeted bytecode):

* 750 → ~48 KiB buffer estimate, ~6 KiB frame worst case.
* 2000 → ~128 KiB buffer estimate, ~16 KiB frame worst case.

This is the "real budget accounting, not a low constant" requirement: the
numbers are derived from the backend's own sizing formula rather than picked.

### Hotness comes from real profile data

`hot_loop_ranges()` derives `[header, back_edge]` pc ranges from
`profile::MethodProfile::loops` (keyed by back-edge pc) plus the bytecode — the
header is recovered by decoding the branch at the back-edge pc (3-byte
`goto`/conditional with a signed 16-bit offset, or 5-byte `goto_w` with a
32-bit one). A back-edge is a branch whose target is at or before its own pc;
anything else keyed there is ignored rather than guessed at.

`call_site_is_hot()` accepts either signal:

1. the site lies in a loop whose back-edge count clears
   `INLINE_HOT_LOOP_BACKEDGES` (100 — the same threshold
   `LoopTripProfile::suggests_unroll_factor` already uses, so the two profile
   consumers agree on what "hot" means); or
2. the site's own receiver-type profile records ≥ `INLINE_HOT_SITE_OBSERVATIONS`
   (500) executions. `MethodProfile::receivers` records one observation per
   executed `invokevirtual`/`invokeinterface`, so the sum is a direct execution
   count.

**With no profile, every site is cold and every budget collapses to its
pre-change value** — an unprofiled compile inlines bit-for-bit identically.
`inline_site_expansion_cost(site)` is kept as `…_tiered(site, false)`, so dev's
existing cost tests remain exactly as written and remain meaningful.

### 4.1 Required follow-up: two constant-pin tests

`MAX_INLINE_BYTECODE_SIZE` is the value the VM-side admission filter
`vm/src/runtime/interpreter.rs::resolve_inline_site` gates on
(`if code_len > cratonvm_jit::MAX_INLINE_BYTECODE_SIZE { return None }`), which
is why it had to become the *largest* of the three tiers: the resolver admits
candidates and `try_compile_inner` then applies the per-site tier. Raising it
to 325 keeps that filter correct with **no change to `interpreter.rs`**.

Two pin tests in `vm/src/vm.rs` (outside this change's file ownership) assert
the old literals and will fail:

* `vm/src/vm.rs:72758` — `assert_eq!(cratonvm_jit::MAX_INLINE_BYTECODE_SIZE, 35);`
  → should become `325`, or better, assert `MAX_INLINE_SIZE_COLD == 35` (the
  constant that actually kept the old meaning).
* `vm/src/vm.rs:72763` — `assert_eq!(cratonvm_jit::MAX_INLINE_BUDGET, 250);`
  → should become `750`.

Both are pure tautology tests whose purpose is to notice exactly this change.
The doc comment on `MAX_INLINE_BYTECODE_SIZE` warns that lowering it back to 35
would make the hot tier unreachable.

---

## 5. Keeping tiering coherent

Two new bounds, one static and one observed.

**Static — `ir::IR_MAX_GRAPH_NODES` (20 000).** Checked in `try_compile_inner`
immediately after `IrBuilder::build`, before any optimization runs. A bytecode
length cap does not bound compile time: `ir_optimize`'s GVN, the
escape-analysis connection graph and the scheduler are all super-linear in
*node* count, and an 8000-byte straight-line arithmetic method builds a far
larger graph than an 8000-byte call-heavy one. An over-large graph costs one
linear build and then takes single-pass.

**Observed — `tiered::MAX_C2_COMPILE_TIME_MS` (250 ms).** A C2 compile that
*actually* took longer than the budget sets `c2_bailout`, so the method
degrades to C1 for the rest of the process. Reusing `c2_bailout` rather than
adding new `MethodState` is deliberate: every existing degradation path already
consults it (`should_compile`, `on_backedge`, `request_osr`,
`request_c2_upgrade`), so the compile-cost demotion is coherent with the
deopt-driven one **by construction**, and the `c2_bailouts` statistic keeps
counting the whole "stopped trying C2" population.

Deopt degradation itself is unchanged and already correct: 3 deopts
(`MAX_DEOPTS_BEFORE_BAILOUT`) → `c2_bailout` → the method stays at C1, which is
the single-pass backend — the same backend every IR bail falls back to. A
method newly admitted to the IR path therefore degrades along an
already-exercised path.

### 5.1 Latent bug fixed in passing

`ir_lower::lower_inner` never checked `ExecutableBuffer::overflowed()`. That
method is non-panicking: on capacity exhaustion it sets a sticky flag and
**drops the write**, so an under-estimated buffer yields a silently truncated
body and execution runs off the end of the emitted code. Harmless only while
per-node emission was tiny and bounded; inline caches (~250 bytes/site) plus
the widened invoke budget make the estimate materially harder. Two changes:

* the buffer estimate now budgets call nodes explicitly
  (`nodes*32 + call_nodes*320 + 1024`, saturating), and
* `lower_inner` bails to single-pass when `buf.overflowed()`, exactly like the
  existing unallocated-slot latch.

`patch_rel32_to_here` swallows patch failures for the same reason: they are
only reachable on an already-overflowed buffer whose artifact is discarded, so
an `expect` there would turn a recoverable fallback into a compile-thread
panic.

---

## 6. NOT LANDED: bounded-depth self-recursive inlining

This was task item 4 and it is **not implemented**. The reason is a hard
structural prerequisite, not scope. Recording it precisely so the next attempt
does not rediscover it.

### The blocker

`IrBuilder::build` records a `SafepointSnapshot { bci, locals, stack }` at
**every** bytecode boundary (`ir.rs`, the `if self.ctrl != NO_NODE` push before
the opcode dispatch). The lowerer resolves a deopt point by
`graph.safepoints.iter().find(|s| s.bci == bci)` — **first match wins** — and
anchors native offsets through `bci_native: HashMap<usize, usize>`, keyed by
bytecode pc.

Inlining a copy of *the same method's* bytecode therefore produces two
snapshots claiming the same `bci`. Any deopt inside the inlined region would
resolve to the **outer** frame state: the wrong locals, the wrong operand
stack, silently. `bci_native` would likewise collapse two distinct native
regions onto one pc.

This is not specific to self-recursion — it blocks *any* IR-level inlining.
HotSpot solves it with an inline tree and a caller chain in the deopt state.
CratonVM's `SafepointSnapshot` and `FrameState` have no frame identity at all.

### What a correct implementation needs

1. **Inline-aware deopt frames.** `SafepointSnapshot` needs an inline-frame
   index (or a caller chain), `bci_native` needs to be keyed by
   `(inline_frame, bci)`, and `deopt::FrameState` needs to reconstruct a chain
   of interpreter frames rather than one. This is the real work.
2. Only then: splice a nested `IrBuilder` build of the callee into the caller's
   graph — node copy with id remap, `Op::Param(i)` → the actual argument, the
   nested start-control/memory rewired to the caller's `ctrl`/`mem`, and the
   nested `Op::Return` nodes merged into an `Op::Merge` + `Op::Phi` (the
   builder's convention is `Merge.inputs = ctrl predecessors` and
   `Phi.inputs = [merge, v0, v1, …]` in the same order — see
   `activate_loop_header`). The builder currently produces one `Op::Return` per
   `*return` opcode, so multi-return merging is mandatory, not optional.
3. A memory phi over each return's incoming memory token, which requires the
   builder to record `(return_node, mem_at_return)` — it currently does not.

### Interim alternatives that were considered and rejected

* **Restricting to single-return callees** sidesteps the Region/Phi work but
  not the safepoint aliasing, and `fib` has two returns, so it would not fire
  on the case that motivated the item.
* **Restricting to guard-free, deopt-free bodies** (so no deopt point is ever
  anchored inside the inlined region) *would* be sound and `fib` qualifies —
  but it is still ~250 lines of graph surgery against a scheduler whose
  invariants would have to be verified by construction alone. Given that this
  change cannot be built or tested here, landing blind graph surgery on top of
  a large, otherwise self-contained change was judged the wrong trade: a wrong
  splice is a silent miscompile of every recursive method.
* **Reusing the single-pass bytecode inliner** for the self-callee is a dead
  end: `x64.rs::try_emit_inline_body` supports only `0xb7` (elidable
  super-ctor) among invokes, so a recursive callee's own `invokestatic` bails
  the attempt (safely — it rolls back — but with no gain), and `x64.rs` is
  read-only for this change.

The remaining Fibonacci(44) gap named by BENCHMARK.md is therefore still open,
and its *other* named cause — cross-call register allocation, i.e. the IR
lowerer spilling every node result to a frame slot — is likewise untouched
(`jit/src/regalloc.rs` is outside this change's scope).

---

## 7. Test coverage

Added in files owned by this change:

`jit/src/ir_lower.rs`
* `ic_site_emits_mic_pic_cascade_not_blind_dispatch` — the full cascade is
  emitted, the class id is loaded exactly once, both slot addresses are baked,
  there are `1 + JIT_PIC_ENTRIES` cached-entry `CALL R11`s, the miss path is
  `jit_invoke_virtual_mic`, and the blind `jit_invoke_dispatch` is **not** also
  emitted.
* `ic_guards_use_published_slot_offsets` — every guard/gate/entry-load
  displacement is derived from `JitMICSlot::*_OFFSET` /
  `JitPICSlot::*_OFFSETS`, not a hand-copied literal.
* `absent_ic_plan_keeps_generic_dispatch` — no plan ⇒ the historical lowering,
  and no cached-entry call at all.
* `ic_cascade_leaves_no_unpatched_branch` — no `rel32` operand is left as its
  `00 00 00 00` placeholder (a stale one would fall through into the next cache
  entry's body instead of branching to `.pic` / `.slow` / `.done`).
* `ic_declines_when_args_overflow_the_abi_register_file` — pins
  `ENTRY_ABI_REGS.len()` per platform and asserts the planner-side bound
  (`lib.rs::ir_entry_abi_reg_count`) agrees with the lowerer's.

`jit/src/ir.rs`
* `test_ir_compatible_rejects_over_cap` extended: each budget's boundary is
  exact; 6 invokes and 6 field ops (the shapes the *original* caps rejected)
  are admitted; a virtual site is admitted on the same budget as a static one;
  and each surviving exclusion (`athrow`, `invokedynamic`, `multianewarray`,
  `checkcast`) is separately re-asserted.
* `test_ir_compatible_sized_rejects_large_methods` extended to the new
  `IR_MAX_BYTECODE_SIZE` boundary.

`jit/src/lib.rs`
* `hot_site_gets_freq_inline_size_allowance`,
  `cold_tier_matches_legacy_behaviour`,
  `trivial_callee_bypasses_size_cap_but_not_expansion_cap`.
* `ir_virtual_call_wiring_routes_through_ir_only_with_flag` **inverted**: the
  default now takes the IR path, and only the explicit opt-out declines it.

`jit/src/tiered.rs`
* `slow_c2_compile_demotes_to_c1`, `slow_c1_compile_does_not_demote`.

### Cases to add to `jit/tests/ir_vs_singlepass.rs`

That file is a differential harness (IR vs single-pass, executing the compiled
code) and is **outside this change's file ownership**. The inline-cache work
needs end-to-end coverage there that the in-crate tests cannot provide, because
they assert on the instruction stream rather than executing it:

1. **Monomorphic hit.** A `static int f(Obj o, int n) { return o.g(n); }` with
   real `helpers.invoke_virtual_mic` wired, invoked repeatedly with one
   receiver class. Assert result equality with single-pass across ≥ 3
   invocations — the first misses and populates, the rest must hit the inline
   guard and return the same values.
2. **Polymorphic, 2–3 receivers.** Same shape, cycling receivers so PIC entries
   0..2 fill. Assert result equality per receiver, then add a 4th receiver
   class and assert it still returns correctly via the megamorphic helper path.
3. **Null receiver.** Assert the NPE raised through the `.slow` path matches
   the single-pass and interpreter behaviour (the inline guard routes null to
   the helper rather than dereferencing it).
4. **Interface dispatch (`0xb9`).** The five-byte encoding with a receiver, to
   pin that the IC path is reached for `invoke_kind == 2` as well as `0`.
5. **Wide return through an IC.** A `J`-returning virtual callee that returns
   exactly `Long.MIN_VALUE`, to pin that `emit_call_return_check`'s
   `dispatch_threw` disambiguation is reached from the IC hit path too, not
   just from the helper path.
6. **ABI overflow fallback.** A virtual site with enough arguments to overflow
   the entry ABI register file on Win64 (receiver + 3+); assert it still
   compiles and returns correctly (via helper dispatch).
