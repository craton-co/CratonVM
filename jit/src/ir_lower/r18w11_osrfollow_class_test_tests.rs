use super::*;

/// Every jump of the fused class test lands where its comment says: the
/// null arm on `OR EAX, 1`, the non-plain arm and the compare's exit on the
/// end, and the class id is the compare's immediate.
#[test]
fn the_fused_class_test_jumps_land_on_their_targets() {
    let b = exact_class_test_flags_bytes(0x1234_5678);
    let target = |at: usize| at + 2 + usize::from(b[at + 1]);
    assert_eq!(&b[..3], &[0x48, 0x85, 0xC0]);
    assert_eq!(b[3], 0x74, "JZ");
    assert_eq!(
        &b[target(3)..target(3) + 3],
        &[0x83, 0xC8, 0x01],
        "null -> OR EAX, 1"
    );
    assert_eq!(b[9], 0x75, "JNE");
    assert_eq!(target(9), b.len(), "non-plain -> done with ZF=0");
    assert_eq!(&b[11..14], &[0x81, 0x78, 0x00]);
    assert_eq!(&b[14..18], &0x1234_5678u32.to_le_bytes());
    assert_eq!(b[18], 0xEB, "JMP");
    assert_eq!(target(18), b.len(), "compare -> done");
    assert_eq!(b[7], cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8);
    assert_eq!(b[8], cratonvm_types::KIND_TAG_BYTE_MASK);
}

// Default ON (`=0` kills), and only the `If` arm reads a fused `ExactClassIs`: its own arm
// emits nothing when fused, and the branch takes `JE`.
