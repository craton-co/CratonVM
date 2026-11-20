use super::{mirror_at_init_enabled, mirror_at_init_flag, SharedVm};



/// The init-path mirror runs after `ClassPrepare` and only on the
/// initialization path (`failure_state` set), where a collection is
/// allowed; the plain link path (`link_class_on_thread`) never collects.
#[test]
fn gcg_g3o_the_init_path_mirror_is_gated_on_the_initialization_path() {
    let src = include_str!("../vm_util.rs");
    let body = src
        .split("fn link_claimed_class(")
        .nth(1)
        .and_then(|rest| rest.split("\nfn ").next())
        .expect("link_claimed_class body");
    let prepare = body
        .find("fire_class_prepare_for_vm(")
        .expect("ClassPrepare report");
    let gate = body
        .find("if failure_state.is_some() && mirror_at_init_enabled(shared)")
        .expect("the init-path gate");
    assert!(prepare < gate, "the mirror is made after ClassPrepare");
}
