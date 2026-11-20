use super::*;
use cratonvm_reader::attribute::{
    Annotation, Attribute, CodeAttribute, ElementValue, ElementValuePair, LazyAttribute,
    LineNumberEntry,
};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;

fn lines(pairs: &[(u16, u16)]) -> Vec<LineNumberEntry> {
    pairs
        .iter()
        .map(|&(start_pc, line_number)| LineNumberEntry { start_pc, line_number })
        .collect()
}

#[test]
fn gci_i32mf_unsigned5_and_line_table_are_hotspots_encoding() {
    assert_eq!(hs_unsigned5_len(0), 1);
    assert_eq!(hs_unsigned5_len(191), 1);
    assert_eq!(hs_unsigned5_len(192), 2);
    assert_eq!(hs_unsigned5_len(12_479), 2);
    assert_eq!(hs_unsigned5_len(12_480), 3);
    assert_eq!(hs_compressed_line_table_bytes(lines(&[]).iter()), 0);
    // (0, 0): no pair, the terminator only.
    assert_eq!(hs_compressed_line_table_bytes(lines(&[(0, 0)]).iter()), 1);
    // line delta 10 does not fit 3 bits: escape + two one-byte ints.
    assert_eq!(hs_compressed_line_table_bytes(lines(&[(0, 10)]).iter()), 4);
    // then bci +4, line +1: one byte.
    assert_eq!(hs_compressed_line_table_bytes(lines(&[(0, 10), (4, 11)]).iter()), 5);
    // (31, 7) would be the 0xFF escape byte: written as a regular pair.
    assert_eq!(hs_compressed_line_table_bytes(lines(&[(31, 7)]).iter()), 4);
}

#[test]
fn gci_i32mf_arrays_and_annotations_are_hotspots_sizes() {
    assert_eq!(hs_array_bytes(1, 0), 0, "shared empty array");
    assert_eq!(hs_array_bytes(1, 5), 16);
    assert_eq!(hs_array_bytes(2, 3), 16);
    assert_eq!(hs_array_bytes(8, 1), 16);
    assert_eq!(hs_array_bytes(HS_RESOLVED_METHOD_ENTRY_BYTES, 2), 56);
    let marker = Annotation { type_index: 1, element_value_pairs: Vec::new() };
    assert_eq!(hs_annotation_bytes(&marker), 4);
    let valued = Annotation {
        type_index: 1,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Const { tag: b'I', const_value_index: 3 },
        }],
    };
    assert_eq!(hs_annotation_bytes(&valued), 9);
    assert_eq!(hs_annotations_attr_bytes(&[marker, valued]), 15);
    // Invisible annotations are not kept.
    let invisible =
        LazyAttribute::new_decoded(Attribute::RuntimeInvisibleAnnotations(Vec::new()));
    assert_eq!(hs_visible_annotation_body(&invisible), None);
}

#[test]
fn gci_i32mf_method_is_method_plus_const_method() {
    let method = |flags: MethodAccessFlags, attributes: Vec<LazyAttribute>| ClassFileMethod {
        access_flags: flags,
        name: std::sync::Arc::from("m"),
        descriptor: std::sync::Arc::from("()V"),
        attributes,
    };
    // Abstract: `Method` + a bare `ConstMethod`.
    let bare = method(MethodAccessFlags::ABSTRACT, Vec::new());
    assert_eq!(hs_method_bytes(&bare), HS_METHOD_BYTES + HS_CONST_METHOD_BYTES);
    // Native: two more words.
    let native = method(MethodAccessFlags::NATIVE, Vec::new());
    assert_eq!(
        hs_method_bytes(&native),
        HS_METHOD_BYTES + HS_NATIVE_METHOD_EXTRA_BYTES + HS_CONST_METHOD_BYTES
    );
    // 10 bytecodes + a 4-byte line table + a generic signature (2) = 16.
    let code = CodeAttribute {
        max_stack: 1,
        max_locals: 1,
        code: cratonvm_reader::ByteView::from_vec(vec![0; 10]),
        exception_table: Vec::new(),
        attributes: vec![Attribute::LineNumberTable(lines(&[(0, 10)]))],
    };
    let concrete = method(
        MethodAccessFlags::PUBLIC,
        vec![
            LazyAttribute::new_decoded(Attribute::Code(Box::new(code))),
            LazyAttribute::new_decoded(Attribute::Signature(std::sync::Arc::from("()V"))),
        ],
    );
    assert_eq!(hs_method_bytes(&concrete), HS_METHOD_BYTES + HS_CONST_METHOD_BYTES + 16);
}

/// Every consumer's figure is the model (the switch's default), and the
/// reader-structure figure stays behind `=0`.
#[test]
fn gci_i32mf_the_metaspace_figure_is_the_hotspot_model() {
    let shared = SharedVm::new(VmConfig::default());
    let cm = shared.classes.class_manager.read();
    if metaspace_hotspot_figure_enabled() {
        assert_eq!(metaspace_used_bytes(&cm), hotspot_metaspace_used_bytes(&cm));
    } else {
        assert_eq!(metaspace_used_bytes(&cm), metaspace_reader_struct_bytes(&cm));
    }
    let src = concat!(include_str!("../vm_init/mod.rs"), "\n", include_str!("../vm_init/missing_native_entry.rs"), "\n", include_str!("../vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../vm_init/wait_site.rs"), "\n", include_str!("../vm_init/class_store_cut_with.rs"));
    let body = &src[src.find("pub(crate) fn metaspace_used_bytes(").expect("the walk")..];
    let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
    // gci i33/ms: the model takes the class manager (the class type
    // annotations live there).
    assert!(body.contains("hotspot_metaspace_used_bytes(cm)"));
}
