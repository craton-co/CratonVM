//! gc-common w9-c: a VM-raised exception's detail message is a FRESH
//! String. It used to be an entry of the intern pool, a strong GC root
//! nothing prunes, so every distinct message stayed live for the life of
//! the VM.

use super::*;
use crate::config::VmConfig;
use crate::vm::Vm;

/// The message reaches the throwable but not the pool.
#[test]
fn a_detail_message_is_not_interned() {
    let mut vm = Vm::new(VmConfig::default());
    let text = "w9c: Index 918273 out of bounds for length 7";
    let Ok(obj) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/ArrayIndexOutOfBoundsException",
        Some(text),
    ) else {
        // Stripped VM (no JDK on the test classpath): nothing to probe with.
        return;
    };
    assert!(
        vm.shared.mem.string_pool.read().get(text).is_none(),
        "a VM-raised detail message was interned: the pool roots it for \
         the life of the VM"
    );
    // Where the layout has the field, the message is still delivered.
    if let Some(slot) = instance_field_index_by_name(&vm.shared, obj, "detailMessage") {
        if let Value::Object(Some(s)) = vm.shared.mem.heap.get_field(obj, slot) {
            assert_eq!(
                crate::vm::read_java_string(&vm.shared.mem.heap, s).as_deref(),
                Some(text)
            );
        }
    }
}

/// Two throwables with one message get two Strings, as on HotSpot.
#[test]
fn equal_detail_messages_are_distinct_strings() {
    let mut vm = Vm::new(VmConfig::default());
    let text = "w9c: / by zero, twice";
    let Ok(first) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/ArithmeticException",
        Some(text),
    ) else {
        return;
    };
    vm.main_thread.native_pin_roots.push(first);
    let second = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/ArithmeticException",
        Some(text),
    );
    let first = vm
        .main_thread
        .native_pin_roots
        .pop()
        .expect("the pin pushed above");
    let Ok(second) = second else { return };
    let Some(slot) = instance_field_index_by_name(&vm.shared, first, "detailMessage") else {
        return;
    };
    let a = vm.shared.mem.heap.get_field(first, slot);
    let b = vm.shared.mem.heap.get_field(second, slot);
    if let (Value::Object(Some(a)), Value::Object(Some(b))) = (a, b) {
        assert_ne!(a, b, "one String shared by two detail messages");
    }
}

/// The no-collect door (a VM-raised `OutOfMemoryError`) still pools its
/// closed set of messages, so a repeated error on a full heap can carry
/// its message without allocating it.
#[test]
fn the_no_collect_door_keeps_its_message_pooled() {
    let mut vm = Vm::new(VmConfig::default());
    let text = "Java heap space";
    let Ok(_) = create_exception_object_no_collect(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/OutOfMemoryError",
        Some(text),
    ) else {
        return;
    };
    assert!(vm.shared.mem.string_pool.read().get(text).is_some());    }