# Garbage Collection in CratonVM — Architecture and Technical Reference

> **Current Architecture Status.** CratonVM provides three garbage-collector backends
> behind a unified dispatcher (`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world
> at the collection level; G1 additionally runs its marking phase concurrently.
> - **Generational** (`-XX:+UseGenerationalGC`): Semi-space young generation with TLAB bump allocation and free-list old generation with concurrent mark-sweep and adaptive compaction.
> - **G1** (`-XX:+UseG1GC`): Region-based evacuation with remembered sets, SATB concurrent marking, humongous regions, and region pinning.
> - **ZGC** (`-XX:+UseZGC`, default): Single-arena whole-heap mark-sweep with bitmap allocation registry, parallel STW marking, and parallel sliding compaction.
> 
> Historical round logs and development records are archived under internal records.

CratonVM has three garbage-collector backends behind one dispatcher
(`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world at the collection
level; G1 additionally runs its marking phase concurrently. Selection is
java-launcher-compatible:

| Flag | Backend | One-liner |
|---|---|---|
| `-XX:+UseGenerationalGC` / `-XX:-UseZGC` | `GenerationalHeap` (`gc/src/gen_heap.rs`) | Semi-space young gen + free-list old gen with a concurrent old-gen mark-sweep cycle. Young collections are **moving by default**; each cycle diverts to the non-moving sweep only when its own root-coverage proof fails (see "Backend details" below). |
| `-XX:+UseG1GC` | `G1Collector` (`gc/src/g1.rs`) | Region-based (region size targets ~2048 regions, clamped to 1-32 MB): young/mixed evacuation with remembered sets, SATB concurrent marking, humongous spans, region pinning. The heap is **reserved** at `-Xmx` and committed on demand. |
| *(default)* / `-XX:+UseZGC` / `-XX:+UseZ` | `ZgcRealHeap` (`gc/src/zgc.rs`) | **Not a real ZGC**: a memory-backed, whole-heap mark-sweep over one arena with a bitmap allocation registry. No colored pointers, and **the load barrier is never armed** — `relocate_active` and `set_barrier_color(Some(..))` are written only by unit tests, so marking is published by an ordinary **SATB pre-write barrier** rather than on read. It is not non-compacting. **Default ON:** a stop-the-world sliding compactor (`CRATONVM_ZGC_RELOCATE`, default ON, `=0` restores the non-moving sweep), parallel by default (`CRATONVM_ZGC_PAR_RELOCATE`), which runs on every cycle whose per-cycle coverage proof holds. **Default ON since zr z3/mt:** parallel STW marking (`CRATONVM_ZGC_PARMARK`, unset = `auto`, HotSpot's pause-worker ergonomic or `-XX:ParallelGCThreads`; `=0` restores the serial loop). **Opt-in:** concurrent marking (`CRATONVM_ZGC_CONC_START=<percent>` or `auto`) and a generational mode (`CRATONVM_ZGC_GENERATIONAL=1`). So a DEFAULT run is a stop-the-world whole-heap mark (parallel; serial on a small heap) and sweep, followed by a parallel sliding compaction. The colored-pointer/`ZPage` code above it in `zgc.rs` (and `zgc_concurrent.rs`) has production consumers now — `forwarding::ZRelocationSet::select` ranks the slide's pages — though the barrier layer itself remains unreached. |

Unrecognized `-XX:+Use*GC` selectors warn and fall back to Generational.
Heap size comes from `-Xmx`/`-Xms` as usual, and on every backend those two
mean what they do on HotSpot: `-Xmx` is the size of the address-space
*reservation* and `-Xms` is the memory committed at startup. An `-Xms` above
`-Xmx` is clamped rather than refused.

**`-Xms` is honoured on all three backends** . G1 got it first
(F-16); ZGC commits the startup budget across both ends of its single arena, and Generational
commits the request across its two young semi-spaces and 
the remainder as an old-generation prefix. Until  the two
non-G1 backends accepted the flag and dropped it *in silence*, on the default
collector — `VmHeap::new_with_heap_sizing` now makes each backend arm state
what it did with the value (`XmsDisposition`), which is what stops that
returning. See
internal records.

Two things `-Xms` deliberately does **not** do. It never sizes a *reservation*:
on ZGC the arena envelope is captured once and read without the arena lock by
`conservative_addr_span`, the object-start bitmap and the JIT read-bounds
publication, so an `-Xms` that shrank it would produce a heap that can never
reach `-Xmx`. On Generational the young pair takes the request first and the
old generation commits the remainder as a prefix, so the startup commit is
`-Xms` (rounded up to 2 MiB granules). *(Until  the old generation
was a `Vec` committed in full at construction, so the startup commit there was
at least `-Xmx / 2` whatever `-Xms` said.)*

`VmHeap::committed_bytes` — `Runtime.totalMemory`, the JMX heap
`committed`, the `(NNNM)` of an `-Xlog:gc` line — is the heap's **committed**
size, HotSpot's meaning: `-Xms64m -Xmx512m` reports about 64m
and grows from there. Until that date it was a sum of *capacities* on
Generational and ZGC, so `totalMemory` was `-Xmx` from the first
instruction on the default collector (`XmsProbe` printed `total=512m` against
HotSpot's `total=64m`). Per backend: Generational counts the young pair's
committed granules plus the old generation's touched prefix
(`OldGen::high_water`); G1 its committed region prefix; ZGC its committed
granules. ZGC preserves `-Xms` across every decommit path, with per-VM
`-XX:+/-ZUncommit` and `-XX:ZUncommitDelay=<seconds>` (default 300).
Its startup budget covers both allocation ends, so `XmsProbe -Xms64m -Xmx512m`
reports `total=64m` in both runtime modes.
See `the resolved ZGC floor issue`.
(G1's first parallel young pause committed the whole reservation until
:
`g1-parallel-survivor-claim-commits-the-whole-reservation`.)
`VmHeap::os_committed_bytes` is the OS-accounting twin: the same on G1 and
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
  ; before that the old generation was a `Vec<u8>` of its whole
  share — half of `-Xmx` — and on Windows took that full commit charge at
  startup)*. The two young semi-spaces are `Arena`s; the old generation
  (`OldGen`) sits on the same `HeapStore` and commits a granule when its
  free-list allocator first hands out storage in it. A refused commit is an
  allocation failure, i.e. the ordinary promotion-failure / `OutOfMemoryError`
  path. The old generation gives pages back only when a compaction
  (`CRATONVM_OLDGEN_COMPACT`, default ON ; `=0` kills it) empties whole granules; the in-place sweep
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
internal records,
which sequences the four current direction documents into one ordered programme,
states the dependency edges between them, and lists what in
[`zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md)
and
`zgc-maturity-assessment-and-plan.md`
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
): an odd primitive the JIT published in a reference home reads as
`slot | INDIRECT_TAG`, and used to crash the pause or corrupt memory when the
remap wrote through it (`ShadowOddLongProbe`). Refused entries are counted
(`rejected_indirect_entries`). The JIT producer -- a dead reference local
whose register another local shared -- is no longer published since, so the count should read zero; the containment stays.
Un-retired TLAB tails are
published as walker skip regions. What protects the frozen state then differs
by backend: Generational and ZGC do not move on such a cycle (`XT_TAKEOVER`
diverts / refuses), while G1 evacuates and relies on pinning the regions the
scanned words name —  a word pointing INTO an object is
resolved to that object's base too (`CRATONVM_XT_TAKEOVER_INTERIOR`, default
on, `=0` restores exact bases only; the runtime matrix is recorded and the page
retired,
`common-c-takeover-probe-drops-derived-pointers`).
The one licence all three read is the table below. The takeover also covers
the four
concurrent-mark STW pauses, and a cross-thread `Thread.getStackTrace` pause
freezes but gathers no roots. `--verbose:gc` prints the engagement census at
exit: `[GC] xt_peer_scan:`, `[GC] xt_peer_shadow:` (blocked peers) and
`[GC] xt_takeover_shadow:` (frozen peers, ), and
`[GC] ttsp: pauses= avg_us= max_us= last_us= max_straggler=` for
time-to-safepoint. That exit line aggregates EVERY stop-the-world pause the
barrier ran — collections and the non-collection pauses (concurrent-mark
STW phases, cross-thread stack walks, heap dumps) alike — so its `pauses=`
is at least, and usually more than, the number of `[GC] pause:` lines.
`max_straggler=`  is the registry `ThreadId` the slowest
pause waited for last — the numbering of `CRATONVM_DBG_STW_CENSUS=1`'s
`[stw-arrive] tid=` lines — or `none` when that pause's quota was met with
no participating arrival (no peer, or the last peers were frozen in
compiled code and excused by the take-over). Per pause (every
backend and every door) it prints `[GC] pause: gc=N door=<alloc-threshold|
alloc-failure|system-gc|...> ttsp_us= straggler= xt_pass_us= xt_post_quota_us= collect_us= xt_passes=
xt_frozen= xt_unclassified= xt_roots= hw_windows= hw_roots=` (gci i8/gs an occupancy
verdict taken through the forced door -- the compiled TLAB refill trigger, and on G1 the JIT
array headroom guards -- prints `door=alloc-threshold`; `door=alloc-failure` is an allocation
that failed; both are HotSpot's "Allocation Failure". Since gci i11/go G1's last-ditch
compaction prints `door=g1-compaction`): that pause's
time-to-safepoint (request accepted to every counted mutator parked or taken
over, HotSpot's "Reaching safepoint"), the take-over's post-quota work
(`xt_post_quota_us`: the frozen peers'
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
`xt_pass_us`  is the take-over passes' own
time (OS suspends, register and stack copies, the word probe). The passes run
inside the barrier wait, so it is PART of `ttsp_us`: `ttsp_us - xt_pass_us` is
the time spent waiting for polling threads. It is 0 exactly when
`xt_passes=0`.
`gc=N` is the VM's collection ordinal — the same number as the
`-Xlog:gc` line's `GC(N)` and the JFR events' `gcId`, 0-based (gci i12/ge a G1
concurrent cycle takes its own id at its initial mark, so later G1 ids are one higher per
cycle, as on HotSpot G1); the
collectors' own per-collection lines count separately (Generational's
`cycle=` is its 1-based minor-collection count), so pair them by adjacency,
not by number. `ttsp_us` is on neither `-Xlog:gc` nor JFR (HotSpot reports
it under `-Xlog:safepoint` and `jdk.SafepointBegin`, neither of which this
VM emits yet), and the `-Xlog:gc` / JFR duration, like HotSpot's, excludes
it. The printed `collect_us`, the `-Xlog:gc` duration and the JFR duration
are one instant, stamped inside the pause.
`straggler=` names the thread this pause waited for last, or `none`. The
non-collection pauses print the same line with `gc=-`, a `door=` naming the
pause (`g1-remark`, `g1-initial-mark`, `gen-remark`, `gen-initial-mark`,
`zgc-mark-start`, `frame-trace`, `heap-dump`, `loop-exit` — the pause that
makes an OSR'd loop leave compiled code when a JVMTI agent or a debugger
needs the interpreter) and `work_us=` in place of
`collect_us=`, so every entry of the exit `[GC] ttsp:` aggregate has a
per-pause line.
Not yet on the line: the split of `xt_post_quota_us` into the frame walk and
the helper-window pass.
JFR safepoint events and `-Xlog:safepoint` were declined for the GC-common
round.

**The take-over licence: one contract, three consumers .**
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
| Generational (`backend_can_pin = false`) | moving young (Cheney copy) | never returned: a Cheney copy cannot honour a pin, so any take-over pause is `NonMoving` | the whole young cycle diverts to the non-moving sweep | `gen_heap.rs` young divert, `takeover_forbids_unpinnable_move` (always) |
| G1 (`true`) | evacuate | evacuate; the regions the scanned words name are pinned out of the collection set | default: evacuate on the same pins (an unread peer, or a pin set that is incomplete or exact-base only, is then not covered); `CRATONVM_G1_TAKEOVER_LICENCE=1`: refuse evacuation for the pause | `g1.rs` young/mixed pause, `g1_takeover_licence_refusal` (opt-in until its refusal rate is measured) |
| ZGC (`true`) | slide (other coverage reasons permitting) | no slide: a take-over marks `XT_TAKEOVER` / `XT_HELPER_WINDOW`, which page pins cannot cover (`UNPINNABLE_COVERAGE_REASONS`, measured) | no slide, and no page-pinned relocation either | `ZgcRealHeap::coverage_incompleteness_is_page_pinnable` (always, as an extra refusal next to the coverage mask) |

So the three backends answer "is a frozen peer safe?" from the same record;
they differ only in how much a `MovePinned` licence buys them. Generational and
ZGC never move under a frozen peer; G1 does, protected by region pins.
A blocked peer's captured native-stack words pin too (`CRATONVM_G1_PINS_PEER_STACK_WORDS`), except a
word naming a region handed out after that peer blocked, which is dropped from the pause's captures
(`CRATONVM_G1_PEER_PINS_SKIP_POST_BLOCK_REGIONS`); where they pin whole regions, a region is pinned
only when one of the words is an object's start, the one word the wake rewrites
(`CRATONVM_G1_PEER_PINS_OBJECT_STARTS_ONLY`); cleanup, which only frees, ignores peer words
(`CRATONVM_G1_CLEANUP_IGNORES_PEER_STACK_WORDS`). All default ON, `=0` kills.
(Generational's pinned young copy, default ON, also moves
under a blocked peer's helper window: `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`,
default ON , `=0` kills; and a Generational take-over pause
waits 10 ms rather than 200 us before it freezes a peer, see
`CRATONVM_XT_FIRST_PASS_GRACE_US`. A pinned in-place cycle that the take-over
or the unproven-frame arm took keeps the legacy all-young seed for its Phase 5
major; every other in-place cycle's major seeds young from the true roots,
`CRATONVM_GC_MOVING_MAJOR_IN_PLACE_PER_CYCLE_ARMS`, default ON ,
`=0` refuses P-C whenever either arm's switch is on.) How often a frozen compiled peer degrades a
pause's licence was measured in verification batteries: in about
0.14 % of take-over pauses (76 of 52 557 over the d7 battery's
`[GC] xt_peer_scan:` lines); only a probe built to freeze a spinner goes above
1 %. The helper-window discharge verdict is a row of the pause ledger, like
the other take-over counts, so it is per VM.

**Several VMs in one process (embedders, the `vm` test binary).** The
per-pause take-over and coverage rows in `gc_quiescence.rs` are fields of each
VM's `PauseLedger`: the take-over verdict, the peer capture
buffers, and  the six moving-young coverage rows and the
helper-window pins (`CoverageCycle`). A write from a thread bound to no ledger
lands in process-wide orphan rows that every read also consults, so it is
never lost.  the young pin-word ledger is per VM too
(`PauseLedger::young_pins`); an unbound young pin-word write is dropped and
counted, and fails the open pause's read (fail-closed), so no per-pause row is
a process static any more
(`common-a-process-global-gc-coordination-state`,
fixed pending one `orphan_coverage_writes=0` probe run). Since
  a pause takes a process-wide **coverage slot**
(`COVERAGE_SLOT_OWNER`, `vm/src/threading/gc_barrier.rs`) before its barrier
lock and the per-pause reset, and `complete_gc` releases it, so another VM's
pause can no longer reset those rows under a pause that is using them. One VM
never waits (one compare-exchange per pause); a second VM's request waits for
the first's pause, outside every lock, for at most 250 ms, and then pauses
without the slot (the pre-w6 behaviour), counted and reported under
`CRATONVM_DBG_STW_CENSUS=1` as `[stw-coverage-slot] … gave up`. The
allocation front end's state is per VM since the same wave :
`ThreadMXBean.getTotalThreadAllocatedBytes` sums only this VM's threads
(`HeapRealm::thread_allocated_total`), and the TLAB refill wedge breaker's
counters (`HeapRealm::tlab_wedge`) cannot be moved by another VM's
refills. The JMX GC beans' counts, times and notification queues are per heap
too, and so is the `System.gc` coalescing counter
(`HeapRealm::system_gc_collections`, gc-common w7-g). One thing cannot be per
VM: the **reference width**. `-XX:+UseCompressedOops` (opt-in, Generational
only) switches `cratonvm_types::narrow_oop` on for the whole process, so every
VM in it runs 4-byte reference slots.  every VM's heap goes
through `compressed_oops::admit_heap` at init: the first VM fixes the width;
a later VM whose heap lies outside the narrow-oop window the first one fixed,
that did not ask for compressed oops, or whose collector is not audited for
them is **refused** (VM creation panics with the reason) instead of being
handed that window unchecked; and once any VM runs wide, a later
`-XX:+UseCompressedOops` falls back to 64-bit references with a message
(`vm_init.rs`, the `admit_heap` call with `backend_audited` true only for
Generational; `common-w7f-compressed-oops-second-vm-skips-the-fit-check`,
FIXED  with its `vm_init` handoff applied).

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
 and, one saying a live JIT frame always forces the
sweep and the other saying it never does by itself.

**Native side tables and native pins.** A native that keeps per-object
state in a Rust table must not key it by a raw address or a bare identity
hash. The address moves; the hash is shared by the Nth object of every VM
and is reused inside one heap. Since the 8-byte object header (below) a
compact instance's hash is 20 counter bits plus 11 class-id bits
(`ObjectHeader::short_hash_value`, `types/src/heap_types.rs`), so two live
instances of ONE class share a hash every 2^20 (about a million) mints,
not every 2^31. .. 26) every such table
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
`gc_collection_count` epoch, or compare the content it describes, or key
the row by the weak lock key (gc-common w33-a / w34-a). The last such lookup,
the JIT's VarHandle plan, names the handle's address and VM 
(`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`,
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
cycle certain to take the non-moving young marker (an explicit `System.gc`
among them). That marker follows `metadata_pin` from young owners and seeds
it from old ones (`seed_metadata_pins_of_old_owners`). A young value on a
moving young cycle is still rooted.

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
  unpinned-local finder behind the  audit; it takes the files to
  scan and has no baseline.

The test mocks (`native-builtins/src/test_utils.rs`,
`native-io/src/test_support.rs`) panic on a read of a released pin.

**The 8-byte object header (dev, ; `658fddf3c` and its
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
(`SatbQueue::is_active`); the phase is not consulted, and the queue holds
the compiled-code pre-gate's count for exactly that window . Both are internal
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
whose referent survived ( — a slot the program nulled with
`Reference.clear` is no longer written back, and the row is retired so it is
never enqueued). Queue linkage uses the Reference's real `next` field. A
`Reference` object is never a synthetic GC root: an unreachable one is
garbage and is never enqueued, as on HotSpot. The one exception is a pending
queued `Reference` still held in a liveness-dead local of the collecting
thread's interpreter frame, which is kept so `r = new WeakReference<>(x, q);
System.gc; q.remove` delivers as it does in HotSpot's interpreter
(`roots.rs` step 22). Finalizable objects dead in a collection are
**resurrected** (evacuated/marked with their subtree) so `finalize` runs
against valid memory; every GC door queues them through the one shared pause.
An object becomes finalizable however it was created: `new`, the JNI
`NewObject*` family, and  `Object.clone` of a
finalizable object (HotSpot's `JVM_Clone` registers the clone;
`tools/probes/CloneFinalizeProbe.java`), and `Finalizer.register` registers a
finalizer row, not a cleaner one., Generational's old-generation collectors resurrect
dead finalizables too. The stop-the-world ones (`major_gc_finalizing`,
`sweep_old_gen_non_moving_finalizing`) run an `OldGenFinalizerPass` round
after their root closure. The concurrent cycle's remark retains dead
published candidates, and the next pause reports them. Until then a
finalizable that died after promotion was freed without `finalize`, and
`tools/probes/OldGenFinalizeProbe.java`'s `System.gc` arm read 0/64; it now
reads 64/64. The probe's allocation-driven arm still reads 0/64. That run
performs no old-gen collection of any kind, because old gen stays at 0.5 %
occupancy, so the promoted objects are never judged. HotSpot shows the same
result whenever it promotes them (Serial `-Xmn32m`: 0/64). Default G1 passes
only because its large young generation never promotes them. HotSpot Serial
at the page's own configuration prints 64/64 for the same reason, and 0/64
at CratonVM's 32 MiB young size, so the page was retired as not a defect
(`common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED`,
gcd d1/c).
The same rounds report the depth-2 resurrection closure
(`VmHeap::take_resurrection_closure`) on Generational, so a weak reference to
an object reachable only through a finalizable is cleared, as HotSpot does.
G1 (Phase 3.5's fresh evacuations) and ZGC (the resurrection pass's marks)
report it too.
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
`System.gc` and `Runtime.runFinalization` wait for it (at most 2 s, less
if it is seen blocked on the caller's own monitor); a lock-holding caller then
returns whether or not they ran, and never runs them itself. Under
`--jdk-only`  the `ReferenceQueue` wake-ups of every
collection go to that daemon by default too (setting unset; `=0` still records
none) — the JDK's own `ReferenceQueue.remove` sleeps on its lock and nothing
else wakes it — and
`Reference.waitForReferenceProcessing` waits for the daemon under the full
hand-off (`false` otherwise); `--compatible` is unchanged. The per-setting
table is `docs/gc-tuning.md`, "Finalizer and reference delivery". Under G1,
references whose referents die in uncollected Old regions are processed at
concurrent-mark completion against the mark bitmap (INT-8: referent-slot hiding
during marking + a `Reference.get` keep-alive barrier + remark-time
processing callback). `System.gc` runs a finalizer-aware collection and also
drives the G1 concurrent cycle forward.

**What the dispatcher adds to every collection .** Both public
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
  `collector_decision_report` and its histogram are no longer empty on the
  default backend;
* (unchanged) bump the FFM epoch and splice GPU keep-alive roots.

**SoftReference pressure on G1 (fixed ).** The LRU soft-reference
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
internal records).

`System.gc` (`force_gc_from_native`) now **retries** when it loses the STW
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

Concurrent `System.gc` calls **coalesce**,
by HotSpot's `skip_operation` rule, per VM: a caller reads the VM's
`System.gc`-door collection count before its first attempt, and after a
LOST attempt returns as soon as one such collection has completed since —
N threads calling together see fewer `door=system-gc` pause lines than calls,
and `getCollectionCount` moves more slowly under contention (H2's
`while (prev == cur) System.gc;` still terminates: someone collected). A
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

**Verified against HotSpot, and where it is not** (re-measured  by
the baseline verification battery, dev `aa58f3a2e`, JDK 25; each probe
once per backend and once on HotSpot). Matching HotSpot's result line on all
three backends: `FinalizeOnceProbe`, `ReachableFinalizeProbe`, `RefProbe`,
`G1ChurnPauseProbe`, `HumongousChurn`, `MtChurnProbe`,
`ConcurrencyUnderGcSweep`, `RandomLiveAcrossGc`, `SteadyChurnRecreation`,
`ObjectStreamLookupGcRaceProbe` (though it takes 44–51 s on each backend
against well under a second on HotSpot, not yet attributed), `HeapUsedDelta`,
`SkipSpanRetireProbe`, `MtShrink`. The `ObjectStreamLookupGcRaceProbe`
timing has *not been re-measured* since.

The rows that did **not** match on that baseline, and where each stands on
 :

| probe | backend(s) | baseline  | now |
|---|---|---|---|
| `ThreadLocalFinalizeProbe` | all three | `live=11`, `PROBE-FAIL` after 30 s (HotSpot `live=0`) | **fixed**: a dead thread's `ThreadLocal` values are released at thread exit; `PROBE-OK` |
| `GcOverheadProbe -Xmx128m` | G1 | process abort (`FATAL: G1: out of heap space … cannot report failure`) | **fixed**: rc=1 with an uncaught `OutOfMemoryError`, as HotSpot |
| `XmsProbe -Xms64m -Xmx512m` | all three | `total=512m` (HotSpot `total=64m`) | **fixed **: see the `committed_bytes` paragraph above for the two residuals |
| `RefCheckOld` | Generational | `finalized=32/32` | matches, but only because `System.gc`'s non-moving young sweep keeps its finalizables young; `OldGenFinalizeProbe`, which promotes them, finalizes **0/64** (open, see java.lang.ref above) |

Historical test failures are also fixed:
**class-loader unloading** now happens in the shape
`vm/tests/class_loader_unload_regression.rs` exercises (verified passing:
`roots.rs` step 22 no longer roots every queued `Reference`, and the
`Class$Atomic` side store is loader-conditional), and **the JDK `Finalizer`
thread no longer dies** on a `null` from `ReferenceQueue.remove`
.

Three probes regressed on dev.
Two pass on Generational in verification batteries : `GcNotificationThreadProbe --compatible` receives its
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
   deleted  along with the simulated `NumaAllocator`; a real
   deduplication table would have to participate in every collector's remap
   and purge protocol. **There is no NUMA-aware placement** either: the
   topology probe (`gc/src/numa.rs`) feeds one `tracing::trace!`, Windows is
   always one node, and nothing binds memory to a node.
3. **Class metadata ("Metaspace") is bounded only by `-XX:MaxMetaspaceSize`,
   and only approximately.** Since  the launcher honours the flag
   (per VM; unset = unbounded, as before): a class defined from raw bytes
   (`ClassLoader.defineClass*`, `Lookup.defineClass` / `defineHiddenClass`,
   `Unsafe.defineClass`) that would exceed it first runs one full collection —
   class unloading gives a dead loader's charge back — and is then refused
   with `OutOfMemoryError: Metaspace`. That collection is NOT a `System.gc`
   (gc-common w7-g, `force_gc_for_class_metadata`): the defining thread
   holds the class-loading lock, so it runs no finalizer, Cleaner or
   GC-notification drain and never waits for the delivery thread; the work the
   pause queued is handed to the next GC door. What is charged is the CLASS-FILE size
   of every class not defined by the bootstrap loader
   (`ClassManager::metaspace_charged_bytes`) — an under-estimate of HotSpot's
   metadata, so a limit that fits there fits here and a leak hits it a little
   later; class-path loads of the built-in loaders are charged but never
   refused; `-XX:MetaspaceSize` is still ignored. The
   serviceability heap summary and the JMX non-heap `MemoryUsage`
   (`MemoryMXBean.getNonHeapMemoryUsage`) both report the same MEASURED
   lower bound  (class records, constant pools, method tables
   and bytecode of the live classes; it falls when a loader is unloaded), with
   `committed == used` (no reservation stands behind it) and `max = -1`. Not
   counted: interned strings, vtables, resolution caches, JIT code.
   The JMX memory-pool and garbage-collector beans are HotSpot's for each
   collector shape, with real usage and GC notifications: Serial's on
   Generational,
   G1's and ZGC's since gc-common w7-f : G1 `G1 Young
   Generation` / `G1 Old Generation` / `G1 Concurrent GC` over `G1 Eden
   Space` / `G1 Survivor Space` / `G1 Old Gen`, ZGC HotSpot 25's
   `ZGC Minor Cycles` / `ZGC Minor Pauses` / `ZGC Major Cycles` /
   `ZGC Major Pauses` over `ZGC Young Generation` / `ZGC Old Generation`
   (the gcd d10/o retirement, ; it was JDK 21's
   non-generational `ZGC Cycles` / `ZGC Pauses` over `ZHeap`, names no JDK
   25 HotSpot answers; the ZGC state lives in
   `HeapRealm::zgc_gc_beans`, applied at the wave-7 merge; the historical
   single `G1 Young Generation` bean with `-1` pools is gone on every
   backend). See the `-Xlog:gc`, JMX row of the diagnostics table below.
4. G1's **young** evacuation is parallel **by default**, on a persistent
   worker pool, and  so is the **mixed** pause:
   `mixed_collection` dispatches to `mixed_collection_parallel` unless
   `CRATONVM_G1_PARALLEL_MIXED=0` (a `System.gc()` or allocation-failure
   compaction stays serial). `[GC] g1 mixed-route: serial=<n>
   parallel=<n>` (printed unconditionally, `gc_metrics.rs`) is the census
   that says which arm a run took. It was serial by default because on a
   mixed pause whose copying is the old half — an old live set reached
   through the remembered set — **100% of the bytes are copied in Phase 2,
   by the driver thread alone**, and this lever parallelises Phase 3. A
   mixed pause also copies the whole young generation, though: on
   `G1epKeepRounds 10 -Xmx1g`'s all-mixed mode the serial body spent 1.66 s
   of closure on 279 MB, nearly all young, and those runs took 44 s against
   9.5 s on the parallel route (the g1-young-evacuation known-issue page).
   See internal records
   for the measurement and
   internal records
   for the capability gap this replaced.

   Two claims this entry used to make were false in both halves, which is
   why they are spelled out here. The switch is `CRATONVM_G1_PARALLEL_EVAC`
   and it is `on_unless_zero` (`types/src/flags.rs`) — an opt-**out**, never
   an opt-in. (Since the  flag groups, `CRATONVM_GC=g1-parallel-evac`
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
   gate). The ones with a visible symptom :
   * On Generational, recovering from an `OutOfMemoryError` after the JIT
     built the dropped chain: `Gcd1ThreadExitSpillProbe` passes 2 of 9 runs
     and `Gcd1PinnedCalleeOomeProbe` none of 12; `NativeGrowthReclaimProbe`
     (the open dev regression under "Current correctness state") fails 6/6.
     The census of the first two names the true-root major's
     fallback to the legacy young seed on promoting cycles .
   * On Generational, a heap-full program can livelock on young cycles that
     all fall back to the non-moving sweep (1 of 12 default runs of
     `GenR4W4HeapFullThrashProbe`), and `GenR4W5ThreadsOomProbe` takes
     200-320 s where HotSpot takes about 8 s.
   * On Generational with the JIT on, young cycles rarely copy, so young
     fragments and collects several times more often than HotSpot Serial
     (`GenR4W4EvacThroughputProbe` 17.9-23.1 s against about 3 s).
   * `Thread(group, target, name, stackSize)` and `-Xss` size the carrier
     now, but compiled self-recursion is still capped at 4 MiB, so a deep
     recursion that HotSpot runs on a large-stack thread overflows.
   * A JNI native that blocks in C no longer holds every pause: all three JNI switches are default ON (see
     "JNI and threads in native" below; `CRATONVM_JNI_NATIVE_TRANSITIONS=0`
     restores the old behaviour).
   * With the JIT on, an allocation-driven collection on **G1** can keep a
     weakly-reachable referent (cause 2 of
     `common-w4o-allocation-driven-collections-do-not-clear-weak-referents`;
     the JIT-owned cause 1 is fixed and measured on Generational).
   * `-Xms` is not a floor for ZGC's decommit (the `committed_bytes`
     paragraph above).

The STW barrier and class-unloading drivers run across all backends. For current class-loader liveness and reclamation contracts, see [Class-loader unloading and bounded metadata](architecture/class-loader-unloading.md).

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

> **Configuration note:** Every per-flag
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
> internal records.

| Switch | What it does |
|---|---|
| `--verbose:gc` | **G1:** per-pause `[GC-STAT]` lines + exit `[GC-SUMMARY]`. **ZGC:** one `[GC] zgc-real:` line per collection. **Generational** **: one line per collection, `[GC] generational: cycle=N kind=moving\|non-moving\|refused[+major] young=A->B old=C->D pause=X.XXXms [divert=<reason>]`. `kind` comes from `gc_metrics::last_collector_decision`, i.e. from the branch that decided, not a re-derivation; `refused` is a cycle that did not collect at all; `divert=` names the `divert_non_moving` term that refused the copy. ** A `kind=refused` line carries `refusal=<reason>` instead of `divert=` — nothing was diverted, neither collector ran — and the reason names which of the three refusal causes fired (`skipped-young-to-space-undersized`, `skipped-young-walk-incomplete`, `skipped-young-reserved-tlab-tails`). Before that date this arm emitted an affirmative at `info` level and then no per-collection output for the rest of the run — while the *shutdown* `[GC] …` census did run, which the round-1 write-up overstated . All three backends print the shutdown census, and ** the backend-independent `[GC] pause: gc=N door=… ttsp_us=… straggler=… xt_pass_us=… xt_post_quota_us=… collect_us=… xt_…` line after every collection (non-collection pauses: `gc=-`, `work_us=`; see the take-over paragraph above) |
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
| `CRATONVM_GC=-g1-tlab-clamp` *(alias: `CRATONVM_G1_TLAB_CLAMP=0`)* | Restore the pre- behaviour: `refill_tlab` REFUSES a request larger than half a region instead of clamping it to that bound. The refusal was a one-way cliff — `tlab::TlabPressureTracker` doubles a hot thread's request up to `MAX_TLAB_SIZE` (1 MiB) and the region-size ergonomic gives 1 MiB regions up to a 2 GiB heap, so the ladder's last rung is over the bound, and the sizer only re-sizes on a refill that actually happened. A thread that reached it fell back to per-object allocation through the exclusive guard for the rest of its life. `[GC] g1 tlab: oversize_clamped=` is the engagement counter; a zero means the ladder never climbed that far on your workload. |
| `CRATONVM_GC=-g1-humongous-best-fit` *(alias: `CRATONVM_G1_HUMONGOUS_BEST_FIT=0`)* | Restore first-fit for the humongous contiguous-run search. By default it is best-fit — the SHORTEST run of free regions that still fits, ties to the lowest index — so a small humongous object spends a tight hole instead of eating the head of a long run. G1 has no compaction pass that can manufacture a run back, so a long run consumed by a request that would have fitted in a hole is gone until the regions above it are freed. Both arms return a run of exactly `count` adjacent Free regions, so this is a placement policy, not a correctness switch; first-fit stops at its first hit while best-fit always scans the table, and `[GC] g1 free-scan: contiguous(...)` prices both. |
| `CRATONVM_GC=g1-heap-resize` *(alias: `CRATONVM_G1_HEAP_RESIZE=1`)* | **Opt-in.** The adaptive heap-sizing policy: a *soft capacity* between `-Xms` and the `-Xmx` region grid, grown when GC overhead exceeds 7.7 % (G1's `GCTimeRatio=12`) or less than `MinHeapFreeRatio` (40 %) of it is free, to the capacity that leaves 40 % free; shrunk after four consecutive pauses with more than `MaxHeapFreeRatio` (70 %) free and < 1 % overhead, to the capacity that leaves 70 % free; at a Remark, from that census alone; starts at `-Xms`; honours `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio` (gci i13/ef; before it: 5 % overhead / 70 % occupancy to grow, 40 % occupancy to shrink, to 60 %). The capacity is the denominator of the free-fraction trigger AND the floor for the trailing-Free-run shrink, which this flag also moves from `cleanup` to the end of every evacuation pause (gated on phase Idle, SATB inactive and an empty gray set, the same gate `eager_reclaim_early_decline` computes). Without it, `-Xmx` is a one-way ratchet on RSS for any workload that never crosses IHOP, because `cleanup` is the shrink's only caller. Watch `heap_grows` and `heap_shrinks` on the `[GC-STAT]` line: two large, nearly equal counts are the hysteresis being too weak. |
| `CRATONVM_GC=g1-pause-cost-model` *(alias: `CRATONVM_G1_PAUSE_COST_MODEL=1`)* | **Opt-in.** Make the mixed-CSet copy budget price the pause it is actually buying. Two terms: the YOUNG half of the collection set is charged first (every Eden and Survivor region is in the CSet unconditionally, so `max_gc_pause_ms` was spending its whole goal on the *second* copy of the pause), and each old candidate is charged the MARGINAL fix-up cost it causes — `fixup_ns_ema / fixup_regions_ema` times the walk regions it adds (its to-space destination, plus its remembered-set sources that no already-selected candidate has paid for). Before this, `fixup_ns_ema` was a constant with respect to the very choice being made, so the model under-estimated in the direction that overruns. Both terms are subtractions from the budget, so arming it can only make a mixed collection set SMALLER — safe for the pause goal, and a throughput question for the mixed phase, which is why it is opt-in. The selector still takes one region unconditionally for forward progress. Since gci i11/gm the marginal fix-up price is charged on every production mixed pause (one selector, `choose_old_cset`); before, only the test hook charged it. |
| `CRATONVM_GC=-g1-humongous-run-guard` *(alias: `CRATONVM_G1_HUMONGOUS_RUN_GUARD=0`)* | Restore the old hinted first-Free-region scan for an explicit performance A/B. By default, `claim_free_region` preserves the longest Free run: it prefers a Free region outside that run and otherwise takes its END. That prevents a single-region Old or humongous-start claim from splitting a span a later humongous allocation needs. Placement only: both arms return a Free region and neither can fail where the other succeeds. The default pays a full table scan under the exclusive guard; `[GC] g1 free-scan: single(...)` prices both arms, and `[GC] g1 humongous: requests= failures= free_at_failure= longest_run_at_failure=` is the outcome census. |
| `CRATONVM_GC=-g1-eager-humongous` *(alias: `CRATONVM_G1_EAGER_HUMONGOUS=0`)* | Restore cleanup-only humongous reclaim. By default an evacuation pause also frees humongous spans it can prove nothing references. This is the only path that frees memory outside the collection set, so it is the first thing to rule out if a live humongous object goes missing. |
| `CRATONVM_GC=g1-young-pause-target` *(alias: `CRATONVM_G1_YOUNG_PAUSE_TARGET=1`)* | **Default ON** (`=0` turns it off). Let `max_gc_pause_ms` bound the YOUNG generation too, not just the old half of a mixed collection set: G1 also collects once the Eden+Survivor region count reaches an adaptive target, tightened by 20% after any PRODUCTIVE pause that overruns the goal and relaxed while pauses stay under half of it. Does nothing until such an overrun is measured (the target starts at its 60%-of-regions ceiling and a target at the ceiling is not a trigger). Measured trade on `G1ChurnPauseProbe` at `-Xmx2048m`: p50 -21%, p99 +3%, wall +4.2%, one extra pause — see the young-sizing paragraph under Backend details for the full table and the measurement that preceded the flip. |
| `CRATONVM_GC=g1-young-target-net-of-pins` *(alias: `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS=1`)* | **Opt-in (gci i7/g1y).** The young-target arm counts the young generation NET of the regions the last young/mixed pause kept out of its collection set under a pin (`jni_pinned_out + jit_pinned_out`); without it a pause-target at its floor plus ~20 pinned young regions fires a young pause every few Eden regions (G1 4-thread javac -Xmx512m: ~2 800 pauses, timeout). Rows: vi7-g1y J2. |
| `CRATONVM_DBG=g1-verify-unbounded-on-verbose-gc` *(alias: `CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC=1`)* | **Opt-in (gci i7/g1y).** `--verbose:gc` selects the whole-heap CSet verifier again, as . Default: `--verbose:gc` runs the same budgeted sampler as every other release run. |
| `CRATONVM_GC=g1-young-target-net-of-heap-costs` *(alias: `CRATONVM_G1_YOUNG_TARGET_NET_OF_HEAP_COSTS=1`)* | **Opt-in (gci i8/gs).** The young pause-goal loop is fed the pause minus `verify_us` and the whole-heap `fixup_us`, the two phases that scale with the old generation, so a large old generation cannot drive the young target to its floor. Rows: vi8-gs J. |
| `CRATONVM_GC=g1-young-target-futile-below-region` *(alias: `CRATONVM_G1_YOUNG_TARGET_FUTILE_BELOW_REGION=1`)* | **Opt-in (gci i8/gs).** A young or mixed pause that freed less than one region resets the young target to its ceiling (the default resets only on a pause that copied and freed nothing). Rows: vi8-gs J. |
| `CRATONVM_GC=g1-free-trigger-net-of-pins` *(alias: `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS=1`)* | **Opt-in (gci i8/gs).** The young floor of the low-Free triggers counts young net of the regions the last young/mixed pause kept under a pin; the to-space headroom bound keeps the gross count. Rows: vi8-gs S. |
| `CRATONVM_GC=g1-fixup-skip-clean-pinned` *(alias: `CRATONVM_G1_FIXUP_SKIP_CLEAN_PINNED=1`)* | **Opt-in (gci i9/fa).** The parallel young pause's Phase-4 fix-up skips a pinned region that is wholly card-clean and unchanged by the pause; armed only with `CRATONVM_G1_CARD_RSET` and `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` on and `CRATONVM_G1_CARD_CLEAN` off. Census `fixup_skipped_regions` / `fixup_skipped_bytes` on `[GC] g1 pause-phases:`. |
| `CRATONVM_GC=g1-parallel-fixup` *(alias: `CRATONVM_G1_PARALLEL_FIXUP=1`)* | **Opt-in (gci i10/pp).** The evacuation pause's Phase-4 fix-up walks its regions on the G1 evacuation pool (remembered-set edges merged by the driver in region order, as the serial walk applies them); not for a walk that owes the humongous census. Census `fixup_parallel_walks`. Rows: vi10-pp A. |
| `CRATONVM_GC=g1-pin-skip-above-cursor` *(alias: `CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR=1`)* | **Opt-in (gci i10/pp).** A JIT pin word or non-object root strictly above its Eden / Survivor / Old region's allocation cursor no longer keeps that region out of the collection set (it names no allocated byte). Census `pin_words_above_cursor`. |
| `CRATONVM_GC=g1-mixed-hotspot-choice` *(alias: `CRATONVM_G1_MIXED_HOTSPOT_CHOICE`)* | **Default ON  (gci i10/rh), `=0` kills.** HotSpot G1's mixed choice: candidate live bytes are the cursor minus the garbage fixed at cleanup, each mixed pause takes at least `ceil(candidates / G1MixedGCCountTarget)` old regions, the phase ends on no candidate or the strict `G1HeapWastePercent` floor (not after 8 pauses). `[GC] g1 mixed-live:` counts copies into marked Old regions on both arms. Rows: vi10-rh M. |
| `CRATONVM_GC=g1-mixed-count-skips-compaction` *(alias: `CRATONVM_G1_MIXED_COUNT_SKIPS_COMPACTION=1`)* | **Opt-in (gci i11/gm).** A full or allocation-failure compaction pause inside a mixed phase no longer spends one of its `G1MixedGCCountTarget` pauses. `[GC] g1 gm-census: compaction_in_mixed_phase=`. Rows: vi11-gm M3. |
| `CRATONVM_GC=g1-eager-reclaim-during-mark` *(alias: `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK`)* | **Default ON  (gci i11/gm), `=0` kills.** Eager humongous reclaim also runs while a marking cycle is open, for primitive-array spans only (HotSpot's `typeArray` rule); declines a pause on which a marker holds popped work or SATB has pending entries. `[GC] g1 gm-census: eager_mark_*`. Rows: vi11-gm EM. |
| `CRATONVM_GC=g1-full-compaction-skips-mark-drive` *(alias: `CRATONVM_G1_FULL_COMPACTION_SKIPS_MARK_DRIVE=1`)* | **Opt-in (gci i11/go).** The compaction pause that ends a G1 full collection skips the `alloc_fail` mark-cycle drive, so no concurrent cycle opens right after a full collection (HotSpot starts none). Row: vi11-go C. |
| `CRATONVM_GC=g1-prepare-mixed` *(alias: `CRATONVM_G1_PREPARE_MIXED=1`)* | **Opt-in (gci i11/go).** HotSpot's `Prepare Mixed`: the first pause after a cycle's cleanup is young-only (`Pause Young (Prepare Mixed)`), mixed pauses start with the next. Row: vi11-go P. |
| `CRATONVM_GC=g1-full-gc-one-report` *(alias: `CRATONVM_G1_FULL_GC_ONE_REPORT=1`)* | **Opt-in (gci i12/ge).** A G1 full collection (`System.gc`, `jcmd GC.run`, live inspection / dump) is reported as HotSpot's ONE `Pause Full (<cause>)`: its young pause, marking cycle and compaction fold into one `G1 Old Generation` collection, one `GC(n)` line and one JFR old event (`--verbose:gc` still prints each pause); `jdk.GarbageCollection.longestPause` follows. |
| `CRATONVM_XLOG_REFUSE_INVALID_SELECTION=0` | **Default ON (gci i27/ob), `=0` kills.** The launcher refuses an `-Xlog` option HotSpot 25 refuses (an unknown level or tag name) with HotSpot's lines and exit status 1; `=0` restores the warn-and-run tolerance. |
| `CRATONVM_G1_CONCURRENT_START_FROM_DRIVE=0` | **Default ON (gci i27/ob), `=0` kills.** A G1 young pause is labelled `Pause Young (Concurrent Start)` (`-Xlog:gc`, `jdk.G1GarbageCollection.type`) only when its requester's mark-cycle drive opened the cycle; its report is printed just before the cycle's `Concurrent Mark Cycle` line, or as `Normal`. `=0`: the in-pause prediction labels it. |
| `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE=0` | **Default ON (gci cs48), `=0` kills.** HotSpot 25's `decide_on_concurrent_start_pause`: a G1 marking cycle opens only after a young-only pause, never right after a `Pause Young (Mixed)` (the next young-only pause opens it and is the `Concurrent Start`); and `System.gc` / `jcmd GC.run` under `-XX:+ExplicitGCInvokesConcurrent` with no cycle open take their own young pause as the forced `Pause Young (Concurrent Start)`, open the cycle whatever IHOP says, and run no compaction pause after its cleanup. `=0`: a cycle opens after any pause the IHOP drive follows, and the user-requested door ticks IHOP and compacts. Rows vi27-ob CS1, vi28-dg E4. |
| `CRATONVM_G1_RESERVE_RECLAIM_CREDIT=0` | **Default ON (gci nd48), `=0` kills.** A small fallible G1 allocation (every allocation ladder's retry: `new`, the native doors, the JIT helpers) at the TLAB reserve may claim a Free region when collections have given back half a region of bytes per such claim since the reserve last refused one, and the claim leaves one Free region; on a heap filling with live data no credit is earned and the reserve stays for the infallible callers. `=0`: the reserve refuses every claim. Rows vi48-x `zl2_a2_g1_`. |
| `CRATONVM_G1_FULL_DOOR_SURVIVOR_STAMP_IN_PAUSE=0` | **Default ON (gci i41/rs), `=0` kills.** The G1 full-collection door's re-stamp of `G1 Survivor Space`'s collection usage (empty, as HotSpot's one `Pause Full` leaves it; gci i30/ob) is taken inside the door's own full-compaction pause, before the world is released, and only by a pause that ran that compaction on a full-collection door; `=0` restores the stamp at the door's end, after the release, where another thread's young pause in between had its Survivor figure overwritten by the door's late zero. Rows vi41-rs S, R. |
| `CRATONVM_JFR_GC_CONFIG_HOTSPOT=0` | **Default ON (gci i42/mx), `=0` kills.** JFR's periodic GC configuration events answer HotSpot 25's values: `jdk.GCHeapMemoryPoolUsage` one row per heap pool under its name, `jdk.GCConfiguration` HotSpot's GC thread flags per collector (Serial 0 / 0, G1 the `WorkerPolicy` formula) and the explicit-GC flags, `jdk.GCHeapConfiguration` and the `MaxHeapSize` / `InitialHeapSize` flag rows the `gc,init` heap sizes, the narrow-oop fields this process's state; `=0` restores the old rows (one `Java heap` pool row, CPU-count threads, `totalMemory` / `maxMemory`). Rows vi42-mx C. |
| `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS=0` | **Default ON (gci i43/lf), `=0` kills.** The launcher parses `-XX:SurvivorRatio`, `-XX:TLABSize`, `-XX:MinTLABSize`, `-XX:±UseTLAB`, `-XX:±ResizeTLAB`, `-XX:TLABWasteTargetPercent`, `-XX:TLABRefillWasteFraction`, `-XX:TLABAllocationWeight` and `-XX:TLABWasteIncrement` with HotSpot 25's ranges and `AfterMemoryInit` constraints (Serial / G1): a value HotSpot refuses prints HotSpot's lines and exits 1; legal values are carried on `VmConfig` (read by the collectors since gci i45/sc: `CRATONVM_GEN_SURVIVOR_RATIO_FLAG`, `CRATONVM_TLAB_HOTSPOT_FLAGS`; `TLABWasteIncrement` has no consumer). `=0`: the old silent `-XX:` catch-all. Rows vi43-lf R, A, K. |
| `CRATONVM_LAUNCHER_UNKNOWN_XX_REFUSAL=0` | **Default ON , `=0` kills.** With `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`, a `-XX:` name HotSpot 25 does not have on this OS is refused as HotSpot refuses it: `Unrecognized VM option '<argname>'` (and `Did you mean ...?` from HotSpot's fuzzy match, which scores `develop` flags too), or, for a `develop` flag, `Error: VM option '<Name>' is develop and is available only in debug version of VM.` and its spelling's line; exit 1. Obsolete flags (`Ignoring option ...`), the `CreateMinidumpOnCrash` alias (deprecation warning), `-XX:Flags=` / `-XX:VMOptionsFile=`, `-XX:+IgnoreUnrecognizedVMOptions`, and this launcher's own `-XX:` options (`AuditMissingNatives`, `UseGenerationalGC`, `MaxRAMFraction`, `PrintTenuringDistribution`: `LAUNCHER_OWN_XX_OPTIONS` in `vm-cli/src/main.rs`) are taken. The names come from `vm-cli/src/hotspot25_flags.rs`, `hotspot25_develop_flags.rs` (mined from the JDK by `tools/gen-hotspot25-develop-flags.py`) and the per-OS lists of `hotspot25_os_flags.rs`; the tables are x86-64 Linux / Windows ones, so on another CPU or OS the refusal stays off (an unknown name is dropped silently there, as with `=0`). |
| `CRATONVM_GEN_HOTSPOT_YOUNG_ERGO=0` | **Default ON , `=0` kills.** `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` print HotSpot 25's `[warning][gc,ergo]` lines (stdout, `-Xlog` decorators) for the collector HotSpot would run: Serial's `GenArguments` (`NewSize (...) is equal to or greater than initial heap size`, `MaxNewSize (...) is equal to or greater than the entire heap`, `Inconsistency between generation sizes and heap size`; `cratonvm_gc::gen_young_sizing::hotspot_serial_young_ergonomics`) on `-XX:+UseSerialGC` / `-XX:+UseGenerationalGC`, G1's `G1YoungGenSizer` on G1 and on a command line naming no collector (`NewSize (1331k) is greater than the MaxNewSize (0k)` for `-XX:MaxNewSize=0`). A Serial initial eden HotSpot's own bootstrap cannot fit in (at most 64 KiB on Windows; at most 128 KiB on Linux and 192 KiB on Windows under `-Xshare:off`; never on Linux with CDS on; measured, `hotspot_serial_boot_failing_eden`) ends the launch with `Error occurred during initialization of VM` / `GC triggered before VM initialization completed. Try increasing NewSize, current value <n>K.`, exit 1. The Generational collector's own `Warning:` clamp notes are `--verbose:gc` only. `=0`: those notes on stderr and nothing else, as before. |
| `CRATONVM_GEN_SURVIVOR_RATIO_FLAG=0` | **Default ON (gci i45/sc), `=0` kills.** A typed `-XX:SurvivorRatio` (validated by the launcher, gci i43/lf) sizes the Generational heap as HotSpot Serial: the Serial eden `Y - 2 * align_down(Y / (SurvivorRatio + 2))` (young trigger, age clock, followed initial eden), the `CRATONVM_GEN_SURVIVOR_POOL_RESERVE` survivor reserve `semi / (SurvivorRatio + 1)`, and the tenuring target's survivor capacity `eden / SurvivorRatio`. Nothing typed or 8: unchanged. `=0`: the constant 8. Rows vi45-sc O, A. |
| `CRATONVM_TLAB_HOTSPOT_FLAGS=0` | **Default ON (gci i45/sc), `=0` kills.** TLAB sizing on every collector (Generational and G1; ZGC since gci i48/zl3, zx48 A8) reads `-XX:TLABSize` (first desired size), `-XX:MinTLABSize` (floor), `-XX:-ResizeTLAB` (fixed size, no raise), `-XX:TLABWasteTargetPercent` (`target_refills = max(100 / (2 * pct), 2)`), `-XX:TLABRefillWasteFraction` (refill-waste limit), `-XX:TLABAllocationWeight` (average weight) and `-XX:-UseTLAB` (no buffer beyond the object: every allocation takes the shared path), per VM (`HeapRealm::tlab_sizing`). Nothing typed: the old sizes byte for byte. `=0`: the old constants whatever was typed. ZGC (gci i48/zl3): the refill site sizes the request as on the other heaps and `ZgcRealHeap::refill_tlab_at_least` carves a floored request below its 8 KiB chunk floor (a typed `MinTLABSize`, or the object alone under `-UseTLAB`); the launcher range-checks the ZGC values (`hotspot_space_tlab_geometry`, TLAB max 262144). Rows vi45-sc T. |
| `CRATONVM_JMX_SAFEPOINT_COUNTERS=0` | **Default ON (gci i44/sp), `=0` kills.** `VMManagementImpl.getSafepointCount` / `getTotalSafepointTime` / `getSafepointSyncTime` / `getTotalApplicationNonStoppedTime` (`sun.management.HotspotRuntime`) answer the GC barrier's safepoint accounting (`GcBarrier::safepoint_stats`: completed pauses, request -> completion ms, time-to-safepoint ms, ms between pauses) as HotSpot's `RuntimeService` does; `=0` restores the constant 0. Rows vi44-sp S. |
| `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0` | **Default ON (gci i46/lg), `=0` kills.** A pause HotSpot 25 runs no safepoint for (loop-exit and inline-cache handshakes, a single thread's stack trace, Generational's concurrent-cycle pauses, G1's initial mark, marking pauses inside a G1 full collection) adds nothing to `getSafepointCount` / `getTotalSafepointTime` / `getSafepointSyncTime`, and `getTotalApplicationNonStoppedTime` runs on across it. Read once per VM. `=0`: every pause counts, as in i44/sp. Rows vi46-lg P. |
| `CRATONVM_JMX_MEMORY_MXBEAN_JDK_SINGLETON=0` | **Default ON (gci i46/hc), `=0` kills.** The platform `MemoryMXBean` the `getMemoryMXBean` / `getPlatformMXBean` natives answer is the JDK's `ManagementFactoryHelper.getMemoryMXBean` singleton -- the bean the platform MBeanServer registers -- so a memory-threshold notification reaches a listener added through the MBeanServer under `--compatible`, as on HotSpot; `=0` mints the VM's own `MemoryImpl`. Rows vi46-hc M. |
| `CRATONVM_JMX_GC_BEAN_LIST_HOTSPOT_ORDER=0` | **Default ON (gci i46/hc), `=0` kills.** `--compatible`'s `ManagementFactory.getGarbageCollectorMXBeans` lists the collectors in HotSpot's order (the JDK's `HashMap` of canonical object names: G1 Young, Concurrent, Old); `=0` keeps `MemoryImpl`'s manager order. Rows vi46-hc L. |
| `CRATONVM_JMX_FRESH_BEAN_STRINGS=0` | **Default ON (gci i46/hc), `=0` kills.** Memory pool and collector names, GC notification name / action / cause and the `GcThreadCount` attribute strings are fresh `String`s, as HotSpot's `create_from_str` builds them (`==` against a literal is `false`); `=0` interns them. Rows vi46-hc I. |
| `CRATONVM_XLOG_SAFEPOINT_LINES=0` | **Default ON (gci i45/sl), `=0` kills.** `-Xlog:safepoint` (`safepoint*`, `all`) prints HotSpot 25's info line per pause, `Safepoint "<VM operation>", Time since last: N ns, Reaching safepoint: N ns, At safepoint: N ns, Leaving safepoint: 0 ns, Total: N ns, Threads: 0 runnable, N total` (gci i47w/j45: HotSpot 25's shape; the i45 line carried JDK 21's `Cleanup:`), from the GC barrier's figures (application-time sum, time-to-safepoint) and HotSpot's operation names (`SerialCollectForAllocation`, `SerialGCCollect`, `G1CollectForAllocation`, `G1CollectFull`, `G1TryInitiateConcMark`, `CollectForMetadataAllocation`, `G1PauseRemark`, `HeapDumper`, `GC_HeapInspection`, `ThreadDump`, `RedefineClasses`); pauses HotSpot runs no safepoint for print none; `=0`: no line, as before. Rows vi45-sl S. |
| `CRATONVM_XLOG_FILE_ROTATION=0` | **Default ON (gci i46/lg), `=0` kills.** An `-Xlog:...:file=<name>` output follows HotSpot 25's `LogFileOutput`: at VM start an existing regular file is archived to `<name>.<n>` (`filecount`, default 5; the first free index, else the oldest archive's) or truncated (`filecount=0`), and the file is archived again once `filesize` (default 20M; `0` never) bytes were written; output options on an output that exists already print HotSpot's `Output options for existing outputs are ignored.` warning. `=0`: options ignored, every file appended to, as before. Rows vi46-lg R. |
| `CRATONVM_XLOG_SAFEPOINT_FRAME_TRACE_OPS=0` | **Default ON (gci i46/lg), `=0` kills.** A frame-trace pause's `-Xlog:safepoint` VM operation is the one its initiator's scope names: a single thread's `Thread.getStackTrace` is a handshake (no line, and no `sun.rt.safepoints` count), `jcmd Thread.print` is `PrintThreads`; no scope, `ThreadDump`. `=0`: `ThreadDump` always, as in i45/sl. Rows vi46-lg P. |
| `CRATONVM_XLOG_SAFEPOINT_AFTER_PARKED_REPORT=0` | **Default ON (gci i46/lg), `=0` kills.** The `safepoint` line of a G1 young pause whose `gc` lines wait for the requester's mark-cycle drive (the `Concurrent Start` label) is printed after those lines, as HotSpot orders them. `=0`: printed at the pause's finish, before them, as in i45/sl. |
| `CRATONVM_GC_GUARD_RING_ONLY_WARN=0` | **Default ON (gci i46/hb), `=0` kills.** The reclaim guard (`vm/src/memory/reclaim_guard.rs`) reports an old-gen or young freed-RING hit at `WARN` unless the free-list verdict fired or the caller proved the receiver corrupt (an impossible `clone` dispatch, a decoded corrupt cell): the rings are never retracted when memory is re-served, so a legitimate failing `checkcast` of a live object in a reused span printed `ERROR` lines on stderr (HotSpot: none). `=0`: every ring hit at `ERROR`, as before. Rows vi46-hb U, R. |
| `CRATONVM_GPU_VETO_TLS_GONE_FAILS_SAFE=0` | **Default ON (gci i46/ts), `=0` kills; feature `gpu-offload` only.** A collection whose collecting thread's per-cycle veto slot is gone (thread-locals torn down) still vetoes relocation after a timed-out GPU drain, through the process flag every cycle end clears; `=0` restores the fail-open answer (no veto: the cycle relocates). No shipped build enables the feature. Rows vi46-ts U-GPU. |
| `CRATONVM_JFR_GC_FLAG_CONFIG_HOTSPOT=0` | **Default ON (gci i43/tj), `=0` kills.** JFR's flag-valued GC configuration events answer HotSpot 25's flag values for this command line: `jdk.YoungGenerationConfiguration` HotSpot's `NewSize` / `MaxNewSize` / `NewRatio` (Serial's `GenArguments` arithmetic, G1 the flags with `Long.MIN_VALUE` max), `jdk.GCSurvivorConfiguration` the tenuring thresholds, `jdk.GCTLABConfiguration` and the `UseTLAB` row the launcher's TLAB flags, `jdk.GCConfiguration.gcTimeRatio` 12 on G1, and each flag row's `origin` (`Command line` / `Ergonomic` / `Default`); `=0` restores the old rows (a third of `totalMemory` / `maxMemory`, 15 / 7, the TLAB defaults, 99, every origin `Ergonomic`). Rows vi43-tj C. |
| `CRATONVM_GC=jvmti-force-gc-leaves-drains` *(alias: `CRATONVM_JVMTI_FORCE_GC_LEAVES_DRAINS=1`)* | **Opt-in (gci i27/ob).** JVMTI `ForceGarbageCollection` leaves the queued finalizers, cleaner actions and GC notifications to the next door that may run Java (HotSpot's `Finalizer` thread) instead of running them inline on the agent's thread. Rows: vi27-ob F3. |
| `CRATONVM_GC=g1-full-gc-aborts-open-cycle` *(alias: `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE=1`)* | **Opt-in (gci i13/gp, extended i15/ca).** A G1 full collection (and the OOME ladder's last-ditch cycle) that finds a cycle marking ABORTS it as HotSpot does: its own pause joins the marker and drops the cycle (no remark, reference processing or cleanup), only the full collection's fresh cycle marks, and the cycle reports as HotSpot's aborted cycle (`Concurrent Mark Abort`, no Remark / Cleanup, no `G1 Concurrent GC` count). Rows: vi15-ca O, C PASS; S VOID (no `System.gc` overlapped a cycle), re-shaped as vi16-ca. |
| `CRATONVM_GC=g1-metadata-concurrent-start` *(alias: `CRATONVM_G1_METADATA_CONCURRENT_START=1`)* | **Opt-in (gci i13/gp).** G1 answers a `-XX:MaxMetaspaceSize` refusal with `Pause Young (Concurrent Start) (Metadata GC Threshold)` and a cycle it does not wait for; the full collection runs only if the define still does not fit. |
| `CRATONVM_GC=g1-humongous-reserve-one` *(alias: `CRATONVM_G1_HUMONGOUS_RESERVE_ONE=1`)* | **Opt-in (gci i13/oa).** A fallible humongous array at the TLAB reserve is served when it leaves one Free region (HotSpot G1 has no reserve; the default refuses it `reserve` regions early). Rows: vi13-oa A. |
| `CRATONVM_GC=g1-oome-skip-futile-soft-rung` *(alias: `CRATONVM_G1_OOME_SKIP_FUTILE_SOFT_RUNG`)* | **Default ON  (gci i13/oa), `=0` kills.** The last-ditch reclaim skips its soft-reference young pause when an armed remark ran in its cycle and no active soft referent is young or humongous. Rows: vi13-oa E. |
| `CRATONVM_GC=g1-humongous-compaction-needs-reserve` *(alias: `CRATONVM_G1_HUMONGOUS_COMPACTION_NEEDS_RESERVE=1`)* | **Opt-in (gci i13/oa).** The fragmented-humongous compaction is armed only when the Free pool also covers the retry's reserve. Rows: vi13-oa F. |
| `CRATONVM_GC=g1-survivor-overflow-to-old` *(alias: `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD=1`)* | **Opt-in (gci i13/ef).** The serial and (gci i27/gs) parallel evacuators retry a Survivor copy that found no room in Old before it self-forwards (HotSpot's `allocate_copy_slow`); `[GC-STAT]` `survivor_overflow_promotions=`. |
| `CRATONVM_GC=gen-survivor-pool-reserve` *(alias: `CRATONVM_GEN_SURVIVOR_POOL_RESERVE`)* | **Default ON  (gci i27/gs), `=0` kills.** Generational: `Survivor Space` reports HotSpot Serial's survivor reserve (`semi / (SurvivorRatio + 1)`, 64 KiB aligned) as its max and init, and the young commit's reserve as its committed, carved out of `Eden Space`; the pools' max still sum to `maxMemory` and their committed to `totalMemory`. Rows: vi27-gs SP. |
| `CRATONVM_GC=gen-survivor-space-aligned` *(alias: `CRATONVM_GEN_SURVIVOR_SPACE_ALIGNED`)* | **Default ON  (gci i27/gs), `=0` kills.** Generational: the adaptive tenuring target's survivor capacity is aligned down to 64 KiB as HotSpot's to-space is (`Desired survivor size` 4456448 at `-Xms256m -Xmx256m`, HotSpot's). Rows: vi27-gs DS. |
| `CRATONVM_JIT=xt-takeover-drop-tail-words` *(alias: `CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS=1`)* | **Opt-in (gci i27/gs, d10t).** The take-over drops roots strictly inside a frozen peer's reserved TLAB tail whose memory reads unallocated (they name no object); a root at the tail's start and every root of a tail that holds anything are kept. Rows: vi27-gs XT. |
| `CRATONVM_GC=g1-pinned-young-as-survivor` *(alias: `CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR=1`)* | **Opt-in (gci i13/ef).** An Eden region a pin kept out of a young or mixed CSet is retyped Survivor at the end of the pause (not if a frozen peer's TLAB tail lies in it), so `Eden regions: a->0` as on HotSpot; `[GC-STAT]` `pinned_eden_relabelled=`. |
| `CRATONVM_GC=g1-young-survival-young-only` *(alias: `CRATONVM_G1_YOUNG_SURVIVAL_YOUNG_ONLY=1`)* | **Opt-in (gci i20/ys).** The young survival ratio behind G1's evacuation-headroom trigger is sampled only from young-only pauses, over the Eden handed out since the last pause plus the packed survivor footprint (default: every pause but a humongous compaction, over every young region counted full, so mixed / full-compaction old copies read as young survival). Rows: vi20-ys SV. |
| `CRATONVM_GC=g1-humongous-ihop-start` *(alias: `CRATONVM_G1_HUMONGOUS_IHOP_START`)* | **Default ON  (gci i20/ys), `=0` kills.** A G1 humongous allocation that would take the old occupancy (plus the request, in whole regions) over IHOP first runs a `G1 Humongous Allocation` young pause and drives the mark cycle, as HotSpot's `attempt_allocation_humongous` does. Without it `G1 Humongous Allocation` is reported only for a humongous allocation that failed. Rows: vi20-ys HI. |
| `CRATONVM_GC=g1-young-reserves-mixed-min-old` *(alias: `CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD=1`)* | **Opt-in (gci i22/sz).** On the HotSpot young model, Eden before a mixed pause is sized in what the pause goal leaves after the base AND the predicted copy of the mixed phase's minimum old collection set (`ceil(candidates / G1MixedGCCountTarget)` regions at their mean live bytes), as HotSpot 25's `calculate_desired_eden_length_before_mixed`; the young target is also re-sized at cleanup, when the phase turns mixed. Default: the young half takes the whole goal and the old half gets its own budget on top. Rows: vi22-sz MR. |
| `CRATONVM_GC=g1-mixed-waiver-base-trigger` *(alias: `CRATONVM_G1_MIXED_WAIVER_BASE_TRIGGER=1`)* | **Opt-in (gci i23/tn).** The mixed gate's `tight` waiver of the `G1HeapWastePercent` floor is measured against twice the BASE free trigger (50 % Free) instead of twice the live trigger, which evacuation failures ratchet to 50 % -- where the waiver held at any occupancy. Rows: vi23-tn MW. |
| `CRATONVM_GC=g1-weak-string-table` *(alias: `CRATONVM_G1_WEAK_STRING_TABLE=1`)* | **Opt-in (gci i21/sw).** G1: the String intern table is weak at the marking cycle -- the mark root scans leave it out, the remark judges it on the completed bitmap and drops the dead entries after a trusted cleanup, and every pool hit while marking is logged through the SATB keep-alive (`vm/src/memory/string_table.rs`). Evacuation pauses keep rooting it. Deciding rows `vi21-sw.list` (FLIP RULE). `CRATONVM_GC_WEAK_STRING_TABLE=0` also turns it off. |
| `CRATONVM_GC=jni-in-native-proven-depth-credit` *(alias: `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT=1`)* | **Opt-in (gci i24/bp, Generational).** An in-native compiled thread whose deposit proved its JIT depth is credited at the pause (depth into the coverage account, band words into the young pin ledger), so its band no longer forces `CROSS_THREAD_JIT_PEER` and the young cycle may still move through the pinned copy. Rows: vi24-bp S / E / C. |
| `CRATONVM_GC=dedupe-initiator-snapshot` *(alias: `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT=1`)* | **Default ON on Generational since the gcj j2 WARNING flip ; G1 / ZGC unchanged.** (gci i21/pr.) The collection door's registry gather leaves out the initiator's own root snapshot, which `collect_roots` step 11 already pushed (root set unchanged; a Generational pinned old-gen compaction stops pinning the initiator's precise frame roots through the duplicate). Rows: vi21-pr E / S / C. |
| `CRATONVM_DBG=g1-dbg-flagless-sweep` *(alias: `CRATONVM_G1_DBG_FLAGLESS_SWEEP=1`)* | **Diagnostic (gci i29/cv2).** Whole-heap header sweeps at pause start/end, remark start and cleanup end; prints every NEW object start without `GC_FLAG_HEADER` and the phase that produced it (gci-i29cv2). O(heap) per sweep. Rows: vi29-cv2 D/R. |
| `CRATONVM_DBG=g1-dbg-freed-slot-audit` *(alias: `CRATONVM_G1_DBG_FREED_SLOT_AUDIT=1`)* | **Diagnostic (gci i39/cv).** After every free (each young / mixed / drain `free_or_keep_cset`, and cleanup's in-place Old and dead humongous frees) walks the surviving heap and prints every reference slot that still names a region the free just reset, with `why=` (fixup-miss, rset-hole, rset-stale, card-hole, walk-skip, marked-holder) and a per-event summary. Reads only; O(heap) per free. Rows: vi39-cv F / Z. |
| `CRATONVM_DBG=g1-dbg-closure-root-entries` *(alias: `CRATONVM_G1_DBG_CLOSURE_ROOT_ENTRIES=1`)* | **Diagnostic (gci i41/hc).** At the G1 pause that queues a dead kept-region finalizable, prints `[g1-closure-root]` lines: the closure's members and, for every root entry / pin word / overlay row / outside slot that named a member, every VM structure holding that address (the initiator's `collect_roots` step, each thread's deposited snapshot with blocked / conservative / precise / slot origin and whether the thread published it as a JIT pin, compiled-frame provenance, native root sources, statics / class tables / strings / JNI globals, finalizer and cleaner queues). Also turns the `collect_roots` scan marks on. Reads only (`try_*` locks). Rows: vi41-hc C / D / P. |
| `CRATONVM_GC=old-post-close-incremental` *(alias: `CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL=1`)* | **Opt-in (gci i28/cl, Generational).** After a non-idle finalizer round the post-resurrection closure scans only the objects popped since the last closure (P3/cl); arms only while `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` is on. Rows: vi28-cl S / C. |
| `CRATONVM_GC=jni-in-native-band-narrow` *(alias: `CRATONVM_JNI_IN_NATIVE_BAND_NARROW`)* | **Default ON (=0 kills).** The in-native band's top is the outermost JIT entry's SP when the stack above the chain is proven free of compiled frames (P5/nb). Linux rows: every banded call of `GciBp24InNativeMovingProbe` / `GchBp4BandAtPauseProbe` narrowed, band words per pause 0.44-0.48x, every soundness row SAME as HotSpot, `noop` per-call median 0.99x . |
| `CRATONVM_GC=jmx-diagnostic-command-mbean` *(alias: `CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN`)* | **Default ON  (gci i28/dg), `=0` kills.** Seeds `remoteDiagnosticCommandsSupport` so the `DiagnosticCommand` MBean is exposed; its four natives run the attach listener's commands (gci-i14mp item 11b). Rows: vi28-dg D. |
| `CRATONVM_GC=g1-remark-addr-keyed-mark-exact` *(alias: `CRATONVM_G1_REMARK_ADDR_KEYED_MARK_EXACT=1`)* | **Opt-in (gci i22/g1rk).** G1: after a completed remark + cleanup whose closure was trusted, every address-keyed side table (smuggled longs, Throwable traces, inherited buckets, TLS / JUL / lock-key / io / socket / zip / publisher / Undertow rows) is swept on the completed mark (`G1Collector::trusted_mark_liveness`), not on the region-granular verdict, so a dead key in a region the cleanup kept loses its row at the remark instead of at the mixed pause that evacuates the region. Deciding rows `vi22-g1rk.list` (FLIP RULE). `CRATONVM_DBG_G1_REMARK_ADDR_KEYED=1` prints the per-sweep counts. |
| `CRATONVM_GC=g1-conc-frozen-eager-scan` *(alias: `CRATONVM_G1_CONC_FROZEN_EAGER_SCAN`)* | **Default ON (gci i12/wb), `=0` kills it.** The concurrent-start pause scans every object a thread frozen by the JIT take-over holds, so a compiled store whose SATB decision preceded the pause cannot hide the value it overwrites (G1 twin of Generational's `initial_mark_with_frozen`). |
| `CRATONVM_GC=g1-itl-defer-at-mark` *(alias: `CRATONVM_G1_ITL_DEFER_AT_MARK`)* | **Default ON since gci vi6-g1t, `=0` kills it.** G1 marking cycles defer never-started threads' inherited `ThreadLocal` values to the child `Thread` (`metadata_pin`) instead of rooting them; the remark's mark-exact bucket sweep makes it sound (`gcCalls=1` as on HotSpot; vi7-flip F-ITL). |
| `CRATONVM_GC=g1-workers=<n>` *(alias: `CRATONVM_G1_WORKERS=<n>`)* | Force the evacuation worker count; `=1` drains the parallel path serially, which separates a concurrency race from a logic divergence. Overrides `-XX:ParallelGCThreads`, which in turn overrides the machine-derived default |
| `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` | **Default ON** (gch h4/g1), `=0` restores the old order. G1's final remark runs reference processing first, marks what it keeps (policy-kept soft referents, finalizables, cleaner chains) and drains their closure, and only then reconciles class mirrors and defining loaders and unloads, on the final bitmap -- HotSpot's remark order. Before it, a class or user loader reachable only through a kept `SoftReference` was unloaded while its objects survived (the JDK's `LambdaForm` caches: every G1 GC notification after the first round failed). Pages `gch-h3os-g1-remark-unloads-...`, `gch-h2ob-g1-gc-notifications-fail-...`; rows vh4-g1 A. |
| `CRATONVM_G1_REMARK_ITL_SWEEP` | **Default ON** (gch h4/g1), `=0` kills; inert under `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP=0`. `--compatible`: the G1 remark sweeps the never-started threads' inherited-`ThreadLocal` buckets on the completed mark instead of the region-granular verdict, so a dead child's values are released at the first cycle that finds it unmarked. Page `gch-h3jx-g1-compat-unstarted-thread-...`; rows vh4-g1 C. |
| `CRATONVM_G1_NATIVE_OOME_DEBT` | **Opt-in** (gch h4/g1): arm the native funnel's post-OOME debt on G1 (paid with the complete marking cycle), as Generational does by default. Page `gcd-d10o-native-funnel-oome-debt-is-generational-only`; rows vh4-g1 F. |
| `CRATONVM_DBG=gc-stress=<bytes>` *(alias: `CRATONVM_DBG_GC_STRESS=<bytes>`)* | Force young GCs every N allocated bytes (Generational) |
| `CRATONVM_GC=par-threads=<n>` *(alias: `CRATONVM_GC_PAR_THREADS=<n>`)* | Generational young-GC worker count. `0`/`1` forces the sequential collector; `>= 2` forces that many workers regardless of heap size. Unset = `min(available_parallelism, 8)` once the young gen passes the size floor. `available_parallelism` follows CPU affinity, so a `taskset -c N` run is automatically sequential |
| `CRATONVM_GC=par-min-bytes=<bytes>` *(alias: `CRATONVM_GC_PAR_MIN_BYTES=<bytes>`)* | Young-gen size floor below which the young GC stays sequential (default 16 MiB) |
| `CRATONVM_GC=sync-young-wipe` *(alias: `CRATONVM_GC_SYNC_YOUNG_WIPE=1`)* | Zero the evacuated young semi-space INSIDE the pause, as . By default the memset runs on a helper thread after the pause (the arena is the next cycle's to-space, which no mutator allocates into) and is joined before the next collection; `[gcpause]` reports `wipe_deferred_bytes=`. The first lever to pull if a conservative root is ever reported inside the inactive semi-space |
| `CRATONVM_GC=-par-evac` *(alias: `CRATONVM_GC_PAR_EVAC=0`)* | Force the single-threaded evacuator for the Generational **moving** (Cheney) young cycle. Parallel evacuation is the default, but it engages only where `CRATONVM_GC_PAR_THREADS` policy already asks for two or more workers, the cycle is a moving one, and to-space covers from-space (a thin-slack cycle runs *bufferless* — one exact span per object off the shared cursor — rather than declining) — so a run that never sees it is common and expected. Workers run on a persistent `evac_pool` (the same pool type G1 uses, but the generational heap owns its own instance; parked on a per-worker condvar, never respawned per pause, and grown to the worker policy's width before every parallel copy), claim to-space in per-cycle buffers sized against the live set off one atomic cursor, promote through the old generation's allocator under a lock, and claim each object with a tagged CAS on its mark word (the same copy-then-CAS protocol G1 uses); a retired buffer's tail is stamped with a `TLAB_FILLER`/`GAP_FILLER` sentinel so the arena stays walkable as the next cycle's from-space. Both evacuators seed from the same three sources and reach the same forwarding map, but `=0` is **not** behaviour-neutral: to-space becomes the next cycle's from-space, and only the parallel arm leaves buffer-tail fillers in it, so the next cycle's object-start walk, anchors and trigger point differ. A defect that involves striding a filler disappears under `=0` and reads as "the parallel evacuator is at fault"; quote `gen_evac::par_evac_census.filler_bytes` for both runs (zero means that run had no fillers to compare). (The second lever, a parallel copy with no to-space buffers, was removed in gce e2: a bisection lever no round ran.) |
| — | `--verbose:gc` prints `[GC] par_evac: cycles=… helper_scans=… cas_losses=… declined_for_slack=… filler_bytes=… promotions=… deferred_cards=…` at exit, unconditionally, including the all-zero line. Read `cycles` first: a zero says the path never engaged, which is a different claim from "it engaged and did nothing". `helper_scans` says whether it was actually *parallel* — a cycle where the driver did everything and the helpers scanned nothing is a load-balancing regression that every correctness test in the suite passes. `promotions` and `deferred_cards` are the two arms whose absence would not fail immediately: promotion is the only shared-lock contention point in the copy phase, and a deferred card is the old→young edge whose loss surfaces a cycle later somewhere else — a zero in either means whatever you ran never exercised it. `declined_for_slack` should stay at 0; a young GC triggers with from-space ~99.9% full, so the slack the Cheney invariant leaves is small (measured: 121 KB out of 128 MB on bt18) and the cycle runs bufferless rather than declining. `filler_bytes` prices the per-worker buffering, and is 0 on a bufferless cycle. |
| — | **Driving the copy phase in a soak.** A plain benchmark run yields one moving cycle or none. `CRATONVM_DBG=gc-stress=250000` turns that into ~500–1000 per process, which is what makes a soak mean anything: `CRATONVM_GC=par-threads=8,moving-young CRATONVM_DBG=gc-stress=250000 cratonvm --XX:UseGc Generational -Xmx256m --verbose:gc -cp … BinT 14`. (The legacy `CRATONVM_GC_PAR_THREADS=8 CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_GC_STRESS=250000` spelling still works and warns once; note that two tokens of the SAME group go in one comma-separated value — a second `CRATONVM_GC=` assignment would replace the first.) Use oracles that are not a second run of this VM — `bench/BinT.java` sums to `10 * (2^(d+1) − 1)`, and `bench/HashMapOnly.java` / `bench/StringRegexOnly.java` document their checksums in their own headers. |
| `CRATONVM_GC=sweep-anchor-stride=<bytes>` *(alias: `CRATONVM_GC_SWEEP_ANCHOR_STRIDE=<bytes>`)* | Byte spacing of the parallel-sweep anchors (default 8 MiB). Also sizes the MOVING path's parallel object-start-walk chunks. Lower it to drive either on a small young gen under `CRATONVM_DBG_GC_STRESS` |
| `CRATONVM_GC=verify-rset` *(alias: `CRATONVM_GC_VERIFY_RSET=1`)* | After each young collection's old->young seeding, walk the whole old generation and report `[rset-verify] site=.. edges=N missing=M seeded=S`. `missing > 0` names an edge the card table did not deliver, and the first one's referrer/class/slot. **Read `edges` too**: `missing=0` on a run that found no edges at all is vacuous, not clean. Costs a full old-gen walk per young GC |
| `CRATONVM_GC=full-rset-scan` *(alias: `CRATONVM_GC_FULL_RSET_SCAN=1`)* | Restore the pre- whole-old-generation old->young walk on every young collection. The revert lever for the default flip below; the first thing to try if a premature-reclamation defect is suspected under Generational |
| `CRATONVM_GC=young-trigger-percent=<n>` *(alias: `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<n>`)* | Moving young collection trigger, as a percent of from-space capacity (default 50, clamped 1..=95). Raising it collects less often and copies more survivors per cycle; see "Young sizing" below. The NON-moving sweep has its own, higher trigger — `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`, 90, not settable — because it reclaims in place and needs no Cheney headroom. Since the user's flip  a heap built from `-Xmx` uses HotSpot Serial's eden instead (`CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN=0` restores the percent knob). |
| `CRATONVM_GC=-peer-pin-divert` *(alias: `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`)* | Let a moving young cycle proceed even when a conservatively-discovered JIT root was published this cycle. Restores the pre- behaviour. **This is the single largest lever on whether a JIT-warm Generational run compacts at all** — the term it disables (`unrewritable_conservative_jit_roots`, see the `divert_non_moving` table below) fires on essentially every cycle once compiled code is live. It is off by default because a conservative pin landing in young-from names an object whose holder word nothing can rewrite; turn it on only to A/B a suspected over-diversion, and read `[GC] moving_young:` on both arms |
| `CRATONVM_DBG=gcpause` (alias `CRATONVM_DBG_GCPAUSE=1`) | Per-phase breakdown of the MOVING young cycle, printed for any collection over 100 ms. **The rows partition the pause **, as G1's `[GC-STAT]` line does: every figure is microseconds rendered `{:.3}ms`, and an `other=` residual is printed unconditionally, including zero, so the rows plus `other` equal the total by construction. Until that date there was no residual — everything before the first mark and after the last was charged to nothing — and every figure was `as_millis`, so twenty sub-millisecond phases printed twenty zeros against a 12 ms total. **Any `[gcpause]` output quoted from  has both defects**, including the table under "Where a moving young pause actually goes" below. internal records. ** The 100 ms threshold is `CRATONVM_DBG_GCPAUSE_MIN_US` (token `CRATONVM_DBG=gcpause-min-us=<us>`; `0` prints every cycle), so the phase medians of small, stress-driven pauses can be taken; while a JFR recording runs the same marks are emitted as `jdk.GCPhasePause` events, `other` included |
| `-Xlog:gc`, JMX *(generational, | The `-Xlog:gc` line's `before->after(committed)` is the heap's live occupancy (`VmHeap::live_bytes_estimate`: young `used - free_list` plus old used), not the bump high-water, which does not move across a non-moving young sweep and printed `8M->8M` on every pause of a run that was reclaiming. `ManagementFactory` reports HotSpot Serial's beans: collectors `Copy` (cycles that did not reclaim old gen) and `MarkSweepCompact` (cycles that did; a full cycle counts there only), pools `Eden Space` / `Survivor Space` / `Tenured Gen` with real usage. G1  reports HotSpot G1's beans: `G1 Young Generation` (every collection pause, young or mixed; `end of minor GC`), `G1 Old Generation` (full collections, which this G1 does not run, so 0 — HotSpot's `System.gc` is a full collection there), `G1 Concurrent GC` (the marking cycle's initial-mark and remark + cleanup pauses; `end of concurrent GC pause`, cause `No GC`), and pools `G1 Eden Space` / `G1 Survivor Space` / `G1 Old Gen` (Old + humongous regions; committed = the committed heap minus the young pools) from one region census, with `getCollectionUsage`, `getLastGcInfo` and GC notifications. ZGC reports HotSpot 25's four beans (gcd d10/o retirement, ): every whole-heap collection — every collection of the default mode — moves `ZGC Major Cycles` (`end of GC cycle`) and `ZGC Major Pauses` (`end of GC pause`; mark starts count there too; since zs s4/ob3 a collection is booked there as the HotSpot pauses it has a real phase for -- `Pause Mark Start` (unless its mark start was its own pause), `Pause Mark End`, `Pause Relocate Start` -- cut at its own laps and summing to its pause, `CRATONVM_ZGC_PAUSE_PHASES=0` for one record; HotSpot books 3 per minor cycle, 5 per major, 8 per `System.gc`: `zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight`), a young cycle of the opt-in generational mode `ZGC Minor Cycles` / `ZGC Minor Pauses`; pools `ZGC Young Generation` (always 0 used: this ZGC keeps no separate young space) and `ZGC Old Generation` (the heap), each managed by all four beans. `-Xlog:gc` prints HotSpot 25's `GC(n) Major Collection (<cause>)` start line and `GC(n) Major Collection (<cause>) 8M(3%)->2M(1%) 0.021s` end line, and every backend now prints its `Using` line (`Using G1`, `Using The Z Garbage Collector`) and a `gc,init` block. Until then ZGC answered JDK 21's non-generational `ZGC Cycles` / `ZGC Pauses` over `ZHeap` and `Garbage Collection (<cause>) a->b(c) t ms`, and G1 and ZGC printed no `Using` line. The ZGC state lives in `HeapRealm::zgc_gc_beans` (`handoff-w7f-zgc-backend-gc-beans`, applied; internal records). On G1 and ZGC the per-bean counts and times are the common GC event plumbing's (`gc_metrics::BackendGcBeans`, one per door pause, per VM), not the collector's own counters: a G1 evacuation-failure drain pass is not a second collection there. A marking pause's notification is delivered at the next drain point (the next collection's door, or the delivery thread's next batch). `getCollectionTime` is real accumulated pause time on every backend: Generational per bean from the heap's pause totals, **the floor of the bean's microsecond sum, as HotSpot** (gen r5w3/obs7; until then each pause was rounded up to a millisecond, so a run of sub-millisecond pauses reported one millisecond per pause — `GenR5W3CollectionTimeProbe` matches Serial now; H2's `collectGarbage`, which loops until the value moves, may call `System.gc` a few more times when full collections take under 1 ms, and terminates as it does on HotSpot — see `gengc-r5w2-obs6-collection-time-ceils-every-pause`); G1 and ZGC per bean from the plumbing's sealed durations (`pause_sum_as_collection_time_ms`: the floor of the microsecond sum, as HotSpot and the Serial beans; until the gce e1o fix,, it added one millisecond per pause). The single-bean fallback (`NativeContext::gc_collection_time_ms`, `VmHeap::collection_time_ms`, still what a backend with no bean state answers) is G1's own microsecond pause sum rounded the same way, and on ZGC the plumbing's backend-independent sum (`HeapRealm::gc_pause_ns_total`, gc-common w6-f). G1 and ZGC pools support HotSpot 25's thresholds (the old pool both, the young pools the collection-usage one; checked at collection ends) and report `init` (the startup commit on the old pool, 0 on the young ones). Intended differences: G1's `System.gc` is reported as what this G1 runs -- `Pause Young (System.gc)` on `G1 Young Generation` plus the marking cycle's pauses on `G1 Concurrent GC` -- where HotSpot runs and reports a full collection (`Pause Full`, `G1 Old Generation`); ZGC's young pool reads 0 (no nursery in the default mode); and every collection of the default (single-generation) ZGC, `Allocation Rate` ones included, is booked on `ZGC Major Cycles` / `ZGC Major Pauses`, where HotSpot 25 books its rate-driven young collections on the `Minor` pair: the default mode marks and relocates the whole heap, so a `Minor` record would claim a young-only collection that did not run (decision recorded by zs s6/ob5 on `zgc-zr9hp-allocation-collections-book-major-beans`; the opt-in `CRATONVM_ZGC_GENERATIONAL=1` books its young cycles on `Minor`). ZGC's non-heap `Metaspace` pool (zs s3/ob2) carries its measured figure on every cycle `GcInfo`, including collections that ran before the pool was first handed out (`CRATONVM_ZGC_METASPACE_FIRST_CYCLES`, zs s6/ob5), honours a usage threshold (checked when set and at each collection's end, as HotSpot; `CRATONVM_ZGC_METASPACE_THRESHOLD`) and keeps its collection-edge samples as peaks (`CRATONVM_ZGC_METASPACE_PEAK_AT_GC`). Since zt t1/ob (the zs / dev merge decision) ZGC lists HotSpot 25's five non-heap pools and `CodeCacheManager` / `Metaspace Manager` with gci i15/nh / i33/ms's figures, as every other backend, so the pool beans and `getNonHeapMemoryUsage` agree; a pause `GcInfo` carries zeros for all five and a cycle `GcInfo`'s `Metaspace` / `Compressed Class Space` entries come from one edge walk (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`, `=0` for zs s3/ob2's `Metaspace`-only list); zt t3/ob: each code heap's `init` is HotSpot's 2555904, so the non-heap `init` is HotSpot's 7667712 (`CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`). Further intended differences of ZGC's `-Xlog` and `Runtime` figures (recorded by zt t3/ob; each names a HotSpot structure this collector does not have, so printing it would invent the figure): the twelve `gc,init` lines of HotSpot's colored-pointer reservation, variable medium pages and dynamic per-generation worker pools (`Address Space Size`, `Reserved Space Type` / `Size`, `Page Size Medium`, `GC Workers for Old/Young Generation`, `GC Workers Max`, ...); `Runtime` / `MemoryMXBean` heap `used` byte-exact, where HotSpot's is the sum of its 2 MiB-granular pages; the `gc,exit` page-cache lines (` Cache NM (N)`, `  size classes NM (N)`) and the `gc,stats` table (HotSpot's `ZStat` 10 s / 10 m / 10 h windows); and the uncommitter's `gc+heap=debug` `Timeout:` / `Cancel Next Cycle Timeout:` lines and paced passes (the info line `Uncommitter (0) Uncommitted:` matches). Also intended (recorded by zt t5/jf, local HotSpot 25.0.3 `Zt1JfVmOperations`): the pause count of a ZGC major. HotSpot runs 8 stop-the-world pauses / VM operations per `System.gc` -- a promote-all young collection's `ZMarkStartYoung`, `ZMarkEndYoung`, `ZRelocateStartYoung`, then the major's `ZMarkStartYoungAndOld`, `ZMarkEndYoung`, `ZRelocateStartYoung`, `ZMarkEndOld`, `ZRelocateStartOld` -- plus `ZMarkFlushOperation` handshakes (requested by its `ZWorker*` threads) and one `ZRendezvousGCThreads`, and 5 pauses per other major; this VM books 3 on the `ZGC Major Pauses` bean, as `jdk.GCPhasePause` rows and as VM operations (`ZMarkStartYoungAndOld`, `ZMarkEndOld`, `ZRelocateStartOld`), the three agreeing. Its whole-heap collection has no promote-all young collection, no separate young and old marks or relocations, no VM-requested mark flush and no relocation-worker rendezvous, so the rest would be laps cut at invented points (`zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight`; `Zt1JfVmOperations` compares the shared shape SAME, its `seq` mode prints the sequences, 8 vs 3). Since zt t5/jf (`CRATONVM_ZGC_XLOG_PAUSE_PHASES`) a ZGC collection's `-Xlog:gc*` block carries, between its `Major Collection` start and end lines, its three pause lines under `gc,phases` (`Y: Pause Mark Start (Major)`, `O: Pause Mark End`, `O: Pause Relocate Start`; `y:` for a minor) and one `gc,metaspace` line (`O: Metaspace: NM used, NM committed, NM reserved`); the rest of HotSpot 25.0.3's per-collection `gc*` block (local capture of `Zt1JfVmOperations`, 675 lines against this VM's 27) is an intended difference, each line naming a structure or a measurement this collector does not have: the generation sub-collection headers and their transition lines (`Y: Young Generation (Promote All)` / `(Collect Roots) 60M(23%)->36M(14%) 0.002s`, `O: Old Generation ...`), the concurrent phase lines (`Concurrent Mark`, `Concurrent Mark Free`, `Concurrent Reset Relocation Set`, `Concurrent Select Relocation Set`, `Concurrent Process Non-Strong`, `Concurrent Remap Roots`, `Concurrent Relocate`: this collector's mark, reference processing, selection and relocation run inside the pause), `gc,task` (`Using N Workers for Young / Old Generation`: dynamic per-generation worker pools), `gc,reloc` (`Using tenuring threshold`, the per-page-size Candidates / Selected / In-Place table, `Forwarding Usage`, the `Age Table`), `gc,alloc` (the per-phase allocation-stall table), `gc,marking` (mark stripes and proactive / terminate flushes), `gc,nmethod` (registered / unregistered nmethods), the per-generation `gc,ref` Encountered / Discovered / Enqueued table (HotSpot's discovery counts; this VM's reference pass counts clears and enqueues, see the zs7jf JFR page), and the periodic `gc,stats` table (as above). Since zt t31/x10 (`CRATONVM_ZGC_XLOG_LOAD_MMU`, `zgc-zt13x10-review-residuals` item 2) each generation block prints HotSpot's `gc,load` line (the system load averages and their share of the CPUs) and `gc,mmu` line (the running minimum mutator utilisation over this VM's last 200 pauses, one per stop-the-world collection), and `-Xlog:gc+load`, `gc+mmu`, `gc+reloc`, `gc+task`, `gc+nmethod`, `gc+stats` are selectable without a warning (the last four print nothing, for the reasons above). The `gc,heap` Heap Statistics and generation statistics tables keep HotSpot's six columns over this collector's edges (`CRATONVM_ZGC_XLOG_EDGE_BOOK`, same page item 1): Mark Start is the occupancy when a concurrent mark started (the bytes allocated since are the young table's `Allocated`), otherwise the occupancy before the pause; Mark End and Relocate Start read the occupancy before the pause (nothing is allocated or freed inside it; HotSpot frees its empty pages at Relocate Start, this collector frees every dead object at relocate end); `Compacted` is the bytes the slide copied. The capacity lines `gc,heap Min / Max / Soft Max Capacity` are not printed yet (true figures exist; residual R9 on `zgc-zt1jf-jfr-review-residuals`). `--verbose:gc`'s shutdown census carries the same split as `[GC] jmx_collectors:` |

Note: `tracing::debug!` is compiled out of release builds
(`release_max_level_info`); for cycle-phase confirmation attach gdb to
un-inlined gc-crate symbols (LTO is off for `cratonvm-cli` dev builds).

**Tuning knobs** (java-compatible): `-Xmx` (reserved address space on every
backend, including the Generational old generation ),
`-Xms` (committed at startup, **honoured on all three backends** since
 — see the heap-size note at the top of this
file), `-XX:G1HeapRegionSize=<bytes>` (gci i11/gm an explicit value follows HotSpot:
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
invisible. `--verbose:gc` does not change the budget (gci i7/g1y;
`CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC=1` restores the whole-heap sweep it
used to select). On the parallel evacuator phases 1-3 are fused into one work-stealing
closure and are reported wholly as `closure_us`, with `roots_us`/`rset_us` zero
— that is the honest reading, not a missing measurement.

## Backend details worth knowing

**Generational.** Young is a pair of semi-spaces with TLAB bump
allocation. The pair is half of `-Xmx` unless the operator passes `-Xmn`,
`-XX:NewRatio`, `-XX:NewSize` or `-XX:MaxNewSize` (honoured  on
this backend only; the old generation gets the rest of `-Xmx`, and every clamp
prints one `Warning:` line) — see `gc-tuning.md`, "Young generation size". G1
and ZGC print a one-line note and ignore the four flags.

*When a young cycle relocates, stated against the code.* The decision is
`gen_heap::collect_garbage_inner`'s `divert_non_moving`, and it has **six**
terms, any one of which selects the non-moving sweep:

| term | fires when |
|---|---|
| `!moving_young` | moving-young is not in effect for this cycle — either the operator opted out (`CRATONVM_GC=-moving-young`, alias `CRATONVM_NO_MOVING_YOUNG=1`) or this cycle's coverage proof failed. Subsumes the legacy rule (a live JIT frame, or an unregistered compiled frame on the stack, while moving-young is OFF) |
| `unrewritable_conservative_jit_roots` | **moving-young is ON**, a live JIT frame, *and* a conservative JIT scan ran this cycle (`gc_quiescence::conservative_jit_scans > 0`).  `CRATONVM_GC=-peer-pin-divert` (alias `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`) is the opt-out |
| `honor_promotion_oom_risk` | both generations ≥ 90 % full *and* conservative roots exist (or `CRATONVM_GC=promotion-oom-guard-broad`, alias `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`) |
| `divert_for_incomplete_moving_coverage` | this cycle's per-frame rewritable-root proof failed |
| `explicit_full_gc` | `System.gc` asked for an old-gen-inclusive cycle — unless `CRATONVM_GC=system-gc-moving-young` (alias `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`, default off, ) withdraws the in-place promise: the root gatherer then roots class mirrors, loader metadata and collection overlays unconditionally on that cycle (no class unloading or overlay-only reclamation by it), and the cycle may compact young; the old-generation half still runs |
| `gpu_relocation_forbidden` | a GPU device held the arena in place past the collector's bounded wait |

Only `CRATONVM_DBG=force-moving` (alias `CRATONVM_DBG_FORCE_MOVING=1`) can carry a
cycle past the first three.

> **The first term lost a conjunct, and that is the repair for a
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
> internal records.
>
> **Reason code.** A cycle diverted by the opt-out *alone* is recorded as
> `nonmoving-young-compaction-disabled` (; until then it borrowed
> `nonmoving-conservative-jit-roots` and the `--verbose:gc` line qualified it as
> `/no-moving-young-optout`). See
> internal records.

**This page said for a month that a live JIT frame "does not by itself force
the non-moving sweep", and that the older `any JIT frame ⇒ non-moving` rule was
"reachable today only under `CRATONVM_GC=-moving-young` or
`CRATONVM_GC=-moving-young-jit-frames`" (aliases `CRATONVM_NO_MOVING_YOUNG=1` /
`CRATONVM_MOVING_YOUNG_NO_JIT=1`). Both claims are false on the shipped
default.** The second term above restores that rule almost exactly: every
mutator publishes a conservative JIT-root scan on its way into the pause, so on
a JIT-warm workload `conservative_jit_scans > 0` holds on essentially every
cycle, and a live compiled frame anywhere in the process diverts the collection
to the non-moving sweep with moving-young switched on. What changed in 
is the *justification* (a conservative pin in young-from cannot be rewritten, so
the cycle must not relocate), not the outcome.

A JIT-warm workload therefore spends most or all of its cycles non-moving. That
is still a *measured* rate rather than a claim to be taken on trust, and the
running process states its own answer through
`gc_metrics::collector_decision_report`. Since  the three
situations that used to share one reason code are three histogram rows, each
with its own lever:

| reason code | term | lever |
|---|---|---|
| `nonmoving-unrewritable-conservative-jit-roots` | the  term above (moving-young ON) | `CRATONVM_GC=-peer-pin-divert` (off by default for a soundness reason) |
| `nonmoving-conservative-jit-roots` | the legacy rule: a live compiled frame with moving-young OFF | turn moving-young on |
| `nonmoving-young-compaction-disabled` | the opt-out alone, nothing compiled | turn moving-young on |

(Until then all three were `nonmoving-conservative-jit-roots`, and a
`[GC] conservative-jit-root diverts:` line under the histogram approximated the
split from `moving_young_requested`; that line is gone. See
internal records.)

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
Parallel EVACUATION exists here  (`gc/src/gen_evac.rs`):
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

**What the opt-out actually does, .** Every generational cycle
takes the non-moving young sweep, whatever is or is not compiled — `kind=non-moving`
on every `--verbose:gc` line and `moving=0` in the decision histogram. Because
`major_gc` (the old-gen **mark-compact**) has exactly one caller and it is on the
moving young path, the opt-out also stops the old generation compacting: old-gen
reclamation drops to the in-place `sweep_old_gen_non_moving`. *(Correction,
: `major_gc` compacts only when `CRATONVM_OLDGEN_COMPACT` /
`CRATONVM_GC=oldgen-compact` opts in — it has been off by default since
 — so on a default run the old generation never compacts on either
path, and `major_gc` is an in-place mark-sweep too. The opt-out changes which
young collector runs; it only changes old-gen relocation when that opt-in is
set. See `gc/src/old_gen.rs`'s module header. Since  the opt-in is the
default: `CRATONVM_OLDGEN_COMPACT` and `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` are default ON
by the user's flip (WARNING gci-user-flip), `=0` kills each.)* So the flag is a
whole-heap "nothing relocates" lever, which is what makes it a usable bisect for
a suspected relocation defect — and also why it costs fragmentation headroom and
is not a throughput setting. Before that date it was none of those things on a
process with no live compiled frame; see the note under the `divert_non_moving`
table.

*Two diagnostics to know about on that run.* `[GC] moving_young:` is printed
only on the **Generational** backend ( — before that it also
printed on G1 and ZGC runs, counting those backends' pauses under the young
collector's name), and there only when `gc_quiescence::moving_young_enabled`
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
`gc_metrics::collector_decision_report`, before attributing any
cost or behaviour to compaction.

**Read the first two fields; the third is expected to be zero and cannot tell
you what its name suggests.** This is the single account of that line — two
independent round-1 lanes reached it from different directions (a rename in
`vm_heap.rs`, and a reachability argument about the call site) and this
paragraph replaces both.

| field | source | what it can tell you | what it CANNOT |
|---|---|---|---|
| `moving_cycles_total` | `gc_metrics::decision_histogram`, summed over the `moving-*` reason codes | how many generational collections relocated | — |
| `non_moving_cycles_total` | the same histogram, `nonmoving-*` codes | how many took the in-place sweep instead. The per-reason rows say **why** | — |
| `refused_cycles_total` ** | the same histogram, `skipped-*` codes | how many young cycles ran **neither** collector: to-space could not cover from-space, the from-space object-start walk did not complete, or a live mutator still owned a reserved TLAB tail inside the arena (T-3). Each has its own reason code, so the per-reason rows say which. A refused cycle over-retains and is retried on the next trigger | whether the heap recovered — read the following cycles' `kind=` for that |
| `moving_cycles_under_live_jit` | `gc_quiescence::moving_young_cycle_count` | in principle, moving cycles taken while a compiled frame was live | **anything, in practice.** See below |
| `coverage_fallbacks` | `moving_young_coverage_fallback_count` | cycles whose per-cycle root-coverage proof failed | — |

`moving_cycles_under_live_jit` is **structurally zero once compiled code is
live**, and a zero there is not evidence of anything. Its only bump site sits
behind `if moving_young && has_conservative_roots` — but twenty lines earlier
`unrewritable_conservative_jit_roots` requires the same two terms plus
`conservative_jit_scans > 0`, and diverts the cycle to the non-moving sweep
before it can arrive; that count "is positive whenever anything is compiled"
(the term's own comment). So the conjunction is false in any JIT-warm process
and true only where nobody publishes a conservative scan — a unit test driving
the collector directly, or `CRATONVM_GC=precise-only-roots`. Measured
 on `BinT 14` under `CRATONVM_DBG=gc-stress=250000`:
`moving_cycles_total=1510 non_moving_cycles_total=2
moving_cycles_under_live_jit=0 coverage_fallbacks=0`
 — 1510 cycles
that demonstrably copied, and the JIT-active count reads zero because that soak
never exercised the JIT-frame path at all. Do not read it as "the collector
never moved under the JIT"; read it as "this run says nothing about that case".
The counter was kept under its honest name rather than widened or deleted
.

**The key changed .** Until then the line's only cycle count was
keyed `cycles=` and carried the `moving_cycles_under_live_jit` number, so a run
with no compiled frames could relocate on every collection and still print
`moving_young: cycles=0 coverage_fallbacks=0` — which reads as "the young
generation never moved", the precise inverse of the truth, and is the reading
several earlier investigations started from (see
internal records, whose separate point is that a DEFAULT run
is ZGC and never reaches this collector at all). **Any quotation of
`moving_young: cycles=N` from a log older than  is a JIT-active count,
not a moving-cycle count**. (`ARCHITECTURE.md` and
`docs/feature-designs/default-moving-young-gen.md` used to instruct readers to
read the old field; both now name `moving_cycles_total`.)

Old gen is a free-list
allocator collected by a VM-driven concurrent cycle (initial mark STW →
concurrent trace → remark STW → concurrent sweep, with a remark-time
TAMS snapshot gating the sweep).
In current architecture, the cycle is OWNED — one driver at a time
CASes the shared phase `Idle → InitialMark` and every early exit abandons it —
and the concurrent trace runs in slices of `CRATONVM_GEN_CONC_MARK_SLICE`
objects (default 32 Ki; `0` restores the single unbounded trace) with the
old-gen lock released and a safepoint polled between slices, so promotions and
other threads' pauses wait for one slice rather than the whole closure. A remark
that loses its pause race joins the winner and retries (4 attempts). Since wave
3 the sweep is sliced the same way (the same knob, objects walked per slice; a
foreign old-gen free between slices stops it —
internal records),
Phase 2 drains the SATB log and repairs mark-queue overflow itself instead of
leaving both to the remark pause, and a finished cycle is counted as an old-gen
collection by the trigger. `CRATONVM_GC_OLD_GIVE_BACK` (opt-in) returns the
whole granules of large free blocks to the OS after an in-place old sweep that
leaves the generation below its 75 % trigger.

*Where a moving young pause actually goes, and the  changes.*

> **These numbers were taken with a broken instrument and have not been
> re-measured.** At the time, the `[gcpause]` rows had no residual/`other` term
> and every figure was quantised to whole milliseconds, so they did not
> partition the pause the way the "before"/"after" columns assume. Concretely,
> the "after" column reads 22–39 + 0 + 32–63 + 28–52 + 0 = **82–154 ms** against
> a stated total of **100–133 ms**; the rows do not sum, the gap is not stated,
> and nothing in the output says whether the difference is unmeasured phases or
> quantisation. The instrument was repaired  (microsecond marks, an
> `epilogue` mark, an unconditional `other=` residual, and a `debug_assert`
> against double-counting — see
> internal records);
> **this table has not been re-run on it.** The direction of the 
> changes is not in doubt (the `full_old_rset_scan` row went to a hard zero),
> but the magnitudes are not citable to better than "large" until someone
> repeats the run.

`CRATONVM_DBG=gcpause` reports a per-phase breakdown for the MOVING cycle.
Before, on `bench/OldGenRsetProbe 19 700 16` at `-Xmx1g`
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
  anchor grid and chunked across `young_gc_threads` workers, each chunk
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
  gates are published by ZGC only: `ref_store_gates` requires all three
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
held, the cost rule failed). **Default ON **
(WARNING gci-user-flip, `=0` kills it), together with the HotSpot age
clock (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`, `..._OLD_DIRECT`).
Survivors of a heap built from `-Xmx` age once per HotSpot Serial eden of young
allocation, not once per young cycle (`CRATONVM_GEN_AGE_BY_SERIAL_EDEN`, default
ON, `=0` kills; `gen_heap::serial_eden_age_epoch`):
the trigger and the cycle count stay, a cycle that has not seen an eden of
allocation since the last aging cycle ages nothing and tenures only by force,
and the tenuring survivor target follows that eden. It closes the medium-lived
tenuring divergence (`OldGenFinalizeProbe` and `GC21CMediumLivedWeak` 64/64 as
HotSpot at `-Xmx256m`); `[GC] young_trigger:` carries `age_by_serial_eden=
age_epoch_aging_cycles= age_epoch_held_cycles=`.


### Generational sizing continuation

Fixed Serial pool accounting (`CRATONVM_GEN_TENURED_POOL_MAX_FIXED`, default ON,
`=0` diagnostic kill) separates the reporting budgets from physical arenas and
copy workspace. Live-byte totals are conserved across survivor overflow and
borrowed old storage. Fragmented pinned repair, free-ratio growth, stepped
shrinking and reversible old-capacity borrowing are enabled in the private GCJ
continuation, with `=0` controls. Integrated native behavior and interleaved CPU
acceptance remain pending in `internal project records`.

## ZGC: Intended Differences from HotSpot 25

These are architectural design differences of this collector (a stop-the-world whole-heap collection by default, no page allocator, no generational default) compared to HotSpot 25.

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
HotSpot's (`Warmup`, `Allocation Rate`, `Allocation Stall`, `System.gc`), but the number and the
timing of collections differ, and a mutator that hits the trigger waits for the collection instead
of allocating through a concurrent cycle. Since zt t12/st a compiled allocation whose TLAB refill
finds the trigger armed takes that collection at the refill (`CRATONVM_ZGC_REFILL_GATE_COLLECTS`,
`=0` kill), so `Allocation Stall` is left to an allocation that actually fails; HotSpot's
additional `Allocation Stall (<thread>)` lines are waits on a concurrent cycle in flight, which a
stop-the-world collection has no counterpart for.
The DURATION of an allocation stall that still happens is also an intended difference (zt12st
residual 1, decided in zt t21). A stall here is the stalling thread's own stop-the-world, whole-heap
collection: time-to-safepoint, mark, reference processing and the sliding relocation all fall inside
it, where HotSpot 25's stalling thread waits for a concurrent cycle already in flight and is released
as soon as relocation frees a page. The `Allocation Stall (<thread>) N.NNNms` line under `-Xlog:gc`
(default ON, `CRATONVM_ZGC_XLOG_ALLOCATION_STALL=0` turns it off), the `jdk.ZAllocationStall`
duration and the stalled thread's wall time are therefore larger: medians of 13-44 ms against
HotSpot's 0.5-1.3 ms on `Zr1TrCauseProbe 200` at `-Xmx48m` with the refill gate killed (with the gate
on, that row has no stall left). The line is printed only by the thread that stalls; HotSpot prints
one for every thread that waits on the cycle. Concurrent marking alone
(`CRATONVM_ZGC_CONC_EVERY_TRIGGER`, opt-in) does not shorten it, because the stop-the-world slide
dominates the pause. Concurrent relocation is built (zt t32, R5-R6 of
internal records) as the opt-in
`CRATONVM_ZGC_CONCURRENT_RELOCATE`: the closing pause plans the relocation and the copies happen after it, so the
pause no longer grows with the moved bytes; since zt t33 it no longer walks the live set for stale words
, and
since zt t34 it sizes no survivor outside the regions it selects: region liveness and the legacy holders come
from the mark (`CRATONVM_ZGC_CRL_MARK_LIVENESS`, default ON under R6), and the `Reference` family is compact
(`CRATONVM_SYNTHETIC_FLOOR_REFERENCE_STUB_EXTENT`, default ON)
. Marking
through the load barrier is the opt-in `CRATONVM_ZGC_LOAD_BARRIER_MARKING`.

**B. Near a full heap (zr6tp, zs6no, zt3tp, zr5cr).** Near a full heap each collection marks and
sweeps the whole live set inside one pause (about 40 ms of CPU per collection at a ~230 MB live set
on the Linux host), where HotSpot 25 mostly runs minor, concurrent cycles; near-OOM rows therefore
cost several times HotSpot's CPU even at HotSpot's collection count. (The larger, count-independent
part of that gap is mutator and compiled-code cost, tracked by the JIT pages.) This collector has no
page allocator: it serves the last one or two MiB of free space object by object and throws
`OutOfMemoryError` only when an allocation finds no hole after a collection, where HotSpot's ZGC
throws when no 2 MiB page can be had; a program that fills the heap retains slightly more before
its OOME (about 2 % more on `ProxyBoxingOomeProbe` and `Zr2CpCapacity grow`). HotSpot clears every
soft reference on a major collection caused by an allocation stall; this VM's stalls are rare and are not HotSpot's waits, and it
stands in with a free-share rule: an allocation-driven collection whose previous cycle left under
6 % of the heap free clears all soft references (`CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`,
default 60, `=0` turns it off). That matches HotSpot at 95 % fill and below 92 %; between the two
HotSpot's own answer varies run to run with its stall timing, and this VM's can differ. The opt-in
exhausted verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`) is the lever that would end a fill earlier
(proposal `zgc-zs6no-proposal-exhausted-verdict-flip`).

**C. Pauses, beans and JFR (zs4ob3, zs2ob, zt1jf-default, zt1jf R8, zs7jf, zr4jf's table).** A collection is
booked as the HotSpot pauses it has a real phase for: `Pause Mark Start` (unless its mark start was
its own pause), `Pause Mark End` and `Pause Relocate Start`, three per collection, with the three
matching JFR VM operations (`ZMarkStartYoungAndOld`, `ZMarkEndOld`, `ZRelocateStartOld`). HotSpot
25 books five per major and eight per `System.gc` (a promote-all young collection, then separate
young and old marks and relocations), plus non-pause `ZMarkFlushOperation` and
`ZRendezvousGCThreads` operations; this collector's single whole-heap mark and single relocation
have no boundary at which to book them, and no record is invented. As a result the `ZGC Major
Pauses` bean counts three per collection and its collection time is the whole stop-the-world time
(HotSpot's pause beans stay near 0 ms); the default mode runs no young-only collection, so it writes
no `ZGC Minor` bean entries or JFR `ZGC Minor` rows (`CRATONVM_ZGC_GENERATIONAL=1` does); a
major writes one `jdk.ZYoungGarbageCollection` (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`) where HotSpot's
`System.gc` major writes two (its pre-clean and roots young collections); a
`GcInfo`'s `GcThreadCount` is 1 and `jdk.GCConfiguration`'s ergonomic thread counts are generic
(HotSpot's figures describe its own worker pools); JFR does not write `jdk.ZPageAllocation`,
`jdk.ZUnmap`, `jdk.ZStatistics*`, `jdk.ZThreadPhase` or any concurrent phase but `Concurrent
Mark`; `jdk.ZRelocationSet` is written only when the selector ran; and collection events are
committed on `VM Thread`, not on `ZDriverMajor`.

**D. Code heaps (zt1ob item 2).** The JIT keeps one unsegmented
code arena, reported in `CodeHeap 'non-profiled nmethods'`; `CodeHeap 'non-nmethods'` and
`CodeHeap 'profiled nmethods'` read 0 bytes used (this VM's interpreter and stubs are in the
binary). On ZGC each code heap's `init` is HotSpot's 2555904 (`CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`). A
cycle's `GcInfo` samples the three code heaps at its two edges, as HotSpot's `gc_begin` / `gc_end`
do (`CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES`).

**E. Reference statistics (zs7jf item 7, zt1jf's carried soft half).** `jdk.GCReferenceStatistics`
counts, per type, the references this collection cleared or enqueued. HotSpot counts the
references it discovered with an unmarked referent, which includes the JDK's own references
(267, 342 and 123 on three local runs of one probe: run-dependent on HotSpot itself) and every
soft reference its policy then keeps alive; this VM's processor keeps no record of the soft
references it keeps. No figure can match HotSpot's byte for byte, so the row's shape (four rows per
major, HotSpot's type names) is what is compared; the weak, final and phantom counts are the same
set on a stop-the-world collection.

**F. A `GcInfo`'s metaspace figures after a redefinition (zt1ob item 5).** The ZGC cycle edges
reuse the last class-store walk while the loaded and unloaded class counts are unchanged
(`BackendGcBeans::metaspace_sample_figures`), so a JVMTI `RedefineClasses` between two
collections shows in a `GcInfo`'s `Metaspace` / `Compressed Class Space` entries only from the next
define or unload; HotSpot samples its arenas at every edge. The `Metaspace` pool bean walks per
query and is exact. The lever that would remove the lag without a walk per edge is the proposal
`zgc-zt1ob-proposal-incremental-metaspace-figures.md`.

**G. `-XX:ZFragmentationLimit` and `-XX:ZYoungCompactionLimit` (zt23hunt4 fragmentation page, item 3;
decided in zt t24).** Both are accepted with HotSpot 25's `double` syntax and `range(0, 100)`
(`CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_SYNTAX`, `CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_RANGE`) and then
ignored. They tune HotSpot's page-based relocation-set selection (how much garbage an old or a young
page may hold before it is relocated); this collector has no pages and, in its default
non-generational mode, no young/old split, and its selector's cutoffs
(`ZFWD_DEFAULT_MAX_LIVE_OCCUPANCY`, the pressure tiers above `ZGC_PRESSURE_FAILURE_MAX_LIVE_OCCUPANCY`,
`CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT`) are per-cycle tiers tuned on host batteries. Mapping the
flags onto them would invent a figure, not imitate one, and nothing Java-visible depends on them
beyond footprint.
