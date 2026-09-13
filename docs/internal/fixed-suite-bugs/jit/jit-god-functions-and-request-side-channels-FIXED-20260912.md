# FIXED: the JIT's compile drivers were god functions fed through thread-local side channels

**Status: FIXED 2026-09-12.** Found by the 2026-09-12 JIT review as finding #69
(architecture).

## Where

| Function | File | Size at review time | Size after fix |
|---|---|---|---|
| `compile_bytecode` (the single-pass emitter's opcode walk) | `jit/src/x64/bytecode_walk.rs` | ~13,900 lines | ~420 lines (walk loop + 182-line dispatch) |
| `try_compile_inner` (the compile driver) | `jit/src/lib.rs` | ~6,300 lines | ~830 lines (admission + scan driver) |
| `compile_osr_artifact` | `vm/src/runtime/interpreter/jit_bridge.rs` | ~2,900 lines | ~260 lines (gates + reuse) |

`try_compile` took over 20 positional parameters, 13 of them optional resolver
closures. It also depended on:

- thread-local "request" side channels, which a caller set before the call and
  the driver consumed;
- 21 process-wide `set_*_direct_fn` setters.

## Why it mattered

- **Stale side-channel state.** Nothing enforced that an early return from the
  driver cleared the thread-local request it consumed. A bail that skipped the
  clear left a stale request for the *next* compile on that thread, which
  then ran with another method's settings.
- **Untestable seams.** No part of admit, build, optimize, lower or publish could
  be driven in isolation without constructing most of a compile.
- **Merge friction.** Nearly every JIT change touched one of these three
  functions. The 2026-09-12 review branches conflicted there repeatedly.

## The fix

The architectural rework landed in five staged steps on `fix/jit-review-r2-20260912`
and merged into `dev`:

### 1. `CompileRequest` as a struct (commit `24a254fce`)

`CompileRequest<'a>` in `jit/src/compile_request.rs` folds all 30+ positional
parameters and 13 optional resolver closures into one owned/borrowed request
value passed by reference.

Thread-local side channels were eliminated or bounded:
- `SELF_CALL_IDENTITY_STABLE` became `CompileRequest::self_call_identity_stable`,
  explicitly populated by the caller.
- `LOCAL_HANDLERS_DISARMED` is passed explicitly to `try_compile_inner`.
- `LOCAL_HANDLERS_ARMED` is scoped via `LocalHandlersArmedScope`, guaranteeing
  cleanup on exit and unwind.
- `SITE_TRAPS_THIS_BUILD` and `STRING_ACCESS_SITE_PCS` are scoped via
  `ir::IrBuildResultsScope` on entry and drop.
- `try_compile_request` became the primary entrance for compilation requests;
  positional wrappers remain only as thin adapters for legacy tests.

### 2. Per-VM `DirectHelperTable` and `BackendRequest` (commits `5530d3a85`, `3324f4130`)

- The 21 process-wide `set_*_direct_fn` function pointer setters and their
  underlying static cells were deleted.
- The VM now constructs a `DirectHelperTable` (`jit/src/direct_helpers.rs`) per VM
  instance via `jit::helpers::direct_helper_table(shared)` and attaches it to
  `CompileRequest::direct_helpers`.
- The single-pass backend's staged thread-locals (compact field info, verified
  max stack, exception ranges, kernel register homes, precise exception frames,
  and protected ranges) were converted into `x64::BackendRequest`, passed by value
  into `compile_with_request`.

### 3. Staged compile drivers (commits `41881d1b0`, `d19c2964e`)

- `try_compile_inner` in `jit/src/lib.rs` was decomposed into typed stages:
  - Admission and pre-scan stay in the driver.
  - The optimizing IR pipeline was extracted to `ir_tier(...) -> Option<CompiledMethod>`.
  - The single-pass pipeline was extracted to `single_pass_tier(...) -> Option<CompiledMethod>`.
  - Fall-through from `ir_tier` returning `None` cleanly flows into `single_pass_tier`.
- In `vm/src/runtime/interpreter/jit_bridge.rs`, `compile_osr_artifact` was split:
  - `compile_osr_artifact` manages early gates, census, and cache reuse.
  - `compile_osr_body` resolves per-site metadata tables, invokes the backend,
    and publishes the artifact.

### 4. Per-opcode-family emitter modules (commit `1f9f80e09`)

The ~13,900-line monolithic opcode walk in `jit/src/x64/bytecode_walk.rs` was split
into seven domain modules under `jit/src/x64/`:
- `op_local_stack.rs` — local variables, constants, stack manipulations.
- `op_array.rs` — array loads, stores, and lengths.
- `op_arith.rs` — integer/floating-point arithmetic, bitwise, and conversions.
- `op_control.rs` — branches, comparisons, switches, and returns.
- `op_field.rs` — getfield, putfield, getstatic, putstatic.
- `op_invoke.rs` — invokevirtual, invokespecial, invokestatic, invokeinterface, invokedynamic.
- `op_object.rs` — new, newarray, checkcast, instanceof, monitors, athrow.

The main `compile_bytecode` loop was reduced to a 182-line opcode match dispatching
to `WalkFamily::walk_*`, leaving control transitions to explicit `WalkStep` values.

## Regression coverage

- `jit/src/compile_request.rs`: unit tests verifying RAII cleanup on normal exit
  and panic unwinds for `LocalHandlersArmedScope` and `IrBuildResultsScope`.
- `jit/tests/process_global_statics_ratchet.rs`: ratchet dropped 747 -> 717
  process-wide globals after removal of the direct helper statics.
- `jit/src/x64/tests.rs`: `the_dispatch_arm_parser_reads_the_real_match` and
  `the_unlowered_opcode_catch_all_names_itself` pin the opcode family dispatch table.
- Complete `cratonvm-jit` test suite (2,491 unit tests passing).
