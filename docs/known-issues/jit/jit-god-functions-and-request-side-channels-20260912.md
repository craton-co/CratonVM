# The JIT's compile drivers are god functions fed through thread-local side channels

**Status:** OPEN (architecture). Found by the 2026-09-12 JIT review.

## Where

| Function | File | Size at review time |
|---|---|---|
| `compile_bytecode` (the single-pass emitter's opcode walk) | `jit/src/x64/bytecode_walk.rs` | ~13,900 lines |
| `try_compile_inner` (the compile driver) | `jit/src/lib.rs` | ~6,300 lines |
| `compile_osr_artifact` | `vm/src/runtime/interpreter/jit_bridge.rs` | ~2,900 lines |

`try_compile` takes 22 positional parameters, 13 of them optional resolver
closures. It also depends on:

- thread-local "request" side channels, which a caller sets before the call and
  the driver consumes;
- 19 process-wide `set_*_direct_fn` setters.

## Why it matters

- **Stale side-channel state.** Nothing enforces that an early return from the
  driver clears the thread-local request it consumed. A bail that skips the
  clear leaves a stale request for the *next* compile on that thread, which
  then runs with another method's settings.
- **Untestable seams.** No part of admit, build, optimize, lower or publish can
  be driven in isolation without constructing most of a compile.
- **Merge friction.** Nearly every JIT change touches one of these three
  functions. The 2026-09-12 review branches conflicted there repeatedly.

## Direction

1. **`CompileRequest` as a struct.** Fold the positional parameters and
   resolver closures into one owned request value, built once by each door
   (method entry, eager first call, OSR). Pass it explicitly and delete the
   thread-local side channels. Where a channel must stay for now, give it an
   RAII scope that clears it on every exit, including unwinds.
2. **A staged pipeline.** Split the driver into `admit → build → optimize →
   lower → publish`. Each stage is a function with typed inputs and outputs,
   and a refusal is a value, not an early return from a 6,000-line body.
3. **Per-opcode-family emitter modules.** Split `compile_bytecode` into
   families (arithmetic, fields, arrays, invokes, control flow, allocation,
   monitors) behind one dispatch `match`, the way `x64/arith.rs` and
   `x64/objects.rs` already partly do.
4. **Replace the `set_*_direct_fn` setters** with a per-VM resolver table
   carried on the request.
