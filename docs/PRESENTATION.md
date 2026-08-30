# CratonVM

### The Java Runtime for AI Inference and Big Data at Scale

**CratonVM turns ordinary Java into GPU-accelerated AI and data
infrastructure — automatically. No annotations. No new APIs. No rewrites.
Just your Java, running faster than the leading alternatives, on a
memory-safe Rust foundation built for this decade.**

*Version 0.3.0 · Java SE 8–25 · 400,000+ lines of Rust · 19,000+ automated
tests · by Craton Software Company*

---

## Built for the AI era

Every modern AI workload lives or dies on one operation: matrix
multiplication. CratonVM ships GEMM — the core computation behind every
neural-network layer, every transformer attention block, every LLM inference
pass — as a built-in, GPU-accelerated kernel, in both full precision (fp32)
and the half precision (fp16) that makes modern inference fast.

And it doesn't stop at matmul. CratonVM's automatic GPU pipeline recognizes
eligible Java compute — division-heavy math, reductions, high-resolution
rendering, and more — and moves it to the GPU with nothing more than a build
flag and a run flag. The same `.class` files. The same `main()`. The same
results HotSpot would give you — bit for bit — just dramatically faster.

**Compare that to the alternative.** [TornadoVM](https://github.com/beehive-lab/TornadoVM),
the best-known Java GPU framework, asks you to annotate your kernels,
restructure them around a TaskGraph API, and adopt new off-heap array types
before a single line moves to the GPU. CratonVM asks for none of that — and
still comes out ahead:

| Compute kernel (16.7M+ elements)          | CPU (HotSpot C2) | TornadoVM GPU | **CratonVM GPU** | vs HotSpot     | vs TornadoVM   |
|--------------------------------------------|------------------|---------------|-------------------|-----------------|-----------------|
| Integer division chain                     | 2,146 ms         | 26 ms         | **11 ms**         | **195x faster** | **2.4x faster** |
| Double-precision division chain            | 1,780 ms         | 128 ms        | **95 ms**         | **18.7x faster**| **1.3x faster** |
| 128 multiply-adds/element                  | 1,300 ms         | 27 ms         | **8 ms**          | **163x faster** | **3.4x faster** |
| Dot-product reduction (int·int → long)     | 1,172 ms         | unimplemented | **12 ms**         | **98x faster**  | —               |
| Ray tracer kernel (33.2M pixels)           | 837 ms           | 24.3 ms       | **12.3 ms**       | **68x faster**  | **2.0x faster** |

Every number above is checksum-verified bit-for-bit against HotSpot,
including the GPU results — no rounding, no approximation, no "close enough."
And that dot-product row isn't a gap in our table: TornadoVM's own engine
throws `unimplemented` on that shape. CratonVM just runs it.

---

## Built for Big Data — at the scale that actually matters

AI needs matrix multiplication. Big data needs throughput, uptime, and heaps
that don't stall your service under load. CratonVM was engineered for both:

- **A collector built for large heaps and low pauses.** CratonVM's default
  garbage collector is a modern, compaction-capable design sized for the
  memory profile real data services carry today — not a decades-old
  collector stretched to cover a new decade of workloads.
- **Runs the frameworks your data stack is already built on.** Spring &
  Spring Boot, Tomcat, and Hibernate boot, run, and pass their real test
  suites on CratonVM today, backed by a full embedded H2 SQL database.
  Netty, Quarkus, and Hibernate Reactive are next, bringing the full
  reactive, event-driven stack online.
- **One self-contained binary.** No JDK to install, no `rt.jar`, no bloated
  base image. Download it and run it — in a container, at the edge, in a
  pipeline, wherever your data actually lives.
- **19,000+ automated tests, checked line-for-line against real HotSpot
  behavior**, so what runs on CratonVM behaves the way your team already
  expects Java to behave.

---

## A runtime built different, on purpose

For twenty-five years the JVM has rested on millions of lines of C and C++.
CratonVM starts over — in Rust.

- **Memory-safe by construction.** The interpreter, JIT compiler, and
  garbage collector are written in safe Rust wherever the problem allows
  it, closing off entire categories of memory-corruption bugs that have
  shadowed native runtimes for decades.
- **A real optimizing JIT**, not a toy interpreter: tiered compilation,
  on-stack replacement, bounds-check elimination, AVX2 vectorization — your
  hot loops compiled straight to native x86-64.
- **Modern Java, fully covered.** Java 8 through Java 25 — lambdas and
  streams, records and sealed classes, pattern matching, virtual threads,
  scoped values.
- **400,000+ lines of from-scratch Rust across 22 crates** — one coherent
  runtime, not a patchwork of decades-old C++ with a new frontend bolted on.

---

## Who's already building on it

- **AI/ML platform teams** who want GPU acceleration for Java-based
  inference and data-prep pipelines without rewriting a single kernel.
- **Data platform engineers** running Spring Boot, Hibernate, and H2 at
  scale, who want a lower-pause, more predictable GC underneath them.
- **Performance engineers** tired of opaque, decades-old JVM internals —
  CratonVM is transparent, hackable, and built to be understood.
- **Security-conscious organizations** who want their runtime's
  memory-safety story to be a design decision, not a patch cycle.

---

## Get started in sixty seconds

```bash
cargo build --release -p cratonvm-cli
./target/release/cratonvm -cp . HelloWorld
```

- **Developer guide & capability matrix:** [`README.md`](../README.md)
- **Full benchmark data & methodology:** [`BENCHMARK.md`](../BENCHMARK.md)
- **GPU offload reference:** [`docs/gpu/README.md`](gpu/README.md)
- **Install guide:** [`docs/INSTALL.md`](INSTALL.md)

**CratonVM: the Java runtime built for what's next. Built in Rust, by
Craton Software Company.**

*CratonVM is under active, fast-moving development. Java and OpenJDK are
trademarks of Oracle and/or its affiliates. NVIDIA and CUDA are trademarks
of NVIDIA Corporation. TornadoVM is a project of the Beehive Lab, University
of Manchester. These references are used for compatibility and comparison
identification only.*
