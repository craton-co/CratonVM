# CratonVM

### Java on the GPU. No annotations. No rewrites. Just faster.

**CratonVM is the Java Virtual Machine that runs your existing Java code on
the GPU — and beats the fastest Java GPU framework doing it. Built entirely
in Rust: one self-contained binary, memory-safe from the first line.**

*Version 0.3.0 · Java SE 8–25 · by Craton Software Company*

---

## Your Java. On the GPU. Automatically.

Every other path to GPU acceleration in Java asks you to rewrite your code:
special annotations, task graphs, new APIs, new build steps. CratonVM asks
for nothing. Point it at your compiled classes and eligible computations move
to the GPU transparently — the same `.class` files, the same `main()`, the
same results, bit for bit.

And it isn't just easier. **It's faster.** On division-dominated compute —
the workloads where CPUs have no vectorized answer — CratonVM's GPU
pipeline outruns [TornadoVM](https://github.com/beehive-lab/TornadoVM), the
leading Java GPU framework:

| Compute kernel (16.7M elements) | CPU (HotSpot C2) | TornadoVM GPU | **CratonVM GPU** |
|---------------------------------|------------------|----------------|-------------------|
| Integer division chain          | 1,910 ms         | 28 ms          | **9 ms — 3.1x faster than TornadoVM, 212x faster than CPU** |
| Double-precision division chain | 1,508 ms         | 129 ms         | **91 ms — 1.4x faster than TornadoVM, 16.6x faster than CPU** |

Three things make that second row special:

1. **No code changes.** TornadoVM needed `@Parallel` annotations and its
   TaskGraph API to get its number. CratonVM ran plain Java.
2. **Bit-exact results.** CratonVM's GPU division matches HotSpot's answer
   to the last bit, IEEE-754 exact. TornadoVM's doesn't.
3. **It handles what others can't.** On a reduction kernel TornadoVM's own
   backend throws `unimplemented`, CratonVM just runs it.

Where the CPU is genuinely better — multiply-heavy kernels that AVX2
vectorizes beautifully — CratonVM leaves the work on the CPU. Transparent
means honest: the right processor for each job.

---

## A JVM rebuilt for this decade

For twenty-five years the Java runtime has rested on millions of lines of
C and C++. CratonVM starts over in **Rust**:

- **Memory-safe by construction.** The interpreter, the JIT compiler, and
  the garbage collector inherit Rust's ownership and bounds guarantees.
  Whole categories of classic VM vulnerabilities are designed out before
  the first line of your code runs.
- **One self-contained binary.** No JDK install, no `JAVA_HOME`, no
  `rt.jar`. Download, run. When a JDK *is* present, CratonVM can boot from
  its real class library for maximum fidelity.
- **A real optimizing JIT.** Tiered compilation, on-stack replacement,
  bounds-check elimination, AVX2 vectorization — compiling your hot code to
  native x86-64, then getting out of the way.
- **Modern Java, covered.** Lambdas and streams, records and sealed
  classes, pattern matching, virtual threads, scoped values — Java 8
  through Java 25.

---

## Runs what you actually build

This isn't a toy that runs Fibonacci. CratonVM boots the frameworks your
applications are made of, and passes their test suites:

- **Spring & Spring Boot** — the world's most-used Java application stack
- **Tomcat** — servlets, NIO, WebSockets, HTTP/2
- **Hibernate** — the standard for Java persistence
- **H2** — a full embedded SQL database

**Next up: the reactive stack.** Netty, Quarkus, and Hibernate Reactive are
on the roadmap, together with real database connectivity — bringing
cloud-native, event-driven Java to CratonVM.

---

## Who it's for

- **Teams with GPU-hungry Java compute** — simulation, pricing, scoring,
  signal processing — who want acceleration without a rewrite.
- **Performance engineers** who want a transparent, hackable runtime
  instead of an opaque black box.
- **Security-conscious organizations** evaluating a runtime that eliminates
  native-memory vulnerability classes by construction.
- **Researchers and enthusiasts** who want to see what a from-scratch,
  memory-safe JVM can do.

CratonVM is fast-moving, research-grade software — early, ambitious, and
improving measurably every week. Every published benchmark ships with
matching checksums and reproducible methodology.

---

## Get started in sixty seconds

```bash
cargo build --release -p cratonvm-cli
./target/release/cratonvm -cp . HelloWorld
```

- **Developer guide & capability matrix:** [`README.md`](../README.md)
- **Benchmarks & methodology:** [`BENCHMARK.md`](../BENCHMARK.md)
- **GPU offload reference:** [`docs/gpu/README.md`](gpu/README.md)
- **Install guide:** [`docs/INSTALL.md`](INSTALL.md)

**CratonVM: your Java, on every processor you own. Built in Rust by Craton
Software Company.**

*Java and OpenJDK are trademarks of Oracle and/or its affiliates. NVIDIA and
CUDA are trademarks of NVIDIA Corporation. TornadoVM is a project of the
Beehive Lab, University of Manchester. These references are used for
compatibility and comparison identification only.*
