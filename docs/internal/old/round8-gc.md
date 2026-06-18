# Round 8 — GC Crate Review

Scope: regression audit of round-7 wave 1+2 + leftover round-5/6/7 items + new angles.

## Findings

### F1 — CRIT: Humongous continuation regions not zeroed
`gc/src/g1.rs:402-432` — `alloc_humongous_locked` only zeros `size.min(region_size)` bytes in the first region; continuation regions are claimed and their `cursor` advanced but their backing `data` Vec is never wiped. A subsequent heap walk / mark sees stale ObjectHeader-shaped bytes from a prior allocator life, which the marker will follow as live references → UAF and pointer corruption.
**Fix:** loop over `start+1..start+regions_needed` and `write_bytes(regions[i].base_ptr_mut(), 0, regions[i].cursor)` after computing each continuation's cursor.

### F2 — CRIT: OldGen `min_satisfying_bucket` skips fitting blocks
`gc/src/old_gen.rs:73-82` — `next_power_of_two(size)` is used to compute the starting bucket, but bucket `k` holds blocks in `[2^(s+k), 2^(s+k+1))`. For `size = 17`, `next_power_of_two = 32` → start at bucket 2 (which holds ≥32 byte blocks). A free 24-byte block in bucket 1 is **never inspected**, so `alloc()` returns `None` even when fragmentation could service the request → spurious OOM under any non-power-of-two allocation pattern.
**Fix:** start search at `bucket_for(size + align - 1)` (bucket of the request size itself), not the next-pow-2 bucket. The first iteration may scan blocks too small to fit; bail to the next bucket on miss.

### F3 — CRIT: Evacuate "data-then-header" publication has no fence
`gc/src/region.rs:531-575` — comment claims the dest header is "published as a single atomic-sized store *after* the data area is in place". In reality `copy_nonoverlapping` (the data copy) and the subsequent plain field writes (`(*dst_hdr).class_id = ...`, `forwarding_ptr = ...`) have **no ordering**: no `compiler_fence`, no `Release`-ordered atomic, no `fence(Ordering::Release)`. LLVM and ARM64 are free to reorder the field stores ahead of the memcpy. The mark_word is the only atomic, and it is written *last* with `Relaxed`. Under STW this is harmless (no concurrent reader), but the comment promises concurrent-scanner safety that does not exist.
**Fix:** insert `std::sync::atomic::fence(Ordering::Release)` between the memcpy and the header writes, and write `forwarding_ptr` through an `AtomicPtr::store(_, Release)` if concurrent readers are ever envisioned. Otherwise downgrade the comment.

### F4 — CRIT: SATB log TOCTOU on deactivation
`gc/src/g1.rs:1889-1896` and `gc/src/gen_heap.rs:1107-1128` — `satb_pre_barrier` checks `is_active()` then calls `satb_thread_local_log()`. The collector may flip `active = false` between the two. The post-deactivation log lands in the per-thread buffer and survives until the *next* drain — at which point it is treated as a root, falsely retaining a now-dead object and (worse) potentially keeping a dangling raw address that has been recycled by a young evac in between.
**Fix:** move the `is_active()` check inside `satb_thread_local_log` and re-check after the borrow-mut, or have the deactivate path issue a global epoch fence + drain every per-thread buffer one final time before any allocator-recycle path runs.

### F5 — HIGH: `gc_worker_threads` default 4 contradicts single-threaded evac
`gc/src/g1.rs:78` (default = 4) and `gc/src/g1.rs:980-1001` — the dedup comment says the code is correct "only because young/mixed evacuation runs single-threaded under STW". There is no runtime guard that the configured worker count is 1 before entering the single-threaded path. A future `gc_worker_threads=4` parallel evacuator that forgets the DashMap migration will silently TOCTOU. Today the field is unread, but a config value of 4 is a foot-gun.
**Fix:** add `debug_assert!(self.config.gc_worker_threads == 1)` at the top of `young_collection` / mixed collection, or change the default to 1 until the parallel evacuator lands.

### F6 — HIGH: TLAB filler depends on un-asserted 8-aligned `end`
`gc/src/tlab.rs:253-311` — the filler size formula `length = data_bytes / 4` is correct **only when `data_bytes` is a multiple of 8** (so the `array_data_size` 8-byte round-up doesn't grow past the tail). The proof relies on `end_addr` being 8-aligned, but `Tlab::new` sets `end = ptr.add(size)` with no enforcement that `size` is multiple of 8. A caller that hands a 7- or 12-byte buffer triggers a write past `end`.
**Fix:** `debug_assert_eq!(end_addr & 7, 0)` at filler entry; or compute `length = (data_bytes & !7) / 4` and zero the residual.

### F7 — HIGH: Mark stack is unbounded
`gc/src/g1.rs:270` (`mark_worklist: Mutex<Vec<usize>>`) and concurrent_mark.rs — no overflow handling. A pathological reference graph (or a marker that falls badly behind a mutator) lets the worklist grow without bound until OS-OOM. HotSpot's restart-from-roots-on-overflow contract is absent.
**Fix:** cap the worklist at `~heap_bytes / 16`; on overflow set a `restart_required` flag, dump residual into a per-region pending bitmap, and re-seed from roots at the next safepoint.

### F8 — HIGH: ReferenceQueue still busy-spins (round-5 #14 unresolved)
`gc/src/reference.rs:125-145` — `remove_timeout` / `remove_blocking` spin with `thread::yield_now()` for up to 60 s, pinning a core at 100% per blocked `Reference.remove()` caller. Documented as a known leftover.
**Fix:** replace with a `parking_lot::Condvar` paired with `enqueue`'s push-notify, or a `crossbeam_channel` bounded queue. Drop the 60 s safety cap once parking is in place.

### F9 — MED: SATB shard hash uses `DefaultHasher` per call
`gc/src/satb.rs:172-182` — every `flush()` re-creates a `DefaultHasher` and hashes `ThreadId`. SipHash on a single 8-byte input is hundreds of cycles; multiplied by per-256-stores frequency this is measurable. Worse, `ThreadId`'s `Hash` impl is not documented as cheap.
**Fix:** cache the shard index in a `thread_local!` `Cell<usize>` (lazy-init to `hash(thread::current().id()) & MASK`).

### F10 — MED: Mark-bitmap `clear()` per-word `Release` lacks inter-word ordering for background pre-clear
`gc/src/mark_bitmap.rs:131-136` — the trailing `fence(SeqCst)` is correct **only when `clear()` runs to completion before any marker observes any word**. Today `clear()` is STW-bracketed so the comment notes the property holds. A future background pre-clear (mentioned in the comment) that races a stray `try_mark` on a word the clear loop has not yet visited will observe a stale black bit. Recommend documenting "STW-only" as an assertion rather than a future-tense aspiration.
**Fix:** add `#[cfg(debug_assertions)]` STW-check, or split `clear()` into `prepare_clear()` + `finish_clear()` with the fence only on `finish_clear()`.

### F11 — MED: `young_collection` worklist remap is O(W·R)
`gc/src/g1.rs:614-631` — for each surviving worklist entry the code calls `self.region_for_ptr(&regions, ...)` which does a linear scan over R regions. With heaps of hundreds of regions and a sizable in-flight gray set this dominates STW pause.
**Fix:** call the O(log R) `lookup_region_for_addr` already cached on `G1Collector` instead.

### F12 — LOW: Card-table dirty-card list grows monotonically until safepoint clear
`gc/src/card_table.rs:51,116,142` — `mark_dirty` and `mark_dirty_bulk` push to `dirty_cards` even when the same `index` is already dirty. Repeated stores to a hot card produce O(stores) entries and an O(N) dedup on `take_dirty_cards`.
**Fix:** check `cells.cards[index] == CARD_CLEAN` before push (already done in `mark_dirty` line 113-119 — apply the same gate to `mark_dirty_bulk`).
