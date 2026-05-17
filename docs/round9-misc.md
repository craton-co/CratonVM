# Round 9 — Misc Audit (native-awt, cuda-bridge, jit-cuda, jit-api, types, vm-cli)

Scope: regression audit of round-8 changes plus selected new angles.

## (A) Regression Audit

### native-awt

**PASS — `font.rs:497-542` (`GlyphAtlas::get_or_rasterize`):** Hot-path scoped lock + single-lock miss path correct. Bulk eviction is gated by `cache.len() >= self.cap && !cache.contains_key(&key)` (line 515), so the in-flight key is **never** evicted before its `entry().or_insert_with` call. Two racing threads both observe the same `Arc::ptr_eq` instance — verified by the `or_insert_with` semantics (only the first holder of the lock allocates).

**PASS — `edt.rs:465-502` (`coalesce_and_drain`):** Queue mutex is dropped (line 497) before invoking `notify_if_invocation`, which acquires the orthogonal `runnables` lock (line 389) and may transitively grab `pending_invocations` (line 365). Lock order `queue → (released) → runnables → pending_invocations` matches the established pattern in `poll_event`, `wait_event`, `drain_events`.

### cuda-bridge

**CRIT — `lib.rs:184-192` `launch_raw_no_sync` has zero callers.** `Grep` across the workspace finds the symbol only inside `cuda-bridge/` itself (definition + an internal `backend_cuda.rs` reference). `jit-cuda` and every other launch site uses `launch_raw`, so the ~3µs/launch win advertised in the round-7/8 notes is unrealized. Fix: wire `jit-cuda/src/lowering*.rs` GPU-offload launches to the no-sync variant when the next op is another launch on the same stream (the common back-to-back PTX kernel pattern), or delete the symbol if `jit-cuda` will never need it.

**PASS — `backend_cuda.rs:197-215` `optimal_block_size`:** `cudarc-0.13.9/src/driver/safe/core.rs:395` exposes `occupancy_max_potential_block_size((fn,usize,u32,Option<CUoccupancy_flags_enum>)) -> Result<(u32,u32)>` — call matches signature. `backend_stub.rs:62-68` returns `None`, and `lib.rs:108` `.unwrap_or(256)` provides the 256 fallback on stub + on driver-failure.

### vm-cli

**HIGH — `main.rs:1972` + `vm/src/vm/vm_exec.rs:1922` — 2 MiB stack mis-rationalized & risky.** The comment claims "matching HotSpot's `-Xss` default" but HotSpot's default `-Xss` is **512 KiB on Linux x64, 1 MiB on most other platforms** (NOT 2 MiB). More importantly, HotSpot's `-Xss` budgets **Java** stack — CratonVM's interpreter is recursive in **Rust**, so each Java frame costs 2–4 KiB of host stack. 2 MiB allows ~500–1000 Java frames; Quarkus/WildFly bootstrap chains routinely exceed that under recursive `<clinit>`. Fix: bump default to 8 MiB (the old Rust default, also matches glibc), keep `RUST_MIN_STACK` override, drop the "matches HotSpot" claim. Also delete the now-dead `DEFAULT_JAVA_STACK_SIZE = 64 MiB` constant at `main.rs:29`.

**LOW — `main.rs:641-662` `looks_quarkus`:** Lowercase happens once per filename, substrings (`"quarkus-"`, `"-runner.jar"`) are lowercase. Correct. Minor: manifest key check at line 661 (`k.starts_with("Quarkus-")`) is case-sensitive, but Java manifest keys are spec'd as case-insensitive — unlikely to matter in practice (Quarkus emits canonical-cased keys) but inconsistent with the filename hardening.

### types

**PASS — `compact_value.rs:156-160` `cold_degraded_object_ptr`:** `#[cold]` + `#[inline(never)]` correctly tag the null/unaligned object-pointer recovery as rare. The hot branch (line 514) inlines `Value::Object(Some(ObjectRef::from_raw))` while LLVM places the cold body off the hot icache line. -0.0 round-trip verified (bits `0x8000...` does not match `NANBOX_BITS=0xFFFC...`, stored untagged).

### jit-api

**PASS — `lib.rs:182-243` maintenance contract:** Docstring lines 184-200 clearly state the three-step contract (array literal, parallel `field_names`, `NUM_FIELDS` bump). Comment at line 239 explains why no runtime `debug_assert_eq!` (tautological — `arr.len()` is `NUM_FIELDS` by return-type pinning). TODO(round-9) for macro-ification is appropriate.

## (B) Remaining Items (carried)

**MED — `native-awt/src/natives.rs:1300-1306` `SwingUtilities.invokeAndWait`:** native delegates to `edt.invoke_and_wait_runnable`, which `assert!(!is_edt(), …)` panics at `edt.rs:316`. A panic across the JNI boundary is undefined behavior; the Java contract is to throw `InvocationTargetException` (or `Error`). Fix: in the native wrapper, check `edt::is_edt()` and return a thrown `java/lang/Error` instead of letting the assert fire.

**MED — `cuda-bridge`:** no `device_count` / `enumerate_devices` API surface anywhere; multi-GPU still single-device-zero only. Pinned-host alloc remains documented-TODO at `backend_cuda.rs:356-365`. Carry to round-10.

**LOW — `native-awt/src/renderer.rs:1169`:** bicubic TODO untouched in round-8.

## (C) New Angles

**LOW — `jit-api/src/gpu_lowering.rs:46` `Arc<dyn GpuLowering>`:** vtable call only at JIT-compile time (one `lower()` per method), not per JIT-invoked Java call. No perf concern; leave as-is.

**Files referenced (absolute):**
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-awt\src\font.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-awt\src\edt.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-awt\src\natives.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\cuda-bridge\src\lib.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\cuda-bridge\src\backend_cuda.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\vm-cli\src\main.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\vm\src\vm\vm_exec.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\types\src\compact_value.rs`
- `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\jit-api\src\lib.rs`
