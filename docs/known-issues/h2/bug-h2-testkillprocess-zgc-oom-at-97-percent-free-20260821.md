# `TestKillProcessWhileWriting` — `OutOfMemoryError` with 97 % of the heap free, because ZGC never compacts once the JIT engages

## Status

**FIXED for this class 2026-08-24. Two classes remain, on a DIFFERENT
obligation.** `TestKillProcessWhileWriting` passes in the default configuration
— `rc=0`, zero `OutOfMemoryError`, zero `arena allocation failed` — on three
runs interleaved with three failures of the kill-switch arm of the **same
binary**. The `osr-shadow-coverage-unproven` disjunct that refused relocation
on **725 of 759** collections is now **0 of 54**, and the whole workload needs
54 collections instead of 759.

**The blocker was not the one §"Still open" (2026-08-22) predicted.** That
section named `CROSS_THREAD_JIT_PEER` as what was left. Measured on this class
at the 2026-08-23 `dev` tip: `cross-thread-jit-peer` fires **0 times in 759
collections**. What actually blocked it was two defects one level below, both
found by counting what the aggregate `map_coverage=N` counter would not say —
see §"Follow-up 2026-08-24".

Still open, and now on a different obligation entirely: `TestMVStoreTool` and
`org.h2.test.jdbc.TestCachedQueryResults` reach the same fragmentation wall
because their collections refuse on `ACTIVE_FRAME_MAP` — the innermost compiled
frame cannot be *located*, not disbelieved. See §"Still open".

> ### Read this before quoting the "FIXED" above
>
> **The class passes on the tree this work was developed against
> (`3ed73bf89` + these fixes) and FAILS again after merging the `dev` of
> 2026-08-24 (`d2db39944`), for a reason that is not this one.** Two reps per
> arm, same class, same host, same day, the two binaries interleaved:
>
> | arm | rc | oom | collections | compactions | `none` (proven) | `compiled-frame-oop-not-published` |
> |---|---|---:|---:|---:|---:|---:|
> | pre-merge ×2 | **0 PASS** | 0 | 76 / 79 | 24 / 24 | 24 | 6 |
> | post-merge ×2 | 1 FAIL | 4 | 234 / 1039 | 9 / 8 | 9 | 82 |
>
> Causal, not a consequence of the death spiral: over the **same first 76
> cycles** of each run, `none` goes 24 → 9 and
> `compiled-frame-oop-not-published` goes 6 → 28.
>
> The two fixes below are still doing their job on the merged tree — with
> `CRATONVM_OSR_COVERAGE_SHADOW=0` the OSR disjunct fires 750 times, with it on
> it fires **0** — so this is a NEW blocker stacked on top, not a regression of
> them. It is `UNPUBLISHED_FRAME_OOP`: the band verifier finding a
> movable-resident word in a compiled frame's spill band that the shadow stack
> never published. Ruled out as its cause, on one binary:
> `CRATONVM_REGISTER_IMAGE_REMAP=0` (dev's own kill switch for the register-image
> repair that landed in the same window) leaves it at `unpub=264/855` and
> `unpub=149/515`, still failing.
>
> **Bisected to the `invokedynamic` bridge**, on one binary, using dev's own
> kill switches — no rebuild, five arms, `UNPUBLISHED_FRAME_OOP` counted over
> the first 76 collections of each:
>
> | arm | `compiled-frame-oop-not-published` | `none` (proven) |
> |---|---:|---:|
> | none off (the merged default) | 28 | 9 |
> | `CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0` | 25 | 11 |
> | `CRATONVM_MAP_VIEW_CACHE=0` | 23 | 10 |
> | `CRATONVM_VECTOR_TEMPLATES=0` | 21 | 12 |
> | **`CRATONVM_JIT_INDY_BRIDGE=0`** | **5** | 13 |
> | all four + `CRATONVM_REGISTER_IMAGE_REMAP=0` | 3 | 12 |
>
> One switch accounts for essentially all of it, and 5 is the pre-merge level
> (6). `2cc02f7d3 feat(jit): bridge every frame-stack-only invokedynamic
> bootstrap` compiles a call shape whose live oops the shadow stack does not
> publish — which is the same family as §"Follow-up 2026-08-24" §2, and
> `a51077342 wip: instrument the indy bridge and close its staged-arg oop-map
> gap` says its author was already looking at that gap. Not patched here: it is
> another session's live feature, and the fix belongs with it.
>
> **Later that day, one more real defect on this path, which changed nothing
> here.** The optimizing tier's inline caches never restored the innermost-frame
> mirror — a genuine gap every sibling path closes, now fixed and pinned by a
> discriminating test — and on this class it moves the `no_map` rate not at all
> (5.06/5.33/5.09 % on against 5.57/5.18 % off, engagement counter 3 695–3 730
> against 0). See §"Follow-up 2026-08-24 (second)". The same section records
> that this class has become FLAKY on `dev` — `rc=0` once and `rc=1` five times
> from one binary on one config — which is the first thing any future arm here
> has to be scored against.
>
> > **A second, smaller contributor is unaccounted for.** With the indy bridge
> off, `active-safepoint-map-incomplete` is still 56 per 76 cycles against 46
> before the merge, and proven cycles are 13 against 24 — so the class still
> fails. None of the five switches covers it. That is the same
> `ACTIVE_FRAME_MAP` obligation §7 characterises for `TestMVStoreTool`, and the
> two are probably one question.
## Symptom

`org.h2.test.store.TestKillProcessWhileWriting`, default configuration
(ZGC + JIT), `--Xmx 1g`, real JDK 25:

```
Exception in thread "main" org/h2/mvstore/MVStoreException:
  java.lang.OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576) [2.4.249/3]
  ...
  at org/h2/mvstore/FileStore.getWriteBuffer(FileStore.java)
  at org/h2/mvstore/WriteBuffer.<init>(WriteBuffer.java)
  at org/h2/mvstore/MVStore.panic(MVStore.java:515)
```

MVStore turns the allocation failure into `MVStore.panic`, which closes the
store mid-write, so the visible result is a data-integrity failure rather than
a clean OOM.

## The heap at the moment of failure

The collector's own guard says this is not an exhausted heap. Ten rungs of the
try / GC / try / reclaim ladder, `failure_seq` 1 through 512, all identical:

```
zgc: arena allocation failed
  request=1048592  used=1073476720  capacity=1073741824
  free_list_bytes=1041642104   largest_free_block=1041368
  free_spans=24122  failure_seq=16
```

**97 % of the heap is free** and no single hole is big enough for a 1 MB
buffer. `largest_free_block` settles at **524 096 bytes — one `ZGC_TLAB_MAX_CHUNK`
minus a header**, the exact ceiling `ZGC_LARGE_OBJECT_MIN`'s own doc predicts
("one survivor per chunk caps every hole in the heap at one chunk").

The one-shot fragmentation report names what stands in the way, and the number
is the finding:

```
zgc frag: the CHEAPEST window that could serve this request — 104 live bytes in
  1 run(s) are all that stand between 1204192 free bytes spread over 1204296
  bytes of contiguous arena.
zgc frag: wall occupant class=java/lang/String  count=1 bytes=80
zgc frag: wall occupant class=java/lang/Object  count=1 bytes=24
```

**104 bytes block 1.2 MB.** Nothing is wrong with the allocator's search: there
is no hole, and on a heap that does not compact there never will be one again.

## Root cause

`ZgcRealHeap::relocate_stw` — the stop-the-world slide that is this collector's
**only** defragmentation — opens with:

```rust
if crate::gc_quiescence::is_active()
    || crate::gc_quiescence::unregistered_jit_frame_on_stack()
{ /* decline to relocate */ }
```

`gc_quiescence::is_active()` is true whenever **any** thread is inside compiled
code. In a steady-state workload that is true from the moment the JIT threshold
trips, so the refusal fires on essentially every cycle and **the default
configuration has no defragmentation at all**.

The refusal itself is correct — a slide under a compiled frame whose registers
and spill slots the collector cannot rewrite corrupts the heap. What is at issue
is only whether "a compiled frame exists" is the right way to ask.

## Measured

One class, one host, `--Xmx 1g`, each arm in its own working directory, on the
`dev` binary as it stands.

| arm | rc | secs | `OutOfMemoryError` | arena alloc failures |
|---|---|---:|---:|---:|
| **ZGC + JIT (the default)** | **1 FAIL** | 162 | **4** | 9 |
| ZGC + `--nojit` — relocation permitted, so the arena compacts | **0 PASS** | 813 | 0 | 0 |
| ZGC + JIT, `CRATONVM_ZGC_RELOCATE=0` | 1 FAIL | 174 | 4 | 10 |
| Generational + JIT | 0 PASS | 372 | 0 | 0 |
| G1 + JIT | 1 FAIL | 34 | 2 | 0 |
| HotSpot JDK 25 | 0 PASS | 6 | 0 | 0 |

The failure is **ZGC-specific** and **JIT-specific**: the one CratonVM arm that
compacts (`--nojit`) is the one that never sees an allocation failure.

**The times are not comparable across rows.** This host runs many agents and its
load moved between 4 and 16 during the sweep; a later repeat of the `--nojit`
arm hit a 900 s cap having finished in 813 s here. Neither `rc` nor the
allocator counts move with load, which is why the verdict rests on those
columns.

**G1 fails this class too**, in 31–43 s, with no arena allocation failure — so
not this defect — and with a face that varies between runs: a null
`FileChannel` in one, `OutOfMemoryError` in another. Separate open issue; see
*Still open*.

## The fix, withdrawn once and then earned

`gen_heap::collect_garbage_inner` had the identical `is_active()` term and
deleted it on 2026-07-26 (`arch-2026-07-26/moving-young-precise-roots`), in a
comment that describes this defect one collector over:

> Because `gc_quiescence::is_active()` is true whenever ANY thread holds a live
> JIT frame — i.e. in every steady-state workload once the 500-invocation JIT
> threshold trips — that term made the young generation stop being a copying
> collector the moment the JIT engaged.

What replaced it there is a **per-cycle coverage proof**. ZGC was written after
that deletion and inherited the deleted form, so the repair looks like a
one-line adoption: refuse only when a compiled frame is live *and* this
collection did not prove that frame rewritable.

**It was implemented, and it works.** The class passes in the default
configuration (`rc=0`, `oom=0`), the kill switch reproduces the original
failure on the same binary, and the `TestMultiThread` corruption canary was
0/6 against a base 1/6.

**It was unsound on the first attempt, and was reverted.** Two independent
reasons, both already written down in the tree — and both since fixed (§"Where
it stands now"):

1. **The proof is never run for this collector.**
   `memory::roots::collect_roots` computes it inside a `&&` chain whose second
   term is `heap.is_generational() || g1_precise_only_roots`. Rust
   short-circuits, so on a ZGC cycle
   `refresh_moving_young_coverage_for_collection()` is not called at all, and
   the published verdict stays at the value `begin_moving_young_coverage_cycle`
   reset it to — `false`, meaning *complete*. A refusal reading that verdict is
   reading a proof nobody ran. `roots.rs` says so where it excludes ZGC: it
   "likewise gets a **vacuous coverage proof**".
2. **Running it anyway would fail closed.** The band scan's residency test reads
   `JIT_REGION_BOUNDS`, and
   `conservative_roots::moving_young_unpublished_frame_oop_present` states that
   "G1 deliberately keeps it empty, ZGC never fills it", returning
   `YOUNG_BOUNDS_UNPUBLISHED` when it is not live. So even a proof that ran
   would report incomplete for ZGC, and the refusal would behave exactly as it
   does now.

`bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`
reaches the same conclusion from the other end, and records that
`precise_only_true` is **0 on ZGC** on every workload it measured.

Note what the passing test run does and does not prove. It proves the
fragmentation is what fails the class, and that compaction removes it. It does
not prove the relocation was safe — the corruption it risks is latent, "the
suppressed pause has to coincide with a staged-argument oop that actually
moves", and 6 clean runs cannot see that. Fragmentation wastes a heap; a slide
behind an unrewritable frame corrupts one, so the tie is not broken by which
one was observed.

### What would actually lift it — 1 and 2 are DONE

1. **Make `collect_roots` run the proof for ZGC as well.** Done. It computed the
   proof and the conservative-scan suppression in one short-circuiting `&&`
   chain; they are two expressions now, and only the suppression is
   collector-gated. `roots.rs`'s `the_coverage_proof_runs_for_every_collector`
   is a source witness on the order, because the defect is a call that does not
   happen and no runtime assertion can see one.
2. **Give the band verifier something to classify addresses with.** Done, and
   NOT by filling `JIT_REGION_BOUNDS`: that table's load-bearing second job is
   the store-side "may an inline reference store skip the write barrier", which
   G1 and ZGC answer by leaving it empty, so filling it to fix a verifier would
   silently re-enable those stores. A third table, `MOVABLE_BOUNDS`, names what
   a relocating collection may move; ZGC publishes its arena envelope into it at
   construction, `addr_is_movable` is the union with the young table, and
   `movable_bounds_are_live` is the fail-closed gate on the union.
3. Stage (b) of `feature-designs/zgc-jit-load-barrier.md` remains the route that
   needs no per-cycle proof at all.

## Where it stands now

Same class, default configuration, `--Xmx 1g`,
`CRATONVM_DBG_JIT_ROOTSCAN=1 CRATONVM_GC_STATS=1`:

```text
jitroots lines=263          <- the proof RAN on 263 collections
proven=true  21             <- and passed on 21
proven=false 242

[GC] zgc-features: compaction_cycles=21 objects_relocated=203007
                   relocation_skipped_jit=242 relocation_on_proven_jit=20
```

**20 of the 21 compactions happened with a compiled frame live** — exactly the
cycles the old refusal existed to reject. Before, all 263 declined and
`compaction_cycles` was 0.

`rc=1`, 4 `OutOfMemoryError`, 8 arena allocation failures: still failing.

### The blocker, named

The `[jitroots]` line now reports WHICH obligation was unmet, and the histogram
over that run is unambiguous:

| reason | cycles |
|---|---:|
| `osr-shadow-coverage-unproven` | **234** |
| `none` (proven) | 21 |
| `compiled-frame-oop-not-published` | 8 |

Not the cross-thread peer handshake one would guess for a multi-threaded
workload, and not the codegen's oop maps.
`conservative_roots::moving_young_osr_shadow_fallback_needed` fires when a live
compiled-via-OSR method has an incomplete shadow layout
(`shadow_thread_slot_off` / `shadow_savetop_slot_off` / `shadow_off_in_thread`)
**or** precise maps without `fully_oop_covered` and an exact `rbp`. H2's MVStore
loops are OSR-compiled constantly, so this is the dominant shape here.

**Which of those two disjuncts fires has not been measured.** That is the next
step and it is a JIT-side question. Nothing here says the obligation is wrong —
it is a refusal, and a refusal that fires is the safe direction.

### What the other collectors do now

* **Generational** — byte-identical. Its residency answer is the young table OR
  an empty one, i.e. itself, and the old `&&` chain already evaluated every term
  for it. `TestKillProcessWhileWriting` passes on both binaries (386 s / 360 s,
  no OOM, no allocation failure).
* **G1** — its verdict stops being a lie. Measured over one run of this class:
  base reports `incomplete=false` 722 499 times and `incomplete=true` 1 169;
  with the change, `incomplete=false` **0** and `incomplete=true` 886 790, all
  `reason=young-bounds-unpublished-verifier-vacuous` — which is correct, since
  G1 publishes neither table. Its behaviour is unchanged because
  `refuse_evacuation` is gated on `CRATONVM_G1_COVERAGE_PIN`, default off; what
  changed is that `record_g1_pause_coverage` stops measuring a vacuous verdict,
  and G1's own "pause ran against a root set it could not prove" warning now
  fires where it always applied. Both G1 arms hang at the 900 s cap on this
  class, before and after — pre-existing, see *Still open*.

## What this corrects in the record

* **`relocate_stw`'s own measurement.** The refusal carries a 2026-08-15 A/B on
  a synthetic allocator workload concluding *"Compaction is not what buys
  contiguity on this collector, so deferring it is not what loses it."* That is
  refuted on a real workload: the arm that compacts has zero allocation
  failures; the arm that does not has ten and an `OutOfMemoryError`. The
  synthetic arm never reached the state the H2 workload reaches. The paragraph
  is kept in the source with this result beside it.
* **The 40-class census** (`nonpassed-40-census-20260818.md`) files
  `TestKillProcessWhileWriting` under §2a, wall-clock, noting only that it
  "finishes as FAIL". It is not a wall-clock row: it is a spurious
  `OutOfMemoryError` on a heap that is 97 % free.

## Reproduction

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"

# fails
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting

# passes -- the only difference is that relocation is permitted
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g --nojit \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

`rc` is meaningful for this class, but the allocator's own lines are the
diagnosis: `grep -c 'arena allocation failed'` and `grep 'zgc frag:'`. The
`[GC] zgc-features:` line's `relocation_skipped_jit` is the count of cycles
that declined.

**As of 2026-08-24 the first command PASSES.** To see the old failure on the
same binary — the only A/B worth taking — use the kill switch:

```bash
# fails: restores the frame-slot reading of the OSR coverage check
CRATONVM_OSR_COVERAGE_SHADOW=0 <cratonvm-bin> --java-home /data/toolchain/jdk-25 \
    --Xmx 1g -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

The instruments the 2026-08-24 work rests on, all additive, all off by default:

| flag | what it prints |
|---|---|
| `CRATONVM_DBG_JIT_ROOTSCAN=1` | the per-cycle `[jitroots]` line: `reason=`, `osr_reason=(…)`, and now `frame_cov=(no_slot= misaligned= no_map= incomplete= ok=)` and `xt_cov=(accepted= refused= deposits=)` |
| `CRATONVM_DBG_OOPCOV=1` | per compiled method, both coverage notions side by side with the safepoints each is missing, plus the six frame-slot causes and the seven shadow causes |
| `CRATONVM_DBG_XT_COVERAGE=1` | the cross-thread handshake's per-cycle `peer_depth`/`proven` arithmetic and each peer's deposit |

`frame_cov` and `osr_reason` are CUMULATIVE process counters: take the
difference between two lines, or the tail line before exit.

## Regression cover left behind

* `gc/src/zgc.rs::a_live_compiled_frame_forbids_relocation_whatever_the_coverage_verdict_says`
  asserts three states: frame live + verdict *incomplete* → nothing moves;
  frame live + verdict *complete* → **still** nothing moves, which is the half
  that pins the withdrawal (the fixture's verdict is exactly the vacuous
  `false` a real ZGC cycle carries); no frame → moves, so the first two are
  measuring the guard rather than an inert fixture.
* `gc/src/arena.rs::warn_small_alloc_in_high_region`, a tripwire on a small
  allocation landing in the large-object region — see *Still open*.
* `gc/src/zgc.rs::a_heap_that_publishes_no_movable_bounds_cannot_prove_coverage`
  — the envelope is published, it covers this heap's own allocations, and a
  dropped heap does not wipe bounds it did not publish. That third assertion
  caught a real teardown bug on its first run: `ZgcRealHeap::drop` cleared
  `JIT_READ_BOUNDS` unconditionally, so a short-lived heap wiped a live heap's
  bounds — the same defect `GenerationalHeap::drop` was fixed for, with the
  measured consequence that an empty table makes the verifier vacuous. Both ZGC
  clears are owner-checked now.
* `vm/src/memory/roots.rs::the_coverage_proof_runs_for_every_collector`.
* Corruption A/B on `org.h2.test.db.TestMultiThread`, ABBA-interleaved, 6 reps
  per arm, clean directory per rep: **0/6 on both**. The change relocates in a
  configuration that previously never did, so this is the measurement that had
  to be taken.
* `cargo test --lib -p cratonvm-gc -p cratonvm-types -p cratonvm-vm`:
  1685 + 573 + 2578 pass, 0 fail.

Added 2026-08-24:

* `jit/src/x64/tests.rs::the_method_entry_poll_knows_its_own_live_oop_locals` —
  the entry pc answers with the SEEDED mask (not zero, and not
  `local_oop_masks[0]`, which a back edge can have narrowed), a reached bci
  answers with its own, and an unreached bci still answers `None`. Three
  directions, so the fix cannot be mistaken for "everything is covered now".
* `conservative_roots::the_osr_coverage_check_reads_the_shadow_aggregate_not_the_frame_slot_subset`
  — built on the shape the fast tier actually emits for an OSR method with a
  direct call taking a reference argument (shadow complete, frame-slot
  incomplete), and the kill switch flips the answer on the SAME fixture, so it
  pins which field is read rather than riding on a fixture that would pass
  either way. Its pair,
  `…_still_refuses_an_incomplete_shadow_claim`, is the inverse fixture.
* `conservative_roots::peer_coverage_is_accepted_only_when_every_peer_entry_is_accounted_for`
  — the handshake's arithmetic, with both shortfall shapes (one short, and
  nothing deposited) asserted, since the shortfall is the whole soundness
  argument.
* `conservative_roots::beginning_a_coverage_cycle_clears_the_peer_ledger` —
  a stale carry-over is the ledger's only unsound state, and the test deposits
  first so the clear is proving something.
* `conservative_roots::a_thread_with_no_jit_frames_deposits_nothing`.
* `cargo test --release --lib -p cratonvm-jit -p cratonvm-gc -p cratonvm-types
  -p cratonvm-vm`: 2105 + 1685 + 581 + 2610 pass, 0 fail.

## Also confirmed, 2026-08-21: four more classes hit the identical signature

Found while triaging `hangs-true-vs-perfcliff-RESOLVED-20260821.md`
(the 48-class FAIL/HANG-union rerun), on the same `dev` tip, same `--Xmx 1g`,
default config (ZGC + JIT). None of these four were suspected of this
mechanism going in — each was simply re-run to settle "true hang or perf
cliff", and each turned out to be neither: a fast, spurious `OutOfMemoryError`
from the identical fragmentation wall.

| class | request | rc | wall | signature |
|---|---|---|---:|---|
| `org.h2.test.db.TestOpenClose` | `ByteBuffer.allocate 1048576` (1 MB), `FileStore.getWriteBuffer` | 1 | 2:04 | `MVStoreException` → `MVStore.panic`, uncaught, process exits |
| `org.h2.test.store.TestMVStoreTool` | `anewarray component 604 length 32768` | 1 | 1:33 | same `MVStore.panic` path, this time through the **bytecode** `anewarray` ladder — confirms the full try/GC/try/reclaim ladder runs and still cannot find the hole |
| `org.h2.test.store.TestMVStoreCachePerformance` | `Capacity: 2097152` (2 MB) | 1 | 5:55, 7 OOM/arena lines | same `MVStoreException` → `MVStore.panic` shape |
| `org.h2.test.jdbc.TestCachedQueryResults` | `native reference array of length 32768` (256 KB) | 124 (killed at cap) | ran the full 40 min cap | **not a crash — a livelock.** The test's own code catches the `OutOfMemoryError` (this looks like intentional cache-eviction-under-pressure testing) and retries. Because the fragmentation that caused the first failure never clears — this collector does not compact — every retry fails identically. **23,468** occurrences of the same `native_oom` WARN between the first one (6 min in) and the process being killed at the 2400 s cap, roughly 12/s, across 5-6 threads all doing it at once. This is the only one of the four that does not crash outright, and it is the only one of the ten classes `hangs-true-vs-perfcliff-RESOLVED-20260821.md` originally listed as "still HANG" that is genuinely stuck — not on a deadlock, but on an allocation that this collector configuration has made permanently unsatisfiable. |

Two things this adds to the record:

* **The `s2_bb_alloc` fix (`bug-h2-testbenchmark-writebuffer-oom-at-1g-FIXED-20260818.md`) is not what's missing here.** `TestMVStoreTool`'s failure goes through the bytecode `anewarray` path, which already runs the full retry ladder (`gc_alloc_array`) — and it still fails. The ladder is not the gap; the collector's refusal to compact while any thread holds a live JIT frame is, exactly as this page already found.
* **`TestCachedQueryResults` reclassifies one of `hangs-true-vs-perfcliff-RESOLVED-20260821.md`'s ten "still HANG" rows.** That page's own framing was "none of the ten are confirmed as a true stuck/deadlocked hang, but neither is that ruled out." This is the one row where it should have been ruled *in* — not a deadlock (threads are burning CPU, not blocked), but a livelock with the same practical consequence: it will never finish, no matter how long the cap. See that page for the other nine, which really are just slow.

None of the four needed a fresh repro to confirm — the existing `grep -c 'arena allocation failed'` / `grep 'zgc frag:'` / `OutOfMemoryError` triage from this page's own "Reproduction" section was sufficient run unmodified against each class.

## A fifth class, and a pre-existing bug this page's fix is not responsible for

Re-verified 2026-08-21 on a clean rebuild of `fix/zgc-movable-bounds-20260821`
merged onto same-day `dev`: `TestOpenClose` and `TestMVStoreCachePerformance`
no longer hit the fast, clean `MVStoreException`/`OutOfMemoryError` crash this
page describes — both now run for several minutes before failing, consistent
with the 21/263 compactions this page already measured. `TestKillProcessWhileWriting`
and `TestMVStoreTool` still fail via the identical clean OOM shape, just later
(`failure_seq` roughly doubled — 32 vs the original 16 — before the ladder gives
up), which is what "proof still incomplete on 89% of cycles" predicts.

**`TestOpenClose` surfaced a second, unrelated defect once it stopped crashing
immediately.** After several `arena allocation failed` warnings it throws:

```
Exception in thread "main" java/lang/Object
(no Java stack frames were captured for this exception)
```

— not a real exception class, no captured frames, thrown from inside
`FileStore.rewriteChunks` → `RandomAccessStore.doHousekeeping`'s background-writer
lambda. **Confirmed pre-existing and unrelated to this page's fix**, not a new
corruption it introduced: reproduces identically, same code path, same shape,
with `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` (the kill switch that restores
this page's exact pre-fix behavior) on the same binary. Something in
exception-object construction goes wrong specifically when the housekeeping
lambda's own OOM handling runs repeatedly — worth its own page, not filed here
because the differential proves it isn't this defect.

## Follow-up 2026-08-22: which disjunct, found and fixed

Instrumented `moving_young_osr_method_needs_fallback` with four per-disjunct
counters (`conservative_roots::osr_fallback_reason`) and wired a cumulative
snapshot into the `[jitroots]` line. On `TestKillProcessWhileWriting`, every
single OSR-fallback firing read `osr_reason=(shadow=0 debug=0 map_coverage=N
exact_rbp=0)` — the blocker is **exclusively** imprecise map coverage, never
the shadow-stack layout and never a missing exact RBP.

> **Corrected 2026-08-24.** The paragraph below asserts that the optimizing
> tier "actually compiles H2's hot OSR loops". It does not compile any OSR body
> at all: the OSR door
> (`jit_bridge.rs`, right before `cm.compiled_via_osr = true`) calls
> `x64::compile_with_param_slots`, the FAST tier, and `ir_lower.rs`'s own
> finalize asserts that "this backend publishes no `osr_pc_to_native`, so
> `osr_enter` refuses at its first `?`". The one-line fix below is still right
> and still landed, but it cannot have been what this class was hitting — which
> is consistent with `map_coverage` still being the sole nonzero disjunct on the
> 2026-08-23 tip. See §"Follow-up 2026-08-24" for what was.

Tracing `fully_oop_covered` (the field that feeds `map_coverage`) found the
mechanism: `x64/driver.rs` (the fast tier) computes it from a real bytecode-PC
subset check; `ir_lower.rs` (the **optimizing** tier — believed at the time to
compile H2's hot OSR loops, per `plan_inline`'s type-guarded virtual
inlining) never computed it at all. It stayed at the `CompiledMethod` struct
default of `false` for every method this tier ever compiled, unconditionally,
OSR or not. `moving_young_osr_method_needs_fallback` reads `false` as
"imprecise" and refuses — so every optimizing-tier OSR artifact failed this
check by construction, regardless of how good its actual maps were.

**The fix is one line**, and it needed no new tracking: each `OopMapEntry`
this backend emits already carries its own `moving_young_coverage_complete`
verdict — the exact per-safepoint flag `gc_quiescence`'s per-cycle proof
already trusts for non-OSR collections, computed at emission time from
`coverable && (published || slots.is_empty())`. `cm.oop_maps` is the complete
set this compilation ever pushed, so the aggregate is a straight `AND`:

```rust
cm.fully_oop_covered = cm.oop_maps.iter().all(|m| m.moving_young_coverage_complete);
```

Landed in `ir_lower.rs`, right after `cm.oop_maps = oop_maps;`.

### Verified three ways

1. **Unit tests.** `cargo test --release -p cratonvm-jit`: 2091+ tests, 0
   failed — the fix touches no existing invariant the suite already checks.
2. **The runtime oracle, differentially.** `CRATONVM_DBG_VERIFY_OOP_MAPS`
   counts `never_mapped (while_covered=N)` — an in-band live oop that was
   never mapped **despite its frame claiming full coverage**, which is
   exactly the shape a wrong `fully_oop_covered=true` would produce. Ran
   `TestKillProcessWhileWriting` twice, same binary, same host, one variable:
   with the fix, `while_covered=1192`; with `ir_lower.rs`'s one line reverted
   (binary rebuilt, class rerun), `while_covered=1410`. **The count did not
   go up with the fix — if anything it went down.** This settles that the
   fix does not introduce a new false-coverage claim: the audit finding is
   pre-existing, and almost certainly lives in the fast tier's own
   bytecode-PC subset check (`x64/driver.rs`), which this page's earlier
   fast-tier analysis already flagged as collision-prone once more than one
   bytecode stream shares the PC namespace — untouched by this fix, and now
   confirmed present with or without it. **Filed separately, not fixed
   here**: it is a distinct, pre-existing correctness gap this fix's own
   verification oracle surfaced, not something this fix caused.
3. **Corruption canary.** 6× `TestMultiThread`, ABBA-style, clean directory
   per rep, on the fixed binary: zero `SIGSEGV`/`ClassCastException`/panics/
   assertion failures across all six (all six hit the host's own 300 s cap
   under heavy contention, not a class-level failure).

### Measured effect on the five affected classes

`TestOpenClose` and `TestMVStoreCachePerformance` — the two that used to
crash via a fast, clean `MVStoreException`/`OutOfMemoryError` within a few
minutes — no longer hit that crash at all on a solo, uncontended rerun;
`TestOpenClose` instead surfaces the unrelated pre-existing `java/lang/Object`
exception documented above, confirmed via the same kill-switch differential
methodology to be unaffected by either this fix or the ZGC one.
`TestKillProcessWhileWriting` and `TestMVStoreTool` still eventually fail via
the same clean OOM shape, later than before (`failure_seq` roughly doubled) —
consistent with the coverage proof now succeeding some of the time rather
than never, but not always: H2's MVStore workload keeps enough peer threads
in compiled code that `CROSS_THREAD_JIT_PEER` (a different, still-conservative
obligation — see `roots.rs`'s own comment on the cross-thread coverage
handshake `arch-2026-07-26/moving-young-precise-roots.md` specifies and
nobody has built) still blocks the majority of cycles.


## Follow-up 2026-08-24: the blocker, twice one level down

Everything below is on one host, one class (`TestKillProcessWhileWriting`,
`--Xmx 1g`, default ZGC + JIT, real JDK 25), and every comparison is against a
kill switch on the SAME binary.

### 1. The measurement that redirected the search

`CRATONVM_DBG_JIT_ROOTSCAN=1` on the 2026-08-23 `dev` tip, full run:

| reason | cycles |
|---|---:|
| `osr-shadow-coverage-unproven` | **725** |
| `none` (proven) | 25 |
| `compiled-frame-oop-not-published` | 4 |
| `xt-helper-window-conservative-scan` | 4 |
| `active-safepoint-map-incomplete` | 1 |
| **`cross-thread-jit-peer`** | **0** |

`compaction_cycles=25`, `relocation_skipped_jit=734`, 4 `OutOfMemoryError`, 10
`arena allocation failed`, `rc=1`. The `osr_reason` breakdown put every one of
the 725 on `map_coverage` — the disjunct the 2026-08-22 follow-up believed it
had fixed — and none on the shadow layout or a missing exact RBP.

The cross-thread handshake this page asked for **was** built (a peer proves its
own frames at its own park and deposits the proven depth; the initiator accepts
only when the deposits account for every peer JIT entry in the process;
`CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` restores the blanket refusal). On this
workload it decides nothing: `xt_cov=(accepted=0 refused=0 deposits=0)`. That
is the honest report — it removes a blanket refusal this class was not hitting.

### 2. `CRATONVM_DBG_OOPCOV` — the method, and which term said no

`fully_oop_covered` going false is reported to the runtime as one counter, and
six conditions produce it. Counted separately, on the same run:

```
causes(marks_inexact=10 oop_in_reg=0 stack_deep=0 local_deep=0
       staged_deep=0 staged_unmappable=439)
```

**439 of 449 are one shape.** All three direct JIT-to-JIT call arms raise
`pending_staged_args_unmapped` when any argument is a reference: the argument is
popped off the operand stack and marshalled into the outgoing-ABI area, which
no frame-slot map can name, so the safepoint's pc is withheld from
`mapped_safepoint_pcs` and the method's coverage bit goes false permanently.

That is a correct answer to the wrong question.
`moving_young_osr_shadow_fallback_needed` is about **rewritable shadow
coverage**, and `fully_oop_covered` on the fast tier — the only tier that
produces OSR artifacts, since `ir_lower` publishes no `osr_pc_to_native` — means
`safepoint_pcs` being a subset of `mapped_safepoint_pcs`, i.e. every live oop
named by a FRAME SLOT. The staged argument is not the caller's live value any
more; it is the CALLEE's parameter, covered by the callee's own locals map, and
the caller never re-reads the outgoing area after the call.

`CompiledMethod::fully_shadow_covered` is the aggregate that asks the shadow
question (has maps AND every one claims `moving_young_coverage_complete`),
computed by **both** backends, and the OSR check reads it.
`CRATONVM_OSR_COVERAGE_SHADOW=0` restores the frame-slot reading.

Narrowing that term does not weaken the proof. The OSR fallback
SHORT-CIRCUITS PAST `refresh_moving_young_coverage_for_collection`, so the
effect of the change is that the strictly sharper per-frame checks get to run
at all: `moving_young_frame_coverage_complete` on the active frame and every
parent (the same flag, resolved through the *live safepoint id*, so per-frame
rather than per-method) and the band verifier's empirical walk for
young-resident unpublished words.

### 3. The entry poll made every method's shadow bit false

Moving the OSR check onto `fully_shadow_covered` changed nothing on its own —
359 of 392 collections still refused. Same problem one level over, and seven
counters later:

```
scauses(gate=0 desync=0 marks=10 scratch=0 locals64=0 dataflow=800 nopush=0)
```

with every uncovered method reporting exactly `shadow_missing_pcs=[4294967295]`.

`emit_safepoint_poll_prologue` stamps `cur_bc_pc` with `u32::MAX` so the
method-entry poll's map cannot collide with a genuine bci-0 safepoint in the
sp-id-keyed lookup. That is right, and nothing then answered for that pc:
`local_oop_reached.get(u32::MAX)` is `None`, so
`moving_young_safepoint_coverage_complete` read the entry poll as "the dataflow
never reached here" and pushed its `OopMapEntry` with
`moving_young_coverage_complete: false`. A method-wide AND over the maps
therefore read **false for every method the fast tier ever compiled**.

The entry state is knowable and is not a guess: the poll is emitted from the
END of `emit_prologue`, so every argument is homed; the operand stack is empty;
and `emit_pre_safepoint_spill` flushes every register-homed local to its
canonical frame slot before the call. The live oops are exactly the reference
parameters — `param_oop_mask`, the same value that seeds the dataflow at bci 0.
It is kept as its own field rather than read back out of `local_oop_masks[0]`,
because when bci 0 is also a branch target that entry has been intersected with
the back edge, which is a SUBSET, and publishing a subset under a completeness
claim is the unsound direction. One accessor, `local_oop_mask_at_current_pc`,
now serves all five readers, so the map, the push and the reload cannot
disagree about what the entry poll covers.

### 4. Measured, on the class this page is about

| | `dev` (2026-08-23) | with the fixes |
|---|---:|---:|
| `rc` | **1 FAIL** | **0 PASS** |
| `OutOfMemoryError` | 4 | **0** |
| `arena allocation failed` | 10 | **0** |
| collections | 759 | **54** |
| `osr-shadow-coverage-unproven` | 725 | **0** |
| `relocation_skipped_jit` | 734 | **29** |
| `relocation_on_proven_jit` | 23 | 25 |
| `objects_relocated` | 197 834 | 318 490 |

The compaction COUNT barely moves (25 to 25). What moves is the rate: the same
number of compactions now happens across 54 collections instead of 759, i.e.
early enough that the heap never enters the death spiral of hundreds of
unproductive cycles. Reading `compaction_cycles` alone would have missed the
fix entirely.

The remaining refusals are honest per-frame ones:
`active-safepoint-map-incomplete` 21, `compiled-frame-oop-not-published` 8,
`none` (proven) 25.

### 5. Verification

* **ABBA A/B, three reps, one binary, interleaved.** `on` = default,
  `off` = `CRATONVM_OSR_COVERAGE_SHADOW=0`:

  | rep | on | off |
  |---|---|---|
  | 1 | `rc=0 oom=0` | `rc=1 oom=4` |
  | 2 | `rc=0 oom=0` | `rc=1 oom=4` |
  | 3 | `rc=0 oom=0` | `rc=1 oom=4` |

  Against a same-day base rate of 3 failures in 3 runs of the unmodified `dev`
  binary: 6 failures on the old behaviour, 3 passes on the new, no crossovers.
* **Corruption canary.** `org.h2.test.db.TestMultiThread` times 6 on the fixed
  binary, clean directory per rep: 6/6 `rc=0`, zero SIGSEGV, zero
  `ClassCastException`. Note what this canary can and cannot see — it records
  `compaction_cycles=0` on five of the six reps, so the class barely exercises
  relocation. The runs that actually compact under this change are the passing
  `TestKillProcessWhileWriting` arms above (22–25 compactions, 288–318 k objects
  relocated, no SIGSEGV, no wrong answer), and those are the real exposure.
* **Other collectors.** Generational: `rc=0`, no OOM, no arena failure —
  unchanged. G1: fails this class before and after, and was A/B'd against the
  kill switch on `TestRandomMapOps` as well (5 interleaved arms, identical in
  every column) because this change also decides whether G1 publishes shadow
  oops pinned or movable. See
  `bug-h2-testrandommapops-classcastexception-20260821.md`.
* `cargo test --release --lib -p cratonvm-jit -p cratonvm-gc -p cratonvm-types
  -p cratonvm-vm`: 2105 + 1685 + 581 + 2610 pass, 0 fail. (One run, on a host at
  load 234, flaked
  `threading::monitor::tests::a_timed_waiter_returns_as_soon_as_it_is_notified`
  on its 5 s budget; it passes on an idle host and touches nothing here.)

### 6. Effect on the other four classes this page names

All five re-run on the fixed binary, `--Xmx 1g`, default config. HotSpot 25 at
`-Xmx1g` passes every one of them (26 s / 25 s / 39 s / 10 s), so each remaining
failure is a real CratonVM defect.

| class | before | after |
|---|---|---|
| `TestKillProcessWhileWriting` | OOM, `rc=1` | **`rc=0`** |
| `TestMVStoreTool` | OOM, `rc=1` | OOM, `rc=1` — `compaction_cycles=0`, see *Still open* |
| `TestOpenClose` | arena failures + the `java/lang/Object` exception | **no OOM**; fails only on that separate, pre-existing exception defect |
| `TestMVStoreCachePerformance` | OOM | **no OOM, no arena failure at all**; now fails on `NoSuchMethodError: 'boolean org.h2.mvstore.Page$PageReference.isPersistent()'` — a different defect |
| `TestCachedQueryResults` | livelock, 23 468 `native_oom` | still a livelock, 7 761–15 522 `native_oom` |

Two of the five are off this defect entirely; a third is fixed; two remain.

### 7. `TestMVStoreTool`'s residual, named

Same wall — `request=262160`, `free_list_bytes=1010924832` (94 % free),
`largest_free_block=65440`, `free_spans=77928` — reached in only **8
collections**, all of which refused, `compaction_cycles=0`. The per-cause split
of `ACTIVE_FRAME_MAP` (added 2026-08-24) is unambiguous:

```
frame_cov=(no_slot=0 misaligned=0 no_map=9 incomplete=0 ok=48)
```

**No map ever refuses on its own claim.** Nine frames simply cannot be located:
the word in the innermost frame's safepoint-id slot matches no `OopMapEntry` of
the method the frame record names. `[frame-cov]` prints each one, and the shape
is the finding:

```
method=org/h2/mvstore/RootReference.isLocked:()Z maps=0 sp_id_slot_off=24
  rbp=0x...3a00 own_ret=0x...353e own_ret_ok=true
  own_caller=org/h2/test/store/TestMVStoreTool.testCompact:()V
  decoded_callee=None
```

One `rbp`, one saved return address, and across collections **four different
methods** claiming it — `RootReference.isLocked` (which has **zero** oop maps,
so it can never be the frame at a safepoint), `MVMap.getRoot`,
`MVMap$DecisionMaker.decide`, `MVMap.replacePage`. At most one can be right.
`decoded_callee=None` on every occurrence: the calls are indirect (`CALL R11`),
which is why the `(rbp, compile-id)` mirror exists and why the stack cannot
corroborate it.

Two hypotheses were tested and **both failed**, which is the useful part:

* **Not the spliced-call path.** With `CRATONVM_JIT_INLINE_CALLS`,
  `CRATONVM_JIT_INLINE_NEST` and `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` all `0`,
  the identical shape appears — same four methods, same rbp.
* **Not a missing post-call republish.** Publishing the mirror at every
  GC-capable safepoint (where the GC reads it) was implemented, confirmed
  engaged (`[INLINE-FR] inline frame-record ENABLED: storing RBP via mov
  fs:[0xffffdd20]`), and moved nothing: `no_map` 7 to 5, `ok` 64 to 70,
  `rc=1 oom=4 compaction_cycles=0` in both arms. It was reverted — two stores
  per safepoint with no workload behind them do not stay in the hot path — and
  the negative result is recorded here so the next attempt starts after it
  rather than before it.


## Follow-up 2026-08-24 (second): a real frame-record gap, and what it did NOT fix

Taking on §7's residual — `ACTIVE_FRAME_MAP` / `frame_cov=(… no_map=N …)`, the
innermost compiled frame that cannot be LOCATED.

### The defect, found by reading and pinned by a test

The optimizing tier reaches a compiled Java callee from an inline cache through
one helper, `ir_lower::emit_call_cached_entry` (`MOV R11,[R10+d] ; CALL R11`),
used by the monomorphic MIC arm and by every rung of the polymorphic PIC
cascade. **Neither restored the innermost-frame mirror afterwards.** Every
sibling path does: the single-pass backend republishes at both of its
equivalent arms, the shared hashed/vtable stub is handed `frame_record` for
exactly this, the Rust dispatch path brackets it in `try_call_compiled_entry`,
and `ir_lower`'s other three call sites call `emit_post_call_frame_record` —
a function this tier already had, already RAX-safe, simply never called here.

So after any inline-cache hit in that tier the mirror named the callee's frame,
which had already returned.

Fixed by folding the republish INTO `emit_call_cached_entry`, so a third arm
cannot be added without it. `CRATONVM_JIT_NO_IC_FRAME_REPUBLISH=1` is the
same-binary A/B.

`every_inline_cache_hit_restores_the_frame_record_before_anything_else` asserts
the byte immediately after each cached-entry `CALL R11`, because the PLACEMENT
is the property — anything emitted between the return and the republish runs
under a mirror naming a dead frame. **It discriminates**: with the kill switch
set it fails with `cached-entry CALL at 197 is followed by 0xe9`, `0xE9` being
the `JMP rel32` to `.done` that used to follow the call directly. A second
control (`frame_record = 0`, which switches the whole record off) asserts that
NO site carries the republish, so the test cannot pass vacuously.

### It is heavily engaged — and it changes nothing here

`ic_fr_sites` on the `[jitroots]` line is the compile-time count of
optimizing-tier inline-cache sites that received the republish. It is **3 585 –
7 700 per run** on these H2 classes, and **0** with the kill switch set, so this
is a working engagement control rather than an inert probe.

`TestKillProcessWhileWriting`, current `dev` + this fix, one binary, arms
interleaved, `--Xmx 1g`:

| arm | `ic_fr_sites` | `no_map` / (`no_map` + `ok`) | compactions | cycles | rc |
|---|---:|---:|---:|---:|---|
| on | 3 725 | 137 / 2 708 = **5.06 %** | 12 | 216 | 1 |
| off | 0 | 175 / 3 144 = **5.57 %** | 10 | 268 | 1 |
| on | 3 730 | 402 / 7 539 = **5.33 %** | 11 | 582 | 1 |
| off | 0 | 427 / 8 248 = **5.18 %** | 8 | 636 | 1 |
| on | 3 695 | 174 / 3 419 = **5.09 %** | 9 | 270 | 1 |

The rates are indistinguishable, and every arm fails with the same 4
`OutOfMemoryError`. **The optimizing tier's inline caches are not what produces
`no_map` on these classes.** The fix is kept on its own correctness merits —
the gap is real, every sibling path closes it, and the test proves the
placement — but it is not this page's repair, and nobody should read it as one.

Two calibrations that come with that table, both of which say a workload verdict
is not available on this host today:

* **The class is flaky on current `dev`.** The same binary and the same flags
  produced `rc=0 oom=0` (11 compactions, 67 cycles) in one run and `rc=1 oom=4`
  in the five above. Score any future arm against that, not against a
  remembered deterministic failure.
* **Wall clock swings 5×.** `TestMVStoreTool` ranges 89 s – 1 500 s at host load
  45 – 65, and its `no_map` share has fallen from 16 % (9 of 57) on the
  2026-08-23 tree to ~1 %. An arm taken here cannot separate a repair from the
  host.

### The census, re-taken, and what is now ruled out

The signature §7 describes still holds on current `dev` — several unrelated
methods claiming ONE rbp with one saved return address, all
`decoded_callee=None` because the calls are indirect:

```
MVMap.put            maps=6 sp_id_slot_off=40 rbp=0x…3a00 own_ret=0x…2bfb
MVStore.openMap      maps=3 sp_id_slot_off=32 rbp=0x…3a00 own_ret=0x…2bfb
MVStore$Builder.autoCommitDisabled maps=3 sp_id_slot_off=24 rbp=0x…3a00 own_ret=0x…2bfb
DataUtils.getPageMaxLength maps=1 sp_id_slot_off=40 rbp=0x…2bf0
```

At most one of the three at `0x…3a00` can be right, so the mirror IS naming a
frame that has returned. What no longer explains it:

1. **Not the spliced-call path** — identical shape with
   `CRATONVM_JIT_INLINE_CALLS` / `_NEST` / `_SPLICE_DEVIRT` all `0`.
2. **Not a missing publish at the safepoint** — publishing the mirror at every
   GC-capable safepoint was implemented, confirmed engaged, moved nothing, and
   was reverted (§7).
3. **Not the optimizing tier's inline caches** — the table above.

Every JIT→JIT return path now republishes, and the Rust dispatch bracket covers
both directions, so the next candidate is a path that returns WITHOUT running
any of them. Worth checking in this order: the OSR trampoline's exit, the deopt
/ callee-deopt service bail, and any exception unwind that leaves a compiled
frame without passing its call site's republish.

### Also measured

* `TestCachedQueryResults` shows `incomplete=5` — the FIRST occurrence anywhere
  of a map refusing on its own claim, alongside `no_map=2477 ok=35503` over
  4 991 collections and 9 962 `OutOfMemoryError`. That is a different obligation
  from `no_map` and has never been looked at.
* `UNPUBLISHED_FRAME_OOP` (the `CRATONVM_JIT_INDY_BRIDGE` blocker bisected in
  §Status) is present in every arm above — 67, 81, 166, 195, 85 refusals — so it
  remains a second, independent reason these collections decline.
* 2 096 jit + 581 types + 1 687 gc + 2 610 vm unit tests pass with the fix.
## Still open

Ordered by what a next session should pick up first.

* **The `invokedynamic` bridge's compiled frames do not publish their live oops
  to the shadow stack.** Bisected to `CRATONVM_JIT_INDY_BRIDGE=0` on one binary
  — see the table under §Status: it takes `compiled-frame-oop-not-published`
  from 28 back to 5 per 76 collections, where none of the other four 2026-08-24
  switches moves it below 21. The feature is `2cc02f7d3` and its own WIP commit
  `a51077342` names "the indy bridge's staged-arg oop-map gap", so this is
  most likely already known to its author. Left for them; recorded here because
  it is what makes this class fail on `dev` today, and because the counter that
  found it is now on the `[jitroots]` line for anyone else.
* **A second, unidentified contributor to `ACTIVE_FRAME_MAP`.** With the indy
  bridge off, that reason is 56 per 76 cycles against 46 before the merge, and
  proven cycles 13 against 24. None of the five switches covers it. Almost
  certainly the same question as the `no_map` residual below.
* **`TestMVStoreTool` and `TestCachedQueryResults` — the innermost frame cannot
  be LOCATED, and THREE hypotheses are now dead.** Read §"Follow-up 2026-08-24
  (second)" before picking this up: the spliced-call path, a missing publish at
  the safepoint, and the optimizing tier's inline caches have each been tested
  and each changed nothing, the last of them against a working engagement
  counter (`ic_fr_sites` 3 695–3 730 vs 0). Every JIT->JIT return path now
  republishes and the Rust dispatch bracket covers both directions, so what is
  left is a path that returns WITHOUT passing any of them — the OSR trampoline's
  exit, the deopt / callee-deopt service bail, or an exception unwind. Note also
  that this class is now FLAKY on `dev` (`rc=0` once, `rc=1` five times, same
  binary, same flags) and that its wall clock swings 5x at host load 45–65, so
  the next arm needs a measured base rate before it means anything.
* **`TestCachedQueryResults` shows `incomplete=5`** — the first time anywhere
  that a map refuses on its OWN claim rather than being unlocatable. Different
  obligation from `no_map`, never investigated, and it sits alongside 9 962
  `OutOfMemoryError` over 4 991 collections.
* **`TestOpenClose`: `Exception in thread "main" java/lang/Object`, no captured
  frames.** Now the ONLY thing failing this class — the fragmentation OOM is
  gone. Already confirmed pre-existing and unrelated by the kill-switch
  differential on 2026-08-21, and it reproduces on the fixed binary with zero
  `OutOfMemoryError` and four `arena allocation failed`, which is as clean a
  separation as this defect will ever get. Its own page, and now cheap to
  reproduce.
* **`TestMVStoreCachePerformance`: `NoSuchMethodError: 'boolean
  org.h2.mvstore.Page$PageReference.isPersistent()'`.** Also now the only thing
  failing that class — no OOM and no arena failure at all. A method-resolution
  defect with nothing to do with this page; filed here only because this page
  is what was watching the class.
* **`-XX:+UseG1GC` fails `TestKillProcessWhileWriting`**, identically before and
  after this work (13 `OutOfMemoryError` and a 1500 s cap on the fixed binary,
  2 `OutOfMemoryError` in 31–43 s when this page first measured it). The face
  varies between runs, so reproduce it several times before believing any
  single one. Not this defect: no `arena allocation failed` in either era.
* **A pre-existing, unrelated correctness gap the 2026-08-22 OSR fix's own
  verification surfaced.** `CRATONVM_DBG_VERIFY_OOP_MAPS`'s
  `never_mapped (while_covered=N)` counter is nonzero on `dev` with or without
  that fix (1192 vs 1410 on the same class, same host) — some frame claims
  `fully_oop_covered=true` while an in-band live oop goes unmapped, on the fast
  tier's own bytecode-PC subset check. **Read that in the light of §"Follow-up
  2026-08-24" §2 before re-opening it**: `fully_oop_covered` is the FRAME-SLOT
  notion, and the direct-call staged-argument shape means it is routinely false
  for reasons that are not a bug — so the interesting number is now the same
  oracle measured against `fully_shadow_covered`, which nobody has taken. Only
  23 distinct `code+offset` sites produce the 1410 baseline occurrences.
* **The two small objects in the large-object region.** The fragmentation report
  placed an 80-byte `String` and a 24-byte `Object` above `high_cursor`, where
  `ZGC_LARGE_OBJECT_MIN`'s design says only large objects should live, and they
  are what caps `high_max` below the request. The tripwire added to
  `Arena::alloc` fired **zero** times across a full failing run, so the low-end
  allocation paths are not the producer. Note its reach before trusting that
  zero: it covers the three free-list exits of `Arena::alloc` and
  `push_block_routed`, not the TLAB fast path (whose chunks are carved from the
  low end) and not the bump path (which cannot cross `high_cursor`). It has
  never been seen to fire, so it is an untriggered instrument, not evidence.
  Less urgent than it was: the class this page is about no longer reaches the
  wall at all.
* **The cross-thread coverage handshake decides nothing yet.** It is built,
  default-ON, and `xt_cov=(accepted=0 refused=0 deposits=0)` on this workload —
  no peer was ever in compiled code at a collection here. It removes a blanket
  refusal that a genuinely many-threaded workload would hit; that claim is
  untested because this class does not produce the condition. The engagement
  counter is on the `[jitroots]` line precisely so nobody reads a win into it.
