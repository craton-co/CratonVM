// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What a hand-emitted thunk is allowed to assume about a lambda proxy's
//! captured field.
//!
//! `jit::lambda_adapter` emits a per-(proxy class, impl) thunk that reads a
//! capturing lambda's captures straight out of the proxy object and tail-jumps
//! to the implementation. It has no frame, no helper call and no way to ask a
//! question at run time, so it bakes THREE decisions:
//!
//!   * the ADDRESS — `HEADER_SIZE + i * SLOT_SIZE + payload`, the uniform
//!     16-byte `Value` cell, because a lambda proxy never has a registered
//!     `CompactLayout` (its class id comes from `alloc_lambda_proxy_id`,
//!     `0x8000_0000` upward) and `plan_object_alloc` sets `GC_FLAG_COMPACT`
//!     only when a layout matches;
//!   * the WIDTH — 8 bytes at `+8` for a reference, `long` or `double`, 4 bytes
//!     at `+4` for everything else;
//!   * and, for a REFERENCE, that the word it finds there is a plain machine
//!     pointer needing no decode and no load barrier.
//!
//! The third was originally not trusted. `lambda_adapter_entry` refused a
//! reference capture whenever `narrow_oops_block_inline_fields()` held —
//! compressed oops on, or ZGC's read barrier armed — by analogy with the inline
//! `getfield` codegen, which does refuse under exactly that condition.
//!
//! **The analogy was wrong, and this file is why.** That gate protects the
//! COMPACT slot emission: a compact reference slot is narrowed to 4 bytes under
//! compressed oops, and ZGC's colouring applies to the paths that decode one. A
//! legacy 16-byte `Value` cell is neither. It holds a Rust `Value` verbatim,
//! whose `Object` payload is a full `ObjectRef`; and ZGC's own
//! `get_field` reads a legacy cell with a bare `std::ptr::read::<Value>` — its
//! load barrier is applied on `get_array_element`, not here. So the interpreter
//! fast path that the gate diverted captures to reads the very same word, the
//! very same way. The refusal bought nothing.
//!
//! # Why this is a test and not a comment
//!
//! Removing a safety gate on the strength of a code reading is exactly the move
//! that should be pinned executably, because the two facts it rests on live in
//! files nobody editing them would think to connect to a JIT thunk. So every
//! assertion below compares the thunk's baked decision against the COLLECTOR'S
//! OWN accessor, for every backend, across the flag settings the gate named.
//! If a future change narrows a legacy cell, or teaches `get_field` to barrier
//! one, these fail and name the thunk.
//!
//! It is deliberately NOT a test that reimplements `get_field` and checks it
//! agrees with itself: `raw_capture_word` below is the emitter's arithmetic,
//! written out, and the other side of every assertion is the real heap API.

use cratonvm_gc::collector::MonitorCleanup;
use cratonvm_gc::{GcBackend, VmHeap};
use cratonvm_types::{
    is_compact_object, ClassId, ObjectRef, Value, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, HEADER_SIZE, SLOT_SIZE,
};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const HEAP_BYTES: usize = 8 * 1024 * 1024;

/// A synthetic id in the range `alloc_lambda_proxy_id` hands out. Nothing
/// registers a layout for it, which is the property under test.
const PROXY_CLASS: u32 = 0x8000_0007;

/// A plain class for the captured object to be an instance of.
const PAYLOAD_CLASS: u32 = 0xD00D;

fn backends() -> Vec<(&'static str, GcBackend)> {
    let mut v = vec![("gen_heap", GcBackend::Generational), ("g1", GcBackend::G1)];
    #[cfg(feature = "zgc")]
    v.push(("zgc", GcBackend::Zgc));
    v
}

/// **The emitter's arithmetic, written out in Rust.**
///
/// `jit::lambda_adapter::capture_payload_offset` computes exactly this, and
/// `emit_load_mem` performs exactly this load. Keep the two in step by hand —
/// sharing a constant would make the assertions below compare the emitter with
/// itself.
///
/// # Safety
/// `obj` must be a live legacy-laid-out object with more than `index` fields.
unsafe fn raw_capture_word(obj: ObjectRef, index: usize, wide: bool) -> u64 {
    let cell = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
    if wide {
        // MOV r64, [base + HEADER_SIZE + i*SLOT_SIZE + 8]
        std::ptr::read_unaligned(cell.add(FIELD_CELL_PAYLOAD64_OFFSET) as *const u64)
    } else {
        // MOV r32, [base + HEADER_SIZE + i*SLOT_SIZE + 4] (zero-extended)
        std::ptr::read_unaligned(cell.add(FIELD_CELL_PAYLOAD32_OFFSET) as *const u32) as u64
    }
}

/// A proxy carrying `captures`, allocated exactly as `allocate_lambda_proxy`
/// allocates one: `try_alloc_object(proxy_class_id, num_captures)` with no
/// registered layout.
fn proxy_with(heap: &VmHeap, captures: &[Value]) -> ObjectRef {
    let obj = heap.alloc_object(ClassId::new(PROXY_CLASS), captures.len());
    for (i, v) in captures.iter().enumerate() {
        heap.set_field(obj, i, *v);
    }
    obj
}

/// The premise everything else rests on: a proxy is LEGACY on every backend.
///
/// Asserted rather than assumed, and asserted first, because if a proxy were
/// ever compact then every later assertion here would be comparing two readings
/// of the wrong object and would still agree with each other.
#[test]
fn a_lambda_proxy_is_legacy_laid_out_on_every_backend() {
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let obj = proxy_with(&heap, &[Value::Int(1), Value::Long(2)]);
        let header = heap.get_header(obj);
        assert!(
            !is_compact_object(header),
            "{name}: a lambda proxy carried GC_FLAG_COMPACT. The thunk's baked \
             `HEADER_SIZE + i*SLOT_SIZE` addressing is wrong for it, and \
             `lambda_adapter_entry`'s `class_layout_for_fields` refusal is the \
             gate that is supposed to have caught this — check that it still \
             runs before believing any other test in this file.",
        );
    }
}

/// A REFERENCE capture: the word the thunk loads is the pointer the collector
/// reports, with no decode and no barrier.
///
/// This is the assertion that licenses removing
/// `narrow_oops_block_inline_fields()` from the emitter's reference arm.
#[test]
fn a_reference_capture_is_a_plain_pointer_at_the_wide_payload() {
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let payload = heap.alloc_object(ClassId::new(PAYLOAD_CLASS), 1);
        // Capture 1, not 0, so a thunk that ignored the index would read the
        // `Int` cell and fail here rather than pass by luck.
        let obj = proxy_with(
            &heap,
            &[Value::Int(0x1234_5678), Value::Object(Some(payload))],
        );

        let via_heap = match heap.get_field(obj, 1) {
            Value::Object(Some(r)) => r.as_ptr() as u64,
            other => panic!("{name}: capture 1 read back as {other:?}, not a reference"),
        };
        // SAFETY: `obj` is live, legacy (asserted above) and has two fields.
        let via_thunk = unsafe { raw_capture_word(obj, 1, true) };

        assert_eq!(
            via_thunk, via_heap,
            "{name}: the thunk's raw 8-byte load at the legacy cell's wide \
             payload does not equal the pointer `get_field` reports. Either a \
             legacy reference cell is now narrowed or encoded, or this \
             collector's `get_field` now decodes/barriers one — in both cases \
             `jit::lambda_adapter` must go back to refusing reference captures \
             (`narrow_oops_block_inline_fields`).",
        );
        assert_eq!(
            via_thunk,
            payload.as_ptr() as u64,
            "{name}: ...and the pointer must be the object that was stored, so \
             that an agreement between two equally-wrong readings cannot pass.",
        );
    }
}

/// A null capture must be a zero word — the thunk passes it straight through as
/// the impl's `null` argument, and there is no encoding in which a non-zero
/// word would be right.
#[test]
fn a_null_reference_capture_is_a_zero_word() {
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let obj = proxy_with(&heap, &[Value::Object(None)]);
        assert_eq!(heap.get_field(obj, 0), Value::Object(None), "{name}: setup");
        // SAFETY: live, legacy, one field.
        assert_eq!(
            unsafe { raw_capture_word(obj, 0, true) },
            0,
            "{name}: a null capture is not a zero word, so the thunk would hand \
             the implementation a wild pointer where it expects `null`",
        );
    }
}

/// The other three loads, against the collector's own answers, in one object so
/// the capture INDEX is exercised alongside the widths.
#[test]
fn every_capture_width_matches_the_collectors_reading() {
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let obj = proxy_with(
            &heap,
            &[
                Value::Long(0x0102_0304_0506_0708),
                Value::Int(-100),
                Value::Double(f64::consts_pi()),
                Value::Float(0.15625),
            ],
        );

        // `long` — the wide payload, all 64 bits.
        // SAFETY: live, legacy, four fields (applies to each read below).
        assert_eq!(
            unsafe { raw_capture_word(obj, 0, true) },
            0x0102_0304_0506_0708u64,
            "{name}: `long` capture",
        );
        // `int` — the narrow payload. The thunk sign-extends; this compares the
        // 32 bits that carry the value, which is what the callee reads.
        assert_eq!(
            unsafe { raw_capture_word(obj, 1, false) } as u32,
            (-100i32) as u32,
            "{name}: `int` capture",
        );
        // `double` — raw bits at the wide payload, which is how the JIT ABI
        // carries one.
        assert_eq!(
            unsafe { raw_capture_word(obj, 2, true) },
            f64::consts_pi().to_bits(),
            "{name}: `double` capture",
        );
        // `float` — its bits, zero-extended out of the narrow payload.
        assert_eq!(
            unsafe { raw_capture_word(obj, 3, false) },
            0.15625f32.to_bits() as u64,
            "{name}: `float` capture",
        );

        // …and the collector agrees about each, so the reads above are of the
        // values that were stored rather than of whatever happened to be there.
        assert_eq!(heap.get_field(obj, 0), Value::Long(0x0102_0304_0506_0708));
        assert_eq!(heap.get_field(obj, 1), Value::Int(-100));
        assert_eq!(heap.get_field(obj, 2), Value::Double(f64::consts_pi()));
        assert_eq!(heap.get_field(obj, 3), Value::Float(0.15625));
    }
}

/// **Compressed oops on**: a legacy cell's reference payload is still a full
/// 8-byte pointer at `+8`.
///
/// This is the first of the two conditions
/// `narrow_oops_block_inline_fields()` names, and the reason the gate does not
/// apply here: `narrow_oop::ref_field_size()` is documented as the width of a
/// *compact* reference instance field, and a legacy cell holds a Rust `Value`
/// whose `Object` payload is an `ObjectRef` — no encoder is reached on either
/// the write or the read.
///
/// Runs LAST-ish and restores the flag, because it is process-wide.
#[test]
fn compressed_oops_do_not_narrow_a_legacy_capture_cell() {
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let payload = heap.alloc_object(ClassId::new(PAYLOAD_CLASS), 1);

        // Enable around the store AND the read: a narrowing encoder would be
        // reached by whichever of the two consults the flag.
        let base = payload.as_ptr() as u64 & !0xFFF_FFFF;
        let enabled = cratonvm_types::narrow_oop::enable(base, 3);
        let obj = proxy_with(&heap, &[Value::Int(7), Value::Object(Some(payload))]);
        // SAFETY: live, legacy, two fields.
        let via_thunk = unsafe { raw_capture_word(obj, 1, true) };
        let via_heap = heap.get_field(obj, 1);
        let width = cratonvm_types::narrow_oop::ref_field_size();
        cratonvm_types::narrow_oop::disable_for_test();

        assert!(
            enabled,
            "{name}: could not turn compressed oops on, so this test asserted \
             nothing about them — it must not report a pass",
        );
        assert_eq!(
            width, 4,
            "{name}: compressed oops were on but a COMPACT reference field is \
             still 8 bytes, so the condition under test was not in force",
        );
        assert_eq!(
            via_heap,
            Value::Object(Some(payload)),
            "{name}: the collector lost the reference under compressed oops",
        );
        assert_eq!(
            via_thunk,
            payload.as_ptr() as u64,
            "{name}: a legacy capture cell was narrowed under compressed oops. \
             The thunk's 8-byte load at +8 is wrong, and \
             `lambda_adapter_entry` must go back to refusing reference captures \
             when `narrow_oops_block_inline_fields()` holds.",
        );
    }
}

/// **ZGC's read barrier armed**: the field path is unchanged, so the thunk's
/// raw read still equals the collector's.
///
/// The second condition the gate names. ZGC applies `load_barrier_slot` on
/// `get_array_element`; `get_field` — compact arm and legacy arm alike — does
/// not. Arming therefore changes nothing about a captured FIELD, which is why a
/// thunk baked while disarmed stays correct if a cycle arms later.
///
/// If someone gives `get_field` a barrier, this is the test that fails and says
/// the thunk must be re-gated.
#[test]
#[cfg(feature = "zgc")]
fn arming_zgcs_read_barrier_does_not_change_a_captured_field() {
    let heap = VmHeap::new(GcBackend::Zgc, HEAP_BYTES);
    let payload = heap.alloc_object(ClassId::new(PAYLOAD_CLASS), 1);
    let obj = proxy_with(&heap, &[Value::Int(7), Value::Object(Some(payload))]);

    // SAFETY: live, legacy, two fields.
    let disarmed_word = unsafe { raw_capture_word(obj, 1, true) };
    let disarmed_heap = heap.get_field(obj, 1);

    cratonvm_types::set_zgc_read_barrier_armed(true);
    // SAFETY: as above.
    let armed_word = unsafe { raw_capture_word(obj, 1, true) };
    let armed_heap = heap.get_field(obj, 1);
    let gate_was_on = cratonvm_types::zgc_read_barrier_armed();
    cratonvm_types::set_zgc_read_barrier_armed(false);

    assert!(
        gate_was_on,
        "the codegen gate did not come on, so nothing below was tested with a \
         barrier armed",
    );
    assert_eq!(
        armed_word, disarmed_word,
        "the raw capture word changed when the read barrier armed",
    );
    assert_eq!(
        armed_heap, disarmed_heap,
        "`get_field` answered differently with the barrier armed — it has \
         gained a decode the thunk cannot perform, and \
         `jit::lambda_adapter` must refuse reference captures again",
    );
    assert_eq!(
        armed_word,
        payload.as_ptr() as u64,
        "...and both must still be the object that was stored",
    );
}

/// Local `PI` so the file needs no float-constant import and the value is
/// obviously not a round number a wrong read could produce.
trait Pi {
    fn consts_pi() -> f64;
}
impl Pi for f64 {
    fn consts_pi() -> f64 {
        std::f64::consts::PI
    }
}
