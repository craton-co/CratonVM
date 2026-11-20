    use super::*;

const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    /// Frame words: thread [RBP-8], context [RBP-16], receiver [RBP-24], the
    /// scratch word [RBP-40] (class word at [RBP-36]).
    const THREAD_OFF: i32 = 8;
    const CONTEXT_OFF: i32 = 16;
    const RECEIVER_OFF: i32 = 24;
    const SCRATCH_OFF: i32 = 40;
    const TLAB_WORDS: usize = 64;



    fn plan(
        shape: ArrayCloneShape,
        jvmti_word: usize,
        ref_gates: Option<RefCopyGates>,
    ) -> ArrayClonePlan {
        ArrayClonePlan {
            shape,
            bump: InlineTlabArrayPlan {
                thread_slot_off: THREAD_OFF,
                thread_tls_disp: 0,
                cursor_off: 0,
                end_off: 8,
                context_off: CONTEXT_OFF,
                atype: shape.atype,
                length_off: SCRATCH_OFF,
                announce: 0,
                component_class_id: shape.is_reference().then_some(0),
                const_length: None,
            },
            stub_target: 0x1000,
            receiver_off: RECEIVER_OFF,
            jvmti_word,
            ref_gates,
        }
    }

    /// `extern "C" fn(thread, receiver) -> copy | 0`.
    fn build(plan: &ArrayClonePlan) -> Option<ExecutableBuffer> {
        let mut buf = ExecutableBuffer::new(2048)?;
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 64.
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
        let args = super::super::ENTRY_ABI_REGS;
        for (off, reg) in [(THREAD_OFF, args[0]), (RECEIVER_OFF, args[1])] {
            let mut store = FrameAccess::new();
            enc_frame_store(reg, off, &mut store);
            buf.emit(store.as_slice());
        }
        buf.emit(&[0x48, 0x8B, 0x45, 0xE8]); // MOV RAX, [RBP-24]
        let mut decline = Vec::new();
        if !emit_array_clone(&mut buf, plan, &mut decline) {
            return None;
        }
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET -- RAX is the copy
        for p in decline {
            patch_rel32_to_here(&mut buf, p);
        }
        buf.emit(&[0x31, 0xC0, 0xC9, 0xC3]); // XOR EAX, EAX; LEAVE; RET
        assert!(!buf.overflowed());
        buf.finalize();
        Some(buf)
    }

    /// A receiver array: header (`class`, `elem`, `length`), then `payload`.
    fn receiver(class: u32, elem: ArrayElementType, length: u32, payload: &[u64]) -> Vec<u64> {
        let header = cratonvm_types::ObjectHeader::new(
            cratonvm_types::ClassId::new(class),
            cratonvm_types::ObjectKind::Array,
            elem,
            length,
            length,
        );
        let mut words = vec![header.header_word(), u64::from(length)];
        words.extend_from_slice(payload);
        words
    }

    fn address(words: &[u64]) -> u64 {
        words.as_ptr() as u64 // Cast: a test object's address
    }

    /// One clone from a fresh TLAB with `room` bytes: `(copy offset or None,
    /// cursor advance, TLAB words)`.
    fn run(buf: &ExecutableBuffer, room: u64, recv: u64) -> (Option<u64>, u64, Vec<u64>) {
        let mut tlab = vec![UNTOUCHED; TLAB_WORDS];
        let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
        let mut thread = [base, base + room, 0, 0, 0, 0];
        // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64) -> u64` that
        // reads the receiver words and the thread block and writes at most
        // `room` bytes of `tlab`; it calls nothing.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
        };
        let copy = f(thread.as_mut_ptr() as u64, recv); // Cast: the thread block
        let at = (copy != 0).then_some(copy.wrapping_sub(base));
        (at, thread[0].wrapping_sub(base), tlab)
    }

    fn bump_switches_off() -> bool {
        !crate::x64::inline_tlab_newarray_enabled()
            || !crate::x64::inline_tlab_new_enabled()
            || !crate::x64::inline_tlab_zero_elision_enabled()
    }



    fn ref_gates(pre: &u8, post: &u8, young: YoungTest) -> RefCopyGates {
        RefCopyGates {
            pre: pre as *const u8 as usize, // Cast: a gate byte's address
            post: post as *const u8 as usize, // Cast: a gate byte's address
            young,
        }
    }


