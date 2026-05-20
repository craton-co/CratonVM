# Round-5 GC Review

Audit of round-4 changes to `gc/` plus remaining items from round-4.
Severities: **CRIT** = correctness / concurrency bug, **HIGH** = perf or
correctness within STW, **MED** = perf / hygiene.

---

## 1. CRIT — SATB per-thread buffer never flushed by mutators

`gc/src/satb.rs:54` defines `flush_thread_satb_buffer`, but a grep over
the whole tree shows the only call sites are **tests** (`g1.rs:3397,
3406`) and the unit tests in `satb.rs`. Neither the safepoint
mechanism, `start_concurrent_mark` (`g1.rs:1183`), nor `remark` calls
it on the mutator side. Each mutator can carry up to 256 buffered SATB
entries that the global queue never sees → references overwritten just
before the remark STW pause are lost → live objects are missed and
**reclaimed** in the next CSet evacuation (classic SATB UAF).

**Fix:** wire `flush_thread_satb_buffer(&satb_queue)` into the
safepoint-entry hook for every mutator (e.g. interpreter / JIT poll
slow-path) and call it again on every thread at remark-STW start
(roll the thread list and `flush_thread_satb_buffer` per-thread, or
make threads flush before parking).

---

## 2. CRIT — `MarkBitmap::clear()` Release-stores vs `try_mark` Relaxed-fetch_or

`gc/src/mark_bitmap.rs:109-113` clears every word with `store(0,
Release)`. `try_mark` (line 78) uses `fetch_or(mask, Relaxed)`.
On weakly-ordered ISAs (ARM/Power) a marker thread that started a new
cycle right after `clear()` can see stale `1` bits from the previous
cycle and skip marking → live objects treated as already-black.

**Fix:** either `clear()` should use `SeqCst` followed by a
release-fence, or `try_mark` should use `AcqRel` for the fetch_or
ordering on the first probe per cycle. Simplest: `fetch_or(..,
AcqRel)` — the perf comment claiming ARM benefit is moot because
correctness mandates the synchronisation.

---

## 3. CRIT — duplicate worklist pushes in `scan_and_evacuate_refs`

`gc/src/g1.rs:977-985` — both arms of the `if/else` push
`new_ptr` onto `work_list`. Even if `pointer_map.get` shows the
object was previously forwarded (already on/processed off the
worklist), it is pushed again. The Cheney scan therefore re-scans
forwarded objects an unbounded number of times for cyclic graphs.
Not a soundness bug (idempotent), but it is O(N²) under cycles and
defeats the audit comment immediately above it.

**Fix:** push `new_ptr` only inside the `if !pointer_map.contains_key`
branch; in the `else` arm just rewrite the slot.

---

## 4. HIGH — `take_dirty_cards()` clears tracking list but not bitmap

`gc/src/card_table.rs:190` is `mem::take(&mut dirty_cards)` — the
`cards[idx]` bytes stay `CARD_DIRTY`. The contract assumed by
`scan_dirty_cards` (`gen_heap.rs:2377`) is that the caller calls
`clear_all()` immediately after (`gen_heap.rs:1724`), which is true
today. **But** `mark_dirty`/`mark_dirty_bulk` (lines 114, 140) skip
appending an index when `cards[idx] == CARD_DIRTY` — so any
write-barrier hit between `take_dirty_cards` and `clear_all` (e.g. a
GC worker thread, JNI, or async finalizer thread) is silently dropped.

**Fix:** make `take_dirty_cards` also reset every taken index to
`CARD_CLEAN` in the same lock acquisition, or document the invariant
and assert it with `debug_assert!` in `mark_dirty`.

---

## 5. HIGH — `touch_soft_reference` defined but never wired into native `Reference.get()`

`gc/src/reference.rs:259` adds `touch_soft_reference`, but
`native-builtins/src/reference.rs:237 native_ref_get` simply returns
`ctx.get_field(this, REF_FIELD_REFERENT)` with no LRU touch. Every
`SoftReference.get()` therefore still leaves `last_access_time_ms = 0`
→ every soft ref looks infinitely stale on the next major GC →
caches get nuked, defeating SoftReference semantics entirely.

**Fix:** in `native_ref_get`, after retrieving the referent, look up
the type via the class and, if `SoftReference`, fetch the millisecond
clock and call `touch_soft_reference(this.as_ptr() as usize, now_ms)`
through the `ReferenceProcessor` mutex held on the VM.

---

## 6. HIGH — ReferenceQueue spin-yield burns idle CPU

`gc/src/reference.rs:125-138` `remove_timeout` does a tight
`yield_now`/`pop_front` loop for up to 60 s (default in
`remove_blocking`). One idle finalizer / cleaner thread saturates a
core.

**Fix:** swap `pending: VecDeque<usize>` for a `Mutex<VecDeque> +
Condvar` (or `parking_lot::Condvar`); `enqueue` notifies one,
`remove_timeout` does `wait_timeout`.

---

## 7. MED — OldGen double-zero + O(N) free-list

`gc/src/old_gen.rs:44` `vec![0u8; capacity]` zeros the whole arena at
startup; `alloc` (line 137) calls `std::ptr::write_bytes(ptr, 0, size)`
again on every allocation. The first zero is the kernel's
zero-on-fault, the second is a real memcpy. Same pattern in
`arena.rs:24` paired with `gen_heap.rs:2081` and in
`region.rs:205/329` (`bump_alloc_in`).

`alloc` is also O(N) over the free list (best-fit scan, line 73),
and `free` linearly `Vec::insert` into a sorted Vec (line 166).

**Fix:** drop the alloc-time `write_bytes` (the arena is already zero
on first touch, and `try_alloc_young`/`bump_alloc_in` are the only
allocators); replace `free_list: Vec<FreeBlock>` with a
size-segregated free list (or `BTreeMap<usize, Vec<offset>>`) to make
alloc/free O(log N).

---

## 8. MED — ZGC stub compiles unconditionally (1.8 kLOC dead code)

`gc/src/zgc.rs` is 1884 lines wired via `gc/src/lib.rs:38 pub mod
zgc;` with no `#[cfg(feature = "…")]`. `gc/Cargo.toml` has no `zgc`
feature. Nothing outside the crate references the symbols. It just
bloats compile times and the binary.

**Fix:** add `zgc = []` to `[features]` and gate `pub mod zgc;` and
the `pub use` lines (none today) behind `#[cfg(feature = "zgc")]`.

---

## 9. MED — TLAB `retire()` does not return waste bytes to the arena

`gc/src/tlab.rs:203-207` nulls `start/cursor/end` and lets the unused
tail (up to `MAX_TLAB_SIZE = 1 MiB`) sit until the *next* GC resets
the whole arena. With N threads churning short-lived TLABs this is up
to `N · 1 MiB` of dead space wasted between collections.

**Fix:** add `retire_with_refund(arena: &mut Arena)` that hands
`self.end - self.cursor` back to the arena cursor (only valid when
`self.end == arena.cursor`, i.e. the TLAB is the most recent
allocation). For non-recent TLABs, accept the waste but record it for
the adaptive sizer to shrink next refill.

---

## 10. MED — `mark_word` atomic transfer is correct under STW but `forwarding_ptr` is plain

`gc/src/g1.rs:920-929` correctly does atomic load/store of
`mark_word` after a `copy_nonoverlapping`. However the
`pub forwarding_ptr: *mut u8` field
(`types/src/heap_types.rs:209`) is plain. Today this only matters
inside STW so a plain write is fine, but the file-level comment
("future concurrent G1 needs a different forwarding protocol")
is **the only thing** keeping a future contributor from racing the
field. The bulk `copy_nonoverlapping` at `g1.rs:906` also copies the
old `forwarding_ptr` value into the new header for one instruction
before line 936 nulls it — a concurrent reader observing the new
header in that window would see a self-stale forwarding pointer.

**Fix:** convert `forwarding_ptr` to `AtomicPtr<u8>` now; the cost
is zero on x86 and small on ARM, and it removes the foot-gun for the
concurrent collector that is on the roadmap. Set null **before** the
header copy ends being observable (or write the header field-by-field
rather than memcpy).

---

## 11. MED — `write_barrier` is `#[inline]` but reads two object headers on every reference store

`gc/src/gen_heap.rs:1058-1092`. The fast-path reads
`obj`'s header for `gc_flags`, then the **target** header for
`gc_flags` — even for young→young stores (the most common case in
typical workloads, since most stores happen during object init).
Two dependent loads on the write-barrier hot path measurably hurts
the interpreter loop on Skylake.

**Fix:** stash the source generation in the high bit of `ObjectRef`
(unused tag bit) at promotion time, so the source check becomes a
register test; only when the source bit says "old" do we deref the
target header. This was the trick HotSpot used before card-mark
inlining.

---

## 12. MED — `region_lookup` table never rebuilt; humongous future-proofing

`gc/src/g1.rs:286, 307-312` builds `region_lookup` once in `new()`.
Today the region table is fixed-length (`g1.rs:277`), so this is
sound. But there is no `debug_assert!` enforcing that invariant —
any future code that does `regions.push(...)` (e.g. dynamic
humongous expansion) will silently corrupt every
`lookup_region_for_addr` result without test failure.

**Fix:** wrap the regions field with an internal helper that
panics on length change, or compute `region_lookup` lazily under a
`OnceLock<Vec<(usize, usize)>>` cleared whenever the region table
mutates. At minimum add `debug_assert_eq!(regions.len(),
self.region_lookup.len())` at the top of
`lookup_region_for_addr`.

---

**Word count:** ~590.
