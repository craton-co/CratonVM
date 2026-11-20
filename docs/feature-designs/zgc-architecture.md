# ZGC architecture

ZGC is the default collector. Its production concurrent profile is enabled by
default; it uses SATB marking and supports concurrent sweep and reference work.
`CRATONVM_ZGC_CONCURRENT=0` disables that profile.

## Relocation and references

Stop-the-world compaction and concurrent marking do not require the concurrent
relocation profile. Concurrent relocation is opt-in with
`CRATONVM_ZGC_CONCURRENT_RELOCATE=1`; it requires concurrency and a
non-generational heap. That profile arms the colored-reference load barrier,
color flip, relocation plan, and remapping/quarantine machinery. Individual
switches can override the profile, so diagnostics must report the effective
configuration rather than infer it from the collector's name.

The implementation is in `gc/src/zgc.rs` and `gc/src/zgc/`. Allocation uses
TLABs and a page allocator; moving cycles coordinate marking, reference
processing, forwarding, and root repair. A cycle must refuse unsafe relocation
when root coverage, native references, or compiled access paths are unproven.

## Generational configuration

`CRATONVM_ZGC_GENERATIONAL=1` enables ZGC's generational configuration. The
concurrent relocation profile refuses this configuration. The separate
`-XX:+UseGenerationalGC` collector has its own young/old design.

See [concurrent GC](concurrent-gc-maturation.md),
[JIT load barriers](zgc-jit-load-barrier.md),
[reference slots](zgc-reference-slot-representation.md), and [GC tuning](../gc-tuning.md).
