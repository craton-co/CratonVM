# Garbage Collection in CratonVM — architecture and current state

> **STATUS (2026-10-13, GC round "gci", waves i1-i13).** Generational backend and the common
> infrastructure, G1 from wave i5g on (user, 2026-10-12); ZGC and proposals out of scope. The
> user flipped every pending decision ON on 2026-10-13 (sites marked
> `WARNING gci-user-flip-20261013`, `=0` kills each): old-generation compaction on requested and
> moving majors, the Serial-eden young trigger with HotSpot's age clock, the pinned young copy
> with its parallel arm and ledger guard, helper-window root starts, and the JNI native
> transitions on every backend. About 25 more switches are default ON on their verdicts or as
correctness fixes
> (P-C true-root majors, precise-root promote, the IR dead-home clears, the G1 in-native band,
> live-walk DCMDs, interpreter type-map locals, ...), and about fifty opt-ins were added, none
> deleted. Text further down that names any of these as opt-in predates the round. The list and
> the deciding rows: "Generational and G1: what the gci round changed" under "Backend details
> worth knowing"; G1 opt-ins are rows of the G1 switch table. Records:
> `docs/internal/gc-round-gci-20261011/` (`orch-notes.md` is authoritative).

> **STATUS (2026-10-09, GC round "gch", waves h1-h4).** Serial-shaped Generational backend and
> the common infrastructure; G1 internals are in scope from wave h4 (user, 2026-10-09), ZGC
> internals stay out. Waves h1-h3 merged into dev as `3756535896`: nine switches default ON
> (among them `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` and
> `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS`, which earlier tables below list as opt-in), four
> deleted, about a dozen opt-ins added. Wave h4 (lanes fl, wr, jm, pc, bp, g1) turns five more
> defaults ON (two of them G1's) and adds nine opt-ins; its `vh4-*` batteries are pending. No switch
> is deleted from h4 on (user triage, 2026-10-09). The list and the deciding rows: "Generational:
> what the gch round changed" under "Backend details worth knowing". Records:
> `docs/internal/gc-round-gch-20261008/` (`orch-notes.md` is authoritative).

> **STATUS (2026-10-01, end of the GC design and performance round, "gcf").**
> - **Scope.** Seven waves on Generational and the shared infrastructure. G1 and ZGC internals
>   were out of scope.
> - **Verification.** Five verification batteries (`vf1`-`vf5`, 340-471 rows each) against
>   HotSpot 25 on all three collectors. The f6 and f7 batteries come on top.
> - **New defaults, each with `=0` as its kill switch:**
>   - `CRATONVM_GEN_SATB_OLD_ONLY`, `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` and
>     `CRATONVM_GEN_CONC_INLINE_START`;
>   - per-kind preallocated OOMEs (`CRATONVM_GC_PREALLOCATED_OOME_KINDS`);
>   - HotSpot-shaped adaptive tenuring (`CRATONVM_GC_ADAPTIVE_TENURING`);
>   - the JNI allocation ladder (`CRATONVM_GC_JNI_ALLOC_LADDER`);
>   - dead-reference-home clearing in optimizing-tier frames
>     (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`);
>   - the young-allocation-premium cuts (`CRATONVM_GEN_WALK_SIZE_MEMO`,
>     `CRATONVM_GEN_SWEEP_BATCH_CENSUS`, `CRATONVM_GEN_SWEEP_3A_REPLAY`);
>   - per-drain class-unloading side-table lookups.
> - **Six dead switches were deleted.** About 30 opt-ins were added; each awaits a decision.
> - **Where to read more.** The changes: "Generational: what the gcf round changed" under
>   "Backend details worth knowing". Text further down that names these switches as opt-in
>   predates this round. Summary: `docs/internal/gc-design-perf-round-20261001/README.md`. Open
>   items: `docs/internal/gc/gcf-handoff-gc-design-perf-round-close-DONE-20261001.md`.

> **STATUS (2026-09-29, end of the GC design and performance round, "gce").** Two waves
> on Generational and the shared infrastructure, verified on Linux on all three collectors
> (the default probe battery: 93 of 116 rows match HotSpot 25 Serial, against 90 at wave e1).
> Defaults the round changed, each with `=0` as its kill switch:
> `CRATONVM_GEN_CONC_PRECEDENCE`, `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` and
> `CRATONVM_GEN_PROMOTE_LIVE_FINALIZABLES` (on); the concurrent class-unloading set
> `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, `CRATONVM_GEN_CONC_CLASS_UNLOAD`,
> `CRATONVM_GEN_Y2O_LIVE_SEED` and `CRATONVM_GEN_YOUNG_MIRROR_DEFER` (on: the concurrent
> cycle now processes references at remark and unloads classes); `CRATONVM_GC_LATCHED_GRACE`
> (on: a latched GC-overhead exit takes one graced allocation attempt before throwing);
> the Linux take-over signals only peers with a compiled frame
> (`CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY`) after a 200 us cooperative grace
> (`CRATONVM_XT_FIRST_PASS_GRACE_US`), and parked peers sleep instead of spinning. Eleven dead
> switches were removed (the list is in the round summary). Text below that names those
> switches as opt-in describes the state before this round. Summary:
> `docs/internal/gc-design-perf-round-20260929/README.md`; open items:
> `docs/internal/gc/gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-28, end of the GC defects round).** The Generational
> backend and the common infrastructure are brought up to the round's final
> verification (the round branch at `307f0c6a2`, Linux release, against
> HotSpot 25 `-XX:+UseSerialGC`: the standing battery reads 84 SAME / 32 DIFF,
> against 48 DIFF at the round's start, and no row became DIFF). See
> "Generational: what the GC defects round changed" under "Backend details
> worth knowing": one opt-in flipped to default on
> (`CRATONVM_GC_OLD_HUMONGOUS_TOP`; the `CRATONVM_GC_OLD_PINNED_COMPACT` flip was
> reverted on its verification run), six new
> default-on fixes with kill switches, the OOME ladder's second major, the
> opt-in JNI in-native transitions, new `[GC]` summary lines, and the triage
> of every opt-in switch and proposal. The compiled-`monitorenter` crash the
> previous pass listed as open is fixed, and two of the three dev probe
> regressions now pass on Generational. G1 and ZGC were out of the round's
> scope; their text is as the passes below left it. The round's summary is
> `docs/internal/gc-defects-round-20260927/README.md`.
>
> **Previous pass (2026-09-26, gen r5w4/obs8).** The Generational backend section
> was brought up to date with generational rounds 4 and 5 (through wave 3,
> `897210f83`, plus wave 4's observability lane): see "Generational: what
> rounds 4 and 5 changed" under "Backend details worth knowing" — the default
> flips (TLAB tail sink, concurrent start from compiled code, concurrent-first
> with hysteresis, old-gen shrink and OOM compaction), stack bands that stop
> below static TLS, the state of the memory-corruption family, the opt-in
> features awaiting a default decision, and the HotSpot-Serial-shaped
> `Runtime` / pool / threshold / `-Xlog` model. The rest of the page is as
> the previous pass below left it.
>
> **Previous pass (2026-09-26, re-read against `b5c9b6c6e`).** This page describes
> the code as it is after the GC common-infrastructure round (waves 1-34,
> `docs/internal/gc-common-round-20260923/README.md`) and the merge of dev's
> 8-byte object header. What changed since the last full pass: the three
> baseline-battery failures this page listed (`ThreadLocalFinalizeProbe`, the
> G1 `GcOverheadProbe` abort, class-loader unloading) and the dying JDK
> `Finalizer` thread are fixed; every GC door runs the one shared pause; the
> ZGC JMX beans and the compressed-oops admission of every VM are applied; the
> identity hash is 20 bits per class since the 8-byte header, which is why
> native side tables are keyed as "Native side tables" below says. Still open,
> and the ones a user is most likely to meet: old-generation finalizables on
> Generational are never finalized, allocation-driven collections can keep a
> weak referent a compiled loop keeps loading, and three probes that
> regressed on dev (a heap OOME from ZGC fragmentation, no GC notifications
> under `--compatible`). Everything
> open is in [`known-issues/gc/`](known-issues/gc/); see its `README.md`.
> Claims marked *not re-verified* below were not checked against the current
> code in this pass.

CratonVM has three garbage-collector backends behind one dispatcher
(`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world at the collection
level; G1 additionally runs its marking phase concurrently. Selection is
java-launcher-compatible:

| Flag | Backend | One-liner |
|---|---|---|
| `-XX:+UseGenerationalGC` / `-XX:-UseZGC` | `GenerationalHeap` (`gc/src/gen_heap.rs`) | Semi-space young gen + free-list old gen with a concurrent old-gen mark-sweep cycle. Young collections are **moving by default**; each cycle diverts to the non-moving sweep only when its own root-coverage proof fails (see "Backend details" below). |
| `-XX:+UseG1GC` | `G1Collector` (`gc/src/g1.rs`) | Region-based (region size targets ~2048 regions, clamped to 1-32 MB): young/mixed evacuation with remembered sets, SATB concurrent marking, humongous spans, region pinning. The heap is **reserved** at `-Xmx` and committed on demand. |
| *(default)* / `-XX:+UseZGC` / `-XX:+UseZ` | `ZgcRealHeap` (`gc/src/zgc.rs`) | **Not a real ZGC**: a memory-backed, whole-heap mark-sweep over one arena with a bitmap allocation registry. No colored pointers, and **the load barrier is never armed** — `relocate_active` and `set_barrier_color(Some(..))` are written only by unit tests, so marking is published by an ordinary **SATB pre-write barrier** rather than on read. It is not non-compacting. **Default ON:** a stop-the-world sliding compactor (`CRATONVM_ZGC_RELOCATE`, default ON since 2026-08-13, `=0` restores the non-moving sweep), parallel by default (`CRATONVM_ZGC_PAR_RELOCATE`), which runs on every cycle whose per-cycle coverage proof holds. **Default ON since zr z3/mt:** parallel STW marking (`CRATONVM_ZGC_PARMARK`, unset = `auto`, HotSpot's pause-worker ergonomic or `-XX:ParallelGCThreads`; `=0` restores the serial loop). **Opt-in:** concurrent marking (`CRATONVM_ZGC_CONC_START=<percent>` or `auto`) and a generational mode (`CRATONVM_ZGC_GENERATIONAL=1`). So a DEFAULT run is a stop-the-world whole-heap mark (parallel; serial on a small heap) and sweep, followed by a parallel sliding compaction. The colored-pointer/`ZPage` code above it in `zgc.rs` (and `zgc_concurrent.rs`) has production consumers now — `forwarding::ZRelocationSet::select` ranks the slide's pages — though the barrier layer itself remains unreached. |

Unrecognized `-XX:+Use*GC` selectors warn and fall back to Generational.
Heap size comes from `-Xmx`/`-Xms` as usual, and on every backend those two
mean what they do on HotSpot: `-Xmx` is the size of the address-space
*reservation* and `-Xms` is the memory committed at startup. An `-Xms` above
`-Xmx` is clamped rather than refused.

**`-Xms` is honoured on all three backends** since 2026-09-21. G1 got it first
(F-16); ZGC commits the startup budget across both ends of its single arena, and Generational
commits the request across its two young semi-spaces and (since 2026-09-23)
the remainder as an old-generation prefix. Until 2026-09-20 the two
non-G1 backends accepted the flag and dropped it *in silence*, on the default
collector — `VmHeap::new_with_heap_sizing` now makes each backend arm state
what it did with the value (`XmsDisposition`), which is what stops that
returning. See
[`internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md`](internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md).

Two things `-Xms` deliberately does **not** do. It never sizes a *reservation*:
on ZGC the arena envelope is captured once and read without the arena lock by
`conservative_addr_span`, the object-start bitmap and the JIT read-bounds
publication, so an `-Xms` that shrank it would produce a heap that can never
reach `-Xmx`. On Generational the young pair takes the request first and the
old generation commits the remainder as a prefix, so the startup commit is
`-Xms` (rounded up to 2 MiB granules). *(Until 2026-09-23 the old generation
was a `Vec` committed in full at construction, so the startup commit there was
at least `-Xmx / 2` whatever `-Xms` said.)*

`VmHeap::committed_bytes()` — `Runtime.totalMemory()`, the JMX heap
`committed`, the `(NNNM)` of an `-Xlog:gc` line — is the heap's **committed**
size since 2026-09-23, HotSpot's meaning: `-Xms64m -Xmx512m` reports about 64m
and grows from there. Until that date it was a sum of *capacities* on
Generational and ZGC, so `totalMemory()` was `-Xmx` from the first
instruction on the default collector (`XmsProbe` printed `total=512m` against
HotSpot's `total=64m`). Per backend: Generational counts the young pair's
committed granules plus the old generation's touched prefix
(`OldGen::high_water`); G1 its committed region prefix; ZGC its committed
granules. ZGC preserves `-Xms` across every decommit path, with per-VM
`-XX:+/-ZUncommit` and `-XX:ZUncommitDelay=<seconds>` (default 300).
Its startup budget covers both allocation ends, so `XmsProbe -Xms64m -Xmx512m`
reports `total=64m` in both runtime modes.
See [`the resolved ZGC floor issue`](internal/gc/zgc-decommit-ignores-xms-FIXED-20261001.md).
(G1's first parallel young pause committed the whole reservation until
2026-09-30:
[`g1-parallel-survivor-claim-commits-the-whole-reservation`](internal/gc/g1-parallel-survivor-claim-commits-the-whole-reservation-FIXED-20260930.md).)
`VmHeap::os_committed_bytes()` is the OS-accounting twin: the same on G1 and
ZGC, and on Generational it counts the old generation's whole `Vec`.

What the other two backends do with `-Xmx` is **not** "allocate all of it up
front", which is what this paragraph used to claim:

* **ZGC** builds one `Arena`, and `Arena` sits on `reservation.rs`'s
  `HeapStore`, which reserves the address space and commits in 2 MiB granules
  as the allocator's cursors reach them. A fresh `-Xmx8g` ZGC process commits
  megabytes, not gigabytes.
* **Generational** is half and half by default (`-Xmn` / `-XX:NewRatio` move
  the split — `gc-tuning.md`, "Young generation size") and reserves the same
  way, all three spaces *(since
  2026-09-23; before that the old generation was a `Vec<u8>` of its whole
  share — half of `-Xmx` — and on Windows took that full commit charge at
  startup)*. The two young semi-spaces are `Arena`s; the old generation
  (`OldGen`) sits on the same `HeapStore` and commits a granule when its
  free-list allocator first hands out storage in it. A refused commit is an
  allocation failure, i.e. the ordinary promotion-failure / `OutOfMemoryError`
  path. The old generation gives pages back only when a compaction
  (`CRATONVM_OLDGEN_COMPACT`, default ON since the user's flip of
  2026-10-13; `=0` kills it) empties whole granules; the in-place sweep
  keeps its pages resident.
* `CRATONVM_GC_RESERVE=0`, and any reservation the OS refuses, put every arena
  back on the wholly-committed `alloc_zeroed` block, where all of `-Xmx` *is*
  charged up front.

**ZGC is the default**, and the `zgc` Cargo feature is default-ON (it gates
the `GcAlgorithm::Zgc` variant, so the default cannot be `Zgc` without it). It
is still **not a real ZGC** — everything the table above says about it holds.
The promotion is backed by measured suite behaviour, not maturity: across
every suite with a per-collector sweep (Tomcat, Spring Framework, H2,
Hibernate Reactive), ZGC is at parity with or ahead of Generational on PASS
count, ties or leads on HANG, and has never crashed.

Two consequences worth stating plainly:

* **It costs heap — but not by a known factor.** The collector compacts only
  on cycles whose coverage proof holds (`CRATONVM_ZGC_RELOCATE`, default ON).
  A workload whose cycles keep falling back to the non-moving sweep fragments
  like a non-compacting collector and needs more headroom. How much is a
  property of the workload's allocation shapes. Raise `-Xmx` to get moving after a
  post-flip `OutOfMemoryError`, and file it: every known instance of that
  shape so far has turned out to be an allocator bug rather than an inherent
  ZGC cost.
* **`-XX:+UseGenerationalGC` is the escape hatch**, in every build. A
  `--no-default-features` build has no ZGC at all and defaults to Generational.

The plan to make this a real, concurrent, generational, compacting ZGC is
[`internal/zgc-round-20260920/zgc-roadmap-20260920.md`](internal/zgc-round-20260920/zgc-roadmap-20260920.md),
which sequences the four current direction documents into one ordered programme,
states the dependency edges between them, and lists what in
[`zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md)
and
[`zgc-maturity-assessment-and-plan-20260813.md`](internal/zgc-round-20260920/zgc-maturity-assessment-and-plan-20260813.md)
has since gone out of date. Read the roadmap first; those two remain the
authority on their own history.

## The VM ↔ GC protocol

**Stop-the-world.** A GC-initiating thread posts a request on the
`GcBarrier` (`vm/src/threading/gc_barrier.rs`); mutators park at
interpreter safepoint polls (allocation sites and backward branches),
each depositing a **root snapshot** first. Threads inside blocking
natives are excluded from the arrival quota and covered by their
deposited snapshot plus a wake-time fixup (`check_post_block_gc`).
Threads stuck in compiled code that never polls are handled by the
**cross-thread JIT takeover** (INT-3, all backends): the initiator freezes
them at OS level (Windows `SuspendThread`, Linux a signal rendezvous) and
conservatively scans their registers, machine stack and shadow stack, and a
post-barrier **helper-window** pass does the same for blocked threads that
have compiled frames below the blocking call. A shadow-stack entry tagged as
an INDIRECT slot is dereferenced (and, after a moving pause, written through)
only if the slot lies in the owning thread's stack band — its stack pointer up
to the OS thread's stack top (`gc/src/shadow_stack.rs`, `SlotBand`, since
2026-09-24): an odd primitive the JIT published in a reference home reads as
`slot | INDIRECT_TAG`, and used to crash the pause or corrupt memory when the
remap wrote through it (`ShadowOddLongProbe`). Refused entries are counted
(`rejected_indirect_entries`). The JIT producer -- a dead reference local
whose register another local shared -- is no longer published since
2026-09-26, so the count should read zero; the containment stays
([`common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive`](internal/gc-common-round-20260923/common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive-FIXED-20260926.md)).
Un-retired TLAB tails are
published as walker skip regions. What protects the frozen state then differs
by backend: Generational and ZGC do not move on such a cycle (`XT_TAKEOVER`
diverts / refuses), while G1 evacuates and relies on pinning the regions the
scanned words name — since 2026-09-23 a word pointing INTO an object is
resolved to that object's base too (`CRATONVM_XT_TAKEOVER_INTERIOR`, default
on, `=0` restores exact bases only; the runtime matrix is recorded and the page
retired,
[`common-c-takeover-probe-drops-derived-pointers`](internal/gc-common-round-20260923/common-c-takeover-probe-drops-derived-pointers-FIXED-20260923.md)).
The one licence all three read is the table below. The takeover also covers
the four
concurrent-mark STW pauses, and a cross-thread `Thread.getStackTrace()` pause
freezes but gathers no roots. `--verbose:gc` prints the engagement census at
exit: `[GC] xt_peer_scan:`, `[GC] xt_peer_shadow:` (blocked peers) and
`[GC] xt_takeover_shadow:` (frozen peers, since 2026-09-23), and
`[GC] ttsp: pauses= avg_us= max_us= last_us= max_straggler=` for
time-to-safepoint. That exit line aggregates EVERY stop-the-world pause the
barrier ran — collections and the non-collection pauses (concurrent-mark
STW phases, cross-thread stack walks, heap dumps) alike — so its `pauses=`
is at least, and usually more than, the number of `[GC] pause:` lines.
`max_straggler=` (since 2026-09-23) is the registry `ThreadId` the slowest
pause waited for last — the numbering of `CRATONVM_DBG_STW_CENSUS=1`'s
`[stw-arrive] tid=` lines — or `none` when that pause's quota was met with
no participating arrival (no peer, or the last peers were frozen in
compiled code and excused by the take-over). Per pause (since 2026-09-23, every
backend and every door) it prints `[GC] pause: gc=N door=<alloc-threshold|
alloc-failure|system-gc|...> ttsp_us= straggler= xt_pass_us= xt_post_quota_us= collect_us= xt_passes=
xt_frozen= xt_unclassified= xt_roots= hw_windows= hw_roots=` (since gci i8/gs an occupancy
verdict taken through the forced door -- the compiled TLAB refill trigger, and on G1 the JIT
array headroom guards -- prints `door=alloc-threshold`; `door=alloc-failure` is an allocation
that failed; both are HotSpot's "Allocation Failure". Since gci i11/go G1's last-ditch
compaction prints `door=g1-compaction`): that pause's
time-to-safepoint (request accepted to every counted mutator parked or taken
over, HotSpot's "Reaching safepoint"), the take-over's post-quota work
(`xt_post_quota_us`, since 2026-09-23 wave 4: the frozen peers'
interpreter-frame walk, the helper-window pass over blocked peers with JIT
frames, and the TLAB skip-span publish — work that runs after the quota is met
and before the collection starts, so neither neighbouring figure contains it;
a few microseconds when no peer was frozen and no blocked peer had JIT frames,
0 when the take-over is disabled), the collection's own time (roots,
collection, reference processing, remap — sealed inside the pause, before the
release), and the take-over's
coverage. `ttsp_us + xt_post_quota_us + collect_us` is the pause from request
to release, less only the `CRATONVM_DBG_*` pre-collection verifiers. A slow
pause with a large `ttsp_us` is a slow safepoint, not a slow collector; a
large `xt_post_quota_us` is the take-over's frame walk or helper-window pass.
`xt_pass_us` (since 2026-09-24, gc-common w6-f) is the take-over passes' own
time (OS suspends, register and stack copies, the word probe). The passes run
inside the barrier wait, so it is PART of `ttsp_us`: `ttsp_us - xt_pass_us` is
the time spent waiting for polling threads. It is 0 exactly when
`xt_passes=0`.
`gc=N` is the VM's collection ordinal — the same number as the
`-Xlog:gc` line's `GC(N)` and the JFR events' `gcId`, 0-based (since gci i12/ge a G1
concurrent cycle takes its own id at its initial mark, so later G1 ids are one higher per
cycle, as on HotSpot G1); the
collectors' own per-collection lines count separately (Generational's
`cycle=` is its 1-based minor-collection count), so pair them by adjacency,
not by number. `ttsp_us` is on neither `-Xlog:gc` nor JFR (HotSpot reports
it under `-Xlog:safepoint` and `jdk.SafepointBegin`, neither of which this
VM emits yet), and the `-Xlog:gc` / JFR duration, like HotSpot's, excludes
it. The printed `collect_us`, the `-Xlog:gc` duration and the JFR duration
are one instant, stamped inside the pause
([`common-w4f-gc-event-durations-sampled-after-release`](internal/gc-common-round-20260923/common-w4f-gc-event-durations-sampled-after-release-FIXED-20260923.md)).
`straggler=` names the thread this pause waited for last, or `none`. The
non-collection pauses print the same line with `gc=-`, a `door=` naming the
pause (`g1-remark`, `g1-initial-mark`, `gen-remark`, `gen-initial-mark`,
`zgc-mark-start`, `frame-trace`, `heap-dump`, `loop-exit` — the pause that
makes an OSR'd loop leave compiled code when a JVMTI agent or a debugger
needs the interpreter) and `work_us=` in place of
`collect_us=`, so every entry of the exit `[GC] ttsp:` aggregate has a
per-pause line.
Not yet on the line: the split of `xt_post_quota_us` into the frame walk and
the helper-window pass
([`common-c-proposal-takeover-cost-in-the-pause-line`](internal/gc-common-round-20260923/common-c-proposal-takeover-cost-in-the-pause-line-FIXED-20260923.md)).
JFR safepoint events and `-Xlog:safepoint` were declined for the GC-common
round
([`common-a-proposal-ttsp-reporting-FIXED-20260923`](internal/gc-common-round-20260923/common-a-proposal-ttsp-reporting-FIXED-20260923.md)).

**The take-over licence: one contract, three consumers (since 2026-09-23).**
Every pause's take-over ends by publishing ONE `TakeoverVerdict`
(`gc/src/gc_quiescence.rs`, written at the end of `stw_take_over_and_wait`,
reset when the next pause opens): peers `frozen`, blocked peers with a
`helper_window`, peers `unreadable` (neither parked nor classifiable), whether
the pin set is complete (`pins_complete`: no frozen stack read short, every
helper window pinned) and whether it resolves derived pointers
(`derived_pointers_resolved`: `CRATONVM_XT_TAKEOVER_INTERIOR`, default on, for
frozen peers; `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE`, also default on, for
windows).
`may_move_under_takeover(backend_can_pin)` turns it into a `MoveLicence`:
`Move` when nothing was frozen, no helper window was found and every peer was
read; `NonMoving` when a peer was unread, or when the backend cannot honour a
pin set at all; otherwise `MovePinned` if the pin set is complete and resolves
derived pointers, else `NonMoving`. What each backend does with it:

| backend (asks with) | `Move` | `MovePinned` | `NonMoving` | read at |
|---|---|---|---|---|
| Generational (`backend_can_pin = false`) | moving young (Cheney copy) | never returned: a Cheney copy cannot honour a pin, so any take-over pause is `NonMoving` | the whole young cycle diverts to the non-moving sweep | `gen_heap.rs` young divert, `takeover_forbids_unpinnable_move()` (always) |
| G1 (`true`) | evacuate | evacuate; the regions the scanned words name are pinned out of the collection set | default: evacuate on the same pins (an unread peer, or a pin set that is incomplete or exact-base only, is then not covered); `CRATONVM_G1_TAKEOVER_LICENCE=1`: refuse evacuation for the pause | `g1.rs` young/mixed pause, `g1_takeover_licence_refusal()` (opt-in until its refusal rate is measured) |
| ZGC (`true`) | slide (other coverage reasons permitting) | no slide: a take-over marks `XT_TAKEOVER` / `XT_HELPER_WINDOW`, which page pins cannot cover (`UNPINNABLE_COVERAGE_REASONS`, measured) | no slide, and no page-pinned relocation either | `ZgcRealHeap::coverage_incompleteness_is_page_pinnable` (always, as an extra refusal next to the coverage mask) |

So the three backends answer "is a frozen peer safe?" from the same record;
they differ only in how much a `MovePinned` licence buys them. Generational and
ZGC never move under a frozen peer; G1 does, protected by region pins.
(Generational's pinned young copy, default ON since the user's flip of 2026-10-13, can move
under a blocked peer's helper window with the opt-in
`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1`; see "Generational:
what the GC defects round changed".) How often a frozen compiled peer degrades a
pause's licence was measured at the end of the GC defects round: in about
0.14 % of take-over pauses (76 of 52 557 over the d7 battery's
`[GC] xt_peer_scan:` lines); only a probe built to freeze a spinner goes above
1 %. The helper-window discharge verdict is a row of the pause ledger, like
the other take-over counts (gcd d2/i), so it is per VM.

**Several VMs in one process (embedders, the `vm` test binary).** The
per-pause take-over and coverage rows in `gc_quiescence.rs` are fields of each
VM's `PauseLedger`: the take-over verdict (gc-common w10-g), the peer capture
buffers (w18-c), and since w21-e the six moving-young coverage rows and the
helper-window pins (`CoverageCycle`). A write from a thread bound to no ledger
lands in process-wide orphan rows that every read also consults, so it is
never lost. Since gc-common w36-c the young pin-word ledger is per VM too
(`PauseLedger::young_pins`); an unbound young pin-word write is dropped and
counted, and fails the open pause's read (fail-closed), so no per-pause row is
a process static any more
([`common-a-process-global-gc-coordination-state`](internal/gc-common-round-20260923/common-a-process-global-gc-coordination-state-FIXED-20260926.md),
fixed pending one `orphan_coverage_writes=0` probe run). Since
2026-09-24 (gc-common w6-a) a pause takes a process-wide **coverage slot**
(`COVERAGE_SLOT_OWNER`, `vm/src/threading/gc_barrier.rs`) before its barrier
lock and the per-pause reset, and `complete_gc` releases it, so another VM's
pause can no longer reset those rows under a pause that is using them. One VM
never waits (one compare-exchange per pause); a second VM's request waits for
the first's pause, outside every lock, for at most 250 ms, and then pauses
without the slot (the pre-w6 behaviour), counted and reported under
`CRATONVM_DBG_STW_CENSUS=1` as `[stw-coverage-slot] … gave up`. The
allocation front end's state is per VM since the same wave (gc-common w6-c):
`ThreadMXBean.getTotalThreadAllocatedBytes()` sums only this VM's threads
(`HeapRealm::thread_allocated_total`), and the TLAB refill wedge breaker's
counters (`HeapRealm::tlab_wedge`) cannot be moved by another VM's
refills. The JMX GC beans' counts, times and notification queues are per heap
too (gc-common w7-f), and so is the `System.gc()` coalescing counter
(`HeapRealm::system_gc_collections`, gc-common w7-g). One thing cannot be per
VM: the **reference width**. `-XX:+UseCompressedOops` (opt-in, Generational
only) switches `cratonvm_types::narrow_oop` on for the whole process, so every
VM in it runs 4-byte reference slots. Since gc-common w8-f every VM's heap goes
through `compressed_oops::admit_heap` at init: the first VM fixes the width;
a later VM whose heap lies outside the narrow-oop window the first one fixed,
that did not ask for compressed oops, or whose collector is not audited for
them is **refused** (VM creation panics with the reason) instead of being
handed that window unchecked; and once any VM runs wide, a later
`-XX:+UseCompressedOops` falls back to 64-bit references with a message
(`vm_init.rs`, the `admit_heap` call with `backend_audited` true only for
Generational; `common-w7f-compressed-oops-second-vm-skips-the-fit-check`,
FIXED 2026-09-24 with its `vm_init` handoff applied).

**Roots and remap.** `vm/src/memory/roots.rs::collect_roots` gathers the
root families in its numbered steps 1 to 22b (frames, statics, class
locks/mirrors, string pool, JNI global+local refs, JNI pin set, thread
mirrors, the JIT's conservative and shadow-stack roots, the uniform
native-root registry of step 21, pending queued `Reference`s held by a dead
local in step 22, …); each has a paired write-back in
`vm/src/memory/gc.rs::update_all_roots` applying the collection's
pointer map. Parked threads are remapped on resume
(`apply_pointer_map_to_thread`), woken blocked threads via a composed
multi-GC fixup. Conservative JIT-frame roots cannot be rewritten, so
under G1 their regions are **pinned out of the collection set**
(the per-thread pin registry in `gc/src/gc_quiescence.rs`, whose per-pause
rows live in each VM's `PauseLedger` — see the paragraph above);
the Generational collector instead diverts the whole cycle to its
non-moving young sweep, which moves nothing at all. See the
`divert_non_moving` table under "Backend details" for the exact term and
its opt-out — this paragraph and that section disagreed outright between
2026-07-28 and 2026-09-20, one saying a live JIT frame always forces the
sweep and the other saying it never does by itself.

**Native side tables and native pins.** A native that keeps per-object
state in a Rust table must not key it by a raw address or a bare identity
hash. The address moves; the hash is shared by the Nth object of every VM
and is reused inside one heap. Since the 8-byte object header (below) a
compact instance's hash is 20 counter bits plus 11 class-id bits
(`ObjectHeader::short_hash_value`, `types/src/heap_types.rs`), so two live
instances of ONE class share a hash every 2^20 (about a million) mints,
not every 2^31. Since the gc-common round (2026-09-23 .. 26) every such table
is per VM (`VmScoped`, or a `vm` component in the key), is dropped at VM
teardown through `forget_vm_native_root_stores`
(`native-builtins/src/lib.rs`), and takes one of two routes.

**Route R1, the weak lock key.** A table in `native-builtins` keys its row by
the object's weak lock key (`gc_stable_weak_lock_key`,
`native-builtins/src/lib.rs`). The lock-key registry remaps the key after a
move and frees it when the object dies, handing the freed keys to each
table's `forget_*_keys` hook. The registry never gives one live object's key
to another with the same hash (gc-common w25-a removed the "lone occupant"
step that did).

**Route R2, a hash bucket with an owner compare.** `native-io`, `native-awt`
and `native-collections` cannot reach the lock-key registry. There the hash
is only a bucket: every row carries its owner (its current address, a JNI
weak global, or the object in a rooted row) and is compared on lookup.

**Weak global roots.** A JNI weak global is what
`NativeContext::add_weak_global_root` (`vm/src/vm/vm_exec.rs`) hands out: it
keeps nothing alive, `resolve_global_root` answers `None` once a collection
has found the object dead, and `remove_global_root` must still release it.
Dead weak globals are cleared by the epilogue sweep
(`jni::sweep_weak_global_refs_in_place`) and, during a Generational
concurrent sweep, per freed span (`jni::clear_weak_global_refs_in_spans`).

**Memos validated by a hash.** A memo that checks "is this still the same
object?" by comparing identity hashes answers for a same-class replacement
once per 2^20 mints. It must also compare the object's address within one
`gc_collection_count()` epoch, or compare the content it describes, or key
the row by the weak lock key (gc-common w33-a / w34-a). The last such lookup,
the JIT's VarHandle plan, names the handle's address and VM since w36-a
([`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`](internal/gc-common-round-20260923/common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement-FIXED-20260926.md),
row 5).

**Loader-conditional root sources.** A row that holds a user class's object
(a `ClassValue` value, an `AbstractClassLoaderValue` value, a
`TypeVariable`, a `Package`) must not keep that class's loader alive through
a JNI global root. Such rows are reported through a per-VM root source in
`vm/src/memory/native_roots.rs` (`class-values`, `classloader-values`,
`type-variables`, `defined-packages`; also `class-atomic-slots`,
`annotation-proxies`, `indy-call-sites`, `osc-cache`). Under the cycle's
licence (`roots::loader_metadata_licence`) and when the heap says the marker
follows metadata pins for that address (`VmHeap::metadata_pin_deferrable`),
`defer_or_root` pins the row to its loader instead of rooting it, so it dies
with the loader; a built-in-loader or unknown owner, and every cycle without
the licence, is rooted. On Generational that predicate is true for an
old-generation address, and, since gc-common w36-d, for a young one on a
cycle certain to take the non-moving young marker (an explicit `System.gc()`
among them). That marker follows `metadata_pin` from young owners and seeds
it from old ones (`seed_metadata_pins_of_old_owners`). A young value on a
moving young cycle is still rooted
([`common-w2b-generational-young-classvalue-values-root-their-class`](internal/gc-common-round-20260923/common-w2b-generational-young-classvalue-values-root-their-class-FIXED-20260926.md)).

Tables that must be judged by the heap are swept after each collection, at
three sites (the stop-the-world epilogue in `gc_and_alloc.rs` and the two
`addr_keyed.rs` sweeps), in one fixed order: Locale, TLS, logging, lock keys,
native-io, httpserver links, net sockets, zip rows, SubmissionPublisher rows,
Undertow rows. The epilogue judges survival through one
`addr_keyed::InPlaceVerdict` per pause (a per-row verdict made Generational
collections quadratic in the garbage).

Inside a native, an `ObjectRef` is valid only until the next allocation or
Java call. Root it with `pin_native_root` / `NativeHandleScope` and read it
back with `read_native_pin` / `scope.get`. `unpin_native_roots(base)`
**truncates** the thread's pin stack, so releasing an older pin releases every
newer one, and a later `read_native_pin` of those silently answers its stale
fallback: `common-w20v` wrote a FileSystem's fields into a vacated ZGC span
that way.

Source gates watch these shapes. Three run in CI, in
`.github/workflows/stale-receiver-audit.yml`, each after its own selftest;
the figures are what `python scripts/<name>.py` prints on `b5c9b6c6e`:

- `scripts/stale-receiver-audit.py`: `matching the shape 190`,
  `WITH call sites that reuse the receiver: 1 fn(s), 10 site(s)` (that is
  `pe_segment_check_scope`, the one row of
  `scripts/baselines/stale-receiver-sites.txt`), `ok — no new
  stale-receiver sites`. A match is not a defect; the gate fails only if the
  population grows. (This list used to say the baseline is empty; it is not.)
- `scripts/pin-stack-order-audit.py`: `ok -- no pin is read after an older
  pin released it` (empty baseline);
- `scripts/identity-hash-key-audit.py`: `ok -- 152 identity_hash_code
  call(s) in 133 reviewed function(s)`. Every `identity_hash_code(` call in
  the native crates is on an exact per-function allow-list with its reason
  (a Java-visible hash, a bucket whose rows compare their owner, a stripe, a
  validated memo, debug output); a new call, or a changed count, fails. The
  allow-list was re-read against the 2^20 same-class period in gc-common
  w33-a.

Two more are run by hand, not in CI:

- `scripts/stale-handle-across-alloc-audit.py`: a ratchet over
  `scripts/baselines/stale-handle-across-alloc-sites.txt`: `106 site(s)
  across 92 function(s)`, `ok — nothing grew`. The population was triaged in
  gc-common waves 19-20 and re-baselined since;
- `scripts/unpinned-native-local-audit.py <glob>`: the per-crate
  unpinned-local finder behind the 2026-08-25 audit; it takes the files to
  scan and has no baseline.

The test mocks (`native-builtins/src/test_utils.rs`,
`native-io/src/test_support.rs`) panic on a read of a released pin.

**The 8-byte object header (dev, 2026-09-24; `658fddf3c` and its
follow-ups).** Every object starts with one header word: the class id at 0
and a 32-bit mark word at 4 whose top two bytes carry the kind, element type,
GC flags (`OLD_GEN`, `MARKED`, `COMPACT`, `HEADER`) and age. A compact
instance has nothing else, so its fields start at 8; arrays and legacy
(16-byte `Value` cell) instances keep a second word holding the shape and a
31-bit identity hash. The layout contract is
[`architecture/compact-object-and-field-layout.md`](architecture/compact-object-and-field-layout.md).
What it changed for the collectors:

- **Sizing.** A compact instance has no field count in its header; every
  collector sizes it from its class's current compact layout
  (`ObjectHeader::num_slots`, read through a per-thread field-count cache
  since `e01202d60`). A class whose layout can change while instances exist
  (a compatibility stub and its subclasses) is never compact.
- **Forwarding.** The forwarding target is the object's second word
  (`FORWARDING_TARGET_OFFSET = 8`; every object is at least
  `MIN_OBJECT_SIZE` = 16 bytes). Parallel evacuators claim with one CAS of
  the mark word to `FORWARDED | BUSY`, write the target, then clear `BUSY`
  (`try_claim_forwarding` / `publish_claimed_forwarding`,
  `types/src/heap_types.rs`); a self-forward writes only the mark word.
- **Monitors.** An inflated monitor is no longer named by the header; it is
  found by address in the VM's sharded monitor index, and every moving
  collector re-keys that index (`MonitorCleanup::remap_after_gc`,
  `gc/src/collector.rs`).
- **Identity hash.** A compact instance's hash is 20 counter bits in the
  NEUTRAL mark word plus 11 class-id bits; a hashed compact object inflates to
  lock. This is the 2^20 same-class period the native side-table rules above
  are written against.
- **Interior roots.** `obj + 8` of a compact instance decodes as a plausible
  header, so an address that did not come from a reference field — a
  conservative root from a JIT register image — must carry `GC_FLAG_HEADER`
  before it is treated as an object. G1 pins such a root's region instead of
  evacuating it (`2341fc6f6`); before that it wrote `FORWARDED` into the real
  object's forwarding word.

Of the three probe regressions that dev brought in with or beside this change,
one still fails on Generational; the other two pass there (not re-run on G1
or ZGC; see "Current correctness state").

**Write barriers.** Reference stores fire an SATB pre-barrier (old value
logged to per-thread buffers spilling into a sharded queue) when a mark
cycle is active, and G1's remembered-set post-barrier. On Generational and G1
"active" means the collector's `SatbQueue` is accepting logs
(`SatbQueue::is_active()`); the phase is not consulted, and the queue holds
the compiled-code pre-gate's count for exactly that window (gc-common w5-e). Both are internal
to the `VmHeap::set_field`/`set_array_element` accessors, so every
interpreter/native/JIT-helper store is covered by construction; the JIT's
inline ref-store fast paths bail to the helper unless the backend
publishes region bounds (only Generational does). Statics live in a
Rust-side table and fire the pre-barrier centrally in
`set_static_shared`.

**java.lang.ref.** Weak/Soft/Phantom semantics are driven by the VM around
each collection: every active weak/phantom referent slot, and every soft slot
the LRU policy condemns, is nulled pre-GC so the tracer cannot keep the
referent alive; the shared `ReferenceProcessor` (`gc/src/reference.rs`)
decides clear/enqueue/finalize with a backend-exact liveness predicate; and
post-GC the processor restores ONLY the slots the pre-GC pass itself nulled
whose referent survived (since 2026-09-23 — a slot the program nulled with
`Reference.clear()` is no longer written back, and the row is retired so it is
never enqueued). Queue linkage uses the Reference's real `next` field. A
`Reference` object is never a synthetic GC root: an unreachable one is
garbage and is never enqueued, as on HotSpot. The one exception is a pending
queued `Reference` still held in a liveness-dead local of the collecting
thread's interpreter frame, which is kept so `r = new WeakReference<>(x, q);
System.gc(); q.remove()` delivers as it does in HotSpot's interpreter
(`roots.rs` step 22). Finalizable objects dead in a collection are
**resurrected** (evacuated/marked with their subtree) so `finalize()` runs
against valid memory; every GC door queues them through the one shared pause
([`common-e-gc-doors-disagree-on-dead-finalizers`](internal/gc-common-round-20260923/common-e-gc-doors-disagree-on-dead-finalizers-FIXED-20260923.md)).
An object becomes finalizable however it was created: `new`, the JNI
`NewObject*` family, and since 2026-09-24 `Object.clone()` of a
finalizable object (HotSpot's `JVM_Clone` registers the clone;
`tools/probes/CloneFinalizeProbe.java`), and `Finalizer.register` registers a
finalizer row, not a cleaner one.
Since gc-common w36-d, Generational's old-generation collectors resurrect
dead finalizables too. The stop-the-world ones (`major_gc_finalizing`,
`sweep_old_gen_non_moving_finalizing`) run an `OldGenFinalizerPass` round
after their root closure. The concurrent cycle's remark retains dead
published candidates, and the next pause reports them. Until then a
finalizable that died after promotion was freed without `finalize()`, and
`tools/probes/OldGenFinalizeProbe.java`'s `System.gc()` arm read 0/64; it now
reads 64/64. The probe's allocation-driven arm still reads 0/64. That run
performs no old-gen collection of any kind, because old gen stays at 0.5 %
occupancy, so the promoted objects are never judged. HotSpot shows the same
result whenever it promotes them (Serial `-Xmn32m`: 0/64). Default G1 passes
only because its large young generation never promotes them. HotSpot Serial
at the page's own configuration prints 64/64 for the same reason, and 0/64
at CratonVM's 32 MiB young size, so the page was retired as not a defect
([`common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED-20260928`](internal/gc-common-round-20260923/common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED-20260928.md),
gcd d1/c).
The same rounds report the depth-2 resurrection closure
(`VmHeap::take_resurrection_closure`) on Generational, so a weak reference to
an object reachable only through a finalizable is cleared, as HotSpot does.
G1 (Phase 3.5's fresh evacuations) and ZGC (the resurrection pass's marks)
report it too
([`common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot`](internal/gc-common-round-20260923/common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot-FIXED-20261010.md)).
Queued finalizers run inline on the thread that drains them, except that a
thread holding a user-visible lock (a monitor or an ownable synchronizer)
hands them to a per-VM daemon, "Craton Finalizer", started on first such use
(JLS §12.6) — that is the default, `CRATONVM_FINALIZER_THREAD` unset
(`WhenLockHeld`); `CRATONVM_FINALIZER_THREAD=1` hands it every finalizer,
cleaner action, `ReferenceQueue` wake-up and GC notification, `=0` restores
the inline drain everywhere. Under `--jdk-only` the default policy also gives
the daemon the collector's `ReferenceQueue` wake-ups and every GC notification
(a listener never runs on an application thread, as with HotSpot's
`Notification Thread`); `--compatible` keeps both inline. Where the daemon takes a caller's finalizers,
`System.gc()` and `Runtime.runFinalization()` wait for it (at most 2 s, less
if it is seen blocked on the caller's own monitor); a lock-holding caller then
returns whether or not they ran, and never runs them itself. Under
`--jdk-only` (since 2026-09-24) the `ReferenceQueue` wake-ups of every
collection go to that daemon by default too (setting unset; `=0` still records
none) — the JDK's own `ReferenceQueue.remove` sleeps on its lock and nothing
else wakes it — and
`Reference.waitForReferenceProcessing()` waits for the daemon under the full
hand-off (`false` otherwise); `--compatible` is unchanged. The per-setting
table is `docs/gc-tuning.md`, "Finalizer and reference delivery". Under G1,
references whose referents die in uncollected Old regions are processed at
concurrent-mark completion against the mark bitmap (INT-8: referent-slot hiding
during marking + a `Reference.get()` keep-alive barrier + remark-time
processing callback). `System.gc()` runs a finalizer-aware collection and also
drives the G1 concurrent cycle forward.

**What the dispatcher adds to every collection (2026-09-23).** Both public
entry points, `VmHeap::collect_garbage` and
`VmHeap::collect_garbage_with_finalizers`, now do these things, so a backend
cannot opt out by omission:

* fire the JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish` hooks
  for G1 and ZGC (Generational fires its own pair at its two entry points, so
  the dispatcher skips it — `jvmti_gc_hooks_fired_here`). Until this date the
  only callers were the Cheney collector of the non-selectable semi-space
  `Heap`, so a JVMTI agent received neither event on any real backend;
* on ZGC, wait for the direct GPU critical-section tokens
  (`wait_for_gpu_critical_drain`, `gpu-offload` builds) that Generational and
  G1 already waited for at their own entry — the default collector used not
  to, so a direct `VmHeap::enter_gpu_critical` token protected a kernel's
  arrays on two backends and not on the third;
* on ZGC, record a collector decision
  (`gc_metrics::record_collector_decision`): `moving-backend-compacted` when the
  slide ran, `nonmoving-coverage-incomplete` (with the failed obligation) when
  the coverage proof refused it, `nonmoving-gpu-critical-section` on a GPU veto,
  `nonmoving-backend-has-no-young-copy` otherwise — so
  `collector_decision_report()` and its histogram are no longer empty on the
  default backend;
* (unchanged) bump the FFM epoch and splice GPU keep-alive roots.

**SoftReference pressure on G1 (fixed 2026-09-23).** The LRU soft-reference
policy's free-heap figure (`VmHeap::soft_ref_policy_free_mb`) read `0` MB on
G1 whatever the occupancy — it counted only the unused tails of regions that
were currently Eden or Old, never the Free regions — so the LRU threshold
(`SoftRefLRUPolicyMSPerMB × free MB`) was 0 ms and every *softly*-reachable
referent was cleared at every G1 pause, which is HotSpot's behaviour only at
the brink of `OutOfMemoryError`. G1 and ZGC now report whole-heap headroom
(`heap_capacity - allocated`, HotSpot's `LRUMaxHeapPolicy` figure);
Generational keeps its deliberately pessimistic young/old minimum; since gcd
d4/n its young term counts the young free list too, so after non-moving
sweeps it no longer reads the bump cursor alone (which made
`GenR4SoftRefLruProbe` fail with adaptive tenuring on;
`internal/gc/gcd-d4n-soft-policy-free-mb-ignores-the-young-free-list-FIXED-20260928.md`).

`System.gc()` (`force_gc_from_native`) now **retries** when it loses the STW
race instead of returning after merely taking part in another thread's
(possibly young) pause: up to four attempts, each of which either collects or
lets a competing pause finish. Before, the full collection it asked for did not
happen, and its thread-local major-GC request leaked into that thread's next,
unrelated collection. A thread that sees a pause already in flight no longer
opens a new coverage cycle on its way to discovering it lost — that reset used
to erase the in-flight pause's per-pause coverage verdicts and JIT pins
(`maybe_gc`, `force_gc_from_native`; since gc-common w2-a the
allocation-failure door `maybe_gc_forced_at` runs the same shared pause,
`run_collection_pause`, so it no longer opens the coverage cycle before its
request either).

Concurrent `System.gc()` calls **coalesce** since 2026-09-24 (gc-common w7-g),
by HotSpot's `skip_operation` rule, per VM: a caller reads the VM's
`System.gc()`-door collection count before its first attempt, and after a
LOST attempt returns as soon as one such collection has completed since —
N threads calling together see fewer `door=system-gc` pause lines than calls,
and `getCollectionCount()` moves more slowly under contention (H2's
`while (prev == cur) System.gc();` still terminates: someone collected). A
caller in its own blocked region never coalesces. Residual: on G1 a coalesced
call still runs `g1_force_full_cycle`, so N concurrent calls give one pause but
can give more than one marking cycle (throughput only).

## Current correctness state

A systematic review, backed by a deterministic differential probe kit diffed
against HotSpot JDK 25, found and fixed: G1 humongous accounting/reclaim
(IHOP-blind humongous, decay-to-zero IHOP, last-ditch full cycle before
OOM), TLAB gap-sentinel desync in every G1 region walker, initiator-only
JIT pinning, SATB holes (statics side-table, thread-exit buffer loss,
remark seed drops at the gray-set cap), the java.lang.ref protocol
(enqueued weak refs never read cleared; finalize never/thrice), JNI
local-ref scan/remap pairing, ZGC fragmentation + GC-storm latch +
registry/monitor leaks + mark validation, mixed-CSet selection of
liveness-unknown regions, kept-region coherence after evacuation
failure, a Generational remark→sweep TAMS window, and the cross-thread
JIT takeover for G1/ZGC (INT-3) including concurrent-mark pauses.

**Verified against HotSpot, and where it is not** (re-measured 2026-09-23 by
the gc-common round's baseline battery, dev `aa58f3a2e`, JDK 25; each probe
once per backend and once on HotSpot). Matching HotSpot's result line on all
three backends: `FinalizeOnceProbe`, `ReachableFinalizeProbe`, `RefProbe`,
`G1ChurnPauseProbe`, `HumongousChurn`, `MtChurnProbe`,
`ConcurrencyUnderGcSweep`, `RandomLiveAcrossGc`, `SteadyChurnRecreation`,
`ObjectStreamLookupGcRaceProbe` (though it takes 44–51 s on each backend
against well under a second on HotSpot, not yet attributed), `HeapUsedDelta`,
`SkipSpanRetireProbe`, `MtShrink`. The `ObjectStreamLookupGcRaceProbe`
timing has *not been re-measured* since.

The rows that did **not** match on that baseline, and where each stands on
2026-09-26 (per the round's `orchestrator-w*-verification.md` reports):

| probe | backend(s) | baseline (2026-09-23) | now |
|---|---|---|---|
| `ThreadLocalFinalizeProbe` | all three | `live=11`, `PROBE-FAIL` after 30 s (HotSpot `live=0`) | **fixed in wave 3**: a dead thread's `ThreadLocal` values are released at thread exit; `PROBE-OK` |
| `GcOverheadProbe -Xmx128m` | G1 | process abort (`FATAL: G1: out of heap space … cannot report failure`) | **fixed in wave 3**: rc=1 with an uncaught `OutOfMemoryError`, as HotSpot |
| `XmsProbe -Xms64m -Xmx512m` | all three | `total=512m` (HotSpot `total=64m`) | **fixed 2026-09-23**: see the `committed_bytes` paragraph above for the two residuals |
| `RefCheckOld` | Generational | `finalized=32/32` from wave 2 on | matches, but only because `System.gc()`'s non-moving young sweep keeps its finalizables young; `OldGenFinalizeProbe`, which promotes them, finalizes **0/64** (open, see java.lang.ref above) |

Two further failures the round's first test run surfaced are also fixed:
**class-loader unloading** now happens in the shape
`vm/tests/class_loader_unload_regression.rs` exercises (green since wave 3:
`roots.rs` step 22 no longer roots every queued `Reference`, and the
`Class$Atomic` side store is loader-conditional), and **the JDK `Finalizer`
thread no longer dies** on a `null` from `ReferenceQueue.remove()`
(`docs/internal/fixed-bugs/r11w11-orch-compatible-finalizer-thread-npe-FIXED-20260924.md`).

Three probes regressed on dev after the round's wave 32
([`common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications`](internal/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md)).
Two pass on Generational at the end of the GC defects round (2026-09-28, not
re-run on G1 or ZGC): `GcNotificationThreadProbe --compatible` receives its
notifications and `FullHeapResolveProbe --compatible` prints `PROBE-OK`. The
third is **open**: `NativeGrowthReclaimProbe -Xmx128m` still dies of an
uncaught `OutOfMemoryError` at its line 95, 6/6 on Generational in both modes
(on ZGC the arena reported fragmentation, not exhaustion); its holder is not
yet attributed.

**Standing invariants the collectors are checked against.** These are asserted
in `gc/`, not just documented:

- The published TLAB skip-offset list is sorted, coalesced and disjoint. Two
  partially overlapping spans would make the sweep walk resync twice and
  silently skip every object between them.
- A TLAB's published reserved tail starts 8-byte aligned. A non-aligned start
  is rounded up (the fail-safe direction) rather than dropped.
- A moving young collection **refuses to run** while a non-empty clipped tail
  set is published: the cycle over-retains, spills to old gen and retries,
  instead of relocating over a TLAB some mutator left un-retired.
- `OldGen::free` returns exactly the extent `alloc` reserved. An unrounded
  return would leave a remainder off the free list, and `walk_objects` derives
  allocated extents from the gaps *between* free blocks, so the walk would
  resume at a non-object-start and abandon the rest of the region.
- The old-gen in-place sweep runs a live-set closure before its free loop, so
  an unmarked block still referenced by a marked old-gen object is retained
  transitively rather than handed back.
- A conservative root that lands in a field or a mid-object spill is resolved
  to the object that contains it. Both plausibility screens are exact-base
  tests, so an interior root would otherwise mark nothing and let the sweep
  free a live block under it. The compacting arm cannot honour an interior
  root — a slid object leaves it dangling — and is downgraded to the in-place
  sweep for that cycle.

**Current limitations:**

1. `-XX:+UseZGC` selects the compatibility implementation described
   above, not HotSpot's concurrent colored-pointer ZGC.
2. `-XX:+UseStringDeduplication` is parsed but intentionally inert. The
   unwired `StringDeduplicator` model that used to sit in `gc/src/numa.rs` was
   deleted on 2026-09-23 along with the simulated `NumaAllocator`; a real
   deduplication table would have to participate in every collector's remap
   and purge protocol. **There is no NUMA-aware placement** either: the
   topology probe (`gc/src/numa.rs`) feeds one `tracing::trace!`, Windows is
   always one node, and nothing binds memory to a node.
3. **Class metadata ("Metaspace") is bounded only by `-XX:MaxMetaspaceSize`,
   and only approximately.** Since 2026-09-24 the launcher honours the flag
   (per VM; unset = unbounded, as before): a class defined from raw bytes
   (`ClassLoader.defineClass*`, `Lookup.defineClass` / `defineHiddenClass`,
   `Unsafe.defineClass`) that would exceed it first runs one full collection —
   class unloading gives a dead loader's charge back — and is then refused
   with `OutOfMemoryError: Metaspace`. That collection is NOT a `System.gc()`
   (since gc-common w7-g, `force_gc_for_class_metadata`): the defining thread
   holds the class-loading lock, so it runs no finalizer, Cleaner or
   GC-notification drain and never waits for the delivery thread; the work the
   pause queued is handed to the next GC door. What is charged is the CLASS-FILE size
   of every class not defined by the bootstrap loader
   (`ClassManager::metaspace_charged_bytes`) — an under-estimate of HotSpot's
   metadata, so a limit that fits there fits here and a leak hits it a little
   later; class-path loads of the built-in loaders are charged but never
   refused; `-XX:MetaspaceSize` is still ignored
   ([`common-w5f-max-metaspace-size-is-not-enforced`](internal/gc-common-round-20260923/common-w5f-max-metaspace-size-is-not-enforced-FIXED-20260923.md)). The
   serviceability heap summary and the JMX non-heap `MemoryUsage`
   (`MemoryMXBean.getNonHeapMemoryUsage()`) both report the same MEASURED
   lower bound since 2026-09-23 (class records, constant pools, method tables
   and bytecode of the live classes; it falls when a loader is unloaded), with
   `committed == used` (no reservation stands behind it) and `max = -1`. Not
   counted: interned strings, vtables, resolution caches, JIT code
   ([`common-d-metaspace-numbers-are-fabricated-FIXED-20260923`](internal/gc-common-round-20260923/common-d-metaspace-numbers-are-fabricated-FIXED-20260923.md)).
   The JMX memory-pool and garbage-collector beans are HotSpot's for each
   collector shape, with real usage and GC notifications: Serial's on
   Generational since 2026-09-23
   ([`gengc-r4-plumbing-mxbeans-are-not-per-generation-FIXED-20260923`](internal/gc/gengc-r4-plumbing-mxbeans-are-not-per-generation-FIXED-20260923.md)),
   G1's and ZGC's since gc-common w7-f (2026-09-24): G1 `G1 Young
   Generation` / `G1 Old Generation` / `G1 Concurrent GC` over `G1 Eden
   Space` / `G1 Survivor Space` / `G1 Old Gen`, ZGC HotSpot 25's
   `ZGC Minor Cycles` / `ZGC Minor Pauses` / `ZGC Major Cycles` /
   `ZGC Major Pauses` over `ZGC Young Generation` / `ZGC Old Generation`
   (since the gcd d10/o retirement, 2026-10-04; it was JDK 21's
   non-generational `ZGC Cycles` / `ZGC Pauses` over `ZHeap`, names no JDK
   25 HotSpot answers; the ZGC state lives in
   `HeapRealm::zgc_gc_beans`, applied at the wave-7 merge; the historical
   single `G1 Young Generation` bean with `-1` pools is gone on every
   backend). See the `-Xlog:gc`, JMX row of the diagnostics table below.
4. G1's **young** evacuation is parallel **by default**, on a persistent
   worker pool. The **mixed** pause is serial by default, but that is now a
   measured decision rather than a missing route:
   `CRATONVM_G1_PARALLEL_MIXED=1` dispatches `mixed_collection` to
   `mixed_collection_parallel`, and `[GC] g1 mixed-route: serial=<n>
   parallel=<n>` (printed unconditionally, `gc_metrics.rs`) is the census
   that says which arm a run took. Leaving it off buys nothing measurable
   and costs nothing: on the shape a mixed pause actually has — an old live
   set reached through the remembered set — **100% of the bytes the pause
   copies are copied in Phase 2, by the driver thread alone**, and this
   lever parallelises Phase 3. Seventeen workers scan; none copies a byte.
   See `docs/internal/g1-2026-09-20/w6p-the-mixed-parallel-number-and-the-phase-it-is-not-about.md`
   for the measurement and
   `docs/internal/fixed-bugs/g1-mixed-collections-never-reach-the-parallel-evacuator-RETIRED-20260921.md`
   for the capability gap this replaced.

   Two claims this entry used to make were false in both halves, which is
   why they are spelled out here. The switch is `CRATONVM_G1_PARALLEL_EVAC`
   and it is `on_unless_zero` (`types/src/flags.rs`) — an opt-**out**, never
   an opt-in. (Since the 2026-09-20 flag groups, `CRATONVM_GC=g1-parallel-evac`
   is a valid token, but it only restates the default; the lever is
   `CRATONVM_GC=-g1-parallel-evac`.) And the
   helpers stopped being a per-collection `thread::scope` when they became
   the persistent `EvacPool` (`gc/src/g1.rs`, `pool.scope`); putting thread
   creation inside every pause is the cost that move removed. A reader
   following the old text would have set a flag that did not exist and
   concluded the default path was serial.
5. **Open defects a user can meet.** Everything still to do is filed in
   [`known-issues/gc/`](known-issues/gc/) (its `README.md` is the index:
   the Serial / Generational and common defects by area, the G1 and ZGC
   pages, the ranked proposals, and every opt-in switch with its flip
   gate). The ones with a visible symptom (as of the end of the GC
   defects round, 2026-09-28):
   * On Generational, recovering from an `OutOfMemoryError` after the JIT
     built the dropped chain: `Gcd1ThreadExitSpillProbe` passes 2 of 9 runs
     and `Gcd1PinnedCalleeOomeProbe` none of 12; `NativeGrowthReclaimProbe`
     (the open dev regression under "Current correctness state") fails 6/6.
     The census of the first two names the true-root major's
     fallback to the legacy young seed on promoting cycles (see "Generational:
     what the GC defects round changed" below).
   * On Generational, a heap-full program can livelock on young cycles that
     all fall back to the non-moving sweep (1 of 12 default runs of
     `GenR4W4HeapFullThrashProbe`), and `GenR4W5ThreadsOomProbe` takes
     200-320 s where HotSpot takes about 8 s.
   * On Generational with the JIT on, young cycles rarely copy, so young
     fragments and collects several times more often than HotSpot Serial
     (`GenR4W4EvacThroughputProbe` 17.9-23.1 s against about 3 s).
   * `Thread(group, target, name, stackSize)` and `-Xss` size the carrier
     now, but compiled self-recursion is still capped at 4 MiB, so a deep
     recursion that HotSpot runs on a large-stack thread overflows
     ([`gcd-d6s-thread-stack-size-argument-is-ignored-20260928`](internal/gc/gcd-d6s-thread-stack-size-argument-is-ignored-FIXED-20260929.md)).
   * A JNI native that blocks in C no longer holds every pause: since the
     user's flip of 2026-10-13 all three JNI switches are default ON (see
     "JNI and threads in native" below; `CRATONVM_JNI_NATIVE_TRANSITIONS=0`
     restores the old behaviour).
   * With the JIT on, an allocation-driven collection on **G1** can keep a
     weakly-reachable referent (cause 2 of
     [`common-w4o-allocation-driven-collections-do-not-clear-weak-referents`](internal/gc/common-w4o-allocation-driven-collections-do-not-clear-weak-referents-FIXED-20261002.md);
     the JIT-owned cause 1 is fixed and measured on Generational).
   * `-Xms` is not a floor for ZGC's decommit (the `committed_bytes`
     paragraph above).

**G1 and Generational at the end of the gci round (2026-10-16).** The round
ran waves i1-i36. The last judged binary is `cratonvm-gci-i35a` (Batch 7,
Linux host); the tree then took wave i36 and a second `dev` merge
(`d1bd887947`), and neither has a recorded battery. Close-out:
[`round-close-20261016.md`](internal/gc-round-gci-20261011/round-close-20261016.md);
open pages: [`known-issues/gc/README.md`](known-issues/gc/README.md). The three
G1 items this section listed as HIGH on 2026-10-14 stand as follows:

- **A G1 young pause corrupted javac objects**
  ([`gci-i18orch-g1-young-pause-corrupts-javac-objects-before-any-marking-cycle-FIXED-20261015`](internal/gc/gci-i18orch-g1-young-pause-corrupts-javac-objects-before-any-marking-cycle-FIXED-20261015.md)):
  FIXED. The round-2 wrong digests were lane fz's defect (the G1 cleanup
  nulled the fields of queued finalizables after a marking cycle; fixed by
  `CRATONVM_G1_MARK_PENDING_FINALIZERS`, default ON), 3/3 reference digest on
  i19c. The later Mixed-pause `corrupt Value cell` bursts were the budgeted
  CSet verifier reading a reused region (gci i29/cv2; `gci-i26cv` retired
  FIXED). A live object whose slot names an interior address is now refused by
  the evacuation screen (`CRATONVM_G1_EVAC_REFUSE_COVERED_FLAGLESS`, default
  ON); its producer is open on `gci-i29cv2`.
- **A parked virtual thread's holder graph reads wrong on G1's default
  path**
  ([`gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018`](internal/gc/gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018.md)):
  still OPEN, HIGH. Item 1 (CORRUPT holder) is narrowed to a stale
  compiled-frame word under pin pressure; `CRATONVM_G1_VT_PRECISE_DEPOSIT`
  (default ON) did not fix it. Item 2 (the `-Xmx64m` OOME: JIT pins of parked
  carriers' frozen bands, 46 of 64 regions a pause) is fixed on the default
  path by `CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE` (G1, default ON
  since the WARNING flip `f7fbf55f90`), awaiting `vi24-jp.list`.
- **The G1 javac young-pause storm**
  ([`gci-i14yt-g1-default-young-loop-has-no-base-exceeds-goal-case-FIXED-20261016`](internal/gc/gci-i14yt-g1-default-young-loop-has-no-base-exceeds-goal-case-FIXED-20261016.md)):
  fixed on the default path by the WARNING flip `83b1120a0f`:
  `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL` and `CRATONVM_G1_SURVIVOR_PLAB` are
  default ON (vi19-ys M5: 0/3 stalled against 3/3), with the young-target
  floor `CRATONVM_G1_YOUNG_TARGET_MIN_EDEN`. The model has HotSpot's
  base-exceeds-goal case, so `CRATONVM_G1_YOUNG_TARGET_BASE_AWARE` (opt-in) is
  ignored while it is on. Open on rows: the 0-in-20 retirement rule
  (`gci-i2st` G1 half) and the vi20-ys mtc50 FAIL, which i24/ym showed is
  HotSpot 25's algorithm honouring `-XX:MaxGCPauseMillis=50` on our costs.

Verified by probe rows in waves i15-i18 (on i15a unless noted): the in-native
leave fairness (vi15-hx R + T, page retired); HotSpot's five non-heap pools
(vi15-nh); the G1 remark half of G-9 under its opt-in (vi15-rf U) and the G1
reference log shape (vi15-rf X); JNI `IsAssignableFrom` / class-id fallback /
no per-call Java-heap allocation (vi15-jk U, H, P2); the full collection's
cycle abort under its opt-in (vi15-ca O, C); the G1 half of
`CRATONVM_GC_VT_PARKED_MIRROR_KEYED` (vi15-vh E1, E3, D); the stale-slot and
mixed-names probes on i17a (vi17-nr G, SAME x3).

FIXED IN CODE and awaiting rows, so no longer described here as open
symptoms: cleanup leaving slots into the regions it freed (i16/nc, default
ON scrub; i17/nr, i18/nr); 300-virtual-thread probes hanging under `--nojit`
(i17/vt, i18/vp); a JNI context outside the native-call funnel aborting the
process on a heap refusal at the fifteen sites i16/oo, i17/na and i18/na
covered (about 35 others are refuted by reading or still open, listed on
[`gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-where-hotspot-does-not-FIXED-20261017`](internal/gc/gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-where-hotspot-does-not-FIXED-20261017.md));
that page's G1 OOME at `-Xmx96m` (mixed phase starved by the futile
breaker; i17/mx, i18/mx); a frozen peer's TLAB-tail allocations and
marking-scan rows read dead at a G1 remark (i16/mr, i17/rv); the JNI repeat
probe's `full` mode slowdown (i17/jh: slow runs, not a hang; i18/fh); and
`Thread.print` printing no frames for a running thread (i18/dc).

The STW barrier race and missing class-unloading driver described by
earlier versions of this page are fixed — the driver runs on every backend,
and since gc-common wave 3 the loaders `class_loader_unload_regression.rs`
drops are found dead and unloaded (on Generational a young `ClassValue`
value can still keep its class, see "Loader-conditional root sources"
above). Since gc-common w36-c unloading sweeps each JIT table once for the
whole unloaded set, not once per class
([`common-w8e-class-unload-sweeps-whole-tables-once-per-class`](internal/gc-common-round-20260923/common-w8e-class-unload-sweeps-whole-tables-once-per-class-FIXED-20260926.md)).
For the current class-loader
liveness and reclamation contract, see
[Class-loader unloading and bounded metadata](architecture/class-loader-unloading.md).

## Verifying and debugging

**Probe kit** — `/data/data/gcprobes-0710` on the Linux probe host (self-
checking, deterministic, HotSpot-diffable):

```
./cratonvm --java-home <jdk> -XX:+UseG1GC -Xmx256m -c . ChurnCheck
```

ChurnCheck (linked-list churn + payload verification), HumongousCheck
(multi-region arrays), CopyChurn (ref-array `System.arraycopy` barrier
coverage), MTChurn (monitors + counters under moving GC), RefCheck /
RefCheckOld (weak/soft/finalizer protocol, young and old referents),
SpinPoll / SpinPollMark (never-polling compiled spins vs STW + concurrent
mark), BinaryTrees (deep recursion). Always diff against a real JDK run.

Two generational remembered-set probes live in `bench/` and are ordinary
tracked sources rather than part of the kit above:

* `OldGenRsetProbe [retainedDepth] [rounds] [churnDepth]` -- a large tenured
  set with **no** old-to-young edges, so every old-to-young scan it provokes is
  measurable waste. This is the pause-breakdown probe.
* `OldToYoungEdgeProbe [nodes] [rounds] [churnDepth]` -- its companion, which
  stores freshly allocated young objects into tenured fields and then verifies
  every one of them. This is the probe on which a card-table-only collector can
  actually be wrong, so it is the one to run under `CRATONVM_GC_VERIFY_RSET=1`;
  a verifier run whose `edges` is zero has tested nothing.

**Diagnostics** (env-gated, in the release binary):

> **The spellings in this table were stale until 2026-09-20.** Every per-flag
> `CRATONVM_*` variable below is now a **token in a grouped variable**
> (`cratonvm_types::flag_groups`, expanded by `vm-cli/src/main.rs` before any
> thread starts). The grouped spelling is the supported one; the old per-flag
> name still works — `Resolved::get` falls through to the raw environment — but
> a run that sets one prints, once, at startup:
>
> ```
> [cratonvm] 3 per-flag variable(s) set directly; the supported spelling is now:
>            CRATONVM_DBG=gc-stress CRATONVM_GC=par-threads CRATONVM_GC=moving-young
> ```
>
> (`CRATONVM_DBG=-deprecations` silences it.) The rules, all from
> `flag_groups::resolve`:
>
> * `CRATONVM_<GROUP>=tok` enables, `=-tok` disables, `=+tok` is the same as
>   `=tok`, and tokens are comma-separated: `CRATONVM_GC=-par-evac,verify-rset`.
> * A knob that carries a **value** takes it after a second `=`:
>   `CRATONVM_GC=par-threads=8`. Presence alone means `1`.
> * **An unrecognised token is fatal** — the VM exits 2 rather than run a
>   configuration you did not ask for. A misspelt *legacy* variable is not,
>   because nothing claims it; it is simply ignored. That asymmetry is a reason
>   to prefer the grouped spelling in scripts.
> * `CRATONVM_<GROUP>=all` enables every token in that group.
>
> Per-row verdicts, including two rows in `docs/gc-tuning.md` that named
> variables which **do not exist**, are in
> `docs/internal/reviews/gengc-round2-plumbing2-20260920.md`.

| Switch | What it does |
|---|---|
| `--verbose:gc` | **G1:** per-pause `[GC-STAT]` lines + exit `[GC-SUMMARY]`. **ZGC:** one `[GC] zgc-real:` line per collection. **Generational** *(since 2026-09-20)*: one line per collection, `[GC] generational: cycle=N kind=moving\|non-moving\|refused[+major] young=A->B old=C->D pause=X.XXXms [divert=<reason>]`. `kind` comes from `gc_metrics::last_collector_decision()`, i.e. from the branch that decided, not a re-derivation; `refused` is a cycle that did not collect at all; `divert=` names the `divert_non_moving` term that refused the copy. *(2026-09-21)* A `kind=refused` line carries `refusal=<reason>` instead of `divert=` — nothing was diverted, neither collector ran — and the reason names which of the three refusal causes fired (`skipped-young-to-space-undersized`, `skipped-young-walk-incomplete`, `skipped-young-reserved-tlab-tails`). Before that date this arm emitted an affirmative at `info` level and then no per-collection output for the rest of the run — while the *shutdown* `[GC] …` census did run, which the round-1 write-up overstated (`docs/internal/reviews/gengc-round1-probe-results-20260920.md`). All three backends print the shutdown census, and *(since 2026-09-23)* the backend-independent `[GC] pause: gc=N door=… ttsp_us=… straggler=… xt_pass_us=… xt_post_quota_us=… collect_us=… xt_…` line after every collection (non-collection pauses: `gc=-`, `work_us=`; see the take-over paragraph above) |
| `CRATONVM_DBG=g1-dbg-reach` *(alias: `CRATONVM_G1_DBG_REACH=1`)* | Post-pause BFS-from-roots corruption detector; `[g1][FREED]`/`[WALKBRK]` traces |
| `CRATONVM_DBG=g1-dbg-pins` *(alias: `CRATONVM_G1_DBG_PINS=1`)* | Per-pause conservative-JIT-pin census |
| `CRATONVM_DBG=gc-verify-stale` *(alias: `CRATONVM_GC_VERIFY_STALE=1`)* | Post-GC stale-frame-slot verifier (recycled drain destinations are recognized as benign) |
| `CRATONVM_DBG=weakref` *(alias: `CRATONVM_DBG_WEAKREF=1`)* | Weak/Phantom null/restore pass tracing |
| `CRATONVM_GC=-g1-evac-retry` *(alias: `CRATONVM_G1_NO_EVAC_RETRY=1`)* | Disable the evacuation-failure drain (bisection) |
| `CRATONVM_GC=-g1-parallel-evac` *(alias: `CRATONVM_G1_PARALLEL_EVAC=0`)* | Force the single-threaded evacuator. Parallel evacuation is the **default**; the worker threads are not respawned per pause. Still owed: a gauntlet-scale soak and a throughput number, so this remains the bisection lever for any suspected parallel-evacuation regression. |
| `CRATONVM_GC=-g1-parallel-evac-in-jit` *(alias: `CRATONVM_G1_PARALLEL_EVAC_IN_JIT=0`)* | Restore the serial fallback for pauses taken while a thread is in compiled code. That fallback used to be unconditional, on the stated ground that only the serial path pinned conservative JIT roots — which was stale (the parallel driver applies the same collection-set exclusion). It mattered because on a JIT-warm application it is true for nearly every pause, so G1 copied single-threaded in production. **First thing to try for any G1 crash seen only with the JIT warm** — defect G1-11 lives in this path. |
| `CRATONVM_GC=g1-cleanup-walk` *(alias: `CRATONVM_G1_CLEANUP_WALK=1`)* | Make the concurrent-cycle cleanup pause recompute per-region liveness by walking every object, instead of reading the byte counter the marker maintains. The walk was cleanup's only implementation until F-06 — an O(heap) STW pass at the end of every cycle. `=1` restores it as the authority; a debug build runs both and asserts they agree. First thing to try if a cycle is suspected of freeing a live Old region. |
| `CRATONVM_GC=-g1-adaptive-ihop` *(alias: `CRATONVM_G1_ADAPTIVE_IHOP=0`)* | Restore the pause-time-driven marking threshold. By default the threshold is planned from the measured mark duration and old-generation growth rate and tightened on to-space exhaustion; pause time drives only the young size, which is what it actually describes. |
| `CRATONVM_GC=-g1-adaptive-tenuring` *(alias: `CRATONVM_G1_ADAPTIVE_TENURING=0`)* | Restore the fixed `promotion_age` (15). By default the tenuring threshold is re-derived after each pause from an age histogram of surviving bytes, and may tenure earlier than configured — never later — when survivor space would overflow. G1's desired survivor size is HotSpot's: `ceil(young target / SurvivorRatio)` regions at `TargetSurvivorRatio` 50 % (gci i22/sz); a drain pass of an evacuation-failure recovery folds into its pause's age table. G1 honours `-XX:MaxTenuringThreshold` (0..16; 0 always tenures, 16 never tenures by age) and `-XX:TargetSurvivorRatio` as HotSpot G1 does; `-XX:InitialTenuringThreshold` is Parallel's only. A malformed or out-of-range tenuring value refuses to start with HotSpot's lines (gci i23/tn). |
| `CRATONVM_GC=-g1-reserve-heap` *(alias: `CRATONVM_G1_RESERVE_HEAP=0`)* | Commit the whole heap at startup instead of reserving `-Xmx` and committing on demand. Also the state a platform without a reservation implementation is in anyway. First thing to try for a G1 fault at a heap address that looks mapped. |
| `CRATONVM_GC=g1-uncommit` *(alias: `CRATONVM_G1_UNCOMMIT=1`)* | **Opt-in.** Return the pages of a trailing run of Free regions to the OS at the end of a concurrent-mark cleanup, so a process that has finished a burst does not hold its high-water mark for life. Never shrinks below `-Xms`, and only above the highest region still in use — the committed set has to stay a prefix. Opt-in because the two halves of the reserved heap have different failure modes: getting growth wrong is a missed optimisation, getting the shrink wrong is a fault in compiled code. |
| `CRATONVM_G1_UNCOMMIT_TAIL_GRANULE` | The G1 heap shrink (`CRATONVM_G1_UNCOMMIT`, `CRATONVM_G1_HEAP_RESIZE`) also returns the mapped remainder of the committed prefix's last 2 MiB granule (the bytes above the prefix, which nothing may touch); the old range ended at the prefix and the store's inward rounding kept that granule resident. Reached only from the two opt-in shrinks. **Default ON since gci i42/ta** (`=0` restores the old range); `vi42-ta.list` U + B1 keep it. |
| `CRATONVM_GC=-g1-tlab-clamp` *(alias: `CRATONVM_G1_TLAB_CLAMP=0`)* | Restore the pre-2026-09-20 behaviour: `refill_tlab` REFUSES a request larger than half a region instead of clamping it to that bound. The refusal was a one-way cliff — `tlab::TlabPressureTracker` doubles a hot thread's request up to `MAX_TLAB_SIZE` (1 MiB) and the region-size ergonomic gives 1 MiB regions up to a 2 GiB heap, so the ladder's last rung is over the bound, and the sizer only re-sizes on a refill that actually happened. A thread that reached it fell back to per-object allocation through the exclusive guard for the rest of its life. `[GC] g1 tlab: oversize_clamped=` is the engagement counter; a zero means the ladder never climbed that far on your workload. |
| `CRATONVM_GC=-g1-humongous-best-fit` *(alias: `CRATONVM_G1_HUMONGOUS_BEST_FIT=0`)* | Restore first-fit for the humongous contiguous-run search. By default it is best-fit — the SHORTEST run of free regions that still fits, ties to the lowest index — so a small humongous object spends a tight hole instead of eating the head of a long run. G1 has no compaction pass that can manufacture a run back, so a long run consumed by a request that would have fitted in a hole is gone until the regions above it are freed. Both arms return a run of exactly `count` adjacent Free regions, so this is a placement policy, not a correctness switch; first-fit stops at its first hit while best-fit always scans the table, and `[GC] g1 free-scan: contiguous(...)` prices both. |
| `CRATONVM_GC=g1-heap-resize` *(alias: `CRATONVM_G1_HEAP_RESIZE=1`)* | **Opt-in.** The adaptive heap-sizing policy: a *soft capacity* between `-Xms` and the `-Xmx` region grid, grown when GC overhead exceeds 7.7 % (G1's `GCTimeRatio=12`) or less than `MinHeapFreeRatio` (40 %) of it is free, to the capacity that leaves 40 % free; shrunk after four consecutive pauses with more than `MaxHeapFreeRatio` (70 %) free and < 1 % overhead, to the capacity that leaves 70 % free; at a Remark, from that census alone; starts at `-Xms`; honours `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio` (gci i13/ef; before it: 5 % overhead / 70 % occupancy to grow, 40 % occupancy to shrink, to 60 %). The capacity is the denominator of the free-fraction trigger AND the floor for the trailing-Free-run shrink, which this flag also moves from `cleanup` to the end of every evacuation pause (gated on phase Idle, SATB inactive and an empty gray set, the same gate `eager_reclaim_early_decline` computes). Without it, `-Xmx` is a one-way ratchet on RSS for any workload that never crosses IHOP, because `cleanup` is the shrink's only caller. Watch `heap_grows` and `heap_shrinks` on the `[GC-STAT]` line: two large, nearly equal counts are the hysteresis being too weak. |
| `CRATONVM_GC=g1-pause-cost-model` *(alias: `CRATONVM_G1_PAUSE_COST_MODEL=1`)* | **Opt-in.** Make the mixed-CSet copy budget price the pause it is actually buying. Two terms: the YOUNG half of the collection set is charged first (every Eden and Survivor region is in the CSet unconditionally, so `max_gc_pause_ms` was spending its whole goal on the *second* copy of the pause), and each old candidate is charged the MARGINAL fix-up cost it causes — `fixup_ns_ema / fixup_regions_ema` times the walk regions it adds (its to-space destination, plus its remembered-set sources that no already-selected candidate has paid for). Before this, `fixup_ns_ema` was a constant with respect to the very choice being made, so the model under-estimated in the direction that overruns. Both terms are subtractions from the budget, so arming it can only make a mixed collection set SMALLER — safe for the pause goal, and a throughput question for the mixed phase, which is why it is opt-in. The selector still takes one region unconditionally for forward progress. Since gci i11/gm the marginal fix-up price is charged on every production mixed pause (one selector, `choose_old_cset`); before, only the test hook charged it. |
| `CRATONVM_GC=-g1-humongous-run-guard` *(alias: `CRATONVM_G1_HUMONGOUS_RUN_GUARD=0`)* | Restore the old hinted first-Free-region scan for an explicit performance A/B. By default, `claim_free_region` preserves the longest Free run: it prefers a Free region outside that run and otherwise takes its END. That prevents a single-region Old or humongous-start claim from splitting a span a later humongous allocation needs. Placement only: both arms return a Free region and neither can fail where the other succeeds. The default pays a full table scan under the exclusive guard; `[GC] g1 free-scan: single(...)` prices both arms, and `[GC] g1 humongous: requests= failures= free_at_failure= longest_run_at_failure=` is the outcome census. |
| `CRATONVM_GC=-g1-eager-humongous` *(alias: `CRATONVM_G1_EAGER_HUMONGOUS=0`)* | Restore cleanup-only humongous reclaim. By default an evacuation pause also frees humongous spans it can prove nothing references. This is the only path that frees memory outside the collection set, so it is the first thing to rule out if a live humongous object goes missing. |
| `CRATONVM_GC=g1-young-pause-target` *(alias: `CRATONVM_G1_YOUNG_PAUSE_TARGET=1`)* | **Default ON** (`=0` turns it off). Let `max_gc_pause_ms` bound the YOUNG generation too, not just the old half of a mixed collection set: G1 also collects once the Eden+Survivor region count reaches an adaptive target, tightened by 20% after any PRODUCTIVE pause that overruns the goal and relaxed while pauses stay under half of it. Does nothing until such an overrun is measured (the target starts at its 60%-of-regions ceiling and a target at the ceiling is not a trigger). Measured trade on `G1ChurnPauseProbe` at `-Xmx2048m`: p50 -21%, p99 +3%, wall +4.2%, one extra pause — see the young-sizing paragraph under Backend details for the full table and the measurement that preceded the flip. |
| `CRATONVM_GC=g1-young-target-net-of-pins` *(alias: `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS=1`)* | **Opt-in (gci i7/g1y).** The young-target arm counts the young generation NET of the regions the last young/mixed pause kept out of its collection set under a pin (`jni_pinned_out + jit_pinned_out`); without it a pause-target at its floor plus ~20 pinned young regions fires a young pause every few Eden regions (G1 4-thread javac -Xmx512m: ~2 800 pauses, timeout). Rows: vi7-g1y J2. |
| `CRATONVM_DBG=g1-verify-unbounded-on-verbose-gc` *(alias: `CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC=1`)* | **Opt-in (gci i7/g1y).** `--verbose:gc` selects the whole-heap CSet verifier again, as before 2026-10-13. Default: `--verbose:gc` runs the same budgeted sampler as every other release run. |
| `CRATONVM_GC=g1-young-target-net-of-heap-costs` *(alias: `CRATONVM_G1_YOUNG_TARGET_NET_OF_HEAP_COSTS=1`)* | **Opt-in (gci i8/gs).** The young pause-goal loop is fed the pause minus `verify_us` and the whole-heap `fixup_us`, the two phases that scale with the old generation, so a large old generation cannot drive the young target to its floor. Rows: vi8-gs J. |
| `CRATONVM_GC=g1-young-target-futile-below-region` *(alias: `CRATONVM_G1_YOUNG_TARGET_FUTILE_BELOW_REGION=1`)* | **Opt-in (gci i8/gs).** A young or mixed pause that freed less than one region resets the young target to its ceiling (the default resets only on a pause that copied and freed nothing). Rows: vi8-gs J. |
| `CRATONVM_GC=g1-free-trigger-net-of-pins` *(alias: `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS=1`)* | **Opt-in (gci i8/gs).** The young floor of the low-Free triggers counts young net of the regions the last young/mixed pause kept under a pin; the to-space headroom bound keeps the gross count. Rows: vi8-gs S. |
| `CRATONVM_GC=g1-fixup-skip-clean-pinned` *(alias: `CRATONVM_G1_FIXUP_SKIP_CLEAN_PINNED=1`)* | **Opt-in (gci i9/fa).** The parallel young pause's Phase-4 fix-up skips a pinned region that is wholly card-clean and unchanged by the pause; armed only with `CRATONVM_G1_CARD_RSET` and `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` on and `CRATONVM_G1_CARD_CLEAN` off. Census `fixup_skipped_regions` / `fixup_skipped_bytes` on `[GC] g1 pause-phases:`. |
| `CRATONVM_GC=g1-parallel-fixup` *(alias: `CRATONVM_G1_PARALLEL_FIXUP=1`)* | **Opt-in (gci i10/pp).** The evacuation pause's Phase-4 fix-up walks its regions on the G1 evacuation pool (remembered-set edges merged by the driver in region order, as the serial walk applies them); not for a walk that owes the humongous census. Census `fixup_parallel_walks`. Rows: vi10-pp A. |
| `CRATONVM_GC=g1-pin-skip-above-cursor` *(alias: `CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR=1`)* | **Opt-in (gci i10/pp).** A JIT pin word or non-object root strictly above its Eden / Survivor / Old region's allocation cursor no longer keeps that region out of the collection set (it names no allocated byte). Census `pin_words_above_cursor`. |
| `CRATONVM_GC=g1-mixed-hotspot-choice` *(alias: `CRATONVM_G1_MIXED_HOTSPOT_CHOICE`)* | **Default ON since 2026-10-17 (gci i10/rh), `=0` kills.** HotSpot G1's mixed choice: candidate live bytes are the cursor minus the garbage fixed at cleanup, each mixed pause takes at least `ceil(candidates / G1MixedGCCountTarget)` old regions, the phase ends on no candidate or the strict `G1HeapWastePercent` floor (not after 8 pauses). `[GC] g1 mixed-live:` counts copies into marked Old regions on both arms. Rows: vi10-rh M. |
| `CRATONVM_GC=g1-mixed-count-skips-compaction` *(alias: `CRATONVM_G1_MIXED_COUNT_SKIPS_COMPACTION=1`)* | **Opt-in (gci i11/gm).** A full or allocation-failure compaction pause inside a mixed phase no longer spends one of its `G1MixedGCCountTarget` pauses. `[GC] g1 gm-census: compaction_in_mixed_phase=`. Rows: vi11-gm M3. |
| `CRATONVM_GC=g1-eager-reclaim-during-mark` *(alias: `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK`)* | **Default ON since 2026-10-17 (gci i11/gm), `=0` kills.** Eager humongous reclaim also runs while a marking cycle is open, for primitive-array spans only (HotSpot's `typeArray` rule); declines a pause on which a marker holds popped work or SATB has pending entries. `[GC] g1 gm-census: eager_mark_*`. Rows: vi11-gm EM. |
| `CRATONVM_GC=g1-full-compaction-skips-mark-drive` *(alias: `CRATONVM_G1_FULL_COMPACTION_SKIPS_MARK_DRIVE=1`)* | **Opt-in (gci i11/go).** The compaction pause that ends a G1 full collection skips the `alloc_fail` mark-cycle drive, so no concurrent cycle opens right after a full collection (HotSpot starts none). Row: vi11-go C. |
| `CRATONVM_GC=g1-prepare-mixed` *(alias: `CRATONVM_G1_PREPARE_MIXED=1`)* | **Opt-in (gci i11/go).** HotSpot's `Prepare Mixed`: the first pause after a cycle's cleanup is young-only (`Pause Young (Prepare Mixed)`), mixed pauses start with the next. Row: vi11-go P. |
| `CRATONVM_GC=g1-full-gc-one-report` *(alias: `CRATONVM_G1_FULL_GC_ONE_REPORT=1`)* | **Opt-in (gci i12/ge).** A G1 full collection (`System.gc()`, `jcmd GC.run`, live inspection / dump) is reported as HotSpot's ONE `Pause Full (<cause>)`: its young pause, marking cycle and compaction fold into one `G1 Old Generation` collection, one `GC(n)` line and one JFR old event (`--verbose:gc` still prints each pause); `jdk.GarbageCollection.longestPause` follows. |
| `CRATONVM_XLOG_REFUSE_INVALID_SELECTION=0` | **Default ON (gci i27/ob), `=0` kills.** The launcher refuses an `-Xlog` option HotSpot 25 refuses (an unknown level or tag name) with HotSpot's lines and exit status 1; `=0` restores the warn-and-run tolerance. |
| `CRATONVM_G1_CONCURRENT_START_FROM_DRIVE=0` | **Default ON (gci i27/ob), `=0` kills.** A G1 young pause is labelled `Pause Young (Concurrent Start)` (`-Xlog:gc`, `jdk.G1GarbageCollection.type`) only when its requester's mark-cycle drive opened the cycle; its report is printed just before the cycle's `Concurrent Mark Cycle` line, or as `Normal`. `=0`: the in-pause prediction labels it. |
| `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE=0` | **Default ON (gci cs48), `=0` kills.** HotSpot 25's `decide_on_concurrent_start_pause`: a G1 marking cycle opens only after a young-only pause, never right after a `Pause Young (Mixed)` (the next young-only pause opens it and is the `Concurrent Start`); and `System.gc()` / `jcmd GC.run` under `-XX:+ExplicitGCInvokesConcurrent` with no cycle open take their own young pause as the forced `Pause Young (Concurrent Start)`, open the cycle whatever IHOP says, and run no compaction pause after its cleanup. `=0`: a cycle opens after any pause the IHOP drive follows, and the user-requested door ticks IHOP and compacts. Rows vi27-ob CS1, vi28-dg E4. |
| `CRATONVM_G1_RESERVE_RECLAIM_CREDIT=0` | **Default ON (gci nd48), `=0` kills.** A small fallible G1 allocation (every allocation ladder's retry: `new`, the native doors, the JIT helpers) at the TLAB reserve may claim a Free region when collections have given back half a region of bytes per such claim since the reserve last refused one, and the claim leaves one Free region; on a heap filling with live data no credit is earned and the reserve stays for the infallible callers. `=0`: the reserve refuses every claim. Rows vi48-x `zl2_a2_g1_`. |
| `CRATONVM_G1_FULL_DOOR_SURVIVOR_STAMP_IN_PAUSE=0` | **Default ON (gci i41/rs), `=0` kills.** The G1 full-collection door's re-stamp of `G1 Survivor Space`'s collection usage (empty, as HotSpot's one `Pause Full` leaves it; gci i30/ob) is taken inside the door's own full-compaction pause, before the world is released, and only by a pause that ran that compaction on a full-collection door; `=0` restores the stamp at the door's end, after the release, where another thread's young pause in between had its Survivor figure overwritten by the door's late zero. Rows vi41-rs S, R. |
| `CRATONVM_JFR_GC_CONFIG_HOTSPOT=0` | **Default ON (gci i42/mx), `=0` kills.** JFR's periodic GC configuration events answer HotSpot 25's values: `jdk.GCHeapMemoryPoolUsage` one row per heap pool under its name, `jdk.GCConfiguration` HotSpot's GC thread flags per collector (Serial 0 / 0, G1 the `WorkerPolicy` formula) and the explicit-GC flags, `jdk.GCHeapConfiguration` and the `MaxHeapSize` / `InitialHeapSize` flag rows the `gc,init` heap sizes, the narrow-oop fields this process's state; `=0` restores the old rows (one `Java heap` pool row, CPU-count threads, `totalMemory()` / `maxMemory()`). Rows vi42-mx C. |
| `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS=0` | **Default ON (gci i43/lf), `=0` kills.** The launcher parses `-XX:SurvivorRatio`, `-XX:TLABSize`, `-XX:MinTLABSize`, `-XX:±UseTLAB`, `-XX:±ResizeTLAB`, `-XX:TLABWasteTargetPercent`, `-XX:TLABRefillWasteFraction`, `-XX:TLABAllocationWeight` and `-XX:TLABWasteIncrement` with HotSpot 25's ranges and `AfterMemoryInit` constraints (Serial / G1): a value HotSpot refuses prints HotSpot's lines and exits 1; legal values are carried on `VmConfig` (read by the collectors since gci i45/sc: `CRATONVM_GEN_SURVIVOR_RATIO_FLAG`, `CRATONVM_TLAB_HOTSPOT_FLAGS`; `TLABWasteIncrement` has no consumer). `=0`: the old silent `-XX:` catch-all. Rows vi43-lf R, A, K. |
| `CRATONVM_GEN_SURVIVOR_RATIO_FLAG=0` | **Default ON (gci i45/sc), `=0` kills.** A typed `-XX:SurvivorRatio` (validated by the launcher, gci i43/lf) sizes the Generational heap as HotSpot Serial: the Serial eden `Y - 2 * align_down(Y / (SurvivorRatio + 2))` (young trigger, age clock, followed initial eden), the `CRATONVM_GEN_SURVIVOR_POOL_RESERVE` survivor reserve `semi / (SurvivorRatio + 1)`, and the tenuring target's survivor capacity `eden / SurvivorRatio`. Nothing typed or 8: unchanged. `=0`: the constant 8. Rows vi45-sc O, A. |
| `CRATONVM_TLAB_HOTSPOT_FLAGS=0` | **Default ON (gci i45/sc), `=0` kills.** TLAB sizing on every collector (Generational and G1; ZGC since gci i48/zl3, zx48 A8) reads `-XX:TLABSize` (first desired size), `-XX:MinTLABSize` (floor), `-XX:-ResizeTLAB` (fixed size, no raise), `-XX:TLABWasteTargetPercent` (`target_refills = max(100 / (2 * pct), 2)`), `-XX:TLABRefillWasteFraction` (refill-waste limit), `-XX:TLABAllocationWeight` (average weight) and `-XX:-UseTLAB` (no buffer beyond the object: every allocation takes the shared path), per VM (`HeapRealm::tlab_sizing`). Nothing typed: the old sizes byte for byte. `=0`: the old constants whatever was typed. ZGC (gci i48/zl3): the refill site sizes the request as on the other heaps and `ZgcRealHeap::refill_tlab_at_least` carves a floored request below its 8 KiB chunk floor (a typed `MinTLABSize`, or the object alone under `-UseTLAB`); the launcher range-checks the ZGC values (`hotspot_space_tlab_geometry`, TLAB max 262144). Rows vi45-sc T. |
| `CRATONVM_JMX_SAFEPOINT_COUNTERS=0` | **Default ON (gci i44/sp), `=0` kills.** `VMManagementImpl.getSafepointCount` / `getTotalSafepointTime` / `getSafepointSyncTime` / `getTotalApplicationNonStoppedTime` (`sun.management.HotspotRuntime`) answer the GC barrier's safepoint accounting (`GcBarrier::safepoint_stats`: completed pauses, request -> completion ms, time-to-safepoint ms, ms between pauses) as HotSpot's `RuntimeService` does; `=0` restores the constant 0. Rows vi44-sp S. |
| `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0` | **Default ON (gci i46/lg), `=0` kills.** A pause HotSpot 25 runs no safepoint for (loop-exit and inline-cache handshakes, a single thread's stack trace, Generational's concurrent-cycle pauses, G1's initial mark, marking pauses inside a G1 full collection) adds nothing to `getSafepointCount` / `getTotalSafepointTime` / `getSafepointSyncTime`, and `getTotalApplicationNonStoppedTime` runs on across it. Read once per VM. `=0`: every pause counts, as in i44/sp. Rows vi46-lg P. |
| `CRATONVM_JMX_MEMORY_MXBEAN_JDK_SINGLETON=0` | **Default ON (gci i46/hc), `=0` kills.** The platform `MemoryMXBean` the `getMemoryMXBean` / `getPlatformMXBean` natives answer is the JDK's `ManagementFactoryHelper.getMemoryMXBean()` singleton -- the bean the platform MBeanServer registers -- so a memory-threshold notification reaches a listener added through the MBeanServer under `--compatible`, as on HotSpot; `=0` mints the VM's own `MemoryImpl`. Rows vi46-hc M. |
| `CRATONVM_JMX_GC_BEAN_LIST_HOTSPOT_ORDER=0` | **Default ON (gci i46/hc), `=0` kills.** `--compatible`'s `ManagementFactory.getGarbageCollectorMXBeans()` lists the collectors in HotSpot's order (the JDK's `HashMap` of canonical object names: G1 Young, Concurrent, Old); `=0` keeps `MemoryImpl`'s manager order. Rows vi46-hc L. |
| `CRATONVM_JMX_FRESH_BEAN_STRINGS=0` | **Default ON (gci i46/hc), `=0` kills.** Memory pool and collector names, GC notification name / action / cause and the `GcThreadCount` attribute strings are fresh `String`s, as HotSpot's `create_from_str` builds them (`==` against a literal is `false`); `=0` interns them. Rows vi46-hc I. |
| `CRATONVM_XLOG_SAFEPOINT_LINES=0` | **Default ON (gci i45/sl), `=0` kills.** `-Xlog:safepoint` (`safepoint*`, `all`) prints HotSpot 25's info line per pause, `Safepoint "<VM operation>", Time since last: N ns, Reaching safepoint: N ns, At safepoint: N ns, Leaving safepoint: 0 ns, Total: N ns, Threads: 0 runnable, N total` (gci i47w/j45: HotSpot 25's shape; the i45 line carried JDK 21's `Cleanup:`), from the GC barrier's figures (application-time sum, time-to-safepoint) and HotSpot's operation names (`SerialCollectForAllocation`, `SerialGCCollect`, `G1CollectForAllocation`, `G1CollectFull`, `G1TryInitiateConcMark`, `CollectForMetadataAllocation`, `G1PauseRemark`, `HeapDumper`, `GC_HeapInspection`, `ThreadDump`, `RedefineClasses`); pauses HotSpot runs no safepoint for print none; `=0`: no line, as before. Rows vi45-sl S. |
| `CRATONVM_XLOG_FILE_ROTATION=0` | **Default ON (gci i46/lg), `=0` kills.** An `-Xlog:...:file=<name>` output follows HotSpot 25's `LogFileOutput`: at VM start an existing regular file is archived to `<name>.<n>` (`filecount`, default 5; the first free index, else the oldest archive's) or truncated (`filecount=0`), and the file is archived again once `filesize` (default 20M; `0` never) bytes were written; output options on an output that exists already print HotSpot's `Output options for existing outputs are ignored.` warning. `=0`: options ignored, every file appended to, as before. Rows vi46-lg R. |
| `CRATONVM_XLOG_SAFEPOINT_FRAME_TRACE_OPS=0` | **Default ON (gci i46/lg), `=0` kills.** A frame-trace pause's `-Xlog:safepoint` VM operation is the one its initiator's scope names: a single thread's `Thread.getStackTrace` is a handshake (no line, and no `sun.rt.safepoints` count), `jcmd Thread.print` is `PrintThreads`; no scope, `ThreadDump`. `=0`: `ThreadDump` always, as in i45/sl. Rows vi46-lg P. |
| `CRATONVM_XLOG_SAFEPOINT_AFTER_PARKED_REPORT=0` | **Default ON (gci i46/lg), `=0` kills.** The `safepoint` line of a G1 young pause whose `gc` lines wait for the requester's mark-cycle drive (the `Concurrent Start` label) is printed after those lines, as HotSpot orders them. `=0`: printed at the pause's finish, before them, as in i45/sl. |
| `CRATONVM_GC_GUARD_RING_ONLY_WARN=0` | **Default ON (gci i46/hb), `=0` kills.** The reclaim guard (`vm/src/memory/reclaim_guard.rs`) reports an old-gen or young freed-RING hit at `WARN` unless the free-list verdict fired or the caller proved the receiver corrupt (an impossible `clone()` dispatch, a decoded corrupt cell): the rings are never retracted when memory is re-served, so a legitimate failing `checkcast` of a live object in a reused span printed `ERROR` lines on stderr (HotSpot: none). `=0`: every ring hit at `ERROR`, as before. Rows vi46-hb U, R. |
| `CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE=0` | **Default ON (gci i46/ts), `=0` kills; feature `gpu-offload` only.** A collection whose collecting thread's per-cycle veto slot is gone (thread-locals torn down) still vetoes relocation after a timed-out GPU drain, through the process flag every cycle end clears; `=0` restores the fail-open answer (no veto: the cycle relocates). No shipped build enables the feature. Rows vi46-ts U-GPU. |
| `CRATONVM_JFR_GC_FLAG_CONFIG_HOTSPOT=0` | **Default ON (gci i43/tj), `=0` kills.** JFR's flag-valued GC configuration events answer HotSpot 25's flag values for this command line: `jdk.YoungGenerationConfiguration` HotSpot's `NewSize` / `MaxNewSize` / `NewRatio` (Serial's `GenArguments` arithmetic, G1 the flags with `Long.MIN_VALUE` max), `jdk.GCSurvivorConfiguration` the tenuring thresholds, `jdk.GCTLABConfiguration` and the `UseTLAB` row the launcher's TLAB flags, `jdk.GCConfiguration.gcTimeRatio` 12 on G1, and each flag row's `origin` (`Command line` / `Ergonomic` / `Default`); `=0` restores the old rows (a third of `totalMemory()` / `maxMemory()`, 15 / 7, the TLAB defaults, 99, every origin `Ergonomic`). Rows vi43-tj C. |
| `CRATONVM_GC=jvmti-force-gc-leaves-drains` *(alias: `CRATONVM_JVMTI_FORCE_GC_LEAVES_DRAINS=1`)* | **Opt-in (gci i27/ob).** JVMTI `ForceGarbageCollection` leaves the queued finalizers, cleaner actions and GC notifications to the next door that may run Java (HotSpot's `Finalizer` thread) instead of running them inline on the agent's thread. Rows: vi27-ob F3. |
| `CRATONVM_GC=g1-full-gc-aborts-open-cycle` *(alias: `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE=1`)* | **Opt-in (gci i13/gp, extended i15/ca).** A G1 full collection (and the OOME ladder's last-ditch cycle) that finds a cycle marking ABORTS it as HotSpot does: its own pause joins the marker and drops the cycle (no remark, reference processing or cleanup), only the full collection's fresh cycle marks, and the cycle reports as HotSpot's aborted cycle (`Concurrent Mark Abort`, no Remark / Cleanup, no `G1 Concurrent GC` count). Rows: vi15-ca O, C PASS; S VOID (no `System.gc()` overlapped a cycle), re-shaped as vi16-ca. |
| `CRATONVM_GC=g1-metadata-concurrent-start` *(alias: `CRATONVM_G1_METADATA_CONCURRENT_START=1`)* | **Opt-in (gci i13/gp).** G1 answers a `-XX:MaxMetaspaceSize` refusal with `Pause Young (Concurrent Start) (Metadata GC Threshold)` and a cycle it does not wait for; the full collection runs only if the define still does not fit. |
| `CRATONVM_GC=g1-humongous-reserve-one` *(alias: `CRATONVM_G1_HUMONGOUS_RESERVE_ONE=1`)* | **Opt-in (gci i13/oa).** A fallible humongous array at the TLAB reserve is served when it leaves one Free region (HotSpot G1 has no reserve; the default refuses it `reserve` regions early). Rows: vi13-oa A. |
| `CRATONVM_GC=g1-oome-skip-futile-soft-rung` *(alias: `CRATONVM_G1_OOME_SKIP_FUTILE_SOFT_RUNG`)* | **Default ON since 2026-10-17 (gci i13/oa), `=0` kills.** The last-ditch reclaim skips its soft-reference young pause when an armed remark ran in its cycle and no active soft referent is young or humongous. Rows: vi13-oa E. |
| `CRATONVM_GC=g1-humongous-compaction-needs-reserve` *(alias: `CRATONVM_G1_HUMONGOUS_COMPACTION_NEEDS_RESERVE=1`)* | **Opt-in (gci i13/oa).** The fragmented-humongous compaction is armed only when the Free pool also covers the retry's reserve. Rows: vi13-oa F. |
| `CRATONVM_GC=g1-survivor-overflow-to-old` *(alias: `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD=1`)* | **Opt-in (gci i13/ef).** The serial and (gci i27/gs) parallel evacuators retry a Survivor copy that found no room in Old before it self-forwards (HotSpot's `allocate_copy_slow`); `[GC-STAT]` `survivor_overflow_promotions=`. |
| `CRATONVM_GC=gen-survivor-pool-reserve` *(alias: `CRATONVM_GEN_SURVIVOR_POOL_RESERVE`)* | **Default ON since 2026-10-17 (gci i27/gs), `=0` kills.** Generational: `Survivor Space` reports HotSpot Serial's survivor reserve (`semi / (SurvivorRatio + 1)`, 64 KiB aligned) as its max and init, and the young commit's reserve as its committed, carved out of `Eden Space`; the pools' max still sum to `maxMemory()` and their committed to `totalMemory()`. Rows: vi27-gs SP. |
| `CRATONVM_GC=gen-survivor-space-aligned` *(alias: `CRATONVM_GEN_SURVIVOR_SPACE_ALIGNED`)* | **Default ON since 2026-10-17 (gci i27/gs), `=0` kills.** Generational: the adaptive tenuring target's survivor capacity is aligned down to 64 KiB as HotSpot's to-space is (`Desired survivor size` 4456448 at `-Xms256m -Xmx256m`, HotSpot's). Rows: vi27-gs DS. |
| `CRATONVM_JIT=xt-takeover-drop-tail-words` *(alias: `CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS=1`)* | **Opt-in (gci i27/gs, d10t).** The take-over drops roots strictly inside a frozen peer's reserved TLAB tail whose memory reads unallocated (they name no object); a root at the tail's start and every root of a tail that holds anything are kept. Rows: vi27-gs XT. |
| `CRATONVM_GC=g1-pinned-young-as-survivor` *(alias: `CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR=1`)* | **Opt-in (gci i13/ef).** An Eden region a pin kept out of a young or mixed CSet is retyped Survivor at the end of the pause (not if a frozen peer's TLAB tail lies in it), so `Eden regions: a->0` as on HotSpot; `[GC-STAT]` `pinned_eden_relabelled=`. |
| `CRATONVM_GC=g1-young-survival-young-only` *(alias: `CRATONVM_G1_YOUNG_SURVIVAL_YOUNG_ONLY=1`)* | **Opt-in (gci i20/ys).** The young survival ratio behind G1's evacuation-headroom trigger is sampled only from young-only pauses, over the Eden handed out since the last pause plus the packed survivor footprint (default: every pause but a humongous compaction, over every young region counted full, so mixed / full-compaction old copies read as young survival). Rows: vi20-ys SV. |
| `CRATONVM_GC=g1-humongous-ihop-start` *(alias: `CRATONVM_G1_HUMONGOUS_IHOP_START`)* | **Default ON since 2026-10-17 (gci i20/ys), `=0` kills.** A G1 humongous allocation that would take the old occupancy (plus the request, in whole regions) over IHOP first runs a `G1 Humongous Allocation` young pause and drives the mark cycle, as HotSpot's `attempt_allocation_humongous` does. Without it `G1 Humongous Allocation` is reported only for a humongous allocation that failed. Rows: vi20-ys HI. |
| `CRATONVM_GC=g1-young-reserves-mixed-min-old` *(alias: `CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD=1`)* | **Opt-in (gci i22/sz).** On the HotSpot young model, Eden before a mixed pause is sized in what the pause goal leaves after the base AND the predicted copy of the mixed phase's minimum old collection set (`ceil(candidates / G1MixedGCCountTarget)` regions at their mean live bytes), as HotSpot 25's `calculate_desired_eden_length_before_mixed`; the young target is also re-sized at cleanup, when the phase turns mixed. Default: the young half takes the whole goal and the old half gets its own budget on top. Rows: vi22-sz MR. |
| `CRATONVM_GC=g1-mixed-waiver-base-trigger` *(alias: `CRATONVM_G1_MIXED_WAIVER_BASE_TRIGGER=1`)* | **Opt-in (gci i23/tn).** The mixed gate's `tight` waiver of the `G1HeapWastePercent` floor is measured against twice the BASE free trigger (50 % Free) instead of twice the live trigger, which evacuation failures ratchet to 50 % -- where the waiver held at any occupancy. Rows: vi23-tn MW. |
| `CRATONVM_GC=g1-weak-string-table` *(alias: `CRATONVM_G1_WEAK_STRING_TABLE=1`)* | **Opt-in (gci i21/sw).** G1: the String intern table is weak at the marking cycle -- the mark root scans leave it out, the remark judges it on the completed bitmap and drops the dead entries after a trusted cleanup, and every pool hit while marking is logged through the SATB keep-alive (`vm/src/memory/string_table.rs`). Evacuation pauses keep rooting it. Deciding rows `vi21-sw.list` (FLIP RULE). `CRATONVM_GC_WEAK_STRING_TABLE=0` also turns it off. |
| `CRATONVM_GC=jni-in-native-proven-depth-credit` *(alias: `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT=1`)* | **Opt-in (gci i24/bp, Generational).** An in-native compiled thread whose deposit proved its JIT depth is credited at the pause (depth into the coverage account, band words into the young pin ledger), so its band no longer forces `CROSS_THREAD_JIT_PEER` and the young cycle may still move through the pinned copy. Rows: vi24-bp S / E / C. |
| `CRATONVM_GC=dedupe-initiator-snapshot` *(alias: `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT=1`)* | **Default ON on Generational since the gcj j2 WARNING flip (2026-10-19, `docs/internal/gc-round-gcj-20261019/triage-j1.md`; `=0` kills); G1 / ZGC unchanged.** (gci i21/pr.) The collection door's registry gather leaves out the initiator's own root snapshot, which `collect_roots` step 11 already pushed (root set unchanged; a Generational pinned old-gen compaction stops pinning the initiator's precise frame roots through the duplicate). Rows: vi21-pr E / S / C. |
| `CRATONVM_DBG=g1-dbg-flagless-sweep` *(alias: `CRATONVM_G1_DBG_FLAGLESS_SWEEP=1`)* | **Diagnostic (gci i29/cv2).** Whole-heap header sweeps at pause start/end, remark start and cleanup end; prints every NEW object start without `GC_FLAG_HEADER` and the phase that produced it (gci-i29cv2). O(heap) per sweep. Rows: vi29-cv2 D/R. |
| `CRATONVM_DBG=g1-dbg-freed-slot-audit` *(alias: `CRATONVM_G1_DBG_FREED_SLOT_AUDIT=1`)* | **Diagnostic (gci i39/cv).** After every free (each young / mixed / drain `free_or_keep_cset`, and cleanup's in-place Old and dead humongous frees) walks the surviving heap and prints every reference slot that still names a region the free just reset, with `why=` (fixup-miss, rset-hole, rset-stale, card-hole, walk-skip, marked-holder) and a per-event summary. Reads only; O(heap) per free. Rows: vi39-cv F / Z. |
| `CRATONVM_DBG=g1-dbg-closure-root-entries` *(alias: `CRATONVM_G1_DBG_CLOSURE_ROOT_ENTRIES=1`)* | **Diagnostic (gci i41/hc).** At the G1 pause that queues a dead kept-region finalizable, prints `[g1-closure-root]` lines: the closure's members and, for every root entry / pin word / overlay row / outside slot that named a member, every VM structure holding that address (the initiator's `collect_roots` step, each thread's deposited snapshot with blocked / conservative / precise / slot origin and whether the thread published it as a JIT pin, compiled-frame provenance, native root sources, statics / class tables / strings / JNI globals, finalizer and cleaner queues). Also turns the `collect_roots` scan marks on. Reads only (`try_*` locks). Rows: vi41-hc C / D / P. |
| `CRATONVM_GC=old-post-close-incremental` *(alias: `CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL=1`)* | **Opt-in (gci i28/cl, Generational).** After a non-idle finalizer round the post-resurrection closure scans only the objects popped since the last closure (P3/cl); arms only while `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` is on. Rows: vi28-cl S / C. |
| `CRATONVM_GC=jni-in-native-band-narrow` *(alias: `CRATONVM_JNI_IN_NATIVE_BAND_NARROW=1`)* | **Opt-in (gci i28/nb).** The in-native band's top is the outermost JIT entry's SP when the stack above the chain is proven free of compiled frames (P5/nb). Rows: vi28-nb S / E / C. |
| `CRATONVM_GC=jmx-diagnostic-command-mbean` *(alias: `CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN`)* | **Default ON since 2026-10-17 (gci i28/dg), `=0` kills.** Seeds `remoteDiagnosticCommandsSupport` so the `DiagnosticCommand` MBean is exposed; its four natives run the attach listener's commands (gci-i14mp item 11b). Rows: vi28-dg D. |
| `CRATONVM_GC=g1-remark-addr-keyed-mark-exact` *(alias: `CRATONVM_G1_REMARK_ADDR_KEYED_MARK_EXACT=1`)* | **Opt-in (gci i22/g1rk).** G1: after a completed remark + cleanup whose closure was trusted, every address-keyed side table (smuggled longs, Throwable traces, inherited buckets, TLS / JUL / lock-key / io / socket / zip / publisher / Undertow rows) is swept on the completed mark (`G1Collector::trusted_mark_liveness`), not on the region-granular verdict, so a dead key in a region the cleanup kept loses its row at the remark instead of at the mixed pause that evacuates the region. Deciding rows `vi22-g1rk.list` (FLIP RULE). `CRATONVM_DBG_G1_REMARK_ADDR_KEYED=1` prints the per-sweep counts. |
| `CRATONVM_GC=g1-conc-frozen-eager-scan` *(alias: `CRATONVM_G1_CONC_FROZEN_EAGER_SCAN`)* | **Default ON (gci i12/wb), `=0` kills it.** The concurrent-start pause scans every object a thread frozen by the JIT take-over holds, so a compiled store whose SATB decision preceded the pause cannot hide the value it overwrites (G1 twin of Generational's `initial_mark_with_frozen`). |
| `CRATONVM_GC=g1-itl-defer-at-mark` *(alias: `CRATONVM_G1_ITL_DEFER_AT_MARK`)* | **Default ON since gci vi6-g1t (2026-10-13), `=0` kills it.** G1 marking cycles defer never-started threads' inherited `ThreadLocal` values to the child `Thread` (`metadata_pin`) instead of rooting them; the remark's mark-exact bucket sweep makes it sound (`gcCalls=1` as on HotSpot; vi7-flip F-ITL). |
| `CRATONVM_GC=g1-workers=<n>` *(alias: `CRATONVM_G1_WORKERS=<n>`)* | Force the evacuation worker count; `=1` drains the parallel path serially, which separates a concurrency race from a logic divergence. Overrides `-XX:ParallelGCThreads`, which in turn overrides the machine-derived default |
| `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` | **Default ON** (gch h4/g1), `=0` restores the old order. G1's final remark runs reference processing first, marks what it keeps (policy-kept soft referents, finalizables, cleaner chains) and drains their closure, and only then reconciles class mirrors and defining loaders and unloads, on the final bitmap -- HotSpot's remark order. Before it, a class or user loader reachable only through a kept `SoftReference` was unloaded while its objects survived (the JDK's `LambdaForm` caches: every G1 GC notification after the first round failed). Pages `gch-h3os-g1-remark-unloads-...`, `gch-h2ob-g1-gc-notifications-fail-...`; rows vh4-g1 A. |
| `CRATONVM_G1_REMARK_ITL_SWEEP` | **Default ON** (gch h4/g1), `=0` kills; inert under `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP=0`. `--compatible`: the G1 remark sweeps the never-started threads' inherited-`ThreadLocal` buckets on the completed mark instead of the region-granular verdict, so a dead child's values are released at the first cycle that finds it unmarked. Page `gch-h3jx-g1-compat-unstarted-thread-...`; rows vh4-g1 C. |
| `CRATONVM_G1_NATIVE_OOME_DEBT` | **Opt-in** (gch h4/g1): arm the native funnel's post-OOME debt on G1 (paid with the complete marking cycle), as Generational does by default. Page `gcd-d10o-native-funnel-oome-debt-is-generational-only`; rows vh4-g1 F. |
| `CRATONVM_DBG=gc-stress=<bytes>` *(alias: `CRATONVM_DBG_GC_STRESS=<bytes>`)* | Force young GCs every N allocated bytes (Generational) |
| `CRATONVM_GC=par-threads=<n>` *(alias: `CRATONVM_GC_PAR_THREADS=<n>`)* | Generational young-GC worker count. `0`/`1` forces the sequential collector; `>= 2` forces that many workers regardless of heap size. Unset = `min(available_parallelism, 8)` once the young gen passes the size floor. `available_parallelism` follows CPU affinity, so a `taskset -c N` run is automatically sequential |
| `CRATONVM_GC=par-min-bytes=<bytes>` *(alias: `CRATONVM_GC_PAR_MIN_BYTES=<bytes>`)* | Young-gen size floor below which the young GC stays sequential (default 16 MiB) |
| `CRATONVM_GC=sync-young-wipe` *(alias: `CRATONVM_GC_SYNC_YOUNG_WIPE=1`)* | Zero the evacuated young semi-space INSIDE the pause, as before 2026-09-02. By default the memset runs on a helper thread after the pause (the arena is the next cycle's to-space, which no mutator allocates into) and is joined before the next collection; `[gcpause]` reports `wipe_deferred_bytes=`. The first lever to pull if a conservative root is ever reported inside the inactive semi-space |
| `CRATONVM_GC=-par-evac` *(alias: `CRATONVM_GC_PAR_EVAC=0`)* | Force the single-threaded evacuator for the Generational **moving** (Cheney) young cycle. Parallel evacuation is the default, but it engages only where `CRATONVM_GC_PAR_THREADS` policy already asks for two or more workers, the cycle is a moving one, and to-space covers from-space (a thin-slack cycle runs *bufferless* — one exact span per object off the shared cursor — rather than declining) — so a run that never sees it is common and expected. Workers run on a persistent `evac_pool` (the same pool type G1 uses, but the generational heap owns its own instance; parked on a per-worker condvar, never respawned per pause, and grown to the worker policy's width before every parallel copy), claim to-space in per-cycle buffers sized against the live set off one atomic cursor, promote through the old generation's allocator under a lock, and claim each object with a tagged CAS on its mark word (the same copy-then-CAS protocol G1 uses); a retired buffer's tail is stamped with a `TLAB_FILLER`/`GAP_FILLER` sentinel so the arena stays walkable as the next cycle's from-space. Both evacuators seed from the same three sources and reach the same forwarding map, but `=0` is **not** behaviour-neutral: to-space becomes the next cycle's from-space, and only the parallel arm leaves buffer-tail fillers in it, so the next cycle's object-start walk, anchors and trigger point differ. A defect that involves striding a filler disappears under `=0` and reads as "the parallel evacuator is at fault"; quote `gen_evac::par_evac_census().filler_bytes` for both runs (zero means that run had no fillers to compare). (The second lever, a parallel copy with no to-space buffers, was removed in gce e2: a bisection lever no round ran.) |
| — | `--verbose:gc` prints `[GC] par_evac: cycles=… helper_scans=… cas_losses=… declined_for_slack=… filler_bytes=… promotions=… deferred_cards=…` at exit, unconditionally, including the all-zero line. Read `cycles` first: a zero says the path never engaged, which is a different claim from "it engaged and did nothing". `helper_scans` says whether it was actually *parallel* — a cycle where the driver did everything and the helpers scanned nothing is a load-balancing regression that every correctness test in the suite passes. `promotions` and `deferred_cards` are the two arms whose absence would not fail immediately: promotion is the only shared-lock contention point in the copy phase, and a deferred card is the old→young edge whose loss surfaces a cycle later somewhere else — a zero in either means whatever you ran never exercised it. `declined_for_slack` should stay at 0; a young GC triggers with from-space ~99.9% full, so the slack the Cheney invariant leaves is small (measured: 121 KB out of 128 MB on bt18) and the cycle runs bufferless rather than declining. `filler_bytes` prices the per-worker buffering, and is 0 on a bufferless cycle. |
| — | **Driving the copy phase in a soak.** A plain benchmark run yields one moving cycle or none. `CRATONVM_DBG=gc-stress=250000` turns that into ~500–1000 per process, which is what makes a soak mean anything: `CRATONVM_GC=par-threads=8,moving-young CRATONVM_DBG=gc-stress=250000 cratonvm --XX:UseGc Generational -Xmx256m --verbose:gc -cp … BinT 14`. (The legacy `CRATONVM_GC_PAR_THREADS=8 CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_GC_STRESS=250000` spelling still works and warns once; note that two tokens of the SAME group go in one comma-separated value — a second `CRATONVM_GC=` assignment would replace the first.) Use oracles that are not a second run of this VM — `bench/BinT.java` sums to `10 * (2^(d+1) − 1)`, and `bench/HashMapOnly.java` / `bench/StringRegexOnly.java` document their checksums in their own headers. |
| `CRATONVM_GC=sweep-anchor-stride=<bytes>` *(alias: `CRATONVM_GC_SWEEP_ANCHOR_STRIDE=<bytes>`)* | Byte spacing of the parallel-sweep anchors (default 8 MiB). Also sizes the MOVING path's parallel object-start-walk chunks. Lower it to drive either on a small young gen under `CRATONVM_DBG_GC_STRESS` |
| `CRATONVM_GC=verify-rset` *(alias: `CRATONVM_GC_VERIFY_RSET=1`)* | After each young collection's old->young seeding, walk the whole old generation and report `[rset-verify] site=.. edges=N missing=M seeded=S`. `missing > 0` names an edge the card table did not deliver, and the first one's referrer/class/slot. **Read `edges` too**: `missing=0` on a run that found no edges at all is vacuous, not clean. Costs a full old-gen walk per young GC |
| `CRATONVM_GC=full-rset-scan` *(alias: `CRATONVM_GC_FULL_RSET_SCAN=1`)* | Restore the pre-2026-09-02 whole-old-generation old->young walk on every young collection. The revert lever for the default flip below; the first thing to try if a premature-reclamation defect is suspected under Generational |
| `CRATONVM_GC=young-trigger-percent=<n>` *(alias: `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<n>`)* | Moving young collection trigger, as a percent of from-space capacity (default 50, clamped 1..=95). Raising it collects less often and copies more survivors per cycle; see "Young sizing" below. The NON-moving sweep has its own, higher trigger — `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`, 90, not settable — because it reclaims in place and needs no Cheney headroom. Since the user's flip (2026-10-13) a heap built from `-Xmx` uses HotSpot Serial's eden instead (`CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN=0` restores the percent knob). |
| `CRATONVM_GC=-peer-pin-divert` *(alias: `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`)* | Let a moving young cycle proceed even when a conservatively-discovered JIT root was published this cycle. Restores the pre-2026-09-06 behaviour. **This is the single largest lever on whether a JIT-warm Generational run compacts at all** — the term it disables (`unrewritable_conservative_jit_roots`, see the `divert_non_moving` table below) fires on essentially every cycle once compiled code is live. It is off by default because a conservative pin landing in young-from names an object whose holder word nothing can rewrite; turn it on only to A/B a suspected over-diversion, and read `[GC] moving_young:` on both arms |
| `CRATONVM_DBG=gcpause` (alias `CRATONVM_DBG_GCPAUSE=1`) | Per-phase breakdown of the MOVING young cycle, printed for any collection over 100 ms. **The rows partition the pause since 2026-09-20**, as G1's `[GC-STAT]` line does: every figure is microseconds rendered `{:.3}ms`, and an `other=` residual is printed unconditionally, including zero, so the rows plus `other` equal the total by construction. Until that date there was no residual — everything before the first mark and after the last was charged to nothing — and every figure was `as_millis()`, so twenty sub-millisecond phases printed twenty zeros against a 12 ms total. **Any `[gcpause]` output quoted from before 2026-09-20 has both defects**, including the table under "Where a moving young pause actually goes" below. `docs/internal/gc/gengc-plumbing-gcpause-phases-do-not-sum-FIXED-20260923.md`. *(2026-09-23)* The 100 ms threshold is `CRATONVM_DBG_GCPAUSE_MIN_US` (token `CRATONVM_DBG=gcpause-min-us=<us>`; `0` prints every cycle), so the phase medians of small, stress-driven pauses can be taken; while a JFR recording runs the same marks are emitted as `jdk.GCPhasePause` events, `other` included |
| `-Xlog:gc`, JMX *(generational, 2026-09-23; round 5 changes in "Generational: what rounds 4 and 5 changed" below)* | The `-Xlog:gc` line's `before->after(committed)` is the heap's live occupancy (`VmHeap::live_bytes_estimate`: young `used - free_list` plus old used), not the bump high-water, which does not move across a non-moving young sweep and printed `8M->8M` on every pause of a run that was reclaiming. `ManagementFactory` reports HotSpot Serial's beans: collectors `Copy` (cycles that did not reclaim old gen) and `MarkSweepCompact` (cycles that did; a full cycle counts there only), pools `Eden Space` / `Survivor Space` / `Tenured Gen` with real usage. G1 (gc-common w7-f) reports HotSpot G1's beans: `G1 Young Generation` (every collection pause, young or mixed; `end of minor GC`), `G1 Old Generation` (full collections, which this G1 does not run, so 0 — HotSpot's `System.gc()` is a full collection there), `G1 Concurrent GC` (the marking cycle's initial-mark and remark + cleanup pauses; `end of concurrent GC pause`, cause `No GC`), and pools `G1 Eden Space` / `G1 Survivor Space` / `G1 Old Gen` (Old + humongous regions; committed = the committed heap minus the young pools) from one region census, with `getCollectionUsage()`, `getLastGcInfo()` and GC notifications. ZGC reports HotSpot 25's four beans (gcd d10/o retirement, 2026-10-04): every whole-heap collection — every collection of the default mode — moves `ZGC Major Cycles` (`end of GC cycle`) and `ZGC Major Pauses` (`end of GC pause`; mark starts count there too; since zs s4/ob3 a collection is booked there as the HotSpot pauses it has a real phase for -- `Pause Mark Start` (unless its mark start was its own pause), `Pause Mark End`, `Pause Relocate Start` -- cut at its own laps and summing to its pause, `CRATONVM_ZGC_PAUSE_PHASES=0` for one record; HotSpot books 3 per minor cycle, 5 per major, 8 per `System.gc()`: [`zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight`](internal/gc/zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight-CLOSED-20261019.md)), a young cycle of the opt-in generational mode `ZGC Minor Cycles` / `ZGC Minor Pauses`; pools `ZGC Young Generation` (always 0 used: this ZGC keeps no separate young space) and `ZGC Old Generation` (the heap), each managed by all four beans. `-Xlog:gc` prints HotSpot 25's `GC(n) Major Collection (<cause>)` start line and `GC(n) Major Collection (<cause>) 8M(3%)->2M(1%) 0.021s` end line, and every backend now prints its `Using` line (`Using G1`, `Using The Z Garbage Collector`) and a `gc,init` block. Until then ZGC answered JDK 21's non-generational `ZGC Cycles` / `ZGC Pauses` over `ZHeap` and `Garbage Collection (<cause>) a->b(c) t ms`, and G1 and ZGC printed no `Using` line ([`gcd-d10o-g1-and-zgc-gc-log-and-mxbean-shapes-differ-from-hotspot`](internal/gc/gcd-d10o-g1-and-zgc-gc-log-and-mxbean-shapes-differ-from-hotspot-FIXED-20261005.md)). The ZGC state lives in `HeapRealm::zgc_gc_beans` (`handoff-w7f-zgc-backend-gc-beans`, applied; `docs/internal/gc-common-round-20260923/gengc-r4w2-obs-g1-and-zgc-jmx-beans-are-still-the-legacy-pair-20260923-FIXED-20260923.md`). On G1 and ZGC the per-bean counts and times are the common GC event plumbing's (`gc_metrics::BackendGcBeans`, one per door pause, per VM), not the collector's own counters: a G1 evacuation-failure drain pass is not a second collection there. A marking pause's notification is delivered at the next drain point (the next collection's door, or the delivery thread's next batch). `getCollectionTime()` is real accumulated pause time on every backend: Generational per bean from the heap's pause totals, **the floor of the bean's microsecond sum, as HotSpot** (since gen r5w3/obs7; until then each pause was rounded up to a millisecond, so a run of sub-millisecond pauses reported one millisecond per pause — `GenR5W3CollectionTimeProbe` matches Serial now; H2's `collectGarbage()`, which loops until the value moves, may call `System.gc()` a few more times when full collections take under 1 ms, and terminates as it does on HotSpot — see `gengc-r5w2-obs6-collection-time-ceils-every-pause`); G1 and ZGC per bean from the plumbing's sealed durations (`pause_sum_as_collection_time_ms`: the floor of the microsecond sum, as HotSpot and the Serial beans; until the gce e1o fix, 2026-10-04, it added one millisecond per pause). The single-bean fallback (`NativeContext::gc_collection_time_ms`, `VmHeap::collection_time_ms`, still what a backend with no bean state answers) is G1's own microsecond pause sum rounded the same way, and on ZGC the plumbing's backend-independent sum (`HeapRealm::gc_pause_ns_total`, gc-common w6-f). G1 and ZGC pools support HotSpot 25's thresholds (the old pool both, the young pools the collection-usage one; checked at collection ends) and report `init` (the startup commit on the old pool, 0 on the young ones). Intended differences: G1's `System.gc()` is reported as what this G1 runs -- `Pause Young (System.gc())` on `G1 Young Generation` plus the marking cycle's pauses on `G1 Concurrent GC` -- where HotSpot runs and reports a full collection (`Pause Full`, `G1 Old Generation`); ZGC's young pool reads 0 (no nursery in the default mode); and every collection of the default (single-generation) ZGC, `Allocation Rate` ones included, is booked on `ZGC Major Cycles` / `ZGC Major Pauses`, where HotSpot 25 books its rate-driven young collections on the `Minor` pair: the default mode marks and relocates the whole heap, so a `Minor` record would claim a young-only collection that did not run (decision recorded by zs s6/ob5 on `zgc-zr9hp-allocation-collections-book-major-beans`; the opt-in `CRATONVM_ZGC_GENERATIONAL=1` books its young cycles on `Minor`). ZGC's non-heap `Metaspace` pool (zs s3/ob2) carries its measured figure on every cycle `GcInfo`, including collections that ran before the pool was first handed out (`CRATONVM_ZGC_METASPACE_FIRST_CYCLES`, zs s6/ob5), honours a usage threshold (checked when set and at each collection's end, as HotSpot; `CRATONVM_ZGC_METASPACE_THRESHOLD`) and keeps its collection-edge samples as peaks (`CRATONVM_ZGC_METASPACE_PEAK_AT_GC`). Since zt t1/ob (the zs / dev merge decision) ZGC lists HotSpot 25's five non-heap pools and `CodeCacheManager` / `Metaspace Manager` with gci i15/nh / i33/ms's figures, as every other backend, so the pool beans and `getNonHeapMemoryUsage()` agree; a pause `GcInfo` carries zeros for all five and a cycle `GcInfo`'s `Metaspace` / `Compressed Class Space` entries come from one edge walk (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`, `=0` for zs s3/ob2's `Metaspace`-only list); zt t3/ob: each code heap's `init` is HotSpot's 2555904, so the non-heap `init` is HotSpot's 7667712 (`CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`). Further intended differences of ZGC's `-Xlog` and `Runtime` figures (recorded by zt t3/ob; each names a HotSpot structure this collector does not have, so printing it would invent the figure): the twelve `gc,init` lines of HotSpot's colored-pointer reservation, variable medium pages and dynamic per-generation worker pools (`Address Space Size`, `Reserved Space Type` / `Size`, `Page Size Medium`, `GC Workers for Old/Young Generation`, `GC Workers Max`, ...); `Runtime` / `MemoryMXBean` heap `used` byte-exact, where HotSpot's is the sum of its 2 MiB-granular pages; the `gc,exit` page-cache lines (` Cache NM (N)`, `  size classes NM (N)`) and the `gc,stats` table (HotSpot's `ZStat` 10 s / 10 m / 10 h windows); and the uncommitter's `gc+heap=debug` `Timeout:` / `Cancel Next Cycle Timeout:` lines and paced passes (the info line `Uncommitter (0) Uncommitted:` matches). Also intended (recorded by zt t5/jf, local HotSpot 25.0.3 `Zt1JfVmOperations`): the pause count of a ZGC major. HotSpot runs 8 stop-the-world pauses / VM operations per `System.gc()` -- a promote-all young collection's `ZMarkStartYoung`, `ZMarkEndYoung`, `ZRelocateStartYoung`, then the major's `ZMarkStartYoungAndOld`, `ZMarkEndYoung`, `ZRelocateStartYoung`, `ZMarkEndOld`, `ZRelocateStartOld` -- plus `ZMarkFlushOperation` handshakes (requested by its `ZWorker*` threads) and one `ZRendezvousGCThreads`, and 5 pauses per other major; this VM books 3 on the `ZGC Major Pauses` bean, as `jdk.GCPhasePause` rows and as VM operations (`ZMarkStartYoungAndOld`, `ZMarkEndOld`, `ZRelocateStartOld`), the three agreeing. Its whole-heap collection has no promote-all young collection, no separate young and old marks or relocations, no VM-requested mark flush and no relocation-worker rendezvous, so the rest would be laps cut at invented points ([`zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight`](internal/gc/zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight-CLOSED-20261019.md); `Zt1JfVmOperations` compares the shared shape SAME, its `seq` mode prints the sequences, 8 vs 3). Since zt t5/jf (`CRATONVM_ZGC_XLOG_PAUSE_PHASES`) a ZGC collection's `-Xlog:gc*` block carries, between its `Major Collection` start and end lines, its three pause lines under `gc,phases` (`Y: Pause Mark Start (Major)`, `O: Pause Mark End`, `O: Pause Relocate Start`; `y:` for a minor) and one `gc,metaspace` line (`O: Metaspace: NM used, NM committed, NM reserved`); the rest of HotSpot 25.0.3's per-collection `gc*` block (local capture of `Zt1JfVmOperations`, 675 lines against this VM's 27) is an intended difference, each line naming a structure or a measurement this collector does not have: the generation sub-collection headers and their transition lines (`Y: Young Generation (Promote All)` / `(Collect Roots) 60M(23%)->36M(14%) 0.002s`, `O: Old Generation ...`), the concurrent phase lines (`Concurrent Mark`, `Concurrent Mark Free`, `Concurrent Reset Relocation Set`, `Concurrent Select Relocation Set`, `Concurrent Process Non-Strong`, `Concurrent Remap Roots`, `Concurrent Relocate`: this collector's mark, reference processing, selection and relocation run inside the pause), `gc,task` (`Using N Workers for Young / Old Generation`: dynamic per-generation worker pools), `gc,reloc` (`Using tenuring threshold`, the per-page-size Candidates / Selected / In-Place table, `Forwarding Usage`, the `Age Table`), `gc,alloc` (the per-phase allocation-stall table), `gc,load`, `gc,mmu` (HotSpot's ZStat load and minimum-mutator-utilisation windows), `gc,marking` (mark stripes and proactive / terminate flushes), `gc,nmethod` (registered / unregistered nmethods), the per-generation `gc,ref` Encountered / Discovered / Enqueued table (HotSpot's discovery counts; this VM's reference pass counts clears and enqueues, see the zs7jf JFR page), the `gc,heap` Heap Statistics and generation statistics tables sampled at Mark Start / Mark End / Relocate Start / Relocate End / High / Low (this collector samples the heap at the pause's two edges only), and the periodic `gc,stats` table (as above). The capacity lines `gc,heap Min / Max / Soft Max Capacity` are not printed yet (true figures exist; residual R9 on `zgc-zt1jf-jfr-review-residuals`). `--verbose:gc`'s shutdown census carries the same split as `[GC] jmx_collectors:` |

Note: `tracing::debug!` is compiled out of release builds
(`release_max_level_info`); for cycle-phase confirmation attach gdb to
un-inlined gc-crate symbols (LTO is off for `cratonvm-cli` dev builds).

**Tuning knobs** (java-compatible): `-Xmx` (reserved address space on every
backend, including the Generational old generation since 2026-09-23),
`-Xms` (committed at startup, **honoured on all three backends** since
2026-09-21 — see the heap-size note at the top of this
file), `-XX:G1HeapRegionSize=<bytes>` (since gci i11/gm an explicit value follows HotSpot:
raised to 1 MiB, rounded up to a power of two, capped at the heap's largest power of two; above
512 MiB the launcher prints HotSpot's range error and exits 1; `0` = ergonomic, which still
picks 1-32 MB), `-XX:InitiatingHeapOccupancyPercent=<n>` — a **ceiling**
on the adaptive threshold, never a floor, floored at max(1 % of heap, one
region) — `-XX:MaxGCPauseMillis=<n>` (the young-generation size target and the
mixed collection's copy-time budget), `-XX:ParallelGCThreads=<n>`,
`-XX:G1MixedGCLiveThresholdPercent=<n>` (default 85: an Old region at or above
this percent live is never a mixed-collection candidate),
`-XX:G1HeapWastePercent=<n>` (default 5: the mixed phase ends early once the
candidates' garbage is below this percent of the heap),
`-XX:MaxHeapSize` (= `-Xmx`; until gcg g3/x the launcher ignored it and the heap got the
ergonomic quarter of RAM), `-XX:InitialHeapSize` (= `-Xms` when no `-Xms` is given),
`-XX:MinHeapSize` (reported by `-Xlog:gc+init` as `Heap Min Capacity`),
`-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio` (validated as HotSpot 25 does, with its refusal
lines and exit 1; honoured by the Generational old generation's resize, defaults 40 / 70 as
HotSpot Serial), `-XX:+HeapDumpOnOutOfMemoryError`.

**Reading a G1 pause.** `--verbose:gc` prints one `[GC-STAT]` line per pause
whose phase fields are a *partition* of the pause, not a sample of it:

```
roots_us + rset_us + closure_us + fixup_us + free_us + verify_us + other_us == pause_us
```

`fixup_regions` / `fixup_bytes` are the denominator for `fixup_us` — a long
fix-up on a big old generation and a long fix-up on a small one are different
problems. `verify_us` is the budgeted post-pause dangling-reference sweep, which
runs in release builds (`CRATONVM_G1_VERIFY_BUDGET=0` opts out); it used to be
charged to no phase at all, so the rows did not sum and the difference was
invisible. `--verbose:gc` does not change the budget (since gci i7/g1y;
`CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC=1` restores the whole-heap sweep it
used to select). On the parallel evacuator phases 1-3 are fused into one work-stealing
closure and are reported wholly as `closure_us`, with `roots_us`/`rset_us` zero
— that is the honest reading, not a missing measurement.

## Backend details worth knowing

**Generational.** Young is a pair of semi-spaces with TLAB bump
allocation. The pair is half of `-Xmx` unless the operator passes `-Xmn`,
`-XX:NewRatio`, `-XX:NewSize` or `-XX:MaxNewSize` (honoured since 2026-09-23 on
this backend only; the old generation gets the rest of `-Xmx`, and every clamp
prints one `Warning:` line) — see `gc-tuning.md`, "Young generation size". G1
and ZGC print a one-line note and ignore the four flags.

*When a young cycle relocates, stated against the code.* The decision is
`gen_heap::collect_garbage_inner`'s `divert_non_moving`, and it has **six**
terms, any one of which selects the non-moving sweep:

| term | fires when |
|---|---|
| `!moving_young` | moving-young is not in effect for this cycle — either the operator opted out (`CRATONVM_GC=-moving-young`, alias `CRATONVM_NO_MOVING_YOUNG=1`) or this cycle's coverage proof failed. Subsumes the legacy rule (a live JIT frame, or an unregistered compiled frame on the stack, while moving-young is OFF) |
| `unrewritable_conservative_jit_roots` | **moving-young is ON**, a live JIT frame, *and* a conservative JIT scan ran this cycle (`gc_quiescence::conservative_jit_scans() > 0`). Added 2026-09-06; `CRATONVM_GC=-peer-pin-divert` (alias `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`) is the opt-out |
| `honor_promotion_oom_risk` | both generations ≥ 90 % full *and* conservative roots exist (or `CRATONVM_GC=promotion-oom-guard-broad`, alias `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`) |
| `divert_for_incomplete_moving_coverage` | this cycle's per-frame rewritable-root proof failed |
| `explicit_full_gc` | `System.gc()` asked for an old-gen-inclusive cycle — unless `CRATONVM_GC=system-gc-moving-young` (alias `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`, default off, 2026-09-23) withdraws the in-place promise: the root gatherer then roots class mirrors, loader metadata and collection overlays unconditionally on that cycle (no class unloading or overlay-only reclamation by it), and the cycle may compact young; the old-generation half still runs |
| `gpu_relocation_forbidden` | a GPU device held the arena in place past the collector's bounded wait |

Only `CRATONVM_DBG=force-moving` (alias `CRATONVM_DBG_FORCE_MOVING=1`) can carry a
cycle past the first three.

> **The first term lost a conjunct on 2026-09-21, and that is the repair for a
> measured defect.** It read `has_conservative_roots && !moving_young`, so on a
> process with **no live compiled frame** the opt-out selected nothing: no term
> of the OR fired and the cycle ran the Cheney copy, having already recorded
> `moving_young_requested=false` on its own decision line. Measured on `BinT 10`
> at `-Xmx256m` under `CRATONVM_DBG_GC_STRESS=250000`: 1512 of 1512 cycles
> `kind=moving reason=moving-no-jit-frames-live`. That is the configuration this
> page and `types/src/flags.rs` (the doc comment on `DEFAULT_MOVING_YOUNG`)
> both call the supported compatibility
> opt-out, and it is the only relocation bisect this backend offers — so an
> operator on a short repro that never tiered up set it, saw identical
> behaviour, and concluded relocation was not the cause. Filed as
> `docs/internal/gc/gengc-probe-no-moving-young-optout-does-not-opt-out-FIXED-20260923.md`.
>
> **Reason code.** A cycle diverted by the opt-out *alone* is recorded as
> `nonmoving-young-compaction-disabled` (since 2026-09-23; until then it borrowed
> `nonmoving-conservative-jit-roots` and the `--verbose:gc` line qualified it as
> `/no-moving-young-optout`). See
> `docs/internal/gc/gengc-plumbing-conservative-root-divert-reason-FIXED-20260923.md`.

**This page said for a month that a live JIT frame "does not by itself force
the non-moving sweep", and that the older `any JIT frame ⇒ non-moving` rule was
"reachable today only under `CRATONVM_GC=-moving-young` or
`CRATONVM_GC=-moving-young-jit-frames`" (aliases `CRATONVM_NO_MOVING_YOUNG=1` /
`CRATONVM_MOVING_YOUNG_NO_JIT=1`). Both claims are false on the shipped
default.** The second term above restores that rule almost exactly: every
mutator publishes a conservative JIT-root scan on its way into the pause, so on
a JIT-warm workload `conservative_jit_scans() > 0` holds on essentially every
cycle, and a live compiled frame anywhere in the process diverts the collection
to the non-moving sweep with moving-young switched on. What changed in 2026-09-06
is the *justification* (a conservative pin in young-from cannot be rewritten, so
the cycle must not relocate), not the outcome.

A JIT-warm workload therefore spends most or all of its cycles non-moving. That
is still a *measured* rate rather than a claim to be taken on trust, and the
running process states its own answer through
`gc_metrics::collector_decision_report()`. Since 2026-09-23 the three
situations that used to share one reason code are three histogram rows, each
with its own lever:

| reason code | term | lever |
|---|---|---|
| `nonmoving-unrewritable-conservative-jit-roots` | the 2026-09-06 term above (moving-young ON) | `CRATONVM_GC=-peer-pin-divert` (off by default for a soundness reason) |
| `nonmoving-conservative-jit-roots` | the legacy rule: a live compiled frame with moving-young OFF | turn moving-young on |
| `nonmoving-young-compaction-disabled` | the opt-out alone, nothing compiled | turn moving-young on |

(Until then all three were `nonmoving-conservative-jit-roots`, and a
`[GC] conservative-jit-root diverts:` line under the histogram approximated the
split from `moving_young_requested`; that line is gone. See
`docs/internal/gc/gengc-plumbing-conservative-root-divert-reason-FIXED-20260923.md`.)

That default young
collection is PARALLEL in two phases. The transitive closure is drained
by several workers over a lock-free mark bitmap (one bit per 8 bytes of
from-space) — sound because the phase is pure and read-only on a frozen
heap and the only write is an atomic bit claim. The sweep walk, a linear
header chase that is inherently sequential, is split at anchors the
ALLOCATOR supplies rather than ones a walk rediscovers. `arena.rs` keeps
one verified object start per 4 KiB bucket, armed on `new`/`grow`,
cleared on `reset`, and recorded by the TLAB-refill, young slow-path and
Cheney-copy paths for a shift, a bounds-checked load, a compare and a
per-bucket-once store under a lock the caller already holds. The grid is
completed by the END of every pre-existing free/TLAB-skip block -- a
sweep coalesces dead spans up to a survivor and never past one, so a
region that survived an earlier collection is never re-handed-out and
would otherwise contribute no anchor -- plus offset 0 and `used` as
terminals. Anchors landing inside a free block are filtered out, because
one minted by adjacent uncoalesced blocks would abort the whole parallel
attempt. This replaced a full-arena exact-base oracle walk costing ~240
ms and 2 GiB walked per collection; the same grid, subsampled at
`CRATONVM_GC_SWEEP_ANCHOR_STRIDE`, now costs ~0 ms and 4.7 MB walked,
and the conservative-candidate oracle traverses only those anchor
intervals that actually contain a candidate. The truncated-oracle fail-safe survives as `verified_spans`: an interval
counts as proved only if its chain lands EXACTLY on the next anchor, an
unproved interval has its ranges discarded rather than trusted, and a
candidate outside every proved span falls back to direct validation. Each chunk
re-proves its own anchor by requiring its chain to land exactly on the
next one, and the parallel walker writes nothing — on any grid anomaly
it is abandoned wholesale and the untouched sequential walk (which owns
every diagnostic and the unwind/re-anchor recovery) runs from scratch.
Parallel EVACUATION exists here since 2026-09-02 (`gc/src/gen_evac.rs`):
the transitive-closure copy runs on the same worker count as the mark, each
worker bump-allocating into its own to-space chunk and old-gen promotion
buffer, claiming each source object by a compare-and-swap of its mark word;
`CRATONVM_GC_PAR_EVAC=0` restores the sequential drain. The evacuated
semi-space is zeroed off-pause on a helper thread
(`CRATONVM_GC_SYNC_YOUNG_WIPE=1` restores the in-pause memset). The moving young gen's
JIT-held-oop corruption is fixed, and it is now the **default**
(`types/src/flags.rs::DEFAULT_MOVING_YOUNG`), with
`CRATONVM_GC=-moving-young` (alias `CRATONVM_NO_MOVING_YOUNG=1`) as the
compatibility opt-out.

**What the opt-out actually does, since 2026-09-21.** Every generational cycle
takes the non-moving young sweep, whatever is or is not compiled — `kind=non-moving`
on every `--verbose:gc` line and `moving=0` in the decision histogram. Because
`major_gc` (the old-gen **mark-compact**) has exactly one caller and it is on the
moving young path, the opt-out also stops the old generation compacting: old-gen
reclamation drops to the in-place `sweep_old_gen_non_moving`. *(Correction,
2026-09-23: `major_gc` compacts only when `CRATONVM_OLDGEN_COMPACT` /
`CRATONVM_GC=oldgen-compact` opts in — it has been off by default since
2026-08-03 — so on a default run the old generation never compacts on either
path, and `major_gc` is an in-place mark-sweep too. The opt-out changes which
young collector runs; it only changes old-gen relocation when that opt-in is
set. See `gc/src/old_gen.rs`'s module header. Since 2026-10-13 the opt-in is the
default: `CRATONVM_OLDGEN_COMPACT` and `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` are default ON
by the user's flip (WARNING gci-user-flip-20261013), `=0` kills each.)* So the flag is a
whole-heap "nothing relocates" lever, which is what makes it a usable bisect for
a suspected relocation defect — and also why it costs fragmentation headroom and
is not a throughput setting. Before that date it was none of those things on a
process with no live compiled frame; see the note under the `divert_non_moving`
table.

*Two diagnostics to know about on that run.* `[GC] moving_young:` is printed
only on the **Generational** backend (since 2026-09-23 — before that it also
printed on G1 and ZGC runs, counting those backends' pauses under the young
collector's name), and there only when `gc_quiescence::moving_young_enabled()`
is true or a coverage fallback was recorded (`VmHeap::print_gc_summary`), so
under the opt-out the line is **absent**. It is now absent for an honest reason — there is nothing it would
report but zeros — but it does mean the shutdown census cannot confirm the
opt-out held. Read `[GC] decision histogram: moving=0 non_moving=N skipped=K`
instead, which is printed unconditionally, or the per-collection `kind=` field.

The flag being
on is not the same as a cycle having compacted: every cycle must carry
its own root-coverage proof, and one that cannot prove complete coverage
diverts to the non-moving sweep rather than relocating — so read
`[GC] moving_young: moving_cycles_total=… non_moving_cycles_total=…
refused_cycles_total=… moving_cycles_under_live_jit=… coverage_fallbacks=…`, and the
`[GC] decision #N:` line rendered by
`gc_metrics::collector_decision_report()`, before attributing any
cost or behaviour to compaction.

**Read the first two fields; the third is expected to be zero and cannot tell
you what its name suggests.** This is the single account of that line — two
independent round-1 lanes reached it from different directions (a rename in
`vm_heap.rs`, and a reachability argument about the call site) and this
paragraph replaces both.

| field | source | what it can tell you | what it CANNOT |
|---|---|---|---|
| `moving_cycles_total` | `gc_metrics::decision_histogram()`, summed over the `moving-*` reason codes | how many generational collections relocated | — |
| `non_moving_cycles_total` | the same histogram, `nonmoving-*` codes | how many took the in-place sweep instead. The per-reason rows say **why** | — |
| `refused_cycles_total` *(2026-09-21)* | the same histogram, `skipped-*` codes | how many young cycles ran **neither** collector: to-space could not cover from-space, the from-space object-start walk did not complete, or a live mutator still owned a reserved TLAB tail inside the arena (T-3). Each has its own reason code, so the per-reason rows say which. A refused cycle over-retains and is retried on the next trigger | whether the heap recovered — read the following cycles' `kind=` for that |
| `moving_cycles_under_live_jit` | `gc_quiescence::moving_young_cycle_count()` | in principle, moving cycles taken while a compiled frame was live | **anything, in practice.** See below |
| `coverage_fallbacks` | `moving_young_coverage_fallback_count()` | cycles whose per-cycle root-coverage proof failed | — |

`moving_cycles_under_live_jit` is **structurally zero once compiled code is
live**, and a zero there is not evidence of anything. Its only bump site sits
behind `if moving_young && has_conservative_roots` — but twenty lines earlier
`unrewritable_conservative_jit_roots` requires the same two terms plus
`conservative_jit_scans() > 0`, and diverts the cycle to the non-moving sweep
before it can arrive; that count "is positive whenever anything is compiled"
(the term's own comment). So the conjunction is false in any JIT-warm process
and true only where nobody publishes a conservative scan — a unit test driving
the collector directly, or `CRATONVM_GC=precise-only-roots`. Measured
2026-09-20 on `BinT 14` under `CRATONVM_DBG=gc-stress=250000`:
`moving_cycles_total=1510 non_moving_cycles_total=2
moving_cycles_under_live_jit=0 coverage_fallbacks=0`
(`docs/internal/reviews/gengc-round1-probe-results-20260920.md`) — 1510 cycles
that demonstrably copied, and the JIT-active count reads zero because that soak
never exercised the JIT-frame path at all. Do not read it as "the collector
never moved under the JIT"; read it as "this run says nothing about that case".
The counter was kept under its honest name rather than widened or deleted
(`docs/internal/gc/gengc-core-moving-young-cycle-counter-inert-FIXED-20260923.md`;
`vm_heap.rs::print_gc_summary` still prints it).

**The key changed on 2026-09-20.** Until then the line's only cycle count was
keyed `cycles=` and carried the `moving_cycles_under_live_jit` number, so a run
with no compiled frames could relocate on every collection and still print
`moving_young: cycles=0 coverage_fallbacks=0` — which reads as "the young
generation never moved", the precise inverse of the truth, and is the reading
several earlier investigations started from (see
`docs/internal/jdk-only/G27-1-the-young-collection-that-never-runs-20260817.md`, whose separate point is that a DEFAULT run
is ZGC and never reaches this collector at all). **Any quotation of
`moving_young: cycles=N` from a log older than 2026-09-20 is a JIT-active count,
not a moving-cycle count**. (`ARCHITECTURE.md` and
`docs/feature-designs/default-moving-young-gen.md` used to instruct readers to
read the old field; both now name `moving_cycles_total`.)

Old gen is a free-list
allocator collected by a VM-driven concurrent cycle (initial mark STW →
concurrent trace → remark STW → concurrent sweep, with a remark-time
TAMS snapshot gating the sweep).
Since 2026-09-23 (round 4 wave 2) the cycle is OWNED — one driver at a time
CASes the shared phase `Idle → InitialMark` and every early exit abandons it —
and the concurrent trace runs in slices of `CRATONVM_GEN_CONC_MARK_SLICE`
objects (default 32 Ki; `0` restores the single unbounded trace) with the
old-gen lock released and a safepoint polled between slices, so promotions and
other threads' pauses wait for one slice rather than the whole closure. A remark
that loses its pause race joins the winner and retries (4 attempts). Since wave
3 the sweep is sliced the same way (the same knob, objects walked per slice; a
foreign old-gen free between slices stops it —
`docs/internal/gc/gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`),
Phase 2 drains the SATB log and repairs mark-queue overflow itself instead of
leaving both to the remark pause, and a finished cycle is counted as an old-gen
collection by the trigger. `CRATONVM_GC_OLD_GIVE_BACK` (opt-in) returns the
whole granules of large free blocks to the OS after an in-place old sweep that
leaves the generation below its 75 % trigger.

*Where a moving young pause actually goes, and the 2026-09-02 changes.*

> **These numbers were taken with a broken instrument and have not been
> re-measured.** At the time, the `[gcpause]` rows had no residual/`other` term
> and every figure was quantised to whole milliseconds, so they did not
> partition the pause the way the "before"/"after" columns assume. Concretely,
> the "after" column reads 22–39 + 0 + 32–63 + 28–52 + 0 = **82–154 ms** against
> a stated total of **100–133 ms**; the rows do not sum, the gap is not stated,
> and nothing in the output says whether the difference is unmeasured phases or
> quantisation. The instrument was repaired on 2026-09-20 (microsecond marks, an
> `epilogue` mark, an unconditional `other=` residual, and a `debug_assert`
> against double-counting — see
> `docs/internal/gc/gengc-plumbing-gcpause-phases-do-not-sum-FIXED-20260923.md`);
> **this table has not been re-run on it.** The direction of the 2026-09-02
> changes is not in doubt (the `full_old_rset_scan` row went to a hard zero),
> but the magnitudes are not citable to better than "large" until someone
> repeats the run.

`CRATONVM_DBG=gcpause` reports a per-phase breakdown for the MOVING cycle.
Before 2026-09-02, on `bench/OldGenRsetProbe 19 700 16` at `-Xmx1g`
(medians of the 12 collections after the retained set tenures, total median
pause 229 ms):

| phase | before | after | scales with |
|---|---:|---:|---|
| the from-space object-start walk | 120 ms | **22-39 ms** | young *allocated* |
| `full_old_rset_scan` -- the whole old-gen walk | 50 ms | **0 ms** | old live set |
| `cheney_drain` -- copying the survivors | 29 ms | 32-63 ms | young *live* |
| `cardclear+young_reset` | 23 ms | 28-52 ms | card count |
| `scan_dirty_cards` | 8 ms | **0 ms** | old-gen size |
| **total pause** | **229 ms** | **100-133 ms** | |

Only ~12 % of a minor collection copied live objects. The two largest phases
were O(young allocated) and O(old live set), which is the shape a generational
collector exists to avoid. Afterwards the copy is the largest phase, and only
6 of 14 collections still cross the 100 ms threshold `gcpause` reports at.
Five things changed:

* **The object-start walk is parallel.** It is split at the allocator's own
  anchor grid and chunked across `young_gc_threads()` workers, each chunk
  proved by requiring its chain to land exactly on the next anchor -- the same
  contract the non-moving sweep's parallel walk has always had, and which the
  MOVING (default) path did not use. Any refusal abandons the attempt
  wholesale and the untouched sequential walk runs from scratch against a
  FRESH bitmap, because a partially-filled one is worse than none. The walk is
  now its own `objstart_walk` phase mark with `objstart_chunks` /
  `objstart_parallel` counters beside it: `pre_evacuate` also covered the
  safepoint spin and the arena locks, and a 52 % attribution to a mark that
  wide was a hypothesis, not a measurement.
* **The whole-old-generation scan is off by default.** It ran AFTER the
  dirty-card scan had already answered the same question, and made young pause
  time grow permanently with old-gen size. `CRATONVM_GC_FULL_RSET_SCAN=1`
  restores it; `CRATONVM_GC_VERIFY_RSET=1` replaces it, running the same walk
  as a checker that prints `edges=N missing=M` -- the shape G1 already uses for
  its own remembered set.
* **The card map is no longer scanned with atomic RMWs.** `take_dirty_cards`
  used `swap(AcqRel)` on every card byte and `clear_all` stored over every byte
  again: two O(cards) locked passes per cycle, ~8.3 ns/card, measured linear
  from 98 K to 1 M cards while finding nothing. Both now read first (`Acquire`,
  a plain `mov`) and write only the bytes that are genuinely dirty.
* **The write barrier marks the card directly.** The interpreter/native
  barrier went through a TLS lookup, an `Arc`, a `parking_lot::Mutex` and a
  growable `Vec` per reference store, with no deduplication; it now performs
  the same conditional byte store the JIT's inline barrier emits, so there is
  one card-marking rule in the VM instead of two.
* **The compiled reference store is NOT part of this batch.** An inline SATB
  gate was written for it here and then withdrawn: `ref_store_pre_gate` (helper
  ABI v10) had landed on dev first and is a strict superset -- it gates the
  post barrier and a young-age floor as well, and does not require the field's
  old value to be null. Shipping a second mechanism into the same emitter is
  how `region_bounds_addr` came to mean two things at once. Note that those
  gates are published by ZGC only: `ref_store_gates()` requires all three
  slots and Generational cannot express its post-barrier as an age floor (it
  keys on `GC_FLAG_OLD_GEN`, a mask test), so under `-XX:+UseGenerationalGC`
  every compiled reference store still pays the helper call. Closing that is
  its own piece of work.

*Young sizing.* `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` (default 50) is the
percentage of from-space occupancy that triggers a moving collection. The 50 %
is documented as leaving room for survivors, but to-space has the SAME capacity
as from-space and promotion drains to old gen on top of that, so the copying
collector's real constraint permits considerably more. Raising it collects less
often and copies more survivors per cycle; which effect wins is a property of
the workload's survival rate, which is why this ships as a measurable knob at
its historical default rather than as a new default nobody has swept.
Under `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` a heap built from `-Xmx` uses
HotSpot Serial's eden instead (gch h4/fl flipped it and reverted it: conformance
held, the cost rule failed). **Default ON since the user's flip of 2026-10-13**
(WARNING gci-user-flip-20261013, `=0` kills it), together with the HotSpot age
clock (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`, `..._OLD_DIRECT`).
Survivors of a heap built from `-Xmx` age once per HotSpot Serial eden of young
allocation, not once per young cycle (`CRATONVM_GEN_AGE_BY_SERIAL_EDEN`, default
ON since gc wave 9 / lane gcC, `=0` kills; `gen_heap::serial_eden_age_epoch`):
the trigger and the cycle count stay, a cycle that has not seen an eden of
allocation since the last aging cycle ages nothing and tenures only by force,
and the tenuring survivor target follows that eden. It closes the medium-lived
tenuring divergence (`OldGenFinalizeProbe` and `GC21CMediumLivedWeak` 64/64 as
HotSpot at `-Xmx256m`); `[GC] young_trigger:` carries `age_by_serial_eden=
age_epoch_aging_cycles= age_epoch_held_cycles=`.

### Generational: what the gcf round changed (2026-10-01)

The gcf round ran seven waves of parallel lanes. Each wave was verified on Linux against HotSpot 25
(`-XX:+UseSerialGC` for Generational) by a probe battery of 340-471 rows. G1 and ZGC internals
were out of scope.

- **Round records.**
  - Summary, waves and verdicts: `docs/internal/gc-design-perf-round-20261001/README.md`.
  - Per-lane records: the `f1..f7-*-report.md` files in that folder.
- **Open items.** `docs/internal/gc/gcf-handoff-gc-design-perf-round-close-DONE-20261001.md` lists
  them in order. The JitOomRoot conservative-retention family and a suspected 4-9 % young-GC time
  step are there.

**Default ON since the round.** Each switch below has `=0` as its kill switch.

| Switch | What it does |
|---|---|
| `CRATONVM_GEN_SATB_OLD_ONLY` | The SATB pre-barrier enqueues only old-generation previous values. Young values are covered by the young collection. |
| `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` | A free-list hand-out inside a span the last sweep already zeroed is not zeroed again. |
| `CRATONVM_GEN_CONC_INLINE_START` | The concurrent cycle can start at the tail of the young pause that crossed its trigger. Before this, it was polled only after the next young collection. |
| `CRATONVM_GC_PREALLOCATED_OOME_KINDS` | One preallocated `OutOfMemoryError` per HotSpot message kind (`Java heap space`, `GC overhead limit exceeded`, ...), not one shared instance. |
| `CRATONVM_GC_ADAPTIVE_TENURING` | The tenuring threshold follows HotSpot's survivor target (`TargetSurvivorRatio` of the survivor space). Under `cfg(test)` it stays off for the fixed-threshold unit tests (a known compromise; see the handoff). |
| `CRATONVM_GC_JNI_ALLOC_LADDER` | JNI object and array allocation takes the OOME ladder: collect, then a graced retry, then the preallocated OOME. |
| `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS` | At each optimizing-tier call site, the compiled code zeroes the reference homes that no later use and no reachable deopt snapshot reads. A dead value in a live IR frame stops being a conservative root. |
| `CRATONVM_GEN_WALK_SIZE_MEMO`, `CRATONVM_GEN_SWEEP_BATCH_CENSUS`, `CRATONVM_GEN_SWEEP_3A_REPLAY` | The young allocation premium (lane q): the selective-promotion walk memoizes object sizes, the sweep census is batched, and fix-up 3a replays its recorded slots instead of re-walking. |
| `CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM`, `CRATONVM_JNI_GET_FIELD_ID_EXACT`, `CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST`, `CRATONVM_JNI_IN_NATIVE_REPEAT`, `CRATONVM_GC_MOVING_MAJOR_PROMOTION_SEED` | Lane fixes that shipped default-on: compiled `new` of compact classes, `GetFieldID`'s static/instance check, the in-native envelope order, repeated in-native entries, and the moving major's promotion seed. |

The concurrent marker's class-unloading side tables are now read once per drain instead of once
per object. This has no switch; `CRATONVM_GEN_CONC_UNLOAD_PER_OBJECT_LOOKUP=1` restores the old
path for the equivalence test.

**Deleted.** These switches have no readers left; setting them does nothing:
- `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED`;
- `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB`;
- `CRATONVM_GEN_YOUNG_TRIGGER_EDEN`;
- `CRATONVM_GC_IR_REF_MAP_ROOTS`;
- `CRATONVM_GC_CONC_START_STEP_BUFFER`;
- `CRATONVM_GC_NATIVE_OOME_DEBT_ALL`.

**Opt-in, awaiting a decision.** These are default OFF. Rows and verdicts are in the round folder;
each page's `STATUS (2026-10-01, gcf close)` line says what is missing.

| Switch | Area | State |
|---|---|---|
| `CRATONVM_GC_IR_DEOPT_IMAGE_RESIDUE_ROOTS` | An optimizing-tier deopt register block this activation never wrote stops being a root. | flipped in f6, reverted (`f6-verdicts.md`) |
| `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`, `CRATONVM_GEN_PINNED_COPY_DEFRAG` | The pinned young copy's exact roots, and defragmentation around pinned islands. Since gch h2/yg the defrag gate is also armed by a large (64 KiB or more) young miss on a free list holding 8x it (`pcd_large_misses`). | vf5: JitOomRoot 6/10; gch: both ride on the pinned-copy flip (the pinstale6 gate's battery half passed on h3a, its MANUAL rows could not run); vh2-yg E: the defrag gate's large-miss arm works (12 copies instead of 89) but reaches what the pinned copy alone does |
| `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` | Preallocated OOMEs carry no boot stack trace and no mutable state. | **Default ON since gcg g1b** (`=0` kills); see "what gcg waves g1-g3 and the tg1 triage changed" below |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS`, `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`, `CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY` | Old generation: the true-root young seed on every non-moving major (HotSpot's full-GC root set), a requested major that compacts, pinning only unrewritable words (band D). | true roots: **Default ON since gch h3/mj** (`=0` kills; see "what the gch round changed" below) -- correct on vg1/vg3/vg5/vg6; flipped in g6 and reverted on vg6 (total major time -21 % / +36 % per interleaved pair, +13..16 % per major), then flipped on a Java-visible parity defect, not on timing; requested compaction: correct, pause +16..+83 % per major (`og5_rqc_*`), a parity-vs-pause policy choice (`gcf-f1c-...`), **default ON since the user's flip of 2026-10-13** (WARNING gci-user-flip-20261013, `=0` kills); pinning: undecided (band D) |
| `CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED` | The megamorphic-table, PIC and MIC-hit dispatch doors stop pinning a call's arguments across a callee body that owns them (no handler, no frameless trap, not synchronized), so an argument the callee drops is collectable as in HotSpot. | gcg g6/jm: vg6 jm6_argsp_on = off (3-4 of 6 FAIL both); the probe's pinning door is not one of the three gated. d9d page. |
| `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` | Optimizing-tier frames publish a per-safepoint live set; the band scan skips the dead homes it proves. | **Default ON since gch h3/fd** (`=0` kills; see "what the gch round changed" below). gcg g6/jm: flipped on vg3/vg5, reverted on vg6 (jm6_pfl_jor_on 1 oome-thread-exit FAIL vs off 0/2); the 8+8 interleaved rows (vh2-fj B) then decided it. |
| `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER` | The young sweep and copy. | **Default ON since 2026-10-13** with the pinned young copy (the user's flip; gci i8/pe flipped this companion with it, WARNING gci-user-flip-20261013, `=0` kills) |
| `CRATONVM_LAMBDA_PROXY_TLAB`, `CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT` | TLAB refill and OOME policy. | **Default ON since gcg** (`=0` kills each): the lambda-proxy TLAB since the tg1 triage, the HotSpot overhead default since g1b; see the section below |
| `CRATONVM_DBG_JNI_REPEAT_AUDIT` | JNI. | undecided |
| `CRATONVM_REF_SETTLED_ROWS_UNTRACKED`, `CRATONVM_UNCAUGHT_RUST_RENDER` | References and uncaught-exception rendering. | undecided |

### Generational: what gcg waves g1-g3 and the tg1 triage changed (2026-10-01)

The gcg round's summary and its full flip and deletion tables are in
`docs/internal/gc-round-gcg-20261001/README.md`; the open items are in
`docs/internal/gc/gcg-handoff-gc-round-close-CLOSED-20261015.md`. This section lists what the
wave sections below do not.

**Default ON.** `=0` is the kill switch.

| Switch | When | What it does, and the evidence |
|---|---|---|
| `CRATONVM_JIT_INLINE_CARD_MARK` | tg1 | Compiled old-receiver reference stores check the card inline; a dirty card skips the barrier helper. gcg g1/p on vg1: old-receiver stores ~150x faster on both tiers, 3.5x on the single-pass tier, verify row clean. |
| `CRATONVM_LAMBDA_PROXY_TLAB` | tg1 | Capturing-lambda proxies try the thread's TLAB first. tg1: 60 s -> 10 s on 4 threads. |
| `CRATONVM_GC_OOME_WARM_TRACE_CLASSES` | g1/o | `StackTraceElement` and the `StackTraceElement[]` / `Throwable[]` mirrors are prepared at boot, so building an OOME's trace needs no class work. Born default ON; vg1: the control fired and the arm passed. |
| `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` | g1b | Preallocated OOMEs carry no boot stack trace and no mutable state; after HotSpot's pool of 4 traced errors per VM, a VM-raised OOME is the traceless default. vg1 `o1_*_pi_*` SAME 12/12 on all three collectors. |
| `CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT` | g1b | No GC-overhead limit, as HotSpot Serial. vg1 `o1_pcallee`: `hl` 0/6 = `def` 0/6, no timeout. |
| `CRATONVM_GC_CLASS_PREP_RECLAIM` | g1b/o | A class preparation refused by the heap on the initialization path collects once and retries (door 1 of the caught-OOME doors). Born default ON. gci i22/oo: also the reflective link (getDeclaredFields0 / Methods0 / Constructors0), one collection and one more link. |

`CRATONVM_GC_G1_NATIVE_MIRROR_OOME` (g3/o) is in the g3 table below. `CRATONVM_GC_HEAP_ALIGN_HOTSPOT`
(g4/yg) is in the g4 table.

**Deleted on the tg1 triage** (no gain; the code went in wave g4, listed again in its section):
`CRATONVM_TLAB_REFILL_THREAD_COUNTERS`, `CRATONVM_GC_REFILL_GATE_LOCKFREE`,
`CRATONVM_JIT_TLAB_ASSUME_ALIGNED`, `CRATONVM_JIT_SPLICE_TRUSTED_THIS_PUTFIELD`,
`CRATONVM_GC_PAR_EVAC_SPIN`, `CRATONVM_GEN_SWEEP_INCREMENTAL_COALESCE`,
`CRATONVM_GEN_SWEEP_3C_LISTED_SLOTS`, `CRATONVM_GEN_SWEEP_EVAC_RESERVE`,
`CRATONVM_GC_OLD_MARK_ADMISSION`, `CRATONVM_GC_OLD_ZERO_SKIP_ABOVE_HIGH_WATER`. vg2 later
measured a gain for the bitmap mark admission (steady major 283 ms vs 337 ms with the closure
bitmap alone); the form lives on as `CRATONVM_GC_OLD_SHARED_STARTS`.

**Opt-in, out of scope.** Built in g2/g3 for proposals the user later moved to band D
(`docs/internal/proposals/gc/`): `CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`,
`CRATONVM_STW_LOCKFREE_BLOCKED`, `CRATONVM_STW_COVERAGE_SLOT_NO_WAIT`, `CRATONVM_GC_TLS_SPAN_DROP`.
Default OFF; no round decides them.

### Generational: what gcg wave g3 added (2026-10-01)

Wave g3 of the gcg round (`docs/internal/gc-round-gcg-20261001/`, the `g3-*-report.md` files).
Each switch's deciding probe rows and pass rules are in its lane's report.

**Default ON.** `=0` is the kill switch.

| Switch | What it does |
|---|---|
| `CRATONVM_GC_G1_NATIVE_MIRROR_OOME` | A class mirror a native asks for on a G1 heap with no Free region left raises a catchable OutOfMemoryError instead of aborting the process (gcg g3/o). |
| `CRATONVM_G1_SURVIVOR_REGION_CAP` *(`=0` kills)* | **Default ON (gci i23/tn).** HotSpot's Survivor region cap: an evacuation pause claims at most `ceil(young target / SurvivorRatio)` fresh Survivor regions (`G1Policy::update_survivors_policy`'s `_max_survivor_regions`); a young copy that then finds no Survivor room is copied into Old (`allocate_in_next_plab`) and that worker's later copies go to Old for the rest of the pause. Shared-PLAB and serial evacuators; not on a compaction pause. `[GC-STAT]` adds `survivor_cap_promotions= survivor_cap_pauses=` once it bites. Rows: vi23-tn SC. |
| `CRATONVM_GC_NATIVE_MIRROR_RECLAIM` | **Default ON (gci i22/oo), `=0` kills.** `Object.getClass()` whose mirror must be minted on a full Generational or G1 heap collects and retries (as ZGC since zr z4/ab) instead of raising `OutOfMemoryError` with nothing collected. Rows: vi22-oo M. |
| `CRATONVM_GC_NATIVE_OOM_REPLAY` | **Default ON (gci i31/om), `=0` kills.** A restartable native (reflection listings, `Class.getName` incl. its LEAF body, thread stack natives, `--compatible` formatters, `Object.getClass`) whose allocation refused on Generational or G1 is re-run by the native funnel after a collection (as ZGC since zr z11/nd) instead of raising `OutOfMemoryError` with nothing collected. Rows: vi31-om O, G. |

**Deleted.** These switches have no readers left; setting them does nothing:
- `CRATONVM_XT_TAKEOVER_PARALLEL_SIGNAL` (gcg g3/h: the vf2 battery signalled 0 peers on every
  pass with it on; the take-over is the serial loop again).

**Opt-in, awaiting a decision.** These are default OFF.

| Switch | Area | State |
|---|---|---|
| `CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS` | A blocked peer's helper-window marking roots narrow to object starts (Generational only; pins unchanged). | **Default ON since the user's flip of 2026-10-13** (WARNING gci-user-flip-20261013, `=0` kills). gcg g3/h (rows `h3_jor_*`, `h3_census_*`, `h3_snd_*`); gci vi2-fr D: sound, no gain row. Also **Default ON since JIT r20 GC wave 9** (dev, lane gcD, host w9c; `=0` kills): JitOomRoot FAIL 0/8 on vs 1/8 off, on/off twins SAME, no stale or dangling line; the blocked-native derived-pointer audit (g3h item 6) is clean |
| `CRATONVM_GC_NATIVE_FACTORY_ATTEMPT_LADDER` | OOME doors: the native factory door tries after its young collection. (`CRATONVM_GC_MIRROR_AT_INIT` and `CRATONVM_GC_JIT_LADDER_UNIFIED` are default ON since gcg g6/oo.) | **Default ON since gch h3/os** (`=0` kills; see "what the gch round changed" below). gcg g6/oo had kept it opt-in (no measured benefit in vg3, vg5 `oo5-4`) |
| `CRATONVM_MONITOR_LAZY_WAKE_LIST`, `CRATONVM_STW_NO_BLOCKED_COUNTER`, `CRATONVM_LOCK_KEY_SHARDS` | Threads and monitors: a per-VM list of lazy parkers to wake, blocked transitions without the anonymous blocked counter, and a sharded lock-key registry. | gcg g3/k: `k3_lw_*`, `k3_nbc_*`, `k3_lks_*` decide. gch h3/jx: lock-key shards 0.998x at 16 threads, 1.11x slower at 32 (vh3-jx F); the lock-key shards switch stays registered and default OFF, not deleted (user triage, 2026-10-09) |
| `CRATONVM_JIT_SPILL_WRITTEN_SET` | Compiled-frame maps: a single-pass map publishes the blind-spill slots its safepoint wrote. | gcg g3/v: `v3_*` decide |

### Generational: what gcg wave g4 added (2026-10-02)

Wave g4 of the gcg round (`docs/internal/gc-round-gcg-20261001/`, the `g4-*-report.md` files).
Each switch's deciding probe rows and pass rules are in its lane's report.

**Default ON.** `=0` is the kill switch.

| Switch | What it does |
|---|---|
| `CRATONVM_GC_HEAP_ALIGN_HOTSPOT` | `-Xmx`, `-Xms` and `MinHeapSize` are rounded up to the collector's HotSpot `HeapAlignment`: 2 MiB on the Generational collector, as Serial does (`-Xmx9m` is a 10M heap); since gci i48/zl1 (`788e422f14`, zx48 A4) also `max(region, 2 MiB)` on G1 and the 2 MiB granule on ZGC (HotSpot oracle `-Xmx9m` / `-Xmx100000k` / `-Xms5000k` gives 10M / 98M / 6M on Serial, G1 and ZGC). Default ON, `=0` keeps the typed sizes on every collector. Launcher only (gcg g4/yg: `yg4_align_*` decide). |
| `CRATONVM_GC_OVERHEAD_CRAWL_BACKSTOP` | Generational only. Under the HotSpot overhead default (`CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT`, no GC-overhead limit), eight forced young cycles in a row that starved the program (old generation wedged, under 2 % of the heap allocated since the last forced cycle, live set flat) end the crawl with an ordinary `Java heap space` after a full collection, the way Serial's compaction ends it. Without it, `GenR4W6JitOomRootProbe` `oome-thread-exit` crawled for minutes on g2a (gcg g4/oo; `oo4_jor_*` / `oo4_thrash_*` decide). gch h2/ob: with it at `=0` the crawl no longer recurs (vh1-ob `ob1_crawl_bs0_*` SAME, max streak 5); its retirement is a user-triage proposal (`gch-h2ob-proposal-retire-the-overhead-crawl-backstop-CLOSED-20261015`). |
| `CRATONVM_GC_IR_DEOPT_IMAGE_NEVER_ROOTS` | An optimizing-tier body's frame-deopt register block is neither a root nor a pin at any scan, whatever its RBP image (the block is live only inside the stub's one call to `ir_deopt_entry`, which has no GC point). Flipped by gcg g4/jm on vg1+vg2: `GenR4W6JitOomRootProbe` `oome-compiled-callee` failed 5/20 default vs 0/20 with it; `jm4_nev_*` confirm on the build that carries the flip. |
| `CRATONVM_STRING_INTERN_RETURNS_RECEIVER` | `String.intern()` pools and returns its receiver on a miss (JLS, HotSpot); `=0` restores the fresh copy. Flipped by gcg g4/rs on vg2: `w2_identity_rv_*` SAME 6/6 on all three collectors, the `off` control DIFF 3/3 at the identity tokens. |
| `CRATONVM_GC_OLD_CLOSE_BITMAP` | The walked old-gen live-set closure admits through a per-call start bitmap (walks of 4096 objects or more) instead of a binary search per edge; `=0` restores the search. Flipped by gcg g4/og on vg2: steady major 427 -> 337 ms, old pause 1.79 -> 1.28 s, all rows SAME; `og4_v2_cb_*` add the shapes whose majors ran void. |

**Deleted.** These switches have no readers left; setting them does nothing (each deleted arm was
default off, so the default path is unchanged; triage tg1 measured no gain for the first four
items, vg2 for the last):
- `CRATONVM_TLAB_REFILL_THREAD_COUNTERS`, `CRATONVM_GC_REFILL_GATE_LOCKFREE` (gcg g4/jn: the
  refill path is the old default arm);
- `CRATONVM_JIT_TLAB_ASSUME_ALIGNED`, `CRATONVM_JIT_SPLICE_TRUSTED_THIS_PUTFIELD` (gcg g4/jm);
- `CRATONVM_GC_PAR_EVAC_SPIN`, `CRATONVM_GEN_SWEEP_INCREMENTAL_COALESCE`,
  `CRATONVM_GEN_SWEEP_3C_LISTED_SLOTS`, `CRATONVM_GEN_SWEEP_EVAC_RESERVE` (gcg g4/yg);
- `CRATONVM_GC_OLD_MARK_ADMISSION`, `CRATONVM_GC_OLD_ZERO_SKIP_ABOVE_HIGH_WATER` (gcg g4/og);
- `CRATONVM_TL_GET_ONE_GLOBAL_REF_LOCK` (gcg g4/xt on vg2: `ThreadLocal.get` +0.3 %, every
  pair within 0.5 %; `ThreadLocal.get` resolves its row under two global-ref locks as before).

**Opt-in, awaiting a decision.** These are default OFF.

| Switch | Area | State |
|---|---|---|
| `CRATONVM_GEN_CONC_OWED_POLL` | With no service thread, a due Generational concurrent cycle is owed to the next safepoint poll instead of running inside the failing allocation (the compiled poll tests the latch on x86-64). | gcg g4/oo: `oo4_owed_*` decide. gch h3/os: failed its flip rule a third time (vh2-ob E: TTSP outliers of 80 / 148 ms on the on arm only); it stays registered and default OFF, not deleted (user triage, 2026-10-09) |
| `CRATONVM_REF_DISCOVER_AT_ALLOCATION` | References (gcg-g2r discovery at allocation): a `java.lang.ref` reference is registered at its allocation and bound at the next collection, so the constructor bridges are no longer required. | gcg g4/rs: `rs4_gate_*`, `rs4_refs_on_*` decide |
| `CRATONVM_GC_HOTSPOT_INITIAL_HEAP` | Without `-Xms` the initial heap is HotSpot's `physical / 64` (launcher only). From `a4af35def8` to gci i48: ON on ZGC, opt-in (`=1`) on Generational and G1 (gci i20/mr finding 13). **Default ON on every collector** since the WARNING commit `dc72abfc64` (user decision 2026-10-17, marker `WARNING gci-i48-initial-heap-all-collectors-20261017`), `=0` kills on every collector; flipped against the failed vi1 peak-RSS rule (Gen `-Xmx2g` 187.7 vs 141.7 MB, the G1 default 554.8 vs 492.6 MB). | gcg g4/yg: `yg4_init_*`; the flip gate is startup RSS (MANUAL) |

### Generational: what gcg wave g5 changed (2026-10-02)

Wave g5 of the gcg round (`docs/internal/gc-round-gcg-20261001/`, the `g5-*-report.md` files).
Each switch's deciding probe rows and pass rules are in its lane's report.

**Default ON.** `=0` is the kill switch.

| Switch | What it does |
|---|---|
| `CRATONVM_GC_SP_STUB_OWN_MAP` | A single-pass frame parked in a post-call deopt stub (trace-only safepoint id) reads a map derived from its call's: the call's cursor, written set and java-local oracle, and its register mask cut to the callee-saved GPRs. Band claims only: the precise half and moving-young coverage still find no map. Flipped by gcg g5/jm: vg4 `Gcd1PinnedCalleeOomeProbe` 8/8 = HotSpot against a base failing 8/8; it replaced the three deleted switches below. |
| `CRATONVM_JNI_BULK_ARRAY_COPY` | `Get/Set<Type>ArrayRegion` copy the region with one memcpy when the array's element type is the region's (gcg g5/jn; vg4 `jn4_*_bulk_*`: 60-1000x faster on a 64 Ki-element round trip, HotSpot's output on Gen, G1, ZGC). |
| `CRATONVM_GC_POST_GC_CLOSE_ORPHAN_FDS` | The fds a dead `DatagramChannel` / synthetic `RandomAccessFile` left parked are closed at the post-collection Cleaner drain, as HotSpot's Cleaner closes them (common-w16c; gcg g5/rs: the OS check `bound=0` on Gen and `--compatible`, as HotSpot). |
| `CRATONVM_XLOG_GC_FULL_PHASES` | `-Xlog:gc+phases` prints Serial's `Phase 1: Mark live objects` .. `Phase 4: Move objects` on every Generational full collection (gcg g5/yg: vg4 `yg4_phases_*`), and since gci i30/ob G1 Full's five phase pairs on every G1 full collection (`gc_events.rs`; `=0` kills both). ZGC has no full-GC phases. Since gch h2/og the old collection stamps its own split (`OldGcPhaseSplit`): Phase 2 carries the reclamation (in-place sweep, or plan + slide), Phase 3 the post-compaction fix-up, Phase 1 the rest of the pause, Phase 4 prints 0.000ms; until then Phase 1 carried the whole pause (vh2-og E: the phases sum to the pause). |
| `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` | Humongous reference arrays zeroed outside the old-gen lock (under a same-footprint `long[]` disguise). Flipped by gcg g5/og: vg4 `og4_t6_*` 5/5 interleaved pairs faster at 8 threads (median +33 %), 4 threads no regression, every row `nonzero=0 ... ok`. `=0` restores the locked memset. |

**Deleted.** These switches have no readers left; setting them does nothing (each was default
off, so the default path is unchanged):
- `CRATONVM_XT_HELPER_CAPTURE_WINDOWS_ONLY` (gcg g5/xt, vg4: every repaired blocked-wake word
  came from a pure-native band, so the switch dropped the repair);
- `CRATONVM_XT_TAKEOVER_INTERIOR_INNERMOST` (gcg g5: no effect on the frozen-peer interior
  holder, jm XTI verdict);
- `CRATONVM_GC_SP_STUB_CALL_MAP_ROOTS`, `CRATONVM_GC_SP_STUB_IMAGE_CALLER_SAVED_DEAD`,
  `CRATONVM_JIT_STUB_ZERO_DEAD_SPILL` (gcg g5/jm: replaced by `CRATONVM_GC_SP_STUB_OWN_MAP`);
- `CRATONVM_GEN_YOUNG_FRAG_SPILL` (gcg g5/yg: vg3 `y1b_frag_prem_on` 3.2x the run time);
- `CRATONVM_GC_OLD_LIVE_SWEEP`, `CRATONVM_GC_OLD_LIVE_KEPT_BITMAP`,
  `CRATONVM_GC_OLD_LIVE_RESURRECT`, `CRATONVM_GC_OLD_LIVE_TRUE_ROOTS`,
  `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY` (gcg g5/og: the O(live) old-gen sweep lost to the walked
  collection on every measured shape).

### Generational: what gcg wave g6 changed (2026-10-02)

Wave g6 of the gcg round, passes 1 and 2 (`docs/internal/gc-round-gcg-20261001/`, the
`g6-*-report.md` files; pass 2 judged the vg5 battery). Each switch's deciding probe rows and pass
rules are in its lane's report.

**Default ON.** `=0` is the kill switch.

| Switch | What it does |
|---|---|
| `CRATONVM_JIT_CARD_STORE_BARRIER` | Compiled reference stores into an old object mark a CLEAN card inline (and its summary group), under the view's scan-bound mirror, instead of calling the barrier helper; both tiers, putfield and aastore. gcg g6/jm: vg5 clean-card totals within 2x of the single-pass tier (off ~15x), forms 21 ok both tiers, rset-verify missing=0, conc / hashmap / G1 / ZGC SAME. |
| `CRATONVM_OVERLAY_OWNER_ONCE_PER_SLOT` | A collection overlay's owner row is registered when its slot is minted, not on every overlay operation. gcg g6/xt: vg5 `xt5_ov_*` 0.71x, 6/6 pairs, all `bad=0`. |
| `CRATONVM_GC_MIRROR_AT_INIT` | A class's java.lang.Class mirror is created on its initialization path (HotSpot has every mirror from load), so a native's first getClass() on a full heap does not fail (gcg g6/oo; vg5 `oo5-5` wall within 5 %, SAME 8/8). |
| `CRATONVM_GC_JIT_LADDER_UNIFIED` | The JIT helpers' latched GC-overhead exit decides as the interpreter's and the native door's do (one more attempt in two cells where the JIT threw) (gcg g6/oo; vg5 `oo5-3` `PASS all 8` 5/5, no timeout). |
| `CRATONVM_GC_OOME_DEBT_SKIP_CONSTRUCTION` | While a VM-raised OutOfMemoryError is being constructed, its natives do not pay the post-OOME debt (two futile majors on the unchanged heap); the first native after the construction pays (gcg g6/oo; vg5 `oo5-6` SAME 4/4, shorter in 2/2). |
| `CRATONVM_GC_OLD_SHARED_STARTS` | One walked start bitmap per STW major admits every mark and closure edge (no per-edge binary search). gcg g6/og: vg5 `og5_ss_*` SAME 20/20; live-heavy probe faster in 5/5 pairs, steady probe not slower. `=0` restores the binary search / per-call closure bitmap. |
| `CRATONVM_GC_OLD_BORROW_YOUNG` | The old generation may grow in place into young's budget after a refusal (reserved to 2/3 Xmx), as HotSpot's tenured grows. gcg g6/yg: vg5 `yg5_borrow_promofail_*`: a 48 MiB live set at `-Xmx64m` ran 3/3 where it threw `OutOfMemoryError`. |
| `CRATONVM_VERBOSE_GC_IS_XLOG_GC` | Launcher: single-dash `-verbose:gc` prints HotSpot's `[info][gc]` lines on stdout (it means `-Xlog:gc`); `--verbose:gc` keeps this VM's `[GC]` diagnostics. gcg g6/yg: vg3 `x3_verbose_gc_*`, vg5 `yg5_isverbose_*`. |
| `CRATONVM_GC_WEAK_STRING_TABLE` | Generational only: the String intern table is a weak root at every stop-the-world pause with no concurrent cycle open, swept in the pause epilogue (`vm/src/memory/string_table.rs`); G1 and ZGC keep the strong pool, and `=0` restores it. gcg g6/rs: vg5 `rs5_wst_conc_*` checksums equal their `=0` twins with a concurrent cycle open; vg2-vg4 reclaim, unload, literal and gc-stress rows. |
| `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD` | Default ON (gci i32/sl), every collector: an `ldc` of a lone-surrogate String literal keeps its surrogate-pool row WEAK and records the literal in its class's never-evicted constant record (HotSpot's resolved references), so the literal dies with its last class and its row clears (`constants.rs` `intern_wide_string_literal_or_oom_as`, `record_wide_string_literal_as_of`). `=0` restores the strong row (the literal lives for the VM's life). Deciding rows `vi32-sl.list` U / K. |
| `CRATONVM_GC_SURROGATE_CONSTANT_VALUE_INTERNED` | Default ON (gci i39/rw; `=0` kills), every collector: a `static final String` whose `ConstantValue` carries a lone surrogate is seeded at preparation with the interned literal (the surrogate pool's instance, claimed if new; class-held weak row under `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD`, the static field holding it), as HotSpot's `string_initial_value` interns it, so `Field.get`, an `ldc` of the text and `intern()` answer one object (`vm_util.rs` `prepare_class_shared`, `constants.rs` `intern_wide_string_constant_value`). `=0` restores the unpooled String. Deciding rows `vi39-rw.list` C. |
| `CRATONVM_GC_CONDY_STRING_ARG_RECORD` | Default ON (gci i33/sc), every collector: a `CONSTANT_String` static argument of a condy bootstrap or of an `invokedynamic` generic bootstrap (`invokedynamic.rs` `link_generic_call_site`) is recorded at its own constant-pool entry on `ldc`'s terms (HotSpot's resolved references), so its class's record owns it -- an ordinary String no longer dies under the weak String table while its class lives, and a lone-surrogate one keeps a WEAK pool row and dies with its last class (`constants.rs` `condy_string_arg`, `condy_wide_string_arg`; the wide half also needs `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD`). `=0` restores the unrecorded arguments (the lone-surrogate one on a strong row). An indy lone-surrogate argument is passed with its exact UTF-16 units whatever the switch (it was U+FFFD before i33). Deciding rows `vi33-sc.list` U / K / I / IK. |
| `CRATONVM_GC_SYSTEM_GC_LEAVES_BUSY_FINALIZERS` | Default ON (`=0` kills): under `CRATONVM_FINALIZER_THREAD=1`, a lock-free `System.gc()` (and the VM-internal / JVMTI forced collections) whose 2 s wait for a busy delivery thread runs out returns and leaves the queued finalizers to that thread, as HotSpot's `System.gc()` never runs a finalizer on its caller; `runFinalization()` still drains. Off: the caller runs them inline, as before. | i35/jf; rows `vi35-jf.list` S |
| `CRATONVM_GC_REMARK_RAISES_REF_WORK_HINT` | Default ON (`=0` kills): a G1 or Generational remark whose reference round queued `finalize()` or cleaner actions raises the "reference work queued elsewhere" hint, as every collection pause does, so the next interpreted allocation drains them instead of the next collection (`remark_reference_rows` / `remark_note_ref_work_queued`). Off: the work waits for the next collection's pause. | i42/rp; rows `vi42-rp.list` R |
| `CRATONVM_METASPACE_HOTSPOT_FIGURE` | Default ON (gci i32/mf; `=0` kills). Every `Metaspace` number (MemoryPool `Metaspace`, `-Xlog:gc+metaspace`, `GC.heap_info` / gc,exit, jstat, JFR `MetaspaceSummary` and its `gcThreshold`, non-heap usage thresholds) is a per-class model of HotSpot 25 `-Xshare:off`'s `MetaspaceUtils::used_bytes()`; `=0` reports the size of the VM's reader structures. Rows `vi32-mf.list` P/X. |
| `CRATONVM_METASPACE_HOTSPOT_SPACES` | Default ON (gci i33/ms; `=0` kills). The metaspace model states HotSpot 25's two spaces: the `Compressed Class Space` pool, `-Xlog:gc+metaspace`'s `NonClass:` / `Class:` parts, the `class space` line of gc,exit / `GC.heap_info` / SIGQUIT / the debug Metaspace blocks, JFR `MetaspaceSummary.classSpace` / `dataSpace`, jstat `CCSC` / `CCSU`; committed is HotSpot's 64 KiB-granule figure per arena (pool, `MC`, xlog, exit lines, JFR, its `gcThreshold` step), reserved HotSpot's (64 MiB non-class nodes + `CompressedClassSpaceSize`). `=0`: committed = reserved = used, no class space (pre-i33). Rows `vi33-ms.list` S. |
| `CRATONVM_METASPACE_LIMIT_HOTSPOT_CHECK` | Default ON (gci i33/ms; `=0` kills). `-XX:MaxMetaspaceSize` refuses a define (after the two collections) when the model's committed metaspace of every live class, boot classes included, plus the new class exceeds the limit (HotSpot's `MetaspaceGC::allowed_expansion`); summed incrementally on the class manager. `=0`: the non-boot class-file bytes, which refused later than HotSpot. Rows `vi33-ms.list` L. |
| `CRATONVM_JUL_HANDLER_PUBLISH_THROWS` | Default ON (`=0` kills): a JUL handler's `publish` exception leaves the `--compatible` logging natives, as HotSpot's `Logger.log(LogRecord)` (no catch); off, both handler loops swallow it as before. Not under the switch (HotSpot's behaviour, i32/jh): a `LogRecord` constructor or filter failure of `log(Level, String, Object[])` / `entering` / `exiting` / `throwing` propagates, a null message is published, and `logp` prints no console line for a failed publish. | i32/jh; rows `vi32-jh.list` F. Inert on the vi32-jh probe doors (JDK bytecode runs under `--compatible`; i35/jh). |
| `CRATONVM_JULI_NO_CONSOLE_SINK` | Default ON (`=0` kills): under Tomcat's `org.apache.juli.ClassLoaderLogManager` a JUL record no handler took is dropped, as on HotSpot (no JULI configuration writes stdout); off, the `--compatible` natives print it as `LEVEL [name] msg` on stdout as before. | i38/jb; rows `vi38-jb.list` J |
| `CRATONVM_JBOSS_NO_CONSOLE_SINK` | Default ON (`=0` kills): under jboss-logmanager's `org.jboss.logmanager.LogManager` a JUL record no handler took is dropped, as on HotSpot; the six jboss-logging backends' `doLog` / `doLogf` console stand-ins are not registered (their library bytecode runs; -12 retired-shadow stubs); the `org/jboss/logmanager/Logger` `logRaw` / `log(LogRecord)` stand-ins drop an undelivered record and `log(Level, Supplier)` runs the JUL overload's gate-publish-drop body. Off: the stdout `LEVEL [name] msg` sink and the twelve stand-ins as before. | i40/jb; rows `vi40-jb.list` B / S (J4: whether either console-sink gate is live on any real-JDK door) |
| `CRATONVM_GC_STRING_TABLE_FUNNEL` | The String intern table's single insert funnel: a pooled String is allocated with no pool lock held and inserted if absent (the race loser answers the winner); on Generational under the weak table the funnel also keeps a young-entry list (each row the key and the String's current address, kept exact by every pause since gcj j1/st) so a young pause judges, removes and remaps only the listed entries without hashing their texts, and the pre-GC watch reads the list (`vm/src/memory/string_table.rs`). Opt-in (gci i25/st2, steps 1-2 of `gcg-g2w-proposal-string-table-keyed-by-the-string`); deciding rows `vi25-st2.list` (FLIP RULE) and the j1-st report's blocks A and C. |
| `CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE` | Generational under the weak String table: a String the VM's pooled constructors pooled since the last pause is rooted through the next pause (`gcg-g4rs` blanket mitigation for Rust holders across a non-moving pause). Opt-in (gcj j1/st): one pause of Java-visible over-retention. |
| `CRATONVM_TLAB_SHARE_SIZER` | HotSpot-shaped per-thread TLAB sizing, on the Generational heap and, since gci i48/zl3 (`9fda97cd08`, zx48 A7), on G1 (`alloc_policy_defaults::TLAB_SHARE_SIZER_G1`, `=0` kills); ZGC stays OFF (its VM TLABs are sized by `CRATONVM_ZGC_VM_TLAB_SHARE`). gcg g6/jn: vg5-jn C, 3 interleaved pairs: park 82-88 -> 37 minors, skew 116 -> 113, rate a tie. |
| `CRATONVM_GC_ESCALATED_YOUNG_BOOKS_COPY` | A young pause that also ran an unrequested major counts under the `Copy` bean too, with its young half's time (HotSpot books every young collection); a requested major (`System.gc()`, the allocation ladder's) books `MarkSweepCompact` only. gcg g6/yg; `yg4_beans_gen_*` on the g6 build decide. |

**Fixes without a switch.**
- The parallel young copy no longer aborts the process (`FATAL: parallel GC could not relocate a
  live object`) when both generations are 90 % full or more: such a moving cycle copies on the
  serial evacuator (gcg g6/yg).
- The pinned young plan no longer stops at a recorded start whose header does not size: it warns
  and resumes at the next recorded start, so the rest of the pinned page stays in place (gcg g6/xt).
- `Throwable.getStackTrace()` builds its `StackTraceElement`s through the reclaiming allocation,
  so a trace read after a caught `OutOfMemoryError` collects before it throws (gcg g6/oo, door 4).
- `CRATONVM_GC_MIRROR_AT_INIT` is read once per VM, not once per class initialization (gcg g6/oo).
- The JIT allocation class cache's opt-out is read once per VM instead of from a process static
  (gcg g6/jm).

**Deleted.** No readers are left; setting one does nothing:
- `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` (gcg g6/yg: it failed alone, vg1 `y1_d4m`, and with
  `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN`, vg3 `c1b_tc_*` +17 %). Its
  `[GC] young_sweep_trigger:` shutdown line is gone with it.
- `CRATONVM_JIT_IR_AASTORE_POST_BARRIER` (gcg g6/jm: superseded by the card-store form, now
  default ON, which needs no call and also covers putfield and the single-pass tier);
- `CRATONVM_JIT_SPILL_ZERO_PUBLISHED_REGS`, `CRATONVM_JIT_SP_LOCAL_LIVENESS` (gcg g6/jm: no row
  argued for either; vg2 / vg3 arms identical);
- `CRATONVM_XT_HELPER_WINDOW_JIT_ONLY`, default ON since g5, so this changes the default path
  (gcg g6/xt: it skipped every pure-native blocked peer, the population the blocked-wake
  stack-slot repair writes; vg5 `xt5_cap_g1_*` wrote 0 words against 259 / 163 on g4b). Every
  blocked peer reaches the helper-window pass again.

The `[GC] oldgen_sizing:` shutdown line lost ten always-zero keys with their counters
(`oldsz_live_sweep*`, `oldsz_true_root_young_freed_bytes`, `oldsz_mark_admission_bots`,
`oldsz_zero_skipped_bytes`; gcg g6/og). `CRATONVM_OLDGEN_COMPACT` stayed opt-in: correct, but
+26 % per major on vg5 `og5_cw_*`, the same parity-vs-pause choice as the requested-major
compaction (`gcf-f1c-...`). The user chose parity on 2026-10-13: both are default ON
(WARNING gci-user-flip-20261013, `=0` kills each).

**Opt-in, awaiting a decision.** Default OFF.

| Switch | Area | State |
|---|---|---|
| `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS` | The young-root guard excludes words of snapshots deposited under the conservative local probe. | **Default ON since gch h2/yg** (`=0` kills; see "what the gch round changed" below) |

### Generational: what the gcj round changed (2026-10-19 → 2026-10-20)

The gcj round ran three waves of parallel lanes on the Generational backend and the common
infrastructure (j1: 9 lanes, j2: 9 lanes, j3: 5 lanes); G1 and ZGC were out of scope, so every
shared change is gated to Generational unless it is a collector-independent bug fix (said below).
Lanes read and wrote code only; the orchestrator built each merged wave on Windows (no-LTO release),
ran the unit suites and every lane's battery rows against Temurin 25.0.3 `-XX:+UseSerialGC`
(stdout byte-compared), sent failures back to the lane that wrote the code, and retired pages.
The Linux build host named for the round refused the SSH key, so everything ran locally.

- **Round records.** `docs/internal/gc-round-gcj-20261019/`: `orch-notes.md` (authoritative),
  `LANE-RULES.md` (the three wave tables), `triage-j1.md` (flip triage of older opt-ins), and the
  `j<wave>-<lane>-report.md` lane reports with each switch's rows and rule.
- **Open items.** The `gcj-*` pages in [`known-issues/gc/README.md`](known-issues/gc/README.md).

**Default flips** (WARNING commits, `git log --oneline --grep=WARNING:`; `=0` kills each; Generational
only, G1 and ZGC keep the opt-in):

| Switch | Evidence |
|---|---|
| `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK` | hashmap-pair 10x faster (20 vs 230 young GCs); alloc-ring 1.14x was traced to a second memset and removed by `CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY` (j2/yg2) |
| `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT` | vi21-pr E1 / S / C1; hardened in j3/x2 (only the arming thread's gather consumes the arm) |
| `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` | HotSpot Serial keeps a referent only a dead finalizable reaches (vi16-fr); its walk is counted (`[watchref] stw-probe:`) since j3/x2 |
| `CRATONVM_JNI_FOREIGN_ALLOC_LADDER` | a foreign-attached thread's refused JNI allocation collects and retries as on HotSpot (block FA) |
| `CRATONVM_JNI_VIRTUAL_UPCALL_SELECTS` | `Call<Type>Method*` runs the method ID's own method (block U, regression rows jk15 / mf12 / cr10 SAME) |
| `CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX` | pinned compaction 0.80x at `-Xmx1g`, 0.89x at 64m, CPU 0.95-0.98x (j3/og3 rows with a probe that compacts) |

**New default-ON fixes** (each with a `=0` kill; Generational unless noted): ec47 on all three seed
paths (`CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`, `CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`:
the old children of a dead young finalizable clear their weak references before `finalize()`, and the
default-path old-depth reference parity gap closed with them); `CRATONVM_GEN_REFERENT_READ_KEEP_ALIVE`
(Unsafe / JNI / lambda referent reads, plus the CAS witness since j2/x1);
`CRATONVM_GC_WATCHED_SET_PER_HEAP`; `CRATONVM_GEN_SOFT_CLEAR_ALL_ADVANCES_CLOCK`;
`CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN` (an old-gen growth before initial mark no longer wastes the
cycle; the capture re-reads the extent inside the pause since j3/x2);
`CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT`, `CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED`,
`CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED`; the parallel evacuator's and `claim_in_place`'s CAS retry on a
lost non-forward word (plain fix); `CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`,
`CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR`, `CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES` (was `abort()`), 24-bit
TLAB totals counters; `CRATONVM_JNI_ARRAY_VM_LIMIT`, `CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES`;
`CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK` (an XNIO / Vert.x loop thread retires itself: a pause no longer
waits for it); `CRATONVM_GC_HANDSHAKE_LOOP_KICK`, `CRATONVM_GC_HANDSHAKE_SPIN_WAIT` (Windows 15.6 ms
condvar ticks); launcher and observability parity: `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS`,
`CRATONVM_GEN_HOTSPOT_VALUE_RANGES`, `CRATONVM_GEN_SERIAL_SELECTOR_SILENT`,
`CRATONVM_GEN_JCMD_STRINGTABLE_LIVE`, `CRATONVM_GEN_JCMD_HOTSPOT_TEXTS`,
`CRATONVM_GEN_JFR_NAME_MAXSIZE_OPTIONS`, `CRATONVM_GEN_XLOG_SAFEPOINT_STATS`,
`CRATONVM_XLOG_GEN_TLAB_TOTALS_ALONE`, `CRATONVM_GEN_THRESHOLD_SET_DRAIN`. Collector-independent bug
fixes on every backend: `CRATONVM_JIT_MH_SITE_IDENTITY` (MH site memos survive a `JitInvokeInfo`
address reuse), `CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED`, `CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM`.

**Built opt-in, waiting for deciding rows** (each page / report names them): old gen
`CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT` (mark 0.95x, borderline), `..._PINNED_COMPACT_REUSE_CLOSE`,
`..._COMPACT_TRUST_MARK`, `..._PAR_ADJUST`, `..._MARK_PREFETCH`, `..._SHRINK_FACTOR`,
`..._CAPACITY_SHRINK`, `..._FREE_RATIO_GROWTH`, `CRATONVM_GEN_TENURED_POOL_MAX_FIXED`
(`CRATONVM_GC_OLD_TRIGGER_SERIAL` refused: 6 majors per run vs HotSpot's 1); young
`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` (its predecessor `..._MIRROR_ROOT_WHEN_PROMOTABLE`
kept dead loaders from unloading and went back to opt-in), `CRATONVM_GEN_PINNED_COPY_OBJECT_WINDOWS`,
`CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`, `CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`,
`CRATONVM_GEN_SERIAL_EDEN_NO_PAUSE_GOAL`; JIT barriers `CRATONVM_JIT_POST_ONLY_CARD_MARK` (36 -> 18
instructions on the single-pass dirty-card store), `CRATONVM_JIT_CARD_BYTE_MAP_BASE`,
`CRATONVM_JIT_IR_FRESH_STORE_NO_BARRIER`, `CRATONVM_JIT_IR_SATB_GATE_ONCE`; JNI
`CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`, `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB`; safepoints
`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, `CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`,
`CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE`; references / string table
`CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE`, `CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`,
`CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`, `CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE`; launcher
`CRATONVM_GEN_PRINT_FLAGS_FINAL`, `CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`, `CRATONVM_JFR_STARTUP_BANNER`.
Two og j1 switches that shipped default ON without a measured gain went back to opt-in
(`CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT`; the forward index came back ON on og3's rows).

**Kept OFF by triage** (`triage-j1.md`): `CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS`,
`CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED`, `CRATONVM_JIT_LEAN_CARD_POST`,
`CRATONVM_JNI_IN_NATIVE_BAND_NARROW` (never engages; the refusal is now counted by reason), and the
VOID ones (`CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL`, `CRATONVM_GEN_YOUNG_LOADER_PIN_SNAPSHOT`,
`CRATONVM_EXIT_HOOK_GC_SAFE`).

**Harness.** `docs/internal/gc-round-gci-20261011/battery-win.sh` now records each row's full 32-bit
exit code (`<row>.rcfull`) and a resource heartbeat (`<row>.hb`) through `battery-observe.ps1`
(`FULLRC=0` restores the old path); `types/tests/flag_default_matches_reader.rs` checks that every
inventory row's `off_word` agrees with its reader's default.

### Generational and G1: what the gci round changed (2026-10-11 → 2026-10-13)

The gci round ran thirteen waves of parallel lanes (i1-i13) on the Generational backend and the
common infrastructure; G1 joined the scope after wave i5 (user, 2026-10-12), ZGC and proposals
stayed out. Each wave was judged against HotSpot 25 (`-XX:+UseSerialGC`, `-XX:+UseG1GC` for G1
rows) by its battery (`vi<wave>-<lane>.list`). No switch was deleted: one that failed its rule
stays registered and default OFF.

- **Round records.** `docs/internal/gc-round-gci-20261011/`: `orch-notes.md` (what was
  registered, flipped, reverted and retired; authoritative), `lane-rules.md`, the `i*-brief.md`
  briefs and the `i<wave>-<lane>-report.md` lane reports with each switch's pre-registered rule.
- **Open items.** The `gci-*` pages (and the older pages the round worked) in
  [`known-issues/gc/README.md`](known-issues/gc/README.md).
- **G1 switches** the round added are rows of the G1 table under "Verifying and debugging"
  above (`g1-young-target-*`, `g1-fixup-*`, `g1-mixed-*`, `g1-full-*`, `g1-humongous-*`, ...),
  not repeated here.

**User decision of 2026-10-13: every pending decision flipped ON.** Each site is marked
`WARNING gci-user-flip-20261013` (grep it) and goes into its own `WARNING:` commit so an apps
triage can find and revert it. `=0` kills each.

| Switch | What it does |
|---|---|
| `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`, `CRATONVM_OLDGEN_COMPACT` | A requested STW major compacts the old generation, and the old-gen major compacts (gcf-f1c: parity over pause, +16..83 % per major). |
| `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`, `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`, `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` | Survivor ages count HotSpot Serial's young collections (one per HotSpot eden of young allocation, old-direct allocations charged too), and the young trigger follows Serial's eden (gcf-f5c; `OldGenFinalizeProbe` 64/64). |
| `CRATONVM_GEN_PINNED_YOUNG_COPY`, `..._PARALLEL`, `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER` | The pinned young copy under conservative JIT roots, on the parallel evacuator, with its ledger guard (the pinstale6 family; gci i8/pe flipped the two companions with the copy, which every gate had run as a triple). |
| `CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS` | Helper-window marking roots narrow to object starts (gcg-g3h item 2). |
| `CRATONVM_JNI_NATIVE_TRANSITIONS` | The JNI package's native transitions, on every backend (gcd-d2i): a Java thread in a JNI native is no longer a counted mutator. |
| `CRATONVM_GC_DEPOSIT_LOCALS_LIVENESS` | Flipped, then **reverted to default OFF** by the user the same day: vi5-ob D showed it moves away from HotSpot (gch-h1jp item 1). **Flipped default ON again 2026-10-17** by the user's decision (WARNING, marker `WARNING gci-i48-flipB-20261017`, `=0` kills): vi19-vd E 12/12 SAME, `gci-i19vd`. |

**Default ON on a verdict or as a correctness fix** (`=0` kills each).

| Switch | What it does, and the evidence |
|---|---|
| `CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED` | The non-moving sweep's dirty-card seed on the evacuation pool. Flipped i1/yg (vi1-orch T3 pause median 0.90; vi1-yg D SAME). |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS`, `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` | The gch h4 true-root opt-ins (rows in the gch table below). Flipped i1/og; vi1-og B + D. |
| `CRATONVM_GC_TRUE_ROOT_BATCHED_VERDICT` | The non-moving true-root major takes P-C's batched dead-promotion verdict and grid-index reached set (exact, cheaper). gci i3/og; vi3-og / vi4-og K. |
| `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS`, `CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD` | P-C (the moving cycle's major seeds from the true roots), and its watched-dead verdict (gci i3/og). Flipped on vi5-og T-W; vi7-flip F-PC retired gcd-d4j. |
| `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` | The non-moving sweep promotes a young value rooted only by statics, interned strings, mirrors or JNI globals. Flipped on vi6-pn C10; vi7-flip F-PRP. |
| `CRATONVM_DCMD_LIVE_HEAP_WALKS` | `jcmd GC.class_histogram` / `GC.heap_dump` without `-all` (and `jmap -histo:live` / `-dump:live`) run a full collection first, as HotSpot. gci i4/hd; flipped on vi5-hd Q; vi7-flip F-DCMD. |
| `CRATONVM_DCMD_GC_ON_ATTACH_THREAD` | `jcmd GC.run` and the live-only walks run their full collection at once on a thread attached as "Attach Listener", with HotSpot's causes (`Diagnostic Command`, `Heap Inspection Initiated GC`, `Heap Dump Initiated GC`); `=0`: wait up to 10 s for a mutator. Born ON, gci i9/dc. |
| `CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES` | A single-pass call zeroes the popped reference arguments' source slots once its staging read them (gcg-g5jm's holder). gci i1/sp, flipped i2/sp. |
| `CRATONVM_JIT_IR_ALLOC_SITE_CLEARS` | Optimizing-tier allocation nodes arm the dead-home clears and liveness claim like calls (oomjit9 item 1). gci i1/fr, flipped on vi2-fr C. |
| `CRATONVM_JIT_IR_OTHER_GC_POINT_CLEARS`, `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO`, `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO_LAZY` | The control-pinned non-call GC points (getstatic/putstatic, checkcast/instanceof, ldc, monitors, multianewarray) arm the same clears; a reference parameter's prologue word is zeroed once copied into its home (`_LAZY`: at the planned clears of its own colour instead). gci i3-i5/ir; flipped on vi5-ir; vi7-flip F-IR. |
| `CRATONVM_JIT_IR_CARD_POST_BARRIER_ONLY` | The optimizing tier's inline card arms answer a card found clean after the store (a take-over pause cleaned it) with the card barrier alone instead of re-running the store through the helper. Born ON, gci i12/wb. |
| `CRATONVM_JIT_IR_KEEP_SET_SHARED_COLOUR_KILLS` | The optimizing tier's dead-home keep set kills a colour at every `Ref` call / allocation / load that defines it, not only at a sole occupant: a value only chain snapshots name shares its word, so a spliced loop's recomputed value (`cleared(ref)`'s `ref.get()` inside an OSR body) was kept across the loop's `System.gc()` (`keep_set_step_kill`). Born ON, gci i40/os; rows `vi40-os.list`. |
| `CRATONVM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS` | The optimizing tier's dead-home keep set no longer has a φ or a pure op at a spliced pc read the snapshots recorded at that pc: a guarded splice's join φ sits at the hit body's `areturn`, whose chain snapshot names the hit value, so on the MISS edge the previous round's `ref.get()` result stayed live across a loop's `System.gc()` / poll (`keep_set_step_pc_reads`). Born ON, gci i47w/ks47; `=0` restores the per-pc union; rows `vi40-os.list`, `vi41-hc.list`. |
| `CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE` | A blocked peer that never published a JIT depth is a pure-native band, not a refusing helper window. gci i3/tk, flipped on vi3-tk A. |
| `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS` | An interpreted frame keeps a liveness-dead local that is still reference-typed at its pc, as HotSpot's interpreter oop maps do (liveness alone is HotSpot's compiled-code answer). Born ON, gci i10/np. |
| `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1` | The in-native band scanned at the pause on G1 too (gcf-f1j's G1 half; the band's roots are pinned for the cycle). gci i5g/g1r, flipped on vi6-g1r. |
| `CRATONVM_JNI_FOREIGN_QUIET_IDLE` | An idle foreign-attached thread skips the full wake on a quiet leave and re-enters idle incrementally after a VM-only JNI function. gci i5/fj, flipped on vi5-fj (Gen no-churn U 0.745x). |
| `CRATONVM_INVOKESTATIC_FALLBACK_STATIC` | `execute_invokestatic`'s by-name fallback uses the static entries: no receiver read, only the declaring class's `<clinit>` (JVMS 5.5). Born ON, gci i10/cr. |
| `CRATONVM_BYNAME_STATIC_ARG_NOT_RECEIVER` | The by-name walk no longer reads a static call's first argument as a receiver, so another loader's same-named class cannot decide which copy runs. Born ON, gci i11/cf. |
| `CRATONVM_JIMAGE_DIRECT_MAP` | `NativeImageBuffer.getNativeMap` answers a direct buffer over one read-only map of `lib/modules` instead of a 145 MiB heap copy. Born ON, gci i2/jm. |

`CRATONVM_JUL_FIRST_START_PREWARM` (gch-h1ak) was born default ON in gci i2/wr and flipped to
**opt-in** on vi4-ju C: HotSpot does not pre-warm `java.util.logging` on the first
`Thread.start()`. Also default ON from this round: `CRATONVM_SOFTREF_HOTSPOT_LRU` on G1 (i5g/g1o),
`CRATONVM_G1_CONC_FROZEN_EAGER_SCAN` and `CRATONVM_G1_ITL_DEFER_AT_MARK` (G1 table).

**The JNI dispatch memo** (`CRATONVM_JNI_DISPATCH_MEMO`, gci i8/dm; gcd-d5f; opt-in until the
WARNING flip `83b1120a0f` made it default ON on 2026-10-15 after vi19-jt E + S + A2, `=0` kills): a JNI-bound
native this thread already dispatched skips the by-name dispatch prefix (per-thread rows, retired by
`RegisterNatives` / redefinition; a JNI-table write bumps the registration generation only when it
replaced or removed a bound key). Inside it, each default ON with its own `=0`:
`CRATONVM_JNI_CALL_SITE_MEMO` (i9/cs: an interpreted or compiled `invokestatic` site whose slow call
ended in a table-bound native calls the door directly), `CRATONVM_JNI_DISPATCH_MEMO_SELECTED`
(i10/cr: the memo serves receiver-selected interpreter calls), `CRATONVM_JNI_VIRTUAL_SITE_MEMO`
(i11/cf: interpreter `invokevirtual` / `invokeinterface` site rows), `CRATONVM_JNI_JIT_VIRTUAL_SITE_MEMO`
(i12/mf: compiled virtual / interface sites, and compiled `invokespecial` / devirtualized
private-or-final sites) and `CRATONVM_JNI_SITE_PROBE_MISS_STAMP` (i13/jm: a site whose last two slow
calls never reached the JNI arm stops arming the fill probe). Rows `vi8-dm`, `vi12-mf`, `vi13-jm`.

**Opt-in, awaiting rows.** Default OFF; each lane report names the deciding block.

| Switch | Area | Origin, deciding rows |
|---|---|---|
| `CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES` | A refusing helper window whose stack was read whole leaves Generational promotion on. | i3/tk; vi3-tk D3 failed its cost rule (host noise per i5/tk), stays OFF |
| `CRATONVM_GEN_TAKEOVER_PROMOTES` | A pause whose take-over froze compiled peers keeps promotion when every frozen stack was read whole. | i3/tk, widened i5/tk; page `gci-i1jn-...` |
| `CRATONVM_XT_HELPER_WINDOW_ZERO_DEPTH_NATIVE` | A blocked peer whose published JIT depth is 0 is not a helper window (gcg-g3h item 1). | i1/fr; no population on Windows, Linux census row owed |
| `CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE` | G1, DEFAULT ON (`=0` kills): a blocked peer whose deposit deferred a complete compiled chain is not a helper window (captures kept, no pins; its compiled frames are pinned by the deferred walk): the parked-carrier over-pin of gci-i20vh. | i20/vh; default since i24/jp (WARNING gci-i24jp-flip-20261015), decided by vi24-jp P + S + K |
| `CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS` | Generational: a frozen peer's stack words above its innermost compiled segment root object starts only (gcf-f7f kind A). | i1/fr; vi2-fr B sound |
| `CRATONVM_GEN_YOUNG_LOADER_PIN_SNAPSHOT` | The moving young copy (both arms) takes one `loader_pin` snapshot per copy instead of the registry read lock per copied object; same answers, pause cost only. | i10/lp; page `gci-i9yl-...` |
| `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS` | The unrecorded-root guard also refuses a cycle on which a dirty-card or overlay value names an unrecorded from-space address; restores the consumed cards, `[unrecorded-young-card-root]` at WARN. | i10/gr; sizer10 (a) |
| `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES` | Needs `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`: a moving `System.gc()` keeps the side-table licence (loader metadata, mirrors and overlays deferred to their owners; the copy follows their rows, so a dead young loader unloads on that call). | i8/pa, i9/yl; page `gengc-core-system-gc-forces-non-moving` |
| `CRATONVM_GC_SYSTEM_GC_TENURES_ALL` | Generational and G1, and since gci i48/zl3 (`9fda97cd08`, zx48 A9) ZGC's generational model heap (`CRATONVM_ZGC_GENERATIONAL=1`; default ZGC already reports young as allocated-since-cycle): the `System.gc()` door's collection tenures every movable live young object from the first cycle (in-place sweep: the promotion walk forced; moving copy: `force_promote_all`; G1: evacuation threshold 0 for that pause; ZGC generational: the major promotes all young), as HotSpot 25's full collection leaves young empty, so the old pool reads the data and its usage threshold trips. Root-pinned / late-pinned objects, a gated walk, a full old gen and a failed G1 Old copy stay young. **Default ON** since the user-approved 2026-10-17 WARNING flip (`=0` kills): rows `vi47w-sg` M1/T1/T2 PASS, regression sweep with it ON (`vi45-sc`, `vi46-hc`, `vi40-os` r5w) lost no verdict, `hc46_m_*` 5 DIFF -> SAME. | i47w/sg47; page `gci-i47w-j46-system-gc-leaves-live-young-data-out-of-the-old-pool`, rows `vi47w-sg` |
| `CRATONVM_GC_MOVING_MAJOR_LOADER_REKEY` | Every P-C moving major follows `loader_pin` / `mirror_pin` rows re-keyed by post-copy address instead of rooting every moved pinned loader and its mirrors (forced on a licensed moving `System.gc()`). | i8/pa; gch-h3mj item 6 |
| `CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP` | A STW collection that pre-empts the concurrent cycle during its sweep measures the cycle's growth net of what the sweep has freed. | i13/cm item 2 |
| `CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE` | The concurrent initial-mark and remark root scans skip the precise-root-promote counting no young sweep consumes. | i13/cm item 6; vi13-cm N |
| `CRATONVM_GC_YOUNG_PAUSE_TRIAL_JUDGED_IN_BAND` | An in-band young pause judges a pending halving trial of the pause-goal loop instead of dropping it. | i2/yg |
| `CRATONVM_GEN_SERIAL_EDEN_INITIAL` | The Serial-shaped young trigger starts at HotSpot Serial's eden for the initial heap (`-Xms`, or RAM/64 under `CRATONVM_GC_HOTSPOT_INITIAL_HEAP`, default ON on every collector since gci i48) and is resized after full collections by HotSpot's `compute_new_size` rules, never above the maximum eden. | i22/ed gci i35/fz: on the cratonvm launcher, without -Xms and with CRATONVM_GC_HOTSPOT_INITIAL_HEAP unset, the switch applies the ergonomic initial heap (it had followed the 16 MiB VmConfig default: fz31_f_ed_only 0/64); =0 of CRATONVM_GC_HOTSPOT_INITIAL_HEAP keeps the 16 MiB start. |
| `CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED` | A young cycle the allocation-failure door forced is not fed to the pause-goal loop (`adapt_skipped_forced=`). | i3/yg; vi3-yg SF3 gc_ms 1.08, stays OFF |
| `CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS` | A JIT-guarded TLAB refill young cannot serve collects young once per cycle instead of tenuring the object. | i4/yg |
| `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK` | **Default ON since the gcj j2 WARNING flip (2026-10-19, `docs/internal/gc-round-gcj-20261019/triage-j1.md`; `=0` kills); cost: alloc-ring 1.14x, wave j2 lane yg2.** A young TLAB refill takes the smallest whole free block that makes a useful TLAB before the default carve cuts a larger one (best fit by size; `yp_refill_whole_below_request` counts it). | i22/dr |
| `CRATONVM_GEN_SIDE_ROW_EPHEMERON` | Generational: the per-object JUL rows' values and the `SSLContext`-keyed TrustManager / KeyManager rows (no engine / lease / parked socket holding the key) are reported as `metadata_pin` rows of their key instead of roots on a licensed STW scan, so a handler or trust manager that reaches its own key no longer pins its webapp loader (ephemeron semantics of the field each row stands for). **Default ON** since the 2026-10-17 user-approved WARNING flip (marker `WARNING gci-i48-flipB-20261017`, `=0` kills). | i23/ep |
| `CRATONVM_JUL_WEAK_LOGGER_REGISTRY` | `=1` (default off): hold named JUL loggers minted by the `--compatible` natives weakly (root and `global` strong), so a webapp logger nobody keeps -- and its handlers, filter and class loader -- can be collected as on HotSpot. | i25/jl; opt-in pending the `vi25-jl.list` JL-W rows |
| `CRATONVM_JIT_LEAN_CARD_POST` | Single-pass compiled old-receiver reference stores answer the POST card check with one compare when the PRE check found the card already dirty (R10 = &card kept across the store); every other PRE edge keeps the full POST. Bytes unchanged when off. | i23/lc; vi23-lc E/P/S |
| `CRATONVM_GC_HANDSHAKE_FRAME_TRACE` | A cross-thread `getStackTrace()` / `ThreadInfo` of a running peer asks that peer alone, through a per-thread handshake served at its next `safepoint_check` (an allocation, a pause arrival), and takes the `door=frame-trace` pause only when it was not served within 1 ms or a pause began. | i23/hs |
| `CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED` | Reuse a clean unregistered-frame verdict over the stack band above the outermost JIT entry while the JIT entry chain, the stack top and the code-range set are unchanged, on every root deposit (not only JNI / FFM / blocking natives); never in the collector's own root pass or coverage proof. | i23/r6 |
| `CRATONVM_GC_YOUNG_MARK_SHARE_EARLY` | The parallel young mark shares work with a parked peer from 8 local entries, not 64. | i2/st; vi2-st S3 kept it OFF |
| `CRATONVM_GC_OVERHEAD_SEALED_WINDOW` | The GC-overhead verdict reads counters sealed inside the pause, so two threads' verdicts cannot interleave. | i2/oo |
| `CRATONVM_STW_RESUME_FAIRNESS` | A pause request yields (<= 2 ms) until every thread the previous pause released is back and has run 250 us. | i6/jr; vi6-jr: the starvation lead refuted, stays OFF |
| `CRATONVM_SOFTREF_STAMP_GC_CLOCK` | SoftReference LRU stamps on HotSpot's GC clock instead of the access's wall clock (with `CRATONVM_SOFTREF_HOTSPOT_LRU`). **Default ON** since 2026-10-17 (user decision, `=0` kills). | i11/rp R2 |
| `CRATONVM_GC_SPILL_IMAGE_PINS` | A compiled frame's blind GPR spill-image word always pins its object, even with shadow emission on (default: movable only when the shadow reload refreshes the register; pins under `CRATONVM_JIT_MY_SHADOW_EMISSION=0`, the default-path fix). | i8/nt; Netty page, vi8-nt N-L |
| `CRATONVM_GC_VT_PARKED_FOLD_FILTER` | A relocating pause skips the blocked-thread fold of an unmounted virtual thread whose cached address filter proves none of its parked words is in the pause's pointer map. **Default ON** since 2026-10-17 (user decision, `=0` kills). | i12/vp; rows A-D |
| `CRATONVM_GC_VT_PARKED_GATHER_FILTER` | Unmounted virtual threads none of whose words is young are handed over as an old-only root section that a young-only collection skips (Generational outside a concurrent mark, G1 plain young pauses; never ZGC). **Default ON** since 2026-10-17 (user decision, `=0` kills). | i13/vg |
| `CRATONVM_VT_REAP_TERMINATED` | The virtual-thread manager forgets a JDK continuation's record when its slice ends (skipped while JDWP is configured). **Default ON** since 2026-10-17 (user decision, `=0` kills). | i11/vt item 4 |
| `CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS_IN_SPLICES` | With `CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS=1`, the single-pass maps also drop the outer method's dead reference locals at safepoints inside a splice. | i8/sp; oomjit9 5(b), vi8-sp A/B |
| `CRATONVM_JIT_SPLICE_ZERO_DEAD_INLINE_REFS` | A splice return arm zeroes the callee's dead reference locals and operand words. | i3/il; oomjit9 5(c), vi8-sp C |
| `CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS` | Loop back-edge safepoint polls arm the dead-home facts. | i5/ir; vi5-ir B2 VOID |
| `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE` | With a weak-args door (`CRATONVM_JIT_INTERP_DOOR_ARGS_WEAK=1`: `execute_jit_call`, `execute_jit_call_decoded`; `CRATONVM_JIT_FIRST_CALL_DOOR_ARGS_WEAK=1`: `interpreter::execute`'s first-call door), the door publishes the reference argument words it handed over for the call, and the outermost compiled frame's callee-saved image (RBX, RSI, RDI, R12-R15) drops a word only when it equals one of them (`door_promise_claim`, oracle `door-handed`); every other word stays `Keep`, and a dropped word is still remapped. Sound iff the weak door is (no register is zeroed since i14/od; i13/oj's zeroing parked the callers' registers unrooted and was removed). | i13/oj, i14/od; oomjit10 item 1, vi13-oj, vi14-od |
| `CRATONVM_JIT_UNLOAD_KEEPS_LIVE_NAMESAKES` | A class unload no longer withdraws compiled code of a live same-named class (a redeploy's new loader) that inlined its own copy. | i12/ul F3; vi12-ul R |
| `CRATONVM_ARRAY_CLASS_LOADER_ROWS` | An array class of a user-loader class gets its element's defining-loader row, so the array's mirror no longer pins the loader for the VM's life and the array class unloads with it. **Default ON** since 2026-10-17 (user decision, `=0` kills). | i12/ul F2; vi12-ul A + H |
| `CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM` | A class that reports no loader resolves its `MethodType` descriptor types in its own namespace, not through the bootstrap `forName0`. | i3/jd |
| `CRATONVM_JNI_CRITICAL_HOTSPOT_RELEASE` | `ReleasePrimitiveArrayCritical(JNI_ABORT)` copies the native's writes back and `GetPrimitiveArrayCritical` reports `isCopy` 0, as HotSpot's pinned critical. **Default ON** since 2026-10-17 (user decision, `=0` kills). | i13/jc F7; vi13-jc H |
| `CRATONVM_JNI_LOCAL_CAPACITY_HOTSPOT` | `EnsureLocalCapacity` / `PushLocalFrame` refuse a capacity `< 0` or `> 65536` with `JNI_ERR` and no pending exception (HotSpot's `MaxJNILocalCapacity`). **Default ON** since 2026-10-17 (user decision, `=0` kills). | i13/jc F8 |
| `CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD` | A C agent's `ClassLoad` posted where the defining thread's own handle is not installed is delivered at that thread's next own-handle point, so the callback never borrows a thread whose `&mut` is live up the stack (timing change for agents only). | i12/ev; vi12-ev D |
| `CRATONVM_EXIT_HOOK_GC_SAFE` | `System.exit` / `Runtime.exit` / `Runtime.halt` run the pre-exit hook inside one blocking region on the exiting thread's own context, so a collection another thread requests no longer waits for it. | i11/ja; vi11-ja X |

**Diagnostics added** (default OFF): `CRATONVM_DBG_OOM_DUMP_PAUSE` (one line per OOME heap dump
whose pause was refused: refusals waited out, and whether the GC-safe retry got the pause; i9/hj),
`CRATONVM_DBG_VT_PARKED_ROOTS` (`[vt-parked-roots] gather` / `fold` lines; i12/vp),
`CRATONVM_DBG_BASE_BODY_ENTRY` (frame entry into a superclass body whose receiver chain overrides
it, `[BASE-BODY]` lines; the value is a class-name prefix, `1` = javac's `Locations$`, `all` = every
class) and `CRATONVM_DBG_IHASH_WITNESS` (identity-hash divergence witness; both
i11/jd, the javac door), `CRATONVM_DBG_JAVAC_INFER_PIN` (`[JAVAC-INFER] ... moved`; i5/jd),
`CRATONVM_DBG_FOREIGN_PATH_YIELD` (a foreign-`Path` call the carrier natives used to answer wrongly;
i6/jd), `CRATONVM_DBG_ROOT_REMAP_AUDIT_BREAK_FRAME_REMAP` (with `CRATONVM_DBG_ROOT_REMAP_AUDIT`: the
audit's positive control, the oop-map frame rewrite gets an empty map; i6/mv). Census lines:
`[GC] g1 pause-phases:` and `[GC] g1 pin-attribution:` (i9/fa; the latter under
`CRATONVM_G1_DBG_PINS`), `[xt-deopt-stash]` and `jni_locals=` under `CRATONVM_DBG_XT_JIT_ROOT_SCAN`
(i9/ds, i9/jw).

**Default-path changes without a switch** (waves i8-i13; each page in the index says how it is
verified):
- **Frozen-peer roots.** A take-over's frozen-peer walk also roots the peer's JNI local table
  (i9/jw; the JNA "not sorted" wrong result) and its JIT deopt stashes (i9/ds), read only while the
  peer is frozen. A virtual thread retires its TLAB before the deposit at every yield (i11/vt).
- **G1 logging and events, HotSpot G1's shapes** (i11/go, i12/ge, i13/gp, i13/oa): allocation pauses
  report `G1 Evacuation Pause` (JMX `getGcCause()`, notifications, JFR, `-Xlog:gc`) and print `Pause
  Young (Normal)` / `(Mixed)` / `(Concurrent Start)`; a humongous request collects under `G1 Humongous
  Allocation` (interpreter, JIT and native array doors); the last-ditch compaction is `G1 Compaction
  Pause`; a full collection's compaction pause carries its requester's cause and is not judged for
  the overhead limit (i10/dl); a cycle prints `Concurrent Mark Cycle`, `Pause Remark`, `Pause Cleanup`
  under its own `GC(n)`; `-Xlog:gc+heap` prints the region lines, and `gc,start` / `gc,metaspace` /
  `gc,cpu` print on G1; an evacuation failure gets HotSpot's suffix; JFR `jdk.G1GarbageCollection`
  and the `ExecuteVMOperation` names are written.
- **G1 heap.** A serial Old-destination claim stays inside the committed prefix, so
  `Runtime.totalMemory()` no longer jumps to `maxMemory()` (i13/ef); one old-CSet selector
  (`choose_old_cset`) for every mixed pause (i11/gm); explicit `G1HeapRegionSize` follows HotSpot's
  bounds (tuning knobs above).
- **Serviceability.** `jcmd Thread.dump_to_file` writes its file (`ThreadSnapshot.create` is
  registered; the JDK's `ThreadDumper` runs on an "Attach Listener" thread); `jcmd` splits arguments
  as HotSpot's `DCmdArgIter`; `VM.version` prints HotSpot's two lines; a thread dump prints
  `Carrying virtual thread #<id>`; `-version` with an `-Xlog` selection creates the VM first, so
  `gc+init` prints (i12/td). JVMTI heap walks, `GetTag` / `SetTag` / `GetObjectsWithTags` and the OOME
  heap dump wait out a refused pause GC-safe instead of walking unpaused (i8/hd, i9/hj, i10/al);
  JVMTI callbacks post on the thread's own context (i11/ja, i12/ev).
- **JNI** (i13/jc): `NewDirectByteBuffer` is big-endian as on HotSpot (it was little-endian), takes a
  NULL address and refuses a bad capacity with the JDK's `IllegalArgumentException`;
  `GetStringChars` is NUL-terminated and a lone surrogate keeps its exact UTF-16 unit;
  `IsSameObject` keeps no weak referent alive; `PopLocalFrame` of a global returns a local; the
  critical copies live in a per-VM table; the Generational remark clears weak globals after its mark
  is final.
- **Class unloading** (i12/ul): `VMManagementImpl`'s total loaded class count is live + unloaded; an
  array mirror's `getClassLoader()` follows its element; arrays are out of the class+unload log, JFR
  and counts. `-Xlog:class+load` prints the VM's lambda proxies and nestmate hidden classes (i11/cl).
- **Generational concurrent cycle** (i13/cm): a cycle unwound after its sweep no longer kills the
  next one, and a stopped sweep is not counted as a collection or a cycle. The `Final` census no
  longer counts collector-resurrected finalizables (i11/rp R1).
- **Barriers** (i12/wb): the SATB gate's refresh race is closed (one atomic state word); the
  interpreter's `putfield` / `aastore` skip the old-value read while SATB is unarmed; the G1 double
  log of one store is gone.

### Generational and G1: what gci waves i15-i18 changed (2026-10-14)

Waves i15-i18 were the last four of the gci round's first session (the round resumed on 2026-10-15
with waves i20-i36: next section); i15 and i16 mixed defects with cost, and the user
limited i17 and i18 (2026-10-14) to defects only, with the round's proposals (P1-P5) out of scope. Every lane worked
by reading (no cargo, no runs); the orchestrator built and judged. The last snapshot is
`gci-snap-i18b` (`261a812d01`): gc 3117/0, vm 5530/0, dbg 236/0 unit tests, release build rc=0.
**Most of what follows is FIXED IN CODE and awaiting its probe rows**: of the per-wave batteries
only the vi15 lists (on i15a) and vi17-nr R1 (on i17a) were judged before the round closed; the
`vi16-*`, `vi17-*` and `vi18-*` lists name each change's deciding block. Judged results are quoted
where they exist. Lane reports: `docs/internal/gc-round-gci-20261011/i15-*` .. `i18-*-report.md`.

**Default-path changes without a switch**, by area:

- **G1 cleanup and stale slots** (i16/nc, i17/nr, i18/nr). i16/nc found the np-arm mechanism: a
  young pause copies dead young objects through dead Old holders above TAMS (never scrubbed),
  cleanup frees their targets, the mutator reuses the memory, and the next pause follows the stale
  slot into a live object (`corrupt Value cell` bursts after `door=g1-remark`, a wrong javac
  result). Cleanup now nulls every slot naming a region it freed (switch below, default ON). i17/nr:
  the scrub also nulls dead holders' slots the dead-holder filter skipped, and a candidate whose
  remembered set names a source region the scrub could not walk to its end is kept one cycle
  (`retained_regions=`). i18/nr: a source region with no dirty card is not walked (whole-region card
  screen, i17/rv finding 3); `mark_proven_dead` needs `CRATONVM_G1_MARK_ROOT_SCREEN` and
  `CRATONVM_G1_REMARK_SEEDS_JIT_PINS` both on; the scrub counters and switch latches are per heap
  (the `FREED_TARGET_SCRUB_*` statics are gone); diagnostics `marked_holders=` on the scrub line
  and a `[GC] g1 cleanup fail-safe:` line. i15/ra F7: the scrub no longer skips JNI-pinned regions
  (matters only under `CRATONVM_JNI_CRITICAL_G1_PIN`). vi17-nr G (stale-slot probe, mixed names)
  SAME x3 on i17a; R1 FAILS 2/10 (see "Current correctness state").
- **G1 marking and remark** (i16/mr, i17/rv, i18/nr). Every remark closes every `metadata_pin` row
  over the completed mark first, before the reference, class-unload, keyed and weak verdicts
  (`g1_remark_close_metadata_rows`; loader and inherited-`ThreadLocal` rows filed while the marker
  ran were read dead). A frozen peer's reserved TLAB tail at the initial mark lowers that region's
  TAMS to the tail's start (`tams_below_reserved_tails`), so objects the frozen thread carves after
  the snapshot are not read dead at a remark with no young pause; i17/rv closed the hole that left
  pre-snapshot objects above the lowered TAMS untraced (`pre_snapshot_starts_above_lowered_tams`).
  The metadata-row close reads a per-VM `metadata_pin::vm_snapshot` (i18/nr, rv f9).
- **G1 mixed phase and OOME at a small heap** (i16/oo, i17/mx, i18/mx; page
  `gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-...`). The W7-L futile-copy breaker read a pause
  over HotSpot-legitimate candidates (67-85 % live) as futile and, after four, refused every later
  mixed phase while Old crept to the heap's end: a mixed pause now also counts productive when
  `freed * live_threshold > copied * 100` (`mixed_choice::mixed_pause_productive`), and an
  evacuation-failure, full or plan compaction pause no longer feeds the futile streak
  (`skipped_samples`; census `[GC] g1 mixed-futile-i18:`). The GC-notification drain after a
  collection (which built `GcInfo`'s `MemoryUsage[8]` on the infallible arm and aborted the process
  with `G1: out of heap space for array allocation (80 bytes)`) runs under
  `deliver_catching_native_alloc_oom`, now a wrapper of `native_oom::catch_alloc_oom_on`.
- **G1 reference processing logs** (i15/rf G-7): the G1 reference block of `-Xlog:gc+phases+ref`
  is indented under `Post Evacuate Collection Set`, HotSpot's shape (vi15-rf X: PASS).
- **G1 reference processing switches (gci i27/gr):**
  - `CRATONVM_G1_PINNED_REFERENT_VERDICT` (default ON, `=0` restores the region-granular verdict): after a G1 pause that
    kept a young region in place (JIT / JNI pin, evacuation failure), a weak / phantom referent or `Reference` there
    that nothing holds is cleared / pruned as HotSpot does (gci i27/gr).
  - `CRATONVM_G1_PINNED_REFERENT_FINALIZER_ONLY` (default ON, `=0` keeps the i27 answer): in a young region a G1 pause
    kept in place, a weak referent only a finalizable the pause resurrected reaches has its weak references cleared, as
    HotSpot clears them before `finalize()` (gci i30/rp).
  - `CRATONVM_G1_PINNED_REFERENT_TABLE_OWNERS` (default ON, `=0` stops offering them): the owners of the VM's Locale,
    TLS and lock-key side-table rows are judged by the same kept-region verdict, so a row whose owner died there goes at
    that pause (gci i30/rp).
  - `CRATONVM_G1_PINNED_REFERENT_FINALIZABLES` (default ON; `=0` leaves the finalizable registered until its region is
    evacuated): a dead registered finalizable in a young region that a G1 pause kept in place (JIT or JNI pin) is
    queued for `finalize()` at that pause, and weak references to it are cleared, as HotSpot's in-place evacuation does
    (gci i31/gq).
  - `CRATONVM_G1_PINNED_REFERENT_EMPTY_CSET` (default ON, `=0` keeps such a pause unarmed): a G1 young pause whose
    collection set is empty because every young region is pinned (JNI critical / JIT pin) still judges the referents
    and finalizables of those regions per object, as HotSpot's in-place evacuation does at that pause (gci i32/ec).
  - `CRATONVM_G1_PINNED_REFERENT_FINALIZABLE_CLOSURE` (default ON, `=0` keeps them): weak references to objects only a
    dead finalizable of a kept young region reaches (children the pause copied out of its collection set included) are
    cleared at that pause, before `finalize()`, as HotSpot clears them (gci i32/ec).
  - `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES` (default ON since the 2026-10-17 user-approved WARNING flip, marker `WARNING gci-i48-flipB-20261017`, `=0` kills): G1 `System.gc()` and the other full-collection
    doors hold the finalizables their young pause just queued out of the stop-the-world mark's roots, and treat what
    only they reach as finalizer-reachable. A weak reference to an Old child that only a dead finalizable reaches is
    then cleared, as in HotSpot's single full collection (gci i31/gq, G-2; flip rule in `vi31-gq.list` block G).
  - `CRATONVM_G1_LAST_DITCH_CYCLE_STW` (default ON, `=0` kills): G1's allocation-failure (last-ditch / overhead-limit)
    full cycle is the stop-the-world one even under `-XX:+ExplicitGCInvokesConcurrent`, as HotSpot's `VM_G1CollectFull`
    (gci i30/ob).
  - `CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS`: default ON since gci i30/ob (WARNING flip; `=0` restores the initial mark's
    `G1 Concurrent GC` booking; HotSpot books it on `G1 Young Generation`).
  - `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS` (default OFF; needs `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`): the
    remark of a G1 full collection leaves alone the `Reference`s its stop-the-world pause found undiscovered (gci i27/gr).
  - `CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS` (default OFF, gci rf48): a G1 full collection's young and compaction
    pauses keep a `Reference` only a dead finalizable reaches undiscovered, the cycle's remark carries it and its
    compaction pause holds it. Opt-in: with it, weak referents are not cleared by the next collection where HotSpot
    clears them (`GciI16FrStwUndiscoveredProbe nested`, vi19-fz F, i48p).
  - `CRATONVM_DBG_G1_PINNED_REFERENTS=1`: one `[g1-pinned-referents]` line per armed G1 pause (candidates, unheld,
    regions / bytes walked, or the decline reason).
- **G1 SATB and JMX** (i18/sw): `compare_and_swap_field_shared`'s lock path no longer logs the
  overwritten value twice on G1 (i12wb item 4); `JmxPoolSampler` splits G1's committed bytes as
  HotSpot's `recalculate_sizes` (`g1_pool_committed_split`, i14mp item 5).
- **Virtual threads** (i17/vt, i18/vp; page `gci-i15orch-virtual-threads-never-start-under-nojit-...`).
  Under `--nojit`, 300-virtual-thread probes hung: the no-JIT `ReentrantLock.lock()` intrinsic ran a
  contended lock's JDK body as a NESTED call, so its park counted as under an opaque native and pinned
  the carrier, and the live `Unsafe.park(ZJ)V` never compensated; workers blocked on main's lock took
  all 32 carriers. `rl_fast_jdk_body` now declares its frame transparent
  (`continuation_pin::transparent`), and 28 tail-call natives whose nested JDK or application body can
  park (`ThreadLocal.get()`'s `--jdk-only` intrinsic, `ClassLoader.getResource*` / `loadClass(String)`,
  `MethodType.makeImpl`'s memo miss, the executor bridges, `URL.openStream`, `JarURLConnection`,
  Derby `connect`, four Spring rows) declare theirs through `continuation_pin::tail_call`. A park
  beneath a native that is not a tail call (reflective `Method.invoke`, Spring's `createWebServer`)
  still pins; `CRATONVM_VT_PINNED_PARK_COMPENSATE` is the opt-in remedy.
- **JNI natives and OOME** (i17/na, i18/na). Fifteen `NativeContextImpl` sites built outside
  `safe_native_call_impl` that reach an allocator now run under `native_oom::catch_alloc_oom_on`: a
  heap refusal there is an `OutOfMemoryError` (or a logged / dropped event, or `JVMTI_ERROR_OUT_OF_MEMORY`
  from JVMTI `on_calling_thread`) instead of a process abort. Covered: the launcher's host-signal
  dispatch, the monitorenter JFR event, the lambda `Number` unbox, the exact-`VarHandle` refusal, the
  agent system-search append, the class-path package define at link, both JFR class-init contexts, the
  AIO completion drain, `run_attached_service` (JDWP heap service, attach listener),
  `release_foreign_thread_locals`, the early ClassFileLoadHook replay and two launcher doors. About 35
  sites are refuted by reading or left open with a line each on the 96m page (`fill_throwable_directly`,
  `RunAgentThread`, `ffm_upcall_attach`, boot contexts, the `debug/*` parked-thread runs).
- **JNI correctness and cost** (i15/jk, i15/rb, i16/dp, i17/dk, i18/dk). `IsAssignableFrom` answers
  identity for primitive mirrors (`int.class` / `long.class` was TRUE); an over-`u32` class id falls
  back to `JNI_NO_CLASS`; `GetStringUTFLengthAsLong` runs as a leaf. JVMTI `ForceGarbageCollection`
  no longer runs its collection and finalizer drain while flagged in native (`on_calling_thread`,
  `force_gc_for_vm`, xlog cause); `GetVirtualThread` / `GetCarrierThread` hold a JNI entry; the
  in-native leave has a drop guard. Per-call Rust allocations removed (identical contents): up-call
  argument lists, `va_list` up-calls read in place, repeat deposits reuse their saved activations
  (`dep_repeat_walk_skipped` under `CRATONVM_DBG_JNI_PHASE`), stack pin lists, an unchanged
  `publish_pinned_jit_roots` skips its lock, and `frame_class_owners` always takes the per-class arm
  (no `HashMap` of every hidden mirror per object-returning native). Rows: vi15-jk U, H, P2 PASS
  (no per-call Java-heap allocation; gcg-g1j narrowed); the rest vi16-dp, vi17-dk, vi18-dk.
- **Non-heap pools and thresholds** (i15/nh, i16/nt, i18/dk). The five HotSpot non-heap pools
  (`Metaspace` measured, the non-profiled code heap = JIT code, the others zero / undefined) with
  `CodeCacheManager` and `Metaspace Manager` (vi15-nh L/N/G/K/I/M SAME: FIXED). Non-heap usage
  thresholds post `MEMORY_THRESHOLD_EXCEEDED`, checked when set, at `define_class_full`, at each GC
  notification and (i18/dk) at a JIT install; `Metaspace` is sampled with a metaspace-only walk
  under a `try_read` guard.
- **Serviceability** (i18/dc). `jcmd Thread.print`, `jstack` and SIGQUIT from an outsider thread take
  one frame-trace pause when a listed thread is running, so running threads print their frames (before,
  only blocked threads did; page `gci-i17jh-thread-print-shows-no-frames-...`); rides
  `CRATONVM_THREAD_DUMP_ONE_PAUSE` (default ON), whose `=0` also turns this pause off and prints running
  threads without frames. hprof roots carry their owner thread's serial.

**Default ON, `=0` kills each.**

| Switch | What it does | Origin, rows |
|---|---|---|
| `CRATONVM_G1_CLEANUP_SCRUB_FREED_TARGETS` | After a concurrent cycle's cleanup frees Old regions in place and reclaims dead humongous spans, every reference slot naming one of them is nulled in every region their remembered sets named; without it a dead object at or above TAMS keeps a slot into reused memory and the next pause follows it into a live object. Since i17/nr a candidate whose remembered set names a source region the scrub could not walk to its end is kept one cycle (`retained_regions=`); since i18/nr the counters are per heap and a source region with no dirty card is not walked. `=0` restores the pre-nc cleanup exactly. | i16/nc; vi16-nc N, S; vi17-nr G PASS on i17a |
| `CRATONVM_G1_VT_PRECISE_DEPOSIT` | On G1 a virtual thread's continuation deposits its roots precisely at its yield and wake, without the conservative-locals probes that run while any thread is in compiled code (a continuation never runs compiled code; the probes are sound only on a non-moving sweep, and G1 always moves). Candidate fix for the parked holder graph. Generational and ZGC: unchanged. | i18/hw; vi18-hw P, A, K |
| `CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS` | A pause request first lets a thread that waited out the previous pause on its way out of native (a JNIEnv / JVMTI call from a JNI native with the transitions package) leave and run 1 ms (<= 2 ms hold). Without it such a native advanced about one JNIEnv call per collection under a `System.gc()` loop. Census: `holds=` of `[GC] stw_resume:`. | i15/hx; vi15-hx R, T PASS, page retired |
| `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY` | The in-native leave rule holds only a pause request that raced the leaver (leaver not out yet, or out < 250 us), still through its whole 1 ms grant; a request later in the grant is not held (`in_native_skips=`), and any hold ends when another thread's pause is requested (`hold_yielded=`). `=0`: i15/hx's trigger (the whole grant) and hold loop. Targets the JNI repeat probe's `full` slowdown since i15 (i17/jh: slow runs, not a hang). | i18/fh; vi18-fh F2, F5 |
| `CRATONVM_STW_BACK_TO_BACK_FAIRNESS` | A pause request made by the thread that ran the previous `complete_gc`, less than 5 ms after it (a `System.gc()` loop), first lets the threads that pause released back in and run 250 us (i6/jr's general rule and hold, <= 2 ms, ending when another thread's pause is requested). Without it a thread leaving a blocked region lost the race to every next request: `System.exit` beside a `System.gc()` daemon printed, then took 25 s to more than 120 s to end. Census: `back_to_back_holds=` of `[GC] stw_resume:`. | i47w/ex47; vi11-ja X |

**Opt-in (default OFF), awaiting rows or a flip.**

| Switch | What it does | Origin, rows |
|---|---|---|
| `CRATONVM_G1_YOUNG_TARGET_BASE_AWARE` | The default young-target loop learns a predicted base time (the pause net of its copy) from every young-only pause; when that base is at or over `MaxGCPauseMillis` it takes HotSpot's minimum young length, `max(5 % of the heap, young regions the pause left + 1)`, at once and shrinks no further; under the goal it also regrows when the young-dependent part of the pause is under half of what the goal leaves it. Ignored while `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL` is on. Does not end the javac young-pause storm. | i15/bg; vi15-bg J1, J2, G, S PASS (FIXED IN CODE, opt-in) |
| `CRATONVM_G1_YOUNG_CARD_SCREEN` | The young pause's remembered-set scan and fix-up go card-granular: a third card value OLD_ONLY (dirty to every existing reader, clean to the young screen) is written by the fix-up for cards whose objects reference no young region; Phase 2 (all-young CSet) and the fix-up of regions the pause did not write then visit only young-dirty cards. Armed only with `CRATONVM_G1_CARD_RSET` on and `CRATONVM_G1_CARD_CLEAN` off; mixed pauses keep the sticky screen; the parallel fix-up arm is untested. | i15/rs; vi15-rs Y, J, S not judged (run stopped by the user) |
| `CRATONVM_G1_YOUNG_GEN_SIZER` | G1 honours `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` / `-XX:NewRatio` as HotSpot 25's `G1YoungGenSizer` does (min / max young length; `-Xmn`, `NewSize == MaxNewSize` and `NewRatio` alone fix it; an operator's maximum binds the young-target arm, which collects at it), prints HotSpot's two `[warning][gc,ergo]` lines, and applies `-XX:SurvivorRatio` to the maximum survivor regions. No effect without those flags. **Default ON since gci i43/gy** (`=0`: the flags are ignored under G1, as before). | i43/gy; vi43-gy U, A, B, C, D |
| `CRATONVM_G1_YOUNG_BOUNDS_COMMITTED` | G1's young bounds (every sizer kind but `-Xmn` / `NewSize` + `MaxNewSize`) are taken from the committed regions -- the soft capacity, `InitialHeapSize` at start -- and re-derived, with the young target, on every heap size change, as HotSpot 25's `G1YoungGenSizer::heap_size_changed` does. The soft capacity differs from the `-Xmx` grid only under `CRATONVM_G1_HEAP_RESIZE=1`, so without it nothing changes. **Default ON since gci i45/gy** (`=0`: the `-Xmx` grid, as before). | i45/gy; vi45-gy U, A, B, C |
| `CRATONVM_G1_OVERLAY_REMEMBER_HUMONGOUS` | G1's young-pause overlay remembered set also keeps every collection-overlay row whose owner or a value lies in a humongous region, so every young pause re-reads it and the eager humongous reclaim sees the value as live (before, an old collection's humongous value held only by its overlay row was freed by the next young pause outside a mark cycle). **Default ON since gci i44/cd** (`=0`: only rows holding a young owner or value are remembered, as before). | i44/cd; vi44-cd U, A, C |
| `CRATONVM_G1_REMARK_UNDISCOVERED_REFS_STRONG` | G1 remark keeps a `Reference` reachable only from a dead finalizable object (its row survives, the round leaves it alone, its referent is kept), as HotSpot's keep-alive phase with discovery disabled; closes a referent freed behind a live `Reference` (G-9). | i15/rf; vi15-rf U PASS; **default ON since the WARNING flip `83b1120a0f` (2026-10-15, after vi19-fz cleared it of the NPE), `=0` kills** |
| `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` | **Default ON on Generational since the gcj j2 WARNING flip (2026-10-19, `docs/internal/gc-round-gcj-20261019/triage-j1.md`; `=0` kills); G1 / ZGC unchanged.** G-9's stop-the-world half, every backend: the referents of the nulled `Reference`s a walk from the registered finalizable objects reaches become extra resurrection candidates, and the round leaves alone the ones the collection marked only through finalizer resurrection. i17/fz: the Generational concurrent exchange takes the candidates back in the same pause, and a phantom reference to an object reachable only through a candidate-only referent is processed in the same round. | i16/fr, i17/fz; vi16-fr S, N, R; vi17-fz D, R |
| `CRATONVM_G1_EVAC_CANDIDATE_REQUIRES_HEADER_FLAG` | The reference-slot evacuation screen refuses a candidate whose header lacks `GC_FLAG_HEADER` (counted on the default path as `flagless_candidates`). Unproven. | i16/nc; no deciding row |
| `CRATONVM_G1_EVAC_REFUSE_COVERED_FLAGLESS` | **Default ON (gci i30/hz), `=0` kills.** The reference-slot evacuation screen refuses a candidate without `GC_FLAG_HEADER` that the extent of the published header before it covers (an interior address, gci-i29cv2), instead of installing a forwarding state inside a live object. |
| `CRATONVM_G1_OOME_STAGED_FULL_COMPACTION` | After the G1 last-ditch recovery pause, up to 16 more armed full-compaction pauses on the same mark while each grows the Free pool (HotSpot's full GC compacts in place; the copying one reclaims only what the Free pool holds). | i16/oo; vi16-oo S; **default ON since the WARNING flip `a4af35def8` (2026-10-15 proposal triage), `=0` kills** |
| `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE` | The first futile-breaker poll of each mixed phase (fresh mark data) restarts the futile streak; without it a trip latches across marking cycles (one mixed phase in ~32 cycles survives). | i18/mx; vi18-mx S, M2; **default ON since the WARNING flip `a4af35def8` (2026-10-15 proposal triage), `=0` kills** |
| `CRATONVM_G1_YOUNG_SIZES_TO_RESERVE` | `needs_gc`'s Free-fraction trigger (and the native-pressure latch) fires at HotSpot's `G1ReservePercent` floor (10 % of the regions) instead of the adaptive 25-50 %; an evacuation failure raises the survival prediction to 100 % instead of the trigger by 8 points, so the headroom arm sizes young. | i18/mx; vi18-mx S, V2, J2 |
| `CRATONVM_VT_PINNED_PARK_COMPENSATE` | A pinned park of a mounted JDK virtual thread (`parkOnCarrierThread`) asks the scheduler for a compensating carrier (`Blocker.begin` / `end`) on the live `Unsafe.park(ZJ)V`; the 2026-10-10 compensation sat on the unregistered `park(Object,J)` overload. THREADS group. | i17/vt; vi17-vt D |
| `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION` | Debug builds (`experimental-debug`): a thread a debugger or a C JVMTI agent holds suspended while its JNI native runs in C stops at its next JNIEnv / C JVMTI call, still in native (GC-blocked, never waited for by a pause), until resumed: HotSpot's native -> VM suspension check. i17/fz: an `AttachCurrentThread` thread calling JNIEnv is held too. COMPAT group. | i16/sn, i17/fz; vi16-sn S, vi17-fz F |

**Existing switches whose arm changed.**
- `CRATONVM_GC_VT_PARKED_MIRROR_KEYED` (default ON since 2026-10-17, `=0` kills; i14/ve): a parked JDK virtual thread whose mirror and
  frames are all old is no GC root; its frames are kept only while something reaches its
  `VirtualThread` (HotSpot's stack chunks), and one nothing reaches is collected with them.
  Generational: licensed pauses (`System.gc()` / JIT-active, no concurrent cycle, no debugger). Since
  i15/vh also G1: the initial-mark and remark scans only, retired after a trusted cleanup. ZGC and the
  bound path: inert. vi15-vh E1, E3, D PASS; S undecided. i18/hw found it keys no thread under the JIT.
- `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE` (opt-in): the abort itself, not only its report (G1 table
  above; i15/ca).
- `CRATONVM_G1_SURVIVOR_PLAB` (i14; opt-in until the WARNING flip `83b1120a0f` made it default ON
  on 2026-10-15 with the HotSpot young model, `=0` kills): one installer per fresh survivor PLAB region and a
  larger-than-region check before the claim (i15/ra); since i16/pl the chunks are not zeroed (the copy
  writes every byte; HotSpot does not zero PLABs either), and an object of at least 10 % of a PLAB that
  misses its chunk is copied straight into the shared region (`direct_objects=` / `direct_bytes=`).
- `CRATONVM_G1_PLAB_COPY_END_ELIDE` (default ON, i25; `=0` kills): a parallel copy into a shared Survivor PLAB chunk skips the per-copy `parallel_copy_end` publish (the region cursor its chunk claim advanced already covers it); the exit line prints `switch_waits=` and `copy_end_elide=`.
- `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL` (i14; opt-in until the WARNING flip `83b1120a0f` made it
  default ON on 2026-10-15 after vi19-ys M5 + S, `=0` kills): since i16/pl the per-region cost is
  Eden-sourced survivors per Eden region, and Survivor re-copies are priced in a separate term added
  to the base (HotSpot's `predict_survivor_regions_evac_time`; `surv_survival_permille=`,
  `surv_term_us=`, `split_samples=`).

### Generational and G1: what gci waves i19-i36 changed (2026-10-14 → 2026-10-16)

Wave i19 closed the first session. The round then resumed on the Linux host on 2026-10-15. It
merged `origin/dev` (`61bab708c4`), triaged every default-off GC switch
(`proposal-flip-triage-20261015.md`) and ran waves i20-i36. From i30 on, the waves were defects
only. The close-out lists every wave, fix, flip and open item:
[`round-close-20261016.md`](internal/gc-round-gci-20261011/round-close-20261016.md). The switch
rows are in the tables of this file (the G1 table under "Verifying and debugging", the Generational
tables) and in `docs/config/flag-inventory.md`. This section lists only the default changes.

**WARNING flips (user rule; `=0` kills each).**

- `83b1120a0f`:
  - the JNI dispatch memo family;
  - `CRATONVM_G1_REMARK_UNDISCOVERED_REFS_STRONG`;
  - `CRATONVM_G1_SURVIVOR_PLAB` and `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`.
- `a4af35def8`:
  - `CRATONVM_G1_OOME_STAGED_FULL_COMPACTION` and `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE`;
  - `CRATONVM_GC_HOTSPOT_INITIAL_HEAP`, on ZGC only. On Generational and G1 it stays opt-in
    (`=1`). Superseded 2026-10-17: default ON on every collector (WARNING commit `dc72abfc64`).
- `f7fbf55f90`: `CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, on G1.
- `736c5f9e96`: `CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS`.
- `02c864c01e`: `CRATONVM_GEN_PINNED_WATCHED_UNNAMED_NOT_ROOTS`.
- `a08d9a9bc3`: `CRATONVM_GEN_PINNED_FINALIZABLE_UNNAMED_NOT_ROOTS`.

**Born default ON, or flipped on a verdict (`=0` kills each).** These are correctness or
HotSpot-parity fixes:

- G1 young sizing:
  - `CRATONVM_G1_YOUNG_TARGET_MIN_EDEN`;
  - `CRATONVM_G1_PLAB_COPY_END_ELIDE`;
  - `CRATONVM_G1_SURVIVOR_REGION_CAP`.
- G1 roots and evacuation:
  - `CRATONVM_GC_SPILL_IMAGE_PUSH_PROOF`;
  - `CRATONVM_G1_MARK_PENDING_FINALIZERS`;
  - `CRATONVM_G1_EVAC_REFUSE_COVERED_FLAGLESS`.
- G1 pinned referents:
  - `CRATONVM_G1_PINNED_REFERENT_VERDICT`;
  - `..._FINALIZER_ONLY` and `..._TABLE_OWNERS`;
  - `..._FINALIZABLES`, `..._EMPTY_CSET` and `..._FINALIZABLE_CLOSURE`.
- G1 full and concurrent cycles:
  - `CRATONVM_G1_LAST_DITCH_CYCLE_STW`;
  - `CRATONVM_G1_CONCURRENT_START_FROM_DRIVE`;
  - `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE` (gci cs48);
  - `CRATONVM_G1_RESERVE_RECLAIM_CREDIT` (gci nd48).
- Logging: `CRATONVM_XLOG_REFUSE_INVALID_SELECTION`.
- Native OOME:
  - `CRATONVM_GC_NATIVE_MIRROR_RECLAIM`;
  - `CRATONVM_GC_NATIVE_OOM_REPLAY`.
- String literals:
  - `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD`;
  - `CRATONVM_GC_CONDY_STRING_ARG_RECORD`;
  - `CRATONVM_GC_SURROGATE_CONSTANT_VALUE_INTERNED`.
- Metaspace:
  - `CRATONVM_METASPACE_HOTSPOT_FIGURE`;
  - `CRATONVM_METASPACE_HOTSPOT_SPACES`;
  - `CRATONVM_METASPACE_LIMIT_HOTSPOT_CHECK`.
- `java.util.logging`: `CRATONVM_JUL_HANDLER_PUBLISH_THROWS`; `CRATONVM_JULI_NO_CONSOLE_SINK`;
  `CRATONVM_JBOSS_NO_CONSOLE_SINK`.
- `ThreadLocal` under `--compatible`: `CRATONVM_TL_INITIAL_TAIL_CALL`.
- Finalizers: `CRATONVM_GC_SYSTEM_GC_LEAVES_BUSY_FINALIZERS`.
- Generational pinned young copy: `CRATONVM_GEN_PINNED_WATCHED_START_NEEDS_ROOT`.

**Stay default OFF after their rule failed or stayed undecided.** These are pending user
decisions (close-out section 7):

- `CRATONVM_G1_FULL_GC_ONE_REPORT`: vi25-sg F FAIL.
- `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`: G2 has two readings, and the conservative
  one held. **Flipped default ON 2026-10-17** (WARNING, marker `WARNING gci-i48-flipB-20261017`, `=0` kills).
- `CRATONVM_GEN_SERIAL_EDEN_INITIAL`.
- `CRATONVM_GEN_SIDE_ROW_EPHEMERON`. **Flipped default ON 2026-10-17** (WARNING, marker `WARNING gci-i48-flipB-20261017`, `=0` kills).
- `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`.
- `CRATONVM_GC_DEPOSIT_LOCALS_LIVENESS`: reverted by the user on 2026-10-13. vi19-vd E later
  showed no parity cost on G1 or Generational. **Flipped default ON 2026-10-17** (WARNING,
  marker `WARNING gci-i48-flipB-20261017`, `=0` kills).

### Generational: what the gch round changed (2026-10-08)

The gch round ran three waves of parallel lanes (h1-h3) on the Serial-shaped Generational backend
and the common infrastructure; G1 and ZGC internals were out of scope. Each wave was verified on
Linux against HotSpot 25 (`-XX:+UseSerialGC` for Generational) by its battery (`vh1-*`, `vh2-*`,
`vh3-*` lists); the h2 and h3 code ran as one binary, h3a. Waves h1-h3 merged into dev as
`3756535896`. Wave h4 (lanes fl, wr, jm, pc, bp, g1) brought G1 into scope (user, 2026-10-09;
ZGC stays out); its rows are the `vh4-*` lists, still to run, and every switch it flipped or added
names the block that decides it. From h4 on no switch is deleted: one that fails its rule stays
registered and default OFF, and the user decides (triage of 2026-10-09).

- **Round records.** `docs/internal/gc-round-gch-20261008/`: `orch-notes.md` (what was applied,
  flipped, reverted or deleted; authoritative), the `h1-*` .. `h4-*-report.md` lane reports with
  each switch's pre-registered rule and deciding rows, and the `vh*.list` batteries.
- **Open items.** The `gch-*` pages in [`known-issues/gc/README.md`](known-issues/gc/README.md).

**Default ON.** `=0` is the kill switch.

| Switch | What it does, and the evidence |
|---|---|
| `CRATONVM_GC_ESCALATED_PAUSE_SPLIT` | A Generational young pause that escalated into an unrequested major is reported as HotSpot Serial's `Pause Young` + `Pause Full`: two `GC(n)` ids, two JFR collections, `Copy` / `MarkSweepCompact` notifications split at the mark, so the `-Xlog` `Pause Young` count equals the `Copy` bean's. A requested major (`System.gc()`, the allocation ladder) still prints one `Pause Full`. Built gch h1/ob, flipped h2/ob on vh1-ob `ob1_split_*` 8/8 SAME; vh2-ob A on h3a 9/9 SAME (`Pause Young` lines = `copy=48`, `Pause Full` = `msc=2`, ids contiguous). |
| `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS` | The unrecorded-young-root guard excludes the words of conservatively deposited snapshots, except a blocked thread's live locals and operand slots (precise since gch h1/yp). Flipped h2/yg on vh1-yp A (`_def` 2 refused young cycles in 8 runs, `_dep` 0, SAME 16/16); vh2-yg A on h3a SAME 9/9, refusals 0/4 against the `=0` twin's 1/4, G1 / ZGC `PASS all 6`. |
| `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_PARKED_PRECISE` | A parked thread's conservative snapshot keeps its safepoint deposit's precise words (mapped locals, the ruled operand stack, monitors) under the same guard; until then the whole parked snapshot was excluded. Born default ON (gch h3/yc); vh3-yc A on h3a: `parked_precise=` 8-17 on the default arm, 0 on every `=0` twin, 0 refusals in 14 runs, SAME. |
| `CRATONVM_GEN_SWEEP_PARALLEL_PROMOTION_WALK` | The non-moving sweep's selective-promotion walk is decided on the evacuation pool as anchored chunks (two or more); the collecting thread copies the candidates in ascending order, and a refused chunk falls back to the serial walk. Built gch h1/yy, flipped h3/yc on vh1-yy (promote-keep `walk=` 16-26 vs 39-67 ms, pause 118-149 vs 145-217 ms, SAME); vh3-yc B on h3a confirms over all cycles (`walk=` 17-25 vs 36-66 ms, 0 fallbacks, sp-verify clean). |
| `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` | Optimizing-tier frames publish a per-safepoint live set; the band scan skips the dead homes it proves. Flipped gch h3/fd on vh2-fj B on h3a: `fj2_pfl_{on,off}_{1..8}` 16/16 SAME with 0 FAIL lines on either arm, both oracle rows `contradictions=0` (gcg g6 had flipped and reverted it; see the gcf opt-in table). |
| `CRATONVM_GC_NATIVE_FACTORY_ATTEMPT_LADDER` | OOME doors: a native factory refused on an UNLATCHED heap tries its allocation right after the young collection and runs the full rungs only if that fails (HotSpot's `mem_allocate_work` order: young, attempt, full, attempt). Built gcg g3/o, flipped gch h3/os on vh2-ob D (`ob2_door_*` 6/6 SAME, `major=` 34 -> 18 per pair, `factory_enter=8`); vh3b-os D (queued) is its confirmation on all three collectors. |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS` | Every non-moving STW major, not only a requested one, seeds its old mark from the true roots (HotSpot Serial's full-GC root set). Flipped gch h3/mj on a Java-visible defect, not on timing: under the launcher's concurrent-first policy the window's unrequested major left weak references to a dead old holder's young child's targets set (vh3b-mj B2 `mj3b_cf_def_jit_*` STALE 340 of 1527 3/3, ok with the switch; page `gch-h3mj-concurrent-first-full-gc-leaves-weak-refs-of-a-dead-holder-chain-set-FIXED-20261008`). `vh4-mj` F (queued) confirms it on the next build. |
| `CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT` | Default ON, `=0` kills (gci i47w/ws47, a soundness fix). On a Generational pause that reclaimed old storage, the weak String pool keeps an OLD entry only if its address is still an old-generation object base after the collection (`string_table::OldBaseScreen`). A sliding compaction records only moved and watched objects, so a dead pooled String could sit inside a survivor that slid over it. The in-place verdict kept that entry, and the next `intern()` handed out the interior address (`gcg-g4oo` item 5: `Field.name` with a null `value`). Costs one old-generation walk per such pause, and only when an old entry reaches the in-place verdict. Deciding rows: `vi40-fg.list` (FG4 / R3). |
| `CRATONVM_GC_WEAK_STRING_TABLE_WATCH_YOUNG` | With the weak String pool left out of a pause's roots, its young entries join the watched-referent set, so a true-root major keeps their old referents (`string_table::watch_young_pool_entries`). Born default ON (gch h2/jr), the fix of `gch-h2og-true-root-major-frees-the-old-referents-of-a-pooled-young-string-FIXED-20261012`: vh2-jr F `jr2_pooled_{def,pc}` SAME and engaged (`watched_young=3860`), though the `=0` twin is SAME too (no fail-first control); vh3-jx E: no measurable pause cost. |
| `CRATONVM_SOFTREF_HOTSPOT_FREE` | The soft-reference LRU is fed HotSpot's `LRUMaxHeapPolicy` input, `-Xmx` minus the used figure of the last collection (`ref_discovery::soft_lru_free_mb`); `=0` restores `VmHeap::soft_ref_policy_free_mb` (Generational: young-bounded; G1: soft capacity less allocated; ZGC: arena less allocated). Generational since gch h2/jr; G1 and ZGC since gci i48/zl2 (`ebfde29798`, zx48 A3; inert there before). Flipped gch h4/wr, the user's call on parity: vh2-jr D and vh3-jx B `_hs` SAME x4, the young-bounded arm DIFF, no OOME on `softcache`; default-arm battery `vh4-wr.list` S. |
| `CRATONVM_JNI_FOREIGN_TID_PUBLISH` | A foreign-attached thread publishes its OS tid while it runs a Java call or a JNI function, with the default JNI switches too, and withdraws it while idle; `=0` publishes only under `CRATONVM_JNI_FOREIGN_TRANSITIONS`. Born default ON (gch h1/jw); vh1-jw / vh2-jr: 0 `ROSTER HOLE` in every whole `.err` (base included, so no failing control), `noop` cost 850 vs 829 ns. |
| `CRATONVM_TL_INHERITED_PENDING_EPHEMERON` | `--compatible` only: a never-started `Thread`'s inherited `ThreadLocal` values are held by the pending table and pinned to the `Thread` (`metadata_pin`, native-root row `inherited-tl-pending`) on Generational STW collections with no concurrent cycle open, rooted on every other cycle and on G1 / ZGC; `=0` restores the JNI-global values. gch h3/jx; vh3c-jx on h3c sound on every row, Generational `gcCalls=` 3 -> 2 (1 when no true-root major runs; HotSpot 1), inert on G1 / ZGC; flipped gch h4/fl on `vh4-fl` C (the default arm, all three collectors). |
| `CRATONVM_TL_INITIAL_TAIL_CALL` | `--compatible` only: a `ThreadLocal.withInitial` miss runs as a tail call into the VM-assembled `cratonvm/internal/ThreadLocalInitial.supplied` (supplier, then the side-table `set`), so a supplier that parks unmounts its virtual thread as on HotSpot instead of pinning its carrier; `=0` keeps the nested call. gci i27/vt; decided by `vi27-vt.list` block TL. |
| `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` | G1 (in scope from h4): the final remark processes references first, marks their keep closure, then reconciles mirrors and loaders and unloads classes on the final bitmap, HotSpot's order; `=0` restores the old order. Born default ON (gch h4/g1), the fix of `gch-h3os-g1-remark-unloads-classes-before-it-resurrects-soft-retained-referents-FIXED-20261012` and `gch-h2ob-g1-gc-notifications-fail-after-the-first-round-FIXED-20261008`; `vh4-g1` A decides (the `_old` twins must DIFF). Details in the G1 switch table. |
| `CRATONVM_G1_REMARK_ITL_SWEEP` | G1, `--compatible`: the remark sweeps the never-started threads' inherited-`ThreadLocal` buckets on the completed mark; inert under `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP=0`. Born default ON (gch h4/g1), the fix of `gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released-FIXED-20261012`; `vh4-g1` C decides. |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD` | Old generation: the non-moving true-root major does not seed the watched set (gcd d5/r); a watched young survivor its trace did not reach is judged dead (its identity entry leaves the pointer map, `VmHeap::watched_pre_gc_addr_survived` answers no), so its weak references clear and enqueue, a JNI weak global to it clears and a pooled String drops its entry in that same collection, as on HotSpot. The moving major (P-C) gets the same verdict under `CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD` (gci i3/og, default ON since vi5-og). Default ON since jr20 wave 9 (gch h4/wr: vh4-wr A `wr4_itl_wd_cpt_*` `gcCalls=1` SAME, D 137.6 vs 157.0 ms per major, E on h4b SAME) and since gci i1/og (`=0` kills). **Default ON since gci i1/og** (`=0` kills). gch h4/wr. Page `gch-h3jx-gen-true-root-major-keeps-unstarted-compat-threads-one-cycle-FIXED-20261012`; rows and the pre-registered flip rule in `vh4-wr.list` (A, B, D). Counter `[GC] true_root_watched: oldsz_true_root_watched_dead=`. |

**Deleted.** These switches have no readers left; setting them does nothing (each was default off,
so the default path is unchanged):
- the `frame-scan-interior-filter` switch (gcf f7/f; gch h2/fj, removed h3/yc): vh1-fr C printed
  `[frame-interior] dropped=` 0 in 6 runs, and the holder it targeted is the take-over's interior
  rooting, which the filter exempts by design. The `[frame-interior] words_seen=` census stays.
- the `gen-pin-blocked-peer-words` switch (gch h2/yg, removed h3/yc): vh2-yg D found no default
  relocating cycle with a blocked-peer word, and the pinned copy already pins the rest. The
  `[GC] young_blocked_peer_words:` census stays, without its `bpw_enabled=` key.
- the `gen-tlab-refill-spares-large-blocks` switch (gch h1/yy, removed h3/yc with h3/os): alone it
  raised `hashmap-pair`'s young count (77-151 -> 255) and cost 3-8x (vh1-yy); with the Serial-eden
  trigger it refused 47 065 of 49 521 refills and turned the finalize probe 0/64 (vh2-ob B).
  `[GC] young_premium:` lost `yp_refill_spare_caps=`.
- the `gen-precise-root-promote-frames` switch (gch h3/fd): inert on vh3-fd G (`prp_exempt`
  identical on and off in every pair; each counted local had uncounted twins).

The h1 Serial-eden sweep cap under `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` (no switch of its own)
was refuted by vh1-ob B and deleted in h2.

**Opt-in, awaiting a decision.** These were default OFF at the gch close. Each lane report holds the switch's
pre-registered rule. Rows marked **Default ON since gci** were flipped by the gci round (see "what
the gci round changed" above); the rest are still default OFF.

| Switch | Area | State |
|---|---|---|
| `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` | The young trigger and tenuring survivor target of a heap built from `-Xmx` follow HotSpot Serial's eden for its `-Xmx` / `-Xmn` / `NewRatio` (capped at 95 % of the semi-space) instead of 50 % of a semi-space. | **Default ON since the user's flip of 2026-10-13** (WARNING gci-user-flip-20261013, `=0` kills; with the age clock; confirmed by the user's 2026-10-17 WARNING decision, marker `WARNING gci-i48-flipB-20261017`). gcf f7/c. Conformance holds (`OldGenFinalizeProbe` 64/64, 11/11 over f7, vh2-ob, vh3b-os, vh4-fl A). gch h4/fl flipped it and REVERTED it on vh4-fl B (idle host, 5 interleaved rounds): YoungPremium wall median 8578 ms over the bound 8209, hashmap-pair 8246 us over 8208; half the young cycles, each dearer (the fuller-semi sweep, `gcg-g1c-jit-warm-young-forces-a-sweep-per-large-array`) |
| `CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED` | The non-moving sweep's dirty-card seed runs on the evacuation pool (4096 pairs or more). | **Default ON since gci i1/yg** (`=0` kills; vi1-orch T3 pause median 0.90, vi1-yg D SAME). gch h1/yy. vh3-yc C + vh3b-yc A: `mark-card-seed` lower in 8/8 pairs and the pause in 7/8, but the pre-registered median term failed (128 vs 121 ms), so a lane's flip was reverted to opt-in. **Default ON since JIT r20 wave 9 (lane gcI, `=0` kills)**: gm-I.sh on w9c, pause median lower in 4 of 5 interleaved pairs, median of medians 109.1 vs 112.4 ms, PASS line 10/10 |
| `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS` | Old generation (P-C of `gengc-core-system-gc-forces-non-moving`): a MOVING young cycle's Phase 5 major seeds its old mark from the true roots over the post-swap young grid. Not armed on a compacting major, the pinned in-place cycle, or under live compiled frames. | gch h2/og; vh2-og on h3a: correct and engaged (thrash legacy-seed majors 84 -> 5-6), and required on the moving `System.gc()` (`og2_pc_jor_off` FAIL 1 of 8, `_on` PASS all 8), but +18..76 % per major. **Default ON since gci (vi5-og T-W, 2026-10-13)**, `=0` kills it; vi7-flip F-PC confirmed it on the default arm. With `CRATONVM_OLDGEN_COMPACT` default ON too, Phase 5 takes P-C in place where the gate allows and compacts every other moving major (`moving_major_compaction_plan`, gci i8/pa; page `gci-i8rv-oldgen-compact-default-disarms-moving-true-roots-FIXED-20261017`). Dev: **Default ON since JIT r20 GC wave 9 (lane gcA, `=0` kills)**: gm-A.sh d4j on w9c -- witness `GchMj3OccupancyMajorWeakProbe --nojit` SAME (stale 0) vs default STALE 253; steady per-major 632.6 vs off 698.7 ms, total 22637 vs 24455 ms (rule C2); C1 (the cut's gain over P-C) failed and was ruled a size-of-gain term by the orchestrator. |
| `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS`, `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` | Old generation, every true-root major (requested, `..._ALL_MAJORS` occupancy, P-C): the BFS seeds each popped object's young targets in its own scan, so the true-root rounds stop rescanning every marked old object; and the in-mark closure is not repeated after an idle finalizer round (nor inside the finalizer seed). No mark changes. | **Default ON since gci i1/og** (`=0` kills each; the flips stood on vi1-og B + D). gch h4/pc; default ON since jr20 wave 9 lane G (`=0` kills): vh4-pc A parity and D (thrash median per major 103.2 vs 117.1 ms on w9c, steady at h4a) |
| `CRATONVM_JIT_INTERP_DOOR_ARGS_WEAK`, `CRATONVM_JIT_FIRST_CALL_DOOR_ARGS_WEAK` | Interpreter-to-compiled doors (d9d P3: `execute_jit_call`; P2: `interpreter::execute`'s first call) hold an owning body's reference arguments as weak JNI globals across the run, so a parameter the callee nulled can be collected. | gch h2/fj, h3/fd. vh3-fd F on h3a: together they free 3 of the 4 open `Gcd1ArgPinSpecialProbe` cases on every collector (`FAIL 1 of 6` vs 4 of 6), and the keep-alive rows (K) pass 4/4; flip both after the cost A/B (vh3-fd block Q). The last case is `gch-h3fd-proposal-single-pass-call-site-maps-keep-dead-locals-CLOSED-20261015` **Default ON since JIT r20 GC wave 9 (lane gcA, `=0` kills each)**: gm-A.sh d9d on w9c, core-pinned vh4-jm C -- not VOID (off max/min 1.052 / 1.054 / 1.076), BinT median ratio 1.0019, MtChurn 0.9833, door 1.1995 (<= 1.25), stdout SAME. |
| `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS` | The by-name dispatch's bytecode tail releases its argument pins before `interpreter::execute` roots its own copy (d9d P1). | gch h1/fr. vh1-fr B: sound on all three collectors, but on = off: P1 is not the probe's holder |
| `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION` | The call-site service pins a taken callee stash's object words across the frame rebuild and restashes a frame refused after a collection (the gcd-d10f restash proposal). | gch h2/fj. vh2-fj C: never engaged on its rows; stays registered and default OFF, not deleted (user triage, 2026-10-09) |
| `CRATONVM_GC_DEPOSIT_LOCALS_LIVENESS` | The in-native root deposit keeps the bytecode liveness proof for frame locals under the conservative-locals rule, as `collect_roots` does. | **Default ON since the 2026-10-17 user-approved WARNING flip** (marker `WARNING gci-i48-flipB-20261017`, `=0` kills). gch h2/fj. vh2-fj D: sound, no retention row gains. The user flipped it ON on 2026-10-13 and reverted it the same day after vi5-ob D showed it moves AWAY from HotSpot (`cleared-while-parked=true`; HotSpot keeps a parked frame's dead local) |
| `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT` | JNI package only (with its no-signal mark): a thread entering native during a pause arrives at the barrier and does not park. | gch h2/jr. vh2-jr B / vh3b-jx A: engaged and sound, `in_native_region` average 2.6-9x lower, but `ns_per_round` unchanged and the `noop` cost bimodal on the host; rides the package flip |
| `CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE` | JNI package only, with the flag-first request order (`CRATONVM_STW_LOCKFREE_BLOCKED`, default ON): the in-native leave takes no barrier lock when no pause is in progress (a per-thread leaving word the census counts; `[GC] jni_phase:` key `leave_lockfree`). Opt-in. | gci i20/jt; vi20-jt LL |
| `CRATONVM_JNI_IN_NATIVE_KEEP_SNAPSHOT` | JNI package only, a thread whose frames are all interpreted: the in-native leave's full wake (a pause completed or folded into the thread while it was in native) keeps the snapshot the pauses folded instead of a refresh deposit, and a VM-only JNIEnv function then re-enters native incrementally (`check_post_block_gc_in_native`; `[GC] jni_phase:` key `leave_wake_kept`). Default ON; `=0` restores the refresh on every wake. | gci i48 jl48; i10al item 4 (`ja11_w_tag`) |
| `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` | JNI package only (with its no-signal mark), Generational, a compiled caller: the in-native deposit skips the compiled frames' coverage proof and chain scan; the pause scans the thread's band `[SP below the dispatch, stack top)` instead (`ThreadRegistry::scan_in_native_bands`), with the caller's callee-saved registers spilled into the dispatch's frame. Census `dep_band_at_pause`, `pause_band_*` on `[GC] jni_phase:`. `CRATONVM_DBG_JNI_BAND_SKIP_PAUSE_SCAN=1` is its UNSOUND positive control. | **Default ON with the package since gci i1/jn** (`=0` kills; the package itself is default ON since the user's flip of 2026-10-13). gch h4/bp (the g4jn build). The G1 twin `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1` is default ON since vi6-g1r |
| `CRATONVM_DBG_JNI_BAND_SKIP_PAUSE_SCAN` | Debug, UNSOUND: with `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` on, the pause does not scan the in-native bands (`gc_and_alloc::append_in_native_band_roots`); the positive control of `vh4-bp` block B. Never for production. | gch h4/bp |
| `CRATONVM_G1_NATIVE_OOME_DEBT` | G1: arm the native funnel's post-OOME debt (paid with the complete marking cycle), as Generational does by default. | gch h4/g1. Page `gcd-d10o-native-funnel-oome-debt-is-generational-only-REFUTED-20261012`; `vh4-g1` F decides a flip |
| `CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS` | Diagnostic: at each concurrent initial mark, counts the seeded old objects only old holders' young children name (`[GC] conc_y2o_holders:` under `CRATONVM_GC_STATS=1`); changes no seed. | gch h3/mj. vh3-mj A: about 0 on the battery, 917 refs / 12.9 MB at one `--nojit` initial mark; kept for real-workload runs |
| `CRATONVM_DBG_XT_REFUSAL_CENSUS` | Diagnostic: one `[xt-refusal] arm= tid= reason=` stderr line (with `depth=` / `band=`) per helper window that refused a collection (gcd-d9c step 1). | gch h3/fd. vh3-fd C: every fallback named, all `shadow-unpublished` |
| `CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS` | Single-pass GC maps drop every liveness-dead reference local (Stage 2 slots, the local oracle, the register mask, the shadow push), not only one whose register another local shares; never under a debugger or inside a splice. | gch h4/jm (g1/s's deleted rule rebuilt with a control: d9d's `spliced-direct` holder is `relay`'s dead parameter home). vh4-jm B decides |
| `CRATONVM_GC_SP_STACK_PRIM_ROOTS` | A single-pass frame's operand slots its exact maps classify as primitives stop being band roots (object starts that are not minted handles), as the optimizing tier's prim colours already do. | gch h4/jm (gcg-g5jm's named holder). vh4-jm A decides, after the `[sp-band]` census says whether the holder is a primitive operand |
| `CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS` | A registered-chain optimizing-tier frame published `frame_holds_no_references` stops being a band root below `callee_saved_lo` (its register tail is still scanned): no root, no G1 pin from a stale word an earlier activation left there. | gci i23/rf (proposal P1/rf, common-w36b); opt-in, decided by vi23-rf E/E2/S/O + R |
| `CRATONVM_DBG_SP_BAND_OWNERS` | Diagnostic: a compile-time `[sp-band]` stderr line per single-pass safepoint map, each operand-spill word below the cursor classed `oop` / `prim` / `inline-local` / `unowned`; `1` = every method, else a method-label substring. | gch h4/jm |

**The pinned young copy** (`CRATONVM_GEN_PINNED_YOUNG_COPY` with `_PARALLEL` and
`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER`) stayed opt-in at the gch close: the battery half of its
pinstale6 gate passed on h3a (vh2-yg B, `vh2-yp-pinstale6-gate`), but its MANUAL netty and QDox rows
could not run on the host. **All three are default ON since the user's flip of 2026-10-13**
(WARNING gci-user-flip-20261013, `=0` kills each; see the gci section).

**Fixes without a switch.**
- The GC's compiled RBP-chain walks are bounded by the entry band (`GcChainWalkGuard`); a walk
  cut at the limit is an incomplete coverage proof (`compiled-frame-band-unbounded`), no longer a
  silent "the chain ended" at 4096 activations (gch h1/jw; vh2-jr A: 9 cuts on the
  `CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY=0` arm, 0 by default).
- `-Xlog:gc+phases` full collections carry the old collection's real Phase 2 / Phase 3 times (gch
  h2/og; see the `CRATONVM_XLOG_GC_FULL_PHASES` row of the g5 table).
- The Proxy and annotation-proxy dispatch doors raise a catchable `OutOfMemoryError` when a
  mirror mint is refused on a full heap, instead of aborting (gch h2/fj; vh2-fj E 4/4).
- An embedded real-JDK VM preallocates its `OutOfMemoryError` at its first `invoke` after init
  level 4 (gch h1/jp; `jp1_oome_launcher_*` 4/4 SAME).
- Every root-producing frame scan roots every local, not only the liveness-proven ones, while a
  debugger can read locals, as HotSpot does under `can_access_local_variables` (gch h1/fr +
  h1/jp; the JDWP row is MANUAL and still owed).
- Compiled code loads the VM context through the checked loader on both tiers; a compile that
  would read an unreserved slot is refused by name (gch h2/fj; vh2-fj A: 0 bails).
- The parallel sweep prefix discards an attempt whose chunk reclaimed a forward the sequential
  walk retains (gch h1/yy), and the parallel evacuator takes one old-generation lock per
  promotion-buffer refill instead of two.

**Diagnostics added.**
- `CRATONVM_DBG_GCPHASE=1` also prints `[gcphase] selective-split: walk= installs= fix3a= fix3b=
  fix3c= par_walk= ...` per non-moving sweep (gch h1/yy).
- `CRATONVM_DBG=oldmark-root-census` also prints the `[holder-census]` door attribution lines
  (`door pinned-markers:`, `frames:`, `interp:`; h1/fr), an `unreached marker` line per staged
  marker no root reaches (h1/jo), and `[xt-frame]` per frozen peer frame (h3/yc).
- `CRATONVM_DBG_MIC_TRACE` prints `[MIC_TRACE] <callee> kind= cycles=` per MIC door call (h1/fr).
- `--verbose:gc` prints `[GC] gc_notification_failed: n= collector= action= cause= error=` for
  the first four failed GC-notification deliveries (h2/ob, with the message since h3/os).
- Shutdown census keys: `[GC] young_unrecorded_roots:` gains `urr_excluded_cycles=`,
  `urr_excluded_roots=` and `urr_max_streak=`; `[GC] young_pinned_copy:` gains
  `pycopy_incomplete_raw_jni=`; `[GC] young_blocked_peer_words:` is new; `[GC] oldgen_sizing:`
  gains `oldsz_moving_legacy_seed_majors=`; `[GC] young_premium:` gains `yp_par_walks=`,
  `yp_par_walk_fallbacks=` and `yp_par_card_seeds=`; `[GC] gc_notifications:` splits the first
  delivery's latency (`notif_first_*`); the `[GC_OVERHEAD]` line ends `backstop=`; the
  `[GC] jni_phase:` exit line now prints on a normal return from `main` (h3/jx).
- `[GC] true_root_watched:` (new, `oldsz_true_root_watched_dead=`) and
  `[string-table] ... left_out=` (h4/wr).
- `CRATONVM_DBG_GCPHASE=1` also prints `[gcphase] old-gen mark-closures: count=` per old
  collection, and `mark-rounds-in-bfs: count=1` / `mark-post-close-skipped: count=1` under the
  h4/pc switches.
- `[GC] jni_phase:` gains `dep_band_at_pause`, `pause_band_scan`, `pause_band_threads`,
  `pause_band_words` and `pause_band_roots` under `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` (h4/bp).
- `-Xlog:class+load` (and JVMTI `SetVerboseFlag(CLASS)`) prints HotSpot 25's
  `<external name> source: <src>` load lines (h4/fl; residuals on
  `gch-h4fl-xlog-class-load-residuals-20261009`).

**User triage of 2026-10-09** (the orch-notes USER TRIAGE list): keep
`CRATONVM_GEN_CONC_OWED_POLL`, `CRATONVM_LOCK_KEY_SHARDS` and
`CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION` as registered, default-off switches (user, 2026-10-09);
`CRATONVM_SOFTREF_HOTSPOT_FREE` flipped (h4/wr, in the default-ON table above); G1 brought into
the round's scope, so the G1 remark class-unloading fix shipped in h4/g1
(`CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP`). Still awaiting the user: retire
`CRATONVM_GC_OVERHEAD_CRAWL_BACKSTOP` (proposal `gch-h2ob-proposal-retire-the-overhead-crawl-backstop-CLOSED-20261015`).

### Generational: what the GC defects round changed (2026-09-27 → 2026-09-28)

The GC defects round worked the open Generational and common pages with
HotSpot 25 `-XX:+UseSerialGC` as the oracle; G1 and ZGC internals were out
of its scope.

- **Summary, waves, retired and filed pages:**
  `docs/internal/gc-defects-round-20260927/README.md`; per-lane records are
  the `d1-*` .. `d8-*` reports beside it.
- **Still open:** indexed in
  [`known-issues/gc/README.md`](known-issues/gc/README.md), with the ranked
  proposals and every opt-in switch's flip gate.
- **Figures below:** the orchestrator's Linux release runs; the last set is
  the round branch at `307f0c6a2` ("d7"). The standing battery there reads 84
  SAME / 32 DIFF against HotSpot Serial, against 48 DIFF at the round's start,
  with no row newly DIFF.

**Defaults that changed.** Each is Generational only and has a kill switch
(`=0`; the first one also takes `false` / `off`).

| switch | default | what it does |
|---|---|---|
| `CRATONVM_GC_OLD_HUMONGOUS_TOP` | **ON** since the round's triage (opt-in since gen r5w3/oldgen7) | A humongous array (one that goes straight to the old generation) is carved from the END of the highest free block that can hold it, committing only its own pages, instead of the best fit from low addresses. Humongous arrays then cluster at the top, so a dead one merges with the free tail. `GenR4W4HeapFullThrashProbe -Xmx128m` passed 3/3 with it and 0/3 without on the d4 build. Counters `oldsz_humongous_top_allocs` / `oldsz_humongous_top_fallbacks`. `=0` restores the best fit. |
| `CRATONVM_GC_FUTILE_YOUNG_BACKOFF` | **ON** (new, gcd d2/j) | An object door whose forced young cycle left young unable to hold the object spills later objects to the old generation, and forces the next young cycle only after a quantum of allocation (256 KiB, doubling, at most 2 % of the heap), instead of one collection per miss (`gc_and_alloc::futile_young_backoff_skips`). |
| `CRATONVM_JIT_SHADOW_BAIL_BLOCKS_MOVING` | **ON** (new, gcd d2/f; per thread since d3/o) | After a compiled shadow-stack push bails on the buffer's end, its oops are published nowhere, so a young collection takes the non-moving sweep while any proving thread's shadow stack is still deep enough to hold that push (`shadow_bail_refuses_moving`). `CRATONVM_JIT_SHADOW_BAIL_STICKY=1` (opt-in) refuses every later moving cycle instead, as d2/f first did. |
| `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD` | **ON** (new, gcd d4/n) | A moving young cycle's Phase 5 major does not slide the old generation on a fragmentation request while compiled frames are live, since their unmapped words could name old objects. Counted by `oldsz_moving_major_vetoes`. |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS` | **ON** (new, gcd d4/n; widened by d5/r) | A REQUESTED major (`System.gc()`, the allocation ladder) seeds young from the true roots (`TrueRootYoung`, `TrueRootMajorScope`), not from every young survivor, so dead old data that a dead young object names is freed in the same collection. The seed also takes every address the VM's weak tables hold for the pause (the reference processor's rows, JNI weak globals). The young objects it leaves out are freed by the next young collection, which is why the ladder's second major stays. It falls back to the legacy seed when the cycle promoted anything, the old walk has a gap, or a pinned compaction is planned. Counters `oldsz_true_root_majors`, `oldsz_true_root_fallbacks`, `oldsz_true_root_young_excluded`. |
| `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS` | **ON** (new, gcd d4/o; phi kills since d5/u) | The optimizing tier's dead-home clears no longer keep a value past its own definition: a value a loop recomputes through a spliced call, or through a phi, is not kept across that loop's calls (`refine_reach_by_def_kills`). |
| `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES` | **ON** (new, gcd d6/u) | The dead-home clears judge a value's range by its value reads only; a call or load the IR reuses as the next memory operation's ordering token no longer keeps its result's home alive (`value_use_ranges`). With it, `GenR5W2OsrDeadSlotProbe` passes 6/6 on d7 (the `=0` control fails 2/2). |

`CRATONVM_GC_OVERHEAD_PROGRESS` (default on since gen round 4) now covers
more of the OOME ladder; `=0` still restores the pre-round ladder whole:

- **Compiled allocation.** An allocation in compiled code that meets a
  latched GC-overhead limit runs a major collection on its own thread first,
  as the interpreter does, and throws only if the limit stays latched
  (`jit_overhead_limit_major`, gcd d1/b). The JIT helpers decide an OOME
  after the same majors as the interpreter's ladder
  (`majors_to_decide_oome`, d4/o and d5/u).
- **A second major before the error.** When the first major before an
  `OutOfMemoryError` freed little, a second runs (gcd d4/j).
- **The native door.** A native allocation never collects on its own. After a
  heap OOME, the thread that raised it owes the native funnel one collection,
  paid at its next native call, at most twice, and not once the old
  generation has recovered (`note_heap_oome_raised`,
  `native_call_owes_oome_major`; gcd d4/j and d5/q).
- **Exception construction.** Building an exception under a latched overhead
  limit runs a major first, and the last-ditch Generational major retries a
  lost stop-the-world race (4 attempts) and withdraws its request (gcd d2/j).

**Correctness fixes on the default path, no switch.**

* **The compiled `monitorenter` crash, the last netty signature, is fixed.**
  `ir_lower.rs::scan_frame_needs` did not reserve the context slot for
  monitor ops, so the monitor stub loaded the saved caller RBP as the
  helper's VM pointer. JIT round 13 fixed it (`48f4bb76d`, merged into this
  round). gcd d1/a added a VM-pointer screen in the helpers
  (`[cratonvm] FATAL <helper>: vm_ptr=… is not a SharedVm`) and IR
  context-slot audit tests. The d7 netty run (four `io.netty.buffer` classes
  x 3) is clean
  ([`gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928`](internal/gc/gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928.md)).
* **Young objects keep their defining loaders.** A stop-the-world old
  collection retains all of young, and since gcd d1/c also each young
  object's defining loader (`mark_young_to_old_refs`), as the concurrent
  cycle does (`gen_conc_young_instance_loaders`). A young walk anomaly seeds
  the loaders of the published headers inside the skipped stretch (d2/g).
  The missing seed was the lost class loader behind the share sizer's
  mechanism (A) and behind `CRATONVM_GEN_YOUNG_MIRROR_DEFER`'s null static.
* **Reference rows.** The concurrent sweep drops the reference-processor rows
  of the `Reference`s it frees, by freed span and registration number
  (`concdrv_sweep_reference_rows_dropped`, gcd d1/c). Where two soft rows
  share an address (a dead `SoftReference`'s row not yet pruned, and a new
  one on its block), the later registration owns `SoftReference.get()`'s
  touch, as it owns `clear()` / `enqueue()` (`claim_soft_addr`, d3/n).
* **Retained layouts.** A stop-the-world collection that reclaimed old storage
  also takes the retained-layout census (`gen_stw_layout_census`, d2/g), so a
  layout a concurrent remark retained is released even when every later old
  collection is a stop-the-world major.
* **Young copy.** A precise root naming a from-space address the object-start
  walk did not record refuses the moving cycle, once per address
  (`[unrecorded-young-root]`, counted on `[GC] young_unrecorded_roots:`,
  d2/h), instead of dangling. The evacuator's refusal ledger
  (`[forward-refused]`, `evacuator_verdict=`) is per heap (d3/m).
* **Cards.** `take_dirty_cards` answers from the byte map alone; the tracking
  list is gone (d1/d). On an element-precise table, a promoted reference
  array defers its young elements' cards, not its header's (d2/h); the old
  generation's pinned compaction re-dirties a surviving array's young
  elements' cards (d3/n), and so does the non-moving sweep's selective
  promotion (`push_promoted_holder_cards`, d5/s). The CAS lock arm no longer
  marks an array's header card after the element's (d3/o).
* **TLAB refill and the `-Xms` floor.** A Generational TLAB refill never
  carves a buffer smaller than the object that missed
  (`refill_tlab_at_least`, d1/d; the G1 and ZGC arms are still open), and the
  evacuated semi-space's give-back stops at the `-Xms` floor
  (`Arena::decommit_unused_above`), so there is no per-pause release and
  re-commit churn.
* **JNI.** An exception a JNI function could not build (heap exhausted) pends
  the preallocated `OutOfMemoryError` instead of escaping as a sentinel
  (`pend_unbuilt_throwable`, d4/k), and `JNI_OnLoad` runs inside an implicit
  local frame and a raw-locals bracket (d4/o).
* **Thread stack size.** A platform thread's carrier honours
  `Thread(group, target, name, stackSize)` and `-Xss`
  (`read_thread_stack_size_request`, `carrier_stack_bytes`, gcd d6/f): a
  request above the 8 MiB default gets `requested + 8 MiB`, rounded up to
  1 MiB and clamped at 1 GiB. Compiled self-recursion still has a fixed
  4 MiB budget (`SELF_CALL_STACK_BUDGET`), so a deep recursion that HotSpot
  runs on a large-stack thread still overflows here (JIT-owned).
* **JFR.** `-XX:StartFlightRecording` (both the `=` and `:` forms) reaches
  JFR through the launcher (d5/f); a recording of a Generational run with the
  service thread shows the concurrent cycle's `jdk.GCPhaseConcurrent` rows
  (45 `Concurrent Mark`, 45 `Concurrent Sweep`) and its pauses. On G1 (gci
  i8/jf, i12/ge) the rows take HotSpot G1's names: the cycle has
  `Concurrent Mark From Roots`, `Pause Remark` and `Pause Cleanup`, every G1
  collection has its top-level `GC Pause` row, and the concurrent start's
  `GC Pause` is its young collection's row; a collection's young/old event
  follows HotSpot's operation (full collections old, `Metadata GC Threshold`
  young on G1), and `jdk.SystemGC` is written once per request. Since gci
  i9/jc a recording started through the JDK's `jdk.jfr` (`Recording`,
  `RecordingStream`, `jcmd JFR.start`) gets the same collection and
  concurrent-cycle rows as `-XX:StartFlightRecording` (before, it got no
  collection at all), and both recorders nest a Generational or G1
  collection's phases as HotSpot Serial does (one `GC Pause` row, the phases
  as `jdk.GCPhasePauseLevel1`).

**Which OOME recoveries still fail, and why.** A requested major's true-root
seed falls back to the legacy young seed on promoting cycles. That fallback
is the holder the census names in the remaining OOME-recovery failures:
`Gcd1ThreadExitSpillProbe` (2 of 9 default runs pass),
`Gcd1PinnedCalleeOomeProbe` (0 of 12), and the one-major arm of the nepotism
page. Each failing run has `oldsz_true_root_fallbacks` above
`oldsz_true_root_majors`, and a `cat=seed/promotion-dest` holder. Separately,
heap-full OOME shapes can livelock on young cycles that all fall back to the
non-moving sweep (1 in 12 default thrash runs;
[`gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928`](internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md)).
`GenR4W4NativeStringOomProbe` cannot judge any of this against HotSpot:
HotSpot 25 dies in its string-concatenation bootstrap, so its four lines are
the probe's own verdict.

**JNI and threads in native.** Two of the three JNI switches are default ON
since 2026-10-10 (gc JNI retirement):

- `CRATONVM_JNI_INDIRECT_LOCALS` (default ON, `=0` kills it and the foreign
  transitions with it): a JNI local ref handed to native code is an indirect
  `(frame, slot)` handle into the local-frame table every moving collection
  rewrites, as in HotSpot, not a raw heap address. Free on the default path
  (`Gcd1JniCostProbe` `noop` 812 vs 834 ns, Linux).
- `CRATONVM_JNI_FOREIGN_TRANSITIONS` (default ON, `=0` kills it; never on
  without indirect locals): an attached host thread runs every JNIEnv function
  as a counted mutator (idle -> running -> idle) and keeps the locals it makes
  outside a Java call in an attach-level frame until detach.

Evidence for both: `Gcd1JniRootsProbe` prints HotSpot's seven lines 3/3 on
each collector (Linux and Windows), 0 `ROSTER HOLE` under the audit, and
`tools/bench/GcJniLibrariesProbe.java` (JNA upcalls and native-thread
callbacks, lz4-java, zstd-jni, netty-tcnative) prints HotSpot's lines.

Opt-in at this round's close, and the reason a Java thread inside a JNI native was still a
counted mutator (a native that blocks in C holds every pause). **Since the user's flip of
2026-10-13 the native transitions are default ON on every backend** (WARNING
gci-user-flip-20261013; `CRATONVM_JNI_NATIVE_TRANSITIONS=0` kills them), so the whole package
is the default; the cost table below predates the flip:

- `CRATONVM_JNI_NATIVE_TRANSITIONS` (gcd d3/k): the native runs in native
  (GC-safe, HotSpot's `_thread_in_native`); every JNIEnv call transitions
  native -> VM -> native, and pauses exclude the thread for the rest of the
  call. Inert without indirect locals, because the native's own copies of its
  local refs must survive a move.
- Since gcd d5/f, a JNIEnv call that only touches primitive contents of one
  of the native's own locals (`GetArrayLength`, `Get/Set<T>ArrayRegion`,
  `GetString[UTF]Length`, `DeleteLocalRef`) runs in a leaf window: the
  thread stays in native and a pause request waits for the window to close
  (`GcBarrier::try_open_leaf_window`, `GcBarrier::drain_leaf_windows`).
  Leaving native skips the wake's fixup and snapshot refresh when no pause
  ran in the window.

The native transitions' cost is still above the flip bar (`Gcd1JniCostProbe`,
medians, d7; "default" there is the pre-2026-10-10 default, which the two
flipped switches did not change measurably):

| call | default | all three switches |
|---|---|---|
| leaf JNIEnv call (`int-region`) | 24-27 ns | 34-36 ns (about 1.4x) |
| empty native (`noop`) | about 0.53 us | 4.1-5.6 us (8-10x) |
| `NewStringUTF` | about 0.5 us | 7.9-9.5 us (about 17x) |

The flip waits on
[`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928`](internal/proposals/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md).
Round 13's `CRATONVM_FFM_DOWNCALL_GC_SAFE` (opt-in) runs an FFM downcall in
native the same way; since gcd d5/f a JNIEnv call from C inside such a
downcall leaves the downcall's region first and re-enters on return. The
default dispatch of an empty JNI native costs about 525 ns against HotSpot's
5-10 ns, a separate open page
([`gcd-d5f-jni-default-dispatch-costs-500ns-20260928`](internal/gc/gcd-d5f-jni-default-dispatch-costs-500ns-CLOSED-20261012.md)).

**The young copy under conservative roots (opt-in).** The pinned young copy
(`CRATONVM_GEN_PINNED_YOUNG_COPY`) gained three opt-in arms this round:

- **Take-over arm** (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1`, gcd d3/m).
  A young cycle whose only divert is helper windows the pass pinned whole
  runs the pinned copy, with every young word of each window's band pinned.
  It engages on d7: `ptko_candidates=39 ptko_taken=39` on its probe.
  Since gci i11/tk it is no longer declined under `CRATONVM_OLDGEN_COMPACT`
  (default ON since the user's flip): Phase 5's veto already keeps a
  ledger-pinned cycle's major in place, so the compaction intent guarded nothing.
- **Blocked-monitor proof** (`CRATONVM_XT_BLOCKED_MONITOR_PROOF=1`, d2/i and
  d3/m). A peer blocked in the compiled `monitorenter` helper, whose
  blocking deposit proved its JIT chain rewritable, is credited like a
  parked peer; its band is read into the young pin ledger.
- (The d5/s no-promote hold on a requested major was removed in gce e2: gcd
  d9/a's wide true-root seed made it redundant.)

On a ledger-pinned young cycle, as on any cycle with live compiled frames
under the guard above, Phase 5's major still runs and consumes a
fragmentation request, but in place: only the compaction is vetoed
(`major_gc_finalizing`'s `no_compact`; gcd d3/m, d4/m, d4/n). The pinned copy was still not
default at this round's close: row 3a of its flip gate passes on its main probe (arm D `PASS all 8`
5/5), but row 3, liveness, does not. With the futile-young backoff at its
default, arm D passed 1 of 2, and two flag-on runs timed out in the livelock
above. (It is default ON since the user's flip of 2026-10-13, with `_PARALLEL` and the
ledger guard; the take-over arm and the blocked-monitor proof stay opt-in.)

Also opt-in this round:

- `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` (d2/g). The non-moving sweep does not
  pin a young root value all of whose root occurrences came from statics,
  interned strings, class mirrors or JNI globals, so such an object can be
  promoted under a live JIT (`[GC] precise_root_promote:`). **Default ON since
  gci vi6-pn C10** (2026-10-13, `=0` kills; confirmed by vi7-flip F-PRP).
- `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` (d5/q; deleted by gcg g6/yg, see
  that wave's section). The non-moving sweep was triggered by its own pause
  loop (90 % start) instead of the moving collector's pause-goal-lowered
  threshold, and printed `[GC] young_sweep_trigger:` when on.
- `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD` (d5/s), the element-card consumer.
- `CRATONVM_GC_PREALLOCATED_OOME_KINDS` (d2/j). Preallocates `Requested array
  size exceeds VM limit` and `Metaspace` errors beside the `Java heap space`
  singleton.

**New diagnostics.**

* `[GC] young_pinned_takeover: ptko_candidates= ptko_taken=
  ptko_ledger_incomplete= ptko_over_bound=` (d3/m) and
  `[GC] young_pinned_takeover_declines: ptkod_reached= ptkod_no_takeover_peer=
  ptkod_frozen= ptkod_unread= ptkod_window_refused= ptkod_derived_unresolved=
  ptkod_other_coverage= ptkod_other_divert=` (d5/s). Why the take-over arm
  declined a cycle, by first failing reason
  (`GenerationalHeap::pinned_takeover_declines`).
* `[GC] header_screens: hscr_forward_declined= hscr_busy_declined=
  hscr_busy_wait_abandoned=` (gen r5w6/pin10, printed since d5/s). How often a
  header screen refused a forward chain or a BUSY mark
  (`gen_heap::header_screen_counts`). `hscr_busy_wait_abandoned` should read
  0; it did on all 202 d7 prints.
* `[GC] young_unrecorded_roots: urr_cycles_refused= urr_failed_open=
  urr_roots_sum=` (d2/h), with the `[unrecorded-young-root]` line per refused
  address.
* `[GC] oldgen_sizing:` gained `oldsz_moving_major_vetoes`,
  `oldsz_true_root_majors`, `oldsz_true_root_fallbacks` and
  `oldsz_true_root_young_excluded` (d4/n). A fallback count above the major
  count is the legacy-seed signature above.
* Under `CRATONVM_DBG=gc-overhead`: `[GC_OVERHEAD] oome-majors:`,
  `[GC_OVERHEAD] native-oome-debt:` and `[GC_OVERHEAD] futile-young verdict:`,
  one line per ladder decision (d2/j, d5/q).
* **Band-word provenance.** Every rooted band word in the
  `CRATONVM_DBG=oldmark-root-census` holder census carries a `prov=` suffix
  (`band_word_context`, `vm/src/jit/conservative_roots.rs`). Since d1/b it
  gives the frame's tier (`tier=ir` / `sp`), its active safepoint, the live
  cursor the scan used and whether that safepoint's map names the word
  (`in_map=`). Since d7/u it also gives `rbp=`, `above_scan=` (how far above
  the scanner the frame sits) and `returns_to=` (`jit:<method>` or `native`),
  which tells a callee's frame after its exception was caught from the same
  method's frame during the fill.

**Triage of the opt-ins and the proposals (d8).** Every opt-in switch has a
verdict with its evidence in
`docs/internal/gc-defects-round-20260927/triage.md` ("Final triage, wave
d8"). Two became defaults (above); the rest are NOT YET, each with a gate, or
NEVER as a default:

- **Shrink after a concurrent cycle** (`CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT`
  and `CRATONVM_GC_OLD_INTERIOR_DECOMMIT`). The old generation shrinks after
  stop-the-world collections only, as Serial's tenured generation does. The
  pair returns memory after a concurrent cycle, a G1-style footprint option
  for embedders. It is not a candidate for the default, because Serial keeps
  that memory (`GenR4W6OldShrinkProbe` prints `FAIL` on HotSpot).
- **Bisection levers and superseded arms** stay opt-in, and the no-gain
  evacuator items are candidates for removal.
- **Proposals.** The GC proposal pages were triaged the same way: 6 done, 16
  rejected, 54 kept, each with a gate, a size and a rank
  (`d8-y-report.md`, `d8-y-decisions.tsv`).

### Generational: what rounds 4 and 5 changed (2026-09-23 → 2026-09-27)

The generational rounds 4 (waves 1-6) and 5 (waves 1-6) reworked most of
this backend.

- **Per-lane records:** `docs/internal/reviews/gengc-round4-*.md` and
  `gengc-round5-*.md`.
- **Still open:** indexed in
  [`known-issues/gc/README.md`](known-issues/gc/README.md).
- **Figures below:** the orchestrator's measurements on Linux release
  builds, not the lanes', which built nothing. The last set was taken on the
  round-5 tip `c6c98c760`, the wave-6 merge.

**Defaults that changed.** Each has a kill switch (`=0` unless stated).

| switch | default | what it does |
|---|---|---|
| `CRATONVM_GEN_TLAB_TAIL_SINK` | **ON** since gen r5w2 | A retiring TLAB's unused tail goes back to young from-space (the bump cursor retracts, or the tail joins the young free list) instead of being buried under an `int[]` filler the young trigger counts as used. The sink is offered a tail only when its first word reads zero. `GenR4W2ParkAllocProbe 8 20000 64`: 6.4 s / 610 minors → 3.5 s / ~40; `GenR4W4SkewedShareProbe` 108 → ~60 minors; correctness identical on every arm. Counters `gen_tlab_tail_*` on `[GC] gen-alloc:`. |
| `CRATONVM_SOFTREF_HOTSPOT_LRU` | **ON on Generational** since gen r5w4, **and on G1 since gci i5g/g1o** (per-backend default, `soft_lru_takes_hotspot_inputs_by_default`; `=0` kills); dev: **ON on G1 and ZGC** since jr20 wave 9 (lane gcE: the VM latches the Generational default on every backend; G1 never cleared an idle soft reference, ZGC cleared a hot one). Effective default: **ON on all three** (ZGC through its own default `SOFTREF_HOTSPOT_LRU_ZGC`, zr z1/rf, via `ReferenceProcessor::new_for_backend`); `=0` kills on every backend (zx48 audit C) | The SoftReference LRU policy takes HotSpot's `LRUMaxHeapPolicy` inputs (the soft clock and free heap as of the previous collection), so a soft reference used since the last collection is not cleared by the policy; `-XX:SoftRefLRUPolicyMSPerMB` is honoured. `GenR5W2SoftRefIdleYoungProbe`: `lost=0 PASS` by default, as HotSpot (was `lost=2 FAIL`). |
| `CRATONVM_GEN_XMS_USABLE_FIRST` | **ON** since gen r5w4 | The `-Xms` startup commit is split HotSpot Serial's way (one survivor of copy reserve, the rest usable), so a fresh `-Xms64m -Xmx512m` heap reports `totalMemory()` ≈ 64m, not 32m. `GenR5W3XmsTotalProbe` PASS by default (= HotSpot). |
| `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` | **ON** since gen r5w2 | Every young collection asks the concurrent-start trigger once, including those entered from compiled code (`maybe_gc_forced`), and a due cycle runs inline. Before, compiled code's collections never asked, so the concurrent-first policy was inert in JIT-heavy programs and old-gen reclamation fell to STW majors. `[GC] conc_driver:` `concdrv_start_asked` / `concdrv_start_due`; per door: `[GC] conc_doors:`. |
| `CRATONVM_GEN_ZERO_ONCE` | ON (r4w4) | Young memory handed out from the bump tail is not zeroed a second time (the semi-space wipe or the OS re-commit already did it); `gen_zero_repairs=` counts refused skips. |
| `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED` | ON (r4w6) | Primitive arrays of 64 KiB and more are zeroed after the old-gen lock is released. Reference arrays: opt-in, below. |
| `CRATONVM_MONITOR_ENTER_SPIN` | ON (r4w5) | Up to 24 attempts before a contended `monitorenter` parks; without it every lock hand-off retired a 256 KiB TLAB (the two-thread heap-full thrash). |
| concurrent-first (`CRATONVM_GC_NO_CONCURRENT_FIRST=1` restores the legacy trigger) | ON (r4w4) | The old generation is reclaimed by the concurrent cycle first. Adaptive start threshold (45 % until measured, then `F − 1.25G − C/32`, clamped 20-75 %; `CRATONVM_GC_CONC_START_PERCENT` pins it); the STW major defers to an open cycle unless requested, after an allocation failure, or at 90 %. `[GC] conc_policy:`, `[GC] major_cadence:`. |
| `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` | ON (r4w5) | Without it concurrent-first starved above the 75 % floor: `GenR4W5MajorCadenceProbe` went from 518 back-to-back STW majors per 545 young collections to 0, with 89 concurrent cycles. |
| `CRATONVM_GC_OLD_SHRINK` | ON (r4w4) | After a STW old collection the committed end shrinks (HotSpot's `MaxHeapFreeRatio` 70, damped, never below `-Xms`); `Runtime.totalMemory()` falls. |
| `CRATONVM_GC_OLD_OOM_COMPACT` | ON (r4w4; widened r5w1) | A fragmentation or humongous refusal arms one compacting collection (`compact_around_pins` when no conservative pin exists), so `GenR4W4HumongousFragProbe` passes where it threw `OutOfMemoryError`. On the non-moving path, where conservative pins exist, the request is answered by `CRATONVM_GC_OLD_PINNED_COMPACT` (default on since the GC defects round) instead of being discarded. |
| `CRATONVM_GC_OLD_WALK_GAP_RECOVERY` | ON (r4w4) | The STW old sweep recovers through a walk gap instead of stopping all old-gen reclamation; old→young roots inside such a gap are now found by the card scan and pinned (r5w1/r5w2). |
| `CRATONVM_GC_OVERHEAD_PROGRESS` | ON (r4w5; r5w2) | The GC-overhead limit's mutator-progress half; a full live heap throws `OutOfMemoryError` instead of thrashing, and a latched streak throws after the soft-reference rung (fixes the `chain-hot` hang). The GC defects round widened it: compiled allocations, a second major, the native door (see "Generational: what the GC defects round changed"). |
| `CRATONVM_GC_PRECISE_ARRAY_CARDS` | ON (r4w3; r5w2) | An old reference array's store dirties the element's card, not the header's; deferred cards are `(holder, offset)`. Since r5w6 the non-moving sweep re-dirties the element's card too. |
| `CRATONVM_JIT_IR_DEAD_HOME_CLEARS` | **ON** since gen r5w5 | Optimizing-tier frames zero the homes of dead IR references at armed calls. It is the largest single lever on `GenR4W6JitOomRootProbe`: with it off, 1 shape passes. |
| `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` | **ON** since gen r5w5 | An OSR entry clears the reference locals of the interpreter frame it leaves behind. |
| `CRATONVM_JIT_RAX_DEAD_AT_CALL` | **ON** since gen r5w6 | A single-pass Java invoke's register oop mask leaves RAX out: nothing reads its pre-call value. This fixed `GenR5W3OsrHolderProbe`'s `safepoint-gpr-spill-image` holder. |
| `CRATONVM_GC_CALLEE_SAVED_IMAGE_LIVENESS` | **ON** since gen r5w6 | A callee's saved image of a caller's register is decided by the compiled frame that owns the register. A Rust caller keeps the word. |
| `CRATONVM_JIT_IR_PRECISE_KEEP_SET` | **ON** since gen r5w6 | The IR keep set is a liveness with kills; snapshot locals are narrowed by bytecode liveness, and phi homes are clearable. |
| `CRATONVM_GC_IR_PRIM_SLOT_ROOTS` | **ON** since gen r5w6 | An optimizing frame's primitive-coloured slots stop being roots unless the word is a minted handle. |

**Correctness fixes on the default path, no switch.**

* **Stack scanning stops below static TLS.** glibc keeps each thread's static
  TLS block and `struct pthread` at the top of the mapping
  `pthread_getattr_np` reports as the stack, so an own-stack band that ran to
  that end scanned (and could remap) TLS words.
  `cratonvm_gc::shadow_stack::clamp_stack_top_below_static_tls`, used by
  `current_thread_stack()` and the conservative-roots limits, stops bands
  below it (fails open, i.e. unclamped, when it cannot tell; the main thread's
  TLS is malloc'd and needs no clamp). Code deriving a stack top must use
  those helpers, never a raw pthread or `/proc/self/maps` end. Also: a scan
  whose SP is outside the thread's own stack fails closed
  (`OWN_STACK_FOREIGN_SP`), native-slot write-backs are routed to the newest
  claimant of an OS tid and refused off the owner's stack
  (`NATIVE_SLOTS_REFUSED_OFF_STACK`), and JIT-frame remap stores are bounded
  the same way (`JIT_REMAP_REFUSED_OFF_STACK`).
* **The evacuator.** Helpers keep their forwarding reservation, a claimed
  forward is waited for without a bound (a bounded wait could hand back
  NULL), and a CAS loser fills its copy. The thirteen young linear walks share
  one grid probe (`probe_grid_at` + `GridRules`), and the young commit screen
  is one immutable snapshot.
* **Full-heap failure paths throw instead of aborting.** `ldc` of a String or
  Class constant, class-mirror creation at static-synchronized and native
  first uses (`VmHeap::try_set_field`), and the array-limit OOME keeps its
  "Requested array size exceeds VM limit" message. HotSpot's
  `-XX:+ExitOnOutOfMemoryError`, `+CrashOnOutOfMemoryError`,
  `-XX:OnOutOfMemoryError=` and `+HeapDumpOnOutOfMemoryError` are honoured.
* **A TLAB cursor in a register is not a root into a skip span.**
  `skip_spans_hold_no_root` counts such words (`SKIP_SPAN_CURSOR_ROOTS`)
  rather than reporting a violation.
* **Header screens never follow a forward the heap did not install**
  (gen r5w5/pin9, r5w6/pin10, reconciled with dev `ed3a63469`).
  * Conservative candidates are sized through
    `ObjectHeader::screen_shape(inside)`. It follows a forward only into this
    heap's arenas, never waits on a BUSY-looking mark, and bounds its hops.
    `shape_source` additionally follows only targets in a span that has
    recorded a reference.
  * This fixed the netty `addr=0x100000004` SIGSEGV. The `Value::Object`
    payload word whose high half read as a FORWARDED mark was sized through
    its neighbour cell.
  * Refusals are counted by `gen_heap::header_screen_counts()`, printed as
    `[GC] header_screens:` (since the GC defects round), and by the
    `[GC] forward_screen:` line.
* **The young pin-ledger deposit reads a header only inside a young region**
  (`YoungPinRange::contains_span`, round-5 merge). The ledger's range
  includes one-past-end, and a stack word equal to the arena's end had its
  header read on the unmapped page after it. That was 3 of 12 netty runs on
  the default path.
* **Allocation from natives collects before it throws.** The
  `String.valueOf` / `Integer.toString` natives allocate with
  `create_string_uninterned_gc_safe`.
* **A TLAB refill is never smaller than the object in hand**
  (`object_floor`, gen r5w6/sizer10).
* **The concurrent cycle fails closed:**
  * the remark refuses its sweep when the mark bitmap does not cover the
    old generation;
  * the concurrent young-to-old seed word-scans young stretches it cannot
    parse instead of dropping them (gen r5w6/conc10).

**What the collector does not do yet, and why a JIT-warm program feels it.**
Once compiled code is live, essentially every young cycle diverts to the
in-place non-moving sweep (the `unrewritable_conservative_jit_roots` term
above): `GenR4W4JitWarmDivertProbe` 151 of 151 cycles. Young then fragments,
collects 130-170 times where HotSpot Serial collects ~34, and each sweep is a
serial O(allocated) walk. `GenR4W4EvacThroughputProbe` took 7.6-8.2 s on the
round-5 tip against HotSpot's 3 s, with the same checksum (14.5 s on wave 4);
single runs on the GC defects round's d7 build read 17.9-23.1 s by default and
14.1 s with the pinned copy, its parallel arm and the card seed, on a host
whose in-JVM timings swing about 3x between runs.
([`gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young`](internal/gc/gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-DONE-20261012.md)).
The pinned in-place young copy below is the fix. It is not the default: the
corruption family in the next paragraph is closed, but row 3 (liveness) of its
flip gate still fails (see "Generational: what the GC defects round
changed").

**The memory-corruption family (`generational-bytebuf-suite-sigsegv-…`).**
On the default path it is root-caused and fixed: every own-stack band ran to
the end of the stack mapping, which holds the thread's static TLS and
`struct pthread`, so TLS words that looked like heap objects were scanned and
rewritten by moving collections (the `%fs:0`, `OnceLock<bool>`,
hashbrown/`Arc`/mimalloc pointer crashes). The bands now stop below static
TLS (above); the QDox 4-thread parse under gc-stress went from SIGSEGV to
3/3 and the Finalizer crash from ~3/5 runs to 0/10.

Waves 5-6 explained the netty signatures that remained, and the first two
were on the default path too:

1. **`addr=0x100000004`** (the header screens above) was fixed by pin9 and
   pin10.
2. **A read past the young arena's end** in the pin-ledger deposit was fixed
   at the merge.
3. **The compiled monitor path**, the last one, is fixed too.
   `jit_monitor_enter` received a stack address as its VM pointer (4 of 20
   default netty runs on the round-5 tip, 1 of 20 with the pinned copy)
   because `ir_lower.rs::scan_frame_needs` did not reserve the context slot
   for monitor ops; JIT round 13 fixed it (`48f4bb76d`), and the GC defects
   round's d7 netty run is clean:
   [`gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer`](internal/gc/gengc-r5w6-orch-jit-monitor-enter-gets-a-stack-address-as-its-vm-pointer-FIXED-20260928.md).

No signature is left; both pages are retired. See
[`generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928`](internal/gc/generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928.md).

**Opt-in features awaiting a default decision.** Each is off by default; the
page named is where the flip criteria live. The "state" column is updated to
the GC defects round's final triage (d7 build); the verdict for every switch,
with its evidence, is in `docs/internal/gc-defects-round-20260927/triage.md`,
and the gates are indexed in
[`known-issues/gc/README.md`](known-issues/gc/README.md).
`CRATONVM_GC_OLD_HUMONGOUS_TOP` left this table: it is default on since that
round (its defaults table above).

| switch | what it does | state |
|---|---|---|
| `CRATONVM_GC_OLD_PINNED_COMPACT` | The non-moving old-gen collection answers a fragmentation request by compacting around everything a conservative word names (`OldGen::compact_around_pins`), where it otherwise discards the request; JIT-warm requested and ladder majors take that path. Counters `oldsz_pinned_compactions` / `oldsz_pinned_refusals`. | Opt-in. Flipped in `88b5b2dd7` and reverted: with it on, `GenR4W5OldPinnedCompactProbe` passes, but every planned pinned compaction makes the full collection fall back from the true-root seed to the legacy young seed (`d1b_armC`: `oldsz_true_root_fallbacks=46` vs `oldsz_true_root_majors=2`, 67 pinned compactions), and the OOME-retention rows fail again (`foome_jitoomroot` 4/5, `jit_oom_root_both`, `d1b_jitoomroot_B`, `w2_jitoomroot_clear`). Gate: the true-root seed must hold across a pinned compaction ([`gcd-d5r` true-root proposal](known-issues/gc/README.md)), then the same rows. |
| `CRATONVM_GEN_PINNED_YOUNG_COPY` | Young cycles relocate under conservative JIT roots: each conservative word pins its 4 KiB page, pinned objects forward to themselves, every other survivor is copied into free from-space spans or promoted; declines when the pin ledger is incomplete or pinned pages exceed 1/8 of from-space (`moving-pinned-pages`, `[GC] young_pinned_copy:`). | **Default ON since the user's flip of 2026-10-13** (WARNING gci-user-flip-20261013, `=0` kills; parse `truthy_word_default_true`). JitWarm 125/125 moving; `GenR4W4EvacThroughputProbe` 7.0-7.2 s against 7.6-8.2 s default (round-5 tip). The monitor-path crash that held the netty rows is fixed. NOT YET (d7): row 3a of the flip gate passes (`GenR4W6JitOomRootProbe` arm D `PASS all 8` 5/5), row 3 (liveness) does not (arm D with the backoff at its default 1/2; two flag-on 300 s timeouts, the fallback-young livelock). [`gengc-r4w6-pinstale6-pinned-copy-default-flip-gate`](internal/proposals/gc/gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-CLOSED-USER-DECISION-20261012.md) |
| `CRATONVM_GEN_PINNED_WATCHED_UNNAMED_NOT_ROOTS` | Pinned in-place young cycle (**default ON since gci i35** -- WARNING flip after vi35-wy WY3-F/G PASS; `=0` kills): a watched object (weak referent, Reference, queue, JNI weak referent, weak String entry) that no pin word names is not a seed of the cycle -- kept in place if reached, reclaimed otherwise -- so a weak reference whose referent shares a pinned page with a compiled-frame-held Reference can clear. Since i34 a pin word's base-minus-offset reach (256 bytes) names only arrays for this arm. | gci i30/wy; default ON in i30, back to OPT-IN after vi30-wy on i30a (WY-R 0/9, WY-E `pxr_dropped_sum=0`, WY-S FAIL by the letter). Flip rule met: vi35-wy WY3-F (with the i35 START_NEEDS_ROOT rule). Page gci-i26wy. `=0` kill proven: vi39-rv TW1-TW5 PASS (i47w, Windows rows; `=0` prints the pre-flip `inline=LIVE viaMethod=LIVE` 7/7 incl. `--compatible`). |
| `CRATONVM_GEN_PINNED_WATCHED_START_NEEDS_ROOT` | Inside the `CRATONVM_GEN_PINNED_WATCHED_UNNAMED_NOT_ROOTS` arm (default ON since the i35 WARNING flip): a pin word exactly at a watched object's start makes it a seed of the pinned in-place cycle only if the pause's root vector holds the object -- or a card walk-gap value names it (gci i39/rv: a heap word the card scan cannot trace is a root; before i39 such an object was dropped and could be freed under the word) (the pin still keeps it in place). | gci i35/wy; default ON, `=0` restores the i34 rule. Rows vi35-wy WY3-F. |
| `CRATONVM_DBG_YOUNG_PIN_SWEEP_ENTRY_ONLY` | Diagnostic, UNSOUND: the young pin ledger's layout-free sweep stops at the outermost compiled entry instead of the stack top (gen r5w2/roots6), to name whether a pin word came from the Rust frames above the entry. | gci i35/wy; rows only (vi35-wy WY3-P). |
| `CRATONVM_GC_BAND_MAP_DEAD_NOT_ROOTS` | Compiled-frame band scan (opt-in): a word in a region the dataflow models (java-local home, operand spill below the live cursor, mapped ref-hoist slot) that the resolved active safepoint map (slot offsets + local-oop-mask homes) does not name is neither a root nor a pin -- the deadness claim the band verifier and the young pin ledger already spend. Targets dead compiled-frame words that keep dropped data alive (GciOm31KeptOomeProbe, GciI32EcKeptFinalizableChildProbe aged). | gci i38/ds; pages gci-i31om, gci-i38ec. Flip rule: vi38-ds VERDICT (LEVER + S + Z). |
| `CRATONVM_DBG_FRAME_ROOT_UNNAMED` | Diagnostic: one stderr line `[frame-root-unnamed] obj= cid= method= off= region= map= ... verdict=rooted` (or `verdict=excluded-map-dead`) per compiled-frame band word the marking scan roots (or the opt-in map-dead claim lets go) although the active safepoint map does not name its slot. | gci i38/ds; rows only (vi38-ds K, E, M4). |
| `CRATONVM_GEN_PINNED_FINALIZABLE_UNNAMED_NOT_ROOTS` | Pinned in-place young cycle: a registered finalizable object that no pin word names is not a seed of the cycle (kept in place if reached; otherwise resurrected in place and reported, so its finalize() runs as on HotSpot). | gci i31/fz, page gci-i26ed2; default ON again (WARNING flip, gci i35): vi35-fz S off/on twins 6/6 equal (the vi31 S DIFFs are the open wcy / jor defects); `=0` kills. `=0` kill NOT yet proven: vi39-rv TF1 / TF2 VOID on Windows (the ERG rows need the vi35-fz host geometry; both arms printed 64/64); TF3 / TF4 PASS. |
| `CRATONVM_GEN_IN_PLACE_RESURRECTION_CLOSURE` | Pinned in-place young cycle: the objects the finalizer-resurrection drain queues for an in-place scan (from-space span copies, kept survivors, pinned non-seeds) join the depth-2 resurrection set (`take_resurrection_closure`), so a weak reference to an object only a dead finalizable reaches clears at that pause, before finalize(), as on HotSpot (before: one pause late). | gci i40/gn, page gci-i38ec; default ON, `=0` kills (the w36-d record: to-space and promoted only). Rows vi40-gn R / K / H. |
| `CRATONVM_DBG_GEN_FIN_CHILD_CENSUS` | Diagnostic: at the start of each moving young cycle's Phase 2.5, one stderr line `[gen-fin-child] fin= fin_class= child= child_class= holder= to= pinned= seed= in_roots= in_place_cycle=` per young reference slot of each dead young finalizable about to be resurrected; `holder` is finalizer-only, root-vector, pin-seed, pinned-reached, strong-path, old-gen or outside-young. Reads only; at most 32 lines a cycle. | gci i40/gn; vi40-gn C names the holder of the held shape. |
| `CRATONVM_GC_OLD_EXACT_START_CLASS` | Old-gen free-list allocation starts its size-class search at the class that contains the request, so a 16- or 24-byte request reuses an exactly fitting hole (the `size + align - 1` start, harmless with whole power-of-two classes, skipped those two classes once gen r4w4/oldgen4 split them in quarters). Placement is unchanged for 8-aligned requests of 32 bytes and up. | **Default ON since gci i41/ev** (`=0` restores the old start class); `vi41-ev.list` A + B1 + C keep it. |
| `CRATONVM_GEN_CONC_REMARK_THREAD_SATB_FLUSH` | Generational concurrent remark: every thread's SATB bucket (a peer the take-over froze never flushed its own) is pulled into the shards before the remark's first closure, so the dead-finalizer retention and the remark-time reference processing see the whole snapshot (before: a reachable finalizable named only by a frozen peer's bucket was finalized; under the refproc hook its weak reference was cleared). As G1's remark (finding #18). | gci i42/mk, page gci-i42mk; default ON, `=0` kills (the late flush in `remark_finish`). Rows vi42-mk U / A / C. |
| `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL` | The pinned cycle on the parallel evacuator. | Rides with the above: **default ON since 2026-10-13** (gci i8/pe flipped it with the copy under the user's decision, `=0` kills). [`gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy`](internal/gc/gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-DONE-20260928.md) |
| `CRATONVM_GC_PAR_EVAC_CARD_SEED`, `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE` | Evacuator items W2 and L2 tail window. | No gain measured on the evac probe; they ride with the pinned copy. (W1, W4 and the scan prefetch were removed in gce e2.) [`gengc-r5w3-evac7-proposal-young-copy-round-two`](internal/gc/gengc-r5w3-evac7-proposal-young-copy-round-two-RETIRED-20260927.md) |
| `CRATONVM_TLAB_SHARE_SIZER` | HotSpot-shaped per-thread TLAB sizing (allocation share per young cycle / 50, 8 KiB-1 MiB) plus the refill-waste limit. | **Default ON on Generational since gcg g6/jn, and on G1 since gci i48/zl3** (`=0` kills on both; ZGC stays OFF): vg5-jn C, 3 interleaved pairs: park 82-88 -> 37 minors, skew 116 -> 113, rate a tie, ConcUnload and NativeStringOom --nojit correct 5/5. Open reading: the JNI package's `full` repeat mode ran 14-55 % slower on g5a than on g1a with the sizer on (waves apart, not isolated; `vg6-jn` B decides). [`gengc-r5w4-orch-share-sizer-hands-out-live-memory`](internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md) |
| `CRATONVM_GC_OLD_INTERIOR_DECOMMIT`, `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` | Return interior free runs of the old generation to the OS, and shrink after a concurrent cycle, not only after a STW one. | NEVER as a default: HotSpot Serial keeps that memory too (`GenR4W6OldShrinkProbe` prints `FAIL` on HotSpot; the pair prints `PASS shrunk`). An embedder's footprint option. [`gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection`](internal/gc/gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection-SUPERSEDED-20260928.md) |
| `CRATONVM_GC_ADAPTIVE_TENURING` (or any of `-XX:MaxTenuringThreshold` / `InitialTenuringThreshold` / `TargetSurvivorRatio`) | HotSpot's adaptive tenuring; `-XX:+PrintTenuringDistribution` honoured. Default: fixed age 3. | `GenR4W6TenuringProbe` `died-young` (= HotSpot) with it; the default promotes 96 % of the medium-lived set. Its blocker (`GenR4SoftRefLruProbe` failing with it) was a default-path bug, fixed in gcd d4/n. NOT YET: items 4-8 of the page's gate not run with the flag. [`gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure`](internal/gc/gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure-DONE-20260924.md) |
| `CRATONVM_GEN_CONC_SERVICE_THREAD` | A VM daemon drives the concurrent cycle (wakes on hand-off, on direct old-gen growth, or every 20 ms) instead of mutators polling it after young collections. | Every probe row run with it matches HotSpot (d7: 30+ runs over the conc-mark-gate, steady-promotion, rset, fragmentation, direct-old-growth and remark probes). Held for a gc-stress run, a thread-visibility check (it registers no `Thread` object) and a latency number, and because on `GenR4W4SteadyPromotionProbe 4` its arm completes no concurrent cycle. [`gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection`](internal/gc/gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md) |
| `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, `CRATONVM_GEN_CONC_CLASS_UNLOAD` | Reference processing at the concurrent remark, and class unloading by the concurrent cycle (needs the hook); unloaded classes' layouts are retained until a sweep census proves no instance is left (gen r5w4/conc8). | With `CRATONVM_GEN_Y2O_LIVE_SEED=1` and `CRATONVM_GEN_YOUNG_MIRROR_DEFER=1` (gen r5w6/conc10, also opt-in), `GenR5W5ConcUnloadProbe` unloads the dead loader and class exactly as HotSpot Serial does, with the JIT on (3/3). Without those two it prints `dead-loader-unloaded=false`. The remark matches HotSpot on `GenR5W5RemarkRefsProbe` except `weak-to-resurrected-cleared`, which the hook arm still prints `false` on d7. `CRATONVM_GEN_YOUNG_MIRROR_DEFER` is the switch that makes the concurrent cycle able to unload a class at all: with it alone `GenR5W5ConcUnloadProbe` = HotSpot 3/3; without it 63 concurrent cycles unload nothing. NOT YET: the four flip together after the remark token is fixed and a full battery. [`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`](internal/gc/gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md), [`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes`](internal/gc/gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-DONE-20260926.md) |
| `CRATONVM_GEN_SATB_OLD_ONLY` | The SATB pre-barrier logs only old-generation old values. | [`gengc-r4w5-concmark5-satb-barrier-logs-young-old-values`](internal/gc/gengc-r4w5-concmark5-satb-barrier-logs-young-old-values-20260924.md) |
| `CRATONVM_GEN_Y2O_LIVE_SEED`, `CRATONVM_GEN_YOUNG_MIRROR_DEFER` | The concurrent cycle seeds old gen only from LIVE young objects (a young trace first), and a young user-class mirror is not a root while it is young. | Needed for concurrent class unloading with the JIT on (row above). [`gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects`](internal/gc/gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md) |
| `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`, `CRATONVM_GC_STAGED_ARGS_KEEP_MASK` | Store-form and scanner-form fixes for compiled frames that keep dead references as roots after an OOME. `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` moved to the defaults above in r5w5. | NEVER as defaults (d8): no d7 probe needs them. (`CRATONVM_JIT_OSR_DROP_ORPHANS` was in this row; dev has since made it default ON, `=0` kills.) `GenR5W3OsrHolderProbe` and, since the GC defects round's value-range dead homes, `GenR5W2OsrDeadSlotProbe` pass by default; `NativeGrowthReclaimProbe` does not. [`gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2`](internal/gc/gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2-FIXED-20260928.md) |
| `CRATONVM_JIT_INLINE_CARD_MARK` | Compiled old-receiver reference stores check the card inline instead of always calling the barrier helper. | **Default ON since the gcg triage tg1** (`=0` kills; gcg g1/p on vg1: old-receiver stores ~150x faster on both tiers, 3.5x on the single-pass tier, verify row clean). Clean cards are marked inline too since gcg g6/jm (`CRATONVM_JIT_CARD_STORE_BARRIER`, default ON). [`gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper`](internal/gc/gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper-FIXED-20260924.md) |

**Observability, as it now stands on this backend** (HotSpot Serial's shapes;
the probes print identical stdout on `java -XX:+UseSerialGC`):

* **`Runtime`.** `totalMemory()` is `VmHeap::committed_bytes`: one
  semi-space's commit plus the old generation's, **excluding the copy
  reserve** (to-space); `maxMemory()` is the semi-space ceiling plus the old
  reservation, clamped to `-Xmx` (`GenR5W2HeapNumbersProbe` matches Serial).
* **Pools.** `Eden Space` committed is the young term, `Tenured Gen`
  committed the old generation's commit (it reported the reservation until
  r5w1), `init` is reported, and the pool usages sum to `committed_bytes`.
  `Survivor Space` is no longer empty: young live bytes are split at the last
  collection's survivors on moving and non-moving cycles
  (`GenR5W3SurvivorPoolProbe`). `Tenured Gen.getCollectionUsage()` moves only
  on full collections, as Serial's. Pool peaks are sampled at both collection
  edges.
* **Thresholds.** The pools support Serial's thresholds (`Tenured Gen` usage
  and collection usage; `Eden Space` / `Survivor Space` collection usage
  only); crossings are checked at each collection's end (and when a usage
  threshold is set) and delivered through the pool beans' own
  `Sensor`s as `MEMORY_THRESHOLD_EXCEEDED` /
  `MEMORY_COLLECTION_THRESHOLD_EXCEEDED` on the `MemoryMXBean`
  (`GenR5W3PoolThresholdProbe`; the pool beans are built by the JDK's own
  constructor since gen r5w4/obs8 — on `897210f83` the registered constructor
  bridge ran instead and every pool reported `false/false`). Not modelled:
  HotSpot's check after slow-path allocation, so a crossing by direct
  old-generation allocation is seen at the next collection
  ([`gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth`](internal/gc/gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth-20260926.md)).
* **`ManagementFactory.getMemoryMXBean()`** is one bean per VM (it was a fresh
  object per call, so no listener on it could ever be notified).
* **`getCollectionTime()`**: floor of the bean's microsecond sum (row above).
* **`-Xlog`** (every backend): HotSpot's default decorators
  `uptime,level,tags` with HotSpot's padding, every HotSpot decorator
  accepted, comma-separated selections, `all`, `disable`, `tag*` wildcards,
  output options accepted (no rotation), `%p` / `%t` in file names, and
  repeated `-Xlog` options honoured. A bare `-Xlog` is `-Xlog:all` and no
  longer takes the next argument (gen r5w4/obs8). On Generational the
  collector prints Serial's `Using Serial` and, since gcg g3/x, HotSpot's
  ten-line `gc,init` block (with `Large Page Support`, `NUMA Support`,
  `Compressed Oops`, `Pre-touch`, and `Heap Min Capacity` as HotSpot's
  `MinHeapSize`) at startup, and per collection `gc,start`, two `gc,heap`
  lines (`DefNew: … Eden: … From: …`, `Tenured: …`), the `gc` line and
  `gc,cpu` (`User=… Sys=… Real=…`). `gc+heap=debug` adds the `Minimum heap …`
  sizes line and the `Heap Before/After GC invocations=…` blocks,
  `gc+metaspace=debug` the `Metaspace Before/After GC` blocks, and
  `gc+ref=debug` / `gc+phases+ref=debug` the reference-processing block
  (gcg g3/x). The tag sets `gc+phases+ref`, `gc+phases+start`,
  `gc+promotion`, `gc+tlab` and (gcg g4/yg) `gc+jni` are accepted, and
  `gc,cpu` prints on Windows too (gcg g4/yg). The concurrent cycle's line is
  under `gc+marking`. Not printed yet: `gc,heap,exit`, `gc,age`
  ([`gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines`](internal/gc/gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines-DONE-20260926.md)).
  Full collections print Serial's `gc,phases` `Phase 1..4` lines by default
  since gcg g5/yg (`CRATONVM_XLOG_GC_FULL_PHASES=0` kills them).
* **JFR.** `tenuringThreshold` is HotSpot's unit (`promotion_age - 1`); the
  concurrent cycle emits `jdk.GCPhaseConcurrent` rows and its pauses
  `jdk.GCPhasePause` rows; `GCHeapSummary("Before GC")` carries the committed
  size before the collection.
* **Shutdown census** (`--verbose:gc` or `CRATONVM_DBG=gc-stats`): new lines
  `[GC] gen-alloc:`, `tlab-sizer:`, `conc_policy:`, `conc_driver:`,
  `conc_doors:`, `major_cadence:`, `young_conservative_divert:`,
  `young_pinned_copy:`, `oldgen_sizing:` (the `oldsz_*` keys, printed since
  r5w3), and `conc_unload:`. The GC defects round added
  `young_pinned_takeover:`, `young_pinned_takeover_declines:` (`ptkod_*`),
  `header_screens:` (`hscr_*`), `young_unrecorded_roots:` and four
  `oldsz_*` keys (see "Generational: what the GC defects round changed").

**G1.** A contiguous arena split into fixed regions with an O(log R)
address→region table. Young pauses evacuate all Eden+Survivor regions
except pinned ones (JNI-critical pins, conservative-JIT pins, frozen-peer
tails); remembered sets (per-region source sets fed by the post-barrier
plus a GC-internal rebuild each pause) drive old→young discovery. Mixed
pauses add the most-garbage Old regions bounded by count and copy-time
budget — only regions with liveness data from a completed mark cycle are
eligible. Marking is SATB tri-color with a background worker that also
drains the SATB shards each step; cleanup frees wholly-dead Old regions
in place, reclaims dead humongous spans and arms mixed collections.
Humongous objects (> half a region) occupy physically contiguous region
runs and are never evacuated — but a pause can still free one: after
Phase 5 it reclaims any span that neither a root nor any object its
Phase-4 walk visited refers to, which is what stops short-lived
humongous garbage waiting on a mark cycle that may never fire. Evacuation
failure self-forwards live objects in place, keeps their regions, and a
same-pause drain recovers them; a wedged drain leaves the kept regions
coherent (remembered-set edges recorded, precise liveness answers).
Allocation failure escalates: young pause → synchronous full mark cycle
→ OOM.

*Young sizing, opt-in.* `max_gc_pause_ms` reaches exactly one
decision by default — how many OLD regions a mixed collection set may take.
The young half is bounded by the free pool alone (`needs_gc` fires below
25 % free), so Eden grows to roughly three quarters of `-Xmx` and young
pause time scales with the heap SIZE. `CRATONVM_G1_YOUNG_PAUSE_TARGET=1`
adds an adaptive young-region target: shrink 20 % after a pause that
overruns the goal, give 12.5 % back while pauses stay under half of it,
floor/ceiling 5 %/60 % of regions. It does nothing until a PRODUCTIVE pause
has been measured to overrun — the target starts at its ceiling and a
target at the ceiling is not a trigger — and an unproductive pause (nothing
copied, nothing freed) resets it to the ceiling so it can never storm.

It is **not** a default, and the reason is measured. On
`probes/G1ChurnPauseProbe 96 900` at `-Xmx2048m` (96 MiB retained, 3.6 GiB
of garbage, 200 ms goal), medians of 3 interleaved reps:

| arm | wall | pauses | total pause | p50 | p99 |
|---|---|---|---|---|---|
| pre-audit baseline | 5773 ms | 3 | 3801 ms | 1082 ms | 1726 ms |
| audit fixes, flag OFF | 2633 ms | 3 | 719 ms | 236 ms | 243 ms |
| audit fixes, flag ON | 2744 ms | 4 | 814 ms | 187 ms | 250 ms |

The 7x pause reduction there belongs to the audit's evacuation-destination,
free-region-scan, region-scrub and remembered-set fixes — the middle row has
this flag off. The flag itself buys the third row against the second: p50
−21 %, p99 **+3 %**, wall +4.2 %, one extra pause. p99 is what a pause goal
is about and it did not move, and on an adaptive scheme it cannot: the
target only tightens after a pause has already overrun, so the largest pause
is always paid in full and it is the one p99 reports. A latency-sensitive
workload may still want the median improvement — turn it on and measure your
own pause distribution.

**That table predates five findings, and it was re-run.** The +4.2 % wall-clock
is the cost of taking MORE pauses at the per-pause price of the day, and that
price changed: the parallel evacuator now runs on JIT-warm pauses instead of
falling back to serial (F-01), forwarding moved out of a side hash map (F-02),
collection-set membership stopped being a SipHash lookup on the innermost loop
(F-03), the parallel driver stopped taking the whole-heap fix-up (F-04), and
cleanup stopped walking the heap (F-06).

Re-run 2026-09-02 on the same probe and heap, RELEASE build, eight interleaved
reps per arm, medians:

| | flag OFF | flag ON | delta |
|---|---|---|---|
| p50 pause | 644 ms | 580 ms | **&minus;10 %** |
| max pause | 826 ms | 749 ms | **&minus;9 %** |
| total pause | 2074 ms | 2007 ms | &minus;3 % |
| wall | 8102 ms | 8258 ms | **+1.9 %** |
| pauses | 3 | 5 | +2 |

The trade improved in the predicted direction and by roughly the predicted
amount: the wall-clock cost more than halved (+4.2 % &rarr; +1.9 %), total pause
went from a cost to a small saving, and the reduction now reaches the MAXIMUM
pause as well as the median — which is what a pause goal is actually about, and
what the original measurement could not show.

**It still ships opt-in, and the reason is the host, not the numbers.**
`/proc/loadavg` read 28&ndash;44 throughout, from other work on the machine, and
this document's own rule — the one every number in the older table was taken
under — is that a contended host inverts an A/B of this size. A +1.9 % wall cost
measured at load 40 is not evidence that the default should change. What would
settle it is the same eight reps on an idle machine; the harness and the probe
are both in the tree now, so that is a twenty-minute job rather than a
reconstruction.

One obstacle had to be cleared first: **the probe was not in the tree**. It was
committed with the measurement, then deleted along with the rest of `probes/` by
a "major doc consistency update" while every citation of it survived — including
this document's, which also named the wrong directory. It is restored, and its
`checksum` line is there so a run can be diffed against a real JDK's — all
sixteen runs above produced `checksum=2063754854400`, identical to JDK 25's.

*Where a young pause actually goes.* Every `--verbose:gc`
`[GC-STAT]` line now carries a per-phase breakdown — `roots_us`, `rset_us`,
`closure_us`, `fixup_us`, `free_us`, `verify_us`, `other_us` — with `fixup_us`
printed beside the `fixup_regions` / `fixup_bytes` it covered, because a slow
walk and a large old generation are different problems. The seven fields SUM to
`pause_us`: `verify_us` is the budgeted post-pause dangling-reference sweep,
which runs in release builds and used to be charged to no phase at all, and
`other_us` is the derived remainder. A table whose rows do not sum to the total
cannot be used to argue that a cost was removed rather than moved, which is
exactly what the rest of this section tries to do with it.

*What the pause DECIDED, beside what it cost (2026-09-20).* The phase
breakdown says where a pause went and the region census says what it bought;
neither says what the collector chose. Every `[GC-STAT]` line now also carries
the state of the four adaptive controllers, read after they have been updated
for that pause — so the fields say what the NEXT pause will be sized by:

| field | controller | read it when |
|---|---|---|
| `goal_us` | `max_gc_pause_ms`, in the unit everything else on the line uses | always — it is the denominator of every other decision |
| `young_target` / `young_bounds` / `young_now` | `update_young_target` | pause frequency looks wrong. `young_target` at the top of `young_bounds` is the ceiling, which is also the state in which the young half of `needs_gc` is deliberately inert |
| `free_pool` / `free_trigger_pct` | `needs_gc`'s free-fraction arm | a pause storm: a pause that frees bytes without returning REGIONS leaves this trigger latched |
| `tenuring` / `survivor_target` | `update_tenuring_threshold` | survivors are being copied too many times, or promoted too early |
| `evac_ns_per_byte` / `fixup_ns_ema` / `old_budget_ns` | `update_evac_cost`, `old_cset_copy_budget_ns` | a mixed collection set looks too small. `old_budget_ns` is the goal MINUS the fix-up walk's rolling cost, so a fix-up that has grown to the whole goal leaves the old half nothing but its one guaranteed region |
| `mixed_remaining` / `old_gen_bytes` / `mark_threshold` | the IHOP and mixed-phase state | old-generation growth is not being reclaimed |

At exit, `[GC] g1 young-sizing:` reports the same young controller's end state
beside the `[GC] g1 ihop:` and `[GC] g1 tenuring:` lines that were already
there — the young target was the one adaptive policy nothing printed.

The tenuring histogram on that line used to be structurally zero whenever
`CRATONVM_G1_ADAPTIVE_TENURING=0`: the evacuators fill it unconditionally and
the only thing that snapshots and clears it ran behind the flag, so the off arm
both reported nothing and accumulated forever. The snapshot now runs on every
pause; the flag gates only whether the derived threshold is published.

*The card screen's skip-rate, as a trend (2026-09-20, wave 2).* Phase 2's card
screen has counted the bytes it scanned and the bytes it stepped over since
F-05, and those totals were rendered in exactly one place — `[GC] g1
card-clean:` at shutdown, which is reached only from `vm-cli`'s normal-return
teardown. A workload that ends in `System.exit` never unwinds to it, so on the
JUnit suites the rate was never printed at all, and a `debug!` would not have
helped either: `release_max_level_info` compiles it out and no `RUST_LOG` value
recovers it from a shipped binary.

That made one specific experiment unrunnable, and it is the one that decides
`CRATONVM_G1_CARD_CLEAN`'s default. G1's card table is **additive-only**: it
saturates by construction, so the screen's skip-rate decays over a long run and
cleaning is the only thing that pushes back. Whether cleaning earns its cost is
therefore a question about the SHAPE of that decay, which a single cumulative
total averages away. Every `[GC-STAT]` line now carries:

| field | meaning |
|---|---|
| `card_scanned_total` / `card_skipped_total` | run-cumulative bytes the screen visited and stepped over |
| `card_skip_rate_pct` | the cumulative rate, derived from those two — not accumulated separately, because two accumulators drift |
| `card_skip_rate_first_pct` | the rate over the run's **first 64 instrumented pauses**, frozen. The trend's left endpoint |
| `card_skip_rate_now_pct` | the same rate smoothed 7/8 over every pause. The trend's right endpoint |
| `card_clean` | whether `CRATONVM_G1_CARD_CLEAN` is armed, so a comparison between two runs cannot be mislabelled |

`card_skip_rate_now_pct` well below `card_skip_rate_first_pct` IS the
saturation, happening. `[GC] g1 card-screen trend:` repeats the three at exit
with a `decayed=` verdict. A pause that offered the screen no source regions at
all (`rset_regions_offered=0`) is excluded from the rolling rate rather than
folded in as a 0 % sample — "Phase 2 had nothing to do" is a different fact from
"the screen ran and refused every region", and averaging them together is
exactly how a decaying rate hides behind a growing pile of empty pauses.

*What the pause decided about the HEAP (2026-09-20, wave 2).* Under
`CRATONVM_G1_HEAP_RESIZE=1` the same line also carries
`heap_target_regions` / `heap_target_bytes` / `heap_floor_regions`,
`gc_overhead_ppm`, `shrink_streak`, `heap_grows` / `heap_shrinks`,
`uncommitted_bytes` and `committed_bytes`, and every actual resize is logged at
INFO as `[GC] g1 heap-resize: grow|shrink …` outside the `--verbose:gc` gate —
a heap that changed size is a fact about the process, not a GC statistic, and
an operator watching RSS move has to be able to attribute it without having
asked for per-pause logging in advance.

`heap_grows` and `heap_shrinks` are meant to be read **together**. Two large,
nearly equal counts are the hysteresis being too weak, i.e. the policy paying
the decommit and the page faults forever and converging on nothing — the one
failure mode a single pause's line cannot show.

`[GC-STAT]`'s old `CRATONVM_G1_DBG_REACH` region census is gone. Every
collection path reached the emitter holding the regions guard as a writer, so
its `try_read` always returned `None` and the six counts it was armed to print
were never once printed; the flag arm rendered a strict subset of the default
line. The same six numbers are in `phase_note` (`free_regions`, `eden_regions`,
…), taken inside the pause where the table is already borrowed. An instrument
armed where it cannot fire is worse than none, because its zero reads as a
measurement.

The first thing the completed partition showed is a cost nobody had a number
for. On `probes/G1ChurnPauseProbe 8 40` at `-Xmx256m`, ten runs, **`verify_us`
is 10.8-14.5 % of every young pause** — the budgeted post-pause
dangling-reference sweep, which runs in release builds and was previously
charged to no phase at all. It is defensible while G1-11 is open, but it is a
choice, and `CRATONVM_G1_VERIFY_BUDGET` is now a decision an operator can
actually make. (Also from those runs: the seven fields summed to `pause_us`
EXACTLY, all ten times.)

*Parallel evacuation on JIT-warm pauses (F-01), first reading.* Same probe and
heap, five interleaved reps per arm, one pause per run:

| arm | median pause | median wall |
|---|---|---|
| `CRATONVM_GC=-g1-parallel-evac-in-jit` (the old fallback) | 305 ms | 7606 ms |
| default | 268 ms | 7683 ms |

Pause −12 %, wall unchanged, and the probe's checksum was identical across all
ten runs and equal to a real JDK 25's. Read it as a direction, not a
measurement: it is a **debug build** on a host running two other agents'
compiles, so the absolute numbers mean nothing and the copy loop's share is not
the release build's. The release-build version of this is owed, together with
the young-sizing re-run above.

One asymmetry in it is real and expected: the parallel arm's fix-up walked 21-26
regions against the serial arm's 12. N workers claim N to-space regions, so more
regions are "written into" and the narrowed Phase-4 set is correspondingly
wider. The parallel closure pays for it and then some. On `probes/G1ChurnPauseProbe 96 900`
at `-Xmx2048m` a 330 ms young pause split: roots 0.7 %, remembered-set
walks 0.03 %, Cheney closure 38 %, whole-heap fix-up 10-18 %, freeing the
collection set **42 %**. That last figure is why the phase breakdown exists
at all: the reclaim phase was the most expensive part of a G1 pause and
nobody had ever looked.

It was `G1Region::reset` scrubbing every reclaimed region — 1.61 GB at
11.9 GB/s, which is memset bandwidth and nothing else — and it was
redundant with the allocator's own zeroing (`bump_alloc` zeroes exactly the
range it hands out; `alloc_humongous_locked` zeroes its whole span; no
inter-object padding can exist because every object size is a multiple of
8; no walker reads a `Free` region). Removed: single-binary A/B, medians of
3 interleaved reps, identical program checksums in both arms — p50 403 ->
265 ms, p99 412 -> 267 ms, total pause 1213 -> 790 ms, the free phase
itself 152 -> 18 ms. `CRATONVM_G1_SCRUB_FREE=1` restores it, and that is
the first thing to try if a G1 heap-corruption investigation wants the old
"a freed region reads as zeros" world back.

*The inline G1 write barrier reaches only one of the two JIT tiers.*
`CRATONVM_G1_INLINE_BARRIER=1` emits a real G1 post-write barrier inline
(null test, same-region test, out-of-line helper for anything those two cannot
dismiss) from all four reference-store emitters in the single-pass /
bytecode-walk tier. It cannot reach the IR tier at all: `ir_lower` has **no
reference-store site**, as its own `read_bounds_addr` doc states — it asks only
the read-side "is this address mapped" question — so a method the IR tier
compiles keeps the out-of-line `putfield_object` helper whatever the flag says.

Measured, not inferred. With `RUST_LOG=cratonvm_jit=info` the emitter logs
`jit: G1 inline post-write barrier ACTIVE` once per process: it appears on
`apps/g1_probe/G1CardChurn` with the flag on and never with it off, and never on
`probes/G1ChurnPauseProbe` in either arm. So the workload that exhibits the
barrier and the workload that exhibits pause behaviour are different ones, which
is why the flag ships opt-in with no pause-level number — a `jit/` gap, not a
`gc/` one.

Watch the log filter when checking this: a bare `RUST_LOG=info` shows nothing,
because the launcher builds its filter as
`from_default_env().add_directive(WARN)` and a global WARN ties with a global
`info` on specificity, resolving last-added-wins. Use the target-scoped form.

*Free-region search: measured, and still linear.* Both searches
(`find_free_region_from`, `find_contiguous_free`) are O(regions), and the
region count used to grow with `-Xmx`. `[GC] g1 free-scan:` reports calls,
regions probed and the worst single scan for each. Two readings:

| workload | single | contiguous |
|---|---|---|
| young churn, 2048 regions | 1543 calls, 1543 probed, **worst 1** | never called |
| 400 humongous allocations, 256 regions | 15 calls, 1027 probed, worst 195 | 400 calls, 35943 probed, **worst 227** |

The ordinary path is free — the rotating hint answers in one probe, always. The
humongous path scans most of the heap per call, and that is still 36,000
comparisons of an enum against a constant across a whole run, on a path that
then memsets megabytes. A hint for it was tried and measured at 0.9% (35,943 →
35,617 probes) and dropped: the free-scan cursor tracks single-region Eden
claims and has no relationship to where a humongous span was freed.

What bounds it is the region-size ergonomic above: ~2048 regions at any heap
size means the scan is bounded by a constant rather than by `-Xmx`, which is the
property the concern was actually about. A free-region bitmap would buy those
comparisons at the price of a second source of truth for "is this region Free" —
read in ~200 places, written in 8 — and one that says Free about a live region
hands the allocator memory that is in use. Refused on the number; the instrument
stays so it can be revisited against a workload.

*The Phase-4 walk, narrowed.* A young pause's reference fix-up walks only the
collection set's remembered-set sources plus every region the pause WROTE
INTO — not every object of every non-CSet region, which would make pause
time O(live heap) rather than O(young live set). `CRATONVM_G1_NARROW_FIXUP=0`
restores the whole-heap walk, and is the first lever to pull for any
suspected G1 dangling-reference or lost-edge defect.

Why that set is sufficient: a slot needing a forwarding rewrite points at an
evacuated object, so it lives in a root (Phase 1 rewrites those), in the CSet
(Phase 3 scans every to-space copy), or in a non-CSet region reachable only
through the remembered set (Phase 2 walks exactly those). The remembered set
is complete because every mutator reference store reaches
`post_write_barrier_rset` — the interpreter's and every native's directly,
and every JIT-compiled one through `jit_putfield_object` since G1-2 closed.
The walk's other job, the GC-internal edge rebuild, only concerns regions the
pause wrote into, and those are found by diffing a pre-evacuation
`(region_type, cursor)` snapshot rather than by asking the evacuator — so no
allocation path, present or future, can forget to register itself. The
humongous census still forces the wide walk, because "nothing in the heap
references this span" is a whole-heap claim.

Scope: all four evacuation drivers — serial young, mixed, and both parallel
arms — each pass a narrowed set to `update_references_in_regions`. (This
paragraph used to say "the SERIAL young pause; mixed pauses and the parallel
evacuator still walk wide". That was true when the narrowing landed and has
not been since the other three drivers adopted it; a reader sizing a mixed
pause from it would have budgeted for a whole-heap fix-up that no longer
happens.) What genuinely still walks wide is the evacuation-**failure**
drain, `drain_kept_self_forwards`, which passes `None` — and the humongous
census, for the reason given above.

Measured (single binary, one env flag, interleaved, `G1ChurnPauseProbe`): on
a workload whose live set stays YOUNG the narrow set equals the wide one and
nothing changes — `fixup_regions` is 116 in both arms. On one whose live set
has settled into Old (`-Xmx512m`, 48 MiB live, 12 GiB of garbage) the walk
drops from **60 regions / 58 MiB to 1 region / 0 MiB**, p50 27 -> 17 ms,
total pause -12%, p99 unchanged, checksums identical. The size of the win is
the ratio of settled old generation to young — which is the shape the
original criticism was always about.

Verification, and its limits. The unit suite CANNOT discriminate this change:
every pre-existing test passes even when Phase 4 walks nothing, because on
every constructible fixture the mutator barrier alone already records every
edge. The tests that do discriminate check the narrow SET's composition. The
consequence is checked at runtime instead — `CRATONVM_G1_DBG_RSET=1` verifies
after each pause that every cross-region edge into a collectable region is
named in that region's remembered set, and prints `edges=N missing=M` so a
green result cannot hide a vacuous one. On the shape that genuinely skips 59
of 60 regions it reports `edges=2114 missing=0`, and a unit test proves that
checker can fail (clear every remembered set and all 2114 are reported
missing). What is still owed is a suite-scale soak.

**ZgcRealHeap.** One arena + free list (post-sweep coalesced) + hash-set
registry of allocation bases. `needs_gc` triggers at 75 % occupancy with
a post-sweep re-arm so a large live set cannot storm. That clause makes pause
work scale with the heap FLAG rather than with the garbage: at `-Xmx2g` with
50 MiB live it fires when `allocated` reaches 1.5 GiB, so every cycle lets
~1.45 GiB accumulate and the registry, the mark bitmap and the sweep all cover
the span the bump cursor ran over — doubling `-Xmx` doubles every pause on a
workload whose live set did not change.

`CRATONVM_ZGC_ALLOC_TRIGGER=<percent>` (added 2026-09-03; **default 0, i.e.
off, with or without a pause target**. This line read "0 without a pause target,
25 with one" until 2026-09-20, describing the implicit floor that was made the
default on 2026-09-03 and **withdrawn on 2026-09-04** — see the pairing below
for the `TestLargeBlob` crash that withdrew it. `alloc_trigger_percent_for`
ignores the pause target and returns the operator's percent or zero.) adds a
second clause that
collects once that percent of capacity has been allocated since the last
cycle, capping the span a pause walks at `budget + live`. It is a pause-versus-throughput DIAL, measured on
`G1ChurnPauseProbe 50 600` at `-Xmx2048m` (release, three runs a row, one
binary, only this switch moved):

| percent | wall ms | cycles/run | mean pause | max pause | registered at the worst pause |
|---|---|---|---|---|---|
| 0 (off) | 2891 | 2 | 116 ms | **202 ms** | 10,597,520 |
| 50 | 3234 (+12 %) | 3 | 113 ms | 174 ms | 7,485,260 |
| 25 | 3463 (+20 %) | 6 | 80 ms | 122 ms | 3,953,214 |
| 12 | 3767 (+30 %) | 12 | 57 ms | **79 ms** | 2,116,551 |

The worst pause falls 2.6× for a 30 % wall cost, and the cost is not an
artefact — collecting six times as often pays the live-set-proportional half
of a cycle (the mark, the registry snapshot) six times as often. It is off by
default for that reason: a percentage of capacity says nothing about how long
the resulting pause will be, so it is a dial the operator has to tune per
workload. The pause-target form below is what replaced it. `[GC] zgc-pause:` prints
`alloc_trigger=<fires>/<budget bytes>` as its engagement counter. The
regression suite is 88/88 both with the clause off (the shipped default) and
with `CRATONVM_ZGC_ALLOC_TRIGGER=12`, so the switch is safe to turn on — what
it has NOT had is suite time on the larger corpora, which is what a default
change would need.

`-XX:MaxGCPauseMillis=<n>` (or `CRATONVM_ZGC_PAUSE_TARGET_MS`, **default 200**,
added 2026-09-03) is the form that ships on. The flag reached only G1 before
that date, so on the DEFAULT collector an operator who asked for a pause target
got no answer and no diagnostic. It is a CEILING, not a setpoint: the clause
starts unconstrained and engages only once a pause has actually overrun the
target, then scales the span it will allow by `target / pause` — multiplicative,
so it never has to model the pause cost curve, whose fixed part is large
(~34 ms here). It tightens on an overrun, holds inside `[0.75, 1.0] × target`,
and relaxes a quarter at a time but never back past ⅞ of the span that last
overran. On a workload whose pauses never reach the target it never engages at
all, which is what makes a non-zero default defensible where the percentage form
had to ship off. `refresh_pause_target_budget` records the control law, the
three earlier and wrong versions of it, and what each one measured.

Measured on `G1ChurnPauseProbe 50 1800` at `-Xmx2048m`, whose unconstrained
worst pause is 244 ms (release, three interleaved reps, one binary, only the
target moved). "p50 after engagement" excludes the overruns the controller had
to *observe* in order to react — no feedback loop can prevent those:

| target | wall ms | cycles/run | p50 after engagement | worst |
|---|---|---|---|---|
| off | 8773 | 6 | — | 244 ms |
| **200 ms (default)** | 9234 (+5.3 %) | 6.3 | 193 ms | 210 ms |
| 100 ms | 11352 (+29 %) | 24 | **68 ms** | 196 ms |
| 80 ms | 10210 (+16 %) | 26 | **57 ms** | 143 ms |
| `ALLOC_TRIGGER=12` | 10525 (+20 %) | 36 | — | 93 ms |

**Why the unit had to change.** Doubling `-Xmx` on a workload whose live set did
not move nearly doubles the worst pause — and the *percentage* form doubles with
it, because 12 % of a bigger heap is a bigger budget. Only a target holds:

| `-Xmx` | off | `ALLOC_TRIGGER=12` | target 100 ms |
|---|---|---|---|
| 2048m | 313 ms worst | 85 ms worst, 246 MiB budget | p50 73.5 ms |
| 4096m | 595 ms worst | **249 ms** worst, 492 MiB budget | p50 79.7 ms |

**What it does NOT control, and why.** It holds the MEDIAN; the tail stays high.
The pause floor on this collector is the arena's HIGH-WATER MARK, not the live
set: the bitmap sweep covers `[base, low_cursor)` and the cursor does not
retract, so once any cycle has run the bump cursor out to 1.3 GiB, every later
pause pays a scan over that span whatever the allocation budget is. No
allocation trigger can undo that — only compaction and a cursor retraction can
(`CRATONVM_ZGC_RELOCATE`, `Arena::retract_cursor_to`). That is why a 100 ms
target is reachable at `-Xmx2048m` on this probe and not at `-Xmx4096m`, and
why the loop gives up after three unreachable verdicts rather than paying one
unconstrained cycle per retry (measured: 56 cycles and +101 % wall for a p50 of
103.8 ms — worse on both axes than never trying).

`[GC] zgc-pause:` prints `alloc_trigger=<fires>/<budget bytes>` and
`pause_target=<ms>/<affordable span>/unreachable=<n>` as the engagement
counters. A climbing `unreachable` says the target is not achievable at this
live set, which is a different answer from "the loop is holding the target" and
produces an identical budget without it.
The sweep prunes dead bases in place and feeds the exact dead list to the
monitor registry. Reference semantics come entirely from the VM-level
protocol. *(This paragraph used to end "Non-moving ⇒ the pointer map is
always empty and no barriers are needed". Neither half holds on a default
run: the sliding compactor returns a non-empty pointer map, see the last
paragraph of this section, and marking publishes through the SATB
pre-write barrier, see the backend table at the top.)*

**The per-cycle bitmap passes are bounded by the arena's bumped ends**
(`CRATONVM_ZGC_BITMAP_BOUNDS`, default on, added 2026-09-03). The object-start
registry and the mark bits are sized by CAPACITY — one bit per 8 arena bytes,
so `-Xmx / 512` bytes of words — and the snapshot the mark phase takes was
copying all of it every collection: 8.4 million atomic loads into a fresh
64 MiB `Vec` at `-Xmx4g`, paid whether the heap holds ten objects or ten
million. That is a pause floor proportional to the heap FLAG, and it is what
made a 100 ms pause target reachable at 2 GiB and unreachable at 4 GiB.

`Arena` is two-ended, so the span between the low bump cursor and the
large-object end has never been handed out: no allocation starts there, no bit
in it is set, and copying it transfers zeroes. The snapshot and the mark-bit
clear now visit only the two ends. Measured on `G1ChurnPauseProbe 50 1800` at
`-Xmx4096m` with a 100 ms pause target, one binary and this switch the only
variable, at matched cycle counts:

| | snapshot | mark | sweep | pause p50 |
|---|---|---|---|---|
| whole capacity | 18.8 ms | 13.8 ms | 48.5 ms | 82.5 ms |
| bounded | 13.7 ms | 8.2 ms | 43.2 ms | **66.4 ms** |

**This is also what pays for the cursor retraction.**
`Arena::retract_cursor_to` has lowered the cursor onto the last survivor after
every sweep since 2026-09-02, but with capacity-sized bitmap passes that only
helped the *allocator* find contiguous space — the pause work was the same
either way. Bounded, retraction shrinks the pause: the tail of garbage a burst
allocated is handed back and the next cycle's snapshot, clear and complement
sweep all stop at the new cursor.

The bound is the HIGH-WATER mark and not the cursor, which is a correctness
requirement rather than a refinement: retraction lowers the cursor *after* the
sweep, and the mark bitmap has bits above the new cursor set earlier in the
same cycle. Clearing bounded by the cursor would leave them, and the next cycle
would read a mark set carrying a previous cycle's bits and retain whatever they
name. `Arena::low_high_water` is `cursor.max(pre_retract_high)`, reset once per
collection after both bitmaps are clear.
`ZObjectStartBits::debug_assert_clear_outside` verifies the invariant in debug
builds — it walks the skipped words and asserts every one is zero — and it is
what turned that ordering bug into a failing test on the first run.

**Pair a pause target with a percentage floor.** The target cannot bound the
FIRST cycle: it has nothing to measure until a pause has happened, so the
occupancy clause lets that one run to 75 % of `-Xmx`, and on this probe it is
the worst pause of the run by a factor of five. Setting
`CRATONVM_ZGC_ALLOC_TRIGGER` as well caps it, and the target then controls the
steady state. Measured at `-Xmx4096m`:

| configuration | wall | worst pause |
|---|---|---|
| neither | 9091 ms | 466 ms |
| target 100 ms alone | 12943 ms | 509 ms |
| **target 100 ms + `ALLOC_TRIGGER=25`** | 11180 ms | **190 ms** |
| target 200 ms alone | 14849 ms | 530 ms |
| **target 200 ms + `ALLOC_TRIGGER=25`** | 13548 ms | **221 ms** |

The pairing is better than the target alone on BOTH axes: the floor stops the
one cycle the controller is blind to, and paying for that cycle up front costs
less than the controller's recovery from it. **It was made the default on 2026-09-03 and WITHDRAWN on 2026-09-04**, the day after: arming the floor implicitly changed how often the collector runs on every ZGC workload, and `org.h2.test.db.TestLargeBlob` went from 0 collections and a PASS to 34 collections and a SIGSEGV inside `FileChannelImpl.implWrite` -> `IOUtil.write` -> `DirectByteBuffer` (3 crashes in 4 runs with the floor on, 0 in 3 with it off, same binary). The crash is almost certainly older than the flag — without the floor that test never collects at all, so nothing exercised the path — but a default that turns a passing test into a native crash does not ship while that bug is open. Pair them explicitly with `CRATONVM_ZGC_ALLOC_TRIGGER=<percent>` if you want it. `CRATONVM_ZGC_ALLOC_TRIGGER=0` is an explicit refusal
rather than an absence, and is how the "target alone" arm is measured; any
other explicit value wins over the floor in both directions.

The shipped default measured against what it replaced, and against no trigger
at all — same probe, one binary, switches only:

| `-Xmx` | configuration | wall | pause p50 | worst |
|---|---|---|---|---|
| 2048m | no trigger | 11323 ms | 227.8 ms | 276.8 ms |
| 2048m | target 200 alone (the old default) | 11184 ms | 199.4 ms | 242.1 ms |
| 2048m | **target 200 + floor 25 (shipped 2026-09-03, WITHDRAWN 2026-09-04)** | 11234 ms | **92.7 ms** | **116.0 ms** |
| 4096m | no trigger | 11129 ms | 421.4 ms | 565.6 ms |
| 4096m | target 200 alone (the old default) | 12449 ms | 147.2 ms | 654.7 ms |
| 4096m | **target 200 + floor 25 (shipped 2026-09-03, WITHDRAWN 2026-09-04)** | 11485 ms | 182.6 ms | **227.1 ms** |

At 2048m it more than halves both the median and the worst pause for no wall
cost at all; at 4096m it cuts the worst pause by 60 % against no trigger and by
65 % against the target alone, and costs 3 % of wall against no trigger while
being 8 % FASTER than the old default. The old default was the worse of the
three on the tail at both sizes — the controller was paying for a first cycle
it could not see and then recovering from it, which is precisely what the floor
removes. **None of that is the default any more**: the floor was withdrawn the
day after it shipped (see above), so reproducing these rows needs
`CRATONVM_ZGC_ALLOC_TRIGGER=25` named explicitly alongside the target.

### ZGC: what the zs round changed (2026-10-14 → 2026-10-16)

Ten waves (s1-s10) on the zr verification rule (HotSpot 25 `-XX:+UseZGC`, both modes, positive and
kill-switch controls, <= 1.05x CPU on every interleaved row for a default flip). The round branch is
closed but **not yet merged into dev**; handoff, the full list of default-ON keys per wave, the
unmeasured wave-s9 keys and the dev-merge overlaps:
[`internal/gc/zgc-zs-handoff-round-close-SUPERSEDED-20261019.md`](internal/gc/zgc-zs-handoff-round-close-SUPERSEDED-20261019.md).
Host verdicts per build: [`internal/zgc-round-zs-20261014/orch-notes.md`](internal/zgc-round-zs-20261014/orch-notes.md).

In short: OOME doors and define paths (nd..nd8: mirror reserved before every define, launcher and boot
OOME shapes, FFM upcall containment, proxy modules per loader), observability (ob..ob6: HotSpot pool
init/peaks, Metaspace pool and thresholds, thread-flag refusals), JFR GC events (jf, jf2), compiled
allocation and store gates (ca..ca5, bt, bt2: fused reference-store gate on every site, TLAB align
test; varlen allocation at HotSpot parity on the host), slide/relocation set fixes (sl, sl2), boot RSS
(rs: jimage package index, lazy zero tables). Kept opt-in after measurement: `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`
(breaks launcher OOME stderr parity), `CRATONVM_ZGC_STALL_CLEARS_SOFT`, `CRATONVM_ZGC_NEAR_FULL_FLOOR_FITS`,
`CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE`.

### ZGC: what the zr round changed (2026-10-12 → 2026-10-13)

The ZGC design and performance round (tag `zr`, 11 waves z1-z11, 50+ lanes, every change verified on
Linux against HotSpot 25 `-XX:+UseZGC` in both `--jdk-only` and `--compatible`). Authoritative
record: [`internal/zgc-round-zr-20261012/orch-notes.md`](internal/zgc-round-zr-20261012/orch-notes.md)
(host verdicts per build) and the `z*-<lane>-report.md` lane reports beside it. Open items:
[`internal/gc/zgc-zr-handoff-round-close-SUPERSEDED-20261019.md`](internal/gc/zgc-zr-handoff-round-close-SUPERSEDED-20261019.md).
Every behaviour change below has a `=0` kill switch; rows are in [`gc-tuning.md`](gc-tuning.md).

**Defaults flipped ON** (proposal triage t1 and the waves):
`CRATONVM_STW_LOCKFREE_BLOCKED`, `CRATONVM_GC_HOTSPOT_INITIAL_HEAP` (t1);
`CRATONVM_ZGC_PARMARK` unset = `auto` (z3/mt; pause sums 2.5x lower, CPU +15-20 %);
`CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT=2` (z4/cp; tight-heap evacuation, retained >= HotSpot on every
capacity shape, ends the old selector's near-OOM crawl: grow 9 s vs 167-190 s CPU).
**Kept opt-in after measurement:** `CRATONVM_ZGC_CONC_START` / `CRATONVM_ZGC_CONC_EVERY_TRIGGER`
(pause sum -80 % but CPU +16-17 %: the pause-budget knob), `CRATONVM_ZGC_OVERHEAD_BACKSTOP`
(an overhead verdict HotSpot's ZGC does not have; UseGCOverheadLimit is Parallel-only),
`CRATONVM_ZGC_RATE_TRIGGER`, `CRATONVM_ZGC_REFILL_CAPACITY_GATE`, `CRATONVM_ZGC_MARK_PREFETCH`.

**Correctness fixed on the default path** (each with a host row and a positive control):
huge-array use-after-free seams (rl, rv, rv3: derived pointers into arrays past 1<<28 elements);
a failed parallel mark falling back to a serial marker that skipped below the claimed roots (rv4,
`CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN`); soft references not cleared before `OutOfMemoryError` on
ZGC (a pre-existing dev defect: conservative JIT words kept the referent; cp + rv5 precise-first
verdict); JIT boxing helpers and VM-internal allocations aborting the process instead of throwing
OOME (lt, ab, `Object.getClass` lazy mirror retry); the generational mode's floor-bounded young
cycle freeing children of dead registered parents (g2, rv3); mark workers holding the heap past its
last reference (cc); per-heap switch/counter state instead of process latches (lt, ab, rv4).

**HotSpot parity on the default path:** MXBean manager and notification order, young pool
since-cycle used and committed (pools sum to the heap), GC causes (`Warmup`, `Allocation Rate`,
`Allocation Stall` classified by the forced-collection site), `-XX:ParallelGCThreads` /
`ConcGCThreads` / `UseDynamicNumberOfGCThreads` honoured, `MaxGCPauseMillis` no longer paces ZGC,
`gc+init` backing/worker lines, uncommit down to MinHeapSize with a per-heap uncommitter thread
(`ZUncommitDelay`, `-XX:-ZUncommit`, `Uncommitter (0) Uncommitted:` log line), ZGC JFR events
(phase pauses, relocation set, `ZOldGarbageCollection`, `ZAllocationStall`, `ZUncommit`).

**Performance on the default path:** inline TLAB bump for compiled `Object[]` (-21 % per allocation),
mark-loop cuts (layout memo, ref displacements, test-first: -15..25 % mark time), idle-spill for the
parallel mark pool, untimed idle park of mark workers, live-proportional sweep (-10 % allocation
cost), sparse bitmap clear, forwarding/scratch trims, slide buffer trim, raw zeroing outside the
arena lock, fused slide screen (screen+rewrite 2602 -> 1664 us).

**Open (see the handoff):** a whole-heap STW collection costs ~60 ms CPU at ~229 MB live against
HotSpot's ~9 ms (near-OOM CPU 7-9x HotSpot at collection parity); ProxyBoxing near-full trigger
pacing (156 collections vs 38); compiled small-array allocation ~5x HotSpot per allocation; boot
RSS 207 MB vs 42 MB (non-GC); generational mode still costs more CPU than the default.

#### Waves z7-z11 (2026-10-13, the round's second half)

Five more waves (z7 rv6 ca pc sp cm rs; z8 rv7 st fm nr zb; z9 rv8 sl bo hp; z10 mt; z11 rv9 ia nd md),
host builds `cvm-z7a` … `cvm-zf`, the same verification rule (HotSpot 25 `-XX:+UseZGC`, both modes,
positive and kill-switch controls, interleaved CPU A/B for every performance claim; a default flip
needs ≤ 1.05x CPU on every row). The final candidate `zf` keeps every z6a census verdict.

**New defaults (each `=0` kills):**
- *Collection CPU:* `CRATONVM_ZGC_RELOCATE_SIZE_TABLE` + `CRATONVM_ZGC_SWEEP_SIZE_HINTS` (pc: -15 % CPU
  per collection round on `Zr7PcCollectionCpu prim`); `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` (zb: the
  per-dead-object header store gone, prim -16 %, fixed8 -11 %, fixed0 -17 % ns/op).
- *Compiled allocation:* `CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW`, `..._SKIP_GRID_TEST`,
  `..._COMMIT_FROM_RAX` (ia: the inline start-bit announce + commit 44 -> 30 instructions; prim -14 %,
  `Zr3JaRefArrayAlloc` -15 % ns/op; HotSpot is still ~4x faster per small array);
  `CRATONVM_JIT_ZGC_INLINE_ARM_SKIPS_REPUBLISH` (ca, value-identical, no measurable gain).
- *Footprint:* `CRATONVM_ZGC_SIDE_TABLE_NO_THP` (mt: the ZGC bitmaps on their own no-huge-page mapping,
  -6..8 MB boot / class-loading RSS at equal CPU); `CRATONVM_CLASS_CODE_COMPACT` +
  `CRATONVM_BOOT_CLASS_BYTES_REFETCH` (md: 23.8 -> 16.4 KB allocator bytes per java.base class, -20 MB
  RSS at 3000 classes, equal CPU; also fixes evicted classes losing `getAnnotatedSuperclass` /
  retransform / CDS bytes); fm's boot fixes (`CRATONVM_JIMAGE_DIRECT_MAP`, `CRATONVM_JMOD_LAZY_INDEX`:
  the jrt walk at `-Xmx64m` now matches HotSpot, RSS 300 vs 440 MB).
- *OOME parity (HotSpot never throws a heap OOME without a clear-soft collection first):*
  `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` (every collector since gci i48/zl2, `ebfde29798`; the name is
  kept), `CRATONVM_ZGC_NATIVE_ROOM_SKIP_REPAY` (nr), and
  `CRATONVM_ZGC_NATIVE_OOM_REPLAY` (nd: the native funnel collects and re-runs 16 audited restartable
  natives instead of converting each; closed the jdk-only `dumpThreads` door); also fixed: recursion
  depth counters in `getDeclaredMethods`/`getMethods`/`isAssignableFrom` leaked one level per OOME
  (after 51 every `getDeclaredMethods()` returned an empty array), and `dumpThreads` left its marker set.
  `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_ROOT_VALUES` (sp) narrows the conservative soft-clear trade.
- *Observability:* `CRATONVM_VERSION_XLOG_BOOT` (`-Xlog:gc -version` prints HotSpot's lines),
  `CRATONVM_ZGC_EXIT_HEAP_SUMMARY` (`gc+exit` block), `CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET` (no
  `GCPhasePauseLevel1` rows, as HotSpot) (hp).
- *Concurrent-mode soundness / pacing (the concurrent path itself stays opt-in):* cm's window fixes
  (`CRATONVM_ZGC_CONC_RATE_FROM_ALLOCATION`, `CRATONVM_ZGC_CONC_WINDOW_EXTRAPOLATE`,
  `CRATONVM_ZGC_SATB_FILTER_MARKED=2`, `CRATONVM_ZGC_MARK_ROOT_FILTER_CONC`) and rv9's
  `CRATONVM_ZGC_CONC_POOL_WEAK_HEAP` (the pool no longer pins a dropped heap),
  `CRATONVM_ZGC_CONC_RATE_EXCLUDE_PAUSE`, `CRATONVM_ZGC_CONC_START_ZERO_KILLS`,
  `CRATONVM_ZGC_MARK_SPAWN_BACKOFF`, `CRATONVM_ZGC_SELECTIVE_BYPASS_ON_LATCH`.

**Measured and kept opt-in (or reverted):** `CRATONVM_ZGC_CONC_EVERY_TRIGGER` (flipped at z8, reverted:
CPU 1.11x; the slide, not the mark, dominates the pause); `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` (default
at z7, reverted at z10: -80 MB RSS but +7..20 % CPU from 4 KiB first-touch faults);
`CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP` (-14 MB, under the 30 MB bar); `CRATONVM_ZGC_SELECTIVE_RELOCATE`
(sl: -11 % on the collection-cost row, +12 % on `MtChurnProbe`); `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE` (bo:
-15 % before ia, no gain on top of ia); `CRATONVM_ZGC_LAZY_TLAB_STARTS` (st: the query census refutes
the lazy-starts premise, 1000-2042 headers per query).

**Refuted by measurement:** a card / block-offset start index (bo: 5-6 oracle queries per allocation
would cost ~1.2 s to save ~0.007 s); the chunk-zeroing frontier (shipped once as
jit round 9's lazy TLAB zeroing switch and removed); "the slide copies the whole live set every cycle" (sl: the
`bytes_copied=` field is the sweep's survivor total).

**Still open:** the handoff page lists them (collection CPU vs HotSpot, compiled allocation ~4x,
boot RSS, per-class metadata residuals, the native doors outside the funnel).

### ZGC switches added in the 2026-09-20 round

Full table with rationale in
[`docs/gc-tuning.md`](gc-tuning.md#zgc-flags-added-or-renamed-in-the-2026-09-20-round).
In brief:

- **`CRATONVM_ZGC_VM_TLAB_SHARE`** (default **on**) — counts the VM-thread
  buffer against the TLAB reservation share. See the TLAB paragraph below for
  the regression it closes.
- **`CRATONVM_ZGC_HEADROOM_BYPASSES_REARM`** (default **off**) — lets the
  `headroom_low` clause of `needs_gc` fire below the `gc_rearm` floor, but only
  while the last cycle reclaimed something. `gc_rearm` is sized from **live
  bytes** and `headroom_low` is about **allocatable space**; on a heap whose
  cycles may decline to compact those come apart without bound, and the shared
  floor can silence the headroom clause exactly on the heap it was written for
  (large `-Xmx`, small live set, fragmented free list). The failure is not a
  slow run — the infallible `alloc_object` aborts rather than raising
  `OutOfMemoryError`, so "never collects" is a crash with most of the heap free.
- **`CRATONVM_ZGC_CENSUS`** / **`CRATONVM_ZGC_CENSUS_ACCESS`** (default
  **off**) — turn on `ZSlotCensus`, the sweep-time reference-slot walk. Before
  this, `ZSlotCensus::enable()` had no caller outside its own tests, so the
  instrument built to measure the legacy-slot share had never run in any
  process. Read `legacy_share` off the **`last_walk`** block, not the cumulative
  one, which is lifetime-weighted and reads high on legacy.
- **`CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE`** (default **off**) — feed
  the pause-target controller the pause *including* the slide, which
  `collect_garbage` read before the compaction block. That controller sizes
  `alloc_trigger_bytes`, a clause of `needs_gc`, so it decides how often the
  collector runs; the pre-slide reading was about half the real pause on
  `ZgcTlabStress 8 200`.
- **`CRATONVM_ZGC_METRICS`** (default **off**) — also print `ZgcMetrics`'s
  OpenJDK-shaped per-cycle line and its run summary at heap teardown.
  *Recording* is unconditional now; only the rendering is opt-in, because the
  format differs from `[GC] zgc-real:` and the harnesses key on that.
- **`CRATONVM_ZGC_TRIGGER_SHADOW`** (default **off**) — one
  `[GC] zgc-trigger-shadow:` line per collection showing what an
  allocation-rate-driven trigger would have decided. Stage 1 of
  `docs/internal/zgc-round-20260920/proposal-e-allocation-rate-driven-trigger.md`, i.e. the measurement that
  decides whether the rest is worth building.

All seven are declared in `types/src/flag_groups.rs` (`:3461`–`:3467` on
2026-09-26, tokens `zgc-vm-tlab-share` .. `zgc-trigger-shadow`), so each also
has a `CRATONVM_GC=<token>` spelling (`docs/flag-tokens.md`) and a row in
`docs/config/flag-inventory.md`. *(This paragraph said the opposite — "not yet
in `types/src/flag_groups.rs`" — until 2026-09-21; the registration landed in
wave 2 and the rendering was corrected in wave 3, `9a03106ff`.)* Three further
`CRATONVM_ZGC_*` keys the round added or made operator-relevant are documented
in `docs/gc-tuning.md` rather than here: `CRATONVM_ZGC_MARK_REF_CHUNK`
(numeric, default 4096, live on every configuration),
`CRATONVM_ZGC_TLAB_RESERVED_BYTES` (default off, counter readable anyway) and
`CRATONVM_ZGC_PAGE_EVAC` (default off, and switches nothing today).

*zr z1/tr (2026-10-12) added three more, rows in `docs/gc-tuning.md`:*
`CRATONVM_ZGC_RATE_TRIGGER` (default **off**; HotSpot ZGC's director rules --
allocation rate, proactive, high usage -- decide, and the 75% live-bytes
clause stands down; stage 2 of the trigger proposal),
`CRATONVM_ZGC_HOTSPOT_GC_CAUSE` (default **on**, `=0` kill; allocation-driven
ZGC collections are reported as HotSpot ZGC's `Warmup` / `Allocation Rate` /
`Proactive` / `High Usage` / `Allocation Stall` instead of `Allocation
Failure`), and the HotSpot flag `-XX:SoftMaxHeapSize` (ZGC only). The
`gc,init` block now also states `Uncommit:` / `Uncommit Delay:` (this VM's
real policy) and the applied `Soft Max Capacity:`.

*zr z2/t2 (2026-10-12) added five, rows in `docs/gc-tuning.md`:*
`CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER` (default **on**; the compiled refill
trigger's occupancy cycles carry their rule's cause instead of `Allocation
Stall`), `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE` (default **on**; young pool =
allocated since the last cycle, old pool = survivors with `committed == used`,
young committed = heap committed less old's; the pools sum to the heap),
`CRATONVM_ZGC_HOTSPOT_BEAN_ORDER` (default **on**; HotSpot's pool-manager
order and pause-before-cycle notifications), `CRATONVM_ZGC_INIT_BACKING_LINE`
(default **on**; the `gc,init` backing-filesystem line) and
`CRATONVM_ZGC_XLOG_ALLOCATION_STALL` (default **off**). Mutator allocation
stalls are now recorded on `ZgcMetrics`.

Two renames, from when the sweep optimisations stopped being scoped to young
cycles: `CRATONVM_ZGC_GEN_HEADER_ZERO` → `CRATONVM_ZGC_SWEEP_HEADER_ZERO` and
`CRATONVM_ZGC_GEN_DEAD_RUNS` → `CRATONVM_ZGC_SWEEP_DEAD_RUNS`. The `GEN_` names
are read by nothing and set silently to nothing.

**Mutators have TLABs on this backend, and since 2026-09-02 the JIT's inline
allocator can have one too -- default-on since 2026-09-18 (JIT review round 9;
`CRATONVM_ZGC_JIT_TLAB=0` turns it off).** `VmHeap::refill_tlab` on the `Zgc` arm
hands the VM thread's own `Tlab` a zeroed chunk from the low arena
(`gc/src/zgc/vm_tlab.rs`) unless `CRATONVM_ZGC_JIT_TLAB=0`, so the interpreter's
`new` and the compiled inline bump both hit; each object is registered in the start bitmap
the moment its header is complete (`VmHeap::note_tlab_object`), the unused tail
goes back to the arena free list when the thread retires the buffer
(`Tlab::retire` → `TlabTailSink`), and a tail the STW protocol publishes for a
blocked or frozen peer pins its pages against the slide. Before that date the
arm returned `None`, every compiled allocation took the `jit_new_object` helper,
and the former VM-wide TLAB-hit metric was zero here by construction. Below the VM buffer,
`ZgcRealHeap::alloc_raw_tlab` (over `gc/src/zgc/arena_tlab.rs`) remains the
funnel for the helper path and every native-side allocation; it is **on by
default**, and `CRATONVM_ZGC_TLAB=0` is its kill switch (which also switches the
VM buffer off, since both carve from the same source). **The VM buffer was
opt-in, on a measurement that said it was slower; that reversed on 2026-09-18
and it is default-on** — see `zgc_vm_tlab_enabled_by_default` and
`ZGC_VM_TLAB_DEFAULT_ON` for the numbers (bintrees ~6x faster than the helper
path) and for the register-only helper the win needed. This paragraph carried
both statements at once until 2026-09-20.

A TLAB chunk is *reserved* space that
no collection can reclaim while its owning thread lives, so it is invisible to
any trigger that counts live bytes; the reservation budget is bounded by the
buffer count (`ZGC_TLAB_RESERVATION_SHARE`, a sixteenth of the arena divided by
the buffers claiming one and clamped up to `ZGC_TLAB_MAX_CHUNK`) for exactly
that reason. **That divisor counted only `zgc::arena_tlab`'s own cells until
2026-09-20**, and a thread allocating through the VM buffer registers no such
cell — so from the 2026-09-18 default flip until then, essentially all
allocation ran through a path the reservation budget could not see and every
thread was sized as if it were alone. `CRATONVM_ZGC_VM_TLAB_SHARE=0` restores
the old arithmetic; the failure it re-opened is Tomcat's `TestNonBlockingAPI` at
`-Xmx2g`, ~4 000 threads x 512 KiB against a 2 GB heap.

The arena is **two-ended**: small objects and TLAB chunks bump up from offset 0,
allocations at or above `ZGC_LARGE_OBJECT_MIN` (64 KiB — the size no TLAB will
ever serve) bump *down* from capacity with their own free list, and a reserve
(`capacity / 8`) keeps the low end from consuming the whole large-object end.

**The pointer map is no longer always empty, and this paragraph said it was
until 2026-09-20 — **five weeks** after the slide became the default.** *(This
sentence said "three months" until 2026-09-21, contradicting its own next
paragraph four lines down, which gives the correct date. Three pages carried
that same wrong interval; all three are fixed.)* It read:
*"The always-empty pointer map and the neutral `VmHeap::Zgc` arms that go with
it are correct only while the collector is non-moving … which must land before
any compaction does."* Compaction has been default-on since 2026-08-13, so a
reader taking that at face value would conclude a stale-reference crash could
not be the slide.

What is true is the narrower statement. A default cycle returns a **non-empty**
`PointerMap`, and every consumer of one therefore runs for this collector: JIT
frame maps, monitor tables, external root providers and native side tables.
Those consumers are collector-agnostic and already ran for the generational
moving-young path, and the two `VmHeap::Zgc` predicates that take a pre-GC
address were audited and tested (R6) — but "already runs for another collector"
is not "has run for this one". **A crash, a stale-reference warning or a
silently-wrong result that disappears under `CRATONVM_ZGC_RELOCATE=0` is that
class of bug, and the flag is the bisect.** The remaining hardening is Phase 4
of
[`docs/feature-designs/zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md).

### ZGC allocation and sweep: what zr z1/al changed (2026-10-12)

- **`CRATONVM_ZGC_RAW_ZERO_OUTSIDE_LOCK`** (default **on**, `=0` kill) --
  `alloc_raw` (every object a TLAB will not take, every large array) zeroes a
  reused span after releasing the arena lock, as `carve_tlab_chunk` always
  has; it used to `memset` under the lock every TLAB refill needs.
- **`CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS`** (default **on**, `=0` kill)
  -- a dead array over the 256 MiB `alloc_size` screen on the large-object end
  is sized from its header after extent checks (inside the large-object end,
  no other base inside it, no free block overlapping it) and reclaimed. It
  used to be retained forever, so allocating and dropping one per iteration
  ran out of heap where HotSpot ZGC does not. The per-object sweep frees
  such an array too (`CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE`), and a LIVE one
  is sized as an immovable survivor (`CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS`);
  see `docs/internal/gc/zgc-zr1al-oversize-arrays-are-unsizable-outside-the-high-sweep-FIXED-20261017.md`.
- **`CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`** (default **on** since zr z8/zb,
  2026-10-13; `=0` kill) -- the complement sweep stops zeroing each dead low
  object's header (the bytes are zeroed again on reuse; HotSpot never writes a
  dead object either). z5a measured -20 % on the compiled small-array row.
  `=0` restores the zeroed header as a stale-reference tripwire for a run
  hunting a VM bug (the "Stale pointer detected" fallbacks and
  `reclaim_guard`'s `ClassId(0)` gate read it; `CRATONVM_DBG_ZGC_CORPSE`
  forces it).
- `[GC] zgc-alloc-sweep:` (with `CRATONVM_GC_STATS=1` / `--verbose:gc`) prints
  the engagement counters of `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS`
  (`forget_passes` / `forget_declined` / `blocks_forgotten`),
  `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT` (`inflight_tails_lowered` /
  `inflight_cover_bytes`), the oversize-array sweep, and the dead-header skip
  (`dead_headers_skipped`; with the switch set explicitly, `--verbose:gc` also
  prints a per-cycle `[GC] zgc-sweep-skip-dead-headers:` line), and (zr z2/rv)
  `huge_extent_hits`: interior probes answered by the huge-extent table, i.e.
  derived pointers more than 256 MiB into a live array that the bounded
  backwards scan of `resolve_interior_for_pin` / `is_heap_addr` cannot reach.
  Every allocation past 256 MiB is recorded there (no switch: a bug fix --
  since z1 such arrays are freed when dead, so a missed interior root became a
  use-after-free).
- **`CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK`** (default **on**, `=0` kill) -- a
  VM-TLAB tail retired after a collection is no longer subtracted from
  `allocated`, because the collection republished it as live bytes that never
  held the tail. Since 2026-10-17 the same switch also re-charges every tail
  the collection withheld for an owner that could not retire. Those bytes stay
  in `allocated`, so what the owner bumps after the collection is counted, and
  its retire returns only the unused rest (`[GC] zgc-features: ...
  vm_tlab_tail_recharge_bytes=`).
- The per-object parallel sweep (`CRATONVM_ZGC_PARSWEEP` with the bitmap sweep
  off) splits on the covered words, as the complement arm does.

### ZGC capacity: what zr z2/cp changed (2026-10-12)

- **`CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`** (default `2` since 2026-10-13:
  also widens to 0.75 live on a tight heap; `=1` after-failure only, `=0`
  kill) -- the slide after a hard allocation failure
  evacuates pages up to 95 % live (the default cutoff is 25 %), so a
  contiguous request finds room: `Zr2CpCapacity grow` 360 145 -> ~947 000
  retained `int[64]` at `-Xmx256m` (HotSpot ~915 000-925 000).
- **`CRATONVM_ZGC_OVERHEAD_BACKSTOP`** (opt-in `=1`; it was default on for a
  day) -- ends the near-OOM crawl that capacity exposed (one forced collection
  per refusal while the live set creeps up), at the price of an overhead
  verdict HotSpot ZGC does not have. The crawl itself is open:
  `zgc-zr2cp-near-oom-thrash-one-collection-per-refusal-FIXED-20261012.md`.
- **`CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE`** (default on) -- the last-ditch
  clear-all-soft collection clears soft references whose referent only a
  conservative compiled-frame word kept.
- **`CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND`** (default on) -- the infallible
  allocation arms raise a catchable `OutOfMemoryError` where safe instead of
  aborting.
- Counters: `[GC] zgc-capacity:`. Report:
  `docs/internal/zgc-round-zr-20261012/z2-cp-report.md`.

### ZGC allocation: what zr z2/a2 changed (2026-10-12)

- **`CRATONVM_ZGC_MULTIARRAY_TLAB`** (default **on**, `=0` kill) -- on ZGC,
  `multianewarray` (interpreter and JIT, through `alloc_multi_array_collecting`)
  bumps each level that fits a TLAB (<= 32 KiB) from the executing thread's VM
  TLAB, as `newarray` does, instead of one `try_alloc_array_full` per
  sub-array through the heap's own locked TLAB cell. `new int[n][3]` is `n`
  such allocations. Those levels are now also charged to
  `getThreadAllocatedBytes`, as on HotSpot. Generational and G1 unchanged.
  See `docs/internal/gc/zgc-zr2a2-rbc6-array-alloc-probe-is-a-quadratic-allocation-workload-CLOSED-20261019.md`.

### ZGC allocation from compiled code: what zr z3/ja changed (2026-10-12)

- **`CRATONVM_JIT_INLINE_TLAB_ANEWARRAY`** (default **on**, `=0` kill) -- on ZGC
  a compiled `anewarray` (`new Object[n]`, up to 32 KiB) bumps the TLAB inline in
  both JIT tiers, like `newarray` and `new`: the component class is the class
  word, `jit_anewarray_object` the slow path. Generational and G1 unchanged.
  Engagement: `CRATONVM_DBG_JIT_METHOD_STATS=1` (`jit_anewarray_small_calls=` on
  the compiled-new exit line) and `CRATONVM_INTRINSIC_STATS=1`
  (`compiled zgc anewarray sites:`).
- **`CRATONVM_ZGC_DROP_STALE_GC_REQUESTS`** (default **on**, `=0` kill) -- a
  collection request stamped at its decision (the compiled refill trigger, the
  JIT helpers' "no headroom" arms) is dropped when a collection completed in
  between, as HotSpot's `VM_GC_Operation::skip_operation` does. Every collector
  since gci i48/zl2 (`ebfde29798`; Generational and G1 stamp through the same
  shared pause epoch; the name is kept). Exit line
  `[GC] zgc_stale_gc_requests: stamped= dropped=` under `CRATONVM_GC_STATS=1`,
  printed on every collector.
- **`CRATONVM_ZGC_REFUSAL_SKIP_FUTILE`** (default **on**, `=0` kill; zr z5/cr) -- an
  allocation refusal at a door whose ladder continues with an attempt and the
  clear-soft rung, on a heap that allocated under 4 KiB since its last
  collection, skips its forced collection; the rung then owes (and pays) one
  collection even with no soft reference active. The native-boundary relief on
  the hard-failure latch alone is skipped on the same test. HotSpot's stall that
  fails after a GC which freed no page. Exit line `[GC] zgc-refusal:`
  (`skipped_futile= owed_paid= relief_skipped=`, plus the window buckets of every
  refusal collection) under `CRATONVM_GC_STATS=1`.
- **`CRATONVM_ZGC_NATIVE_ROOM_SKIPS_SOFT`** (default **on**, `=0` kill; zr z5/cr) --
  the native factory door's unlatched tail skips the clear-soft rung when its
  forced collection was the only one since the arena refused the request and a
  single span now serves it (young, try, full, try). `soft_skipped_room=`.
- **`CRATONVM_ZGC_NEAR_FULL_PROGRESS=<percent>`** (default **75**, `=0` kill; zr
  z6/tp) -- while the post-collection live set is at or above the 75% live-bytes
  threshold, the occupancy clauses of `needs_gc` (threshold, headroom) wait for
  `max(1 MiB, percent% of the free space)` of new allocation instead of
  `max(free / 4, 64 KiB)`. The quarter floor decays geometrically near full (a
  whole-heap cycle every ~5% of free-space shrinkage: `ProxyBoxingOomeProbe`
  154-166 collections vs HotSpot ZGC's 38). Below the threshold nothing changes;
  when the floor exceeds the free space the allocation failure collects (the
  refusal ladder), so no `OutOfMemoryError` comes earlier. Exit line
  `[GC] zgc-pacing:` (rule and allocation window of every near-full collection,
  `pace_floor_deferred=` = intervals the floor held back) under
  `CRATONVM_GC_STATS=1`.
- **zr zs s6/no (2026-10-15), near-OOM (both opt-in):**
  - **`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR=<KiB>|on`** (default **off**; `on` = 2048 KiB,
    HotSpot's small page) -- the exhausted verdict. At the end of an allocation refusal's
    collection (the ladder doors in `ZGC_REFUSAL_SKIP_SITES` and the clear-soft rung), if the
    collection left under the floor free, the refusing thread's fallible attempts
    (`ZgcRealHeap::try_alloc_object` / `try_alloc_array`) are refused until the next collection
    or until the ladder's `OutOfMemoryError` is built (`create_vm_oome_object` lifts it). The
    ladder then runs its clear-soft rung and throws: HotSpot's stall fails after the first
    collection that freed no page. Without it, below ~1.33 MiB free every collection is an
    allocation failure's and frees only the window since the previous one, so the free space
    shrinks by the retained share per collection down to the last object. Thread-local and
    heap-keyed; infallible VM-internal allocations and other threads never read it.
    `exhausted_armed=` / `exhausted_refused=` on `[GC] zgc-refusal:`.
  - **`CRATONVM_ZGC_NEAR_FULL_FLOOR_FITS`** (default **off**, `=1`) -- a near-full floor at or
    above the free space falls back to `max(legacy, percent% of free)`
    (`director::fit_rearm_floor`), so an occupancy collection still runs before exhaustion
    (the zr7rv6 page's mitigation 1, for natives that refuse without collecting). Costs the
    geometric tail of collections in the last MiB. `floor_fitted=` on `[GC] zgc-refusal:`.
  - **zs s9/no2:** the exhausted verdict is spent once per fill. A verdict lifted while still live
    (its ladder is throwing) blocks every later verdict until a collection leaves the floor free
    (`exhausted_spent=`), so the rest of the fill runs the default ladder. Fixes the s6a L4
    `Zs1NdLauncherOome -Xmx32m` stderr regression under the verdict.
  - **`CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE=<permille>|on`** (default **off**, `on` = 60;
    zs s9/no2): an allocation-driven ZGC collection whose previous cycle left under that share of
    the heap free asks every active soft reference cleared. This stands in for HotSpot's stalled
    majors near full, which this stop-the-world collector does not have. `near_full_soft_clears=`.
  - **`CRATONVM_ZGC_STALL_CLEARS_SOFT`** (default **off**, `=1`) -- a ZGC refusal collection
    (this VM's allocation stall) asks the per-VM clear-all-soft request the last-ditch rung uses,
    when soft references are active: HotSpot's ZGC clears every soft reference in a collection
    caused by an allocation stall. Off, a soft cache survives every stall that ends in a served
    allocation. `stall_soft_clears=` on `[GC] zgc-refusal:`.
- **zr zs s1/tp (2026-10-14), triggers:**
  - **`CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS`** (default **on**, `=0` kill) -- the warm-up clause
    counts warm-up cycles, not collections, as HotSpot does. A `System.gc()` before the first
    warm-up therefore no longer moves every step up by one. HotSpot 25.0.3 prints
    `System.gc()` and then `Warmup` at 10/20/30%; this VM had printed two `Warmup` lines, at
    20 and 30%. The clause also waits for at most half a step of new allocation instead of
    the `gc_rearm` floor.
  - **`CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`** (default **on**, `=0` kill; acts only under
    `CRATONVM_ZGC_RATE_TRIGGER=1`) -- a director rule waits for the same anti-storm floor as
    the occupancy clauses. `director=` and `director_deferred=` are on `[GC] zgc-trigger:`.
  - **`CRATONVM_ZGC_DIRECTOR_STW_MODEL`** (opt-in) -- the director translated to a
    stop-the-world cycle that polls on bytes. The rate reserve is the poll latency, and the
    proactive rule assumes a full throughput drop. See
    `docs/internal/proposals/gc/zgc-zs1tp-proposal-director-stw-model-20261014.md`.
  - **`CRATONVM_ZGC_CONC_START_RECHECK`** (default **on**, `=0` kill) -- a concurrent mark
    start re-tests that it is still due before requesting its pause. `ct_lost_races=` and
    `ct_start_rechecks=` are on `[GC] zgc-conc-trigger:`.
- **`CRATONVM_ZGC_REFILL_CAPACITY_GATE`** (default off, opt-in; zr z5/cr) -- the
  compiled TLAB refill gate asks whether the arena can carve a chunk instead of
  the occupancy verdict, so an armed trigger no longer sends every object down
  the locked per-object path. `gate_passed= gate_carved=`.
- **`CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM`** (default off, opt-in) -- `jit_newarray`
  collects on ZGC's occupancy verdict as its siblings do, instead of spilling
  each array until the TLAB wedge breaker fires. See
  `docs/internal/gc/zgc-zr3ja-compiled-newarray-overshoots-the-occupancy-trigger-SUPERSEDED-20261019.md`.
- Under `CRATONVM_TLAB_SHARE_SIZER=1` on ZGC the refill-waste keep arm no longer
  applies (its fallback on ZGC is the locked per-object path). See
  `docs/internal/gc/zgc-zr3ja-proposal-tlab-share-sizer-on-zgc-20261012.md`.

## ZGC: intended differences from HotSpot 25 recorded by the zt round (2026-10-19)

These are design differences of this collector (a stop-the-world whole-heap collection by default, no page allocator, no generational default) that the zt round decided to record rather than imitate with invented figures. Each paragraph names the pages it closes; the evidence is the local HotSpot 25.0.3 battery (`docs/internal/zgc-round-zt-20261018/`). The pause-count and `-Xlog:gc*` differences are recorded in the `-Xlog:gc`, JMX row above.

**A. Pacing (zr1tr, zs1tp, zt1tp items 5-6, zt4ch-small item 4).** Every collection of the
default ZGC mode is a stop-the-world, whole-heap collection, so the default trigger is not HotSpot
25's generational director. It collects at HotSpot's warm-up points (10, 20 and 30 % of
`SoftMaxHeapSize`), at a live-bytes threshold (75 % of `-Xmx`, re-armed after each collection at
`live + max(free / 4, 64 KiB)`, and near a full heap at 75 % of the free space,
`CRATONVM_ZGC_NEAR_FULL_PROGRESS`), and, once a pause has overrun it, at an allocation budget sized
by a CratonVM-only 200 ms pause target (`CRATONVM_ZGC_PAUSE_TARGET_MS`, `0` turns it off).
HotSpot's director rules (allocation rate, high usage, proactive) are the opt-in
`CRATONVM_ZGC_RATE_TRIGGER=1`; its relocation headroom is never subtracted, because the slide needs
no to-space; its minor / major split and `Timer` rule have no counterpart. `-XX:MaxGCPauseMillis`
is ignored, as on HotSpot's ZGC (`CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS`). Cause names are
HotSpot's (`Warmup`, `Allocation Rate`, `Allocation Stall`, `System.gc()`), but the number and the
timing of collections differ, and a mutator that hits the trigger waits for the collection instead
of allocating through a concurrent cycle.

**B. Near a full heap (zr6tp, zs6no, zt3tp, zr5cr).** Near a full heap each collection marks and
sweeps the whole live set inside one pause (about 40 ms of CPU per collection at a ~230 MB live set
on the Linux host), where HotSpot 25 mostly runs minor, concurrent cycles; near-OOM rows therefore
cost several times HotSpot's CPU even at HotSpot's collection count. (The larger, count-independent
part of that gap is mutator and compiled-code cost, tracked by the JIT pages.) This collector has no
page allocator: it serves the last one or two MiB of free space object by object and throws
`OutOfMemoryError` only when an allocation finds no hole after a collection, where HotSpot's ZGC
throws when no 2 MiB page can be had; a program that fills the heap retains slightly more before
its OOME (about 2 % more on `ProxyBoxingOomeProbe` and `Zr2CpCapacity grow`). HotSpot clears every
soft reference on a major collection caused by an allocation stall; this VM does not stall, and
stands in with a free-share rule: an allocation-driven collection whose previous cycle left under
6 % of the heap free clears all soft references (`CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`,
default 60, `=0` turns it off). That matches HotSpot at 95 % fill and below 92 %; between the two
HotSpot's own answer varies run to run with its stall timing, and this VM's can differ. The opt-in
exhausted verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`) is the lever that would end a fill earlier
(proposal `zgc-zs6no-proposal-exhausted-verdict-flip`).

**C. Pauses, beans and JFR (zs4ob3, zs2ob, zt1jf-default, zs7jf, zr4jf's table).** A collection is
booked as the HotSpot pauses it has a real phase for: `Pause Mark Start` (unless its mark start was
its own pause), `Pause Mark End` and `Pause Relocate Start`, three per collection, with the three
matching JFR VM operations (`ZMarkStartYoungAndOld`, `ZMarkEndOld`, `ZRelocateStartOld`). HotSpot
25 books five per major and eight per `System.gc()` (a promote-all young collection, then separate
young and old marks and relocations), plus non-pause `ZMarkFlushOperation` and
`ZRendezvousGCThreads` operations; this collector's single whole-heap mark and single relocation
have no boundary at which to book them, and no record is invented. As a result the `ZGC Major
Pauses` bean counts three per collection and its collection time is the whole stop-the-world time
(HotSpot's pause beans stay near 0 ms); the default mode runs no young-only collection, so it writes
no `ZGC Minor` bean entries or JFR `ZGC Minor` rows (`CRATONVM_ZGC_GENERATIONAL=1` does); a
`GcInfo`'s `GcThreadCount` is 1 and `jdk.GCConfiguration`'s ergonomic thread counts are generic
(HotSpot's figures describe its own worker pools); JFR does not write `jdk.ZPageAllocation`,
`jdk.ZUnmap`, `jdk.ZStatistics*`, `jdk.ZThreadPhase` or any concurrent phase but `Concurrent
Mark`; `jdk.ZRelocationSet` is written only when the selector ran; and collection events are
committed on `VM Thread`, not on `ZDriverMajor`.

**D. Code heaps (zt1ob item 2; the page stays open for item 3).** The JIT keeps one unsegmented
code arena, reported in `CodeHeap 'non-profiled nmethods'`; `CodeHeap 'non-nmethods'` and
`CodeHeap 'profiled nmethods'` read 0 bytes used (this VM's interpreter and stubs are in the
binary). On ZGC each code heap's `init` is HotSpot's 2555904 (`CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`).
