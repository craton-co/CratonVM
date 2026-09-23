# ZGC allocator constants — what each one bounds, and whether that thing is itself bounded

**Written 2026-08-13.** Phase 2.3 of
[`zgc-maturity-assessment-and-plan-20260813.md`](zgc-maturity-assessment-and-plan-20260813.md),
which asked for exactly this and said why:

> `ZGC_TLAB_MAX_CHUNK` was raised once to dodge a specific array shape and was
> still flat in the thread count; `zgc_headroom_margin` was sized against the
> whole arena rather than the end under pressure; the chunk request was fixed
> at a size its own recycled remnants could never satisfy. Each was found by a
> different OOM. Audit the rest the same way — for every constant in the
> allocator, write down what it bounds and check that the thing it bounds is
> itself bounded.

The question is deliberately narrow. Not "is this value well chosen" — that is
a tuning question and it needs a measurement. **"What is the unbounded quantity
on the other side of this constant?"** Every one of the three defects above was
a constant standing in front of something nobody had bounded: a thread count, a
remnant size, a request size.

---

## The table

| constant | value | what it bounds | is that thing bounded? |
|---|---|---|---|
| `ZGC_REAL_DEFAULT_HEAP` | 64 MiB | arena size when no `-Xmx` | **Yes** — superseded by the user's `-Xmx` whenever one is given. |
| `ZGC_REAL_GC_THRESHOLD_PERCENT` | 75 | `allocated` (live object bytes) before the classic trigger fires | **Now yes, and this is the 2026-08-13 defect.** `allocated` never counted the un-handed-out part of a TLAB chunk, so the trigger could not see the arena filling with reservations. It is bounded *because* `ZGC_TLAB_RESERVATION_SHARE` now bounds the reservations at `capacity/16`; the trigger's blind spot is that share and no larger. **This entry only became true when that constant landed** — before it, the blind spot was `live_threads × 512 KiB`, i.e. unbounded. |
| `zgc_headroom_margin` | `max(capacity/128, 8 MiB)` | the un-bumped middle below which the allocatable-space trigger arms | **No — see §2.** |
| `ZGC_TLAB_MAX_CHUNK` | 512 KiB | one thread's chunk **ceiling** | **Yes**, and it is a ceiling only: `chunk_bytes_now` divides a `capacity/16` budget by the live buffer count and clamps into `[min_tlab_size(), ceiling]`. |
| `ZGC_TLAB_RESERVATION_SHARE` | 16 | total bytes claimable by all chunks at once (`capacity/16`) | **Yes**, provided `live_slots` tracks reality — see §3. |
| `ZGC_LARGE_OBJECT_MIN` | `ZGC_TLAB_MAX_CHUNK/8` = 64 KiB | which end of the arena a request is served from | **Yes**, and it is *derived*, not chosen: it is exactly `ZTlabConfig::max_tlab_alloc`, i.e. "an object no TLAB will ever hold". If the chunk ceiling moves, this moves with it. |
| `Arena::high_reserve` | `capacity/8`, clamped to `capacity/4` | floor under the large-object end | **Yes** — clamped at the setter, and it is a *preference* the small-object end may overrun rather than a hard partition, so it cannot itself cause an OOM. |
| `ZGC_TLAB_ALIGN` | 8 | chunk and object alignment inside a chunk | **Yes** — a property of the object layout, not of any workload. |
| `ZGC_REAL_MAX_ARRAY_LENGTH` | `i32::MAX` | array **length** accepted before refusal | **Yes** — the JVMS bound. Note it bounds the length, not the byte size: `i32::MAX` longs is 16 GiB, which is the correct behaviour (refuse at allocation, not at length). |
| `MAX_PLAUSIBLE_BODY` | `(1<<24) × 8` = 128 MiB | a body size read out of a header, to reject the `1<<40` corrupt-header sentinel | **Yes, and correctly scoped** — it is applied to `ObjectKind::Object` only. A 4 GiB `char[]` is sized through `array_data_size` and is unaffected, which is what lets ZGC pass `TestCharChunkLargeHeap` where the generational collector cannot. Had this screen been applied to arrays it would have been a silent 128 MiB array cap. |
| `ZGC_FRAG_REPORT_WALLS` | 32 | walls walked and rows printed by the one-shot failure report | **Yes** — a diagnostic output cap, no allocation behaviour behind it. |
| `ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE` | 250 | which collections count as fragmentation evidence (new, Phase 2.2) | **Yes** — a share of capacity. |
| `ZGC_FRAG_FLOOR_PERMILLE` | 10 | when the gauge warns once | **Yes**, and derived: 1% of capacity is below one `ZGC_TLAB_MAX_CHUNK` on any heap up to 50 MiB. |
| `ZGC_ADDRESS_BITS` / the `ZGC_COLOR_*` bits | 42 / bits 42-45 | addressable heap under a colored pointer (4 TiB) | **Not yet load-bearing** — `vaddr` is adopted only as an enum today and slots hold raw pointers. This becomes a real bound in Phase 4 and should be re-derived there, against the arena's actual address range rather than against OpenJDK's. |

Ten of thirteen are sound. Two are new and were derived rather than picked. The
remaining one is below.

---

## 2. `zgc_headroom_margin` bounds "a typical allocation", and is used as a proxy for "this allocation"

The finding. In `alloc_raw`, after every **successful** allocation:

```rust
let margin = zgc_headroom_margin(arena.capacity());
let tail = arena.capacity().saturating_sub(arena.used());
if tail < margin && !arena.has_free_block_at_least(margin) {
    self.headroom_low.store(true, Ordering::Relaxed);
}
```

`margin` is a constant share of the heap. **The request size is not bounded by
anything** — `ZGC_REAL_MAX_ARRAY_LENGTH` bounds the *length*, and a legal Java
array can be many times `margin`. So for any request larger than `margin`:

* `has_free_block_at_least(margin)` can answer **yes** — there is a 20 MiB
  hole — while the actual request needs 2 GiB;
* `headroom_low` therefore never arms;
* and `needs_gc()`'s other arm counts live bytes, which on a reservation-heavy
  or fragmented arena is exactly the quantity that does not move.

The heap then refuses the request having never collected on its account.

**This is not a fix waiting to be applied, and that matters.** The obvious
repair — scale the margin to the largest request seen — was already tried in
its general form on 2026-08-13 (widening the margin by the unclaimed large
object reserve), measured at **4.6% on `TestTomcat` for no change in outcome**,
and removed. Collecting earlier is the wrong lever: it charges every workload
for a shape only some workloads have.

**The right repair is to act on the failure rather than predict it**, which is
Phase 2.4 — and the two phases meeting here is the actual result of this audit.
A margin cannot bound an unbounded request, so the allocator should stop trying
to and should instead treat a refusal as the signal it is. That is now what
happens: `ZgcRealHeap::hard_alloc_failure` latches on a genuine refusal and the
native boundary honours it **without** re-asking the occupancy predicate that,
by the argument above, cannot know.

So the entry in the table stays "No" on purpose. The constant is not wrong; the
job of predicting an unbounded quantity is.

---

## 3. `ZGC_TLAB_RESERVATION_SHARE` is bounded, but only because `live_slots` is pruned

`chunk_bytes_now` divides `capacity / ZGC_TLAB_RESERVATION_SHARE` by
`live_slots`. That bounds total reservations at `capacity/16` **only if
`live_slots` never under-counts the buffers actually holding chunks.**

Checked, and it holds, in the direction that matters:

* it is stored from `slots.len()` when a thread attaches, so it rises
  immediately with new threads;
* it is stored again after `retain(|_, cell| Arc::strong_count(cell) > 1)`
  during a collection, which reaps buffers whose owning thread has exited;
* between collections it therefore only ever **over**-counts, which shrinks the
  chunk — fail-safe.

The failure mode that would matter is the reverse: a live thread holding a
chunk that `slots` has forgotten. That cannot happen, because a live thread
always holds a second `Arc` (in `ZGC_TLAB_HANDLES` for its whole life, or on
its stack inside `attach`/`alloc_tlab`), which is precisely the invariant the
prune's `strong_count > 1` test rests on.

Worth stating because it is the same *shape* as the original defect and the
lesson from it applies verbatim: **a doc that bounds the blind spot by
`live_threads × chunk` has only bounded it if something bounds `live_threads`.**
Here something does, and the thing that does is a `strong_count` invariant in a
different module — so if that invariant is ever weakened, this constant silently
stops bounding anything.

---

## 4. What this audit changed

* Nothing in the table's ten sound rows. Auditing is allowed to find things
  correct, and recording *why* they are correct is the deliverable — the next
  person to reach for `ZGC_TLAB_MAX_CHUNK` will find it documented as a ceiling
  with a divisor behind it, which is what stops the third instance of the
  2026-08-11 → 2026-08-13 treadmill.
* Two rows are new (`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`,
  `ZGC_FRAG_FLOOR_PERMILLE`) and were derived from what they bound, in
  accordance with the rule this audit exists to enforce.
* One row (`zgc_headroom_margin`) is a standing "No", with the reasoning above
  for why the fix belongs in Phase 2.4 rather than in the constant.
* One row (`ZGC_ADDRESS_BITS`) is flagged as **not yet load-bearing** and owed a
  re-derivation in Phase 4, against this arena's address range rather than
  OpenJDK's 4 TiB.
