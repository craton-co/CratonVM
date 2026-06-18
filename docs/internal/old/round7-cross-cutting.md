# Round 7 — Cross-cutting (build / allocator / profile)

## 1. [CRIT] Workspace has NO `[profile.release]` block — using cargo defaults

**File:** `Cargo.toml:1-62` (only `[profile.dev.package.miniz_oxide]` / `flate2` overrides exist)

Default release is `opt-level=3, lto=false, codegen-units=16, panic=unwind, incremental=false`. LTO off and 16 codegen units leaves 10-25% perf on the table for a tight interpreter loop, and prevents cross-crate inlining of the `value_stack` / `frame` `#[inline(always)]` helpers when they're called from `vm/runtime/interpreter.rs`. The whole inlining audit (`vm/src/runtime/value_stack.rs` ~40 `#[inline(always)]` sites) is mostly defeated without LTO across the workspace crate boundary.

**Fix:** Add to `Cargo.toml` (panic must stay `unwind` — 37 `catch_unwind` sites incl. `vm/src/lib.rs`, `runtime/jvmti.rs`):

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "unwind"
debug = "line-tables-only"   # for perf, profiler symbols
strip = "none"
incremental = false
overflow-checks = false

[profile.release-pgo]
inherits = "release"

[profile.bench]
inherits = "release"
```

## 2. [CRIT] No global allocator override — Windows HeapAlloc is the default

**File:** `vm-cli/Cargo.toml:23-30`, `vm-cli/src/main.rs:1-9` (no `#[global_allocator]`)

`Cargo.lock` contains no `mimalloc`/`jemallocator`. On Windows, the default `HeapAlloc` is ~2-3× slower than mimalloc for the workload's pattern (millions of small `Arc<str>`, `Vec<CompactValue>`, hashmap nodes). `Frame::new`, lockfree-resolve insert, vtable cache, and reader's `intern_arc` all hit this.

**Fix:** In `vm-cli/Cargo.toml` add `mimalloc = { version = "0.1", default-features = false }`; in `vm-cli/src/main.rs` top:
```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```
Expected 10-20% wall-time win on class-load-heavy workloads.

## 3. [HIGH] Tracing has no compile-time level cap — every dispatch checks the runtime filter

**File:** `Cargo.toml:19` (`tracing = "0.1"`), `vm/src/runtime/interpreter.rs:4355` (`trace!(pc=…, instruction=?instruction, …, "execute")` lives inside the per-opcode dispatch loop)

Even when the runtime subscriber filters out TRACE, the macro still calls into the `tracing` dispatcher to check level — measurable per-opcode overhead. `instruction = ?instruction` and `stack_depth = …` are also computed each call because tracing's lazy evaluation only kicks in inside callbacks.

**Fix:** In workspace `Cargo.toml`:
```toml
tracing = { version = "0.1", features = ["release_max_level_info"] }
```
Eliminates the per-opcode `trace!` at compile time. Also strips the GC-mark `tracing::debug!` paths in `runtime/interpreter.rs:1092` if downgraded to `release_max_level_warn`.

## 4. [HIGH] No `.cargo/config.toml` — `target-cpu=native` / `-C target-cpu=x86-64-v3` not applied

**File:** missing `.cargo/config.toml`; `native-awt/src/renderer.rs:309` runtime-dispatches SSE2 via `is_x86_feature_detected!`

The runtime check is correct (binary stays portable), but without a baseline above `x86-64` the rest of the workspace (hashing, interpreter ALU ops, copy loops) compiles to 2003-era ISA. JDK shadergen + JIT codegen inner loops would benefit from popcnt/lzcnt/bmi2.

**Fix:** Create `.cargo/config.toml`:
```toml
[build]
# rustflags = ["-C", "target-cpu=x86-64-v3"]  # opt-in: baseline = Haswell+
[target.'cfg(target_os = "windows")']
rustflags = ["-C", "link-arg=/STACK:8388608"]
```
Plus a `release-native` profile via `RUSTFLAGS="-C target-cpu=native"` env for local PGO builds.

## 5. [HIGH] `zgc.rs` (1884 LOC) compiled unconditionally despite being a stub

**File:** `gc/src/lib.rs:38` (`pub mod zgc;`), `gc/Cargo.toml:21-27` (only `gpu-offload` feature)

Round-5 review flagged ZGC as a stub. It still compiles into every build, inflating LLVM time and binary size. No callers in `vm/`/`gc/` outside the module itself (per round-5-gc).

**Fix:** `gc/Cargo.toml`:
```toml
[features]
default = []
zgc-stub = []
```
`gc/src/lib.rs:38`: `#[cfg(feature = "zgc-stub")] pub mod zgc;`.

## 6. [HIGH] Only 1 `#[cold]` annotation in the entire workspace

**File:** `native-api/src/registry.rs` (sole occurrence); missing on `vm/src/runtime/exceptions.rs` throw paths, `runtime/env_cache.rs:64,103` `disable_jit`/`strict_swallows` error reporters, `value_stack` `pop_*` `RuntimeError` returns, `frame.rs` OSR-reject branch.

Without `#[cold]`, LLVM treats exception/error paths as warm and may not push them out of line, polluting i-cache and forcing the branch predictor to stage them.

**Fix:** Add `#[cold]` to `set_jit_pending_npe`, `throw_*` helpers in `runtime/exceptions.rs`, the `Err(…)` arm constructors in `value_stack::pop_*`, and the `unimplemented!()` natives' bodies (wrap in `#[cold] fn unimpl() -> ! { … }`).

## 7. [MED] Hot-path `#[inline]` gaps after recent rounds

**Files / fix:**
- `vm/src/runtime/lockfree_resolve.rs:144` `get_method`, `:182` `get_field` — add `#[inline]` (called every invokevirtual/getfield miss-path).
- `vm/src/runtime/vtable.rs:237` `lookup_slot` — add `#[inline]`; this is the hash-probe under `resolve_virtual_method`.
- `vm/src/runtime/vtable.rs:478` `resolve_virtual` — `#[inline]` (small wrapper).

`frame.rs`, `value_stack.rs`, `byte_view.rs`, `mark_bitmap.rs`, `env_cache.rs` are already adequately annotated.

## 8. [MED] No PGO scaffolding

**File:** none

`cargo-pgo` workflow would yield 5-15% on the interpreter dispatch. No `[profile.release-pgo]`, no `scripts/pgo-*.sh`, no instructions in `BUILD_GUIDE.md`.

**Fix:** Add `[profile.release-pgo] inherits = "release"` and a `scripts/pgo.sh` running `cargo pgo build && cargo pgo run -- <bench> && cargo pgo optimize build`. Document in `docs/PROFILING.md`.

## 9. [LOW] `Frame` struct could shrink — `Vec<(usize,u32)> osr_attempt_counts` always allocated

**File:** `vm/src/runtime/frame.rs:163`

24-byte Vec header per Frame even when OSR never fires. `Option<Box<SmallVec<[(u32,u32);2]>>>` would shave 16 B and add a single null check on the hot exit path.

**Fix:** Convert `osr_attempt_counts: Vec<(usize,u32)>` → `osr_attempt_counts: Option<Box<smallvec::SmallVec<[(u32,u32);2]>>>`. Saves ~16 B × frame depth and pulls Frame back below a cache-line stride.
