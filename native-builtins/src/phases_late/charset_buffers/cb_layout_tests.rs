use super::*;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::ClassId;

/// On a real-JDK-shaped CharBuffer the indexed `CB_FIELD_*` writes must
/// not happen at all: slot 0 is `mark` and slot 4 is `address`.
///
/// The `mark` half is what `Buffer.<init>`'s `mark > position` check
/// caught. Before the guard, `cb_write_hb` left the backing `char[]`'s
/// reference sitting in `mark`, and `CharBuffer.allocate(12).duplicate()`
/// — which passes `markValue()` straight into the constructor — threw
/// `IllegalArgumentException: mark > position: (52784352 > 0)`.
///
/// Mirrors `servlet.rs::bb_write_hb_real_layout_preserves_address_and_mark`,
/// which pins the identical hazard on the ByteBuffer side.
#[test]
fn cb_write_hb_real_layout_preserves_address_and_mark() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("java/nio/CharBuffer")
        .expect("class init");
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, 12);
    let buf = ctx.alloc_object(class_id, 10);

    cb_write_hb(&mut ctx, buf, arr, 12);

    // Slot 0 is the real `mark`. The indexed fallback would put the
    // backing `char[]`'s reference there — the exact corruption
    // `Buffer.<init>`'s `mark > position` check surfaced. Assert on the
    // SLOTS rather than the names: this is what the guard controls, and
    // it is observable without the field-name metadata a mock context
    // does not carry for this class.
    assert_ne!(
        ctx.get_field(buf, CB_FIELD_ARRAY),
        Value::Object(Some(arr)),
        "slot 0 is the real Buffer.mark — the indexed fallback must be suppressed here"
    );
    // Slot 4 is the real `address`; the indexed fallback would put -1
    // there, which is the defect `cb_write_heap_address` exists to undo.
    assert_ne!(
        ctx.get_field(buf, CB_FIELD_MARK),
        Value::Int(-1),
        "slot 4 is the real Buffer.address — the indexed fallback must be suppressed here"
    );
}

/// The full `hasArray`/`array`/`arrayOffset` truth table, as measured on
/// HotSpot 25.0.3+9 (`probes/BufferAccessibleArrayProbe`).
///
/// Asserted on the pure decision function rather than through a
/// `MockNativeContext`: the mock resolves field NAMES through a slot
/// fallback, so a receiver-shaped test here would be measuring the mock's
/// name-to-slot table and not the ordering this function exists to fix.
/// The two inputs are exactly the two fields the JDK bodies read.
///
/// The function is now `cratonvm_native_io::buffer_array_access` and the
/// local `cb_array_access` is gone (F14-1 N1). These three tests are KEPT
/// rather than deleted as duplicates of `native-io`'s own: they assert
/// from the CONSUMER side, so a future lane that re-introduces a local
/// copy has to delete an assertion to do it, and the CharBuffer-specific
/// receivers each cell names (`StringCharBuffer`, `ByteBufferAsCharBufferB`)
/// are witnesses `native-io`'s ByteBuffer-side rows do not carry.
#[test]
fn charbuffer_natives_use_the_shared_jdk_three_way_table() {
    // HeapCharBuffer — `CharBuffer.allocate(4)`.
    assert_eq!(
        buffer_array_access(true, false),
        BufferArrayAccess::Accessible
    );
    // HeapCharBufferR — `.asReadOnlyBuffer()`. HotSpot: hasArray() == false,
    // array() throws ReadOnlyBufferException.
    assert_eq!(buffer_array_access(true, true), BufferArrayAccess::ReadOnly);
    // ByteBufferAsCharBufferB — a writable VIEW with no `hb`. HotSpot:
    // array() throws UnsupportedOperationException even though it is not
    // read-only, which is why "not accessible" cannot be one branch.
    assert_eq!(buffer_array_access(false, false), BufferArrayAccess::Absent);
}

/// The ORDER, pinned on its own because it is the single cell a
/// read-only-first implementation gets wrong.
///
/// `CharBuffer.wrap("abcd")` is a `StringCharBuffer`: read-only AND
/// array-less at the same time. The JDK checks `hb == null` FIRST, so it
/// answers `UnsupportedOperationException`, not `ReadOnlyBufferException` —
/// and the two are unrelated classes, so a `catch` cannot absorb the
/// difference. This is the row `RJdkIntrinsics2 --only=bounds` asserts.
#[test]
fn shared_table_checks_absent_before_read_only_for_stringcharbuffer() {
    assert_eq!(buffer_array_access(false, true), BufferArrayAccess::Absent);
}

/// Only ONE of the three states admits the array, and it is the one where
/// `hasArray()` is true. A mutation that made `hasArray()` derive from
/// anything but this function would let a read-only buffer hand out a
/// mutable alias to its backing store.
#[test]
fn exactly_one_state_hands_the_array_over() {
    let states = [
        buffer_array_access(false, false),
        buffer_array_access(false, true),
        buffer_array_access(true, false),
        buffer_array_access(true, true),
    ];
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == BufferArrayAccess::Accessible)
            .count(),
        1
    );
}

/// A genuinely synthetic carrier has no field-name metadata, so the
/// indexed fallback is the only way these natives round-trip state. Guard
/// against the fix above suppressing it for the case it exists to serve.
#[test]
fn cb_write_hb_pure_synthetic_layout_still_gets_indexed_fallback() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ClassId::new(9999); // never registered by name
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, 8);
    let buf = ctx.alloc_object(class_id, 5);

    cb_write_hb(&mut ctx, buf, arr, 8);

    assert_eq!(ctx.get_field(buf, CB_FIELD_ARRAY), Value::Object(Some(arr)));
    assert_eq!(ctx.get_field(buf, CB_FIELD_POS), Value::Int(0));
    assert_eq!(ctx.get_field(buf, CB_FIELD_LIMIT), Value::Int(8));
    assert_eq!(ctx.get_field(buf, CB_FIELD_CAPACITY), Value::Int(8));
    assert_eq!(ctx.get_field(buf, CB_FIELD_MARK), Value::Int(-1));
}
