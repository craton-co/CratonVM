# JIT register allocation, escape analysis, profiling and deopt

Slug: `jit-regalloc-and-deopt` · Wave: arch-2026-07-26 · Base: `dev` @ `6495a191c`

Owned files: `jit/src/regalloc.rs`, `jit/src/escape_analysis.rs`, `jit/src/deopt.rs`,
`jit/src/profile.rs`, `jit/src/pgo.rs`.

---

## 1. What the register allocator actually does today

`ARCHITECTURE.md` says there is "no cross-call register allocation". **That is
stale.** Read the code, not the summary.

`jit/src/regalloc.rs` is a real **Chaitin–Briggs graph-colouring allocator over
JVM locals**:

| Stage | Function | Notes |
|---|---|---|
| CFG construction | `build_cfg` | block starts from branch targets, switch targets, fallthroughs |
| Backward liveness | `compute_gen_kill` + `solve_liveness` | worklist to fixpoint, capped at `MAX_LIVENESS_ITERATIONS = 1000` |
| Interference | `build_interference` | per-instruction backward walk plus a live-in clique per block |
| Spill priority | `count_uses` | HotSpot-style loop-depth weighting, `10^depth` capped at `1e6` |
| Colouring | `color_graph` | simplify / potential-spill / select |
| Validation | `regalloc_invariants_hold` | GPR×XMM overlap, float-category, interference — **is** called, from `allocate_registers_with:1444`; on failure the whole allocation is discarded and every local falls back to its frame slot |

**Register pool** (`jit/src/x64.rs:7505,7522`, `regalloc.rs:47,51`):

| Target | GPR pool | FP pool |
|---|---|---|
| Windows x64 | `R12,R13,R14,R15,RBX,RSI,RDI` (7) | `XMM8..15` (8) |
| System V x86-64 | `R12,R13,R14,R15,RBX` (5) | **none** — `x64.rs:8830-8832` discards `xmm_assignments` because SysV makes every XMM caller-saved |
| AArch64 | `X19..X28` (10) | `D8..D15` (8) |

**Every register in every pool is callee-saved.** That is the answer to "does a
value survive a call": yes, by ABI, and it has since the allocator landed. The
callee's own prologue saves it (`x64.rs:13689-13692`, MOV into frame slots
rather than PUSH so RSP stays stable) and its epilogue restores it
(`x64.rs:14052-14070`).

**Cap:** 64 locals. Liveness sets, gen/kill and the interference graph are `u64`
bitsets; locals `>= 64` never receive a register and fall back to the frame slot,
which is always correct.

**Frame layout** (ascending below `rbp`, `x64.rs:8781-8901`):

```
[rbp-8 ...]              locals, canonical home = [rbp - (idx+1)*8]
base_spill               operand-stack spill region, size = max_stack*8
callee_saved_base        prologue GPR save area, size = used_callee_saved.len()*8
xmm_saved_base           prologue XMM save area
reg_spill_base           per-safepoint blind register spill (112 bytes with =all)
deopt_regs_base          256-byte SavedRegisters region (deopt_real / precise-exc only)
shadow space (32) + stack_arg_reserve (16)
```

The historical **spill-overflow defect** (`x64.rs:18653-18675`): the
operand-stack spill cursor `next_spill_offset` marched past `spill_limit_offset`
into `callee_saved_base` and overwrote the caller's saved R12/R13 (HSQLDB
`Token.duplicate` returning null). Guarded now by `checked_spill_range_end`
(`x64.rs:9642-9645`) plus per-instruction `reset_spills`. The **regalloc side**
of that contract — `used_callee_saved` must be exactly the deduped set of
registers `assignments` actually uses, since the prologue sizes the save area
and the safepoint spill region from it — was previously untested. It is now
(`save_area_contract_*` in `regalloc.rs`).

### What a call boundary really costs

This is where the Fibonacci gap lives. At a GC-capable call the emitter runs
`flush_scratch_registers` then `emit_pre_safepoint_spill` (`x64.rs:9951-10023`),
which emits, **on the default configuration**:

1. one `mov [rbp-(i+1)*8], reg` per register-homed local — *all* of them,
   reference or not (`x64.rs:9958-9963`);
2. the **blind full-GPR-file spill**: `safepoint_reg_spill_all` is implied by
   `precise_maps && !CRATONVM_NO_PRECISE_REG_SPILL` (`x64.rs:8697-8700`), and
   `precise_jit_maps_enabled()` is default-ON, so this is **14 stores**
   (`ALL_SPILL_GPRS`, `x64.rs:2677-2679`) at every single call;
3. the safepoint-id store (2 instructions).

`emit_post_safepoint_reload` (`x64.rs:10689-10709`) then reloads only the *oop*
register-locals.

There is a bypass — `can_elide_self_call_register_spill` (`x64.rs:10128-10149`),
written for exactly the direct self-recursive call — but it **fails closed on**:

```rust
|| self.local_assignments.iter().any(Option::is_some)
```

So the moment the allocator gives *any* local a register home, a recursive
`int fib(int)` pays ~17 extra stores per call site, twice per invocation. The
allocator makes the recursion-bound benchmark *worse* at the call boundary than
it would be with allocation off. That, not "no cross-call allocation", is the
register-allocation half of BENCHMARK.md's 2.79× Fibonacci row.

---

## 2. What changed in this wave

### `jit/src/regalloc.rs`

**(a) CFG hole after unconditional terminators — soundness.**
`build_cfg`'s first pass marked the fallthrough PC after a *branch* and after a
*switch*, but not after `ireturn..return` (0xac-0xb1) or `athrow` (0xbf). The
second pass skips any PC that is not a block start, so a region beginning right
after a `return`/`athrow` and not otherwise a branch target belonged to **no
basic block at all** — its local loads and stores were invisible to
`compute_gen_kill`, `build_interference` and `live_locals_per_pc`. That is the
CM-FASTMATH failure shape (invisible uses ⇒ two simultaneously-live locals
coloured onto one register) reached through the CFG instead of through a bad
instruction length.

The reachable instance is an **exception handler** whose protected block ends in
`return`/`athrow` — javac's ordinary shape when a `try` body returns. Handlers
are entered by the runtime router, never by a branch, so nothing else marks
them. Fixed by marking the successor of every unconditional terminator.

Monotone-safe: more block starts ⇒ more code covered ⇒ strictly more
interference edges ⇒ a more conservative colouring (spill to the frame slot,
always correct), never less. Instruction stepping is unchanged.

**(b) `SafepointPublishPlan` + `plan_safepoint_publication` — the perf lever.**
Splits register-homed locals into the ones a GC-capable call must publish to
their frame slots and the ones it must not bother with:

* `reference_locals` — `find_reference_locals` ∪ a caller-supplied
  reference-parameter mask. A single `aload`/`astore` anywhere taints the slot
  for the whole method, so javac's cross-scope slot reuse cannot defeat it.
* `register_homed_reference_locals` — the only population whose register
  residency can hide a GC root.
* `publish_always` — bci-independent, no precondition.
* `publish_at[bci]` — additionally narrowed by liveness. **Precondition:**
  liveness has no exception-handler CFG edges (the handler table is not threaded
  into this module), which is safe only while
  `lib.rs::local_handler_reads_unsafe_local` keeps refusing to compile any
  method whose handler reads a non-parameter local.
* `no_reference_in_registers()` — the drop-in replacement for the
  `can_elide_self_call_register_spill` predicate.

Uncovered PCs fall back to the conservative set: the liveness table's default is
`0`, and a `0` that means "no block reaches this pc" must never be read as
"nothing is live". `live_locals_per_pc_with_coverage` supplies the parallel
coverage bitmap that makes the distinction possible.

**(c) tests** — CFG coverage after a terminator, interference in a
handler-shaped region, publish-plan behaviour on an int-only recursive kernel /
reference locals / dead references / spilled locals / uncovered PCs, and the
save-area sizing contract under register pressure and full spill.

### `jit/src/deopt.rs`

Verified and pinned, no behaviour change. `FrameValue` already carries
`Register(r)` / `RegisterLong(r)` / `RegisterRef(r)` / `XmmFloat` / `XmmDouble`;
`resolve_value` (`deopt.rs:1122`) reads them from `SavedRegisters`, which the
x64 frame-deopt stub fills with all 16 GPRs then all 16 XMMs. The x64 snapshot
builder (`build_and_record_deopt_point`, `x64.rs:9452-9475`) already **prefers
the register descriptor** over the slot descriptor for a register-homed local.
So deopt does not read a register-homed local's frame slot at all.

New tests: `register_homed_locals_ignore_a_stale_canonical_frame_slot` (fills
`[rbp-8]`, `[rbp-16]`, `[rbp-24]` with wrong values and proves none leak),
`register_descriptor_variant_selects_the_resumed_value_category`,
`inlined_caller_frames_resolve_register_locals_from_the_same_regfile`,
`monitors_resolve_register_homed_objects`, and
`saved_registers_layout_matches_the_stub_spill_region` (the 256-byte `#[repr(C)]`
ABI contract with the stub).

**Deopt thresholds, as measured:** `DeoptimizationLog` defaults to 20 deopts per
method before `should_give_up`. The "3 deopts → stay on C1" rule is separate and
lives in `jit/src/tiered.rs` (`MAX_DEOPTS_BEFORE_BAILOUT`, applied at
`tiered.rs:1190-1195`) — not in `deopt.rs`.

### `jit/src/escape_analysis.rs`

**Non-convergence now fails closed.** `propagate_escape_states` iterated to a cap
of 100 and, on exhaustion, returned a **partial** lattice. The lattice only joins
upward (`NoEscape < ArgEscape < GlobalEscape`), so a partial result
*under*-estimates escape, and both live consumers act on `== NoEscape`. An
unconverged run therefore handed `find_scalar_replacements` and
`find_lock_elisions` objects that actually escape — scalar replacement deletes
stores another party observes, lock elision drops a monitor another thread
contends on. Both silent wrong answers. Now: if the loop exits on the iteration
bound rather than a fixed point, `escalate_all_to_global` raises every node
(including nodes with no map entry, which `get_escape` reports as `NoEscape`) to
`GlobalEscape`, disabling both optimisations for that method.

**Which analysis is which.** There are two `analyze_escapes` in the JIT:

* `escape_analysis.rs::analyze_escapes(&Graph)` — sea-of-nodes, reached only from
  the tier-2 IR pipeline (`lib.rs:7073`).
* `x64.rs::analyze_escapes(code, code_len, &invokespecial_shapes)` (private,
  `x64.rs:4306`) — bytecode-level, runs for the vast majority of compiled
  methods, re-run authoritatively at `x64.rs:28350`.

The known **varargs-constructor receiver-null defect (BUG-05)** belongs to the
*x64* one — the `InvokeSpecialShape { arg_slots, is_trivial_void_init }` gate
that escapes any non-trivial `<init>` receiver is that analysis's. Fixing this
module does not touch it.

**`stack_allocatable` is dead output.** `EscapeAnalysisResult` computes it (every
`ArgEscape` allocation) and **no caller reads it** — stack allocation is not
implemented. It is not a flag-gated feature; there is no code to enable. Only
`scalar_replaceable` and `elide_locks` are consumed. Documented in the module
header and pinned by `stack_allocatable_holds_only_arg_escaping_allocations`.

### `jit/src/profile.rs`

**The gap the inlining sibling needs to know about:** before this change there
was **no per-call-site invocation count**. The store carried per-*method*
invocation counters (`increment_invocation`, keyed by
`(class_id << 32) | name_desc_hash`) and per-bci *receiver* maps — and receivers
are only recorded on the receiver-resolution path of `invokevirtual` /
`invokeinterface` (4 call sites in `vm/src/runtime/interpreter.rs`). There is no
receiver at an `invokestatic`, so a static call site had **zero** per-bci
evidence. Per-method counts cannot substitute: a call inside a rarely-taken
branch of a hot method looks identical to one on the hot path.

Added: `MethodProfile::call_sites` (kind-agnostic per-bci counter),
`record_call_site`, `ProfileStore::record_call_site{,_borrowed}` (behind the same
`is_profiling_enabled` gate as every other recorder), and — the part that works
*today* with no interpreter change — `call_site_count(pc) -> CallSiteEvidence`,
which prefers the direct counter and otherwise derives a count by summing the
already-populated receiver observations. `CallSiteEvidence` distinguishes
`None` (nothing observed) from a count of zero, because an inliner that treats
"no evidence" as "cold" would refuse to inline every static call in the VM.
`hot_call_sites(min)` returns a deterministically ordered ranking (count desc,
bci asc) so the same profile always produces the same artifact.

`call_sites` is carried through `get_profile` and `snapshot_all`.

**Caveat, stated plainly:** the new counter has no recorder yet — see cross-owner
request R3. Until that one-line interpreter hook lands, `call_site_count` answers
from receivers for virtual/interface sites and `None` everywhere else. That is
strictly more than existed before, and the `None` case is explicit rather than a
misleading zero.

### `jit/src/pgo.rs`

**`pgo.rs` is entirely dead code.** Verified 2026-07-26: no reference to `pgo::`,
`PgoRepository`, `CallSiteProfile`, `ReceiverTypeProfile`, `InliningPolicy`,
`DeoptProfile` or its `MethodProfile` exists anywhere in `jit/`, `vm/` or the
test suites. `lib.rs` declares `pub mod pgo;` and never uses it. **Every counter
in that file is permanently zero at runtime.**

This matters right now: the file advertises exactly the API an inlining change
reaches for — `inline_benefit_score`, `get_inline_candidates`, `is_monomorphic`,
`InliningPolicy::should_inline` — and every one of them would report "no
candidates / not hot / not monomorphic" for every call site in the VM, making
the optimisation indistinguishable from being switched off. That is the
landed-but-never-runs failure mode this repo already tracks in
`docs/internal/flag-census.md`. Added a prominent module-header warning pointing
at `crate::profile` (the live one) instead. No behaviour change.

---

## 3. GC-safety argument for values held in registers across a call

Nothing in this wave changes what lives in a register across a call. The
argument below is what licenses the codegen change requested in §4.

**Claim.** A register-homed local `i` for which
`(find_reference_locals | param_oop_mask) & (1 << i) == 0` may stay in its
callee-saved register across a GC-capable call without being published to
`[rbp-(i+1)*8]`.

1. **It cannot be an oop.** `find_reference_locals` taints slot `i` if *any*
   `aload`/`astore` (including `wide` forms) anywhere in the method touches it.
   A reference enters a local only via `astore` or as an incoming parameter, and
   the parameter case is covered by the unioned `param_oop_mask`. javac reuses
   slots across unrelated scopes, but the taint is method-wide, so reuse makes
   the mask *more* conservative, never less. This is the identical argument the
   pure-kernel register-home path already relies on
   (`x64.rs:2750-2752`, applied at `x64.rs:28288-28293`); the only change is
   applying it per-local instead of per-method.

2. **Therefore it cannot be an invisible root.** The scan is stack-only:
   `OopMapEntry` carries `frame_slot_offsets` and no register bitmap (the
   `reg_oops` TODO at `x64.rs:9946-9950` is still unimplemented), and the
   conservative walk covers `[scanner_sp, entry_sp)`. A register holding a
   non-reference contributes nothing either way — the consumer re-validates
   every word through `heap.is_object_address` regardless.

3. **A moving collector cannot invalidate it.** Primitives are not relocated.
   `emit_post_safepoint_reload` already reloads *only* oop register-locals for
   exactly this reason (`x64.rs:10686-10688`).

4. **Deopt cannot observe the stale slot.** `build_and_record_deopt_point`
   emits `FrameValue::Register`/`RegisterLong` for a register-homed non-oop
   local (`x64.rs:9469-9471` → `typed_local_frame_value`, `x64.rs:8380-8390`),
   and `resolve_value` reads the deopt stub's spilled GPR file. Pinned by
   `register_homed_locals_ignore_a_stale_canonical_frame_slot` in `deopt.rs`.

5. **The in-JIT exception path cannot observe it either.** The local load/store
   handlers read and write the register when `reg_for_local(idx)` is `Some`
   (`x64.rs:19546-19560`, `19802-19809`), so a handler compiled into the same
   frame sees the register. The *runtime* exception router reconstructs only
   `this` + declared parameters, and any method whose handler reads more than
   that is already refused compilation by
   `lib.rs::local_handler_reads_unsafe_local`.

**What the claim does NOT cover, and must stay spilled:**

* register-homed *reference* locals — unchanged, still published;
* `StackSlot::CalleeSaved` operand-stack entries marked as oops — already
  flushed by `flush_scratch_registers` (`x64.rs:16944-16968`);
* the blind full-GPR spill's *other* purpose: an oop staged in an argument or
  caller-saved register that no tracker tagged. Requests R1/R2 below deliberately
  keep that spill except where the existing `can_elide_self_call_register_spill`
  preconditions (`precise_maps`, no shadow stack, no moving young, exact oop
  marks, every operand-stack entry frame-resident) already prove it unnecessary.
* liveness narrowing (`publish_at`) if the RBC.6 admission gate is ever relaxed —
  see §5.

---

## 4. Cross-owner requests

`jit/src/x64.rs` is reserved for another wave; these are specified, not made.

### R1 — relax `can_elide_self_call_register_spill` (the Fibonacci win)

`jit/src/x64.rs:10128-10149`. Replace the all-or-nothing local test:

```rust
    || self.local_assignments.iter().any(Option::is_some)
```

with a reference-only test. The compiler already has everything needed: it
computes the reference mask at `x64.rs:28288` for the kernel-homes path
(`find_reference_locals(code, code_len, max_locals) | param_oop_mask`). Store it
on `Compiler` (say `local_ref_mask: u64`) unconditionally rather than only under
`kernel_reg_homes`, and test:

```rust
    || self
        .local_assignments
        .iter()
        .enumerate()
        .any(|(i, a)| a.is_some() && (i >= 64 || (self.local_ref_mask >> i) & 1 == 1))
```

Every other precondition in that function stays exactly as it is. Effect on
`int fib(int)`: both recursive call sites drop from the 14-store blind spill +
per-local publish + sp-id store down to `emit_safepoint_metadata_only`.
Soundness: §3 clauses 1-5, plus the function's own existing argument that the
callee prologue canonicalises its arguments before it can safepoint.

Rather than open-coding the mask test, prefer
`regalloc::plan_safepoint_publication(...).no_reference_in_registers()`, which
carries the argument and the tests.

### R2 — narrow `emit_pre_safepoint_spill`'s per-local publish loop

`jit/src/x64.rs:9958-9963`. Today:

```rust
for idx in 0..self.local_assignments.len() {
    if let Some(reg) = self.local_assignments[idx] {
        let off = self.local_offset(idx);
        self.emit_store_local(off, reg);
    }
}
```

Gate on the publish plan:

```rust
let publish = self.safepoint_publish.publish_at_bci(self.cur_bc_pc);
for idx in 0..self.local_assignments.len().min(64) {
    if (publish >> idx) & 1 == 0 { continue; }
    if let Some(reg) = self.local_assignments[idx] {
        let off = self.local_offset(idx);
        self.emit_store_local(off, reg);
    }
}
// locals >= 64 never receive a register home, so the loop bound is exact
```

This generalises R1 to every call site, not just the direct self-call. **Land R1
first and measure it alone** — R1 is the one with the existing preconditions
already proven, R2 widens the blast radius to every invoke.

Note `emit_oop_map_for_safepoint` (`x64.rs:10623-10641`) records oop locals'
canonical slots from `local_oop_masks[pc]`. If R2 lands, the publish set must be
a **superset** of what the oop map advertises for that pc, or the GC will scan a
slot the code no longer keeps current. `local_oop_masks[pc]` is per-bci and
oop-typed; `publish_at[bci]` is per-bci, oop-capable and liveness-narrowed —
they should agree, but this is the single invariant to assert (a
`debug_assert!(map_slots ⊆ published)` in `emit_oop_map_for_safepoint`) before
trusting R2.

### R3 — record per-call-site invocation counts

`vm/src/runtime/interpreter.rs`. The invoke dispatch already builds a
`profile::MethodKey` in several arms (e.g. `35567`, `35688`, `36462`) and
already calls `record_receiver_borrowed` for virtual/interface receivers
(`39240`, `39749`, `40235`, `40384`). Add, in the invoke dispatch for **every**
invoke kind:

```rust
shared.jit.profile_store.record_call_site_borrowed(cid, mn, md, saved_pc);
```

using the same borrowed-key carriers as the neighbouring
`record_branch_borrowed` calls. It is behind the same `is_profiling_enabled()`
atomic-load gate, so the cost when profiling is off is unchanged.

### R4 — thread the exception table into register allocation

Only needed if the RBC.6 admission gate is relaxed — see §5.

---

## 5. Assumptions about the concurrent inlining work (`jit/src/lib.rs`, IR pipeline)

For reconciliation at merge:

1. **Register pressure.** More inlining means more simultaneously-live locals per
   compiled body. The pool is 5 registers on System V. `color_graph`'s
   potential-spill heuristic (`use_count / degree`, loop-depth weighted) already
   handles over-subscription by spilling to frame slots, which is always correct.
   Expect the *marginal* value of a register home to fall as the budget rises;
   nothing breaks.

2. **The 64-local cap is a real ceiling.** Inlining raises `max_locals`. Locals
   `>= 64` silently receive no register at all. If the raised budget pushes hot
   methods past 64 locals, the allocator quietly stops helping them — measure
   `max_locals` distribution before and after. Raising the cap means widening
   every `u64` bitset in `regalloc.rs` plus `interference: Vec<u64>`.

3. **Inline caches at call sites.** Teaching the IR to lower calls with inline
   caches does not change the publish analysis: an IC dispatch is still a
   GC-capable call, and `plan_safepoint_publication` keys off local *types*, not
   call kinds.

4. **Deopt frame chains get deeper.** An inlined caller shares the physical frame
   and the register file; `reconstruct_frame_from_machine_state` flattens
   `FrameState.caller` into `caller_frames` and resolves each against the same
   `SavedRegisters`. Pinned by
   `inlined_caller_frames_resolve_register_locals_from_the_same_regfile`.

5. **Do not build hot/cold call-site decisions on `jit/src/pgo.rs`.** It is dead
   (§2). Use `crate::profile::MethodProfile::call_site_count` and treat
   `CallSiteEvidence::None` as *unknown*, not cold.

6. **Assumed unchanged by the sibling:** `regalloc::allocate_registers`'s
   signature, `RegAllocResult`'s fields, and the `x64.rs:28259` call site. If the
   IR pipeline grows its own allocator, `plan_safepoint_publication` is
   backend-agnostic (it takes an `assignments: &[Option<u8>]` slice) and applies
   unchanged.

### Latent dependency: exception-handler CFG edges

`build_cfg` has **no edges into exception handlers** — the handler table is not
passed to `regalloc`. A local defined in a `try` body and read in the handler is
therefore considered dead after its last normal-path read, and could be coloured
onto the same register as an unrelated local. That is not a live miscompile today
only because `lib.rs::local_handler_reads_unsafe_local` (`lib.rs:6579-6601`)
refuses to compile any method whose handler reads a non-parameter local.

**If that gate is ever relaxed** — the precise exceptional-frame handoff it is
waiting on — `regalloc` MUST gain handler edges first, or the allocator will
coalesce a live handler local. Concretely: add
`allocate_registers_with_handlers(..., handlers: &[(usize, usize, usize)])` that
adds a CFG edge from every instruction in `[start, end)` to `handler_pc`, keep
`allocate_registers` delegating with `&[]`, and pass `cached.exception_table`
from `x64.rs:28260`. `SafepointPublishPlan::publish_at`'s liveness narrowing
carries the same precondition (`publish_always` does not).

Fix (a) in §2 is the prerequisite half of this: handler *bodies* are now visible
to liveness; the missing piece is the *edge into* them.

---

## 6. Not done / open

* R1-R4 are unmade by design (file ownership).
* No build, no test run — this wave is nine concurrent agents on one host.
  `rustfmt --edition 2021 --check` is clean on all five owned files (the one
  pre-existing diff at `deopt.rs:507` predates this branch).
* `regalloc.rs` block-start change alters block partitioning for methods with
  code after a `return`/`athrow`. Colouring can only get more conservative, but
  it is a codegen-affecting change and deserves a QuickBench + one real suite run.
* `pgo.rs` is 1981 lines of dead code. Deleting it, or giving it a recorder, is a
  decision above this wave.
