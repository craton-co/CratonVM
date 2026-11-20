use super::{
    args_refreshed_if_moved, invoke_or_native_keeping_args_current, push_object_arg_pins,
    PinnedDispatchArgs, SharedVm,
};
use crate::classloading::ClassId;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;

#[test]
fn an_unmoved_argument_list_is_not_copied_and_a_moved_one_is_refreshed() {
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "i15-l5-refresh");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let args = [
        Value::Int(1),
        Value::Object(Some(a)),
        Value::Object(None),
        Value::Object(Some(a)),
    ];
    let pin_base = thread.native_pin_roots.len();
    assert!(push_object_arg_pins(&mut thread, &args));
    assert_eq!(thread.native_pin_roots.len(), pin_base + 2);
    assert!(
        args_refreshed_if_moved(&thread, pin_base, &args).is_none(),
        "nothing moved: no copy"
    );
    // A collector forwarding the second pinned reference.
    thread.native_pin_roots[pin_base + 1] = b;
    let fresh = args_refreshed_if_moved(&thread, pin_base, &args).expect("a reference moved");
    assert_eq!(fresh.len(), 4);
    assert!(matches!(fresh[0], Value::Int(1)));
    assert!(matches!(fresh[1], Value::Object(Some(x)) if x == a));
    assert!(matches!(fresh[2], Value::Object(None)));
    assert!(matches!(fresh[3], Value::Object(Some(x)) if x == b));
    thread.native_pin_roots.truncate(pin_base);
    assert!(!push_object_arg_pins(
        &mut thread,
        &[Value::Int(2), Value::Object(None)]
    ));
    assert_eq!(thread.native_pin_roots.len(), pin_base);
}

/// Nine object arguments (past the guard's inline capacity) among
/// primitives: each is refreshed from its own pin, a slot the dispatch
/// has since cleared is left alone, and the drop releases every pin.
#[test]
fn the_dispatch_guard_refreshes_by_position_and_releases_its_pins() {
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "i15-l5-guard");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut values: Vec<Value> = (0..14)
        .map(|i: i32| {
            if i % 3 == 0 {
                Value::Int(i)
            } else {
                Value::Object(Some(a))
            }
        })
        .collect();
    let pin_base = thread.native_pin_roots.len();
    {
        let guard = PinnedDispatchArgs::new(&mut thread, &values);
        assert_eq!(thread.native_pin_roots.len(), pin_base + 9);
        // Forward the last pinned reference (argument 13).
        thread.native_pin_roots[pin_base + 8] = b;
        values[1] = Value::Object(None);
        guard.refresh(&mut values);
        assert!(matches!(values[0], Value::Int(0)));
        assert!(matches!(values[1], Value::Object(None)));
        assert!(matches!(values[2], Value::Object(Some(x)) if x == a));
        assert!(matches!(values[12], Value::Int(12)));
        assert!(matches!(values[13], Value::Object(Some(x)) if x == b));
    }
    assert_eq!(thread.native_pin_roots.len(), pin_base, "pins released");
}

/// gch h1/jp (fr review item 3): the rescue door's attempt fails, its
/// pins are released, and the arguments it refreshed are the pinned ones
/// (nothing moved here, so unchanged); without `keep_current` it pins
/// nothing at all.
#[test]
fn gch_h1jp_a_failed_by_name_attempt_releases_its_rescue_pins() {
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "gch-h1jp-rescue");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let pin_base = thread.native_pin_roots.len();
    for keep_current in [true, false] {
        let mut args = vec![Value::Object(Some(a)), Value::Int(7), Value::Object(None)];
        let result = invoke_or_native_keeping_args_current(
            &shared,
            &mut thread,
            "<unknown class 1>",
            "m",
            "(ILjava/lang/Object;)V",
            &mut args,
            keep_current,
        );
        assert!(result.is_err(), "a placeholder class name never resolves");
        assert_eq!(thread.native_pin_roots.len(), pin_base, "pins released");
        assert!(matches!(args[0], Value::Object(Some(x)) if x == a));
        assert!(matches!(args[1], Value::Int(7)));
        assert!(matches!(args[2], Value::Object(None)));
    }
}
