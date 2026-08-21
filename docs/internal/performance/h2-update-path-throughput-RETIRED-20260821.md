# The H2 UPDATE path constant factor — RETIRED 2026-08-21

| | |
|---|---|
| **Status** | RETIRED — every named target on the page has a verdict against a 2026-08-21 measurement, and the two that are still live have owner pages |
| **Opened** | 2026-08-02, successor to `bug-h2-testmultithread-concurrent-update-timeout-RESOLVED-20260802.md` |
| **Retired by** | `perf/h2-update-path-residuals-20260821` |
| **Headline change** | the interpreter-to-interpreter gap this page existed to characterise is **6.3-6.6x, not 9.9-10.7x**; one interpreted call level is **291 ns, not ~700 ns**; and the 8-thread scaling cliff is **gone** |
| **What it never was** | a bug list. That was the page's thesis from the first day and it survives retirement intact — see §8 |

This page spent nineteen days saying the same true thing: the H2 UPDATE gap is a
flat constant factor spread across the whole interpreter/dispatch path, no
symbol on its profile is a wall, and removing every symbol it names is worth
less than 2x against a gap of 30-87x. It is retired not because the gap closed
but because **every question it left open now has an answer**, and the two
residuals that remain are owned by pages written to hold them.

The gap did also narrow, by a lot, and by work that had nothing to do with this
page — which is itself the page's argument, demonstrated: a flat cost comes down
when the whole path gets faster, not when someone fixes the top symbol.

---

## 1. Verdicts on every named target

Everything this page ever pointed at, and where it stands on `dev` at
`ba36a7afa` (2026-08-21). "Profile" means the flat `perf` self-attribution in
§3.

| target, as the page named it | verdict |
|---|---|
| `Arena::free_blocks_sorted` at **24.34 %**, "the single largest symbol" | **GONE.** Absent from both the 1- and 4-thread profiles. The page marked it obsolete on 08-11 when ZGC became the default young collector; that is now confirmed rather than predicted — this workload does not take the arena free-list path at all. |
| `is_object_address` at **5.80 %**, "the clearest single target on the list" | **MOVED, and owned elsewhere.** It is now ZGC's: `ZObjectStarts::contains` 2.97 % + `is_object_address` 1.58 %. It is *not* mainly the conservative root scan — see §5. Owner: `feature-designs/zgc-jit-load-barrier.md`, which already states the verdict and the condition for moving it. |
| `load_class_concurrent` at **1.4 %**, "the work item is the lock" | **CLOSED.** 0.26 % at 1 thread, 0.28 % at 4 — no thread term at all, and below the 0.5 % report floor in both arms. The whole ClassManager lock family is 1.1 % at 1 thread and 1.65 % at 4, of which the contention term (`lock_shared_slow` + `lock_slow`, symbols that exist only under contention) is **0.52 %**. The page's 25-thread figure of 5.2 % was a 16-core measurement of a different tree. |
| `JitCache::invalidate_for_class` at 1.60 %, "something is invalidating compiled code in steady state" | **CLOSED 08-11 and confirmed.** Nothing is; 20 000 updates cause three class definitions. Absent from the fresh profile. |
| `field_layout::object_body_size` at 1.26 %, "new code on a hot path" | **CLOSED.** Below the 0.4 % floor. |
| `value::record_object_ref_payload_slow` at 1.26 %, "a fast path that stopped being taken" | **STILL THERE**, 1.14 % at 1 thread / 1.30 % at 4. The 8-way memo landed 2026-08-13 and did not remove it. This is one of the two live residuals — §7. |
| JIT bookkeeping cluster, **~5.4 %** | **PARTLY FIXED.** `invalidate_for_class` gone; `validate_code_ptr`'s global `Mutex` removed on the retiring branch (§6); `try_jit_site_cached_native_dispatch` 1.77 %, `JitCache::get` 1.13 %, `compute_jit_key_hash` 0.56 % remain and are ordinary constant-factor work. |
| the **~3.5x cliff at 8 threads**, "wall throughput does not merely stop scaling, it INVERTS" | **NOT REPRODUCIBLE.** §4. |
| the thread-scaling slope, 3.33x → 2.45x from 4 to 25 threads | **SUPERSEDED SHAPE.** That was a 16-core host; `nproc` is 8. A 25-thread arm here measures oversubscription, not the VM, and the page says so itself. Re-taken at 1/2/4/8 in §4. |
| a call level costs **~700 ns**, 40-48x HotSpot `-Xint` | **IMPROVED to 291 ns, 27.4x.** §2. |
| the flat **9.9-10.7x** per-statement band | **IMPROVED to 6.3-6.6x, and still flat.** §2. |
| the INSERT loop at **10.5x** interpreter-to-interpreter | **IMPROVED to 8.2x.** §2. |
| `TestLob` HANG, and "the five-class gap" | **HANDED OFF.** `TestLob` has its own page and its own HotSpot control showing the race fires on HotSpot too; it is row 3 of the ten classes still at the cap in `known-issues/h2/hangs-true-vs-perfcliff-20260821.md`. |
| `TestMultiThread` flapping on `LOCK_TIMEOUT` | **HANDED OFF.** It **PASSes in 1390 s** at a 5x cap (same page). A timeout is a statement about wall-clock, not about being stuck. |
| `TestTransaction` at a 50 ms budget, 10 of 10 FAIL | **ALREADY SETTLED.** Not a correctness defect; `known-issues/h2/nonpassed-40-census-20260818.md` §"not a correctness bug". |
| `MERGE ... USING` is not a defect and not a slow path | **UNCHANGED AND RE-CONFIRMED.** Its ratio still sits inside the band (§2), which is the test the page defined for it. |
| no `org/h2/` JIT package ban to lift | **UNCHANGED.** |

---

## 2. The bands, re-taken

### One interpreted call level: 291 ns, not ~700 ns

`probes/CallShapeBench.java`, interleaved arms, **min of 5** (on a shared host
the minimum is the least-contaminated sample; load 4.4-6.6 recorded per rep):

| | 3-call chain, net of the loop | call-free loop |
|---|---:|---:|
| HotSpot `-Xint` | 31.85 ns | 5.46 ns/iter |
| cratonvm `--nojit` | 871.76 ns | 80.98 ns/iter |
| ratio | **27.4x** | **14.8x** |

**A CALL LEVEL COSTS ~291 ns interpreted, against ~10.6 ns on HotSpot `-Xint`.**
The page's 2026-08-10 figures were ~700 ns and 40-48x. The call-free loop ratio
is unchanged (14.8x against 13-21x), so the improvement is specifically in the
CALL path — which is where the interpreter and invoke work of the last two weeks
went, and none of it was aimed here.

**Read the internal ratio, not the absolute.** Per rep the chain/loop ratio was
10.8, 10.2, 9.9 for cratonvm and 5.6, 4.6, 5.0 for HotSpot while the absolutes
moved 40 %: the host speed cancels, the shape does not. A call level costs
cratonvm ~3.3 loop-iterations against HotSpot's ~1.7.

### The per-statement band: 6.3-6.6x, and still flat

`MergeLockBudgetProbe bench 500 3`, interleaved, 3 reps x 3 internal reps per
arm, load 5.8-6.5:

| 500 ops on a 500-row table | HotSpot `-Xint` | cratonvm `--nojit` | ratio (min) | ratio (median) |
|---|---:|---:|---:|---:|
| `MERGE ... USING` | 68.28 ms | 444.96 ms | **6.52x** | 7.83x |
| `UPDATE ... WHERE id=?` | 52.09 ms | 330.21 ms | **6.34x** | 6.10x |
| `SELECT ... WHERE id=?` | 33.76 ms | 218.66 ms | **6.48x** | 6.46x |
| `INSERT VALUES (?, ?)` | 25.30 ms | 167.89 ms | **6.64x** | 6.48x |

Minimums; medians of all nine samples per cell in the last column, and the two
agree. **The band held together while it moved**: four statement kinds within
5 % of each other on the min column. That is the page's most reusable finding
and it is still true — a per-statement ratio that stands out from this band is a
real lead, one that sits inside it is this page's constant factor.

### The INSERT loop: 8.2x

`H2InsertLoopProbe`, 10 000 autocommit inserts, min of 3, load 5.1-9.7:

| arm | loop | µs/row | vs `-Xint` |
|---|---:|---:|---:|
| HotSpot C2 | 237.9 ms | 23.8 | 0.15x |
| HotSpot `-Xint` | 1 548.4 ms | 154.8 | 1x |
| cratonvm, JIT | 7 415.3 ms | 741.5 | **4.8x** |
| cratonvm, `--nojit` | 12 656.4 ms | 1 265.6 | **8.2x** |

cratonvm's JIT is worth **1.71x** on this shape (was 1.5x).

**The page's warning about which ratio to quote is now demonstrated rather than
argued.** Across three reps at load 5.1 / 7.8 / 9.7 the C2 arm read 237.9 /
861.7 / 383.2 ms — a **3.6x swing** — while `-Xint` moved 9 % and cratonvm
`--nojit` 18 %. Every C2-relative number this page ever quoted (556x, 141x, 78x)
was measuring the scheduler. Compare interpreters.

---

## 3. The profile, at 1 and 4 threads, in one window

`H2UpdateScaleProbe`, `--Xmx 1g`, `sudo -n perf record -F 199`, flat
self-attribution, ZGC default. Both arms do **20 000 updates** and were taken
back to back at load 3.2-3.6, so the difference between the columns *is* the
contention term — this page's own discipline note 8.

1 thread: 30.22 s wall / 35.20 CPU-s. 4 threads: 17.30 s wall / 42.95 CPU-s.

| 1 thread | 4 threads | symbol |
| ---: | ---: | --- |
| **4.97 %** | 4.31 % | `interpreter::execute_frame_from_index` |
| 3.19 % | 2.87 % | `dispatch_virtual::execute_invokevirtual_cached` |
| 2.97 % | **3.27 %** | `zgc::ZObjectStarts::contains` |
| 2.51 % | 2.17 % | `__memcmp_evex_movbe` |
| 2.43 % | 2.63 % | `vm_exec::invoke_on_class_shared_inner` |
| 1.77 % | 1.55 % | `jit::helpers::try_jit_site_cached_native_dispatch` |
| 1.71 % | 1.74 % | `jit::helpers::jit_invoke_virtual_mic` |
| 1.71 % | 1.65 % | `vm_exec::safe_native_call_impl` |
| 1.58 % | **2.03 %** | `zgc::ZgcRealHeap::is_object_address` |
| 1.32 % | 1.24 % | `NativeMethodRegistry::find_with_kind` |
| 1.30 % | 1.24 % | `jit::validate_code_ptr` |
| 1.30 % | 1.23 % | `_mi_page_malloc_zero` |
| 1.29 % | 1.48 % | `VmHeap::load_and_forward_inner` |
| 1.17 % | 1.17 % | `InvokeCache<RetainedCode>::get` |
| 1.14 % | 1.30 % | `value::record_object_ref_payload_slow` |
| 0.78 % | 0.72 % | `NativeMethodRegistry::slot_index_for_key` |
| — | 0.44 % | `RawRwLock::lock_shared_slow` |
| 0.26 % | 0.28 % | `SharedVm::load_class_concurrent_for` |

### Re-verified after merging 51 dev commits

The table above is `ba36a7afa`. Between taking it and retiring the page, `dev`
moved 51 commits — including `4e8e8afe5`, a real ZGC fix (it refused to compact
whenever the JIT was warm and threw `OutOfMemoryError` on a 97 %-free heap), and
its partial revert `4b84a4117`. A collector fix of that size is exactly the kind
of thing that invalidates a profile, so the 1-thread arm was re-taken on the
merged tree (load 7.3-9.4, hence the higher absolutes):

| symbol | `ba36a7afa` | merged | Δ |
|---|---:|---:|---:|
| `execute_frame_from_index` | 4.97 % | 4.42 % | −0.55 |
| `execute_invokevirtual_cached` | 3.19 % | 3.32 % | +0.13 |
| `zgc::ZObjectStarts::contains` | 2.97 % | 3.03 % | +0.06 |
| `__memcmp_evex_movbe` | 2.51 % | 2.74 % | +0.23 |
| `invoke_on_class_shared_inner` | 2.43 % | 2.46 % | +0.03 |
| `zgc::ZgcRealHeap::is_object_address` | 1.58 % | 1.69 % | +0.11 |
| `jit::validate_code_ptr` | 1.30 % | 1.33 % | +0.03 |
| `record_object_ref_payload_slow` | 1.14 % | 1.18 % | +0.04 |

**Nothing moves by more than half a point.** The shape is not sensitive to the
collector fix, and the verdicts in §1 stand on the merged tree, not only on the
ref they were taken at.

**The top symbol is 4.97 % and there is almost no contention term.** Going from
1 to 4 threads moves nothing by more than half a point, and the only symbols
that exist *because* of contention — `lock_shared_slow` and `lock_slow` — total
0.52 %. That is the strongest form of this page's thesis it has ever had:
the cost is not a lock, it is not a symbol, it is the path.

---

## 4. The 8-thread cliff is not reproducible

The page recorded a **~3.5x jump** in CPU per update between 4 and 8 threads,
reproduced at two host loads, with wall throughput *inverting* (1176 updates/s
at 4 threads, 261/s at 8) and voluntary context switches up ~40x — read as
"spin-then-park on a contended lock", with naming the lock as the next question.

Re-taken with the same method — **16 000 updates at every thread count** so the
shapes are comparable, the 0-update baseline of each shape interleaved as an
ordinary arm, and the sweep run twice in **opposite order** so load drift cannot
fake a slope:

| threads | pass A (load 1.9→3.0) | pass B (load 5.3→4.7) | wall, A | updates/s, A |
|---:|---:|---:|---:|---:|
| 1 | 1.282 CPU-ms/update | 1.589 | 16 702 ms | 958 |
| 2 | 1.294 | 1.497 | 8 724 ms | 1 834 |
| 4 | 1.851 | 1.896 | 6 695 ms | **2 390** |
| 8 | 2.039 | 2.146 | 6 903 ms | **2 318** |

**There is no cliff.** CPU per update rises 1.6x from 1 to 8 threads, smoothly,
in both directions of the sweep. Throughput does not invert: 4 and 8 threads are
within 3 % of each other, on an 8-core host where 8 mutator threads plus the GC
and compiler threads are already oversubscribed. Pass B is uniformly ~10-15 %
worse than pass A because the host load rose between them — which is exactly
what the two-direction sweep exists to expose, and the *shape* is identical in
both.

The lock the page went looking for was the free-list sort in the stop-the-world
sweep, it was found and fixed on 2026-08-08, and the collector that path
belonged to is no longer the default. Nothing has replaced it.

---

## 5. What the fresh censuses say that no profile could

Four instruments already in the tree, run on this workload for the first time.
Each answers a question this page asked and could not reach.

**`is_object_address` is a DISPATCH cost, not a GC cost.** The page called the
conservative root scan "per-thread-stack work per collection, so it grows with
(threads × collections)". `CRATONVM_DBG_JIT_METHOD_STATS` counts the membership
walks by site, and over one 5 000-update run (8.1 s):

| site | walks |
|---|---:|
| `native-dispatch-cached` | **7 092 992** |
| `checkcast` | 4 701 375 |
| `getfield` | 2 216 749 |
| `native-dispatch-decode-args` | 1 328 097 |
| `instanceof` | 386 459 |
| `invoke-dispatch` | 38 694 |
| **total** | **~15.8 M** |

Nearly half are one line: `try_jit_site_cached_native_dispatch` validating the
receiver before it consults its own site cache. This is per-CALL work, not
per-collection, and the "grows with threads × collections" model was wrong.
`getfield helper calls: 2 216 749, of which trusted-ref: 0` — the proven-oop
fast path does not engage once on this workload, which corroborates
`zgc-jit-load-barrier.md`'s table (ZGC: 100 % `outside-published-bounds`) on a
second workload.

**~610 native-registry probes per UPDATE, 2.17 per invoke.** Taken as a
**marginal** rate, because `lookup_census`'s own doc warns its printed
`lookups_per_invoke` cannot be read as probes-per-call (the denominator does not
move with the workload — it read 501, 598 and 842 on the three runs below purely
because the numerator grew):

| updates | lookups | general invokes |
|---:|---:|---:|
| 0 (setup only) | 4 678 941 | 2 214 835 |
| 2 000 | 5 881 496 | 2 763 037 |
| 8 000 | 9 538 357 | 4 444 983 |

Marginal, over the 6 000-update difference: **609.5 lookups and 280.3 invokes
per update** — `find` 213.6, `find_with_kind` 287.5, `resolve_id` 108.4, and
**31.0 descriptor-quirk rewrites**, a path marked `#[cold]` running 31 times per
UPDATE.

This is the number `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md`
§2 asks for before anyone restructures the dispatch entry points: *"a profile
share can say `slot_for_exact` is 8.5 %, but only this says whether a 'one
lookup per invoke' rewrite would divide it by 1 or by 10."* **It would divide it
by 2.17.** The whole registry cluster is ~5 % here, so the ceiling on that
rewrite is ~2.5 % of this workload. Worth knowing before, not after.

**Half the cost of an interpreted call is the RETURN.**
`CRATONVM_DBG=invoke-phases`, 62 282 instrumented `invokestatic` calls:

| phase | share |
|---|---:|
| `ret_total` | **50.5 %** |
| — of which `ret_recycle` | 22.7 % |
| `ic_lookup` | 22.4 % |
| `args` | 10.5 % |
| `guards` | 10.1 % |
| `frame_build` | 4.7 % |
| `frame_push` | 1.8 % |
| `CALIB(noop)` | 8.6 % |

`frames: owned=812 082 cached=1 579 626` — 34 % of frames are not served from
the pool. Ranking, not costing (the rdtsc overhead is included, which is what
`CALIB` is for), but the ranking is the point: the page spent nineteen days
looking at *entry* — dispatch, resolution, the registry — and the return path is
half the call. That is where the 291 ns lives, and it is the sharpest thing this
page has ever been able to say about its own headline quantity.

**Nothing resolves classes in steady state, still.** `define-census`: 973
definitions over 973 distinct names for the whole run. Consistent with the 08-11
reading of 933/936.

**The hot-path counters, per update:** `retarget_field` 729, `resolve_method_ref`
355, `lookup_loader_initiated` 159, `force_native` **2.0**. The last one matters
because `force_native_over_real_jdk_bytecode_memoized` allocates three
`Box<str>` and takes a global `Mutex` on *every* call, hit or miss — a genuine
defect, and at 2 calls per update it is categorically not this workload's. It is
recorded here so the next person to find it does not re-derive it; its cost is
on reflective/Mockito-shaped workloads, where the same function is already
documented as a hotspot.

---

## 6. What the retiring branch changed

**`jit::validate_code_ptr` no longer takes a process-global `Mutex`.**

It runs on every compiled call — `CompiledMethod::try_call` and
`try_call_with_context` both validate the entry they are about to jump to — and
it took a global `std::sync::Mutex` to binary-search the code-region list.
**11 332 560 lock acquisitions in one 20 000-update run, 566 per UPDATE.**

The region list barely changes: one bump per `ExecutableBuffer` create or drop,
1 887 against those 11.3 M calls. A structure read 6 000x more often than it is
written is a read-copy-update shape, so mutators now publish an immutable
snapshot into an `ArcSwap` and readers binary-search that with no lock.

**Priced honestly: the lock goes, the run does not move.** The symbol's own
share falls 1.69 % → 1.49 % and the acquisitions go to zero. Four
ABBA-interleaved campaigns on the probe's own update-phase wall:

| arm | result | direction |
|---|---|---|
| 1 thread, profiled, quiet host (load 2.3), n=3 | +1.2 % | 1/3 |
| 1 thread, load 4-12, n=4 | −13 % median | 3/4 |
| 4 threads, n=3 | −3.3 % median | **3/3** |
| 8 threads, n=3 | −0.3 % wall, −2.0 % CPU | 2/3 |

**Not separable from zero.** An uncontended `std::sync::Mutex` costs about what
an `ArcSwap` load costs, which is why a quiet single-threaded host cannot see
this at all. The change is kept on *shape* — a global lock on a
per-compiled-call path is the "genuinely scaling rather than constant-factor"
category this page named as its next target — and because it costs nothing
measurable anywhere. Quote the symbol, not the run.

Two earlier cuts are recorded in the source because both were measured and both
were too narrow, and the engagement counters are the only reason that was
visible rather than being absorbed into a timing wash:

| attempt | hits | misses | hit rate |
|---|---:|---:|---:|
| 1-way per-thread memo | 263 299 | 3 225 254 | 7.5 % |
| 16-way, direct-mapped on the page | 1 163 588 | 2 477 691 | 32 % |
| `ArcSwap` snapshot | 3 448 638 | 0 | **100 %** |

Every compiled method gets its own `ExecutableBuffer`, hence its own region, so
a thread calls into a rotating set of them and any small cache is evicted by the
next call.

**And one measurement bug worth more than the fix.** The first A/B set
`CRATONVM_DBG_JIT_METHOD_STATS` in *both* arms to read the engagement counter,
and measured the memo **12 % slower**. The counter is only reached in the memo
arm, so switching the census on charged one arm 11.3 M global atomic
read-modify-writes and the other none. The whole reading was of the instrument.
The counters are now gated for the same reason they must be: counting
unconditionally would have replaced a `Mutex` with a contended cache line, which
at 25 threads is not obviously the better of the two.

`CRATONVM_JIT=-code-ptr-memo` puts the lock back, so the arms are one binary.
Four tests pin the change: the snapshot and the locked lookup must agree on
every probe cold and warm, a dropped `ExecutableBuffer` must stop validating,
the epoch must move on register and deregister, and the kill switch's `off_key`
must be the key the reader reads.

---

## 7. The two live residuals, and who owns them

**1. The ZGC membership walk — `feature-designs/zgc-jit-load-barrier.md`.**
`ZObjectStarts::contains` 2.97 % + `is_object_address` 1.58 % = ~4.5 % at 1
thread, from ~15.8 M walks per 8 s. That page already owns it, already has the
verdict (*"ZGC publishes nothing into `JIT_REGION_BOUNDS` … publishing bounds to
make the check pass would be the defect, not the fix"*), and already names the
condition for the number to move: the load barrier lands, or it does not move.
The H2 numbers above are a second workload's corroboration, not a new question.
What is new and belongs there: **`native-dispatch-cached` is the largest single
site at 7.1 M walks**, ahead of `getfield`, and that page's site inventory is
written around `getfield`.

**2. `record_object_ref_payload_slow` at 1.14-1.30 %.** The only symbol on this
page's original list that is still on it, still at its original share, after the
8-way memo landed on 2026-08-13 for a different workload. A `_slow` suffix at
over 1 % is a fast path that is not being taken, and nobody has asked why on
*this* shape. It has no owner page. It is ~1 % of a workload that is 6x off, so
it is filed here as a lead rather than promoted to one.

Neither is a defect. Both are constant-factor work, which is the category this
whole page is about.

---

## 8. The thesis, and why it survives

> That is ~39 % of the profile. **Removing all of it is under 2x, against
> 30-87x.** Treating the list as a bug list is the mistake this page exists to
> stop repeating.

Nineteen days later the arithmetic is unchanged and the demonstration is
stronger. Every item that was fixed — the free-list radix sort (−40 % CPU at 25
threads), the `invalidate_for_class` early-out, the double `read()` guard, the
`validate_code_ptr` lock — moved its symbol and left the run where it was. The
gap closed anyway, from ~10x to ~6.5x interpreter-to-interpreter and from ~700 ns
to 291 ns per call level, through invoke and interpreter work aimed at no
symbol on this list.

**A flat cost is paid down by making the path faster, not by removing its top
symbol.** That is the finding. It is why three sessions aimed at "≈100x, that is
the bug" produced nothing, and why the sessions that never read this page
produced the improvement.

The one place the page pointed that is still worth pointing: **half of an
interpreted call is the return** (§5). Nothing on any profile said so, because
`ret_recycle` does not appear as a symbol.

---

## 9. Measurement discipline this host requires

The reusable half of the page. Rules 1-8 are inherited; 9 and 10 are what the
2026-08-21 re-measurement had to add.

1. **Never quote a debug-build ratio.** ~5-10x slower than release on its own.
2. **Never quote a multi-threaded wall-clock number.** Use CPU time, round-robin
   the arms, median or min of N, and record `uptime` beside every number.
3. **Aggregate `perf report` by symbol** (`--sort symbol`) — the default groups
   by command, which on a 25-thread run divides every symbol by 25.
4. **An empty stdout is not a pass.** H2's `TestBase` reports some failures on
   stderr; check the exit code.
5. **Size the shape so the work term dominates the baseline.** VM start plus the
   `MERGE` seed is ~10 CPU-s on a quiet host and ~40 on a busy one.
6. **Interleave the 0-update baseline as an ordinary arm.** Taken once up front,
   it carries that minute's load into every number derived from it.
7. **Trust `perf`'s flat self-attribution here; do not trust its call graphs.**
   `--call-graph=dwarf` through this binary's inlining produces chains that are
   wrong, not merely shallow. A caller question needs an in-VM counter.
8. **Measure single-threaded too.** One thread costs nothing extra and splits
   every symbol into a work term and a contention term.
9. **Your instrument must be in BOTH arms, or in neither.** §6: a debug flag
   that only the ON arm reaches turned a neutral change into a 12 % regression
   and the number was entirely the counter. Export it identically, or gate it so
   it cannot be reached differently.
10. **Alternate the arms within a rep (ABBA), not between reps (ABAB).** The
    first 1-thread A/B here ran ABAB while the host was quietening; the trend
    handed the second arm of every pair a systematic advantage, and it read as a
    real 1.2 % regression until the order was fixed.

And the one that supersedes an inherited rule: **`sudo -n perf record` works on
this host** (`perf_event_paranoid` is 4 and `sudo` is passwordless) — do not
change the sysctl, it is shared. But `sudo` **resets the environment**, so an
exported variable does not reach the profiled process: pass arm env explicitly
as `env VAR=val "$BIN"`, or both arms are silently the same arm.

---

## 10. Reproducing

```bash
javac -cp <h2>/target/classes -d probe probes/H2UpdateScaleProbe.java
<cratonvm> --java-home <jdk25> --Xmx 1g -c "<h2>/target/classes:probe" \
  -Dprobe.dir=./h2updb H2UpdateScaleProbe <threads> <updates> 10000
```

Run `<threads> 0 10000` for the baseline of the same shape. The bands:

```bash
javac -cp <h2>/target/classes -d probe \
  apps/h2database-suite-runner/probes/{CallShapeBench,MergeLockBudgetProbe,H2InsertLoopProbe}.java
java -Xint -cp probe CallShapeBench                       # the control
<cratonvm> --java-home <jdk25> --nojit --Xmx 2g -c probe CallShapeBench
<cratonvm> ... --nojit -c "probe:<h2>/target/classes" MergeLockBudgetProbe bench 500 3
```

The censuses in §5, all already in the tree:

```bash
CRATONVM_DBG_JIT_METHOD_STATS=1 <cratonvm> ...   # membership walks by site, code-ptr memo
CRATONVM_DBG=native-lookups     <cratonvm> ...   # registry probes, and the missed triples
CRATONVM_DBG=invoke-phases      <cratonvm> ...   # where a call's cycles go
CRATONVM_DBG=define-census      <cratonvm> ...   # what this run actually defined
CRATONVM_DBG=hotpath-counts     <cratonvm> ...   # force_native / resolve_method_ref / retarget_field
```

## Related

* `feature-designs/zgc-jit-load-barrier.md` — owns residual 1.
* `known-issues/h2/hangs-true-vs-perfcliff-20260821.md` — owns every H2 class
  this page ever quoted as a HANG, with direct evidence that six of them
  recover at a 5x cap.
* `known-issues/h2/nonpassed-40-census-20260818.md` — the 40-class,
  three-collector census, and the `TestTransaction` adjudication.
* `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md` — §5 answers its
  §2 question.
* the retired `bug-h2-testmultithread-concurrent-update-timeout`,
  `…-concurrent-insert-throughput`, `…-testtransaction-merge-using-lock-timeout`
  and `…-mvstore-insert-loop-perf-hang` write-ups — this page's predecessors.
* `bug-h2-classid0-stale-address-family-FIXED.md` — the memory-safety family
  found in this class. Unrelated to throughput.
