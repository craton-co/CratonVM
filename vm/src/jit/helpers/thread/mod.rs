#[allow(unused_imports)]
pub(super) use super::*;
// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// JIT runtime helper functions — called from JIT-compiled code via absolute CALL.
//
// These functions need access to `SharedVm` and other VM internals, so they
// live in the VM crate rather than the standalone JIT crate.
//
// Most helpers are a thin `extern "C"` shell around a `*_body` function run
// under [`crate::jit::helper_guard::contain`], so a panic in the body becomes
// the helper's failure sentinel instead of a process abort. That module lists
// which helpers are guarded, with which sentinel and policy, and which are
// deliberately not.

use std::cell::Cell;

use cratonvm_jit::{DescriptorParamIter, JitInvokeInfo, JitMICSlot, JitPICSlot, JitRuntimeHelpers};
use cratonvm_native_api::{
    NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_types::{
    ArrayElementType, ClassId, ObjectRef, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};

use crate::jit::helper_guard::{contain, OnPanic};
use crate::memory::vm_heap::VmHeap;
// `class_was_redefined` is this file's memoized wrapper of the
// `redefine_state` function of that name (round 12 wave 2, lane rt).
use crate::runtime::redefine_state::{class_id_or_name_was_redefined, named_class_was_redefined};
use crate::threading::jvm_thread::JvmThread;
use crate::threading::thread_state::{self, ThreadExecState};
use crate::vm::SharedVm;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, write_ref_slot};
// ---------------------------------------------------------------------------
// WS1 diagnostic profiling for the JIT dispatch helpers
// (env-gated: CRATONVM_DBG_MIC_PROF=1; zero-cost when off beyond one cached
// bool branch). Dumps cumulative path counts + rdtsc cycle totals to stderr
// every 2^24 `jit_invoke_virtual_mic` entries, so the last dump before exit
// approximates the run's totals.
// ---------------------------------------------------------------------------

pub mod mic_prof {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static MIC_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static MIC_HIT_ENTRY: AtomicU64 = AtomicU64::new(0);
    pub static MIC_HIT_NOENTRY: AtomicU64 = AtomicU64::new(0);
    pub static MIC_MISS: AtomicU64 = AtomicU64::new(0);
    pub static MIC_LAMBDA: AtomicU64 = AtomicU64::new(0);
    pub static CYC_MIC_TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static CYC_HIT_ENTRY_CALL: AtomicU64 = AtomicU64::new(0);
    pub static CYC_INVOKE: AtomicU64 = AtomicU64::new(0);
    pub static CYC_COMPILE_PROBE: AtomicU64 = AtomicU64::new(0);
    pub static DISP_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static CYC_DISP_TOTAL: AtomicU64 = AtomicU64::new(0);

    /// Why an entryless MIC hit failed to leave a callable entry behind.
    /// `hit_entry == 0` while `hit_noentry` climbs means the slot never learns
    /// a target, so every call re-pays the compile probe. These three separate
    /// the candidate causes instead of leaving them to inference:
    ///   * `probe_none`  — `try_jit_compile_callee` handed back nothing;
    ///   * `barred`      — an exception-table / indy-trap callee, deliberately
    ///                     kept on the helper path;
    ///   * `published`   — `mic.update` was actually called with an entry, so a
    ///                     still-zero `hit_entry` means the entry was
    ///                     downgraded inside `jit_entry_publishable`.
    pub static PUB_PROBE_NONE: AtomicU64 = AtomicU64::new(0);
    pub static PUB_BARRED: AtomicU64 = AtomicU64::new(0);
    pub static PUB_PUBLISHED: AtomicU64 = AtomicU64::new(0);

    /// `pub_probe_none` collapses FOUR distinct events into one number. On
    /// `ZipContentTests` it read *exactly* equal to `hit_noentry` with
    /// `pub_published=0` — a signature that says "the cache never learns a
    /// target" but cannot say why, because three of the four ways to reach it
    /// never run a probe at all:
    ///
    ///   * `not_probed_disabled`    — `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`
    ///   * `not_probed_redefine`    — a redefinition has quiesced JIT dispatch
    ///   * `not_probed_uncacheable` — the receiver is not cacheable at this site
    ///   * `probe_returned_none`    — a probe DID run and `try_jit_compile_callee`
    ///                                declined it
    ///
    /// Only the last is a compile refusal; the other three are gates, and the
    /// fix for each is a different piece of code. `pub_probe_none` stays as
    /// their sum so numbers from earlier runs still compare.
    pub static NOT_PROBED_DISABLED: AtomicU64 = AtomicU64::new(0);
    pub static NOT_PROBED_REDEFINE: AtomicU64 = AtomicU64::new(0);
    pub static NOT_PROBED_UNCACHEABLE: AtomicU64 = AtomicU64::new(0);
    pub static PROBE_RETURNED_NONE: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || false)    }

    /// `CRATONVM_DBG_MIC_TRACE` — the per-call `[DISP_TRACE]` line, separate
    /// from [`enabled`] because one `eprintln` per dispatched call is only
    /// affordable when dispatch is cold. It is not cold on a call-dense
    /// workload, where it buries the counters it shares a switch with under
    /// hundreds of megabytes of stderr.
    pub fn trace_enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || false)    }

    #[inline]
    #[cfg(target_arch = "x86_64")]
    pub fn now() -> u64 {
        // SAFETY: rdtsc is unprivileged on x86-64.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// The same counter off x86-64, where `_rdtsc` does not exist.
    ///
    /// AArch64's equivalent is the virtual counter `CNTVCT_EL0`, unprivileged
    /// under Linux by default. It ticks at a fixed frequency rather than the
    /// core clock, so these numbers are NOT comparable across architectures --
    /// which is fine, because every consumer uses them as deltas within one
    /// process. Anything else falls back to the monotonic clock so the counters
    /// keep working rather than the crate refusing to build.
    #[inline]
    #[cfg(not(target_arch = "x86_64"))]
    pub fn now() -> u64 {
        #[cfg(target_arch = "aarch64")]
        {
            let cnt: u64;
            // SAFETY: CNTVCT_EL0 is a read-only counter, unprivileged on Linux.
            unsafe { std::arch::asm!("mrs {}, cntvct_el0", out(reg) cnt) };
            cnt
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    }

    /// Counter families for the re-entrancy gate. Depth is tracked PER FAMILY,
    /// not globally: `cyc_invoke` is nested inside `cyc_mic_total` by
    /// construction, so one shared depth would silently stop `cyc_invoke` ever
    /// accumulating — which is precisely the number the funnel question needs.
    const CYC_FAMILIES: usize = 5;

    fn cyc_family(ctr: &'static AtomicU64) -> usize {
        if std::ptr::eq(ctr, &CYC_MIC_TOTAL) {
            0
        } else if std::ptr::eq(ctr, &CYC_HIT_ENTRY_CALL) {
            1
        } else if std::ptr::eq(ctr, &CYC_INVOKE) {
            2
        } else if std::ptr::eq(ctr, &CYC_COMPILE_PROBE) {
            3
        } else {
            4 // CYC_DISP_TOTAL
        }
    }

    thread_local! {
        /// Re-entrancy depth per counter family, so a nested dispatch does not
        /// add its cycles to an enclosing span of the SAME family.
        static CYC_DEPTH: std::cell::Cell<[u32; CYC_FAMILIES]> =
            const { std::cell::Cell::new([0; CYC_FAMILIES]) };
    }

    /// Cycle accumulator that survives early returns.
    ///
    /// **Only the OUTERMOST guard accumulates.** These spans cover the whole
    /// helper *including execution of the callee*, and a compiled callee
    /// routinely dispatches again — so a naive guard counts the inner call's
    /// cycles at every enclosing level. That is not a small distortion: on
    /// `ZipContentTests` the naive `cyc_mic_total` + `cyc_disp_total` summed to
    /// 2.9e12 cycles against a ~260 s CPU run, i.e. roughly 4x more time than
    /// the process actually had, which makes the totals unusable as a share of
    /// anything and quietly invites reading them as one.
    ///
    /// Gating on depth makes them true wall-of-the-outermost-call totals, so
    /// `cyc_mic_total - cyc_invoke` is the funnel's own cost rather than a
    /// number contaminated by every callee beneath it.
    pub struct CycGuard {
        t0: u64,
        ctr: &'static AtomicU64,
        family: usize,
    }
    impl CycGuard {
        pub fn new(ctr: &'static AtomicU64) -> Option<Self> {
            if !enabled() {
                return None;
            }
            let family = cyc_family(ctr);
            let outermost = CYC_DEPTH.with(|d| {
                let mut cur = d.get();
                let was = cur[family];
                cur[family] = was + 1;
                d.set(cur);
                was == 0
            });
            // `u64::MAX` marks "nested — hold the depth, accumulate nothing".
            Some(Self {
                t0: if outermost { now() } else { u64::MAX },
                ctr,
                family,
            })
        }
    }
    impl Drop for CycGuard {
        fn drop(&mut self) {
            let family = self.family;
            CYC_DEPTH.with(|d| {
                let mut cur = d.get();
                cur[family] = cur[family].saturating_sub(1);
                d.set(cur);
            });
            // `u64::MAX` is the "nested, do not accumulate" marker set in `new`.
            if self.t0 != u64::MAX {
                self.ctr
                    .fetch_add(now().wrapping_sub(self.t0), Ordering::Relaxed);
            }
        }
    }

    #[inline]
    pub fn bump(ctr: &'static AtomicU64) {
        if enabled() {
            ctr.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counterpart of [`dump_maybe`] driven from `jit_invoke_dispatch`, so a
    /// workload that's hot through the dispatch helper but cold through the
    /// MIC helper still produces dumps.
    pub fn dump_maybe_disp() {
        if !enabled() {
            return;
        }
        let calls = DISP_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        if calls & ((1 << 20) - 1) != 0 {
            return;
        }
        dump_now();
    }

    pub fn dump_maybe() {
        if !enabled() {
            return;
        }
        let calls = MIC_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        if calls & ((1 << 20) - 1) != 0 {
            return;
        }
        dump_now();
    }

    pub fn dump_now() {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        eprintln!(
            "[MIC_PROF] quiesce_depth={} mic_calls={} hit_entry={} hit_noentry={} miss={} lambda={} \
             cyc_mic_total={} cyc_hit_entry_call={} cyc_invoke={} cyc_compile_probe={} \
             disp_calls={} cyc_disp_total={} \
             pub_probe_none={} (not_probed_disabled={} not_probed_redefine={} \
             not_probed_uncacheable={} probe_returned_none={}) \
             pub_barred={} pub_published={} probe_memo_skips={} \
             owner_reuse={} registry_pins={} ic_refusals={} ic_unowned_pub={}",
            cratonvm_gc::gc_quiescence::depth(),
            g(&MIC_CALLS),
            g(&MIC_HIT_ENTRY),
            g(&MIC_HIT_NOENTRY),
            g(&MIC_MISS),
            g(&MIC_LAMBDA),
            g(&CYC_MIC_TOTAL),
            g(&CYC_HIT_ENTRY_CALL),
            g(&CYC_INVOKE),
            g(&CYC_COMPILE_PROBE),
            g(&DISP_CALLS),
            g(&CYC_DISP_TOTAL),
            g(&PUB_PROBE_NONE),
            g(&NOT_PROBED_DISABLED),
            g(&NOT_PROBED_REDEFINE),
            g(&NOT_PROBED_UNCACHEABLE),
            g(&PROBE_RETURNED_NONE),
            g(&PUB_BARRED),
            g(&PUB_PUBLISHED),
            super::MIC_COMPILE_PROBE_MEMO_SKIPS.load(Ordering::Relaxed),
            super::CACHED_ENTRY_OWNER_REUSE_HITS.load(Ordering::Relaxed),
            super::CACHED_ENTRY_REGISTRY_PINS.load(Ordering::Relaxed),
            cratonvm_jit::unowned_ic_entry_refusals(),
            cratonvm_jit::unowned_ic_entry_publishes(),
        );
        // `pub_probe_none` says the inline cache never learned an entry; the
        // tally says WHICH callees were refused and under which of the four
        // reasons. Together they are a diagnosis; separately they are the
        // "hit_entry=0 forever" observation that has been made before and left
        // unexplained. No-op unless `CRATONVM_DBG=callee-probe` is also set.
        crate::runtime::interpreter::jit_bridge::dump_callee_probe_tally();
        super::disp_census::report();
        // The guarded inline `getfield`'s miss count, printed next to the
        // dispatch census because it answers the same class of question about a
        // different fast path: not "was it emitted" but "did it ever run".
        // `probes/AccessorDispatchProbe.java` supplies an exact denominator.
        eprintln!(
            "[GETFIELD_CENSUS] helper_calls={}",
            super::GETFIELD_HELPER_CALLS.load(Ordering::Relaxed)
        );
        // The JIT reference-slot census, printed beside the two above because it
        // answers the third question in the same family. Those two ask "was it
        // emitted" and "did it fall through to the helper"; this one asks
        // whether the ZGC load-barrier seam those helpers funnel through ever
        // RAN. Without a denominator, a seam wired into an arm that never
        // executes and a seam that runs cleanly are the same silence -- the
        // failure mode this file's other censuses were each built after. The
        // colored-word tripwire counters print even when the per-site half is
        // switched off.
        super::ref_load_census::report();
    }
}

/// WHAT the generic dispatch helper's calls are — by invoke kind, and by which
/// arm answered them.
///
/// `disp_calls` alone is a denominator with no shape. On netty's
/// `BigEndianHeapByteBufTest` it read **110 863 473** for a 63 s run against
/// 6 203 669 `jit_invoke_virtual_mic` calls, i.e. the run's 113 890 789
/// `jit_entries` are overwhelmingly this helper — and nothing said whether they
/// were natives, compiled callees reached through a thread-local cache, or the
/// `invoke_or_native` tail. Those three have completely different fixes, and
/// `perf` cannot separate them: the samples land in the same handful of
/// symbols whichever arm called them.
///
/// Rides on `CRATONVM_DBG=mic-prof` — one more line on a dump that is already
/// the place this question gets asked, and one relaxed `fetch_add` behind the
/// same gate as every other counter here.
pub mod disp_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `invoke_kind` 0-3 (virtual, special, interface, static).
    pub const KIND: [usize; 4] = [0, 1, 2, 3];
    /// The exact-receiver object-native cache (`HashMap`/`Matcher`/`StringBuilder`).
    pub const OUT_OBJECT_NATIVE: usize = 4;
    /// `try_jit_site_cached_native_dispatch` — the leaf/site native cache.
    pub const OUT_SITE_NATIVE: usize = 5;
    /// The `Integer.valueOf`/`intValue` boxing cache.
    pub const OUT_INTEGER_NATIVE: usize = 6;
    /// `VIRTUAL_DISPATCH_CACHE` — a compiled callee for a virtual site.
    pub const OUT_VIRT_CACHE: usize = 7;
    /// `DISPATCH_CACHE` — a compiled callee for a statically bound site.
    pub const OUT_DCACHE: usize = 8;
    /// A `JitCache` lookup that found a body this site had not cached yet.
    pub const OUT_JCACHE: usize = 9;
    /// The tier-up arm compiled the callee and called it.
    pub const OUT_COMPILED_NOW: usize = 10;
    /// Everything past the fast arms: the `invoke_or_native` tail.
    pub const OUT_TAIL: usize = 11;
    /// `jit_invoke_virtual_mic`: served by the leaf/site native cache.
    pub const MIC_SITE_NATIVE: usize = 12;
    /// `jit_invoke_virtual_mic`: served by a by-name native fast path (Matcher,
    /// `StringBuilder`, the `ClassLoader` resource intercept) or the lambda/SAM
    /// arm — every arm that returns before the MIC's own hit/miss counters.
    pub const MIC_EARLY_OTHER: usize = 13;
    /// `jit_invoke_virtual_mic`: served out of `VIRTUAL_DISPATCH_CACHE` — the
    /// exception-table callees the machine-code caches are barred from holding.
    pub const MIC_RUST_CACHE: usize = 14;
    /// `jit_invoke_virtual_mic`: reached the megamorphic PIC secondary cache.
    pub const MIC_PIC: usize = 15;
    /// An `invokestatic` to an uncompiled callee served from the call site's
    /// cached interpreter frame template (`try_jit_static_bytecode_callee`).
    ///
    /// Counted because the whole point of that path is that it replaces a
    /// by-name resolution, and a version of it that never fires is
    /// indistinguishable from one that fires and buys nothing — the failure
    /// mode this module's other counters exist for.
    pub const OUT_STATIC_BC: usize = 16;
    /// That path DECLINED, per call. A site's refusal is cached, so a large
    /// number here against a small `out_static_bc` means the population is
    /// genuinely ineligible rather than the cache thrashing.
    pub const OUT_STATIC_BC_REFUSED: usize = 17;
    /// An `invokevirtual`/`invokeinterface` to an uncompiled callee served from
    /// the (site, receiver class) cached interpreter frame template
    /// (`try_jit_virtual_bytecode_callee`), from EITHER entry point — the
    /// generic dispatcher's 0/2 arm or `jit_invoke_virtual_mic`'s
    /// hit-noentry/miss arms.
    pub const OUT_VIRTUAL_BC: usize = 18;
    /// That path DECLINED, per call. Read against `out_virtual_bc` the same way
    /// `out_static_bc_refused` is read against `out_static_bc`.
    pub const OUT_VIRTUAL_BC_REFUSED: usize = 19;
    /// An `invokespecial` to an uncompiled callee served from the call site's
    /// cached interpreter frame template (`try_jit_special_bytecode_callee`).
    pub const OUT_SPECIAL_BC: usize = 20;
    /// That path DECLINED, per call.
    pub const OUT_SPECIAL_BC_REFUSED: usize = 21;
    /// `jit_invoke_virtual_mic`'s resolving miss arm entered the body its
    /// compile probe had just answered, instead of the by-name tail (round 12
    /// wave 1, lane `calls`, W18-3).
    pub const MIC_MISS_CALLED_COMPILED: usize = 22;
    /// `jit_invoke_virtual_mic`: served by the VM's shared megamorphic
    /// dispatch table (round 12 wave 2, lane `calls`, W2-1) after the site's
    /// own caches missed.
    pub const MIC_MEGA_TABLE: usize = 23;
    /// A resolving arm's `JitPICSlot::install` found no way for the receiver
    /// while one of the ways it could have taken was retired and waiting for
    /// its grace (round 13 wave 11, lane mega9): the count proposal M13-4 (a
    /// poll-word grace request) is gated on. A large number here, on a run
    /// with a thread spinning in compiled code, is receivers sent to the
    /// helper by the grace alone.
    pub const MIC_GRACE_LAG_REFUSED: usize = 24;
    /// `jit_invoke_virtual_mic`'s megamorphic arm missed at a site whose PIC
    /// has NO bound selector (it never published), while the VM's shared
    /// megamorphic table already held a way for this receiver under the
    /// selector the site would bind (round 14 wave 7, lane mega): the helper
    /// resolutions a compile-time bind (shape 4 of
    /// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`,
    /// proposal M3-5) would have turned into table hits. Read against
    /// `mic_mega_table` and `mic_calls`.
    pub const MIC_UNBOUND_TABLE_HELD: usize = 25;
    /// A resolving arm of `jit_invoke_virtual_mic` bound its site's
    /// shared-table selector before any publication (round 15 wave 2, lane
    /// mega, MG7-1, `CRATONVM_JIT_MEGA_BIND_AT_RESOLUTION`): one per site
    /// bound early. Read against `mic_unbound_table_held`, which the switch
    /// should drive towards zero.
    pub const MIC_BOUND_AT_RESOLUTION: usize = 26;
    /// A `mic_mega_table` hit served by the receiver's class CELL, which the
    /// Rust reader could not see before (round 15 wave 3, lane mega2, MG15-3,
    /// `CRATONVM_JIT_MEGA_RUST_CELL_READ`): each one was a full resolution.
    pub const MIC_MEGA_CELL_READ: usize = 27;
    /// A shared-table publication that only the receiver's class cell took
    /// (the hashed set was full): a way only the class-slot probes read
    /// (round 15 wave 3, lane mega2, MG15-3's writer-side count).
    pub const MIC_CELL_ONLY_PUBLISH: usize = 28;
    /// At an `invokeinterface` site, a publication the receiver's class cell
    /// refused because a live way of another key holds that column (round 15
    /// wave 3, lane mega2: W4-1a's census, which decides MG15-5,
    /// `CRATONVM_JIT_MEGA_IFACE_SELECTOR_COLUMN`).
    pub const MIC_IFACE_CELL_TAKEN: usize = 29;
    /// A shared-table publication whose receiver got no class cell because
    /// making its array would pass the table's column budget (round 16, lane
    /// mega, MG2-2 step 1 of `jit-r15w3-mega2-proposals-RETIRED-20261003.md`): non-zero on a
    /// real application is what step 2 (arrays sized to the columns actually
    /// published) waits for.
    pub const MIC_CELL_BUDGET_REFUSED: usize = 30;
    /// A shared-table publication whose receiver's class array exists but is
    /// shorter than the site's column (round 16, lane mega): an interface
    /// column recorded from a receiver with a deeper vtable, read against
    /// `mic_iface_cell_taken` when deciding MG15-5.
    pub const MIC_CELL_PAST_ARRAY: usize = 31;
    /// `jit_invoke_dispatch`'s blind virtual arm missed its per-thread memo
    /// below the tier-up threshold while the VM's shared megamorphic table
    /// already held a way for this receiver under the site's selector (round
    /// 16 wave 8, lane mega: MG7-3's census of shape 6 of
    /// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`). In the
    /// default arm each one is a generic `invoke_virtual` the hint would have
    /// replaced by one probe per `(site, receiver class, thread)`.
    pub const DISP_BLIND_TABLE_HELD: usize = 32;
    /// That arm probed for the callee at once on such a miss
    /// (`CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT=1`): one per key the hint
    /// moved ahead of its threshold.
    pub const DISP_BLIND_TABLE_HINT: usize = 33;
    /// That arm missed its per-thread memo for a key PAST the tier-up
    /// threshold: the key's one probe was spent and answered no body (a
    /// callee whose background compile had not landed yet), so by default
    /// every later call of that `(site, receiver class)` on this thread is a
    /// generic call for the life of the thread (round 16 wave 8, lane mega:
    /// `r16-mega-blind-dispatch-spent-key-never-reprobes-20261001`).
    pub const DISP_BLIND_SPENT_MISS: usize = 34;
    /// A spent key re-probed at a doubling of the threshold
    /// (`CRATONVM_JIT_BLIND_DISPATCH_REPROBE=1`).
    pub const DISP_BLIND_REPROBE: usize = 35;
    /// The blind arm's interpreter template stood down for a COMPILED callee,
    /// and the key was set to probe on its next call (round 16 wave 17, lane
    /// mega5: `blind_dispatch_late_probe_rewind`). At most one per key per
    /// memo generation.
    pub const DISP_BLIND_LATE_PROBE: usize = 36;
    /// Tier-1 round wave 8, lane IC: a publication replaced its receiver's
    /// shared-table array by a longer copy to reach the site's column (IC21-1,
    /// `CRATONVM_JIT_MEGA_CLASS_ROW_GROW`, or forced by
    /// `CRATONVM_JIT_MEGA_IFACE_COLUMN_SPACE`): read beside
    /// `mic_cell_past_array`, which it should drive to zero.
    pub const MIC_CELL_GROWN: usize = 37;
    /// An `invokeinterface` site's first publication was handed an
    /// interface-space column (IC21-3): roughly the number of interface
    /// selectors in play (a racing first bind of one selector counts twice).
    /// The census the sparse-segment proposal asks for before IC21-3 is A/B'd.
    pub const MIC_IFACE_SPACE_ISSUED: usize = 38;
    /// An `invokevirtual` site was bound to its selector and column at compile
    /// time (IC21-2, `CRATONVM_JIT_MEGA_VTABLE_COLUMN_BAKED`): the sites whose
    /// probe can bake them.
    pub const MIC_BAKED_BIND: usize = 39;

    const N: usize = 40;
    const NAMES: [&str; N] = [
        "kind_virtual",
        "kind_special",
        "kind_interface",
        "kind_static",
        "out_object_native",
        "out_site_native",
        "out_integer_native",
        "out_virt_cache",
        "out_dcache",
        "out_jcache",
        "out_compiled_now",
        "out_tail",
        "mic_site_native",
        "mic_early_other",
        "mic_rust_cache",
        "mic_pic",
        "out_static_bc",
        "out_static_bc_refused",
        "out_virtual_bc",
        "out_virtual_bc_refused",
        "out_special_bc",
        "out_special_bc_refused",
        "mic_miss_called_compiled",
        "mic_mega_table",
        "mic_grace_lag_refused",
        "mic_unbound_table_held",
        "mic_bound_at_resolution",
        "mic_mega_cell_read",
        "mic_cell_only_publish",
        "mic_iface_cell_taken",
        "mic_cell_budget_refused",
        "mic_cell_past_array",
        "disp_blind_table_held",
        "disp_blind_table_hint",
        "disp_blind_spent_miss",
        "disp_blind_reprobe",
        "disp_blind_late_probe",
        "mic_cell_grown",
        "mic_iface_space_issued",
        "mic_baked_bind",
    ];

    static COUNTS: [AtomicU64; N] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        // Tier-1 round wave 8, lane IC: the three IC21 engagement rows.
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];

    /// Count one event. Gated on [`super::mic_prof::enabled`], like every counter here.
    #[inline]
    pub fn note(slot: usize) {
        if super::mic_prof::enabled() {
            if let Some(c) = COUNTS.get(slot) {
                c.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Count the invoke kind of one `jit_invoke_dispatch` call.
    #[inline]
    pub(crate) fn note_kind(invoke_kind: u8) {
        note(usize::from(invoke_kind).min(3));
    }

    pub fn report() {
        let mut line = String::from("[DISP_CENSUS]");
        for (i, name) in NAMES.iter().enumerate() {
            line.push_str(&format!(" {name}={}", COUNTS[i].load(Ordering::Relaxed)));
        }
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod r19_funnel2_walk_once_tests;
#[cfg(test)]
mod gcg_g6jm_door_pin_tests;
#[cfg(test)]
mod nio21w17_l5_deferred_trap_snapshot_tests;
/// Declare a per-thread dispatch memo keyed on a [`JitSiteKey`] — i.e. on a
/// `JitInvokeInfo` **address** — and generate the single flush that empties
/// every such memo.
///
/// **Declaration and flush come from one list on purpose.** The defect
/// `383e7f5cf` fixed was precisely those two drifting apart: four of the eight
/// memos below sat on neither flush trigger's hand-maintained list, so a
/// recycled `JitInvokeInfo` address let one call site serve another's
/// dispatch. Writing the list twice is what made that possible. A memo
/// declared here cannot repeat it — it does not exist unless it is also
/// flushed, and the compiler, not a reviewer, is what enforces that.
///
/// A memo whose key does NOT contain an info address (`INTEGER_WRAPPER_CLASS_CACHE`,
/// `MATCHER_CLASS_CACHE`, the `JIT_TYPECHECK_*` family) does not belong here:
/// those key on `(vm_identity, ClassId)` or on process-interned name pointers
/// that are never freed, so address reuse cannot rename them.
///
/// **Two groups, by what a first-time publication can do to an entry** (rt
/// proposal W8-1; see [`site_memo_flush`]). `retire_only` memos hold answers
/// that no publication of a NEW key can falsify — a compiled callee body (a
/// replacement of that body is a retire), a receiver's dispatch class, a
/// bytecode template — so they are dropped only when a body is withdrawn or
/// freed. `publication_sensitive` memos hold refusals, call counts, or native
/// resolutions that were made without a compiled body to prefer, so every
/// generation change still drops them, exactly as before the split. When in
/// doubt, a new memo goes in the second group: that is always safe.
macro_rules! site_keyed_memos {
    (
        retire_only {$(
            $(#[$rattr:meta])*
            $rname:ident : $rkey:ty => $rval:ty ;
        )+}
        publication_sensitive {$(
            $(#[$pattr:meta])*
            $pname:ident : $pkey:ty => $pval:ty ;
        )+}
    ) => {
        thread_local! {
            $(
                $(#[$rattr])*
                pub static $rname: std::cell::RefCell<rustc_hash::FxHashMap<$rkey, $rval>> =
                    std::cell::RefCell::new(rustc_hash::FxHashMap::default());
            )+
            $(
                $(#[$pattr])*
                pub static $pname: std::cell::RefCell<rustc_hash::FxHashMap<$pkey, $pval>> =
                    std::cell::RefCell::new(rustc_hash::FxHashMap::default());
            )+
        }

        /// Drop every thread-local memo whose key contains a `JitInvokeInfo`
        /// address. Generated from [`site_keyed_memos!`]'s own declaration
        /// list, so it cannot fall behind it.
        #[cold]
        pub fn clear_site_keyed_dispatch_memos() {
            $( $rname.with(|c| c.borrow_mut().clear()); )+
            $( $pname.with(|c| c.borrow_mut().clear()); )+
        }

        /// Drop only the `publication_sensitive` group: what a publication
        /// that retired nothing requires.
        #[cold]
        pub fn clear_publication_sensitive_site_memos() {
            $( $pname.with(|c| c.borrow_mut().clear()); )+
        }

        /// `(memo name, live entry count)` for every site-keyed memo on this
        /// thread, in declaration order. Test-only: it is what lets
        /// `a_jit_generation_change_clears_every_site_keyed_memo` assert
        /// against the real list rather than a copy of it.
        #[cfg(test)]
        pub fn site_keyed_memo_census() -> Vec<(&'static str, usize)> {
            vec![
                $( (stringify!($rname), $rname.with(|c| c.borrow().len())), )+
                $( (stringify!($pname), $pname.with(|c| c.borrow().len())), )+
            ]
        }

        /// Names of the `publication_sensitive` group. Test-only.
        #[cfg(test)]
        pub fn publication_sensitive_memo_names() -> Vec<&'static str> {
            vec![ $( stringify!($pname) ),+ ]
        }
    };
}

#[cfg(test)]
mod zt6_rv3_narrow_memo_flush_tests;
#[cfg(test)]
mod gch_h1jp_mic_miss_rerun_tests;
#[cfg(test)]
mod r13w9_replay5_unresolved_callee_tests;

#[cfg(test)]
mod gce_e1f_owned_args_handoff_tests;
#[cfg(test)]
mod j27_sdr17_callee_rerun_refusal_tests;
/// t1 wave 6 (lane J6): [`arm_not_entrant_for_dependency_invalidation`] arms
/// the cache only with the switch on, per VM.
#[cfg(test)]
mod t1w6_j6_invalidate_not_entrant_arming_tests;
/// Round 16 wave 15 (lane trapfix): the eviction's not-entrant patch and the
/// call-site service's once-only charge, on a hand-built body.
#[cfg(all(test, target_arch = "x86_64"))]
mod r16w15_trapfix_tests;
#[cfg(test)]
mod optimizing_catch_retier_tests;
#[cfg(test)]
mod r21_loops_predicate_trap_charge_tests;

#[cfg(test)]
mod nio21w7_chain_trap_charge_tests;
#[cfg(test)]
mod gch_h2fj_restash_tests;
#[cfg(test)]
mod r23w6_handshake_at_helper_entry_tests;

// ---------------------------------------------------------------------------

/// t1 wave 5 (lane V4): every VM-side inline-cache publication that can take
/// its `JdkOnly` refusal (`jit_entry_publishable` in the jit crate) opens this
/// VM's `JdkOnlySinkScope` first (`jdk_only_ic_publication_scope`), so the
/// refusal is counted in THIS VM's `--real-jdk-report` and not in the process
/// sink no VM reads. Wave 4 (lane Y2) scoped the MIC / PIC sites and missed
/// the shared megamorphic table's install, which the blind dispatch helper
/// reaches with no scope open.
#[cfg(test)]
mod t1v4_ic_publication_scope_tests;

/// JIT round 21, lane revvm2 (LAMBDATRAP21-1): the division-guard and
/// hot-catch re-tiers re-decide a repeat whose IR ban could not be recorded,
/// and only that repeat.
#[cfg(test)]
mod jr21_revvm2_site_trap_reclaim_tests;

/// JIT round 21, lane smallfix (REVVM221-1): the division-guard counter
/// re-fires past its threshold, so one thread's hot site reaches
/// `site_trap_policy_must_apply` again and re-decides while the IR ban could
/// not be recorded.
#[cfg(test)]
mod jr21_smallfix_division_guard_refire_tests;

#[cfg(test)]
mod j27_disprows_dispatch_target_tests;

#[cfg(test)]
mod j27_t1ic_mega_bake_access_tests;

/// j27 wave 7, lane T1LAMTRAP (LAMBDATRAP21-1): the one claim/ban step of the
/// three site-trap-style policies.
#[cfg(test)]
mod j27_t1lamtrap_claim_site_trap_policy_tests;



mod direct_static_compiled_callee_entry;
#[allow(unused_imports)]
pub use direct_static_compiled_callee_entry::*;
#[allow(unused_imports)]
use direct_static_compiled_callee_entry::*;
mod class_was_redefined;
#[allow(unused_imports)]
pub use class_was_redefined::*;
#[allow(unused_imports)]
use class_was_redefined::*;
mod jit_service_callee_deopt_body;
#[allow(unused_imports)]
pub use jit_service_callee_deopt_body::*;
#[allow(unused_imports)]
use jit_service_callee_deopt_body::*;
mod decode_dispatch_values_into_shaped;
#[allow(unused_imports)]
pub use decode_dispatch_values_into_shaped::*;
#[allow(unused_imports)]
use decode_dispatch_values_into_shaped::*;
