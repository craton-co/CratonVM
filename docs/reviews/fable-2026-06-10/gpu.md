# CratonVM GPU Stack — Code Review (Fable, 2026-06-10)

Scope: `jit-cuda/src` (9 files, ~4.8k LOC), `cuda-bridge/src` (6 files) + `cuda-bridge/tests`, and `craton-gpu` (`cratonvm-gpu`, 15-LOC lib + 262-LOC build.rs). Static review only — no builds were run.

## Summary

This is the most heavily-audited subtree I have reviewed. Both crates carry a dense, dated audit trail (CRIT-1 device-ptr plumbing, UAF keep-alive handles, H10a/b/c host-buffer and per-buffer-event ordering, SOUND-1 `bind_to_thread` contract, interner DoS cap, alloc-size overflow guards). The high-risk attack surface — `jit-cuda`'s bytecode→PTX lowering that walks attacker-controlled classfile bytecode — is defensively coded: every raw operand read in `emit.rs` is gated by a `pc + size <= bytes.len()` check before dispatch, every uncertain shape rejects to a CPU fallback (correctness over coverage), and integer divide / array bounds / loop-shape canonicality are all validated.

I found **no memory-safety vulnerability and no reachable panic from untrusted bytecode**. The findings below are correctness nits, a couple of defensive-hardening items, a meaningful pile of stub/no-op surfaces (mostly intentional and well-documented — the stub backend and the unfinished lowering shapes), perf micro-opportunities, and a packaging question about the near-empty `cratonvm-gpu` crate. Test coverage is genuinely good for a JIT/codegen module (real `.class` fixtures, no synthetic bytecode) — estimated ~70%, short of 85% mainly because the entire `cuda` backend path and all PTX *semantic* correctness (vs PTX *shape*) are untested without a GPU.

The cuda-bridge unsafe FFI is sound *conditional on the documented `bind_to_thread` caller contract*, which the bridge itself does not enforce — that is the one systemic soundness caveat worth flagging for the open-sourcing audit.

---

## Bugs (correctness)

### B1 — `caload`/`castore` mis-handle `char` as signed on the GPU edges (low/medium)
`jit-cuda/src/lowering/emit.rs`. `array_load_16(is_char=true)` correctly emits `ld.global.u16` + `cvt.u32.u16` (zero-extend) for `caload` (line 1326-1334), and `i2c` routes through `conv_zext_u16` (line 1172). Good. But `array_store_char_or_short` (line 1428) handles **both** `castore` (0x55) and `sastore` (0x56) identically with `st.global.s16` (line 1461-1465). For a `char` store the value width is fine (16-bit truncation is identical for signed/unsigned store), so this is benign in isolation. The real asymmetry risk is conceptual: `castore`/`sastore` share one helper and the bounds check uses the same path — verify on a GPU host that a round-trip `char[]` map preserves values ≥ 0x8000. Likely correct, flagged for the ptxas/oracle pass since it cannot be checked statically.

### B2 — Loop work-estimate literal-bound recovery can mis-attribute a bound (low)
`jit-cuda/src/analyzer.rs:399-422`. `last_const` tracks the most-recent literal push and `literal_bound` is captured at the first forward `if_icmp*`. The estimate is advisory (used only to decide round-trip worthwhileness), and a wrong value cannot cause incorrect execution — but `last_const` is not reset by intervening non-const, non-branch ops that consume the stack (e.g. an `iadd` between the push and the comparison), so a contrived body can capture a stale literal as the trip count. Impact is bounded to a sub-optimal launch/skip decision; no correctness or safety effect. Worth a comment or a tightening to "previous op was the push."

### B3 — `non-reduction scalar return` correctness depends entirely on the analyzer gate (low, latent)
`jit-cuda/src/lowering/emit.rs:1528-1538`. The non-reduction `scalar_return` emits a plain `st.global` to `ret_ptr` and the comment asserts "every thread writes the same value … racing is benign." That invariant only holds for the `StraightLine` shape; for a counted loop it would be a racy wrong result. It is *currently* safe because `analyze` rejects array-in/scalar-out (`ReductionNotImplemented`) and scalar-in/scalar-out counted loops (`CountedLoopScalarReturn`) unless recognized as a dot-product reduction (which takes the atomic path). This is a two-layer coupling with no defense-in-depth in the emitter: if a future analyzer relaxation admits a non-reduction counted-loop scalar return, this silently corrupts. Recommend an emitter-side assertion that `is_reduction || shape == StraightLine` before the plain store (mirrors the `debug_assert!`s already in `emit_loop_guard`).

---

## Vulnerabilities

None found. Notes on the surfaces examined:

### V-note 1 — Unsafe FFI soundness is conditional on an unenforced caller contract (informational, systemic)
`cuda-bridge/src/{lib.rs,backend_cuda.rs,stream.rs,event.rs}`. Every `unsafe impl Send/Sync` (on `DeviceContext`, `DeviceBuffer`, `Stream`, `EventCuda`, `StreamBarriers`) is justified by the CUDA "any thread with the primary context bound" rule, and each cross-thread-callable method now binds via `bind_to_thread()` (the H10c/SOUND-1 fix). The residual hazard is that `Event::synchronize/query` and `Stream::record_event/wait_event` *do* bind, but the contract is "the bridge does not enforce binding on raw paths" (documented in the `# Safety` blocks). For an Apache-2.0 release this is the single item a downstream safety auditor will flag; consider a debug-build TLS sentinel that asserts the context is bound. Only relevant under `--features cuda` (default builds compile only the inert stub).

### V-note 2 — Build-script `javac`/`jar` shell-out uses only trusted inputs (informational)
`craton-gpu/build.rs` and `jit-cuda/build.rs` invoke `javac`/`jar` via `Command` with args from a filesystem walk of a developer-controlled source root and the `CRATON_GPU_JAVA_SRC` env var. No shell interpolation (`Command` arg vector, not a shell string), no untrusted/network input. Not a runtime attack surface. The `..`-relative `resolve_java_root` default is build-host-only.

### V-note 3 — Alloc-size overflow is guarded; ZSTs rejected (positive)
`cuda-bridge/src/backend_cuda.rs:800` `check_alloc_size` (`checked_mul` of `len * size_of::<T>`) covers both `uninit` and `zeros`; `ASSERT_DEVICE_REPR` rejects zero-sized `T`. The kernel-name interner is bounded at `MAX_INTERNED_KERNEL_NAMES = 4096` with a clean `KernelNotFound` past the cap (the documented unbounded-leak DoS fix). These are the right defenses for untrusted `len`/name inputs.

---

## Stubs and Unimplemented

Most are intentional and well-documented; the project policy targets *synthetic stubs that fake app behavior*, and none here fake results — they return `NoDriver`, reject to CPU fallback, or are reserved-but-unused enum variants. Listed for completeness.

1. **`cuda-bridge/src/backend_stub.rs` (whole file)** — every fallible entry returns `DeviceError::NoDriver`; `from_ptx` returns `Ok(inert)` and `launch_raw` returns `NoDriver`. Intentional default-build no-driver backend. `device_ptr_arg` returns `0`, `len` returns `0`. Not faking behavior (errors are surfaced), so policy-compliant.
2. **`craton-gpu/src/lib.rs`** — 15 lines, two `const` strings from `env!`. The Rust API is **dead at runtime** (only referenced in a commented-out test block in `vm/tests/gpu_async_stub.rs`). See Feature F6 re: whether to publish it.
3. **`jit-cuda` lowering rejections that are really "not implemented yet"** (each correctly falls back to CPU, never fakes a result):
   - `frem`/`drem` (emit.rs:620-621) — no PTX `rem.f32/f64`; rejected. IEEE-remainder lowering is future work.
   - `lcmp`/`fcmp*/dcmp*` (emit.rs:674) — rejected; no element-wise use.
   - `dup2_x1`/`dup2_x2` (emit.rs:585) — rejected as "uncommon."
   - `getfield`/non-static receiver-access (analyzer.rs:454) — rejected; emitter has no `getfield` arm and never binds `this`. Genuinely unimplemented kernel shape.
   - `ldc`/`ldc_w`/`ldc2_w` (analyzer.rs:537) — rejected; no numeric-`ldc` arm in the emitter.
   - Reduction shapes other than dot/sum (analyzer.rs `ReductionNotImplemented`) — rejected; only sum/dot atomic-add is lowered.
   - Non-canonical loops (`<=`,`!=`, non-unit stride, non-zero start, multi-loop) — rejected in `loop_recog.rs`.
4. **`AdmissionHint::AllowDivByZero`** (annotations.rs:62, analyzer.rs:568) — parsed and threaded through but a **no-op**: the analyzer "never injects a zero-divisor guard" so the hint does nothing today. Documented, but it is a knob that silently has no effect.
5. **`AdmissionHint::AllowIntrinsicCalls`** (analyzer.rs:550) — admits *any* `invokestatic` (not just `Math.{sqrt,sin,cos,exp,log}`) because the analyzer has no constant-pool handle; the doc calls this `PHASE1-GUESS` and relies on the lowering layer to refuse unknown callees. The lowering layer rejects all invokes, so an `AllowIntrinsicCalls`-annotated method with a real intrinsic call would be analyzer-eligible but **always** lowering-rejected — the intrinsic path is effectively unimplemented end-to-end.
6. **`GridShape::RowPerThread` / `BlockReduction`** (annotations.rs:43-47) — recognized at parse time, never routed by the analyzer/launcher. Reserved.
7. **`KernelSignature.this_field_cps`** (signature.rs:46, analyzer.rs:359) — always empty (non-static receiver methods are rejected outright); retained for ABI stability. Dead field.
8. **`DeviceContextInner::copy_h2d_raw` / `compute_raw`** (backend_cuda.rs:230,244) — `#[allow(dead_code)]` accessors retained but unused after the H10a/HIGH-2 fixes.
9. **`optimal_block_size` autotune** (backend_cuda.rs:346) — real, but the dynamic-smem callback always returns 0 and `ctx` is ignored; partial.

---

## Performance

The bridge already pools per-launch arg buffers (thread-local `PTR_SCRATCH`/`ARG_SCRATCH`), interns kernel names, and uses three streams for H2D/compute/D2H overlap. Remaining micro-opportunities, all cold/low-impact (lowering runs once per method per JIT pass):

1. **`build_param_list` still clones one `String` per param** (lowering.rs:147-159) — the reused-buffer trick saves the `format!` but `buf.clone()` is unavoidable given `PtxParam` owns its name. Minor; could intern common names (`ret_ptr`, `failure_flag`, `pN_len`).
2. **`Emitter::new` eagerly builds `param_len_name` via `format!("p{i}_len")` for every param** (emit.rs:198-200) even for scalar params that never have a `_len`. Cheap but wasted for scalar-heavy signatures.
3. **`array_param_of` is an O(params) linear scan per array op** (emit.rs:1195-1202) — for a kernel with many array params and many array accesses this is O(accesses × params). In practice params ≤ a handful; not worth a map unless a pathological kernel appears.
4. **`mangle` builds the kernel name char-by-char with per-char `push`** (lowering.rs:227-242) — fine; one-shot.
5. **`PtxModule::render` / `PtxKernel::render` use `format!` inside push loops** (emitter.rs:51,77,81) — many small allocations per render. Cold path (once per compiled kernel); `write!` into the existing buffer would avoid the temporaries.

No hot-loop allocation or lock contention found on the launch path (the pooling work already addressed it).

---

## Tests

Estimated coverage: **~70%**. Does **not** plausibly reach 85%.

What is covered (well):
- **Analyzer**: real-`.class` fixtures for eligible (vectorAdd, saxpy, dot), and every reject reason (allocation, invoke, synchronized, ref-array, switch, throw, field-access, type-check, non-static receiver), plus annotation-loosening (AllowAllocation, AllowIntrinsicCalls) and default-unchanged. Strong.
- **Loop recognition / lowering**: canonical loop lowers; `<=`/`!=`/stride-2/nonzero-start/two-loop/frem/drem all rejected with asserted messages; dot-product reduction emits `atom.global.add.u64` and *not* a racing plain store. White-box stride/exit-op checks. Strong for *PTX shape*.
- **cuda-bridge stub mode**: stream id uniqueness, op-log record/clone-not-drain, event record/wait id matching, `launch_on_stream` event choreography (kernel-waits-on-upload, D2H-waits-on-kernel), three-stage pipeline structural ordering, `last_write` slot sharing, alloc geometry. Strong for the stub op-log contract.
- **Annotations reader**: default admit, allow-allocation, exclude reason, enable-async warmup, unknown-ignored.

Gaps (why 85% is not reached):
- **The entire `cuda` backend (`backend_cuda.rs`, ~1100 LOC) has zero executed tests** — all stream/event/memcpy/launch FFI is `#[ignore]` or stub-gated. CI is documented to run `cargo check --features cuda` for compile-honesty only.
- **No PTX *semantic* verification** — `ptxas` round-trip and numeric-oracle tests are `#[ignore]` (require CUDA toolkit). The lowering tests assert PTX *substrings*, not that the kernel computes the right answer. A wrong `cvt`/offset/atomic-suffix would pass the string asserts.
- **No truncated/adversarial-bytecode tests** — the careful `pc + size > len` guards in `walk`/`locate_bound`/`pre_loop_bound_source`/`collect_backward_branches` have no fuzz or hand-crafted-truncation unit test proving they reject (vs panic). This is the highest-value missing test for the open-sourcing security posture.
- **No `i64`/`f64` param-slot-width** lowering test (long/double consume 2 local slots; `bind_param_locals` increments `slot += 2`) against a real fixture.
- **`array_store_char_or_short` / `caload`/`saload`** value-range round-trip untested (B1).

Most important missing tests, in priority order:
1. Adversarial/truncated classfile bytecode fed to `analyze` + `lower_method` → must `Err`, never panic (fuzz `scan_bytecode`, `detect_loop`, `walk`).
2. `ptxas` assembly of every lowered fixture (gate on toolkit) — wire into the `gpu-it` feature so a GPU CI lane runs it.
3. Numeric oracle: run a lowered kernel on a GPU and compare against the CPU interpreter for vectorAdd/saxpy/dot.
4. long/double parameter and `caload` value-preservation lowering tests.

Basis for the estimate: analyzer + lowering + annotations + stub-bridge logic is ~exhaustively unit-tested by reading; the unexecuted cuda backend and absent semantic/adversarial tests are large untested risk surfaces, which caps the realistic figure well below 85%.

---

## Feature Suggestions

1. **Adversarial-bytecode fuzz target** (fuzz/ already exists in the workspace) — fuzz `analyze` and `lower_method` against arbitrary byte slices to prove "reject, never panic." Highest security value for the release.
2. **Debug-build `bind_to_thread` sentinel** — a TLS flag the bridge sets on first bind and `debug_assert!`s on every raw FFI entry, turning the documented soundness contract (V-note 1) into a checked invariant under `--features cuda`.
3. **End-to-end intrinsic lowering** — make `AllowIntrinsicCalls` actually work: resolve the `invokestatic` CP target and emit `sqrt.rn.f64` / `sin.approx.f32` etc., so the analyzer-eligible-but-always-lowering-rejected dead path (Stub #5) becomes real.
4. **Wire a GPU CI lane** behind the existing `gpu-it` feature that runs the `#[ignore]`d `ptxas_round_trip_*` and a numeric oracle, so PTX *correctness* (not just shape) is regression-guarded.
5. **Emitter defense-in-depth assertion** for the non-reduction scalar store (B3) — assert `StraightLine` shape before the plain `st.global [ret_ptr]`.
6. **Resolve the `cratonvm-gpu` packaging question** — its Rust API is dead at runtime; either (a) keep it but document it as a build-only `links`-channel crate and mark the `const`s `#[doc(hidden)]`/`#[allow(dead_code)]` with a note, or (b) fold the annotation-jar build into `jit-cuda`'s build.rs and drop the published crate. Publishing a crate whose only Rust surface is two unused `env!` consts is confusing for a public Apache-2.0 release.

---

## Files sampled vs fully read

**Fully read:**
- `craton-gpu/src/lib.rs`, `craton-gpu/build.rs`, `craton-gpu/Cargo.toml`
- `jit-cuda/src/lib.rs`, `signature.rs`, `analyzer.rs`, `annotations.rs`, `emitter.rs`, `test_support.rs`, `Cargo.toml`
- `jit-cuda/src/lowering.rs`, `lowering/loop_recog.rs`
- `jit-cuda/src/lowering/emit.rs` — read in full across chunks (structure grep + lines 29-403, 404-792, 793-972, 1180-1719); the div/rem/shift/conv helper bodies between 972-1180 were structurally confirmed via grep and spot-read.
- `cuda-bridge/src/lib.rs`, `backend_cuda.rs`, `backend_stub.rs`, `stream.rs`, `event.rs`, `launch.rs`, `Cargo.toml`
- `cuda-bridge/tests/stub_op_log.rs`

**Sampled (grep + targeted read, not line-by-line):**
- `jit-cuda/build.rs` (read ~first 120 lines; classpath-propagation logic confirmed)
- Cross-crate references to `cratonvm-gpu`/`craton_gpu` via grep to establish runtime-dead status of the lib API.
