// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The STRING_ACCESS intrinsics under **compressed oops**.
//!
//! Hole 1 of `gc/src/compressed_oops.rs`'s "two correctness holes":
//! `emit_load_string_value_ptr` emitted an unconditional 64-bit load of the
//! `String.value` reference field, so under narrow oops it read 4 bytes of
//! narrow oop plus 4 bytes of the adjacent `coder` and dereferenced the
//! result — a deterministic wild pointer. The stopgap was to refuse every
//! String intrinsic while narrow oops were on; the fix is the narrow arm
//! `emit_load_narrow_ref_field`, selected by
//! `StringFieldLayout::value_compact_is_narrow`.
//!
//! `cratonvm_types::narrow_oop`'s configuration is **process-wide and
//! write-once**, so this lives in its own integration-test binary (one process
//! per file) and does all its work in a single `#[test]`. Do not add a second
//! test function here that expects narrow oops to be off.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{try_resolve_string_intrinsic, JitDirectCall, StringFieldLayout};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::narrow_oop;
use cratonvm_types::{ArrayElementType, ObjectKind, HEADER_SIZE};
use std::collections::{HashMap, HashSet};

unsafe extern "C" fn noop_uncommon_trap(_vm: i64, _reason: i64, _bci: i64) -> i64 {
    0
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("narrow-oop String intrinsic test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: noop_uncommon_trap as *const () as usize,
        math_fma_double: s,
        math_fma_float: s,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// A heap object backed by a `Vec<u64>`, so the base address is 8-byte aligned.
struct FakeObj {
    storage: Vec<u64>,
}

impl FakeObj {
    fn with_bytes(total: usize) -> Self {
        FakeObj {
            storage: vec![0u64; total.div_ceil(8).max(1)],
        }
    }
    fn base(&mut self) -> *mut u8 {
        self.storage.as_mut_ptr() as *mut u8
    }
    fn ptr(&self) -> i64 {
        self.storage.as_ptr() as i64
    }
}

fn make_byte_array(data: &[u8]) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + data.len());
    let base = obj.base();
    unsafe {
        *base.add(4) = ObjectKind::Array as u8;
        *base.add(5) = ArrayElementType::Byte as u8;
        let len_le = (data.len() as u32).to_le_bytes();
        std::ptr::copy_nonoverlapping(len_le.as_ptr(), base.add(12), 4);
        std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(HEADER_SIZE), data.len());
    }
    obj
}

/// Dense-registry-safe class id — `register_class_layout` indexes a dense
/// `Vec` by class id.
const NARROW_STRING_CLASS_ID: u32 = 7;

/// The real JDK25 `java/lang/String` compact layout **at narrow widths**:
/// `value:[B` (4 bytes) at 0, `coder:B` at 4, `hash:I` at 8, `hashIsZero:Z` at
/// 12 — a 16-byte body where the wide layout needs 24.
fn register_narrow_string_layout() -> u32 {
    use cratonvm_types::{register_class_layout, CompactLayout, FieldStorageKind};
    register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        NARROW_STRING_CLASS_ID,
        std::sync::Arc::new(CompactLayout {
            field_offsets: vec![0, 4, 8, 12],
            is_ref: vec![true, false, false, false],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Byte,
                FieldStorageKind::Int,
                FieldStorageKind::Boolean,
            ],
            ref_offsets: vec![0],
            body_size: 16,
        }),
    );
    NARROW_STRING_CLASS_ID
}

/// A COMPACT `java/lang/String` whose `value` slot holds a **narrow** oop.
fn make_narrow_string(value_ptr: i64, coder: u8, hash: i32) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + 16);
    let base = obj.base();
    let narrow = narrow_oop::encode(value_ptr as u64);
    unsafe {
        std::ptr::copy_nonoverlapping(NARROW_STRING_CLASS_ID.to_le_bytes().as_ptr(), base, 4);
        *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = ObjectKind::Object as u8;
        *base.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
        std::ptr::copy_nonoverlapping(
            4u32.to_le_bytes().as_ptr(),
            base.add(cratonvm_types::NUM_SLOTS_OFFSET),
            4,
        );
        let body = base.add(HEADER_SIZE);
        std::ptr::copy_nonoverlapping(narrow.to_le_bytes().as_ptr(), body, 4);
        *body.add(4) = coder;
        std::ptr::copy_nonoverlapping(hash.to_le_bytes().as_ptr(), body.add(8), 4);
        *body.add(12) = 0;
    }
    obj
}

/// Compile `return this.<name>()` for a no-arg int-returning String intrinsic.
fn compile_unary(layout: StringFieldLayout, name: &str, descriptor: &str) -> impl Fn(i64) -> i64 {
    let entry = try_resolve_string_intrinsic("java/lang/String", name, descriptor, Some(layout))
        .unwrap_or_else(|| {
            panic!("{name}{descriptor} must still register under narrow oops — the blanket refusal is gone")
        })
        .0;
    let code: Vec<u8> = vec![0x2a, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            1,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 0,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(layout),
    )
    .unwrap_or_else(|| panic!("{name}{descriptor} failed to compile under narrow oops"));
    move |this: i64| unsafe { compiled.try_call(&[this]).expect("test JIT call") }
}

fn java_hash(s: &str) -> i32 {
    s.bytes()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

#[test]
fn string_intrinsics_decode_a_narrow_value_slot() {
    // The backing array has to be encodable, so derive the heap base from it:
    // `base = value_ptr - 8` puts the array at narrow oop 1 under shift 3.
    let latin1 = make_byte_array(b"hello world");
    let value_ptr = latin1.ptr();
    assert_eq!(value_ptr % 8, 0, "FakeObj is Vec<u64>-backed, so 8-aligned");
    let heap_base = (value_ptr as u64) - 8;
    assert!(
        narrow_oop::enable(heap_base, 3),
        "narrow-oop geometry must be accepted"
    );
    assert!(narrow_oop::narrow_oops_enabled());
    assert_eq!(
        narrow_oop::ref_field_size(),
        4,
        "a ref field is now 4 bytes"
    );
    assert_eq!(
        narrow_oop::decode(narrow_oop::encode(value_ptr as u64)),
        value_ptr as u64
    );

    let layout = StringFieldLayout::new(0, Some(1), 2, register_narrow_string_layout());
    assert_eq!(
        layout.value_compact_offset, HEADER_SIZE as i32,
        "compact offsets are exact payload addresses"
    );
    assert!(
        layout.value_compact_is_narrow,
        "a registered narrow ref field must select the narrow load arm",
    );

    let s = make_narrow_string(value_ptr, 0, 0);
    let s_ptr = s.ptr();

    // `length()` = value.length >> coder — it dereferences the decoded slot to
    // read the array header. A wide load here reads `coder`/`hash` as the top
    // half of the pointer and faults.
    let length = compile_unary(layout, "length", "()I");
    assert_eq!(length(s_ptr), 11, "length over a narrow value slot");

    let is_empty = compile_unary(layout, "isEmpty", "()Z");
    assert_eq!(is_empty(s_ptr), 0, "isEmpty over a narrow value slot");

    // `hashCode()` walks the array *elements*, so it proves the decoded base is
    // the real array and not merely a readable address.
    let hash_code = compile_unary(layout, "hashCode", "()I");
    assert_eq!(
        hash_code(s_ptr) as i32,
        java_hash("hello world"),
        "hashCode over a narrow value slot",
    );

    // An empty string: `value.length == 0`, and the narrow slot is still a
    // real (non-null) array reference.
    let empty_arr = make_byte_array(b"");
    let empty = make_narrow_string(empty_arr.ptr(), 0, 0);
    assert_eq!(length(empty.ptr()), 0, "empty length");
    assert_eq!(is_empty(empty.ptr()), 1, "empty isEmpty");

    // Keep the backing arrays alive past the last JIT call that reads them.
    drop(latin1);
    drop(empty_arr);
    drop(s);
    drop(empty);
}
