//! jr21 localshi (R16B-177): `FrameLayout::java_locals_hi` is the EXCLUSIVE
//! end of the Java-local words. Local `i` is at `[rbp - (i + 1) * 8]`, so a
//! method with `n` locals publishes `(n + 1) * 8` (single-pass
//! `x64/frames.rs`, IR `ir_published_java_locals_hi` under
//! `CRATONVM_JIT_IR_PUBLISH_LAST_LOCAL`), and every reader must test
//! `off < java_locals_hi`. `verifier_local_verdict` and
//! `classify_frame_slot` used to read it inclusively.
use super::{
    classify_frame_slot, layout_is_java_local, region_is_dataflow_modelled,
    verifier_local_verdict, OracleTypeMapsScope, VerifierSlotVerdict,
};

/// Two Java locals (`n = 2`): the layout both x64 producers publish.
fn two_local_layout() -> cratonvm_jit::FrameLayout {
    cratonvm_jit::FrameLayout {
        java_locals_hi: 24,
        locals_hi: 48,
        spill_lo: 48,
        spill_hi: 80,
        ..Default::default()
    }
}

#[test]
fn the_last_local_is_a_java_local_and_the_next_word_is_not() {
    let l = two_local_layout();
    assert_eq!(classify_frame_slot(8, &l), "java-local");
    assert_eq!(classify_frame_slot(16, &l), "java-local", "the LAST local");
    assert_eq!(
        classify_frame_slot(24, &l),
        "reserved-locals-tail",
        "one past"
    );
    assert_eq!(classify_frame_slot(48, &l), "operand-spill");
    assert!(layout_is_java_local(&l, 16));
    assert!(!layout_is_java_local(&l, 24));
    assert!(region_is_dataflow_modelled(&l, 16));
    assert!(!region_is_dataflow_modelled(&l, 24));
    assert!(!l.is_non_oop_reserved_tail_slot(16));
    assert!(l.is_non_oop_reserved_tail_slot(24));
}

/// The diagnostic classifier and the production ladder name every word of
/// the locals / tail / spill band the same way.
#[test]
fn classify_frame_slot_agrees_with_region_name_on_the_locals_band() {
    let l = two_local_layout();
    let mut off = 8;
    while off < 80 {
        let c = classify_frame_slot(off, &l);
        assert_eq!(c, l.region_name(off), "off={off}");
        assert_eq!(
            c == "java-local",
            layout_is_java_local(&l, off),
            "off={off}"
        );
        off += 8;
    }
}

/// The verifier oracle answers for the last local and declines one word
/// past it, even when the class file's row is wider than the published
/// locals (so the old inclusive bound would have answered `Oop` there).
#[test]
fn the_verifier_oracle_stops_at_the_exclusive_bound() {
    use cratonvm_classloading::vtype::VType;
    use std::sync::Arc;

    let id = cratonvm_types::ClassId::new(0x003F_0177);
    let mut b = cratonvm_classloading::MethodTypeMapsBuilder::new(3, 0);
    b.record_slots(
        0,
        &[
            VType::Int,
            VType::ObjectRef(Arc::from("p/A")),
            VType::ObjectRef(Arc::from("p/A")),
        ],
        &[],
    );
    let store = Arc::new(cratonvm_classloading::TypeMapStore::new());
    assert!(store.publish(
        id,
        cratonvm_classloading::ClassTypeMaps::new(vec![(
            Arc::<str>::from("m"),
            Arc::<str>::from("()V"),
            Some(b.finish(true)),
        )])
    ));
    let buf = cratonvm_jit::ExecutableBuffer::new(64)
        .expect("executable buffer alloc must succeed in tests");
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.method_label = "p/Q.m:()V".to_string();
    cm.frame_layout = two_local_layout();
    cm.owner_class_id = id.as_u32();

    let _vm = OracleTypeMapsScope::bind_armed(&store);
    assert!(verifier_local_verdict(&cm, 8, 0) == VerifierSlotVerdict::NotOop);
    assert!(
        verifier_local_verdict(&cm, 16, 0) == VerifierSlotVerdict::Oop,
        "the last published local is asked"
    );
    assert!(
        verifier_local_verdict(&cm, 24, 0) == VerifierSlotVerdict::Unknown,
        "one word past the exclusive bound is not a published local"
    );
}
