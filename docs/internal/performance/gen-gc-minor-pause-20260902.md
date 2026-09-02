# The generational minor pause: 88% of it was not the collection

Slug: `gen-gc-minor-pause` · 2026-09-02
Follows `moving-young-throughput-20260726.md`, whose three named residuals this
page closes two of, corrects one of, and replaces with measurements.

---

## VERDICT

**In steady state, ~12% of a minor collection copied live objects.** The rest
was two whole-space linear walks and a card-map scan that returned zero — all
of it single-threaded, all of it on the DEFAULT path.

`bench/OldGenRsetProbe 19 700 16` at `-Xmx1g`, `-XX:+UseGenerationalGC`,
`CRATONVM_DBG=gcpause`. Medians of the 12 collections after the retained set
tenures into old gen (total median pause 229 ms, n=12):

| phase | before | after | scales with |
|---|---:|---:|---|
| from-space object-start walk | 120 ms | **22–39 ms** | young *allocated* |
| `full_old_rset_scan` | 50 ms | **0 ms** | old live set |
| `cheney_drain` — the actual copy | 29 ms | 32–63 ms | young *live* |
| `cardclear+young_reset` | 23 ms | 28–52 ms | card count |
| `scan_dirty_cards` | 8 ms | **0 ms** | old-gen size |
| **total pause** | **229 ms** | **100–133 ms** | |

A generational collector exists so a young pause costs O(young live set). The
two largest phases were O(young **allocated**) and O(**old** live set), and the
phase that is actually O(young live set) was the smallest of the three.

After: only 6 of 14 collections cross the 100 ms threshold `gcpause` reports
at; before, every steady-state collection did. `cheney_drain` and
`cardclear+young_reset` did not move outside host drift, and **the copy is now
the largest phase** — which is the shape a generational young pause is
supposed to have.

---

## Method, and what it is not

The host was a 32-core Windows box running at 79–100% CPU from unrelated
sessions throughout. **No absolute millisecond figure on this page is this
collector's speed.** Every claim below is one of three things that survive
that: a ratio within a single run, an exact counter, or an interleaved paired
A/B where all rounds agree in sign.

The probe retains a large tree and never mutates it, so it has genuinely zero
old→young edges. That is deliberate — it is what makes the two rset costs
readable as pure waste — and it is also why it is a *best* case for the F2
change below and a *worst* case for F3. A workload that mutates tenured objects
will move both, which is why `bench/OldToYoungEdgeProbe` exists as its
companion: it stores a freshly allocated young object into a tenured node's
field tens of millions of times and then checks every one of them, so it is the
shape on which a card-table-only collector can actually be wrong.

---

## F1 — the default path was the sequential one

`pre_evacuate` covered the interval from the top of `collect_garbage_inner` to
the card scan: the safepoint-token spin, the arena locks, the bounds publish,
the card-buffer drain **and** the exact from-space object-start walk, under one
name. Attributing 52% of a pause to a mark that wide is a hypothesis, so the
walk got its own `objstart_walk` mark and an `objstart_walk_bytes` counter
*before* anything was optimised.

That ordering paid for itself immediately: it is what made the F1 regression
below visible as a number instead of as a rounding error inside a phase that
also covers a spin loop. Measured after the split, `pre_walk` is **0 ms** --
the walk was the whole of `pre_evacuate`, which the original 52% figure could
only assume.

The walk is a linear header chase over **allocated** bytes (268 MB per cycle
here) whose two jobs are an exact object-start predicate for conservative roots
and a proof that from-space is parseable.

The non-moving sweep — the *fallback* path — already solved exactly this:
allocator-supplied anchors on a 4 KiB grid, chunked across workers, each chunk
re-proving its own anchor by requiring its chain to land exactly on the next
one, abandoned wholesale on any grid anomaly. `drain_parallel`,
`zero_spans_parallel` and `young_gc_threads` are reached only from
`sweep_young_non_moving`. The moving path — the default since 2026-07-30 — used
none of it.

### The first fix made it worse, by 2x

Sharing the bitmap meant making `ObjectStartBits` atomic. The first version
also kept its `len` as an `AtomicUsize` bumped inside `insert` -- **one
process-global cache line taking a `lock add` from every worker for every
object in from-space**, about 6.7 M of them on a 268 MB space. Same binary,
`CRATONVM_GC_PAR_THREADS=1` as the lever:

| arm | `objstart_walk` |
|---|---|
| parallel (8 workers) | 443 / 513 / 522 / 465 ms |
| sequential | 236 / 274 / 255 / 227 ms |

The "parallel" walk was **slower than serial**, and the serial path had itself
regressed from ~120 ms because it paid the same atomics. A shared counter
incremented once per unit of work is the canonical way to make a parallel loop
lose, and it was maintained for a value nothing outside the unit tests reads.

### The real fix, and the same measurement afterwards

`len()` now popcounts on demand, and a `StartRun` cursor batches bit sets into
one atomic OR per 512-byte word (~12 objects at typical sizes), flushed
explicitly before anything reads the bitmap -- `Drop` alone would flush at the
end of the collection, long after `forward_object` starts consulting the bits.

Release-mode direct measurement of the two walkers, 128 MiB from-space,
1,677,723 objects, 16 chunks, 8 workers, identical `starts` in both arms:

| rep | sequential | parallel(8) | speedup |
|---|---:|---:|---:|
| 0 | 25.6 ms | 8.5 ms | 3.02x |
| 1 | 25.9 ms | 11.6 ms | 2.23x |
| 2 | 29.7 ms | 18.1 ms | 1.64x |

End-to-end, same binary, same lever:

| arm | `objstart_walk` |
|---|---|
| parallel | **23 / 38 / 22 / 39 ms** |
| sequential | 166 / 189 / 169 / 170 ms |

So the default path went 120 ms -> 500 ms -> **22-39 ms**. The middle number is
the point of this section: without F0's phase mark it would have shipped.

### The three properties that make the fallback safe

Each is a test:

* the chunked walk produces a **bit-identical** bitmap to the sequential one
  (`the_parallel_object_start_walk_agrees_with_the_sequential_one` — compared
  bit-for-bit over the whole span, not by count: two walks can record the same
  *number* of starts and disagree about where they are);
* an anchor that is not an object start **abandons** the attempt
  (`the_parallel_object_start_walk_refuses_an_anchor_that_is_not_a_start`);
* a failed attempt gets a **fresh** bitmap for the sequential fallback. A
  partially-filled one is worse than none — a bit set by a chunk that walked
  from a bogus anchor names a non-object, and `forward_object` relocates
  through exactly those.

`objstart_chunk` is deliberately *stricter* than the sequential walk: where
`skip_free_blocks` resyncs a cursor that landed inside a free block, the chunk
refuses. It walks from an anchor rather than from offset 0, so a cursor inside
a free block means the grid and the free list disagree and the exact-landing
proof is worth nothing if the walk may guess its way back. That asymmetry is
also what makes the duplicated stepping logic safe: every disagreement returns
`None`, so drift between the two walkers can cause an unnecessary fallback but
never a wrong bitmap.

---

## F2 — the whole old generation, walked on every young GC, by default

`full_old_rset_scan_enabled()` was `!card_table_only`, and
`CRATONVM_CARD_TABLE_ONLY` is opt-in. So `scan_dirty_cards` ran, and then
`scan_all_old_to_young` ran anyway: a full `OldGen::walk_objects()` — which
materialises a `Vec` of every tenured object — scanning every reference slot of
every one of them, on every young collection.

`moving-young-throughput-20260726.md` named it and left the A/B open: *"Whether
it helps, hurts, or is neutral is open and needs a quiet host."* Measured now,
on the shape built for it. One binary, one env var, A,B,A,B,A,B interleaved,
identical program checksums and identical `minor=14` in every arm:

| arm | R1 | R2 | R3 | min | median |
|---|---:|---:|---:|---:|---:|
| default | 16194 | 15714 | 16138 | 15714 | 16138 |
| `CRATONVM_CARD_TABLE_ONLY=1` | 15165 | 15080 | 13940 | **13940** | **15080** |
| paired delta | −1029 | −634 | −2198 | **−11.3%** | **−6.6%** |

All three paired rounds favour the card-table-only arm.

**Flipped to off by default.** It was never a bug — it was a standing premium
against a missed write barrier, and a missed barrier really would free a
reachable young object. What replaces it is the same walk as a *checker*:

```
CRATONVM_GC_VERIFY_RSET=1   →  [rset-verify] site=moving edges=N missing=M seeded=S
```

which is the shape G1 already uses (`CRATONVM_G1_DBG_RSET`). It prints `edges`
as well as `missing` for the reason that page gives: a `missing=0` on a run
whose old generation held no old→young edges at all is vacuous, not clean, and
that is the most common shape.

`CRATONVM_GC_FULL_RSET_SCAN=1` is the revert lever.
`CRATONVM_CARD_TABLE_ONLY=1` keeps its documented meaning and still forces the
scan off, so a script that sets it is unaffected.

### The test that changed sides

`full_old_rset_scan_preserves_unbarriered_old_to_young_ref` asserted that an
edge installed by a **raw**, barrier-bypassing store survives a collection —
which was true only because of the full scan. It is now two tests that state
the new contract:

* `the_rset_verifier_reports_the_edge_the_card_table_did_not_deliver` — the
  verifier *can* fail (unseeded: `edges >= 1`, `missing >= 1`) and does not cry
  wolf (seeded: `missing == 0`);
* `an_unbarriered_old_to_young_store_is_not_preserved_by_the_card_table_alone`
  — asserted against `full_old_rset_scan_enabled()` rather than against one
  hard-coded outcome, so it states the trade in both configurations.

This is not a coverage regression for any real store: every mutator reference
store reaches `VmHeap::set_field` / `set_array_element` and therefore the
barrier, by construction.

### The verifier has been run, and not vacuously

`bench/OldToYoungEdgeProbe` is the shape built to make `edges` non-zero:

| run | result |
|---|---|
| 20k nodes, 200 rounds, `-Xmx128m`, card table alone | `edges_verified=20000`, `minor=55 major=4`, `old_to_young_edges=1,201,502` |
| verifier, `-Xmx128m` | 34 passes, **all `missing=0`**, up to `edges=771,714` |
| verifier, `-Xmx320m`, 40k nodes | `site=moving edges=44894 missing=0 seeded=44894`, plus 34 non-moving, all clean |

Both seeding paths covered, with non-zero edge counts. The three `edges=0`
lines in the first verifier run are the cycles before anything tenured --
correctly identified as vacuous by the column that exists to identify them.

**Still owed:** the same verifier armed over a suite-scale run (Tomcat, H2,
Spring). Until then, `CRATONVM_GC_FULL_RSET_SCAN=1` is the answer to any
premature-reclamation report under Generational.

---

## F3 — two O(cards) atomic passes per cycle, to find nothing

`take_dirty_cards` merged its tracking list with a `swap(AcqRel)` over **every**
card byte — a locked read-modify-write per 512 bytes of old gen, executed
whether or not the byte was dirty. The full scan is unavoidable (the JIT's
direct card stores never enter the tracking list), which also means the whole
per-thread-buffer → `pending_offsets` → `drain_pending` pipeline bought the
collector nothing. Then Phase 3's `clear_all` stored `CARD_CLEAN` over every
byte again.

Holding the workload fixed and scaling only the card map isolates it. Every arm
reported `dirty_scanned=0` and `old_to_young_edges=0`, so the whole cost *is*
the scan:

| `-Xmx` | old gen | cards | passes | total ms | ms/pass | ns/card |
|---:|---:|---:|---:|---:|---:|---:|
| 96m | 48 MiB | 98,304 | 11 | 9.12 | 0.83 | 8.4 |
| 192m | 96 MiB | 196,608 | 10 | 18.29 | 1.83 | 9.3 |
| 384m | 192 MiB | 393,216 | 10 | 31.96 | 3.20 | 8.1 |
| 1g | 512 MiB | 1,048,576 | 15 | 123.91 | 8.26 | 7.9 |

Linear across a 10× range, ~8.3 ns/card. Extrapolated: a 4 GiB old gen is 8.4M
cards ≈ **70 ms of empty scanning on every minor GC**.

**Fixed** by reading first. Both passes now `load(Acquire)` — a plain `mov` on
x86-64, an `LDAR` on aarch64 — and store only on the bytes that are genuinely
dirty. Same probe, same `rset_bytes=1048576`, so the same card count:

| | refinement | passes | ms/pass |
|---|---:|---:|---:|
| before | 123.906 ms | 15 | 8.26 |
| after | 11.274 ms | 14 | **0.81** |

**10.2x on the phase**, and `scan_dirty_cards` now rounds to 0 ms in the pause
breakdown.

The documented pairing is preserved exactly: what has to happen-after
the JIT's release byte-store is an *acquire read* of that byte, which is what
the load is. `clear_all`'s conditional store is also strictly safer than the
unconditional one under a hypothetical concurrent writer: a card dirtied
between the load and the store survives, where the old code wiped it.

---

## F4 — one hash insert per survivor, into a table that started empty

`pointer_map` takes exactly one insert per surviving object:
`pointer_map_len = fwd_copies = 1,446,210` in a single 450 ms drain. It started
every cycle at capacity zero, so building it reallocated and rehashed about
twenty times, moving roughly 2.8M entries on top of the 1.4M real inserts. That
growth is the cost `moving-young-throughput-20260726`'s profile named:
`RawTable::reserve_rehash` at 8.3% of the whole process beside
`HashMap::insert` at 40.7%.

**Correcting that page's residual, and this investigation's own first
suggestion.** Both proposed a sorted `Vec<(from, to)>` — push on the collector
side, binary search on the consumer side. The call sites refute it:
`pointer_map.get(..)` appears **127** times against **18** `insert`s. It is a
read-mostly structure with point lookups, so trading O(1) hashing for ~21
cache-missing probes per `get` would buy a cheaper build with a more expensive
everything-else.

**Fixed** by pre-sizing from the previous cycle's survivor count
(`GenerationalHeap::prev_survivor_count`). A live set is stable enough between
consecutive collections for the last count to be a good guess, and a wrong
guess only costs the growth it would have paid anyway.

**Not separately measurable on this probe.** `cheney_drain` did not move
outside host drift and there is no kill switch to A/B it against, so this
change rests on the mechanism and on `pointer_map_len` (154,084-156,256 per
steady-state cycle, 1,446,210 on the pre-tenure ones) rather than on a pause
number. Parallel evacuation — the real answer to the remaining
`cheney_drain` — is explicitly out of scope here and remains open.

---

## F5 — the write barrier, both halves

**The Rust half** went through a TLS lookup, an `Arc` deref, a
`parking_lot::Mutex`, a linear scan of a table-id vec-map and a `Vec::push`
that may reallocate — per reference store, with no deduplication, so a field
written in a loop buffered one entry per store all naming the same card.
`duplicate_card_marks` is the counter that measured exactly that. The
indirection existed only because the byte map was unreachable without the
collector's mutex. `CardTable::cards_addr` now caches its base (the map is
allocated once in `new` and never resized — the same reasoning
`jit_cards_addr` has always relied on), so `mark_dirty_lockfree` does what the
JIT has always done: a bounds check, a shift, a relaxed load and, only if the
card is not already dirty, one release byte store. **There is now one
card-marking rule in this VM instead of two**, which is also the precondition
for ever revisiting `inline_card_mark_available`.

The conditional store is not only cheaper: re-storing `CARD_DIRTY` over an
already-dirty byte writes a line every other storing mutator may hold, so an
unconditional mark ping-pongs the card line between cores on exactly the
workload where the barrier is hottest.

**The JIT half** bailed to `jit_putfield_object` on ANY non-null old field
value, unconditionally — while `satb_barrier`, the thing it bailed *to*, asks
whether marking is active first and returns in two instructions. Overwriting a
non-null reference field is one of the most common stores in Java. The receiver
at that point has already been proven young (the old-generation test bails
first), so with no mark cycle running it owes no barrier at all.

**Fixed** by publishing a process-global SATB arming counter
(`cratonvm_gc::satb_armed_addr`, helper-ABI v10) and testing it inline
(`Compiler::emit_satb_pre_barrier_gate`, one gate shared by all three call
sites so they cannot drift).

Two things make this sound rather than a race:

* the address is baked, the **value** is read at runtime, so a cycle that arms
  after a method is compiled is seen — pinned by case 5 of
  `inline_ref_putfield_satb_bail_is_gated_on_a_live_mark_cycle`, which disarms
  the counter and gets the fast path back *in the same compiled body*;
* a mutator cannot read a stale zero and then store into a live mark cycle. The
  counter is armed by `set_phase(ConcurrentMark)` inside
  `ConcurrentMarker::initial_mark`, which runs during the initial-mark STW
  pause with every mutator parked. This is the same guarantee the interpreter's
  `satb_barrier` already relies on.

It is a **counter**, not a flag, because a VM host may own more than one heap;
a bare flag would let heap A leaving its mark phase disarm the barrier while
heap B is still marking. Every way the count can be wrong (conservatively high,
or leaked by a heap dropped mid-mark) runs *more* barrier code, not less.

---

## F6 — a quarter of the heap is nursery

`with_capacity` splits `-Xmx` as ¼ from-space, ¼ to-space, ½ old gen, and the
trigger fires at 50% of from-space. At `-Xmx1g` that is 128 MB of allocation
per cycle out of a 1 GB heap. The stated reason is "so the to-space can hold
all survivors" — but to-space has the *same* capacity as from-space and
promotion drains survivors to old gen on top of that, so the copying
collector's real constraint permits considerably more.

**Shipped as a knob at its historical default**, not as a new default:
`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` (default 50, clamped 1..=95). Raising it
collects less often but raises the peak survivor volume one to-space must
absorb, and a cycle that cannot fit them diverts to the non-moving sweep and
spills to old gen. Which effect wins is a property of the workload's survival
rate.

### The sweep was run, and the knob does nothing here

Interleaved, three rounds:

| trigger | with the 200 ms pause goal | with `YOUNG_PAUSE_MS=0` |
|---|---|---|
| 50 % | `minor=14`, 14690 / 14798 / 14616 ms | `minor=14`, 15532 / 14727 / 14962 ms |
| 75 % | `minor=14`, 14291 / 14516 / 15069 ms | — |
| 90 % | `minor=14`, 14096 / 15463 / 15501 ms | `minor=14`, 15251 / 20952 / 14339 ms |

**Identical collection counts at every setting, with and without the pause
goal.** The reason is in the counters, not the timings: `young_bytes_before` is
268,435,400 — the from-space is **full** at collection time, not half full.
These collections are driven by allocation FAILURE, not by `needs_gc()`, so the
threshold never gets to fire at all. On a JIT-allocating workload the inline
TLAB path exhausts the arena before the trigger is ever consulted.

So the knob ships at its historical default and the
50%-is-2x-conservative hypothesis is still **untested** — but it is now known
what would test it: a workload whose collections are threshold-driven, which
this one is not. That is a sharper statement of the residual than "the sweep is
owed", and it is what running the sweep bought.

---

## Not established, and one in-tree claim withdrawn

* **The young pause goal has no measured throughput effect, in either
  direction.** `adapt_young_trigger_to_pause` was observed halving the trigger
  128 MB → 64 → 32 → 16 MB across three consecutive collections without ever
  reaching its 200 ms goal — a multiplicative decrease reacting to a pause
  dominated by copying a fixed live set, which shrinking the nursery cannot
  reduce. A/B'd (`CRATONVM_GC_YOUNG_PAUSE_MS=0` vs 200, three interleaved
  rounds, 700-round probe): **identical collection counts (14 in every arm) and
  no separable wall time.** The mechanism is real; the cost is not established,
  and it is not filed as a defect.

* **`moving-young-throughput-20260726.md` is wrong about re-encounters.** It
  records *"`fwd_copies` and `fwd_reencounters` are roughly 1:1"*. Measured on
  this tree: `fwd_copies=1,446,210` against `fwd_reencounters=5,726` — **0.4%**.
  Any optimisation aimed at `forward_object`'s already-forwarded re-encounter
  path is worth nothing on this shape, which retires the third of that page's
  three residuals without doing it.

* **The mutator-side card buffering is now dead but not deleted.**
  `THREAD_DIRTY_BUFFER`, `BUFFER_REGISTRY`, `DirtyPartitions`,
  `thread_local_dirty*` and `flush_dirty_buffer` have no caller on the mutator
  path any more; `flush_all` + `drain_pending` at GC start are walks over empty
  buffers. Deleting them removes the V6 cross-thread-drain and DoHead
  orphan-buffer fixes' machinery, which deserves its own change and its own
  soak rather than riding along with a performance batch.

* **`duplicate_card_marks` is now structurally zero.** It counted buffered
  offsets naming an already-dirty card, and the barrier no longer buffers —
  the duplicates are *avoided* by the conditional store rather than counted.
  The counter is not broken; there is nothing left for it to see on this path.

* **F5 is not separately measured.** The probes here are JIT-allocating and
  old-receiver-light, so neither the lock-free card mark nor the inline SATB
  gate shows up as a pause number. Both are argued from the instruction
  sequences and pinned by tests; a barrier-heavy throughput measurement is
  owed.

* **Absolute pause numbers.** See Method. The shares and the counters are what
  this page establishes.

---

## Two things deliberately NOT done, and what would justify them

**A second-level card summary** (one bit per 4 KiB of card map, so a clean
region is skipped without being read) would take F3's scan from O(cards) to
O(dirty regions). It is not here because the JIT's inline post barrier writes
the byte map directly and cannot maintain a summary beside it — so a summary
kept only by the Rust side would be a correct-today, latent trap for whoever
re-enables that emitter. The same objection rules out reading the map eight
bytes at a time: `AtomicU8` cannot soundly be read as a `u64`, and the
alternative (a non-atomic word read justified only by the STW invariant) buys
a constant factor by trading a type-level guarantee for a comment. The
`Acquire`-load form takes the locked RMW out and is unconditionally sound,
which is the better trade at this size.

**Re-enabling `inline_card_mark_available`.** It returns `false`
unconditionally, so the emitter, the published `jit_card_table_info` metadata
and the acquire scan in `take_dirty_cards` that pairs with it are dead code on
every generational run today, and every compiled reference store with an
old-generation receiver takes a full helper call. The hold is a real WildFly
boot audit that observed an old `org/jboss/modules/Module` reference to a young
child on a clean card.

F5 removed the structural half of that objection: the Rust barrier and the
emitter now implement one rule rather than two, which is what makes an audit of
"does every compiled store form mark its card" a question about one rule
instead of a diff between two. What is still missing is the audit itself. The
flip needs a WildFly boot under `CRATONVM_GC_VERIFY_RSET=1` reporting
`missing=0` with non-zero `edges`, and it should not be attempted without one —
a missed card is a silent premature reclamation, which is the most expensive
class of defect this collector has.

---

## Reproducing

```bash
# Phase breakdown. Steady state is collection 4 onward, once the retained
# tree has tenured; the first three are dominated by cheney_drain instead.
RUST_LOG=error CRATONVM_DBG=gc-stats,gcpause \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx1g \
  -c bench OldGenRsetProbe 19 700 16

# F2's A/B. Interleave the arms; a block of A then a block of B measures the
# host, as the 2026-07-26 page's own inverted early pass demonstrates.
for i in 1 2 3; do for A in "" "CRATONVM_GC_FULL_RSET_SCAN=1"; do
  env $A cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx1g \
    -c bench OldGenRsetProbe 19 700 16
done; done

# F3's isolation. Hold the workload, scale only the card map; read
# refinement_ms / passes off the `[GC] cards:` line.
for X in 96m:800 192m:1600 384m:3200; do
  CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC \
    -Xmx${X%%:*} -c bench OldGenRsetProbe 16 ${X##*:} 12
done
```

Read `[GC] moving_young: cycles=N coverage_fallbacks=M` and the
`[GC] decision #N:` line before attributing anything to compaction. Every run
above reported `moving=N non_moving=0 coverage_fallbacks=0` — every collection
really did copy.
