# Problem 5: two x86-64 compilers, no portable backend

Filed 2026-11-11 from code analysis only. Measured on the tree at `39e1145b06` (`dev` of 2026-11-01). Counts are regex counts over `*.rs`
and are approximate.

## Problem

The JIT is two separate compilers that both emit x86-64 bytes directly, with
their own copies of the same optimizations, plus a third bytecode translator
for aarch64 that is a prototype. There is no machine-level IR between
optimization and encoding, so nothing is shared across tiers or targets.

## Evidence

- Single-pass tier: `jit/src/x64/` is 182,528 lines, bytecode straight to
  machine code, with its own `inlining.rs` (8,715), `licm.rs` (8,710),
  `bce.rs` (6,875), `escape_analysis.rs` (5,515), `null_check_elim.rs`
  (1,269).
- Optimizing tier: `jit/src/ir.rs` (68,749), `ir_optimize.rs` (39,251),
  `ir_check_elim.rs` (8,474), `escape_analysis.rs` (8,798),
  `null_check_elim.rs` (1,582), then `ir_lower.rs`.
- `jit/src/ir_lower.rs` is **159,723 lines** and lowers the IR graph directly
  to x86-64. Its own header says: "This file is a SECOND x86-64 emitter
  alongside `x64.rs`, and it bakes object header displacements straight into
  instruction encodings", and that an inventory built from `x64.rs` alone
  missed its sites. It has about 7,500 encoder-level references.
- aarch64: `jit/src/aarch64_backend.rs` (22,039 lines) translates bytecode
  again. Its header says "This is **not** a working second-tier JIT. It is an
  arithmetic-only prototype"; a method compiles only if every bytecode is in
  its supported set, with no OSR. `target_arch` cfgs: 376 x86_64 against 59
  aarch64 in `jit`, 189 against 19 in `vm`.
- The tiers disagree by construction: `jit/src/backend_parity.rs` exists to
  compile a method with both and diff the bodies, and notes that
  `single_pass_only_lowering_for` keeps six shapes away from the optimizing
  tier.
- Glue: `jit/src/lib.rs` is 98,613 lines with 208 statics;
  `vm/src/runtime/interpreter/jit_bridge.rs` is 65,438 lines.
- 2,809 JIT switches (problem 1) select among these paths.

## Why it is architectural

- Every object-layout, barrier, safepoint or calling-convention change must
  be made in three emitters. The collectors change barriers often.
- Every optimization is written twice and its bugs are fixed twice.
- aarch64 cannot get the optimizing tier without rewriting 160k lines.
- Frame layout differs per emitter, which is one cause of problem 3.

## Plan

1. **Introduce a machine-level IR (LIR)** under the optimizing tier: virtual
   registers, explicit memory operands, calls with a declared clobber and
   reference signature, safepoint records. Small and boring; the model is the
   existing `regalloc.rs` `ValueLoc` / `CopyOp` vocabulary.
2. **Split `ir_lower.rs` into select and encode.** Instruction selection
   produces LIR; a per-target encoder turns LIR into bytes. Do it by node
   family (arithmetic, memory, calls, allocation, barriers), keeping the old
   path behind the acceptance gate until the family is byte-compared with
   `backend_parity`.
3. **One place for layout and barriers.** Field offsets, header
   displacements and each collector's barrier become LIR-building helpers
   used by both tiers, so a layout change is one edit.
4. **Make the single-pass tier dumb.** Once the optimizing tier compiles fast
   enough at a low opt level (measure first), delete the single-pass tier's
   inlining, LICM, BCE and escape analysis. Its job is profile collection and
   quick code. If the low-opt IR path is fast enough, delete the tier.
5. **aarch64 through the encoder.** Write the aarch64 LIR encoder and frame
   layout; retire `aarch64_backend.rs`. The instruction encoder in
   `jit-aarch64` is kept.
6. **One frame and safepoint format** for all compiled code, which is what
   problem 3's contract needs.

## Done when

- One optimizer, one frame format, two encoders.
- `ir_lower.rs` contains no instruction bytes.
- The aarch64 build runs the same regression suite with the JIT on.
