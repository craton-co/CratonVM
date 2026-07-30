# CratonBench HashMap half-gap closeout (2026-07-30)

## Acceptance

Goal: reduce the `CratonBench hashmap` wall-time gap between CratonVM and
HotSpot by at least 50%, with every run returning the exact checksum
`1549999915000000`.

```text
old_gap = median(baseline) - median(HotSpot)
new_gap = median(final)    - median(HotSpot)
gap_reduction = 1 - new_gap / old_gap
```

Baseline is `origin/dev` at `9ac1feffe`, the commit this branch was last
merged from.

## First: the recorded starting point was stale by ~6x

`BENCHMARK.md` carried `HashMap (10M put/get) — 1,039 / 22,077 ms, 21.2x`
(2026-07-25) and an OPEN item calling the regression "CONFIRMED and bounded to
`a36b9d121..e57f0bc7d`". **Neither reproduces.** Measured on this host before
touching anything, isolated fresh processes, `-Xmx8g`, alternating arms pinned
to **cpu 15**:

| n | HotSpot JDK 25 | CratonVM `dev` @ `9ac1feffe` | ratio | BENCHMARK.md's recorded value |
|---|---:|---:|---:|---|
| 1,000,000 | 66 ms | 427 ms | 6.5x | 3,084 ms / 42.7x |
| 10,000,000 | 997 ms | 3,523 ms | 3.53x | 22,077 ms / 21.2x |

427 ms at 1M is within noise of the 434 ms that the same document records for
the *healthy* `a36b9d121` (07-18) binary, so the bisect range it recommends has
nothing left in it. 3,523 ms at 10M is also consistent with the 4,300 ms
perf-gate baseline that the same document declares unreproducible.

The most likely explanation is the one `BENCHMARK.md` states two paragraphs
below the numbers it invalidates: **the gate's default pin is cpu 13, and
concurrent sessions on this shared host pin their own CratonBench to cpu 13
as well**, so a "quiet host" reading (1-min load average near 1) can still be
timesharing one core with several other benchmarks while `mpstat` shows the
other fifteen idle. Every measurement in this document is pinned to cpu 15 and
was taken with `mpstat -P ALL` confirming cpu 15 at 98-100% for this process
alone.

That correction is applied to `BENCHMARK.md` and `README.md` as part of this
change. It is a *documentation* fix — no code caused it and none fixes it.

## Root cause

With the real starting point established, `perf record -F 299 --call-graph
dwarf` over the 10M phase showed the kernel is fully JIT-compiled (the OSR body
reaches `jit_hashmap_put_direct` / `jit_integer_value_of_direct` — no
interpreter dispatch symbols appear at all), and that the time went to two
different kinds of waste.

**1. The Integer-keyed dense overlay maintained a second full hash table.**
`DenseIntEntries::note_fresh_insert` — four field updates and one
`FxHashMap<i32, u64>` insert — was **19.7%** of the phase on its own, second
only to its caller. That map existed solely so
`keys_in_java_hashmap_order` could reproduce real bucket-chain iteration order.
For the dominant shape — monotonically inserted non-negative keys — insertion
sequence *equals the key*, so the table was storing a value it could derive.
Its 10M cache-missing stores also showed up a second time in the caller: the
mutex unlock's `xchg` (a full store-buffer drain) carried **89.7%** of
`try_hm_int_fast_put`'s own samples, which is what a barrier standing in front
of ten million pending cache-missing stores looks like.

**2. Per-object fixed taxes, each a call/return pair around a no-op.** Once
the overlay stopped dominating, the profile flattened into small costs that
every object touch pays:

| symbol | share of phase | what it was doing |
|---|---:|---|
| `GenerationalHeap::get_header` | 7.0% | out-of-line call per header read; both diagnostics it hosts are default-inert |
| `record_object_ref_payload` | 6.1% | two-level provenance bitmap walk per `ObjectRef` construction |
| `jit_checkcast` + `jit_typecheck_resolve` + `str::from_utf8` | 4.5% | re-deriving a monomorphic site's answer, including UTF-8 validation of the class name, every call |
| `enforce_single_os_thread` | 1.4% | emitted as a real call although disarmed |
| `a2dbg::record` | 1.2% | seven-argument call whose first statement is `if !enabled() { return }` |

## Fix

Inherited from the branch's earlier commits (`806ac07cd`, `a4dfc55fc`):

- `DenseIntEntries` derives the insertion sequence of a dense key from the key
  itself and keeps a `dense_seq_overrides` side table for only the out-of-order,
  migrated or reinserted keys. A sequential generated-ID map leaves it empty.
  The sparse arm carries its sequence inline in the entry instead.
- A sequential-append fast path: when the key is exactly the next dense slot and
  no sparse entry can own it, push straight onto the vector, skipping `resize`,
  the occupied-slot probe and the sparse lookup.

Added by this session:

- **The three arena-membership probes that `806ac07cd` had also dropped are
  restored** (`78090431b`). See "Correctness" below.
- `GenerationalHeap::get_header` is inline; both gated diagnostic bodies moved
  to a `#[cold] get_header_diagnostics`.
- `record_object_ref_payload` keeps a per-thread `(4 KiB block, granule bits
  known set)` memo. Recording is idempotent and the bitmap never clears, so a
  remembered bit means the global store already happened — and consecutive TLAB
  allocations share a block for roughly a hundred objects. The steady state is
  two shifts, a compare and a bit test.
- `enforce_single_os_thread` is `inline(always)` with the armed branch outlined;
  `a2dbg::record` likewise.
- `jit_checkcast` / `jit_instanceof` gained a move-to-front positive-answer memo
  keyed on `(vm, site name ptr/len, receiver class id, lenient)`, consulted
  *ahead of* the UTF-8 validation. Its soundness argument is
  `JIT_SUBTYPE_POSITIVE_CACHE`'s — only `true` is memoized, because a class's
  supertype set is fixed at definition while a `false` can be observed before
  the hierarchy is fully populated — plus two receiver conditions: no class
  redefine in flight, and the receiver must not be an array (a reference
  array's header stores its *component* class id, which the array branch of
  `jit_typecheck_resolve` never consults).

## Correctness

### The inherited safety relaxation, split

`806ac07cd` removed `vm.mem.heap.is_object_address` from four places on the
grounds that the verifier and the call site's oop map already prove the value
is a live oop. That argument holds for exactly one of them:

* **Kept removed** — the exact-`HashMap` receiver in `jit_hashmap_get_direct`
  and `jit_hashmap_put_direct`. `jit_hashmap_receiver_is_exact` has already
  read the receiver's class word through a raw `std::ptr::read` by that point,
  so a later probe cannot prevent a dereference that has already happened.
* **Restored** — `jit_integer_int_value_direct`'s receiver,
  `jit_hashmap_get_direct`'s key, and `jit_hashmap_put_direct`'s key and value.
  Each of those is dereferenced for the *first* time downstream (`get_field`,
  and `unbox_wrapper`'s `class_id_of`). The alignment and canonical-address
  tests that remain reject wild bit patterns but accept any plausible-looking
  non-object address, so the probe was the only thing between a stale or
  fabricated argument and that dereference.

The restoration was measured, not assumed: four interleaved cycles, cpu 15,
10M, medians 1,976 ms without the probes and 2,037 ms with them — about 3% of
wall time, or 6% of the remaining gap. That is cheap insurance and it is kept.

### Tests

<!-- FILLED IN BELOW -->

## Performance evidence

<!-- FILLED IN BELOW -->

## Artifacts

All binaries are fat-LTO release builds from the isolated task worktree
`/data/data/wt-hashmap-halfgap-20260730` on the Azure EPYC bench host, with
task-unique names under `/data/data/bin-hmhg/`.
