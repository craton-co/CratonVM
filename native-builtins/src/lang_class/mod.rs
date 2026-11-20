// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Class, reflect.Method, reflect.Field, reflect.Constructor native method implementations.

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{
    AnnotationData, FieldMetadata, MethodMetadata, NativeContext, NativeHandleScope,
    TypeArgAnnotations,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ClassId, ObjectRef, Value};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::lang_math::alloc_wrapper;
use crate::obj_arg;
use crate::reflect_door_phases::PhaseTimer;
use crate::try_alloc_concurrent_synthetic;
use crate::try_alloc_with_appended_slots;
// ---------------------------------------------------------------------------
// Access control constants (JVM access flags)
//
// Canonical definitions live in `cratonvm_types::access_flags`; the `_I32`
// aliases here keep existing call sites compact while pointing at the
// shared source of truth.
// ---------------------------------------------------------------------------

use cratonvm_types::access_flags::{
    ACC_ENUM, ACC_FINAL_I32 as ACC_FINAL, ACC_PUBLIC_I32 as ACC_PUBLIC,
    ACC_STATIC_I32 as ACC_STATIC, ACC_VOLATILE_I32 as ACC_VOLATILE,
};

#[cfg(test)]
impl FieldIdent {
    /// For the pure-Rust helper tests below, which have no VM to read a real
    /// `Field` from.
    fn for_test(qualified: &str, ty: &str, modifiers: i32) -> Self {
        FieldIdent {
            qualified: IdentText::Owned(qualified.to_string()),
            ty: IdentText::Owned(ty.to_string()),
            is_static: (modifiers & ACC_STATIC) != 0,
            is_final: (modifiers & ACC_FINAL) != 0,
            trusted_final: (modifiers & ACC_FINAL) != 0 && (modifiers & ACC_STATIC) != 0,
        }
    }
}

#[cfg(test)]
mod i59_l3_runtime_package_tests;

#[cfg(test)]
mod r15w5_jdkonly7_access_text_tests;

/// JIT round 21 wave 14, lane hidden-text (`CRATONVM_REFLECT_HIDDEN_NAME_TEXT`):
/// reflection texts name a hidden class as `Class.getName()` does.
#[cfg(test)]
mod r21w14_hidden_name_text_tests;
macro_rules! s111_dbg {
    ($($arg:tt)*) => {
        if crate::lang_class::s111_dbg_enabled() {
            eprintln!($($arg)*);
        }
    };
}

#[cfg(test)]
mod i133_l5d_builtin_for_name_record_tests;
#[cfg(test)]
mod r15w5_jdkonly7_serialization_shape_tests;

#[cfg(test)]
mod i98_l8_serialization_ctor_refusal_tests;
#[cfg(test)]
mod r15w6_jni6_lambda_new_instance_tests;
#[cfg(test)]
mod r15w4_jdkonly6_class_new_instance_tests;
#[cfg(test)]
mod nio21_w21_ite_init_memo_tests;
#[cfg(test)]
mod accessor_illegal_argument_tests;
/// Legacy alias kept for the test-helper `make_field_mirror` which still
/// uses the synthetic 7-slot layout directly (the mock `NativeContext`
/// doesn't populate a real class-hierarchy so `get_field_by_name` would
/// fall back to slot 0 вЂ” instead the tests index by the synthetic slots).
#[cfg(test)]
const FIELD_NUM_FIELDS: usize = FIELD_NUM_FIELDS_LEGACY_FLOOR;
#[cfg(test)]
mod i7_l2_component_tests;
// ---------------------------------------------------------------------------
// java.lang.reflect.Method вЂ” object layout and natives
// ---------------------------------------------------------------------------

// Number of "extra" slots appended after the JDK Method layout to hold
// CratonVM-specific metadata that doesn't exist on real JDK Method:
//   +0 в†’ String (raw descriptor, e.g. "(II)I")
//   +1 в†’ Int    (parameter count вЂ” cached)
//   +2 в†’ Int    (accessible flag, 0 or 1)
// The fourth tail slot marks metadata written by `create_method_object` as
// immutable and safe to use without rebuilding the JDK field descriptor.

// ---------------------------------------------------------------------------
// getDeclaredMethods phase profiler (CRATONVM_DBG_GDM_PROF=1).
//
// `Class.getDeclaredMethods()` on a 1000-method class costs ~3.4 ms on
// CratonVM against ~47 us on HotSpot, and the per-method cost GROWS with the
// method count -- so there is a super-linear term on top of a large linear
// one. This splits `create_method_object` into phases so the next fix is
// aimed at a measured cost rather than a plausible one. Off by default; when
// off, each boundary is one `Option` test.
// ---------------------------------------------------------------------------
pub mod gdmp {
    use std::cell::RefCell;
    use std::sync::OnceLock;
    use std::time::Instant;

    pub const NBUCKETS: usize = 14;
    pub const NAMES: [&str; NBUCKETS] = [
        "alloc",
        "mirror+name",
        "ret_mirror",
        "param_arr",
        "method_exceptions",
        "exc_arr+desc_str",
        "pin_reread",
        "set_field_by_name",
        "named_layout",
        "method_signature",
        "extra_slots",
        "declared_methods",
        "filter+reorder",
        "link_isolated",
    ];

    static ON: OnceLock<bool> = OnceLock::new();

    pub fn on() -> bool {
        *ON.get_or_init(|| {
            // `runtime_var`, not `std::env::var` — see the flag-declaration
            // guard: a raw getenv bypasses the latched `VmFlags` snapshot.
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false)
        })
    }

    thread_local! {
        static ACC: RefCell<[u64; NBUCKETS]> = const { RefCell::new([0u64; NBUCKETS]) };
    }

    pub fn reset() {
        ACC.with(|a| *a.borrow_mut() = [0u64; NBUCKETS]);
    }

    pub fn add(bucket: usize, ns: u64) {
        ACC.with(|a| a.borrow_mut()[bucket] += ns);
    }

    pub fn snapshot() -> [u64; NBUCKETS] {
        ACC.with(|a| *a.borrow())
    }

    /// Stopwatch that attributes the time since its last mark to a bucket.
    pub struct Lap {
        last: Option<Instant>,
    }

    impl Lap {
        pub fn new() -> Self {
            Self {
                last: if on() { Some(Instant::now()) } else { None },
            }
        }
        pub fn mark(&mut self, bucket: usize) {
            if let Some(last) = self.last {
                let now = Instant::now();
                add(bucket, now.duration_since(last).as_nanos() as u64);
                self.last = Some(now);
            }
        }
    }

    pub fn report(tag: &str, n: usize, total_ns: u64) {
        if !on() {
            return;
        }
        let acc = snapshot();
        let sum: u64 = acc.iter().sum();
        let mut parts = String::new();
        for (i, ns) in acc.iter().enumerate() {
            if *ns == 0 {
                continue;
            }
            parts.push_str(&format!(" {}={:.0}us", NAMES[i], *ns as f64 / 1000.0));
        }
        eprintln!(
            "[gdmprof] class={} n={} total={:.0}us accounted={:.0}us{}",
            tag,
            n,
            total_ns as f64 / 1000.0,
            sum as f64 / 1000.0,
            parts
        );
    }
}

// ---------------------------------------------------------------------------
// getDeclaredConstructors0 phase profiler (CRATONVM_DBG_GDC_PROF=1;
// interpreter round i4 wave 89, lane L3).
//
// `Class.getConstructor()` on a freshly defined class costs ~111 us here
// against ~1.5 us on HotSpot. The JDK half (`privateGetDeclaredConstructors`,
// `reflectionData()`'s `SoftReference`, `searchConstructors`,
// `copyConstructor`) is Java; this times the native half,
// `native_class_get_declared_constructors` and each
// `create_constructor_object`, by phase, and prints the per-call means every
// 64 calls (and the totals at the end of each window) on stderr:
//   [gdcprof] calls=64 native=<us/call> ctors=<n> <phase>=<us/call> ...
// Off by default; when off each boundary is one `Option` test.
// ---------------------------------------------------------------------------
pub mod gdcp {
    use std::cell::RefCell;
    use std::sync::OnceLock;
    use std::time::Instant;

    pub const NBUCKETS: usize = 18;
    // Wave 90, lane L3: wave 89's `ctor_alloc_mirror` (10 us of 27) is split
    // into `ctor_alloc_class` (the by-name `ensure_class_initialized` of
    // `java/lang/reflect/Constructor` and its field count, or the
    // `CRATONVM_REFLECT_CTOR_MIRROR_MEMO` read), `ctor_alloc_obj`
    // (`alloc_object` + pin) and `ctor_decl_mirror` (the declaring class's
    // mirror + pin). The three sum to the old bucket.
    pub const NAMES: [&str; NBUCKETS] = [
        "link",
        "note",
        "declared_methods",
        "order",
        "signature_types",
        "component",
        "ctor_alloc_class",
        "ctor_param_arr",
        "ctor_desc_str",
        "ctor_exceptions",
        "ctor_annotations",
        "ctor_fields",
        "ctor_plan",
        "ctor_signature",
        "ctor_extra",
        "ctor_side_table",
        "ctor_alloc_obj",
        "ctor_decl_mirror",
    ];
    pub const LINK: usize = 0;
    pub const NOTE: usize = 1;
    pub const DECLARED: usize = 2;
    pub const ORDER: usize = 3;
    pub const SIGNATURE_TYPES: usize = 4;
    pub const COMPONENT: usize = 5;
    pub const CTOR_ALLOC: usize = 6;
    pub const CTOR_PARAMS: usize = 7;
    pub const CTOR_DESC: usize = 8;
    pub const CTOR_EXCEPTIONS: usize = 9;
    pub const CTOR_ANNOTATIONS: usize = 10;
    pub const CTOR_FIELDS: usize = 11;
    pub const CTOR_PLAN: usize = 12;
    pub const CTOR_SIGNATURE: usize = 13;
    pub const CTOR_EXTRA: usize = 14;
    pub const CTOR_SIDE: usize = 15;
    pub const CTOR_ALLOC_OBJ: usize = 16;
    pub const CTOR_DECL_MIRROR: usize = 17;

    const WINDOW: u64 = 64;

    static ON: OnceLock<bool> = OnceLock::new();

    pub fn on() -> bool {
        *ON.get_or_init(|| {
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false)
        })
    }

    struct Acc {
        buckets: [u64; NBUCKETS],
        calls: u64,
        ctors: u64,
        native: u64,
    }

    thread_local! {
        static ACC: RefCell<Acc> = const {
            RefCell::new(Acc { buckets: [0u64; NBUCKETS], calls: 0, ctors: 0, native: 0 })
        };
    }

    /// Stopwatch attributing the time since its last mark to a bucket.
    pub struct Lap {
        start: Option<Instant>,
        last: Option<Instant>,
    }

    impl Lap {
        pub fn new() -> Self {
            let now = if on() { Some(Instant::now()) } else { None };
            Self { start: now, last: now }
        }
        pub fn mark(&mut self, bucket: usize) {
            if let Some(last) = self.last {
                let now = Instant::now();
                let ns = now.duration_since(last).as_nanos() as u64;
                ACC.with(|a| a.borrow_mut().buckets[bucket] += ns);
                self.last = Some(now);
            }
        }
        /// One `getDeclaredConstructors0` call ends: its total, the number of
        /// constructors it built, and the window's report.
        pub fn finish_call(&self, ctors: usize) {
            let Some(start) = self.start else {
                return;
            };
            let total = start.elapsed().as_nanos() as u64;
            ACC.with(|a| {
                let mut a = a.borrow_mut();
                a.calls += 1;
                a.ctors += ctors as u64;
                a.native += total;
                if a.calls % WINDOW == 0 {
                    let calls = a.calls as f64;
                    let mut parts = String::new();
                    for (i, ns) in a.buckets.iter().enumerate() {
                        if *ns != 0 {
                            parts.push_str(&format!(
                                " {}={:.2}",
                                NAMES[i],
                                *ns as f64 / calls / 1000.0
                            ));
                        }
                    }
                    eprintln!(
                        "[gdcprof] calls={} native={:.2}us/call ctors={}{} (us/call)",
                        a.calls,
                        a.native as f64 / calls / 1000.0,
                        a.ctors,
                        parts
                    );
                }
            });
        }
    }
}


/// Public for tests that still index by a synthetic slot layout. Mirrors
/// the floor so tests that assume the 8-slot layout still have room.
#[cfg(test)]
const METHOD_NUM_FIELDS: usize = METHOD_NUM_FIELDS_LEGACY_FLOOR;
#[cfg(test)]
mod zt9cl5_class_name_memo_tests;
#[cfg(test)]
mod zr11nd_depth_restore_tests;

#[cfg(test)]
const CONSTRUCTOR_NUM_FIELDS: usize = CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR;
#[cfg(test)]
mod annotation_proxy_cache_unload_tests;

/// Round 16 wave 16 (lane proxy6): the handed-out `AnnotationInvocationHandler`
/// rows and the AO-2 member-order memo follow the annotation-proxy tables'
/// lifecycle (root scan, remap, prune, unload, redefinition, teardown).
#[cfg(test)]
mod annotation_jdk_handler_table_tests;
#[cfg(test)]
mod annorder_cds_order_tests;

/// One `<clinit>`/`values()` pair per enum, because the registry takes fn
/// POINTERS and a loop over `(class, constants)` would need capturing
/// closures. Each body is a single delegation, so the logic these six share
/// still has exactly one implementation.
#[cfg(feature = "synthetic-jdk")]
macro_rules! synthetic_enum_pair {
    ($clinit:ident, $values:ident, $class:expr, $constants:expr) => {
        pub fn $clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
            publish_synthetic_enum_constants(ctx, $class, $constants)
        }
        pub fn $values(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
            let _ = ctx.ensure_class_initialized($class);
            synthetic_enum_values(ctx, $class, $constants)
        }
    };
}



#[cfg(test)]
mod protection_domain_layout_tests;

#[cfg(test)]
mod serialization_constructor_accessor_tests;

#[cfg(test)]
mod reflection_factory_caller_visibility_tests;

#[cfg(test)]
mod reflective_long_from_reference_tests;

#[cfg(test)]
mod i7_l6_lambda_name_tests;

#[cfg(test)]
mod i8_l6_lambda_reflection_tests;

#[cfg(test)]
mod r13w7_predicates_runtime_proxy_name_tests;

#[cfg(test)]
mod r21_reflect_sig_poly_uoe_tests;

#[cfg(test)]
mod r21_invoke0_vm_texts_tests;

#[cfg(test)]
mod r21_native_accessor_only_tests;

/// JIT round 23 wave 1, lane L6 (REVIEW22-2): the per-VM memo of the
/// `--compatible` name bridges' verdicts.
#[cfg(test)]
mod r23_w1_l6_name_bridge_verdict_tests;

/// The VM publishes built-in loader packages during class definition, so the
/// JDK's own `getDefinedPackage` bytecode remains authoritative.
#[cfg(test)]
mod t1w13_defined_package_body_authority_tests;

/// t1 wave 9, lane INTV: the `packages` readers name `java.lang.ClassLoader`'s
/// own field, so a user loader subclass's field of the same name is not read
/// in its place ([`classloader_packages_field`]).
#[cfg(test)]
mod t1w9_intv_loader_packages_field_tests;

/// JIT round 21, lane manifest (`jr21-manifest-cache-rows-outlive-their-vm-20261014.md`): the
/// class-path `Manifest` cache is per VM and VM teardown drops its rows.
#[cfg(test)]
mod jr21_manifest_tests;

#[cfg(test)]
mod jr21_reflect3_forname_tests;

#[cfg(test)]
mod j27_tracerows_for_name_array_cnfe_tests;

mod dbg_bb_enabled;
#[allow(unused_imports)]
pub use dbg_bb_enabled::*;
#[allow(unused_imports)]
use dbg_bb_enabled::*;
mod wf7_synthesise_entry_class_if;
#[allow(unused_imports)]
pub use wf7_synthesise_entry_class_if::*;
#[allow(unused_imports)]
use wf7_synthesise_entry_class_if::*;
mod descriptor_to_class_mirror_via;
#[allow(unused_imports)]
pub use descriptor_to_class_mirror_via::*;
#[allow(unused_imports)]
use descriptor_to_class_mirror_via::*;
mod native_field_set;
#[allow(unused_imports)]
pub use native_field_set::*;
#[allow(unused_imports)]
use native_field_set::*;
mod executable_access_refusal_chain;
#[allow(unused_imports)]
pub use executable_access_refusal_chain::*;
#[allow(unused_imports)]
use executable_access_refusal_chain::*;
mod forget_lang_class_keys;
#[allow(unused_imports)]
pub use forget_lang_class_keys::*;
#[allow(unused_imports)]
use forget_lang_class_keys::*;
mod gc_update_annotation_proxy_refs;
#[allow(unused_imports)]
pub use gc_update_annotation_proxy_refs::*;
#[allow(unused_imports)]
use gc_update_annotation_proxy_refs::*;
mod native_class_is_annotation_present;
#[allow(unused_imports)]
pub use native_class_is_annotation_present::*;
#[allow(unused_imports)]
use native_class_is_annotation_present::*;
mod classloader_packages_synced;
#[allow(unused_imports)]
pub use classloader_packages_synced::*;
#[allow(unused_imports)]
use classloader_packages_synced::*;
mod declaring_class_loader_aware;
#[allow(unused_imports)]
pub use declaring_class_loader_aware::*;
#[allow(unused_imports)]
use declaring_class_loader_aware::*;
