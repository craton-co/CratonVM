use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

/// A scalar-replaced object placeholder: id `id`, class `class_id`,
/// `num_fields` default-`Undefined` fields.
fn vobj(id: usize, class_id: u32, num_fields: usize) -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id,
        class_id,
        num_fields,
        field_values: vec![FrameValue::Undefined; num_fields],
    })
}

fn frame_of(locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "T.m".to_string(),
        bci: 0,
        locals,
        stack,
        monitors: Vec::new(),
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

/// Flat slot accessor over locals-then-stack (test helper).
fn frame_slot(frame: &ReconstructedFrame, slot: usize) -> &FrameValue {
    if slot < frame.locals.len() {
        &frame.locals[slot]
    } else {
        &frame.stack[slot - frame.locals.len()]
    }
}

#[test]
fn counts_virtual_objects_across_locals_and_stack() {
    let frame = frame_of(
        vec![FrameValue::Int(7), vobj(0, 1, 0)],
        vec![vobj(1, 1, 0), FrameValue::Int(3), vobj(2, 1, 0)],
    );
    assert_eq!(count_virtual_objects(&frame), 3);
}

#[test]
fn counts_zero_when_no_virtual_objects() {
    let frame = frame_of(
        vec![FrameValue::Int(1), FrameValue::Undefined],
        vec![FrameValue::Int(2)],
    );
    assert_eq!(count_virtual_objects(&frame), 0);
}

/// Step-5: shells materialize as real, distinct, header-valid objects that
/// survive a forced GC while temporarily rooted; the frame's top-level slots
/// are rewritten to real `Object`s.
#[test]
fn shells_materialize_and_survive_forced_gc() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 3 virtual objects at flat slots 1, 2, 3 (locals first, then stack).
    let mut frame = frame_of(
        vec![FrameValue::Int(1), vobj(0, 0, 2), vobj(1, 0, 0)],
        vec![vobj(2, 0, 3), FrameValue::Int(9)],
    );
    assert_eq!(count_virtual_objects(&frame), 3);

    let pin_base = thread.native_pin_roots.len();
    let mapped = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ true,
        /* keep_pins */ false,
    )
    .expect("materialization should succeed");

    assert_eq!(mapped.len(), 3);
    assert_eq!(
        mapped.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    let mut seen = std::collections::HashSet::new();
    for &(slot, addr) in &mapped {
        assert_ne!(addr, 0, "shell address must be non-null");
        assert!(seen.insert(addr), "shells must be distinct objects");
        // Frame slot rewritten to the real object.
        assert_eq!(frame_slot(&frame, slot), &FrameValue::Object(addr));
        let obj = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
        assert_eq!(
            shared.mem.heap.class_id_of(obj),
            ClassId::new(0),
            "shell header must carry the requested class id post-GC"
        );
    }
    assert_eq!(thread.native_pin_roots.len(), pin_base);
}

/// A malformed graph is refused BEFORE any shell is allocated (round 11
/// wave 2, r11-tier-deopt-resume-gc-windows): a top-level
/// `VirtualObjectRef` naming an id no `VirtualObject` defines. The old code
/// allocated the defined shell first and then panicked indexing `shells`.
#[test]
fn malformed_graph_is_refused_before_any_allocation() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mut frame = frame_of(
        vec![vobj(0, 0, 1), FrameValue::VirtualObjectRef(99)],
        Vec::new(),
    );
    let before = frame.clone();
    let pin_base = thread.native_pin_roots.len();
    let r = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ true,
        /* keep_pins */ true,
    );
    assert!(r.is_err(), "an undefined virtual id must be refused");
    assert_eq!(thread.native_pin_roots.len(), pin_base, "no pin may leak");
    assert_eq!(
        format!("{frame:?}"),
        format!("{before:?}"),
        "frame untouched"
    );

    // An unsupported field form is refused the same way.
    let mut frame = frame_of(
        vec![FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 0,
            class_id: 0,
            num_fields: 1,
            field_values: vec![FrameValue::Unsupported],
        })],
        Vec::new(),
    );
    let r = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ true,
        /* keep_pins */ true,
    );
    assert!(r.is_err());
    assert_eq!(thread.native_pin_roots.len(), pin_base);
}

/// Step-6: a virtual object's primitive + already-real-object fields are
/// stored into the shell; an `Undefined` field defaults to 0.
#[test]
fn materializes_primitive_and_object_fields() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // A pre-existing real object to reference from a field.
    let real = shared.mem.heap.alloc_object(ClassId::new(7), 0);
    let real_addr = real.as_ptr() as usize as u64;

    // One virtual object (id 0, class 5, 3 fields): [Int(42), Object(real), Undefined].
    let vo = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: 5,
        num_fields: 3,
        field_values: vec![
            FrameValue::Int(42),
            FrameValue::Object(real_addr),
            FrameValue::Undefined,
        ],
    });
    let mut frame = frame_of(vec![vo], vec![]);

    let mapped = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ false,
        /* keep_pins */ false,
    )
    .expect("materialization should succeed");
    assert_eq!(mapped.len(), 1);
    let (slot, addr) = mapped[0];
    assert_eq!(slot, 0);
    assert_eq!(frame.locals[0], FrameValue::Object(addr));

    let shell = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
    assert_eq!(shared.mem.heap.class_id_of(shell), ClassId::new(5));
    assert_eq!(shared.mem.heap.get_field(shell, 0), Value::Int(42));
    assert_eq!(
        shared.mem.heap.get_field(shell, 1),
        Value::Object(Some(real))
    );
    assert_eq!(shared.mem.heap.get_field(shell, 2), Value::Int(0)); // Undefined -> 0
}

/// j27 wave 5 (lane EAHUNT): the optimizing tier describes a never-stored
/// field as `Int(0)` whatever its declared type. Into a COMPACT reference
/// slot, `set_field` boxes a primitive, so the materialized object's null
/// field came back as a non-null wrapper (`o.next == null` answered false
/// after the deopt). A zero recipe must leave the shell's typed zero.
#[test]
fn a_zero_recipe_leaves_a_compact_reference_field_null() {
    use cratonvm_types::field_layout::{CompactLayout, FieldStorageKind};
    const CLASS: u32 = 61_027;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let domain = shared
        .classes
        .class_manager
        .read()
        .class_store()
        .layout_domain();
    cratonvm_types::register_class_layout(
        domain,
        CLASS,
        Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![true, false],
            field_kinds: vec![FieldStorageKind::Reference, FieldStorageKind::Int],
            ref_disps: vec![8],
            total_size: 24,
        }),
    );
    let vo = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: CLASS,
        num_fields: 2,
        // `ir_lower`'s zero default for both fields.
        field_values: vec![FrameValue::Int(0), FrameValue::Int(5)],
    });
    let mut frame = frame_of(vec![vo], vec![]);
    let mapped = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ false,
        /* keep_pins */ false,
    )
    .expect("materialization should succeed");
    let shell = unsafe { ObjectRef::from_raw(mapped[0].1 as usize as *mut u8) };
    let compact = cratonvm_types::is_compact_object(shared.mem.heap.get_header(shell));
    let (reference, int) = (
        shared.mem.heap.get_field(shell, 0),
        shared.mem.heap.get_field(shell, 1),
    );
    cratonvm_types::unregister_class_layout(CLASS);
    assert!(compact, "the shell of a class with a layout is compact");
    assert_eq!(
        reference,
        Value::Object(None),
        "a never-stored reference field is null, not a boxed 0"
    );
    assert_eq!(int, Value::Int(5));
}

/// j27 wave 6 (lane T1TIERMGR, LAMBDA3FIX21-1 review): a LEGACY shell (no
/// registered layout, as a scalar-replaced lambda proxy is) holds
/// `Value::Int(0)` cells, so a null / `0L` / `0.0` capture recipe must be
/// stored. Fails on the wave-5 rule (every field read back `Int(0)`).
#[test]
fn a_zero_recipe_is_stored_into_a_legacy_shell() {
    const CLASS: u32 = 61_028; // no layout registered
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let vo = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: CLASS,
        num_fields: 4,
        field_values: vec![
            FrameValue::Object(0),
            FrameValue::Long(0),
            FrameValue::Double(0),
            FrameValue::Int(0),
        ],
    });
    let mut frame = frame_of(vec![vo], vec![]);
    let mapped = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ false,
        /* keep_pins */ false,
    )
    .expect("materialization should succeed");
    let shell = unsafe { ObjectRef::from_raw(mapped[0].1 as usize as *mut u8) };
    assert!(!cratonvm_types::is_compact_object(shared.mem.heap.get_header(shell)));
    let fields: Vec<Value> = (0..4).map(|i| shared.mem.heap.get_field(shell, i)).collect();
    assert_eq!(
        fields,
        vec![
            Value::Object(None),
            Value::Long(0),
            Value::Double(0.0),
            Value::Int(0)
        ]
    );
}

#[test]
fn a_legacy_shell_skips_only_its_own_int_zero() {
    for fv in [FrameValue::Undefined, FrameValue::Int(0)] {
        assert!(is_fresh_shell_default(&fv, false));
        assert!(is_fresh_shell_default(&fv, true));
    }
    for fv in [
        FrameValue::Object(0),
        FrameValue::Long(0),
        FrameValue::Float(0),
        FrameValue::Double(0),
    ] {
        assert!(!is_fresh_shell_default(&fv, false), "{fv:?} legacy: stored");
        assert!(is_fresh_shell_default(&fv, true), "{fv:?} compact: skipped");
    }
    assert!(!is_fresh_shell_default(&FrameValue::Int(5), true));
}

/// Step-6 cyclic graph: two objects referencing each other materialize to two
/// heap objects whose fields point at each other — the two-phase
/// shells-first design makes the back-edge resolvable. Run with stress GC so
/// the half-built graph is proven to survive a collection.
#[test]
fn materializes_two_object_cycle() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // A (id 0).field0 -> B ;  B (id 1).field0 -> A
    let a = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: 1,
        num_fields: 1,
        field_values: vec![FrameValue::VirtualObjectRef(1)],
    });
    let b = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 1,
        class_id: 1,
        num_fields: 1,
        field_values: vec![FrameValue::VirtualObjectRef(0)],
    });
    let mut frame = frame_of(vec![a, b], vec![]);

    let mapped = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ true,
        /* keep_pins */ false,
    )
    .expect("materialization should succeed");
    assert_eq!(mapped.len(), 2);

    let addr_a = mapped.iter().find(|&&(s, _)| s == 0).unwrap().1;
    let addr_b = mapped.iter().find(|&&(s, _)| s == 1).unwrap().1;
    let oa = unsafe { ObjectRef::from_raw(addr_a as usize as *mut u8) };
    let ob = unsafe { ObjectRef::from_raw(addr_b as usize as *mut u8) };

    // The cycle is wired: A.field0 == B and B.field0 == A.
    assert_eq!(shared.mem.heap.get_field(oa, 0), Value::Object(Some(ob)));
    assert_eq!(shared.mem.heap.get_field(ob, 0), Value::Object(Some(oa)));
    // Frame slots rewritten.
    assert_eq!(frame.locals[0], FrameValue::Object(addr_a));
    assert_eq!(frame.locals[1], FrameValue::Object(addr_b));
}

/// The lifted pin/re-read: every non-null `Object(addr)` in a detached frame
/// — locals, stack, monitor objects and a virtual object's field recipe —
/// is rooted across a forced collection and rewritten from its pin, in the
/// same order; nulls and primitives are left alone.
#[test]
fn frame_object_refs_survive_a_collection_between_pin_and_reread() {
    use cratonvm_jit::deopt::MonitorInfo;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Told apart after the collection by a marker in field 0.
    let objs: Vec<ObjectRef> = (0..4)
        .map(|i| {
            let o = shared.mem.heap.alloc_object(ClassId::new(0), 1);
            shared.mem.heap.set_field(o, 0, Value::Int(100 + i));
            o
        })
        .collect();
    let addr = |o: ObjectRef| o.as_ptr() as usize as u64;
    let nested = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: 1,
        num_fields: 2,
        field_values: vec![FrameValue::Object(addr(objs[2])), FrameValue::Object(0)],
    });
    let mut frame = ReconstructedFrame {
        class_id: 0,
        method_key: "T.m".to_string(),
        bci: 0,
        locals: vec![
            FrameValue::Object(addr(objs[0])),
            FrameValue::Int(5),
            nested,
        ],
        stack: vec![FrameValue::Object(0), FrameValue::Object(addr(objs[1]))],
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(addr(objs[3])),
            lock_depth: 1,
            relock: false,
        }],
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    };

    let base = pin_frame_object_refs(&mut thread, &frame);
    assert_eq!(
        thread.native_pin_roots.len(),
        base + 4,
        "four non-null refs"
    );
    maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-materialize-test");
    reread_frame_object_refs(&thread, &mut frame, base);

    let pinned: Vec<u64> = thread.native_pin_roots[base..]
        .iter()
        .map(|o| o.as_ptr() as usize as u64)
        .collect();
    // Traversal order: locals (with the nested recipe), stack, monitors.
    assert_eq!(frame.locals[0], FrameValue::Object(pinned[0]));
    assert_eq!(frame.locals[1], FrameValue::Int(5));
    match &frame.locals[2] {
        FrameValue::VirtualObject(state) => {
            assert_eq!(state.field_values[0], FrameValue::Object(pinned[1]));
            assert_eq!(state.field_values[1], FrameValue::Object(0));
        }
        other => panic!("nested recipe replaced: {other:?}"),
    }
    assert_eq!(frame.stack[0], FrameValue::Object(0));
    assert_eq!(frame.stack[1], FrameValue::Object(pinned[2]));
    assert_eq!(frame.monitors[0].object, FrameValue::Object(pinned[3]));
    for (i, marker) in [(0usize, 100), (1, 102), (2, 101), (3, 103)] {
        let obj = unsafe { ObjectRef::from_raw(pinned[i] as usize as *mut u8) };
        assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Int(marker));
    }
    thread.native_pin_roots.truncate(base);
}

/// Phase C (monitors): a held monitor whose object is a scalar-replaced one
/// (referenced by `VirtualObjectRef`) is rewritten to the SAME materialized
/// shell as the local that defines it, so the resume can relock the real object.
#[test]
fn materialize_rewrites_held_monitor_object() {
    use cratonvm_jit::deopt::MonitorInfo;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // local 1 defines virtual object id 0; a held monitor references it.
    let mut frame = ReconstructedFrame {
        class_id: 0,
        method_key: "T.m".to_string(),
        bci: 0,
        locals: vec![FrameValue::Int(0), vobj(0, 1, 0)],
        stack: vec![],
        monitors: vec![MonitorInfo {
            object: FrameValue::VirtualObjectRef(0),
            lock_depth: 2,
            relock: true,
        }],
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    };

    let mapped = materialize_virtual_objects(
        &shared,
        &mut thread,
        &mut frame,
        /* stress_gc */ true,
        /* keep_pins */ false,
    )
    .expect("materialization should succeed");

    // The defining local was materialized to a real Object.
    let local_addr = mapped
        .iter()
        .find(|&&(s, _)| s == 1)
        .expect("local 1 materialized")
        .1;
    assert_eq!(frame.locals[1], FrameValue::Object(local_addr));
    // The monitor's object was rewritten to the SAME shell; depth preserved.
    assert_eq!(frame.monitors[0].object, FrameValue::Object(local_addr));
    assert_eq!(frame.monitors[0].lock_depth, 2);
}

/// JIT round 21 wave 3 (lane ea, EA21-2): an auto-box state's payload must
/// be its wrapper's primitive; anything else is refused by the validation
/// BEFORE any allocation or `valueOf` call.
#[test]
fn an_auto_box_payload_is_its_wrappers_primitive_or_the_frame_is_refused() {
    use cratonvm_jit::deopt::{AUTO_BOX_INTEGER_ATYPE, AUTO_BOX_LONG_ATYPE};
    let state = |atype: u8, v: FrameValue| VirtualObjectState {
        array_element_type: Some(atype),
        id: 3,
        class_id: 0,
        num_fields: 1,
        field_values: vec![v],
    };
    assert_eq!(
        auto_box_payload(&state(AUTO_BOX_INTEGER_ATYPE, FrameValue::Int(-5))).ok(),
        Some(Value::Int(-5))
    );
    assert_eq!(
        auto_box_payload(&state(AUTO_BOX_LONG_ATYPE, FrameValue::Long(1 << 40))).ok(),
        Some(Value::Long(1 << 40))
    );
    assert!(auto_box_payload(&state(AUTO_BOX_INTEGER_ATYPE, FrameValue::Long(1))).is_err());
    assert!(auto_box_payload(&state(AUTO_BOX_LONG_ATYPE, FrameValue::Undefined)).is_err());

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pins = thread.native_pin_roots.len();
    let bad = FrameValue::VirtualObject(state(AUTO_BOX_INTEGER_ATYPE, FrameValue::Double(0)));
    let mut frame = frame_of(vec![bad], vec![]);
    assert!(materialize_virtual_objects(&shared, &mut thread, &mut frame, false, false).is_err());
    assert_eq!(thread.native_pin_roots.len(), pins, "nothing pinned on a refusal");
}

/// A scalar-replaced REFERENCE array (atype 0, a varargs `Object[]`) comes
/// back as an array of its component class, its stored element in place and
/// its never-stored element (`Int(0)`) left null.
#[test]
fn materializes_a_reference_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let real = shared.mem.heap.alloc_object(ClassId::new(7), 0);
    let real_addr = real.as_ptr() as usize as u64;
    let arr = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: Some(0),
        id: 0,
        class_id: 9,
        num_fields: 2,
        field_values: vec![FrameValue::Object(real_addr), FrameValue::Int(0)],
    });
    let mut frame = frame_of(vec![arr], vec![]);
    let mapped = materialize_virtual_objects(&shared, &mut thread, &mut frame, false, false)
        .expect("a reference array materializes");
    let (_, addr) = mapped[0];
    let shell = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
    assert_eq!(
        shared.mem.heap.array_element_type(shell),
        Some(ArrayElementType::Reference)
    );
    assert_eq!(shared.mem.heap.get_array_element(shell, 0), Ok(Value::Object(Some(real))));
    assert_eq!(shared.mem.heap.get_array_element(shell, 1), Ok(Value::Object(None)));
}
