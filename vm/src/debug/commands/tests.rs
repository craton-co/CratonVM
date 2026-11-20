use super::*;
use crate::debug::DebugState;

fn fresh_state() -> DebugState {
    DebugState::new()
}

/// A `LocationOnly` modifier (class 4, method 9, index 0) for a request
/// that needs one.
fn put_location_only(body: &mut PayloadWriter) {
    body.put_u8(7); // LocationOnly
    body.put_u8(1); // TypeTag.CLASS
    body.put_u64_be(4);
    body.put_u64_be(9);
    body.put_u64_be(0);
}

/// Interpreter round i1 wave 29: `EventRequest.Set` refuses what HotSpot
/// 25.0.3's back end refuses (measured,
/// `tools/probes/interp/L1/L1W29RawJdwpEventRequestErrors.java`), and
/// `EventRequest.Clear` refuses an event kind JDWP does not define.
#[test]
fn event_requests_are_refused_as_hotspot_refuses_them() {
    fn set(st: &mut DebugState, kind: EventKind, modifiers: &[Vec<u8>]) -> u16 {
        let mut body = PayloadWriter::new();
        body.put_u8(kind as u8);
        body.put_u8(SuspendPolicy::None as u8);
        body.put_u32_be(modifiers.len() as u32); // Cast: a handful
        for m in modifiers {
            body.put_bytes(m);
        }
        dispatch(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), st).error_code
    }
    fn clear(st: &mut DebugState, kind: u8, id: u32) -> u16 {
        let mut body = PayloadWriter::new();
        body.put_u8(kind);
        body.put_u32_be(id);
        dispatch(CS_EVENT_REQUEST, CMD_ER_CLEAR, &body.into_bytes(), st).error_code
    }
    fn location(index: u64) -> Vec<u8> {
        let mut b = PayloadWriter::new();
        b.put_u8(7); // LocationOnly
        b.put_u8(1); // TypeTag.CLASS
        b.put_u64_be(4);
        b.put_u64_be(9);
        b.put_u64_be(index);
        b.into_bytes()
    }
    fn count(n: i32) -> Vec<u8> {
        let mut b = PayloadWriter::new();
        b.put_u8(1);
        b.put_u32_be(n as u32); // Cast: a JDWP int on the wire
        b.into_bytes()
    }
    let class_match = {
        let mut b = PayloadWriter::new();
        b.put_u8(5);
        b.put_string("x.*");
        b.into_bytes()
    };
    let field_only = {
        let mut b = PayloadWriter::new();
        b.put_u8(9);
        b.put_u64_be(4);
        b.put_u64_be(4 << 32);
        b.into_bytes()
    };
    let step = {
        let mut b = PayloadWriter::new();
        b.put_u8(10);
        b.put_u64_be(3);
        b.put_u32_be(1);
        b.put_u32_be(0);
        b.into_bytes()
    };
    let mut st = fresh_state();
    let st = &mut st;
    use crate::debug::events::EventKind as K;
    assert_eq!(set(st, K::Breakpoint, &[]), ERR_INTERNAL, "no location");
    assert_eq!(set(st, K::FieldAccess, &[]), ERR_INTERNAL, "no field");
    assert_eq!(set(st, K::SingleStep, &[]), ERR_NONE, "a step without Step");
    assert_eq!(
        set(st, K::Breakpoint, &[count(0), location(0)]),
        ERR_INTERNAL
    );
    assert_eq!(
        set(st, K::Breakpoint, &[count(-1), location(0)]),
        ERR_INTERNAL
    );
    assert_eq!(set(st, K::Breakpoint, &[count(1), location(0)]), ERR_NONE);
    assert_eq!(
        set(st, K::ThreadStart, &[location(0)]),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(
        set(st, K::ThreadStart, &[class_match.clone()]),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(
        set(st, K::ClassUnload, &[class_match]),
        ERR_NONE,
        "a class unload is filtered by pattern"
    );
    assert_eq!(
        set(st, K::MethodEntry, &[field_only.clone()]),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(set(st, K::MethodEntry, &[step]), ERR_ILLEGAL_ARGUMENT);
    assert_eq!(
        set(st, K::MethodEntry, &[location(0)]),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(set(st, K::Exception, &[location(0)]), ERR_NONE);
    assert_eq!(set(st, K::FieldAccess, &[field_only]), ERR_NONE);
    // A thread or class filter naming nothing is `INVALID_OBJECT` (the
    // probe's "no such thread" / "no such class" rows).
    let thread_only = |tid: u64| {
        let mut b = PayloadWriter::new();
        b.put_u8(3);
        b.put_u64_be(tid);
        b.into_bytes()
    };
    let class_only = |cid: u64| {
        let mut b = PayloadWriter::new();
        b.put_u8(4);
        b.put_u64_be(cid);
        b.into_bytes()
    };
    assert_eq!(
        set(st, K::MethodEntry, &[thread_only(0x7654_3210)]),
        ERR_INVALID_OBJECT
    );
    assert_eq!(
        set(st, K::MethodEntry, &[class_only(0x7654_3210)]),
        ERR_INVALID_OBJECT
    );
    st.thread_names.insert(0x7654_3210, "t".into());
    assert_eq!(
        set(st, K::MethodEntry, &[thread_only(0x7654_3210)]),
        ERR_NONE
    );
    // A code index past the method's code, when its length is known.
    st.class_details
        .entry(4)
        .or_default()
        .code_lengths
        .insert(9, 5);
    assert_eq!(set(st, K::Breakpoint, &[location(5)]), ERR_INVALID_LOCATION);
    assert_eq!(set(st, K::Breakpoint, &[location(4)]), ERR_NONE);
    // Clear: an undefined event kind; a request that does not exist.
    assert_eq!(clear(st, 77, 1), ERR_INVALID_EVENT_TYPE);
    assert_eq!(clear(st, K::Breakpoint as u8, 0x7654_3210), ERR_NONE);
    // Wave 37: a suspend policy JDWP does not define is accepted, as
    // HotSpot accepts it, and applied as `EVENT_THREAD`, what HotSpot
    // suspends for it (`L1W37RawJdwpUnknownSuspendPolicy`).
    let mut body = PayloadWriter::new();
    body.put_u8(K::MethodEntry as u8);
    body.put_u8(7);
    body.put_u32_be(0);
    let res = dispatch(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), st);
    assert_eq!(res.error_code, ERR_NONE, "policy 7");
    let id = u32::from_be_bytes([res.data[0], res.data[1], res.data[2], res.data[3]]);
    assert_eq!(
        st.events.get_request(id).map(|r| r.suspend_policy),
        Some(SuspendPolicy::EventThread)
    );
    // Wave 38: and its event sets carry the byte it sent.
    assert_eq!(st.events.wire_suspend_policy(id), Some(7));
}

#[test]
fn dispatch_vm_version() {
    let mut st = fresh_state();
    let res = dispatch(CS_VM, CMD_VM_VERSION, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);

    // The payload should contain "CratonVM" somewhere.
    let payload = String::from_utf8_lossy(&res.data);
    assert!(payload.contains("CratonVM"));
}

#[test]
fn dispatch_vm_id_sizes() {
    let mut st = fresh_state();
    let res = dispatch(CS_VM, CMD_VM_ID_SIZES, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    // 5 x u32 = 20 bytes, all set to 8
    assert_eq!(res.data.len(), 20);
    for chunk in res.data.chunks(4) {
        let val = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        assert_eq!(val, 8);
    }
}

#[test]
fn dispatch_vm_capabilities() {
    let mut st = fresh_state();
    let res = dispatch(CS_VM, CMD_VM_CAPABILITIES, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(res.data.len(), 7);
    // Wave 10: the two watch capabilities are true (the field bytecodes
    // deliver the events); wave 23: so is `canGetSyntheticAttribute`;
    // wave 24: and `canGetBytecodes`; wave 25: and the owned and
    // contended monitor ones; i5 wave 103: and `canGetMonitorInfo`.
    assert_eq!(res.data, vec![1, 1, 1, 1, 1, 1, 1]);
}

#[test]
fn dispatch_vm_capabilities_new() {
    let mut st = fresh_state();
    let res = dispatch(CS_VM, CMD_VM_CAPABILITIES_NEW, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(res.data.len(), 32);
    assert_eq!(
        &res.data[..7],
        &[1, 1, 1, 1, 1, 1, 1],
        "agrees with Capabilities"
    );
    // Wave 25: `canGetMonitorFrameInfo` (17), with
    // `OwnedMonitorsStackDepthInfo`; wave 26: `canRequestVMDeathEvent`
    // (13), with the `SUSPEND_ALL` hold; wave 43:
    // `canGetSourceDebugExtension` (12) and `canForceEarlyReturn` (20);
    // wave 44: `canPopFrames` (10); i4 wave 85: `canUseInstanceFilters`
    // (11); i5 wave 100: `canSetDefaultStratum` (14); i5 wave 103:
    // `canGetInstanceInfo` (15), `canRequestMonitorEvents` (16) and
    // `canGetConstantPool` (19), and `canUseSourceNameFilters` (18) is
    // false, as HotSpot 25 answers it (measured); i5 wave 104:
    // `canRedefineClasses` (7).
    for (i, &b) in res.data.iter().enumerate().skip(7) {
        assert_eq!(
            b,
            u8::from(matches!(i, 7 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 19 | 20)),
            "capability {i}"
        );
    }
}

/// Interpreter round i5 wave 100, lane L1: against a class list, an
/// `ExceptionOnly`, `LocationOnly` or `FieldOnly` naming no class is
/// `INVALID_OBJECT`, as HotSpot's `inStream_readClassRef` answers it;
/// `ExceptionOnly` 0 ("any exception") and a known class are accepted.
#[test]
fn class_ids_in_exception_location_and_field_filters_must_name_a_class() {
    let mut st = fresh_state();
    st.class_signatures.insert(4, "LA;".to_string());
    let set = |st: &mut DebugState, kind: EventKind, modifier: &dyn Fn(&mut PayloadWriter)| {
        let mut body = PayloadWriter::new();
        body.put_u8(kind as u8);
        body.put_u8(SuspendPolicy::None as u8);
        body.put_u32_be(1);
        modifier(&mut body);
        dispatch(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), st).error_code
    };
    let exception_only = |class: u64| {
        move |b: &mut PayloadWriter| {
            b.put_u8(8);
            b.put_u64_be(class);
            b.put_u8(1);
            b.put_u8(1);
        }
    };
    let location_only = |class: u64| {
        move |b: &mut PayloadWriter| {
            b.put_u8(7);
            b.put_u8(1);
            b.put_u64_be(class);
            b.put_u64_be(9);
            b.put_u64_be(0);
        }
    };
    let field_only = |class: u64| {
        move |b: &mut PayloadWriter| {
            b.put_u8(9);
            b.put_u64_be(class);
            b.put_u64_be(5 << 32);
        }
    };
    assert_eq!(set(&mut st, EventKind::Exception, &exception_only(77)), ERR_INVALID_OBJECT);
    assert_eq!(set(&mut st, EventKind::Exception, &exception_only(0)), ERR_NONE);
    assert_eq!(set(&mut st, EventKind::Exception, &exception_only(4)), ERR_NONE);
    assert_eq!(set(&mut st, EventKind::Breakpoint, &location_only(77)), ERR_INVALID_OBJECT);
    assert_eq!(set(&mut st, EventKind::Breakpoint, &location_only(4)), ERR_NONE);
    assert_eq!(
        set(&mut st, EventKind::FieldAccess, &field_only(77)),
        ERR_INVALID_OBJECT
    );
    assert_eq!(set(&mut st, EventKind::FieldAccess, &field_only(4)), ERR_NONE);
}

/// Wave 10: `ExceptionOnly` is stored (it was read and dropped) and an
/// `InstanceOnly` is accepted on a field watch only.
#[test]
fn exception_only_is_stored_and_instance_only_is_scoped_to_field_watches() {
    let mut st = fresh_state();
    let mut body = PayloadWriter::new();
    body.put_u8(EventKind::Exception as u8);
    body.put_u8(SuspendPolicy::All as u8);
    body.put_u32_be(1);
    body.put_u8(8); // ExceptionOnly
    body.put_u64_be(42);
    body.put_u8(0); // caught: no
    body.put_u8(1); // uncaught: yes
    let res = dispatch(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let id = u32::from_be_bytes([res.data[0], res.data[1], res.data[2], res.data[3]]);
    assert_eq!(
        st.events.get_request(id).map(|r| r.modifiers.clone()),
        Some(vec![EventModifier::ExceptionOnly {
            class_id: 42,
            caught: false,
            uncaught: true,
        }])
    );
    assert_eq!(st.events.filter_class_ids(EventKind::Exception), vec![42]);
    let watch = |kind: EventKind| {
        let mut body = PayloadWriter::new();
        body.put_u8(kind as u8);
        body.put_u8(SuspendPolicy::None as u8);
        body.put_u32_be(2);
        body.put_u8(9); // FieldOnly
        body.put_u64_be(5);
        body.put_u64_be(5 << 32);
        body.put_u8(11); // InstanceOnly
        body.put_u64_be(0x77);
        body.into_bytes()
    };
    let res = dispatch(
        CS_EVENT_REQUEST,
        CMD_ER_SET,
        &watch(EventKind::FieldModification),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(
        st.events.instance_filter_ids(EventKind::FieldModification),
        vec![0x77]
    );
    let before = st.events.request_count();
    let res = dispatch(
        CS_EVENT_REQUEST,
        CMD_ER_SET,
        &watch(EventKind::Breakpoint),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_ILLEGAL_ARGUMENT);
    assert_eq!(st.events.request_count(), before);
}

#[test]
fn dispatch_vm_suspend_resume() {
    let mut st = fresh_state();
    assert!(!st.any_suspension());
    dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
    assert!(st.any_suspension() && st.is_thread_suspended(3));
    dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
    // A thread a SUSPEND_EVENT_THREAD breakpoint parked on its own.
    st.suspend_thread(7);
    dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
    assert!(
        !st.any_suspension(),
        "VirtualMachine.Resume resumes a thread suspended once"
    );
}

/// Wave 7: JDWP suspend counts. A thread suspended on its own and by the
/// VM needs two resumes; `VirtualMachine.Resume` takes one from every
/// count and leaves a thread at zero at zero; `SuspendCount` reports it.
#[test]
fn suspensions_are_counted_per_thread() {
    let mut st = fresh_state();
    // Wave 15: `SuspendCount` / `Resume` answer `INVALID_THREAD` for an
    // id that names no thread, so the thread is named.
    st.thread_names.insert(7, "t7".into());
    let tid = 7u64.to_be_bytes();
    let count = |st: &mut DebugState| {
        let res = dispatch(CS_THREAD_REF, CMD_TR_SUSPEND_COUNT, &tid, st);
        assert_eq!(res.error_code, ERR_NONE);
        u32::from_be_bytes([res.data[0], res.data[1], res.data[2], res.data[3]])
    };
    dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
    dispatch(CS_THREAD_REF, CMD_TR_SUSPEND, &tid, &mut st);
    assert_eq!(count(&mut st), 2);
    dispatch(CS_THREAD_REF, CMD_TR_RESUME, &tid, &mut st);
    assert!(st.is_thread_suspended(7), "one resume of two");
    dispatch(CS_THREAD_REF, CMD_TR_RESUME, &tid, &mut st);
    assert_eq!(count(&mut st), 0);
    assert!(st.is_thread_suspended(8), "the VM-wide suspension stands");
    dispatch(CS_THREAD_REF, CMD_TR_RESUME, &tid, &mut st);
    assert_eq!(count(&mut st), 0, "a resume at zero does nothing");
    // VirtualMachine.Resume: 8 goes to zero, 7 stays at zero, and the
    // next VirtualMachine.Suspend stops both again.
    dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
    assert!(!st.is_thread_suspended(7) && !st.is_thread_suspended(8));
    assert!(!st.any_suspension());
    dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
    assert_eq!(count(&mut st), 1);
    assert_eq!(st.suspend_count(8), 1);
    st.resume_all();
    assert!(!st.any_suspension());
}

/// Wave 7: `CreateString` is command 11 and `DisposeObjects` 14 (they
/// were swapped); `DisposeObjects` drops the counted references.
#[test]
fn dispose_objects_releases_counted_ids() {
    use crate::debug::ids::ObjectExport;
    assert_eq!((CMD_VM_CREATE_STRING, CMD_VM_DISPOSE_OBJECTS), (11, 14));
    let mut st = fresh_state();
    st.objects
        .insert(0x1_0000_0000_0001, 0xA0, 0x1000, ObjectExport::Sent);
    st.objects
        .insert(0x1_0000_0000_0002, 0xB0, 0x2000, ObjectExport::Sent);
    let mut pw = PayloadWriter::new();
    pw.put_u32_be(2);
    pw.put_u64_be(0x1_0000_0000_0001);
    pw.put_u32_be(1);
    pw.put_u64_be(0xDEAD); // unknown: ignored
    pw.put_u32_be(1);
    let res = dispatch(CS_VM, CMD_VM_DISPOSE_OBJECTS, &pw.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert!(res.data.is_empty());
    assert!(!st.objects.contains(0x1_0000_0000_0001));
    assert!(st.objects.contains(0x1_0000_0000_0002));
    assert_eq!(st.objects.take_released(), vec![0xA0]);
}

/// Wave 7: a static method's frame has no `this` (its slot 0 is its
/// first argument), and a `this` handed out counts as a sent reference.
#[test]
fn this_object_of_a_static_frame_is_null() {
    use crate::debug::ids::ObjectExport;
    let mut st = fresh_state();
    let this_id = 0x1_0000_0000_0005;
    st.objects
        .insert(this_id, 0xC0, 0x3000, ObjectExport::Pinned);
    st.class_methods.insert(
        4,
        vec![
            MethodInfo {
                method_id: MethodId(1),
                name: "inst".to_string(),
                signature: "()V".to_string(),
                mod_bits: 0x0001,
            },
            MethodInfo {
                method_id: MethodId(2),
                name: "stat".to_string(),
                signature: "(Ljava/lang/Object;)V".to_string(),
                mod_bits: 0x0009,
            },
        ],
    );
    st.thread_frames.insert(
        3,
        vec![
            crate::debug::FrameEntry {
                frame_id: 0,
                class_id: 4,
                method_id: 2,
                offset: 0,
            },
            crate::debug::FrameEntry {
                frame_id: 1,
                class_id: 4,
                method_id: 1,
                offset: 0,
            },
        ],
    );
    for frame in 0..2 {
        st.update_frame_locals(3, frame, vec![crate::debug::LocalValue::ObjectRef(this_id)]);
    }
    // Wave 14: a `StackFrame` command needs the thread suspended.
    st.thread_names.insert(3, "worker".into());
    st.suspend_thread(3);
    let this_of = |st: &mut DebugState, frame: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(3);
        pw.put_u64_be(frame);
        let res = dispatch(CS_STACK_FRAME, CMD_SF_THIS_OBJECT, &pw.into_bytes(), st);
        u64::from_be_bytes(res.data[1..9].try_into().unwrap())
    };
    assert_eq!(this_of(&mut st, 0), 0, "static frame");
    assert_eq!(this_of(&mut st, 1), this_id, "instance frame");
    // Sent once, so the snapshot's unpin keeps it.
    st.objects.unpin(this_id);
    assert!(st.objects.contains(this_id));
}

/// Wave 6: `ThreadReference.Resume` releases one thread from a VM-wide
/// suspension, `ThreadReference.Status` reports a VM-wide suspension, and
/// the next `VirtualMachine.Suspend` stops the resumed thread again.
#[test]
fn a_thread_resumes_out_of_a_vm_wide_suspension() {
    let mut st = fresh_state();
    // Wave 11: `Status` answers `INVALID_THREAD` for an id that names no
    // thread, so the two threads are named.
    st.thread_names.insert(7, "t7".into());
    st.thread_names.insert(8, "t8".into());
    let tid = 7u64.to_be_bytes();
    let status = |st: &mut DebugState| {
        let res = dispatch(CS_THREAD_REF, CMD_TR_STATUS, &tid, st);
        assert_eq!(res.error_code, ERR_NONE);
        res.data[7] // suspendStatus, the low byte of the second u32
    };
    dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
    assert!(st.is_thread_suspended(7) && st.is_thread_suspended(8));
    assert_eq!(status(&mut st), 1, "suspended by VirtualMachine.Suspend");
    dispatch(CS_THREAD_REF, CMD_TR_RESUME, &tid, &mut st);
    assert!(!st.is_thread_suspended(7), "resumed on its own");
    assert!(st.is_thread_suspended(8), "the others stay suspended");
    assert_eq!(status(&mut st), 0);
    dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
    assert!(st.is_thread_suspended(7), "a new VM-wide suspension");
    dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
    assert!(!st.is_thread_suspended(7));
    // Wave 7 (suspend counts): thread 8 was suspended by both
    // `VirtualMachine.Suspend`s, so it needs both resumes.
    assert!(st.is_thread_suspended(8), "count 2 -> 1");
    dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
    assert!(!st.is_thread_suspended(7) && !st.is_thread_suspended(8));
}

/// Wave 6: `Method.VariableTable`'s `argCnt` is the argument frame words
/// (`this`, two per `long` / `double`), not the variable count.
#[test]
fn variable_table_arg_count_is_argument_words() {
    let mut st = fresh_state();
    let method = |id: u64, sig: &str, mod_bits: u32| MethodInfo {
        method_id: MethodId(id),
        name: "m".to_string(),
        signature: sig.to_string(),
        mod_bits,
    };
    st.class_methods.insert(
        5,
        vec![
            method(1, "(IJ[DLjava/lang/String;[[Ljava/lang/Object;)V", 0x0001),
            method(2, "(D)I", 0x0008),
        ],
    );
    assert_eq!(method_arg_words(&st, 5, 1), 1 + 1 + 2 + 1 + 1 + 1);
    assert_eq!(method_arg_words(&st, 5, 2), 2, "static: no `this`");
    assert_eq!(method_arg_words(&st, 5, 3), 0, "unknown method");

    st.method_variables.insert(
        (5, 2),
        vec![
            crate::debug::VariableInfo {
                code_index: 0,
                name: "d".to_string(),
                signature: "D".to_string(),
                length: 4,
                slot: 0,
            },
            crate::debug::VariableInfo {
                code_index: 2,
                name: "n".to_string(),
                signature: "I".to_string(),
                length: 2,
                slot: 2,
            },
        ],
    );
    let mut payload = 5u64.to_be_bytes().to_vec();
    payload.extend(2u64.to_be_bytes());
    let res = dispatch(CS_METHOD, CMD_M_VARIABLE_TABLE, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(&res.data[0..4], &2u32.to_be_bytes(), "argCnt");
    assert_eq!(&res.data[4..8], &2u32.to_be_bytes(), "slots");
}

#[test]
fn dispatch_vm_dispose() {
    let mut st = fresh_state();
    dispatch(CS_VM, CMD_VM_DISPOSE, &[], &mut st);
    assert!(st.disposed);
}

#[test]
fn dispatch_vm_all_threads_empty() {
    let mut st = fresh_state();
    let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    // count = 0  (4 bytes)
    assert_eq!(res.data, [0, 0, 0, 0]);
}

#[test]
fn dispatch_vm_all_threads_with_threads() {
    let mut st = fresh_state();
    let t1 = st.ids.register_thread(100);
    st.thread_names.insert(t1.0, "main".into());
    let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    // count(4) + 1 thread_id(8) = 12
    assert_eq!(res.data.len(), 12);
}

/// Wave 49 (lane L1): `AllThreads` and `ThreadGroupReference.Children`
/// leave the virtual threads out, as HotSpot's back end does without
/// `includevirtualthreads=y`; with it (`include_virtual_threads`)
/// `AllThreads` lists them, and (wave 102) `Children` still does not:
/// JVMTI `GetThreadGroupChildren` returns platform threads only.
#[test]
fn all_threads_leaves_virtual_threads_out() {
    let mut st = fresh_state();
    st.include_virtual_threads = false;
    st.thread_names.insert(7, "main".into());
    st.thread_names.insert(9, "vsleeper".into());
    st.virtual_threads.insert(9);
    let count = |data: &[u8]| u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let listed = |st: &mut DebugState| {
        let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], st);
        assert_eq!(res.error_code, ERR_NONE);
        let children = dispatch(CS_THREAD_GROUP_REF, CMD_TGR_CHILDREN, &[0; 8], st);
        assert_eq!(children.error_code, ERR_NONE);
        (count(&res.data), count(&children.data))
    };
    assert_eq!(listed(&mut st), (1, 1));
    assert_eq!(known_thread_ids(&st), vec![7, 9]);
    st.include_virtual_threads = true;
    assert_eq!(listed(&mut st), (2, 1));
}

/// Wave 11: `ThreadReference.Status` answers the state the server
/// refreshed from the registry (it answered RUNNING for every id), a
/// zombie is not suspended and is left out of `AllThreads` and
/// `ThreadGroupReference.Children`, and an id that names no thread is
/// `INVALID_THREAD`.
#[test]
fn thread_status_answers_the_refreshed_state() {
    let mut st = fresh_state();
    for (tid, name, status) in [
        (1u64, "main", THREAD_STATUS_RUNNING),
        (2, "sleeper", THREAD_STATUS_SLEEPING),
        (3, "waiter", THREAD_STATUS_WAIT),
        (4, "blocked", THREAD_STATUS_MONITOR),
        (5, "done", THREAD_STATUS_ZOMBIE),
    ] {
        st.thread_names.insert(tid, name.into());
        st.thread_statuses.insert(tid, status);
    }
    let status_of = |st: &mut DebugState, tid: u64| {
        let res = dispatch(CS_THREAD_REF, CMD_TR_STATUS, &tid.to_be_bytes(), st);
        (res.error_code, res.data)
    };
    let answer = |thread: u32, suspended: u32| {
        let mut pw = PayloadWriter::new();
        pw.put_u32_be(thread);
        pw.put_u32_be(suspended);
        pw.into_bytes()
    };
    assert_eq!(status_of(&mut st, 2), (ERR_NONE, answer(2, 0)));
    assert_eq!(status_of(&mut st, 3), (ERR_NONE, answer(4, 0)));
    assert_eq!(status_of(&mut st, 4), (ERR_NONE, answer(3, 0)));
    dispatch(CS_VM, CMD_VM_SUSPEND, &[], &mut st);
    assert_eq!(status_of(&mut st, 1), (ERR_NONE, answer(1, 1)));
    assert_eq!(
        status_of(&mut st, 5),
        (ERR_NONE, answer(0, 0)),
        "a zombie is never suspended"
    );
    dispatch(CS_VM, CMD_VM_RESUME, &[], &mut st);
    assert_eq!(status_of(&mut st, 99).0, ERR_INVALID_THREAD);

    let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let mut expected = PayloadWriter::new();
    expected.put_u32_be(4);
    for tid in 1u64..=4 {
        expected.put_u64_be(tid);
    }
    assert_eq!(res.data, expected.into_bytes(), "the zombie is not listed");

    // `ThreadReference.ThreadGroup`: the "system" group for a live
    // thread, the null group for a zombie, `INVALID_THREAD` for no thread.
    let group_of = |st: &mut DebugState, tid: u64| {
        let res = dispatch(CS_THREAD_REF, CMD_TR_THREAD_GROUP, &tid.to_be_bytes(), st);
        (res.error_code, res.data)
    };
    assert_eq!(
        group_of(&mut st, 2),
        (ERR_NONE, SYSTEM_THREAD_GROUP_ID.to_be_bytes().to_vec())
    );
    assert_eq!(
        group_of(&mut st, 5),
        (ERR_NONE, 0u64.to_be_bytes().to_vec())
    );
    assert_eq!(group_of(&mut st, 99).0, ERR_INVALID_THREAD);

    // `ThreadReference.Suspend`: no thread is `INVALID_THREAD`; a zombie
    // is counted (HotSpot 25 answers `SuspendCount` 1, wave 29) but does
    // not arm the gates — neither leaves a live-thread suspension.
    let suspend = |st: &mut DebugState, tid: u64| {
        dispatch(CS_THREAD_REF, CMD_TR_SUSPEND, &tid.to_be_bytes(), st).error_code
    };
    assert_eq!(suspend(&mut st, 99), ERR_INVALID_THREAD);
    assert_eq!(suspend(&mut st, 5), ERR_NONE);
    assert!(
        !st.any_suspension_of_a_live_thread(),
        "neither arms the gates"
    );
    assert_eq!(st.suspend_count(5), 1, "the zombie's suspension is counted");
    assert_eq!(suspend(&mut st, 3), ERR_NONE);
    assert!(st.is_thread_suspended(3));
}

/// Wave 11: `ThreadReference.Frames` / `FrameCount` answer for a
/// suspended thread only (`THREAD_NOT_SUSPENDED` otherwise,
/// `INVALID_THREAD` for no thread), and a range past the frames is
/// `INVALID_INDEX` / a negative length `INVALID_LENGTH`, as in HotSpot;
/// they answered the snapshot (or 0 frames) regardless and clamped the
/// range.
#[test]
fn frames_need_a_suspended_thread_and_a_valid_range() {
    let mut st = fresh_state();
    st.thread_names.insert(3, "worker".into());
    let entry = |frame_id: u64| crate::debug::FrameEntry {
        frame_id,
        class_id: 100,
        method_id: 7,
        offset: frame_id,
    };
    st.thread_frames.insert(3, vec![entry(0), entry(1)]);
    let frames = |st: &mut DebugState, tid: u64, start: i32, len: i32| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(tid);
        pw.put_u32_be(start as u32); // Cast: JDWP int
        pw.put_u32_be(len as u32); // Cast: JDWP int
        let res = dispatch(CS_THREAD_REF, CMD_TR_FRAMES, &pw.into_bytes(), st);
        (res.error_code, res.data)
    };
    let count = |st: &mut DebugState, tid: u64| {
        dispatch(CS_THREAD_REF, CMD_TR_FRAME_COUNT, &tid.to_be_bytes(), st).error_code
    };
    assert_eq!(frames(&mut st, 3, 0, -1).0, ERR_THREAD_NOT_SUSPENDED);
    assert_eq!(count(&mut st, 3), ERR_THREAD_NOT_SUSPENDED);
    assert_eq!(frames(&mut st, 99, 0, -1).0, ERR_INVALID_THREAD);
    assert_eq!(count(&mut st, 99), ERR_INVALID_THREAD);

    dispatch(CS_THREAD_REF, CMD_TR_SUSPEND, &3u64.to_be_bytes(), &mut st);
    assert_eq!(count(&mut st, 3), ERR_NONE);
    let (err, data) = frames(&mut st, 3, 0, -1);
    assert_eq!(err, ERR_NONE);
    assert_eq!(&data[0..4], &2u32.to_be_bytes(), "both frames");
    let (err, data) = frames(&mut st, 3, 1, 1);
    assert_eq!(err, ERR_NONE);
    assert_eq!(&data[0..4], &1u32.to_be_bytes());
    assert_eq!(&data[4..12], &1u64.to_be_bytes(), "the second frame");
    assert_eq!(frames(&mut st, 3, 2, -1).0, ERR_NONE, "none remaining");
    // Wave 42: HotSpot 25.0.3's answers (`L1W42RawJdwpErrorAnswers`).
    assert_eq!(frames(&mut st, 3, 1, 2).0, ERR_INVALID_LENGTH);
    assert_eq!(frames(&mut st, 3, 0, 3).0, ERR_INVALID_LENGTH);
    let (err, data) = frames(&mut st, 3, 3, 0);
    assert_eq!(err, ERR_NONE, "an empty range past the end");
    assert_eq!(&data[0..4], &0u32.to_be_bytes());
    assert_eq!(
        frames(&mut st, 3, 3, -1).0,
        ERR_INVALID_INDEX,
        "past the end"
    );
    assert_eq!(frames(&mut st, 3, -1, 1).0, ERR_INVALID_INDEX);
    assert_eq!(frames(&mut st, 3, 0, -2).0, ERR_INVALID_LENGTH);
}

#[test]
fn dispatch_unknown_command() {
    let mut st = fresh_state();
    let res = dispatch(255, 255, &[], &mut st);
    assert_eq!(res.error_code, ERR_NOT_IMPLEMENTED);
}

#[test]
fn dispatch_thread_name() {
    let mut st = fresh_state();
    let tid = st.ids.register_thread(1);
    st.thread_names.insert(tid.0, "worker-0".into());

    // Build payload: thread_id as u64 BE
    let data = tid.0.to_be_bytes();
    let res = dispatch(CS_THREAD_REF, CMD_TR_NAME, &data, &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let payload = String::from_utf8_lossy(&res.data);
    assert!(payload.contains("worker-0"));
}

#[test]
fn dispatch_event_request_set_and_clear() {
    let mut st = fresh_state();

    // Set a breakpoint event request.
    let mut set_data = PayloadWriter::new();
    set_data.put_u8(EventKind::Breakpoint as u8); // eventKind
    set_data.put_u8(SuspendPolicy::All as u8); // suspendPolicy
                                               // Wave 29: a breakpoint needs its location (`INTERNAL` without one).
    set_data.put_u32_be(1);
    put_location_only(&mut set_data);
    let res = dispatch(
        CS_EVENT_REQUEST,
        CMD_ER_SET,
        &set_data.into_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_NONE);
    let req_id = u32::from_be_bytes([res.data[0], res.data[1], res.data[2], res.data[3]]);
    assert!(req_id > 0);

    // Clear it.
    let mut clear_data = PayloadWriter::new();
    clear_data.put_u8(EventKind::Breakpoint as u8);
    clear_data.put_u32_be(req_id);
    let res = dispatch(
        CS_EVENT_REQUEST,
        CMD_ER_CLEAR,
        &clear_data.into_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(st.events.request_count(), 0);
}

/// Wave 9: an event request carrying a modifier this server cannot read
/// (here 11, `InstanceOnly`) is refused with `ILLEGAL_ARGUMENT` and
/// registers nothing; it used to be registered with the modifiers read
/// before it, reporting events its filters would have suppressed.
#[test]
fn a_request_with_an_unreadable_modifier_is_refused() {
    let mut st = fresh_state();
    let mut body = PayloadWriter::new();
    // Wave 85: a thread start (a breakpoint takes `InstanceOnly` now).
    body.put_u8(EventKind::ThreadStart as u8);
    body.put_u8(SuspendPolicy::All as u8);
    body.put_u32_be(2);
    body.put_u8(11); // InstanceOnly
    body.put_u64_be(0x1234);
    body.put_u8(1); // Count
    body.put_u32_be(1);
    let res = dispatch(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_ILLEGAL_ARGUMENT);
    assert_eq!(st.events.request_count(), 0);
}

/// Wave 9: `EventRequest.ClearAllBreakpoints` removes every breakpoint
/// request and nothing else (it answered `NOT_IMPLEMENTED`).
#[test]
fn clear_all_breakpoints_leaves_the_other_requests() {
    let mut st = fresh_state();
    for kind in [
        EventKind::Breakpoint,
        EventKind::Breakpoint,
        EventKind::ThreadStart,
    ] {
        let mut body = PayloadWriter::new();
        body.put_u8(kind as u8);
        body.put_u8(SuspendPolicy::None as u8);
        // Wave 29: a breakpoint needs its location (`INTERNAL` without
        // one); a thread start takes none (`ILLEGAL_ARGUMENT`).
        if kind == EventKind::Breakpoint {
            body.put_u32_be(2);
            put_location_only(&mut body);
        } else {
            body.put_u32_be(1);
        }
        body.put_u8(1); // Count
        body.put_u32_be(3);
        let res = dispatch(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
        assert_eq!(res.error_code, ERR_NONE);
    }
    assert_eq!(st.events.request_count(), 3);
    let res = dispatch(CS_EVENT_REQUEST, CMD_ER_CLEAR_ALL_BREAKPOINTS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(
        st.events.request_count(),
        1,
        "the ThreadStart request stays"
    );
}

/// Wave 8: `CreateString` and the other heap commands are served by
/// `debug::inspect` against the live VM; a bare `DebugState` refuses them
/// instead of minting an id with no `String` behind it.
#[test]
fn dispatch_heap_commands_without_a_vm_are_refused() {
    let mut st = fresh_state();
    let mut data = PayloadWriter::new();
    data.put_string("hello");
    let res = dispatch(CS_VM, CMD_VM_CREATE_STRING, &data.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_NOT_IMPLEMENTED);
    for (set, cmd) in [
        (CS_OBJECT_REF, CMD_OR_GET_VALUES),
        (CS_OBJECT_REF, CMD_OR_SET_VALUES),
        (CS_ARRAY_REF, CMD_AR_LENGTH),
        (CS_STRING_REF, CMD_SR_VALUE),
        (CS_STACK_FRAME, CMD_SF_SET_VALUES),
    ] {
        let res = dispatch(set, cmd, &0u64.to_be_bytes(), &mut st);
        assert_eq!(res.error_code, ERR_NOT_IMPLEMENTED, "({set}, {cmd})");
    }
    assert_eq!(ERR_INVALID_EVENT_TYPE, 102, "JDWP INVALID_EVENT_TYPE");
}

#[test]
fn dispatch_top_level_thread_groups() {
    let mut st = fresh_state();
    let res = dispatch(CS_VM, CMD_VM_TOP_LEVEL_THREAD_GROUPS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    // count(4) + group_id(8) = 12
    assert_eq!(res.data.len(), 12);
}

// -----------------------------------------------------------------------
// T6.4 wire-level smoke test: build real JDWP packet bytes, dispatch, and
// assert the exact reply bytes, same as an IDE like IntelliJ would.
// -----------------------------------------------------------------------

use crate::debug::protocol::{read_packet, write_packet, JdwpPacket, REPLY_FLAG};
use std::io::Cursor;

fn wire_roundtrip(cmd_set: u8, cmd: u8, payload: &[u8], st: &mut DebugState) -> JdwpPacket {
    // --- Client side: build a command packet and serialise it. ------
    let client_pkt = JdwpPacket::Command {
        id: 0xCAFEBABE,
        flags: 0,
        command_set: cmd_set,
        command: cmd,
        data: payload.to_vec(),
    };
    let mut wire = Vec::new();
    write_packet(&mut wire, &client_pkt).unwrap();

    // --- Server side: parse from the wire, dispatch, produce reply. -
    let parsed = read_packet(&mut Cursor::new(&wire[..])).unwrap();
    let (id, cs, c, data) = match parsed {
        JdwpPacket::Command {
            id,
            command_set,
            command,
            data,
            ..
        } => (id, command_set, command, data),
        _ => panic!("expected command"),
    };
    let result = dispatch(cs, c, &data, st);
    let reply = if result.error_code == 0 {
        JdwpPacket::ok_reply(id, result.data)
    } else {
        JdwpPacket::error_reply(id, result.error_code)
    };
    let mut reply_wire = Vec::new();
    write_packet(&mut reply_wire, &reply).unwrap();

    // --- Client side again: parse the reply bytes. -----------------
    read_packet(&mut Cursor::new(&reply_wire[..])).unwrap()
}

#[test]
fn t64_wire_vm_version() {
    let mut st = fresh_state();
    let reply = wire_roundtrip(CS_VM, CMD_VM_VERSION, &[], &mut st);
    match reply {
        JdwpPacket::Reply {
            id,
            error_code,
            data,
        } => {
            assert_eq!(id, 0xCAFEBABE);
            assert_eq!(error_code, 0);
            assert!(String::from_utf8_lossy(&data).contains("CratonVM"));
        }
        _ => panic!("expected reply"),
    }
}

#[test]
fn t64_wire_vm_id_sizes() {
    let mut st = fresh_state();
    let reply = wire_roundtrip(CS_VM, CMD_VM_ID_SIZES, &[], &mut st);
    match reply {
        JdwpPacket::Reply {
            id,
            error_code,
            data,
        } => {
            assert_eq!(id, 0xCAFEBABE);
            assert_eq!(error_code, 0);
            // Five u32 BE fields, each = 8 (our ID size).
            assert_eq!(data.len(), 20);
            for chunk in data.chunks(4) {
                let v = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                assert_eq!(v, 8);
            }
        }
        _ => panic!("expected reply"),
    }
}

#[test]
fn t64_wire_event_request_set_breakpoint_then_clear() {
    let mut st = fresh_state();

    // EventRequest/Set, BREAKPOINT, SuspendPolicy::All, its location
    // (wave 29: required).
    let mut body = PayloadWriter::new();
    body.put_u8(EventKind::Breakpoint as u8);
    body.put_u8(SuspendPolicy::All as u8);
    body.put_u32_be(1);
    put_location_only(&mut body);
    let reply = wire_roundtrip(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
    let req_id = match reply {
        JdwpPacket::Reply {
            error_code, data, ..
        } => {
            assert_eq!(error_code, 0);
            assert_eq!(data.len(), 4);
            u32::from_be_bytes([data[0], data[1], data[2], data[3]])
        }
        _ => panic!("expected reply"),
    };
    assert!(req_id > 0);
    assert_eq!(st.events.request_count(), 1);

    // EventRequest/Clear same request.
    let mut clr = PayloadWriter::new();
    clr.put_u8(EventKind::Breakpoint as u8);
    clr.put_u32_be(req_id);
    let reply = wire_roundtrip(CS_EVENT_REQUEST, CMD_ER_CLEAR, &clr.into_bytes(), &mut st);
    match reply {
        JdwpPacket::Reply {
            error_code, data, ..
        } => {
            assert_eq!(error_code, 0);
            assert!(data.is_empty());
        }
        _ => panic!("expected reply"),
    };
    assert_eq!(st.events.request_count(), 0);
}

#[test]
fn t64_wire_event_request_set_step_over() {
    let mut st = fresh_state();
    // Wave 29: a Step modifier must name a thread the server knows.
    st.thread_names.insert(1, "main".into());

    // EventRequest/Set, SINGLE_STEP, with one Step modifier for step-over
    // (size=LINE, depth=OVER).
    let mut body = PayloadWriter::new();
    body.put_u8(EventKind::SingleStep as u8);
    body.put_u8(SuspendPolicy::EventThread as u8);
    body.put_u32_be(1); // one modifier
    body.put_u8(10); // mod kind = Step
    body.put_u64_be(1); // thread id
    body.put_u32_be(StepSize::Line as u32);
    body.put_u32_be(StepDepth::Over as u32);

    let reply = wire_roundtrip(CS_EVENT_REQUEST, CMD_ER_SET, &body.into_bytes(), &mut st);
    match reply {
        JdwpPacket::Reply {
            error_code, data, ..
        } => {
            assert_eq!(error_code, 0);
            assert_eq!(data.len(), 4);
            let req_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            assert!(req_id > 0);
        }
        _ => panic!("expected reply"),
    }
    assert_eq!(st.events.request_count(), 1);
}

#[test]
fn t64_reply_flag_round_trips_over_wire() {
    // Double check that the reply we emit actually has REPLY_FLAG set on
    // byte 8 of the header — this is what clients parse.
    let mut st = fresh_state();
    let client_pkt = JdwpPacket::Command {
        id: 1,
        flags: 0,
        command_set: CS_VM,
        command: CMD_VM_VERSION,
        data: Vec::new(),
    };
    let mut wire = Vec::new();
    write_packet(&mut wire, &client_pkt).unwrap();
    let parsed = read_packet(&mut Cursor::new(&wire[..])).unwrap();
    let (id, cs, c, data) = match parsed {
        JdwpPacket::Command {
            id,
            command_set,
            command,
            data,
            ..
        } => (id, command_set, command, data),
        _ => panic!(),
    };
    let result = dispatch(cs, c, &data, &mut st);
    let reply = JdwpPacket::ok_reply(id, result.data);
    let mut reply_wire = Vec::new();
    write_packet(&mut reply_wire, &reply).unwrap();
    // Byte 8 of the header carries the flags (reply_flag).
    assert_eq!(reply_wire[8] & REPLY_FLAG, REPLY_FLAG);
}

// =======================================================================
// T6.5 — Debugger-initiated method invocation
// =======================================================================

use crate::debug::{BridgeError, DebuggerValue, DebuggerVmBridge, InvokeOutcome};
use std::sync::{Arc, Mutex};

/// Scriptable mock bridge used to drive the wire-level tests for the
/// three invocation commands without spinning up a full VM.
#[derive(Default)]
struct MockBridge {
    /// Queued outcomes for `invoke_static`.
    static_outcomes: Mutex<Vec<Result<InvokeOutcome, BridgeError>>>,
    /// Queued outcomes for `invoke_instance`.
    instance_outcomes: Mutex<Vec<Result<InvokeOutcome, BridgeError>>>,
    /// Queued outcomes for `new_array`.
    array_outcomes: Mutex<Vec<Result<u64, BridgeError>>>,
    /// Log of everything the handler actually passed us.
    calls: Mutex<Vec<MockCall>>,
    /// The `single_threaded` flag of each invocation, in call order.
    single_threaded: Mutex<Vec<bool>>,
}

#[derive(Debug, Clone)]
enum MockCall {
    Static {
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: Vec<DebuggerValue>,
        return_sig: String,
    },
    Instance {
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: Vec<DebuggerValue>,
        return_sig: String,
        non_virtual: bool,
    },
    NewArray {
        array_type_id: u64,
        length: i32,
    },
    NewInstance {
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: Vec<DebuggerValue>,
    },
}

impl DebuggerVmBridge for MockBridge {
    fn invoke_static(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.single_threaded.lock().unwrap().push(single_threaded);
        self.calls.lock().unwrap().push(MockCall::Static {
            class_id,
            method_id,
            thread_id,
            args: args.to_vec(),
            return_sig: return_sig.to_string(),
        });
        self.static_outcomes
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Err(BridgeError::Internal))
    }

    fn invoke_instance(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        non_virtual: bool,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.single_threaded.lock().unwrap().push(single_threaded);
        self.calls.lock().unwrap().push(MockCall::Instance {
            receiver_id,
            class_id,
            method_id,
            thread_id,
            args: args.to_vec(),
            return_sig: return_sig.to_string(),
            non_virtual,
        });
        self.instance_outcomes
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Err(BridgeError::Internal))
    }

    fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError> {
        self.calls.lock().unwrap().push(MockCall::NewArray {
            array_type_id,
            length,
        });
        self.array_outcomes
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Err(BridgeError::Internal))
    }

    /// Wave 24: answered from the `invoke_static` queue.
    fn new_instance(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.single_threaded.lock().unwrap().push(single_threaded);
        self.calls.lock().unwrap().push(MockCall::NewInstance {
            class_id,
            method_id,
            thread_id,
            args: args.to_vec(),
        });
        self.static_outcomes
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Err(BridgeError::Internal))
    }
}

/// Register a method with a known return signature on the state and
/// install a fresh mock bridge.
fn state_with_bridge(
    class_id: u64,
    method_id: u64,
    descriptor: &str,
) -> (DebugState, Arc<MockBridge>) {
    let mut st = DebugState::new();
    // Register a minimal method so `return_signature_for` can resolve
    // the descriptor and hand the bridge the right return signature.
    st.class_methods.insert(
        class_id,
        vec![MethodInfo {
            method_id: crate::debug::ids::MethodId(method_id),
            name: "mock".to_string(),
            signature: descriptor.to_string(),
            mod_bits: 0,
        }],
    );
    let bridge: Arc<MockBridge> = Arc::new(MockBridge::default());
    st.vm_bridge = Some(bridge.clone() as Arc<dyn DebuggerVmBridge>);
    (st, bridge)
}

fn build_ct_invoke_payload(
    class_id: u64,
    thread_id: u64,
    method_id: u64,
    args: &[DebuggerValue],
    invoke_options: u32,
) -> Vec<u8> {
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(class_id);
    pw.put_u64_be(thread_id);
    pw.put_u64_be(method_id);
    pw.put_u32_be(args.len() as u32);
    for a in args {
        write_tagged_value(&mut pw, a);
    }
    pw.put_u32_be(invoke_options);
    pw.into_bytes()
}

fn build_or_invoke_payload(
    object_id: u64,
    thread_id: u64,
    class_id: u64,
    method_id: u64,
    args: &[DebuggerValue],
    invoke_options: u32,
) -> Vec<u8> {
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(object_id);
    pw.put_u64_be(thread_id);
    pw.put_u64_be(class_id);
    pw.put_u64_be(method_id);
    pw.put_u32_be(args.len() as u32);
    for a in args {
        write_tagged_value(&mut pw, a);
    }
    pw.put_u32_be(invoke_options);
    pw.into_bytes()
}

// --- ClassType.InvokeMethod --------------------------------------------

#[test]
fn t65_ct_invoke_method_happy_path_int_return() {
    let (mut st, bridge) = state_with_bridge(10, 42, "(II)I");
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::returned(DebuggerValue::Int(7))));

    let payload = build_ct_invoke_payload(
        10,
        1,
        42,
        &[DebuggerValue::Int(3), DebuggerValue::Int(4)],
        0,
    );
    let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);

    // Reply: tag(1)=I + int(4) + excTag(1)=L + excId(8) = 14 bytes.
    assert_eq!(res.data.len(), 14);
    assert_eq!(res.data[0], b'I');
    let ret = i32::from_be_bytes([res.data[1], res.data[2], res.data[3], res.data[4]]);
    assert_eq!(ret, 7);
    // Exception: tag L, id 0.
    assert_eq!(res.data[5], b'L');
    assert_eq!(
        u64::from_be_bytes([
            res.data[6],
            res.data[7],
            res.data[8],
            res.data[9],
            res.data[10],
            res.data[11],
            res.data[12],
            res.data[13]
        ]),
        0
    );

    // Verify handler forwarded the right things to the bridge.
    let calls = bridge.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    match &calls[0] {
        MockCall::Static {
            class_id,
            method_id,
            thread_id,
            args,
            return_sig,
        } => {
            assert_eq!(*class_id, 10);
            assert_eq!(*method_id, 42);
            assert_eq!(*thread_id, 1);
            assert_eq!(args, &vec![DebuggerValue::Int(3), DebuggerValue::Int(4)]);
            assert_eq!(return_sig, "I");
        }
        c => panic!("expected Static call, got {:?}", c),
    }
}

#[test]
fn t65_ct_invoke_method_exception_path() {
    let (mut st, bridge) = state_with_bridge(10, 42, "()V");
    // Simulate the method throwing — exception ID 99.
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::threw(99)));

    let payload = build_ct_invoke_payload(10, 1, 42, &[], 0);
    let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);

    // Reply: returnValue = null object (tag L + 8-byte zero), per JDWP
    // convention when the method throws.  Then excTag L + excId 8.
    // Total: 1 + 8 + 1 + 8 = 18 bytes.
    assert_eq!(res.data.len(), 18);
    assert_eq!(res.data[0], b'L');
    let ret_id = u64::from_be_bytes([
        res.data[1],
        res.data[2],
        res.data[3],
        res.data[4],
        res.data[5],
        res.data[6],
        res.data[7],
        res.data[8],
    ]);
    assert_eq!(ret_id, 0); // null return
    assert_eq!(res.data[9], b'L');
    let exc = u64::from_be_bytes([
        res.data[10],
        res.data[11],
        res.data[12],
        res.data[13],
        res.data[14],
        res.data[15],
        res.data[16],
        res.data[17],
    ]);
    assert_eq!(exc, 99);
}

#[test]
fn t65_ct_invoke_method_invalid_thread() {
    let (mut st, bridge) = state_with_bridge(10, 42, "()V");
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Err(BridgeError::InvalidThread));

    let payload = build_ct_invoke_payload(10, 0xDEAD, 42, &[], 0);
    let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_INVALID_THREAD);
    assert!(res.data.is_empty());
}

// --- ObjectReference.InvokeMethod --------------------------------------

#[test]
fn t65_or_invoke_method_happy_path_object_return() {
    let (mut st, bridge) = state_with_bridge(5, 17, "()Ljava/lang/String;");
    bridge
        .instance_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::returned(DebuggerValue::Object(0xBEEF))));

    let payload = build_or_invoke_payload(100, 1, 5, 17, &[], 0);
    let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);

    // Reply: tag(1)=L + objId(8) + excTag(1)=L + excId(8) = 18 bytes.
    assert_eq!(res.data.len(), 18);
    assert_eq!(res.data[0], b'L');
    let obj_id = u64::from_be_bytes([
        res.data[1],
        res.data[2],
        res.data[3],
        res.data[4],
        res.data[5],
        res.data[6],
        res.data[7],
        res.data[8],
    ]);
    assert_eq!(obj_id, 0xBEEF);
    assert_eq!(res.data[9], b'L');

    let calls = bridge.calls.lock().unwrap();
    match &calls[0] {
        MockCall::Instance {
            receiver_id,
            class_id,
            method_id,
            thread_id,
            args,
            return_sig,
            non_virtual,
        } => {
            assert_eq!(*receiver_id, 100);
            assert_eq!(*class_id, 5);
            assert_eq!(*method_id, 17);
            assert_eq!(*thread_id, 1);
            assert!(args.is_empty());
            assert_eq!(return_sig, "Ljava/lang/String;");
            assert!(!non_virtual);
        }
        c => panic!("expected Instance call, got {:?}", c),
    }
}

#[test]
fn t65_or_invoke_method_non_virtual_dispatch() {
    let (mut st, bridge) = state_with_bridge(5, 17, "()V");
    bridge
        .instance_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::returned(DebuggerValue::Void)));

    // invokeOptions = 2 => non-virtual
    let payload = build_or_invoke_payload(100, 1, 5, 17, &[], 2);
    let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);

    let calls = bridge.calls.lock().unwrap();
    match &calls[0] {
        MockCall::Instance { non_virtual, .. } => assert!(*non_virtual),
        c => panic!("expected Instance call, got {:?}", c),
    }
}

/// Wave 9: `INVOKE_SINGLE_THREADED` reaches the bridge (it was never
/// read), alone or with `INVOKE_NONVIRTUAL`, and an invocation is marked
/// as running Java (the JDWP server answers it asynchronously) while an
/// array allocation is not. `ALREADY_INVOKING` is JDWP error 502.
#[test]
fn invoke_options_reach_the_bridge() {
    let (mut st, bridge) = state_with_bridge(5, 17, "()V");
    for _ in 0..3 {
        bridge
            .instance_outcomes
            .lock()
            .unwrap()
            .push(Ok(InvokeOutcome::returned(DebuggerValue::Void)));
    }
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Err(BridgeError::AlreadyInvoking));
    for options in [
        0,
        INVOKE_SINGLE_THREADED,
        INVOKE_SINGLE_THREADED | INVOKE_NONVIRTUAL,
    ] {
        let payload = build_or_invoke_payload(100, 1, 5, 17, &[], options);
        let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
        assert_eq!(res.error_code, ERR_NONE, "options {options}");
    }
    let payload = build_ct_invoke_payload(5, 1, 17, &[], INVOKE_SINGLE_THREADED);
    let res = dispatch(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_ALREADY_INVOKING);
    assert_eq!(ERR_ALREADY_INVOKING, 502);
    assert_eq!(
        *bridge.single_threaded.lock().unwrap(),
        vec![false, true, true, true]
    );
    let calls = bridge.calls.lock().unwrap();
    assert!(
        matches!(
            &calls[2],
            MockCall::Instance {
                non_virtual: true,
                ..
            }
        ),
        "both bits: {:?}",
        calls[2]
    );
    drop(calls);
    let payload = build_ct_invoke_payload(5, 1, 17, &[], 0);
    let prepared = prepare_bridge_command(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &st);
    assert!(matches!(prepared, Some(Ok(ref call)) if call.runs_java()));
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(77);
    pw.put_u32_be(1);
    let prepared =
        prepare_bridge_command(CS_ARRAY_TYPE, CMD_AT_NEW_INSTANCE, &pw.into_bytes(), &st);
    assert!(matches!(prepared, Some(Ok(ref call)) if !call.runs_java()));
}

#[test]
fn t65_or_invoke_method_exception_path() {
    let (mut st, bridge) = state_with_bridge(5, 17, "()I");
    bridge
        .instance_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::threw(0xCAFE)));

    let payload = build_or_invoke_payload(100, 1, 5, 17, &[], 0);
    let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);

    // returnValue = null object (tag L + id 0) since method threw.
    // Then excTag L + excId 8.
    assert_eq!(res.data.len(), 18);
    assert_eq!(res.data[0], b'L');
    let ret_id = u64::from_be_bytes([
        res.data[1],
        res.data[2],
        res.data[3],
        res.data[4],
        res.data[5],
        res.data[6],
        res.data[7],
        res.data[8],
    ]);
    assert_eq!(ret_id, 0); // null return
    assert_eq!(res.data[9], b'L');
    let exc = u64::from_be_bytes([
        res.data[10],
        res.data[11],
        res.data[12],
        res.data[13],
        res.data[14],
        res.data[15],
        res.data[16],
        res.data[17],
    ]);
    assert_eq!(exc, 0xCAFE);
}

#[test]
fn t65_or_invoke_method_invalid_thread() {
    let (mut st, bridge) = state_with_bridge(5, 17, "()V");
    bridge
        .instance_outcomes
        .lock()
        .unwrap()
        .push(Err(BridgeError::InvalidThread));

    let payload = build_or_invoke_payload(100, 0xDEAD, 5, 17, &[], 0);
    let res = dispatch(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_INVALID_THREAD);
    assert!(res.data.is_empty());
}

// --- ArrayType.NewInstance ---------------------------------------------

#[test]
fn t65_at_new_instance_happy_path() {
    let (mut st, bridge) = state_with_bridge(0, 0, "()V");
    bridge.array_outcomes.lock().unwrap().push(Ok(0x4242));

    let mut pw = PayloadWriter::new();
    pw.put_u64_be(77); // arrayTypeID
    pw.put_u32_be(16); // length
    let res = dispatch(
        CS_ARRAY_TYPE,
        CMD_AT_NEW_INSTANCE,
        &pw.into_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_NONE);

    // Reply: tag(1)=[ + id(8) = 9 bytes.
    assert_eq!(res.data.len(), 9);
    assert_eq!(res.data[0], b'[');
    let id = u64::from_be_bytes([
        res.data[1],
        res.data[2],
        res.data[3],
        res.data[4],
        res.data[5],
        res.data[6],
        res.data[7],
        res.data[8],
    ]);
    assert_eq!(id, 0x4242);

    let calls = bridge.calls.lock().unwrap();
    match &calls[0] {
        MockCall::NewArray {
            array_type_id,
            length,
        } => {
            assert_eq!(*array_type_id, 77);
            assert_eq!(*length, 16);
        }
        c => panic!("expected NewArray call, got {:?}", c),
    }
}

#[test]
fn t65_at_new_instance_invalid_class() {
    let (mut st, bridge) = state_with_bridge(0, 0, "()V");
    bridge
        .array_outcomes
        .lock()
        .unwrap()
        .push(Err(BridgeError::InvalidClass));

    let mut pw = PayloadWriter::new();
    pw.put_u64_be(0xDEAD);
    pw.put_u32_be(10);
    let res = dispatch(
        CS_ARRAY_TYPE,
        CMD_AT_NEW_INSTANCE,
        &pw.into_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_INVALID_CLASS);
    assert!(res.data.is_empty());
}

#[test]
fn t65_at_new_instance_no_bridge_vm_dead() {
    let mut st = DebugState::new(); // no bridge installed
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(77);
    pw.put_u32_be(16);
    let res = dispatch(
        CS_ARRAY_TYPE,
        CMD_AT_NEW_INSTANCE,
        &pw.into_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_VM_DEAD);
}

/// Interpreter round i1 wave 43: a negative length is `OUT_OF_MEMORY`,
/// as HotSpot answers it, and never reaches the bridge.
#[test]
fn at_new_instance_of_a_negative_length_is_out_of_memory() {
    let (mut st, bridge) = state_with_bridge(0, 0, "()V");
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(77);
    pw.put_u32_be(u32::MAX); // -1
    let res = dispatch(
        CS_ARRAY_TYPE,
        CMD_AT_NEW_INSTANCE,
        &pw.into_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_OUT_OF_MEMORY);
    assert!(bridge.calls.lock().unwrap().is_empty());
}

// Wire-level round-trip for the three new handlers: serialises the full
// JDWP command, dispatches, and parses the reply bytes back through the
// decoder, confirming the full header/flags/length math is consistent.

#[test]
fn t65_wire_ct_invoke_method_roundtrip() {
    let (mut st, bridge) = state_with_bridge(10, 42, "(I)I");
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::returned(DebuggerValue::Int(99))));

    let payload = build_ct_invoke_payload(10, 1, 42, &[DebuggerValue::Int(1)], 0);
    let reply = wire_roundtrip(CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD, &payload, &mut st);
    match reply {
        JdwpPacket::Reply {
            error_code, data, ..
        } => {
            assert_eq!(error_code, 0);
            // Reply should decode tag=I, i32=99, excTag=L, excId=0.
            assert_eq!(data[0], b'I');
            let ret = i32::from_be_bytes([data[1], data[2], data[3], data[4]]);
            assert_eq!(ret, 99);
            assert_eq!(data[5], b'L');
        }
        _ => panic!("expected reply"),
    }
}

/// Wave 12: `StackFrame.GetValues` answers a `boolean` / `byte` /
/// `char` / `short` local with its own tag and width, as HotSpot does;
/// they went out as a four-byte `I`. An `int` local is unchanged.
#[test]
fn get_values_tags_narrow_locals_with_their_own_type() {
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_frames.insert(
        5,
        vec![crate::debug::FrameEntry {
            frame_id: 0,
            class_id: 4,
            method_id: 1,
            offset: 0,
        }],
    );
    st.suspend_thread(5);
    st.update_frame_locals(
        5,
        0,
        vec![
            crate::debug::LocalValue::Int(1),
            crate::debug::LocalValue::Int(-2),
            crate::debug::LocalValue::Int(0x41),
            crate::debug::LocalValue::Int(-3),
            crate::debug::LocalValue::Int(7),
        ],
    );
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(5); // thread
    pw.put_u64_be(0); // frame
    pw.put_u32_be(5);
    for (slot, sig) in [(0u32, b'Z'), (1, b'B'), (2, b'C'), (3, b'S'), (4, b'I')] {
        pw.put_u32_be(slot);
        pw.put_u8(sig);
    }
    let res = dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let mut expected = 5u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&[b'Z', 1]);
    expected.extend_from_slice(&[b'B', 0xFE]);
    expected.extend_from_slice(&[b'C', 0x00, 0x41]);
    expected.extend_from_slice(&[b'S', 0xFF, 0xFD]);
    expected.extend_from_slice(&[b'I', 0, 0, 0, 7]);
    assert_eq!(res.data, expected);
}

/// Wave 14: `StackFrame.GetValues` / `ThisObject` check the thread and
/// the frame as HotSpot's back end does (`validateThreadFrame`):
/// `INVALID_THREAD`, `THREAD_NOT_SUSPENDED`, `INVALID_FRAMEID` for an id
/// the thread's published frames do not hold, and `INVALID_SLOT` for a
/// slot outside the frame. They answered typed zeros / a null `this`.
#[test]
fn stack_frame_reads_check_the_thread_the_frame_and_the_slot() {
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_frames.insert(
        5,
        vec![crate::debug::FrameEntry {
            frame_id: 0,
            class_id: 4,
            method_id: 1,
            offset: 0,
        }],
    );
    st.update_frame_locals(5, 0, vec![crate::debug::LocalValue::Int(7)]);
    let get = |st: &mut DebugState, tid: u64, frame: u64, slot: u32| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(tid);
        pw.put_u64_be(frame);
        pw.put_u32_be(1);
        pw.put_u32_be(slot);
        pw.put_u8(b'I');
        dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), st).error_code
    };
    let this_of = |st: &mut DebugState, tid: u64, frame: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(tid);
        pw.put_u64_be(frame);
        dispatch(CS_STACK_FRAME, CMD_SF_THIS_OBJECT, &pw.into_bytes(), st).error_code
    };
    assert_eq!(get(&mut st, 99, 0, 0), ERR_INVALID_THREAD);
    assert_eq!(get(&mut st, 5, 0, 0), ERR_THREAD_NOT_SUSPENDED);
    assert_eq!(this_of(&mut st, 5, 0), ERR_THREAD_NOT_SUSPENDED);
    st.suspend_thread(5);
    assert_eq!(get(&mut st, 5, 0, 0), ERR_NONE);
    assert_eq!(get(&mut st, 5, 0, 1), ERR_INVALID_SLOT);
    assert_eq!(get(&mut st, 5, 1, 0), ERR_INVALID_FRAMEID);
    assert_eq!(this_of(&mut st, 5, 1), ERR_INVALID_FRAMEID);
    assert_eq!(this_of(&mut st, 5, 0), ERR_NONE);
}

/// Interpreter round i6 wave 119, lane L1: `ThisObject` of a native
/// frame -- the listing's head, or a JNI native spliced in the middle and
/// so also an opaque row -- is a null `this` with no error, as HotSpot
/// answers it (`L1W119RawJdwpNativeThisObject`); the opaque Java row
/// below it keeps `OPAQUE_FRAME`.
#[test]
fn this_object_of_a_native_frame_is_null() {
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_frames.insert(
        5,
        vec![
            crate::debug::FrameEntry {
                frame_id: 0,
                class_id: 4,
                method_id: 1,
                offset: crate::debug::NATIVE_FRAME_LOCATION,
            },
            crate::debug::FrameEntry {
                frame_id: 1,
                class_id: 4,
                method_id: 2,
                offset: 3,
            },
        ],
    );
    st.opaque_frames.insert((5, 0), true);
    st.opaque_frames.insert((5, 1), true);
    st.suspend_thread(5);
    let this_of = |st: &mut DebugState, frame: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(5);
        pw.put_u64_be(frame);
        dispatch(CS_STACK_FRAME, CMD_SF_THIS_OBJECT, &pw.into_bytes(), st)
    };
    let native = this_of(&mut st, 0);
    assert_eq!(native.error_code, ERR_NONE);
    assert_eq!(native.data, [b'L', 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(this_of(&mut st, 1).error_code, ERR_OPAQUE_FRAME);
}

/// Wave 28: a frame id from before a resume is refused with
/// `INVALID_FRAMEID` after the next suspension, as HotSpot refuses it
/// (its ids carry the thread's `frameGeneration`); it used to name the
/// frame that stood at the same position then. Within one suspension
/// the ids hold still, including across a `VirtualMachine.Resume` that
/// leaves the thread suspended.
#[test]
fn a_frame_id_from_before_a_resume_is_refused() {
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_names.insert(6, "other".into());
    // What `interpreter::publish_frame_snapshot` publishes: one frame
    // per position, its id minted by `DebugState::frame_id`.
    let publish = |st: &mut DebugState, tid: u64, value: i32| -> u64 {
        let id = st.frame_id(tid, 0);
        st.thread_frames.remove(&tid);
        st.frame_locals.retain(|&(t, _), _| t != tid);
        st.thread_frames.insert(
            tid,
            vec![crate::debug::FrameEntry {
                frame_id: id,
                class_id: 4,
                method_id: 1,
                offset: 0,
            }],
        );
        st.update_frame_locals(tid, id, vec![crate::debug::LocalValue::Int(value)]);
        id
    };
    let get = |st: &mut DebugState, tid: u64, frame: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(tid);
        pw.put_u64_be(frame);
        pw.put_u32_be(1);
        pw.put_u32_be(0);
        pw.put_u8(b'I');
        let res = dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), st);
        (res.error_code == ERR_NONE)
            .then(|| i32::from_be_bytes([res.data[5], res.data[6], res.data[7], res.data[8]]))
            .ok_or(res.error_code)
    };

    // A first suspension: the bare positions, as before wave 28.
    st.suspend_thread(5);
    let first = publish(&mut st, 5, 1);
    assert_eq!(first, 0);
    assert_eq!(get(&mut st, 5, first), Ok(1));
    // A republication within the suspension keeps the id.
    assert_eq!(publish(&mut st, 5, 1), first);

    // Resumed, then suspended in another frame: the old id is stale.
    st.resume_thread(5);
    st.suspend_thread(5);
    let second = publish(&mut st, 5, 2);
    assert_ne!(second, first);
    assert_eq!(get(&mut st, 5, first), Err(ERR_INVALID_FRAMEID));
    assert_eq!(get(&mut st, 5, second), Ok(2));

    // Suspended twice (on its own and by the VM): one resume leaves it
    // suspended, and its ids stand.
    st.suspend_all();
    st.resume_vm();
    assert!(st.is_thread_suspended(5));
    assert_eq!(publish(&mut st, 5, 2), second);
    // Thread 6 (suspended only by the VM) moves with the VM resume.
    st.suspend_all();
    let other = publish(&mut st, 6, 6);
    assert_eq!(st.frame_id(5, 0), second, "5 stays suspended");
    st.resume_vm();
    assert!(!st.is_thread_suspended(6));
    assert_eq!(st.frame_id(5, 0), second, "5 is still suspended");
    st.suspend_all();
    let other_again = publish(&mut st, 6, 7);
    assert_ne!(other_again, other);
    assert_eq!(get(&mut st, 6, other), Err(ERR_INVALID_FRAMEID));
    assert_eq!(get(&mut st, 6, other_again), Ok(7));

    // `ThreadReference.Resume` taking 5 to zero while a VM-wide
    // suspension stands: its generation moves.
    st.resume_thread(5); // count 2 -> 1: still suspended
    assert!(st.is_thread_suspended(5));
    assert_eq!(st.frame_id(5, 0), second);
    st.resume_thread(5); // count 1 -> 0: resumed
    assert!(!st.is_thread_suspended(5));
    st.suspend_thread(5);
    let third = publish(&mut st, 5, 3);
    assert_ne!(third, second);
    assert_eq!(get(&mut st, 5, second), Err(ERR_INVALID_FRAMEID));

    // `VirtualMachine.Resume` taking 5 to zero while a VM-wide count
    // stands after it (5 resumed once on its own): its generation moves.
    st.suspend_all(); // 5: count 2
    st.resume_thread(5); // count 1
    st.resume_vm(); // count 0, the VM-wide count still 1
    assert!(!st.is_thread_suspended(5));
    st.suspend_thread(5);
    let fourth = publish(&mut st, 5, 4);
    assert_ne!(fourth, third);
    assert_eq!(get(&mut st, 5, third), Err(ERR_INVALID_FRAMEID));
    assert_eq!(get(&mut st, 5, fourth), Ok(4));

    // A detach resumes everything.
    st.resume_all();
    st.suspend_all();
    assert_ne!(publish(&mut st, 5, 5), fourth);
    assert_eq!(get(&mut st, 5, fourth), Err(ERR_INVALID_FRAMEID));
}

/// Wave 15: `ThreadReference.Resume` and `SuspendCount` answer
/// `INVALID_THREAD` for an id that names no thread, as the other
/// `ThreadReference` commands do (they answered success and 0); a dead
/// thread is neither resumed nor counted.
#[test]
fn resume_and_suspend_count_check_the_thread() {
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_names.insert(6, "gone".into());
    st.thread_statuses.insert(6, THREAD_STATUS_ZOMBIE);
    let count = |st: &mut DebugState, tid: u64| {
        let res = dispatch(CS_THREAD_REF, CMD_TR_SUSPEND_COUNT, &tid.to_be_bytes(), st);
        let n = res
            .data
            .get(..4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        (res.error_code, n)
    };
    let resume = |st: &mut DebugState, tid: u64| {
        dispatch(CS_THREAD_REF, CMD_TR_RESUME, &tid.to_be_bytes(), st).error_code
    };
    assert_eq!(count(&mut st, 99), (ERR_INVALID_THREAD, None));
    assert_eq!(resume(&mut st, 99), ERR_INVALID_THREAD);
    st.suspend_thread(5);
    assert_eq!(count(&mut st, 5), (ERR_NONE, Some(1)));
    assert_eq!(resume(&mut st, 5), ERR_NONE);
    assert_eq!(count(&mut st, 5), (ERR_NONE, Some(0)));
    assert_eq!(resume(&mut st, 6), ERR_NONE, "a dead thread is left alone");
    assert_eq!(count(&mut st, 6), (ERR_NONE, Some(0)));
    // Wave 29: a dead thread's own suspension is counted and resumed.
    assert_eq!(
        dispatch(CS_THREAD_REF, CMD_TR_SUSPEND, &6u64.to_be_bytes(), &mut st).error_code,
        ERR_NONE
    );
    assert_eq!(count(&mut st, 6), (ERR_NONE, Some(1)));
    assert!(!st.any_suspension_of_a_live_thread());
    assert_eq!(resume(&mut st, 6), ERR_NONE);
    assert_eq!(count(&mut st, 6), (ERR_NONE, Some(0)));
    // A VM-wide suspension covers every thread, but a dead one reports
    // no count, as `Status` reports it not suspended.
    st.suspend_all();
    assert_eq!(count(&mut st, 6), (ERR_NONE, Some(0)));
    assert_eq!(count(&mut st, 5), (ERR_NONE, Some(1)));
}

/// Wave 15: `StackFrame.GetValues` answers `TYPE_MISMATCH` when the type
/// asked for is not the variable's, as HotSpot's `GetLocal*` does:
/// against the `LocalVariableTable` entry covering the slot at the
/// frame's location, else (no entry) only reference against primitive.
/// It answered a typed zero or null.
#[test]
fn get_values_answers_a_type_mismatch() {
    use crate::debug::{FrameEntry, LocalValue, VariableInfo};
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_frames.insert(
        5,
        vec![FrameEntry {
            frame_id: 0,
            class_id: 4,
            method_id: 1,
            offset: 10,
        }],
    );
    st.suspend_thread(5);
    st.update_frame_locals(
        5,
        0,
        vec![
            LocalValue::Int(7),
            LocalValue::Long(1 << 40),
            LocalValue::Long(0),
            LocalValue::ObjectRef(0),
            LocalValue::Float(1.5),
        ],
    );
    let var = |slot: usize, signature: &str, code_index: u64, length: usize| VariableInfo {
        code_index,
        name: format!("v{slot}"),
        signature: signature.to_string(),
        length,
        slot,
    };
    // Slot 0 an `int`, slot 1 a `long`, slot 3 a `String`; slot 4's entry
    // ends at the frame's location (10), which (wave 100) covers it.
    st.method_variables.insert(
        (4, 1),
        vec![
            var(0, "I", 0, 20),
            var(1, "J", 0, 20),
            var(3, "Ljava/lang/String;", 5, 10),
            var(4, "Ljava/lang/Object;", 0, 10),
        ],
    );
    let get = |st: &mut DebugState, slot: u32, sig: u8| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(5);
        pw.put_u64_be(0);
        pw.put_u32_be(1);
        pw.put_u32_be(slot);
        pw.put_u8(sig);
        dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), st)
    };
    assert_eq!(get(&mut st, 0, b'I').error_code, ERR_NONE);
    assert_eq!(get(&mut st, 0, b'Z').error_code, ERR_NONE, "the int family");
    assert_eq!(get(&mut st, 0, b'J').error_code, ERR_TYPE_MISMATCH);
    assert_eq!(get(&mut st, 0, b'L').error_code, ERR_TYPE_MISMATCH);
    let long = get(&mut st, 1, b'J');
    assert_eq!(long.error_code, ERR_NONE);
    let mut want = 1u32.to_be_bytes().to_vec();
    want.push(b'J');
    want.extend_from_slice(&(1u64 << 40).to_be_bytes());
    assert_eq!(long.data, want, "a long local's value");
    assert_eq!(get(&mut st, 1, b'D').error_code, ERR_TYPE_MISMATCH);
    assert_eq!(get(&mut st, 3, b'L').error_code, ERR_NONE, "a null String");
    assert_eq!(
        get(&mut st, 3, b'[').error_code,
        ERR_NONE,
        "both references"
    );
    assert_eq!(get(&mut st, 3, b'I').error_code, ERR_TYPE_MISMATCH);
    // Slot 4's entry ends at the location: since wave 100 (lane L1) it
    // covers the slot (HotSpot's inclusive end), so its `Object` kind is
    // checked; the frame's float mismatches it either way.
    assert_eq!(get(&mut st, 4, b'F').error_code, ERR_TYPE_MISMATCH);
    assert_eq!(get(&mut st, 4, b'I').error_code, ERR_TYPE_MISMATCH);
    assert_eq!(get(&mut st, 4, b'L').error_code, ERR_TYPE_MISMATCH);
    // Wave 29 (HotSpot 25.0.3, `L1W29RawJdwpThreadAndFrameErrors`): every
    // object tag reads a reference slot, and mismatches a primitive one;
    // a tag that names no type is `INVALID_TAG`.
    for tag in *b"stglc" {
        let res = get(&mut st, 3, tag);
        assert_eq!(res.error_code, ERR_NONE, "tag {}", char::from(tag));
        assert_eq!(res.data[4], b'L', "a null reference, not void");
        assert_eq!(get(&mut st, 0, tag).error_code, ERR_TYPE_MISMATCH);
    }
    for tag in [b'V', 0, b'X'] {
        assert_eq!(get(&mut st, 0, tag).error_code, ERR_INVALID_TAG);
    }
    st.method_variables.clear();
    assert_eq!(get(&mut st, 3, b'I').error_code, ERR_NONE, "null: unknown");
    assert_eq!(get(&mut st, 0, b'L').error_code, ERR_TYPE_MISMATCH);
}

/// Wave 42 (HotSpot 25.0.3, `L1W42RawJdwpErrorAnswers`): the frame
/// first — a `long` whose second slot holds a reference is
/// `INVALID_SLOT`, reference against primitive `TYPE_MISMATCH` — then the
/// `LocalVariableTable`, where a slot no entry covers is `INVALID_SLOT`.
/// `probe(int x, String s)` at bytecode 0, `t` (slot 2) not yet in scope.
#[test]
fn get_values_checks_the_frame_then_the_variable_table() {
    use crate::debug::{FrameEntry, LocalValue, VariableInfo};
    let mut st = fresh_state();
    st.thread_names.insert(5, "worker".into());
    st.thread_frames.insert(
        5,
        vec![FrameEntry {
            frame_id: 0,
            class_id: 4,
            method_id: 1,
            offset: 0,
        }],
    );
    st.suspend_thread(5);
    st.update_frame_locals(
        5,
        0,
        vec![
            LocalValue::Int(3),
            LocalValue::ObjectRef(77),
            LocalValue::Int(0),
        ],
    );
    let var = |slot: usize, signature: &str, code_index: u64, length: usize| VariableInfo {
        code_index,
        name: format!("v{slot}"),
        signature: signature.to_string(),
        length,
        slot,
    };
    st.method_variables.insert(
        (4, 1),
        vec![
            var(0, "I", 0, 12),
            var(1, "Ljava/lang/String;", 0, 12),
            var(2, "Ljava/lang/String;", 8, 4),
        ],
    );
    let get = |st: &mut DebugState, slot: u32, sig: u8| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(5);
        pw.put_u64_be(0);
        pw.put_u32_be(1);
        pw.put_u32_be(slot);
        pw.put_u8(sig);
        dispatch(CS_STACK_FRAME, CMD_SF_GET_VALUES, &pw.into_bytes(), st).error_code
    };
    assert_eq!(get(&mut st, 0, b'I'), ERR_NONE);
    assert_eq!(
        get(&mut st, 0, b'J'),
        ERR_INVALID_SLOT,
        "a String second slot"
    );
    assert_eq!(get(&mut st, 0, b'D'), ERR_INVALID_SLOT);
    assert_eq!(get(&mut st, 1, b'I'), ERR_TYPE_MISMATCH);
    assert_eq!(get(&mut st, 1, b'J'), ERR_TYPE_MISMATCH);
    assert_eq!(
        get(&mut st, 2, b'L'),
        ERR_TYPE_MISMATCH,
        "an unwritten slot is no reference"
    );
    assert_eq!(get(&mut st, 2, b'I'), ERR_INVALID_SLOT, "not yet in scope");
    assert_eq!(get(&mut st, 9, b'I'), ERR_INVALID_SLOT);
    assert_eq!(
        get(&mut st, 2, b'J'),
        ERR_INVALID_SLOT,
        "a long past the frame"
    );
}

/// Wave 15: `ClassesBySignature` and `AllClassesWithGeneric` answer the
/// status the server refreshed (`DebugState::class_statuses`); a class
/// with none answers VERIFIED | PREPARED, as every class did. Two classes
/// of one name (two loaders) are both found; one per name was kept.
#[test]
fn class_queries_answer_the_refreshed_status() {
    let mut st = fresh_state();
    for (id, sig) in [(4, "LA;"), (5, "LB;"), (6, "LA;")] {
        st.class_signatures.insert(id, sig.to_string());
        st.class_type_tags.insert(id, 1);
    }
    st.class_statuses.insert(4, 7);
    let by_sig = |st: &mut DebugState, sig: &str| {
        let mut pw = PayloadWriter::new();
        pw.put_string(sig);
        let res = dispatch(CS_VM, CMD_VM_CLASSES_BY_SIGNATURE, &pw.into_bytes(), st);
        assert_eq!(res.error_code, ERR_NONE);
        let mut r = PayloadReader::new(&res.data);
        let mut found = Vec::new();
        for _ in 0..r.read_u32_be().expect("count") {
            let _tag = r.read_u8().expect("tag");
            let id = r.read_u64_be().expect("id");
            found.push((id, r.read_u32_be().expect("status")));
        }
        found
    };
    assert_eq!(by_sig(&mut st, "LA;"), vec![(4, 7), (6, 3)]);
    assert_eq!(by_sig(&mut st, "LB;"), vec![(5, 3)]);
    assert!(by_sig(&mut st, "LC;").is_empty());
    let res = dispatch(CS_VM, CMD_VM_ALL_CLASSES_WITH_GENERIC, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let mut r = PayloadReader::new(&res.data);
    let mut statuses = Vec::new();
    for _ in 0..r.read_u32_be().expect("count") {
        let _tag = r.read_u8().expect("tag");
        let id = r.read_u64_be().expect("id");
        let _sig = r.read_string().expect("signature");
        let _generic = r.read_string().expect("generic signature");
        statuses.push((id, r.read_u32_be().expect("status")));
    }
    assert_eq!(statuses, vec![(4, 7), (5, 3), (6, 3)], "in id order");
}

/// Wave 15: a frame's location carries its class's own type tag — an
/// interface's static or default method is `INTERFACE` (2) — where every
/// location was `CLASS` (1); a class the session does not know stays 1.
#[test]
fn a_frame_location_carries_its_class_type_tag() {
    let mut st = fresh_state();
    st.class_type_tags.insert(4, 2);
    st.class_signatures.insert(4, "LI;".to_string());
    st.thread_names.insert(5, "worker".into());
    st.thread_frames.insert(
        5,
        vec![
            crate::debug::FrameEntry {
                frame_id: 0,
                class_id: 4,
                method_id: 1,
                offset: 0,
            },
            crate::debug::FrameEntry {
                frame_id: 1,
                class_id: 9,
                method_id: 1,
                offset: 3,
            },
        ],
    );
    st.suspend_thread(5);
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(5);
    pw.put_u32_be(0);
    pw.put_u32_be(u32::MAX); // -1: all
    let res = dispatch(CS_THREAD_REF, CMD_TR_FRAMES, &pw.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    // count(4), then per frame: id(8) tag(1) class(8) method(8) index(8).
    assert_eq!(res.data[12], 2, "an interface method's location");
    assert_eq!(res.data[12 + 33], 1, "an unknown class stays CLASS");
    assert_eq!(st.location_type_tag(4), 2);
}

/// Interpreter round i1 wave 19, lane L3: `Method.IsObsolete` answers
/// true for the null method id an obsolete frame lists
/// (`debug::frame_method_id`), false for an id naming a current method,
/// where it answered false always; a short payload is refused.
#[test]
fn is_obsolete_answers_for_the_null_method_id_only() {
    let mut st = fresh_state();
    let ask = |st: &mut DebugState, method_id: u64| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(4);
        pw.put_u64_be(method_id);
        dispatch(CS_METHOD, CMD_M_IS_OBSOLETE, &pw.into_bytes(), st)
    };
    let obsolete = ask(&mut st, crate::debug::OBSOLETE_METHOD_ID);
    assert_eq!(obsolete.error_code, ERR_NONE);
    assert_eq!(obsolete.data, vec![1]);
    let current = ask(&mut st, crate::debug::jdwp_method_id("run", "()V"));
    assert_eq!(current.error_code, ERR_NONE);
    assert_eq!(current.data, vec![0]);
    let short = dispatch(CS_METHOD, CMD_M_IS_OBSOLETE, &[0; 8], &mut st);
    assert_eq!(short.error_code, ERR_INTERNAL);
}

/// Interpreter round i1 wave 23: `HoldEvents` / `ReleaseEvents` toggle
/// the hold the JDWP server loop honours, and `SetDefaultStratum` is
/// accepted; all three answered `NOT_IMPLEMENTED`.
#[test]
fn hold_release_events_and_default_stratum_are_served() {
    let mut st = fresh_state();
    assert!(!st.events_held);
    let res = dispatch(CS_VM, CMD_VM_HOLD_EVENTS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert!(st.events_held);
    let res = dispatch(CS_VM, CMD_VM_RELEASE_EVENTS, &[], &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert!(!st.events_held);
    let mut pw = PayloadWriter::new();
    pw.put_string("Java");
    let res = dispatch(CS_VM, CMD_VM_SET_DEFAULT_STRATUM, &pw.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let res = dispatch(CS_VM, CMD_VM_SET_DEFAULT_STRATUM, &[0, 0], &mut st);
    assert_eq!(res.error_code, ERR_INTERNAL, "truncated string");
}

/// Interpreter round i1 wave 23: a `SourceNameMatch` modifier (modKind
/// 12) is accepted on a `ClassPrepare` request and refused elsewhere, as
/// HotSpot does; and the `ReferenceType` commands answer `INVALID_CLASS`
/// for an id naming no class (`Fields` / `Methods` answered an empty
/// list), `ABSENT_INFORMATION` for a class without a source file, and
/// serve the `*WithGeneric` forms JDI sends.
#[test]
fn source_name_filters_and_reference_type_refusals() {
    let mut st = fresh_state();
    let request = |kind: EventKind| {
        let mut body = PayloadWriter::new();
        body.put_u8(kind as u8);
        body.put_u8(SuspendPolicy::None as u8);
        body.put_u32_be(1);
        body.put_u8(12); // SourceNameMatch
        body.put_string("*.kt");
        body.into_bytes()
    };
    let res = dispatch(
        CS_EVENT_REQUEST,
        CMD_ER_SET,
        &request(EventKind::ClassPrepare),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_NONE);
    let id = u32::from_be_bytes([res.data[0], res.data[1], res.data[2], res.data[3]]);
    assert_eq!(
        st.events.get_request(id).map(|r| r.modifiers.clone()),
        Some(vec![EventModifier::SourceNameMatch {
            pattern: "*.kt".to_string()
        }])
    );
    let res = dispatch(
        CS_EVENT_REQUEST,
        CMD_ER_SET,
        &request(EventKind::Breakpoint),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_ILLEGAL_ARGUMENT);

    st.class_signatures.insert(4, "LA;".to_string());
    st.class_methods.insert(
        4,
        vec![MethodInfo {
            method_id: MethodId(9),
            name: "m".to_string(),
            signature: "()V".to_string(),
            mod_bits: 0x0001,
        }],
    );
    let unknown = 77u64.to_be_bytes();
    for cmd in [
        CMD_RT_FIELDS,
        CMD_RT_METHODS,
        CMD_RT_FIELDS_WITH_GENERIC,
        CMD_RT_METHODS_WITH_GENERIC,
        CMD_RT_SIGNATURE_WITH_GENERIC,
        CMD_RT_INTERFACES,
        CMD_RT_STATUS,
    ] {
        let res = dispatch(CS_REF_TYPE, cmd, &unknown, &mut st);
        assert_eq!(res.error_code, ERR_INVALID_CLASS, "2/{cmd}");
    }
    let known = 4u64.to_be_bytes();
    let res = dispatch(CS_REF_TYPE, CMD_RT_SOURCE_FILE, &known, &mut st);
    assert_eq!(res.error_code, ERR_ABSENT_INFORMATION);
    let res = dispatch(CS_REF_TYPE, CMD_RT_METHODS_WITH_GENERIC, &known, &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let mut r = PayloadReader::new(&res.data);
    assert_eq!(r.read_u32_be().expect("count"), 1);
    assert_eq!(r.read_u64_be().expect("id"), 9);
    assert_eq!(r.read_string().expect("name"), "m");
    assert_eq!(r.read_string().expect("signature"), "()V");
    assert_eq!(r.read_string().expect("generic"), "", "none known");
    assert_eq!(r.read_u32_be().expect("modifiers"), 0x0001);
    let res = dispatch(CS_REF_TYPE, CMD_RT_SIGNATURE_WITH_GENERIC, &known, &mut st);
    let mut r = PayloadReader::new(&res.data);
    assert_eq!(r.read_string().expect("signature"), "LA;");
    assert_eq!(r.read_string().expect("generic"), "");
    // A method the class does not declare; a class without a variable
    // table.
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(4);
    pw.put_u64_be(10);
    let res = dispatch(CS_METHOD, CMD_M_LINE_TABLE, &pw.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_INVALID_METHODID);
    let mut pw = PayloadWriter::new();
    pw.put_u64_be(4);
    pw.put_u64_be(9);
    let res = dispatch(CS_METHOD, CMD_M_VARIABLE_TABLE, &pw.into_bytes(), &mut st);
    assert_eq!(res.error_code, ERR_ABSENT_INFORMATION);
}

/// Interpreter round i1 wave 24: `ClassType.NewInstance` runs through
/// the bridge as an invocation (answered after it returns) and replies
/// `newObject` then `exception`, one of them null, as HotSpot does;
/// `InterfaceType.InvokeMethod` has `ClassType.InvokeMethod`'s layout and
/// runs as a static invocation. Both answered `NOT_IMPLEMENTED`.
#[test]
fn w24_new_instance_and_interface_invoke_run_through_the_bridge() {
    let (mut st, bridge) = state_with_bridge(10, 42, "(I)V");
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::returned(DebuggerValue::Object(0x55))));
    let payload = build_ct_invoke_payload(10, 1, 42, &[DebuggerValue::Int(7)], 1);
    let prepared = prepare_bridge_command(CS_CLASS_TYPE, CMD_CT_NEW_INSTANCE, &payload, &st);
    assert!(matches!(prepared, Some(Ok(ref call)) if call.runs_java()));
    let res = dispatch(CS_CLASS_TYPE, CMD_CT_NEW_INSTANCE, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    let mut want = vec![b'L'];
    want.extend_from_slice(&0x55u64.to_be_bytes());
    want.push(b'L');
    want.extend_from_slice(&0u64.to_be_bytes());
    assert_eq!(res.data, want, "newObject L 0x55, exception L null");
    {
        let calls = bridge.calls.lock().unwrap();
        match &calls[0] {
            MockCall::NewInstance {
                class_id,
                method_id,
                thread_id,
                args,
            } => {
                assert_eq!((*class_id, *method_id, *thread_id), (10, 42, 1));
                assert_eq!(args, &vec![DebuggerValue::Int(7)]);
            }
            c => panic!("expected NewInstance call, got {:?}", c),
        }
    }
    assert_eq!(*bridge.single_threaded.lock().unwrap(), vec![true]);
    // A constructor that throws: null object, then the exception.
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::threw(0x77)));
    let res = dispatch(CS_CLASS_TYPE, CMD_CT_NEW_INSTANCE, &payload, &mut st);
    let mut want = vec![b'L'];
    want.extend_from_slice(&0u64.to_be_bytes());
    want.push(b'L');
    want.extend_from_slice(&0x77u64.to_be_bytes());
    assert_eq!(res.data, want, "newObject L null, exception L 0x77");

    // `InterfaceType.InvokeMethod`: a static call with the return type.
    let (mut st, bridge) = state_with_bridge(11, 43, "(I)I");
    bridge
        .static_outcomes
        .lock()
        .unwrap()
        .push(Ok(InvokeOutcome::returned(DebuggerValue::Int(7))));
    let payload = build_ct_invoke_payload(11, 1, 43, &[DebuggerValue::Int(4)], 0);
    let prepared =
        prepare_bridge_command(CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD, &payload, &st);
    assert!(matches!(prepared, Some(Ok(ref call)) if call.runs_java()));
    let res = dispatch(CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD, &payload, &mut st);
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(&res.data[..5], &[b'I', 0, 0, 0, 7]);
    let calls = bridge.calls.lock().unwrap();
    assert!(
        matches!(&calls[0], MockCall::Static { class_id: 11, return_sig, .. } if return_sig == "I"),
        "{:?}",
        calls[0]
    );
}

/// Wave 24: the bare one-group model answers `system` for its own id
/// only; every other id is `INVALID_THREAD_GROUP` (it answered `system`
/// for any id).
#[test]
fn w24_thread_group_name_refuses_an_unknown_group() {
    let mut st = fresh_state();
    let res = dispatch(
        CS_THREAD_GROUP_REF,
        CMD_TGR_NAME,
        &SYSTEM_THREAD_GROUP_ID.to_be_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_NONE);
    assert_eq!(
        PayloadReader::new(&res.data).read_string().expect("name"),
        "system"
    );
    let res = dispatch(
        CS_THREAD_GROUP_REF,
        CMD_TGR_NAME,
        &7u64.to_be_bytes(),
        &mut st,
    );
    assert_eq!(res.error_code, ERR_INVALID_THREAD_GROUP);
}
