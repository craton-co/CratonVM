# CratonVM

### The Java Virtual Machine, reimagined in Rust.

**A modern JVM with a custom x86-64 JIT, memory-safe by construction, and built
for the GPU era: zero C/C++ legacy, one self-contained native binary.**

*Version 0.3.0 - Java SE 8-25 language coverage - Brought to you by Craton
Software Company*

---

## Why CratonVM

For twenty-five years, the Java runtime has rested on millions of lines of C
and C++. CratonVM starts over on a foundation of **Rust**. The result is a Java
Virtual Machine that is fast-moving, lean, and safe by design, with the ambition
to run modern Java workloads on modern hardware, including the GPU.

CratonVM is **cutting-edge, research-grade software**: early, fast-moving, and
unafraid to rethink the runtime from first principles.

- **Memory-safe by construction.** Built end-to-end in Rust, CratonVM inherits
  Rust's ownership and bounds-checking guarantees. Entire categories of classic
  VM vulnerabilities are designed out at the language level.
- **No C/C++ legacy.** Not a fork, not a wrapper, not a binding layer. A
  ground-up runtime with a clean, auditable codebase.
- **One self-contained binary.** No JDK install, no `JAVA_HOME`, no `rt.jar`.
  CratonVM ships its own implementations of the Java standard library and runs
  as a single native executable.
- **GPU-accelerated compute (emerging).** An opt-in pipeline lowers Java
  bytecode toward the GPU via NVIDIA CUDA.
- **Broad Java language coverage.** Targets Java SE 8 through 25: lambdas and
  streams, records and sealed classes, pattern matching, virtual threads, scoped
  values, and more.
- **Performance work in progress.** Current measurements show OSR-enabled
  Arithmetic, Sieve, and Matrix within roughly 1.3x-1.6x of JDK 25 C2, while
  recursive Fibonacci and allocation-heavy Binary Trees remain major gaps.

---

## Key Capabilities

- **Custom x86-64 JIT compiler** - compiles hot methods to native machine code,
  with loop-invariant code motion, bounds-check elimination, AVX2 SIMD
  vectorization, on-stack replacement, and graph-coloring register allocation.
  AArch64 support is in progress.
- **Memory-safe Rust runtime** - the VM, interpreter, GC, and JIT are written in
  safe-by-default Rust.
- **Generational garbage collection** - young/old generations with write
  barriers and a card table, plus additional collector backends.
- **True multi-threading** - monitors, locks, and barriers, with
  `java.util.concurrent` support.
- **Rich standard library** - thousands of native method implementations across
  `java.lang`, `java.util`, `java.io`, `java.time`, `java.util.stream`,
  `java.util.concurrent`, and more.
- **Lambdas and `invokedynamic`** - modern functional Java runs out of the box.
- **GPU offload (opt-in, emerging)** - bytecode-to-GPU lowering via NVIDIA
  CUDA, off by default so the standard build stays a lean CPU-only JVM.
- **Security-minded by design** - built-in bytecode verification and a
  memory-safe core.

---

## Performance Snapshot

Single-run snapshot measured 2026-07-02 on Windows 11, JDK 25.0.1 LTS, and
CratonVM code `b80c50b5`.

| Benchmark | JDK 25 C2 | CratonVM default | Default ratio | CratonVM OSR, threshold=1 | OSR ratio |
|-----------|-----------|------------------|---------------|---------------------------|-----------|
| Arithmetic (300M) | 991 ms | 73,820 ms | 74.5x | 1,534 ms | 1.55x |
| Fibonacci(42) | 2,071 ms | 28,969 ms | 14.0x | 28,525 ms | 13.8x |
| Sieve (100K x 500) | 358 ms | 31,665 ms | 88.4x | 466 ms | 1.30x |
| Matrix 500x500 | 336 ms | 39,078 ms | 116.3x | 452 ms | 1.35x |
| **QuickBench total** | **3,756 ms** | **173,532 ms** | **46.2x** | **30,977 ms** | **8.25x** |

The OSR column sets `CRATONVM_JIT_OSR=1 CRATONVM_JIT_THRESHOLD=1`. Default
launcher settings leave these one-shot hot loops mostly interpreted.

Backed by a layered Rust/Java test strategy and HotSpot-differential tooling.

---

## Who It's For

- **Systems and language researchers** exploring what a memory-safe,
  from-scratch JVM can do.
- **Performance engineers** who want a transparent, hackable JIT instead of an
  opaque black box.
- **GPU and HPC explorers** curious about pushing JVM compute onto the GPU via
  CUDA.
- **Security-conscious teams** evaluating a runtime engineered to eliminate
  whole classes of native-code vulnerabilities by construction.
- **Rust and JVM enthusiasts** who want to see the two worlds meet.

CratonVM is experimental and not yet production-ready; it is a fast-moving
platform for the curious and the ambitious.

---

## Get Started

Run your first class in seconds:

```bash
cargo run --release -p cratonvm-cli -- --classpath . HelloWorld
```

- **README** - features, command-line reference, and the full capability matrix:
  [`README.md`](../README.md)
- **Install guide** - binary installation and getting started:
  [`docs/INSTALL.md`](INSTALL.md)
- **GPU offload** - the opt-in CUDA acceleration reference:
  [`docs/gpu/README.md`](gpu/README.md)

**CratonVM: modern Java, memory-safe, GPU-ready. Built in Rust by Craton
Software Company.**

*Java and OpenJDK are trademarks of Oracle and/or its affiliates. NVIDIA and
CUDA are trademarks of NVIDIA Corporation. These references are used for
compatibility identification only.*
