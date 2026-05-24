# cuda-bridge review

Crate: `C:\Projects\CratonVM\cuda-bridge` (workspace member, `publish = false`).
Surface: 6 source files, 1 integration test file, 1 README, 1 dedicated CI workflow.
LOC (rough): `lib.rs` 671, `backend_cuda.rs` 917, `backend_stub.rs` 83, `event.rs` 397,
`stream.rs` 411, `launch.rs` 114, `tests/stub_op_log.rs` 275. Two build modes: default
"stub" (every fallible call returns `NoDriver`), and `cuda` (real `cudarc` 0.13 backend
against `cuda-12060`). All four CRIT/UAF/PERF audits referenced in comments
(2026-05-16 / 17 / 20 / 22 / 24) have landed in source; the 2026-05-24 stream-port fix is
the most recent and is partially correct.

## Summary

- **HIGH** — `DeviceBuffer::from_host_async` (cuda mode, `lib.rs:502`) submits the H→D
  copy on the **context's** `copy_h2d` stream, not on the user-supplied `stream`. A
  subsequent `module.launch_on_stream(&ctx, ..., &stream)` is **not** ordered after the
  upload (`launch_raw_on_stream` deliberately skips `e_h2d` wait — `backend_cuda.rs:400-404`),
  so the kernel reads uninitialised device memory. The cross-stream pipeline advertised
  by the README is silently broken under real CUDA.
- **HIGH** — `DeviceBuffer::to_host_async` (`lib.rs:534` → `backend_cuda.rs:837`) waits on
  the context-wide `e_k` event — which is only recorded by the *default* `launch_raw`
  path. `launch_on_stream` deliberately does not record `e_k`
  (`backend_cuda.rs:559-565`), so a download chained behind a user-stream launch is not
  ordered after that launch. Race: `dst` may be read before the kernel writes it.
- **HIGH** — `from_host_async` host-buffer lifetime contract is misdocumented
  (`lib.rs:499-501`). The doc says the slice must outlive
  "the next `stream.synchronize()`" of the *user* stream; in reality the copy is in
  flight on `copy_h2d` and is only observable after `copy_h2d.synchronize()` (or
  `ctx.synchronize()`). A caller that drops `host` after only the user-stream sync hits
  use-after-free of the host buffer the driver is still DMA-reading.
- **MED** — Every driver error is collapsed into `DeviceError::Driver(format!("{e:?}"))`.
  Distinct `CUresult` codes (OOM, illegal address, invalid context, launch failure) are
  lost, so callers can neither retry on OOM nor present meaningful diagnostics.
  `DeviceError::Load` is never produced; `Launch` and `KernelNotFound` are
  underused (`lib.rs:30-35`).
- **MED** — `unsafe impl Send/Sync` on `DeviceContext`, `DeviceBuffer`, `Stream`,
  `EventCuda`, `StreamBarriers` rely on an unwritten "caller must `bind_to_thread`"
  contract acknowledged in the source itself (`lib.rs:104-124`, `lib.rs:449-460`,
  `stream.rs:127-141`, `event.rs:68-82`). The bridge does **not** call
  `bind_to_thread` at public-API entry — driving any of these from a worker thread other
  than the one that constructed the context is UB by the CUDA driver model.

## 1. Code review

### Bugs (HIGH unless noted)

- **HIGH** `lib.rs:502` `DeviceBuffer::from_host_async` runs the H→D copy on
  `ctx.copy_h2d`, not on the user `stream`. The recorded `StreamOp::UploadAsync` is a
  no-op under cuda; the user stream's queue is untouched. Combined with
  `launch_raw_on_stream` (`backend_cuda.rs:392-414`) skipping the `e_h2d` wait, kernels
  launched on the user stream race the upload. **Fix**: submit
  `memcpy_htod_async` on `stream.raw()` (or insert `cuStreamWaitEvent(stream, e_h2d)`
  inside `launch_on_stream`).
- **HIGH** `backend_cuda.rs:837-864` `to_host_async_raw` waits the user stream on
  `self._barriers.e_k`, but `e_k` is only recorded by `launch_raw_inner` on the
  *context's compute stream* and **only when `needs_d2h_sync` is true**. After a
  `launch_on_stream` on a user stream the `e_k` event reflects a stale (or never
  recorded) marker, so the D→H copy is not ordered behind the kernel that produced the
  data. **Fix**: have `launch_on_stream` record an event on its own stream and have
  `to_host_async_raw` consume the caller-chosen event.
- **HIGH (UB)** `lib.rs:499-510` host slice lifetime — same misdirection: the doc
  promises that `host` need only outlive a sync on the *user* stream, but the copy is
  on `copy_h2d`. **Fix**: either document `ctx.synchronize()` as the sync point, or
  re-route the copy through the user stream.
- **MED** `backend_cuda.rs:780-825` `to_host` requires `dst.len() == self.len()`
  (strict equality), but `lib.rs:514` doc says "must be at least `self.len()` long".
  Code/doc mismatch; pick one.
- **MED** `backend_cuda.rs:50` `map_err`'s `format!("{stage}: {e:?}")` discards
  `CUresult` enum value; no programmatic recovery.
- **MED** `backend_cuda.rs:466-469` `ASSERT_DEVICE_REPR` only checks
  `size_of::<T>() > 0`. ZSTs are already excluded by `cudarc::DeviceRepr`; the const is
  cosmetic. (Reachable for stub mode wouldn't matter — it's `#[cfg(feature = "cuda")]`.)
- **LOW** `event.rs:99` doc claim "Re-recording on a stream overwrites the prior
  marker" is technically true for *future* waits, but `cuStreamWaitEvent` snapshots the
  marker at *submission* time — re-recording does not retroactively change in-flight
  waits. Reword.
- **LOW** `stream.rs:76`/`stream.rs:148` two independent ID counters (`STREAM_ID_COUNTER`
  for stub, `CUDA_STREAM_ID_COUNTER` for cuda). Each backend is in its own build, so the
  ID space is mode-local — fine, but the duplication invites confusion. Consider one
  counter.

### Vulnerabilities / soundness

- **HIGH (soundness)** `unsafe impl Send`/`Sync` on `DeviceContext` (`lib.rs:125-126`),
  `DeviceBuffer` (`lib.rs:461-462`), `Stream` (`stream.rs:143-145`), `EventCuda`
  (`event.rs:84-86`), `StreamBarriers` (`backend_cuda.rs:119-120`) all carry the
  *same unwritten caller contract* ("must `bind_to_thread` first"). The bridge does
  **not** enforce it. The only mitigating note is that today's sole cross-thread
  consumer (`vm/.../OffloadCache`) confines each context to one worker. As soon as a
  second consumer arrives, this is silent UB. **Fix**: wrap every cross-thread-callable
  entry point with a `self.dev.bind_to_thread()?` prelude (cost: one TLS check on
  the second-and-subsequent calls per thread).
- **MED** `backend_cuda.rs:442-549` raw-pointer marshalling — `launch_args` builds
  `*mut c_void` into `ptr_h` and `args.raw`. The reasoning that both backing stores
  outlive `launch_on_stream` is sound *as written*; the `_keep_alive` anchor at
  `:540` provides the compile-time pin. Soundness here hinges on
  `launch_on_stream` returning before the kernel actually starts dereferencing — true
  for cudarc 0.13 because it copies parameter bytes into the driver synchronously. If
  cudarc ever moves to a deferred-arg model this becomes UB. Add a regression test
  that intentionally drops `ptr_h` immediately after the call and expects a compiler
  error.
- **MED** `backend_cuda.rs:122-130` `StreamBarriers::Drop` ignores
  `event::destroy` errors via `let _ = ...`. A failure here leaks a CUevent. Log
  rather than swallow.
- **LOW** `lib.rs:174-190` `intern_kernel_name`: deliberate bounded leak. Sound, but
  poisoned mutex on `unwrap_or_else(|p| p.into_inner())` silently keeps using a half-
  mutated set after a panic — acceptable here because the only mutation is `insert`,
  but worth a comment.

### Stubs / TODOs

- **MED** `backend_cuda.rs:58-63` `device_count()` is `pub(crate)` and **never
  called** from any public-facing path. Multi-GPU enumeration is not actually exposed.
- **MED** `backend_cuda.rs:302-310` `launch_raw_no_d2h_sync` is `pub(crate)`, has no
  public wrapper on `DeviceModule`; `launch_raw_no_sync` is referenced in a doc comment
  but **doesn't exist**. The PERF Fix 3 work is dead code today.
- **MED** `backend_cuda.rs:327-345` `optimal_block_size` is `pub(crate)` and unused.
  `LaunchConfig::elementwise` still hard-codes block=256 (`lib.rs:62`). The Round-8
  fix never reached the public surface.
- **MED** `launch.rs:90-113` `launch_on_stream` unit tests are
  `#[ignore = "PHASE2-CUDA-TODO: needs DeviceModule::for_test() / DeviceContext::for_test() fixtures"]`.
  Two ignored tests.
- **LOW** `lib.rs:911` `device_ptr_arg` has `let _ = (&self.dev, &self.copy_d2h);
  // retained for future use` — dead-store comment for fields that are already
  retained by struct membership; remove the line.

### Performance

- **GOOD** `PTR_SCRATCH` / `ARG_SCRATCH` thread-locals
  (`backend_cuda.rs:230-246`, `:442-549`) eliminate per-launch heap allocation. Sound
  rent-back pattern, though a panic between `take` and `restore` leaves the
  thread-local empty (no UB, just a forgone optimisation). Wrap in a `Drop` guard for
  panic safety.
- **MED** Pinned host memory: the bridge advertises async transfer overlap but uses
  pageable host buffers. `cuMemcpyHtoDAsync` on pageable memory is **synchronous w.r.t.
  the host** (driver staging copy), so the three-stream pipeline cannot actually
  overlap H→D with compute. No `cuMemHostAlloc`/`cuMemHostRegister` path exists.
- **MED** `event.rs:243-279` no batched event API. Each `record_event`/`wait_event`
  is a separate FFI call; for a graph of N stages this is N round-trips. Consider
  exposing CUDA Graph capture (`cuGraphCreate`/`cuStreamBeginCapture`) for
  fire-and-forget micro-kernel chains.
- **LOW** `lib.rs:61-69` `LaunchConfig::elementwise` hard-codes block=256.
  `optimal_block_size` exists but is unreachable from the public API; see Stubs.
- **LOW** `backend_cuda.rs:529-533` per-launch `func.clone()` — cheap (Arc bump), but
  if cudarc's `CudaFunction::clone` ever stops being trivial this becomes a hot spot.

## 2. Tests

### Coverage

- **Stub unit tests in `lib.rs`** (3): `probe_returns_no_driver`,
  `device_context_new_returns_no_driver`, `launch_config_elementwise_sizing`.
- **`stream.rs` unit tests** (4): `stream_new_assigns_unique_ids`,
  `record_op_appends_to_log`, `synchronize_records_op`, `ops_returns_clone_not_drain`.
- **`event.rs` unit tests** (5): `event_new_assigns_unique_ids` (early-returns
  because `DeviceContext::new` returns `NoDriver` in stub — body unreachable),
  `record_event_writes_recorded_on_field`, `record_event_pushes_op_to_stream_log`,
  `wait_event_pushes_op_to_stream_log`, `query_unrecorded_returns_false`.
- **`launch.rs` unit tests** (2): both `#[ignore]`.
- **Integration `tests/stub_op_log.rs`** (4): `upload_async_records_byte_count`,
  `launch_on_stream_records_kernel_name`, `event_record_and_wait`, `three_stage_pipeline`
  — every one of them is **gated on `DeviceContext::probe()` succeeding**, which in
  stub mode it never does. So on a no-GPU CI box (the only mode where this file
  compiles, by its `#![cfg(not(feature = "cuda"))]` gate) every integration test
  **early-returns without executing the body**. Net executed integration coverage on
  CI: zero.
- **GPU-feature tests**: none. `gpu-it` feature exists (`Cargo.toml:37`) but there is
  **no `#[cfg(feature = "gpu-it")]`** test in the crate. The dedicated CI workflow
  (`.github/workflows/cuda-bridge.yml`) only runs `cargo check` for both feature sets;
  no `cargo test` at all.

### Gaps

- **HIGH** Zero behavioural tests of the `cuda` backend. The two `HIGH` bugs above are
  exactly the kind of cross-stream race that mock-mode op-log tests will never catch.
- **HIGH** Stub integration tests are vacuous on CI because of the
  `ctx_or_return!()` early-return. The crate ships with the *appearance* of a
  populated integration-test suite that actually executes nothing. Add a stub-friendly
  `DeviceContext::for_test()` (or a feature flag) so the bodies run.
- **MED** No tests for `KernelArgs::push_*` byte layout. `push_i32`/`push_f32`/
  `push_device_ptr` need a unit test that asserts the marshalled `*mut c_void` vector
  layout matches the CUDA kernel-parameter ABI.
- **MED** No tests for the UAF fix (`AUDIT 2026-05-22`). Concrete addition: in stub
  mode, drop the originating `DeviceBuffer` after `push_device_ptr` and assert (via
  `Arc::strong_count` on the keep-alive — needs an accessor) that the `KernelArg`
  still holds it. Under cuda mode, write a real-GPU test that drops the buffer
  immediately after pushing and verifies the kernel still reads the correct data.
- **MED** No tests for the new H2D-async path that the 2026-05-24 stream-port fix
  introduced. Under stub mode the only assertion is "op log gets `UploadAsync` with
  correct byte count" (`tests/stub_op_log.rs:55-77`), which never runs.
- **MED** No fuzz target. `KernelArgs` builder + `LaunchConfig::elementwise` are
  pure-Rust, deterministic, and accept arbitrary user input — a `cargo-fuzz` target
  fuzzing arg-marshalling against a model implementation would be cheap.
- **LOW** No test for `intern_kernel_name` dedup behaviour. Add a property test that
  loading the same name twice returns the same `'static` slot.
- **LOW** Coverage estimate (default stub backend, lib.rs free of cuda paths):
  approximately 35-45% line coverage executed on CI. With `ctx_or_return!()` gated
  bodies counted as executed: drops to ~25%. **Well below the 85% bar.**

### Concrete additions (prioritised)

1. `stub_op_log.rs`: replace `ctx_or_return!()` with a stub-friendly fixture so the
   four integration tests actually run on no-GPU CI. (~50 LOC, ships the test surface
   the crate already pretends to ship.)
2. Real-GPU regression test for the cross-stream race (HIGH-1/HIGH-2): three streams
   doing upload→launch→download, verifying the host result is correct. Gate behind
   `gpu-it`.
3. UAF regression test for `push_device_ptr` keep-alive (stub mode is enough: use
   `Arc::strong_count` via a test-only accessor on `KernelArg::DevicePtr`).
4. Property test: `LaunchConfig::elementwise(n)` produces `grid * block >= n` for
   arbitrary `n: u32`.
5. Fuzz target: `KernelArgs` builder + `launch_raw` argument marshalling against a
   model implementation (no driver needed; assert offset/alignment invariants).
6. CI: add `cargo test -p cuda-bridge` to `.github/workflows/cuda-bridge.yml`; today
   only `cargo check` runs.

## 3. Documentation

### Existing

- **Crate root** `lib.rs:4-18` — concise crate-level rustdoc covering the two
  compilation modes; correctly points at the README. Good.
- **README** (`README.md`, 132 lines) — build-mode matrix, CUDA-toolkit pin
  rationale, two worked examples (sync + async/streams), `StreamOp` variant table.
  Genuinely good.
- **Per-module rustdoc** — every public type (`DeviceCaps`, `LaunchConfig`,
  `DeviceContext`, `DeviceModule`, `DeviceBuffer`, `KernelArgs`, `Event`, `Stream`,
  `StreamOp`) has at least a one-paragraph doc.
- **In-source `AUDIT` blocks** are exceptional — every soundness/perf decision since
  2026-05-16 is annotated with rationale and prior-state. Internal-grade docs.
- **CI workflow** is documented in-line (`cuda-bridge.yml:1-17`).

### Missing / inconsistencies

- **HIGH** `README.md:53-79` "Streams, events, and async memcpy" — sketch contradicts
  reality. The example
  `let buf = DeviceBuffer::<f32>::from_host_async(&ctx, &host[..], &stream)?;`
  then `stream.synchronize()` will UB-touch a freed host buffer under real CUDA, per
  HIGH bug #3. The README needs either a fix or a "this is the contract we **want**;
  the real H2D copy currently runs on the context's `copy_h2d` stream" warning.
- **MED** No CUDA-version compatibility matrix. README says "CUDA Toolkit 12.x" but
  `cudarc` features pin `cuda-12060` specifically — what happens on 12050 or 12070?
  What's the minimum driver version (R525? R535?)? What's the highest cudarc-supported
  one?
- **MED** No "supported `T` for `DeviceBuffer<T>`" table. `DeviceElem` is sealed and
  the `cudarc::driver::DeviceRepr` impls are not enumerated. Callers must guess.
- **MED** No discussion of the `bind_to_thread` caller contract anywhere a user would
  see it (README + every `unsafe impl Send/Sync` source comment is internal). Public
  rustdoc on `DeviceContext`/`Stream`/`Event`/`DeviceBuffer` needs a `# Thread safety`
  section.
- **MED** No alignment with `docs/gpu/streams-events.md` (workspace-level streams &
  events doc). At minimum, link from README → that doc.
- **LOW** `lib.rs:7` "See the crate-level README.md" rustdoc link uses
  `[\`README.md\`](../../README.md)` — depending on the rustdoc backend that path may
  or may not resolve. Workspace doc-builds typically use crate-rooted paths.
- **LOW** `lib.rs:301`'s "Public surface: routed through
  `DeviceModule::launch_raw_no_sync`" — that wrapper does not exist. Either implement
  it or remove the reference.

## 4. OSS readiness

### Cargo.toml

- Workspace-inherited `version`, `edition`, `rust-version`, `license`,
  `repository`. Good.
- `description` set; `keywords = ["cuda", "ffi", "gpu", "nvidia"]`;
  `categories = ["api-bindings", "external-ffi-bindings"]`. **MED — `"nvidia"` and
  `"cuda"` are NVIDIA trademarks**; using them as keywords on a published crate
  invites NVIDIA's trademark guidelines (technically permissible in a
  descriptive/nominative sense but worth a NOTICE clarification — see below).
- `publish = false` inherited from `[workspace.package]` (`Cargo.toml:6`). The crate
  will not be publishable to crates.io until that's flipped.
- `[features]` are well documented in-line including a CI rationale.

### Headers / SPDX

- Every source file in this crate starts with
  `// SPDX-License-Identifier: Apache-2.0` and
  `// Copyright 2024-2026 Craton Software Company`. Consistent and correct.
- `Cargo.toml` declares `license.workspace = true` → `Apache-2.0` (workspace
  `:11`). Good.

### Workspace LICENSE / NOTICE

- Root `LICENSE` is the full Apache-2.0 text (10.8 KB). Good.
- Root `NOTICE` mentions only Craton — does **not** acknowledge `cudarc`, `bytemuck`,
  or the (dynamic-load) dependency on NVIDIA's proprietary `libcuda.so` /
  `nvcuda.dll`. **MED**: even a trivial line — "This product dynamically loads
  NVIDIA's CUDA Driver (libcuda) when built with the `cuda` feature. CUDA® is a
  trademark of NVIDIA Corporation. This project is not endorsed or sponsored by
  NVIDIA." — is the standard nominative-use disclaimer.

### Licence interaction

- **Apache-2.0 vs NVIDIA libcuda**: the crate **does not statically link** any
  proprietary NVIDIA code. `cudarc` is itself MIT/Apache-2.0 and uses `libloading`
  (or direct `extern "C"` in newer versions) to **dynamically open** `libcuda.so` /
  `nvcuda.dll` at runtime. The CUDA EULA permits redistribution of headers and
  end-user redistribution of the driver runtime in derivative works. The crate
  itself ships **no NVIDIA bytes** — only a runtime FFI binding. This is the same
  legal posture as `rust-cuda`, `tch-rs`, and `cudarc` upstream. **No blocker.**
- **`cudarc` 0.13** is dual-licensed Apache-2.0 / MIT — compatible. **bytemuck** is
  Apache-2.0 / MIT / Zlib — compatible.

### Blockers (publication)

1. `publish = false` at workspace level — intentional today; flip per-crate when
   ready.
2. NOTICE missing the NVIDIA-trademark / libcuda-dynamic-load disclaimer (MED).
3. README example contradicts the implementation under real CUDA (HIGH, see Docs).
4. Two HIGH cross-stream correctness bugs (cannot ship the async API as advertised).
5. Zero behavioural test coverage of the `cuda` backend (no `gpu-it` test
   exists despite the feature being declared).

## Top 5 fix priorities

1. **Fix `from_host_async` / `to_host_async` / `launch_on_stream` cross-stream
   ordering** so the user-supplied `Stream` actually receives the work, OR insert
   the missing `cuStreamWaitEvent`s on entry. Current state silently races under
   real CUDA. (HIGH bugs #1, #2, #3 — single coordinated fix.)
2. **Land a real `gpu-it` integration test** (three-stream upload→launch→download)
   that would have caught the cross-stream race, and wire `cargo test
   --features gpu-it` into a self-hosted GPU runner — even if it only runs nightly.
3. **De-vacuum the stub integration tests** — replace `ctx_or_return!()` with a
   stub-friendly `DeviceContext::for_test()` so the four `tests/stub_op_log.rs`
   tests actually execute on no-GPU CI (today they all early-return).
4. **Enforce the `bind_to_thread` caller contract** by adding a one-line
   `self.dev.bind_to_thread()?` prelude to every public method on `DeviceContext`,
   `DeviceBuffer`, `Stream`, and `Event`. Removes the open soundness liability on
   the `unsafe impl Send/Sync` blocks.
5. **Expand `DeviceError` to surface `CUresult`** (at minimum: `OutOfMemory`,
   `IllegalAddress`, `InvalidContext`, `LaunchFailed`, `NotFound`) so the rest of
   the workspace can retry on OOM / classify failures, and **add the NOTICE
   line** acknowledging dynamic `libcuda` linkage + NVIDIA trademark. These are
   small standalone changes that unblock OSS-readiness review.
