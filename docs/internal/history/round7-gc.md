# Round 7 — GC crate review

## (A) Regression audit of round-6 wave-1 changes

### 1. CRIT — JIT putfield/aastore skips `satb_barrier` entirely
**File:** `jit/src/x64.rs:8634-8644` (and aarch64 backend); contrast `vm/src/runtime/interpreter.rs:4093,5164,5961,6206`.
The interpreter calls `shared.heap.satb_barrier(old_value)` before every reference store. JIT-emitted `aastore`/`putfield` only call `helpers.write_barrier` (card-table post-store). When concurrent marking is active and JIT code overwrites an old reference, the SATB log never sees the original — classic SATB lost-object → UAF on the next mixed evacuation. The round-6 thread-local SATB plumbing is **moot for any tier-1 JITted method**.
**Fix:** before the store sequence, load the old slot, call `helpers.satb_barrier` if `satb_queue.is_active()` (inline-check the AtomicBool), then perform the store + card mark.

### 2. CRIT — `scan_and_evacuate_refs` dedup correct under STW only
**File:** `gc/src/g1.rs:982-996, 1020-1037`.
The `contains_key` pre-check is sound **because young/mixed collections run under STW** with a single owning thread holding `regions.lock()`; `pointer_map` is a plain `HashMap` passed `&mut`. If `gc_worker_threads > 1` is ever wired to actually parallelize evacuation (the config exists at `g1.rs:78`), this becomes a TOCTOU: two scanners sample `contains_key=false`, both call `evacuate_object`, one wins the insert, the loser drops its allocation but pushed the winner's `new_ptr` onto its worklist → double-scan and a leaked Survivor block.
**Fix:** convert `pointer_map` to `DashMap` or per-worker shards and use `entry().or_insert_with`; the boolean return of the entry call is the dedup signal.

### 3. CRIT — Async/JIT safepoints can miss SATB flush
**File:** `vm/src/runtime/interpreter.rs:106,229,270,899,1031,1082`.
`flush_thread_satb` is wired at: bytecode-loop poll (897), GC initiator entry (106/229/270), G1 phase initiators (1031/1082). **No JIT safepoint path** calls it — JIT poll stubs jump back to interpreter only after the JIT method returns. A JIT method running a long loop that crossed a backward-branch safepoint will park at the GcBarrier without draining its thread-local buffer (the per-thread SATB buffer is keyed by OS thread, so the JIT-running thread has its own non-empty buffer). Same UAF mechanism as JIT putfield above.
**Fix:** the JIT-emitted safepoint stub must call a runtime helper that runs `shared.heap.flush_thread_satb()` before `arrive_and_wait`.

### 4. HIGH — `MarkBitmap::clear()` `Release` insufficient if not on STW initiator
**File:** `gc/src/mark_bitmap.rs:115-119`.
Called from `g1.rs:1211` inside `start_concurrent_mark`, which runs in `brief_stw(...)` (initial-mark). At that point all mutators are parked at the gc_barrier — the `Release` per word does pair correctly with markers' subsequent `Acquire` after the barrier release. **However** the per-region loop emits N independent `Release` stores with no global fence; a marker thread waking on a different region whose clear hasn't issued yet can observe stale bits. In practice the STW barrier's unlock provides the fence, but the invariant is not documented and would break if `clear()` were moved out of STW (e.g. background pre-clear).
**Fix:** add a single `fence(Release)` after the loop, and a doc comment that callers must hold STW or pair with their own barrier.

### 5. MED — `touch_soft_reference` correctly sequenced
**File:** `native-builtins/src/reference.rs:277-283`.
Verified: referent is read first, `touch` runs only on the live path. If a concurrent GC clears between read and touch, the touch refreshes a now-zombie entry — harmless (next cycle re-clears). Looks fine.

## (B) Remaining round-5 items

### 6. HIGH — `OldGen::alloc` zeros entire allocation twice
**File:** `gc/src/old_gen.rs:44, 137`.
`new()` does `vec![0u8; capacity]` (zeroes the arena once), then every `alloc()` calls `write_bytes(ptr, 0, size)` again. Best-fit scan is also O(N) per allocation (`old_gen.rs:73-91`).
**Fix:** drop the alloc-time zero (callers re-init headers); replace freelist with a size-segregated free list (powers-of-two buckets) for O(1) alloc.

### 7. HIGH — TLAB retire silently drops up to 1 MiB of tail
**File:** `gc/src/tlab.rs:200-207, MAX_TLAB_SIZE=33`.
`retire` just nulls pointers; the unused tail between `cursor` and `end` is reclaimed only at the next region reset. With `MAX_TLAB_SIZE=1 MiB` and N threads, worst-case waste is N×1MiB per cycle.
**Fix:** before nulling, fill `[cursor..end)` with a dummy filler object (header with `ObjectKind::Array`/byte, `array_length = remaining - HEADER_SIZE`) so the heap walker treats it as garbage and the region's accounting reflects it.

### 8. MED — `forwarding_ptr` non-atomic during bulk memcpy
**File:** `gc/src/g1.rs:905-907, 936`.
`copy_nonoverlapping` propagates whatever stale `forwarding_ptr` was in the source header into the new copy; immediately overwritten on line 936, but during the window the new header is technically inconsistent. Under STW only the collector sees it, so currently benign — becomes UB if any concurrent reader (e.g. card-scanner) walks the new region before line 936.
**Fix:** memcpy header field-wise excluding `forwarding_ptr`, or write `forwarding_ptr = null` immediately before publishing the new pointer in `pointer_map`.

### 9. MED — `write_barrier` double header deref on hot path
**File:** `gc/src/gen_heap.rs:1072, 1078`.
Two `*const ObjectHeader` derefs per ref store. `target_header.gc_flags` only needs one byte; the load is fine, but inlining a single combined check (`(src.gc_flags & OLD) & !(tgt.gc_flags & OLD)`) using a single u32 mask compare would let the branch predictor merge them. Also: bias toward young→young (the most common store) — check `header.gc_flags & GC_FLAG_OLD_GEN == 0` and early-return before touching the target header.
**Fix:** swap the order so src-young is the first early-out; gen `Value::Object` tag check via raw discriminant compare.

### 10. MED — `ReferenceQueue::remove_blocking` 60s spin-yield
**File:** `gc/src/reference.rs:125-145`.
`thread::yield_now` loop wakes ~10–100 K times/s burning CPU; Java `ReferenceQueue.remove()` is supposed to park indefinitely. Cleaner threads waiting on a quiet app peg a core.
**Fix:** replace with `parking_lot::Condvar` + `Mutex<VecDeque>`; the GC's reference-enqueue path notifies.

### 11. LOW — `region_lookup` built once with no invariant guard
**File:** `gc/src/g1.rs:307-312, 339`.
Built from `r.data.as_ptr()` at construction. Comment claims region data is stable, but `G1Region::data: Vec<u8>` is not pinned — any future code that `push`/`reserve`/replaces a region's Vec invalidates the lookup silently.
**Fix:** wrap `data` in `Box<[u8]>` (no realloc API) and add `debug_assert_eq!(regions[i].data.as_ptr() as usize, region_lookup_for_idx(i))` at each `young_collection` entry.

## (C) New angles

### 12. HIGH — Concurrent mark vs young collection interaction undefined
**File:** `gc/src/g1.rs:468 (young_collection), 1239 (concurrent_mark_step)`.
Young STW evacuates Eden/Survivor, rewrites refs via `pointer_map`. Concurrent marker holds raw `*mut u8` pointers in `mark_worklist` — if a young GC moves a worklist entry, the marker dereferences a stale pointer post-GC. There's no remap of `mark_worklist` against the young-cycle's `pointer_map`.
**Fix:** at end of `young_collection`, drain-and-remap the global `mark_worklist` against the cycle's `pointer_map`; drop entries whose region was freed.

### 13. MED — Humongous regions never reclaimed mid-cycle
**File:** `gc/src/g1.rs:402-432`.
`alloc_humongous_locked` marks `HumongousStart`/`Continuation` but no path resets these back to `Free` outside full GC. A short-lived 600KB array pins two regions for the lifetime of the JVM.
**Fix:** during young/mixed cleanup, check humongous-start regions for liveness via the mark bitmap and reset both start and continuations when unreferenced.
