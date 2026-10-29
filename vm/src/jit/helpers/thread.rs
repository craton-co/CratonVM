// SPDX-License-Identifier: Apache-2.0
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

// These two gate the RUNTIME dispatch helper's OWN direct-entry cache
// (`jit_invoke_dispatch`'s `DISPATCH_CACHE`/`jit_cache` lookups and
// `jit_invoke_virtual_mic`'s cached `entry_ptr` fast path) — NOT the same
// thing as `cratonvm_jit::direct_jit_callee_calls_enabled` (which gates
// compile-time eager-callee-compile in `jit/src/lib.rs` and the inline
// MIC/PIC codegen in `jit/src/x64.rs`, and is default-ON: see that
// function's doc comment for the closed RBP-mirror race and the
// general-throughput regression its default avoids).
//
// `statically_bound` (this function's only caller-side gate, besides this
// flag) is `matches!(info.invoke_kind, 1 | 3)` — invokespecial/invokestatic,
// whose target is fully determined by the constant-pool entry under JVM
// semantics, with no receiver-class ambiguity. Default-ON: measured on
// `bench/BenchSuite.java` bintrees16 (self-recursive `static Node make(int)`/
// `static long check(Node)`, the classic allocation+recursion micro-
// benchmark) at 43.7s with this flag off vs. 2.8s on — every invokestatic/
// invokespecial call site was paying the full `jit_invoke_dispatch` helper
// round trip instead of a cached direct entry. invokestatic/invokespecial
// are among the most common call forms in ordinary Java (constructors,
// private/static helpers, `super` calls), so leaving this off by default is
// a severe, general JIT throughput regression, not a narrow one.
//
// Formerly known residual, now FIXED: the static CP-owner cache was not a
// sound target resolver for every invokespecial/static BRIDGE specifically
// (Lucene DataOutput's `writeByte` bridge resolved as `Object.writeByte` was
// the originally-observed case) — a synthetic-bridge target-resolution gap,
// not a receiver-ambiguity one. Root cause: `info.class_name` (and
// `try_jit_compile_callee_slow`'s `class_name` parameter more generally) is
// the literal constant-pool-referenced class for an `invokespecial` site,
// but that is NOT always the class JVMS §6.5 says method *selection* should
// start searching from — for a genuine `super.m(...)` call (ACC_SUPER set on
// the calling class, target not `<init>`, CP-referenced class a genuine
// superclass of the caller), selection restarts at the CALLING class's own
// direct superclass instead. A class between the caller and the far-off
// CP-referenced ancestor that overrides the method (a compiler-generated
// bridge, or an ordinary override) was walked straight past, landing on a
// much-less-specific declaration higher up the chain. Fixed by computing the
// JVMS-correct selection-start class at JIT-compile time (before either
// `DISPATCH_CACHE` or `jit_cache` ever see the site) via
// `classloading::invokespecial_selection_start`, applied identically by both
// the interpreter (`interpreter::invokespecial_owner_class_name`) and the
// JIT compiler (`jit::try_compile_with_invokespecial_resolver`'s
// `cp_invokespecial_owner_resolver`), so the two execution modes agree and
// neither the per-callsite `DISPATCH_CACHE` nor the global `jit_cache` can
// ever cache a target resolved from the wrong starting class.
#[inline]
fn direct_static_compiled_callee_entry_enabled() -> bool {
    static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

// The virtual-call counterpart of the flag above. Default-ON now that every
// compilation path owns MIC/PIC slots, cache publication rejects class-only
// profile seeds, both entry ABIs are lowered, and the generated caller
// republishes its active frame after a raw call. Opt out for diagnosis with
// `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`.
//
// What the default-OFF period cost, measured independently on 2026-07-27 (H2
// `org/h2/` ban residuals): this flag gates the ONLY write of
// `mic.cached_entry_word`, so with it off the inline MIC/PIC cascade the codegen
// emits can never open and every virtual call out of compiled code falls back
// through `invoke_or_native` into the INTERPRETER. Compiling a method therefore
// made its callees slower, and compiling more of a program made it slower
// overall. On H2 `TestFreeSpace` -- `org/h2/mvstore/FreeSpaceBitSet.toString`
// compiled, its `java/util/BitSet.nextClearBit` callee compiled too -- the
// `toString` scan loop cost 5582 ms per 2000 calls with this off and 169 ms
// with it on, and the off-cost grew with the callee's working set while the
// on-cost did not.
//
// Consequence for anything verified while it was off: a JIT ban whose mechanism
// is compiled-to-compiled virtual dispatch could not reproduce during a
// default-OFF run, because the dispatch it guards was inert. JASPER-JDT.2/.3
// (`org/eclipse/jdt/internal/compiler/parser/` and `ast/`) were removed on
// 2026-07-26 on exactly such runs and had to be restored on 2026-07-27, when
// turning this on brought the miscompile straight back. They were only removed
// for good on 2026-07-28, after the defect behind them was root-caused (the
// LICM / speculative pre-header bypass fixed in `613b10f4c`) and re-measured
// with this ON -- see their entry in `skip_list.rs`. Re-verify any similar
// removal with this ON.
#[inline]
fn direct_virtual_compiled_callee_entry_enabled() -> bool {
    static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY")
        {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

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
        cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MIC_PROF"))
    }

    /// `CRATONVM_DBG_MIC_TRACE` — the per-call `[DISP_TRACE]` line, separate
    /// from [`enabled`] because one `eprintln` per dispatched call is only
    /// affordable when dispatch is cold. It is not cold on a call-dense
    /// workload, where it buries the counters it shares a switch with under
    /// hundreds of megabytes of stderr.
    pub fn trace_enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MIC_TRACE"))
    }

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

// ---------------------------------------------------------------------------
// Thread-local JvmThread pointer for JIT helper access
// ---------------------------------------------------------------------------

thread_local! {
    /// DIAGNOSTIC: name of the most recently dispatched JIT callee on this
    /// thread (`class.method desc`). Set at the top of `jit_invoke_dispatch` /
    /// `jit_invoke_virtual_mic`. Read by `jit_putfield_int` when it sees a
    /// non-canonical receiver, to name the miscompiled method. Only touched on
    /// dispatch (cheap) and never in release-critical inner loops.
    static CURRENT_JIT_CALLEE: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };

    /// Stores a raw pointer to the current thread's JvmThread.
    /// Safety invariant: only ONE `&mut JvmThread` is derived from this at a time,
    /// and only within a single JIT helper call scope. The pointer is set before
    /// entering JIT code and cleared immediately after.
    static JIT_THREAD: Cell<*mut JvmThread> = const { Cell::new(std::ptr::null_mut()) };

    /// ALL out-of-band JIT→interpreter signals for this thread, consolidated
    /// in ONE thread-local. `execute_jit_call`'s post-return drain used to
    /// pay SIX separate thread-local accesses (pending exception, AIOOBE,
    /// arithmetic, NPE, NPE action, deopt flag) on EVERY JIT invocation —
    /// ~50-60ns, the single largest constant in the interpreter→JIT entry
    /// overhead on short-callee shapes. One struct = one TLS address
    /// computation for the whole drain (`take_all_jit_signals`).
    ///
    /// **This block holds NO heap references.** The one signal that did —
    /// `exception`, the pending Java throwable — now lives on
    /// [`JvmThread::jit_pending_exception`], because a `thread_local!` is
    /// unreachable from a collecting thread and the throwable was therefore
    /// neither scanned nor remapped for the whole stash→drain window (see
    /// `jit-signals-root-gap.md`). Every remaining field is a
    /// plain scalar the collector has no interest in, which is why they may
    /// stay here and keep the one-TLS-access drain. **Do not add an
    /// `ObjectRef`, a `Value`, or a raw heap address to this struct** — put it
    /// on `JvmThread` and root it in `memory/roots.rs` + `memory/gc.rs`.
    ///
    /// Field semantics (formerly the individual statics):
    /// * `athrow_bci` — see the field; the bci of the `athrow` that produced
    ///   the (thread-resident) pending exception.
    /// * `aioobe` — pending AIOOBE `(index, length)` from a JIT bounds
    ///   check (`jit_throw_aioobe`), consumed on the `i64::MIN` sentinel.
    /// * `arithmetic` — pending `ArithmeticException` ("/ by zero") from
    ///   the integer-division zero-divisor guard; drained like `aioobe`
    ///   (throwing through the exception table instead of re-running the
    ///   method, which double-executed prior side effects).
    /// * `npe` — pending NullPointerException from a JIT array helper on a
    ///   null array reference; drained like `aioobe`.
    /// * `npe_action` — JEP 358 (partial): the *operation kind* that raised
    ///   the pending JIT NPE (`helpful_npe::jit_action` code, `0` = none),
    ///   set in lockstep with `npe` so the drain can attach an action-only
    ///   message; reset by every bare `set_jit_pending_npe()` so a stale
    ///   code never leaks onto an unrelated NPE.
    /// * `deopt` — the out-of-band deopt/exception signal (`i64::MIN`
    ///   sentinel collision disambiguation): every JIT path producing the
    ///   `i64::MIN` deopt sentinel ALSO sets this, so a method legitimately
    ///   returning `Long.MIN_VALUE` is not mistaken for a deopt (which
    ///   would spuriously re-run it, double-executing side effects).
    static JIT_SIGNALS: JitSignals = const {
        JitSignals {
            athrow_bci: Cell::new(-1),
            athrow_owner: Cell::new(0),
            aioobe: Cell::new(None),
            arithmetic: Cell::new(false),
            npe: Cell::new(false),
            npe_action: Cell::new(0),
            trap_frames: std::cell::RefCell::new(None),
            deopt: Cell::new(false),
            frameless_trap: Cell::new(FramelessTrapSite::Unknown),
            redispatch_escape: Cell::new(0),
            trap_snapshot_deferred: Cell::new(DeferredTrapSnapshot::None),
            trap_snapshot_bound: Cell::new(usize::MAX),
        }
    };

    /// Debug-only reentrancy guard for [`jit_thread_mut`]. Set while a
    /// `&mut JvmThread` handed out by `jit_thread_mut` is considered live, and
    /// cleared when the [`JitThreadGuard`] returned alongside it is dropped.
    /// A nested/aliasing `jit_thread_mut` call observes the set flag and trips
    /// the `debug_assert!`. Compiled out entirely in release builds, so release
    /// behaviour is unchanged.
    #[cfg(debug_assertions)]
    static JIT_THREAD_BORROWED: Cell<bool> = const { Cell::new(false) };

    /// Debug-only: where the currently-live `jit_thread_mut` borrow was taken.
    ///
    /// The `debug_assert!` in `jit_thread_mut` says two borrows overlap; it
    /// cannot say WHICH two, and with 58 call sites in this file that is the
    /// whole difficulty. Populated only when
    /// `CRATONVM_DBG_JIT_BORROW_SITES=1`, because capturing a backtrace on
    /// every borrow costs far more than the borrow.
    ///
    /// `Cell<Option<Box<..>>>` rather than `RefCell`: the one place this is
    /// read is the aliasing trip itself, which is by definition a reentrant
    /// moment, and a `RefCell` double-borrow panic there would replace the
    /// diagnosis with a different panic.
    #[cfg(debug_assertions)]
    static JIT_THREAD_BORROW_SITE: Cell<Option<Box<std::backtrace::Backtrace>>> =
        const { Cell::new(None) };
}

/// Debug-only: is the borrow-site backtrace capture switched on?
#[cfg(debug_assertions)]
fn jit_borrow_site_capture_enabled() -> bool {
    static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_BORROW_SITES"))
}

/// Debug-only: snapshot the borrow flag and clear it, so a nested JIT entry
/// (the interpreter re-entering JIT from inside a bail) starts a fresh borrow
/// level. Returns the previous value for [`restore_jit_borrow`]. No-op in
/// release builds.
///
/// Carries the recorded borrow SITE along with the flag. Without that, the
/// nested case leaves `flag = true` restored over `site = None` (the inner
/// guard's drop cleared it), and a trip in the outer level then reports "no
/// site recorded" while the capture is switched on — an instrument that goes
/// quiet exactly where it is needed.
#[cfg(debug_assertions)]
fn suspend_jit_borrow() -> (bool, Option<Box<std::backtrace::Backtrace>>) {
    let prev = JIT_THREAD_BORROWED.with(|b| {
        let prev = b.get();
        b.set(false);
        prev
    });
    let site = JIT_THREAD_BORROW_SITE.with(|s| s.take());
    (prev, site)
}

/// Debug-only: restore the borrow flag suspended by [`suspend_jit_borrow`]
/// once the nested JIT call has returned. No-op in release builds.
#[cfg(debug_assertions)]
fn restore_jit_borrow(prev: (bool, Option<Box<std::backtrace::Backtrace>>)) {
    let (flag, site) = prev;
    JIT_THREAD_BORROWED.with(|b| b.set(flag));
    JIT_THREAD_BORROW_SITE.with(|s| s.set(site));
}

/// Debug-only RAII guard that marks the `jit_thread_mut` borrow as released
/// when dropped. In release builds this is a zero-sized no-op.
pub(crate) struct JitThreadGuard {
    #[cfg(debug_assertions)]
    _private: (),
}

impl Drop for JitThreadGuard {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROWED.with(|b| b.set(false));
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROW_SITE.with(|s| s.set(None));
    }
}

/// Opaque token returned by [`set_jit_thread`] and consumed by
/// [`restore_jit_thread`]. Bundles the previously-stored raw thread pointer
/// with the suspended debug borrow flag, so a strictly-nested JIT re-entry
/// (outer `jit_invoke_dispatch` → interpreter bail → inner
/// `jit_invoke_dispatch`) gets a clean borrow level while the outer borrow is
/// frozen on the call stack for the nested call's duration.
pub struct JitThreadScope {
    prev_ptr: *mut JvmThread,
    /// Shadow-stack `top` watermark captured at JIT entry (follow-up §3). On exit
    /// [`restore_jit_thread`] resets `top` to this value, healing any push an
    /// abnormal JIT exit (exception/deopt that skipped a method epilogue) left
    /// unbalanced. `None` when the shadow-stack gate is off (no-op). On a normal
    /// exit the per-method epilogues already restored `top`, so the reset is a
    /// no-op there.
    saved_shadow_top: Option<usize>,
    #[cfg(debug_assertions)]
    prev_borrow: (bool, Option<Box<std::backtrace::Backtrace>>),
}

/// DIAGNOSTIC: read the current dispatched JIT callee name.
fn current_jit_callee() -> String {
    CURRENT_JIT_CALLEE.with(|c| c.borrow().clone())
}

/// Return a replacement JIT argument buffer when a safepoint has forwarded an
/// object argument.  The JIT ABI carries raw addresses, so canonicalizing only
/// the receiver is insufficient for signatures such as `([BII)V`: a moved
/// byte array can otherwise be decoded from its recycled pre-GC address.
///
/// The common case allocates nothing.  `num_jit_args` counts compact native
/// values (including an instance receiver), so each parsed descriptor parameter
/// advances one buffer position even for category-2 Java values.
fn forward_jit_reference_args(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<Vec<i64>> {
    let mut replacement: Option<Vec<i64>> = None;
    for_each_jit_reference_arg(info, args, |index, raw| {
        forward_jit_arg_at(vm, args, &mut replacement, index, raw);
    });
    replacement
}

/// [`forward_jit_reference_args`] that also answers WHICH argument words it
/// validated as live heap bases: bit `i` of the mask is set when buffer index
/// `i` (`i < 64`) of the buffer the caller goes on to use -- the replacement
/// when there is one, else `args` -- names an object `load_and_forward_checked`
/// accepted (the same structural predicate as `is_object_address`).
///
/// JIT round 19 wave 2, lane funnel2 (FUNNEL17-2,
/// `CRATONVM_JIT_NATIVE_WALK_ONCE`): the native-site door then builds those
/// references from the word instead of walking the heap a second time. The
/// mask is only good until the next GC point; both dispatch helpers run none
/// between this pass and the door.
fn forward_jit_reference_args_validated(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> (Option<Vec<i64>>, u64) {
    let mut replacement: Option<Vec<i64>> = None;
    let mut mask = 0u64;
    for_each_jit_reference_arg(info, args, |index, raw| {
        if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
            return;
        }
        // SAFETY: as `forward_jit_arg_at`: a descriptor-declared reference
        // word that passed the canonicality checks above.
        let object = unsafe { ObjectRef::from_raw(raw as usize as *mut u8) };
        let (forwarded, valid) = vm.mem.heap.load_and_forward_checked(object);
        let forwarded = forwarded.as_ptr() as i64;
        if forwarded != raw {
            let buf = replacement.get_or_insert_with(|| args.to_vec());
            buf[index] = forwarded;
        }
        if valid && index < 64 {
            mask |= 1u64 << index;
        }
    });
    (replacement, mask)
}

/// A reference word the forwarding pass already validated
/// ([`forward_jit_reference_args_validated`]'s mask), as an `ObjectRef`
/// without a second heap walk; `None` when the bit is clear.
#[inline]
fn prevalidated_jit_ref(prevalidated: u64, index: usize, raw: i64) -> Option<ObjectRef> {
    if index < 64 && prevalidated & (1u64 << index) != 0 && raw != 0 {
        // SAFETY: the forwarding pass validated this exact word as a live
        // object base, and no GC point ran since (the mask's contract).
        Some(unsafe { ObjectRef::from_raw(raw as usize as *mut u8) })
    } else {
        None
    }
}

#[cfg(test)]
mod r19_funnel2_walk_once_tests {
    use super::prevalidated_jit_ref;

    /// Only a set bit for that exact index stands for a walk; a clear bit, an
    /// index past the mask and a null word all fall back to walking.
    #[test]
    fn the_mask_answers_only_its_own_indices() {
        let word = 0x1000_0000_i64;
        assert!(prevalidated_jit_ref(0b10, 1, word).is_some());
        assert!(prevalidated_jit_ref(0b10, 0, word).is_none());
        assert!(prevalidated_jit_ref(0, 1, word).is_none());
        assert!(prevalidated_jit_ref(u64::MAX, 64, word).is_none());
        assert!(prevalidated_jit_ref(u64::MAX, 3, 0).is_none());
    }
}

/// Visit every reference position of a compact JIT argument buffer: the
/// receiver (when [`jit_info_has_receiver`] says there is one) and each `L`/`[` parameter of
/// `info.descriptor`. `visit(index, raw)` gets the buffer index and its word.
///
/// The one descriptor walk shared by [`forward_jit_reference_args`] and
/// [`CompileArgPins::pin_raw`], so the two can never disagree about which
/// words are references.
fn for_each_jit_reference_arg(
    info: &JitInvokeInfo,
    args: &[i64],
    mut visit: impl FnMut(usize, i64),
) {
    // JIT round 23 wave 3d (lane L1): the receiver rule is
    // `jit_info_has_receiver`, the same one the argument decoder asks. This
    // read `invoke_kind != 3`, which named argument 0 of a STATIC direct
    // self-call row (kind 4) a receiver and shifted every descriptor
    // parameter one buffer word to the right.
    let has_receiver = jit_info_has_receiver(info);
    let mut arg_index = usize::from(has_receiver);

    // Instance receivers are object references regardless of their descriptor.
    if has_receiver {
        if let Some(&raw) = args.first() {
            visit(0, raw);
        }
    }

    let bytes = info.descriptor.as_bytes();
    let mut p = match bytes.iter().position(|&b| b == b'(') {
        Some(i) => i + 1,
        None => return,
    };
    while p < bytes.len() && bytes[p] != b')' && arg_index < args.len() {
        let is_ref = matches!(bytes[p], b'L' | b'[');
        if is_ref {
            visit(arg_index, args[arg_index]);
        }
        match bytes[p] {
            b'L' => {
                while p < bytes.len() && bytes[p] != b';' {
                    p += 1;
                }
                p = p.saturating_add(1);
            }
            b'[' => {
                while p < bytes.len() && bytes[p] == b'[' {
                    p += 1;
                }
                if p < bytes.len() && bytes[p] == b'L' {
                    while p < bytes.len() && bytes[p] != b';' {
                        p += 1;
                    }
                    p = p.saturating_add(1);
                } else {
                    p = p.saturating_add(1);
                }
            }
            _ => p += 1,
        }
        arg_index += 1;
    }
}

/// A JIT argument word that names a live heap object, validated. Null,
/// misaligned and non-canonical words (and anything the heap does not
/// recognise as an object base) answer `None`.
#[inline]
fn validated_jit_ref(vm: &SharedVm, raw: i64) -> Option<ObjectRef> {
    let bits = raw as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    vm.mem.heap.is_object_address(bits as usize)
}

/// Reference arguments rooted in the current thread's `native_pin_roots`
/// across a synchronous callee compile (r11-irlower-outgoing-arg-staging-refs-unpublished).
///
/// The dispatch helpers read their arguments out of the compiled caller's
/// outgoing staging block. That block is in no oop map, and neither are the
/// helper's own copies (`forward_jit_reference_args`'s `Vec`, a decoded
/// `Value` vector). A callee compile can resolve fields and load classes,
/// i.e. run `<clinit>` and collect, so a moving collection inside it leaves
/// every one of those copies naming the pre-move address — which the helper
/// then hands to the freshly compiled callee. Re-forwarding afterwards is not a
/// repair: `load_and_forward` needs a forwarding word at the old address, and
/// ZGC's slide leaves none. Pinning is: `native_pin_roots` is scanned and
/// remapped with the thread, so reading the pins back yields the current
/// addresses.
///
/// Push and release are watermark-based (`base`), the same discipline every
/// other `native_pin_roots` user follows, so nested pins inside the compile
/// compose.
///
/// # Released on every exit (gen r5w4/jit8)
///
/// The explicit releases below read the pins back and truncate. Until wave 4
/// they were the ONLY release: a scope left any other way -- a `compile()` or
/// `call()` that unwound into the helper's `contain`, or the "cannot happen"
/// arm of [`compile_with_ref_args_pinned`] / [`call_compiled_then_route`]
/// whose `jit_thread_mut()` came back `None` -- left the arguments rooted
/// "until the next watermark release below them". On the outermost thread
/// scope there is no such release: every later pin scope takes its watermark
/// ABOVE the orphans, so they stayed roots for the life of the thread, and a
/// dropped structure an argument reached stayed reachable
/// (`docs/feature-designs/deopt-osr.md`,
/// candidate 1). `Drop` now truncates to the watermark whenever an explicit
/// release has not run.
///
/// Liveness argument (why the implicit release drops nothing the program can
/// still reach): the pins exist only so the helper can READ its arguments back
/// after a GC point inside the scope. When the scope ends without an explicit
/// release, the helper has stopped reading them -- it is unwinding, or it took
/// the arm that already gave up reading them back -- and every value in
/// `native_pin_roots[base..]` was pushed by this scope or by a scope nested in
/// it, all of which have ended. A reference the program still holds is held
/// by its own frame (the compiled caller's map / shadow push, the interpreter
/// frame, the callee's parameter home), exactly as on the normal path, where
/// the explicit release truncates the same range.
///
/// The truncation goes through the thread pointer captured at pin time (the
/// thread whose `native_pin_roots` was pushed), not through
/// [`jit_thread_mut`]: `Drop` can run while the helper's own `&mut JvmThread`
/// borrow is still on the stack, and a second `jit_thread_mut` there would
/// trip its aliasing assertion in a debug build. The write happens only on
/// the exits that skipped the explicit release, never beside a live use of
/// that borrow.
struct CompileArgPins {
    /// `native_pin_roots.len()` before the first pin: the release watermark.
    base: usize,
    /// The argument index of each pin, in push order.
    indices: smallvec::SmallVec<[usize; INLINE_JIT_NATIVE_ARGS]>,
    /// The thread the pins were pushed onto, until an explicit release has
    /// truncated them; null afterwards. Read only by `Drop`.
    owner: *mut JvmThread,
}

impl CompileArgPins {
    /// Pin every validated reference word of a compact JIT argument buffer.
    fn pin_raw(vm: &SharedVm, thread: &mut JvmThread, info: &JitInvokeInfo, args: &[i64]) -> Self {
        let base = thread.native_pin_roots.len();
        let mut indices: smallvec::SmallVec<[usize; INLINE_JIT_NATIVE_ARGS]> =
            smallvec::SmallVec::new();
        for_each_jit_reference_arg(info, args, |index, raw| {
            if let Some(obj) = validated_jit_ref(vm, raw) {
                thread.native_pin_roots.push(obj);
                indices.push(index);
            }
        });
        Self {
            base,
            indices,
            owner: thread as *mut JvmThread,
        }
    }

    /// Pin every validated object in a decoded argument vector.
    fn pin_values(vm: &SharedVm, thread: &mut JvmThread, values: &[Value]) -> Self {
        let base = thread.native_pin_roots.len();
        let mut indices: smallvec::SmallVec<[usize; INLINE_JIT_NATIVE_ARGS]> =
            smallvec::SmallVec::new();
        for (index, value) in values.iter().enumerate() {
            if let Value::Object(Some(obj)) = value {
                if let Some(obj) = vm.mem.heap.is_object_address(obj.as_ptr() as usize) {
                    thread.native_pin_roots.push(obj);
                    indices.push(index);
                }
            }
        }
        Self {
            base,
            indices,
            owner: thread as *mut JvmThread,
        }
    }

    /// Truncate `thread`'s pins to the watermark and disarm `Drop`: the one
    /// place an explicit release ends the scope.
    fn truncate_and_disarm(&mut self, thread: &mut JvmThread) {
        thread.native_pin_roots.truncate(self.base);
        self.owner = std::ptr::null_mut();
    }

    /// The argument buffer as it stands NOW, pins kept: `Some(copy)` with the
    /// moved words replaced when a collection moved any pinned object since
    /// [`Self::pin_raw`], `None` when every word is still current.
    fn current_raw(&self, thread: &JvmThread, args: &[i64]) -> Option<Vec<i64>> {
        let mut moved: Option<Vec<i64>> = None;
        for (k, &index) in self.indices.iter().enumerate() {
            let (Some(pinned), Some(&old)) =
                (thread.native_pin_roots.get(self.base + k), args.get(index))
            else {
                continue;
            };
            let now = pinned.as_ptr() as usize as i64;
            if now != old {
                if let Some(slot) = moved.get_or_insert_with(|| args.to_vec()).get_mut(index) {
                    *slot = now;
                }
            }
        }
        moved
    }

    /// Release the pins, answering the argument buffer as it stands NOW
    /// (see [`Self::current_raw`]).
    fn release_raw(mut self, thread: &mut JvmThread, args: &[i64]) -> Option<Vec<i64>> {
        let moved = self.current_raw(thread, args);
        self.truncate_and_disarm(thread);
        moved
    }

    /// Release the pins without reading them back.
    fn release(mut self, thread: &mut JvmThread) {
        self.truncate_and_disarm(thread);
    }

    /// Release the pins, writing each pinned object's current address back
    /// into `values`.
    fn release_values(mut self, thread: &mut JvmThread, values: &mut [Value]) {
        for (k, &index) in self.indices.iter().enumerate() {
            if let (Some(&pinned), Some(slot)) = (
                thread.native_pin_roots.get(self.base + k),
                values.get_mut(index),
            ) {
                *slot = Value::Object(Some(pinned));
            }
        }
        self.truncate_and_disarm(thread);
    }
}

impl Drop for CompileArgPins {
    /// The implicit release (gen r5w4/jit8): see "Released on every exit" on
    /// the type. A no-op after any explicit release.
    fn drop(&mut self) {
        if self.owner.is_null() {
            return;
        }
        // SAFETY: `owner` is the `JvmThread` this scope pushed onto, captured
        // from a live `&mut` at pin time. A `CompileArgPins` never outlives
        // the helper call that created it (every one is a local of a JIT
        // helper or of a function it calls, never stored), and the thread
        // outlives every helper call made on it. Reaching here means no
        // explicit release ran, i.e. the scope is being left by an unwind or
        // by an arm that stopped reading the pins; the helper's own borrow of
        // the thread is not in use at this instant (it is the frame being
        // dropped, or a caller frame suspended in the call that is dropping
        // this one), so this is the only access.
        let thread = unsafe { &mut *self.owner };
        if thread.native_pin_roots.len() > self.base {
            thread.native_pin_roots.truncate(self.base);
        }
        self.owner = std::ptr::null_mut();
    }
}

// gcg g6/jm (2026-10-02) -- `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`,
// row 3. The whole census of `Gcd1ArgPinSpecialProbe more settle` (vg3, g3a)
// names `native_pin_roots` -- a dispatch door's `CompileArgPins` -- as the only
// holder of a 16 MiB argument the callee had already dropped, with the caller
// compiled. The doors pin a call's arguments so that a sentinel's service can
// RE-RUN the callee with current words (its handler, or from entry after a
// declined stash). The hit and PIC arms pin only when the site once published
// a callee with an exception table (`JitMICSlot::callee_rerun_pins`, sticky),
// and the megamorphic table arm pinned ALWAYS. A body that owns its arguments
// (`cratonvm_jit::callee_body_owns_its_arguments`: no handler, no frameless
// trap stub, no indy trap, not synchronized) cannot take the handler arm and
// has no frameless trap; what is left is a re-run after a DECLINED stash -- the
// same exposure every hit / PIC arm already accepts for a handler-free callee,
// where it pins nothing. So for such a body these doors now pin nothing either,
// and the argument dies with the callee's own use of it, as in HotSpot.

/// `CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED` -- opt-in (gcg g6/jm; vg6 jm6_argsp
/// on = off, so not the probe's door); off keeps the unconditional
/// megamorphic-arm pin and the site-flag pins of the hit / PIC arms. Read ONCE per VM, when its `JitCache` is built
/// (`JitCache::door_owned_args_unpinned`): this is asked on every door call.
#[inline]
fn door_owned_args_unpinned_enabled(vm: &SharedVm) -> bool {
    vm.jit.jit_cache.door_owned_args_unpinned()
}

/// Must a Rust dispatch door pin this call's arguments across the callee
/// (see the section above)? `wanted` is the door's own reason (the site flag,
/// or `true` for the megamorphic arm); `callee_owns` says whether the body it
/// is about to enter owns its arguments; `unpinned_on` is
/// [`door_owned_args_unpinned_enabled`].
#[inline]
fn door_pins_call_args(wanted: bool, callee_owns: bool, unpinned_on: bool) -> bool {
    wanted && !(callee_owns && unpinned_on)
}

#[cfg(test)]
mod gcg_g6jm_door_pin_tests {
    use super::door_pins_call_args;

    /// gcg g6/jm: a door pins only for its own reason, and never for a body
    /// that owns its arguments unless the switch is `0`; the switch is read
    /// once per VM, when its `JitCache` is built.
    #[test]
    fn gcg_g6jm_a_door_does_not_pin_an_owning_callees_arguments() {
        assert!(
            door_pins_call_args(true, false, true),
            "a body that may re-run"
        );
        assert!(
            !door_pins_call_args(true, true, true),
            "an owning body: no pin"
        );
        assert!(
            !door_pins_call_args(false, false, true),
            "no reason, no pin"
        );
        assert!(!door_pins_call_args(false, true, true));
        assert!(door_pins_call_args(true, true, false), "=0: the old pin");
        assert!(!door_pins_call_args(false, true, false));
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED", Some("1"))],
            cratonvm_jit::JitCache::new,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED", None)],
            cratonvm_jit::JitCache::new,
        );
        assert!(on.door_owned_args_unpinned(), "=1 at construction");
        assert!(!off.door_owned_args_unpinned(), "default OFF");
    }

    /// gch h1/fr: the by-name tail's switch is read once per VM, default OFF.
    #[test]
    fn gch_h1fr_the_by_name_tail_switch_is_per_vm_and_default_off() {
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS", Some("1"))],
            cratonvm_jit::JitCache::new,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS", None)],
            cratonvm_jit::JitCache::new,
        );
        assert!(on.by_name_tail_unpins_args(), "=1 at construction");
        assert!(!off.by_name_tail_unpins_args(), "default OFF");
    }
}

/// Run a synchronous callee compile with the call's reference arguments
/// pinned (see [`CompileArgPins`]), and answer the compile's result together
/// with the argument buffer as it stands afterwards (`None`: unchanged).
///
/// For callers that hold NO `jit_thread_mut` borrow: the thread is borrowed
/// once to pin and once to release, never across `compile`.
///
/// # Safety
/// As [`jit_thread_mut`]: no other borrow of the current JIT thread may be
/// live at the call.
unsafe fn compile_with_ref_args_pinned<R>(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
    compile: impl FnOnce() -> R,
) -> (R, Option<Vec<i64>>) {
    let pins =
        jit_thread_mut().map(|(thread, _guard)| CompileArgPins::pin_raw(vm, thread, info, args));
    let result = compile();
    let moved = match pins {
        Some(pins) => match jit_thread_mut() {
            Some((thread, _guard)) => pins.release_raw(thread, args),
            // The thread pointer is restored by every nested entry, so this
            // cannot happen; if it did, `pins` is dropped here and its `Drop`
            // truncates to the watermark (gen r5w4/jit8 -- it used to stay
            // until the next watermark release below it).
            None => None,
        },
        None => None,
    };
    (result, moved)
}

/// Call a compiled callee through `call` and finish the call through
/// [`route_implicit_exc_through_callee`], with the call's reference arguments
/// rooted across BOTH when `root_args`
/// (r11w4-rt-callee-rerun-reads-pre-call-argument-words). `None` when `call`
/// made no call (more arguments than the register tables cover).
///
/// A callee that returns the `i64::MIN` sentinel can be RE-RUN with this
/// call's arguments: from entry in the interpreter, or at its own handler.
/// `args` was read BEFORE the callee ran, and a compiled callee allocates and
/// reaches safepoints. While it runs, its own frame roots its parameters; once
/// it has returned, nothing roots `args`, so a moving collection leaves a word
/// that reads back as `null` (nothing there now) or as ANOTHER object
/// (something was allocated there). The re-run then throws an NPE the program
/// never had, or runs on the wrong object. `load_and_forward` cannot repair it
/// after the fact: ZGC's slide leaves no forwarding word. The window does not
/// close when the callee returns: the routing itself allocates (the implicit
/// throwable) and can run a deopt-frame rebuild before it reads the arguments,
/// so the pins stay until the routing is done and it reads the words back
/// through them ([`args_now`]).
///
/// Only a callee that declares an exception table can take a re-run, so each
/// caller passes that fact (memoized in its [`DispatchCache`] entry, or asked
/// once on a cold fill) and every other call pays nothing.
///
/// # Safety
/// As [`compile_with_ref_args_pinned`]: no `jit_thread_mut` borrow may be live
/// at the call. `receiver_class_id` as [`route_implicit_exc_through_callee`].
#[inline]
unsafe fn call_compiled_then_route(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
    receiver_class_id: ClassId,
    root_args: bool,
    call: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    let pins = if root_args {
        jit_thread_mut().map(|(thread, _guard)| CompileArgPins::pin_raw(vm, thread, info, args))
    } else {
        None
    };
    let routed = call().map(|rc| {
        route_implicit_exc_through_callee(vm, info, args, receiver_class_id, rc, pins.as_ref())
    });
    if let Some(pins) = pins {
        // As `compile_with_ref_args_pinned`: every nested entry restores the
        // thread pointer, so this finds it; if it did not, `pins` is dropped
        // at the end of this block and its `Drop` truncates to the watermark.
        if let Some((thread, _guard)) = jit_thread_mut() {
            pins.release(thread);
        }
    }
    routed
}

/// `args` as it stands now under `pins` ([`CompileArgPins::current_raw`]);
/// `args` itself when nothing is pinned or nothing moved.
fn args_now<'a>(
    pins: Option<&CompileArgPins>,
    thread: &JvmThread,
    args: &'a [i64],
) -> std::borrow::Cow<'a, [i64]> {
    match pins.and_then(|pins| pins.current_raw(thread, args)) {
        Some(moved) => std::borrow::Cow::Owned(moved),
        None => std::borrow::Cow::Borrowed(args),
    }
}

/// Repair one JIT argument if the collector moved the object it names.
///
/// # The seventh plausibility filter in this file, and why it looks like none
///
/// The `raw >= 1 << 48` test below is 2.5.5 of
/// `docs/feature-designs/zgc-jit-load-barrier.md`: a hand-rolled plausibility
/// check that does not call `plausible_heap_pointer`, so it does not appear in
/// any grep for the idiom and was missed by that document's first draft. It
/// degrades to "skip forwarding" rather than to null, which is why it is not on
/// the silent-null list -- but a ZGC colored word has bit 63 set and would trip
/// it, so the argument would silently NOT be forwarded, with no diagnostic.
///
/// It is deliberately left alone. JIT arguments arrive already barriered: they
/// came from a caller's barriered load or from a fresh allocation, which
/// carries the allocating color by construction. That is an ASSUMPTION rather
/// than a proof, and it is the same class of assumption as J2, so it is
/// recorded here instead of being rediscovered.
///
/// # Why the ZGC load barrier is NOT plumbed through here
///
/// 2.5.5 suggests this as the natural home for the dispatch, because
/// `vm.mem.heap.load_and_forward` below is already this file's
/// backend-dispatching chokepoint for moving collectors. That reading does not
/// survive the signature: `load_and_forward` takes an `ObjectRef` -- a machine
/// pointer the caller has already built -- and therefore cannot accept a
/// colored word at all. It repairs a plain address the compacting slide moved
/// (its `Zgc` arm is a real `forwarded_after_slide` lookup, not a no-op); it
/// does not decode a color. The colored-word chokepoint has to take the SLOT,
/// which is what [`jit_load_ref_slot`] and `.agent-requests/A9-gc-barrier.txt`
/// are about.
#[inline]
fn forward_jit_arg_at(
    vm: &SharedVm,
    original: &[i64],
    replacement: &mut Option<Vec<i64>>,
    index: usize,
    raw: i64,
) {
    if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
        return;
    }
    // SAFETY: the JIT calling convention guarantees that descriptor-declared
    // references are live object pointers at this boundary; the canonicality
    // checks above reject immediate/tagged values before constructing ObjectRef.
    let object = unsafe { ObjectRef::from_raw(raw as usize as *mut u8) };
    let forwarded = vm.mem.heap.load_and_forward(object).as_ptr() as i64;
    if forwarded != raw {
        let args = replacement.get_or_insert_with(|| original.to_vec());
        args[index] = forwarded;
    }
}

/// DIAGNOSTIC: crash-handler-safe read of the current JIT callee name.
/// Uses `try_with`/`try_borrow` so it never panics if the TLS is being
/// destroyed or the `RefCell` is already borrowed when a fault lands here.
/// Returns an empty string if unavailable. Only meaningful when
/// `CRATONVM_DBG_JIT_PUTFIELD=1` (that gate is what populates the cell).
pub fn current_jit_callee_for_crash() -> String {
    CURRENT_JIT_CALLEE
        .try_with(|c| c.try_borrow().map(|s| s.clone()).unwrap_or_default())
        .unwrap_or_default()
}

/// DIAGNOSTIC RAII guard: records `info` as the current callee, and on drop
/// restores the PREVIOUS value. This makes `current_jit_callee()` name the
/// method whose body is *currently executing inline* (the one containing a
/// faulting putfield), rather than a sub-call it dispatched and returned from.
struct JitCalleeGuard(String);
impl JitCalleeGuard {
    fn new(info: &JitInvokeInfo) -> Self {
        let prev = CURRENT_JIT_CALLEE.with(|c| {
            let mut s = c.borrow_mut();
            let prev = s.clone();
            s.clear();
            s.push_str(info.class_name);
            s.push('.');
            s.push_str(info.method_name);
            s.push_str(info.descriptor);
            prev
        });
        JitCalleeGuard(prev)
    }
}
impl Drop for JitCalleeGuard {
    fn drop(&mut self) {
        CURRENT_JIT_CALLEE.with(|c| {
            let mut s = c.borrow_mut();
            s.clear();
            s.push_str(&self.0);
        });
    }
}

/// Set the current thread's JvmThread pointer for JIT helper access.
/// Returns a [`JitThreadScope`] capturing the previously-stored pointer (and,
/// in debug builds, the suspended borrow level) so callers can restore both
/// later via [`restore_jit_thread`]. Suspending the borrow level here is what
/// makes a re-entrant JIT call (interpreter::execute inside
/// jit_invoke_dispatch) a *nested child reborrow* rather than a false-positive
/// *aliasing sibling* under the `jit_thread_mut` debug check.
///
/// # Safety contract
/// The caller must ensure that no other `&mut JvmThread` reference exists for the
/// duration of JIT execution. The pointer is only dereferenced inside JIT helpers
/// which execute on the same thread that set it.
pub fn set_jit_thread(thread: &mut JvmThread) -> JitThreadScope {
    // Shadow-stack precise roots (CRATONVM_SHADOW_STACK): ensure this thread's
    // shadow stack is allocated before any JIT code that may push to it runs.
    // Cheap `base != 0` check after the first entry; gated, no-op otherwise.
    // Done here (with a legitimate `&mut JvmThread`) rather than in the extern-C
    // `jit_get_current_thread` getter to avoid deriving an aliasing `&mut`.
    let mut saved_shadow_top: Option<usize> = None;
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.ensure_allocated();
        // §3 (unwind safety): snapshot the boundary `top` watermark so
        // `restore_jit_thread` can reset it on exit, healing any push that an
        // abnormal JIT exit (exception/deopt skipping a method epilogue) left
        // unbalanced. Captured after `ensure_allocated` so `top` is valid.
        saved_shadow_top = Some(thread.shadow_stack.top);
        // Publish this thread's shadow-stack ADDRESS, plus the `base`/`end` of
        // its backing buffer, so a GC initiator that freezes this thread can
        // scan the window it cannot otherwise reach -- and can verify the
        // address still describes THIS buffer before trusting `top`.
        // Once per thread. See
        // `cratonvm_gc::gc_quiescence::publish_self_shadow_addr`.
        if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
            crate::jit::conservative_roots::publish_self_shadow_addr_once(
                &thread.shadow_stack as *const _ as usize,
                thread.shadow_stack.base,
                thread.shadow_stack.end,
            );
        }
        if if crate::runtime::env_cache::hot_lookup_cache() {
            crate::runtime::env_cache::dbg_shadow()
        } else {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SHADOW")
        } {
            use std::sync::atomic::{AtomicBool, Ordering};
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[SHADOW] set_jit_thread: thread={:p} shadow base={:#x} top={:#x} end={:#x} ss_off={}",
                    thread as *mut JvmThread,
                    thread.shadow_stack.base,
                    thread.shadow_stack.top,
                    thread.shadow_stack.end,
                    JvmThread::shadow_stack_offset(),
                );
            }
        }
    }
    // Round 13 wave 4 (lane mega3, M13-3): the floor compiled inline-cache
    // stack checks read through the mirror published below. Rewritten on every
    // publish because a thread's `JvmThread` is not tied to one OS stack.
    //
    // NEVER with the suspended value `0`. `raise_jit_stack_overflow` builds its
    // error under `SuspendedStackFloor`, which stores `0` in the TLS cell, and
    // the constructor's Java re-enters compiled code through this function.
    // Copying that `0` into the word disarmed every compiled inline-cache check
    // of the thread for good: nothing rewrites the word when the suspension
    // ends, so the next unbounded recursion ran the 128 MiB `main-vm` stack
    // into its guard page (orchestrator, build w4c: `R13CrashIcRecursionSoe`,
    // `R13MegaIcRecursionMixed`, EXCEPTION_STACK_OVERFLOW). Keeping the armed
    // value is sound during a suspension too: a check that trips on it goes to
    // its exact side, which reads the TLS cell (`jit_native_stack_floor` in
    // single-pass, `jit_self_call_stack_guard` in the IR tier), sees the
    // suspension and lets the call proceed.
    let floor = current_thread_self_call_floor(SelfCallBudget::of_thread(thread));
    if floor != 0 {
        thread.jit_stack_floor = floor;
    }
    let prev_ptr = JIT_THREAD.with(|t| {
        let old = t.get();
        t.set(thread as *mut JvmThread);
        old
    });
    cratonvm_jit::x64::publish_jit_thread_mirror(thread as *mut JvmThread as usize);
    // Suspend any borrow held by an outer JIT level: the nested JIT call about
    // to run is a child reborrow of `thread`, not an aliasing sibling, so it
    // must start its own borrow level. The outer borrow is frozen on the call
    // stack and is provably unused until this nested call returns and
    // `restore_jit_thread` un-suspends it.
    #[cfg(debug_assertions)]
    let prev_borrow = suspend_jit_borrow();
    JitThreadScope {
        prev_ptr,
        saved_shadow_top,
        #[cfg(debug_assertions)]
        prev_borrow,
    }
}

/// Check if the JIT thread pointer is already set.
#[inline(always)]
pub fn is_jit_thread_set() -> bool {
    JIT_THREAD.with(|t| !t.get().is_null())
}

/// The `*mut JvmThread` installed for this OS thread, or null when none is.
///
/// Unlike [`jit_get_current_thread`] this does **no** JIT-boundary bookkeeping
/// (`note_jit_boundary`), so GC-side verification code can consult it without
/// perturbing the per-thread scan cache it is in the middle of using.
#[inline(always)]
pub fn current_jit_thread_ptr() -> *mut JvmThread {
    JIT_THREAD.with(|t| t.get())
}

/// Restore a previously saved JIT thread scope. Used to support re-entrant
/// JIT calls (e.g. JIT put() → jit_invoke_dispatch → interpreter::execute hash()
/// which may JIT-compile hash() and call set_jit_thread again). Re-installs the
/// prior thread pointer and un-suspends the outer level's debug borrow flag.
pub fn restore_jit_thread(scope: JitThreadScope) {
    // §3 (unwind safety): reset the shadow `top` to the watermark captured at the
    // matching `set_jit_thread`, healing any push that an abnormal JIT exit
    // (exception/deopt that skipped a method epilogue) left unbalanced. On a
    // normal return the per-method epilogues already restored `top`, so this is a
    // no-op (`set_top` is idempotent at the boundary value). `JIT_THREAD` still
    // names the thread whose JIT call just returned — it is reset to `prev_ptr`
    // just below — so it identifies the correct shadow stack to heal.
    if let Some(saved_top) = scope.saved_shadow_top {
        let cur = JIT_THREAD.with(|t| t.get());
        if !cur.is_null() {
            // SAFETY: execution is back on `cur`'s own thread and its JIT call has
            // returned, so no aliasing `&mut` to its shadow stack is live; the
            // backing buffer address is stable for the thread's lifetime, and
            // `set_top` clamps into `[base, end]` so a stale value can't widen the
            // scan range.
            unsafe { (*cur).shadow_stack.set_top(saved_top) };
        }
        report_shadow_overflow_once();
    }
    JIT_THREAD.with(|t| t.set(scope.prev_ptr));
    cratonvm_jit::x64::publish_jit_thread_mirror(scope.prev_ptr as usize);
    #[cfg(debug_assertions)]
    restore_jit_borrow(scope.prev_borrow);
}

/// One-shot warning when a JIT-emitted shadow-stack push has hit the `end`
/// guard and bailed. Checked at the JIT→Rust boundary (which is where the
/// watermark heal already runs) because the bail itself cannot call out — its
/// site has the callee's arguments staged in the ABI registers.
///
/// A bail is not a crash, but it is not free either: the safepoints that bailed
/// published no oops, so their `moving_young_coverage_complete` claim is not
/// backed at runtime while their calls are in flight, and every collection
/// that a bail may still overlap falls back to the non-moving sweep
/// (`conservative_roots::shadow_bail_refuses_moving`, gcd d2/f, per thread
/// since gcd d3/o; before d2/f nothing implemented that fallback). The printed
/// line below says "degraded" rather than "until X": the per-thread check
/// decides cycle by cycle. It always means some
/// compiled method leaks pushes, or a legitimately deep compiled recursion.
/// `CRATONVM_DBG_LOCAL_HANDLER_LOOKUP`, read once per process (nio21 wave 17:
/// it was a declared-flag read on every compiled local-handler lookup).
fn local_handler_lookup_dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LOCAL_HANDLER_LOOKUP"))
}

fn report_shadow_overflow_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.load(Ordering::Relaxed) {
        return;
    }
    // nio21 wave 10 lane L5: HotSpot's stderr holds no such line, so it prints
    // under `CRATONVM_DBG=exit-census` or the diag switch it names
    // (`CRATONVM_SHADOW_OVERFLOW_DIAG`) only. The degradation it reports
    // (non-moving sweep) is unchanged either way.
    //
    // nio21 wave 17: the overflow counter (one relaxed atomic load) is asked
    // FIRST. This runs at every JIT->Rust boundary, and the two declared-flag
    // reads below each cost a UTF-8 decode plus two name hashes: the w16 host
    // `CRATONVM_DBG_FLAGREADS` census counted ~400 000 of each on the 200 000-op
    // `rlock` row. With no overflow they are now never read.
    let Some((count, label)) = cratonvm_jit::shadow_overflow_status() else {
        return;
    };
    if !(cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_EXIT_CENSUS")
        || cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_OVERFLOW_DIAG").is_some())
    {
        return;
    }
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "[JIT] shadow-stack overflow: {} push(es) bailed on the `end` guard{} \
         — a compiled method is leaking shadow pushes; GC precision has \
         degraded to the non-moving sweep. Set CRATONVM_SHADOW_OVERFLOW_DIAG=1 \
         to name the method.",
        count,
        match label.as_deref() {
            Some(m) => format!(" (last: {})", m),
            None => String::new(),
        }
    );
}

/// The loop-poll-word mirror a compiled-entry door publishes for its VM
/// (interpreter round i6 wave 119, lane L2; `CRATONVM_JIT_POLL_THREAD_WORD`,
/// default off; stage 3 of
/// `i24-L3-proposal-one-per-thread-poll-word-for-safepoints-and-frame-moves`).
///
/// Soundness: a compiled poll that reads the word instead of its VM's baked
/// flag sees that VM's pauses only if the word is counted by that VM's
/// barrier. [`Self::enter`] makes it so first: it asks the VM's barrier for
/// this OS thread's word (`GcBarrier::loop_poll_word_for_this_thread`, which
/// registers the word with that barrier unless it already is; a word stays in
/// every barrier list it joined until its thread ends, so a later
/// registration with another VM does not uncount it here). Only then is the
/// mirror set, and it is restored to the enclosing door's value (null outside
/// every door) when the scope ends, the `set_jit_thread` / `restore_jit_thread`
/// discipline. A thread that reaches compiled code by any path that is not
/// such a door finds the mirror null and polls the baked flag, as before.
/// A word counted by two barriers sends this VM's polls to the slow path
/// during the other VM's pauses too; the slow path then finds no pause of
/// its own VM and returns: a cost, never a missed pause.
pub(crate) struct PollWordMirrorScope {
    prev: usize,
    active: bool,
}

impl PollWordMirrorScope {
    /// Publish `shared`'s view of this thread's poll word. One cached-flag
    /// load when `CRATONVM_JIT_POLL_THREAD_WORD` is off.
    #[inline]
    pub(crate) fn enter(shared: &SharedVm) -> Self {
        if !cratonvm_jit::x64::jit_poll_thread_word_enabled() {
            return Self {
                prev: 0,
                active: false,
            };
        }
        let word = shared.mem.gc_barrier.loop_poll_word_for_this_thread();
        // SAFETY: the barrier answers this OS thread's word (or its own
        // always-raised one), valid for as long as this thread's door runs.
        let addr = unsafe { &*word }.word_addr();
        let prev = cratonvm_jit::x64::publish_loop_poll_word_mirror(addr);
        Self { prev, active: true }
    }
}

impl Drop for PollWordMirrorScope {
    fn drop(&mut self) {
        if self.active {
            let _ = cratonvm_jit::x64::publish_loop_poll_word_mirror(self.prev);
        }
    }
}

/// Install `thread` (null for none) as this OS thread's JIT thread pointer,
/// with its mirror, and hand back the previous one. Interpreter round i6 wave
/// 120, lane L6: the carrier-slice scope of `jdk_continuation::enter_special`
/// (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`) clears it for the continuation's
/// slice and puts the carrier's back afterwards. Only between JIT calls (no
/// borrow of the installed thread is live): the debug borrow flag is left
/// alone.
pub(crate) fn replace_jit_thread_ptr(thread: *mut JvmThread) -> *mut JvmThread {
    let prev = JIT_THREAD.with(|t| t.replace(thread));
    cratonvm_jit::x64::publish_jit_thread_mirror(thread as usize);
    prev
}

/// Clear the JIT thread pointer after JIT execution completes.
pub fn clear_jit_thread() {
    JIT_THREAD.with(|t| t.set(std::ptr::null_mut()));
    cratonvm_jit::x64::publish_jit_thread_mirror(0);
}

/// The consolidated out-of-band JIT→interpreter signal block — see the
/// [`JIT_SIGNALS`] thread-local for field semantics.
///
/// Scalars only — the pending throwable lives on
/// [`JvmThread::jit_pending_exception`] so the collector can see and relocate
/// it (`jit-signals-root-gap.md`).
struct JitSignals {
    /// RBC.6 correctness fix — the bytecode pc of the `athrow` that produced
    /// the thread's `jit_pending_exception`, when statically known at
    /// JIT-compile time (`-1` = unknown,
    /// e.g. an exception propagated up from a dispatched callee, which this
    /// method has no bci for). Set ONLY by `jit_throw_exception` alongside the
    /// throwable itself; every other site that stashes a general exception (no
    /// known local throw site) leaves/resets this to `-1`. Consumed by
    /// `execute_jit_call` to give `route_jit_exception_through_method` a real
    /// `throw_pc` instead of the `usize::MAX`-means-unknown fallback, which
    /// cannot range-check a *typed* handler against the entry it actually
    /// belongs to — with 2+ exception-table entries whose catch types are in
    /// a subtype relationship (e.g. one entry catches `RuntimeException`, a
    /// LATER, unrelated entry catches `IllegalStateException`), the
    /// declaration-order type-only match picks whichever entry comes first
    /// regardless of which try-region actually threw, silently running the
    /// wrong handler. Confirmed via a two-sequential-try/catch differential
    /// repro (`AthrowCountBisect.twoThrowsSequential`,
    /// `vm/tests/jit_local_exception_handler_tests.rs`) before this fix.
    athrow_bci: Cell<i64>,
    /// Interpreter round i5 wave 102, lane L2: the identity of the method
    /// whose compiled exit stamped `athrow_bci` ([`method_identity_key`] of
    /// its name and descriptor), the token the exit packed beside the bci
    /// (`cratonvm_jit::implicit_null::throw_stamp_with_owner`). `0` = unknown
    /// (no token packed, or a Rust-side setter): the stamp is
    /// then trusted as before. Reset with `athrow_bci` by every general
    /// setter and the drain. See [`DrainedJitSignals::athrow_bci_owned_by`].
    athrow_owner: Cell<u64>,
    aioobe: Cell<Option<(i64, i64)>>,
    arithmetic: Cell<bool>,
    npe: Cell<bool>,
    npe_action: Cell<u8>,
    /// The COMPILED frames that were live when a JIT helper signalled the NPE.
    ///
    /// An implicit NPE in compiled code is not thrown where it happens: the
    /// helper sets `npe`, returns `i64::MIN`, compiled code returns to the
    /// interpreter, and only THEN is the `java/lang/NullPointerException`
    /// constructed. `fillInStackTrace` therefore runs on a stack the compiled
    /// frames have already left, and the trace loses every method between the
    /// throw site and the first interpreter frame — the whole point of the
    /// trace. Snapshot them while they are still on the stack; the drain hands
    /// them to the throwable. See
    /// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`.
    ///
    /// `RefCell` rather than `Cell` because the payload is not `Copy`; it is
    /// only ever borrowed for the length of a `take`/`replace`, never across a
    /// call, so it cannot be re-entered.
    trap_frames:
        std::cell::RefCell<Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>>,
    deopt: Cell<bool>,
    /// What raised `deopt` last: a compiled body's frameless trap stub
    /// (`jit_uncommon_trap`), or anything else. See [`FramelessTrapSite`].
    frameless_trap: Cell<FramelessTrapSite>,
    /// gce e1/f: the pending exception ESCAPED the method named here
    /// ([`redispatch_escape_hash`]; `0` = none) after a not-entrant
    /// re-dispatch ran it through the VM's invoke path
    /// (`jit_not_entrant_entry_body`), which consults the method's exception
    /// table itself. So the call site's service must not search that table a
    /// second time. See [`take_redispatch_escape_for`]. Cleared by every
    /// pending-exception setter and by [`take_all_jit_signals`].
    redispatch_escape: Cell<u64>,
    /// nio21 wave 17, lane L5: an implicit-exception setter skipped its
    /// compiled-frame snapshot because the pad's local-handler probe follows
    /// (`cratonvm_jit::implicit_null::TRAP_SNAPSHOT_DEFER_BIT`). Taken by
    /// [`jit_local_handler_lookup`], which runs next with the same frames
    /// live and takes the snapshot itself unless the throw is served by the
    /// fast throw's preallocated instance. See [`DeferredTrapSnapshot`].
    trap_snapshot_deferred: Cell<DeferredTrapSnapshot>,
    /// JIT round 20 (lane throw): how many compiled activations per chain
    /// entry a trap snapshot walks (innermost first), as the bounded live
    /// capture walks them (`stackwalker::throwable_walk_activation_bound` of
    /// the VM's `-XX:MaxJavaStackTraceDepth`). `usize::MAX` (the whole chain,
    /// as before) until a door that knows the VM sets it
    /// ([`note_trap_snapshot_bound`]). A scalar, like every field here.
    trap_snapshot_bound: Cell<usize>,
}

/// The snapshot an implicit-exception setter deferred to the probe that
/// follows it (nio21 wave 17, lane L5; proposal L5W16-1): what to call to take
/// it exactly as the setter would have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferredTrapSnapshot {
    /// Nothing deferred: the setter snapshotted (or the snapshot is off).
    None,
    /// [`snapshot_trap_frames_at_bci`] with this stamped bci (AIOOBE, `/ by zero`).
    AtBci(i64),
    /// [`snapshot_trap_frames`] with this inline null-check trap key (NPE).
    NpeKey(u32),
}

/// Record a deferred snapshot in place of taking one: the slot is cleared
/// exactly as the setter's own snapshot would have overwritten it. With the
/// snapshot switched off (`CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1`) nothing is
/// recorded and nothing is cleared, as the setter does then.
fn defer_trap_snapshot(deferred: DeferredTrapSnapshot) {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = None;
        s.trap_snapshot_deferred.set(deferred);
    });
}

/// Take the deferred-snapshot record ([`JitSignals::trap_snapshot_deferred`]).
fn take_deferred_trap_snapshot() -> DeferredTrapSnapshot {
    JIT_SIGNALS.with(|s| s.trap_snapshot_deferred.replace(DeferredTrapSnapshot::None))
}

/// Take, now, the snapshot a setter deferred, into the signal block exactly
/// where the setter would have put it. Must run while the frames the setter
/// saw are still live: inside the probe's `jit_local_handler_lookup`, before
/// it returns to the pad.
///
/// The one difference from the setter's own walk: the probe has already
/// cleared this frame's safepoint-id slot to the no-map sentinel. The
/// innermost frame's bci and inline chain do not come from that slot when the
/// trap site is described (an NPE with a trap key, `apply_npe_trap_site`; a
/// stamped AIOOBE / `/ by zero` bci on a single-pass precise entry,
/// `apply_direct_throw_trap_bci`), which is every pad that sets the defer bit
/// in practice; otherwise the walk answers `-1` for that frame where the
/// setter would have answered the line of its last call, both of which are
/// the undescribed-site answer.
fn take_deferred_trap_snapshot_now(deferred: DeferredTrapSnapshot) {
    match deferred {
        DeferredTrapSnapshot::None => {}
        DeferredTrapSnapshot::AtBci(bci) => snapshot_trap_frames_at_bci(bci),
        DeferredTrapSnapshot::NpeKey(key) => snapshot_trap_frames(key),
    }
}

#[cfg(test)]
mod nio21w17_l5_deferred_trap_snapshot_tests {
    use super::{
        defer_trap_snapshot, take_deferred_trap_snapshot, trap_frame_snapshot_enabled,
        DeferredTrapSnapshot, JIT_SIGNALS,
    };

    /// A deferred record is taken exactly once, and deferring clears a stale
    /// snapshot the way the setter's own snapshot would have overwritten it.
    #[test]
    fn a_deferred_snapshot_is_recorded_once_and_clears_the_slot() {
        if !trap_frame_snapshot_enabled() {
            return;
        }
        JIT_SIGNALS.with(|s| *s.trap_frames.borrow_mut() = Some(Vec::new()));
        defer_trap_snapshot(DeferredTrapSnapshot::AtBci(7));
        assert!(JIT_SIGNALS.with(|s| s.trap_frames.borrow().is_none()));
        assert_eq!(
            take_deferred_trap_snapshot(),
            DeferredTrapSnapshot::AtBci(7)
        );
        assert_eq!(take_deferred_trap_snapshot(), DeferredTrapSnapshot::None);
        defer_trap_snapshot(DeferredTrapSnapshot::NpeKey(3));
        assert_eq!(
            take_deferred_trap_snapshot(),
            DeferredTrapSnapshot::NpeKey(3)
        );
    }
}

/// Which producer raised the thread's deopt flag last, for the replay rule of
/// a re-run after a BARE sentinel (round 13 wave 8, lane replay4;
/// `deopt_resume::frameless_rerun_is_exact`).
///
/// Every producer of the flag goes through [`set_jit_deopt_pending`], which
/// records [`Self::Helper`]; the frameless trap stub's helper
/// (`jit_uncommon_trap`) then overwrites it with [`Self::Stub`]. So while the
/// flag is set, this names its producer. Taking the flag leaves the record,
/// which is read only beside a set flag's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FramelessTrapSite {
    /// Nothing recorded on this thread yet, or a stub with no compile id.
    Unknown,
    /// A helper raised the flag: an exception stub, a helper that could not
    /// build its throwable or found no JIT thread, a contained panic, a
    /// dropped frame. None of these is one of the body's frameless trap stubs.
    Helper,
    /// The frameless trap stub of the body with this compile id, at `bci`
    /// (interpreter bytecode). `exact` is that body's
    /// `frameless_traps_replay_exact`, read while it was trapping.
    ///
    /// `method` (round 13 wave 13, lane jitfix;
    /// `r13w12-replay6-callee-service-trusts-any-stubs-exact-stamp`): the
    /// trapping body's METHOD, as [`frameless_trap_method_hash`] of its
    /// label, `0` when the label did not parse. `exact` is a statement about
    /// re-running that method only, so a call-site service trusts it only
    /// for a callee with the same identity. Deliberately not the published
    /// body's compile id: the trap's own evict / recompile can replace that
    /// body before the service asks.
    Stub {
        compile_id: u32,
        bci: u32,
        exact: bool,
        method: u64,
    },
    /// A compiled callee's stashed frame that `try_resume_trapped_callee`
    /// dropped (its rebuild failed after a collection), judged by the replay
    /// rule at its own bci before it was dropped. Only the callee's call-site
    /// service may read it; any other sink answers the whole-body rule.
    Dropped { exact: bool },
    /// gce e1/f: an owned-arguments call site handed its callee's trap to the
    /// caller's caller, which re-runs the CALLER from entry
    /// ([`note_owned_args_handoff`]). The site admits ownership only where the
    /// caller's bytecode before the call commits nothing, so that re-run
    /// replays the callee's prefix alone, and `exact` is the callee replay's
    /// verdict. Any sink may read it: the next one to re-run is the caller's.
    /// Before, the sink judged the bare sentinel as the caller body's own
    /// frameless trap and could call the hand-off unsound and retire the
    /// caller.
    Handoff { exact: bool },
}

/// Record that the deopt flag just raised stands for a dropped callee frame
/// judged `exact` ([`FramelessTrapSite::Dropped`]; round 13 wave 8, lane
/// replay4). Call right after [`set_jit_deopt_pending`].
fn note_dropped_callee_frame(exact: bool) {
    JIT_SIGNALS.with(|s| s.frameless_trap.set(FramelessTrapSite::Dropped { exact }));
}

/// The method identity `FramelessTrapSite::Stub::method` carries: FNV-1a
/// over `class.method` and the descriptor, never `0` (which means "unknown").
/// The producer hashes the trapping body's `method_label`
/// ([`split_method_label`]); a call-site service hashes the callee it resolved
/// (its declaring class's name, the site's name and descriptor) -- the same
/// three strings the JIT cache is keyed on at both ends.
pub(crate) fn frameless_trap_method_hash(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = [
        class_name.as_bytes(),
        ".".as_bytes(),
        method_name.as_bytes(),
        descriptor.as_bytes(),
    ];
    for part in parts {
        for &b in part {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h.max(1)
}

/// The producer of the thread's last deopt flag ([`FramelessTrapSite`]).
pub(crate) fn last_frameless_trap_site() -> FramelessTrapSite {
    JIT_SIGNALS.with(|s| s.frameless_trap.get())
}

/// One-shot snapshot-and-clear of EVERY out-of-band JIT signal, produced by
/// [`take_all_jit_signals`] in a single thread-local access. The
/// interpreter's post-JIT-return drain consumes this instead of six separate
/// `take_*` calls.
pub(crate) struct DrainedJitSignals {
    pub exception: Option<ObjectRef>,
    /// See `JitSignals::athrow_bci`. `-1` iff unknown (matches the field's
    /// own sentinel, so callers can pass it straight through as `usize::MAX`
    /// when negative without an extra branch).
    pub athrow_bci: i64,
    /// See `JitSignals::athrow_owner`; `0` = unknown.
    pub athrow_owner: u64,
    pub aioobe: Option<(i64, i64)>,
    pub arithmetic: bool,
    pub npe: bool,
    /// The compiled frames that were live when the helper signalled `npe`.
    /// See `JitSignals::trap_frames`; drained here so that no path
    /// can construct the NPE without also being handed the frames it is
    /// about to lose.
    pub trap_frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    /// Drained alongside `npe` for hygiene (a stale action code must not
    /// outlive its NPE). Read by the two restash paths, which put it back with
    /// the flag and the frame snapshot — see [`restash_jit_pending_npe`].
    pub npe_action: u8,
    pub deopt: bool,
    /// See `JitSignals::redispatch_escape` (`0` = none). Interpreter round i5
    /// wave 103, lane L2: carried out of the drain (it used to be cleared
    /// unread) so an interpreter door can see that the throwable already left
    /// the method it entered through a not-entrant re-dispatch
    /// ([`Self::escaped_a_redispatch_of`]).
    pub redispatch_escape: u64,
}

/// Snapshot-and-clear ALL JIT signals: one thread-local access for the six
/// scalars, plus one field read on `thread` for the pending throwable.
/// Draining everything unconditionally is deliberate: a signal surviving into
/// the next unrelated JIT call was the recurring Round-8..11 leak-bug class,
/// and clearing a flag nobody set is free.
///
/// `thread` must be the thread whose JIT call just returned — the same one
/// that was installed via `set_jit_thread`. It is the drain's *only* way to
/// reach the throwable now that the reference is GC-rooted on the thread
/// rather than parked in TLS; passing a different thread silently drains an
/// empty slot and strands the exception on the real one.
#[inline]
pub(crate) fn take_all_jit_signals(thread: &mut JvmThread) -> DrainedJitSignals {
    let exception = thread.jit_pending_exception.take();
    JIT_SIGNALS.with(|s| {
        // gce e1/f: a call-site service that wants it takes it first
        // ([`take_redispatch_escape_for`]); drained here for hygiene, and
        // handed out (interpreter round i5 wave 103, L2) for the interpreter
        // doors ([`DrainedJitSignals::escaped_a_redispatch_of`]).
        let redispatch_escape = s.redispatch_escape.replace(0);
        // nio21 wave 17 L5: the probe's lookup takes it before any drain;
        // a record that outlived its trap must not defer the next one's.
        s.trap_snapshot_deferred.set(DeferredTrapSnapshot::None);
        DrainedJitSignals {
            redispatch_escape,
            exception,
            athrow_bci: s.athrow_bci.replace(-1),
            athrow_owner: s.athrow_owner.replace(0),
            aioobe: s.aioobe.take(),
            arithmetic: s.arithmetic.take(),
            npe: s.npe.take(),
            trap_frames: s.trap_frames.borrow_mut().take(),
            npe_action: s.npe_action.take(),
            deopt: s.deopt.take(),
        }
    })
}

/// Interpreter round i5 wave 102, lane L2: the method identity a throw-site
/// stamp carries ([`JitSignals::athrow_owner`]): the method's
/// `cratonvm_jit::implicit_null::method_stamp_token` of the body's declaring
/// class, name and descriptor (the class is in it because `run()V`-like
/// shapes recur across classes). Never `0`.
pub(crate) fn method_identity_key(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    u64::from(cratonvm_jit::implicit_null::method_stamp_token(
        class_name,
        method_name,
        descriptor,
    ))
}

/// [`method_identity_key`] of a `CompiledMethod::method_label`
/// (`"pkg/C.name:(desc)R"`), or `0` for a label that is not a method's (a
/// lambda adapter's `"lambda-adapter->0x.."`, an empty label).
fn method_identity_key_of_label(label: &str) -> u64 {
    u64::from(cratonvm_jit::implicit_null::method_stamp_token_of_label(
        label,
    ))
}

/// Split a compiled exit's stamp word (`cratonvm_jit::implicit_null::
/// throw_stamp_with_owner`) into the stamp and the stamping method's token
/// (`0` = unknown). The token is baked at compile time from the method's
/// class, name and descriptor (build w102 hand-back: not a compile id, which made codegen
/// non-deterministic), so nothing is resolved here.
fn split_throw_stamp_word(raw: i64) -> (i64, u64) {
    let (stamp, token) = cratonvm_jit::implicit_null::split_throw_stamp_owner(raw);
    (stamp, u64::from(token))
}

impl DrainedJitSignals {
    /// Interpreter round i5 wave 102, lane L2 (page
    /// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    /// `athrow_bci` as a throw pc of the method `class_name.method_name
    /// descriptor` (the declaring class of the drained body), or `-1` when the
    /// stamp names ANOTHER method. Before, a
    /// foreign stamp that happened to be an instruction boundary of this
    /// method was range-checked as this method's own pc; the drains now see
    /// "unknown" for it, which is what it is. A stamp of unknown owner is
    /// answered as before.
    pub(crate) fn athrow_bci_owned_by(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> i64 {
        self.athrow_bci_owned_by_key(method_identity_key(class_name, method_name, descriptor))
    }

    /// [`Self::athrow_bci_owned_by`] for the drain that holds the body it
    /// entered rather than the method's name: keyed by the body's label. A
    /// label that names no method (a lambda adapter) checks nothing.
    pub(crate) fn athrow_bci_owned_by_label(&self, label: &str) -> i64 {
        match method_identity_key_of_label(label) {
            0 => self.athrow_bci,
            key => throw_stamp_owned_by_named(self.athrow_bci, self.athrow_owner, key, label),
        }
    }

    fn athrow_bci_owned_by_key(&self, key: u64) -> i64 {
        throw_stamp_owned_by(self.athrow_bci, self.athrow_owner, key)
    }

    /// Interpreter round i5 wave 103, lane L2 (page
    /// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    /// did the pending throwable already LEAVE the method `method_name
    /// descriptor` through a not-entrant re-dispatch, so that no table of
    /// that method may be searched for it again?
    ///
    /// A door checks that the body is entrant BEFORE it enters, and the
    /// background tier-up can retire the body between that check and the
    /// call. The entry then runs the not-entrant stub, whose first pass answers
    /// the call through the VM's invoke path (`jit_not_entrant_entry_body`):
    /// the method ran there, its handlers were offered the exception at the
    /// real throw pc, and the throwable that escaped is re-posted with NO
    /// stamp. The door then read "pc unknown", and the pc-unknown search
    /// matched a typed row by exception class alone: one swallowed exception
    /// per supersede (`test_callee_that_cannot_catch_propagates_instead_of_re_running`,
    /// 19997 of 20000 under load). The call-site services already honour the
    /// record (`take_redispatch_escape_for`); the doors drained it unread.
    ///
    /// Only an UNSTAMPED throwable counts: the re-post sets no stamp, and the
    /// stub returns straight to the door, so a stamp means a compiled frame of
    /// some body ran its own exit after the record was written (a stale record
    /// left by a nested activation of the same method).
    pub(crate) fn escaped_a_redispatch_of(&self, method_name: &str, descriptor: &str) -> bool {
        self.exception.is_some()
            && self.athrow_bci < 0
            && self.redispatch_escape != 0
            && not_entrant_escape_enabled()
            && self.redispatch_escape == redispatch_escape_hash(method_name, descriptor)
    }
}

/// The body of [`DrainedJitSignals::athrow_bci_owned_by`] on the two fields,
/// for a drain that has already moved the throwable out of its record. `key`
/// is the drained method's [`method_identity_key`].
pub(crate) fn throw_stamp_owned_by(athrow_bci: i64, athrow_owner: u64, key: u64) -> i64 {
    throw_stamp_owned_by_named(athrow_bci, athrow_owner, key, "")
}

/// [`throw_stamp_owned_by`] for a drain that knows the drained body's label,
/// which the census line then names (interpreter round i5 wave 104, lane L2).
fn throw_stamp_owned_by_named(athrow_bci: i64, athrow_owner: u64, key: u64, label: &str) -> i64 {
    if athrow_bci < 0 || athrow_owner == 0 || key == 0 || athrow_owner == key {
        return athrow_bci;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        // Interpreter round i5 wave 104, lane L2: `JITC-PC-CENSUS` is the one
        // tag both close-criterion lines of page `i101-L2-pc-unknown-...` carry
        // (this one and `pc-unknown typed match`), so a battery log is counted
        // with a single `grep -c JITC-PC-CENSUS`.
        eprintln!(
            "[cratonvm-jitc] JITC-PC-CENSUS foreign throw stamp refused: bci={athrow_bci} owner={athrow_owner:#x} drained={key:#x} label={label}{}",
            jitc_census_backtrace()
        );
    }
    -1
}

/// Interpreter round i5 wave 104, lane L2: the Rust call path of a
/// `JITC-PC-CENSUS` line, so one run names the drain (and so the producer
/// whose stamp leaked). Opt-in through the standard library's own
/// `RUST_LIB_BACKTRACE=1` (or `RUST_BACKTRACE=1`); empty otherwise. Only
/// ever called under `CRATONVM_DBG_JITC`.
pub(crate) fn jitc_census_backtrace() -> String {
    let bt = std::backtrace::Backtrace::capture();
    if bt.status() == std::backtrace::BacktraceStatus::Captured {
        format!("\n{bt}")
    } else {
        String::new()
    }
}

/// gce e1/f: the identity [`JitSignals::redispatch_escape`] records -- the
/// method's name and descriptor, which a not-entrant record and the call
/// site that reached it spell identically (the class a site names may be a
/// subclass of the declaring one). Never `0`.
fn redispatch_escape_hash(method_name: &str, descriptor: &str) -> u64 {
    frameless_trap_method_hash("", method_name, descriptor)
}

/// `CRATONVM_JIT_NOT_ENTRANT_ESCAPE` -- default ON, `=0` restores the old
/// behaviour (the call site's service searches the re-dispatched method's
/// exception table again). Read on the throw path only, no static.
fn not_entrant_escape_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_NOT_ENTRANT_ESCAPE")
}

/// gce e1/f (`gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler-DONE-20260928.md`):
/// the not-entrant re-dispatch returned `rc`; when that is the sentinel with a
/// throwable pending, the throwable has already left the method (the VM's
/// invoke path ran its handlers), so record that for the call site. Returns
/// `rc`.
fn note_redispatch_escape(thread: &JvmThread, info: &JitInvokeInfo, rc: i64) -> i64 {
    if rc == i64::MIN && thread.jit_pending_exception.is_some() && not_entrant_escape_enabled() {
        let h = redispatch_escape_hash(info.method_name, info.descriptor);
        JIT_SIGNALS.with(|s| s.redispatch_escape.set(h));
    }
    rc
}

/// `CRATONVM_JIT_DISPATCH_ERROR_ESCAPE_RECORD` (interpreter round i5 wave 106,
/// lane L2; parity fix, default ON, `=0` records only the not-entrant
/// re-dispatch as before). Read on the throw path only.
fn dispatch_error_escape_record_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DISPATCH_ERROR_ESCAPE_RECORD")
}

/// Interpreter round i5 wave 106, lane L2 (page
/// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
/// [`handle_jit_dispatch_error`] has just re-posted a throwable for the call of
/// `info` (`pending`: it stashed one). Every such re-post is a throwable that
/// already LEFT `info`'s method: the VM ran the method's bytecode (a trapped
/// frame resumed, a handler resumed in an interpreted frame, a re-run from
/// entry, a not-entrant re-dispatch, the by-name invoke path), whose unwinder
/// searched its table at the real throw pc; or the call failed before the
/// method was entered (a class-initialization or linkage error raised in the
/// dispatch helper), where the method's handlers do not apply at all. The
/// re-post carries no stamp, so the next consumer used to read "pc unknown":
/// the w105 battery's 115,360 `JITC-UNSTAMPED-ESCAPE` lines
/// (`L1W93XPair.rethrow`). Record it, the same record the not-entrant stub
/// writes ([`note_redispatch_escape`]): the call-site services
/// ([`take_redispatch_escape_for`]) and the interpreter doors
/// ([`DrainedJitSignals::escaped_a_redispatch_of`]) then propagate without a
/// search. Its exact-pc equivalent is the stamp `OutsideAllRanges`: the throw is
/// at a pc of the method no row of it catches, which is what an escape is.
///
/// A compiled exit that stamps afterwards retires the record
/// ([`jit_set_throw_bci`]), so it can only speak for the activation it was
/// written for (a self-recursive caller's own pad re-stamps before its own
/// caller's service reads anything).
fn note_dispatch_error_escape(pending: bool, method_name: &str, descriptor: &str) {
    if pending && not_entrant_escape_enabled() && dispatch_error_escape_record_enabled() {
        let h = redispatch_escape_hash(method_name, descriptor);
        JIT_SIGNALS.with(|s| s.redispatch_escape.set(h));
    }
}

/// gce e1/f: take the escape record ([`JitSignals::redispatch_escape`]) and
/// say whether it names `info`'s method, i.e. whether the pending throwable
/// already left the callee this site called. Taken, so it can speak for one
/// site only; a record naming another method is dropped (it was stale).
fn take_redispatch_escape_for(info: &JitInvokeInfo) -> bool {
    redispatch_escape_record_names(
        take_redispatch_escape_record(),
        info.method_name,
        info.descriptor,
    )
}

/// The raw escape record ([`JitSignals::redispatch_escape`], `0` = none),
/// taken. For a service that must ask about TWO methods with one record:
/// the MIC door of a SAM call, whose `info` names the SAM (`call`, `apply`)
/// while the not-entrant record names the body that was re-dispatched -- the
/// lambda IMPL (interpreter round i5 wave 104, lane L2).
fn take_redispatch_escape_record() -> u64 {
    JIT_SIGNALS.with(|s| s.redispatch_escape.replace(0))
}

/// Does `record` (from [`take_redispatch_escape_record`]) name the method
/// `method_name descriptor`?
fn redispatch_escape_record_names(record: u64, method_name: &str, descriptor: &str) -> bool {
    record != 0 && record == redispatch_escape_hash(method_name, descriptor)
}

/// Kill switch for the compiled-frame snapshot taken when a JIT helper signals
/// an implicit NPE. Default ON; `CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1` restores
/// the historical (frame-losing) trace, so the difference is an A/B inside one
/// binary rather than a comparison across two builds.
fn trap_frame_snapshot_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT"))
}

/// Snapshot the live compiled frames for a JIT-signalled NPE.
///
/// Called from the two `set_jit_pending_npe*` setters, i.e. from inside the
/// helper, with every compiled frame between the throw site and the
/// interpreter still on the stack. Cheap by construction: this records the
/// same small structs the GC root walk already builds, and does no class-store
/// lookup, no string formatting and takes no lock.
fn snapshot_trap_frames(trap_key: u32) {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    let mut frames = trap_snapshot_walk();
    // The one frame the walk cannot put a line on is the one that raised: an
    // inline null check publishes no safepoint id. `trap_key` is the site id its
    // cold trampoline passed in, and this is the only place both the key and
    // the frames exist at once.
    crate::jit::conservative_roots::apply_npe_trap_site(&mut frames, trap_key);
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = (!frames.is_empty()).then_some(frames);
    });
}

/// [`snapshot_trap_frames`] for the two DIRECT-THROW helpers
/// (`jit_throw_arithmetic`, `jit_throw_aioobe`), whose single-pass stub knows
/// the trapping bci exactly: `stamped_bci` is that bci, or `-1` for none.
///
/// A zero-divisor guard or a bounds check publishes no safepoint id, so the
/// walk cannot place the frame that raised (`-1`, or the line of the last call
/// it made); `apply_direct_throw_trap_bci` puts the stub's bci on it. Page
/// `r11w14-orch-implicit-exception-frame-reports-line-minus-one`.
fn snapshot_trap_frames_at_bci(stamped_bci: i64) {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    let mut frames = trap_snapshot_walk();
    crate::jit::conservative_roots::apply_direct_throw_trap_bci(&mut frames, stamped_bci);
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = (!frames.is_empty()).then_some(frames);
    });
}

/// JIT round 20 (lane throw,
/// `jr20-deep-stack-throws-walk-the-whole-compiled-chain-and-compiled-only-throw-loops-never-take-the-trace-gc-request-FIXED-20261012.md`):
/// the compiled-frame walk of a trap snapshot. It used to read the WHOLE chain
/// for every implicit trap, so a recursion 10-20k compiled levels deep that
/// catches one exception per level paid O(depth^2) frame visits (and the
/// capture laid every snapshot frame out before the 1024 cap) where HotSpot
/// stops at `MaxJavaStackTraceDepth`. Bounded to the innermost
/// [`JitSignals::trap_snapshot_bound`] activations of each chain entry -- the
/// bound the live capture walk already uses for the same cap, so a capture
/// that takes the snapshot (stand-in or splice, both capped) keeps the same
/// frames: an entry is cut only past `bound` (>= 4097) activations, outside
/// the kept innermost `cap` frames.
///
/// The bound is per thread, set by the doors that know the VM; on a thread
/// that alternates between VMs with different caps, the last door's bound
/// applies (the cut can then drop frames only past the smaller VM's bound).
fn trap_snapshot_walk() -> Vec<crate::jit::conservative_roots::ActiveCompiledFrame> {
    let bound = JIT_SIGNALS.with(|s| s.trap_snapshot_bound.get());
    if bound == usize::MAX {
        return crate::jit::conservative_roots::active_compiled_frames();
    }
    crate::jit::conservative_roots::active_compiled_frames_bounded(0, bound)
}

/// JIT round 20 (lane throw): set this thread's trap-snapshot walk bound
/// ([`trap_snapshot_walk`]) from `vm`'s `-XX:MaxJavaStackTraceDepth`.
/// `CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED=0` keeps the whole-chain walk.
pub(crate) fn note_trap_snapshot_bound(vm: &SharedVm) {
    let bound = if trap_snapshot_bounded_enabled() {
        crate::runtime::stackwalker::throwable_walk_activation_bound(
            vm.config.max_java_stack_trace_depth,
        )
    } else {
        usize::MAX
    };
    JIT_SIGNALS.with(|s| s.trap_snapshot_bound.set(bound));
}

/// `CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED` -- default ON (JIT round 20, lane
/// throw), `=0` walks the whole compiled chain per trap snapshot, as before.
/// Read only where a door sets the bound, not per walk.
///
/// JIT round 20 wave 18 (lane xThrow, THROW20-5): those doors are two per
/// compiled implicit catch (the lookup and the construction), so the switch
/// was a flag-table read (name-set probe, override table) twice per throw.
/// Under `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD` (default ON) it is the value
/// latched with the other throw-path switches; `=0` reads it per call again.
fn trap_snapshot_bounded_enabled() -> bool {
    let switches = crate::runtime::exceptions::throw1_switches();
    if switches.implicit_throw_lean_build {
        return switches.trap_snapshot_bounded;
    }
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TRAP_SNAPSHOT_BOUNDED")
}

/// Take the compiled frames snapshotted for a pending JIT NPE, if any.
///
/// The drain calls this beside [`take_jit_pending_npe`]. Always a `take`: a
/// snapshot that outlived its NPE would be attached to an unrelated throwable,
/// and a trace that is confidently wrong is worse than one that is short.
pub fn take_jit_pending_trap_frames(
) -> Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>> {
    JIT_SIGNALS.with(|s| s.trap_frames.borrow_mut().take())
}

/// Store a pending Java exception from JIT dispatch. Called when
/// `jit_invoke_dispatch` encounters an `ExceptionThrown` error.
///
/// Always resets `athrow_bci` to `-1` (unknown) — this is the general,
/// origin-agnostic setter (a dispatched callee threw, or a re-stash), never
/// the direct-local-athrow path. Only `jit_throw_exception`'s dedicated
/// `set_jit_pending_exception_with_bci` may set a real bci, and only for the
/// exception it is stashing in that same call.
/// The throwable is stored on `thread` (not in `JIT_SIGNALS`) so that a
/// collection occurring between this stash and the interpreter's drain both
/// keeps it alive and rewrites it — see [`JvmThread::jit_pending_exception`].
fn set_jit_pending_exception(thread: &mut JvmThread, exc: ObjectRef) {
    thread.jit_pending_exception = Some(exc);
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
        // gce e1/f: a new (or re-stashed) throwable carries no escape record.
        s.redispatch_escape.set(0);
    });
}

/// RBC.6 correctness fix — sibling of `set_jit_pending_exception` for the ONE
/// call site (`jit_throw_exception`) that knows the exact bytecode pc of the
/// `athrow` producing this exception at JIT-compile time. See
/// `JitSignals::athrow_bci` for why this matters (typed-handler routing
/// correctness with 2+ exception-table entries when `throw_pc` would
/// otherwise be `usize::MAX`).
fn set_jit_pending_exception_with_bci(thread: &mut JvmThread, exc: ObjectRef, bci: i64) {
    set_jit_pending_exception_with_stamp(thread, exc, bci, 0);
}

/// [`set_jit_pending_exception_with_bci`] with the stamping method's identity
/// (`JitSignals::athrow_owner`, `0` = unknown). Interpreter round i5 wave 102,
/// lane L2.
fn set_jit_pending_exception_with_stamp(
    thread: &mut JvmThread,
    exc: ObjectRef,
    bci: i64,
    owner: u64,
) {
    thread.jit_pending_exception = Some(exc);
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(bci);
        s.athrow_owner.set(owner);
        // gce e1/f: as `set_jit_pending_exception`.
        s.redispatch_escape.set(0);
    });
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending Java exception. Used by `try_osr` when the OSR return path
/// drained the flag but cannot return an error from its own signature — the
/// exception must be re-posted so the interpreter dispatch loop drains it on
/// the next iteration via `take_jit_pending_exception`. Crate-pub because
/// only the OSR entry path should use it; ordinary JIT helpers set the flag
/// directly via the private `set_jit_pending_exception` above.
///
/// Deliberately loses any `athrow_bci` the exception may have carried before
/// being taken (this is the general re-stash path, not the direct-athrow
/// one) — always falls back to the pre-existing `usize::MAX`-means-unknown
/// behavior for the re-stashed exception, never a regression, just not the
/// newly-precise case.
pub(crate) fn stash_jit_pending_exception(thread: &mut JvmThread, exc: ObjectRef) {
    set_jit_pending_exception(thread, exc);
}

/// Stash a `java.lang.InternalError` naming a JIT runtime helper whose body
/// panicked, for [`crate::jit::helper_guard::OnPanic::Throw`].
///
/// Returns whether a throwable was stashed. `vm_ptr` is the helper's own
/// `SharedVm` argument, or `0` for a helper that takes none, in which case the
/// process VM is used when exactly one is published. Nothing is stashed when no
/// JIT thread is installed or the throwable cannot be built, and the guard then
/// returns the bare sentinel: the same fallback [`jit_alloc_oom`] has.
///
/// Replaces an exception that was already pending. The helper that panicked did
/// not finish whatever it was doing with that exception, and the panic is the
/// report that has to reach Java.
#[cold]
pub(crate) fn stash_jit_helper_panic(vm_ptr: i64, helper: &'static str, detail: &str) -> bool {
    // SAFETY: called only from `helper_guard::contain` on the thread that ran
    // the guarded helper, after the helper's body has fully unwound, so every
    // `JitThreadGuard` that body took has already been dropped.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return false;
    };
    let process_vm = if vm_ptr == 0 {
        crate::native::jni::process_vm_strict()
    } else {
        None
    };
    let vm: &SharedVm = match process_vm.as_deref() {
        Some(vm) => vm,
        // SAFETY: a non-zero `vm_ptr` is the `SharedVm` pointer compiled code
        // handed the guarded helper, under that helper's own caller contract.
        None if vm_ptr != 0 => unsafe { &*(vm_ptr as *const SharedVm) },
        None => return false,
    };
    let message = format!("JIT runtime helper {helper} panicked: {detail}");
    match crate::runtime::exceptions::create_exception_object(
        vm,
        thread,
        "java/lang/InternalError",
        Some(&message),
    ) {
        Ok(exc) => {
            set_jit_pending_exception(thread, exc);
            true
        }
        Err(_) => false,
    }
}

/// Forget the `athrow` bci carried by the pending exception, keeping the
/// exception itself.
///
/// Called when a dispatch helper hands a CALLEE's pending exception back to its
/// compiled caller: from that point the bci names a method that is no longer on
/// the stack, and the caller's drain would range-check its own exception table
/// against it. See `JitSignals::athrow_bci`.
pub(crate) fn clear_jit_athrow_bci() {
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
    });
}

/// [`take_jit_deopt_pending`] and [`clear_jit_athrow_bci`] in one
/// `JIT_SIGNALS` access: the entry retire of an interpreter-to-compiled door
/// under `CRATONVM_JIT_DOOR_THREAD_BLOCK` (t1 wave 5, lane S). The same three
/// cells end in the same state.
#[inline]
pub(crate) fn retire_door_entry_signals() {
    JIT_SIGNALS.with(|s| {
        s.deopt.set(false);
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
    });
}

/// Read the `athrow` bci without consuming it.
///
/// Exists for the one consumer that must see the bci BEFORE
/// [`clear_jit_athrow_bci`] runs: `route_implicit_exc_through_callee` routes
/// through the CALLEE's own exception table, and the stamped bci is that
/// callee's own throw site — precisely the pc that lookup needs. See the
/// capture at that function's entry.
pub(crate) fn peek_jit_athrow_bci() -> i64 {
    JIT_SIGNALS.with(|s| s.athrow_bci.get())
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending-NPE flag. See `stash_jit_pending_exception` for the OSR
/// drain-without-route rationale.
///
/// JIT round 20 wave 7 (lane fuzzC): its last caller, `jit_throw_exception`'s
/// null arm, now raises [`set_jit_pending_npe_athrow`]; kept for the docs that
/// name it as the bare-signal setter.
#[allow(dead_code)]
pub(crate) fn stash_jit_pending_npe() {
    set_jit_pending_npe();
}

/// Raise the pending-NPE flag WITHOUT taking a compiled-frame snapshot and
/// WITHOUT touching the JEP-358 action code.
///
/// For the one shape [`stash_jit_pending_npe`] is wrong for: a door that
/// drained the flag with [`take_jit_pending_npe`] and is putting it back. That
/// take leaves `trap_frames` untouched, so the snapshot from the trap
/// is still there and is still the right one; taking another would overwrite it
/// with a stack the raising frame has already left.
///
/// The action cell is left alone for exactly the same reason, and it used to be
/// ZEROED here. That take does not consume the action either, so the code
/// sitting in the cell is this NPE's own — clearing it threw away the one thing
/// that tells the eventual drain which opcode trapped, and an `invoke` on a
/// null receiver that took a round trip through a door which declined to
/// service it came out unmessaged. See
/// `runtime::interpreter::jit_npe_message::jit_npe_message`, which reads it.
pub(crate) fn set_jit_pending_npe_flag_only() {
    JIT_SIGNALS.with(|s| s.npe.set(true));
}

/// Put back an NPE that [`take_all_jit_signals`] drained WHOLE -- flag, JEP-358
/// action code and the compiled-frame snapshot -- exactly as it was found.
///
/// The action code was previously dropped by every restash (the setter resets
/// it to `0`), so an implicit NPE that took a round trip through a door which
/// declined to service it lost its "Cannot load from int array" message. Both
/// halves travel together because both describe the same trap.
pub(crate) fn restash_jit_pending_npe(
    action: u8,
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(action);
        *s.trap_frames.borrow_mut() = frames;
    });
}

/// Put back a div-by-zero that [`take_all_jit_signals`] drained WHOLE — flag
/// AND compiled-frame snapshot — exactly as it was found.
///
/// The sibling of [`restash_jit_pending_npe`], and it did not exist until
/// 2026-09-05: both whole-drain restores called the bare
/// [`stash_jit_pending_arithmetic`], which puts the flag back and lets
/// `DrainedJitSignals` drop the snapshot on the floor. The next door then
/// constructed the `ArithmeticException` with nothing to attach, and the
/// throwable kept the empty trace `fillInStackTrace` had just built — the
/// exact defect `restash_jit_pending_npe`'s own doc describes for its half,
/// one signal kind over.
///
/// Do NOT re-sample here. `active_compiled_frames()` at a restore point
/// describes a shallower stack than the trap did, which is the mistake
/// [`restash_implicit_signal`]'s NPE arm is written to avoid; the frames belong
/// to the trap, not to the drain that happened to pass them through.
pub(crate) fn restash_jit_pending_arithmetic(
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| {
        s.arithmetic.set(true);
        *s.trap_frames.borrow_mut() = frames;
    });
}

/// Put back an AIOOBE that [`take_all_jit_signals`] drained WHOLE — payload
/// AND compiled-frame snapshot — exactly as it was found.
///
/// The third of the family, after [`restash_jit_pending_npe`] and
/// [`restash_jit_pending_arithmetic`], and the same rule as both: do NOT
/// re-sample. `active_compiled_frames()` at a restore point describes a
/// shallower stack than the trap did; the frames belong to the trap, not to the
/// drain that happened to pass them through.
pub(crate) fn restash_jit_pending_aioobe(
    index: i64,
    length: i64,
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| {
        s.aioobe.set(Some((index, length)));
        *s.trap_frames.borrow_mut() = frames;
    });
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending-AIOOBE payload. See `stash_jit_pending_exception` for the
/// OSR drain-without-route rationale.
///
/// FLAG ONLY, and that is now a distinction that matters: a door which drained
/// the payload with [`take_jit_pending_aioobe`] left the snapshot where the
/// helper put it and must use this, while a door which drained everything with
/// [`take_all_jit_signals`] is holding the snapshot and must use
/// [`restash_jit_pending_aioobe`] instead.
pub(crate) fn stash_jit_pending_aioobe(index: i64, length: i64) {
    JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
}

/// Take (consume) any pending Java exception set by JIT dispatch.
/// Returns `Some(ObjectRef)` if an exception was pending, `None` otherwise.
///
/// `thread` must be the thread whose JIT call stashed it — see
/// [`take_all_jit_signals`]. The returned `ObjectRef` is the *post-move*
/// address if a collection intervened, because the slot it comes out of is
/// rooted and remapped (`memory/roots.rs` §10, `memory/gc.rs` §10).
pub fn take_jit_pending_exception(thread: &mut JvmThread) -> Option<ObjectRef> {
    let exc = thread.jit_pending_exception.take();
    if exc.is_some() {
        drain_superseded_implicit_signals();
    }
    exc
}

/// Drop the implicit-trap flags when a real exception is being delivered.
///
/// An implicit signal (`npe`, `aioobe`, `arithmetic`) is a REQUEST for a
/// throwable, not a throwable. Once another exception is in flight, that request
/// can never be granted: the frame whose trap raised it is unwinding, and no
/// door downstream owns the flag. Leaving it set is not inert -- the next
/// unrelated JIT call drains it and builds a fresh exception at a site that
/// never faulted.
///
/// The shape that found this (2026-09-03, see
/// `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`):
/// the lambda direct arm finishes a deopted body in the interpreter and returns
/// zero with the real NPE parked in `jit_pending_exception`, exactly as its
/// contract says. Compiled code then evaluates the second operand of the same
/// expression before its post-invoke guard fires, dereferences the SAME null and
/// raises a second trap. The first exception is delivered and caught; the second
/// flag survives two iterations and surfaces as a `NullPointerException` for a
/// receiver that was never null.
///
/// This is the same rule [`take_all_jit_signals`] already applies by taking
/// everything at once -- stated for the other consumption point, so the two
/// cannot disagree about whether a signal outlives the exception that overtook
/// it. The deopt flag is deliberately NOT dropped: it describes the compiled
/// frame's fate, which an exception does not settle.
fn drain_superseded_implicit_signals() {
    let npe = take_jit_pending_npe();
    if npe {
        let _ = take_jit_pending_npe_action();
    }
    let aioobe = take_jit_pending_aioobe().is_some();
    let arithmetic = take_jit_pending_arithmetic();
    // The snapshot was taken for a raise that will never happen; a later drain
    // would attach frames the raising code has long since left. All three
    // signals carry one now (the NPE since 2026-09-02, the div-by-zero and the
    // AIOOBE since 2026-09-05), so all three have to drop it — dropping it only
    // for the NPE would leave a superseded array trap's frames in the slot for
    // whatever raises next.
    if npe || aioobe || arithmetic {
        let _ = take_jit_pending_trap_frames();
    }
}

/// Non-consuming peek: returns `true` if a pending Java exception is set.
///
/// Used by `jit_invoke_dispatch` (and its bail/cache paths) to decide
/// whether to return the `i64::MIN` deopt sentinel — so the JIT caller's
/// post-invoke exception guard fires and the interpreter routes the
/// stashed exception through the method's exception table — instead of
/// returning a bogus `0` that the JIT would keep computing with.
/// Resolves the thread through [`current_jit_thread_ptr`] rather than taking a
/// `&mut JvmThread`, because three of its four call sites sit *before* the
/// `jit_thread_mut()` acquisition in their function and cannot name one. That
/// is sound here and only here: this is a **read**, so it creates no aliasing
/// `&mut`, and every call site runs inside a JIT helper on the thread that
/// installed the pointer. A null pointer (no JIT thread installed — the only
/// way to reach that is from outside JIT dispatch, where nothing can have
/// stashed an exception) reads as "nothing pending", which is the same answer
/// the empty TLS cell used to give.
pub(crate) fn jit_pending_exception_is_set() -> bool {
    let t = current_jit_thread_ptr();
    if t.is_null() {
        return false;
    }
    // SAFETY: `t` is this OS thread's own `JvmThread`, installed by
    // `set_jit_thread` and alive for the whole JIT call; the read is a
    // shared-reference-shaped `Option` load with no aliasing `&mut` created.
    unsafe { (*t).jit_pending_exception.is_some() }
}

/// Take (consume) a pending AIOOBE from JIT bounds check.
/// Returns `Some((index, length))` if an AIOOBE was pending.
pub fn take_jit_pending_aioobe() -> Option<(i64, i64)> {
    JIT_SIGNALS.with(|s| s.aioobe.take())
}

/// Take (consume) a pending `ArithmeticException` ("/ by zero") set by the JIT
/// integer-division zero-divisor guard. Returns `true` if one was pending.
/// Mirrors [`take_jit_pending_aioobe`]; the interpreter's post-JIT drain throws
/// a real `ArithmeticException` through the method's exception table.
pub fn take_jit_pending_arithmetic() -> bool {
    JIT_SIGNALS.with(|s| s.arithmetic.take())
}

/// Put a compiled-frame snapshot back into the signal record, without touching
/// any flag.
///
/// For a door that drained the snapshot to build a throwable it then had to
/// DISCARD: the flag is restored separately (`set_jit_pending_npe_flag_only`),
/// and the frames have to go with it or the next throwable is built from a bare
/// flag and the trace names only the frames the interpreter still holds.
///
/// Assigns the slot, so it must be called at most once per restash — the same
/// rule `handle_compiled_callee_deopt_sentinel`'s three-way restash states, for
/// the same reason: there is ONE `trap_frames` cell and a second call with
/// `None` wipes what the first put back.
pub(crate) fn restash_jit_pending_trap_frames(
    frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) {
    JIT_SIGNALS.with(|s| *s.trap_frames.borrow_mut() = frames);
}

/// Re-stash a previously taken pending-arithmetic flag. Mirrors
/// [`stash_jit_pending_aioobe`] for the OSR drain-without-route path, so a
/// div-by-zero raised in OSR-compiled code with no in-frame handler survives the
/// OSR→interpreter handoff and is surfaced by the next JIT-return drain.
pub(crate) fn stash_jit_pending_arithmetic() {
    JIT_SIGNALS.with(|s| s.arithmetic.set(true));
}

/// Take (consume) a pending NPE from a JIT array helper (`jit_iaload`,
/// `jit_aaload`, `jit_arraylength`). Returns `true` if an NPE was pending.
///
/// The JVM specifies that all three opcodes throw `NullPointerException`
/// when their array reference is null; the JIT array helpers previously
/// swallowed the null silently (returning 0 / -1), which let JIT'd
/// Java code continue with corrupt state. Now they set this flag, return
/// `i64::MIN`, and the interpreter post-JIT path constructs the real
/// `java/lang/NullPointerException` and routes it through the method's
/// exception table — same pattern as `take_jit_pending_aioobe`.
pub fn take_jit_pending_npe() -> bool {
    JIT_SIGNALS.with(|s| s.npe.take())
}

/// Read the JEP-358 action code recorded alongside a pending JIT NPE WITHOUT
/// consuming it.
///
/// For a door that drained the flag with [`take_jit_pending_npe`] and may put
/// it back: it needs the code to build a message, and must not take it, because
/// the drain that eventually services the NPE reads the same cell.
pub(crate) fn peek_jit_pending_npe_action() -> u8 {
    JIT_SIGNALS.with(|s| s.npe_action.get())
}

/// Take (consume) the JEP-358 *action code* recorded alongside a pending JIT
/// NPE (`helpful_npe::jit_action::*`; `0` when none was recorded — e.g. the NPE
/// came from a helper that doesn't carry its operation kind). The interpreter
/// drain calls this right after [`take_jit_pending_npe`] to build the
/// action-only message.
pub fn take_jit_pending_npe_action() -> u8 {
    JIT_SIGNALS.with(|s| s.npe_action.take())
}

/// Internal: set the pending-NPE flag with no action code (the existing bare
/// signal — yields an unmessaged NPE). Resets the action cell to `0` so a stale
/// code from a prior op can never leak onto this NPE.
#[inline]
fn set_jit_pending_npe() {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(0);
    });
    snapshot_trap_frames(0);
}

/// Internal: set the pending-NPE flag *with* a JEP-358 action code
/// (`helpful_npe::jit_action::*`). Called from the array/length helpers that
/// know their operation kind, so the interpreter can attach an action-only
/// JEP-358 message to the JIT-originated NPE.
#[inline]
fn set_jit_pending_npe_action(code: u8) {
    set_jit_pending_npe_action_at(code, 0);
}

/// As [`set_jit_pending_npe_action`], with the id of the inline null-check
/// site that trapped (`0` when the caller has none).
///
/// Only the inline null-check stubs can name a site: they are the shape whose
/// bci is otherwise unrecoverable, because the check is not a GC-capable call
/// and so publishes no safepoint id. Every other NPE-signalling helper is a
/// call, and its frame's slot already holds an id `activation_bci` can use.
#[inline]
fn set_jit_pending_npe_action_at(code: u8, trap_key: u32) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(code);
    });
    snapshot_trap_frames(trap_key);
}

/// The pending NPE of an `athrow` on a null reference in compiled code
/// (`jit_throw_exception`'s null arm; JIT round 20 wave 7, lane fuzzC,
/// correctness, unconditional; probe `R20FuzzCAthrowNull1`).
///
/// The arm used to raise the bare signal ([`set_jit_pending_npe`]: action
/// `NONE`, a snapshot placed by the frame's safepoint-id slot). An `athrow` is
/// not a published safepoint, so that slot holds the frame's LAST call's bci
/// (or the prologue's unset sentinel): the drain could corroborate nothing,
/// the NPE came out with `getMessage() == null` where HotSpot (and this VM's
/// interpreter) says `Cannot throw exception because "<expr>" is null`, and its
/// top frame carried the last call's line instead of the `athrow`'s.
///
/// `bci` is the `athrow`'s own bci, which both x64 tiers pass as a compile-time
/// immediate (the enclosing method's: neither builds an `athrow` inside a
/// spliced body). It goes on the innermost frame as the direct-throw helpers'
/// stamped bci does (`conservative_roots::apply_athrow_trap_bci`, which refuses
/// any frame it cannot vouch for, e.g. an aarch64 artifact, and then keeps the
/// walk's answer), and the [`ATHROW`] action code lets the drain read the
/// opcode there as the one that trapped only when it IS an `athrow`.
///
/// [`ATHROW`]: cratonvm_jit_api::npe_action::ATHROW
fn set_jit_pending_npe_athrow(bci: i64) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action
            .set(crate::runtime::exceptions::helpful_npe::jit_action::ATHROW);
    });
    if !trap_frame_snapshot_enabled() {
        return;
    }
    let mut frames = trap_snapshot_walk();
    crate::jit::conservative_roots::apply_athrow_trap_bci(&mut frames, bci);
    JIT_SIGNALS.with(|s| {
        *s.trap_frames.borrow_mut() = (!frames.is_empty()).then_some(frames);
    });
}

/// Re-stash a previously-taken JIT NPE action code (OSR drain-without-route
/// path, mirroring [`stash_jit_pending_npe`]). Preserves the action so a
/// re-surfaced NPE keeps its JEP-358 message.
pub(crate) fn stash_jit_pending_npe_action(code: u8) {
    set_jit_pending_npe_action(code);
}

/// Set the out-of-band deopt/exception signal (MEDIUM fix: `i64::MIN` sentinel
/// collision). MUST be called on EVERY path that produces the `i64::MIN` deopt
/// sentinel, so the interpreter can tell a genuine deopt/exception apart from a
/// method that legitimately returns `Long.MIN_VALUE`. Idempotent; cheap.
///
/// Crate-public so the out-of-line JIT stubs in `jit/src/x64.rs` can route
/// through a tiny `extern "C"` helper that sets it (see [`jit_set_deopt_pending`]).
#[inline]
pub(crate) fn set_jit_deopt_pending() {
    JIT_SIGNALS.with(|s| {
        s.deopt.set(true);
        // Round 13 wave 8 (lane replay4): not a frameless trap stub unless
        // `jit_uncommon_trap` says so right after this.
        s.frameless_trap.set(FramelessTrapSite::Helper);
    });
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // the deopt trap is the `CompiledUninterruptible -> Deoptimizing` edge —
    // the only tabled way into that state. From here until the interpreter has
    // materialised the frames, the thread holds `FrameValue` buffers that are
    // in neither the compiled frame's oop map nor a not-yet-built interpreter
    // frame, which is exactly what makes the window its own state.
    //
    // The window is closed by whichever comes first: the JIT entry pop
    // (`conservative_roots::leaving_compiled_state`, which resolves
    // `Deoptimizing` to `JavaRunning` rather than manufacturing an untabled
    // edge) or the interpreter's own resume sites.
    thread_state::record_transition(
        ThreadExecState::Deoptimizing,
        "jit::helpers::set_jit_deopt_pending",
    );
}

/// Read+clear the out-of-band deopt/exception signal. The interpreter's
/// post-invoke check calls this when it observes an `i64::MIN` return: `true`
/// means the JIT took the exception/deopt path and the `i64::MIN` is the
/// sentinel (route/re-run); `false` means `i64::MIN` is a real returned value
/// and must be pushed verbatim. See [`set_jit_deopt_pending`].
#[inline]
pub fn take_jit_deopt_pending() -> bool {
    JIT_SIGNALS.with(|s| s.deopt.take())
}

/// `extern "C"` trampoline for the out-of-line deopt/exception stubs emitted in
/// `jit/src/x64.rs`. Those stubs already `CALL` a helper (e.g. `jit_bastore`,
/// `jit_throw_aioobe`, the dispatch helper) that sets one of the pending-exception
/// flags AND `JIT_DEOPT_PENDING` before producing `i64::MIN`. This dedicated,
/// side-effect-free trampoline lets a stub that does NOT otherwise call such a
/// helper (or whose helper predates this flag) set the deopt signal with a
/// single `CALL` and no argument marshalling.
///
/// SAFETY: no pointer arguments; only touches a thread-local. Safe to call from
/// JIT-compiled code at any point before loading the `i64::MIN` sentinel.
pub extern "C" fn jit_set_deopt_pending() {
    set_jit_deopt_pending();
}

/// `i64::MIN`-sentinel disambiguation for `J`/`D` (long/double) call returns —
/// PEEK (non-clearing) of every out-of-band exception/deopt signal.
///
/// A compiled caller signals a callee exception/deopt by the dispatch helpers'
/// `i64::MIN` return in RAX. For `int`/ref/void returns that is unambiguous (no
/// such legitimate value), so the post-invoke check is a plain
/// `CMP RAX, i64::MIN; JE bail`. But a callee that *legitimately* returns
/// `Long.MIN_VALUE` (a `J`/`D` whose bits equal `i64::MIN`) returns the SAME
/// value with NO pending-signal flag set — so a `J`/`D` call site cannot tell the
/// two apart from RAX alone.
///
/// At a `J`/`D` call site the backend therefore emits, ONLY on the (rare)
/// `RAX == i64::MIN` branch, a `CALL` here. We peek (do not clear) every signal a
/// dispatch helper or its callee could have raised before returning `i64::MIN`:
///
///   * `JIT_PENDING_EXCEPTION`  — an explicit/native throwable (athrow, NPE-on
///     -receiver, re-stashed callee exception),
///   * `JIT_PENDING_NPE`        — a void-return-store / null-receiver NPE,
///   * `JIT_PENDING_AIOOBE`     — a bounds-check failure,
///   * `JIT_DEOPT_PENDING`      — the generic out-of-band deopt flag (set by
///     `jit_throw_aioobe` / `jit_uncommon_trap` / the x64 stubs), and
///   * a stashed IR-deopt frame (`cratonvm_jit::deopt::has_last_deopt`) — an
///     IR-path deopt of a dispatched callee returns `i64::MIN` WITHOUT the VM
///     flag (the stash lives in the jit crate).
///
/// Returns `1` iff ANY is pending (the `i64::MIN` is a genuine sentinel — the
/// caller bails through its epilogue, and the interpreter's post-JIT path drains
/// the still-set flag and routes/resumes), `0` iff none is pending (the
/// `i64::MIN` is a real `Long.MIN_VALUE` return — the caller keeps it). The peek
/// is non-destructive so the outer interpreter drain still observes the flag.
///
/// SAFETY: no pointer arguments; only reads this thread's own signal block and
/// `JvmThread`. Safe to call from JIT-compiled code immediately after a
/// dispatch returns `i64::MIN`.
pub extern "C" fn jit_dispatch_threw() -> i64 {
    // The throwable half lives on the `JvmThread` (GC-rooted); the scalar
    // flags stay in TLS. Both are this OS thread's own state, so the peek is
    // still allocation-free and lock-free — it just reads two places instead
    // of one. See `jit_pending_exception_is_set` for why resolving the thread
    // through the raw pointer is sound in a read-only peek.
    // `arithmetic` too (interpreter round i1 wave 4, L7): a `/ by zero` is the
    // one implicit signal this peek did not read. It is raised beside the deopt
    // flag, but a door that restores it flag-only after a nested door has
    // drained everything (`route_implicit_exc_through_callee`'s
    // `restash_and_return`) hands back a `J`/`D` sentinel with the arithmetic
    // flag alone, which read as a genuine `Long.MIN_VALUE` and lost the throw.
    let pending = (jit_pending_exception_is_set()
        || JIT_SIGNALS.with(|s| {
            s.npe.get() || s.aioobe.get().is_some() || s.arithmetic.get() || s.deopt.get()
        }))
        || cratonvm_jit::deopt::has_last_deopt();
    if pending {
        1
    } else {
        0
    }
}

/// JEP 358 (helpful NPE), inline-codegen path — `extern "C"` trampoline for the
/// per-action inline null-check failure stubs emitted in
/// `jit/src/x64.rs::emit_null_check_store_stubs`.
///
/// Each inline array load/store/`arraylength` null-check site is grouped by its
/// JEP-358 [`cratonvm_jit_api::npe_action`] code, and the per-action stub passes
/// that code here. We set the pending-NPE flag *with* the code (so the
/// interpreter's post-JIT NPE drain attaches the right action-only message —
/// "Cannot load from int array", …, gated behind
/// `-XX:+ShowCodeDetailsInExceptionMessages`) and the out-of-band deopt signal
/// (the stub loads `i64::MIN` as the method return value, so the interpreter
/// must not mistake it for a real `Long.MIN_VALUE`).
///
/// This replaces the old single shared stub, which called `jit_bastore(0)` and
/// therefore fabricated `ASTORE_BYTE` ("Cannot store to byte array") for *every*
/// inline array opcode regardless of its real element type or load/store
/// direction. `code == NONE` (0) sets a bare (unmessaged) NPE — the same shape
/// as today's default path — which is what the non-array intrinsic null-check
/// sites use.
///
/// SAFETY: no pointer arguments; only touches thread-locals. Safe to call from
/// JIT-compiled code at any point before loading the `i64::MIN` sentinel. The
/// `code` is truncated to `u8`; an out-of-range value maps to no message
/// (`jit_action_message` returns `None`), never a panic.
pub extern "C" fn jit_npe_with_action(code: i64) {
    let f = || jit_npe_with_action_body(code);
    contain("jit_npe_with_action", OnPanic::Deopt, (), f)
}

/// Body of [`jit_npe_with_action`], run under its panic guard.
fn jit_npe_with_action_body(code: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache,
    // matching every other array/field helper's entry (the next GC must
    // re-scan after we deopt back out to the interpreter).
    crate::jit::conservative_roots::note_jit_boundary();
    // The argument is PACKED: the low byte is the JEP-358 action code, and the
    // upper 24 bits are the inline null-check site id, or zero. A stub that
    // sets only the action (the historical shape, and the reason-10 precise
    // putfield path) therefore reads back as `trap_key == 0` with no special
    // case. See `x64::inlining::NpeTrapSite` and
    // `x64::Compiler::emit_null_check_store_stubs`.
    //
    // `as u64` before the shift, not `as u32`: the sign of an `i64` argument is
    // nobody's business here and an arithmetic shift on a hypothetical negative
    // would fabricate a key.
    let packed = code as u64;
    // Truncation: the low byte IS the action code by construction.
    let action = (packed & 0xff) as u8;
    // Truncation: `record_npe_trap_site` caps ids at 24 bits.
    let trap_key = ((packed >> 8) & 0x00ff_ffff) as u32;
    // nio21 wave 17 L5: a single-pass pad whose local-handler probe follows
    // stamped its bci (`jit_set_throw_bci`, the call just before this one)
    // with the defer bit. Strip it, and leave the frame snapshot to the probe.
    let (stamped, deferred) = cratonvm_jit::implicit_null::split_trap_snapshot_defer_bit(
        JIT_SIGNALS.with(|s| s.athrow_bci.get()),
    );
    if deferred {
        JIT_SIGNALS.with(|s| {
            s.athrow_bci.set(stamped);
            s.npe.set(true);
            s.npe_action.set(action);
        });
        defer_trap_snapshot(DeferredTrapSnapshot::NpeKey(trap_key));
    } else {
        set_jit_pending_npe_action_at(action, trap_key);
    }
    // Out-of-band deopt signal: the stub loads `i64::MIN` as the return value
    // (same invariant as `jit_bastore`'s null arm did before).
    set_jit_deopt_pending();
}

/// Obtain an exclusive reference to the JIT thread. Returns `None` if not set,
/// otherwise the `&mut JvmThread` paired with a [`JitThreadGuard`] RAII token.
///
/// The caller MUST keep the guard alive for as long as it uses the returned
/// reference (binding it to `_guard` is sufficient). When the guard drops it
/// clears the debug reentrancy flag; a nested/aliasing `jit_thread_mut` call
/// made while a prior guard is still live trips a `debug_assert!`.
///
/// # Safety
/// Caller must ensure this is only called from JIT helper functions on the same
/// thread that called `set_jit_thread`, and that no other reference to the
/// JvmThread is live.
// SAFETY: Caller must ensure this is only called from JIT helper functions on the
// same thread that called `set_jit_thread`, and that no other `&mut JvmThread`
// reference is live. The pointer was set by `set_jit_thread` from a valid `&mut JvmThread`.
// The `JIT_THREAD_BORROWED` flag + `JitThreadGuard` enforce the "no aliasing
// borrow" half of this invariant in debug builds; release builds are unaffected.
#[inline]
// SAFETY: invoked on the thread that installed `JIT_THREAD`, so no other
// `&mut JvmThread` is live; the returned exclusive borrow is unique for the
// guard's lifetime.
unsafe fn jit_thread_mut() -> Option<(&'static mut JvmThread, JitThreadGuard)> {
    let ptr = JIT_THREAD.with(|t| t.get());
    if ptr.is_null() {
        None
    } else {
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROWED.with(|b| {
            // This now fires ONLY for a genuine *same-level* aliasing
            // fabrication: two live `jit_thread_mut` borrows that did NOT cross
            // a `set_jit_thread` re-entry boundary. The common case — an outer
            // `jit_invoke_dispatch` bailing into the interpreter, which
            // re-enters JIT and recurses into a second `jit_invoke_dispatch` —
            // is a *strictly nested* reborrow (the inner `&mut *ptr` descends
            // from the outer's `&mut` via the `set_jit_thread(thread)` cast),
            // and `set_jit_thread`/`restore_jit_thread` suspend+restore this
            // flag around that boundary so the legitimate nesting does NOT trip
            // here. (Empirically verified: DaCapo avrora drives ~1100 such
            // nested borrows and completes cleanly with no UB.)
            if b.get() {
                // Name BOTH sides before tripping. The assertion's own message
                // asserts this is "a genuine sibling fabrication"; that claim
                // is only checkable if the prior borrow's site is printed
                // next to this one's.
                let prior = JIT_THREAD_BORROW_SITE.with(|s| s.take());
                match prior {
                    Some(bt) => eprintln!(
                        "[jit-borrow] PRIOR borrow was taken here:\n{bt}\n\
                         [jit-borrow] SECOND borrow is being taken here:\n{}",
                        std::backtrace::Backtrace::force_capture()
                    ),
                    None => eprintln!(
                        "[jit-borrow] aliasing borrow detected; re-run with \
                         CRATONVM_DBG_JIT_BORROW_SITES=1 to see both sites"
                    ),
                }
            }
            debug_assert!(
                !b.get(),
                "jit_thread_mut: aliasing &mut JvmThread borrow detected \
                 (a prior JitThreadGuard is still live at the SAME JIT nesting \
                 level — this is a genuine sibling fabrication, not a re-entry)"
            );
            b.set(true);
            if jit_borrow_site_capture_enabled() {
                JIT_THREAD_BORROW_SITE
                    .with(|s| s.set(Some(Box::new(std::backtrace::Backtrace::force_capture()))));
            }
        });
        Some((
            &mut *ptr,
            JitThreadGuard {
                #[cfg(debug_assertions)]
                _private: (),
            },
        ))
    }
}

/// Attempt to call a JIT-compiled entry through the transmute tables.
/// Returns `Some(rc)` on success, `None` when the arg count exceeds the
/// table coverage (16 ABI words since round 12 wave 2, context included --
/// `try_call_compiled_entry_wide`; 8 under its kill switch: the tables were
/// widened past the register file, and the platform C ABI puts the overflow
/// on the stack exactly where a compiled prologue reads it). The
/// caller is expected to fall through to the interpreter slow-path in
/// `jit_invoke_dispatch` (which decodes args into `Value`s and bails)
/// rather than returning the previous silent `0` — that was the round-5
/// MED follow-up to the round-4 wave-2 fix for the helper dispatch path.
///
/// CRIT (round-5 review): three sibling sites in `jit_invoke_dispatch`
/// (the thread-local DISPATCH_CACHE fast-path, the JIT cache fast-path,
/// and the post-compile fast-path) each carried `_ => 0` arms that
/// silently dropped 5+-arg callees. Sharing this helper keeps the
/// register-table dispatch in one place and removes those drop sites.
///
/// SAFETY: `entry` must be a live JIT-compiled extern "C" entry point
/// whose calling convention matches `needs_ctx` (with-ctx prepends an
/// `i64` VM pointer to the Java arg slots). `args_slice` must contain
/// exactly `args_slice.len()` valid i64 arg slots; on overflow we don't
/// dereference the table at all.
#[inline]
// SAFETY: `entry` is a live JIT-compiled `extern "C"` code pointer produced by
// the compiler; it is transmuted to a fn signature matching the (with/without
// NativeContext) arity actually called below.
unsafe fn try_call_compiled_entry(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    // The compiled callee's prologue publishes ITS rbp into the precise-maps
    // innermost-RBP mirror; nothing on this Rust path republishes the JIT
    // caller's rbp after the callee returns, so a later GC would walk the
    // dead callee frame (see `top_rbp_mirror_write`). Snapshot + restore the
    // mirror around the raw entry call.
    //
    // Both halves or neither: the callee's prologue publishes its IDENTITY
    // beside its rbp, so restoring only the rbp would leave the mirrors naming
    // two different frames — the caller's rbp with the callee's identity — and
    // nothing downstream can detect that, because both halves would still read
    // consistently out of the mirrors. See `top_cm_id_mirror_read`.
    let saved_top_rbp = crate::jit::conservative_roots::top_rbp_mirror_read();
    let saved_cm_id = crate::jit::conservative_roots::top_cm_id_mirror_read();
    let r = try_call_compiled_entry_inner(entry, needs_ctx, vm_ptr, args_slice);
    crate::jit::conservative_roots::top_rbp_mirror_write(saved_top_rbp);
    crate::jit::conservative_roots::top_cm_id_mirror_write(saved_cm_id);
    r
}

// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
unsafe fn try_call_compiled_entry_inner(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let n = args_slice.len();
    // Past the eight-word tables below: see `try_call_compiled_entry_wide`.
    if n + usize::from(needs_ctx) > HELPER_ENTRY_TABLE_WORDS {
        return try_call_compiled_entry_wide(entry, needs_ctx, vm_ptr, args_slice);
    }
    if needs_ctx {
        Some(match n {
            0 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry (caller contract);
                // the 0-arg with-ctx callee takes exactly (vm_ptr) per the JIT ABI.
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr)
            }
            1 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 1-arg with-ctx
                // callee takes (vm_ptr, arg0) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0])
            }
            2 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 2-arg with-ctx
                // callee takes (vm_ptr, arg0, arg1) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1])
            }
            3 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 3-arg with-ctx
                // callee takes (vm_ptr, arg0, arg1, arg2) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                )
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                )
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                )
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                )
            }
            _ => return None,
        })
    } else {
        Some(match n {
            0 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry (caller contract);
                // the 0-arg no-ctx callee takes no arguments per the JIT ABI.
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                f()
            }
            1 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 1-arg no-ctx
                // callee takes (arg0) per the JIT ABI.
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0])
            }
            2 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 2-arg no-ctx
                // callee takes (arg0, arg1) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1])
            }
            3 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 3-arg no-ctx
                // callee takes (arg0, arg1, arg2) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 4-arg no-ctx
                // callee takes (arg0, arg1, arg2, arg3) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                )
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                )
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                )
            }
            8 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                    args_slice[7],
                )
            }
            _ => return None,
        })
    }
}

/// ABI words (the context pointer included) the per-arity arms of
/// [`try_call_compiled_entry_inner`] cover: 8 with no context, 7 Java
/// arguments plus the context.
const HELPER_ENTRY_TABLE_WORDS: usize = 8;

/// ABI words [`try_call_compiled_entry_wide`] passes: every compiled-entry
/// call from a helper whose context pointer plus Java arguments fit in it.
const HELPER_ENTRY_MAX_WORDS: usize = 16;

/// `CRATONVM_JIT_HELPER_WIDE_ENTRY_CALLS` (default on; `0` restores the
/// eight-word cap, so a wider compiled callee reached through a helper runs
/// interpreted as before round 12 wave 2). Cached once per process: it only
/// selects between two correct routes.
pub(crate) fn helper_wide_entry_calls_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HELPER_WIDE_ENTRY_CALLS")
    })
}

/// `CRATONVM_JIT_DISPATCH_VIRTUAL_MEMO_FIRST` (default on; `0` restores the
/// order before round 12 wave 2): `jit_invoke_dispatch`'s virtual arm consults
/// its `(site, receiver class)` compiled-entry memo before resolving the
/// dispatch target, instead of after. Cached once per process.
fn dispatch_virtual_memo_first_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DISPATCH_VIRTUAL_MEMO_FIRST")
    })
}

/// A compiled-entry call wider than the per-arity tables (round 12 wave 2,
/// lane `calls`): 9..=[`HELPER_ENTRY_MAX_WORDS`] ABI words.
///
/// Before this, every such call answered `None`, and each caller's fallback
/// ended in the interpreter: the MIC-hit arm bailed by name, the megamorphic
/// arm fell through to the resolving arm (a compile probe per call) and then
/// `invoke_or_native`, whose door refuses more than eight words too
/// (`jit_bridge::jit_abi_max_java_params`), so the compiled callee's
/// BYTECODE ran. On Win64 that is any instance callee with seven or more
/// arguments that takes the context (four ABI registers), which is how
/// `R12CallsMicMissCompiled` (receiver + 7) measured 19x HotSpot.
///
/// The call pads the arguments with zeros to one fixed sixteen-word
/// signature. Both x86-64 C ABIs are caller-cleanup and pass the words past
/// the register file on the caller's stack in order (Win64 above the 32-byte
/// home area, SysV at the top), which is exactly where every compiled prologue
/// reads its stack arguments (`x64::frames` and `ir_lower`'s
/// `STACK_ARG_BASE`); a callee reads only the words it declares, so the
/// padding is never observed. The per-arity arms above already rely on the
/// same layout for their stack words.
#[inline(never)]
// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
unsafe fn try_call_compiled_entry_wide(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let lead = usize::from(needs_ctx);
    // x86-64 only: the layout argument above is the two x86-64 C ABIs' and
    // the x86-64 prologues'; another backend keeps the interpreter fallback.
    if !cfg!(target_arch = "x86_64")
        || args_slice.len() + lead > HELPER_ENTRY_MAX_WORDS
        || !helper_wide_entry_calls_enabled()
    {
        return None;
    }
    let mut w = [0i64; HELPER_ENTRY_MAX_WORDS];
    if needs_ctx {
        w[0] = vm_ptr;
    }
    for (slot, &arg) in w[lead..].iter_mut().zip(args_slice) {
        *slot = arg;
    }
    type Wide = unsafe extern "C" fn(
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) -> i64;
    // SAFETY: `entry` is a live compiled entry (caller contract) whose
    // prologue reads `args_slice.len() + lead` words in the C ABI's layout;
    // the trailing zero words sit past them and are never read.
    let f: Wide = std::mem::transmute(entry);
    Some(f(
        w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7], w[8], w[9], w[10], w[11], w[12], w[13],
        w[14], w[15],
    ))
}

/// Call a compiled entry from a JIT dispatch helper while suspending the
/// debug borrow tracker for the outer JIT frame.
///
/// A compiled callee can immediately re-enter JIT dispatch and borrow the
/// same `JvmThread`. That nested borrow is a child reborrow of the outer JIT
/// frame, but the direct fast paths do not go through `set_jit_thread`, so the
/// debug tracker must be suspended explicitly around the raw call.
#[inline]
// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
unsafe fn try_call_compiled_entry_reentrant(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    note_cached_entry_arm(&CACHED_ENTRY_REGISTRY_PINS);
    // This helper is itself called from compiled dispatch code.  Its raw ABI
    // call used to enter the nested compiled method without registering a
    // `JitEntryGuard`, so a GC triggered by that callee found a JIT return
    // address above an empty guard chain.  The moving-young collector then
    // had to fall back to conservative/non-moving collection and retained
    // the repeated Hibernate bootstrap graphs until OOM.  Resolve the entry
    // back to its live CompiledMethod and register the precise frame for the
    // full duration of the nested call.
    // Pin, don't peek. This is the ONE path into compiled code that used to hold
    // no owning reference to the body it entered: it resolved a bare `cm_ptr`
    // out of the code-range registry and dereferenced it, on the argument that
    // "the registry owns this CompiledMethod while its entry remains callable".
    // The registry stores a raw address, not an `Arc`, so it owns nothing — and
    // the caller's keep-alive here is a *thread-local dispatch cache entry*
    // (`try_mic_rust_cached_entry` reads `entry`/`needs_context` out of the map
    // and calls the raw pointer). A nested dispatch from the callee re-enters
    // `flush_raw_entry_dispatch_caches`, whose `clear()` drops exactly that
    // entry — measured releasing a published body at `active_jit_executions`
    // = 1 on `BasicErrorControllerIntegrationTests`.
    //
    // Holding the pin across the call is what makes the process-wide invariant
    // true: **a thread inside a compiled body always holds an owning reference
    // to it**, so a reference count reaching zero is itself a proof that no
    // thread is inside. One atomic increment (the registry carries a `Weak`).
    let pinned = match cratonvm_jit::pin_jit_code_range_owner_checked(entry) {
        cratonvm_jit::JitEntryPin::Live(owner) => Some(owner),
        cratonvm_jit::JitEntryPin::NotJit => None,
        // Round 14 wave 1 (lane codecache,
        // `r14w1-codecache-unpinned-raw-entry-call-patch-FIXED-20260929.md`): compiled
        // code whose owner is gone is never entered. `None` sends every caller
        // to its interpreter bail, as a call too wide for the tables does.
        // `CRATONVM_JIT_REFUSE_DEAD_ENTRY=0` calls it anyway (read only here).
        cratonvm_jit::JitEntryPin::Dead => {
            if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_REFUSE_DEAD_ENTRY") {
                return None;
            }
            None
        }
    };
    call_compiled_entry_under_owner(pinned.as_deref(), entry, needs_ctx, vm_ptr, args_slice)
}

/// [`try_call_compiled_entry_reentrant`] for a caller that ALREADY holds an
/// owning reference to the callee's artifact.
///
/// The registry round trip the other form performs — an `ArcSwap` load, a binary
/// search over every registered code range, a `Weak::upgrade` CAS and the
/// matching `Arc` drop — re-derives an `Arc<CompiledMethod>` the thread-local
/// dispatch caches already hold in their [`cratonvm_jit::RetainedCode`] field.
/// A `RetainedCode` clone is one relaxed increment and answers the same
/// question, so the documented invariant is unchanged: **a thread inside a
/// compiled body always holds an owning reference to it**, which is what
/// `RetainedCode`'s own `Drop` relies on when it declines to route a
/// provably-not-last release through the retirement queue.
///
/// It is a clone and not a borrow because the map cannot be kept borrowed across
/// the call: a nested dispatch from the callee re-enters
/// `flush_raw_entry_dispatch_caches`, which takes `borrow_mut` on the same
/// thread-local — and, more to the point, may evict this very entry. The clone
/// is what survives that.
///
/// `CRATONVM_JIT='-cached-entry-owner-reuse'` sends these callers back through
/// the registry so one binary can be A/B'd against itself, and the two counters
/// this bumps say which arm ran.
#[inline]
// SAFETY: same entry ABI contract as `try_call_compiled_entry_reentrant`.
unsafe fn try_call_compiled_entry_reentrant_owned(
    owner: &cratonvm_jit::RetainedCode,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    if !cached_entry_owner_reuse_enabled() {
        return try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice);
    }
    note_cached_entry_arm(&CACHED_ENTRY_OWNER_REUSE_HITS);
    call_compiled_entry_under_owner(Some(&**owner), entry, needs_ctx, vm_ptr, args_slice)
}

/// `CRATONVM_JIT='-cached-entry-owner-reuse'` — make every cached compiled
/// dispatch re-resolve its keep-alive through the code-range registry, as it did
/// before 2026-08-17. Default ON.
fn cached_entry_owner_reuse_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// How often a site whose callee the compiler declined re-asks. See
/// [`MIC_COMPILE_DECLINED`].
const MIC_COMPILE_PROBE_RETRY: u32 = 1024;

/// Compile probes skipped because [`MIC_COMPILE_DECLINED`] already held a
/// refusal for this `(site, receiver class)`.
///
/// Counted separately from `not_probed_*`/`probe_returned_none` deliberately:
/// the comment on those says why a skip that lands in a "the compiler refused"
/// counter makes the number unreadable.
pub static MIC_COMPILE_PROBE_MEMO_SKIPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Cached dispatches that reused the owner their cache entry already held.
pub static CACHED_ENTRY_OWNER_REUSE_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Cached dispatches that resolved the owner through the code-range registry.
pub static CACHED_ENTRY_REGISTRY_PINS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Count one arm of the cached-dispatch split, **behind the same gate as every
/// other counter in this file**.
///
/// The gate is not tidiness. The reuse arm runs 111 564 628 times in a 55 s
/// netty run, and an ungated `fetch_add` there is one contended cache line
/// shared by every thread in the VM — an instrument that would have been a
/// variable of the very comparison it exists to settle, present on one arm of
/// the A/B and absent from the other.
#[inline]
fn note_cached_entry_arm(counter: &'static std::sync::atomic::AtomicU64) {
    if mic_prof::enabled() {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The body both forms share: register the precise frame for the nested call,
/// correct a lying ABI flag against the artifact's own, and make the raw call.
#[inline]
// SAFETY: `entry` must be a live JIT entry owned by `compiled` when it is
// `Some`; `args_slice` must match the callee's ABI.
unsafe fn call_compiled_entry_under_owner(
    compiled: Option<&cratonvm_jit::CompiledMethod>,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let mut needs_ctx = needs_ctx;
    // An `if let` in THIS function's body, not `compiled.map(|..| ..)`: the
    // guard records `current_stack_pointer()` as the band's `entry_sp` /
    // `frame_base`, and inside a closure that is the sp of a frame that has
    // already RETURNED by the time `try_call_compiled_entry` pushes the callee.
    // Whenever the closure is not inlined, the callee's frame can then sit
    // ABOVE the recorded base, failing the scanners' `exact_rbp < frame_base`
    // test, and the fallback band `[scanner_sp, entry_sp)` does not cover it
    // (round 11, lane `rt`; the `JitEntryGuard` doc warns against exactly
    // this). `enter_with_compiled` and `enter_with_compiled_at` are both
    // `inline(always)`, so the sp below is this frame's own, as at the
    // method-entry door.
    let jit_root_guard = if let Some(compiled) = compiled {
        // cceres2 (WildFly SIGSEGV cores SF2/SF3/SM): the caller-supplied ABI
        // flag can come from a cache whose (entry, needs_context) pair was
        // read non-atomically across a concurrent inline-cache retarget or
        // C1->C2 supersede. Marshalling with the wrong flag shifts every
        // argument by one inside the callee (its `this` reads leftover
        // register junk — the receiver=0x2d0-style PIC-dispatch SIGSEGV).
        // The resolved CompiledMethod is the entry's OWN record; its flag is
        // definitive. Override and count/log the mismatch so the lying cache
        // can be identified.
        let own = compiled.needs_context();
        if own != needs_ctx {
            static MISMATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = MISMATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 16 || n.is_power_of_two() {
                tracing::warn!(
                    "JIT ABI flag mismatch #{n}: cached needs_ctx={} but compiled entry 0x{:x} \
                     says {} — using the compiled method's own flag",
                    needs_ctx,
                    entry,
                    own,
                );
            }
            needs_ctx = own;
        }
        Some(crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled(compiled))
    } else {
        None
    };
    // Entry hygiene, as `run_jit_body` and `try_osr` apply it: a compiled caller
    // that is issuing a call is not in an exceptional state, so a deopt flag
    // pending now cannot describe the callee about to run. Retiring it makes
    // the flag PER-CALL FRESH, which is what lets
    // `route_implicit_exc_through_callee` read "sentinel + flag + nothing
    // stashed" as this callee's own frameless trap and service it, instead of
    // mistaking a stale flag beside a genuine `Long.MIN_VALUE` for one. One
    // thread-local store.
    let _ = take_jit_deopt_pending();
    #[cfg(debug_assertions)]
    let borrow = suspend_jit_borrow();
    let result = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice);
    #[cfg(debug_assertions)]
    restore_jit_borrow(borrow);
    drop(jit_root_guard);
    // AFTER the guard: the caller's owning reference is what keeps the body
    // mapped for the whole call, so it must outlive both the call and the chain
    // entry naming it. Both callers hold theirs across this return.
    result
}

/// The constant-pool owner of a bridged `invokespecial` (`bail_to_interpreter`,
/// `--jdk-only`) through `caller`'s loader. For an isolated URL loader's
/// caller (`constants::class_entry_record_applies`) through the caller's
/// `CONSTANT_Class` entry naming `name` (its record first, a fresh resolution
/// recorded): `resolve_class_loader_aware` ignores such a loader's memo, so
/// every bridged call asked its `loadClass` again, where HotSpot asks once per
/// entry. `JitInvokeInfo` carries no constant-pool index, so the entry is the
/// caller's first `CONSTANT_Class` naming `name` (a pool scan, on this slow
/// path and for such a caller only); a substituted name the pool does not
/// hold resolves as before. Every other caller resolves exactly as before.
/// Round 15 wave 5, lane classsite2
/// (`r15w5-classsite-class-entry-shared-by-every-instruction-patch-FIXED-20260930.md`,
/// part D).
///
/// JIT round 16, lane classsite (proposal CS2-2): the other bridged owners
/// that re-resolved `JitInvokeInfo::class_name` by name -- the dispatch
/// tail's `invokespecial`, its cached-bytecode-callee door and the
/// `invokestatic` owner override -- come through here too, as
/// [`jit_bridged_owner_entry`], behind `CRATONVM_JIT_CLASS_ENTRY_DOORS`.
/// The `JitInvokeInfo` field the proposal named was not added: the struct
/// has ~180 literals across the jit and vm crates, and the pool search is
/// paid on a slow path by an isolated loader's caller only.
fn jit_bridged_special_owner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    name: &str,
) -> Option<ClassId> {
    jit_bridged_owner(vm, thread, caller, name, false)
}

/// [`jit_bridged_special_owner`] for the bridged owners round 16 routed
/// through the entry (`CRATONVM_JIT_CLASS_ENTRY_DOORS`, default on; off,
/// they resolve by name as before).
fn jit_bridged_owner_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    name: &str,
) -> Option<ClassId> {
    jit_bridged_owner(vm, thread, caller, name, true)
}

/// The body of [`jit_bridged_special_owner`] / [`jit_bridged_owner_entry`];
/// `switched` asks the round-16 switch past the (cheaper) record predicate.
fn jit_bridged_owner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    name: &str,
    switched: bool,
) -> Option<ClassId> {
    if crate::runtime::interpreter::constants::class_entry_record_applies(vm, thread, caller)
        && (!switched || crate::runtime::interpreter::jit_bridge::jit_class_entry_doors_enabled())
    {
        // Before the pool read: the record is filed only while no
        // redefinition of `caller` may have replaced that pool.
        let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let entry = {
            let cm = vm.classes.class_manager.read();
            cm.get_class(caller).and_then(|class| {
                let pool = &class.constant_pool;
                let len = u16::try_from(pool.len()).unwrap_or(u16::MAX);
                (1..len).find(|&index| pool.get_class_name(index) == Some(name))
            })
        };
        if let Some(cp_index) = entry {
            return crate::runtime::interpreter::constants::resolve_class_entry_recorded(
                vm,
                thread,
                (caller, cp_index),
                name,
                fill_as_of,
            )
            .ok();
        }
    }
    crate::runtime::interpreter::resolve_class_loader_aware(vm, thread, caller, name).ok()
}

/// Bail a JIT-dispatched call out to the interpreter when the compiled
/// callee has more arguments than `call_jit_compiled_method_entry`'s
/// register-arg dispatch tables can pass. Issues `invoke_or_native` with
/// the full `Value` argument vector and converts the result back to the
/// `i64` register-ABI return value expected by the JIT caller. Exceptions
/// are stashed via `handle_jit_dispatch_error` so the interpreter post-JIT
/// path can route them through the caller's exception table.
#[inline(never)]
// SAFETY: called from a JIT helper with a live `SharedVm` and the current
// `JvmThread` set up, so resuming interpretation / routing through the caller's
// exception table operates on valid VM state.
unsafe fn bail_to_interpreter(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args: &[Value],
) -> i64 {
    // This bail resolves the callee from a class NAME, and a lambda-proxy
    // receiver has no name of its own: it resolves as its functional interface,
    // which for four of them runs a collector native in place of the lambda
    // body. Same hazard as the dispatch slow path — see
    // `try_lambda_proxy_sam_dispatch`, which declines everything that is not a
    // proxy's SAM, so the routing below is unchanged for every other call.
    crate::vm::dbg_dispatch_tally(
        "jit_bail_to_interpreter",
        info.class_name,
        info.method_name,
        info.descriptor,
    );
    if let Some(result) = try_lambda_proxy_sam_dispatch(vm, thread, info, args) {
        return match result {
            Ok(Some(Value::Int(v))) => v as i64,
            Ok(Some(Value::Long(v))) => v,
            Ok(Some(Value::Float(f))) => f.to_bits() as i64,
            Ok(Some(Value::Double(d))) => d.to_bits() as i64,
            Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
            Ok(Some(Value::Object(None)) | None) => 0,
            Ok(_) => 0,
            Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
        };
    }
    // invokespecial (kind=1) must NOT virtually re-target onto the receiver's
    // runtime class — same rationale as the kind=1 arm of the dispatch slow
    // path below. `invoke_or_native` → `invoke_on_class_shared` applies the
    // abstract→receiver retarget, which turns a compiled super-call bridge
    // into a self-call loop: H2's `ValueVarchar.compareTypeSafe` is
    // `invokespecial ValueStringBase.compareTypeSafe` (4 args + ctx, so
    // `try_call_compiled_entry`'s register table always bails it here);
    // retargeting the abstract `ValueStringBase` back to the `ValueVarchar`
    // receiver re-enters the bridge → dispatch → bail → ∞, surfacing as the
    // GROUP BY `StackOverflowError` from the JIT depth guard (every H2
    // TreeMap<ValueRow> comparator walk dies). `invoke_special_shared`
    // preserves super-call semantics with native-override priority.
    //
    // JIT round 21 wave 14, lane hidden-static: a hidden caller's call of its
    // own method is dispatched on the caller by id, in either mode (the
    // by-name doors would load the class path's copy of the name).
    let hidden_self = if matches!(info.invoke_kind, 1 | 3) {
        jit_hidden_self_owner(vm, ClassId::new(info.declaring_class_id), info.class_name)
    } else {
        None
    };
    let res = if info.invoke_kind == 1 {
        // A substituted owner by its recorded id, as the dispatch arm does.
        // Otherwise, under `--jdk-only`, the constant-pool class through the
        // CALLER's loader, as the dispatch arm's tail does
        // (BUG-JIT-INVOKESPECIAL-LOADER-20260726): the by-name
        // `invoke_special_shared` binds whichever copy the global map holds.
        // `--compatible` keeps the by-name route here (interpreter round i1
        // wave 38, lane L6, review item 4).
        let mut owner = jit_substituted_owner(vm, info).or(hidden_self);
        if owner.is_none() && info.declaring_class_id != 0 && vm.config.is_jdk_only() {
            owner = jit_bridged_special_owner(
                vm,
                thread,
                ClassId::new(info.declaring_class_id),
                info.class_name,
            );
        }
        match owner {
            Some(owner) => crate::vm::invoke_special_shared_on_class(
                vm,
                thread,
                owner,
                info.class_name,
                info.method_name,
                info.descriptor,
                args,
            ),
            None => crate::vm::invoke_special_shared(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                args,
            ),
        }
    } else if matches!(info.invoke_kind, 0 | 2) {
        // VIRTUAL DISPATCH FIX (bug #2 — regex zero-width corruption under
        // `CRATONVM_JIT_VIRTUAL_TIERUP`). For virtual/interface kinds,
        // `info.class_name` is the *static* call-site type, not the receiver's
        // runtime class. `invoke_or_native` resolves the callee against the
        // class name it is handed (it does NOT re-dispatch on `args[0]`), so a
        // static type that declares a *concrete* base method shadows the
        // receiver's override. The regex engine is the canonical victim: the
        // `root.match(...)` call site is statically typed `Pattern$Node`, whose
        // base `match` is an unconditional zero-width "accept" (`matcher.last =
        // i; return true`). Bailing on the static type ran that accept node at
        // every position instead of `Pattern$Start.match`, so `find()` reported
        // a zero-width match before every character and
        // `"org.x".replaceAll("[.]","/")` produced "/o/r/g/...". The compiled-
        // entry MIC path already resolves on the receiver class (see
        // `jit_invoke_virtual_mic`); this register-overflow / exception-reroute
        // bail must do the same. Mirrors the array→Object and synthetic-id
        // fallbacks used there.
        let dispatch_class = virtual_dispatch_class(vm, args, info);
        crate::vm::invoke_or_native(
            vm,
            thread,
            &dispatch_class,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if let (3, Some(owner)) = (info.invoke_kind, hidden_self) {
        // A hidden caller's call of its own static method (see `hidden_self`),
        // on the caller by id (5.4.3.3 lookup and 5.5 initialization of the
        // declaring class), as the not-entrant static arm and the dispatch
        // tail's owner override do.
        crate::vm::invoke_static_shared_on_class(
            vm,
            thread,
            owner,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if let Some(owner) = jit_hidden_static_owner(vm, info) {
        // JIT round 21 lane reflect4 (correctness, unconditional): the
        // hidden-owner leaf's class is found by its id only; by name this
        // re-run would look for a spun `LambdaForm$MH` no loader answers.
        crate::vm::invoke_static_shared_on_class(
            vm,
            thread,
            owner,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if info.invoke_kind == 3 {
        // The static entry: JVMS §5.5 before a registered static native where
        // `static_by_name_init_enforced` holds; `--compatible` is censused
        // under door `jit_bail_static` until it is flipped.
        crate::vm::invoke_static_or_native(
            vm,
            thread,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
            "jit_bail_static",
        )
    } else {
        crate::vm::invoke_or_native(
            vm,
            thread,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    };
    match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    }
}

/// Dispatch a SAM call on a lambda-proxy receiver through the proxy registry.
/// `None` means "not that" — not a virtual/interface call, not a lambda-proxy
/// receiver, or not the proxy's SAM — and the caller proceeds unchanged.
///
/// Every JIT path that resolves a callee BY NAME needs this in front of it. A
/// lambda proxy's class id is synthetic and absent from the class store, so
/// name resolution falls back to the constant-pool class — the FUNCTIONAL
/// INTERFACE — and CratonVM registers natives on four of those
/// (`java/util/function/Supplier.get`, `BiConsumer.accept`, `Function.apply`,
/// `BinaryOperator.apply`) to serve the synthetic objects
/// `Collector.accumulator()` and its siblings hand out. Those natives cannot
/// decline: with no collector tag in field 0 they take their catch-all arm, so
/// an ordinary user lambda routed to them has its body skipped entirely —
/// `Supplier.get` returns an empty `ArrayList`, `Function.apply` returns its
/// argument, `BiConsumer.accept(a, b)` calls `a.add(b)`, and
/// `BinaryOperator.apply(a, b)` calls `a.addAll(b)`. Three of those four are
/// SILENT.
///
/// The interpreter never had the problem — `try_lambda_dispatch` runs before
/// native resolution there — so the failure is JIT-only, and it additionally
/// needs the call site to be megamorphic, since a site that only ever sees one
/// proxy class stays on a cached path. That combination is why it took
/// Hibernate's `forEachSubInitializer(BiConsumer, ..)` — invoked with
/// `Initializer::startLoading`, `::resolveKey` and `::initializeInstance` — to
/// surface it, as `NoSuchMethodError: <sub-initializer>.add(Ljava/lang/Object;)Z`,
/// three failures in every full-class run of `ASTParserLoadingTest`.
/// `apps/hib-suite-runner/FunctionalInterfaceHijackProbe.java` is the reduced
/// witness for all four interfaces; `hql-ordinal-parameter-dropped-under-jit-20260731-FIXED.md` is the writeup.
///
/// Kept as one helper rather than repeated at each bail so a third by-name
/// dispatch path added later inherits the guard instead of re-opening the hole.
///
/// SAFETY: same contract as the callers — a live `SharedVm` and a current
/// `JvmThread`, since the SAM body may resume interpretation.
fn try_lambda_proxy_sam_dispatch(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    let Some(Value::Object(Some(receiver))) = values.first().copied() else {
        return None;
    };
    // An array of lambda proxies (`Array.newInstance(r.getClass(), n)`) carries
    // the proxy's id in its header; it dispatches on `Object`, not the SAM.
    let receiver_class_id = jit_dispatch_class(vm, receiver).class_id()?;
    if !vm.classes.is_lambda_proxy_class(receiver_class_id) {
        return None;
    }
    match crate::runtime::interpreter::try_lambda_dispatch(
        vm,
        thread,
        receiver,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        &values[1..],
    ) {
        Ok(Some(result)) => Some(Ok(result)),
        // Not the proxy's SAM (a default method on the functional interface,
        // say) — fall through to the caller's ordinary path, unchanged.
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

/// The class a JIT helper DISPATCHES a receiver on, read from its header.
///
/// Round 11 wave 13 (lane `icwide`; the "Siblings checked" half of
/// `docs/feature-designs/jdk-only-mode.md`).
/// An array's header carries its COMPONENT's class id, so a bare
/// `class_id_of` takes an `Integer[]` for an `Integer`, a `K[]` for a `K`, and
/// an array of lambda proxies for a lambda proxy. An array dispatches on
/// `java/lang/Object` (JVMS §4.4.1 / §5.4.6), and no class-id key can name
/// that, so [`Self::class_id`] answers `None` for it and the caller declines
/// to its resolving path. Every class-id-keyed cache, guard or resolution in
/// this file reads its receiver through [`jit_dispatch_class`] /
/// [`jit_dispatch_class_validated`], so the kind screen cannot be forgotten
/// at the next one.
#[derive(Clone, Copy)]
enum JitDispatchClass {
    /// A plain object: the header's class id is its runtime class.
    Class(ClassId),
    /// An array (or any other non-plain header): `java/lang/Object`'s methods.
    JavaLangObject,
}

impl JitDispatchClass {
    /// The id to key a cache or guard on; `None` for `java/lang/Object`
    /// dispatch (an array), which such a key cannot describe.
    #[inline]
    fn class_id(self) -> Option<ClassId> {
        match self {
            Self::Class(cid) => Some(cid),
            Self::JavaLangObject => None,
        }
    }
}

/// [`JitDispatchClass`] of `obj` through the validating header reads
/// (`kind_of` / `class_id_of`); for a receiver not just proved by
/// `is_object_address`.
#[inline]
fn jit_dispatch_class(vm: &SharedVm, obj: ObjectRef) -> JitDispatchClass {
    if vm.mem.heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return JitDispatchClass::JavaLangObject;
    }
    JitDispatchClass::Class(vm.mem.heap.class_id_of(obj))
}

/// [`jit_dispatch_class`] for an `obj` the caller has JUST obtained from
/// `is_object_address` (the `class_id_of_validated` contract).
#[inline]
fn jit_dispatch_class_validated(vm: &SharedVm, obj: ObjectRef) -> JitDispatchClass {
    if vm.mem.heap.kind_of_validated(obj) != cratonvm_types::ObjectKind::Object {
        return JitDispatchClass::JavaLangObject;
    }
    JitDispatchClass::Class(vm.mem.heap.class_id_of_validated(obj))
}

struct VirtualDispatchTarget {
    class_name: std::sync::Arc<str>,
    cacheable_receiver: bool,
}

/// Resolve the runtime dispatch class for a virtual/interface helper call.
/// `invoke_or_native` binds to the class name it is handed rather than
/// re-dispatching on the receiver, so the helper must hand it the receiver's
/// *runtime* class when that class is trustworthy, or the CP-resolved
/// call-site class when the receiver carries only an unusable/synthetic id.
///
/// `cacheable_receiver` is true only when `class_name` is the receiver class
/// named by the MIC/PIC key. If the receiver is `ClassId(0)`, an array, or a
/// bare/interface fallback to the CP class, publishing that result under the
/// receiver id would poison later inline-cache hits.
///
/// Residual-6 diagnosis (env-gated, `CRATONVM_TRACE_CLASSVALUE`): true when
/// the `ClassValue.get(Class)` dispatch-trace probes should fire. Cached so
/// the hot dispatch path pays two slice compares + one bool load.
pub(crate) fn cv_trace_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || cratonvm_types::flags::runtime_flag_on("CRATONVM_TRACE_CLASSVALUE"))
}

/// Residual-6 diagnosis: does this invoke-info describe the
/// `get(Ljava/lang/Class;)Ljava/lang/Object;` signature the `ClassValue`
/// native answers? (Class-blind on purpose — the probe wants every route.)
fn cv_trace_match(info: &JitInvokeInfo) -> bool {
    // `CRATONVM_DBG_MIC_METHOD=<substring>` retargets this trace at any method.
    // It was hardcoded to one investigation's `get(Class)Object` site, which
    // made it useless for the next one; the MIC is the single hardest place in
    // the VM to observe, so the instrument should not need a rebuild to move.
    if let Some(want) = mic_trace_method_filter() {
        return info.method_name.contains(want);
    }
    // The latched flag first (r9w5): every compiled `get` call — `List.get`,
    // `Map.get` — reaches this on each `jit_invoke_virtual_mic`, and with the
    // trace off (always, outside an investigation) one cached bool answers it
    // without the two string compares.
    cv_trace_enabled()
        && info.method_name == "get"
        && info.descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;"
}

/// `CRATONVM_DBG_MIC_METHOD` — retarget [`cv_trace_match`] at an arbitrary
/// method name (substring match).
fn mic_trace_method_filter() -> Option<&'static str> {
    use std::sync::OnceLock;
    static F: OnceLock<Option<String>> = OnceLock::new();
    F.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_MIC_METHOD").ok())
        .as_deref()
}

// SAFETY: `vm` must be live for the call, and `receiver` must be a valid
// heap reference — it is dereferenced through `kind_of`/`class_id_of` to read
// the object header. Callers reach this only from a JIT dispatch site that has
// already decoded the receiver from an operand slot, so it is non-null and has
// not been moved by a collection in the interval.
unsafe fn virtual_dispatch_target_for_receiver(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> VirtualDispatchTarget {
    // An array-typed call site resolves against `java.lang.Object`'s method
    // table by JVMS §4.4.1, whatever the receiver's header says — decide it from
    // the SITE, above the header check below. See the matching comment in
    // `runtime::interpreter::invoke`'s `execute_invoke_kind`: the header-driven
    // form is correct only while the header is trustworthy, and a
    // reclaimed-and-re-served block's is not.
    let dispatch = if info.class_name.starts_with('[') {
        JitDispatchClass::JavaLangObject
    } else {
        jit_dispatch_class(vm, receiver)
    };
    let Some(cid) = dispatch.class_id() else {
        return VirtualDispatchTarget {
            class_name: std::sync::Arc::from("java/lang/Object"),
            cacheable_receiver: false,
        };
    };
    if cid == ClassId::new(0) {
        return VirtualDispatchTarget {
            class_name: if crate::vm::is_object_member(info.method_name, info.descriptor) {
                std::sync::Arc::from("java/lang/Object")
            } else {
                std::sync::Arc::from(info.class_name)
            },
            cacheable_receiver: false,
        };
    }

    let cm = vm.classes.class_manager.read();
    let Some(recv_class) = cm.get_class(cid) else {
        // A lambda proxy is not in the class store. A call that is not its SAM
        // (the lambda arm declined it) is a default method of the functional
        // interface, or an abstract method of one of its super-interfaces that
        // the interface implements with a default (`interface Throwing<F, R>
        // extends Function<F, R> { default R apply(F f) {..} }`). Resolving the
        // by-name tail against the SITE's class (`Function`) invoked the
        // abstract `Function.apply` on the proxy, which answered its first
        // argument; the interpreter selects from the proxy's own interface.
        // Quarkus suite wave 1 (`JandexUtilTest`: AssertJ `extracting(..)`).
        let class_name = vm
            .classes
            .lambda_call_site_for(cid)
            .filter(|_| !crate::vm::is_object_member(info.method_name, info.descriptor))
            .map(|site| site.functional_interface.clone())
            .unwrap_or_else(|| std::sync::Arc::from(info.class_name));
        return VirtualDispatchTarget {
            class_name,
            cacheable_receiver: false,
        };
    };
    let recv_name = recv_class.name.clone();
    let recv_is_iface = recv_class.is_interface();
    let recv_is_bare_object = recv_name.as_ref() == "java/lang/Object";
    let cp_is_not_object = info.class_name != "java/lang/Object";
    if (recv_is_iface || recv_is_bare_object)
        && cp_is_not_object
        && !crate::vm::is_object_member(info.method_name, info.descriptor)
        && recv_name.as_ref() != info.class_name
    {
        // COL8-2: an instance minted on a sub-interface of the site's class
        // that declares the method runs its own (published nowhere).
        let own = recv_is_iface
            && interface_receiver_dispatches_on_itself(
                vm,
                &cm,
                cid,
                jit_site_owner(&cm, info),
                info.class_name,
                info.method_name,
                info.descriptor,
            );
        VirtualDispatchTarget {
            class_name: if own {
                recv_name
            } else {
                std::sync::Arc::from(info.class_name)
            },
            cacheable_receiver: false,
        }
    } else if let Some((selected, _)) = receiver_selection_retarget(&cm, cid, info) {
        // The receiver's class would run a body §5.4.6 does not select (see
        // `receiver_selection_retarget`): dispatch by the selected class's
        // name, and publish nothing under the receiver's id.
        VirtualDispatchTarget {
            class_name: selected,
            cacheable_receiver: false,
        }
    } else {
        VirtualDispatchTarget {
            class_name: recv_name,
            cacheable_receiver: true,
        }
    }
}

/// The class a compiled call site's constant pool names (`info.class_name`),
/// looked up through the site's own loader (`info.declaring_class_id`). `None`
/// for an array owner, a class not loaded there, and a site with no context
/// (`declaring_class_id == 0`: the synthetic direct-dispatch sites, which name
/// JDK classes with one definition and need no selection — and whose by-name
/// bootstrap lookup would allocate on every uncached dispatch). Loads nothing.
fn jit_site_owner(cm: &crate::classloading::ClassManager, info: &JitInvokeInfo) -> Option<ClassId> {
    if info.class_name.starts_with('[') || info.declaring_class_id == 0 {
        return None;
    }
    cm.find_cp_class_for_class(info.class_name, ClassId::new(info.declaring_class_id))
}

/// JVMS §5.4.6 for a compiled virtual/interface call dispatched on the
/// receiver's class: the NAME of the class whose declaration the call runs,
/// when the lenient walk from `receiver_cid` would run another one — a private
/// or static method hiding the inherited one, or a public method in another
/// package that does not override a package-private resolved method. The
/// by-name dispatch (`invoke_or_native`) then starts at the selected class and
/// finds its own declaration first.
///
/// `None` — the usual answer, and the only one for a hierarchy with a
/// compatibility class in it or for §5.4.6's error answers — keeps the
/// receiver's class; see `resolve::selection::walk_selection_moves_to`.
///
/// Returns the selected class's id with its name (MEGA3-17-4), so a caller
/// that compiles for it (`CachedDispatchTarget::selected_cid`) needs no
/// by-name lookup. The name need not round-trip through the global table
/// (round 17 wave 3, lane select, MEGA3-17-2,
/// `r17-mega3-retarget-dropped-for-an-ambiguous-selected-name-FIXED-20261005.md`):
/// the selected class is a supertype of the receiver, and the by-name tail
/// dispatches a supertype name on the receiver's own linked copy
/// (`by_name_tail_dispatch_class` in `vm/src/vm/vm_exec.rs`). Where it does
/// not round-trip, `CRATONVM_SELECTION_RECEIVER_SUPERTYPE=0` restores the old
/// `None` (the receiver's own, non-selected body ran).
///
/// Asked where a dispatch target is resolved under the class-manager read
/// lock the caller already holds, which for the memoized form is once per
/// (site, receiver class).
fn receiver_selection_retarget(
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> Option<(std::sync::Arc<str>, ClassId)> {
    let owner = jit_site_owner(cm, info)?;
    let store = cm.class_store();
    let selected = crate::runtime::resolve::selection::receiver_dispatch_moves_to(
        store,
        receiver_cid,
        owner,
        info.method_name,
        info.descriptor,
    )?;
    let name = store.get(selected)?.name.clone();
    let faithful = cm.get_loaded_class_id(&name) == Some(selected)
        || crate::vm::vm_exec::selection_receiver_supertype_enabled();
    faithful.then_some((name, selected))
}

/// [`crate::runtime::resolve::selection::select_for_receiver_dispatch`] for a
/// compiled call site: the method a receiver of class `receiver_cid` runs for
/// `info`'s method, where every resolver in this file used to answer with the
/// lenient walk from the receiver's class alone.
fn select_for_jit_receiver<'a>(
    cm: &'a crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> Option<(&'a cratonvm_reader::method::ClassFileMethod, ClassId)> {
    crate::runtime::resolve::selection::select_for_receiver_dispatch(
        cm.class_store(),
        receiver_cid,
        jit_site_owner(cm, info),
        info.method_name,
        info.descriptor,
    )
}

/// A resolved virtual/interface dispatch target, memoized per thread by
/// [`virtual_dispatch_target_cached`].
///
/// `class_name` is an `Rc<str>` rather than the `Arc<str>` the uncached
/// resolver returns, and that is deliberate: the memo lives in a thread-local
/// and every consumer only ever needs `&str`, so a cache hit must not touch a
/// refcount shared with other threads. An `Arc::clone` here would be an atomic
/// RMW on one class's refcount from every mutator at once — the same contended
/// cache line this memo exists to stop touching.
#[derive(Clone)]
struct CachedDispatchTarget {
    class_name: std::rc::Rc<str>,
    cacheable_receiver: bool,
    /// `get_loaded_class_id(class_name) == Some(receiver_cid)` — the name
    /// round-trips back to exactly this receiver class, so resolving a callee
    /// *by name* cannot land in another loader's same-named class. Only
    /// meaningful when `cacheable_receiver`. No longer a publication or
    /// compile gate (interpreter round i1 wave 7): every door that used it now
    /// compiles for the receiver's class id; kept for the `cv-mic` trace.
    globally_named: bool,
    /// JVMS §5.4.6 selection for this (site, receiver class) ENDS IN AN ERROR
    /// (`AbstractMethodError` / conflicting defaults' ICCE) that the lenient
    /// walk from the receiver would answer with a body. Such a target is never
    /// `cacheable_receiver` (no compiled entry may be published for it), and
    /// every door that reads it raises [`jit_selection_failure`] instead of
    /// dispatching by name (round 12 wave 8 orchestrator,
    /// `r12w8-orch-probe-findings-on-w8` item 1: compiled callers ran the
    /// lenient body where the interpreter threw). Also set, under
    /// `--jdk-only`, for an `invokeinterface` whose receiver does not
    /// implement the interface (`jit_receiver_does_not_implement`, JVMS §6.5
    /// `IncompatibleClassChangeError`; interpreter round i1 wave 38, lane L6).
    selection_error: bool,
    /// The class id JVMS §5.4.6 selection moved this receiver's dispatch to
    /// (`receiver_selection_retarget`: a package-private resolved method next
    /// to a non-overriding public one in another package), when the target
    /// may be compiled for THAT class and published under the RECEIVER's id;
    /// `0` otherwise. Set only under `CRATONVM_JIT_MEGA_CACHE_RETARGETED`
    /// ([`mega_cache_retargeted_enabled`]; round 17 wave 2, lane mega2,
    /// MEGA17-3), and never together with `cacheable_receiver` or
    /// `selection_error`. Sound because the selected body is a function of
    /// (the site's resolution, the receiver class), which is exactly what
    /// every cache it is published into is keyed by: the site's own MIC/PIC
    /// ways, the per-thread entry cache keyed (site, receiver class id), and
    /// the shared table keyed (selector, receiver class id), whose selector
    /// names the resolved class, method and loader context. So it can never
    /// answer for a receiver whose selection differs.
    selected_cid: u32,
}

impl CachedDispatchTarget {
    /// May a compiled entry be published for this (site, receiver class)
    /// target: a receiver-named one, or one whose §5.4.6 selection was
    /// retargeted ([`Self::selected_cid`]). The one publication condition of
    /// the MIC arms and the blind arm (j27 wave 4, lane DISPROWS, MEGA3-17-3).
    fn publishable(&self) -> bool {
        self.cacheable_receiver || self.selected_cid != 0
    }

    /// The class id the published body is compiled for: the selected class
    /// of a retargeted receiver, else the receiver's own `receiver_cid`.
    fn compile_cid(&self, receiver_cid: u32) -> u32 {
        if self.selected_cid != 0 {
            self.selected_cid
        } else {
            receiver_cid
        }
    }
}

/// `CRATONVM_JIT_MEGA_CACHE_RETARGETED` (default ON since the round-17 close,
/// `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md; round 17 wave 2,
/// lane mega2, MEGA17-3,
/// `r17-mega-selection-retargeted-receivers-are-never-cached-FIXED-20261005.md`).
/// With it on, a receiver whose §5.4.6 selection moves to a superclass gets
/// its SELECTED class's compiled body published under its own class id (see
/// [`CachedDispatchTarget::selected_cid`]) instead of paying a by-name Rust
/// dispatch on every call. Read only where a (site, receiver class) target is
/// resolved on a memo miss, and only for a retargeted receiver.
fn mega_cache_retargeted_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_CACHE_RETARGETED")
}

/// COL8-2 (round 17 wave 2, lane mega2): may a receiver whose runtime class
/// is itself an INTERFACE dispatch on that interface rather than on the call
/// site's constant-pool class? The S111r12 fallback (here and in the
/// interpreter's `execute_invoke_kind`) sends every interface-classed receiver
/// to the constant-pool class, which is right for the case it was written for
/// (a receiver whose class id names an unrelated interface) and wrong for an
/// instance minted ON a sub-interface of the site's interface: the
/// `--compatible` `IntStream.iterator()` snapshot is a `PrimitiveIterator$OfInt`
/// and `invokeinterface Iterator.next` ran `Iterator.next`'s native instead of
/// `OfInt.next`'s (`R16CollectionsPrimItrMin`), while every cached door ran
/// the receiver's own. True when the receiver interface is a subtype of the
/// site's class AND declares the method itself, with a body or a registered
/// native, which is what §5.4.6 selection from that class would find first.
/// `CRATONVM_IFACE_RECEIVER_OWN_DISPATCH=0` restores the fallback. The switch
/// is read last, only for such a receiver.
pub(crate) fn interface_receiver_dispatches_on_itself(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    site_owner: Option<ClassId>,
    site_class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some(recv_class) = cm.get_class(receiver_cid) else {
        return false;
    };
    if !recv_class.is_interface() || recv_class.name.as_ref() == site_class_name {
        return false;
    }
    let is_subtype = match site_owner {
        Some(owner) => cm.is_subclass_of(receiver_cid, owner),
        None => cm.is_assignable_to_name(receiver_cid, site_class_name),
    };
    if !is_subtype {
        return false;
    }
    let declares = recv_class
        .find_method(method_name, descriptor)
        .is_some_and(|m| m.code().is_some())
        || vm
            .natives
            .native_methods
            .find(&recv_class.name, method_name, descriptor)
            .is_some();
    declares
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_IFACE_RECEIVER_OWN_DISPATCH")
}

/// The error a [`CachedDispatchTarget::selection_error`] target raises, built
/// on the (rare) call that meets it. `None` when the switch
/// (`CRATONVM_NATIVE_INVOKE_STRICT_SELECTION` under `--compatible`) is off or
/// the answer changed; the caller then dispatches as before.
fn jit_selection_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> Option<crate::error::MethodCallFailed> {
    // JVMS 5.4.3.3 / 5.4.3.4 before §6.5 and §5.4.6: a site whose own
    // reference does not resolve raises its `NoSuchMethodError` whatever the
    // receiver (wave 100, lane L2; `virtual_dispatch_target_cached` flags it).
    let unresolvable_owner = {
        let cm = vm.classes.class_manager.read();
        jit_site_unresolvable_owner(vm, &cm, info)
    };
    if let Some(owner_name) = unresolvable_owner {
        return Some(jit_site_unresolvable_error(vm, thread, owner_name, info));
    }
    let Some(receiver_cid) = jit_dispatch_class(vm, receiver).class_id() else {
        // An array receiver (`virtual_dispatch_target_cached`'s array arm).
        let message = jit_array_receiver_does_not_implement(vm, receiver, info)?;
        return Some(crate::runtime::exceptions::throw_linkage_error(
            vm,
            thread,
            crate::error::LinkageError::IncompatibleClassChangeError { message },
        ));
    };
    if receiver_cid == ClassId::new(0) {
        // A plain `java.lang.Object` (`virtual_dispatch_target_cached`'s
        // class-id-0 arm); nothing else is flagged for class id 0.
        let message = jit_object_receiver_does_not_implement(vm, receiver, info)?;
        return Some(crate::runtime::exceptions::throw_linkage_error(
            vm,
            thread,
            crate::error::LinkageError::IncompatibleClassChangeError { message },
        ));
    }
    let (owner, not_implemented) = {
        let cm = vm.classes.class_manager.read();
        (
            jit_site_owner(&cm, info),
            jit_receiver_does_not_implement(vm, &cm, receiver_cid, info),
        )
    };
    // JVMS §6.5 `invokeinterface`, checked before selection as HotSpot does
    // (`LinkResolver::runtime_resolve_interface_method`).
    if let Some(message) = not_implemented {
        return Some(crate::runtime::exceptions::throw_linkage_error(
            vm,
            thread,
            crate::error::LinkageError::IncompatibleClassChangeError { message },
        ));
    }
    let owner = owner?;
    // j27 wave 9 (lane W9EXC): a COMPILED caller raises the resolution text
    // (`Receiver class X does not define or inherit an implementation of the
    // resolved method ... Selected method is 'abstract ...'.`) even at a
    // linked `invokevirtual` site: HotSpot's compiled code reaches an abstract
    // selection through `SharedRuntime` (IC miss / `handle_wrong_method_abstract`),
    // which passes the receiver to `LinkResolver::throw_abstract_method_error`.
    // Measured on HotSpot 25.0.3 (`J27DispatchSelectRows`
    // `watermark-abstract-redecl-msg`): default, C1-only and C2-only answer the
    // receiver text; only `-Xint` answers the linked vtable-entry text, which
    // the interpreter keeps (`CRATONVM_LINKED_INVOKEVIRTUAL_AME_TEXT`).
    // Not `reflective_selection_failure`: its same-class fast accept answered
    // `None` for a receiver of exactly the site's class, and the site then ran
    // the lenient body (wave 35).
    crate::vm::vm_exec::flagged_selection_failure(
        vm,
        thread,
        receiver_cid,
        owner,
        info.method_name,
        info.descriptor,
    )
}

/// Does the call `info` end in an error for a receiver of class `receiver_cid`
/// that the lenient walk would answer with a body: an `invokeinterface` on a
/// receiver that does not implement the interface
/// ([`jit_receiver_does_not_implement`]), or a §5.4.6 selection error
/// ([`jit_strict_selection_errs`])? For the doors that ask before building a
/// callee template, under the caller's read guard.
fn jit_selection_errs(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    // Resolution of the site's own reference first (wave 100, lane L2): an
    // unresolvable one is the `NoSuchMethodError` whatever the receiver.
    jit_site_unresolvable_owner(vm, cm, info).is_some()
        || jit_receiver_does_not_implement(vm, cm, receiver_cid, info).is_some()
        || jit_strict_selection_errs(cm, receiver_cid, info)
}

/// JVMS §6.5 `invokeinterface` under `--jdk-only`: HotSpot's
/// `IncompatibleClassChangeError` message when `receiver_cid` provably does
/// not implement the interface the site `info` names
/// ([`crate::runtime::resolve::selection::receiver_does_not_implement`], the
/// interpreter's check since wave 30).
///
/// Asked only where a (site, receiver class) target is resolved: the miss
/// path of [`virtual_dispatch_target_cached`], which memoizes the answer, and
/// the bytecode-callee door's cold fill. A receiver it flags is never
/// `cacheable_receiver`, so no inline cache, entry memo or shared
/// megamorphic table is ever filled for it and no hit path carries the check
/// (interpreter round i1 wave 38, lane L6; `L6W38InterfaceReceiverHot`).
/// Kind 2 only: `invokevirtual` cannot name an interface, and a private
/// interface method pinned as kind 1 is not compiled under `--jdk-only`
/// (wave 37). `--compatible` keeps its duck typing.
fn jit_receiver_does_not_implement(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> Option<String> {
    if info.invoke_kind != 2 || !vm.classes.selection_checks {
        return None;
    }
    let owner = jit_site_owner(cm, info)?;
    crate::runtime::resolve::selection::receiver_does_not_implement(cm, receiver_cid, owner)
}

/// [`jit_receiver_does_not_implement`] for a receiver whose header carries
/// class id 0: a plain `java.lang.Object` has it
/// ([`crate::runtime::resolve::selection::object_receiver_does_not_implement`]
/// says when it is one), and the dispatch helpers otherwise treat the id as
/// "no usable class" and call by the interface's name. Takes the
/// class-manager lock itself; asked only for a class-id-0 receiver at an
/// `invokeinterface` site under `--jdk-only`, never on a hit path.
fn jit_object_receiver_does_not_implement(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> Option<String> {
    if info.invoke_kind != 2 || !vm.classes.selection_checks {
        return None;
    }
    let slots = vm.mem.heap.num_fields(receiver);
    if slots != 0 {
        return None;
    }
    let cm = vm.classes.class_manager.read();
    let owner = jit_site_owner(&cm, info)?;
    crate::runtime::resolve::selection::object_receiver_does_not_implement(&cm, slots, owner)
}

/// [`jit_receiver_does_not_implement`] for an ARRAY receiver
/// ([`crate::runtime::resolve::selection::array_receiver_does_not_implement`]):
/// the dispatch helpers answer one with `java/lang/Object` before any class
/// check, so an `invokeinterface` of an interface's default method on an
/// `int[]` ran the default, where HotSpot raises the ICCE (interpreter round
/// i1 wave 39, lane L2; item 1 of `i38-L6-compiled-dispatch-leftovers`).
/// `--jdk-only`, kind 2, not an array-typed site; a site naming `Cloneable`
/// or `Serializable` answers before any lock. Never on a hit path: an array
/// receiver has no class-id key, so every call of one takes the resolving
/// path this is asked on.
fn jit_array_receiver_does_not_implement(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> Option<String> {
    if info.invoke_kind != 2
        || !vm.classes.selection_checks
        || info.class_name.starts_with('[')
        || crate::runtime::resolve::selection::array_interface_name(info.class_name)
        || vm.mem.heap.kind_of(receiver) != cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    let descriptor = crate::runtime::interpreter::array_descriptor_of(vm, receiver)?;
    let cm = vm.classes.class_manager.read();
    let owner = jit_site_owner(&cm, info)?;
    crate::runtime::resolve::selection::array_receiver_does_not_implement(&cm, &descriptor, owner)
}

/// Does §5.4.6 selection of `info`'s method for `receiver_cid` end in an
/// error the lenient walk would answer with a body? For
/// [`CachedDispatchTarget::selection_error`], under the caller's read guard.
fn jit_strict_selection_errs(
    cm: &crate::classloading::ClassManager,
    receiver_cid: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    let Some(owner) = jit_site_owner(cm, info) else {
        return false;
    };
    crate::runtime::resolve::selection::receiver_selection_error(
        cm.class_store(),
        receiver_cid,
        owner,
        info.method_name,
        info.descriptor,
    )
    .is_some()
}

/// Clear-on-full bound for [`VIRTUAL_TARGET_CACHE`]. Entries are per
/// (call site, receiver class) pair, so a long-running server with a large
/// megamorphic surface could otherwise grow this without limit. Losing the
/// memo just reverts to the (correct, slower) locked resolution — same
/// bounded-cache posture as `ClassManager::note_synthetic_upgrade_absent`.
const VIRTUAL_TARGET_CACHE_CAP: usize = 4096;

/// Identity of one JIT call site, for every per-thread dispatch memo in this
/// module: `(SharedVm::vm_identity, JitInvokeInfo pointer)`.
///
/// **The `vm_identity` half is load-bearing — do not drop it.** A
/// `JitInvokeInfo` pointer alone does NOT identify a call site across VMs:
///
///  * several call sites pass the address of a process-global `static
///    JitInvokeInfo` (`INTEGER_VALUE_OF_INFO`, `INTEGER_INT_VALUE_INFO`,
///    `HASHMAP_PUT_DIRECT_INFO`, `HASHMAP_GET_DIRECT_INFO`,
///    `CONCURRENT_HASHMAP_GET_DIRECT_INFO`, `STRING_LATIN1_LOWER_DIRECT_INFO`),
///    which is literally the SAME address in every VM in the process;
///  * every other info is owned by the `CompiledMethod` that bakes it
///    (`_jit_invoke_infos`) and freed with that artifact, so a later
///    compilation — in this VM or another — can be handed the same address.
///
/// What these memos hold is per-VM to the word: raw compiled entry pointers
/// into one VM's code cache, receiver `ClassId`s, `NativeMethodId` census
/// handles from one VM's registry, and resolved callee class names. A
/// cross-VM hit therefore does not degrade to a slow path — it CALLs another
/// VM's compiled body, or runs `java/util/HashMap`'s native against whatever
/// class happens to hold that id in this VM. See
/// `vm-jit-cache-keying.md`.
///
/// `vm_identity` is a monotonically issued counter (`vm_init.rs`
/// `NEXT_VM_IDENTITY`), never an address, so it is never recycled — unlike a
/// `&SharedVm` cast to `usize`, which a sequentially-created second VM can
/// plausibly inherit from a dropped one.
pub(crate) type JitSiteKey = (usize, usize);

/// Build a [`JitSiteKey`]. Free function (not a method on `SharedVm`) so the
/// keying can be unit-tested without constructing a VM.
#[inline]
pub(crate) fn jit_site_key(vm_identity: usize, info_ptr: usize) -> JitSiteKey {
    (vm_identity, info_ptr)
}

thread_local! {
    /// `(vm_identity, that VM's class-store definition epoch,
    /// any_class_redefined)` this thread last flushed its class-identity
    /// dispatch memos at. See [`flush_class_identity_dispatch_memos`].
    /// `vm_identity` 0 is never a real VM, so the first call flushes.
    static DISPATCH_MEMO_CLASS_IDENTITY: Cell<(usize, u64, bool)> =
        const { Cell::new((0, 0, false)) };
    /// zt t3/nd: the narrow key of [`flush_class_identity_dispatch_memos`]
    /// under `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH`: `(vm_identity, name
    /// generation, hierarchy-edge epoch, in-place origin epoch,
    /// any_class_redefined)`, and the definition epoch the refusal entries
    /// were last dropped at. `vm_identity` 0 never matches.
    static DISPATCH_MEMO_CLASS_NAMES: Cell<((usize, u64, u64, u64, bool), u64)> =
        const { Cell::new(((0, 0, 0, 0, false), 0)) };
    /// `(vm_identity, redefine epoch)` at which this thread last ran the
    /// quiesced-site flush in `jit_invoke_dispatch_body`. `u32::MAX` never
    /// matches a real epoch before four billion redefinitions.
    static QUIESCED_SITE_FLUSH_STAMP: Cell<(usize, u32)> = const { Cell::new((0, u32::MAX)) };
}

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
                static $rname: std::cell::RefCell<rustc_hash::FxHashMap<$rkey, $rval>> =
                    std::cell::RefCell::new(rustc_hash::FxHashMap::default());
            )+
            $(
                $(#[$pattr])*
                static $pname: std::cell::RefCell<rustc_hash::FxHashMap<$pkey, $pval>> =
                    std::cell::RefCell::new(rustc_hash::FxHashMap::default());
            )+
        }

        /// Drop every thread-local memo whose key contains a `JitInvokeInfo`
        /// address. Generated from [`site_keyed_memos!`]'s own declaration
        /// list, so it cannot fall behind it.
        #[cold]
        fn clear_site_keyed_dispatch_memos() {
            $( $rname.with(|c| c.borrow_mut().clear()); )+
            $( $pname.with(|c| c.borrow_mut().clear()); )+
        }

        /// Drop only the `publication_sensitive` group: what a publication
        /// that retired nothing requires.
        #[cold]
        fn clear_publication_sensitive_site_memos() {
            $( $pname.with(|c| c.borrow_mut().clear()); )+
        }

        /// `(memo name, live entry count)` for every site-keyed memo on this
        /// thread, in declaration order. Test-only: it is what lets
        /// `a_jit_generation_change_clears_every_site_keyed_memo` assert
        /// against the real list rather than a copy of it.
        #[cfg(test)]
        fn site_keyed_memo_census() -> Vec<(&'static str, usize)> {
            vec![
                $( (stringify!($rname), $rname.with(|c| c.borrow().len())), )+
                $( (stringify!($pname), $pname.with(|c| c.borrow().len())), )+
            ]
        }

        /// Names of the `publication_sensitive` group. Test-only.
        #[cfg(test)]
        fn publication_sensitive_memo_names() -> Vec<&'static str> {
            vec![ $( stringify!($pname) ),+ ]
        }
    };
}

/// Flush this thread's dispatch memos whose validity depends on class
/// *identity* — the name-to-`ClassId` mapping — as opposed to compiled-code
/// lifetime.
///
/// Two atomic loads on the steady-state path, and nothing else.
///
/// `class_definition_epoch` moves whenever a class is defined or unloaded,
/// which is precisely what can invalidate a memoized `globally_named` answer.
/// None of the signals `jit_invoke_dispatch` already checks covers that:
/// a second loader defining the same name redefines nothing, publishes no
/// compiled code, and supersedes no tier. So clearing `DISPATCH_CACHE` and
/// `VIRTUAL_DISPATCH_CACHE` here also closes a hole those two already had —
/// they cache a compiled entry chosen on the strength of a `globally_named`
/// test that nothing was re-running.
///
/// `STATIC_BYTECODE_CALLEE_CACHE` is here for the same reason and needs it
/// most: it holds an interpreter frame template resolved through
/// `get_loaded_class_id(info.class_name)`, i.e. keyed on a class NAME. A second
/// loader defining that name is precisely the event that makes the template
/// describe the wrong class's method — and it is invisible to the redefine and
/// tier signals, so without this line the site would keep entering the first
/// loader's body forever.
///
/// `any_class_redefined` is folded in as cheap insurance. None of the other
/// memoized fields can actually move under redefinition (a `ClassId`'s name
/// and `ACC_INTERFACE` are fixed at definition, and redefinition rewrites
/// method bodies in place), but it is a sticky bool whose single false->true
/// transition costs one flush for the life of the process, so paying it is
/// strictly cheaper than reasoning about every future redefinition path.
///
/// The epoch is `vm`'s class store's (`StoreEpochs`, interpreter round i1
/// wave 21, lane L5): every memo flushed here is keyed by `vm_identity`, and
/// a class defined in another VM cannot change what a name means in `vm`, so
/// it no longer empties this thread's memos. The state names the VM too: a
/// thread that switches VMs flushes, so every entry left in the memos was
/// filled by the VM the state names, under the epoch it records.
/// Release what THIS thread's memos hold of the VM `vm_identity`'s compiled
/// code, when that VM is dropped (`Drop for Vm`).
///
/// The site-keyed memos (`DispatchCache::owner`), [`LAMBDA_GET_MEMO`]'s direct
/// routes and the lambda site caches each own a compiled body. They are per
/// thread and keyed by VM identity, and nothing else empties them for a VM
/// that is gone: the site-keyed ones are flushed only when this thread next
/// dispatches into some VM, and the lambda ones never. So a thread that
/// outlived its VM kept that VM's bodies -- and the lambda thunks their inline
/// caches own -- mapped for as long as it lived
/// (`docs/internal/jit/t1-jitic-lambda-thunk-map-outlives-its-vm-20261025.md`).
///
/// The site-keyed memos are emptied whole, every VM's rows: they are caches,
/// a VM drop is rare, and a living VM's rows refill on their next miss. A
/// released owner goes through the retirement queue (`RetainedCode`'s drop).
/// Other threads' memos are theirs to drop: a thread of the dropped VM ends
/// with it, and its TLS with it.
pub(crate) fn release_thread_jit_memos_for_vm(vm_identity: usize) {
    clear_site_keyed_dispatch_memos();
    LAMBDA_GET_MEMO.with(|m| {
        if let Ok(mut map) = m.try_borrow_mut() {
            map.retain(|key, _| key.0 != vm_identity);
        }
    });
    crate::runtime::interpreter::lambda::release_thread_lambda_sites_for_vm(vm_identity);
}

fn flush_class_identity_dispatch_memos(vm: &SharedVm) {
    if dispatch_memo_name_epoch_enabled() {
        flush_class_identity_dispatch_memos_narrow(vm);
        return;
    }
    let state = (
        vm.vm_identity,
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
        crate::classloading::any_class_redefined(),
    );
    DISPATCH_MEMO_CLASS_IDENTITY.with(|seen| {
        if seen.get() != state {
            seen.set(state);
            VIRTUAL_TARGET_CACHE.with(|c| c.borrow_mut().clear());
            DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            // Resolved through `get_loaded_class_id(info.class_name)` too.
            WRAPPED_STATIC_CALLEE.with(|c| c.borrow_mut().clear());
            CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| c.borrow_mut().clear());
        }
    });
}

/// zt t3/nd (`proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016-RETIRED-20261024.md`, the
/// per-define cost): `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH` (default ON since
/// g1gen, `=0` kill; read once per process). The class-definition epoch moves on EVERY
/// class definition, so a loop that defines a class per iteration (a fresh
/// `Proxy.getProxyClass` loader, a `defineHiddenClass` per call) emptied every
/// memo of [`flush_class_identity_dispatch_memos`] on every compiled call
/// after each define. A brand-new name cannot change a POSITIVE entry: each
/// holds a resolution of a name that was already mapped (it changes only with
/// the name generation: a second definer of an existing name, a removal, an
/// unload), a selection over stored classes' edges (the hierarchy-edge epoch,
/// which every forward edge also moves) and method tables (the in-place
/// origin epoch: a stub upgraded to real bytes), or a redefinition (the
/// sticky flag). A brand-new name CAN turn a cached REFUSAL into an answer,
/// so on a definition-epoch move alone the refusals are dropped (`None`
/// entries of the three bytecode-callee memos, and `WRAPPED_STATIC_CALLEE`,
/// which holds only refusals) and the positive entries stay.
///
/// Not every refusal-like entry is dropped: `VIRTUAL_TARGET_CACHE`'s
/// `selection_error: true` rows (unresolvable owner, receiver does not
/// implement, strict selection error) are KEPT. Each is a function of the
/// owner's or the receiver's stored hierarchy and method tables, which a
/// brand-new class cannot change without moving one of the four keyed
/// epochs. `DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE` and
/// `CALLEE_HANDLER_TEMPLATE_CACHE` hold positive entries only.
///
/// zt t6/rv3 (`zgc-zt4x3-*` item 1): read through a
/// [`cratonvm_types::flags::MemoSlot`] (was a process `OnceLock`, which a
/// test's thread override could never reach); an override resets the slot.
fn dispatch_memo_name_epoch_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH",
            );
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// [`flush_class_identity_dispatch_memos`] under
/// [`dispatch_memo_name_epoch_enabled`]: the full flush when a name answer,
/// an edge, an in-place origin or the redefinition flag moved (or the thread
/// switched VMs); only the refusals when nothing but the definition epoch
/// moved. Five atomic loads on the steady state.
fn flush_class_identity_dispatch_memos_narrow(vm: &SharedVm) {
    let epochs = cratonvm_classloading::store_epochs(vm.jit.class_layout_domain);
    let state = (
        vm.vm_identity,
        epochs.class_name_generation(),
        epochs.hierarchy_edge_epoch(),
        epochs.class_origin_in_place_epoch(),
        crate::classloading::any_class_redefined(),
    );
    let definition = epochs.class_definition_epoch();
    narrow_dispatch_memo_flush(state, definition);
}

/// The body of [`flush_class_identity_dispatch_memos_narrow`] given the VM's
/// current keyed `state` and `definition` epoch (zt t6/rv3: split out so a
/// unit test can move one epoch at a time without defining classes).
fn narrow_dispatch_memo_flush(state: (usize, u64, u64, u64, bool), definition: u64) {
    DISPATCH_MEMO_CLASS_NAMES.with(|seen| {
        let (seen_state, seen_definition) = seen.get();
        if seen_state != state {
            seen.set((state, definition));
            VIRTUAL_TARGET_CACHE.with(|c| c.borrow_mut().clear());
            DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_DISPATCH_CACHE.with(|c| c.borrow_mut().clear());
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().clear());
            WRAPPED_STATIC_CALLEE.with(|c| c.borrow_mut().clear());
            CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| c.borrow_mut().clear());
        } else if seen_definition != definition {
            seen.set((state, definition));
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().retain(|_, v| v.is_some()));
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().retain(|_, v| v.is_some()));
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow_mut().retain(|_, v| v.is_some()));
            WRAPPED_STATIC_CALLEE.with(|c| c.borrow_mut().clear());
        }
    });
}

#[cfg(test)]
mod zt6_rv3_narrow_memo_flush_tests {
    //! zt t6/rv3 (`zgc-zt4x3-wave-t3-review-residuals-DECIDED-20261019.md` item 1):
    //! `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH`'s narrow flush, one epoch at a
    //! time, and its switch reached by a thread override.
    use super::*;

    type Template = Option<(
        std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
        crate::classloading::resolution::RedefineGate,
    )>;

    fn template() -> Template {
        Some((
            std::sync::Arc::new(crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: ClassId::new(77),
                    class_name: std::sync::Arc::from("com/example/Callee"),
                    method_name: std::sync::Arc::from("m"),
                    method_descriptor: std::sync::Arc::from("()V"),
                    source_file: None,
                    code: crate::runtime::frame::padded_bytecode(&[0xb1]),
                    exception_table: std::sync::Arc::from(Vec::new()),
                    max_stack: 0,
                    max_locals: 0,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: true,
                },
            )),
            crate::classloading::resolution::RedefineGate::never_stale(),
        ))
    }

    /// `answer`: a positive entry in each bytecode-callee memo. `refused`: a
    /// cached refusal in each, a wrapped-entry refusal and a
    /// `selection_error` virtual target.
    fn fill(answer: JitSiteKey, refused: JitSiteKey) {
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.insert(answer, template());
            c.insert(refused, None);
        });
        VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.insert((answer, 7), template());
            c.insert((refused, 7), None);
        });
        SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.insert(answer, template());
            c.insert(refused, None);
        });
        WRAPPED_STATIC_CALLEE.with(|c| {
            c.borrow_mut().insert(refused, ());
        });
        VIRTUAL_TARGET_CACHE.with(|c| {
            c.borrow_mut().insert(
                (refused, 7),
                CachedDispatchTarget {
                    class_name: std::rc::Rc::from("com/example/Owner"),
                    cacheable_receiver: true,
                    globally_named: true,
                    selection_error: true,
                    selected_cid: 0,
                },
            );
        });
    }

    /// `(static, virtual, special)` memo entry of `site`: `Some(true)` a
    /// positive entry, `Some(false)` a refusal, `None` no entry.
    fn entries(site: JitSiteKey) -> [Option<bool>; 3] {
        [
            STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&site).map(|v| v.is_some())),
            VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&(site, 7)).map(|v| v.is_some())),
            SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&site).map(|v| v.is_some())),
        ]
    }

    fn wrapped(site: JitSiteKey) -> bool {
        WRAPPED_STATIC_CALLEE.with(|c| c.borrow().contains_key(&site))
    }

    fn selection_error_kept(site: JitSiteKey) -> bool {
        VIRTUAL_TARGET_CACHE.with(|c| c.borrow().contains_key(&(site, 7)))
    }

    #[test]
    fn zt6rv3_a_definition_only_move_drops_refusals_and_keeps_answers() {
        // A `vm_identity` no test VM is issued (a counter from 1).
        const VM: usize = usize::MAX - 61;
        let answer = jit_site_key(VM, 0x1000);
        let refused = jit_site_key(VM, 0x2000);
        let state = (VM, 4, 5, 6, false);
        narrow_dispatch_memo_flush(state, 10);
        fill(answer, refused);

        // Nothing moved: nothing is dropped.
        narrow_dispatch_memo_flush(state, 10);
        assert_eq!(entries(refused), [Some(false); 3]);
        assert!(wrapped(refused));

        // Only the definition epoch moved (a brand-new class): the refusals
        // go, the answers and the `selection_error` target stay.
        narrow_dispatch_memo_flush(state, 11);
        assert_eq!(entries(answer), [Some(true); 3], "answers survive");
        assert_eq!(entries(refused), [None; 3], "refusals dropped");
        assert!(!wrapped(refused), "the wrapped-entry refusal is dropped");
        assert!(selection_error_kept(refused), "selection_error rows are kept");

        // Positive control: a second definer of an existing name moves the
        // name generation, which flushes every memo.
        fill(answer, refused);
        narrow_dispatch_memo_flush((VM, 5, 5, 6, false), 12);
        assert_eq!(entries(answer), [None; 3], "full flush");
        assert_eq!(entries(refused), [None; 3]);
        assert!(!wrapped(refused));
        assert!(!selection_error_kept(refused));

        // And each other keyed epoch flushes in full too.
        for moved in [(VM, 5, 6, 6, false), (VM, 5, 6, 7, false), (VM, 5, 6, 7, true)] {
            fill(answer, refused);
            narrow_dispatch_memo_flush(moved, 12);
            assert_eq!(entries(answer), [None; 3], "{moved:?}");
        }
    }

    /// The switch is a `MemoSlot` now, so a thread override reaches it (a
    /// process `OnceLock` latched the first answer for every later test).
    #[test]
    fn zt6rv3_the_name_epoch_switch_reaches_a_thread_override() {
        const KEY: &str = "CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH";
        for (value, want) in [(Some("1"), true), (Some("0"), false), (None, true)] {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, value)], || {
                assert_eq!(dispatch_memo_name_epoch_enabled(), want, "{value:?}");
            });
        }
    }
}

/// Was the class the call site `info` names ever redefined
/// ([`class_id_or_name_was_redefined`])? Memoized per site at its VM's
/// redefinition epoch (`QUIESCE_VERDICT`; round 12 wave 1, lane rt, the
/// quiesce-verdict memo of `r11-rt-helper-hot-paths-repeat-work-per-call`).
///
/// Once any class in the process was redefined (a Mockito inline mock, an
/// agent's retransform) the uncached verdict is a class-manager read lock and
/// a by-id-or-name lookup on EVERY `jit_invoke_dispatch_body` call, at every
/// site, redefined or not. The memo makes it one thread-local probe.
///
/// Sound because a `false` only turns `true` through a redefinition, and the
/// one production path (`redefine_class_with`) bumps the class's generation
/// under the class-manager writer BEFORE `JitCache::bump_redefine_epoch`
/// (`note_class_redefinition_of`). The epoch is read BEFORE the verdict, so an
/// entry stamped `e` saw every redefinition `e` counts. A stale `true` (the
/// name now resolves to a newly defined class) only keeps the site on the
/// quiesced path, which is conservative. Kill switch:
/// `CRATONVM_JIT_QUIESCE_VERDICT_MEMO=0`.
fn site_class_was_redefined(vm: &SharedVm, info: &JitInvokeInfo, info_key: JitSiteKey) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    if !quiesce_verdict_memo_enabled() {
        return class_id_or_name_was_redefined(vm, info.declaring_class_id, info.class_name);
    }
    let epoch = vm.jit.jit_cache.redefine_epoch();
    let memo = QUIESCE_VERDICT.with(|m| m.borrow().get(&info_key).copied());
    if let Some((stamp, verdict)) = memo {
        if stamp == epoch {
            return verdict;
        }
    }
    let verdict = class_id_or_name_was_redefined(vm, info.declaring_class_id, info.class_name);
    QUIESCE_VERDICT.with(|m| {
        m.borrow_mut().insert(info_key, (epoch, verdict));
    });
    verdict
}

/// `CRATONVM_JIT_QUIESCE_VERDICT_MEMO`: default ON; `0` / `false` / `off`
/// asks [`class_id_or_name_was_redefined`] on every dispatch, as before.
fn quiesce_verdict_memo_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_QUIESCE_VERDICT_MEMO").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Does `jit_invoke_dispatch_body` stand its compiled-entry arms down for a
/// redefinition of the site's class on THIS call? Round 12 wave 2, lane rt
/// (proposal R12W1-1 of `docs/feature-designs/jit-ir-optimizations.md`).
///
/// This was [`site_class_was_redefined`], which never goes back to `false`:
/// after one `Mockito.mock(Foo.class)` or one agent retransform, every
/// compiled call site into `Foo` ran its callee interpreted for the life of
/// the process. Now the site stands down once per thread per redefinition of
/// its VM: on the call that runs the quiesced-site flush of the thread's
/// raw-entry caches (`QUIESCED_SITE_FLUSH_STAMP`, stamped by the caller). The
/// MIC helper has quiesced this way, once per slot per redefinition epoch,
/// since `hierarchy_was_redefined` left it.
///
/// Sound because staleness is handled where the bodies live, not here. A
/// reader that sees the class's new generation took the class-manager read
/// lock after the redefinition's writer released it, and that writer had
/// already withdrawn every stale body (`note_class_redefinition_of`: full
/// flush, or the scoped withdrawal plus not-entrant patching). A compile that
/// began before is refused at publication (`flush_barrier`, the per-class
/// barrier). A thread-local entry naming a withdrawn body is dropped at its
/// hit (`DispatchCache::names_a_stale_body`) and by the retire generation
/// (`flush_raw_entry_dispatch_caches`). The bytecode templates carry their
/// own `RedefineGate`. A body entered after that was compiled from the
/// current bytecode, the question the per-body stamp of the proposal would
/// ask. Obsolete frames (JEP 109) are interpreter frames and never reach
/// this helper.
///
/// Kill switch `CRATONVM_JIT_QUIESCE_BY_GENERATION=0`: the permanent
/// quiescence, as before.
fn site_quiesced_for_redefinition(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
) -> bool {
    if !site_class_was_redefined(vm, info, info_key) {
        return false;
    }
    if !quiesce_by_generation_enabled() {
        return true;
    }
    let stamp = (vm.vm_identity, vm.jit.jit_cache.redefine_epoch());
    QUIESCED_SITE_FLUSH_STAMP.with(|seen| seen.get()) != stamp
}

/// Must [`try_resume_trapped_callee`] refuse the frame a compiled body of
/// `key_class.key_method key_desc` stashed at the point `point_addr`, because
/// that class was redefined? Round 12 wave 2, lane rt: the other half of
/// R12W1-1.
///
/// The frame is rebuilt against the method's CURRENT bytecode, so it may be
/// resumed only if the body that trapped was compiled from it. This asked
/// "was the class ever redefined", which refused every trap of every body of
/// a mocked class forever. A refused trap is re-run from the callee's entry
/// (`rerun_declined_callee_from_entry`), which replays what the compiled body
/// had already committed. The MIC helper enters such bodies after one
/// quiesced call per redefinition, and since [`site_quiesced_for_redefinition`]
/// `jit_invoke_dispatch_body` does too, so the replay was reachable on every
/// uncommon trap in them.
///
/// Now a body is current when it is the one published for the method, it owns
/// the point that fired (`CompiledMethod::owns_deopt_point`), and its
/// compilation began after the class's last redefinition
/// (`JitCache::compiled_since_redefinition_of`, the question
/// `granted_exit_of_a_current_body` asks). Anything else (a superseded or
/// withdrawn body, an OSR body, a name another loader resolves, no point)
/// keeps the refusal. The trapping body is pinned by the call arm that
/// entered it, so its points cannot be reused by the published body's.
/// `CRATONVM_JIT_QUIESCE_BY_GENERATION=0` restores the plain refusal.
///
/// The class is the one the call site's loader resolves `key_class` to
/// (`declaring_class_id`, as [`try_resume_trapped_callee`] resolves the method
/// it rebuilds the frame from, and [`stash_is_from_a_current_body`] the body):
/// interpreter round i1 wave 29, lane L3. It was the loader-blind
/// `get_loaded_class_id` name lookup, which answers the BUILT-IN loaders' class
/// of that name first, so with the same name defined by the application loader
/// (never redefined) and by a user loader (redefined: a mocked class in a
/// test framework's isolated loader), a stale body of the user loader's class
/// was judged "not redefined" and its trap rebuilt against the class's NEW
/// bytecode. A name the site's loader cannot resolve keeps the name answer.
fn stashed_callee_predates_redefinition(
    vm: &SharedVm,
    declaring_class_id: u32,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
    point_addr: usize,
) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let resolved = {
        let cm = vm.classes.class_manager.read();
        if declaring_class_id == 0 {
            cm.find_bootstrap_class_by_name(key_class)
        } else {
            cm.find_cp_class_for_class(key_class, ClassId::new(declaring_class_id))
        }
    };
    let Some(class_id) = resolved else {
        // The resume below cannot resolve it either and refuses; keep the
        // answer this gave before.
        return named_class_was_redefined(vm, key_class);
    };
    if !class_was_redefined(vm, class_id) {
        return false;
    }
    if !quiesce_by_generation_enabled() || point_addr == 0 {
        return true;
    }
    let cache = &vm.jit.jit_cache;
    let by_name = cache
        .get(key_class, key_method, key_desc, class_id)
        .is_some_and(|body| {
            body.owns_deopt_point(point_addr)
                && cache.compiled_since_redefinition_of(class_id, body.install_epoch)
        });
    // Interpreter round i2 wave 61, lane L2: the body the trampoline named
    // answers first; the published body found by name only when it is absent.
    match named_trapping_body_is_current(vm, class_id, point_addr, by_name, "predates-redefinition")
    {
        Some(current) => !current,
        None => !by_name,
    }
}

/// Did the body the deopt stash names for its top entry (the trampoline's
/// own answer, [`cratonvm_jit::deopt::peek_last_deopt_trapping_body`]) run
/// `class_id`'s CURRENT bytecode -- compiled after that class's last
/// redefinition? `None` when the stash names no body of `class_id` at
/// `point_addr` (the switch off, a restash, the re-run sentinel, a body
/// published under another class's key): the caller then asks the method's
/// PUBLISHED body by name, as before.
///
/// Interpreter round i2 wave 61, lane L2: the remaining sinks of
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// The name lookup answers what is published NOW, so a body superseded by a
/// tier-up (or withdrawn) after its compile -- exactly the body whose trap is
/// in flight -- was judged stale even when it had been compiled from the
/// current bytecode, and its frame went to the own-source resume or the
/// re-run from entry. `by_name` is the lookup's answer, printed beside the
/// named one under `CRATONVM_DBG_DEOPT` (`agrees=`) so the host can see where
/// the two differ. Cold: only on a callee trap of a redefined class.
fn named_trapping_body_is_current(
    vm: &SharedVm,
    class_id: ClassId,
    point_addr: usize,
    by_name: bool,
    sink: &str,
) -> Option<bool> {
    if point_addr == 0 || !cratonvm_jit::deopt::stash_names_trapping_body_enabled() {
        return None;
    }
    let body = cratonvm_jit::deopt::peek_last_deopt_trapping_body().filter(|body| {
        body.owns_deopt_point(point_addr) && body.owner_class_id == class_id.as_u32()
    })?;
    let current = vm
        .jit
        .jit_cache
        .compiled_since_redefinition_of(class_id, body.install_epoch);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] callee trap body named by the stash ({sink}): class_id={} \
             current={current} by-name={by_name} agrees={}",
            class_id.as_u32(),
            current == by_name
        );
    }
    Some(current)
}

/// `CRATONVM_JIT_QUIESCE_BY_GENERATION`: default ON; `0` / `false` / `off`
/// keeps every call through a redefined class's site quiesced for the life of
/// the process ([`site_quiesced_for_redefinition`]).
fn quiesce_by_generation_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_QUIESCE_BY_GENERATION").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

thread_local! {
    /// [`class_was_redefined`]'s memo: `(vm_identity, that VM's redefine
    /// epoch, its class-store definition epoch)` the verdicts were read
    /// under, and `class id -> ever redefined`.
    static REDEFINED_CLASS_MEMO: std::cell::RefCell<(
        (usize, u32, u64),
        rustc_hash::FxHashMap<u32, bool>,
    )> = std::cell::RefCell::new(((0, u32::MAX, 0), rustc_hash::FxHashMap::default()));
}

/// Clear-on-full bound for [`REDEFINED_CLASS_MEMO`].
const REDEFINED_CLASS_MEMO_CAP: usize = 4096;

/// Was `class_id` ever redefined? [`crate::runtime::redefine_state::class_was_redefined`],
/// memoized per thread (round 12 wave 2, lane rt, proposal R12W1-3). It
/// shadows that function for every caller in this file on purpose: the
/// native-site hit, the object-native and wrapper-class caches, the
/// HashMap/CHM/Matcher receiver guards. Once any class in the process was
/// redefined, each of them took the class-manager read lock per call.
///
/// Sound for the reason [`site_class_was_redefined`] is: a `false` turns
/// `true` only through a redefinition of this VM, which bumps the class's
/// generation under the class-manager writer before
/// `JitCache::bump_redefine_epoch`, and the epoch is read before the verdict.
/// A stale `true` only keeps a fast path off. The class-definition epoch in
/// the stamp retires a `true` whose class id an unload freed and a new class
/// took. Kill switch `CRATONVM_JIT_REDEFINED_CLASS_MEMO=0`.
#[inline]
fn class_was_redefined(vm: &SharedVm, class_id: ClassId) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    if !redefined_class_memo_enabled() {
        return crate::runtime::redefine_state::class_was_redefined(vm, class_id);
    }
    let stamp = (
        vm.vm_identity,
        vm.jit.jit_cache.redefine_epoch(),
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
    );
    let raw = class_id.as_u32();
    let memo = REDEFINED_CLASS_MEMO.with(|m| {
        let m = m.borrow();
        if m.0 == stamp {
            m.1.get(&raw).copied()
        } else {
            None
        }
    });
    if let Some(verdict) = memo {
        return verdict;
    }
    let verdict = crate::runtime::redefine_state::class_was_redefined(vm, class_id);
    REDEFINED_CLASS_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.0 != stamp || m.1.len() >= REDEFINED_CLASS_MEMO_CAP {
            m.0 = stamp;
            m.1.clear();
        }
        m.1.insert(raw, verdict);
    });
    verdict
}

/// `CRATONVM_JIT_REDEFINED_CLASS_MEMO`: default ON; `0` / `false` / `off`
/// asks the class manager on every [`class_was_redefined`] call, as before.
fn redefined_class_memo_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_REDEFINED_CLASS_MEMO").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Where the retire count sits in [`cratonvm_jit::jit_cache_generation`].
///
/// `0` means "every bump is a retire", which is exactly the behaviour before
/// the publish/retire split: any generation change drops every site-keyed
/// memo. A nonzero shift is sound only because `jit/src/lib.rs` bumps the high
/// half on every withdrawal and in `Drop for CompiledMethod` (without those, a
/// recycled `JitInvokeInfo` address would keep the previous site's positive
/// memo); both landed together in round 11 wave 9
/// (`r11w9-rt-lib-publish-retire-generation-patch-FIXED-20260924.md`).
const JIT_RETIRE_SHIFT: u32 = cratonvm_jit::JIT_CACHE_RETIRE_SHIFT;

/// What a change of the JIT cache generation requires of the site-keyed memos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SiteMemoFlush {
    /// The generation did not move.
    Nothing,
    /// Only publications moved it: a new key cannot falsify a positive memo,
    /// and no `JitInvokeInfo` address was freed.
    PublicationSensitive,
    /// A body was replaced, withdrawn or freed.
    All,
}

/// Decide the flush for a thread that last saw `previous` and now sees `now`.
///
/// Pure, so both settings of the shift are unit-tested whatever
/// [`JIT_RETIRE_SHIFT`] is in the build. A carry out of the publish half into
/// the retire half reads as a retire: a spurious full flush, which is safe.
#[inline]
fn site_memo_flush(previous: u64, now: u64, retire_shift: u32) -> SiteMemoFlush {
    if previous == now {
        SiteMemoFlush::Nothing
    } else if previous.checked_shr(retire_shift).unwrap_or(0)
        != now.checked_shr(retire_shift).unwrap_or(0)
    {
        SiteMemoFlush::All
    } else {
        SiteMemoFlush::PublicationSensitive
    }
}

/// Flush every per-thread memo keyed by [`JitSiteKey`], so a probe cannot serve
/// one call site's resolution to a different one.
///
/// **The key half that makes this mandatory is the `JitInvokeInfo` pointer.**
/// Those boxes are owned by `CompiledMethod::_jit_invoke_infos` and are freed
/// when that method drops (see the field doc in `jit/src/lib.rs`). The allocator
/// is then free to hand the same address to the next compile's `JitInvokeInfo` —
/// at which point `(vm_identity, info_ptr)` names a DIFFERENT call site while
/// every memo still holds the old site's answer. What the old site's answer is
/// decides how the aliasing surfaces:
///
///  * `NATIVE_SITE_CACHE` holds a resolved leaf-native callback, so the reused
///    site CALLS the previous site's native and returns whatever that returns —
///    e.g. a `boolean` 1 landing in a slot the caller then treats as an
///    `Annotation[]`, whose `arraylength` reads `1 + 0xC`. That is the
///    `EXCEPTION_ACCESS_VIOLATION ... read at address 0x000000000000000D`
///    signature of the `OffsetDateTimeTest` discovery crash.
///  * `VIRTUAL_TARGET_CACHE` holds the resolved dispatch CLASS NAME, so the
///    reused site resolves its own (correct) method name against the previous
///    site's class — surfacing as `NoSuchMethodError:
///    java.lang.Object.annotationType()` /
///    `java.lang.Class.annotationType()`, a real method name against a class
///    that never declared it.
///  * `OBJECT_NATIVE_DISPATCH_CACHE` / `INTEGER_NATIVE_DISPATCH_CACHE` hold the
///    same shape of decision for their own fast paths.
///
/// Only `DISPATCH_CACHE` and `VIRTUAL_DISPATCH_CACHE` were flushed here before,
/// because the hazard was framed as "a raw entry pointer can go stale". The
/// address-reuse hazard is broader: a memo does not have to hold a code pointer
/// to be wrong once its key stops identifying its site.
///
/// The JIT cache generation closes it exactly: publishing the new
/// `CompiledMethod` that owns the reused `JitInvokeInfo` is itself an
/// unconditional `JIT_CACHE_GENERATION.fetch_add` (`JitCache::put` /
/// `put_osr`), so an address can never be re-issued without a generation the
/// thread has not yet seen — and this runs before every consult.
///
/// Cost on the steady state is unchanged: two thread-local compares, and the
/// `clear()` calls only ever execute inside the changed-generation branch.
///
/// **Which memos a change drops** is [`site_memo_flush`]'s decision. Once
/// the generation word carries a retire count in its high half
/// ([`JIT_RETIRE_SHIFT`]), a publication that retired nothing drops only the
/// `publication_sensitive` memos, and the positive ones survive a compile
/// storm. The address-reuse argument above then rests on the retire bump in
/// `Drop for CompiledMethod`, which runs before the boxes are freed, instead
/// of on the next publication.
pub(crate) fn flush_raw_entry_dispatch_caches() {
    // Every compiled publication/invalidation advances this generation. Flush
    // the site-keyed memos before probing them, to pick up tier replacements,
    // to release their code owners after invalidation, and above all so a
    // recycled `JitInvokeInfo` address cannot inherit the previous site's
    // resolution.
    let generation = cratonvm_jit::jit_cache_generation();
    let previous = DISPATCH_CACHE_JIT_GENERATION.with(|seen| seen.replace(generation));
    match site_memo_flush(previous, generation, JIT_RETIRE_SHIFT) {
        SiteMemoFlush::Nothing => {}
        SiteMemoFlush::PublicationSensitive => clear_publication_sensitive_site_memos(),
        SiteMemoFlush::All => clear_site_keyed_dispatch_memos(),
    }
    // No supersede-epoch check here any more (interpreter round i1 wave 19).
    // It cleared the publication-sensitive memos a second time for an event
    // the generation above had already reported: the epoch's one bumper (the
    // C2 publish in `background_compile_task`) runs after `JitCache::put`,
    // whose unconditional publish-half bump is a `PublicationSensitive` flush
    // here, and a C2 body that REPLACED one marked the old body `superseded`
    // before that bump, so a positive memo on the old body is dropped at its
    // next hit (`DispatchCache::names_a_stale_body`). The epoch is per VM now
    // (`JitRealm::supersede_epoch`) and this function has no VM, so keeping
    // the check would have meant one VM's supersede clearing another VM's
    // memos, or a TLS read and an atomic load per dispatch for nothing.
}

/// `CRATONVM_DBG_SITE_ALIAS=1` — count of dispatch-helper entries whose `JitSiteKey` named a
/// DIFFERENT call site than the one that first used it — i.e. a `JitInvokeInfo`
/// address that was freed with its `CompiledMethod` and re-issued.
///
/// This measures the PRECONDITION of the recycled-address defect rather than
/// its (rare, workload-dependent) visible corruption, which is why it can
/// answer "does this workload alias?" in a single run.
static SITE_ALIAS_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SITE_ALIAS_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn site_alias_hit_count() -> u64 {
    SITE_ALIAS_HITS.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn site_alias_key_count() -> u64 {
    SITE_ALIAS_KEYS.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    /// `JitSiteKey -> the (class, method, descriptor) that key FIRST named`.
    /// Diagnostic-only; unbounded on purpose so nothing is missed.
    static SITE_IDENTITY: std::cell::RefCell<
        rustc_hash::FxHashMap<JitSiteKey, (String, String, String)>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Record/verify what this `JitSiteKey` names. Called at the
/// top of both dispatch entry points, so it is independent of WHICH memo a
/// given site would have consulted.
#[cold]
#[inline(never)]
fn note_site_identity(info_key: JitSiteKey, info: &JitInvokeInfo) {
    SITE_IDENTITY.with(|m| {
        let mut m = m.borrow_mut();
        match m.get(&info_key) {
            Some((c, mm, d)) => {
                if c != info.class_name || mm != info.method_name || d != info.descriptor {
                    SITE_ALIAS_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The running totals ride on the event line, not only on a
                    // shutdown summary: a JUnit runner ends the process with
                    // `System.exit`, so anything printed at VM shutdown is
                    // unreachable in exactly the workloads this exists for.
                    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 40 {
                        eprintln!(
                            "[site-alias] #{} of {} keys: key={:#x} WAS {}.{}{} NOW {}.{}{}",
                            n + 1,
                            SITE_ALIAS_KEYS.load(std::sync::atomic::Ordering::Relaxed),
                            info_key.1,
                            c,
                            mm,
                            d,
                            info.class_name,
                            info.method_name,
                            info.descriptor
                        );
                        if n + 1 == 40 {
                            eprintln!("[site-alias] (further hits are counted, not printed)");
                        }
                    }
                    m.insert(
                        info_key,
                        (
                            info.class_name.to_string(),
                            info.method_name.to_string(),
                            info.descriptor.to_string(),
                        ),
                    );
                }
            }
            None => {
                SITE_ALIAS_KEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                m.insert(
                    info_key,
                    (
                        info.class_name.to_string(),
                        info.method_name.to_string(),
                        info.descriptor.to_string(),
                    ),
                );
            }
        }
    });
}

fn site_alias_detect_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    // `runtime_var_os`, not `std::env::var_os`: this name IS declared
    // (`CRATONVM_DBG=site-alias`), and a raw read on a declared name is served
    // by a live `getenv` instead of the latched snapshot — so the grouped
    // spelling silently did nothing here, and `flags::with_thread_overrides`
    // could not arrange the probe in a test. Check 4 of
    // `tools/flag-census/check-surface.sh` names exactly this call site.
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SITE_ALIAS"))
}

/// May a callee that declares an exception table be published into the
/// machine-code MIC/PIC? **Yes, by default since 2026-08-17.**
///
/// The ban existed because the inline cascade in `jit/src/x64.rs` CALLs the
/// cached entry directly, so an `i64::MIN` deopt/exception sentinel from the
/// callee had no Rust frame to notice it and route it through the *callee's*
/// own exception table — it surfaced at the caller's epilogue as the caller's
/// own deopt.
///
/// That hole is closed. `Compiler::emit_inline_callee_deopt_check` is emitted
/// after **every** inline direct-entry CALL (both PIC slots and the MIC arm,
/// and the megamorphic hashed stub's `emit_callee_deopt_check` twin) and hands
/// the sentinel to `jit_service_callee_deopt`, a thin wrapper over the same
/// [`handle_compiled_callee_deopt_sentinel`] every helper arm uses. It landed
/// later, for the H2 `MVMap`/`DataType.read` case, and the ban was never
/// revisited against it.
///
/// **The mechanism is measured, not assumed.** `probes/CalleeExceptionTable/// SemanticsProbe.java` drives six exception-table callees (implicit AIOOBE /
/// NPE / divide, an explicit athrow, a table that does NOT cover what it
/// throws, and a `finally`) through a monomorphic interface site, with every
/// throwing call INSIDE the hot loop so it goes through the published cache.
/// Four arms, one binary:
///
/// | arm | result |
/// |---|---|
/// | HotSpot 25 (the oracle) | PASS |
/// | ban kept | PASS |
/// | ban lifted | PASS |
/// | ban lifted + `CRATONVM_JIT_SP_IC_DEOPT_CHECK=0`, **pre-interlock binary** | **FAIL 8/8 — `ArithmeticException` escapes `Div.apply`'s own `catch` to `main`** |
/// | the same arm on a binary carrying the interlock below | PASS 8/8 |
///
/// The fourth arm is the point: deleting the sentinel check is the only way to
/// make the lifted ban wrong, which is what says the check is what makes it
/// right. `SP_IC_DEOPT_CHECK` is therefore an INTERLOCK below, not a separate
/// knob — publishing while the check is suppressed is unsound, and `SkipVoid`
/// suppresses it for exactly the void callees whose return register carries no
/// value.
///
/// The fifth row is the interlock working, not the probe going blind, and the
/// difference is visible as a THROUGHPUT reading rather than a claim.
/// `NativeFunnelFloorProbe`'s try/catch rung under `MIC_EXC_TABLE_PUBLISH=1
/// SP_IC_DEOPT_CHECK=0`: **207.04 ns/op** with the interlock (nothing was
/// published — the banned reading) against **24.12 ns/op** without it
/// (published with no check — the unsound state the fourth arm catches).
/// Anyone re-running the red proof must do it on a binary that predates the
/// interlock, or they will read a refusal as a pass.
///
/// **What it buys.** `probes/NativeFunnelFloorProbe.java`, ABBA on one binary,
/// two interleaved rounds, ns/op:
///
/// | rung | ban kept | ban lifted |
/// |---|---:|---:|
/// | interface call, callee has no exception table (control) | 15.75 / 15.91 | 15.65 / 15.60 |
/// | interface call, callee has `try`/`catch` | **125.98 / 125.94** | **14.35 / 14.56** |
///
/// 8.7x, with the control rung unmoved — the whole Rust helper route measured
/// against the inline cascade the callee was barred from. The older note here
/// recorded the opposite ("indistinguishable") from `probes/LazyArmVariants.java`
/// at ten threads; that probe measures a ten-thread lock-contention shape where
/// the dispatch round trip is not the limiter, so it could not see this. Do not
/// re-derive the ban from it.
///
/// `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=0` restores the ban.
fn mic_publish_exception_table_callees() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        // INTERLOCK. `emit_inline_callee_deopt_check` is the entire reason
        // publishing is sound; if the codegen is not emitting it at every
        // direct-entry CALL, publishing must not happen. Pairing the two here
        // means a future reader cannot turn one off and leave the other on.
        if cratonvm_jit::sp_ic_deopt_check_mode() != cratonvm_jit::SpIcDeoptCheck::On {
            return false;
        }
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Opt-out for the Rust-level compiled-entry cache the virtual-MIC helper uses
/// for exception-table callees, so one binary can be A/B'd against itself.
fn mic_rust_entry_cache_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE")
    })
}

/// Call this site's compiled callee out of the per-thread entry cache the
/// virtual-MIC helper keeps for callees the machine-code caches are barred
/// from holding. `None` means no usable entry — resolve normally.
///
/// # Why this cache exists
///
/// A callee that declares an exception table can never be published into the
/// MIC or the PIC: the inline cascade in `jit/src/x64.rs` machine-CALLs the
/// raw entry, and nothing on that route can run
/// [`handle_compiled_callee_deopt_sentinel`] to send the callee's own trap
/// through the callee's own table. The MIC slot therefore never gets a class
/// id either, so every call to such a callee took the *cache-miss* arm — a
/// compile probe, an exception-table probe, and `invoke_or_native` — forever,
/// and the compiled body that was sitting in the JIT cache was never entered.
///
/// This map is a plain thread-local the inline cascade cannot see, so a hit
/// runs the compiled body *and* keeps the sentinel routing. It is the same
/// trade `jit_invoke_dispatch`'s virtual arm already makes for its own callees
/// (see the long note at its consult site).
///
/// Measured with `probes/LazyArmVariants.java`, whose V7/V8 pair differs only
/// by a never-taken `try`/`catch` in the delegate — the shape of
/// `CharsetCache.getCharset` — at ten threads: 8901 vs 44859 ns/op before,
/// with both delegates confirmed compiled either way.
///
/// SAFETY: same contract as the surrounding helper — `vm`/`info`/`args_slice`
/// are the live values the JIT caller passed, and `thread` is this thread's
/// exclusive borrow.
#[inline]
unsafe fn try_mic_rust_cached_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_ptr: i64,
    receiver_cid: u32,
    vm_ptr: i64,
    args_slice: &[i64],
    // jr20 wave 9 / gcE: `args_slice` is a forwarded Rust copy (in no root
    // set): root it across the call whatever the site flag says.
    args_forwarded: bool,
) -> Option<i64> {
    if !mic_rust_entry_cache_enabled() || !direct_virtual_compiled_callee_entry_enabled() {
        return None;
    }
    let key = (
        jit_site_key(vm.vm_identity, info_ptr as usize),
        receiver_cid,
    );
    // The owner comes out WITH the entry: it is the keep-alive this map already
    // holds for exactly this raw pointer, so cloning it here spares the callee a
    // code-range registry round trip per call. See
    // `try_call_compiled_entry_reentrant_owned`.
    // A superseded or invalidated body is dropped here and the call takes the
    // resolving path below, which finds the current one. See
    // `DispatchCache::names_a_stale_body`.
    let (entry, needs_ctx, owner, rerun_pins) = VIRTUAL_DISPATCH_CACHE.with(|dc| {
        let mut dc = dc.borrow_mut();
        if dc.get(&key).is_some_and(DispatchCache::names_a_stale_body) {
            dc.remove(&key);
            return None;
        }
        dc.get(&key)
            .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
    })?;
    // The sentinel below can re-run the callee at its handler with these
    // arguments, so they are rooted across the call AND its servicing, and
    // read back through the pins (see [`call_compiled_then_route`]; `thread`
    // is this helper's own borrow, so the pins go straight onto it).
    let pins = (rerun_pins || args_forwarded)
        .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
    let called = match owner.as_ref() {
        Some(owner) => {
            try_call_compiled_entry_reentrant_owned(owner, entry, needs_ctx, vm_ptr, args_slice)
        }
        None => try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
    };
    let result = called.map(|rc| {
        if rc == i64::MIN {
            if let Some(v) = handle_rust_armed_callee_sentinel(
                vm,
                thread,
                info,
                ClassId::new(receiver_cid),
                args_slice,
                pins.as_ref(),
            ) {
                return v;
            }
        }
        rc
    });
    if let Some(pins) = pins {
        pins.release(thread);
    }
    result
}

/// Publish a compiled entry into the cache [`try_mic_rust_cached_entry`] reads.
///
/// `pin_jit_entry` is the only keep-alive for the raw pointer stored here, so
/// a failure to pin simply means the entry is not cached — never a dangling
/// one.
///
/// `rerun_pins` is what [`try_mic_rust_cached_entry`] roots the arguments
/// with across the call. The two table-barred callers pass `true` (their
/// callee declares an exception table, so its sentinel can re-run it). The
/// no-room caller in the miss arm (round 11 wave 18, lane hashmap) passes the
/// site's sticky `callee_rerun_pins` flag, which is set before any table
/// callee is published at the site, so it is exact or conservative, never
/// short.
fn publish_mic_rust_cached_entry(
    vm_identity: usize,
    info_ptr: i64,
    receiver_cid: u32,
    entry_ptr: usize,
    needs_ctx: bool,
    rerun_pins: bool,
) {
    if !mic_rust_entry_cache_enabled() || entry_ptr == 0 {
        return;
    }
    let Some(owner) = cratonvm_jit::pin_jit_entry(entry_ptr) else {
        return;
    };
    VIRTUAL_DISPATCH_CACHE.with(|dc| {
        dc.borrow_mut().insert(
            (jit_site_key(vm_identity, info_ptr as usize), receiver_cid),
            DispatchCache {
                entry: entry_ptr,
                needs_context: needs_ctx,
                owner: Some(std::rc::Rc::new(owner.into())),
                rerun_pins,
            },
        );
    });
}

/// `CRATONVM_JIT_MEGA_DISPATCH_TABLE` (default on; `0` neither publishes into
/// nor reads the VM's shared megamorphic dispatch table,
/// `cratonvm_jit::MegaDispatchTable`, round 12 wave 2, lane calls, W2-1).
/// Cached once per process: it only selects between two correct routes.
fn mega_dispatch_table_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_DISPATCH_TABLE")
    })
}

/// An inline-cache install was refused only because a retired way's grace
/// lags: ask for an inline-cache grace handshake if this VM's switch is on
/// (`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`, default off, read once per VM by its
/// megamorphic table) and it has not asked within the interval. JIT round 13
/// wave 11 lane mega9's patch (`r13w11-mega9-ic-grace-handshake-patch-FIXED-20260929.md`),
/// applied in round 14 wave 1 by lane codecache. Called on a mutator inside a
/// dispatch helper, past every probe it made, so the thread itself is outside
/// every inline-cache window. `initiator` is the caller's thread id (`None`:
/// unknown, no request).
fn request_ic_grace_if_due(vm: &SharedVm, initiator: Option<crate::ThreadId>) {
    let Some(initiator) = initiator else {
        return;
    };
    if !vm.jit.jit_cache.mega_dispatch_table().claim_grace_request() {
        return;
    }
    let _ = crate::runtime::interpreter::request_ic_grace_handshake(vm, initiator);
}

/// Publish the body a resolving arm of `jit_invoke_virtual_mic` just put into
/// its site's PIC into the VM's shared megamorphic table as well, under the
/// selector of the site's call record (bound on the site's [`JitPICSlot`] the
/// first time), so every other site that resolves the same method finds it
/// (round 12 wave 2, lane calls, proposal W2-1 step 1).
///
/// The caller has already applied the PIC's own admission (a cacheable,
/// plain-object receiver, a body compiled for its class id, no indy trap, not
/// barred, not forbidden). On top of that the publication is withdrawn when a
/// redefinition landed after `epoch_seen` (as
/// `withdraw_ic_publication_if_redefined` does for the site's own caches).
///
/// A callee that declares an exception table is published exactly when the
/// site's own PIC takes it (`mic_publish_exception_table_callees`, round 12
/// wave 3, lane mega2). Wave 2 refused every one, because only the PUBLISHING
/// site's MIC records that its arms must root the arguments of a callee its
/// sentinel can re-run (`JitMICSlot::callee_rerun_pins`). The table's two
/// readers do not need that record: [`try_mega_dispatch_table_entry`] roots the
/// arguments on every hit, and the hashed stub's machine-code probe services a
/// sentinel exactly as the per-site hashed ways it shares its tail with. The
/// refusal only sent every such receiver past a site's own sixteen ways to the
/// helper, on every call. `CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES=0` restores it.
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live and `pic` is this site's.
unsafe fn publish_mega_dispatch_entry(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_cid: u32,
    entry_ptr: u64,
    needs_ctx: bool,
    epoch_seen: u32,
) {
    if !mega_dispatch_table_enabled() || entry_ptr == 0 {
        return;
    }
    if !(mega_table_exc_callees_enabled() && mic_publish_exception_table_callees())
        && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info)
    {
        return;
    }
    install_mega_dispatch_way(
        vm,
        info,
        Some(pic),
        receiver_cid,
        entry_ptr,
        needs_ctx,
        epoch_seen,
    );
}

/// `CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES` (default on; `0` keeps every callee
/// that declares an exception table out of the shared megamorphic table, the
/// round-12-wave-2 rule). See [`publish_mega_dispatch_entry`]. Cached once
/// per process, like [`mega_dispatch_table_enabled`]: it selects between two
/// correct routes.
fn mega_table_exc_callees_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES")
    })
}

/// `CRATONVM_JIT_MEGA_TABLE_PUBLISH_LAMBDA` (default on; `0` leaves lambda
/// adapter thunks in their SAM site's own inline caches only): proposal W2-1
/// step 3, round 12 wave 3, lane mega2. See `install_lambda_inline_cache`.
fn mega_table_publish_lambda_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_PUBLISH_LAMBDA")
    })
}

/// `CRATONVM_JIT_MEGA_TABLE_PUBLISH_DISPATCH` (default on; `0` has
/// `jit_invoke_dispatch`'s virtual arm fill only its per-thread memo, as
/// before): proposal W2-1 step 3, round 12 wave 3, lane mega2.
fn mega_table_publish_dispatch_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_PUBLISH_DISPATCH")
    })
}

/// `CRATONVM_JIT_MEGA_SELECTOR_BY_LOADER` (default on; `0` keys a site's
/// shared-table selector on its caller's CLASS id, the round-12-wave-3 key):
/// proposal M3-2, round 12 wave 4, lane mega3. See [`mega_selector_context`].
/// Cached once per process, like its neighbours: both keys are sound, the
/// loader key only shares more.
fn mega_selector_by_loader_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_SELECTOR_BY_LOADER")
    })
}

/// `CRATONVM_JIT_MEGA_CLASS_SLOTS` (default on; `0` binds no column and
/// publishes into no per-class cell, so the hashed stub's class-slot probe
/// finds nothing and falls through, exactly the round-12-wave-3 behaviour):
/// proposal M3-1, round 12 wave 4, lane mega3. See [`mega_class_slot_plan`].
fn mega_class_slots_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_CLASS_SLOTS")
    })
}

/// `CRATONVM_JIT_MEGA_CLASS_SLOTS_IFACE` (default on; `0` gives
/// `invokeinterface` sites no column, as `invokevirtual`-only M3-1 did): an
/// interface site's column is the slot of the method in its FIRST publishing
/// receiver's own vtable (proposal W4-1, round 12 wave 4, lane mega3). See
/// [`mega_class_slot_plan`].
fn mega_class_slots_iface_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_CLASS_SLOTS_IFACE")
    })
}

/// The resolution context a site's selector is interned under (proposal M3-2):
/// its caller's DEFINING LOADER, where the round-12-wave-3 key used the
/// caller's class id.
///
/// Why this is the same resolution. Between `jit_invoke_virtual_mic`'s entry
/// and a publication, the helper reads `info.declaring_class_id` in exactly
/// one place, [`jit_site_owner`] (called by [`receiver_selection_retarget`],
/// [`select_for_jit_receiver`] and so by `mic_callee_has_exception_table`),
/// and there only as `find_class_by_name_for_class(info.class_name, caller)`,
/// which is `find_class_by_name_for_loader(name, get_loader_id(caller))`. The
/// compile probe, the dispatch target (`virtual_dispatch_target_for_receiver`)
/// and the indy screen are functions of the receiver and the constant-pool
/// names. `jit_invoke_dispatch`'s virtual arm and the lambda-thunk arm publish
/// through the same resolution. Access checks, the one other input a caller
/// class could contribute, fail (and publish nothing) before a site binds, and
/// a site binds only through its own successful publication. Loader ids come
/// from a process-wide monotonic counter and are never reused, so a selector
/// never outlives the namespace it names; a class id context stays for a site
/// with no caller (`0`, the synthetic sites) or whose caller has no loader
/// record.
fn mega_selector_context(vm: &SharedVm, info: &JitInvokeInfo) -> u64 {
    let caller = info.declaring_class_id;
    if caller == 0 || !mega_selector_by_loader_enabled() {
        return u64::from(caller);
    }
    let loader = vm
        .classes
        .class_manager
        .read()
        .get_loader_id(ClassId::new(caller));
    match loader {
        Some(loader) => cratonvm_jit::mega_selector_loader_context(loader.to_native_id()),
        None => u64::from(caller),
    }
}

/// The `(column, columns)` of `receiver_cid`'s cell for this site in the
/// shared table's per-class arrays (proposal M3-1), or `None`. For an
/// `invokevirtual` site the column is its resolved method's vtable slot plus
/// one, looked up once in the vtable of the class the site's constant pool
/// names (through the caller's loader, [`jit_site_owner`]). An
/// `invokeinterface` site has no single slot across its receivers; it takes
/// the slot of the method in its FIRST publishing receiver's own vtable
/// (W4-1), which is every receiver's slot in the common shape of
/// implementations that add the method at one depth under one base, and only
/// with a PIC to bind it on (a blind site's receivers would each pick their
/// own). Either way the column is then bound on the site's PIC; `columns` is
/// the receiver's vtable length, which sizes its array the first time one is
/// made.
///
/// The column is an INDEX, never a target: a cell is called only when its key
/// -- this site's selector and the receiver's class id -- matches
/// (`cratonvm_jit::MegaDispatchTable`, "Per-class columns"), so a vtable slot
/// that does not describe JVMS 5.4.6 selection (package-private overriding
/// across packages, a slot moved by a redefinition, an interface default, a
/// receiver that declares the interface method at another slot) can only cost
/// a miss.
///
/// The selector must already be bound on `pic` (the column is stored after it).
///
/// Takes the class-manager and vtable-manager read locks one after the other,
/// never together and never under a JIT lock; a publication is cold.
fn mega_class_slot_plan(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    table: &cratonvm_jit::MegaDispatchTable,
    pic: Option<&JitPICSlot>,
    receiver_cid: u32,
) -> Option<(u32, u32)> {
    let virtual_site = info.invoke_kind == 0;
    let interface_site = info.invoke_kind == 2 && pic.is_some() && mega_class_slots_iface_enabled();
    if !mega_class_slots_enabled() || !(virtual_site || interface_site) {
        return None;
    }
    let bound = pic.map_or(0, JitPICSlot::mega_class_slot);
    let owner = if bound == 0 && virtual_site {
        let cm = vm.classes.class_manager.read();
        Some(jit_site_owner(&cm, info)?)
    } else {
        None
    };
    // Round 15 wave 3 (lane mega2, MG15-5): with
    // `CRATONVM_JIT_MEGA_IFACE_SELECTOR_COLUMN` on, an interface site binds the
    // column its selector's first columned site bound (off: always `None`, and
    // the site binds this receiver's own slot, W4-1). An index, never a target.
    let selector = pic.map_or(0, JitPICSlot::mega_selector);
    // JIT round 21 wave 4, lane ic (IC21-3, `CRATONVM_JIT_MEGA_IFACE_COLUMN_SPACE`):
    // or the selector's own interface-range column, which every receiver of
    // the selector shares (`None` with that switch off, as before).
    let recorded = if bound == 0 && interface_site {
        table
            .recorded_interface_column(selector)
            .or_else(|| {
                let issued = table.interface_space_column(selector);
                // Tier-1 round wave 8, lane IC: IC21-3's census row.
                if issued.is_some() {
                    disp_census::note(disp_census::MIC_IFACE_SPACE_ISSUED);
                }
                issued
            })
    } else {
        None
    };
    let (column, columns) = {
        let vtables = vm.classes.vtable_manager.read();
        let receiver_vtable = vtables.get_vtable(u64::from(receiver_cid));
        let column = if bound != 0 {
            bound
        } else if let Some(recorded) = recorded {
            recorded
        } else {
            let slot = match owner {
                Some(owner) => vtables
                    .get_vtable(u64::from(owner.as_u32()))?
                    .lookup_slot(info.method_name, info.descriptor)?,
                // An interface site: the first receiver's own slot.
                None => receiver_vtable?.lookup_slot(info.method_name, info.descriptor)?,
            };
            u32::try_from(slot).ok()?.checked_add(1)?
        };
        let columns =
            receiver_vtable.map_or(0, |vtable| u32::try_from(vtable.len()).unwrap_or(u32::MAX));
        (column, columns)
    };
    // Outside the vtable lock: the first site of the selector records its
    // column, a racing one adopts it.
    let column = if bound == 0 && interface_site && recorded.is_none() {
        table.selector_interface_column(selector, column)
    } else {
        column
    };
    // Round 17 wave 2 (lane mega2): publish at the column the site is bound to
    // AFTER the bind, which is a racing first binder's when it won
    // (`JitPICSlot::bind_mega_class_slot`, `CRATONVM_JIT_MEGA_CLASS_SLOT_CAS`).
    // For a receiver that declares the method at another depth the cell is then
    // refused (`PastArray` / `Taken`) as a later call's would be.
    let column = if bound == 0 {
        match pic {
            Some(pic) => match pic.bind_mega_class_slot(table, column) {
                0 => column,
                won => won,
            },
            None => column,
        }
    } else {
        column
    };
    Some((column, columns))
}

/// JIT round 21 wave 4, lane ic (IC21-2, `CRATONVM_JIT_MEGA_VTABLE_COLUMN_BAKED`;
/// `CompileRequest::mega_site_binder`): bind a fresh `invokevirtual` site's
/// slot, at COMPILE time, to the shared-table selector and column its first
/// publication would bind -- the selector interned from exactly the inputs
/// [`install_mega_dispatch_way`] interns ([`mega_selector_context`] and the
/// info's own fields), the column [`mega_class_slot_plan`]'s for a virtual
/// site (the resolved method's slot in the site owner's vtable, plus one) --
/// so the emitter can bake them as immediates.
///
/// One thing the compile-time bind changes: the site is bound before any of
/// its own resolutions has SUCCEEDED, and a bound site's code reads the cells
/// every site of its selector publishes. Today a site whose resolution fails
/// its access check never publishes, so never binds, so never reads another
/// site's cells. The selector key names the caller's LOADER, not its class,
/// and access is a property of the caller's class (and module), so the bind
/// is made only where access is certain for this caller
/// ([`mega_bake_access_is_certain`]); every other site keeps the
/// first-publication bind. Binds nothing for a site with no caller, a
/// substituted owner, a bound-site slot, or a slot already bound.
pub(crate) fn bake_mega_virtual_site(vm: &SharedVm, info: &JitInvokeInfo, pic: &JitPICSlot) {
    if info.invoke_kind != 0
        || info.declaring_class_id == 0
        || info.owner_class_id != 0
        || !mega_dispatch_table_enabled()
        || !mega_class_slots_enabled()
        || pic.is_bound_site()
        || pic.mega_selector() != 0
        || pic.mega_class_slot() != 0
    {
        return;
    }
    let owner = {
        let cm = vm.classes.class_manager.read();
        let Some(owner) = jit_site_owner(&cm, info) else {
            return;
        };
        if !mega_bake_access_is_certain(&cm, ClassId::new(info.declaring_class_id), owner, info) {
            return;
        }
        owner
    };
    let column = {
        let vtables = vm.classes.vtable_manager.read();
        let Some(slot) = vtables
            .get_vtable(u64::from(owner.as_u32()))
            .and_then(|vtable| vtable.lookup_slot(info.method_name, info.descriptor))
        else {
            return;
        };
        let Some(column) = u32::try_from(slot).ok().and_then(|s| s.checked_add(1)) else {
            return;
        };
        column
    };
    let table = vm.jit.jit_cache.mega_dispatch_table();
    let Some(selector) = table.selector_id_in_context(
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        mega_selector_context(vm, info),
        info.owner_class_id,
    ) else {
        return;
    };
    pic.bind_mega_dispatch(table, selector);
    if pic.mega_selector() == selector {
        pic.bind_mega_class_slot(table, column);
        // Tier-1 round wave 8, lane IC: IC21-2's engagement row (the emitter
        // bakes only a binding this left whole).
        if pic.baked_column_binding().is_some() {
            disp_census::note(disp_census::MIC_BAKED_BIND);
        }
    }
}

/// [`bake_mega_virtual_site`]'s access rule: may the class `caller` link an
/// `invokevirtual` of `info`'s method on the class `owner` its constant pool
/// names, whatever happens at run time? Only when `owner` is a class (not an
/// interface) in the caller's own RUNTIME package (same defining loader, same
/// package name, hence the same module), and the method it resolves to is an
/// instance, non-private method that is public or declared in that same
/// runtime package (package-private and protected members of one's own
/// runtime package are accessible). Anything else -- another package, another
/// loader, a protected member across packages, an unresolvable reference --
/// answers `false` and keeps the first-publication bind.
fn mega_bake_access_is_certain(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    owner: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let store = cm.class_store();
    let package = |name: &str| -> String {
        name.rsplit_once('/').map_or(String::new(), |(p, _)| p.to_string())
    };
    let same_runtime_package = |a: ClassId, b: ClassId| -> bool {
        let (Some(ca), Some(cb)) = (store.get(a), store.get(b)) else {
            return false;
        };
        let (Some(la), Some(lb)) = (cm.get_loader_id(a), cm.get_loader_id(b)) else {
            return false;
        };
        la.to_native_id() == lb.to_native_id() && package(&*ca.name) == package(&*cb.name)
    };
    if store.get(owner).map_or(true, |c| c.is_interface()) || !same_runtime_package(caller, owner) {
        return false;
    }
    let Some(declaring) = crate::runtime::resolve::selection::resolve_declaring(
        store,
        owner,
        info.method_name,
        info.descriptor,
    ) else {
        return false;
    };
    let Some(method) = store
        .get(declaring)
        .and_then(|c| c.find_method(info.method_name, info.descriptor))
    else {
        return false;
    };
    if method.is_static() || method.access_flags.contains(MethodAccessFlags::PRIVATE) {
        return false;
    }
    method.access_flags.contains(MethodAccessFlags::PUBLIC) || same_runtime_package(caller, declaring)
}

/// Install `(receiver class, the site's selector) -> entry` into the VM's
/// shared megamorphic table, and withdraw it at once if a redefinition moved
/// the epoch since `epoch_seen` (the publisher's half of the table's
/// redefinition rule: `bump_redefine_epoch` empties the table AFTER the bump,
/// so a publication either lands before that clear or sees the new epoch
/// here). A `pic` memoizes the selector and binds the site to the table, which
/// is what lets its hashed stub probe the table in machine code; with none
/// (a blind `jit_invoke_dispatch` site) the selector is interned per
/// publication, which only the cold fills below make.
///
/// Every admission is the caller's.
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live, and `pic`, if any, is the
/// site's own.
#[allow(clippy::too_many_arguments)]
unsafe fn install_mega_dispatch_way(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: Option<&JitPICSlot>,
    receiver_cid: u32,
    entry_ptr: u64,
    needs_ctx: bool,
    epoch_seen: u32,
) {
    if !mega_dispatch_table_enabled() || entry_ptr == 0 {
        return;
    }
    let table = vm.jit.jit_cache.mega_dispatch_table();
    let bound = pic.map_or(0, JitPICSlot::mega_selector);
    let selector = if bound != 0 {
        bound
    } else {
        // Round 12 wave 4 (lane mega3, M3-2): under the caller's loader, not
        // its class, so every site of the method in that namespace shares it.
        let Some(selector) = table.selector_id_in_context(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
            mega_selector_context(vm, info),
            info.owner_class_id,
        ) else {
            return;
        };
        if let Some(pic) = pic {
            pic.bind_mega_dispatch(table, selector);
        }
        selector
    };
    // Round 12 wave 4 (lane mega3, M3-1): and the receiver's per-class cell at
    // the site's column, which the hashed stub reads by direct index.
    let class_slot = mega_class_slot_plan(vm, info, table, pic, receiver_cid);
    // t1 wave 5 (lane V4): the shared table's install is an inline-cache
    // publication too (`jit_entry_publishable`), and the blind dispatch
    // helper's arm (`dispatch.rs`) reaches it with no scope open; without one
    // its `JdkOnly` refusal fell into the process sink no VM's report reads.
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    let report = table.install_with_class_slot(
        receiver_cid,
        selector,
        class_slot,
        entry_ptr,
        needs_ctx,
        jdk_only,
    );
    // Round 15 wave 3 (lane mega2): the census of MG15-3 (ways only a class
    // cell holds) and of W4-1a / MG15-5 (interface cells another key holds).
    if report.cell && !report.hashed {
        disp_census::note(disp_census::MIC_CELL_ONLY_PUBLISH);
    }
    if report.cell_taken && info.invoke_kind == 2 {
        disp_census::note(disp_census::MIC_IFACE_CELL_TAKEN);
    }
    // Round 16 (lane mega, MG2-2 step 1): the two silent cell refusals.
    if report.cell_budget_refused {
        disp_census::note(disp_census::MIC_CELL_BUDGET_REFUSED);
    }
    if report.cell_past_array {
        disp_census::note(disp_census::MIC_CELL_PAST_ARRAY);
    }
    // Tier-1 round wave 8, lane IC: IC21-1's engagement row.
    if report.cell_grown {
        disp_census::note(disp_census::MIC_CELL_GROWN);
    }
    if vm.jit.jit_cache.redefine_epoch() != epoch_seen {
        // Cast: an entry address.
        table.retire_entry(entry_ptr as usize);
    }
}

/// Round 14 wave 7 (lane mega): the shape-4 census of
/// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`. At a site
/// whose PIC has no bound selector, [`try_mega_dispatch_table_entry`] reads
/// nothing; count the calls for which the table already held a way under the
/// selector the site WOULD bind (`disp_census::MIC_UNBOUND_TABLE_HELD`), i.e.
/// the full resolutions a compile-time bind (M3-5) would save. Only under
/// `CRATONVM_DBG=mic-prof`, and it binds and interns nothing (a peek), so the
/// arm it measures runs exactly as with the census off.
///
/// # Safety
/// Since round 15 wave 3 it also reads a class cell without a lock
/// (`MegaDispatchTable::lookup_site_class_cell`): the caller must be the MIC
/// helper, inside compiled code past its entry quiescent stamp.
unsafe fn note_unbound_selector_table_held(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_cid: u32,
) {
    if !mic_prof::enabled() || !mega_dispatch_table_enabled() || pic.mega_selector() != 0 {
        return;
    }
    let Some(table) = vm.jit.jit_cache.mega_dispatch_table_if_any() else {
        return;
    };
    let Some(selector) = table.interned_selector_in_context(
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        mega_selector_context(vm, info),
        info.owner_class_id,
    ) else {
        return;
    };
    // Round 15 wave 3 (lane mega2, MG15-3): a way whose hashed set was full
    // lives only in the receiver's class cell; count it too, at the column
    // the site would read (the census undercounted exactly those calls).
    let held = table.lookup(receiver_cid, selector).is_some() || {
        let column = mega_census_column(vm, info, table, selector, receiver_cid);
        // SAFETY: called only from `jit_invoke_virtual_mic_body`'s megamorphic
        // arm, entered from compiled code past the handler's entry quiescent
        // stamp, and the read neither stamps, polls nor blocks.
        unsafe { table.lookup_site_class_cell(receiver_cid, selector, column) }.is_some()
    };
    if held {
        disp_census::note(disp_census::MIC_UNBOUND_TABLE_HELD);
    }
}

/// The column an UNBOUND site would read `receiver_cid`'s class cell at,
/// without binding or interning anything (round 15 wave 3, lane mega2; the
/// census of [`note_unbound_selector_table_held`] only, under mic-prof): what
/// [`mega_class_slot_plan`] would bind -- the resolved method's slot in the
/// site owner's vtable for `invokevirtual`, the selector's recorded column
/// (MG15-5) or else this receiver's own slot for `invokeinterface` -- plus
/// one; `0` when there is none. Takes the class-manager and vtable-manager
/// read locks one after the other.
fn mega_census_column(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    table: &cratonvm_jit::MegaDispatchTable,
    selector: u32,
    receiver_cid: u32,
) -> u32 {
    if !mega_class_slots_enabled() {
        return 0;
    }
    let owner = match info.invoke_kind {
        0 => {
            let cm = vm.classes.class_manager.read();
            match jit_site_owner(&cm, info) {
                Some(owner) => owner.as_u32(),
                None => return 0,
            }
        }
        2 if mega_class_slots_iface_enabled() => {
            if let Some(recorded) = table.recorded_interface_column(selector) {
                return recorded;
            }
            receiver_cid
        }
        _ => return 0,
    };
    let vtables = vm.classes.vtable_manager.read();
    vtables
        .get_vtable(u64::from(owner))
        .and_then(|vtable| vtable.lookup_slot(info.method_name, info.descriptor))
        .and_then(|slot| u32::try_from(slot).ok())
        .and_then(|slot| slot.checked_add(1))
        .unwrap_or(0)
}

/// The high bit of a `VIRTUAL_DISPATCH_COUNTER` value: this key was already
/// set to probe once by [`blind_dispatch_late_probe_rewind`] in this memo
/// generation. The blind arm masks it off before comparing the count (round 16
/// wave 17, lane mega5).
const BLIND_LATE_PROBE_MARK: u32 = 1 << 31;

/// `CRATONVM_JIT_BLIND_DISPATCH_LATE_PROBE=1` (round 16 wave 17, lane mega5;
/// **default OFF**, unmeasured): shape 6 of
/// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`, its last
/// warm-up round trip. A blind virtual key probes for a compiled callee when
/// its per-thread miss count reaches `jit_invocation_threshold` (or a doubling
/// of it, [`blind_dispatch_spent_reprobe`]). The count lives in
/// `VIRTUAL_DISPATCH_COUNTER`, a `publication_sensitive` memo: every
/// publication in the VM clears it, so during warm-up, while compiles keep
/// landing, a key keeps restarting from zero and may not reach its threshold
/// for a long time. Every call in between misses the memo, finds
/// `try_jit_virtual_bytecode_callee`'s template standing down for a callee
/// that IS compiled, and takes the by-name tail (`jit_dispatch_virtual_tail`
/// -> `invoke_or_native`). That stand-down is the evidence the probe waits
/// for: this sets the key's counter to `threshold - 1` so its NEXT call runs
/// the threshold's own probe, which then only has to find the published body.
/// Once per key per memo generation (the mark, which the same clears drop), so
/// a probe that still answers nothing (a wrapped or foreign body) costs at
/// most one probe per publication. Only the blind arm's own template call
/// (`blind`) sets it; the MIC's template calls count nothing here.
fn blind_dispatch_late_probe_rewind(key: (JitSiteKey, u32), blind: bool) {
    if !blind || !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_BLIND_DISPATCH_LATE_PROBE") {
        return;
    }
    let threshold = crate::runtime::env_cache::jit_invocation_threshold();
    if blind_late_probe_rewind_counter(key, threshold) {
        disp_census::note(disp_census::DISP_BLIND_LATE_PROBE);
    }
}

/// The counter half of [`blind_dispatch_late_probe_rewind`], switch-free so a
/// unit test can drive it: `true` when the key was set to probe.
fn blind_late_probe_rewind_counter(key: (JitSiteKey, u32), threshold: u32) -> bool {
    if threshold == 0 || threshold >= BLIND_LATE_PROBE_MARK {
        return false;
    }
    VIRTUAL_DISPATCH_COUNTER.with(|dc| {
        let mut counts = dc.borrow_mut();
        let raw = counts.entry(key).or_insert(0);
        if *raw & BLIND_LATE_PROBE_MARK != 0 {
            return false;
        }
        *raw = (threshold - 1) | BLIND_LATE_PROBE_MARK;
        true
    })
}

/// Round 16 wave 8 (lane mega):
/// `r16-mega-blind-dispatch-spent-key-never-reprobes-20261001`. The blind
/// virtual arm of [`jit_invoke_dispatch_body`] probes for a `(site, receiver
/// class)` key's callee once, when the key's per-thread miss count EQUALS the
/// tier-up threshold. A probe that answers no body (the callee's own
/// background compile, queued at the same invocation count, has not landed)
/// is never repeated: the count only grows past the threshold, so every later
/// call of that key on this thread is a generic call for the thread's life,
/// although the body is published moments later. `R16MegaBlindSites` reads
/// a fifth of its steady-state calls on that tail.
///
/// Called on a memo miss with the key's miss count after this one. Past the
/// threshold it counts the miss (`disp_census::DISP_BLIND_SPENT_MISS`) and,
/// under `CRATONVM_JIT_BLIND_DISPATCH_REPROBE` (default ON since the round-16
/// wave-8 A/B, `4bb285359b`; `=0` kills it), answers `true` at every doubling
/// of the threshold (2x, 4x, 8x, ...): at most 23 more probes per key over a
/// `u32` count, each one the probe the threshold ran. The switch is read only
/// at those counts, so it costs no static and no read on any other call.
/// (Round 17 wave 1, lane mega: this doc sat above `BLIND_LATE_PROBE_MARK`,
/// which round 16 wave 17 inserted between it and this function, and said
/// "default OFF".)
fn blind_dispatch_spent_reprobe(count: u32, threshold: u32) -> bool {
    if threshold == 0 || count <= threshold {
        return false;
    }
    disp_census::note(disp_census::DISP_BLIND_SPENT_MISS);
    if count % threshold != 0 || !(count / threshold).is_power_of_two() {
        return false;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_BLIND_DISPATCH_REPROBE") {
        return false;
    }
    disp_census::note(disp_census::DISP_BLIND_REPROBE);
    true
}

/// Round 16 wave 8 (lane mega): shape 6 of
/// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips` (proposal
/// MG2-4 of `jit-r15w3-mega2-proposals-RETIRED-20261003.md`, with MG7-3's census). The blind
/// virtual arm of [`jit_invoke_dispatch_body`] (an IR site with no inline
/// cache) memoizes a compiled callee per `(site, receiver class)` in
/// `VIRTUAL_DISPATCH_CACHE`, per thread, and probes for one only once a key
/// has missed `jit_invocation_threshold` times: until then every call is a
/// generic `invoke_virtual`, even when the body was compiled long ago and the
/// VM's shared megamorphic table already holds it for this receiver under
/// the selector every site of the method in this loader shares. A fresh
/// thread, or a recompiled caller, pays that warm-up again per receiver.
///
/// This peeks the table on such a miss (`count`, the key's miss count after
/// this one, below `threshold`). When it holds a way, the miss is counted
/// (`disp_census::DISP_BLIND_TABLE_HELD`) and, under
/// `CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT=1`
/// ([`cratonvm_jit::MegaDispatchTable::blind_dispatch_hint_enabled`]), the
/// key's counter is moved to `threshold` and `true` answered: the caller runs
/// the probe its threshold would have run, now. The table only cues the
/// probe; what is called is what the probe answers, with every admission the
/// threshold path applies (receiver-derived target, the receiver's own class
/// id, `pin_jit_entry`), so a stale or foreign way can cost one early probe,
/// never a wrong callee. A key whose early probe answers nothing is past its
/// threshold, as after a failed threshold probe: one probe per key either way.
///
/// Free when off: without the switch and without `CRATONVM_DBG=mic-prof` it
/// returns before it reads anything. With either, the peek interns nothing
/// (`MegaDispatchTable::peek_interned_way`) and reads a class cell only at the
/// column the site would bind ([`mega_census_column`]).
///
/// # Safety
/// The class-cell read has [`cratonvm_jit::MegaDispatchTable::lookup_site_class_cell`]'s
/// contract: the caller is `jit_invoke_dispatch_body`, entered from compiled
/// code past its entry quiescent stamp, and the read neither stamps, polls nor
/// blocks.
unsafe fn blind_dispatch_table_hint(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    key: (JitSiteKey, u32),
    receiver_cid: u32,
    count: u32,
    threshold: u32,
) -> bool {
    if count >= threshold || !mega_dispatch_table_enabled() {
        return false;
    }
    let Some(table) = vm.jit.jit_cache.mega_dispatch_table_if_any() else {
        return false;
    };
    let hint = table.blind_dispatch_hint_enabled();
    if !hint && !mic_prof::enabled() {
        return false;
    }
    let Some((selector, hashed)) = table.peek_interned_way(
        receiver_cid,
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        mega_selector_context(vm, info),
        info.owner_class_id,
    ) else {
        return false;
    };
    // A way whose hashed set was full lives only in the receiver's class cell
    // (MG15-3), at the column a columned site of this selector reads.
    let held = hashed || {
        let column = mega_census_column(vm, info, table, selector, receiver_cid);
        // SAFETY: the caller's contract (see above).
        unsafe { table.lookup_site_class_cell(receiver_cid, selector, column) }.is_some()
    };
    if !held {
        return false;
    }
    disp_census::note(disp_census::DISP_BLIND_TABLE_HELD);
    if !hint {
        return false;
    }
    VIRTUAL_DISPATCH_COUNTER.with(|dc| {
        dc.borrow_mut().insert(key, threshold);
    });
    disp_census::note(disp_census::DISP_BLIND_TABLE_HINT);
    true
}

/// Round 15 wave 2 (lane mega): proposal MG7-1 of `jit-r14-mega-proposals-RETIRED-20260930.md`,
/// shape 4 of `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`.
/// Called by the resolving arms of `jit_invoke_virtual_mic_body` once a
/// receiver-derived target resolved (`cacheable_receiver`: no selection
/// error, the receiver's own class): bind the site's shared-table selector
/// NOW instead of at its first publication
/// (`MegaDispatchTable::bind_site_at_resolution`, switch
/// `CRATONVM_JIT_MEGA_BIND_AT_RESOLUTION`, default off). A site whose first
/// receivers publish nothing (a declined or memo-skipped compile probe, a
/// barred callee) then reads the ways every other site of the method filled,
/// in the hashed stub and in [`try_mega_dispatch_table_entry`].
///
/// The binding argument is [`mega_selector_context`]'s: the selector is
/// interned under the same context a publication would use, and a
/// publication's bind is preceded by exactly this resolution (the compile
/// probe between the two reads only the receiver). An `invokevirtual` site
/// also binds its column, which is the resolved method's slot in the site
/// OWNER's vtable and so the column its first publication would bind; an
/// `invokeinterface` site keeps W4-1's rule (the first PUBLISHING receiver's
/// slot) and binds its column at that publication, as before.
///
/// A table nobody allocated holds nothing to read, so none is made here: the
/// site binds at a later resolution or at its own publication.
fn bind_mega_selector_at_resolution(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_cid: u32,
) {
    if pic.mega_selector() != 0 || !mega_dispatch_table_enabled() {
        return;
    }
    let Some(table) = vm.jit.jit_cache.mega_dispatch_table_if_any() else {
        return;
    };
    let selector = table.bind_site_at_resolution(
        pic,
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        || mega_selector_context(vm, info),
        info.owner_class_id,
    );
    if selector == 0 {
        return;
    }
    disp_census::note(disp_census::MIC_BOUND_AT_RESOLUTION);
    if info.invoke_kind == 0 {
        // The selector is bound above, so the column may follow it.
        let _ = mega_class_slot_plan(vm, info, table, Some(pic), receiver_cid);
    }
}

/// The megamorphic arm's last resort before the full resolution: the body the
/// VM's shared megamorphic table holds for `(receiver class, this site's
/// selector)`, called with its retained owner (round 12 wave 2, lane calls,
/// proposal W2-1 step 1). `None`: nothing was called.
///
/// Every hit is re-validated before the call -- the owner neither superseded
/// nor retired, no redefinition since `epoch_seen` -- so a table way an
/// invalidation pass has not reached yet costs this call the full resolution,
/// never a stale body. The arguments are rooted across the call and its
/// sentinel service, which can re-run the callee -- except for a body that owns
/// its arguments (gcg g6/jm, [`door_pins_call_args`]).
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live, `thread` is this thread's
/// exclusive borrow, `pic` is this site's, and `args_slice` is the call's
/// canonicalised argument block.
#[allow(clippy::too_many_arguments)]
unsafe fn try_mega_dispatch_table_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_class_id: ClassId,
    vm_ptr: i64,
    args_slice: &[i64],
    epoch_seen: u32,
) -> Option<i64> {
    if !mega_dispatch_table_enabled() {
        return None;
    }
    let selector = pic.mega_selector();
    if selector == 0 {
        return None;
    }
    let table = vm.jit.jit_cache.mega_dispatch_table_if_any()?;
    // Round 15 wave 3 (lane mega2, MG15-3): a way whose hashed set was full
    // lives only in the receiver's class cell, which the hashed stub reads
    // and `lookup` does not; read it at this site's column before paying the
    // full resolution (`CRATONVM_JIT_MEGA_RUST_CELL_READ=0`: hashed ways only).
    let (entry, needs_ctx, owner, from_cell) =
        match table.lookup(receiver_class_id.as_u32(), selector) {
            Some((entry, needs_ctx, owner)) => (entry, needs_ctx, owner, false),
            None => {
                // SAFETY: this is the resolving helper, entered from compiled
                // code (JIT depth > 0) past its entry quiescent stamp, and
                // nothing between here and the read's return stamps again,
                // polls or blocks: the machine probe's grace covers it.
                let (entry, needs_ctx, owner) = table.lookup_site_class_cell(
                    receiver_class_id.as_u32(),
                    selector,
                    pic.mega_class_slot(),
                )?;
                (entry, needs_ctx, owner, true)
            }
        };
    let owner = owner?;
    if owner.superseded.load(std::sync::atomic::Ordering::Acquire)
        || owner.retired.load(std::sync::atomic::Ordering::Acquire)
        || vm.jit.jit_cache.redefine_epoch() != epoch_seen
    {
        return None;
    }
    // gcg g6/jm: not for a body that owns its arguments (`door_pins_call_args`).
    let pins = door_pins_call_args(
        true,
        cratonvm_jit::callee_body_owns_its_arguments(&owner),
        door_owned_args_unpinned_enabled(vm),
    )
    .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
    note_cached_entry_arm(&CACHED_ENTRY_OWNER_REUSE_HITS);
    // Cast: an entry address.
    let called = call_compiled_entry_under_owner(
        Some(&*owner),
        entry as usize,
        needs_ctx,
        vm_ptr,
        args_slice,
    );
    let result = called.map(|rc| {
        if rc == i64::MIN {
            handle_rust_armed_callee_sentinel(
                vm,
                thread,
                info,
                receiver_class_id,
                args_slice,
                pins.as_ref(),
            )
            .unwrap_or(rc)
        } else {
            rc
        }
    });
    if let Some(pins) = pins {
        pins.release(thread);
    }
    if from_cell && result.is_some() {
        disp_census::note(disp_census::MIC_MEGA_CELL_READ);
    }
    result
}

/// `CRATONVM_JIT_MIC_MISS_CALLS_COMPILED` (default on; `0` restores the
/// by-name `invoke_or_native` tail for the call that found the body): see
/// [`call_body_the_mic_probe_just_found`]. Read only on the resolving arms of
/// `jit_invoke_virtual_mic`, after a compile probe answered a body, so it adds
/// no process global and no read on any hit path.
fn mic_miss_calls_compiled_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MIC_MISS_CALLS_COMPILED")
}

/// Round 12 wave 1 (lane `calls`; proposal W18-3 in
/// `docs/feature-designs/jit-ir-optimizations.md`): enter the body the resolving arm of
/// `jit_invoke_virtual_mic` just obtained from its compile probe, instead of
/// re-resolving the same callee BY NAME through `invoke_or_native` (which
/// re-derives the target, re-checks the JIT cache and re-enters through the
/// interpreter door). `None`: nothing was called, take the by-name tail as
/// before.
///
/// The call is the one the megamorphic PIC arm makes for a hashed way:
///
/// * the arguments are the DECODED ones (`full_args`, re-read from their pins
///   after the probe, which can run `<clinit>` and collect), re-encoded one
///   word per slot the way the entry ABI reads them. A value no JIT slot can
///   carry answers `None`;
/// * the body is pinned for the call, and a body already superseded or
///   retired is not entered (the by-name tail finds the current one), nor is
///   any body once a redefinition moved the epoch the caller read or the VM
///   entered interpreter-only mode;
/// * a callee that can be RE-RUN by its sentinel (an exception-table callee,
///   `rerun_pins`) has its arguments rooted across the call and the servicing,
///   exactly as [`try_mic_rust_cached_entry`] does, and the `i64::MIN`
///   sentinel goes through [`handle_rust_armed_callee_sentinel`].
///
/// The caller owns the rest of the admission: a body only for a cacheable,
/// plain-object receiver, compiled for that receiver's own class id, never one
/// carrying an unconditional indy trap (the same bodies the arm publishes, plus
/// the table-barred ones the per-thread entry cache already calls this way).
///
/// # What the sentinel service re-reads (gch h1/jp)
///
/// The re-encoding `raw` is a Rust buffer in no map and no pin list. Every
/// other arm hands the service the CALLER's argument block, which the
/// caller's frame roots for the whole call (a single-pass block is named in
/// the site's oop map and remapped in place; an optimizing body's staging
/// region lies inside the frame band the conservative scan reads, so its
/// objects stay put -- see `gch_h1jp_both_tiers_root_the_dispatch_staging_block`).
/// `raw` used to go to the service instead, unpinned for a handler-free
/// callee, so a moving collection inside the callee left the declined-stash
/// and frameless re-runs reading from-space words
/// (`gch-h1fr-compiled-frames-and-dispatch-doors-review-findings-FIXED-20261008.md`
/// item 1). Now, without `rerun_pins`, the service reads `caller_args` (the
/// caller's block, `None` when the door read a forwarded Rust copy instead)
/// whenever it names the same objects as `raw`
/// ([`mic_miss_rerun_reads_the_callers_block`]): the exposure of every other
/// arm, and no pin, so no added retention. Only when they differ -- the
/// compile probe or the SATB flush moved an argument -- is `raw` pinned across
/// the call (unless the body owns its arguments and
/// `CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED` is on, [`door_pins_call_args`]).
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live, `thread` is this thread's
/// exclusive borrow, and `entry` came from the probe for `receiver_class_id`.
#[allow(clippy::too_many_arguments)]
unsafe fn call_body_the_mic_probe_just_found(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    full_args: &[Value],
    caller_args: Option<&[i64]>,
    rerun_pins: bool,
    epoch_seen: u32,
) -> Option<i64> {
    if entry == 0
        || !mic_miss_calls_compiled_enabled()
        || !direct_virtual_compiled_callee_entry_enabled()
        || vm.jit.jit_cache.redefine_epoch() != epoch_seen
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    let mut raw: smallvec::SmallVec<[i64; INLINE_JIT_NATIVE_ARGS]> = smallvec::SmallVec::new();
    for value in full_args {
        raw.push(match *value {
            Value::Int(v) => i64::from(v),
            Value::Long(v) => v,
            // Cast: the entry ABI carries a float's bits in the low half.
            Value::Float(f) => i64::from(f.to_bits()),
            // Cast: the entry ABI carries a double's bits verbatim.
            Value::Double(d) => d.to_bits() as i64,
            // Cast: a live object's address.
            Value::Object(Some(obj)) => obj.as_ptr() as i64,
            Value::Object(None) => 0,
            _ => return None,
        });
    }
    // An entry the code-range registry cannot name is not entered either.
    let pinned = cratonvm_jit::pin_jit_code_range_owner(entry);
    let live = pinned.as_deref().is_some_and(|cm| {
        !cm.superseded.load(std::sync::atomic::Ordering::Acquire)
            && !cm.retired.load(std::sync::atomic::Ordering::Acquire)
    });
    if !live {
        return None;
    }
    // gch h1/jp: see "What the sentinel service re-reads" above.
    let callers_block = if rerun_pins {
        None
    } else {
        caller_args.filter(|caller| mic_miss_rerun_reads_the_callers_block(&raw, caller, full_args))
    };
    let pin_raw = rerun_pins
        || (callers_block.is_none()
            && door_pins_call_args(
                true,
                pinned
                    .as_deref()
                    .is_some_and(cratonvm_jit::callee_body_owns_its_arguments),
                door_owned_args_unpinned_enabled(vm),
            ));
    let pins = pin_raw.then(|| CompileArgPins::pin_raw(vm, thread, info, &raw));
    note_cached_entry_arm(&CACHED_ENTRY_REGISTRY_PINS);
    let called = call_compiled_entry_under_owner(pinned.as_deref(), entry, needs_ctx, vm_ptr, &raw);
    let result = called.map(|rc| {
        if rc == i64::MIN {
            // `pins` index `raw`, so a pinned call is serviced over `raw`.
            let rerun_args: &[i64] = match callers_block {
                Some(caller) if pins.is_none() => caller,
                _ => &raw[..],
            };
            handle_rust_armed_callee_sentinel(
                vm,
                thread,
                info,
                receiver_class_id,
                rerun_args,
                pins.as_ref(),
            )
            .unwrap_or(rc)
        } else {
            rc
        }
    });
    if let Some(pins) = pins {
        pins.release(thread);
    }
    result
}

/// Bound on [`MIC_TAIL_NO_BODY`]; a full table is cleared (slower, never wrong).
const MIC_TAIL_NO_BODY_CAP: usize = 1024;

thread_local! {
    /// `(site, receiver class id)` -> the `JitCache::generation()` at which
    /// [`mic_tail_call_selected_declaring_body`] found no body under the
    /// declaring class's key.
    static MIC_TAIL_NO_BODY: std::cell::RefCell<rustc_hash::FxHashMap<(JitSiteKey, u32), u64>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// nio21 wave 19, lane L2 (`r21-by-name-invoke-ladder-is-paid-per-call-CLOSED-20261012.md`
/// item 2): the inline-cache miss of a virtual site whose compile probe found
/// nothing under the RECEIVER's key, while the method §5.4.6 selects for that
/// receiver IS compiled under its DECLARING class's key.
///
/// Host w18 `dispatch-tally` on `Nio20JitRows rlock`: every one of 600 000
/// unlocks ran `AQS.release -> tryRelease` as `jit_mic_tail ->
/// invoke_or_native -> invoke_on_class_shared_inner` (the whole by-name
/// ladder), receiver `ReentrantLock$NonfairSync`, method declared and compiled
/// on `ReentrantLock$Sync`. The probe asks for `(NonfairSync, tryRelease)`.
///
/// This asks the question once more by IDENTITY: the selected method and its
/// declaring class id (`select_for_jit_receiver`, the same §5.4.6 answer the
/// dispatch doors publish under), cross-checked with the lenient template
/// build (`build_bytecode_callee_cached`, which declines a native shadow on
/// any class from the receiver up to the declaring one, a synchronized,
/// native or code-less method), then the JIT cache under the declaring
/// class's own key. A wrapped-entry or indy-trapping body is refused, as the
/// probe arm refuses it. The call goes through
/// [`call_body_the_mic_probe_just_found`] (registry pin, sentinel service,
/// argument pins for a table callee), and the entry is published into the
/// per-thread `(site, receiver class)` entry cache the helper consults first
/// on its next call ([`try_mic_rust_cached_entry`]), whose stale-body test
/// drops it on supersession or invalidation. Nothing is published into the
/// machine-code MIC/PIC: that is the probe arm's decision, unchanged.
///
/// `None`: nothing ran; the caller takes the by-name tail as before.
/// Kill switch `CRATONVM_JIT_MIC_TAIL_DECLARING_BODY=0` (read only once a
/// declaring-class body has been found, i.e. right before it would be entered).
///
/// # Safety
/// As [`call_body_the_mic_probe_just_found`].
#[allow(clippy::too_many_arguments)]
unsafe fn mic_tail_call_selected_declaring_body(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_ptr: i64,
    receiver_class_id: ClassId,
    vm_ptr: i64,
    full_args: &[Value],
    caller_args: Option<&[i64]>,
    site_rerun_pins: bool,
    epoch_seen: u32,
    ic: Option<(i64, i64, &str)>,
) -> Option<i64> {
    if receiver_class_id.as_u32() == 0 || info.method_name == "<init>" {
        return None;
    }
    // A (site, receiver class) whose declaring-class body was absent from the
    // JIT cache at this generation is absent still: nothing has been published
    // or withdrawn since. Without the memo every call of a site whose body was
    // withdrawn (a callee's deopt takes its raw callers with it) re-ran the
    // selection and a by-name class lookup below, ~25 % of the miss path's
    // samples on `CharBuffer.position` in a loop, until the recompile landed.
    let memo_key = (jit_site_key(vm.vm_identity, info_ptr as usize), receiver_class_id.as_u32());
    let generation = vm.jit.jit_cache.generation();
    if MIC_TAIL_NO_BODY.with(|m| m.borrow().get(&memo_key) == Some(&generation)) {
        return None;
    }
    let (decl_name, decl_id, has_table) = {
        let cm = vm.classes.class_manager.read();
        // An interface- or `Object`-classed receiver is dispatched on the
        // site's class by `virtual_dispatch_target_for_receiver` (S111r12 /
        // COL8-2), not on its own: not this arm's question.
        let receiver_class = cm.get_class(receiver_class_id)?;
        if receiver_class.is_interface() || &*receiver_class.name == "java/lang/Object" {
            return None;
        }
        let (method, decl_id) = select_for_jit_receiver(&cm, receiver_class_id, info)?;
        if method.is_static() || method.is_abstract() || method.is_native() {
            return None;
        }
        let has_table = !method.code()?.exception_table.is_empty();
        let decl_name = cm.class_store().get(decl_id)?.name.clone();
        (decl_name, decl_id, has_table)
    };
    let Some(compiled) =
        vm.jit
            .jit_cache
            .read()
            .get(&decl_name, info.method_name, info.descriptor, decl_id)
    else {
        MIC_TAIL_NO_BODY.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() >= MIC_TAIL_NO_BODY_CAP {
                m.clear();
            }
            m.insert(memo_key, generation);
        });
        return None;
    };
    if compiled.requires_wrapped_entry || compiled.has_indy_trap {
        return None;
    }
    // The lenient walk must agree with the selection, and no native may
    // shadow the body anywhere from the receiver up to its declaring class.
    let (template, _gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        false,
    )?;
    if template.declaring_class_id != decl_id {
        return None;
    }
    // Read only once a body is in hand: a site this arm serves publishes into
    // the entry cache and does not come back; one it cannot serve never reads.
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MIC_TAIL_DECLARING_BODY") {
        return None;
    }
    // Cast: a code address.
    let entry = compiled.entry_ptr() as usize;
    let needs_ctx = compiled.needs_context();
    let rerun_pins = has_table || site_rerun_pins;
    let rc = call_body_the_mic_probe_just_found(
        vm,
        thread,
        info,
        receiver_class_id,
        entry,
        needs_ctx,
        vm_ptr,
        full_args,
        caller_args,
        rerun_pins,
        epoch_seen,
    )?;
    if vm.jit.jit_cache.redefine_epoch() == epoch_seen {
        publish_mic_rust_cached_entry(
            vm.vm_identity,
            info_ptr,
            receiver_class_id.as_u32(),
            entry,
            needs_ctx,
            rerun_pins,
        );
        // nio21 w20 L2: and, for a callee with no exception table, into the
        // site's own machine-code MIC/PIC exactly as the probe arm publishes a
        // body it found (`update` / `install` retain the owner by entry, the
        // shared table takes it under its selector, a redefinition that
        // landed meanwhile withdraws it). The cascade then CALLs the selected
        // body with no Rust on the path. A table callee stays in the Rust
        // entry cache, whose sentinel service runs its handler.
        if let Some((mic_ptr, pic_ptr, class_name)) = ic {
            if !has_table && !rerun_pins {
                mic_tail_publish_declaring_body(
                    vm,
                    thread,
                    info,
                    receiver_class_id.as_u32(),
                    entry,
                    needs_ctx,
                    epoch_seen,
                    mic_ptr,
                    pic_ptr,
                    class_name,
                );
            }
        }
    }
    drop(compiled);
    Some(rc)
}

/// nio21 w20 L2: the machine-code half of
/// [`mic_tail_call_selected_declaring_body`]'s publication. Kill switch
/// `CRATONVM_JIT_MIC_TAIL_DECLARING_BODY_IC=0` (read once per publication,
/// i.e. once per site and receiver class).
///
/// # Safety
/// `mic_ptr` is the live `JitMICSlot` of the call site whose miss is being
/// served (non-zero), `pic_ptr` its co-allocated `JitPICSlot` or 0.
#[allow(clippy::too_many_arguments)]
unsafe fn mic_tail_publish_declaring_body(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_cid: u32,
    entry: usize,
    needs_ctx: bool,
    epoch_seen: u32,
    mic_ptr: i64,
    pic_ptr: i64,
    class_name: &str,
) {
    if mic_ptr == 0
        || entry == 0
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_MIC_TAIL_DECLARING_BODY_IC",
        )
    {
        return;
    }
    let mic = &*(mic_ptr as *const JitMICSlot);
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    if !mic_publication_skipped(pic_ptr) {
        mic.update(receiver_cid, class_name, entry as u64, needs_ctx, jdk_only);
    }
    if pic_ptr != 0 {
        let pic = &*(pic_ptr as *const JitPICSlot);
        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
        if !pic_way_redundant_with_mic(mic, pic, receiver_cid)
            && pic.install(receiver_cid, class_name, entry as u64, needs_ctx, jdk_only)
        {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(vm, Some(thread.thread_id));
        }
        publish_mega_dispatch_entry(
            vm,
            info,
            pic,
            receiver_cid,
            entry as u64,
            needs_ctx,
            epoch_seen,
        );
    }
    withdraw_ic_publication_if_redefined(
        vm,
        epoch_seen,
        mic,
        (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot)),
    );
}

/// JIT round 20 wave 19, lane yLadder (DISP20-7,
/// `docs/feature-designs/jit-ir-optimizations.md`): the body
/// [`mic_tail_call_selected_declaring_body`] finds, answered in the shape of
/// the receiver-keyed compile probe (`try_jit_compile_callee_for_class`:
/// the pinning `Arc`, the entry, `needs_context`) so the two doors that had
/// only that probe can take it as the probe's own answer.
///
/// The probe asks the JIT cache under the RECEIVER's key, which names an
/// INHERITED method's body only when the slow path compiled a copy for the
/// receiver; the body background tiering published under the DECLARING
/// class's key (`ReentrantLock$Sync.tryRelease` on a `NonfairSync`,
/// `AQS.release` / `Sync.lock` reached on a `NonfairSync`) is invisible to it.
/// w19/w20 taught the MIC's entryless-hit arm to ask for that body; two doors
/// still went to the by-name `invoke_or_native` ladder on every call:
///
/// * the MIC MISS arm (`jit_invoke_virtual_mic_body`'s cache-miss branch),
///   where every call lands for a receiver class other than the one the
///   monomorphic MIC was first populated with -- its declined probe was
///   memoised (`MIC_COMPILE_DECLINED`) and the by-name tail ran per call;
/// * the blind dispatch door (`jit_invoke_dispatch`'s virtual arm, sites
///   with no MIC), whose probe fills `VIRTUAL_DISPATCH_CACHE` only with what
///   the probe answers.
///
/// Same rules as the entryless-hit arm: the method JVMS §5.4.6 selects for
/// the receiver (`select_for_jit_receiver`, by class id), not static,
/// abstract or native; not an interface- or `Object`-classed receiver (the
/// doors dispatch those on the site's class); the body published for the
/// EXACT declaring class id; a wrapped-entry or indy-trapping body refused;
/// and the template build (`build_bytecode_callee_cached`, which declines a
/// native shadow on any class from the receiver up to the declaring one and
/// a synchronized method) must agree on the declaring class. The caller then
/// publishes and calls it exactly as it does a probe answer: the exception
/// table rule (`mic_callee_has_exception_table`), the indy-trap rule, the
/// redefinition withdrawal and the stale-body drop all resolve the callee
/// through the receiver's selection, so they judge this body.
///
/// Switch `CRATONVM_JIT_PROBE_DECLARING_BODY` (default ON, `=0` restores the
/// probe's own answer), read only once a body is in hand -- i.e. on a probe
/// miss that this would serve, never per call.
fn probe_selected_declaring_body(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    if receiver_class_id.as_u32() == 0 || info.method_name == "<init>" {
        return None;
    }
    let (decl_name, decl_id) = {
        let cm = vm.classes.class_manager.read();
        let receiver_class = cm.get_class(receiver_class_id)?;
        if receiver_class.is_interface() || &*receiver_class.name == "java/lang/Object" {
            return None;
        }
        let (method, decl_id) = select_for_jit_receiver(&cm, receiver_class_id, info)?;
        if decl_id == receiver_class_id
            || method.is_static()
            || method.is_abstract()
            || method.is_native()
            || method.is_synchronized()
        {
            return None;
        }
        let decl_name = cm.class_store().get(decl_id)?.name.clone();
        (decl_name, decl_id)
    };
    let compiled =
        vm.jit
            .jit_cache
            .read()
            .get(&decl_name, info.method_name, info.descriptor, decl_id)?;
    if compiled.requires_wrapped_entry || compiled.has_indy_trap {
        return None;
    }
    let (template, _gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        false,
    )?;
    if template.declaring_class_id != decl_id {
        return None;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_PROBE_DECLARING_BODY") {
        return None;
    }
    // Cast: a code address, as the probe's own arm.
    let entry = compiled.entry_ptr() as usize;
    let needs_ctx = compiled.needs_context();
    Some((compiled, entry, needs_ctx))
}

/// gch h1/jp: may [`call_body_the_mic_probe_just_found`]'s sentinel service
/// read the caller's argument block `caller` instead of the call's re-encoding
/// `raw`? Only when the two are the same length as the decoded `full_args` and
/// every reference slot (`Value::Object`, null included) carries the same word
/// in both: then they name the same objects at the call, and the caller's
/// block is the one its frame keeps current. A primitive slot may differ in
/// its unused upper bits (the re-encoding widens canonically), which the
/// service reads the way every other arm reads the caller's block.
fn mic_miss_rerun_reads_the_callers_block(
    raw: &[i64],
    caller: &[i64],
    full_args: &[Value],
) -> bool {
    raw.len() == caller.len()
        && full_args.len() == raw.len()
        && full_args
            .iter()
            .zip(raw.iter().zip(caller.iter()))
            .all(|(value, (&r, &c))| !matches!(value, Value::Object(_)) || r == c)
}

#[cfg(test)]
mod gch_h1jp_mic_miss_rerun_tests {
    use super::{mic_miss_rerun_reads_the_callers_block, ObjectRef, Value};

    /// gch h1/jp (fr review item 1): the caller's block is read only while it
    /// names the same objects as the re-encoding.
    #[test]
    fn gch_h1jp_the_mic_miss_rerun_reads_the_callers_block_only_when_it_is_current() {
        // SAFETY: never dereferenced; only its address is compared.
        let obj = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
        let full = [
            Value::Object(Some(obj)),
            Value::Int(-1),
            Value::Object(None),
        ];
        let raw = [0x1000_i64, -1, 0];
        // The caller kept an int's upper bits dirty: still the same objects.
        let caller = [0x1000_i64, 0xffff_ffff, 0];
        assert!(mic_miss_rerun_reads_the_callers_block(&raw, &caller, &full));
        // The probe moved the receiver: the caller's word is pre-move.
        let moved = [0x2000_i64, -1, 0];
        assert!(!mic_miss_rerun_reads_the_callers_block(
            &moved, &caller, &full
        ));
        // A null that is not null in the caller's block.
        let stray = [0x1000_i64, -1, 0x3000];
        assert!(!mic_miss_rerun_reads_the_callers_block(&raw, &stray, &full));
        // Shapes that do not line up are never read across.
        assert!(!mic_miss_rerun_reads_the_callers_block(
            &raw,
            &caller[..2],
            &full
        ));
        assert!(!mic_miss_rerun_reads_the_callers_block(
            &raw[..2],
            &caller[..2],
            &full
        ));
    }

    /// gch h1/jp (fr review item 2): both compiled tiers root the argument
    /// block a dispatch helper is handed, so the declined-stash and frameless
    /// re-runs that read it after the callee read current words. Single-pass:
    /// every reference staged into the helper's buffer is pushed to
    /// `pending_staged_arg_oops`, which the call's safepoint map names (precise,
    /// remapped in place). Optimizing tier: the staging region is carved out of
    /// the frame (`frame_size - shadow - stack_arg_reserve` down), inside the
    /// `[rbp - frame_size, rbp)` band every compiled-frame scan reads, and no
    /// liveness claim drops a word the current call staged -- a conservative
    /// root, whose object is pinned, not moved. A source pin, so an edit that
    /// moves either block out of its root set fails here first.
    #[test]
    fn gch_h1jp_both_tiers_root_the_dispatch_staging_block() {
        let single = include_str!("../../../../jit/src/x64/op_invoke.rs").replace("\r\n", "\n");
        assert!(
            single.contains("if arg_oops[i] {\n                                self.pending_staged_arg_oops.push(buf_offset);"),
            "single-pass: the helper's args buffer names its references in the map"
        );
        let ir = include_str!("../../../../jit/src/ir_lower.rs").replace("\r\n", "\n");
        assert!(
            ir.contains("let args_stage_top_off = frame_size - shadow - stack_arg_reserve;"),
            "optimizing tier: the staging region lies inside the frame"
        );
        assert!(
            ir.contains("The words this call does stage stay covered-and-live"),
            "optimizing tier: the liveness claim keeps the current call's staged words"
        );
        let scan = include_str!("../conservative_roots.rs").replace("\r\n", "\n");
        assert!(
            scan.contains("bands.push(rbp - frame_size, rbp);"),
            "the compiled-frame scan reads the whole frame band"
        );
    }
}

/// [`virtual_dispatch_target_for_receiver`] *plus* the `globally_named`
/// round-trip test, both served from a per-thread memo.
///
/// The uncached pair costs TWO `class_manager` read-lock acquisitions on every
/// call — one to turn the receiver's `ClassId` into a name, one to check that
/// the name resolves back to it — on the path taken by every virtual call
/// whose inline cache holds no compiled entry. That is a third of all MIC
/// calls, and unavoidably *every* call to a native callee, which can never
/// have a compiled entry. At ten threads it was the single largest cost in the
/// VM: reader-reader `parking_lot` contention on one cache line, ~13% of all
/// CPU in `lock_shared_slow` alone, with every workload converging on the same
/// per-op cost regardless of what it actually did
/// (23-charsetcache-pathological-slowdown.md).
///
/// Callers MUST have run [`flush_class_identity_dispatch_memos`] on this
/// thread first — that is what makes a hit as fresh as a locked resolution.
///
/// SAFETY: identical contract to [`virtual_dispatch_target_for_receiver`].
unsafe fn virtual_dispatch_target_cached(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
) -> CachedDispatchTarget {
    // KC26: array receivers store their COMPONENT class id in the header, so
    // the `(site, class id)` key cannot tell `X[]` from `X`. Resolve them
    // directly — array classes inherit Object's method table (JVMS §4.4.1).
    //
    // An array-typed call SITE gets the same answer without asking the header
    // at all, which is the form that survives a header that lies: a block
    // reclaimed while still referenced and then re-served reads back as a
    // perfectly ordinary object of whatever now occupies it, and the
    // receiver-driven form would then resolve `"[J".clone()` against that
    // occupant's class.
    let dispatch = if info.class_name.starts_with('[') {
        JitDispatchClass::JavaLangObject
    } else {
        jit_dispatch_class(vm, receiver)
    };
    let Some(cid) = dispatch.class_id() else {
        // `--jdk-only`: an ARRAY receiver at an `invokeinterface` implements
        // only `Cloneable` and `Serializable` (interpreter round i1 wave 39,
        // lane L2). Not memoized, as the class-id-0 arm below: a flagged
        // pair throws on every call.
        let selection_error = jit_array_receiver_does_not_implement(vm, receiver, info).is_some();
        return CachedDispatchTarget {
            class_name: std::rc::Rc::from("java/lang/Object"),
            cacheable_receiver: false,
            globally_named: false,
            selection_error,
            selected_cid: 0,
        };
    };
    if cid == ClassId::new(0) {
        // `--jdk-only`: class id 0 is also a plain `java.lang.Object`, which
        // the by-name answer below would run the interface's default method
        // on. Not memoized: every call of such a pair throws.
        if jit_object_receiver_does_not_implement(vm, receiver, info).is_some() {
            return CachedDispatchTarget {
                class_name: std::rc::Rc::from("java/lang/Object"),
                cacheable_receiver: false,
                globally_named: false,
                selection_error: true,
                selected_cid: 0,
            };
        }
        // Not a class at all — a synthetic/unidentified receiver. The answer
        // is a pure function of `info`, so there is nothing worth memoizing.
        return CachedDispatchTarget {
            class_name: if crate::vm::is_object_member(info.method_name, info.descriptor) {
                std::rc::Rc::from("java/lang/Object")
            } else {
                std::rc::Rc::from(info.class_name)
            },
            cacheable_receiver: false,
            globally_named: false,
            selection_error: false,
            selected_cid: 0,
        };
    }

    let key = (info_key, cid.as_u32());
    if let Some(hit) = VIRTUAL_TARGET_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }

    // Miss. Resolve the name AND its round-trip under ONE read lock, where the
    // uncached path took two.
    //
    // A §5.4.6 retarget (`receiver_selection_retarget`) is receiver-derived
    // too — a pure function of (site, receiver class) — so it is memoized even
    // though it publishes nothing under the receiver's id.
    let mut retargeted = false;
    let resolved = {
        let cm = vm.classes.class_manager.read();
        match cm.get_class(cid) {
            // A lambda proxy is not in the class store. A call that is not its
            // SAM (the lambda arm declined it) is a default method of the
            // functional interface, or an abstract method of one of its
            // super-interfaces that the interface implements with a default
            // (`interface Throwing<F, R> extends Function<F, R> { default R
            // apply(F f) {..} }`). Resolving the by-name tail against the
            // SITE's class (`Function`) invoked the abstract `Function.apply`
            // on the proxy, which answered its first argument; the interpreter
            // selects from the proxy's own interface. Quarkus suite wave 1
            // (`JandexUtilTest`: AssertJ `extracting(..)`).
            None => CachedDispatchTarget {
                class_name: vm
                    .classes
                    .lambda_call_site_for(cid)
                    .filter(|_| !crate::vm::is_object_member(info.method_name, info.descriptor))
                    .map(|site| std::rc::Rc::from(&*site.functional_interface))
                    .unwrap_or_else(|| std::rc::Rc::from(info.class_name)),
                cacheable_receiver: false,
                globally_named: false,
                selection_error: false,
                selected_cid: 0,
            },
            Some(recv_class) => {
                let recv_name = recv_class.name.clone();
                let recv_is_iface = recv_class.is_interface();
                let recv_is_bare_object = recv_name.as_ref() == "java/lang/Object";
                let cp_is_not_object = info.class_name != "java/lang/Object";
                // First: before the by-interface-name answer below, which a
                // plain `Object` receiver would otherwise take, and before any
                // selection (HotSpot checks it first).
                // Before even that (wave 100, lane L2): the site's own
                // reference resolves to nothing, so no receiver selects a
                // body. Flagged and memoized per (site, receiver class); the
                // doors raise it through `jit_selection_failure`.
                let unresolvable = jit_site_unresolvable_owner(vm, &cm, info).is_some();
                let not_implemented = if unresolvable {
                    None
                } else {
                    jit_receiver_does_not_implement(vm, &cm, cid, info)
                };
                if unresolvable {
                    retargeted = true;
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: true,
                        selected_cid: 0,
                    }
                } else if let Some(message) = not_implemented {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jit] invokeinterface receiver does not implement: {message} \
                             (site {}.{}{})",
                            info.class_name, info.method_name, info.descriptor
                        );
                    }
                    retargeted = true;
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: true,
                        selected_cid: 0,
                    }
                } else if (recv_is_iface || recv_is_bare_object)
                    && cp_is_not_object
                    && !crate::vm::is_object_member(info.method_name, info.descriptor)
                    && recv_name.as_ref() != info.class_name
                {
                    // COL8-2 (round 17 wave 2, lane mega2): an instance minted
                    // on a sub-interface of the site's class that declares the
                    // method runs its own, as every cached door does. A pure
                    // function of (site, receiver class), so memoized; it
                    // stays unpublished (not `cacheable_receiver`).
                    if recv_is_iface
                        && interface_receiver_dispatches_on_itself(
                            vm,
                            &cm,
                            cid,
                            jit_site_owner(&cm, info),
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        )
                    {
                        retargeted = true;
                        CachedDispatchTarget {
                            class_name: std::rc::Rc::from(&*recv_name),
                            cacheable_receiver: false,
                            globally_named: false,
                            selection_error: false,
                            selected_cid: 0,
                        }
                    } else {
                        CachedDispatchTarget {
                            class_name: std::rc::Rc::from(info.class_name),
                            cacheable_receiver: false,
                            globally_named: false,
                            selection_error: false,
                            selected_cid: 0,
                        }
                    }
                } else if let Some((selected, selected_id)) =
                    receiver_selection_retarget(&cm, cid, info)
                {
                    retargeted = true;
                    // MEGA17-3: the selected class's id (MEGA3-17-4: straight
                    // from `receiver_selection_retarget`, not a by-name lookup,
                    // which another loader's copy of the name would defeat), so
                    // its body can be compiled and published under this
                    // receiver's id.
                    let selected_cid = if mega_cache_retargeted_enabled() {
                        selected_id.as_u32()
                    } else {
                        0
                    };
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*selected),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: false,
                        selected_cid,
                    }
                } else if jit_strict_selection_errs(&cm, cid, info) {
                    retargeted = true;
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: true,
                        selected_cid: 0,
                    }
                } else {
                    let globally_named = cm.get_loaded_class_id(&recv_name) == Some(cid);
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: true,
                        globally_named,
                        selection_error: false,
                        selected_cid: 0,
                    }
                }
            }
        }
    };

    // Only the receiver-derived answers are worth keeping. The others are pure
    // functions of `info` that cost one comparison to redo, and memoizing them
    // would fill the map with entries no fast path consults.
    if resolved.cacheable_receiver || retargeted {
        VIRTUAL_TARGET_CACHE.with(|c| {
            let mut map = c.borrow_mut();
            if map.len() >= VIRTUAL_TARGET_CACHE_CAP {
                map.clear();
            }
            map.insert(key, resolved.clone());
        });
    }
    resolved
}

/// Resolve the dispatch class for a virtual/interface bail
/// (`bail_to_interpreter`, kinds 0/2). Null/non-object receivers fall back to
/// the static call-site class so we never dispatch on an empty name.
///
/// SAFETY: `vm` must be a live `SharedVm`; `args[0]` (when present) is the
/// receiver `Value` decoded by `decode_dispatch_values`/`decode_values`.
unsafe fn virtual_dispatch_class(
    vm: &SharedVm,
    args: &[Value],
    info: &JitInvokeInfo,
) -> std::sync::Arc<str> {
    let receiver = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return std::sync::Arc::from(info.class_name),
    };
    virtual_dispatch_target_for_receiver(vm, receiver, info).class_name
}

/// BUG-H: does the statically-bound callee declare a non-empty exception
/// table? Resolved from the class manager by `(class, method, descriptor)`.
///
/// Only meaningful for the statically-bound kinds (invokestatic/invokespecial),
/// where `info.class_name`/`method_name`/`descriptor` name the exact callee —
/// the virtual/interface kinds resolve on the runtime receiver type elsewhere.
///
/// SAFETY: `vm` must be a live `SharedVm`; `info` must point to a valid
/// `JitInvokeInfo` whose name fields are live `&str`s.
/// Resolve the callee named by `info` to a `CachedBytecodeMethod`, so its own
/// exception table can be consulted and its handler run without re-executing
/// the method. Mirrors `try_resume_trapped_callee`'s resolution recipe.
///
/// Memoized per `(site, receiver class)` in [`CALLEE_HANDLER_TEMPLATE_CACHE`]:
/// a serviced callee exception used to pay the lock, the hierarchy walk and a
/// copy of the whole method body on every throw.
///
/// SAFETY: same contract as `callee_has_exception_table`.
unsafe fn resolve_callee_cached(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    if let Some(hit) = callee_handler_template_hit(vm, info, receiver_class_id) {
        return Some(hit);
    }
    let (cached, gate) = resolve_callee_uncached(vm, info, receiver_class_id)?;
    let key = callee_handler_template_key(vm, info, receiver_class_id);
    CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= CALLEE_HANDLER_TEMPLATE_CACHE_CAP {
            map.clear();
        }
        map.insert(key, (std::sync::Arc::clone(&cached), gate));
    });
    Some(cached)
}

/// Clear-on-full bound for [`CALLEE_HANDLER_TEMPLATE_CACHE`]; an entry exists
/// only for a (site, receiver class) whose callee actually caught something.
const CALLEE_HANDLER_TEMPLATE_CACHE_CAP: usize = 1024;

/// The memo key: the receiver class selects the override only for kinds 0/2.
fn callee_handler_template_key(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> (JitSiteKey, u32) {
    let receiver = if matches!(info.invoke_kind, 0 | 2) {
        receiver_class_id.as_u32()
    } else {
        0
    };
    (
        jit_site_key(vm.vm_identity, info as *const JitInvokeInfo as usize),
        receiver,
    )
}

/// A live [`CALLEE_HANDLER_TEMPLATE_CACHE`] entry for this site, re-validated.
///
/// Runs both memo flushes first: the generated inline cascade reaches the
/// callee-handler service (`jit_service_callee_deopt`) without passing through
/// a dispatch helper's own flushes, and a recycled `JitInvokeInfo` address or a
/// second loader defining the callee's class name must not be served the old
/// answer. Both are two thread-local compares when nothing changed. A stale
/// `RedefineGate` evicts the entry: the template holds the old body.
fn callee_handler_template_hit(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    flush_raw_entry_dispatch_caches();
    flush_class_identity_dispatch_memos(vm);
    let key = callee_handler_template_key(vm, info, receiver_class_id);
    let (cached, gate) = CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| c.borrow().get(&key).cloned())?;
    if gate.is_stale() {
        CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| {
            c.borrow_mut().remove(&key);
        });
        return None;
    }
    Some(cached)
}

/// The resolution [`resolve_callee_cached`] memoizes. The redefine gate is
/// snapshotted under the same class-manager read lock the method is read
/// under, so a redefinition cannot slip between the two.
///
/// SAFETY: same contract as `callee_has_exception_table`.
/// The method a JIT call site runs, with its declaring class: the §5.4.6
/// selection for a receiver-class dispatch (what the dispatch doors run,
/// `receiver_selection_retarget`), else the named method looked up from the
/// site's owner (a substituted owner by its recorded id). Shared by the
/// handler template (`resolve_callee_uncached`) and the re-run verdict
/// (`callee_rerun_code`), so both judge the same method.
fn jit_site_target_method<'a>(
    cm: &'a crate::classloading::ClassManager,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    substituted: Option<ClassId>,
) -> Option<(&'a cratonvm_reader::method::ClassFileMethod, ClassId)> {
    let receiver_dispatch = matches!(info.invoke_kind, 0 | 2) && receiver_class_id.as_u32() != 0;
    let class_id = match info.invoke_kind {
        0 | 2 if receiver_class_id.as_u32() != 0 => Some(receiver_class_id),
        _ if substituted.is_some() => substituted,
        _ if info.declaring_class_id == 0 => cm.find_bootstrap_class_by_name(info.class_name),
        _ => {
            cm.find_cp_class_for_class(info.class_name, ClassId::new(info.declaring_class_id))
        }
    }?;
    if receiver_dispatch {
        select_for_jit_receiver(cm, class_id, info)
    } else {
        crate::classloading::find_method_recursive(
            class_id,
            info.method_name,
            info.descriptor,
            cm.class_store(),
        )
    }
}

unsafe fn resolve_callee_uncached(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    // A substituted owner by its recorded id, asked before this guard.
    let substituted = jit_substituted_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let store = cm.class_store();
    let (method, declaring_id) = jit_site_target_method(&cm, info, receiver_class_id, substituted)?;
    let code_attr = method.code()?;
    // JIT round 17 wave 8 (lane syncdoor5): a synchronized callee is served
    // when its compiled body is a self-locking one with a table, i.e. under
    // `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=1`
    // (`r17-self-locking-handlers-refuse-recursive-exceptional-resume-FIXED-20261005.md`).
    if code_attr.exception_table.is_empty()
        || (method.is_synchronized()
            && !crate::runtime::interpreter::synchronized_callee_handlers_served())
    {
        return None;
    }
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
    let gate = crate::classloading::resolution::RedefineGate::snapshot(
        cm.class_redefine_generation_handle(declaring_id),
    );
    let num_params = crate::runtime::interpreter::count_method_params(info.descriptor);
    // The pool generation the code indexes, read under the guard the code is
    // copied under (interpreter round i1 wave 43, lane L2;
    // `CachedBytecodeMethod::pool_generation`).
    let pool_generation = cratonvm_classloading::class_redefinition_count();
    let cached = std::sync::Arc::new(
        crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring_id,
                class_name: std::sync::Arc::from(declaring_class_name),
                method_name: std::sync::Arc::from(info.method_name),
                method_descriptor: std::sync::Arc::from(info.descriptor),
                source_file: store
                    .get(declaring_id)
                    .and_then(|c| c.source_file.as_deref())
                    .map(std::sync::Arc::from),
                // Interned per method identity, as every interpreter frame's
                // code is: `local_liveness` and the quickening table key on
                // the `Arc`'s address.
                code: crate::runtime::frame::padded_bytecode_for_method(
                    declaring_id,
                    info.method_name,
                    info.descriptor,
                    &code_attr.code,
                    &code_attr.exception_table,
                ),
                exception_table: std::sync::Arc::from(code_attr.exception_table.as_slice()),
                max_stack: code_attr.max_stack,
                max_locals: code_attr.max_locals,
                num_params: num_params as u16,
                is_synchronized: method.is_synchronized(),
                is_static: method.is_static(),
            },
        )
        .with_pool_generation(pool_generation),
    );
    Some((cached, gate))
}

/// WHICH implicit signal a compiled callee left behind alongside the `i64::MIN`
/// sentinel — the one place that answers it, for both doors.
///
/// A bounds check, a null array reference and a zero divisor do NOT hand back a
/// throwable. Each sets a flag and returns the sentinel, and the throwable is
/// built on this side ([`materialize_implicit_signal`]). Two functions route
/// such a sentinel through the CALLEE's own exception table —
/// [`route_implicit_exc_through_callee`] for the Rust dispatch helper's
/// compiled-entry fast paths, and [`handle_compiled_callee_deopt_sentinel`] for
/// the machine-code direct `CALL` and the MIC/PIC cascade.
///
/// **They each carried their own copy of the table, and the copies drifted.**
/// The arithmetic arm existed only in the second, so a `/ by zero` arriving at
/// the first read as "no implicit signal here" and the sentinel went out to the
/// compiled CALLER, whose drain threw the `ArithmeticException` through the
/// CALLER's exception table. 198 000 of 200 000 `catch (ArithmeticException)`
/// skipped on `probes/EscapeStaticProbe`; ~8 % of `probes/ExcTableDirectCallOracle`
/// runs dying at `main:156`. Both pages are in
/// `fixed-suite-bugs/jit/`.
///
/// The arm was added where it was missing. This type is the other half: there is
/// now ONE table, and — the load-bearing part — the question *"is there an
/// implicit signal at all?"* is answered by the SAME function that decides
/// *which one*. What made the drift possible was that the first door asked it
/// with a hand-written `aioobe.is_none() && !npe` companion to the `match`, so
/// adding a flag to the `match` and not to the companion was a silent no-op.
///
/// Precedence — AIOOBE, then NPE, then arithmetic — is the order both callers
/// already drained in. It is a precedence and not a set because only one
/// throwable can be delivered, and the compiled body can only have trapped once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImplicitSignal {
    Aioobe { index: i64, length: i64 },
    Npe,
    Arithmetic,
    None,
}

/// Classify a drained signal triple. Pure, so the table above is testable
/// without a heap — the bug it encodes is entirely an arithmetic property.
pub(crate) fn implicit_signal_of(
    aioobe: Option<(i64, i64)>,
    npe: bool,
    arithmetic: bool,
) -> ImplicitSignal {
    match aioobe {
        Some((index, length)) => ImplicitSignal::Aioobe { index, length },
        None if npe => ImplicitSignal::Npe,
        None if arithmetic => ImplicitSignal::Arithmetic,
        None => ImplicitSignal::None,
    }
}

/// Put a classified signal back exactly as it was drained.
///
/// Both doors fall back on this, and it is not hygiene: the compiled caller's
/// own drain builds the throwable FROM the flag, so a signal consumed here and
/// not restored is an exception that never happens at all — the caller sees a
/// bare deopt sentinel and carries on.
fn restash_implicit_signal(signal: ImplicitSignal) {
    match signal {
        ImplicitSignal::Aioobe { index, length } => stash_jit_pending_aioobe(index, length),
        // `set_jit_pending_npe_flag_only`, NOT `stash_jit_pending_npe`. This
        // door drains the NPE with `take_jit_pending_npe()`, which takes the
        // FLAG and leaves the compiled-frame snapshot where the helper put it.
        // Re-stashing through the ordinary setter would take a SECOND snapshot
        // here -- one frame shallower, because the callee whose code raised the
        // NPE has already returned -- and silently overwrite the real one. The
        // trace would still be plausible and would be missing exactly the frame
        // it was taken to keep.
        ImplicitSignal::Npe => set_jit_pending_npe_flag_only(),
        ImplicitSignal::Arithmetic => stash_jit_pending_arithmetic(),
        ImplicitSignal::None => {}
    }
}

/// Build the throwable an implicit signal is a *request* for.
///
/// One body, so the message text cannot diverge by dispatch route: a
/// `getMessage()` that depended on which door serviced the trap would be its
/// own wrong answer, and keeping the two arms in step by hand is what this
/// replaces.
///
/// `create_exception_object` allocates, so this is a safepoint; each caller
/// documents what it has pinned across it.
fn materialize_implicit_signal(
    vm: &SharedVm,
    thread: &mut JvmThread,
    signal: ImplicitSignal,
    trap_frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    npe_action: u8,
) -> Option<ObjectRef> {
    materialize_implicit_signal_or_frames(vm, thread, signal, trap_frames, npe_action, false).ok()
}

/// [`materialize_implicit_signal`], handing the snapshot BACK when no
/// throwable was built.
///
/// The snapshot is consumed only by the throwable it is attached to. On the
/// one miss -- the class will not load or the allocation failed -- it was never
/// attached, and a caller that must restore the signal exactly as it found it
/// needs it back. Returning it saves that caller cloning the whole snapshot
/// (a `Vec` of frames, an `Arc` bump per label and a `Vec` per inline chain)
/// on EVERY throw just to cover the miss (round 12 wave 3, lane exc:
/// `local_handler_enter_implicit` paid that clone on each compiled catch).
/// The fast-throw door drops the snapshot, exactly as before.
///
/// `live_snapshot` (nio21 wave 18 L1): every frame `trap_frames` names is
/// still on the machine stack and nothing ran since it was taken -- only the
/// compiled local handler's implicit arm builds there. See
/// `exceptions::create_exception_object_with_trap_frames_live`.
fn materialize_implicit_signal_or_frames(
    vm: &SharedVm,
    thread: &mut JvmThread,
    signal: ImplicitSignal,
    trap_frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    npe_action: u8,
    live_snapshot: bool,
) -> Result<ObjectRef, Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>> {
    // r9w9 exc9: `OmitStackTraceInFastThrow` (opt-in, default off). A hot site
    // gets a stackless throwable and skips the message, the constructor, the
    // stack walk and the snapshot splice below; `trap_frames` is simply dropped.
    let fast_kind = match &signal {
        ImplicitSignal::Aioobe { .. } => {
            Some(crate::runtime::exceptions::FastThrowKind::ArrayIndexOutOfBounds)
        }
        ImplicitSignal::Npe => Some(crate::runtime::exceptions::FastThrowKind::NullPointer),
        ImplicitSignal::Arithmetic => Some(crate::runtime::exceptions::FastThrowKind::Arithmetic),
        ImplicitSignal::None => None,
    };
    if let Some(kind) = fast_kind {
        let site = crate::runtime::exceptions::fast_throw_site_of(trap_frames.as_deref());
        if let Some(exc) =
            crate::runtime::exceptions::try_fast_throw(vm, thread, kind, site.as_ref())
        {
            return Ok(exc);
        }
    }
    match signal {
        ImplicitSignal::Aioobe { index, length } => {
            // JIT round 20 wave 18 (lane xThrow, THROW20-5): the same text,
            // without a heap `String` under the lean build.
            let mut buf: crate::runtime::exceptions::AioobeMessageBuf = [0; 96];
            let formatted: String;
            let lean_text = if crate::runtime::exceptions::implicit_throw_lean_build_enabled() {
                crate::runtime::exceptions::format_aioobe_message(&mut buf, index, length)
            } else {
                None
            };
            let msg: &str = match lean_text {
                Some(text) => text,
                None => {
                    formatted = format!("Index {index} out of bounds for length {length}");
                    formatted.as_str()
                }
            };
            // The third arm to need the snapshot, and the last. Same defect as
            // the other two: the bounds-check helper flagged the signal and the
            // compiled body ran its epilogue, so a trace `fillInStackTrace`
            // walked here names nothing at all. The snapshot either stands in
            // for that walk or is spliced on after it (round 12 wave 4, W3-2).
            crate::runtime::exceptions::create_exception_object_with_trap_frames_live(
                vm,
                thread,
                "java/lang/ArrayIndexOutOfBoundsException",
                Some(msg),
                trap_frames,
                live_snapshot,
            )
            .map_err(|(_, frames)| frames)
        }
        ImplicitSignal::Npe => {
            // JEP 358, on the door the interpreter's drain does not serve.
            //
            // This arm passed `None` and so produced `getMessage() == null` for
            // every null dereference CAUGHT by a compiled method's own handler
            // — `probes/L2JitNpeProbe.java`'s `caughtHere` shape, where the
            // interpreter and `java -XX:-OmitStackTraceInFastThrow` both name
            // the field. It is the same
            // defect as the drain's and it is fixed from the same place, so the
            // two cannot answer differently: `jit_npe_message` rebuilds the
            // message from the trapping method's bytecode, using the very
            // snapshot this function is about to attach.
            //
            // Built BEFORE the construction, which consumes the snapshot --
            // or, round 15 wave 7 (lane throw2, TH1-5), deferred to
            // `getMessage()` under `--jdk-only` as HotSpot computes it
            // (`exceptions::create_compiled_implicit_npe`, which builds the
            // eager message exactly as here otherwise).
            //
            // The frames the helper snapshotted at the trap. Without them the
            // NPE materialised HERE -- i.e. every implicit NPE routed into a
            // compiled callee's own handler -- keeps the frameless trace a
            // late `fillInStackTrace` walk builds, which is the whole defect
            // the snapshot exists to close. Handing it over in the constructor
            // arm rather than at each door is what stops a fourth door from
            // silently reopening it.
            crate::runtime::exceptions::create_compiled_implicit_npe_live(
                vm,
                thread,
                trap_frames,
                npe_action,
                live_snapshot,
            )
            .map_err(|(_, frames)| frames)
        }
        ImplicitSignal::Arithmetic => {
            // The NPE arm above has carried the snapshot since it landed; this
            // arm had the identical defect and no test that could see it.
            // `pgo02_guarded_virtual_inline`'s stack-trace check could, and
            // did — as a ~20% flake, because whether a given `callDivider`
            // call ENTERS the artifact is timing-dependent, so the same run
            // alternated between a 2-frame interpreted trace and an empty
            // compiled one.
            crate::runtime::exceptions::create_exception_object_with_trap_frames_live(
                vm,
                thread,
                "java/lang/ArithmeticException",
                Some("/ by zero"),
                trap_frames,
                live_snapshot,
            )
            .map_err(|(_, frames)| frames)
        }
        ImplicitSignal::None => Err(trap_frames),
    }
}

/// Run the compiled callee's own handler for an exception that escaped it,
/// resuming AT the handler instead of re-running the method from entry.
///
/// Returns `Some(rc)` when a handler covering the throw site was found and the
/// resumed frame completed; `None` leaves the caller's existing behaviour
/// (propagate, or the conservative whole-method re-run) untouched. The
/// exception is only consumed on the `Some` path.
///
/// A miss carries the throwable to propagate or restore and whether it
/// REPLACED `exc` — a covering row's catch type failed to resolve and its
/// resolution error is now the exception in flight (JVMS §5.4.3, see
/// `search_compiled_frame_rows`). The implicit-signal doors need that bit: they
/// restore a bare flag on a miss, which would rebuild the ORIGINAL exception
/// and drop the error. It is decided by identity against this function's own
/// pin of `exc` (which `run_jit_callee_handler`'s search never writes), so a
/// relocation alone does not read as a replacement.
///
/// SAFETY: same contract as `route_implicit_exc_through_callee`.
unsafe fn try_run_callee_handler(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    exc: cratonvm_types::ObjectRef,
    throw_pc: usize,
) -> Result<
    i64,
    (
        crate::runtime::interpreter::CalleeHandlerMiss,
        cratonvm_types::ObjectRef,
        bool,
    ),
> {
    // `exc` arrives here having already been DRAINED out of
    // `thread.jit_pending_exception` by the caller, so for the length of this
    // function it is a bare Rust local — the one heap reference to a live
    // throwable that no root provider knows about. `run_jit_callee_handler`
    // runs arbitrary Java before it pushes `exc` onto the resumed frame's
    // operand stack. Pin it across that. (gen r4w4/rooting2: an earlier note
    // here said `resolve_callee_cached` could run a user `loadClass`. It
    // cannot -- it is a class-manager READ-lock lookup, `find_class_by_name_*`
    // plus `find_method_recursive`, and loads nothing -- so it is not a GC
    // point, and decoding `args_slice` after it is sound. The pin is kept for
    // the handler run.) `native_pin_roots` is the right home: it is
    // both scanned (`memory/roots.rs`) and remapped (`memory/gc.rs`), so we
    // re-read the slot afterwards to pick up a relocation instead of handing
    // the interpreter a from-space address.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    // The arguments are decoded BEFORE the resolve, while `args_slice` is
    // current (the caller's contract), and rooted across it the same way, for
    // the same reason: decoding after a collection in the resolve read
    // pre-move words (r11w4-rt-callee-rerun-reads-pre-call-argument-words).
    let mut args = decode_dispatch_values(vm, info, args_slice);
    let arg_pins = CompileArgPins::pin_values(vm, thread, &args);
    let resolved = resolve_callee_cached(vm, info, receiver_class_id);
    // Truncates to the argument pins' own watermark, above `exc`'s.
    arg_pins.release_values(thread, &mut args);
    let Some(cached) = resolved else {
        // Re-read through the pin: the resolve above may have collected.
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        // Nothing was consulted, so nothing is known — keep the caller's
        // pre-existing conservative fallback rather than claiming "not caught".
        // j27 wave 4 (SDR17-1): named apart from a frame `Declined`, for the
        // refusal message (`callee_rerun_refusal_what`).
        return Err((
            crate::runtime::interpreter::CalleeHandlerMiss::Unresolved,
            exc,
            false,
        ));
    };
    let exc = thread.native_pin_roots[pin_base];
    let res = crate::runtime::interpreter::run_jit_callee_handler(
        vm, thread, &cached, throw_pc, exc, &args,
    );
    // The original, as the collector left it: `run_jit_callee_handler` pins its
    // own copy and only that copy can be replaced.
    let original = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    let res = res.map_err(|(why, now)| (why, now, now != original))?;
    // The callee's handler ran here, in the interpreter. When the body the
    // cache publishes for it is an optimizing one, that is a catch the body
    // could not run itself (round 12 wave 3, lane exc).
    note_optimizing_callee_catch(vm, &cached);
    Ok(match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    })
}

// SAFETY: `vm` must be live for the call. The body only takes a read lock on
// the class manager and looks the callee up by name; it dereferences no raw
// pointer of its own. The `unsafe` marker is the shared JIT-callback contract
// (the caller is JIT-generated code holding a valid `SharedVm`), not an
// additional obligation of this function.
unsafe fn callee_has_exception_table(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    // A memoized handler template exists only for a callee with a non-empty
    // table, so a hit answers without the lock or the hierarchy walk.
    if callee_handler_template_hit(vm, info, ClassId::new(0)).is_some() {
        return true;
    }
    // The NEGATIVE answer, which the template memo cannot give: every
    // exception crossing a compiled caller asks this about each callee it
    // leaves, and a callee without a table used to pay the lock, a by-name
    // class lookup and a hierarchy walk per crossing, per throw.
    let stamp = callee_table_verdict_stamp(vm);
    if let Some(has_table) = callee_table_verdict_hit(vm, info, false, 0, stamp) {
        return has_table;
    }
    let answer = callee_has_exception_table_uncached(vm, info);
    if let Some(has_table) = answer {
        remember_callee_table_verdict(vm, info, false, 0, stamp, has_table);
    }
    answer.unwrap_or(false)
}

/// [`callee_has_exception_table`]'s resolution. `None` when the class or the
/// method did not resolve, an answer that is not remembered.
///
/// SAFETY: as [`callee_has_exception_table`].
unsafe fn callee_has_exception_table_uncached(vm: &SharedVm, info: &JitInvokeInfo) -> Option<bool> {
    // A substituted owner by its recorded id; asked before the guard below,
    // which `jit_substituted_owner` would otherwise take a second time.
    let substituted = jit_substituted_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let class_id = if substituted.is_some() {
        substituted
    } else if info.declaring_class_id == 0 {
        cm.find_bootstrap_class_by_name(info.class_name)
    } else {
        cm.find_cp_class_for_class(info.class_name, ClassId::new(info.declaring_class_id))
    };
    let class_id = class_id?;
    let store = cm.class_store();
    let (method, _declaring_id) = crate::classloading::find_method_recursive(
        class_id,
        info.method_name,
        info.descriptor,
        store,
    )?;
    Some(
        method
            .code()
            .map_or(false, |c| !c.exception_table.is_empty()),
    )
}

/// One remembered answer of [`callee_has_exception_table`] or
/// [`mic_callee_has_exception_table`] (round 12 wave 3, lane exc).
///
/// Keyed by the `JitInvokeInfo` ADDRESS for the lookup, but validated against
/// the site's CONTENT: an info freed with its body and re-issued at the same
/// address names another site, and must not be served this one's answer --
/// the defect `site_keyed_memos!` exists for. Every input the resolution reads
/// is in the entry, so a hit is exactly the resolution: the three names, the
/// two class-id hints and the kind (what `jit_substituted_owner` and the
/// by-name lookup read), the receiver class (what the selection reads), and a
/// stamp of what can change a name's meaning or a method's table -- the VM's
/// class definition epoch (a class defined or unloaded) and the process's
/// redefinition count. `selected` keeps the two resolutions apart: the
/// receiver-selecting one ([`mic_callee_has_exception_table`]) and the by-name
/// one may answer differently for one site.
struct CalleeTableVerdict {
    vm_identity: usize,
    selected: bool,
    receiver: u32,
    declaring: u32,
    owner: u32,
    kind: u8,
    class_name: Box<str>,
    method_name: Box<str>,
    descriptor: Box<str>,
    stamp: (u64, u64),
    has_table: bool,
    /// JIT round 16 wave 12 (lane throw3, TP16-14): the JIT cache's retire
    /// count ([`callee_verdict_retire_epoch`]) when this entry last proved
    /// it names the info at its address -- made, or content-validated.
    retire_epoch: u64,
}

impl CalleeTableVerdict {
    /// Everything but the three names: integer compares.
    fn scalars_match(
        &self,
        vm_identity: usize,
        info: &JitInvokeInfo,
        selected: bool,
        receiver: u32,
        stamp: (u64, u64),
    ) -> bool {
        self.stamp == stamp
            && self.vm_identity == vm_identity
            && self.selected == selected
            && self.receiver == receiver
            && self.declaring == info.declaring_class_id
            && self.owner == info.owner_class_id
            && self.kind == info.invoke_kind
    }

    fn names(
        &self,
        vm_identity: usize,
        info: &JitInvokeInfo,
        selected: bool,
        receiver: u32,
        stamp: (u64, u64),
    ) -> bool {
        self.scalars_match(vm_identity, info, selected, receiver, stamp)
            && *self.class_name == *info.class_name
            && *self.method_name == *info.method_name
            && *self.descriptor == *info.descriptor
    }

    /// The verdict for `info`, or `None` (a miss), and whether the names had
    /// to be compared and matched under an `epoch` (the caller then stamps
    /// the entry with it).
    ///
    /// JIT round 16 wave 12 (lane throw3, TP16-14): with an `epoch` (the
    /// switch on) an entry proved at that retire epoch answers on its integer
    /// fields alone -- see [`callee_verdict_retire_epoch`] for why the address
    /// then still names the same, unchanged info. Without one, or at another
    /// epoch, the names are compared exactly as before.
    fn answer(
        &self,
        vm_identity: usize,
        info: &JitInvokeInfo,
        selected: bool,
        receiver: u32,
        stamp: (u64, u64),
        epoch: Option<u64>,
    ) -> (Option<bool>, bool) {
        if epoch == Some(self.retire_epoch) {
            let hit = self.scalars_match(vm_identity, info, selected, receiver, stamp);
            return (hit.then_some(self.has_table), false);
        }
        let hit = self.names(vm_identity, info, selected, receiver, stamp);
        (hit.then_some(self.has_table), hit && epoch.is_some())
    }
}

/// The JIT cache's retire count: the high half of
/// `cratonvm_jit::jit_cache_generation()` ([`JIT_RETIRE_SHIFT`]; the whole word
/// if the shift were 0, which only makes every publication count as a retire).
///
/// JIT round 16 wave 12 (lane throw3, TP16-14). A `JitInvokeInfo` freed with
/// its `CompiledMethod` is freed only after `Drop for CompiledMethod` bumped
/// this count (see [`flush_raw_entry_dispatch_caches`]), so an address that
/// named site A while the count read `e` still names A -- the same, unchanged
/// allocation -- while it still reads `e`. A verdict made (or re-validated)
/// at `e` therefore needs no name compare at `e`: identity before content.
fn callee_verdict_retire_epoch() -> u64 {
    cratonvm_jit::jit_cache_generation()
        .checked_shr(JIT_RETIRE_SHIFT)
        .unwrap_or(0)
}

/// Clear-on-full bound for [`CALLEE_TABLE_VERDICTS`].
const CALLEE_TABLE_VERDICT_CAP: usize = 1024;

thread_local! {
    /// `(vm_identity, info address, selected, receiver class or 0)` -> the
    /// verdict. Per thread, like every other dispatch memo: no lock on the
    /// throw path.
    static CALLEE_TABLE_VERDICTS: std::cell::RefCell<
        // addr-keyed: not a heap address -- VM identity + `JitInvokeInfo` (retire epoch).
        rustc_hash::FxHashMap<(usize, usize, bool, u32), CalleeTableVerdict>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// The stamp a [`CalleeTableVerdict`] is valid under. Read BEFORE resolving,
/// so a definition that lands during the resolution leaves an entry that is
/// already stale rather than one that looks current.
fn callee_table_verdict_stamp(vm: &SharedVm) -> (u64, u64) {
    (
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
        cratonvm_classloading::class_redefinition_count(),
    )
}

/// `CRATONVM_JIT_CALLEE_TABLE_MEMO` -- default ON; `0` resolves every time, as
/// before. Cached: read on the throw path.
fn callee_table_memo_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CALLEE_TABLE_MEMO")
    })
}

fn callee_table_verdict_hit(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    selected: bool,
    receiver: u32,
    stamp: (u64, u64),
) -> Option<bool> {
    if !callee_table_memo_enabled() {
        return None;
    }
    let key = (
        vm.vm_identity,
        info as *const JitInvokeInfo as usize,
        selected,
        receiver,
    );
    // TP16-14 (`CRATONVM_JIT_CALLEE_VERDICT_IDENTITY=1`, default OFF): an
    // entry proved at the current retire epoch answers on its integer fields.
    let epoch = vm
        .threads
        .throwable_layout_cache
        .callee_verdict_identity_enabled()
        .then(callee_verdict_retire_epoch);
    CALLEE_TABLE_VERDICTS.with(|memo| {
        let mut memo = memo.try_borrow_mut().ok()?;
        let entry = memo.get_mut(&key)?;
        let (answer, refresh) =
            entry.answer(vm.vm_identity, info, selected, receiver, stamp, epoch);
        if let (true, Some(epoch)) = (refresh, epoch) {
            entry.retire_epoch = epoch;
        }
        answer
    })
}

fn remember_callee_table_verdict(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    selected: bool,
    receiver: u32,
    stamp: (u64, u64),
    has_table: bool,
) {
    if !callee_table_memo_enabled() {
        return;
    }
    let key = (
        vm.vm_identity,
        info as *const JitInvokeInfo as usize,
        selected,
        receiver,
    );
    CALLEE_TABLE_VERDICTS.with(|memo| {
        let Ok(mut memo) = memo.try_borrow_mut() else {
            return;
        };
        if memo.len() >= CALLEE_TABLE_VERDICT_CAP && !memo.contains_key(&key) {
            memo.clear();
        }
        // Read while `info` is live (the caller is crossing its site), so the
        // epoch is one at which the address names this info (TP16-14).
        let retire_epoch = callee_verdict_retire_epoch();
        memo.insert(
            key,
            CalleeTableVerdict {
                retire_epoch,
                vm_identity: vm.vm_identity,
                selected,
                receiver,
                declaring: info.declaring_class_id,
                owner: info.owner_class_id,
                kind: info.invoke_kind,
                class_name: Box::from(info.class_name),
                method_name: Box::from(info.method_name),
                descriptor: Box::from(info.descriptor),
                stamp,
                has_table,
            },
        );
    });
}

/// BUG-H (virtual sibling of [`callee_has_exception_table`]): does the
/// receiver-resolved virtual/interface callee declare a non-empty exception
/// table? The MIC hit path knows the callee only by `(info.method_name,
/// descriptor)` + the runtime receiver class, so resolution starts from the
/// receiver's class id (the real override site).
///
/// SAFETY: as [`callee_has_exception_table`]; `receiver_class_id` must be a
/// live class id obtained from the receiver object.
unsafe fn mic_callee_has_exception_table(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    // As `callee_has_exception_table`. Only for a real receiver class: with
    // none, the memo's entry was resolved by name, which this probe never does.
    if receiver_class_id.as_u32() != 0
        && matches!(info.invoke_kind, 0 | 2)
        && callee_handler_template_hit(vm, info, receiver_class_id).is_some()
    {
        return true;
    }
    // The negative answer, as in `callee_has_exception_table`; keyed on the
    // receiver class too, which is what the selection below reads.
    let stamp = callee_table_verdict_stamp(vm);
    let receiver = receiver_class_id.as_u32();
    if let Some(has_table) = callee_table_verdict_hit(vm, info, true, receiver, stamp) {
        return has_table;
    }
    // The method the receiver's class runs by §5.4.6, as the dispatch doors
    // select it (`receiver_selection_retarget`): the question is about the
    // body a publication would enter.
    let answer = {
        let cm = vm.classes.class_manager.read();
        select_for_jit_receiver(&cm, receiver_class_id, info).map(|(method, _decl)| {
            method
                .code()
                .map_or(false, |c| !c.exception_table.is_empty())
        })
    };
    if let Some(has_table) = answer {
        remember_callee_table_verdict(vm, info, true, receiver, stamp, has_table);
    }
    answer.unwrap_or(false)
}

/// Before the dispatch helper publishes a compiled callee into a site's MIC or
/// PIC: stamp the site's `callee_rerun_pins` flag when that callee declares an
/// exception table, so the helper's own MIC-hit and megamorphic-PIC arms root
/// the call's arguments across a sentinel that can re-run it
/// (r11w4-rt-callee-rerun-reads-pre-call-argument-words).
///
/// The table probe (a class-manager read lock) runs only on a publication, only
/// while table callees are publishable at all, and only until the sticky flag
/// is set, so the hit paths pay one relaxed load. `publishes` is false when
/// there is no entry to publish.
///
/// SAFETY: as [`mic_callee_has_exception_table`].
unsafe fn mark_site_rerun_pins_before_publish(
    vm: &SharedVm,
    mic: &JitMICSlot,
    receiver_cid: u32,
    info: &JitInvokeInfo,
    publishes: bool,
) {
    if publishes
        && mic_publish_exception_table_callees()
        && !mic.callee_rerun_pins()
        && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info)
    {
        mic.mark_callee_rerun_pins();
    }
}

/// BUG-H fix: route an *implicit* runtime exception thrown by a directly
/// invoked compiled callee through the CALLEE's own exception table.
///
/// When a JIT-compiled callee `B` is invoked directly — either via a
/// machine-code `CALL` to its entry or through one of this helper's
/// `try_call_compiled_entry` fast paths — and it throws an implicit runtime
/// exception (`ArrayIndexOutOfBoundsException` / `NullPointerException`), `B`'s
/// own in-method `catch` is never consulted: `B` returns the `i64::MIN` deopt
/// sentinel with the thread-local pending-exception flag set, and that flag is
/// only drained at the *outermost* interpreter↔JIT boundary — which routes it
/// through the wrong method's exception table (the `TestHexUtils` /
/// `HexUtils.getDec` escape: `T[i-'0']` inside `catch (AIOOBE)` returning -1).
///
/// If `rc` is the deopt sentinel, an implicit AIOOBE/NPE is pending, and the
/// callee declares a non-empty exception table, re-execute the callee in the
/// interpreter with the same args. The interpreter re-throws the same
/// exception at the same bytecode and routes it through the callee's table —
/// running its handler (which may swallow the exception and return a normal
/// value) or re-propagating it to the caller via `handle_jit_dispatch_error`.
/// Re-execution from the start mirrors the existing whole-method deopt
/// semantics; it is confined to the (cold) exceptional path of a method that
/// actually declares a handler region, so straight-line callees are untouched.
///
/// `args_slice` is the call's argument block as the caller read it, and
/// `pins`, when present, roots its references ([`call_compiled_then_route`]).
/// Every re-run below reads the words back through the pins ([`args_now`]) at
/// the point it uses them, because the callee, the throwable this function
/// allocates and a refused deopt-frame rebuild can each collect first.
/// `receiver_class_id` is the receiver's class read BEFORE the call — a class
/// id cannot go stale, a receiver word can — and is used only for the
/// virtual/interface kinds.
///
/// SAFETY: same contract as the surrounding `jit_invoke_dispatch` fast paths —
/// `vm`/`info`/`args_slice` are the live values passed by the JIT caller, and
/// no `jit_thread_mut` borrow is live at the call site.
///
/// HANDLER11-3 (JIT round 19 wave 12, lane handler12): a call this route
/// completes with a value over the stash entry it saw on entry left an orphan
/// ([`note_completed_call_left_stash`], `dispatch-callee-route`).
#[inline]
unsafe fn route_implicit_exc_through_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver_class_id: ClassId,
    rc: i64,
    pins: Option<&CompileArgPins>,
) -> i64 {
    if rc != i64::MIN {
        return rc;
    }
    let entry_top = cratonvm_jit::deopt::deopt_stash_top_seq();
    let v =
        route_implicit_exc_through_callee_inner(vm, info, args_slice, receiver_class_id, rc, pins);
    if v != i64::MIN {
        note_completed_call_left_stash(entry_top, "dispatch-callee-route");
    }
    v
}

/// [`route_implicit_exc_through_callee`] for a sentinel, without the
/// HANDLER11-3 orphan note.
///
/// SAFETY: as [`route_implicit_exc_through_callee`].
unsafe fn route_implicit_exc_through_callee_inner(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver_class_id: ClassId,
    rc: i64,
    pins: Option<&CompileArgPins>,
) -> i64 {
    // The callee has returned. Any `athrow` bci the pending exception carries is
    // ITS bci, and every path out of here either re-runs the callee interpreted
    // (which regenerates the exception) or hands the sentinel to the compiled
    // CALLER, whose drain would treat that bci as its own. See
    // `clear_jit_athrow_bci`.
    //
    // CAPTURE IT FIRST. Being the callee's own bci is exactly what makes it
    // right for the KCFULL-13 branch below, which routes through THAT callee's
    // own exception table — the one lookup in this function for which the pc is
    // not foreign at all. Clearing it before the branch read it left
    // `throw_pc = usize::MAX`, so `find_jit_exception_handler` took its
    // pc-unknown path, which deliberately skips a catch-all whose region does
    // not span the whole method — i.e. every javac `finally`. The resume-at-
    // handler fix (`run_jit_callee_handler`) could then never fire: measured
    // 4213 entries to the branch and 0 handlers found, with the fall-through
    // re-running the callee from entry and leaking one increment per throw
    // (`CallPathProbe` IFACE-class-delegating, `FinallyBalanceProbe`).
    //
    // The clear itself stays, so every path that propagates outward or re-runs
    // the callee behaves exactly as before; only the branch that consumes the
    // bci for the callee's OWN table sees it.
    let callee_throw_bci = peek_jit_athrow_bci();
    clear_jit_athrow_bci();
    // gce e1/f: the pending throwable already left a not-entrant re-dispatch
    // of this callee (see `handle_compiled_callee_deopt_sentinel`). Then the
    // general-exception branch below must neither search the callee's table
    // again nor re-run the callee from entry.
    let escaped_redispatch = take_redispatch_escape_for(info);
    // Taken from the caller, which read it before the call. Re-deriving it
    // from `args_slice[0]` here read a receiver word that a collection inside
    // the callee may have moved -- unrooted when the callee has no exception
    // table -- and a wrong class here picks the handler table and the resume
    // identity of a different method.
    let receiver_class_id = if matches!(info.invoke_kind, 0 | 2) {
        receiver_class_id
    } else {
        ClassId::new(0)
    };
    let callee_has_handler = if matches!(info.invoke_kind, 0 | 2) {
        mic_callee_has_exception_table(vm, receiver_class_id, info)
    } else {
        callee_has_exception_table(vm, info)
    };
    if rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_implicit_exc_through_callee ENTER {}.{}{} has_last_deopt={} pending_exc={} pending_npe_or_aioobe_unread=?",
            info.class_name,
            info.method_name,
            info.descriptor,
            cratonvm_jit::deopt::has_last_deopt(),
            jit_pending_exception_is_set(),
        );
    }
    // jit-invokedynamic-groovy-regression fix — FIRST chance: a frame-stashing
    // deopt (the unconditional invokedynamic reason-8 trap, or a precise guard
    // bail) in the compiled callee THIS helper just invoked. Resume the callee
    // precisely at its trapping bci and hand the real result back to the
    // compiled caller — no side-effect re-run, no sentinel escape. Refusals
    // (identity mismatch / unmappable / no stash) leave the stash + sentinel
    // to propagate exactly as before. The cheap `has_last_deopt` pre-check
    // keeps the common exception path free of the thread-guard acquire.
    if cratonvm_jit::deopt::has_last_deopt() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(v) = try_resume_trapped_callee(vm, thread, info, receiver_class_id) {
                return v;
            }
        }
    }
    // Did the callee raise an *implicit* runtime exception?
    //
    // THREE flags, not two. `/ by zero` is stashed by the same machinery as
    // the other two (`stash_jit_pending_arithmetic`, from the compiled `idiv`
    // /`irem` guard and from `handle_compiled_callee_deopt_sentinel`'s
    // fall-through), and this function used to consult only AIOOBE and NPE:
    // an ArithmeticException fell into the `aioobe.is_none() && !npe` branch
    // below, found no *general* pending exception either (the flag is not one),
    // and left through the bare `return rc` — the callee's own exception table
    // never consulted, the sentinel handed to the next frame out.
    //
    // Measured on `probes/EscapeStaticProbe.java`: from the iteration where the
    // caller is compiled to the end of the loop, 198 000 of 200 000 `catch
    // (ArithmeticException)` in the CALLER were skipped and the throw surfaced
    // one frame too high. `route_implicit_exc_through_callee ENTER … pending_exc
    // =false has_last_deopt=false` was printed 198 000 times, which is this
    // function being handed a signal it had no arm for. The sibling door
    // (`handle_compiled_callee_deopt_sentinel`) has always had the arm; the two
    // doors simply disagreed.
    let aioobe = take_jit_pending_aioobe();
    let npe = if aioobe.is_none() {
        take_jit_pending_npe()
    } else {
        false
    };
    let arithmetic = if aioobe.is_none() && !npe {
        take_jit_pending_arithmetic()
    } else {
        false
    };
    // ONE classification, used for both questions this function asks of the
    // three flags — which signal, and whether there is one. See
    // [`ImplicitSignal`]: the second question used to be a separate,
    // hand-written test, and that is what let the arm above go missing here
    // while the sibling door had it.
    let implicit = implicit_signal_of(aioobe, npe, arithmetic);
    if rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_implicit_exc_through_callee IMPLICIT {implicit:?} \
             callee_has_handler={callee_has_handler} {}.{}{}",
            info.class_name, info.method_name, info.descriptor,
        );
    }
    // Re-stash the consumed flag and propagate the sentinel unchanged. Used for
    // the pure-deopt case (no flag) and the no-local-handler case (the callee
    // cannot catch it, so existing outward propagation is already correct).
    let restash_and_return = || {
        restash_implicit_signal(implicit);
        rc
    };
    if implicit == ImplicitSignal::None {
        // KCFULL-13 — *general* pending exception (an explicit `athrow`, or a
        // native-raised throwable, originating in this compiled callee's own
        // callee chain). The original bug-H assumption — "methods-with-tables
        // that `athrow` are never compiled, so a general `JIT_PENDING_EXCEPTION`
        // can't reach a direct compiled call" — only holds for a method that
        // throws *itself*. A method that merely *catches* (declares an
        // exception table but contains no `athrow`) passes the `has_athrow`
        // compile gate and IS compiled; yet the JIT still cannot dispatch to
        // its in-method handler, so an exception propagating up from an
        // (interpreted) sub-callee silently skips its `catch` and escapes to
        // the outermost frame. Canonical victim: keycloak's
        // `CryptoIntegration` chain — JIT'd `getSelectedProvider` wraps an
        // interpreted `CryptoIntegration.getProvider()` (kept interpreted by
        // the string-ldc gate) in `try { ... } catch (IllegalStateException)`,
        // and the ISE escaped the catch under JIT.
        //
        // Mirror the implicit path: if the compiled callee declares a handler,
        // re-execute it in the interpreter so its exception table runs. The
        // interpreter re-run regenerates and routes the exception, so the
        // stale stashed copy is dropped first to avoid a double-drain.
        if jit_pending_exception_is_set() && callee_has_handler && !escaped_redispatch {
            if let Some((thread, _guard)) = jit_thread_mut() {
                // FIRST: resume the callee AT its own handler. The re-run below
                // double-executes everything the compiled attempt already did
                // before the throw -- for `try { n++; mayThrow(); } finally
                // { n--; }` that is one leaked increment per throw, because the
                // compiled body ran `n++` and skipped the `finally`, and the
                // re-run then adds its own balanced pass. Witnessed by
                // `CallPathProbe`/`FinallyShapeProbe`
                // (docs/known-issues/repros/jitban-remaining-20260726/).
                let signals = take_all_jit_signals(thread);
                if let Some(exc) = signals.exception {
                    // `signals.athrow_bci` is always -1 here: the entry clear
                    // above ran before this drain. Fall back to the value
                    // captured before it — the callee's own throw site, which
                    // is what this lookup against the callee's own table needs.
                    let stamped_bci = if signals.athrow_bci >= 0 {
                        signals.athrow_bci
                    } else {
                        callee_throw_bci
                    };
                    // i105 L2: an explicit throwable with no stamp has
                    // already left the callee (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`).
                    let throw_pc = if stamped_bci >= 0 {
                        stamped_bci as usize
                    } else {
                        crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT
                    };
                    let throw_pc_unknown =
                        throw_pc >= crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT;
                    let handler_args = args_now(pins, thread, args_slice);
                    // j27 wave 4 (SDR17-1): the only arm that does not return
                    // answers what the replay refusal below names.
                    let refusal_what = match try_run_callee_handler(
                        vm,
                        thread,
                        info,
                        receiver_class_id,
                        &handler_args,
                        exc,
                        throw_pc,
                    ) {
                        Ok(v) => {
                            // The handler ran and the call is complete, so any
                            // exceptional frame the callee's compiled body
                            // published describes a FINISHED attempt. Drop it here
                            // as well as on the fall-through below. Leaving it
                            // stashed keeps a heap reference alive for an unbounded
                            // time, and lets a later drain for the same method
                            // claim it — the match compares method names only.
                            clear_own_exceptional_frame(vm, thread, info);
                            return v;
                        }
                        // Only trustworthy with a KNOWN throw pc. The
                        // pc-unknown search deliberately under-reports — it
                        // skips a catch-all whose region does not span the whole
                        // method, i.e. every javac `finally` — so "no handler"
                        // there means "cannot tell", and propagating on it would
                        // skip cleanup the re-run does perform. bc-java's
                        // `SymmetricConstraintsTest` is the witness: its
                        // `finally` restores a PROCESS-WIDE
                        // `CryptoServicesRegistrar` constraint, and losing it
                        // failed all 14 `HPKETestVectors` cases afterwards with
                        // "service does not provide 192 bits of security".
                        //
                        // `run_jit_callee_handler` makes that distinction
                        // itself (`Unknown` vs `NotCaught`), against the pc its
                        // lookup ACTUALLY used. It used to be made here from
                        // `throw_pc`, the athrow stamp -- which is always
                        // unknown for a callee compiled with precise exception
                        // frames, because its protected throw sites exit
                        // through the reason-9 stub and publish a frame instead
                        // of stamping. The lookup ran against that frame's
                        // exact bci, found nothing, and the answer was
                        // discarded for a re-run from entry. See
                        // `CalleeHandlerMiss`.
                        // Every miss hands back the throwable to propagate or
                        // restore, re-read from a pin (the search can collect)
                        // and possibly a catch type's resolution error
                        // (JVMS §5.4.3), so it shadows the drained `exc`.
                        Err((
                            crate::runtime::interpreter::CalleeHandlerMiss::NotCaught,
                            exc,
                            _,
                        )) => {
                            // The callee's own table does not cover this throw:
                            // the JVM answer is to propagate to the caller, whose
                            // table has not been consulted yet. Falling through to
                            // the whole-method re-run below would execute the
                            // callee's prefix a SECOND time — and for a prefix
                            // that is not idempotent that does not merely
                            // duplicate work, it can swallow the exception
                            // outright. `CipherInputStream.nextChunk` is the
                            // witness: `finaliseCipher()` sets `finalized = true`
                            // and THEN throws on a bad AEAD tag, so the re-run
                            // takes the `if (finalized) return -1` exit and the
                            // caller reads a clean EOF over tampered ciphertext
                            // (bc-java `CipherStreamTest`).
                            if throw_pc_unknown {
                                set_jit_pending_exception(thread, exc);
                            } else {
                                set_jit_pending_exception_with_bci(thread, exc, throw_pc as i64);
                            }
                            // The callee is gone; its exceptional frame can never
                            // be claimed and must not be left for a later drain.
                            clear_own_exceptional_frame(vm, thread, info);
                            return rc;
                        }
                        Err((why, exc, _)) => {
                            // A handler matched but could not be resumed with
                            // correct locals, or the throw pc is unknown so the
                            // "not caught" answer above cannot be trusted.
                            // Restore the signal exactly as it was found and fall
                            // through to the whole-method re-run, which is this
                            // branch's pre-existing answer.
                            if throw_pc_unknown {
                                set_jit_pending_exception(thread, exc);
                            } else {
                                set_jit_pending_exception_with_bci(thread, exc, throw_pc as i64);
                            }
                            callee_rerun_refusal_what(&why)
                        }
                    };
                    // Round 13 wave 8 (lane replay4): the re-run below calls
                    // the callee's throwing sub-call again. Counted in the
                    // door census (`callee-exception`, uncounted before) and
                    // refused when the replay rule calls it unsound
                    // (`CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES`).
                    let callee = callee_rerun_code(vm, info, receiver_class_id);
                    let exact =
                        callee_exception_rerun_is_exact(vm, callee.as_ref(), info, throw_pc);
                    let cause = crate::runtime::interpreter::DoorRerunCause::CalleeException;
                    crate::runtime::interpreter::note_door_rerun(cause, exact);
                    if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
                        callee_rerun_class_redefined(vm, callee.as_ref())
                    }) {
                        let _ = take_jit_pending_exception(thread);
                        clear_own_exceptional_frame(vm, thread, info);
                        return refuse_callee_rerun(
                            vm,
                            thread,
                            info,
                            callee.as_ref(),
                            cause,
                            refusal_what,
                        );
                    }
                }
                let _ = take_jit_pending_exception(thread);
                // The callee is about to be re-executed from its entry in the
                // interpreter, which regenerates and routes the exception
                // itself. Any exceptional frame its compiled body published
                // describes the abandoned attempt — drop it with the stashed
                // exception rather than leave it to be mis-claimed.
                clear_own_exceptional_frame(vm, thread, info);
                let bail_args =
                    decode_dispatch_values(vm, info, &args_now(pins, thread, args_slice));
                if rbc6_dbg() {
                    eprintln!(
                        "[rbc6-dbg] route_implicit_exc_through_callee KCFULL-13 bail_to_interpreter {}.{}{}",
                        info.class_name, info.method_name, info.descriptor
                    );
                }
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
        }
        // JIT round 17 wave 3 (lane syncdoor3,
        // `r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`):
        // the throwable already left a not-entrant re-dispatch of this callee
        // (gce e1/f), so no activation of the callee will ever claim a frame
        // naming it: one still on top is an orphan of that re-dispatch, and
        // what it names (a handed method monitor, a block lock) would be
        // released by nobody once a later take discarded it beneath a newer
        // frame. Take it and release, under the exceptional hand-over's
        // switches only; off, the frame stays for the caller's sinks as before.
        if escaped_redispatch
            && jit_pending_exception_is_set()
            && crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released()
        {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let _ = clear_own_exceptional_frame(vm, thread, info);
            }
        }
        // A FRAMELESS trap in the callee: `jit_uncommon_trap` raised the deopt
        // flag and returned the sentinel with nothing stashed, no implicit
        // signal and no throwable. Passing it through bails the compiled
        // CALLER's whole activation, and the door that entered the caller can
        // only re-run it from entry (`run_jit_body`'s `Deopt`) or, for an OSR'd
        // caller, replay every iteration it committed (`try_osr`'s "no frame"
        // safe reject). The callee is the one frame that lost its work, and the
        // one frame this helper can re-run cheaply: its arguments are still
        // here. Re-run it interpreted and hand its result — value or pending
        // throwable — to the caller as an ordinary return, which is what an
        // interpreted caller does with the same trap. The flag is per-call
        // fresh (`call_compiled_entry_under_owner` retires it before the callee
        // is entered), so it cannot be a stale flag beside a genuine
        // `Long.MIN_VALUE`. A stash left here (one `try_resume_trapped_callee`
        // refused) is NOT frameless and keeps propagating as before.
        // Interpreter round i1 wave 5, L7:
        // `i4-L7-proposal-service-bare-callee-traps-at-the-dispatch-helper`.
        if frameless_callee_trap_pending() {
            if let Some((thread, _guard)) = jit_thread_mut() {
                return service_frameless_callee_trap(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    args_slice,
                    pins,
                );
            }
        }
        // The stash `try_resume_trapped_callee` declined above, when it is this
        // site's callee's: re-run the callee here rather than hand its frame to
        // a caller sink that can only replay more
        // (r11w12-tier-helpers-declined-callee-rerun-patch).
        if !jit_pending_exception_is_set() && cratonvm_jit::deopt::has_last_deopt() {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Some(v) = rerun_declined_callee_from_entry(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    args_slice,
                    pins,
                ) {
                    return v;
                }
            }
        }
        return rc;
    }
    if !callee_has_handler {
        return restash_and_return();
    }
    // Round 12 wave 3 (lane exc): the callee's OWN throw pc, stamped by the pad
    // that raised the signal (`jit_throw_aioobe`'s bci, the NPE / `/ by zero`
    // stubs' `set_throw_bci`) or by the exit stub of the frame it propagated
    // out of. This arm used to hand `run_jit_callee_handler` `usize::MAX`, and
    // the pc-unknown search then matched a TYPED row by exception class alone,
    // whatever its range: `int v = a[i]; try { .. } catch
    // (ArrayIndexOutOfBoundsException e) { return -1; }` returned -1 for an
    // out-of-range `i` instead of throwing, and a narrow `finally` covering the
    // trap was skipped (`R12ExcCalleeThrowPc`). The
    // MIC door (`handle_compiled_callee_deopt_sentinel`) has always range-tested
    // with the stamp; the two doors now agree.
    let callee_pc = implicit_callee_throw_pc(callee_throw_bci);
    // A pc no row of the callee covers cannot be caught there. Answer that
    // BEFORE building the throwable: this arm used to build one, find no
    // handler, discard it and let the caller-side drain build it again.
    if let Some(pc) = callee_pc {
        if implicit_throw_is_uncovered(vm, info, receiver_class_id, pc) {
            return restash_and_return();
        }
    }
    // Materialize the implicit signal and resume the callee at its handler.
    // Re-entering at bytecode 0 replays every prefix side effect (and was the
    // source of the old finally/counter leak).
    if let Some((thread, _guard)) = jit_thread_mut() {
        // The exceptional frame the compiled attempt published is KEPT across
        // the allocation below. It used to be dropped here, and that is what
        // made RBC.6's `getfield`/`putfield` admission a miscompile: the
        // compiled body publishes a correct reason-9/10 frame at the trapping
        // bci, this line threw it away, and `run_jit_callee_handler` — finding
        // nothing — refused the handler and let the exception propagate past a
        // `catch` that catches it.
        //
        // The reason it was dropped no longer holds. `create_exception_object`
        // does allocate, and an allocation is a safepoint, but a
        // `ReconstructedFrame` in `LAST_EXCEPTIONAL` **is** a GC root now: the
        // scan half runs in `memory/roots.rs` §10
        // (`for_each_stashed_deopt_object`) and the remap half in
        // `memory/gc.rs` (`remap_stashed_deopt_objects`), paired by a debug
        // assertion that refuses one without the other. Both land on the thread
        // that owns the stash, which is this thread. `docs/jit/
        // deopt-thread-local-roots.md` names THIS window as the shortest
        // instance of the hazard it closed — a reason-9 frame published by
        // `emit_post_invoke_exception_check` whose sink allocates the throwable
        // before draining it — so keeping the frame here is precisely what that
        // wiring was for.
        //
        // Dropping it was never the "conservative" answer its old comment
        // claimed. Refusing to enter a handler is not a safe subset of entering
        // it: JVMS requires the handler to run, so the refusal is a wrong
        // answer that happens to be loud (an escaping exception) instead of
        // quiet (zeroed locals). The three OTHER exceptional-frame drops in
        // this function are different and stay: each of them runs on a path
        // where the compiled attempt is FINISHED or ABANDONED, so its frame can
        // never be legitimately claimed and must not be left for a later drain
        // to mis-match. Since N6 all three go through
        // `clear_own_exceptional_frame`, which drops the frame only if it
        // belongs to the callee this helper invoked -- being entitled to drop
        // a frame on this path is not the same as being entitled to drop
        // somebody else's.
        // The message text cannot diverge from
        // `handle_compiled_callee_deopt_sentinel`'s or from the interpreter's
        // own `idiv`, because there is one body — a `getMessage()` that changed
        // with the dispatch route would be its own wrong answer, and this used
        // to be kept true by hand.
        // This door drained the FLAG only (`take_jit_pending_npe`), so the
        // snapshot is still in the signal record; take it here so it is
        // consumed by exactly the throwable it belongs to.
        //
        // Taken for `Arithmetic` as well as `Npe` since 2026-09-05: those are
        // the two signals whose setters record a snapshot, and gating the take
        // on `Npe` alone left the arithmetic snapshot in the cell — where the
        // next take, belonging to a different throwable, would have found it.
        // So this was never only a missing feature; it was also a stale-frames
        // hazard, and the drain contract in
        // `take_jit_pending_trap_frames` says why that is the worse
        // half ("a trace that is confidently wrong is worse than one that is
        // short").
        let trap_frames = if matches!(
            implicit,
            ImplicitSignal::Npe | ImplicitSignal::Arithmetic | ImplicitSignal::Aioobe { .. }
        ) {
            take_jit_pending_trap_frames()
        } else {
            None
        };
        // The action code is PEEKED, not taken: this door drained the flag
        // only, and the drain that eventually services this NPE reads the same
        // cell.
        let npe_action = peek_jit_pending_npe_action();
        // Cloned for the discard path below. A snapshot is a handful of small
        // structs and this is the cold exception path; the alternative is
        // materialising the throwable twice to find out whether it is needed.
        let unused_frames = trap_frames.clone();
        let exc = materialize_implicit_signal(vm, thread, implicit, trap_frames, npe_action);
        if let Some(exc) = exc {
            // Read AFTER the allocation above, which can collect.
            let handler_args = args_now(pins, thread, args_slice);
            match try_run_callee_handler(
                vm,
                thread,
                info,
                receiver_class_id,
                &handler_args,
                exc,
                callee_pc.unwrap_or(usize::MAX),
            ) {
                Ok(v) => {
                    // The trap is SERVICED: its handler ran and `v` is the call's
                    // result. Retire the two cells this door read without taking —
                    // the out-of-band deopt flag the trapping stub raised beside
                    // the implicit one, and the peeked NPE action code — exactly as
                    // the sibling door's `take_all_jit_signals` does. A deopt flag
                    // left set is not inert: the compiled caller carries on, and
                    // the next `J`/`D` callee that legitimately returns
                    // `Long.MIN_VALUE` reads it through `jit_dispatch_threw` as a
                    // sentinel and bails its WHOLE activation — which an OSR'd
                    // caller's `try_osr` then safe-rejects, re-running every
                    // iteration since entry (interpreter round i1 wave 3, L7;
                    // `tools/probes/interp/L7/L7JitDoorOutcomes.java`).
                    let _ = take_jit_deopt_pending();
                    let _ = take_jit_pending_npe_action();
                    return v;
                }
                Err((_, error, true)) => {
                    // A covering row's catch type failed to resolve, and its
                    // resolution error REPLACED the implicit exception (JVMS
                    // §5.4.3; the interpreter unwinder and every other compiled
                    // route answer the same). Restashing the flag below would
                    // have the caller-side drain rebuild the ORIGINAL exception
                    // and drop the error, so the error escapes as an ordinary
                    // pending exception instead, exactly as the general arm
                    // above propagates a `NotCaught` miss. The implicit signal
                    // is retired with it: its flag and snapshot were taken
                    // above (the snapshot's clone goes unused), and the two
                    // cells the `Ok` arm retires are retired here too.
                    let _ = take_jit_deopt_pending();
                    let _ = take_jit_pending_npe_action();
                    set_jit_pending_exception(thread, error);
                    clear_own_exceptional_frame(vm, thread, info);
                    return rc;
                }
                Err(_) => {}
            }
        }
        // The callee's table did not take it, so the throwable built above is
        // DISCARDED and the caller-side drain will build another from the flag
        // `restash_and_return` is about to put back. Everything the discarded
        // one consumed has to go back with it — above all the compiled-frame
        // snapshot, which `materialize_implicit_signal` moved out. Without this
        // the second throwable is built from a bare flag and names only the
        // frames the interpreter still holds: `len=1 [main]` where HotSpot
        // reads five.
        restash_jit_pending_trap_frames(unused_frames);
    }
    restash_and_return()
}

/// The callee's stamped throw pc for [`route_implicit_exc_through_callee`]'s
/// implicit arm, or `None` for "unknown" (nothing stamped, or the switch off,
/// which restores the historical pc-unknown search).
fn implicit_callee_throw_pc(stamped: i64) -> Option<usize> {
    if stamped < 0 || !implicit_callee_throw_pc_enabled() {
        return None;
    }
    usize::try_from(stamped).ok()
}

/// `CRATONVM_JIT_IMPLICIT_CALLEE_THROW_PC` -- default ON; `0` restores the
/// pc-unknown handler search (and the build-then-discard) in
/// [`route_implicit_exc_through_callee`]'s implicit arm. Cached: throw path.
fn implicit_callee_throw_pc_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IMPLICIT_CALLEE_THROW_PC")
    })
}

/// Does no row of the callee's exception table cover `pc`, so that an
/// implicit exception raised there certainly propagates out of it?
///
/// `false` whenever that cannot be known without building the throwable: the
/// callee does not resolve to a handler template ([`resolve_callee_uncached`]:
/// no exception table, or `ACC_SYNCHRONIZED` while
/// `synchronized_callee_handlers_served()` is off -- a synchronized callee
/// with a table resolves under `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS`;
/// j27 wave 3 lane BACKLOG, SDR17-3), or a reason-9 frame of this callee is
/// stashed -- `run_jit_callee_handler`
/// range-tests THAT frame's bci in preference to the stamp, so the stamp alone
/// does not decide.
///
/// SAFETY: as [`route_implicit_exc_through_callee`].
unsafe fn implicit_throw_is_uncovered(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    pc: usize,
) -> bool {
    let own_frame_stashed =
        cratonvm_jit::deopt::peek_exceptional_identity().is_some_and(|(key, _)| {
            exceptional_frame_key_names(&key, info.method_name, info.descriptor)
        });
    if own_frame_stashed {
        return false;
    }
    let Some(callee) = resolve_callee_cached(vm, info, receiver_class_id) else {
        return false;
    };
    !callee
        .exception_table
        .iter()
        .any(|row| pc >= usize::from(row.start_pc) && pc < usize::from(row.end_pc))
}

/// jit-invokedynamic-groovy-regression fix — does the JIT cache hold a
/// compiled artifact for `(class, method, descriptor)` whose body contains an
/// unconditional `invokedynamic` uncommon trap (`has_indy_trap`)?
///
/// Entry-publication gates consult this: such an artifact must NEVER be
/// published where MACHINE CODE calls it directly (a baked JIT→JIT direct
/// call, a MIC/PIC inline-cache entry), because the trap's `i64::MIN`
/// sentinel + stashed frame would bail through the compiled CALLER's
/// epilogue, past the only point (the dispatch helper) that can resume the
/// callee precisely. Keyed exactly like the compile probes at each gate site
/// (the receiver class for MIC/PIC, the CP class for static direct calls) so
/// the lookup hits the same cache row `try_jit_compile_callee` filled.
pub(crate) fn compiled_entry_has_indy_trap(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // `&str`-only and resolved globally by name, so not loader-aware. A site
    // that holds the artifact it is about to publish reads `has_indy_trap`
    // off it instead (the MIC arms and the OSR direct bind do, since
    // interpreter round i1 wave 7); this probe remains for the MIC miss arm
    // when no body was compiled, where nothing callable is published.
    let class_id = vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let jit_cache = vm.jit.jit_cache.read();
    jit_cache
        .get(class_name, method_name, descriptor, class_id)
        .map_or(false, |c| c.has_indy_trap)
}

/// jit-invokedynamic-groovy-regression fix — precise, in-place resolution of a
/// compiled callee that deopted with a stashed frame, at the dispatch-helper
/// call site that invoked it.
///
/// When a compiled callee returns the `i64::MIN` sentinel because it hit a
/// frame-stashing deopt (the unconditional `invokedynamic` reason-8 trap, or a
/// precise guard bail), the stash describes the CALLEE's own frame at the
/// trapping bci. THIS helper — sitting directly between the compiled caller
/// and the callee — is the only place the callee can be resumed without
/// losing any caller's continuation: rebuild the callee's interpreter frame
/// from the stash, run it to completion, and hand the REAL result back to the
/// compiled caller as if the callee had returned normally. No side effect is
/// re-run, no sentinel escapes.
///
/// Refusal (returns `None`, stash left in place / restored) when:
///   * no stash, or the superseded-artifact sentinel (`bci == u32::MAX`);
///   * the stash's baked `method_key` does not name the SAME method this
///     helper invoked (`info`'s name+descriptor; the declaring class may be a
///     supertype of the call-site class for virtual dispatch, so the class is
///     taken from the key and verified to declare the method) — e.g. a
///     deeper machine-called method's guard deopt propagating through;
///   * the method is `ACC_SYNCHRONIZED` (conservative — monitor accounting
///     for the compiled prologue/epilogue vs the interpreter continuation is
///     not audited for this path);
///   * the frame is unmappable (`build_deopt_frame_inner` bails).
/// On refusal the sentinel propagates exactly as before — the outer sinks'
/// identity checks then despeculate + fall back to the safe re-run.
///

/// Two method descriptors name the same parameter list, ignoring the return
/// type.
///
/// The covariant-return BRIDGE case, and the reason `try_resume_trapped_callee`
/// cannot demand descriptor equality. `javac` emits, for
/// `class RowDataType implements DataType<SearchRow>`:
///
/// ```text
/// public Object      read(ByteBuffer)  { return read(b); }   // the bridge
/// public SearchRow   read(ByteBuffer)  { ...the real body... }
/// ```
///
/// A call site typed through the interface names the bridge's descriptor,
/// while the method that actually runs — and therefore the one that traps and
/// stashes a frame — is the covariant target. Requiring full descriptor
/// equality refuses precise resume for every such trap, and the stash then
/// reaches a sink that can only raise `InternalError`.
///
/// Matching on the parameter list alone keeps the guard's actual job (reject a
/// stash from an unrelated method) while admitting the one shape the JVM
/// guarantees is the same call: same name, same arity, same parameter types.
/// Nothing downstream trusts `info` — the callee is re-resolved from the
/// stash's own key and cross-checked against its declaring class.
/// Exceptional frames a sink declined to drop because they were not its own.
///
/// **Expected to stay at zero**, and the number that says whether the rule this
/// enforces was ever being broken. Non-zero means a dispatch helper abandoned
/// its compiled attempt while a DIFFERENT method's pending-exception frame was
/// stashed — a nested callee's, which the abandoning caller never published and
/// is not entitled to discard. Before N6 those were dropped silently.
static FOREIGN_EXCEPTIONAL_FRAMES_KEPT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`FOREIGN_EXCEPTIONAL_FRAMES_KEPT`].
pub fn foreign_exceptional_frames_kept() -> u64 {
    FOREIGN_EXCEPTIONAL_FRAMES_KEPT.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_DBG_DEOPT`, read ONCE — `runtime_flag_on`'s value rule, the same
/// as `runtime::interpreter::deopt_resume::dbg_deopt_enabled` (which is not
/// reachable from here).
///
/// Round 11 (lane `rt`): the eight deopt / trap / exceptional-frame sites in
/// this file re-read the variable on every event (`runtime_flag_on` is
/// uncached: a declared-name set probe plus a string rebuild), and a soft
/// `OsrExit` re-runs that path on every call.
#[inline]
fn dbg_deopt_on() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT"))
}

/// Frameless callee traps `route_implicit_exc_through_callee` serviced by
/// re-running only the callee, where the sentinel used to bail the compiled
/// caller's whole activation. The census the proposal asked for: nonzero on a
/// workload means callers (and OSR'd loops) that used to replay.
static BARE_CALLEE_TRAPS_SERVICED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`BARE_CALLEE_TRAPS_SERVICED`].
pub fn bare_callee_traps_serviced() -> u64 {
    BARE_CALLEE_TRAPS_SERVICED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Is the thread's signal state exactly a compiled callee's FRAMELESS trap —
/// the deopt flag, and nothing else: no pending throwable, no implicit
/// NPE/AIOOBE/`/ by zero` flag, no stashed frame (a stash a resume refused is
/// NOT frameless and keeps propagating)?
///
/// Only meaningful right after a RUST-armed compiled call, where
/// `call_compiled_entry_under_owner` retired the flag before the callee ran,
/// so a set flag is the callee's own and not a stale one beside a genuine
/// `Long.MIN_VALUE`. The inline machine-code cascade has no such retirement,
/// which is why `jit_service_callee_deopt` does not ask this.
#[inline]
fn frameless_callee_trap_pending() -> bool {
    !jit_pending_exception_is_set()
        && !cratonvm_jit::deopt::has_last_deopt()
        && JIT_SIGNALS.with(|s| {
            s.deopt.get() && !s.npe.get() && s.aioobe.get().is_none() && !s.arithmetic.get()
        })
}

/// Service a frameless callee trap ([`frameless_callee_trap_pending`]): take
/// the flag and re-run only the callee interpreted, handing its result — value
/// or pending throwable — to the compiled caller as an ordinary return. Passed
/// through instead, the sentinel bails the CALLER's whole activation, whose
/// door can only re-run it from entry or, for an OSR'd caller, replay every
/// iteration it committed. See
/// `i4-L7-proposal-service-bare-callee-traps-at-the-dispatch-helper`.
///
/// # Safety
/// As the dispatch helpers — `vm` live, `info` a live `JitInvokeInfo`,
/// `args_slice` the caller's outgoing argument slots.
///
/// Round 13 wave 8 (lane replay4): `receiver_class_id` (the receiver's class
/// read before the call, `ClassId::new(0)` for none) selects the override the
/// re-run dispatches to, so the replay verdict is asked of the method that
/// actually re-runs; and a re-run the verdict calls unsound is REFUSED
/// ([`refuse_callee_rerun`]) instead of committing the callee's prefix again.
#[cold]
unsafe fn service_frameless_callee_trap(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
) -> i64 {
    let _ = take_jit_deopt_pending();
    BARE_CALLEE_TRAPS_SERVICED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Round 12 wave 5 (lane replay3): the door census's `callee-frameless`
    // row. Round 13 wave 8 (lane replay4): judged by the trap site the stub
    // recorded, over the method the re-run dispatches to
    // (`frameless_callee_rerun_is_exact`); it used to be the whole-body rule
    // over the method the SITE names, which for a virtual site was the
    // interface's abstract method and read unsound whatever ran.
    let callee = callee_rerun_code(vm, info, receiver_class_id);
    let exact = frameless_callee_rerun_is_exact(vm, callee.as_ref(), info);
    let cause = crate::runtime::interpreter::DoorRerunCause::CalleeFrameless;
    crate::runtime::interpreter::note_door_rerun(cause, exact);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] frameless callee trap serviced at the dispatch \
             helper: re-running {}.{}{} interpreted, replay={} (site {:?})",
            info.class_name,
            info.method_name,
            info.descriptor,
            if exact { "exact" } else { "UNSOUND" },
            last_frameless_trap_site(),
        );
    }
    if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
        callee_rerun_class_redefined(vm, callee.as_ref())
    }) {
        return refuse_callee_rerun(vm, thread, info, callee.as_ref(), cause, "a frameless trap");
    }
    // Through the pins, like every other re-run here: a collection during the
    // call may have moved the arguments (round 11, `CompileArgPins`).
    let bail_args = decode_dispatch_values(vm, info, &args_now(pins, thread, args_slice));
    bail_to_interpreter(vm, thread, info, &bail_args)
}

/// The method a call-site service's re-run from entry dispatches to, with
/// its padded bytecode: `(declaring class id, declaring class name, code)`
/// (round 13 wave 8, lane replay4). The resolution
/// [`resolve_callee_uncached`] makes for the handler template (receiver
/// selection for kinds 0/2, the substituted owner, the site's loader), without
/// its "has a table, not synchronized" filter. `None` when nothing resolves,
/// or the method has no code.
///
/// SAFETY: as [`resolve_callee_uncached`].
unsafe fn callee_rerun_code(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)> {
    // Asked before the guard below, which it would otherwise take twice.
    let substituted = jit_substituted_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let store = cm.class_store();
    let (method, declaring_id) = jit_site_target_method(&cm, info, receiver_class_id, substituted)?;
    let code_attr = method.code()?;
    let class_name: std::sync::Arc<str> = std::sync::Arc::from(&*store.get(declaring_id)?.name);
    let code = crate::runtime::frame::padded_bytecode_for_method(
        declaring_id,
        info.method_name,
        info.descriptor,
        &code_attr.code,
        &code_attr.exception_table,
    );
    Some((declaring_id, class_name, code))
}

/// The replay verdict for a call-site service re-running a callee after its
/// bare sentinel (round 13 wave 8, lane replay4): the trap site the thread
/// recorded (`deopt_resume::frameless_rerun_is_exact` with no entered body;
/// a stub's own stamp answers for its body), else the published body's stamp
/// and the whole-body rule over `callee`'s code. A callee that did not
/// resolve is answered `false`: a census must not call exact a re-run it
/// could not check.
///
/// Round 13 wave 9 (lane replay5): except a trap site that carries its own
/// verdict, which needs no code: a stub stamped `exact` by the single-pass
/// finalizer, or a dropped frame judged exact at its own bci. With the
/// wave-8 refusal a `false` here is an `InternalError`, so answering it for a
/// re-run the trapping body's own stamp proves exact was a spurious one.
fn frameless_callee_rerun_is_exact(
    vm: &SharedVm,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
    info: &JitInvokeInfo,
) -> bool {
    let Some((class_id, class_name, code)) = callee else {
        return unresolved_callee_trap_site_is_exact(last_frameless_trap_site());
    };
    let site = last_frameless_trap_site();
    if matches!(site, FramelessTrapSite::Unknown) {
        let artifact =
            vm.jit
                .jit_cache
                .read()
                .get(class_name, info.method_name, info.descriptor, *class_id);
        return crate::runtime::interpreter::frameless_rerun_is_exact(
            code,
            artifact.as_deref(),
            site,
        );
    }
    // Round 13 wave 13 (lane jitfix): a stub's stamp answers only for the
    // method this service re-runs.
    crate::runtime::interpreter::frameless_rerun_is_exact_for_callee(
        code,
        site,
        frameless_trap_method_hash(class_name, info.method_name, info.descriptor),
    )
}

/// [`frameless_callee_rerun_is_exact`] for a callee that did not resolve
/// (round 13 wave 9, lane replay5): `true` only for a trap site whose own
/// verdict is exact, the two `deopt_resume::frameless_rerun_is_exact` trusts
/// for a service without reading any code. Under
/// `CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY=0` (no site is read) it stays `false`.
fn unresolved_callee_trap_site_is_exact(site: FramelessTrapSite) -> bool {
    matches!(
        site,
        FramelessTrapSite::Stub { exact: true, .. }
            | FramelessTrapSite::Dropped { exact: true }
            // gce e1/f: an owned-arguments hand-off judged exact.
            | FramelessTrapSite::Handoff { exact: true }
    ) && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY")
}

#[cfg(test)]
mod r13w9_replay5_unresolved_callee_tests {
    use super::{unresolved_callee_trap_site_is_exact, FramelessTrapSite};

    /// Only a site carrying its own exact verdict answers for a callee whose
    /// code did not resolve; every other producer stays refused.
    #[test]
    fn an_unresolved_callee_is_exact_only_by_its_trap_sites_own_verdict() {
        let stub = |exact| FramelessTrapSite::Stub {
            compile_id: 7,
            bci: 3,
            exact,
            method: 0,
        };
        assert!(unresolved_callee_trap_site_is_exact(stub(true)));
        assert!(!unresolved_callee_trap_site_is_exact(stub(false)));
        assert!(unresolved_callee_trap_site_is_exact(
            FramelessTrapSite::Dropped { exact: true }
        ));
        assert!(!unresolved_callee_trap_site_is_exact(
            FramelessTrapSite::Dropped { exact: false }
        ));
        assert!(!unresolved_callee_trap_site_is_exact(
            FramelessTrapSite::Helper
        ));
        assert!(!unresolved_callee_trap_site_is_exact(
            FramelessTrapSite::Unknown
        ));
    }

    /// gce e1/f: an owned-arguments hand-off's own verdict answers the same
    /// way.
    #[test]
    fn gce_e1f_an_owned_args_handoff_answers_by_its_own_verdict() {
        let on =
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY");
        assert_eq!(
            unresolved_callee_trap_site_is_exact(FramelessTrapSite::Handoff { exact: true }),
            on
        );
        assert!(!unresolved_callee_trap_site_is_exact(
            FramelessTrapSite::Handoff { exact: false }
        ));
    }
}

#[cfg(test)]
mod gce_e1f_owned_args_handoff_tests {
    use super::*;

    fn info() -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: "p/C",
            method_name: "callee",
            descriptor: "(Ljava/lang/Object;)V",
            num_jit_args: 1,
            return_type: b'V',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    /// The caller's re-run after a hand-off of a FRAMELESS callee trap is as
    /// exact as the callee's replay: the trapping stub's own stamp, only when
    /// it names this callee; a dropped frame's verdict; nothing else.
    #[test]
    fn gce_e1f_a_frameless_handoff_is_judged_by_the_callees_own_stamp() {
        let i = info();
        let own = frameless_trap_method_hash(i.class_name, i.method_name, i.descriptor);
        let stub = |exact, method| FramelessTrapSite::Stub {
            compile_id: 9,
            bci: 4,
            exact,
            method,
        };
        assert!(owned_args_frameless_callee_replay_is_exact(
            stub(true, own),
            &i
        ));
        assert!(!owned_args_frameless_callee_replay_is_exact(
            stub(false, own),
            &i
        ));
        assert!(
            !owned_args_frameless_callee_replay_is_exact(stub(true, own ^ 1), &i),
            "another method's stamp says nothing about this callee"
        );
        assert!(
            !owned_args_frameless_callee_replay_is_exact(stub(true, 0), &i),
            "an unparsed label is not trusted here"
        );
        assert!(owned_args_frameless_callee_replay_is_exact(
            FramelessTrapSite::Dropped { exact: true },
            &i
        ));
        assert!(!owned_args_frameless_callee_replay_is_exact(
            FramelessTrapSite::Dropped { exact: false },
            &i
        ));
        for site in [
            FramelessTrapSite::Helper,
            FramelessTrapSite::Unknown,
            FramelessTrapSite::Handoff { exact: true },
        ] {
            assert!(!owned_args_frameless_callee_replay_is_exact(site, &i));
        }
    }

    /// The record replaces the `Helper` the re-raise wrote, and the next
    /// raise replaces it in turn.
    #[test]
    fn gce_e1f_the_handoff_record_follows_the_reraise() {
        set_jit_deopt_pending();
        note_owned_args_handoff(true);
        let on = cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT",
        );
        let expect = if on {
            FramelessTrapSite::Handoff { exact: true }
        } else {
            FramelessTrapSite::Helper
        };
        assert_eq!(last_frameless_trap_site(), expect);
        set_jit_deopt_pending();
        assert_eq!(last_frameless_trap_site(), FramelessTrapSite::Helper);
        let _ = take_jit_deopt_pending();
    }
}

/// The replay verdict for the KCFULL-13 re-run of `callee` after an exception
/// escaped it at `throw_pc` (`usize::MAX`: unknown; round 13 wave 8, lane
/// replay4). The attempt ran through the throwing instruction, so the prefix
/// rule applies only to an `athrow` there (which commits nothing itself, and
/// re-executes); a throw out of a sub-call ran that call, which a re-run calls
/// again, so anything else is the whole-body rule. The spliced-body half is
/// the published body's (`false` without one).
fn callee_exception_rerun_is_exact(
    vm: &SharedVm,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
    info: &JitInvokeInfo,
    throw_pc: usize,
) -> bool {
    let Some((class_id, class_name, code)) = callee else {
        return false;
    };
    let body =
        vm.jit
            .jit_cache
            .read()
            .get(class_name, info.method_name, info.descriptor, *class_id);
    // 0xbf: athrow.
    let bci = match u32::try_from(throw_pc) {
        Ok(bci) if code.get(throw_pc) == Some(&0xbf) => bci,
        _ => u32::MAX,
    };
    // Round 13 wave 12 (lane replay6): the body's local handlers may have run
    // a `catch` block placed before the throw on the way here, so the extent
    // follows their edges too.
    crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_after(
        code,
        body.as_deref(),
        bci,
    )
}

/// Was the class of the method a call-site service would re-run redefined?
/// Any redefinition in this VM when the method did not resolve: the
/// conservative side here keeps the re-run (the owner decision for a hot
/// swap).
fn callee_rerun_class_redefined(
    vm: &SharedVm,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
) -> bool {
    match callee {
        Some((class_id, _, _)) => class_was_redefined(vm, *class_id),
        None => crate::classloading::any_class_redefined(),
    }
}

/// A call-site service's answer to a re-run of `info`'s callee the replay
/// rule refused ([`crate::runtime::interpreter::unsound_rerun_refused`];
/// round 13 wave 8, lane replay4): count the refusal and raise
/// `InternalError` at the call, as the method-entry doors raise it. The
/// callee's compiled attempt committed a prefix that no sink can resume, and
/// running the callee again would commit it twice.
#[cold]
#[inline(never)]
fn refuse_callee_rerun(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
    cause: crate::runtime::interpreter::DoorRerunCause,
    what: &str,
) -> i64 {
    crate::runtime::interpreter::note_door_rerun_refused(cause);
    // Retired, as a door retires the method whose re-run it refused, so the
    // next call does not raise again.
    if let Some((class_id, class_name, _)) = callee {
        crate::runtime::interpreter::retire_callee_after_refused_rerun(
            vm,
            *class_id,
            class_name,
            info.method_name,
            info.descriptor,
        );
    }
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] call-site service REFUSED re-running {}.{}{} from entry \
             ({what}, cause={cause:?})",
            info.class_name, info.method_name, info.descriptor,
        );
    }
    let message = format!(
        "precise deoptimization unavailable for {}.{}{} after {what} (call-site service: the \
         callee's frame could not be resumed); refusing side-effecting replay",
        info.class_name, info.method_name, info.descriptor,
    );
    let failed = crate::runtime::exceptions::throw_runtime_error(
        vm,
        thread,
        crate::error::RuntimeError::InternalError { message },
    );
    handle_jit_dispatch_error(vm, thread, failed, info)
}

/// j27 wave 4 (lane MONROWS, SDR17-1 of
/// `jit-r17-syncdoor5-proposals`): what a call-site service's refused re-run
/// ([`refuse_callee_rerun`]) names after a callee-handler miss. An
/// [`Unresolved`](crate::runtime::interpreter::CalleeHandlerMiss::Unresolved)
/// template consulted no table, so it says so; every other miss keeps the
/// message it always had (a handler that could not be resumed, or a throw pc
/// that could not be trusted). Diagnosis only: both arms refuse alike.
fn callee_rerun_refusal_what(
    why: &crate::runtime::interpreter::CalleeHandlerMiss,
) -> &'static str {
    match why {
        crate::runtime::interpreter::CalleeHandlerMiss::Unresolved => {
            "an exception whose callee handler template did not resolve"
        }
        _ => "an exception its handler could not resume",
    }
}

#[cfg(test)]
mod j27_sdr17_callee_rerun_refusal_tests {
    use super::callee_rerun_refusal_what;
    use crate::runtime::interpreter::CalleeHandlerMiss;

    #[test]
    fn an_unresolved_template_is_named_apart_from_a_frame_decline() {
        assert_eq!(
            callee_rerun_refusal_what(&CalleeHandlerMiss::Unresolved),
            "an exception whose callee handler template did not resolve"
        );
        for kept in [
            CalleeHandlerMiss::Declined,
            CalleeHandlerMiss::Unknown,
            CalleeHandlerMiss::NotCaught,
        ] {
            assert_eq!(
                callee_rerun_refusal_what(&kept),
                "an exception its handler could not resume",
                "the message every other miss always had"
            );
        }
    }
}

/// [`handle_compiled_callee_deopt_sentinel`] for a callee a RUST arm of
/// `jit_invoke_virtual_mic` entered (the Rust-cached entry, the megamorphic
/// PIC probe, the MIC hit): the same stash / handler service, then the
/// frameless-trap service `route_implicit_exc_through_callee` gives the
/// dispatch helper's arms. Those three arms call through
/// `call_compiled_entry_under_owner`, so the flag is per-call fresh there too;
/// they were left out of stage 2 only because they share the sentinel handler
/// with the inline cascade, which is not. Interpreter round i1 wave 6, L7.
///
/// # Safety
/// As [`handle_compiled_callee_deopt_sentinel`].
unsafe fn handle_rust_armed_callee_sentinel(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
) -> Option<i64> {
    if let Some(v) = handle_compiled_callee_deopt_sentinel(
        vm,
        thread,
        info,
        receiver_class_id,
        args_slice,
        pins,
        false,
    ) {
        return Some(v);
    }
    if frameless_callee_trap_pending() {
        return Some(service_frameless_callee_trap(
            vm,
            thread,
            info,
            receiver_class_id,
            args_slice,
            pins,
        ));
    }
    None
}

/// Drop the stashed pending-exception frame **only if it belongs to the callee
/// this dispatch helper invoked** — N6.
///
/// # Why the unchecked drop was wrong
///
/// `cratonvm_jit::deopt::clear_exceptional_frame`'s own doc states the rule —
/// *"sinks that merely pass an exception along must NOT call this"* — and then
/// drops whatever is there, including a nested callee's frame that the
/// abandoning caller never published. That made the rule documentation rather
/// than enforcement, and the two cases are indistinguishable from inside the
/// function: it is handed no identity at all.
///
/// The three call sites in `route_implicit_exc_through_callee` are entitled to
/// drop a frame on the paths they take — the compiled attempt there is FINISHED
/// or ABANDONED, so its frame can never be legitimately claimed and leaving it
/// stashed lets a later drain for the same method mis-claim it. What they are
/// not entitled to do is drop somebody else's.
///
/// # The identity rule
///
/// Method name and descriptor, not the class — the same rule
/// `try_resume_trapped_callee`'s gate applies, and for the same reason: for a
/// virtual dispatch the frame's baked key names the RESOLVED declaring class,
/// which may be a supertype of the class the call site was written with.
/// Descriptors are compared modulo the return type because a call site's
/// descriptor and the resolved method's can differ there (covariant returns).
///
/// A frame we decline to drop is COUNTED
/// ([`foreign_exceptional_frames_kept`]) rather than left silent: the whole
/// point is to find out whether this was ever happening.
///
/// # What the dropped frame holds (JIT round 17 wave 3, lane syncdoor3)
///
/// A frame the attempt published may name locks its compiled code still holds:
/// a block lock, or, under the exceptional hand-over, a self-locking body's
/// METHOD monitor. Only a sink that takes the frame ever releases them, so a
/// plain drop kept them held for good, and the re-run from entry that follows
/// on the `Declined` arm entered the method monitor AGAIN
/// (`r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`).
/// Under the hand-over's switches the frame is TAKEN and what it names is
/// released, as a propagation would ([`drop_exceptional_frame_of`]).
fn clear_own_exceptional_frame(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
) -> bool {
    let Some((key, bci)) = cratonvm_jit::deopt::peek_exceptional_identity() else {
        return false;
    };
    if !exceptional_frame_key_names(&key, info.method_name, info.descriptor) {
        FOREIGN_EXCEPTIONAL_FRAMES_KEPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] keeping a foreign exceptional frame: stash={key} bci={bci} \
                 vs call site {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return false;
    }
    drop_exceptional_frame_of(vm, thread, &key, bci, "abandoned callee attempt")
}

/// Drop the innermost stashed exceptional frame when it is the one named by
/// `(method_key, bci)`, keeping every frame beneath it. Returns whether a
/// frame was dropped.
///
/// JIT round 17 wave 3, lane syncdoor3: under
/// [`crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released`]
/// (`CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=1`, the exceptional hand-over not
/// `0`) the frame is TAKEN (`take_exceptional_frame_above_if`, the take twin
/// of `clear_exceptional_frame_of`: same identity, same top-only pop) and the
/// locks it names are released
/// ([`crate::runtime::interpreter::CompiledLocksOfAStash::release_for_a_dropped_frame`]).
/// Off, exactly `clear_exceptional_frame_of`. An empty key never matches.
fn drop_exceptional_frame_of(
    vm: &SharedVm,
    thread: &mut JvmThread,
    method_key: &str,
    bci: u32,
    site: &'static str,
) -> bool {
    use crate::runtime::interpreter::CompiledLocksOfAStash;
    if !CompiledLocksOfAStash::dropped_frame_holds_released() {
        return cratonvm_jit::deopt::clear_exceptional_frame_of(method_key, bci);
    }
    if method_key.is_empty() {
        return false;
    }
    let Some((rframe, _cause, _point)) =
        cratonvm_jit::deopt::take_exceptional_frame_above_if(0, |frame, _point| {
            frame.method_key == method_key && frame.bci == bci
        })
    else {
        return false;
    };
    // Released right after the take, before anything that can allocate: the
    // stash rooted the frame's words until now.
    let _ = CompiledLocksOfAStash::release_for_a_dropped_frame(vm, thread, &rframe, site);
    true
}

/// Does a stashed exceptional frame's baked `method_key` name this call site's
/// callee? The identity rule [`clear_own_exceptional_frame`] applies, split out
/// so it can be tested without a live `JitInvokeInfo`.
///
/// Method name and descriptor only. The class is deliberately NOT compared: for
/// a virtual dispatch the key names the RESOLVED declaring class, which may be
/// a supertype of the class the call site was written with — the same widening
/// `try_resume_trapped_callee`'s gate makes, for the same reason. The
/// descriptor is compared modulo the return type, because a call site's
/// descriptor and the resolved method's can differ there.
///
/// An unparseable or empty key names no method at all, so it is nobody's to
/// drop — the fail-closed direction, and the same one
/// `deopt_frame_matches_method` takes.
fn exceptional_frame_key_names(key: &str, method_name: &str, descriptor: &str) -> bool {
    let Some((rest, key_desc)) = key.rsplit_once(':') else {
        return false;
    };
    let Some((_key_class, key_method)) = rest.rsplit_once('.') else {
        return false;
    };
    key_method == method_name && descriptors_match_modulo_return(key_desc, descriptor)
}

fn descriptors_match_modulo_return(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // The `)` that CLOSES the parameter list, not the first one: a class name
    // may contain `)`, and `(LA)B;)Z` vs `(LA)C;)Z` cut at the first `)`
    // compared `(LA)` with `(LA)` and matched two different parameter lists.
    let params = cratonvm_jit_api::descriptor_param_list_end;
    match (params(a), params(b)) {
        (Some(ia), Some(ib)) => a.as_bytes()[..=ia] == b.as_bytes()[..=ib],
        _ => false,
    }
}

/// Service a compiled callee's `i64::MIN` sentinel returned by a GENERATED
/// inline call (the MIC/PIC cascade in `jit/src/x64.rs`).
///
/// The inline cascade calls the cached compiled entry directly, so unlike every
/// dispatch-helper arm there is no Rust frame in between to notice the callee
/// trapped. Without this, the sentinel reached the caller's generated code as
/// if the CALLER had deopted, and the callee's reconstructed frame stayed in
/// the thread's one stash slot until an unrelated sink consumed it — which
/// de-speculates the wrong method and then, correctly, refuses to resume a
/// frame that is not its own (`InternalError: precise deoptimization
/// unavailable`). Reproduced on H2 `TestReopen` / `TestPageStoreCoverage`,
/// where `MVMap` reads every row through the megamorphic `DataType.read`.
///
/// Returns the resumed result, or `i64::MIN` unchanged when the sentinel must
/// keep propagating (no stash of ours, an implicit exception with no local
/// handler, …) — i.e. the pre-existing behaviour on every path this does not
/// service.
///
/// # Safety
/// Same contract as the other JIT dispatch helpers: `vm_ptr` a live `SharedVm`,
/// `info_ptr` a live `JitInvokeInfo`, `args_ptr` the caller's outgoing argument
/// slots (`num_args` of them).
#[no_mangle]
pub unsafe extern "C" fn jit_service_callee_deopt(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let f = || jit_service_callee_deopt_body(vm_ptr, info_ptr, args_ptr, num_args);
    contain(
        "jit_service_callee_deopt",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// JIT round 19 wave 4, lane throws4 (THROWS3-3 of
/// `docs/feature-designs/jit-ir-optimizations.md`,
/// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`): would
/// [`handle_compiled_callee_deopt_sentinel`] do nothing but take the signal
/// record and put it back? If so, apply its one net effect (the `athrow_bci`
/// stamp is reset, as `take_all_jit_signals` + `set_jit_pending_exception`
/// leave it) and answer `true`; the service then returns `i64::MIN`.
///
/// Every arm of the full service is ruled out term by term:
///
/// * no deopt frame stashed (`try_resume_trapped_callee`, the declined-stash
///   re-run), and no exceptional frame (the "MIC door miss" drop);
/// * a throwable or an implicit-exception flag is pending, so the frameless
///   re-run (a bare deopt flag) is not this case;
/// * no not-entrant escape recorded (`take_redispatch_escape_for`; it must be
///   `0` here, which is also what the full service leaves);
/// * a statically bound site (the receiver-based table probe needs the
///   arguments) whose callee has no exception table (the memoized
///   `callee_has_exception_table`), so no handler arm runs.
///
/// What the full service would then do is restore every drained field as it
/// was except `athrow_bci`, reset to `-1` (the restash is the general setter),
/// and re-raise the deopt flag it drained. Reads TLS and the memoized table
/// verdict only; takes no thread guard and allocates nothing.
///
/// SAFETY: as [`jit_service_callee_deopt`] (`vm` live, `info` a live
/// `JitInvokeInfo`).
unsafe fn service_has_nothing_to_do_but_propagate(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    if matches!(info.invoke_kind, 0 | 2)
        || cratonvm_jit::deopt::has_last_deopt()
        || cratonvm_jit::deopt::has_exceptional_frame()
    {
        return false;
    }
    let (signal_pending, escape) = JIT_SIGNALS.with(|s| {
        (
            s.npe.get() || s.aioobe.get().is_some() || s.arithmetic.get(),
            s.redispatch_escape.get(),
        )
    });
    if escape != 0 || !(signal_pending || jit_pending_exception_is_set()) {
        return false;
    }
    if callee_has_exception_table(vm, info) {
        return false;
    }
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
    });
    true
}

/// Body of [`jit_service_callee_deopt`], run under its panic guard.
///
/// `num_args` is a plain count. Between 2026-09-22 and 2026-09-22 its bit 32
/// carried a flag — set by `emit_inline_callee_deopt_check`'s
/// `is_self_recursive` arm, read here as `trust_throw_pc` — whose only job was
/// to opt this call OUT of a stamped-pc downgrade in `run_jit_callee_handler`.
/// That downgrade turned out to be the defect it was meant to work around (it
/// matched a TYPED handler through the pc-unknown search regardless of the
/// handler's protected range), so it was deleted and the bit with it. An ABI
/// bit nobody decodes reads as a signal that is being honoured; there is no
/// second reader to inherit it.
///
/// gcd d9/d (2026-09-28): one WHOLE-word value is not a count:
/// [`cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE`] (negative, so no count can
/// be it), passed with a null `args_ptr` by a single-pass direct call that
/// kept no copy of its arguments because they belong to the callee
/// (`x64/op_invoke.rs`, `sp_direct_call_args_owned_by_callee`). It is decoded
/// here and handed to [`handle_compiled_callee_deopt_sentinel`] as
/// `args_owned_by_callee`.
unsafe fn jit_service_callee_deopt_body(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // Rust<->JIT boundary — same bookkeeping the dispatch helpers do.
    crate::jit::conservative_roots::note_jit_boundary();
    // No flag bits any more: every emitter passes the plain count. See the
    // doc above for the bit that used to live at 32 and why it is gone.
    if vm_ptr == 0 || info_ptr == 0 {
        // r10 `help`: raise the out-of-band deopt flag with the sentinel. A
        // bare `i64::MIN` with every signal CLEAR is not a sentinel at all to
        // the interpreter — `jit_bridge`'s post-JIT path gates its deopt arm on
        // `deopt_signaled` and otherwise pushes the value as a REAL return
        // ("A bare `result == i64::MIN` with the flag CLEAR is a method
        // legitimately returning `Long.MIN_VALUE` ... and must be pushed as a
        // real value below"). For an `L`-returning frame that is a wild
        // pointer in an oop slot.
        set_jit_deopt_pending();
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    // JIT round 19 wave 4, lane throws4 (THROWS3-3): a direct self-call's
    // lean sentinel path ORs `SERVICE_ARGS_LEAN_PROPAGATE_BIT` into a plain
    // (non-negative) count. Masked off before anything reads the count.
    // Wave 5, lane throws5 (THROWS4-1): every statically bound direct-call
    // route may set it, and a single-pass site whose callee owns the
    // arguments sets it with `SERVICE_ARGS_LEAN_ARGS_OWNED_BIT`, decoded back
    // to `SERVICE_ARGS_OWNED_BY_CALLEE`.
    let (num_args, lean) = cratonvm_jit::decode_service_args_word(num_args);
    if lean && service_has_nothing_to_do_but_propagate(vm, info) {
        return i64::MIN;
    }
    // gcd d9/d: the site kept no argument copy; `n` is then 0 and the slice
    // empty, and nothing below may read arguments.
    let args_owned_by_callee = num_args == i64::from(cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE);
    let n = if num_args > 0 { num_args as usize } else { 0 };
    let args_slice: &[i64] = if n == 0 || args_ptr == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, n)
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        // As the arm above: the sentinel needs a signal beside it or the
        // interpreter reads it as this method's return value.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    // The receiver's class id, for the callee-exception-table probe.
    //
    // Only kinds 0/2 have one. For a statically bound site (`invokestatic` /
    // `invokespecial`) arg 0 is an ordinary argument, and reading it as a
    // receiver resolves the callee from whatever class that argument happens to
    // point at — a different method with the same name and descriptor, or none.
    // `route_implicit_exc_through_callee` and `resolve_callee_cached` both
    // already branch on `invoke_kind` for exactly this reason; this arm did
    // not, and the direct-call door (`x64/bytecode_walk.rs`, the
    // `emit_inline_callee_deopt_check` after a baked `invokestatic` CALL) is a
    // live caller of it. `ClassId::new(0)` is the "no receiver" value the
    // callee-side probe below reads as "resolve by name".
    // An array receiver dispatches on `java/lang/Object`, which is what the
    // by-name resolution from the site's class answers; its header's
    // COMPONENT id would resolve the component's override instead
    // (`jit_dispatch_class`).
    let receiver_class_id = if matches!(info.invoke_kind, 0 | 2) {
        args_slice
            .first()
            .and_then(|raw| vm.mem.heap.is_object_address(*raw as usize))
            .and_then(|obj| jit_dispatch_class_validated(vm, obj).class_id())
            .unwrap_or_else(|| ClassId::new(0))
    } else {
        ClassId::new(0)
    };
    // No pins: this door is entered AFTER the callee returned (see
    // r11w4-rt-callee-rerun-reads-pre-call-argument-words).
    match handle_compiled_callee_deopt_sentinel(
        vm,
        thread,
        info,
        receiver_class_id,
        args_slice,
        None,
        args_owned_by_callee,
    ) {
        Some(v) => v,
        None => {
            // The sentinel is about to keep propagating. If a stash is STILL
            // present at this point it is leaving the one call site that could
            // attribute it — every consumer above here sees a frame belonging
            // to a method it did not invoke. Name it here rather than at the
            // sink, where the caller chain is already gone.
            if cratonvm_jit::deopt::has_last_deopt() && dbg_deopt_on() {
                if let Some((key, bci)) = cratonvm_jit::deopt::peek_last_deopt_identity() {
                    eprintln!(
                        "[cratonvm-deopt] ORPHANED at the serviced call site {}{}: \
                         stash={key} bci={bci}",
                        info.method_name, info.descriptor,
                    );
                }
            }
            i64::MIN
        }
    }
}

/// The VM half of a NOT ENTRANT compiled body (interpreter round i1 wave 22,
/// lane L6; `jit/src/not_entrant.rs`).
///
/// A redefinition patches the entry of every compiled body it makes stale
/// with a jump to that body's stub, and the stub calls this with the body's
/// [`cratonvm_jit::NotEntrantRecord`], the argument registers it spilled and
/// the caller's stack-passed arguments. Whoever made the call -- a baked
/// direct `CALL` of either tier, an instance-site bind, a stale inline-cache
/// word, a Rust door -- is answered here: the call is re-dispatched by name
/// through the interpreter's invoke path (`bail_to_interpreter`, non-virtually,
/// since the caller already selected exactly this method), which runs the
/// class's current bytecode or its newest compiled body. The result goes back
/// in the compiled ABI; a throw goes back as the `i64::MIN` sentinel with the
/// pending exception stashed, exactly as a compiled callee that threw, which
/// every caller already handles. HotSpot's `handle_wrong_method` stub answers
/// the same entry; it also re-binds the caller's call site. Since interpreter
/// round i1 wave 23 (lane L6) this re-binds the STUB instead: it hands the
/// body now published under the same key to
/// [`cratonvm_jit::NotEntrantRecord::forward_to`], and every later call jumps
/// from the stub straight to that body without coming here.
///
/// # Safety
///
/// Called only by a not-entrant stub: `record` is the live record of the body
/// whose entry was patched (it lives as long as that body's code, and the
/// stub's frame is on this stack), `regs` points at the spilled argument
/// registers and `stack_args` at the caller's stack-passed arguments of a call
/// made into that body.
#[no_mangle]
pub unsafe extern "C" fn jit_not_entrant_entry(record: i64, regs: i64, stack_args: i64) -> i64 {
    // The VM the body belongs to, for the panic report.
    let vm_ptr = if record == 0 {
        0
    } else {
        // SAFETY: per this function's contract `record` is a live
        // `NotEntrantRecord`.
        // Cast: `vm_ptr` is an address, carried as the helpers' `i64` VM word.
        unsafe { (*(record as *const cratonvm_jit::NotEntrantRecord)).vm_ptr as i64 }
    };
    let f = || {
        // SAFETY: forwarded unchanged under this function's own contract.
        unsafe { jit_not_entrant_entry_body(record, regs, stack_args) }
    };
    contain(
        "jit_not_entrant_entry",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_not_entrant_entry`], run under its panic guard.
///
/// # Safety
///
/// As [`jit_not_entrant_entry`].
unsafe fn jit_not_entrant_entry_body(record: i64, regs: i64, stack_args: i64) -> i64 {
    // Rust<->JIT boundary — same bookkeeping the dispatch helpers do.
    crate::jit::conservative_roots::note_jit_boundary();
    if record == 0 || regs == 0 {
        // No record: nothing names the method. The sentinel needs a signal
        // beside it or the caller reads it as a returned `Long.MIN_VALUE`.
        set_jit_deopt_pending();
        return i64::MIN;
    }
    // SAFETY: per the contract, a live record.
    let record = unsafe { &*(record as *const cratonvm_jit::NotEntrantRecord) };
    if record.vm_ptr == 0 {
        set_jit_deopt_pending();
        return i64::MIN;
    }
    // SAFETY: the record's VM is the `SharedVm` that armed the patch; a VM
    // outlives every compiled body of its cache.
    let vm = unsafe { &*(record.vm_ptr as *const SharedVm) };
    // SAFETY: `regs` / `stack_args` are what the stub passed for a call into
    // this record's body (the contract), so every argument the body's own
    // prologue would read is there.
    let raw = unsafe { record.java_args(regs as *const i64, stack_args as *const i64) };
    // SAFETY: a helper on the thread that entered compiled code.
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        set_jit_deopt_pending();
        return i64::MIN;
    };
    // Entry hygiene, as `call_compiled_entry_under_owner` applies it before a
    // compiled callee runs: a caller issuing a call is not in an exceptional
    // state, so a deopt flag pending now describes no part of this call, and
    // left set it would turn a genuine `Long.MIN_VALUE` result into a
    // sentinel at the caller.
    let _ = take_jit_deopt_pending();
    // The stub built no frame: publish the intercepted call's return address
    // as the innermost compiled frame's pc for the re-dispatch, so a trace
    // captured by the new code places the caller by its return site
    // (`r13w1-callcost-not-entrant-innermost-native-pc-patch-FIXED-20260928.md`).
    // SAFETY: `stack_args` is the pointer the stub passed (the contract).
    let ret_pc =
        unsafe { cratonvm_jit::NotEntrantRecord::caller_return_address(stack_args as *const i64) };
    let prev_ret_pc = crate::jit::conservative_roots::swap_top_not_entrant_ret_pc(ret_pc);
    struct RestoreRetPc(usize);
    impl Drop for RestoreRetPc {
        fn drop(&mut self) {
            crate::jit::conservative_roots::swap_top_not_entrant_ret_pc(self.0);
        }
    }
    let _restore_ret_pc = RestoreRetPc(prev_ret_pc);
    // Interpreter round i1 wave 23, lane L6: point the stub at the body now
    // published under the same key, so the caller's next call jumps straight
    // there instead of coming back here (HotSpot re-binds the caller's call
    // site for the same reason; `NotEntrantRecord::forward_to` says why the
    // stub is re-bound instead). This call is still answered through the VM
    // below. Not while an agent may need any method interpreted (a JVMTI
    // interpreter-only event, an armed JDWP gate): a forward bypasses the
    // doors that ask.
    // Wave 24 (lane L6): nor to a method a statically bound call must reach
    // through the dispatch helper now (a recursion cycle closed by an inline
    // cache, `JitVerdictRegistry::direct_call_requires_dispatch_of`) -- the
    // verdict a caller compiled today would ask before it bakes the call, and
    // one that may have been reached after this record's callers were compiled.
    let forwarded = {
        let info = &record.info;
        let class_id = ClassId::new(info.declaring_class_id);
        info.declaring_class_id != 0
            && !crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(vm)
            && !vm
                .jit
                .tiered_manager
                .verdicts()
                .direct_call_requires_dispatch_of(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    || Some(class_id),
                )
            && vm
                .jit
                .jit_cache
                .get(info.class_name, info.method_name, info.descriptor, class_id)
                // Through the cache: a forward that closes a reference cycle
                // (the strings round's `copyOfRange` -> `StringBuilder` ->
                // `String.<init>` -> stale `copyOfRange`) is installed and
                // registered there, and released when its target is withdrawn.
                .is_some_and(|current| vm.jit.jit_cache.forward_not_entrant(record, &current))
    };
    // JIT round 20 wave 19, lane yStash (STASH20-2,
    // `CRATONVM_JIT_STALE_CALL_REBIND`): with the forward installed, re-point
    // the caller's own `CALL rel32` at the body it forwards to, as HotSpot's
    // wrong-method stub re-resolves the caller's call site.
    let rebound = forwarded && stale_call_rebind_on_stub_pass(vm, record, ret_pc);
    // JIT round 20 wave 11, lane pLate (W16L1-3,
    // `CRATONVM_JIT_STALE_CALLER_RECOMPILE`): the caller that baked this
    // stale entry gets ONE recompile, so its next body binds the current
    // callee instead of paying the stub on every call. Bounded per caller
    // artifact; see the function. Not for a caller whose call site was just
    // rebound (STASH20-2): it no longer pays the stub.
    if !rebound {
        let info = &record.info;
        crate::runtime::interpreter::jit_bridge::stale_caller_recompile_on_stub_pass(
            vm,
            ret_pc,
            (
                info.class_name,
                info.method_name,
                info.descriptor,
                info.declaring_class_id,
            ),
        );
    }
    // Round 16 wave 16 (lane chains2, TF15-2): counted per record, printed at
    // the first call and every power of two (`n=` is the running count).
    let redispatch_line_due = if dbg_deopt_on() {
        record.note_redispatch()
    } else {
        None
    };
    if let Some(n) = redispatch_line_due {
        // Why the stub is not forwarding, re-asked term by term (only when
        // the line prints): a record that names no reason here keeps every
        // baked call on the by-name path below, the r16 strings bimodality.
        let refusal = if forwarded {
            String::new()
        } else {
            let info = &record.info;
            let class_id = ClassId::new(info.declaring_class_id);
            let reason = if info.declaring_class_id == 0 {
                "no declaring class id"
            } else if crate::runtime::interpreter::compiled_frames_may_be_asked_to_leave(vm) {
                "compiled frames may be asked to leave"
            } else if vm
                .jit
                .tiered_manager
                .verdicts()
                .direct_call_requires_dispatch_of(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    || Some(class_id),
                )
            {
                "a direct call must dispatch this method"
            } else {
                match vm.jit.jit_cache.get(
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    class_id,
                ) {
                    None => "no current body published",
                    Some(current) => record.explain_forward_refusal(&current),
                }
            };
            format!("; no forward: {reason}")
        };
        eprintln!(
            "[cratonvm-deopt] not-entrant entry: re-dispatching {}.{}{} ({} args) \
             n={n} from ret={:#x}{}{refusal}",
            record.info.class_name,
            record.info.method_name,
            record.info.descriptor,
            raw.len(),
            // SAFETY: `stack_args` is the pointer the stub passed (the contract).
            unsafe {
                cratonvm_jit::NotEntrantRecord::caller_return_address(stack_args as *const i64)
            },
            if forwarded {
                "; later calls forward to the current body"
            } else {
                ""
            },
        );
    }
    // SAFETY: `vm` live, `record.info` a live `JitInvokeInfo` owned by the
    // record, `raw` the call's own argument words.
    let args = unsafe { decode_dispatch_values(vm, &record.info, &raw) };
    let info = &record.info;
    if info.invoke_kind == 3 && info.declaring_class_id != 0 {
        // A static method: on the class the body was published under, by id,
        // not by name -- `bail_to_interpreter`'s static arm resolves the name
        // through the global map, which can answer another loader's copy (or
        // nothing, for a hidden class). JVMS 5.4.3.3 lookup from that class,
        // and 5.5 initialization of the declaring class, as
        // `jit_invoke_dispatch`'s loader-faithful static arm does.
        let res = crate::vm::invoke_static_shared_on_class(
            vm,
            thread,
            ClassId::new(info.declaring_class_id),
            info.class_name,
            info.method_name,
            info.descriptor,
            &args,
        );
        let rc = match res {
            // Cast: the compiled ABI returns an `int` sign-extended in RAX.
            Ok(Some(Value::Int(v))) => v as i64,
            Ok(Some(Value::Long(v))) => v,
            // Cast: floating-point results travel as their bits.
            Ok(Some(Value::Float(f))) => f.to_bits() as i64,
            // Cast: as above.
            Ok(Some(Value::Double(d))) => d.to_bits() as i64,
            // Cast: a reference travels as its address.
            Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
            Ok(Some(Value::Object(None))) | Ok(None) => 0,
            Ok(_) => 0,
            Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
        };
        // gce e1/f: a throw here has already left the method.
        return note_redispatch_escape(thread, info, rc);
    }
    // An instance method: `invoke_kind` 1 on the substituted owner (the
    // record names its class id), i.e. `invoke_special_shared_on_class`.
    // SAFETY: as above; `thread` is this thread's exclusive borrow.
    let rc = unsafe { bail_to_interpreter(vm, thread, info, &args) };
    // gce e1/f: as the static arm.
    note_redispatch_escape(thread, info, rc)
}

/// `CRATONVM_JIT_STALE_CALL_REBIND` (STASH20-2 of
/// `docs/feature-designs/jit-ir-optimizations.md`, JIT round 20 wave
/// 19, lane yStash; **default ON**, `=0` kills): see
/// [`stale_call_rebind_on_stub_pass`]. Read on a not-entrant stub's slow path
/// only (cold).
fn stale_call_rebind_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_STALE_CALL_REBIND")
}

/// STASH20-2: a call returning to `ret_pc` has just come through `record`'s
/// not-entrant stub, and [`jit_not_entrant_entry`] installed the stub's
/// forward to the method's current body. Re-point the caller's direct `CALL
/// rel32` at that body (`cratonvm_jit::JitCache::rebind_stale_call_site`, which
/// checks the site, the lifetime and the word, and writes it atomically
/// under the cache's mutation lock), so the caller stops paying the patched
/// `JMP` plus the forwarding prefix on every call: the `not-entrant-stub
/// <method>` share in `perf` (1.2% on nio21 w15 `bufascii`). HotSpot's
/// `handle_wrong_method` re-resolves the caller's call site the same way.
///
/// The first rebound sites of a record also clear its forward word, so the
/// NEXT stale caller comes through here once and is rebound too; a call that
/// cannot be rebound (a Rust door, an inline cache, a straddling site) leaves
/// the forward installed for everyone, as before. Returns whether the site was
/// rebound. Under `CRATONVM_DBG_JITC`: `stale-call REBOUND` / `stale-call
/// rebind DECLINED` lines.
fn stale_call_rebind_on_stub_pass(
    vm: &SharedVm,
    record: &cratonvm_jit::NotEntrantRecord,
    ret_pc: usize,
) -> bool {
    if ret_pc == 0 || !stale_call_rebind_enabled() {
        return false;
    }
    let info = &record.info;
    let Some(target) = vm
        .jit
        .jit_cache
        .get(
            info.class_name,
            info.method_name,
            info.descriptor,
            ClassId::new(info.declaring_class_id),
        )
        .map(cratonvm_jit::RetainedCode::new)
    else {
        return false;
    };
    // The caller's frame is live (the stub returns into it), so its body is
    // pinned by the code-range registry; released through the retirement
    // queue. `None`: a Rust door or a stub, nothing to rebind.
    let Some(caller) =
        cratonvm_jit::pin_jit_code_range_owner(ret_pc).map(cratonvm_jit::RetainedCode::new)
    else {
        return false;
    };
    let answer = vm
        .jit
        .jit_cache
        .rebind_stale_call_site(record, target.arc(), &caller, ret_pc);
    if crate::runtime::env_cache::dbg_jitc() {
        match answer {
            Ok(cleared) => eprintln!(
                "[cratonvm-jitc] stale-call REBOUND {}.{}{} in {} (ret={ret_pc:#x}) -> entry={:#x}{}",
                info.class_name,
                info.method_name,
                info.descriptor,
                caller.method_label,
                // Cast: an address, for the trace line.
                target.entry_ptr() as usize,
                if cleared {
                    "; forward word cleared for the next stale caller"
                } else {
                    ""
                },
            ),
            Err(why) => eprintln!(
                "[cratonvm-jitc] stale-call rebind DECLINED {}.{}{} in {} (ret={ret_pc:#x}, caller entry={:#x} artifact={}): {why}",
                info.class_name,
                info.method_name,
                info.descriptor,
                caller.method_label,
                // Cast: an address, for the trace line.
                caller.entry_ptr() as usize,
                caller.artifact_id,
            ),
        }
    }
    answer.is_ok()
}

/// Handle the `i64::MIN` deopt/exception sentinel returned by a direct call
/// into a compiled callee.
///
/// Shared by the direct-entry arms of `jit_invoke_virtual_mic` — the
/// monomorphic inline cache and the megamorphic PIC, through
/// [`handle_rust_armed_callee_sentinel`] — and by the inline cascade's
/// `jit_service_callee_deopt`. They MUST agree: a
/// sentinel returned unhandled is indistinguishable, to the compiled caller,
/// from the caller's own deopt, so the callee's stashed frame travels up to an
/// unrelated sink that de-speculates the wrong method and cannot resume.
///
/// Returns `Some(v)` when the trap was fully serviced here (`v` is the call's
/// result), `None` when the sentinel should propagate unchanged.
///
/// `pins`, when the caller rooted `args_slice` across the call, is what the
/// handler runs read the arguments back through ([`args_now`]); `None` reads
/// `args_slice` as given (see r11w4-rt-callee-rerun-reads-pre-call-argument-words
/// for the arms that cannot pin yet).
///
/// `args_owned_by_callee` (gcd d9/d, 2026-09-28): the call site kept no copy
/// of its arguments (`cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE`), so
/// `args_slice` is empty and no arm may read arguments. The precise resume
/// and the exception pass-through are unchanged. The three arms that read
/// arguments back after the call -- the declined-stash re-run, the frameless
/// re-run and the callee's own handler -- are not taken: the trap leaves as a
/// frameless deopt of the CALLER (the declined stash is dropped first, its
/// locks released and its method charged, as the re-run would have), and the
/// caller's caller re-runs the caller, which is how a frameless trap of the
/// caller itself is answered. That re-run commits nothing twice: since gcd
/// d10/f a site claims ownership only where the caller's bytecode before the
/// call (closed over loops) makes no store, invoke or monitor operation
/// (`x64/op_invoke.rs`, `owned_args_handoff_replays_exactly`; see
/// `gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md`), so
/// the only replay left is the callee's own, as at a copying site. The site
/// only claims ownership for a callee
/// whose method has no exception table and whose body has no frameless trap
/// stub (`cratonvm_jit::callee_body_owns_its_arguments`), so those arms are
/// the rare declines; a handler found anyway (a redefinition added one) is
/// answered the same way, with the pending throwable dropped, since the
/// re-run raises it again. Each such hand-off is named under
/// `CRATONVM_DBG_DEOPT` ([`note_callee_owned_args_trap_propagated`]).
///
/// SAFETY: same contract as the callers — `vm` live, `info` a live
/// `JitInvokeInfo`, `thread` the current thread's exclusive borrow.
///
/// HANDLER11-3 (JIT round 19 wave 12, lane handler12): a call this service
/// COMPLETES with a value while the stash it saw on entry is still the
/// innermost entry left an orphan; [`note_completed_call_left_stash`] names it
/// (`callee-deopt-service`) and, under its opt-in switch, drops it.
unsafe fn handle_compiled_callee_deopt_sentinel(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
    args_owned_by_callee: bool,
) -> Option<i64> {
    let entry_top = cratonvm_jit::deopt::deopt_stash_top_seq();
    let answer = handle_compiled_callee_deopt_sentinel_inner(
        vm,
        thread,
        info,
        receiver_class_id,
        args_slice,
        pins,
        args_owned_by_callee,
    );
    if answer.is_some_and(|v| v != i64::MIN) {
        note_completed_call_left_stash(entry_top, "callee-deopt-service");
    }
    answer
}

/// `CRATONVM_JIT_COMPLETED_CALL_DROPS_LEFT_STASH` (HANDLER11-3, JIT round 19
/// wave 12, lane handler12; **default OFF**, `1` enables): a consumer that
/// completes a call over the deopt stash entry it saw on entry discards that
/// entry instead of only naming it ([`note_completed_call_left_stash`]). Read
/// only when such an orphan is born (cold).
fn completed_call_drops_left_stash_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_COMPLETED_CALL_DROPS_LEFT_STASH")
}

/// HANDLER11-3 (JIT round 19 wave 12, lane handler12;
/// `r19-outside-manager-c2-upgrade-crashes-under-load-FIXED-20261012.md`): a consumer
/// of a callee's sentinel has completed the call -- its caller gets a value,
/// or (at a door) the call's throwable -- and `entry_top` is the innermost
/// stash entry it saw when it began (`cratonvm_jit::deopt::deopt_stash_top_seq`).
/// If that entry is still innermost, no sentinel travels on to a sink that
/// could take it: it is an orphan, born here. Stamped with `label` (every
/// later discard names it under `CRATONVM_DBG_DEOPT`, so one run of the
/// loaded stress names the consumer that leaves R16LocaleWide's orphan), and
/// discarded at once under `CRATONVM_JIT_COMPLETED_CALL_DROPS_LEFT_STASH`.
/// One thread-local read on a completing sentinel path; nothing when the
/// entry stash was empty.
pub(crate) fn note_completed_call_left_stash(entry_top: Option<u64>, label: &'static str) {
    if entry_top.is_none() {
        return;
    }
    cratonvm_jit::deopt::note_stash_left_by(
        entry_top,
        label,
        completed_call_drops_left_stash_enabled,
    );
}

/// [`handle_compiled_callee_deopt_sentinel`] without the HANDLER11-3 orphan
/// note.
///
/// SAFETY: as [`handle_compiled_callee_deopt_sentinel`].
unsafe fn handle_compiled_callee_deopt_sentinel_inner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
    args_owned_by_callee: bool,
) -> Option<i64> {
    // Precise resume of a frame-stashing deopt in the dispatched callee (see
    // `try_resume_trapped_callee`). Must run before consuming the exception
    // signals below: a pure deopt sets no exception flags.
    if let Some(v) = try_resume_trapped_callee(vm, thread, info, receiver_class_id) {
        return Some(v);
    }

    // A machine-call cache hit returns the callee's sentinel in the caller's
    // ABI.  Drain the *whole* signal record here, while the receiver class and
    // outgoing arguments still identify that callee, and run its handler at
    // the recorded throw bci.  Re-entering the callee from bytecode 0 used to
    // duplicate all side effects before a caught bounds/null/divide exception.
    // gce e1/f: taken first -- `take_all_jit_signals` clears it. Kept raw
    // (interpreter round i5 wave 104, lane L2): the lambda arm below asks it
    // about the IMPL as well as about `info`'s SAM method.
    let escape_record = take_redispatch_escape_record();
    let escaped_redispatch =
        redispatch_escape_record_names(escape_record, info.method_name, info.descriptor);
    let mut signals = take_all_jit_signals(thread);
    // gcd d9/d: set once an owned-arguments trap has been handed to the
    // caller's caller, so it is counted once.
    let mut owned_args_propagated = false;
    // gce e1/f: the replay verdict of the caller's re-run a hand-off asks for
    // (`FramelessTrapSite::Handoff`), `None` when this is no hand-off or the
    // callee's own replay could not be judged.
    let mut handoff_exact: Option<bool> = None;
    // A pure deopt (no throwable, no implicit flag) whose stash
    // `try_resume_trapped_callee` declined above: re-run this site's callee
    // from entry instead of letting its frame travel to the caller's sink
    // (r11w12-tier-helpers-declined-callee-rerun-patch). The drained deopt
    // flag belongs to that trap and is spent with it.
    if signals.exception.is_none()
        && signals.aioobe.is_none()
        && !signals.npe
        && !signals.arithmetic
        && cratonvm_jit::deopt::has_last_deopt()
    {
        if args_owned_by_callee {
            // gcd d9/d: no argument to re-run it with. Drop the stash as the
            // re-run would have, and leave as the caller's frameless deopt.
            if let Some(exact) = drop_declined_callee_stash(vm, thread, info, receiver_class_id) {
                note_callee_owned_args_trap_propagated(info, "declined stash");
                owned_args_propagated = true;
                signals.deopt = true;
                handoff_exact = Some(exact);
            }
        } else if let Some(v) =
            rerun_declined_callee_from_entry(vm, thread, info, receiver_class_id, args_slice, pins)
        {
            return Some(v);
        }
    }
    // The frameless twin of the block above, as the dispatch door's
    // `BARE_CALLEE_TRAPS_SERVICED` arm (round 11 wave 13, applied by lane
    // icwide from lane tier's r11w13-tier-helpers-mic-door-frameless-callee-
    // rerun-patch): a trap in this site's callee that left no frame (its own
    // frameless stub, or `try_resume_trapped_callee`'s GC-drop arm above) is
    // re-run here, instead of reaching the caller as the caller's own
    // frameless deopt, which an OSR'd caller can only answer by replaying
    // every committed iteration. The drained flag is spent with it; it is
    // per-call fresh (`call_compiled_entry_under_owner` retires it before the
    // callee runs), so it is never a stale flag beside a real `Long.MIN_VALUE`.
    if signals.deopt
        && signals.exception.is_none()
        && signals.aioobe.is_none()
        && !signals.npe
        && !signals.arithmetic
        && !cratonvm_jit::deopt::has_last_deopt()
    {
        if args_owned_by_callee {
            // gcd d9/d: no argument to re-run it with; the deopt flag stays
            // set and the tail below re-raises it for the caller's caller.
            if !owned_args_propagated {
                note_callee_owned_args_trap_propagated(info, "frameless trap");
                owned_args_propagated = true;
                // gce e1/f: the callee's own frameless record, read before
                // the tail's re-raise overwrites it.
                handoff_exact = Some(owned_args_frameless_callee_replay_is_exact(
                    last_frameless_trap_site(),
                    info,
                ));
            }
        } else {
            // Round 13 wave 8 (lane replay4): through the dispatch door's
            // service, so this re-run is counted in the door census
            // (`callee-frameless`) and refused when unsound, as there. It was
            // the one call-site re-run from entry nobody counted. The
            // service's own counter bump and flag take are this arm's (the
            // take is a no-op: drained above).
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] frameless callee trap at the MIC door: {}.{}{}",
                    info.class_name, info.method_name, info.descriptor,
                );
            }
            return Some(service_frameless_callee_trap(
                vm,
                thread,
                info,
                receiver_class_id,
                args_slice,
                pins,
            ));
        }
    }
    let throw_pc = if signals.athrow_bci >= 0 {
        signals.athrow_bci as usize
    } else {
        usize::MAX
    };
    // Interpreter round i5 wave 105, lane L2: the pc an EXPLICIT throwable is
    // offered with. Unstamped, it was re-posted by the VM after the callee's
    // bytecode already ran (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`); an implicit
    // signal built here keeps `throw_pc`.
    let explicit_throw_pc = if throw_pc == usize::MAX {
        crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT
    } else {
        throw_pc
    };
    // nio21 wave 21 L1: a lambda proxy receiver whose handler-bearing impl
    // left a throwable (explicit, or an implicit signal built here): run the
    // IMPL's table, which the receiver-based probe below cannot find (a proxy
    // has no class-store method). A miss leaves the throwable pending for the
    // restore tail, as a named callee's miss does.
    if !args_owned_by_callee && !(escaped_redispatch && signals.exception.is_some()) {
        let lambda_site = lambda_handler_site_for_service(vm, info, receiver_class_id)
            .filter(|site| !explicit_throwable_already_left_impl(&signals, escape_record, site));
        if let Some(site) = lambda_site {
            let lambda_pc = if signals.exception.is_some() {
                explicit_throw_pc
            } else {
                throw_pc
            };
            let exc = match signals.exception.take() {
                Some(exc) => Some(exc),
                None => {
                    let implicit =
                        implicit_signal_of(signals.aioobe, signals.npe, signals.arithmetic);
                    if implicit == ImplicitSignal::None {
                        None
                    } else {
                        let built = materialize_implicit_signal(
                            vm,
                            thread,
                            implicit,
                            signals.trap_frames.clone(),
                            signals.npe_action,
                        );
                        if built.is_some() {
                            // Consumed: the throwable carries the snapshot.
                            signals.aioobe = None;
                            signals.npe = false;
                            signals.arithmetic = false;
                            signals.trap_frames = None;
                        }
                        built
                    }
                }
            };
            if let Some(exc) = exc {
                // Read AFTER the allocation above, which can collect.
                let handler_args = args_now(pins, thread, args_slice);
                match lambda_route_thrown_through_impl(
                    vm,
                    thread,
                    &site,
                    info,
                    &handler_args,
                    exc,
                    lambda_pc,
                ) {
                    Ok(v) => return Some(v),
                    Err(exc) => signals.exception = Some(exc),
                }
            }
        }
    }
    // Same split as `route_implicit_exc_through_callee`: resolve from the
    // receiver for a virtual/interface site, by name for a statically bound
    // one. Asking the receiver-based probe about an `invokestatic` callee reads
    // arg 0 as a class, which is not one.
    let has_handler = if matches!(info.invoke_kind, 0 | 2) {
        mic_callee_has_exception_table(vm, receiver_class_id, info)
    } else {
        callee_has_exception_table(vm, info)
    };
    // gce e1/f: a throwable that escaped a not-entrant re-dispatch of this
    // very callee has already been offered to its table, by the VM's invoke
    // path, at its real throw pc. The search below would offer it again with
    // the pc unknown, where a typed row matches on class alone and a row the
    // throw had left runs a second time
    // (`gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler-DONE-20260928.md`).
    // Propagate it, as a `NotCaught` miss does.
    let has_handler = has_handler && !(escaped_redispatch && signals.exception.is_some());
    // gcd d9/d: an owned-arguments site binds only a callee with no table, so
    // a handler here means the method was redefined with one since. It cannot
    // be run without the arguments: drop the throwable (the caller's re-run
    // raises it again) and leave as the caller's frameless deopt.
    let throwable_pending = signals.exception.is_some()
        || signals.aioobe.is_some()
        || signals.npe
        || signals.arithmetic;
    if args_owned_by_callee && has_handler && throwable_pending {
        if !owned_args_propagated {
            note_callee_owned_args_trap_propagated(info, "callee handler");
        }
        signals.exception = None;
        signals.aioobe = None;
        signals.npe = false;
        signals.arithmetic = false;
        signals.trap_frames = None;
        signals.deopt = true;
    }
    if has_handler && !args_owned_by_callee {
        if let Some(exc) = signals.exception {
            let handler_args = args_now(pins, thread, args_slice);
            match try_run_callee_handler(
                vm,
                thread,
                info,
                receiver_class_id,
                &handler_args,
                exc,
                explicit_throw_pc,
            ) {
                Ok(v) => return Some(v),
                // Restored below. The miss's throwable, not the drained one:
                // it was re-read from a pin, and may be a catch type's
                // resolution error that replaced it.
                Err((_, now, _)) => signals.exception = Some(now),
            }
        } else if throw_pc != usize::MAX
            && implicit_callee_throw_pc_enabled()
            && implicit_throw_is_uncovered(vm, info, receiver_class_id, throw_pc)
        {
            // Round 12 wave 3 (lane exc): no row of the callee covers its
            // stamped throw pc, so `run_jit_callee_handler` would answer
            // `NotCaught` -- after the throwable had been built, only to be
            // discarded and built again by the caller-side drain. Fall through
            // to the restash below instead, which is what that miss did.
        } else {
            // The SAME table the dispatch-helper door uses. This door is where
            // the arithmetic arm always existed; sharing the table is what stops
            // the two from disagreeing about the SET again. See
            // [`ImplicitSignal`].
            // CLONED, not taken: the restash at the end of this function puts
            // the snapshot back when nothing here consumes the throwable, and
            // `take` left it with `None` to put back. Same defect and same fix
            // as the sibling door's.
            let implicit = materialize_implicit_signal(
                vm,
                thread,
                implicit_signal_of(signals.aioobe, signals.npe, signals.arithmetic),
                signals.trap_frames.clone(),
                signals.npe_action,
            );
            if let Some(exc) = implicit {
                // Read AFTER the allocation above, which can collect.
                let handler_args = args_now(pins, thread, args_slice);
                match try_run_callee_handler(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    &handler_args,
                    exc,
                    throw_pc,
                ) {
                    Ok(v) => return Some(v),
                    Err((_, error, true)) => {
                        // A catch type's resolution error replaced the
                        // implicit exception (JVMS §5.4.3). Restore it as a
                        // GENERAL pending exception, as the arm above restores
                        // its miss: restashing the implicit flag and snapshot
                        // below would rebuild the original exception at the
                        // caller and drop the error.
                        signals.exception = Some(error);
                        signals.aioobe = None;
                        signals.npe = false;
                        signals.arithmetic = false;
                        signals.trap_frames = None;
                    }
                    Err(_) => {}
                }
            }
        }
    }

    // Nothing in the callee consumed the exception. Restore precisely the
    // original signal shape so the existing caller-side drain remains the
    // fallback; this preserves behaviour for a miss or a non-covering catch.
    if let Some(exc) = signals.exception {
        set_jit_pending_exception(thread, exc);
    }
    // Restore the snapshot the drain took WITH the flag it belongs to, rather
    // than letting a setter take a fresh one here: `take_all_jit_signals` moved
    // it out, and a second `active_compiled_frames()` at this point describes a
    // shallower stack than the trap did. Without this the caller-side drain
    // builds the throwable from a bare flag and the trace is EMPTY.
    // ONE `trap_frames` slot, so exactly ONE of these three may restore it
    // and the rest come back flag-only. Writing it more than once is not a
    // tidiness point: every one of these setters ASSIGNS the slot, so a
    // second call with `None` would wipe the frames the first put back.
    // JVMS does not let one frame have two of these pending at once, so
    // the priority never arbitrates anything real -- it is written out so
    // the next reader does not have to derive that from three `take()`s.
    let frames = signals.trap_frames.take();
    if signals.npe {
        restash_jit_pending_npe(signals.npe_action, frames);
        if signals.arithmetic {
            stash_jit_pending_arithmetic();
        }
        if let Some((index, length)) = signals.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if signals.arithmetic {
        restash_jit_pending_arithmetic(frames);
        if let Some((index, length)) = signals.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if let Some((index, length)) = signals.aioobe {
        restash_jit_pending_aioobe(index, length, frames);
    }
    // JIT round 17 wave 4, lane stashdrop (site 5 of
    // `r17-syncdoor3-exceptional-frames-discarded-beneath-a-take-keep-their-holds-FIXED-20261005.md`):
    // the callee threw and its exception leaves this door (a `Declined` /
    // `Unknown` miss, which includes a `resolve_callee_cached` failure, an
    // `escaped_redispatch`, or an owned-arguments hand-off). Its activation
    // has unwound and no sink will ever resume the frame it published, so
    // under the exceptional hand-over's switches a top frame naming it is
    // taken and what it names released
    // (`CompiledLocksOfAStash::release_for_a_dropped_frame`), as
    // `route_implicit_exc_through_callee` does through
    // `clear_own_exceptional_frame`. Left stashed, it ended under the caller's
    // own frame and was discarded unreleased at the next take. A `NotCaught`
    // miss already took and released it. Nothing here allocates: the restored
    // throwable stays valid. Off, the frame is left for the caller as before.
    //
    // JIT round 19 wave 4, lane syncdoor4 (SELFLOCK19-1 of
    // `jit-r19-selflock19-proposals.md`): the stash is asked BEFORE the two
    // switches (each an env read). It is empty for almost every exception that
    // leaves here, so most escapes now read no flag. The same answer: all three
    // questions are pure, and the drop needs every one of them.
    if throwable_pending {
        if let Some((key, bci)) = cratonvm_jit::deopt::peek_exceptional_identity() {
            if exceptional_frame_key_names(&key, info.method_name, info.descriptor)
                && crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released(
                )
            {
                let _ = drop_exceptional_frame_of(vm, thread, &key, bci, "MIC door miss");
            }
        }
    }
    if signals.deopt {
        set_jit_deopt_pending();
        // gce e1/f: an owned-arguments hand-off tells the sink that re-runs
        // the caller what that re-run replays (after the re-raise above,
        // which records `Helper`).
        if let Some(exact) = handoff_exact {
            note_owned_args_handoff(exact);
        }
    }
    None
}

/// gce e1/f (`gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md`,
/// the census residual): record that the deopt flag just raised is an
/// owned-arguments hand-off whose caller re-run replays `exact`ly
/// ([`FramelessTrapSite::Handoff`]). Call right after
/// [`set_jit_deopt_pending`]. `CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT=0`
/// leaves the `Helper` record, i.e. the old whole-body judgement.
fn note_owned_args_handoff(exact: bool) {
    // gcf f3/f: counted whatever the switch says, for the one-line exit
    // report (`deopt_resume::owned_args_handoff_census`), so a row's
    // engagement is read from the end of its stderr.
    crate::runtime::interpreter::note_door_rerun(
        crate::runtime::interpreter::DoorRerunCause::OwnedArgsHandoff,
        exact,
    );
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT") {
        JIT_SIGNALS.with(|s| s.frameless_trap.set(FramelessTrapSite::Handoff { exact }));
    }
}

/// gce e1/f: the replay verdict of a caller re-run after an owned-arguments
/// site handed off its callee's FRAMELESS trap. The caller's own prefix is
/// exact by the site's admission (`x64/op_invoke.rs`,
/// `owned_args_handoff_replays_exactly`), so it is the callee's replay: the
/// trapping stub's stamp when the record names this very callee, or the
/// dropped frame's verdict; anything else is not exact (the old answer).
fn owned_args_frameless_callee_replay_is_exact(
    site: FramelessTrapSite,
    info: &JitInvokeInfo,
) -> bool {
    match site {
        FramelessTrapSite::Stub { exact, method, .. } => {
            exact
                && method != 0
                && method
                    == frameless_trap_method_hash(
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                    )
        }
        FramelessTrapSite::Dropped { exact } => exact,
        FramelessTrapSite::Handoff { .. }
        | FramelessTrapSite::Helper
        | FramelessTrapSite::Unknown => false,
    }
}

/// `CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT` (JIT round 16 wave 15, lane trapfix;
/// **default ON**, `=0` restores the round-16 base): does a deoptimization that
/// evicts a method's published body
/// (`cratonvm_jit::tiered::deopt_evicts_method_body`) also make that body not
/// entrant ([`make_trap_evicted_body_not_entrant`]), and does the call-site
/// service leave uncharged a trap whose method an earlier trap already
/// declared not compilable ([`trap_from_a_body_already_declared_not_compilable`])?
///
/// The defect it closes
/// (`r16-framestate-spliced-callee-trap-repeats-after-make-not-compilable-FIXED-20261002.md`):
/// the eviction withdrew the body from the cache, the inline caches and the
/// published raw callers, but a caller frame ALREADY RUNNING a loop kept the
/// evicted body's address in its baked `CALL` ([`despeculate_trapped_method`]'s
/// doc measured it on `UnresolvedTrapProbe`). Each failing call then trapped in
/// the evicted body and, the slot being empty, was charged again: ~6 700
/// `MakeNotCompilable` decisions in `R16FramestateChainCoverage`, one per
/// failing call. Read per eviction and per uncharged-candidate trap, both cold.
fn trap_eviction_not_entrant_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT")
}

/// Make `evicted`, the method-entry body a deoptimization just withdrew
/// (`DeoptimizationController::deoptimize_in`'s `remove`), not entrant: its
/// entry pad becomes a `JMP` to the not-entrant stub
/// (`jit/src/not_entrant.rs`), so a caller that baked its address -- above
/// all a caller frame already running when the eviction happened, which no
/// invalidation can re-bind -- re-dispatches the call instead of entering the
/// withdrawn code: forwarded to the method's current body once one is
/// published (`jit_not_entrant_entry`'s forward word), interpreted while none
/// is (a `MakeNotCompilable` method is then simply interpreted, and its
/// failing guard costs an interpreted throw, not a deoptimization). Frames
/// already inside the body finish in it.
///
/// The eviction flavour of the pass (`JitCache::make_not_entrant_after_eviction`):
/// the body runs current bytecode, so it is not marked withdrawn by a
/// redefinition. The hook is armed exactly as the redefinition, the VarHandle
/// mint and the late-bind retire arm it; idempotent. A refusal (no entry pad,
/// an OSR body, an arity the record cannot name) changes nothing: the
/// call-site service's once-only charge then still bounds the cost
/// ([`trap_from_a_body_already_declared_not_compilable`]).
///
/// `evicted` is released through `RetainedCode`, not dropped bare: it may be
/// the body's last owner once the retirement queue drained its own reference,
/// and a bare drop would free published code outside an authorised
/// reclamation (`published_code_free_audit`).
fn make_trap_evicted_body_not_entrant(
    vm: &SharedVm,
    method_key: &str,
    evicted: std::sync::Arc<cratonvm_jit::CompiledMethod>,
) {
    let evicted = cratonvm_jit::RetainedCode::new(evicted);
    vm.jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: vm as *const SharedVm as usize,
        });
    let patched = vm
        .jit
        .jit_cache
        .make_not_entrant_after_eviction(std::slice::from_ref(evicted.arc()));
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] {method_key}: evicted body id={} made not entrant (patched={patched})",
            evicted.compile_id
        );
    }
}

/// t1 wave 6 (lane J6): arm this VM's not-entrant hook at startup when
/// `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT` (opt-in, t1 wave 5 lane X2;
/// `cratonvm_jit::jit_invalidate_not_entrant_enabled`) is on. Called by
/// `Vm::new` once the `Arc<SharedVm>` exists (the hook carries the VM's
/// address, stable only then; `SharedVm::new` cannot arm it).
///
/// Without it the switch was inert until something else armed the cache: the
/// first trap eviction, redefinition, VarHandle mint, late-bind retire or
/// MH-constant retire. A dependency invalidation before that (a class load
/// that breaks a CHA assumption, the common case) withdrew its bodies and
/// patched none, so an optimizing caller's baked `CALL` kept entering them;
/// whether a run patched depended on event order. Armed here, every
/// invalidation of the run is covered.
///
/// Per VM (this VM's cache, this VM's address), idempotent like every other
/// arming site. Arming also engages, from startup, what the other arming
/// sites engage from their first event: `JitCache::put`'s supersede redirect
/// and the not-entrant halves of the VarHandle mint / late bind (the effect
/// `CRATONVM_JIT_ARM_NOT_ENTRANT_AT_PUBLISH` measured neutral in round 20).
/// Returns whether it armed.
pub(crate) fn arm_not_entrant_for_dependency_invalidation(vm: &SharedVm) -> bool {
    if !cratonvm_jit::jit_invalidate_not_entrant_enabled() {
        return false;
    }
    vm.jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: vm as *const SharedVm as usize,
        });
    true
}

/// t1 wave 6 (lane J6): [`arm_not_entrant_for_dependency_invalidation`] arms
/// the cache only with the switch on, per VM.
#[cfg(test)]
mod t1w6_j6_invalidate_not_entrant_arming_tests {
    use super::*;

    fn vm() -> std::sync::Arc<SharedVm> {
        let config = crate::config::VmConfig {
            use_synthetic_jdk: true,
            ..Default::default()
        };
        std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config))
    }

    #[test]
    fn startup_arming_follows_the_invalidate_not_entrant_switch_per_vm() {
        const SW: &str = "CRATONVM_JIT_INVALIDATE_NOT_ENTRANT";
        let off = vm();
        let armed = cratonvm_types::flags::with_thread_overrides(&[(SW, None)], || {
            arm_not_entrant_for_dependency_invalidation(&off)
        });
        assert!(!armed, "opt-in: default off arms nothing");
        assert!(!off.jit.jit_cache.not_entrant_armed());
        let on = vm();
        let other = vm();
        let armed = cratonvm_types::flags::with_thread_overrides(&[(SW, Some("1"))], || {
            arm_not_entrant_for_dependency_invalidation(&on)
        });
        assert!(armed);
        assert!(on.jit.jit_cache.not_entrant_armed(), "this VM's cache is armed");
        assert!(
            !other.jit.jit_cache.not_entrant_armed(),
            "per VM: another VM's cache is untouched"
        );
    }
}

/// Is a trap the call-site service is about to charge one that an earlier
/// trap already charged to the end (JIT round 16 wave 15, lane trapfix;
/// FS14-4 of `jit-r16-framestate-proposals-RETIRED-20261006.md`)? `true` when the cache
/// publishes no body for the method and this VM's runtime de-speculation
/// registry holds a current `MakeNotCompilable` verdict for it
/// (`RuntimeDespecRegistry::is_not_compilable`, which a redefinition or a
/// code-cache flush expires): the trap came out of a body that verdict
/// evicted, through a binding the eviction could not withdraw. Charging it
/// again records another deoptimization, re-decides `MakeNotCompilable`,
/// re-writes the skip set and the registry and re-counts the per-bci de-spec,
/// with no compile left to shape. The resume itself still runs.
///
/// Under `CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT` ([`trap_eviction_not_entrant_enabled`]).
fn trap_from_a_body_already_declared_not_compilable(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    class_id: ClassId,
    bci: u32,
) -> bool {
    let decided = trap_eviction_not_entrant_enabled()
        && vm
            .jit
            .jit_cache
            .get(class_name, method_name, descriptor, class_id)
            .is_none()
        && vm
            .jit
            .runtime_despec
            .is_not_compilable(class_id, class_name, method_name, descriptor);
    if decided && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] {class_name}.{method_name}:{descriptor} bci={bci}: trap from a body \
             already declared not compilable, not charged"
        );
    }
    decided
}

/// Round 16 wave 15 (lane trapfix): the eviction's not-entrant patch and the
/// call-site service's once-only charge, on a hand-built body.
#[cfg(all(test, target_arch = "x86_64"))]
mod r16w15_trapfix_tests {
    use super::*;

    fn vm() -> std::sync::Arc<SharedVm> {
        let config = crate::config::VmConfig {
            use_synthetic_jdk: true,
            ..Default::default()
        };
        let vm = std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config));
        *vm.self_arc.write() = Some(std::sync::Arc::downgrade(&vm));
        vm
    }

    /// `NOP5; MOV EAX, 7; RET`, a static `()I` body with the entry pad.
    fn padded_body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&cratonvm_jit::ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        body.entry_java_args = 0;
        body
    }

    const CLASS: &str = "r16w15/Trap";
    const METHOD: &str = "kernel";
    const DESC: &str = "()I";

    fn publish(vm: &SharedVm, id: ClassId) -> std::sync::Arc<cratonvm_jit::CompiledMethod> {
        vm.jit
            .jit_cache
            .put(CLASS.into(), METHOD.into(), DESC.into(), id, padded_body());
        vm.jit
            .jit_cache
            .get(CLASS, METHOD, DESC, id)
            .expect("published")
    }

    /// A `MakeNotCompilable` eviction patches the body a running caller baked,
    /// without the redefinition's mark; the service then leaves the next trap
    /// of that method uncharged.
    #[test]
    fn a_not_compilable_eviction_patches_the_body_and_charges_once() {
        let vm = vm();
        let id = ClassId::new(0x1615);
        let body = publish(&vm, id);
        assert!(!trap_from_a_body_already_declared_not_compilable(
            &vm, CLASS, METHOD, DESC, id, 19
        ));
        // `UnreachedCode` is `MakeNotCompilable` on the first occurrence.
        let action = DeoptimizationController::deoptimize_in(
            &vm,
            id,
            CLASS,
            METHOD,
            DESC,
            cratonvm_jit::deopt::DeoptReason::UnreachedCode,
            19,
        );
        assert_eq!(action, cratonvm_jit::deopt::DeoptAction::MakeNotCompilable);
        assert!(vm.jit.jit_cache.get(CLASS, METHOD, DESC, id).is_none());
        assert!(body.is_not_entrant(), "the evicted entry is patched");
        assert!(!body.is_withdrawn_by_redefinition());
        assert_eq!(vm.jit.jit_cache.bodies_withdrawn_by_redefinition(), 0);
        assert!(trap_from_a_body_already_declared_not_compilable(
            &vm, CLASS, METHOD, DESC, id, 19
        ));
        // Another class id is another method.
        assert!(!trap_from_a_body_already_declared_not_compilable(
            &vm,
            CLASS,
            METHOD,
            DESC,
            ClassId::new(0x1616),
            19
        ));
        drop(cratonvm_jit::RetainedCode::new(body));
    }

    /// An eviction short of `MakeNotCompilable` (a first `BoundsCheck`,
    /// `Reinterpret`, which still evicts) patches the body too -- a running
    /// caller's `CALL` then forwards to the recompile -- and the service keeps
    /// charging: no verdict exists yet.
    #[test]
    fn a_recompiling_eviction_patches_the_body_and_keeps_the_charge() {
        let vm = vm();
        let id = ClassId::new(0x1617);
        let body = publish(&vm, id);
        let reason = cratonvm_jit::deopt::DeoptReason::BoundsCheck;
        let action =
            DeoptimizationController::deoptimize_in(&vm, id, CLASS, METHOD, DESC, reason, 7);
        assert!(cratonvm_jit::tiered::deopt_evicts_method_body(
            reason, action
        ));
        assert_ne!(action, cratonvm_jit::deopt::DeoptAction::MakeNotCompilable);
        assert!(vm.jit.jit_cache.get(CLASS, METHOD, DESC, id).is_none());
        assert!(body.is_not_entrant());
        assert!(!trap_from_a_body_already_declared_not_compilable(
            &vm, CLASS, METHOD, DESC, id, 7
        ));
        drop(cratonvm_jit::RetainedCode::new(body));
    }
}

/// De-speculate a method whose compiled body just trapped — TAKING THE
/// SITE-TRAP DECISION ONCE.
///
/// `fallback_reason` is what to record when this is NOT an IR site trap; the
/// two callers pass different ones because they know different things.
///
/// # Why the two consumers of a stashed deopt frame must ask the same function
///
/// A trapped compiled body's frame reaches one of two sinks, depending on how
/// the callee was entered: `try_resume_trapped_callee` (the compiled caller's
/// dispatch helper, for a callee that already has an artifact) or `execute`'s
/// first-call tier-up sink (`interpreter.rs`, `execute-first-call-tierup`, for
/// the invocation that installs one). They disagreed. The helper applied the
/// policy below; the tier-up sink passed the DEOPT POINT'S OWN reason straight
/// to `deoptimize`, and for a body whose trapping bci carries a safepoint that
/// reason is `TransferToInterpreter`, whose `recommend_action` is `Reinterpret`
/// — the artifact stays live, the next call re-enters it, and it traps again,
/// forever.
///
/// # The policy
///
/// `UnreachedCode` (the usual fallback) is the one-shot "give up immediately"
/// reason: the trapping method is made not-compilable on the first resolution
/// and the tiered manager stops re-queuing recompiles that would just trap.
///
/// EXCEPT for an IR SITE TRAP. `probes/UnresolvedTrapProbe.java` measures why:
/// the trapped method deopts on EVERY call (200,000 of 200,000) and
/// `MakeNotCompilable` is consulted by `compile_gate` itself, so the method
/// loses its body on EVERY tier — including the single-pass backend, which
/// lowers `invokedynamic` and unresolved typechecks perfectly well. Before site
/// traps existed the IR tier simply refused such a method and C1 compiled it;
/// blacklisting is a strict regression on that.
///
/// A site trap means one thing only: the OPTIMIZING tier could not lower one
/// call site. So ban the optimizing tier for this method — the memo
/// `try_compile_inner` already consults — and pick a reason that RECOMPILES
/// instead of blacklisting. The recompile then goes single-pass and does not
/// trap. `SpeculationFailed` is that reason: `RecompileAndReinterpret` until the
/// per-method deopt count crosses `max_deopts_per_method`, which keeps a
/// backstop if the assumption above is ever wrong.
///
/// # Why the decision is taken ONCE, and in its OWN set
///
/// Everything after the first is a REPEAT of a decision already taken, and
/// repeating it is not harmless: `SpeculationFailed` escalates on the
/// per-method deopt COUNT, so a trapped site inside a long-running caller drives
/// the method to `MakeNotCompilable` — the exact outcome this arm exists to
/// avoid — purely by being reached often.
///
/// Why "often" is unavoidable: eviction and the epoch bump both fire on the
/// first deopt, and `invalidate_matching` even evicts the direct CALLER
/// transitively. But the caller may be a single in-flight invocation running a
/// loop — measured on `probes/UnresolvedTrapProbe.java`, where one `main` frame
/// calls the trapped callee 200,000 times through a `CALL` baked into code that
/// is already executing. Re-binding happens (the tracer shows `main` recompiled
/// and baking a second callee entry) but the RUNNING frame keeps the old
/// address, and nothing short of deoptimizing the caller's frame can change
/// that. `MakeNotEntrant` is an enum variant here with no entry-patching behind
/// it, so there is no cheap displacement to reach for.
///
/// What IS in reach is not compounding the damage: take the policy decision
/// once, and let the remaining calls resume in the interpreter — correct, and
/// self-correcting the moment that caller frame returns and re-enters its
/// recompiled self.
///
/// "Once" is `JitVerdictRegistry::claim_site_trap_decision`, a set of its own (per VM, per declaring class, per redefine epoch), and not
/// `ir_evidence`'s refusal memo — see 81c9c9fd7 and the comment at the call.
///
/// Round 16 wave 15 (lane trapfix): `MakeNotEntrant` now has entry patching
/// behind it. Every evicting deoptimization makes the evicted method-entry
/// body not entrant ([`make_trap_evicted_body_not_entrant`]), so the running
/// caller's baked `CALL` reaches the not-entrant stub, which forwards to the
/// recompiled body or re-dispatches through the interpreter.
pub(crate) fn despeculate_trapped_method(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
) {
    despeculate_trapped_method_inner(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        fallback_reason,
        bci,
        None,
        false,
        None,
    );
}

/// `CRATONVM_DEOPT_HELPER_CHAIN_SITE_WILDCARD` -- JIT round 19 wave 12, lane
/// tail12 (`r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`).
/// **Default ON**, `0` (kill word) restores the old record: with
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS` off, an inlined CHAIN's
/// per-bci de-spec at the helper and first-call sinks records the WILDCARD at
/// the call site (counted per `(reason, call-site bci)`, as before), exactly as
/// the door sink's chain arm records it, instead of the reason's named id there
/// -- which no consumer reads, so a spliced guard that kept failing was spliced
/// again by every recompile until the aggregate per-method count retired the
/// method. Read per trapped-frame despeculation (cold); not cached.
fn helper_chain_site_wildcard_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_HELPER_CHAIN_SITE_WILDCARD")
}

/// JIT round 19 wave 4 (lane splice4, SPLICE3-2): [`despeculate_trapped_method`]
/// for a sink that holds the stashed frame `rframe` the trap is charged for.
/// Under `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS` (with
/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW`), an inlined CHAIN's per-bci
/// escalation goes through `deopt_resume::despec_chain_site_after_trap_limit`
/// (an implicit exception names the innermost guard first, as the door sink
/// does since wave 3) instead of recording the call-site entry at once, and
/// the call-site entry it records is the wildcard, as the door sink's chain
/// arm records it (off the switch this sink records the reason's named id at
/// the call-site bci, which no consumer reads, so a chain-trapping splice is
/// never withdrawn here: `r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`).
/// Switch off, or a single-scope frame: exactly [`despeculate_trapped_method`].
/// The caller has already asked `deopt_resume::stash_sink_expected_throw_uncharged`
/// and skipped this call for a marked trip it leaves uncharged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn despeculate_trapped_method_of_frame(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) {
    despeculate_trapped_method_of_frame_despec_as(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        fallback_reason,
        bci,
        rframe,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn despeculate_trapped_method_inner(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
    chain: Option<&cratonvm_jit::deopt::ReconstructedFrame>,
    chain_site_wildcard: bool,
    despec_as: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    // Interpreter round i1 wave 15, lane L3: a method-entry body that left at
    // a back-edge poll because the method must now run interpreted did not
    // mis-speculate. No charge, no site-trap memo, no per-bci de-spec
    // (`interpreter::exit_left_for_the_interpreter`).
    if crate::runtime::interpreter::exit_left_for_the_interpreter(
        vm,
        fallback_reason,
        declaring_class_id,
        method_name,
        descriptor,
    ) {
        return;
    }
    // JIT round 19 wave 3, lane eager3 (review of EAGER19-1): an
    // uninitialized-`new` trap is a SITE trap (the builder lists its pc), so
    // the policy below would ban the optimizing tier for the method (sticky
    // under the default evidence policy) and charge `SpeculationFailed`, which
    // `deoptimize_in`'s hook does not serve: the late-bind recompile the trap
    // exists to buy was never asked, and the method stayed single-pass. Charge
    // it as itself instead (`jit_bridge::charge_uninit_new_trap`). Engages only
    // at a site `CRATONVM_JIT_UNINIT_NEW_TRAP` / `_GUARD` admitted (one lock
    // when that map is non-empty), so unconditional.
    if fallback_reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap
        && bci != u32::MAX
        && crate::runtime::interpreter::jit_bridge::uninit_new_trap_site_known(
            vm,
            declaring_class_id,
            method_name,
            descriptor,
            bci,
        )
    {
        crate::runtime::interpreter::jit_bridge::charge_uninit_new_trap(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            bci,
            false,
        );
        return;
    }
    // Cheap negative first: both sinks call this on EVERY trapped-frame
    // despeculation, and a VM that planted no site trap at all must not pay a
    // hash and a lock read for it. Doubly so now that site traps are opt-in.
    // This VM's site-trap memo and refusal memo, keyed by the declaring class
    // id (and the decision by this VM's redefine epoch): interpreter round i1
    // wave 23, `i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`.
    let verdicts = crate::runtime::interpreter::jit_bridge::jit_verdicts(vm);
    let (ir_site_trap, h) = if verdicts.any_site_trap_registered() {
        let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
        let method_level = verdicts.method_has_site_trap(h, declaring_class_id);
        (
            site_trap_policy_applies(
                verdicts,
                h,
                declaring_class_id,
                bci,
                method_level,
                (class_name, method_name, descriptor),
            ),
            h,
        )
    } else {
        (false, 0)
    };
    // JIT round 21 lane indylate (`CRATONVM_JIT_INDY_LATE_LINK_TRAP_RECOMPILE`):
    // a planted `invokedynamic` site trap whose instruction the linked bridge
    // serves once the interpreter has linked it (it was unlinked when the body
    // was built) is a late bind, not a site this tier cannot lower: recompile
    // without the IR ban (`jit_bridge::indy_late_link_trap_reprieve`).
    if ir_site_trap
        && chain.is_none()
        && bci != u32::MAX
        && verdicts.method_has_site_trap_at(h, declaring_class_id, bci)
        && crate::runtime::interpreter::jit_bridge::indy_late_link_trap_reprieve(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            bci,
        )
    {
        DeoptimizationController::deoptimize_or_by_name(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
            bci,
        );
        return;
    }
    let mut decided = true;
    if ir_site_trap {
        cratonvm_jit::ir::note_site_trap_taken();
        // A DEDICATED set, not `ir_evidence`'s refusal memo (81c9c9fd7): that
        // memo has a second writer -- the acceptance gate marks a method
        // refused whenever it discards an optimizing body -- so reading it here
        // would let a gate-refused method look "already decided" on its FIRST
        // trap, and the policy, including the eviction, would never be applied.
        // The IR ban goes under the compile door's own key (method, declaring
        // class id, redefine epoch); a repeat re-decides while that ban could
        // not be recorded (JIT round 21, lane lambdatrap). j27 wave 7, lane
        // T1LAMTRAP (LAMBDATRAP21-1): the one claim/ban step all three
        // site-trap-style policies share, `claim_site_trap_policy`.
        decided = claim_site_trap_policy(
            verdicts,
            h,
            (class_name, method_name, descriptor),
            declaring_class_id,
            SiteTrapPolicy::SiteTrap,
        );
    }
    if decided {
        // The caller's declaring class id, not a by-name re-lookup: with two
        // loaders defining `class_name` the lookup answers id 0 and the
        // tiered manager's key misses the real state
        // (r11-tier-deopt-tiering-key-drops-the-class-id). Id 0 (unit tests,
        // an unresolved caller) keeps the by-name route.
        DeoptimizationController::deoptimize_or_by_name(
            vm,
            declaring_class_id,
            class_name,
            method_name,
            descriptor,
            if ir_site_trap {
                cratonvm_jit::deopt::DeoptReason::SpeculationFailed
            } else {
                fallback_reason
            },
            bci,
        );
        // Per-bci de-spec, the rule `real_frame_deopt_resume_and_despeculate`
        // (`deopt_resume.rs`) applies. It lives here too because a method whose
        // trap arrives through a COMPILED caller never reaches that sink: its
        // failing loop-header speculative-BCE guard was re-emitted on every
        // recompile until the aggregate count retired the whole method
        // (`TryLoop`: 18.9 s vs HotSpot's 16 ms; round 9 wave 8, review8c).
        // `deopt_count_at_bci` counts charged traps only, so an exception-only
        // bci never de-specs. `deoptimize` has released the log lock by now
        // (`record_deoptimization` scopes it), so re-locking cannot deadlock.
        // The rule itself (limit, grain, sentinel, insert) is
        // `deopt_resume::despec_site_after_trap_limit`'s (round 14 wave 4,
        // lane resume2, RS-3); this site keeps its choice of grain and id.
        if bci != u32::MAX {
            let method_key = format!("{class_name}.{method_name}:{descriptor}");
            // `trapped_callee_deopt_reason` answers `UncommonTrap` exactly
            // when the reason at this bci could not be read uniquely; record
            // the wildcard then, as the other sink does for `Ambiguous`/`None`.
            let wildcard =
                ir_site_trap || fallback_reason == cratonvm_jit::deopt::DeoptReason::UncommonTrap;
            // Count at the grain of the verdict (HotSpot's
            // `PerBytecodeTrapLimit` is per `(bci, reason)`): three traps of
            // one kind and one of another at a bci must not withdraw the
            // speculation that failed once. The wildcard withdraws everything
            // at the bci, so it keeps the whole-bci sum. Same rule as
            // `deopt_resume::real_frame_deopt_resume_and_despeculate`
            // (r11w4-tier-helpers-per-bci-despec-limit-sums-every-reason).
            // nio21 wave 7 (L2): a caller-named attribution (an inlined
            // chain's, [`chain_trap_charge`]) is the doors' grain: counted at
            // its reason, withdrawn under its id. JIT round 19 wave 12, lane
            // tail12: otherwise a chain's call-site record is the wildcard
            // (`CRATONVM_DEOPT_HELPER_CHAIN_SITE_WILDCARD`), still counted at
            // the reason's grain.
            let (counted, speculation) = match despec_as.filter(|_| !ir_site_trap) {
                Some(c) => (Some(c.reason), c.speculation_id),
                None if wildcard => (None, cratonvm_jit::deopt::SPECULATION_ID_ANY),
                None if chain_site_wildcard => (
                    Some(fallback_reason),
                    cratonvm_jit::deopt::SPECULATION_ID_ANY,
                ),
                None => (
                    Some(fallback_reason),
                    cratonvm_jit::deopt::speculation_id(bci, fallback_reason),
                ),
            };
            // JIT round 19 wave 4 (lane splice4, SPLICE3-2): a chain's
            // implicit exception names its innermost guard first
            // (`despeculate_trapped_method_of_frame`; `None` everywhere else).
            // Its call-site record is the WILDCARD, as the door sink's chain
            // arm records it: the planner withdraws a splice only for the
            // wildcard at its invoke (`ir_splice_site_withdrawn`), so the named
            // `speculation_id(call-site bci, reason)` this sink records for a
            // chain off the switch withdraws nothing (see
            // `r19-trapped-callee-sink-never-withdraws-a-chain-trapping-splice-20261007`).
            let withdrawn = match chain {
                Some(rframe) if !ir_site_trap => {
                    crate::runtime::interpreter::despec_chain_site_after_trap_limit(
                        vm,
                        &method_key,
                        rframe,
                        bci,
                        counted,
                        cratonvm_jit::deopt::SPECULATION_ID_ANY,
                    )
                }
                _ => crate::runtime::interpreter::despec_site_after_trap_limit(
                    vm,
                    &method_key,
                    bci,
                    counted,
                    speculation,
                ),
            };
            if let Some(bci_deopts) = withdrawn {
                if dbg_deopt_on() {
                    eprintln!(
                        "[cratonvm-deopt] per-bci de-spec (trapped callee): {method_key} \
                         bci={bci} ({bci_deopts} deopts >= {})",
                        crate::runtime::interpreter::PER_BCI_DESPEC_LIMIT
                    );
                }
            }
        }
    }
}

/// `CRATONVM_JIT_SITE_TRAP_PER_BCI` (JIT round 19 proposal wave 7, lane
/// interp7; EAGER3-19-1 / OSRENTRY6-1 of `jit-r19-eager3-proposals.md` /
/// `jit-r19-osrentry6-proposals.md`; **default OFF**, `1` enables): see
/// [`site_trap_policy_applies`]. Read per trapped-frame despeculation (cold).
///
/// Placed AFTER [`despeculate_trapped_method_inner`] on purpose: the source
/// witness `r19w3_eager3_tests::the_site_trap_policy_charges_an_uninit_new_trap_as_itself`
/// reads a fixed window from `despeculate_trapped_method`.
fn site_trap_per_bci_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_SITE_TRAP_PER_BCI")
}

/// Does the site-trap policy (IR ban, `SpeculationFailed`, the wildcard
/// per-bci de-spec) apply to a trap of the method `h` / `class_id` at `bci`?
///
/// `method_level` is the per-method registration
/// (`JitVerdictRegistry::method_has_site_trap`): some optimizing body of the
/// method once planted a site trap SOMEWHERE. That key is shared by the
/// method-entry body and every OSR body and is never withdrawn (an older
/// trapping body may still run), so after a recompile publishes a body with no
/// site trap, a genuine uncommon trap at an unrelated bci still took the
/// policy (EAGER3-19-1). Under the switch, a known `bci` takes the policy only
/// when a body planted a site trap AT that bci
/// (`JitVerdictRegistry::method_has_site_trap_at`, registered beside the
/// per-method key); any other bci is an ordinary uncommon trap, the route of
/// every method that never planted one. An unknown bci (`u32::MAX`) keeps the
/// per-method answer.
///
/// Diagnostic (both arms, `CRATONVM_DBG_JITC`): EAGER3-19-1's first-step
/// count, one line per policy decision at a bci that holds no planted trap.
fn site_trap_policy_applies(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    class_id: ClassId,
    bci: u32,
    method_level: bool,
    names: (&str, &str, &str),
) -> bool {
    if !method_level || bci == u32::MAX {
        return method_level;
    }
    if verdicts.method_has_site_trap_at(h, class_id, bci) {
        return true;
    }
    let per_bci = site_trap_per_bci_enabled();
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] site-trap policy at a bci with no planted site trap: {}.{}{} bci={bci} ({})",
            names.0,
            names.1,
            names.2,
            if per_bci {
                "ordinary uncommon trap under CRATONVM_JIT_SITE_TRAP_PER_BCI"
            } else {
                "policy applied by the per-method key"
            },
        );
    }
    !per_bci
}

/// Zero-divisor exits of one compiled `(method, bci)` tolerated before the
/// method is re-tiered — see [`retier_hot_division_guard_trap`]. HotSpot's
/// `PerBytecodeTrapLimit` is 4; 16 is this VM's own choice (it once matched
/// `FAST_THROW_HOT_THRESHOLD`, which has been 10 000 since nio21 wave 16 --
/// comment corrected JIT round 22 wave 2, lane throws, L5W16-4), so a
/// method that divides by zero a handful of times keeps its optimizing body.
const DIVISION_GUARD_RETIER_THRESHOLD: u32 = 16;

/// Bound on the per-thread table below; a full table is cleared (counting
/// restarts — slower, never wrong).
const DIVISION_GUARD_SITE_CAP: usize = 1024;

/// `(vm identity, ir_method_memo_hash, declaring class id, the VM's redefine epoch,
/// bci)`. The class id and epoch since interpreter round i1 wave 23: the
/// counter fired once per key (since round 21: at the threshold, then at
/// powers of two), so a key by names alone let a second
/// loader's copy, or the redefined version, of a method count past the
/// threshold without ever re-tiering
/// (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`).
type DivisionGuardSiteKey = (usize, u64, u32, u32, u32);

thread_local! {
    /// Zero-divisor guard exits counted per site, per thread. Per thread for
    /// the reason `exceptions.rs`'s fast-throw table is: no lock on a path
    /// that is already paying a deopt, and a threshold per thread only delays
    /// the decision on a multi-threaded program, it never changes it.
    static DIVISION_GUARD_TRAPS: std::cell::RefCell<rustc_hash::FxHashMap<DivisionGuardSiteKey, u32>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `idiv`, `ldiv`, `irem`, `lrem` — the four bytecodes a zero divisor traps.
fn is_integer_division_opcode(op: u8) -> bool {
    matches!(op, 0x6c | 0x6d | 0x70 | 0x71)
}

/// Count one guard exit at `key`; `true` on the exit that reaches
/// `threshold`, and again on every later exit whose count is a power of two
/// (JIT round 21, lane smallfix, REVVM221-1; correctness of the re-decide,
/// unconditional). Firing once only meant the same thread and site never
/// asked `site_trap_policy_must_apply` again, so a repeat whose IR ban could
/// not be recorded (refusal memo at its bound) kept the rebuilt optimizing
/// body for good. Bounded: at most ~32 fires per key over a `u32` count
/// (`u32::MAX`, where the count saturates, is not a power of two), and each
/// later fire is a claim-set no-op while the ban holds. Pure over `counts`.
fn note_division_guard_trap<K: std::hash::Hash + Eq>(
    counts: &mut rustc_hash::FxHashMap<K, u32>,
    key: K,
    threshold: u32,
    cap: usize,
) -> bool {
    if counts.len() >= cap && !counts.contains_key(&key) {
        counts.clear();
    }
    let n = counts.entry(key).or_insert(0);
    *n = n.saturating_add(1);
    *n == threshold || (*n > threshold && n.is_power_of_two())
}

/// Should a precise-resumed trap be counted as a zero-divisor guard exit of
/// the OPTIMIZING tier? Pure, for the test.
///
/// The stash names the division's own bci and the artifact's point there is
/// the plain `TransferToInterpreter` resume point: that is the IR tier's
/// anchored `Guard` for `ArithmeticException` (`ir_lower.rs`
/// `emit_div_zero_guard`, "the ArithmeticException is that guard's job"),
/// which deopts so the interpreter re-executes the division and throws. The
/// single-pass tier never stashes a frame here — its zero check calls
/// `jit_throw_arithmetic` and throws without leaving the caller's compiled
/// code — so nothing else can produce this pair.
fn is_division_guard_trap(reason: cratonvm_jit::deopt::DeoptReason, opcode: Option<u8>) -> bool {
    reason == cratonvm_jit::deopt::DeoptReason::TransferToInterpreter
        && opcode.is_some_and(is_integer_division_opcode)
}

/// r9w10 exc10 — stop a hot zero divisor from deoptimizing its callee on
/// every throw.
///
/// `perf-implicit-exceptions-in-compiled-code-cost-microseconds-each`: with
/// `ExcLoop.div` compiled by the optimizing tier, EVERY `k / 0` is
/// `reason=TransferToInterpreter action=Reinterpret` plus a helper
/// precise-resume (199 378 deopts for 198 500 throws on `callee 200000`,
/// `CRATONVM_DBG_DEOPT`), ~4.1 us each, flag or no flag. `Reinterpret` keeps
/// the artifact, `TransferToInterpreter` is never charged, so the per-bci
/// de-spec above never fires either: nothing ever changes, and the callee
/// traps forever. When `div` stays on the single-pass tier the same throw is
/// `jit_throw_arithmetic` + the caller's drain, ~1.5 us (fast throw on) /
/// ~3.2 us (off) on the same run.
///
/// So after [`DIVISION_GUARD_RETIER_THRESHOLD`] such exits at one site, apply
/// the SITE-TRAP policy [`despeculate_trapped_method`] documents, for the
/// same reason it exists: the optimizing tier cannot lower this site without
/// a deopt, and the single-pass tier can. Ban the IR tier for the method (the
/// memo `try_compile_inner` consults) and deoptimize with
/// `SpeculationFailed`, which evicts and recompiles — single-pass, whose zero
/// check throws directly. Claimed ONCE per method through the same
/// `claim_site_trap_decision` set, so the repeats a long-running caller's baked
/// `CALL` still produces cannot escalate the method to not-compilable.
///
/// Kill switch: `CRATONVM_JIT_DIV_GUARD_RETIER=0` (default on).
#[allow(clippy::too_many_arguments)]
fn retier_hot_division_guard_trap(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    reason: cratonvm_jit::deopt::DeoptReason,
    opcode: Option<u8>,
    bci: u32,
) {
    if !is_division_guard_trap(reason, opcode) || !division_guard_retier_enabled() {
        return;
    }
    let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
    let key: DivisionGuardSiteKey = (
        vm.vm_identity,
        h,
        declaring_class_id.as_u32(),
        vm.jit.jit_cache.redefine_epoch(),
        bci,
    );
    let reached = DIVISION_GUARD_TRAPS.with(|traps| match traps.try_borrow_mut() {
        Ok(mut counts) => note_division_guard_trap(
            &mut counts,
            key,
            DIVISION_GUARD_RETIER_THRESHOLD,
            DIVISION_GUARD_SITE_CAP,
        ),
        Err(_) => false,
    });
    if !reached {
        return;
    }
    let verdicts = crate::runtime::interpreter::jit_bridge::jit_verdicts(vm);
    // JIT round 21 (lane revvm2, LAMBDATRAP21-1): a repeat re-decides when the
    // IR ban could not be recorded (`claim_site_trap_policy`, which also
    // records the ban and charges a repeat).
    if !claim_site_trap_policy(
        verdicts,
        h,
        (class_name, method_name, descriptor),
        declaring_class_id,
        SiteTrapPolicy::DivisionGuard,
    ) {
        return;
    }
    let action = DeoptimizationController::deoptimize_or_by_name(
        vm,
        declaring_class_id,
        class_name,
        method_name,
        descriptor,
        cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
        bci,
    );
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] division-guard re-tier: {class_name}.{method_name}:{descriptor} \
             bci={bci} ({DIVISION_GUARD_RETIER_THRESHOLD} zero-divisor exits) -> IR tier \
             refused, action={action:?}"
        );
    }
}

/// JIT round 21 (lane revvm2, LAMBDATRAP21-1 of
/// `docs/internal/proposals/jit/tier1/jit-r21-lambdatrap-proposals.md`; correctness of
/// routing, unconditional): must a site-trap-style re-tier
/// ([`retier_hot_division_guard_trap`], [`retier_hot_optimizing_catch`]) apply
/// its policy now? Yes when this is the method's first decision
/// (`claim_site_trap_decision`), and ALSO when a later one finds the IR ban
/// not in effect (`ir_method_ban_holds`: the refusal memo was at its bound
/// when the first decision tried to record it). Then the recompile the first
/// decision asked for rebuilt the same optimizing body, and returning on the
/// claim alone left it installed for good -- the hole round 21 closed in
/// [`despeculate_trapped_method_inner`] only. The re-decision is bounded as
/// that one is: `SpeculationFailed`'s per-method escalation.
///
/// t1 wave 8, lane LAMBDATRAP: a repeat records the ban itself before asking
/// whether it holds, as [`despeculate_trapped_method_inner`] does. Otherwise a
/// repeat on another thread that lands between the first decision's claim and
/// its `note_ir_method_refused` saw "no ban" and applied the policy a second
/// time (one more `SpeculationFailed` charge toward not-compilable, one more
/// eviction). The record is idempotent; at the memo's bound it still fails,
/// which is the case the re-decision is for.
fn site_trap_policy_must_apply(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    (class_name, method_name, descriptor): (&str, &str, &str),
    declaring_class_id: ClassId,
) -> bool {
    if verdicts.claim_site_trap_decision(h, declaring_class_id) {
        return true;
    }
    verdicts.note_ir_method_refused(class_name, method_name, descriptor, declaring_class_id);
    !verdicts.ir_method_ban_holds(class_name, method_name, descriptor, declaring_class_id)
}

/// The three site-trap-style policies that share one per-method decision
/// ([`claim_site_trap_policy`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SiteTrapPolicy {
    /// A planted IR site trap ([`despeculate_trapped_method_inner`]).
    SiteTrap,
    /// A hot zero-divisor guard ([`retier_hot_division_guard_trap`]).
    DivisionGuard,
    /// Hot catches outside compiled code ([`retier_hot_optimizing_catch`]).
    HotCatch,
}

impl SiteTrapPolicy {
    /// The `policy` word of the `site-trap repeat` census line.
    fn census_name(self) -> &'static str {
        match self {
            SiteTrapPolicy::SiteTrap => "site-trap",
            SiteTrapPolicy::DivisionGuard => "division-guard",
            SiteTrapPolicy::HotCatch => "hot-catch",
        }
    }

    /// Whether a repeat charges the process-wide `note_site_trap_repeat`
    /// counter. A hot catch is not a trap, so it never did.
    fn charges_site_trap_repeat(self) -> bool {
        self != SiteTrapPolicy::HotCatch
    }
}

/// j27 wave 7, lane T1LAMTRAP (LAMBDATRAP21-1 of
/// `docs/internal/proposals/jit/tier1/jit-r21-lambdatrap-proposals.md`): the
/// ONE claim/ban step of the three site-trap-style policies, which each did it
/// by hand (`despeculate_trapped_method_inner` inline, the two re-tiers through
/// [`site_trap_policy_must_apply`]). Answers whether the caller applies its
/// policy (deoptimize with `SpeculationFailed`) now: the method's first
/// decision, or a repeat that finds no IR ban in effect. Either way the IR ban
/// is recorded under the compile door's own key (method, declaring class id,
/// redefine epoch; idempotent). A repeat that stands down charges the repeat
/// counter (not for a hot catch) and the per-method census line
/// ([`note_site_trap_repeat_census`]). Behaviour is the three callers' old
/// one, except that a hot-catch repeat now prints its census line too
/// (`CRATONVM_DBG_DEOPT` only).
fn claim_site_trap_policy(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    names: (&str, &str, &str),
    declaring_class_id: ClassId,
    policy: SiteTrapPolicy,
) -> bool {
    let apply = site_trap_policy_must_apply(verdicts, h, names, declaring_class_id);
    let (class_name, method_name, descriptor) = names;
    verdicts.note_ir_method_refused(class_name, method_name, descriptor, declaring_class_id);
    if !apply {
        if policy.charges_site_trap_repeat() {
            cratonvm_jit::ir::note_site_trap_repeat();
        }
        note_site_trap_repeat_census(
            verdicts,
            h,
            names,
            declaring_class_id,
            policy.census_name(),
        );
    }
    apply
}

/// t1 wave 7, lane NBB (LAMBDATRAP21-2 of
/// `docs/internal/proposals/jit/tier1/jit-r21-lambdatrap-proposals.md`): under
/// `CRATONVM_DBG_DEOPT`, one `[cratonvm-deopt] site-trap repeat #n of C.m`
/// line on the 1st, 16th, 256th, ... repeat of one method's already-claimed
/// decision, saying whether the IR ban holds -- the process-wide
/// `note_site_trap_repeat` counter names no method, and the jr21 8664 ns cliff
/// (thousands of repeats, ban not in effect) would have shown in one run.
/// Off the debug flag it costs one cached bool read.
fn note_site_trap_repeat_census(
    verdicts: &cratonvm_jit::JitVerdictRegistry,
    h: u64,
    (class_name, method_name, descriptor): (&str, &str, &str),
    declaring_class_id: ClassId,
    policy: &str,
) {
    if !dbg_deopt_on() {
        return;
    }
    let Some(n) = verdicts.note_site_trap_repeat_of(h, declaring_class_id) else {
        return;
    };
    if cratonvm_jit::ir::site_trap_repeat_is_reported(n) {
        let ban =
            verdicts.ir_method_ban_holds(class_name, method_name, descriptor, declaring_class_id);
        eprintln!(
            "[cratonvm-deopt] site-trap repeat #{n} of {class_name}.{method_name}{descriptor} \
             (class id {}, policy {policy}): ban holds={ban}",
            declaring_class_id.as_u32(),
        );
    }
}

/// `CRATONVM_JIT_DIV_GUARD_RETIER_CHAINS` (JIT round 19 wave 12, lane tail12,
/// TAIL11-1; opt-in): the call-site service asks
/// [`retier_hot_division_guard_trap`] about an inlined chain's TRAPPING scope
/// (its opcode, the stash's reason) rather than the outer invoke. Read per
/// charged chain trap (cold); not cached.
fn div_guard_retier_chains_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DIV_GUARD_RETIER_CHAINS")
}

/// `CRATONVM_JIT_DIV_GUARD_RETIER_SINKS` (JIT round 19 wave 14, TAIL12-1;
/// opt-in): the two sinks an optimizing body is entered from the interpreter
/// through (`deopt_resume`'s door sink, `execute`'s first-call sink) also ask
/// [`retier_hot_division_guard_trap`], which only the call-site service asked:
/// a body entered from an interpreted loop whose `idiv` guard keeps trapping
/// was never re-tiered. Read per charged trap (cold); not cached.
fn div_guard_retier_sinks_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DIV_GUARD_RETIER_SINKS")
}

/// TAIL12-1: the door / first-call sinks' entry to
/// [`retier_hot_division_guard_trap`], behind
/// `CRATONVM_JIT_DIV_GUARD_RETIER_SINKS`. `opcode` is the TRAPPING scope's
/// (the frame's own for a single frame, the innermost scope's for a chain),
/// `bci` the charge site in the method the key names. The same counter, the
/// same once-per-method claim as the call-site service's, so a method that
/// traps through both counts once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn retier_division_guard_trap_at_sink(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    reason: cratonvm_jit::deopt::DeoptReason,
    opcode: Option<u8>,
    bci: u32,
) {
    if !div_guard_retier_sinks_enabled() {
        return;
    }
    retier_hot_division_guard_trap(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        reason,
        opcode,
        bci,
    );
}

/// `CRATONVM_JIT_DIV_GUARD_RETIER` — default ON; `0`/`false`/`off`/`no` turns
/// [`retier_hot_division_guard_trap`] off. Read per call (j27 wave 7, lane
/// T1IC: was a process `OnceLock`): only a division-guard deopt trap asks.
fn division_guard_retier_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DIV_GUARD_RETIER")
}

/// Catches one OPTIMIZING-tier method may run outside compiled code within
/// [`OPTIMIZING_CATCH_RETIER_WINDOW`] before it is re-tiered. The division
/// guard's threshold, for the same reason: a method that catches a handful of
/// exceptions keeps its optimizing body.
const OPTIMIZING_CATCH_RETIER_THRESHOLD: u32 = 16;

/// The window [`OPTIMIZING_CATCH_RETIER_THRESHOLD`] is counted over. A RATE,
/// unlike the division guard's lifetime count: the cost this policy removes is
/// paid per catch, so a method that catches once a minute is better served by
/// its optimizing body than by the single-pass one.
const OPTIMIZING_CATCH_RETIER_WINDOW: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Bound on the per-thread table below; a full table is cleared (counting
/// restarts -- slower, never wrong).
const OPTIMIZING_CATCH_SITE_CAP: usize = 1024;

/// `(vm identity, ir_method_memo_hash, declaring class id, the VM's redefine
/// epoch)`: [`DivisionGuardSiteKey`] without the bci, for the same reason
/// (interpreter round i1 wave 23): a key by names alone, or by the
/// `SharedVm`'s reissued address, lets another loader's copy, a redefined
/// version or the next VM count as this method.
type OptimizingCatchKey = (usize, u64, u32, u32);

thread_local! {
    /// Catches an optimizing-tier body had to leave compiled code for, per
    /// method, per thread: `(count in the current window, window start)`. Per
    /// thread for the reason `DIVISION_GUARD_TRAPS` is.
    static OPTIMIZING_CATCH_EXITS: std::cell::RefCell<
        rustc_hash::FxHashMap<OptimizingCatchKey, (u32, std::time::Instant)>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Count one catch at `key` at `now`; `true` EXACTLY ONCE per window, on the
/// catch that reaches `threshold` within `window` of the window's first catch.
/// Pure over `counts`.
fn note_optimizing_catch_exit<K: std::hash::Hash + Eq>(
    counts: &mut rustc_hash::FxHashMap<K, (u32, std::time::Instant)>,
    key: K,
    now: std::time::Instant,
    threshold: u32,
    window: std::time::Duration,
    cap: usize,
) -> bool {
    if counts.len() >= cap && !counts.contains_key(&key) {
        counts.clear();
    }
    let entry = counts.entry(key).or_insert((0, now));
    if now.saturating_duration_since(entry.1) > window {
        *entry = (0, now);
    }
    entry.0 = entry.0.saturating_add(1);
    entry.0 == threshold
}

/// Round 12 wave 3, lane exc: stop a hot `catch` from deoptimizing its
/// optimizing-tier method on every exception.
///
/// The optimizing tier compiles no handler blocks. A throw its body can catch
/// leaves through a reason-9 rethrow pad (`ir_lower::emit_rethrow_pads`), and
/// the handler -- and THE REST OF THAT INVOCATION -- runs in the interpreter
/// from a rebuilt frame (`route_jit_signal_exception` at an interpreter door,
/// `run_jit_callee_handler` at a compiled caller's). A method that catches in
/// a loop (`for (..) { try { kernel(); } catch (..) {} }`, the
/// `R12IroptBoundsEdges.round` and `R12IrbackNoTerminator.drive` shapes) pays
/// a frame rebuild per catch plus an interpreted remainder, forever:
/// `PendingException` is never charged, so nothing escalates. The single-pass
/// tier enters its own `catch` in compiled code (`jit_local_handler_lookup`).
///
/// So after [`OPTIMIZING_CATCH_RETIER_THRESHOLD`] such catches within
/// [`OPTIMIZING_CATCH_RETIER_WINDOW`], apply the policy
/// [`retier_hot_division_guard_trap`] applies to a hot zero divisor: ban the
/// IR tier for the method and deoptimize with `SpeculationFailed`, which
/// evicts and recompiles it single-pass. Claimed ONCE per method through the
/// same `claim_site_trap_decision` set, so repeats cannot escalate the method
/// to not-compilable.
///
/// Callers pass only catches of an optimizing body (`used_ir_backend`) whose
/// handler actually ran. Kill switch: `CRATONVM_JIT_IR_CATCH_RETIER=0`
/// (default on).
pub(crate) fn retier_hot_optimizing_catch(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
) {
    if !optimizing_catch_retier_enabled() {
        return;
    }
    let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
    let key: OptimizingCatchKey = (
        vm.vm_identity,
        h,
        declaring_class_id.as_u32(),
        vm.jit.jit_cache.redefine_epoch(),
    );
    let now = std::time::Instant::now();
    let reached = OPTIMIZING_CATCH_EXITS.with(|exits| match exits.try_borrow_mut() {
        Ok(mut counts) => note_optimizing_catch_exit(
            &mut counts,
            key,
            now,
            OPTIMIZING_CATCH_RETIER_THRESHOLD,
            OPTIMIZING_CATCH_RETIER_WINDOW,
            OPTIMIZING_CATCH_SITE_CAP,
        ),
        Err(_) => false,
    });
    if !reached {
        return;
    }
    // Already decided for this method (by this policy, the division guard's,
    // or a site trap): the IR tier is refused already. Not a site trap, so
    // `note_site_trap_repeat` is not charged either. This VM's verdicts, keyed
    // by the declaring class id (interpreter round i1 wave 23).
    let verdicts = crate::runtime::interpreter::jit_bridge::jit_verdicts(vm);
    // JIT round 21 (lane revvm2, LAMBDATRAP21-1): unless the IR ban could not
    // be recorded (`claim_site_trap_policy`, which also records the ban).
    if !claim_site_trap_policy(
        verdicts,
        h,
        (class_name, method_name, descriptor),
        declaring_class_id,
        SiteTrapPolicy::HotCatch,
    ) {
        return;
    }
    // No bci names this decision; 0 keys the ledger row, as any bci would.
    let action = DeoptimizationController::deoptimize_or_by_name(
        vm,
        declaring_class_id,
        class_name,
        method_name,
        descriptor,
        cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
        0,
    );
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] hot-catch re-tier: {class_name}.{method_name}:{descriptor} \
             ({OPTIMIZING_CATCH_RETIER_THRESHOLD} catches outside compiled code within \
             {OPTIMIZING_CATCH_RETIER_WINDOW:?}) -> IR tier refused, action={action:?}"
        );
    }
}

/// `CRATONVM_JIT_IR_CATCH_RETIER` -- default ON; `0`/`false`/`off`/`no` turns
/// [`retier_hot_optimizing_catch`] off. Cached: read on a catch path.
fn optimizing_catch_retier_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_CATCH_RETIER")
    })
}

/// [`retier_hot_optimizing_catch`] for the compiled-caller door: a callee's
/// handler that `try_run_callee_handler` just ran in the interpreter. That door
/// holds no `CompiledMethod`, so the tier is read off the body the cache
/// publishes for the callee; a miss (another key, nothing published) or a
/// single-pass body is not counted.
fn note_optimizing_callee_catch(
    vm: &SharedVm,
    callee: &crate::classloading::resolution::CachedBytecodeMethod,
) {
    if !optimizing_catch_retier_enabled() {
        return;
    }
    let optimizing = vm
        .jit
        .jit_cache
        .read()
        .get(
            &callee.class_name,
            &callee.method_name,
            &callee.method_descriptor,
            callee.declaring_class_id,
        )
        .is_some_and(|body| body.used_ir_backend);
    if optimizing {
        retier_hot_optimizing_catch(
            vm,
            &callee.class_name,
            &callee.method_name,
            &callee.method_descriptor,
            callee.declaring_class_id,
        );
    }
}

#[cfg(test)]
mod optimizing_catch_retier_tests {
    use super::note_optimizing_catch_exit;
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_millis(1_000);

    /// A burst reaches the threshold exactly once: the threshold-th catch
    /// answers `true`, every other one `false`.
    #[test]
    fn a_burst_fires_exactly_once_at_the_threshold() {
        let mut counts = rustc_hash::FxHashMap::default();
        let t0 = Instant::now();
        let fired: Vec<bool> = (0..40u64)
            .map(|i| {
                let at = t0 + Duration::from_millis(i);
                note_optimizing_catch_exit(&mut counts, 7u32, at, 16, WINDOW, 64)
            })
            .collect();
        assert_eq!(fired.iter().filter(|f| **f).count(), 1);
        assert!(fired[15], "the 16th catch is the one that fires");
    }

    /// Catches spaced wider than the window never accumulate: a method that
    /// catches now and then keeps its optimizing body.
    #[test]
    fn a_slow_trickle_never_fires() {
        let mut counts = rustc_hash::FxHashMap::default();
        let t0 = Instant::now();
        for i in 0..100u64 {
            let at = t0 + Duration::from_millis(i * 1_001);
            assert!(!note_optimizing_catch_exit(
                &mut counts,
                7u32,
                at,
                16,
                WINDOW,
                64
            ));
        }
    }

    /// Two methods count separately, and a full table restarts rather than
    /// growing.
    #[test]
    fn keys_are_independent_and_the_table_is_bounded() {
        let mut counts = rustc_hash::FxHashMap::default();
        let t0 = Instant::now();
        for _ in 0..15 {
            assert!(!note_optimizing_catch_exit(
                &mut counts,
                1u32,
                t0,
                16,
                WINDOW,
                4
            ));
            assert!(!note_optimizing_catch_exit(
                &mut counts,
                2u32,
                t0,
                16,
                WINDOW,
                4
            ));
        }
        assert!(note_optimizing_catch_exit(
            &mut counts,
            1u32,
            t0,
            16,
            WINDOW,
            4
        ));
        for k in 10..20u32 {
            let _ = note_optimizing_catch_exit(&mut counts, k, t0, 16, WINDOW, 4);
            assert!(counts.len() <= 4);
        }
    }
}

/// Does the receiver at this call site resolve `info`'s SAM to the method the
/// stashed frame names?
///
/// This is the identity `try_resume_trapped_callee` could not supply from
/// `info` alone, and the one `try_lambda_site_direct_call` already spends on
/// the other door (`site.cached_impl()`). It is read from the metafactory's own
/// record — `LambdaCallSite::impl_handle`, the handle the `invokedynamic`
/// bootstrap bound — rather than from `lambda_jit_site`, whose eligibility
/// gates (static impl, identity coercion, no exception table, the
/// `CRATONVM_JIT_LAMBDA_SITE` switch) decide whether a FAST PATH may serve the
/// call and have nothing to say about which method a trapped frame belongs to.
/// Measured on `DeoptLambdaRerunCount`: that door's `calls` counter is 0 for
/// the shape this arm has to answer for, so keying on it would have been a fix
/// that never fires.
///
/// Every clause is an equality, and all four must hold. `false` for a receiver
/// that is not a lambda proxy, which is the overwhelmingly common case and
/// costs one `FxHashMap` probe under a read lock.
fn lambda_site_resolves_to(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    // `ClassId::new(0)` is the "no receiver" value the dispatch helpers pass for
    // a statically bound site. A lambda SAM call always has one.
    if receiver_class_id.as_u32() == 0 {
        return false;
    }
    let Some(call_site) = vm.classes.lambda_call_site_for(receiver_class_id) else {
        return false;
    };
    // The proxy must answer THIS call — name and descriptor both, the same pair
    // `LambdaJitSite::serves` asks. A default method on the functional
    // interface reaches the same receiver and is not the SAM.
    if &*call_site.sam_method_name != info.method_name
        || !descriptors_match_modulo_return(&call_site.sam_descriptor, info.descriptor)
    {
        return false;
    }
    let h = &call_site.impl_handle;
    (&*h.class_name == key_class && &*h.member_name == key_method && &*h.descriptor == key_desc)
        // JIT round 20 wave 12 (lane qLam, DISP20-3): an instance-method impl
        // entered through a receiver thunk is the body the RECEIVER's class
        // resolves to, which the handle does not name (`Derived.m` for a
        // `Base::m` handle). Same member and descriptor, a virtual / interface
        // handle, the switch on (no receiver thunk exists otherwise), and the
        // trapping body is a virtual target of the handle (its class is the
        // stash's and a subtype of the handle's) or a site of this proxy on
        // this thread binds exactly that method.
        || (matches!(
            h.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        ) && &*h.member_name == key_method
            && &*h.descriptor == key_desc
            && crate::runtime::interpreter::lambda_instance_thunk_enabled()
            && (lambda_stash_body_is_virtual_target_of(vm, &h.class_name, key_class)
                || crate::runtime::interpreter::lambda_instance_site_binds(
                    vm,
                    receiver_class_id,
                    key_class,
                    key_method,
                    key_desc,
                )))
}

/// JIT round 20 wave 12, lane qLam (DISP20-3): is the body that left the
/// pending stash one a virtual dispatch of `handle_class`'s method can select
/// -- published under a class (the cache key: the receiver's class) that is
/// assignable, by name, to both `key_class` (the stash's declaring class) and
/// `handle_class`? `false` when the stash names no
/// published body. Asked only by [`lambda_site_resolves_to`] on a refusal of
/// the handle's own identity.
fn lambda_stash_body_is_virtual_target_of(
    vm: &SharedVm,
    handle_class: &str,
    key_class: &str,
) -> bool {
    let Some(body_class) = cratonvm_jit::deopt::peek_last_deopt_trapping_body()
        .and_then(|body| body.published_class_id())
        .filter(|&id| id != 0)
        .map(ClassId::new)
    else {
        return false;
    };
    // JIT r20 lane rev12 (correctness, unconditional): the published id is
    // the CACHE KEY class -- the receiver's class for an inherited body
    // (`Derived` for a `Base.m` body), not the declaring class the stash key
    // names -- and an interface handle (`I::f`, `List::size`) is reached
    // through the interface DAG, which `is_subclass_of_by_name` (supers only)
    // never walks. Either miss refused the stash on every thread but the one
    // whose site cache bound the method, although the receiver thunk lives in
    // shared code; the refused stash was then orphaned and the impl re-run
    // from entry. The body's class must reach both the stash's declaring
    // class and the handle's class, supers and interfaces alike.
    let cm = vm.classes.class_manager.read();
    cm.is_assignable_to_name(body_class, key_class)
        && cm.is_assignable_to_name(body_class, handle_class)
}

/// What actually trapped in a compiled callee, read from the callee's own
/// artifact rather than assumed — N3(b).
///
/// # Why this is not just `UnreachedCode`
///
/// `try_resume_trapped_callee`'s de-speculation call passed a hard-coded
/// `UnreachedCode`, and `UnreachedCode` is the single reason whose
/// `DeoptimizationLog::recommend_action` is `MakeNotCompilable` on the FIRST
/// occurrence. So the *success* path — the precise, no-side-effect-replayed
/// resume of a trapped callee, the outcome the whole stash machinery exists to
/// produce — permanently retired the callee after one trap of any kind: a
/// `ReceiverTypeChanged` guard that a recompile would re-profile correctly, a
/// `BoundsCheck` speculation, a `NullCheck`, all filed as "this code is
/// unreachable".
///
/// The evidence is in the artifact: the stub baked a `DeoptimizationPoint`, and
/// `osr_exit::deopt_reason_at_bci` reads it — skipping the `rethrow_exception`
/// points (an ordinary stash from `take_last_deopt` cannot have come from one)
/// and refusing rather than guessing when what remains disagrees. That is the
/// same disambiguation `trap_reason_for_frame` performs for the stash sinks in
/// `deopt_resume.rs`; this site differs only in that it has the callee's names
/// and bci rather than a `ReconstructedFrame`, so it must find the artifact
/// itself.
///
/// `UncommonTrap` is the fallback for every way the lookup can come up empty —
/// no artifact in the cache (it may have been evicted between the trap and this
/// call), no point at the bci, or two points that disagree. It is CHARGED, so
/// the trap rate stays visible and the method still backs off, and it escalates
/// on the ordinary count-based schedule rather than one-shot, so an attribution
/// nobody could make can never blacklist a method by itself.
///
/// Reserve `UnreachedCode` for genuinely unreachable code.
fn trapped_callee_deopt_reason(
    vm: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    bci: u32,
    frame: Option<(&[u8], &cratonvm_jit::deopt::ReconstructedFrame)>,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    stashed_point: usize,
    sibling_body: Option<cratonvm_jit::RetainedCode>,
) -> cratonvm_jit::deopt::DeoptReason {
    // `class_id` is the resolved declaring class the caller already holds —
    // the JIT cache's own key. The by-name `get_loaded_class_id` this used
    // answers `None` when two loaders define the name, and the lookup then
    // missed the artifact and charged `UncommonTrap` for a nameable guard.
    let published = {
        let cache = vm.jit.jit_cache.read();
        cache.get(class_name, method_name, descriptor, class_id)
    };
    // JIT round 19 wave 11, lane tail11 (DEOPT10-1 of
    // `jit-r19-deopt10-proposals-RETIRED-20261009.md`, opt-in
    // `CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY`): ask the body the stash
    // NAMED (`taken_trapping_body`) before the published one. A charged trap
    // is never a superseded body's (the caller's check), so the two agree
    // whenever the slot holds that body. They differ for an EMPTY slot (the
    // body withdrawn by a redefinition or an interpreter-only request while
    // its activation ran), where the published lookup answers `None` and the
    // trap was charged `UncommonTrap` -- the wrong speculation id for a
    // nameable guard; and for a body published by another thread BETWEEN the
    // caller's superseded check and this read, whose points at `bci` describe
    // another compile. Off: the published body only, as before.
    let named_on = charge_reason_from_named_body_enabled();
    let named = (stashed_point != 0
        && (named_on || published.is_none())
        && cratonvm_jit::deopt::stash_names_trapping_body_enabled())
    .then(|| stale_trap_not_entrant::taken_trapping_body_retained(stashed_point))
    .flatten();
    // j27 wave 7 (lane T1LAM3): every handle below is a `RetainedCode`, so
    // whichever is the last owner releases through the retirement queue.
    let published = published.map(cratonvm_jit::RetainedCode::new);
    if published.is_none() && dbg_deopt_on() {
        // The census that decides DEOPT10-1: a charge with an empty slot,
        // and whether the stash named the body that trapped.
        eprintln!(
            "[cratonvm-deopt] callee trap charged with an empty slot: \
             {class_name}.{method_name}:{descriptor} bci={bci} named-body={} \
             used={} (CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY)",
            named.is_some(),
            if named_on { "named" } else { "published" },
        );
    }
    // A trap of a body published under ANOTHER key of the same method (the
    // receiver class's, for an inherited method; `sibling_key` at the charge
    // site) is read from THAT body. The slot the declaring key publishes holds
    // a different compile, whose points at `bci` need not include the guard
    // that fired: `CharsetDecoder.decode(BB,CB,Z)`'s loop-header poll exit
    // (`OsrExit`, bci 54) was then the only point there, the trap was charged
    // as an `OsrExit` and, two charges later, `MakeNotCompilable` -- the method
    // interpreted for the rest of the run -- while the `UncommonTrap` that
    // actually fired was never de-specialized and every recompile carried it
    // again.
    let compiled = if sibling_body.is_some() {
        sibling_body
    } else if named_on {
        named.or(published)
    } else {
        published
    };
    match (compiled.as_deref(), frame) {
        // With the reconstructed frame in hand, the rule every other sink
        // applies (`trap_reason_for_frame`: rethrow and OSR-exit points
        // skipped, the array NullCheck/BoundsCheck pair told apart by the
        // array reference on the frame's operand stack). Before round 11
        // wave 3 this sink alone read the bare bci, so one trap was charged
        // under a different reason depending on which sink caught it.
        (Some(compiled), Some((code, rframe))) => {
            crate::runtime::interpreter::trap_reason_for_frame_with_cause(
                compiled, code, rframe, cause,
            )
        }
        (compiled, _) => {
            deopt_reason_from_artifact(compiled.map(|c| &c.deopt_points[..]), bci, cause)
        }
    }
}

/// `CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY` (JIT round 19 wave 11, lane
/// tail11, DEOPT10-1; opt-in): [`trapped_callee_deopt_reason`] reads an
/// empty-slot trap's reason from the body the stash named. Read per charge
/// (cold); not cached.
fn charge_reason_from_named_body_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DEOPT_CHARGE_REASON_FROM_NAMED_BODY")
}

/// The decision half of [`trapped_callee_deopt_reason`], with the artifact
/// lookup already done.
///
/// Split out so the rule is testable without a live `SharedVm`: every arm here
/// is a policy choice about what to charge, and every one of them used to be a
/// hard-coded `UnreachedCode`. `None` for the slice means the artifact was not
/// in the cache — it can be evicted between the trap and this call, and that is
/// not evidence about the method either.
///
/// Since interpreter round i1 wave 49 (lane L2, behind
/// `cratonvm_jit::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`) the stash's own
/// `cause` is believed first, under `trap_cause_at_frame`'s rule (a
/// non-rethrow point of this artifact at `bci` with that reason and, for a
/// non-wildcard id, that id): the frame-less arm (an inlined chain) was the
/// one production sink that still dropped it, and an `OsrExit` map may now
/// share its bci with a guard, where only the cause says which stub fired.
fn deopt_reason_from_artifact(
    points: Option<&[cratonvm_jit::deopt::DeoptimizationPoint]>,
    bci: u32,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> cratonvm_jit::deopt::DeoptReason {
    if let (true, Some(points), Some(c)) = (
        cratonvm_jit::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS,
        points,
        cause,
    ) {
        let named = points.iter().any(|p| {
            p.bci == bci
                && p.reason == c.reason
                && !p.semantics.rethrow_exception
                && (c.speculation_id == cratonvm_jit::deopt::SPECULATION_ID_ANY
                    || p.speculation_id == c.speculation_id)
        });
        if named {
            return c.reason;
        }
    }
    match points {
        Some(points) => match cratonvm_jit::osr_exit::deopt_reason_at_bci(points, bci) {
            cratonvm_jit::osr_exit::ReasonAtBci::Unique(r) => r,
            cratonvm_jit::osr_exit::ReasonAtBci::Ambiguous
            | cratonvm_jit::osr_exit::ReasonAtBci::None => {
                cratonvm_jit::deopt::DeoptReason::UncommonTrap
            }
        },
        None => cratonvm_jit::deopt::DeoptReason::UncommonTrap,
    }
}

/// [`despeculate_trapped_method_of_frame`] with the per-bci de-spec's grain
/// given by the caller: `Some(c)` counts at `(c.reason, bci)` and withdraws
/// `c.speculation_id` (the doors' rule for an attribution they named), `None`
/// keeps the inner function's own rule. An IR site trap always records the
/// wildcard. Round nio21 wave 7, lane L2 (folded onto dev's frame-aware form
/// at the nio21 merge; placed after [`despeculate_trapped_method_inner`] so
/// the `r19w3_eager3_tests` source window still reaches the inner body).
#[allow(clippy::too_many_arguments)]
fn despeculate_trapped_method_of_frame_despec_as(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    fallback_reason: cratonvm_jit::deopt::DeoptReason,
    bci: u32,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    despec_as: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    let is_chain = !rframe.caller_frames.is_empty();
    let chain = (is_chain && cratonvm_jit::ir::ir_splice_expected_throw_stash_sinks_enabled())
        .then_some(rframe);
    // JIT round 19 wave 12, lane tail12: the switch-off chain records the
    // wildcard at the call site (`helper_chain_site_wildcard_enabled`).
    let chain_site_wildcard = is_chain && chain.is_none() && helper_chain_site_wildcard_enabled();
    despeculate_trapped_method_inner(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        fallback_reason,
        bci,
        chain,
        chain_site_wildcard,
        despec_as,
    );
}

/// `CRATONVM_JIT_CALLSITE_CHAIN_TRAP_CHARGE`, **default ON** (round nio21
/// wave 7, lane L2): the call-site service charges an inlined chain's trap as
/// the doors do ([`chain_trap_charge`]). `=0` restores the reason read from
/// the artifact's points at the call site. Read only on a chain trap.
fn callsite_chain_trap_charge_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CALLSITE_CHAIN_TRAP_CHARGE")
}

/// The doors' chain attribution (`deopt_resume::stash_charge_site`, the
/// chain arm of `stash_trap_attribution`) for the call-site service: for an
/// inlined chain stash, the stash's own reason (else `UncommonTrap`) under the
/// wildcard speculation id, to be charged and de-spec'd at the call site.
/// `None` for a single frame, the superseded-guard sentinel, or under
/// `CRATONVM_JIT_CALLSITE_CHAIN_TRAP_CHARGE=0`. (The W3L4-4 nested-trap named
/// id needs the trapping artifact's spliced sources, which this sink does not
/// hold; the wildcard is the coarser answer the doors gave before W3L4-4.)
fn chain_trap_charge(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<cratonvm_jit::deopt::DeoptCause> {
    if rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
        || !callsite_chain_trap_charge_enabled()
    {
        return None;
    }
    Some(cratonvm_jit::deopt::DeoptCause {
        reason: cause.map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason),
        // JIT round 23 wave 4 (lane L1): a spliced loop's entry predicate is
        // named by its chain id, which the planter asks at the call site;
        // every other chain trap keeps the wildcard.
        // JIT round 25 (item 4 residual 1): so is a spliced field site's
        // compactness guard (`chain_named_charge_id`).
        speculation_id: crate::runtime::interpreter::chain_named_charge_id(rframe, cause)
            .unwrap_or(cratonvm_jit::deopt::SPECULATION_ID_ANY),
    })
}

/// JIT round 21 wave 1 (lane loops, LOOPS21-2): under
/// `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`, the per-bci grain of a
/// single-frame trap whose stashed cause is a loop-entry predicate guard's
/// (`UncommonTrap` under a planter's id,
/// `cratonvm_jit::deopt::is_predicate_speculation_id` at the trap's bci):
/// counted at `(UncommonTrap, bci)`, withdrawn under that id, so the next
/// compile drops that planter's predicate alone. `None` (the sink's own rule:
/// the wildcard for an unnamed `UncommonTrap`) for a chain, the
/// superseded-guard sentinel, any other cause or reason, or the switch off.
fn predicate_trap_charge(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    bci: u32,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    trap_reason: cratonvm_jit::deopt::DeoptReason,
) -> Option<cratonvm_jit::deopt::DeoptCause> {
    use cratonvm_jit::deopt::DeoptReason;
    if !rframe.caller_frames.is_empty()
        || rframe.bci == u32::MAX
        || rframe.bci != bci
        || trap_reason != DeoptReason::UncommonTrap
        || !cratonvm_jit::deopt::predicate_own_speculation_enabled()
    {
        return None;
    }
    let c = cause?;
    (c.reason == DeoptReason::UncommonTrap
        && cratonvm_jit::deopt::is_predicate_speculation_id(bci, c.speculation_id))
    .then_some(c)
}

/// JIT round 23 wave 3 review (`CRATONVM_JIT_OSR_PREDICATE_EXIT_CHARGE`;
/// `docs/feature-designs/deopt-osr.md`):
/// charge an optimizing OSR body's loop-entry predicate exit exactly as the
/// trapped-frame sink charges a method-entry body's predicate trap. Counted at
/// `(UncommonTrap, bci)`. It is withdrawn under the planter's own id when
/// [`predicate_trap_charge`] names it (`CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`),
/// and otherwise under the wildcard at the header, which
/// `cratonvm_jit::deopt::predicate_withdrawn` and the OSR arm checks both
/// read. The caller (`jit_bridge::charge_osr_guard_exit`) has already decided
/// that the exit is such a predicate (`jit_bridge::osr_predicate_exit_charge`)
/// and that the frame is flat.
pub(crate) fn despeculate_osr_predicate_exit(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    declaring_class_id: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    let reason = cratonvm_jit::deopt::DeoptReason::UncommonTrap;
    let despec_as = predicate_trap_charge(rframe, rframe.bci, cause, reason);
    despeculate_trapped_method_of_frame_despec_as(
        vm,
        class_name,
        method_name,
        descriptor,
        declaring_class_id,
        reason,
        rframe.bci,
        rframe,
        despec_as,
    );
}

#[cfg(test)]
mod r21_loops_predicate_trap_charge_tests {
    use super::predicate_trap_charge;
    use cratonvm_jit::deopt::{
        predicate_speculation_id, speculation_id, DeoptCause, DeoptReason, PredicateKind,
        ReconstructedFrame,
    };

    const KEY: &str = "CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION";

    fn frame(bci: u32) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "p/C.m:()V".into(),
            bci,
            ..Default::default()
        }
    }

    /// A predicate guard's cause is named (switch on) and nothing else is.
    #[test]
    fn r21_loops_a_predicate_trap_is_named_only_under_the_switch() {
        let pred = DeoptCause {
            reason: DeoptReason::UncommonTrap,
            speculation_id: predicate_speculation_id(9, PredicateKind::Class),
        };
        let plain = DeoptCause {
            reason: DeoptReason::UncommonTrap,
            speculation_id: speculation_id(9, DeoptReason::UncommonTrap),
        };
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            let t = DeoptReason::UncommonTrap;
            assert_eq!(predicate_trap_charge(&frame(9), 9, Some(pred), t), Some(pred));
            assert_eq!(predicate_trap_charge(&frame(9), 9, Some(plain), t), None);
            assert_eq!(predicate_trap_charge(&frame(9), 9, None, t), None);
            // Another bci's id, the sentinel, another reason.
            assert_eq!(predicate_trap_charge(&frame(10), 10, Some(pred), t), None);
            assert_eq!(predicate_trap_charge(&frame(u32::MAX), 9, Some(pred), t), None);
            assert_eq!(
                predicate_trap_charge(&frame(9), 9, Some(pred), DeoptReason::NullCheck),
                None
            );
            // A chain keeps the doors' grain.
            let mut chain = frame(9);
            chain.caller_frames = vec![frame(3)];
            assert_eq!(predicate_trap_charge(&chain, 9, Some(pred), t), None);
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                predicate_trap_charge(&frame(9), 9, Some(pred), DeoptReason::UncommonTrap),
                None
            );
        });
    }
}

#[cfg(test)]
mod nio21w7_chain_trap_charge_tests {
    use super::chain_trap_charge;
    use cratonvm_jit::deopt::{DeoptCause, DeoptReason, ReconstructedFrame, SPECULATION_ID_ANY};

    fn chain(bci: u32) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "p/Callee.m:()V".into(),
            bci,
            caller_frames: vec![ReconstructedFrame {
                method_key: "p/Outer.run:()V".into(),
                bci: 12,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// nio21 wave 7 (L2): a chain is charged under the stash's reason with
    /// the wildcard (the doors' grain), `UncommonTrap` without a cause; a
    /// single frame and the sentinel keep the service's own attribution.
    #[test]
    fn nio21w7_a_chain_trap_is_charged_under_the_stash_reason_with_the_wildcard() {
        if !super::callsite_chain_trap_charge_enabled() {
            return; // the kill switch is set in this environment
        }
        let cause = DeoptCause {
            reason: DeoptReason::ReceiverTypeChanged,
            speculation_id: 77,
        };
        assert_eq!(
            chain_trap_charge(&chain(3), Some(cause)),
            Some(DeoptCause {
                reason: DeoptReason::ReceiverTypeChanged,
                speculation_id: SPECULATION_ID_ANY,
            })
        );
        assert_eq!(
            chain_trap_charge(&chain(3), None).map(|c| c.reason),
            Some(DeoptReason::UncommonTrap)
        );
        assert_eq!(chain_trap_charge(&chain(u32::MAX), Some(cause)), None);
        let single = ReconstructedFrame {
            method_key: "p/Outer.run:()V".into(),
            bci: 3,
            ..Default::default()
        };
        assert_eq!(chain_trap_charge(&single, Some(cause)), None);
    }
}

/// The reason a trapped INLINED CHAIN is charged under at the call-site
/// service, under `CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS` (JIT round 19
/// wave 10, lane deopt10; opt-in): the stash's own cause, else
/// `UncommonTrap` -- `deopt_resume::stash_charge_site`'s chain arm, so the
/// door sinks and this one charge one chain trap alike. The trap fired at a
/// bci of a spliced callee; the outer method's points at the outermost
/// scope's bci describe the call site, not the guard that failed.
fn chain_trap_charge_reason(
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> cratonvm_jit::deopt::DeoptReason {
    cause.map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason)
}

/// A pure deopt in the callee this call site invoked, still stashed because
/// [`try_resume_trapped_callee`] declined to resume it (a redefined callee, a
/// class the site's loader cannot resolve, a synchronized callee, a frame that
/// cannot be materialised): re-run that callee from entry in the interpreter
/// and hand its result, value or pending throwable, to the compiled caller.
///
/// Round 11 wave 12 (lane tier's patch
/// `r11w12-tier-helpers-declined-callee-rerun-patch-FIXED-20260924.md`, applied by
/// lane hashmap). Left stashed, the frame reached a sink that could not resume
/// it either: an OSR'd caller replayed its loop from the entry pc (`try_osr`'s
/// identity-mismatch arm), and a method-entry caller re-ran from bci 0. Both
/// call the callee again, so re-running only the callee duplicates strictly
/// less. The stash is charged as those sinks charged it.
///
/// `None`, touching nothing, when the stash does not name this site's callee.
///
/// The arguments are read through `pins` AFTER the de-speculation (as the
/// KCFULL-13 re-run reads them), so a collection in between cannot hand the
/// interpreter a stale reference word where the caller pinned one.
///
/// # Safety
/// As [`try_resume_trapped_callee`].
unsafe fn rerun_declined_callee_from_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
) -> Option<i64> {
    // The identity question `try_resume_trapped_callee` asks, on the same
    // (outermost) scope.
    let (key, _) = cratonvm_jit::deopt::peek_last_deopt_outermost_identity()?;
    let (rest, key_desc) = key.rsplit_once(':')?;
    let (key_class, key_method) = rest.rsplit_once('.')?;
    let names_this_callee = (key_method == info.method_name
        && descriptors_match_modulo_return(key_desc, info.descriptor))
        || lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc);
    if !names_this_callee
        || !stash_reachable_from_site(vm, info, receiver_class_id, key_class, key_method, key_desc)
    {
        return None;
    }
    let (rframe, cause, point_addr) = cratonvm_jit::deopt::take_last_deopt_with_point()?;
    // The callee's frame is dropped and the callee re-runs from entry, so what
    // its compiled code locked is released first; the re-run takes its own
    // (interpreter round i1 wave 23, lane L2). Pinned before the charge below.
    let compiled_locks =
        crate::runtime::interpreter::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
    // Round 12 wave 5 (lane replay3): counted in the door census with the
    // replay rule's verdict for the callee's own bytecode, so the double
    // commits this service still makes are sized (`callee-declined`).
    //
    // Round 13 wave 8 (lane replay4): a stash class the site's loader cannot
    // resolve (one of the declines that brought the stash here) is answered
    // from the method the re-run dispatches to, by the whole-body rule, where
    // it used to read unsound unchecked; and an unsound re-run is refused.
    //
    // Round 13 wave 9 (lane replay5): judged BEFORE the charge below. The
    // verdict reads the published body (its `spliced_bodies_side_effect_free`
    // and whether it owns the stashed point), and the charge evicts that body
    // by name (`DeoptimizationController::deoptimize_in`), so judged after it
    // every storing callee read UNSOUND whatever its trap bci (the `None`
    // artifact arm assumes impure splices): a spurious `InternalError` where
    // the prefix rule proves the re-run exact. Kill switch
    // `CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST=0`.
    let verdict = || {
        callee_rerun_replay_is_exact(
            vm,
            info.declaring_class_id,
            key_class,
            key_method,
            key_desc,
            Some((&rframe, point_addr)),
        )
        .unwrap_or_else(|| {
            callee_rerun_code(vm, info, receiver_class_id).is_some_and(|(_, _, code)| {
                crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                    &code,
                    false,
                    u32::MAX,
                )
            })
        })
    };
    let judged_first = cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST",
    )
    .then(&verdict);
    // Not charged when the callee left for the interpreter-only mode (wave 15).
    crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
        vm, &rframe, cause, point_addr,
    );
    compiled_locks.release_for_a_rerun(vm, thread);
    // The flag a frame-stashing trap raises beside its stash is spent with it.
    let _ = take_jit_deopt_pending();
    let exact = judged_first.unwrap_or_else(verdict);
    let cause = crate::runtime::interpreter::DoorRerunCause::CalleeDeclined;
    crate::runtime::interpreter::note_door_rerun(cause, exact);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] declined callee trap re-run at the call site: {}.{}{} \
             (stash {} bci={}) replay={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            rframe.method_key,
            rframe.bci,
            if exact { "exact" } else { "UNSOUND" },
        );
    }
    drop(rframe);
    // A redefined callee keeps its re-run (the owner decision for a hot swap;
    // `try_resume_trapped_callee` declines every stash of a body compiled
    // before its class's last redefinition, and this is where those land).
    // By name, or any redefinition at all when the name is another loader's
    // too: the conservative side keeps the re-run.
    if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
        named_class_was_redefined(vm, key_class)
            || (crate::classloading::any_class_redefined()
                && vm
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(key_class)
                    .is_none())
    }) {
        let callee = callee_rerun_code(vm, info, receiver_class_id);
        return Some(refuse_callee_rerun(
            vm,
            thread,
            info,
            callee.as_ref(),
            cause,
            "a declined stash",
        ));
    }
    let bail_args = decode_dispatch_values(vm, info, &args_now(pins, thread, args_slice));
    Some(bail_to_interpreter(vm, thread, info, &bail_args))
}

/// gcd d9/d (2026-09-28): the callee-owned-arguments twin of
/// [`rerun_declined_callee_from_entry`], for a call site that kept no copy of
/// its arguments (`cratonvm_jit::SERVICE_ARGS_OWNED_BY_CALLEE`).
///
/// Everything that function does before its re-run -- the same identity
/// question on the outermost scope, the take, the release of what the
/// callee's compiled code locked, the charge, the spent deopt flag -- and no
/// re-run: there are no arguments to run it with. The caller then leaves as
/// a frameless deopt ([`handle_compiled_callee_deopt_sentinel`] raises the
/// flag again) and is re-run by its own caller, whose arguments that caller
/// still holds.
///
/// `None`, touching nothing, when there is no stash or it does not name this
/// site's callee (a foreign stash keeps propagating, as before). Otherwise
/// the callee replay's verdict (gce e1/f: the hand-off's record,
/// [`FramelessTrapSite::Handoff`]).
fn drop_declined_callee_stash(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<bool> {
    let (key, _) = cratonvm_jit::deopt::peek_last_deopt_outermost_identity()?;
    let (rest, key_desc) = key.rsplit_once(':')?;
    let (key_class, key_method) = rest.rsplit_once('.')?;
    let names_this_callee = (key_method == info.method_name
        && descriptors_match_modulo_return(key_desc, info.descriptor))
        || lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc);
    if !names_this_callee
        || !stash_reachable_from_site(vm, info, receiver_class_id, key_class, key_method, key_desc)
    {
        return None;
    }
    let (rframe, cause, point_addr) = cratonvm_jit::deopt::take_last_deopt_with_point()?;
    // gcd d10/f: the callee is re-run from entry all the same -- inside the
    // caller's re-run, whose own prefix the site's admission proved commits
    // nothing (`x64/op_invoke.rs`, `owned_args_handoff_replays_exactly`) --
    // so the callee's replay is counted exactly as the copying site's
    // `rerun_declined_callee_from_entry` counts it (`callee-declined`): judged
    // BEFORE the charge below, which evicts the body the stash rule reads
    // (round 13's `CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST` ordering), and
    // with the same whole-body fallback when the method cannot be found.
    let exact = callee_rerun_replay_is_exact(
        vm,
        info.declaring_class_id,
        key_class,
        key_method,
        key_desc,
        Some((&rframe, point_addr)),
    )
    .unwrap_or_else(|| {
        // SAFETY: `callee_rerun_code`'s contract is `resolve_callee_uncached`'s
        // (`vm` live, `info` a live `JitInvokeInfo`), which this function's
        // only caller, `handle_compiled_callee_deopt_sentinel`, holds.
        unsafe { callee_rerun_code(vm, info, receiver_class_id) }.is_some_and(|(_, _, code)| {
            crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                &code,
                false,
                u32::MAX,
            )
        })
    });
    // As the re-run: the frame is dropped, so what its compiled code locked is
    // released, through pins taken before the charge (which can collect).
    let compiled_locks =
        crate::runtime::interpreter::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
    crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
        vm, &rframe, cause, point_addr,
    );
    compiled_locks.release_for_a_rerun(vm, thread);
    let _ = take_jit_deopt_pending();
    crate::runtime::interpreter::note_door_rerun(
        crate::runtime::interpreter::DoorRerunCause::CalleeDeclined,
        exact,
    );
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] declined callee trap dropped at an owned-arguments call site: \
             {}.{}{} (stash {} bci={}) callee replay={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            rframe.method_key,
            rframe.bci,
            if exact { "exact" } else { "UNSOUND" },
        );
    }
    Some(exact)
}

/// gcd d9/d (2026-09-28): name, under `CRATONVM_DBG_DEOPT`, a callee trap an
/// owned-arguments call site hands to the caller's caller instead of
/// re-running the callee (`why`: which arm could not run). Rare by the
/// site's own admission rule (`sp_direct_call_args_owned_by_callee` and
/// `cratonvm_jit::callee_body_owns_its_arguments`); a workload where these
/// lines are frequent is one where `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` should
/// be compared.
fn note_callee_owned_args_trap_propagated(info: &JitInvokeInfo, why: &str) {
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] owned-arguments call site: {why} of {}.{}{} leaves as the \
             caller's frameless deopt (no argument copy to re-run it with)",
            info.class_name, info.method_name, info.descriptor,
        );
    }
}

/// The replay rule's verdict for a call-site service about to re-run a callee
/// from entry, for the door census (round 12 wave 5, lane replay3;
/// `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns` cause 4).
///
/// `class_name` is read as the method's DECLARING class (a stash key names
/// it), resolved from `from_class_id`'s loader (`0`: the bootstrap loader) as
/// `try_resume_trapped_callee` resolves it; only that class's own methods are
/// searched. `stash` is the callee's frame with its point address; `None`
/// asks the whole-body rule. A method this cannot find is answered `None`
/// (round 13 wave 8: the caller decides; a census must not call exact a
/// re-run it could not check).
fn callee_rerun_replay_is_exact(
    vm: &SharedVm,
    from_class_id: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    stash: Option<(&cratonvm_jit::deopt::ReconstructedFrame, usize)>,
) -> Option<bool> {
    let found = {
        let cm = vm.classes.class_manager.read();
        let class_id = if from_class_id == 0 {
            cm.find_bootstrap_class_by_name(class_name)
        } else {
            cm.find_cp_class_for_class(class_name, ClassId::new(from_class_id))
        };
        class_id.and_then(|class_id| {
            let class = cm.class_store().get(class_id)?;
            let method = class
                .methods
                .iter()
                .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)?;
            let code_attr = method.code()?;
            Some((
                class_id,
                crate::runtime::frame::padded_bytecode_for_method(
                    class_id,
                    method_name,
                    descriptor,
                    &code_attr.code,
                    &code_attr.exception_table,
                ),
            ))
        })
    };
    let (class_id, code) = found?;
    let artifact = vm
        .jit
        .jit_cache
        .read()
        .get(class_name, method_name, descriptor, class_id);
    // A chain's `bci` is its innermost scope's, not this method's: only the
    // whole-body rule can answer for the outermost method.
    let stash = stash.filter(|(rframe, _)| rframe.caller_frames.is_empty());
    Some(match (stash, artifact.as_deref()) {
        (Some((rframe, point)), Some(ran)) => {
            crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_for_stash(
                &code, ran, rframe, point,
            )
        }
        (Some((rframe, _)), None) => {
            crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                &code, false, rframe.bci,
            )
        }
        // Round 13 wave 1 (lane replay): a body whose frameless traps the
        // single-pass finalizer proved exact re-runs exactly, as the doors'
        // verdict reads it (`door_rerun_verdict`).
        (None, ran) => {
            ran.is_some_and(|r| r.frameless_traps_replay_exact)
                || crate::runtime::interpreter::replay_from_entry_is_observably_equivalent(
                    &code,
                    ran.is_some_and(|r| r.spliced_bodies_side_effect_free),
                    u32::MAX,
                )
        }
    })
}

/// Was the trap stashed right now taken in the body currently published for
/// `key_class.key_method key_desc`, compiled after that class's last
/// redefinition (interpreter round i1 wave 23, lane L6)? Such a body ran the
/// class's CURRENT bytecode, which is what [`try_resume_trapped_callee`]
/// rebuilds the frame from, so the frame resumes exactly -- as the
/// interpreter door's sink already resumes a fresh body of a redefined class
/// (`deopt_resume`'s `compilation_epoch` test). Refusing it re-ran the callee
/// from entry (`rerun_declined_callee_from_entry`) and replayed every effect
/// it had committed before the trap: for EVERY guard trap in a compiled
/// callee of a class an agent had retransformed once, however long ago. The
/// not-entrant stub's forwarding (`NotEntrantRecord::forward_to`) sends a
/// patched body's callers straight into exactly such a body.
///
/// Identity by the stashed point's address: it must lie in that body's
/// `deopt_points` or be one of its boxed points. A superseded body, one
/// published under another key (an inherited method under a receiver's key),
/// or a class the call site's loader does not resolve answers `false`, the
/// conservative refusal -- except (wave 61, lane L2) a body the stash itself
/// names, which answers by its own compile epoch whether or not it is still
/// published.
fn stash_is_from_a_current_body(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    let point = cratonvm_jit::deopt::peek_last_deopt_point();
    if point == 0 {
        return false;
    }
    let class_id = {
        let cm = vm.classes.class_manager.read();
        if info.declaring_class_id == 0 {
            cm.find_bootstrap_class_by_name(key_class)
        } else {
            cm.find_cp_class_for_class(key_class, ClassId::new(info.declaring_class_id))
        }
    };
    let Some(class_id) = class_id else {
        return false;
    };
    let by_name = vm
        .jit
        .jit_cache
        .get(key_class, key_method, key_desc, class_id)
        .is_some_and(|body| {
            body.owns_deopt_point(point)
                && vm
                    .jit
                    .jit_cache
                    .compiled_since_redefinition_of(class_id, body.install_epoch)
        });
    // Interpreter round i2 wave 61, lane L2: the superseded test asks the
    // body the trampoline named first (`named_trapping_body_is_current`); a
    // body superseded by a tier-up after a compile that began past the
    // redefinition ran the current bytecode too.
    named_trapping_body_is_current(vm, class_id, point, by_name, "current-body").unwrap_or(by_name)
}

/// gch h2/fj (`gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection-20260928`,
/// step 1): push every object word of a frame just TAKEN out of the deopt
/// stash onto `pins` (a thread's `native_pin_roots`), in
/// [`cratonvm_jit::deopt::ReconstructedFrame::for_each_object_address`]
/// order -- locals, stack, monitors, scalar-replaced objects' fields, caller
/// scopes. Out of the stash nothing else roots them, and the rebuild that
/// follows can collect. Answers how many were pushed.
fn pin_taken_stash_frame_words(
    pins: &mut Vec<ObjectRef>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> usize {
    let base = pins.len();
    rframe.for_each_object_address(&mut |addr: u64| {
        // SAFETY: a non-null heap reference (the visitor never offers `0`)
        // the stash rooted until the take; it is pinned before anything can
        // collect.
        pins.push(unsafe { ObjectRef::from_raw(addr as usize as *mut u8) });
    });
    pins.len() - base
}

/// Step 2 of the same proposal: write the pins [`pin_taken_stash_frame_words`]
/// pushed at `pins[base..base + count]` (rewritten in place by any moving
/// collection since) back into `rframe`'s object words, in the same order, so
/// the frame names current addresses and may go back into the stash. Answers
/// `false`, leaving `rframe` untouched, when the pin window or the frame's
/// word count no longer matches (the caller then drops the frame, as before).
fn refresh_taken_stash_frame_words(
    pins: &[ObjectRef],
    base: usize,
    count: usize,
    rframe: &mut cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    let Some(window) = base.checked_add(count).and_then(|end| pins.get(base..end)) else {
        return false;
    };
    let mut words = 0usize;
    rframe.for_each_object_address(&mut |_addr: u64| words += 1);
    if words != count {
        return false;
    }
    let mut k = 0usize;
    rframe.for_each_object_address_mut(&mut |addr: u64| {
        let now = window.get(k).map(|pin| pin.as_ptr() as usize as u64);
        k += 1;
        now.filter(|&now| now != addr)
    });
    true
}

#[cfg(test)]
mod gch_h2fj_restash_tests {
    use super::{pin_taken_stash_frame_words, refresh_taken_stash_frame_words, ObjectRef};
    use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame};

    fn frame() -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci: 3,
            locals: vec![
                FrameValue::Object(0x1000),
                FrameValue::Int(7),
                FrameValue::Object(0),
                FrameValue::Object(0x2000),
            ],
            stack: vec![FrameValue::Long(-1), FrameValue::Object(0x3000)],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(0x1000),
                lock_depth: 1,
                relock: false,
            }],
            ..ReconstructedFrame::default()
        }
    }

    /// gch h2/fj (d10f restash): the frame's words are pinned in visiting
    /// order (nulls and scalars skipped) and read back from their pins after
    /// a simulated move, so a frame refused after a collection is restashed
    /// at current addresses.
    #[test]
    fn gch_h2fj_a_refused_frame_reads_its_words_back_from_its_pins() {
        // SAFETY: never dereferenced; only addresses are compared.
        let lock = unsafe { ObjectRef::from_raw(0x8000usize as *mut u8) };
        let mut pins = vec![lock];
        let mut rf = frame();
        let n = pin_taken_stash_frame_words(&mut pins, &rf);
        assert_eq!(n, 4, "three locals/stack words and the monitor, no null");
        let seen: Vec<usize> = pins[1..].iter().map(|p| p.as_ptr() as usize).collect();
        assert_eq!(seen, vec![0x1000, 0x2000, 0x3000, 0x1000]);
        // A moving collection rewrote two of the pins in place.
        // SAFETY: as above.
        pins[1] = unsafe { ObjectRef::from_raw(0x9000usize as *mut u8) };
        pins[3] = unsafe { ObjectRef::from_raw(0xa000usize as *mut u8) };
        assert!(refresh_taken_stash_frame_words(&pins, 1, n, &mut rf));
        assert_eq!(rf.locals[0], FrameValue::Object(0x9000));
        assert_eq!(rf.locals[1], FrameValue::Int(7));
        assert_eq!(rf.locals[2], FrameValue::Object(0));
        assert_eq!(rf.locals[3], FrameValue::Object(0x2000));
        assert_eq!(rf.stack[1], FrameValue::Object(0xa000));
        assert!(matches!(rf.monitors[0].object, FrameValue::Object(0x1000)));
        assert_eq!(
            pins[0].as_ptr() as usize,
            0x8000,
            "the pins below are not read"
        );
    }

    /// A window that does not match the frame is refused and leaves it alone.
    #[test]
    fn gch_h2fj_a_mismatched_pin_window_is_refused() {
        let mut pins = Vec::new();
        let mut rf = frame();
        let n = pin_taken_stash_frame_words(&mut pins, &rf);
        assert!(
            !refresh_taken_stash_frame_words(&pins, 0, n - 1, &mut rf),
            "short count"
        );
        assert!(
            !refresh_taken_stash_frame_words(&pins, 1, n, &mut rf),
            "past the end"
        );
        pins.truncate(2);
        assert!(
            !refresh_taken_stash_frame_words(&pins, 0, n, &mut rf),
            "truncated"
        );
        assert_eq!(rf.locals[0], FrameValue::Object(0x1000));
        let mut empty = ReconstructedFrame::default();
        assert_eq!(pin_taken_stash_frame_words(&mut pins, &empty), 0);
        assert!(refresh_taken_stash_frame_words(
            &pins,
            pins.len(),
            0,
            &mut empty
        ));
    }

    /// The switch is read once per VM, default OFF.
    #[test]
    fn gch_h2fj_the_restash_switch_is_per_vm_and_default_off() {
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION", Some("1"))],
            cratonvm_jit::JitCache::new,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION", None)],
            cratonvm_jit::JitCache::new,
        );
        assert!(
            on.refused_resume_restashes_after_collection(),
            "=1 at construction"
        );
        assert!(
            !off.refused_resume_restashes_after_collection(),
            "default OFF"
        );
    }
}

/// `CRATONVM_JIT_CALLEE_SERVICE_STASH_AGE_TEST` (HANDLER11-4, JIT round 19
/// wave 12, lane handler12; default ON since JIT round 20 (40/40 loaded
/// R16LocaleWide runs per arm, R20StashNamesake equal to HotSpot), `0` kills): see
/// [`stash_reachable_from_site`]. Read only by a service that holds a stash
/// naming its callee (cold).
fn callee_service_stash_age_test_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CALLEE_SERVICE_STASH_AGE_TEST")
}

/// HANDLER11-4 (JIT round 19 wave 12, lane handler12;
/// `r19-callsite-service-claims-a-stash-by-name-and-parameters-only-FIXED-20261012.md`):
/// may the call-site services ([`try_resume_trapped_callee`],
/// [`rerun_declined_callee_from_entry`], [`drop_declined_callee_stash`]) claim
/// the innermost stash entry, which already names this site's callee by method
/// name and parameter descriptor (`key_method` is the stash's outermost
/// method)? Those two terms alone let an ORPHAN -- a frame some earlier
/// consumer left -- be resumed as this call's callee whenever the names agree:
/// another class's namesake, whose result word need not even be of the
/// caller's kind (the `0x7`-reference reading of the R16LocaleWide stress).
///
/// Under the switch two more terms, both fail-open where nothing is known:
///
/// * the entry was not stamped as left by a consumer that already completed
///   its call (`cratonvm_jit::deopt::peek_last_deopt_left_by`, HANDLER11-3):
///   such an entry is nobody's live trap;
/// * the trapping body the trampoline named
///   (`cratonvm_jit::deopt::peek_last_deopt_trapping_body`) was published under
///   a class this site can reach: the receiver's class or a supertype of it
///   for a virtual or interface site with a known receiver, the class the
///   instruction names or a supertype of it for a static or special one. A
///   body without that record, a site whose class does not resolve, or a
///   lambda site (the stash names the impl, not `info`'s method) passes.
///
/// JIT round 20 (lane stash): a receiver whose class the class store does not
/// hold passes too (fail-open: no supertype question can be answered for it).
/// That is every lambda proxy (a VM-internal id, `LAMBDA_PROXY_ID_BASE` and
/// up): a proxy whose metafactory bound an impl that carries the SAM's own
/// name (`task::run` as a `Runnable`, `Integer::compare` as a `Comparator`)
/// names this site's callee by the name arm, and a default method of the
/// functional interface (`andThen`, `reversed`) by name too; its trapping
/// body's class is the impl's or the interface's, which `is_subclass_of`
/// cannot relate to an id it does not hold. Refused, such an entry -- this
/// call's own trap -- travelled to the caller's door as a foreign stash (a
/// re-run from entry or an `InternalError`). The SAM-named impl is also
/// accepted by its exact identity (`lambda_site_resolves_to`).
///
/// `true` (claim as before) with the switch off.
fn stash_reachable_from_site(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    if !callee_service_stash_age_test_enabled() {
        return true;
    }
    if let Some(left_by) = cratonvm_jit::deopt::peek_last_deopt_left_by() {
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] callee service at {}.{}{} refuses an orphan left by {left_by}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return false;
    }
    if key_method != info.method_name {
        return true;
    }
    let Some(body_class) = cratonvm_jit::deopt::peek_last_deopt_trapping_body()
        .and_then(|body| body.published_class_id())
        .filter(|&id| id != 0)
        .map(ClassId::new)
    else {
        return true;
    };
    // JIT round 21 lane reflect4: the hidden-owner leaf's class by its id
    // (asked before the guard below, which it takes itself).
    let hidden_owner = jit_hidden_static_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let reachable = match info.invoke_kind {
        3 if hidden_owner.is_some() => hidden_owner == Some(body_class),
        0 | 2 if receiver_class_id.as_u32() != 0 => {
            receiver_class_id == body_class
                // JIT round 20 (lane stash): an id the store does not hold
                // (a lambda proxy) cannot be asked; fail-open.
                || cm.get_loader_id(receiver_class_id).is_none()
                || cm.is_subclass_of(receiver_class_id, body_class)
        }
        1 | 3 => {
            let site_class = if info.declaring_class_id == 0 {
                cm.find_bootstrap_class_by_name(info.class_name)
            } else {
                cm.find_cp_class_for_class(
                    info.class_name,
                    ClassId::new(info.declaring_class_id),
                )
            };
            site_class.is_none_or(|site| site == body_class || cm.is_subclass_of(site, body_class))
        }
        _ => true,
    };
    drop(cm);
    // The SAM-named lambda impl, by its exact identity (see the doc): asked
    // only on a refusal, after the class manager's read lock is released.
    let reachable = reachable
        || lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc);
    if !reachable && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] callee service at {}.{}{} refuses a stash whose trapping body's \
             class {} it cannot reach",
            info.class_name,
            info.method_name,
            info.descriptor,
            body_class.as_u32(),
        );
    }
    reachable
}

/// `CRATONVM_JIT_RESUME_HIDDEN_RECEIVER_CLASS` -- correctness, **default ON**,
/// `0` restores the refusal (JIT r23 wave 4b, lane L2): a call-site service
/// resumes a stash that names the receiver's own HIDDEN class by the
/// receiver's class id, since no loader resolves a hidden class's name
/// ([`try_resume_trapped_callee`]). Read on the cold resume path only.
fn resume_hidden_receiver_class_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_RESUME_HIDDEN_RECEIVER_CLASS")
}

/// `CRATONVM_DBG_FORCE_CALLEE_DECLINE=<Class.method>` (j27 wave 8, lane W8ARG;
/// debug, unset by default): [`try_resume_trapped_callee`] declines every
/// stash whose outermost identity (`pkg/Class.method:(desc)R`) contains the
/// value, as it declines a redefined callee. The decline arms are otherwise
/// reached only by accident (a redefinition, a frame lost to a collection
/// during the build): the owned-arguments hand-off, whose caller's caller
/// re-runs the caller from bci 0 (`FramelessTrapSite::Handoff`, the two-level
/// rule `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY` included), and the
/// copying site's `rerun_declined_callee_from_entry`. A sound VM prints the
/// same facts with and without it (`tools/probes/jit/j27/J27W8ArgDeclineRows.java`).
/// Scope it to the probe's own methods: a filter without a `.` declines
/// nothing, so `=1` is inert. Read on the cold resume path only, no static.
fn forced_callee_decline(stash_key: &str) -> bool {
    cratonvm_types::flags::runtime_var("CRATONVM_DBG_FORCE_CALLEE_DECLINE")
        .is_ok_and(|filter| forced_decline_filter_names(&filter, stash_key))
}

/// The pure half of [`forced_callee_decline`]: does `filter` name a method
/// (it contains a `.`) whose text `stash_key` contains?
fn forced_decline_filter_names(filter: &str, stash_key: &str) -> bool {
    let filter = filter.trim();
    filter.contains('.') && stash_key.contains(filter)
}

/// SAFETY: same contract as the surrounding dispatch helpers — `vm` live,
/// `info` a live `JitInvokeInfo`, `thread` the current thread's exclusive
/// borrow (passed in, NOT re-acquired via `jit_thread_mut`, because some call
/// sites — `jit_invoke_virtual_mic` — hold the thread guard for their whole
/// body and a nested acquire would alias the `&mut`).
unsafe fn try_resume_trapped_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<i64> {
    let trc = |why: &str, detail: &dyn std::fmt::Display| {
        if dbg_deopt_on() {
            eprintln!("[cratonvm-deopt] callee-resume refused ({why}): {detail}");
        }
    };
    // N2 — the OUTERMOST scope's identity, not the innermost.
    //
    // Every question this function asks below is about "the method this call
    // site invoked": does the stash name it, does its class resolve, is its bci
    // inside its code, which artifact's deopt points describe the trap. For a
    // frame with no caller chain — every frame in the tree today — that is the
    // stash's own `method_key` and the two peeks are the same call. For an
    // inlined chain it is the OUTERMOST scope: the innermost names a spliced
    // callee, which is never the name the call site was written with, so asking
    // the innermost would refuse every chain by construction and leave the
    // frame to travel outward as an orphan.
    let Some((key, bci)) = cratonvm_jit::deopt::peek_last_deopt_outermost_identity() else {
        return None;
    };
    // `OmitStackTraceInFastThrow`: does the body that trapped stand for C2
    // code? Read now, while the stash still names it; only a resumed frame at
    // the trapping bytecode of such a body is a C2 throw site
    // (`ResumedCompiledFrameScope::enter_at`).
    let trapped_c2_tier = crate::runtime::exceptions::omit_stack_trace_in_fast_throw(vm)
        && cratonvm_jit::deopt::peek_last_deopt_trapping_body()
            .is_some_and(|body| crate::runtime::exceptions::body_is_c2_equivalent(&body));
    if key.is_empty() || bci == u32::MAX {
        trc(
            "no usable stash identity",
            &format_args!("key={key:?} bci={bci}"),
        );
        return None;
    }
    let Some((rest, key_desc)) = key.rsplit_once(':') else {
        trc("unparseable stash key", &key);
        return None;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        trc("unparseable stash key", &key);
        return None;
    };
    // j27 wave 8 (lane W8ARG): the debug lever that drives the decline arms
    // (the owned-arguments hand-off's caller re-run, the copying site's
    // callee re-run) on purpose; see [`forced_callee_decline`].
    if forced_callee_decline(&key) {
        trc("forced by CRATONVM_DBG_FORCE_CALLEE_DECLINE", &key);
        return None;
    }
    // A redefine of this exact callee invalidates the stashed frame. An
    // unrelated agent transformation must not disable deopt resume globally.
    //
    // Interpreter round i1 wave 18, lane L3: except the exit the safepoint
    // slow path granted a body compiled AFTER every redefinition of this VM
    // (`interpreter::granted_exit_of_a_current_body`). That body ran the
    // class's current bytecode, which is what the frame is rebuilt from
    // below; refusing it re-ran the callee from entry
    // (`rerun_declined_callee_from_entry`) and replayed what it committed.
    // Round 12 wave 2 (lane rt): a body compiled from the current bytecode
    // resumes (`stashed_callee_predates_redefinition`).
    let trap_point = cratonvm_jit::deopt::peek_last_deopt_point();
    // Interpreter round i1 wave 46, lane L2: the exit the safepoint verdict
    // granted a method-entry body forced to leave as its class's obsolete
    // activation after a renumbering redefinition
    // (`cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`).
    // Its frame is rebuilt in the bytecode the body was compiled from and
    // restamped with that bytecode's pool generation below, so it is neither
    // refused as a frame of a redefined class nor rebuilt from the class's
    // current bytecode. A single frame only (such a body leaves only at its
    // own code), of a method that holds no method monitor.
    let obsolete = crate::runtime::interpreter::granted_obsolete_activation_source(
        vm,
        cratonvm_jit::deopt::peek_last_deopt_cause(),
        trap_point,
        key_class,
        key_method,
        key_desc,
    )
    .filter(|(source, _)| {
        // Shape 1 (b) of the i41 page: a synchronized one only when the
        // stash carries the method monitor its self-locking body kept, which
        // the rebuilt frame then owns (as for a current body, below).
        cratonvm_jit::deopt::peek_last_deopt_frame(|f| {
            f.caller_frames.is_empty()
                && (!source.is_synchronized
                    || (cratonvm_jit::INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE
                        && (crate::runtime::interpreter::frame_hands_over_method_monitor(
                            vm,
                            source.declaring_class_id,
                            true,
                            source.is_static,
                            &source.code,
                            f,
                        ) || crate::runtime::interpreter::caller_holds_method_monitor(
                            vm,
                            thread,
                            source.is_static,
                            source.declaring_class_id,
                            f,
                        ))))
        })
        .unwrap_or(false)
    });
    // Round 14 wave 2 (lane deopt; proposal R13RP6-1): a SUPERSEDED body of
    // the redefined callee resumes in the bytecode it was compiled from when
    // its trampoline stashed that (`callsite_trap_own_source`), instead of
    // re-running from entry in the new bytecode. `Some((source, cp stamp))`.
    let own_source = if obsolete.is_none()
        && stashed_callee_predates_redefinition(
            vm,
            info.declaring_class_id,
            key_class,
            key_method,
            key_desc,
            trap_point,
        )
        && !crate::runtime::interpreter::granted_exit_of_a_current_body(
            vm,
            cratonvm_jit::deopt::peek_last_deopt_cause(),
            trap_point,
        )
        && !stash_is_from_a_current_body(vm, info, key_class, key_method, key_desc)
    {
        let class_id = {
            let cm = vm.classes.class_manager.read();
            if info.declaring_class_id == 0 {
                cm.find_bootstrap_class_by_name(key_class)
            } else {
                cm.find_cp_class_for_class(key_class, ClassId::new(info.declaring_class_id))
            }
        };
        let own = class_id.and_then(|id| {
            crate::runtime::interpreter::callsite_trap_own_source(vm, id, key_method, key_desc)
        });
        if own.is_none() {
            trc("the stashed callee was redefined", &key);
            return None;
        }
        own
    } else {
        None
    };
    // The identity check, asked of the method the site RESOLVED to rather than
    // only of the name it was written with.
    //
    // For an ordinary call those are the same method, and `info` answers. For a
    // SAM call on a lambda they are NEVER the same: `info.method_name` is the
    // interface method (`apply`, `accept`, `test`) while the frame the compiled
    // body stashed names the synthetic impl (`lambda$static$0`). Comparing the
    // two therefore refused EVERY lambda callee's deopt, unconditionally and by
    // construction — the frame was dropped as an orphan and the impl was
    // re-entered from bci 0, re-running whatever it had already committed. See
    // `docs/feature-designs/deopt-osr.md`.
    //
    // The widening is exactly one alternative and it is not a relaxation: the
    // receiver must be a lambda proxy, its SAM must be the method this call
    // site names, and the impl the metafactory bound must be the stashed method
    // NAME FOR NAME. A foreign frame — the Groovy "duplicate `main`" shape that
    // forced the `5ceb880f` revert, and the reason this refusal is load-bearing
    // — satisfies none of those.
    if !(key_method == info.method_name
        && descriptors_match_modulo_return(key_desc, info.descriptor))
        && !lambda_site_resolves_to(vm, receiver_class_id, info, key_class, key_method, key_desc)
    {
        trc(
            "stash is not this call site's callee",
            &format_args!("stash={key} site={}{}", info.method_name, info.descriptor),
        );
        return None;
    }
    // HANDLER11-4 (JIT round 19 wave 12, lane handler12): and the body that
    // trapped must be one this site can reach (opt-in).
    if !stash_reachable_from_site(vm, info, receiver_class_id, key_class, key_method, key_desc) {
        trc(
            "stash's trapping body is not reachable from this call site",
            &format_args!(
                "stash={key} site={}.{}{}",
                info.class_name, info.method_name, info.descriptor
            ),
        );
        return None;
    }
    // A resume claimed through the lambda arm above is a lambda body's deopt
    // that did NOT re-run from entry, which is what the SAM door's own outcome
    // split counts. Record it there — the counter's question is "was a lambda
    // body's trapped frame spent or dropped", and the answer is the same
    // whichever door spent it. See `lambda_site_deopt_outcomes`.
    let via_lambda_identity = key_method != info.method_name;

    // Resolve the trapping method from ITS OWN declaring class (baked in the
    // key) — mirrors the `callee_compiler` resolution recipe.
    // Every refusal below is TRACED. They used to be bare `?` / `return None`
    // against a bound-but-unused `_resolve_trace`, so the five ways resolution
    // can decline were indistinguishable from "no stash" — and each one leaves
    // the frame stashed for an outer consumer that cannot attribute it, i.e.
    // each one MINTS an orphan. A refusal that cannot be named cannot be
    // counted, which is why the orphan in
    // `jit-direct-call-mints-an-orphaned-deopt-frame` was attributed to
    // inlining on no evidence.
    //
    // Wave 46 (lane L2): a renumbered obsolete activation's exit is rebuilt
    // in the bytecode its body was compiled from, not in the class's current
    // method, which a renumbering redefinition replaced.
    // R13RP6-1: an own-source resume already resolved the class and checked
    // the method against the body's own template.
    let cached = if let Some((source, _)) = &obsolete {
        std::sync::Arc::clone(source)
    } else if let Some((source, _)) = own_source.as_ref() {
        std::sync::Arc::clone(source)
    } else if let Some(template) = crate::runtime::interpreter::callsite_trap_current_template(
        vm, key_class, key_method, key_desc,
    ) {
        // Round 14 wave 3 (lane resume; proposal R14DP-1): the trampoline
        // named the trapping body, whose class was never redefined, so its
        // own template IS the method's current bytecode: no name resolution
        // through the call site's loader, no lock, no allocation.
        template
    } else {
        // JIT round 21 lane reflect4 (correctness, unconditional): the
        // hidden-owner leaf's callee (a spun `LambdaForm$MH`) by its id; by
        // name it never resolves, and the refusal left its frame an orphan.
        // Asked before the guard below, which `jit_substituted_owner` takes.
        let hidden_owner =
            jit_hidden_static_owner(vm, info).filter(|_| key_class == info.class_name);
        let cm = vm.classes.class_manager.read();
        let class_id = if let Some(owner) = hidden_owner {
            owner
        } else if info.declaring_class_id == 0 {
            match cm.find_bootstrap_class_by_name(key_class) {
                Some(id) => id,
                None => {
                    trc("stash class not found (bootstrap)", &key);
                    return None;
                }
            }
        } else {
            // The name is the declaring class of the method the call ran. For a
            // virtual or interface call that is the receiver's class or one of
            // its supertypes, which the RECEIVER's loader sees and the call
            // site's need not: a JDK `HashMap.hash` calling an application
            // class's `hashCode` is in the boot namespace, which cannot name
            // `org/springframework/core/ResolvableType`. The call site's loader
            // still answers first, so a static or special callee resolves as it
            // did.
            let from_site =
                cm.find_cp_class_for_class(key_class, ClassId::new(info.declaring_class_id));
            let resolved = from_site
                .or_else(|| {
                    (receiver_class_id.as_u32() != 0
                        && receiver_class_id.as_u32() != info.declaring_class_id)
                        .then(|| cm.find_class_by_name_for_class(key_class, receiver_class_id))
                        .flatten()
                })
                .or_else(|| {
                    // JIT r23 wave 4b (lane L2; `CRATONVM_JIT_RESUME_HIDDEN_RECEIVER_CLASS`,
                    // page `r23-w4b-hidden-class-callee-trap-resume-by-name-FIXED-20261021.md`):
                    // a HIDDEN class is named by no loader, so neither lookup above finds
                    // it. When the stash names the receiver's own hidden class (a JDK-spun
                    // lambda forwarder's `apply` that trapped at its receiver null guard),
                    // the receiver's class id IS the callee's class. Without this the
                    // resume was refused and the call raised `InternalError` instead of
                    // re-executing the trapping instruction.
                    (receiver_class_id.as_u32() != 0 && resume_hidden_receiver_class_enabled())
                        .then(|| cm.get_class(receiver_class_id))
                        .flatten()
                        .filter(|c| c.is_hidden() && *c.name == *key_class)
                        .map(|_| receiver_class_id)
                })
                .or_else(|| {
                    // A lambda proxy has no loader of its own to ask, and the call site (a JDK
                    // `Predicate.test` caller in the boot namespace) cannot name an application
                    // class: `java.util.function.Predicate.test` on JUnit's
                    // `ReflectionUtils.lambda$findAllMethodsInHierarchy$37` was refused here and
                    // raised `InternalError` (the 12 `testBeginHandshakeCloseOutbound` invocations of
                    // `JdkSslEngineTest`). The lambda's own impl handle names the class, so when the
                    // receiver is a proxy whose SAM impl is the stashed method and exactly one loader
                    // defines that name, that definition is the callee's.
                    let call_site = vm.classes.lambda_call_site_for(receiver_class_id)?;
                    let h = &call_site.impl_handle;
                    (&*h.class_name == key_class
                        && &*h.member_name == key_method
                        && &*h.descriptor == key_desc)
                        .then(|| cm.find_unique_class_by_name(key_class))
                        .flatten()
                });
            match resolved {
                Some(id) => id,
                None => {
                    trc(
                        "stash class not found from the call site's loader",
                        &format_args!("{key} from_class_id={}", info.declaring_class_id),
                    );
                    return None;
                }
            }
        };
        let store = cm.class_store();
        let Some((method, declaring_id)) =
            crate::classloading::find_method_recursive(class_id, key_method, key_desc, store)
        else {
            trc("stash method not found in its own class", &key);
            return None;
        };
        let Some(code_attr) = method.code() else {
            trc("stash method has no Code attribute", &key);
            return None;
        };
        let Some(declaring_class_name) = store.get(declaring_id).map(|c| &*c.name) else {
            trc("stash declaring class has no name", &key);
            return None;
        };
        // The key must name the method's OWN declaring class — a mismatch
        // means the name resolution drifted (e.g. class redefinition);
        // refuse rather than resume against different bytecode.
        if declaring_class_name != key_class {
            trc(
                "stash key names a different declaring class than resolution found",
                &format_args!("{key} resolved={declaring_class_name}"),
            );
            return None;
        }
        // Round 13 wave 9 (lane chain4): unless the stash carries the method
        // monitor its self-locking body kept at the guard, which the rebuilt
        // frame then owns as its `monitor_on_exit`
        // (`build_deopt_frame_or_refusal`); a frame this service cannot resume
        // is released as any compiled lock is (`CompiledLocksOfAStash`).
        //
        // Shape 2 (c) of the i41 page: or a caller-held synchronized CALL's
        // callee, whose monitor this thread holds for the caller; the rebuilt
        // frame then releases nothing of it and the caller's own edge does
        // (`caller_holds_method_monitor`).
        if method.is_synchronized()
            && !cratonvm_jit::deopt::peek_last_deopt_frame(|f| {
                crate::runtime::interpreter::frame_hands_over_method_monitor(
                    vm,
                    declaring_id,
                    true,
                    method.is_static(),
                    &code_attr.code,
                    f,
                ) || crate::runtime::interpreter::caller_holds_method_monitor(
                    vm,
                    thread,
                    method.is_static(),
                    declaring_id,
                    f,
                )
            })
            .unwrap_or(false)
        {
            trc("stash method is ACC_SYNCHRONIZED", &key);
            return None;
        }
        let num_params = crate::runtime::interpreter::count_method_params(key_desc);
        std::sync::Arc::new(
            crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: declaring_id,
                    class_name: std::sync::Arc::from(declaring_class_name),
                    method_name: std::sync::Arc::from(key_method),
                    method_descriptor: std::sync::Arc::from(key_desc),
                    source_file: store
                        .get(declaring_id)
                        .and_then(|c| c.source_file.as_deref())
                        .map(std::sync::Arc::from),
                    // Interned per method identity (no body copy per trap),
                    // as `resolve_callee_uncached` does.
                    code: crate::runtime::frame::padded_bytecode_for_method(
                        declaring_id,
                        key_method,
                        key_desc,
                        &code_attr.code,
                        &code_attr.exception_table,
                    ),
                    exception_table: std::sync::Arc::from(code_attr.exception_table.as_slice()),
                    max_stack: code_attr.max_stack,
                    max_locals: code_attr.max_locals,
                    num_params: num_params as u16,
                    is_synchronized: method.is_synchronized(),
                    is_static: method.is_static(),
                },
            ),
        )
    };
    // `cached.code` carries 2 bytes of speculative-read padding, which are not
    // an instruction: a bci landing on them must be refused like one past the
    // end (it compared against the padded length, and admitted both).
    let code_len = cached.code.len().saturating_sub(2);
    if bci as usize >= code_len {
        trc(
            "resume bci past the method's code",
            &format_args!("{key} bci={bci} len={code_len}"),
        );
        return None;
    }

    // Commit: take the stash. From here every path either resumes or restores
    // it so nothing is silently dropped.
    // Round 14 wave 3 (lane resume; R14DP-3): with the body's own template,
    // which the restash below puts back.
    // `mut`: gch h2/fj's restash arm writes the pinned words back into it.
    let (mut rframe, stashed_cause, stashed_point, stashed_source) =
        cratonvm_jit::deopt::take_last_deopt_with_source()?;

    // That take may have discarded older frames beneath this one. They are
    // stale leftovers, never a deeper callee of this frame: the ordinary stash
    // admits re-execute points only, so an invoke at `bci` has not run, and a
    // callee that returned the sentinel never let this frame reach a later
    // guard. So nothing is refused on that account — see
    // `cratonvm_jit::deopt::deopt_stash_nesting_counts`.

    // Read BEFORE the build, for the refusal arm below: may a refused frame go
    // back into the stash? The stash rooted its `Object` words; taken out of
    // it, nothing does, and the build can collect (the virtual-object
    // materialiser allocates, the pool refill can GC).
    let gc_count_before = vm.mem.heap.collection_count();
    // Interpreter round i1 wave 23, lane L2: the locks the callee's compiled
    // code took, pinned below `pin_base` so every `truncate(pin_base)` leaves
    // them. A resumed frame and a restashed one keep them; the frame dropped
    // after a collection is re-run from entry, so its locks are released there.
    let compiled_locks =
        crate::runtime::interpreter::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
    let pin_base = thread.native_pin_roots.len();
    // gch h2/fj (`gcd-d10f-proposal-restash-a-callee-frame-refused-after-a-collection`,
    // opt-in `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION`): root the taken frame's
    // own object words across the build, ABOVE `pin_base`, so every path that
    // truncates to `pin_base` (the resumed frame's push, the refusals) releases
    // them with the build's own pins. A single frame only: a chain's run keeps
    // what sits above `pin_base` until it returns.
    let restash_pinned = rframe.caller_frames.is_empty()
        && vm.jit.jit_cache.refused_resume_restashes_after_collection();
    let frame_pins = if restash_pinned {
        pin_taken_stash_frame_words(&mut thread.native_pin_roots, &rframe)
    } else {
        0
    };
    // N2 — the stash may name an inlined caller chain: the trap happened inside
    // a body this artifact spliced, so the callee's own frame sits beneath one
    // or more inlined scopes. `build_deopt_frame_inner` returns ONE frame by
    // value and cannot express that, so a chain is materialised through its own
    // entry point and run through its own tail below. Both are built before
    // anything is pushed or de-speculated, which keeps this function's
    // fail-before-commit discipline: a refusal here restores the stash and
    // leaves the caller exactly where it was.
    //
    // NOT LIVE TODAY. `jit/src/x64/inlining.rs`'s splice postcondition rolls
    // back any splice that published a deopt point, so no artifact carries a
    // chain; this is the consumer half landing first, so that relaxing the
    // postcondition does not hand a chain to a sink that raises
    // `InternalError: precise deoptimization unavailable`. See N2/N5 in
    // `NOTES-deopt.md`.
    enum Rebuilt {
        Single(crate::runtime::frame::Frame),
        /// Outermost first. Opaque: only
        /// `run_deopt_frame_chain_to_completion` may spend it.
        Chain(crate::runtime::interpreter::DeoptFrameChain),
    }
    let rebuilt = if rframe.caller_frames.is_empty() {
        match crate::runtime::interpreter::build_deopt_frame_or_refusal(
            vm, thread, &cached, &rframe, false, true,
        ) {
            Ok(frame) => Some(Rebuilt::Single(frame)),
            // Round 12 wave 5 (lane replay3): the callee's scalar-replaced
            // objects did not fit a full heap. HotSpot pops the callee and
            // throws `OutOfMemoryError` at this call; restashing or dropping
            // the frame here instead re-ran the callee from entry
            // (`service_frameless_callee_trap`), committing again what it had
            // committed. The build released the callee's compiled locks, so
            // `compiled_locks` (pinned below `pin_base`) only drops its pins.
            Err(why) if crate::runtime::interpreter::frame_build_heap_failure_is_answered(why) => {
                thread.native_pin_roots.truncate(pin_base);
                compiled_locks.unpin_keeping_the_locks(thread);
                trc(
                    "re-materialisation exhausted the heap; OutOfMemoryError at the call",
                    &format_args!("{key} bci={bci}"),
                );
                let _ = take_jit_deopt_pending();
                crate::runtime::interpreter::despeculate_stashed_frame_method_with_cause(
                    vm,
                    &rframe,
                    stashed_cause,
                    stashed_point,
                );
                let oom = crate::runtime::interpreter::rematerialisation_out_of_memory(vm, thread);
                return Some(handle_jit_dispatch_error(vm, thread, oom, info));
            }
            Err(_) => None,
        }
    } else {
        // Round 13 wave 8 (lane chain3): by the trapping artifact's scope
        // class ids, found through the stashed point, with its redefinition
        // checks (`r13w8-chain3-helpers-callsite-chain-by-point-patch`).
        crate::runtime::interpreter::build_deopt_frame_chain_for_point(
            vm,
            &cached,
            &rframe,
            stashed_point,
        )
        .map(Rebuilt::Chain)
    };
    let Some(rebuilt) = rebuilt else {
        // gch h2/fj (d10f restash): a collection ran during the build, so the
        // frame's words may name from-space; read them back from their pins
        // (rewritten in place by the collector) BEFORE the truncate below.
        let collected = vm.mem.heap.collection_count() != gc_count_before;
        let refreshed = collected
            && restash_pinned
            && refresh_taken_stash_frame_words(
                &thread.native_pin_roots,
                pin_base,
                frame_pins,
                &mut rframe,
            );
        // Unmappable — release partial pins, restore the stash, and let
        // the sentinel propagate to the outer (identity-checked) sinks.
        thread.native_pin_roots.truncate(pin_base);
        trc("frame not materialisable", &format_args!("{key} bci={bci}"));
        // EXCEPT when a collection ran during the build
        // (r11-tier-deopt-resume-gc-windows, helpers half). Every refusal that
        // does not depend on an allocation is asked before the materialiser
        // and the pool refill, so what is left is an allocation that FAILED
        // after shells were allocated (OOM, which collects before it fails)
        // and the unreachable post-refill `StackPush`. Then `rframe`'s
        // `Object` words may name from-space, and restashing them hands the
        // next sink stale oops. Drop the frame instead: the sentinel
        // propagates without it and the caller takes the re-run. A refusal
        // with no collection in between restashes exactly as before, so the
        // outer sinks still see (and charge) an ordinary unmappable frame.
        // gch h2/fj: unless the frame's words were pinned across the build
        // and read back above (`refreshed`): then it is current and restashed
        // as after a refusal with no collection.
        if collected && !refreshed {
            trc(
                "refused after a collection; the frame is dropped, not restashed",
                &format_args!("{key} bci={bci}"),
            );
            // The frame is gone and the callee re-runs from entry: release
            // what its compiled code locked, through the pins (the collection
            // may have moved the objects).
            compiled_locks.release_for_a_rerun(vm, thread);
            // Leave a FRAMELESS trap behind, not a bare sentinel. A trampoline
            // stash (`x64_deopt_entry`, `ir_deopt_entry`) raises no deopt flag,
            // so with the frame gone nothing would mark the `i64::MIN` as a
            // trap: `jit_dispatch_threw` would answer 0 and a `J`/`D` call site
            // would keep it as a genuine `Long.MIN_VALUE` (round 11 wave 12,
            // the residual of r11w12-tier-helpers-declined-callee-rerun-patch).
            // With the flag, the dispatch door's frameless arm re-runs the
            // callee and every other sink treats the sentinel as a deopt.
            //
            // Round 13 wave 8 (lane replay4): judged here, while the frame's
            // bci still exists, so the service's replay verdict is the
            // prefix rule and not the whole-body one (which would refuse a
            // re-run that commits nothing twice). The verdict reads no
            // object word of the frame.
            // R13RP6-1: an own-source frame's bci is in the OLD bytecode, and
            // its re-run runs the new one: never exact.
            let exact = own_source.is_none()
                && callee_rerun_replay_is_exact(
                    vm,
                    info.declaring_class_id,
                    key_class,
                    key_method,
                    key_desc,
                    Some((&rframe, stashed_point)),
                )
                .unwrap_or(false);
            set_jit_deopt_pending();
            note_dropped_callee_frame(exact);
            return None;
        }
        if refreshed {
            // The positive control of `CRATONVM_DEOPT_RESTASH_AFTER_COLLECTION`.
            trc(
                "refused after a collection; restashed at its pinned addresses",
                &format_args!("{key} bci={bci} words={frame_pins}"),
            );
        }
        // Back into the stash, whose next sink decides about the locks.
        compiled_locks.unpin_keeping_the_locks(thread);
        // R14DP-3: the next sink keeps the name of the body that trapped
        // (`CRATONVM_DEOPT_RESTASH_KEEPS_SOURCE=0` drops it, as before).
        let stashed_source = stashed_source.filter(|_| {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_RESTASH_KEEPS_SOURCE")
        });
        cratonvm_jit::deopt::restash_last_deopt_with_source(
            rframe,
            stashed_cause,
            stashed_point,
            stashed_source,
        );
        return None;
    };

    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] helper precise-resume of trapped callee {key} at bci={bci}{}",
            if own_source.is_some() {
                " (in the bytecode its superseded body was compiled from)"
            } else {
                ""
            }
        );
    }
    if via_lambda_identity {
        crate::runtime::interpreter::lambda_site_bump_resumed();
    }
    // Committed: this trap is serviced here, so the deopt flag a
    // frame-stashing `jit_uncommon_trap` raises beside its stash is retired
    // with it. Left set, it outlives the call — the compiled caller continues
    // on the resumed value and a later legitimate `Long.MIN_VALUE` return reads
    // it as a sentinel (see `route_implicit_exc_through_callee`'s serviced
    // arm). A throw out of the resumed frame parks its own pending exception.
    let _ = take_jit_deopt_pending();

    // De-speculate the trapping method FIRST (record + evict + escalate), so
    // repeated traps stop re-entering the artifact. The policy — and the
    // reason it is not a plain `deoptimize` call — lives in
    // `despeculate_trapped_method`, which the tier-up sink in
    // `interpreter.rs` asks too so the two cannot answer differently.
    //
    // N3(b): the fallback reason used to be a hard-coded `UnreachedCode`, on
    // the SUCCESS path — this is the precise resume of a trapped callee that
    // the whole machinery above exists to produce. `UnreachedCode` is the one
    // reason `recommend_action` answers `MakeNotCompilable` on the FIRST
    // occurrence, so a single trap of any kind permanently retired the callee,
    // whatever actually trapped. The real reason is on the
    // `DeoptimizationPoint` the stub baked; read it from the callee's own
    // artifact, and reserve `UnreachedCode` for genuinely unreachable code.
    // The frame-aware rule only for a single frame: a chain's `rframe.bci` is
    // the INNERMOST scope's, while `bci` and the artifact are the outermost's.
    // The point that fired names the artifact that trapped. When the cache
    // publishes a DIFFERENT body for this method, the trap came out of a body
    // an earlier trap already retired (a stale call-site cache kept calling
    // it). Charging it would evict the fresh body BY NAME and count the one
    // speculation failure again, N callers N times -- the helper-sink half of
    // r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff,
    // `trapping_artifact_is_superseded`'s rule. An empty slot and an unknown
    // point (0) keep the old charge. The resume below still runs.
    // R13RP6-1: an own-source resume is by construction a superseded body's
    // trap (compiled before the redefinition), whatever the slot holds now.
    // Interpreter round i2 wave 61, lane L2
    // (`i59-L2-proposal-every-stash-names-its-trapping-body`): the published
    // body is compared with the one the trampoline named, by POINTER, when
    // the stash named one; by the point's address otherwise.
    //
    // One method, two cache keys: an inherited method compiled for a receiver
    // class is published under THAT class's name (`ISO_8859_1$Decoder.decode`
    // beside `CharsetDecoder.decode`, both the one bytecode), and the key this
    // sink read from the stash is the declaring class's. Asked of the declaring
    // key alone, the receiver-keyed body is "a different body than the
    // published one" for as long as the process lives -- never charged, so
    // never evicted, and (published) never made not entrant: `TestMethodPerformance`'s
    // `MessageBytes` loop resumed `CharsetDecoder.decode(BB,CB,Z)` at bci 54 in
    // the interpreter on about three iterations in four. A body the stash
    // named, that its OWN key still publishes, is the live body of that key,
    // not a superseded one. Its trap is charged under the declaring class's
    // key (the one the log, the tiered manager and a recompile's de-spec read),
    // with the reason read from THAT body, and the body is evicted under its own
    // key.
    // j27 wave 7 (lane T1LAM3): the named body and the cache's handle are
    // `RetainedCode`s (`taken_trapping_body_retained`): the stash's `Weak`
    // upgrade may be the body's last owner.
    let sibling_key: Option<(std::sync::Arc<str>, ClassId, cratonvm_jit::RetainedCode)> =
        (stashed_point != 0
            && own_source.is_none()
            && cratonvm_jit::deopt::stash_names_trapping_body_enabled())
        .then(|| stale_trap_not_entrant::taken_trapping_body_retained(stashed_point))
        .flatten()
        .and_then(|named| {
            let (class, method, descriptor, class_id) = named.published_key()?;
            if &*method != key_method || &*descriptor != key_desc {
                return None;
            }
            if &*class == key_class && class_id == cached.declaring_class_id.as_u32() {
                return None;
            }
            let held = vm
                .jit
                .jit_cache
                .read()
                .get(&class, &method, &descriptor, ClassId::new(class_id))
                .map(cratonvm_jit::RetainedCode::new)?;
            std::sync::Arc::ptr_eq(held.arc(), named.arc())
                .then(|| (class, ClassId::new(class_id), named))
        });
    if let Some((class, _, _)) = &sibling_key {
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] {key}: the trapping body is published under {class}, not under \
                 the declaring class's key: charged once under the declaring key, evicted under its own"
            );
        }
    }
    let trapped_a_superseded_body = own_source.is_some()
        || (stashed_point != 0
            && vm
                .jit
                .jit_cache
                .read()
                .get(key_class, key_method, key_desc, cached.declaring_class_id)
                // j27 wave 7 (lane T1LAM3): both handles are `RetainedCode`s.
                .map(cratonvm_jit::RetainedCode::new)
                .is_some_and(|live| {
                    let by_address = !live.owns_deopt_point(stashed_point);
                    let named = cratonvm_jit::deopt::stash_names_trapping_body_enabled()
                        .then(|| stale_trap_not_entrant::taken_trapping_body_retained(stashed_point))
                        .flatten();
                    match named {
                        Some(named) => {
                            // A sibling (published under its own key) is not
                            // superseded: it is charged below.
                            let superseded = !std::sync::Arc::ptr_eq(live.arc(), named.arc())
                                && sibling_key.is_none();
                            if dbg_deopt_on() {
                                eprintln!(
                                    "[cratonvm-deopt] callee trap body named by the stash \
                                     (superseded): {key} superseded={superseded} \
                                     by-address={by_address} agrees={} named@{:p} \
                                     not-entrant={} live@{:p} not-entrant={} chain-frames={} fired=[{}]",
                                    superseded == by_address,
                                    named.entry_ptr(),
                                    named.is_not_entrant(),
                                    live.entry_ptr(),
                                    live.is_not_entrant(),
                                    rframe.caller_frames.len(),
                                    named
                                        ._deopt_point_boxes
                                        .iter()
                                        .map(|b| &**b)
                                        .chain(named.deopt_points.iter())
                                        .find(|p| *p as *const cratonvm_jit::deopt::DeoptimizationPoint as usize == stashed_point)
                                        .map(|p| format!("reason={:?} action={:?} spec={} bci={} sem={:?}", p.reason, p.action, p.speculation_id, p.bci, p.semantics))
                                        .unwrap_or_default()
                                );
                            }
                            superseded
                        }
                        None => by_address,
                    }
                }));
    if trapped_a_superseded_body && dbg_deopt_on() {
        eprintln!("[cratonvm-deopt] {key} bci={bci}: trap from a superseded body, not charged");
    }
    // j27 wave 7 (lane T1LAM3, t1-X2 page; `CRATONVM_JIT_STALE_TRAP_NOT_ENTRANT`,
    // opt-in): a baked call still enters that stale body, so make IT not
    // entrant, as an eviction does (`stale_trap_not_entrant`).
    if trapped_a_superseded_body && stale_trap_not_entrant::enabled() {
        if let Some(stale) = stale_trap_not_entrant::body_to_patch(
            vm,
            stashed_point,
            own_source.is_some(),
            sibling_key.is_some(),
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
        ) {
            if dbg_deopt_on() {
                eprintln!(
                    "[cratonvm-deopt] stale-body-trap {key} bci={bci}: body id={} made not entrant",
                    stale.compile_id
                );
            }
            make_trap_evicted_body_not_entrant(vm, &key.to_string(), stale);
        }
    }
    // Round 16 wave 15 (lane trapfix; FS14-4): an empty slot whose method an
    // earlier trap already declared not compilable is charged ONCE, by that
    // earlier trap. A later trap can only come out of the evicted body
    // through a binding the eviction did not withdraw; charging it again
    // re-ran the whole `MakeNotCompilable` decision per failing call.
    let trapped_a_superseded_body = trapped_a_superseded_body
        || trap_from_a_body_already_declared_not_compilable(
            vm,
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
            bci,
        );
    // Interpreter round i1 wave 15, lane L3: a callee that left at a
    // back-edge poll because the method must now run interpreted is resumed
    // like any trap, and not charged. Asked of the stash's own cause, so an
    // artifact the cache no longer holds (whose reason
    // `trapped_callee_deopt_reason` could not read) is not charged as an
    // `UncommonTrap` either.
    let left_for_the_interpreter = stashed_cause.is_some_and(|c| {
        crate::runtime::interpreter::exit_left_for_the_interpreter(
            vm,
            c.reason,
            cached.declaring_class_id,
            key_method,
            key_desc,
        )
    });
    // Round 15 wave 4 (lane chain5): a forced chain exit of
    // `CRATONVM_DEOPT_EAGER_CHAINS` (claimed by the chain's build,
    // `build_deopt_frame_chain_for_point`) is a passing guard, not charged
    // under `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`.
    let uncharged_forced = !trapped_a_superseded_body
        && !left_for_the_interpreter
        && crate::runtime::interpreter::leave_forced_trip_uncharged(
            vm,
            matches!(&rebuilt, Rebuilt::Chain(chain) if chain.is_forced_trip()),
        );
    // JIT round 19 wave 4 (lane splice4, SPLICE3-2,
    // `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS`): a marked
    // expected-throw trip of the live body is resumed below and not charged,
    // as the door sink leaves it (`deopt_resume::stash_sink_expected_throw_uncharged`;
    // the live body is the one that fired: a superseded one is not charged
    // anyway). Off: `false`, today's charge.
    let expected_throw = !trapped_a_superseded_body
        && !left_for_the_interpreter
        && !uncharged_forced
        && cratonvm_jit::ir::ir_splice_expected_throw_stash_sinks_enabled()
        && {
            let live = vm.jit.jit_cache.read().get(
                key_class,
                key_method,
                key_desc,
                cached.declaring_class_id,
            );
            crate::runtime::interpreter::stash_sink_expected_throw_uncharged(
                live.as_deref(),
                &rframe,
            )
        };
    if expected_throw && dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] {key} bci={bci}: expected throw in a spliced {} bci={} \
             (helper sink, CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS), not charged",
            rframe.method_key, rframe.bci
        );
    }
    if !trapped_a_superseded_body
        && !left_for_the_interpreter
        && !uncharged_forced
        && !expected_throw
    {
        let frame_for_reason = rframe
            .caller_frames
            .is_empty()
            .then_some((&cached.code[..], &rframe));
        // Round nio21 wave 7 (lane L2,
        // `r21-callsite-service-chain-trap-despec-grain-differs-from-the-doors-OBSOLETE-20261012.md`):
        // an inlined chain's trap is charged as the doors charge it
        // (`deopt_resume::stash_charge_site` / `stash_trap_attribution`):
        // at the call site `bci` in this method, under the STASH's own reason
        // (else `UncommonTrap`), de-spec'd there with the wildcard. The
        // artifact's points at `bci` describe the call site, not the spliced
        // guard that fired: an optimizing body publishes a plain
        // `TransferToInterpreter` resume point there, whose `Reinterpret`
        // never evicted the body, and whose derived id withdrew nothing the
        // splice planner asks about. Default ON
        // (`CRATONVM_JIT_CALLSITE_CHAIN_TRAP_CHARGE=0` turns it off).
        //
        // JIT round 19 wave 10, lane deopt10 (`i62-L2-proposal-single-pass-
        // chain-points-leave-the-roots-bci-key-space`, opt-in
        // `CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS`) charges the same reason
        // independently; with the nio21 switch off it still applies. Off both:
        // as before.
        let chain_charge = chain_trap_charge(&rframe, stashed_cause);
        let trap_reason = match chain_charge {
            Some(c) => c.reason,
            None if frame_for_reason.is_none()
                && cratonvm_jit::osr_exit::chain_points_off_root_bcis_enabled() =>
            {
                chain_trap_charge_reason(stashed_cause)
            }
            None => trapped_callee_deopt_reason(
                vm,
                cached.declaring_class_id,
                key_class,
                key_method,
                key_desc,
                bci,
                frame_for_reason,
                stashed_cause,
                stashed_point,
                sibling_key.as_ref().map(|(_, _, body)| body.clone()),
            ),
        };
        // nio21 wave 20, lane L4 (`CRATONVM_DEOPT_CAUSE_FROM_GUARD_BOXES`): a
        // single-frame trap attributed to the table's uncharged
        // `TransferToInterpreter` resume point while the point that FIRED (by
        // its stashed address, in the live body the charge is about) is a
        // guard with its own reason is charged under that reason. The w19b
        // rule covers a stash that carries its cause; this one covers a stash
        // that carries only the point.
        let trap_reason = if chain_charge.is_none() && stashed_point != 0 {
            let live = vm.jit.jit_cache.read().get(
                key_class,
                key_method,
                key_desc,
                cached.declaring_class_id,
            );
            crate::runtime::interpreter::nio21_charge_reason_from_fired_point(
                trap_reason,
                live.as_deref().and_then(|l| {
                    crate::runtime::interpreter::nio21_fired_point_reason(l, stashed_point)
                }),
            )
        } else {
            trap_reason
        };
        // Charged ONCE, under the declaring class's name: that is the key the
        // deoptimization log, the tiered manager and the optimizing compile's
        // de-spec registry (`despec_method_key`) read, so the per-bci
        // withdrawal reaches the next body and the per-method trap budget moves
        // by one trap per trap. Recorded under the receiver class's name only,
        // the withdrawal was never seen and the next body carried the same
        // failing guard; under both names the budget ran out twice as fast.
        // Wave 4 (lane splice4, SPLICE3-2): the frame-aware form, with the
        // nio21 wave 7 chain grain; both switches off, exactly
        // `despeculate_trapped_method`.
        // JIT round 21 wave 1 (lane loops, LOOPS21-2,
        // `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`, opt-in): a loop-entry
        // predicate's trap is withdrawn under its planter's own id (the
        // point's, `predicate_trap_charge`), not the wildcard this sink
        // records for an unnamed `UncommonTrap`. Off: `chain_charge`, as before.
        let despec_as = chain_charge
            .or_else(|| predicate_trap_charge(&rframe, bci, stashed_cause, trap_reason));
        despeculate_trapped_method_of_frame_despec_as(
            vm,
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
            trap_reason,
            bci,
            &rframe,
            despec_as,
        );
        // The eviction, though, is the sibling key's: the controller above
        // removed whatever the DECLARING key publishes, and the body that
        // trapped is published under its own. Withdrawn as the controller
        // withdraws a body (removed, then made not entrant), under the
        // controller's own predicate, and only while the key still publishes
        // that very body.
        if let Some((sibling_class, sibling_id, sibling_body)) = &sibling_key {
            if cratonvm_jit::tiered::deopt_evicts_method_body(
                trap_reason,
                cratonvm_jit::deopt::DeoptAction::Reinterpret,
            ) {
                let evicted = {
                    let mut jit_cache = vm.jit.jit_cache.write();
                    let held = jit_cache.get(sibling_class, key_method, key_desc, *sibling_id);
                    match held {
                        Some(held) if std::sync::Arc::ptr_eq(&held, sibling_body.arc()) => {
                            jit_cache.remove(sibling_class, key_method, key_desc, *sibling_id);
                            Some(held)
                        }
                        other => {
                            // Not the body that trapped: left in place.
                            if let Some(other) = other {
                                drop(cratonvm_jit::RetainedCode::new(other));
                            }
                            None
                        }
                    }
                };
                if let Some(evicted) = evicted {
                    if trap_eviction_not_entrant_enabled() {
                        make_trap_evicted_body_not_entrant(vm, &key.to_string(), evicted);
                    } else {
                        drop(cratonvm_jit::RetainedCode::new(evicted));
                    }
                }
            }
        }
        // r9w10 exc10: a zero divisor that keeps exiting the optimizing tier.
        // JIT round 19 wave 12, lane tail12 (TAIL11-1, opt-in
        // `CRATONVM_JIT_DIV_GUARD_RETIER_CHAINS`): for an inlined CHAIN the
        // division is the TRAPPING scope's (`bci` is the outer invoke), and
        // its reason is the stash's own cause (the outer artifact's points at
        // the invoke describe the call site). Off: the outer invoke's opcode,
        // which is never a division, so a spliced division never re-tiered.
        let (retier_reason, retier_opcode) = match &rebuilt {
            Rebuilt::Chain(chain) if div_guard_retier_chains_enabled() => (
                chain_trap_charge_reason(stashed_cause),
                chain.trapping_scope_opcode(),
            ),
            _ => (trap_reason, cached.code.get(bci as usize).copied()),
        };
        retier_hot_division_guard_trap(
            vm,
            key_class,
            key_method,
            key_desc,
            cached.declaring_class_id,
            retier_reason,
            retier_opcode,
            bci,
        );
    }

    // Run the reconstructed frame to completion. A single frame's pins are
    // released as soon as it is pushed (gen r5w4/jit8,
    // `execute_resumed_frame_releasing_pins`): they used to stay installed for
    // the whole run, "over-rooting is harmless" -- harmless for safety, but it
    // kept every reference the trapped frame held at its deopt point alive
    // until the resumed method returned, whatever it dropped meanwhile. The
    // truncate after the match stays: it releases what a chain's path or a
    // leaked nested scope left above `pin_base`.
    //
    // For a chain the same thing happens one level up: every frame is pushed
    // outermost-first and the whole chain runs until the OUTERMOST frame — the
    // callee this dispatch helper actually invoked — returns. That is the value
    // the compiled caller is waiting for. See
    // `run_deopt_frame_chain_to_completion` for why the depth it measures
    // against cannot be `execute_prebuilt_frame`'s.
    let res = match rebuilt {
        Rebuilt::Single(frame) => {
            // r9w9 exc9: the resumed frame IS the trapped compiled callee, so an
            // implicit exception its own re-executed bytecode raises (the zero
            // divisor that trapped it) is a compiled-code throw for
            // `OmitStackTraceInFastThrow` (opt-in, default off). The frame will
            // sit at `len() + 1` once pushed, at `bci`, the trapping bytecode
            // (a single frame: the outermost identity is the innermost).
            let _fast_throw_scope = crate::runtime::exceptions::ResumedCompiledFrameScope::enter_at(
                thread.frames.len() + 1,
                bci,
                trapped_c2_tier,
            );
            // No MethodEntry: the callee was entered in compiled code.
            match (&obsolete, own_source.as_ref()) {
                // Wave 46 (lane L2): restamped with the body's pool generation
                // after the push and before its first bytecode, then moved onto
                // its translated body, as the interpreter doors' sink does.
                (Some((_, stamp)), _) => {
                    if dbg_deopt_on() {
                        eprintln!(
                            "[cratonvm-deopt] {key} bci={bci}: renumbered obsolete activation \
                             resumed by the call-site service in the bytecode its body was \
                             compiled from"
                        );
                    }
                    crate::runtime::interpreter::execute_rebuilt_frame_releasing_pins(
                        vm,
                        thread,
                        frame,
                        pin_base,
                        Some(*stamp),
                    )
                }
                // R13RP6-1: restamped with the body's pool generation.
                (None, Some(&(_, cp_stamp))) => {
                    crate::runtime::interpreter::execute_own_source_frame_releasing_pins(
                        vm, thread, frame, pin_base, cp_stamp,
                    )
                }
                (None, None) => crate::runtime::interpreter::execute_resumed_frame_releasing_pins(
                    vm, thread, frame, pin_base,
                ),
            }
        }
        Rebuilt::Chain(chain) => {
            let depth = chain.frame_count();
            // gcd d1/b: the chain's pins end at its push too, as the single
            // frame's do (`run_deopt_frame_chain_to_completion_releasing_pins`).
            match crate::runtime::interpreter::run_deopt_frame_chain_to_completion_releasing_pins(
                vm, thread, chain, pin_base,
            ) {
                Some(res) => res,
                None => {
                    // `push_inlined_chain` only fails on an operand-stack push
                    // that materialisation already bounded against the frame's
                    // own `max_stack`, so this is unreachable by construction —
                    // but if it ever happens, some frames of the chain ARE
                    // already on the stack, so "restore the stash and let the
                    // caller re-run" would run them twice. REFUSE loudly
                    // instead: the interpreter unwinds the pushed frames the
                    // way it unwinds any error, and no side effect is replayed.
                    thread.native_pin_roots.truncate(pin_base);
                    compiled_locks.unpin_keeping_the_locks(thread);
                    return Some(handle_jit_dispatch_error(
                        vm,
                        thread,
                        crate::error::MethodCallFailed::InternalError(
                            crate::error::VmError::Internal {
                                message: format!(
                                    "deopt resume could not push a {depth}-frame inlined chain \
                                     for {key} at bci={bci}; refusing to re-execute the invoke"
                                ),
                            },
                        ),
                        info,
                    ));
                }
            }
        }
    };
    thread.native_pin_roots.truncate(pin_base);
    // The resumed frame held the locks and released them itself.
    compiled_locks.unpin_keeping_the_locks(thread);

    Some(match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    })
}

/// Inline capacity shared with `safe_native_call_impl`. It covers receiver
/// plus the full x64 register envelope and keeps ordinary JIT-to-native
/// dispatch free of temporary heap allocations.
pub(crate) const INLINE_JIT_NATIVE_ARGS: usize = 8;
type JitDecodedArgs = smallvec::SmallVec<[Value; INLINE_JIT_NATIVE_ARGS]>;

/// Decode a JIT dispatch helper's raw `i64` argument slice into the
/// `Value` slice the interpreter expects. Centralised so that the slow
/// path in `jit_invoke_dispatch` and the three `try_call_compiled_entry`
/// overflow bailouts (DISPATCH_CACHE hit, JIT-cache hit, post-compile)
/// all reconstruct args the same way — round-5 CRIT-1 fix.
///
/// `invoke_kind` matches the JIT calling-convention encoding: 0/1/2 are
/// virtual/static/special with an explicit receiver in `args_slice[0]`;
/// 3 is the no-receiver form (used for static and indy callees that the
/// JIT emits without a leading `this` slot).
///
/// SAFETY: `args_slice` must be a slice of valid `i64` arg slots produced
/// by the JIT caller. `vm` must be a live `SharedVm`; the heap is queried
/// to validate any potential object pointers before round-tripping them
/// through `ObjectRef`.
#[inline]
/// Does a call site of this [`JitInvokeInfo::invoke_kind`] pass a RECEIVER in
/// argument slot 0?
///
/// `0` virtual, `1` special, `2` interface, `3` static, `4` self-recursive
/// static DIRECT call. Slots 0–2 carry a receiver; **3 and 4 do not** — kind 4
/// is a *static* call, the IR backend's marker for "emit a raw `CALL` to this
/// method's own entry" rather than a different dispatch shape.
///
/// # Why this is a named predicate and not `!= 3`
///
/// It was `!= 3`, written when 4 did not exist, and
/// [`JitInvokeInfo::invoke_kind`]'s own doc explains why that looked safe:
/// "such a call NEVER reaches the dispatch helper, so the helper need not
/// handle 4". True of `jit_invoke_dispatch`. Not true of the DEOPT-SERVICING
/// door — `jit_service_callee_deopt` → `handle_compiled_callee_deopt_sentinel`
/// → `try_run_callee_handler` → [`decode_dispatch_values`] — which a direct
/// self-`CALL` reaches on every sentinel return, and which rebuilds the
/// callee's interpreter frame from these very values.
///
/// So kind 4 took the receiver branch: argument 0 (an ordinary `int`, `long`,
/// whatever the method's first parameter is) was consumed as `this`,
/// `is_object_address` rejected it, and it became `Value::Object(None)` —
/// **and every remaining argument shifted down one slot**. The rebuilt handler
/// frame's local 0 was then a null reference instead of the method's first
/// parameter.
///
/// MEASURED on `dev` 7281ce4b6, a self-recursive `static long f(int)` whose own
/// body catches what its recursive call throws implicitly, reading the result
/// from INSIDE the compiled caller (10,000 calls each):
///
/// | bottom-of-recursion trap | `--nojit` / `C2_ACCEPT=never` | default | `always` |
/// | --- | --- | --- | --- |
/// | `NUL.hashCode()` (NPE)      | 0 wrong | **9,941** | **9,944** |
/// | `EMPTY[3]` (AIOOBE)         | 0 wrong | **9,940** | **9,944** |
/// | `1 / 0` (ArithmeticException) | 0 wrong | 17–226 | ~228 |
/// | `throw new IllegalStateException` (control) | 0 wrong | 0 | 0 |
///
/// The handler returned `-0` where it owed `-1`: `catch (…) { return -n; }`
/// read `n` out of a local the decoder had replaced with null. The explicit
/// `throw` row is the control — it never reaches this decoder — and the two
/// interpreted arms are the oracle.
///
/// Every OTHER `invoke_kind` test in this file already spells the set it
/// means (`matches!(info.invoke_kind, 0 | 2)` for receiver-CLASS resolution,
/// `matches!(info.invoke_kind, 1 | 3)` for statically bound), which is why
/// this one is a predicate now rather than a fourth spelling.
#[inline]
fn invoke_site_has_receiver(invoke_kind: u8) -> bool {
    matches!(invoke_kind, 0 | 1 | 2)
}

/// Does the compact argument buffer of `info`'s site carry a receiver in word
/// 0? [`invoke_site_has_receiver`] for kinds 0-3; for the IR's direct
/// self-call row (kind 4) the row's own shape: a static self-call has none,
/// and a receiver-carrying one (JIT round 23 wave 3d,
/// `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`; the VM is normally handed
/// its kind-1 twin) has one exactly when `num_jit_args` is one more than the
/// descriptor's argument count (`cratonvm_jit::self_call_info_has_receiver`).
/// The one rule the reference walk ([`for_each_jit_reference_arg`]) and the
/// argument decoder ([`decode_dispatch_values_into_shaped`]) share.
#[inline]
fn jit_info_has_receiver(info: &JitInvokeInfo) -> bool {
    invoke_site_has_receiver(info.invoke_kind) || cratonvm_jit::self_call_info_has_receiver(info)
}

unsafe fn decode_dispatch_values(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
) -> JitDecodedArgs {
    let mut values = JitDecodedArgs::new();
    decode_dispatch_values_into(vm, info, args_slice, None, &mut values);
    values
}

/// [`decode_dispatch_values`] writing into a caller-owned buffer, with an
/// optional already-resolved receiver.
///
/// Two costs the by-value form carried, both measured in
/// `jit_native_dispatch_profile`:
///
///  * **`SmallVec::with_capacity`.** `JitDecodedArgs` is a
///    `SmallVec<[Value; 8]>` — eight 16-byte `Value`s plus a capacity word,
///    ~144 bytes. The zero-argument rung (`decode: static, ()J`) measured
///    **10.5 ns** with no argument work to do at all, and building the buffer
///    with `new()` in a caller-owned local took the same rung to **0.2 ns**.
///    The first guess was the return move; it was not — `with_capacity` is an
///    outlined call, so the whole value has to be materialised in memory for
///    it, while `new()` inlines and the buffer stays in registers for a decode
///    this small. Worth recording because the by-value shape reads like the
///    expensive part and is not.
///  * **A third validation of the receiver.** `try_jit_site_cached_native_dispatch`
///    resolves the receiver through `is_object_address` to guard the site, and
///    passes the resolved `ObjectRef` here rather than handing back the raw word
///    for this function to validate again. `None` keeps the original behaviour
///    for callers that have not resolved one.
#[inline]
unsafe fn decode_dispatch_values_into(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver: Option<Option<ObjectRef>>,
    values: &mut JitDecodedArgs,
) {
    decode_dispatch_values_into_shaped(vm, info, args_slice, receiver, None, 0, values)
}

/// [`decode_dispatch_values_into`] for a caller holding the site's parameter
/// tags already — see [`NativeSiteDescriptor`]. `params` is `None` for a
/// caller that has none, and then the descriptor is parsed exactly as before.
///
/// `prevalidated` is [`forward_jit_reference_args_validated`]'s mask for this
/// very buffer (`0` = none): a parameter word whose bit is set is taken as the
/// validated reference it is, without a second heap walk (FUNNEL17-2, JIT
/// round 19 wave 2, lane funnel2).
#[inline]
unsafe fn decode_dispatch_values_into_shaped(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver: Option<Option<ObjectRef>>,
    params: Option<&[u8]>,
    prevalidated: u64,
    values: &mut JitDecodedArgs,
) {
    values.clear();
    let mut desc_iter = DescriptorParamIter::new(info.descriptor);
    let mut param_index = 0usize;

    if jit_info_has_receiver(info) {
        if !args_slice.is_empty() {
            match receiver {
                // The caller already ran the validator on this very slot.
                Some(resolved) => values.push(Value::Object(resolved)),
                None => {
                    let ptr = args_slice[0];
                    if ptr == 0 {
                        values.push(Value::Object(None));
                    } else {
                        // Defensive: tagged-long bits in an L-typed receiver slot
                        // are downgraded to null instead of being treated as a
                        // heap pointer (else GC SEGVs walking a bogus oop).
                        let bits = ptr as u64;
                        let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                            crate::jit::helpers::note_membership_walk(2);
                            vm.mem.heap.is_object_address(bits as usize)
                        } else {
                            None
                        };
                        match validated {
                            Some(obj) => values.push(Value::Object(Some(obj))),
                            None => values.push(Value::Object(None)),
                        }
                    }
                }
            }
        }
    }

    let start_idx = usize::from(jit_info_has_receiver(info));
    for (offset, &raw) in args_slice[start_idx..].iter().enumerate() {
        let tag = match params {
            Some(tags) => {
                let tag = tags.get(param_index).copied();
                param_index += 1;
                tag
            }
            None => desc_iter.next(),
        };
        let val = match tag {
            Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                Value::Int(raw as i32)
            }
            Some(b'J') => Value::Long(raw),
            Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
            Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
            Some(b'L') | Some(b'[') => {
                if raw == 0 {
                    Value::Object(None)
                } else if let Some(obj) =
                    prevalidated_jit_ref(prevalidated, start_idx + offset, raw)
                {
                    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`): the
                    // forwarding pass validated this word; no second walk.
                    Value::Object(Some(obj))
                } else {
                    let bits = raw as u64;
                    let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                        crate::jit::helpers::note_membership_walk(2);
                        vm.mem.heap.is_object_address(bits as usize)
                    } else {
                        None
                    };
                    match validated {
                        Some(obj) => Value::Object(Some(obj)),
                        None => Value::Object(None),
                    }
                }
            }
            _ => Value::Int(raw as i32),
        };
        values.push(val);
    }
}

// ---------------------------------------------------------------------------
// Helper: extract heap from SharedVm pointer
// ---------------------------------------------------------------------------

// SAFETY: Caller must ensure vm_ptr is a valid pointer to a live SharedVm instance.
// The SharedVm is heap-allocated and outlives all JIT helper calls.
#[inline]
unsafe fn heap_from_vm(vm_ptr: i64) -> &'static VmHeap {
    debug_assert!(vm_ptr != 0, "heap_from_vm called with null VM pointer");
    // SAFETY: vm_ptr was passed from JIT-compiled code which received it from the
    // interpreter's SharedVm reference, so it points to a valid SharedVm.
    &(*(vm_ptr as *const SharedVm)).mem.heap
}

// ---------------------------------------------------------------------------
// JIT safepoint SATB flush — Round-7 fix (CRIT, audit §3)
// ---------------------------------------------------------------------------
//
// The interpreter drains its per-thread SATB buffer at every safepoint
// arrival (`runtime/interpreter.rs::safepoint_check`, line 899). JIT-
// running threads have no such poll — they only return through one of
// the runtime helpers below. If any of those helpers participates in an
// STW pause (directly via `collect_garbage` or transitively via the
// shared barrier on a concurrent GC trigger) without first draining the
// per-thread SATB buffer, up to `DEFAULT_SATB_CAPACITY` (256) overwritten
// references stay invisible to the marker. The next mixed evacuation
// then turns the classic SATB lost-object scenario into a use-after-
// free (audit: round7-gc.md §3).
//
// `flush_thread_satb` itself is a cheap inline call when `is_active() ==
// false`: a single Acquire load and an early return. We invoke it
// unconditionally at the top of every GC-triggering JIT helper so the
// invariant holds without a separate JIT-emitted safepoint stub.
//
// The same door retires a leftover native-return handoff root first —
// `drain_native_return_at_compiled_helper_entry`, r11 2026-09-25: compiled
// code never consumes `native_pending_return`, so without it the LAST object a
// compiled-code native call returned (a `SoftReference.get()` referent, say)
// stayed a GC root through every collection this helper went on to run.
// SAFETY: every caller is a JIT helper invoked from compiled code; `vm_ptr` is
// either 0 (handled by the early return) or a live `SharedVm` pointer per the
// universal JIT-helper caller contract.
#[inline]
unsafe fn jit_safepoint_flush_satb(vm_ptr: i64) {
    drain_native_return_at_compiled_helper_entry();
    if vm_ptr == 0 {
        return;
    }
    // SAFETY: caller contract for every JIT helper — vm_ptr is a live
    // SharedVm pointer.
    let vm = &*(vm_ptr as *const SharedVm);
    vm.mem.heap.flush_thread_satb();
    serve_handshakes_at_helper_entry(vm);
}

/// JIT round 23 wave 6 (`CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY`, default ON,
/// `=0` kills; the proxy-define page): run this thread's queued per-thread
/// handshakes at a helper's entry door. Compiled code reaches
/// `safepoint_check` only at its poll's slow path, which fires on a pause
/// request alone, or at an allocation slow path, so a compiled peer left a
/// cross-thread frame-trace handshake unserved for the whole 1 ms budget and
/// the requester then stopped the world anyway. A helper entry is a point
/// where this thread can walk its own frames: the same state as a native it
/// is about to dispatch to (`Throwable.fillInStackTrace`), whose own-thread
/// walk reads the same compiled chain and the call site's stamp. The op runs
/// ON this thread, so no other thread walks frames that can move.
/// Steady state: one TLS load and one `Acquire` load.
#[inline]
unsafe fn serve_handshakes_at_helper_entry(vm: &SharedVm) {
    if !cratonvm_types::flags().gc.handshake_at_helper_entry {
        return;
    }
    if let Some((thread, _guard)) = jit_thread_mut() {
        if thread.gc_block_state.handshakes.is_pending() {
            thread.run_pending_handshakes(vm);
        }
    }
}

#[cfg(test)]
mod r23w6_handshake_at_helper_entry_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{HandshakeState, JvmThread, ThreadId};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    /// A frame-trace handshake queued for a thread running compiled code is
    /// served at the next helper entry door (no pause requested): it runs
    /// once, on that thread, publishes and bumps `root_publish_seq`.
    #[test]
    fn r23w6_a_helper_entry_serves_the_queued_frame_trace_handshake() {
        if !cratonvm_types::flags().gc.handshake_at_helper_entry {
            return;
        }
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let tid = ThreadId(23_0601);
        let mut thread = Box::new(JvmThread::new(tid, "r23w6-door"));
        shared.threads.thread_registry.register(tid, "r23w6-door", None);
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let seq = thread.root_publish_seq.load(Ordering::Relaxed);
        let ticket = thread
            .gc_block_state
            .handshakes
            .push(crate::runtime::interpreter::frame_trace_handshake_op());
        assert!(!shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire));
        let scope = set_jit_thread(&mut thread);
        // SAFETY: `vm_ptr` names the live `SharedVm` above and the JIT thread
        // pointer is installed for this OS thread, as compiled code has it.
        unsafe {
            jit_safepoint_flush_satb(vm_ptr);
            // Served once: a second door finds nothing queued.
            jit_safepoint_flush_satb(vm_ptr);
        }
        restore_jit_thread(scope);
        assert_eq!(ticket.state(), HandshakeState::Ran);
        assert!(!thread.gc_block_state.handshakes.is_pending());
        assert_eq!(thread.root_publish_seq.load(Ordering::Relaxed), seq + 1);
    }

    /// Source pin: the door serves handshakes after the SATB flush, under
    /// the switch.
    #[test]
    fn r23w6_the_satb_door_serves_handshakes_under_the_switch() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("\nunsafe fn jit_safepoint_flush_satb(")
            .expect("the door");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("its end")];
        let flush = body.find("flush_thread_satb();").expect("the flush");
        let serve = body
            .find("serve_handshakes_at_helper_entry(vm);")
            .expect("the serve");
        assert!(flush < serve);
        let gate_at = src
            .find("\nunsafe fn serve_handshakes_at_helper_entry(")
            .expect("the server");
        assert!(src[gate_at..].contains("flags().gc.handshake_at_helper_entry"));
    }
}

// ---------------------------------------------------------------------------

/// t1 wave 5 (lane V4): every VM-side inline-cache publication that can take
/// its `JdkOnly` refusal (`jit_entry_publishable` in the jit crate) opens this
/// VM's `JdkOnlySinkScope` first (`jdk_only_ic_publication_scope`), so the
/// refusal is counted in THIS VM's `--jdk-only-report` and not in the process
/// sink no VM reads. Wave 4 (lane Y2) scoped the MIC / PIC sites and missed
/// the shared megamorphic table's install, which the blind dispatch helper
/// reaches with no scope open.
#[cfg(test)]
mod t1v4_ic_publication_scope_tests {
    /// The publication calls whose last argument is the `jdk_only` bit.
    const CALLS: [&str; 3] = ["mic.update(", "pic.install(", ".install_with_class_slot("];
    /// Top-level function heads in the helper sources. t1 wave 7 (lane V6):
    /// every visibility / `unsafe` / `extern "C"` spelling, so a publication
    /// in a `pub(super)` or `extern "C"` function is not attributed to the
    /// function above it (whose own scope would then pass for it).
    const HEADS: [&str; 15] = [
        "\nunsafe fn ",
        "\nfn ",
        "\npub unsafe extern \"C\" fn ",
        "\npub(crate) unsafe fn ",
        "\npub(crate) fn ",
        "\npub unsafe fn ",
        "\npub fn ",
        "\npub(super) fn ",
        "\npub(super) unsafe fn ",
        "\nunsafe extern \"C\" fn ",
        "\nextern \"C\" fn ",
        "\npub extern \"C\" fn ",
        "\npub(crate) extern \"C\" fn ",
        "\npub(crate) unsafe extern \"C\" fn ",
        "\npub(super) unsafe extern \"C\" fn ",
    ];

    /// The call's last top-level argument, trimmed (`open` indexes its `(`).
    fn last_argument(src: &str, open: usize) -> String {
        let mut depth = 0usize;
        let mut last_comma = open;
        for (i, c) in src[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let arg = src[last_comma + 1..open + i].trim();
                        return arg.trim_end_matches(',').to_string();
                    }
                }
                ',' if depth == 1 => {
                    // A trailing comma before `)` keeps the previous argument.
                    if !src[open + i + 1..].trim_start().starts_with(')') {
                        last_comma = open + i;
                    }
                }
                _ => {}
            }
        }
        String::new()
    }

    #[test]
    fn t1v4_every_jdk_only_ic_publication_opens_the_vm_scope() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let mut judged = 0usize;
        let mut unscoped = Vec::new();
        for call in CALLS {
            for (at, _) in src.match_indices(call) {
                let line_start = src[..at].rfind('\n').map_or(0, |n| n + 1);
                let line = src[line_start..at].trim_start();
                // Comments and source-pin string literals are not calls.
                let quoted = src[..at].ends_with('"') || src[..at].ends_with('`');
                if line.starts_with("//") || quoted {
                    continue;
                }
                let open = at + call.len() - 1;
                let last = last_argument(&src, open);
                // A literal `false` (a test's Compatible publication) can never refuse.
                if last == "false" || last.is_empty() {
                    continue;
                }
                let head = HEADS
                    .iter()
                    .filter_map(|h| src[..at].rfind(h))
                    .max()
                    .unwrap_or(0);
                judged += 1;
                if !src[head..at].contains("jdk_only_ic_publication_scope(") {
                    let name = src[head..].lines().nth(1).unwrap_or("").to_string();
                    unscoped.push(format!("{call} in {name}"));
                }
            }
        }
        assert!(judged >= 12, "the guard judged only {judged} publication sites");
        assert!(unscoped.is_empty(), "IC publications with no VM sink scope: {unscoped:?}");
    }
}

/// JIT round 21, lane revvm2 (LAMBDATRAP21-1): the division-guard and
/// hot-catch re-tiers re-decide a repeat whose IR ban could not be recorded,
/// and only that repeat.
#[cfg(test)]
mod jr21_revvm2_site_trap_reclaim_tests {
    use super::*;

    #[test]
    fn a_repeat_re_decides_only_while_the_ir_ban_is_missing() {
        let verdicts = cratonvm_jit::JitVerdictRegistry::default();
        let cid = ClassId::new(4242);
        let first = ("jr21/Rv2", "m", "()V");
        let h = cratonvm_jit::ir_method_memo_hash(first.0, first.1, first.2);
        assert!(
            site_trap_policy_must_apply(&verdicts, h, first, cid),
            "the first decision is claimed"
        );
        verdicts.note_ir_method_refused(first.0, first.1, first.2, cid);
        assert!(
            !site_trap_policy_must_apply(&verdicts, h, first, cid),
            "a repeat under a recorded ban does nothing"
        );
        // Fill the refusal memo to its bound with other methods: a NEW
        // method's ban can no longer be recorded.
        for i in 0..8192u32 {
            let name = "fill".to_string() + &i.to_string();
            verdicts.note_ir_method_refused("jr21/Rv2Fill", &name, "()V", cid);
        }
        let late = ("jr21/Rv2", "n", "()V");
        let h2 = cratonvm_jit::ir_method_memo_hash(late.0, late.1, late.2);
        assert!(site_trap_policy_must_apply(&verdicts, h2, late, cid));
        verdicts.note_ir_method_refused(late.0, late.1, late.2, cid);
        assert!(
            !verdicts.ir_method_ban_holds(late.0, late.1, late.2, cid),
            "the positive control: the memo is full"
        );
        assert!(
            site_trap_policy_must_apply(&verdicts, h2, late, cid),
            "a repeat with no ban in effect re-decides"
        );
        assert!(
            !site_trap_policy_must_apply(&verdicts, h, first, cid),
            "the recorded ban still holds"
        );
    }

    /// t1 wave 8, lane LAMBDATRAP: a repeat that lands between the first
    /// decision's claim and its ban record (another thread) records the ban
    /// itself and does not apply the policy a second time.
    #[test]
    fn a_repeat_racing_the_first_ban_record_does_not_re_decide() {
        let verdicts = cratonvm_jit::JitVerdictRegistry::default();
        let cid = ClassId::new(7175);
        let m = ("t1/LambdatrapRace", "m", "()V");
        let h = cratonvm_jit::ir_method_memo_hash(m.0, m.1, m.2);
        assert!(site_trap_policy_must_apply(&verdicts, h, m, cid), "thread A claims");
        // Thread A has not yet called `note_ir_method_refused`.
        assert!(!verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid));
        assert!(
            !site_trap_policy_must_apply(&verdicts, h, m, cid),
            "thread B's repeat records the ban and stands down"
        );
        assert!(verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid));
    }
}

/// JIT round 21, lane smallfix (REVVM221-1): the division-guard counter
/// re-fires past its threshold, so one thread's hot site reaches
/// `site_trap_policy_must_apply` again and re-decides while the IR ban could
/// not be recorded.
#[cfg(test)]
mod jr21_smallfix_division_guard_refire_tests {
    use super::*;

    #[test]
    fn a_full_refusal_memo_is_re_decided_by_the_same_threads_repeats() {
        let verdicts = cratonvm_jit::JitVerdictRegistry::default();
        let cid = ClassId::new(4243);
        for i in 0..8192u32 {
            let name = "fill".to_string() + &i.to_string();
            verdicts.note_ir_method_refused("jr21/SfFill", &name, "()V", cid);
        }
        let site = ("jr21/SfDiv", "div", "(II)I");
        let h = cratonvm_jit::ir_method_memo_hash(site.0, site.1, site.2);
        let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        let mut decisions = 0;
        for _ in 0..64 {
            if note_division_guard_trap(
                &mut counts,
                7,
                DIVISION_GUARD_RETIER_THRESHOLD,
                DIVISION_GUARD_SITE_CAP,
            ) && site_trap_policy_must_apply(&verdicts, h, site, cid)
            {
                decisions += 1;
                verdicts.note_ir_method_refused(site.0, site.1, site.2, cid);
            }
        }
        assert!(
            !verdicts.ir_method_ban_holds(site.0, site.1, site.2, cid),
            "the positive control: the memo is full"
        );
        assert_eq!(decisions, 3, "exits 16, 32 and 64 each re-decide");
    }

    /// With the ban recorded, the re-fires are claim-set no-ops.
    #[test]
    fn a_recorded_ban_makes_every_re_fire_a_no_op() {
        let verdicts = cratonvm_jit::JitVerdictRegistry::default();
        let cid = ClassId::new(4244);
        let site = ("jr21/SfDivBanned", "div", "(II)I");
        let h = cratonvm_jit::ir_method_memo_hash(site.0, site.1, site.2);
        let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        let mut decisions = 0;
        for _ in 0..256 {
            if note_division_guard_trap(
                &mut counts,
                7,
                DIVISION_GUARD_RETIER_THRESHOLD,
                DIVISION_GUARD_SITE_CAP,
            ) && site_trap_policy_must_apply(&verdicts, h, site, cid)
            {
                decisions += 1;
                verdicts.note_ir_method_refused(site.0, site.1, site.2, cid);
            }
        }
        assert_eq!(decisions, 1);
    }
}

#[cfg(test)]
mod j27_disprows_dispatch_target_tests {
    //! j27 wave 4 (lane DISPROWS, MEGA3-17-3): the one publication condition
    //! of the MIC arms and the blind arm; SELECT17-5: the retarget itself.
    use super::*;

    fn target(cacheable_receiver: bool, selected_cid: u32) -> CachedDispatchTarget {
        CachedDispatchTarget {
            class_name: std::rc::Rc::from("p/Recv"),
            cacheable_receiver,
            globally_named: true,
            selection_error: false,
            selected_cid,
        }
    }

    #[test]
    fn a_receiver_named_target_publishes_its_own_class() {
        let t = target(true, 0);
        assert!(t.publishable());
        assert_eq!(t.compile_cid(0x51), 0x51);
    }

    #[test]
    fn a_retargeted_receiver_publishes_the_selected_class() {
        let t = target(false, 0x77);
        assert!(t.publishable());
        assert_eq!(t.compile_cid(0x51), 0x77);
    }

    #[test]
    fn a_by_name_target_never_publishes() {
        let t = target(false, 0);
        assert!(!t.publishable());
        assert_eq!(t.compile_cid(0x51), 0x51);
    }

    /// A version-52 class `name extends sup` with one method `m()I`
    /// (`iconst_1; ireturn`) of access `flags`. Shared with
    /// `j27_t1ic_mega_bake_access_tests` (j27 wave 7, lane T1IC).
    pub(super) fn one_method_class(name: &str, sup: &str, flags: u16) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, name); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, sup); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "m"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&flags.to_be_bytes());
        b.extend_from_slice(&[0, 5, 0, 6, 0, 1]); // m ()I, one attribute
        let code = [0x04, 0xac]; // iconst_1; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 1]); // max_stack 1, max_locals 1
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// SELECT17-5: `receiver_selection_retarget` with two loaders in one
    /// `ClassStore`. `B` (a user loader) declares a PRIVATE `m` and extends
    /// that loader's `Base`; the application loader holds another `Base`.
    /// An `invokevirtual Base.m` site in `B`'s loader selects the user
    /// loader's `Base` (JVMS §5.4.6: a private method overrides nothing),
    /// whose name does not round-trip to it through the global table: the
    /// retarget names it by id under `CRATONVM_SELECTION_RECEIVER_SUPERTYPE`
    /// (default on) and declines with the switch off.
    #[test]
    fn a_retarget_names_the_selected_class_of_the_sites_own_loader() {
        use cratonvm_classloading::{ClassLoaderId, DefineClassOptions};
        const BASE: &str = "j27disprows/SelBase";
        const FORK_BASE: &str = "j27disprows/SelBase$Fork";
        const B: &str = "j27disprows/SelB";
        const FORK: ClassLoaderId = ClassLoaderId::UserDefined(27_004);
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let defined = {
            let mut cm = shared.classes.class_manager.write();
            let mut define = |name: &str, bytes: Vec<u8>| {
                cm.define_class_with_options(
                    name,
                    &bytes,
                    ClassLoaderId::Application,
                    DefineClassOptions::default(),
                )
                .ok()
            };
            let app_base = define(BASE, one_method_class(BASE, "java/lang/Object", 0x0001));
            let fork_base =
                define(FORK_BASE, one_method_class(FORK_BASE, "java/lang/Object", 0x0001));
            let b = define(B, one_method_class(B, FORK_BASE, 0x0002));
            match (app_base, fork_base, b) {
                (Some(app_base), Some(fork_base), Some(b)) => {
                    // The fork's copy takes the shared name and the user
                    // loader (the class manager's test side-door), as `B`.
                    for (id, name) in [(fork_base, BASE), (b, B)] {
                        if let Some(class) = cm.class_store.get_mut(id) {
                            class.name = cratonvm_types::intern_arc(name);
                            class.loader_id = FORK;
                        }
                        cm.register_class_name(FORK, name, id);
                    }
                    Some((app_base, fork_base, b))
                }
                _ => None,
            }
        };
        let Some((app_base, fork_base, b)) = defined else {
            return; // the stripped test VM could not define them
        };
        assert_ne!(app_base, fork_base);
        let info = JitInvokeInfo {
            class_name: BASE,
            method_name: "m",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: b.as_u32(),
            owner_class_id: 0,
        };
        let cm = shared.classes.class_manager.read();
        assert_eq!(jit_site_owner(&cm, &info), Some(fork_base), "the site's own loader");
        if crate::runtime::resolve::selection::receiver_dispatch_moves_to(
            cm.class_store(),
            b,
            fork_base,
            "m",
            "()I",
        ) != Some(fork_base)
        {
            return; // no real `Object` in this test VM: selection keeps the walk
        }
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SELECTION_RECEIVER_SUPERTYPE", None)],
            || receiver_selection_retarget(&cm, b, &info),
        );
        assert_eq!(on.map(|(name, id)| (name.to_string(), id)), Some((BASE.to_string(), fork_base)));
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SELECTION_RECEIVER_SUPERTYPE", Some("0"))],
            || receiver_selection_retarget(&cm, b, &info),
        );
        assert!(off.is_none(), "the old answer: the name must round-trip");
    }
}

#[cfg(test)]
mod j27_t1ic_mega_bake_access_tests {
    //! j27 wave 7 (lane T1IC, IC21-2 of `jit-r21-ic-proposals.md`): the
    //! access rule of the compile-time bind (`mega_bake_access_is_certain`),
    //! untested since r21 ("they need a ClassManager"). A baked site reads the
    //! cells every site of its selector (caller LOADER, not class) publishes,
    //! so the bind is allowed only where the caller's resolution of the
    //! method cannot fail its access check: owner and caller in one runtime
    //! package, an instance non-private method that is public or of that
    //! package. Every refusal keeps the first-publication bind.
    use super::j27_disprows_dispatch_target_tests::one_method_class;
    use super::*;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions};

    const CALLER: &str = "j27t1ic/p/Caller";

    fn info() -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: "j27t1ic/p/Owner",
            method_name: "m",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    #[test]
    fn the_bake_is_certain_only_inside_the_callers_runtime_package() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        // (owner name, method flags, expected answer, why)
        let rows: [(&str, u16, bool, &str); 6] = [
            ("j27t1ic/p/Public", 0x0001, true, "public, same package"),
            ("j27t1ic/p/Package", 0x0000, true, "package-private, same package"),
            ("j27t1ic/p/Private", 0x0002, false, "private: never a virtual bind"),
            ("j27t1ic/p/Static", 0x0009, false, "static: resolution throws ICCE"),
            ("j27t1ic/q/Other", 0x0001, false, "owner in another package"),
            ("j27t1ic/p/Forked", 0x0001, false, "same name package, another loader"),
        ];
        let defined = {
            let mut cm = shared.classes.class_manager.write();
            let mut define = |name: &str, flags: u16| {
                cm.define_class_with_options(
                    name,
                    &one_method_class(name, "java/lang/Object", flags),
                    ClassLoaderId::Application,
                    DefineClassOptions::default(),
                )
                .ok()
            };
            let caller = define(CALLER, 0x0001);
            let owners: Vec<Option<ClassId>> =
                rows.iter().map(|&(name, flags, _, _)| define(name, flags)).collect();
            // The class manager's test side-door: the last owner moves to a
            // user loader under its own name (another runtime package).
            if let Some(Some(forked)) = owners.last() {
                if let Some(class) = cm.class_store.get_mut(*forked) {
                    class.loader_id = ClassLoaderId::UserDefined(27_071);
                }
            }
            caller.zip(owners.into_iter().collect::<Option<Vec<ClassId>>>())
        };
        // j27 TRIC: a silent return here made the test pass without one
        // assert; the default test VM defines the classes, so refusing to
        // define them is a failure, not a skip.
        let (caller, owners) = defined.expect("the test VM defines the caller and owner classes");
        let cm = shared.classes.class_manager.read();
        for (&(name, _, expected, why), &owner) in rows.iter().zip(owners.iter()) {
            assert_eq!(
                mega_bake_access_is_certain(&cm, caller, owner, &info()),
                expected,
                "{name}: {why}"
            );
        }
        // A method the owner's chain does not declare: nothing to bake.
        let absent = JitInvokeInfo {
            method_name: "absent",
            ..info()
        };
        assert!(
            !mega_bake_access_is_certain(&cm, caller, caller, &absent),
            "an unresolvable method refuses"
        );
    }

    /// j27 wave 7 (lane T1IC): the helper switches were process `OnceLock`
    /// latches, which kept whichever answer the first reader in the test
    /// binary saw. As `MemoSlot`s they follow a thread override both ways.
    #[test]
    fn the_mega_class_slot_switches_follow_a_thread_override() {
        for (key, read) in [
            ("CRATONVM_JIT_MEGA_CLASS_SLOTS", mega_class_slots_enabled as fn() -> bool),
            ("CRATONVM_JIT_MEGA_CLASS_SLOTS_IFACE", mega_class_slots_iface_enabled),
            ("CRATONVM_JIT_MEGA_DISPATCH_TABLE", mega_dispatch_table_enabled),
        ] {
            let at = |v: Option<&str>| cratonvm_types::flags::with_thread_overrides(&[(key, v)], read);
            assert!(!at(Some("0")), "{key}=0 kills");
            assert!(at(None), "{key} is default ON and nothing latched the kill");
            assert!(!at(Some("0")), "{key}=0 again: nothing latched the default");
        }
    }
}

/// j27 wave 7, lane T1LAMTRAP (LAMBDATRAP21-1): the one claim/ban step of the
/// three site-trap-style policies.
#[cfg(test)]
mod j27_t1lamtrap_claim_site_trap_policy_tests {
    use super::*;

    const POLICIES: [SiteTrapPolicy; 3] = [
        SiteTrapPolicy::SiteTrap,
        SiteTrapPolicy::DivisionGuard,
        SiteTrapPolicy::HotCatch,
    ];

    /// The first decision applies the policy AND leaves the IR ban in effect
    /// (the bare `site_trap_policy_must_apply` does not record it on a claim);
    /// a repeat under that ban stands down.
    #[test]
    fn the_first_decision_applies_and_records_the_ban() {
        for (i, policy) in POLICIES.into_iter().enumerate() {
            let verdicts = cratonvm_jit::JitVerdictRegistry::default();
            let cid = ClassId::new(9100 + i as u32);
            let m = ("j27/T1Lamtrap", "m", "()V");
            let h = cratonvm_jit::ir_method_memo_hash(m.0, m.1, m.2);
            assert!(
                claim_site_trap_policy(&verdicts, h, m, cid, policy),
                "{policy:?}: the first decision applies"
            );
            assert!(
                verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid),
                "{policy:?}: the ban is recorded with the decision"
            );
            assert!(
                !claim_site_trap_policy(&verdicts, h, m, cid, policy),
                "{policy:?}: a repeat under the ban stands down"
            );
        }
    }

    /// With the refusal memo at its bound no ban can be recorded, so every
    /// policy's repeat re-decides -- the hole LAMBDATRAP21-1 is about.
    #[test]
    fn a_repeat_without_a_ban_re_decides_under_every_policy() {
        let verdicts = cratonvm_jit::JitVerdictRegistry::default();
        let cid = ClassId::new(9200);
        for i in 0..8192u32 {
            let name = "fill".to_string() + &i.to_string();
            verdicts.note_ir_method_refused("j27/T1LamtrapFill", &name, "()V", cid);
        }
        for (i, policy) in POLICIES.into_iter().enumerate() {
            let name = "late".to_string() + &i.to_string();
            let m = ("j27/T1LamtrapLate", name.as_str(), "()V");
            let h = cratonvm_jit::ir_method_memo_hash(m.0, m.1, m.2);
            assert!(claim_site_trap_policy(&verdicts, h, m, cid, policy));
            assert!(
                !verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid),
                "{policy:?}: the positive control, the memo is full"
            );
            assert!(
                claim_site_trap_policy(&verdicts, h, m, cid, policy),
                "{policy:?}: a repeat with no ban in effect re-decides"
            );
        }
    }

    /// The census words, and only a hot catch spares the trap counter.
    #[test]
    fn census_words_and_repeat_charging() {
        let words: Vec<_> = POLICIES.iter().map(|p| p.census_name()).collect();
        assert_eq!(words, ["site-trap", "division-guard", "hot-catch"]);
        let charged: Vec<_> = POLICIES.iter().map(|p| p.charges_site_trap_repeat()).collect();
        assert_eq!(charged, [true, true, false]);
    }

    /// One decision in one place: the helpers claim a site-trap decision in
    /// exactly one production call (inside `site_trap_policy_must_apply`).
    /// Before this lane `despeculate_trapped_method_inner` claimed inline too.
    #[test]
    fn the_site_trap_claim_has_one_production_caller() {
        let src = crate::jit::helpers::read_helpers_src_from_disk().replace("\r\n", "\n");
        // Split so this test's own text is not a match.
        let call = concat!("verdicts.claim_site_trap", "_decision(h, declaring_class_id)");
        let claims = src.matches(call).count();
        assert_eq!(claims, 1, "claim the decision only through claim_site_trap_policy");
    }
}

#[cfg(test)]
mod j27_w8arg_forced_decline_tests {
    //! j27 wave 8 (lane W8ARG): `CRATONVM_DBG_FORCE_CALLEE_DECLINE` names the
    //! methods whose stashes the callee service declines, and nothing else.
    use super::{forced_callee_decline, forced_decline_filter_names};

    const KEY: &str = "CRATONVM_DBG_FORCE_CALLEE_DECLINE";
    const LEAF: &str = "J27W8ArgDeclineRows.leafS:([II)I";

    #[test]
    fn unset_declines_nothing_and_a_method_filter_declines_its_stashes() {
        let with = |v: Option<&str>, key: &str| {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, v)], || forced_callee_decline(key))
        };
        assert!(!with(None, LEAF), "unset: every stash is offered to the resume");
        assert!(with(Some("J27W8ArgDeclineRows.leaf"), LEAF));
        assert!(!with(Some("J27W8ArgDeclineRows.leaf"), "java/lang/String.hashCode:()I"));
    }

    #[test]
    fn a_filter_without_a_method_dot_is_inert() {
        for filter in ["1", "true", "", "  ", "leafS"] {
            assert!(!forced_decline_filter_names(filter, LEAF), "{filter:?}");
        }
        assert!(forced_decline_filter_names(" J27W8ArgDeclineRows.leafS ", LEAF), "trimmed");
    }
}
