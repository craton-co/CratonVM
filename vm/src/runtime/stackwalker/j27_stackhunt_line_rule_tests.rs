use super::line_number_for_bci_in_method;
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute, LineNumberEntry};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use std::sync::Arc;

/// A method with an eight-byte body (`J27CompiledStackRows`'s generated
/// `new; dup; invokespecial; athrow`) and the given tables, in order.
fn method(tables: &[&[(u16, u16)]]) -> ClassFileMethod {
    let attributes = tables
        .iter()
        .map(|rows| {
            Attribute::LineNumberTable(
                rows.iter()
                    .map(|&(start_pc, line_number)| LineNumberEntry {
                        start_pc,
                        line_number,
                    })
                    .collect(),
            )
        })
        .collect();
    let code = CodeAttribute {
        max_stack: 2,
        max_locals: 0,
        code: cratonvm_reader::ByteView::from_vec(vec![
            0xbb, 0, 6, 0x59, 0xb7, 0, 10, 0xbf,
        ]),
        exception_table: Vec::new(),
        attributes,
    };
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from("dupExact"),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(code)))],
    }
}

/// Two rows at the trapping pc: HotSpot answers the FIRST (201); the
/// scan used to keep the last (202).
#[test]
fn the_first_row_at_an_exact_pc_wins() {
    let m = method(&[&[(0, 100), (4, 201), (4, 202)]]);
    assert_eq!(line_number_for_bci_in_method(&m, 4), Some(201));
    assert_eq!(line_number_for_bci_in_method(&m, 0), Some(100));
}

/// Below the bci the LAST of equal starts wins, as HotSpot's `>=`.
#[test]
fn the_last_of_equal_starts_below_the_pc_wins() {
    let m = method(&[&[(0, 301), (0, 302)]]);
    assert_eq!(line_number_for_bci_in_method(&m, 4), Some(302));
    let m = method(&[&[(0, 100), (4, 201), (4, 202)]]);
    assert_eq!(line_number_for_bci_in_method(&m, 7), Some(202));
}

/// The union of several tables is read in attribute order, so the exact
/// row of the earlier attribute wins; a pc before every row has none.
#[test]
fn tables_are_read_in_attribute_order() {
    let m = method(&[&[(4, 401)], &[(4, 402), (0, 400)]]);
    assert_eq!(line_number_for_bci_in_method(&m, 4), Some(401));
    assert_eq!(line_number_for_bci_in_method(&m, 3), Some(400));
    let m = method(&[&[(4, 1)]]);
    assert_eq!(line_number_for_bci_in_method(&m, 3), None);
}

/// j27 wave 5 (lane REFSP): HotSpot answers `-1` for a bci at or past
/// `code_size()`; the scan used to answer the last row there.
#[test]
fn a_bci_past_the_code_has_no_line() {
    let m = method(&[&[(0, 100), (4, 201)]]);
    assert_eq!(line_number_for_bci_in_method(&m, 7), Some(201), "the last pc");
    assert_eq!(line_number_for_bci_in_method(&m, 8), None, "code_size");
    assert_eq!(line_number_for_bci_in_method(&m, 4000), None);
}
