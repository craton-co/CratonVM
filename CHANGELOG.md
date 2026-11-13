# Changelog

All notable changes to CratonVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- ZGC's concurrent relocation is opt-in again (`CRATONVM_ZGC_CONCURRENT_RELOCATE=1`); concurrent marking stays default ON. Armed, the colored load barrier sent reference copies, element reads and intrinsic natives through runtime helpers: the JDK string/regex rows ran 1.5-4x more instructions per operation, and a fragmenting 200-600 MB live set (`tools/probes/ZgcRelocHiccup.java`) lost 30-40% mutator throughput and used 50% more CPU, while its longest pause stayed in the 200-600 ms range (HotSpot ZGC: 2-5 ms at 200 MB).
- ZGC (the default collector) now marks concurrently and relocates concurrently by default. `CRATONVM_ZGC_CONCURRENT=0` restores the stop-the-world cycle; `CRATONVM_ZGC_CONCURRENT_RELOCATE=0` keeps concurrent marking with the in-pause slide. A generational heap, a narrow-oop heap, a heap wider than 4 TiB and a second VM in one process keep the in-pause slide. Fixes made on the way: the director's rate window starved once cycles came less than 100 ms apart, the concurrent rate rule reserved HotSpot's 100 ms tick on a byte-polled director (`CRATONVM_ZGC_DIRECTOR_BYTE_POLLED_INTERVAL`), and a fresh receiver's colored reference store skipped the SATB gate and could leave a good-colored word naming a relocated object.

### Added
- GPU: the HIP (AMD) renderer now lowers all 154 golden kernels (was 34). Emitted PTX lines are lifted to the kernel IR where they are born, kept only when they round-trip byte for byte, so the PTX is unchanged; the warp-reduction fold is expanded for the target's wavefront width; Java semantics (wrapping integers, `MIN_VALUE / -1`, saturating float-to-int, shift clamping) are pinned in the HIP renderer and compile options. First run on an AMD Instinct MI300X: hiprtc compiles every rendered kernel. `tools/gpu/amd-remote/` gains Linux runners for the benchmark rows.
- Continuous differential testing enhancements and extended Java standard library test coverage.
- Ongoing performance tuning for enterprise workloads and cloud deployment environments.

## [0.3.0]

### Added
- **Tiered JIT Compiler**: Multi-tier compilation engine (Interpreter → C1 baseline → C2 optimizing compiler) featuring:
  - High-level Intermediate Representation (IR) with SSA form.
  - Intermediate representation optimizer with loop-invariant code motion (LICM), global value numbering (GVN), and bounds-check elimination (BCE).
  - Profile-guided optimization (PGO), inline caching, and speculative devirtualization.
  - On-Stack Replacement (OSR) for long-running interpreter loops.
  - Escape analysis and scalar replacement of aggregates.
  - Vectorization and SIMD instruction selection (AVX2).
  - Precise stack maps and comprehensive deoptimization mechanisms.
  - Experimental AArch64 JIT compilation backend.
- **Modern Garbage Collectors**:
  - Production region-based G1 garbage collector (`-XX:+UseG1GC`) with concurrent marking and incremental evacuation.
  - Low-latency concurrent ZGC (`-XX:+UseZGC`) as the default collector, featuring colored pointers, load barriers, concurrent marking, sliding compaction, and optional generational mode.
- **Compatible / Synthetic Optimization Layer**:
  - Optimized compatibility mode (`--compatible`) engineered specifically as a high-performance acceleration layer for ultra-fast cold-start startup and specialized throughput execution paths.
  - Native standard library acceleration bridges complementing authentic OpenJDK class libraries.
- **Observability and Diagnostic Infrastructure**:
  - Java Flight Recorder (JFR) event recording and profiling.
  - Differential testing engine (`difftest`) comparing bytecode execution byte-for-byte against HotSpot.

## [0.2.0]

### Added
- **GPU Acceleration Subsystem**:
  - Native GPU offload engine supporting both NVIDIA PTX/CUDA (via CUDA Driver API) and AMD HIP/ROCm architectures.
  - Bytecode kernel analyzer and intermediate representation lowering pure Java computational loops into high-performance GPU kernels.
  - Automatic offload for eligible parallel array operations and math-intensive loops without code changes.
  - Parallel array and reduction execution with GPU warp-level primitives.
  - Zero-copy device memory management, host-device streaming, and asynchronous execution queues.

## [0.1.0]

### Added
- **Foundational JVM Runtime**:
  - Core JVM execution engine implemented purely in Rust.
  - Direct execution and authoritative loading of authentic OpenJDK class libraries (`java.base` and core modules).
  - High-fidelity JVM interpreter executing standard JVM bytecode specifications.
  - Comprehensive class file reader, validator, and class loading subsystem (`reader`, `classloading`).
  - Thread model, monitor synchronisation, object layout management, and standard exception handling.
  - Full support for running real-world enterprise frameworks out of the box, including Spring Boot, Tomcat, Netty, and Hibernate.
  - Embedding interfaces including C-ABI library (`libcratonvm`) and native invocation APIs.
