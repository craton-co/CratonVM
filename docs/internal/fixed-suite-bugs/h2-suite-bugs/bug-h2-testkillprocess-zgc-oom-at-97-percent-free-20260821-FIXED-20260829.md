# `TestKillProcessWhileWriting` — `OutOfMemoryError` with 97 % of the heap free, because ZGC never compacts once the JIT engages

## Status

> ### 2026-08-29 — read this first
>
> **The page's own class has passed since 2026-08-24/26. What was still failing
> on 2026-08-28 — `TestMVStoreTool` and `TestCachedQueryResults` — was failing
> on FOUR defects, and §"Follow-up 2026-08-29" fixes all four.** In the order
> that matters:
>
> 1. **The slide was discarding every byte it emptied** whenever the cursor
>    could not follow it down, and no later sweep could rediscover them (the
>    sweep walks the object-start registry, which the slide has just rebuilt).
>    885 793 objects relocated per run and the largest free block was 8 184
>    bytes.
> 2. **The large-object end had no compactor at all** — 99–198 free blocks
>    where one would do.
> 3. **The TLAB refill floor** was one notch above what the free list could
>    serve, so every refill bumped and the 128 MiB large-object reserve was
>    spent on churn.
> 4. **The region tripwire** meant to catch small objects leaking into that end
>    was armed on three exits that could not fire it.
>
> Each has a same-binary kill switch (`CRATONVM_ZGC_PUBLISH_VACATED`,
> `CRATONVM_ZGC_HIGH_COMPACTION`, `CRATONVM_ZGC_TLAB_STARVED_RECYCLE`) and its
> own engagement counter on the `[GC] zgc-high-compaction:` line, because the
> feature this work supersedes shipped reading zero for a week and the only
> reason anyone found out is that it carried a counter.
>
> **One tracked class is NOT fixed, and it is where the repairs are best
> measured.** `org.h2.test.jdbc.TestCachedQueryResults` still livelocks, and it
> is the one class in this family that throws THOUSANDS of `OutOfMemoryError`
> per run rather than one — so a rate is measurable. Same binary, 900 s cap:
> **`oom=2990` on the default against `oom=6318` with all three switches off.**
> The repairs halve it and change nothing else. It throws thousands
> and keeps running, which is a different shape from every other class here
> (those failed once and died), so something is catching and retrying and that
> is not a collector question. It has its own page:
> `bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829.md`.
>
> **Everything below this box is the record of how the diagnosis got here**,
> including three attributions this page had to withdraw. Read
> §"What this page no longer tracks, and where it went" for where each 2026-08-28
> row ended up.

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

**The `ACTIVE_FRAME_MAP` residual is CLOSED as of 2026-08-26.** The two
innermost-frame mirrors — the RBP and the compile id — were not moving together
across a JIT entry-chain push or pop, so the pair named two different frames and
the proof read the wrong method's slot offset. `no_map` is **0 in all five
`on` arms and 9–601 in all five `off` arms** of the same-binary A/B, and
`TestKillProcessWhileWriting` passes 2/2 with it against 2/2 failing without.
See §"Follow-up 2026-08-26".

Still open: `TestMVStoreTool` and `org.h2.test.jdbc.TestCachedQueryResults`
reach the same fragmentation wall on TWO other obligations —
`compiled-frame-oop-not-published` (the `invokedynamic` bridge, bisected below)
and `xt-helper-window-conservative-scan`. See §"Still open".

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
> (5.06/5.33/5.09 % on against 5.57/5.18/5.14 % off, 6/6 failing, engagement
> counter 3 695–3 730
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
| off | 0 | 216 / 4 200 = **5.14 %** | 9 | 329 | 1 |

Three reps per arm: **5.06 / 5.33 / 5.09 %** on against **5.57 / 5.18 / 5.14 %** off. Indistinguishable, and all six fail with the same 4
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

## Follow-up 2026-08-26: the cause — the two frame-record mirrors were not moving together

`ACTIVE_FRAME_MAP` / `frame_cov=(… no_map=N …)` is **fixed**. The innermost
compiled frame could not be located because the pair that names it was allowed
to describe two different frames.

### The rule, and where the entry chain broke it

`top_cm_id_mirror_read`'s own doc states it: the two innermost-frame mirrors —
the RBP and the compile id — are written together by generated code, and

> restoring one without the other is how the identity becomes actively wrong
> rather than merely absent

because the scan cannot detect it: both halves still read consistently out of
the mirrors. The JIT entry chain broke that rule in **both** directions:

* `push_entry_full` snapshots both halves into the outgoing top entry, then
  calls `top_rbp_set(0)` for the incoming one — and never zeroed the identity.
  A fresh entry's rbp was paired with the method of the entry *below* it from
  its first instruction.
* `reload_top_rbp_cache` — commented "mirror now tracks the entry that became
  top again" — restored only `exact_rbp` from the snapshot, leaving the identity
  at whatever last ran.

And the coverage proof feeds itself the bad pair:
`refresh_moving_young_coverage_for_current_thread` calls
`prune_returned_jit_entries` **first**, which lands in `reload_top_rbp_cache`
whenever it pruned anything, and then immediately stamps

```rust
info.exact_rbp   = top_rbp_get();          // correct
info.exact_cm_id = published_compile_id(); // whatever nothing moved
```

`moving_young_frame_coverage_complete` then reads
`[rbp − wrong_method.sp_id_slot_off]`, matches no map, and refuses the cycle.

That is exactly the signature §7 recorded and could not explain: ONE rbp with
one saved return address, claimed across collections by four different methods
— `MVMap.put`, `MVStore.openMap`, `MVStore$Builder.autoCommitDisabled`, and
`RootReference.isLocked`, the last with `maps=0`, a method that emits no
safepoint and therefore cannot be the frame at a collection at all. At most one
could be right. The rbp was right every time; the identity was the half nobody
moved.

### Measured, one binary, `CRATONVM_GC_NO_CM_ID_PAIRING=1` as the control

| class | arm | `no_map` | `active-safepoint-map-incomplete` | cycles | compactions | rc |
|---|---|---:|---:|---:|---:|---|
| `TestMVStoreTool` | on | **0** | 0 | 44 | 0 | 1 |
| | off | 9 | 7 | 40 | 7 | 1 |
| | on | **0** | 0 | 18 | 7 | 1 |
| | off | 11 | 0 | 44 | 0 | 1 |
| | on | **0** | 0 | 44 | 0 | 1 |
| | off | 2 | 2 | 8 | 3 | 1 |
| `TestKillProcessWhileWriting` | on | **0** | 0 | **52** | **21** | **0 PASS** |
| | off | 601 | 601 | 886 | 12 | 1 |
| | on | **0** | 0 | **52** | **16** | **0 PASS** |
| | off | 105 | 104 | 148 | 8 | 1 |

**Zero in all five `on` arms, nonzero in all five `off` arms**, and the reason
code tracks the counter exactly. `TestKillProcessWhileWriting` passes 2/2 with
the pairing and fails 2/2 without it, `oom=0 arena=0` against `oom=4 arena=7–10`
— and the death spiral this page opened on (886 and 148 collections) collapses
to 52.

### What this does NOT close

`TestMVStoreTool` still fails, with `oom=4–6`. Its `no_map` obligation is gone;
what refuses now is `compiled-frame-oop-not-published` (the
`CRATONVM_JIT_INDY_BRIDGE` blocker bisected under §Status) and
`xt-helper-window-conservative-scan`. Those are independent obligations and
this repair does not touch them — one `on` arm above spent all 44 of its cycles
refusing on the helper window alone.

Note also that zero is the *honest* value when a top entry carries no snapshot:
it means "nothing published", which routes `published_innermost_method` to the
stack decode and, failing that, fails closed. A wrong id does not fail closed —
it resolves confidently to another method's oop map, which
`remap_active_jit_frames` would then have used to rewrite that frame's slots.
So this was a latent corruption hazard, not only a lost compaction.

### Pinned

`both_frame_record_mirrors_move_together_across_a_jit_boundary` asserts the
invariant through the public push/pop entry points, because it is about what a
JIT boundary leaves behind rather than about one function. It **discriminates**:
with `CRATONVM_GC_NO_CM_ID_PAIRING=1` it fails with the incoming entry's rbp
paired with `0xABCD1234`, the outer entry's identity. Its sibling,
`the_rbp_mirror_half_is_restored_regardless_of_the_pairing_switch`, asserts only
the half that was always correct, so a future change there cannot hide behind
the switch.

### Three hypotheses that were wrong, kept because they cost runs

Recorded so nobody re-derives them: the spliced-call path (§7), publishing the
mirror at every safepoint (§7, reverted), and the optimizing tier's inline-cache
republish (§"Follow-up 2026-08-24 (second)", a real gap, fixed on its own
merits, and a measured null here against an engagement counter). All three were
about generated code restoring the mirror. The defect was in the *chain
bookkeeping* that generated code hands off to.


## Follow-up 2026-08-26 (second): `UNPUBLISHED_FRAME_OOP`, censused

### First, a correction: the indy-bridge attribution no longer holds

§Status bisected this reason to `CRATONVM_JIT_INDY_BRIDGE` on 2026-08-24 (28 → 5
per 76 collections). **Re-taken on today's `dev`, on an idle host, that
separation is gone:**

| arm | `unpub` | proven | rc |
|---|---:|---:|---|
| indy bridge on | 30 | 20 | 0 |
| indy bridge off | 28 | 23 | 0 |
| indy bridge on | 27 | 23 | 0 |
| indy bridge off | 33 | 19 | 0 |

The tree moved twice underneath that bisect — most of all the mirror-pairing
repair in §"Follow-up 2026-08-26", which changed *which frames reach the band
verifier at all* (`no_map` went from hundreds to ~1). Quote the 08-24 number as
history, not as a live attribution. `TestKillProcessWhileWriting` also passes
4/4 on plain `dev` now.

### What the unpublished words actually are

`CRATONVM_MOVING_YOUNG_BAND_DBG=1` runs `report_unpublished_band_words`, which
names the method and the frame REGION of every word the verifier objects to.
One `TestKillProcessWhileWriting` run on `dev`:

| region | words |
|---|---:|
| **`java-local`** | **64** |
| `operand-spill` | 17 |
| `reserved-locals-tail` | 2 |

and by method, 64 of the 83 are in **one**:

```
64  org/h2/mvstore/MVStore.closeStore:(ZI)V
12  org/h2/mvstore/MVStore$Builder.open:()Lorg/h2/mvstore/MVStore;
 2  org/h2/mvstore/MVStore.hasUnsavedChanges:()Z
 2  org/h2/mvstore/FileStore.clearCaches:()V
```

A representative line — note the same object in three consecutive local slots:

```
[moving-young-band] org/h2/mvstore/MVStore.closeStore:(ZI)V off=72 region=java-local
  value=0x2004f4d3e28 published=8 live_hi=Some(144)
  layout=FrameLayout { java_locals_hi: 80, … locals_hi: 128, spill_lo: 128, … }
```

`off=56/64/72` are locals 6, 7 and 8, all holding `0x2004f4d3e28`, none of them
in the 8 values the shadow stack published at that safepoint.

**This is a liveness question, and it forks two ways.** If the dataflow says
those locals are LIVE at that pc, `collect_live_oop_homes`'s oop-locals loop —
which publishes exactly `local_oop_mask_at_current_pc()` — has a mask gap, and
that is a codegen defect. If it says they are DEAD, then not publishing them is
CORRECT and the slot merely holds a stale reference nothing will read; the band
verifier cannot tell the two apart, so it refuses, and the repair belongs on the
verifier's side (a liveness bound it can consult, or zeroing dead ref slots).
Nobody has asked which. That is the next measurement, and `closeStore` is a
one-method target for it.

### The staged invoke-argument buffer: a real gap, fixed, and why it does not move `unpub`

`a51077342` added the staged buffer to the oop MAP (`emit_oop_map_for_safepoint`
Stage 3) and stopped there. `collect_live_oop_homes` — which decides what
`emit_shadow_push` publishes — enumerates the simulated operand stack and the
oop locals, and by construction neither can see these: they were popped off the
operand stack before the call, the same reason the map needed a Stage 3.

The two are different channels. The map makes a slot a precise root for
MARKING; the shadow stack is the REWRITABLE channel, and it is the only one
`moving_young_unpublished_frame_oop_present` consults. Fixed, gated on moving
coverage like the frame-slot homes beside it, `CRATONVM_JIT_NO_STAGED_ARG_SHADOW=1`
as the A/B.

**It does exactly what it targets and nothing more** — same class, same run
shape, band census on each binary:

| region | `dev` | with the fix |
|---|---:|---:|
| `java-local` | 64 | 65 |
| **`operand-spill`** | **17** | **7** |
| `reserved-locals-tail` | 2 | 4 |

And it does **not** move the per-cycle refusal count:

| arm | `unpub`/cycles | proven | arm | `unpub`/cycles | proven |
|---|---:|---:|---|---:|---:|
| on | 24/51 | 26 | off | 31/51 | 19 |
| on | 23/51 | 27 | off | 33/52 | 19 |
| on | 28/51 | 23 | off | 25/51 | 26 |

That null is **explained, not mysterious**: `unpub` counts COLLECTIONS, and a
collection refuses if ANY word in any band is unpublished. While `java-local`
still contributes 64 words to the same cycles, removing 10 `operand-spill` words
cannot change a single cycle's verdict. The fix is kept on that basis — it is
correct, it is measurably effective on its own category, and it cannot show a
cycle-level win until the dominant category is closed.

Two honest caveats on it:

* On `TestMVStoreTool` the `unpub`-per-cycle RATE went the wrong way (0.57 and
  0.82 with, against 0.47 and 0.50 without). That class's runs vary from 10 to
  38 collections and 62 s to 356 s, so this is not a measurement I would defend;
  it is recorded because publishing more shadow homes has a documented over-pin
  hazard (the bt18 small-heap OOM `collect_live_oop_homes` warns about) and
  somebody should re-take it on a quiet host.
* Two unit tests pin it and both DISCRIMINATE — with the kill switch the
  publish assertion fails with `homes=[]`, while the non-moving control passes
  in both arms, so neither can pass vacuously.


## Follow-up 2026-08-27: `UNPUBLISHED_FRAME_OOP` was refusing on DEAD slots

The fork §"Follow-up 2026-08-26 (second)" left open is closed, and it went the
second way: the dataflow, the oop map and the shadow push all agreed with each
other. Only the band scan objected, and it was objecting to words nothing would
ever read.

### The measurement that decided it

`report_unpublished_band_words` now also prints the frame's `sp_id` and whether
the ACTIVE safepoint's oop map names the slot. On one
`TestKillProcessWhileWriting` run, **all 74** unpublished words report
`in_map=false` — not one is a slot the map calls live:

```
68  region=java-local           in_map=Some(false)
 4  region=operand-spill        in_map=Some(false)
 2  region=reserved-locals-tail in_map=Some(false)
```

and the dominant method resolves completely:

```
MVStore.closeStore:(ZI)V off=72/64/56/48 region=java-local
  value=0x2004f4c5c88 (the SAME object in all four) published=8
  sp_id=Some(174) in_map=Some(false) live_hi=Some(144)
```

Offsets 48/56/64/72 are locals 5..8. `closeStore`'s `LocalVariableTable` scopes
slot 5 (`map`) to bci **149..170**, so at `sp_id=174` it is already out of
scope, and 6..8 are javac's loop/`finally` copies of it — the method's
`astore 6/7/8` all sit at bci 370/404/424, in the duplicated `finally` tails.
Four slots, one dead object, one refusal per collection.

### The repair, and how narrow it is

The band scan cannot tell live from dead on its own, so it demanded that every
movable word in a java-local or operand-spill slot be published on the shadow
stack. For the regions the abstract interpreter MODELS that is stricter than the
collector itself needs: the safepoint's map already states which of those slots
hold live references at that pc.

`band_slot_is_verifiable_with_map` now treats a word in a modelled region that
the active map does not name as dead. `region_is_dataflow_modelled` admits only
java locals and operand spill; **LICM hoist slots, scalar-replacement fields, the
reserved-locals tail and the register images are untouched**, because covering
exactly what the map cannot describe is the whole reason this scan exists (see
`frame_band_scan_rejects_a_relocatable_word_the_shadow_stack_never_published`).
And `None` — no sp-id, or no map for it — keeps every word verifiable, which is
the fail-closed direction. `CRATONVM_GC_NO_BAND_MAP_LIVENESS=1` is the A/B.

### Measured, one binary, arms interleaved

| class | arm | `unpub` | compacting | of cycles | rc |
|---|---|---:|---:|---:|---|
| `TestKillProcessWhileWriting` | on | **4** | **46** | 51 | 0 |
| | off | 24 | 28 | 52 | 0 |
| | on | **3** | **45** | 51 | 0 |
| | off | 22 | 29 | 51 | 0 |
| | on | **1** | **48** | 51 | 0 |
| | off | 25 | 25 | 51 | 0 |

`unpub` 1–4 against 22–25, no overlap. Compacting collections go from **49–57 %
to 88–94 %** of all cycles. Corruption canary — this change lets collections
RELOCATE that previously refused, so it is the measurement that had to be taken
— `org.h2.test.db.TestMultiThread` ×4, all `rc=0`, no SIGSEGV, no
`ClassCastException`, with 62–66 k objects actually relocated in three of them.

### What it does NOT fix, and what it costs

`TestMVStoreTool` still fails with `oom=4`: it OOMs after only 7–14 collections,
so it dies before a higher compaction rate can help it. Its `unpub` is already
1–4 in both arms. That class needs its own answer.

And the cost is real and worth stating: for java locals and operand spill this
scan **was** a backstop against a wrong oop map — refusing the moving cycle
meant a wrong map could not corrupt anything, because nothing moved. That
defence is now gone for those two regions, deliberately, because it was
suppressing ~half of all compaction to hedge against a map defect nobody has
demonstrated. ~~The right instrument for that hedge is the map-completeness
oracle already listed under §"Still open".~~ **WITHDRAWN 2026-08-27** — it was
re-pointed at `fully_shadow_covered` and run, and it cannot carry the load:
`TestMultiThread` PASSES with 1.1 M never-mapped words, 670 k of them in frames
asserting shadow coverage. The counter is dominated by its own declared false
positive and by the dead slots this section is about. See §"Follow-up 2026-08-27
(second)". The relaxation stands on the `closeStore` scope proof and the
corruption canary; a backstop that can tell dead from live without the map does
not exist yet.


## Follow-up 2026-08-27 (second): the oracle is NOT the backstop I said it was

§"Follow-up 2026-08-27" gave up the band scan's role as a check on a wrong oop
map for java locals and operand spill, and said "the direct instrument for that
hedge is the map-completeness oracle already listed under §Still open". **That
claim is wrong, and this section withdraws it.** The oracle was fixed to ask the
right question, then run, and it cannot carry the load.

### It was asking a question whose guard is almost never true

`while_covered` keyed on `fully_oop_covered` — the FRAME-SLOT notion, which a
direct JIT→JIT call taking a reference argument can never satisfy (439 of 449
recorded coverage failures were that one shape, §"Follow-up 2026-08-24" §2). So
`while_covered=0` had been reporting "never asked", not "never wrong". The moving
path spends `fully_shadow_covered`.

That is now counted separately, and — the part that makes either number readable
— **beside its denominator**, per frame INSPECTED:

```
[cratonvm] oop-map audit: … never_mapped=N
  (while_covered=A of B claiming; while_shadow_covered=C of D claiming) …
```

`verify_active_coverage_into` refutes on either counter, so the pre-suppression
gate can fire at all. Both are behind `CRATONVM_DBG_VERIFY_OOP_MAPS`.

### Measured — and the control is what settles it

| class | rc | `never_mapped` / words | `while_shadow_covered` / claiming | distinct sites |
|---|---|---|---|---|
| `TestKillProcessWhileWriting` | 1 | 15 232 / 247 672 | 15 228 / 16 926 | 21 |
| `TestMVStoreTool` | 1 | 844 / 16 190 | 654 / 714 | 52 |
| **`TestMultiThread`** | **0 PASS** | **1 108 464 / 21 770 329** | **670 474 / 972 492** | 166 |

Read the third row. `TestMultiThread` **passes**, under a moving collector, with
**1.1 million** never-mapped words and 670 k of them in frames asserting
`fully_shadow_covered`. If those were genuine live references no map names, that
workload would corrupt. It does not.

So the counter is dominated by the false positive its own doc declares —
*"a primitive `i64` whose bits land on a live object header is counted"* — and by
the dead stale slots §"Follow-up 2026-08-27" characterised. At **5–6 % of every
in-band word on every workload measured**, it cannot separate a real coverage
gap from either. The doc's own summary was right and should have been read
harder: *"a non-zero `never_mapped` is a lead, and a ZERO is the strong
result."* There is no zero to be had here.

### Where that leaves the relaxation

Standing, on its own evidence rather than on this oracle: the `closeStore`
`LocalVariableTable` proof (slot 5 scoped 149..170, read at bci 174) and the
argument that the active map is the liveness authority for the regions the
abstract interpreter models. The corruption canary is the empirical half —
`TestMultiThread` ×4 clean with 62–66 k objects relocated.

What is genuinely open is a backstop that can tell a dead slot from a live one
without the map. Two candidates, neither cheap: teach the oracle liveness (the
`LocalVariableTable` scopes, or a type-aware filter to kill the primitive false
positive), or have codegen clear reference locals as they go dead so no stale
movable word survives at all. Until one exists, this page should not claim to
have a check on the map.

### The site list is now actionable

`code=0x781daa08b000 rbp-0x58 operand-spill` could not be checked by anybody.
Sites now carry the METHOD and the BCI — which is exactly what settled
`closeStore` — plus both claim flags, and every never-mapped hit is recorded
rather than only the `fully_oop_covered` subset (which was 3 122 of 15 232, with
the other 12 110 sites discarded).


## Follow-up 2026-08-27 (third): `TestMVStoreTool`'s wall is 2 888 live bytes, and the instrument for it already existed

The class is not short of compaction. It is short of a *contiguous* 256 KB, and
the thing standing in the way is **2 888 bytes of live objects in 49 runs**.

### What the existing frag profile says, unedited

`TestMVStoreTool` on the 2026-08-27 `dev` tip, `--Xmx 1g`, default config —
11 compaction cycles, 1 817 474 objects relocated, and still `rc=1 oom=4`:

```
zgc: arena allocation failed  request=262160 used=1073545800 capacity=1073741824
     free_list_bytes=685889664 largest_free_block=131088 free_spans=62070

zgc frag: the CHEAPEST window that could serve this request — 2888 live bytes in
     49 run(s) are all that stand between 263216 free bytes spread over 266104
     bytes of contiguous arena.

zgc frag: wall occupant class=org/h2/mvstore/Page$PageReference count=17 bytes=1120
zgc frag: wall occupant class=org/h2/mvstore/Page$NonLeaf       count=2  bytes=320
zgc frag: wall occupant class=java/lang/Integer                 count=13 bytes=312
zgc frag: wall occupant class=java/lang/Object                  count=2  bytes=144
```

**34 named objects, 1.1 % occupancy, holding a 260 KB window hostage.** None of
this needed a new instrument: `Arena::frag_profile` has been computing the
cheapest window and walking its walls all along.

### The causal chain, and the control that closes it

| | |
|---|---|
| TLAB chunks carve the arena into ≤ 512 KB pieces | `ZGC_TLAB_MAX_CHUNK` |
| one survivor per chunk caps every hole at chunk granularity | `largest_free_block` sits at 131 088 = 128 KiB + 16 |
| compaction runs and relocates 1.8 M objects — but by its own policy | `compaction_cycles=11` |
| …so it does not evacuate the window the failing request needs | 2 888 live bytes survive in it |
| the 256 KB request cannot be served, at 64 % free | `oom=4` |

And the control: **with `CRATONVM_ZGC_TLAB=0` the class PASSES** — `rc=0`,
`oom=0`, 4 342 943 objects relocated, 561 s against 103 s. No chunk carving, no
wall, no OOM. That is the same lever the 2026-08-11 Hibernate investigation
found (`sql.exec.SmokeTests` and `DefaultCatalogAndSchemaTest`, `65528`-byte
holes against a 65 552-byte `DFAState[8192]`), which makes this the second
workload family localised to it.

Progress since the page opened is real but insufficient for this class:
`largest_free_block` was **65 440** on 2026-08-21 and is **131 088** now — the
compaction work doubled the ceiling. The request is 262 160.

### What this asks for

Not more compaction — *targeted* compaction. The failure path already knows
which window is cheapest and which objects wall it; what it cannot do is act on
that. The natural shape is to let the next collection's compaction take the
window the last failure named as its target, since evacuation has to happen at a
safepoint and not inside an allocation failure. Nothing in this page's history
suggests the general-policy compactor will find those 49 runs on its own — it
relocated 1.8 M objects in this very run and left them.

### One correction landed with this

The failure line opened with *"this heap does not compact, so the bump cursor
never rewinds"*. The first clause stopped being true — the run printing it
reported 11 compaction cycles and 1.8 M relocated objects — and it is precisely
the sentence that sends a reader looking for a missing compactor rather than at
the `zgc frag:` lines directly below, which had already localised the failure to
those 2 888 bytes. The cursor half is still true and is kept; the message now
points at the frag lines.

### `TestCachedQueryResults`, for the record

Still the livelock: `rc=124` at the 1 500 s cap, **18 048** `OutOfMemoryError`
and 14 arena failures. Same fragmentation family, far past the point where a
single window would help.


## Follow-up 2026-08-28: the large-object end is never compacted

Targeted compaction was built, and the measurement it was built for says the
target cannot be reached. That is a better answer than the feature would have
been.

### What was built

`ZRelocationSet::select` ranks pages by garbage ratio and drops anything at or
above `max_live_occupancy` — the right question for "reclaim the most per byte
copied", the wrong one for "make one CONTIGUOUS hole of size N", since the pages
walling such a window are dense by construction. The allocation-failure path now
records `Arena::frag_profile`'s cheapest window beside the latch that already
asks for a collection, and the next relocation consumes it as a page-id range;
pages in it bypass both filters and sort first, because the evacuation budget is
a prefix rule and a target behind a full budget would be dropped silently.

Two tests, the first carrying its own control (the ranking must REFUSE the
90 %-live page without a target, then admit and prioritise it with one).

### And it engages zero times

`targeted_pages` reads **0** on `TestMVStoreTool`, `TestKillProcessWhileWriting`
and `TestMultiThread`. `CRATONVM_DBG_ZGC_TARGET=1` says why in one line:

```
[zgc-target] recorded window start=1072365328 end=1072627680 width=262352
             request=262160 used_low=1068498592 capacity=1073741824
             in_low_region=false
```

The window begins **3.9 MB above `used_low_for_compaction()`**. `logical_pages`
builds candidates over `base .. base+used_low` only, so nothing in the
relocation set can ever cover that range.

**`TestMVStoreTool`'s failing request is 262 160 bytes, which is above
`ZGC_LARGE_OBJECT_MIN` (65 536), so it is served from the large-object end — and
that end has no logical pages, no candidates, and `relocate_large_pages` is
`false` by default besides.** Large-object fragmentation is not something any
current mechanism can repair. That is the finding, and it supersedes "compaction
has no targeted mode" as the description of this class's failure.

### Why it ships opt-in

`CRATONVM_ZGC_TARGETED_COMPACTION=1`, default OFF. The machinery is correct and
tested, and it engaged zero times on every workload measured — shipping that as
a default is how a feature comes to look measured when it is not. It is kept
rather than reverted because it is the low-region half of a repair whose other
half does not exist yet, and because the diagnostic that proved the gap is worth
more than the code.

### What this asks for next

Making the large-object end relocatable — logical pages over the high region, or
a compaction pass that understands a bump-down region — is now the blocking
item, not target selection. Anyone picking it up should start by re-reading the
`in_low_region=false` line above rather than the frag-profile numbers: those are
correct and actionable, and there is currently nothing that can act on them.

## Follow-up 2026-08-29: both ends are compacted now, and the floor that fed one end to the other

Four defects, in the order a reader should take them: **the slide was losing
every byte it emptied whenever the cursor could not follow it down**, the
large-object end had no compactor at all, the TLAB refill floor was spending
that end's reserve on churn, and the instrument meant to catch small objects
leaking into that end was armed on three sites that could not fire it.

### 1. A correction to §"Follow-up 2026-08-28" before anything else

That section closed with *"the LARGE-OBJECT end is never compacted, and that is
the whole of `TestMVStoreTool`"*, on a reading where the failing window began
3.9 MB above `used_low_for_compaction()` (`in_low_region=false`).

The first clause was right and is now fixed. **The second does not hold on the
2026-08-29 tip.** Same class, same `--Xmx 1g`, same 262 160-byte request:

```text
[zgc-target] recorded window start=234314432 end=234586208 width=271776
             request=262160 used_low=1069022960 capacity=1073741824
             in_low_region=true
```

`in_low_region=**true**`. The window placement varies run to run, so a single
observation of it could never have carried "and that is the whole of" — which
is the methodological point this page keeps re-learning, and the reason the
sentence is corrected here rather than quietly dropped.

What the same run does say, at the failing request, is where the wall really is:

```text
request=262160 used=1073545504 capacity=1073741824 free_list_bytes=815073056
largest_free_block=104896 free_spans=63934
high_cursor=1069219280 high_blocks=2 high_bytes=104984 high_max=104896
high_reserve_unclaimed=129695184
```

`high_cursor - cursor` is **196 320 bytes**: the two ends have met. The
large-object end holds 4.5 MB and is asked for 262 160; the reserve that exists
to stop exactly this is **129 695 184 bytes unclaimed**, i.e. it was never
claimed because the low end had already bumped through it. That is item 4
below, and items 2 and 3 are what let it get there.

> ### Before reading any arm of this section: `relocation_on_proven_jit=0` VOIDS a run
>
> Every repair in items 2 and 3 happens inside `relocate_stw`, and that function
> declines outright while a compiled frame is live whose oops the cycle could
> not prove rewritable. On a contended host that refusal fires on *every* cycle.
> Measured here, one run of `TestMVStoreTool` at `--Xmx 1g` on this Azure box at
> load 32:
>
> ```text
> compaction_cycles=0 objects_relocated=0
> relocation_skipped_jit=6 relocation_on_proven_jit=0
> ```
>
> That run OOM'd with `oom=4` and says **nothing** about either repair, because
> neither ran. The same class on the same binary at load 8–19 reports
> `relocation_on_proven_jit=4…15`.
>
> So: read `relocation_on_proven_jit` before reading `rc`. A run with a zero
> there is a measurement of the host, and this page has a long history of
> readings that turned out to be exactly that.
>
> **And check what the control arm actually turns off.** The first pass at the
> A/B here ran `neither` as
> `CRATONVM_ZGC_HIGH_COMPACTION=0 CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` — written
> before item 2 existed, so it left the LARGEST of the four repairs switched ON
> in the "pre-change" arm. Both arms then survived 400 s and the table said
> nothing. A control that is missing a switch is not a control, and the tell was
> that it agreed with the treatment too well.

### 2. The slide was LOSING what it emptied — the largest of the four

This is the one that explains why compaction never produced a big hole, and it
had been true since compaction went default-on on 2026-08-13.

**Reclaim was the cursor drop and nothing else.** `Arena::compact_low_to`
retracts the bump cursor to `new_cursor` and hands back `[new_cursor, cursor)`.
That is the whole answer only when the cursor can reach `dest` — the top of the
region the slide packed its survivors into. One survivor on an unselected dense
page above it pins `new_cursor` higher, and then the bytes the slide just
emptied are neither below the cursor nor on the free list.

**And no later sweep can find them.** The sweep free-lists dead objects by
walking the object-start REGISTRY, and the slide rebuilds that registry with the
survivors' NEW bases a few statements later — so the old ones stop existing as
far as every other subsystem is concerned. The space is invisible to the
allocator for the rest of the process.

The measurement that names it, `TestMVStoreTool` at `--Xmx 1g`:

```text
compaction_cycles=4 objects_relocated=885793
...
request=262160 free_list_bytes=706940280 largest_free_block=8184
span_hist=8:47715 16:40407 32:80 64:138 256:1 512:1 1K:116147 2K:81414 4K:46675
```

**885 793 objects relocated, and the largest free block is 8 184 bytes.** Read
the histogram: nothing above 4 KiB exists. A compactor that moves nearly a
million objects per run and leaves no span bigger than a page of text is not
compacting for the allocator's benefit at all — and the slide's own output is
2 MiB-granular and contiguous by construction, which is exactly the shape the
262 160-byte request needed.

The caller now names the offset spans it emptied and `compact_low_to` publishes
them. Two details are load-bearing:

* **They are zeroed first.** A slid-away survivor leaves its old bytes behind
  verbatim, including a valid-looking `ObjectHeader`; a conservative scanner
  that met one would resurrect a corpse. That is the same contract the span
  above `new_cursor` already had and the reason the sweep zeroes a dead
  object's header before free-listing it.
* **They are SCREENED against the live set at its post-slide addresses**, not
  argued for from the page partition. A selected page above `dest` is dead by
  construction — every survivor based on it was packed below — but an OBSTACLE
  (an object based on an unselected page whose tail straddles into a selected
  one) never moves, and neither does anything above the point where the slide
  gave up on an unsizable header. One pass, a prefix maximum and a binary
  search per page settle it. Same shape as the `live_ceiling` check beside it
  and for the same reason: an argument that the partition is exhaustive is an
  argument about the partition, not about what is in the span. It can only DROP
  a span, i.e. reclaim less.

**Engagement, measured on the merged tip** (`TestMVStoreTool`, `--Xmx 512m`,
two runs, `CRATONVM_GC_STATS=1`):

```text
compaction_cycles=16 objects_relocated=3049299 relocation_on_proven_jit=16
  vacated_spans=2579 vacated_bytes=5391509672
compaction_cycles=24 objects_relocated=3168136 relocation_on_proven_jit=24
  vacated_spans=4315 vacated_bytes=9023929152
```

**5.4 GB and 9.0 GB republished over a run, on a 512 MB heap** — ten to
eighteen heaps' worth, roughly 340 MB per cycle.

**Read that number for what it is.** It is the volume handed back as
PAGE-GRANULAR spans, and a selected page can be 100 % garbage, in which case the
sweep had already free-listed its objects one at a time. So `vacated_bytes` is
not all newly-recovered memory: it is the sum of (a) the space the slide's own
survivors vacated, which really was lost before — bounded by `objects_relocated`
times the mean object, so ~19 MB per cycle here — and (b) free space that
existed only as dust and now exists as 2 MiB blocks.

(b) is not a rounding error, it is the point. `largest_free_block=8184` with
707 MB free was the failure; a free list of the same bytes in page-sized pieces
is a different heap. The accounting stays exact either way —
`compact_low_to` clears the low list and rebuilds it from the kept blocks plus
the spans, and a kept block inside a span is dropped rather than kept beside
it — so nothing is counted or handed out twice.

**The A/B, one binary, arms interleaved, and it does NOT say what a first
reading suggests** (`TestMVStoreTool`, `--Xmx 1g`, 500 s cap, two reps):

| arm | switches OFF | rc | secs | `oom` | `arena` | load |
|---|---|---:|---:|---:|---:|---:|
| `base` | — | 124 (cap) | 500 | **0** | 0 | 18.1 |
| `base` | — | 124 (cap) | 500 | **0** | 0 | 16.3 |
| `neither` | all three | 124 (cap) | 500 | **0** | 0 | 30.9 |
| `neither` | all three | 124 (cap) | 500 | **0** | 0 | 24.1 |
| `novac` | `PUBLISH_VACATED` only | 1 | **81** | **6** | 1 | 14.2 |
| `novac` | `PUBLISH_VACATED` only | 1 | **60** | **6** | 1 | 9.6 |

Read the third and fifth rows together. **`novac` fails 2/2 and `neither`,
which has that same switch off AND two more besides, passes 2/2.** So this is
not "the publication fixes the class". It is:

**THE SWITCHES ARE NOT INDEPENDENT — item 4 is only safe with item 2.** The
starved refill floor takes 8–64 KiB blocks off the free list when the bump is
out of headroom. With item 2 supplying page-granular spans back, that is
recycling. Without it, nothing replenishes the large end of the free list and
the floor grinds the last of it into TLAB chunks. The failure line from a
`novac` run says exactly that:

```text
request=9888 used=1073740088 capacity=1073741824
free_list_bytes=797496464 largest_free_block=8184
```

**A 9 888-byte request failing with 797 MB free** — not the 262 160-byte
large-object request this page opened on, a ten-kilobyte one. That arm reports
`vacated_spans=0 vacated_bytes=0`, which is what the switch is for.

**The page's own class, on the merged tip with all four repairs:**

```text
org.h2.test.store.TestKillProcessWhileWriting  rc=0  secs=403  oom=0  arena=0
  compaction_cycles=47 objects_relocated=336650
  relocation_skipped_jit=4 relocation_on_proven_jit=46
  zgc-high-compaction: cycles=16 declined=31 objects_relocated=24
                       bytes_copied=25166208
                       vacated_spans=996 vacated_bytes=2082517560
```

`rc=0`, and **46 of 47 collections compacted** — the number this page spent
2026-08-21 to 2026-08-26 getting off the floor, still there. 2.08 GB of vacated
span republished on the way. **2/2** (the second run `rc=0 secs=351 oom=0
arena=0`).

**The corruption canary**, which is the measurement that HAD to be taken because
item 2 hands the vacated span back to the allocator and so removes the safety
net `TestMultiThread`'s own open stale-holder defect was standing on:

```text
org.h2.test.db.TestMultiThread  rc=0  secs=235  oom=0  arena=0
  compaction_cycles=1 objects_relocated=3736 relocation_on_proven_jit=1
  zgc-high-compaction: cycles=1 declined=0 vacated_spans=6 vacated_bytes=11904888
```

`rc=0`, zero `names an address the ZGC slide VACATED` reports, zero
`ClassCastException`/`NoSuchMethodError`. **2/2** (the second `rc=0 secs=217
oom=0 arena=0`). Two runs are not a rate — that class is flaky by its own
page's account — but they are the runs that had to come back clean before this
shipped, and they did.

**Closed in code, not only in prose.** `recycled_chunk_size` now takes the
publication's state and the starved floor is inert without it
(`starved_recycle_permitted`), with a test carrying these numbers. Both default
ON, so the shipped configuration is byte-for-byte the one measured above; this
only affects somebody turning the publication off to bisect, and it stops that
bisect from being worse than either endpoint.

**And what the table does NOT establish**: `neither` is the pre-2026-08-29
behaviour and it passed 2/2 here, so these runs do not show a rate improvement
over it. They cannot: the same class on the same host, same day, on a
pre-change binary, failed at 58 s, 63 s, 65 s, 67 s, 76 s and 127 s and passed
past 400 s twice. **`TestMVStoreTool` is flaky on this host today**, and a
two-rep table cannot separate a flaky pass from a fix. What this section
therefore claims is what it measured: the mechanisms ENGAGE (the census above),
one switch turns a passing configuration into a failing one 2/2, and the defect
each repair names is real in the code. A rate claim needs a quiet host and
ten reps an arm, and it is not made here.

**One cost is known and deliberately not optimised yet.** The span is zeroed
with a single `fill(0)`, so the pass memsets roughly `live / max_live_occupancy`
bytes — about four times what the slide itself copies — inside the pause. The
cheaper form is the one the sweep already uses (`zgc_sweep_header_zero`): only
the `ObjectHeader` of each address the slide vacated needs clearing, because
"the body is only reachable through that header", and the caller has exactly
that list in `pairs`. It is not done that way here because the safe version is
unconditional and the cheap version depends on getting "which `from` addresses
are above `dest` and therefore not already overwritten by a memmove" right —
and getting that wrong zeroes a live object rather than costing a memset. Worth
doing, worth doing with its own test.

`CRATONVM_ZGC_PUBLISH_VACATED=0` is the same-binary bisect, and it is per heap
so the A/B runs inside one test binary — which is what
`a_slide_that_cannot_drop_the_cursor_still_frees_what_it_emptied` does: the
control arm must reclaim **zero** to the free list, or the test proves nothing
about the other one.

> **This removes a safety net that a DIFFERENT open defect was standing on, and
> that has to be said out loud.**
> `bug-h2-testmultithread-mvstore-writer-object-identity-20260816.md` is open on
> a holder that keeps naming an address the slide vacated
> (`receiver names an address the ZGC slide VACATED … target_still_live=true`).
> While the vacated span was merely leaked, that stale read met a zeroed corpse
> and surfaced as `java.lang.Object`. Now the span goes back to the allocator,
> so the same stale read meets whatever was allocated over it — a different
> FACE of the same defect, possibly sooner and possibly louder.
>
> That is not a reason to keep leaking the memory; the leak is what produced
> the `OutOfMemoryError` this whole page is about. It IS a reason to run
> `TestMultiThread` as the canary on every arm here and to expect its signature
> to change, and it is why the canary is in the measurement table above rather
> than in a follow-up.

### 3. The large-object end is relocatable — `ZgcRealHeap::compact_high_region`

Survivors are packed against `capacity` in DESCENDING address order (the mirror
of the low slide's ascending walk, and just as load-bearing), and
`Arena::compact_high_to` publishes the emptied spans as merged blocks on the
high free list. The pairs join the low slide's, so the rewrite pass, the
registry rebuild and the returned `PointerMap` cover both ends with one
mechanism rather than two. A pinned large object stays put and the walk
continues below it; the free space on its far side is preserved rather than
dropped, which is the memory loss `Arena::compact_low_to`'s own history records
having made once at the other end.

**It engages, and here is what it does per cycle** (`CRATONVM_DBG_ZGC_HIGH=1`,
`TestMVStoreTool`, `--Xmx 1g`):

```text
[zgc-high] region=4260384 moved=224 copied=2108760 pinned=0 spans=1
           before=(blocks=198 bytes=1037480 largest=10880)
           after=(blocks=1  bytes=1037480 largest=1037480)
[zgc-high] region=4522544 moved=308 copied=1459008 pinned=0 spans=1
           before=(blocks=110 bytes=900688  largest=241960)
           after=(blocks=1  bytes=900688  largest=900688)
```

**99–198 blocks into one, every cycle.** The largest servable large-object
block goes from 10 880 to 1 037 480 bytes in the first line — a request of
262 160 is unservable before and servable after. Run totals, three runs across two binaries:
`cycles=12 declined=3 objects_relocated=2737 bytes_copied=8800848`,
`cycles=8 declined=8 objects_relocated=9895 bytes_copied=25959272`,
`cycles=4 declined=20 objects_relocated=3259 bytes_copied=9003776`.

Read `declined` beside `cycles`. It is the pass looking at the high end and
finding under one large object's worth of contiguity to gain, which is the
right answer and a different fact from not running — and it is why the
`[GC] zgc-high-compaction:` line prints both.

The engagement counters are on their own `[GC] zgc-high-compaction:` line, and
`declined` is beside `cycles` deliberately: `cycles=0 declined=0` means the pass
never ran, `cycles=0 declined=812` means it ran and found nothing worth doing,
and only the first is a defect. The feature this supersedes
(`CRATONVM_ZGC_TARGETED_COMPACTION`) shipped reading zero for a week and the
only reason anyone found out is that it carried a counter.

`CRATONVM_ZGC_HIGH_COMPACTION=0` is the same-binary bisect.

### 4. A starved bump must recycle a short chunk, not eat the reserve

`recycled_chunk_size`'s floor was `want / 8` unconditionally — 64 KiB against a
512 KiB chunk, which is `ZTlabConfig::max_tlab_alloc` and a good floor while
refusing costs one clean full-size bump.

`TestMVStoreTool` fails one notch below it. From the same failure line:

```text
span_hist=8:53403 16:67817 32:1 64:1 512:1 1K:51960 2K:11501 4K:14
          8K:63860 16K:40 32K:18 64K:1
```

815 MB free, the largest LOW block **104 896** bytes — and that one is the high
end's; the largest low block is under 64 KiB, one notch under the floor, with
63 860 spans of 8–16 KiB sitting unusable beneath it. So every TLAB refill in
the process bumped. The cursor reached capacity. `Arena::alloc`'s last-resort
arm then spent the whole 128 MiB large-object reserve on TLAB churn — that is
the `high_reserve_unclaimed=129695184` above — and the 262 160-byte request the
reserve exists to serve had nowhere left to go.

"Is a short chunk worth taking?" has two answers and they turn on what refusing
costs. With bump headroom, refusing buys a clean full-size bump and the short
chunk is pure churn. With none, refusing does not buy a full chunk: it walks
straight down to the arm that eats the reserve. The chunk is taken either way;
the only question is out of WHICH space. Below the preferred floor the decision
now consults `Arena::low_bump_headroom`, with a hard `want / 64` floor so dust
is still refused and `need` still bounding both regimes.

`CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` is the same-binary bisect.
`CRATONVM_ZGC_TLAB=0` is NOT a substitute for it — that turns the whole
thread-local buffer off and changes the allocation path, the free-list shape and
the run time (561 s against 103 s), so an arm that differs by it differs by far
more than this one decision.

### 5. The region tripwire was armed on three sites that could not fire it

§"Still open" has carried *"the two small objects in the large-object region"*
since this page was filed, with the note that the tripwire *"fired zero times
across a full failing run"* and the correct warning that this makes it *"an
untriggered instrument, not evidence"*.

It could not have fired. `note_region_leak` sat on `Arena::alloc`'s three LOW
free-list exits, and a low tier only ever returns an offset below `high_cursor`,
so `is_high` is false at all three **by construction**. The one exit that can
return a high offset — `alloc`'s last-resort `high_fit`, which spends the
large-object end's own free list rather than raise `OutOfMemoryError` — had no
tripwire at all. It is armed now, as `high-free-list-last-resort`, with a test
that exhausts the low end and watches the counter move.

Still not a refusal, and it should not become one: refusing would trade a
fragmentation hazard for an `OutOfMemoryError` on a heap that has bytes. What
changed the balance is that the damage is no longer PERMANENT — item 2 packs a
small survivor up there against the top with everything else, so it stops
walling the region at the next relocating cycle.

(Item numbering: item 2 is the vacated-span publication; the high-end compactor
is item 3. The sentence above means both, because a small object up there is
only re-packed by the high-end pass.)

### 6. `CRATONVM_ZGC_TARGETED_COMPACTION` records a window nothing consumes

Measured with the flag ON, and it is a finding about that feature rather than
about this class: `targeted_pages=0` even though the window it recorded was
`in_low_region=true` and therefore reachable by `logical_pages`.

The `[zgc-target] recorded` line appears once in the whole run, and
`[zgc-target] consumed` never appears. `record_compaction_target` fires on the
allocation failure, and on this class the allocation failure is what **ends the
run** — the last `[zgc-high]` line is one line above the first
`arena allocation failed`. A target recorded at the failure that raises
`OutOfMemoryError` has no next cycle to consume it.

So the feature's own engagement number was never going to be non-zero on this
family, for a reason unrelated to the one §"Follow-up 2026-08-28" gave. It stays
default-OFF.

### 7. The oop-map backstop exists now, and it is the CLASS FILE's verifier

§"Follow-up 2026-08-27 (second)" withdrew the map-completeness oracle as a
backstop and closed with: *"a backstop that can tell a dead slot from a live one
without the map does not exist yet."*

**Asking the JIT's own model would have been circular** — the oop map IS its
output, so "the map does not name this slot" and "the model says it is not live
here" are the same sentence. `classloading::type_maps` is a different oracle
with a different provenance: what `bytecode_verifier` retains from the JVMS
§4.10.1 StackMapTable walk, i.e. **javac's claim** about the type of every local
at every instruction start. It answers both of the oracle's declared false
positives at once — a primitive local is not an oop there, and an out-of-scope
local is `Top`.

Every never-mapped word in a java local is now classified against it. Measured,
`CRATONVM_DBG_VERIFY_OOP_MAPS=1`, on the 2026-08-29 tip:

| class | `verifier_oop` | `verifier_not_oop` | `verifier_unknown` | name index |
|---|---:|---:|---:|---|
| `TestMVStoreTool` | **0** | 40 | 198 | 762 names, 0 collisions |
| `TestMultiThread` (PASSES) | **262** | **847 347** | 1 284 994 | 1 215 names, 0 collisions |

Read the second row against what this page recorded on 2026-08-27:
**1 108 464 never-mapped words on that class, 670 474 of them in frames
asserting shadow coverage, on a run that PASSES.** The verifier refutes
**847 347** of them outright and leaves **262** corroborated. That is the
difference between a counter and a lead: 262 sites, each with a method and a
bci, is a list somebody can work; a million is a number nobody can act on, which
is exactly why that section withdrew it.

`TestMVStoreTool`'s **zero** is the strong result the oracle's own doc says to
look for.

Three things this does NOT claim:

* **`verifier_unknown` is large and that is honest, not a rounding error.** It
  is every refusal to answer — an INLINED frame (a spliced callee's locals share
  the region and the bci belongs to the callee's bytecode, so the outer map read
  at that pc is a different method's types), a slot outside the java locals
  (operand spill is indexed by runtime depth, which the frame does not carry),
  a pc with no row, a name the index cannot resolve. Counting them separately is
  the point: a backstop whose "no gap" silently includes "could not look" is the
  vacuous green this page has spent a week avoiding.
* **262 corroborated hits on a PASSING class is a lead, not a verdict.** They
  may be live references the conservative band scan is still catching, or
  genuine gaps that happen not to bite. What changed is that there are 262 of
  them to look at rather than a million.
* The index is name-keyed and cannot tell two loaders' versions of one class
  apart. `0 collisions` on both runs is why the numbers above are trustworthy;
  a non-zero there discounts the run.

### 8. The cross-thread coverage handshake DOES decide something — on a
### many-threaded class

§"Still open" carried: *"The cross-thread coverage handshake decides nothing
yet. It is built, default-ON, and `xt_cov=(accepted=0 refused=0 deposits=0)` on
this workload — no peer was ever in compiled code at a collection here. It
removes a blanket refusal that a genuinely many-threaded workload would hit;
that claim is untested because this class does not produce the condition."*

Run on a class that DOES produce the condition — `org.h2.test.db.TestMultiThread`,
`CRATONVM_DBG_JIT_ROOTSCAN=1`, 2026-08-29 tip:

```text
frame_cov=(no_slot=0 misaligned=0 no_map=6 incomplete=0 ok=372)
xt_cov=(accepted=1 refused=8 deposits=26)
```

**26 deposits, 1 accepted, 8 refused.** The handshake is consulted, and it both
admits and refuses. The residual was never "it is broken", it was "nobody has
run it on a workload that reaches it"; this is that run. `rc=0` on the same run.

`no_map=6 of 378` on the same line, and `incomplete=0` — that class is not where
the §"Follow-up 2026-08-27" `incomplete` residual lives.

### 9. `incomplete=5` does not reproduce — and the handshake names what does

The residual read *"`TestCachedQueryResults` shows `incomplete=5` — the first
time anywhere that a map refuses on its OWN claim rather than being
unlocatable."* Re-run on the 2026-08-29 tip with `CRATONVM_DBG_JIT_ROOTSCAN=1`:

```text
frame_cov=(no_slot=0 misaligned=0 no_map=77 incomplete=0 ok=6962)
xt_cov=(accepted=0 refused=1730 deposits=469)
```

**`incomplete=0`.** It does not reproduce.

And the line beside it is the real finding for that class: **the cross-thread
handshake refuses 1 730 times and accepts 0**, against `accepted=1 refused=8`
on `TestMultiThread`, which passes. A cycle the handshake refuses does not
relocate at all, so none of the four repairs above runs on it. That is where
`TestCachedQueryResults` should be attacked, and it is on its own page now.

And the eight-bucket census the flag was there for, run on the same class
(`CRATONVM_DBG_OOPCOV=1`):

```text
scauses(gate=0 desync=0 marks=83 scratch=0 locals64=0
        dataflow=151 nopush=0 inline_scope=5)
```

**`dataflow=151` and `marks=83` are the whole of it.** The forward "must be
oop" dataflow never reaching a bytecode pc (so there is no local oop mask to
publish from), and the operand-stack oop marks not being exact at the safepoint
(a revived dead-code merge reconstructing the stack at a nonzero depth). Not the
gate, not a missing push, not a slot count. Two shapes to attack, both in the
compiler rather than the collector.

### 10. …and the census below is still the instrument for it

§"Still open" carried *"`TestCachedQueryResults` shows `incomplete=5` — the
first time anywhere that a map refuses on its OWN claim rather than being
unlocatable. Different obligation from `no_map`, never investigated."*

The obligation is `OopMapEntry::moving_young_coverage_complete`: the JIT itself
recorded, at compile time, that the shadow-stack publication for that safepoint
was not complete enough for relocation. That is the HONEST refusal — the
mechanism working, not failing — and **the reason is already censused, in eight
buckets**, by `x64::safepoint::shadow_incomplete_cause` (gate off, mark-vector
desync, inexact operand marks, an oop in a scratch or XMM slot, more than 64
locals, the local-oop dataflow never reaching the pc, no push emitted, an
unmapped inline scope). `CRATONVM_DBG_OOPCOV=1` prints it per method, beside
the exact `bytecode_pc`s whose shadow claim is false.

So the residual was "nobody has run the flag", not "there is no way to ask".
Recorded here so the next reader spends a run rather than an instrument.

## What this page no longer tracks, and where it went

Every row that was open on 2026-08-28 is accounted for here. Three were the
defects §"Follow-up 2026-08-29" fixed; four were never this defect and now have
their own pages; one was closed by a measurement rather than by anybody fixing
it.

### Fixed on 2026-08-29 — see §"Follow-up 2026-08-29"

* **The LARGE-OBJECT end is never compacted.** `ZgcRealHeap::compact_high_region`
  packs that end against `capacity` and merges its holes; measured at 99–198
  free blocks into ONE, per cycle, on `TestMVStoreTool`.
  `CRATONVM_ZGC_HIGH_COMPACTION=0` reverts it.
* **The two small objects in the large-object region.** The tripwire that read
  zero could not have fired: it was armed on three exits that return low
  offsets by construction, and not on the one exit that can return a high one.
  Armed, with a test. The damage is also no longer permanent, because the
  high-end compactor packs such an object against the top with everything else.
* **…and the two nobody had listed**, both found while measuring the two above:
  the slide was discarding every byte it emptied whenever the cursor could not
  follow it down (`CRATONVM_ZGC_PUBLISH_VACATED=0`), and the TLAB refill floor
  was spending the large-object reserve on churn one notch above what the free
  list could serve (`CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0`).

### Closed by measurement, not by a fix

* **There is no working backstop against a wrong oop map for java locals and
  operand spill.** There is one now, and it is not the map-completeness oracle
  this page withdrew on 2026-08-27 — that counter reads 5–6 % of every in-band
  word on every workload and cannot be one. The oracle now classifies each
  never-mapped word against the CLASS FILE's own verifier type maps
  (`classloading::type_maps`, retained from the JVMS §4.10.1 StackMapTable
  walk), which is an independent oracle and answers both declared false
  positives at once: a primitive local is not an oop there, and an out-of-scope
  local is `Top`. `verifier_oop` is the actionable number, `verifier_not_oop`
  the subtracted false positives, and `verifier_unknown` every refusal to
  answer — an inlined frame, a non-local slot, a pc with no row, a name the
  index cannot resolve — counted rather than folded in, because a backstop
  whose "no gap" silently includes "could not look" is the vacuous green this
  page has been avoiding all along. Behind `CRATONVM_DBG_VERIFY_OOP_MAPS`.
  **Measured**: `verifier_oop=0` on `TestMVStoreTool`, and on `TestMultiThread`
  — where this page recorded 1 108 464 never-mapped words — the verifier
  refutes **847 347** and leaves **262**. See §"Follow-up 2026-08-29" §7.
* **The cross-thread coverage handshake decided nothing** — because nothing
  had run it on a workload that reaches the condition. Run on
  `org.h2.test.db.TestMultiThread`: `xt_cov=(accepted=1 refused=8
  deposits=26)`, `rc=0`. It is consulted, and it both admits and refuses. See
  §"Follow-up 2026-08-29" §8.
* **`CRATONVM_ZGC_TARGETED_COMPACTION` engages zero times**, and §"Follow-up
  2026-08-29" §6 says why in a way §"Follow-up 2026-08-28" could not: the
  target is recorded on the allocation failure, and on this family the
  allocation failure is what ends the run. It stays default-OFF, superseded by
  the two compactors rather than fixed.

### Never this defect — split out with their own pages

| row | page |
|---|---|
| `TestOpenClose`: `Exception in thread "main" java/lang/Object`, no frames — and on 2026-08-29 an MVStore-writer OOM alongside it, on a run where the slide fired ONCE in 17 collections | `fixed-suite-bugs/h2-suite-bugs/bug-h2-testopenclose-throwable-is-java-lang-object-FIXED-20260830.md` |
| `TestMVStoreCachePerformance`: `NoSuchMethodError` for `Page.isPersistent()` against a `Page$PageReference` RECEIVER — **PASSES 1/1 on 2026-08-29** (`rc=0`, and the slide never ran, so the run does not exercise the path it would live on) | `bug-h2-testmvstorecacheperformance-pagereference-receiver-20260829.md` |
| `-XX:+UseG1GC` fails `TestKillProcessWhileWriting` | `bug-h2-testkillprocesswhilewriting-g1-oom-20260829.md` |
| the same fragmentation symptom in Spring Framework and Hibernate, never censused | `../gc/zgc-arena-fragmentation-occurrences-to-reverify-20260829.md` |
| `TestCachedQueryResults`: still a LIVELOCK, `oom=2990` in 900 s on the fixed tip against 18 048 in 1 500 s before — a 3.6× lower rate and the same outcome | `bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829.md` |

The last row is the one that matters most, and it is the reason this page
retires with a caveat rather than a clean sweep: **`TestCachedQueryResults` is
not fixed.** It throws thousands of `OutOfMemoryError` and keeps running, which
is a different shape from every other class here — those failed once and died.
Something catches and retries, and that is not a collector question. A rate
improvement on a livelock is not a fix and this page does not claim one.

The others were already labelled "not this defect" here, with a measurement
behind the label; splitting them out is what stops this page's Status line from
being read as a verdict on them. The G1 one in particular has a five-arm
interleaved A/B behind it and is identical before and after every repair on
this page.
