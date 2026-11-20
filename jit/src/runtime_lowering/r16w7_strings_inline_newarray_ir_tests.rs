    use super::*;

    const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    /// Frame slots: thread at [RBP-8], context at [RBP-16], length at [RBP-24].
    const THREAD_OFF: i32 = 8;
    const CONTEXT_OFF: i32 = 16;
    const LENGTH_OFF: i32 = 24;
    const T_BYTE: u8 = 8;
    const T_INT: u8 = 10;

    fn plan(atype: u8, announce: usize) -> InlineTlabArrayPlan {
        InlineTlabArrayPlan {
            thread_slot_off: THREAD_OFF,
            thread_tls_disp: 0,
            cursor_off: 0,
            end_off: 8,
            context_off: CONTEXT_OFF,
            atype,
            length_off: LENGTH_OFF,
            announce,
            component_class_id: None,
            const_length: None,
        }
    }

    /// zr z3/ja: an `anewarray` plan (atype 0, a component class).
    fn ref_plan(component: u32, announce: usize) -> InlineTlabArrayPlan {
        InlineTlabArrayPlan {
            component_class_id: Some(component),
            ..plan(0, announce)
        }
    }

    /// What the stand-in announce helper saw, reached through the context slot.
    #[repr(C)]
    #[derive(Default)]
    struct Seen {
        calls: u64,
        obj: u64,
        size: u64,
    }

    extern "C" fn record_announce(ctx: u64, obj: u64, size: u64) -> u64 {
        // SAFETY: the tests pass `&mut Seen` as the context word.
        let seen = unsafe { &mut *(ctx as *mut Seen) };
        seen.calls += 1;
        seen.obj = obj;
        seen.size = size;
        obj
    }

    /// `extern "C" fn(thread, ctx, length) -> array | 0` around the fast path
    /// alone: the array on the fall-through, `0` on every slow-path edge.
    fn build(plan: &InlineTlabArrayPlan) -> Option<ExecutableBuffer> {
        let mut buf = ExecutableBuffer::new(2048)?;
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 64 (the slots, then 32 bytes of
        // Win64 shadow space below them for the announce call).
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
        emit_store_frame(&mut buf, THREAD_OFF, ENTRY_ABI_REGS[0]);
        emit_store_frame(&mut buf, CONTEXT_OFF, ENTRY_ABI_REGS[1]);
        emit_store_frame(&mut buf, LENGTH_OFF, ENTRY_ABI_REGS[2]);
        let mut slow = Vec::new();
        emit_inline_tlab_newarray_ir_impl(&mut buf, plan, 0x1000, 0, Some(&mut slow), None, None)?;
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET -- RAX is the array
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

    /// One allocation from a fresh 64-byte TLAB with `room` bytes before its
    /// end: `(array offset or None for the slow path, cursor advance, words)`.
    fn allocate(
        plan: &InlineTlabArrayPlan,
        room: u64,
        length: u64,
        seen: &mut Seen,
    ) -> Option<(Option<u64>, u64, [u64; 8])> {
        let buf = build(plan)?;
        let mut tlab = Box::new([UNTOUCHED; 8]);
        // The thread block: cursor, end, and zeroed start / owned-epoch /
        // announce words, so the inline start-bit store (when armed) reads
        // "no table" and calls the helper.
        let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
        let mut thread = [base, base + room, 0, 0, 0, 0];
        // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64, u64) -> u64`;
        // it touches the thread words, at most `room` (<= 64) bytes of `tlab`,
        // and `seen` through the announce helper.
        let run = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(buf.as_ptr())
        };
        let ctx = seen as *mut Seen as u64; // Cast: the context word
        let obj = run(thread.as_mut_ptr() as u64, ctx, length); // Cast: the thread block
        let obj_off = (obj != 0).then_some(obj.wrapping_sub(base));
        Some((obj_off, thread[0].wrapping_sub(base), *tlab))
    }

    fn mark_of(elem: cratonvm_types::ArrayElementType) -> u32 {
        cratonvm_types::ObjectHeader::new(
            cratonvm_types::ClassId::new(0),
            cratonvm_types::ObjectKind::Array,
            elem,
            0,
            0,
        )
        .mark_word
        .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn dword(words: &[u64; 8], off: usize) -> u32 {
        (words[off / 8] >> (8 * (off % 8))) as u32 // Cast: the selected half
    }

    fn assert_header(words: &[u64; 8], elem: cratonvm_types::ArrayElementType, len: u32) {
        assert_eq!(dword(words, 0), 0, "a primitive array's class id is 0");
        assert_eq!(
            dword(words, cratonvm_types::MARK_WORD_OFFSET),
            mark_of(elem)
        );
        assert_eq!(dword(words, cratonvm_types::ARRAY_LENGTH_OFFSET), len);
        assert_eq!(dword(words, cratonvm_types::IDENTITY_HASH_OFFSET), 0);
    }

    #[test]
    fn the_cap_is_the_tlab_s_own() {
        assert_eq!(
            IR_INLINE_ARRAY_TLAB_MAX_ALLOC,
            cratonvm_gc::tlab::tlab_max_alloc()
        );
    }

    #[test]
    fn a_byte_array_gets_the_whole_header_and_nothing_past_it() {
        let mut seen = Seen::default();
        let (obj, used, words) =
            allocate(&plan(T_BYTE, 0), 64, 5, &mut seen).expect("the bump emits");
        assert_eq!(obj, Some(0));
        assert_eq!(used as usize, cratonvm_types::ARRAY_DATA_OFFSET + 8); // Cast: small
        assert_header(&words, cratonvm_types::ArrayElementType::Byte, 5);
        let data_word = cratonvm_types::ARRAY_DATA_OFFSET / 8;
        assert!(
            words[data_word..].iter().all(|&w| w == UNTOUCHED),
            "the body is the refill's zeroed chunk and is not written: {words:#x?}"
        );
        assert_eq!(seen.calls, 0, "no announce on a chunk-parsing collector");
    }

    #[test]
    fn an_int_array_rounds_its_data_up_to_eight_and_reads_the_low_dword() {
        let mut seen = Seen::default();
        // High half set: the length is the low dword, as `jit_newarray` narrows.
        let (obj, used, words) = allocate(&plan(T_INT, 0), 64, 0xFFFF_FFFF_0000_0003, &mut seen)
            .expect("the bump emits");
        assert_eq!(obj, Some(0));
        assert_eq!(used as usize, cratonvm_types::ARRAY_DATA_OFFSET + 16); // Cast: small
        assert_header(&words, cratonvm_types::ArrayElementType::Int, 3);
    }

    #[test]
    fn negative_oversized_and_exhausted_take_the_slow_path_untouched() {
        let huge = (IR_INLINE_ARRAY_TLAB_MAX_ALLOC as u64) + 1; // Cast: small
        for (room, length) in [(64u64, 0xFFFF_FFFFu64), (64, huge), (16, 5)] {
            let mut seen = Seen::default();
            let (obj, used, words) =
                allocate(&plan(T_BYTE, 0), room, length, &mut seen).expect("the bump emits");
            assert_eq!(obj, None, "room={room} length={length:#x}");
            assert_eq!(used, 0, "the cursor must not move");
            assert!(words.iter().all(|&w| w == UNTOUCHED), "{words:#x?}");
        }
    }

    #[test]
    fn zgc_announces_the_array_with_its_footprint() {
        let mut seen = Seen::default();
        let helper_fn: extern "C" fn(u64, u64, u64) -> u64 = record_announce;
        let helper = helper_fn as usize;
        let (obj, used, words) =
            allocate(&plan(T_BYTE, helper), 64, 9, &mut seen).expect("the bump emits");
        assert_eq!(obj, Some(0));
        assert_header(&words, cratonvm_types::ArrayElementType::Byte, 9);
        assert_eq!(seen.calls, 1, "the announce helper ran once");
        assert_ne!(seen.obj, 0, "with the array as its object argument");
        assert_eq!(seen.size, used, "its footprint is the cursor advance");
        assert_eq!(used as usize, cratonvm_types::ARRAY_DATA_OFFSET + 16); // Cast: small
    }


    #[test]
    fn a_reference_or_unknown_atype_is_declined() {
        let mut buf = ExecutableBuffer::new(256).expect("buffer");
        let before = buf.pos();
        for atype in [0u8, 1, 3, 12] {
            assert!(emit_inline_tlab_newarray_ir(&mut buf, &plan(atype, 0), 0x1000, 0).is_none());
        }
        assert_eq!(buf.pos(), before, "a decline emits nothing");
    }

    /// zr z3/ja: an `anewarray` on ZGC bumps inline with the component class
    /// as its class word, `ref_element_size()` bytes per element rounded up to
    /// eight, the reference mark word, an untouched (zeroed-chunk = all-null)
    /// body, and the announce helper called once with the footprint.
    #[test]
    fn zr3ja_a_reference_array_on_zgc_gets_its_component_class_and_announce() {
        const COMPONENT: u32 = 915_377;
        let mut seen = Seen::default();
        let helper_fn: extern "C" fn(u64, u64, u64) -> u64 = record_announce;
        let helper = helper_fn as usize;
        let (obj, used, words) =
            allocate(&ref_plan(COMPONENT, helper), 64, 3, &mut seen).expect("the bump emits");
        assert_eq!(obj, Some(0));
        let data = (3 * cratonvm_types::narrow_oop::ref_element_size() + 7) & !7;
        assert_eq!(used as usize, cratonvm_types::ARRAY_DATA_OFFSET + data); // Cast: small
        assert_eq!(
            dword(&words, 0),
            COMPONENT,
            "the class word is the component"
        );
        assert_eq!(
            dword(&words, cratonvm_types::MARK_WORD_OFFSET),
            mark_of(cratonvm_types::ArrayElementType::Reference)
        );
        assert_eq!(dword(&words, cratonvm_types::ARRAY_LENGTH_OFFSET), 3);
        assert_eq!(dword(&words, cratonvm_types::IDENTITY_HASH_OFFSET), 0);
        let data_word = cratonvm_types::ARRAY_DATA_OFFSET / 8;
        let end_word = (used as usize) / 8; // Cast: small
        assert!(
            words[data_word..end_word].iter().all(|&w| w == UNTOUCHED),
            "the body is the refill's zeroed chunk (nulls) and is not written: {words:#x?}"
        );
        assert_eq!(seen.calls, 1, "announced once");
        assert_eq!(seen.size, used, "with the cursor advance as footprint");
        // The header the allocator itself stamps for the same array.
        let reference = cratonvm_types::ObjectHeader::new(
            cratonvm_types::ClassId::new(COMPONENT),
            cratonvm_types::ObjectKind::Array,
            cratonvm_types::ArrayElementType::Reference,
            3,
            3,
        );
        assert_eq!(
            words[0],
            reference.header_word(),
            "byte for byte the allocator's word"
        );
    }

    /// zr z3/ja: a zero-length `Object[]` is a bare header; a length past the
    /// cap, a negative one and an exhausted TLAB take the stub untouched.
    #[test]
    fn zr3ja_reference_array_edges() {
        let helper_fn: extern "C" fn(u64, u64, u64) -> u64 = record_announce;
        let helper = helper_fn as usize;
        let mut seen = Seen::default();
        let (obj, used, _) =
            allocate(&ref_plan(7, helper), 64, 0, &mut seen).expect("the bump emits");
        assert_eq!(obj, Some(0));
        assert_eq!(used as usize, cratonvm_types::ARRAY_DATA_OFFSET); // Cast: small
        let huge = (IR_INLINE_ARRAY_TLAB_MAX_ALLOC as u64) + 1; // Cast: small
        for (room, length) in [(64u64, 0xFFFF_FFFFu64), (64, huge), (16, 5)] {
            let mut seen = Seen::default();
            let (obj, used, words) =
                allocate(&ref_plan(7, helper), room, length, &mut seen).expect("emits");
            assert_eq!(obj, None, "room={room} length={length:#x}");
            assert_eq!(used, 0, "the cursor must not move");
            assert!(words.iter().all(|&w| w == UNTOUCHED), "{words:#x?}");
            assert_eq!(seen.calls, 0);
        }
    }

    /// zr z3/ja: never without an announce (a chunk-parsing collector), never
    /// without a component class, never with `CRATONVM_JIT_INLINE_TLAB_ANEWARRAY=0`.
/// JIT round 21 wave 16 (lane perf-cf-lean, `CRATONVM_JIT_IR_ALLOC_PREFETCH`):
    /// the prefetch encodings, byte for byte.
    #[test]
    fn r21w16_lean_prefetch_bytes_are_hotspot_style_one() {
        assert_eq!(
            alloc_prefetch_bytes(1, true),
            vec![0x0F, 0x0D, 0x88, 0xC0, 0x00, 0x00, 0x00],
            "PREFETCHW [RAX + 192]"
        );
        assert_eq!(
            alloc_prefetch_bytes(3, false),
            vec![
                0x0F, 0x18, 0x80, 0xC0, 0x00, 0x00, 0x00, // PREFETCHNTA [RAX + 192]
                0x0F, 0x18, 0x80, 0x00, 0x01, 0x00, 0x00, // PREFETCHNTA [RAX + 256]
                0x0F, 0x18, 0x80, 0x40, 0x01, 0x00, 0x00, // PREFETCHNTA [RAX + 320]
            ]
        );
        assert!(alloc_prefetch_bytes(0, true).is_empty());
        assert_eq!(alloc_prefetchw_supported(), alloc_prefetchw_supported(), "stable");
    }

