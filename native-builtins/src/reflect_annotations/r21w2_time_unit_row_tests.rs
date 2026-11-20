use super::*;

/// nio21 wave 2, lane L4 (`JdkTimeUnit`, `CRATONVM_COMPAT_ALIGN=-jdk-time-unit`):
/// over a real JDK, `--compatible` with the row (the default) registers
/// none of the nine `TimeUnit` stand-ins, and with the row off all nine
/// stay, each a stated `SyntheticStub`. The rest of the registrar is not
/// the row's (`InvocationTargetException.getCause` stays either way).
#[test]
fn time_unit_stand_ins_leave_with_the_row_and_state_their_kind() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let build = |rows: HotSpotAlignments| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_real_jdk(true);
        registry.set_hotspot_alignments(rows);
        register_annotation_overrides(&mut registry);
        registry
    };
    let time_unit_rows = |registry: &NativeMethodRegistry| {
        registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == "java/util/concurrent/TimeUnit")
            .count()
    };
    let on = build(HotSpotAlignments::defaults());
    assert_eq!(time_unit_rows(&on), 0);
    let off = build(HotSpotAlignments::defaults().with(Row::JdkTimeUnit, false));
    assert_eq!(time_unit_rows(&off), 9);
    assert_eq!(
        off.kind_of("java/util/concurrent/TimeUnit", "toNanos", "(J)J"),
        Some(cratonvm_native_api::NativeKind::SyntheticStub)
    );
    for registry in [&on, &off] {
        assert!(registry
            .find(
                "java/lang/reflect/InvocationTargetException",
                "getCause",
                "()Ljava/lang/Throwable;"
            )
            .is_some());
    }
}
