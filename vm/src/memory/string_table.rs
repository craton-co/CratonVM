// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The interned-String table's GC disposition (gcg g2/w,
//! `docs/feature-designs/jdk-only-mode.md`).
//!
//! # What the table is
//!
//! `HeapRealm::string_pool` (`FxHashMap<String, ObjectRef>`) is the VM's
//! `StringTable`: `ldc` String literals, `String.intern()` results and the
//! names the natives intern all live there. Until this module it was a STRONG
//! root (`roots::collect_roots` section 5) that nothing ever pruned, so every
//! String that ever entered it stayed live for the life of the VM. HotSpot's
//! `StringTable` holds its entries weakly (JDK 7+): an interned String nothing
//! else references is collected, and a literal stays alive because its class's
//! resolved-references array holds it.
//!
//! # The weak mode (`CRATONVM_GC_WEAK_STRING_TABLE`, default on since gcg g6/rs)
//!
//! `=0` restores the strong pool.
//!
//! When the switch is on AND the heap is Generational:
//!
//! * a stop-the-world collection that starts while no concurrent old-gen cycle
//!   is open does not root the pool ([`pool_is_weak_for_this_scan`], read by
//!   `roots::collect_roots` section 5);
//! * the same pause's epilogue (`gc::update_all_roots`, inside the
//!   allocation-free `InPlaceVerdict` window, before section 5's remap) drops
//!   every entry whose String died ([`sweep_dead_interned_strings`]).
//!
//! A literal does not need the pool to stay alive: its `ldc` record (the
//! resolution cache's `condy` store, rooted by `collect_roots` step 13 and
//! remapped by `update_condy_refs`) and a compiled site's JNI-global slot both
//! hold it, exactly as HotSpot's resolved-references array does. A String that
//! dies with nothing referencing it is re-created on the next `ldc` / `intern()`
//! of the same text; nobody can tell, because nobody held the old one.
//!
//! # Why concurrent cycles keep the pool strong
//!
//! The Generational concurrent old-gen cycle (initial mark, concurrent mark,
//! remark, concurrent sweep) builds its root sets through the same
//! `collect_roots`. A weak table under a SATB marker needs a keep-alive barrier
//! on every lookup (HotSpot's `StringTable` peeks through a `WeakHandle` that
//! enqueues the String during marking): a mutator that fetched an unmarked
//! pooled String and stored it into an already-scanned object would hide it
//! from the marker, and the concurrent sweep would free a reachable String.
//! There is no such barrier here, so while the cycle's phase is not `Idle` the
//! pool is rooted exactly as before, every pooled String is marked at the
//! initial mark (or at the remark, for one pooled since), and the concurrent
//! sweep can never free one. That is also why no pool sweep is wired into
//! `addr_keyed::drop_address_keyed_rows_in`: that drop runs UNDER the old-gen
//! guard, and `vm_object::create_java_string_reporting` holds the pool's write
//! lock while it allocates (which can take the old-gen lock), so a pool sweep
//! there would invert that order.
//!
//! # ZGC (`CRATONVM_ZGC_WEAK_STRING_TABLE`, zr z1/rf; default on since zt t10, `=0` opts out)
//!
//! A ZGC heap whose every reclamation is a stop-the-world whole-heap
//! collection (no concurrent mark configured, generational mode off:
//! `VmHeap::zgc_weak_string_table_sound`) is the Generational STW case: the
//! pause that frees a pooled String ends in `update_all_roots`, whose
//! epilogue sweeps the pool under the pause's `InPlaceVerdict` (ZGC hands the
//! slide's relocations in `pointer_map`, and the verdict's relocation screen
//! refuses an address the slide re-issued). So [`weak_table_engaged`] is also
//! true there when the switch is set (and `CRATONVM_GC_WEAK_STRING_TABLE` is
//! not `0`). A ZGC heap with generational mode keeps the strong pool under
//! `CRATONVM_ZGC_GEN_WEAK_STRING_TABLE=0` (since zt t27/gn5 the default
//! engages there: "Generational ZGC" below), and so does one with concurrent
//! marking under `CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT=0` (since zt
//! t23/st1 the default engages there too: "Concurrent ZGC" below). The deciding rows are in
//! `docs/feature-designs/zgc-architecture.md`; zt t10 flipped the
//! switch default ON (`docs/gc-tuning.md` row). The gate is read per scan,
//! but every input of `zgc_weak_string_table_sound` is fixed for a launcher
//! run (an embedder toggling the generational mode on a running heap was item
//! 5 of `zgc-zt11x8-wave-t10-review-residuals-SUPERSEDED-20261019.md`; zt t13/x10 made
//! the gate's first "strong" answer sticky, `StringTableState::zgc_sound_now`):
//! `conc_start_bytes` is non-zero from construction whenever concurrent
//! marking is configured (`auto` and every-trigger seed it; the adaptive
//! refresh clamps it to at least a quarter of the threshold), and no cycle
//! opens while it is zero, so a run never moves between the weak and the
//! strong disposition (zt t11/x8 review).
//!
//! # G1 (`CRATONVM_G1_WEAK_STRING_TABLE`, opt-in since gci i21/sw)
//!
//! HotSpot's G1 holds its `StringTable` weakly and cleans it at the remark
//! (`G1ConcurrentMark::weak_refs_work` -> `WeakProcessor`), and a lookup
//! while marking keeps the String alive (`WeakHandle::peek` under the SATB
//! keep-alive). The G1 half of
//! `docs/feature-designs/zgc-architecture.md`,
//! when the switch is set (and `CRATONVM_GC_WEAK_STRING_TABLE` is not `0`):
//!
//! * the two G1 MARK root scans (the initial mark and the remark, and the
//!   stop-the-world full mark cycle's one scan -- exactly the scans taken
//!   under `gc_quiescence::with_class_unload_marking` on G1) leave the pool
//!   out ([`pool_left_out_now`]); every evacuation pause (young, mixed, the
//!   full compaction) still roots it, so nothing a pause copies or remaps
//!   changes, and no `update_all_roots` sweep runs on G1
//!   ([`sweep_after_pause`]);
//! * inside the remark callback, beside the JNI weak-global clear and on the
//!   same completed bitmap (`G1Collector::is_live_after_mark`), every entry
//!   whose String the mark left unmarked is judged dead
//!   ([`g1_remark_judge_interned_strings`]); right after
//!   `g1_final_remark_and_cleanup` returns, still inside the pause, the judged
//!   entries leave the pool -- only when that cleanup TRUSTED its closure
//!   (`last_mark_trusted`; an untrusted cleanup frees nothing, so the entries
//!   and their Strings both stay) ([`g1_remark_settle_interned_strings`]);
//! * every pool HIT while G1's SATB queue is active logs the String it answers
//!   through the SATB barrier ([`keep_alive_on_hit`]), so a String that was
//!   reachable only through the pool at the snapshot and that a mutator
//!   fetches and stores into an already-scanned object during the concurrent
//!   mark is marked (the same keep-alive `Reference.get()` uses). A miss needs
//!   none: a String allocated during marking lies above TAMS, and a receiver
//!   `intern()` pools is an object the mutator holds (reachable at the
//!   snapshot or allocated after it).
//!
//! Every `ldc` literal is held by its record exactly as on Generational
//! ([`weak_table_engaged`] is true on such a G1 heap, so
//! `constants.rs`'s `weak_table_needs_the_record` keeps the record).
//!
//! # Lone-surrogate literals (`CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD`, default on since gci i32/sl)
//!
//! A String Rust text cannot hold lives in `lang_string`'s surrogate pool
//! (UTF-16 keyed, JNI global handles), not here. Its `String.intern()` rows
//! are weak on every collector; an `ldc` literal's row is weak too, and the
//! literal is held by its class's never-evicted `ldc` record on EVERY
//! collector and whatever this table's disposition (`constants.rs`,
//! `record_wide_string_literal_as_of`): a loader-conditional root that the
//! unload transaction drops, so the literal dies with its last class as on
//! HotSpot. `=0` makes the literal's row strong (immortal for the VM's life).
//!
//! # Concurrent ZGC (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`, zt t23/st1; default on, `=0` opts out)
//!
//! HotSpot 25's ZGC holds the `StringTable` weakly on every cycle (all of
//! them concurrent): the pool is no mark root, its dead entries are cleared
//! after Pause Mark End, and a lookup while marking keeps its String alive.
//! Under the switch the ZGC arm of [`weak_table_engaged`] also answers yes on
//! a heap that marks concurrently (`CRATONVM_ZGC_CONCURRENT=1`,
//! every-trigger, any `CRATONVM_ZGC_CONC_START`;
//! `VmHeap::zgc_weak_string_table_sound_under_concurrent_mark`), and the
//! three G1 pieces have these ZGC twins:
//!
//! * **no root.** Both scans a concurrent cycle takes are `collect_roots`
//!   calls whose section 5 asks [`pool_is_weak_for_this_scan`]: the mark
//!   start's (`gc_and_alloc::zgc_concurrent_mark_cycle`) and the closing
//!   collection's (`run_collection_pause`, whose roots the heap's
//!   `finish_concurrent_mark` re-scans). Both leave the pool out; nothing
//!   else roots it (the `ldc` records and compiled `ldc` slots are roots of
//!   their own, rescanned at the mark end).
//! * **keep-alive on hits.** Every pool hit answers through
//!   [`keep_alive_on_hit`], whose ZGC arm logs the String through ZGC's SATB
//!   pre-barrier while a cycle marks: a String only the pool held at mark
//!   start that a mutator fetches and stores into a scanned (or black) object
//!   is marked. A miss needs none: a String allocated while marking is black,
//!   and a pooled receiver is held by its caller.
//! * **the clear.** The closing collection's epilogue sweep
//!   ([`sweep_after_pause`] from `update_all_roots`, inside the pause) drops
//!   every entry whose String the final mark left unmarked: ZGC's in-place
//!   verdict reads the object-start registry, pruned by the mark (`starts &=
//!   marks`) in the pause on a deferred-sweep cycle too
//!   (`CRATONVM_ZGC_CONCURRENT_SWEEP`, `defer_sweep_and_close_pause`), so no
//!   entry survives its String into the window where the sweep runs after
//!   the release. The sweep reads no heap slot (it judges the pool's own
//!   addresses), so no load barrier, strong or weak, is involved.
//!
//! No reclamation happens between the mark start and that pause (the
//! deferred sweep of the PREVIOUS cycle is finished before a mark starts),
//! so an unmarked pooled String stays valid storage until its entry is
//! dropped. The `ldc` record rule (`constants.rs`, `weak_table_needs_the_record`)
//! follows [`weak_table_engaged`], so on such a heap every literal is held by
//! its record for the whole run. `=0`: the strong pool on a concurrently
//! marking heap, as before (the latch then closes at the first scan).
//!
//! # Generational ZGC (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`, zt t27/gn5; default on, `=0` opts out)
//!
//! ZGC's generational mode (`CRATONVM_ZGC_GENERATIONAL=1`) used to keep the
//! strong pool: both gates refused it because a minor cycle's young sweep had
//! not been reviewed for the table's verdicts
//! (`docs/feature-designs/zgc-architecture.md`
//! item 13). Reviewed (zt t27/gn5), under the switch the ZGC arm of
//! [`weak_table_engaged`] answers yes in the mode too
//! (`VmHeap::zgc_weak_string_table_sound_for` /
//! `zgc_weak_string_table_sound_under_concurrent_mark_for` with
//! `generational_ok`), and every collection of the mode -- minor or major --
//! leaves the pool out of its roots and sweeps it in its own epilogue
//! ([`sweep_after_pause`]), which is sound because:
//!
//! * every collection of the mode is stop-the-world (a young cycle is never
//!   run while a concurrent cycle is open), and ends in `update_all_roots`;
//! * the ZGC verdict the sweep asks is the exact start registry
//!   (`ZgcRealHeap::is_object_address`), and a minor's sweep visits only the
//!   NON-old bases (`sweep_young_aged_words`: `registered & !old`). So an OLD
//!   pooled String a minor did not trace (a minor marks no old object) stays
//!   registered and keeps its entry: only a major, which traces the old
//!   generation, can prove it dead. A YOUNG pooled String nothing else holds
//!   is unmarked by the minor's mark (the pool is no root), freed and
//!   unregistered by its sweep, and its entry is dropped; one an old object
//!   holds is marked through the remembered set and kept;
//! * a promotion that moves its String (the opt-in young slide) is a
//!   `pointer_map` key: kept, and remapped by `update_all_roots` section 5.
//!
//! The floor-bounded sweep arm (a young cycle without a usable old-start
//! bitmap) would keep a dead young String below the floor registered while
//! freeing its dead young `value` above it; it runs only under kills
//! (`CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR=0` or the bitmap off), which are
//! already recorded as unsound for weak referents the same way.
//!
//! HotSpot 25 marks through its weak roots in a YOUNG collection
//! (`ZMarkYoungRootsTask` iterates the colored roots strong AND weak), so a
//! dead young pooled String survives its minors there and leaves the table at
//! the old mark after it was promoted (or at the next `System.gc()`, which
//! promotes every young object first). Dropping it at the minor instead is
//! not visible to Java: nothing else references it (a `WeakReference` to it is
//! a strong edge in a young cycle, `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`,
//! so its String survives the minor and keeps its entry), and the next
//! `intern()` of the text pools a new String nobody can tell from the old.
//! `=0`: the strong pool in the mode, as before (the latch then closes at
//! the first scan).
//!
//! # `String.intern()` returns its receiver (`CRATONVM_STRING_INTERN_RETURNS_RECEIVER`, default on since gcg g4/rs; `=0` opts out)
//!
//! JLS / `String.intern` javadoc: when the pool holds no equal String, "this
//! String object is added to the pool and a reference to this String object is
//! returned". The native used to allocate a NEW pooled String instead, so
//! `s.intern() == s` answered `false` for a fresh `s` where HotSpot answers
//! `true`. Under the switch, [`intern_receiver`] pools the receiver itself
//! (`NativeContextImpl::intern_string_receiver`). Independent of the
//! collector, and of the weak mode.
//!
//! # The insert funnel (`CRATONVM_GC_STRING_TABLE_FUNNEL`, opt-in since gci i25/st2)
//!
//! Steps 1-2 of
//! `docs/feature-designs/jdk-only-mode.md`
//! (with `gch-h2jr-review-findings-RETIRED-RULING-20261018` item 6 and
//! `gch-h4wr-review-findings-FIXED-20261017` item 5). Under the switch:
//!
//! * **Allocate outside the lock** (every collector). The three pooled
//!   constructors in `vm_object.rs` (`create_java_string_reporting`,
//!   `try_create_java_string`, `try_create_java_string_reporting`) probe the
//!   pool under its read lock, and on a miss allocate the String with NO pool
//!   lock held, then insert it through [`pool_insert_if_absent`] -- the one
//!   insert funnel, which [`intern_receiver`] uses too. A thread that lost
//!   the race answers the winner (through [`keep_alive_on_hit`]); its fresh
//!   String is garbage at once (HotSpot's `StringTable::do_intern` discards
//!   its loser the same way). The pool -> old-gen lock order the allocation
//!   used to fix (module doc above, "Why concurrent cycles keep the pool
//!   strong") is gone on this path. The allocations are the non-collecting
//!   `try_alloc_*_full` ones, so no collection can separate the allocation
//!   from the insert (the same argument as [`intern_receiver`]'s).
//! * **The young-entry list** (Generational under the weak table only,
//!   [`young_list_engaged`]). The funnel records the KEY of every entry whose
//!   String lies outside the old-generation arena at insertion. A pause that
//!   reclaimed no old storage then judges only the listed entries
//!   ([`sweep_dead_interned_strings`]: every unlisted entry is an old String
//!   that the whole-pool walk would have kept as `old_untouched` without
//!   asking), drops the dead ones by key (no `retain` over the pool), and
//!   records the moved ones for [`remap_pool_after_pause`]; the pre-GC watch
//!   ([`watch_young_pool_entries`]) reads the list instead of the pool. A
//!   pause that reclaimed old storage walks the whole pool exactly as before.
//!   The list is pruned by every sweep (dead entries, and entries whose String
//!   now lies in the old arena).
//! * gcj j1/st: every row of the list carries its String's current address,
//!   kept exact by every pause, so a pause hashes no listed text except to
//!   drop a dead entry or remap a moved one (the text-only list cost about
//!   twice the whole-pool walk while the pool was young:
//!   `docs/feature-designs/jdk-only-mode.md`).
//!
//! Off the switch nothing changes: the constructors keep their old bodies,
//! `intern_receiver` its old insert, and the sweep / watch their walks.
//!
//! # The insert grace (`CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE`, opt-in since gcj j1/st)
//!
//! The blanket mitigation of
//! `docs/feature-designs/jdk-only-mode.md`:
//! on Generational under the weak table, a String one of the pooled
//! constructors pooled since the last pause stays rooted through the next
//! pause ([`note_pooled_by_constructor`]), so a Rust caller holding the answer
//! in an unpinned local across one non-moving pause keeps a live String.
//! Opt-in: it is a Java-visible over-retention of one pause.

use crate::types::ObjectRef;
use crate::vm::SharedVm;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Per-VM state of the interned-String table's GC disposition. Lives in
/// `HeapRealm::string_table`; never a process global.
pub struct StringTableState {
    /// `CRATONVM_GC_WEAK_STRING_TABLE` as this VM was built with it.
    weak: AtomicBool,
    /// `CRATONVM_STRING_INTERN_RETURNS_RECEIVER` as this VM was built with it.
    intern_receiver: AtomicBool,
    /// zr z1/rf: `CRATONVM_ZGC_WEAK_STRING_TABLE` (opt-in) as this VM was
    /// built with it: the weak mode also engages on a ZGC heap that only
    /// collects stop-the-world ([`weak_table_engaged`]).
    zgc_weak: AtomicBool,
    /// zt t13/x10 (`zgc-zt11x8-*` item 5;
    /// `CRATONVM_ZGC_WEAK_STRING_TABLE_STRONG_LATCH`, default ON, `=0` kill)
    /// as this VM was built with it: once the ZGC arm of
    /// [`weak_table_engaged`] answered "strong" (the heap was not
    /// stop-the-world only), it answers "strong" for the VM's life
    /// ([`Self::zgc_sound_now`]).
    zgc_strong_latch: AtomicBool,
    /// zt t13/x10: the latch above has closed (a "strong" answer was seen).
    zgc_strong_latched: AtomicBool,
    /// zt t23/st1 (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`, default ON,
    /// `=0` kill) as this VM was built with it: the ZGC arm also engages on a
    /// heap that marks concurrently (module doc, "Concurrent ZGC").
    zgc_conc_weak: AtomicBool,
    /// zt t23/st1: pool hits [`keep_alive_on_hit`] logged through ZGC's SATB
    /// barrier while a concurrent cycle marked, over the VM's life.
    zgc_keep_alives: AtomicU64,
    /// zt t27/gn5 (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`, default ON, `=0`
    /// kill) as this VM was built with it: the ZGC arm also engages in ZGC's
    /// generational mode (module doc, "Generational ZGC").
    zgc_gen_weak: AtomicBool,
    /// Pauses whose epilogue swept the table.
    sweeps: AtomicU64,
    /// Entries those sweeps dropped (their String died), over the VM's life.
    dropped: AtomicU64,
    /// gcg g4/rs: a root scan left the pool OUT since the last sweep
    /// ([`pool_is_weak_for_this_scan`] sets it, [`sweep_after_pause`] takes
    /// it). A pause whose scan rooted the pool (a concurrent cycle was open)
    /// marked every pooled String, so its sweep could drop nothing; it is
    /// skipped, unless the phase reads `Idle` again at the sweep (then the
    /// sweep runs, which is always sound).
    weak_scan_pending: AtomicBool,
    /// gcg g4/rs: sweeps skipped by that rule, over the VM's life.
    skipped: AtomicU64,
    /// gch h2/jr: `CRATONVM_GC_WEAK_STRING_TABLE_WATCH_YOUNG` (default ON,
    /// `=0` the kill switch) as this VM was built with it. See
    /// [`watch_young_pool_entries`].
    watch_young: AtomicBool,
    /// gch h2/jr: young pool entries [`watch_young_pool_entries`] published
    /// into the watched set, over the VM's life (the engagement counter).
    watched_young: AtomicU64,
    /// gch h3/jx (`gch-h2jr-review-findings-RETIRED-RULING-20261018` item 7):
    /// `CRATONVM_DBG_STRING_TABLE` as this VM was built with it. It used to be
    /// read from the environment by every sweep, inside the pause.
    dbg_line: AtomicBool,
    /// gci i21/sw: `CRATONVM_G1_WEAK_STRING_TABLE` (opt-in) as this VM was
    /// built with it: the weak mode also engages on a G1 heap (module doc,
    /// "G1").
    g1_weak: AtomicBool,
    /// gci i21/sw: the pre-collection addresses the current G1 remark judged
    /// dead ([`g1_remark_judge_interned_strings`]), dropped from the pool by
    /// [`g1_remark_settle_interned_strings`] once cleanup trusted its mark.
    /// Replaced by every judge, taken by every settle.
    // addr-keyed: lives within one remark pause (judge -> settle); never read after it.
    g1_remark_dead: parking_lot::Mutex<Vec<usize>>,
    /// gci i21/sw: G1 remarks whose settle dropped entries (trusted), entries
    /// they dropped, settles that kept a judged set because the cleanup did
    /// not trust its mark, and pool hits logged by [`keep_alive_on_hit`],
    /// over the VM's life.
    g1_remark_clears: AtomicU64,
    g1_dropped: AtomicU64,
    g1_untrusted_kept: AtomicU64,
    g1_keep_alives: AtomicU64,
    /// gci i25/st2: `CRATONVM_GC_STRING_TABLE_FUNNEL` (opt-in) as this VM was
    /// built with it -- the single insert funnel (module doc, "The insert
    /// funnel"): pooled Strings are allocated with no pool lock held, and on
    /// Generational under the weak table the young-entry list below replaces
    /// the per-pause whole-pool walks.
    funnel: AtomicBool,
    /// gci i25/st2: the young-entry list -- one [`YoungEntry`] per entry whose
    /// String lay outside the old-generation arena when the funnel inserted
    /// it. A superset of the pool's young entries at every pause (an entry is
    /// young only at insertion; the sweep drops rows whose entry left the pool
    /// or whose String now lies in the old arena).
    ///
    /// gcj j1/st (`gci-i47w-oa47-...-RETIRED-UNFIXED-20261018`): each row also
    /// carries its String's CURRENT address, kept EXACT by every pause (the
    /// young-list sweep advances it through `pointer_map`; a pause that walks
    /// the whole pool advances it in [`remap_pool_after_pause`]), so no pause
    /// looks a row up by text except to remove a dead entry or remap a moved
    /// one. It used to hold the text alone, and every pause hashed every
    /// listed text four times and sorted the texts once -- about twice the
    /// whole-pool walk while the pool was still young. Invariant (checked in
    /// debug builds, `debug_check_young_list`): outside a pause, every row's
    /// `addr` is `string_pool[key]`, and every entry outside the old arena has
    /// a row. Lock order: `string_pool` -> `young_list` (the funnel pushes
    /// under the pool's write lock; the remap takes the list under it); every
    /// other user takes this lock alone.
    young_list: parking_lot::Mutex<Vec<YoungEntry>>,
    /// gci i25/st2: the old-generation arena `(base, len)` as last read
    /// (`VmHeap::old_gen_info`), for the funnel's classification outside any
    /// lock. The base never moves and the arena only grows (in a pause,
    /// `OldGen::grow_after_refusal`), so a stale `len` only classifies more
    /// Strings as listable -- the superset direction. Refreshed by every
    /// young-list sweep that reads the arena anyway.
    // addr-keyed: the arena's own bounds, never an object address; the base is fixed for the heap's life.
    old_base: std::sync::atomic::AtomicUsize,
    old_len: std::sync::atomic::AtomicUsize,
    /// gci i25/st2: recorded by a young-list sweep for
    /// [`remap_pool_after_pause`] with the collection count it ran at: the
    /// list's rows now name POST-collection addresses (the moved ones carry
    /// `moved_from`), and the rows this pause PROMOTED (moved into the old
    /// arena: they leave the list, but their pool entry still needs the
    /// remap) are held here. Cleared at the start of every
    /// [`sweep_after_pause`].
    young_remap: parking_lot::Mutex<Option<(u64, Vec<YoungEntry>)>>,
    /// gci i25/st2: keys the funnel listed, races it lost (another thread
    /// pooled the text between the miss and the insert; its fresh String is
    /// garbage at once), sweeps that judged only the listed entries, and
    /// remaps that walked only the moved listed entries, over the VM's life.
    funnel_listed: AtomicU64,
    funnel_lost_races: AtomicU64,
    young_list_sweeps: AtomicU64,
    young_list_remaps: AtomicU64,
    /// gci i47w/ws47: `CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT` (default ON,
    /// `=0` the kill switch) as this VM was built with it. See
    /// [`OldBaseScreen`].
    old_exact: AtomicBool,
    /// gci i47w/ws47: OLD entries the screen dropped because their address
    /// is no object base of the old generation after the pause, over the
    /// VM's life.
    old_exact_dropped: AtomicU64,
    /// gcj j1/st (`gcg-g4rs-...-RETIRED-UNFIXED-20261018`, "Fix"):
    /// `CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE` (opt-in) as this VM was
    /// built with it. See [`note_pooled_by_constructor`].
    insert_grace: AtomicBool,
    /// gcj j1/st: the Strings the pooled constructors pooled since the last
    /// pause's epilogue, under [`insert_grace_engaged`]; rooted by every root
    /// scan that leaves the pool out ([`push_insert_grace_roots`]) and
    /// cleared by [`sweep_after_pause`], so a row never outlives the pause
    /// that read it.
    // addr-keyed: lives until the next pause's epilogue clears it; rooted (never judged) by that pause's scans.
    fresh: parking_lot::Mutex<Vec<ObjectRef>>,
    /// gcj j1/st: Strings the grace rooted (counted when cleared), over the
    /// VM's life -- the engagement counter.
    grace_rooted: AtomicU64,
    /// gcj j2/rf2 (the g4rs grace residual):
    /// `CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE` (opt-in, needs the insert
    /// grace) as this VM was built with it. See [`hit_grace_engaged`].
    hit_grace: AtomicBool,
    /// gcj j2/rf2: pool hits the hit grace noted, over the VM's life.
    grace_hits: AtomicU64,
    /// gcj j2/rf2 (the deciding counter of
    /// `gcj-j1st-proposal-string-table-old-entry-screen-reads-the-major-mark-not-an-old-walk`):
    /// [`OldBaseScreen`]'s old-generation walks and their summed time (us),
    /// over the VM's life.
    old_screen_walks: AtomicU64,
    old_screen_walk_us: AtomicU64,
    /// gcj j3/rf3 (`gcj-j2rf2-proposal-string-table-young-entries-ride-a-seed-only-watched-row`):
    /// `CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG` (opt-in) as this VM was built
    /// with it: [`watch_young_pool_entries`] publishes into the heap's seed-only
    /// row (`gc_quiescence::add_seed_only_referents`), read by a major's young
    /// seed and watched-dead verdict only, instead of the watched set every
    /// young sweep reads.
    seed_only_young: AtomicBool,
    /// gcj j3/rf3 (`gcj-j1st-proposal-generational-weak-string-table-through-the-concurrent-cycle`,
    /// the young-pause slice): `CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE`
    /// (opt-in) as this VM was built with it. See
    /// [`young_pause_in_cycle_leaves_pool_out`].
    young_in_cycle: AtomicBool,
    /// gcj j1/st: [`HotspotStringLedger`] is armed
    /// (`CRATONVM_GEN_STRINGTABLE_HOTSPOT_LEDGER`).
    ledger_on: AtomicBool,
    /// gcj j1/st: HotSpot's size, uncleaned entries and owed concurrent work.
    ledger: parking_lot::Mutex<HotspotStringLedger>,
    /// gcj j1/st: `-XX:+PrintStringTableStatistics`' exit block was printed
    /// (once per VM, whichever exit arm comes first).
    exit_statistics_printed: AtomicBool,
}

/// gcj j1/st: one row of the young-entry list (`StringTableState::young_list`).
struct YoungEntry {
    /// The entry's text (the pool key), for the two by-key operations: removing
    /// a dead entry and remapping a moved one.
    key: Box<str>,
    /// The pooled String's current address (module invariant above); within a
    /// pause, between the young-list sweep and the remap, its POST-collection
    /// address.
    addr: usize,
    /// Non-zero only between a young-list sweep that saw the String move and
    /// the remap that follows it in the same pause: the PRE-collection address
    /// the pool entry still holds.
    moved_from: usize,
}

impl StringTableState {
    /// Both switches read from the environment, once, at VM construction.
    ///
    /// gcg g4/rs (vg2 verdict): `CRATONVM_STRING_INTERN_RETURNS_RECEIVER` is
    /// default ON (`=0` restores the fresh pooled copy): `w2_identity_rv_*`
    /// matched HotSpot on all three collectors in both modes, and the off arm
    /// is the measured difference.
    ///
    /// gcg g6/rs (vg5 verdict): `CRATONVM_GC_WEAK_STRING_TABLE` is default ON
    /// too (`=0` restores the strong pool); it engages on Generational only
    /// ([`weak_table_engaged`]). Decided by `w2_reclaim_*` / `w2_unload_wk`
    /// (vg2), the gc-stress soundness rows `rs4_wst_stress_*` (vg4) and the
    /// open-cycle rows `rs5_wst_conc_*` (vg5: a concurrent cycle opened,
    /// `skipped>0`, output equal to the strong twin).
    pub fn from_flags() -> Self {
        let state = Self::with(
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_WEAK_STRING_TABLE"),
            cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STRING_INTERN_RETURNS_RECEIVER",
            ),
        );
        state.set_watch_young(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_GC_WEAK_STRING_TABLE_WATCH_YOUNG",
        ));
        // zt t10: `CRATONVM_ZGC_WEAK_STRING_TABLE` default ON (`=0` kill) on the
        // stop-the-world-only ZGC heap (`zgc_weak_string_table_sound`): rows
        // I1-I6 SAME as HotSpot 25.0.3 in both modes, 40/40 soundness runs
        // clean, CPU 0.28-0.89x of the strong pool (zr z1/rf rows).
        state.set_zgc_weak(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_ZGC_WEAK_STRING_TABLE",
        ));
        // zt t13/x10: the ZGC arm's "strong" answer is sticky (`=0` kill).
        state.set_zgc_strong_latch(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_ZGC_WEAK_STRING_TABLE_STRONG_LATCH",
        ));
        // zt t23/st1: the ZGC arm under a concurrent mark, a parity fix
        // (HotSpot 25's StringTable is weak on every ZGC cycle), default ON
        // (`=0` kill: the strong pool on a concurrently marking heap, as before).
        state.set_zgc_conc_weak(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT",
        ));
        // zt t27/gn5: the ZGC arm in the generational mode, a parity fix
        // (HotSpot 25's StringTable is weak; the gn page's item 13), default
        // ON (`=0` kill: the strong pool in the mode, as before).
        state.set_zgc_gen_weak(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_ZGC_GEN_WEAK_STRING_TABLE",
        ));
        // gci i21/sw: opt-in until its deciding rows pass
        // (`docs/feature-designs/jdk-only-mode.md`).
        state.set_g1_weak(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_G1_WEAK_STRING_TABLE",
        ));
        // gci i25/st2: opt-in until its deciding rows pass
        // (`docs/feature-designs/jdk-only-mode.md`).
        state.set_funnel(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_GC_STRING_TABLE_FUNNEL",
        ));
        // gci i47w/ws47: a soundness fix, default ON (`=0` kills).
        state.set_old_exact(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT",
        ));
        // gcj j1/st: opt-in (a Java-visible over-retention of one pause).
        state.set_insert_grace(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE",
        ));
        // gcj j2/rf2: opt-in, and only with the insert grace.
        state.set_hit_grace(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE",
        ));
        // gcj j3/rf3: opt-in until its deciding rows pass.
        state.set_seed_only_young(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG",
        ));
        // gcj j3/rf3: opt-in until its deciding rows pass.
        state.set_young_in_cycle(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE",
        ));
        state.dbg_line.store(
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_STRING_TABLE"),
            Ordering::Relaxed,
        );
        // gcj j1/st: a parity fix (HotSpot's uncleaned entries, growth and
        // `stringtable` lines), default ON (`=0` kills). The start size is
        // `StringTableSize`'s, set by the VM's construction.
        state.set_hotspot_ledger(hotspot_ledger_requested(), None);
        state
    }

    /// Explicit switches (tests, embedders). The young-entry watch is on
    /// ([`Self::set_watch_young`]).
    pub fn with(weak: bool, intern_receiver: bool) -> Self {
        Self {
            weak: AtomicBool::new(weak),
            intern_receiver: AtomicBool::new(intern_receiver),
            zgc_weak: AtomicBool::new(false),
            zgc_strong_latch: AtomicBool::new(false),
            zgc_strong_latched: AtomicBool::new(false),
            zgc_conc_weak: AtomicBool::new(false),
            zgc_keep_alives: AtomicU64::new(0),
            zgc_gen_weak: AtomicBool::new(false),
            sweeps: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            weak_scan_pending: AtomicBool::new(false),
            skipped: AtomicU64::new(0),
            watch_young: AtomicBool::new(true),
            watched_young: AtomicU64::new(0),
            dbg_line: AtomicBool::new(false),
            g1_weak: AtomicBool::new(false),
            g1_remark_dead: parking_lot::Mutex::new(Vec::new()),
            g1_remark_clears: AtomicU64::new(0),
            g1_dropped: AtomicU64::new(0),
            g1_untrusted_kept: AtomicU64::new(0),
            g1_keep_alives: AtomicU64::new(0),
            funnel: AtomicBool::new(false),
            young_list: parking_lot::Mutex::new(Vec::new()),
            old_base: std::sync::atomic::AtomicUsize::new(0),
            old_len: std::sync::atomic::AtomicUsize::new(0),
            young_remap: parking_lot::Mutex::new(None),
            funnel_listed: AtomicU64::new(0),
            funnel_lost_races: AtomicU64::new(0),
            young_list_sweeps: AtomicU64::new(0),
            young_list_remaps: AtomicU64::new(0),
            old_exact: AtomicBool::new(true),
            old_exact_dropped: AtomicU64::new(0),
            insert_grace: AtomicBool::new(false),
            fresh: parking_lot::Mutex::new(Vec::new()),
            grace_rooted: AtomicU64::new(0),
            hit_grace: AtomicBool::new(false),
            grace_hits: AtomicU64::new(0),
            old_screen_walks: AtomicU64::new(0),
            old_screen_walk_us: AtomicU64::new(0),
            seed_only_young: AtomicBool::new(false),
            young_in_cycle: AtomicBool::new(false),
            ledger_on: AtomicBool::new(false),
            ledger: parking_lot::Mutex::new(HotspotStringLedger::new(
                crate::runtime::serviceability::STRINGTABLE_BUCKETS,
            )),
            exit_statistics_printed: AtomicBool::new(false),
        }
    }

    /// gcj j2/rf2: count one [`OldBaseScreen`] walk that took `elapsed`.
    fn note_old_screen_walk(&self, elapsed: std::time::Duration) {
        self.old_screen_walks.fetch_add(1, Ordering::Relaxed);
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.old_screen_walk_us.fetch_add(us, Ordering::Relaxed);
    }

    /// gcj j2/rf2: `(walks, summed us)` of [`OldBaseScreen`], over the VM's
    /// life.
    pub fn old_screen_walk_census(&self) -> (u64, u64) {
        (
            self.old_screen_walks.load(Ordering::Relaxed),
            self.old_screen_walk_us.load(Ordering::Relaxed),
        )
    }

    /// gcj j2/rf2: switch the hit grace (tests; the VM reads
    /// `CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE` once at construction).
    pub fn set_hit_grace(&self, on: bool) {
        self.hit_grace.store(on, Ordering::Relaxed);
    }

    /// gcj j3/rf3: switch the seed-only young watch (tests; the VM reads
    /// `CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG` once at construction).
    pub fn set_seed_only_young(&self, on: bool) {
        self.seed_only_young.store(on, Ordering::Relaxed);
    }

    /// gcj j3/rf3: switch the in-cycle young-pause rule (tests; the VM reads
    /// `CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE` once at construction).
    pub fn set_young_in_cycle(&self, on: bool) {
        self.young_in_cycle.store(on, Ordering::Relaxed);
    }

    /// gcj j2/rf2: is the hit grace requested?
    pub fn hit_grace_requested(&self) -> bool {
        self.hit_grace.load(Ordering::Relaxed)
    }

    /// gcj j2/rf2: pool hits the hit grace noted, over the VM's life.
    pub fn grace_hits(&self) -> u64 {
        self.grace_hits.load(Ordering::Relaxed)
    }

    /// gcj j1/st: switch the insert grace (tests; the VM reads
    /// `CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE` once at construction).
    pub fn set_insert_grace(&self, on: bool) {
        self.insert_grace.store(on, Ordering::Relaxed);
    }

    /// gcj j1/st: is the insert grace requested?
    pub fn insert_grace_requested(&self) -> bool {
        self.insert_grace.load(Ordering::Relaxed)
    }

    /// gcj j1/st: Strings the insert grace rooted, over the VM's life.
    pub fn grace_rooted(&self) -> u64 {
        self.grace_rooted.load(Ordering::Relaxed)
    }

    /// gci i47w/ws47: switch [`OldBaseScreen`] (tests; the VM reads
    /// `CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT` once at construction).
    pub fn set_old_exact(&self, on: bool) {
        self.old_exact.store(on, Ordering::Relaxed);
    }

    /// gci i47w/ws47: is the old-entry base screen requested?
    pub fn old_exact_requested(&self) -> bool {
        self.old_exact.load(Ordering::Relaxed)
    }

    /// gci i47w/ws47: old entries the screen dropped, over the VM's life.
    pub fn old_exact_dropped(&self) -> u64 {
        self.old_exact_dropped.load(Ordering::Relaxed)
    }

    /// gci i25/st2: switch the insert funnel (tests; the VM reads
    /// `CRATONVM_GC_STRING_TABLE_FUNNEL` once at construction). Only
    /// meaningful before the first pooled insert: the young-entry list must
    /// have seen every insert to be the superset the sweep trusts, so turning
    /// it on with entries already pooled empties nothing but may miss them
    /// (tests set it on an empty pool).
    pub fn set_funnel(&self, on: bool) {
        self.funnel.store(on, Ordering::Relaxed);
    }

    /// gci i25/st2: is the insert funnel requested?
    pub fn funnel_requested(&self) -> bool {
        self.funnel.load(Ordering::Relaxed)
    }

    /// gci i25/st2: `(keys listed, races lost, young-list sweeps, young-list
    /// remaps)` over the VM's life, and the list's current length.
    pub fn funnel_census(&self) -> (u64, u64, u64, u64, usize) {
        (
            self.funnel_listed.load(Ordering::Relaxed),
            self.funnel_lost_races.load(Ordering::Relaxed),
            self.young_list_sweeps.load(Ordering::Relaxed),
            self.young_list_remaps.load(Ordering::Relaxed),
            self.young_list.lock().len(),
        )
    }

    /// gci i21/sw: switch the G1 arm of the weak mode (tests; the VM reads
    /// `CRATONVM_G1_WEAK_STRING_TABLE` once at construction). Only meaningful
    /// before the first G1 marking cycle, as [`Self::set_weak`].
    pub fn set_g1_weak(&self, on: bool) {
        self.g1_weak.store(on, Ordering::Relaxed);
    }

    /// gci i21/sw: is the G1 arm of the weak mode requested?
    pub fn g1_weak_requested(&self) -> bool {
        self.g1_weak.load(Ordering::Relaxed)
    }

    /// gci i21/sw: `(trusted remarks that settled, entries dropped, judged
    /// sets kept by an untrusted cleanup, keep-alive logs)` over the VM's
    /// life.
    pub fn g1_census(&self) -> (u64, u64, u64, u64) {
        (
            self.g1_remark_clears.load(Ordering::Relaxed),
            self.g1_dropped.load(Ordering::Relaxed),
            self.g1_untrusted_kept.load(Ordering::Relaxed),
            self.g1_keep_alives.load(Ordering::Relaxed),
        )
    }

    /// gch h2/jr: switch [`watch_young_pool_entries`] (tests; the VM reads
    /// `CRATONVM_GC_WEAK_STRING_TABLE_WATCH_YOUNG` once at construction).
    pub fn set_watch_young(&self, on: bool) {
        self.watch_young.store(on, Ordering::Relaxed);
    }

    /// gch h2/jr: young entries published into the watched set, over the
    /// VM's life.
    pub fn watched_young_entries(&self) -> u64 {
        self.watched_young.load(Ordering::Relaxed)
    }

    /// zr z1/rf: switch the ZGC arm of the weak mode (tests; the VM reads
    /// `CRATONVM_ZGC_WEAK_STRING_TABLE` once at construction). Only
    /// meaningful before the first collection, as [`Self::set_weak`].
    pub fn set_zgc_weak(&self, on: bool) {
        self.zgc_weak.store(on, Ordering::Relaxed);
    }

    /// zr z1/rf: is the ZGC arm of the weak mode requested?
    pub fn zgc_weak_requested(&self) -> bool {
        self.zgc_weak.load(Ordering::Relaxed)
    }

    /// zt t23/st1: switch the ZGC arm's concurrent-mark half
    /// (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`; tests, embedders: `with`
    /// leaves it off, `from_flags` reads it once). Only meaningful before the
    /// first collection, as [`Self::set_weak`].
    pub fn set_zgc_conc_weak(&self, on: bool) {
        self.zgc_conc_weak.store(on, Ordering::Relaxed);
    }

    /// zt t23/st1: is the ZGC arm's concurrent-mark half requested?
    pub fn zgc_conc_weak_requested(&self) -> bool {
        self.zgc_conc_weak.load(Ordering::Relaxed)
    }

    /// zt t27/gn5: switch the ZGC arm's generational half
    /// (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`; tests, embedders: `with` leaves
    /// it off, `from_flags` reads it once). Only meaningful before the first
    /// collection, as [`Self::set_weak`].
    pub fn set_zgc_gen_weak(&self, on: bool) {
        self.zgc_gen_weak.store(on, Ordering::Relaxed);
    }

    /// zt t27/gn5: is the ZGC arm's generational half requested?
    pub fn zgc_gen_weak_requested(&self) -> bool {
        self.zgc_gen_weak.load(Ordering::Relaxed)
    }

    /// zt t23/st1: pool hits kept alive through ZGC's SATB barrier, over the
    /// VM's life (the engagement counter of [`keep_alive_on_hit`]'s ZGC arm).
    pub fn zgc_keep_alives(&self) -> u64 {
        self.zgc_keep_alives.load(Ordering::Relaxed)
    }

    /// zt t13/x10: switch the sticky "strong" answer of the ZGC arm
    /// (`CRATONVM_ZGC_WEAK_STRING_TABLE_STRONG_LATCH`; tests, embedders: `with`
    /// leaves it off, `from_flags` reads it once).
    pub fn set_zgc_strong_latch(&self, on: bool) {
        self.zgc_strong_latch.store(on, Ordering::Relaxed);
    }

    /// zt t13/x10 (`zgc-zt11x8-wave-t10-review-residuals-SUPERSEDED-20261019.md` item 5):
    /// the ZGC arm's soundness answer, `sound` asked only while the latch is
    /// open. The gate is read per root scan and per `ldc` record decision.
    /// Moving from the weak to the strong disposition is sound (the pool
    /// roots every entry again), the other way is not: a literal resolved
    /// while the pool was strong has no `ldc` record
    /// (`constants::weak_table_needs_the_record` answered no), so a
    /// `WeakReference` to it could clear while its class lives. An embedder
    /// that turns a running heap's generational mode off
    /// (`ZgcRealHeap::set_generational_enabled(false)`) made that move. With
    /// the latch, the first "strong" answer holds for the VM's life. A
    /// launcher run never moves (every input of
    /// `VmHeap::zgc_weak_string_table_sound` is fixed for it; module doc), so
    /// the latch changes nothing there. Two relaxed loads while open.
    #[inline]
    pub(crate) fn zgc_sound_now(&self, sound: impl FnOnce() -> bool) -> bool {
        if self.zgc_strong_latched.load(Ordering::Relaxed) {
            return false;
        }
        let now = sound();
        if !now && self.zgc_strong_latch.load(Ordering::Relaxed) {
            self.zgc_strong_latched.store(true, Ordering::Relaxed);
        }
        now
    }

    /// Is the weak mode requested for this VM? (The collector gate is
    /// [`weak_table_engaged`].)
    pub fn weak_requested(&self) -> bool {
        self.weak.load(Ordering::Relaxed)
    }

    /// Does `String.intern()` pool its receiver on a miss?
    pub fn intern_returns_receiver(&self) -> bool {
        self.intern_receiver.load(Ordering::Relaxed)
    }

    /// Switch the weak mode. Only meaningful before the first collection: a
    /// pause whose roots were built under one setting must be swept under the
    /// same one (a sweep after a strong scan is harmless -- every pooled
    /// String was marked -- but a strong-scan-free pause with no sweep would
    /// leave dead entries behind).
    pub fn set_weak(&self, on: bool) {
        self.weak.store(on, Ordering::Relaxed);
    }

    /// Switch the receiver-returning `intern()`.
    pub fn set_intern_returns_receiver(&self, on: bool) {
        self.intern_receiver.store(on, Ordering::Relaxed);
    }

    /// `(sweeps, dropped)` over the VM's life.
    pub fn census(&self) -> (u64, u64) {
        (
            self.sweeps.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }

    /// Sweeps skipped because the pause's root scan kept the pool strong
    /// (gcg g4/rs), over the VM's life.
    pub fn skipped_sweeps(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }
}

/// Weak mode requested AND the heap is Generational, or (zr z1/rf, under
/// `CRATONVM_ZGC_WEAK_STRING_TABLE`) a ZGC heap that only collects
/// stop-the-world (`VmHeap::zgc_weak_string_table_sound`): the backends whose
/// every reclamation of a pooled String is followed by `update_all_roots`'s
/// sweep -- see the module doc. gci i21/sw: or (under
/// `CRATONVM_G1_WEAK_STRING_TABLE`) a G1 heap, whose reclamation of a pooled
/// String is the remark's cleanup, preceded by the remark's own clear
/// ([`g1_weak_table_engaged`]); off that switch G1 is never engaged.
///
/// Read by the `ldc` record rule (`constants.rs`): under every engaged arm a
/// literal is held by its record, not by the pool.
#[inline]
pub(crate) fn weak_table_engaged(shared: &SharedVm) -> bool {
    let table = &shared.mem.string_table;
    if !table.weak_requested() {
        return false;
    }
    let heap = &shared.mem.heap;
    heap.is_generational()
        || (table.zgc_weak_requested() && table.zgc_sound_now(|| zgc_arm_sound(table, heap)))
        || (table.g1_weak_requested() && heap.is_g1())
}

/// zt t23/st1: the soundness question the ZGC arm of [`weak_table_engaged`]
/// asks (inside the latch). Under `CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`
/// a concurrently marking heap is sound too (module doc, "Concurrent ZGC");
/// `=0`: only a heap that never marks concurrently, as before. zt t27/gn5:
/// under `CRATONVM_ZGC_GEN_WEAK_STRING_TABLE` either answer admits ZGC's
/// generational mode (module doc, "Generational ZGC").
#[inline]
fn zgc_arm_sound(table: &StringTableState, heap: &crate::memory::vm_heap::VmHeap) -> bool {
    let generational_ok = table.zgc_gen_weak_requested();
    if table.zgc_conc_weak_requested() {
        heap.zgc_weak_string_table_sound_under_concurrent_mark_for(generational_ok)
    } else {
        heap.zgc_weak_string_table_sound_for(generational_ok)
    }
}

/// zt t23/st1: the ZGC arm of [`weak_table_engaged`] with its concurrent-mark
/// half: both ZGC switches, the plain weak switch and a sound ZGC heap. Gates
/// [`keep_alive_on_hit`]'s ZGC arm. A heap that never marks concurrently
/// answers yes too (harmless: its SATB barrier is never armed).
#[inline]
pub(crate) fn zgc_conc_weak_table_engaged(shared: &SharedVm) -> bool {
    let table = &shared.mem.string_table;
    table.zgc_conc_weak_requested()
        && table.zgc_weak_requested()
        && table.weak_requested()
        && !shared.mem.heap.is_generational()
        && !shared.mem.heap.is_g1()
        && weak_table_engaged(shared)
}

/// gci i21/sw: the G1 arm of [`weak_table_engaged`] alone -- both switches
/// and a G1 heap. Gates the remark's judge / settle and the keep-alive.
#[inline]
pub(crate) fn g1_weak_table_engaged(shared: &SharedVm) -> bool {
    let table = &shared.mem.string_table;
    table.g1_weak_requested() && table.weak_requested() && shared.mem.heap.is_g1()
}

/// Does the root set being built now leave the pool OUT?
///
/// Only when [`weak_table_engaged`] and no Generational concurrent cycle is
/// open: its phase is claimed (`Idle -> InitialMark`, `try_open_cycle`) before
/// the initial mark collects its roots, and stays non-`Idle` through the
/// remark and the concurrent sweep, so every root set a concurrent cycle
/// builds keeps the pool strong (module doc, "Why concurrent cycles keep the
/// pool strong").
///
/// gcg g4/rs: a `true` answer also records that the next sweep is owed
/// ([`sweep_after_pause`]); a scan that roots the pool owes none.
///
/// gci i21/sw: on G1 no sweep is ever owed (the remark settles instead,
/// [`g1_remark_settle_interned_strings`]), so nothing is recorded there.
#[inline]
pub(crate) fn pool_is_weak_for_this_scan(shared: &SharedVm) -> bool {
    let weak = pool_left_out_now(shared);
    if weak && !shared.mem.heap.is_g1() {
        shared
            .mem
            .string_table
            .weak_scan_pending
            .store(true, Ordering::Relaxed);
    }
    weak
}

/// The rule of [`pool_is_weak_for_this_scan`] without its bookkeeping: would a
/// root scan built now leave the pool out? For diagnostics that label roots
/// (the old-mark census) and must not make a sweep owed. gcg g4/rs.
///
/// gci i21/sw: on G1, only a MARK root scan -- the initial mark, the remark
/// and the stop-the-world full mark cycle, the scans G1 takes under
/// `gc_quiescence::with_class_unload_marking` (`roots::conditional_loader_metadata`
/// keys its G1 licence on the same thread-local) -- and only under
/// [`g1_weak_table_engaged`]. Every evacuation pause roots the pool: it must,
/// because no `update_all_roots` sweep follows on G1, and a pooled String the
/// pause did not copy would leave its entry naming a freed collection-set
/// region. Generational's concurrent phase is not consulted on G1.
#[inline]
pub(crate) fn pool_left_out_now(shared: &SharedVm) -> bool {
    if shared.mem.heap.is_g1() {
        return g1_weak_table_engaged(shared)
            && cratonvm_gc::gc_quiescence::class_unload_marking();
    }
    weak_table_engaged(shared)
        && (shared.mem.concurrent_gc_state.phase() == cratonvm_gc::ConcurrentGcPhase::Idle
            || young_pause_in_cycle_leaves_pool_out(shared))
}

/// gcj j3/rf3 (`gcj-j1st-proposal-generational-weak-string-table-through-the-concurrent-cycle`,
/// the young-pause slice) — `CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE`
/// (opt-in, Generational only): with a concurrent old cycle OPEN, does a root
/// scan that is not the cycle's own (its initial mark and remark run inside
/// `gen_heap::with_no_young_sweep_following`, `in_no_young_sweep_scope`) leave
/// the pool out anyway?
///
/// Sound without the SATB keep-alive the full proposal needs, because such a
/// pause reclaims only YOUNG storage and its sweep keeps every OLD entry by
/// range (`judge_entries`' `old_untouched`; an old-reclaiming pause judges
/// them by the exact STW verdict, as on an `Idle` pause). The cycle's own two
/// scans still root the whole pool, so the concurrent marker marks every
/// pooled String it can meet and its sweep frees none; a String pooled after
/// the initial mark is young or allocated above the cycle's TAMS. What changes:
/// a dead YOUNG pooled String is dropped at the young pause that finds it
/// dead, instead of every young pause of the cycle rooting the whole pool and
/// skipping its sweep (`sweep_owed`). Off (the default): `Idle` only.
#[inline]
fn young_pause_in_cycle_leaves_pool_out(shared: &SharedVm) -> bool {
    shared.mem.string_table.young_in_cycle.load(Ordering::Relaxed)
        && shared.mem.heap.is_generational()
        && !cratonvm_gc::gen_heap::in_no_young_sweep_scope()
}

/// gch h2/jr (`docs/feature-designs/jdk-only-mode.md`,
/// fix A): publish the pool's YOUNG entries into this collection's
/// watched-referent set when the root scan left the pool out
/// ([`pool_left_out_now`], the rule [`pool_is_weak_for_this_scan`] answered
/// with for this pause: the concurrent phase cannot change inside it).
/// Returns how many it published.
///
/// Why: a young pooled String `S` the young phase keeps only for a dead old
/// holder's card survives the pause, and the epilogue sweep keeps its entry
/// ("moved" or `survived`); but the true-root major's young seed (gcd d4/n:
/// roots, finalizables and the watched set, `gen_heap.rs` `old_gen_gc_inner`
/// "young-seed") never reached `S`, so an OLD `value` array of `S` (a `new
/// String(original)` shares `original.value`; a large array is placed in old
/// gen directly) is freed under a live entry, and the next `intern()` of the
/// text answers `S` with a freed `value`. The watched set is how every other
/// weak table that hands an object back after a pause is seeded (gcd d5/r:
/// the reference processor's addresses, the JNI weak globals,
/// `jni::watch_weak_global_referents`); the pool is one more. Over-retention
/// for those Strings' old referents only, for one major. Old entries need
/// nothing: the major marks or frees them with their `value`, and the sweep
/// asks `survived` for an old address on a pause that reclaimed old storage.
///
/// Must run AFTER the reference processor's `set_watched_referents` for this
/// collection (which replaces the set): `weakref_null_referents_pre_gc` calls
/// it right after that store, on both of its arms. Cost: one range test per
/// pool entry, under the pool's read lock (the sweep already walks the pool
/// once per pause). `CRATONVM_GC_WEAK_STRING_TABLE_WATCH_YOUNG=0` turns it
/// off.
pub(crate) fn watch_young_pool_entries(shared: &SharedVm) -> usize {
    let table = &shared.mem.string_table;
    // zr z1/rf: only a Generational heap has young entries (and reads the
    // watched set); on a ZGC heap under `CRATONVM_ZGC_WEAK_STRING_TABLE` this
    // would walk the whole pool per collection to find none.
    if !table.watch_young.load(Ordering::Relaxed)
        || !shared.mem.heap.is_generational()
        || !pool_left_out_now(shared)
    {
        return 0;
    }
    let heap = &shared.mem.heap;
    // gci i25/st2: under the young-entry list, only the listed entries (a
    // superset of the young ones) are read -- O(young entries), not O(pool).
    // gcj j1/st: each row carries its String's current address (the list's
    // invariant), so the pool is not read at all.
    let young: Vec<usize> = if young_list_engaged(shared) {
        #[cfg(debug_assertions)]
        debug_check_young_list(shared, None);
        table
            .young_list
            .lock()
            .iter()
            .map(|e| e.addr)
            .filter(|&addr| heap.is_in_young_addr(addr))
            .collect()
    } else {
        shared
            .mem
            .string_pool
            .read()
            .values()
            .map(|obj| obj.as_ptr() as usize)
            .filter(|&addr| heap.is_in_young_addr(addr))
            .collect()
    };
    if young.is_empty() {
        return 0;
    }
    // gcj j3/rf3 (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`, opt-in): into
    // the seed-only row, which only a major's young seed and its watched-dead
    // verdict read (`gc_quiescence::watched_major_snapshot_for`), so the young
    // sweeps emit no identity entry for a young pooled String (its in-place
    // verdict is `addr_keyed::survived_in_place`'s). A true-root major still
    // judges every one it did not reach.
    if table.seed_only_young.load(Ordering::Relaxed) {
        cratonvm_gc::gc_quiescence::add_seed_only_referents(&young);
    } else {
        cratonvm_gc::gc_quiescence::add_watched_referents(&young);
    }
    table
        .watched_young
        .fetch_add(young.len() as u64, Ordering::Relaxed);
    young.len()
}

/// Must this pause's epilogue sweep the pool? (gcg g4/rs; g3/rw review item 7.)
///
/// Yes when a root scan left the pool out since the last sweep (`pending`),
/// and also when the concurrent phase reads `Idle` now (`idle_now`): a pause
/// whose roots were built without `collect_roots` section 5, or whose cycle
/// closed inside the pause, is swept, which is always sound. No only when every
/// scan since the last sweep rooted the pool and a concurrent cycle is open:
/// every pooled String was then marked, so the sweep would drop nothing and
/// would cost one O(pool) walk per young pause of the cycle.
#[inline]
fn sweep_owed(pending: bool, idle_now: bool) -> bool {
    pending || idle_now
}

/// One sweep's outcome.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StringTableSweep {
    /// Entries in the table when the sweep began.
    pub entries: usize,
    /// Entries kept because the pause moved their String (a `pointer_map` key;
    /// `update_all_roots` section 5 remaps them afterwards).
    pub moved: usize,
    /// Entries kept without asking the heap: an old-generation String on a
    /// pause that reclaimed no old-generation storage.
    pub old_untouched: usize,
    /// Entries kept by the in-place survival verdict.
    pub in_place: usize,
    /// Entries dropped: their String died in this pause.
    pub dropped: usize,
    /// gch h4/wr: of `dropped`, the young Strings the pause's true-root major
    /// left out of its trace (`VmHeap::true_root_watched_left_out`, under
    /// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`): still in place, but
    /// dead, and their old `value` may have been freed.
    pub left_out: usize,
}

/// Drop every pool entry whose String did not survive the pause that just ran.
///
/// Must run inside that pause's epilogue, in the allocation-free window of the
/// caller's `addr_keyed::InPlaceVerdict` (whose `survived` is `survived`), and
/// BEFORE `update_all_roots` section 5 remaps the pool: the verdicts are over
/// PRE-collection addresses. Changes no value; only removes entries.
///
/// Per entry, with its pre-collection address `a`:
///
/// * `a` is a `pointer_map` key: the String moved (or is an identity survivor);
///   kept, and section 5 remaps it;
/// * `a` is in the old generation and the pause reclaimed no old-generation
///   storage (`gc_quiescence::old_gen_reclaimed_last_cycle`, the collecting
///   thread's flag for this very pause): a minor cycle leaves every old-gen
///   object where it was, so kept without asking the heap -- the common case
///   on a young pause, and the reason the sweep costs one range test per
///   tenured literal rather than a heap query;
/// * otherwise `survived(a)` decides.
///
/// gch h4/wr: asked before all three, a young String the pause's true-root
/// major left out of its trace (`VmHeap::true_root_watched_left_out`, under
/// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`; it is watched because
/// [`watch_young_pool_entries`] published it) is dropped: it is still in place,
/// so `survived` would keep it, but only a dead old holder named it and its old
/// `value` may have been freed. HotSpot drops the entry (`S` is unreachable).
///
/// Dropping a live entry would break `intern()`'s identity contract (two equal
/// texts answering two objects); keeping a dead one would hand the next
/// `intern()` a freed address. `survived` never says yes for reclaimed space
/// and never says no for a survivor (`addr_keyed::survived_in_place`), which is
/// the property both directions need.
///
/// The pool lock is not held while the heap is asked.
///
/// gci i25/st2: under [`young_list_engaged`] a pause that reclaimed no old
/// storage judges only the young-entry list ([`sweep_young_listed_entries`]:
/// the same verdicts, over the only entries that could get one other than
/// `old_untouched`); a pause that reclaimed old storage walks the whole pool
/// as before. Either way the list is pruned afterwards.
pub(crate) fn sweep_dead_interned_strings(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> StringTableSweep {
    let stats = if young_list_engaged(shared) {
        sweep_young_listed_entries(shared, pointer_map, survived)
    } else {
        sweep_whole_pool(shared, pointer_map, survived).stats
    };
    let table = &shared.mem.string_table;
    table.sweeps.fetch_add(1, Ordering::Relaxed);
    table
        .dropped
        .fetch_add(stats.dropped as u64, Ordering::Relaxed);
    stats
}

/// gci i25/st2: the sweep over the young-entry list (module doc, "The insert
/// funnel").
///
/// * On a pause that reclaimed old storage: [`sweep_whole_pool`], then the
///   rows whose entry it dropped leave the list (by address: the rows still
///   hold PRE-collection addresses, which is what the walk judged). The rest
///   keep their PRE-collection address; the whole-pool remap that follows
///   (no young remap is recorded) advances them with the pool.
/// * Otherwise: judges every row's address (its PRE-collection address: the
///   list's invariant) with [`judge_entries`] exactly as the whole-pool walk
///   would (same `pointer_map`, old-arena range, left-out and `survived`
///   rules), removes the dead entries BY KEY (only if the key still names the
///   judged String), advances every surviving row to its POST-collection
///   address (a moved one keeps its old address in `moved_from` for
///   [`remap_pool_after_pause`]), and drops the rows whose String now lies in
///   the old arena (promoted; a moved promoted row is handed to the remap
///   record, which still owes its pool entry the move).
///
/// gcj j1/st (`gci-i47w-oa47-...-RETIRED-UNFIXED-20261018`): no listed text is
/// hashed except a dead entry's (to remove it) -- and a moved one's, once, in
/// the remap -- and nothing is sorted or cloned. The text-keyed list it
/// replaces hashed every listed text four times per pause.
///
/// Equivalence with the walk: every unlisted entry's String was inside the
/// old arena at insertion and is still there (an old String is never moved
/// or made young by a pause that reclaimed no old storage), so the walk
/// would have kept it as `old_untouched` without asking anyone. The arena is
/// read after the collection, so it covers any growth.
fn sweep_young_listed_entries(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> StringTableSweep {
    let table = &shared.mem.string_table;
    let old_reclaimed = cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle();
    let old_range = if old_reclaimed {
        None
    } else {
        Some(shared.mem.heap.old_gen_info())
    };
    // The superset and exactness the shortcut rests on, checked against the
    // whole pool in debug builds (a writer that bypassed
    // `pool_insert_if_absent` would trip it).
    #[cfg(debug_assertions)]
    debug_check_young_list(shared, old_range);
    let Some(old_range) = old_range else {
        let judged = sweep_whole_pool(shared, pointer_map, survived);
        if !judged.dead.is_empty() {
            table
                .young_list
                .lock()
                .retain(|e| !judged.dead.contains(&e.addr));
        }
        return judged.stats;
    };
    note_old_arena(shared, old_range);
    let list = std::mem::take(&mut *table.young_list.lock());
    // gch h4/wr: one snapshot, as the whole-pool walk takes it.
    let left_out = shared.mem.heap.true_root_watched_left_out_snapshot();
    let verdict = {
        // addr-keyed: this pause's PRE-collection addresses of the listed entries; dropped before the sweep returns.
        let addrs: Vec<usize> = list.iter().map(|e| e.addr).collect();
        judge_entries(
            &addrs,
            pointer_map,
            old_range,
            &|a| !left_out.is_empty() && left_out.binary_search(&a).is_ok(),
            survived,
        )
    };
    if !verdict.dead.is_empty() {
        let mut pool = shared.mem.string_pool.write();
        let mut removed: Vec<&str> = Vec::new();
        for e in &list {
            if verdict.dead.contains(&e.addr)
                && pool.get(&*e.key).map(|obj| obj.as_ptr() as usize) == Some(e.addr)
            {
                pool.remove(&*e.key);
                removed.push(&e.key);
            }
        }
        drop(pool);
        table.note_dropped_texts(removed.into_iter());
    }
    let (kept, promoted) = advance_young_rows(list, pointer_map, &verdict.dead, old_range);
    *table.young_remap.lock() = Some((shared.mem.heap.collection_count(), promoted));
    table.young_list_sweeps.fetch_add(1, Ordering::Relaxed);
    restore_young_list(shared, kept);
    verdict.stats
}

/// gcj j1/st: the young-list sweep's row update, pure (no pool, no heap).
/// Drops the rows whose PRE-collection address is in `dead`; advances every
/// other row through `pointer_map` (a moved row remembers its old address in
/// `moved_from`); answers `(kept, promoted)`: `kept` are the rows whose
/// POST-collection address lies outside `old_range`, `promoted` the MOVED rows
/// whose String now lies inside it (they leave the list, but the remap still
/// owes their pool entry the move). A row already inside the old arena that
/// did not move leaves the list with nothing owed.
fn advance_young_rows(
    list: Vec<YoungEntry>,
    pointer_map: &cratonvm_types::PointerMap,
    dead: &rustc_hash::FxHashSet<usize>, // addr-keyed: this pause's PRE-collection verdict
    old_range: (usize, usize),
) -> (Vec<YoungEntry>, Vec<YoungEntry>) {
    let mut kept = Vec::with_capacity(list.len());
    let mut promoted = Vec::new();
    for mut e in list {
        if dead.contains(&e.addr) {
            continue;
        }
        if let Some(&post) = pointer_map.get(&e.addr) {
            if post != e.addr {
                e.moved_from = e.addr;
                e.addr = post;
            }
        }
        if !in_arena(e.addr, old_range) {
            kept.push(e);
        } else if e.moved_from != 0 {
            promoted.push(e);
        }
    }
    (kept, promoted)
}

/// gcj j1/st: debug builds only -- the young list's invariant (field doc of
/// `StringTableState::young_list`), checked against the whole pool: every row
/// names its entry's current String, and (with the old arena, on a pause that
/// reclaimed no old storage) every entry outside the arena has a row. Takes the
/// pool's read lock, then the list's (the module's lock order).
#[cfg(debug_assertions)]
fn debug_check_young_list(shared: &SharedVm, old_range: Option<(usize, usize)>) {
    let pool = shared.mem.string_pool.read();
    let list = shared.mem.string_table.young_list.lock();
    // addr-keyed: debug-only, this call's addresses; dropped before it returns.
    let mut listed: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    for e in list.iter() {
        let pooled = pool.get(&*e.key).map(|obj| obj.as_ptr() as usize);
        debug_assert!(
            pooled == Some(e.addr) && e.moved_from == 0,
            "string_table: young-list row {:?} names {:#x} (moved_from {:#x}) but the pool holds {pooled:?}",
            e.key,
            e.addr,
            e.moved_from
        );
        listed.insert(e.addr);
    }
    if let Some(range) = old_range {
        for (k, obj) in pool.iter() {
            let a = obj.as_ptr() as usize;
            debug_assert!(
                in_arena(a, range) || listed.contains(&a),
                "string_table: pooled String {k:?} at {a:#x} lies outside the old arena but is not on the young list"
            );
        }
    }
}

/// The whole-pool sweep: every entry's address copied out and judged (the
/// sweep without the young-entry list). Answers the verdict (gcj j1/st: the
/// young-list sweep drops the rows of the entries it condemned).
fn sweep_whole_pool(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> Judged {
    let addrs: Vec<usize> = shared
        .mem
        .string_pool
        .read()
        .values()
        .map(|obj| obj.as_ptr() as usize)
        .collect();
    let old_untouched = !cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle();
    let old_range = if old_untouched && !addrs.is_empty() {
        shared.mem.heap.old_gen_info()
    } else {
        (0, 0)
    };
    // gch h4/wr: one snapshot (empty on almost every pause), not a heap lock
    // per entry.
    let left_out = shared.mem.heap.true_root_watched_left_out_snapshot();
    // gci i47w/ws47: an OLD entry on a pause that reclaimed old storage is
    // kept only if its address is still an object base (see `OldBaseScreen`).
    let screen = OldBaseScreen::for_pause(shared);
    let screened = std::cell::Cell::new(0u64);
    let verdict = judge_entries(
        &addrs,
        pointer_map,
        old_range,
        &|a| !left_out.is_empty() && left_out.binary_search(&a).is_ok(),
        &|a| screened_survived(a, survived, &|x| screen.admits(x), &screened),
    );
    if screened.get() > 0 {
        shared
            .mem
            .string_table
            .old_exact_dropped
            .fetch_add(screened.get(), Ordering::Relaxed);
    }
    if !verdict.dead.is_empty() {
        let mut pool = shared.mem.string_pool.write();
        let mut removed: Vec<String> = Vec::new();
        let ledger = shared.mem.string_table.hotspot_ledger_on();
        pool.retain(|text, obj| {
            let keep = !verdict.dead.contains(&(obj.as_ptr() as usize));
            if !keep && ledger {
                removed.push(text.clone());
            }
            keep
        });
        drop(pool);
        shared.mem.string_table.note_dropped_texts(removed.iter().map(String::as_str));
    }
    verdict
}

/// gci i47w/ws47: the in-place verdict `survived`, then [`OldBaseScreen`]'s
/// `admits`; `screened` counts the addresses only the screen refused. Pure.
#[inline]
fn screened_survived(
    addr: usize,
    survived: &dyn Fn(usize) -> bool,
    admits: &dyn Fn(usize) -> bool,
    screened: &std::cell::Cell<u64>,
) -> bool {
    if !survived(addr) {
        return false;
    }
    if admits(addr) {
        return true;
    }
    screened.set(screened.get() + 1);
    false
}

/// gci i47w/ws47 (`gcg-g4oo-oome-ladder-review-findings-FIXED-20261016` item 5):
/// the in-place verdict is not exact for an OLD address on a pause whose
/// old-generation collection SLID survivors.
///
/// A sliding compaction (`OldGen::compact_around_pins`, the requested major
/// of the non-moving path under `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`; the
/// moving path's `compact_walked`) records `old -> new` only for an object
/// that MOVED, plus an identity entry for a WATCHED one that stayed put. An
/// old pooled String is not watched (only young entries are,
/// [`watch_young_pool_entries`]), so a dead one at `a` is no map key, and
/// after the slide `a` can lie inside a survivor that slid over it. The
/// in-place verdict then answers "survived": `is_addr_live` is the old
/// free-list test (`a` is allocated again) and the structural
/// `is_object_address` accepts interior bytes that decode as a header; the
/// relocation screen catches only a survivor whose BASE landed on `a`. The
/// entry stayed, the next `intern()` of its text answered `a`, and the
/// caller stored an interior address of another object as a String
/// (`Field.name` of a reflected field: `this.value` null, an
/// `AnnotatedElement` "header"; `vi40-fg` FG4, bisected by `ws47`:
/// `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS=0` and `CRATONVM_OLD_SWEEP_JIT=0`
/// each SAME 3/3). HotSpot's StringTable never hands out a collected String.
///
/// The screen: on a Generational pause that reclaimed old storage
/// (`gc_quiescence::old_gen_reclaimed_last_cycle`), an old-generation
/// address the in-place verdict kept must also be an object BASE of the old
/// generation's walk after the collection; otherwise the entry is dead. A
/// survivor that stayed put is a walked base, so no live entry is dropped
/// unless it lies in a walk gap (an unparseable stretch); dropping that one
/// only costs identity (the next `intern()` pools a new String), never
/// soundness. The walk runs at most once per pause, and only when an old
/// entry reaches the in-place verdict.
/// `CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT=0` turns it off.
pub(crate) struct OldBaseScreen<'a> {
    shared: Option<&'a SharedVm>,
    /// `(base, len)` of the old generation; `(0, 0)` when unarmed.
    // addr-keyed: the arena's own bounds, never an object address; read within this pause.
    old_range: (usize, usize),
    // addr-keyed: this pause's post-collection old-generation bases; dropped before the sweep returns.
    bases: std::cell::OnceCell<Vec<usize>>,
}

impl<'a> OldBaseScreen<'a> {
    /// Armed iff the switch is on, the heap is Generational and the pause
    /// reclaimed old storage.
    pub(crate) fn for_pause(shared: &'a SharedVm) -> Self {
        let armed = old_base_screen_armed(
            shared.mem.string_table.old_exact_requested(),
            shared.mem.heap.is_generational(),
            cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle(),
        );
        Self {
            shared: armed.then_some(shared),
            old_range: if armed {
                shared.mem.heap.old_gen_info()
            } else {
                (0, 0)
            },
            bases: std::cell::OnceCell::new(),
        }
    }

    /// May the in-place verdict's "kept" stand for `addr`?
    pub(crate) fn admits(&self, addr: usize) -> bool {
        let Some(shared) = self.shared else {
            return true;
        };
        old_entry_admitted(self.old_range, addr, || {
            self.bases
                .get_or_init(|| {
                    // gcj j2/rf2 (the j1-st old-screen proposal's deciding
                    // counter): one clock pair per walk, at most once a pause.
                    let t = std::time::Instant::now();
                    let bases = old_gen_object_bases(&shared.mem.heap);
                    shared.mem.string_table.note_old_screen_walk(t.elapsed());
                    bases
                })
                .binary_search(&addr)
                .is_ok()
        })
    }
}

/// [`OldBaseScreen`]'s arming rule, pure.
#[inline]
fn old_base_screen_armed(switch_on: bool, generational: bool, old_reclaimed: bool) -> bool {
    switch_on && generational && old_reclaimed
}

/// [`OldBaseScreen::admits`]'s rule, pure: an address outside the old
/// generation `(base, len)` is admitted unasked; an old one only if
/// `is_base` (the post-collection walk holds it as an object base).
#[inline]
fn old_entry_admitted(
    old_range: (usize, usize),
    addr: usize,
    is_base: impl FnOnce() -> bool,
) -> bool {
    let (base, len) = old_range;
    if len == 0 || addr.wrapping_sub(base) >= len {
        return true;
    }
    is_base()
}

/// The old generation's object bases, ascending (sorted defensively). Empty
/// off Generational.
fn old_gen_object_bases(heap: &crate::memory::vm_heap::VmHeap) -> Vec<usize> {
    match heap {
        crate::memory::vm_heap::VmHeap::Generational(h) => {
            let (objects, _gaps) = h.old_gen_lock().walk_objects_with_gaps();
            let mut bases: Vec<usize> = objects.iter().map(|&(p, _)| p as usize).collect();
            bases.sort_unstable();
            bases
        }
        _ => Vec::new(),
    }
}

/// The verdicts of [`sweep_dead_interned_strings`], split out so they can be
/// tested without a heap.
struct Judged {
    stats: StringTableSweep,
    // addr-keyed: one sweep's PRE-collection verdict; dropped before the sweep returns.
    dead: rustc_hash::FxHashSet<usize>,
}

/// `old_range` is `(base, len)` of the old generation, or `(0, 0)` when the
/// pause reclaimed old-generation storage (then no address is trusted by
/// range). `left_out` is the true-root major's dead-watched verdict (gch
/// h4/wr), asked first.
fn judge_entries(
    addrs: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
    old_range: (usize, usize),
    left_out: &dyn Fn(usize) -> bool,
    survived: &dyn Fn(usize) -> bool,
) -> Judged {
    let mut stats = StringTableSweep {
        entries: addrs.len(),
        ..StringTableSweep::default()
    };
    let mut dead = rustc_hash::FxHashSet::default();
    let (old_base, old_len) = old_range;
    for &addr in addrs {
        if left_out(addr) {
            stats.left_out += 1;
            stats.dropped += 1;
            dead.insert(addr);
        } else if pointer_map.contains_key(&addr) {
            stats.moved += 1;
        } else if old_len != 0 && addr.wrapping_sub(old_base) < old_len {
            stats.old_untouched += 1;
        } else if survived(addr) {
            stats.in_place += 1;
        } else {
            stats.dropped += 1;
            dead.insert(addr);
        }
    }
    Judged { stats, dead }
}

/// The sweep as `update_all_roots` drives it: only when [`weak_table_engaged`]
/// and a sweep is owed ([`sweep_owed`], gcg g4/rs), with the diagnostic line
/// `CRATONVM_DBG_STRING_TABLE=1` asks for.
pub(crate) fn sweep_after_pause(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) {
    sweep_after_pause_inner(shared, pointer_map, survived);
    // gcj j1/st item 3: HotSpot notifies its String table once per
    // collection, swept or not (`StringTable::gc_notification`).
    hotspot_gc_notification(shared);
}

/// [`sweep_after_pause`]'s sweep, before the ledger's notification.
fn sweep_after_pause_inner(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) {
    // gci i25/st2: a young remap set is valid for the pause whose sweep made
    // it, and only up to that pause's section 5: clear any older one first, so
    // a skipped or whole-pool sweep can never leave one behind for
    // `remap_pool_after_pause`.
    if shared.mem.string_table.funnel_requested() {
        *shared.mem.string_table.young_remap.lock() = None;
    }
    // gcj j1/st: the insert grace lasts until this pause's epilogue: its rows
    // were rooted by this pause's scans and are PRE-collection addresses, so
    // they are dropped here, before any remap could make them stale.
    clear_insert_grace(shared);
    // gci i21/sw: never on G1 -- its evacuation pauses root the pool, and its
    // remark clears it ([`g1_remark_settle_interned_strings`]); G1's in-place
    // verdict is region-granular, so a sweep here could only cost.
    if !weak_table_engaged(shared) || shared.mem.heap.is_g1() {
        return;
    }
    let table = &shared.mem.string_table;
    let pending = table.weak_scan_pending.swap(false, Ordering::Relaxed);
    let idle_now = shared.mem.concurrent_gc_state.phase() == cratonvm_gc::ConcurrentGcPhase::Idle;
    if !sweep_owed(pending, idle_now) {
        table.skipped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let s = sweep_dead_interned_strings(shared, pointer_map, survived);
    // gch h3/jx: latched per VM (`from_flags`), no environment read in the pause.
    if table.dbg_line.load(Ordering::Relaxed) {
        let (sweeps, dropped_total) = shared.mem.string_table.census();
        // gci i25/st2: the funnel's counters are appended (older scrapes read
        // the fields before them unchanged). `entries` is the number JUDGED:
        // the young list's length on a young-list sweep.
        // gcj j1/st: `young_keys` is the young list's length (rows, no longer
        // bare keys; the field name is kept for the scrapes); `grace_rooted`
        // is appended; gcj j2/rf2: `grace_hits`, `old_screen_walks` and
        // `old_screen_walk_us` (cumulative) after it.
        let (old_walks, old_walk_us) = table.old_screen_walk_census();
        let (listed, lost_races, young_sweeps, young_remaps, young_rows) =
            table.funnel_census();
        eprintln!(
            "[string-table] gc={} entries={} moved={} old_untouched={} in_place={} dropped={} sweeps={} dropped_total={} skipped={} watched_young={} left_out={} pool={} funnel_listed={} funnel_lost_races={} young_list_sweeps={} young_list_remaps={} young_keys={} old_exact_dropped={} grace_rooted={} grace_hits={} old_screen_walks={} old_screen_walk_us={} zgc_keep_alives={}",
            shared.mem.heap.collection_count(),
            s.entries,
            s.moved,
            s.old_untouched,
            s.in_place,
            s.dropped,
            sweeps,
            dropped_total,
            table.skipped_sweeps(),
            table.watched_young_entries(),
            s.left_out,
            shared.mem.string_pool.read().len(),
            listed,
            lost_races,
            young_sweeps,
            young_remaps,
            young_rows,
            table.old_exact_dropped(),
            table.grace_rooted(),
            table.grace_hits(),
            old_walks,
            old_walk_us,
            // zt t23/st1: appended (older scrapes read the fields before it).
            table.zgc_keep_alives(),
        );
    }
}

/// `String.intern()` on `receiver`, whose text is `text`, under
/// `CRATONVM_STRING_INTERN_RETURNS_RECEIVER`: the pooled String equal to it if
/// there is one, else `receiver` itself, now pooled. Allocates nothing on the
/// heap, so it cannot fail for want of heap.
///
/// `text` must be `receiver`'s exact content: the caller decodes it, and only a
/// string Rust text can represent comes here (an unpaired surrogate goes to the
/// units-keyed pool instead -- `lang_string::intern_unrepresentable`, which
/// already answers the receiver on a miss).
///
/// No collection can run between the receiver's read and its insertion: this is
/// Rust code on a mutator between safepoints, and the collector's own pool
/// accesses take the same lock.
///
/// gci i21/sw: a HIT answers through [`keep_alive_on_hit`]; the miss pools the
/// receiver, which the caller holds (no keep-alive needed, module doc "G1").
pub(crate) fn intern_receiver(shared: &SharedVm, receiver: ObjectRef, text: &str) -> ObjectRef {
    if let Some(obj) = shared.mem.string_pool.read().get(text).copied() {
        return keep_alive_on_hit(shared, obj);
    }
    // gci i25/st2: the one insert funnel (lists a young receiver; no key copy
    // when another thread pooled the text since the read probe).
    if funnel_engaged(shared) {
        let (pooled, inserted) = pool_insert_if_absent(shared, text, receiver);
        return if inserted {
            pooled
        } else {
            keep_alive_on_hit(shared, pooled)
        };
    }
    let pooled = {
        let mut pool = shared.mem.string_pool.write();
        *pool.entry(text.to_string()).or_insert(receiver)
    };
    if pooled == receiver {
        pooled
    } else {
        keep_alive_on_hit(shared, pooled)
    }
}

/// gci i21/sw: the keep-alive every pool HIT answers through (the G1 half of
/// `gcg-g2w-proposal-weak-string-table-on-g1-and-zgc-20261001`, step 2).
/// Returns `obj`.
///
/// Under [`g1_weak_table_engaged`] the G1 mark root scans leave the pool out,
/// so a pooled String nothing else reached at the snapshot is unmarked. A
/// mutator that fetches it from the pool while the concurrent mark runs and
/// stores it into an object the marker already scanned (or into one allocated
/// since mark start, which lies above TAMS and is never scanned) would hide it
/// from the marker, and the remark would judge it dead while it is reachable:
/// its entry would leave the pool (a later `intern()` of the text answering a
/// second object) and cleanup could free it under the holder. Logging the
/// String through the SATB barrier while the queue is active marks it, as
/// HotSpot's `StringTable` lookup does through its `WeakHandle` (and as
/// `Reference.get()` does here: `NativeContextImpl::gc_reference_keep_alive`).
///
/// Off the G1 arm this is one relaxed load and a return. A pool hit that only
/// lands in a ROOT (a pinned local, a JNI global, a compiled `ldc` slot) needs
/// no keep-alive -- the remark rescans the roots -- but taking it is harmless.
///
/// gcj j2/rf2 (the g4rs grace residual): under [`hit_grace_engaged`] (opt-in)
/// the hit is noted for the insert grace too, so a Rust caller holding a
/// HIT's String in an unpinned local across one non-moving pause keeps it,
/// as the insert grace does for a String the call pooled. Off, one more
/// relaxed load. May be called with the pool's read lock held (order
/// `string_pool` -> `fresh`).
#[inline]
pub(crate) fn keep_alive_on_hit(shared: &SharedVm, obj: ObjectRef) -> ObjectRef {
    let table = &shared.mem.string_table;
    if hit_grace_engaged(shared) {
        table.fresh.lock().push(obj);
        table.grace_hits.fetch_add(1, Ordering::Relaxed);
    }
    if table.g1_weak_requested() && g1_weak_table_engaged(shared) && shared.mem.heap.satb_armed() {
        shared
            .mem
            .heap
            .satb_barrier(crate::types::Value::Object(Some(obj)));
        table.g1_keep_alives.fetch_add(1, Ordering::Relaxed);
    } else if table.zgc_conc_weak_requested()
        && shared.mem.heap.zgc_concurrent_mark_active()
        && zgc_conc_weak_table_engaged(shared)
    {
        // zt t23/st1: the ZGC twin (module doc, "Concurrent ZGC"). The
        // concurrent mark start left the pool out, so a String only the pool
        // held at mark start is unmarked; logging it through ZGC's SATB
        // pre-barrier (`ZgcRealHeap::satb_pre_barrier`: per-thread buffer,
        // drained into the marker or replayed at the mark end) marks it, as
        // HotSpot's `StringTable` lookup does through its `WeakHandle` peek
        // and as `Reference.get()` does here. The barrier re-tests
        // `mark_active` itself; the cycle flag above only keeps the common
        // no-cycle hit at three relaxed loads. zt t23/zb8: through
        // `referent_keep_alive`, which is that SATB log unflipped and the
        // load barrier's mark step (`mark_live`) under the color flip.
        shared
            .mem
            .heap
            .referent_keep_alive(crate::types::Value::Object(Some(obj)));
        table.zgc_keep_alives.fetch_add(1, Ordering::Relaxed);
    }
    obj
}

/// gci i25/st2: is the insert funnel on for this VM (module doc, "The insert
/// funnel")? One relaxed load.
#[inline]
pub(crate) fn funnel_engaged(shared: &SharedVm) -> bool {
    shared.mem.string_table.funnel_requested()
}

/// gci i25/st2: does the funnel keep the young-entry list, and do the sweep,
/// the watch and the remap read it? The funnel switch, a Generational heap
/// (the only backend with a young generation the pool sweep serves), and the
/// weak table requested (on Generational that is [`weak_table_engaged`]): the
/// list is read only by the weak table's per-pause passes. Every input is
/// latched at VM construction, so the answer never changes between an insert
/// and the pause that trusts the list.
#[inline]
pub(crate) fn young_list_engaged(shared: &SharedVm) -> bool {
    let table = &shared.mem.string_table;
    table.funnel_requested() && table.weak_requested() && shared.mem.heap.is_generational()
}

/// gcj j1/st (`gcg-g4rs-weak-string-table-frees-pooled-strings-rust-callers-hold-RETIRED-UNFIXED-20261018`,
/// "Fix", the blanket mitigation): is the insert grace on for this VM? The
/// switch (`CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE`, opt-in), the weak
/// table requested and a Generational heap -- the only backend whose
/// non-moving pauses free a pooled String a Rust local still holds. G1 and ZGC
/// never note, root or clear anything (one relaxed load).
#[inline]
pub(crate) fn insert_grace_engaged(shared: &SharedVm) -> bool {
    let table = &shared.mem.string_table;
    table.insert_grace_requested() && table.weak_requested() && shared.mem.heap.is_generational()
}

/// gcj j2/rf2 (`gcg-g4rs-...-RETIRED-UNFIXED-20261018`, the grace's residual):
/// is the hit grace on? `CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE` (opt-in) on
/// top of [`insert_grace_engaged`]. The insert grace covers a String a pooled
/// constructor POOLED; a pool HIT hands a Rust caller a String that may have
/// no Java holder left (its last one died since it was pooled), and the same
/// unpinned-local-across-a-pause shape frees it. Under this switch every hit
/// answered through [`keep_alive_on_hit`] (the pooled constructors, the
/// `exceptions.rs` message cache, a pool hit of `NativeContext::create_string`) is noted in
/// the same `fresh` list and rooted through the next pause. Cost: one lock and
/// one push per hit while on; Java-visible over-retention of one pause for a
/// String hit since the last pause and dead otherwise.
#[inline]
pub(crate) fn hit_grace_engaged(shared: &SharedVm) -> bool {
    shared.mem.string_table.hit_grace_requested() && insert_grace_engaged(shared)
}

/// gcj j1/st: a pooled constructor (`vm_object.rs`: `create_java_string_reporting`,
/// `try_create_java_string`, `try_create_java_string_reporting`, on both
/// funnel arms) has just pooled `obj`. Under [`insert_grace_engaged`] the String
/// is kept alive through the next pause (rooted by every root scan of it that
/// leaves the pool out, [`push_insert_grace_roots`]), so a Rust caller that
/// holds the answer in an unpinned local across ONE non-moving pause -- the
/// shape g4rs is about -- keeps a live String. HotSpot has no such window
/// (a `Handle` roots it), and the grace is over-retention: a String pooled
/// since the last pause and referenced by nothing else dies one pause later
/// than on HotSpot (a `WeakReference` to it clears one collection late).
/// That is why it is opt-in. `String.intern()`'s receiver path
/// ([`intern_receiver`]) does not note: its String is the Java caller's own.
///
/// May be called with the pool's write lock held (order `string_pool` ->
/// `fresh`).
#[inline]
pub(crate) fn note_pooled_by_constructor(shared: &SharedVm, obj: ObjectRef) {
    if insert_grace_engaged(shared) {
        shared.mem.string_table.fresh.lock().push(obj);
    }
}

/// gcj j1/st: `roots::collect_roots` section 5, when the scan leaves the pool
/// out: root the Strings [`note_pooled_by_constructor`] noted since the last
/// pause's epilogue. A no-op (one relaxed load) off [`insert_grace_engaged`].
pub(crate) fn push_insert_grace_roots(shared: &SharedVm, roots: &mut Vec<ObjectRef>) {
    if !insert_grace_engaged(shared) {
        return;
    }
    roots.extend(shared.mem.string_table.fresh.lock().iter().copied());
}

/// gcj j1/st: end the grace at a pause's epilogue ([`sweep_after_pause`]): the
/// rows were rooted by this pause's scans (and are PRE-collection addresses),
/// so they are dropped before any remap. Counts them in `grace_rooted`.
fn clear_insert_grace(shared: &SharedVm) {
    let table = &shared.mem.string_table;
    if !table.insert_grace_requested() {
        return;
    }
    let mut fresh = table.fresh.lock();
    if !fresh.is_empty() {
        table
            .grace_rooted
            .fetch_add(fresh.len() as u64, Ordering::Relaxed);
        fresh.clear();
    }
}

/// `addr` inside `(base, len)`? `len == 0` contains nothing.
#[inline]
fn in_arena(addr: usize, (base, len): (usize, usize)) -> bool {
    len != 0 && addr.wrapping_sub(base) < len
}

/// gci i25/st2: the old-generation arena as last read, reading it once when
/// nothing was read yet. Called with NO pool lock held (it may take the
/// old-gen lock once).
fn cached_old_arena(shared: &SharedVm) -> (usize, usize) {
    let table = &shared.mem.string_table;
    let len = table.old_len.load(Ordering::Acquire);
    if len != 0 {
        return (table.old_base.load(Ordering::Relaxed), len);
    }
    let (base, len) = shared.mem.heap.old_gen_info();
    note_old_arena(shared, (base, len));
    (base, len)
}

/// Store the arena `(base, len)` for [`cached_old_arena`] (base first, so a
/// reader that sees the length sees the base).
fn note_old_arena(shared: &SharedVm, (base, len): (usize, usize)) {
    let table = &shared.mem.string_table;
    table.old_base.store(base, Ordering::Relaxed);
    table.old_len.store(len, Ordering::Release);
}

/// gci i25/st2: must the funnel list a String it pools at `addr`? Under
/// [`young_list_engaged`], every String outside the old-generation arena:
/// a superset of the young ones (an entry is young only at insertion), so a
/// sweep that judges only the listed entries misses no entry the whole-pool
/// walk would have asked about. Decided BEFORE the pool lock is taken.
fn listable_at_insert(shared: &SharedVm, addr: usize) -> bool {
    young_list_engaged(shared) && !in_arena(addr, cached_old_arena(shared))
}

/// gci i25/st2: the one pool insert (module doc, "The insert funnel"): pool
/// `fresh` under `text` unless an entry exists, and answer `(pooled, inserted)`.
/// A miss inserts `fresh` (and lists it, with its address, when
/// [`listable_at_insert`]); an existing entry -- another thread pooled the
/// text since the caller's read probe -- is answered as is, without a key
/// copy, and the caller answers it through [`keep_alive_on_hit`].
///
/// Holds only the pool's write lock (and the young list's lock under it: order
/// `string_pool` -> `young_list`); allocates nothing on the heap and takes no
/// heap lock under it.
pub(crate) fn pool_insert_if_absent(
    shared: &SharedVm,
    text: &str,
    fresh: ObjectRef,
) -> (ObjectRef, bool) {
    let addr = fresh.as_ptr() as usize;
    let list = listable_at_insert(shared, addr);
    let table = &shared.mem.string_table;
    let mut pool = shared.mem.string_pool.write();
    let existing = pool.get(text).copied();
    if let Some(winner) = existing {
        drop(pool);
        table.funnel_lost_races.fetch_add(1, Ordering::Relaxed);
        return (winner, false);
    }
    pool.insert(text.to_string(), fresh);
    if list {
        table.young_list.lock().push(YoungEntry {
            key: Box::from(text),
            addr,
            moved_from: 0,
        });
        table.funnel_listed.fetch_add(1, Ordering::Relaxed);
    }
    (fresh, true)
}

/// gci i25/st2: the pooled constructors' MISS path under the funnel (the
/// caller's read probe missed). `alloc` builds the String with no pool lock
/// held (it must not collect: the `try_alloc_*_full` constructors); then
/// [`pool_insert_if_absent`]. Answers `(String, allocated_by_this_call)`:
/// `false` when another thread won the race (its String, kept alive like any
/// hit) or when `alloc` failed but the text was pooled meanwhile (a hit needs
/// no heap). `None` only when `alloc` failed and the text is still unpooled.
///
/// gcj j1/st: a String this call pooled is noted for the insert grace
/// ([`note_pooled_by_constructor`]); every caller is a pooled constructor.
pub(crate) fn intern_miss_via_funnel(
    shared: &SharedVm,
    text: &str,
    alloc: impl FnOnce() -> Option<ObjectRef>,
) -> Option<(ObjectRef, bool)> {
    let Some(fresh) = alloc() else {
        let hit = shared.mem.string_pool.read().get(text).copied();
        return hit.map(|obj| (keep_alive_on_hit(shared, obj), false));
    };
    let (pooled, inserted) = pool_insert_if_absent(shared, text, fresh);
    if inserted {
        note_pooled_by_constructor(shared, pooled);
        Some((pooled, true))
    } else {
        Some((keep_alive_on_hit(shared, pooled), false))
    }
}

/// Put `rows` back into the young list (merged with anything pushed while
/// they were out; nothing is, inside a pause).
fn restore_young_list(shared: &SharedVm, rows: Vec<YoungEntry>) {
    let mut list = shared.mem.string_table.young_list.lock();
    if list.is_empty() {
        *list = rows;
    } else {
        list.extend(rows);
    }
}

/// gci i25/st2: `update_all_roots` section 5 (the pool remap) with the
/// young-list shortcut. Returns the entries it remapped.
///
/// When THIS pause's sweep ran over the young list (it recorded this
/// collection's count, [`sweep_young_listed_entries`]), only the rows it saw
/// move -- those still listed (`moved_from` set) and the promoted ones it
/// handed over -- are remapped: the pause reclaimed no old storage, so no old
/// String moved (the invariant the sweep's `old_untouched` rule already rests
/// on), and every String that could have moved is listed. Otherwise --
/// switch off, another collector, a pause that reclaimed old storage, a
/// skipped sweep -- every entry is probed, exactly as section 5 always did;
/// gcj j1/st: and, under the young list, every row's address is advanced the
/// same way (the rows still hold PRE-collection addresses then), which keeps
/// the list exact. Debug builds check the shortcut against the whole pool.
pub(crate) fn remap_pool_after_pause(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> usize {
    let table = &shared.mem.string_table;
    let owed = if table.funnel_requested() {
        table.young_remap.lock().take()
    } else {
        None
    };
    // A record of another collection (never made by the production order:
    // every `sweep_after_pause` clears it first) means its rows were already
    // advanced; they are only un-flagged after the whole walk below.
    let mut rows_advanced = false;
    if let Some((at, mut promoted)) = owed {
        if at == shared.mem.heap.collection_count() {
            let mut pool = shared.mem.string_pool.write();
            let mut list = table.young_list.lock();
            #[cfg(debug_assertions)]
            {
                let owed_keys: rustc_hash::FxHashSet<&str> = list
                    .iter()
                    .filter(|e| e.moved_from != 0)
                    .chain(promoted.iter())
                    .map(|e| &*e.key)
                    .collect();
                for (k, obj) in pool.iter() {
                    let a = obj.as_ptr() as usize;
                    debug_assert!(
                        pointer_map.get(&a).map_or(true, |&n| n == a)
                            || owed_keys.contains(k.as_str()),
                        "string_table: a moved pooled String ({k:?} at {a:#x}) is missing from the young remap set"
                    );
                }
            }
            let mut n = 0;
            for e in list
                .iter_mut()
                .filter(|e| e.moved_from != 0)
                .chain(promoted.iter_mut())
            {
                if let Some(obj) = pool.get_mut(&*e.key) {
                    if remap_pool_value(obj, pointer_map) {
                        n += 1;
                    }
                    debug_assert_eq!(
                        obj.as_ptr() as usize,
                        e.addr,
                        "string_table: young-list row {:?} and its remapped entry disagree",
                        e.key
                    );
                }
                e.moved_from = 0;
            }
            table.young_list_remaps.fetch_add(1, Ordering::Relaxed);
            return n;
        }
        rows_advanced = true;
    }
    let mut pool = shared.mem.string_pool.write();
    let mut n = 0;
    for obj in pool.values_mut() {
        if remap_pool_value(obj, pointer_map) {
            n += 1;
        }
    }
    if young_list_engaged(shared) {
        for e in table.young_list.lock().iter_mut() {
            if !rows_advanced {
                if let Some(&post) = pointer_map.get(&e.addr) {
                    e.addr = post;
                }
            }
            e.moved_from = 0;
        }
    }
    n
}

/// One entry's remap: `true` when `pointer_map` moved it elsewhere.
#[inline]
fn remap_pool_value(obj: &mut ObjectRef, pointer_map: &cratonvm_types::PointerMap) -> bool {
    let old_addr = obj.as_ptr() as usize;
    match pointer_map.get(&old_addr) {
        Some(&new_addr) if new_addr != old_addr => {
            // Safety: new_addr was produced by the GC's pointer map and points
            // at the relocated String (section 5's own contract).
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            true
        }
        _ => false,
    }
}

/// gci i21/sw: the G1 remark's verdict on the pool, inside the remark callback
/// (`gc_and_alloc.rs`: `g1_final_remark_cleanup`'s callback and
/// `g1_remark_reference_pass`), right after the JNI weak-global clear and on
/// the same completed bitmap `is_live` (`G1Collector::is_live_after_mark`:
/// bitmap below TAMS, live above it, outside the heap, or in a region recycled
/// since the snapshot). The keep closure is marked by then on both arms of
/// `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP`; `keep` itself is exempted anyway,
/// as the JNI clear exempts it.
///
/// Records the dead set for [`g1_remark_settle_interned_strings`] (REPLACING
/// any earlier one: a remark that unwound before its settle leaves nothing
/// for the next) and returns its size. Changes no entry: the cleanup that
/// follows decides whether the mark is trusted. A no-op (0) off
/// [`g1_weak_table_engaged`].
///
/// Sound because the mark scans of this cycle left the pool out and every pool
/// hit since the snapshot was logged ([`keep_alive_on_hit`]): an unmarked
/// pooled String is reachable from nothing but the pool. Nothing moves in a
/// remark, so no remap. The pool's read lock only; no mutator runs.
pub(crate) fn g1_remark_judge_interned_strings(
    shared: &SharedVm,
    is_live: &dyn Fn(usize) -> bool,
    keep: &[usize],
) -> usize {
    if !g1_weak_table_engaged(shared) {
        return 0;
    }
    let addrs: Vec<usize> = shared
        .mem
        .string_pool
        .read()
        .values()
        .map(|obj| obj.as_ptr() as usize)
        .collect();
    let dead = judge_g1_remark_entries(&addrs, keep, is_live);
    let n = dead.len();
    *shared.mem.string_table.g1_remark_dead.lock() = dead;
    n
}

/// The verdict of [`g1_remark_judge_interned_strings`], without a heap: every
/// address neither kept nor live.
fn judge_g1_remark_entries(
    addrs: &[usize],
    keep: &[usize],
    is_live: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    // addr-keyed: pause-local, one remark callback; nothing moves or is freed before it drops.
    let keep: rustc_hash::FxHashSet<usize> = keep.iter().copied().collect();
    addrs
        .iter()
        .copied()
        .filter(|a| !keep.contains(a) && !is_live(*a))
        .collect()
}

/// gci i21/sw: drop the entries the remark judged dead
/// ([`g1_remark_judge_interned_strings`]), right after
/// `VmHeap::g1_final_remark_and_cleanup` returned `completed`, still inside
/// the remark's pause (no mutator runs, so nothing read the pool since the
/// judge). Returns the entries dropped.
///
/// Only when the remark completed AND its cleanup trusted its closure
/// (`G1Collector::parked_mirror_keyed_view(false).0`, the `last_mark_trusted`
/// that cleanup stores): an untrusted cleanup (`retain_all`: an implausible
/// gray entry or an undrained closure) freed nothing and proved nothing, so its
/// unmarked Strings may be reachable; their entries stay and the next cycle
/// judges them again. A trusted cleanup may just have freed the judged
/// Strings' regions, so the entries must go now, before any mutator can hit
/// them. The judged set is taken (emptied) on every call.
pub(crate) fn g1_remark_settle_interned_strings(shared: &SharedVm, completed: bool) -> usize {
    let table = &shared.mem.string_table;
    let dead = std::mem::take(&mut *table.g1_remark_dead.lock());
    if dead.is_empty() {
        if completed && g1_weak_table_engaged(shared) {
            table.g1_remark_clears.fetch_add(1, Ordering::Relaxed);
            g1_remark_dbg_line(shared, 0, 0, true);
        }
        return 0;
    }
    if !completed {
        // The callback runs only in a completed remark, so a judged set here
        // is a stale one (an unwound remark's): dropped, never applied.
        return 0;
    }
    if !g1_last_mark_trusted(shared) {
        table.g1_untrusted_kept.fetch_add(1, Ordering::Relaxed);
        g1_remark_dbg_line(shared, dead.len(), 0, false);
        return 0;
    }
    let dropped = drop_judged_entries(shared, &dead);
    table.g1_remark_clears.fetch_add(1, Ordering::Relaxed);
    table.g1_dropped.fetch_add(dropped as u64, Ordering::Relaxed);
    g1_remark_dbg_line(shared, dead.len(), dropped, true);
    dropped
}

/// Remove every entry whose String is at one of `dead` (pre-collection
/// addresses; nothing moved since the judge). Returns how many left.
fn drop_judged_entries(shared: &SharedVm, dead: &[usize]) -> usize {
    if dead.is_empty() {
        return 0;
    }
    // addr-keyed: pause-local, judged in this pause's remark; nothing moved since.
    let dead_set: rustc_hash::FxHashSet<usize> = dead.iter().copied().collect();
    let mut pool = shared.mem.string_pool.write();
    let before = pool.len();
    let mut removed: Vec<String> = Vec::new();
    let ledger = shared.mem.string_table.hotspot_ledger_on();
    pool.retain(|text, obj| {
        let keep = !dead_set.contains(&(obj.as_ptr() as usize));
        if !keep && ledger {
            removed.push(text.clone());
        }
        keep
    });
    let dropped = before - pool.len();
    drop(pool);
    shared.mem.string_table.note_dropped_texts(removed.iter().map(String::as_str));
    dropped
}

/// Did the last G1 cleanup trust its closure? `false` off G1.
fn g1_last_mark_trusted(shared: &SharedVm) -> bool {
    match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::G1(g1) => g1.parked_mirror_keyed_view(false).0,
        _ => false,
    }
}

/// `CRATONVM_DBG_STRING_TABLE=1`'s line for a G1 remark settle (latched per VM).
fn g1_remark_dbg_line(shared: &SharedVm, judged: usize, dropped: usize, trusted: bool) {
    let table = &shared.mem.string_table;
    if !table.dbg_line.load(Ordering::Relaxed) {
        return;
    }
    let (clears, dropped_total, untrusted, keep_alives) = table.g1_census();
    eprintln!(
        "[string-table] g1-remark gc={} judged={} dropped={} trusted={} entries={} clears={} dropped_total={} untrusted_kept={} keep_alives={}",
        shared.mem.heap.collection_count(),
        judged,
        dropped,
        trusted,
        shared.mem.string_pool.read().len(),
        clears,
        dropped_total,
        untrusted,
        keep_alives,
    );
}

// ---------------------------------------------------------------------------
// HotSpot's String table bookkeeping (gcj j1/st items 2 and 3)
// ---------------------------------------------------------------------------

/// HotSpot 25's `StringTable` grows to at most 2^24 buckets (`END_SIZE`).
const HOTSPOT_STRINGTABLE_MAX_SIZE: usize = 1 << 24;

/// HotSpot 25's `PREF_AVG_LIST_LEN`: past this load factor the concurrent
/// work grows the table.
const HOTSPOT_PREF_AVG_LIST_LEN: f64 = 2.0;

/// HotSpot 25's `CLEAN_DEAD_HIGH_WATER_MARK`: past this dead factor a
/// collection triggers the concurrent clean.
const HOTSPOT_CLEAN_DEAD_HIGH_WATER_MARK: f64 = 0.5;

/// gcj j1/st items 2 and 3: the String table's HotSpot-visible bookkeeping
/// (`StringTable` in `stringTable.cpp`, HotSpot 25.0.4), which the
/// `stringtable` log lines, `jcmd VM.stringtable` and
/// `-XX:+PrintStringTableStatistics` report. This VM's pool drops a dead
/// String's entry inside the pause that found it dead; HotSpot's table keeps
/// the dead entry (its `WeakHandle` cleared) until the service thread's
/// concurrent work cleans or grows the table, and counts it as an entry until
/// then. So the ledger keeps, per VM:
///
/// * `size`: the bucket count, `StringTableSize` rounded up to a power of two
///   (2^16 by default), doubled by each grow up to 2^24;
/// * `dead`: the bucket hashes of the entries dropped since the last clean or
///   grow -- HotSpot's uncleaned items;
/// * `has_work`: a collection triggered the concurrent work and it has not
///   run yet.
///
/// Measured on HotSpot 25.0.4 (Serial and G1, `-Xlog:stringtable*=trace`):
/// every collection logs `Uncleaned items:<n>` with the uncleaned count (a
/// dead entry is counted again by every later collection until it is
/// cleaned), and triggers the concurrent work when the dead factor exceeds
/// the load factor or 0.5, or the load factor exceeds 2; the work grows the
/// table when the load factor exceeds 2 (dead entries go with the grow:
/// the next collection logs `Uncleaned items:0`), else cleans it.
#[derive(Debug)]
pub(crate) struct HotspotStringLedger {
    size: usize,
    dead: Vec<u32>,
    has_work: bool,
}

impl HotspotStringLedger {
    fn new(size: usize) -> Self {
        Self {
            size,
            dead: Vec::new(),
            has_work: false,
        }
    }
}

/// `StringTableSize`'s start size: `log2i_ceil`, so the next power of two
/// (HotSpot: `-XX:StringTableSize=1000` logs `Start size: 1024 (10)`),
/// clamped to the table's maximum. Pure.
pub(crate) fn hotspot_stringtable_start_size(requested: Option<usize>) -> usize {
    requested
        .map(|n| n.max(1).next_power_of_two().min(HOTSPOT_STRINGTABLE_MAX_SIZE))
        .unwrap_or(crate::runtime::serviceability::STRINGTABLE_BUCKETS)
}

/// `[trace][stringtable] Start size: <n> (<log2 n>)`, as HotSpot's
/// `StringTable::create_table` logs it. Pure.
pub(crate) fn hotspot_start_size_text(size: usize) -> String {
    format!("Start size: {size} ({})", size.trailing_zeros())
}

/// `CRATONVM_GEN_STRINGTABLE_HOTSPOT_LEDGER` (gcj j1/st; default ON, `=0`
/// kill: no uncleaned entries are kept, the table never grows, and no
/// collection logs a `stringtable` line). Latched per VM ([`StringTableState::from_flags`]).
pub(crate) fn hotspot_ledger_requested() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_STRINGTABLE_HOTSPOT_LEDGER")
}

impl StringTableState {
    /// gcj j1/st: arm the HotSpot ledger with `StringTableSize`'s start size
    /// (`None`: HotSpot's default). Called once, at VM construction.
    pub fn set_hotspot_ledger(&self, on: bool, start_size: Option<usize>) {
        self.ledger_on.store(on, Ordering::Relaxed);
        *self.ledger.lock() = HotspotStringLedger::new(hotspot_stringtable_start_size(start_size));
    }

    /// Is the HotSpot ledger armed for this VM?
    pub fn hotspot_ledger_on(&self) -> bool {
        self.ledger_on.load(Ordering::Relaxed)
    }

    /// The table's bucket count (grown by the concurrent work).
    pub fn hotspot_table_size(&self) -> usize {
        self.ledger.lock().size
    }

    /// The bucket hashes of the entries dropped and not yet cleaned (a
    /// snapshot, for the statistics).
    pub(crate) fn hotspot_uncleaned_hashes(&self) -> Vec<u32> {
        self.ledger.lock().dead.clone()
    }

    /// Record the entries a collection dropped (their texts), as HotSpot's
    /// table keeps them: dead and uncleaned. One lock; nothing when the
    /// ledger is off or nothing died.
    pub(crate) fn note_dropped_texts<'a>(&self, texts: impl Iterator<Item = &'a str>) {
        if !self.hotspot_ledger_on() {
            return;
        }
        let hashes: Vec<u32> = texts.map(crate::runtime::serviceability::string_table_bucket_hash).collect();
        if !hashes.is_empty() {
            self.ledger.lock().dead.extend(hashes);
        }
    }
}

/// gcj j1/st item 3: HotSpot's `StringTable::gc_notification`, once per
/// collection, after the pause's String-table sweep (or its absence):
/// `[trace][stringtable] Uncleaned items:<n>`, then, when the factors call
/// for it, `[debug][stringtable] Concurrent work triggered, live factor: <%g>
/// dead factor: <%g>` and the work is owed to [`hotspot_concurrent_work`].
/// Logs to `-Xlog` outputs only; the pause does no I/O when neither tag is
/// selected. A no-op when the ledger is off.
pub(crate) fn hotspot_gc_notification(shared: &SharedVm) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let table = &shared.mem.string_table;
    if !table.hotspot_ledger_on() {
        return;
    }
    let live = shared.mem.string_pool.read().len();
    let mut ledger = table.ledger.lock();
    let num_dead = ledger.dead.len();
    log_unified(
        &[LogTag::StringTable],
        LogLevel::Trace,
        &format!("Uncleaned items:{num_dead}"),
    );
    if ledger.has_work {
        return;
    }
    let size = ledger.size as f64;
    let load_factor = (live + num_dead) as f64 / size;
    let dead_factor = num_dead as f64 / size;
    if dead_factor > load_factor
        || load_factor > HOTSPOT_PREF_AVG_LIST_LEN
        || dead_factor > HOTSPOT_CLEAN_DEAD_HIGH_WATER_MARK
    {
        log_unified(
            &[LogTag::StringTable],
            LogLevel::Debug,
            &format!(
                "Concurrent work triggered, live factor: {} dead factor: {}",
                crate::runtime::serviceability::c_format_g(load_factor),
                crate::runtime::serviceability::c_format_g(dead_factor)
            ),
        );
        ledger.has_work = true;
    }
}

/// gcj j1/st item 3: HotSpot's `StringTable::do_concurrent_work` (the
/// service thread's), run by the collecting thread once the world is
/// released and the pause's `-Xlog:gc` line is written, as HotSpot's lines
/// follow it: `[debug][stringtable,perf] Concurrent work, live factor: <%g>`,
/// then a grow (`[trace][stringtable] Started to grow`, `[debug]
/// [stringtable,perf] Grow, <s> secs`, `[debug][stringtable] Grown to
/// size:<n>`) when the load factor exceeds 2 and the table is below 2^24
/// buckets, else a clean (`[debug][stringtable,perf] Clean, <s> secs`,
/// `[debug][stringtable] Cleaned <dead> of <entries>`). Either way the
/// uncleaned entries are gone. HotSpot's `Cleaned <n> of <m>` counts the
/// concurrent hash table's per-node visits, which double-count; this VM
/// prints the entries removed and the entries the table held. One load
/// when no work is owed.
pub(crate) fn hotspot_concurrent_work(shared: &SharedVm) {
    use crate::runtime::serviceability::c_format_g;
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let table = &shared.mem.string_table;
    if !table.hotspot_ledger_on() {
        return;
    }
    let live = shared.mem.string_pool.read().len();
    let mut ledger = table.ledger.lock();
    if !ledger.has_work {
        return;
    }
    let start = std::time::Instant::now();
    let num_dead = ledger.dead.len();
    let load_factor = (live + num_dead) as f64 / ledger.size as f64;
    log_unified(
        &[LogTag::StringTablePerf],
        LogLevel::Debug,
        &format!("Concurrent work, live factor: {}", c_format_g(load_factor)),
    );
    if load_factor > HOTSPOT_PREF_AVG_LIST_LEN && ledger.size < HOTSPOT_STRINGTABLE_MAX_SIZE {
        log_unified(&[LogTag::StringTable], LogLevel::Trace, "Started to grow");
        ledger.size *= 2;
        ledger.dead.clear();
        log_unified(
            &[LogTag::StringTablePerf],
            LogLevel::Debug,
            &format!("Grow, {} secs", c_format_g(start.elapsed().as_secs_f64())),
        );
        log_unified(
            &[LogTag::StringTable],
            LogLevel::Debug,
            &format!("Grown to size:{}", ledger.size),
        );
    } else {
        ledger.dead.clear();
        log_unified(
            &[LogTag::StringTablePerf],
            LogLevel::Debug,
            &format!("Clean, {} secs", c_format_g(start.elapsed().as_secs_f64())),
        );
        log_unified(
            &[LogTag::StringTable],
            LogLevel::Debug,
            &format!("Cleaned {num_dead} of {}", live + num_dead),
        );
    }
    ledger.has_work = false;
}

/// gcj j1/st items 1 and 2: the String table's statistics census as HotSpot
/// prints it (`jcmd VM.stringtable`, `-XX:+PrintStringTableStatistics`): the
/// live texts, plus the ledger's uncleaned entries over its current size.
/// The pool's read lock is held while the texts are counted.
pub(crate) fn hotspot_stringtable_census(
    shared: &SharedVm,
    verbose: bool,
) -> crate::runtime::serviceability::StringTableCensus {
    let table = &shared.mem.string_table;
    let (dead, size) = if table.hotspot_ledger_on() {
        let ledger = table.ledger.lock();
        (ledger.dead.clone(), ledger.size)
    } else {
        (Vec::new(), crate::runtime::serviceability::STRINGTABLE_BUCKETS)
    };
    let pool = shared.mem.string_pool.read();
    crate::runtime::serviceability::stringtable_census_with(
        pool.keys().map(String::as_str),
        &dead,
        size,
        verbose,
    )
}

/// gcj j1/st item 2: the Symbol table's statistics census
/// (`crate::runtime::serviceability::symboltable_census` over the class
/// reader's interned UTF-8 pool).
pub(crate) fn hotspot_symboltable_census(verbose: bool) -> crate::runtime::serviceability::StringTableCensus {
    let texts = cratonvm_types::intern::global_pool().snapshot();
    crate::runtime::serviceability::symboltable_census(texts.iter().map(|t| &**t), verbose)
}

/// gcj j1/st item 2: HotSpot 25's `-XX:+PrintStringTableStatistics` exit
/// block, once per VM and only under the flag (`VmConfig::print_string_table_statistics`).
/// HotSpot prints it on every exit (`main` returning, an uncaught exception,
/// `System.exit`, `Runtime.halt`), after the shutdown hooks and the
/// `safepoint,stats` exit summary. The block (`print_statistics` in `java.cpp`: `SymbolTable::dump(tty)` then
/// `StringTable::dump(tty)`), on stdout: each table's `[debug][<table>,perf]
/// GetStatistics, <s> secs` line (when selected), then its statistics. No
/// `Shared ... Table statistics:` block: this VM has no CDS archive, as
/// HotSpot `-Xshare:off`.
pub fn print_string_table_statistics_at_exit(shared: &SharedVm) {
    if !shared.config.print_string_table_statistics
        || shared
            .mem
            .string_table
            .exit_statistics_printed
            .swap(true, Ordering::AcqRel)
    {
        return;
    }
    print_string_table_statistics(shared);
}

/// [`print_string_table_statistics_at_exit`]'s block, unconditionally.
pub fn print_string_table_statistics(shared: &SharedVm) {
    use crate::runtime::serviceability::{c_format_g, default_vm_stringtable_text, default_vm_symboltable_text};
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    use std::io::Write as _;
    let t0 = std::time::Instant::now();
    let symbols = hotspot_symboltable_census(false);
    log_unified(
        &[LogTag::SymbolTablePerf],
        LogLevel::Debug,
        &format!("GetStatistics, {} secs", c_format_g(t0.elapsed().as_secs_f64())),
    );
    {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(default_vm_symboltable_text(&symbols).as_bytes());
        let _ = out.flush();
    }
    let t1 = std::time::Instant::now();
    let strings = hotspot_stringtable_census(shared, false);
    log_unified(
        &[LogTag::StringTablePerf],
        LogLevel::Debug,
        &format!("GetStatistics, {} secs", c_format_g(t1.elapsed().as_secs_f64())),
    );
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(default_vm_stringtable_text(&strings).as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// Pool `text` without leaving its address in the caller's frame.
    #[inline(never)]
    fn intern_and_forget(shared: &SharedVm, text: &str) {
        let s = crate::vm::create_java_string(shared, text);
        std::hint::black_box(s);
    }

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: only compared and hashed, never dereferenced, except by the
        // heap's own address predicates, which validate it first.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// gcj j1/st items 2 and 3: a dropped entry stays one of HotSpot's
    /// uncleaned items (counted by the statistics, no literal bytes) until
    /// the concurrent work its collection triggered cleans it; a table past
    /// load factor 2 grows instead. `StringTableSize` rounds up to a power of
    /// two. The kill keeps nothing.
    #[test]
    fn gcj_j1st_ledger_keeps_uncleaned_entries_until_the_concurrent_work() {
        assert_eq!(hotspot_stringtable_start_size(Some(1000)), 1024);
        assert_eq!(hotspot_stringtable_start_size(None), 65_536);
        assert_eq!(hotspot_start_size_text(65_536), "Start size: 65536 (16)");
        assert_eq!(hotspot_start_size_text(32_768), "Start size: 32768 (15)");
        let shared = gen_vm();
        let table = &shared.mem.string_table;
        table.set_weak(true);
        table.set_hotspot_ledger(true, Some(1000));
        assert_eq!(table.hotspot_table_size(), 1024);
        let map = cratonvm_types::PointerMap::default();
        {
            let mut pool = shared.mem.string_pool.write();
            for i in 0..600usize {
                pool.insert(format!("gcj-j1st-dead-{i}"), at(0x4000 + i * 0x20));
            }
        }
        // 600 of 1024 dead: dead factor 0.586 > 0.5, load factor 0.586 < 2.
        sweep_after_pause(&shared, &map, &|_| false);
        assert!(shared.mem.string_pool.read().is_empty());
        assert_eq!(table.hotspot_uncleaned_hashes().len(), 600);
        let census = hotspot_stringtable_census(&shared, false);
        assert_eq!((census.entries, census.literal_bytes, census.buckets), (600, 0, 1024));
        assert!(table.ledger.lock().has_work);
        hotspot_concurrent_work(&shared);
        assert!(table.hotspot_uncleaned_hashes().is_empty(), "cleaned");
        assert_eq!(table.hotspot_table_size(), 1024, "a clean, not a grow");
        assert!(!table.ledger.lock().has_work);
        // 3000 live entries over 1024 buckets: load factor 2.93 > 2 grows.
        {
            let mut pool = shared.mem.string_pool.write();
            for i in 0..3000usize {
                pool.insert(format!("gcj-j1st-live-{i}"), at(0x10_0000 + i * 0x20));
            }
        }
        sweep_after_pause(&shared, &map, &|_| true);
        assert_eq!(shared.mem.string_pool.read().len(), 3000);
        assert!(table.ledger.lock().has_work);
        hotspot_concurrent_work(&shared);
        assert_eq!(table.hotspot_table_size(), 2048);
        // The kill: nothing kept, nothing owed.
        shared.mem.string_pool.write().clear();
        table.set_hotspot_ledger(false, None);
        shared
            .mem
            .string_pool
            .write()
            .insert("gcj-j1st-killed".to_string(), at(0x4000));
        sweep_after_pause(&shared, &map, &|_| false);
        assert!(table.hotspot_uncleaned_hashes().is_empty());
        assert!(!table.ledger.lock().has_work);
    }

    /// gcj j1/st: the ledger is a default-ON parity fix with a `=0` kill.
    #[test]
    fn gcj_j1st_hotspot_ledger_is_default_on_with_a_kill() {
        use cratonvm_types::flags::with_thread_overrides;
        with_thread_overrides(&[("CRATONVM_GEN_STRINGTABLE_HOTSPOT_LEDGER", None)], || {
            assert!(hotspot_ledger_requested());
        });
        with_thread_overrides(&[("CRATONVM_GEN_STRINGTABLE_HOTSPOT_LEDGER", Some("0"))], || {
            assert!(!hotspot_ledger_requested());
        });
    }

    /// gci i47w/ws47: the screen's arming rule -- all three conditions.
    #[test]
    fn gci_i47w_ws47_screen_arms_only_on_a_generational_old_reclaim_with_the_switch() {
        assert!(old_base_screen_armed(true, true, true));
        assert!(!old_base_screen_armed(false, true, true), "=0 kills");
        assert!(!old_base_screen_armed(true, false, true), "G1 / ZGC");
        assert!(!old_base_screen_armed(true, true, false), "a minor pause");
    }

    /// gci i47w/ws47: outside the old range nothing is asked; inside it only
    /// a walked base is admitted.
    #[test]
    fn gci_i47w_ws47_only_an_old_object_base_is_admitted() {
        let old = (0x10_0000, 0x1_0000);
        let bases = [0x10_0000usize, 0x10_0040, 0x10_0100];
        let asked = std::cell::Cell::new(0);
        let is_base = |a: usize| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                bases.binary_search(&a).is_ok()
            }
        };
        assert!(old_entry_admitted(old, 0x3000, is_base(0x3000)), "young");
        assert!(
            old_entry_admitted(old, 0x11_0000, is_base(0x11_0000)),
            "past the end"
        );
        assert_eq!(asked.get(), 0, "a non-old address is never asked");
        assert!(old_entry_admitted(old, 0x10_0040, is_base(0x10_0040)));
        assert!(
            !old_entry_admitted(old, 0x10_0048, is_base(0x10_0048)),
            "interior of a survivor"
        );
        assert!(
            !old_entry_admitted(old, 0x10_0200, is_base(0x10_0200)),
            "freed tail"
        );
        assert!(
            old_entry_admitted((0, 0), 0x10_0048, is_base(0x10_0048)),
            "unarmed"
        );
    }

    /// gci i47w/ws47 -- the producer's shape. A compaction slid survivor X
    /// over dead pooled String D (`D` inside X, not X's base): `D` is no map
    /// key, and the in-place verdict says "survived" (allocated, plausible
    /// bytes). Without the screen the entry is kept (the freed String is
    /// handed out again); with it, it is dropped. A String that stayed put is
    /// a base and kept; a moved one is kept through the map as before.
    #[test]
    fn gci_i47w_ws47_a_dead_old_entry_inside_a_slid_survivor_is_dropped() {
        let old = (0x10_0000, 0x1_0000);
        let stayed = 0x10_0000usize; // pinned / below the first gap
        let slid_dest = 0x10_0040usize; // X now starts here...
        let dead = 0x10_0058usize; // ...and D's old address is inside X
        let moved_src = 0x10_0400usize;
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x10_0300, slid_dest); // X moved
        map.insert(moved_src, 0x10_0080); // a pooled String that moved
        let bases = [stayed, slid_dest, 0x10_0080];
        let survived = |_: usize| true; // the in-place verdict's answer
        let addrs = [stayed, dead, moved_src];

        let unscreened = judge_entries(&addrs, &map, (0, 0), &|_| false, &survived);
        assert!(unscreened.dead.is_empty(), "the hole: D is kept");

        let screened = std::cell::Cell::new(0u64);
        let admits = |a: usize| old_entry_admitted(old, a, || bases.binary_search(&a).is_ok());
        let judge_screened = |a: usize| screened_survived(a, &survived, &admits, &screened);
        let j = judge_entries(&addrs, &map, (0, 0), &|_| false, &judge_screened);
        assert_eq!(j.stats.moved, 1);
        assert_eq!(j.stats.in_place, 1);
        assert_eq!(j.stats.dropped, 1);
        assert!(j.dead.contains(&dead) && j.dead.len() == 1);
        assert_eq!(screened.get(), 1, "counted as the screen's drop");
    }

    /// gci i47w/ws47: the switch is latched per VM, default ON, `=0` off.
    #[test]
    fn gci_i47w_ws47_old_exact_switch_is_default_on_per_vm() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT", None)],
            || assert!(StringTableState::from_flags().old_exact_requested()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_WEAK_STRING_TABLE_OLD_EXACT", Some("0"))],
            || assert!(!StringTableState::from_flags().old_exact_requested()),
        );
        assert!(StringTableState::with(true, true).old_exact_requested());
    }

    /// gci i47w/ws47: on a live Generational VM after a pause that reclaimed
    /// nothing old, the screen is unarmed and admits everything; armed, a
    /// freshly allocated old-gen-or-young String is judged by the walk only
    /// when it is old.
    #[test]
    fn gci_i47w_ws47_screen_is_unarmed_without_an_old_reclaim() {
        let shared = gen_vm();
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
        let screen = OldBaseScreen::for_pause(&shared);
        assert!(screen.shared.is_none());
        assert!(screen.admits(0x10_0058));
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        let armed = OldBaseScreen::for_pause(&shared);
        assert!(armed.shared.is_some());
        let s = crate::vm::create_java_string(&shared, "gci-i47w-ws47-screen");
        let a = s.as_ptr() as usize;
        let (base, len) = shared.mem.heap.old_gen_info();
        let old = a.wrapping_sub(base) < len;
        assert!(
            armed.admits(a),
            "a live object is admitted (young unasked, old as a walked base: old={old})"
        );
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
    }

    /// The pure verdict: a moved entry and an old entry on a minor pause are
    /// kept without asking `survived`; the rest is `survived`'s answer.
    #[test]
    fn gcg_w2_judge_entries_keeps_moved_and_untouched_old_and_asks_for_the_rest() {
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        let old = (0x10_0000, 0x1_0000);
        let asked = std::cell::RefCell::new(Vec::new());
        let survived = |a: usize| {
            asked.borrow_mut().push(a);
            a == 0x3000
        };
        let j = judge_entries(
            &[0x1000, 0x10_0040, 0x3000, 0x4000],
            &map,
            old,
            &|_| false,
            &survived,
        );
        assert_eq!(
            j.stats,
            StringTableSweep {
                entries: 4,
                moved: 1,
                old_untouched: 1,
                in_place: 1,
                dropped: 1,
                left_out: 0,
            }
        );
        assert!(j.dead.contains(&0x4000) && j.dead.len() == 1);
        assert_eq!(
            *asked.borrow(),
            vec![0x3000, 0x4000],
            "moved and old entries are never asked"
        );

        // Positive control: the same old address on a pause that reclaimed
        // old storage (no trusted range) IS asked, and dies when it is dead.
        let j = judge_entries(&[0x10_0040], &map, (0, 0), &|_| false, &|_| false);
        assert_eq!(j.stats.dropped, 1);
        assert_eq!(j.stats.old_untouched, 0);
    }

    /// gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`): a young
    /// pooled String the true-root major left out of its trace is dropped
    /// whatever else says it survived -- an identity map entry (the young
    /// sweep's, had it not been removed) or the in-place verdict (it IS still
    /// in place) -- and `survived` is never asked about it. Every other entry
    /// is judged as before.
    #[test]
    fn gch_wr_judge_entries_drops_a_string_the_true_root_trace_left_out() {
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x1000); // identity: left out anyway
        map.insert(0x2000, 0x9000); // moved, not left out
        let asked = std::cell::RefCell::new(Vec::new());
        let survived = |a: usize| {
            asked.borrow_mut().push(a);
            true
        };
        let left_out = |a: usize| a == 0x1000 || a == 0x3000;
        let j = judge_entries(
            &[0x1000, 0x2000, 0x3000, 0x4000],
            &map,
            (0, 0),
            &left_out,
            &survived,
        );
        assert_eq!(
            j.stats,
            StringTableSweep {
                entries: 4,
                moved: 1,
                old_untouched: 0,
                in_place: 1,
                dropped: 2,
                left_out: 2,
            }
        );
        assert!(j.dead.contains(&0x1000) && j.dead.contains(&0x3000) && j.dead.len() == 2);
        assert_eq!(
            *asked.borrow(),
            vec![0x4000],
            "a left-out entry is never asked"
        );
    }

    /// The sweep removes exactly the entries the verdict condemns and leaves
    /// every other entry's VALUE untouched (section 5 remaps afterwards).
    #[test]
    fn gcg_w2_sweep_drops_dead_entries_and_changes_no_value() {
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        {
            let mut pool = shared.mem.string_pool.write();
            pool.insert("gcg-w2-dead".to_string(), at(0x4000));
            pool.insert("gcg-w2-live".to_string(), at(0x3000));
            pool.insert("gcg-w2-moved".to_string(), at(0x1000));
        }
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        let s = sweep_dead_interned_strings(&shared, &map, &|a| a == 0x3000);
        assert_eq!(s.dropped, 1);
        let pool = shared.mem.string_pool.read();
        assert!(
            pool.get("gcg-w2-dead").is_none(),
            "a dead String's entry is dropped"
        );
        assert_eq!(
            pool.get("gcg-w2-live").map(|o| o.as_ptr() as usize),
            Some(0x3000)
        );
        assert_eq!(
            pool.get("gcg-w2-moved").map(|o| o.as_ptr() as usize),
            Some(0x1000),
            "the sweep never remaps; update_all_roots section 5 does"
        );
        assert_eq!(shared.mem.string_table.census(), (1, 1));
    }

    /// The gate: off by default, Generational-only, and strong while a
    /// concurrent cycle's phase is not `Idle`.
    #[test]
    fn gcg_w2_pool_is_weak_only_when_engaged_and_no_concurrent_cycle_is_open() {
        let shared = gen_vm();
        shared.mem.string_table.set_weak(false);
        assert!(!pool_is_weak_for_this_scan(&shared), "off unless requested");
        shared.mem.string_table.set_weak(true);
        assert!(weak_table_engaged(&shared));
        assert!(pool_is_weak_for_this_scan(&shared));
        for phase in [
            cratonvm_gc::ConcurrentGcPhase::InitialMark,
            cratonvm_gc::ConcurrentGcPhase::ConcurrentMark,
            cratonvm_gc::ConcurrentGcPhase::Remark,
            cratonvm_gc::ConcurrentGcPhase::ConcurrentSweep,
        ] {
            shared.mem.concurrent_gc_state.set_phase(phase);
            assert!(
                !pool_is_weak_for_this_scan(&shared),
                "a concurrent cycle's root sets keep the pool strong ({phase:?})"
            );
        }
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
        assert!(pool_is_weak_for_this_scan(&shared));

        let g1 = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        g1.mem.string_table.set_weak(true);
        assert!(!weak_table_engaged(&g1), "G1 keeps the strong pool");
    }

    fn zgc_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            ..VmConfig::default()
        })
    }

    /// zr z1/rf: the ZGC arm of the gate. Off unless BOTH switches ask for
    /// it (the plain weak switch alone leaves ZGC strong, as before), and
    /// never on G1.
    #[test]
    fn zr1rf_zgc_weak_table_engages_only_under_its_own_switch() {
        let zgc = zgc_vm();
        zgc.mem.string_table.set_weak(true);
        zgc.mem.string_table.set_zgc_weak(false);
        assert!(
            !weak_table_engaged(&zgc),
            "CRATONVM_ZGC_WEAK_STRING_TABLE=0 keeps the strong pool on ZGC"
        );
        zgc.mem.string_table.set_zgc_weak(true);
        assert!(
            zgc.mem.heap.zgc_weak_string_table_sound(),
            "a default ZGC heap only collects stop-the-world"
        );
        assert!(weak_table_engaged(&zgc));
        assert!(pool_is_weak_for_this_scan(&zgc));
        zgc.mem.string_table.set_weak(false);
        assert!(
            !weak_table_engaged(&zgc),
            "CRATONVM_GC_WEAK_STRING_TABLE=0 wins"
        );

        let g1 = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        g1.mem.string_table.set_weak(true);
        g1.mem.string_table.set_zgc_weak(true);
        assert!(!weak_table_engaged(&g1), "G1 is unchanged");
        assert!(!g1.mem.heap.zgc_weak_string_table_sound());
    }

    /// zt t11/x8: `CRATONVM_ZGC_WEAK_STRING_TABLE` is default ON since zt t10
    /// and `=0` kills it, latched per VM by `from_flags`; `with` (tests,
    /// embedders) leaves it off. On a default ZGC VM the weak table then
    /// engages, and the kill arm keeps the strong pool.
    #[test]
    fn zt11x8_zgc_weak_string_table_is_default_on_with_a_zero_kill() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_WEAK_STRING_TABLE", None)],
            || assert!(StringTableState::from_flags().zgc_weak_requested()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_WEAK_STRING_TABLE", Some("0"))],
            || assert!(!StringTableState::from_flags().zgc_weak_requested()),
        );
        assert!(!StringTableState::with(true, true).zgc_weak_requested());
        // zt t13/x10 (`zgc-zt12x9-*` item 8): what the VM itself latched, not
        // the setters: a ZGC VM built with the switch unset engages, one
        // built under `=0` keeps the strong pool. A default ZGC heap only
        // collects stop-the-world; a build where it does not fails here
        // rather than skipping.
        for (value, engaged) in [(None, true), (Some("0"), false)] {
            let zgc = cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_ZGC_WEAK_STRING_TABLE", value),
                    ("CRATONVM_GC_WEAK_STRING_TABLE", None),
                ],
                zgc_vm,
            );
            assert!(
                zgc.mem.heap.zgc_weak_string_table_sound(),
                "a default ZGC heap only collects stop-the-world"
            );
            assert_eq!(
                zgc.mem.string_table.zgc_weak_requested(),
                engaged,
                "{value:?}"
            );
            assert_eq!(weak_table_engaged(&zgc), engaged, "{value:?}");
        }
    }

    /// zt t13/x10 (`zgc-zt11x8-*` item 5,
    /// `CRATONVM_ZGC_WEAK_STRING_TABLE_STRONG_LATCH`): once the ZGC arm
    /// answered "strong" (the heap went generational), turning the
    /// generational mode off again does not bring the weak pool back -- a
    /// literal resolved while strong has no `ldc` record. The weak -> strong
    /// move stays (it is the sound one). Kill control (`=0`): the arm follows
    /// the heap both ways, as before.
    #[test]
    fn zt13x10_the_zgc_arm_latches_its_first_strong_answer() {
        for latch in [true, false] {
            let zgc = zgc_vm();
            let crate::memory::vm_heap::VmHeap::Zgc(h) = &zgc.mem.heap else {
                panic!("zgc_vm builds a ZGC heap");
            };
            zgc.mem.string_table.set_weak(true);
            zgc.mem.string_table.set_zgc_weak(true);
            zgc.mem.string_table.set_zgc_strong_latch(latch);
            // zt t27/gn5: the generational mode is the "strong" heap this
            // test needs only with `CRATONVM_ZGC_GEN_WEAK_STRING_TABLE` off.
            zgc.mem.string_table.set_zgc_gen_weak(false);
            assert!(weak_table_engaged(&zgc), "latch={latch}: weak while sound");
            h.set_generational_enabled(true);
            assert!(!weak_table_engaged(&zgc), "latch={latch}: weak -> strong");
            h.set_generational_enabled(false);
            assert!(zgc.mem.heap.zgc_weak_string_table_sound());
            assert_eq!(
                weak_table_engaged(&zgc),
                !latch,
                "latch={latch}: strong -> weak only without the latch"
            );
        }
        let latched = |value: Option<&str>| {
            let state = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_WEAK_STRING_TABLE_STRONG_LATCH", value)],
                StringTableState::from_flags,
            );
            state.zgc_strong_latch.load(Ordering::Relaxed)
        };
        assert!(latched(None), "default ON");
        assert!(!latched(Some("0")), "=0 kill");
    }

    /// zt t23/st1: a ZGC VM built with concurrent marking configured
    /// (`CRATONVM_ZGC_CONC_START=60`, a construction seed).
    fn zgc_conc_vm() -> SharedVm {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_CONC_START", Some("60"))],
            zgc_vm,
        )
    }

    /// zt t23/st1 (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT`): on a heap
    /// that marks concurrently the ZGC arm engages under the switch and stays
    /// strong without it (the positive control: the defect the hunt5 page
    /// filed, latched as before). Generational mode stays strong either way,
    /// and the stop-the-world-only heap engages either way (no regression).
    #[test]
    fn zt23st1_the_zgc_arm_engages_on_a_concurrently_marking_heap() {
        for conc_weak in [true, false] {
            let zgc = zgc_conc_vm();
            assert!(
                !zgc.mem.heap.zgc_weak_string_table_sound(),
                "premise: CRATONVM_ZGC_CONC_START=60 configures concurrent marking"
            );
            assert!(zgc
                .mem
                .heap
                .zgc_weak_string_table_sound_under_concurrent_mark());
            let t = &zgc.mem.string_table;
            t.set_weak(true);
            t.set_zgc_weak(true);
            t.set_zgc_strong_latch(true);
            t.set_zgc_conc_weak(conc_weak);
            // zt t27/gn5: the generational arm below is the kill arm.
            t.set_zgc_gen_weak(false);
            assert_eq!(weak_table_engaged(&zgc), conc_weak, "conc_weak={conc_weak}");
            assert_eq!(pool_is_weak_for_this_scan(&zgc), conc_weak);
            assert_eq!(zgc_conc_weak_table_engaged(&zgc), conc_weak);
            if conc_weak {
                let crate::memory::vm_heap::VmHeap::Zgc(h) = &zgc.mem.heap else {
                    panic!("zgc_vm builds a ZGC heap");
                };
                h.set_generational_enabled(true);
                assert!(
                    !weak_table_engaged(&zgc),
                    "generational mode stays strong under CRATONVM_ZGC_GEN_WEAK_STRING_TABLE=0"
                );
                h.set_generational_enabled(false);
                assert!(!weak_table_engaged(&zgc), "and the latch holds that answer");
            }
            let stw = zgc_vm();
            stw.mem.string_table.set_weak(true);
            stw.mem.string_table.set_zgc_weak(true);
            stw.mem.string_table.set_zgc_conc_weak(conc_weak);
            assert!(
                weak_table_engaged(&stw),
                "a stop-the-world-only heap: conc_weak={conc_weak}"
            );
        }
    }

    /// zt t23/st1: `collect_roots` section 5 on a concurrently marking ZGC
    /// heap -- the scan the mark start and the closing collection both take,
    /// with and without the class-unload marking licence -- leaves the pooled
    /// String out under the switch and roots it with the switch off.
    #[test]
    fn zt23st1_a_concurrent_zgc_scan_leaves_the_pool_out() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use cratonvm_gc::gc_quiescence::with_class_unload_marking;
        for conc_weak in [true, false] {
            let zgc = zgc_conc_vm();
            let thread = JvmThread::new(ThreadId(0), "test");
            let t = &zgc.mem.string_table;
            t.set_weak(true);
            t.set_zgc_weak(true);
            t.set_zgc_conc_weak(conc_weak);
            let s = crate::vm::create_java_string(&zgc, "zt23st1-root");
            let mark_scan =
                with_class_unload_marking(|| crate::memory::roots::collect_roots(&zgc, &thread));
            let pause_scan = crate::memory::roots::collect_roots(&zgc, &thread);
            assert_eq!(
                mark_scan.contains(&s),
                !conc_weak,
                "mark start, conc_weak={conc_weak}"
            );
            assert_eq!(
                pause_scan.contains(&s),
                !conc_weak,
                "closing, conc_weak={conc_weak}"
            );
        }
    }

    /// zt t23/st1: with no cycle marking, a pool hit logs nothing (the ZGC
    /// keep-alive's engagement counter stays 0 and the hit answers the pooled
    /// String); the switch is default ON with an `=0` kill; and the ZGC arm
    /// of `keep_alive_on_hit` is gated on the cycle flag and logs through the
    /// SATB barrier (source pin: a test heap cannot open a concurrent cycle --
    /// `start_concurrent_mark` refuses a heap with no `Arc` identity -- so
    /// rows `st1_KA*` are its runtime evidence).
    #[test]
    fn zt23st1_keep_alive_on_hit_zgc_arm() {
        let zgc = zgc_conc_vm();
        let t = &zgc.mem.string_table;
        t.set_weak(true);
        t.set_zgc_weak(true);
        t.set_zgc_conc_weak(true);
        let s = crate::vm::create_java_string(&zgc, "zt23st1-hit");
        assert_eq!(
            crate::vm::create_java_string(&zgc, "zt23st1-hit"),
            s,
            "a hit"
        );
        assert!(!zgc.mem.heap.zgc_concurrent_mark_active());
        assert_eq!(t.zgc_keep_alives(), 0, "no cycle: nothing logged");

        let built = |value: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT", value)],
                StringTableState::from_flags,
            )
            .zgc_conc_weak_requested()
        };
        assert!(built(None), "default ON");
        assert!(!built(Some("0")), "=0 kill");
        assert!(!StringTableState::with(true, true).zgc_conc_weak_requested());

        let src = include_str!("string_table.rs").replace("\r\n", "\n");
        let start = src
            .find(concat!("\npub(crate) fn keep_alive", "_on_hit("))
            .expect("keep_alive_on_hit");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("its end")];
        let arm = &body[body.find("zgc_conc_weak_requested()").expect("the ZGC arm")..];
        let gate = arm
            .find("zgc_concurrent_mark_active()")
            .expect("the cycle gate");
        let engaged = arm
            .find("zgc_conc_weak_table_engaged(shared)")
            .expect("the arm gate");
        // zt t23/zb8: the ZGC arm logs through `referent_keep_alive` (SATB
        // unflipped, the load barrier's mark step under the color flip).
        let log = arm.find(".referent_keep_alive(").expect("the keep-alive");
        let count = arm.find("zgc_keep_alives.fetch_add").expect("the counter");
        assert!(gate < engaged && engaged < log && log < count);
    }

    /// zt t27/gn5 (`CRATONVM_ZGC_GEN_WEAK_STRING_TABLE`, the gn page's item
    /// 13): in ZGC's generational mode the ZGC arm engages under the switch
    /// -- on the stop-the-world heap and on a concurrently marking one -- and
    /// the pool leaves the root scan; `=0` keeps the strong pool there (the
    /// kill control: the zt t23 answer). The switch is default ON with an
    /// `=0` kill, latched per VM by `from_flags`; `with` leaves it off. A
    /// generational heap that sees the strong answer first stays strong (the
    /// latch), so each arm builds its own VM.
    #[test]
    fn zt27gn5_the_zgc_arm_engages_in_the_generational_mode_under_its_switch() {
        for gen_weak in [true, false] {
            for conc in [false, true] {
                let zgc = if conc { zgc_conc_vm() } else { zgc_vm() };
                let crate::memory::vm_heap::VmHeap::Zgc(h) = &zgc.mem.heap else {
                    panic!("zgc_vm builds a ZGC heap");
                };
                h.set_generational_enabled(true);
                let t = &zgc.mem.string_table;
                t.set_weak(true);
                t.set_zgc_weak(true);
                t.set_zgc_strong_latch(true);
                t.set_zgc_conc_weak(true);
                t.set_zgc_gen_weak(gen_weak);
                assert_eq!(
                    zgc.mem
                        .heap
                        .zgc_weak_string_table_sound_under_concurrent_mark_for(gen_weak),
                    gen_weak,
                    "conc={conc} gen_weak={gen_weak}: the concurrent gate"
                );
                assert!(
                    !zgc.mem
                        .heap
                        .zgc_weak_string_table_sound_under_concurrent_mark(),
                    "the switch-less gate still refuses the mode"
                );
                if !conc {
                    assert_eq!(
                        zgc.mem.heap.zgc_weak_string_table_sound_for(gen_weak),
                        gen_weak,
                        "gen_weak={gen_weak}: the stop-the-world gate"
                    );
                    assert!(!zgc.mem.heap.zgc_weak_string_table_sound());
                }
                assert_eq!(
                    weak_table_engaged(&zgc),
                    gen_weak,
                    "conc={conc} gen_weak={gen_weak}"
                );
                assert_eq!(pool_is_weak_for_this_scan(&zgc), gen_weak);
                // The stop-the-world-only arm
                // (`CRATONVM_ZGC_WEAK_STRING_TABLE_CONCURRENT=0`) reads the
                // same switch; a concurrently marking heap stays strong there.
                let stw_arm = if conc { zgc_conc_vm() } else { zgc_vm() };
                let crate::memory::vm_heap::VmHeap::Zgc(h2) = &stw_arm.mem.heap else {
                    panic!("zgc_vm builds a ZGC heap");
                };
                h2.set_generational_enabled(true);
                let t2 = &stw_arm.mem.string_table;
                t2.set_weak(true);
                t2.set_zgc_weak(true);
                t2.set_zgc_conc_weak(false);
                t2.set_zgc_gen_weak(gen_weak);
                assert_eq!(
                    weak_table_engaged(&stw_arm),
                    gen_weak && !conc,
                    "conc={conc} gen_weak={gen_weak}: the stop-the-world-only arm"
                );
            }
        }
        let built = |value: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_GEN_WEAK_STRING_TABLE", value)],
                StringTableState::from_flags,
            )
            .zgc_gen_weak_requested()
        };
        assert!(built(None), "default ON");
        assert!(!built(Some("0")), "=0 kill");
        assert!(!StringTableState::with(true, true).zgc_gen_weak_requested());
    }

    /// zt t27/gn5: end to end on a generational ZGC VM. The first collection
    /// of the mode is whole-heap (nothing is old yet); under the switch it
    /// drops the unreferenced interned String and keeps the held one at its
    /// current address, and `=0` keeps both (the kill control: the defect the
    /// gn page's item 13 recorded). The minor's half -- an old String the
    /// minor did not trace stays registered, a dead young one is unregistered
    /// -- is pinned on the heap whose registry the sweep's verdict reads:
    /// `zt27gn5_a_minor_keeps_untraced_old_bases_and_unregisters_dead_young_ones`
    /// (`gc/src/zgc.rs`).
    #[test]
    fn zt27gn5_a_generational_zgc_collection_drops_an_unreferenced_interned_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        for gen_weak in [true, false] {
            let shared = zgc_vm();
            let crate::memory::vm_heap::VmHeap::Zgc(h) = &shared.mem.heap else {
                panic!("zgc_vm builds a ZGC heap");
            };
            h.set_generational_enabled(true);
            shared.mem.string_table.set_weak(true);
            shared.mem.string_table.set_zgc_weak(true);
            shared.mem.string_table.set_zgc_gen_weak(gen_weak);
            let mut thread = JvmThread::new(ThreadId(0), "test");
            intern_and_forget(&shared, "zt27gn5-unreferenced");
            let kept = crate::vm::create_java_string(&shared, "zt27gn5-held");
            let base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(kept);

            crate::runtime::interpreter::maybe_gc_forced_pub_at(
                &shared,
                &mut thread,
                "zt27gn5-test",
            );

            let pool = shared.mem.string_pool.read();
            let held_now = thread.native_pin_roots[base];
            assert_eq!(
                pool.get("zt27gn5-held").map(|o| o.as_ptr()),
                Some(held_now.as_ptr()),
                "a held interned String keeps its entry (gen_weak={gen_weak})"
            );
            if gen_weak {
                assert!(pool.get("zt27gn5-unreferenced").is_none());
                let (sweeps, dropped) = shared.mem.string_table.census();
                assert!(sweeps >= 1 && dropped >= 1, "swept: {sweeps} {dropped}");
            } else {
                assert!(pool.get("zt27gn5-unreferenced").is_some(), "strong pool");
                assert_eq!(shared.mem.string_table.census(), (0, 0));
            }
        }
    }

    /// zr z1/rf: end to end on ZGC -- the unreferenced interned String leaves
    /// the table at the next collection and the held one keeps its entry at
    /// its current address (the slide may have moved it); with the ZGC switch
    /// off the same collection keeps both (the positive control).
    #[test]
    fn zr1rf_zgc_collection_drops_an_unreferenced_interned_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        for zgc_weak in [true, false] {
            let shared = zgc_vm();
            shared.mem.string_table.set_weak(true);
            shared.mem.string_table.set_zgc_weak(zgc_weak);
            let mut thread = JvmThread::new(ThreadId(0), "test");
            intern_and_forget(&shared, "zr1rf-unreferenced");
            let kept = crate::vm::create_java_string(&shared, "zr1rf-held");
            let base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(kept);

            crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "zr1rf-test");

            let pool = shared.mem.string_pool.read();
            let held_now = thread.native_pin_roots[base];
            assert_eq!(
                pool.get("zr1rf-held").map(|o| o.as_ptr()),
                Some(held_now.as_ptr()),
                "a held interned String keeps its entry, at its current address (zgc_weak={zgc_weak})"
            );
            if zgc_weak {
                assert!(pool.get("zr1rf-unreferenced").is_none());
                let (sweeps, dropped) = shared.mem.string_table.census();
                assert!(sweeps >= 1 && dropped >= 1, "swept: {sweeps} {dropped}");
            } else {
                assert!(pool.get("zr1rf-unreferenced").is_some(), "strong pool");
                assert_eq!(shared.mem.string_table.census(), (0, 0));
            }
            drop(pool);
            thread.native_pin_roots.truncate(base);
        }
    }

    /// `collect_roots` section 5 follows the gate: the pooled String is a root
    /// with the switch off (positive control) and not one with it on.
    #[test]
    fn gcg_w2_collect_roots_leaves_the_pool_out_in_weak_mode() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = gen_vm();
        let thread = JvmThread::new(ThreadId(0), "test");
        let s = crate::vm::create_java_string(&shared, "gcg-w2-root");
        shared.mem.string_table.set_weak(false);
        assert!(crate::memory::roots::collect_roots(&shared, &thread).contains(&s));
        shared.mem.string_table.set_weak(true);
        assert!(!crate::memory::roots::collect_roots(&shared, &thread).contains(&s));
    }

    /// End to end on Generational: an interned String nothing references
    /// leaves the table at the next collection, one a root holds stays, and its
    /// entry names the String's post-collection address.
    #[test]
    fn gcg_w2_a_collection_drops_an_unreferenced_interned_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        let mut thread = JvmThread::new(ThreadId(0), "test");
        intern_and_forget(&shared, "gcg-w2-unreferenced");
        let kept = crate::vm::create_java_string(&shared, "gcg-w2-held");
        let base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(kept);

        crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "gcg-w2-test");

        let (sweeps, dropped) = shared.mem.string_table.census();
        assert!(sweeps >= 1, "the pause's epilogue swept the table");
        assert!(dropped >= 1);
        let pool = shared.mem.string_pool.read();
        assert!(pool.get("gcg-w2-unreferenced").is_none());
        let held_now = thread.native_pin_roots[base];
        assert_eq!(
            pool.get("gcg-w2-held").map(|o| o.as_ptr()),
            Some(held_now.as_ptr()),
            "a held interned String keeps its entry, at its current address"
        );
        drop(pool);
        thread.native_pin_roots.truncate(base);
    }

    /// Positive control for the test above: with the switch off the same
    /// collection keeps the unreferenced entry (the pool is a strong root).
    #[test]
    fn gcg_w2_the_strong_pool_keeps_an_unreferenced_interned_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = gen_vm();
        shared.mem.string_table.set_weak(false);
        let mut thread = JvmThread::new(ThreadId(0), "test");
        intern_and_forget(&shared, "gcg-w2-strong");
        crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "gcg-w2-test");
        assert!(shared.mem.string_pool.read().get("gcg-w2-strong").is_some());
        assert_eq!(
            shared.mem.string_table.census(),
            (0, 0),
            "no sweep when off"
        );
    }

    /// Under the weak table a reference `ldc` record goes to the never-evicted
    /// store (it is what keeps a literal alive); a primitive record, and every
    /// record with the switch off (positive control), stays in the FIFO store.
    #[test]
    fn gcg_w2_weak_mode_records_reference_constants_permanently() {
        use crate::classloading::ClassId;
        use crate::runtime::interpreter::constants::store_recorded_cp_constant_as_of as record;
        use crate::types::Value;
        let shared = gen_vm();
        let s = crate::vm::create_java_string(&shared, "gcg-w2-literal");
        let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let permanent = || {
            shared
                .classes
                .resolution_cache
                .read()
                .permanent_constant_count()
        };

        shared.mem.string_table.set_weak(false);
        record(&shared, ClassId::new(77), 3, Value::Object(Some(s)), as_of);
        assert_eq!(permanent(), 0, "switch off: the FIFO store, as before");

        shared.mem.string_table.set_weak(true);
        record(&shared, ClassId::new(77), 4, Value::Object(Some(s)), as_of);
        assert_eq!(
            permanent(),
            1,
            "weak table: a reference record is permanent"
        );
        record(&shared, ClassId::new(77), 5, Value::Int(9), as_of);
        assert_eq!(permanent(), 1, "a primitive record is not");
        assert_eq!(
            shared
                .classes
                .resolution_cache
                .read()
                .get_condy(ClassId::new(77), 4)
                .copied(),
            Some(Value::Object(Some(s))),
            "and it answers the probe exactly as the FIFO record would"
        );
    }

    /// gcg g3/rw (review item 3): with the `ldc` cache off, a reference
    /// constant still needs its record under the weak table, once; never
    /// with the table strong (positive control), never for a primitive.
    #[test]
    fn gcg_g3rw_cache_off_ldc_records_a_literal_only_under_the_weak_table() {
        use crate::classloading::ClassId;
        use crate::runtime::interpreter::constants::{
            store_recorded_cp_constant_as_of as record, weak_table_needs_the_record as needs,
        };
        use crate::types::Value;
        let shared = gen_vm();
        let s = crate::vm::create_java_string(&shared, "gcg-g3rw-literal");
        let lit = Value::Object(Some(s));
        let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let (cls, idx) = (ClassId::new(78), 6u16);

        shared.mem.string_table.set_weak(false);
        assert!(
            !needs(&shared, cls, idx, &lit),
            "strong table: the pool holds it"
        );
        shared.mem.string_table.set_weak(true);
        assert!(
            !needs(&shared, cls, idx, &Value::Int(1)),
            "a primitive is never held"
        );
        assert!(
            !needs(&shared, cls, idx, &Value::Object(None)),
            "nor a null"
        );
        assert!(needs(&shared, cls, idx, &lit), "weak table, no record yet");
        record(&shared, cls, idx, lit, as_of);
        assert!(
            !needs(&shared, cls, idx, &lit),
            "recorded once: a read probe answers"
        );
        assert_eq!(
            shared
                .classes
                .resolution_cache
                .read()
                .permanent_constant_count(),
            1,
            "the record went to the never-evicted store"
        );
    }

    /// gcg g4/rs: the sweep is owed after a weak scan, or whenever the phase
    /// reads `Idle`; skipped only when every scan rooted the pool and a cycle
    /// is open.
    #[test]
    fn gcg_g4rs_sweep_owed_truth_table() {
        assert!(sweep_owed(true, true));
        assert!(sweep_owed(true, false), "a weak scan is always swept");
        assert!(sweep_owed(false, true), "Idle at the sweep: swept (sound)");
        assert!(!sweep_owed(false, false));
    }

    /// gcg g4/rs: the side-effect-free form answers like the scan's gate but
    /// owes no sweep; the scan's gate does (positive control).
    #[test]
    fn gcg_g4rs_pool_left_out_now_owes_no_sweep() {
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        let pending = || {
            shared
                .mem
                .string_table
                .weak_scan_pending
                .load(Ordering::Relaxed)
        };
        assert!(pool_left_out_now(&shared));
        assert!(!pending(), "the diagnostic form leaves no trace");
        assert!(pool_is_weak_for_this_scan(&shared));
        assert!(pending(), "the scan's gate owes the sweep");
    }

    /// gcg g4/rs: a pause of an open concurrent cycle (its scan rooted the
    /// pool) skips the O(pool) sweep and drops nothing; the positive controls
    /// are the same call after a weak scan with the cycle still open, and the
    /// same call with the phase back at `Idle`, which both sweep.
    #[test]
    fn gcg_g4rs_a_strong_scan_pause_skips_the_sweep() {
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        let map = cratonvm_types::PointerMap::default();
        let dead = |_: usize| false;
        let insert_dead = || {
            shared
                .mem
                .string_pool
                .write()
                .insert("gcg-g4rs-dead".to_string(), at(0x4000));
        };
        insert_dead();
        let state = &shared.mem.concurrent_gc_state;

        // The cycle is open: the scan roots the pool and owes no sweep.
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        assert!(!pool_is_weak_for_this_scan(&shared));
        sweep_after_pause(&shared, &map, &dead);
        assert!(shared.mem.string_pool.read().get("gcg-g4rs-dead").is_some());
        assert_eq!(shared.mem.string_table.census(), (0, 0));
        assert_eq!(shared.mem.string_table.skipped_sweeps(), 1);

        // Positive control 1: a weak scan recorded before the cycle opened is
        // still swept, although the phase is not `Idle` at the sweep.
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
        assert!(pool_is_weak_for_this_scan(&shared));
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::InitialMark);
        sweep_after_pause(&shared, &map, &dead);
        assert!(shared.mem.string_pool.read().get("gcg-g4rs-dead").is_none());
        assert_eq!(shared.mem.string_table.census(), (1, 1));

        // Positive control 2: no scan recorded, but `Idle` at the sweep.
        insert_dead();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
        sweep_after_pause(&shared, &map, &dead);
        assert!(shared.mem.string_pool.read().get("gcg-g4rs-dead").is_none());
        assert_eq!(shared.mem.string_table.census(), (2, 2));
        assert_eq!(shared.mem.string_table.skipped_sweeps(), 1);
    }

    /// gch h2/jr (og's page, fix A): with the pool left out of the root set,
    /// every YOUNG pool entry joins the watched set, ADDED to what the
    /// reference processor published (not replacing it), and an old entry
    /// does not; with the switch off, the table strong, or a concurrent cycle
    /// open (the pool rooted), nothing is published (positive controls).
    #[test]
    fn gch_jr_young_pool_entries_join_the_watched_set_when_the_pool_is_weak() {
        use cratonvm_gc::gc_quiescence::{set_watched_referents, watched_referents_snapshot};
        let shared = gen_vm();
        let s = crate::vm::create_java_string(&shared, "gch-jr-young-pooled");
        let s_addr = s.as_ptr() as usize;
        let young_expected: Vec<usize> = shared
            .mem
            .string_pool
            .read()
            .values()
            .map(|o| o.as_ptr() as usize)
            .filter(|&a| shared.mem.heap.is_in_young_addr(a))
            .collect();
        // A sentinel the reference processor "published" first: kept.
        const SENTINEL: usize = 0x5e17_0008;

        shared.mem.string_table.set_weak(true);
        shared.mem.string_table.set_watch_young(false);
        set_watched_referents(&[SENTINEL]);
        assert_eq!(watch_young_pool_entries(&shared), 0, "killed");
        shared.mem.string_table.set_watch_young(true);

        shared.mem.string_table.set_weak(false);
        assert_eq!(watch_young_pool_entries(&shared), 0, "strong table: rooted");
        shared.mem.string_table.set_weak(true);

        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        assert_eq!(watch_young_pool_entries(&shared), 0, "open cycle: rooted");
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);

        let n = watch_young_pool_entries(&shared);
        assert_eq!(n, young_expected.len(), "exactly the young entries");
        let watched = watched_referents_snapshot().unwrap_or_default();
        assert!(watched.contains(&SENTINEL), "added, never a replacement");
        for a in &young_expected {
            assert!(watched.contains(a), "young entry {a:#x} watched");
        }
        if shared.mem.heap.is_in_young_addr(s_addr) {
            assert!(watched.contains(&s_addr), "the fresh pooled String");
            assert!(n >= 1);
        }
        assert_eq!(shared.mem.string_table.watched_young_entries(), n as u64);
        set_watched_referents(&[]);
    }

    /// gcj j3/rf3 (`CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE`, opt-in):
    /// with a concurrent cycle open, a young pause's scan leaves the pool out
    /// only with the switch, never inside the cycle's own initial-mark /
    /// remark scope (`with_no_young_sweep_following`), and the `Idle` rule is
    /// unchanged on both arms. Positive control: the switch flips the open-
    /// cycle answer; kill control: off, the open cycle roots the pool.
    #[test]
    fn gcj3rf3_young_in_cycle_leaves_the_pool_out_outside_the_cycles_own_scans() {
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        let open = cratonvm_gc::ConcurrentGcPhase::ConcurrentMark;
        let idle = cratonvm_gc::ConcurrentGcPhase::Idle;
        for on in [false, true] {
            shared.mem.string_table.set_young_in_cycle(on);
            shared.mem.concurrent_gc_state.set_phase(idle);
            assert!(pool_left_out_now(&shared), "on={on}: idle is weak");
            shared.mem.concurrent_gc_state.set_phase(open);
            assert_eq!(pool_left_out_now(&shared), on, "on={on}: a young pause in the cycle");
            let left_out = || pool_left_out_now(&shared);
            let in_scope = cratonvm_gc::gen_heap::with_no_young_sweep_following(left_out);
            assert!(!in_scope, "on={on}: the cycle's own scans root the pool");
        }
        shared.mem.string_table.set_young_in_cycle(false);
        shared.mem.concurrent_gc_state.set_phase(idle);
    }

    /// gcj j3/rf3: the switch is Generational only (a G1 heap keeps its own
    /// mark-scan rule, whatever the Generational phase reads).
    #[test]
    fn gcj3rf3_young_in_cycle_is_generational_only() {
        let shared = g1_vm();
        shared.mem.string_table.set_weak(true);
        shared.mem.string_table.set_young_in_cycle(true);
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        assert!(!young_pause_in_cycle_leaves_pool_out(&shared));
        assert!(!pool_left_out_now(&shared), "G1 evacuation scans root the pool");
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
    }

    /// gcj j3/rf3 (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`, opt-in): the
    /// young entries go to the seed-only row (the major's view) and not to the
    /// watched set the young sweeps read; off, the watched set as before.
    #[test]
    fn gcj3rf3_seed_only_young_entries_skip_the_sweeps_watched_set() {
        use cratonvm_gc::gc_quiescence as q;
        let shared = gen_vm();
        let s = crate::vm::create_java_string(&shared, "gcj3rf3-seed-only");
        let s_addr = s.as_ptr() as usize;
        if !shared.mem.heap.is_in_young_addr(s_addr) {
            return; // nothing young to watch on this heap shape
        }
        shared.mem.string_table.set_weak(true);
        let key = 0x7900_0000_0000usize;
        for seed_only in [false, true] {
            shared.mem.string_table.set_seed_only_young(seed_only);
            q::set_watched_referents_for(key, &[]);
            assert!(watch_young_pool_entries(&shared) >= 1, "seed_only={seed_only}");
            let sweep = q::watched_referents_snapshot_for(key).unwrap_or_default();
            let major = q::watched_major_snapshot_for(key).unwrap_or_default();
            assert_eq!(sweep.contains(&s_addr), !seed_only, "seed_only={seed_only}");
            assert!(major.contains(&s_addr), "seed_only={seed_only}: the major sees it");
        }
        shared.mem.string_table.set_seed_only_young(false);
        q::set_watched_referents(&[]);
        std::hint::black_box(s);
    }

    /// `intern_receiver`: a miss pools and answers the receiver; a hit answers
    /// the pooled String, not the receiver.
    #[test]
    fn gcg_w2_intern_receiver_pools_the_receiver_on_a_miss() {
        let shared = gen_vm();
        let fresh = crate::vm::create_java_string_uninterned(&shared, "gcg-w2-recv");
        assert_eq!(intern_receiver(&shared, fresh, "gcg-w2-recv"), fresh);
        assert_eq!(
            shared.mem.string_pool.read().get("gcg-w2-recv").copied(),
            Some(fresh)
        );
        let other = crate::vm::create_java_string_uninterned(&shared, "gcg-w2-recv");
        assert_ne!(other, fresh);
        assert_eq!(intern_receiver(&shared, other, "gcg-w2-recv"), fresh);
    }

    // ------------------------------------------------------------------
    // gci i21/sw: the weak String table on G1 (`CRATONVM_G1_WEAK_STRING_TABLE`).
    // ------------------------------------------------------------------

    fn g1_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        })
    }

    /// The G1 arm engages only under BOTH switches and only on G1; the
    /// Generational arm is unchanged by the G1 switch.
    #[test]
    fn gci_i21sw_g1_weak_table_engages_only_under_its_own_switch() {
        let g1 = g1_vm();
        let t = &g1.mem.string_table;
        assert!(!t.g1_weak_requested(), "opt-in: off unless set");
        t.set_weak(true);
        t.set_g1_weak(false);
        assert!(!weak_table_engaged(&g1), "G1 keeps the strong pool by default");
        assert!(!g1_weak_table_engaged(&g1));
        t.set_g1_weak(true);
        assert!(weak_table_engaged(&g1));
        assert!(g1_weak_table_engaged(&g1));
        t.set_weak(false);
        assert!(!weak_table_engaged(&g1), "CRATONVM_GC_WEAK_STRING_TABLE=0 wins");
        assert!(!g1_weak_table_engaged(&g1));

        let generational = gen_vm();
        generational.mem.string_table.set_weak(true);
        generational.mem.string_table.set_g1_weak(true);
        assert!(weak_table_engaged(&generational), "Generational, as before");
        assert!(
            !g1_weak_table_engaged(&generational),
            "the G1 arm is G1's only"
        );
    }

    /// On G1 the pool is left out only by a MARK root scan (the scans taken
    /// under `with_class_unload_marking`), never by an evacuation pause's, and
    /// such a scan owes no Generational sweep. Positive controls: the same
    /// scan with the G1 switch off roots the pool.
    #[test]
    fn gci_i21sw_g1_pool_is_left_out_only_by_a_mark_scan() {
        use cratonvm_gc::gc_quiescence::with_class_unload_marking;
        let g1 = g1_vm();
        let t = &g1.mem.string_table;
        let pending = || t.weak_scan_pending.load(Ordering::Relaxed);
        t.set_weak(true);
        t.set_g1_weak(false);
        assert!(!with_class_unload_marking(|| pool_left_out_now(&g1)), "switch off");
        t.set_g1_weak(true);
        assert!(!pool_left_out_now(&g1), "an evacuation pause roots the pool");
        assert!(!pool_is_weak_for_this_scan(&g1));
        assert!(with_class_unload_marking(|| pool_left_out_now(&g1)));
        assert!(with_class_unload_marking(|| pool_is_weak_for_this_scan(&g1)));
        assert!(!pending(), "no update_all_roots sweep is ever owed on G1");
        // The Generational concurrent phase is not G1's business.
        g1.mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        assert!(with_class_unload_marking(|| pool_left_out_now(&g1)));
        g1.mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
    }

    /// `collect_roots` section 5 on G1: a mark scan leaves the pooled String
    /// out under the switch; an evacuation scan, and a mark scan with the
    /// switch off, root it (positive controls).
    #[test]
    fn gci_i21sw_g1_collect_roots_leaves_the_pool_out_of_mark_scans_only() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use cratonvm_gc::gc_quiescence::with_class_unload_marking;
        let g1 = g1_vm();
        let thread = JvmThread::new(ThreadId(0), "test");
        let s = crate::vm::create_java_string(&g1, "gci-i21sw-root");
        let roots = |mark: bool| {
            if mark {
                with_class_unload_marking(|| crate::memory::roots::collect_roots(&g1, &thread))
            } else {
                crate::memory::roots::collect_roots(&g1, &thread)
            }
        };
        g1.mem.string_table.set_weak(true);
        g1.mem.string_table.set_g1_weak(false);
        assert!(roots(true).contains(&s), "switch off: rooted at the mark");
        g1.mem.string_table.set_g1_weak(true);
        assert!(roots(false).contains(&s), "an evacuation scan roots it");
        assert!(!roots(true).contains(&s), "a mark scan leaves it out");
    }

    /// The pure remark verdict: dead = neither kept nor live; `is_live` is
    /// never asked about a kept address.
    #[test]
    fn gci_i21sw_judge_g1_remark_entries_drops_only_unkept_unmarked() {
        let asked = std::cell::RefCell::new(Vec::new());
        let is_live = |a: usize| {
            asked.borrow_mut().push(a);
            a == 0x3000
        };
        let dead = judge_g1_remark_entries(&[0x3000, 0x4000, 0x5000], &[0x5000], &is_live);
        assert_eq!(dead, vec![0x4000]);
        assert_eq!(*asked.borrow(), vec![0x3000, 0x4000], "a kept entry is not asked");
        assert!(judge_g1_remark_entries(&[], &[], &|_| false).is_empty());
    }

    /// Judge, then settle: an untrusted (or incomplete) remark keeps every
    /// judged entry and empties the judged set; the drop itself removes
    /// exactly the judged entries. Off the switch the judge records nothing.
    #[test]
    fn gci_i21sw_g1_remark_judge_and_settle() {
        let g1 = g1_vm();
        let t = &g1.mem.string_table;
        t.set_weak(true);
        t.set_g1_weak(true);
        {
            let mut pool = g1.mem.string_pool.write();
            pool.insert("gci-i21sw-dead".to_string(), at(0x4000));
            pool.insert("gci-i21sw-live".to_string(), at(0x3000));
            pool.insert("gci-i21sw-kept".to_string(), at(0x5000));
        }
        let judged = || t.g1_remark_dead.lock().clone();
        assert_eq!(
            g1_remark_judge_interned_strings(&g1, &|a| a == 0x3000, &[0x5000]),
            1
        );
        assert_eq!(judged(), vec![0x4000]);

        // Incomplete remark: nothing dropped, the set is consumed.
        assert_eq!(g1_remark_settle_interned_strings(&g1, false), 0);
        assert!(judged().is_empty());
        assert!(g1.mem.string_pool.read().get("gci-i21sw-dead").is_some());

        // A fresh G1 heap has run no cleanup: `last_mark_trusted` is false,
        // so a completed but untrusted remark keeps the entries too.
        assert!(!g1_last_mark_trusted(&g1));
        g1_remark_judge_interned_strings(&g1, &|a| a == 0x3000, &[0x5000]);
        assert_eq!(g1_remark_settle_interned_strings(&g1, true), 0);
        assert!(judged().is_empty());
        assert!(g1.mem.string_pool.read().get("gci-i21sw-dead").is_some());
        assert_eq!(t.g1_census(), (0, 0, 1, 0), "one untrusted settle counted");

        // The trusted arm's drop: exactly the judged entries leave.
        assert_eq!(drop_judged_entries(&g1, &[0x4000]), 1);
        let pool = g1.mem.string_pool.read();
        assert!(pool.get("gci-i21sw-dead").is_none());
        assert_eq!(pool.get("gci-i21sw-live").map(|o| o.as_ptr() as usize), Some(0x3000));
        assert_eq!(pool.get("gci-i21sw-kept").map(|o| o.as_ptr() as usize), Some(0x5000));
        drop(pool);

        // Switch off (positive control): the judge records nothing.
        t.set_g1_weak(false);
        assert_eq!(g1_remark_judge_interned_strings(&g1, &|_| false, &[]), 0);
        assert!(judged().is_empty());
    }

    /// No `update_all_roots` sweep on G1, even with both switches set: the
    /// evacuation pauses rooted the pool, and the remark owns the clear.
    #[test]
    fn gci_i21sw_g1_never_sweeps_after_a_pause() {
        let g1 = g1_vm();
        g1.mem.string_table.set_weak(true);
        g1.mem.string_table.set_g1_weak(true);
        g1.mem
            .string_pool
            .write()
            .insert("gci-i21sw-nosweep".to_string(), at(0x4000));
        sweep_after_pause(&g1, &cratonvm_types::PointerMap::default(), &|_| false);
        assert!(g1.mem.string_pool.read().get("gci-i21sw-nosweep").is_some());
        assert_eq!(g1.mem.string_table.census(), (0, 0));
        assert_eq!(g1.mem.string_table.skipped_sweeps(), 0);
    }

    /// The keep-alive: with G1's SATB queue active, a pool hit (here through
    /// `create_java_string`, the `vm_object` hit arm, and `intern_receiver`'s
    /// hit) is logged for the marker; with the switch off, or the queue
    /// inactive, nothing is (positive controls).
    #[test]
    fn gci_i21sw_a_pool_hit_while_marking_is_logged_for_the_marker() {
        use crate::memory::vm_heap::VmHeap;
        let g1 = g1_vm();
        let s = crate::vm::create_java_string(&g1, "gci-i21sw-hit");
        let addr = s.as_ptr() as usize;
        let VmHeap::G1(h) = &g1.mem.heap else {
            panic!("test requires the G1 backend");
        };
        let q = h.satb_queue();
        let logged = || {
            cratonvm_gc::satb::flush_thread_satb_buffer(q);
            q.drain()
        };
        let _ = logged();
        g1.mem.string_table.set_weak(true);
        g1.mem.string_table.set_g1_weak(true);

        // Queue inactive: no log.
        assert_eq!(keep_alive_on_hit(&g1, s), s);
        assert!(!logged().contains(&addr));
        assert_eq!(g1.mem.string_table.g1_census().3, 0);

        q.activate();
        // Switch off: no log.
        g1.mem.string_table.set_g1_weak(false);
        assert_eq!(crate::vm::create_java_string(&g1, "gci-i21sw-hit"), s);
        assert!(!logged().contains(&addr), "switch off");
        g1.mem.string_table.set_g1_weak(true);

        assert_eq!(keep_alive_on_hit(&g1, s), s);
        assert!(logged().contains(&addr), "the helper logs");
        assert_eq!(crate::vm::create_java_string(&g1, "gci-i21sw-hit"), s);
        assert!(logged().contains(&addr), "the vm_object hit arm logs");
        let other = crate::vm::create_java_string_uninterned(&g1, "gci-i21sw-hit");
        assert_eq!(intern_receiver(&g1, other, "gci-i21sw-hit"), s);
        assert!(logged().contains(&addr), "intern_receiver's hit logs");
        assert_eq!(g1.mem.string_table.g1_census().3, 3);
        q.deactivate();
    }

    /// Source pin: both G1 remark callbacks judge the pool right after the
    /// JNI weak-global clear, and both remark doors settle it after the heap
    /// call returned and before the address-keyed sweep.
    #[test]
    fn gci_i21sw_both_g1_remarks_judge_and_settle_the_pool() {
        let src = include_str!("../runtime/interpreter/gc_and_alloc.rs").replace("\r\n", "\n");
        let body = |sig: &str| -> String {
            let start = src.find(sig).unwrap_or_else(|| panic!("{sig}"));
            let rest = &src[start..];
            let end = rest.find("\n}\n").unwrap_or_else(|| panic!("{sig}: end"));
            rest[..end].to_string()
        };
        let at = |b: &str, needle: &str| b.find(needle).unwrap_or_else(|| panic!("{needle}"));
        const JNI: &str = "clear_weak_global_refs_unmarked(shared, is_live, &keep)";
        const JUDGE: &str = "g1_remark_judge_interned_strings(";
        const HEAP: &str = ".g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));";
        const SETTLE: &str = "g1_remark_settle_interned_strings(shared, completed)";
        const SWEEP: &str = "sweep_address_keyed_tables(shared)";
        let remark = body("\npub(super) fn g1_final_remark_cleanup(");
        assert!(at(&remark, JNI) < at(&remark, JUDGE));
        assert!(at(&remark, JUDGE) < at(&remark, HEAP));
        assert!(at(&remark, HEAP) < at(&remark, SETTLE));
        assert!(at(&remark, SETTLE) < at(&remark, SWEEP));
        assert_eq!(remark.matches(JUDGE).count(), 1);
        assert_eq!(remark.matches(SETTLE).count(), 1);
        let pass = body("\nfn g1_remark_reference_pass(");
        assert!(at(&pass, JNI) < at(&pass, JUDGE));
        let stw = body("\npub(super) fn g1_stw_full_mark_cycle(");
        assert!(stw.contains("g1_remark_reference_pass(shared, is_live)"));
        assert!(at(&stw, HEAP) < at(&stw, SETTLE));
        assert!(at(&stw, SETTLE) < at(&stw, SWEEP));
        assert_eq!(stw.matches(SETTLE).count(), 1);
    }

    // ---- gci i25/st2: the insert funnel and the young-entry list ----

    /// A Generational VM with the weak table and the funnel on (the pool is
    /// still empty, as `set_funnel` requires).
    fn funnel_gen_vm() -> SharedVm {
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        shared.mem.string_table.set_funnel(true);
        shared
    }

    fn young_keys_of(shared: &SharedVm) -> Vec<String> {
        let mut keys: Vec<String> = shared
            .mem
            .string_table
            .young_list
            .lock()
            .iter()
            .map(|e| e.key.to_string())
            .collect();
        keys.sort();
        keys
    }

    /// gcj j1/st: the young list's rows as `(key, addr, moved_from)`, sorted.
    fn young_rows_of(shared: &SharedVm) -> Vec<(String, usize, usize)> {
        let mut rows: Vec<(String, usize, usize)> = shared
            .mem
            .string_table
            .young_list
            .lock()
            .iter()
            .map(|e| (e.key.to_string(), e.addr, e.moved_from))
            .collect();
        rows.sort();
        rows
    }

    /// gcj j1/st: list `key` at `addr`, as the funnel would have (tests that
    /// build the pool by hand).
    fn list_row(shared: &SharedVm, key: &str, addr: usize) {
        let mut list = shared.mem.string_table.young_list.lock();
        list.push(YoungEntry {
            key: Box::from(key),
            addr,
            moved_from: 0,
        });
    }

    fn pool_keys_of(shared: &SharedVm) -> Vec<String> {
        let mut keys: Vec<String> = shared.mem.string_pool.read().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The gates: the funnel is off unless asked for; the young list engages
    /// only with the funnel AND the weak table on a Generational heap; on G1
    /// and ZGC the funnel (allocation outside the lock) engages alone.
    #[test]
    fn gci_i25st2_funnel_gates() {
        let state = StringTableState::with(true, true);
        assert!(!state.funnel_requested(), "opt-in");

        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        shared.mem.string_table.set_funnel(false);
        assert!(!funnel_engaged(&shared) && !young_list_engaged(&shared));
        shared.mem.string_table.set_funnel(true);
        assert!(funnel_engaged(&shared) && young_list_engaged(&shared));
        shared.mem.string_table.set_weak(false);
        assert!(
            funnel_engaged(&shared) && !young_list_engaged(&shared),
            "the strong pool keeps no young list (nothing would read it)"
        );

        for other in [g1_vm(), zgc_vm()] {
            other.mem.string_table.set_weak(true);
            other.mem.string_table.set_funnel(true);
            assert!(funnel_engaged(&other));
            assert!(!young_list_engaged(&other), "Generational only");
        }
    }

    /// The three pooled constructors under the funnel: a miss allocates and
    /// pools (reporting the allocation), a hit answers the same object and
    /// allocates nothing, and every young pooled String's key is listed once.
    #[test]
    fn gci_i25st2_pooled_constructors_go_through_the_funnel() {
        let shared = funnel_gen_vm();
        let (a, allocated) = crate::vm::create_java_string_reporting(&shared, "gci-i25st2-a");
        assert!(allocated);
        assert_eq!(
            crate::vm::create_java_string_reporting(&shared, "gci-i25st2-a"),
            (a, false)
        );
        let b = crate::vm::try_create_java_string(&shared, "gci-i25st2-b").expect("room");
        assert_eq!(crate::vm::try_create_java_string(&shared, "gci-i25st2-b"), Some(b));
        let (c, allocated) =
            crate::vm::try_create_java_string_reporting(&shared, "gci-i25st2-c").expect("room");
        assert!(allocated);
        assert_eq!(
            crate::vm::try_create_java_string_reporting(&shared, "gci-i25st2-c"),
            Some((c, false))
        );
        // Unicode text takes the UTF-16 constructor arm.
        let d = crate::vm::create_java_string(&shared, "gci-i25st2-\u{e9}");
        assert_eq!(crate::vm::create_java_string(&shared, "gci-i25st2-\u{e9}"), d);
        assert_eq!(
            pool_keys_of(&shared),
            vec!["gci-i25st2-a", "gci-i25st2-b", "gci-i25st2-c", "gci-i25st2-\u{e9}"]
        );
        let pooled = [
            ("gci-i25st2-a", a),
            ("gci-i25st2-b", b),
            ("gci-i25st2-c", c),
            ("gci-i25st2-\u{e9}", d),
        ];
        let young: Vec<String> = pooled
            .iter()
            .filter(|(_, s)| shared.mem.heap.is_in_young_addr(s.as_ptr() as usize))
            .map(|(k, _)| k.to_string())
            .collect();
        let listed = young_keys_of(&shared);
        for k in &young {
            assert!(listed.contains(k), "young pooled String {k} is listed");
        }
        assert_eq!(
            shared.mem.string_table.funnel_census().0,
            listed.len() as u64,
            "one listing per insert, none per hit"
        );
        assert_eq!(shared.mem.string_table.funnel_census().1, 0, "no race");
    }

    /// The funnel's insert: a miss pools `fresh`; an existing entry wins (a
    /// lost race), is answered without listing anything, and is counted.
    #[test]
    fn gci_i25st2_pool_insert_if_absent_answers_the_winner() {
        let shared = funnel_gen_vm();
        let first = crate::vm::create_java_string_uninterned(&shared, "gci-i25st2-race");
        let second = crate::vm::create_java_string_uninterned(&shared, "gci-i25st2-race");
        assert_ne!(first, second);
        assert_eq!(
            pool_insert_if_absent(&shared, "gci-i25st2-race", first),
            (first, true)
        );
        let listed = shared.mem.string_table.funnel_census().0;
        assert_eq!(
            pool_insert_if_absent(&shared, "gci-i25st2-race", second),
            (first, false),
            "the loser answers the winner"
        );
        let (listed_after, lost, ..) = shared.mem.string_table.funnel_census();
        assert_eq!(listed_after, listed, "a lost race lists nothing");
        assert_eq!(lost, 1);
        assert_eq!(
            shared.mem.string_pool.read().get("gci-i25st2-race").copied(),
            Some(first)
        );
    }

    /// A failed allocation answers a hit when the text was pooled meanwhile,
    /// and `None` otherwise (the caller's OOM ladder runs).
    #[test]
    fn gci_i25st2_failed_allocation_rereads_the_pool() {
        let shared = funnel_gen_vm();
        assert_eq!(
            intern_miss_via_funnel(&shared, "gci-i25st2-oom", || None),
            None
        );
        let s = crate::vm::create_java_string(&shared, "gci-i25st2-oom");
        assert_eq!(
            intern_miss_via_funnel(&shared, "gci-i25st2-oom", || None),
            Some((s, false))
        );
    }

    /// `intern_receiver` under the funnel: a miss pools (and lists, when
    /// young) the receiver itself; a hit answers the pooled String.
    #[test]
    fn gci_i25st2_intern_receiver_goes_through_the_funnel() {
        let shared = funnel_gen_vm();
        let fresh = crate::vm::create_java_string_uninterned(&shared, "gci-i25st2-recv");
        assert_eq!(intern_receiver(&shared, fresh, "gci-i25st2-recv"), fresh);
        if shared.mem.heap.is_in_young_addr(fresh.as_ptr() as usize) {
            assert_eq!(young_keys_of(&shared), vec!["gci-i25st2-recv"]);
        }
        let other = crate::vm::create_java_string_uninterned(&shared, "gci-i25st2-recv");
        assert_eq!(intern_receiver(&shared, other, "gci-i25st2-recv"), fresh);
    }

    /// Four threads intern the same texts at once under the funnel: every
    /// thread answers the same String for each text (one winner per text),
    /// and the pool holds one entry per text.
    #[test]
    fn gci_i25st2_racing_interns_agree_on_one_string_per_text() {
        let shared = funnel_gen_vm();
        let shared_ref = &shared;
        let texts: Vec<String> = (0..64).map(|i| format!("gci-i25st2-mt-{i}")).collect();
        let texts_ref = &texts;
        let answers: Vec<Vec<usize>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(move || {
                        texts_ref
                            .iter()
                            .map(|t| crate::vm::create_java_string(shared_ref, t).as_ptr() as usize)
                            .collect::<Vec<usize>>()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("no panic")).collect()
        });
        for other in &answers[1..] {
            assert_eq!(other, &answers[0], "one String per text");
        }
        assert_eq!(shared.mem.string_pool.read().len(), texts.len());
        let (listed, lost, ..) = shared.mem.string_table.funnel_census();
        assert!(listed <= texts.len() as u64, "one listing per winning insert");
        let _ = lost; // races are scheduling-dependent: counted, not asserted
    }

    /// The young-list sweep on a pause that reclaimed no old storage judges
    /// only the listed entries and reaches exactly the whole-pool walk's
    /// verdict (the twin VM, funnel off, is the positive control); the old
    /// entry is never asked about; the dead entry leaves by key; the moved
    /// survivor's key is recorded for the remap; the list is pruned.
    #[test]
    fn gci_i25st2_young_list_sweep_matches_the_whole_pool_walk() {
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
        let shared = funnel_gen_vm();
        let twin = gen_vm();
        twin.mem.string_table.set_weak(true);
        let (old_base, old_len) = shared.mem.heap.old_gen_info();
        assert!(old_len > 0, "a Generational heap has an old arena");
        let old_addr = old_base + 64;
        for vm in [&shared, &twin] {
            // Each VM's own old arena (the twin's differs).
            let own_old = vm.mem.heap.old_gen_info().0 + 64;
            let mut pool = vm.mem.string_pool.write();
            pool.insert("gci-i25st2-dead".to_string(), at(0x4000));
            pool.insert("gci-i25st2-live".to_string(), at(0x3000));
            pool.insert("gci-i25st2-moved".to_string(), at(0x1000));
            pool.insert("gci-i25st2-old".to_string(), at(own_old));
        }
        list_row(&shared, "gci-i25st2-dead", 0x4000);
        list_row(&shared, "gci-i25st2-live", 0x3000);
        list_row(&shared, "gci-i25st2-moved", 0x1000);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        let asked = std::cell::RefCell::new(Vec::new());
        let survived = |a: usize| {
            asked.borrow_mut().push(a);
            a == 0x3000
        };
        let s = sweep_dead_interned_strings(&shared, &map, &survived);
        assert_eq!(
            s,
            StringTableSweep {
                entries: 3,
                moved: 1,
                old_untouched: 0,
                in_place: 1,
                dropped: 1,
                left_out: 0,
            }
        );
        assert!(!asked.borrow().contains(&old_addr), "an unlisted old entry is never judged");
        let t = sweep_dead_interned_strings(&twin, &map, &|a| a == 0x3000);
        assert_eq!(t.dropped, 1);
        assert_eq!(t.old_untouched, 1, "the walk keeps the old entry by range");
        assert_eq!(pool_keys_of(&shared), pool_keys_of(&twin), "the same verdict");
        assert_eq!(
            pool_keys_of(&shared),
            vec!["gci-i25st2-live", "gci-i25st2-moved", "gci-i25st2-old"]
        );
        // gcj j1/st: the rows were advanced in the sweep -- the moved one to
        // its POST-collection address, its old one kept for the remap -- with
        // no text lookup; the dead row left.
        assert_eq!(
            young_rows_of(&shared),
            vec![
                ("gci-i25st2-live".to_string(), 0x3000, 0),
                ("gci-i25st2-moved".to_string(), 0x9000, 0x1000),
            ],
            "the dead row leaves, the young survivors stay, the moved one is advanced"
        );
        let census = shared.mem.string_table.funnel_census();
        assert_eq!(census.2, 1, "one young-list sweep");
        assert_eq!(shared.mem.string_table.census(), (1, 1));
        let owed = shared
            .mem
            .string_table
            .young_remap
            .lock()
            .as_ref()
            .map(|(at, promoted)| (*at, promoted.len()));
        assert_eq!(
            owed,
            Some((shared.mem.heap.collection_count(), 0)),
            "recorded for this pause; nothing promoted"
        );

        // The remap touches only the moved row's entry, and moves exactly it.
        assert_eq!(remap_pool_after_pause(&shared, &map), 1);
        assert_eq!(shared.mem.string_table.funnel_census().3, 1);
        let pool = shared.mem.string_pool.read();
        assert_eq!(pool.get("gci-i25st2-moved").map(|o| o.as_ptr() as usize), Some(0x9000));
        assert_eq!(pool.get("gci-i25st2-live").map(|o| o.as_ptr() as usize), Some(0x3000));
        assert_eq!(pool.get("gci-i25st2-old").map(|o| o.as_ptr() as usize), Some(old_addr));
        drop(pool);
        assert_eq!(
            young_rows_of(&shared),
            vec![
                ("gci-i25st2-live".to_string(), 0x3000, 0),
                ("gci-i25st2-moved".to_string(), 0x9000, 0),
            ],
            "the remap clears the moved mark: rows and pool agree again"
        );
        // Taken: the next remap is the whole walk (and finds nothing to move,
        // and leaves the rows where they are: 0x9000 is no map key).
        assert_eq!(remap_pool_after_pause(&shared, &map), 0);
        assert_eq!(shared.mem.string_table.funnel_census().3, 1);
        assert_eq!(
            young_rows_of(&shared)[1],
            ("gci-i25st2-moved".to_string(), 0x9000, 0)
        );
    }

    /// A young-list key whose String the pause promoted into the old arena
    /// leaves the list (its entry stays, and is recorded for the remap).
    #[test]
    fn gci_i25st2_a_promoted_entry_leaves_the_young_list() {
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
        let shared = funnel_gen_vm();
        let (old_base, old_len) = shared.mem.heap.old_gen_info();
        assert!(old_len > 256);
        shared
            .mem
            .string_pool
            .write()
            .insert("gci-i25st2-promoted".to_string(), at(0x1000));
        list_row(&shared, "gci-i25st2-promoted", 0x1000);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, old_base + 128);
        let s = sweep_dead_interned_strings(&shared, &map, &|_| false);
        assert_eq!((s.moved, s.dropped), (1, 0));
        assert!(young_keys_of(&shared).is_empty(), "promoted: no longer listed");
        assert_eq!(
            shared
                .mem
                .string_table
                .young_remap
                .lock()
                .as_ref()
                .map(|(_, promoted)| promoted.len()),
            Some(1),
            "handed to the remap record: its entry still owes the move"
        );
        assert_eq!(remap_pool_after_pause(&shared, &map), 1);
        assert_eq!(
            shared
                .mem
                .string_pool
                .read()
                .get("gci-i25st2-promoted")
                .map(|o| o.as_ptr() as usize),
            Some(old_base + 128)
        );
    }

    /// A pause that reclaimed old storage walks the whole pool (the old entry
    /// IS asked, and dies when dead), records no remap set, and prunes only
    /// the keys whose entry left the pool.
    #[test]
    fn gci_i25st2_an_old_reclaiming_pause_walks_the_whole_pool() {
        let shared = funnel_gen_vm();
        let (old_base, _) = shared.mem.heap.old_gen_info();
        let old_addr = old_base + 64;
        {
            let mut pool = shared.mem.string_pool.write();
            pool.insert("gci-i25st2-young-dead".to_string(), at(0x4000));
            pool.insert("gci-i25st2-young-live".to_string(), at(0x3000));
            pool.insert("gci-i25st2-old-dead".to_string(), at(old_addr));
        }
        list_row(&shared, "gci-i25st2-young-dead", 0x4000);
        list_row(&shared, "gci-i25st2-young-live", 0x3000);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x3000, 0x7000);
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        let s = sweep_dead_interned_strings(&shared, &map, &|a| a == 0x3000);
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
        assert_eq!((s.entries, s.dropped), (3, 2), "the whole pool, old entry included");
        assert_eq!(pool_keys_of(&shared), vec!["gci-i25st2-young-live"]);
        assert_eq!(young_keys_of(&shared), vec!["gci-i25st2-young-live"]);
        assert!(shared.mem.string_table.young_remap.lock().is_none());
        assert_eq!(shared.mem.string_table.funnel_census().2, 0, "not a young-list sweep");
        // gcj j1/st: the surviving row still holds its PRE-collection address;
        // the whole-pool remap advances it with its entry (the list stays exact).
        assert_eq!(
            young_rows_of(&shared),
            vec![("gci-i25st2-young-live".to_string(), 0x3000, 0)]
        );
        assert_eq!(remap_pool_after_pause(&shared, &map), 1);
        assert_eq!(
            young_rows_of(&shared),
            vec![("gci-i25st2-young-live".to_string(), 0x7000, 0)]
        );
        assert_eq!(
            shared
                .mem
                .string_pool
                .read()
                .get("gci-i25st2-young-live")
                .map(|o| o.as_ptr() as usize),
            Some(0x7000)
        );
    }

    /// The remap set lives for one pause: `sweep_after_pause` clears it first
    /// (here the sweep is then skipped, an open concurrent cycle), and a set
    /// recorded at another collection count is never trusted (the whole walk
    /// runs and remaps the unlisted entry too).
    #[test]
    fn gci_i25st2_a_stale_remap_set_is_never_used() {
        let shared = funnel_gen_vm();
        shared
            .mem
            .string_pool
            .write()
            .insert("gci-i25st2-unlisted".to_string(), at(0x1000));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        let now = shared.mem.heap.collection_count();

        *shared.mem.string_table.young_remap.lock() = Some((now, Vec::new()));
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        sweep_after_pause(&shared, &map, &|_| true);
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
        assert!(
            shared.mem.string_table.young_remap.lock().is_none(),
            "cleared by every sweep_after_pause"
        );

        *shared.mem.string_table.young_remap.lock() = Some((now + 1, Vec::new()));
        assert_eq!(remap_pool_after_pause(&shared, &map), 1, "the whole walk");
        assert_eq!(shared.mem.string_table.funnel_census().3, 0);
        assert_eq!(
            shared
                .mem
                .string_pool
                .read()
                .get("gci-i25st2-unlisted")
                .map(|o| o.as_ptr() as usize),
            Some(0x9000)
        );
    }

    /// Off the funnel, `remap_pool_after_pause` is section 5 as it was: every
    /// entry probed, a recorded set ignored.
    #[test]
    fn gci_i25st2_remap_off_the_funnel_walks_every_entry() {
        let shared = gen_vm();
        shared
            .mem
            .string_pool
            .write()
            .insert("gci-i25st2-off".to_string(), at(0x1000));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        let now = shared.mem.heap.collection_count();
        *shared.mem.string_table.young_remap.lock() = Some((now, Vec::new()));
        assert_eq!(remap_pool_after_pause(&shared, &map), 1);
    }

    /// The pre-GC watch reads the young list under the funnel: a listed young
    /// entry is published, an UNLISTED young entry (inserted behind the
    /// funnel's back, which no production path does) is not -- the proof the
    /// list, not the pool, is read; the positive control is the same pool with
    /// the funnel off, which publishes both.
    #[test]
    fn gci_i25st2_the_watch_reads_the_young_list() {
        use cratonvm_gc::gc_quiescence::{set_watched_referents, watched_referents_snapshot};
        let shared = funnel_gen_vm();
        let listed = crate::vm::create_java_string(&shared, "gci-i25st2-watch-listed");
        let behind = crate::vm::create_java_string_uninterned(&shared, "gci-i25st2-watch-behind");
        shared
            .mem
            .string_pool
            .write()
            .insert("gci-i25st2-watch-behind".to_string(), behind);
        let heap = &shared.mem.heap;
        if !heap.is_in_young_addr(listed.as_ptr() as usize)
            || !heap.is_in_young_addr(behind.as_ptr() as usize)
        {
            return; // nothing young to watch on this heap shape
        }
        set_watched_referents(&[]);
        assert_eq!(watch_young_pool_entries(&shared), 1);
        let watched = watched_referents_snapshot().unwrap_or_default();
        assert!(watched.contains(&(listed.as_ptr() as usize)));
        assert!(!watched.contains(&(behind.as_ptr() as usize)));
        assert_eq!(
            young_keys_of(&shared),
            vec!["gci-i25st2-watch-listed"],
            "the list is restored after the read"
        );

        set_watched_referents(&[]);
        shared.mem.string_table.set_funnel(false);
        assert_eq!(watch_young_pool_entries(&shared), 2, "positive control: the walk");
        set_watched_referents(&[]);
    }

    /// End to end on Generational with the funnel: the unreferenced interned
    /// String leaves the table at the next collection, the held one keeps its
    /// entry at its current address, and the young list no longer names the
    /// dropped text.
    #[test]
    fn gci_i25st2_a_collection_drops_an_unreferenced_interned_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = funnel_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        intern_and_forget(&shared, "gci-i25st2-unreferenced");
        let kept = crate::vm::create_java_string(&shared, "gci-i25st2-held");
        let base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(kept);

        crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "gci-i25st2-test");

        let (sweeps, dropped) = shared.mem.string_table.census();
        assert!(sweeps >= 1 && dropped >= 1, "swept: {sweeps} {dropped}");
        let pool = shared.mem.string_pool.read();
        assert!(pool.get("gci-i25st2-unreferenced").is_none());
        let held_now = thread.native_pin_roots[base];
        assert_eq!(
            pool.get("gci-i25st2-held").map(|o| o.as_ptr()),
            Some(held_now.as_ptr()),
            "a held interned String keeps its entry, at its current address"
        );
        drop(pool);
        assert!(
            !young_keys_of(&shared).contains(&"gci-i25st2-unreferenced".to_string()),
            "a dropped entry's key is pruned"
        );
        thread.native_pin_roots.truncate(base);
    }

    // ---- gcj j1/st: the exact young list, the insert grace ----

    /// The rows and the pool agree (the list's invariant), checked by hand.
    fn assert_young_list_exact(shared: &SharedVm) {
        let rows = young_rows_of(shared);
        let pool = shared.mem.string_pool.read();
        for (key, addr, moved_from) in rows {
            assert_eq!(moved_from, 0, "{key}: no moved mark outside a pause");
            assert_eq!(
                pool.get(key.as_str()).map(|o| o.as_ptr() as usize),
                Some(addr),
                "{key}: the row names its entry's current String"
            );
        }
    }

    /// The row update is pure: a dead row leaves, a moved row is advanced and
    /// keeps its old address, a moved row now in the old arena is handed to
    /// the remap, and an unmoved row already in the old arena leaves with
    /// nothing owed.
    #[test]
    fn gcj1st_advance_young_rows_drops_advances_and_promotes() {
        let old = (0x10_0000, 0x1_0000);
        let row = |key: &str, addr: usize| YoungEntry {
            key: Box::from(key),
            addr,
            moved_from: 0,
        };
        let list = vec![
            row("dead", 0x4000),
            row("stayed", 0x3000),
            row("moved", 0x1000),
            row("promoted", 0x2000),
            row("old-unmoved", 0x10_0040),
        ];
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        map.insert(0x2000, 0x10_0100);
        map.insert(0x3000, 0x3000); // identity: not a move
        // addr-keyed: test-local addresses.
        let dead: rustc_hash::FxHashSet<usize> = [0x4000usize].into_iter().collect();
        let (kept, promoted) = advance_young_rows(list, &map, &dead, old);
        let shape = |v: &[YoungEntry]| -> Vec<(String, usize, usize)> {
            v.iter()
                .map(|e| (e.key.to_string(), e.addr, e.moved_from))
                .collect()
        };
        assert_eq!(
            shape(&kept),
            vec![
                ("stayed".to_string(), 0x3000, 0),
                ("moved".to_string(), 0x9000, 0x1000),
            ]
        );
        assert_eq!(
            shape(&promoted),
            vec![("promoted".to_string(), 0x10_0100, 0x2000)]
        );
    }

    /// A pause whose sweep is skipped (an open concurrent cycle) still moves
    /// the pool through the whole-pool remap; the rows are advanced with it,
    /// so the list stays exact. The positive control is the pool entry
    /// itself, which the same remap moves.
    #[test]
    fn gcj1st_a_skipped_sweep_still_advances_the_young_rows() {
        let shared = funnel_gen_vm();
        shared
            .mem
            .string_pool
            .write()
            .insert("gcj1st-skip".to_string(), at(0x1000));
        list_row(&shared, "gcj1st-skip", 0x1000);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
        sweep_after_pause(&shared, &map, &|_| true);
        shared
            .mem
            .concurrent_gc_state
            .set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
        assert_eq!(shared.mem.string_table.skipped_sweeps(), 1, "skipped");
        assert_eq!(
            young_rows_of(&shared),
            vec![("gcj1st-skip".to_string(), 0x1000, 0)],
            "a skipped sweep leaves the row at its PRE-collection address"
        );
        assert_eq!(remap_pool_after_pause(&shared, &map), 1);
        assert_eq!(
            young_rows_of(&shared),
            vec![("gcj1st-skip".to_string(), 0x9000, 0)]
        );
        assert_young_list_exact(&shared);
        assert_eq!(
            shared.mem.string_table.funnel_census().3,
            0,
            "not the shortcut"
        );
    }

    /// End to end under the funnel, over several collections: after each one
    /// the young list is exact (every row names its entry's current String),
    /// the held Strings keep their entries at their current addresses, and
    /// the dropped ones are gone.
    #[test]
    fn gcj1st_the_young_list_stays_exact_across_collections() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = funnel_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let base = thread.native_pin_roots.len();
        for i in 0..8 {
            let held = crate::vm::create_java_string(&shared, &format!("gcj1st-held-{i}"));
            thread.native_pin_roots.push(held);
            intern_and_forget(&shared, &format!("gcj1st-dropped-{i}"));
        }
        for round in 0..3 {
            crate::runtime::interpreter::maybe_gc_forced_pub_at(
                &shared,
                &mut thread,
                "gcj1st-test",
            );
            assert_young_list_exact(&shared);
            let pool = shared.mem.string_pool.read();
            for i in 0..8 {
                let held = pool.get(format!("gcj1st-held-{i}").as_str());
                assert_eq!(
                    held.map(|o| o.as_ptr()),
                    Some(thread.native_pin_roots[base + i].as_ptr()),
                    "round {round}: a held String keeps its entry, at its current address"
                );
                assert!(
                    pool.get(format!("gcj1st-dropped-{i}").as_str()).is_none(),
                    "round {round}: an unreferenced String's entry is dropped"
                );
            }
        }
        thread.native_pin_roots.truncate(base);
    }

    /// The insert grace's switch: opt-in, latched per VM, and engaged only on
    /// a Generational heap with the weak table requested.
    #[test]
    fn gcj1st_insert_grace_is_opt_in_and_generational_only() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE", None)],
            || assert!(!StringTableState::from_flags().insert_grace_requested()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE", Some("1"))],
            || assert!(StringTableState::from_flags().insert_grace_requested()),
        );
        assert!(!StringTableState::with(true, true).insert_grace_requested());

        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        assert!(!insert_grace_engaged(&shared), "off unless requested");
        shared.mem.string_table.set_insert_grace(true);
        assert!(insert_grace_engaged(&shared));
        shared.mem.string_table.set_weak(false);
        assert!(
            !insert_grace_engaged(&shared),
            "the strong pool needs no grace"
        );
        for other in [g1_vm(), zgc_vm()] {
            other.mem.string_table.set_weak(true);
            other.mem.string_table.set_insert_grace(true);
            assert!(!insert_grace_engaged(&other), "G1 / ZGC: never");
            let _ = crate::vm::create_java_string(&other, "gcj1st-grace-other");
            let mut roots = Vec::new();
            push_insert_grace_roots(&other, &mut roots);
            assert!(roots.is_empty(), "nothing rooted");
            assert!(
                other.mem.string_table.fresh.lock().is_empty(),
                "nothing noted"
            );
        }
    }

    /// Under the grace, a String a pooled constructor pooled is a root of the
    /// next scan that leaves the pool out, and the pause's epilogue ends the
    /// grace (and counts it). Kill control: the same scan without the switch
    /// does not root it; `String.intern()`'s receiver path never notes.
    #[test]
    fn gcj1st_insert_grace_roots_a_fresh_string_for_one_pause() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        let thread = JvmThread::new(ThreadId(0), "test");
        let off = crate::vm::create_java_string(&shared, "gcj1st-grace-off");
        assert!(
            !crate::memory::roots::collect_roots(&shared, &thread).contains(&off),
            "kill control: no grace without the switch"
        );

        shared.mem.string_table.set_insert_grace(true);
        let s = crate::vm::create_java_string(&shared, "gcj1st-grace-on");
        let t = crate::vm::try_create_java_string(&shared, "gcj1st-grace-try");
        let t = t.expect("room");
        let reported = crate::vm::try_create_java_string_reporting(&shared, "gcj1st-grace-rep");
        let (u, allocated) = reported.expect("room");
        assert!(allocated);
        let hit = crate::vm::create_java_string(&shared, "gcj1st-grace-on");
        assert_eq!(hit, s, "a hit notes nothing new");
        let receiver = crate::vm::create_java_string_uninterned(&shared, "gcj1st-grace-recv");
        assert_eq!(
            intern_receiver(&shared, receiver, "gcj1st-grace-recv"),
            receiver
        );
        let roots = crate::memory::roots::collect_roots(&shared, &thread);
        for (name, obj) in [("create", s), ("try", t), ("reporting", u)] {
            assert!(roots.contains(&obj), "{name}: rooted by the grace");
        }
        assert!(
            !roots.contains(&receiver),
            "the receiver path does not note"
        );
        assert!(
            !roots.contains(&off),
            "pooled before the switch: not noted"
        );
        assert_eq!(shared.mem.string_table.fresh.lock().len(), 3);

        sweep_after_pause(&shared, &cratonvm_types::PointerMap::default(), &|_| true);
        assert!(
            shared.mem.string_table.fresh.lock().is_empty(),
            "one pause only"
        );
        assert_eq!(shared.mem.string_table.grace_rooted(), 3);
        assert!(
            !crate::memory::roots::collect_roots(&shared, &thread).contains(&s),
            "after the epilogue the String is weak again"
        );
    }

    /// End to end: under the grace an unreferenced String pooled by a
    /// constructor survives the first pause after it was pooled (its entry
    /// stays, checked when the first forced collection ran one sweep) and is
    /// dropped by a later one; without the grace (the positive control of the
    /// hazard's shape) the first collection drops it.
    #[test]
    fn gcj1st_insert_grace_keeps_an_unreferenced_string_one_collection() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        for grace in [true, false] {
            let shared = gen_vm();
            shared.mem.string_table.set_weak(true);
            shared.mem.string_table.set_insert_grace(grace);
            let mut thread = JvmThread::new(ThreadId(0), "test");
            intern_and_forget(&shared, "gcj1st-grace-e2e");
            crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "gcj1st-1");
            let after_one = shared
                .mem
                .string_pool
                .read()
                .contains_key("gcj1st-grace-e2e");
            let (sweeps, _) = shared.mem.string_table.census();
            if !grace {
                assert!(!after_one, "no grace: the first collection drops it");
            } else if sweeps == 1 {
                // One pause so far: the grace covered it.
                assert!(after_one, "grace: kept through the first pause");
            }
            crate::runtime::interpreter::maybe_gc_forced_pub_at(&shared, &mut thread, "gcj1st-2");
            assert!(
                !shared
                    .mem
                    .string_pool
                    .read()
                    .contains_key("gcj1st-grace-e2e"),
                "grace={grace}: dropped once the grace is over"
            );
            assert_eq!(shared.mem.string_table.grace_rooted() >= 1, grace);
        }
    }

    /// gcj j2/rf2 (the g4rs grace residual): the hit grace is opt-in, latched
    /// per VM, and on top of the insert grace. With it, a pool HIT of a String
    /// pooled before any grace was on is rooted by the next scan that leaves
    /// the pool out, for one pause; with the insert grace alone (the kill
    /// control) a hit notes nothing. G1 and ZGC never note.
    #[test]
    fn gcj2rf2_hit_grace_roots_a_hit_for_one_pause() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE", None)],
            || assert!(!StringTableState::from_flags().hit_grace_requested()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE", Some("1"))],
            || assert!(StringTableState::from_flags().hit_grace_requested()),
        );
        let shared = gen_vm();
        shared.mem.string_table.set_weak(true);
        let thread = JvmThread::new(ThreadId(0), "test");
        let pooled = crate::vm::create_java_string(&shared, "gcj2rf2-hit");
        shared.mem.string_table.set_insert_grace(true);
        assert_eq!(crate::vm::create_java_string(&shared, "gcj2rf2-hit"), pooled);
        assert!(
            !crate::memory::roots::collect_roots(&shared, &thread).contains(&pooled),
            "kill control: the insert grace alone notes no hit"
        );
        shared.mem.string_table.set_hit_grace(true);
        assert!(hit_grace_engaged(&shared));
        assert_eq!(crate::vm::create_java_string(&shared, "gcj2rf2-hit"), pooled);
        assert!(
            crate::memory::roots::collect_roots(&shared, &thread).contains(&pooled),
            "the hit is rooted by the grace"
        );
        assert_eq!(shared.mem.string_table.grace_hits(), 1);
        sweep_after_pause(&shared, &cratonvm_types::PointerMap::default(), &|_| true);
        assert!(
            !crate::memory::roots::collect_roots(&shared, &thread).contains(&pooled),
            "one pause only"
        );
        shared.mem.string_table.set_insert_grace(false);
        assert!(!hit_grace_engaged(&shared), "needs the insert grace");
        for other in [g1_vm(), zgc_vm()] {
            other.mem.string_table.set_weak(true);
            other.mem.string_table.set_insert_grace(true);
            other.mem.string_table.set_hit_grace(true);
            assert!(!hit_grace_engaged(&other), "G1 / ZGC: never");
            let _ = crate::vm::create_java_string(&other, "gcj2rf2-hit-other");
            let _ = crate::vm::create_java_string(&other, "gcj2rf2-hit-other");
            assert!(
                other.mem.string_table.fresh.lock().is_empty(),
                "nothing noted"
            );
            assert_eq!(other.mem.string_table.grace_hits(), 0);
        }
    }
}
