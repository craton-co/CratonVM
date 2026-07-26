# GC allocation fast path — audit and fixes (2026-07-26)

Scope: `gc/src/{heap,arena,tlab,old_gen,pinned}.rs`. Everything else named here
was read-only. Base: `dev` @ `6495a191c`.

Context that shaped the work: the default collector does **not** compact
(`moving_young` defaults off and `gen_heap` fail-closes to a non-moving
mark-sweep whenever a live JIT frame exists — the steady state at a
500-invocation JIT threshold). Compaction's correctness blocker was closed on
2026-07-26 (`docs/internal/fixed-suite-bugs/app-jvm-bugs/moving-young-gen-drops-jit-held-oops-FIXED.md`),
but moving-young stays opt-in on throughput grounds — so the *free-list*
allocator is still the production allocator, not a degraded fallback, and the
fast path has to be good in a fragmenting world.

---

## 1. The real allocation path, end to end

There are **two** allocators for an ordinary object and they do not share code.

### 1a. Compiled code — `jit/src/x64.rs::emit_inline_tlab_new` (read-only)

Nothing in `gc/` runs. The whole allocation is inline machine code:

```
  ; optional 3-instr layout-replace guard (compact classes)
  mov  rax, [rbp - jit_thread_slot]     ; cached JvmThread*, else helper call
  test rax, rax / jz  slow
  mov  r10, rax
  mov  r11, [r10 + tlab_off + 0]        ; Tlab::cursor
  add  r11, 7
  and  r11, -8                          ; align up to 8
  lea  rax, [r11 + total_size]          ; new cursor  (total_size is a constant)
  cmp  rax, [r10 + tlab_off + 8]        ; Tlab::end
  ja   slow
  xor  edx, edx
  mov  [r11 + 32 .. total_size], rdx    ; (total_size-32)/8 body-zeroing stores
  mov  dword [r11 + 0],  class_id
  mov  dword [r11 + 4],  0              ; kind/elem/pad/gc_flags
  mov  dword [r11 + 8],  0              ; identity_hash (lazy-mint contract)
  mov  dword [r11 + 12], num_fields     ; shape
  mov  dword [r11 + 16], 0              ; forwarding_ptr lo
  mov  dword [r11 + 20], 0              ; forwarding_ptr hi
  mov  dword [r11 + 24], 0              ; mark_word lo
  mov  dword [r11 + 28], 0              ; mark_word hi
  mov  [r10 + tlab_off + 0], rax        ; COMMIT — the linearization point
  ; then either `mov rax, r11` or a call to jit_post_tlab_init
```

Measured cost of the common case (48-byte object, cached thread pointer,
`skip_post_init_helper`): **~9 bookkeeping instructions + 10 stores, no atomics,
no call** — roughly 15–25 cycles. That part is healthy.

It is *not* healthy when `skip_post_init_helper` is false, which is the
conservative default in `try_compile`. `jit_post_tlab_init`
(`vm/src/jit/helpers.rs:2226`) then runs **per allocation** and performs
`note_jit_boundary()`, a `class_layout()` registry lookup, a locked
`next_identity_hash()`, `a2dbg::record`, `jit_post_alloc_init` and
`dbg_jit_alloc_filter()`. See cross-owner request X3.

### 1b. Interpreter / JIT slow path — `Tlab::alloc_initialized`

`vm/src/runtime/interpreter.rs::tlab_alloc_object_inner` →
`gc/src/tlab.rs::alloc_initialized`:

- load `cursor`, align up (2), round `size` to the alignment footprint with an
  overflow check (3), add + overflow check (2), `cmp` against `end` + branch (2)
- run `init` — a 32-byte header write plus **`next_identity_hash()`, a locked
  `xadd`**
- a `Release` compiler fence (0 instructions on x86-64 — it exists to order the
  header stores before the cursor commit, matching the JIT's TSO reasoning)
- store `cursor` (1)
- three plain counter updates on the pressure tracker (~4)

then back in the caller, **two more locked `xadd`s** on process-shared cache
lines: `shared.mem.tlab_hit_count` and `shared.mem.bytes_allocated_total`.

**Measured per-allocation cost: ~20 instructions plus 3 lock-prefixed RMWs**
≈ 60–130 cycles uncontended, and it degrades with thread count because all
three atomics are shared lines. HotSpot's TLAB fast path is ~5 instructions and
no atomics. This is the largest single constant on the interpreted path and it
lives entirely outside my files — cross-owner requests X1/X2.

### 1c. Refill

`tlab_alloc_object_inner` → `Tlab::next_refill_size()` → gate
(`young_bump_headroom` / `young_has_free_block`) → `Tlab::retire()` (installs the
`int[]` tail filler) → `gen_heap::refill_tlab(requested)` → `Arena::alloc` →
`write_bytes(0, size)` → `Tlab::new` + `begin_refill` (two `Instant::now()`).

### 1d. Slow-path object allocation / promotion

`try_alloc_young_initialized` takes the `young_from` mutex, `Arena::alloc`s,
records a sweep anchor, zeroes, initialises. Old-gen spill goes to
`OldGen::alloc`, a 28-bucket log2-segregated best-fit list.

---

## 2. Rust and JIT TLAB layouts — **they agree**

Verified field by field, not by comment:

| | Rust `Tlab::alloc_initialized` | JIT `emit_inline_tlab_new` |
|---|---|---|
| cursor offset | `CURSOR_OFFSET = 0`, `#[repr(C)]`, asserted by `test_tlab_offsets` | `helpers.tlab_cursor_offset_in_thread` = `JvmThread::tlab_offset() + Tlab::CURSOR_OFFSET` |
| end offset | `END_OFFSET = 8` | same derivation, `+ Tlab::END_OFFSET` |
| align-up | `(cursor + align-1) & !(align-1)`, align = 8 | `add r11,7 / and r11,-8` |
| reserved footprint | `(size + align-1) & !(align-1)` | `total_size` used raw |
| bound test | `new_cursor > end` → fail | `cmp / ja` → fail (`==` allowed both sides) |
| commit order | header stores, `Release` fence, then cursor | header stores then cursor (x86-64 TSO) |

The one asymmetry — the JIT does not round `total_size` — is **safe, and I
checked why rather than assuming**: `total_size = HEADER_SIZE(32) +
compact_body.unwrap_or(num_fields * SLOT_SIZE(16))`, and a compact body is
`8 * refs + 16 * prims`. Every term is a multiple of 8, so the JIT's raw
`total_size` already equals the Rust side's rounded footprint for `align = 8`.
The `debug_assert_eq!((total_size - HEADER_SIZE) % 8, 0)` in the JIT is the
guard on that. If a future layout change makes `CompactLayout::body_size`
non-8-multiple, the two allocators diverge by up to 7 bytes and the linear heap
walk desyncs — see cross-owner request X4 for making that a hard check.

**No divergence in the layout.** The divergence I did find is in *bookkeeping*,
and it was costing real throughput — section 3.

---

## 3. Fixes landed

### FIX-1 — TLAB sizing was blind to the JIT, and ratcheted one way (`tlab.rs`)

`TlabPressureTracker::alloc_count` only counts allocations that go through
`Tlab::alloc_initialized`. Compiled code bumps the cursor inline and never
touches the tracker, so a JIT-driven thread reports `alloc_count == 0` for its
entire TLAB lifetime. The heuristic read that as *idle*:

```rust
let shrink = elapsed_ms > 100 || alloc_count < SLOW_REFILL_ALLOC_COUNT;  // 4
```

`shrink` was therefore **permanently true** for any thread running compiled
code. Consequences:

- Any TLAB that took ≥ `FAST_REFILL_THRESHOLD_MS` (1 ms) to fill was halved.
  Applied repeatedly this walks 256 KiB → 8 KiB (`MIN_TLAB_SIZE`).
- It is a **one-way ratchet**: at the floor, a furiously-allocating thread has
  `grow = true` *and* `shrink = true`, which the heuristic resolves as "keep".
  The thread could never climb back, whatever its allocation rate.

An 8 KiB TLAB refills 32x more often than the baseline, and every refill is a
`young_from` mutex acquisition, a `write_bytes` over the whole chunk, a
tail-filler header write, a free-list search and two `Instant::now()` calls.

Fix: `Tlab::consumed_bytes()` exposes the live `cursor - start` span — the only
measure that sees the inline bump — and `Tlab::next_refill_size()` feeds it to
`TlabPressureTracker::next_refill_size_with_consumed()`. A TLAB that was
actually drained (≥75% consumed) is no longer classified as idle, and gets the
grow arm when it drained inside the slow-fill window.

`Tlab::next_refill_size` must keep being called **before** `retire()` (the
cursor is read live). It is: `interpreter.rs:2821` vs `:2893`, the sole caller.

Interpreter-path behaviour is unchanged by construction — the tracker's own
byte tally is used when it is the larger of the two, and all eleven pre-existing
sizing tests were walked through the new predicate by hand before landing.

Tests added: `consumed_bytes_sees_a_raw_cursor_bump` (simulates the JIT's raw
cursor store), `jit_drained_tlab_does_not_ratchet_down`,
`jit_drained_tlab_climbs_back_from_the_floor`,
`barely_used_tlab_still_shrinks_with_consumption_signal` (the idle arm must
survive), `tlab_next_refill_size_feeds_its_own_consumption`.

### FIX-2 — the small free-list tier is now size-segregated (`arena.rs`)

The non-moving sweep hands every dead object back via `add_free_block`, so the
small tier is where the production allocator lives. It was a single flat `Vec`
walked first-fit with a 16-block scan budget, and its own doc comments record
the resulting three-stage pathology:

1. `swap_remove` back-fills the scan prefix with freshly split dust, so a
   same-shaped request stops first-fitting at index 0;
2. the bounded scan then misses satisfiable blocks, which fall through to the
   bump tail — gone, once the non-moving cursor pins at capacity — and then to
   an unbounded rescue scan;
3. `largest_free_block()` walked the whole thing, and it is called by
   `gen_heap::refill_tlab`'s fragmentation fallback on every refill the main
   path could not serve. In that mode the fallback hands out
   `FRAG_TLAB_FLOOR`-sized mini-TLABs — as few as ~5 objects each — so an
   O(free-list) walk over the live-captured 33 MB of uniform 4080-byte remnants
   (~8500 blocks) was running roughly *per allocation*.

Fix: one bucket per **exact 8-byte size class** (`SMALL_BUCKETS = 512`, covering
everything below `LARGE_BLOCK_MIN`) plus a 512-bit occupancy bitmap.

- A request lands on its own class. Same-shaped recycling is an **exact fit with
  no remainder**, so the allocator stops manufacturing dust at the source.
- A miss escalates via the bitmap. Indexing by `size / 8` *floored* guarantees
  every block in bucket `k` is ≥ `k*8`, so for `align <= 8` the next occupied
  class always covers size + worst-case padding — the head of that bucket is
  taken with no scan, and no satisfiable class is skipped.
- `largest_free_block` is `mask_last()` for the small tier and a memoised value
  (`large_max_exact`, a `Cell` — `Arena` always lives inside a `Mutex`, which
  needs `Send`, not `Sync`) for the span tier. It still answers a value that is
  genuinely allocatable, never an over-estimate: callers size a subsequent
  `alloc` from it, and an over-estimate turns straight into a failed refill.
- `has_free_block_at_least` no longer walks the small tier on a miss.
- `max_free_upper` stays a sound *upper* bound (`small_max_ceil`), while
  `largest_free_block` uses the *lower* bound (`small_max_floor`). The two
  coincide for every block the heap actually produces; the 7-byte spread exists
  only so both directions stay sound if the 8-byte-grid tripwire ever fires.

The 16-block scan budget and the post-bump-tail rescue scan are now redundant
(the class search is complete). The rescue arm is kept — it only runs on a path
that was about to return `None` — but its comment now records that it is a
safety net, not load-bearing.

Tests added: `small_bucket_mask_probes_are_exact` (bit 0, word boundaries, last
bucket, out-of-range start), `small_tier_exact_class_reuses_holes_without_splitting`
(500 node-sized holes recycled with the free list shrinking by exactly one node
each time — the no-dust property), `largest_free_block_is_allocatable_across_tiers`,
`max_free_upper_stays_a_sound_upper_bound`,
`small_tier_escalates_to_the_next_occupied_class`,
`free_blocks_sorted_spans_every_size_class` (the sweep's hole map must still see
every block, in ascending offset order, whichever class it landed in).

---

## 4. Audited, no change needed

**`pinned.rs`** — the JNI keep-alive pin set is refcounted, gated behind a
relaxed `PINNED_COUNT` load so the no-pins path never locks, keyed by object
base and re-keyed after a moving collection (`update_after_gc`). It is *not*
cleared per cycle, and that is correct: entries are owned by
`Get…ArrayElements`/`Release` pairs that legitimately span collections. Bounded
by the number of live JNI critical sections; over-pinning here would cost
retention, not correctness, under the non-moving sweep. Nothing to fix.

**`old_gen.rs` promotion** — `promotion_oom_risk` (`gen_heap.rs:3700`) is
**recomputed per collection, not latched**: it is a fresh `old_used*10 >=
old_cap*9 && young_used*10 >= young_cap*9` test, and it is only *honoured* when
conservative JIT roots are actually present. It cannot wedge the heap in a
degraded mode on its own. The historical wedge (staying non-moving forever,
which skips the mark-compact that would relieve old-gen pressure) was closed by
the `honor_promotion_oom_risk` narrowing. No change.

**`old_gen.rs::alloc`** — 28 log2 buckets, best-fit within a bucket, escalating.
One latent O(n): the within-bucket scan only breaks early on `waste == 0`, so a
bucket holding many blocks that all fit with non-zero waste is scanned in full
on every allocation (e.g. 100k free 56-byte blocks against a 48-byte request).
Exact-size recycling — the common case — hits `waste == 0` at index 0. I left it
alone: it is one size class away from the small-tier problem I just fixed, but
old-gen allocation is per *promotion*, not per allocation, and the fix would be
the same 8-byte-class treatment applied to a file that is much less hot.
Recorded here so the next pass does not have to rediscover it.

**`heap.rs`** — the semi-space `Heap` is not the production allocator
(`GenerationalHeap` is; see the memory note "Default collector is Generational").
Its `alloc_object`/`try_alloc_*` path is a mutex + `Arena::alloc` per object with
no TLAB at all, but it is only reached by the standalone semi-space collector and
tests. `compact_oop_scan`'s thread-local single-entry layout cache is a genuine
hot-path win on the *GC* side and is correctly generation-validated. No change.

---

## 5. Best hypothesis for HashMap 1M (42.7x) being worse than 10M (21.2x)

The inversion is **mostly a fixed cost, not a per-allocation constant**, and the
numbers say so directly. From BENCHMARK.md's three-way run:

| | CratonVM 1M | CratonVM 10M | per-op |
|---|---|---|---|
| | 3,084 ms | 25,063 ms | 3.08 µs vs 2.51 µs |

CratonVM scales **8.1x for 10x the data** — sub-linear. HotSpot scales 26x
(54 → 1,415 ms) over the same range, because at 1M its absolute is small enough
that its own fixed costs dominate the *ratio*. Fit a constant: ~600 ms fixed
plus ~2.5 µs/op reproduces both CratonVM rows to within a few percent. The
"inversion" is therefore ~0.5–0.6 s of one-time cost — class loading, synthetic
JDK bring-up and JIT warm-up — amortised over 10x fewer operations, *not* a
per-allocation cost that gets worse at small n.

The genuinely per-op excess (3.08 vs 2.51 µs, ~23%) is consistent with the
interpreted warm-up window being a larger fraction of a 1M run, which points at
the same place: **the interpreted allocation path, whose three lock-prefixed
RMWs per object (§1b) are ~10-20x the cost of the whole compiled bump.**
`HashMap.put` allocates a `Node` per insertion, so the warm-up window is
allocation-saturated by construction.

Two testable predictions, in priority order:

1. Time `HashMapOnly` at n=1M with `-XX`-equivalent JIT threshold 0 vs the
   default 500. If the fixed-cost read is right, the gap collapses at 1M and
   barely moves at 10M.
2. Remove the two `shared.mem` counter RMWs from the TLAB hit path (X1) and
   re-measure 1M. A ~10-15% move on 1M with a smaller move on 10M confirms the
   warm-up-window component.

Note that the confirmed regression window `a36b9d121..e57f0bc7d` moved 1M by
7.1x and 10M by 8.9x — i.e. it hit both scales *similarly*. Whatever landed in
that window is a per-op regression, and is a **separate** investigation from
this inversion; do not conflate them. BENCHMARK.md's own advice (bisect on
HashMap 1M, ~8 steps) still stands and is the higher-value next action.

---

## 6. Cross-owner requests

I did not make these. Each names the exact file and function.

### X1 — `vm/src/runtime/interpreter.rs::tlab_alloc_object_inner` (owner: vm)

The TLAB **hit** path performs two `fetch_add(Ordering::Relaxed)` on
process-shared cache lines per allocation: `shared.mem.tlab_hit_count` and
`shared.mem.bytes_allocated_total`. Both are pure statistics. On a multithreaded
allocation storm they are the dominant cost of the fast path (each locked RMW is
~20-40 cycles uncontended and hundreds under sharing), and they more than double
the instruction count of the bump itself.

Request: accumulate both **per thread** (they can live next to the TLAB, which
is already `&mut`-exclusive to its owner) and flush into the shared counters at
TLAB refill — once per ~10k allocations instead of once per allocation.
`bytes_allocated_total` is read by the wedge-breaker's re-arm check, which is
already explicitly documented as tolerant of a stale/lagging value.

### X2 — `gc/src/gen_heap.rs::next_identity_hash` (owner: gen_heap)

Called once per interpreted allocation (`init_object_header`) and once per
non-skipped JIT allocation (`jit_post_tlab_init`), each a locked `xadd` on one
global counter — a single contended line across every allocating thread.

Request: hand out per-thread **blocks** of the hash space (e.g. reserve 1024 ids
with one `fetch_add`, then increment a thread-local cursor). Identity hashes only
need to be distinct, not ordered. The JIT's inline path already writes 0 and
relies on the lazy-mint contract, so this only affects the two slow paths.

### X3 — `jit/src/x64.rs::try_compile` / `emit_inline_tlab_new` (owner: jit)

`skip_post_init_helper` is documented as defaulting to the conservative `false`.
When it is false, every compiled allocation calls `jit_post_tlab_init`, which
does `note_jit_boundary()`, a `class_layout()` registry lookup, a locked
`next_identity_hash()`, an `a2dbg::record` and `jit_post_alloc_init` — turning a
~20-cycle inline bump into a helper call with a lock and a map lookup.

Request: quantify how often `skip_post_init_helper` is actually true for the
benchmark classes (`HashMap$Node`, `BinTreesClassic$TreeNode`). If it is usually
false, widening the static "no non-zero-tag primitive init and no finalizer"
proof is worth far more than any further micro-tuning of the bump sequence.

### X4 — `jit/src/x64.rs::emit_inline_tlab_new` (owner: jit)

The inline path bumps by a raw `total_size` while `Tlab::alloc_initialized`
reserves `(size + align - 1) & !(align - 1)`. They agree **only because**
`CompactLayout::body_size` is always a multiple of 8 today. The sole guard is a
`debug_assert_eq!((total_size - HEADER_SIZE) % 8, 0)`, which is compiled out of
the release builds that run the benchmarks. A violation is silent heap
corruption (off-grid objects desync the non-moving linear walk), not a slowdown.

Request: make it a hard check at JIT-compile time — if
`(total_size - HEADER_SIZE) % 8 != 0`, fall through to the `new_object` helper
instead of emitting the inline bump. Compile-time cost, zero runtime cost.

### X5 — `gc/src/gen_heap.rs::refill_tlab` (owner: gen_heap) — optional

`refill_tlab`'s fragmentation fallback calls `largest_free_block()` and then
immediately allocates `largest.min(actual_size)`. After FIX-2 that call is cheap
for the small tier, but it still recomputes the span-tier maximum whenever the
previous refill consumed the largest span.

Request (only if profiling still shows it): add and call a capped variant —
`Arena::largest_free_block_capped(cap)` — that early-exits as soon as it finds a
block ≥ `cap`, since the caller never uses more than `actual_size` anyway. I can
add the `Arena` side on request; the call-site change is yours.
