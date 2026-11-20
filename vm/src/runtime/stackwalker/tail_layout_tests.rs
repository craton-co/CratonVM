use super::*;

fn interp(method: &str, cid: u32, pc: usize) -> Frame {
    let mut f = Frame::new(
        ClassId::new(cid),
        format!("probe/C{cid}"),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0; 16],
        vec![],
        1,
        1,
        &[],
    );
    f.last_instr_pc = pc;
    f
}

fn compiled(depth: u32, name: &str, owner: u32, chain: &[(&str, u32)]) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        label: Arc::from(format!("probe/K.{name}:()V")),
        owner_class_id: owner,
        cm_ptr: 0,
        bci: 3,
        inline_chain: chain
            .iter()
            .map(|(m, cid)| InlinedLevel {
                label: Arc::from(format!("probe/I.{m}:()V")),
                bci: 1,
                class_id: *cid,
                hidden: None,
            })
            .collect(),
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
        chain_index: 0,
    }
}

/// `(method, holder, holder name)` of each slot, outermost first.
fn identities(slots: &[TraceSlot<'_>]) -> Vec<(String, Option<ClassId>, String)> {
    slots
        .iter()
        .filter_map(|s| s.fill_identity())
        .map(|(m, h, c)| (m.to_string(), h, c.to_string()))
        .collect()
}

#[test]
fn a_suffix_layout_is_the_whole_layouts_suffix() {
    // Ten interpreter frames; compiled entries at several depths (0 sits
    // before frame 0, 10 after the last frame), some with inlined callees,
    // one with an empty chain, two at the same depth.
    let frames: Vec<Frame> = (0..10)
        .map(|i| interp(&format!("m{i}"), i + 1, i as usize))
        .collect();
    let jit = vec![
        compiled(0, "k0", 100, &[("a", 200)]),
        compiled(2, "k2", 101, &[]),
        compiled(2, "k2b", 102, &[("b", 201), ("c", 202)]),
        compiled(5, "k5", 103, &[("d", 203)]),
        compiled(9, "k9", 104, &[]),
        compiled(10, "k10", 105, &[("e", 204), ("f", 205), ("g", 206)]),
    ];
    let osr = OsrBciOverrides::default();
    let whole = collect_trace_slots(&frames, &jit, &osr);
    let whole_ids = identities(&whole);
    for start in 0..frames.len() {
        let floor = start as u32 + 1;
        let above: Vec<ActiveCompiledFrame> = jit
            .iter()
            .filter(|f| f.interp_depth >= floor)
            .cloned()
            .collect();
        let suffix = collect_trace_slots_from(&frames, start, &above, &osr);
        let suffix_ids = identities(&suffix);
        assert!(suffix_ids.len() <= whole_ids.len());
        assert_eq!(
            suffix_ids,
            whole_ids[whole_ids.len() - suffix_ids.len()..],
            "start {start}"
        );
        // The suffix begins exactly at frame `start`.
        assert_eq!(suffix_ids[0].0, format!("m{start}"), "start {start}");
    }
}

#[test]
fn the_class_id_tail_matches_the_whole_walk_without_compiled_frames() {
    if !crate::jit::stack_trace::active_compiled_frames().is_empty() {
        // Compiled frames on this thread: the live walk answers for them.
        return;
    }
    let frames: Vec<Frame> = (0..40)
        .map(|i| interp(&format!("m{i}"), i % 7 + 1, 0))
        .collect();
    let whole = frame_class_ids_with_compiled(&frames);
    for want in [0usize, 1, 2, 3, 4, 7, 8, 9, 16, 39, 40, 41, 100] {
        let (tail, is_whole) = frame_class_ids_tail(&frames, want);
        assert_eq!(tail, whole[..whole.len().min(want)].to_vec(), "want {want}");
        assert_eq!(is_whole || tail.len() == want, true, "want {want}");
    }
    // A short stack is the whole answer.
    let few = &frames[..3];
    let (tail, is_whole) = frame_class_ids_tail(few, 5);
    assert!(is_whole);
    assert_eq!(tail, frame_class_ids_with_compiled(few));
}

#[test]
fn the_entry_tail_matches_the_whole_capture_without_compiled_frames() {
    if !crate::jit::stack_trace::active_compiled_frames().is_empty() {
        return;
    }
    let store = ClassStore::default();
    let frames: Vec<Frame> = (0..40)
        .map(|i| interp(&format!("m{i}"), i % 7 + 1, i as usize))
        .collect();
    let whole = capture_full_trace(&store, &frames);
    let key = |e: &StackTraceEntry| {
        (
            e.class_name.clone(),
            e.method_name.clone(),
            e.class_id,
            e.byte_code_index,
        )
    };
    for want in [1usize, 2, 3, 4, 8, 9, 20, 39, 40, 41] {
        let (tail, is_whole) = capture_tail_trace(&store, &frames, want);
        let expect: Vec<_> = whole[whole.len().saturating_sub(want)..]
            .iter()
            .map(key)
            .collect();
        let got: Vec<_> = tail.iter().map(key).collect();
        assert_eq!(got, expect, "want {want}");
        assert_eq!(is_whole || got.len() == want, true, "want {want}");
    }
}

/// `MethodHandles.lookup()` called from `Friend.look()`, which the JIT inlined
/// into `main`'s OSR body; `lookup` runs as its own compiled activation. The
/// OSR half of `main` is dropped as the interpreter frame's continuation, and
/// the class-id walk must still list the `look` level it inlined: the caller of
/// `lookup` is `Friend`, not `main` (probe `tools/probes/LookupClassInlined.java`).
#[test]
fn an_osr_continuations_inlined_levels_stay_in_the_class_id_walk() {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let cm = cratonvm_jit::CompiledMethod::new(buf);
    let cm_ptr = &cm as *const cratonvm_jit::CompiledMethod as usize;
    let (main_cid, friend_cid, mh_cid) = (1u32, 2u32, 3u32);
    let frames = vec![interp("main", main_cid, 16)];
    let mut osr_main = compiled(1, "main", main_cid, &[("look", friend_cid)]);
    osr_main.label = Arc::from("probe/C1.main:()V");
    osr_main.cm_ptr = cm_ptr;
    let lookup = compiled(1, "lookup", mh_cid, &[]);
    let walk = || vec![osr_main.clone(), lookup.clone()];
    let ids = |v: Vec<ClassId>| v.into_iter().map(ClassId::as_u32).collect::<Vec<_>>();

    let registry = |i: usize| (i == 0).then_some(cm_ptr);
    assert_eq!(
        ids(interleave_class_ids(&frames, 0, walk(), registry)),
        vec![mh_cid, friend_cid, main_cid],
    );
    // A chain from the coarse key is not expanded, as for a kept entry.
    let mut coarse = osr_main.clone();
    coarse.chain_exact = false;
    assert_eq!(
        ids(interleave_class_ids(
            &frames,
            0,
            vec![coarse, lookup.clone()],
            registry
        )),
        vec![mh_cid, main_cid],
    );
    drop(cm);
}
