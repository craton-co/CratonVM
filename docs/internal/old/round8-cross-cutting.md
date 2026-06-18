# Round 8 — Cross-cutting audit (build / allocator / measurement)

Audit of round-7 cross-cutting fixes plus new opportunities.

## Round-7 verification (verified OK)

`Cargo.toml:82-89` `[profile.release]` lto=fat / cu=1 / panic=unwind, and `[profile.bench]` mirrors it. `vm-cli/Cargo.toml:36,47` + `vm-cli/src/main.rs:7-9` install mimalloc. `Cargo.toml:26` `release_max_level_info` elides `trace!`/`debug!` (e.g. `interpreter.rs:4530,7785`). `gc/src/lib.rs:42` cfg-gates `zgc` (0 callers). `lockfree_resolve.rs:152,193` + `vtable.rs:240` `#[inline]` landed; `exceptions.rs` has 8 `#[cold]`.

## Findings

### 1. [CRIT] CI workflows clobber `.cargo/config.toml` rustflags
**`.github/.wf/ci.yml:11`, `.wf/bench-gate.yml:26`** set env `RUSTFLAGS: -Dwarnings`. Cargo rule: env `RUSTFLAGS` **replaces** `target.<cfg>.rustflags`, no merge. CI drops `+sse4.2,+pclmul` from `.cargo/config.toml:18`, builds the x86-64-v1 baseline. Bench-gate numbers underestimate the binary users actually get.
**Fix:** Move `-D warnings` into `[workspace.lints.rust] warnings = "deny"` and unset env, or pass `--config 'build.rustflags=["-D","warnings"]'`.

### 2. [HIGH] 64 MiB per-thread stack reservation
**`vm-cli/src/main.rs:29,1957`**, **`vm/src/vm/vm_exec.rs:1924-1928`** — every Java thread gets a 64 MiB OS stack. WildFly spawns 100+ threads → 6.4 GiB virtual reservation. HotSpot default is 1 MiB Linux / 512 KiB Windows.
**Fix:** Drop default to 2 MiB; honour `-Xss`; let `StackOverflowError` retry on a larger stack rather than pre-reserving.

### 3. [HIGH] `zip` duplicated: 0.6 (native-builtins) vs 2.x (vm, classloading, native-io)
**`native-builtins/Cargo.toml:58`** pulls `zip = "0.6"`; the other three crates pull `zip = "2"`. Both link in → duplicate deflate, CRC32, ~200 KiB bloat. `cargo tree --duplicates` also shows 3× `getrandom`, 3× `hashbrown`, 3× `windows-sys`.
**Fix:** Bump native-builtins to `zip = { version = "2", default-features = false, features = ["deflate"] }`.

### 4. [HIGH] PGO scaffolding still missing
Round-7 finding #8 documented but never landed. No `[profile.release-pgo]`, no `scripts/pgo.{sh,ps1}`, no `BUILD_GUIDE.md` mention.
**Fix:** Add `[profile.release-pgo] inherits = "release"`; `scripts/pgo.sh` runs `cargo pgo build && cargo pgo run -- --classpath bench QuickBench && cargo pgo optimize build`. Pair with `llvm-bolt` for another 5-10%.

### 5. [MED] `tracing` re-declared instead of `tracing.workspace = true`
**`native-builtins/Cargo.toml:45`**, **`native-awt/Cargo.toml:19`** spell out `tracing = { version = "0.1", features = ["release_max_level_info"] }`. Today identical → harmless via Cargo feature unification. If workspace cap drops to `release_max_level_warn`, these silently keep DEBUG → drift.
**Fix:** Replace both with `tracing.workspace = true`.

### 6. [MED] `#[cold]` missing on `value_stack` error returns
**`vm/src/runtime/value_stack.rs:531,551,604,614`** (`pop_int/long/float/double`) return `Err(RuntimeError)` on type mismatch. Round-7 #6 said add `#[cold]`; didn't land.
**Fix:** Extract each `Err(...)` into a `#[cold] #[inline(never)] fn err_*()` helper.

### 7. [MED] `bench` profile has no debuginfo → flamegraphs unusable
**`Cargo.toml:101-108`** sets `debug = false` for benches. `samply`/`perf` on `cargo bench` output shows mangled hashes. `release-with-debug` (94-97) exists but `cargo bench` doesn't pick it up.
**Fix:** Change bench profile to `debug = "line-tables-only"` (~3% size, 0% perf cost).

### 8. [MED] `docs/PROFILING.md` is stale
**`docs/PROFILING.md:76`** quotes "Round 26 / 1.41x vs HotSpot". No mention of bench-gate, mimalloc, `RUSTFLAGS=-C target-cpu=native`, criterion suite (15 benches), or PGO.
**Fix:** Refresh; document `cargo bench --bench vm_benchmarks` + `samply record`.

### 9. [LOW] `vm/build.rs:79` shells out to `javac -version` every build
Test-only impact (~300-500 ms cold incremental). Adds latency to edit-build-test.
**Fix:** Stamp `OUT_DIR/.javac-checked`; gate via `cargo:rerun-if-env-changed=JAVA_HOME`.

### 10. [LOW] Main-thread stack bump is Windows-only
**`.cargo/config.toml:26`** sets `/STACK:16777216` only for `cfg(target_os="windows")`. Linux/macOS launcher gets 8 MiB from `ulimit -s`. Class-init recursion that needs 16 MiB silently SOs on tight `ulimit`.
**Fix:** Document `ulimit -s 16384` in `BUILD_GUIDE.md`, or spawn `main` on a sized `std::thread`.

### 11. [LOW] No JIT-specific bench in `bench-gate`
**`vm/benches/vm_benchmarks.rs`** has 15 benches (startup, GC, interpreter, shootout, specjvm). None exercise JIT codegen → JIT regressions slip past the 15% gate.
**Fix:** Add `bench_jit_compile_method` and `bench_jit_steady_state` (1M calls of compiled fib).

### 12. [LOW] `Frame::osr_attempt_counts: Vec<(usize,u32)>` (24 B/frame)
**`vm/src/runtime/frame.rs:163`** — round-7 #9 still open. Cold field paying hot cache footprint.
**Fix:** `Option<Box<SmallVec<[(u32,u32);2]>>>` — saves 16 B/frame, one null check on hot exit.
