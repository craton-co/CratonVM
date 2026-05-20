# Round-9 GC Audit

## (A) Round-8 regression audit

### 1. CRIT — `g1.rs::alloc_humongous_locked` continuation regions misparsed by walkers

`gc/src/g1.rs:464-472` zeros `cursor` bytes per region. Heap walkers (`g1.rs:1185,1278,1583`) iterate every non-Free region — incl. `HumongousContinuation` — and parse offset 0 as `ObjectHeader`. Continuation data bytes are NOT a header. All-zero header decodes as `kind=Object,num_slots=0,size=40`, yielding ~25k spurious "live objects" per MB. Fix: walkers must `continue` on `HumongousContinuation` and treat `HumongousStart` as one object of `humongous_total_bytes`.

### 2. CRIT — `region.rs::evacuate` Release fence pairs with nothing

`gc/src/region.rs:573` `fence(Release)` comment claims pairing with Acquire on `forwarding_ptr`. But the field is a plain `*mut u8` (`gen_heap.rs:2343,2357`); no atomic load exists. On ARM/POWER scanners reorder freely. Fix: make it `AtomicPtr<u8>` with `load(Acquire)` on read side, or drop the fence and document STW-only.

### 3. CRIT — `old_gen.rs::alloc(0)` aliases live free space

`old_gen.rs:149-151` returns `self.data.as_mut_ptr()` for size 0 — same address bucket[N] later hands out as head of the initial free block. `alloc(0)` then `alloc(...)` aliases offset 0. Return a unique sentinel or reject size 0.

### 4. HIGH — SATB `deactivate_and_drain` racy under weak memory

`satb.rs:281-303`: pass1 → late `flush()` mid-`shards[X].lock()` → pass2 → store INACTIVE. Late writer can release its shard lock AFTER the INACTIVE store, stranding its entry until next cycle. Fix: third drain after INACTIVE store, OR have `flush()` re-check `is_active()` while holding the shard lock.

### 5. HIGH — Mark worklist panic is a DoS

`g1.rs:1431-1437,1456-1462,1518-1524` and `concurrent_mark.rs:188-195` panic at 1M entries. A Java `Object[1_500_000]` crashes the VM. The TODO mentions spill/fallback — none implemented. Degrade to STW full-mark from card table; don't panic.

### 6. HIGH — `install_tail_filler` is dead code

`tlab.rs:273` has no production callers (only same-file tests). Round-7 wave-2's "filler in place" claim is false. Every TLAB retire still leaves stale tail bytes that break walkers not stopping at `cursor`. Wire into actual `Tlab::retire` site(s), or delete.

### 7. MED — `Tlab::new` release-mode silent round-down

`tlab.rs:154-175` asserts in debug, silently drops up to 7 bytes in release. Return `Option<Tlab>` or unconditionally assert.

## (B) Remaining issues

### 8. CRIT — `post_write_barrier_rset` global `regions.lock()` per ref store

`g1.rs:1985`: full `Mutex<Vec<G1Region>>` per reference store serializes all mutator writes. Only mutation is `rset.add_reference`. Lift rset out into per-region `Mutex<FxHashSet<usize>>` so the barrier contends only on the destination region's lock.

### 9. HIGH — No humongous reclamation in young GC

`g1.rs`: no path frees humongous regions in `young_collection`. Apps that allocate then drop `byte[1MB]` leak humongous regions until full GC. Real G1 reclaims dead humongous each young pause. Add humongous-sweep at end of `young_collection`.

### 10. MED — ReferenceProcessor single-threaded + spin-yield

`reference.rs:125-138` `remove_timeout` spin-yields (context switch per iter). `process_references` (line 285) does Soft/Weak/Final/Phantom serially. Use `Condvar` for blocking remove; parallelize the four phases with `rayon::join`.

### 11. MED — No concurrent-mark cancellation

`concurrent_mark.rs:313` drains until empty with no alloc-rate feedback. If mutators outpace marker, IHOP refires repeatedly. Add alloc-rate sample + cancel hook on `ConcurrentMarker.state`.

### 12. LOW — TLAB adaptive sizer can't shrink mid-lifetime

`tlab.rs:501` `next_refill_size` only at refill boundary. Burst-then-idle thread holds 1 MiB the whole idle period. Add `should_yield_tlab(now)` hook for safepoint poll to retire-and-shrink oversized idle TLABs.
