# `SmokeTests.testQueryConcurrency` — stale-receiver crash — RETIRED

**Status: RETIRED (2026-08-04).** This page was filed the same day as a "new
witness of the already-tracked DoHead **Layer 1** register-invisible-root
mechanism". **That attribution is refuted by the page's own log** (§1). The
chain it recorded is a different one — a blocked main thread resuming on
addresses in the semispace a MOVING collection had evacuated — and that chain
is now instrumented at the cycle that causes it rather than at whatever
bytecode dereferences it minutes later (§4). One concrete root-coverage defect
on exactly that path is fixed (§3).

What the page contributed and what is left of it:

* its mechanism section — **wrong**, and worth keeping only as the worked
  example in §1 of how to falsify one;
* its scope claim ("the same mechanism reaches Hibernate/JUnit internals") —
  **kept**, and now carried by an executable probe instead of prose (§5);
* the generic `ClassId(0)` / stale-address family it belongs to — **still
  open**, on its own page:
  `../../../known-issues/h2/bug-h2-classid0-stale-address-family.md`. That is
  where a future occurrence goes. Do not re-file a per-suite witness page for
  it; file the evidence there.

The 120 s throughput timeout on this same test method is a separate, still-open
issue: `../../../known-issues/hibernate/smoketests-concurrent-println-timeout-20260723.md`.
Every run in §2 below hit it, on both binaries — that is the reproducible
failure of this test, and it is not this page's subject.

---

## 1. The original attribution, and why it is wrong

The page claimed a non-moving young sweep freeing an object whose only live
reference sat in a register invisible to the conservative scan — "Layer 1" of
`../tomcat/dohead-jit-heap-corruption-register-invisibility-FIXED.md`. Three
lines in the same log rule that out:

```
ERROR cratonvm::gc::guard: receiver points into RECLAIMED memory …
  obj="0x16cc325d598"  site="blocked frame slot (block entry)"
  location=young TO-space (the inactive semispace)   span="0x16cc3050000+0x0"
  target_class=tid=0 frame#14
      org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute pc=73 local[7]
```

* **`location`.** `GenerationalHeap::reclaimed_hole_at` distinguishes its
  answers. A non-moving sweep's victim reads back as `young from-space FREE
  BLOCK (reclaimed)`. This one reads `young TO-space (the inactive
  semispace)` — the arena a MOVING cycle evacuates, swaps out and zeroes.
  A live object is never there; a *pre-copy* address always is.
* **The young reclamation ring is silent.** `young_freed_lookup` is
  unconditional (it needs no flag armed in advance) and records every span the
  non-moving sweep zeroes. `grep` of the raw log: zero hits for
  "young span the non-moving sweep", "RECLAIMED BY THE YOUNG SWEEP", and the
  old-gen equivalent. If the non-moving sweep had freed this object, that ring
  would name it.
* **`site` and `target_class`.** Not a register — a numbered INTERPRETER frame
  slot of `tid=0`, reported by the audit's live-locals mask, i.e. a slot the
  collector was required to keep. Eight such slots were reported at once,
  across frames #1, #2, #14, #18, #27, #36 and #45 — a whole stack going stale
  in one step, which is the signature of one missed fixup application, not of
  N independent register-invisible roots.

`0x16cc325d598` is the same address the downstream `NoSuchMethodError` chain
dereferences, so the two halves of the log are one event end to end:
`EngineExecutionOrchestrator.execute` local[7] (an `Iterator`) →
`invokevirtual` sees an all-zero header → falls back to the CP class
`java/util/Iterator` → the registered `native_snapshot_itr_has_next` reads
field 0 of the dead object → `gen_heap::get_field` correctly drops the
out-of-bounds read.

**Mechanism, restated:** a thread inside a blocked region is excluded from
every stop-the-world census, so a moving collection completes without it. The
only things that keep its frames usable are the initiator-side fold
(`ThreadRegistry::fold_pointer_map_into_blocked`) and the wake-side apply
(`check_post_block_gc`), both driven by what its `deposit_root_snapshot`
published. `testQueryConcurrency` parks the JUnit main thread in
`ExecutorService.invokeAll` for 50×400 tasks under a ~45-frame stack while five
workers churn young — which is why this test, and not another, is where it
showed.

## 2. Reproduction: 41 runs, both binaries, neither reproduces

The page said "not yet confirmed deterministic — a single occurrence from one
run" and asked for 3-5 interleaved re-runs. Done, and then some.

| binary | runs | corruption | `@@RESULT` |
|---|---:|---:|---|
| dev tip `41349f661` and this branch's builds on it | 35 | **0** | 120 s timeout, `ok=16 failed=1` under load |
| the witness build itself (`a43a74ded`, `CratonVM-hib-local-0712-v3`) | 6 | **0** | same |
| HotSpot JDK 25 control | 1 | — | `ok=17` in 15.3 s |

(Logs under `apps/hib-suite-runner/runs/`: `base-smoke1` 1, `base-par` 8,
`dbg-par` 12, `det-par` 12, `ab/dev-*` 6, `ab/wit-*` 6, `fix-solo` 1.)

`--Xmx 1500m`, real JDK, JIT on, the page's own command, 3-6 VMs concurrently
(the original run's load shape), arms interleaved in the same window. Counting
`Stale pointer detected`, `points into RECLAIMED memory` and `NoSuchMethodError`
separately; all zero on every run. A solo run passes `ok=17` in 120 s.

So the event is rarer than 1-in-6 on this box, **including on the exact binary
that produced it** — which also means it cannot be bisected. That is the reason
this page closes on an instrumented invariant rather than on an A/B.

## 3. Fixed: the operand-stack half of a frame's root scan was screened too strictly

`Frame::scan_local_objects` screens a local root with `is_heap_addr`
(alignment + arena containment) and says why in its own comment: the strict
`is_object_address` header probe rejects a young / mid-init object whose header
it cannot yet vouch for, and dropping such a root reclaims a still-referenced
object (the BouncyCastle `EC5Util.getCurve` local). `ValueStack::scan_object_refs`
was changed for the same reason — "the prior code dropped such roots
(use-after-free risk for newly allocated objects) — the regression this fixes" —
and separately validates the JNI long-as-jobject smuggle with `is_heap_addr`
because the strict probe rejects those too (the WildFly `jboss-modules` SEGV).

Every caller then re-screened *that scan's output* with `is_object_address`,
putting both fixes straight back on the operand-stack half. There are four
copies of the filter and all four had it:

* `deposit_root_snapshot_inner` (`vm/src/vm/vm_exec.rs`) — the BLOCKED path,
  where the snapshot is the collector's only view of the thread;
* `scan_frame_roots` + the inline copy in `update_root_snapshot`
  (`vm/src/runtime/interpreter/gc_and_alloc.rs`) — the safepoint path;
* the frozen-in-JIT peer scan in `stw_take_over_and_wait` (same file) — a peer
  that cannot speak for itself at all;
* `collect_roots` (`vm/src/memory/roots.rs`) — the GC INITIATOR's own frames,
  where a dropped root has no second chance.

Its justification comment — that `scan_object_refs` "treats pointer-shaped
`Long` bits as roots without heap validation" — had gone stale: that scan is
kind-gated (`KIND_LONG`/`KIND_DOUBLE` slots are never rooted through the object
branch) and heap-validates the smuggle branch. All four sites now use
`is_heap_addr`, matching the locals half.

Why a loose root is safe here, and a dropped one is not: both generations
already screen a root with an exact object-base oracle before writing anything
through it — `young_object_starts` membership in `forward_object`,
`walk_objects`-derived `walked_bases` in `old_gen_gc` — so the cost of a false
root is one cycle of over-retention, while the cost of a dropped one is a
use-after-free. The locals half has been feeding these same loose roots into
the same snapshots all along.

Cost of the extra retention, measured: `bench/BinTreesClassic` A/B/B/A
interleaved, 6 runs per arm, scored on per-process USER CPU TIME because this
box had other VMs and cargo jobs on it (wall clock swung 8.4 s to 36.2 s within
one arm and cannot resolve anything). d=16: B/A **1.029 mean, 0.975 min** —
noise. d=18: **1.055 mean, 1.035 min** — at most ~3 % on a loaded host, and the
min-of-6 is the robust half of that. Checksums exact on both arms and both
depths (14985902 / **68332206**, the HotSpot-verified values), which is also
the correctness half of this measurement: a root set that retained something it
should not would not change them, but a collector writing through a false root
would.

Pinned by `root_snapshot_screen_tests::operand_stack_roots_use_the_same_screen_as_locals`,
which asserts the discriminating precondition (`is_object_address` rejects the
address, `is_heap_addr` accepts it) before asserting that both slot kinds
survive the scan — so it cannot pass vacuously. Verified in both directions:
it passes on the fix and fails on the re-injected pre-fix screen with
`1 of 2 survived`.

## 4. Added: the invariant, checked in the cycle that breaks it

Every previous witness of this family named a *use* site — a
`NoSuchMethodError` on `java.lang.Object`, a failed `checkcast`, an
out-of-bounds field read — an unbounded distance from the collection with the
gap. Two checks move that to the cause.

**In-cycle (`ThreadRegistry::fold_pointer_map_into_blocked`, unconditional).**
After the fold, no address a blocked thread will resume on may lie in the
inactive young semispace. The per-slot tracker `GcBlockState::slot_origins`
already pins down every `(frame, slot)` a blocked thread holds and advances it
through each cycle's pointer map, so the check is two integer compares per slot
against a range read once per fold. A hit reports tid / frame / slot / address
plus `was_a_scanned_root`, which forks the fix: `false` = the deposit never
published the slot (root COVERAGE gap), `true` = the collector held it and left
it behind (EVACUATION gap). `VmHeap::young_inactive_semispace_range` is
`reclaimed_hole_at`'s inactive-semispace arm hoisted so this costs one arena
lock rather than three per address.

`SlotOrigin` gained a `live` flag for this, recorded from the frame's per-bci
liveness mask at deposit time — the check is only meaningful for slots the
collector was obliged to keep. The first build without it fired on every round
of the probe below: `AbstractExecutorService.invokeAll`'s `tasks` argument is
scoped out at the `f.get()` the caller parks in, so it is DEAD, correctly not
rooted, correctly reclaimed, and never read again. That is the liveness
analysis working, and it is exactly the population
`audit_frames_for_reclaimed_slots` already filters out for the same reason.

The whole-snapshot version of the same question is gated behind
`CRATONVM_DBG_BLOCKGC`: a snapshot legitimately carries conservative candidates
that are not object starts, and `forward_object` correctly declines to relocate
those, so it lands in the vacated arena on every moving cycle by design.
Measured: ~10-12 such benign entries per SmokeTests run, and **zero** frame-slot
hits in 12 runs.

**Deposit-side (`deposit_root_snapshot_inner`, `CRATONVM_DBG_BLOCKGC`).**
The precondition, checked with no GC race needed at all: a frame slot that
decodes as `Value::Object(Some(_))` and is LIVE at its frame's pc but is absent
from the snapshot is an object the collector cannot know to keep — and
`slot_origins` cannot rescue it either, because an object nothing rooted is
never IN a pointer map. Emits `[blockgc] UNPUBLISHED-LIVE-SLOT` with frame,
method, pc and slot. Measured **zero** over a full SmokeTests run (22 folds, 8
wakes), which is what closes out the locals half of the same question.

## 5. Added: the probe, and the repro harness

* `probes/BlockedFrameRootProbe.java` — the page's shape at its own rate
  instead of through a three-minute Hibernate class: a deep recursive stack
  whose every level holds four object locals across a blocking `invokeAll`,
  read back afterwards by field, by array content and by a virtual call (the
  three faces a reclaimed receiver fails). Exit status is the number of
  corrupted slots. HotSpot 0/8200; CratonVM 0/8200 with JIT and with `--nojit`.
  This is the "second, independent repro family" the page asked for on behalf
  of the precise-oop-maps work, in executable form.
  It is also what caught the `SlotOrigin::live` gap in §4 — the detector's
  first build fired on it every round.
* `ab-cpu.ps1` — two-binary A/B/B/A scored on per-process USER CPU TIME, for a
  shared box where wall clock cannot resolve anything. It quotes arguments
  containing spaces (an unquoted `--java-home "C:\Program Files\..."` reaches
  the VM as `C:\Program`, so every run fails argument parsing in ~40 ms — fast,
  uniform and entirely fake) and refuses to print a ratio if any run exited
  non-zero.
* `apps/hib-suite-runner/run-smoke-repro.sh` — N-run harness for the class,
  counting the three signatures separately and reporting a hit rate rather than
  a verdict. It pins its own CWD (see
  `reference_hib_suite_runner_class_overrides_and_cwd_trap`: from anywhere but
  the fixture directory every run dies in ~1 s in
  `JdbcConnectionContext.<clinit>` and scores as a crash).

## 6. Repro

```
cd apps/hib-suite-runner
./run-smoke-repro.sh -n 6 -b <cratonvm.exe>          # the class, hit-rate form
# invariant instead of symptom:
CRATONVM_DBG=blockgc <cratonvm.exe> --java-home <jdk25> --Xmx 256m \
    -cp probes/out -Dpasses=200 BlockedFrameRootProbe
```

A future occurrence should arrive as a
`blocked-thread frame slot points into the semispace this moving cycle just
evacuated` line naming the frame — not as a `NoSuchMethodError` on
`java.lang.Object`. If it does not, the gap is outside the blocked-thread path
and belongs on the `ClassId(0)` family page.
