# jit-cuda review

## Summary

- **MED — Loop-recognizer's `iv_slot` resolution is positional, not data-flow.** `classify_counted_loop` (`src/lowering/loop_recog.rs:200,255-262`) picks "the second-to-last `iload` before the exit-if" as the induction variable. javac always emits `iload iv; iload bound; if_icmp*` for the canonical loop, but a method whose header contains *any* additional `iload` (e.g. `iload other; iadd; iload iv; iload bound; if_icmpge`) would have `iv_slot` resolved to the wrong local, and the subsequent `verify_zero_start` / `find_iv_stride` would still succeed if that "false iv" happens to satisfy them — silently mis-lowering loops. Today this is masked by `analyzer::analyze` accepting only forbidden-opcode-free bodies and javac's stable codegen, but no fixture or assertion pins it.
- **MED — Signed branch-target arithmetic wraps to a huge `usize` on negative `pc+off`.** `classify_counted_loop` (`src/lowering/loop_recog.rs:221`) computes `let target = (pc as i32 + off) as usize;` with no bounds check. A backward `if_icmp*` inside the loop body (legal in nested-conditional code) wraps to a value `> back_branch_pc` and the loop is then mis-classified — the body's backward `if*` becomes "the exit-if". Same `as usize` pattern in `emit.rs:687,695` is safe because the result only feeds an equality check against `header_pc`, but the loop_recog site is consumed as an ordering predicate.
- **MED — Reduction recognition is shape-only, not semantics-checked.** `analyzer.rs:336-434` admits any "counted loop + array load + arithmetic `*add` + scalar return" as a dot-product reduction and the emitter at `emit.rs:1464-1521` then issues `atom.global.add.<suffix>` against `ret_ptr`. A method that uses `*add` for an unrelated purpose (e.g. accumulating something into a *local* that is then dropped, while the actual scalar return comes from an unrelated `iload`) would still match the shape — and the atomic-add would race-accumulate the wrong values into `ret_ptr`. No data-flow proof links the accumulator local to the return.
- **LOW — `verify_zero_start` is a peephole, not a true reaching-definition.** `loop_recog.rs:373-451` only tracks `last_const` across the single previous instruction. A prelude like `iconst_0; nop; istore iv` is harmless, but `iconst_0; istore other; istore iv` reads `last_const = None` and correctly rejects; `bipush 0; istore iv` accepts. Edge cases (`iconst_0; dup; istore iv`) silently reject when they could accept — correctness-preserving but documented gap.
- **OSS verdict: NEEDS LIGHT FIXES.** Code is clean, well-documented, no `unsafe`, no FFI, no `cudarc`/`libcuda` (scope discipline enforced — see `README.md:18-22`). 44 unit tests, 2 gated `#[ignore]`-marked GPU-required tests. SPDX headers consistent. `publish = false` inherited from workspace. Per-crate `LICENSE`/`NOTICE` symlinks missing for crates.io upload (same situation as the rest of the workspace). NVIDIA-trademark exposure is minimal — the crate emits PTX text but never says "NVIDIA"; `README.md:18-20` actively disclaims `cudarc`/`libcuda`.

## 1. Code review

### Bugs

- **MED — `loop_recog.rs:255-262`: positional `iv_slot` resolution.** The recognizer trusts that the header is exactly `iload iv; iload bound; if_icmp*` and picks `iload_history[len-2]` as `iv_slot`. No assertion that `iload_history.len() == 2`, no check that the "iv" candidate is actually the LHS operand of the comparison. A prelude that pushes more iloads into the header (e.g. via `iinc`-after-iload or any non-canonical javac codegen quirk) silently picks the wrong local. Combined with `verify_zero_start` only matching the *most recent* `istore iv_slot` (line 423), a method that initialises the wrong local to 0 and the real iv to non-zero could still get past validation. Recommended: walk back from `exit_if_pc` two instructions and assert that `bytes[exit_if_pc - size_of_iload]` is the bound-producer and `bytes[exit_if_pc - 2*size_of_iload]` is `iload iv`, with an explicit canonical-shape check.
- **MED — `loop_recog.rs:218-225`: signed branch-target wrap inside the loop walk.** `(pc as i32 + off) as usize` on line 221: if `off` is negative (backward `if*` inside the loop body — e.g. a `do-while` nested inside the outer `for`), `target` wraps to a huge `usize`, satisfies `target > back_branch_pc`, and the body's `if*` becomes the recognised exit-if. Today this is partially masked because `collect_backward_branches` (lines 121-167) requires *exactly one* backward branch, but a forward `if_icmplt body_start` (loop continuation in `do-while`) followed by a `goto back` still single-counts as one backward branch. Guard with `if off >= 0 && (target as usize) > back_branch_pc`.
- **MED — `analyzer.rs:351-362` / `emit.rs:1464-1521`: reduction shape detection is purely syntactic.** The analyzer flips `is_dot_reduction = body_has_array_load && body_has_add && has_backward` and the emitter then emits `atom.global.add` instead of `st.global` for any scalar return. There is no data-flow proof that the value being returned IS the accumulated value. A method shaped like `for (i...) { acc += a[i]; } return a[0];` would be admitted (counted loop, array load, add, scalar return) and lowered to atomically-add the per-thread-`tid` element across all threads into `ret_ptr` — silently wrong. Fixture coverage is insufficient to rule this out; `dot_product_lowers_with_long_math` only confirms the *happy* path.
- **LOW — `analyzer.rs:202-204`: empty exception-table check is correct but unused below.** `code.exception_table.is_empty()` rejects with `HasExceptionHandlers`, but the corresponding test fixture/case is missing — the only place `HasExceptionHandlers` is enum-mentioned is in the variant declaration (line 103). Add a fixture method with a real try-catch.
- **LOW — `emit.rs:280` `mov.b32` reinterpret-cast u32→s32.** Bit-pattern-correct, but `mov.b32` between differently-typed registers is non-standard PTX style; `cvt.s32.u32` or `setp` round-trip would be clearer. Today's ptxas accepts both; pinned by the manual `ptxas_round_trip_vector_add` test (gated on CUDA).
- **LOW — `emit.rs:1571-1574` `bytes[bound_pc + 1..bound_pc + 3]` no per-offset guard.** Safe by construction (the surrounding walker validates `pc + size <= bytes.len()` at line 1551), but the +2/+3 reads inside the `0xC4 if bytes[bound_pc + 1] == 0x15` arm assume the wide prefix is followed by a complete 4-byte instruction — which `instr_size` enforces. No bug, but the pattern is fragile if `instr_size` ever loosens.

### Vulnerabilities

- **No untrusted-PTX path.** This crate produces PTX text only. There is no CUBIN ingestion, no JIT linker call, no `cuModuleLoad` — that all lives in `cuda-bridge`. Inputs are JVM bytecode that has already passed the analyzer's strict opcode filter (`analyzer.rs:451-503`).
- **No CUDA FFI.** Zero `unsafe`, zero `extern "C"`, zero `libcuda`/`cudarc` references (grep-verified; `lib.rs:13-15` explicitly disclaims). Lifetime issues for device pointers / streams / modules belong to `cuda-bridge`.
- **Integer overflow in launch dims / shared-mem.** Annotation parsing clamps `block_x/y/z` and `shared_bytes` to non-negative i32 then casts to u32 (`annotations.rs:303-327`) — no overflow vector. `KernelSignature.estimated_work` is `usize` with `1 << 20` constant or `bytes.len()` (`analyzer.rs:422`); also safe.
- **Out-of-range bytecode targets** caught by `collect_backward_branches` (`loop_recog.rs:138,156`) with `< 0 || >= bytes.len()`.

### Stubs / todo / unimplemented

- **None.** Grep for `todo!`, `unimplemented!`, `FIXME`, `XXX`, `HACK`: zero hits in source. Two `PHASE1-GUESS` documentation breadcrumbs at `analyzer.rs:469` (CP-index resolution deferred to lowering) and `analyzer.rs:806` (test fixture not yet built); both annotate accepted design gaps, not stubs.

### Performance

- **`emit.rs:1184-1225` (`array_load`):** Every `*aload` emits its own bounds check + index→byte-offset conversion + base+offset → addr. Adjacent loads of the same array (e.g. `c[i] = a[i] + b[i]`) re-emit `ld.param.s32` for the length each time (`emit.rs:359`). No CSE; the PTX optimizer will likely fold the redundant `ld.param.s32 [pN_len]` but the IR-level redundancy adds 3-4 lines per access. Acceptable for first-pass JIT.
- **`emit.rs:262-282` (`emit_tid`):** Emits five mov/mad instructions even for straight-line kernels where `tid` is unused. The straight-line path at `lowering.rs:62-72` calls `emit_tid()` unconditionally. Minor codegen waste; ptxas will DCE the unused result.
- **`emit.rs:354-389` (`emit_bounds_check`):** Materialises a fresh zero constant per check (`mov.s32 zero, 0`). Hoisting once at kernel entry would save N×3 bytes of PTX text per kernel.
- **`lowering.rs:227-243` (`mangle`):** O(n) char walk per symbol; one allocation per character via `for ch in ...chain(...)`. Fine for a per-method one-shot, but the chain iterators force a virtual call. `String::push` + `replace_with`-style would be tighter.
- **`analyzer.rs:354-435` walks bytecode three logical things in one pass (good).** The previous separate `estimate_work` / `body_match` / classifier walks were merged — comment at lines 244-251 confirms.

## 2. Tests

- **Test count:** 44 `#[test]` (16 `analyzer.rs`, 20 `lowering.rs`, 5 `annotations.rs`, 3 `emitter.rs`). Two marked `#[ignore]` (PTX dump + `ptxas` round-trip; both require host tooling).
- **Coverage estimate:** ~80%. Analyzer paths: every `Reason::*` variant has at least one acceptance/rejection fixture except `HasExceptionHandlers` (no try-catch fixture in `test_classes/gpu/`) and `JsrRet` (deprecated bytecode, no realistic javac path). Lowering paths: counted-loop happy path (`vector_add`, `saxpy`, `dot`), non-canonical loop rejections (`leLoop`, `neLoop`, `stride2Loop`, `start5Loop`), straight-line, two-loops rejection, `frem`/`drem` rejection, reduction atomic-add emission. Emitter paths: render-only smoke tests for empty module, trivial kernel, params+regs.

### Gaps

- **No fixture for `HasExceptionHandlers`.** `analyzer.rs:203-204` rejects on a non-empty exception table; no Java fixture under `test_classes/gpu/` triggers it. Add `RejectExceptionHandler.java` with a real `try { ... } catch (Exception e) {}` body.
- **No fixture for `LoadConstant`.** `analyzer.rs:463` rejects `ldc`/`ldc_w`/`ldc2_w` (C31 audit). No test pins this.
- **No fixture for `UnknownOpcode`.** Only synthesisable from a malformed `.class`; the "no synthetic bytecode" rule in `test_support.rs:7-12` rules that out. Either relax the rule for negative bytecode-error tests or accept the gap.
- **No `BadDescriptor` fixture.** Same situation — would need a hand-crafted bad descriptor string in a real class file.
- **Reduction-shape false-positive test.** Construct a Java method whose body satisfies "counted loop + array load + arithmetic add + scalar return" but where the returned scalar is NOT the accumulator (e.g. `int acc = 0; for (...) { acc += a[i]; } return a[0];`). Today the analyzer admits it and the emitter atomically-adds the per-thread `a[tid]` into `ret_ptr` — wrong. This test would catch the data-flow gap noted above and force a fix.
- **No `i2c` / signed-vs-unsigned conv test for the truncate path.** `emit.rs:1140-1152` (`conv_truncate_i32`) is used for `i2b`/`i2c`/`i2s` but no fixture exercises a Java method that depends on the sign-extend semantics (`i2c` is unsigned; the current code emits `shr.s32` for char). `EligibleI2cConvert.java` exists in the fixtures dir but isn't referenced from any `#[test]`.
- **No fuzz.** The crate has no fuzz harness — no entry in `../fuzz/Cargo.toml` for `jit-cuda`. The analyzer's bytecode walker (`scan_bytecode` + `instruction_size`) is a natural fuzz target: feed random bytes, assert no panic / no UB. Same for the lowering walker.
- **No proptest.** Annotation parsing (`annotations.rs`) and the canonical-loop verifier (`loop_recog.rs`) both have shape predicates that would benefit from generative testing.
- **`ptxas` round-trip test only covers `vector_add`.** `lowering.rs:482-501` is the sole `ptxas` test; `saxpy`, `dot`, `f32 i2b`, etc. are not round-tripped. Even gated on `#[ignore]`, adding them costs nothing if the host has CUDA.
- **No oracle / golden test for PTX text.** Every lowering test asserts on substring matches (`text.contains("ld.global.s32")`). A regression that re-orders instructions or doubles up a `cvt` would pass. Consider a `expect_test`-style snapshot for at least one canonical kernel.
- **Brittleness:** Substring assertions like `text.contains("ld.global.s32")` would survive a kernel that emitted the right opcodes in the wrong order, or for the wrong arrays. The `vector_add_kernel_has_correct_param_list_in_ptx` (`lowering.rs:504`) does pin parameter names, which is the right pattern — extend it.

### Concrete additions

1. `test_classes/gpu/RejectExceptionHandler.java` — `try-catch` body to pin `HasExceptionHandlers` rejection.
2. `test_classes/gpu/RejectLdcString.java` — `return "x".length();` to pin the C31 `LoadConstant` rejection.
3. `test_classes/gpu/ReductionFalsePositive.java` — `int acc = 0; for (int i = 0; i < a.length; i++) acc += a[i]; return a[0];` to catch the data-flow gap in shape detection.
4. Wire `EligibleI2cConvert` into `lowering.rs` tests — confirm `cvt.u32.u16` is emitted (not `cvt.s32.s16`) on the char path.
5. Add a `fuzz/fuzz_targets/jit_cuda_analyze.rs` that calls `analyze` on a `ClassFileMethod` materialised from random bytes and asserts no panic. (`reader` already has a fuzz harness; reuse the corpus.)
6. Add a proptest for `annotations::read_method_annotations` over randomly-constructed `Annotation` lists with mixed valid/invalid `type_index` UTF-8 lookups.
7. Add an expect-test snapshot of `vector_add`'s rendered PTX so a future opcode reorder is flagged.

## 3. Documentation

### Existing

- **Crate-level rustdoc** (`lib.rs:4-17`): scope, non-goals, fixture discipline. Good.
- **`README.md`**: scope, non-goals, usage example, status, license. Mirrors `lib.rs`.
- **Module-level rustdoc** in every file (`analyzer.rs:4-21`, `annotations.rs:4-25`, `emitter.rs:4-16`, `lowering.rs:4-23`, `lowering/emit.rs:4-19`, `lowering/loop_recog.rs:4-55`, `signature.rs:4-6`, `test_support.rs:4-12`).
- **Inline AUDIT comments** with dates (`2026-05-16`, `2026-05-17`, `2026-05-19`, `2026-05-20`, `2026-05-22`, `2026-05-24` (C31)). Each documents the bug, the fix, and the contract. Excellent forensic trail.
- **Public-API rustdoc:** `ParamKind`, `Reason`, `OffloadVerdict`, `analyze`, `analyze_with_annotations`, `KernelSignature`, `PtxModule`, `PtxKernel`, `PtxParam`, `PtxParamKind`, `RegDecl`, `RegKind`, `LoweringError`, `lower_method`, `build_param_list`, `MethodAnnotations`, `ClassAnnotations`, `GpuKernelAttrs`, `GpuExcludeAttrs`, `EnableAsyncAttrs`, `AdmissionHint`, `GridShape` — all rustdoc'd.
- **`build.rs` doc-comment** (lines 1-26) explains the cross-crate `links`-mechanism dance with `craton-gpu` and why `cargo:rustc-env=` doesn't propagate.

### Missing

- **No capability matrix.** Phase-1 spec at `docs/gpu/phase1-spec.md` lists supported intrinsics (`Math.sqrt`/`sin`/`cos`/`exp`/`log`) but the crate-level rustdoc doesn't enumerate which Java constructs are supported. Add a table: "Java construct → PTX lowering → status (full / partial / rejected)".
- **No supported-intrinsics list.** `AdmissionHint::AllowIntrinsicCalls` documents the intent (`annotations.rs:64-66`) but neither the analyzer nor the lowering layer enumerates which CP-resolved targets actually lower. (Today: none — the lowering layer rejects every `invokestatic`. The analyzer's loosening is a no-op in practice. This should be called out in the rustdoc.)
- **No PTX-version compatibility doc.** `emitter.rs:50` hardcodes `.version 7.5` and `.address_size 64`; `lower_method` takes `sm_major`/`sm_minor` parameters but the crate doesn't document the range of (sm_major, sm_minor) it has been validated against. `dot` requires sm_60+ for `atom.add.f64`; the crate accepts any sm_* without checking.
- **No consistency check with `docs/gpu/`.** The `docs/gpu/` workspace docs (`phase1-spec.md`, `annotations.md`, `phase2-spec.md`, etc.) are the design source; `jit-cuda` rustdoc cites §2.4 of the Phase-1 spec by number but doesn't link to the actual file. Add `[`#references`]` rustdoc blocks pointing at the workspace docs.
- **Phase 10 #2 contract on the host side is hinted but not linked.** `KernelSignature::writes_param_mask` rustdoc says "see the marshaller in `vm::runtime::offload`" — but the consumer crate isn't named and there's no link.
- **Reduction host-marshaller contract.** `signature.rs:73-86` and `emit.rs:1473-1499` both say the host MUST pre-zero `*ret_ptr` before launch for the atomic accumulation to land on the right identity. This contract lives only in two rustdoc blocks; it should be a top-level doc bullet in `lib.rs` or `README.md` because it crosses crates.
- **Per-crate `LICENSE` / `NOTICE` files.** Workspace-root `LICENSE` and `NOTICE` exist; `jit-cuda/` does not symlink or copy them. crates.io won't accept the publish without per-crate copies, but workspace has `publish = false`, so non-blocking today.

## 4. OSS readiness

### Cargo.toml

- **`publish = false`** inherited via `version.workspace = true` from `[workspace.package]`. Explicitly required by workspace policy.
- **`license = "Apache-2.0"`** inherited from workspace — SPDX-correct.
- **`description`** set to "Java bytecode → PTX lowering for CratonVM GPU offload". Suitable for crates.io.
- **`readme = "README.md"`** present.
- **`repository` / `keywords` / `categories`** inherited from workspace.
- **`[features]`**: `gpu-it` for GPU-only tests, default empty. Clean.
- **No transitive dep concerns.** Only deps are workspace-internal (`cratonvm-reader`, `cratonvm-types`, `cratonvm-jit-api`, `craton-gpu`) and `thiserror`. No `cudarc`, no `libcuda`. Scope discipline holds.
- **Build dep:** none declared; `build.rs` invokes `javac` via `std::process::Command` (`Cargo.toml:36-38` documents). Build silently skips on missing `javac` — comment at `build.rs:55-61` confirms.

### Headers / SPDX

- All 7 `.rs` files in `src/` and `src/lowering/` begin with `// SPDX-License-Identifier: Apache-2.0` and `// Copyright 2024-2026 Craton Software Company`. Consistent.
- `build.rs` has no SPDX header. **Add one** — minor consistency fix.
- `Cargo.toml` and `README.md` have no SPDX comments (conventional — Apache-2.0 in the `license = ` field is sufficient).

### NVIDIA-trademark exposure

- **Minimal.** The crate emits PTX (NVIDIA's IR) but never says "NVIDIA" in source. `cuda-bridge`'s README (out of scope here) holds the trademark attribution.
- **`ptxas`** is mentioned in test code (`lowering.rs:482-501`) as a host-binary lookup — uses `std::env::var("PTXAS").unwrap_or_else(|_| "ptxas".to_string())`. No trademark assertion.
- **`README.md:18`** disclaims `cudarc`/`libcuda`, which keeps the crate's own surface clean.
- **No `NVIDIA®` / `CUDA®` glyphs anywhere.**
- **Recommendation:** add a one-line NOTICE entry at workspace root: "This crate emits PTX text for NVIDIA® CUDA® GPUs. CUDA, PTX, and NVIDIA are trademarks of NVIDIA Corporation. CratonVM is not affiliated with or endorsed by NVIDIA Corporation."

### Blockers

- **None for internal/workspace use.** Builds, runs, tests pass per the AUDIT trail in source.
- **For crates.io publish:** workspace-wide `publish = false`; per-crate `LICENSE` / `NOTICE` files missing (symlink workspace ones or copy); `Cargo.toml` missing `homepage` (inherited blank from workspace — actually workspace sets it, fine).
- **Phase-1 spec reference loose end:** `docs/gpu/phase1-spec.md` cites `§2.4` from the crate's rustdoc (`analyzer.rs:181-184`), but no link.

## Top 5 fix priorities

1. **Add a Java fixture that satisfies the dot-product *shape* but whose return value is NOT the accumulator, and wire it into the test.** Confirms whether `analyzer.rs:351-362` + `emit.rs:1473-1521` silently mis-lower reductions. If the test fails (as suspected), gate `is_reduction` on a real data-flow link from accumulator local to scalar return.
2. **Guard the loop-recognizer's signed branch-target wrap at `loop_recog.rs:221`.** Add `if off >= 0 && (target as usize) > back_branch_pc`. Add a fixture with a backward `if*` inside the loop body (a nested `do-while`) to pin the rejection.
3. **Strengthen `iv_slot` resolution to a real LHS-of-comparison check, not "second-to-last `iload`".** `loop_recog.rs:255-262`. Add a fixture whose loop header contains an extra `iload` (e.g. `iload other; iadd; iload iv; iload bound; if_icmpge`) and confirm the recognizer rejects or correctly identifies `iv`.
4. **Add a fuzz target for `analyze` over random bytecode.** Catches `instruction_size` underflows, `iload_history` pathologies, integer-overflow in branch decode. Wire into the existing `fuzz/` crate.
5. **Add `RejectExceptionHandler.java` + `RejectLdcString.java` fixtures.** Closes the two `Reason::*` variants that have no test today (`HasExceptionHandlers`, `LoadConstant`). Both are one-line Java sources, build via the existing `build.rs` path.
