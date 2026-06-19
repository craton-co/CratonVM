# Real-Frame Deoptimization (the keystone)

> **Increment landed — IR-path deopt type source (oop + cat-2 safety).** The
> reconstruct→resume pipeline already existed on the IR path (gated
> `CRATONVM_IR_DEOPT_RESUME`, fired live by the div-by-zero guard
> `emit_div_zero_guard`), but it was **integer-only and silently unsafe for
> object/cat-2 slots**: `resolve_value` turned every `StackSlot` into `Int`, so a
> ref slot (e.g. an instance method's `this`) resolved to a *truncated pointer*
> that still passed the resume's all-`Int` check — resuming with a garbage `this`.
> What shipped (the doc's "single most under-scoped item" — the type source):
> - **Typed slot locations** (`jit/src/deopt.rs`): `FrameValue::StackSlotRef(i32)`
>   (resolves to `Object(word)` — the raw slot word IS the heap pointer) and
>   `FrameValue::Unsupported` (a live slot that can't yet be precisely
>   reconstructed — cat-2 `long`/`double`, FP-in-slot — which forces the **safe
>   re-run** instead of fabricating/truncating a value).
> - **IR producer** (`jit/src/ir_lower.rs`): `frame_value_for` now routes each
>   spilled value by its IR `ty` via `typed_stack_slot` — `Ref`→`StackSlotRef`,
>   `Int`→`StackSlot`, `Long`/`Double`/`Float`→`Unsupported` (long constants too).
> - **VM resume** (`vm/src/runtime/interpreter.rs`): `ir_deopt_frame_values` maps
>   `Object`→`Value::Object` (a real reference), keeps `Int`/`Undefined`, and
>   returns `None` (→ re-run) for `Unsupported`/`Float`/virtual/unresolved — the
>   safety contract that a not-yet-resumable slot re-runs rather than resumes with
>   garbage. The oop is read synchronously at the guard (no intervening Java
>   alloc, hence no GC) so the pointer stays valid.
> - **Diagnostic:** `CRATONVM_DBG_DEOPT` traces each deopt as `PRECISE resume …`
>   (with the reconstructed locals) or `FALLBACK re-run … (reason)`.
> - **Validation.** jit `reconstruct_resolves_typed_slots` (StackSlotRef→Object,
>   StackSlot→Int, Unsupported pass-through) + vm `ir_deopt_frame_values_maps_object_and_int`;
>   68 jit-deopt + 17 ir_lower + the vm test all green. **Live end-to-end PROOF**
>   (debug binary, JDK 25, `CRATONVM_IR_DEOPT_RESUME=1`): a pure-int IR-compiled
>   method `d(II)I` div-by-zero deopts and **precisely resumes at the div bci**
>   (`[cratonvm-deopt] PRECISE resume … at bci=2`), throwing `ArithmeticException`
>   correctly — the mechanism fires on a live VM.
> - **Honest scope / next step.** The object/cat-2 arms are **unit-validated, not
>   yet live-exercised**: the *current* IR-path selection routes object-bearing
>   and side-effecting methods (e.g. an instance `g(I)I`, anything with
>   `putstatic`) to the **single-pass x64 backend**, which has no IR deopt — so
>   they never reach this resume today. This increment is the *correct
>   prerequisite* (the type source the x64 backport doc names as most-under-scoped)
>   that makes object resume sound once a producer compiles such methods —
>   **broaden the IR-path selection to object/side-effecting methods, or land the
>   x64 backport** (which compiles everything) to make it fire live. Cat-2
>   two-slot expansion + FP-slot resolution + `materialize_virtual_objects`
>   (still a panic stub) remain the follow-ups.

Status: design / not started. XL. **This is the keystone** — almost every
other aggressive JIT optimization (speculative guards, aggressive inlining,
scalar replacement that survives a guard failure) is unsafe until the JIT can
rebuild a *precise* interpreter frame at the exact trapping bci. Until then the
JIT is limited to optimizations that are provably correct without a fallback.

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

## Current state (cited)

The deopt *data model* already exists and is well-shaped; what's missing is the
machine-state → frame plumbing and the actual mid-method resume.

- **The sentinel re-run model.** A JIT method signals deopt by returning
  `i64::MIN` and setting an out-of-band flag. The interpreter consumes it in
  two places and falls back by **re-running the method from entry**:
  - `vm/src/runtime/interpreter.rs:16966` takes `take_jit_deopt_pending()`;
    `:17043` `if result == i64::MIN && deopt_signaled { ... }` drops into the
    interpreter slow path.
  - `vm/src/runtime/interpreter.rs:17322` `if result == i64::MIN &&
    deopt_signaled { return Ok(None); }` — `Ok(None)` means "no JIT value
    produced, run it interpreted". Re-execution starts at the method's first
    bytecode. The surrounding comments explicitly note that **re-running a
    method with side effects double-executes them** (`:17042`, `:17319`), which
    is exactly why today's JIT cannot speculate past any side-effecting bc.
  - The signal helpers live in `vm/src/jit/helpers.rs`
    (`take_jit_deopt_pending`, `take_jit_pending_exception`,
    `take_jit_pending_npe`, `take_jit_pending_aioobe`).
- **The frame data model (`jit/src/deopt.rs`).** Already defines everything a
  real deopt needs, but it is **not fed by the codegen** and **not consumed by
  a resume path**:
  - `FrameValue` (`deopt.rs:73`) with `Register(u8)`, `StackSlot(i32)`,
    `Object(u64)`, `Int`, `Float`, and `VirtualObject(VirtualObjectState)`.
    The `Register`/`StackSlot` variants are exactly the "value lives in machine
    state" cases that frame materialization must resolve.
  - `FrameState` (`deopt.rs:107`): `method_key`, `bci`, `locals`, `stack`,
    `monitors`, and a `caller: Option<Box<FrameState>>` chain for inlined
    frames.
  - `DeoptimizationPoint` (`deopt.rs:128`): `native_offset`, `bci`, `reason`,
    `action`, `speculation_id`, `frame_state`. This is the per-safepoint record
    that must be emitted by codegen and indexed by native PC.
  - `reconstruct_frame` / `reconstruct_frame_owned` (`deopt.rs:565` / `:615`)
    already walk a `FrameState` (+ inlined caller chain) into a
    `ReconstructedFrame`. **But they assume the `FrameValue`s are already
    resolved constants** — there is no register/stack-slot reader.
  - `materialize_virtual_objects` (`deopt.rs:704` test-only, `:734`
    panicking stub) is **explicitly unwired** and hard-gated: the non-test body
    `panic!`s rather than mint fake heap addresses. Its doc comment
    (`deopt.rs:680`) is the canonical spec for GC-backed re-materialization:
    allocate via the live TLAB (may GC — the deopt frame must already be a
    valid root set), write the header, recursively materialize fields, patch
    cyclic back-references.
  - `DeoptimizationLog` (`deopt.rs:159`) records events and drives the
    give-up-after-N-deopts policy (`should_give_up`, `most_common_reason`) —
    this part is live-usable today.
- **Tiered hook (`jit/src/tiered.rs`).** `CompilationTier` (`:40`) and the
  C1↔C2↔Interpreter transition graph (`:18`) describe deopt as the C2→C1 /
  C2→Interpreter edge. The manager is consulted at
  `interpreter.rs:14181` (`on_method_invocation`) but its tier recommendation
  is dropped (`let _recommended_tier = ...`). See `wire-tiered-manager.md`.

So today: data types exist, codegen emits none of them, resume is "re-run from
bci 0".

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
  assumption, deopt every affected frame. The `InvalidationManager` named in
  the module doc (`deopt.rs:13`) is the hook.
- **Scalar replacement that survives** (`jit/src/escape_analysis.rs`): an object
  proven non-escaping *along the fast path* can be scalar-replaced even if a
  guard failure would need it — `VirtualObject` re-materializes it on deopt.
  This is the second front in `activate-ir-optimizer.md`.

## Implementation steps (ordered)

1. **Abstract-state tracking in the IR lowerer.** Make `ir_lower.rs` carry the
   interpreter locals[]/stack[] shadow and, at each candidate safepoint, snapshot
   each slot's provenance into a `FrameState`. No behavior change yet (emit +
   discard).
2. **Safepoint table on `CompiledMethod`.** Store the `Vec<DeoptimizationPoint>`
   sorted by `native_offset`; add a PC→point lookup. Verify offsets against the
   final assembled code (relocations).
3. **Deopt trampoline + register save.** Add the stub and the
   `jit_integration` entry point; route one *non-speculative* guard (e.g. an
   existing `jit_uncommon_trap`) through it and confirm it reconstructs the
   frame and resumes at the right bci with a trivial method (no virtuals, no
   inlining).
4. **GC-backed `materialize_virtual_objects`.** Replace the `deopt.rs:734`
   panic with the real allocator-threaded implementation; root the half-built
   frame. Gate behind `CRATONVM_DEOPT_REAL` while it soaks.
5. **Inlined-frame reconstruction.** Handle the `FrameState.caller` chain →
   multiple interpreter frames.
6. **Flip the first real speculation.** Convert null-check elision (or the
   monomorphic inline cache) to guard+deopt; measure deopt rate via
   `DeoptimizationLog`.
7. **Retire the `i64::MIN` re-run path** once parity is proven; keep it as the
   fallback for methods whose safepoint maps can't be built.

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

## Effort

XL. Realistically 3 landable phases: (A) safepoint maps + trampoline + simple
resume (no virtuals, no inlining) — large; (B) GC-backed virtual-object
materialization — medium but GC-coupled; (C) inlined-frame chains + flipping
real speculations — large. (A) alone is the gate for everything else.
