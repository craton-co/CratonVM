use super::mh_filter_line_at;
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute, LineNumberEntry};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use std::sync::Arc;

fn method(tables: Vec<Vec<(u16, u16)>>) -> ClassFileMethod {
    let code = CodeAttribute {
        max_stack: 1,
        max_locals: 1,
        code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
        exception_table: Vec::new(),
        attributes: tables
            .into_iter()
            .map(|rows| {
                Attribute::LineNumberTable(
                    rows.into_iter()
                        .map(|(start_pc, line_number)| LineNumberEntry {
                            start_pc,
                            line_number,
                        })
                        .collect(),
                )
            })
            .collect(),
    };
    ClassFileMethod {
        access_flags: MethodAccessFlags::STATIC,
        name: Arc::from("loop"),
        descriptor: Arc::from("(I)I"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(code)))],
    }
}

#[test]
fn the_largest_start_at_or_before_the_bci_names_the_line() {
    // `F\n    .invokeExact(x)`: the getstatic at 0 on line 10, the
    // invoke's operands from 3 on line 11 (out of order, split tables).
    let m = method(vec![vec![(3, 11)], vec![(0, 10)]]);
    assert_eq!(mh_filter_line_at(&m, 0), Some(10));
    assert_eq!(mh_filter_line_at(&m, 2), Some(10));
    assert_eq!(mh_filter_line_at(&m, 4), Some(11));
    assert_ne!(mh_filter_line_at(&m, 0), mh_filter_line_at(&m, 4));
}

#[test]
fn no_table_or_a_bci_before_the_first_entry_has_no_line() {
    assert_eq!(mh_filter_line_at(&method(Vec::new()), 4), None);
    assert_eq!(mh_filter_line_at(&method(vec![vec![(2, 7)]]), 1), None);
}
