# CratonVM

### The Java runtime for AI inference and big data at scale

**CratonVM runs your existing Java on the GPU — automatically. No
annotations. No new APIs. No rewrites. The same `.class` files, the same
`main()`, the same answers HotSpot gives you, bit for bit, on a memory-safe
Rust foundation built for this decade.**

*Version 0.3.0 · Java SE 8–25 · 400K+ lines of Rust · 19K+ automated tests ·
by Craton Software Company*

---

## The problem this removes

Java owns the enterprise data layer and almost none of the accelerated
compute. When a numerical stage starts to dominate your wall clock you have
had two options, and both cost you a boundary.

**Rewrite the kernel.** In CUDA C, or in a framework that asks you to
restructure the work around its own graph API and annotate every parallel
loop. Either way there is now a second language in code review, a second
build, and a native artifact per platform in your release pipeline.

**Move the workload out.** To a Python service, where your data leaves the
JVM on every call and you have acquired a second deployment, a second
on-call rotation, a second auth surface, and a network hop in the middle of
a hot loop.

CratonVM removes the boundary instead of moving it. Your bytecode is the
input. The GPU is a target the runtime already knows how to reach.

---

## What "automatic" actually means

You compile with `javac` and you run with a flag.

```bash
cargo build --release -p cratonvm-cli --features gpu-driver
./target/release/cratonvm --gpu -c . MyPipeline
```

There is no annotation, no `TaskGraph`, no off-heap array type, no separate
kernel source. The runtime's analyzer reads the bytecode you already have,
decides which methods it can lower to PTX **while guaranteeing an identical
result**, and moves those. Everything else runs on the JIT.

That guarantee is the whole design, and it is worth being precise about:
where the analyzer cannot prove the GPU would produce the same answer, it
declines and runs your method on the CPU. `--print-gpu-decisions` prints the
verdict for every candidate and the reason it was reached. **Slower and
correct beats faster and wrong** — a runtime that quietly changed your
numbers under a performance flag would be a defect, not a feature.

---

## Measured against the alternatives

[TornadoVM](https://github.com/beehive-lab/TornadoVM) is the best-known Java
GPU framework and the fair comparison. It asks you to annotate kernels,
restructure them around a TaskGraph API, and adopt new array types before a
single line moves. CratonVM asks for none of that — and still comes out
ahead on the same hardware:

| Compute kernel (N = 2²⁴)                          | HotSpot C2 | TornadoVM GPU   | **CratonVM GPU** | vs HotSpot | vs TornadoVM |
|-----------------------------------------------------|------------|-----------------|-------------------|------------|--------------|
| Integer division chain (48 divs/elem)               | 2,179 ms   | 27 ms           | **7 ms**          | **311x**   | **3.9x**     |
| Double-precision division chain (64 divs/elem)      | 1,784 ms   | 135 ms          | **82 ms**         | **21.8x**  | **1.6x**     |
| 128 multiply-adds/elem (data-dependent multiplier)  | 1,298 ms   | 26 ms           | **7 ms**          | **185x**   | **3.7x**     |
| Dot-product reduction (int·int → long, ×300/elem)   | 1,168 ms   | *unimplemented* | **2 ms**          | **584x**   | n/a          |
| Ray tracer kernel (33.2M pixels)†                   | 837 ms     | 24.3 ms         | **12.3 ms**       | **68x**    | **2.0x**     |

Same box throughout — RTX 2060, TornadoVM 4.0.1 (PTX backend), warm, full
host→device→host round-trip included. **Only the software varies.** That is
what makes these numbers a claim about a runtime rather than about a
graphics card.

Every result, GPU rows included, is checksum-verified bit-for-bit against
HotSpot. No rounding, no approximation, no "close enough."

And the `unimplemented` row is not a gap in our table.
`TornadoSnippetReflectionProvider.forBoxed` is an unconditional stub in both
the 4.0.1 jar and current `master`, so a `@Reduce` kernel that needs an
`int`→`long` widening before accumulating into a differently-typed reduce
array cannot run there. We isolated it with three minimal repros on the same
GPU rather than inferring it from a stack trace. CratonVM's automatic path
completes the same reduction and the full read-back.

*Provenance: the first four rows were measured together on 2026-09-07 on a
verified-quiet host — same session, same build, CPU baselines included, so the
columns are comparable to each other. † The ray-tracer row is a reduced proxy
kernel carried over from an earlier run; it needs a separate binary that is not
in the tree and was not re-measured. Per-row methodology and the caveats in
full are in [BENCHMARK.md](../BENCHMARK.md) — read it before quoting any of
this.*

---

## Built for the AI era

Every modern AI workload lives or dies on one operation. CratonVM ships GEMM
— the computation behind every neural-network layer, every attention block,
every inference pass — as a built-in, GPU-accelerated kernel in both fp32
and the fp16 that makes modern inference fast.

It is not one kernel pretending to suit every shape. The GEMM ships **two
tilings and picks by the shape of your output**, because no single choice
wins everywhere: a 128×128 tile has better arithmetic intensity but covers a
256×256 problem in only four blocks, leaving most of a 30-SM device idle,
while a 64×64 tile makes sixteen and fills it. At N=1024 the large tile is
58% faster; at N=256 the small one is 67% faster. The winner reverses, and
you get the right one without asking.

For Java code that wants to call this directly rather than rely on automatic
offload, [**gpu4j**](https://github.com/craton-co/gpu4j) is the typed API
onto this runtime: `GpuBlas`, device-resident `GpuArray`, fp16 `Half`,
streams, and CUDA graph capture — one Maven dependency, no CUDA toolchain,
no JNI.

---

## Built for big data — at the scale that actually matters

AI needs matrix multiplication. Big data needs throughput, uptime, and heaps
that do not stall your service under load.

- **A collector built for large heaps and low pauses.** The default is ZGC:
  concurrent marking, compaction, colored pointers and a load barrier, with
  an opt-in generational mode — a modern design sized for the memory profile
  real data services carry, not a decades-old collector stretched to cover a
  new decade.
- **Runs the frameworks your stack is already built on.** Spring and Spring
  Boot, Tomcat, and Hibernate boot, run, and pass their real test suites on
  CratonVM today, backed by a full embedded H2 SQL database. Netty, Quarkus
  and Hibernate Reactive are next.
- **One self-contained binary.** No JDK to install, no `rt.jar`, no bloated
  base image. Download it and run it — in a container, at the edge, in a
  pipeline, wherever your data actually lives.
- **19,000+ automated tests, checked against real HotSpot behaviour**, so
  what runs on CratonVM behaves the way your team already expects Java to
  behave.

---

## A runtime built different, on purpose

For twenty-five years the JVM has rested on millions of lines of C and C++.
CratonVM starts over, in Rust.

- **Memory-safe by construction.** The interpreter, JIT and collector are
  written in safe Rust wherever the problem allows it, closing off entire
  categories of memory-corruption bug that have shadowed native runtimes for
  decades.
- **A real optimizing JIT**, not a fast interpreter: tiered compilation,
  on-stack replacement, bounds-check elimination, AVX2 vectorization — your
  hot loops compiled straight to native x86-64.
- **Modern Java, fully covered.** Java 8 through Java 25 — lambdas and
  streams, records and sealed classes, pattern matching, virtual threads,
  scoped values.
- **400,000+ lines of from-scratch Rust across 22 crates** — one coherent
  runtime, not decades-old C++ with a new frontend bolted on.

---

## Who is building on it

- **AI/ML platform teams** who want GPU acceleration for Java inference and
  data-prep without rewriting a single kernel.
- **Data platform engineers** running Spring Boot, Hibernate and H2 at
  scale, who want a lower-pause, more predictable GC underneath them.
- **Performance engineers** tired of opaque, decades-old JVM internals —
  CratonVM is transparent, hackable and built to be understood.
- **Security-conscious organizations** who want their runtime's
  memory-safety story to be a design decision, not a patch cycle.

---

## Honest about scope

Better to find these out here than in week three.

**The automatic GPU path is a narrow eligibility subset.** It is not a
promise that arbitrary Java runs on the accelerator. It is a promise that
what does run returns the same answer, and that you can ask why anything was
declined.

**NVIDIA only, for now.** GPU offload needs a CratonVM built with
`--features gpu-driver` and an NVIDIA device. The CPU runtime has no such
requirement.

**Pre-1.0 and fast-moving.** Java SE 8–25 is covered and 19,000+ tests
enforce it, but this is a young runtime. Bring us your workload and we will
tell you honestly whether it is ready for you.

---

## Get started in sixty seconds

```bash
cargo build --release -p cratonvm-cli
./target/release/cratonvm -cp . HelloWorld
```

- **Developer guide & capability matrix:** [`README.md`](../README.md)
- **Full benchmark data & methodology:** [`BENCHMARK.md`](../BENCHMARK.md)
- **GPU offload reference:** [`docs/gpu/README.md`](gpu/README.md)
- **How it compares, in detail:** [`docs/gpu/COMPARISON.md`](gpu/COMPARISON.md)
- **Install guide:** [`docs/INSTALL.md`](INSTALL.md)
- **The Java API onto this runtime:** [gpu4j](https://github.com/craton-co/gpu4j)

**Bring us a workload.** A batch that runs too long, a model you cannot
serve economically, a pipeline stage that dominates your cluster bill — we
will benchmark it against what you run today, and tell you if the answer
is no.

---

*CratonVM is under active, fast-moving development. Java and OpenJDK are
trademarks of Oracle and/or its affiliates. NVIDIA and CUDA are trademarks
of NVIDIA Corporation. TornadoVM is a project of the Beehive Lab, University
of Manchester. These references are used for compatibility and comparison
identification only.*
