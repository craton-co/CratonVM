# Real-frame deoptimization

**Status:** Shipped (default on; `CRATONVM_DEOPT_REAL=0` opts out).

## What it does today

A failed speculative guard **resumes the interpreter at the trapping bci**
rather than re-running the whole method from its entry.

- **Metadata.** `jit/src/deopt.rs` carries `DeoptimizationPoint`, `FrameState`
  and `FrameValue`, including the typed slot locations `StackSlotRef`,
  `StackSlotLong`, `StackSlotFloat`, `StackSlotDouble`, plus `VirtualObject`
  and `MaterializationRequired`, and the `DeoptVerifier` (the `InvalidationManager` that
  used to sit beside it was deleted 2026-09-12).
- **Production codegen sets the per-method gate.** The single-pass x64 driver
  (`jit/src/x64/driver.rs`) sets `can_deopt_resume` from
  `!deopt_points.is_empty() && !has_elided_monitor`, and `can_osr_exit`
  likewise. This is the shipping backend, not a dormant IR path.
- **Both dispatch sinks consume it.**
  `vm/src/runtime/interpreter/jit_bridge.rs` calls
  `real_frame_deopt_resume_and_despeculate` when the gate is on; the resume
  itself (epoch staleness guard, de-speculation, cat-2/FP/ref slot
  reconstruction) is in `vm/src/runtime/interpreter/deopt_resume.rs`.
- Call-site canonical-boundary guard snapshots (`snapshot_pre_intrinsic_call`)
  are emitted from `jit/src/x64/bytecode_walk.rs`, with reason routing in
  `jit/src/x64/deopt_stubs.rs`.

**Cost:** a JIT frame reserves an extra 256 B for the `SavedRegisters` deopt
region while the feature is on.

Companion default-off diagnostics and experiments, all opt-in:
`CRATONVM_DEOPT_VERIFY` (structural/oop verifier), `CRATONVM_DEOPT_EAGER`
(force the reconstruct+resume path on every loop), `CRATONVM_SCALAR_DEOPT`
(guard-surviving scalar replacement), `CRATONVM_OSR_EXIT_TRANSFER`.

## What is not built yet

- **Guard-surviving scalar replacement** is default-off and refuses
  monitor-bearing graphs; see
  [`activate-ir-optimizer.md`](activate-ir-optimizer.md).
- **aarch64 parity is unaudited.** The gates and stubs live under
  `jit/src/x64/`; whether the aarch64 backend carries an equivalent has not
  been established.

## Goal

When a speculative JIT assumption fails at runtime (a null where the compiler
elided the check, a receiver type the inline cache didn't expect, an uncommon
branch), transfer control from the middle of the compiled method back into the
interpreter **at the trapping bci**, with the interpreter's locals / operand
stack / monitor set reconstructed from the JIT's machine state — instead of
re-running the whole method from entry.

Concretely: replace the current "return the `i64::MIN` sentinel → interpreter
re-executes the method from bci 0" model with HotSpot-style frame
materialization.

## Design

Four pieces, in dependency order.

### 1. Per-safepoint register→slot maps (codegen, `jit/src/x64.rs`)

At every point the JIT could deopt (guard sites, call returns, uncommon-trap
branches), emit a `DeoptimizationPoint` whose `frame_state` describes, **for
the trapping bci**, where every live interpreter local and operand-stack slot
currently is:

- a constant (`FrameValue::Int/Float/Object`),
- a machine register (`FrameValue::Register(reg)`),
- a native spill slot (`FrameValue::StackSlot(rbp_off)`), or
- a scalar-replaced object (`FrameValue::VirtualObject`).

This requires the codegen/regalloc to carry an **abstract interpreter state**
(locals[] + operand stack[]) alongside the physical state as it lowers each
bytecode — the same shadow the verifier-style abstract interpreter would track.
`jit/src/x64.rs` is the single-pass bytecode→x64 backend; the IR pipeline
(`jit/src/ir*.rs`) tracks SSA values and is the cleaner long-term home for this
(an IR node already knows its def site). Recommend: build the safepoint-map
emission on the **IR path** (`ir_lower.rs`) where value provenance is explicit,
and only later backport to the single-pass backend.

Index the emitted points by `native_offset` in a per-`CompiledMethod`
sorted table (binary-search by faulting PC), mirroring how a stackmap table is
keyed in HotSpot.

### 2. Deopt entry trampoline

Replace the `i64::MIN` return-value protocol with a **trampoline** the guard
jumps to:

- A guard that fails does **not** return to the caller; it `jmp`s (or calls) a
  per-method or shared deopt stub, passing the `native_offset` (or a small
  deopt-point index) and a pointer to the saved register file.
- The stub spills all live GPRs/XMMs to a known scratch area, then calls into
  the VM (`vm/src/runtime/jit_integration.rs`) with `(method, native_offset,
  &saved_registers, rsp/rbp)`.
- The VM looks up the `DeoptimizationPoint` for that `native_offset`.

This is the moment the conservative-vs-precise GC question becomes load-bearing:
during materialization the half-built frame must be a valid GC root set. See
`default-moving-young-gen.md` — precise JIT roots and deopt share the same
register→oop map infrastructure.

### 3. Frame reconstruction in the VM

Extend `reconstruct_frame` (`deopt.rs:565`) — or add a VM-side wrapper in
`vm/src/runtime/` — to **resolve** `FrameValue::Register`/`StackSlot` against
the saved register file / native stack passed by the trampoline:

1. For each local/stack slot, read the value: constant → use directly;
   `Register(r)` → read `saved_registers[r]`; `StackSlot(off)` → read
   `*(rbp + off)`.
2. For each `VirtualObject`, call the **real** GC-backed
   `materialize_virtual_objects` (replace the `deopt.rs:734` panic stub) to
   allocate + populate the heap object, registering it as a root before the
   next allocation can GC. Patch cyclic references in a second pass.
3. Build interpreter `Frame`s (`vm/src/runtime/frame.rs`): push the inlined
   caller chain (the `FrameState.caller` list) as separate interpreter frames,
   innermost first, each resuming at its own `bci`.
4. Restore `monitors` (re-enter the held monitors recorded in
   `MonitorInfo`) so `monitorexit` balance is preserved.
5. Resume interpretation at `frame_state.bci` — **not** bci 0.

### 4. Unlocking speculative guards + aggressive inlining

Once 1–3 land, the JIT can emit a guard + deopt where today it must emit a slow
path or decline the optimization:

- **Null-check elision** (`jit/src/null_check_elim.rs`): elide the check, guard
  with a deopt on the rare null instead of a full re-run.
- **Monomorphic/bimorphic inline caches**: inline the hot receiver's target,
  guard the receiver class, deopt on a megamorphic miss (`ReceiverTypeChanged`
  in `deopt.rs:36`).
- **Aggressive inlining**: inline a callee speculatively (CHA / single
  implementor), and when `ClassLoading` (`deopt.rs:38`) invalidates the
  assumption, deopt every affected frame. The `InvalidationManager` (deleted 2026-09-12) named in
  the module doc (`deopt.rs:13`) is the hook.
- **Scalar replacement that survives** (`jit/src/escape_analysis.rs`): an object
  proven non-escaping *along the fast path* can be scalar-replaced even if a
  guard failure would need it — `VirtualObject` re-materializes it on deopt.
  This is the second front in `activate-ir-optimizer.md`.

## Risks

- **GC root correctness during materialization** is the sharpest edge: a GC
  during step 4 with the half-built frame not yet rooted corrupts the heap. The
  `deopt.rs:680` doc and `default-moving-young-gen.md` must be co-designed.
- **Register/stack-slot map drift**: if the emitted map disagrees with the
  actual regalloc by one slot, deopt silently restores garbage. Needs a
  self-check mode that deopts *eagerly* at every safepoint in a test build and
  compares interpreter results.
- **Side-effect double-execution** is the bug this *fixes*, but a partial
  rollout (some guards deopt-resume, some still re-run) must never mix the two
  for the same method.
- **Inlined monitor balance**: held monitors across an inline boundary must be
  re-entered in the right order.

