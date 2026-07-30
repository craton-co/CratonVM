# CratonVM

### Java on the GPU. Plain static kernels, one `--gpu` flag, measurably faster.

**CratonVM is a Java Virtual Machine that moves eligible Java compute kernels
onto the GPU — and beats the fastest Java GPU framework on the kernels both
can run. Built entirely in Rust: one self-contained binary.**

*Version 0.3.0 · Java SE 8–25 · by Craton Software Company*

---

## Your Java. On the GPU. Automatically — inside a documented envelope.

Every other path to GPU acceleration in Java asks you to rewrite your kernels:
special annotations, task graphs, new APIs, off-heap array types, new build
steps. CratonVM asks for a build flag and a run flag. Build with
`--features gpu-driver`, run with `--gpu`, and the methods that match a
deliberately narrow, documented shape — pure static methods over primitive
arrays in counted loops — move to the GPU automatically: the same `.class`
files, the same `main()`, the same results, bit for bit. Everything outside
that subset keeps running on the CPU, unchanged. The envelope is written
down, not guessed at ([`docs/gpu/README.md`](gpu/README.md)).

And it isn't just easier. **It's faster.** On division-dominated compute —
the workloads where CPUs have no vectorized answer — CratonVM's GPU
pipeline outruns [TornadoVM](https://github.com/beehive-lab/TornadoVM), the
leading Java GPU framework:

| Compute kernel (16.7M elements) | CPU (HotSpot C2) | TornadoVM GPU | **CratonVM GPU** |
|---------------------------------|------------------|----------------|-------------------|
| Integer division chain          | 1,910 ms         | 28 ms          | **9 ms — 3.1x faster than TornadoVM, 212x faster than CPU** |
| Double-precision division chain | 1,508 ms         | 129 ms         | **91 ms — 1.4x faster than TornadoVM, 16.6x faster than CPU** |

Three things make that second row special:

1. **No kernel rewrite.** TornadoVM needed `@Parallel` annotations, its
   TaskGraph API, and off-heap array types to get its number. CratonVM ran
   plain Java `int[]`/`double[]` static methods, unmodified, behind `--gpu`.
2. **Bit-exact results.** CratonVM's GPU division matches HotSpot's answer
   to the last bit, IEEE-754 exact. TornadoVM's doesn't.
3. **It handles what others can't.** On a reduction kernel TornadoVM's own
   backend throws `unimplemented`, CratonVM just runs it.

Where the CPU is genuinely better — multiply-heavy kernels that AVX2
vectorizes beautifully — CratonVM leaves the work on the CPU. Automatic
means honest: the right processor for each job, and no offload at all for
shapes the analyzer doesn't accept.

These are point-in-time measurements on one RTX 2060, checksum-verified
against HotSpot. There is no self-hosted GPU hardware CI re-running them on
every change, so treat them as a published result rather than an enforced
budget.

---

## A JVM rebuilt for this decade

For twenty-five years the Java runtime has rested on millions of lines of
C and C++. CratonVM starts over in **Rust**:

- **Rust all the way down.** The interpreter, the JIT compiler, and the
  garbage collector are written in safe Rust wherever they can be, which
  removes many of the ambient memory hazards a C++ runtime lives with. The
  VM, JIT, GC, FFI, I/O, AWT, CUDA, and JFR still contain reviewed and
  still-being-audited `unsafe` regions — see
  [SECURITY.md](../SECURITY.md).
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
  signal processing — whose hot kernels fit the supported shape and who want
  acceleration without rewriting them.
- **Performance engineers** who want a transparent, hackable runtime
  instead of an opaque black box.
- **Security-conscious organizations** evaluating a runtime whose memory
  handling is Rust-first, with its remaining `unsafe` surface documented
  rather than assumed away.
- **Researchers and enthusiasts** who want to see what a from-scratch,
  Rust-native JVM can do.

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
