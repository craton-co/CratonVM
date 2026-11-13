# Netty ByteBuf access and composite-suite performance

> **2026-11-12 verification: OPEN.** The historical attribution below is superseded.
> The original `RunNetty bytebuf` driver is absent from the repository and host.
> Its 983/9 ms row, component estimates and promised parity are not reproducible
> measurements. Unsafe inline accesses already exist in both tiers; plain long
> native stores have no unconditional sequential-consistency fence.

## Reproduction and implemented repairs

The checked [ByteBuf probe](../../../tools/bench/apps/netty/README.md) checks variable
values, bounds, endian order, unaligned access, slices and release in separate
processes for four buffer kinds. Its workload and timing boundary differ from
the missing driver; these results do not replace that historical row as an A/B.
Both Java 25 Unsafe policies and the same complete Netty/JCTools classpath are
used for HotSpot and both CratonVM modes.

Implemented on the task branch from `575a2e2d83`, reconciled with upstream
`5427879a33`; broad lifecycle/composite validation below uses `cbe3f7b297`:

- `f4e89ca851`: validated plain integral Unsafe field reads retain their fast arm
  under the ZGC reference-read gate. The per-access ref-count helper no longer
  runs millions of times; volatile, store and reference guards remain.
- `a2db3a1ea3`: boot-image restore excludes the process-local code-window cursor.
  Restoring that cursor without its OS reservation let `MAP_FIXED` overwrite
  libc arenas. Baseline/candidate GDB and strace controls, fresh capture and
  24 restored starts verify the repair.
- `ef61b1bfea`: branch scans reject offsets beyond the maximum target before
  hashing and skip empty exemption sets. Isolated compiler instruction cost
  drops; this did not close the whole-class gate.
- `f7f2653183`: positive native allocation membership is memoized by exact address
  and invalidation generation. Every successful shared take/free/resize retires
  the proof; no negative result or FFM scope verdict is assumed valid.
- `20fd398fa2`: plain byte-array view writes use the existing judged five-word
  helper ABI for all six primitive carriers. Type/identity/bounds/contiguity
  checks, endian semantics and failed-write atomicity are retained.
- `22561ad2cc`: an exact shared session may use the existing close window under
  armed ZGC. Layout, state, null-owner and in-window checks remain. Real owned
  session verdicts are no longer cached across a JDK state-only close, fixing
  post-close reads/writes and allocator corruption. A view closed without freeing
  its backing also matches HotSpot. Four alternating pooled-direct repetitions
  improve about 204/182 ns to 46/46 ns per pair before upstream reconciliation.

Reconciled runtime `cbe3f7b297` passes 180 VM lifecycle/view processes
across both modes and three collectors with default and explicitly armed
concurrent relocation, the 96-process access matrix, 12 composite processes,
and 18 kernel processes with exact HotSpot checksums. Kernel pricing has one
repetition per arm and is not a broad performance non-regression verdict.

## 2026-11-12 conditional close-window repair

`803d3e7c1a` opens a ByteBuffer close window only when the buffer has a
memory segment. Scoped carriers retain their in-window session proof and the
guard through the memory access. Ordinary heap/direct buffers retain all other
validation. The close fixture now interleaves ordinary and scoped accesses and
collects before the concurrent close.

Before reconciling the next upstream changes, 180 lifecycle/view processes and
the 96-process access matrix pass. Four alternating default unpooled-direct
process pairs observe 358.20 -> 334.08 ns in foundational mode and
353.20 -> 343.67 ns in compatible mode. Whole-class A/B totals overlap; this
is not closure of that gate. All six kernel checksums match HotSpot.

The `Long.reverseBytes` splice experiment was reverted after a pooled-buffer
slowdown; its compatible-mode gains are not delivered. The larger compiler
control also exposed a pre-existing [javac correctness residual](netty-javac-120-c1-door-off-loses-resource-symbol-20261112.md)
when C1-first is disabled. The default-policy controls match all 225 HotSpot
class files. All 80 startup controls also pass on reconciled runtime
`cbe3f7b297`. The report remains **OPEN** for the performance and javac residuals.

## Earlier measurements and remaining broad retirement gates

Host `64.177.43.150`, shared Linux x86-64, no-LTO release, HotSpot 25. Each cell
below is the median of four process medians of warm rounds 1-4, ns per long
set/get pair. These are shared-host observations, not a quiet-host baseline.

| Unsafe policy | Buffer | HotSpot | Foundational | Compatible |
| --- | --- | ---: | ---: | ---: |
| allow | unpooled heap | 0.81 | 42.05 | 42.14 |
| allow | unpooled direct | 0.86 | 40.63 | 52.73 |
| allow | pooled heap | 0.91 | 43.35 | 43.40 |
| allow | pooled direct | 0.86 | 39.25 | 51.80 |
| default | unpooled heap | 0.74 | 249.90 | 255.48 |
| default | unpooled direct | 0.79 | 361.39 | 363.88 |
| default | pooled heap | 0.90 | 282.89 | 267.77 |
| default | pooled direct | 0.95 | 45.54 | 45.55 |

`AdvancedLeakAwareCompositeByteBufTest` still costs median **225.38 G / 224.87 G**
instructions in foundational/compatible mode against **34.59 G** for HotSpot
(four fresh processes each; every arm found 508, passed 499, aborted 9, failed 0).
The <=2x gate is **not met**. Upstream collector, memo and regex changes are included;
the change from older totals must not be attributed solely to this task.

Still open: public-access call/inline cost, default-policy heap and unpooled
DirectByteBuf VarHandle cost, and the related [whole-class CPU residual](netty-buffer-suite-cpu-cost-jit-compile-20261102.md).
The linked architecture proposal also retains startup/javac validation debt.
The absence of the original driver does not retire these reproduced residuals.
Do not archive this page as a completed performance fix yet. The combined H2
page was archived upstream for its H2 findings and points here for Netty.

Raw logs, binaries, source snapshots, validated JSON and the action log remain
under `/root/CVM-netty-retire-01a1c7cf-evidence/` on the authorized host.

## Paired ByteBuf access results, 2026-11-12

Runtime `60c95229cc` is rebuilt from reconciled `dev`. Against baseline
`1d85eeef89`, the maintained probe's default-policy aggregate median slowdown
falls **50.8% foundational / 51.1% compatible**: **255.1x → 125.7x / 260.7x →
127.5x**. These are the [PERFORMANCE.MD](../../../PERFORMANCE.MD) row's median
across all four buffer kinds and four process repetitions, using each process's
median of warm rounds 1–4. Baseline/candidate order alternates, with common
HotSpot controls. The historical missing driver's 109.22x is not an A/B baseline.

The implementation removes redundant work: byte-array carriers use one checked
forwarding proof and the existing validated-kind accessor; metadata planners
already reject exact handles, so the duplicate thin-helper exactness check is
removed; ByteBuffer carriers reuse the existing epoch-scoped receiver proof
before validated forwarding. Type, bounds, endian, exact-descriptor, collector
and scope/close behavior remain checked. No new cache, flag or dependency is added.

| Default Unsafe policy | Foundational before/after (ms per 1M pairs) | Compatible before/after |
| --- | ---: | ---: |
| unpooled heap | 252.69 / 120.80 | 246.30 / 123.73 |
| unpooled direct | 344.93 / 248.02 | 351.97 / 246.21 |
| pooled heap | 259.12 / 134.48 | 261.42 / 128.15 |
| pooled direct | 40.29 / 40.33 | 42.36 / 44.59 |
| aggregate median | 254.93 / 125.55 | 260.51 / 127.38 |

The halving applies to the aggregate default-policy row, not every buffer kind.
Unsafe-allowed aggregate medians are 31.94 → 36.67 ms foundational and
41.95 → 43.61 ms compatible; that path did not improve in this shared-host run.
No general performance non-regression claim is made.

All 160 fresh processes (64 baseline, 64 candidate, 32 HotSpot) pass five exact
checksums each. The rebuilt runtime also passes all five primitive VarHandle
fixtures and the buffer scope/close fixture, covering both modes, three
collectors, exact handles, endian/wire bits, bounds and fallback behavior.
The wider-inline and endian-loop experiments were discarded after failing to
show dependable gains. The integrated changes delete redundant checks instead.

Raw commands, binary hashes, per-process timings and the aggregate gate are in
`/root/CVM-netty-retire-01a1c7cf-evidence/half-gap-paired-final/` (`results.json`,
`binaries.json`, `summary.json`); exact-source build/test logs and the action log
are in its parent. The whole-class CPU and javac gates above remain open.

## Historical analysis (unverified attribution)

**Date:** 2026-11-10  
**Target:** CratonVM (cratonvm-cli) vs OpenJDK HotSpot 25 / GraalVM 25  
**Workload Analyzed:** io.netty.buffer.ByteBufAccessBenchmark (Direct & Heap ByteBuf Read/Write Access: ~109.22x gap)  
**Parent Document:** [PERFORMANCE.MD](../../../PERFORMANCE.MD)

---

## 1. Executive Summary

In the real-world enterprise suite evaluation ([PERFORMANCE.MD](../../../PERFORMANCE.MD)), CratonVM demonstrates a typical execution latency overhead of **2.5x to 7.5x** relative to HotSpot 25 across complex business logic, parsing, reflection, and database persistence (e.g. Tomcat EL parser at 2.91x, Spring AOP at 2.78x, H2 MVStore at 3.01x, Hibernate ORM at 4.86x).

However, **Netty ByteBufAccessBenchmark** surfaced as a pronounced outlier:
- **HotSpot 25 (C2 JIT):** 9 ms for 1,000,000 operations (~9.0 ns/op)
- **CratonVM:** 983 ms for 1,000,000 operations (~983.0 ns/op)
- **Slowdown Ratio:** **109.22x**

This investigation provides an in-depth breakdown of the assembly, JIT compiler intrinsics, memory safety guarantees, and native bridge dispatch mechanisms that cause this performance disparity, followed by a targeted implementation roadmap to bring Netty buffer throughput to near-parity.

---

## 2. Benchmark Architecture

The Netty ByteBufAccessBenchmark (io.netty.buffer.ByteBufAccessBenchmark) measures the raw read and write throughput of off-heap direct memory buffers and on-heap arrays:

`java
// Simplified execution loop from ByteBufAccessBenchmark:
public void setGetLong(Blackhole bh) {
    for (int i = 0; i < 1_000_000; i += 8) {
        buffer.setLong(i, 0x0123456789ABCDEFL);
    }
    for (int i = 0; i < 1_000_000; i += 8) {
        bh.consume(buffer.getLong(i));
    }
}
`

Tested buffer implementations:
- UnpooledUnsafeDirectByteBuf (backed by off-heap address allocated via Unsafe.allocateMemory)
- PooledUnsafeDirectByteBuf (Netty arena slab allocator with direct memory addresses)
- UnsafeHeapByteBuf (backed by on-heap yte[] accessed via array base offset)

---

## 3. Disassembly & Subsystem Mechanics

### 3.1 HotSpot 25 (C2 JIT Compiler Intrinsics)

In OpenJDK HotSpot:
1. sun.misc.Unsafe.putLong(long address, long value) and Unsafe.getLong(long address) are registered as **C2 compiler intrinsics** (mIntrinsics::_putLong, mIntrinsics::_getLong).
2. **Inlining Down to Zero Call Overhead:** HotSpot C2 completely inlines ByteBuf.setLong and Unsafe.putLong. There is no JNI boundary crossing, no stack frame creation, and no parameter marshaling.
3. **Hardware Instruction Emission:** C2 lowers the operation directly to a single x86_64 machine instruction:
   `ssembly
   ; HotSpot C2 compiled loop kernel for setGetLong
   .L_loop:
     movq   , (%rdi, %rax)  ; 1 cycle (direct memory write)
     addq   , %rax
     cmpq   , %rax
     jl     .L_loop
   `
4. **Loop Vectorization & Pipelining:** The CPU execution pipeline executes this tight unrolled loop with zero pipeline stalls at ~9 nanoseconds per operation (limited only by L1d/L2 cache write-combining buffers).

### 3.2 CratonVM (Native Bridge Dynamic Dispatch)

In CratonVM, direct memory access follows a fundamentally different path:

`
[Java Bytecode: invokevirtual Unsafe.putLong]
         │
         ▼
[VM Interpreter / Baseline JIT Stub]
         │
         ▼
[Native Bridge: native_unsafe_put_long_mb]
         ├── 1. Argument Extraction & Boxing (~120 ns)
         ├── 2. Arena Handle & Bounds Validation (~240 ns)
         ├── 3. Memory Barrier Synchronization (~180 ns)
         └── 4. Native Bridge Call/Return Overhead (~440 ns)
         │
         ▼
[Memory Write: *mut u64 = val]
`

#### Detailed Breakdown of Overhead per Operation:
1. **Dynamic Native Bridge Invocation (~440 ns):** The bytecode interpreter or baseline JIT dispatches through cratonvm_native_api. This allocates and sets up a native bridge call frame, transitions the execution context, and manipulates the operand stack.
2. **Argument Unpacking (~120 ns):** Java parameters (long address, long value) are extracted from VM Value representations into Rust native primitives.
3. **Arena Handle & Memory Bounds Verification (~240 ns):** CratonVM validates that the raw pointer falls within a registered memory arena or valid direct buffer allocation to prevent segfaults and protect VM integrity.
4. **Memory Barrier Synchronization (~180 ns):** CratonVM enforces sequential consistency fences (ence(Ordering::SeqCst)) on off-heap modifications to satisfy multi-threaded Java visibility rules.
5. **Total Per-Operation Latency:**
   pprox 440\,	ext{ns} + 120\,	ext{ns} + 240\,	ext{ns} + 180\,	ext{ns} pprox 980\,	ext{ns}
6. **Cumulative 1,000,000 Operations:**
   1{,}000{,}000 	imes 980\,	ext{ns} = 980{,}000\,	ext{µs} = 980\,	ext{ms}
   Directly accounting for the measured **983 ms**.

---

## 4. Why This Does Not Affect General Application Workloads

Most application workloads (Tomcat, Spring, Hibernate, Bouncy Castle) perform bulk I/O, business logic, ORM mapping, and transaction management where primitive buffer access accounts for less than 1% of total instruction retirement:
- **Tomcat EL Parsing (TestELParserPerformance):** 2.91x ratio
- **Spring AOP Delegation (BenchmarkTests):** 2.78x ratio
- **H2 MVStore Key-Value Engine (TestMVStoreBenchmark):** 3.01x ratio
- **Hibernate ORM Batch Persistence (ElementCollectionPerfTest):** 4.86x ratio

The 109x gap is an isolated micro-kernel artifact strictly confined to synthetic loops executing millions of back-to-back unbuffered primitive memory writes over Unsafe.

---

## 5. Architectural Optimization Roadmap

To bring Netty buffer operations to near-parity with HotSpot C2, the following VM optimizations are planned:

| Phase | Optimization | Technical Action | Projected Latency |
| :--- | :--- | :--- | :--- |
| **Phase 1** | **Unsafe Fast-Path Stubs** | Bypass generalized cratonvm_native_api reflection stubs for Unsafe.getLong/putLong; execute direct Rust pointer dereference on validated buffer memory. | ~150 ns / op (~15x gap) |
| **Phase 2** | **JIT Compiler Intrinsics** | Recognize sun.misc.Unsafe / jdk.internal.misc.Unsafe calls in the JIT compiler tier and lower them directly to single machine instructions (movq [rdi + rsi], rdx). | ~10–15 ns / op (~1.2x–1.6x gap) |
| **Phase 3** | **Memory Fence Coalescing** | Coalesce sequential consistency memory barriers across consecutive array/buffer loop iterations unless the buffer is declared olatile. | ~9 ns / op (Parity) |

---

## 6. Conclusion

The 109x latency gap on Netty ByteBufAccessBenchmark is an architectural characteristic of executing raw Unsafe memory operations through safe native bridge wrappers rather than compiler intrinsics. It does not reflect general VM degradation. With JIT compiler intrinsics for primitive memory access, this workload will achieve sub-20ms execution times and reach parity with HotSpot C2.
