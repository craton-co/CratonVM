//! Two censuses that answer "what is the interpreter actually running, and why
//! was it never nominated for compilation".
//!
//! Both are DEFAULT-OFF and cost one relaxed `AtomicBool` load each on their
//! hot path. They exist because the composition page
//! (`known-issues/perf/completablefuture-composition-is-20x-and-5-percent-compiled-20260901.md`)
//! reached a profile whose largest bucket is "the interpreter and the plumbing
//! around it" with no instrument able to name a single METHOD in that bucket:
//! `jit-method-stats` ranges only over NOMINATED methods, so its population is
//! the answer's complement.
//!
//! * `CRATONVM_DBG_INTERP_FRAMES=1` — one row per method per interpreted frame
//!   push. This is the census of what runs interpreted.
//! * `CRATONVM_DBG_TIERUP_DECLINE=1` — one row per cached `invokevirtual`
//!   dispatch, keyed by the FIRST condition in `execute_invokevirtual_cached`'s
//!   tier-up chain that refused it. A method that appears here with a large
//!   count and a reason other than `admitted` is a method the invocation
//!   counter never saw, which is why lowering `CRATONVM_JIT_THRESHOLD` cannot
//!   reach it.
//!
//! Both dump at exit from `vm-cli`'s report block, sorted by count.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

static INTERP_FRAMES_ON: AtomicU8 = AtomicU8::new(0);
static TIERUP_DECLINE_ON: AtomicU8 = AtomicU8::new(0);

/// 0 = not yet read, 1 = off, 2 = on. Self-initialising rather than wired into
/// `SharedVm::new`: the two hot sites are reached from several entry points
/// (including the ones a unit test drives directly), and a gate that depends on
/// an init call it might not get is a gate that reads OFF for reasons that have
/// nothing to do with the switch.
#[inline(always)]
fn gate(cell: &AtomicU8, var: &str) -> bool {
    match cell.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_var_os(var).is_some();
            cell.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

#[inline(always)]
pub fn interp_frames_enabled() -> bool {
    gate(&INTERP_FRAMES_ON, "CRATONVM_DBG_INTERP_FRAMES")
}

#[inline(always)]
pub fn tierup_decline_enabled() -> bool {
    gate(&TIERUP_DECLINE_ON, "CRATONVM_DBG_TIERUP_DECLINE")
}

static DIRECT_BINDS_ON: AtomicU8 = AtomicU8::new(0);

/// `CRATONVM_DBG_DIRECT_BINDS=1` — the thin-direct-call engagement census.
#[inline(always)]
pub fn direct_binds_enabled() -> bool {
    gate(&DIRECT_BINDS_ON, "CRATONVM_DBG_DIRECT_BINDS")
}

/// `Integer.intValue()` thin-direct-call engagement.
///
/// `sites_bound` counts BIND EVENTS, not distinct call sites: a method compiled
/// at C1 and again at C2 binds its sites twice. `HibfixComposeProbe2` has two
/// `intValue` sites in `lambda$chain$0` and reports 4.
///
/// `[0]` calls served by the helper's own field-0 read, `[1]` calls the helper
/// declined back to `jit_invoke_dispatch`. The compile-time site count lives in
/// the `jit` crate (`cratonvm_jit::integer_int_value_direct_sites`), because
/// two of the three doors that bind it are there.
pub static INT_VALUE_DIRECT: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `Long.longValue()` sibling of [`INT_VALUE_DIRECT`].
pub static LONG_VALUE_DIRECT: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Count one served (`declined = false`) or declined (`true`) direct call.
/// Caller has already tested [`direct_binds_enabled`].
#[inline]
pub fn note_int_value_direct(declined: bool) {
    INT_VALUE_DIRECT[usize::from(declined)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// [`note_int_value_direct`] for the `Long.longValue()` helper.
#[inline]
pub fn note_long_value_direct(declined: bool) {
    LONG_VALUE_DIRECT[usize::from(declined)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

type Census = Mutex<HashMap<String, u64>>;

fn interp_census() -> &'static Census {
    static C: OnceLock<Census> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn decline_census() -> &'static Census {
    static C: OnceLock<Census> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one interpreted frame push. Caller has already tested
/// [`interp_frames_enabled`].
#[cold]
pub fn record_interp_frame(class: &str, method: &str, descriptor: &str) {
    let key = format!("{class}.{method}{descriptor}");
    if let Ok(mut m) = interp_census().lock() {
        *m.entry(key).or_insert(0) += 1;
    }
}

/// Record one cached `invokevirtual` dispatch and the first tier-up condition
/// that refused it (`"admitted"` when none did). Caller has already tested
/// [`tierup_decline_enabled`].
#[cold]
pub fn record_tierup_decline(reason: &str, class: &str, method: &str, descriptor: &str) {
    let key = format!("{reason} {class}.{method}{descriptor}");
    if let Ok(mut m) = decline_census().lock() {
        *m.entry(key).or_insert(0) += 1;
    }
}

fn dump(label: &str, c: &Census, top: usize) {
    let Ok(m) = c.lock() else { return };
    if m.is_empty() {
        return;
    }
    let mut rows: Vec<(&String, &u64)> = m.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let total: u64 = rows.iter().map(|(_, n)| **n).sum();
    eprintln!("[{label}] {} distinct rows, {total} total", rows.len());
    for (k, n) in rows.iter().take(top) {
        eprintln!("[{label}] {n:>12} {k}");
    }
}

/// Print both censuses. Called from `vm-cli`'s exit report block; a census
/// that was never armed prints nothing.
pub fn report_at_exit() {
    dump("interp-frames", interp_census(), 60);
    dump("tierup-decline", decline_census(), 60);
    // C1→C2 supersede engagement. `unchanged`/`first-publish` are the two
    // outcomes that cannot invalidate anything, and `ic_evictions` is what the
    // epoch bump actually costs — the number of `Jit` invoke-cache entries it
    // threw away process-wide. Printed together because the second is the only
    // thing that makes the first two worth acting on.
    let (first_publish, unchanged, changed) =
        crate::runtime::interpreter::jit_bridge::supersede_census();
    if crate::runtime::env_cache::dbg_jitc() && first_publish + unchanged + changed > 0 {
        eprintln!(
            "[c2-supersede] publishes: first_publish={first_publish} unchanged={unchanged} changed={changed}; ic_evictions_from_epoch={}",
            cratonvm_classloading::epoch_stale_evictions(),
        );
    }
    if direct_binds_enabled() {
        eprintln!(
            "[direct-binds] Integer.intValue: sites_bound={} served={} declined_to_dispatch={}",
            cratonvm_jit::integer_int_value_direct_sites(),
            INT_VALUE_DIRECT[0].load(Ordering::Relaxed),
            INT_VALUE_DIRECT[1].load(Ordering::Relaxed),
        );
        eprintln!(
            "[direct-binds] Long.longValue: sites_bound={} served={} declined_to_dispatch={}",
            cratonvm_jit::long_long_value_direct_sites(),
            LONG_VALUE_DIRECT[0].load(Ordering::Relaxed),
            LONG_VALUE_DIRECT[1].load(Ordering::Relaxed),
        );
    }
}
