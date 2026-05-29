# CratonVM

### The Java Virtual Machine, reimagined in Rust.

**A modern JVM with a custom x86-64 JIT, memory-safe by construction, and built for the GPU era — zero C/C++ legacy, one self-contained native binary.**

*Version 0.3.0 · Java SE 8–25 language coverage · Brought to you by Craton Software Company*

---

## Why CratonVM

For twenty-five years, the Java runtime has rested on millions of lines of C and C++. CratonVM starts over — clean — on a foundation of **Rust**. The result is a Java Virtual Machine that is fast, lean, and safe by design, with the ambition to run modern Java workloads on modern hardware, including the GPU.

CratonVM is **cutting-edge, research-grade software**: early, fast-moving, and unafraid to rethink the runtime from first principles.

- **Memory-safe by construction.** Built end-to-end in Rust, CratonVM inherits Rust's ownership and bounds-checking guarantees. Entire categories of classic VM vulnerabilities — buffer overruns, use-after-free, data races — are designed out at the language level rather than patched in after the fact.
- **No C/C++ legacy.** Not a fork, not a wrapper, not a binding layer. A ground-up runtime with a clean, auditable codebase and no decades-old native baggage.
- **One self-contained binary.** No JDK install, no `JAVA_HOME`, no `rt.jar`. CratonVM ships its own implementations of the Java standard library and runs as a single native executable.
- **GPU-accelerated compute (emerging).** An opt-in pipeline lowers Java bytecode toward the GPU via NVIDIA CUDA — bringing data-parallel acceleration into the JVM itself.
- **Broad Java language coverage.** Targets Java SE 8 through 25: lambdas and streams, records and sealed classes, pattern matching, virtual threads, scoped values, and more.
- **Performance that competes.** A custom x86-64 JIT delivers runtimes within roughly 1.5x of JDK 25's C2 compiler on the project's benchmarks.

---

## Key Capabilities

- **Custom x86-64 JIT compiler** — compiles hot methods straight to native machine code, with loop-invariant code motion, bounds-check elimination, AVX2 SIMD vectorization, on-stack replacement, and graph-coloring register allocation. AArch64 support is in progress.
- **Memory-safe Rust runtime** — the whole VM, interpreter, GC, and JIT are written in safe-by-default Rust.
- **Generational garbage collection** — young/old generations with write barriers and a card table, plus additional collector backends.
- **True multi-threading** — monitors, locks, and barriers, with `java.util.concurrent` support.
- **Rich standard library** — 3,100+ native method implementations across `java.lang`, `java.util`, `java.io`, `java.time`, `java.util.stream`, `java.util.concurrent`, and more — no `rt.jar` required.
- **Lambdas and `invokedynamic`** — modern functional Java runs out of the box.
- **GPU offload (opt-in, emerging)** — bytecode-to-GPU lowering via NVIDIA CUDA, off by default so the standard build stays a lean CPU-only JVM.
- **Security-minded by design** — built-in bytecode verification and a memory-safe core.

---

## Performance Highlight

On the project's QuickBench suite, CratonVM's x86-64 JIT runs **within about 1.5x of JDK 25's HotSpot C2** — and on recursive Fibonacci, within **1.31x**.

| Benchmark | JDK 25 C2 | CratonVM | Ratio |
|-----------|-----------|----------|-------|
| Arithmetic (300M) | 889 ms | 1,676 ms | 1.89x |
| Fibonacci(42) | 1,876 ms | 2,457 ms | 1.31x |
| Sieve (100K×500) | 324 ms | 510 ms | 1.57x |
| Matrix 500×500 | 351 ms | 518 ms | 1.48x |
| **QuickBench total** | **3,440 ms** | **5,161 ms** | **1.50x** |

*Measured 2026-03-31 on Windows 11, JDK 25.0.1 LTS. Competitive — not a claim to beat the JDK, but to stand close to it, in a runtime that is years younger and built on a safer foundation.*

Backed by **6,000+ passing tests** and a clean, warning-free codebase.

---

## Who It's For

- **Systems and language researchers** exploring what a memory-safe, from-scratch JVM can do.
- **Performance engineers** who want a transparent, hackable JIT instead of an opaque black box.
- **GPU and HPC explorers** curious about pushing JVM compute onto the GPU via CUDA.
- **Security-conscious teams** evaluating a runtime engineered to eliminate whole classes of native-code vulnerabilities by construction.
- **Rust and JVM enthusiasts** who want to see the two worlds meet.

CratonVM is experimental and not yet production-ready — it is a fast-moving platform for the curious and the ambitious.

---

## Get Started

Run your first class in seconds — no JDK runtime required:

```bash
cargo run --release -p cratonvm-cli -- --classpath . HelloWorld
```

- **README** — features, command-line reference, and the full capability matrix: [`README.md`](../README.md)
- **Install guide** — binary installation and getting started: [`docs/INSTALL.md`](INSTALL.md)
- **GPU offload** — the opt-in CUDA acceleration reference: [`docs/gpu/README.md`](gpu/README.md)

**CratonVM — modern Java, memory-safe, GPU-ready. Built in Rust by Craton Software Company.**

*Java and OpenJDK are trademarks of Oracle and/or its affiliates. NVIDIA and CUDA are trademarks of NVIDIA Corporation. These references are used for compatibility identification only.*
