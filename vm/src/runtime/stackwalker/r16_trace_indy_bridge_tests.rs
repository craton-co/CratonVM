use super::{
    capture_throwable_backtrace_without_store, capture_throwable_trace_without_store,
    entry_is_vm_bridge, entry_may_be_hidden, frame_is_vm_bridge, stack_entry_is_hidden,
    JIT_INDY_BRIDGE_FRAME_CLASS, LINE_NUMBER_UNKNOWN,
};
use crate::classloading::{ClassId, ClassStore};
use crate::native::registry::StackTraceEntry;
use crate::runtime::frame::Frame;
use std::sync::Arc;

fn owned(class: &str, method: &str, descriptor: &str) -> Frame {
    Frame::new(
        ClassId::new(1),
        class.to_string(),
        method.to_string(),
        descriptor.to_string(),
        None,
        vec![0; 16],
        vec![],
        4,
        4,
        &[],
    )
}

fn bridge(method: &str) -> Frame {
    let mut frame = owned(JIT_INDY_BRIDGE_FRAME_CLASS, method, "()Ljava/lang/Object;");
    frame.mark_vm_synthetic();
    frame
}

fn entry(class: &str, method: &str, synthetic: bool) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 0,
        class_id: (!synthetic).then(|| ClassId::new(1)),
        method_index: None,
    }
}

#[test]
fn the_bridge_frame_requires_the_vm_synthetic_marker() {
    assert!(frame_is_vm_bridge(&bridge("bridge")));
    assert!(frame_is_vm_bridge(&bridge("concat")));
    assert!(!frame_is_vm_bridge(&owned("probe/Target", "bridge", "()V")));
    assert!(!frame_is_vm_bridge(&owned("<jit-indy>", "bridge", "()V")));
    assert!(entry_is_vm_bridge(&entry("<jit-indy>", "bridge", true)));
    assert!(!entry_is_vm_bridge(&entry("<jit-indy>", "bridge", false)));
    assert!(!entry_is_vm_bridge(&entry("probe/Target", "bridge", false)));
}

/// A throwable captured with the bridge frame live (a record component's
/// `toString()` throwing under a bridged `ObjectMethods` site) lists the
/// frames below and above it, and not it: the `R16IndyRecord` `trace:` row.
#[test]
fn a_throwable_capture_leaves_the_bridge_frame_out() {
    if !crate::jit::stack_trace::active_compiled_frames().is_empty() {
        return;
    }
    let frames = vec![
        owned("probe/Target", "main", "([Ljava/lang/String;)V"),
        bridge("bridge"),
        owned("probe/Target$Boom", "toString", "()Ljava/lang/String;"),
    ];
    let mut keep_all = |_: Option<ClassId>, _: &str| false;
    let trace = capture_throwable_trace_without_store(&frames, 0, &mut keep_all);
    let names: Vec<String> = trace
        .iter()
        .map(|e| format!("{}.{}", e.class_name, e.method_name))
        .collect();
    assert_eq!(names, ["probe/Target.main", "probe/Target$Boom.toString"]);
    let retained = capture_throwable_backtrace_without_store(&frames, 0, &mut keep_all, &[]);
    let names: Vec<String> = retained
        .iter()
        .map(|f| {
            let e = f.to_entry();
            format!("{}.{}", e.class_name, e.method_name)
        })
        .collect();
    assert_eq!(names, ["probe/Target.main", "probe/Target$Boom.toString"]);
}

/// A walk's entry for the frame (`StackWalker`, `Thread.getStackTrace()`)
/// is hidden without the class manager knowing its class.
#[test]
fn a_walk_entry_of_the_bridge_is_hidden() {
    let store = ClassStore::new();
    assert_eq!(entry_may_be_hidden(JIT_INDY_BRIDGE_FRAME_CLASS), false);
    assert!(stack_entry_is_hidden(
        &store,
        &entry("<jit-indy>", "bridge", true)
    ));
    assert!(stack_entry_is_hidden(
        &store,
        &entry("<jit-indy>", "concat", true)
    ));
    assert!(!stack_entry_is_hidden(
        &store,
        &entry("probe/Target", "main", false)
    ));
}
