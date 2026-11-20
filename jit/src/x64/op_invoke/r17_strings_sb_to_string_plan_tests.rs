use super::sb_to_string_plan;

#[test]
fn a_layout_without_a_string_class_id_is_refused() {
    let outer = crate::StringFieldLayout::new(0, Some(1), 2, 0);
    let plan = sb_to_string_plan(&outer);
    assert!(plan.is_err());
    if !crate::jit_sb_to_string_inline_enabled() {
        assert_eq!(plan, Err("switch off"));
    }
}
