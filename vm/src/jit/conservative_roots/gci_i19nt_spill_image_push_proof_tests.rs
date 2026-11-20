use super::*;

fn map(sp_id: u32, complete: bool, pushed: u16) -> cratonvm_jit::OopMapEntry {
    let mut m = cratonvm_jit::OopMapEntry::new(0);
    m.bytecode_pc = sp_id;
    m.moving_young_coverage_complete = complete;
    m.shadow_pushed = pushed;
    m
}

fn compiled_method() -> cratonvm_jit::CompiledMethod {
    cratonvm_jit::CompiledMethod::new(
        cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests"),
    )
}

/// The push size is vouched for only when every map of the id claims
/// complete coverage, and it is the smallest of their counts (a prefix of
/// either push). No map at all is no claim.
#[test]
fn gci_i19nt_complete_push_count_needs_every_map_complete() {
    let a = map(7, true, 3);
    let b = map(7, true, 2);
    let c = map(7, false, 5);
    assert_eq!(complete_push_count([&a].into_iter()), Some(3));
    assert_eq!(complete_push_count([&a, &b].into_iter()), Some(2));
    assert_eq!(complete_push_count([&b, &a].into_iter()), Some(2));
    assert_eq!(complete_push_count([&a, &c].into_iter()), None);
    assert_eq!(complete_push_count([&c].into_iter()), None);
    assert_eq!(complete_push_count(std::iter::empty()), None);
}

/// A push is live only inside `[base, top)`: an overflow-tagged or
/// misaligned saved base, one below the buffer, and one whose entries end
/// above `top` (popped) prove nothing.
#[test]
fn gci_i19nt_push_entries_must_lie_inside_the_live_window() {
    let base = 0x10_0000usize;
    let top = base + 64;
    assert_eq!(push_entries_in_window(base + 16, 2, base, top), Some((base + 16, 2)));
    assert_eq!(push_entries_in_window(base + 48, 2, base, top), Some((base + 48, 2)));
    assert_eq!(push_entries_in_window(base + 56, 2, base, top), None, "popped / past top");
    assert_eq!(push_entries_in_window((base + 16) | 1, 2, base, top), None, "bail tag");
    assert_eq!(push_entries_in_window(base + 12, 1, base, top), None, "misaligned");
    assert_eq!(push_entries_in_window(base - 8, 1, base, top), None, "below the buffer");
    assert_eq!(push_entries_in_window(usize::MAX - 7, 4, base, top), None, "overflow");
}

/// The whole frame path on a fake frame, thread and shadow buffer: a value
/// the frame's live push holds is proven; an outer frame's entry below the
/// saved base, a popped push, a bailed push and an incomplete map are not.
#[test]
fn gci_i19nt_the_push_proof_reads_only_the_frames_own_live_push() {
    const SP_ID: u32 = 42;
    const SHADOW_OFF: i32 = 32;
    const W1: usize = 0x7f00_1000;
    const W2: usize = 0x7f00_2000;
    const OUTER: usize = 0x7f00_3000;
    // addr-keyed: not a heap address -- a test's fake shadow-stack buffer of made-up words, no collection runs.
    let mut buffer = vec![0usize; cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS];
    let base = buffer.as_mut_ptr() as usize;
    let end = base + buffer.len() * 8;
    buffer[0] = OUTER;
    buffer[1] = W1;
    buffer[2] = W2;
    let savebase = base + 8;
    // The fake thread: its `ShadowStack` (top, end, base) at +SHADOW_OFF.
    let mut thread = [0usize; 8];
    let thread_ptr = thread.as_mut_ptr() as usize;
    thread[4] = base + 24;
    thread[5] = end;
    thread[6] = base;
    // The frame: [rbp - 8] cached thread, [rbp - 16] safepoint id,
    // [rbp - 24] the push's saved base.
    let mut frame = [0usize; 8];
    let rbp = frame.as_mut_ptr() as usize + 8 * 8;
    frame[7] = thread_ptr;
    frame[6] = SP_ID as usize;
    frame[5] = savebase;
    let mut cm = compiled_method();
    cm.sp_id_slot_off = 16;
    cm.shadow_thread_slot_off = 8;
    cm.shadow_savebase_slot_off = 24;
    cm.shadow_off_in_thread = SHADOW_OFF;
    cm.push_oop_map(map(SP_ID, true, 2));

    let proof = SpillImagePushProof::new(rbp, &cm, true);
    assert!(proof.publishes(W1), "the push's first entry");
    assert!(proof.licenses(W1), "default ON: the proof licenses a published value");
    assert!(!proof.licenses(0x7f00_4000), "default ON: an unpublished value pins");
    assert!(proof.publishes(W2), "the push's second entry");
    assert!(!proof.publishes(OUTER), "an outer frame's entry is not this push");
    assert!(!proof.publishes(0x7f00_4000), "a value the push does not hold");

    // Popped: `top` back at the saved base.
    thread[4] = savebase;
    assert!(!SpillImagePushProof::new(rbp, &cm, true).publishes(W1), "popped push");
    thread[4] = base + 24;

    // Bailed: the overflow tag on the saved base.
    frame[5] = savebase | 1;
    assert!(!SpillImagePushProof::new(rbp, &cm, true).publishes(W1), "bailed push");
    frame[5] = savebase;

    // Another safepoint id standing in the slot: no map, no claim.
    frame[6] = (SP_ID + 1) as usize;
    assert!(!SpillImagePushProof::new(rbp, &cm, true).publishes(W1), "no map for the id");
    frame[6] = SP_ID as usize;

    // A map that does not claim complete coverage.
    let mut partial = compiled_method();
    partial.sp_id_slot_off = 16;
    partial.shadow_thread_slot_off = 8;
    partial.shadow_savebase_slot_off = 24;
    partial.shadow_off_in_thread = SHADOW_OFF;
    partial.push_oop_map(map(SP_ID, false, 2));
    assert!(
        !SpillImagePushProof::new(rbp, &partial, true).publishes(W1),
        "incomplete coverage"
    );
    // Not G1: the proof is inert and licenses every word.
    assert!(SpillImagePushProof::new(rbp, &partial, false).licenses(W1), "inert off G1");
    assert!(SpillImagePushProof::new(rbp, &partial, false).licenses(0x7f00_4000));

    // No cached thread (the push was skipped through a null thread).
    frame[7] = 0;
    assert!(!SpillImagePushProof::new(rbp, &cm, true).publishes(W1), "null cached thread");
    frame[7] = thread_ptr;
    assert!(
        SpillImagePushProof::new(rbp, &cm, true).publishes(W2),
        "restored frame proves again"
    );
    drop(buffer);
}

