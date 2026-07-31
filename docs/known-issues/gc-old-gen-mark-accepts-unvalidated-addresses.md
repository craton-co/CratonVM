# The old-gen mark accepts unvalidated addresses: how a header with an invalid `kind` byte reaches the scan (and a silent payload corruptor on the same path)

## Status

**OPEN.** Diagnosed 2026-07-31 on branch `invest/invalid-kind-oldgen-20260731`.
Two `#[ignore]`d tests in `gc/src/gen_heap.rs` pin the two halves; both fail on
demand:

```bash
cargo test -p cratonvm-gc --lib -- --ignored non_moving_old_sweep old_gen_mark_accepts
```

This record answers the question left open by
`docs/internal/fixed-suite-bugs/gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md`
("why a header with an invalid `kind` byte is reachable by the old-gen scan at
all"). That commit made the condition *survivable*; this one explains it.

## The short version

Two independent gaps compose:

1. **A liveness predicate that lies.** `VmHeap::is_addr_live` treats *any*
   old-gen address as live. That is only true if minor GCs never collect old
   gen — and on the non-moving path they do, in place.
2. **Nine push sites into the old-gen mark worklist, six of which validate
   nothing.** The root seed was hardened against exactly this hazard in
   2026-07-03 and the hardening was never carried to its siblings.

(1) manufactures dangling old-gen addresses inside long-lived side tables;
(2) lets them through to `scan_object_for_old_refs`, which decodes whatever
bytes are at that address as an `ObjectHeader`.

## Half 1 — `is_addr_live` reports freed old-gen blocks as live

`VmHeap::is_addr_live` (`gc/src/vm_heap.rs:1873`) is:

```rust
VmHeap::Generational(h) => h.is_old_gen_addr(addr) || h.is_live_young_survivor(addr)
```

and `GenerationalHeap::is_old_gen_addr` (`gc/src/gen_heap.rs:3489`) is
`self.old_gen.lock().contains(addr as *const u8)` — a bare range check over the
whole backing `Vec`. Its doc comment justifies that:

> Used by weak/soft/phantom reference processing after a **young** GC ... A
> minor collection never touches the old generation, so every old-gen object is
> live.

**That premise does not hold.** `sweep_old_gen_non_moving`
(`gc/src/gen_heap.rs:8424`) runs `old_gen_gc(.., compact = false)` — a full
old-gen mark-sweep that reclaims dead blocks **in place** — and it runs during a
*young* collection, whenever a live JIT frame blocks the moving young collector.
Its own doc comment says so:

> this sweep frees dead old-gen blocks in place, so after it runs "the address
> is inside old gen" no longer implies "the object is still there".

The two comments contradict each other. `OldGen::free` returns the block to a
size-segregated free list and does not zero it (only `alloc` zeroes, and only
`compact` zeroes a freed tail), so the dead object's bytes stay in place and the
address keeps passing `contains`.

This matters because **every collection in the failing window took that path** —
the H2 `TestOutOfMemory` log shows `[moving-young] fallback:
reason=unregistered-jit-frame-on-stack` on every cycle.

### What that breaks

`interpreter.rs:2207` builds

```rust
let is_marked = |addr: usize| -> bool {
    pointer_map.contains_key(&addr) || shared.mem.heap.is_addr_live(addr)
};
```

and hands it to the side-table reconcilers:

| consumer | table |
| --- | --- |
| `prune_external_roots` | overlay-backed collection roots |
| `reconcile_class_mirrors` -> `rebuild_mirror_pins` | `class_mirrors`, `mirror_pin` |
| `gc_reconcile_defining_loaders` | defining-loader map, feeds `loader_pin` |

For an old-gen address this predicate is constant `true`, so **no old-gen entry
is ever pruned from any of them.** Once an object is promoted, its side-table
entry is effectively immortal regardless of whether the old-gen sweep freed it.

Note the correct predicate already exists in the tree —
`VmHeap::watched_pre_gc_addr_survived`, backed by the watched-survivor identity
map that `sweep_old_gen_non_moving` returns for precisely this reason — but it
is wired only to the *reference processor's* `is_marked` (`interpreter.rs:2323`,
behind `weakref_clear_enabled()`), not to the side-table prune above. Same
"fixed at one site, never carried to the siblings" shape as the SIGSEGV this
record follows.

Pinned by
`non_moving_old_sweep_leaves_is_addr_live_reporting_a_freed_block_as_live`.

## Half 2 — six of nine worklist push sites validate nothing

`old_gen_gc` seeds and drives a mark worklist. The sites:

| # | site | validated? |
| --- | --- | --- |
| 1 | root seed (`roots`) | **yes** — alignment, `kind <= 1`, slot/length bounds, `header_reserved_fields_plausible`, zero-word0 neighbour check, extent fits |
| 2 | `mark_young_to_old_refs` conservative word scan | **yes** — exact match against `old_gen.walk_objects()` bases |
| 3 | `mark_young_to_old_refs` normal ref slots | no |
| 4 | external overlay, young owner | no |
| 5 | external overlay, BFS owner | no |
| 6 | `loader_pin` | no |
| 7 | `mirror_pin` | no |
| 8 | `metadata_pin` | no |
| 9 | `scan_object_for_old_refs` ref slots (the BFS itself) | no |

Sites 3-9 all reduce to:

```rust
if old_gen.contains(ref_ptr) {
    let ref_header = &mut *(ref_ptr as *mut ObjectHeader);
    if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
        ref_header.gc_flags |= GC_FLAG_MARKED;
        worklist.push(ref_ptr);
    }
}
```

No alignment test, no header plausibility test — just the range check. And
`for_each_ref_slot` hands raw slot words straight through as pointers.

Site 1's comment already spells out why this is dangerous, in the context of its
own fix:

> `OldGen::contains` is a bare bounds check (no alignment, no header
> validation), so before this fix ANY garbage address landing inside old gen's
> byte range got `gc_flags` blindly RMW'd — the exact same corruption family, on
> the OTHER generation.

Site 2's comment says the same thing even more directly — "base validation is
mandatory (`OldGen::contains` is a raw range check, so an interior/colliding
word would otherwise get a mark-bit write into a live object's payload and feed
a garbage 'header' into the BFS)". Both describe the bug that sites 3-9 still
have.

### This is a silent data corruptor, not only a diagnostic nuisance

`old_gen_mark_accepts_an_interior_address_as_an_object_header` points one
reference slot at an 8-aligned **interior** address of a live old-gen object
whose four `int` fields are all `0x41414141`, then runs the old-gen mark. After
the mark:

```
victim payload: [0x43414141, 0x41414141, 0x41414141, 0x41414141]
                    ^^ |= 0x02  (GC_FLAG_MARKED)
```

The collector wrote its mark bit into a **live object's payload**, changing a
Java `int` field from `1094795585` to `1128350017`. No crash, no diagnostic —
the value is simply wrong from then on. The same run also increments
`OLDMARK_BAD_KIND_HITS`, i.e. those payload bytes were decoded as an
`ObjectHeader` — which is the answer to the original question.

## Why the observed kind bytes look the way they do

The bytes recorded in the SIGSEGV record were `0x04`, `0x16`, `0xc6`, with
`shape=512, class_id=4` recurring:

```
GC: header kind=0x04 reached the legacy-object sizing arm (shape=0,   class_id=6021)
GC: header kind=0x04 reached the legacy-object sizing arm (shape=512, class_id=4)
GC: header kind=0x16 reached the legacy-object sizing arm (shape=0,   class_id=6004)
GC: header kind=0xc6 reached the legacy-object sizing arm (shape=512, class_id=4)
```

Two things follow from the mechanism above and are worth recording, because both
contradict the guesses written into `gen_object_total_size`'s own comment:

* **They are not `GAP_FILLER_CLASS_ID` sentinels.** That sentinel is
  `ClassId(0xF111E701)`; the observed `class_id` values are 4, 6004 and 6021.
* **They are not the 8/16/24/32 gap-length low bytes** the comment predicts
  either.

Structured, *recurring* values are what you expect from reading real object
payload at a non-base address, not from reading random freed bytes — consistent
with the interior-address case the test reproduces. Note also that
`0x0000020000000000` (= `identity_hash=0` ‖ `array_length=512`, i.e. exactly the
`shape=512` entries) is already named in site 1's comment as the fabricated
pointer behind an earlier crash face, so this byte pattern has form.

## Reproduction status — read this before trusting a null result

The condition is **rare and clustered**. The original observations came from a
single H2 `MemFsInsertProbe` run that hit it five times; **nine further runs on
2026-07-31 (1 x 3-round light, 1 x heavy, 5 x heavy in parallel, 2 x forced
`System.gc()`) reproduced it zero times.** Do not read a clean workload run as
evidence that this is fixed.

Two traps found while trying:

* The plain probe **never runs a major GC at all** — `old_gen_gc` needs old gen
  at >=75% capacity or an explicit `System.gc()`. A run that never crosses either
  exercises none of this.
* `gen_object_total_size` has ~26 call sites, so the
  "reached the legacy-object sizing arm" message alone does not identify the
  walker. Capture a real backtrace at the funnel instead of assuming the caller.

The deterministic tests exist because the workload would not cooperate; they are
the reliable instrument here.

## The fix (not applied)

Two changes, independently useful:

1. **Make old-gen liveness shape-based, not range-based.** An address inside a
   free-list block is not live. `OldGen` can answer this directly; the cheap
   form is a `free_block_containing(ptr)` lookup over the already-cached
   offset-sorted free-block view (`with_sorted_free_blocks`), used to gate
   `is_old_gen_addr`. Blast radius is real — reference processing, weak-ref
   clearing and class unloading all consume this predicate — so it wants its own
   change and its own validation pass.
2. **Carry the root seed's screen to the other seven push sites.** The
   predicate already exists in-tree (alignment + `kind <= 1` + slot/length
   bounds + `header_reserved_fields_plausible` + extent-fits); factor it into
   one helper and apply it everywhere a worklist push happens. Site 1's comment
   already argues that rejecting is the safe response: "a failing candidate was
   never a valid object to begin with", and old gen has no side-mark escape
   hatch, so marking a garbage address is strictly worse than skipping it.

(2) alone stops the payload corruption and the invalid-kind decode. (1) is what
stops the dangling entries being manufactured in the first place.

## See also

* `docs/internal/fixed-suite-bugs/gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md`
  — the crash this explains; made the condition survivable and left this
  question open by name.
* `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted-FIXED.md`
  — the workload.
