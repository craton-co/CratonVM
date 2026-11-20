# Feature contracts

These pages describe current capabilities and limits. Real OpenJDK execution is
the foundational architecture; the launcher defaults to `--real-jdk`.
`--compatible` enables the cold-start and throughput optimization layer.
Embedding entry points deliberately default to compatible policy.

## Runtime and class libraries

- [Foundational execution contract](real-jdk-mode.md)
- [Native registration and dispatch](native-dispatch-architecture.md) and [promotion review](native-promotion-review.md)
- [Class resolution and caching](class-resolution-and-caching.md)
- [Embedding API](embedding-api.md) and [foreign-thread attach](foreign-thread-attach.md)
- [JVMTI delivery](jvmti-delivery-threading.md)
- [Helpful null-pointer messages](jep358-helpful-npe.md)
- [Key stores and post-quantum algorithms](keystore-mldsa-mlkem.md)
- [Synthetic-class fallibility](synthetic-class-fallibility.md) and [field-name lookup](by-name-field-reads.md)
- [Collection interception](collections-interception.md)

## JIT

- [IR optimization](jit-ir-optimizations.md) and [optimizer integration](activate-ir-optimizer.md)
- [Machine instructions and allocation](jit-machine-level-and-instruction-selection.md)
- [Backend boundaries](two-backends-decision.md) and [tiered compilation](wire-tiered-manager.md)
- [Real-frame deoptimization](real-frame-deopt.md) and [OSR/deopt contract](deopt-osr.md)
- [OSR entry](jit-osr-entry-metadata.md) and [exit/recompile](jit-osr-exit-and-recompile.md)
- [Precise maps](precise-jit-maps-default.md) and [local exception handlers](jit-local-exception-handlers.md)
- [Profile-guided inlining](profile-guided-inlining.md) and [compiled regex](regex-jit-contract.md)
- [AVX-512 primitives](avx-512-vector-acceleration.md) and [numerical loops](avx-512-commons-math.md)
- [Optional GPU offload](gpu-offload.md), [per-VM GPU GC isolation proposal](gpu-per-vm-gc-isolation.md), and [async retirement & fault mocking proposal](gpu-async-retirement-and-fault-mocking.md)

## Memory management

ZGC is the default collector. Its concurrent marking profile is enabled;
concurrent relocation is opt-in. The collector name alone does not imply that
the colored-reference relocation barrier is armed.

- [ZGC architecture](zgc-architecture.md) and [capabilities/limits](zgc-production-implementation-plan.md)
- [Concurrent collection](concurrent-gc-maturation.md)
- [JIT load barriers](zgc-jit-load-barrier.md) and [reference slots](zgc-reference-slot-representation.md)
- [Generational collector](generative-gc-architecture.md) and [moving young generation](default-moving-young-gen.md)
- [Compact reference fields](compact-ref-field-layout.md)
- [Native handles](native-handle-discipline.md) and [barrier lowering](jit-barrier-lowering.md)

## Correctness infrastructure

- [Class-file parser hardening](class-file-parser-hardening.md)
- [Semantic differential harness](differential-fuzzer.md)
- [Fuzz harness](fuzzing-state.md)
- [Curated JDK regression baseline](jdk-regression-baseline.md)

Configuration defaults belong to the source read site. The [configuration
reference](../CONFIG.md) lists the available flags; [known issues](../known-issues/)
records unresolved defects and accepted limitations. Proposals and historical
evidence belong to the ignored local archive.
