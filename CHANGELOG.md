# Changelog

All notable changes to RustJVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Session 100 (2026-05-02) — ServerLogger CCE + System.exit + RBIGDEC.1 partial

Four-agent parallel batch dispatched; three shipped, one (Agent A,
JDKModuleLogger NPE) returned no work product.

- **Session 99 baseline (commit `b9f421d`)** — ManagementFactory
  UnsatisfiedLinkError + JBoss LM synthetic shim landed via 2-agent
  merge (Agent 1 + Agent 4 from a prior batch).
  - ManagementFactory `<clinit>` ULE — RESOLVED. The vm_exec.rs
    override-allowlist for `System.loadLibrary` / `Runtime.loadLibrary*`
    was a dead-code path (target natives weren't registered in real-JDK
    mode). Fix: register them as no-ops in `register_vm_management_impl`
    plus `ManagementFactory.loadNativeLib()V`. Files:
    `native-builtins/src/jmx.rs` (+128).
  - JBoss LogManager synthetic shim (Block 2C). New file
    `native-builtins/src/jboss_logmanager.rs` (394 lines) provides
    synthetic `org/jboss/logmanager/LogManager`. Eliminates the
    `WARNING: Failed to load the specified log manager class` line
    and routes WildFly logger calls to stderr or
    `org.jboss.boot.log.file`.
- **Session 100 (commit `022c043`)**:
  - **ServerLogger `_$logger_en_US` ClassCastException** — RESOLVED.
    Non-obvious root cause: WildFly's `_$logger_en_US.class` is
    intentionally absent; JBoss-Logging probes locale-specific names
    wrapped in `catch (CNFE)` and falls back to the locale-less
    `_$logger`. Our class loader synthesized an interface stub for the
    missing name (because it contains `$`); JDK bytecode then ran
    `Class.asSubclass(Logger.class)` which threw CCE. Fix:
    `classloading/src/class_manager.rs:1172-1190` (new match arm) +
    `:3325-3344` (helper `is_jboss_logging_locale_lookup`) — return
    `ClassNotFound` for `_$logger_<locale>` / `_$bundle_<locale>`
    patterns instead of synthesizing a stub.
  - **System.exit(1) source identified + soft-return env shipped.**
    Caller chain: `Main.main → Module.run → org/jboss/as/server/Main.abort
    → SystemExiter.logAndExit → DefaultExiter.exit → System.exit(1)`.
    Added `RUSTJVM_DBG_EXIT=1` (caller-chain dump) and
    `RUSTJVM_SOFT_EXIT=1` (env-gated soft-return; default behavior
    unchanged). With soft-exit set, KC16 main reaches normal completion
    past `Main.abort` for the first time.
    File: `native-builtins/src/lang_system.rs`.
  - **RBIGDEC.1 — partial.** Real root cause identified (deeper than
    Session 98's intCompact theory): `bi_read` / `bi_signum` / `bd_read`
    natives in `native-builtins/src/lib.rs` use synthetic-stub slot
    indices but real-JDK BigInteger layout differs at slot 1. Slot-0
    fixable via descriptor-cache poison + String overlay; slot-1 NOT
    fixable the same way (other bytecode reads `mag` as `[I`). Out of
    single-file scope to refactor `lib.rs` natives.
    Files: `vm/src/vm/vm_util.rs` (+172/-42, more thorough fixup +
    descriptor-cache poison + dual-layout overlay) +
    `vm/tests/rbigdec1_arithmetic.rs` (new, 197 lines).
    BdProbe progression: `0\n0\nOK` → `0\nObject@1ea\nOK`. Target
    `11\n20\nOK` requires `lib.rs` refactor.

KC16 boot post-Session-100:
- DEFAULT mode: 1 swallow remains (JDKModuleLogger NPE — Agent A's
  failed territory) + System.exit(1) terminates. rc=0.
- WITH `RUSTJVM_SOFT_EXIT=1`: main() reaches normal completion past
  the bootstrap fatal-error path. WildFly init proceeds further than
  ever observed. rc=0.

Apps: HelloWorld / AnnoTest / FjpSum / CipherProbe / DigestProbe
unchanged. BdProbe better.

### Session 97 (2026-05-02) — partial: VMManagementImpl signature fixes

Six-agent batch dispatched to close the remaining KC16 boot blockers
(`ManagementFactory.<clinit>` UnsatisfiedLinkError, BigDecimal arithmetic
returning 0 on post-clinit-populated statics, JIT regalloc clobber on
deep recursion, three blocks of JBoss LogManager wiring). All six agents
hit token limits before iterating on their first attempts. Only one
agent shipped a net-positive partial:

- **RKC16N.12 (partial)** — VMManagementImpl int-typed thread counters +
  uptime / processor natives. `getLiveThreadCount` / `getPeakThreadCount` /
  `getDaemonThreadCount` were registered with descriptor `()J` (long), but
  JDK 25 declares them as `()I` (int) — the dispatcher matches on full
  descriptor so the registrations were silently ignored, surfacing as ULE
  during `ManagementFactory.<clinit>`. Re-registered with `()I` and added
  the missing `getUptime0()J` + `getAvailableProcessors()I` natives. Added
  `System.loadLibrary` / `Runtime.loadLibrary0` override-allowlist entry
  so the loadLibrary("management") path doesn't throw. Files:
  `native-builtins/src/jmx.rs` (+93), `vm/src/vm/vm_exec.rs` (+28).
  Tests: `vm/tests/management_factory_clinit.rs`,
  `jmx_tests::test_vm_management_impl_int_typed_thread_counters`,
  `jmx_tests::test_vm_management_impl_uptime_and_processors`.
- **`ManagementFactory.<clinit>` swallow still fires** post-fix — at least
  one more missing native deeper in the JMM init chain (likely in
  `sun/management/MemoryPoolImpl`, `MemoryManagerImpl`,
  `GarbageCollectorImpl`, or `HotSpotDiagnostic`). Diagnose via
  `RUSTJVM_STRICT_SWALLOWS=1`.

Not delivered (deferred to a future batch with smaller scopes):

- RBIGDEC.1 (BigDecimal arithmetic on populated statics)
- RFJP.1 (JIT regalloc clobber on deeply-recursive `RecursiveTask<Long>`)
- Block 2A (JBoss LM JAR auto-discovery — agent's wiring exists but
  doesn't make the class reachable to `Class.forName` from inside the
  JDK's `java.util.logging.LogManager.<clinit>`; not merged)
- Block 2B (`LogManager.getLogManager()` factory for arbitrary subclasses)
- Block 2C (synthetic `org.jboss.logmanager.LogManager` shim fallback)

### Session 96 (2026-05-02) — KC16 boot-blocker batch

- **`Object.get(Object)Object` `NoSuchMethodError` on KC16 boot — RESOLVED.**
  `native-builtins/src/lang_system.rs::native_system_getenv_all` was allocating
  the returned HashMap with `ClassId::new(0)`; the dispatcher's stale-pointer
  detector misrouted the resulting `Map.get(key)` invokeinterface to
  `java/lang/Object`. Fix: route allocation through
  `ctx.ensure_class_initialized("java/util/HashMap")`. Pinned by
  `vm/tests/wp8_10_10_system_getenv_map_class.rs`.
- **`BigDecimal.<clinit>` NPE cascade on KC16 boot — RESOLVED on the boot
  path** (arithmetic still red — see RBIGDEC.1 follow-up). Two fixes in
  `vm/src/vm/vm_util.rs`: (a) `set_static_by_name` was using enumerate-indexing
  where it should have been using static-only indexing — JDK classes with
  interleaved static/instance fields (BigDecimal has `JLA`/`INFLATED` between
  instance fields) silently wrote statics into instance slots; (b) added
  post-clinit fixup arms for `BigInteger` and `BigDecimal` populating
  `ZERO`/`ONE`/`TWO`/`NEGATIVE_ONE`/`TEN` when the swallow path triggers.
- **`ClassLoader.getResources` for classpath JARs — pinned by regression test.**
  Override-allowlist in `vm/src/vm/vm_exec.rs` + JAR walker in
  `native-builtins/src/classloader.rs::cl_get_resources` had landed silently
  in a prior commit; `vm/tests/rslf4j1_get_resources.rs` now locks it in.
  Verified end-to-end on Windows.
- **22 leaked diagnostic eprintlns removed** from `native-builtins/src/`
  (`[WP4.2 essential]`, `[FJPTRACE]`, etc.). 14 deleted, 8 converted to
  `tracing::debug!`. CI gate: `scripts/check-no-diag-prints.sh` (called from
  `.github/workflows/ci.yml`) enforces 0 hits across the workspace. Closes
  RJ.1.

#### Known limitations after Session 96
- `RBIGDEC.1`: BigDecimal/BigInteger arithmetic on the post-clinit-populated
  statics returns 0 (`BigDecimal.ONE.add(BigDecimal.TEN)` → `0`). KC16 boot
  doesn't compute with these values, so it's unblocked, but anything that
  does (JDBC numeric, Jackson numeric) remains broken. Reproducer:
  `apps/bigdecimal_probe/BdProbe.java`.
- `RFJP.1`: `pool.invoke(RecursiveTask)` for divide-and-conquer at depth ≥10
  returns 0. JIT correctness bug in deeply-recursive boxed-Long arithmetic
  (likely register clobber across `jit_invoke_dispatch`). Workaround:
  `RUSTJVM_DISABLE_JIT=1`. Pinned (failing) by
  `vm/tests/fjp_recursive.rs::fjp_probe_recursive_returns_correct_sum`
  (`#[ignore]`-gated).
- KC16 main() exits 0 but the WildFly ServiceContainer never starts:
  `ManagementFactory.<clinit>` swallow remains, JBoss LogManager wiring not
  yet done. See `docs/kc16-blocker-map.md`.

### Added
- **JIT XMM register allocation for float/double locals** — graph-coloring allocator now runs a separate XMM pass assigning float/double locals to XMM8-XMM15 (callee-saved on Windows x64). Previously all FP locals spilled to frame memory. Prologue saves/restores callee-saved XMMs via R11 (not RAX, which holds the return value). Zero-init loop skips XMM registers already loaded from params.
- **JIT Math.sqrt intrinsic** — `invokestatic java/lang/Math.sqrt:(D)D` is now inlined as `SQRTSD XMM0, XMM0` via a sentinel `JitDirectCall` (entry = `MATH_SQRT_INTRINSIC`). Eliminates interpreter dispatch overhead for sqrt in FP-heavy methods.
- **JIT `dup2` opcode** — duplicate top two operand stack slots. Required for compound array assignments like `a[i] += x`. Implemented via peek-and-copy (no pop/re-push needed).
- **JIT `ldc2_w` opcode** — load long/double constant from constant pool. New `cp_ldc2w_resolver` resolves CP index to i64 value at compile time; value embedded as immediate `MOV RAX, imm64`. Methods with unresolved ldc2_w bail out to interpreter.
- **JIT OSR XMM transfer** — OSR trampoline now loads float/double locals into their assigned XMM registers via `MOVQ XMMn, RAX`, not just GPR registers. Required for correct OSR entry into FP-heavy methods.
- **GC allocation: removed redundant double-zeroing** — `alloc_array` was zeroing the data region after `try_alloc_young` had already zeroed the entire block. Removed the redundant O(size) memset.
- **GC allocation: lock scope reduction** — `try_alloc_young` now releases the young-gen mutex before zeroing memory. The O(size) memset no longer holds the global allocation lock.
- **Math.sqrt intrinsic in OSR and early JIT paths** — OSR compilation now detects `invokestatic java/lang/Math.sqrt` and emits `SQRTSD` instead of going through `jit_invoke_dispatch`. Also resolves `ldc2_w` constants in the OSR path. N-Body improved from 167x to ~100x vs JDK.
- **Streams collect() no longer stack-overflows** — verified `stream().filter().collect(Collectors.toList())` works correctly (returns correct size). Previously caused infinite recursion.
- **JIT getstatic caching** — unique static field values are loaded once in the method prologue and cached in dedicated frame slots below the operand stack. Subsequent `getstatic` calls load from cache (1 MOV) instead of calling `jit_getstatic` helper (function call + RwLock read). `putstatic` refreshes the cache.
- **JIT Xmm StackSlot** — new `StackSlot::Xmm(u8)` variant for the simulated operand stack. `dload`/`fload` of XMM-allocated locals now push a zero-cost register reference instead of materializing through RAX→frame. `emit_double_binop` consumes Xmm slots directly via `MOVSD` instead of `MOVQ GPR→XMM`. Double binop results stay in XMM0 as `Xmm(0)` — consecutive FP operations chain without memory access. Math.sqrt SQRTSD also uses Xmm input/output. `flush_xmm0_slots()` ensures correctness before XMM0-clobbering operations. N-Body improved from 464x → **20x** vs JDK.
- Extracted 11 crates from monolithic vm: classloading, gc, jit, jit-api, types, native-api, native-builtins, native-collections, native-io, native-sql, jfr
- G1 and ZGC garbage collectors
- AArch64 JIT backend (45%)
- Java Flight Recorder support
- JVMTI event framework
- Security hardening: checked arithmetic in GC and JIT

### Changed
- Updated benchmark numbers: QuickBench 1.41x, Fannkuch 1.57x, N-Body 167x (was 464x interpreter-only, now JIT-compiled with XMM allocation) vs JDK 25.0.1 C2 (measured 2026-03-31)
- Added Binary Trees (CLBG) benchmark: 23.3x ratio reveals GC allocation bottleneck
- N-Body and Fannkuch-Redux benchmarks now run correctly (were blocked by JIT bugs)
- Rewrote roadmap.md with honest production-readiness evaluation: 18% real progress (was 75% measuring stubs)
- Roadmap now distinguishes real working features from Rust-side stub implementations
- New tiered priority matrix: Tier 0 (basic correctness) through Tier 3 (production grade)
- New measurable success metrics table verified against real Java code execution

### Fixed
- **VM exceptions now catchable by Java try/catch** — AIOOBE, NPE, ArithmeticException, ClassCastException and all other RuntimeErrors are now converted to real Java exception objects via `throw_runtime_error()` and routed through exception table handlers. Previously these were Rust-side errors that bypassed Java exception handling entirely. Fixes: fast-path bytecodes (xaload, xastore, aastore) and fast-path invoke dispatch (invokevirtual, invokespecial, invokestatic, invokeinterface) all now convert RuntimeErrors to catchable exceptions.
- **HashMap.entrySet() iteration** — added `synthetic_implements()` name-based type compatibility check for synthetic classes that lack proper interface registration in the ClassStore. HashMap$Entry, TreeMap$Entry, etc. now pass checkcast/instanceof to Map$Entry. Also covers Iterator, Iterable, Collection, and Comparable patterns.
- **Thread(Runnable) constructor** — registered `<init>(Ljava/lang/Runnable;)V` and `<init>(Ljava/lang/String;)V` constructors. Runnable target stored in field 3 where `Thread.run()` already reads it.
- **Thread.start()** — registered as alias for `start0()` so Java code calling `thread.start()` works.
- **Class.getName()** — registered `getName` as alias for existing `getName0` implementation. Also added `getSimpleName`.
- **FileWriter** — registered constructors and write methods for `java/io/FileWriter`, reusing FileOutputStream's fd-based I/O. Supports String path, File path, append mode, and write(String).
- **JIT return type for Z/B/C/S** — JIT-compiled methods returning boolean (Z), byte (B), char (C), or short (S) were mapped to `Ok(None)` (void) instead of `Ok(Some(Value::Int(...)))`. Also added float (F) and double (D) return type handling.
- **JIT call dispatch for float/double arguments** — `execute_jit_call()` was converting `Value::Float` and `Value::Double` to `0` (via `_ => 0` catch-all) instead of preserving their bit patterns. Fixed by adding `Value::Float(f) => f.to_bits() as i64` and `Value::Double(d) => d.to_bits() as i64`. This caused all JIT-compiled methods receiving float/double parameters (e.g., `advance(double dt)`) to see `0.0` instead of the actual value.
- **JIT invoke_dispatch missing thread context** — `set_jit_thread()` was not called before executing JIT-compiled code via `compiled.call()`/`call_with_context()` in the `execute()` function. This caused `jit_invoke_dispatch` to return 0 for all `invokevirtual`/`invokeinterface` calls because `jit_thread_mut()` returned `None`. Fixed by adding `set_jit_thread(thread)` before JIT execution and `clear_jit_thread()` after.
- **Stream.filter() with lambda predicates** — the two JIT fixes above together fix `stream.filter(x -> x > 3).count()` which previously returned 0. Root cause was: (1) JIT-compiled lambda body's `invokevirtual Integer.intValue()` returned 0 due to missing thread context, and (2) the boolean return value was discarded as void.
- **JIT register allocation correctness** — `build_interference()` in `jit/src/regalloc.rs` rewritten from block-level to instruction-level liveness analysis. Fixes Fannkuch wrong results where locals sharing registers (e.g., locals 2 and 3 both mapped to R12) produced incorrect values.
- **JIT stack canonicalization at branch targets** — forward gotos now canonicalize operand stack to `base_spill + i*8`; dead-to-live transitions reconstruct canonical stack; fall-through at branch targets also canonicalize. Fixes miscompilation on complex control flow.
- **JIT ifeq..ifle codegen** — changed from `CMP reg,reg` (always ZF=1) to `TEST reg,reg` (properly sets ZF/SF based on value). Added `emit_test_r32_r32` and `emit_cmp_r32_r32` helpers.
- **JIT if_icmpXX codegen** — optimized to use `slot_to_gpr` + `emit_cmp_r32_r32` instead of redundant stack manipulation.
- **N-Body segfault** — root-caused to loop unrolling producing corrupt native code (not FP-specific). N-Body now runs to completion with correct results.
- Integer truncation in array allocation (security)
- Unchecked branch offsets in JIT (security)
- Path traversal in resource loading (security)
- StringBuilder O(n^2) insert performance
- Bytecode verifier now accepts InterfaceMethodref for invokestatic/invokespecial (Java 8+ static interface methods)
- SSLEngine handshake state machine: wrap/unwrap/beginHandshake transitions
- Crypto deriveKey/deriveData now call HKDF implementation
- File descriptor leak in fd_table: rollback on overflow, close() returns Result
- Serialization write methods now throw UnsupportedOperationException instead of silently succeeding
- JIT negative cache: failed JIT compilations no longer re-attempted on every invocation
- vm-cli args array error handling: proper map_err instead of with_context on non-Error type

### Disabled
- **JIT loop unrolling** — disabled in `jit/src/x64.rs` (`compiler.unroll_loops = Vec::new()`). The byte-copy unrolling approach produced corrupt native code when the tuple fix made it find correct loops. Must be reimplemented before re-enabling.

### Known Issues (Roadmap Phase 16 — Core VM Correctness)
- ~~VM-generated exceptions cannot be caught~~ **FIXED** — now catchable via Java try/catch
- ~~HashMap.entrySet() ClassCastException~~ **FIXED** — synthetic type compatibility
- ~~Thread(Runnable), Class.getName(), FileWriter missing~~ **FIXED** — registered
- ~~invokeinterface lambda dispatch~~ **FIXED** — JIT thread context + return type mapping
- Streams collect() causes interpreter stack overflow (recursive architecture)
- FP code interpreter-only (464x slower than JDK) — JIT FP codegen needs work
- GC 23x slower than JDK on allocation-heavy workloads (Binary Trees)

## [0.2.0] - 2025-06-01

### Added
- x86-64 JIT compiler with 26 optimization rounds (~140 bytecodes compiled)
  - AVX2 SIMD vectorization for integer reduction loops
  - On-Stack Replacement (OSR) at hot loop back-edges
  - Loop-Invariant Code Motion (LICM)
  - Array Bounds Check Elimination (BCE)
  - Magic number division (no IDIV)
  - SSE float/double arithmetic pipeline
  - SoA (Structure-of-Arrays) value layout for 44% memory reduction
- Generational garbage collector with write barriers and card table
- Multi-threading with monitors, ReentrantLock, CountDownLatch, Semaphore, CyclicBarrier
- Virtual threads (simplified carrier-based scheduler)
- Java 11 support: nest-based access control (JEP 181)
- Java 17 support: records (JEP 395), sealed classes (JEP 409)
- Java 21 support: pattern matching for switch, sequenced collections
- Java 25 support: stream gatherers, scoped values, structured concurrency
- Panama FFI: MemorySegment, Arena, ValueLayout, SymbolLookup, Linker (downcall/upcall)
- 3,100+ native method registrations across java.lang, java.util, java.io, java.time, java.nio
- Full reflection: Class.forName, Method.invoke, Field.get/set, Constructor.newInstance
- Lambda/invokedynamic via LambdaMetafactory and StringConcatFactory
- CONSTANT_Dynamic (condy) support
- Enhanced NPE messages (JEP 358)
- Hidden classes (JEP 371)
- Partial JNI function table (229 slots, 13 implemented)
- Module system basics: Module, ModuleDescriptor, ModuleLayer
- Class file versions 45-69 (Java 1.1 through Java 25)
- Dependabot for automated dependency updates
- CODEOWNERS for review routing
- GitHub Security Advisories for private vulnerability reporting
- ARCHITECTURE.md for contributor onboarding
- Release workflow for automated binary builds

### Changed
- Improved SAFETY documentation on unsafe blocks in heap allocator
- Added checked allocation methods (`alloc_object_checked`, `alloc_array_checked`)
- Replaced test `panic!()` calls with proper `assert!` macros in GC and JIT tests
- Updated test count references across all documentation (6,000+)

### Performance
- Within 1.41x of JDK 25 C2 on QuickBench overall
- Fibonacci(42): 1.07x — within 7% of C2

## [0.1.0] - 2025-01-15

### Added
- Bytecode interpreter with 200+ JVM instructions
- `.class` file parser supporting all standard attributes
- Command-line launcher with classpath and heap size configuration
- CI pipeline with cross-platform testing, coverage, and Miri

[Unreleased]: https://github.com/craton-co/rust-jvm/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/craton-co/rust-jvm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/craton-co/rust-jvm/releases/tag/v0.1.0
