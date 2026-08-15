# ZGC's own rewrite pass faults walking a reference array

**Status: OPEN, found 2026-08-15.** The collector SIGSEGVs inside itself,
during compaction's reference-slot rewrite. Distinct from the JIT-frame
relocation defect fixed the same day — this one reproduces with `--nojit`, so
no compiled frame is involved.

## The fault

```
Faulting access: read at address 0x000001DD2E9D0000     <- page-aligned
thread: "main-vm"
jit: guarded compiled frames live process-wide: no (quiescence depth=0)
jit: 0 compiled code range(s), cache generation 0
```

Symbolized against the matching binary (`CRATONVM_SYMBOLIZE`, PDB in place):

```
0x316B6F  cratonvm_gc::zgc::impl$23::reference_slots+0x1AF   gc/src/zgc.rs:7128
0x3119D1  cratonvm_gc::zgc::ZgcRealHeap::relocate_stw+0x3721 gc/src/zgc.rs:4114
0x306767  cratonvm_gc::zgc::impl$30::collect_garbage+0x6807  gc/src/zgc.rs:8582
0x389548  cratonvm_gc::vm_heap::VmHeap::collect_garbage_with_finalizers  vm_heap.rs:1509
```

`zgc.rs:4114` is the **rewrite pass** — the loop over `live_now` that re-points
every survivor's reference slots after the slide. `zgc.rs:7128` is inside
`reference_slots`' `ObjectKind::Array` arm, striding `array_length()` elements.

A **page-aligned** faulting address is the signature of walking off the end of
a mapped region, so the walk is reading past the object: either
`array_length()` is not this object's, or the base being walked is no longer
the object the walker thinks it is.

## Why that is possible here

`live_now` is built as

```rust
let live_now: Vec<usize> = live.iter().map(|b| record.get(*b).unwrap_or(*b)).collect();
```

`unwrap_or(*b)` keeps the ORIGINAL address for any live object the record does
not list — correct only if an unlisted object genuinely did not move **and its
memory was not written over**. The slide writes survivors downward into vacated
space; the relocation set is non-contiguous (the selector skips dense pages),
and the destination probe was taught to skip unselected pages on 2026-08-14.
The hypothesis to test first is whether an unlisted live object can still be
overwritten — in which case `live_now` hands the rewrite pass an address whose
contents are now some other object, and the array arm reads a length that was
never this object's.

That is a hypothesis, not a finding. It has not been measured.

## First things to try

1. **Assert before walking.** In the rewrite loop, check `registry.contains`
   and that the header's kind/length are self-consistent (`alloc_size` returns
   `Some`, and `base + size <= arena_hi`) before calling `reference_slots`.
   Report and skip rather than fault — the list of offenders is worth far more
   than the first crash, and it turns a SIGSEGV inside the collector into data.
2. **`CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`** already classifies post-slide dangling
   slots. It runs AFTER the rewrite, so it never gets to speak when the rewrite
   is what faults; a pre-rewrite variant of the same walk would.
3. **Check whether the object is in `moved_from`.** If the faulting base is an
   address the slide vacated, this is the overwrite hypothesis confirmed. The
   `CRATONVM_DBG_ZGC_CORPSE` ledger already records exactly that set.

## Rate

On `io.netty.util.ResourceLeakDetectorTest`, 10 interleaved reps each:

| arm | SIGSEGV |
|---|---|
| ZGC, JIT on | 6/10 |
| ZGC, `--nojit` | **2/10** |
| ZGC, `CRATONVM_ZGC_RELOCATE=0` | 0/10 |

The JIT-on excess is the separate relocation-under-a-live-compiled-frame defect
(fixed 2026-08-15). The `--nojit` residue is this one. Both vanish with
relocation off, so both are compaction defects.

`--nojit` is the cheap repro: it removes the other defect from the picture
entirely, at the cost of needing about five runs per hit.

## Related

- `docs/known-issues/netty/zgc-resourceleakdetector-corpse-read-20260815.md` —
  the JIT-frame half, and the instruments (`CRATONVM_DBG_ZGC_CORPSE`,
  `CRATONVM_DBG_JIT_NAMES`, `CRATONVM_SYMBOLIZE`) used to separate the two.
- The 2026-08-14 fix for the slide crossing unselected pages is the nearest
  neighbour of the hypothesis above; read it before assuming the destination
  probe is the problem, because that specific hole is closed.
