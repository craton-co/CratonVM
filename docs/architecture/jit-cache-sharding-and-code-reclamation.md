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
publish an immutable snapshot, while GC/frame lookup performs a lock-free
binary search. A snapshot is a shared sorted `base` plus a small sorted
`recent` half (about sqrt(n) entries, merged into `base` when full), so a
registration copies O(sqrt n) entries rather than the whole table. Stack
scanners can retain a snapshot for an entire scan. The validation region list
(`validate_code_ptr`) is republished lazily on additions and eagerly on
removals, so its snapshot may lag by a new buffer (answered by the locked
lookup) but never by a freed one.

## Reclamation invariants

Every raw executable edge has a matching owner:

- lock-free cache readers hold `Arc<CompiledMethod>`;
- baked JIT-to-JIT calls are recorded in the caller and retain their callee;
- MIC/PIC entries retain their compiled targets;
- VM thread-local dispatch caches retain their compiled targets;
- interpreter invoke-cache entries already retain the compiled method;
- active JIT frames are bounded by the VM JIT entry guard.

Invalidation logs itself in the cache's invalidation log, marks every doomed
body `retired`, computes the transitive reverse closure of baked direct calls,
clears matching MIC/PIC targets, and publishes replacement cache snapshots. A
publication re-checks the log (a compile that began before an invalidation of
something it inlined is refused), refuses retired direct callees, and an
inline-cache install re-checks `retired` after publishing and rolls back.

Withdrawn owners enter a deferred queue, stamped with a retirement generation,
while any JIT frame is active; this covers the machine-code interval between
loading an entry word and executing the indirect call. The queue is drained
when the in-JIT count reads zero, or per owner on per-thread evidence, so a
thread parked inside compiled code holds only the bodies its stack names (see
`docs/jit/code-cache-lifecycle.md`). The last `Arc<CompiledMethod>` unregisters
its code range, releases its compile id, purges dependent OSR trampolines,
drops embedded metadata (including its deopt boxes), and returns the
executable mapping to the OS.

Inline-cache ways are write-once under a per-slot writer lock: a way's class id
and tagged entry word are never retargeted under a reader. A stale way is
retired (class id set to a tombstone, then the word cleared) and reused for a
different receiver only once its retirement generation is graced.

External dispatch caches compare a monotonic JIT cache generation before using
a raw entry. A tier replacement or invalidation therefore flushes stale entries
and releases their owners before the cache is probed again.

`CRATONVM_JIT_FREE_CODE` no longer exists. Deoptimization metadata follows the
same artifact ownership as its code, so an executing superseded frame remains
reconstructable without whole-method replay.

## Verification

- 929 `cratonvm-jit` library tests pass.
- 22 VM JIT-boundary/precise-root tests pass.
- Focused tests cover concurrent cache publication, lock-free range lookup,
  direct-callee ownership, immediate reclamation after the last reader, and
  deferred MIC reclamation at JIT quiescence.
- Release probes cover JIT and `--nojit`; call-heavy throughput is compared
  against the phase-1 binary on the same pinned CPU.
