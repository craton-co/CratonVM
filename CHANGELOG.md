# Changelog

All notable changes to CratonVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

A cross-crate review-driven fix orchestrator landed 50+ commits across security, soundness, correctness, and OSS-distribution hygiene. Highlights:

#### Security
- JEP-290 `ObjectInputFilter` honored with `maxdepth`/`maxrefs`/`maxbytes`/`maxarray` caps (`native-builtins/src/object_input_filter.rs`).
- JAR signer chain verified against the JCE/JDK trust store before classes load (`classloading/src/jar_signer.rs`).
- Panama / FFI host calls gated behind `--enable-native-access`; unauthorized callers throw `IllegalCallerException` (`native-builtins/src/panama_*.rs`).
- `ProcessBuilder.start` and Panama host calls now consult `SecurityManager.checkExec` (`native-builtins/src/process.rs`).
- Test-only TLS certs and keys moved behind `cfg(test)` so they cannot ship in release artifacts (`native-builtins/src/tls_test_certs.rs`).
- Outbound network calls (HTTP/Socket/URL) run through an SSRF policy hook with a per-connect timeout (`native-io/src/net.rs`).
- `RandomAccessFile`, `WatchService`, and `ProcessBuilder` now route paths through `validate_path` before opening (`native-io/src/*`, `native-builtins/src/process.rs`).
- New libfuzzer targets cover classfile reader, JImage parser, PKCS#12 keystore, and JAR signer (`fuzz/fuzz_targets/`).
- `vm` identity-validates resolution-cache keys so a forged class identity cannot poison lookups (`vm/src/runtime/resolution_cache.rs`).
- `vm` verifier-skip path is now gated on the bootstrap classloader identity, not just the loader pointer (`vm/src/runtime/verifier_gate.rs`).

#### Soundness
- SATB pre-barrier wired at remaining `aastore`/`putfield` sites plus a real stop-the-world for `newarray` (`vm/src/runtime/interpreter.rs`, `jit/src/runtime_helpers.rs`).
- `gc` mutating heap entry points now require a `StopTheWorldToken` witness (`gc/src/lib.rs`).
- Async-signal-safe SIGSEGV handler installed on Unix (no allocations, no locks) (`vm/src/runtime/signals.rs`).
- AArch64 icache flush on Linux and FreeBSD after JIT code emission (`jit/src/aarch64.rs`).
- `vm` hot locks reordered through `OrderedMutex` matching `docs/lock-order.md` (`vm/src/lock_order.rs`).
- JIT switch-target offsets are now overflow-checked; `try_patch` replaces panicking `patch_i32`/`patch_byte` (`jit/src/buffer.rs`).
- `reader::ByteView::try_new` returns `Result` on overflow / misalignment instead of UB (`reader/src/byte_view.rs`).
- `gc` bitmap clears use `AcqRel` ordering; the `SATB` write barrier is now part of the trait surface (`gc/src/g1.rs`).
- `jfr::SpscEventRing::Drop` performs a bounded shutdown and releases pending payloads (`jfr/src/ring.rs`).

#### Correctness
- JFR field emit validates variant against declared type per event (`jfr/src/event.rs`).
- Native collections rekey GC overlays on `identity_hash_code` so post-GC pointer remap keeps maps consistent (`native-collections/src/*`).
- Blocking queue park / notify discipline cleaned up with read-locks instead of unsynchronized shared state (`native-collections/src/blocking_queue.rs`).
- CUDA H2D → kernel → D2H now sequenced on the same stream; previous code raced (`cuda-bridge/src/stream.rs`).
- `jit-api` exposes a `validate()` loop, `repr(C)` golden offsets, and a fixed `NUM_FIELDS` constant for ABI lock-in (`jit-api/src/lib.rs`).
- `types::CompactValue::update_object_ptr` returns `Result`, and `as_long_unchecked` documents its lazy-decode invariant (`types/src/compact_value.rs`).
- `native-builtins` `--enable-native-access` audit; Panama host calls check the caller module against the allow-list.
- `reader` attribute shape validation propagates the signature depth-guard "sticky" flag (`reader/src/attribute.rs`).
- `native-builtins` JCA crypto routes AES / AES-GCM through `aes` / `aes-gcm` RustCrypto (constant-time).
- `vm-cli` rebuilt for HotSpot `-Xmx` / `-XX` parsing, `--nojit`, `String[] args` (`vm-cli/src/main.rs`).
- `native-api::allocate` no longer leaks on the error path; `init_level` is monotonic; `tcp_available` no longer clobbers state (`native-api/src/lib.rs`).

#### OSS / Distribution
- `vm-cli` produces the `cratonvm` binary by default; the `java[.exe]` alias is opt-in via `--features java-bin-alias` so `cargo install` does not shadow a real JDK (`vm-cli/Cargo.toml`).
- Added `SUPPORT.md`, `GOVERNANCE.md`, `MAINTAINERS.md`, `THIRD-PARTY-NOTICES.md`, and a GitHub issue-template config (top-level + `.github/`).
- SPDX `Apache-2.0` headers on every Rust source file across the workspace.
- MSRV bumped to 1.77 and synchronized across `README.md`, `BUILD_GUIDE.md`, `CONTRIBUTING.md`, and `docs/INSTALL.md`.
- Workspace version raised to `0.3.0`; every inter-crate `path = "../<crate>"` declaration now carries `version = "0.3.0"` so `cargo publish --dry-run` accepts the manifest.
- Per-crate `README.md` added for crates.io rendering (all 18 workspace members, incl. `fuzz`).
- `fuzz/` is now a workspace member (still nightly-only; `publish = false`).
- Workspace crate-count references aligned to 18 workspace members (17 plus `fuzz`) in `README.md`, `ARCHITECTURE.md`, `BUILD_GUIDE.md`.
- CI parked workflows reactivated with `clippy -D warnings` as a hard gate (`.github/workflows/ci.yml`).

#### Known follow-ups
- Re-enable JIT loop unrolling — previous byte-copy unrolling produced corrupt native code and was disabled (`jit/src/x64/unroll.rs`).
- Real-JDK boot via `java.base` JMOD remains opt-in; synthetic stubs cover the default path.
- Concurrent GC marking is still serialized under STW; G1 / ZGC remain experimental.

## [0.3.0] - 2026-05-24

### Added
- Real cryptographic signature verification in `x509_manager::validate_chain` for RSA-SHA256 (PKCS#1 v1.5) and ECDSA-with-SHA256 over P-256, replacing the previous structural-only "signature present" check. DSA-with-SHA1, RSA-PSS, and Ed25519 now report `TrustError::NotImplemented { oid }` so callers can choose to delegate to JCE.
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
- MSRV bumped to 1.77 (was 1.75).
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
- JIT loop unrolling re-enabled behind a byte-copy-safety predicate. The byte-copy unroller is only correct when every opcode in the body is position-independent (or one of the rel32 patch flavours the duplicator now handles, namely `forward_patches`, `bounds_check_stubs`, and `null_check_store_stubs`). Bodies containing field/static accesses, invokes, allocations, throws, instanceof/checkcast, monitor ops, switches, or any other helper-call opcode are skipped. Set `CRATONVM_UNROLL_UNSAFE_BODIES=1` to re-enter the legacy unguarded path for bisection.
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
- JIT loop unrolling is now gated on a byte-copy safety predicate (above); pure-arithmetic and array-index-store kernels are unrolled, but loops with field accesses or invokes still execute unrolled-by-1 until the duplicator learns to clone deopt/exception/MIC/PIC stubs.
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
- CI pipeline with cross-platform testing (coverage and Miri jobs scaffolded but planned, not yet enabled)

[Unreleased]: https://github.com/craton-co/cratonvm/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/craton-co/cratonvm/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/craton-co/cratonvm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/craton-co/cratonvm/releases/tag/v0.1.0
