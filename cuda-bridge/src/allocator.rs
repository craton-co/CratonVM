//! Stream-ordered GPU VRAM suballocator with segregated-fit / buddy
//! coalescing, same-stream zero-wait reuse, cross-stream non-blocking event
//! reclamation, and two-tier OOM recovery.
//!
//! # Design
//!
//! Each CUDA context owns a [`SubAllocator`] backed either by 2 MiB-aligned
//! CUDA Driver VMM slabs ([`crate::vmm::VmmPool`]) or, when VMM is unsupported
//! or disabled, standard `cuMemAlloc` / `cuMemFree` backing slabs.
//!
//! - **Free blocks** live in segregated-fit size-class buckets indexed alongside
//!   an address-ordered block map so adjacent free blocks within a slab
//!   coalesce immediately.
//! - **Pending-free blocks** (`PendingFree { block, stream_id, event }`) were
//!   dropped while GPU work might still reference them.
//! - **Same-stream zero-wait reuse**: allocating on `stream_id = Some(s)` scans
//!   `PendingFree` entries tagged with stream `s` first. Because CUDA streams
//!   execute in FIFO order, reusing a same-stream block (and splitting any
//!   excess tail back into `PendingFree` on the same `(stream_id, event)`)
//!   needs zero host synchronization and records
//!   `cratonvm_types::gpu_event_census::note_stream_ordered_reuse()`.
//! - **Cross-stream non-blocking reclamation**: [`reclaim_all_completed_pools`]
//!   and [`SubAllocator::reclaim_completed`] poll `PendingFree` events via
//!   non-blocking `query()` (`cuEventQuery`), move retired blocks into `Free`
//!   (coalescing neighbors), update global VRAM usage counters/mirrors, and
//!   signal the allocator condvar via [`notify_vram_freed`].
//! - **Two-tier OOM recovery**:
//!   - Tier 1: non-blocking reclaim across pools; if still short, synchronize
//!     blocking `PendingFree` events (releasing the allocator lock across each
//!     wait) and trim idle slabs, then retry.
//!   - Tier 2: drop ALL allocator locks, invoke the registered VRAM-pressure GC
//!     hook ([`register_vram_pressure_gc_hook`]), record
//!     `cratonvm_types::gpu_event_census::note_gc_triggered_by_vram_pressure()`,
//!     wait up to 50 ms on the global VRAM-freed condvar, then re-acquire and
//!     retry before returning an OOM error.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI8, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::{DeviceError, Result};

/// Minimum suballocation alignment (bytes). Matches standard GPU alignment.
pub const MIN_BLOCK_ALIGN: usize = 256;

/// Default backing slab size (4 MiB; expanded when a single request is larger).
pub const DEFAULT_SLAB_BYTES: usize = 4 * 1024 * 1024;

/// Maximum time Tier 2 OOM recovery waits for GC / deferred frees to release VRAM.
const TIER2_OOM_WAIT: Duration = Duration::from_millis(50);

/// Number of segregated-fit size-class buckets (handles 256 B .. multi-GiB).
const NUM_SIZE_CLASSES: usize = 64;

// ---------------------------------------------------------------------------
// Global VRAM coordination state & public API
// ---------------------------------------------------------------------------

static VRAM_USED_BYTES: AtomicU64 = AtomicU64::new(0);
static VRAM_LIMIT_BYTES: AtomicU64 = AtomicU64::new(0);

static MIRROR_USED: AtomicPtr<AtomicU64> = AtomicPtr::new(std::ptr::null_mut());
static MIRROR_LIMIT: AtomicPtr<AtomicU64> = AtomicPtr::new(std::ptr::null_mut());

/// Function pointer stored as `usize` (`0` = unregistered).
static GC_HOOK: AtomicUsize = AtomicUsize::new(0);
/// Asynchronous soft-watermark GC hint function pointer (`0` = unregistered).
static SOFT_GC_HOOK: AtomicUsize = AtomicUsize::new(0);

#[derive(Default, Clone, Copy)]
struct VmGcHooks {
    hard: Option<fn() -> bool>,
    soft: Option<fn() -> bool>,
}

static VM_GC_HOOKS: OnceLock<std::sync::RwLock<HashMap<u64, VmGcHooks>>> = OnceLock::new();

fn vm_gc_hooks_map() -> &'static std::sync::RwLock<HashMap<u64, VmGcHooks>> {
    VM_GC_HOOKS.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

/// Default soft VRAM watermark percentage (80%) for proactive pre-reclaim.
pub const DEFAULT_VRAM_SOFT_WATERMARK_PERCENT: u8 = 80;
static VRAM_SOFT_WATERMARK_PCT: AtomicU64 =
    AtomicU64::new(DEFAULT_VRAM_SOFT_WATERMARK_PERCENT as u64);
static PROACTIVE_RECLAIM_COUNT: AtomicU64 = AtomicU64::new(0);
static PRE_RECLAIM_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Override for `CRATONVM_GPU_NATIVE_STREAM_POOL` (`-1` = env/default false, `0` = off, `1` = on).
static NATIVE_STREAM_POOL_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

/// Sentinel `AllocLease::slab_id` for allocations delegated to driver-native
/// `cuMemAllocFromPoolAsync` / `hipMallocFromPoolAsync`.
pub const NATIVE_STREAM_POOL_SLAB_ID: u64 = u64::MAX - 1;

static VRAM_FREED_COND: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

static REGISTERED_POOLS: Mutex<Vec<Weak<dyn ReclaimablePool>>> = Mutex::new(Vec::new());

/// Per-`(vm_identity, device_ordinal)` VRAM accounting and quota slot.
#[derive(Debug)]
struct QuotaSlot {
    used: AtomicU64,
    peak_used: AtomicU64,
    soft_limit: AtomicU64,
    limit: AtomicU64,
    spill_used: AtomicU64,
    spill_limit: AtomicU64,
    alloc_count: AtomicU64,
    alloc_bytes_total: AtomicU64,
    soft_watermark_triggers: AtomicU64,
    oom_failures: AtomicU64,
}

impl QuotaSlot {
    fn new() -> Self {
        Self {
            used: AtomicU64::new(0),
            peak_used: AtomicU64::new(0),
            soft_limit: AtomicU64::new(0),
            limit: AtomicU64::new(0),
            spill_used: AtomicU64::new(0),
            spill_limit: AtomicU64::new(0),
            alloc_count: AtomicU64::new(0),
            alloc_bytes_total: AtomicU64::new(0),
            soft_watermark_triggers: AtomicU64::new(0),
            oom_failures: AtomicU64::new(0),
        }
    }

    fn record_peak(&self, used_after: u64) {
        let mut cur = self.peak_used.load(Ordering::Relaxed);
        while used_after > cur {
            match self.peak_used.compare_exchange_weak(
                cur,
                used_after,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    fn snapshot(&self, vm_identity: u64, device_ordinal: u32) -> VmGpuMemoryReport {
        VmGpuMemoryReport {
            vm_identity,
            device_ordinal,
            vram_used_bytes: self.used.load(Ordering::Relaxed),
            vram_peak_bytes: self.peak_used.load(Ordering::Relaxed),
            vram_soft_limit_bytes: self.soft_limit.load(Ordering::Relaxed),
            vram_hard_limit_bytes: self.limit.load(Ordering::Relaxed),
            spill_used_bytes: self.spill_used.load(Ordering::Relaxed),
            spill_limit_bytes: self.spill_limit.load(Ordering::Relaxed),
            alloc_count: self.alloc_count.load(Ordering::Relaxed),
            alloc_bytes_total: self.alloc_bytes_total.load(Ordering::Relaxed),
            soft_watermark_triggers: self.soft_watermark_triggers.load(Ordering::Relaxed),
            oom_failures: self.oom_failures.load(Ordering::Relaxed),
        }
    }
}

/// JFR / MBean-style per-`(vm_identity, device_ordinal)` GPU memory quota and usage report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmGpuMemoryReport {
    pub vm_identity: u64,
    pub device_ordinal: u32,
    pub vram_used_bytes: u64,
    pub vram_peak_bytes: u64,
    pub vram_soft_limit_bytes: u64,
    pub vram_hard_limit_bytes: u64,
    pub spill_used_bytes: u64,
    pub spill_limit_bytes: u64,
    pub alloc_count: u64,
    pub alloc_bytes_total: u64,
    pub soft_watermark_triggers: u64,
    pub oom_failures: u64,
}

static QUOTA_SLOTS: OnceLock<Mutex<HashMap<(u64, u32), Arc<QuotaSlot>>>> = OnceLock::new();

fn quota_slot_for(vm_identity: u64, device_ordinal: u32) -> Arc<QuotaSlot> {
    let map = QUOTA_SLOTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    Arc::clone(
        guard
            .entry((vm_identity, device_ordinal))
            .or_insert_with(|| Arc::new(QuotaSlot::new())),
    )
}

/// Set the hard VRAM quota in bytes for a specific `(vm_identity, device_ordinal)`
/// pair (`0` = no per-VM/device cap beyond the global `VRAM_LIMIT_BYTES`).
pub fn set_vm_device_vram_limit(vm_identity: u64, device_ordinal: u32, limit_bytes: u64) {
    quota_slot_for(vm_identity, device_ordinal)
        .limit
        .store(limit_bytes, Ordering::Relaxed);
}

/// Configure both soft (proactive reclaim trigger) and hard VRAM quotas in bytes
/// for a specific `(vm_identity, device_ordinal)` pair (`0` = inherit/unbounded).
pub fn set_vm_device_vram_quotas(
    vm_identity: u64,
    device_ordinal: u32,
    soft_limit_bytes: u64,
    hard_limit_bytes: u64,
) {
    let slot = quota_slot_for(vm_identity, device_ordinal);
    slot.soft_limit.store(soft_limit_bytes, Ordering::Relaxed);
    slot.limit.store(hard_limit_bytes, Ordering::Relaxed);
}

/// Return the hard VRAM quota in bytes for `(vm_identity, device_ordinal)` (`0` = unlimited).
pub fn vm_device_vram_limit_bytes(vm_identity: u64, device_ordinal: u32) -> u64 {
    quota_slot_for(vm_identity, device_ordinal)
        .limit
        .load(Ordering::Relaxed)
}

/// Return the explicit soft VRAM quota in bytes for `(vm_identity, device_ordinal)` (`0` = none).
pub fn vm_device_vram_soft_limit_bytes(vm_identity: u64, device_ordinal: u32) -> u64 {
    quota_slot_for(vm_identity, device_ordinal)
        .soft_limit
        .load(Ordering::Relaxed)
}

/// Return current live + pending-free VRAM usage in bytes for `(vm_identity, device_ordinal)`.
pub fn vm_device_vram_usage_bytes(vm_identity: u64, device_ordinal: u32) -> u64 {
    quota_slot_for(vm_identity, device_ordinal)
        .used
        .load(Ordering::Relaxed)
}

/// Partition `total_limit_bytes` evenly across all `(vm_identity, device_ordinal)`
/// pairs and configure `VRAM_LIMIT_BYTES` to `total_limit_bytes`.
pub fn partition_vram_limit(
    vm_identities: &[u64],
    device_ordinals: &[u32],
    total_limit_bytes: u64,
) {
    set_vram_reservation_limit(total_limit_bytes);
    let count = vm_identities.len().saturating_mul(device_ordinals.len());
    if count == 0 {
        return;
    }
    let per_partition = total_limit_bytes / (count as u64);
    for &vm_id in vm_identities {
        for &dev in device_ordinals {
            set_vm_device_vram_limit(vm_id, dev, per_partition);
        }
    }
}

/// Partition both total soft and hard VRAM budgets evenly across all
/// `(vm_identity, device_ordinal)` pairs and set global `VRAM_LIMIT_BYTES`
/// to `hard_total_bytes`.
pub fn partition_vram_quotas(
    vm_identities: &[u64],
    device_ordinals: &[u32],
    soft_total_bytes: u64,
    hard_total_bytes: u64,
) {
    set_vram_reservation_limit(hard_total_bytes);
    let count = vm_identities.len().saturating_mul(device_ordinals.len());
    if count == 0 {
        return;
    }
    let soft_per = soft_total_bytes / (count as u64);
    let hard_per = hard_total_bytes / (count as u64);
    for &vm_id in vm_identities {
        for &dev in device_ordinals {
            set_vm_device_vram_quotas(vm_id, dev, soft_per, hard_per);
        }
    }
}

/// Return a JFR / MBean-style GPU memory report for `(vm_identity, device_ordinal)`.
pub fn vm_gpu_memory_report(vm_identity: u64, device_ordinal: u32) -> VmGpuMemoryReport {
    quota_slot_for(vm_identity, device_ordinal).snapshot(vm_identity, device_ordinal)
}

/// Return sorted GPU memory reports for all known `(vm_identity, device_ordinal)` partitions.
pub fn all_vm_gpu_memory_reports() -> Vec<VmGpuMemoryReport> {
    let Some(map) = QUOTA_SLOTS.get() else {
        return Vec::new();
    };
    let snapshot: Vec<((u64, u32), Arc<QuotaSlot>)> = {
        let guard = map.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .iter()
            .map(|(&k, v)| (k, Arc::clone(v)))
            .collect()
    };
    let mut reports: Vec<VmGpuMemoryReport> = snapshot
        .into_iter()
        .map(|((vm_id, dev), slot)| slot.snapshot(vm_id, dev))
        .collect();
    reports.sort_by_key(|r| (r.vm_identity, r.device_ordinal));
    reports
}

/// Clear all per-device VRAM quota entries owned by `vm_identity` on VM teardown.
pub fn clear_vm_vram_quotas(vm_identity: u64) {
    if let Some(map) = QUOTA_SLOTS.get() {
        let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|&(vm, _), _| vm != vm_identity);
    }
}

#[inline]
fn sync_used_observers(used: u64) {
    let mirror = MIRROR_USED.load(Ordering::Acquire);
    if !mirror.is_null() {
        // SAFETY: `bind_vram_mirrors` only ever stores `&'static AtomicU64`
        // pointers, which remain valid for `'static`.
        unsafe { (*mirror).store(used, Ordering::Relaxed) };
    }
    cratonvm_types::gpu_event_census::set_vram_used_bytes(used);
}

#[inline]
fn sync_limit_mirror(limit: u64) {
    let mirror = MIRROR_LIMIT.load(Ordering::Acquire);
    if !mirror.is_null() {
        // SAFETY: `bind_vram_mirrors` only ever stores `&'static AtomicU64`
        // pointers, which remain valid for `'static`.
        unsafe { (*mirror).store(limit, Ordering::Relaxed) };
    }
}

#[inline]
fn add_global_vram_used(delta: u64) {
    if delta == 0 {
        return;
    }
    let prev = VRAM_USED_BYTES.fetch_add(delta, Ordering::Relaxed);
    let next = prev.saturating_add(delta);
    sync_used_observers(next);
    maybe_trigger_proactive_reclaim(next, false);
}

#[inline]
fn try_reserve_global_vram(delta: u64) -> bool {
    if delta == 0 {
        return true;
    }
    let limit = VRAM_LIMIT_BYTES.load(Ordering::Relaxed);
    if limit == 0 {
        add_global_vram_used(delta);
        return true;
    }
    let res = VRAM_USED_BYTES.try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        let next = cur.checked_add(delta)?;
        if next <= limit {
            Some(next)
        } else {
            None
        }
    });
    match res {
        Ok(prev) => {
            let next = prev.saturating_add(delta);
            sync_used_observers(next);
            maybe_trigger_proactive_reclaim(next, false);
            true
        }
        Err(_) => false,
    }
}

#[inline]
fn slot_used_over_soft_watermark(slot: &QuotaSlot, used: u64, pct: u64) -> bool {
    let soft = slot.soft_limit.load(Ordering::Relaxed);
    if soft > 0 && used >= soft {
        return true;
    }
    if pct == 0 {
        return false;
    }
    let part_limit = slot.limit.load(Ordering::Relaxed);
    part_limit > 0 && used >= part_limit.saturating_mul(pct) / 100
}

#[inline]
fn slot_over_soft_watermark(slot: &QuotaSlot, pct: u64) -> bool {
    slot_used_over_soft_watermark(slot, slot.used.load(Ordering::Relaxed), pct)
}

fn reserve_vram_in_slot(slot: &QuotaSlot, delta: u64) -> bool {
    if delta == 0 {
        return true;
    }
    let part_limit = slot.limit.load(Ordering::Relaxed);
    let part_used_after = if part_limit > 0 {
        match slot
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                let next = cur.saturating_add(delta);
                (next <= part_limit).then_some(next)
            }) {
            Ok(prev) => prev.saturating_add(delta),
            Err(_) => return false,
        }
    } else {
        slot.used.fetch_add(delta, Ordering::Relaxed).saturating_add(delta)
    };
    if try_reserve_global_vram(delta) {
        slot.record_peak(part_used_after);
        let pct = VRAM_SOFT_WATERMARK_PCT.load(Ordering::Relaxed);
        if slot_used_over_soft_watermark(slot, part_used_after, pct) {
            slot.soft_watermark_triggers.fetch_add(1, Ordering::Relaxed);
            maybe_trigger_proactive_reclaim(vram_usage_bytes(), true);
        }
        true
    } else {
        let _ = slot
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(delta))
            });
        false
    }
}

fn release_vram_in_slot(slot: &QuotaSlot, delta: u64) {
    if delta == 0 {
        return;
    }
    let _ = slot
        .used
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.saturating_sub(delta))
        });
    sub_global_vram_used(delta);
}

/// Atomically reserve `delta` bytes against both `(vm_identity, device_ordinal)`
/// and the process-wide `VRAM_LIMIT_BYTES`, reclaiming idle pools once if tight.
pub(crate) fn try_reserve_vram_partition_with_reclaim(
    vm_identity: u64,
    device_ordinal: u32,
    delta: u64,
) -> bool {
    let slot = quota_slot_for(vm_identity, device_ordinal);
    if reserve_vram_in_slot(&slot, delta) {
        return true;
    }
    let _ = reclaim_all_completed_pools();
    let _ = trim_all_idle_slabs();
    reserve_vram_in_slot(&slot, delta)
}

/// Release `delta` bytes previously reserved for `(vm_identity, device_ordinal)`.
pub(crate) fn release_vram_partition(vm_identity: u64, device_ordinal: u32, delta: u64) {
    let slot = quota_slot_for(vm_identity, device_ordinal);
    release_vram_in_slot(&slot, delta);
}

#[inline]
fn sub_global_vram_used(delta: u64) {
    if delta == 0 {
        return;
    }
    let _ = VRAM_USED_BYTES.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        let next = cur.saturating_sub(delta);
        Some(next)
    });
    sync_used_observers(VRAM_USED_BYTES.load(Ordering::Relaxed));
}

#[inline]
fn current_vram_freed_epoch() -> u64 {
    *VRAM_FREED_COND.0.lock().unwrap_or_else(|p| p.into_inner())
}

/// Bind static VRAM telemetry mirrors owned by `cratonvm_types`.
///
/// Once bound, every change to [`vram_usage_bytes`] and
/// [`vram_reservation_limit_bytes`] is mirrored into `used` and `limit`.
pub fn bind_vram_mirrors(used: &'static AtomicU64, limit: &'static AtomicU64) {
    MIRROR_USED.store(used as *const AtomicU64 as *mut AtomicU64, Ordering::Release);
    MIRROR_LIMIT.store(limit as *const AtomicU64 as *mut AtomicU64, Ordering::Release);

    used.store(VRAM_USED_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
    let current_limit = VRAM_LIMIT_BYTES.load(Ordering::Relaxed);
    if current_limit == 0 {
        let mirror_val = limit.load(Ordering::Relaxed);
        if mirror_val != 0 {
            VRAM_LIMIT_BYTES.store(mirror_val, Ordering::Relaxed);
        }
    } else {
        limit.store(current_limit, Ordering::Relaxed);
    }
}

/// Register the GC callback invoked during Tier 2 VRAM-pressure OOM recovery.
///
/// The hook is called with ALL suballocator mutexes released and returns
/// `true` if a GC cycle was triggered to reclaim GPU references.
pub fn register_vram_pressure_gc_hook(hook: fn() -> bool) {
    GC_HOOK.store(hook as usize, Ordering::Release);
}

fn load_gc_hook() -> Option<fn() -> bool> {
    let raw = GC_HOOK.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: `GC_HOOK` is only ever written by
        // `register_vram_pressure_gc_hook` with a valid `fn() -> bool`.
        Some(unsafe { std::mem::transmute::<usize, fn() -> bool>(raw) })
    }
}

/// Register a per-VM VRAM-pressure GC callback for `vm_identity`.
pub fn register_vm_vram_pressure_gc_hook(vm_identity: u64, hook: fn() -> bool) {
    let mut map = vm_gc_hooks_map().write().unwrap_or_else(|p| p.into_inner());
    map.entry(vm_identity).or_default().hard = Some(hook);
}

/// Register a per-VM soft-watermark GC callback for `vm_identity`.
pub fn register_vm_vram_soft_watermark_gc_hook(vm_identity: u64, hook: fn() -> bool) {
    let mut map = vm_gc_hooks_map().write().unwrap_or_else(|p| p.into_inner());
    map.entry(vm_identity).or_default().soft = Some(hook);
}

/// Remove any per-VM GC callbacks registered for `vm_identity` on VM teardown.
pub fn unregister_vm_vram_pressure_gc_hooks(vm_identity: u64) {
    if let Some(map) = VM_GC_HOOKS.get() {
        map.write().unwrap_or_else(|p| p.into_inner()).remove(&vm_identity);
    }
}

fn load_gc_hook_for(vm_identity: u64) -> Option<fn() -> bool> {
    if vm_identity != 0 {
        if let Some(map) = VM_GC_HOOKS.get() {
            if let Some(h) = map
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .get(&vm_identity)
                .and_then(|e| e.hard)
            {
                return Some(h);
            }
        }
    }
    load_gc_hook()
}

/// Register an asynchronous GC hint callback invoked on a background worker
/// when VRAM usage crosses the soft watermark.
pub fn register_vram_soft_watermark_gc_hook(hook: fn() -> bool) {
    SOFT_GC_HOOK.store(hook as usize, Ordering::Release);
}

fn load_soft_gc_hook() -> Option<fn() -> bool> {
    let raw = SOFT_GC_HOOK.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: `SOFT_GC_HOOK` is only written by
        // `register_vram_soft_watermark_gc_hook` with a valid `fn() -> bool`.
        Some(unsafe { std::mem::transmute::<usize, fn() -> bool>(raw) })
    }
}

/// Set the proactive soft VRAM watermark as a percentage of `VRAM_LIMIT_BYTES`
/// (`0` disables proactive pre-reclaim; clamped to `0..=100`).
pub fn set_vram_soft_watermark_percent(percent: u8) {
    VRAM_SOFT_WATERMARK_PCT.store(percent.min(100) as u64, Ordering::Relaxed);
}

/// Current proactive soft VRAM watermark percentage (`0` = disabled).
pub fn vram_soft_watermark_percent() -> u8 {
    VRAM_SOFT_WATERMARK_PCT.load(Ordering::Relaxed) as u8
}

/// Computed soft watermark threshold in bytes (`0` when `VRAM_LIMIT_BYTES == 0`
/// or watermark percentage is `0`).
pub fn vram_soft_watermark_bytes() -> u64 {
    let limit = VRAM_LIMIT_BYTES.load(Ordering::Relaxed);
    let pct = VRAM_SOFT_WATERMARK_PCT.load(Ordering::Relaxed);
    if limit == 0 || pct == 0 {
        0
    } else {
        limit.saturating_mul(pct) / 100
    }
}

/// Total number of background proactive pre-reclaim cycles triggered.
pub fn proactive_reclaim_count() -> u64 {
    PROACTIVE_RECLAIM_COUNT.load(Ordering::Relaxed)
}

fn collect_soft_watermark_hooks_scoped(global_over: bool) -> (bool, Vec<fn() -> bool>) {
    let mut hooks: Vec<fn() -> bool> = Vec::new();
    let mut push_unique = |h: fn() -> bool| {
        let addr = h as usize;
        if !hooks.iter().any(|&existing| existing as usize == addr) {
            hooks.push(h);
        }
    };

    if global_over {
        if let Some(map) = VM_GC_HOOKS.get() {
            let guard = map.read().unwrap_or_else(|p| p.into_inner());
            for entry in guard.values() {
                if let Some(h) = entry.soft {
                    push_unique(h);
                }
            }
        }
        if let Some(h) = load_soft_gc_hook() {
            push_unique(h);
        }
        return (true, hooks);
    }

    let pct = VRAM_SOFT_WATERMARK_PCT.load(Ordering::Relaxed);
    let mut over_vms: Vec<u64> = Vec::new();
    if let Some(map) = QUOTA_SLOTS.get() {
        let guard = map.lock().unwrap_or_else(|p| p.into_inner());
        for (&(vm_id, _dev), slot) in guard.iter() {
            if slot_over_soft_watermark(slot, pct) && !over_vms.contains(&vm_id) {
                over_vms.push(vm_id);
            }
        }
    }
    if over_vms.is_empty() {
        return (false, hooks);
    }
    let mut any_unscoped = over_vms.contains(&0);
    if let Some(map) = VM_GC_HOOKS.get() {
        let guard = map.read().unwrap_or_else(|p| p.into_inner());
        for &vm_id in &over_vms {
            if let Some(h) = guard.get(&vm_id).and_then(|e| e.soft) {
                push_unique(h);
            } else if vm_id != 0 {
                any_unscoped = true;
            }
        }
    } else {
        any_unscoped = true;
    }
    if any_unscoped {
        if let Some(h) = load_soft_gc_hook() {
            push_unique(h);
        }
    }
    (true, hooks)
}

fn pre_reclaim_worker_tx() -> &'static std::sync::mpsc::SyncSender<()> {
    static TX: OnceLock<std::sync::mpsc::SyncSender<()>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let _ = std::thread::Builder::new()
            .name("CudaVramPreReclaim".to_string())
            .spawn(move || {
                while rx.recv().is_ok() {
                    PROACTIVE_RECLAIM_COUNT.fetch_add(1, Ordering::Relaxed);
                    let _ = reclaim_all_completed_pools();
                    let threshold = vram_soft_watermark_bytes();
                    let global_over = threshold > 0 && vram_usage_bytes() >= threshold;
                    let (should_fire, hooks) = collect_soft_watermark_hooks_scoped(global_over);
                    if should_fire {
                        for hook in hooks {
                            let _ = hook();
                        }
                    }
                    PRE_RECLAIM_IN_FLIGHT.store(false, Ordering::Release);
                }
            });
        tx
    })
}

#[inline]
fn maybe_trigger_proactive_reclaim(used: u64, force_partition_trigger: bool) {
    if !force_partition_trigger {
        let threshold = vram_soft_watermark_bytes();
        if threshold == 0 || used < threshold {
            return;
        }
    }
    if PRE_RECLAIM_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        if pre_reclaim_worker_tx().try_send(()).is_err() {
            PRE_RECLAIM_IN_FLIGHT.store(false, Ordering::Release);
        }
    }
}

/// Enable or disable delegating [`SubAllocator`] to driver-native
/// `cuMemAllocFromPoolAsync` / `hipMallocFromPoolAsync` when supported.
pub fn set_native_stream_pool_enabled(enabled: bool) {
    NATIVE_STREAM_POOL_OVERRIDE.store(if enabled { 1 } else { 0 }, Ordering::Release);
}

/// Whether driver-native stream-ordered pool delegation is enabled.
///
/// Defaults to `false` (portable `SubAllocator + VmmPool` path) unless enabled
/// via [`set_native_stream_pool_enabled(true)`] or `CRATONVM_GPU_NATIVE_STREAM_POOL=1`.
pub fn native_stream_pool_enabled() -> bool {
    static ENV_ENABLED: OnceLock<bool> = OnceLock::new();
    match NATIVE_STREAM_POOL_OVERRIDE.load(Ordering::Acquire) {
        0 => false,
        1 => true,
        _ => *ENV_ENABLED.get_or_init(|| {
            std::env::var("CRATONVM_GPU_NATIVE_STREAM_POOL")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
                .unwrap_or(false)
        }),
    }
}

/// Set the process-wide GPU VRAM reservation limit in bytes (`0` = unlimited).
pub fn set_vram_reservation_limit(limit_bytes: u64) {
    VRAM_LIMIT_BYTES.store(limit_bytes, Ordering::Relaxed);
    sync_limit_mirror(limit_bytes);
}

/// Current live + pending-free VRAM usage in bytes across all active pools.
pub fn vram_usage_bytes() -> u64 {
    VRAM_USED_BYTES.load(Ordering::Relaxed)
}

/// Current VRAM reservation limit in bytes (`0` = unlimited).
pub fn vram_reservation_limit_bytes() -> u64 {
    VRAM_LIMIT_BYTES.load(Ordering::Relaxed)
}

/// Wake any threads waiting in Tier 2 OOM recovery after VRAM has been freed.
pub fn notify_vram_freed() {
    let mut epoch = VRAM_FREED_COND.0.lock().unwrap_or_else(|p| p.into_inner());
    *epoch = epoch.wrapping_add(1);
    VRAM_FREED_COND.1.notify_all();
}

/// Whether the device suballocation pool is enabled (always `true`).
#[inline(always)]
pub(crate) fn device_pool_enabled() -> bool {
    true
}

/// Non-blockingly poll all live suballocator pools and move completed
/// `PendingFree` blocks into their coalesced free lists.
///
/// Returns the total number of `PendingFree` blocks reclaimed across pools.
pub fn reclaim_all_completed_pools() -> usize {
    #[cfg(feature = "gpu-driver")]
    reclaim_deferred_host_uploads();
    let pools = snapshot_registered_pools();
    let mut reclaimed = 0usize;
    for pool in pools {
        reclaimed += pool.reclaim_completed_erased();
    }
    if reclaimed > 0 {
        notify_vram_freed();
    }
    reclaimed
}

#[cfg(feature = "gpu-driver")]
static DEFERRED_HOST_UPLOADS: Mutex<Vec<(Arc<crate::event::Event>, Arc<dyn std::any::Any + Send + Sync>)>> =
    Mutex::new(Vec::new());

/// Park an incomplete `from_host_async` staging allocation until its upload
/// event retires, avoiding a synchronous `ev.synchronize()` on `DeviceBuffer::drop`.
#[cfg(feature = "gpu-driver")]
pub(crate) fn defer_host_upload(
    event: Arc<crate::event::Event>,
    staging: Arc<dyn std::any::Any + Send + Sync>,
) {
    let mut guard = DEFERRED_HOST_UPLOADS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    guard.retain(|(ev, _)| matches!(ev.query(), Ok(false)));
    if guard.len() >= 256 {
        let overflow: Vec<_> = guard.drain(..128).collect();
        drop(guard);
        for (ev, buf) in overflow {
            if ev.synchronize().is_err() {
                std::mem::forget(buf);
            }
        }
        guard = DEFERRED_HOST_UPLOADS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
    }
    guard.push((event, staging));
}

#[cfg(feature = "gpu-driver")]
fn reclaim_deferred_host_uploads() {
    let to_drop = {
        let mut guard = DEFERRED_HOST_UPLOADS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if guard.is_empty() {
            return;
        }
        let mut done = Vec::new();
        let mut i = 0;
        while i < guard.len() {
            match guard[i].0.query() {
                Ok(true) => {
                    done.push(guard.swap_remove(i));
                }
                Ok(false) => {
                    i += 1;
                }
                Err(_) => {
                    let (ev, buf) = guard.swap_remove(i);
                    if ev.synchronize().is_err() {
                        std::mem::forget(buf);
                    }
                }
            }
        }
        done
    };
    drop(to_drop);
}

fn trim_all_idle_slabs() -> usize {
    let pools = snapshot_registered_pools();
    let mut trimmed = 0usize;
    for pool in pools {
        trimmed += pool.trim_idle_slabs_erased();
    }
    if trimmed > 0 {
        notify_vram_freed();
    }
    trimmed
}

fn snapshot_registered_pools() -> Vec<Arc<dyn ReclaimablePool>> {
    let mut guard = REGISTERED_POOLS.lock().unwrap_or_else(|p| p.into_inner());
    let mut live = Vec::with_capacity(guard.len());
    guard.retain(|weak| {
        if let Some(arc) = weak.upgrade() {
            live.push(arc);
            true
        } else {
            false
        }
    });
    live
}

fn register_pool(weak: Weak<dyn ReclaimablePool>) {
    let mut guard = REGISTERED_POOLS.lock().unwrap_or_else(|p| p.into_inner());
    guard.retain(|w| w.strong_count() > 0);
    guard.push(weak);
}

fn any_pool_has_pending() -> bool {
    snapshot_registered_pools()
        .into_iter()
        .any(|pool| pool.has_pending_erased())
}

trait ReclaimablePool: Send + Sync {
    fn reclaim_completed_erased(&self) -> usize;
    fn trim_idle_slabs_erased(&self) -> usize;
    fn has_pending_erased(&self) -> bool;
}

// ---------------------------------------------------------------------------
// Abstraction traits for backend slabs & completion fences
// ---------------------------------------------------------------------------

/// Completion fence attached to a [`PendingFree`] block.
pub trait FenceEvent: Clone + Send + Sync + 'static {
    /// Non-blocking completion poll (`true` once all preceding GPU work retired).
    fn is_complete(&self) -> bool;

    /// Blocking host wait until the event completes.
    fn sync(&self) -> Result<()>;
}

#[cfg(feature = "gpu-driver")]
impl FenceEvent for Arc<crate::event::Event> {
    fn is_complete(&self) -> bool {
        matches!(self.query(), Ok(true))
    }

    fn sync(&self) -> Result<()> {
        self.synchronize()
    }
}

/// Retirement fence state shared by driver-backed `ManagedAlloc` wrappers.
#[cfg(feature = "gpu-driver")]
#[derive(Default)]
pub(crate) struct RetireState {
    unfenced: bool,
    stream_id: Option<u64>,
    events: Vec<Arc<crate::Event>>,
}

#[cfg(feature = "gpu-driver")]
impl RetireState {
    pub(crate) const fn idle() -> Self {
        Self {
            unfenced: false,
            stream_id: None,
            events: Vec::new(),
        }
    }

    pub(crate) const fn unfenced() -> Self {
        Self {
            unfenced: true,
            stream_id: None,
            events: Vec::new(),
        }
    }

    pub(crate) fn set_unfenced(mutex: &Mutex<Self>) {
        mutex.lock().unwrap_or_else(|p| p.into_inner()).unfenced = true;
    }

    /// Update the retirement fence in-place under `mutex`.
    ///
    /// Passing `event = None` preserves any existing `unfenced` flag or
    /// active fence events so stream-capture slot clears cannot drop an
    /// in-flight async transfer un-synchronized.
    pub(crate) fn set_fence(
        mutex: &Mutex<Self>,
        stream_id: Option<u64>,
        event: Option<Arc<crate::Event>>,
    ) {
        let Some(ev) = event else {
            return;
        };
        let mut state = mutex.lock().unwrap_or_else(|p| p.into_inner());
        state.unfenced = false;
        state.stream_id = stream_id;
        state.events.retain(|e| !e.is_complete());
        state.events.push(ev);
    }

    /// Record an additional completion fence (e.g., concurrent chunked D2H reads)
    /// without clearing `unfenced` or overwriting other stream fences.
    pub(crate) fn add_fence(
        mutex: &Mutex<Self>,
        stream_id: Option<u64>,
        event: Option<Arc<crate::Event>>,
    ) {
        let Some(ev) = event else {
            return;
        };
        let mut state = mutex.lock().unwrap_or_else(|p| p.into_inner());
        if state.stream_id.is_none() {
            state.stream_id = stream_id;
        }
        state.events.retain(|e| !e.is_complete());
        state.events.push(ev);
    }

    /// Mark the allocation idle under `mutex`.
    pub(crate) fn set_idle(mutex: &Mutex<Self>) {
        let mut state = mutex.lock().unwrap_or_else(|p| p.into_inner());
        state.unfenced = false;
        state.stream_id = None;
        state.events.clear();
    }

    /// Drain any unfenced work or in-flight fence events before releasing
    /// a non-pooled (e.g. shared-memory host-mapped) allocation.
    pub(crate) fn sync_if_busy(mutex: &Mutex<Self>, sync_fallback: impl FnOnce()) {
        let mut state = std::mem::take(&mut *mutex.lock().unwrap_or_else(|p| p.into_inner()));
        if state.unfenced {
            sync_fallback();
        }
        state.events.retain(|ev| !ev.is_complete());
        for ev in state.events {
            let _ = ev.synchronize();
        }
    }

    /// Retire `lease` into `pool` according to the current `RetireState`.
    ///
    /// If the lease is `unfenced`, calls `sync_fallback` first so un-fenced
    /// async DMA cannot race with pool recycling.
    pub(crate) fn retire_lease<B: SlabBackend>(
        mutex: &Mutex<Self>,
        pool: &SubAllocator<B, Arc<crate::Event>>,
        lease: AllocLease,
        sync_fallback: impl FnOnce(),
    ) {
        let mut state = std::mem::take(&mut *mutex.lock().unwrap_or_else(|p| p.into_inner()));
        if state.unfenced {
            sync_fallback();
        }
        state.events.retain(|ev| !ev.is_complete());
        if state.events.is_empty() {
            pool.free_deferred(lease, None, None);
        } else {
            let primary = state.events.swap_remove(0);
            pool.free_deferred_fenced(lease, state.stream_id, Some(primary), state.events);
        }
    }
}

/// Backing slab descriptor returned by [`SlabBackend`].
#[derive(Debug, Clone)]
pub struct BackingSlab<H> {
    /// Base device address of the slab.
    pub base_ptr: u64,
    /// Total byte length of the slab.
    pub size_bytes: usize,
    /// Backend-specific slab handle (e.g. VMM vs raw `cuMemAlloc` tag).
    pub handle: H,
}

/// Physical/driver backing allocator for [`SubAllocator`].
///
/// Implemented by [`CudaSlabBackend`] for CUDA (using [`crate::vmm::VmmPool`]
/// with `cuMemAlloc` fallback) and by lightweight CPU mock backends in tests.
pub trait SlabBackend: Send + Sync + 'static {
    /// Handle identifying how the slab was created.
    type Slab: Send + Sync;

    /// Allocate a new contiguous backing slab of at least `min_bytes`.
    fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>>;

    /// Stream-aware variant of [`Self::alloc_slab`] so backends with VMM can
    /// associate allocated chunks with `stream_id` for per-stream defragmentation.
    fn alloc_slab_for_stream(
        &self,
        min_bytes: usize,
        _stream_id: Option<u64>,
    ) -> Result<BackingSlab<Self::Slab>> {
        self.alloc_slab(min_bytes)
    }

    /// Release an entirely idle backing slab back to the driver/OS.
    fn free_slab(&self, slab: BackingSlab<Self::Slab>);

    /// Attempt to coalesce/remap scattered free VMM chunks into a contiguous
    /// slab without disturbing active allocations. Returns `Ok(None)` when
    /// unsupported or when insufficient idle chunks are available.
    fn defragment_for(&self, _min_bytes: usize) -> Result<Option<BackingSlab<Self::Slab>>> {
        Ok(None)
    }

    /// Hint the backend that a slab is temporarily idle (VMM can mark chunks
    /// `IdleMapped` for possible remapping without immediately unmapping).
    fn mark_slab_idle(&self, _slab: &BackingSlab<Self::Slab>) {}

    /// Whether [`Self::defragment_for`] can coalesce idle physical chunks after
    /// [`Self::park_idle_slab`] detaches them from [`SubAllocator`].
    fn supports_defragment(&self) -> bool {
        false
    }

    /// Park an idle slab that has already been detached from [`SubAllocator`]'s
    /// free lists so [`Self::defragment_for`] may remap its physical chunks
    /// without aliasing live virtual ranges.
    fn park_idle_slab(&self, slab: BackingSlab<Self::Slab>) {
        self.free_slab(slab);
    }

    /// Release any unmapped parked physical chunks back to the driver.
    fn trim_parked(&self) {}

    /// Whether this backend supports driver-native stream-ordered pool
    /// allocation (`cuMemAllocFromPoolAsync` on CUDA 11.2+ / `hipMallocFromPoolAsync`
    /// on ROCm 5.2+).
    fn supports_native_stream_pool(&self) -> bool {
        false
    }

    /// Allocate `bytes` asynchronously on `stream_id` from the driver's native
    /// stream-ordered memory pool. Returns `Ok(None)` when unsupported.
    fn alloc_from_stream_pool(&self, _bytes: usize, _stream_id: Option<u64>) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Free `ptr` asynchronously on `stream_id` back to the driver's native
    /// stream-ordered memory pool.
    fn free_to_stream_pool(&self, _ptr: u64, _stream_id: Option<u64>) -> Result<()> {
        Ok(())
    }

    /// Trim cached memory in the driver's native stream-ordered memory pool
    /// down to `min_bytes_to_keep`.
    fn trim_stream_pool(&self, _min_bytes_to_keep: usize) -> Result<()> {
        Ok(())
    }

    /// Opt-in Managed / Unified Memory fallback allocation used only when both
    /// Tier-1/Tier-2 GC reclamation and VMM compaction still leave physical VRAM
    /// exhausted.
    fn alloc_managed_slab(
        &self,
        _min_bytes: usize,
        _read_mostly: bool,
    ) -> Result<BackingSlab<Self::Slab>> {
        Err(DeviceError::Driver(
            "managed/unified memory spill-over unsupported by backend".into(),
        ))
    }
}

/// Opt-in policy governing Managed / Unified Memory spill-over on persistent
/// Tier-2 OOM (after GC reclamation and VMM compaction both fail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillConfig {
    /// Explicit opt-in switch (defaults to `false`: strict physical VRAM OOM).
    pub enabled: bool,
    /// Maximum Managed / Unified Memory bytes this allocator may spill (`0` when
    /// `enabled == true` means unbounded up to driver Managed Memory).
    pub budget_bytes: u64,
    /// Minimum aligned request size eligible to spill (`1 MiB` by default).
    pub min_tensor_bytes: usize,
    /// When `true` (default), only allocations tagged [`AllocHint::ReadMostlyTensor`]
    /// may spill into Managed Memory.
    pub read_mostly_only: bool,
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            budget_bytes: 0,
            min_tensor_bytes: 1024 * 1024,
            read_mostly_only: true,
        }
    }
}

/// Caller allocation hint controlling Managed Memory spill eligibility on
/// persistent Tier-2 OOM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AllocHint {
    /// Standard device VRAM allocation.
    #[default]
    Default,
    /// Large read-mostly tensor eligible for Managed / Unified Memory migration
    /// when opt-in spill is enabled and physical VRAM + VMM compaction are exhausted.
    ReadMostlyTensor,
}

/// Diagnostic snapshot of Managed / Unified Memory spill-over usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpillStats {
    pub enabled: bool,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    pub alloc_count: u64,
}

// ---------------------------------------------------------------------------
// SubAllocator core
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockState {
    Free,
    Allocated,
    PendingFree,
}

#[derive(Debug, Clone, Copy)]
struct BlockNode {
    ptr: u64,
    size: usize,
    slab_id: u64,
    state: BlockState,
}

struct SlabRecord<S> {
    backing: BackingSlab<S>,
    /// Number of blocks inside this slab currently `Allocated` or `PendingFree`.
    busy_blocks: usize,
}

struct PendingEntry<E> {
    id: u64,
    ptr: u64,
    size: usize,
    slab_id: u64,
    stream_id: Option<u64>,
    event: Option<E>,
    extra_events: Vec<E>,
}

impl<E: FenceEvent> PendingEntry<E> {
    fn all_complete(&self) -> bool {
        self.event.as_ref().is_none_or(|ev| ev.is_complete())
            && self.extra_events.iter().all(|ev| ev.is_complete())
    }
}

/// Allocation token handed back to callers (`ManagedAlloc` / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocLease {
    /// Device virtual address for the allocated block.
    pub ptr: u64,
    /// Aligned capacity in bytes managed by the suballocator for this block.
    pub alloc_bytes: usize,
    /// Logical byte size requested by the caller.
    pub requested_bytes: usize,
    /// Owning slab ID (`0` for zero-byte allocations, `u64::MAX` for unpooled direct slabs).
    pub slab_id: u64,
}

/// Snapshot of [`SubAllocator`] memory state for diagnostics and unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorStats {
    /// Total bytes currently held by driver/VMM slabs.
    pub backing_bytes: usize,
    /// Bytes currently leased to live buffers.
    pub live_bytes: usize,
    /// Bytes waiting in `PendingFree` for stream events to fire.
    pub pending_bytes: usize,
    /// Bytes sitting ready in segregated `Free` lists.
    pub free_bytes: usize,
    /// Number of `Free` fragments across all buckets.
    pub free_block_count: usize,
    /// Number of entries waiting in `PendingFree`.
    pub pending_count: usize,
    /// Number of active backing slabs.
    pub slab_count: usize,
}

#[inline]
fn align_request(bytes: usize) -> Result<usize> {
    if bytes == 0 {
        return Ok(0);
    }
    let rem = bytes % MIN_BLOCK_ALIGN;
    if rem == 0 {
        Ok(bytes)
    } else {
        bytes
            .checked_add(MIN_BLOCK_ALIGN - rem)
            .ok_or_else(|| DeviceError::Driver("suballocator size overflow".into()))
    }
}

#[inline]
fn size_class(bytes: usize) -> usize {
    if bytes <= MIN_BLOCK_ALIGN {
        return 0;
    }
    let units = (bytes - 1) / MIN_BLOCK_ALIGN;
    let bit = (usize::BITS - 1 - units.leading_zeros()) as usize;
    bit.min(NUM_SIZE_CLASSES - 1)
}

struct AllocatorState<S, E> {
    next_slab_id: u64,
    next_pending_id: u64,
    slabs: HashMap<u64, SlabRecord<S>>,
    /// Address-ordered blocks inside pooled slabs.
    blocks: BTreeMap<u64, BlockNode>,
    /// Segregated free lists `(ptr, size)` indexed by `size_class(size)`.
    buckets: Vec<Vec<(u64, usize)>>,
    /// Unpooled direct allocations kept out of the coalescing buddy map.
    unpooled_slabs: HashMap<u64, BackingSlab<S>>,
    /// Managed / Unified Memory spill-over slabs kept separate from VRAM free buckets.
    managed_slabs: HashMap<u64, BackingSlab<S>>,
    /// Blocks dropped with an in-flight stream/event fence.
    pending: Vec<PendingEntry<E>>,
    live_bytes: usize,
    pending_bytes: usize,
    free_bytes: usize,
    backing_bytes: usize,
}

impl<S, E> AllocatorState<S, E> {
    fn new() -> Self {
        Self {
            next_slab_id: 1,
            next_pending_id: 1,
            slabs: HashMap::new(),
            blocks: BTreeMap::new(),
            buckets: (0..NUM_SIZE_CLASSES).map(|_| Vec::new()).collect(),
            unpooled_slabs: HashMap::new(),
            managed_slabs: HashMap::new(),
            pending: Vec::new(),
            live_bytes: 0,
            pending_bytes: 0,
            free_bytes: 0,
            backing_bytes: 0,
        }
    }

    fn insert_free_bucket(&mut self, ptr: u64, size: usize) {
        let cls = size_class(size);
        self.buckets[cls].push((ptr, size));
    }

    fn remove_free_bucket(&mut self, ptr: u64, size: usize) {
        let cls = size_class(size);
        if let Some(pos) = self.buckets[cls].iter().position(|&(p, _)| p == ptr) {
            self.buckets[cls].swap_remove(pos);
        }
    }

    fn take_best_free_block(&mut self, aligned_bytes: usize) -> Option<(u64, usize, u64)> {
        let start_cls = size_class(aligned_bytes);
        for cls in start_cls..NUM_SIZE_CLASSES {
            if self.buckets[cls].is_empty() {
                continue;
            }
            // Pick the smallest fitting block in this bucket.
            let mut best_idx: Option<(usize, usize)> = None;
            for (idx, &(_p, sz)) in self.buckets[cls].iter().enumerate() {
                if sz >= aligned_bytes
                    && best_idx.is_none_or(|(_, best_sz)| sz < best_sz)
                {
                    best_idx = Some((idx, sz));
                    if sz == aligned_bytes {
                        break;
                    }
                }
            }
            if let Some((idx, full_size)) = best_idx {
                let (ptr, _) = self.buckets[cls].swap_remove(idx);
                let slab_id = self.blocks.get(&ptr).map(|b| b.slab_id).unwrap_or(0);
                self.free_bytes = self.free_bytes.saturating_sub(full_size);

                // Split remainder back into Free if >= MIN_BLOCK_ALIGN.
                let rem = full_size - aligned_bytes;
                if rem >= MIN_BLOCK_ALIGN {
                    let rem_ptr = ptr + aligned_bytes as u64;
                    if let Some(node) = self.blocks.get_mut(&ptr) {
                        node.size = aligned_bytes;
                        node.state = BlockState::Allocated;
                    }
                    self.blocks.insert(
                        rem_ptr,
                        BlockNode {
                            ptr: rem_ptr,
                            size: rem,
                            slab_id,
                            state: BlockState::Free,
                        },
                    );
                    self.free_bytes += rem;
                    self.insert_free_bucket(rem_ptr, rem);
                } else if let Some(node) = self.blocks.get_mut(&ptr) {
                    node.state = BlockState::Allocated;
                }

                let assigned = if rem >= MIN_BLOCK_ALIGN {
                    aligned_bytes
                } else {
                    full_size
                };
                if let Some(slab) = self.slabs.get_mut(&slab_id) {
                    slab.busy_blocks += 1;
                }
                self.live_bytes += assigned;
                return Some((ptr, assigned, slab_id));
            }
        }
        None
    }

    fn take_same_stream_pending(
        &mut self,
        aligned_bytes: usize,
        stream_id: u64,
    ) -> Option<(u64, usize, u64)> {
        let mut best_idx: Option<(usize, usize)> = None;
        for (idx, entry) in self.pending.iter().enumerate() {
            if entry.stream_id == Some(stream_id)
                && entry.extra_events.is_empty()
                && entry.size >= aligned_bytes
                && best_idx.is_none_or(|(_, best_sz)| entry.size < best_sz)
            {
                best_idx = Some((idx, entry.size));
                if entry.size == aligned_bytes {
                    break;
                }
            }
        }
        let (idx, full_size) = best_idx?;
        let entry = self.pending.swap_remove(idx);
        let rem = full_size - aligned_bytes;

        let assigned = if rem >= MIN_BLOCK_ALIGN {
            let rem_ptr = entry.ptr + aligned_bytes as u64;
            if let Some(node) = self.blocks.get_mut(&entry.ptr) {
                node.size = aligned_bytes;
                node.state = BlockState::Allocated;
            }
            self.blocks.insert(
                rem_ptr,
                BlockNode {
                    ptr: rem_ptr,
                    size: rem,
                    slab_id: entry.slab_id,
                    state: BlockState::PendingFree,
                },
            );
            let pid = self.next_pending_id;
            self.next_pending_id = self.next_pending_id.wrapping_add(1).max(1);
            // Keep the remainder pending under the exact same stream/event fence.
            self.pending.push(PendingEntry {
                id: pid,
                ptr: rem_ptr,
                size: rem,
                slab_id: entry.slab_id,
                stream_id: entry.stream_id,
                event: entry.event,
                extra_events: Vec::new(),
            });
            if let Some(slab) = self.slabs.get_mut(&entry.slab_id) {
                slab.busy_blocks += 1;
            }
            aligned_bytes
        } else {
            if let Some(node) = self.blocks.get_mut(&entry.ptr) {
                node.state = BlockState::Allocated;
            }
            full_size
        };

        self.pending_bytes = self.pending_bytes.saturating_sub(assigned);
        self.live_bytes += assigned;
        Some((entry.ptr, assigned, entry.slab_id))
    }

    /// Move a block at `ptr` (`size`) into `Free` and coalesce with adjacent
    /// free blocks belonging to the same `slab_id`. Returns `true` if the whole
    /// slab became free (`busy_blocks == 0`).
    fn move_to_free_and_coalesce(&mut self, ptr: u64, size: usize, slab_id: u64) -> bool {
        let mut merged_ptr = ptr;
        let mut merged_size = size;

        // Coalesce immediate predecessor in the same slab.
        if let Some((&prev_ptr, prev_node)) = self.blocks.range(..merged_ptr).next_back() {
            if prev_node.slab_id == slab_id
                && prev_node.state == BlockState::Free
                && prev_ptr + prev_node.size as u64 == merged_ptr
            {
                let prev_size = prev_node.size;
                self.remove_free_bucket(prev_ptr, prev_size);
                self.blocks.remove(&prev_ptr);
                self.free_bytes = self.free_bytes.saturating_sub(prev_size);
                merged_ptr = prev_ptr;
                merged_size += prev_size;
            }
        }

        // Coalesce immediate successor in the same slab.
        let right_key = merged_ptr + merged_size as u64;
        if let Some(next_node) = self.blocks.get(&right_key).copied() {
            if next_node.slab_id == slab_id && next_node.state == BlockState::Free {
                self.remove_free_bucket(next_node.ptr, next_node.size);
                self.blocks.remove(&next_node.ptr);
                self.free_bytes = self.free_bytes.saturating_sub(next_node.size);
                merged_size += next_node.size;
            }
        }

        if merged_ptr != ptr {
            self.blocks.remove(&ptr);
        }
        self.blocks.insert(
            merged_ptr,
            BlockNode {
                ptr: merged_ptr,
                size: merged_size,
                slab_id,
                state: BlockState::Free,
            },
        );
        self.free_bytes += merged_size;
        self.insert_free_bucket(merged_ptr, merged_size);

        if let Some(slab) = self.slabs.get_mut(&slab_id) {
            slab.busy_blocks = slab.busy_blocks.saturating_sub(1);
            slab.busy_blocks == 0
        } else {
            false
        }
    }

    fn rollback_taken_free_block(&mut self, ptr: u64, alloc_bytes: usize, slab_id: u64) {
        self.live_bytes = self.live_bytes.saturating_sub(alloc_bytes);
        let _ = self.move_to_free_and_coalesce(ptr, alloc_bytes, slab_id);
    }

    fn insert_new_slab(&mut self, slab: BackingSlab<S>) -> u64 {
        let slab_id = self.next_slab_id;
        self.next_slab_id += 1;
        let ptr = slab.base_ptr;
        let size = slab.size_bytes;
        self.backing_bytes += size;
        self.slabs.insert(
            slab_id,
            SlabRecord {
                backing: slab,
                busy_blocks: 0,
            },
        );
        self.blocks.insert(
            ptr,
            BlockNode {
                ptr,
                size,
                slab_id,
                state: BlockState::Free,
            },
        );
        self.free_bytes += size;
        self.insert_free_bucket(ptr, size);
        slab_id
    }

    /// Detach all completely idle slabs (`busy_blocks == 0`) from the free map
    /// so the caller can return them to [`SlabBackend::free_slab`] outside locks.
    fn drain_idle_slabs(&mut self) -> Vec<BackingSlab<S>> {
        let idle_ids: Vec<u64> = self
            .slabs
            .iter()
            .filter_map(|(&id, rec)| (rec.busy_blocks == 0).then_some(id))
            .collect();
        let mut drained = Vec::with_capacity(idle_ids.len());
        for id in idle_ids {
            if let Some(rec) = self.slabs.remove(&id) {
                let base = rec.backing.base_ptr;
                let size = rec.backing.size_bytes;
                if let Some(node) = self.blocks.remove(&base) {
                    debug_assert_eq!(node.state, BlockState::Free);
                    self.remove_free_bucket(base, node.size);
                    self.free_bytes = self.free_bytes.saturating_sub(node.size);
                }
                self.backing_bytes = self.backing_bytes.saturating_sub(size);
                drained.push(rec.backing);
            }
        }
        drained
    }
}

/// Stream-ordered segregated-fit / buddy suballocator managing GPU VRAM slabs.
pub struct SubAllocator<B: SlabBackend, E: FenceEvent> {
    backend: B,
    default_slab_bytes: usize,
    pooling_enabled: bool,
    vm_identity: u64,
    device_ordinal: u32,
    quota_slot: Arc<QuotaSlot>,
    spill_config: Mutex<SpillConfig>,
    spill_used_bytes: AtomicU64,
    spill_allocs: AtomicU64,
    state: Mutex<AllocatorState<B::Slab, E>>,
}

impl<B: SlabBackend, E: FenceEvent> SubAllocator<B, E> {
    /// Construct a new suballocator and register it for global cross-pool reclaim.
    pub fn new_arc(backend: B, default_slab_bytes: usize, pooling_enabled: bool) -> Arc<Self> {
        Self::new_arc_with_quota(backend, default_slab_bytes, pooling_enabled, 0, 0)
    }

    /// Construct a new suballocator bound to `(vm_identity, device_ordinal)` for
    /// per-VM / per-GPU quota enforcement and register it for cross-pool reclaim.
    pub fn new_arc_with_quota(
        backend: B,
        default_slab_bytes: usize,
        pooling_enabled: bool,
        vm_identity: u64,
        device_ordinal: u32,
    ) -> Arc<Self> {
        let quota_slot = quota_slot_for(vm_identity, device_ordinal);
        let arc = Arc::new(Self {
            backend,
            default_slab_bytes: default_slab_bytes.max(MIN_BLOCK_ALIGN),
            pooling_enabled,
            vm_identity,
            device_ordinal,
            quota_slot,
            spill_config: Mutex::new(SpillConfig::default()),
            spill_used_bytes: AtomicU64::new(0),
            spill_allocs: AtomicU64::new(0),
            state: Mutex::new(AllocatorState::new()),
        });
        let weak: Weak<dyn ReclaimablePool> = Arc::downgrade(&arc) as Weak<dyn ReclaimablePool>;
        register_pool(weak);
        arc
    }

    /// Configure Managed / Unified Memory spill-over fallback on persistent Tier-2 OOM.
    pub fn set_spill_config(&self, config: SpillConfig) {
        *self.spill_config.lock().unwrap_or_else(|p| p.into_inner()) = config;
        self.quota_slot
            .spill_limit
            .store(config.budget_bytes, Ordering::Relaxed);
    }

    /// Active Managed / Unified Memory spill-over policy.
    pub fn spill_config(&self) -> SpillConfig {
        *self.spill_config.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Snapshot of Managed / Unified Memory spill-over counters.
    pub fn spill_stats(&self) -> SpillStats {
        let cfg = self.spill_config();
        SpillStats {
            enabled: cfg.enabled,
            budget_bytes: cfg.budget_bytes,
            used_bytes: self.spill_used_bytes.load(Ordering::Relaxed),
            alloc_count: self.spill_allocs.load(Ordering::Relaxed),
        }
    }

    /// Return a JFR / MBean-style memory report for this allocator's
    /// `(vm_identity, device_ordinal)` partition.
    pub fn gpu_memory_report(&self) -> VmGpuMemoryReport {
        self.quota_slot
            .snapshot(self.vm_identity, self.device_ordinal)
    }

    /// Owning VM identity (`0` when unscoped).
    pub fn vm_identity(&self) -> u64 {
        self.vm_identity
    }

    /// Owning GPU device ordinal.
    pub fn device_ordinal(&self) -> u32 {
        self.device_ordinal
    }

    /// Current live + pending-free VRAM usage in bytes for this allocator's
    /// `(vm_identity, device_ordinal)` partition.
    pub fn partition_vram_usage_bytes(&self) -> u64 {
        self.quota_slot.used.load(Ordering::Relaxed)
    }

    /// Configured VRAM quota in bytes for this allocator's `(vm_identity, device_ordinal)`
    /// partition (`0` = no per-partition cap).
    pub fn partition_vram_limit_bytes(&self) -> u64 {
        self.quota_slot.limit.load(Ordering::Relaxed)
    }

    #[inline]
    fn would_exceed_limit(&self, additional_bytes: u64) -> bool {
        let global_limit = vram_reservation_limit_bytes();
        if global_limit > 0 && vram_usage_bytes().saturating_add(additional_bytes) > global_limit {
            return true;
        }
        let part_limit = self.quota_slot.limit.load(Ordering::Relaxed);
        part_limit > 0
            && self
                .quota_slot
                .used
                .load(Ordering::Relaxed)
                .saturating_add(additional_bytes)
                > part_limit
    }

    #[inline]
    fn add_vram_used(&self, delta: u64) {
        if delta == 0 {
            return;
        }
        let after = self
            .quota_slot
            .used
            .fetch_add(delta, Ordering::Relaxed)
            .saturating_add(delta);
        self.quota_slot.record_peak(after);
        add_global_vram_used(delta);
        let pct = VRAM_SOFT_WATERMARK_PCT.load(Ordering::Relaxed);
        if slot_used_over_soft_watermark(&self.quota_slot, after, pct) {
            self.quota_slot
                .soft_watermark_triggers
                .fetch_add(1, Ordering::Relaxed);
            maybe_trigger_proactive_reclaim(vram_usage_bytes(), true);
        }
    }

    #[inline]
    fn try_reserve_vram(&self, delta: u64) -> bool {
        reserve_vram_in_slot(&self.quota_slot, delta)
    }

    #[inline]
    fn sub_vram_used(&self, delta: u64) {
        release_vram_in_slot(&self.quota_slot, delta)
    }

    #[inline]
    fn uses_native_stream_pool(&self) -> bool {
        self.pooling_enabled
            && native_stream_pool_enabled()
            && self.backend.supports_native_stream_pool()
    }

    /// Access the underlying slab backend (e.g., to inspect VMM stats in tests).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Allocate `bytes` aligned to [`MIN_BLOCK_ALIGN`], attempting same-stream
    /// `PendingFree` reuse first when `stream_id` is `Some`, then `Free` blocks,
    /// then a new backing slab, with two-tier OOM recovery on failure or when
    /// `vram_reservation_limit_bytes()` would be exceeded.
    pub fn alloc(&self, bytes: usize, stream_id: Option<u64>) -> Result<AllocLease> {
        self.alloc_with_hint(bytes, stream_id, AllocHint::Default)
    }

    /// Variant of [`Self::alloc`] carrying an [`AllocHint`] so large read-mostly
    /// tensors may opt-in to Managed / Unified Memory spill-over when persistent
    /// Tier-2 recovery (after GC reclamation and VMM compaction) remains OOM.
    pub fn alloc_with_hint(
        &self,
        bytes: usize,
        stream_id: Option<u64>,
        hint: AllocHint,
    ) -> Result<AllocLease> {
        if bytes == 0 {
            return Ok(AllocLease {
                ptr: 0,
                alloc_bytes: 0,
                requested_bytes: 0,
                slab_id: 0,
            });
        }
        let aligned = align_request(bytes)?;
        match self.alloc_physical(bytes, aligned, stream_id) {
            Ok(lease) => {
                self.quota_slot.alloc_count.fetch_add(1, Ordering::Relaxed);
                self.quota_slot
                    .alloc_bytes_total
                    .fetch_add(lease.alloc_bytes as u64, Ordering::Relaxed);
                Ok(lease)
            }
            Err(oom_err) => match self.try_managed_spill(bytes, aligned, hint)? {
                Some(lease) => {
                    self.quota_slot.alloc_count.fetch_add(1, Ordering::Relaxed);
                    self.quota_slot
                        .alloc_bytes_total
                        .fetch_add(lease.alloc_bytes as u64, Ordering::Relaxed);
                    Ok(lease)
                }
                None => {
                    self.quota_slot.oom_failures.fetch_add(1, Ordering::Relaxed);
                    Err(oom_err)
                }
            },
        }
    }

    fn alloc_physical(
        &self,
        bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<AllocLease> {
        if !self.pooling_enabled {
            return self.alloc_unpooled(bytes, aligned);
        }

        if self.uses_native_stream_pool() {
            return self.alloc_native_stream_pool(bytes, aligned, stream_id);
        }

        // Fast path under lock.
        if let Some(lease) = self.try_alloc_fast(bytes, aligned, stream_id)? {
            return Ok(lease);
        }

        // Tier 1: non-blocking reclaim across pools, then sync blocking pending
        // events if still short, then trim idle slabs & defragment VMM if needed.
        if let Some(lease) = self.try_tier1_recover(bytes, aligned, stream_id)? {
            return Ok(lease);
        }

        // Tier 2: release ALL locks, run registered GC hook + telemetry, wait up
        // to 50 ms on the VRAM-freed condvar, then retry once more.
        if let Some(lease) = self.try_tier2_recover(bytes, aligned, stream_id)? {
            return Ok(lease);
        }

        Err(DeviceError::Driver(format!(
            "VRAM out of memory allocating {bytes} bytes (aligned {aligned}, used {} / limit {}, partition {}/{} vm={} dev={})",
            vram_usage_bytes(),
            vram_reservation_limit_bytes(),
            self.partition_vram_usage_bytes(),
            self.partition_vram_limit_bytes(),
            self.vm_identity,
            self.device_ordinal,
        )))
    }

    fn try_managed_spill(
        &self,
        requested_bytes: usize,
        aligned: usize,
        hint: AllocHint,
    ) -> Result<Option<AllocLease>> {
        let cfg = self.spill_config();
        let read_mostly = hint == AllocHint::ReadMostlyTensor;
        if !cfg.enabled || aligned < cfg.min_tensor_bytes || (cfg.read_mostly_only && !read_mostly)
        {
            return Ok(None);
        }

        let delta = aligned as u64;
        if cfg.budget_bytes > 0 {
            if self
                .spill_used_bytes
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    let next = cur.saturating_add(delta);
                    (next <= cfg.budget_bytes).then_some(next)
                })
                .is_err()
            {
                return Ok(None);
            }
        } else {
            self.spill_used_bytes.fetch_add(delta, Ordering::Relaxed);
        }

        let slab = match self.backend.alloc_managed_slab(aligned, read_mostly) {
            Ok(s) => s,
            Err(_) => {
                let _ =
                    self.spill_used_bytes
                        .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                            Some(cur.saturating_sub(delta))
                        });
                return Ok(None);
            }
        };

        let ptr = slab.base_ptr;
        let slab_id = {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let id = st.next_slab_id;
            st.next_slab_id += 1;
            st.managed_slabs.insert(id, slab);
            id
        };
        self.spill_allocs.fetch_add(1, Ordering::Relaxed);
        self.quota_slot
            .spill_used
            .fetch_add(delta, Ordering::Relaxed);
        cratonvm_types::gpu_event_census::note_managed_spill_alloc(delta);
        Ok(Some(AllocLease {
            ptr,
            alloc_bytes: aligned,
            requested_bytes,
            slab_id,
        }))
    }

    fn alloc_native_stream_pool(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<AllocLease> {
        let try_native = || -> Result<Option<AllocLease>> {
            if !self.try_reserve_vram(aligned as u64) {
                return Ok(None);
            }
            match self.backend.alloc_from_stream_pool(aligned, stream_id) {
                Ok(Some(ptr)) => {
                    cratonvm_types::gpu_event_census::note_alloc_pool_hit();
                    {
                        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                        st.live_bytes += aligned;
                    }
                    Ok(Some(AllocLease {
                        ptr,
                        alloc_bytes: aligned,
                        requested_bytes,
                        slab_id: NATIVE_STREAM_POOL_SLAB_ID,
                    }))
                }
                Ok(None) => {
                    self.sub_vram_used(aligned as u64);
                    Ok(None)
                }
                Err(e) => {
                    self.sub_vram_used(aligned as u64);
                    Err(e)
                }
            }
        };

        match try_native() {
            Ok(Some(lease)) => return Ok(lease),
            Ok(None) => {}
            Err(_) => {}
        }

        let _ = reclaim_all_completed_pools();
        let _ = self.backend.trim_stream_pool(0);
        if let Some(lease) = try_native()? {
            return Ok(lease);
        }

        self.run_tier2_gc_wait(any_pool_has_pending());
        let _ = reclaim_all_completed_pools();
        let _ = self.backend.trim_stream_pool(0);
        if let Some(lease) = try_native()? {
            return Ok(lease);
        }

        Err(DeviceError::Driver(format!(
            "VRAM out of memory in native stream pool allocating {requested_bytes} bytes (used {} / limit {}, partition {}/{})",
            vram_usage_bytes(),
            vram_reservation_limit_bytes(),
            self.partition_vram_usage_bytes(),
            self.partition_vram_limit_bytes(),
        )))
    }

    fn alloc_unpooled(&self, requested_bytes: usize, aligned: usize) -> Result<AllocLease> {
        cratonvm_types::gpu_event_census::note_alloc_pool_miss();
        let mut reserved = self.try_reserve_vram(aligned as u64);
        if !reserved {
            let _ = self.try_tier1_wait_only();
            reserved = self.try_reserve_vram(aligned as u64);
        }
        if !reserved {
            self.run_tier2_gc_wait(any_pool_has_pending());
            let _ = reclaim_all_completed_pools();
            reserved = self.try_reserve_vram(aligned as u64);
        }
        if !reserved {
            return Err(DeviceError::Driver(format!(
                "VRAM reservation limit exceeded allocating {requested_bytes} bytes"
            )));
        }
        let slab = match self.backend.alloc_slab(aligned) {
            Ok(s) => s,
            Err(e) => {
                self.sub_vram_used(aligned as u64);
                return Err(e);
            }
        };
        let ptr = slab.base_ptr;
        let slab_size = slab.size_bytes;
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let slab_id = st.next_slab_id;
        st.next_slab_id += 1;
        st.unpooled_slabs.insert(slab_id, slab);
        st.backing_bytes += slab_size;
        st.live_bytes += aligned;
        drop(st);
        Ok(AllocLease {
            ptr,
            alloc_bytes: aligned,
            requested_bytes,
            slab_id,
        })
    }

    fn try_alloc_fast(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<Option<AllocLease>> {
        let has_pending = {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(sid) = stream_id {
                if let Some((ptr, alloc_bytes, slab_id)) =
                    st.take_same_stream_pending(aligned, sid)
                {
                    // Same-stream reuse keeps total `live + pending` unchanged.
                    cratonvm_types::gpu_event_census::note_stream_ordered_reuse();
                    return Ok(Some(AllocLease {
                        ptr,
                        alloc_bytes,
                        requested_bytes,
                        slab_id,
                    }));
                }
            }

            if let Some((ptr, alloc_bytes, slab_id)) = st.take_best_free_block(aligned) {
                if self.try_reserve_vram(alloc_bytes as u64) {
                    cratonvm_types::gpu_event_census::note_alloc_pool_hit();
                    return Ok(Some(AllocLease {
                        ptr,
                        alloc_bytes,
                        requested_bytes,
                        slab_id,
                    }));
                }
                st.rollback_taken_free_block(ptr, alloc_bytes, slab_id);
                return Ok(None);
            }
            !st.pending.is_empty()
        };

        if has_pending && self.reclaim_completed() > 0 {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((ptr, alloc_bytes, slab_id)) = st.take_best_free_block(aligned) {
                if self.try_reserve_vram(alloc_bytes as u64) {
                    cratonvm_types::gpu_event_census::note_alloc_pool_hit();
                    return Ok(Some(AllocLease {
                        ptr,
                        alloc_bytes,
                        requested_bytes,
                        slab_id,
                    }));
                }
                st.rollback_taken_free_block(ptr, alloc_bytes, slab_id);
                return Ok(None);
            }
        }

        // Neither PendingFree (same stream) nor Free satisfied the request:
        // try growing via a new backing slab (or VMM remap if backend supports it).
        self.try_grow_and_alloc(requested_bytes, aligned, stream_id)
    }

    fn try_grow_and_alloc(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<Option<AllocLease>> {
        if self.would_exceed_limit(aligned as u64) {
            return Ok(None);
        }

        let slab_request = self.default_slab_bytes.max(aligned);
        let new_slab = match self.backend.alloc_slab_for_stream(slab_request, stream_id) {
            Ok(s) => s,
            Err(_) => {
                // Retry VMM defragmentation only after detaching all completely
                // idle slabs from `self.state` so SubAllocator and VmmPool never
                // hold aliasing descriptors over the same virtual addresses.
                if self.backend.supports_defragment() {
                    let idle = {
                        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                        st.drain_idle_slabs()
                    };
                    for slab in idle {
                        self.backend.park_idle_slab(slab);
                    }
                    if let Ok(Some(remapped)) = self.backend.defragment_for(aligned) {
                        remapped
                    } else if slab_request > aligned {
                        match self.backend.alloc_slab_for_stream(aligned, stream_id) {
                            Ok(s) => s,
                            Err(_) => return Ok(None),
                        }
                    } else {
                        return Ok(None);
                    }
                } else if slab_request > aligned {
                    match self.backend.alloc_slab_for_stream(aligned, stream_id) {
                        Ok(s) => s,
                        Err(_) => return Ok(None),
                    }
                } else {
                    return Ok(None);
                }
            }
        };

        cratonvm_types::gpu_event_census::note_alloc_pool_miss();
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let _slab_id = st.insert_new_slab(new_slab);
        if let Some((ptr, alloc_bytes, slab_id)) = st.take_best_free_block(aligned) {
            if self.try_reserve_vram(alloc_bytes as u64) {
                Ok(Some(AllocLease {
                    ptr,
                    alloc_bytes,
                    requested_bytes,
                    slab_id,
                }))
            } else {
                st.rollback_taken_free_block(ptr, alloc_bytes, slab_id);
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn try_tier1_recover(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<Option<AllocLease>> {
        let start = Instant::now();
        let res = self.try_tier1_recover_inner(requested_bytes, aligned, stream_id);
        let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        cratonvm_types::gpu_event_census::record_tier1_latency_ns(ns);
        res
    }

    fn try_tier1_recover_inner(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<Option<AllocLease>> {
        // Tier 1a: non-blocking poll of completed PendingFree entries.
        let _ = reclaim_all_completed_pools();
        let _ = self.reclaim_completed();
        if let Some(lease) = self.try_alloc_fast(requested_bytes, aligned, stream_id)? {
            return Ok(Some(lease));
        }

        // Tier 1b: synchronize any pending-free events so in-flight buffers
        // become immediately reusable across streams, and trim idle slabs to
        // release backing pages if the driver itself was short.
        if self.sync_pending_and_reclaim() > 0 {
            if let Some(lease) = self.try_alloc_fast(requested_bytes, aligned, stream_id)? {
                return Ok(Some(lease));
            }
        }

        let _ = trim_all_idle_slabs();
        let _ = self.trim_idle_slabs();
        self.try_alloc_fast(requested_bytes, aligned, stream_id)
    }

    fn try_tier1_wait_only(&self) -> usize {
        let n = reclaim_all_completed_pools() + self.reclaim_completed();
        if n > 0 {
            return n;
        }
        self.sync_pending_and_reclaim()
    }

    fn invoke_tier2_gc_hook(&self) -> bool {
        let Some(hook) = load_gc_hook_for(self.vm_identity) else {
            return false;
        };
        let census_before = cratonvm_types::gpu_event_census::gc_triggered_by_vram_pressure();
        let requested = hook();
        if requested
            && cratonvm_types::gpu_event_census::gc_triggered_by_vram_pressure() == census_before
        {
            cratonvm_types::gpu_event_census::note_gc_triggered_by_vram_pressure();
        }
        requested
    }

    fn wait_vram_freed_slice(epoch: &mut u64, deadline: Instant, poll_pending: bool) -> bool {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let step = if poll_pending {
            remaining.min(Duration::from_millis(5))
        } else {
            remaining
        };
        let mut guard = VRAM_FREED_COND.0.lock().unwrap_or_else(|p| p.into_inner());
        while *guard == *epoch {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return false;
            }
            let wait_for = if poll_pending {
                rem.min(Duration::from_millis(5))
            } else {
                rem
            };
            let (next_guard, wait_res) = VRAM_FREED_COND
                .1
                .wait_timeout(guard, wait_for)
                .unwrap_or_else(|p| p.into_inner());
            guard = next_guard;
            if wait_res.timed_out() {
                let _ = step;
                break;
            }
        }
        *epoch = *guard;
        true
    }

    fn run_tier2_gc_wait(&self, has_pending: bool) {
        let mut epoch = current_vram_freed_epoch();
        let gc_triggered = self.invoke_tier2_gc_hook();
        if gc_triggered || has_pending {
            let deadline = Instant::now() + TIER2_OOM_WAIT;
            let _ = Self::wait_vram_freed_slice(&mut epoch, deadline, has_pending);
        }
    }

    fn try_tier2_recover(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<Option<AllocLease>> {
        let start = Instant::now();
        let res = self.try_tier2_recover_inner(requested_bytes, aligned, stream_id);
        let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        cratonvm_types::gpu_event_census::record_tier2_latency_ns(ns);
        res
    }

    fn try_tier2_recover_inner(
        &self,
        requested_bytes: usize,
        aligned: usize,
        stream_id: Option<u64>,
    ) -> Result<Option<AllocLease>> {
        let has_pending = {
            let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            !st.pending.is_empty()
        } || any_pool_has_pending();

        let mut epoch = current_vram_freed_epoch();
        let gc_triggered = self.invoke_tier2_gc_hook();

        let deadline = Instant::now() + TIER2_OOM_WAIT;
        loop {
            let _ = reclaim_all_completed_pools();
            let _ = self.reclaim_completed();
            let _ = trim_all_idle_slabs();
            let _ = self.trim_idle_slabs();
            if let Some(lease) = self.try_alloc_fast(requested_bytes, aligned, stream_id)? {
                return Ok(Some(lease));
            }
            let pending_now = any_pool_has_pending();
            if !(gc_triggered || has_pending || pending_now) {
                return Ok(None);
            }
            if !Self::wait_vram_freed_slice(&mut epoch, deadline, pending_now) {
                // One final synchronous drain in case events completed/arrived as deadline elapsed.
                let _ = self.sync_pending_and_reclaim();
                let _ = reclaim_all_completed_pools();
                let _ = self.trim_idle_slabs();
                return self.try_alloc_fast(requested_bytes, aligned, stream_id);
            }
        }
    }

    fn sync_pending_and_reclaim(&self) -> usize {
        let events: Vec<E> = {
            let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.pending
                .iter()
                .flat_map(|p| p.event.iter().chain(p.extra_events.iter()).cloned())
                .collect()
        };
        if events.is_empty() {
            return self.reclaim_completed();
        }
        // Wait outside the suballocator mutex so concurrent drops are never blocked.
        for ev in events {
            let _ = ev.sync();
        }
        self.reclaim_completed()
    }

    /// Retire a buffer allocation into `PendingFree` (or immediately into
    /// coalesced `Free` when `event` is `None`).
    pub fn free_deferred(&self, lease: AllocLease, stream_id: Option<u64>, event: Option<E>) {
        self.free_deferred_fenced(lease, stream_id, event, Vec::new());
    }

    pub(crate) fn free_deferred_fenced(
        &self,
        lease: AllocLease,
        stream_id: Option<u64>,
        event: Option<E>,
        extra_events: Vec<E>,
    ) {
        if lease.alloc_bytes == 0 {
            return;
        }

        if lease.slab_id == NATIVE_STREAM_POOL_SLAB_ID {
            let incomplete_ev = event
                .into_iter()
                .chain(extra_events)
                .filter(|ev| !ev.is_complete())
                .collect::<Vec<_>>();
            if let Some(first_ev) = incomplete_ev.first().cloned() {
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                let pid = st.next_pending_id;
                st.next_pending_id = st.next_pending_id.wrapping_add(1).max(1);
                st.live_bytes = st.live_bytes.saturating_sub(lease.alloc_bytes);
                st.pending_bytes += lease.alloc_bytes;
                st.pending.push(PendingEntry {
                    id: pid,
                    ptr: lease.ptr,
                    size: lease.alloc_bytes,
                    slab_id: NATIVE_STREAM_POOL_SLAB_ID,
                    stream_id,
                    event: Some(first_ev),
                    extra_events: incomplete_ev.into_iter().skip(1).collect(),
                });
                return;
            }
            {
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                st.live_bytes = st.live_bytes.saturating_sub(lease.alloc_bytes);
            }
            let _ = self.backend.free_to_stream_pool(lease.ptr, stream_id);
            self.sub_vram_used(lease.alloc_bytes as u64);
            notify_vram_freed();
            return;
        }

        let is_managed = {
            let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.managed_slabs.contains_key(&lease.slab_id)
        };
        if is_managed {
            if let Some(ev) = event {
                let _ = ev.sync();
            }
            for ev in extra_events {
                let _ = ev.sync();
            }
            let managed = {
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                st.managed_slabs.remove(&lease.slab_id)
            };
            if let Some(slab) = managed {
                let sz = slab.size_bytes as u64;
                self.backend.free_slab(slab);
                let _ =
                    self.spill_used_bytes
                        .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                            Some(cur.saturating_sub(sz))
                        });
                let _ = self.quota_slot.spill_used.try_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |cur| Some(cur.saturating_sub(sz)),
                );
                notify_vram_freed();
            }
            return;
        }

        if !self.pooling_enabled {
            if let Some(ev) = event {
                let _ = ev.sync();
            }
            for ev in extra_events {
                let _ = ev.sync();
            }
            let unpooled = {
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                st.live_bytes = st.live_bytes.saturating_sub(lease.alloc_bytes);
                let removed = st.unpooled_slabs.remove(&lease.slab_id);
                if let Some(ref s) = removed {
                    st.backing_bytes = st.backing_bytes.saturating_sub(s.size_bytes);
                }
                removed
            };
            if let Some(slab) = unpooled {
                self.backend.free_slab(slab);
            }
            self.sub_vram_used(lease.alloc_bytes as u64);
            notify_vram_freed();
            return;
        }

        let mut reclaimed_now_bytes = 0usize;
        let mut notify = false;

        {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if !matches!(
                st.blocks.get(&lease.ptr).map(|n| n.state),
                Some(BlockState::Allocated)
            ) {
                debug_assert!(
                    false,
                    "free_deferred called for non-allocated block {:#x} ({}B)",
                    lease.ptr,
                    lease.alloc_bytes
                );
                return;
            }
            st.live_bytes = st.live_bytes.saturating_sub(lease.alloc_bytes);

            if event.is_some() || !extra_events.is_empty() {
                if let Some(node) = st.blocks.get_mut(&lease.ptr) {
                    node.state = BlockState::PendingFree;
                }
                let pid = st.next_pending_id;
                st.next_pending_id = st.next_pending_id.wrapping_add(1).max(1);
                st.pending_bytes += lease.alloc_bytes;
                st.pending.push(PendingEntry {
                    id: pid,
                    ptr: lease.ptr,
                    size: lease.alloc_bytes,
                    slab_id: lease.slab_id,
                    stream_id,
                    event,
                    extra_events,
                });
            } else {
                // Idle immediately (no event fence or already complete): coalesce into Free.
                cratonvm_types::gpu_event_census::note_alloc_pool_parked();
                let _slab_idle =
                    st.move_to_free_and_coalesce(lease.ptr, lease.alloc_bytes, lease.slab_id);
                reclaimed_now_bytes = lease.alloc_bytes;
                notify = true;
            }
        }

        if reclaimed_now_bytes > 0 {
            self.sub_vram_used(reclaimed_now_bytes as u64);
        }
        if notify {
            notify_vram_freed();
        }
    }

    /// Non-blocking reclamation: poll every `PendingFree` event with
    /// [`FenceEvent::is_complete`], moving retired blocks to coalesced `Free`,
    /// updating global VRAM counters, and returning the count of blocks reclaimed.
    pub fn reclaim_completed(&self) -> usize {
        // Snapshot pending fences without draining `st.pending` so concurrent
        // `has_pending_erased()`, `take_same_stream_pending()`, and
        // `sync_pending_and_reclaim()` never lose visibility of in-flight blocks.
        let snapshot: Vec<(u64, Option<E>, Vec<E>)> = {
            let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if st.pending.is_empty() {
                return 0;
            }
            st.pending
                .iter()
                .map(|e| (e.id, e.event.clone(), e.extra_events.clone()))
                .collect()
        };

        // Poll fence events outside `self.state` so driver `cuEventQuery`
        // calls never serialize pool allocations/frees on other threads.
        let completed_ids: Vec<u64> = snapshot
            .into_iter()
            .filter_map(|(id, ev, extra)| {
                let done = ev.as_ref().is_none_or(|e| e.is_complete())
                    && extra.iter().all(|e| e.is_complete());
                done.then_some(id)
            })
            .collect();

        if completed_ids.is_empty() {
            return 0;
        }

        let mut reclaimed_count = 0usize;
        let mut reclaimed_bytes = 0usize;
        let mut native_to_free: Vec<(u64, Option<u64>)> = Vec::new();
        {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let mut i = 0;
            while i < st.pending.len() {
                let id = st.pending[i].id;
                if completed_ids.contains(&id) || st.pending[i].all_complete() {
                    let entry = st.pending.swap_remove(i);
                    st.pending_bytes = st.pending_bytes.saturating_sub(entry.size);
                    reclaimed_bytes += entry.size;
                    reclaimed_count += 1;
                    if entry.slab_id == NATIVE_STREAM_POOL_SLAB_ID {
                        native_to_free.push((entry.ptr, entry.stream_id));
                    } else {
                        cratonvm_types::gpu_event_census::note_alloc_pool_parked();
                        let _slab_idle =
                            st.move_to_free_and_coalesce(entry.ptr, entry.size, entry.slab_id);
                    }
                } else {
                    i += 1;
                }
            }
        }
        for (ptr, stream_id) in native_to_free {
            let _ = self.backend.free_to_stream_pool(ptr, stream_id);
        }

        if reclaimed_bytes > 0 {
            self.sub_vram_used(reclaimed_bytes as u64);
        }
        if reclaimed_count > 0 {
            notify_vram_freed();
        }
        reclaimed_count
    }

    /// Release completely idle backing slabs back to [`SlabBackend::free_slab`],
    /// returning the number of slabs freed.
    pub fn trim_idle_slabs(&self) -> usize {
        if self.uses_native_stream_pool() {
            let _ = self.backend.trim_stream_pool(0);
        }
        let drained = {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.drain_idle_slabs()
        };
        let count = drained.len();
        for slab in drained {
            self.backend.free_slab(slab);
        }
        self.backend.trim_parked();
        count
    }

    /// Return a diagnostic snapshot of current pool usage and fragmentation.
    pub fn stats(&self) -> AllocatorStats {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let free_block_count = st.buckets.iter().map(Vec::len).sum();
        AllocatorStats {
            backing_bytes: st.backing_bytes,
            live_bytes: st.live_bytes,
            pending_bytes: st.pending_bytes,
            free_bytes: st.free_bytes,
            free_block_count,
            pending_count: st.pending.len(),
            slab_count: st.slabs.len() + st.unpooled_slabs.len(),
        }
    }
}

impl<B: SlabBackend, E: FenceEvent> ReclaimablePool for SubAllocator<B, E> {
    fn reclaim_completed_erased(&self) -> usize {
        self.reclaim_completed()
    }

    fn trim_idle_slabs_erased(&self) -> usize {
        self.trim_idle_slabs()
    }

    fn has_pending_erased(&self) -> bool {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        !st.pending.is_empty()
    }
}

impl<B: SlabBackend, E: FenceEvent> Drop for SubAllocator<B, E> {
    fn drop(&mut self) {
        let (slabs_to_free, unpooled_to_free, managed_to_free, pending_events, unaccounted_used) = {
            let st = self.state.get_mut().unwrap_or_else(|p| p.into_inner());
            let mut slabs = Vec::with_capacity(st.slabs.len());
            for (_, rec) in st.slabs.drain() {
                slabs.push(rec.backing);
            }
            let unpooled: Vec<BackingSlab<B::Slab>> =
                st.unpooled_slabs.drain().map(|(_, s)| s).collect();
            let managed: Vec<BackingSlab<B::Slab>> =
                st.managed_slabs.drain().map(|(_, s)| s).collect();
            let used = st.live_bytes.saturating_add(st.pending_bytes) as u64;
            let pending_events: Vec<E> = st
                .pending
                .drain(..)
                .flat_map(|p| p.event.into_iter().chain(p.extra_events))
                .collect();
            st.blocks.clear();
            for b in &mut st.buckets {
                b.clear();
            }
            st.live_bytes = 0;
            st.pending_bytes = 0;
            st.free_bytes = 0;
            st.backing_bytes = 0;
            (slabs, unpooled, managed, pending_events, used)
        };
        for ev in pending_events {
            let _ = ev.sync();
        }
        self.sub_vram_used(unaccounted_used);
        let spilled = self.spill_used_bytes.swap(0, Ordering::Relaxed);
        if spilled > 0 {
            let _ = self.quota_slot.spill_used.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |cur| Some(cur.saturating_sub(spilled)),
            );
        }
        for s in slabs_to_free {
            self.backend.free_slab(s);
        }
        for s in unpooled_to_free {
            self.backend.free_slab(s);
        }
        for s in managed_to_free {
            self.backend.free_slab(s);
        }
    }
}

// ---------------------------------------------------------------------------
// CUDA backing slab implementation (VMM with cuMemAlloc/cuMemFree fallback)
// ---------------------------------------------------------------------------

#[cfg(feature = "cuda")]
mod cuda_backend {
    use std::sync::Arc;

    use cudarc::driver::CudaDevice;

    use super::{BackingSlab, SlabBackend, SubAllocator, DEFAULT_SLAB_BYTES};
    use crate::vmm::{
        probe_vmm_support, CudaVmmDriver, VmmPool, VmmSupport, DEFAULT_VMM_RESERVE_BYTES,
    };
    use crate::{DeviceError, Event, Result};

    /// Tag distinguishing VMM-mapped slabs, `cuMemAlloc` slabs, and Managed spill slabs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CudaSlabKind {
        /// Backed by 2 MiB-aligned CUDA Driver VMM physical chunks.
        Vmm,
        /// Backed by `cuMemAlloc` (`malloc_sync` / `free_sync`).
        RawMalloc,
        /// Backed by CUDA Managed / Unified Memory (`cuMemAllocManaged`).
        Managed,
    }

    /// CUDA [`SlabBackend`] that prefers Driver VMM and falls back to `cuMemAlloc`,
    /// with optional CUDA 11.2+ `cuMemAllocFromPoolAsync` stream-ordered pool support.
    pub struct CudaSlabBackend {
        dev: Arc<CudaDevice>,
        vmm: Option<VmmPool<CudaVmmDriver>>,
        vmm_support: VmmSupport,
        stream_pool: Option<cudarc::driver::sys::CUmemoryPool>,
    }

    // SAFETY: `CUmemoryPool` is an opaque driver handle serialized by the CUDA
    // driver and only used after `self.dev.bind_to_thread()`.
    unsafe impl Send for CudaSlabBackend {}
    unsafe impl Sync for CudaSlabBackend {}

    fn probe_cuda_default_mem_pool(
        dev: &Arc<CudaDevice>,
    ) -> Option<cudarc::driver::sys::CUmemoryPool> {
        dev.bind_to_thread().ok()?;
        let mut supported: i32 = 0;
        let attr = cudarc::driver::sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_MEMORY_POOLS_SUPPORTED;
        // SAFETY: `supported` is a valid out-pointer and `dev.cu_device()` is a live CUdevice.
        let status = unsafe {
            cudarc::driver::sys::lib().cuDeviceGetAttribute(&mut supported, attr, *dev.cu_device())
        };
        if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS || supported == 0 {
            return None;
        }
        let mut pool: cudarc::driver::sys::CUmemoryPool = std::ptr::null_mut();
        // SAFETY: `pool` is a valid out-pointer; `dev.cu_device()` is live.
        let status = unsafe {
            cudarc::driver::sys::lib().cuDeviceGetDefaultMemPool(&mut pool, *dev.cu_device())
        };
        if status == cudarc::driver::sys::CUresult::CUDA_SUCCESS && !pool.is_null() {
            Some(pool)
        } else {
            None
        }
    }

    impl CudaSlabBackend {
        /// Probe CUDA Driver VMM and stream-ordered pool support on `dev`.
        pub fn new(dev: Arc<CudaDevice>) -> Self {
            Self::new_for_vm(dev, 0, 0)
        }

        /// Probe CUDA Driver VMM and stream-ordered pool support on `dev` scoped
        /// to `(vm_identity, device_ordinal)` for unified quota tracking.
        pub fn new_for_vm(dev: Arc<CudaDevice>, vm_identity: u64, device_ordinal: u32) -> Self {
            let vmm_support = probe_vmm_support(&dev);
            let vmm = if vmm_support.supported {
                let drv = CudaVmmDriver::new(dev.clone(), vmm_support);
                let pool = VmmPool::new_for_vm(
                    drv,
                    DEFAULT_VMM_RESERVE_BYTES,
                    vm_identity,
                    device_ordinal,
                );
                let mut target_prewarm = crate::vmm::vmm_prewarm_min_chunks();
                if target_prewarm > 0 {
                    let chunk = pool.chunk_bytes() as u64;
                    let g_limit = super::vram_reservation_limit_bytes();
                    let p_limit = super::quota_slot_for(vm_identity, device_ordinal)
                        .limit
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let eff_limit = match (g_limit, p_limit) {
                        (0, p) => p,
                        (g, 0) => g,
                        (g, p) => g.min(p),
                    };
                    if eff_limit > 0 {
                        target_prewarm = target_prewarm.min((eff_limit / chunk.max(1)) as usize);
                    }
                    if target_prewarm > 0 {
                        pool.prewarm_async(target_prewarm);
                    }
                }
                Some(pool)
            } else {
                None
            };
            let stream_pool = probe_cuda_default_mem_pool(&dev);
            Self {
                dev,
                vmm,
                vmm_support,
                stream_pool,
            }
        }

        /// Probed VMM capabilities for this context's device.
        pub fn vmm_support(&self) -> VmmSupport {
            self.vmm_support
        }

        /// Direct access to the underlying [`VmmPool`] when VMM is active.
        pub fn vmm_pool(&self) -> Option<&VmmPool<CudaVmmDriver>> {
            self.vmm.as_ref()
        }

        /// Reserve a dedicated graph-scoped VMM virtual range when VMM is active.
        pub fn reserve_graph_vmm_arena(
            &self,
            capacity_bytes: usize,
        ) -> Result<Option<Arc<crate::vmm::GraphVmmArena<CudaVmmDriver>>>> {
            let Some(ref vmm) = self.vmm else {
                return Ok(None);
            };
            Ok(Some(vmm.reserve_graph_arena(capacity_bytes)?))
        }

        fn map_err<E: std::fmt::Debug>(stage: &str) -> impl FnOnce(E) -> DeviceError + '_ {
            move |e| DeviceError::Driver(format!("{stage}: {e:?}"))
        }
    }

    impl SlabBackend for CudaSlabBackend {
        type Slab = CudaSlabKind;

        fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
            self.alloc_slab_for_stream(min_bytes, None)
        }

        fn alloc_slab_for_stream(
            &self,
            min_bytes: usize,
            stream_id: Option<u64>,
        ) -> Result<BackingSlab<Self::Slab>> {
            if let Some(ref vmm) = self.vmm {
                if let Ok(slab) = vmm.acquire_slab_for_stream(min_bytes, stream_id) {
                    return Ok(BackingSlab {
                        base_ptr: slab.base_ptr,
                        size_bytes: slab.size_bytes,
                        handle: CudaSlabKind::Vmm,
                    });
                }
            }

            self.dev
                .bind_to_thread()
                .map_err(Self::map_err("bind_to_thread"))?;
            // SAFETY: context is bound; `min_bytes` is a positive allocation size.
            let ptr = unsafe {
                cudarc::driver::result::malloc_sync(min_bytes).map_err(Self::map_err("cuMemAlloc"))?
            };
            Ok(BackingSlab {
                base_ptr: ptr,
                size_bytes: min_bytes,
                handle: CudaSlabKind::RawMalloc,
            })
        }

        fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
            match slab.handle {
                CudaSlabKind::Vmm => {
                    if let Some(ref vmm) = self.vmm {
                        let _ = vmm.unmap_chunks(slab.base_ptr, slab.size_bytes, true);
                    }
                }
                CudaSlabKind::RawMalloc | CudaSlabKind::Managed => {
                    let _ = self.dev.bind_to_thread();
                    // SAFETY: `slab.base_ptr` came from `malloc_sync` or
                    // `cuMemAllocManaged`, all suballocations inside it are freed,
                    // and the context is bound.
                    unsafe {
                        let _ = cudarc::driver::result::free_sync(slab.base_ptr);
                    }
                }
            }
        }

        fn defragment_for(&self, min_bytes: usize) -> Result<Option<BackingSlab<Self::Slab>>> {
            let Some(ref vmm) = self.vmm else {
                return Ok(None);
            };
            let chunk = vmm.chunk_bytes().max(1);
            let min_chunks = min_bytes.div_ceil(chunk).max(1);
            match vmm.defragment_or_remap(min_chunks)? {
                Some(slab) => Ok(Some(BackingSlab {
                    base_ptr: slab.base_ptr,
                    size_bytes: slab.size_bytes,
                    handle: CudaSlabKind::Vmm,
                })),
                None => Ok(None),
            }
        }

        fn supports_defragment(&self) -> bool {
            self.vmm.is_some()
        }

        fn park_idle_slab(&self, slab: BackingSlab<Self::Slab>) {
            match slab.handle {
                CudaSlabKind::Vmm => {
                    if let Some(ref vmm) = self.vmm {
                        let _ = vmm.unmap_chunks(slab.base_ptr, slab.size_bytes, false);
                    }
                }
                CudaSlabKind::RawMalloc | CudaSlabKind::Managed => self.free_slab(slab),
            }
        }

        fn alloc_managed_slab(
            &self,
            min_bytes: usize,
            read_mostly: bool,
        ) -> Result<BackingSlab<Self::Slab>> {
            self.dev
                .bind_to_thread()
                .map_err(Self::map_err("bind_to_thread"))?;
            let mut dptr: cudarc::driver::sys::CUdeviceptr = 0;
            let flags = cudarc::driver::sys::CUmemAttach_flags::CU_MEM_ATTACH_GLOBAL as u32;
            // SAFETY: context is bound; `dptr` is a valid out-pointer; `min_bytes > 0`.
            let status = unsafe {
                cudarc::driver::sys::lib().cuMemAllocManaged(&mut dptr, min_bytes, flags)
            };
            if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS || dptr == 0 {
                return Err(DeviceError::Driver(format!(
                    "cuMemAllocManaged({min_bytes}): {status:?}"
                )));
            }
            if read_mostly {
                // Best-effort read-mostly advice so Unified Memory can replicate
                // hot pages onto GPU VRAM without blocking host/device migration.
                // SAFETY: `dptr..dptr+min_bytes` is valid managed allocation on `cu_device`.
                unsafe {
                    let _ = cudarc::driver::sys::lib().cuMemAdvise(
                        dptr,
                        min_bytes,
                        cudarc::driver::sys::CUmem_advise::CU_MEM_ADVISE_SET_READ_MOSTLY,
                        *self.dev.cu_device(),
                    );
                }
            }
            Ok(BackingSlab {
                base_ptr: dptr,
                size_bytes: min_bytes,
                handle: CudaSlabKind::Managed,
            })
        }

        fn trim_parked(&self) {
            if let Some(ref vmm) = self.vmm {
                let _ = vmm.release_unused_chunks(true);
            }
        }

        fn mark_slab_idle(&self, slab: &BackingSlab<Self::Slab>) {
            if slab.handle == CudaSlabKind::Vmm {
                if let Some(ref vmm) = self.vmm {
                    vmm.mark_chunks_idle(slab.base_ptr, slab.size_bytes);
                }
            }
        }

        fn supports_native_stream_pool(&self) -> bool {
            self.stream_pool.is_some()
        }

        fn alloc_from_stream_pool(&self, bytes: usize, stream_id: Option<u64>) -> Result<Option<u64>> {
            let Some(pool) = self.stream_pool else {
                return Ok(None);
            };
            self.dev
                .bind_to_thread()
                .map_err(Self::map_err("bind_to_thread"))?;
            let stream = stream_id.unwrap_or(0) as usize as cudarc::driver::sys::CUstream;
            let mut dptr: cudarc::driver::sys::CUdeviceptr = 0;
            // SAFETY: context is bound; `pool` is the device's default memory pool
            // and `stream` is either a live CUstream or the default null stream (0).
            let status = unsafe {
                cudarc::driver::sys::lib().cuMemAllocFromPoolAsync(&mut dptr, bytes, pool, stream)
            };
            if status == cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                Ok(Some(dptr))
            } else {
                Err(DeviceError::Driver(format!(
                    "cuMemAllocFromPoolAsync({bytes}): {status:?}"
                )))
            }
        }

        fn free_to_stream_pool(&self, ptr: u64, stream_id: Option<u64>) -> Result<()> {
            if self.stream_pool.is_none() || ptr == 0 {
                return Ok(());
            }
            let _ = self.dev.bind_to_thread();
            let stream = stream_id.unwrap_or(0) as usize as cudarc::driver::sys::CUstream;
            // SAFETY: `ptr` was allocated via `cuMemAllocFromPoolAsync` on this context.
            let status = unsafe { cudarc::driver::sys::lib().cuMemFreeAsync(ptr, stream) };
            if status == cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                Ok(())
            } else {
                Err(DeviceError::Driver(format!("cuMemFreeAsync: {status:?}")))
            }
        }

        fn trim_stream_pool(&self, min_bytes_to_keep: usize) -> Result<()> {
            let Some(pool) = self.stream_pool else {
                return Ok(());
            };
            let _ = self.dev.bind_to_thread();
            // SAFETY: `pool` is a valid `CUmemoryPool` on the bound context.
            let status = unsafe {
                cudarc::driver::sys::lib().cuMemPoolTrimTo(pool, min_bytes_to_keep)
            };
            if status == cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                Ok(())
            } else {
                Err(DeviceError::Driver(format!("cuMemPoolTrimTo: {status:?}")))
            }
        }
    }

    /// Concrete CUDA suballocator type used by [`crate::backend::DeviceContextInner`].
    pub type CudaSubAllocator = SubAllocator<CudaSlabBackend, Arc<Event>>;

    /// Construct a [`CudaSubAllocator`] for `dev`.
    pub fn new_cuda_suballocator(
        dev: Arc<CudaDevice>,
        pooling_enabled: bool,
    ) -> Arc<CudaSubAllocator> {
        new_cuda_suballocator_with_quota(dev, pooling_enabled, 0, 0)
    }

    /// Construct a [`CudaSubAllocator`] for `dev` bound to `(vm_identity, device_ordinal)`.
    pub fn new_cuda_suballocator_with_quota(
        dev: Arc<CudaDevice>,
        pooling_enabled: bool,
        vm_identity: u64,
        device_ordinal: u32,
    ) -> Arc<CudaSubAllocator> {
        let backend = CudaSlabBackend::new_for_vm(dev, vm_identity, device_ordinal);
        let default_slab = if backend.vmm_support().supported {
            backend.vmm_support().chunk_bytes.max(DEFAULT_SLAB_BYTES)
        } else {
            DEFAULT_SLAB_BYTES
        };
        SubAllocator::new_arc_with_quota(
            backend,
            default_slab,
            pooling_enabled,
            vm_identity,
            device_ordinal,
        )
    }

    /// Construct a [`CudaSubAllocator`] for `ctx`.
    pub fn new_cuda_suballocator_for_context(
        ctx: &crate::DeviceContext,
        pooling_enabled: bool,
    ) -> Arc<CudaSubAllocator> {
        new_cuda_suballocator(Arc::clone(ctx.inner().device()), pooling_enabled)
    }

    /// Construct a [`CudaSubAllocator`] for `ctx` bound to `(vm_identity, device_ordinal)`.
    pub fn new_cuda_suballocator_for_context_with_quota(
        ctx: &crate::DeviceContext,
        pooling_enabled: bool,
        vm_identity: u64,
        device_ordinal: u32,
    ) -> Arc<CudaSubAllocator> {
        new_cuda_suballocator_with_quota(
            Arc::clone(ctx.inner().device()),
            pooling_enabled,
            vm_identity,
            device_ordinal,
        )
    }
}

#[cfg(feature = "cuda")]
pub use cuda_backend::{
    new_cuda_suballocator, new_cuda_suballocator_for_context,
    new_cuda_suballocator_for_context_with_quota, new_cuda_suballocator_with_quota,
    CudaSlabBackend, CudaSlabKind, CudaSubAllocator,
};
