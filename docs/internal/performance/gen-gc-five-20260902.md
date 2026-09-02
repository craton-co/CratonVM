# The generational minor pause, part two: five findings, the one the first fix exposed, and the crash the merge found

Slug: `gen-gc-five` · 2026-09-02
Follows `gen-gc-minor-pause-20260902.md`, which left the copy as the largest
phase and named parallel evacuation as "explicitly out of scope here". This
page takes the five ranked findings of the review that followed it, on the
MOVING young collection of `-XX:+UseGenerationalGC`.

**Read §3 first if you are here about a crash.** The gc unit gate had been red
on dev since parallel evacuation landed, and the cause is a rule this tree can
break again.

---

## VERDICT

1. **The young trigger was never consulted on compiled code.** The refill-time
   gate demanded 65,536 slow-path entries since the last GC; healthy TLAB flow
   makes a few hundred per semi-space. Every collection ran at allocation
   failure and the pause-goal loop moved a number nothing read. Fixed with a
   second, bytes-based re-arm metric. `young_bytes_before` now sits at the
   trigger rather than at capacity.
2. **The from-space memset left the pause.** `cardclear+young_reset` was
   18–33 ms of a ~105 ms steady-state pause; it is 0–1 ms with the wipe on a
   helper thread. It is a PAUSE win and **not** a throughput win — §2.
   `CRATONVM_GC_SYNC_YOUNG_WIPE=1` brings it back.
3. **Evacuation is parallel** — on **dev's** engine (`d86eeb816`), not this
   branch's, which was withdrawn. And it **faulted on the first parallel cycle
   of every fresh heap**: the evacuator writes to-space through its own cursor
   and so never reaches the one place that commits reserved granules. §3.
4. **Promotion goes through per-worker buffers** carved unzeroed from old gen,
   16 KiB doubling to 256 KiB, tails returned without stamping the reclaim
   epoch.
5. **The pointer map is sharded and built in parallel.** With the copy spread
   over eight workers the single-threaded fold of their pair lists was the
   largest sequential term left: `map_merge` 15–25 ms of ~105 ms. Sixteen
   shards, each thread folding a disjoint slice.

And the residual the first fix exposed: with the trigger reachable, the
pause-goal loop halved the nursery 136 → 67 → 33 → 19 MB across consecutive
tenure cycles whose pause is the fixed live set, and a probe run took 88
collections where 14 would do. A halving is now a trial judged by the pause
it produces (`next_young_trigger`).

---

## Method, and what it is not

Host: the 32-core Windows box, shared with other sessions' builds and suites
for the whole period. Every number below is a ratio within one run, an exact
counter, or an interleaved same-binary A/B; absolute milliseconds are this
host's, not the collector's. Probes: `bench/OldGenRsetProbe 19 700 16` at
`-Xmx1g` (a large tenured tree, no old→young stores; the pause-shape probe)
and `bench/OldToYoungEdgeProbe 40000 200` at `-Xmx320m` (the store-heavy
companion, run under `CRATONVM_GC_VERIFY_RSET=1`). Both print checksums that
are pure functions of their arguments; they were identical in every arm of
every run below, so no arm did different work.

**Two binaries are measured, and the difference matters.** `r1` carried this
branch's own evacuator; `r3` added the sharded map and the pause-goal trial.
The engine was then replaced by dev's in the merge, so every r1/r3 figure
about the ENGINE is historical and is labelled as such. §7 is the merged
binary.

---

## 1. The trigger nobody could reach

`tlab_alloc_object_inner` consulted `needs_gc_for_jit_allocation()` only
once `TLAB_SLOWPATH_ENTRIES_SINCE_GC >= 65_536`. That count was chosen for
the degraded modes the guard was written for — the crumb wedge enters the
slow path tens of thousands of times per second — and a bytes stamp was
rejected there because the per-object path never bumps one. Both were right
about their own mode. On healthy TLAB flow a 256 MiB semi-space is exhausted
in 300–1000 refills, so the gate never opened, and the 09-02 page's sweep of
`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` at 50/75/90 % returned identical
collection counts with `young_bytes_before == capacity` on every line.

Two re-arm metrics, OR-ed: the entry count for the wedge, and
`TLAB_REFILL_BYTES_SINCE_GC >= 4 MiB` for healthy flow. `needs_gc` keeps its
own anti-livelock floor, so consulting it every few TLABs cannot storm a
young gen whose live set sits above the threshold. Engagement: the default arm
now collects at `young_bytes_before` 101–137 MB against a 268 MB semi-space,
i.e. at the 50 % trigger, where the 09-02 page saw 268 MB.

**The sweep this unblocks is still owed.** F1 makes
`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` non-vacuous; it does not sweep it.

## 2. The wipe, off the pause — and what it is not worth

`Arena::reset` zeroed `[0, cursor)` and the high region inside the pause —
up to the semi-space capacity per cycle — and every one of those bytes is
zeroed again before an object lands on it (`refill_tlab` zeroes each TLAB,
`OldGen::alloc` zeroes each block). The documented blocker was that
`is_object_address` accepts any aligned address in either semi-space, so a
stale header in a reset arena could become a false root.

`Arena::reset_deferring_zero` does the metadata reset in the pause and hands
back the extents; at the end of the cycle — after a possible `grow`, which
can move the backing — `deferred_wipe_spans` turns them into committed
absolute spans and `spawn_evacuated_wipe` zeroes them on a named thread.
The arena is the next cycle's to-space, which no mutator allocates into;
the thread is joined at the top of the next collection and by `Drop`, and
`is_object_address` declines the inactive semi-space while
`wipe_in_flight` is set. `wipe_deferred_bytes` is the engagement counter.

Steady-state phase `cardclear+young_reset`: default 0–1 ms,
`CRATONVM_GC_SYNC_YOUNG_WIPE=1` 18–33 ms, on pauses of 100–140 ms.

**In wall clock it is worth nothing measurable**, and that is the honest
result. Three interleaved rounds on the r3 binary, `OldGenRsetProbe 19 700 16`:

| arm | wall (r1 / r2 / r3) | minor GCs |
|---|---|---:|
| default (off-pause) | 12117 / 12561 / 11991 ms | 29 |
| `SYNC_YOUNG_WIPE=1` | 12157 / 12330 / 11953 ms | 29 |

Identical within noise, in both directions across rounds. The work did not
disappear — it moved to a core that was idle *on this box*, and this probe is
single-threaded. A machine with no spare core pays it anyway, and a workload
with more mutator threads competes for it. The claim is the pause figure.

## 3. Parallel evacuation: dev's engine, and the crash in it

Two engines were written for this finding on the same day. **dev's landed
first (`d86eeb816`) and is the one kept**: persistent `evac_pool` threads
rather than a `thread::scope` per pause, copy-then-CAS forwarding on the
source mark word, per-worker to-space buffers whose retired tails carry the
existing `TLAB_FILLER` / `GAP_FILLER` sentinels, and a bufferless mode for a
cycle whose to-space slack is thin. This branch's engine was withdrawn
unmeasured against it: two mechanisms answering one question in one emitter is
the mistake `region_bounds_addr` is the standing example of.

### The gc gate had been red since it landed

`cargo test -p cratonvm-gc --lib` exits **0xc0000005
(STATUS_ACCESS_VIOLATION)** on a pristine `origin/dev` worktree, in
`a_parallel_copy_phase_follows_an_old_to_young_reference`.

**Cause.** Arena backing is reserved address space committed per granule
(`gc/src/reservation.rs`), and `Arena::hand_out` is where every allocation
path commits the granules it is about to write. The parallel evacuator is the
one path that never reaches it: `parallel_evacuation_region()` hands out a raw
address range and each worker bumps an atomic cursor and `memcpy`s in. A
to-space granule that no cycle has filled yet is reserved but **not mapped**,
so the copy faults rather than reading zero.

Only a heap's **first** parallel cycle can hit it — after one cycle the arena
has been written as a from-space, so its granules are committed. That
asymmetry is the entire reason a bt18 soak was green and a unit test was not,
and it is why the defect reads as flaky until you notice which cycle it is.

**Fix.** `Arena::commit_evacuation_region` before the phase opens; the driver
falls back to the serial evacuator if the OS refuses. `ParEvac::plan`'s
`region_end` became the phase's **consumption bound** — survivors, one
in-flight buffer per worker, and a now-BOUNDED abandoned-tail allowance —
rather than the whole to-space tail, so committing it does not charge the
reservation for a semi-space the cycle never touches. Without that bound the
commit would have re-introduced exactly the "charge all of `-Xmx` up front"
behaviour the reservation work removed.

**How it was found**, because the method is the transferable part: three
`eprintln!` phase markers, two rebuilds. A long read of the pool's termination
handshake and the CAS protocol found nothing, because neither was wrong.

### What the parallel copy is worth

`evac_drain` against the serial `cheney_drain`, steady-state cycles, r3
binary. The drain column cannot be read across arms — the one-worker arm
collects far more often, so each of its cycles copies less — but the pause
columns can:

| arm | pauses ≥ 100 ms | Σ those pauses | minor GCs |
|---|---:|---:|---:|
| default (8 workers) | 3 | 380–430 ms | 29 |
| `CRATONVM_GC_PAR_EVAC=0` | 5–13 | 1921–2769 ms | 46–57 |
| `CRATONVM_GC_PAR_THREADS=1` | 3–5 | 802–1038 ms | 54–56 |

## 4. Promotion buffers

Every promotion used to be `OldGen::alloc` under the old-gen mutex: a walk up
the size buckets, a best-fit scan inside one, the split remainder re-pushed,
the sorted free-list cache invalidated, and a `memset` of the block that the
copy then overwrote. A worker now carves a buffer with
`OldGen::alloc_unzeroed` (16 KiB, doubling to 256 KiB while the worker keeps
promoting) and bumps promoted objects out of it, so the mutex is taken once
per buffer instead of once per object. A buffer never ends 8 bytes short —
`old_lab_alloc` refuses the allocation that would leave that remainder,
because 8 bytes is below the free list's minimum block — and its tail goes
back through `OldGen::release_unused_tail`, which does **not** stamp
`reclaim_epoch`: the tail never held an object, so no concurrent-mark remark
snapshot can name an address in it, and a young cycle retiring its buffers
therefore does not invalidate an in-flight old-gen sweep.

**Not separately measured.** On these probes the tenure cycles promote ~1.2M
objects at once and the steady-state cycles promote almost nothing, so the
cycle that would show it is the one whose pause is dominated by the copy. The
mechanism is argued from the instruction sequence and pinned by
`releasing_an_unused_tail_keeps_the_reclaim_epoch`; a promotion-heavy A/B is
owed.

## 5. The map

`pointer_map` takes one entry per survivor and is read by ~160 `get` sites
after the pause, so the 09-02 page's rejection of a sorted vector stands.
What changed is that with the copy on eight workers, folding their pair
lists into one `FxHashMap` on the collector thread was the largest
sequential term left: `map_merge` 15–25 ms of a ~105 ms pause for 320–390k
survivors, ~60 ns per entry, cache misses on a table that does not fit L2.

`cratonvm_types::PointerMap` is now a struct of 16 `FxHashMap` shards
selected by `(addr >> 3) & 15` — adjacent objects land in different shards —
with the `HashMap` surface the 76 files naming it use, and
`par_extend_pairs(sources, threads)`: each thread owns a disjoint slice of
the shards and scans every source, inserting the pairs whose shard it owns,
so the build costs `N / threads` inserts of wall time. A lookup is the old
probe plus a shift and a mask. The five native helpers that were generic
over `&HashMap<usize, usize, S>` now take the map by name.

## 6. The residual the trigger exposed: a halving is a trial

With the trigger live, `adapt_young_trigger_to_pause` did what the 09-02
page predicted. On the debug probe run the tenure cycles copy 1.2–1.3M
objects and their pause is that copy; the loop halved the trigger 136 → 67 →
33 → 19 MB across consecutive cycles without the pause moving, and the run
took 88 collections where 14 would do. On the r3 A/B the same loop is why the
`PAR_EVAC=0` arm ran 46–57 collections against the default arm's 29: its
pauses were over the goal, so its nursery was shrunk, and every extra
collection cost a full copy of the live set.

`next_young_trigger` is pure and tested: a halving opens a trial; the next
over-goal pause judges it, and one that did not fall by a quarter reverts
the halving and latches on that survivor volume, so the loop leaves the
trigger alone until the volume changes by a factor of two — the signal
that the live set, not the nursery, was the pause.

## 7. The merged binary

One release binary (`r6`, the merge plus the `evac_drain` mark below), four
arms selected by one environment variable each, interleaved, three rounds.
`OldGenRsetProbe 19 700 16` at `-Xmx1g`. Checksums identical in all twelve
runs.

**The phase split, and a correction this page had to make to itself.** The
first merged measurement reported `map_merge = 127 ms` of a 141 ms pause. That
was wrong, and wrong in the way `gen-gc-minor-pause` F0 warns about: dev's
engine had no phase mark between the drain and the merge, so the `map_merge`
interval covered the copy as well. With `mv_phase!("evac_drain")` added after
the drain returns, the same steady-state tenure cycles read:

| phase | ms | what it is |
|---|---:|---|
| `objstart_walk` | 8 | the parallel from-space object-start walk |
| `scan_dirty_cards` | 0 | this probe has no old→young edges |
| **`evac_drain`** | **71–73** | the parallel copy of ~1.27M survivors |
| **`map_merge`** | **19–22** | the sharded forwarding-map build |
| `cardclear+young_reset` | 0 | the wipe is off-pause |
| **total** | **102–105** | |

Against the serial evacuator on the *same* cycles, at the same
`objects_copied` (1.20–1.31M):

| arm | copy phase | total pause |
|---|---:|---:|
| default, 8 workers | `evac_drain` **71–73 ms** | **102–105 ms** |
| `CRATONVM_GC_PAR_EVAC=0` | `cheney_drain` **259–355 ms** | 266–365 ms |

The ranges are disjoint, which is what makes it a result on a shared host:
**3.6–4.9× on the copy phase**, and the copy is now 70 % of a pause whose
other phases the previous page had already emptied.

Per-run, medians of three interleaved rounds:

| arm | wall (median) | Σ pauses ≥ 100 ms | minor GCs |
|---|---:|---:|---:|
| **default** | **7135 ms** | **310 ms** | **29** |
| `CRATONVM_GC_PAR_EVAC=0` | 9145 ms | 960 ms | 32–35 |
| `CRATONVM_GC_SYNC_YOUNG_WIPE=1` | 10180 ms | 461 ms | 29 |
| `CRATONVM_GC_PAR_THREADS=1` | 14096 ms | 2318 ms | 39–56 |

Every paired round favours the default over `PAR_EVAC=0`, on both wall and
pause. **The wipe arm is the one to read carefully**: it lost all three rounds
here (7525/10180/10535 against 7135/6726/9568) but was a dead heat in both
directions on the earlier r5 binary (§2). Two runs disagreeing in sign is a
noisy host, not a throughput effect, so the wipe's claim stays what §2 says it
is — a pause change worth 18–33 ms, with no established wall-clock effect.

**Engagement**, from `--verbose:gc` on the default arm, so a green result
cannot be a silently-declined one:

```
par_evac: cycles=29 helper_scans=7322924 cas_losses=0
          declined_for_slack=0 filler_bytes=7744056 promotions=1060143
```

`cycles=29` is every collection in the run; `helper_scans=7.3M` says the
helpers really did the work rather than the driver doing it while they waited
(the failure mode this branch's own withdrawn engine hit and fixed);
`declined_for_slack=0` says `plan` never fell back; `promotions=1.06M` says the
old-gen buffer arm of §4 ran. `cas_losses=0` is expected on a tree, where each
node is referenced once — the race path is covered by a test, not by this
probe.

## Correctness

* **1,782 gc unit tests and every gc integration target green — the first
  green gc gate in this lane**, where dev's tip crashes. New tests: the
  fresh-heap first-parallel-cycle commit, the plan's region bound, the
  bufferless plan, the async and sync wipes, the promotion-tail release, and
  the pause-goal feedback sequence.
* 2,656 vm unit tests; the types suite including the sharded map's
  equivalence to the sequential fold at 1/2/3/8/16/64 threads.
* `bench/OldToYoungEdgeProbe 20000 200` at `-Xmx128m` under
  `CRATONVM_GC_VERIFY_RSET=1`: 57 collections, every one with edges reports
  `edges=20000 missing=0`, 16 of them through the moving engine
  (`edges_verified=20000`).
* **The regression suite on the merged release binary: 85 of 85 passed, 0
  failed.** Both of the failures seen on the earlier DEBUG binary are absent
  there — `RMapGcStress` (a 900 s timeout that is debug-binary slowness, not a
  hang) and `REncodingFidelity` (the Windows console-encoding known issue).

## Not established, and what is owed

* **This branch's own evacuator was withdrawn**, not merged and not measured
  against dev's. The r1 engine figures are historical.
* **F4 is not separately measured.** See §4.
* **The trigger-percent sweep is owed** on a quiet host. §1.
* **A vm config test (`java_resolves_to_the_sibling_alias_before_path`) fails
  under the parallel workspace run and passes alone.** A test-isolation
  interaction in `config.rs`, untouched here and unrelated to GC.
* **The wipe lost all three rounds of the r6 A/B and tied on r5.** Read that
  as host noise, not as a cost — §7.
* Absolute pause numbers are this host's under load — see Method.
* `OldToYoungEdgeProbe` shows the item-7 shape the review named and this page
  does not touch: with dirty cards near the end of a large old gen,
  `scan_dirty_cards` is O(old objects) (137–161 ms spikes in the r1 A/B's
  `PAR_EVAC=0` arm). A block-offset table is its own change.
