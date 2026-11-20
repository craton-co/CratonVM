use super::*;

/// No header store writes this pattern: a TLAB word still holding it after
/// the bump was not touched by the allocator.
const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
/// Class ids no other test registers a layout for.
const COMPACT_CID: u32 = 915_150;
const LEGACY_CID: u32 = 915_151;

fn plan(class_id: u32, num_fields: usize) -> InlineTlabPlan {
    InlineTlabPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        // Never called: a `skip_post_init` plan emits no call. Non-zero
        // only because the emitter declines a plan without one.
        post_init: 0x1000,
        context_off: 16,
        class_id,
        num_fields,
        announce: 0,
        skip_post_init: true,
        register_first: 0,
        post_init_helper_first: false,
    }
}

/// `extern "C" fn(thread_block) -> object | 0` around the fast path alone:
/// the object on the fall-through, `0` on every slow-path edge.
fn build(plan: &InlineTlabPlan) -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(1024)?;
    // PUSH RBP; MOV RBP, RSP; SUB RSP, 32. The plan's thread slot is [RBP-8].
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x20]);
    emit_store_frame(&mut buf, plan.thread_slot_off, ENTRY_ABI_REGS[0]);
    let mut slow = Vec::new();
    if !emit_inline_tlab_fast_path_ir(&mut buf, plan, 0x1000, &mut slow) {
        return None;
    }
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET -- RAX is the object
    for p in slow {
        patch_rel32_to_here(&mut buf, p);
    }
    buf.emit(&[0x31, 0xC0, 0xC9, 0xC3]); // XOR EAX, EAX; LEAVE; RET
    if buf.overflowed() {
        return None;
    }
    buf.finalize();
    Some(buf)
}

/// One allocation from a fresh 64-byte TLAB whose end is `room` bytes past
/// its start: `(object offset or None for the slow path, bytes the cursor
/// advanced, the TLAB's words afterwards)`.
fn allocate(plan: &InlineTlabPlan, room: u64) -> Option<(Option<u64>, u64, [u64; 8])> {
    let buf = build(plan)?;
    let mut tlab = Box::new([UNTOUCHED; 8]);
    let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
    let mut thread = [base, base + room];
    // SAFETY: `buf` was finalized above and holds a complete
    // `extern "C" fn(u64) -> u64`. It reads and writes the two thread
    // words and, at most, the first `room` (<= 64) bytes of `tlab`.
    let run =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    let obj = run(thread.as_mut_ptr() as u64); // Cast: the thread block's address
    let obj_off = (obj != 0).then_some(obj.wrapping_sub(base));
    Some((obj_off, thread[0].wrapping_sub(base), *tlab))
}

/// The dword at byte `off` (4-aligned) of `words`.
fn dword(words: &[u64; 8], off: usize) -> u32 {
    (words[off / 8] >> (8 * (off % 8))) as u32 // Cast: the selected half
}

/// The header word a fresh plain object carries: class id, NEUTRAL mark,
/// kind `Object`, and `flags` in the mark word's top byte.
fn header_word(class_id: u32, flags: u8) -> u64 {
    u64::from(class_id) | (u64::from(flags) << (8 * cratonvm_types::GC_FLAGS_BYTE_OFFSET))
}

#[test]
fn a_compact_instance_is_one_header_word_and_its_first_field_is_left_alone() {
    cratonvm_types::register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        COMPACT_CID,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps: vec![8],
            is_ref: vec![false],
            field_kinds: vec![cratonvm_types::FieldStorageKind::Int],
            ref_disps: Vec::new(),
            total_size: 16,
        }),
    );
    // Compact fields off, or a second layout domain in this process: the
    // bump allocates the legacy shape, which the legacy test pins.
    if cratonvm_types::compact_tlab_total_size(COMPACT_CID, 1) != Some(16) {
        return;
    }
    let (obj, used, words) = allocate(&plan(COMPACT_CID, 1), 64).expect("the bump emits");
    assert_eq!(obj, Some(0), "the object is the aligned TLAB cursor");
    assert_eq!(used, 16, "a compact instance is its layout's total_size");
    let flags = cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
    assert_eq!(words[0], header_word(COMPACT_CID, flags));
    let mark = (words[0] >> 32) as u32; // Cast: the mark word, the header word's high half
    assert!(
        cratonvm_types::ObjectHeader::is_short_mark(mark),
        "the header must read back as a COMPACT (8-byte) header: {mark:#010x}"
    );
    assert!(
        words[1] == UNTOUCHED || words[1] == 0,
        "offset 8 is a compact instance's FIRST FIELD, not a shape word; the \
         allocator wrote {:#018x} over it",
        words[1]
    );
    assert!(
        words[2..].iter().all(|&w| w == UNTOUCHED),
        "nothing past the object may be written: {words:#x?}"
    );
}

#[test]
fn a_legacy_instance_gets_the_long_header() {
    if cratonvm_types::class_layout(LEGACY_CID).is_some() {
        return;
    }
    let (obj, used, words) = allocate(&plan(LEGACY_CID, 1), 64).expect("the bump emits");
    assert_eq!(obj, Some(0));
    assert_eq!(
        used as usize, // Cast: a small byte count
        cratonvm_types::HEADER_SIZE + cratonvm_types::SLOT_SIZE,
        "a legacy instance is the long header plus one 16-byte cell"
    );
    assert_eq!(
        words[0],
        header_word(LEGACY_CID, cratonvm_types::GC_FLAG_HEADER)
    );
    let mark = (words[0] >> 32) as u32; // Cast: the mark word
    assert!(!cratonvm_types::ObjectHeader::is_short_mark(mark));
    assert_eq!(
        dword(&words, cratonvm_types::NUM_SLOTS_OFFSET),
        1,
        "the shape word carries the field count"
    );
    assert_eq!(
        dword(&words, cratonvm_types::IDENTITY_HASH_OFFSET),
        0,
        "and the hash word is written as 'no hash yet', not left stale"
    );
    assert!(
        words[4..].iter().all(|&w| w == UNTOUCHED),
        "nothing past the object may be written: {words:#x?}"
    );
}

#[test]
fn an_exhausted_tlab_takes_the_slow_path_and_writes_nothing() {
    let (obj, used, words) = allocate(&plan(LEGACY_CID, 1), 8).expect("the bump emits");
    assert_eq!(obj, None, "no room: the slow path");
    assert_eq!(used, 0, "the cursor must not move");
    assert!(words.iter().all(|&w| w == UNTOUCHED), "{words:#x?}");
}

