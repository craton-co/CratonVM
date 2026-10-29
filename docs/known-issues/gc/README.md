# Open GC issues

This folder holds only GC pages with work left to do (since 2026-10-18 only ZGC pages and the proposal index below): open defects
(including pages that are partly fixed: the unfixed part keeps them here),
features that landed behind a `CRATONVM_*` switch and wait for their
deciding rows, and proposals. On 2026-10-10 the user moved the proposals and
the older `*-review-findings-*` pages to
[`docs/internal/proposals/gc/`](../../internal/proposals/gc/README.md); the
2026-10-15 `dev` merge moved the proposal books (`*-proposal-*` pages) back
here, and on 2026-10-17 the user moved them out again: every `*-proposal-*` page
is in `docs/internal/proposals/gc/` (out of the gci round's scope), and the links
below point there.

A resolved page moves to `docs/internal/`: `common-*` pages to
`docs/internal/gc-common-round-20260923/`, collector pages (`gcd-*`, `gce-*`,
`gcf-*`, `gcg-*`, `gch-*`, `gci-*`, `gengc-*`, `g1-*`, `zgc-*`) to `docs/internal/gc/`,
with a `-FIXED-`, `-DONE-` (a proposal that was built), `-REJECTED-`,
`-SUPERSEDED-`, `-REFUTED-`, `-CLOSED-` or `-RETIRED-` suffix and the date. The architecture
and the state of every backend are in [`docs/GC.md`](../../GC.md).

**GC round gci (2026-10-11 to 2026-10-16, waves i1-i36).** Generational backend and the common
infrastructure; G1 in scope from wave i5g (user, 2026-10-12); ZGC out of scope. Waves i1-i19 ran on
Windows (2026-10-11 to 2026-10-14); the round resumed on 2026-10-15 on the Linux host, merged
`origin/dev` twice and ran waves i20-i36 (defects only from i30, by the user's rule). Close-out
summary: [`round-close-20261016.md`](../../internal/gc-round-gci-20261011/round-close-20261016.md).
Records: [`docs/internal/gc-round-gci-20261011/`](../../internal/gc-round-gci-20261011/orch-notes.md)
(`orch-notes.md` is authoritative; `lane-rules.md`, the `i*-brief.md` briefs, the lane reports, the
`vi*.list` batteries and the `vi*-verdicts*.md` judgements). Default flips the user decided are
marked `WARNING gci-user-flip-<date>` at their sites and sit in their own `WARNING:` commits
(`git log --oneline --grep=WARNING:`), each with a `=0` kill. Its switch changes are in `docs/GC.md`,
sections "Generational and G1: what the gci round changed", "what gci waves i15-i18 changed" and
"what gci waves i19-i36 changed"; G1 opt-ins are rows of GC.md's G1 switch table. The pages it filed
are named `gci-i<wave><lane>-*`.

**ZGC round zt (2026-10-18 to 2026-10-19, waves t1-t5).** ZGC design and performance; GenGC and G1
out of scope. Rows ran locally (Windows) against HotSpot 25.0.3 `-XX:+UseZGC`, stdout byte-compared, process
CPU from the OS. Records: [`docs/internal/zgc-round-zt-20261018/`](../../internal/zgc-round-zt-20261018/LANE-RULES.md)
(lane rules and the `t<wave>-<lane>-report.md` lane reports). The wave-t5 audit
[`t5-pg3-report.md`](../../internal/zgc-round-zt-20261018/t5-pg3-report.md) classes every `zgc-*` page
(RETIRE / HOST / STRUCTURAL / OTHER-OWNER / OPEN), proposes the GC.md intended-difference paragraphs and
lists the moves; each page carries its verdict as a `zt t5/pg3` STATUS line. **This list is the
authoritative set of ZGC pages that stay here (19).** Every other `zgc-*` page leaves this folder:
retired to `docs/internal/gc/`, recorded as intended in `docs/GC.md`, or moved to its owner
(`docs/known-issues/jit/`, `docs/known-issues/footprint/`, a G1 / Generational / common page here, or the
`docs/known-issues/` root); the zr and zs lists below keep their entries, unlinked where the page leaves.

Collector work (mark, relocation, pacing, references):
- [`zgc-zt1rl-relocation-review-residuals-20261019`](../../internal/gc/zgc-zt1rl-relocation-review-residuals-SUPERSEDED-20261019.md)
- [`zgc-zt1tp-director-parity-residuals-20261019`](../../internal/gc/zgc-zt1tp-director-parity-residuals-FIXED-20261019.md)
- [`zgc-zt4ch-small-allocations-crawl-while-a-collection-is-due-20261019`](../../internal/gc/zgc-zt4ch-small-allocations-crawl-while-a-collection-is-due-DECIDED-20261019.md)
- [`zgc-zs9la-launcher-oome-rows-a-free-span-remainder-serves-the-receiver-20261016`](../../internal/gc/zgc-zs9la-launcher-oome-rows-a-free-span-remainder-serves-the-receiver-FIXED-20261019.md)
- [`zgc-zr4rv4-mark-pool-default-residuals-20261012`](../../internal/gc/zgc-zr4rv4-mark-pool-default-residuals-DECIDED-20261019.md)
- [`zgc-zs3ac-collection-cpu-residual-shares-20261014`](../../internal/gc/zgc-zs3ac-collection-cpu-residual-shares-SUPERSEDED-20261019.md)
- [`zgc-zr2cc-overlay-repair-row-is-vacuous-over-a-real-jdk-20261012`](../../internal/gc/zgc-zr2cc-overlay-repair-row-is-vacuous-over-a-real-jdk-DECIDED-20261019.md)
- [`zgc-zt3gn-generational-mode-review-residuals-20261019`](../../internal/gc/zgc-zt3gn-generational-mode-review-residuals-SUPERSEDED-20261019.md)
- [`zgc-zt3gn-young-cycles-process-references-and-finalizers-20261019`](../../internal/gc/zgc-zt3gn-young-cycles-process-references-and-finalizers-FIXED-20261019.md)

Compiled allocation and barriers (ZGC emitters):
- [`zgc-zt2ca-compiled-allocation-and-store-gates-what-is-left-20261019`](../../internal/gc/zgc-zt2ca-compiled-allocation-and-store-gates-what-is-left-SUPERSEDED-20261019.md)

Opt-in switches (the flip table):
- [`zgc-zt11pr-opt-in-flip-decisions-20261019`](../../internal/gc/zgc-zt11pr-opt-in-flip-decisions-RETIRED-20261019.md) (RETIRED zt t26/rt2; the current table of every ZGC opt-in key is section 2 of [`t26-rt2-report.md`](../../internal/zgc-round-zt-20261018/t26-rt2-report.md))

Native doors, define paths, strings:
- [`zgc-zt1nd-define-and-door-residuals-after-the-merge-20261019`](../../internal/gc/zgc-zt1nd-define-and-door-residuals-after-the-merge-SUPERSEDED-20261019.md)
- [`zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-20261015`](../../internal/gc/zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-CLOSED-20261019.md)
- [`zgc-zs10rv7-wave-s9-review-residuals-20261016`](../../internal/gc/zgc-zs10rv7-wave-s9-review-residuals-CLOSED-20261019.md)

Observability (MXBeans, JFR, flags):
- [`zgc-zt1ob-non-heap-pool-residuals-20261019`](../../internal/gc/zgc-zt1ob-non-heap-pool-residuals-CLOSED-20261019.md)
- [`zgc-zt1jf-jfr-review-residuals-20261019`](../../internal/gc/zgc-zt1jf-jfr-review-residuals-CLOSED-20261019.md)
- [`zgc-residuals-handed-over-by-the-gci-round-20261014`](../../internal/gc/zgc-residuals-handed-over-by-the-gci-round-FIXED-20261019.md)

Review residuals:
- [`zgc-zt3x2-wave-t2-review-residuals-20261019`](../../internal/gc/zgc-zt3x2-wave-t2-review-residuals-FIXED-20261019.md)
- [`zgc-zt4x3-wave-t3-review-residuals-20261019`](../../internal/gc/zgc-zt4x3-wave-t3-review-residuals-DECIDED-20261019.md)
- [`zgc-zt10x7-wave-t9-review-residuals-20261019`](../../internal/gc/zgc-zt10x7-wave-t9-review-residuals-SUPERSEDED-20261019.md) (zt t10/x7; the list above predates waves t6-t9's pages, and the links of pages retired to `docs/internal/gc/` were repointed there the same day)

Footprint, owed a Linux row (HOST):
- [`zgc-zr3fp-side-structures-kept-their-peak-20261012`](../../internal/gc/zgc-zr3fp-side-structures-kept-their-peak-SUPERSEDED-20261019.md)

**ZGC round zt, waves t6-t30 (to 2026-10-22; merged into `dev` by t28 and t30).** Grades of the last host
rows and the fixes of t28: [`t28-report.md`](../../internal/zgc-round-zt-20261018/t28-report.md); of t29 / t30:
[`t29-report.md`](../../internal/zgc-round-zt-20261018/t29-report.md). The ZGC pages that stay here after t30
(each opens with its t29/t30 or t28 STATUS):

- [`perf-counters-with-no-cratonvm-source`](../../internal/gc/perf-counters-with-no-cratonvm-source-FIXED-20261026.md) -- RETIRED 2026-10-26: the nine counters it listed now have sources, `sun.rt.internalVersion` carries the build tail; the by-design values are recorded there

Retired by zt t34 (2026-10-25; [`t34-report.md`](../../internal/zgc-round-zt-20261018/t34-report.md)):
[`zgc-concurrent-relocation-plan-sizes-every-survivor-in-the-pause`](../../internal/gc/zgc-concurrent-relocation-plan-sizes-every-survivor-in-the-pause-RETIRED-20261025.md)
(the R6 plan takes region liveness and legacy holders from the mark, `CRATONVM_ZGC_CRL_MARK_LIVENESS`; the
`Reference` family is compact, `CRATONVM_SYNTHETIC_FLOOR_REFERENCE_STUB_EXTENT`) and
[`zgc-zt32-ring-test-trips-a-debug-bitmap-assertion`](../../internal/gc/zgc-zt32-ring-test-trips-a-debug-bitmap-assertion-FIXED-20261025.md)
(region queries use `bases_in_span`, not the bounded `bases_within` pass).

Also retired by zt t34 (2026-10-25, the reference-handler lane): [`zgc-r6-reference-handler-spins-in-reference-queue-enqueue`](../../internal/gc/zgc-r6-reference-handler-spins-in-reference-queue-enqueue-FIXED-20261025.md) (the deferred sweep pruned the monitors of objects R6 had copied), [`zgc-round-stress-leaves-weak-references-unenqueued-under-load`](../../internal/gc/zgc-round-stress-leaves-weak-references-unenqueued-under-load-FIXED-20261025.md) and [`zgc-generational-weak-references-never-enqueued-under-round-stress`](../../internal/gc/zgc-generational-weak-references-never-enqueued-under-round-stress-FIXED-20261025.md) (late, not lost: the round now links into an unlocked queue in the pause).

Open after zwr (2026-10-25):

- [`zgc-rare-survivor-no-diagnostic-build-reproduces-20261025`](zgc-rare-survivor-no-diagnostic-build-reproduces-20261025.md) -- `Zt1MkSystemGcFresh` `--jdk-only` 3 survivors in 3 000 runs of the landed tree, none in 8 800 diagnostic runs (the residual of the delivery-thread page)

Retired by zwr (2026-10-25): [`zgc-delivery-thread-conservative-jit-band-keeps-a-weak-referent-FIXED-20261024`](../../internal/gc/zgc-delivery-thread-conservative-jit-band-keeps-a-weak-referent-FIXED-20261024.md) (five stale-word holders on the reference-delivery thread: the A5 residue sweep, the take-over's interior, Rust-band and companion words, and the previous notification's never-written slots).

Retired by zt t31 (2026-10-24; [`t31-zc5b-jd1-report.md`](../../internal/zgc-round-zt-20261018/t31-zc5b-jd1-report.md)):
[`zgc-zt26zc5b-compatible-loader-unload-and-weak-round-residuals`](../../internal/gc/zgc-zt26zc5b-compatible-loader-unload-and-weak-round-residuals-FIXED-20261024.md)
(the kept weak round was a frozen peer's TLAB `end` word, `CRATONVM_XT_SKIP_TLAB_POINTER_SLOTS`; the side rows
defer inside a concurrent cycle, `CRATONVM_ZGC_SIDE_ROW_DEFER_IN_CYCLE`) and
[`zgc-zt27jd1-compiled-static-call-decides-on-an-unloaded-owner`](../../internal/gc/zgc-zt27jd1-compiled-static-call-decides-on-an-unloaded-owner-FIXED-20261024.md)
(the `--compatible` thin-helper bind asks the resolver, `CRATONVM_JIT_COMPAT_BIND_ASKS_RESOLVER`).

Retired by zt t31/x10 (2026-10-24): [`zgc-zt13x10-review-residuals-CLOSED-20261019`](../../internal/gc/zgc-zt13x10-review-residuals-CLOSED-20261019.md)
(the review residuals: the ZGC `-Xlog` edge book, load and MMU lines; the stall mark on a joined pause; the full-collection heap
dumps and G1's periodic collection; the bare `-Xmx` / `-Xms` / `-Xmn` refusals; one atomic per TLAB refill; `runFinalization()`
off the caller on ZGC; the restored jdk-only contract). Filed from it:
[`launcher-source-file-mode-is-not-implemented-FIXED-20261025`](../../internal/fixed-bugs/launcher-source-file-mode-is-not-implemented-FIXED-20261025.md).

Retired by zki (2026-10-23; `docs/internal/zgc-round-zt-20261018/zki-report.md`):
[`zgc-zt23hunt4-transparent-huge-pages-flag-ignored-20261020`](../../internal/gc/zgc-zt23hunt4-transparent-huge-pages-flag-ignored-FIXED-20261023.md)
(explicit hugetlbfs pages, the OS-enforced advice, G1 / Serial lines) and
[`zgc-zt25hunt7b-zr2cp-grow-cpu-is-mutator-time-before-the-collections-20261020`](../../internal/gc/zgc-zt25hunt7b-zr2cp-grow-cpu-is-mutator-time-before-the-collections-FIXED-20261023.md)
(the wrapper intrinsics yield to the JDK bodies; compact `new` post-init).

Retired by t31 ([`t31-report.md`](../../internal/zgc-round-zt-20261018/t31-report.md)):
[`zgc-zt12df4-define-path-what-is-left-FIXED-20261023`](../../internal/gc/zgc-zt12df4-define-path-what-is-left-FIXED-20261023.md),
[`zgc-zt12pl4-perf-counters-what-is-left-FIXED-20261023`](../../internal/gc/zgc-zt12pl4-perf-counters-what-is-left-FIXED-20261023.md).

Retired by zgc wr / pc3 (2026-10-23): [`zgc-r21-collection-cost-per-allocated-byte-is-80x-hotspot-20261019`](../../internal/gc/zgc-r21-collection-cost-per-allocated-byte-is-80x-hotspot-CLOSED-20261023.md)
(host rows: parity per allocation, collector CPU 1.1-1.3x HotSpot's; generational arm measured worse, T1 decided;
two more pure-cost cuts) and [`zgc-weak-reference-held-by-a-dead-finalizable-is-cleared-before-finalize-20261022`](../../internal/gc/zgc-weak-reference-held-by-a-dead-finalizable-is-cleared-before-finalize-FIXED-20261023.md)
(`CRATONVM_STW_UNDISCOVERED_REFS_STRONG` default ON on ZGC).

Retired by t29: [`zgc-zt27fin1-finalizable-churn-and-signals-CLOSED-20261022`](../../internal/gc/zgc-zt27fin1-finalizable-churn-and-signals-CLOSED-20261022.md),
[`zgc-zt12gn4-generational-what-is-left-CLOSED-20261022`](../../internal/gc/zgc-zt12gn4-generational-what-is-left-CLOSED-20261022.md).

Retired by t28: [`zgc-zt12tg5-timer-and-launcher-what-is-left-CLOSED-20261022`](../../internal/gc/zgc-zt12tg5-timer-and-launcher-what-is-left-CLOSED-20261022.md),
[`zgc-zt24hunt6-freed-code-buffer-line-was-range-reuse-CLOSED-20261022`](../../internal/gc/zgc-zt24hunt6-freed-code-buffer-line-was-range-reuse-CLOSED-20261022.md),
[`zgc-zt12ca4-compiled-allocation-what-is-left-CLOSED-20261022`](../../internal/gc/zgc-zt12ca4-compiled-allocation-what-is-left-CLOSED-20261022.md)
(B4 moved to the JIT page `docs/internal/fixed-bugs/churn-constructor-runs-as-its-own-c1-body-under-a-c2-caller-FIXED-20261024.md`, FIXED 2026-10-24),
[`zgc-zt22hunt3-always-pre-touch-ignored-FIXED-20261022`](../../internal/gc/zgc-zt22hunt3-always-pre-touch-ignored-FIXED-20261022.md)
(its G1 half is item 10 of [`g1-and-generational-items-carried-from-the-zgc-zt-round-20261019`](../../internal/gc/g1-and-generational-items-carried-from-the-zgc-zt-round-FIXED-20261023.md)).

Retired by g1gen (2026-10-23): [`g1-and-generational-items-carried-from-the-zgc-zt-round-20261019`](../../internal/gc/g1-and-generational-items-carried-from-the-zgc-zt-round-FIXED-20261023.md)
(all ten items closed). G1 / Generational pages it filed:
[`gen-system-gc-keeps-dead-young-weak-referents-20261022`](../../internal/gc/gen-system-gc-keeps-dead-young-weak-referents-FIXED-20261026.md)
(retired below),
[`g1-near-full-heap-proxy-defines-collect-10x-and-refuse-allocations-20261022`](../../internal/gc/g1-near-full-heap-proxy-defines-collect-10x-and-refuse-allocations-SUPERSEDED-20261024.md)
(superseded by g1ep, below).

Retired 2026-10-26 (branch `gen-sysgc-weak-referents`):
[`gen-system-gc-keeps-dead-young-weak-referents-20261022`](../../internal/gc/gen-system-gc-keeps-dead-young-weak-referents-FIXED-20261026.md).
Fixed by `fcf5df15b0` (a lazily reclaimed young corpse is not a survivor); `G1gWeakClear` 64 / 64 and
`Zs7JfGcEvents sysgc` `weak_count_ge_64_each=true` on `-XX:+UseSerialGC`, both modes; its `Zt7ClClassNames`
Serial row is clean too (noted on the j3yg3 page).

Retired by g1ep (2026-10-24, branch `g1-evac-proxy-ki`):
[`g1-near-full-heap-proxy-defines-collect-10x-and-refuse-allocations-20261022`](../../internal/gc/g1-near-full-heap-proxy-defines-collect-10x-and-refuse-allocations-SUPERSEDED-20261024.md)
and `docs/known-issues/jit/`'s
[`r23-w6-g1-evacuation-copy-throughput-20261021`](../../internal/gc/r23-w6-g1-evacuation-copy-throughput-SUPERSEDED-20261024.md).
Their refusals, timeout and round shape are fixed; the cost left is filed here:
[`g1-near-full-proxy-row-runs-5x-hotspots-pauses-and-conservative-jit-words-retain-eden-20261024`](../../internal/gc/g1-near-full-proxy-row-runs-5x-hotspots-pauses-and-conservative-jit-words-retain-eden-SUPERSEDED-20261025.md)
(superseded by g1ep round 2, below),
[`g1-young-evacuation-copies-3x-slower-per-byte-and-keeplong-runs-7x-hotspot-20261024`](g1-young-evacuation-copies-3x-slower-per-byte-and-keeplong-runs-7x-hotspot-20261024.md).

Retired by g1sz (2026-10-27): the zr7ca residual close's
[`g1-default-sizing-never-collects-a-short-allocation-run-20261025`](../../internal/fixed-bugs/g1-default-sizing-never-collects-a-short-allocation-run-FIXED-20261026.md)
(no collection in 3.4 GB of allocation, RSS 3.9 GB against HotSpot's 0.64). `CRATONVM_G1_HEAP_RESIZE` is
default ON: the young-pause work of the g1sz round brought `BinTreesClassic 18`, the row that refused the
2026-10-25 soak at 1.16x CPU, to 0.98x, and the second soak's soundness rows read the same on both arms.
The young-copy residual above stays open with its g1sz measurements. Filed by the same round, a cost
of `dev`'s `c4ecb17ec2` that the flip made visible on `BinTreesClassic 18` (1.31x CPU, 1.03x with its
switch off), and retired by g1pin (2026-10-27: a blocked peer's stack word pins its object, not its
region, `CRATONVM_G1_PINS_PEER_STACK_OBJECTS`):
[`g1-peer-stack-word-pins-hold-eden-regions-and-double-young-copies-20261027`](../../internal/fixed-bugs/g1-peer-stack-word-pins-hold-eden-regions-and-double-young-copies-FIXED-20261027.md).

Retired by g1ep round 2 (2026-10-25, branch `g1-proxy-r2`):
[`g1-near-full-proxy-row-runs-5x-hotspots-pauses-and-conservative-jit-words-retain-eden-20261024`](../../internal/gc/g1-near-full-proxy-row-runs-5x-hotspots-pauses-and-conservative-jit-words-retain-eden-SUPERSEDED-20261025.md)
(superseded: 1397 pauses against HotSpot's 935; HotSpot's stdout in 66 of 72 runs three at a time).
It also found and fixed
[`g1-evacuation-failures-under-the-in-place-pass-dropped-live-objects-FIXED-20261025`](../../internal/gc/g1-evacuation-failures-under-the-in-place-pass-dropped-live-objects-FIXED-20261025.md).
What was left, a door OOME in about one run in twelve, was filed as
[`g1-near-full-proxy-row-refuses-a-rare-allocation-when-a-pinned-eden-region-fills-the-heap-20261025`](../../internal/gc/g1-near-full-proxy-row-refuses-a-rare-allocation-when-a-pinned-eden-region-fills-the-heap-FIXED-20261026.md)
and is fixed (2026-10-27, branch `g1-proxy-row-alloc`): a held region keeps only its named objects
in place (`CRATONVM_G1_IN_PLACE_HELD_OBJECTS_MOVE`), and a mark root is its own seed only when the
gray gate would scan it (`CRATONVM_G1_MARK_ROOT_SCREEN_DECODES`). Its R22 gate showed that
[`r22-w7-flipgcrows-nojit-g1-oom`](../../internal/fixed-bugs/r22-w7-flipgcrows-nojit-g1-oom-FIXED-20261021.md)
still throws about once in 30 `--nojit` runs, on `dev` as on the branch:
[`g1-r22-aastore-humongous-row-refuses-a-fragmented-humongous-allocation-under-nojit-20261027`](../../internal/gc/g1-r22-aastore-humongous-row-refuses-a-fragmented-humongous-allocation-under-nojit-FIXED-20261027.md),
fixed (2026-10-27, branch `g1-r22-frag-humongous`): a humongous request still refused on shape
after the staged compactions gets one in-place slide into the Free regions below the top movable
region (`CRATONVM_G1_FRAGMENTED_HUMONGOUS_SLIDE`).

Retired by the gcj j1pn retirement (2026-10-25, branch `gcj-j1pn-fix`):
[`gcj-j1pn-raw-root-write-owner-findings-20261019`](../../internal/gc/gcj-j1pn-raw-root-write-owner-findings-FIXED-20261025.md)
(the wake floor, the guard-band audit verdict, a multi-writer ring with VM tags) and
[`gcj-j1pn-young-pin-ledger-review-findings-20261019`](../../internal/gc/gcj-j1pn-young-pin-ledger-review-findings-FIXED-20261025.md)
(the from-space screen, the collector-entry cut, the bounded dead-spill excuse flipped ON).

Retired by gcj close (2026-10-23):
[`gcj-j1pn-interpreter-conservative-probes-root-primitives-20261019`](../../internal/gc/gcj-j1pn-interpreter-conservative-probes-root-primitives-FIXED-20261023.md)
(its two switches default ON, `--compatible` minted-only),
[`jfr-gc-safepoint-rows-have-no-behavioural-unit-test-20261019`](../../internal/gc/jfr-gc-safepoint-rows-have-no-behavioural-unit-test-FIXED-20261023.md)
(was `docs/known-issues/jfr/`), and the page its rows filed,
[`gen-jit-young-pauses-keep-both-weak-referents-again-20261023`](../../internal/gc/gen-jit-young-pauses-keep-both-weak-referents-again-FIXED-20261023.md)
(JIT round 23's lazy zero left dead headers whole; the young survival test now asks the lazy-dirty set).

**ZGC round zs (2026-10-14 to 2026-10-16, waves s1-s10, CLOSED; merged into dev by the zt round).** Handoff:
`zgc-zs-handoff-round-close-SUPERSEDED-20261019.md` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED)). Records:
[`docs/internal/zgc-round-zs-20261014/`](../../internal/zgc-round-zs-20261014/orch-notes.md). Pages it filed are
named `zgc-zs<wave><lane>-*`; the s10/pg2 audit classes each one.

**ZGC round zr (2026-10-12 to 2026-10-13, waves z1-z11, CLOSED).** Handoff and open backlog:
[`zgc-zr-handoff-round-close-SUPERSEDED-20261019.md`](../../internal/gc/zgc-zr-handoff-round-close-SUPERSEDED-20261019.md). ZGC design and
performance (GenGC and G1 out of scope), verified on Linux against HotSpot 25 `-XX:+UseZGC` in both
modes. Records: [`docs/internal/zgc-round-zr-20261012/`](../../internal/zgc-round-zr-20261012/orch-notes.md)
(`orch-notes.md` is authoritative). Changes: [`docs/GC.md`](../../GC.md), section "ZGC: what the zr
round changed". Pages it filed are named `zgc-zr<wave><lane>-*`.

**Retired in the gci round.** The first session (waves i1-i19) retired 34 pages, recorded one by
one in `orch-notes.md`. From the resumed session, 61 more: 48 on 2026-10-15 and 13 on 2026-10-16,
each with its old name, new name and suffix in
[`retired-20261015.tsv`](../../internal/gc-round-gci-20261011/retired-20261015.tsv) (collector pages
now in `docs/internal/gc/`, `common-*` pages in `docs/internal/gc-common-round-20260923/`).

Open zr pages (48):
- [`zgc-zr11ia-inline-alloc-trims-what-remains-20261013`](../../internal/gc/zgc-zr11ia-inline-alloc-trims-what-remains-SUPERSEDED-20261019.md)
- `zgc-zr11md-unnamed-per-class-side-tables-20261013` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zr11md-vtable-descriptors-are-copied-per-class-20261013` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- `zgc-zr11rv9-z8-z9-review-residuals-20261013` (leaves this folder: t5/pg3 RETIRE (FIXED))
- [`zgc-zr1al-infallible-alloc-still-aborts-20261012`](../../internal/gc/zgc-zr1al-infallible-alloc-still-aborts-FIXED-20261017.md)
- [`zgc-zr1al-per-object-parallel-sweep-splits-the-raw-word-range-20261012`](../../internal/gc/zgc-zr1al-per-object-parallel-sweep-splits-the-raw-word-range-FIXED-20261017.md)
- [`zgc-zr1al-vm-tlab-tail-retired-after-a-cycle-undercounts-allocated-20261012`](../../internal/fixed-bugs/zgc-zr1al-vm-tlab-tail-retired-after-a-cycle-undercounts-allocated-FIXED-20261017.md)
- [`zgc-zr1br-takeover-straddles-the-satb-gate-20261012`](../../internal/fixed-bugs/zgc-zr1br-takeover-straddles-the-satb-gate-FIXED-20261017.md)
- `zgc-zr1gn-object-cards-rescan-whole-old-arrays-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- [`zgc-zr1rf-concurrent-mark-traces-reference-slots-strongly-20261012`](../../internal/gc/zgc-zr1rf-concurrent-mark-traces-reference-slots-strongly-FIXED-20261018.md)
- `zgc-zr1rl-raw-jni-locals-on-a-native-peer-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- [`zgc-zr1rl-relocations-table-prune-every-cycle-20261012`](../../internal/gc/zgc-zr1rl-relocations-table-prune-every-cycle-FIXED-20261019.md)
- `zgc-zr1tr-maxgcpausemillis-sizes-zgc-collection-frequency-20261012` (leaves this folder: t5/pg3 structural, recorded)
- `zgc-zr2a2-rbc6-array-alloc-probe-is-a-quadratic-allocation-workload-20261012` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- [`zgc-zr2cc-overlay-repair-row-is-vacuous-over-a-real-jdk-20261012`](../../internal/gc/zgc-zr2cc-overlay-repair-row-is-vacuous-over-a-real-jdk-DECIDED-20261019.md)
- `zgc-zr2cp-maxrss-is-four-times-hotspot-20261012` (leaves this folder: t5/pg3 moves to another owner)
- [`zgc-zr2t2-gc-init-lines-not-printed-20261012`](../../internal/gc/zgc-zr2t2-gc-init-lines-not-printed-CLOSED-20261019.md)
- [`zgc-zr2t2-heap-used-is-byte-granular-not-page-granular-20261012`](../../internal/gc/zgc-zr2t2-heap-used-is-byte-granular-not-page-granular-CLOSED-20261019.md)
- [`zgc-zr3fp-side-structures-kept-their-peak-20261012`](../../internal/gc/zgc-zr3fp-side-structures-kept-their-peak-SUPERSEDED-20261019.md)
- `zgc-zr3fp-slide-transients-scale-with-objects-moved-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zr3ja-compiled-newarray-overshoots-the-occupancy-trigger-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zr3lt-global-state-review-20261012` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zr3mt-hotspot-spelling-of-parallelgcthreads-never-reached-g1-20261012` (leaves this folder: t5/pg3 moves to another owner)
- [`zgc-zr3mt-zero-gc-thread-flags-warn-instead-of-refusing-20261012`](../../internal/gc/zgc-zr3mt-zero-gc-thread-flags-warn-instead-of-refusing-FIXED-20261019.md)
- `zgc-zr3rv3-huge-extent-table-residuals-20261012` (leaves this folder: t5/pg3 RETIRE (FIXED))
- [`zgc-zr4ab-infallible-residuals-20261012`](../../internal/gc/zgc-zr4ab-infallible-residuals-FIXED-20261017.md)
- [`zgc-zr4jf-allocation-stall-event-has-no-size-20261012`](../../internal/gc/zgc-zr4jf-allocation-stall-event-has-no-size-FIXED-20261019.md)
- `zgc-zr4jf-jfr-events-not-produced-and-writer-limits-20261012` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zr4rv4-every-trigger-residuals-20261012` (leaves this folder: t5/pg3 RETIRE (FIXED))
- `zgc-zr4rv4-footprint-residuals-20261012` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- [`zgc-zr4rv4-latched-construction-seeds-20261012`](../../internal/gc/zgc-zr4rv4-latched-construction-seeds-FIXED-20261019.md)
- [`zgc-zr4rv4-mark-pool-default-residuals-20261012`](../../internal/gc/zgc-zr4rv4-mark-pool-default-residuals-DECIDED-20261019.md)
- [`zgc-zr4rv4-stale-request-stamp-and-ja-residuals-20261012`](../../internal/gc/zgc-zr4rv4-stale-request-stamp-and-ja-residuals-FIXED-20261019.md)
- `zgc-zr4un-uncommitter-needs-the-readable-give-back-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- [`zgc-zr4un-uncommitter-shape-residuals-20261012`](../../internal/gc/zgc-zr4un-uncommitter-shape-residuals-CLOSED-20261019.md)
- `zgc-zr5cr-near-oom-collections-cost-seven-times-hotspots-cpu-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zr5rv5-uncommitter-resets-beside-mutators-on-windows-20261012` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zr6tp-near-full-trigger-pacing-20261013` (leaves this folder: t5/pg3 structural, recorded)
- [`zgc-zr7ca-compiled-small-array-allocation-is-five-times-hotspot-20261013`](../../internal/gc/zgc-zr7ca-compiled-small-array-allocation-is-five-times-hotspot-SUPERSEDED-20261019.md)
- `zgc-zr7pc-a-collection-reads-every-live-header-four-times-20261013` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zr7rs-boot-rss-by-owner-20261013` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zr8fm-boot-footprint-owners-20261013` (leaves this folder: t5/pg3 moves to another owner)
- [`zgc-zr8zb-byte-proportional-allocation-cost-decomposed-20261013`](../../internal/gc/zgc-zr8zb-byte-proportional-allocation-cost-decomposed-SUPERSEDED-20261019.md)
- [`zgc-zr9hp-xlog-exit-and-stats-lines-20261013`](../../internal/gc/zgc-zr9hp-xlog-exit-and-stats-lines-CLOSED-20261019.md)
- [`zgc-zr9rv8-z8-review-residuals-20261013`](../../internal/gc/zgc-zr9rv8-z8-review-residuals-FIXED-20261019.md)

**ZGC (93 pages: 60 open, 33 proposals; out of the gci round's scope).** The zr round (2026-10-12
to 2026-10-13, waves z1-z11, CLOSED) owns them: handoff and open backlog in
[`zgc-zr-handoff-round-close-SUPERSEDED-20261019.md`](../../internal/gc/zgc-zr-handoff-round-close-SUPERSEDED-20261019.md), the gci round's
ZGC leftovers in
[`zgc-residuals-handed-over-by-the-gci-round-FIXED-20261019.md`](../../internal/gc/zgc-residuals-handed-over-by-the-gci-round-FIXED-20261019.md);
records [`docs/internal/zgc-round-zr-20261012/`](../../internal/zgc-round-zr-20261012/orch-notes.md),
changes in `docs/GC.md` section "ZGC: what the zr round changed". The zr pages are `zgc-*` here;
the 34 it retired are `zgc-*` with a suffix in `docs/internal/gc/`.

zr proposals (24; [`zgc-zr2a2-proposal-per-thread-chunk-cache`](../../internal/gc/zgc-zr2a2-proposal-per-thread-chunk-cache-REFUTED-20261025.md) was refuted on 2026-10-25: the arena lock is under 0.3 % of small-thread CPU and off-CPU time on every deciding row):
- [`zgc-zr1al-proposal-bounded-complement-young-sweep-20261012`](../../internal/proposals/gc/tier3/zgc-zr1al-proposal-bounded-complement-young-sweep-20261012.md)
- [`zgc-zr1al-proposal-page-chunks-and-size-classes-20261012`](../../internal/proposals/gc/tier3/zgc-zr1al-proposal-page-chunks-and-size-classes-20261012.md)
- [`zgc-zr1al-proposal-per-page-live-bytes-sweep-20261012`](../../internal/proposals/gc/tier2/zgc-zr1al-proposal-per-page-live-bytes-sweep-20261012.md)
- [`zgc-zr1al-proposal-skip-zeroing-recommitted-and-swept-spans-20261012`](../../internal/proposals/gc/tier3/zgc-zr1al-proposal-skip-zeroing-recommitted-and-swept-spans-20261012.md)
- [`zgc-zr1br-proposal-load-barrier-arming-plan-20261012`](../../internal/proposals/gc/tier2/zgc-zr1br-proposal-load-barrier-arming-plan-20261012.md)
- [`zgc-zr1br-proposal-takeover-resumes-gate-windows-20261012`](../../internal/proposals/gc/tier3/zgc-zr1br-proposal-takeover-resumes-gate-windows-20261012.md)
- [`zgc-zr1gn-proposal-generational-direction-20261012`](../../internal/proposals/gc/tier3/zgc-zr1gn-proposal-generational-direction-20261012.md)
- [`zgc-zr1mk-proposal-concurrent-mark-default-20261012`](../../internal/gc/zgc-zr1mk-proposal-concurrent-mark-default-SUPERSEDED-20261026.md) (SUPERSEDED 2026-10-26)
- [`zgc-zr1mk-proposal-mark-loop-costs-20261012`](../../internal/proposals/gc/tier2/zgc-zr1mk-proposal-mark-loop-costs-20261012.md)
- [`zgc-zr1rf-proposal-concurrent-soft-references-take-mark-end-roots-20261012`](../../internal/proposals/gc/tier3/zgc-zr1rf-proposal-concurrent-soft-references-take-mark-end-roots-20261012.md)
- [`zgc-zr1rf-proposal-stw-only-zgc-takes-the-generational-weak-table-rules-20261012`](../../internal/proposals/gc/tier3/zgc-zr1rf-proposal-stw-only-zgc-takes-the-generational-weak-table-rules-20261012.md)
- [`zgc-zr1rl-proposal-cost-gate-by-slot-density-and-fragmentation-20261012`](../../internal/proposals/gc/tier3/zgc-zr1rl-proposal-cost-gate-by-slot-density-and-fragmentation-20261012.md)
- [`zgc-zr1rl-proposal-remap-in-the-next-mark-20261012`](../../internal/proposals/gc/tier2/zgc-zr1rl-proposal-remap-in-the-next-mark-20261012.md)
- [`zgc-zr2a2-proposal-heap-cell-charges-per-chunk-20261012`](../../internal/proposals/gc/tier2/zgc-zr2a2-proposal-heap-cell-charges-per-chunk-20261012.md)
- [`zgc-zr2a2-proposal-tlab-sizing-ergonomics-20261012`](../../internal/proposals/gc/tier2/zgc-zr2a2-proposal-tlab-sizing-ergonomics-20261012.md)
- [`zgc-zr2cc-proposal-every-trigger-opens-a-concurrent-cycle-20261012`](../../internal/proposals/gc/tier3/zgc-zr2cc-proposal-every-trigger-opens-a-concurrent-cycle-20261012.md)
- [`zgc-zr2cp-proposal-large-objects-without-arena-contiguity-20261012`](../../internal/proposals/gc/tier3/zgc-zr2cp-proposal-large-objects-without-arena-contiguity-20261012.md)
- [`zgc-zr3fp-proposal-bitmap-pages-follow-arena-granules-20261012`](../../internal/proposals/gc/tier3/zgc-zr3fp-proposal-bitmap-pages-follow-arena-granules-20261012.md)
- [`zgc-zr3mt-proposal-serial-claim-without-an-atomic-rmw-20261012`](../../internal/proposals/gc/tier3/zgc-zr3mt-proposal-serial-claim-without-an-atomic-rmw-20261012.md)
- [`zgc-zr4ab-proposal-vm-internal-reserve-20261012`](../../internal/proposals/gc/tier3/zgc-zr4ab-proposal-vm-internal-reserve-20261012.md)
- [`zgc-zr4un-proposal-pointer-map-as-a-sorted-slice-20261012`](../../internal/proposals/gc/tier2/zgc-zr4un-proposal-pointer-map-as-a-sorted-slice-20261012.md)
- [`zgc-zr7ca-proposal-chunk-zeroing-frontier-20261013`](../../internal/proposals/gc/tier2/zgc-zr7ca-proposal-chunk-zeroing-frontier-20261013.md)

**Earlier rounds.** gch (2026-10-08 to 2026-10-09, waves h1-h4): handoff
[`gch-handoff-gc-round-close-RETIRED-20261012`](../../internal/gc/gch-handoff-gc-round-close-RETIRED-20261012.md),
records [`docs/internal/gc-round-gch-20261008/`](../../internal/gc-round-gch-20261008/orch-notes.md),
GC.md section ["Generational: what the gch round changed"](../../GC.md#generational-what-the-gch-round-changed-2026-10-08).
gcg (closed 2026-10-02): handoff
[`gcg-handoff-gc-round-close-CLOSED-20261015`](../../internal/gc/gcg-handoff-gc-round-close-CLOSED-20261015.md),
summary [`docs/internal/gc-round-gcg-20261001/README.md`](../../internal/gc-round-gcg-20261001/README.md).
gcf (closed 2026-10-01): handoff
[`gcf-handoff-gc-design-perf-round-close`](../../internal/gc/gcf-handoff-gc-design-perf-round-close-DONE-20261001.md),
records [`docs/internal/gc-design-perf-round-20261001/`](../../internal/gc-design-perf-round-20261001/README.md).
The pages those rounds retired are in `docs/internal/gc/`; their lists are in each round's records.

**Retired on 2026-10-18 (user: "retire all").** Every remaining non-ZGC page moved to
`docs/internal/gc/`: the eleven the user ruled on (i9ds, i8rv, i7orch, i44cd, i8pe, sizer10 and A7,
i13ef and `CRATONVM_G1_FULL_GC_ONE_REPORT`, h2jr JC, i1jn) as `-RETIRED-RULING-20261018`, the
already-closed ones under their `-CLOSED-` names, and the rest as `-RETIRED-UNFIXED-20261018`.
Those last pages still describe open defects or unjudged rows (among them i29cv2, i15rf, i13oa D,
i24ym, i43gy, i12ge, i12ul, i20ys, i22sz and the i48 pages); grep `RETIRED-UNFIXED` to find them and
move a page back here if it bites. Flag defaults were left as they were. The 278 stale round records
(superseded lane notes, briefs and reports nothing cites) moved to
[`docs/internal/retired/stale-gc-round-docs-20261018/`](../../internal/retired/stale-gc-round-docs-20261018/).
Every move is a row of
[`retired-20261015.tsv`](../../internal/gc-round-gci-20261011/retired-20261015.tsv).

Retired after the zr close, during the zs round (30, each now in `docs/internal/gc/`; they were still listed as open above until 2026-10-16):
- [`zgc-zr11md-constant-pool-slots-are-24-bytes-20261013`](../../internal/gc/zgc-zr11md-constant-pool-slots-are-24-bytes-20261013.md)
- [`zgc-zr11nd-native-oom-replay-residuals-20261013`](../../internal/gc/zgc-zr11nd-native-oom-replay-residuals-20261013.md)
- [`zgc-zr1al-oversize-arrays-are-unsizable-outside-the-high-sweep-20261012`](../../internal/gc/zgc-zr1al-oversize-arrays-are-unsizable-outside-the-high-sweep-FIXED-20261017.md)
- [`zgc-zr1rl-proposal-concurrent-relocation-prerequisites-20261012`](../../internal/gc/zgc-zr1rl-proposal-concurrent-relocation-prerequisites-SUPERSEDED-20261028.md)
- [`zgc-zr1rl-proposal-page-evac-arm-20261012`](../../internal/gc/zgc-zr1rl-proposal-page-evac-arm-SUPERSEDED-20261028.md)
- [`zgc-zr2cp-proxyboxing-collects-four-times-hotspot-near-oom-20261013`](../../internal/gc/zgc-zr2cp-proxyboxing-collects-four-times-hotspot-near-oom-20261013.md)
- [`zgc-zr2cp-softrefbeforeoome-conservative-jit-word-keeps-the-referent-20261013`](../../internal/gc/zgc-zr2cp-softrefbeforeoome-conservative-jit-word-keeps-the-referent-20261013.md)
- [`zgc-zr2t2-proposal-stw-aware-rate-reserve-20261012`](../../internal/gc/zgc-zr2t2-proposal-stw-aware-rate-reserve-REFUTED-20261028.md)
- [`zgc-zr3ja-proposal-tlab-share-sizer-on-zgc-20261012`](../../internal/gc/zgc-zr3ja-proposal-tlab-share-sizer-on-zgc-NEUTRAL-20261028.md)
- [`zgc-zr5cr-proposal-refill-capacity-gate-20261012`](../../internal/proposals/gc/tier3/zgc-zr5cr-proposal-refill-capacity-gate-20261012.md)
- [`zgc-zr7ca-proposal-ir-inline-alloc-fast-path-trims-20261013`](../../internal/gc/zgc-zr7ca-proposal-ir-inline-alloc-fast-path-trims-CLOSED-20261026.md)
- [`zgc-zr8fm-class-metadata-per-class-cost-20261013`](../../internal/gc/zgc-zr8fm-class-metadata-per-class-cost-20261013.md)
- [`zgc-zr8fm-proposal-raw-address-unsafe-reads-in-the-jit-20261013`](../../internal/proposals/gc/tier3/zgc-zr8fm-proposal-raw-address-unsafe-reads-in-the-jit-20261013.md)
- [`zgc-zr8nr-one-attempt-native-doors-remaining-20261013`](../../internal/gc/zgc-zr8nr-one-attempt-native-doors-remaining-20261013.md)
- [`zgc-zr8st-lazy-tlab-starts-remaining-steps-20261013`](../../internal/gc/zgc-zr8st-lazy-tlab-starts-remaining-steps-20261013.md)
- [`zgc-zr9bo-proposal-card-start-index-refuted-by-the-query-census-20261013`](../../internal/proposals/gc/tier4/zgc-zr9bo-proposal-card-start-index-refuted-by-the-query-census-20261013.md)
- [`zgc-zr9sl-selective-relocation-20261013`](../../internal/gc/zgc-zr9sl-selective-relocation-20261013.md)
- [`zgc-zr1tr-pause-beans-count-one-pause-per-cycle-20261012`](../../internal/gc/zgc-zr1tr-pause-beans-count-one-pause-per-cycle-20261012.md)
- [`zgc-zr7rv6-near-full-floor-runs-the-heap-dry-before-non-collecting-native-doors-20261013`](../../internal/gc/zgc-zr7rv6-near-full-floor-runs-the-heap-dry-before-non-collecting-native-doors-20261013.md)
- [`zgc-zr9hp-allocation-collections-book-major-beans-20261013`](../../internal/gc/zgc-zr9hp-allocation-collections-book-major-beans-20261013.md)
- [`zgc-zr1br-proposal-compiled-store-barrier-trims-20261012`](../../internal/proposals/gc/tier3/zgc-zr1br-proposal-compiled-store-barrier-trims-20261012.md)
- [`zgc-zr1tr-proposal-rate-trigger-flip-20261012`](../../internal/proposals/gc/tier2/zgc-zr1tr-proposal-rate-trigger-flip-20261012.md)
- [`zgc-zs1fp-verifier-type-maps-were-the-unnamed-per-class-share-20261014`](../../internal/gc/zgc-zs1fp-verifier-type-maps-were-the-unnamed-per-class-share-20261014.md)
- [`zgc-zs8rv5-wave-s6-s7-review-residuals-20261015`](../../internal/gc/zgc-zs8rv5-wave-s6-s7-review-residuals-20261015.md)
- [`zgc-relocate-skips-an-unwalkable-survivor-on-native-growth-reclaim-20261014`](../../internal/gc/zgc-relocate-skips-an-unwalkable-survivor-on-native-growth-reclaim-FIXED-20261017.md)
- [`zgc-zs1ca-proposal-per-buffer-announce-gate-20261014`](../../internal/gc/zgc-zs1ca-proposal-per-buffer-announce-gate-20261014.md)
- [`zgc-zs2ca2-proposal-aligned-chunks-biased-bit-address-20261014`](../../internal/gc/zgc-zs2ca2-proposal-aligned-chunks-biased-bit-address-20261014.md)
- [`zgc-zs2mc2-proposal-stealable-reference-array-chunks-20261014`](../../internal/gc/zgc-zs2mc2-proposal-stealable-reference-array-chunks-20261014.md)
- [`zgc-zs3bt-proposal-fresh-receiver-elision-and-loop-held-store-gate-20261014`](../../internal/gc/zgc-zs3bt-proposal-fresh-receiver-elision-and-loop-held-store-gate-20261014.md)
- [`zgc-zs6ob5-proposal-class-metadata-byte-counter-20261015`](../../internal/gc/zgc-zs6ob5-proposal-class-metadata-byte-counter-20261015.md)

**ZGC round zs (from 2026-10-14, open).** ZGC design and performance, GenGC and G1 out of scope,
verified on Linux against HotSpot 25.0.4 `-XX:+UseZGC` in both modes. Records:
[`docs/internal/zgc-round-zs-20261014/`](../../internal/zgc-round-zs-20261014/orch-notes.md)
(`orch-notes.md` is authoritative). Pages it filed are named `zgc-zs<wave><lane>-*`. The per-page
audit of every `zgc-*` page at wave s10 (class, evidence, open items) is
[`s10-pg2-report.md`](../../internal/zgc-round-zs-20261014/s10-pg2-report.md).

Open zs pages (39):
- [`zgc-zs1ca-compiled-allocation-gap-decomposed-20261014`](../../internal/gc/zgc-zs1ca-compiled-allocation-gap-decomposed-SUPERSEDED-20261019.md)
- `zgc-zs1mc-collection-cpu-is-not-accounted-outside-the-mark-20261014` (leaves this folder: t5/pg3 RETIRE (FIXED))
- `zgc-zs1tp-trigger-review-residuals-20261014` (leaves this folder: t5/pg3 structural, recorded)
- [`zgc-zs2ca2-compiled-allocation-loop-is-instruction-bound-20261014`](../../internal/gc/zgc-zs2ca2-compiled-allocation-loop-is-instruction-bound-SUPERSEDED-20261019.md)
- `zgc-zs2fp2-remaining-per-class-metadata-owners-20261014` (leaves this folder: t5/pg3 moves to another owner)
- [`zgc-zs2mc2-mark-engine-review-residuals-20261014`](../../internal/gc/zgc-zs2mc2-mark-engine-review-residuals-FIXED-20261019.md)
- `zgc-zs2nd2-native-door-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- [`zgc-zs2ob-non-heap-memory-pools-are-not-modelled-20261014`](../../internal/gc/zgc-zs2ob-non-heap-memory-pools-are-not-modelled-FIXED-20261019.md)
- `zgc-zs2ob-observability-review-residuals-20261014` (leaves this folder: t5/pg3 structural, recorded)
- `zgc-zs2rf-references-sweep-and-satb-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- `zgc-zs2rv-wave-s1-review-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- `zgc-zs2sl2-slide-review-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- [`zgc-zs3ac-collection-cpu-residual-shares-20261014`](../../internal/gc/zgc-zs3ac-collection-cpu-residual-shares-SUPERSEDED-20261019.md)
- [`zgc-zs3bt-unfused-reference-store-gates-outside-the-ir-putfield-and-aastore-20261014`](../../internal/gc/zgc-zs3bt-unfused-reference-store-gates-outside-the-ir-putfield-and-aastore-FIXED-20261019.md)
- `zgc-zs3cr-relocation-review-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zs3nd3-native-door-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- `zgc-zs3ob2-default-collector-thread-flag-residuals-20261014` (leaves this folder: t5/pg3 moves to another owner)
- [`zgc-zs3ob2-metaspace-pool-residuals-20261014`](../../internal/gc/zgc-zs3ob2-metaspace-pool-residuals-FIXED-20261019.md)
- `zgc-zs3rv2-wave-s2-review-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- `zgc-zs4nd4-launcher-oome-probes-reach-main-through-a-fill-hole-20261015` (leaves this folder: t5/pg3 RETIRE (SUPERSEDED))
- [`zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-20261015`](../../internal/gc/zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-CLOSED-20261019.md)
- `zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight-20261015` (leaves this folder: t5/pg3 structural, recorded)
- `zgc-zs4rv3-wave-s3-review-residuals-20261014` (leaves this folder: t5/pg3 RETIRE (CLOSED))
- [`zgc-zs5nd5-proxy-loader-leak-and-upcall-abort-residuals-20261015`](../../internal/gc/zgc-zs5nd5-proxy-loader-leak-and-upcall-abort-residuals-SUPERSEDED-20261019.md)
- [`zgc-zs5ta-bytes-served-after-a-final-refusal-are-unattributed-20261015`](../../internal/gc/zgc-zs5ta-bytes-served-after-a-final-refusal-are-unattributed-SUPERSEDED-20261019.md)
- [`zgc-zs6ca3-varlen-allocation-gap-is-outside-the-inline-sequence-20261015`](../../internal/gc/zgc-zs6ca3-varlen-allocation-gap-is-outside-the-inline-sequence-CLOSED-20261019.md)
- `zgc-zs6no-soft-references-survive-the-collections-hotspot-stalls-on-20261015` (leaves this folder: t5/pg3 structural, recorded)
- `zgc-zs6ob5-a-set-time-threshold-crossing-waits-for-a-drain-door-20261015` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zs6rs-boot-rss-owners-left-after-s6-20261015` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zs6rv4-wave-s5-review-residuals-20261015` (leaves this folder: t5/pg3 moves to another owner)
- `zgc-zs7jf-jfr-gc-event-residuals-20261015` (leaves this folder: t5/pg3 structural, recorded)
- `zgc-zs7nd6-native-door-residuals-20261015` (leaves this folder: t5/pg3 RETIRE (FIXED))
- [`zgc-zs8nd7-define-mirror-residuals-20261015`](../../internal/gc/zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md)
- [`zgc-zs9la-launcher-oome-rows-a-free-span-remainder-serves-the-receiver-20261016`](../../internal/gc/zgc-zs9la-launcher-oome-rows-a-free-span-remainder-serves-the-receiver-FIXED-20261019.md)
- `zgc-zs9nd8-proxy-row-cpu-and-door-residuals-20261016` (leaves this folder: t5/pg3 moves to another owner)
- [`zgc-zs9rv6-wave-s8-review-residuals-20261016`](../../internal/gc/zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md)

zs proposals (30):
- [`zgc-zs1fp-proposal-mimalloc-first-touch-arena-20261014`](../../internal/proposals/gc/tier2/zgc-zs1fp-proposal-mimalloc-first-touch-arena-20261014.md)
- [`zgc-zs1fp-proposal-verifier-type-maps-on-first-use-20261014`](../../internal/proposals/gc/tier3/zgc-zs1fp-proposal-verifier-type-maps-on-first-use-20261014.md)
- [`zgc-zs1mc-proposal-every-trigger-cpu-attribution-20261014`](../../internal/proposals/gc/tier2/zgc-zs1mc-proposal-every-trigger-cpu-attribution-20261014.md)
- [`zgc-zs1mc-proposal-mark-engine-per-object-costs-20261014`](../../internal/proposals/gc/tier2/zgc-zs1mc-proposal-mark-engine-per-object-costs-20261014.md)
- [`zgc-zs1sl-proposal-concurrent-relocation-with-load-barriers-20261014`](../../internal/gc/zgc-zs1sl-proposal-concurrent-relocation-with-load-barriers-DONE-20261025.md)
- [`zgc-zs1tp-proposal-director-stw-model-20261014`](../../internal/proposals/gc/tier2/zgc-zs1tp-proposal-director-stw-model-20261014.md)
- [`zgc-zs2fp2-proposal-one-vtable-copy-in-the-vm-20261014`](../../internal/proposals/gc/tier3/zgc-zs2fp2-proposal-one-vtable-copy-in-the-vm-20261014.md)
- [`zgc-zs2nd2-proposal-mirror-inside-the-define-20261014`](../../internal/proposals/gc/tier3/zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md)
- [`zgc-zs2ob-proposal-non-heap-pools-from-measured-owners-20261014`](../../internal/proposals/gc/tier3/zgc-zs2ob-proposal-non-heap-pools-from-measured-owners-20261014.md)
- [`zgc-zs2ob-proposal-xlog-gc-star-honest-cycle-lines-20261014`](../../internal/proposals/gc/tier4/zgc-zs2ob-proposal-xlog-gc-star-honest-cycle-lines-20261014.md)
- [`zgc-zs2rf-proposal-reference-discovery-by-class-20261014`](../../internal/proposals/gc/tier2/zgc-zs2rf-proposal-reference-discovery-by-class-20261014.md)
- [`zgc-zs2sl2-proposal-cycle-gate-priced-on-the-selection-20261014`](../../internal/proposals/gc/tier3/zgc-zs2sl2-proposal-cycle-gate-priced-on-the-selection-20261014.md)
- [`zgc-zs2sl2-proposal-evacuate-when-the-tail-has-room-20261014`](../../internal/proposals/gc/tier2/zgc-zs2sl2-proposal-evacuate-when-the-tail-has-room-20261014.md)
- [`zgc-zs2sl2-proposal-evacuated-pages-quarantine-20261014`](../../internal/proposals/gc/tier2/zgc-zs2sl2-proposal-evacuated-pages-quarantine-20261014.md)
- [`zgc-zs3ac-proposal-price-worker-counts-from-the-cpu-shares-20261014`](../../internal/proposals/gc/tier2/zgc-zs3ac-proposal-price-worker-counts-from-the-cpu-shares-20261014.md)
- [`zgc-zs3cr-proposal-evacuate-into-free-pages-below-the-cursor-20261014`](../../internal/proposals/gc/tier3/zgc-zs3cr-proposal-evacuate-into-free-pages-below-the-cursor-20261014.md)
- [`zgc-zs3cr-proposal-quarantine-as-the-forwarding-store-20261014`](../../internal/proposals/gc/tier3/zgc-zs3cr-proposal-quarantine-as-the-forwarding-store-20261014.md)
- [`zgc-zs3fp3-proposal-dispatch-snapshots-on-first-dispatch-20261014`](../../internal/proposals/gc/tier3/zgc-zs3fp3-proposal-dispatch-snapshots-on-first-dispatch-20261014.md)
- [`zgc-zs5bt2-proposal-store-gate-next-steps-20261015`](../../internal/proposals/gc/tier2/zgc-zs5bt2-proposal-store-gate-next-steps-20261015.md)
- [`zgc-zs5mc3-proposal-array-split-granularity-20261015`](../../internal/proposals/gc/tier2/zgc-zs5mc3-proposal-array-split-granularity-20261015.md)
- [`zgc-zs6ca3-proposal-fall-through-inline-arm-and-loop-bench-gate-20261015`](../../internal/proposals/gc/tier3/zgc-zs6ca3-proposal-fall-through-inline-arm-and-loop-bench-gate-20261015.md)
- [`zgc-zs6no-proposal-exhausted-verdict-flip-20261015`](../../internal/proposals/gc/tier3/zgc-zs6no-proposal-exhausted-verdict-flip-20261015.md)
- [`zgc-zs6rs-proposal-compiler-threads-share-one-allocator-heap-20261015`](../../internal/proposals/gc/tier3/zgc-zs6rs-proposal-compiler-threads-share-one-allocator-heap-20261015.md)
- [`zgc-zs9la-proposal-retire-sub-request-spans-at-the-final-refusal-20261016`](../../internal/proposals/gc/tier3/zgc-zs9la-proposal-retire-sub-request-spans-at-the-final-refusal-20261016.md)
- [`zgc-zs9rs2-proposal-boot-big-blocks-off-the-thp-arena-by-default-20261016`](../../internal/proposals/gc/tier3/zgc-zs9rs2-proposal-boot-big-blocks-off-the-thp-arena-by-default-20261016.md)

**GC round gch (2026-10-08 to 2026-10-09, waves h1-h4, CLOSED).** Handoff and next backlog:
[`gch-handoff-gc-round-close-20261009.md`](../../internal/gc/gch-handoff-gc-round-close-RETIRED-20261012.md). Serial-shaped Generational backend and
the common infrastructure, verified on Linux against HotSpot 25. Waves h1-h3 merged into dev as
`3756535896`. Wave h4 adds lanes fl (young sizing, `-Xlog:class+load`), wr (references, weak
tables, the watched set), jm (compiled frames, operand slots), pc (old-gen majors), bp (in-native
deposits, pinning) and g1; G1 internals are in scope from h4 (user, 2026-10-09), ZGC internals stay
out. Its `vh4-*` batteries ran on binaries h4a and h4b. From h4 on no switch is deleted: one that fails its rule
stays registered and default OFF (user triage, 2026-10-09). Records: [`docs/internal/gc-round-gch-20261008/`](../../internal/gc-round-gch-20261008/orch-notes.md)
(`orch-notes.md` is authoritative). Its switch changes are in `docs/GC.md`, section
["Generational: what the gch round changed"](../../GC.md#generational-what-the-gch-round-changed-2026-10-08).
The pages it filed are named `gch-h<wave><lane>-*` and are indexed below in their sections; pages
the round has marked for retirement are not indexed.

Built proposals (status as of 2026-10-16, not re-read):
[`gcd-d10r-proposal-one-relocation-target-set-per-pause-20260928`](../../internal/proposals/gc/tier2/gcd-d10r-proposal-one-relocation-target-set-per-pause-20260928.md)
-- default path, `vi22-dr.list` DR;
[`gcg-g1p-proposal-card-check-counts-its-skip-edges-20261001`](../../internal/proposals/gc/tier3/gcg-g1p-proposal-card-check-counts-its-skip-edges-20261001.md)
-- trace-only, under `CRATONVM_DBG_SP_REF_STORE_TRACE`;
[`gcg-g2r-proposal-watched-set-built-once-per-pause-with-siphash-20261001`](../../internal/proposals/gc/tier2/gcg-g2r-proposal-watched-set-built-once-per-pause-with-siphash-20261001.md)
-- steps 1-2 on the default path (FxHash watched set, i21/ws).

## GC round gcj (Generational + common, 2026-10-19 to 2026-10-20)

Three waves of lanes (j1: 9, j2: 9, j3: 5) on the Generational backend and the common GC infrastructure; G1 and ZGC
out of scope. Rows ran locally (Windows, Temurin 25.0.3 `-XX:+UseSerialGC`, stdout byte-compared). Records:
[`docs/internal/gc-round-gcj-20261019/`](../../internal/gc-round-gcj-20261019/orch-notes.md) (`orch-notes.md` is
authoritative; `LANE-RULES.md`, `triage-j1.md`, the `j<wave>-<lane>-report.md` lane reports). What changed is in
`docs/GC.md`, section "Generational: what the gcj round changed". Fixed pages were retired to `docs/internal/gc/`
with `-FIXED-` / `-CLOSED-` and the date. Open pages (each newest STATUS line says what is left):

- [`gcj-j1pn-pinned-cycle-review-findings-20261019`](../../internal/gc/gcj-j1pn-pinned-cycle-review-findings-FIXED-20261026.md) -- FIXED 2026-10-26 (register-image ledger words rooted for the watched start-word rule; the in-place tail gap committed; items 2-6 and 8 verified on dev)
- [`gcj-j1st-string-table-logging-and-statistics-are-not-hotspots-20261019`](../../internal/gc/gcj-j1st-string-table-logging-and-statistics-are-not-hotspots-FIXED-20261025.md) -- FIXED 2026-10-25 (`-XX:+PrintStringTableStatistics`, `jcmd VM.symboltable`, the `stringtable` / `symboltable` log lines and HotSpot's uncleaned-entry bookkeeping)
- [`gcj-j1yg-jit-warm-young-cycles-still-sweep-under-term-3-and-near-full-heaps-20261019`](../../internal/gc/gcj-j1yg-jit-warm-young-cycles-still-sweep-under-term-3-and-near-full-heaps-RETIRED-20261027.md) -- RETIRED 2026-10-27 (coverage FIXED: the take-over and unproven-frame arms default ON, the young pin deposit cap 2048, a Generational take-over grace of 10 ms; 4-thread javac copies on 96-100 % of its young cycles; the pause half moves to the next entry)
- [`pinned-copy-pause-is-5x-hotspot-serial-on-jit-warm-javac-20261027`](pinned-copy-pause-is-5x-hotspot-serial-on-jit-warm-javac-20261027.md) -- OPEN; round 1 (2026-10-27) halved the pause: parallel card scan, parallel card seed, named roots default ON (`docs/internal/gc-round-gcj-20261019/j1yg-pause-report.md`); round 4 (2026-10-28): 5-7x HotSpot on a 4-vCPU host, the GC CPU itself ~5x (start walk ~30 %); dense-lazy-grid backoff added opt-in (no measured gain), the start walk sized through the walk size memo; round 5 (2026-10-29): precise edges skip the start proof, sorted pointer map with an insert overlay, kept shard/card buffers, parallel survivor sort, elastic pool dispatch, compiles yield to a pause (GC CPU -30 %); still 1.6-2.4x HotSpot on a loaded 16-core host
- [`scoped-young-slide-costs-more-than-it-saves-20261026`](scoped-young-slide-costs-more-than-it-saves-20261026.md) -- OPEN (opt-in `CRATONVM_ZGC_GEN_YOUNG_EVACUATE`); 2026-10-28: the scoped slide no longer registers a remembered set per aged page (24 of 28 ms a slide), the scope build's young walk visits only young bases
- [`gcj-j1yg-oome-shape-probe-intermittently-thrashes-non-moving-cycles-on-dev-20261027`](../../internal/gc/gcj-j1yg-oome-shape-probe-intermittently-thrashes-non-moving-cycles-on-dev-FIXED-20261028.md) -- FIXED 2026-10-28 (a wedged old gen counts as a blocked drain for the futile-young verdict; dev 3/40 runs thrash, the fix 0/40, all `caught=8`)
- [`gen-gcd1-pinned-callee-oome-probe-crawls-unaged-young-sweeps-20261028`](../../internal/gc/gen-gcd1-pinned-callee-oome-probe-crawls-unaged-young-sweeps-FIXED-20261028.md) -- FIXED 2026-10-29 (a non-moving sweep with high survival now arms promote-on-pressure as a moving cycle does; dev 7/30 runs crawl, the fix 0/30, all `PASS`)
- [`gen-oome-chain-fill-full-gcs-are-10x-hotspot-in-cost-and-5x-in-count-20261029`](gen-oome-chain-fill-full-gcs-are-10x-hotspot-in-cost-and-5x-in-count-20261029.md) -- OPEN (chain-fill OOME probes: full GCs ~0.6-1 s against HotSpot's 50-120 ms, and 146 against 22 per run at `-Xmx256m`; stdout SAME)
- [`arg-pin-warm-caller-fails-on-dev-FIXED-20261028`](../../internal/fixed-bugs/arg-pin-warm-caller-fails-on-dev-FIXED-20261028.md) -- FIXED 2026-10-28 (a spliced step's enclosing invoke snapshot kept the outgoing arguments at every spliced pc; `CRATONVM_JIT_IR_KEEP_SET_SPLICED_INVOKE_ARGS_EVERY_PC` default ON)
- [`big-allocation-after-jit-churn-oome-FIXED-20261028`](../../internal/fixed-bugs/big-allocation-after-jit-churn-oome-FIXED-20261028.md) -- FIXED 2026-10-28 (`T1W13BigAfterChurn` and `T1W9 temp-arg` ok on G1, ZGC and Gen: single-pass RAX image, the callee-deopt service copies (rerun window), lazy IR parameter word, ZGC movable pins, G1 dead-word reject pins and fix-only-marked slide)
- [`gcj-j1yg-lazy-object-start-chunk-walk-crossed-a-skip-at-the-chunk-end-20261027`](../../internal/gc/gcj-j1yg-lazy-object-start-chunk-walk-crossed-a-skip-at-the-chunk-end-FIXED-20261027.md) -- FIXED 2026-10-27 (`objstart_chunk`'s skip loop stops at the chunk end; `GenR5W1SoftRefYoungProbe` aborted on every run of dev, now `PASS`)
- [`gcj-j2og2-free-ratio-growth-flip-blockers-20261019`](../../internal/gc/gcj-j2og2-free-ratio-growth-flip-blockers-FIXED-20261027.md) -- FIXED 2026-10-27 (`CRATONVM_GC_OLD_FREE_RATIO_GROWTH` default ON, `=0` kills; inert on the default Serial young geometry, and on the pair split it now also grows after a concurrent cycle: `Gcj1OgFreeRatioGrowthProbe` 69-132 old collections -> 26-31)
- [`gcj-j2pn2-by-name-dispatch-pins-and-door-register-image-keep-dropped-arguments-FIXED-20261027`](../../internal/gc/gcj-j2pn2-by-name-dispatch-pins-and-door-register-image-keep-dropped-arguments-FIXED-20261027.md) -- FIXED 2026-10-29 for the single-pass tier (`Gcd1ArgPinSpecialProbe` `PASS all 6` on Generational / G1 / ZGC with only the single-pass tier compiling: uncached `invokestatic` enters the compiled body, inline-cache hits file their args buffer for the service-copy window, single-pass splices hand their calls' arguments to the callee); P1 and PROMISE stay opt-in; the optimizing tier's `FAIL 6 of 6` is filed on `docs/known-issues/jit/t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers-20261027.md`
- [`gcj-j2pn2-pinning-and-conservative-root-review-residuals-20261019`](../../internal/gc/gcj-j2pn2-pinning-and-conservative-root-review-residuals-FIXED-20261026.md) -- FIXED 2026-10-26 (item 1 built and run on a virtual-thread row, 18/18 SAME; items 2-4 were already fixed on dev by j3/x2 R9 and the j1pn retirements; 5-6 process rules)
- [`gcj-j2rf2-two-empty-stdout-leads-a-hotspot-class-load-flake-and-a-silent-exit-1-20261019`](../../internal/gc/gcj-j2rf2-two-empty-stdout-leads-a-hotspot-class-load-flake-and-a-silent-exit-1-FIXED-20261026.md) -- FIXED 2026-10-26 (battery VOID-HS rule and atomic HotSpot cache; the silent `rc=1` row re-run 20 times on Windows with the full exit code; any such death is now reported by the default-on unhandled-exception filter)
- [`gcj-j2yg2-young-mirror-defer-forces-every-jit-warm-young-cycle-to-sweep-20261019`](../../internal/gc/gcj-j2yg2-young-mirror-defer-forces-every-jit-warm-young-cycle-to-sweep-FIXED-20261026.md) -- FIXED 2026-10-26 (`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` default ON: the deferral no longer forces the in-place sweep; the mirrors are promoted after the tenuring threshold and every JIT-warm cycle reaches the pinned copy)
- [`gcj-j3rf3-ec47-promoted-resurrection-members-and-stretches-still-seed-strongly-20261019`](../../internal/gc/gcj-j3rf3-ec47-promoted-resurrection-members-and-stretches-still-seed-strongly-FIXED-20261026.md) -- FIXED 2026-10-26 (`old_gen_gc_inner`'s legacy-arm root loop defers an old member of the young resurrection set to the resurrection round, so a promoted member no longer marks its old children strongly; the unparseable-stretch arm is by design)
- [`gcj-j3rf3-windows-unhandled-exceptions-outside-the-fatal-list-die-silently-20261019`](../../internal/gc/gcj-j3rf3-windows-unhandled-exceptions-outside-the-fatal-list-die-silently-FIXED-20261026.md) -- FIXED 2026-10-26 (`CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT` default on: the full fatal report for every unhandled code off the fatal list; the exit status stays the exception code)
- [`gen-hashmap-grow-throws-oome-at-xmx256m-where-hotspot-completes-20261026`](../../internal/gc/gen-hashmap-grow-throws-oome-at-xmx256m-where-hotspot-completes-FIXED-20261027.md) -- FIXED 2026-10-27 (the dropped maps were held by the optimizing tier's OSR body -- the arm's `OsrLocal` and a pre-merge phi kept across the loop's calls, `CRATONVM_JIT_IR_KEEP_SET_OSR_LOCALS_ARM_ONLY` / `..._PHI_REACH_KILLS` -- and by the in-place majors' legacy young seed, `CRATONVM_GC_MOVING_MAJOR_IN_PLACE_PER_CYCLE_ARMS`; all default ON)
- [`native-growth-reclaim-probe-fails-on-g1-20261027`](../../internal/gc/native-growth-reclaim-probe-fails-on-g1-FIXED-20261028.md) -- FIXED 2026-10-28 (five holders: stale parked-peer words, `CRATONVM_G1_PEER_PINS_SKIP_POST_BLOCK_REGIONS` / `CRATONVM_G1_CLEANUP_IGNORES_PEER_STACK_WORDS` / `CRATONVM_G1_PEER_PINS_OBJECT_STARTS_ONLY`; the single-pass tier's RAX image at helper calls and stack guards, `CRATONVM_JIT_RAX_DEAD_AT_CALL`; the last-ditch staged compaction cutting the run the recovery pause freed, `CRATONVM_G1_STAGED_COMPACTION_SPARES_FITTING_REQUEST`; all default ON)
- [`pinned-and-kept-survivors-never-age-20261026`](../../internal/gc/pinned-and-kept-survivors-never-age-FIXED-20261028.md) -- FIXED 2026-10-28 (Generational pinned and kept young survivors age in place on the serial and parallel arms and enter the age table only while below the tenuring age, HotSpot's AgeTable rule; `CRATONVM_GEN_PINNED_COPY_AGES_IN_PLACE` default ON, `=0` kills)
- [`gcj-j3yg3-young-collection-residuals-20261019`](../../internal/gc/gcj-j3yg3-young-collection-residuals-FIXED-20261026.md) -- FIXED 2026-10-26 (R2 as j2yg2 above; R3 HotSpot Serial's young geometry, `CRATONVM_GEN_SERIAL_YOUNG_GEOMETRY` default ON: young collections within 1.04-1.12x of HotSpot under `-Xmn`, `NewRatio` and the default)

Retired on 2026-10-25 (gcj rt3/x2 residuals: queued finalizables are strong roots on Generational and G1,
native threads own their death, the leaf-window drain releases the barrier lock):
[`gcj-j3rt3-runtime-review-residuals-FIXED-20261025`](../../internal/gc/gcj-j3rt3-runtime-review-residuals-FIXED-20261025.md),
[`gcj-j3x2-wave-j2-default-on-review-residuals-FIXED-20261025`](../../internal/gc/gcj-j3x2-wave-j2-default-on-review-residuals-FIXED-20261025.md).

Retired on 2026-10-25 (the three j2/jt2 barrier switches and og2's N4 / N5 measured and default ON,
`WARNING gcj-j2-residuals-flip-20261025`):
[`gcj-j2jt2-post-only-card-mark-flip-residuals-FIXED-20261025`](../../internal/gc/gcj-j2jt2-post-only-card-mark-flip-residuals-FIXED-20261025.md),
[`gcj-j2og2-old-gen-review-residuals-FIXED-20261025`](../../internal/gc/gcj-j2og2-old-gen-review-residuals-FIXED-20261025.md).

Retired on 2026-10-22 (the launcher's `-XX:` answers come from HotSpot 25's own flag table on every
collector; `jcmd VM.stringtable` / `VM.flags` are HotSpot's shapes):
[`launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-FIXED-20261022`](../../internal/gc/launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-FIXED-20261022.md),
[`gcj-j1ob-launcher-typed-value-residuals-FIXED-20261022`](../../internal/gc/gcj-j1ob-launcher-typed-value-residuals-FIXED-20261022.md),
[`gcj-j2ob2-launcher-and-serviceability-review-residuals-FIXED-20261022`](../../internal/gc/gcj-j2ob2-launcher-and-serviceability-review-residuals-FIXED-20261022.md),
[`gcj-j3x2-jcmd-stringtable-verbose-is-not-hotspots-FIXED-20261022`](../../internal/gc/gcj-j3x2-jcmd-stringtable-verbose-is-not-hotspots-FIXED-20261022.md).

Retired on 2026-10-25 (an unknown `-XX:` name is refused as HotSpot 25 refuses it,
`CRATONVM_LAUNCHER_UNKNOWN_XX_REFUSAL`; the young-size flags print HotSpot's `[gc,ergo]` lines,
`CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`; no flag row disagrees with its reader):
[`launcher-hotspot-flag-table-residuals-FIXED-20261025`](../../internal/gc/launcher-hotspot-flag-table-residuals-FIXED-20261025.md),
[`gcj-j3x2-flag-rows-whose-off-word-disagrees-with-the-reader-FIXED-20261025`](../../internal/gc/gcj-j3x2-flag-rows-whose-off-word-disagrees-with-the-reader-FIXED-20261025.md).

## Opt-in switches waiting for a flip rule (or a user decision)

Built behind a default-OFF switch (or kept OFF by the zr triage); the page names the switch and its rule.

User decisions pending (see the round close-out):
- [`common-w14d-proposal-ephemeron-native-side-tables`](../../internal/proposals/gc/tier2/common-w14d-proposal-ephemeron-native-side-tables.md)
  -- `CRATONVM_GEN_SIDE_ROW_EPHEMERON`: flipped default ON 2026-10-17 (flips B); the name-keyed JUL
  tables follow it since i24/lk.
- [`gch-h4fl-proposal-serial-eden-follows-the-initial-heap-20261009`](../../internal/proposals/gc/tier2/gch-h4fl-proposal-serial-eden-follows-the-initial-heap-20261009.md)
  -- `CRATONVM_GEN_SERIAL_EDEN_INITIAL` stays OFF; the ergonomic initial heap it follows is now the
  default on every collector (`CRATONVM_GC_HOTSPOT_INITIAL_HEAP`, WARNING `dc72abfc64`); with `=0`
  (the 16 MiB start) 0/64.

G1:
- [`gcg-g2w-proposal-weak-string-table-on-g1-and-zgc-20261001`](../../internal/proposals/gc/tier2/gcg-g2w-proposal-weak-string-table-on-g1-and-zgc-20261001.md)
  -- G1 half behind `CRATONVM_G1_WEAK_STRING_TABLE` (`vi21-sw.list` FLIP RULE).
- [`gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009`](../../internal/proposals/gc/tier2/gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009.md)
  -- `CRATONVM_G1_REMARK_ADDR_KEYED_MARK_EXACT`.

Generational, JIT and common:
- [`gcg-g2w-proposal-string-table-keyed-by-the-string-20261001`](../../internal/proposals/gc/tier2/gcg-g2w-proposal-string-table-keyed-by-the-string-20261001.md)
  -- steps 1-2 behind `CRATONVM_GC_STRING_TABLE_FUNNEL`.
- [`gch-h4bp-proposal-narrow-the-in-native-band-to-the-chain-20261009`](../../internal/proposals/gc/tier2/gch-h4bp-proposal-narrow-the-in-native-band-to-the-chain-20261009.md)
  -- `CRATONVM_JNI_IN_NATIVE_BAND_NARROW`.
- [`gch-h1jw-proposal-in-native-arrival-without-waiting-20261008`](../../internal/proposals/gc/tier2/gch-h1jw-proposal-in-native-arrival-without-waiting-20261008.md)
  -- keep opt-in; flip only with the JNI package (now ON).
- [`gch-h1yy-proposal-refill-takes-the-smallest-whole-free-block-20261008`](../../internal/proposals/gc/tier2/gch-h1yy-proposal-refill-takes-the-smallest-whole-free-block-20261008.md)
  -- `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`; `vi22-dr.list` YY.
- [`common-a-proposal-per-thread-handshakes`](../../internal/retired/common-a-proposal-per-thread-handshakes-RETIRED-20261027.md)
  -- steps 1-2 behind `CRATONVM_GC_HANDSHAKE_FRAME_TRACE`.
- [`common-b-proposal-dedupe-the-initiator-root-set`](../../internal/proposals/gc/tier3/common-b-proposal-dedupe-the-initiator-root-set.md)
  -- step 11 behind `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`.
- [`common-w36b-proposal-reference-free-frames-skip-the-band-scan`](../../internal/proposals/gc/tier3/common-w36b-proposal-reference-free-frames-skip-the-band-scan.md)
  -- `CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS`; `vi23-rf.list`.
- [`gengc-r5w2-roots6-proposal-generation-keyed-jit-scan-memo-20260926`](../../internal/proposals/gc/tier3/gengc-r5w2-roots6-proposal-generation-keyed-jit-scan-memo-20260926.md)
  -- `CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED`; `vi23-r6.list`.
- [`gcf-f1p-proposal-lean-inline-card-check-20261001`](../../internal/proposals/gc/tier2/gcf-f1p-proposal-lean-inline-card-check-20261001.md)
  -- item 1 behind `CRATONVM_JIT_LEAN_CARD_POST`.
- [`gcg-g3e-proposal-post-resurrection-closure-scans-only-what-the-round-marked-20261001`](../../internal/proposals/gc/tier3/gcg-g3e-proposal-post-resurrection-closure-scans-only-what-the-round-marked-20261001.md)
  -- `CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL`; `vi28-cl.list`.
- [`gcd-d10t-proposal-takeover-probe-skips-reserved-tlab-tails-20260928`](../../internal/proposals/gc/tier2/gcd-d10t-proposal-takeover-probe-skips-reserved-tlab-tails-20260928.md)
  -- `CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS`.
- [`gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection-20260928`](../../internal/proposals/gc/tier3/gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection-20260928.md)
  -- `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION`.
- [`gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries-20260927`](../../internal/proposals/gc/tier2/gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries-20260927.md)
  -- implemented opt-in (gcg), awaiting triage.
- [`gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928`](../../internal/proposals/gc/tier3/gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928.md)
  -- `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`, decided with the pinned copy (default ON since 2026-10-13).
- [`gengc-r5w4-jit8-proposal-zero-fresh-frames-on-every-tier-20260926`](../../internal/proposals/gc/tier2/gengc-r5w4-jit8-proposal-zero-fresh-frames-on-every-tier-20260926.md)
  -- KEEP (d8 triage).
- [`common-w21e-proposal-coverage-slot-without-the-wait`](../../internal/proposals/gc/tier3/common-w21e-proposal-coverage-slot-without-the-wait.md)
  -- KEEP OFF (zr triage: sound, no gain on one VM).
- [`common-w26c-proposal-sub-linear-freed-span-drops`](../../internal/proposals/gc/tier2/common-w26c-proposal-sub-linear-freed-span-drops.md)
  -- KEEP OFF (zr triage; no TLS-heavy cost row).
- [`gcf-f1r-proposal-settled-reference-rows-leave-the-per-pause-walks-20261001`](../../internal/proposals/gc/tier3/gcf-f1r-proposal-settled-reference-rows-leave-the-per-pause-walks-20261001.md)
  -- KEEP OFF (zr triage; perf not measured).
- [`gcg-g1s-proposal-oop-map-records-the-spill-images-written-set-20261001`](../../internal/proposals/gc/tier3/gcg-g1s-proposal-oop-map-records-the-spill-images-written-set-20261001.md)
  -- KEEP OFF (zr triage; no gain row).

## Proposals (not built)

Proposals are not defects: they belong in
[`docs/internal/proposals/gc/`](../../internal/proposals/gc/README.md); the pages listed below still
sit in this folder (2026-10-17) until they move.

Lanes may file proposals; they do not build one unless their brief says so (`lane-rules.md`). The
gci round ranked the older ones in
[`proposal-rank-i10.md`](../../internal/gc-round-gci-20261011/proposal-rank-i10.md) and triaged their
switches in
[`proposal-flip-triage-20261015.md`](../../internal/gc-round-gci-20261011/proposal-flip-triage-20261015.md).
Filed by the gci round in its defects-only waves, awaiting the user's triage:
- [`gci-i45sc-proposal-tlab-refill-waste-limit-per-fill-20261016`](../../internal/proposals/gc/tier2/gci-i45sc-proposal-tlab-refill-waste-limit-per-fill-20261016.md)
  -- `TLABWasteIncrement` consumer and the per-refill waste limit (changes every thread's default path).
- [`gci-i20jt-proposal-lockfree-quiet-leave-for-every-flagged-region-20261015`](../../internal/proposals/gc/tier2/gci-i20jt-proposal-lockfree-quiet-leave-for-every-flagged-region-20261015.md)
  -- waits on the JNI half's verdict (`vi20-jt.list` LL).
- [`gci-i30gv-proposal-runner-exhaustion-requeues-a-resumed-slice-20261015`](../../internal/proposals/gc/tier3/gci-i30gv-proposal-runner-exhaustion-requeues-a-resumed-slice-20261015.md)
- [`gci-i30hz-proposal-marker-drops-covered-flagless-grays-without-retain-all-20261015`](../../internal/proposals/gc/tier3/gci-i30hz-proposal-marker-drops-covered-flagless-grays-without-retain-all-20261015.md)
- [`gci-i30ob-proposal-per-vm-xlog-logger-handle-20261015`](../../internal/proposals/gc/tier4/gci-i30ob-proposal-per-vm-xlog-logger-handle-20261015.md)
- [`gci-i30rp-proposal-kept-region-owner-lists-filtered-by-young-span-20261015`](../../internal/proposals/gc/tier3/gci-i30rp-proposal-kept-region-owner-lists-filtered-by-young-span-20261015.md)
- [`gci-i30wy-proposal-pinned-page-rule-roots-only-what-a-word-can-name-20261015`](../../internal/proposals/gc/tier3/gci-i30wy-proposal-pinned-page-rule-roots-only-what-a-word-can-name-20261015.md)

Older proposals (newest STATUS in brackets):
- [`common-d-proposal-marker-discovered-references`](../../internal/proposals/gc/tier3/common-d-proposal-marker-discovered-references.md) -- ZGC half narrowed by reading (zr), not built.
- [`common-w2g-proposal-blocked-threads-replay-pause-maps`](../../internal/proposals/gc/tier2/common-w2g-proposal-blocked-threads-replay-pause-maps.md) -- narrowed (gcg): only the measurement half landed.
- [`gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot-20260927`](../../internal/proposals/gc/tier3/gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot-20260927.md) -- item 1 confirmed, items 2-3 not built.
- [`gcd-d1c-proposal-reference-rows-pruned-at-every-reclamation-20260927`](../../internal/proposals/gc/tier2/gcd-d1c-proposal-reference-rows-pruned-at-every-reclamation-20260927.md) -- narrowed (gcg).
- [`gcd-d2g-proposal-tag-root-occurrences-by-rewritability-20260927`](../../internal/proposals/gc/tier2/gcd-d2g-proposal-tag-root-occurrences-by-rewritability-20260927.md) -- frames step refuted and removed; open.
- [`gcd-d3m-proposal-blocked-deposits-stand-in-the-young-pin-ledger-20260927`](../../internal/proposals/gc/tier2/gcd-d3m-proposal-blocked-deposits-stand-in-the-young-pin-ledger-20260927.md) -- not built.
- [`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928`](../../internal/proposals/gc/tier2/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md) -- open (band B).
- [`gcd-d9d-proposal-no-call-site-rerun-from-entry-20260928`](../../internal/proposals/gc/tier3/gcd-d9d-proposal-no-call-site-rerun-from-entry-20260928.md) -- open; the weak doors stay opt-in.
- [`gcg-g1f-proposal-frozen-peer-frames-read-with-their-band-claims-20261001`](../../internal/proposals/gc/tier3/gcg-g1f-proposal-frozen-peer-frames-read-with-their-band-claims-20261001.md) -- step 2 not built (tiny population).
- [`gcg-g4og-proposal-concurrent-initial-mark-seeds-only-root-reached-young-20261002`](../../internal/proposals/gc/tier2/gcg-g4og-proposal-concurrent-initial-mark-seeds-only-root-reached-young-20261002.md) -- open, band C; manual census owed.
- [`gch-h3mj-proposal-moving-true-root-major-under-compiled-frames-20261008`](../../internal/retired/gch-h3mj-proposal-moving-true-root-major-under-compiled-frames-20261008-RETIRED-20261027.md) -- inventory done, steps 2-3 not built.
- [`gch-h4bp-proposal-band-at-pause-on-g1-and-zgc-20261009`](../../internal/proposals/gc/tier3/gch-h4bp-proposal-band-at-pause-on-g1-and-zgc-20261009.md) -- its G1 half exists as `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1` (default ON since vi6-g1r); the page's STATUS is older and the 2026-10-15 inventory called it retire-ready.
- [`gengc-r5w1-crash5-proposal-owner-checked-raw-root-writes-20260926`](../../internal/proposals/gc/tier3/gengc-r5w1-crash5-proposal-owner-checked-raw-root-writes-20260926.md) -- band C hardening, no population.
- [`gengc-r5w1-young5-proposal-grid-probe-next-steps-20260926`](../../internal/proposals/gc/tier3/gengc-r5w1-young5-proposal-grid-probe-next-steps-20260926.md) -- P4 only, no population.
- [`gengc-r5w4-defaults8-proposal-per-backend-defaults-for-g1-and-zgc-20260926`](../../internal/proposals/gc/tier2/gengc-r5w4-defaults8-proposal-per-backend-defaults-for-g1-and-zgc-20260926.md) -- G1 share sizer undecided by reading; `vi27-gs.list` GA.
- [`gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927`](../../internal/proposals/gc/tier2/gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927.md) -- narrowed (two readings).
- [`gengc-r5w6-pin10-proposal-young-copy-round-three-20260927`](../../internal/proposals/gc/tier1/gengc-r5w6-pin10-proposal-young-copy-round-three-20260927.md) -- items 1, 2, 4 in the tree; 3 and 5 open.
- [`jit-r15w5-gcpage-proposals`](../../internal/proposals/gc/tier4/jit-r15w5-gcpage-proposals.md) -- GP-1 open; GP-3 step 1 opt-in.
- [`jit-r21-nio-proposals-w17-L3`](../../internal/proposals/gc/tier4/jit-r21-nio-proposals-w17-L3.md) -- blocked-thread root deposits (triage).
- [`jit-r21-nio-proposals-w18-L5`](../../internal/proposals/gc/tier4/jit-r21-nio-proposals-w18-L5.md) -- blocking-native deposits, `--jdk-only` boot order (triage).
- [`gci-i39rw-proposal-metaspace-figures-from-the-incremental-tally-20261016`](../../internal/proposals/gc/tier4/gci-i39rw-proposal-metaspace-figures-from-the-incremental-tally-20261016.md)
  -- not built.

## External and out of scope

- [`zgc-tlab-refill-ignores-the-object-floor-20261012`](../../internal/gc/zgc-tlab-refill-ignores-the-object-floor-FIXED-20261019.md)
  -- ZGC only; the exact edit is on the page.
- [`jit-r21-nio-proposals-w10-L1`](../../internal/proposals/gc/tier4/jit-r21-nio-proposals-w10-L1.md),
  [`jit-r21-nio-proposals-w9-L3`](../../internal/proposals/gc/tier4/jit-r21-nio-proposals-w9-L3.md) -- ZGC inline-allocation proposals.

## Opt-in switches and their flip gates

The current state of every GC switch is in `docs/GC.md`, by round: the gci sections ("what the gci
round changed", "what gci waves i15-i18 changed", "what gci waves i19-i36 changed"), the G1 switch
table under "Verifying and debugging", then the gch, gcg and gcf sections. Those tables are
authoritative; they are not repeated here. The 2026-10-15 triage of every default-off GC switch
(FLIP / NEEDS-ROWS / KEEP-OFF-FAILED / ...) is
[`proposal-flip-triage-20261015.md`](../../internal/gc-round-gci-20261011/proposal-flip-triage-20261015.md).
Older opt-ins that predate the gcf round and still wait for their gate are in
[`triage.md`](../../internal/gc-defects-round-20260927/triage.md), section "Final triage, wave d8",
and on each page. A switch that failed its rule stays registered and default OFF; from gch h4 on no
switch is deleted (user triage, 2026-10-09).

**Never as a default** (keep as embedder options or bisection levers, or
remove):

| Switch(es) | Why |
|---|---|
| `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` + `CRATONVM_GC_OLD_INTERIOR_DECOMMIT` | They return memory HotSpot Serial keeps (`GenR4W6OldShrinkProbe` prints `FAIL` on HotSpot); a G1-style footprint option for embedders |
| `CRATONVM_JIT_LOCAL_HANDLER_CLEAR_DEAD`, `CRATONVM_GC_STAGED_ARGS_KEEP_MASK` | No probe needs them on d7. (`CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS` and `CRATONVM_JIT_OSR_DROP_ORPHANS` left this row: dev made both default ON, `=0` kills; see the gcg g1-s report.) |

Switches removed by gce e2 (`CRATONVM_GC_PAR_EVAC_TARGETED_WAKE`,
`CRATONVM_GC_PAR_EVAC_OLD_PLAB_HINT`, `CRATONVM_GEN_EVAC_SCAN_PREFETCH`,
`CRATONVM_GC_PAR_EVAC_BUFFERLESS`, `CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES`,
`CRATONVM_TLAB_SIZE_RETIRED`, `CRATONVM_TLAB_WASTE_SHRINK`, `CRATONVM_GEN_PRISTINE_CHUNKS`,
`CRATONVM_GC_NATIVE_LATCHED_MAJOR`, `CRATONVM_GEN_REQUESTED_MAJOR_NO_PROMOTE`,
`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG`) no longer exist. From gch h4 on no switch is
deleted (user triage, 2026-10-09).
