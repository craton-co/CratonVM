# Round 4 — Misc Crates Performance & Bug Review

Crates: `types/`, `jit-api/`, `jit-cuda/`, `cuda-bridge/`, `vm-cli/`

Findings sorted by impact. Most of these crates are in good shape after rounds 1-3; the surface area for real perf issues is small.

---

## `cuda-bridge/`

### [HIGH] Single default stream — no overlap between H→D, kernel, D→H
File: `cuda-bridge/src/backend_cuda.rs:49` (and `:179-204`)

`DeviceContextInner::new` calls `ctx.default_stream()` and clones the same `Arc<CudaStream>` into every `DeviceBufferInner` (lines 173-186, 196). All `memcpy_stod`, `alloc`, `launch`, and `memcpy_dtoh` therefore execute serially on the default stream. The CUDA Driver API explicitly serializes operations on a single stream, so a typical `from_host → launch → to_host` sequence cannot overlap upload with compute or compute with download — exactly the pattern jit-cuda emits for element-wise kernels.

Impact: 2-3× wall-time penalty for kernels whose runtime is comparable to PCIe transfer time (vector_add, saxpy — i.e. every kernel the analyzer currently accepts).

Fix: add an optional `CudaStream` parameter (or two — upload / compute) to `DeviceContextInner::new`, route `from_host` and `to_host` to a copy stream, kernel launches to a compute stream, and gate `to_host` on a stream event recorded after `launch`. Even a 2-stream split (compute + copy) recovers most of the overlap.

### [MED] Host-side staging vector allocated per launch
File: `cuda-bridge/src/backend_cuda.rs:116`

`launch_raw` constructs a fresh `Vec<u64>` (`ptr_h`) for every kernel launch to hold device-pointer addresses the launch builder borrows. Pre-sizing with `Vec::with_capacity(args.raw.len())` would avoid the realloc on the first push for kernels with several pointer params (vector_add / saxpy have 3-4 pointer params, hitting the default-grow ladder of 4 → 8 reallocations). Even better: stack-allocate via `smallvec::SmallVec<[u64; 8]>` since the realistic upper bound is around 8 pointers.

Impact: one heap allocation per launch — modest, but kernel launches are short and meant to be cheap; this allocation is unconditional and on the critical path.

Fix: `let mut ptr_h: Vec<u64> = Vec::with_capacity(args.raw.len());` (one-line change, eliminates the realloc cascade with zero behavior change).

### [LOW] `synchronize()` is the only sync API — no per-buffer / per-event sync
File: `cuda-bridge/src/lib.rs:97-101`

`DeviceContext::synchronize` blocks until *all* stream work drains. Once stream overlap is wired (HIGH above) the bridge needs `record_event` / `wait_event` / per-buffer `sync()` so the GC copy-back path doesn't have to flush the whole context. Not a regression of current behavior — flagged because the HIGH fix above will need it.

---

## `vm-cli/`

### [MED] `expand_aggregate_jars` scans parent directory per missing entry, lowercases each filename twice
File: `vm-cli/src/main.rs:254-319` (esp. `:295-296`)

For every classpath entry that does not exist on disk, the helper does a full `read_dir` on the parent directory and then calls `name.to_ascii_lowercase()` twice per dirent (once for the `starts_with` check, once for the inequality check). A real Quarkus classpath has 200+ entries; if any prefix has many siblings (Quarkus' `lib/main/` has ~150 jars), the dirent-side allocation is paid twice per file per probe.

Impact: startup wall-time for cold-cache Quarkus/Keycloak runs — measurable (tens of milliseconds) on slow disks.

Fix: hoist `name.to_ascii_lowercase()` into a single `let lower = name.to_ascii_lowercase();` and reuse. While there, cache the per-parent `read_dir` results across iterations of the outer `for entry in entries` loop using a `HashMap<PathBuf, Vec<DirEntry>>` so multiple missing `netty-*` aggregates on the same classpath don't re-scan the same directory.

### [LOW] `MAX_CAUSE_CHAIN_DEPTH` constant declared but the cause-chain loop hardcodes `0..8`
File: `vm-cli/src/main.rs:22` (declaration) and `:1362` (use site)

`const MAX_CAUSE_CHAIN_DEPTH: usize = 8;` is defined "to extract magic numbers", but the rendering loop at line 1362 still reads `for depth in 0..8`. The dead constant is a documentation hazard (a future audit may bump the constant assuming the loop honors it; it won't).

Fix: `for _depth in 0..MAX_CAUSE_CHAIN_DEPTH` and drop the unused `let _ = depth;` at line 1596.

### [LOW] `class_name.replace('.', "/")` on every launch even when already in internal form
File: `vm-cli/src/main.rs:626` and `:631`

Minor — single string copy on startup. Flagged only because both `--jar`-mode and class-mode pay the cost unconditionally, and the internal/external check is a single `contains('.')`. Not worth fixing in isolation, but worth bundling with any future startup-budget pass.

---

## `types/`

### [LOW] `CompactValue::object` evaluates the same two asserts twice
File: `types/src/compact_value.rs:215-238`

The constructor first runs `debug_assert!(ptr != 0, ...)` and `debug_assert!(ptr & !PAYLOAD_MASK == 0, ...)`, then unconditionally re-runs both as `assert!(...)`. In release builds the `debug_assert!`s are stripped, so the duplication is only wasteful in debug — but it is also genuinely dead code: the `assert!` already covers the precondition in every profile. The "belt-and-suspenders" comment is misleading: there is only one belt.

Impact: ~zero runtime; minor source-noise / future-maintenance hazard (a reviewer changing the predicate has to update two copies).

Fix: delete the two `debug_assert!` lines (216-224); keep only the `assert!` block.

### [LOW] `ObjectRef::from_raw` panics in release on misalignment, but `ObjectRef::from_raw_nonnull` only `debug_assert!`s
File: `types/src/value.rs:77-92` vs. `:102-109`

`from_raw` promotes the alignment check to a release-build `panic!` (line 85-87 — `if (ptr as usize) % 8 != 0 { panic!(...) }`); `from_raw_nonnull` only checks in debug builds. Either both should match, or the doc should explicitly justify the asymmetry. Today, callers who hold a `NonNull<u8>` and route through `from_raw_nonnull` silently accept misaligned pointers in release, while callers passing `*mut u8` get a panic — same precondition, different safety net.

Impact: latent — `from_raw_nonnull` is the newer API and currently used by paths that already validated alignment upstream; if a future caller skips that validation the failure mode is a silent SIGSEGV instead of a clean panic.

Fix: mirror the unconditional alignment check in `from_raw_nonnull` (lines 103-107), or add a doc note explaining why the looser check is acceptable there.

---

## `jit-cuda/`

### [LOW] `analyzer::estimate_work` walks bytecode a second time after `scan_bytecode` already did
File: `jit-cuda/src/analyzer.rs:168-178` and `:293-325`

`analyze()` walks the method's bytecode once in `scan_bytecode` (forbidden-opcode scan) and a second time in `estimate_work` (backward-branch + length heuristic). Both use the same `instruction_size` table. For methods that pass eligibility — i.e. exactly the methods we are about to lower into PTX — this is paid up-front per offload candidate.

Impact: small (jit-cuda eligibility is rare and short-bytecode-only), but trivially fused: have `scan_bytecode` return both `Ok(())` and a `has_backward: bool` so `estimate_work` becomes a single arithmetic expression.

Fix: change `scan_bytecode` signature to `-> Result<bool, Reason>` returning `has_backward`; drop the second walk.

### [LOW] `build_param_list` re-allocates `format!("p{i}")` strings every lowering call
File: `jit-cuda/src/lowering.rs:127-191`

Every parameter name is built via `format!("p{i}")` / `format!("p{i}_ptr")` / `format!("p{i}_len")`, producing 1-3 fresh heap allocations per parameter per lower. Lowering happens per JIT-eligible method, not per launch, so the cost is bounded — but a 5-param kernel takes ~15 allocations of 4-byte names. Trivial fix once it starts showing up: pre-allocate from a small static table or use a `SmallString` / `ArrayString<8>`.

Not worth fixing today; flagged for completeness.

---

## `jit-api/`

No findings. The crate is data-only (`CachedBytecodeMethod` is `Arc`-backed and `Clone`-cheap; `JitRuntimeHelpers` is `Copy`). No trait-object indirection on hot paths — the JIT calls helpers via embedded absolute addresses, not vtables. The `gpu_lowering::GpuLowering` trait sits behind an `Arc<dyn>` per the module doc, but it's invoked once per method at compile time, not per launch.
