# Open GC issues

This folder holds only GC pages with work left to do: open defects
(including pages that are partly fixed: the unfixed part keeps them here),
features that landed behind an off-by-default `CRATONVM_*` switch and wait
for a default decision, and proposals (`*-proposal-*`), which the user
triages.

A resolved page moves to `docs/internal/`: `common-*` pages to
`docs/internal/gc-common-round-20260923/`, collector pages (`gcd-*`,
`gengc-*`, `generational-*`, `g1-*`, `zgc-*`) to `docs/internal/gc/`, with a
`-FIXED-`, `-DONE-` (a proposal that was built), `-REJECTED-`,
`-SUPERSEDED-` or `-RETIRED-` suffix and the date. The architecture and the
state of every backend are in [`docs/GC.md`](../../GC.md).

**Start here: [`gce-handoff-gc-design-perf-round-close-20260929.md`](gce-handoff-gc-design-perf-round-close-20260929.md).**
The GC design and performance round (gce) closed on 2026-09-29 after two waves (e1, e2),
verified on Linux on all three collectors. Its summary, lane reports and per-page verdicts are
in [`docs/internal/gc-design-perf-round-20260929/`](../../internal/gc-design-perf-round-20260929/README.md).
The handoff lists the open items in order, the defaults the round changed and the pages it
filed (`gce-*`), which the sections below predate. The previous round's handoff
(`gcd-handoff-gc-defects-round-close-SUPERSEDED-20260929`) is retired into it.

This folder now holds 132 pages besides the handoff: 67 defect pages and 65 proposals.
The paragraph below describes the index as it stood at the end of wave d8; links to pages
retired since point into `docs/internal/` and say so.

**Status as of 2026-09-28, the end of the GC defects round's wave d8** (Serial /
Generational and common infrastructure; G1 and ZGC internals were out of
scope). Every page except the G1 / ZGC ones carries a STATUS block dated
2026-09-28 at its top, written from the round's final verification (the
Linux release build of the round branch at `307f0c6a2`, "d7"). That block,
not this index, is authoritative. The round summary, with the 100 pages it
retired, is
[`docs/internal/gc-defects-round-20260927/README.md`](../../internal/gc-defects-round-20260927/README.md).

What is here: 127 pages. 72 defect pages, of which 60 are Serial /
Generational or common and 12 belong to G1 or ZGC; 55 proposals, of which 54
were ranked by the round's triage and 1 belongs to G1 / ZGC.

## Open defects: Serial / Generational and common infrastructure

"Gen half fixed" marks a page whose Generational half is fixed and measured
and which stays open only for the G1 or ZGC half.

### OOME and compiled-frame liveness

- [`g1-humongous-refusal-with-free-space-and-old-array-fixup-misses-FIXED-20260930`](../../internal/gc/g1-humongous-refusal-with-free-space-and-old-array-fixup-misses-FIXED-20260930.md) (retired 2026-09-30, FIXED)
  -- `NativeFactoryReclaimProbe` 10/10 on G1: an evacuation-headroom young
  trigger, a fresh last-ditch mark, a cleanup scrub of dead objects' slots,
  and a completed-mark source filter.
- [`g1-needs-gc-is-a-shared-rmw-on-every-allocation`](g1-needs-gc-is-a-shared-rmw-on-every-allocation.md)
  -- a thread-local recount cadence is integrated, but repeatable performance
  evidence remains outstanding.

After an `OutOfMemoryError`, data the program dropped can stay reachable.
The named holder of most remaining failures is the true-root major's
fallback to the legacy young seed on promoting cycles (rank 1 proposal).

- [`common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications`](common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md)
  -- item 3 open: `NativeGrowthReclaimProbe` fails 6/6 in both modes (OOME at
  line 95); items 1 and 2 pass on Generational.
- [`gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928`](gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928.md)
  -- old half fixed for requested majors; the young half is not landed, and
  the one-major arm still fails 5/5.
- [`gcd-d4j-threads-oom-probe-crawls-on-a-full-old-gen-FIXED-20260928`](../../internal/gc/gcd-d4j-threads-oom-probe-crawls-on-a-full-old-gen-FIXED-20260928.md)
  -- `GenR4W5ThreadsOomProbe` takes 206-322 s (HotSpot about 8 s) and times
  out 1 run in 7.
- [`gcd-d4o-thread-exit-recovery-fails-without-an-exited-frame-holder-FIXED-20260928`](../../internal/gc/gcd-d4o-thread-exit-recovery-fails-without-an-exited-frame-holder-FIXED-20260928.md)
  -- 2 of 9 default runs pass; the census names the legacy-seed fallback.
- [`gcd-d6u-pinned-callee-oome-heap-stays-full-after-the-catch-20260928`](gcd-d6u-pinned-callee-oome-heap-stays-full-after-the-catch-20260928.md)
  -- 0 of 12 runs pass; the post-catch holder is the legacy-seed fallback.
- [`gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928`](../../internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md)
  -- new: heap-full shapes can livelock on young cycles that all fall back to
  the non-moving sweep (1 of 12 default thrash runs, three opt-in arms).
- [`gcd-d6s-pinned-copy-heap-full-run-can-time-out-20260928`](../../internal/gc/gcd-d6s-pinned-copy-heap-full-run-can-time-out-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- the flag-on timeouts are that livelock, not a hang.
- [`gengc-r4w6-review6-heap-full-thrash-jit-and-gc-stress-residual-FIXED-20260929`](../../internal/gc/gengc-r4w6-review6-heap-full-thrash-jit-and-gc-stress-residual-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- 11 of 12 default runs pass (0/3 on d4); one livelock timeout.
- [`gcd-d6s-thread-stack-size-argument-is-ignored-20260928`](../../internal/gc/gcd-d6s-thread-stack-size-argument-is-ignored-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- native stack and `-Xss` fixed; compiled self-recursion is still capped at
  4 MiB (item 2, JIT), so the probe prints `FAIL deep`.
- [`gcd-d2f-shadow-overflow-bail-keeps-moving-gc-FIXED-20260929`](../../internal/gc/gcd-d2f-shadow-overflow-bail-keeps-moving-gc-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- the fix is in; its probe is vacuous (overflows at 4034 levels before
  the shadow buffer bails) until the stack-size item above lands.
- [`gengc-r5w4-jit8-call-argument-pins-outlive-the-callees-use-of-them-20260926`](gengc-r5w4-jit8-call-argument-pins-outlive-the-callees-use-of-them-20260926.md)
  -- channel (a): `Gcd1ArgPinProbe` fails its warm case in every run; fix is
  the callee-owned-arguments proposal.
- [`gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927`](gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md)
  -- items 1 and 5 open; item 3 measured fixed.
- [`gengc-r5w6-oomjit10-remaining-holders-and-the-rust-boundary-20260927`](gengc-r5w6-oomjit10-remaining-holders-and-the-rust-boundary-20260927.md)
  -- items 1 and 5 open; no failing probe line of its own.
- [`gengc-r4w5-oomjit5-exceptional-frame-orphans-outlive-a-compiled-catch-20260924`](gengc-r4w5-oomjit5-exceptional-frame-orphans-outlive-a-compiled-catch-20260924.md)
  -- by reading; no probe line fails because of it.

### Pinning, conservative roots and the young copy

Once compiled code is live, young cycles rarely copy (the conservative-root
divert). The pinned young copy is the fix and is still opt-in.

- [`gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924`](gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md)
  -- the flip gate: row 3a passes on its main probe, row 3 (liveness) fails;
  not flipped.
- [`gengc-r4w4-young4-pinned-young-evacuation-design-20260924`](gengc-r4w4-young4-pinned-young-evacuation-design-20260924.md)
  -- design page; option C passes its probes, open until the flip.
- [`gengc-r4w5-pinned5-in-place-copy-limits-20260924`](gengc-r4w5-pinned5-in-place-copy-limits-20260924.md)
  -- L3 and L5 open (perf, flag-on only).
- [`gengc-r4w5-pinned5-serial-evacuator-residuals-20260924`](gengc-r4w5-pinned5-serial-evacuator-residuals-20260924.md)
  -- items 1 and 3 open; item 2 done.
- [`gengc-r4w5-pinwords5-young-pin-word-ledger-option-b-20260924`](gengc-r4w5-pinwords5-young-pin-word-ledger-option-b-20260924.md)
  -- working as designed, opt-in; waits for the TERM4 netty arm.
- [`gengc-r5w4-pin8-the-pin-ledger-spent-deadness-claims-the-marking-scan-does-not-trust-20260926`](gengc-r5w4-pin8-the-pin-ledger-spent-deadness-claims-the-marking-scan-does-not-trust-20260926.md)
  -- fixed in code; waits for the flag-on netty loop.
- [`gengc-r5w6-sizer10-refused-young-root-dangles-20260927`](gengc-r5w6-sizer10-refused-young-root-dangles-20260927.md)
  -- the landed check is clean on 11 rows; open for the producer hunt and
  the fail-open residuals.
- [`gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-20260927`](gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-20260927.md)
  -- fix opt-in (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`); flip gate not run.
- [`common-c-linux-takeover-signals-every-thread-FIXED-20260929`](../../internal/gc-common-round-20260923/common-c-linux-takeover-signals-every-thread-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- Generational soak clean but no take-over engaged in it; G1 / ZGC cells
  not run.
- [`gengc-core-system-gc-forces-non-moving-20260920`](gengc-core-system-gc-forces-non-moving-20260920.md)
  -- still true by default; the opt-in flip needs P-A, P-B, P-C.
- [`gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926`](gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md)
  -- perf: JIT-warm young cycles sweep in place; nothing flipped.
- [`gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928`](gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md)
  -- item 1 opt-in (`CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`), unmeasured; item 2
  unwritten.
- [`gengc-r4w4-young4-parallel-evacuator-scaling-limits-20260924`](gengc-r4w4-young4-parallel-evacuator-scaling-limits-20260924.md)
  -- perf; single d7 runs inside the host's noise decide nothing.

### JNI and FFM

The three JNI switches pass every correctness row together, but miss the
cost bar; they flip only together.

- [`gcd-d2i-jni-native-methods-are-counted-mutators-20260927`](gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md)
  -- correct with the flags; item 4 missed (`noop` 9.0x, `new-string` 18x of
  the default).
- [`common-w2c-jni-local-refs-are-raw-addresses`](common-w2c-jni-local-refs-are-raw-addresses.md)
  -- open with the defaults; the foreign transitions alone crashed once in
  three on a raw local.
- [`common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked`](common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md)
  -- closed with the flags on, open with the defaults.
- [`gengc-r4w4-rooting2-jni-foreign-thread-accessors-and-unrooted-results-20260924`](gengc-r4w4-rooting2-jni-foreign-thread-accessors-and-unrooted-results-20260924.md)
  -- closed with the flags on; merges into w9g and retires with the flip.
- [`gcd-d2i-foreign-attached-threads-publish-no-os-tid-20260927`](gcd-d2i-foreign-attached-threads-publish-no-os-tid-20260927.md)
  -- fixed behind `CRATONVM_JNI_FOREIGN_TRANSITIONS`; retires with the flip.
- [`gcd-d5f-jni-default-dispatch-costs-500ns-20260928`](gcd-d5f-jni-default-dispatch-costs-500ns-20260928.md)
  -- an empty native costs 524-530 ns by default; nothing of the fix landed.

### Old generation, cards, allocation and sizing

- [`gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure-20260924`](gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure-20260924.md)
  -- adaptive tenuring landed opt-in; its flip gate items 4-8 not run.
- [`gengc-r4w3-cards3-precise-array-cards-residuals-FIXED-20260929`](../../internal/gc/gengc-r4w3-cards3-precise-array-cards-residuals-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- item 1 and producer (g) done; open for the opt-in consumer's flag-on
  runs.
- [`gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper-20260924`](gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper-20260924.md)
  -- perf flip; the IR tier's `putfield` still ignores the inline card mark.
- [`gengc-alloc-tlab-sizer-is-blind-to-waste-DONE-20260929`](../../internal/gc/gengc-alloc-tlab-sizer-is-blind-to-waste-DONE-20260929.md) (retired 2026-09-29, DONE)
  -- fixed as opt-in arms; open only as the default decision (protocol A).
- [`gengc-alloc2-adaptive-tlab-sizer-is-bypassed-after-every-early-retire-DONE-20260929`](../../internal/gc/gengc-alloc2-adaptive-tlab-sizer-is-bypassed-after-every-early-retire-DONE-20260929.md) (retired 2026-09-29, DONE)
  -- fixed as opt-in arms; open only as the default decision.
- [`gengc-r4-alloc-tlab-memory-is-zeroed-twice-FIXED-20260929`](../../internal/gc/gengc-r4-alloc-tlab-memory-is-zeroed-twice-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- zero-once skips about 1.5 % of the zeroing; the swept free list is the
  remaining double write.
- [`gengc-r4w6-tlab6-humongous-reference-arrays-zeroed-under-the-old-gen-lock-20260924`](gengc-r4w6-tlab6-humongous-reference-arrays-zeroed-under-the-old-gen-lock-20260924.md)
  -- fix opt-in; the interleaved perf A/B decides.
- [`gengc-r5w5-sizer9-refill-can-carve-less-than-the-object-20260927`](gengc-r5w5-sizer9-refill-can-carve-less-than-the-object-20260927.md)
  -- Gen half fixed; open for the G1 / ZGC `refill_tlab_at_least` edit.

### Concurrent cycle, references and class unloading

- [`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929`](../../internal/gc/gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- `weak-to-resurrected-cleared=false` persists with the hook and the live
  young seed on.
- [`gengc-r4w5-concmark5-remark-reference-processing-design-DONE-20260929`](../../internal/gc/gengc-r4w5-concmark5-remark-reference-processing-design-DONE-20260929.md) (retired 2026-09-29, DONE)
  -- design implemented opt-in; retires with mark2.
- [`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926`](gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md)
  -- works behind the four opt-ins; the default cannot unload concurrently;
  gate item 2 fails one token.
- [`gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929`](../../internal/gc/gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- fix complete behind `CRATONVM_GEN_YOUNG_MIRROR_DEFER`; open as the
  four-switch flip.
- [`gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929`](../../internal/gc/gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- concurrent half opt-in; STW half done for requested majors.
- [`gengc-r5w3-unload7-retained-layouts-are-never-released-20260926`](gengc-r5w3-unload7-retained-layouts-are-never-released-20260926.md)
  -- the concurrent arm releases; the STW arm was not run with the four
  switches.
- [`gengc-r4w3-oldgen3-stw-major-and-concurrent-cycle-share-one-trigger-20260923`](../../internal/gc/gengc-r4w3-oldgen3-stw-major-and-concurrent-cycle-share-one-trigger-DONE-20260929.md) (retired 2026-09-29, DONE)
  -- the recorded run fails its bounds: STW majors still pre-empt the open
  cycle; with the service thread no cycle completes.
- [`gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924`](gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md)
  -- premise closed on the default path; open only as the service thread's
  flip.
- [`gengc-r4w5-oldcompact5-concurrent-remark-refuses-its-sweep-over-a-walk-gap-FIXED-20260929`](../../internal/gc/gengc-r4w5-oldcompact5-concurrent-remark-refuses-its-sweep-over-a-walk-gap-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- the d7 flip-gate runs opened no concurrent cycle (inconclusive).
- [`gengc-r4w5-concmark5-satb-barrier-logs-young-old-values-20260924`](gengc-r4w5-concmark5-satb-barrier-logs-young-old-values-20260924.md)
  -- perf; the opt-in filter is correct, only its measurement decides.
- [`common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot`](common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot.md)
  -- Gen half fixed (6/6 = HotSpot); open for G1 and ZGC.
- [`common-w4o-allocation-driven-collections-do-not-clear-weak-referents`](common-w4o-allocation-driven-collections-do-not-clear-weak-referents.md)
  -- cause 1 fixed and measured; open for cause 2 (G1).
- [`common-w4b-loader-pin-collision-unroots-another-vms-statics`](common-w4b-loader-pin-collision-unroots-another-vms-statics.md)
  -- Gen and root side fixed; open for the G1 / ZGC marker edits.

### Observability and test infrastructure

- [`gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted-20260923`](gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted-20260923.md)
  -- Gen half fixed (45 concurrent mark + 45 sweep rows in a recording); open
  for G1 and ZGC.
- [`gcd-d5q-wasteful-retire-census-counts-small-drains-FIXED-20260929`](../../internal/gc/gcd-d5q-wasteful-retire-census-counts-small-drains-FIXED-20260929.md) (retired 2026-09-29, FIXED)
  -- `wasteful_retires=` counts drains of small TLABs; the one-line fix is
  not written.

## G1 and ZGC pages (out of scope for this round)

Not re-verified this round; their STATUS blocks are older. Owners: the G1
and ZGC collector rounds.

- [`gcd-d10v-g1-heap-filling-probes-exit-before-any-output-with-an-uncaught-oome-20260929`](gcd-d10v-g1-heap-filling-probes-exit-before-any-output-with-an-uncaught-oome-20260929.md)
  -- new at the round's close: on G1, every heap-filling OOME probe dies
  before its first line with an uncaught `OutOfMemoryError` that has no Java
  frames. It blinds the G1 half of the OOME battery. Now also the home of the
  G1 `FullHeapResolveProbe` / `NativeGrowthReclaimProbe` rows that were parked
  on the retired humongous page.
- [`g1-concurrent-mark-reads-corrupt-value-cells-with-the-jit-on-FIXED-20260930`](../../internal/gc/g1-concurrent-mark-reads-corrupt-value-cells-with-the-jit-on-FIXED-20260930.md) (retired 2026-09-30, FIXED)
  -- 0 reports in 10 runs of each battery row; the 2026-09-30 source was the
  recovery reachability walk, not the marker.
- [`g1-humongous-refusal-with-free-space-and-old-array-fixup-misses-FIXED-20260930`](../../internal/gc/g1-humongous-refusal-with-free-space-and-old-array-fixup-misses-FIXED-20260930.md) (retired 2026-09-30, FIXED)
  -- see the entry above.
- [`g1-needs-gc-is-a-shared-rmw-on-every-allocation`](g1-needs-gc-is-a-shared-rmw-on-every-allocation.md)
  -- the interpreter no longer calls it per allocation; the JIT refill
  trigger still does.
- [`g1-w6p-mixed-parallel-equivalence-test-flakes`](g1-w6p-mixed-parallel-equivalence-test-flakes.md)
  -- a test flake on a loaded host.
- [`common-b-g1-roots-every-overlay-element-unconditionally`](common-b-g1-roots-every-overlay-element-unconditionally.md)
  -- G1 roots every collection-overlay element on every pause; needs a G1
  marker change.
- [`gcd-d1e-g1-zero-header-screens-read-num-slots-before-the-mark-20260927`](gcd-d1e-g1-zero-header-screens-read-num-slots-before-the-mark-20260927.md)
  -- by reading: G1's zero-header screens can follow a forwarded-looking mark
  out of the heap.
- [`gengc-r5w1-refs5-g1-remark-leaves-enqueued-phantom-referent-dangling-20260926`](gengc-r5w1-refs5-g1-remark-leaves-enqueued-phantom-referent-dangling-20260926.md)
  -- latent dangling referent after G1's remark enqueues a phantom
  reference.
- [`gengc-r5w3-unload7-g1-remark-may-unregister-layouts-of-unswept-instances-20260926`](gengc-r5w3-unload7-g1-remark-may-unregister-layouts-of-unswept-instances-20260926.md)
  -- suspected, not reproduced.
- [`zgc-decommit-ignores-xms`](zgc-decommit-ignores-xms.md)
  -- ZGC gives memory back at every cycle, below `-Xms`.
- [`gengc-r4w4-cards4-zgc-ref-store-plan-is-still-process-global-20260924`](gengc-r4w4-cards4-zgc-ref-store-plan-is-still-process-global-20260924.md)
  -- latent: needs two ZGC heaps in one process.
- Proposal:
  [`gengc-r5w4-defaults8-proposal-per-backend-defaults-for-g1-and-zgc-20260926`](gengc-r5w4-defaults8-proposal-per-backend-defaults-for-g1-and-zgc-20260926.md)
  -- measure the share sizer and the soft-ref LRU on G1 and ZGC before
  making them defaults there.

## Proposals, ranked

The round's triage (lane d8/y,
[`d8-y-report.md`](../../internal/gc-defects-round-20260927/d8-y-report.md),
decisions in `d8-y-decisions.tsv`) kept 54 proposals, each with a dated
STATUS block giving its gate and size, and ranked them by value. Ranks 1-6
are correctness or HotSpot parity with a failing or wrong d7 row; 7-19
unblock a flip or fix a measured gap; 20-35 are performance or hardening
with a stated measurement; 36-54 measure first or depend on another flip.

1. [`gcd-d5r-proposal-true-root-seed-beyond-requested-non-promoting-majors-20260928`](gcd-d5r-proposal-true-root-seed-beyond-requested-non-promoting-majors-20260928.md)
   -- free the excluded young survivors in the requested major; the holder
   of the thread-exit, pinned-callee and nepotism failures.
2. [`gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927`](gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md)
   -- single-pass frames stop rooting dead locals; census `NativeGrowthReclaimProbe` first.
3. [`gcd-d4o-proposal-callee-owned-arguments-20260928`](gcd-d4o-proposal-callee-owned-arguments-20260928.md)
   -- the only fix left for ArgPin.
4. [`gengc-r5w1-young5-proposal-grid-probe-next-steps-20260926`](gengc-r5w1-young5-proposal-grid-probe-next-steps-20260926.md)
   -- P3 first: a latent use-after-free in `store_object_starts_locked`.
5. [`common-w9c-proposal-weak-string-intern-table`](common-w9c-proposal-weak-string-intern-table.md)
   -- a weak intern table; `s.intern() == s` as on HotSpot.
6. [`gengc-r5w5-conc9-proposal-default-remark-clears-weak-refs-to-retained-finalizables-REJECTED-20260929`](../../internal/gc/gengc-r5w5-conc9-proposal-default-remark-clears-weak-refs-to-retained-finalizables-REJECTED-20260929.md) (retired 2026-09-29, REJECTED)
   -- premise corrected: the hook arm is wrong too on `weak-to-resurrected-cleared`.
7. [`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928`](gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md)
   -- make entering native cheap; the JNI flip's cost bar.
8. [`gengc-r5w2-conc6-proposal-the-failing-allocation-should-not-wait-for-the-cycle-20260926`](gengc-r5w2-conc6-proposal-the-failing-allocation-should-not-wait-for-the-cycle-20260926.md)
   -- run an owed cycle at a poll, not in the failing allocation.
9. [`common-d-proposal-marker-discovered-references`](common-d-proposal-marker-discovered-references.md)
   -- reference discovery in the marker.
10. [`gengc-r5w5-oomjit9-proposal-ir-oop-maps-from-one-liveness-20260927`](gengc-r5w5-oomjit9-proposal-ir-oop-maps-from-one-liveness-20260927.md)
    -- one exact live set per IR safepoint.
11. [`gcd-d1b-proposal-one-allocation-ladder-for-jit-helpers-REJECTED-20260929`](../../internal/gc/gcd-d1b-proposal-one-allocation-ladder-for-jit-helpers-REJECTED-20260929.md) (retired 2026-09-29, REJECTED)
    -- one allocation ladder for the JIT helpers.
12. [`gcd-d2g-proposal-tag-root-occurrences-by-rewritability-20260927`](gcd-d2g-proposal-tag-root-occurrences-by-rewritability-20260927.md)
    -- a rewritten bit per root occurrence.
13. [`gengc-r5w6-pin10-proposal-young-copy-round-three-20260927`](gengc-r5w6-pin10-proposal-young-copy-round-three-20260927.md)
    -- the young-copy gap (d7: 17.9-23.1 s default against about 3 s on HotSpot).
14. [`gcd-d5q-proposal-known-zero-young-free-list-20260928`](gcd-d5q-proposal-known-zero-young-free-list-20260928.md)
    -- pin10 item 1 in full.
15. [`gengc-r5w4-conc8-proposal-concurrent-start-buffer-covers-one-growth-step-20260926`](gengc-r5w4-conc8-proposal-concurrent-start-buffer-covers-one-growth-step-20260926.md)
    -- the concurrent start buffer covers a growth step.
16. [`gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth-20260926`](gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth-20260926.md)
    -- usage thresholds checked on old-gen growth.
17. [`gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines-20260926`](gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines-20260926.md)
    -- `gc,heap,exit`, phases, metaspace and age lines.
18. [`gcd-d2j-proposal-alloc-ladder-census-line-DONE-20260929`](../../internal/gc/gcd-d2j-proposal-alloc-ladder-census-line-DONE-20260929.md) (retired 2026-09-29, DONE)
    -- an `[GC] alloc_ladder:` exit census.
19. [`gengc-r5w4-defaults8-proposal-print-the-effective-heap-policy-20260926`](gengc-r5w4-defaults8-proposal-print-the-effective-heap-policy-20260926.md)
    -- a `[GC] policy:` line.
20. [`common-w28a-proposal-shard-the-lock-key-registry`](common-w28a-proposal-shard-the-lock-key-registry.md)
21. [`common-w9g-proposal-bulk-jni-array-copies`](common-w9g-proposal-bulk-jni-array-copies.md)
22. [`common-w26a-proposal-register-overlay-owner-once-per-slot`](common-w26a-proposal-register-overlay-owner-once-per-slot.md)
23. [`common-w29a-proposal-thread-local-get-one-global-ref-lock`](common-w29a-proposal-thread-local-get-one-global-ref-lock.md)
24. [`common-w9c-proposal-lambda-proxy-tlab-allocation`](common-w9c-proposal-lambda-proxy-tlab-allocation.md)
25. [`gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot-20260927`](gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot-20260927.md)
26. [`gengc-r5w1-crash5-proposal-owner-checked-raw-root-writes-20260926`](gengc-r5w1-crash5-proposal-owner-checked-raw-root-writes-20260926.md)
27. [`common-w14d-proposal-ephemeron-native-side-tables`](common-w14d-proposal-ephemeron-native-side-tables.md)
28. [`common-w16c-proposal-post-gc-native-cleanup-hook`](common-w16c-proposal-post-gc-native-cleanup-hook.md)
29. [`gengc-r5w6-old10-proposal-live-sweep-resurrects-and-keeps-its-set-in-a-bitmap-20260927`](gengc-r5w6-old10-proposal-live-sweep-resurrects-and-keeps-its-set-in-a-bitmap-20260927.md)
30. [`gengc-r5w5-old9-proposal-parallel-old-gen-phases-on-the-evac-pool-20260927`](gengc-r5w5-old9-proposal-parallel-old-gen-phases-on-the-evac-pool-20260927.md)
31. [`gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928`](gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928.md)
32. [`gcd-d4m-proposal-jni-local-referents-join-the-young-pin-ledger-20260928`](gcd-d4m-proposal-jni-local-referents-join-the-young-pin-ledger-20260928.md)
33. [`gcd-d3m-proposal-blocked-deposits-stand-in-the-young-pin-ledger-20260927`](gcd-d3m-proposal-blocked-deposits-stand-in-the-young-pin-ledger-20260927.md)
34. [`gcd-d1e-proposal-every-moving-cycle-pins-blocked-peer-interior-words-20260927`](gcd-d1e-proposal-every-moving-cycle-pins-blocked-peer-interior-words-20260927.md)
35. [`gcd-d5-proposal-cooperative-first-slice-before-the-round-0-take-over-20260928`](gcd-d5-proposal-cooperative-first-slice-before-the-round-0-take-over-20260928.md)
36. [`common-a-proposal-per-thread-handshakes`](common-a-proposal-per-thread-handshakes.md)
37. [`common-a-proposal-lock-free-blocked-transitions`](common-a-proposal-lock-free-blocked-transitions.md)
38. [`common-b-proposal-generation-aware-root-remap`](../../internal/gc-common-round-20260923/common-b-proposal-generation-aware-root-remap-REJECTED-20260929.md) (retired 2026-09-29, REJECTED)
39. [`common-b-proposal-dedupe-the-initiator-root-set`](common-b-proposal-dedupe-the-initiator-root-set.md)
40. [`common-w2g-proposal-blocked-threads-replay-pause-maps`](common-w2g-proposal-blocked-threads-replay-pause-maps.md)
41. [`common-w21e-proposal-coverage-slot-without-the-wait`](common-w21e-proposal-coverage-slot-without-the-wait.md)
42. [`gengc-r5w2-roots6-proposal-generation-keyed-jit-scan-memo-20260926`](gengc-r5w2-roots6-proposal-generation-keyed-jit-scan-memo-20260926.md)
43. [`gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries-20260927`](gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries-20260927.md)
44. [`gcd-d1c-proposal-reference-rows-pruned-at-every-reclamation-20260927`](gcd-d1c-proposal-reference-rows-pruned-at-every-reclamation-20260927.md)
45. [`common-w26c-proposal-sub-linear-freed-span-drops`](common-w26c-proposal-sub-linear-freed-span-drops.md)
46. [`gengc-r5w3-unload7-proposal-side-table-lookups-per-drain-not-per-object-20260926`](gengc-r5w3-unload7-proposal-side-table-lookups-per-drain-not-per-object-20260926.md)
47. [`gcd-d5u-proposal-ir-tier-inline-card-check-20260928`](gcd-d5u-proposal-ir-tier-inline-card-check-20260928.md)
48. [`gengc-r5w4-jit8-proposal-zero-fresh-frames-on-every-tier-20260926`](gengc-r5w4-jit8-proposal-zero-fresh-frames-on-every-tier-20260926.md)
49. [`common-w36b-proposal-reference-free-frames-skip-the-band-scan`](common-w36b-proposal-reference-free-frames-skip-the-band-scan.md)
50. [`common-w33b-proposal-census-raw-address-keyed-tables`](common-w33b-proposal-census-raw-address-keyed-tables.md)
51. [`common-w25c-proposal-test-vm-identities-from-the-real-counter`](common-w25c-proposal-test-vm-identities-from-the-real-counter.md)
52. [`gengc-r5w1-oom5-proposal-preallocated-oome-per-message-20260926`](gengc-r5w1-oom5-proposal-preallocated-oome-per-message-20260926.md)
53. [`gengc-r5w6-sizer10-proposal-refill-gate-probes-what-refill-can-serve-20260927`](gengc-r5w6-sizer10-proposal-refill-gate-probes-what-refill-can-serve-20260927.md)
54. [`gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927`](gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927.md)

## Opt-in switches and their flip gates

The switches below are off by default. The evidence per row is in
[`triage.md`](../../internal/gc-defects-round-20260927/triage.md), section
"Final triage, wave d8"; what each switch does is in `docs/GC.md`. One
switch was flipped to default on this round: `CRATONVM_GC_OLD_HUMONGOUS_TOP`
(`=0` turns it off). `CRATONVM_GC_OLD_PINNED_COMPACT` was flipped and reverted:
see its row below.

**Not yet: each waits for its gate.**

| Switch(es) | Gate |
|---|---|
| `CRATONVM_GC_OLD_PINNED_COMPACT` | The true-root seed of the full collection must survive a planned pinned compaction (today every one falls back to the legacy young seed: `oldsz_true_root_fallbacks=46` vs `majors=2` on `d1b_armC`), then the OOME-retention rows (`foome_jitoomroot`, `jit_oom_root_both`, `d1b_jitoomroot_B`, `w2_jitoomroot_clear`) SAME with it on; see `triage.md` "Final state" |
| `CRATONVM_GEN_PINNED_YOUNG_COPY` with `_PARALLEL`, `_TAKEOVER`, `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE`, `CRATONVM_GC_PAR_EVAC_CARD_SEED`, `CRATONVM_XT_BLOCKED_MONITOR_PROOF`, `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` (`CRATONVM_GEN_REQUESTED_MAJOR_NO_PROMOTE` was removed in gce e2) | Rows 3 and 3a of the pinstale6 page; no timeout in 10 runs of arms C and D (the livelock page); arm D with the futile-young backoff at its default 5/5 |
| `CRATONVM_JNI_INDIRECT_LOCALS`, `CRATONVM_JNI_NATIVE_TRANSITIONS`, `CRATONVM_JNI_FOREIGN_TRANSITIONS` (only together: the foreign transitions alone crashed once in three) | Arm E's `noop` and `new-string` within 2x of the default (the light in-native deposit proposal); correctness rows unchanged; the strict JNI corpus, netty-tcnative, JNA, lz4 / zstd |
| `CRATONVM_GEN_CONC_CLASS_UNLOAD`, `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, `CRATONVM_GEN_Y2O_LIVE_SEED`, `CRATONVM_GEN_YOUNG_MIRROR_DEFER` | The hook arm prints HotSpot's `GenR5W5RemarkRefsProbe` line; a full battery and a gc-stress set with all four; a Tomcat undeploy / Spring census. `CRATONVM_GEN_YOUNG_MIRROR_DEFER` is the enabling switch and the closest to ready |
| `CRATONVM_GC_ADAPTIVE_TENURING` | Items 4-8 of the tenuring page's gate with the flag (SurvivorOverflow / EvacThroughput A/B, `BinT 14` under gc-stress, the OOM ladder rows, MajorCadence, netty and Spring) |
| `CRATONVM_GC_OLD_LIVE_SWEEP` | `GenR4W4HeapFullThrashProbe -Xmx128m` 5/5 and `GenR4W5ThreadsOomProbe` 3/3 with it, the verify arm clean |
| `CRATONVM_GEN_CONC_SERVICE_THREAD` | The gc-stress rows with it, an unchanged thread count through `Thread.getAllStackTraces()` / `ThreadMXBean`, and the conc6 latency micro-probe; also explain why the service arm completes no cycle on `GenR4W4SteadyPromotionProbe` |
| `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` | The d4m page's three-arm A/B on `GenR4W4EvacThroughputProbe` |
| `CRATONVM_TLAB_SHARE_SIZER` | A round that measures G1 and ZGC (the flag changes their defaults too) |
| `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` | A planted walk break inside a concurrent cycle: `conc_mark_walk_gap_recoveries>=1` and `PASS` |
| `CRATONVM_GC_OLD_FRAG_COMPACT` | `GenR4W4FragProbe` default vs flag arm, and `GenR4W5MajorCadenceProbe` majors not above the default |
| `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` | The tlab6 page's three interleaved runs |
| `CRATONVM_GEN_SATB_OLD_ONLY`, `CRATONVM_JIT_INLINE_CARD_MARK`, `CRATONVM_TLAB_FILLER_SKIP_ZERO`, `CRATONVM_GC_OLD_GIVE_BACK` | Each its interleaved A/B on a quiet host; the inline card mark also needs the IR `putfield` arm |
| `CRATONVM_GC_PREALLOCATED_OOME_KINDS` | The OOME rows print the default arm's lines with it |
| `CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD`, `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` | The gates on the conc10 selective-promotion page, cards3 item 3, and the system-gc page (P-C) |

**Never as a default** (keep as embedder options or bisection levers, or
remove):

| Switch(es) | Why |
|---|---|
| `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` + `CRATONVM_GC_OLD_INTERIOR_DECOMMIT` | They return memory HotSpot Serial keeps (`GenR4W6OldShrinkProbe` prints `FAIL` on HotSpot); a G1-style footprint option for embedders |
| `CRATONVM_GC_OLD_BORROW_YOUNG` | No probe fails without it |
| `CRATONVM_GC_PAR_EVAC_TARGETED_WAKE`, `CRATONVM_GC_PAR_EVAC_OLD_PLAB_HINT`, `CRATONVM_GEN_EVAC_SCAN_PREFETCH` | **Removed in gce e2**: no gain measured in two rounds |
| `CRATONVM_GC_PAR_EVAC_BUFFERLESS`, `CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES` | **Removed in gce e2**: bisection levers nobody ran |
| `CRATONVM_TLAB_SIZE_RETIRED`, `CRATONVM_TLAB_WASTE_SHRINK`, `CRATONVM_GEN_PRISTINE_CHUNKS` | **Removed in gce e2**: superseded (the share sizer; zero-once) |
| `CRATONVM_GC_NATIVE_LATCHED_MAJOR` | **Removed in gce e2**: unbounded per episode; the default native-funnel debt covers its case |
| `CRATONVM_GEN_REQUESTED_MAJOR_NO_PROMOTE` | **Removed in gce e2**: redundant with gcd d9/a's wide true-root seed |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG` | **Removed in gce e2, rejected**: it does not do its job (`fy_nsoom` failed in both arms), and it broke `NativeGrowthReclaimProbe` with OSR off |
| `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` | Superseded by the default-on IR keep set; with the oracle on, `GenR4W6JitOomRootProbe` times out |
| `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS`, `CRATONVM_JIT_OSR_DROP_ORPHANS`, `CRATONVM_JIT_LOCAL_HANDLER_CLEAR_DEAD`, `CRATONVM_GC_STAGED_ARGS_KEEP_MASK`, `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS` | No probe needs them on d7 |
