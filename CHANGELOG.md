# Changelog

All notable changes to CratonVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
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
