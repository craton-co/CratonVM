# SEAM-01 — split `jit/src/x64.rs` (40,303 lines)

**Status:** not started. **Owns:** `jit/src/x64.rs` and `jit/src/x64/*`.
**Conflicts with:** every JIT lane, while it is in flight. Land it between
waves, not during one.

## Why this is worth doing

It is not aesthetics. Concrete costs observed this campaign:

* Four separate lanes needed to edit `x64.rs` in one wave and had to be
  serialised behind whichever one owned it, which pushed two of them into
  "expose an entry point, the orchestrator will wire it" — and one of those
  entry points is still unwired.
* `x64/isel.rs` sat **uncompiled** because a `mod` line was never added. In a
  40k-line file with 180 lines of module preamble, a missing declaration is
  invisible. Its ~1,480 lines of tests had never run; the first compile found a
  real cost-model bug.
* A duplicated `#[test]` attribute and an accidentally deleted test both went
  unnoticed in the same file in the same session.

## Current state

`x64.rs` already declares eleven submodules — `cpu_features`,
`switch_validation`, `reg_encoding`, `disp`, `simd_analysis`,
`bytecode_compat`, `licm`, `null_check_elim`, `escape_analysis`, `licm_int`,
`bce`, plus `vec_emit` and `isel`. So the pattern is established and the
mechanism works; the remaining bulk is the compiler proper.

The natural seams, from the structure that is already there:

| Candidate | Rough content |
|---|---|
| `x64/compiler.rs` | `Compiler` struct, `compile_with_param_slots`, the bytecode walk |
| `x64/emit.rs` | the raw instruction emitters (`emit_call_absolute`, `emit_mov_*`, VEX helpers) |
| `x64/deopt_stubs.rs` | stub emission and the four bci-baking sites |
| `x64/osr.rs` | the OSR publication region |
| `x64/frames.rs` | prologue/epilogue, frame layout, shadow stack |

## How to do it without breaking anything

`git mv`-style moves with **no logic changes in the same commit**. One
submodule per commit, each commit compiling and passing the full jit suite.
The `pub use` glob re-export pattern the existing submodules use keeps every
path resolving, and a glob caps each item at its own declared visibility, so
nothing becomes more public than it was.

Two things to watch, both real:

* A `pub(super)` type carried by a `pub(crate)` enum becomes a
  `private_interfaces` warning once it moves — that happened this wave with the
  loop-rewrite refusal enum.
* Test modules move with their code, and a test that was implicitly relying on
  file-private access will fail to compile. That is the split doing its job.

## How to verify

The jit suite before and after each commit, and — the check that actually
matters for a backend — a byte-for-byte comparison of compiled output for a
corpus of methods across the move. A pure move must not change one byte.

## What to refuse

Any "while I'm here" fix. A move commit that also changes behaviour is
unreviewable, and this file is where this project's silent wrong-code bugs
live.
