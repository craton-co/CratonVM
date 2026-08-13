# SEAM-01 — split `jit/src/x64.rs` — DONE

**Status: complete 2026-08-03.** `jit/src/x64.rs` went from **40,588 lines to
2,503** across seventeen commits, each one verified separately, plus one lift
and two fixes the split turned up. Original problem statement preserved at the
bottom.

## The result

| | before | after |
|---|---|---|
| `x64.rs` | 40,588 | 2,503 |
| files in `x64/` | 13 | 30 |

What is left in `x64.rs` is the part that is genuinely one thing: the module
preamble, `StackSlot` / `ShadowHome` / the `Compiler` struct, `Compiler::new`,
four small queries, `patch_branches` / `patch_self_calls`, and the `mod` wiring.
Everything that emits or decides is behind a name.

The doc proposed five files. Seventeen landed, because three of the five proposed
names covered more than one concern once you looked at what was actually inside
them — `emit.rs` as proposed would have absorbed the vectorised loop bodies, the
array accessors and the arithmetic lowering along with the instruction
encodings.

| file | lines | what |
|---|---|---|
| `x64/bytecode_walk.rs` | 10,145 | `compile_bytecode` — the per-opcode dispatch, a quarter of the original file on its own |
| `x64/tests.rs` | 13,131 | the unit tests |
| `x64/driver.rs` | 2,182 | `compile`, `compile_with_param_slots`, the metadata-staging thread-locals |
| `x64/arith.rs` | 1,711 | peepholes, division strength reduction, FP binops and `fcmp` |
| `x64/inlining.rs` | 1,408 | `try_emit_inline` and the second bytecode walk |
| `x64/emit.rs` | 1,337 | one method per instruction form, deciding nothing |
| `x64/deopt_stubs.rs` | 1,179 | deopt points, exception checks, the out-of-line stub block |
| `x64/simd.rs` | 1,111 | whole-loop AVX2 and bulk-store replacements |
| `x64/safepoint.rs` | 1,072 | polls, shadow-stack publication, oop maps, the two elision proofs |
| `x64/frames.rs` | 864 | frame layout, stack bangs, prologue and epilogue |
| `x64/objects.rs` | 868 | inline TLAB, compact `putfield`, card marks, the string layout |
| `x64/operand_stack.rs` | 844 | the compile-time operand-stack simulation |
| `x64/loop_unroll_admission.rs` | 876 | the unroller-admission tests |
| `x64/flag_and_header_contracts.rs` | 665 | the flag-skew and header-offset contracts |
| `x64/osr.rs` | 474 | OSR exit maps, and OSR entry-metadata publication |
| `x64/arrays.rs` | 419 | element access, bounds checks, null checks |
| `x64/loop_rewrite.rs` | 366 | the bytecode loop-rewriter planning |

## How it was verified

`jit/tests/x64_artifact_corpus.rs` — checked in, so the next lane does not have
to rebuild it. It compiles 157 fixed method shapes and hashes, per shape, the
emitted bytes **and** everything published beside them: the OSR entry table and
its assignment vectors and dead-local masks, the frame layout and the slot
offsets other components address, the oop maps, the deopt points.

Every commit ran the full jit suite and that corpus. The end-to-end check is the
one that matters: a freshly built binary at the pre-split commit `8483b78ef2`
produces **byte-identical code and identical published metadata for all 157
cases** against the finished tree. That covers the whole lane at once, not
commit by commit.

Three things about the corpus are worth carrying forward, because each cost
something to learn:

1. **Every bakeable address is a constant chosen in the test.** With real
   function pointers the emitted bytes embed the test binary's own layout and
   differ run to run under ASLR — the comparison becomes meaningless rather than
   merely noisy.
2. **`code_bytes` alone is not enough.** The OSR entry table is not in the
   instruction stream. The corpus only grew a metadata hash when a code-motion
   step needed one, and the pure-move steps were re-verified against it
   afterwards.
3. **The metadata hash was mutation-tested, and the first version failed.**
   Deleting the OSR high-half nulling changed nothing, because every loop in the
   corpus accumulated into an `int`. Four category-2 accumulator loops fixed
   that: the same mutation now moves 36 cases, flattening the dead-local mask
   refinement moves 13, an off-by-one in `osr_num_reg_locals` moves 151.
   `corpus_actually_exercises_the_published_metadata` is the floor that keeps
   the column from silently going constant again.

## What the doc got right, and what it did not

**Right, and it mattered:**

* `super::X` resolves differently one module deeper. Inside `x64`, `super::`
  names the crate root because `x64` is declared in `lib.rs`; inside
  `x64::bytecode_walk` it names `x64`. Every move rewrites it to `crate::`. This
  is the failure mode the doc's byte-comparison rule exists for: a `super::` that
  silently resolved to a *different* item of the same name would compile and emit
  different code.
* Test modules do move with their code, and one of them broke the build
  immediately — but not for the reason given. Two tests in
  `flag_and_header_contracts` do not test code at all; they scan the backend's
  own source text with `include_str!("x64.rs")`, one of them counting the
  header-offset narrowings the 32-to-16-byte `ObjectHeader` shrink has to visit.
  After a split that would have under-counted, and an under-counting inventory
  reads as "sites were deleted" to an audit whose entire job is to find them all.
  They now scan `backend_sources()`, the union of the files the compiler proper
  was split into, which turns the inventory into a tripwire for the split itself:
  every commit that adds an `x64/*.rs` must add it to that list, and forgetting
  shows up as a count shortfall.

**Not right:**

* *"A `pub(super)` type carried by a `pub(crate)` enum becomes a
  `private_interfaces` warning once it moves — that happened this wave with the
  loop-rewrite refusal enum."* It did not happen. The only payload
  `LoopRewriteRefusal` carries is `Planner(LoopXformRefusal)`, and
  `LoopXformRefusal` is `pub(crate)` in `x64/licm.rs` — the same visibility as
  the enum carrying it, so the condition the lint fires on is absent. Somebody
  widened it after the warning was seen. The hazard is real in general; it was
  not live here.
* *"A test that was implicitly relying on file-private access will fail to
  compile."* No test did. `Compiler` is private to `x64`, and a child module can
  see its parent's private items, so the only visibility work the split needed
  was promoting moved items from private to `pub(super)` when something outside
  the new file still called them — 189 of them, mechanically, with no judgement
  calls.

## The one step that was not a pure move

`osr::publish_entry_metadata`. The doc's `x64/osr.rs` row is "the OSR
publication region", and a region is a span of statements inside a function, so
moving it means giving it a signature — not `git mv`. It got its own commit, and
the corpus grew the metadata hash **before** that commit so the lift could be
checked at all.

Fourteen parameters, which is the honest count of what it read. Passing `&mut
Compiler` was not available: `compiler.buf` is partially moved into
`CompiledMethod::new` a few lines earlier, so the struct cannot be borrowed as a
whole after that point.

## Two things found on the way

* **Fixed, in its own commit.** The doc comment for `compile_with_param_slots`
  was attached to
  `gc_inert_selfrec_candidate`. Rust attaches a doc block to the item that
  follows it, so a bytecode whitelist proving a self-recursive body allocates
  nothing was documented as "compile a method to native code", and the production
  compilation entry point had no doc at all.
* **Recorded, not fixed: XMM local homes are `#[cfg(windows)]`-only.** On a SysV build `x64.rs`
  forces `xmm_assignments` to all-`None`, because there is no post-call XMM
  spill/reload. That makes the XMM half of the OSR high-half nulling, and the XMM
  contribution to the dead-local mask, statically inert on Linux — including the
  ES-tdigest `DualPivotQuicksort` fix those lines were written for. Not fixed
  here; recorded because it looks exactly like a corpus gap and is not, and the
  corpus was enlarged twice chasing it before the `cfg` explained it.

## Residual: one unwired entry point, deliberately still unwired

The problem statement below cites "one of those entry points is still unwired".
That is `x64/vec_emit.rs::emit_vector_loop`, which has no caller anywhere. It is
*not* an oversight and it is not this lane's to wire:

* Its own doc says so explicitly — "deliberately *not* wired into `x64.rs`: the
  call site is added separately" — and states a three-part contract the caller
  must honour, including patching **every** `fallback_sites` offset. Missing one
  emits a vector loop whose safety proof is incomplete.
* It is jointly owned with `isel::select_block` by
  `docs/feature-designs/jit-machine-level-and-instruction-selection.md`, whose
  measured conclusion on 2026-08-03 was to do the 32-bit pattern rows **before
  any production wiring**: shadow selection covers 15.7–19.0% of scheduled data
  nodes on real compiles, and the two rules the work was for fire zero times.

Wiring it would be new behaviour, and would need to land off by default behind a
declared flag against a measurement that currently says not yet. The other two
components the seam doc's motivation named are in better shape than it implies:
`x64/isel.rs` compiles and its 68 tests run (the missing `mod` line was added
2026-08-01), and `regalloc::allocate_linear_scan` does have a production call
site in `ir_lower.rs`.

## Note for the next lane

The suite has three environmentally flaky tests on a shared build host, and
misreading them costs a rerun each. Several off-thread compilation tests in
`tiered.rs` wait on a channel with a 5-second `recv_timeout` and fail at load
average ~20 on 16 cores; `tests::test_inline_cache_reclamation_waits_for_jit_quiescence`
asserts a code range is reclaimed at "the final quiescent transition", but
`jit_execution_enter`/`leave` is a **process-global** epoch counter, so whether
the transition is final depends on what else the parallel harness is running.
Both were confirmed against the *unmodified* tree under the same load before
being treated as noise.

---

# Original problem statement (2026-08-03, preserved)

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
