# vm review

Crate: `cratonvm-vm` at `C:\Projects\CratonVM\vm` — the VM orchestrator,
bytecode interpreter, threading, monitors, exception machinery, JIT/GC bridge,
class-init linker, JNI surface, panic recovery, JVMTI, JDWP, and signal
handling. **94 source files / ~160 k LOC** under `src/` (largest: `vm.rs`
68 k but ~68 000 of that is the inline test module; production code in
`vm/`, `runtime/`, `threading/`, `memory/`, `jit/`, `native/`, `classloading/`,
`debug/`, `jvmti/`, `types/`). **4 458 tests** (3 616 in-crate +
842 integration across 89 files). `cratonvm-vm`, `version.workspace = true`
(0.3.0), `publish = false`, license `Apache-2.0`, every file carries an SPDX
header.

## Summary

- **HIGH** — `runtime/lockfree_resolve.rs:43` keys the per-thread and shared
  resolution caches purely on 3 × `FxHasher` 64-bit hashes
  (`ResolutionKey { class_hash, name_hash, desc_hash }`) with no string field
  and no redefine generation. FxHasher is not collision-resistant; a crafted
  classpath that hashes a hostile `(class, name, desc)` triple to the same key
  as a privileged JDK target returns the wrong `ResolvedTarget` from the
  invoke fast path → type confusion / privilege bypass under untrusted bytecode.
- **HIGH** — Pass 3 bytecode verifier is **unconditionally skipped** for any
  class whose name begins with `java/`, `jdk/`, `sun/`, `com/sun/`
  (`vm/vm_util.rs:312`-`316`). There is no enforcement elsewhere that
  user-supplied classpaths cannot define classes in those packages
  (`grep prohibited.package` returns zero hits across the crate). A malicious
  jar with `java/lang/Hostile.class` therefore loads *without* type-state
  verification and feeds the interpreter / JIT raw unverified bytecode that
  the operand-stack `*_unchecked` helpers (`runtime/value_stack.rs:220-526`)
  rely on being well-typed.
- **HIGH** — Lock-order enforcement is documented but **not wired**.
  `runtime/lock_order.rs:35` ("The wrappers are currently unused at the call
  sites; the codebase still uses raw `std::sync::Mutex`/`RwLock`") and
  `tests/lock_order_smoke.rs:9-19` confirm `SharedVm` still holds raw
  `parking_lot::RwLock`s. With ten contended subsystems
  (`class_manager`, `heap`, `monitors`, `thread_registry`, `statics`,
  `resolution_cache`, `string_pool`, `class_mirrors`, `native_methods`,
  `jni_global_refs`) the published 10-level hierarchy in
  `vm/src/runtime/lock_order.rs` is purely documentary.
- **HIGH** — `runtime/crash_handler.rs:230-322` panic hook and Unix signal
  handler call `format!`, `eprintln!`, `std::fs::write` (via
  `write_crash_report`) — none async-signal-safe. A SIGSEGV that fires while
  the malloc lock is held deadlocks inside the handler, masking the original
  crash. The handler already documents "we are in a signal handler so many
  things are unsafe" (`crash_handler.rs:309-310`) but ships them anyway.
- **MED** — `threading/monitor.rs:818, 842, 861, 879, 888` use
  `.expect("monitor inflation invariant: registry/mark-word desync")` on the
  entry path. The error variant exists precisely because the desync is
  reachable (see `MonitorTable::inflate_locked` doc, line 678-683); the
  monitor enter path should propagate `MethodCallFailed` rather than tear
  down the whole VM.
- **MED** — `runtime/interpreter.rs:4693` `_ => unreachable!()` arm on the
  return of `throw_runtime_error`. A future variant addition in
  `RuntimeError` → exception conversion silently becomes a hard panic on the
  interpreter hot path. The neighbour module documents `unreachable!()` is
  banned (lines 28-31) but this site slipped through.
- **MED** — Untrusted bytecode reaches the interpreter through helpers that
  bypass operand-type checks in release: 8+ `_unchecked` variants
  (`push_int_unchecked`, `pop_long_unchecked`, etc., `value_stack.rs:412-526`)
  with `debug_assert!` only. Sound iff the verifier is bulletproof on every
  reachable opcode; given the HIGH gap above and the verifier's own pre-Java-7
  / JSR/RET caveats called out in `vm_util.rs:322-329`, this stacks two
  contingent guarantees.

## 1. Code review

### Bugs / Vulnerabilities

- **HIGH — Hash-only resolution cache key**, `runtime/lockfree_resolve.rs:43`,
  `:52-58`, `:113-114`, `:305-306`. Both `ThreadLocalResolveCache` and
  `SharedResolutionState` use `ResolutionKey` (3 × `u64` FxHasher digests, no
  string field). Collisions return wrong `ResolvedTarget` / `ResolvedField`
  with no generation/epoch counter. Fix: store full `(Arc<str>, Arc<str>,
  Arc<str>)` keys, or hash with a keyed cryptographic hasher (SipHash with a
  per-VM key), plus an epoch counter incremented on every `redefine_class`.
- **HIGH — Pass 3 verifier skip on `java/`/`jdk/`/`sun/`/`com/sun/`** classes,
  `vm/vm_util.rs:312-316`. Combined with no `IllegalAccessError` /
  `SecurityException` rejection of user classes in the `java/*` package,
  any classpath jar can ship pre-verified-bytecode-looking but un-verified
  classes. Fix: enforce the reserved-package check on every non-boot
  classloader load *before* the skip decision, mirroring HotSpot's
  `package_definers` check (JVMS §5.3).
- **HIGH — Signal handler async-signal-unsafe calls**,
  `runtime/crash_handler.rs:241-262, 304-322`. `format!`, `eprintln!`,
  `std::fs::write` (via `write_crash_report`) all take heap locks; in a
  SIGSEGV handler from inside `malloc` they deadlock. The Unix handler does
  call async-signal-safe `libc::write` for the stderr line (line 320-322)
  but `write_crash_report` does its own `std::fs::File::create + .write_all`
  which goes through `__getreent`/libc malloc internally on many platforms.
  Fix: pre-allocate the report buffer at install time and use only
  `libc::write` / `libc::open` / `libc::lseek` (Linux/macOS) in the SIGSEGV
  path.
- **HIGH — Monitor inflation `.expect()` panics**, `threading/monitor.rs:818,
  842, 861, 879, 888`. The error case ("registry/mark-word desync") exists in
  the result type *because the path is reachable*. Panicking here trips the
  panic hook + the `catch_unwind` boundary; the proper response is
  `Err(MethodCallFailed::InternalError(...))` propagated to the interpreter.
- **MED — `unreachable!()` on interpreter unwind**, `runtime/interpreter.rs:4693`.
  Matches against `throw_runtime_error`'s return variant; not currently
  reachable but adding a third variant to `MethodCallFailed` (or changing the
  helper) becomes a silent panic. Replace with explicit `Err(other)` propagate.
- **MED — `get_arc()` double `unreachable!()`**, `vm/vm_init.rs:2378-2384`.
  Two `unreachable!()` clauses on `self_arc` access. Trips on any call before
  `Vm::new` finishes wiring `self_arc` (test paths can construct `SharedVm`
  directly — see the dedicated `set_global_shared_vm_for_hooks` workaround
  at `vm_init.rs:2263`).
- **MED — `runtime/diagnostics.rs:275 panic!()`** inside `record_swallow` gated
  on `CRATONVM_STRICT_SWALLOWS=1`. Documented as debug-only. Tolerable but
  surprising — an env-var-driven panic on the silent-swallow path.
- **MED — `runtime/gpu_marshal.rs:209, 237 panic!()`** in `host_view_i16` /
  `host_view_i8` GPU path. Fires on type-mismatch which "should never happen"
  but the caller is GPU offload code that has its own analyzer; a
  classification bug in the analyzer reaches a Rust panic.
- **MED — Out-of-band CRT static-destructor shim**, `lib.rs:132-285`.
  Windows-only test-harness hack that installs an SEH unhandled-exception
  filter calling `ExitProcess(0)`. The reasoning ("every test passed before
  the crash") is sound on the documented paths but the constant
  `EXPECTED_PANIC_COUNT = 20` (line 163) is a hardcoded count of
  ambient-panic slots; if a real production-side panic is added (the very
  thing the panic-gate above tries to prevent), this counter increment masks
  it from cargo. There's a `expected_panic_count_matches_source` regression
  test (lines 327-376) but it only counts `#[should_panic]`, not the
  4-slot ambient quota.
- **MED — `Monitor::enter` busy-loop on parked threads**,
  `threading/monitor.rs:385-387`. Re-acquire after `Object.wait` busy-loops on
  `state.owner != Some(thread_id)` calling `entry_condvar.wait` — fine in
  isolation, but combined with the KC16 watchdog stack-dump path at lines
  519-571 (polling on a 5 ms timer) and the per-thread parking permit
  semantics in `ParkState` (`threading/jvm_thread.rs:71-87`), the dispatch
  matrix of "parked vs. interrupted vs. notified vs. watchdog-snapshotting"
  is fragile. Recommend a single test that drives every transition.
- **MED — Verifier sees the `class_manager.read()` lock**, `vm_util.rs:304-340`.
  Verification reads `class_manager` under a `parking_lot::RwLock` read guard,
  then the verifier itself queries the class store. Mixed-write paths
  (`class_manager.write()` at lines 285, 335) can starve a long-running
  verifier and vice-versa under contention; no fairness guarantees from
  parking_lot's `RwLock`.
- **LOW — `dispatch_trace.rs:43` `OnceLock<Mutex<Vec<Slot>>>`** is a 256-slot
  ring behind a single `parking_lot::Mutex` with `try_lock` at every record
  site (`is_enabled()` gate notwithstanding). Header says "lock-free" but the
  module's own commit log (line 17-22) acknowledges this. Not a bug;
  documentation drift.
- **LOW — `windows_build_number()` parsing**, `vm_init.rs:104-110` (and
  elsewhere) shells out to `cmd /C ver` to get the Windows version — synchronous
  process start at every `Vm::new()` on Windows. Cache after first use.

### Stubs (exhaustive inventory across `src/`)

There are **0 `todo!()`** and **0 `unimplemented!()`** in production
code — every occurrence is inside `#[cfg(test)]`, doc comments, or the lint
regex itself (interpreter.rs:15628-15629).

**Production `panic!()` outside `#[cfg(test)]`:**
- `runtime/diagnostics.rs:275` — `CRATONVM_STRICT_SWALLOWS=1` (env-gated debug).
- `runtime/gpu_marshal.rs:209, 237` — type-mismatch assertion on GPU path.
- `vm/vm_util.rs:816` — `CRATONVM_STRICT_SWALLOWS=1` BigDecimal swallow.

**Production `.expect()` on hot paths:**
- `threading/monitor.rs:818, 842, 861, 879, 888` — monitor inflation desync
  (5 sites, see HIGH above).

**Production `unreachable!()`:**
- `vm/vm_init.rs:2381, 2383` — `get_arc()` self-arc invariants (2 sites).
- `memory/gc.rs:488, 496, 503` — locals/stack/printed ref-update branches
  inside `update_local_refs`-style helpers; should not be reachable given a
  correct GC pointer map but converting to `tracing::error!` + skip would
  be safer.
- `runtime/interpreter.rs:4693` — exception-throw return-variant fallback (1).
- `runtime/lock_order.rs:183` — inside `LockLevel` parse error helper.
- `runtime/offload.rs:1873` — inside GPU-offload analyzer (feature-gated).
- `runtime/soak_test.rs:1454` — inside `soak_test` module body (only runs as
  diagnostic).

**`TODO(round-N)` markers (kept for tracking, not stubs proper):**
- `threading/virtual_threads.rs:802` — virtual thread spawns brand-new OS
  thread per fiber (round-8 Bug 6).
- `jit/helpers.rs:246, 368, 396` — register-table coverage capped at 3-arg
  with-ctx / 4-arg no-ctx (round-4/6 wave-2/3). Methods with >4 args take
  interpreter slow-path; correctness OK, performance gap documented.
- `runtime/alloc_fastpath.rs:39` — `vec_pool_stats` feature ungated.
- `runtime/frame.rs:352` — fast-path exclusion narrowing (round-4 wave-3).
- `runtime/interpreter.rs:2012` — `code_size = 0usize` for compiled-size
  reporting (telemetry placeholder).

### Performance

- **MED — `vm.rs` is one 68 k-line file** with a 64 k-line inline test module
  starting at line 60. Compile-time / IDE-time cost is large; refactor the
  test module into `tests/unit_*.rs`.
- **MED — `lockfree_resolve.rs` ThreadLocalResolveCache evicts via VecDeque
  pop_front** (line 181) — already O(1), but the eviction loop at line 178
  runs every miss and can churn under hot resolution storms. A 4096-entry
  cap with FIFO eviction on a 100 k-class workload constantly evicts; a
  size-tier'd LRU would amortise.
- **MED — `vm_util.rs:303-341` verifier guarded by `class_manager.read()` plus
  `class_manager.write()` releases.** Multiple coarse-grained guard pairs
  per class init; switching to a single guard for the whole linking
  sequence (Verifying → Verified → Prepared) would halve the lock acquisitions.
- **LOW — `runtime/exceptions.rs:37-60` `set_detail_message_by_name` walks the
  class hierarchy linearly on every exception construction** — exception
  paths are already cold (`#[cold]` at line 77) so this is fine, but a
  per-class cached `detail_message` slot index would shave off the walk.
- **LOW — `dispatch_trace` records via Mutex** despite the "lock-free" header
  (acknowledged at line 17-22). Future round-N work.

### Soundness — unsafe blocks (397 across 25 files)

- **interpreter.rs (51 unsafe sites)** — every site is annotated `// SAFETY:`
  per the `#![deny(clippy::undocumented_unsafe_blocks)]` gate (line 51). Hot
  paths read padded bytecode bytes (`code_ptr.add(saved_pc+2)`,
  `interpreter.rs:2798-2812`); padding guaranteed by
  `frame::padded_bytecode` (`runtime/frame.rs:28-34`).
- **native/jni.rs (117 unsafe sites, 192 `extern "C"`)** — JNI surface;
  callers must follow the JNI spec. Function table is `[usize; 234]` cast to
  function pointers per index.
- **memory/gc.rs (20 unsafe sites)** — `ObjectRef::from_raw(addr as *mut u8)`
  conversions after the GC pointer-map remap. Each carries a
  `debug_assert!(new_addr != 0)`. Sound only if every step of
  `update_all_roots` reaches every live root — and **`update_all_roots`
  only walks the *current* thread**; cross-thread root state is in
  `thread_registry`'s `root_snapshot` deposits, but a mid-update remap of
  *that* snapshot is not done in `update_all_roots` (handled by the GC
  barrier via `check_post_block_gc` instead). Worth a dedicated assertion
  test.
- **jit/helpers.rs (80 unsafe sites)** — function-pointer `transmute` to the
  register-table ABIs (line 277-394). Sound iff codegen emits matching
  `extern "C"` signatures; covered by `jit-cuda` and `jit` crate tests but
  no contract test here.

## 2. Tests

**Total: ~4 458 tests** (3 616 in-crate + 842 in `vm/tests/`). 89 integration
files; build.rs auto-compiles `tests/resources/cratonvm/*.java`. One bench
file (`vm_benchmarks.rs`), Criterion harness.

### Coverage estimates by subsystem

| Subsystem | LOC | Tests | Est. coverage | Comment |
|---|---:|---:|---:|---|
| interpreter dispatch (`runtime/interpreter.rs`) | 17 242 | 108 in-crate + integration JTReg/TCK | ~85 % | NEW-7 "no production panic" CI gate (lines 15614-15643) is gold-standard; opcode coverage via `interpreter_tests.rs` + `tier1_tests.rs`. Gap: wide-instruction (`wide iinc`) edge cases, table/lookup-switch boundaries, JSR/RET pre-Java-7 paths. |
| vm dispatch (`vm/vm_exec.rs`) | 10 039 | 78 in-crate | ~85 % | Same CI gate. Default-method rescue + `PendingCpIfaceGuard` covered by `wave3_b2_dispatch.rs`, `wp2_9_findspecial.rs`. |
| vm init / linker (`vm/vm_init.rs`) | 8 843 | 184 in-crate | ~80 % | Classpath parsing well-tested; auto-discovery of JDK 8 / JDK 9+/jlink jimage path well-covered. Gap: container/cgroup env interaction, `CRATONVM_JAVA_HOME` fallback edge cases. |
| exception machinery (`runtime/exceptions.rs`) | 888 | 21 in-crate + `exception_tests.rs` (62) + `exception_edge_tests.rs` (~50) | ~80 % | Good coverage of `RuntimeError → Java exception` conversion; gap: `OutOfMemoryError` from inside `create_exception_object` (the helper handles the cycle but no test asserts no-recursion). |
| monitors / thin locks (`threading/monitor.rs`) | 1 822 | 25 in-crate + `monitor_stress.rs` | ~70 % | Stress test drives thin → inflate → contended; gap: the `.expect()` desync paths at lines 818/842/861/879/888 are not directly tested. |
| GC roots (`memory/gc.rs`) | 506 | 5 in-crate + `tier1_tests.rs` | ~70 % | The 12-step `update_all_roots` walk; gap: cross-thread root-snapshot remap not asserted. |
| lock order (`runtime/lock_order.rs`) | 868 | 29 in-crate + `lock_order_smoke.rs` | API-only ~95 % / runtime-wired 0 % | Wrappers tested in isolation; `SharedVm` does not use them. |
| signals / crash (`runtime/signals.rs`, `runtime/crash_handler.rs`) | 1 057 + 877 | 61 + 16 in-crate | ~70 % | Crash report formatting & NPE-JEP-358 well-covered; gap: actual SIGSEGV delivery + async-signal-safe write path not exercised under a real signal. |
| JIT integration (`runtime/jit_integration.rs`) | 1 300 | 50 in-crate | ~75 % | dispatch fallbacks at >4 args (`jit_arity_5plus.rs`); gap: GC interrupting mid-JIT-call. |
| invokedynamic (`runtime/invokedynamic.rs`) | 2 020 | 23 in-crate + `wave2_c_methodhandles.rs`, `wp2_5_proxy.rs` | ~75 % | Lambda + MethodHandle bootstrap paths; gap: `CONSTANT_Dynamic` GC remap (handled at gc.rs:212 but not asserted under load). |
| native JNI surface (`native/jni.rs`) | 5 224 | 71 in-crate | ~70 % | Function-table coverage; gap: OnLoad/OnUnload protocol (Round-near-term roadmap item). |
| thread registry (`threading/thread_registry.rs`) | 998 | 23 in-crate | ~75 % | Daemon thread, async-exception slot, reverse park-map covered; gap: `Thread.stop0` racing with target's safepoint. |
| virtual threads (`threading/virtual_threads.rs`) | 2 215 | 68 in-crate + `vthread_probe_regression.rs` | ~70 % | TODO(round-8 Bug 6) at line 802: spawns OS thread per fiber. |
| JVMTI (`runtime/jvmti.rs`, `jvmti/*`) | 3 920 + 950 + 481 + 351 + 243 | 66 + 25 + 14 + 7 + 5 = 117 in-crate | ~70 % | Feature-gated under `experimental-debug`. |
| JDWP (`debug/*`) | 1 655 + 2 308 + 823 + 458 + 276 + 181 | 27 + 29 + 22 + 9 + 8 + 3 = 98 in-crate | ~70 % | Protocol parsing solid; gap: real `tests/clinit_order_tests.rs`-style end-to-end remote attach. |

### Gaps / concrete additions

1. **`tests/resolution_cache_collision.rs`** — proptest generating
   `(class, method, desc)` triples and asserting no false positives even
   under deliberate collision attacks against `FxHasher`. (Pending the HIGH
   fix.)
2. **`tests/verifier_reserved_package.rs`** — load a synthetic
   `java/lang/Hostile` class on a non-boot classpath; assert
   `IllegalAccessError` / `SecurityException`, never silent acceptance.
3. **`tests/monitor_inflate_desync.rs`** — drive the
   `inflate_locked` registry/mark-word desync path
   (manual `monitors.lock()` mutation between mark-flip and registry insert)
   and assert a clean `MethodCallFailed` propagates instead of `.expect()`-
   tripping.
4. **`tests/gc_cross_thread_root_remap.rs`** — spawn N threads, each parked
   in `Object.wait`, trigger a copying GC, and assert every parked
   thread's `root_snapshot` ObjectRefs are remapped. (Closes a hole around
   `update_all_roots` only walking the current thread.)
5. **`tests/signal_handler_safety.rs`** — fork a child, raise SIGSEGV from
   inside `malloc` (LD_PRELOAD or jemalloc hook), assert child writes the
   `hs_err_pid*.log` file within N ms and exits with the re-raised signal
   code. (Linux-only; gated.)
6. **`tests/lock_order_wired.rs`** — placeholder failing test until
   `SharedVm` uses `OrderedRwLock` for `class_manager`, `heap`, `monitors`;
   currently the smoke test exercises the wrapper API in isolation only.
7. **Fuzz target `vm/fuzz_targets/dispatch_arbitrary_bytecode.rs`** —
   currently `fuzz/` covers the reader/classloading; extending to the
   interpreter would surface stack-tag mismatches in the `_unchecked`
   helpers under unverified bytecode (HIGH item above).
8. **`tests/interpreter_unreachable_paths.rs`** — drive every documented
   `unreachable!()` arm (interpreter.rs:4693, lock_order.rs:183,
   offload.rs:1873) via a debug-only feature flag that returns
   `Err(...)` instead, asserting the VM survives.
9. **Property tests for `value_stack.rs` `_unchecked` helpers** — pre/post
   `len` invariant + tag invariant under random push/pop sequences within
   verifier-conformant traces.
10. **`tests/clinit_failure_propagation.rs` already exists** (`clinit_order_tests.rs`)
    — extend with a test that asserts `ExceptionInInitializerError` wraps
    the original exception (`vm_util.rs:809+` BigDecimal swallow path).

## 3. Documentation

### Existing

- `vm/README.md` (46 lines) — accurate, lists scope and non-goals.
- `src/lib.rs:1-31` — crate-level doc with subsystem map; `src/vm.rs:4-14`
  ties `SharedVm` / `JvmThread` / `Vm`.
- `src/runtime/interpreter.rs:1-46` — bytecode interpreter philosophy,
  NEW-7 panic-discipline gate.
- `src/runtime/lock_order.rs:1-39` — the self-contained canonical lock-order
  authority (the L0–L10 table; formerly cross-referenced a standalone
  `docs/lock-order.md` that has since been consolidated into this module) and
  states the wrappers are not yet wired.
- `src/threading/monitor.rs:1-112` — thin-lock state-machine doc.
- `src/threading/gc_barrier.rs:1-58` — STW + blocked-thread accounting.
- `src/runtime/crash_handler.rs:1-9` — overview.
- Inline `// SAFETY:` annotations on every unsafe block (gated by
  `#![deny(clippy::undocumented_unsafe_blocks)]` in interpreter.rs and
  vm_exec.rs).

### Missing / improvements

- No `MODULES.md` mapping the 94 source files to the README sections; the
  current README scope bullet conflates `runtime/`, `vm/`, and `memory/`.
- `vm/src/runtime/lock_order.rs` documents the hierarchy but does not list the
  call-site wiring status — outdated relative to
  `runtime/lock_order.rs:35`'s "not wired" admission.
- No rustdoc on the major public types' invariants:
  - `SharedVm` (`vm/vm_init.rs:230`) has 50+ public fields with
    field-level doc but no struct-level "thread-safety guarantees" section.
  - `JvmThread` (`threading/jvm_thread.rs:33`) has no module-level lifecycle
    diagram (created when? destroyed when? roots scanned by whom?).
- `docs/jvm-no-synthetic-stubs.md` referenced by Cargo.toml feature comments
  (line 14-21) exists but does not cross-link the per-stub status (the
  `audit_missing_natives` mechanism in `vm_init.rs:46-65` is undocumented
  outside source).
- README does not call out the **optional features** (`deprecated-noop-tls`,
  `legacy-synthetic-crypto`, `management`, `experimental-serialization`,
  `experimental-aot`, `experimental-debug`) — users picking custom feature
  sets have no document to consult. (The first and third were named
  `experimental-tls` / `experimental-jmx` before 2026-07-30.)
- Crash-handler module doc mentions the async-signal-safety concern in a
  comment (`crash_handler.rs:309-310`) but `docs/internal/` has no parent
  document explaining the threat model or the migration plan.
- `harness_exit_shim` in `lib.rs:56-285` has excellent in-source rationale
  but no companion `docs/internal/windows-test-shim.md` to find without
  grepping.

## 4. OSS readiness

| Item | Status |
|---|---|
| `Cargo.toml` package name `cratonvm-vm` | OK |
| `license.workspace = true` → Apache-2.0 | OK |
| `publish.workspace = false` | OK (workspace policy) |
| `description` populated | OK (`"Java Virtual Machine implementation in Rust"`) |
| `repository` from workspace | OK |
| `readme = "README.md"` | OK |
| `keywords` / `categories` from workspace | OK |
| Every `src/**/*.rs` carries `SPDX-License-Identifier: Apache-2.0` + copyright | OK (grep confirms 0 omissions) |
| `NOTICE` file at workspace root | OK |
| `[lints] workspace = true` | OK |
| MSRV 1.77 declared via workspace | OK |
| No `static mut` (rust-2024 future-proof) | OK |
| `#![cfg_attr(not(test), deny(...))]` panic gates on hot files | OK (`interpreter.rs:37-46`, `vm_exec.rs:15-24`, `threading/jvm_thread.rs:17-24`) plus CI gate at `interpreter.rs:15614-15643` |
| README example uses real public API (`Vm::new(VmConfig::default())`) | OK |

**Blockers for a hypothetical first publish:** the HIGH issues above
(`ResolutionKey` collision risk, Pass-3 skip + no reserved-package check,
signal-handler async-safety, lock-order wiring) are correctness / security
gaps that would need documentation at minimum (`SECURITY.md` already lists
"research-grade software" — at least the resolution-cache + reserved-package
issues should be explicit there).

**Soft blockers:** the 68 k-line `vm.rs` test module makes published rustdoc
gigantic; splitting to `tests/unit_*.rs` would help downstream consumers.

## Top 7 fix priorities

1. **Replace `ResolutionKey` hash-only key with full string identity + per-VM
   redefine epoch counter.** `runtime/lockfree_resolve.rs:43-58, 113, 305`.
   The HIGH security/correctness issue. ~1 day; possibly perf cost — measure
   with the existing `vm_benchmarks` harness.
2. **Enforce reserved-package check on user classpath** in
   `vm/vm_util.rs:312-316` (or one level up in the class loader) before
   skipping Pass 3. ~half-day; emit `IllegalAccessError` / `SecurityException`
   per JVMS §5.3.
3. **Wire the documented lock hierarchy.** Replace `parking_lot::RwLock` /
   `Mutex` on the ten `SharedVm` subsystems with the existing
   `OrderedRwLock` / `OrderedMutex` wrappers from `runtime/lock_order.rs`.
   Tracked in the smoke test's preface (line 9-25); biggest single
   correctness improvement after the verifier hole.
4. **Remove the 5 `.expect()` panics from `MonitorTable::enter`**
   (`threading/monitor.rs:818, 842, 861, 879, 888`) and propagate the
   existing `MethodCallFailed` error variant. ~half-day.
5. **Convert signal-handler write path to async-signal-safe primitives.**
   Pre-allocate the `hs_err_pid*.log` buffer at install time, use only
   `libc::open` / `libc::write` / `libc::pwrite` from the SIGSEGV handler
   (`runtime/crash_handler.rs:290-329`). Linux/macOS only; Windows uses SEH
   already. ~1 day.
6. **Replace `runtime/interpreter.rs:4693 unreachable!()`** with explicit
   propagation of the unknown variant and add a unit test that drives the
   missing arm. Companion: audit the other 5 `unreachable!()` sites listed
   in §1 Stubs. ~half-day.
7. **Split `vm.rs`'s 64 k-line inline test module to `vm/tests/unit_*.rs`.**
   Compile-time win, IDE-friendliness, rustdoc-friendliness. ~1 day mechanical;
   may surface a few hidden `pub(crate)` exposures that the inline module
   relied on.
