    use super::*;

    const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    const THREAD_OFF: i32 = 8;
    const CONTEXT_OFF: i32 = 16;
    const LENGTH_OFF: i32 = 24;
    const T_BYTE: u8 = 8;
    const T_INT: u8 = 10;
    const T_LONG: u8 = 11;
    /// A class id no other test registers a layout for: the legacy shape.
    const LEGACY_CID: u32 = 915_163;
type Outcome = (Option<u64>, u64, [u64; 10], u64);


    extern "C" fn count_announce(ctx: u64, obj: u64, _size: u64) -> u64 {
        // SAFETY: the tests pass `&mut u64` as the context word.
        unsafe { *(ctx as *mut u64) += 1 };
        obj
    }

    fn helper() -> usize {
        let f: extern "C" fn(u64, u64, u64) -> u64 = count_announce;
        f as usize
    }

    fn array_plan(atype: u8, announce: usize, const_length: Option<u32>) -> InlineTlabArrayPlan {
        InlineTlabArrayPlan {
            thread_slot_off: THREAD_OFF,
            thread_tls_disp: 0,
            cursor_off: 0,
            end_off: 8,
            context_off: CONTEXT_OFF,
            atype,
            length_off: LENGTH_OFF,
            announce,
            component_class_id: (atype == 0).then_some(7),
            const_length,
        }
    }

    fn new_plan(announce: usize) -> InlineTlabPlan {
        InlineTlabPlan {
            thread_slot_off: THREAD_OFF,
            thread_tls_disp: 0,
            cursor_off: 0,
            end_off: 8,
            post_init: 0x1000,
            context_off: CONTEXT_OFF,
            class_id: LEGACY_CID,
            num_fields: 1,
            announce,
            skip_post_init: true,
            register_first: 0,
            post_init_helper_first: false,
        }
    }

    /// `extern "C" fn(thread, ctx, length) -> object | 0` around one arm's
    /// fast path alone: the object on the fall-through, `0` on every slow edge.
    fn build(
        array: Option<&InlineTlabArrayPlan>,
        new: Option<&InlineTlabPlan>,
    ) -> Option<ExecutableBuffer> {
        let mut buf = ExecutableBuffer::new(2048)?;
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 64 (slots, then Win64 shadow space).
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
        emit_store_frame(&mut buf, THREAD_OFF, ENTRY_ABI_REGS[0]);
        emit_store_frame(&mut buf, CONTEXT_OFF, ENTRY_ABI_REGS[1]);
        emit_store_frame(&mut buf, LENGTH_OFF, ENTRY_ABI_REGS[2]);
        let mut slow = Vec::new();
        if let Some(p) = array {
            emit_inline_tlab_newarray_ir_impl(&mut buf, p, 0x1000, 0, Some(&mut slow), None, None)?;
        } else if let Some(p) = new {
            if !emit_inline_tlab_fast_path_ir(&mut buf, p, 0x1000, &mut slow) {
                return None;
            }
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

    /// One allocation with the cursor `skew` bytes into an 80-byte TLAB whose
    /// end is `room` bytes past its start; the thread block's announce word
    /// is 0, so a ZGC site's inline start-bit store takes the helper.
    /// `(object offset or None, cursor offset after, the TLAB, helper calls)`.
    fn run(buf: &ExecutableBuffer, skew: u64, room: u64, length: u64) -> Outcome {
        let mut tlab = Box::new([UNTOUCHED; 10]);
        let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
        let mut thread = [base + skew, base + room, 0, 0, 0, 0];
        let mut calls = 0u64;
        // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64, u64) -> u64`;
        // it touches the thread words, at most `room` (<= 80) bytes of `tlab`,
        // and `calls` through the announce helper.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(buf.as_ptr())
        };
        let ctx = std::ptr::addr_of_mut!(calls) as u64; // Cast: the context word
        let obj = f(thread.as_mut_ptr() as u64, ctx, length); // Cast: the thread block
        let obj_off = (obj != 0).then_some(obj.wrapping_sub(base));
        (obj_off, thread[0].wrapping_sub(base), *tlab, calls)
    }

    fn array_plans(announce: usize) -> Vec<InlineTlabArrayPlan> {
        let mut v = vec![
            array_plan(T_BYTE, announce, None),
            array_plan(T_INT, announce, None),
            array_plan(T_LONG, announce, None),
            array_plan(T_LONG, announce, Some(0)),
            array_plan(T_BYTE, announce, Some(16)),
        ];
        if announce != 0 {
            v.push(array_plan(0, announce, None));
        }
        v
    }

    /// The bytes that follow the cursor load `MOV R11, [R10 + 0]`.
    fn after_cursor_load(buf: &ExecutableBuffer) -> Vec<u8> {
        // SAFETY: `buf` holds `pos()` emitted bytes from `as_ptr()`.
        let code = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.pos()) };
        let load = [0x4D, 0x8B, 0x9A, 0, 0, 0, 0];
        let at = code
            .windows(load.len())
            .position(|w| w == load)
            .unwrap_or(0);
        code[at + load.len()..].iter().take(6).copied().collect()
    }

    fn with_edits(edits: &[(&str, Option<&str>)]) -> impl Drop {
        cratonvm_types::flags::override_thread(cratonvm_types::flags::VmFlags::from_env_with_edits(
            edits,
        ))
    }

    fn code_of(buf: &ExecutableBuffer) -> Vec<u8> {
        // SAFETY: `buf` holds `pos()` emitted bytes from `as_ptr()`.
        unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.pos()) }.to_vec()
    }

    fn count(code: &[u8], needle: &[u8]) -> usize {
        code.windows(needle.len()).filter(|w| *w == needle).count()
    }



    /// zt t2/ca: every primitive atype plus a reference array, at run-time
    /// lengths across the 8-byte grid, the TLAB end and the screen (a
    /// negative length and one past the cap take the slow path).
    fn every_run_time_array_plan(announce: usize) -> Vec<InlineTlabArrayPlan> {
        let mut v: Vec<InlineTlabArrayPlan> = (4u8..=11)
            .map(|atype| array_plan(atype, announce, None))
            .collect();
        if announce != 0 {
            v.push(array_plan(0, announce, None));
        }
        v
    }

    const ZT2CA_LENGTHS: [(u64, u64); 12] = [
        (80, 0),
        (80, 1),
        (80, 2),
        (80, 3),
        (80, 5),
        (80, 7),
        (80, 9),
        (80, 57),
        (80, 64),
        (80, 0xFFFF_FFFF_0000_0003),
        (80, 0xFFFF_FFFF),
        (16, 5),
    ];










