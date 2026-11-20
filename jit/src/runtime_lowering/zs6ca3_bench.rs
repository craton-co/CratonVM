use super::*;

const RBX: u8 = 3;
const THREAD_OFF: i32 = 16;
const CONTEXT_OFF: i32 = 24;
const LENGTH_OFF: i32 = 32;
const RESULT_OFF: i32 = 40;
const CHUNK: usize = 64 * 1024;
const ROUNDS: usize = 1500;

extern "C" fn count_announce(ctx: u64, obj: u64, _size: u64) -> u64 {
    // SAFETY: the bench passes `&mut u64` as the context word.
    unsafe { *(ctx as *mut u64) += 1 };
    obj
}

/// `extern "C" fn(thread, ctx, iterations) -> 1 | 0`: `iterations` fast-path
/// allocations, each result stored to a frame home as the IR tier does.
fn build_loop(plan: &InlineTlabArrayPlan, length: u32, pad: usize) -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(4096)?;
    // PUSH RBP; MOV RBP, RSP; PUSH RBX; SUB RSP, 56.
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x53, 0x48, 0x83, 0xEC, 0x38]);
    emit_store_frame(&mut buf, THREAD_OFF, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, CONTEXT_OFF, ENTRY_ABI_REGS[1]);
    emit_mov_reg(&mut buf, RBX, ENTRY_ABI_REGS[2]);
    // MOV QWORD [RBP - 32], imm32: the length word a run-time site reads.
    buf.emit(&[0x48, 0xC7, 0x45, 0xE0]);
    buf.emit(&length.to_le_bytes());
    // Loop-head placement: `pad` NOPs, so the bench can report each
    // variant at several code alignments (the JCC-erratum microcode on
    // Skylake-derived cores makes one layout up to 2x slower).
    while buf.pos() % 32 != 0 {
        buf.emit_byte(0x90);
    }
    for _ in 0..pad {
        buf.emit_byte(0x90);
    }
    let top = buf.pos();
    let mut slow = Vec::new();
    // zs s9/ca4: the split form when the variant asks for it -- the
    // inline arm falls through to the result home; the cold half (never
    // run here) goes after the loop.
    let mut cold: Option<ZgcAllocSplit> = None;
    if plan.announce != 0 && zgc_inline_arm_fall_through_enabled() {
        cold = Some(emit_inline_tlab_newarray_ir_split(&mut buf, plan, 0x1000)?);
    } else {
        emit_inline_tlab_newarray_ir_impl(
            &mut buf,
            plan,
            0x1000,
            0,
            Some(&mut slow),
            None,
            None,
        )?;
    }
    emit_store_frame(&mut buf, RESULT_OFF, RAX);
    buf.emit(&[0x48, 0xFF, 0xCB]); // DEC RBX
    buf.emit(&[0x0F, 0x85]); // JNZ top
    let rel = i32::try_from(top).ok()? - (i32::try_from(buf.pos()).ok()? + 4);
    buf.emit(&rel.to_le_bytes());
    buf.emit(&[0xB8, 1, 0, 0, 0]); // MOV EAX, 1
    let out = emit_jmp(&mut buf);
    if let Some(split) = cold {
        emit_inline_tlab_newarray_ir_cold_half(&mut buf, plan, 0x1000, 0, split);
        slow.push(emit_jmp(&mut buf));
    }
    for p in slow {
        patch_rel32_to_here(&mut buf, p);
    }
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    patch_rel32_to_here(&mut buf, out);
    buf.emit(&[0x48, 0x8B, 0x5D, 0xF8]); // MOV RBX, [RBP - 8]
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    if buf.overflowed() {
        return None;
    }
    buf.finalize();
    Some(buf)
}

struct Variant {
    name: &'static str,
    edits: Vec<(&'static str, Option<&'static str>)>,
    atype: u8,
    const_length: Option<u32>,
    announce: bool,
    length: u32,
    footprint: usize,
}


