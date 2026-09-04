# The ZGC relocation slides wrote into granules the give-back had returned

## Status

**FIXED 2026-09-04.** Retired from
`known-issues/jit/bug-box-unbox-intrinsic-segv-under-relocation-20260902.md`,
which named it "the box/unbox intrinsic SIGSEGVs under a relocating
collector". It is neither the intrinsic's fault nor relocation's, and the
intrinsic is default-ON again.

## What the page had, and what was right about it

`org.h2.test.store.TestRandomMapOps --Xmx 256m`, family on: **SIGSEGV, 11/11**
at 25–183 s when the page was written, and **3/3 in 7 s** once the unrelated
`op:1033` wrong answer stopped ending the run first. Fault address always a
page boundary, `rdi` equal to it, fault pc inside libc.

That description is exact, and it is the whole diagnosis. A `memmove` whose
destination register *is* the faulting page boundary is a copy that ran off the
end of a mapping — not a read through a stale reference, which is what the
page inferred.

## The switch table that misled, and the third switch

The page had two switches that each removed the crash:

| arm | SIGSEGV |
|---|---|
| shipped default (family on) | **11 / 11** |
| `CRATONVM_ZGC_RELOCATE=0` | 0 / 3 |
| `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | 0 / 3 |

and read them as *"it takes BOTH relocation and this intrinsic"*. A later pass
added `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` (0/3) and concluded the stale
value was a reference in a live compiled frame that relocation moved without
rewriting — "a root the safepoint's oop map does not name".

There is a third switch, and it is under both of the first two:

| arm | SIGSEGV | note |
|---|---|---|
| `CRATONVM_JIT=box-unbox-intrinsic` | **3 / 3**, 7 s | positive control, same batch |
| ...`+ CRATONVM_GC_RESERVE=0` | **0 / 3** | |

`CRATONVM_GC_RESERVE=0` forces the wholly-committed `alloc_zeroed` backing
store. It has nothing to do with the JIT and nothing to do with oop maps.

**The lesson: a pair of switches that each remove a crash identifies two
ingredients, not a mechanism.** Both of the page's switches are downstream of
the real one — relocation is what performs the copy, and the intrinsic is what
changes the allocation shape enough to make the *high* slide run at all.

## Root cause

gdb, on the 7-second repro:

```
#0 __memcpy_avx512_unaligned_erms
#1 core::ptr::copy<u8>
#2 cratonvm_gc::zgc::…::compact_high_region   gc/src/zgc.rs:5462
#3 cratonvm_gc::zgc::…::relocate_stw          gc/src/zgc.rs:6409
```

and `info proc mappings` at the fault shows `---p` runs *inside* the arena's
own reservation.

`Arena::decommit_free_blocks` returns free-list granules to the OS. Its doc
gave the reason that was safe:

> every path that hands one out again goes through `hand_out`, which commits
> before it returns a pointer

Both of this collector's slides are a path that hands one out and does **not**.
A slide picks its destination inside free space arithmetically and `memmove`s
into it — never asking the allocator for anything is the whole point of a
slide. So the first slide after a give-back writes into a `PROT_NONE` granule.

The high slide is the one that fires here because it packs survivors against
`capacity`, and the top of the arena is exactly where a give-back has most
recently been. The low slide has the identical exposure.

## The fix

`Arena::commit_for_relocation` is the door such a caller needs, and both slides
now go through it. A refused commit is not a reason to write anyway: the high
slide treats the survivor as immovable, the low slide takes its existing
"nowhere below it, so it stays put" branch. A slide that cannot have its
destination is a missed compaction, never a corrupt heap.

The stale promise in `decommit_free_blocks`'s doc is corrected in the same
change — it is the sentence that made this look safe.

## The intrinsic is default-ON again

The mitigation existed for this crash and nothing else. With the slides fixed:

| arm | runs | SIGSEGV |
|---|---:|---:|
| `CRATONVM_JIT=box-unbox-intrinsic`, 1200 s cap | 3 | **0** |
| `CRATONVM_JIT=box-unbox-intrinsic`, 400 s cap | 3 | **0** |

What those runs end on instead is a `NullPointerException` at a **later** seed
(`seed:4604578705726772870`), and it appears identically with the family OFF —
so it is not this page's defect. It is the pre-existing failure
`known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`
records, which the page itself listed as pre-dating its bisect range.

`box_unbox_is_opt_in_until_the_relocation_defect_is_closed` asked to be
inverted when this page closed, "so the flip has to be deliberate". It is
inverted and renamed.

## What this retires from the page, item by item

| the page said | now |
|---|---|
| "it takes BOTH relocation and this intrinsic" | Both are ingredients; the cause is the uncommitted destination. |
| "a root the safepoint's oop map does not name" | No stale root is involved. `RELOCATE_UNDER_PROVEN_JIT=0` worked by removing the slide, not by fixing a root. |
| "the receiver must already be stale when it is LOADED" | It is not. The fault is a WRITE, in the collector, with no mutator frame involved. |
| bisect first-bad `a910b7d9c`, a merge with two good parents | Consistent: the give-back and the slide arrived from different lines. |
| "the repro is currently BLOCKED by an earlier failure" (`op:1033`) | Cleared — see `guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md`. |
| `probes/BoxUnboxReloc.java` / `…MT.java` found nothing over 5M relocations | Correct, and now explained: neither drives the HIGH slide, which needs large-object allocation and a give-back. |

That last row is worth keeping. Both probes were carefully built, their
ingredients were measured as engaged, and they were still blind — because the
ingredient list came from the hypothesis. `objects_relocated=3.2M` proves
relocation ran; it says nothing about `compact_high_region`, which has its own
counter (`high_compaction_cycles`) that neither probe read.
