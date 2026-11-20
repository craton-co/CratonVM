use super::*;
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
use crate::native::registry::StackTraceEntry;
use std::sync::Arc;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the attribution only compares.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// No OSR body holds (GC1119-1's argument when nothing was kept).
fn none(_: usize) -> Vec<ObjectRef> {
    Vec::new()
}

/// GC1119-1 (round 19 wave 12, lane gc12): a block an OSR body took
/// after its entry lands at the interpreter frame it continues, before
/// (innermost of) the blocks that frame recorded itself.
#[test]
fn an_osr_bodys_newer_block_lands_at_the_frame_it_continues() {
    let (a, p) = (obj(0x1000), obj(0x2000));
    let mut main = interp("main");
    main.held_monitors.push(a);
    let frames = vec![main, interp("hot")];
    let jit: Vec<ActiveCompiledFrame> = Vec::new();
    let trace = vec![entry("probe/T", "main"), entry("probe/T", "hot")];
    let osr = Default::default();
    let osr_holds = |i: usize| if i == 0 { vec![p] } else { Vec::new() };
    let pairs =
        attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 2, &[], osr_holds, |_| {
            Vec::new()
        })
        .expect("the layout matches");
    assert_eq!(pairs, vec![(0, p), (0, a)]);
    let (monitors, depths) = attribute_locked_monitors(2, vec![a, p], Some(pairs), None);
    assert_eq!(monitors, vec![p, a]);
    assert_eq!(depths, vec![1, 1]);
}

fn interp(method: &str) -> Frame {
    Frame::new(
        crate::classloading::ClassId::new(3),
        "probe/T".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0; 16],
        vec![],
        2,
        2,
        &[],
    )
}

fn compiled(
    depth: u32,
    method: &str,
    owner: u32,
    chain: Vec<InlinedLevel>,
) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        chain_index: 0,
        label: Arc::from(format!("probe/T.{method}:()V")),
        owner_class_id: owner,
        cm_ptr: 0,
        bci: 1,
        inline_chain: chain,
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

fn entry(class: &str, method: &str) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: 0,
        class_id: None,
        method_index: None,
    }
}

/// `main` (interpreted, holding `a` in a block) -> `hot` (compiled) ->
/// `sync` (compiled static self-lock, holding the mirror `m`, with one
/// inlined callee) -> `report` (interpreted, the innermost). Every
/// monitor lands at its own frame's trace index, innermost first; a
/// disagreeing trace answers `None`.
#[test]
fn interpreted_and_static_compiled_holds_land_at_their_own_positions() {
    let (a, m) = (obj(0x1000), obj(0x2000));
    let mut main = interp("main");
    main.held_monitors.push(a);
    let frames = vec![main, interp("report")];
    let inl = InlinedLevel {
        label: Arc::from("probe/T.inl:()V"),
        bci: 2,
        class_id: 0,
        hidden: None,
    };
    let jit = vec![
        compiled(1, "hot", 9, Vec::new()),
        compiled(1, "sync", 7, vec![inl]),
    ];
    let trace = vec![
        entry("probe/T", "main"),
        entry("probe/T", "hot"),
        entry("probe/T", "sync"),
        entry("probe/T", "inl"),
        entry("probe/T", "report"),
    ];
    let osr = Default::default();
    let hold = |f: &ActiveCompiledFrame| {
        if f.owner_class_id == 7 {
            vec![m]
        } else {
            Vec::new()
        }
    };
    let pairs = attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 5, &[], none, hold)
        .expect("the layout matches the trace");
    assert_eq!(pairs, vec![(2, m), (0, a)]);
    let (monitors, depths) = attribute_locked_monitors(5, vec![a, m], Some(pairs), None);
    assert_eq!(monitors, vec![m, a]);
    assert_eq!(depths, vec![2, 4]);

    // A served frame inserted before captured index 0 shifts the check.
    let mut served = trace.clone();
    served.insert(0, entry("java/lang/Thread", "run"));
    assert!(
        attribute_over_compiled_layout(&frames, &jit, &osr, &served, 5, &[0], none, hold)
            .is_some()
    );
    // A name disagreement, or a length one, falls back to `None`.
    let mut wrong = trace.clone();
    wrong[1] = entry("probe/T", "other");
    assert!(
        attribute_over_compiled_layout(&frames, &jit, &osr, &wrong, 5, &[], none, hold)
            .is_none()
    );
    assert!(
        attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 4, &[], none, hold)
            .is_none()
    );
    let mut class = trace.clone();
    class[0] = entry("probe/U", "main");
    assert!(
        attribute_over_compiled_layout(&frames, &jit, &osr, &class, 5, &[], none, hold)
            .is_none()
    );
}

/// SYNC619-1 (round 19 wave 7, lane gc7): what one activation names. A
/// static self-locking body: its class mirror, with or without frame
/// words. An instance one: its receiver only through a frame word, so
/// nothing with the words off or without a frame pointer. No artifact
/// (`cm_ptr == 0`), or a body that holds nothing: nothing.
#[test]
fn an_activation_names_its_mirror_and_only_frame_checked_words() {
    let artifact = |hold: Option<cratonvm_jit::SelfLockHold>| {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit_byte(0xC3);
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.self_lock_hold = hold;
        cm
    };
    let at = |cm: &cratonvm_jit::CompiledMethod| {
        let mut f = compiled(1, "sync", 7, Vec::new());
        f.cm_ptr = cm as *const cratonvm_jit::CompiledMethod as usize;
        f
    };
    let mirror = |id: u32| (id == 7).then_some(0x2000usize);
    let stat = artifact(Some(cratonvm_jit::SelfLockHold::Static {
        mirror_word_rbp_off: Some(8),
    }));
    for words in [false, true] {
        // SAFETY: `cm_ptr` names `stat`, alive for the call; no frame read
        // is made (the static mirror needs none, and there is no RBP).
        let held =
            unsafe { compiled_frame_hold_words(&at(&stat), words, words, words, mirror) };
        assert_eq!(held.as_slice(), &[0x2000], "words={words}");
        // SAFETY: as above.
        assert!(unsafe { compiled_frame_may_hold(&at(&stat)) });
    }
    let inst = artifact(Some(cratonvm_jit::SelfLockHold::Instance {
        obj_rbp_off: Some(8),
    }));
    for words in [false, true] {
        // SAFETY: as above; `frame_rbp` is `None`, so no word is read.
        let held =
            unsafe { compiled_frame_hold_words(&at(&inst), words, words, words, mirror) };
        assert!(held.is_empty(), "words={words}");
    }
    let plain = artifact(None);
    // SAFETY: as above.
    assert!(
        unsafe { compiled_frame_hold_words(&at(&plain), true, true, true, mirror) }.is_empty()
    );
    // SAFETY: as above.
    assert!(!unsafe { compiled_frame_may_hold(&at(&plain)) });
    let none = compiled(1, "x", 7, Vec::new());
    // SAFETY: `cm_ptr == 0` names no artifact.
    assert!(unsafe { compiled_frame_hold_words(&none, true, true, true, mirror) }.is_empty());
    // SAFETY: as above.
    assert!(!unsafe { compiled_frame_may_hold(&none) });
}

/// GC819-3 (round 19 wave 9, lane gc9): a caller-held synchronized body
/// (`requires_wrapped_entry`, monitor-free) names its method monitor: a
/// static one its class mirror with the switch on, nothing with it off;
/// an instance one only through a frame word (none without a frame
/// pointer); a self-locking, OSR, or arity-unknown body nothing extra.
#[test]
fn a_caller_held_synchronized_body_names_its_method_monitor() {
    let artifact = |java_args: u16, label: &str| {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit_byte(0xC3);
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.requires_wrapped_entry = true;
        cm.entry_java_args = java_args;
        cm.method_label = label.to_string();
        cm
    };
    let at = |cm: &cratonvm_jit::CompiledMethod| {
        let mut f = compiled(1, "sync", 7, Vec::new());
        f.cm_ptr = cm as *const cratonvm_jit::CompiledMethod as usize;
        f
    };
    let mirror = |id: u32| (id == 7).then_some(0x2000usize);
    let stat = artifact(2, "probe/T.sync:(IJ)V");
    assert_eq!(caller_held_body_is_static(&stat), Some(true));
    // SAFETY: `cm_ptr` names `stat`, alive for the call; the static
    // mirror needs no frame read, and there is no RBP.
    assert!(unsafe { compiled_frame_may_hold(&at(&stat)) });
    for words in [false, true] {
        // SAFETY: as above.
        let held = unsafe { compiled_frame_hold_words(&at(&stat), words, words, true, mirror) };
        assert_eq!(held.as_slice(), &[0x2000], "words={words}");
        // SAFETY: as above.
        let off = unsafe { compiled_frame_hold_words(&at(&stat), words, words, false, mirror) };
        assert!(off.is_empty(), "words={words}: the kill switch");
    }
    let inst = artifact(3, "probe/T.sync:(IJ)V");
    assert_eq!(caller_held_body_is_static(&inst), Some(false));
    // SAFETY: as above; `frame_rbp` is `None`, so no word is read.
    assert!(
        unsafe { compiled_frame_hold_words(&at(&inst), true, true, true, mirror) }.is_empty()
    );
    for (what, cm) in [
        ("unknown arity", artifact(u16::MAX, "probe/T.sync:(IJ)V")),
        ("arity disagrees", artifact(5, "probe/T.sync:(IJ)V")),
        ("no descriptor", artifact(2, "probe/T.sync")),
    ] {
        assert_eq!(caller_held_body_is_static(&cm), None, "{what}");
        // SAFETY: as above.
        let held = unsafe { compiled_frame_hold_words(&at(&cm), true, true, true, mirror) };
        assert!(held.is_empty(), "{what}");
    }
    let mut osr = artifact(2, "probe/T.sync:(IJ)V");
    osr.compiled_via_osr = true;
    assert!(!caller_held_method_body(&osr));
    let mut self_locking = artifact(2, "probe/T.sync:(IJ)V");
    self_locking.self_locks_monitor = true;
    assert!(!caller_held_method_body(&self_locking));
    let mut plain = artifact(2, "probe/T.sync:(IJ)V");
    plain.requires_wrapped_entry = false;
    // SAFETY: as above.
    assert!(!unsafe { compiled_frame_may_hold(&at(&plain)) });
}

/// GC920-1 / GC920-4 (round 19 wave 11, lane gc11): a caller-held
/// INSTANCE body read through the held-thread reader, over a synthetic
/// band (`[rbp - 8]` the receiver home, `[rbp - 16]` the safepoint id,
/// a 32-byte frame): a single-pass body names its receiver where the
/// active map names the home; an optimizing-tier body names it only with
/// `CRATONVM_JMX_CALLER_HELD_IR_RECEIVER_HOME` (unset here: nothing); no
/// published frame size, nothing.
#[test]
fn a_caller_held_instance_receiver_is_read_through_the_held_reader() {
    use crate::jit::conservative_roots::FrameWordReader;
    const SP_ID: u32 = 3;
    const RECV: usize = 0x7200_0040;
    let mut band = Box::new([0usize; 16]);
    let rbp = band.as_ptr() as usize + 8 * 12;
    band[11] = RECV;
    band[10] = SP_ID as usize;
    let artifact = |ir: bool, size: i32| {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit_byte(0xC3);
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.requires_wrapped_entry = true;
        cm.entry_java_args = 2;
        cm.method_label = "probe/T.spin:(J)J".to_string();
        cm.used_ir_backend = ir;
        cm.sp_id_slot_off = 16;
        cm.frame_layout.frame_size = size;
        let mut m = cratonvm_jit::OopMapEntry::new(0x10);
        m.bytecode_pc = SP_ID;
        m.frame_slot_offsets = vec![8];
        cm.push_oop_map(m);
        cm
    };
    let at = |cm: &cratonvm_jit::CompiledMethod| {
        let mut f = compiled(1, "spin", 7, Vec::new());
        f.cm_ptr = cm as *const cratonvm_jit::CompiledMethod as usize;
        f.frame_rbp = Some(rbp);
        f
    };
    let mirror = |_: u32| None;
    let held = FrameWordReader::HeldThread;
    let sp = artifact(false, 32);
    assert_eq!(caller_held_body_is_static(&sp), Some(false));
    // SAFETY: `cm_ptr` names `sp`, alive for the call; `frame_rbp` lies
    // inside `band`, which outlives it, and the read stays in its frame.
    let words =
        unsafe { compiled_frame_hold_words_by(&at(&sp), true, true, true, held, mirror) };
    assert_eq!(words.as_slice(), &[RECV]);
    // SAFETY: as above.
    let off =
        unsafe { compiled_frame_hold_words_by(&at(&sp), true, true, false, held, mirror) };
    assert!(off.is_empty(), "the GC819-3 switch");
    let unsized_frame = artifact(false, 0);
    let unsized_at = at(&unsized_frame);
    // SAFETY: as above.
    let none =
        unsafe { compiled_frame_hold_words_by(&unsized_at, true, true, true, held, mirror) };
    assert!(none.is_empty(), "no published frame size");
    if !caller_held_ir_receiver_home_enabled() {
        let ir = artifact(true, 32);
        // SAFETY: as above.
        let ir_words =
            unsafe { compiled_frame_hold_words_by(&at(&ir), true, true, true, held, mirror) };
        assert!(ir_words.is_empty(), "GC920-1 is opt-in");
    }
    drop(band);
}

/// SYNC619-2: two holds of one compiled activation land at its position,
/// innermost first, beside an interpreter frame's.
#[test]
fn several_holds_of_one_activation_keep_their_order_at_its_position() {
    let (a, b, c) = (obj(0x1000), obj(0x2000), obj(0x3000));
    let mut main = interp("main");
    main.held_monitors.push(a);
    let frames = vec![main];
    let jit = vec![compiled(1, "blocks", 7, Vec::new())];
    let trace = vec![entry("probe/T", "main"), entry("probe/T", "blocks")];
    let osr = Default::default();
    let holds = |_: &ActiveCompiledFrame| vec![c, b];
    let pairs =
        attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 2, &[], none, holds)
            .expect("the layout matches");
    assert_eq!(pairs, vec![(1, c), (1, b), (0, a)]);
    let layout = compiled_layout(&frames, &jit, &osr);
    assert!(layout_matches_trace(&layout, &frames, &trace, 2, &[]));
    assert!(!layout_matches_trace(&layout, &frames, &trace[..1], 1, &[]));
}

/// A compiled activation whose label does not parse contributes no
/// position (the trace builders drop it), and its holds are not named.
#[test]
fn an_unparsable_compiled_label_takes_no_position() {
    let frames = vec![interp("main")];
    let mut bad = compiled(1, "x", 7, Vec::new());
    bad.label = Arc::from("no-separators");
    let jit = vec![bad];
    let trace = vec![entry("probe/T", "main")];
    let osr = Default::default();
    let pairs =
        attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 1, &[], none, |_| {
            vec![obj(0x3000)]
        })
        .expect("one interpreter frame");
    assert!(pairs.is_empty());
}
