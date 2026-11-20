#[allow(unused_imports)]
use super::*;

impl ThreadRegistry {
    /// Create a new, empty registry. The next thread id will be 1
    /// (id 0 is reserved for the main thread).
    pub fn new() -> Self {
        static NEXT_REGISTRY_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            threads: RwLock::new(FxHashMap::default()),
            java_ended_thread: AtomicU64::new(u64::MAX),
            destroy_java_vm_listed: AtomicBool::new(false),
            dead_entries: std::sync::atomic::AtomicUsize::new(0),
            jmm_started: AtomicU64::new(0),
            jmm_live: std::sync::atomic::AtomicI32::new(0),
            jmm_peak: std::sync::atomic::AtomicI32::new(0),
            jmm_roster_live: std::sync::atomic::AtomicI32::new(0),
            jmm_roster_daemon: std::sync::atomic::AtomicI32::new(0),
            next_id: AtomicU64::new(1),
            registry_id: NEXT_REGISTRY_ID.fetch_add(1, Ordering::Relaxed),
            thread_obj_to_park: Mutex::new(FxHashMap::default()),
            java_tid_to_id: Mutex::new(FxHashMap::default()),
            former_mirror_addrs: Mutex::new((
                FxHashMap::default(),
                std::collections::VecDeque::new(),
            )),
            synchronizer_owner: std::array::from_fn(|_| Mutex::new(FxHashMap::default())),
            no_roots_lazy_claims_enabled: true,
            ffm_in_native_no_signal: true,
            blocked_in_native_runnable: true,
            thread_info_frameless: true,
            thread_dump_one_pause: true,
            threads_attach_order: true,
            // JIT round 23 wave 6 (proxy-define page): default ON, `=0` kills;
            // compiled code now serves it at its helper-entry door
            // (`CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY`), so it is taken in
            // microseconds on a peer that calls helpers, and the frame-trace
            // pause stays the fallback after the 1 ms budget.
            handshake_frame_trace: true,
            handshake_loop_kick: true,
            handshake_spin_wait: true,
            dbg_stw_expected_ids: None::<std::ffi::OsString>
            .is_some(),
            handshake_target_kick: true,
            // WARNING t1-user-flip-20261026: default ON, `=0` kills.
            handshake_poll_bit: true,
            // t1 lane E2: opt-in, performance.
            handshake_wait_sees_block: true,
            // t1 wave 8 lane HANDSHK: opt-in, performance.
            handshake_unmounted_defers_kick: true,
            handshake_requester_safe_while_waiting: true,
            os_tid_identity_skips_unmounted: true,
            published_wait_kind: true,
            published_all_wait_kinds: true,
            served_wait_kinds: true,
            unrecorded_wait_census: UnrecordedWaitCensus::from_env(),
            vm_internal_waits_runnable: true,
            stand_in_wait_objects: true,
            park_lazy_budget: {
                crate::threading::monitor::monitor_lazy_park_budget()
            },
            park_lazy_stw_wake: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_LAZY_STW_WAKE",
            ),
            lazy_park_phases: AtomicU64::new(0),
            lazy_park_states: Mutex::new(Vec::new()),
            park_deposit_frozen_band: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_DEPOSIT_FROZEN_BAND",
            ),
            park_defers_jit_chain:
                crate::jit::xt_root_scan::helper_window_deferred_chain_native_enabled(),
            unpark_one_lookup: true,
            blocking_leave_keeps_snapshot: true,
            blocking_leave_key_reuses_buffers: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS",
            ),
            park_lazy_suspend_aware: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_LAZY_SUSPEND_AWARE",
            ),
            park_atomic_permit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_ATOMIC_PERMIT",
            ),
            park_lazy_spin: park_lazy_spin_from(
                cratonvm_types::flags::runtime_var("CRATONVM_PARK_LAZY_SPIN_NS")
                    .ok()
                    .as_deref(),
                std::thread::available_parallelism().map_or(1, |n| n.get()),
            ),
            park_lazy_adaptive: true,
            unpark_eetop: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_UNPARK_EETOP"),
            interrupt_locks: std::array::from_fn(|_| Mutex::new(())),
            interrupt_lock_enabled: true,
            park_no_poll: true,
            sleep_no_poll: true,
            park_wake_after_unlock: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_WAKE_AFTER_UNLOCK",
            ),
            park_untimed_event: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_UNTIMED_EVENT",
            ),
            park_event_signal_after_unlock: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK",
            ),
            park_event_lockless_wake: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_EVENT_LOCKLESS_WAKE",
            ),
            sleep_high_resolution_interval: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL",
            ),
            interrupt_one_lookup: true,
            interrupt_one_wake: true,
            interrupt_field_memo: true,
            dbg_interrupt: None::<std::ffi::OsString>
                .is_some(),
            thread_start_one_publish: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_THREAD_START_ONE_PUBLISH",
            ),
            vt_parked_fold_filter: true,
            dbg_vt_parked_roots: false,
            vt_parked_gather_filter: true,
            parked_split: Mutex::new(ParkedSplitGather::default()),
            vt_parked_mirror_keyed: true,
            mirror_keyed: Mutex::new(MirrorKeyedGather::default()),
            g1_vt_precise_deposit: true,
        }
    }

    /// `CRATONVM_G1_VT_PRECISE_DEPOSIT` for this VM (gci i18/hw): one load of
    /// a field fixed at construction, once per blocking deposit.
    #[inline(always)]
    pub fn g1_vt_precise_deposit(&self) -> bool {
        self.g1_vt_precise_deposit
    }

    /// gci i13/vg: ask the NEXT [`Self::collect_all_root_snapshots`] for a
    /// split gather against `young` (the collector's young set for this
    /// pause, `VmHeap::parked_old_only_young_ranges`). The collection door
    /// arms it right before its gather and takes the old-only section right
    /// after ([`Self::take_parked_old_only`]); no other gather runs between
    /// the two (both are inside the door's pause, on its thread).
    ///
    /// gcj j3/x2 R10: the dedupe arm's owner-thread rule
    /// ([`Self::arm_initiator_snapshot_skip`]) for this arm too. It records the
    /// ARMING thread, and only a gather on that thread takes it; a plain
    /// gather on any other thread between the arm and the door's own is the
    /// whole gather, leaves the arm, and is counted
    /// ([`Self::parked_split_foreign_gathers`]). Before, such a gather took the
    /// split -- its old-only section diverted where nobody would read it --
    /// and the door's gather got the whole set. Serialised by the pause
    /// today, so the counter reads 0 on a healthy run.
    pub fn arm_parked_split_gather(&self, young: &[(usize, usize)]) {
        let mut filter = FoldAddrFilter::default();
        for &(lo, hi) in young {
            filter.insert_range(lo, hi);
        }
        let mut s = self.parked_split.lock();
        s.armed = Some((filter, std::thread::current().id()));
        s.old_only.clear();
    }

    /// gcj j3/x2 R10: plain gathers on a thread other than the arming one that
    /// left an armed parked split alone (see [`Self::arm_parked_split_gather`]).
    pub fn parked_split_foreign_gathers(&self) -> u64 {
        self.parked_split.lock().split_foreign_gathers
    }

    /// gci i13/vg: the old-only section of the last armed gather (empty when
    /// none was armed or nothing qualified), and disarm. Idempotent.
    pub fn take_parked_old_only(&self) -> Vec<ObjectRef> {
        let mut s = self.parked_split.lock();
        s.armed = None;
        std::mem::take(&mut s.old_only)
    }

    /// gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, opt-in; step 11 of
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// ask the NEXT [`Self::collect_all_root_snapshots`] to leave out the
    /// snapshot words of every alive entry that shares `initiator_snapshot`
    /// (`Arc::ptr_eq`) -- the collection initiator's own `root_snapshot`,
    /// whose words its `collect_roots` step 11 pushes in the same pause.
    ///
    /// Armed by the collection door (`run_collection_pause`) right before its
    /// `collect_roots_registry_appended` and consumed by the one gather that
    /// follows it, exactly like [`Self::arm_parked_split_gather`]; no other
    /// gather runs between the two (both inside the door's pause, on its
    /// thread). Only that door may arm it: every other gather (the concurrent
    /// cycles' marks, HPROF) has no step 11 in front of it.
    ///
    /// gcj j3/x2 (the flips page's item (i)): the arm records the ARMING
    /// thread, and only a gather on that thread consumes it. A gather on any
    /// other thread between the arm and the door's own (HPROF, another door)
    /// is the historical whole gather and leaves the arm for the door; it is
    /// counted ([`Self::initiator_skip_foreign_gathers`]). Before, the first
    /// gather on ANY thread took the arm and left the initiator's words --
    /// not duplicated in that gather's root set -- out of it.
    pub fn arm_initiator_snapshot_skip(&self, initiator_snapshot: &Arc<Mutex<Vec<ObjectRef>>>) {
        let mut s = self.parked_split.lock();
        s.initiator_skip = Some((Arc::clone(initiator_snapshot), std::thread::current().id()));
        s.initiator_skipped = None;
    }

    /// gcj j3/x2: how many plain gathers ran on a thread other than the
    /// arming one while an initiator-snapshot skip was armed (each left the
    /// arm alone). 0 on a healthy run: the door's pause serialises gathers.
    pub fn initiator_skip_foreign_gathers(&self) -> u64 {
        self.parked_split.lock().initiator_foreign_gathers
    }

    /// gci i21/pr: disarm [`Self::arm_initiator_snapshot_skip`] and return
    /// how many snapshot words the gather that consumed it left out (`None`:
    /// no gather consumed it). Idempotent.
    pub fn disarm_initiator_snapshot_skip(&self) -> Option<usize> {
        let mut s = self.parked_split.lock();
        s.initiator_skip = None;
        s.initiator_skipped.take()
    }

    /// `CRATONVM_GC_VT_PARKED_GATHER_FILTER` for this VM (gci i13/vg, the
    /// gather half of i12/vp's page): one load of a field fixed at construction, once per
    /// collection pause.
    #[inline(always)]
    pub fn vt_parked_gather_filter(&self) -> bool {
        self.vt_parked_gather_filter
    }

    /// Test hook: set [`Self::vt_parked_gather_filter`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_vt_parked_gather_filter_for_test(&mut self, on: bool) {
        self.vt_parked_gather_filter = on;
    }

    /// `CRATONVM_GC_VT_PARKED_MIRROR_KEYED` for this VM (gci i14/ve): one
    /// load of a field fixed at construction, once per collection pause.
    #[inline(always)]
    pub fn vt_parked_mirror_keyed(&self) -> bool {
        self.vt_parked_mirror_keyed
    }

    /// Test hook: set [`Self::vt_parked_mirror_keyed`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_vt_parked_mirror_keyed_for_test(&mut self, on: bool) {
        self.vt_parked_mirror_keyed = on;
    }

    /// gci i14/ve: ask the NEXT [`Self::collect_all_root_snapshots`] to key
    /// the eligible parked continuations by their mirrors
    /// ([`mirror_keyed_mirror`]). `vm` is this VM's identity (the
    /// `metadata_pin` rows are per VM), `[old_base, old_base + old_len)` the
    /// Generational old generation, and `candidates` the registry ids of the
    /// JDK continuations whose state is deposited in the virtual-thread
    /// manager right now (`VirtualThreadManager::deposited_jdk_continuation_ids`).
    /// The collection door arms it inside its pause, right before its one
    /// gather, and only when the pause is licensed
    /// (`gc_and_alloc::arm_parked_mirror_keyed_gather`); the guard it returns
    /// disarms on drop. Does not touch the deferred list.
    pub fn arm_mirror_keyed_gather(
        &self,
        vm: usize,
        old_base: usize,
        old_len: usize,
        candidates: Vec<u64>,
    ) {
        let candidates: rustc_hash::FxHashSet<u64> = candidates.into_iter().collect();
        self.mirror_keyed.lock().armed = Some(MirrorKeyedArm {
            vm,
            old_base,
            old_len,
            old_ranges: Vec::new(),
            candidates,
        });
    }

    /// gci i14/ve: drop an armed request no gather consumed (the door's
    /// guard). Idempotent; the deferred list is the sweep's.
    pub fn disarm_mirror_keyed_gather(&self) {
        self.mirror_keyed.lock().armed = None;
    }

    /// gci i14/ve: the door could not read the candidates without blocking
    /// (the virtual-thread manager's lock was held elsewhere), so this pause
    /// keys nothing and gathers as before. Counted for the census line's
    /// ` keyed_skipped_contended=`.
    pub fn note_mirror_keyed_contended(&self) {
        self.mirror_keyed.lock().skipped_contended += 1;
    }

    /// gci i14/ve: how many pauses [`Self::note_mirror_keyed_contended`]
    /// counted. Test-only since JIT round 22 wave 7 (lane review): the census
    /// line reads the field itself, nothing in the VM calls this.
    #[cfg(test)]
    pub fn mirror_keyed_skipped_contended(&self) -> u64 {
        self.mirror_keyed.lock().skipped_contended
    }

    /// gci i14/ve: the verdict half, inside the SAME pause as the gather that
    /// left entries out (`memory::gc::update_all_roots`, with the in-pause
    /// reclaimed-hole verdict). For every `(tid, mirror)` that gather keyed:
    /// a mirror the pause moved (`pointer_map` key) or kept in place
    /// (`survived`, asked of the PRE-remap address, as every address-keyed
    /// sweep there asks it) keeps its continuation; a mirror that did not
    /// survive was unreachable, so its frames -- `metadata_pin` values owned
    /// by it -- were not marked and may be freed already. That entry is
    /// retired HERE, before the blocked fold, the mirror remap and any later
    /// pause can read its words: TLAB / `JvmThread` addresses withdrawn and
    /// the entry marked dead (a dead entry is never gathered, folded or
    /// counted). Its execution state is dropped after the pause
    /// (`jdk_continuation::retire_collected_continuations`, from
    /// [`Self::take_collected_parked`]). An entry that is no longer the same
    /// unmounted thread of that mirror is left alone (cannot happen: a
    /// mount needs a carrier holding the mirror). Returns how many retired.
    pub fn sweep_mirror_keyed_parked(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        survived: &dyn Fn(usize) -> bool,
    ) -> usize {
        let deferred = std::mem::take(&mut self.mirror_keyed.lock().deferred);
        if deferred.is_empty() {
            return 0;
        }
        let dead = mirror_keyed_dead(&deferred, pointer_map, survived);
        let retired = self.retire_mirror_keyed_dead(&dead);
        if self.dbg_vt_parked_roots {
            eprintln!(
                "[vt-parked-roots] mirror-keyed deferred={} collected={}",
                deferred.len(),
                retired.len()
            );
        }
        let n = retired.len();
        self.mirror_keyed.lock().collected.extend(retired);
        n
    }

    /// gci i14/ve (split out by gci i15/vh, unchanged): retire every
    /// `(tid, mirror)` of `dead` that is still that unmounted, alive thread of
    /// that mirror -- TLAB address withdrawn, entry marked dead -- and return
    /// their ids. Any other entry is left alone with an error line (cannot
    /// happen: a mount needs a carrier holding the mirror).
    pub(in crate::threading::thread_registry) fn retire_mirror_keyed_dead(&self, dead: &[(ThreadId, usize)]) -> Vec<ThreadId> {
        let mut retired: Vec<ThreadId> = Vec::new();
        {
            let threads = self.threads.read();
            for &(tid, mirror) in dead {
                let Some(entry) = threads.get(&tid) else {
                    continue;
                };
                let same = entry.alive.load(Ordering::Acquire)
                    && !entry.mounted.load(Ordering::Acquire)
                    && entry.java_thread_obj.map(|m| m.as_ptr() as usize) == Some(mirror);
                if same {
                    retired.push(tid);
                } else {
                    tracing::error!(
                        "gci i14/ve: mirror-keyed thread {} lost its mirror {mirror:#x} but is \
                         no longer that parked thread; left alone",
                        tid.0
                    );
                }
            }
        }
        for &tid in &retired {
            self.clear_tlab_addr(tid);
            self.mark_dead(tid);
        }
        retired
    }

    /// gci i14/ve: the ids [`Self::sweep_mirror_keyed_parked`] retired, for
    /// the door to drop their execution states once the world is released.
    pub fn take_collected_parked(&self) -> Vec<ThreadId> {
        std::mem::take(&mut self.mirror_keyed.lock().collected)
    }

    /// gci i15/vh (G1): [`Self::arm_mirror_keyed_gather`] with the old space
    /// given as G1's `Old`-region ranges (`G1Collector::parked_mirror_keyed_view`)
    /// instead of one Generational span. Sorted here, so a caller's order does
    /// not matter. Armed only by the two G1 marking root scans (the initial
    /// mark and the remark, `gc_and_alloc::g1_key_parked_for_mark`).
    pub fn arm_mirror_keyed_gather_ranges(
        &self,
        vm: usize,
        mut old_ranges: Vec<(usize, usize)>,
        candidates: Vec<u64>,
    ) {
        old_ranges.retain(|&(lo, hi)| lo < hi);
        old_ranges.sort_unstable();
        let candidates: rustc_hash::FxHashSet<u64> = candidates.into_iter().collect();
        self.mirror_keyed.lock().armed = Some(MirrorKeyedArm {
            vm,
            old_base: 0,
            old_len: 0,
            old_ranges,
            candidates,
        });
    }

    /// gci i15/vh (G1): the mirrors of the pairs the last keyed gather left
    /// out and nobody judged yet, one per keyed thread, for the G1 marking
    /// door to take step 10b's copy of each out of its `collect_roots` result
    /// ([`strip_keyed_mirror_registry_roots`]).
    pub fn mirror_keyed_deferred_mirrors(&self) -> Vec<usize> {
        self.mirror_keyed
            .lock()
            .deferred
            .iter()
            .map(|&(_, mirror)| mirror)
            .collect()
    }

    /// gci i15/vh (G1): drop the deferred pairs and any judgement without a
    /// verdict -- the initial mark (its mark is not complete; only the remark
    /// judges), an unwinding or lost remark, and the door's guard. The rows
    /// stay (the cycle's marker follows them; the first scan after the cycle
    /// drops them). Never touches `collected`.
    pub fn forget_mirror_keyed_deferred(&self) {
        let mut keyed = self.mirror_keyed.lock();
        keyed.deferred.clear();
        keyed.judged_dead.clear();
        keyed.judged_deferred = 0;
    }

    /// gci i15/vh (G1, inside the remark callback, after the keep closure is
    /// marked and before cleanup frees anything): judge every pair the
    /// remark's keyed gather left out by the COMPLETED mark (`is_live` is
    /// `G1Collector::is_live_after_mark`: bitmap below TAMS, live above it).
    /// An unmarked mirror's pair is held for
    /// [`Self::settle_mirror_keyed_parked_after_mark`]; nothing is retired
    /// yet, because only cleanup says whether it trusted this mark. Consumes
    /// the deferred list. Returns how many were judged dead.
    ///
    /// First, when a G1 marking scan keyed anything this cycle, the values of
    /// every `metadata_pin` row whose owner `is_live` are handed to `mark`
    /// (the door's `G1Collector::resurrect_after_remark`: mark gray and drain
    /// to a fixed point), repeated until no further owner turns live
    /// ([`mark_live_row_values_to_fixpoint`]). The concurrent marker runs
    /// while a marking pause gathers, so a batch that captured the side
    /// tables before the gather filed a mirror's rows may have scanned that
    /// mirror without them (`MarkSideTables` re-captures only per batch): the
    /// frames of a LIVE keyed thread -- and an object the thread moved from a
    /// frame into the heap after the initial mark, still named by that
    /// mark's rows -- would then stay unmarked and cleanup would free them.
    /// The fixed point closes the rows over the completed mark instead of
    /// trusting the marker's timing; every value it marks is reachable (its
    /// owner is). A mirror reachable only from another live keyed thread's
    /// frames turns live inside it, before the verdict below.
    pub fn judge_mirror_keyed_parked_after_mark(
        &self,
        is_live: &dyn Fn(usize) -> bool,
        mark: &mut dyn FnMut(&[usize]),
    ) -> usize {
        let (deferred, cycle_keyed) = {
            let mut keyed = self.mirror_keyed.lock();
            (
                std::mem::take(&mut keyed.deferred),
                std::mem::take(&mut keyed.g1_cycle_keyed),
            )
        };
        if cycle_keyed {
            if let Some(rows) = cratonvm_types::metadata_pin::snapshot() {
                let _ = mark_live_row_values_to_fixpoint(&rows, is_live, mark);
            }
        }
        if deferred.is_empty() {
            return 0;
        }
        let dead = mirror_keyed_dead(&deferred, &cratonvm_types::PointerMap::default(), is_live);
        let n = dead.len();
        let mut keyed = self.mirror_keyed.lock();
        keyed.judged_deferred += deferred.len();
        keyed.judged_dead.extend(dead);
        n
    }

    /// gci i15/vh (G1, the remark pause, right after
    /// `VmHeap::g1_final_remark_and_cleanup` returned): retire the judged
    /// pairs when `trusted` (the cleanup trusted the mark that left their
    /// mirrors unmarked: the mirror is unreachable and its frames, rows owned
    /// by it, were left unmarked and may be in a freed region now), else
    /// keep them (an untrusted cleanup freed nothing and proved nothing; the
    /// entries are rooted again by every later gather). Either way the
    /// judgement and any unjudged deferred pair are consumed. Retired ids join
    /// `collected` for the door's post-pause release. Returns how many retired.
    pub fn settle_mirror_keyed_parked_after_mark(&self, trusted: bool) -> usize {
        let (dead, judged) = {
            let mut keyed = self.mirror_keyed.lock();
            keyed.deferred.clear();
            let judged = std::mem::take(&mut keyed.judged_deferred);
            (std::mem::take(&mut keyed.judged_dead), judged)
        };
        if judged == 0 && dead.is_empty() {
            return 0;
        }
        let retired = if trusted {
            self.retire_mirror_keyed_dead(&dead)
        } else {
            Vec::new()
        };
        if self.dbg_vt_parked_roots {
            eprintln!(
                "[vt-parked-roots] mirror-keyed deferred={judged} collected={} door=g1-remark \
                 trusted={trusted} unmarked={}",
                retired.len(),
                dead.len()
            );
        }
        let n = retired.len();
        self.mirror_keyed.lock().collected.extend(retired);
        n
    }

    /// `CRATONVM_GC_VT_PARKED_FOLD_FILTER` for this VM (gci i12/vp): one load
    /// of a field fixed at construction, once per relocating pause.
    #[inline(always)]
    pub fn vt_parked_fold_filter(&self) -> bool {
        self.vt_parked_fold_filter
    }

    /// Test hook: set [`Self::vt_parked_fold_filter`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_vt_parked_fold_filter_for_test(&mut self, on: bool) {
        self.vt_parked_fold_filter = on;
    }

    /// `CRATONVM_THREAD_START_ONE_PUBLISH` for this VM (interpreter round i3
    /// wave 82, lane L6): one load of a field fixed at construction, once per
    /// thread start.
    #[inline(always)]
    pub fn thread_start_one_publish(&self) -> bool {
        self.thread_start_one_publish
    }

    /// Test hook: set [`Self::thread_start_one_publish`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_thread_start_one_publish_for_test(&mut self, on: bool) {
        self.thread_start_one_publish = on;
    }

    /// `CRATONVM_PARK_LAZY_STW_WAKE` for this VM (interpreter round i3 wave
    /// 72, lane L6): is a park's running phase woken by a pause initiator
    /// ([`Self::wake_lazy_park_phases`]) rather than polling in slices? One
    /// load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_stw_wake(&self) -> bool {
        self.park_lazy_stw_wake
    }

    /// `CRATONVM_PARK_LAZY_ADAPTIVE` for this VM (interpreter round i3 wave
    /// 81, lane L6). One load of a field fixed at construction, on the
    /// park's slow path only.
    #[inline(always)]
    pub fn park_lazy_adaptive(&self) -> bool {
        self.park_lazy_adaptive
    }

    /// Test hook: set [`Self::park_lazy_stw_wake`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_lazy_stw_wake_for_test(&mut self, on: bool) {
        self.park_lazy_stw_wake = on;
    }

    /// `CRATONVM_PARK_DEPOSIT_FROZEN_BAND` for this VM (interpreter round i3
    /// wave 72, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_deposit_frozen_band(&self) -> bool {
        self.park_deposit_frozen_band
    }

    /// gci i47w lane pr47: [`Self::park_defers_jit_chain`]'s field, one load
    /// of a value fixed at construction (the G1 test is the caller's).
    #[inline(always)]
    pub fn park_defers_jit_chain(&self) -> bool {
        self.park_defers_jit_chain
    }

    /// `CRATONVM_UNPARK_ONE_LOOKUP` for this VM (interpreter round i3 wave
    /// 72, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn unpark_one_lookup(&self) -> bool {
        self.unpark_one_lookup
    }

    /// `CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT` for this VM (interpreter
    /// round i4 wave 99, lane L1): one load of a field fixed at construction.
    #[inline(always)]
    pub fn blocking_leave_keeps_snapshot(&self) -> bool {
        self.blocking_leave_keeps_snapshot
    }

    /// `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS` for this VM (interpreter
    /// round i5 wave 100, lane L1): one load of a field fixed at construction.
    #[inline(always)]
    pub fn blocking_leave_key_reuses_buffers(&self) -> bool {
        self.blocking_leave_key_reuses_buffers
    }

    /// `CRATONVM_PARK_LAZY_SUSPEND_AWARE` for this VM (interpreter round i3
    /// wave 72, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_suspend_aware(&self) -> bool {
        self.park_lazy_suspend_aware
    }

    /// `CRATONVM_PARK_ATOMIC_PERMIT` for this VM (interpreter round i3 wave
    /// 73, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_atomic_permit(&self) -> bool {
        self.park_atomic_permit
    }

    /// `CRATONVM_PARK_LAZY_SPIN_NS` for this VM (interpreter round i3 wave
    /// 79, lane L6): the permit spin before a park's running phase waits,
    /// `None` when off. One load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_spin(&self) -> Option<std::time::Duration> {
        self.park_lazy_spin
    }

    /// Test hook: set [`Self::park_lazy_spin`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_lazy_spin_for_test(&mut self, spin: Option<std::time::Duration>) {
        self.park_lazy_spin = spin;
    }

    /// Test hook: set [`Self::park_atomic_permit`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_atomic_permit_for_test(&mut self, on: bool) {
        self.park_atomic_permit = on;
    }

    /// `CRATONVM_UNPARK_EETOP` for this VM (interpreter round i3 wave 73,
    /// lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn unpark_eetop(&self) -> bool {
        self.unpark_eetop
    }

    /// `CRATONVM_PARK_UNTIMED_NO_POLL` for this VM (interpreter round i3 wave
    /// 74, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_no_poll(&self) -> bool {
        self.park_no_poll
    }

    /// Test hook: set [`Self::park_no_poll`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_no_poll_for_test(&mut self, on: bool) {
        self.park_no_poll = on;
    }

    /// `CRATONVM_SLEEP_NO_POLL` for this VM (interpreter round i3 wave 75,
    /// lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn sleep_no_poll(&self) -> bool {
        self.sleep_no_poll
    }

    /// Test hook: set [`Self::sleep_no_poll`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_sleep_no_poll_for_test(&mut self, on: bool) {
        self.sleep_no_poll = on;
    }

    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK` for this VM (interpreter round i3
    /// wave 76, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_wake_after_unlock(&self) -> bool {
        self.park_wake_after_unlock
    }

    /// `CRATONVM_PARK_UNTIMED_EVENT` for this VM (interpreter round i5 wave
    /// 107, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_untimed_event(&self) -> bool {
        self.park_untimed_event
    }

    /// `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK` for this VM (interpreter
    /// round i5 wave 108, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_event_signal_after_unlock(&self) -> bool {
        self.park_event_signal_after_unlock
    }

    /// `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` for this VM (interpreter round i5
    /// wave 114, lane L1): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_event_lockless_wake(&self) -> bool {
        self.park_event_lockless_wake
    }

    /// `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL` for this VM (interpreter
    /// round i5 wave 109, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn sleep_high_resolution_interval(&self) -> bool {
        self.sleep_high_resolution_interval
    }

    /// How this VM's `LockSupport.unpark` and `Thread.interrupt` set a
    /// permit and wake (interpreter round i3 wave 77, lane L6;
    /// `ParkState::unpark_with`): `CRATONVM_PARK_ATOMIC_PERMIT`,
    /// `CRATONVM_PARK_UNTIMED_NO_POLL` and `CRATONVM_PARK_WAKE_AFTER_UNLOCK`,
    /// three loads of fields fixed at construction (read on each call, so a
    /// test hook's change is seen).
    #[inline(always)]
    pub fn unpark_shape(&self) -> crate::threading::jvm_thread::UnparkShape {
        crate::threading::jvm_thread::UnparkShape {
            lock_free: self.park_atomic_permit,
            wake_all: self.park_no_poll,
            after_unlock: self.park_wake_after_unlock,
        }
    }

    /// Test hook: set [`Self::park_wake_after_unlock`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_park_wake_after_unlock_for_test(&mut self, on: bool) {
        self.park_wake_after_unlock = on;
    }

    /// `CRATONVM_INTERRUPT_ONE_LOOKUP` for this VM (interpreter round i3 wave
    /// 76, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn interrupt_one_lookup(&self) -> bool {
        self.interrupt_one_lookup
    }

    /// Test hook: set [`Self::interrupt_one_lookup`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_interrupt_one_lookup_for_test(&mut self, on: bool) {
        self.interrupt_one_lookup = on;
    }

    /// `CRATONVM_INTERRUPT_ONE_WAKE` for this VM (interpreter round i4 wave
    /// 91, lane L6): `Thread.interrupt`'s one locked wake of its target's
    /// park state. One load of a field fixed at construction.
    #[inline(always)]
    pub fn interrupt_one_wake(&self) -> bool {
        self.interrupt_one_wake
    }

    /// `CRATONVM_INTERRUPT_FIELD_MEMO` for this VM (interpreter round i3 wave
    /// 77, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn interrupt_field_memo(&self) -> bool {
        self.interrupt_field_memo
    }

    /// Test hook: set [`Self::interrupt_field_memo`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_interrupt_field_memo_for_test(&mut self, on: bool) {
        self.interrupt_field_memo = on;
    }

    /// `CRATONVM_DBG_INTERRUPT` as read once at construction (interpreter
    /// round i3 wave 77, lane L6); `Thread.interrupt` asks it in place of the
    /// environment only with [`Self::interrupt_field_memo`] on.
    #[inline(always)]
    pub fn dbg_interrupt(&self) -> bool {
        self.dbg_interrupt
    }

    /// Run `phase` -- the running phase of a park on `park_state`
    /// (`ParkState::park_lazily_sliced`, whose `keep_running` reads "no pause
    /// is requested" under the state's mutex) -- counted in
    /// [`Self::lazy_park_phases`] and listed in [`Self::lazy_park_states`],
    /// so [`Self::wake_lazy_park_phases`] can wake it (interpreter round i3
    /// wave 72, lane L6, `CRATONVM_PARK_LAZY_STW_WAKE`). Off, this is
    /// `phase()`.
    ///
    /// The handshake is `MonitorTable::lazily_parked_on`'s: the state is
    /// listed BEFORE the count is raised (a `SeqCst` RMW, then a `SeqCst`
    /// fence) and unlisted AFTER it is lowered, and the phase reads
    /// `stw_requested` only after that fence. An initiator raises
    /// `stw_requested`, fences, then reads the count: either the parker sees
    /// the request and leaves, or the initiator sees the count, takes the
    /// list lock (so it sees this entry) and wakes the state under its mutex,
    /// which the parker holds from its read until it is inside the wait.
    pub(crate) fn lazily_parked<R>(
        &self,
        park_state: &Arc<ParkState>,
        phase: impl FnOnce() -> R,
    ) -> R {
        if !self.park_lazy_stw_wake {
            return phase();
        }
        struct Leave<'a> {
            count: &'a AtomicU64,
            list: &'a Mutex<Vec<Arc<ParkState>>>,
            state: *const ParkState,
        }
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.count.fetch_sub(1, Ordering::SeqCst);
                let mut list = self.list.lock();
                if let Some(at) = list
                    .iter()
                    .position(|s| std::ptr::eq(Arc::as_ptr(s), self.state))
                {
                    list.swap_remove(at);
                }
            }
        }
        self.lazy_park_states.lock().push(Arc::clone(park_state));
        self.lazy_park_phases.fetch_add(1, Ordering::SeqCst);
        let _leave = Leave {
            count: &self.lazy_park_phases,
            list: &self.lazy_park_states,
            state: Arc::as_ptr(park_state),
        };
        std::sync::atomic::fence(Ordering::SeqCst);
        phase()
    }

    /// Wake every thread in a park's running phase, for a stop-the-world
    /// INITIATOR right after it raised `stw_requested` (interpreter round i3
    /// wave 72, lane L6, `CRATONVM_PARK_LAZY_STW_WAKE`; beside
    /// `MonitorTable::wake_lazy_parkers` at both `request_stw_opening_cycle`
    /// winners). Each woken parker re-reads `stw_requested`, leaves the
    /// phase and takes the blocking park, whose GC-blocked entry arrives at
    /// the pause. Off: one load. On with nobody in the phase: a fence and a
    /// load. The list lock is dropped before any state's mutex is taken.
    pub fn wake_lazy_park_phases(&self) {
        if !self.park_lazy_stw_wake {
            return;
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.lazy_park_phases.load(Ordering::SeqCst) == 0 {
            return;
        }
        let parked: Vec<Arc<ParkState>> = self.lazy_park_states.lock().clone();
        for state in parked {
            state.wake_lazy_phase();
        }
    }

    /// How many park phases are listed for [`Self::wake_lazy_park_phases`].
    /// Tests.
    #[cfg(test)]
    pub(in crate::threading::thread_registry) fn lazy_park_state_count(&self) -> usize {
        self.lazy_park_states.lock().len()
    }

    /// `CRATONVM_PARK_LAZY` for this VM (interpreter round i3 wave 71, lane
    /// L1): the running phase a platform thread's `Unsafe.park` takes before
    /// its blocking prologue (`NativeContextImpl::park_lazily` in `vm_exec`),
    /// `None` when off. One load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_budget(&self) -> Option<std::time::Duration> {
        self.park_lazy_budget
    }

    /// Test hook: set [`Self::park_lazy_budget`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_lazy_budget_for_test(&mut self, budget: Option<std::time::Duration>) {
        self.park_lazy_budget = budget;
    }

    /// nio21 wave 5 L1: the per-thread interrupt lock guarding `flag` (the
    /// `Arc<AtomicBool>` a `JvmThread` and its registry entry share), or
    /// `None` under `CRATONVM_INTERRUPT_LOCK=0`. Hold the guard across the
    /// flag store and the `Thread.interrupted` field store, and nothing else:
    /// resolve the field slot before taking it. See
    /// [`Self::interrupt_locks`].
    #[inline]
    pub fn lock_interrupt(&self, flag: &AtomicBool) -> Option<parking_lot::MutexGuard<'_, ()>> {
        if !self.interrupt_lock_enabled {
            return None;
        }
        Some(self.interrupt_locks[interrupt_lock_stripe(flag)].lock())
    }

    /// Test hook: set `CRATONVM_INTERRUPT_LOCK` for this registry without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_interrupt_lock_enabled_for_test(&mut self, on: bool) {
        self.interrupt_lock_enabled = on;
    }

    /// `CRATONVM_STAND_IN_WAIT_OBJECTS` for this VM (interpreter round i2
    /// wave 68, lane L1): does a stand-in's blocking region publish the
    /// object its JDK code waits or parks on? One load of a field fixed at
    /// construction.
    #[inline(always)]
    pub fn stand_in_wait_objects(&self) -> bool {
        self.stand_in_wait_objects
    }

    /// Test hook: set [`Self::stand_in_wait_objects`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_stand_in_wait_objects_for_test(&mut self, on: bool) {
        self.stand_in_wait_objects = on;
    }

    /// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE` for this VM (interpreter round
    /// i2 wave 66, lane L1): does a wait HotSpot performs inside the VM read
    /// RUNNABLE (`NativeContext::begin_vm_wait_region`)? One load of a field
    /// fixed at construction.
    #[inline(always)]
    pub fn vm_internal_waits_runnable(&self) -> bool {
        self.vm_internal_waits_runnable
    }

    /// Is `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` counting for this VM
    /// (interpreter round i2 wave 66, lane L1)? One load of a field fixed at
    /// construction; the kind-less doors ask it before anything else.
    #[inline(always)]
    pub fn unrecorded_wait_census_on(&self) -> bool {
        self.unrecorded_wait_census.mode != UnrecordedWaitMode::Off
    }

    /// The census mode (`CRATONVM_DBG_UNRECORDED_WAIT_KINDS`).
    pub fn unrecorded_wait_mode(&self) -> UnrecordedWaitMode {
        self.unrecorded_wait_census.mode
    }

    /// Test hook: set the census mode without the environment.
    #[cfg(test)]
    pub(crate) fn set_unrecorded_wait_mode_for_test(&mut self, mode: UnrecordedWaitMode) {
        self.unrecorded_wait_census.mode = mode;
    }

    /// Count one blocking region that opened with no recorded wait kind, at
    /// `site` (the caller of the door), through `door` (interpreter round i2
    /// wave 66, lane L1, `CRATONVM_DBG_UNRECORDED_WAIT_KINDS`;
    /// `docs/feature-designs/real-jdk-mode.md`).
    /// Under `trace` / `assert` the first region of a site prints one
    /// `[cratonvm] unrecorded-wait-kind:` line; under `assert` a debug build
    /// then fails. Nothing when the census is off. Called by the door before
    /// it deposits, while the thread is still a running mutator; takes only
    /// the census's own mutex.
    #[cold]
    #[inline(never)]
    pub fn note_unrecorded_wait_region(
        &self,
        site: &'static std::panic::Location<'static>,
        door: &'static str,
    ) {
        let mode = self.unrecorded_wait_census.mode;
        if mode == UnrecordedWaitMode::Off {
            return;
        }
        let first = {
            let mut sites = self.unrecorded_wait_census.sites.lock();
            let count = sites
                .entry((site.file(), site.line(), site.column(), door))
                .or_insert(0);
            *count = count.saturating_add(1);
            *count == 1
        };
        if first && matches!(mode, UnrecordedWaitMode::Trace | UnrecordedWaitMode::Assert) {
            eprintln!(
                "[cratonvm] unrecorded-wait-kind: {door} at {}:{}:{}",
                site.file(),
                site.line(),
                site.column()
            );
        }
        if mode == UnrecordedWaitMode::Assert {
            debug_assert!(
                false,
                "CRATONVM_DBG_UNRECORDED_WAIT_KINDS=assert: a blocking region opened with no \
                 recorded wait kind ({door} at {}:{}:{}); open it with \
                 begin_blocking_region_as and the BlockKind of the JDK code it stands for",
                site.file(),
                site.line(),
                site.column()
            );
        }
    }

    /// The census table (interpreter round i2 wave 66, lane L1,
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS`): one row per call site and door,
    /// most regions first, then by site. Empty when the census is off or
    /// nothing was counted.
    pub fn unrecorded_wait_sites(&self) -> Vec<UnrecordedWaitSite> {
        let mut rows: Vec<UnrecordedWaitSite> = self
            .unrecorded_wait_census
            .sites
            .lock()
            .iter()
            .map(|(&(file, line, column, door), &count)| UnrecordedWaitSite {
                site: format!("{file}:{line}:{column}"),
                door,
                count,
            })
            .collect();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.site.cmp(&b.site)));
        rows
    }

    /// Print the census table to stderr (interpreter round i2 wave 66, lane
    /// L1): a `[cratonvm] unrecorded-wait-kinds: R regions at S sites` header,
    /// then one `[cratonvm] unrecorded-wait-kinds:   <count> <door> <site>`
    /// line per row. Nothing when the census is off; the header alone (with
    /// zeros) when it counted nothing, so a clean run says so. Called by the
    /// launcher's shutdown reports (`cratonvm_vm::report_unrecorded_wait_regions_at_exit`).
    pub fn report_unrecorded_wait_regions(&self) {
        if !self.unrecorded_wait_census_on() {
            return;
        }
        let rows = self.unrecorded_wait_sites();
        let regions: u64 = rows.iter().map(|r| r.count).sum();
        eprintln!(
            "[cratonvm] unrecorded-wait-kinds: {regions} regions at {} sites",
            rows.len()
        );
        for row in &rows {
            eprintln!(
                "[cratonvm] unrecorded-wait-kinds:   {} {} {}",
                row.count, row.door, row.site
            );
        }
    }

    /// `CRATONVM_SERVED_WAIT_KINDS` for this VM (interpreter round i2 wave 65,
    /// lane L1): does a served stand-in's `Object.wait` that stands for JDK
    /// code that parks (`NativeContext::monitor_wait_as`) report the JDK
    /// body's state and publish `jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED`?
    /// One load of a field fixed at construction.
    #[inline(always)]
    pub fn served_wait_kinds(&self) -> bool {
        self.served_wait_kinds
    }

    /// Test hook: set [`Self::served_wait_kinds`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_served_wait_kinds_for_test(&mut self, on: bool) {
        self.served_wait_kinds = on;
    }

    /// Is live thread `thread_id` inside a served stand-in's `Object.wait`
    /// that stands for JDK code that parks, as the wait published it
    /// (`jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED`; interpreter round i2
    /// wave 65, lane L1, `CRATONVM_SERVED_WAIT_KINDS`)? JVMTI's kind bit
    /// (`jvmti::native_env::wait_kind`) asks it ahead of the JMX waiting
    /// monitor, which such a wait also publishes (an interrupt wakes the
    /// waiter through it) but which would read `IN_OBJECT_WAIT`. `false`
    /// without the switch, for an unknown or dead thread, and for any other
    /// wait.
    pub fn published_monitor_parked(&self, thread_id: ThreadId) -> bool {
        if !self.served_wait_kinds {
            return false;
        }
        self.gc_block_state_of(thread_id).is_some_and(|block| {
            block.java_wait_kind.load(Ordering::Acquire)
                == crate::threading::jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED
        })
    }

    /// `CRATONVM_PUBLISHED_WAIT_KIND` for this VM (interpreter round i2 wave
    /// 63, lane L1): does a `BlockKind::Sleeping` region publish its wait
    /// kind, and do the JVMTI / JDWP readers trust it
    /// ([`Self::published_sleeping`])? One load of a field fixed at
    /// construction. Also on with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`
    /// ([`Self::published_all_wait_kinds`], wave 64), which publishes the
    /// sleep with every other kind.
    #[inline(always)]
    pub fn published_wait_kind(&self) -> bool {
        self.published_wait_kind || self.published_all_wait_kinds
    }

    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` for this VM (interpreter round i2
    /// wave 64, lane L1): does every wait publish its kind (a park, an
    /// `Object.wait`, the served `Thread.join`), and do the readers trust the
    /// word ([`Self::published_wait_kind_of`], the lazy-wait reading of
    /// [`Self::java_block_state`])? One load of a field fixed at
    /// construction.
    #[inline(always)]
    pub fn published_all_wait_kinds(&self) -> bool {
        self.published_all_wait_kinds
    }

    /// Test hook: set [`Self::published_all_wait_kinds`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_published_all_wait_kinds_for_test(&mut self, on: bool) {
        self.published_all_wait_kinds = on;
    }

    /// Test hook: set [`Self::published_wait_kind`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_published_wait_kind_for_test(&mut self, on: bool) {
        self.published_wait_kind = on;
    }

    /// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE` for this VM (interpreter round i2
    /// wave 59, lane L6): does a `BlockKind::InNative` region report HotSpot's
    /// `_thread_in_native` (RUNNABLE, in native, the native on top of the
    /// stack) rather than WAITING? One load of a field fixed at construction.
    #[inline(always)]
    pub fn blocked_in_native_runnable(&self) -> bool {
        self.blocked_in_native_runnable
    }

    /// Whether no-roots pauses claim lazy FFM downcalls for this VM. Fixed at
    /// registry construction so every downcall return can use one plain load.
    #[inline(always)]
    pub(crate) fn no_roots_lazy_claims_enabled(&self) -> bool {
        self.no_roots_lazy_claims_enabled
    }

    /// `CRATONVM_THREAD_INFO_FRAMELESS` for this VM (interpreter round i2
    /// wave 61, lane L6): does a `ThreadInfo` asked without frames skip the
    /// target's stack capture (and so its frame-trace pause)? One load of a
    /// field fixed at construction.
    #[inline(always)]
    pub fn thread_info_frameless(&self) -> bool {
        self.thread_info_frameless
    }

    /// `CRATONVM_THREAD_DUMP_ONE_PAUSE` for this VM (interpreter round i2
    /// wave 61, lane L6): does a dump of many threads' stacks
    /// (`NativeContext::begin_thread_dump`) take one frame-trace pause for
    /// all its running targets? One load of a field fixed at construction.
    #[inline(always)]
    pub fn thread_dump_one_pause(&self) -> bool {
        self.thread_dump_one_pause
    }

    /// `CRATONVM_THREADS_ATTACH_ORDER` for this VM (interpreter round i6
    /// wave 133, lane L7; HotSpot parity, default on, `=0` off): does
    /// `NativeContext::enumerate_threads` (behind `Thread.getThreads()`,
    /// `ThreadImpl.getThreads()` and `dumpAllThreads`) list the live threads
    /// in attach order ([`Self::alive_thread_objects_in_id_order`]), as
    /// HotSpot's threads list does, instead of the map's hash order? One load
    /// of a field fixed at construction.
    #[inline(always)]
    pub fn threads_attach_order(&self) -> bool {
        self.threads_attach_order
    }

    /// Allocate the next unique `ThreadId`.
    pub fn next_thread_id(&self) -> ThreadId {
        ThreadId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Register a thread (called before or at spawn time).
    pub fn register(&self, thread_id: ThreadId, name: &str, java_thread_obj: Option<ObjectRef>) {
        self.register_with_daemon(thread_id, name, java_thread_obj, false);
    }

    /// T19.K1 — Register a thread with an explicit daemon flag.
    ///
    /// `daemon == true` marks the thread as a JVM "daemon": the CLI's
    /// non-daemon-wait loop ignores it, so the process can exit while
    /// this thread is still running. `daemon == false` (the default)
    /// marks it as a "user" thread that the VM must wait for at
    /// shutdown.
    pub fn register_with_daemon(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
    ) {
        self.register_with_daemon_stw_ready(thread_id, name, java_thread_obj, daemon, true, false);
    }

    /// Register a thread that is alive but not yet able to answer STW barriers.
    pub fn register_starting_with_daemon(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
    ) {
        self.register_with_daemon_stw_ready(thread_id, name, java_thread_obj, daemon, false, false);
    }

    /// zt t15/pl6: [`Self::register_starting_with_daemon`] for a virtual
    /// thread (always a daemon), marked as one for good, so the JMM roster
    /// ([`Self::jmm_perf_thread_counts`]) never lists it, as `ThreadMXBean`
    /// never lists a virtual thread.
    pub fn register_starting_virtual(&self, thread_id: ThreadId, name: &str, vthread: ObjectRef) {
        self.register_with_daemon_stw_ready(thread_id, name, Some(vthread), true, false, true);
    }

    pub(in crate::threading::thread_registry) fn register_with_daemon_stw_ready(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
        stw_ready: bool,
        virtual_thread: bool,
    ) {
        let park_state = Arc::new(ParkState::new());
        if self.park_atomic_permit {
            park_state.enable_lock_free();
        }
        let entry = ThreadEntry {
            name: name.to_string(),
            join_handle: None,
            java_thread_obj,
            java_tid: 0,
            alive: AtomicBool::new(true),
            stw_ready: AtomicBool::new(stw_ready),
            daemon: AtomicBool::new(daemon),
            park_state: park_state.clone(),
            interrupted: Arc::new(AtomicBool::new(false)),
            root_snapshot: Arc::new(Mutex::new(Vec::new())),
            frame_trace: Arc::new(Mutex::new(Vec::new())),
            vm_state: Arc::new(Mutex::new(String::new())),
            gc_block_state: Arc::new(GcBlockState::new()),
            jmx_monitors: Arc::new(JmxMonitorBook::default()),
            jmx_waiting_monitor: Mutex::new(None),
            jmx_locked_synchronizers: Arc::new(Mutex::new(Vec::new())),
            jmx_wait_started: Mutex::new(None),
            jmx_waited_count: AtomicU64::new(0),
            jmx_waited_nanos: AtomicU64::new(0),
            tlab_addr: std::sync::atomic::AtomicUsize::new(0),
            jvm_thread_addr: std::sync::atomic::AtomicUsize::new(0),
            os_tid: std::sync::atomic::AtomicU32::new(0),
            os_tid_seq: AtomicU64::new(0),
            mounted: AtomicBool::new(true),
            mount_epoch: AtomicU64::new(0),
            parked_filter: ParkedFoldFilter::new(),
            registered_at: Instant::now(),
            unstructured_monitor_enters: std::sync::atomic::AtomicU32::new(0),
            jmm_counted: AtomicBool::new(false),
            virtual_thread,
        };
        self.insert_entry(thread_id, entry);
        if let Some(obj) = java_thread_obj {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, park_state);
        }
    }

    /// BUG-03 — publish the address of this thread's `JvmThread`'s
    /// [`cratonvm_gc::Tlab`]. Called once by the thread itself just after it
    /// starts running (the TLAB lives in the `JvmThread`, whose address is
    /// stable for the thread's life). No-op for an unknown id.
    pub fn set_tlab_addr(&self, thread_id: ThreadId, tlab_addr: usize) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(tlab_addr, Ordering::Release);
        }
    }

    /// zt t26/zc5b item 1: the `[tlab, tlab + 24)` span -- the `cursor`, `end`
    /// and `start` words (`cratonvm_gc::Tlab`'s JIT-contract offsets 0, 8 and
    /// 16) -- of every thread whose TLAB address is published. These words
    /// live in the thread's `JvmThread`, i.e. on its own stack, and are never
    /// Java references; the take-over and helper-window band scans skip them
    /// (`xt_root_scan::is_tlab_pointer_slot`). Unsorted; one registry read
    /// lock. A thread that publishes after this call simply has no span this
    /// pause (its words are probed, as before).
    pub fn tlab_pointer_slots(&self) -> Vec<(usize, usize)> {
        const TLAB_POINTER_WORDS: usize = 3;
        let threads = self.threads.read();
        threads
            .values()
            .map(|entry| entry.tlab_addr.load(Ordering::Acquire))
            .filter(|&addr| addr != 0)
            .map(|addr| (addr, addr + TLAB_POINTER_WORDS * 8))
            .collect()
    }

    /// BUG-03 — clear this thread's published TLAB address. MUST be called by
    /// the thread (or its teardown) before its `JvmThread` is dropped so the
    /// collector never dereferences a dangling TLAB pointer.
    pub fn clear_tlab_addr(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(0, Ordering::Release);
            // XT-FRAME-SCAN: the JvmThread address shares the TLAB address's
            // exact lifecycle (published once the JvmThread's address is
            // final, must stop being visible before it drops) — clear both
            // under the same lock hold so no reader can observe a cleared
            // TLAB with a still-published thread address or vice versa.
            entry.jvm_thread_addr.store(0, Ordering::Release);
        }
    }

    /// XT-FRAME-SCAN: publish the owning thread's `JvmThread` address so the
    /// cross-thread STW takeover can walk a frozen peer's interpreter frames
    /// (locals + operand stacks) directly. Same publication discipline as
    /// [`Self::set_tlab_addr`]: called once after the `JvmThread`'s address
    /// is final (worker spawn body / primordial boot / JNI attach); cleared
    /// by [`Self::clear_tlab_addr`] before the `JvmThread` drops. No-op for
    /// an unknown id.
    pub fn set_jvm_thread_addr(&self, thread_id: ThreadId, addr: usize) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.jvm_thread_addr.store(addr, Ordering::Release);
        }
    }

    /// gen r5w2/roots6: record whether `thread_id`'s Java execution is on a
    /// machine stack (see `ThreadEntry::mounted`). A virtual thread calls it
    /// with `false` when its continuation yields and `true` at every mount;
    /// nothing else needs to (entries start mounted). No-op for an unknown id.
    pub fn set_mounted(&self, thread_id: ThreadId, mounted: bool) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            // gci i12/vp: retire the parked fold filter's epoch BEFORE the bit
            // moves (and so before a remount's drain or the owner's next
            // deposit can change a word the filter summarises).
            entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
            entry.mounted.store(mounted, Ordering::Release);
        }
    }

    /// Is `thread_id` mounted (see [`Self::set_mounted`])? `false` for an
    /// unknown id.
    pub fn is_mounted(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .is_some_and(|e| e.mounted.load(Ordering::Acquire))
    }

    /// XT-FRAME-SCAN: the published `JvmThread` addresses of every alive
    /// thread whose OS tid is in `os_tids` (the takeover's frozen-peer set).
    ///
    /// SAFETY contract for the caller's deref: entries must only be
    /// dereferenced for peers that are OS-frozen by the takeover (suspended
    /// with `Rip` inside registered compiled code) and stay frozen until
    /// after the deref ends — compiled code never mutates the `JvmThread`'s
    /// interpreter state, and a frozen thread cannot exit (so `alive` cannot
    /// flip nor the `JvmThread` drop while frozen).
    pub fn frozen_peer_thread_addrs(&self, os_tids: &[u32]) -> Vec<usize> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        // gcf f1/r: membership by binary search over a sorted copy, not a
        // linear `contains` per registry entry (`O(threads * frozen)` inside
        // the pause). Same answer.
        let mut os_tids = os_tids.to_vec();
        os_tids.sort_unstable();
        let threads = self.threads.read();
        let mut out = Vec::new();
        for entry in threads.values() {
            // gen r5w2/roots6: an unmounted virtual thread is not frozen on
            // its last carrier (its frames are on the heap and that carrier
            // may be mounting it right now), and a dropped `JvmThread` must
            // not be read at all.
            if !entry_is_stw_live(entry)
                || !entry.mounted.load(Ordering::Acquire)
                || entry.gc_block_state.owner_dropped()
            {
                continue;
            }
            let tid = entry.os_tid.load(Ordering::Acquire);
            if tid == 0 || os_tids.binary_search(&tid).is_err() {
                continue;
            }
            let addr = entry.jvm_thread_addr.load(Ordering::Acquire);
            if addr != 0 {
                out.push(addr);
            }
        }
        out
    }

    /// Peers whose precise deposits this pause holds until rewrite ends.
    pub(crate) fn precise_lazy_claimed_thread_addrs(&self) -> Vec<usize> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                entry_is_stw_live(e)
                    && e.mounted.load(Ordering::Acquire)
                    && !e.gc_block_state.owner_dropped()
                    && matches!(
                        e.gc_block_state.lazy_native.load(Ordering::Acquire),
                        crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED
                            | crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED_LEAVING
                    )
                    && e.gc_block_state
                        .lazy_native_precise_claim
                        .load(Ordering::Acquire)
            })
            .map(|e| e.jvm_thread_addr.load(Ordering::Acquire))
            .filter(|a| *a != 0)
            .collect()
    }

    /// Claim every live lazy FFM downcall peer whose OS thread is not already
    /// frozen or claimed by this take-over. The claim holds its Java frames
    /// stable so collection roots or frame traces can be published on its behalf.
    /// Number of threads still in a lazy FFM downcall. No-roots pauses that
    /// cannot claim must wait for these peers to return before walking the heap.
    pub(crate) fn lazy_native_population_count(&self) -> usize {
        self.threads
            .read()
            .values()
            .filter(|entry| {
                entry.gc_block_state.lazy_native.load(Ordering::Acquire)
                    == crate::threading::jvm_thread::LAZY_NATIVE_IN
            })
            .count()
    }

    pub(crate) fn claim_lazy_native_peers(&self, already: &[u32]) -> Vec<LazyNativeClaim> {
        let threads = self.threads.read();
        let mut out: Vec<LazyNativeClaim> = Vec::new();
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            if block.lazy_native.load(Ordering::Acquire)
                != crate::threading::jvm_thread::LAZY_NATIVE_IN
            {
                continue;
            }
            if !entry_is_stw_live(entry)
                || !entry.mounted.load(Ordering::Acquire)
                || block.owner_dropped()
                || entry.jvm_thread_addr.load(Ordering::Acquire) == 0
            {
                continue;
            }
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            if os_tid == 0 || already.contains(&os_tid) || out.iter().any(|c| c.os_tid == os_tid) {
                continue;
            }
            if let Some((lo, hi)) = block.try_claim_lazy_native() {
                out.push(LazyNativeClaim {
                    os_tid,
                    lo,
                    hi,
                    state: Arc::clone(block),
                });
            }
        }
        out
    }

    /// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`,
    /// `gcg-g4jn-proposal-in-native-band-scanned-at-the-pause-DONE-20261015`): the
    /// pause's half of a JNI in-native thread's compiled band. For every alive
    /// entry whose thread is in a blocked region with a band recorded
    /// ([`in_native_band_of`]), push every word of
    /// `[lo, hi)` that `probe` accepts onto `out`. Each band is read once (two
    /// entries on one band -- never expected -- are read once).
    ///
    /// Only inside a pause: there the thread cannot leave native
    /// (`GcBarrier::leave_in_native_if` waits the pause out), so its frames
    /// above the C code are frozen and its stack stays mapped (it cannot exit);
    /// why the band is complete is on `native::jni::in_native_band_at_pause`.
    /// The registry read lock is held for the whole scan, as
    /// [`Self::collect_all_root_snapshots`] holds it.
    pub(crate) fn scan_in_native_bands(
        &self,
        probe: &dyn Fn(usize) -> Option<ObjectRef>,
        out: &mut Vec<ObjectRef>,
    ) -> InNativeBandScan {
        let threads = self.threads.read();
        let mut scan = InNativeBandScan::default();
        // gci i30/gv (`i28-nb-report.md` review item 2): a hash set, not a
        // `Vec` searched per entry (O(threads^2) compares per pause with a
        // few hundred in-native threads). Empty sets do not allocate.
        // addr-keyed: not a heap address -- in-native band stack bounds, local to one pause scan.
        let mut seen: rustc_hash::FxHashSet<(usize, usize)> = rustc_hash::FxHashSet::default();
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            let Some((lo, hi)) = in_native_band_of(block) else {
                continue;
            };
            if !entry.alive.load(Ordering::Acquire) || block.owner_dropped() || !seen.insert((lo, hi)) {
                continue;
            }
            // SAFETY: the band was recorded by its thread for the in-native
            // window it is in now (flag and mark up); inside a pause that
            // window cannot end, so `[lo, hi)` is that thread's mapped stack
            // (the doc above).
            let found = unsafe { scan_in_native_band_words(lo, hi, probe, out) };
            scan.threads += 1;
            scan.words += (hi - lo) / 8;
            scan.roots += found;
        }
        scan
    }

    /// gci i18/dc (`gci-i3hd-heap-walks-and-diagnostics-review-findings` item
    /// 5, the in-native band's half): [`Self::scan_in_native_bands`], the same
    /// entries, bands and contract, but each band's roots are handed to `each`
    /// with the thread that owns the band, so the heap dump names that
    /// thread's serial on them (it named serial 1). `each` runs under the
    /// registry read lock: it must not touch the registry. Cold: dumps only.
    #[cold]
    pub(crate) fn scan_in_native_bands_by_owner(
        &self,
        probe: &dyn Fn(usize) -> Option<ObjectRef>,
        each: &mut dyn FnMut(ThreadId, Vec<ObjectRef>),
    ) {
        let threads = self.threads.read();
        // gci i30/gv: a hash set, as in `scan_in_native_bands`.
        // addr-keyed: not a heap address -- in-native band stack bounds, local to one pause scan.
        let mut seen: rustc_hash::FxHashSet<(usize, usize)> = rustc_hash::FxHashSet::default();
        for (tid, entry) in threads.iter() {
            let block = &entry.gc_block_state;
            let Some((lo, hi)) = in_native_band_of(block) else {
                continue;
            };
            if !entry.alive.load(Ordering::Acquire) || block.owner_dropped() || !seen.insert((lo, hi)) {
                continue;
            }
            let mut found = Vec::new();
            // SAFETY: as in `scan_in_native_bands` (inside a pause the
            // in-native window cannot end, so the band stays mapped).
            let _ = unsafe { scan_in_native_band_words(lo, hi, probe, &mut found) };
            each(*tid, found);
        }
    }

    /// gcj j2/jn2 (cut 2 of
    /// `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts-20261019`,
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`): retire, on its owner's behalf,
    /// the TLAB of every JNI in-native thread that went into native keeping it
    /// (`GcBlockState::in_native_tlab_kept`), as HotSpot's
    /// `ensure_parsability` retires every thread's TLAB at the safepoint.
    /// Returns how many it retired.
    ///
    /// Only inside a collecting pause, after its barrier is satisfied
    /// (`gc_and_alloc::retire_in_native_kept_tlabs`). Why the owner cannot
    /// touch its `Tlab` meanwhile: the mark is raised while the owner is
    /// still a counted mutator, before `in_blocked_region` goes up (Release,
    /// read here with Acquire), and the owner leaves native only through
    /// `GcBarrier::leave_in_native_if`, which waits out the pause in progress
    /// before it drops the flag; the C code in between never allocates (every
    /// JNIEnv function leaves native first, and a leaf window does not
    /// allocate). The swap decides the one retirer: a thread whose flag is down
    /// keeps its mark for its own leave to clear. The registry read lock is
    /// held for the walk, as [`Self::collect_reserved_tlab_tails`] holds it.
    pub(crate) fn retire_in_native_kept_tlabs(&self) -> usize {
        let threads = self.threads.read();
        let mut retired = 0usize;
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            if !entry_is_stw_live(entry) || block.owner_dropped() {
                continue;
            }
            if !block.in_blocked_region.load(Ordering::Acquire) {
                continue;
            }
            if !block.in_native_tlab_kept.swap(false, Ordering::AcqRel) {
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                continue;
            }
            // SAFETY: `addr` is the live owner's own `Tlab` (published by it,
            // address-stable for its life, cleared before its `JvmThread` is
            // dropped; `entry_is_stw_live` and `owner_dropped` screened the
            // unwound and the dropped). The owner is in native and cannot
            // leave while this pause holds the barrier (the doc above), so
            // nothing else reads or writes the `Tlab` during this call, and the
            // swap above made this the only retirer.
            unsafe { (*(addr as *mut cratonvm_gc::Tlab)).retire() };
            retired += 1;
        }
        retired
    }

    /// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in;
    /// `gch-h4bp-review-findings-RETIRED-UNFIXED-20261018` item 1): credit, for the pause
    /// stamped `stamp` (`gc_quiescence::young_pin_pause_stamp`, 0 = none:
    /// nothing is credited), every JNI in-native band whose deposit PROVED its
    /// compiled chain (`GcBlockState::in_native_proven_jit_depth`). For each,
    /// `credit(words, depth)` receives the band's words that `young` accepts
    /// (read raw, volatile: interior and derived words as they are) and the
    /// proven depth, and the band is marked credited for `stamp`
    /// (`GcBlockState::in_native_credit_stamp`).
    ///
    /// A band is credited only when [`in_native_credit_admissible`] holds: a
    /// proven depth equal to the depth its OS thread published (`depth_of`,
    /// `gc_quiescence::jit_depth_of_tid` in production -- the coverage account
    /// subtracts published depth, so crediting more would be the one unsound
    /// direction), and the first band of its OS thread and of its address
    /// range (a second entry on one OS tid or one band would credit the same
    /// frames twice). Every other band stays uncredited, and the pause's band
    /// scan then marks the cycle as before (`gc_and_alloc::append_in_native_band_roots`).
    /// A band already credited for `stamp` (a second root gathering in the same
    /// pause) is skipped: each band is credited once per pause.
    ///
    /// Same entries, contract and lock as [`Self::scan_in_native_bands`]: only
    /// inside a pause, where the band's thread cannot leave native, so the band
    /// is frozen and mapped.
    pub(crate) fn credit_in_native_proven_bands(
        &self,
        stamp: u64,
        depth_of: &dyn Fn(u32) -> Option<usize>,
        young: &dyn Fn(usize) -> bool,
        credit: &mut dyn FnMut(&[usize], usize),
    ) -> InNativeCredit {
        let mut out = InNativeCredit::default();
        if stamp == 0 {
            return out;
        }
        let threads = self.threads.read();
        let mut seen_bands: Vec<(usize, usize)> = Vec::new();
        let mut seen_tids: Vec<u32> = Vec::new();
        let mut words: Vec<usize> = Vec::new();
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            let Some((lo, hi)) = in_native_band_of(block) else {
                continue;
            };
            if !entry.alive.load(Ordering::Acquire) || block.owner_dropped() {
                continue;
            }
            if block.in_native_credit_stamp.load(Ordering::Acquire) == stamp {
                out.already += 1;
                continue;
            }
            // Only a thread the pause's blocked roster EXCLUDES (in C, with
            // its foreign-native mark up and the registry's exemption on):
            // any other blocked thread is read by the helper-window pass,
            // whose pinned credit (`xt_cycle_pinned_jit_depth`) could count
            // the same depth twice. The mark only falls during a pause (a
            // leave waiting it out), so a thread the take-over read as a
            // window, before this runs, is never credited here.
            if !(self.ffm_in_native_no_signal && ffm_in_native_signal_exempt(block)) {
                out.not_exempt += 1;
                continue;
            }
            let depth = block.in_native_proven_jit_depth.load(Ordering::Acquire);
            if depth == 0 {
                out.unproven += 1;
                continue;
            }
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let duplicate = seen_tids.contains(&os_tid) || seen_bands.contains(&(lo, hi));
            if !in_native_credit_admissible(depth, os_tid, depth_of(os_tid), duplicate) {
                out.mismatched += 1;
                continue;
            }
            seen_tids.push(os_tid);
            seen_bands.push((lo, hi));
            words.clear();
            // SAFETY: as in `scan_in_native_bands` (inside a pause the
            // in-native window cannot end, so the band stays mapped).
            unsafe { in_native_band_words_where(lo, hi, young, &mut words) };
            credit(&words[..], depth);
            block.in_native_credit_stamp.store(stamp, Ordering::Release);
            out.credited += 1;
            out.depth += depth;
            out.words += words.len();
        }
        out
    }

    /// gci i24/bp: how many in-native bands a pause reads
    /// ([`Self::scan_in_native_bands`]' entries) were NOT credited for `stamp`
    /// by [`Self::credit_in_native_proven_bands`]. `stamp == 0` counts every
    /// band. Same contract and lock.
    pub(crate) fn uncredited_in_native_bands(&self, stamp: u64) -> usize {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                let block = &e.gc_block_state;
                in_native_band_of(block).is_some()
                    && e.alive.load(Ordering::Acquire)
                    && !block.owner_dropped()
                    && (stamp == 0 || block.in_native_credit_stamp.load(Ordering::Acquire) != stamp)
            })
            .count()
    }

    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-4): which of `os_tids`
    /// (peers a take-over just froze or claimed) belong to an alive entry
    /// that is in a blocked region now. Such a peer was never in the pause's
    /// `expected`, or has already arrived through its region's entry, so the
    /// barrier must not excuse it (`gc_and_alloc::stw_take_over_and_wait`).
    ///
    /// An OS tid is answered only when EVERY live entry published on it is
    /// blocked: a mounted virtual thread shares its carrier's tid, and the
    /// carrier's own entry may sit in a blocked region while the mounted
    /// thread runs (and is the one frozen or claimed); an entry that is not
    /// blocked keeps the tid excusable, as before this rule. One registry
    /// read lock.
    ///
    /// gcj j2/cm2 (`gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
    /// item 6): sorted lookups, not a `contains` per entry and a linear `find`
    /// per match -- that was O(entries x `os_tids`) inside the pause, ~0.5 M
    /// compares a round at 750 frozen threads. Same set, answered in
    /// ascending tid order, each tid once.
    pub(crate) fn blocked_among_os_tids(&self, os_tids: &[u32]) -> Vec<u32> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        let mut wanted = os_tids.to_vec();
        wanted.sort_unstable();
        // (tid, this live entry is blocked), one row per matching entry.
        let mut rows: Vec<(u32, bool)> = Vec::new();
        {
            let threads = self.threads.read();
            for entry in threads.values() {
                if !entry_is_stw_live(entry) {
                    continue;
                }
                let tid = entry.os_tid.load(Ordering::Acquire);
                if tid == 0 || wanted.binary_search(&tid).is_err() {
                    continue;
                }
                let blocked = entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire);
                rows.push((tid, blocked));
            }
        }
        rows.sort_unstable();
        let mut out: Vec<u32> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let tid = rows[i].0;
            let mut all = true;
            while i < rows.len() && rows[i].0 == tid {
                all &= rows[i].1;
                i += 1;
            }
            if all {
                out.push(tid);
            }
        }
        out
    }

    /// Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`):
    /// excuse, for the handshake pause whose census runs now, every COUNTED
    /// thread found running a native through the funnel
    /// (`jvm_thread::NativeExcuse::try_excuse`; with `need_listable`, only one
    /// whose frames below the native another thread may list). Under the
    /// barrier lock, inside the request's census closure, with the same
    /// counted test as the census (alive, `stw_ready`, not
    /// `in_blocked_region`) and never the initiator: not `initiator`'s entry,
    /// nor one published on the calling OS thread (`me`, 0 = unknown).
    ///
    /// An excused thread may keep running its native's Rust code, but it
    /// cannot return from it, run Java or move its frames until the pause
    /// releases it, and none of its arrivals counts in this pause
    /// (`GcBarrier`'s `excused_running`) -- so the pause neither waits for it
    /// nor can be released early by it.
    pub(crate) fn excuse_running_natives(
        &self,
        initiator: ThreadId,
        need_listable: bool,
    ) -> Vec<ExcusedNative> {
        self.excuse_running_natives_on(initiator, current_os_tid(), need_listable)
    }

    /// [`Self::excuse_running_natives`] with the calling OS thread's id
    /// supplied (a test places the "caller" on any OS thread).
    pub(in crate::threading::thread_registry) fn excuse_running_natives_on(
        &self,
        initiator: ThreadId,
        me: u32,
        need_listable: bool,
    ) -> Vec<ExcusedNative> {
        let threads = self.threads.read();
        let mut out = Vec::new();
        for (tid, e) in threads.iter() {
            let state = &e.gc_block_state;
            // The common case, one load: not in an excusable native.
            if state.native_excuse.load() & crate::threading::jvm_thread::NX_IN == 0 {
                continue;
            }
            if *tid == initiator
                || !entry_is_stw_live(e)
                || !e.stw_ready.load(Ordering::Acquire)
                || !e.mounted.load(Ordering::Acquire)
                || state.owner_dropped()
                || state.in_blocked_region.load(Ordering::Acquire)
            {
                continue;
            }
            let os_tid = e.os_tid.load(Ordering::Acquire);
            if me != 0 && os_tid == me {
                continue;
            }
            if let Some(word) = state.native_excuse.try_excuse(need_listable) {
                out.push(ExcusedNative {
                    tid: *tid,
                    os_tid,
                    word,
                    state: Arc::clone(state),
                });
            }
        }
        out
    }

    /// The frame trace of an excused thread, built by the pause that excused
    /// it (interpreter round i2 wave 58, lane L6): stored where the thread's
    /// own publish would have put it (`JvmThread::frame_trace`, shared with
    /// the entry).
    pub(crate) fn store_frame_trace(
        &self,
        thread_id: ThreadId,
        trace: Vec<cratonvm_native_api::StackTraceEntry>,
    ) {
        if let Some(e) = self.threads.read().get(&thread_id) {
            *e.frame_trace.lock() = trace;
            // W6-F fix 4: not a live publish's trace (`live_trace_locks`).
            e.jmx_monitors.trace_seq.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// gci i28/tl: every STW-live thread's TLAB cycle counters and running
    /// allocated bytes, keyed by its thread id, for the
    /// `-Xlog:gc+tlab=debug` `TLAB totals` line
    /// (`unified_logging::stash_tlab_totals`). Read-only: the counters are
    /// running totals the collector differences, it never resets another
    /// thread's buffer.
    ///
    /// SAFETY: the call-time contract of [`Self::collect_reserved_tlab_tails`]
    /// (the world is stopped; the liveness screen is the same), whose reads
    /// this mirrors: a handful of plain field loads per thread.
    pub fn tlab_cycle_snapshots(&self) -> Vec<cratonvm_gc::tlab::TlabCycleSnapshot> {
        let threads = self.threads.read();
        let mut out = Vec::with_capacity(threads.len());
        for (id, entry) in threads.iter() {
            if !entry_is_stw_live(entry) || entry.gc_block_state.owner_dropped() {
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                continue;
            }
            // SAFETY: see the method contract.
            let tlab = unsafe { &*(addr as *const cratonvm_gc::Tlab) };
            // gci i43/tj: keyed by the never-reused thread id, not the
            // address (`TlabCycleSnapshot`), so `TlabCycleMark::since` needs
            // no plausibility test.
            let key = usize::try_from(id.0).unwrap_or(usize::MAX);
            out.push((key, tlab.cycle_mark(), tlab.thread_allocated_bytes()));
        }
        out
    }

    /// BUG-03 — collect the reserved (un-retired) TLAB tails of every alive
    /// thread, as absolute `(cursor, end)` pairs.
    ///
    /// Called by the multi-threaded GC initiator AFTER the STW barrier is
    /// satisfied (so no counted peer is running Java): at that point normal
    /// parked or blocked peers should already have retired their TLAB
    /// (`reserved_tail` -> `None`), but collecting every alive thread makes the
    /// collector robust if a blocked or tearing-down path missed that retire.
    /// The non-empty tails are the regions the non-moving young sweep must skip
    /// (see
    /// [`cratonvm_gc::vm_heap::VmHeap::set_jit_tlab_skip_regions`]).
    ///
    /// SAFETY: reads each alive thread's `Tlab` through its published address.
    /// Per the call-time invariant above, no alive thread is mutating its TLAB
    /// (all are parked / blocked / OS-suspended), and an alive thread's
    /// `JvmThread` cannot be concurrently dropped (teardown clears the address
    /// and flips `alive` first). Dead / un-published entries are skipped.
    pub fn collect_reserved_tlab_tails(&self) -> Vec<(usize, usize)> {
        let threads = self.threads.read();
        let mut out = Vec::new();
        // Census counters for the `CRATONVM_G1_DBG_REACH` line below. An empty
        // result has three very different causes and the G1 walk-break reports
        // ("ZERO published skip spans this pause") cannot tell them apart:
        // nobody is registered, everybody is registered but retired, or this
        // function was never reached at all. Counting the population separates
        // the first two, and the absence of the line entirely settles the third.
        let (mut total, mut dead, mut unregistered, mut retired) = (0usize, 0, 0, 0);
        for entry in threads.values() {
            total += 1;
            // `entry_is_stw_live`, not `alive`: a carrier that unwound past
            // `mark_dead` left `tlab_addr` pointing into its dropped stack.
            // gen r5w2/roots6: and one whose `JvmThread` has been DROPPED
            // (`GcBlockState::owner_dropped`) is skipped before its OS thread
            // finishes too -- the unwind window `entry_is_stw_live` cannot see.
            if !entry_is_stw_live(entry) || entry.gc_block_state.owner_dropped() {
                dead += 1;
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                unregistered += 1;
                continue;
            }
            // SAFETY: see method contract — the owning thread is parked /
            // blocked / OS-suspended, so the `Tlab` at `addr` is live and not
            // being mutated.
            let tlab = unsafe { &*(addr as *const cratonvm_gc::Tlab) };
            match tlab.reserved_tail() {
                Some(tail) => {
                    // Paired with `CRATONVM_DBG_WATCH_ALLOC_CID` /
                    // `CRATONVM_DBG_WATCH_HOLDER`: a published "un-allocated
                    // tail" that CONTAINS a live object is not a conservative
                    // degrade -- every linear walk in the sweep skips the span,
                    // and the mark oracle then answers "gap space, not an
                    // object" for any root pointing into it, so the object is
                    // neither scanned nor swept and everything it references is
                    // reclaimed. Name the owner and its cursor when it happens.
                    let watch = cratonvm_gc::heap::young_mark_watch();
                    if watch != 0 && watch >= tail.0 && watch < tail.1 {
                        eprintln!(
                            "[TLAB-SKIP] published tail [{:#x},{:#x}) CONTAINS watch {watch:#x} — owner java_tid={:?} name={} alive={} tlab_addr={addr:#x}",
                            tail.0,
                            tail.1,
                            entry.java_tid,
                            entry.name,
                            entry.alive.load(Ordering::Acquire),
                        );
                    }
                    out.push(tail)
                }
                None => retired += 1,
            }
        }
        if cratonvm_types::flags().gc.g1_dbg_reach {
            // Sequence number, not a pause id: the walk-break reports carry no
            // pause identity either, so the only thing that can be compared is
            // "how many times did the publish path run" against "how many
            // breaks reported ZERO published skip spans". The first run
            // produced exactly ONE census line across 453 s and four
            // zero-span breaks, which is the fact this counter is here to
            // confirm or kill.
            static CENSUS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = CENSUS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "[g1][TLAB-CENSUS] #{seq} entries={total} dead={dead} \
                 alive_no_tlab_addr={unregistered} alive_retired={retired} published={}",
                out.len()
            );
        }
        out
    }

    /// INT-3 — the deposited root-snapshot contents of every alive thread
    /// whose OS tid is in `os_tids`.
    ///
    /// Used by the G1 cross-thread STW JIT takeover: a forcibly-frozen peer
    /// is excused from the barrier, so it never applies the collection's
    /// pointer map to its own frames — its deposit snapshot is the only view
    /// of what those frames reference. Keeping the snapshot alive (it is
    /// already merged into the collection roots) is not enough under an
    /// evacuating collector: the objects must also NOT MOVE, so the caller
    /// pins each returned address's region out of the CSet (see
    /// `cratonvm_gc::gc_quiescence::add_pinned_jit_root`). Not needed on the
    /// Generational backend, whose frozen-peer cycles run the fully
    /// non-moving sweep.
    pub fn root_snapshots_for_os_tids(&self, os_tids: &[u32]) -> Vec<ObjectRef> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        let threads = self.threads.read();
        let mut out = Vec::new();
        for entry in threads.values() {
            if !entry_is_stw_live(entry) {
                continue;
            }
            let tid = entry.os_tid.load(Ordering::Acquire);
            if tid != 0 && os_tids.contains(&tid) {
                out.extend(entry.root_snapshot.lock().iter().copied());
            }
        }
        out
    }

    /// T19.K1 — Update the daemon flag for an already-registered thread.
    ///
    /// `Thread.setDaemon(boolean)` may be called between construction
    /// and `start()`; once `start()` is invoked the JLS forbids further
    /// changes (an `IllegalThreadStateException` is thrown by the JDK
    /// bytecode). We don't enforce that gate here — callers (the
    /// native bridge) are responsible — we just store the new value.
    /// Returns `true` if the thread was found, `false` if the id was
    /// unknown.
    pub fn set_daemon(&self, thread_id: ThreadId, daemon: bool) -> bool {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            let was = entry.daemon.swap(daemon, Ordering::AcqRel);
            // zt t15/pl6: a listed entry's flip moves the roster's daemons.
            // Every edge that changes whether an entry is listed is made
            // under the `threads` write lock, which this read excludes.
            if was != daemon && Self::jmm_roster_lists(entry) {
                let delta = if daemon { 1 } else { -1 };
                self.jmm_roster_daemon.fetch_add(delta, Ordering::AcqRel);
            }
            true
        } else {
            false
        }
    }

    /// T19.K1 — Read the daemon flag for a thread. Returns `false` for
    /// unknown thread ids (matches `is_alive`'s "missing means absent"
    /// convention so callers can treat unknown ids as already-gone
    /// non-daemon threads — the safe default).
    pub fn is_daemon(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.daemon.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// T19.H1 watchdog — unpark every registered thread so threads parked
    /// in `LockSupport.park` (e.g. AQS lock/latch waiters) wake, return
    /// through the interpreter top-of-loop poll, and emit their stack dump.
    /// Diagnostic-only: a spurious unpark is spec-legal (`park` may return
    /// spuriously) and the process aborts immediately after the dump, so the
    /// extra permits never affect correctness.
    pub fn unpark_all_for_stack_dump(&self) {
        // Interpreter round i4 wave 91, lane L6 (page
        // `i77-L6-proposal-every-vm-unpark-through-the-shape`): the VM's
        // unpark shape, as `Unsafe.unpark` gives it.
        let shape = self.unpark_shape();
        let threads = self.threads.read();
        for entry in threads.values() {
            entry.park_state.unpark_with(shape);
        }
    }

    /// Legend printed under the T19.H1 thread-summary header, explaining what
    /// the `deposit=` column means. See [`Self::deposit_freshness`] for why
    /// this exists at all.
    pub(crate) const DEPOSIT_LEGEND: &'static str = concat!(
        "  legend: deposit=live        — parked in a blocking native; blocked=/roots=/top= ARE its current state.\n",
        "          deposit=STALE       — RUNNING right now. top=/roots= are whatever it deposited at its\n",
        "                                LAST blocking call and can be arbitrarily old — read the live\n",
        "                                \"T19.H1 stack dump\" above for where this thread actually is,\n",
        "                                UNLESS the row says no-live-dump (see below).\n",
        "          no-live-dump        — RUNNING, and it produced no live dump: it never reached an\n",
        "                                interpreter dispatch point during the watchdog's grace period.\n",
        "                                That means JIT-COMPILED code or a long native call — NOT a\n",
        "                                deadlock, and NOT evidence about which. Re-run with --nojit:\n",
        "                                if the live dump then appears, it was compiled code.\n",
        "          deposit=post-mortem — dead. alive=false is authoritative; blocked=/roots=/top= are residue from\n",
        "                                its termination sequence (nothing clears them), NOT evidence of a wait."
    );

    /// How current a registry entry's *deposited* state — the `blocked=`,
    /// `roots=` and frame-chain columns of the T19.H1 summary — actually is.
    ///
    /// A thread deposits its root snapshot and frame chain when it is about to
    /// block, and **nothing clears either afterwards**: not on wake, not on
    /// death. So the deposit is a faithful readout of the thread's position in
    /// exactly one case — it is alive AND still inside a blocked region.
    /// Otherwise it is history:
    ///
    /// * `STALE` (alive, not blocked): the thread is executing bytecode. The
    ///   chain names its last *blocking* call, which may be seconds or minutes
    ///   old and in a completely different part of the program.
    /// * `post-mortem` (dead): `blocked=true` here is the terminal
    ///   `deposit_root_snapshot()` in the thread-exit sequence, which raises
    ///   `in_blocked_region` and has no counterpart to lower it. Every STW
    ///   census filters on `alive` before reading that flag
    ///   (`alive_count_blocked_and_os_tids_inner`, `blocked_os_tids`,
    ///   `fold_pointer_map_into_blocked`), so the residue is inert to the
    ///   barrier — it is purely a reporting artifact.
    ///
    /// Labelling this is not cosmetic. Three separate investigations have now
    /// been sent the wrong way by an unmarked stale deposit:
    /// `onclasscondition-join-never-returns-20260801` (read `alive=false` on
    /// dead entries as a missed wakeup), the Elasticsearch
    /// `ES-HANG-20260719-threadjoin` capture (same shape), and
    /// `bug-h2-teststringcache-thread-join-blocked-on-dead-threads-20260807`,
    /// where `main` was `alive=true blocked=false` — running
    /// `TestStringCache.runBenchmark()` — while its unmarked deposit still read
    /// `java/lang/Thread.join@129` from a join that had returned ~40 s earlier.
    /// The live stack dump printed directly above said `runBenchmark`; nothing
    /// flagged the contradiction, and the whole doc was written against the
    /// stale line.
    pub(crate) fn deposit_freshness(alive: bool, blocked: bool) -> &'static str {
        match (alive, blocked) {
            (false, _) => "post-mortem",
            (true, true) => "live",
            (true, false) => "STALE",
        }
    }

    /// T19.H1 watchdog — write [`Self::render_thread_summary_for`] to stderr.
    ///
    /// `live_dumped` is the set of thread ids that answered the watchdog with a
    /// live frame dump. See [`Self::render_thread_summary_for`].
    pub fn dump_thread_summary_to_stderr(&self, live_dumped: &[u64]) {
        use std::io::Write;
        let text = self.render_thread_summary_for(live_dumped);
        let stderr = std::io::stderr();
        let mut h = stderr.lock();
        let _ = h.write_all(text.as_bytes());
        let _ = h.flush();
    }

    /// [`Self::render_thread_summary_for`] with no live-dump information —
    /// every RUNNING thread is then reported as `no-live-dump`, which is the
    /// safe reading when the caller cannot say. Test-only since JIT round 22
    /// wave 7 (lane review): no caller outside the tests was ever added.
    #[cfg(test)]
    pub fn render_thread_summary(&self) -> String {
        self.render_thread_summary_for(&[])
    }

    /// T19.H1 watchdog — a one-line summary of every registered thread (name,
    /// liveness, daemon, deposited-root count), plus the complete deposited
    /// frame chain of every alive thread. Lets the watchdog show threads that
    /// have NO dumpable interpreter frames — e.g. a thread blocked in a
    /// Rust-level native lock, or one that never started — which the frame-dump
    /// path cannot surface. A non-zero `roots` on an otherwise-silent thread
    /// means it deposited roots before blocking (it IS blocked in a native),
    /// distinguishing "blocked in native" from "never ran".
    ///
    /// Every row carries a `deposit=` freshness tag; see
    /// [`Self::deposit_freshness`] for what each value licenses you to
    /// conclude, and why an untagged chain has repeatedly been misread as a
    /// live wait site.
    ///
    /// `live_dumped` lists the thread ids that produced a live frame dump in
    /// response to this watchdog cycle. It is what separates the two very
    /// different states that both render as `blocked=false`:
    ///
    ///  * a RUNNING thread that DID dump — the live dump above is where it is;
    ///  * a RUNNING thread that did NOT — it never reached an interpreter
    ///    dispatch point, i.e. it is in JIT-compiled code or a long native
    ///    call. The summary used to point every such row at a
    ///    `"T19.H1 stack dump: tid=N"` section that was never emitted, and the
    ///    absence of that section then read as "blocked somewhere the watchdog
    ///    cannot reach". It cost
    ///    `known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter`
    ///    a whole investigation: the thread was neither blocked nor in native
    ///    code, it was spinning in a compiled `ByteBuf.writeByte` loop, and
    ///    `--nojit` produced the full 73-frame dump immediately.
    ///
    /// Returned as a `String` (rather than written straight out) so the format
    /// is unit-testable — the tags below are the load-bearing part.
    pub fn render_thread_summary_for(&self, live_dumped: &[u64]) -> String {
        use std::fmt::Write as _;
        let threads = self.threads.read();
        let mut h = String::new();
        let _ = writeln!(
            h,
            "--- T19.H1 thread summary: {} registered thread(s) ---",
            threads.len()
        );
        let _ = writeln!(h, "{}", Self::DEPOSIT_LEGEND);
        struct SummaryRow {
            tid: u64,
            os_tid: u64,
            name: String,
            alive: bool,
            daemon: bool,
            blocked: bool,
            roots: usize,
            state: String,
            top: String,
            full_frames: Vec<String>,
        }
        let mut rows: Vec<SummaryRow> = threads
            .iter()
            .map(|(tid, e)| {
                let state = {
                    let s = e.vm_state.lock();
                    if s.is_empty() {
                        "<unset>".to_string()
                    } else {
                        s.clone()
                    }
                };
                let trace = e.frame_trace.lock();
                let mut top = String::new();
                for (i, frame) in trace.iter().rev().take(3).enumerate() {
                    if i > 0 {
                        top.push_str(" <- ");
                    }
                    top.push_str(&format!(
                        "{}.{}@{}",
                        frame.class_name, frame.method_name, frame.byte_code_index
                    ));
                }
                if top.is_empty() {
                    top.push_str("<no-frame-trace>");
                }
                // 2026-08-03 (onclasscondition-join-never-returns) — the
                // one-line `top` above caps at 3 frames, which is enough to
                // name a wait site but not to tell "this thread is a live
                // participant mid-call-chain" from "this is a stale dead
                // entry that happens to share a wait site with hundreds of
                // others from earlier per-test JVM reboots". Every alive
                // thread's COMPLETE deposited chain, oldest frame first (same
                // order `dump_current_thread_frames` uses), so a hang repro
                // can be told apart from the accumulated dead-thread noise
                // without a second run.
                let full_frames = if e.alive.load(std::sync::atomic::Ordering::Acquire) {
                    trace
                        .iter()
                        .map(|frame| {
                            format!(
                                "{}.{}@{}",
                                frame.class_name, frame.method_name, frame.byte_code_index
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                SummaryRow {
                    tid: tid.0,
                    os_tid: e.os_tid.load(std::sync::atomic::Ordering::Acquire) as u64,
                    name: e.name.clone(),
                    alive: e.alive.load(std::sync::atomic::Ordering::Acquire),
                    daemon: e.daemon.load(std::sync::atomic::Ordering::Acquire),
                    blocked: e
                        .gc_block_state
                        .in_blocked_region
                        .load(std::sync::atomic::Ordering::Acquire),
                    roots: e.root_snapshot.lock().len(),
                    state,
                    top,
                    full_frames,
                }
            })
            .collect();
        rows.sort_by_key(|r| r.tid);
        for row in &rows {
            // `deposit=` sits immediately before `top=` on purpose: the two are
            // read together, and the tag is what stops the chain from being
            // taken for a live wait site.
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} alive={} daemon={} blocked={} roots={} state={:?} deposit={} top={}",
                row.tid,
                row.os_tid,
                row.name,
                row.alive,
                row.daemon,
                row.blocked,
                row.roots,
                row.state,
                Self::deposit_freshness(row.alive, row.blocked),
                row.top
            );
            if row.alive && !row.blocked && !live_dumped.contains(&row.tid) {
                let _ = writeln!(
                    h,
                    "        ^ no-live-dump: this thread produced NO \"T19.H1 stack dump: tid={}\" \
                     section — it never reached an interpreter dispatch point, so it is in \
                     JIT-compiled code or a long native call. Re-run with --nojit; if the live \
                     dump appears there, it was compiled code (and it was RUNNING, not stuck).",
                    row.tid
                );
            }
        }
        // Full frame chains, split out from the one-line-per-thread summary
        // above so that table stays scannable. Restricted to `alive` threads
        // — the registry retains every thread it has ever seen for the life
        // of the process, so a long-running multi-class suite run can carry
        // hundreds of dead entries whose frame chains are pure noise here.
        let live_with_frames: Vec<&SummaryRow> = rows
            .iter()
            .filter(|r| r.alive && !r.full_frames.is_empty())
            .collect();
        // "alive" is NOT the same as "parked here". A running thread keeps the
        // chain it deposited at its last blocking call, so split the count —
        // an all-STALE section is a section with no wait sites in it at all.
        let at_wait_site = live_with_frames.iter().filter(|r| r.blocked).count();
        let _ = writeln!(
            h,
            "--- T19.H1 full frame chains: {} alive thread(s) with a deposited snapshot ({} at a live wait site, {} STALE) ---",
            live_with_frames.len(),
            at_wait_site,
            live_with_frames.len() - at_wait_site
        );
        for row in live_with_frames {
            let tag = if row.blocked {
                "[deposit=live — this IS the thread's current wait site]".to_string()
            } else if live_dumped.contains(&row.tid) {
                format!(
                    "[deposit=STALE — thread is RUNNING (blocked=false); chain below is from its LAST \
                     blocking call and may be arbitrarily old. Its real position is in the \
                     \"T19.H1 stack dump: tid={}\" section above]",
                    row.tid
                )
            } else {
                // Do NOT send the reader to a section that was never emitted —
                // its absence is the finding, not a missing feature.
                "[deposit=STALE + no-live-dump — thread is RUNNING (blocked=false) and answered no \
                 dump, so it is in JIT-compiled code or a long native call. The chain below is from \
                 its LAST blocking call and may be arbitrarily old; there is NO live section for \
                 this thread. Re-run with --nojit to get one.]"
                    .to_string()
            };
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} ({} frame(s), oldest first) {}:",
                row.tid,
                row.os_tid,
                row.name,
                row.full_frames.len(),
                tag
            );
            for (depth, frame) in row.full_frames.iter().enumerate() {
                let _ = writeln!(h, "    [{depth}] {frame}");
            }
        }
        let _ = writeln!(h, "--- T19.H1 end thread summary ---");
        h
    }

    /// Store the OS `JoinHandle` for a spawned thread.
    pub fn set_join_handle(&self, thread_id: ThreadId, handle: JoinHandle<()>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.join_handle = Some(handle);
        }
    }

    /// Mark a thread as dead (called when the thread finishes execution).
    pub fn mark_dead(&self, thread_id: ThreadId) {
        self.mark_dead_with_purge_slack(thread_id, 0);
    }

    /// [`Self::mark_dead`], letting up to `slack` terminated entries pile up
    /// past `TERMINATED_THREAD_ENTRY_CAP` before one purge trims the tail back
    /// to the cap. `0` is `mark_dead`: past the cap, every death collects and
    /// sorts every dead entry to remove the oldest one, under the `threads`
    /// write lock. A JDK continuation's death passes a slack under
    /// `CRATONVM_VTHREAD_LEAN_REGISTRY` (interpreter round i4 wave 94, lane
    /// L6), so a virtual thread per task pays that collect-and-sort once per
    /// `slack` deaths. What a reader sees is unchanged except that up to
    /// `slack` more terminated entries (the newest ones; the purge removes the
    /// oldest) answer as TERMINATED rather than as unknown.
    pub fn mark_dead_with_purge_slack(&self, thread_id: ThreadId, slack: usize) {
        let dead_mirror;
        let dead_park_state;
        let first_death;
        let mut retired_java_tids = Vec::new();
        {
            let mut threads = self.threads.write();
            let Some(entry) = threads.get_mut(&thread_id) else {
                return;
            };
            // JIT round 17 wave 2 (lane smallfix;
            // `r17-takeover-roster-tid-outlives-its-thread-FIXED-20261005.md`): withdraw
            // the published OS tid BEFORE the entry reads dead. The OS recycles
            // a tid once its thread ends, and a dead entry is kept (up to
            // `TERMINATED_THREAD_ENTRY_CAP`) with it: `os_tid_of` (no `alive`
            // filter) would then answer another thread's tid for this one, and
            // a dead virtual thread's entry would keep naming its last
            // carrier. Every roster, census and claimant read already skips a
            // `0` tid. The ordinal is left alone, as `clear_os_tid` does.
            // `CRATONVM_THREAD_DEAD_CLEARS_OS_TID=0` keeps the old tid.
            {
                entry.os_tid.store(0, Ordering::Release);
            }
            // Wave 95 (lane L6): the `alive` edge is counted in
            // `dead_entries` (a second `mark_dead` of a dead entry, e.g. a
            // joiner's, is not).
            first_death = entry.alive.swap(false, Ordering::AcqRel);
            if first_death {
                self.dead_entries.fetch_add(1, Ordering::Relaxed);
                // zt t15/pl6: a listed entry leaves the JMM roster, once.
                self.jmm_roster_leave_on_death(entry);
            }
            // Interpreter round i5 wave 100, lane L6: a counted platform
            // thread leaves the JMM live count, once.
            self.jmm_note_death(entry);
            // Reclaim the OS thread handle. Tomcat's poller/acceptor/pool
            // threads are daemon threads that are never `join()`ed, so their
            // `JoinHandle` would otherwise sit in the registry forever, leaking
            // the underlying Windows thread HANDLE. Across heavy short-lived
            // thread churn — e.g. a parameterized test that starts and stops a
            // whole Tomcat instance ~140 times — this leaks hundreds of thread
            // handles (observed: 219 Thread handles for ~21 live threads),
            // adding steady per-iteration overhead. The thread has finished, so
            // dropping the handle simply detaches it; a later `join()` of an
            // already-dead thread still returns immediately (see `join`).
            entry.join_handle.take();
            dead_mirror = entry.java_thread_obj;
            dead_park_state = entry.park_state.clone();

            // gcj j1/cm: the kept count, not a walk. Every platform thread's
            // death runs this under the registry write lock AND the barrier's
            // `inner` (`finish_after` -> `mark_blocked_region_leave_after`),
            // so the O(entries) walk it replaced serialised every park, wake
            // and pause request behind each death (O(n^2) over a churn
            // burst). Wave 95 (lane L6) keeps `dead_entries` exact; a debug
            // build still checks it against the walk at every death.
            let dead_count = self.dead_entries.load(Ordering::Relaxed);
            debug_assert_eq!(
                dead_count,
                threads
                    .values()
                    .filter(|entry| !entry.alive.load(Ordering::Acquire))
                    .count(),
                "ThreadRegistry::dead_entries drifted from the dead entries"
            );
            if dead_count > TERMINATED_THREAD_ENTRY_CAP.saturating_add(slack) {
                self.purge_dead_tail_locked(&mut threads, dead_count, &mut retired_java_tids);
            }
        }
        // Drop the dead thread's mirror-ADDRESS→ParkState reverse-index entry.
        // The entry itself is retained (TERMINATED `getState()` answers), but
        // once the mirror is unreachable the collector can recycle its heap
        // address for a brand-new `Thread`; a lingering address key would then
        // route that NEW thread's `unpark`/`interrupt` wakeups to this dead
        // ParkState — a silently lost wakeup (observed as Tomcat executor
        // workers parked forever across DoHead's 288 Tomcat start/stop cycles).
        // Guarded by Arc identity so a same-address re-registration that
        // already overwrote the slot (new thread registered before this ran)
        // is left untouched. Lock order threads → thread_obj_to_park matches
        // the register/set paths.
        if let Some(obj) = dead_mirror {
            self.drop_dead_park_index_entry(obj, &dead_park_state, first_death);
        }
        if !retired_java_tids.is_empty() {
            let mut by_java_tid = self.java_tid_to_id.lock();
            for java_tid in retired_java_tids {
                by_java_tid.remove(&java_tid);
            }
        }
        // P1 shadow record — the `-> Terminated` edge. `mark_dead` is NOT
        // always self-called: `ThreadRegistry::join` (below) marks the joinee
        // dead from the *joining* thread. `record_transition_for` therefore
        // records only when the calling thread's cell is bound to this id, so
        // a peer's termination can never be attributed to the caller.
        crate::threading::thread_state::record_transition_for(
            thread_id.0,
            crate::threading::thread_state::ThreadExecState::Terminated,
            "thread_registry::mark_dead",
        );
    }

    /// Drop a dead thread's `thread_obj_to_park` entry: the one keyed by its
    /// mirror's address `obj`, if it still maps to the thread's `park_state`
    /// (a newer thread registered at a recycled address keeps its own).
    ///
    /// Interpreter round i4 wave 96, lane L6: `obj` was read off the entry
    /// before this call, and a dead thread is outside every stop-the-world
    /// census, so a moving collection can relocate the mirror in between;
    /// `update_thread_objs_after_gc` then re-keys the index entry to the new
    /// address and the lookup by `obj` misses. Before, that miss left the dead
    /// `ParkState` in the index for the life of the address -- a terminated
    /// thread kept in a map, which a new `Thread` allocated at that address
    /// before its start would reach. On a miss the index is searched for the
    /// state by identity (rare: only a death that raced a collection). Whatever
    /// the order against the collector's re-key, which moves only keys that are
    /// present, the entry goes: removed here first, the re-key finds nothing;
    /// re-keyed first, the identity search finds it. `first_death`: this call
    /// follows the death that lowered `alive`; a later `mark_dead` of the same
    /// thread (a joiner's) misses because the first one removed the entry, and
    /// does not search.
    pub(in crate::threading::thread_registry) fn drop_dead_park_index_entry(
        &self,
        obj: ObjectRef,
        park_state: &Arc<ParkState>,
        first_death: bool,
    ) {
        let key = obj.as_ptr() as usize;
        let mut idx = self.thread_obj_to_park.lock();
        match idx.get(&key) {
            Some(mapped) => {
                if Arc::ptr_eq(mapped, park_state) {
                    idx.remove(&key);
                }
            }
            None if first_death => idx.retain(|_, mapped| !Arc::ptr_eq(mapped, park_state)),
            None => {}
        }
    }

    /// Insert `entry` under the `threads` write lock, keeping
    /// [`Self::dead_entries`] exact when it replaces a dead entry of the same
    /// id (ids are not reused; this is for the hand-built ones).
    pub(in crate::threading::thread_registry) fn insert_entry(&self, thread_id: ThreadId, entry: ThreadEntry) {
        let mut threads = self.threads.write();
        // zt t15/pl6: the new entry joins the JMM roster when listed.
        if Self::jmm_roster_lists(&entry) {
            self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), 1);
        }
        if let Some(old) = threads.insert(thread_id, entry) {
            if !old.alive.load(Ordering::Acquire) {
                self.dead_entries.fetch_sub(1, Ordering::Relaxed);
            }
            // Wave 100 (lane L6): a replaced live entry leaves the JMM count.
            self.jmm_note_death(&old);
            // zt t15/pl6: and the roster.
            if Self::jmm_roster_lists(&old) {
                self.jmm_roster_add(old.daemon.load(Ordering::Acquire), -1);
            }
        }
    }

    /// Count `thread_id`'s start in the JMM's platform-thread counters
    /// (interpreter round i5 wave 100, lane L6; HotSpot's
    /// `ThreadService::add_thread`): the cumulative started count, the live
    /// count and, raised to it, the peak. Once per entry (a second call is a
    /// no-op); an unknown or dead id is not counted. Callers: a platform
    /// thread's `Thread.start` (`vm_exec::thread_start`, never a virtual
    /// thread), the main thread's registration and a JNI attach.
    pub fn note_jmm_thread_start(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return;
        };
        if !entry.alive.load(Ordering::Acquire) || entry.jmm_counted.swap(true, Ordering::AcqRel) {
            return;
        }
        self.jmm_started.fetch_add(1, Ordering::Relaxed);
        let live = self.jmm_live.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        self.jmm_peak.fetch_max(live, Ordering::AcqRel);
    }

    /// The death half of [`Self::note_jmm_thread_start`]: a counted entry
    /// leaves the live count exactly once (`jmm_counted` lowered).
    pub(in crate::threading::thread_registry) fn jmm_note_death(&self, entry: &ThreadEntry) {
        if entry.jmm_counted.swap(false, Ordering::AcqRel) {
            self.jmm_live.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// The JMM's platform-thread counters `(started, live, peak)` (wave 100,
    /// lane L6; `ThreadMXBean.getTotalStartedThreadCount`, `getThreadCount`,
    /// `getPeakThreadCount`).
    pub fn jmm_counters(&self) -> (i64, i32, i32) {
        let started = i64::try_from(self.jmm_started.load(Ordering::Acquire)).unwrap_or(i64::MAX);
        let live = self.jmm_live.load(Ordering::Acquire);
        let peak = self.jmm_peak.load(Ordering::Acquire).max(live);
        (started, live, peak)
    }

    /// `ThreadMXBean.resetPeakThreadCount`: the peak becomes the live count
    /// (wave 100, lane L6).
    pub fn jmm_peak_reset(&self) {
        self.jmm_peak
            .store(self.jmm_live.load(Ordering::Acquire), Ordering::Release);
    }

    /// zt t15/pl6: does `ThreadMXBean`'s walk list `entry`? Alive, with a
    /// `java.lang.Thread` mirror (`alive_thread_objects`), and not a virtual
    /// thread's (`management::is_virtual_thread_mirror`).
    pub(in crate::threading::thread_registry) fn jmm_roster_lists(entry: &ThreadEntry) -> bool {
        !entry.virtual_thread
            && entry.java_thread_obj.is_some()
            && entry.alive.load(Ordering::Acquire)
    }

    /// zt t15/pl6: move the JMM roster by `delta` (one entry joining or
    /// leaving), and its daemon count too for a daemon.
    pub(in crate::threading::thread_registry) fn jmm_roster_add(&self, daemon: bool, delta: i32) {
        self.jmm_roster_live.fetch_add(delta, Ordering::AcqRel);
        if daemon {
            self.jmm_roster_daemon.fetch_add(delta, Ordering::AcqRel);
        }
    }

    /// zt t15/pl6: `entry`'s `alive` just went true -> false (the caller saw
    /// the edge): it leaves the roster if it was listed then.
    pub(in crate::threading::thread_registry) fn jmm_roster_leave_on_death(&self, entry: &ThreadEntry) {
        if !entry.virtual_thread && entry.java_thread_obj.is_some() {
            self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), -1);
        }
    }

    /// zt t15/pl6 (`zgc-zt12pl4-perf-counters-what-is-left-*` item 1): the
    /// figures `ThreadMXBean` answers, from the registry alone, for the
    /// `java.threads.*` perf counters the sampler writes (it cannot read a
    /// mirror's class or holder off a mutator): `live` (`getThreadCount`),
    /// `daemon` (`getDaemonThreadCount`), `peak` (`getPeakThreadCount`) and
    /// `started` (`getTotalStartedThreadCount`). `live` and `daemon` are the
    /// roster ([`Self::jmm_roster_lists`]) less the ended `main` thread the
    /// bean's enumeration leaves out (`NativeContext::enumerate_threads`);
    /// `peak` and `started` are the JMM counters plus the listed threads they
    /// did not count, never below `live`, with the formulas of
    /// `management::peak_thread_count` / `total_started_thread_count`.
    /// HotSpot's `ThreadService` keeps the four perf counters with the bean's
    /// own figures (`_live_threads_count`, `_daemon_threads_count`,
    /// `_peak_threads_count`, `_total_threads_count`).
    pub fn jmm_perf_thread_counts(&self) -> JmmThreadCounts {
        let mut live = self.jmm_roster_live.load(Ordering::Acquire);
        let mut daemon = self.jmm_roster_daemon.load(Ordering::Acquire);
        if self.java_side_ended(ThreadId(0)) && !self.destroy_java_vm_listed() {
            if let Some(main) = self.threads.read().get(&ThreadId(0)) {
                if Self::jmm_roster_lists(main) {
                    live -= 1;
                    if main.daemon.load(Ordering::Acquire) {
                        daemon -= 1;
                    }
                }
            }
        }
        let (live, daemon) = (live.max(0), daemon.max(0));
        let (started, counted_live, peak) = self.jmm_counters();
        let uncounted = live.saturating_sub(counted_live);
        JmmThreadCounts {
            live,
            daemon,
            peak: peak.saturating_add(uncounted).max(live),
            started: started
                .saturating_add(i64::from(uncounted))
                .max(i64::from(live)),
        }
    }

    /// Remove the oldest (lowest-id) dead entries until `TERMINATED_THREAD_ENTRY_CAP`
    /// remain; `dead_count` is the number of dead entries now. The caller holds
    /// the `threads` write lock and drops each retired `java_tid` from the
    /// index after releasing it.
    pub(in crate::threading::thread_registry) fn purge_dead_tail_locked(
        &self,
        threads: &mut FxHashMap<ThreadId, ThreadEntry>,
        dead_count: usize,
        retired_java_tids: &mut Vec<u64>,
    ) {
        let mut purge: Vec<ThreadId> = threads
            .iter()
            .filter_map(|(id, entry)| (!entry.alive.load(Ordering::Acquire)).then_some(*id))
            .collect();
        purge.sort_by_key(|id| id.0);
        let mut removed = 0usize;
        for id in purge
            .into_iter()
            .take(dead_count.saturating_sub(TERMINATED_THREAD_ENTRY_CAP))
        {
            if let Some(retired) = threads.remove(&id) {
                removed += 1;
                if retired.java_tid != 0 {
                    retired_java_tids.push(retired.java_tid);
                }
            }
        }
        // Only dead entries were removed.
        self.dead_entries.fetch_sub(removed, Ordering::Relaxed);
    }

    /// Register JDK continuation thread `thread_id` COMPLETE, in one `threads`
    /// write (interpreter round i4 wave 95, lane L6, `CRATONVM_VTHREAD_ONE_PUBLISH`):
    /// its virtual-thread mirror and that mirror's `Thread.tid` (`0`: unknown),
    /// the `Arc`s its `JvmThread` already holds (`shared`), and the addresses
    /// of its `JvmThread` and TLAB, as HotSpot's `Threads::add` publishes a
    /// prepared thread once. Replaces `register_starting_with_daemon`, then
    /// `set_java_thread_obj_with_tid` (two writes), `starting_thread_states`,
    /// `set_park_state`, `set_interrupted_flag`, `set_root_snapshot`,
    /// `set_frame_trace`, `set_vm_state`, `set_gc_block_state` (one write each)
    /// and `set_tlab_addr` / `set_jvm_thread_addr`: what
    /// `jdk_continuation::register_continuation_thread` used to do, with the
    /// same end state (a daemon entry, not yet STW-ready, mounted). The
    /// mirror and `tid` indexes are updated after the write, as
    /// [`Self::register_starting_with_daemon`] and
    /// [`Self::set_java_thread_obj_with_tid`] update them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_prepared_continuation_thread(
        &self,
        thread_id: ThreadId,
        name: &str,
        vthread: ObjectRef,
        java_tid: u64,
        shared: crate::threading::jvm_thread::ThreadSharedArcs,
        tlab_addr: usize,
        jvm_thread_addr: usize,
    ) {
        let crate::threading::jvm_thread::ThreadSharedArcs {
            park_state,
            interrupted,
            root_snapshot,
            frame_trace,
            vm_state,
            gc_block_state,
        } = shared;
        // Wave 73 (`CRATONVM_PARK_ATOMIC_PERMIT`): before the state is
        // reachable by an unpark through this registry (`set_park_state`).
        if self.park_atomic_permit {
            park_state.enable_lock_free();
        }
        let entry = ThreadEntry {
            name: name.to_string(),
            join_handle: None,
            java_thread_obj: Some(vthread),
            java_tid,
            alive: AtomicBool::new(true),
            stw_ready: AtomicBool::new(false),
            daemon: AtomicBool::new(true),
            park_state: park_state.clone(),
            interrupted,
            root_snapshot,
            frame_trace,
            vm_state,
            gc_block_state,
            jmx_monitors: Arc::new(JmxMonitorBook::default()),
            jmx_waiting_monitor: Mutex::new(None),
            jmx_locked_synchronizers: Arc::new(Mutex::new(Vec::new())),
            jmx_wait_started: Mutex::new(None),
            jmx_waited_count: AtomicU64::new(0),
            jmx_waited_nanos: AtomicU64::new(0),
            tlab_addr: std::sync::atomic::AtomicUsize::new(tlab_addr),
            jvm_thread_addr: std::sync::atomic::AtomicUsize::new(jvm_thread_addr),
            os_tid: std::sync::atomic::AtomicU32::new(0),
            os_tid_seq: AtomicU64::new(0),
            mounted: AtomicBool::new(true),
            mount_epoch: AtomicU64::new(0),
            parked_filter: ParkedFoldFilter::new(),
            registered_at: Instant::now(),
            unstructured_monitor_enters: std::sync::atomic::AtomicU32::new(0),
            jmm_counted: AtomicBool::new(false),
            virtual_thread: true,
        };
        self.insert_entry(thread_id, entry);
        self.thread_obj_to_park
            .lock()
            .insert(vthread.as_ptr() as usize, park_state);
        if java_tid != 0 {
            self.java_tid_to_id.lock().insert(java_tid, thread_id);
        }
    }

    /// A JDK continuation's mount, in one `threads` READ (interpreter round i4
    /// wave 95, lane L6, `CRATONVM_VTHREAD_ONE_PUBLISH`): the four publishes
    /// `jdk_continuation::run_slice` made one lock each, in their order --
    /// this OS thread's tid with a fresh ordinal (`set_os_tid_current`), the
    /// `mounted` bit, the `JvmThread` and TLAB addresses -- and the entry's
    /// mirror (`java_thread_obj`), which it read with a fifth. `None` for an
    /// unknown id (nothing published), as each of the five answered then.
    /// With `CRATONVM_DBG_BLOCKED_ACCESS` on, the caller keeps the old calls
    /// (`set_os_tid_current` registers its canary there).
    pub(crate) fn mount_continuation_thread(
        &self,
        thread_id: ThreadId,
        jvm_thread_addr: usize,
        tlab_addr: usize,
    ) -> Option<Option<ObjectRef>> {
        let os_tid = current_os_tid();
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if os_tid != 0 {
            entry.os_tid.store(os_tid, Ordering::Release);
            let seq = OS_TID_PUBLISH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            entry.os_tid_seq.store(seq, Ordering::Release);
        }
        // gci merge 2026-10-15: the one-publish mount moves the `mounted` bit
        // too, so it retires the parked fold filter's epoch first, exactly as
        // `set_mounted` does (gci i12/vp).
        entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
        entry.mounted.store(true, Ordering::Release);
        entry
            .jvm_thread_addr
            .store(jvm_thread_addr, Ordering::Release);
        entry.tlab_addr.store(tlab_addr, Ordering::Release);
        Some(entry.java_thread_obj)
    }

    /// The first half of a JDK continuation's death, in ONE `threads` write
    /// with no walk (interpreter round i4 wave 95, lane L6,
    /// `CRATONVM_VTHREAD_ONE_PUBLISH`), for `BlockedGuard::finish_after`
    /// (the GC barrier's transition lock, which every blocked-region enter and
    /// leave in the VM takes, is held across it): `clear_tlab_addr` (the TLAB
    /// and `JvmThread` addresses), then `mark_dead`'s edge -- the OS tid
    /// withdrawn, `alive` lowered and counted in [`Self::dead_entries`], the
    /// join handle dropped. The rest of `mark_dead` -- the mirror index, the
    /// purge of the terminated tail, the `tid` index, the shadow record --
    /// is [`Self::finish_retired_continuation_thread`]'s, after the barrier
    /// lock is released. `None` for an unknown id.
    pub(crate) fn retire_continuation_thread(&self, thread_id: ThreadId) -> Option<RetiredThread> {
        let mut threads = self.threads.write();
        let entry = threads.get_mut(&thread_id)?;
        entry.tlab_addr.store(0, Ordering::Release);
        entry.jvm_thread_addr.store(0, Ordering::Release);
        {
            entry.os_tid.store(0, Ordering::Release);
        }
        let first_death = entry.alive.swap(false, Ordering::AcqRel);
        if first_death {
            self.dead_entries.fetch_add(1, Ordering::Relaxed);
            // zt t15/pl6: a continuation's entry is virtual (never listed);
            // the edge is kept for any other entry retired this way.
            self.jmm_roster_leave_on_death(entry);
        }
        entry.join_handle.take();
        Some(RetiredThread {
            mirror: entry.java_thread_obj,
            park_state: entry.park_state.clone(),
            first_death,
            unstructured_monitor_enters: entry
                .unstructured_monitor_enters
                .load(Ordering::Acquire),
        })
    }

    /// The second half of a JDK continuation's death
    /// ([`Self::retire_continuation_thread`]), outside the GC barrier's lock:
    /// the mirror index entry goes (as in `mark_dead`), and once more than
    /// `TERMINATED_THREAD_ENTRY_CAP + slack` entries are dead -- read from
    /// [`Self::dead_entries`], not counted by a walk -- one purge trims the
    /// tail back to the cap. Answers whether it purged.
    pub(crate) fn finish_retired_continuation_thread(
        &self,
        thread_id: ThreadId,
        retired: &RetiredThread,
        slack: usize,
    ) -> bool {
        if let Some(obj) = retired.mirror {
            self.drop_dead_park_index_entry(obj, &retired.park_state, retired.first_death);
        }
        let limit = TERMINATED_THREAD_ENTRY_CAP.saturating_add(slack);
        let mut retired_java_tids = Vec::new();
        let mut purged = false;
        if self.dead_entries.load(Ordering::Relaxed) > limit {
            let mut threads = self.threads.write();
            // Re-read under the lock: another death may have purged already.
            let dead_count = self.dead_entries.load(Ordering::Relaxed);
            if dead_count > limit {
                self.purge_dead_tail_locked(&mut threads, dead_count, &mut retired_java_tids);
                purged = true;
            }
        }
        if !retired_java_tids.is_empty() {
            let mut by_java_tid = self.java_tid_to_id.lock();
            for java_tid in retired_java_tids {
                by_java_tid.remove(&java_tid);
            }
        }
        crate::threading::thread_state::record_transition_for(
            thread_id.0,
            crate::threading::thread_state::ThreadExecState::Terminated,
            "thread_registry::mark_dead",
        );
        purged
    }

    /// Count one monitor `thread_id` entered outside structured locking
    /// (`ThreadEntry::unstructured_monitor_enters`; interpreter round i4 wave
    /// 95, lane L6, `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`). No-op for an
    /// unknown id.
    pub fn note_unstructured_monitor_enter(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            entry
                .unstructured_monitor_enters
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    /// `thread_id`'s count of [`Self::note_unstructured_monitor_enter`]s not
    /// yet exited. An unknown id answers `u32::MAX` (never "holds nothing").
    pub(crate) fn unstructured_monitor_enters_of(&self, thread_id: ThreadId) -> u32 {
        self.threads.read().get(&thread_id).map_or(u32::MAX, |entry| {
            entry.unstructured_monitor_enters.load(Ordering::Acquire)
        })
    }

    /// The matching successful exit of [`Self::note_unstructured_monitor_enter`]
    /// (never below 0: an exit of a monitor entered by bytecode is not one).
    pub fn note_unstructured_monitor_exit(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            let _ = entry.unstructured_monitor_enters.try_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |n| n.checked_sub(1),
            );
        }
    }

    /// Aliasing-proof identity lookup: registry `ThreadId` by the Java-side
    /// `Thread.tid` value (see `ThreadEntry::java_tid`). Dead threads remain
    /// resolvable (TERMINATED), but a recycled mirror address can never alias
    /// because `tid` values are unique for the life of the process.
    pub fn find_thread_id_by_java_tid(&self, java_tid: u64) -> Option<ThreadId> {
        if java_tid == 0 {
            return None;
        }
        self.java_tid_to_id.lock().get(&java_tid).copied()
    }

    /// ParkState lookup by Java-side `Thread.tid` — the aliasing-proof
    /// counterpart of `find_park_state_by_thread_obj` (see
    /// `find_thread_id_by_java_tid`).
    pub fn find_park_state_by_java_tid(&self, java_tid: u64) -> Option<Arc<ParkState>> {
        let id = self.find_thread_id_by_java_tid(java_tid)?;
        self.get_park_state(id)
    }

    /// `Unsafe.unpark`'s whole registry question in one call (interpreter
    /// round i3 wave 72, lane L6, `CRATONVM_UNPARK_ONE_LOOKUP`): the thread
    /// id and the `ParkState` of the thread whose Java `tid` is `java_tid`.
    /// The two answers `find_thread_id_by_java_tid` and
    /// `find_park_state_by_java_tid` gave separately (the `java_tid` index
    /// taken twice), from one index lookup and one `threads` read.
    pub fn unpark_target_by_java_tid(&self, java_tid: u64) -> Option<(ThreadId, Arc<ParkState>)> {
        let id = self.find_thread_id_by_java_tid(java_tid)?;
        let state = self.get_park_state(id)?;
        Some((id, state))
    }

    /// Pointer-identity walk like `find_thread_id_by_thread_obj`, but guarded
    /// by the caller-supplied Java `tid` of the queried mirror: an entry at
    /// the same address whose recorded `java_tid` differs is a DEAD thread's
    /// stale entry whose mirror address was recycled for the queried (new)
    /// Thread — matching it would misreport a freshly constructed thread as
    /// RUNNABLE/TERMINATED (the DoHead `IllegalThreadStateException`-at-
    /// engine-start flake). An entry with `java_tid == 0` (registered
    /// mid-construction, before the ctor assigned `tid`) is accepted and
    /// lazily backfilled so subsequent lookups take the O(1) tid index.
    pub fn find_thread_id_by_thread_obj_tid_checked(
        &self,
        obj: ObjectRef,
        java_tid: u64,
    ) -> Option<ThreadId> {
        let mut threads = self.threads.write();
        let mut found: Option<ThreadId> = None;
        for (id, entry) in threads.iter() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr())
                    && (entry.java_tid == java_tid || entry.java_tid == 0)
                {
                    found = Some(*id);
                    break;
                }
            }
        }
        if let Some(id) = found {
            if java_tid != 0 {
                let mut backfilled = false;
                if let Some(entry) = threads.get_mut(&id) {
                    if entry.java_tid == 0 {
                        entry.java_tid = java_tid;
                        backfilled = true;
                    }
                }
                drop(threads);
                if backfilled {
                    self.java_tid_to_id.lock().insert(java_tid, id);
                }
            }
        }
        found
    }

    /// Check if a thread is still alive.
    /// Record that `thread_id`'s `java.lang.Thread` ended while its entry
    /// stays alive (see the field `java_ended_thread`).
    pub fn mark_java_side_ended(&self, thread_id: ThreadId) {
        self.java_ended_thread.store(thread_id.0, Ordering::Release);
    }

    /// Did `thread_id`'s `java.lang.Thread` end while its entry stays alive?
    /// One load.
    #[inline]
    pub fn java_side_ended(&self, thread_id: ThreadId) -> bool {
        self.java_ended_thread.load(Ordering::Acquire) == thread_id.0
    }

    /// Record that the ended entry now carries its `DestroyJavaVM` mirror
    /// (see the field `destroy_java_vm_listed`; wave 102, lane L6).
    pub fn mark_destroy_java_vm_listed(&self) {
        self.destroy_java_vm_listed.store(true, Ordering::Release);
    }

    /// Does the ended entry carry a `DestroyJavaVM` mirror? One load.
    #[inline]
    pub fn destroy_java_vm_listed(&self) -> bool {
        self.destroy_java_vm_listed.load(Ordering::Acquire)
    }

    /// Is `thread_obj`, resolved to `thread_id`, a `java.lang.Thread` whose
    /// Java side ended ([`Self::java_side_ended`])? The entry's current
    /// mirror is not, once it is the `DestroyJavaVM` one
    /// ([`Self::destroy_java_vm_listed`]); every other mirror resolved to the
    /// entry (the ended `main`) is.
    pub fn java_side_ended_for(&self, thread_id: ThreadId, thread_obj: ObjectRef) -> bool {
        if !self.java_side_ended(thread_id) {
            return false;
        }
        !(self.destroy_java_vm_listed()
            && self
                .java_thread_obj(thread_id)
                .is_some_and(|m| m.as_ptr() == thread_obj.as_ptr()))
    }

    pub fn is_alive(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.alive.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// `true` only for a thread this registry registered and has since marked
    /// dead — POSITIVE evidence of death, unlike `!is_alive`, which is also true
    /// for an id the registry never saw (the JNI `ThreadId(0)` placeholder
    /// owner, or a unit-test id). Used to release a thin lock whose owner died
    /// before anyone contended it (`vm_exec::release_dead_thin_owner`), where
    /// a false positive would hand a live thread's lock to another thread.
    ///
    /// A dead entry reaped past `TERMINATED_THREAD_ENTRY_CAP` answers `false`:
    /// the recovery then does not happen, which is the behaviour before it
    /// existed, never a wrong release.
    pub(crate) fn is_registered_dead(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .is_some_and(|e| !e.alive.load(Ordering::Acquire))
    }

    /// gcj j3/x2 R7: is `thread_id` registered, alive and able to answer
    /// counted STW barriers ([`Self::mark_stw_ready`])?
    pub fn is_stw_ready(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .is_some_and(|e| e.alive.load(Ordering::Acquire) && e.stw_ready.load(Ordering::Acquire))
    }

    /// Mark a started carrier as able to participate in counted STW barriers.
    pub fn mark_stw_ready(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.stw_ready.store(true, Ordering::Release);
        }
        drop(threads);
        // P1 shadow record — the `Starting -> JavaRunning` edge. Always
        // self-called by the freshly spawned carrier (`vm/src/vm/vm_exec.rs`
        // `thread_start` and the virtual-thread first-mount arm), inside
        // `GcBarrier::run_if_no_stw_requested`, so binding here is safe.
        crate::threading::thread_state::bind_current_thread(thread_id.0);
        crate::threading::thread_state::record_transition(
            crate::threading::thread_state::ThreadExecState::JavaRunning,
            "thread_registry::mark_stw_ready",
        );
    }

    /// Block the calling OS thread until the target thread finishes.
    ///
    /// Takes the `JoinHandle` out of the registry and joins on it.
    /// Returns `true` if the join succeeded, `false` if no handle was found
    /// (already joined, or it's the main thread).
    pub fn join(&self, thread_id: ThreadId) -> bool {
        self.join_giving_up_at(thread_id, None)
    }

    /// [`Self::join`], but a joiner that finds no handle while the thread is
    /// alive gives up at `give_up` (`None`: waits for the death).
    ///
    /// Interpreter round i1 wave 24, lane L7: the no-handle arm used to spin
    /// for about one second and return while the target was still alive. The
    /// arm is not only the spawn/`set_join_handle` race its comment named: a
    /// SECOND concurrent `Thread.join()` of the same thread always lands here,
    /// because the first joiner took the handle. So of two threads joining one
    /// worker that runs longer than a second, the second returned early and
    /// read the worker's results before they existed. `Thread.join()` has no
    /// deadline; only `wait_for_non_daemon_threads`, which re-snapshots its
    /// pending set in a loop and honours its own deadline, bounds the wait.
    pub(in crate::threading::thread_registry) fn join_giving_up_at(&self, thread_id: ThreadId, give_up: Option<std::time::Instant>) -> bool {
        let handle = {
            let mut threads = self.threads.write();
            threads
                .get_mut(&thread_id)
                .and_then(|e| e.join_handle.take())
        };
        match handle {
            Some(h) => {
                let _ = h.join();
                // Mark as dead now that join completed
                self.mark_dead(thread_id);
                true
            }
            None => {
                // No handle — either already joined, or it's the main thread.
                // If the thread is dead, return immediately.
                if !self.is_alive(thread_id) {
                    return false;
                }
                // Thread is alive but its handle is elsewhere: another joiner
                // holds it, or the spawn has not stored it yet. Poll for the
                // death (the thread's exit path and the handle holder both
                // `mark_dead`), backing off from 1 ms to 10 ms.
                let mut slice = std::time::Duration::from_millis(1);
                loop {
                    if !self.is_alive(thread_id) {
                        return false;
                    }
                    if give_up.is_some_and(|d| std::time::Instant::now() >= d) {
                        return false;
                    }
                    std::thread::sleep(slice);
                    slice = (slice * 2).min(std::time::Duration::from_millis(10));
                }
            }
        }
    }

    /// Get the Java Thread object for a given ThreadId.
    pub fn java_thread_obj(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        self.threads
            .read()
            .get(&thread_id)
            .and_then(|e| e.java_thread_obj)
    }

    /// Record that thread `tid`'s `java.lang.Thread` mirror just vacated
    /// `old_addr` because a moving / promoting GC relocated it. Enables
    /// [`Self::recover_stale_mirror`] to repair a frame that resumed holding a
    /// stale copy of `old_addr`. Bounded FIFO ([`FORMER_MIRROR_CAP`]). Takes
    /// only the `former_mirror_addrs` lock — callers must NOT hold the
    /// `threads` lock across this (see `update_thread_objs_after_gc`, which
    /// records only after dropping `threads`) so the lock order stays
    /// `threads → former`, opposite to `recover_stale_mirror`'s
    /// `former → threads`; neither nests, so they cannot deadlock.
    pub(in crate::threading::thread_registry) fn record_former_mirror_addr(&self, old_addr: usize, tid: ThreadId) {
        let mut g = self.former_mirror_addrs.lock();
        let (map, order) = &mut *g;
        if map.insert(old_addr, tid).is_none() {
            order.push_back(old_addr);
            while order.len() > FORMER_MIRROR_CAP {
                if let Some(evict) = order.pop_front() {
                    map.remove(&evict);
                }
            }
        }
    }

    /// If `stale_addr` is a recorded former address of some thread's
    /// `java.lang.Thread` mirror, return that thread's CURRENT (live,
    /// GC-remapped) mirror so a stale-receiver use site can recover. Identity
    /// preserving — a vacated address uniquely identified one thread's mirror,
    /// so the returned mirror is the right thread's even if a *different*
    /// thread is executing. The caller must still verify the returned object
    /// is live (non-zero header) before use.
    pub fn recover_stale_mirror(&self, stale_addr: usize) -> Option<ObjectRef> {
        let tid = {
            let g = self.former_mirror_addrs.lock();
            *g.0.get(&stale_addr)?
        };
        self.java_thread_obj(tid)
    }

    /// Set the Java Thread object for a given ThreadId, recording the
    /// mirror's Java-side `Thread.tid` (0 = unknown; see
    /// `ThreadEntry::java_tid`) so identity lookups can use the
    /// aliasing-proof tid index instead of the mirror's (recyclable) address.
    pub fn set_java_thread_obj_with_tid(&self, thread_id: ThreadId, obj: ObjectRef, java_tid: u64) {
        self.set_java_thread_obj(thread_id, obj);
        if java_tid != 0 {
            {
                let mut threads = self.threads.write();
                if let Some(entry) = threads.get_mut(&thread_id) {
                    entry.java_tid = java_tid;
                } else {
                    return;
                }
            }
            self.java_tid_to_id.lock().insert(java_tid, thread_id);
        }
    }

    /// Set the Java Thread object for a given ThreadId.
    pub fn set_java_thread_obj(&self, thread_id: ThreadId, obj: ObjectRef) {
        let park_state_clone;
        let prev_obj;
        {
            let mut threads = self.threads.write();
            if let Some(entry) = threads.get_mut(&thread_id) {
                prev_obj = entry.java_thread_obj;
                // zt t15/pl6: a first mirror lists the entry.
                let listed = Self::jmm_roster_lists(entry);
                entry.java_thread_obj = Some(obj);
                if !listed && Self::jmm_roster_lists(entry) {
                    self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), 1);
                }
                park_state_clone = entry.park_state.clone();
            } else {
                return;
            }
        }
        // WP4.1 — keep reverse index in sync. If we replaced an older obj
        // pointer (rare; only happens if Thread is re-bound), drop the
        // stale entry first.
        let mut idx = self.thread_obj_to_park.lock();
        if let Some(prev) = prev_obj {
            if prev.as_ptr() != obj.as_ptr() {
                idx.remove(&(prev.as_ptr() as usize));
            }
        }
        idx.insert(obj.as_ptr() as usize, park_state_clone);
    }

    /// Withdraw a provisional mirror after construction refuses allocation.
    /// A replacement binding is left alone. Java tid aliases name the logical
    /// thread and remain valid if a caller restores its previous mirror.

    pub(crate) fn clear_java_thread_obj_if(&self, thread_id: ThreadId, expected: ObjectRef) {
        let removed = {
            let mut threads = self.threads.write();
            if let Some(entry) = threads.get_mut(&thread_id) {
                if entry.java_thread_obj == Some(expected) {
                    // zt t15/pl6: a withdrawn mirror unlists the entry.
                    if Self::jmm_roster_lists(entry) {
                        self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), -1);
                    }
                    entry.java_thread_obj.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(object) = removed {
            self.thread_obj_to_park
                .lock()
                .remove(&(object.as_ptr() as usize));
        }
    }

    /// Rename a registered thread's entry (the name thread dumps, JVMTI and
    /// hprof read): a JNI-attached thread's placeholder once its mirror takes
    /// the JDK's `Thread-N` (interpreter round i4 wave 84, lane L6). No-op for
    /// an unknown id.
    pub fn set_thread_name(&self, thread_id: ThreadId, name: &str) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.name = name.to_string();
        }
    }

    /// Restart the entry's thread-dump clock (`elapsed=`) now: an entry that
    /// starts carrying a new Java thread, as the launcher's main entry does
    /// at its `DestroyJavaVM` attach (interpreter round i5 wave 104, lane L6;
    /// HotSpot's `elapsed=` counts from the attached `JavaThread`'s creation,
    /// not from `main`'s).
    pub fn restamp_registered_at(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.registered_at = Instant::now();
        }
    }

    /// Get the name of a thread.
    pub fn thread_name(&self, thread_id: ThreadId) -> Option<String> {
        self.threads.read().get(&thread_id).map(|e| e.name.clone())
    }

    /// The Java-side `Thread.tid` for a registry id, or `None` when this entry
    /// has no Java mirror yet (see [`ThreadEntry::java_tid`], 0 = unknown).
    ///
    /// Needed because `ThreadJmxSnapshot::thread_id` carries the JAVA tid
    /// while monitor ownership is keyed by the registry `ThreadId`. Putting a
    /// registry id in `lock_owner_id` would mix two numbering schemes in one
    /// field, and `findDeadlockedThreads` matches `lock_owner_id` against
    /// other threads' `thread_id` to build its wait-for graph — so the
    /// mismatch would silently yield no edges rather than a visible error.
    pub fn java_tid_of(&self, thread_id: ThreadId) -> Option<u64> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.java_tid)
            .filter(|tid| *tid != 0)
    }

    /// The OS-level thread id for a registry id, or `None` if this entry has
    /// not published one yet (0 = unset).
    ///
    /// This is what an ARBITRARY-thread CPU-time read needs — `GetThreadTimes`
    /// after `OpenThread` on Windows, `/proc/self/task/<tid>/stat` on Linux.
    /// `ThreadMXBean.isThreadCpuTimeSupported()` answered false purely because
    /// this was not reachable from a native, not because the VM lacked the
    /// datum: `set_os_tid_current` has been publishing it all along.
    pub fn os_tid_of(&self, thread_id: ThreadId) -> Option<u32> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.os_tid.load(Ordering::Acquire))
            .filter(|tid| *tid != 0)
    }

    /// Publish a monitor acquisition attempt before it can block. The object
    /// is rooted by this registry entry until the acquire completes.
    ///
    /// Through the thread's cached book (round 11 wave 17), not the registry
    /// map: every writer is the contending thread itself, and the closure
    /// touches only the book's mutexes and atomics, never its single-writer
    /// lock stack, so a caller naming another thread (a unit test) is still
    /// sound -- it just misses the cache.
    pub fn set_jmx_contended_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        self.with_self_jmx_monitors(thread_id, |book| {
            *book.contended_monitor.lock() = Some(monitor);
            *book.contended_started.lock() = Some(Instant::now());
            // Arms the two slots above for `complete_jmx_monitor_enter`, whose
            // uncontended callers skip them entirely — see `JmxMonitorBook`.
            book.contending.store(true, Ordering::Release);
            // Count the block HERE, not on release. The JMM counts a thread as
            // having blocked the moment it starts waiting, and a JMX consumer
            // diagnosing a hang reads `getBlockedCount()` while the thread is
            // STILL blocked — counting on the way out leaves exactly that case
            // reporting zero, which is the case that matters. Measured: a
            // thread parked on a held monitor reported `blockedCount == 0` on
            // every thread in the VM until this moved.
            book.blocked_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Finish an acquisition attempt. Successful acquisitions become owned
    /// monitor roots; failed/aborted attempts simply lose the contention root.
    pub fn complete_jmx_monitor_enter(&self, thread_id: ThreadId, monitor: ObjectRef) {
        self.with_self_jmx_monitors(thread_id, |book| {
            // THE CONTENDED SLOTS, ONLY WHEN THERE WAS CONTENTION. This method
            // is the ownership publish for the uncontended arm too, and that arm
            // never called `set_jmx_contended_monitor`, so both slots are
            // already `None` and locking them twice to write `None` over `None`
            // was pure cost on every `monitorenter` in the process.
            if book.contending.load(Ordering::Acquire) || !monitor_fastpath_enabled() {
                *book.contended_monitor.lock() = None;
                // Only the DURATION is added here — the count was taken when
                // the block began. On the book since round 11 wave 17, so the
                // contended arm no longer takes the registry map either.
                if let Some(start) = book.contended_started.lock().take() {
                    let nanos = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    book.blocked_nanos.fetch_add(nanos, Ordering::Relaxed);
                }
                book.contending.store(false, Ordering::Release);
            }
            book.locked.publish(monitor);
        });
    }

    pub fn remove_jmx_locked_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        self.with_self_jmx_monitors(thread_id, |book| book.locked.retract(monitor));
    }

    /// `(top, spilled)` of the calling thread's own lock stack: the two words
    /// the inline `monitorenter` refuses on (a full stack, a spilled one). For
    /// the `CRATONVM_DBG_JITC` monitor census
    /// (`vm_exec::report_monitor_inline_census`). Self-only, like
    /// [`Self::with_self_jmx_monitors`].
    pub(crate) fn jmx_lock_stack_shape(&self, thread_id: ThreadId) -> Option<(usize, bool)> {
        self.with_self_jmx_monitors(thread_id, |book| {
            (
                book.locked.top.load(Ordering::Relaxed),
                book.locked.spilled.load(Ordering::Relaxed),
            )
        })
    }

    /// Arm `block` (the calling thread's own `JvmThread::jit_monitor_block`)
    /// so compiled code can take an uncontended `monitorenter`/`monitorexit`
    /// inline, recording it in the SAME [`JmxLockStack`] the helpers publish
    /// to. A no-op when already armed from this registry.
    ///
    /// `lease` is asked only when the block is not armed already, and must
    /// answer the calling thread's own thin-lock lease from THIS VM's
    /// `MonitorTable` (`MonitorTable::jit_thin_lease`): the owner bits compiled
    /// code CASes into a mark word and the lease's `LeaseBlock` (the thin-lock
    /// count it keeps, and the inflated-monitor cache it reads).
    ///
    /// Leaves the block unarmed — every compiled site keeps its helper call —
    /// when `CRATONVM_MONITOR_FASTPATH=0`, when this registry does not know the
    /// thread, or when `lease` answers `None` (every lock slot is leased; the
    /// helpers inflate for such a thread). A block armed from ANOTHER registry
    /// is disarmed first, so compiled code can never push into a book, or
    /// count into a lease table, the helpers of this VM do not read.
    ///
    /// Self-only, like [`Self::with_self_jmx_monitors`]: the lock stack has a
    /// single writer and the caller's compiled code is that writer.
    pub(crate) fn arm_jit_monitor_block(
        &self,
        block: &mut crate::threading::jvm_thread::JitMonitorBlock,
        thread_id: ThreadId,
        lease: impl FnOnce() -> Option<crate::threading::monitor::JitThinLease>,
    ) {
        if block.is_armed_for(self.registry_id) {
            return;
        }
        block.disarm();
        if !monitor_fastpath_enabled() {
            return;
        }
        // The lease FIRST (round 11 wave 16,
        // `r11w15-lock-lease-exhaustion-and-carrier-cache-cliff` item 4): a
        // thread refused a lock slot stays unarmed and so comes back here on
        // every helper-path `monitorenter`; its refusal is answered from the
        // thread-local lease cache (`LockSlots::cached_slot`) without the
        // registry read lock, map probe and `Arc` clone below. A thread that
        // gets a lease here would have leased on this same `monitorenter`
        // anyway (`MonitorTable::enter_or_contend`).
        let Some(lease) = lease() else {
            return;
        };
        let Some(book) = self
            .threads
            .read()
            .get(&thread_id)
            .map(|e| Arc::clone(&e.jmx_monitors))
        else {
            return;
        };
        let lock_stack = &book.locked as *const JmxLockStack as usize;
        block.arm(lease, self.registry_id, book, lock_stack);
    }

    /// Run `f` against `thread_id`'s [`JmxMonitorBook`], reaching it through a
    /// per-thread cached handle rather than the registry map.
    ///
    /// Sound because the `Arc` is
    /// created once in `register_with_daemon_stw_ready` and never replaced, so
    /// the cached handle is the same allocation the map holds, forever. The
    /// cache is keyed by `(registry_id, thread_id)` — a monotonic registry id,
    /// not an address, because `ThreadId`s restart per registry and the test
    /// suite builds many registries on one OS thread. Reaping a terminated
    /// entry does not invalidate it: the `Arc` keeps the book alive, and a
    /// thread whose entry is gone is dead.
    ///
    /// Every caller is a SELF-lookup (`monitorenter` / `monitorexit` on the
    /// calling thread), which is what makes the cache hit. It is also a
    /// soundness requirement, not only a performance one: the book's
    /// [`JmxLockStack`] is single-writer, so a publish or retract on ANOTHER
    /// thread's id would race that thread's own writes.
    ///
    /// `f` runs while the thread-local cache is borrowed, so it must not call
    /// back into this method. Every caller is in this file and only touches
    /// the book's own lock stack, mutexes and counters (since round 11 wave 17
    /// the contended-entry publish, `set_jmx_contended_monitor`, and the
    /// blocked-time accounting come through here too; neither touches the
    /// lock stack, so a caller naming another thread stays sound for them).
    pub(in crate::threading::thread_registry) fn with_self_jmx_monitors<R>(
        &self,
        thread_id: ThreadId,
        f: impl FnOnce(&JmxMonitorBook) -> R,
    ) -> Option<R> {
        thread_local! {
            static CACHED: std::cell::RefCell<Option<(u64, ThreadId, Arc<JmxMonitorBook>)>> =
                const { std::cell::RefCell::new(None) };
        }
        CACHED.with(|c| {
            if monitor_fastpath_enabled() {
                let cached = c.borrow();
                if let Some((rid, tid, book)) = cached.as_ref() {
                    if *rid == self.registry_id && *tid == thread_id {
                        return Some(f(book));
                    }
                }
            }
            let book = self
                .threads
                .read()
                .get(&thread_id)
                .map(|e| Arc::clone(&e.jmx_monitors))?;
            let r = f(&book);
            if monitor_fastpath_enabled() {
                *c.borrow_mut() = Some((self.registry_id, thread_id, book));
            }
            Some(r)
        })
    }

    pub fn set_jmx_waiting_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_waiting_monitor.lock() = Some(monitor);
            *entry.jmx_wait_started.lock() = Some(Instant::now());
        }
    }

    /// The monitor `thread_id` is currently blocked in `Object.wait()` on, if
    /// any — without consuming it or closing the JMX wait-time accounting.
    ///
    /// Used by `Thread.interrupt()` to wake a target parked in `Object.wait()`.
    /// The slot is written just before the park and taken just after it, so a
    /// `Some` here means the target is (or was a moment ago) in the wait, and a
    /// wake sent to a target that has already left is harmless: `wait()` is
    /// specified to permit spurious wakeups, and the surrounding Java `while`
    /// loop re-checks and re-parks.
    pub fn peek_jmx_waiting_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.read();
        let monitor = *threads.get(&thread_id)?.jmx_waiting_monitor.lock();
        monitor
    }

    /// EVERY registered thread that is currently parked in `Object.wait()`,
    /// with the object each one is parked on.
    ///
    /// # Why a census, and not the one thread the watchdog reaches
    ///
    /// The wait-site dump reports the FIRST parked thread it finds, which on
    /// the netty `ParameterizedSslHandlerTest` stalls was always the test thread
    /// — and the test thread is parked on a promise that something *else* was
    /// supposed to complete. A dump that names only that thread cannot tell
    /// "nothing ever completed this promise" from "the thread that would have
    /// completed it is itself parked somewhere", and those two need opposite
    /// investigations. One line per waiter settles it inside the stall that is
    /// already happening, with no second run.
    ///
    /// Rows are `(tid, name, alive, waited_on, waited_ms)`, sorted by tid.
    /// `waited_ms` is `0` when the wait-start stamp is missing, so a caller
    /// never has to unwrap it. The `ObjectRef` comes from the same
    /// `jmx_waiting_monitor` slot [`Self::peek_jmx_waiting_monitor`] reads —
    /// a scanned root that `update_thread_objs_after_gc` forwards — so it is
    /// sound to dereference under a moving collector, unlike `Monitor::wait`'s
    /// own entry-time local.
    pub fn waiting_monitor_census(&self) -> Vec<(u64, String, bool, ObjectRef, u64)> {
        let threads = self.threads.read();
        let mut out: Vec<(u64, String, bool, ObjectRef, u64)> = Vec::new();
        for (tid, entry) in threads.iter() {
            let waiting = *entry.jmx_waiting_monitor.lock();
            let Some(obj) = waiting else {
                continue;
            };
            let started = *entry.jmx_wait_started.lock();
            let waited_ms = started
                .map(|t| t.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0);
            out.push((
                tid.0,
                entry.name.clone(),
                entry.alive.load(Ordering::Acquire),
                obj,
                waited_ms,
            ));
        }
        out.sort_by_key(|r| r.0);
        out
    }

    pub fn take_jmx_waiting_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        let monitor = entry.jmx_waiting_monitor.lock().take();
        if let Some(start) = entry.jmx_wait_started.lock().take() {
            entry.jmx_waited_count.fetch_add(1, Ordering::Relaxed);
            entry.jmx_waited_nanos.fetch_add(
                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
        monitor
    }

    /// Publish `monitor` as the object `thread_id` waits on for the span of a
    /// stand-in's blocking region that stands for a JDK `Object.wait` (the
    /// `--compatible` `ReferenceQueue.remove` poll stands for
    /// `lock.wait()`; interpreter round i2 wave 68, lane L1,
    /// `CRATONVM_STAND_IN_WAIT_OBJECTS`). The same slot `monitor_wait`
    /// publishes, so JMX's lock name, JVMTI's contended-monitor and waiter
    /// answers and the wait-site census read it unchanged; it is a scanned
    /// root that a moving collection forwards. Unlike
    /// [`Self::set_jmx_waiting_monitor`] it never overwrites a monitor
    /// already published and starts no wait-time accounting (a poll's
    /// slices are not the JDK's one wait). `true` when it published, which
    /// the caller pairs with exactly one
    /// [`Self::withdraw_region_waiting_monitor`].
    pub fn publish_region_waiting_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) -> bool {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return false;
        };
        let mut slot = entry.jmx_waiting_monitor.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(monitor);
        true
    }

    /// Take what [`Self::publish_region_waiting_monitor`] published, with no
    /// wait-time accounting (interpreter round i2 wave 68, lane L1).
    pub fn withdraw_region_waiting_monitor(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_waiting_monitor.lock() = None;
        }
    }

    /// `AbstractOwnableSynchronizer` has one exclusive owner. Remove a
    /// synchronizer from any former owner before attaching it to the new one.
    ///
    /// # Why this keeps a reverse index instead of scanning
    ///
    /// This runs on **every AQS ownership transition** — `acquire` passes the
    /// owner, `release` passes `null`, so it is twice per uncontended
    /// `ReentrantLock.lock()`/`unlock()` pair (censused in
    /// `probes/LockNativeCensusProbe.java`). The original body walked EVERY
    /// registered thread and took EVERY thread's `jmx_locked_synchronizers`
    /// mutex to `retain` the one it was removing, so the cost of one
    /// uncontended lock grew with the number of threads in the VM — a property
    /// no correct JVM has, and one that a single-threaded microbenchmark like
    /// `probes/AqsBreakdownProbe.java` cannot see at all.
    /// `probes/AqsOwnerScaleProbe.java` is the differential that does.
    ///
    /// `synchronizer_owner` is a derived mirror of the per-thread lists: this
    /// function is their only mutator besides the GC remap in
    /// `update_thread_objs_after_gc` (which rekeys the index in the same pass,
    /// because the key is a heap address and a moving collection relocates it)
    /// and thread reaping. So the previous owner can be looked up instead of
    /// searched for, and the transition touches at most two threads' lists.
    ///
    /// A stale index entry naming a thread that has since exited resolves to
    /// `None` in `threads.get` and is simply dropped — the same no-op the scan
    /// performed for a thread whose entry was already gone.
    /// This thread's own `jmx_locked_synchronizers` list, as a handle it can
    /// hold and reach directly.
    ///
    /// The same shape as [`Self::gc_block_state_of`], and for the same reason:
    /// a thread that needs its own entry on a hot path should not be taking the
    /// registry-wide `RwLock` to find itself in a map. `None` for a thread that
    /// is not registered, whose caller must then use the general
    /// [`Self::set_jmx_owned_synchronizer`].
    ///
    /// Fetch once and keep it: the entry for a given `ThreadId` is inserted
    /// exactly once (`next_id` is monotonic, so ids are never reused), so the
    /// handle cannot go stale while the thread lives. After the thread is
    /// reaped the handle keeps an orphaned list alive and nothing reads it,
    /// which is the correct outcome for a thread that is gone.
    pub fn jmx_locked_synchronizers_of(
        &self,
        thread_id: ThreadId,
    ) -> Option<Arc<Mutex<Vec<ObjectRef>>>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| Arc::clone(&entry.jmx_locked_synchronizers))
    }

    /// [`Self::set_jmx_owned_synchronizer`] for the case AQS actually performs:
    /// the calling thread is the transition's subject, and its own list is the
    /// only one that changes.
    ///
    /// `acquire` passes `Thread.currentThread()` and `release` passes `null`,
    /// both from the owning thread, so `previous` is either `None` or the caller
    /// itself on every transition a `ReentrantLock` generates. The general form
    /// reads the registry `RwLock` and hashes the owner's `ThreadId` to find the
    /// list it already knows — measured at 45.4 ns for one acquire/release pair
    /// in `jit::helpers::jit_native_dispatch_profile`, on a path that runs twice
    /// per uncontended lock/unlock pair.
    ///
    /// A genuine cross-thread steal (`previous` naming a peer) still takes the
    /// registry route for that peer's list, so the recorded state is identical
    /// either way — this only removes a lookup from the case that does not need
    /// one. `own_list` must be the handle [`Self::jmx_locked_synchronizers_of`]
    /// returned for `self_id`; passing any other list would silently split the
    /// record the JMX snapshot reads from the one this writes.
    pub fn set_jmx_owned_synchronizer_own(
        &self,
        self_id: ThreadId,
        own_list: &Mutex<Vec<ObjectRef>>,
        owner: Option<ThreadId>,
        synchronizer: ObjectRef,
    ) {
        debug_assert!(
            owner.is_none() || owner == Some(self_id),
            "set_jmx_owned_synchronizer_own is for a self-owned transition; owner={owner:?} self={self_id:?}"
        );
        let key = synchronizer.as_ptr() as usize;
        let previous = {
            let mut index = self.synchronizer_owner[synchronizer_owner_shard_index(key)].lock();
            match owner {
                Some(owner) => index.insert(key, owner),
                None => index.remove(&key),
            }
        };
        match previous {
            // Ownership was stolen from a peer without it releasing. Not a
            // shape AQS produces; keep it exact rather than fast.
            Some(previous) if previous != self_id => {
                {
                    let threads = self.threads.read();
                    if let Some(entry) = threads.get(&previous) {
                        entry
                            .jmx_locked_synchronizers
                            .lock()
                            .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                    }
                }
                if owner == Some(self_id) {
                    own_list.lock().push(synchronizer);
                }
            }
            // The caller already held it: a release clears it, and a repeated
            // acquire (a reentrant setter re-run) must not double-count.
            Some(_) => {
                if owner.is_none() {
                    own_list
                        .lock()
                        .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                }
            }
            None => {
                if owner == Some(self_id) {
                    own_list.lock().push(synchronizer);
                }
            }
        }
    }

    pub fn set_jmx_owned_synchronizer(&self, owner: Option<ThreadId>, synchronizer: ObjectRef) {
        let key = synchronizer.as_ptr() as usize;
        let previous = {
            let mut index = self.synchronizer_owner[synchronizer_owner_shard_index(key)].lock();
            match owner {
                Some(owner) => index.insert(key, owner),
                None => index.remove(&key),
            }
        };
        let threads = self.threads.read();
        if let Some(previous) = previous {
            if Some(previous) != owner {
                if let Some(entry) = threads.get(&previous) {
                    entry
                        .jmx_locked_synchronizers
                        .lock()
                        .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                }
            }
        }
        if let Some(owner) = owner {
            // `previous == Some(owner)` means this synchronizer was already
            // recorded against this thread (a reentrant acquire re-running the
            // setter); pushing again would double-count it in the snapshot.
            if previous == Some(owner) {
                return;
            }
            if let Some(entry) = threads.get(&owner) {
                entry.jmx_locked_synchronizers.lock().push(synchronizer);
            }
        }
    }

    pub fn jmx_lock_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Option<(
        Option<ObjectRef>,
        Option<ObjectRef>,
        Vec<ObjectRef>,
        Vec<ObjectRef>,
        i64,
        i64,
        i64,
        i64,
    )> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        let snapshot = (
            *entry.jmx_monitors.contended_monitor.lock(),
            *entry.jmx_waiting_monitor.lock(),
            entry.jmx_monitors.locked.snapshot(),
            entry.jmx_locked_synchronizers.lock().clone(),
            (entry.jmx_monitors.blocked_nanos.load(Ordering::Relaxed) / 1_000_000) as i64,
            entry.jmx_monitors.blocked_count.load(Ordering::Relaxed) as i64,
            (entry.jmx_waited_nanos.load(Ordering::Relaxed) / 1_000_000) as i64,
            entry.jmx_waited_count.load(Ordering::Relaxed) as i64,
        );
        Some(snapshot)
    }

    /// Owner only, at the points that publish its frame trace: record which
    /// of `frames` locked each monitor of this thread's lock stack, for
    /// [`Self::jmx_frame_monitors`] (interpreter round i1 wave 24, lane L7).
    ///
    /// Costs one pass over the frames' two monitor fields when no frame holds
    /// a monitor, and touches neither the registry nor a lock unless a
    /// previous publication must be withdrawn (`published`, the owner's
    /// `JvmThread::jmx_frame_monitors_published`). A monitor a frame names
    /// that the lock stack does not hold (a stale record) is left out.
    pub(crate) fn publish_jmx_frame_monitors(
        &self,
        thread_id: ThreadId,
        frames: &[crate::runtime::frame::Frame],
        published: &AtomicBool,
        trace_positions: Option<(&[u32], usize)>,
        compiled_holds: &[(u32, usize)],
    ) {
        let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
        // JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): the compiled
        // activations' holds count only with the trace positions they index.
        let compiled_holds = if trace_positions.is_some() {
            compiled_holds
        } else {
            &[]
        };
        if pairs.is_empty() && compiled_holds.is_empty() && !published.load(Ordering::Relaxed) {
            return;
        }
        published.store(
            !pairs.is_empty() || !compiled_holds.is_empty(),
            Ordering::Relaxed,
        );
        let frame_count = frames.len();
        self.with_self_jmx_monitors(thread_id, |book| {
            // SYNC619-2: a publication made only of compiled holds is made
            // only when one of them IS a lock-stack object; otherwise the
            // thread publishes nothing, as before the wave (the reader's
            // "no attribution" answer, not an empty one).
            let keys = if pairs.is_empty() && !compiled_holds.is_empty() {
                let keys = book.locked.snapshot();
                let any = compiled_holds
                    .iter()
                    .any(|&(_, word)| keys.iter().any(|k| k.as_ptr() as usize == word));
                any.then_some(keys)
            } else {
                None
            };
            if pairs.is_empty() && keys.is_none() {
                published.store(false, Ordering::Relaxed);
                if book.frame_monitors_published.load(Ordering::Relaxed) {
                    *book.frame_monitors.lock() = FrameMonitorAttribution::default();
                    book.frame_monitors_published
                        .store(false, Ordering::Release);
                }
                return;
            }
            let keys = keys.unwrap_or_else(|| book.locked.snapshot());
            let entries: Vec<(u32, u32)> = pairs
                .iter()
                .filter_map(|&(frame, obj)| {
                    let slot = keys.iter().position(|&k| k == obj)?;
                    Some((u32::try_from(frame).ok()?, u32::try_from(slot).ok()?))
                })
                .collect();
            // The published trace interleaves compiled frames (wave 38, lane
            // L7): a reader of the whole trace needs each frame's index in it.
            let (trace_count, trace_entries) = match trace_positions {
                Some((positions, trace_len)) if positions.len() == frame_count => (
                    trace_len,
                    merge_compiled_hold_entries(
                        entries
                            .iter()
                            .filter_map(|&(frame, slot)| {
                                Some((*positions.get(usize::try_from(frame).ok()?)?, slot))
                            })
                            .collect(),
                        compiled_holds,
                        &keys,
                        trace_len,
                    ),
                ),
                _ => (0, Vec::new()),
            };
            if crate::runtime::env_cache::frame_trace() {
                // JIT round 25: which publication a reader will find.
                eprintln!(
                    "[JMX_PUBLISH] tid={} frames={frame_count} trace={trace_count} \
                     entries={entries:?} trace_entries={trace_entries:?} compiled_holds={}",
                    thread_id.0,
                    compiled_holds.len()
                );
            }
            *book.frame_monitors.lock() = FrameMonitorAttribution {
                frame_count,
                lock_len: keys.len(),
                entries,
                trace_count,
                trace_entries,
            };
            book.frame_monitors_published.store(true, Ordering::Release);
        });
    }

    /// JIT round 19 wave 9, lane gc9 (`CRATONVM_JMX_HELD_NATIVE_FRAME_MONITORS`):
    /// [`Self::publish_jmx_frame_monitors`] made by a frame-trace pause on
    /// behalf of `thread_id`, a thread it holds excused in a native
    /// (`gc_and_alloc::publish_excused_native_traces`), for the trace of
    /// `trace_len` entries it just stored, `positions` naming each of
    /// `frames`' entry index. `compiled_holds` (JIT round 19 wave 11, lane
    /// gc11, GC920-4): the held thread's compiled activations' own holds as
    /// `(trace index, address)` candidates
    /// (`held_monitors::held_thread_compiled_holds`; empty unless
    /// `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`), kept only when one IS a
    /// lock-stack object, as the owner's own publish keeps them.
    ///
    /// The owner's `JvmThread::jmx_frame_monitors_published` is the flag the
    /// owner's own next publish reads to decide whether a publication must be
    /// withdrawn, so it is set here through the registry's published
    /// `JvmThread` address -- a field-level atomic, never a reference to the
    /// whole thread. Nothing is published when the address is unknown.
    ///
    /// Sound as the pause's trace read is: the excusal holds the thread in its
    /// native, where it neither runs Java nor changes `frames` or its lock
    /// stack's frame records, and cannot exit (so the `JvmThread` stays).
    pub(crate) fn publish_held_thread_frame_monitors(
        &self,
        thread_id: ThreadId,
        frames: &[crate::runtime::frame::Frame],
        positions: &[u32],
        trace_len: usize,
        compiled_holds: &[(u32, usize)],
    ) {
        let addr = self
            .threads
            .read()
            .get(&thread_id)
            .map_or(0, |e| e.jvm_thread_addr.load(Ordering::Acquire));
        if addr == 0 {
            return;
        }
        let thread = addr as *const crate::threading::jvm_thread::JvmThread;
        // SAFETY: see above -- the held thread's `JvmThread` is alive for the
        // pause; only the atomic field is borrowed (`addr_of!` makes no
        // reference to the rest of the thread, which its owner may hold
        // `&mut` across the native).
        let published: &AtomicBool =
            unsafe { &*std::ptr::addr_of!((*thread).jmx_frame_monitors_published) };
        self.publish_jmx_frame_monitors(
            thread_id,
            frames,
            published,
            Some((positions, trace_len)),
            compiled_holds,
        );
    }

    /// Inside a frame-trace pause, by the thread that publishes `thread_id`'s
    /// trace of `trace_len` entries (the thread itself at its poll, or the
    /// pause for a held one): record the lock stack as of now with the pause's
    /// generation `gen` (JIT round 19 wave 12, lane gc12,
    /// `CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`; [`TraceLockRecord`]). A pause
    /// path only -- a blocking deposit records nothing -- so its cost is one
    /// lock-stack snapshot per thread per reader-requested pause.
    pub(crate) fn record_trace_locks(
        &self,
        thread_id: ThreadId,
        trace_len: usize,
        gen: u64,
        moving_gen: u64,
    ) {
        let Some(book) = self
            .threads
            .read()
            .get(&thread_id)
            .map(|e| Arc::clone(&e.jmx_monitors))
        else {
            return;
        };
        let keys = book
            .locked
            .snapshot()
            .iter()
            .map(|o| o.as_ptr() as usize)
            .collect();
        let seq = book.trace_seq.fetch_add(1, Ordering::AcqRel) + 1;
        *book.trace_locks.lock() = Some(TraceLockRecord {
            gen,
            moving_gen,
            trace_len,
            keys,
            seq,
        });
    }

    /// Owner only, at a blocking deposit that replaced its frame trace (JIT
    /// round 25 wave 6, lane W6-F fix 4): the [`TraceLockRecord`] a pause's
    /// LIVE publish left no longer describes the published trace
    /// ([`Self::live_trace_locks`] answers `None`). The record itself is kept
    /// for [`Self::trace_locks`]' readers, as before fix 3 (which cleared it,
    /// and regressed `Object.wait` samples).
    pub(crate) fn note_trace_deposit(&self, thread_id: ThreadId) {
        self.with_self_jmx_monitors(thread_id, |book| {
            book.trace_seq.fetch_add(1, Ordering::AcqRel);
        });
    }

    /// Owner-only fast snapshot of the thread's inline JMX lock stack for
    /// `InNativeRepeat`. Returns `None` if the lock stack has spilled to the
    /// overflow mutex or a write is mid-flight.
    pub(crate) fn jmx_lock_stack_repeat_state(
        &self,
        thread_id: ThreadId,
    ) -> Option<(usize, [usize; JMX_LOCK_STACK_SLOTS])> {
        match self.with_self_jmx_monitors(thread_id, |book| {
            if book.locked.spilled.load(Ordering::Relaxed)
                || (book.locked.seq.load(Ordering::Relaxed) & 1) != 0
            {
                return None;
            }
            let top = book.locked.top.load(Ordering::Relaxed).min(JMX_LOCK_STACK_SLOTS);
            let mut slots = [0usize; JMX_LOCK_STACK_SLOTS];
            for (dst, src) in slots.iter_mut().zip(book.locked.slots.iter()).take(top) {
                *dst = src.load(Ordering::Relaxed);
            }
            Some((top, slots))
        }) {
            Some(state) => state,
            None => Some((0, [0usize; JMX_LOCK_STACK_SLOTS])),
        }
    }

    /// Owner-only snapshot of the published `FrameMonitorAttribution`, if any.
    pub(crate) fn jmx_frame_monitors_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Option<FrameMonitorAttribution> {
        self.with_self_jmx_monitors(thread_id, |book| {
            book.frame_monitors_published
                .load(Ordering::Relaxed)
                .then(|| book.frame_monitors.lock().clone())
        })
        .flatten()
    }

    /// Owner-only restore of a previously captured `FrameMonitorAttribution`.
    pub(crate) fn restore_jmx_frame_monitors(
        &self,
        thread_id: ThreadId,
        published: &AtomicBool,
        attr: Option<&FrameMonitorAttribution>,
    ) {
        match attr {
            Some(attr) => {
                published.store(true, Ordering::Relaxed);
                self.with_self_jmx_monitors(thread_id, |book| {
                    *book.frame_monitors.lock() = attr.clone();
                    book.frame_monitors_published.store(true, Ordering::Release);
                });
            }
            None => {
                published.store(false, Ordering::Relaxed);
                self.with_self_jmx_monitors(thread_id, |book| {
                    if book.frame_monitors_published.load(Ordering::Relaxed) {
                        *book.frame_monitors.lock() = FrameMonitorAttribution::default();
                        book.frame_monitors_published.store(false, Ordering::Release);
                    }
                });
            }
        }
    }

    /// JIT round 25 wave 9, lane W9-1: [`JmxMonitorBook::trace_seq`] of
    /// `thread_id` now -- bumped by every write of its published frame trace
    /// (a pause's live or excused publish, a blocking deposit) -- or `None`
    /// for an unknown thread. A reader compares two reads to tell whether the
    /// trace was republished in between (`vm_exec::held_trace_is_stale`).
    pub(crate) fn trace_seq(&self, thread_id: ThreadId) -> Option<u64> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.jmx_monitors.trace_seq.load(Ordering::Acquire))
    }

    /// W6-F fix 4: `thread_id`'s [`TraceLockRecord`] when it describes the
    /// trace published NOW -- the live one a frame-trace pause made the
    /// thread publish while it ran, not replaced by a deposit since.
    pub(crate) fn live_trace_locks(&self, thread_id: ThreadId) -> Option<TraceLockRecord> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        let record = book.trace_locks.lock().clone()?;
        (record.seq == book.trace_seq.load(Ordering::Acquire)).then_some(record)
    }

    /// Any thread: `thread_id`'s [`TraceLockRecord`], if any.
    pub(crate) fn trace_locks(&self, thread_id: ThreadId) -> Option<TraceLockRecord> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        let record = book.trace_locks.lock().clone();
        record
    }

    /// [`Self::jmx_frame_monitors`] resolved against `keys` -- the lock stack
    /// a [`TraceLockRecord`] kept with the trace -- instead of the current one
    /// (JIT round 19 wave 12, lane gc12): the publication must describe a
    /// trace of `frame_count` entries and a lock stack of `keys.len()`.
    pub(crate) fn jmx_frame_monitors_against(
        &self,
        thread_id: ThreadId,
        frame_count: usize,
        keys: &[ObjectRef],
    ) -> Option<Vec<(usize, ObjectRef)>> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        if !book.frame_monitors_published.load(Ordering::Acquire) {
            return None;
        }
        let attribution = book.frame_monitors.lock();
        let entries = if attribution.frame_count == frame_count {
            &attribution.entries
        } else if attribution.trace_count != 0 && attribution.trace_count == frame_count {
            &attribution.trace_entries
        } else {
            return None;
        };
        if keys.len() != attribution.lock_len {
            return None;
        }
        entries
            .iter()
            .map(|&(frame, slot)| {
                let obj = *keys.get(usize::try_from(slot).ok()?)?;
                Some((usize::try_from(frame).ok()?, obj))
            })
            .collect()
    }

    /// Any thread: `thread_id`'s published frame attribution as `(frame index
    /// from the bottom, object)` pairs, resolved against its CURRENT lock stack
    /// (whose slots a moving collection remaps in place). `None` -- report
    /// without attribution -- when nothing is published, when the publication
    /// belongs to a trace of a different length than the `frame_count` the
    /// caller is reporting, or when the lock stack changed length since.
    pub fn jmx_frame_monitors(
        &self,
        thread_id: ThreadId,
        frame_count: usize,
    ) -> Option<Vec<(usize, ObjectRef)>> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        if !book.frame_monitors_published.load(Ordering::Acquire) {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!("[JMX_FRAME_MONITORS] tid={} nothing published", thread_id.0);
            }
            return None;
        }
        let attribution = book.frame_monitors.lock();
        // Indexed by interpreter frame (the debugger's reader, and a trace
        // with no compiled frames), or into the whole published trace when it
        // interleaves compiled ones (wave 38, lane L7).
        let entries = if attribution.frame_count == frame_count {
            &attribution.entries
        } else if attribution.trace_count != 0 && attribution.trace_count == frame_count {
            &attribution.trace_entries
        } else {
            if crate::runtime::env_cache::frame_trace() {
                // Round 19 wave 9 (lane gc9): why a published attribution
                // was not used.
                eprintln!(
                    "[JMX_FRAME_MONITORS] tid={} asked={frame_count} published frames={} trace={}",
                    thread_id.0, attribution.frame_count, attribution.trace_count
                );
            }
            return None;
        };
        let keys = book.locked.snapshot();
        if keys.len() != attribution.lock_len {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[JMX_FRAME_MONITORS] tid={} lock stack {} != published {}",
                    thread_id.0,
                    keys.len(),
                    attribution.lock_len
                );
            }
            return None;
        }
        entries
            .iter()
            .map(|&(frame, slot)| {
                let obj = *keys.get(usize::try_from(slot).ok()?)?;
                Some((usize::try_from(frame).ok()?, obj))
            })
            .collect()
    }

    /// JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`):
    /// [`Self::jmx_frame_monitors`] for a reader that holds a TRACE of
    /// `trace_len` entries (another thread's published trace with stand-in
    /// rows stripped from its top): the publication must index a trace of
    /// that length -- its `trace_entries` when it interleaves compiled
    /// frames, else its per-frame `entries` when the trace is one entry per
    /// interpreter frame. Never the per-frame list of an interleaved trace
    /// whose interpreter-frame count merely equals `trace_len` (another index
    /// space).
    pub(crate) fn jmx_trace_monitors(
        &self,
        thread_id: ThreadId,
        trace_len: usize,
    ) -> Option<Vec<(usize, ObjectRef)>> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        if !book.frame_monitors_published.load(Ordering::Acquire) {
            return None;
        }
        let attribution = book.frame_monitors.lock();
        let entries = if attribution.trace_count != 0 {
            (attribution.trace_count == trace_len).then_some(&attribution.trace_entries)?
        } else {
            (attribution.frame_count == trace_len).then_some(&attribution.entries)?
        };
        let keys = book.locked.snapshot();
        if keys.len() != attribution.lock_len {
            return None;
        }
        entries
            .iter()
            .map(|&(frame, slot)| {
                let obj = *keys.get(usize::try_from(slot).ok()?)?;
                Some((usize::try_from(frame).ok()?, obj))
            })
            .collect()
    }

    pub fn reset_jmx_contention_stats(&self) {
        for entry in self.threads.read().values() {
            entry.jmx_monitors.blocked_count.store(0, Ordering::Relaxed);
            entry.jmx_monitors.blocked_nanos.store(0, Ordering::Relaxed);
            entry.jmx_waited_count.store(0, Ordering::Relaxed);
            entry.jmx_waited_nanos.store(0, Ordering::Relaxed);
            *entry.jmx_monitors.contended_started.lock() = None;
            *entry.jmx_wait_started.lock() = None;
        }
    }

    /// Every registered thread's thread-dump facts, in registry id order
    /// (the order the threads registered; HotSpot lists its threads in the
    /// order they were added). Interpreter round i3 wave 81, lane L6: one
    /// registry read, cold (`jcmd Thread.print`, `jstack`, `SIGQUIT`).
    pub fn thread_dump_entries(&self) -> Vec<ThreadDumpEntry> {
        let mut out: Vec<ThreadDumpEntry> = self
            .threads
            .read()
            .iter()
            .map(|(tid, e)| ThreadDumpEntry {
                thread_id: *tid,
                name: e.name.clone(),
                alive: e.alive.load(Ordering::Acquire),
                daemon: e.daemon.load(Ordering::Acquire),
                java_thread_obj: e.java_thread_obj,
                java_tid: Some(e.java_tid).filter(|t| *t != 0),
                os_tid: Some(e.os_tid.load(Ordering::Acquire)).filter(|t| *t != 0),
                registered_at: e.registered_at,
                jvm_thread_addr: e.jvm_thread_addr.load(Ordering::Acquire),
                blocked_wait_lo: e.gc_block_state.blocked_wait_lo.load(Ordering::Acquire),
            })
            .collect();
        out.sort_by_key(|e| e.thread_id.0);
        out
    }

    /// Return all (ThreadId, name) pairs for currently registered threads.
    pub fn all_thread_names(&self) -> Vec<(ThreadId, String)> {
        self.threads
            .read()
            .iter()
            .map(|(tid, entry)| (*tid, entry.name.clone()))
            .collect()
    }

    /// Set the park state for a given ThreadId (used when spawning threads
    /// to share the JvmThread's ParkState with the registry).
    pub fn set_park_state(&self, thread_id: ThreadId, state: Arc<ParkState>) {
        // Wave 73 (`CRATONVM_PARK_ATOMIC_PERMIT`): before the state is
        // reachable by an unpark through this registry.
        if self.park_atomic_permit {
            state.enable_lock_free();
        }
        let obj_opt;
        {
            let mut threads = self.threads.write();
            if let Some(entry) = threads.get_mut(&thread_id) {
                entry.park_state = state.clone();
                obj_opt = entry.java_thread_obj;
            } else {
                return;
            }
        }
        // WP4.1 — keep reverse index pointing at the new ParkState so
        // unpark(Thread) hits the same instance the parked thread is
        // blocked on.
        if let Some(obj) = obj_opt {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, state);
        }
    }

    /// Get the park state for a given ThreadId (for unpark from another thread).
    pub fn get_park_state(&self, thread_id: ThreadId) -> Option<Arc<ParkState>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.park_state.clone())
    }

    /// The `ParkState` and interrupt flag a thread being started adopts
    /// (interpreter round i3 wave 74, lane L6; page
    /// `i73-L6-unpark-in-the-thread-start-window-is-lost-20261106`): the ones
    /// its registry entry already has, so an `unpark` (or an interrupt) that
    /// reached the entry between its registration and the start's
    /// `set_park_state` -- the `java_tid` index and `eetop` are live from
    /// early in `start0` -- lands on the state the thread will park on, as
    /// HotSpot's `Unsafe_Unpark` reaches the new thread's `Parker` once
    /// `JavaThread::prepare` has run. Fresh ones only for an unregistered id.
    pub fn starting_thread_states(&self, thread_id: ThreadId) -> (Arc<ParkState>, Arc<AtomicBool>) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            return (entry.park_state.clone(), entry.interrupted.clone());
        }
        let park_state = Arc::new(ParkState::new());
        if self.park_atomic_permit {
            park_state.enable_lock_free();
        }
        (park_state, Arc::new(AtomicBool::new(false)))
    }

    /// Every `Arc` `thread_id`'s entry shares with its `JvmThread` (park
    /// state, interrupt flag, root snapshot, frame trace, VM-state
    /// breadcrumb, blocked-region GC state), cloned under ONE `threads` read
    /// lock (`CRATONVM_THREAD_START_ONE_PUBLISH`, interpreter round i3 wave
    /// 82, lane L6). The starting thread adopts them
    /// (`JvmThread::with_shared`), so its entry is complete from
    /// registration on -- as HotSpot's `Threads::add` publishes a prepared
    /// thread once -- and no `set_*` write lock re-installs them. `None` for
    /// an unregistered id.
    pub(crate) fn starting_thread_shared(
        &self,
        thread_id: ThreadId,
    ) -> Option<crate::threading::jvm_thread::ThreadSharedArcs> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        Some(crate::threading::jvm_thread::ThreadSharedArcs {
            park_state: entry.park_state.clone(),
            interrupted: entry.interrupted.clone(),
            root_snapshot: entry.root_snapshot.clone(),
            frame_trace: entry.frame_trace.clone(),
            vm_state: entry.vm_state.clone(),
            gc_block_state: entry.gc_block_state.clone(),
        })
    }

    /// Find the park state for a thread identified by its Java Thread object.
    /// Used by `Unsafe.unpark(Thread)` and `LockSupport.unpark(Thread)`.
    ///
    /// WP4.1 — O(1) via the reverse index. Falls back to a linear walk
    /// of `threads` only if the index lookup misses, which should never
    /// happen for a thread that has been properly registered with a
    /// non-null `java_thread_obj`. The fallback exists to keep tests
    /// that construct registries by hand (without the modern register
    /// path) working.
    pub fn find_park_state_by_thread_obj(&self, obj: ObjectRef) -> Option<Arc<ParkState>> {
        if let Some(ps) = self.thread_obj_to_park.lock().get(&(obj.as_ptr() as usize)) {
            return Some(ps.clone());
        }
        // Fallback: legacy registries that bypassed register_with_daemon.
        let threads = self.threads.read();
        for entry in threads.values() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr()) {
                    return Some(entry.park_state.clone());
                }
            }
        }
        None
    }

    /// DBG (`CRATONVM_DBG_UNPARK_MISS`): snapshot of every registered
    /// thread's `(tid, java_thread_obj address, alive)` so an unpark whose
    /// mirror lookup missed can print what the registry believes the live
    /// mirror addresses are (a stale `Node.waiter` shows up as a caller
    /// address absent from this list).
    pub fn debug_thread_obj_addrs(&self) -> Vec<(u64, usize, bool)> {
        let threads = self.threads.read();
        threads
            .iter()
            .map(|(id, e)| {
                (
                    id.0,
                    e.java_thread_obj.map_or(0, |o| o.as_ptr() as usize),
                    e.alive.load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// WP4.1 — Find the `ThreadId` for a registered thread by its Java
    /// `Thread` mirror object.  Real-JDK-mode `Thread` objects do not have
    /// a single fixed slot we can write a `ThreadId` into (the class layout
    /// stores `tid` inside an inner `FieldHolder`), so callers that used
    /// to read field 2 directly must instead walk the registry.
    ///
    /// Pointer-identity comparison matches the existing
    /// `find_park_state_by_thread_obj` helper.
    pub fn find_thread_id_by_thread_obj(&self, obj: ObjectRef) -> Option<ThreadId> {
        let threads = self.threads.read();
        for (id, entry) in threads.iter() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr()) {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Set the interrupted flag for a thread (cross-thread interrupt).
    pub fn set_interrupted(&self, thread_id: ThreadId, value: bool) {
        let woken = {
            let threads = self.threads.read();
            let Some(entry) = threads.get(&thread_id) else {
                return;
            };
            // SeqCst (round 14 wave 5, lane monitor2, RV5-1): with the
            // `fence(SeqCst)` in `MonitorTable::wake_waiter_for_interrupt` and
            // the SeqCst enrolment re-check in `Monitor::wait`, a waiter that
            // inflates its monitor after the interrupter read the mark thin
            // still sees this flag (the store-buffering pattern needs SeqCst).
            entry
                .interrupted
                .store(value, std::sync::atomic::Ordering::SeqCst);
            // Interpreter round i3 wave 74, lane L6
            // (`CRATONVM_PARK_UNTIMED_NO_POLL`, default off): a park no
            // longer polls the flag, so raising it wakes the park (after the
            // `threads` lock is dropped).
            // Wave 75 (`CRATONVM_SLEEP_NO_POLL`): a sleep waits on the same
            // state's condvar, so the same wake ends it.
            (value && (self.park_no_poll || self.sleep_no_poll)).then(|| entry.park_state.clone())
        };
        if let Some(park_state) = woken {
            // Wave 76 (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`): the wake after the
            // state's mutex is released (`ParkState::unpark_after_unlock`).
            park_state.wake_for_interrupt_with(self.park_wake_after_unlock);
        }
    }

    /// [`Self::set_interrupted`]'s wake of a raised interrupt
    /// (`CRATONVM_PARK_UNTIMED_NO_POLL` / `CRATONVM_SLEEP_NO_POLL`), WITHOUT
    /// the flag store: for `Thread.interrupt`, whose flag was already raised
    /// with the Java field under the target's interrupt lock
    /// (`vm_exec::raise_thread_interrupt`, nio21 wave 5 L1). Merged at nio21.
    pub fn wake_raised_interrupt(&self, thread_id: ThreadId) {
        if !(self.park_no_poll || self.sleep_no_poll) {
            return;
        }
        let woken = self
            .threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.park_state.clone());
        if let Some(park_state) = woken {
            park_state.wake_for_interrupt_with(self.park_wake_after_unlock);
        }
    }

    /// Raise a thread's interrupt flag WITHOUT waking it, and hand back its
    /// `ParkState` and the monitor it waits on in `Object.wait()` (the JMX
    /// record [`Self::peek_jmx_waiting_monitor`] reads), all under ONE
    /// `threads` read (interpreter round i3 wave 76, lane L6;
    /// `CRATONVM_INTERRUPT_ONE_LOOKUP` and `CRATONVM_PARK_WAKE_AFTER_UNLOCK`).
    /// `Thread.interrupt` wakes the state itself afterwards: with
    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK` once
    /// (`ParkState::interrupt_wake_after_unlock`: the permit and every
    /// waiter, park or sleep, in one trip through the state's mutex, where
    /// the default takes it twice -- the wake of [`Self::set_interrupted`] and
    /// the unpark after it); otherwise with [`Self::set_interrupted`]'s wake
    /// and the unpark, as before. The flag store is
    /// [`Self::set_interrupted`]'s (`SeqCst`, same reason). `None`: no such
    /// thread (nothing was raised).
    ///
    /// nio21 merge: `raise == false` hands back the same pair WITHOUT the
    /// flag store, for a caller that already raised the flag together with
    /// the Java field under the target's interrupt lock
    /// (`vm_exec::raise_thread_interrupt`, nio21 wave 5 L1); a second store
    /// after the lock is released could re-raise an interrupt a consume took
    /// in between and split the flag from the field again.
    pub fn raise_interrupt(
        &self,
        thread_id: ThreadId,
        raise: bool,
    ) -> Option<(Arc<ParkState>, Option<ObjectRef>)> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if raise {
            entry
                .interrupted
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let monitor = *entry.jmx_waiting_monitor.lock();
        Some((entry.park_state.clone(), monitor))
    }

    /// Get the interrupted flag Arc for a thread (to share with JvmThread).
    pub fn get_interrupted_flag(&self, thread_id: ThreadId) -> Option<Arc<AtomicBool>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.interrupted.clone())
    }

    /// Set the interrupted flag Arc for a thread (share JvmThread's flag with registry).
    pub fn set_interrupted_flag(&self, thread_id: ThreadId, flag: Arc<AtomicBool>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.interrupted = flag;
        }
    }

    /// Set the root snapshot Arc for a thread (share JvmThread's snapshot with registry).
    /// The objects thread `thread_id` last deposited as its roots (the
    /// C JVMTI table's heap walk reports them as the thread's stack
    /// references; interpreter round i2 wave 63, lane L1). Empty for an
    /// unknown thread.
    pub fn root_snapshot_of(&self, thread_id: ThreadId) -> Vec<ObjectRef> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.root_snapshot.lock().clone())
            .unwrap_or_default()
    }

    pub fn set_root_snapshot(&self, thread_id: ThreadId, snapshot: Arc<Mutex<Vec<ObjectRef>>>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.root_snapshot = snapshot;
            // gci i12/vp: a different snapshot; the parked fold filter is stale.
            entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Share the JvmThread's frame-trace Arc with the registry, so other threads
    /// can read this thread's published call stack.
    pub fn set_frame_trace(
        &self,
        thread_id: ThreadId,
        trace: Arc<Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    ) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.frame_trace = trace;
        }
    }

    /// Share the JvmThread's VM-state breadcrumb with the registry.
    pub fn set_vm_state(&self, thread_id: ThreadId, state: Arc<Mutex<String>>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.vm_state = state;
        }
    }

    /// Read a copy of `thread_id`'s last-published frame trace (call stack),
    /// innermost frame first. Empty if the thread is unknown or never deposited.
    ///
    /// **Every entry has `line_number == -1`.** The depositor
    /// (`stackwalker::capture_frames_no_lines`, called from every blocking
    /// deposit point) is deliberately lock-free and takes no `ClassStore`
    /// borrow, so it cannot resolve lines. Prefer
    /// [`frame_trace_of_resolved`](Self::frame_trace_of_resolved) at any call
    /// site that has a `ClassStore` in hand — a thread dump then gains source
    /// lines it does not have today, at zero cost to the deposit path.
    pub fn frame_trace_of(&self, thread_id: ThreadId) -> Vec<cratonvm_native_api::StackTraceEntry> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.frame_trace.lock().clone())
            .unwrap_or_default()
    }

    /// gcg g3/k (`common-a-proposal-per-thread-handshakes`, step 1): ask
    /// `thread_id` to run `op` on itself, without a pause.
    ///
    /// * the thread is unknown or dead: [`HandshakeRequest::Gone`], `op`
    ///   handed back;
    /// * its `in_blocked_region` is up: [`HandshakeRequest::TargetBlocked`],
    ///   `op` handed back -- the target executes no code and its deposit
    ///   (root snapshot, frame trace) describes its frames, so the requester
    ///   works from those (the parked fast path cross-thread
    ///   `getStackTrace()` already takes);
    /// * otherwise the op is queued on the target's [`GcBlockState`] and the
    ///   ticket returned: the target runs it at its next
    ///   `JvmThread::run_pending_handshakes`. A requester that stops waiting
    ///   (the target parked meanwhile, or never polls) takes it back with
    ///   `state.handshakes.withdraw(&ticket)` -- on the SAME `GcBlockState`
    ///   it was queued on, which the variant carries (the entry's state can be
    ///   replaced by `set_gc_block_state` meanwhile) -- and falls back to
    ///   whatever it did before (the blocked path, or a pause), so the op
    ///   runs exactly once or not at all.
    ///
    /// Classified under the registry read lock, so the entry cannot be
    /// removed in between; the blocked flag can still rise right after the
    /// read, which the ticket's timeout and the withdrawal cover.
    pub fn request_handshake(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
    ) -> HandshakeRequest {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return HandshakeRequest::Gone(op);
        };
        if !entry.alive.load(Ordering::Acquire) || entry.gc_block_state.owner_dropped() {
            return HandshakeRequest::Gone(op);
        }
        if entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
        {
            return HandshakeRequest::TargetBlocked(op);
        }
        let ticket = entry.gc_block_state.handshakes.push(op);
        HandshakeRequest::Queued {
            state: Arc::clone(&entry.gc_block_state),
            ticket,
        }
    }

}
