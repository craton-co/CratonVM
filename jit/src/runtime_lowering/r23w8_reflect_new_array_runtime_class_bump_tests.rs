use super::*;

const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
/// Frame slots: thread [RBP-8], context [RBP-16], length [RBP-24], the
/// run-time class word [RBP-32].
const THREAD_OFF: i32 = 8;
const CONTEXT_OFF: i32 = 16;
const LENGTH_OFF: i32 = 24;
const CLASS_OFF: i32 = 32;

fn plan(announce: usize) -> InlineTlabArrayPlan {
    InlineTlabArrayPlan {
        thread_slot_off: THREAD_OFF,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        context_off: CONTEXT_OFF,
        atype: 0,
        length_off: LENGTH_OFF,
        announce,
        component_class_id: Some(0),
        const_length: None,
    }
}

extern "C" fn never_called(_: u64, _: u64, _: u64) -> u64 {
    panic!("a prefix bump never calls the announce helper");
}

/// `extern "C" fn(thread, ctx, length, class) -> array | 0`: the array on
/// the fall-through, `0` on every decline edge.
fn build(plan: &InlineTlabArrayPlan) -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(2048)?;
    // PUSH RBP; MOV RBP, RSP; SUB RSP, 64.
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
    emit_store_frame(&mut buf, THREAD_OFF, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, CONTEXT_OFF, ENTRY_ABI_REGS[1]);
    emit_store_frame(&mut buf, LENGTH_OFF, ENTRY_ABI_REGS[2]);
    emit_store_frame(&mut buf, CLASS_OFF, ENTRY_ABI_REGS[3]);
    let mut decline = Vec::new();
    if !emit_inline_tlab_ref_array_runtime_class_fast_path_ir(
        &mut buf,
        plan,
        0x1000,
        CLASS_OFF,
        &mut decline,
    ) {
        return None;
    }
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET -- RAX is the array
    for p in decline {
        patch_rel32_to_here(&mut buf, p);
    }
    buf.emit(&[0x31, 0xC0, 0xC9, 0xC3]); // XOR EAX, EAX; LEAVE; RET
    assert!(!buf.overflowed());
    buf.finalize();
    Some(buf)
}

/// One allocation from a fresh 64-byte TLAB with `room` bytes before its
/// end: `(array offset or None, cursor advance, words)`.
fn allocate(
    buf: &ExecutableBuffer,
    room: u64,
    length: u64,
    class: u64,
) -> (Option<u64>, u64, [u64; 8]) {
    let mut tlab = Box::new([UNTOUCHED; 8]);
    let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
    let mut thread = [base, base + room, 0, 0, 0, 0];
    // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64, u64, u64) ->
    // u64` touching the thread words and at most `room` (<= 64) bytes of
    // `tlab`; it calls nothing.
    let run = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64, u64) -> u64>(
            buf.as_ptr(),
        )
    };
    let obj = run(thread.as_mut_ptr() as u64, 0, length, class); // Cast: the thread block
    let obj_off = (obj != 0).then_some(obj.wrapping_sub(base));
    (obj_off, thread[0].wrapping_sub(base), *tlab)
}

/// Generational / G1 shape (no announce): the header word is the one the
/// allocator stamps for `component[3]`, the body is untouched (the
/// refill's zeroed chunk), and the cursor advances by the footprint.
#[test]
fn the_run_time_class_word_is_the_allocator_s_header() {
    const COMPONENT: u32 = 731_009;
    let Some(buf) = build(&plan(0)) else {
        // A unit-test process parses the chunk (no registration), so the
        // reference bump is admitted unless a kill switch is set.
        assert!(
            cratonvm_types::jit_tlab_registration_required()
                || !ir_inline_anewarray_any_gc_enabled()
                || !crate::x64::inline_tlab_anewarray_enabled(),
            "declined with every switch on"
        );
        return;
    };
    for (class, length) in [(u64::from(COMPONENT), 3u64), (1, 0), (0xFFFF, 5)] {
        let (obj, used, words) = allocate(&buf, 64, length, class);
        assert_eq!(obj, Some(0), "class={class} length={length}");
        let data = (usize::try_from(length).expect("small")
            * cratonvm_types::narrow_oop::ref_element_size()
            + 7)
            & !7;
        assert_eq!(
            usize::try_from(used).expect("small"),
            cratonvm_types::ARRAY_DATA_OFFSET + data
        );
        let reference = cratonvm_types::ObjectHeader::new(
            cratonvm_types::ClassId::new(u32::try_from(class).expect("small")),
            cratonvm_types::ObjectKind::Array,
            cratonvm_types::ArrayElementType::Reference,
            u32::try_from(length).expect("small"),
            u32::try_from(length).expect("small"),
        );
        assert_eq!(words[0], reference.header_word(), "the header word");
        assert_eq!(
            (words[cratonvm_types::ARRAY_LENGTH_OFFSET / 8]
                >> (8 * (cratonvm_types::ARRAY_LENGTH_OFFSET % 8))) as u32, // Cast: the length dword
            u32::try_from(length).expect("small")
        );
        let data_word = cratonvm_types::ARRAY_DATA_OFFSET / 8;
        let end_word = usize::try_from(used).expect("small") / 8;
        assert!(
            words[data_word..end_word].iter().all(|&w| w == UNTOUCHED),
            "the body is not written: {words:#x?}"
        );
    }
}

/// A negative length, one past the cap and a full TLAB decline with the
/// cursor and every TLAB word untouched.
#[test]
fn every_decline_leaves_the_tlab_untouched() {
    let Some(buf) = build(&plan(0)) else {
        return;
    };
    let huge = u64::try_from(IR_INLINE_ARRAY_TLAB_MAX_ALLOC).expect("small") + 1;
    for (room, length) in [(64u64, 0xFFFF_FFFFu64), (64, huge), (16, 5), (8, 0)] {
        let (obj, used, words) = allocate(&buf, room, length, 77);
        assert_eq!(obj, None, "room={room} length={length:#x}");
        assert_eq!(used, 0, "the cursor must not move");
        assert!(words.iter().all(|&w| w == UNTOUCHED), "{words:#x?}");
    }
}



/// ZGC shape: with the thread's start-bit table absent (zeroed thread
/// words) the inline store refuses; the prefix declines with the cursor
/// NOT committed and the announce helper never called.
#[test]
fn a_zgc_refusal_declines_uncommitted_without_a_call() {
    let helper_fn: extern "C" fn(u64, u64, u64) -> u64 = never_called;
    let Some(buf) = build(&plan(helper_fn as usize)) else {
        assert!(
            !crate::x64::ir_zgc_inline_announce_enabled()
                || !crate::x64::zgc_start_bit_before_commit_enabled()
                || !crate::x64::inline_tlab_anewarray_enabled(),
            "declined with the inline announce armed"
        );
        return;
    };
    let (obj, used, _) = allocate(&buf, 64, 3, 5);
    assert_eq!(obj, None, "the refused store declines");
    assert_eq!(used, 0, "nothing committed");
}
