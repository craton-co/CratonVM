use super::is_marshallable_array_element;
use cratonvm_reader::field_type::FieldType;
use cratonvm_types::ArrayElementType;
use jit_cuda::analyzer::ParamKind;

/// The analyzer decides which kernels are admitted; the marshaller
/// decides which arrays it can actually push to a device. Nothing
/// structural ties the two together, and when they disagree in the
/// analyzer-admits/marshaller-refuses direction the failure is
/// SILENT: `dispatch_method_from_native` returns an error, the VM
/// re-runs the method on the interpreter, and every answer is still
/// correct. A value differential cannot see it -- it ends up
/// comparing the interpreter with itself and reporting a pass.
///
/// That is precisely what happened to `short[]` and `byte[]`:
/// `ParamKind::I16Array`/`I8Array` were admitted, `gpu_marshal`
/// generated the whole `upload_obj_i16`/`download_obj_i16` pair via
/// `direct_xfer!`, `host_view_i16`/`write_back_i16` had unit tests
/// at this altitude -- and the marshal loop's `match element_type`
/// had no arm, so not one such kernel ever reached a GPU. Fixed
/// 2026-09-02; this test is what keeps it fixed, and it needs no
/// device to run.
#[test]
fn analyzer_and_marshaller_admit_the_same_arrays() {
    let cases = [
        (FieldType::Int, ArrayElementType::Int),
        (FieldType::Long, ArrayElementType::Long),
        (FieldType::Float, ArrayElementType::Float),
        (FieldType::Double, ArrayElementType::Double),
        (FieldType::Short, ArrayElementType::Short),
        (FieldType::Byte, ArrayElementType::Byte),
        (FieldType::Char, ArrayElementType::Char),
        (FieldType::Boolean, ArrayElementType::Boolean),
    ];
    for (component, elem) in cases {
        let array_ty = FieldType::Array(Box::new(component.clone()));
        let analyzer_admits = ParamKind::from_field(&array_ty).is_some_and(ParamKind::is_array);
        let marshaller_admits = is_marshallable_array_element(elem);
        assert_eq!(
            analyzer_admits,
            marshaller_admits,
            concat!(
                "{:?}[]: the analyzer {} it but the marshaller ",
                "{} it. Analyzer-admits/marshaller-refuses is the SILENT ",
                "direction: such a kernel compiles, fails to dispatch, ",
                "and quietly re-runs on the interpreter with correct ",
                "results, so no value differential can see it."
            ),
            component,
            if analyzer_admits { "admits" } else { "rejects" },
            if marshaller_admits {
                "accepts"
            } else {
                "refuses"
            },
        );
    }
}

/// The widths the pipeline really carries, spelled out so a silent
/// widening or narrowing of either side has to edit this list.
/// `char[]` joined on 2026-09-03.
#[test]
fn the_admitted_set_is_exactly_the_marshallable_widths() {
    for t in [
        ArrayElementType::Int,
        ArrayElementType::Long,
        ArrayElementType::Float,
        ArrayElementType::Double,
        ArrayElementType::Short,
        ArrayElementType::Byte,
        ArrayElementType::Char,
        ArrayElementType::Boolean,
    ] {
        assert!(
            is_marshallable_array_element(t),
            "{t:?} must be marshallable"
        );
    }
    for t in [ArrayElementType::Reference] {
        assert!(
            !is_marshallable_array_element(t),
            "{t:?} must not be marshallable"
        );
    }
}
