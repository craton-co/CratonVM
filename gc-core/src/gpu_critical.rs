use std::sync::atomic::Ordering;

pub static GPU_CRITICAL_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(feature = "gpu-offload")]
pub mod gpu_coordination {
    use cratonvm_types::ObjectRef;
    use parking_lot::{Mutex, RwLock};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Hook implemented by the GPU runtime crate (`cuda-bridge`, wired by `vm`)
    /// so `gc` coordinates moving cycles without depending on `cuda-bridge`.
    pub trait GpuCriticalBridge: Send + Sync {
        fn collector_wait_budget(&self) -> Duration;
        fn record_forced_non_moving_collection(&self);
        fn relocation_forbidden(&self) -> bool;
        fn relocation_forbidden_for_vm(&self, scope_vm: Option<u64>) -> bool;
        fn outstanding(&self) -> usize;
        fn outstanding_for_vm(&self, scope_vm: Option<u64>) -> usize;
        /// Returns `true` if drained within `budget`, `false` if timed out.
        fn wait_for_relocation_clearance(&self, budget: Duration) -> bool;
        fn wait_for_relocation_clearance_for_vm(
            &self,
            scope_vm: Option<u64>,
            budget: Duration,
        ) -> bool;
        fn begin_moving_cycle(&self);
        fn try_begin_moving_cycle(&self) -> bool;
        fn try_begin_moving_cycle_for_vm(&self, _scope_vm: Option<u64>) -> bool {
            self.try_begin_moving_cycle()
        }
        fn end_moving_cycle(&self);
        fn end_moving_cycle_for_vm(&self, _scope_vm: Option<u64>) {
            self.end_moving_cycle();
        }
        fn outstanding_keepalive_addrs(&self) -> Vec<usize>;
        fn outstanding_keepalive_addrs_for_vm(&self, scope_vm: Option<u64>) -> Vec<usize>;
        fn remap_keepalive(&self, remap: &dyn Fn(usize) -> Option<usize>);
    }

    type PressureCollectorEntry = (u64, Arc<dyn Fn() -> bool + Send + Sync>);
    type HeapUnpinHook = Arc<dyn Fn() + Send + Sync>;
    enum SpanUnpinCallback {
        Once(Box<dyn FnOnce() + Send + Sync>),
        Fallible(Arc<dyn Fn() -> bool + Send + Sync>),
    }
    type SpanUnpinHook = (usize, usize, SpanUnpinCallback);
    type ZgcRelocIdleHook = Arc<dyn Fn(bool) -> Option<bool> + Send + Sync>;

    /// Per-VM (and fallback process-global `COORD`) GPU/GC coordination state.
    pub struct GpuCoordContext {
        /// Scoped VM identity (`0` = process-wide fallback / unscoped).
        owner_vm_id: AtomicU64,
        /// Set for the duration of a cycle that must not relocate.
        relocation_forbidden: AtomicBool,
        /// Active cycles within this coordination domain that vetoed relocation.
        vetoing_cycles: AtomicUsize,
        /// Mirror of device VRAM currently in use (bytes).
        vram_used_bytes: AtomicU64,
        /// Configured VRAM reservation ceiling (bytes); `0` means unbounded.
        vram_reservation_limit: AtomicU64,
        /// Number of collections triggered under GPU VRAM pressure.
        gc_triggered_by_vram_pressure: AtomicU64,
        /// Per-VM pressure collection callbacks keyed by `vm_identity: u64`.
        pressure_collectors: Mutex<Vec<PressureCollectorEntry>>,
        /// Heap teardown unpin hooks invoked before backing storage is freed.
        pinned_unpin_hooks: Mutex<Vec<HeapUnpinHook>>,
        /// Per-span teardown unpin hooks `(base, len, hook)`.
        span_unpin_hooks: Mutex<Vec<SpanUnpinHook>>,
        /// Refcount and shared `CollectingGuard` holding `GPU_COLLECTING`.
        critical_collecting: Mutex<(usize, Option<crate::safepoint::CollectingGuard<'static>>)>,
        /// Registered weak hooks to drain in-flight ZGC concurrent relocation plans.
        zgc_relocate_idle_hooks: Mutex<Vec<ZgcRelocIdleHook>>,
        /// Process-wide bridge to `cuda-bridge` (populated on `COORD`).
        critical_bridge: RwLock<Option<Arc<dyn GpuCriticalBridge>>>,
        #[cfg(test)]
        test_lock: Mutex<()>,
    }

    impl GpuCoordContext {
        pub const fn new_const() -> Self {
            Self {
                owner_vm_id: AtomicU64::new(0),
                relocation_forbidden: AtomicBool::new(false),
                vetoing_cycles: AtomicUsize::new(0),
                vram_used_bytes: AtomicU64::new(0),
                vram_reservation_limit: AtomicU64::new(0),
                gc_triggered_by_vram_pressure: AtomicU64::new(0),
                pressure_collectors: Mutex::new(Vec::new()),
                pinned_unpin_hooks: Mutex::new(Vec::new()),
                span_unpin_hooks: Mutex::new(Vec::new()),
                critical_collecting: Mutex::new((0, None)),
                zgc_relocate_idle_hooks: Mutex::new(Vec::new()),
                critical_bridge: RwLock::new(None),
                #[cfg(test)]
                test_lock: Mutex::new(()),
            }
        }

        /// Construct an isolated per-VM GPU coordination context.
        pub fn new() -> Arc<Self> {
            Arc::new(Self::new_const())
        }

        /// Bind this context to `vm_id` (`0` resets to process-wide scope).
        pub fn set_owner_vm_id(&self, vm_id: u64) {
            self.owner_vm_id.store(vm_id, Ordering::Release);
        }

        /// Bound `vm_identity` (`None` when unscoped/`0`).
        #[inline]
        pub fn owner_vm_id(&self) -> Option<u64> {
            let id = self.owner_vm_id.load(Ordering::Acquire);
            (id != 0).then_some(id)
        }

        /// Install a bridge override directly on this context (used by unit tests).
        pub fn register_bridge(&self, bridge: Arc<dyn GpuCriticalBridge>) {
            *self.critical_bridge.write() = Some(bridge);
        }

        #[inline]
        fn bridge(&self) -> Option<Arc<dyn GpuCriticalBridge>> {
            self.critical_bridge
                .read()
                .clone()
                .or_else(|| COORD.critical_bridge.read().clone())
        }

        /// Direct references to `(vram_used_bytes, vram_reservation_limit)` atomics.
        pub fn vram_mirror_atomics(&self) -> (&AtomicU64, &AtomicU64) {
            (&self.vram_used_bytes, &self.vram_reservation_limit)
        }

        /// Current mirrored device VRAM usage in bytes.
        #[inline]
        pub fn vram_used_bytes(&self) -> u64 {
            self.vram_used_bytes.load(Ordering::Acquire)
        }

        /// Current configured VRAM reservation limit in bytes (`0` means unbounded).
        #[inline]
        pub fn vram_reservation_limit(&self) -> u64 {
            self.vram_reservation_limit.load(Ordering::Acquire)
        }

        /// Update the configured VRAM reservation limit in bytes (`0` means unbounded).
        #[inline]
        pub fn set_vram_reservation_limit(&self, limit: u64) {
            self.vram_reservation_limit.store(limit, Ordering::Release);
        }

        /// Returns `true` when `vram_reservation_limit > 0` and mirrored VRAM usage exceeds 80%.
        #[inline]
        pub fn is_vram_under_pressure(&self) -> bool {
            let limit = self.vram_reservation_limit.load(Ordering::Relaxed);
            if limit == 0 {
                return false;
            }
            let used = self.vram_used_bytes.load(Ordering::Relaxed);
            (used as u128).saturating_mul(5) > (limit as u128).saturating_mul(4)
        }

        /// Register (or replace) a VM's VRAM-pressure collection callback keyed by `vm_id`.
        pub fn register_vm_pressure_collector(
            &self,
            vm_id: u64,
            cb: Arc<dyn Fn() -> bool + Send + Sync>,
        ) {
            let mut guard = self.pressure_collectors.lock();
            if let Some((_, existing)) = guard.iter_mut().find(|(id, _)| *id == vm_id) {
                *existing = cb;
            } else {
                guard.push((vm_id, cb));
            }
        }

        /// Unregister the VRAM-pressure collection callback for `vm_id`.
        pub fn unregister_vm_pressure_collector(&self, vm_id: u64) {
            self.pressure_collectors.lock().retain(|(id, _)| *id != vm_id);
        }

        /// Trigger registered VM pressure collectors outside locks.
        pub fn collect_garbage_for_gpu_pressure(&self) -> bool {
            let collectors: Vec<Arc<dyn Fn() -> bool + Send + Sync>> = {
                let guard = self.pressure_collectors.lock();
                guard.iter().map(|(_, cb)| Arc::clone(cb)).collect()
            };
            if collectors.is_empty() {
                return false;
            }
            let mut any_collected = false;
            for cb in collectors {
                if cb() {
                    any_collected = true;
                }
            }
            if any_collected {
                self.gc_triggered_by_vram_pressure
                    .fetch_add(1, Ordering::Relaxed);
                crate::gc_metrics::record_vram_pressure_gc();
                cratonvm_types::gpu_event_census::note_gc_triggered_by_vram_pressure();
            }
            any_collected
        }

        /// Total collections recorded as triggered by GPU VRAM pressure on this context.
        #[inline]
        pub fn gc_triggered_by_vram_pressure(&self) -> u64 {
            self.gc_triggered_by_vram_pressure.load(Ordering::Acquire)
        }

        /// Register a heap teardown unpin callback.
        pub fn register_heap_unpin_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
            self.pinned_unpin_hooks.lock().push(Arc::from(hook));
        }

        /// Register a one-shot unpin callback scoped to `(base, len)`.
        pub fn register_span_unpin_hook(
            &self,
            base: usize,
            len: usize,
            hook: Box<dyn FnOnce() + Send + Sync>,
        ) {
            if base == 0 || len == 0 {
                return;
            }
            self.span_unpin_hooks
                .lock()
                .push((base, len, SpanUnpinCallback::Once(hook)));
        }

        /// Register (or replace) a fallible span unpin callback scoped to `(base, len)`.
        pub fn register_fallible_span_unpin_hook(
            &self,
            base: usize,
            len: usize,
            hook: Arc<dyn Fn() -> bool + Send + Sync>,
        ) {
            if base == 0 || len == 0 {
                return;
            }
            let mut hooks = self.span_unpin_hooks.lock();
            hooks.retain(|(b, l, cb)| {
                !(*b == base && *l == len && matches!(cb, SpanUnpinCallback::Fallible(_)))
            });
            hooks.push((base, len, SpanUnpinCallback::Fallible(hook)));
        }

        /// Invoke unpin hooks overlapping `spans` outside locks.
        pub fn run_matching_span_unpin_hooks(&self, spans: &[(usize, usize)]) -> bool {
            if spans.is_empty() {
                return true;
            }
            let matching_span_hooks: Vec<SpanUnpinHook> = {
                let mut guard = self.span_unpin_hooks.lock();
                let mut matched = Vec::new();
                let mut i = 0;
                while i < guard.len() {
                    let (h_base, h_len, _) = &guard[i];
                    let h_end = h_base.saturating_add(*h_len);
                    let belongs = spans.iter().any(|&(s_base, s_len)| {
                        let s_end = s_base.saturating_add(s_len);
                        s_base != 0 && s_len > 0 && *h_base < s_end && h_end > s_base
                    });
                    if belongs {
                        matched.push(guard.swap_remove(i));
                    } else {
                        i += 1;
                    }
                }
                matched
            };
            let mut all_unpinned = true;
            let mut retain_active = Vec::new();
            for (h_base, h_len, cb) in matching_span_hooks {
                match cb {
                    SpanUnpinCallback::Once(hook) => hook(),
                    SpanUnpinCallback::Fallible(hook) => {
                        if !hook() {
                            all_unpinned = false;
                            retain_active.push((h_base, h_len, SpanUnpinCallback::Fallible(hook)));
                        }
                    }
                }
            }
            if !retain_active.is_empty() {
                self.span_unpin_hooks.lock().extend(retain_active);
            }
            all_unpinned
        }

        /// Invoke registered heap teardown hooks and matching span unpin hooks.
        pub fn run_heap_unpin_hooks_for_spans(&self, spans: &[(usize, usize)]) {
            let global_hooks: Vec<HeapUnpinHook> = self.pinned_unpin_hooks.lock().clone();
            for hook in global_hooks {
                hook();
            }
            let _ = self.run_matching_span_unpin_hooks(spans);
            if !std::ptr::eq(self, &COORD) {
                COORD.run_heap_unpin_hooks_for_spans(spans);
            }
        }

        /// Drain and invoke all registered heap and span unpin hooks outside locks.
        pub fn run_heap_unpin_hooks(&self) {
            let global_hooks = {
                let mut guard = self.pinned_unpin_hooks.lock();
                std::mem::take(&mut *guard)
            };
            let span_hooks = {
                let mut guard = self.span_unpin_hooks.lock();
                std::mem::take(&mut *guard)
            };
            for hook in global_hooks {
                hook();
            }
            for (_, _, cb) in span_hooks {
                match cb {
                    SpanUnpinCallback::Once(hook) => hook(),
                    SpanUnpinCallback::Fallible(hook) => {
                        let _ = hook();
                    }
                }
            }
        }

        /// Register a ZGC concurrent-relocation idle drain hook on this context.
        pub fn register_zgc_relocate_idle_hook(&self, hook: ZgcRelocIdleHook) {
            self.zgc_relocate_idle_hooks.lock().push(hook);
        }

        /// Drain active ZGC concurrent relocation plans registered on this context.
        pub fn wait_for_zgc_concurrent_relocate_idle(&self) -> bool {
            let hooks: Vec<ZgcRelocIdleHook> = {
                let guard = self.zgc_relocate_idle_hooks.lock();
                if guard.is_empty() {
                    return false;
                }
                guard.clone()
            };
            let mut any_drained = false;
            let mut any_dead = false;
            for hook in &hooks {
                match hook(true) {
                    Some(drained) => any_drained |= drained,
                    None => any_dead = true,
                }
            }
            if any_dead {
                self.zgc_relocate_idle_hooks
                    .lock()
                    .retain(|h| h(false).is_some());
            }
            any_drained
        }

        /// Synchronize ZGC concurrent relocation with GPU relocation exclusion on this context.
        pub fn enter_relocation_forbidden(&self) -> bool {
            self.wait_for_zgc_concurrent_relocate_idle()
        }

        /// Whether any cycle, fallback flag, or VM-scoped critical bridge in this
        /// context forbids relocation right now.
        #[inline]
        pub fn relocation_forbidden(&self) -> bool {
            relocation_forbidden_from(
                true,
                self.vetoing_cycles.load(Ordering::Acquire),
                self.relocation_forbidden.load(Ordering::Acquire),
            ) || self
                .bridge()
                .is_some_and(|b| b.relocation_forbidden_for_vm(self.owner_vm_id()))
        }

        /// Prepare this context for a GC cycle on its owning VM/heap.
        pub fn before_collection(&self) -> CycleGuard<'_> {
            let reg = self.bridge();
            let vm_scope = self.owner_vm_id();
            if std::ptr::eq(self, &COORD) {
                withdraw_this_cycles_veto();
            }
            let budget = reg
                .as_ref()
                .map(|r| r.collector_wait_budget())
                .unwrap_or_else(collector_wait_budget);
            let deadline = std::time::Instant::now() + budget;
            let mut forbid = false;
            if let Some(reg_ref) = reg.as_ref() {
                if reg_ref.outstanding_for_vm(vm_scope) != 0
                    && !reg_ref.wait_for_relocation_clearance_for_vm(
                        vm_scope,
                        deadline.saturating_duration_since(std::time::Instant::now()),
                    )
                {
                    forbid = true;
                }
            }
            let gpu_drain = super::wait_for_gpu_critical_drain_until(deadline, false);
            if gpu_drain.timed_out {
                forbid = true;
            }
            if !forbid {
                if let Some(reg_ref) = reg.as_ref() {
                    if reg_ref.relocation_forbidden_for_vm(vm_scope)
                        || !reg_ref.try_begin_moving_cycle_for_vm(vm_scope)
                    {
                        forbid = true;
                    }
                }
            }
            let vetoed_by_this_cycle = if forbid {
                if std::ptr::eq(self, &COORD) {
                    forbid_relocation_this_cycle();
                    false
                } else {
                    self.vetoing_cycles.fetch_add(1, Ordering::AcqRel);
                    record_forced_non_moving_collection();
                    true
                }
            } else {
                false
            };
            let extra_roots = reg
                .as_ref()
                .map(|r| {
                    r.outstanding_keepalive_addrs_for_vm(vm_scope)
                        .into_iter()
                        // SAFETY: `outstanding_keepalive_addrs_for_vm` returns live heap
                        // addresses declared by outstanding tokens until collection ends.
                        .map(|addr| unsafe { ObjectRef::from_raw(addr as *mut u8) })
                        .collect()
                })
                .unwrap_or_default();
            CycleGuard {
                coord: self,
                extra_roots,
                moving_declared: !forbid && reg.is_some(),
                vetoed_by_this_cycle,
                _gpu_drain: Some(gpu_drain),
            }
        }
    }

    static COORD: GpuCoordContext = GpuCoordContext::new_const();

    std::thread_local! {
        /// Legacy TLS flag for process-global `forbid_relocation_this_cycle()`.
        static THIS_CYCLE_VETOED: Cell<bool> = const { Cell::new(false) };
    }

    /// Register the process-wide GPU critical-section bridge (idempotent).
    pub fn register_gpu_critical_bridge(bridge: Arc<dyn GpuCriticalBridge>) {
        let mut guard = COORD.critical_bridge.write();
        if guard.is_none() {
            *guard = Some(bridge);
        }
    }

    /// Returns `true` once a GPU critical-section bridge has been registered.
    #[inline]
    pub fn has_gpu_critical_bridge() -> bool {
        COORD.critical_bridge.read().is_some()
    }

    pub fn collector_wait_budget() -> Duration {
        COORD
            .critical_bridge
            .read()
            .as_ref()
            .map(|b| b.collector_wait_budget())
            .unwrap_or_else(|| Duration::from_millis(50))
    }

    pub(super) fn record_forced_non_moving_collection() {
        if let Some(b) = COORD.critical_bridge.read().as_ref() {
            b.record_forced_non_moving_collection();
        }
    }

    pub(super) fn bridge_relocation_forbidden() -> bool {
        bridge_relocation_forbidden_for_vm(None)
    }

    pub(super) fn bridge_relocation_forbidden_for_vm(scope_vm: Option<u64>) -> bool {
        COORD
            .critical_bridge
            .read()
            .as_ref()
            .is_some_and(|b| b.relocation_forbidden_for_vm(scope_vm))
    }

    /// Direct references to the process-wide `(vram_used_bytes, vram_reservation_limit)`
    /// mirror atomics for lock-free updates from the VM CUDA/VRAM allocator.
    pub fn vram_mirror_atomics() -> (&'static AtomicU64, &'static AtomicU64) {
        COORD.vram_mirror_atomics()
    }

    /// Current mirrored device VRAM usage in bytes.
    #[inline]
    pub fn vram_used_bytes() -> u64 {
        COORD.vram_used_bytes()
    }

    /// Current configured VRAM reservation limit in bytes (`0` means unbounded).
    #[inline]
    pub fn vram_reservation_limit() -> u64 {
        COORD.vram_reservation_limit()
    }

    /// Update the configured VRAM reservation limit in bytes (`0` means unbounded).
    #[inline]
    pub fn set_vram_reservation_limit(limit: u64) {
        COORD.set_vram_reservation_limit(limit)
    }

    /// Increment the VRAM-pressure GC counter and mirror into `gc_metrics` and
    /// `cratonvm_types::gpu_event_census`.
    pub fn note_gc_triggered_by_vram_pressure() {
        COORD
            .gc_triggered_by_vram_pressure
            .fetch_add(1, Ordering::Relaxed);
        crate::gc_metrics::record_vram_pressure_gc();
        cratonvm_types::gpu_event_census::note_gc_triggered_by_vram_pressure();
    }

    /// Total collections recorded as triggered by GPU VRAM pressure.
    #[inline]
    pub fn gc_triggered_by_vram_pressure() -> u64 {
        COORD.gc_triggered_by_vram_pressure()
    }

    /// Returns `true` when `vram_reservation_limit > 0` and mirrored VRAM
    /// usage exceeds 80% of the limit (`used * 5 > limit * 4`).
    #[inline]
    pub fn is_vram_under_pressure() -> bool {
        COORD.is_vram_under_pressure()
    }

    /// Register (or replace) a VM's VRAM-pressure collection callback keyed by
    /// `vm_id`.
    pub fn register_vm_pressure_collector(
        vm_id: u64,
        cb: Arc<dyn Fn() -> bool + Send + Sync>,
    ) {
        COORD.register_vm_pressure_collector(vm_id, cb);
    }

    /// Unregister the VRAM-pressure collection callback for `vm_id`.
    pub fn unregister_vm_pressure_collector(vm_id: u64) {
        COORD.unregister_vm_pressure_collector(vm_id);
    }

    /// Trigger registered VM pressure collectors to reclaim dead GPU/Cleaner
    /// resources under VRAM pressure.
    pub fn collect_garbage_for_gpu_pressure() -> bool {
        COORD.collect_garbage_for_gpu_pressure()
    }

    /// Register a callback invoked during `VmHeap` teardown before
    /// backing heap storage is unmapped (`cuMemHostUnregister` before
    /// `VirtualFree`/`munmap`).
    pub fn register_heap_unpin_hook(hook: Box<dyn Fn() + Send + Sync>) {
        COORD.register_heap_unpin_hook(hook);
    }

    /// Register a one-shot unpin callback scoped to a specific backing span
    /// `(base, len)`, invoked when the `VmHeap` owning `[base, base + len)`
    /// drops or its backing store decommits that span.
    pub fn register_span_unpin_hook(
        base: usize,
        len: usize,
        hook: Box<dyn FnOnce() + Send + Sync>,
    ) {
        COORD.register_span_unpin_hook(base, len, hook);
    }

    /// Register a fallible span unpin callback scoped to `(base, len)`.
    /// Returning `false` signals that an active DMA pin still holds the range
    /// (`refcount > 0`), so the hook remains registered and range decommit/reset
    /// refuses to unmap under live DMA.
    pub fn register_fallible_span_unpin_hook(
        base: usize,
        len: usize,
        hook: Arc<dyn Fn() -> bool + Send + Sync>,
    ) {
        COORD.register_fallible_span_unpin_hook(base, len, hook);
    }

    /// Invoke unpin hooks overlapping `spans` outside `COORD` locks, leaving
    /// global teardown hooks and unrelated spans intact. Returns `true` if all
    /// matching hooks succeeded (or none matched); returns `false` and retains
    /// any fallible hook that reported an active pin.
    pub fn run_matching_span_unpin_hooks(spans: &[(usize, usize)]) -> bool {
        COORD.run_matching_span_unpin_hooks(spans)
    }

    /// Invoke registered global heap teardown hooks (retaining them for any
    /// surviving `VmHeap`s) and run matching span unpin hooks after releasing
    /// every `COORD` lock.
    pub fn run_heap_unpin_hooks_for_spans(spans: &[(usize, usize)]) {
        COORD.run_heap_unpin_hooks_for_spans(spans);
    }

    /// Atomically acquire or share `CollectingGuard` ownership on
    /// `GPU_CRITICAL_COUNT` while `GpuCriticalDrain` guards are active.
    pub fn try_acquire_critical_collecting() -> bool {
        let mut guard = COORD.critical_collecting.lock();
        if guard.0 > 0 {
            guard.0 += 1;
            return true;
        }
        if let Some(collecting) =
            crate::safepoint::CollectingGuard::try_acquire(&super::GPU_CRITICAL_COUNT)
        {
            guard.0 = 1;
            guard.1 = Some(collecting);
            true
        } else {
            false
        }
    }

    /// Release one active `GpuCriticalDrain`'s reference to `CollectingGuard`,
    /// dropping the underlying `CollectingGuard` outside the coordination lock
    /// when the last active drain completes.
    pub fn release_critical_collecting() {
        let released = {
            let mut guard = COORD.critical_collecting.lock();
            if guard.0 > 1 {
                guard.0 -= 1;
                None
            } else {
                guard.0 = 0;
                guard.1.take()
            }
        };
        drop(released);
    }

    /// Register a hook returning `Some(drained)` while its ZGC heap is alive
    /// (`false` if `drain == false` probe) and `None` once dropped.
    pub fn register_zgc_relocate_idle_hook(hook: ZgcRelocIdleHook) {
        COORD.register_zgc_relocate_idle_hook(hook);
    }

    /// Drain any active post-pause ZGC concurrent relocation plan across live
    /// heaps outside all `COORD` locks. Returns `true` if any heap drained work.
    pub fn wait_for_zgc_concurrent_relocate_idle() -> bool {
        COORD.wait_for_zgc_concurrent_relocate_idle()
    }

    /// Synchronize ZGC concurrent relocation with GPU relocation exclusion so
    /// no post-pause lazy copies race with GPU zero-copy DMA.
    pub fn enter_relocation_forbidden() -> bool {
        COORD.enter_relocation_forbidden()
    }

    #[cfg(test)]
    pub(super) fn lock_for_test() -> parking_lot::MutexGuard<'static, ()> {
        COORD.test_lock.lock()
    }

    /// Drain and invoke all registered heap and span unpin hooks outside locks.
    pub fn run_heap_unpin_hooks() {
        COORD.run_heap_unpin_hooks();
    }

    /// See [`super::gpu_relocation_forbidden`].
    #[inline]
    pub fn relocation_forbidden() -> bool {
        COORD.relocation_forbidden()
    }

    /// The rule of [`relocation_forbidden`], apart. Pure.
    fn relocation_forbidden_from(
        per_cycle: bool,
        vetoing_cycles: usize,
        process_flag: bool,
    ) -> bool {
        if per_cycle {
            vetoing_cycles != 0 || process_flag
        } else {
            process_flag
        }
    }

    /// Veto relocation for the cycle in progress on the global context. Idempotent per cycle;
    /// withdrawn by that cycle's [`CycleGuard::after_collection`].
    pub fn forbid_relocation_this_cycle() {
        match THIS_CYCLE_VETOED.try_with(|c| !c.replace(true)) {
            Ok(true) => {
                COORD.vetoing_cycles.fetch_add(1, Ordering::AcqRel);
                record_forced_non_moving_collection();
            }
            Ok(false) => {}
            Err(_) => {
                let _ = forbid_relocation_without_this_threads_slot();
            }
        }
    }

    fn forbid_relocation_without_this_threads_slot() -> bool {
        if !COORD.relocation_forbidden.swap(true, Ordering::AcqRel) {
            record_forced_non_moving_collection();
        }
        true
    }

    /// Withdraw this thread's cycle veto, if it raised one.
    fn withdraw_this_cycles_veto() {
        let had = THIS_CYCLE_VETOED
            .try_with(|c| c.replace(false))
            .unwrap_or(false);
        if had {
            COORD.vetoing_cycles.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// What one cycle owes the registry when it finishes.
    #[must_use = "after_collection must run, or the relocation veto and the moving-cycle gate stay set"]
    pub struct CycleGuard<'a> {
        coord: &'a GpuCoordContext,
        /// Keep-alive addresses of every outstanding token at cycle start,
        /// as `ObjectRef`s for the root buffer. Empty in the common case.
        pub extra_roots: Vec<ObjectRef>,
        moving_declared: bool,
        vetoed_by_this_cycle: bool,
        _gpu_drain: Option<super::GpuCriticalDrain>,
    }

    /// Process-wide GPU drain preparation (for standalone unit tests).
    pub fn before_collection() -> CycleGuard<'static> {
        COORD.before_collection()
    }

    impl CycleGuard<'_> {
        fn finish_cycle(&mut self) {
            if self.moving_declared {
                if let Some(reg) = self.coord.bridge() {
                    reg.end_moving_cycle_for_vm(self.coord.owner_vm_id());
                }
                self.moving_declared = false;
            }
            if self.vetoed_by_this_cycle {
                self.coord.vetoing_cycles.fetch_sub(1, Ordering::AcqRel);
                self.vetoed_by_this_cycle = false;
            }
            if std::ptr::eq(self.coord, &COORD) {
                withdraw_this_cycles_veto();
            }
            self.coord
                .relocation_forbidden
                .store(false, Ordering::Release);
        }

        /// `remapped` is the tail of the root buffer this guard's
        /// `extra_roots` were appended to, after the collector rewrote it.
        pub fn after_collection(mut self, remapped: &[ObjectRef]) {
            if let Some(reg) = self.coord.bridge() {
                if !self.extra_roots.is_empty() {
                    let moved: rustc_hash::FxHashMap<usize, usize> = self
                        .extra_roots
                        .iter()
                        .zip(remapped)
                        .filter(|(old, new)| old.as_ptr() != new.as_ptr())
                        .map(|(old, new)| (old.as_ptr() as usize, new.as_ptr() as usize))
                        .collect();
                    if !moved.is_empty() {
                        reg.remap_keepalive(&|addr| moved.get(&addr).copied());
                    }
                }
            }
            let gpu_drain = self._gpu_drain.take();
            self.finish_cycle();
            std::mem::forget(self);
            drop(gpu_drain);
        }
    }

    impl Drop for CycleGuard<'_> {
        fn drop(&mut self) {
            self.finish_cycle();
        }
    }

    /// gci i46/ts (`i45-bh-report.md` R2): the slot-gone veto fails safe.
    #[cfg(test)]
    mod gci_i46ts_slot_gone_veto_tests;
}

pub struct GpuCriticalDrain {
    #[cfg(feature = "gpu-offload")]
    pub timed_out: bool,
    #[cfg(feature = "gpu-offload")]
    pub holding_collecting: bool,
}

#[cfg(feature = "gpu-offload")]
pub static GPU_DRAIN_VETOES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl Drop for GpuCriticalDrain {
    fn drop(&mut self) {
        #[cfg(feature = "gpu-offload")]
        {
            if self.timed_out {
                GPU_DRAIN_VETOES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
            if self.holding_collecting {
                gpu_coordination::release_critical_collecting();
            }
        }
    }
}

#[cfg(feature = "gpu-offload")]
pub fn wait_for_gpu_critical_drain() -> GpuCriticalDrain {
    let deadline = std::time::Instant::now() + gpu_coordination::collector_wait_budget();
    wait_for_gpu_critical_drain_until(deadline, true)
}

#[cfg(feature = "gpu-offload")]
fn wait_for_gpu_critical_drain_until(
    deadline: std::time::Instant,
    bail_if_already_forbidden: bool,
) -> GpuCriticalDrain {
    loop {
        if gpu_coordination::try_acquire_critical_collecting() {
            return GpuCriticalDrain {
                timed_out: false,
                holding_collecting: true,
            };
        }
        let now = std::time::Instant::now();
        let already_forbidden = gpu_coordination::relocation_forbidden();
        if (bail_if_already_forbidden && already_forbidden) || now >= deadline {
            let live =
                GPU_CRITICAL_COUNT.load(Ordering::Acquire) & !crate::safepoint::GPU_COLLECTING;
            if now >= deadline {
                tracing::warn!(
                    "GC waited until deadline for {} direct GPU critical token(s); proceeding non-moving",
                    live,
                );
            }
            GPU_DRAIN_VETOES.fetch_add(1, Ordering::AcqRel);
            if bail_if_already_forbidden && !already_forbidden {
                gpu_coordination::record_forced_non_moving_collection();
            }
            return GpuCriticalDrain {
                timed_out: true,
                holding_collecting: false,
            };
        }
        std::thread::yield_now();
    }
}

#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub fn wait_for_gpu_critical_drain() -> GpuCriticalDrain {
    GpuCriticalDrain {}
}

/// Whether the cycle in progress must not relocate because of GPU work.
///
/// Read by every collector at its "may I move this" decision: ZGC's
/// stop-the-world slide and its large-object compactor, the generational
/// collector's moving-young choice, G1's evacuation. `false` for the
/// whole life of a build without `gpu-offload`, and for every cycle of a
/// process that never launched a kernel.
#[cfg(feature = "gpu-offload")]
#[inline]
pub fn gpu_relocation_forbidden() -> bool {
    gpu_coordination::relocation_forbidden()
        || GPU_DRAIN_VETOES.load(std::sync::atomic::Ordering::Acquire) != 0
        || gpu_coordination::bridge_relocation_forbidden()
}

#[cfg(not(feature = "gpu-offload"))]
#[inline]
pub fn gpu_relocation_forbidden() -> bool {
    false
}

/// Per-collector tracker for off-heap GPU VRAM retained by live Java
/// `GpuArray` / `GpuFuture` wrappers.
///
/// Embedded in `GenerationalHeap`, `ZgcRealHeap`, and `G1Collector` so there
/// are zero process-wide statics and zero duplicated pacing rules.
pub struct ExternalGpuPressureTracker {
    retained_bytes: std::sync::atomic::AtomicU64,
    live_wrappers: std::sync::atomic::AtomicU64,
    trigger_bytes: std::sync::atomic::AtomicU64,
    per_object_bias_bytes: std::sync::atomic::AtomicU64,
    min_floor_bytes: std::sync::atomic::AtomicU64,
    rearm_bytes: std::sync::atomic::AtomicU64,
    #[cfg(feature = "gpu-offload")]
    pub gpu_coord: std::sync::Arc<gpu_coordination::GpuCoordContext>,
}

impl ExternalGpuPressureTracker {
    const DEFAULT_PER_OBJECT_BIAS_BYTES: u64 = 1 << 20;

    /// Build a tracker whose defaults scale gently with the Java heap capacity.
    pub fn with_heap_capacity(heap_bytes: usize) -> Self {
        let cap = heap_bytes as u64;
        Self {
            retained_bytes: std::sync::atomic::AtomicU64::new(0),
            live_wrappers: std::sync::atomic::AtomicU64::new(0),
            trigger_bytes: std::sync::atomic::AtomicU64::new(
                (cap / 2).clamp(16 << 20, 256 << 20),
            ),
            per_object_bias_bytes: std::sync::atomic::AtomicU64::new(
                Self::DEFAULT_PER_OBJECT_BIAS_BYTES,
            ),
            min_floor_bytes: std::sync::atomic::AtomicU64::new(
                (cap / 8).clamp(4 << 20, 32 << 20),
            ),
            rearm_bytes: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "gpu-offload")]
            gpu_coord: gpu_coordination::GpuCoordContext::new(),
        }
    }

    /// Whether the cycle in progress must forbid relocation on THIS heap due
    /// to per-VM or process-wide GPU coordination.
    #[inline]
    pub fn gpu_relocation_forbidden(&self) -> bool {
        #[cfg(feature = "gpu-offload")]
        {
            self.gpu_coord.relocation_forbidden()
                || gpu_coordination::relocation_forbidden()
                || GPU_DRAIN_VETOES.load(std::sync::atomic::Ordering::Acquire) != 0
                || gpu_coordination::bridge_relocation_forbidden_for_vm(self.gpu_coord.owner_vm_id())
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            false
        }
    }

    /// Override external pressure thresholds for tests or host policy tuning.
    pub fn set_policy(&self, trigger_bytes: u64, per_object_bias_bytes: u64, min_floor_bytes: u64) {
        use std::sync::atomic::Ordering;
        self.trigger_bytes.store(trigger_bytes, Ordering::Relaxed);
        self.per_object_bias_bytes
            .store(per_object_bias_bytes, Ordering::Relaxed);
        self.min_floor_bytes
            .store(min_floor_bytes, Ordering::Relaxed);
        self.rearm_bytes.store(0, Ordering::Relaxed);
    }

    #[inline]
    fn hysteresis_step(&self, retained: u64) -> u64 {
        use std::sync::atomic::Ordering;
        let floor = self.min_floor_bytes.load(Ordering::Relaxed);
        let trigger = self.trigger_bytes.load(Ordering::Relaxed);
        let base = match (floor > 0, trigger > 0) {
            (true, true) => floor.min(trigger),
            (true, false) => floor,
            (false, true) => trigger,
            (false, false) => 4 << 20,
        };
        (retained / 4).max(base / 2).max(1)
    }

    /// Apply a signed delta to retained off-heap VRAM and live wrapper count,
    /// arming `native_alloc_pressure` if the external pressure predicate fires.
    pub fn apply_signed_delta(
        &self,
        delta_bytes: i64,
        wrapper_delta: i64,
        native_alloc_pressure: &std::sync::atomic::AtomicBool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if delta_bytes > 0 {
            self.retained_bytes
                .fetch_add(delta_bytes as u64, Ordering::Relaxed);
        } else if delta_bytes < 0 {
            let sub = delta_bytes.unsigned_abs();
            let _ = self
                .retained_bytes
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_sub(sub))
                });
        }

        if wrapper_delta > 0 {
            self.live_wrappers
                .fetch_add(wrapper_delta as u64, Ordering::Relaxed);
        } else if wrapper_delta < 0 {
            let sub = wrapper_delta.unsigned_abs();
            let _ = self
                .live_wrappers
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_sub(sub))
                });
        }

        if delta_bytes < 0 {
            let new_ret = self.retained_bytes.load(Ordering::Relaxed);
            if new_ret == 0 || new_ret < self.min_floor_bytes.load(Ordering::Relaxed) {
                self.rearm_bytes.store(0, Ordering::Relaxed);
            } else {
                let cap = new_ret.saturating_add(self.hysteresis_step(new_ret));
                self.rearm_bytes.fetch_min(cap, Ordering::Relaxed);
            }
        }

        let triggered = delta_bytes > 0 && self.should_trigger_gc();
        if triggered {
            native_alloc_pressure.store(true, Ordering::Relaxed);
        }
        triggered
    }

    /// True when retained off-heap VRAM exceeds policy thresholds and clears
    /// the post-collection hysteresis watermark.
    #[inline]
    pub fn should_trigger_gc(&self) -> bool {
        use std::sync::atomic::Ordering;
        let retained = self.retained_bytes.load(Ordering::Relaxed);
        if retained == 0 {
            return false;
        }
        let rearm = self.rearm_bytes.load(Ordering::Relaxed);
        if retained < rearm {
            return false;
        }
        let trigger = self.trigger_bytes.load(Ordering::Relaxed);
        if trigger > 0 && retained >= trigger {
            return true;
        }
        let floor = self.min_floor_bytes.load(Ordering::Relaxed);
        let bias = self.per_object_bias_bytes.load(Ordering::Relaxed);
        let wrappers = self.live_wrappers.load(Ordering::Relaxed);
        bias > 0 && wrappers > 0 && retained >= floor && (retained / wrappers) >= bias
    }

    /// Advance the hysteresis watermark when a GC cycle finishes so surviving
    /// live `GpuArray`/`GpuFuture` wrappers do not trigger back-to-back pauses.
    pub fn on_gc_cycle_end(&self) {
        use std::sync::atomic::Ordering;
        let retained = self.retained_bytes.load(Ordering::Relaxed);
        if retained == 0 {
            self.rearm_bytes.store(0, Ordering::Relaxed);
            return;
        }
        let next_rearm = retained.saturating_add(self.hysteresis_step(retained));
        self.rearm_bytes.store(next_rearm, Ordering::Relaxed);
    }

    #[inline]
    pub fn retained_bytes(&self) -> u64 {
        self.retained_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    pub fn live_wrappers(&self) -> u64 {
        self.live_wrappers.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    pub fn rearm_bytes(&self) -> u64 {
        self.rearm_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
}
