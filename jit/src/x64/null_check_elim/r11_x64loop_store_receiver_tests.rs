use super::{array_receiver_with_starts, bytecode_analysis};

/// Every javac store shape answers `None`: the receiver of a store is three
/// stack slots deep, and the decode looks two instructions back. This pins
/// the fact the store elision's page is about, so a decoder that starts
/// matching stores has to come with the three-pc merge check.
#[test]
fn javac_array_store_shapes_never_name_a_receiver() {
    #[rustfmt::skip]
    let shapes: [&[u8]; 3] = [
        // aload_1; iload_2; iload_3; iastore       a[i] = v
        &[0x2b, 0x1c, 0x1d, 0x4f],
        // aload_1; iconst_0; iconst_5; iastore     a[0] = 5
        &[0x2b, 0x03, 0x08, 0x4f],
        // aload_1; iload_2; aload_3; aastore       a[i] = o
        &[0x2b, 0x1c, 0x2d, 0x53],
    ];
    for code in shapes {
        let starts = bytecode_analysis::instruction_starts(code, code.len());
        // `pc < code.len()` is required, so give the store a successor.
        let mut padded = code.to_vec();
        padded.push(0xb1);
        let starts_p = bytecode_analysis::instruction_starts(&padded, padded.len());
        assert_eq!(starts.len(), code.len());
        assert_eq!(
            array_receiver_with_starts(&padded, code.len() - 1, &starts_p),
            None,
            "{code:02x?}"
        );
    }
}
