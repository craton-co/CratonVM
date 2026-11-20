use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;

/// The walk's production text, whitespace removed.
fn walk_text() -> String {
    let src: String = include_str!("../constants.rs")
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let start = src.find("fnmethod_type_walk(").expect("the walk");
    let end = src[start..]
        .find("fnmethod_type_failure_as_linkage_error(")
        .expect("the walk's end");
    src[start..start + end].to_string()
}

/// With the switch on, a class that reports no loader never reaches the
/// bootstrap `forName0` for a type: its arm runs first, resolves through
/// the class's own namespace and goes on to the next type.
#[test]
fn the_no_loader_arm_runs_before_the_bootstrap_for_name0() {
    let walk = walk_text();
    let arm = walk
        .find("ifloader_slot.is_none()&&method_type_no_loader_resolves_in_vm(){")
        .expect("the no-loader arm");
    let for_name0 = walk.find("\"forName0\"").expect("the forName0 ask");
    assert!(arm < for_name0, "the arm must come before the forName0 ask");
    let body = &walk[arm..for_name0];
    assert!(body.contains(
        "class_mirror_for_field_descriptor(shared,thread,frame_class_id,desc)?"
    ));
    assert!(body.contains("slots.push(thread.native_pin_roots.len()-1);continue;"));
}

/// The arm's miss is the `NoClassDefFoundError` HotSpot's method-type
/// resolution raises, never the bootstrap loader's checked
/// `ClassNotFoundException` (whose escape out of an `invokedynamic` is the
/// page's crash).
#[test]
fn the_no_loader_arms_miss_is_a_no_class_def_found_error() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "gci-i3jd");
    let missed = class_mirror_for_field_descriptor(
        &shared,
        &mut thread,
        ClassId::new(0),
        "Lcratonvm/test/gci/i3jd/NoSuchType;",
    );
    let Err(MethodCallFailed::ExceptionThrown(obj)) = missed else {
        // Stripped VM (no class library on the test class path): nothing
        // Java-visible to check.
        return;
    };
    let name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(shared.mem.heap.class_id_of(obj))
            .map(|c| c.name.to_string())
    };
    assert_eq!(name.as_deref(), Some("java/lang/NoClassDefFoundError"));
}
