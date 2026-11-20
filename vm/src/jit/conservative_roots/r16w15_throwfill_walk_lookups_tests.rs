use super::{activation_bci_with, WalkLookups};

/// A walk-scoped lookup answers every address as the per-call lookup
/// does, including an address asked twice in a row (the recursion memo)
/// and a different one after it.
#[test]
fn walk_scoped_code_ranges_are_the_per_call_answers() {
    // Far from any real body or another test's synthetic range.
    let entry = 0x7E15_0F20_0000usize;
    let cm_ptr = 0x7E15_0F30_0000usize;
    cratonvm_jit::register_jit_code_range(entry, 0x80, cm_ptr);
    let mut scoped = WalkLookups::walk_scoped();
    let mut per_call = WalkLookups::per_call();
    for addr in [
        entry + 0x10,
        entry + 0x10,
        entry - 1,
        entry - 1,
        entry + 0x7F,
        entry + 0x80,
        entry + 0x10,
    ] {
        assert_eq!(
            scoped.code_range(addr),
            per_call.code_range(addr),
            "{addr:#x}"
        );
    }
    assert_eq!(scoped.code_range(entry + 0x20), Some(cm_ptr));
    drop(scoped);
    cratonvm_jit::unregister_jit_code_range(entry);
}

/// JIT round 19 wave 1 (lane throwfill, THROWS18-3): the two-way memo
/// answers every address as the per-call lookup does, for two return
/// addresses taking turns (in and out of two ranges), a third one breaking
/// in, and a repeat; and the one-way walk answers the same.
#[test]
fn two_way_memo_answers_an_alternation_as_the_per_call_lookup() {
    let a = 0x7E19_0A20_0000usize;
    let b = 0x7E19_0B20_0000usize;
    let (cm_a, cm_b) = (0x7E19_0A30_0000usize, 0x7E19_0B30_0000usize);
    cratonvm_jit::register_jit_code_range(a, 0x80, cm_a);
    cratonvm_jit::register_jit_code_range(b, 0x80, cm_b);
    let sequence = [
        a + 0x10,
        b + 0x20,
        a + 0x10,
        b + 0x20,
        a + 0x10,
        b + 0x80,
        a + 0x10,
        b + 0x80,
        b + 0x20,
        b + 0x20,
        a - 1,
        a + 0x10,
        a - 1,
        a + 0x10,
    ];
    for two_way in [true, false] {
        let mut scoped = WalkLookups::walk_scoped_with(two_way);
        let mut per_call = WalkLookups::per_call();
        for addr in sequence {
            assert_eq!(
                scoped.code_range(addr),
                per_call.code_range(addr),
                "{addr:#x} two_way={two_way}"
            );
        }
        assert_eq!(scoped.code_range(a + 0x10), Some(cm_a));
        assert_eq!(scoped.code_range(b + 0x20), Some(cm_b));
    }
    cratonvm_jit::unregister_jit_code_range(a);
    cratonvm_jit::unregister_jit_code_range(b);
}

/// The safepoint id is confirmed by the index exactly as by the scan, and
/// a walk's verdicts reach the census when the walk ends.
#[test]
fn walk_scoped_bci_is_the_per_call_bci() {
    // `[rbp - 8]` holds the frame's safepoint id.
    let mut frame: [usize; 4] = [0, 7, 0, 0];
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64)
        .expect("executable buffer alloc must succeed in tests");
    buf.emit(&[0x90; 32]);
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.sp_id_slot_off = 8;
    for id in [3, 7, 11] {
        let mut m = cratonvm_jit::OopMapEntry::new(0);
        m.bytecode_pc = id;
        cm.push_oop_map(m);
    }
    let before = cratonvm_jit::compiled_frame_line_counts();
    let mut answered = 0u64;
    let mut refused = 0u64;
    for id in [7usize, 3, 11, 5, 0, 12] {
        frame[1] = id;
        let rbp = &frame[2] as *const usize as usize;
        let per_call = activation_bci_with(rbp, &cm, None, &mut WalkLookups::per_call());
        let mut scoped = WalkLookups::walk_scoped();
        let walked = activation_bci_with(rbp, &cm, None, &mut scoped);
        drop(scoped);
        assert_eq!(walked, per_call, "{id}");
        match per_call {
            Some(_) => answered += 2,
            None => refused += 2,
        }
    }
    let after = cratonvm_jit::compiled_frame_line_counts();
    // Other tests add to the process-wide census concurrently: a lower
    // bound per column.
    assert!(
        after[cratonvm_jit::FRAME_LINE_ANSWERED_SINGLE_PASS]
            - before[cratonvm_jit::FRAME_LINE_ANSWERED_SINGLE_PASS]
            >= answered
    );
    assert!(
        after[cratonvm_jit::FRAME_LINE_REFUSED_ID_UNRECORDED]
            - before[cratonvm_jit::FRAME_LINE_REFUSED_ID_UNRECORDED]
            >= refused
    );
    assert_eq!(answered, 6);
}
