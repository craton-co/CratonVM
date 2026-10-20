# Introduction

**CratonVM is a Java Virtual Machine written entirely in Rust, with a custom x86-64 JIT compiler.**

It runs Java bytecode compiled for Java SE 8 through 25. It can boot against a
real JDK when one is present on the host, and run completely standalone — with
no JDK, no `JAVA_HOME`, and no `rt.jar` — when one is not.

```bash
cratonvm --classpath . HelloWorld
```

This manual is the single, navigable home for everything you need to install,
run, tune, secure, embed, and contribute to CratonVM. It is written primarily
for people **running Java programs** on CratonVM, but also covers embedding the
VM in another application and hacking on its internals.

## What CratonVM is

- A **bytecode interpreter** with 140+ fast-path opcodes.
- A **custom x86-64 JIT compiler** with register allocation, loop-invariant code
  motion, bounds-check elimination, AVX2 SIMD, on-stack replacement, and precise
  JIT stack maps. (An AArch64 backend exists but is partial.)
- A **generational garbage collector** (young/old generations, write barriers,
  card table) with an opt-in region-based G1 collector.
- **Multi-threading** with monitors, locks, barriers, and virtual threads.
- **Lambdas and `invokedynamic`** via `LambdaMetafactory`.
- Thousands of **native standard-library method implementations** spanning
  `java.lang`, `java.util`, `java.io`/`nio`, `java.time`,
  `java.util.concurrent`, JCA crypto, and more.
- A **JNI Invocation API** and a stable **C-ABI embedding library**
  (`libcratonvm`) plus a Rust facade (`cratonvm-embed`).
- Opt-in **security hardening** (filesystem confinement, egress/SSRF policy,
  decompression-bomb caps) and **container/cgroup awareness**.
- Opt-in **GPU offload**: Java bytecode → NVIDIA PTX lowering for CUDA.

It is roughly **1,350,000 lines of Rust** across 22 workspace crates, with a
large Rust/Java test corpus and HotSpot-differential regression tooling.
(Roughly 1,350,000 lines in the `.rs` files under the 22 members
listed in the root `Cargo.toml`, excluding `target/`, excluding the non-member
`fuzz/` workspace, and excluding vendored code under any `vendor/` directory.)

## Status & expectations

> CratonVM is an **experimental research JVM**. It is **not certified** and
> **must not be used to run untrusted Java code** in security-sensitive
> environments. It has **not** undergone a formal security audit, and parts of
> its cryptographic stack are best-effort.

CratonVM targets broad Java 8–25 language coverage and runs a wide range of real
applications and libraries. It is, however, research-grade software: some
standard-library corners are unimplemented or partial, and a handful of
correctness and performance items are tracked openly. Where a feature is
incomplete, this manual says so plainly — see [Known
Limitations](java-support/limitations.md), the [Security
Overview](security/overview.md), and [Cryptography](security/cryptography.md)
for the honest picture.

## How this manual is organized

| Part | Read it for |
|------|-------------|
| **[Getting Started](getting-started/installation.md)** | Install CratonVM and run your first program. |
| **[User Guide](user-guide/running-programs.md)** | Day-to-day operation: classpaths, flags, heap, GC, JIT, modules, containers, debugging. |
| **[Java Platform Support](java-support/language-features.md)** | What language features, Java versions, and standard-library classes work. |
| **[Security](security/overview.md)** | Threat model, sandboxing knobs, and cryptographic status. |
| **[Performance](performance/benchmarks.md)** | Benchmarks against HotSpot, the JIT story, and profiling. |
| **[GPU Offload](gpu/overview.md)** | The opt-in CUDA offload feature. |
| **[Embedding](embedding/overview.md)** | Drive CratonVM from C, Rust, or any FFI host. |
| **[Architecture & Internals](internals/architecture.md)** | How the VM is built, subsystem by subsystem. |
| **[Contributing](contributing/building.md)** | Build, test, and contribute changes. |
| **[Reference](reference/environment-variables.md)** | Exhaustive flag/env tables, platform matrix, FAQ, glossary. |

## A 30-second tour

```bash
# Compile a Java source file with a standard JDK compiler.
javac HelloWorld.java

# Run it on CratonVM.
cratonvm --classpath . HelloWorld

# Run a JAR (main class comes from the manifest).
cratonvm --jar app.jar arg1 arg2

# Give a larger program more heap, and watch GC activity.
cratonvm --Xmx 2g --verbose:gc --classpath . BigProgram

# Disable the JIT to isolate an interpreter-vs-JIT question.
cratonvm --nojit --classpath . MyProgram
```

If you are brand new, start with [Installation](getting-started/installation.md)
and [Your First Program](getting-started/first-program.md).

## License

CratonVM is licensed under the **Apache License, Version 2.0**. See the
`LICENSE` and `TRADEMARKS.md` files in the repository root.
