// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Registry for native method implementations.
//!
//! Maps (class, method, descriptor) triples to Rust function callbacks
//! that implement the native method behavior.

// AUDIT 2026-05-16: std::collections::HashMap is unused — the registry
// migrated to rustc_hash::FxHashMap (T10.9.B). Import removed.
use std::sync::{Arc, OnceLock};

// Call-site memoization handles. `NativeMethodId` indexes this file's
// `NativeSlot` table; `NativeMethodKey` is the precomputable digest. Both live
// in `native_id.rs` so the memo cell (`NativeCallSite`) and its documentation
// sit together, away from this file's 3,000-line `NativeContext` trait.
use crate::native_id::{NativeMethodId, NativeMethodKey};

// The ambiguous-vs-absent distinction, and the refusal a by-name lookup is
// allowed to return. See `class_identity` for why a native needs both.
use crate::class_identity::{ClassIdentityError, NameLookup};
use rustc_hash::{FxHashMap, FxHashSet};

use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::{JdkOnlyViolation, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::ClassId;
use cratonvm_types::{ArrayElementType, ObjectKind};
use cratonvm_types::{ObjectRef, Value};

/// `CRATONVM_DBG_NATIVE_LOOKUPS=1` — **how many registry probes one invoke
/// costs**, which is the number the VM-wide per-call page asks for before
/// anyone restructures dispatch:
///
/// > A real fix is one lookup per invoke, handed down the chain — a
/// > restructuring of the dispatch entry points, not a change to the registry.
/// > Anyone starting there should first get the per-invoke lookup COUNT (there
/// > is no counter today; `CRATONVM_DBG_DISPATCH_TALLY` gives callees, not
/// > lookups per callee), because that number, not the profile share, is what a
/// > restructuring would divide.
///
/// A flat profile can only ever say what share `slot_for_exact` has; it cannot
/// say whether that share is one lookup per invoke (in which case a
/// restructuring divides it by one, i.e. buys nothing) or ten. This counts both
/// sides: every entry point into the registry, and every invoke that reaches
/// the two dispatchers those entry points hang off.
///
/// Off by default and gated on one relaxed atomic load, so a disabled run pays
/// a predictable branch per probe and nothing else. When enabled it costs a
/// contended `fetch_add` per probe — fine for a COUNT, useless for a timing.
pub mod lookup_census {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    /// `NativeMethodRegistry::find`.
    pub const FIND: usize = 0;
    /// `NativeMethodRegistry::find_with_kind`.
    pub const FIND_WITH_KIND: usize = 1;
    /// `NativeMethodRegistry::resolve_id` — the every-invoke entry point.
    pub const RESOLVE_ID: usize = 2;
    /// `NativeMethodRegistry::resolve_id_by_key` — the memoized-digest form.
    pub const RESOLVE_ID_BY_KEY: usize = 3;
    /// `resolve_id_with_descriptor_quirks` — the `#[cold]` rewrite arm.
    pub const QUIRKS: usize = 4;
    /// One bytecode-level invoke reaching `try_stackless_invoke`.
    ///
    /// **This is NOT a general per-invoke denominator, and `lookups_per_invoke`
    /// must not be read as "registry probes per Java call".** Measured
    /// 2026-08-18 on `probes/LambdaCompositionProbe.java` at four workload
    /// sizes: `find` scaled perfectly linearly (151 254 / 231 254 / 391 254 /
    /// 711 254) while this counter stayed pinned at **966 in all four runs**.
    /// The lookups that workload generates do not come through this entry
    /// point at all, so the printed ratio grew 162 -> 742 purely because the
    /// numerator moved and the denominator could not.
    ///
    /// The reliable reading is the MARGINAL rate: run two sizes and divide the
    /// difference in `find` by the difference in work. That gave exactly 4.0
    /// lookups per composition stage, with a fixed ~71 k boot cost — a fact
    /// the ratio line could not have produced at any single size.
    pub const INVOKE_STACKLESS: usize = 5;
    /// One call reaching `invoke_or_native`, the general resolver.
    pub const INVOKE_GENERAL: usize = 6;

    const N: usize = 7;
    const NAMES: [&str; N] = [
        "find",
        "find_with_kind",
        "resolve_id",
        "resolve_id_by_key",
        "quirks",
        "invokes(stackless)",
        "invokes(general)",
    ];

    static COUNTS: [AtomicU64; N] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    static INIT: OnceLock<bool> = OnceLock::new();

    #[inline]
    fn enabled() -> bool {
        // The already-initialised load first: after the first probe this is a
        // relaxed pointer read and a compare, which is what a per-invoke path
        // can afford. `get_or_init` runs once.
        if let Some(v) = INIT.get() {
            return *v;
        }
        *INIT.get_or_init(|| {
            None::<std::ffi::OsString>.is_some()        })    }

    /// Count one probe of `kind`. Inlined so the disabled case is a load and a
    /// not-taken branch.
    #[inline]
    pub fn probe(kind: usize) {
        if !enabled() {
            return;
        }
        record(kind);
    }

    #[cold]
    fn record(kind: usize) {
        let n = COUNTS[kind].fetch_add(1, Ordering::Relaxed) + 1;
        // Report on the INVOKE counters only: they are the denominators, so a
        // line is emitted at a round number of invokes rather than at a round
        // number of lookups, and every line is directly comparable.
        if kind == INVOKE_STACKLESS && n % 50_000_000 == 0 {
            report("periodic");
        }
    }

    /// Zero every counter.
    pub fn reset() {
        for c in COUNTS.iter() {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// Tally of MISSED lookup triples — the question the ratio cannot answer:
    /// not "how many", but "which".
    ///
    /// A miss is `find` returning `None` after both the exact probe and the
    /// descriptor-quirk rewrite failed. Measured on `LambdaCompositionProbe`
    /// those are ~100% of all `find` calls and exactly 4.0 per composition
    /// stage, so the top rows here name the four — and a triple like
    /// `java/util/concurrent/CompletableFuture.thenApply` identifies its caller
    /// far more directly than a stack would. Which matters, because `perf`'s
    /// dwarf unwinding through these frames yields bogus return addresses and
    /// gives no callers at all.
    ///
    /// Behind the same `CRATONVM_DBG=native-lookups` gate, and allocating only
    /// when it is on.
    static MISSES: OnceLock<parking_lot::Mutex<std::collections::HashMap<String, u64>>> =
        OnceLock::new();

    /// Record one missed triple. Gated; a disabled run does not allocate.
    #[inline]
    pub fn note_miss(class_name: &str, method_name: &str, descriptor: &str) {
        if !enabled() {
            return;
        }
        record_miss(class_name, method_name, descriptor);
    }

    #[cold]
    fn record_miss(class_name: &str, method_name: &str, descriptor: &str) {
        let map = MISSES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
        let key = format!("{class_name}.{method_name}{descriptor}");
        *map.lock().entry(key).or_insert(0) += 1;
    }

    /// The `n` most-missed triples, hottest first.
    fn top_misses(n: usize) -> Vec<(String, u64)> {
        let Some(map) = MISSES.get() else {
            return Vec::new();
        };
        let mut v: Vec<(String, u64)> = map.lock().iter().map(|(k, c)| (k.clone(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Print the census and the ratio it exists to produce. Safe to call when
    /// disabled — it prints nothing.
    ///
    /// **Read `lookups_per_invoke` with the caveat on [`INVOKE_STACKLESS`].**
    /// It is lookups over *stackless-entry invokes*, not over Java calls, and
    /// on a workload whose lookups arrive by another path the denominator is
    /// constant while the numerator scales — which makes the ratio grow with
    /// the workload and mean nothing. Take two sizes and use the marginal
    /// rate.
    pub fn report(tag: &str) {
        if !enabled() {
            return;
        }
        let v: Vec<u64> = COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        let lookups: u64 = v[FIND] + v[FIND_WITH_KIND] + v[RESOLVE_ID] + v[RESOLVE_ID_BY_KEY];
        let invokes = v[INVOKE_STACKLESS];
        let per_invoke = if invokes == 0 {
            0.0
        } else {
            lookups as f64 / invokes as f64
        };
        let mut parts = String::new();
        for (i, name) in NAMES.iter().enumerate() {
            parts.push_str(&format!(" {name}={}", v[i]));
        }
        eprintln!(
            "[native-lookups {tag}] lookups={lookups} invokes={invokes} \
             lookups_per_invoke={per_invoke:.2}{parts}"
        );
        for (triple, count) in top_misses(12) {
            eprintln!("[native-lookups {tag}] miss {count:>10}  {triple}");
        }
    }
}

/// nio21 wave 17 L1: the class-hash memo answers exactly the walk's state.
#[cfg(test)]
mod nio21w17_class_hash_memo_tests;
#[cfg(test)]
mod native_alloc_no_gc_point_tests;
#[cfg(test)]
impl NativeMethodRegistry {
    /// TEST ONLY: manufacture a 128-bit digest collision.
    ///
    /// Points the digest of `victim` (a triple that is NOT registered) at the
    /// slot owned by `owner` (a triple that IS registered) — exactly the state
    /// a real FNV-1a collision would produce. Everything downstream of the map
    /// probe is the production code path, so this exercises the full-name
    /// verification in `slot_index_for_key` for real. Without that check,
    /// `find(victim)` would return `owner`'s callback: the silent type
    /// confusion the `class_manager::name_to_id` map used to cause.
    ///
    /// Finding a genuine 128-bit collision is computationally infeasible, which
    /// is precisely why the check has to be tested by injection.
    fn inject_digest_collision_for_test(
        &mut self,
        owner: (&str, &str, &str),
        victim: (&str, &str, &str),
    ) {
        let owner_key = native_method_hash(owner.0, owner.1, owner.2);
        let idx = *self
            .slot_by_key
            .get(&owner_key)
            .expect("owner triple must already be registered");
        let victim_key = native_method_hash(victim.0, victim.1, victim.2);
        assert_ne!(
            owner_key, victim_key,
            "test setup: owner and victim must be distinct triples"
        );
        self.slot_by_key.insert(victim_key, idx);
        // The victim triple is deliberately NOT registered, so its class is
        // not in the prefilter — without this the injected collision would be
        // filtered out before `slot_index_for_key` ever ran, and the test
        // would pass for the wrong reason.
        self.classes_with_natives
            .insert(native_class_hash(victim.0));
    }
}

#[cfg(test)]
mod r23w8h_reentry_own_switch_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod i65_l3_settled_door_key_tests;

#[cfg(test)]
mod i72_l5_define_switch_tests;

#[cfg(test)]
mod i81_l5_loader_kind_memo_switch_tests;

#[cfg(test)]
mod i83_l5_loader_namespace_index_switch_tests;

#[cfg(test)]
mod i73_l3_field_door_tests;

#[cfg(test)]
mod i74_l2_stack_trace_element_of_tests;

#[cfg(test)]
mod i75_l2_origin_record_switch_tests;

#[cfg(test)]
mod i76_l2_stack_trace_switch_tests;

#[cfg(test)]
mod i77_l1_reentrant_lock_fast_switch_tests;

#[cfg(test)]
mod i79_l2_compat_sfi_element_switch_tests;

#[cfg(test)]
mod i77_l2_stack_trace_switch_tests;

#[cfg(test)]
mod i74_l3_reflect_switch_tests;

#[cfg(test)]
mod i75_l3_reflect_switch_tests;

#[cfg(test)]
mod i76_l3_reflect_word_batch_switch_tests;

#[cfg(test)]
mod i77_l3_reflect_door_switch_tests;

#[cfg(test)]
mod i81_l3_jit_reflect_door_direct_switch_tests;

#[cfg(test)]
mod i73_l5_read_path_switch_tests;

#[cfg(test)]
mod i74_l5_zip_path_switch_tests;

#[cfg(test)]
mod i75_l5_zip_lookup_switch_tests;

#[cfg(test)]
mod i79_l5_define_records_supertype_initiation_tests;
#[cfg(test)]
mod i80_l4_intrinsic_body_compiles_rows_tests;

#[cfg(test)]
mod i79_l5_intrinsic_body_compiles_tests;

#[cfg(test)]
mod gci_i11ja_jni_binding_default_tests;

#[cfg(test)]
mod i138_l2_strict_family_bridges_tests;


mod real_net_sockets_enabled;
#[allow(unused_imports)]
pub use real_net_sockets_enabled::*;
mod native_alloc_no_gc_point;
#[allow(unused_imports)]
pub use native_alloc_no_gc_point::*;
mod native_gpu_access;
#[allow(unused_imports)]
pub use native_gpu_access::*;
mod registration_provenance;
#[allow(unused_imports)]
pub use registration_provenance::*;
mod set_capabilities;
#[allow(unused_imports)]
pub use set_capabilities::*;
mod read_intrinsic_body_compiles_rows;
#[allow(unused_imports)]
pub use read_intrinsic_body_compiles_rows::*;
