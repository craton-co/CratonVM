# The non-moving young sweep made GC-driven `Reference` enqueue impossible

## Status
**✅ FIXED 2026-08-07.** One condition in
`VmHeap::pre_gc_addr_did_not_survive` (`gc/src/vm_heap.rs`): the Generational
arm called a young address dead whenever it was absent from the collection's
pointer map. That is only equivalent to "dead" after a **moving** young GC.
After a **non-moving sweep** nothing moves, so the map is empty apart from the
identity entries the sweep emits — and it emits those only for watched
*referents*. A `Reference` object and its `ReferenceQueue` are neither, so both
were declared dead and post-GC reference processing skipped every clear and
every enqueue.

Witness: `probes/EnqProbe.java` goes `gc enqueued it = false` -> `true` on the
default collector, with and without the JIT. G1, which never had the bug, is
unchanged.

## The symptom

A `WeakReference` whose referent is unreachable was **cleared but never
enqueued**, so nothing ever came out of its `ReferenceQueue`:

```
explicit enqueue() = true      <- Reference.enqueue() by hand worked
gc enqueued it     = false     <- GC-driven enqueue did not
referent cleared   = true
```

HotSpot prints `true` for both. G1 (`-XX:+UseG1GC`) printed `true` for both,
4 runs of 4; the default Generational collector printed `false`, 3 of 3. That
split is what localised it — the defect is in the Generational post-GC path, not
in reference processing itself.

## Why it happened

`ReferenceProcessor::process_references` produced the right answer all along:
`result.to_enqueue` held `(reference_obj, queue_addr)`. The consumer threw it
away. `CRATONVM_DBG=straystack` shows the discard:

```
[refproc] SKIP dead CLEARED ref @0x20042400d48 (young, not in map)
[refproc] SKIP dead ENQUEUE ref@0x20042400d48/q@0x20042400cc8 (young, not in map)
```

Both addresses are live locals in `main`. The guard that rejected them
(`vm/src/runtime/interpreter/gc_and_alloc.rs`, `is_stale_young`) exists for a
real reason — it stops the proven `bc math-ec 0x4` writer, which writes a
cleared referent through a young address whose memory has already been reused —
and it asks `pre_gc_addr_did_not_survive`. That predicate documented its own
premise:

> a live young object is always in the map after a moving young GC, and
> non-moving sweeps emit identity entries for watched survivors

The first clause is true. The second is narrower than it sounds:
`gc_quiescence::is_watched_referent` is *"Is `addr` a currently-registered
Weak/Soft/Phantom **referent** this cycle?"*. The sweep identity-maps referents
because an earlier bug cleared references whose referent had survived in place.
Nobody extended it to the `Reference` objects and `ReferenceQueue`s that the
*writers* dereference — so on any non-moving sweep, those two were unmappable by
construction and every enqueue was skipped.

This matters far more than it sounds because **the non-moving sweep is not the
exceptional path**. On the H2 workloads it is the only path: every young
collection falls back to it (`reason=unregistered-jit-frame-on-stack`,
`compiled-frame-oop-not-published`, `innermost-rbp-belongs-to-unguarded-callee`).
For those runs, GC-driven enqueue never happened at all.

## The fix

G1 and ZGC already asked the right question in the same `match`:

```rust
VmHeap::Generational(h) => h.is_in_young_either(addr as *const u8),
VmHeap::G1(_) => !self.is_addr_live(addr),
VmHeap::Zgc(_) => !self.is_addr_live(addr),
```

and `is_addr_live`'s Generational arm is *already* built for this exact case —
`is_live_old_gen_addr(addr) || is_live_young_survivor(addr)`, whose comment
reads "recognize kept-in-place young survivors of the NON-MOVING sweep (which
produces no pointer_map entries), or reference processing judges every live
young Reference object/referent dead". The predicate existed; this one caller
was not using it. So:

```rust
VmHeap::Generational(h) => {
    h.is_in_young_either(addr as *const u8) && !self.is_addr_live(addr)
}
```

Keeping the young-space test in front preserves the old rule's strictness for
everything the moving collector governs: the `bc math-ec 0x4` writer is a young
address that is neither mapped nor live, and still answers `true`.

## Verification

* `probes/EnqProbe.java`: `false` -> `true` on the default collector, 3 of 3,
  and with `--nojit`. G1 control still `true`.
* `cargo test -p cratonvm-gc --release`: all green, including the guard's own
  family — `weak_reference_cleared_and_enqueued`,
  `weak_reference_preserved_when_referent_live`,
  `phantom_reference_enqueued_when_referent_dead`,
  `phantom_ref_not_re_enqueued`, `remove_collected_removes_dead_weak_refs`,
  `post_gc_relocation_updates_all_addresses`.
* First 40 H2 suite classes, A/B against the same dev tip without the fix, 4-way
  parallel, 180 s cap: **30 PASS both arms** (without: 30/3 FAIL/7 HANG; with:
  30/5/5). Three classes reported a different verdict —
  `TestAnalyzeTableTx` PASS->FAIL, `TestBigResult` HANG->PASS,
  `TestLargeBlob` HANG->FAIL — so all three were re-run **in isolation**,
  ABBA-interleaved, 6 runs per arm. Every one of them is flaky on *both* arms
  with overlapping distributions:

  | class | without the fix | with the fix |
  | --- | --- | --- |
  | `TestAnalyzeTableTx` | PASS 4 / FAIL 2 / HANG 0 | PASS 4 / FAIL 1 / HANG 1 |
  | `TestBigResult` | PASS 3 / FAIL 0 / HANG 3 | PASS 4 / FAIL 0 / HANG 2 |
  | `TestLargeBlob` | PASS 0 / FAIL 5 / HANG 1 | PASS 0 / FAIL 4 / HANG 2 |

  None of the three is a regression, and none is a recovery: they are the
  known load-sensitive flakes this suite produces near its timeout, and the
  parallel run's three "changes" were the host, not the fix. Recording the
  isolation numbers rather than the parallel ones because the parallel run on
  its own would have read as one regression and one fix, and it is neither.

## What it does NOT do

It does not by itself recover the five classes the monitor-quartet fix left
short of the pre-header-16 baseline. A/B on the three shapes named as most
likely to notice, ABBA-interleaved at `--Xmx 1g`:

| class | without the fix | with the fix |
| --- | --- | --- |
| `TestLob` | FAIL (`OutOfMemoryError: Java heap space`) | FAIL, same |
| `TestMemoryUsage` | PASS | PASS |
| `TestLIRSMemoryConsumption` | PASS | PASS |

So `TestLob`'s OOM is a separate defect that predates this fix and survives it,
and the other two were already green. The enqueue defect was real, is fixed, and
was **not** the whole of the five-class gap — that still needs its own hunt, and
`TestLob`'s OOM at 1 g is the obvious next thread to pull.
