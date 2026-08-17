# Packed object fields: production truth and allocation lookup optimization

Fixed/verified on `codex/complete-architecture-remediation-20260726`.

## Architecture finding

The closeout recommendation that production instance fields still occupy a
universal 16-byte `Value` cell was stale. Descriptor-backed classes already
have a `CompactLayout` with naturally aligned tagless payloads:

- boolean/byte: 1 byte
- char/short: 2 bytes
- int/float: 4 bytes
- reference/long/double: 8 bytes

Allocators stamp `GC_FLAG_COMPACT` and store the body byte size in the object
header. Interpreter, JIT helpers, G1, ZGC, generational GC, serviceability, and
oop-map tracing all dispatch on that per-object flag. Legacy 16-byte cells are
retained only for padded/typeless synthetic containers or a stale field-count
allocation, where descriptors cannot prove a safe tagless representation.
Keeping that fallback is a correctness requirement, not unfinished packing.

## Remaining performance problem

Packing was enabled by default, but `compact_object_body_size` and the
generational allocation planner resolved the current class layout through a
global `RwLock` and cloned/dropped an `Arc` for every allocated object. The
field-access and GC scan paths had already acquired generation-validated
thread-local caches, so allocation remained the avoidable outlier.

`with_current_class_layout` now provides one shared 8-entry per-thread working
set for current-layout allocation and field resolution:

- MRU-first lookup for same-class allocation runs;
- negative caching for legacy/padded classes;
- global layout-generation validation on register, redefine, unload, and test
  reset;
- borrowed cache hits, avoiding both registry locks and `Arc` atomic RMWs;
- current recipes only: historical `(class_id, field_count)` versions remain
  available to GC scanning but are never reused to allocate a newly obsolete
  compact shape.

## Verification

- 15 focused field-layout tests, including replacement, negative-registration,
  unregister, historical-version exclusion, reentrancy, and cross-thread
  eviction.
- all 871 `cratonvm-gc` library tests.
- `cargo check -p cratonvm-vm --all-features`.
- `BinTreesClassic 17` checksum: `29971806` in compact and legacy modes.

Pre-change r4 single-run A/B on an otherwise idle host:

| layout | kernel time |
| --- | ---: |
| packed/default | 588 ms |
| legacy `CRATONVM_COMPACT_REF_FIELDS=0` | 1009 ms |

Packed fields therefore reduced this allocation/GC-heavy kernel time by about
42%.

Post-change r5 alternating medians:

| path | r4 | r5 | change |
| --- | ---: | ---: | ---: |
| JIT, depth 17 | 581 ms | 580 ms | neutral (inline TLAB bypasses planner) |
| `--nojit`, depth 12 | 2,343 ms | 2,326 ms | −0.7% |

The interpreter result is intentionally reported conservatively: the small
gain is close to run noise. The architectural win is deterministic—the steady
allocation path no longer acquires a global layout lock or performs two Arc
atomic RMWs per object—and the change is retained primarily for that bounded
contention removal rather than claiming a large benchmark improvement.

Release binary:
`/data/data/bin/cratonvm-complete-remediation-20260726-r5`.
