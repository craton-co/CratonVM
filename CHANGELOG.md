# Changelog

All notable changes to RustJVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
