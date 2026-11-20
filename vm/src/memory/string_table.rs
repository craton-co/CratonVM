// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The interned-String table's GC disposition (gcg g2/w,
//! `docs/feature-designs/real-jdk-mode.md`).
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
//! `docs/feature-designs/real-jdk-mode.md`
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
//!   `docs/feature-designs/real-jdk-mode.md`).
//!
//! Off the switch nothing changes: the constructors keep their old bodies,
//! `intern_receiver` its old insert, and the sweep / watch their walks.
//!
//! # The insert grace (`CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE`, opt-in since gcj j1/st)
//!
//! The blanket mitigation of
//! `docs/feature-designs/real-jdk-mode.md`:
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
            true,
            true,
        );
        state.set_watch_young(true);
        // zt t10: `CRATONVM_ZGC_WEAK_STRING_TABLE` default ON (`=0` kill) on the
        // stop-the-world-only ZGC heap (`zgc_weak_string_table_sound`): rows
        // I1-I6 SAME as HotSpot 25.0.3 in both modes, 40/40 soundness runs
        // clean, CPU 0.28-0.89x of the strong pool (zr z1/rf rows).
        state.set_zgc_weak(true);
        // zt t13/x10: the ZGC arm's "strong" answer is sticky (`=0` kill).
        state.set_zgc_strong_latch(true);
        // zt t23/st1: the ZGC arm under a concurrent mark, a parity fix
        // (HotSpot 25's StringTable is weak on every ZGC cycle), default ON
        // (`=0` kill: the strong pool on a concurrently marking heap, as before).
        state.set_zgc_conc_weak(true);
        // zt t27/gn5: the ZGC arm in the generational mode, a parity fix
        // (HotSpot 25's StringTable is weak; the gn page's item 13), default
        // ON (`=0` kill: the strong pool in the mode, as before).
        state.set_zgc_gen_weak(true);
        // gci i21/sw: opt-in until its deciding rows pass
        // (`docs/feature-designs/real-jdk-mode.md`).
        state.set_g1_weak(true);
        // gci i25/st2: opt-in until its deciding rows pass
        // (`docs/feature-designs/real-jdk-mode.md`).
        state.set_funnel(false);
        // gci i47w/ws47: a soundness fix, default ON (`=0` kills).
        state.set_old_exact(true);
        // gcj j1/st: opt-in (a Java-visible over-retention of one pause).
        state.set_insert_grace(false);
        // gcj j2/rf2: opt-in, and only with the insert grace.
        state.set_hit_grace(false);
        // gcj j3/rf3: opt-in until its deciding rows pass.
        state.set_seed_only_young(true);
        // gcj j3/rf3: opt-in until its deciding rows pass.
        state.set_young_in_cycle(true);
        state.dbg_line.store(
            false,
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

/// gch h2/jr (`docs/feature-designs/real-jdk-mode.md`,
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
    heap.gen_old_gen_sorted_object_bases()
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
    shared.mem.heap.g1_parked_mirror_keyed_view(false).0
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
    true
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


