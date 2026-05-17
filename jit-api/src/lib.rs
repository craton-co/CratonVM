//! JIT compiler API types for RustJVM.
//!
//! Shared types used by the JIT compiler crate and the VM crate:
//! - [`CachedBytecodeMethod`] — method data needed for JIT compilation
//! - [`JitRuntimeHelpers`] — function pointer table for JIT runtime callbacks
//! - [`gpu_lowering::GpuLowering`] (under the `gpu-lowering` feature) —
//!   trait seam for emitting PTX from a resolved Java method.

#[cfg(feature = "gpu-lowering")]
pub mod gpu_lowering;

use std::sync::Arc;

use rustjvm_reader::attribute::ExceptionTableEntry;
use rustjvm_types::ClassId;

/// Cached bytecode method info — everything needed to create a Frame without
/// any lock acquisitions or string allocations.
#[derive(Clone)]
pub struct CachedBytecodeMethod {
    pub declaring_class_id: ClassId,
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub method_descriptor: Arc<str>,
    pub source_file: Option<Arc<str>>,
    pub code: Arc<[u8]>,
    pub exception_table: Arc<[ExceptionTableEntry]>,
    pub max_stack: u16,
    pub max_locals: u16,
    pub num_params: u16,
    pub is_synchronized: bool,
    pub is_static: bool,
}

/// Function pointer table for JIT runtime callbacks.
///
/// The JIT compiler embeds these addresses into generated machine code as
/// absolute `CALL` targets. Each pointer is the address of an `extern "C"`
/// function implemented in the VM crate.
#[derive(Clone, Copy, Debug)]
pub struct JitRuntimeHelpers {
    pub newarray: usize,
    pub new_object: usize,
    pub anewarray_object: usize,
    pub baload: usize,
    pub bastore: usize,
    pub iaload: usize,
    pub iastore: usize,
    pub aaload: usize,
    pub aastore: usize,
    pub multianewarray_2d: usize,
    pub arraylength: usize,
    pub getfield: usize,
    pub putfield_int: usize,
    pub putfield_long: usize,
    pub putfield_float: usize,
    pub putfield_double: usize,
    pub putfield_object: usize,
    pub getstatic: usize,
    pub putstatic_int: usize,
    pub putstatic_long: usize,
    pub putstatic_float: usize,
    pub putstatic_double: usize,
    pub putstatic_object: usize,
    pub checkcast: usize,
    pub instanceof_check: usize,
    pub throw_aioobe: usize,
    pub invoke_dispatch: usize,
    pub invoke_virtual_mic: usize,
    pub write_barrier: usize,
    /// Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
    ///
    /// Logs the *old* reference value before a reference store, so the
    /// concurrent marker maintains the snapshot-at-the-beginning invariant.
    /// Without this, JIT-compiled `aastore`/`putfield`/`putstatic`
    /// overwriting a still-live reference during concurrent marking would
    /// drop the only path to the overwritten target → missed mark →
    /// use-after-free on the next mixed evacuation.
    ///
    /// Signature: `extern "C" fn(vm_ptr: i64, old_ref: i64)`.
    /// The helper short-circuits cheaply (single Acquire load) when
    /// `SatbQueue::is_active() == false`, which is the steady-state when
    /// no concurrent mark cycle is in flight.
    pub satb_pre_write_barrier: usize,
    /// Uncommon trap handler: called from JIT code when a speculative
    /// optimization fails (wrong receiver type, unreached branch, etc.).
    /// Signature: extern "C" fn(vm_ptr: i64, reason: i64, bci: i64) -> i64
    /// Returns a deopt action code (0=reinterpret, 1=recompile, 2=blacklist).
    pub uncommon_trap: usize,
    /// T1.1.28 — `Math.fma(double, double, double)` fused multiply-add.
    /// Signature: `extern "C" fn(a: f64, b: f64, c: f64) -> f64`.
    /// Implemented via Rust's `f64::mul_add`, which emits `VFMADD231SD`
    /// when the target CPU supports FMA3 and otherwise performs a
    /// software-correct single-rounding fused operation.
    pub math_fma_double: usize,
    /// T1.1.28 — `Math.fma(float, float, float)`.
    /// Signature: `extern "C" fn(a: f32, b: f32, c: f32) -> f32`.
    pub math_fma_float: usize,

    // -----------------------------------------------------------------
    // Inline TLAB bump-pointer wiring (HIGH-6 JIT audit, object_allocation)
    //
    // These three `usize`s are NOT function pointers — they are byte
    // offsets the JIT bakes as immediates into the inline TLAB
    // fast-path emitted by `jit/src/x64.rs::new` (opcode 0xbb).
    //
    // The helper-table populator (`vm/src/jit/helpers.rs::build_helpers`)
    // computes them once at startup from `JvmThread::tlab_offset()` plus
    // `Tlab::CURSOR_OFFSET` / `Tlab::END_OFFSET`. The runtime asserts in
    // `Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`
    // guard the layout — change them in lockstep.
    // -----------------------------------------------------------------
    /// Byte offset of `Tlab::cursor` from `&JvmThread` (i.e. the full
    /// `JvmThread → tlab → cursor` chain). Emitted as `[thread+disp32]`
    /// in the inline bump fast path.
    pub tlab_cursor_offset_in_thread: usize,
    /// Byte offset of `Tlab::end` from `&JvmThread`.
    pub tlab_end_offset_in_thread: usize,
    /// Byte offset of `ObjectHeader.class_id` from the object base.
    /// Currently `0` (JIT contract enforced by `class_id_remains_at_offset_zero`
    /// in `types/src/heap_types.rs`), exposed here so the JIT does not
    /// hardcode the constant in a second place.
    pub class_id_offset_in_obj: usize,
    /// Address of the small `extern "C" fn() -> *mut JvmThread` helper
    /// that returns the current thread's `JvmThread` pointer via the
    /// `JIT_THREAD` thread-local. Used by the inline TLAB bump as the
    /// first step (one CALL is cheaper than a full `jit_new_object`
    /// dispatch). Set to `0` when not wired (JIT then falls back to the
    /// helper-call path).
    pub get_current_thread: usize,
    /// Address of `extern "C" fn(vm_ptr, obj_ptr, class_id, num_fields)
    /// -> i64`, the inline-TLAB completion helper. Called by the JIT
    /// after a successful inline bump-pointer to finish the object
    /// header (kind/hash/num_slots), apply primitive-typed defaults
    /// from class metadata, and register finalizable classes. Returns
    /// the object pointer unchanged.
    pub tlab_post_init: usize,
}

impl JitRuntimeHelpers {
    /// Number of fields included in the bulk-validation arrays
    /// (`all_pointers` / `field_names`).
    ///
    /// Round-7 fix: previously the two parallel arrays each hard-coded
    /// `[T; 33]`. If a new function-pointer field were added but only
    /// one array updated, the parallel-array invariant would silently
    /// drift. Now both arrays use this constant and the constructors
    /// `debug_assert_eq!` their populated length to it, so a missing
    /// update trips loudly in debug builds.
    pub const NUM_FIELDS: usize = 33;

    /// Validate that all function pointers are non-null and properly aligned.
    ///
    /// Function pointers should be non-zero (null function pointers are invalid)
    /// and aligned to at least 2 bytes (the minimum code alignment on most
    /// architectures; x86 allows 1-byte alignment but functions are never at
    /// address 0).
    ///
    /// NOTE: The `tlab_cursor_offset_in_thread`, `tlab_end_offset_in_thread`,
    /// `class_id_offset_in_obj`, `get_current_thread`, and `tlab_post_init`
    /// fields are NOT validated here. The first three are byte offsets
    /// (zero is a valid value — `class_id_offset_in_obj` is 0 by contract);
    /// the latter two are nullable optional helpers (the JIT falls back to
    /// the unconditional `new_object` call when either is zero).
    pub fn validate(&self) -> bool {
        let ptrs = self.all_pointers();
        ptrs.iter().all(|&p| p != 0)
    }

    /// Return a list of field names whose pointer value is null (zero).
    pub fn null_pointers(&self) -> Vec<&'static str> {
        let names = Self::field_names();
        let ptrs = self.all_pointers();
        names
            .iter()
            .zip(ptrs.iter())
            .filter(|(_, &p)| p == 0)
            .map(|(&name, _)| name)
            .collect()
    }

    /// Collect all pointer values into an array for bulk validation.
    ///
    /// **Maintenance contract.** If you add a new function-pointer field
    /// above, you MUST:
    ///   1. Add it to the array literal below.
    ///   2. Add its name to the parallel array in [`Self::field_names`].
    ///   3. Bump [`Self::NUM_FIELDS`] by one.
    ///
    /// The return type `[usize; NUM_FIELDS]` enforces parts (1) and (3)
    /// at compile time — adding a field to the struct without extending
    /// the array would produce a `[usize; N]` value where `N != NUM_FIELDS`
    /// and the type mismatch refuses to compile. Part (2) (the parallel
    /// `field_names` array) is similarly type-pinned. No runtime assert
    /// can express this constraint without becoming tautological, so we
    /// rely on the type system instead.
    ///
    /// TODO(round-9): macro this so the field list lives in exactly one
    /// place — both arrays would then be generated from a single
    /// declarative source of truth.
    fn all_pointers(&self) -> [usize; Self::NUM_FIELDS] {
        let arr = [
            self.newarray,
            self.new_object,
            self.anewarray_object,
            self.baload,
            self.bastore,
            self.iaload,
            self.iastore,
            self.aaload,
            self.aastore,
            self.multianewarray_2d,
            self.arraylength,
            self.getfield,
            self.putfield_int,
            self.putfield_long,
            self.putfield_float,
            self.putfield_double,
            self.putfield_object,
            self.getstatic,
            self.putstatic_int,
            self.putstatic_long,
            self.putstatic_float,
            self.putstatic_double,
            self.putstatic_object,
            self.checkcast,
            self.instanceof_check,
            self.throw_aioobe,
            self.invoke_dispatch,
            self.invoke_virtual_mic,
            self.write_barrier,
            self.satb_pre_write_barrier,
            self.uncommon_trap,
            self.math_fma_double,
            self.math_fma_float,
        ];
        // Parallel-array length is type-pinned by the return signature
        // `[usize; NUM_FIELDS]` — see the maintenance-contract docstring
        // above. A runtime `debug_assert_eq!(arr.len(), NUM_FIELDS)` here
        // would be tautological since `arr.len() == NUM_FIELDS` is true
        // by construction at compile time.
        arr
    }

    fn field_names() -> [&'static str; Self::NUM_FIELDS] {
        let arr = [
            "newarray",
            "new_object",
            "anewarray_object",
            "baload",
            "bastore",
            "iaload",
            "iastore",
            "aaload",
            "aastore",
            "multianewarray_2d",
            "arraylength",
            "getfield",
            "putfield_int",
            "putfield_long",
            "putfield_float",
            "putfield_double",
            "putfield_object",
            "getstatic",
            "putstatic_int",
            "putstatic_long",
            "putstatic_float",
            "putstatic_double",
            "putstatic_object",
            "checkcast",
            "instanceof_check",
            "throw_aioobe",
            "invoke_dispatch",
            "invoke_virtual_mic",
            "write_barrier",
            "satb_pre_write_barrier",
            "uncommon_trap",
            "math_fma_double",
            "math_fma_float",
        ];
        // Length type-pinned by `[&'static str; NUM_FIELDS]`. See
        // `all_pointers` for the maintenance contract.
        arr
    }
}

// AUDIT 2026-05-16: JitRuntimeHelpersBuilder has been deleted. It was
// unused — the real construction site is `vm/src/jit/helpers.rs` (struct-
// literal init) and the only callers of the Builder were this crate's own
// tests. The stringly-typed `set(name: &str, addr: usize)` silently
// `eprintln!`-degraded on typos, providing no compile-time safety while
// duplicating the 32-field list across five call sites. If a future
// caller wants a builder pattern, use the struct literal directly or
// generate it from a declarative macro keyed on `field_names()`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_cached_method() -> CachedBytecodeMethod {
        CachedBytecodeMethod {
            declaring_class_id: ClassId::new(1),
            class_name: Arc::from("java/lang/Object"),
            method_name: Arc::from("hashCode"),
            method_descriptor: Arc::from("()I"),
            source_file: Some(Arc::from("Object.java")),
            code: Arc::from(vec![0xB1u8].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        }
    }

    fn make_helpers() -> JitRuntimeHelpers {
        JitRuntimeHelpers {
            newarray: 0x1000,
            new_object: 0x1008,
            anewarray_object: 0x1010,
            baload: 0x1018,
            bastore: 0x1020,
            iaload: 0x1028,
            iastore: 0x1030,
            aaload: 0x1038,
            aastore: 0x1040,
            multianewarray_2d: 0x1048,
            arraylength: 0x1050,
            getfield: 0x1058,
            putfield_int: 0x1060,
            putfield_long: 0x1068,
            putfield_float: 0x1070,
            putfield_double: 0x1078,
            putfield_object: 0x1080,
            getstatic: 0x1088,
            putstatic_int: 0x1090,
            putstatic_long: 0x1098,
            putstatic_float: 0x10A0,
            putstatic_double: 0x10A8,
            putstatic_object: 0x10B0,
            checkcast: 0x10B8,
            instanceof_check: 0x10C0,
            throw_aioobe: 0x10C8,
            invoke_dispatch: 0x10D0,
            invoke_virtual_mic: 0x10D8,
            write_barrier: 0x10E0,
            satb_pre_write_barrier: 0x1110,
            uncommon_trap: 0x10E8,
            math_fma_double: 0x10F0,
            math_fma_float: 0x10F8,
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 8,
            class_id_offset_in_obj: 0,
            get_current_thread: 0x1100,
            tlab_post_init: 0x1108,
        }
    }

    // --- CachedBytecodeMethod ---

    #[test]
    fn test_cached_method_construction() {
        let m = make_cached_method();
        assert_eq!(m.declaring_class_id, ClassId::new(1));
        assert_eq!(&*m.class_name, "java/lang/Object");
        assert_eq!(&*m.method_name, "hashCode");
        assert_eq!(&*m.method_descriptor, "()I");
        assert_eq!(m.max_stack, 2);
        assert_eq!(m.max_locals, 1);
        assert_eq!(m.num_params, 0);
    }

    #[test]
    fn test_cached_method_source_file_some() {
        let m = make_cached_method();
        assert!(m.source_file.is_some());
        assert_eq!(&*m.source_file.unwrap(), "Object.java");
    }

    #[test]
    fn test_cached_method_source_file_none() {
        let m = CachedBytecodeMethod {
            source_file: None,
            ..make_cached_method()
        };
        assert!(m.source_file.is_none());
    }

    #[test]
    fn test_cached_method_code_content() {
        let m = make_cached_method();
        assert_eq!(m.code.len(), 1);
        assert_eq!(m.code[0], 0xB1); // return opcode
    }

    #[test]
    fn test_cached_method_empty_exception_table() {
        let m = make_cached_method();
        assert!(m.exception_table.is_empty());
    }

    #[test]
    fn test_cached_method_with_exception_table() {
        let entry = ExceptionTableEntry {
            start_pc: 0,
            end_pc: 10,
            handler_pc: 20,
            catch_type: 5,
        };
        let m = CachedBytecodeMethod {
            exception_table: Arc::from(vec![entry].as_slice()),
            ..make_cached_method()
        };
        assert_eq!(m.exception_table.len(), 1);
        assert_eq!(m.exception_table[0].start_pc, 0);
        assert_eq!(m.exception_table[0].end_pc, 10);
        assert_eq!(m.exception_table[0].handler_pc, 20);
        assert_eq!(m.exception_table[0].catch_type, 5);
    }

    #[test]
    fn test_cached_method_clone() {
        let m1 = make_cached_method();
        let m2 = m1.clone();
        assert_eq!(&*m1.class_name, &*m2.class_name);
        assert_eq!(&*m1.method_name, &*m2.method_name);
        assert_eq!(m1.max_stack, m2.max_stack);
        assert_eq!(m1.max_locals, m2.max_locals);
        assert_eq!(m1.code.len(), m2.code.len());
    }

    #[test]
    fn test_cached_method_arc_sharing() {
        let m1 = make_cached_method();
        let m2 = m1.clone();
        // Arc::clone shares the same allocation
        assert!(Arc::ptr_eq(&m1.class_name, &m2.class_name));
        assert!(Arc::ptr_eq(&m1.code, &m2.code));
    }

    #[test]
    fn test_cached_method_large_code() {
        let code: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let m = CachedBytecodeMethod {
            code: Arc::from(code.as_slice()),
            max_stack: 100,
            max_locals: 50,
            ..make_cached_method()
        };
        assert_eq!(m.code.len(), 1024);
        assert_eq!(m.max_stack, 100);
        assert_eq!(m.max_locals, 50);
    }

    // --- JitRuntimeHelpers ---

    #[test]
    fn test_helpers_construction() {
        let h = make_helpers();
        assert_eq!(h.newarray, 0x1000);
        assert_eq!(h.new_object, 0x1008);
        assert_eq!(h.write_barrier, 0x10E0);
    }

    #[test]
    fn test_helpers_copy() {
        let h1 = make_helpers();
        let h2 = h1; // Copy
        assert_eq!(h1.newarray, h2.newarray);
        assert_eq!(h1.invoke_dispatch, h2.invoke_dispatch);
    }

    #[test]
    fn test_helpers_clone() {
        let h1 = make_helpers();
        let h2 = h1.clone();
        assert_eq!(h1.checkcast, h2.checkcast);
        assert_eq!(h1.instanceof_check, h2.instanceof_check);
    }

    #[test]
    fn test_helpers_all_fields_distinct() {
        let h = make_helpers();
        let ptrs = [
            h.newarray, h.new_object, h.anewarray_object,
            h.baload, h.bastore, h.iaload, h.iastore,
            h.aaload, h.aastore, h.multianewarray_2d,
            h.arraylength, h.getfield,
            h.putfield_int, h.putfield_long, h.putfield_float,
            h.putfield_double, h.putfield_object,
            h.getstatic,
            h.putstatic_int, h.putstatic_long, h.putstatic_float,
            h.putstatic_double, h.putstatic_object,
            h.checkcast, h.instanceof_check, h.throw_aioobe,
            h.invoke_dispatch, h.invoke_virtual_mic, h.write_barrier,
            h.satb_pre_write_barrier,
        ];
        // All addresses should be unique
        let mut set = std::collections::HashSet::new();
        for p in &ptrs {
            assert!(set.insert(p), "Duplicate pointer value: {:#x}", p);
        }
        assert_eq!(set.len(), 30);
    }

    #[test]
    fn test_helpers_zero_values() {
        let h = JitRuntimeHelpers {
            newarray: 0,
            new_object: 0,
            anewarray_object: 0,
            baload: 0,
            bastore: 0,
            iaload: 0,
            iastore: 0,
            aaload: 0,
            aastore: 0,
            multianewarray_2d: 0,
            arraylength: 0,
            getfield: 0,
            putfield_int: 0,
            putfield_long: 0,
            putfield_float: 0,
            putfield_double: 0,
            putfield_object: 0,
            getstatic: 0,
            putstatic_int: 0,
            putstatic_long: 0,
            putstatic_float: 0,
            putstatic_double: 0,
            putstatic_object: 0,
            checkcast: 0,
            instanceof_check: 0,
            throw_aioobe: 0,
            invoke_dispatch: 0,
            invoke_virtual_mic: 0,
            write_barrier: 0,
            satb_pre_write_barrier: 0,
            uncommon_trap: 0,
            math_fma_double: 0,
            math_fma_float: 0,
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 0,
            class_id_offset_in_obj: 0,
            get_current_thread: 0,
            tlab_post_init: 0,
        };
        assert_eq!(h.newarray, 0);
        assert_eq!(h.write_barrier, 0);
    }

    #[test]
    fn test_helpers_field_access_all() {
        let h = make_helpers();
        // Verify every field is accessible and has the expected value
        assert_eq!(h.anewarray_object, 0x1010);
        assert_eq!(h.baload, 0x1018);
        assert_eq!(h.bastore, 0x1020);
        assert_eq!(h.iaload, 0x1028);
        assert_eq!(h.iastore, 0x1030);
        assert_eq!(h.aaload, 0x1038);
        assert_eq!(h.aastore, 0x1040);
        assert_eq!(h.multianewarray_2d, 0x1048);
        assert_eq!(h.arraylength, 0x1050);
        assert_eq!(h.getfield, 0x1058);
        assert_eq!(h.putfield_int, 0x1060);
        assert_eq!(h.putfield_long, 0x1068);
        assert_eq!(h.putfield_float, 0x1070);
        assert_eq!(h.putfield_double, 0x1078);
        assert_eq!(h.putfield_object, 0x1080);
        assert_eq!(h.getstatic, 0x1088);
        assert_eq!(h.putstatic_int, 0x1090);
        assert_eq!(h.putstatic_long, 0x1098);
        assert_eq!(h.putstatic_float, 0x10A0);
        assert_eq!(h.putstatic_double, 0x10A8);
        assert_eq!(h.putstatic_object, 0x10B0);
        assert_eq!(h.checkcast, 0x10B8);
        assert_eq!(h.instanceof_check, 0x10C0);
        assert_eq!(h.throw_aioobe, 0x10C8);
        assert_eq!(h.invoke_dispatch, 0x10D0);
        assert_eq!(h.invoke_virtual_mic, 0x10D8);
    }

    #[test]
    fn test_cached_method_multiple_exception_entries() {
        let entries = vec![
            ExceptionTableEntry { start_pc: 0, end_pc: 5, handler_pc: 10, catch_type: 1 },
            ExceptionTableEntry { start_pc: 5, end_pc: 15, handler_pc: 20, catch_type: 2 },
            ExceptionTableEntry { start_pc: 0, end_pc: 15, handler_pc: 30, catch_type: 0 }, // finally
        ];
        let m = CachedBytecodeMethod {
            exception_table: Arc::from(entries.as_slice()),
            ..make_cached_method()
        };
        assert_eq!(m.exception_table.len(), 3);
        assert_eq!(m.exception_table[2].catch_type, 0); // finally handler
    }

    #[test]
    fn test_cached_method_declaring_class_id_equality() {
        let m1 = make_cached_method();
        let m2 = CachedBytecodeMethod {
            declaring_class_id: ClassId::new(2),
            ..make_cached_method()
        };
        assert_eq!(m1.declaring_class_id, ClassId::new(1));
        assert_eq!(m2.declaring_class_id, ClassId::new(2));
        assert_ne!(m1.declaring_class_id, m2.declaring_class_id);
    }

    // --- JitRuntimeHelpers validation ---

    #[test]
    fn test_helpers_validate_all_nonzero() {
        let h = make_helpers();
        assert!(h.validate());
    }

    #[test]
    fn test_helpers_validate_fails_on_zero() {
        let mut h = make_helpers();
        h.newarray = 0;
        assert!(!h.validate());
    }

    #[test]
    fn test_helpers_null_pointers_empty_when_valid() {
        let h = make_helpers();
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn test_helpers_null_pointers_reports_zeroed_fields() {
        let mut h = make_helpers();
        h.newarray = 0;
        h.write_barrier = 0;
        let nulls = h.null_pointers();
        assert!(nulls.contains(&"newarray"));
        assert!(nulls.contains(&"write_barrier"));
        assert_eq!(nulls.len(), 2);
    }

    // --- Builder tests deleted in 2026-05-16 audit: Builder itself
    //     was deleted (see comment above the deleted block in the
    //     module body). The remaining tests cover the runtime-helpers
    //     struct + validation directly.
}
