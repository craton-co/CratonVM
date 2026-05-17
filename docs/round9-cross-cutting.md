# Round 9 — Cross-cutting audit (build / measurement / hot paths)

Verification of round-8 cross-cutting deltas plus new opportunities.

## Round-8 verification (OK)

`Cargo.toml:90-96` `[profile.release]` lto=fat / cu=1 / panic=unwind still
intact; `[profile.bench]` (109-116) mirrors it. `vm-cli/Cargo.toml:36,47`
mimalloc opt-in. Cap `release_max_level_info` at `Cargo.toml:34`.
`gc/Cargo.toml:32` `zgc = []` feature gate. `vm/src/vm/vm_exec.rs:1919-1922`
spawn-thread default lowered to 2 MiB.

## Findings

### 1. [CRIT] `t2-census.yml` + `jck.yml` still clobber `+sse4.2,+pclmul`
`.github/.wf/t2-census.yml:20` and `.github/.wf/jck.yml:45` set
`RUSTFLAGS: -Dwarnings` (no SSE flags). Same regression round-8 fixed in
`ci.yml`/`bench-gate.yml`: env `RUSTFLAGS` **replaces** `.cargo/config.toml`
rustflags. JCK and T2-census runs measure an x86-64-v1 binary while
released binaries are v2 — divergent ISA, divergent perf.
**Fix:** Append `-C target-feature=+sse4.2,+pclmul` to both files'
`RUSTFLAGS:` envs (same string as ci.yml line 16).

### 2. [HIGH] `zip` workspace dep declared but two crates ignore it
`Cargo.toml:26` sets `zip = "2" …` as a workspace dep, but
`vm/Cargo.toml:62` and `classloading/Cargo.toml:20` still spell out the
literal `zip = { version = "2", default-features = false, … }`. Today
they unify, but any future workspace-dep features-bump (e.g. adding
`bzip2`) drifts silently. `native-builtins/Cargo.toml:58` zip = "0.6"
still pending TODO — round-8 finding #3 not yet landed.
**Fix:** Convert vm + classloading to `zip.workspace = true`; complete
the native-builtins `SimpleFileOptions` migration to drop zip 0.6.

### 3. [HIGH] No PGO scaffolding (round-7 #8, round-8 #4 still open)
No `scripts/pgo.sh`, no `[profile.release-pgo]` in `Cargo.toml`. Concrete
shape: (1) `cargo pgo build` writes instrumented binary to
`target/release/`; (2) drive it through `cargo run --release --bin
rustjvm -- --classpath bench QuickBench` + `cargo bench --bench
vm_benchmarks`; (3) `llvm-profdata merge -o merged.profdata
/tmp/pgo-data/*.profraw`; (4) rebuild with
`RUSTFLAGS="-Cprofile-use=$(pwd)/merged.profdata
-Cllvm-args=-pgo-warn-missing-function"`. Expected: 8-15% on interpreter
dispatch, 10-20% with BOLT post-link (`llvm-bolt rustjvm -o rustjvm.bolt
-data=perf.fdata -reorder-blocks=ext-tsp -reorder-functions=hfsort+
-split-functions -split-all-cold`).

### 4. [HIGH] Bench-gate misses JIT dispatch / GC barrier / monitor / throw
`vm/benches/vm_benchmarks.rs` has 15 benches; `bench_specjvm_compiler:570`
measures jit_scan only — no compiled-code dispatch, no write-barrier
cost, no `MONITORENTER`/`MONITOREXIT`, no exception construction+throw.
Rounds 4-8 touched all four. The 15% bench-gate cannot catch regressions
there.
**Fix:** Add `bench_jit_steady_state` (1M iterations of compiled fib),
`bench_gc_write_barrier` (10k object-field stores), `bench_monitor_recursive`
(50k uncontended enter/exit pairs), `bench_throw_catch` (10k AIOOB
trips).

### 5. [HIGH] `docs/PROFILING.md:76` still cites "Round 26 / 1.41x"
Round-8 #8 unaddressed. File pre-dates mimalloc, the criterion suite,
bench-gate, `+sse4.2`, and round-9 ISA baseline.
**Fix:** Rewrite with round-9 numbers, document `cargo bench --bench
vm_benchmarks`, `samply record cargo bench` (needs profile `bench` debug
upgrade — round-8 #7), `RUSTFLAGS=-C target-cpu=native` opt-in, PGO once
#3 lands.

### 6. [MED] Dead `DEFAULT_JAVA_STACK_SIZE = 64 MiB` constant
`vm-cli/src/main.rs:29` defines `const DEFAULT_JAVA_STACK_SIZE: usize =
64 * 1024 * 1024;` — zero references (`grep` confirms). Round-8 #2
lowered the real default to 2 MiB in `vm_exec.rs:1919`; the launcher
constant was orphaned, not deleted. Reading it suggests the wrong value.
**Fix:** Delete line 29; or repurpose as the documented `RUST_MIN_STACK`
fallback shared by both spawn sites (currently inlined as
`2 * 1024 * 1024` literal).

### 7. [MED] `vtable.rs` accessors not `#[inline]`
`vm/src/runtime/vtable.rs:262 get`, `267 len`, `272 is_empty`, `284
class_id` are 1-line accessors hit on every virtual dispatch — round-7
only inlined `lookup_slot` (240). Cross-crate callers (interpreter,
JIT-emitted ICs) see opaque calls.
**Fix:** Add `#[inline]` to all four; also `Itable::lookup`
(`vtable.rs:357`).

### 8. [MED] Transitive triplicates: `getrandom`, `hashbrown`, `windows-sys`
`cargo tree --duplicates` shows 3 versions each of getrandom (0.2/0.3/0.4),
hashbrown (0.14/0.15/0.16), windows-sys (0.48/0.52/0.61). Adds ~600 KiB
binary + 3× monomorphisation. getrandom 0.2 from `p12 0.6`, hashbrown
0.14 from our own `classloading/Cargo.toml:26` (older than indexmap's),
windows-sys 0.48 from a stale leaf dep.
**Fix:** Bump `classloading` hashbrown to 0.15; bump `p12` (or pin
`getrandom = "0.3"` in workspace); `cargo update -p windows-sys@0.48
--precise 0.52.x` if possible.

### 9. [MED] 11 crates each declare `parking_lot = "0.12"` directly
Workspace deps section never added `parking_lot`. If 0.12 → 0.13 ships
with a breaking SmallVec change, every crate needs a manual bump and one
will drift.
**Fix:** Add `parking_lot = "0.12"` to `Cargo.toml [workspace.dependencies]`;
replace 11 literals with `parking_lot.workspace = true`. Same treatment
for `rustc-hash = "1.1"` (5 crates).

### 10. [MED] `vm/build.rs:79` runs `javac -version` every incremental build
Round-8 #9 still open. `Command::new("javac").arg("-version").output()`
fork+exec on every `cargo build` (~50-300 ms cold). Edit-build-test loop
tax.
**Fix:** Stamp `OUT_DIR/.javac-probed` keyed on
`cargo:rerun-if-env-changed=JAVA_HOME`; skip probe if stamp present.

### 11. [LOW] `interpreter.rs:2082` `format!()` on every JIT-compile event
`let method_key = format!("{}.{}:{}", class_name_arc, method_name_arc,
descriptor_arc);` — runs once per compile (rare, but already have the
three Arc<str>s). Re-formatting them to `String` allocates 3 strs of
likely-cached content.
**Fix:** Reuse a `ThreadLocal<String>` scratch buffer with
`write!(buf, …)`; or precompute the key inside `JitMethodKey::display`.

### 12. [LOW] No `opt-level = "s"` candidates investigated
Cold one-shot crates (`vm-cli/build.rs`, `jit-cuda/build.rs` driver,
`jfr` event-table encoder, `native-awt` font shaping) compile at
`opt-level = 3`. Per-package `[profile.release.package.<crate>] opt-level
= "s"` for cold code shrinks icache pressure on hot crates' neighbours.
Worth measuring on `jfr` + `native-awt` (likely 50-100 KiB binary
shrink, 0 perf impact since they're cold).
