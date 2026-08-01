# The old-gen mark accepted unvalidated addresses: an invalid `kind` byte in the scan, and a silent live-payload corruptor on the same path

## Status

**FIXED** 2026-07-31, branch `fix/oldgen-mark-validation-20260731`. Diagnosed
the same day on `invest/invalid-kind-oldgen-20260731` (dev `54de12bb98`), which
filed this record as OPEN with two `#[ignore]`d tests; both now pass and the
`#[ignore]`s are gone.

This closes the question left open by name in
`docs/internal/fixed-suite-bugs/gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md`
— "why a header with an invalid `kind` byte is reachable by the old-gen scan at
all". That commit made the condition survivable; this one removes it.

## The defect

Two independent gaps composed:

1. **A liveness predicate that lied.** `VmHeap::is_addr_live` treated *any*
   old-gen address as live.
2. **Nine push sites into the old-gen mark worklist, seven validating nothing.**

(1) manufactured dangling old-gen addresses inside long-lived side tables;
(2) let them through to `scan_object_for_old_refs`, which decoded whatever bytes
were at that address as an `ObjectHeader`.

### Half 1 — `is_addr_live` reported freed old-gen blocks as live

`VmHeap::is_addr_live` was `is_old_gen_addr(addr) || is_live_young_survivor(addr)`,
and `GenerationalHeap::is_old_gen_addr` is a bare range check over the old-gen
backing `Vec`. Its doc comment justified that:

> A minor collection never touches the old generation, so every old-gen object
> is live.

**The premise was false.** `sweep_old_gen_non_moving` runs
`old_gen_gc(compact = false)` — a full old-gen mark-sweep that reclaims dead
blocks **in place** — and it runs during a *young* collection, whenever a live
JIT frame blocks the moving young collector. Its own doc comment said so:

> this sweep frees dead old-gen blocks in place, so after it runs "the address
> is inside old gen" no longer implies "the object is still there".

Two doc comments in one crate contradicting each other, and the contradiction
was the bug. `OldGen::free` returns the block to a size-segregated free list and
does not zero it (only `alloc` zeroes, and only `compact` zeroes a freed tail),
so the dead object's bytes stayed in place and the address kept passing
`contains`.

Every collection in the failing H2 window took that path — the
`TestOutOfMemory` log shows `[moving-young] fallback:
reason=unregistered-jit-frame-on-stack` on every cycle.

Consequence: `interpreter.rs`'s `is_marked` closure
(`pointer_map.contains_key(addr) || is_addr_live(addr)`) is handed to
`prune_external_roots`, `reconcile_class_mirrors` -> `rebuild_mirror_pins`, and
`gc_reconcile_defining_loaders`. For an old-gen address it was constant `true`,
so **no old-gen entry was ever pruned from any of them** — a promoted object's
side-table entry was immortal even after the sweep freed it.

### Half 2 — seven of nine push sites validated nothing

| # | site | before |
| --- | --- | --- |
| 1 | root seed | validated |
| 2 | `mark_young_to_old_refs` conservative word scan | validated (exact base match) |
| 3 | `mark_young_to_old_refs` ref slots | **no** |
| 4 | external overlay, young owner | **no** |
| 5 | external overlay, BFS owner | **no** |
| 6 | `loader_pin` | **no** |
| 7 | `mirror_pin` | **no** |
| 8 | `metadata_pin` | **no** |
| 9 | `scan_object_for_old_refs` ref slots (the BFS) | **no** |

Sites 3-9 all reduced to a bare range check plus a blind
`gc_flags |= GC_FLAG_MARKED` read-modify-write, and `for_each_ref_slot` hands
raw slot words straight through as pointers. Both validated sites' comments
already named this hazard as their reason for existing; site 2's is verbatim
*"an interior/colliding word would otherwise get a mark-bit write into a live
object's payload and feed a garbage 'header' into the BFS"*.

**This corrupted live data.** Pointing one reference slot at an 8-aligned
interior address of a live old-gen object whose four `int` fields were all
`0x41414141` and running the old-gen mark gave:

```
victim payload: [0x43414141, 0x41414141, 0x41414141, 0x41414141]
                    ^^ |= 0x02 (GC_FLAG_MARKED)
```

A Java `int` silently changed from 1094795585 to 1128350017. No crash, no
diagnostic. The same run decoded those payload bytes as an `ObjectHeader`, which
is the invalid-`kind` path.

## The fix

**Half 1 — `gc/src/old_gen.rs`, `gc/src/gen_heap.rs`, `gc/src/vm_heap.rs`.**

* New `OldGen::is_allocated_addr(ptr)` — `contains(ptr)` *and* not inside a free
  block. O(log n) via `partition_point` over the offset-sorted free-block view
  `walk_objects` already caches, so it costs nothing extra to maintain.
* New `GenerationalHeap::is_live_old_gen_addr(addr)`, the strict counterpart of
  `is_old_gen_addr`.
* `VmHeap::is_addr_live`'s Generational arm now uses it.

`is_old_gen_addr` itself is deliberately **unchanged**. It answers "which
generation is this address in?", which is a different question and one
`metadata_pin_deferrable` / `mirror_pin_deferrable` legitimately ask; two
existing tests also assert the permissive form as a precondition. Conflating the
two questions is what caused this in the first place.

**Half 2 — `gc/src/gen_heap.rs`.**

* New `old_gen_mark_candidate_plausible(ptr, old_gen, conservative)` — the root
  seed's screen, factored out: 8-alignment, old-gen containment, `kind` byte
  (compared **raw**, since an out-of-range discriminant is the case it exists to
  catch) in `0..=1`, `num_slots`/`array_length` bounds,
  `header_reserved_fields_plausible`, and an extent that fits inside old gen.
* New `mark_and_push_old_gen(ptr, old_gen, worklist, site)` — the single mark +
  push transition. All seven unvalidated sites now call it; the root seed calls
  the screen directly with `conservative = true`.
* New counter `OLDMARK_REJECTED_CANDIDATES` plus a capped `tracing::warn!`
  naming the site, because a *precise* reference failing this screen means
  something upstream is holding a stale address.

### Why `conservative` is a parameter

The zero-word0 neighbour disambiguation (`victim8_neighbor_explains_zero_prefix`)
is applied **only** to register/stack-scanned guesses. A `ClassId(0)` ad-hoc
container legitimately has an all-zero word0, and at a precise site a false
reject is a **premature free** — strictly worse than the corruption being fixed.
Every other clause is a property real objects always have, so precise sites take
all of them with no false-reject risk.

## Validation

* `cratonvm-gc --lib`: **883 pass / 0 fail / 0 ignored** (880 before, plus the
  two un-ignored defect tests and one new predicate test).
* `cratonvm-vm --lib`: **2328 pass / 1 fail**, byte-identical to the baseline on
  this branch point. The one failure is
  `runtime::interpreter::tests::hot_files_have_no_production_panics`, which is
  pre-existing and complains about `jit/src/x64/disp.rs` — a file this change
  does not touch. Confirmed by stashing and re-running, and by three consecutive
  full runs.
* Tests:
  * `non_moving_old_sweep_frees_in_place_and_is_addr_live_says_so`
  * `old_gen_mark_rejects_an_interior_address_instead_of_decoding_it` (asserts
    both the payload integrity and that nothing was decoded as a header)
  * `old_gen_is_allocated_addr_distinguishes_freed_blocks_from_live_ones`
* Workload, `--Xmx 512m`, JIT on: `MemFsHeavyProbe` **4/4 clean**; the real
  `org.h2.test.db.TestOutOfMemory` **2/2**, both ending in the class's ordinary
  `AssertionError` (what HotSpot does), 0 SIGSEGV.
* `cargo clippy -p cratonvm-gc --all-targets`: no new warnings. `cargo fmt
  --check` diff count unchanged from baseline (26).

**Read this before trusting a workload result.** The screen logged **zero
rejections** across all six runs, so the workload did not exercise the new path
— it demonstrates *no regression*, not that the fix fires. That is consistent
with the diagnosis session, where nine runs failed to reproduce the original
condition at all (it is rare and clustered: the one original sighting was five
hits in a single run). The deterministic tests are the evidence that the fix
works; the workload runs are the evidence that it breaks nothing.

## Follow-up worth doing

`OLDMARK_REJECTED_CANDIDATES` is now the tripwire. If it ever goes non-zero on a
real workload, the warn names the push site, and that identifies *which* side
table is still handing out stale addresses — the remaining question is whether
Half 1 catches every producer of them, or whether some table also needs an
explicit shape-based prune (the `watched_pre_gc_addr_survived` mechanism already
does this for the reference processor).

## See also

* `docs/internal/fixed-suite-bugs/gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md`
  — the SIGSEGV this explains and closes.
* `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted-FIXED.md`
  — the workload.
