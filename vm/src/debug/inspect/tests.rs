use super::*;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the ordering only compares.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Wave 25: JVMTI `GetOwnedMonitorInfo`'s order for the
/// `L1W25JdiStopMonitors` owner — frame 2 (`LockC.enter`, a synchronized
/// method on C with a block on B), frame 1 (`ownerRun`, a block on A and
/// a re-entry of C), frame 0 holding nothing — given as
/// `frame_locked_monitors` lists it (frames innermost first, a frame's
/// newest block first, its method monitor last): C then B (the frame's
/// entry order), then A; the re-entered C is not listed again; a frame
/// entry the lock stack does not hold is dropped; an owned monitor no
/// frame names follows at -1.
#[test]
fn owned_monitors_follow_jvmti_order_and_depths() {
    let (a, b, c, stale, jni) = (obj(0x10), obj(0x20), obj(0x30), obj(0x40), obj(0x50));
    let pairs = vec![(2, b), (2, c), (1, c), (1, a), (1, stale)];
    let owned = [a, b, c, jni];
    // Frame index (from the bottom) -> JDWP depth: two native / compiled
    // entries sit above frame 2.
    let depth_of = |index: usize| i32::try_from(4 - index).ok();
    let got = jvmti_owned_monitor_order(&owned, pairs, depth_of);
    assert_eq!(got, vec![(c, 2), (b, 2), (a, 3), (jni, -1)]);
    // No attribution: the lock stack's order, every one at -1.
    let got = jvmti_owned_monitor_order(&owned, Vec::new(), depth_of);
    assert_eq!(got, vec![(a, -1), (b, -1), (c, -1), (jni, -1)]);
}

/// Interpreter round i5 wave 102, lane L1: frames align to a listing
/// with extra rows by method id from the bottom; the positional map
/// needs equal counts.
#[test]
fn w102_l1_owned_monitor_frames_align_to_a_longer_listing() {
    // Thread frames bottom first: run(1), ownerRun(2), enter(3),
    // sleepQuietly(4). The listing, top first: Thread.sleep(9) served
    // (depth 1, the native sleep0 at depth 0 is not a row here),
    // sleepQuietly(4) depth 2, enter(3) 3, ownerRun(2) 4, run(1) 5.
    let rows = vec![(1, 9), (2, 4), (3, 3), (4, 2), (5, 1)];
    let got = align_frames_to_listing(&[1, 2, 3, 4], &rows);
    assert_eq!(got, vec![Some(5), Some(4), Some(3), Some(2)]);
    // A frame no row names stays unattributed and consumes nothing.
    let got = align_frames_to_listing(&[1, 7, 2], &rows);
    assert_eq!(got, vec![Some(5), None, Some(4)]);
    assert_eq!(position_depths(2, &[0, 3]), vec![Some(3), Some(0)]);
    assert!(position_depths(3, &[0, 3]).is_empty());
    // Wave 102 hand-back: a window listing names its frames. Top first:
    // the native head, a served level, then frames 2, 1, 0.
    let rows = [None, None, Some(2), Some(1), Some(0)];
    assert_eq!(window_depths(&rows), vec![Some(4), Some(3), Some(2)]);
    assert!(window_depths(&[None]).is_empty());
}

/// Wave 25: the monitor commands are heap commands, and the capabilities
/// JDI checks before sending them are granted.
#[test]
fn monitor_commands_are_served_with_the_heap() {
    use crate::debug::commands::{
        CMD_TR_CURRENT_CONTENDED_MONITOR, CMD_TR_OWNED_MONITORS,
        CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO,
    };
    for cmd in [
        CMD_TR_OWNED_MONITORS,
        CMD_TR_CURRENT_CONTENDED_MONITOR,
        CMD_TR_OWNED_MONITORS_STACK_DEPTH_INFO,
    ] {
        assert!(is_monitor_command(CS_THREAD_REF, cmd));
        assert!(served_with_the_heap(CS_THREAD_REF, cmd));
    }
    assert!(!is_monitor_command(CS_THREAD_REF, CMD_TR_STOP));
    // The JDWP 9 module commands, too.
    use crate::debug::commands::{
        CMD_MR_CLASS_LOADER, CMD_MR_NAME, CMD_RT_MODULE, CMD_VM_ALL_MODULES, CS_MODULE_REF,
    };
    for (set, cmd) in [
        (CS_VM, CMD_VM_ALL_MODULES),
        (CS_REF_TYPE, CMD_RT_MODULE),
        (CS_MODULE_REF, CMD_MR_NAME),
        (CS_MODULE_REF, CMD_MR_CLASS_LOADER),
    ] {
        assert!(is_module_command(set, cmd));
        assert!(served_with_the_heap(set, cmd));
    }
}

/// Wave 8: the heap commands are recognised as such (and only they), the
/// region check follows JDWP's `INVALID_INDEX` / `INVALID_LENGTH`, and a
/// field id round-trips through its class and index.
#[test]
fn heap_commands_regions_and_field_ids() {
    assert!(is_heap_command(CS_OBJECT_REF, CMD_OR_GET_VALUES));
    assert!(is_heap_command(CS_VM, CMD_VM_CREATE_STRING));
    assert!(is_heap_command(CS_ARRAY_REF, CMD_AR_SET_VALUES));
    assert!(!is_heap_command(
        CS_OBJECT_REF,
        super::super::commands::CMD_OR_INVOKE_METHOD
    ));
    assert!(!is_heap_command(
        CS_VM,
        super::super::commands::CMD_VM_VERSION
    ));
    // HotSpot's checks (wave 43): the first index must name an element.
    for rest in [false, true] {
        assert_eq!(region(0, 4, 4, rest), Ok((0, 4)));
        assert_eq!(region(3, 0, 4, rest), Ok((3, 0)));
        assert_eq!(region(4, 0, 4, rest), Err(ERR_INVALID_INDEX));
        assert_eq!(region(0, 0, 0, rest), Err(ERR_INVALID_INDEX), "empty array");
        assert_eq!(region(5, 0, 4, rest), Err(ERR_INVALID_INDEX));
        assert_eq!(region(1, 4, 4, rest), Err(ERR_INVALID_LENGTH));
        assert_eq!(region(u32::MAX, 1, 4, rest), Err(ERR_INVALID_INDEX), "-1");
        assert_eq!(
            region(0, u32::MAX - 1, 4, rest),
            Err(ERR_INVALID_LENGTH),
            "-2"
        );
    }
    // -1 is the rest of the array for `GetValues` only.
    assert_eq!(region(1, u32::MAX, 4, true), Ok((1, 3)));
    assert_eq!(region(0, u32::MAX, 4, false), Err(ERR_INVALID_LENGTH), "-1");
    let id = super::super::jdwp_field_id(ClassId::new(0x0123_4567), 9);
    assert_eq!(
        super::super::field_of_id(id),
        (ClassId::new(0x0123_4567), 9)
    );
    assert_ne!(
        super::super::jdwp_field_id(ClassId::new(1), 0),
        super::super::jdwp_field_id(ClassId::new(2), 0),
        "field 0 of two classes"
    );
}

/// Wave 84 (lane L1): HotSpot's `is_assignable` for a local written by a
/// debugger — a `String` fits a `String` or `Object` variable, not an
/// `Integer` or an array one.
#[test]
fn a_debugger_local_write_must_fit_the_declared_class() {
    assert_eq!(reference_name_of_signature("Ljava/lang/String;"), "java/lang/String");
    assert_eq!(reference_name_of_signature("[Ljava/lang/String;"), "[Ljava/lang/String;");
    assert_eq!(reference_name_of_signature("[I"), "[I");
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cm = shared.classes.class_manager.read();
    let Some(string) = cm.find_class_by_name_for_loader("java/lang/String", cratonvm_types::ClassLoaderId::Bootstrap) else {
        return; // a stripped test image
    };
    assert!(object_fits_local_signature(&cm, string, "Ljava/lang/String;"));
    assert!(object_fits_local_signature(&cm, string, "Ljava/lang/Object;"));
    assert!(!object_fits_local_signature(&cm, string, "[Ljava/lang/String;"));
    assert!(!object_fits_local_signature(&cm, string, "[I"));
    if cm.find_class_by_name_for_loader("java/lang/Integer", cratonvm_types::ClassLoaderId::Bootstrap).is_some() {
        assert!(!object_fits_local_signature(&cm, string, "Ljava/lang/Integer;"));
    }
}

/// Interpreter round i2 wave 61, lane L1: a C agent's `NotifyFramePop` of
/// a blocked thread waits for the thread's wake, once per frame and only
/// for the frame it was recorded against; a `ClearAllFramePops` drops the
/// thread's requests there.
#[test]
fn a_blocked_threads_frame_pop_requests_wait_for_its_wake() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::threading::ThreadId(77), "blocked");
    let frame = |name: &str| {
        crate::runtime::frame::Frame::new(
            ClassId::new(0),
            "T".to_string(),
            name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        )
    };
    thread.frames.push(frame("a"));
    thread.frames.push(frame("b"));
    let method_b = thread
        .frames
        .get(1)
        .map(super::super::frame_method_id)
        .unwrap();
    assert!(defer_jvmti_frame_pop(&shared, 77, 1, 2, 0, method_b));
    assert!(
        !defer_jvmti_frame_pop(&shared, 77, 1, 2, 0, method_b),
        "a duplicate"
    );
    // Recorded against a method frame 0 does not run: dropped at the wake.
    assert!(defer_jvmti_frame_pop(&shared, 77, 0, 2, 0, method_b ^ 1));
    assert!(shared.debug.debugger_gates.frame_writes_pending());
    apply_deferred_frame_writes_if_any(&shared, &mut thread);
    assert_eq!(thread.frame_pop_requests, vec![1]);
    assert!(!shared.debug.debugger_gates.frame_writes_pending());
    defer_jvmti_clear_frame_pops(&shared, 77);
    apply_deferred_frame_writes_if_any(&shared, &mut thread);
    assert!(thread.frame_pop_requests.is_empty());
}

/// Wave 8: reading and writing an object's fields and an array's
/// elements through the object table, on the quiesced server path (no
/// thread is parked in a unit test).
#[test]
fn object_fields_and_array_elements_round_trip() {
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let Some(ints) =
        shared
            .mem
            .heap
            .try_alloc_array_full(ClassId::new(0), ArrayElementType::Int, 3)
    else {
        return; // no room in a stripped test heap
    };
    if shared
        .mem
        .heap
        .is_object_address(ints.as_ptr() as usize) // Cast: address probe
        .is_none()
    {
        return; // a backend that cannot answer the liveness probe
    }
    let id = {
        let mut ds = shared.debug.debug_state.lock();
        super::super::export_object(&shared, &mut ds, ints, ObjectExport::Sent)
    };
    // ArrayReference.SetValues(id, 1, [7, -2]) then GetValues(id, 0, 3).
    let mut set = PayloadWriter::new();
    set.put_u64_be(id);
    set.put_u32_be(1);
    set.put_u32_be(2);
    set.put_u32_be(7);
    set.put_u32_be((-2i32) as u32); // Cast: wire encoding
    let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_SET_VALUES, &set.into_bytes());
    assert_eq!(res.error_code, 0);
    let mut get = PayloadWriter::new();
    get.put_u64_be(id);
    get.put_u32_be(0);
    get.put_u32_be(3);
    let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_GET_VALUES, &get.into_bytes());
    assert_eq!(res.error_code, 0);
    let mut want = vec![b'I', 0, 0, 0, 3];
    for v in [0i32, 7, -2] {
        want.extend_from_slice(&v.to_be_bytes());
    }
    assert_eq!(res.data, want, "tag, count, three untagged ints");
    let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_LENGTH, &id.to_be_bytes());
    assert_eq!(res.data, 3u32.to_be_bytes().to_vec());
    let mut past = PayloadWriter::new();
    past.put_u64_be(id);
    past.put_u32_be(2);
    past.put_u32_be(2);
    let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_GET_VALUES, &past.into_bytes());
    assert_eq!(res.error_code, ERR_INVALID_LENGTH);
    // An id the table never handed out.
    let res = run_heap_command(&shared, CS_ARRAY_REF, CMD_AR_LENGTH, &77u64.to_be_bytes());
    assert_eq!(res.error_code, ERR_INVALID_OBJECT);
    // Wave 43, as HotSpot answers: such an id is collected, enabling its
    // collection succeeds, disabling it is `INVALID_OBJECT`.
    let res = run_heap_command(
        &shared,
        CS_OBJECT_REF,
        CMD_OR_IS_COLLECTED,
        &77u64.to_be_bytes(),
    );
    assert_eq!((res.error_code, res.data), (0, vec![1]));
    let res = run_heap_command(
        &shared,
        CS_OBJECT_REF,
        CMD_OR_ENABLE_COLLECTION,
        &77u64.to_be_bytes(),
    );
    assert_eq!(res.error_code, 0);
    let res = run_heap_command(
        &shared,
        CS_OBJECT_REF,
        CMD_OR_DISABLE_COLLECTION,
        &77u64.to_be_bytes(),
    );
    assert_eq!(res.error_code, ERR_INVALID_OBJECT);
    // IsCollected on a live id; DisableCollection makes its handle strong.
    let res = run_heap_command(
        &shared,
        CS_OBJECT_REF,
        CMD_OR_IS_COLLECTED,
        &id.to_be_bytes(),
    );
    assert_eq!(res.data, vec![0]);
    let res = run_heap_command(
        &shared,
        CS_OBJECT_REF,
        CMD_OR_DISABLE_COLLECTION,
        &id.to_be_bytes(),
    );
    assert_eq!(res.error_code, 0);
    let handle = shared.debug.debug_state.lock().objects.handle_of(id);
    assert_eq!(
        handle.and_then(|h| shared.natives.jni_global_refs.lock().kind(h)),
        Some(crate::native::jni::JniGlobalKind::Strong)
    );
    let res = run_heap_command(
        &shared,
        CS_OBJECT_REF,
        CMD_OR_ENABLE_COLLECTION,
        &id.to_be_bytes(),
    );
    assert_eq!(res.error_code, 0);
    let mut ds = shared.debug.debug_state.lock();
    let handle = ds.objects.handle_of(id);
    assert_eq!(
        handle.and_then(|h| shared.natives.jni_global_refs.lock().kind(h)),
        Some(crate::native::jni::JniGlobalKind::Weak)
    );
    ds.objects.release_all();
    super::super::release_disposed_objects(&shared, &mut ds);
}
