# Garbage Collection in CratonVM — architecture and current state

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
> `docs/known-issues/gc/gce-handoff-gc-design-perf-round-close-20260929.md`.

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
| *(default)* / `-XX:+UseZGC` / `-XX:+UseZ` | `ZgcRealHeap` (`gc/src/zgc.rs`) | **Not a real ZGC**: a memory-backed, whole-heap mark-sweep over one arena with a bitmap allocation registry. No colored pointers, and **the load barrier is never armed** — `relocate_active` and `set_barrier_color(Some(..))` are written only by unit tests, so marking is published by an ordinary **SATB pre-write barrier** rather than on read. It is not non-compacting. **Default ON:** a stop-the-world sliding compactor (`CRATONVM_ZGC_RELOCATE`, default ON since 2026-08-13, `=0` restores the non-moving sweep), parallel by default (`CRATONVM_ZGC_PAR_RELOCATE`), which runs on every cycle whose per-cycle coverage proof holds. **Opt-in:** concurrent marking (`CRATONVM_ZGC_CONC_START=<percent>` or `auto`), parallel STW marking (`CRATONVM_ZGC_PARMARK=<workers>`) and a generational mode (`CRATONVM_ZGC_GENERATIONAL=1`). So a DEFAULT run is a stop-the-world whole-heap mark (serial) and sweep, followed by a parallel sliding compaction. The colored-pointer/`ZPage` code above it in `zgc.rs` (and `zgc_concurrent.rs`) has production consumers now — `forwarding::ZRelocationSet::select` ranks the slide's pages — though the barrier layer itself remains unreached. |

Unrecognized `-XX:+Use*GC` selectors warn and fall back to Generational.
Heap size comes from `-Xmx`/`-Xms` as usual, and on every backend those two
mean what they do on HotSpot: `-Xmx` is the size of the address-space
*reservation* and `-Xms` is the memory committed at startup. An `-Xms` above
`-Xmx` is clamped rather than refused.

**`-Xms` is honoured on all three backends** since 2026-09-21. G1 got it first
(F-16); ZGC commits the requested prefix of its single arena, and Generational
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
granules. One backend residual keeps the number off HotSpot's on that probe:
ZGC gives free granules back at every cycle, below `-Xms` too
([`zgc-decommit-ignores-xms`](known-issues/gc/zgc-decommit-ignores-xms.md)).
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
  path. The old generation gives pages back only when an (opt-in,
  `CRATONVM_OLDGEN_COMPACT`) compaction empties whole granules; the default
  in-place sweep keeps its pages resident.
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
[`docs/feature-designs/zgc-roadmap-20260920.md`](feature-designs/zgc-roadmap-20260920.md),
which sequences the four current direction documents into one ordered programme,
states the dependency edges between them, and lists what in
[`zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md)
and
[`zgc-maturity-assessment-and-plan-20260813.md`](feature-designs/zgc-maturity-assessment-and-plan-20260813.md)
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
alloc-failure|system-gc> ttsp_us= straggler= xt_pass_us= xt_post_quota_us= collect_us= xt_passes=
xt_frozen= xt_unclassified= xt_roots= hw_windows= hw_roots=`: that pause's
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
`-Xlog:gc` line's `GC(N)` and the JFR events' `gcId`, 0-based; the
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
(Generational's opt-in pinned young copy can move under a blocked peer's
helper window, with `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1`; see "Generational:
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
G1 and ZGC still report none.
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
([`common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications`](known-issues/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md)).
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
   Space` / `G1 Survivor Space` / `G1 Old Gen`, ZGC the non-generational
   `ZGC Cycles` / `ZGC Pauses` over `ZHeap` (the ZGC state lives in
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
   * A JNI native that blocks in C holds every pause, and an attached
     thread's JNI calls run GC-blocked, unless all three JNI switches are on
     (see "JNI and threads in native" below).
   * With the JIT on, an allocation-driven collection on **G1** can keep a
     weakly-reachable referent (cause 2 of
     [`common-w4o-allocation-driven-collections-do-not-clear-weak-referents`](known-issues/gc/common-w4o-allocation-driven-collections-do-not-clear-weak-referents.md);
     the JIT-owned cause 1 is fixed and measured on Generational).
   * On G1 and ZGC, weak and soft references to a finalizer-reachable object
     are kept where HotSpot clears them, at depth 2 (the Generational half of
     `common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot` is
     fixed and matches HotSpot 6/6).
   * `-Xms` is not a floor for ZGC's decommit (the `committed_bytes`
     paragraph above).

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
| `CRATONVM_GC=-g1-adaptive-tenuring` *(alias: `CRATONVM_G1_ADAPTIVE_TENURING=0`)* | Restore the fixed `promotion_age` (15). By default the tenuring threshold is re-derived after each pause from an age histogram of surviving bytes, and may tenure earlier than configured — never later — when survivor space would overflow. |
| `CRATONVM_GC=-g1-reserve-heap` *(alias: `CRATONVM_G1_RESERVE_HEAP=0`)* | Commit the whole heap at startup instead of reserving `-Xmx` and committing on demand. Also the state a platform without a reservation implementation is in anyway. First thing to try for a G1 fault at a heap address that looks mapped. |
| `CRATONVM_GC=g1-uncommit` *(alias: `CRATONVM_G1_UNCOMMIT=1`)* | **Opt-in.** Return the pages of a trailing run of Free regions to the OS at the end of a concurrent-mark cleanup, so a process that has finished a burst does not hold its high-water mark for life. Never shrinks below `-Xms`, and only above the highest region still in use — the committed set has to stay a prefix. Opt-in because the two halves of the reserved heap have different failure modes: getting growth wrong is a missed optimisation, getting the shrink wrong is a fault in compiled code. |
| `CRATONVM_GC=-g1-tlab-clamp` *(alias: `CRATONVM_G1_TLAB_CLAMP=0`)* | Restore the pre-2026-09-20 behaviour: `refill_tlab` REFUSES a request larger than half a region instead of clamping it to that bound. The refusal was a one-way cliff — `tlab::TlabPressureTracker` doubles a hot thread's request up to `MAX_TLAB_SIZE` (1 MiB) and the region-size ergonomic gives 1 MiB regions up to a 2 GiB heap, so the ladder's last rung is over the bound, and the sizer only re-sizes on a refill that actually happened. A thread that reached it fell back to per-object allocation through the exclusive guard for the rest of its life. `[GC] g1 tlab: oversize_clamped=` is the engagement counter; a zero means the ladder never climbed that far on your workload. |
| `CRATONVM_GC=-g1-humongous-best-fit` *(alias: `CRATONVM_G1_HUMONGOUS_BEST_FIT=0`)* | Restore first-fit for the humongous contiguous-run search. By default it is best-fit — the SHORTEST run of free regions that still fits, ties to the lowest index — so a small humongous object spends a tight hole instead of eating the head of a long run. G1 has no compaction pass that can manufacture a run back, so a long run consumed by a request that would have fitted in a hole is gone until the regions above it are freed. Both arms return a run of exactly `count` adjacent Free regions, so this is a placement policy, not a correctness switch; first-fit stops at its first hit while best-fit always scans the table, and `[GC] g1 free-scan: contiguous(...)` prices both. |
| `CRATONVM_GC=g1-heap-resize` *(alias: `CRATONVM_G1_HEAP_RESIZE=1`)* | **Opt-in.** The adaptive heap-sizing policy: a *soft capacity* between `-Xms` and the `-Xmx` region grid, grown when GC overhead exceeds 5 % of wall clock or occupancy reaches 70 % of it, and shrunk after four consecutive pauses below 40 % occupancy and 1 % overhead — to a point that leaves occupancy at 60 %, i.e. strictly inside the dead band, so a shrink cannot land where the next pause grows it back. The capacity is the denominator of the free-fraction trigger AND the floor for the trailing-Free-run shrink, which this flag also moves from `cleanup` to the end of every evacuation pause (gated on phase Idle, SATB inactive and an empty gray set, the same gate `eager_reclaim_early_decline` computes). Without it, `-Xmx` is a one-way ratchet on RSS for any workload that never crosses IHOP, because `cleanup` is the shrink's only caller. Watch `heap_grows` and `heap_shrinks` on the `[GC-STAT]` line: two large, nearly equal counts are the hysteresis being too weak. |
| `CRATONVM_GC=g1-pause-cost-model` *(alias: `CRATONVM_G1_PAUSE_COST_MODEL=1`)* | **Opt-in.** Make the mixed-CSet copy budget price the pause it is actually buying. Two terms: the YOUNG half of the collection set is charged first (every Eden and Survivor region is in the CSet unconditionally, so `max_gc_pause_ms` was spending its whole goal on the *second* copy of the pause), and each old candidate is charged the MARGINAL fix-up cost it causes — `fixup_ns_ema / fixup_regions_ema` times the walk regions it adds (its to-space destination, plus its remembered-set sources that no already-selected candidate has paid for). Before this, `fixup_ns_ema` was a constant with respect to the very choice being made, so the model under-estimated in the direction that overruns. Both terms are subtractions from the budget, so arming it can only make a mixed collection set SMALLER — safe for the pause goal, and a throughput question for the mixed phase, which is why it is opt-in. The selector still takes one region unconditionally for forward progress. |
| `CRATONVM_GC=-g1-humongous-run-guard` *(alias: `CRATONVM_G1_HUMONGOUS_RUN_GUARD=0`)* | Restore the old hinted first-Free-region scan for an explicit performance A/B. By default, `claim_free_region` preserves the longest Free run: it prefers a Free region outside that run and otherwise takes its END. That prevents a single-region Old or humongous-start claim from splitting a span a later humongous allocation needs. Placement only: both arms return a Free region and neither can fail where the other succeeds. The default pays a full table scan under the exclusive guard; `[GC] g1 free-scan: single(...)` prices both arms, and `[GC] g1 humongous: requests= failures= free_at_failure= longest_run_at_failure=` is the outcome census. |
| `CRATONVM_GC=-g1-eager-humongous` *(alias: `CRATONVM_G1_EAGER_HUMONGOUS=0`)* | Restore cleanup-only humongous reclaim. By default an evacuation pause also frees humongous spans it can prove nothing references. This is the only path that frees memory outside the collection set, so it is the first thing to rule out if a live humongous object goes missing. |
| `CRATONVM_GC=g1-young-pause-target` *(alias: `CRATONVM_G1_YOUNG_PAUSE_TARGET=1`)* | **Opt-in.** Let `max_gc_pause_ms` bound the YOUNG generation too, not just the old half of a mixed collection set: G1 also collects once the Eden+Survivor region count reaches an adaptive target, tightened by 20% after any PRODUCTIVE pause that overruns the goal and relaxed while pauses stay under half of it. Does nothing until such an overrun is measured (the target starts at its 60%-of-regions ceiling and a target at the ceiling is not a trigger). Measured trade on `G1ChurnPauseProbe` at `-Xmx2048m`: p50 -21%, p99 +3%, wall +4.2%, one extra pause — see the young-sizing paragraph under Backend details for the full table and why it is not a default. |
| `CRATONVM_GC=g1-workers=<n>` *(alias: `CRATONVM_G1_WORKERS=<n>`)* | Force the evacuation worker count; `=1` drains the parallel path serially, which separates a concurrency race from a logic divergence. Overrides `-XX:ParallelGCThreads`, which in turn overrides the machine-derived default |
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
| `CRATONVM_GC=young-trigger-percent=<n>` *(alias: `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<n>`)* | Moving young collection trigger, as a percent of from-space capacity (default 50, clamped 1..=95). Raising it collects less often and copies more survivors per cycle; see "Young sizing" below. The NON-moving sweep has its own, higher trigger — `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`, 90, not settable — because it reclaims in place and needs no Cheney headroom |
| `CRATONVM_GC=-peer-pin-divert` *(alias: `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`)* | Let a moving young cycle proceed even when a conservatively-discovered JIT root was published this cycle. Restores the pre-2026-09-06 behaviour. **This is the single largest lever on whether a JIT-warm Generational run compacts at all** — the term it disables (`unrewritable_conservative_jit_roots`, see the `divert_non_moving` table below) fires on essentially every cycle once compiled code is live. It is off by default because a conservative pin landing in young-from names an object whose holder word nothing can rewrite; turn it on only to A/B a suspected over-diversion, and read `[GC] moving_young:` on both arms |
| `CRATONVM_DBG=gcpause` (alias `CRATONVM_DBG_GCPAUSE=1`) | Per-phase breakdown of the MOVING young cycle, printed for any collection over 100 ms. **The rows partition the pause since 2026-09-20**, as G1's `[GC-STAT]` line does: every figure is microseconds rendered `{:.3}ms`, and an `other=` residual is printed unconditionally, including zero, so the rows plus `other` equal the total by construction. Until that date there was no residual — everything before the first mark and after the last was charged to nothing — and every figure was `as_millis()`, so twenty sub-millisecond phases printed twenty zeros against a 12 ms total. **Any `[gcpause]` output quoted from before 2026-09-20 has both defects**, including the table under "Where a moving young pause actually goes" below. `docs/internal/gc/gengc-plumbing-gcpause-phases-do-not-sum-FIXED-20260923.md`. *(2026-09-23)* The 100 ms threshold is `CRATONVM_DBG_GCPAUSE_MIN_US` (token `CRATONVM_DBG=gcpause-min-us=<us>`; `0` prints every cycle), so the phase medians of small, stress-driven pauses can be taken; while a JFR recording runs the same marks are emitted as `jdk.GCPhasePause` events, `other` included |
| `-Xlog:gc`, JMX *(generational, 2026-09-23; round 5 changes in "Generational: what rounds 4 and 5 changed" below)* | The `-Xlog:gc` line's `before->after(committed)` is the heap's live occupancy (`VmHeap::live_bytes_estimate`: young `used - free_list` plus old used), not the bump high-water, which does not move across a non-moving young sweep and printed `8M->8M` on every pause of a run that was reclaiming. `ManagementFactory` reports HotSpot Serial's beans: collectors `Copy` (cycles that did not reclaim old gen) and `MarkSweepCompact` (cycles that did; a full cycle counts there only), pools `Eden Space` / `Survivor Space` / `Tenured Gen` with real usage. G1 (gc-common w7-f) reports HotSpot G1's beans: `G1 Young Generation` (every collection pause, young or mixed; `end of minor GC`), `G1 Old Generation` (full collections, which this G1 does not run, so 0 — HotSpot's `System.gc()` is a full collection there), `G1 Concurrent GC` (the marking cycle's initial-mark and remark + cleanup pauses; `end of concurrent GC pause`, cause `No GC`), and pools `G1 Eden Space` / `G1 Survivor Space` / `G1 Old Gen` (Old + humongous regions; committed = the committed heap minus the young pools) from one region census, with `getCollectionUsage()`, `getLastGcInfo()` and GC notifications. ZGC reports `ZGC Cycles` (`end of GC cycle`) and `ZGC Pauses` (every stop-the-world pause, collections and mark starts; `end of GC pause`) over one `ZHeap` pool — HotSpot's non-generational ZGC names, this ZGC's default mode — from `HeapRealm::zgc_gc_beans` (`handoff-w7f-zgc-backend-gc-beans`, applied; `docs/internal/gc-common-round-20260923/gengc-r4w2-obs-g1-and-zgc-jmx-beans-are-still-the-legacy-pair-20260923-FIXED-20260923.md`). On G1 and ZGC the per-bean counts and times are the common GC event plumbing's (`gc_metrics::BackendGcBeans`, one per door pause, per VM), not the collector's own counters: a G1 evacuation-failure drain pass is not a second collection there. A marking pause's notification is delivered at the next drain point (the next collection's door, or the delivery thread's next batch). `getCollectionTime()` is real accumulated pause time on every backend: Generational per bean from the heap's pause totals, **the floor of the bean's microsecond sum, as HotSpot** (since gen r5w3/obs7; until then each pause was rounded up to a millisecond, so a run of sub-millisecond pauses reported one millisecond per pause — `GenR5W3CollectionTimeProbe` matches Serial now; H2's `collectGarbage()`, which loops until the value moves, may call `System.gc()` a few more times when full collections take under 1 ms, and terminates as it does on HotSpot — see `gengc-r5w2-obs6-collection-time-ceils-every-pause`); G1 and ZGC per bean from the plumbing's sealed durations (`pause_sum_as_collection_time_ms`: whole milliseconds plus one per pause, gc-common w7-f). The single-bean fallback (`NativeContext::gc_collection_time_ms`, `VmHeap::collection_time_ms`, still what a backend with no bean state answers) is G1's own microsecond pause sum rounded the same way, and on ZGC the plumbing's backend-independent sum (`HeapRealm::gc_pause_ns_total`, gc-common w6-f). `--verbose:gc`'s shutdown census carries the same split as `[GC] jmx_collectors:` |

Note: `tracing::debug!` is compiled out of release builds
(`release_max_level_info`); for cycle-phase confirmation attach gdb to
un-inlined gc-crate symbols (LTO is off for `cratonvm-cli` dev builds).

**Tuning knobs** (java-compatible): `-Xmx` (reserved address space on every
backend, including the Generational old generation since 2026-09-23),
`-Xms` (committed at startup, **honoured on all three backends** since
2026-09-21 — see the heap-size note at the top of this
file), `-XX:G1HeapRegionSize=<bytes>` (rounded to a power of two and
clamped to 1-32 MB), `-XX:InitiatingHeapOccupancyPercent=<n>` — a **ceiling**
on the adaptive threshold, never a floor, floored at max(1 % of heap, one
region) — `-XX:MaxGCPauseMillis=<n>` (the young-generation size target and the
mixed collection's copy-time budget), `-XX:ParallelGCThreads=<n>`,
`-XX:G1MixedGCLiveThresholdPercent=<n>` (default 85: an Old region at or above
this percent live is never a mixed-collection candidate),
`-XX:G1HeapWastePercent=<n>` (default 5: the mixed phase ends early once the
candidates' garbage is below this percent of the heap),
`-XX:MaxHeapSize`, `-XX:+HeapDumpOnOutOfMemoryError`.

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
invisible. On the parallel evacuator phases 1-3 are fused into one work-stealing
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
set. See `gc/src/old_gen.rs`'s module header.)* So the flag is a
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
  (45 `Concurrent Mark`, 45 `Concurrent Sweep`) and its pauses.

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

**JNI and threads in native (opt-in).** By default a Java thread inside a
JNI native method is a counted mutator: a native that blocks in C holds every
pause. An attached host thread runs its JNI functions GC-blocked, and a JNI
local ref is a raw heap address. Three switches change that, and they flip
only together:

- `CRATONVM_JNI_NATIVE_TRANSITIONS=1` (gcd d3/k): the native runs in native
  (GC-safe, HotSpot's `_thread_in_native`); every JNIEnv call transitions
  native -> VM -> native, and pauses exclude the thread for the rest of the
  call. Inert without `CRATONVM_JNI_INDIRECT_LOCALS=1`, because the native's
  own copies of its local refs must survive a move.
- `CRATONVM_JNI_FOREIGN_TRANSITIONS=1`: an attached thread publishes its OS
  tid while it runs, so the take-over covers it. On its own it is not a safe
  configuration: an attached thread's locals stay raw, and one run in three
  crashed on a raw local read after a move (d7).
- Since gcd d5/f, a JNIEnv call that only touches primitive contents of one
  of the native's own locals (`GetArrayLength`, `Get/Set<T>ArrayRegion`,
  `GetString[UTF]Length`, `DeleteLocalRef`) runs in a leaf window: the
  thread stays in native and a pause request waits for the window to close
  (`GcBarrier::try_open_leaf_window`, `GcBarrier::drain_leaf_windows`).
  Leaving native skips the wake's fixup and snapshot refresh when no pause
  ran in the window.

With all three on, the JNI probes print HotSpot's lines (15/15 on d7). The
cost is still above the flip bar (`Gcd1JniCostProbe`, medians):

| call | default | all three switches |
|---|---|---|
| leaf JNIEnv call (`int-region`) | 24-27 ns | 34-36 ns (about 1.4x) |
| empty native (`noop`) | about 0.53 us | 4.1-5.6 us (8-10x) |
| `NewStringUTF` | about 0.5 us | 7.9-9.5 us (about 17x) |

The flip waits on
[`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928`](known-issues/gc/gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md).
Round 13's `CRATONVM_FFM_DOWNCALL_GC_SAFE` (opt-in) runs an FFM downcall in
native the same way; since gcd d5/f a JNIEnv call from C inside such a
downcall leaves the downcall's region first and re-enters on return. The
default dispatch of an empty JNI native costs about 525 ns against HotSpot's
5-10 ns, a separate open page
([`gcd-d5f-jni-default-dispatch-costs-500ns-20260928`](known-issues/gc/gcd-d5f-jni-default-dispatch-costs-500ns-20260928.md)).

**The young copy under conservative roots (opt-in).** The pinned young copy
(`CRATONVM_GEN_PINNED_YOUNG_COPY`) gained three opt-in arms this round:

- **Take-over arm** (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1`, gcd d3/m).
  A young cycle whose only divert is helper windows the pass pinned whole
  runs the pinned copy, with every young word of each window's band pinned.
  It engages on d7: `ptko_candidates=39 ptko_taken=39` on its probe.
- **Blocked-monitor proof** (`CRATONVM_XT_BLOCKED_MONITOR_PROOF=1`, d2/i and
  d3/m). A peer blocked in the compiled `monitorenter` helper, whose
  blocking deposit proved its JIT chain rewritable, is credited like a
  parked peer; its band is read into the young pin ledger.
- (The d5/s no-promote hold on a requested major was removed in gce e2: gcd
  d9/a's wide true-root seed made it redundant.)

On a ledger-pinned young cycle, as on any cycle with live compiled frames
under the guard above, Phase 5's major still runs and consumes a
fragmentation request, but in place: only the compaction is vetoed
(`major_gc_finalizing`'s `no_compact`; gcd d3/m, d4/m, d4/n). The pinned copy is still not
default: row 3a of its flip gate passes on its main probe (arm D `PASS all 8`
5/5), but row 3, liveness, does not. With the futile-young backoff at its
default, arm D passed 1 of 2, and two flag-on runs timed out in the livelock
above.

Also opt-in this round:

- `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` (d2/g). The non-moving sweep does not
  pin a young root value all of whose root occurrences came from statics,
  interned strings, class mirrors or JNI globals, so such an object can be
  promoted under a live JIT (`[GC] precise_root_promote:`).
- `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` (d5/q). The non-moving sweep is
  triggered by its own pause loop (90 % start,
  `adapt_sweep_trigger_to_pause`) instead of the moving collector's
  pause-goal-lowered threshold. It prints `[GC] young_sweep_trigger:` only
  when on, and no d7 row measured it.
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
| `CRATONVM_SOFTREF_HOTSPOT_LRU` | **ON on Generational** since gen r5w4 (per-backend default; G1/ZGC unchanged, OFF) | The SoftReference LRU policy takes HotSpot's `LRUMaxHeapPolicy` inputs (the soft clock and free heap as of the previous collection), so a soft reference used since the last collection is not cleared by the policy; `-XX:SoftRefLRUPolicyMSPerMB` is honoured. `GenR5W2SoftRefIdleYoungProbe`: `lost=0 PASS` by default, as HotSpot (was `lost=2 FAIL`). |
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
([`gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young`](known-issues/gc/gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md)).
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
| `CRATONVM_GEN_PINNED_YOUNG_COPY` | Young cycles relocate under conservative JIT roots: each conservative word pins its 4 KiB page, pinned objects forward to themselves, every other survivor is copied into free from-space spans or promoted; declines when the pin ledger is incomplete or pinned pages exceed 1/8 of from-space (`moving-pinned-pages`, `[GC] young_pinned_copy:`). | JitWarm 125/125 moving; `GenR4W4EvacThroughputProbe` 7.0-7.2 s against 7.6-8.2 s default (round-5 tip). The monitor-path crash that held the netty rows is fixed. NOT YET (d7): row 3a of the flip gate passes (`GenR4W6JitOomRootProbe` arm D `PASS all 8` 5/5), row 3 (liveness) does not (arm D with the backoff at its default 1/2; two flag-on 300 s timeouts, the fallback-young livelock). [`gengc-r4w6-pinstale6-pinned-copy-default-flip-gate`](known-issues/gc/gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md) |
| `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL` | The pinned cycle on the parallel evacuator. | Rides with the above. [`gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy`](internal/gc/gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-DONE-20260928.md) |
| `CRATONVM_GC_PAR_EVAC_CARD_SEED`, `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE` | Evacuator items W2 and L2 tail window. | No gain measured on the evac probe; they ride with the pinned copy. (W1, W4 and the scan prefetch were removed in gce e2.) [`gengc-r5w3-evac7-proposal-young-copy-round-two`](internal/gc/gengc-r5w3-evac7-proposal-young-copy-round-two-RETIRED-20260927.md) |
| `CRATONVM_TLAB_SHARE_SIZER` | HotSpot-shaped per-thread TLAB sizing (allocation share per young cycle / 50, 8 KiB-1 MiB) plus the refill-waste limit. | Protocol A: W1 minors 610 → 63. Briefly the Generational default in gen r5w4, then **reverted**. Its two defects:<br>• (B), an OOME recovery failing: fixed.<br>• (A), a static reading back NULL in `GenR5W3ConcUnloadProbe` with the JIT on: the lost class loader gcd d1/c fixed (`mark_young_to_old_refs`); it did not reproduce in 10 d7 runs.<br>NOT YET: the flag also changes G1 and ZGC defaults, so the flip waits for a round that measures them. [`gengc-r5w4-orch-share-sizer-hands-out-live-memory`](internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md) |
| `CRATONVM_GC_OLD_BORROW_YOUNG` | The old generation may grow in place into young's budget after a refusal (reserved to 2/3·Xmx), as HotSpot's tenured grows. | `GenR5W3OldBorrowProbe 152` at `-Xms8m -Xmx256m` passes on the default arm too (on `897210f83` and on d7). NEVER until a probe needs it. [`gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget`](internal/gc/gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget-DONE-20260928.md) |
| `CRATONVM_GC_OLD_INTERIOR_DECOMMIT`, `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` | Return interior free runs of the old generation to the OS, and shrink after a concurrent cycle, not only after a STW one. | NEVER as a default: HotSpot Serial keeps that memory too (`GenR4W6OldShrinkProbe` prints `FAIL` on HotSpot; the pair prints `PASS shrunk`). An embedder's footprint option. [`gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection`](internal/gc/gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection-SUPERSEDED-20260928.md) |
| `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` | Humongous reference arrays zeroed outside the old-gen lock (under a same-footprint `long[]` disguise). | One d7 run per arm: 20031 against 17119 MiB/s, inside the host's noise; the page's interleaved A/B decides. [`gengc-r4w6-tlab6-humongous-reference-arrays-zeroed-under-the-old-gen-lock`](known-issues/gc/gengc-r4w6-tlab6-humongous-reference-arrays-zeroed-under-the-old-gen-lock-20260924.md) |
| `CRATONVM_GC_ADAPTIVE_TENURING` (or any of `-XX:MaxTenuringThreshold` / `InitialTenuringThreshold` / `TargetSurvivorRatio`) | HotSpot's adaptive tenuring; `-XX:+PrintTenuringDistribution` honoured. Default: fixed age 3. | `GenR4W6TenuringProbe` `died-young` (= HotSpot) with it; the default promotes 96 % of the medium-lived set. Its blocker (`GenR4SoftRefLruProbe` failing with it) was a default-path bug, fixed in gcd d4/n. NOT YET: items 4-8 of the page's gate not run with the flag. [`gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure`](known-issues/gc/gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure-20260924.md) |
| `CRATONVM_GEN_CONC_SERVICE_THREAD` | A VM daemon drives the concurrent cycle (wakes on hand-off, on direct old-gen growth, or every 20 ms) instead of mutators polling it after young collections. | Every probe row run with it matches HotSpot (d7: 30+ runs over the conc-mark-gate, steady-promotion, rset, fragmentation, direct-old-growth and remark probes). Held for a gc-stress run, a thread-visibility check (it registers no `Thread` object) and a latency number, and because on `GenR4W4SteadyPromotionProbe 4` its arm completes no concurrent cycle. [`gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection`](known-issues/gc/gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md) |
| `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, `CRATONVM_GEN_CONC_CLASS_UNLOAD` | Reference processing at the concurrent remark, and class unloading by the concurrent cycle (needs the hook); unloaded classes' layouts are retained until a sweep census proves no instance is left (gen r5w4/conc8). | With `CRATONVM_GEN_Y2O_LIVE_SEED=1` and `CRATONVM_GEN_YOUNG_MIRROR_DEFER=1` (gen r5w6/conc10, also opt-in), `GenR5W5ConcUnloadProbe` unloads the dead loader and class exactly as HotSpot Serial does, with the JIT on (3/3). Without those two it prints `dead-loader-unloaded=false`. The remark matches HotSpot on `GenR5W5RemarkRefsProbe` except `weak-to-resurrected-cleared`, which the hook arm still prints `false` on d7. `CRATONVM_GEN_YOUNG_MIRROR_DEFER` is the switch that makes the concurrent cycle able to unload a class at all: with it alone `GenR5W5ConcUnloadProbe` = HotSpot 3/3; without it 63 concurrent cycles unload nothing. NOT YET: the four flip together after the remark token is fixed and a full battery. [`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`](internal/gc/gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md), [`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes`](known-issues/gc/gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md) |
| `CRATONVM_GEN_SATB_OLD_ONLY` | The SATB pre-barrier logs only old-generation old values. | [`gengc-r4w5-concmark5-satb-barrier-logs-young-old-values`](known-issues/gc/gengc-r4w5-concmark5-satb-barrier-logs-young-old-values-20260924.md) |
| `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` | Optimizing-tier frames drop dead reference homes from the scanned map (a liveness claim, no stores). | NEVER as a default (d8): superseded by the default-on IR keep set and dead-home clears; with the oracle on, `GenR4W6JitOomRootProbe` times out after seven PASS lines on d7. Earlier: `GenR4W6JitOomRootProbe` 4 of 8 pass on the wave-4 build (3 before r5w4's pin scoping; the round-5 tip's default-on fixes are in the defaults table), `[liveset] contradictions=0`; with `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` the oracle walk reports corrupt value cells and the run times out (opt-in arm only, also on r5w3). `GenR5W3OsrHolderProbe` 4/4 fail with the JIT, pass `--nojit`: the holder is outside the compiled frame; `CRATONVM_DBG=oldmark-root-census` now names it (`[holder-census]`, gen r5w4/jit8). [`gengc-r5w2-oomjit6-proposal-precise-ir-frame-liveness`](internal/gc/gengc-r5w2-oomjit6-proposal-precise-ir-frame-liveness-DONE-20260928.md) |
| `CRATONVM_GEN_Y2O_LIVE_SEED`, `CRATONVM_GEN_YOUNG_MIRROR_DEFER` | The concurrent cycle seeds old gen only from LIVE young objects (a young trace first), and a young user-class mirror is not a root while it is young. | Needed for concurrent class unloading with the JIT on (row above). [`gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects`](internal/gc/gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md) |
| `CRATONVM_GC_OLD_LIVE_SWEEP` | An O(live) old-gen sweep (gen r5w5/old9; since r5w6 a reachable finalizable no longer disables it). | `GenR5W5OldLiveSweepProbe` and `GenR5W6LiveSweepFinalizerProbe` match HotSpot; `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY=1` is clean. NOT YET: `GenR4W4HeapFullThrashProbe -Xmx128m` failed 2 of 3 with it on the d3 build; the gate is that probe 5/5 and `GenR4W5ThreadsOomProbe` 3/3 with it. [`gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep`](internal/gc/gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep-DONE-20260928.md) |
| `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`, `CRATONVM_GC_STAGED_ARGS_KEEP_MASK`, `CRATONVM_JIT_OSR_DROP_ORPHANS` | Store-form and scanner-form fixes for compiled frames that keep dead references as roots after an OOME. `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` moved to the defaults above in r5w5. | NEVER as defaults (d8): no d7 probe needs them. `GenR5W3OsrHolderProbe` and, since the GC defects round's value-range dead homes, `GenR5W2OsrDeadSlotProbe` pass by default; `NativeGrowthReclaimProbe` does not. [`gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2`](internal/gc/gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2-FIXED-20260928.md) |
| `CRATONVM_JIT_INLINE_CARD_MARK` | Compiled old-receiver reference stores check the card inline instead of always calling the barrier helper. | Needs the A/B. The optimizing tier's `aastore` arm checks the card inline since JIT round 13 (`emit_ir_aastore_card_arm`, under this switch and `CRATONVM_JIT_IR_INLINE_CARD_CHECK`, default on); its gated `putfield` still calls the helper for every old receiver. [`gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper`](known-issues/gc/gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper-20260924.md) |

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
  ([`gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth`](known-issues/gc/gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth-20260926.md)).
* **`ManagementFactory.getMemoryMXBean()`** is one bean per VM (it was a fresh
  object per call, so no listener on it could ever be notified).
* **`getCollectionTime()`**: floor of the bean's microsecond sum (row above).
* **`-Xlog`** (every backend): HotSpot's default decorators
  `uptime,level,tags` with HotSpot's padding, every HotSpot decorator
  accepted, comma-separated selections, `all`, `disable`, `tag*` wildcards,
  output options accepted (no rotation), `%p` / `%t` in file names, and
  repeated `-Xlog` options honoured. A bare `-Xlog` is `-Xlog:all` and no
  longer takes the next argument (gen r5w4/obs8). On Generational the
  collector prints Serial's `Using Serial` and a truthful `gc,init` subset at
  startup, and per collection `gc,start`, two `gc,heap` lines
  (`DefNew: … Eden: … From: …`, `Tenured: …`), the `gc` line and `gc,cpu`
  (`User=… Sys=… Real=…`). The concurrent cycle's line is under
  `gc+marking`. Not printed yet: `gc,heap,exit`, `gc,metaspace`,
  `gc,phases` for full collections, `gc,age`
  ([`gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines`](known-issues/gc/gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines-20260926.md)).
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
