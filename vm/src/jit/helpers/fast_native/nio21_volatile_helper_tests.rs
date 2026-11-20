use super::*;
use crate::config::VmConfig;

/// The fast arms' target screens decline every input the registered
/// native answers from ANOTHER arm (null base, synthetic band, a word that
/// is not a heap object), and accept an instance field slot / an aligned
/// in-bounds reference-array element.
#[test]
fn nio21_volatile_targets_decline_what_the_native_answers_elsewhere() {
    let shared = SharedVm::new(VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(cid) = cm.try_ensure_synthetic_class("cratonvm/test/Nio21VolatileHolder", 2)
        else {
            panic!("Compatible mode fabricates this fixture");
        };
        cid
    };
    let obj = shared.mem.heap.alloc_object(cid, 2);
    let raw = obj.as_ptr() as i64;
    let array = shared
        .mem
        .heap
        .alloc_array(cid, ArrayElementType::Reference, 4);
    let array_raw = array.as_ptr() as i64;
    // SAFETY: both words are live objects of this heap, or deliberately
    // not objects, which the screens are there to refuse.
    unsafe {
        assert!(
            unsafe_volatile_field_target(&shared, 0, 1).is_none(),
            "null base"
        );
        assert!(
            unsafe_volatile_field_target(&shared, raw, 0x2000_0000).is_none(),
            "synthetic"
        );
        assert!(unsafe_volatile_field_target(&shared, raw, -1).is_none());
        assert!(
            unsafe_volatile_field_target(&shared, raw + 4, 1).is_none(),
            "misaligned"
        );
        // Slot 1's Unsafe offset: `16 + 8` under the default
        // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (HotSpot byte offsets), the
        // slot index itself with `=0`.
        let slot1 = if shared.classes.unsafe_byte_field_offsets {
            24
        } else {
            1
        };
        let (target, slot) =
            unsafe_volatile_field_target(&shared, raw, slot1).expect("an instance field slot");
        assert_eq!(slot, 1);
        assert_eq!(target.as_ptr(), obj.as_ptr());
        assert!(
            unsafe_volatile_field_target(&shared, array_raw, 16).is_none(),
            "array base"
        );

        assert!(
            unsafe_reference_array_slot(&shared, raw, 16).is_none(),
            "not an array"
        );
        assert!(
            unsafe_reference_array_slot(&shared, array_raw, 8).is_none(),
            "below ABASE"
        );
        assert!(
            unsafe_reference_array_slot(&shared, array_raw, 20).is_none(),
            "unaligned"
        );
        assert!(
            unsafe_reference_array_slot(&shared, array_raw, 16 + 8 * 4).is_none(),
            "OOB"
        );
        let (a, index) =
            unsafe_reference_array_slot(&shared, array_raw, 16 + 8 * 3).expect("element 3");
        assert_eq!((a.as_ptr(), index), (array.as_ptr(), 3));

        assert_eq!(
            unsafe_reference_operand(&shared, 0),
            Some(Value::Object(None))
        );
        assert!(unsafe_reference_operand(&shared, 8).is_none());
        assert!(unsafe_reference_operand(&shared, raw + 1).is_none());
        assert_eq!(
            unsafe_reference_operand(&shared, raw),
            Some(Value::Object(Some(obj)))
        );
    }
}
