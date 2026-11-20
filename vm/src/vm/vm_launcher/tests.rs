use super::main_class_load_failure_text;

/// zs s4/nd4: the argument array, pinned as the launcher pins it, is the
/// `String[]` of the arguments, sits in its own `native_pin_roots` slot
/// (the receiver's pin goes above it), survives a collection at its
/// current address, and releasing the pin leaves the pin stack where it
/// found it. Runs on Generational and on ZGC.
#[test]
fn zs4nd4_pinned_main_args_hold_the_arguments_across_a_collection() {
    use crate::config::{GcAlgorithm, VmConfig};
    use cratonvm_native_api::NativeHeapAccess as _;
    for gc in [GcAlgorithm::Generational, GcAlgorithm::Zgc] {
        let mut vm = crate::vm::Vm::new(VmConfig {
            gc_algorithm: gc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        let base = vm.main_thread.native_pin_roots.len();
        let args = vec!["alpha".to_string(), "b\u{e9}ta".to_string()];
        let array = vm
            .launcher_main_arguments(&args)
            .expect("the argument array is built on an empty heap");
        let pin = vm.main_thread.native_pin_roots.len();
        vm.main_thread.native_pin_roots.push(array);
        assert_eq!(pin, base, "{gc:?}: the array takes the next pin slot");
        assert_eq!(vm.main_thread.native_pin_roots.len(), base + 1);
        crate::runtime::interpreter::force_gc_for_vm(&vm.shared, &mut vm.main_thread);
        let array = vm.main_thread.native_pin_roots[pin];
        let mut ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        assert_eq!(ctx.array_length(array), 2, "{gc:?}");
        let read: Vec<Option<String>> = (0..2)
            .map(|i| match ctx.get_array_element(array, i) {
                crate::types::Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            })
            .collect();
        assert_eq!(
            read,
            vec![Some("alpha".to_string()), Some("b\u{e9}ta".to_string())],
            "{gc:?}: the pinned array still names the argument strings"
        );
        vm.main_thread.native_pin_roots.truncate(pin);
        assert_eq!(vm.main_thread.native_pin_roots.len(), base, "{gc:?}");
    }
}

#[test]
fn launcher_arguments_are_fresh_strings_outside_the_intern_pool() {
    use crate::types::Value;
    let mut vm = crate::vm::Vm::new(crate::config::VmConfig::default());
    let _ = crate::vm::create_java_string(&vm.shared, "duplicate");
    let args = vm
        .launcher_main_arguments(&["duplicate".into(), "duplicate".into(), "ñ🦀".into()])
        .expect("argument construction");
    let literal = crate::vm::create_java_string(&vm.shared, "duplicate");
    let first = vm.shared.mem.heap.get_array_element(args, 0).unwrap();
    let second = vm.shared.mem.heap.get_array_element(args, 1).unwrap();
    assert_ne!(first, second);
    assert_ne!(first, Value::Object(Some(literal)));
    assert_ne!(second, Value::Object(Some(literal)));
    assert!(matches!(
        vm.shared.mem.heap.get_array_element(args, 2),
        Ok(Value::Object(Some(_)))
    ));
}

#[test]
fn load_failure_texts_follow_launcher_properties() {
    assert_eq!(
        main_class_load_failure_text("a.B", "java.lang.ClassNotFoundException", "a.B", true),
        "Error: Could not find or load main class a.B\nCaused by: java.lang.ClassNotFoundException: a.B"
    );
    assert_eq!(
        main_class_load_failure_text("a.B", "java.lang.NoClassDefFoundError", "a/C", true),
        "Error: Could not find or load main class a.B\nCaused by: java.lang.NoClassDefFoundError: a/C"
    );
    assert_eq!(
        main_class_load_failure_text("a.B", "java.lang.ClassFormatError", "bad", true),
        "Error: LinkageError occurred while loading main class a.B\n\tjava.lang.ClassFormatError: bad"
    );
}
