# Class-loader unloading and bounded metadata

CratonVM reclaims a user-defined loader as a stop-the-world transaction after
the collector proves its Java `ClassLoader` unreachable. Class IDs remain
monotonic and unloaded `ClassStore` slots become tombstones; a stale numeric ID
can therefore never silently name a replacement class.

## Liveness and ordering

The root scanner treats user-loader class metadata as a conditional graph:

1. live interpreter frames and active compiled entries retain their defining
   loaders;
2. a live loader retains its mirrors, static references, class-monitor objects,
   resolved dynamic constants, and serialization descriptors;
3. ordinary heap reachability still retains an object's defining loader;
4. after marking, dead loader IDs are grouped and reclaimed while all mutators
   remain stopped.

Generational non-moving full marks, G1 initial/final full marks, and ZGC-real
mark-sweep all follow this side-edge closure. Moving young collections and G1
evacuation pauses keep metadata conservatively rooted; an explicit
`System.gc()` completes the collector's full-mark path before returning.

The unload transaction first removes loader/name aliases and tombstones class
metadata. It then invalidates dependent vtables, allocation recipes, profiles,
tiering state, deoptimization assumptions, compiled entries (including callers
which inlined the unloaded class), serialization metadata, mirrors, statics,
locks, initiating-loader entries, and native side tables. Executable entries
use the JIT epoch reclaimer, so retired code is unmapped only after no lookup
reader or active compiled frame can still execute it.

This ordering prevents both premature reclamation and stale-ID reuse. Published
loader/metadata edges participate in the same collector marking closure as
ordinary references.

## Bounded process-lifetime caches

The following caches now have either unload invalidation or hard bounds:

- class-byte and shared-resolution caches;
- ClassValue memoization (65,536 entries, with loader-owned values invalidated
  on unload);
- per-loader initiating-resolution entries (4,096 per loader);
- method epoch counters (65,536 exact entries plus a fail-closed shared
  overflow epoch);
- field-descriptor entries (65,536);
- serialization descriptors, allocation recipes, JIT profiles, tier state,
  vtables, mirrors, class monitors, and static storage.

Correctness-sensitive overflow paths are fail-closed: they may invalidate more
compiled work, but never preserve a raw pointer whose owner can be unloaded.

## Diagnostics and regression gate

`ClassLoadingMXBean` reports the current live class count, cumulative class
definitions, and cumulative unloaded classes. The checked-in
`class_loader_unload` probe repeatedly defines a synchronized, serializable
class in parentless loaders, heats its methods, drops every strong reference,
and verifies weak loader/class references, MXBean counters, and a bounded live
class count in both JIT and `--nojit` modes.
