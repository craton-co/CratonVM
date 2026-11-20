# Concurrent garbage collection

CratonVM supports ZGC, Generational, and G1 collectors. ZGC is the launcher
default. Concurrent work and moving-object safety are separate capabilities.

## ZGC

The concurrent profile is enabled by default in production
(`gc/src/zgc.rs::ZGC_CONCURRENT_PROFILE_DEFAULT_ON`). It uses SATB marking;
`CRATONVM_ZGC_CONCURRENT=0` selects the stop-the-world profile. GC crate unit
tests pin the stop-the-world profile unless they explicitly enable concurrency.

Concurrent relocation and the colored-reference load-barrier stack are opt-in
with `CRATONVM_ZGC_CONCURRENT_RELOCATE=1`. This requires the concurrent profile
and a non-generational heap. Individual stack switches can override the profile.
`CRATONVM_ZGC_GENERATIONAL=1` enables the separate generational configuration.
See [ZGC architecture](zgc-architecture.md).

## Generational and G1

`-XX:+UseGenerationalGC` selects a moving young generation with concurrent
old-generation marking and sweeping. Young evacuation falls back to non-moving
collection when complete root coverage cannot be established.
`-XX:+UseG1GC` selects the experimental region collector and its concurrent marker.

Reference writes must preserve SATB and remembered-set invariants. Blocking
mutators publish roots through the safepoint protocol. Collector concurrency
does not relax JNI handle, frame-map, or reference-publication requirements.
See [barrier lowering](jit-barrier-lowering.md), [native handles](native-handle-discipline.md),
and the [GC reference](../GC.md).
