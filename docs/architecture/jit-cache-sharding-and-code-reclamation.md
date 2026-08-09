# Lock-free JIT lookup and executable-code reclamation

Status: implemented

## Architecture

The compiled-method cache is split into 64 shards. Each shard publishes an
immutable `Arc<FxHashMap<...>>` through `ArcSwap`; `get` and `get_osr` perform
one atomic snapshot load, one hash lookup, and one `Arc<CompiledMethod>` clone.
They acquire no VM-wide or shard lock. Publications copy only the affected
shard. A single mutation mutex serializes publication with transitive
invalidation, where atomic multi-shard visibility matters more than write
throughput.

The native code-range table uses the same read-copy-update shape: writers
publish a sorted immutable snapshot, while GC/frame lookup performs a lock-free
binary search. Stack scanners can retain a snapshot for an entire scan.

## Reclamation invariants

Every raw executable edge has a matching owner:

- lock-free cache readers hold `Arc<CompiledMethod>`;
- baked JIT-to-JIT calls are recorded in the caller and retain their callee;
- MIC/PIC entries retain their compiled targets;
- VM thread-local dispatch caches retain their compiled targets;
- interpreter invoke-cache entries already retain the compiled method;
- active JIT frames are bounded by the VM JIT entry guard.

Invalidation computes the transitive reverse closure of baked direct calls,
clears matching MIC/PIC targets, and publishes replacement cache snapshots.
Cleared inline-cache owners enter a deferred queue while any JIT frame is
active, covering the machine-code interval between loading an entry pointer and
executing the indirect call. The final JIT exit drains that queue. The last
`Arc<CompiledMethod>` unregisters its code range, purges dependent OSR
trampolines, drops embedded metadata, and returns the executable mapping to the
OS.

External dispatch caches compare a monotonic JIT invalidation generation before
using a raw entry. A tier replacement or invalidation therefore flushes stale
entries and releases their owners before the cache is probed again.

`CRATONVM_JIT_FREE_CODE` is no longer needed for reclamation and no longer
changes lifetime behavior. Deoptimization metadata follows the same artifact
ownership as its code, so an executing superseded frame remains reconstructable
without whole-method replay.

## Verification

- 929 `cratonvm-jit` library tests pass.
- 22 VM JIT-boundary/precise-root tests pass.
- Focused tests cover concurrent cache publication, lock-free range lookup,
  direct-callee ownership, immediate reclamation after the last reader, and
  deferred MIC reclamation at JIT quiescence.
- Release probes cover JIT and `--nojit`; call-heavy throughput is compared
  against the phase-1 binary on the same pinned CPU.
