# Cooperative JIT Safepoint Polls

Status: **OFF by default**, opt in with `CRATONVM_JIT_SAFEPOINT_POLLS=1`. First
cut — lays the ABI/codegen/runtime plumbing and covers the two lowest-risk
sites (context-method entry, `goto`-shaped loop back-edges). Not yet the
default; see "What this does NOT fix yet" before flipping it.

## Problem & motivation

JIT-compiled code never polls for a stop-the-world (STW) request. The
interpreter does: it polls an `Acquire` load of `gc_barrier.stw_requested`
(`vm/src/threading/gc_barrier.rs`) once per dispatch iteration
(`vm/src/runtime/interpreter.rs`, the `safepoint_check` hit path), deposits a
fresh root snapshot, and arrives at the barrier. A thread executing
JIT-compiled code has no equivalent — the only way the collector can stop it
is to suspend it at the OS level (`SuspendThread` + `GetThreadContext`,
`vm/src/jit/xt_root_scan.rs`) and conservatively scan its registers and stack
band for anything that looks like a heap pointer.

That conservative, OS-suspend-based scan is why **any thread executing JIT
code today forces the young collector into its non-moving mode**: a
conservatively-discovered "maybe a pointer" slot can never be safely
rewritten, so the moving (Cheney) collector is disabled while a JIT frame is
live (see `docs/feature-designs/precise-jit-maps-default.md` and
`docs/feature-designs/default-moving-young-gen.md` for the throughput
consequences). Giving JIT-compiled code its own cooperative safepoint — so it
can park itself the same way the interpreter does, instead of always being
forcibly suspended — is the prerequisite for eventually retiring the
`SuspendThread`/conservative-register-scan path for cooperating threads.

This change does **not** retire that path yet. It adds the poll machinery
behind an opt-in flag so it can be validated in isolation (correctness,
overhead) before anything downstream is allowed to depend on it.

## Design

### New `JitRuntimeHelpers` fields (`jit-api/src/lib.rs`)

Two `usize` fields, appended at the end of the struct (as every prior
addition has been, to keep existing golden ABI offsets stable):

- `safepoint_flag_addr` — the address of the single STW-requested flag byte
  (`GcBarrier::stw_requested`), NOT a function pointer. Classified
  `FieldKind::Offset` (like `region_bounds_addr`): `0` means "polling
  disabled", and the validator does not require it non-null.
- `safepoint_slow_path` — the address of
  `vm/src/jit/helpers.rs::jit_safepoint_slow_path`, an
  `extern "C" fn(vm_ptr: i64)`. Classified `FieldKind::OptionalPtr`. Always
  wired by `build_helpers` (the function always exists); `safepoint_flag_addr`
  is what actually gates whether the JIT ever emits a `CALL` to it.

Every field in `JitRuntimeHelpers` is a plain `usize`, not a typed pointer —
this matches the struct's existing convention (see e.g. `frame_record`,
`get_current_thread`, `region_bounds_addr`, all of which are conceptually
pointers/addresses stored as `usize`) and keeps the `#[repr(C)]` golden-offset
layout, the `helper_fields!` macro, and the bulk validator working unchanged.

**All construction sites of `JitRuntimeHelpers { .. }` must list both new
fields** (Rust struct-literal syntax requires every field unless `..base` is
used). In-scope sites were updated as part of this change:

- `jit-api/src/lib.rs` (`make_helpers()` test constructor, the
  `test_helpers_zero_values` literal)
- `jit/src/x64.rs` (`test_helpers()`)
- `vm/src/jit/helpers.rs` (`build_helpers()`, the real wiring)

The following construction sites were **out of scope** for this change and
still need the two new fields added before they will compile:

- `jit/src/ir_lower.rs::no_helpers()` — uses `unsafe { std::mem::zeroed() }`,
  so it is unaffected (all fields are `usize`, zero is a valid bit pattern for
  every one of them).
- `jit/tests/ir_vs_singlepass.rs` (three literals: the top-level helpers
  builder, `field_helpers()`, `frem_helpers()`)
- `jit/tests/intrinsic_string_access.rs`
- `jit/tests/intrinsic_string_search.rs`
- `jit/tests/intrinsic_long_bits.rs`
- `jit/tests/intrinsic_crc32.rs`
- `jit/tests/intrinsic_int_bits.rs`
- `jit/tests/intrinsic_arrays_sort.rs`
- `jit/tests/intrinsic_arrays_ops.rs`
- `jit/tests/intrinsic_arraycopy.rs`
- `jit/tests/differential.rs`

Each of these builds a `JitRuntimeHelpers { .. }` literal field-by-field
(mirroring `jit/src/x64.rs::test_helpers()` before this change); add
`safepoint_flag_addr: 0, safepoint_slow_path: 0,` to each.

### `GcBarrier::stw_requested_flag_addr` (`vm/src/threading/gc_barrier.rs`)

```rust
pub fn stw_requested_flag_addr(&self) -> *const u8 {
    &self.stw_requested as *const AtomicBool as *const u8
}
```

`AtomicBool` has the same in-memory representation as `bool` (a single byte),
so the JIT poll reads this address with a plain non-atomic byte `TEST` rather
than an atomic instruction — a false-negative race (observing `false` a few
cycles before a concurrent `store(true, Release)` is globally visible) only
defers detection to the next poll, the same bound the interpreter's own poll
already accepts.

**Stability contract.** `GcBarrier` is a plain field of `SharedVm` (not
independently boxed), and `SharedVm` is always held behind `Arc<SharedVm>` for
the life of the VM (`Vm::new()` allocates it once via `Arc::new` and never
moves or reallocates it). A caller that keeps the owning `Arc<SharedVm>` alive
may treat this address as valid for the VM's entire lifetime.

### `jit_safepoint_slow_path` (`vm/src/jit/helpers.rs`)

```rust
pub unsafe extern "C" fn jit_safepoint_slow_path(vm_ptr: i64) {
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some((thread, _guard)) = jit_thread_mut() {
        crate::runtime::interpreter::safepoint_check(vm, thread);
    }
}
```

This recovers the current `JvmThread` the same way every other JIT helper
does (the `JIT_THREAD` thread-local via `jit_thread_mut()`), then calls
`interpreter::safepoint_check` directly — the exact function the
interpreter's own poll hit calls (`vm/src/runtime/interpreter.rs`, guarded by
the `stw_requested` load at the top of the dispatch loop). `safepoint_check`
retires the thread's TLAB, drains its SATB buffer, invalidates the JIT
conservative-scan cache and calls `update_root_snapshot`, then arrives at the
barrier (`arrive_and_wait_auto`) and applies the resulting pointer map. A
thread that parks here is handled by the *same* GC-side machinery as an
interpreter frame at a poll hit — no new STW code path was introduced.

`build_helpers()` wires `safepoint_flag_addr` from the process-global VM
handle (`crate::native::jni::process_vm()`), which `Vm::new()` publishes
before any bytecode runs (see that function's own doc comment: "for EVERY
creation path"). Since `build_helpers()` is only ever invoked once the
interpreter has started running bytecode and decided a method is
JIT-eligible, `process_vm()` is always populated by the time it matters. If
it were ever `None` (e.g. a unit test that calls `build_helpers()` directly
without going through `Vm::new()`), `safepoint_flag_addr` is left `0`, which
is the same "not wired" contract every other optional helper field already
uses — polling silently stays off, nothing else changes.

Known caveat inherited from `process_vm()` itself: it is a single process-wide
`Weak` cell ("last writer wins"), documented as fine because there is one real
VM per process; an in-process test fixture that constructs a *second* `Vm`
while JIT-compiling methods for the *first* would get the second VM's flag
address baked into the first VM's compiled code. This is the same caveat
`crate::native::jni::set_global_shared_vm_for_hooks` already carries; it is
not a new risk introduced here.

### Codegen (`jit/src/x64.rs`)

Gate: `jit_safepoint_polls_enabled()` (OnceLock-cached read of
`CRATONVM_JIT_SAFEPOINT_POLLS`, default off), following the exact pattern of
the neighboring `CRATONVM_JIT_SAFEPOINT_REG_SPILL` family near `x64.rs:2557`.

Poll shape, emitted by `Compiler::emit_safepoint_poll`:

```asm
MOV  R11, imm64            ; helpers.safepoint_flag_addr
TEST byte ptr [R11], 0xFF  ; nonzero => STW requested
JZ   .no_poll
  <emit_pre_safepoint_spill>                   ; frame-slot oop map valid
  MOV  ARG_REGS[0], [rbp - heap_local_offset]   ; vm_ptr
  CALL helpers.safepoint_slow_path
  <emit_oop_map_for_safepoint>                  ; precise/shadow modes only
.no_poll:
```

x86-64 has no `CMP [m64], imm` form that takes a bare absolute address, so the
flag address is first materialized into the scratch register R11 (never a
Java-local home — see `LOCAL_REGS` — nor an `ARG_REGS`/`SCRATCH_REGS` member,
so it is always free to clobber) via `MOV R11, imm64`, mirroring how
`emit_guarded_getfield_receiver_check` already materializes the
`region_bounds_addr` absolute address into a register. The byte-sized `TEST`
reuses the existing `emit_test_mem8_imm8` primitive (already used for the
`GC_FLAG_COMPACT` header-bit check). The spill/call/oop-map bracketing
mirrors the `self_call_stack_guard` call sequence (the native-stack-headroom
guard at direct self-recursive calls) exactly — same ordering, same gating on
`precise_maps || shadow_enabled` for the oop-map step.

Two call sites:

1. **Method entry**, context methods only (`self.needs_heap` — a pure method
   has no `vm_ptr` frame slot to load the slow path's argument from; see
   "What this does NOT fix yet"). Emitted last in `emit_prologue`, after every
   other prologue effect (param homing, frame-record, shadow-stack thread
   cache) has already committed.

   The prologue call goes through `emit_safepoint_poll_prologue`, a thin
   wrapper that temporarily sets `self.cur_bc_pc` to `u32::MAX` before
   delegating to `emit_safepoint_poll`, then restores it. This matters only
   under `CRATONVM_PRECISE_JIT_MAPS` (default-on): `emit_prologue` runs
   *before* `compile_bytecode`'s per-instruction loop ever assigns
   `self.cur_bc_pc`, so at the prologue's poll site `cur_bc_pc` still holds
   its `Compiler::new` default of `0` — the same value a genuine safepoint at
   the method's first real bytecode instruction (bci 0; a very common shape,
   e.g. a constructor or method opening with `new`/an `invoke`) would use.
   Both `emit_pre_safepoint_spill` and `emit_oop_map_for_safepoint` key their
   precise-map bookkeeping off `cur_bc_pc` (the frame's sp-id slot store, and
   the pushed `OopMapEntry::bytecode_pc`), so recording the prologue poll
   under the same bci as a real bci-0 safepoint would let the GC's
   sp-id-keyed oop-map lookup for a thread parked at *one* of the two
   safepoints match the *other* one's differently-shaped frame-slot list —
   a live-oop under-reporting hazard. `native_pc_offset` (the primary,
   always-unique-per-call-site key on `OopMapEntry`) never collides; only the
   auxiliary `bytecode_pc` cross-check does. `u32::MAX` can never equal a
   genuine bytecode index (no method is anywhere near 4 GiB), so the swap
   removes the collision at zero cost to the common (non-colliding) case.

2. **Loop back-edges**, specifically `goto` (bytecode `0xa7`) instructions
   whose target is backward (`target_pc <= pc`), in `compile_bytecode`'s
   `0xa7` arm, immediately before the final `JMP rel32` to the loop header.
   This is the same "back edge" definition the compiler's own natural-loop
   detector (`detect_natural_loops`) already uses, and it applies uniformly
   whether or not the loop body was unrolled (the unroll duplicator emits
   straight-line copies that all still funnel into the one final backward
   `JMP`, so a loop unrolled N times polls once per N logical iterations, not
   once per copy — a minor latency-to-notice tradeoff under heavy unrolling,
   not a correctness gap).

## What this does NOT fix yet

- **Pure methods are never polled.** A method with no `vm_ptr` frame slot
  (`needs_heap == false`) has nowhere to load `jit_safepoint_slow_path`'s
  argument from without adding a second hidden parameter to every pure-method
  call site — deferred to a follow-up. A pure method that loops for a long
  time between calls into any context method is still only reachable via the
  `SuspendThread` conservative path.
- **Only `goto`-shaped back edges are polled.** The far more common
  javac-emitted loop idiom compiles a `for`/`while` as a forward `goto` to a
  bottom-of-loop condition check followed by a *conditional* backward branch
  (`if_icmpXX`/`ifXX`/`if_acmpXX`). That backward edge is not polled by this
  first cut: safely inserting a taken-path-only poll into a conditional `Jcc`
  site requires inverting the condition and restructuring the branch into
  "Jcc-over-{poll + unconditional JMP}", a materially larger and riskier
  codegen change to a 40k-line file that could not be compile-tested as part
  of this change. **This means a hot loop using the canonical for/while
  shape is currently NOT interrupted by a JIT-side poll even with the flag
  on** — method-entry polling and the pre-existing `SuspendThread` path
  remain the actual safety net for such loops. Closing this gap is the
  highest-value follow-up before this flag could plausibly become the
  default.
- **`SuspendThread`/`xt_root_scan.rs` is still authoritative.** This change
  adds a cooperative path; it does not remove or bypass the OS-suspend
  conservative scan, and nothing yet skips the non-moving-GC-under-JIT
  restriction. Retiring `xt_root_scan.rs` for polling threads is a distinct,
  larger follow-up that additionally needs: back-edge coverage for
  conditional branches (above), a decision on how to handle a thread
  genuinely stuck inside a pure-method call with no poll, and (per
  `docs/feature-designs/precise-jit-maps-default.md`'s Stage B) a
  runtime-verified guarantee that every safepoint a thread can be parked at
  has complete oop-map coverage before the collector is allowed to treat any
  JIT-visible oop as movable.

## Enabling

```
CRATONVM_JIT_SAFEPOINT_POLLS=1
```

Off by default; the orchestrator will flip the default once validated
(correctness under `CRATONVM_DBG_VERIFY_OOP_MAPS` and the GC-root acceptance
lane, throughput cost of the extra poll at every context-method entry and
`goto` back-edge).

## Integration risks

- Struct-literal breakage at every out-of-scope `JitRuntimeHelpers { .. }`
  construction site listed above until the two new fields are added there.
- The method-entry poll's `emit_pre_safepoint_spill()` call spills every
  used callee-saved GPR (and, under `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`,
  every allocatable GPR — including R11, whose spilled value at that instant
  is the flag address itself, harmless dead data the conservative scanner's
  `heap.is_object_address` check will reject) to its reserved frame slot on
  *every* context-method invocation once the flag is on, even when no STW is
  pending — this is the fixed per-call overhead to weigh against the
  benefit, and is not yet measured.
- The R11 scratch register is confirmed unused by `LOCAL_REGS`, `ARG_REGS`,
  and `SCRATCH_REGS` (all checked against this file's own constant
  definitions), so the poll cannot clobber a live Java value — but this
  assumption should be re-verified if the register allocator's candidate
  sets ever change.
