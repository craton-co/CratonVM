use super::*;

fn frame(locals: Vec<FrameValue>, monitors: Vec<MonitorInfo>) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "S.m:()I".to_string(),
        bci: 7,
        locals,
        stack: Vec::new(),
        monitors,
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

fn held(obj: u64) -> MonitorInfo {
    MonitorInfo {
        object: FrameValue::Object(obj),
        lock_depth: 1,
        relock: false,
    }
}

/// The one shape a self-locking body's guard publishes is recognised; every
/// neighbour of it is not.
#[test]
fn only_the_self_lock_shape_is_a_handed_method_monitor() {
    let inst = SinkMethodMonitor::Instance;
    let ok = frame(vec![FrameValue::Object(0x1000)], vec![held(0x1000)]);
    assert_eq!(handed_method_monitor_index(inst, false, &ok), Some(0));
    // A method with monitor ops, an unsynchronized method.
    assert_eq!(handed_method_monitor_index(inst, true, &ok), None);
    assert_eq!(
        handed_method_monitor_index(SinkMethodMonitor::None, false, &ok),
        None
    );
    // A lock on another object, an elided one, two levels, two monitors.
    let other = frame(vec![FrameValue::Object(0x1000)], vec![held(0x2000)]);
    assert_eq!(handed_method_monitor_index(inst, false, &other), None);
    let mut elided = held(0x1000);
    elided.relock = true;
    let elided = frame(vec![FrameValue::Object(0x1000)], vec![elided]);
    assert_eq!(handed_method_monitor_index(inst, false, &elided), None);
    let mut deep = held(0x1000);
    deep.lock_depth = 2;
    let deep = frame(vec![FrameValue::Object(0x1000)], vec![deep]);
    assert_eq!(handed_method_monitor_index(inst, false, &deep), None);
    let two = frame(
        vec![FrameValue::Object(0x1000)],
        vec![held(0x1000), held(0x2000)],
    );
    assert_eq!(handed_method_monitor_index(inst, false, &two), None);
    // No monitor, an undescribed or null `this`, an inlined chain.
    let none = frame(vec![FrameValue::Object(0x1000)], Vec::new());
    assert_eq!(handed_method_monitor_index(inst, false, &none), None);
    let dead_this = frame(vec![FrameValue::Undefined], vec![held(0x1000)]);
    assert_eq!(handed_method_monitor_index(inst, false, &dead_this), None);
    let null_this = frame(vec![FrameValue::Object(0)], vec![held(0)]);
    assert_eq!(handed_method_monitor_index(inst, false, &null_this), None);
    let mut chain = ok.clone();
    chain
        .caller_frames
        .push(frame(vec![FrameValue::Int(1)], Vec::new()));
    assert_eq!(handed_method_monitor_index(inst, false, &chain), None);
}

/// Round 13 wave 11 (lane sync7): a `static synchronized` frame's one
/// taken level is its class mirror, whatever its locals hold (no `this`);
/// every other neighbour of the shape is still refused.
#[test]
fn a_static_frame_hands_over_its_one_taken_level() {
    let stat = SinkMethodMonitor::Static;
    let mirror = frame(vec![FrameValue::Int(3)], vec![held(0x3000)]);
    assert_eq!(handed_method_monitor_index(stat, false, &mirror), Some(0));
    let no_locals = frame(Vec::new(), vec![held(0x3000)]);
    assert_eq!(
        handed_method_monitor_index(stat, false, &no_locals),
        Some(0)
    );
    // A method with monitor ops, no monitor, two, an elided or a deeper
    // level, a null object, an unresolved one, an inlined chain.
    assert_eq!(handed_method_monitor_index(stat, true, &mirror), None);
    let none = frame(vec![FrameValue::Int(3)], Vec::new());
    assert_eq!(handed_method_monitor_index(stat, false, &none), None);
    let two = frame(Vec::new(), vec![held(0x3000), held(0x4000)]);
    assert_eq!(handed_method_monitor_index(stat, false, &two), None);
    let mut elided = held(0x3000);
    elided.relock = true;
    assert_eq!(
        handed_method_monitor_index(stat, false, &frame(Vec::new(), vec![elided])),
        None
    );
    let mut deep = held(0x3000);
    deep.lock_depth = 2;
    assert_eq!(
        handed_method_monitor_index(stat, false, &frame(Vec::new(), vec![deep])),
        None
    );
    assert_eq!(
        handed_method_monitor_index(stat, false, &frame(Vec::new(), vec![held(0)])),
        None
    );
    let unresolved = MonitorInfo {
        object: FrameValue::StackSlotRef(-24),
        lock_depth: 1,
        relock: false,
    };
    assert_eq!(
        handed_method_monitor_index(stat, false, &frame(Vec::new(), vec![unresolved])),
        None
    );
    let mut chain = mirror.clone();
    chain
        .caller_frames
        .push(frame(vec![FrameValue::Int(1)], Vec::new()));
    assert_eq!(handed_method_monitor_index(stat, false, &chain), None);
}

/// JIT round 18 wave 9 (lane syncdoor6, SD17-3): a MARKED frame names the
/// method monitor first, beside block monitors, in a method with monitor
/// bytecode; the unmarked twin of the same frame is never read as one.
#[test]
fn r18_a_marked_frame_hands_over_its_first_monitor_beside_blocks() {
    let inst = SinkMethodMonitor::Instance;
    let marked = |monitors: Vec<MonitorInfo>| {
        let mut f = frame(vec![FrameValue::Object(0x1000)], monitors);
        f.semantics = f.semantics.with_method_monitor_first(true);
        f
    };
    let two = marked(vec![held(0x1000), held(0x2000)]);
    assert_eq!(handed_method_monitor_index(inst, true, &two), Some(0));
    // A block on `this` itself (`synchronized (this)` inside the method).
    let on_this = marked(vec![held(0x1000), held(0x1000)]);
    assert_eq!(handed_method_monitor_index(inst, true, &on_this), Some(0));
    // Outside every block: the method monitor alone, marked.
    let alone = marked(vec![held(0x1000)]);
    assert_eq!(handed_method_monitor_index(inst, true, &alone), Some(0));
    // Unmarked: the shape rule, which monitor bytecode refuses.
    let mut unmarked = two.clone();
    unmarked.semantics = ResumeSemantics::REEXECUTE;
    assert_eq!(handed_method_monitor_index(inst, true, &unmarked), None);
    assert_eq!(handed_method_monitor_index(inst, false, &unmarked), None);
    // Marked, but the first monitor is not `this`, is elided, or is absent.
    let other_first = marked(vec![held(0x2000), held(0x1000)]);
    assert_eq!(handed_method_monitor_index(inst, true, &other_first), None);
    let mut elided = held(0x1000);
    elided.relock = true;
    assert_eq!(
        handed_method_monitor_index(inst, true, &marked(vec![elided])),
        None
    );
    assert_eq!(
        handed_method_monitor_index(inst, true, &marked(Vec::new())),
        None
    );
    // The mark is no resume difference.
    assert_eq!(
        two.semantics.resume_kind(),
        ResumeSemantics::REEXECUTE.resume_kind()
    );
}

/// The peek answers about the innermost stashed frame and leaves it there.
#[test]
fn the_frame_peek_does_not_take_the_stash() {
    std::thread::spawn(|| {
        assert_eq!(peek_last_deopt_frame(|f| f.bci), None);
        restash_last_deopt(frame(vec![FrameValue::Object(0x1000)], vec![held(0x1000)]));
        assert_eq!(peek_last_deopt_frame(|f| f.monitors.len()), Some(1));
        assert!(has_last_deopt(), "a peek must not consume the stash");
        let taken = take_last_deopt().expect("still stashed");
        assert_eq!(taken.bci, 7);
    })
    .join()
    .expect("test thread");
}
