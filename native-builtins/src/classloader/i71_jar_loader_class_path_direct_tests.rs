use super::jar_loader_class_path_direct;
use crate::test_utils::mock_ctx;

/// `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT` (interpreter round i3 wave 71,
/// lane L5) is off in a context that does not turn it on (the mock, as a
/// VM without the switch): the class path is not asked, and the caller
/// goes to the rows exactly as before. The served side is covered by
/// `classloading`'s `a_class_path_jar_serves_its_entries_without_a_row`.
#[test]
fn without_the_switch_the_class_path_is_not_asked() {
    let ctx = mock_ctx();
    assert!(
        jar_loader_class_path_direct(&ctx, "/nowhere/lib.jar", "p/A.class", 0, 0).is_none()
    );
}
