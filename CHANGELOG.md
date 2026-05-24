# Changelog

All notable changes to CratonVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-05-24

### Added
- JIT XMM register allocation for float/double locals (callee-saved XMM8-XMM15 on Windows x64), eliminating frame spills for FP-heavy methods.
- JIT `Math.sqrt` intrinsic inlined as `SQRTSD` instead of going through interpreter dispatch.
- JIT `dup2` opcode support, enabling compound array assignments like `a[i] += x`.
- JIT `ldc2_w` opcode support for loading long/double constants from the constant pool.
- JIT OSR trampoline now transfers float/double locals into their assigned XMM registers.
- JIT `getstatic` caching: unique static field values are loaded once in the method prologue and cached in frame slots.
- JIT `StackSlot::Xmm` operand-stack variant so consecutive double operations chain in XMM registers without memory traffic.
- Extracted 10 crates from the monolithic vm: classloading, gc, jit, jit-api, types, native-api, native-builtins, native-collections, native-io, jfr.
- G1 and ZGC garbage collectors.
- AArch64 JIT backend (partial; 45% of x86-64 opcode coverage).
- Java Flight Recorder support.
- JVMTI event framework.
- Security hardening: checked arithmetic throughout GC and JIT.

### Changed
- Updated benchmark numbers against JDK 25.0.1 C2: QuickBench 1.50x, Fannkuch 1.57x, N-Body 20x (down from 464x interpreter-only).
- Added Binary Trees (CLBG) benchmark, exposing a GC allocation bottleneck (23.3x ratio).
- N-Body and Fannkuch-Redux benchmarks now run to completion with correct results.
- Rewrote roadmap with an honest production-readiness evaluation distinguishing real working features from Rust-side stubs.
- New tiered priority matrix (Tier 0 basic correctness through Tier 3 production grade) with measurable success metrics verified against real Java code.

### Fixed
- VM-generated exceptions (NPE, AIOOBE, ArithmeticException, ClassCastException, etc.) are now catchable by Java `try/catch` instead of being Rust-side errors that bypassed exception handling.
- `HashMap.entrySet()` iteration: synthetic inner-class types like `HashMap$Entry` now satisfy `checkcast`/`instanceof` against `Map.Entry`, `Iterator`, `Iterable`, `Collection`, and `Comparable`.
- `Thread(Runnable)` and `Thread(String)` constructors are now registered; `thread.start()` works as an alias for `start0()`.
- `Class.getName()` and `Class.getSimpleName()` are now registered.
- `java.io.FileWriter` constructors and write methods are registered, including append mode and `File`-path overloads.
- JIT-compiled methods returning `boolean`/`byte`/`char`/`short`/`float`/`double` now return the correct value instead of being treated as `void`.
- JIT call dispatch now preserves `float` and `double` argument bit patterns (previously collapsed to 0).
- JIT invoke dispatch now installs the thread context before executing compiled code, fixing `invokevirtual`/`invokeinterface` returning 0.
- `Stream.filter(...).count()` and `stream().filter(...).collect(...)` now return correct results (previously returned 0 or stack-overflowed).
- JIT register allocator rewritten to use instruction-level liveness, fixing Fannkuch miscompilations where two locals shared a register.
- JIT operand-stack canonicalization at forward-branch targets and dead-to-live transitions, fixing miscompilation on complex control flow.
- JIT `ifeq..ifle` now uses `TEST` instead of `CMP reg,reg`, correctly setting flags.
- JIT `if_icmpXX` codegen optimized to use direct register comparison.
- N-Body segfault root-caused to loop unrolling producing corrupt native code; N-Body now runs cleanly with unrolling disabled.
- Integer truncation in array allocation (security).
- Unchecked branch offsets in JIT (security).
- Path traversal in resource loading (security).
- StringBuilder `insert()` O(n^2) performance regression.
- Bytecode verifier now accepts `InterfaceMethodref` for `invokestatic`/`invokespecial` (Java 8+ static interface methods).
- `SSLEngine` handshake state machine: `wrap`/`unwrap`/`beginHandshake` transitions.
- Crypto `deriveKey`/`deriveData` now call the HKDF implementation instead of returning empty output.
- File descriptor leak in `fd_table`: rollback on overflow, `close()` returns `Result`.
- Serialization write methods now throw `UnsupportedOperationException` instead of silently succeeding.
- JIT negative cache: failed compilations are no longer re-attempted on every invocation.
- `vm-cli` args-array error handling uses `map_err` instead of `with_context` on non-`Error` types.

### Performance
- GC `alloc_array` no longer double-zeroes the data region; the redundant memset after young-gen allocation is removed.
- GC young-gen mutex is released before the zero-init memset, so large-allocation latency no longer holds the global allocation lock.
- N-Body FP arithmetic improved from 464x to 20x vs JDK 25 C2 via XMM stack slots, `Math.sqrt` intrinsic, and OSR XMM transfer.

### Known Issues
- JIT loop unrolling is disabled; the previous byte-copy unrolling produced corrupt native code and needs to be reimplemented before re-enabling.
- BigDecimal/BigInteger arithmetic on post-clinit-populated statics returns 0 (`BigDecimal.ONE.add(BigDecimal.TEN)` yields 0). Boot paths that only reference these values work; numeric workloads (JDBC numeric, Jackson numeric) do not.
- `ForkJoinPool.invoke(RecursiveTask)` at recursion depth >= 10 returns 0 due to a JIT register clobber in deeply-recursive boxed-`Long` arithmetic. Workaround: disable the JIT for affected workloads.
- GC throughput is roughly 23x slower than JDK on allocation-heavy workloads (Binary Trees).

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

[Unreleased]: https://github.com/craton-co/cratonvm/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/craton-co/cratonvm/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/craton-co/cratonvm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/craton-co/cratonvm/releases/tag/v0.1.0
