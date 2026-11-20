use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::NativeCallback;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const MOVE: usize = 1 << 32;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key / pin only; the natives these fake addresses reach
    // never dereference them.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// Forgets the rows filed under `keys` (the lock-key registry hands freed
/// keys to `forget_serialization_keys` only once the w21-c `lib.rs`
/// handoff is applied), the VM's keys, and its reader / retired rows.
struct Forget {
    vm: usize,
    keys: Vec<usize>,
}

impl Drop for Forget {
    fn drop(&mut self) {
        forget_serialization_keys(&self.keys);
        crate::forget_vm_lock_keys(self.vm);
        forget_vm_serialization_state(self.vm);
    }
}

fn native(r: &NativeMethodRegistry, cls: &str, name: &str, desc: &str) -> NativeCallback {
    r.find(cls, name, desc)
        .unwrap_or_else(|| panic!("{cls}.{name}{desc} is registered"))
}

fn move_objects(vm: usize, moves: &[(usize, usize)]) {
    let mut moved = cratonvm_types::PointerMap::default();
    for &(from, to) in moves {
        moved.insert(from, to);
    }
    crate::gc_sweep_and_remap_lock_keys(vm, &|_| true, &moved);
}

/// A collection that moves the streams between two operations: the
/// second write lands in the SAME buffer (it went to a new, empty one
/// keyed by the new address, and the stream came out truncated), and the
/// reader continues where it was (it read zero padding).
#[test]
fn a_moved_stream_keeps_its_buffers() {
    const VM: usize = 0x5E21_0C10;
    let mut ctx = vm_ctx(VM);
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    let header = native(&r, "java/io/ObjectOutputStream", "writeStreamHeader", "()V");
    let read_int = native(&r, "java/io/ObjectInputStream", "readInt", "()I");
    let available = native(&r, "java/io/ObjectInputStream", "available", "()I");

    let (oos_old, oos_new) = (obj(0x1_5E21_0010), obj(0x1_5E21_0010 + MOVE));
    let (ois_old, ois_new) = (obj(0x1_5E21_0020), obj(0x1_5E21_0020 + MOVE));
    let oos_key = stream_key(&ctx, oos_old);
    let ois_key = stream_key(&ctx, ois_old);
    let _forget = Forget {
        vm: VM,
        keys: vec![oos_key, ois_key],
    };
    oos_buf_reset(oos_key);
    ois_buf_load(ois_key, vec![0, 0, 0, 7, 0, 0, 0, 9]);

    header(&mut ctx, &[Value::Object(Some(oos_old))]).expect("header");
    assert_eq!(
        read_int(&mut ctx, &[Value::Object(Some(ois_old))])
            .ok()
            .flatten(),
        Some(Value::Int(7))
    );

    move_objects(
        VM,
        &[
            (addr(oos_old), addr(oos_new)),
            (addr(ois_old), addr(ois_new)),
        ],
    );
    assert_eq!(
        stream_key(&ctx, oos_new),
        oos_key,
        "a move keeps the stream key"
    );
    assert_eq!(
        stream_key(&ctx, ois_new),
        ois_key,
        "a move keeps the stream key"
    );

    header(&mut ctx, &[Value::Object(Some(oos_new))]).expect("header after the move");
    assert_eq!(
        oos_buf_snapshot(oos_key),
        vec![0xAC, 0xED, 0x00, 0x05, 0xAC, 0xED, 0x00, 0x05],
        "both writes are in the stream's one buffer"
    );
    assert_eq!(
        read_int(&mut ctx, &[Value::Object(Some(ois_new))])
            .ok()
            .flatten(),
        Some(Value::Int(9)),
        "the moved reader continues where it was"
    );
    assert_eq!(
        available(&mut ctx, &[Value::Object(Some(ois_new))])
            .ok()
            .flatten(),
        Some(Value::Int(0))
    );
}

/// The writer's back-reference table follows a moved object (the second
/// write of it is a `TC_REFERENCE`, found without touching the heap at
/// the new address) and forgets a dead one's row with its key.
#[test]
fn written_objects_are_keyed_across_a_move_and_forgotten_when_dead() {
    let _serial_guard = serialization_test_guard();
    const VM: usize = 0x5E21_0C20;
    let mut ctx = vm_ctx(VM);
    let _string = ctx.ensure_class_initialized("java/lang/String").unwrap();
    let stream = ctx.fresh_object_ref();
    let key = stream_key(&ctx, stream);
    let s = ctx.create_string("once");
    let s_key = stream_key(&ctx, s);
    let _forget = Forget {
        vm: VM,
        keys: vec![key, s_key],
    };
    oos_buf_reset(key);

    oos_write_value(&mut ctx, key, &Value::Object(Some(s))).expect("first write");
    let first = oos_buf_snapshot(key);
    assert_eq!(first[0], TC_STRING);

    let s_moved = obj(addr(s) + MOVE);
    move_objects(VM, &[(addr(s), addr(s_moved))]);
    oos_write_value(&mut ctx, key, &Value::Object(Some(s_moved))).expect("second write");
    let all = oos_buf_snapshot(key);
    let mut back_ref = vec![TC_REFERENCE];
    back_ref.extend_from_slice(&BASE_WIRE_HANDLE.to_be_bytes());
    assert_eq!(
        &all[first.len()..],
        &back_ref[..],
        "a moved object is a back-reference"
    );

    // The object dies: its key is freed and its row goes from every writer.
    let s_moved_addr = addr(s_moved);
    crate::gc_sweep_lock_keys(VM, &|a| a != s_moved_addr);
    forget_serialization_keys(&[s_key]);
    let registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
    let state = registry.get(&key).expect("the live stream keeps its table");
    assert_eq!(
        state.lookup_handle(s_key),
        None,
        "a dead object's row is dropped"
    );
}

/// The reader's objects are global roots: a `TC_REFERENCE` in a later
/// `readObject` resolves through the root, `close()` releases them, and a
/// stream that dies unclosed hands them to its VM's retired list, which
/// only that VM drains.
#[test]
fn reader_objects_are_rooted_until_close_or_retired_with_the_stream() {
    const VM: usize = 0x5E21_0C30;
    const OTHER_VM: usize = 0x5E21_0C31;
    let mut ctx = vm_ctx(VM);
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    let read_object = native(
        &r,
        "java/io/ObjectInputStream",
        "readObject",
        "()Ljava/lang/Object;",
    );
    let close = native(&r, "java/io/ObjectInputStream", "close", "()V");
    let ois_class = ctx
        .ensure_class_initialized("java/io/ObjectInputStream")
        .unwrap();
    let ois = ctx.alloc_object(ois_class, 6);
    let ois2 = ctx.alloc_object(ois_class, 6);
    let key = stream_key(&ctx, ois);
    let key2 = stream_key(&ctx, ois2);
    let _forget = Forget {
        vm: VM,
        keys: vec![key, key2],
    };
    let mut wire = vec![TC_STRING, 0, 1, b'a', TC_REFERENCE];
    wire.extend_from_slice(&BASE_WIRE_HANDLE.to_be_bytes());
    ois_buf_load(key, wire);
    let roots = ctx.global_root_count();

    let first = read_object(&mut ctx, &[Value::Object(Some(ois))])
        .ok()
        .flatten();
    let second = read_object(&mut ctx, &[Value::Object(Some(ois))])
        .ok()
        .flatten();
    assert!(
        matches!(first, Some(Value::Object(Some(_)))),
        "read a string: {first:?}"
    );
    assert_eq!(first, second, "TC_REFERENCE resolves to the same object");
    assert_eq!(
        ctx.global_root_count(),
        roots + 1,
        "the reader holds its object"
    );
    close(&mut ctx, &[Value::Object(Some(ois))]).expect("close");
    assert_eq!(ctx.global_root_count(), roots, "close releases it");

    // A second reader dies unclosed.
    ois_buf_load(key2, vec![TC_STRING, 0, 1, b'b']);
    read_object(&mut ctx, &[Value::Object(Some(ois2))]).expect("read");
    assert_eq!(ctx.global_root_count(), roots + 1);
    forget_serialization_keys(&[key2]);
    let retired = |vm: usize| {
        serial_retired_roots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(owner, _)| *owner == vm)
            .count()
    };
    assert_eq!(
        retired(VM),
        1,
        "a dead reader's root is retired, not leaked"
    );
    let mut other = vm_ctx(OTHER_VM);
    serial_drain_retired_roots(&mut other);
    assert_eq!(retired(VM), 1, "another VM never releases this VM's roots");
    serial_drain_retired_roots(&mut ctx);
    assert_eq!(retired(VM), 0);
    assert_eq!(ctx.global_root_count(), roots, "the owner releases it");
}

/// `setObjectInputFilter` holds the filter as a root; replacing it or
/// passing null releases the old one.
#[test]
fn a_stream_filter_is_rooted_and_released_when_replaced() {
    const VM: usize = 0x5E21_0C40;
    let mut ctx = vm_ctx(VM);
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    let set = native(
        &r,
        "java/io/ObjectInputStream",
        "setObjectInputFilter",
        "(Ljava/io/ObjectInputFilter;)V",
    );
    let get = native(
        &r,
        "java/io/ObjectInputStream",
        "getObjectInputFilter",
        "()Ljava/io/ObjectInputFilter;",
    );
    let ois = ctx.fresh_object_ref();
    let (f1, f2) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let key = stream_key(&ctx, ois);
    let _forget = Forget {
        vm: VM,
        keys: vec![key],
    };
    let roots = ctx.global_root_count();
    set(
        &mut ctx,
        &[Value::Object(Some(ois)), Value::Object(Some(f1))],
    )
    .expect("set");
    set(
        &mut ctx,
        &[Value::Object(Some(ois)), Value::Object(Some(f2))],
    )
    .expect("replace");
    assert_eq!(
        ctx.global_root_count(),
        roots + 1,
        "the replaced filter is released"
    );
    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(ois))]).ok().flatten(),
        Some(Value::Object(Some(f2)))
    );
    set(&mut ctx, &[Value::Object(Some(ois)), Value::Object(None)]).expect("clear");
    assert_eq!(
        ctx.global_root_count(),
        roots,
        "a null filter releases the root"
    );
    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(ois))]).ok().flatten(),
        Some(Value::Object(None))
    );
}

/// The stream "this" for nested dispatch and a curObj frame's object are
/// read through the entry native's pins, so a move during the hook is
/// seen; leaving the native unregisters the stream and releases its pins.
#[test]
fn the_active_stream_and_frame_object_follow_their_pins() {
    let mut ctx = vm_ctx(0x5E21_0C50);
    let key = 0x5E21_0C58_usize;
    let (stream_old, stream_new) = (obj(0x1_5E21_0060), obj(0x1_5E21_0060 + MOVE));
    let (obj_old, obj_new) = (obj(0x1_5E21_0070), obj(0x1_5E21_0070 + MOVE));
    let pins = ctx.native_pin_count_for_test();
    let entry = enter_stream(&mut ctx, key, stream_old);
    let obj_pin = ctx.pin_native_root(obj_old);
    cur_push(
        key,
        CurFrame {
            obj_pin,
            obj: obj_old,
            class_id: ClassId::new(0),
            read_desc: None,
        },
    );
    ctx.remap_native_pin_addr_for_test(addr(stream_old), addr(stream_new));
    ctx.remap_native_pin_addr_for_test(addr(obj_old), addr(obj_new));
    assert_eq!(active_stream(&ctx, key), Some(stream_new));
    assert_eq!(cur_top(key).map(|f| f.current_obj(&ctx)), Some(obj_new));
    cur_pop(key);
    assert!(cur_top(key).is_none());
    entry.exit(&mut ctx);
    assert_eq!(
        active_stream(&ctx, key),
        None,
        "leaving unregisters the stream"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins,
        "and releases its pins"
    );
}
