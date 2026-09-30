# A humongous REFERENCE array is still zeroed with the old-generation lock held

> **STATUS (2026-09-29, gce e1/x): KEEP -- the perf A/B was not run.** `w1_humongous_ref_unlocked` / `w1_humongous_ref_default` end rc 0 on every battery, with the same stdout verdict on base and e1. **Remaining:** the A/B that decides `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED`.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- FIX LANDED OPT-IN (`CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED`), safe by reading; the perf A/B decides.** One battery run per arm of `GenR4W4HumongousProbe 4 200 50331648 -Xmx2g -Xmn64m`: default 17119 MiB/s (`w1_humongous_ref_default`), unlocked 20031 MiB/s (`w1_humongous_ref_unlocked`), both `sum=5033164800 nonzero=0 ... ok`. One run each is inside this host's noise; the page's interleaved A/B was not run.

> **STATUS (2026-09-28, gcd d5/q): FIX LANDED behind the opt-in
> `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED`; SAFE TO FLIP by reading, gated
> on the runs below (perf only, so the flip is the orchestrator's).** No
> code change this wave. Re-read at `d916d1c40`
> (`gen_heap.rs::try_alloc_array_humongous`, `humongous_ref_disguise`):
>
> * Every reader that can reach the block between the two lock holds strides
>   by the header and decides the body's shape from the HEADER's element tag,
>   not from the class id: the concurrent mark's `scan_object_into` reads a
>   `ConcurrentMarkHeaderSnapshot` (`element_tag`), so the disguise (class id
>   of the reference array, element type `Long`) scans no body. The disguise
>   carries `GC_FLAG_OLD_GEN` like the real header.
> * The opt-in `CRATONVM_GC_OLD_HUMONGOUS_TOP` (gen r5w3/oldgen7, reviewed
>   by gcd d4/n) places the same disguised
>   block from the top (`alloc_from_top(.., false)`), with the same two-hold
>   protocol; nothing new there.
> * No STW reader (conservative scan, heap walk, HPROF pause) can start while
>   the allocating thread is between the holds: it polls no safepoint there.
>
> Flip gate (all with the switch ON, Linux, idle host):
>
> 1. `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx2g -Xmn64m -cp tools/bench GenR4W4HumongousProbe 4 200 50331648`
>    → a line starting `sum=5033164800 nonzero=0 threads=4 iters=200 bytes=50331648`
>    and ending ` ok`; `[GC] gen-alloc: gen_humongous_unlocked_zeroes=` about
>    200 above the flag-off run's. Again with 8 threads, and with
>    `CRATONVM_DBG_DEADREF_STORE=1` (nothing reported).
> 2. The same with `CRATONVM_GEN_CONC_MARK_SLICE=1` (concurrent cycles overlap
>    the allocations): identical lines, no `concurrent mark: skipping object`
>    warning.
> 3. Throughput: flag-on median `mib_per_s=` at or above flag-off over four
>    interleaved rounds. Flip with
>    `alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED = true` if 1-3
>    hold; `=0` stays the kill switch.
>
> *Previous status (kept for the record):*

> **STATUS (2026-09-27, gen r5w5/sizer9): unchanged — FIX LANDED behind the
> opt-in `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED`, awaiting its A/B; stays
> opt-in (perf only).** Re-read against the wave-5 base: the disguise
> protocol in `try_alloc_array_humongous` / `humongous_ref_disguise` is as the
> r5w1 block below describes. One more hazard was checked and is not one:
> step 3 rewrites the WHOLE header (the real reference-array header over the
> `long[]` disguise), which would drop a mark a concurrent marker had set in
> the disguise's `gc_flags` between steps 1 and 3 — but the concurrent mark
> records marks in its side `MarkBitmap` (`concurrent_mark.rs`), not in
> `GC_FLAG_MARKED`, and the stop-the-world major that does use the header
> flag cannot run while the allocating thread is between the two lock holds.
> Nothing else to implement here; the orchestrator's run below (unchanged)
> decides the default.

> **STATUS (2026-09-26, gen r5w1/oldgen5): FIX LANDED behind the opt-in
> `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` (token
> `gen-humongous-ref-zero-unlocked`), awaiting its A/B.** Unbuilt when
> written.
>
> Option 2 of this page, without the gap it feared. Under the lock the block
> gets a DISGUISE header: a `long[]` of exactly the reference array's footprint
> (`data_size / 8` elements; every array body is 8-byte rounded, so the two
> agree to the byte — `gen_heap::humongous_ref_disguise`). Every walker strides
> over it exactly as over the finished array, and the one reader that made
> reference arrays keep the locked memset (the concurrent mark's overflow
> rescan of a MARKED object) reads no body of a primitive array. The body is
> zeroed after the unlock, a release fence follows, and the real header is
> written under a SECOND, O(1) lock hold before the reference is returned;
> every concurrent reader holds that lock, so it sees one header or the other,
> and a zeroed body with the real one. Stop-the-world readers cannot run while
> the allocating thread is between the two holds (the primitive arm's premise).
> Counted in the existing `gen_humongous_unlocked_zeroes=`.
>
> Flag registered the standard way (`types/src/flags.rs`
> `alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED` + `GcFlags` field +
> switch test row, `flag_groups.rs`, `flag-surface.txt`,
> `docs/config/flag-inventory.md` counts 1568/1557, `docs/flag-tokens.md`).
>
> Tests: `cargo test -p cratonvm-gc --lib gen_r5w1_oldgen5_tests::the_humongous_reference_disguise_has_the_arrays_exact_footprint`;
> `cargo test -p cratonvm-types` (flag surface, docs, switch table).
>
> Probe (the existing one already allocates one `Object[]` in four; 48 MiB
> makes that `Object[]` humongous under 4-byte references too):
>
> ```
> CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx2g -Xmn64m -cp tools/bench GenR4W4HumongousProbe 4 200 50331648
> ```
> must print a line starting `sum=5033164800 nonzero=0 threads=4 iters=200
> bytes=50331648` and ending ` ok` (`ms=`/`mib_per_s=` vary), as HotSpot
> `java -Xmx2g -cp tools/bench GenR4W4HumongousProbe 4 200 50331648` does; the
> `[GC] gen-alloc:` line's `gen_humongous_unlocked_zeroes=` must be about 200
> higher than the flag-off run's (the `Object[]`s now count). Repeat with 8
> threads and with `CRATONVM_DBG_DEADREF_STORE=1` (nothing reported). To flip
> the default: the same runs, plus flag-on throughput beating flag-off
> (interleaved medians).

*Filed 2026-09-24, generational GC round 4 wave 6, lane `tlab6`. This is the
residual of
`docs/internal/gc/gengc-r4w3-alloc3-humongous-arrays-are-zeroed-under-the-old-gen-lock-FIXED-20260924.md`,
which is fixed for primitive arrays.*

- **Status:** OPEN
- **Severity:** perf. The lock hold time is proportional to the array, up to
  the 256 MiB humongous cap. It matters only when a program allocates large
  `Object[]`s while other threads want `old_gen`.
- **Code:** `gc/src/gen_heap.rs::try_alloc_array_humongous` and
  `humongous_body_zeroes_outside_the_lock`. The latter answers `false` for
  `ArrayElementType::Reference`, so `OldGen::alloc` zeroes the whole block
  under the lock.

## Why reference arrays were left locked

Wave 4's reader audit (on `humongous_body_zeroes_outside_the_lock`) found one
reader that can reach a fresh old-gen block without a reference to it:
the concurrent mark's overflow rescan, which calls `scan_object` on marked
objects and reads a reference array's body. A block that is not yet zeroed
could then hand the marker stale words as pointers. A primitive body is never
read as references, so the primitive arm is safe.

## What would have to exist

Either of these:

1. **Allocate-black with a zeroed-first body.** The concurrent marker treats
   an object allocated during a cycle as live and never scans it in that
   cycle, so no reader looks at the body until the allocating thread has
   published it. This is `concurrent_mark.rs`'s question, not the allocator's.
2. **A header that says "not yet scannable".** Write the header with length 0
   under the lock, zero the body after the unlock, then store the real length
   with a release store. Any walker that strides by the header would then see
   a short object followed by an unparsable gap, so the old-gen walkers (block
   offset table, sweep, `walk_objects`) would all need a gap rule. That is
   larger than option 1.

## How to verify

- `GenR4W4HumongousProbe` shape with `Object[]` instead of `long[]`, 4 and 8
  threads, `CRATONVM_GEN_CONC_MARK_SLICE` set so concurrent cycles overlap the
  allocations. Read `gen_humongous_unlocked_zeroes=` on `[GC] gen-alloc:`
  (reference arrays add nothing to it today).
- Correctness gate: every element of every array reads `null` at both ends
  and the middle, and `CRATONVM_DBG_DEADREF_STORE=1` reports no stale word.
