# ✅ FIXED — `DefaultCatalogAndSchemaTest`: the low end was not un-compactable, it was being ground down by the starved TLAB rung

## Status

**RESOLVED 2026-08-30** on `fix/zgc-low-end-frag-and-spring-hang-20260830`.

Retires `zgc-low-end-fragmentation-defaultcatalogandschema-20260829.md`, whose
three open questions are all answered below — and whose own four-arm table
already contained the answer, mislabelled.

| | before | after |
|---|---|---|
| `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`, `--Xmx 1500m` | **4 × `OutOfMemoryError`**, no `@@RESULT` | **`found=132 ok=132 failed=0`** |
| real HotSpot, same heap | `ok=132`, 30 s | unchanged |

## The defect

`recycled_chunk_size`'s **starved rung** takes `largest_low_free` — the arena's
LARGEST low free block — for a **thread-private** TLAB buffer, accepting
anything down to `want / 64` (8 KiB), whenever the bump is out of headroom.

Once the bump is gone, *every* refill in the process qualifies. So the large end
of the free list is consumed continuously, and on a heap whose live objects wall
every span nothing replenishes it: `largest_free_block` walks down to the rung's
own floor and stays there.

The failing allocation lands exactly in the band the rung has been grinding:

```text
zgc: arena allocation failed  request=16400  used=1572849576  capacity=1572864000
     free_list_bytes=904895136  largest_free_block=16224  free_spans=39651
```

`request=16400` is a `char[8192]`. It is **TLAB-eligible** — `max_tlab_alloc` is
32 KiB — so it reached the shared arena only because its own buffer could not
serve it and the refill could not either. 905 MB free, and not one contiguous
16 400 bytes.

## The cause matrix

The page this retires had a pass/fail matrix and said so: *"the four-arm table
above is a pass/fail matrix, not a cause matrix."* This is the cause matrix.
Same class, same binary, one arm each:

| arm | result |
|---|---|
| default | 4 × OOM, no `@@RESULT` |
| `CRATONVM_ZGC_RELOCATE=0` | 4 × OOM, `largest_free_block` **identical** |
| `CRATONVM_ZGC_TARGETED_COMPACTION=1` | 4 × OOM (1 window recorded, **0 consumed**) |
| **`CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0`** | **`ok=132/132`**, 423 s |
| `CRATONVM_ZGC_TLAB=0` | `ok=132/132`, 432 s |
| `--Xmx 3000m` | `ok=132/132`, 426 s |
| HotSpot, `-Xmx1500m` | `ok=132/132`, 30 s |

**The answer was already in the old page's own table**, as
`tlab0 / both0 → frag=1 oom=0`, written off as *"they die EARLIER and
differently. Not characterised."* They do not die. A zero in an OOM column is
worth thirty seconds of `grep @@RESULT` before it is called an earlier death.

## The fix

The starved rung now also requires a free block of at least
`ZGC_LARGE_OBJECT_MIN` **besides** the one on offer.

The reserve is **derived, not tuned**: every direct arena allocation is smaller
than that constant by construction — at or above it the request is served from
the arena's other end — so one free block that size is exactly what "the shared
path can still be served" means. `has_free_block_at_least` cannot answer it for
a caller about to consume the largest block, which is why
`Arena::low_free_blocks_at_least` is a count rather than a predicate.

The **preferred** rung (`want / 8` and better) is untouched. There the block is a
retired chunk coming back one survivor short — the shape the whole function was
written for — and gating it would re-open the `TestNonBlockingAPI` failure it
exists to close.

## The three questions the old page left open

**1. "Whether the low end can be compacted at all in the current design."**
It can, and the machinery is correct; it is never given the chance.
`CRATONVM_ZGC_TARGETED_COMPACTION=1` records a window and reports
`in_low_region=true` — unlike the H2 case that made the flag ship default-off,
where the window was above `used_low`. But the target is **never consumed**:
relocation itself is declined on 13 of 14 cycles.

Why it declines was unreadable, because `relocation_skipped_jit` is a count of a
five-term conjunction, and the generational collector's own reason census
(`moving_young_fallback_reason`) is structurally inert on this collector —
`gc_quiescence::moving_young_incomplete_reason_mask`'s doc says so in as many
words, having already cost one build. A per-term census now answers it:

```text
[GC] zgc-relocation-skip-reason: coverage-proof-incomplete=13
[GC] zgc-relocation-coverage-reason: unregistered-jit-frame-on-stack=13
```

Not the `CROSS_THREAD_JIT_PEER` limit the source comment predicted — an
unguarded compiled frame on the collecting thread's own stack.

**And the designated follow-up for that is priced, and it is not the answer.**
`native_stack_jit_frame_census` exists precisely to say whether the frame-shape
filter would convert a cycle before anything is built on it. Measured here:

```text
hits=6 shaped=1   × 8      hits=6 shaped=4      hits=5 shaped=3
hits=4 shaped=1            hits=2 shaped=1      hits=1 shaped=0
```

`shaped >= 1` on **12 of 13** refusing cycles, so the filter converts exactly
one. The refusal is not being weakened on that evidence: a slide under a
compiled frame whose oops nothing can rewrite corrupts the heap, and
fragmentation only wastes it.

**2. "Why `JacksonJsonFormatMapper` sits in the middle of the arena."**
It is not placement, and it is not one object. `wall_bytes=32 walls=1` describes
the *cheapest* window, which is what that report is for; the whole-heap shape in
the same run is `spans=1192792 walls=1192791 wall_bytes=666920264` — 666 MB of
live data in 1.19 M walls averaging 559 bytes, interleaved with 905 MB of free
spans averaging 758. A genuine live/dead mosaic. Relocating that one 32-byte
object would have served that one request; the next request would have found the
same heap.

**3. "The `tlab0` / `both0` arms die earlier and differently."**
They pass. See above.

## Instruments added, because each of these zeroes was unreadable

* `zgc-relocation-skip-reason` / `zgc-relocation-coverage-reason` — which of the
  five gate terms refused, and which obligation the coverage proof failed on.
* `targets_recorded` / `targets_consumed` beside `targeted_pages` — that counter
  reads 0 both when no window was ever named and when every named window went
  unconsumed, and the two want opposite repairs.
* `tlab_recycled_refills` / `tlab_starved_refills` / `tlab_starved_bytes` — the
  starved rung shipped as a switch nothing reported on, so "never reached" and
  "reached constantly" were the same run to a reader.

## One more defect found on the way

`record_compaction_target` returned early while a target was outstanding, with no
generation stamp — so a target nothing ever consumed suppressed every later
re-derivation for the life of the process. It now carries the `gc_count` it was
recorded at: a later failure in the same generation still reuses it (the early
return's purpose is kept), a failure in a later generation re-derives. That is
also the *right* window, for the reason `frag_report_once` reports twice: the
post-collection arena is the only view that says what survives a collection.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm --java-home <jdk25> --Xmx 1500m \
  -Duser.language=en -Duser.country=US -Djava.awt.headless=true \
  @common.args CratonRunner \
  org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
```

`common.args` in a stale checkout may name five module jars that a later
hibernate build replaced with `target/classes/java/main` + `target/resources/main`;
the class then dies on `ServiceConfigurationError: ... CheckClearSchemaListener
not found` on **both** VMs, which is a harness fault and not this one. Check the
classpath entries exist before reading anything into a failure.
