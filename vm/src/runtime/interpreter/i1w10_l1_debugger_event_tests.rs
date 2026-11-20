use super::*;
use crate::classloading::{Class, ClassLoaderId, ClassState};
use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_reader::attribute::ExceptionTableEntry;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::field::ClassFileField;
use std::sync::mpsc::Receiver;

fn field(name: &str, desc: &str, flags: FieldAccessFlags) -> ClassFileField {
    ClassFileField {
        access_flags: flags,
        name: Arc::from(name),
        descriptor: Arc::from(desc),
        attributes: vec![],
    }
}

/// A class `name` declaring `fields` (instance slots from
/// `first_field_index`), with no superclass.
fn add_class(
    shared: &SharedVm,
    name: &str,
    fields: Vec<ClassFileField>,
    first_field_index: usize,
) -> ClassId {
    let instance = fields.iter().filter(|f| !f.is_static()).count();
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::empty(),
        superclass: None,
        interfaces: vec![],
        fields,
        methods: vec![],
        first_field_index,
        num_total_fields: first_field_index + instance,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

fn frame(class_id: ClassId, name: &str, table: Vec<ExceptionTableEntry>) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/W10Events".to_string(),
        name.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        table,
        4,
        2,
        &[],
    )
}

/// Register one request, publish the gates and attach an event channel.
fn arm(
    shared: &SharedVm,
    kind: EventKind,
    policy: SuspendPolicy,
    modifiers: Vec<EventModifier>,
) -> u32 {
    let mut ds = shared.debug.debug_state.lock();
    let id = ds.events.set_event_request(kind, policy, modifiers);
    crate::debug::publish_debugger_gates(shared, &ds);
    id
}

fn channel(shared: &SharedVm) -> Receiver<crate::debug::DebugEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    *shared.debug.debug_event_tx.lock().expect("event channel") = Some(tx);
    rx
}

fn disarm(shared: &SharedVm) {
    let mut ds = shared.debug.debug_state.lock();
    ds.events.clear_all();
    crate::debug::publish_debugger_gates(shared, &ds);
}

fn be64(bytes: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(b)
}

/// The `class.fields` position of a resolved field: statics counted
/// among statics, instance fields from `first_field_index`.
#[test]
fn a_resolved_field_maps_to_its_declared_position() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let inst = FieldAccessFlags::PRIVATE;
    let stat = FieldAccessFlags::PRIVATE | FieldAccessFlags::STATIC;
    let cid = add_class(
        &shared,
        "cratonvm/test/W10FieldPositions",
        vec![
            field("i", "I", inst),
            field("s", "J", stat),
            field("o", "Ljava/lang/Object;", inst),
        ],
        3,
    );
    let cm = shared.classes.class_manager.read();
    let class = cm.class_store.get(cid).expect("just added");
    assert_eq!(declared_field_position(class, false, 3), Some(0));
    assert_eq!(declared_field_position(class, false, 4), Some(2));
    assert_eq!(declared_field_position(class, true, 0), Some(1));
    assert_eq!(declared_field_position(class, false, 5), None);
    assert_eq!(
        declared_field_position(class, false, 2),
        None,
        "a superclass slot"
    );
    assert_eq!(declared_field_position(class, true, 1), None);
}

/// A JDWP `FieldModification` watch reports the field, the object and
/// the new value read in place (the operands stay on the stack); a
/// static's object is null; a watch on another field, or an access
/// where only modifications are watched, reports nothing. Arming raises
/// the process-wide field pre-filter, disarming lowers it.
#[test]
fn field_watches_report_the_field_object_and_new_value() {
    let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let inst = FieldAccessFlags::PRIVATE;
    let stat = FieldAccessFlags::PRIVATE | FieldAccessFlags::STATIC;
    let cid = add_class(
        &shared,
        "cratonvm/test/W10Watched",
        vec![
            field("x", "I", inst),
            field("s", "J", stat),
            field("y", "Ljava/lang/Object;", inst),
        ],
        0,
    );
    let class_wire = u64::from(cid.as_u32());
    let x_id = crate::debug::jdwp_field_id(cid, 0);
    let s_id = crate::debug::jdwp_field_id(cid, 1);
    let watch_x = arm(
        &shared,
        EventKind::FieldModification,
        SuspendPolicy::None,
        vec![EventModifier::FieldOnly {
            class_id: class_wire,
            field_id: x_id,
        }],
    );
    let watch_s = arm(
        &shared,
        EventKind::FieldModification,
        SuspendPolicy::None,
        vec![EventModifier::FieldOnly {
            class_id: class_wire,
            field_id: s_id,
        }],
    );
    assert!(shared.debug.debugger_gates.field_watch_armed());
    assert!(crate::runtime::jvmti::any_field_watchpoint_active());
    let rx = channel(&shared);
    let obj = shared.mem.heap.alloc_object(cid, 2);
    let resolved = |field_index: usize, is_static: bool, desc_byte: u8| ResolvedField {
        declaring_class_id: cid,
        field_index,
        is_static,
        is_volatile: false,
        is_final: false,
        is_reference: matches!(desc_byte, b'L' | b'['),
        desc_byte,
    };
    let mut thread = JvmThread::new(ThreadId(0), "w10-field-watch");
    let mut f = frame(cid, "watcher", Vec::new());
    f.last_instr_pc = 3;
    thread.frames.push(f);

    // putfield x = 7: [obj, 7].
    thread.frames[0]
        .stack
        .push(Value::Object(Some(obj)))
        .expect("push the receiver");
    thread.frames[0].stack.push(Value::Int(7)).expect("push 7");
    deliver_field_watch_if_armed(&shared, &mut thread, 0, &resolved(0, false, b'I'), true);
    assert_eq!(thread.frames[0].stack.len(), 2, "the operands stay");
    let evt = rx.try_recv().expect("one FieldModification event");
    assert_eq!(
        (evt.kind, evt.request_id, evt.offset),
        (EventKind::FieldModification, watch_x, 3)
    );
    let e = &evt.extra;
    assert_eq!(e.len(), 31, "tag, class, field, tagged object, tagged int");
    assert_eq!(e[0], 1, "refTypeTag CLASS");
    assert_eq!(be64(&e[1..]), class_wire);
    assert_eq!(be64(&e[9..]), x_id);
    assert_eq!(e[17], b'L');
    let object_id = be64(&e[18..]);
    {
        let ds = shared.debug.debug_state.lock();
        assert_eq!(
            crate::debug::object_for_id(&shared, &ds, object_id),
            Some(obj),
            "the object id names the receiver"
        );
    }
    assert_eq!(e[26], b'I');
    assert_eq!(&e[27..31], &7u32.to_be_bytes());
    assert!(rx.try_recv().is_err(), "one event");

    // getfield x: only modifications are watched.
    let _ = thread.frames[0].stack.pop();
    deliver_field_watch_if_armed(&shared, &mut thread, 0, &resolved(0, false, b'I'), false);
    // putfield y (unwatched): [obj, null].
    thread.frames[0]
        .stack
        .push(Value::Object(None))
        .expect("push null");
    deliver_field_watch_if_armed(&shared, &mut thread, 0, &resolved(1, false, b'L'), true);
    assert!(rx.try_recv().is_err(), "no access event, no event for y");

    // putstatic s = 9L: [9L]; the object is null.
    thread.frames[0].stack.clear();
    thread.frames[0]
        .stack
        .push(Value::Long(9))
        .expect("push 9L");
    deliver_field_watch_if_armed(&shared, &mut thread, 0, &resolved(0, true, b'J'), true);
    let evt = rx.try_recv().expect("the static's event");
    assert_eq!(evt.request_id, watch_s);
    let e = &evt.extra;
    assert_eq!(be64(&e[9..]), s_id);
    assert_eq!((e[17], be64(&e[18..])), (b'L', 0), "no object for a static");
    assert_eq!(e[26], b'J');
    assert_eq!(be64(&e[27..]), 9);

    disarm(&shared);
    assert!(!shared.debug.debugger_gates.field_watch_armed());
    assert!(
        !crate::runtime::jvmti::any_field_watchpoint_active(),
        "the pre-filter falls with the last watch"
    );
}

fn catch_all(start_pc: u16, end_pc: u16, handler_pc: u16) -> ExceptionTableEntry {
    ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc,
        catch_type: 0,
    }
}

/// An exception is reported where it is thrown, with the handler that
/// catches it as the catch location, or 0 when none does; it is not
/// reported again at the next interpreter entry it unwinds into, but is
/// after a handler caught it (a rethrow). `ExceptionOnly` filters by
/// class and by caught / uncaught.
#[test]
fn exception_events_report_throw_and_catch_locations_once() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/W10Thrown", 0)
        .expect("Compatible mode fabricates");
    let exc = shared.mem.heap.alloc_object(exc_class, 0);
    let owner = ClassId::new(0x00F1_0A01);
    let any = arm(
        &shared,
        EventKind::Exception,
        SuspendPolicy::None,
        vec![EventModifier::ExceptionOnly {
            class_id: 0,
            caught: true,
            uncaught: true,
        }],
    );
    let uncaught_only = arm(
        &shared,
        EventKind::Exception,
        SuspendPolicy::None,
        vec![EventModifier::ExceptionOnly {
            class_id: u64::from(exc_class.as_u32()),
            caught: false,
            uncaught: true,
        }],
    );
    assert!(shared.debug.debugger_gates.exception_events_armed());
    let rx = channel(&shared);
    let method_id = crate::debug::jdwp_method_id("thrower", "()V");

    // Caught at pc 5 by the catch-all [0, 10) -> 7.
    let mut t = JvmThread::new(ThreadId(0), "w10-exceptions");
    t.frames
        .push(frame(owner, "thrower", vec![catch_all(0, 10, 7)]));
    let mut idx = 0usize;
    unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 5).expect("caught");
    let events: Vec<_> = rx.try_iter().collect();
    assert_eq!(
        events.len(),
        1,
        "a caught exception: the unfiltered request only"
    );
    let evt = &events[0];
    assert_eq!(
        (evt.kind, evt.request_id, evt.offset, evt.method_id),
        (EventKind::Exception, any, 5, method_id)
    );
    let e = &evt.extra;
    assert_eq!(e.len(), 34, "tagged exception, then the catch location");
    assert_eq!(e[0], b'L');
    assert_ne!(be64(&e[1..]), 0);
    assert_eq!(e[9], 1, "catch location: CLASS");
    assert_eq!(be64(&e[10..]), u64::from(owner.as_u32()));
    assert_eq!(be64(&e[18..]), method_id);
    assert_eq!(be64(&e[26..]), 7, "the handler pc");

    // Caught, so a rethrow of the same object is a new throw.
    let mut t = JvmThread::new(ThreadId(0), "w10-exceptions");
    t.frames
        .push(frame(owner, "thrower", vec![catch_all(0, 10, 7)]));
    let mut idx = 0usize;
    unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 5).expect("caught again");
    assert_eq!(rx.try_iter().count(), 1, "the rethrow is reported");

    // Uncaught: no handler anywhere on the thread.
    let mut t = JvmThread::new(ThreadId(0), "w10-exceptions");
    t.frames.push(frame(owner, "thrower", Vec::new()));
    let mut idx = 0usize;
    assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 5).is_err());
    let events: Vec<_> = rx.try_iter().collect();
    let mut ids: Vec<u32> = events.iter().map(|e| e.request_id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![any, uncaught_only], "both requests report it");
    assert!(
        events
            .iter()
            .all(|e| e.extra.len() == 34 && e.extra[10..].iter().all(|&b| b == 0)),
        "catch location 0: uncaught"
    );

    // The same exception reaching the unwinder of an outer interpreter
    // entry on the same thread is not a new throw (wave 11: the state is
    // the thread's own, `JvmThread::reported_exception`).
    assert_ne!(t.reported_exception, 0, "the thread remembers it");
    t.frames.push(frame(owner, "outer", Vec::new()));
    let mut idx = 1usize;
    assert!(unwind_to_handler(&shared, &mut t, &mut idx, 1, exc, 2).is_err());
    assert_eq!(rx.try_iter().count(), 0, "reported once, where thrown");

    disarm(&shared);
    assert!(!shared.debug.debugger_gates.exception_events_armed());
}

/// A JVMTI breakpoint arms the dispatch loop's per-method gate on its
/// own, with no JDWP request in force, and the agent's `Breakpoint`
/// callback runs once per execution of its pc — eleven times on the
/// `if_icmpge` inside javac's fusable loop test for a ten-trip loop
/// (fusion declines while the gate is armed). `DUPLICATE` / `NOT_FOUND`
/// as the spec says, and clearing it disarms the method again.
#[test]
fn a_jvmti_breakpoint_fires_on_every_execution_of_its_pc() {
    use crate::runtime::jvmti::{self, EventCallbacks, EventMode, JvmtiError, JvmtiEventKind};
    let _lock = jvmti::jvmti_registry_test_lock();
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let vm = shared.vm_identity;
    jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
    let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Breakpoint, None)
        .expect("enable Breakpoint");
    let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&hits);
    mgr.set_event_callbacks(EventCallbacks {
        breakpoint: Some(Box::new(move |_, _, location| {
            seen.lock().expect("hit list").push(location);
        })),
        ..Default::default()
    })
    .expect("install callbacks");
    let cid = ClassId::new(0x00F1_0A03);
    let class_wire = u64::from(cid.as_u32());
    let key = crate::debug::jdwp_method_id("jvmtiBpLoop", "()I");
    jvmti::add_breakpoint_key(&shared, class_wire, key, 9).expect("set");
    assert_eq!(
        jvmti::add_breakpoint_key(&shared, class_wire, key, 9),
        Err(JvmtiError::Duplicate)
    );
    assert!(shared
        .debug
        .debugger_gates
        .concerns_method(cid.as_u32(), "jvmtiBpLoop", "()I"));
    let code = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    let mut thread = JvmThread::new(ThreadId(0), "w10-jvmti-breakpoint");
    let frame = Frame::new(
        cid,
        "cratonvm/test/W10Events".to_string(),
        "jvmtiBpLoop".to_string(),
        "()I".to_string(),
        None,
        code,
        Vec::new(),
        2,
        3,
        &[],
    );
    let r = execute_prebuilt_frame(&shared, &mut thread, frame);
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    assert_eq!(
        *hits.lock().expect("hit list"),
        vec![9i64; 11],
        "one callback per execution of pc 9"
    );
    jvmti::remove_breakpoint_key(&shared, class_wire, key, 9).expect("clear");
    assert_eq!(
        jvmti::remove_breakpoint_key(&shared, class_wire, key, 9),
        Err(JvmtiError::NotFound)
    );
    assert!(!shared
        .debug
        .debugger_gates
        .concerns_method(cid.as_u32(), "jvmtiBpLoop", "()I"));
    // An id naming no method is refused before anything is stored.
    assert_eq!(
        jvmti::set_breakpoint_for_vm(&shared, u64::from(cid.as_u32()) << 32, 0),
        Err(JvmtiError::InvalidMethodId)
    );
    let _ =
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Breakpoint, None);
    jvmti::forget_vm_jvmti_state(vm);
}

/// While a debugger is attached, a thread entering a blocking native
/// region publishes its frames (top frame at the invoke of the native)
/// and their locals if it is already suspended, and withdraws them when
/// it leaves; a thread that is not suspended only opens its inspection
/// window (wave 12: it published a snapshot on every blocking call).
/// Without a debugger nothing happens.
#[test]
fn a_blocked_thread_publishes_its_frames_while_a_debugger_is_attached() {
    use std::sync::atomic::Ordering;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let owner = ClassId::new(0x00F1_0A04);
    let mut thread = JvmThread::new(ThreadId(0), "w10-blocked");
    let mut f = frame(owner, "sleeper", Vec::new());
    f.last_instr_pc = 4;
    f.set_local(0, Value::Int(5));
    thread.frames.push(f);
    let window = || {
        thread
            .gc_block_state
            .debugger_inspect
            .load(Ordering::Acquire)
    };
    assert!(!publish_blocked_frames(&shared, &thread));
    open_blocked_inspection(&shared, &thread, false);
    assert_eq!(window(), 0, "no debugger: no window");
    assert!(
        shared.debug.debug_state.lock().thread_frames.is_empty(),
        "no debugger: nothing published"
    );

    // Attached, not suspended: a window naming the thread, no snapshot.
    shared.debug.debugger_gates.set_session_attached(true);
    assert!(!publish_blocked_frames(&shared, &thread));
    open_blocked_inspection(&shared, &thread, false);
    assert_eq!(window() & INSPECT_FLAGS, INSPECT_OPEN);
    // Cast: the window's address half
    assert_eq!(
        window() & !INSPECT_FLAGS,
        &thread as *const JvmThread as usize
    );
    assert!(
        shared.debug.debug_state.lock().thread_frames.is_empty(),
        "not suspended: nothing published"
    );
    close_blocked_inspection(&shared, &thread);
    assert_eq!(window(), 0, "the leave closes it");

    // Suspended before it blocks: published on entry, withdrawn on leave.
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.suspend_thread(0);
        crate::debug::publish_debugger_gates(&shared, &ds);
    }
    assert!(shared.debug.debugger_gates.suspension_in_force());
    let published = publish_blocked_frames(&shared, &thread);
    assert!(published);
    open_blocked_inspection(&shared, &thread, published);
    assert_eq!(window() & INSPECT_FLAGS, INSPECT_OPEN | INSPECT_PUBLISHED);
    {
        let ds = shared.debug.debug_state.lock();
        let top = ds.thread_frames.get(&0).and_then(|f| f.first()).cloned();
        assert_eq!(
            top.map(|e| (e.frame_id, e.class_id, e.offset)),
            Some((0, u64::from(owner.as_u32()), 4))
        );
        assert_eq!(
            ds.get_frame_locals(crate::debug::ids::ThreadId(0), 0)
                .and_then(|l| l.first())
                .and_then(|v| v.as_int()),
            Some(5)
        );
    }
    close_blocked_inspection(&shared, &thread);
    assert_eq!(window(), 0);
    {
        let ds = shared.debug.debug_state.lock();
        assert!(ds.thread_frames.is_empty() && ds.frame_locals.is_empty());
    }

    // Wave 28: resumed and suspended again, the same frame is listed
    // under the thread's next frame generation, and the id of the first
    // listing is refused (`INVALID_FRAMEID`), as HotSpot refuses it.
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.resume_thread(0);
        ds.suspend_thread(0);
        crate::debug::publish_debugger_gates(&shared, &ds);
    }
    let published = publish_blocked_frames(&shared, &thread);
    assert!(published);
    open_blocked_inspection(&shared, &thread, published);
    {
        use crate::debug::commands::{
            dispatch, CMD_SF_GET_VALUES, CS_STACK_FRAME, ERR_INVALID_FRAMEID, ERR_NONE,
        };
        let mut ds = shared.debug.debug_state.lock();
        let top = ds
            .thread_frames
            .get(&0)
            .and_then(|f| f.first())
            .map(|e| e.frame_id);
        assert_eq!(top, Some(1u64 << 32), "generation 1, position 0");
        let get = |ds: &mut crate::debug::DebugState, frame: u64| {
            let mut pw = crate::debug::protocol::PayloadWriter::new();
            pw.put_u64_be(0); // thread
            pw.put_u64_be(frame);
            pw.put_u32_be(1);
            pw.put_u32_be(0); // slot
            pw.put_u8(b'I');
            dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), ds)
        };
        assert_eq!(get(&mut *ds, 0).error_code, ERR_INVALID_FRAMEID, "stale id");
        let fresh = get(&mut *ds, 1u64 << 32);
        assert_eq!(fresh.error_code, ERR_NONE);
        assert_eq!(fresh.data, [0, 0, 0, 1, b'I', 0, 0, 0, 5]);
    }
    close_blocked_inspection(&shared, &thread);
    assert_eq!(window(), 0);
    let mut ds = shared.debug.debug_state.lock();
    assert!(ds.thread_frames.is_empty() && ds.frame_locals.is_empty());
    ds.resume_all();
    crate::debug::publish_debugger_gates(&shared, &ds);
    drop(ds);
    assert!(!shared.debug.debugger_gates.suspension_in_force());
    shared.debug.debugger_gates.set_session_attached(false);
}

/// Wave 12: a thread suspended while it is blocked has its frames read on
/// demand through its inspection window, a reference local as the
/// object's current address from the blocking deposit's per-slot record
/// (a slot the deposit found dead as null), once; a reader holding the
/// window keeps the thread's leave waiting, and the leave withdraws the
/// snapshot.
#[test]
fn a_thread_suspended_while_blocked_has_its_frames_read_on_demand() {
    use crate::threading::jvm_thread::SlotOrigin;
    use std::sync::atomic::Ordering;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let cid = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/W12Held", 0)
        .expect("Compatible mode fabricates");
    let moved_from = shared.mem.heap.alloc_object(cid, 0);
    let moved_to = shared.mem.heap.alloc_object(cid, 0);
    let dead = shared.mem.heap.alloc_object(cid, 0);
    let tid = ThreadId(0x0C12);
    let t = tid.0;
    let owner = ClassId::new(0x00F1_0C01);
    let mut thread = JvmThread::new(tid, "w12-blocked");
    let mut f = Frame::new(
        owner,
        "cratonvm/test/W12Events".to_string(),
        "waiter".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        Vec::new(),
        2,
        3,
        &[],
    );
    f.last_instr_pc = 6;
    f.set_local(0, Value::Int(9));
    f.set_local(1, Value::Object(Some(moved_from)));
    f.set_local(2, Value::Object(Some(dead)));
    thread.frames.push(f);
    let registry = &shared.threads.thread_registry;
    registry.register(tid, "w12-blocked", None);
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
    // What the blocking deposit recorded, after a collection moved the
    // object local 1 names.
    // Cast: addresses, the deposit's keys
    let (from, to, gone) = (
        moved_from.as_ptr() as usize,
        moved_to.as_ptr() as usize,
        dead.as_ptr() as usize,
    );
    *thread.gc_block_state.slot_origins.lock() = vec![
        SlotOrigin {
            frame: 0,
            idx: 1,
            is_stack: false,
            live: true,
            orig: from,
            cur: to,
        },
        SlotOrigin {
            frame: 0,
            idx: 2,
            is_stack: false,
            live: false,
            orig: gone,
            cur: gone,
        },
    ];
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    shared.debug.debugger_gates.set_session_attached(true);

    // Blocked, not suspended: a window, nothing to read yet.
    assert!(!publish_blocked_frames(&shared, &thread));
    open_blocked_inspection(&shared, &thread, false);
    assert!(blocked_frames_readable(&shared, t));
    assert!(
        !publish_frames_of_blocked_thread(&shared, t),
        "not suspended"
    );
    assert!(blocked_frames_readable(&shared, t), "the window is kept");

    // A reader holding the window keeps the leave waiting.
    let word = &thread.gc_block_state.debugger_inspect;
    let open = word.fetch_or(INSPECT_READING, Ordering::AcqRel);
    std::thread::scope(|s| {
        let leaver = s.spawn(|| close_inspection_window(word));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!leaver.is_finished(), "the leave waits for the reader");
        word.store(open, Ordering::Release);
        assert!(!leaver.join().expect("the leave"), "nothing was published");
    });
    assert_eq!(word.load(Ordering::Acquire), 0);
    open_blocked_inspection(&shared, &thread, false);

    // Suspended while blocked: read on demand, once.
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.suspend_all();
        crate::debug::publish_debugger_gates(&shared, &ds);
    }
    assert!(publish_frames_of_blocked_thread(&shared, t));
    assert_ne!(word.load(Ordering::Acquire) & INSPECT_PUBLISHED, 0);
    assert!(
        !publish_frames_of_blocked_thread(&shared, t),
        "published once: the snapshot stands"
    );
    {
        let ds = shared.debug.debug_state.lock();
        let top = ds.thread_frames.get(&t).and_then(|f| f.first()).cloned();
        assert_eq!(
            top.map(|e| (e.frame_id, e.class_id, e.method_id, e.offset)),
            Some((
                0,
                u64::from(owner.as_u32()),
                crate::debug::jdwp_method_id("waiter", "()V"),
                6
            ))
        );
        let locals = ds
            .get_frame_locals(crate::debug::ids::ThreadId(t), 0)
            .cloned()
            .expect("locals published");
        assert_eq!(locals.len(), 3);
        assert_eq!(locals[0].as_int(), Some(9));
        // Wave 126 (lane L1, i47-L1, `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS`,
        // default on): a dead slot reads its object while the tracked
        // address is still an object start, as HotSpot's interpreter
        // keeps the local; `=0`: null, as before. Wave 130 (lane L1a):
        // also with `CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS` (default
        // on) in a VM with a JDWP port, where every local is a root.
        if blocked_dead_slots_read(&shared) {
            let id = locals[2].as_object_id().expect("an object id");
            assert_ne!(id, 0, "a dead slot reads its object");
            assert_eq!(crate::debug::object_for_id(&shared, &ds, id), Some(dead));
        } else {
            assert_eq!(locals[2].as_object_id(), Some(0), "a dead slot reads null");
        }
        if shared.mem.heap.is_object_address(to).is_some() {
            let id = locals[1].as_object_id().expect("an object id");
            assert_ne!(id, 0);
            assert_eq!(
                crate::debug::object_for_id(&shared, &ds, id),
                Some(moved_to),
                "the object's current address"
            );
        }
    }

    // Wave 28: resumed and suspended again while still blocked, the
    // standing listing belongs to the old suspension: it is read again,
    // under the thread's next frame generation, once.
    {
        let mut ds = shared.debug.debug_state.lock();
        let before = ds
            .thread_frames
            .get(&t)
            .and_then(|f| f.first())
            .map(|e| e.frame_id);
        assert_eq!(before, Some(0));
        ds.resume_vm();
        ds.suspend_all();
        crate::debug::publish_debugger_gates(&shared, &ds);
        assert!(!ds.has_current_frames(t));
    }
    assert!(
        publish_frames_of_blocked_thread(&shared, t),
        "a listing from before the resume is read again"
    );
    assert!(
        !publish_frames_of_blocked_thread(&shared, t),
        "once per suspension"
    );
    {
        let ds = shared.debug.debug_state.lock();
        assert!(ds.has_current_frames(t));
        let top = ds
            .thread_frames
            .get(&t)
            .and_then(|f| f.first())
            .map(|e| e.frame_id);
        assert_eq!(top, Some(1u64 << 32), "generation 1, position 0");
    }

    // The leave withdraws it.
    close_blocked_inspection(&shared, &thread);
    assert_eq!(word.load(Ordering::Acquire), 0);
    {
        let mut ds = shared.debug.debug_state.lock();
        assert!(!ds.thread_frames.contains_key(&t));
        assert!(ds.frame_locals.keys().all(|&(of, _)| of != t));
        ds.resume_all();
        crate::debug::publish_debugger_gates(&shared, &ds);
    }
    assert!(!publish_frames_of_blocked_thread(&shared, t), "closed");
    shared.debug.debugger_gates.set_session_attached(false);
    thread
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
    registry.mark_dead(tid);
}

/// A suspending `Exception` request parks the thread at the throw site
/// with its frames published, before the unwind; after the resume the
/// handler receives the (pinned) exception.
#[test]
fn a_suspending_exception_request_parks_at_the_throw() {
    use std::time::{Duration, Instant};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/W10ParkThrown", 0)
        .expect("Compatible mode fabricates");
    let exc = shared.mem.heap.alloc_object(exc_class, 0);
    let owner = ClassId::new(0x00F1_0A02);
    let _ = arm(
        &shared,
        EventKind::Exception,
        SuspendPolicy::EventThread,
        vec![],
    );
    let rx = channel(&shared);
    let (caught, observed) = std::thread::scope(|s| {
        let resumer = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                {
                    let mut ds = shared.debug.debug_state.lock();
                    if ds.is_parked(0) {
                        let top = ds
                            .thread_frames
                            .get(&0)
                            .and_then(|f| f.first())
                            .map(|e| e.offset);
                        ds.resume_thread(0);
                        return top;
                    }
                    if Instant::now() > deadline {
                        ds.resume_all();
                        return None;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let mut t = JvmThread::new(ThreadId(0), "w10-exception-park");
        t.frames
            .push(frame(owner, "parker", vec![catch_all(0, 10, 8)]));
        let mut idx = 0usize;
        let r = unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 4);
        let pins_left = t.native_pin_roots.len();
        let top = t.frames[0].stack.pop().ok();
        (
            (r.is_ok(), pins_left, top, t.frames[0].pc),
            resumer.join().expect("the resumer thread"),
        )
    });
    assert_eq!(
        observed,
        Some(4),
        "parked at the throw pc, frames published"
    );
    assert_eq!(
        caught,
        (true, 0, Some(Value::Object(Some(exc))), 8),
        "the handler holds the exception; no pin is left"
    );
    assert_eq!(rx.try_iter().count(), 1);
    disarm(&shared);
}

/// A class `name` of the application loader declaring `methods` (each
/// with no code) and `pool` as its constant pool (wave 15).
fn class_with(
    shared: &SharedVm,
    name: &str,
    pool: Vec<ConstantPoolEntry>,
    methods: Vec<(
        &str,
        &str,
        cratonvm_reader::class_access_flags::MethodAccessFlags,
    )>,
) -> ClassId {
    let id = add_class(shared, name, Vec::new(), 0);
    let mut cm = shared.classes.class_manager.write();
    let class = cm.get_class_mut(id).expect("just added");
    class.constant_pool = ConstantPool::new(pool);
    class.methods = methods
        .into_iter()
        .map(
            |(name, desc, flags)| cratonvm_reader::method::ClassFileMethod {
                access_flags: flags,
                name: Arc::from(name),
                descriptor: Arc::from(desc),
                attributes: Vec::new(),
            },
        )
        .collect();
    id
}

/// Wave 15 (the suspension page's native-method frame): a thread
/// suspended while blocked in a native method lists that method first,
/// at location -1 — named from the invoke at its caller's
/// `last_instr_pc`, only when no override can replace it — and its
/// interpreter frames from id 1; `GetValues` on the native frame is
/// `OPAQUE_FRAME`, and the caller's locals keep answering under its id.
#[test]
fn a_thread_blocked_in_a_native_method_lists_the_native_frame() {
    use crate::debug::commands::{
        dispatch, CMD_SF_GET_VALUES, CMD_TR_FRAMES, CS_STACK_FRAME, CS_THREAD_REF, ERR_NONE,
        ERR_OPAQUE_FRAME,
    };
    use crate::debug::protocol::PayloadWriter;
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let natives = class_with(
        &shared,
        "cratonvm/test/W15Natives",
        vec![ConstantPoolEntry::Tombstone],
        vec![
            ("sleep0", "(J)V", M::STATIC | M::NATIVE),
            ("virt", "()V", M::PUBLIC | M::NATIVE),
            ("spin", "()V", M::STATIC),
        ],
    );
    let caller = class_with(
        &shared,
        "cratonvm/test/W15NativeCaller",
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("cratonvm/test/W15Natives".into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("sleep0".into()),
            ConstantPoolEntry::Utf8("(J)V".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
            ConstantPoolEntry::Utf8("virt".into()),
            ConstantPoolEntry::Utf8("()V".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 7,
                descriptor_index: 8,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 9,
            },
            ConstantPoolEntry::Utf8("spin".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 11,
                descriptor_index: 8,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 12,
            },
        ],
        vec![("run", "()V", M::STATIC)],
    );
    // invokestatic #6 (sleep0); invokevirtual #10 (virt);
    // invokespecial #10 (virt); invokestatic #13 (spin); return.
    let code = vec![0xb8, 0, 6, 0xb6, 0, 10, 0xb7, 0, 10, 0xb8, 0, 13, 0xb1];
    let mut top = Frame::new(
        caller,
        "cratonvm/test/W15NativeCaller".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        code,
        Vec::new(),
        4,
        1,
        &[],
    );
    let natives_wire = u64::from(natives.as_u32());
    let sleep0 = Some((natives_wire, crate::debug::jdwp_method_id("sleep0", "(J)V")));
    let at = |top: &mut Frame, pc: usize| {
        top.last_instr_pc = pc;
        blocked_native_method(&shared, top, 0)
    };
    assert_eq!(at(&mut top, 0), sleep0, "a static native");
    assert_eq!(at(&mut top, 3), None, "a virtual call an override may take");
    assert_eq!(
        at(&mut top, 6),
        Some((natives_wire, crate::debug::jdwp_method_id("virt", "()V"))),
        "invokespecial names its method"
    );
    assert_eq!(at(&mut top, 9), None, "not a native");
    assert_eq!(at(&mut top, 12), None, "not an invoke");

    // Suspended while it blocks in `sleep0`: published on entry.
    top.last_instr_pc = 0;
    top.set_local(0, Value::Int(3));
    let tid = 0x0F15;
    let mut thread = JvmThread::new(ThreadId(tid), "w15-native");
    thread.frames.push(top);
    shared.debug.debugger_gates.set_session_attached(true);
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.thread_names.insert(tid, "w15-native".into());
        ds.suspend_thread(tid);
        crate::debug::publish_debugger_gates(&shared, &ds);
    }
    assert!(publish_blocked_frames(&shared, &thread));
    let mut ds = shared.debug.debug_state.lock();
    let listed: Vec<(u64, u64, u64)> = ds
        .thread_frames
        .get(&tid)
        .map(|frames| {
            frames
                .iter()
                .map(|f| (f.frame_id, f.class_id, f.offset))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        listed,
        vec![
            (0, natives_wire, crate::debug::NATIVE_FRAME_LOCATION),
            (1, u64::from(caller.as_u32()), 0),
        ]
    );
    let mut frames = PayloadWriter::new();
    frames.put_u64_be(tid);
    frames.put_u32_be(0);
    frames.put_u32_be(u32::MAX); // -1: all
    let res = dispatch(CS_THREAD_REF, CMD_TR_FRAMES, &frames.into_bytes(), &mut ds);
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(be64(&res.data[29..37]), u64::MAX, "location index -1");
    let get = |ds: &mut crate::debug::DebugState, frame: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(tid);
        pw.put_u64_be(frame);
        pw.put_u32_be(1);
        pw.put_u32_be(0);
        pw.put_u8(b'I');
        dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), ds)
    };
    assert_eq!(get(&mut ds, 0).error_code, ERR_OPAQUE_FRAME);
    let caller_locals = get(&mut ds, 1);
    assert_eq!(caller_locals.error_code, ERR_NONE);
    assert_eq!(caller_locals.data, vec![0, 0, 0, 1, b'I', 0, 0, 0, 3]);
    ds.resume_all();
    crate::debug::publish_debugger_gates(&shared, &ds);
    let stale = withdraw_debugger_frames(&mut ds, tid);
    for id in stale {
        ds.objects.unpin(id);
    }
    drop(ds);
    shared.debug.debugger_gates.set_session_attached(false);
}

/// Wave 17 (the running-native record): a virtual call to an overridable
/// native is named from the callback the thread runs, as the resolved
/// class's native or a loaded subclass's override, whichever registers
/// that callback; with no record, or a callback no candidate registers,
/// the caller stays the top.
#[test]
fn a_virtual_call_to_an_overridable_native_is_named_from_the_running_callback() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    fn base_virt(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(17)))
    }
    fn sub_virt(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(1717)))
    }
    fn unrelated(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Long(17)))
    }
    const BASE: &str = "cratonvm/test/W17VirtBase";
    const SUB: &str = "cratonvm/test/W17VirtSub";
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    let registry = &mut vm.natives.native_methods;
    registry.register(BASE, "virt", "()V", base_virt, NativeKind::Bridge);
    registry.register(SUB, "virt", "()V", sub_virt, NativeKind::Bridge);
    let shared = Arc::new(vm);
    let native = vec![("virt", "()V", M::PUBLIC | M::NATIVE)];
    let base = class_with(
        &shared,
        BASE,
        vec![ConstantPoolEntry::Tombstone],
        native.clone(),
    );
    let sub = class_with(&shared, SUB, vec![ConstantPoolEntry::Tombstone], native);
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(sub, Some(base));
    let caller = class_with(
        &shared,
        "cratonvm/test/W17VirtCaller",
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(BASE.into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("virt".into()),
            ConstantPoolEntry::Utf8("()V".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
        ],
        vec![("run", "()V", M::STATIC)],
    );
    // invokevirtual #6 (virt); return.
    let mut top = Frame::new(
        caller,
        "cratonvm/test/W17VirtCaller".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0xb6, 0, 6, 0xb1],
        Vec::new(),
        4,
        1,
        &[],
    );
    top.last_instr_pc = 0;
    let virt = crate::debug::jdwp_method_id("virt", "()V");
    let named = |running: usize| blocked_native_method(&shared, &top, running);
    assert_eq!(named(0), None, "no record: an override may have been taken");
    assert_eq!(
        named(base_virt as usize), // Cast: fn address
        Some((u64::from(base.as_u32()), virt))
    );
    assert_eq!(
        named(sub_virt as usize), // Cast: fn address
        Some((u64::from(sub.as_u32()), virt)),
        "the subclass's override"
    );
    assert_eq!(named(unrelated as usize), None, "no candidate runs it"); // Cast: fn address
}

/// Wave 17: the native-call funnel records the running native only while
/// a debugger is attached, and an upcall's native restores the outer one.
#[test]
fn the_running_native_is_recorded_only_while_a_debugger_is_attached() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0x0F17), "w17-running-native");
    assert_eq!(enter_running_native(&shared, &mut thread, 0x1000), None);
    assert_eq!(thread.running_native, 0);
    shared.debug.debugger_gates.set_session_attached(true);
    let outer = enter_running_native(&shared, &mut thread, 0x1000);
    let inner = enter_running_native(&shared, &mut thread, 0x2000);
    assert_eq!(thread.running_native, 0x2000);
    leave_running_native(&mut thread, inner);
    assert_eq!(thread.running_native, 0x1000, "the outer native again");
    leave_running_native(&mut thread, outer);
    assert_eq!(thread.running_native, 0);
    shared.debug.debugger_gates.set_session_attached(false);
}

/// Interpreter round i2 wave 61, lane L6: the funnel's callback record
/// another thread reads (`GcBlockState::funnel_native_callback`) is kept
/// only while the funnel records, nests, and is gone after the outer
/// native's leave; the registry answers it.
#[test]
fn i61_l6_the_funnel_native_callback_is_recorded_and_restored() {
    use std::sync::atomic::Ordering;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0x0F61), "i61-funnel-callback");
    let cell = |t: &JvmThread| {
        t.gc_block_state
            .funnel_native_callback
            .load(Ordering::Acquire)
    };
    assert!(!shared.debug.debugger_gates.running_native_armed());
    assert_eq!(enter_running_native(&shared, &mut thread, 0x1000), None);
    assert_eq!(cell(&thread), 0, "nothing recorded while nobody asks");
    shared.debug.debugger_gates.set_session_attached(true);
    assert!(shared.debug.debugger_gates.running_native_armed());
    let outer = enter_running_native(&shared, &mut thread, 0x1000);
    let inner = enter_running_native(&shared, &mut thread, 0x2000);
    assert_eq!(cell(&thread), 0x2000);
    leave_running_native(&mut thread, inner);
    assert_eq!(cell(&thread), 0x1000, "the outer native again");
    leave_running_native(&mut thread, outer);
    assert_eq!(cell(&thread), 0);
    shared.debug.debugger_gates.set_session_attached(false);
    assert!(!shared.debug.debugger_gates.running_native_armed());
    // No registration runs a made-up address.
    assert!(!funnel_callback_runs_a_native_method(&shared, 0x2000));
    assert!(!funnel_callback_runs_a_native_method(&shared, 0));
}

/// Interpreter round i2 wave 53, lane L6: the thread's acknowledgement of
/// a JDWP suspension in a native (`GcBlockState::debugger_native_ack`) is
/// lowered while Java the native runs executes and put back after; a
/// thread under no native pays one load and keeps nothing; leaving a
/// native restores what its caller had.
#[test]
fn java_a_native_runs_lowers_the_native_acknowledgement_while_it_runs() {
    use std::sync::atomic::Ordering;
    let mut thread = JvmThread::new(ThreadId(0x0F53), "w53-native-ack");
    let ack = |t: &JvmThread| t.gc_block_state.debugger_native_ack.load(Ordering::Acquire);
    {
        let guard = NestedJavaAck::enter(&thread);
        assert!(guard.state.is_none(), "under no native");
    }
    assert!(!ack(&thread));
    // In a native that acknowledged.
    thread.running_native = 0x1000;
    thread
        .gc_block_state
        .debugger_native_ack
        .store(true, Ordering::Release);
    {
        let _outer = NestedJavaAck::enter(&thread);
        assert!(!ack(&thread), "its Java runs");
        {
            let inner = NestedJavaAck::enter(&thread);
            assert!(inner.state.is_none(), "already lowered");
        }
        assert!(!ack(&thread));
    }
    assert!(ack(&thread), "back in the native");
    // Leaving the native puts back what its caller had.
    leave_running_native(
        &mut thread,
        Some(RunningNativePrior {
            callback: 0,
            ack: false,
            window: false,
            funnel: 0,
        }),
    );
    assert_eq!(thread.running_native, 0);
    assert!(!ack(&thread));
}

/// Interpreter round i2 wave 58, lane L1: the C JVMTI table's in-native
/// mark (`GcBlockState::jvmti_in_native`) is lowered while Java a JNI
/// native calls runs and put back after, nested upcalls included; the
/// debugger's acknowledgement is left as it was.
#[test]
fn java_a_native_runs_lowers_the_jvmti_in_native_mark_while_it_runs() {
    use std::sync::atomic::Ordering;
    let mut thread = JvmThread::new(ThreadId(0x0F58), "w58-in-native");
    let mark = |t: &JvmThread| t.gc_block_state.jvmti_in_native.load(Ordering::Acquire);
    // What `jvmti::native_env::CInNativeMark` records for a JNI native.
    thread.running_native = 1;
    thread
        .gc_block_state
        .jvmti_in_native
        .store(true, Ordering::Release);
    {
        let outer = NestedJavaAck::enter(&thread);
        assert!(outer.state.is_some());
        assert!(!mark(&thread), "the upcall's Java is not in native");
        {
            let inner = NestedJavaAck::enter(&thread);
            assert!(inner.state.is_none(), "already lowered");
        }
        assert!(!mark(&thread));
    }
    assert!(mark(&thread), "back in the native");
    assert!(
        !thread
            .gc_block_state
            .debugger_native_ack
            .load(Ordering::Acquire),
        "the acknowledgement is not raised by the mark's restore"
    );
}

/// Interpreter round i2 wave 56, lane L6: the frame stack's one
/// relocation shuts a native frames window -- no new reader -- keeping
/// its published mark for the leave, and waits out a reader first.
#[cfg(feature = "experimental-debug")]
#[test]
fn i56_l6_a_relocating_frame_stack_shuts_the_native_frames_window() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let word = std::sync::Arc::new(AtomicUsize::new(NW_OPEN | NW_PUBLISHED));
    let mut frames = crate::runtime::frame::FrameStack::new();
    frames.set_native_window(&*word as *const AtomicUsize as usize);
    frames.reserve_stable(1);
    assert_eq!(word.load(Ordering::Acquire), NW_PUBLISHED);
    // A reader holds it: the growth waits until it lets go.
    word.store(NW_OPEN | NW_READING, Ordering::Release);
    let reader = {
        let word = std::sync::Arc::clone(&word);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            word.fetch_and(!NW_READING, Ordering::AcqRel);
        })
    };
    frames.reserve_stable(frames.stable_headroom() + 1);
    reader.join().unwrap();
    assert_eq!(
        word.load(Ordering::Acquire),
        0,
        "shut once the reader let go"
    );
    frames.set_native_window(0);
}

/// Wave 56 (lane L6): Java a native runs shuts the native's frames
/// window (the listing would not show that Java), keeping its published
/// mark; the native's acknowledgement is lowered as before.
#[cfg(feature = "experimental-debug")]
#[test]
fn i56_l6_java_a_native_runs_shuts_its_frames_window() {
    use std::sync::atomic::Ordering;
    let mut thread = JvmThread::new(ThreadId(0x0F56), "w56-native-window");
    thread.running_native = 0x1000;
    let state = std::sync::Arc::clone(&thread.gc_block_state);
    state.debugger_native_ack.store(true, Ordering::Release);
    state
        .debugger_native_window
        .store(NW_OPEN | NW_PUBLISHED, Ordering::Release);
    {
        let _java = NestedJavaAck::enter(&thread);
        assert_eq!(
            state.debugger_native_window.load(Ordering::Acquire),
            NW_PUBLISHED
        );
        assert!(!state.debugger_native_ack.load(Ordering::Acquire));
    }
    assert!(state.debugger_native_ack.load(Ordering::Acquire));
    assert_eq!(
        state.debugger_native_window.load(Ordering::Acquire),
        NW_PUBLISHED,
        "not reopened: the native's leave closes it"
    );
}

/// Interpreter round i2 wave 57, lane L6: a native frames window's frames
/// stay current across pauses that move nothing (wave 56 refused every
/// read after a loop-exit pause), go stale after a collection that moved
/// objects, and are current again once the thread remapped them; a shut
/// window is not refreshed.
#[cfg(feature = "experimental-debug")]
#[test]
fn i57_l6_native_frames_stay_current_across_pauses_that_move_nothing() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let thread = JvmThread::new(ThreadId(0x0F57), "w57-frames-current");
    let state = std::sync::Arc::clone(&thread.gc_block_state);
    let barrier = &shared.mem.gc_barrier;
    let pause = |moved: bool| {
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        let mut pm = cratonvm_types::PointerMap::default();
        if moved {
            pm.insert(0x1000, 0x2000);
        }
        barrier.complete_gc(pm);
    };
    state
        .debugger_native_window
        .store(NW_OPEN, Ordering::Release);
    note_native_frames_current(&shared, &thread);
    assert!(!native_frames_stale(&shared, &state));
    pause(false);
    assert!(!native_frames_stale(&shared, &state), "nothing moved");
    pause(true);
    assert!(
        native_frames_stale(&shared, &state),
        "a collection moved objects"
    );
    note_native_frames_current(&shared, &thread);
    assert!(!native_frames_stale(&shared, &state), "remapped since");
    state.debugger_native_window.store(0, Ordering::Release);
    pause(true);
    note_native_frames_current(&shared, &thread);
    assert!(
        native_frames_stale(&shared, &state),
        "a shut window is not refreshed"
    );
}

/// The i53 retirement: the native frames window opens for a virtual
/// thread, and for one with a JNI native's row or a reflective call below
/// the native, whose rows it records for the reader (`NW_ROWS`); a later
/// window with no rows carries no `NW_ROWS`. Before, each of the three
/// refused it.
#[cfg(feature = "experimental-debug")]
#[test]
fn i53_the_native_frames_window_records_the_rows_below_the_native() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.debug.debugger_gates.native_frames_window() {
        return; // `CRATONVM_DEBUG_NATIVE_FRAMES_WINDOW=0` in the environment.
    }
    let mut thread = JvmThread::new(ThreadId(0x0F53), "i53-native-window");
    thread.kind = crate::threading::ThreadKind::Virtual;
    thread.frames.push(Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0, 1, 0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    let row = (1usize, ClassId::new(53), 0x1000u64, 0u32);
    thread.jni_native_frames.push(row);
    let call = crate::runtime::stackwalker::ReflectiveCallRow {
        interp_depth: 1,
        jit_depth: 0,
        kind: cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
        check: None,
        raised: None,
        door_save: None,
        adapter_pin: None,
    };
    thread.reflective_calls.push(call);
    let state = std::sync::Arc::clone(&thread.gc_block_state);
    assert!(open_native_frames_window(&shared, &mut thread, 0x2000));
    let w = state.debugger_native_window.load(Ordering::Acquire);
    assert_eq!(w & (NW_OPEN | NW_ROWS), NW_OPEN | NW_ROWS);
    {
        let rows = state.native_rows.lock();
        assert_eq!(rows.jni_rows, vec![row]);
        assert_eq!(rows.reflective_calls, vec![call]);
    }
    assert_eq!(state.debugger_native_depth.load(Ordering::Acquire), 1);
    close_native_frames_window(&shared, &mut thread);
    assert_eq!(state.debugger_native_window.load(Ordering::Acquire), 0);
    thread.jni_native_frames.clear();
    thread.reflective_calls.clear();
    assert!(open_native_frames_window(&shared, &mut thread, 0x2000));
    assert_eq!(
        state.debugger_native_window.load(Ordering::Acquire) & (NW_OPEN | NW_ROWS),
        NW_OPEN
    );
    close_native_frames_window(&shared, &mut thread);
}

/// Wave 26 (the native method events of
/// `interpreter-L1-proposal-native-method-events-at-the-native-funnel-FIXED`): the real
/// native-call funnel, and its leaf twin, report a JDWP `MethodEntry` and
/// a `MethodExitWithReturnValue` at location -1 for the `ACC_NATIVE`
/// method the caller's invoke names when the callback it runs is that
/// method's — nothing without a request, for a callback the invoke does
/// not name (a nested call), or for a registered stand-in of a method
/// that is not `ACC_NATIVE`.
#[test]
fn the_native_funnel_reports_a_native_methods_entry_and_exit() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    fn bits(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(1_069_547_520)))
    }
    fn stand_in(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(5)))
    }
    const NATIVES: &str = "cratonvm/test/W26Natives";
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    let registry = &mut vm.natives.native_methods;
    registry.register(NATIVES, "bits", "(F)I", bits, NativeKind::Bridge);
    registry.register(NATIVES, "java", "()I", stand_in, NativeKind::Bridge);
    let shared = Arc::new(vm);
    let natives = class_with(
        &shared,
        NATIVES,
        vec![ConstantPoolEntry::Tombstone],
        vec![
            ("bits", "(F)I", M::STATIC | M::NATIVE),
            ("java", "()I", M::STATIC),
        ],
    );
    let caller = class_with(
        &shared,
        "cratonvm/test/W26NativeCaller",
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(NATIVES.into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("bits".into()),
            ConstantPoolEntry::Utf8("(F)I".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
            ConstantPoolEntry::Utf8("java".into()),
            ConstantPoolEntry::Utf8("()I".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 7,
                descriptor_index: 8,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 9,
            },
        ],
        vec![("run", "()V", M::STATIC)],
    );
    // invokestatic #6 (bits); pop; invokestatic #10 (java); pop; return.
    let mut top = Frame::new(
        caller,
        "cratonvm/test/W26NativeCaller".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0, 6, 0x57, 0xb8, 0, 10, 0x57, 0xb1],
        Vec::new(),
        4,
        1,
        &[],
    );
    top.last_instr_pc = 0;
    let tid = 0x0F26;
    let mut thread = JvmThread::new(ThreadId(tid), "w26-native-events");
    thread.frames.push(top);
    shared.debug.debugger_gates.set_session_attached(true);
    let rx = channel(&shared);
    let args = [Value::Float(1.5)];
    let call = |thread: &mut JvmThread| {
        crate::vm::safe_native_call(&shared, thread, bits, &args)
            .ok()
            .flatten()
    };
    assert_eq!(call(&mut thread), Some(Value::Int(1_069_547_520)));
    assert!(rx.try_recv().is_err(), "no request, no event");
    assert!(!shared.debug.debugger_gates.method_events_armed());

    let entry = arm(
        &shared,
        EventKind::MethodEntry,
        SuspendPolicy::None,
        Vec::new(),
    );
    let exit = arm(
        &shared,
        EventKind::MethodExitWithReturnValue,
        SuspendPolicy::None,
        Vec::new(),
    );
    assert!(shared.debug.debugger_gates.method_events_armed());
    let natives_wire = u64::from(natives.as_u32());
    let bits_id = crate::debug::jdwp_method_id("bits", "(F)I");
    let reported = |rx: &Receiver<crate::debug::DebugEvent>| -> Vec<(EventKind, u32, u64, u64, u64, Vec<u8>)> {
        rx.try_iter()
            .map(|e| (e.kind, e.request_id, e.class_id, e.method_id, e.offset, e.extra))
            .collect()
    };
    let expected = vec![
        (
            EventKind::MethodEntry,
            entry,
            natives_wire,
            bits_id,
            crate::debug::NATIVE_FRAME_LOCATION,
            Vec::new(),
        ),
        (
            EventKind::MethodExitWithReturnValue,
            exit,
            natives_wire,
            bits_id,
            crate::debug::NATIVE_FRAME_LOCATION,
            // `I` and 1069547520 (0x3fc00000).
            vec![b'I', 0x3f, 0xc0, 0, 0],
        ),
    ];
    assert_eq!(call(&mut thread), Some(Value::Int(1_069_547_520)));
    assert_eq!(reported(&rx), expected, "the full funnel");
    let leaf = crate::vm::safe_native_call_leaf(&shared, &mut thread, bits, &args);
    assert_eq!(leaf.ok().flatten(), Some(Value::Int(1_069_547_520)));
    assert_eq!(reported(&rx), expected, "a leaf takes the full funnel");

    // A callback the invoke at pc 0 does not name: a nested call.
    let nested = crate::vm::safe_native_call(&shared, &mut thread, stand_in, &[]);
    assert_eq!(nested.ok().flatten(), Some(Value::Int(5)));
    assert!(reported(&rx).is_empty(), "a nested call reports nothing");
    // The registered stand-in of `java`, which is not `ACC_NATIVE`.
    if let Some(top) = thread.frames.last_mut() {
        top.last_instr_pc = 4;
    }
    let java = crate::vm::safe_native_call(&shared, &mut thread, stand_in, &[]);
    assert_eq!(java.ok().flatten(), Some(Value::Int(5)));
    assert!(reported(&rx).is_empty(), "a stand-in reports nothing");

    disarm(&shared);
    assert!(!shared.debug.debugger_gates.method_events_armed());
    *shared.debug.debug_event_tx.lock().expect("event channel") = None;
    shared.debug.debugger_gates.set_session_attached(false);
}

/// Wave 28 (stage 1 of
/// `i27-L1-methods-served-without-a-frame-report-no-method-events`): a
/// registered native standing in for a Java method with bytecode is not
/// called while a breakpoint sits in that method — the funnel runs the
/// bytecode, in a frame, where the breakpoint reports — and is called
/// otherwise (no request, or a breakpoint elsewhere). A method HotSpot
/// serves from a math entry keeps its native, and so (wave 28 follow-up)
/// does a method whose native is a reviewed `NativeKind::Intrinsic`
/// shadow outside the interpreter's intrinsic table — `Class.getName`,
/// whose bytecode reads this VM's internal-name overlay.
#[test]
fn a_stood_in_java_method_runs_its_bytecode_under_a_breakpoint() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    fn stand_in(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(5)))
    }
    const STANDIN: &str = "cratonvm/test/W28StandIn";
    // `--real-jdk`: a `Bridge` over bytecode may yield to it only there.
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    config.use_synthetic_jdk = false;
    let mut vm = SharedVm::new(config);
    vm.natives.native_methods.register(
        STANDIN,
        "java",
        "()I",
        stand_in,
        NativeKind::Bridge,
    );
    // A reviewed shadow over bytecode the table does not serve.
    vm.natives.native_methods.register(
        STANDIN,
        "shadowed",
        "()I",
        stand_in,
        NativeKind::Intrinsic,
    );
    vm.natives.native_methods.register(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        stand_in,
        NativeKind::Intrinsic,
    );
    let shared = Arc::new(vm);
    let owner = class_with(
        &shared,
        STANDIN,
        vec![ConstantPoolEntry::Tombstone],
        vec![
            ("java", "()I", M::STATIC),
            ("other", "()V", M::STATIC),
            ("shadowed", "()I", M::STATIC),
        ],
    );
    // java: iconst_3; ireturn. shadowed: getstatic #1; ireturn, bytecode
    // that reads VM state (the `Class.getName` shape; wave 62, lane L1:
    // a shadow whose bytecode reads only its arguments runs it).
    for (method, code) in [(0, vec![0x06, 0xac]), (2, vec![0xb2, 0x00, 0x01, 0xac])] {
        shared
            .classes
            .class_manager
            .write()
            .get_class_mut(owner)
            .expect("just added")
            .methods[method]
            .attributes = vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))];
    }
    let caller = class_with(
        &shared,
        "cratonvm/test/W28StandInCaller",
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(STANDIN.into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("java".into()),
            ConstantPoolEntry::Utf8("()I".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
            ConstantPoolEntry::Utf8("shadowed".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 7,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 8,
            },
        ],
        vec![("run", "()V", M::STATIC)],
    );
    // invokestatic #6 (java); pop; invokestatic #9 (shadowed); pop; return.
    let mut top = Frame::new(
        caller,
        "cratonvm/test/W28StandInCaller".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0, 6, 0x57, 0xb8, 0, 9, 0x57, 0xb1],
        Vec::new(),
        4,
        1,
        &[],
    );
    top.last_instr_pc = 0;
    let mut thread = JvmThread::new(ThreadId(0x0F28), "w28-stand-in");
    thread.frames.push(top);
    shared.debug.debugger_gates.set_session_attached(true);
    let rx = channel(&shared);
    let call = |thread: &mut JvmThread| {
        crate::vm::safe_native_call(&shared, thread, stand_in, &[])
            .ok()
            .flatten()
    };
    assert_eq!(
        call(&mut thread),
        Some(Value::Int(5)),
        "no request: the native"
    );

    let wire = u64::from(owner.as_u32());
    let breakpoint_in = |method: &str, descriptor: &str| {
        arm(
            &shared,
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![EventModifier::LocationOnly {
                class_id: wire,
                method_id: crate::debug::jdwp_method_id(method, descriptor),
                offset: 0,
            }],
        )
    };
    let elsewhere = breakpoint_in("other", "()V");
    assert!(shared.debug.debugger_gates.java_frames_armed());
    assert!(shared.debug.debugger_gates.method_events_armed());
    assert_eq!(
        call(&mut thread),
        Some(Value::Int(5)),
        "a breakpoint in another method: the native"
    );
    assert!(rx.try_recv().is_err());
    let _ = elsewhere;

    let inside = breakpoint_in("java", "()I");
    assert_eq!(
        call(&mut thread),
        Some(Value::Int(3)),
        "a breakpoint in the method: its bytecode"
    );
    assert_eq!(thread.frames.len(), 1, "the method's frame is gone again");
    let hits: Vec<(EventKind, u32, u64, u64, u64)> = rx
        .try_iter()
        .map(|e| (e.kind, e.request_id, e.class_id, e.method_id, e.offset))
        .collect();
    assert_eq!(
        hits,
        vec![(
            EventKind::Breakpoint,
            inside,
            wire,
            crate::debug::jdwp_method_id("java", "()I"),
            0
        )],
        "the breakpoint reported from the method's frame"
    );

    // A breakpoint in a method whose native is a reviewed `Intrinsic`
    // shadow outside the intrinsic table: the native still answers.
    let _ = breakpoint_in("shadowed", "()I");
    if let Some(top) = thread.frames.last_mut() {
        top.last_instr_pc = 4;
    }
    assert_eq!(
        call(&mut thread),
        Some(Value::Int(5)),
        "a reviewed Intrinsic shadow: its native"
    );
    assert!(rx.try_recv().is_err(), "no frame, no breakpoint");
    {
        use super::jvmti_events::bytecode_reads_only_its_arguments as only_args;
        use super::jvmti_events::stood_in_bytecode_may_run as may_run;
        const CLASS: &str = "java/lang/Class";
        let get_name = "()Ljava/lang/String;";
        assert!(!may_run(&shared, CLASS, CLASS, "getName", get_name, false));
        assert!(!may_run(
            &shared, STANDIN, STANDIN, "shadowed", "()I", false
        ));
        assert!(
            may_run(&shared, STANDIN, STANDIN, "java", "()I", false),
            "a jdk-only Bridge"
        );
        // The interpreter's intrinsic table, whatever the registration.
        const STRING: &str = "java/lang/String";
        assert!(may_run(&shared, STRING, STRING, "length", "()I", false));
        assert!(may_run(
            &shared,
            "java/lang/Math",
            "java/lang/Math",
            "max",
            "(II)I",
            false
        ));
        // Wave 62 (lane L1): a reviewed shadow whose bytecode reads only
        // its arguments runs it (`Integer.numberOfLeadingZeros`).
        assert!(may_run(&shared, STANDIN, STANDIN, "shadowed", "()I", true));
        let cm = shared.classes.class_manager.read();
        assert!(only_args(&shared, &cm, owner, 0, 3), "iconst_3; ireturn");
        assert!(!only_args(&shared, &cm, owner, 2, 3), "a getstatic");
        assert!(!only_args(&shared, &cm, owner, 1, 3), "no Code attribute");
    }
    if let Some(top) = thread.frames.last_mut() {
        top.last_instr_pc = 0;
    }

    disarm(&shared);
    assert!(!shared.debug.debugger_gates.java_frames_armed());
    assert_eq!(
        call(&mut thread),
        Some(Value::Int(5)),
        "disarmed: the native"
    );
    assert!(super::jvmti_events::hotspot_serves_from_a_math_entry(
        "java/lang/Math",
        "sqrt",
        "(D)D"
    ));
    assert!(!super::jvmti_events::hotspot_serves_from_a_math_entry(
        "java/lang/Math",
        "abs",
        "(I)I"
    ));
    // Wave 130 (lane L1a, i40): the float16 conversions, and not their
    // neighbours.
    assert!(super::jvmti_events::hotspot_serves_from_a_float16_entry(
        "java/lang/Float",
        "float16ToFloat",
        "(S)F"
    ));
    assert!(super::jvmti_events::hotspot_serves_from_a_float16_entry(
        "java/lang/Float",
        "floatToFloat16",
        "(F)S"
    ));
    assert!(!super::jvmti_events::hotspot_serves_from_a_float16_entry(
        "java/lang/Float",
        "sum",
        "(FF)F"
    ));
    // Wave 134 (lane D, i40): several returns decline the frame-only
    // report; one return, or a return byte inside an operand, does not.
    assert!(super::shadow_has_several_returns(&[0x03, 0xac, 0x04, 0xac]));
    assert!(!super::shadow_has_several_returns(&[0x03, 0xac]));
    assert!(!super::shadow_has_several_returns(&[0x11, 0x00, 0xac, 0xac]));
    assert!(!super::shadow_has_several_returns(&[0x01, 0xbf]));
    *shared.debug.debug_event_tx.lock().expect("event channel") = None;
    shared.debug.debugger_gates.set_session_attached(false);
}

/// Wave 28 (`interpreter-L1-proposal-method-events-of-jni-bound-natives-FIXED`): a
/// JNI-bound native (here bound as `RegisterNatives` binds one), which
/// the general invocation path calls through `dispatch_jni_native` and
/// never through the native-call funnel, reports its JDWP `MethodEntry`
/// and `MethodExitWithReturnValue` at location -1 while requests for them
/// are in force, and nothing otherwise.
#[test]
fn a_jni_bound_native_reports_its_method_entry_and_exit() {
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    extern "C" fn answer(
        _env: crate::native::jni::JNIEnv,
        _cls: crate::native::jni::JObject,
    ) -> u64 {
        42
    }
    const JNI: &str = "cratonvm/test/W28JniBound";
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // The JNI arm reaches `SharedVm::get_arc`, which `Vm::new` would install.
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let owner = class_with(
        &shared,
        JNI,
        vec![ConstantPoolEntry::Tombstone],
        vec![("answer", "()I", M::STATIC | M::NATIVE)],
    );
    crate::native::jni::register_jni_native(
        &shared.natives,
        JNI,
        "answer",
        "()I",
        answer as *const () as usize, // Cast: fn address
    );
    let mut thread = JvmThread::new(ThreadId(0x0F29), "w28-jni-bound");
    shared.debug.debugger_gates.set_session_attached(true);
    let rx = channel(&shared);
    let call = |thread: &mut JvmThread| {
        crate::vm::invoke_on_class_shared(&shared, thread, owner, "answer", "()I", &[])
            .ok()
            .flatten()
    };
    assert_eq!(call(&mut thread), Some(Value::Int(42)));
    assert!(rx.try_recv().is_err(), "no request, no event");

    let entry = arm(
        &shared,
        EventKind::MethodEntry,
        SuspendPolicy::None,
        Vec::new(),
    );
    let exit = arm(
        &shared,
        EventKind::MethodExitWithReturnValue,
        SuspendPolicy::None,
        Vec::new(),
    );
    assert_eq!(call(&mut thread), Some(Value::Int(42)));
    let wire = u64::from(owner.as_u32());
    let id = crate::debug::jdwp_method_id("answer", "()I");
    let reported: Vec<(EventKind, u32, u64, u64, u64, Vec<u8>)> = rx
        .try_iter()
        .map(|e| {
            (
                e.kind,
                e.request_id,
                e.class_id,
                e.method_id,
                e.offset,
                e.extra,
            )
        })
        .collect();
    assert_eq!(
        reported,
        vec![
            (
                EventKind::MethodEntry,
                entry,
                wire,
                id,
                crate::debug::NATIVE_FRAME_LOCATION,
                Vec::new(),
            ),
            (
                EventKind::MethodExitWithReturnValue,
                exit,
                wire,
                id,
                crate::debug::NATIVE_FRAME_LOCATION,
                vec![b'I', 0, 0, 0, 42],
            ),
        ]
    );

    disarm(&shared);
    assert_eq!(call(&mut thread), Some(Value::Int(42)));
    assert!(rx.try_recv().is_err(), "disarmed: no event");
    *shared.debug.debug_event_tx.lock().expect("event channel") = None;
    shared.debug.debugger_gates.set_session_attached(false);
}

/// Wave 27 (the intrinsic item of
/// `i24-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code`):
/// a native the interpreter's intrinsic table serves reaches the funnel
/// with the intrinsic's trampoline, not its registered callback, and is
/// named from either; `Thread.currentThread`, whose HotSpot interpreter
/// entry posts no event, is the one native kept silent.
#[test]
fn an_intrinsic_trampoline_runs_the_native_it_serves() {
    use super::jvmti_events::{callback_runs_method, hotspot_posts_no_method_events};
    use cratonvm_native_api::InterpIntrinsic;
    use cratonvm_native_builtins::intrinsics::callback_for;
    fn other(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    const ARRAYCOPY: &str = "(Ljava/lang/Object;ILjava/lang/Object;II)V";
    let arraycopy = callback_for(InterpIntrinsic::SystemArraycopy) as usize; // Cast: fn address
    let hash = callback_for(InterpIntrinsic::ObjectHashCode) as usize; // Cast: fn address
    let runs = |class: &str, name: &str, desc: &str, running: usize| {
        callback_runs_method(&shared, class, name, desc, running)
    };
    assert!(runs("java/lang/System", "arraycopy", ARRAYCOPY, arraycopy));
    assert!(runs("java/lang/Object", "hashCode", "()I", hash));
    assert!(
        !runs("java/lang/Object", "hashCode", "()I", arraycopy),
        "another intrinsic's trampoline"
    );
    assert!(!runs("java/lang/Object", "hashCode", "()I", other as usize)); // Cast: fn address
    assert!(
        !runs("java/lang/Object", "hashCode", "()I", 0),
        "no running native"
    );
    assert!(hotspot_posts_no_method_events(
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;"
    ));
    assert!(!hotspot_posts_no_method_events(
        "java/lang/System",
        "arraycopy",
        ARRAYCOPY
    ));
}

/// Wave 27 (stage 4 of
/// `interpreter-L1-proposal-native-method-events-at-the-native-funnel-FIXED`): while
/// the gate's JVMTI bit is up and an env of the VM listens, the real
/// native-call funnel posts a JVMTI `MethodEntry` and a `MethodExit` (not
/// popped by an exception, with the `jint` result) carrying the native
/// method's real `jmethodID`; with the bit down it posts nothing, and the
/// JDWP half of the gate stays down throughout.
#[test]
fn the_native_funnel_posts_jvmti_method_events_of_a_native() {
    use crate::runtime::jvmti::{
        self as rt, EventCallbacks, EventMode, JvmtiEventKind, JvmtiEventManager, LocalValue,
    };
    use cratonvm_native_api::NativeKind;
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    use std::sync::Mutex;
    fn bits(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(1_069_547_520)))
    }
    let _lock = rt::jvmti_registry_test_lock();
    const NATIVES: &str = "cratonvm/test/W27Natives";
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    vm.natives.native_methods.register(
        NATIVES,
        "bits",
        "(F)I",
        bits,
        NativeKind::Bridge,
    );
    let shared = Arc::new(vm);
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let vm_id = shared.vm_identity;
    rt::install_manager_for_vm(vm_id, Arc::new(JvmtiEventManager::new_for_vm(vm_id)));
    // `resolve_method_id_for_vm` reaches the VM through its bridge.
    rt::install_real_agent_env_bridge(&shared);
    let natives = class_with(
        &shared,
        NATIVES,
        vec![ConstantPoolEntry::Tombstone],
        vec![("bits", "(F)I", M::STATIC | M::NATIVE)],
    );
    let caller = class_with(
        &shared,
        "cratonvm/test/W27NativeCaller",
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(NATIVES.into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("bits".into()),
            ConstantPoolEntry::Utf8("(F)I".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
        ],
        vec![("run", "()V", M::STATIC)],
    );
    // invokestatic #6 (bits); pop; return.
    let mut top = Frame::new(
        caller,
        "cratonvm/test/W27NativeCaller".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0, 6, 0x57, 0xb1],
        Vec::new(),
        4,
        1,
        &[],
    );
    top.last_instr_pc = 0;
    let tid = 0x0F27;
    let mut thread = JvmThread::new(ThreadId(tid), "w27-jvmti-native-events");
    thread.frames.push(top);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (on_entry, on_exit) = (Arc::clone(&seen), Arc::clone(&seen));
    let manager = rt::manager_for_vm(vm_id).expect("installed");
    for kind in [JvmtiEventKind::MethodEntry, JvmtiEventKind::MethodExit] {
        manager
            .set_event_notification_mode(EventMode::Enable, kind, None)
            .expect("enable");
    }
    manager
        .set_event_callbacks(EventCallbacks {
            method_entry: Some(Box::new(move |t, m| {
                on_entry
                    .lock()
                    .expect("sink")
                    .push(format!("entry {t} {m:#x}"));
            })),
            method_exit: Some(Box::new(move |t, m, popped, v| {
                on_exit
                    .lock()
                    .expect("sink")
                    .push(format!("exit {t} {m:#x} {popped} {v:?}"));
            })),
            ..Default::default()
        })
        .expect("callbacks");
    let args = [Value::Float(1.5)];
    let call = |thread: &mut JvmThread| {
        crate::vm::safe_native_call(&shared, thread, bits, &args)
            .ok()
            .flatten()
    };

    assert!(!shared.debug.debugger_gates.method_events_armed());
    assert_eq!(call(&mut thread), Some(Value::Int(1_069_547_520)));
    assert!(seen.lock().expect("sink").is_empty(), "the gate is down");

    shared.debug.debugger_gates.set_jvmti_method_events(true);
    assert!(shared.debug.debugger_gates.method_events_armed());
    assert!(!shared.debug.debugger_gates.jdwp_method_events_armed());
    assert_eq!(call(&mut thread), Some(Value::Int(1_069_547_520)));
    // `(class id << 32) | method index`; `bits` is method 0.
    let id = u64::from(natives.as_u32()) << 32;
    assert_eq!(
        *seen.lock().expect("sink"),
        vec![
            format!("entry {tid} {id:#x}"),
            format!(
                "exit {tid} {id:#x} false {:?}",
                LocalValue::Int(1_069_547_520)
            ),
        ]
    );
    assert!(thread.native_pin_roots.is_empty(), "no pin left behind");

    shared.debug.debugger_gates.set_jvmti_method_events(false);
    assert!(!shared.debug.debugger_gates.method_events_armed());
    rt::forget_vm_jvmti_state(vm_id);
}

/// Wave 21, lane L3
/// (`i18-L3-proposal-jdwp-lists-the-compiled-activations-of-a-blocked-thread`,
/// stages 1-3): the compiled activations a blocked thread recorded are
/// listed at their places between its interpreter frames; a frame whose
/// body runs compiled is listed at the compiled half's bytecode index;
/// `GetValues` / `ThisObject` on either answer `OPAQUE_FRAME` instead of
/// stale values, the other frames keep their locals, and no native frame
/// is listed above a compiled one. Without compiled code the capture is
/// empty, and the withdrawal forgets the opaque ids.
/// Interpreter round i1 wave 47, lane L1: the one listing puts a stand-in's
/// Java levels under the native on top, a JNI native's row right below the
/// first interpreter frame its call pushed, and a reflective call's JDK
/// frames right below the first compiled activation entered after it (an
/// inlined callee is never such a place), as a stack trace places them.
#[test]
fn one_listing_places_standin_levels_and_splices() {
    use crate::threading::jvm_thread::{BlockedCompiledRow, BlockedCompiledView};
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let class = class_with(
        &shared,
        "cratonvm/test/W47Listing",
        vec![ConstantPoolEntry::Tombstone],
        vec![("run", "()V", M::STATIC)],
    );
    let frame = |pc: usize| {
        let mut f = Frame::new(
            class,
            "cratonvm/test/W47Listing".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![0x00; 16],
            Vec::new(),
            4,
            1,
            &[],
        );
        f.last_instr_pc = pc;
        f
    };
    let frames = vec![frame(3), frame(5)];
    let wire = u64::from(class.as_u32());
    let run = crate::debug::jdwp_method_id("run", "()V");
    let compiled_row = |below: u32, method_id: u64, chain: Option<u32>| BlockedCompiledRow {
        below,
        class_id: class.as_u32(),
        method_id,
        bci: 4,
        chain,
    };
    let view = BlockedCompiledView {
        rows: vec![
            compiled_row(1, 0xA, Some(0)),
            compiled_row(2, 0xB, Some(1)),
            compiled_row(2, 0xC, None),
        ],
        stale: Vec::new(),
        homes: Vec::new(),
    };
    let splices = [
        ListingSplice {
            interp_depth: 1,
            jit_depth: 1,
            kind: 1,
            rows: vec![(7, 0x11, -1)],
            values: None,
        },
        ListingSplice {
            interp_depth: 2,
            jit_depth: 1,
            kind: 0,
            rows: vec![(8, 0x21, 102), (9, 0x22, 23)],
            values: None,
        },
    ];
    let head = NativeHead::from_standin_chain(&[
        crate::native::registry::StackTraceEntry {
            class_name: Arc::from("java/lang/Thread"),
            method_name: Arc::from("sleep"),
            method_descriptor: Some(Arc::from("(J)V")),
            source_file: None,
            line_number: 540,
            byte_code_index: 17,
            class_id: Some(ClassId::new(5)),
            method_index: Some(3),
        },
        crate::native::registry::StackTraceEntry {
            class_name: Arc::from("java/lang/Thread"),
            method_name: Arc::from("sleepNanos0"),
            method_descriptor: Some(Arc::from("(J)V")),
            source_file: None,
            line_number: -2,
            byte_code_index: -1,
            class_id: Some(ClassId::new(5)),
            method_index: Some(4),
        },
    ])
    .expect("a chain with its native leaf");
    assert_eq!(
        head.native(),
        (5, crate::debug::jdwp_method_id("sleepNanos0", "(J)V"))
    );
    assert_eq!(head.levels().len(), 1);
    let listing = build_frame_listing(&frames, 6, Some(&head), &view, &splices);
    let opaque = |class_id: u64, method_id: u64, location: i64| ListingRow::Opaque {
        class_id,
        method_id,
        location,
        chain: None,
    };
    // Wave 120 (lane L1): a compiled activation's row carries its chain
    // index; an inlined callee's (0xC) none.
    let compiled = |method_id: u64, chain: Option<u32>| ListingRow::Opaque {
        class_id: wire,
        method_id,
        location: 4,
        chain,
    };
    let interp = |index: usize, location: i64| ListingRow::Interp {
        index,
        class_id: wire,
        method_id: run,
        location,
        runs_compiled: false,
    };
    assert_eq!(
        listing,
        vec![
            ListingRow::Head {
                class_id: 5,
                method_id: crate::debug::jdwp_method_id("sleepNanos0", "(J)V"),
            },
            opaque(5, crate::debug::jdwp_method_id("sleep", "(J)V"), 17),
            opaque(wire, 0xC, 4),
            compiled(0xB, Some(1)),
            opaque(9, 0x22, 23),
            opaque(8, 0x21, 102),
            interp(1, 6),
            opaque(7, 0x11, -1),
            compiled(0xA, Some(0)),
            interp(0, 3),
        ]
    );
    // Without splices the listing is the wave-46 one.
    let plain = build_frame_listing(&frames, 6, None, &view, &[]);
    assert_eq!(
        plain,
        vec![
            opaque(wire, 0xC, 4),
            compiled(0xB, Some(1)),
            interp(1, 6),
            compiled(0xA, Some(0)),
            interp(0, 3),
        ]
    );
    // Wave 49: a continuation's head lists its levels, top first, over
    // the kept frames, and no native row.
    let level = |method_id: u64, bci: u64| StandinLevel {
        class_id: 6,
        method_id,
        method_index: 0,
        bci,
        locals: StandinLevelLocals::Unknown,
    };
    let continuation =
        NativeHead::continuation(&[level(0x31, 8), level(0x32, 62)]).expect("two levels fit");
    assert!(!continuation.has_native());
    assert!(NativeHead::continuation(&[]).is_none());
    let empty = BlockedCompiledView::default();
    assert_eq!(
        build_frame_listing(&frames, 6, Some(&continuation), &empty, &[]),
        vec![
            opaque(6, 0x31, 8),
            opaque(6, 0x32, 62),
            interp(1, 6),
            interp(0, 3)
        ]
    );
}

/// j27 wave 4 (lane TRACEROWS, TR16-3): the compiled indy bridge's
/// synthetic `<jit-indy>.bridge` frame (class id 0, no method) is in no
/// debugger listing, as it is in no stack trace, `StackWalker` walk or
/// `Thread.getStackTrace()` (`stackwalker::frame_is_vm_bridge`); the C
/// JVMTI table already dropped its row (`native_env::other_rows`: no
/// method resolves), JDWP listed it with class id 0.
#[test]
fn the_indy_bridge_frame_is_in_no_listing() {
    use crate::threading::jvm_thread::BlockedCompiledView;
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let class = class_with(
        &shared,
        "cratonvm/test/J27Bridge",
        vec![ConstantPoolEntry::Tombstone],
        vec![("run", "()V", M::STATIC)],
    );
    let frame = |class_id: ClassId, name: &str, method: &str, pc: usize| {
        let mut f = Frame::new(
            class_id,
            name.to_string(),
            method.to_string(),
            "()V".to_string(),
            None,
            vec![0x00; 16],
            Vec::new(),
            4,
            1,
            &[],
        );
        f.last_instr_pc = pc;
        f
    };
    let mut bridge = frame(class, "<jit-indy>", "bridge", 0);
    bridge.mark_vm_synthetic();
    let frames = vec![
        frame(class, "cratonvm/test/J27Bridge", "run", 3),
        bridge,
        frame(class, "cratonvm/test/J27Bridge", "run", 5),
    ];
    let wire = u64::from(class.as_u32());
    let run = crate::debug::jdwp_method_id("run", "()V");
    let interp = |index: usize, location: i64| ListingRow::Interp {
        index,
        class_id: wire,
        method_id: run,
        location,
        runs_compiled: false,
    };
    let listing = build_frame_listing(&frames, 6, None, &BlockedCompiledView::default(), &[]);
    assert_eq!(listing, vec![interp(2, 6), interp(0, 3)]);
}

#[test]
fn a_blocked_threads_compiled_activations_are_listed_without_locals() {
    use crate::debug::commands::{
        dispatch, CMD_SF_GET_VALUES, CMD_SF_THIS_OBJECT, CS_STACK_FRAME, ERR_NONE,
        ERR_OPAQUE_FRAME,
    };
    use crate::debug::protocol::PayloadWriter;
    use crate::threading::jvm_thread::{BlockedCompiledRow, BlockedCompiledView};
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let class = class_with(
        &shared,
        "cratonvm/test/W21Blocked",
        vec![ConstantPoolEntry::Tombstone],
        vec![("run", "()V", M::STATIC)],
    );
    let frame = |pc: usize, local: i32| {
        let mut f = Frame::new(
            class,
            "cratonvm/test/W21Blocked".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![0x00; 16],
            Vec::new(),
            4,
            1,
            &[],
        );
        f.last_instr_pc = pc;
        f.set_local(0, Value::Int(local));
        f
    };
    let frames = vec![frame(3, 10), frame(5, 20)];
    assert_eq!(
        capture_blocked_compiled_view(&frames),
        BlockedCompiledView::default(),
        "no compiled code on this thread"
    );
    let wire = u64::from(class.as_u32());
    let hot = crate::debug::jdwp_method_id("hot", "()V");
    let leaf = crate::debug::jdwp_method_id("leaf", "(I)V");
    let view = BlockedCompiledView {
        rows: vec![
            BlockedCompiledRow {
                below: 1,
                class_id: class.as_u32(),
                method_id: hot,
                bci: 9,
                chain: Some(0),
            },
            BlockedCompiledRow {
                below: 2,
                class_id: class.as_u32(),
                method_id: leaf,
                bci: -1,
                chain: Some(1),
            },
        ],
        stale: vec![(0, 7)],
        homes: Vec::new(),
    };
    let tid = 0x0F21;
    // A compiled activation stands above the top interpreter frame, so the
    // native is named from ITS invoke (wave 22): `leaf` is not a method of
    // the class, so none is, whatever the top frame's invoke names.
    let native_top = blocked_native_top(&shared, &frames, &view, 0);
    assert_eq!(native_top, None);
    let _ = wire;
    let mut ds = shared.debug.debug_state.lock();
    ds.thread_names.insert(tid, "w21-blocked".into());
    ds.suspend_thread(tid);
    publish_frame_snapshot(
        &shared,
        &frames,
        &mut ds,
        tid,
        5,
        native_top,
        Some(&view),
        &[],
        |_, _, v| v,
    );
    let listed: Vec<(u64, u64, u64)> = ds
        .thread_frames
        .get(&tid)
        .map(|frames| {
            frames
                .iter()
                .map(|f| (f.frame_id, f.method_id, f.offset))
                .collect()
        })
        .unwrap_or_default();
    let run = crate::debug::jdwp_method_id("run", "()V");
    assert_eq!(
        listed,
        vec![(0, leaf, 0), (1, run, 5), (2, hot, 9), (3, run, 7),],
        "no native frame the compiled one did not call; a compiled frame of \
         unknown bci at code index 0 (wave 22), never at the native -1; the \
         OSR'd frame at its compiled bci"
    );
    assert_eq!(ds.opaque_frames.get(&(tid, 0)), Some(&true));
    assert_eq!(ds.opaque_frames.get(&(tid, 1)), None);
    assert_eq!(ds.opaque_frames.get(&(tid, 2)), Some(&true));
    assert_eq!(ds.opaque_frames.get(&(tid, 3)), Some(&false));
    let get = |ds: &mut crate::debug::DebugState, frame: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(tid);
        pw.put_u64_be(frame);
        pw.put_u32_be(1);
        pw.put_u32_be(0);
        pw.put_u8(b'I');
        dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), ds)
    };
    assert_eq!(get(&mut ds, 0).error_code, ERR_OPAQUE_FRAME);
    let live = get(&mut ds, 1);
    assert_eq!(live.error_code, ERR_NONE);
    assert_eq!(live.data, vec![0, 0, 0, 1, b'I', 0, 0, 0, 20]);
    assert_eq!(get(&mut ds, 2).error_code, ERR_OPAQUE_FRAME);
    assert_eq!(
        get(&mut ds, 3).error_code,
        ERR_OPAQUE_FRAME,
        "the OSR'd frame's locals are the values at entry"
    );
    let mut this = PayloadWriter::new();
    this.put_u64_be(tid);
    this.put_u64_be(2);
    assert_eq!(
        dispatch(
            CS_STACK_FRAME,
            CMD_SF_THIS_OBJECT,
            &this.into_bytes(),
            &mut ds
        )
        .error_code,
        ERR_OPAQUE_FRAME
    );
    let stale = withdraw_debugger_frames(&mut ds, tid);
    for id in stale {
        ds.objects.unpin(id);
    }
    assert!(ds.opaque_frames.is_empty());
    ds.resume_all();
}

/// Wave 22, lane L1 (`i9-L1-jdwp-suspension-does-not-reach-compiled-or-native-code`,
/// item 3; `i15-L1-proposal-record-the-running-native-on-the-thread`,
/// stage 3): a thread blocked in a native it called from COMPILED code
/// lists that native first, named from the innermost compiled
/// activation's invoke, and only while its running-native record is that
/// native's registered callback (the activation's bci is the last
/// safepoint it published, so the callback is what says the invoke there
/// is the running one). No record, another callback, a bci that is not an
/// invoke or is unknown, or a compiled row below the top interpreter
/// frame: no native frame from the row.
#[test]
fn a_native_called_from_compiled_code_is_listed_above_it() {
    use crate::threading::jvm_thread::{BlockedCompiledRow, BlockedCompiledView};
    use cratonvm_native_api::NativeKind;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
    fn sleep0(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }
    fn other(
        _: &mut dyn cratonvm_native_api::NativeContext,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(Some(Value::Int(22)))
    }
    const NATIVES: &str = "cratonvm/test/W22Natives";
    const HOT: &str = "cratonvm/test/W22Hot";
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    vm.natives.native_methods.register(
        NATIVES,
        "sleep0",
        "(J)V",
        sleep0,
        NativeKind::Bridge,
    );
    let shared = Arc::new(vm);
    let natives = class_with(
        &shared,
        NATIVES,
        vec![ConstantPoolEntry::Tombstone],
        vec![("sleep0", "(J)V", M::STATIC | M::NATIVE)],
    );
    let hot = class_with(
        &shared,
        HOT,
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(NATIVES.into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("sleep0".into()),
            ConstantPoolEntry::Utf8("(J)V".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
        ],
        vec![("hot", "()V", M::STATIC)],
    );
    // hot: lconst_0; invokestatic #6 (sleep0); return.
    shared
        .classes
        .class_manager
        .write()
        .get_class_mut(hot)
        .expect("just added")
        .methods[0]
        .attributes = vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
        CodeAttribute {
            max_stack: 2,
            max_locals: 0,
            code: cratonvm_reader::ByteView::from_vec(vec![0x09, 0xb8, 0, 6, 0xb1]),
            exception_table: vec![],
            attributes: vec![],
        },
    )))];
    // The interpreter frame the compiled `hot` was entered from.
    let mut main = Frame::new(
        hot,
        HOT.to_string(),
        "main".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0, 1, 0xb1],
        Vec::new(),
        2,
        1,
        &[],
    );
    main.last_instr_pc = 0;
    let frames = vec![main];
    let hot_id = crate::debug::jdwp_method_id("hot", "()V");
    let row = |below: u32, bci: i64| BlockedCompiledRow {
        below,
        class_id: hot.as_u32(),
        method_id: hot_id,
        bci,
        chain: Some(0),
    };
    let view = |below: u32, bci: i64| BlockedCompiledView {
        rows: vec![row(below, bci)],
        stale: Vec::new(),
        homes: Vec::new(),
    };
    let running = sleep0 as usize; // Cast: fn address
    let named = Some((
        u64::from(natives.as_u32()),
        crate::debug::jdwp_method_id("sleep0", "(J)V"),
    ));
    assert_eq!(
        blocked_native_top(&shared, &frames, &view(1, 1), running).map(|head| head.native()),
        named
    );
    assert_eq!(
        blocked_native_top(&shared, &frames, &view(1, 1), 0),
        None,
        "no running-native record"
    );
    assert_eq!(
        blocked_native_top(&shared, &frames, &view(1, 1), other as usize), // Cast: fn address
        None,
        "another native runs"
    );
    assert_eq!(
        blocked_native_top(&shared, &frames, &view(1, 0), running),
        None,
        "the bci is not an invoke"
    );
    assert_eq!(
        blocked_native_top(&shared, &frames, &view(1, -1), running),
        None,
        "the bci is unknown"
    );
    assert_eq!(
        blocked_native_top(&shared, &frames, &view(0, 1), running),
        None,
        "below the top frame: the top frame's own invoke decides"
    );

    let tid = 0x0F22;
    let mut ds = shared.debug.debug_state.lock();
    ds.thread_names
        .insert(tid, "w22-native-under-compiled".into());
    ds.suspend_thread(tid);
    publish_frame_snapshot(
        &shared,
        &frames,
        &mut ds,
        tid,
        0,
        named.map(|(class_id, method_id)| NativeHead::leaf(class_id, method_id)),
        Some(&view(1, 1)),
        &[],
        |_, _, v| v,
    );
    let listed: Vec<(u64, u64, u64)> = ds
        .thread_frames
        .get(&tid)
        .map(|frames| {
            frames
                .iter()
                .map(|f| (f.frame_id, f.method_id, f.offset))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        listed,
        vec![
            (
                0,
                crate::debug::jdwp_method_id("sleep0", "(J)V"),
                crate::debug::NATIVE_FRAME_LOCATION
            ),
            (1, hot_id, 1),
            (2, crate::debug::jdwp_method_id("main", "()V"), 0),
        ],
        "the native, then the compiled caller at its invoke, then main"
    );
    assert_eq!(ds.opaque_frames.get(&(tid, 1)), Some(&true));
    let stale = withdraw_debugger_frames(&mut ds, tid);
    for id in stale {
        ds.objects.unpin(id);
    }
    ds.resume_all();
}
