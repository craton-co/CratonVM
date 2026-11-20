/// gcg g4/oo: the construction depth `create_vm_oome_object` raises is
/// restored before a foreign panic is resumed, so a contained panic cannot
/// leave the thread skipping the post-OOME debt for good.
#[test]
fn gcg_g4oo_the_construct_depth_is_restored_before_a_panic_resumes() {
    let src = include_str!("../exceptions.rs");
    let body = src
        .split("fn create_vm_oome_object_with_array_retry(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("create_vm_oome_object body");
    let caught = body.find("std::panic::catch_unwind(").expect("the wrapper");
    let restore = body
        .find("thread.oome_construct_depth.saturating_sub(1)")
        .expect("the restore");
    let resume = body
        .find("std::panic::resume_unwind(")
        .expect("the resumed panic");
    assert!(caught < restore && restore < resume);
}
