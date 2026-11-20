# ZGC capabilities and limits

ZGC is the default collector. The [architecture contract](zgc-architecture.md)
describes allocation, marking, relocation, and configuration.

The concurrent production profile is enabled by default. Concurrent relocation
is opt-in (`CRATONVM_ZGC_CONCURRENT_RELOCATE=1`) and is unavailable for the
generational ZGC configuration. SATB marking is distinct from the optional
colored-reference relocation barrier.

JIT code must honor the active heap's barriers and published root metadata.
Native code must use scoped handles for references that survive allocation or
safepoints. Unsupported root coverage and unsafe foreign-reference shapes must
refuse moving collection rather than continue with stale pointers.

The implementation does not claim HotSpot ZGC pause-time or throughput parity.
See [known GC issues](../known-issues/gc/), [JIT barriers](jit-barrier-lowering.md),
and [native handles](native-handle-discipline.md).
