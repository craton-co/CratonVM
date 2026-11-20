use super::*;
use crate::deopt::{take_last_deopt, DespecRegistry, FrameValue};
use crate::ir::IrBuilder;
use crate::{ir_optimize, ir_schedule};

/// `static int rowsum(int[][] a, int i, int n) { int s = 0; for (int j = 0;
/// j < n; j++) s += a[i][j]; return s; }`: locals `a, i, n, s, j`, the loop
/// header at bci 5, `a[i]` (`aaload`) at 14, `a[i][j]` (`iaload`) at 17.
const ROWSUM: [u8; 30] = [
    0x03, 0x3e, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1c, 0xa2, 0x00, 0x12, 0x1d, 0x2a, 0x1b, 0x32,
    0x15, 0x04, 0x2e, 0x60, 0x3e, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xee, 0x1d, 0xac, 0, 0,
];


fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the tests below reach no helper.
    unsafe { std::mem::zeroed() }
}

/// A synthetic array (`crate::tests::synthetic_array`, kept out of this
/// file so the header-shrink inventory counts emission sites only).
fn array(width: usize, elems: &[i64]) -> Vec<u8> {
    crate::tests::synthetic_array(width, elems)
}

/// The four rows `[1,2,3,4]`, `[10,20,30]`, `[5]`, `null`.
fn rows() -> (Vec<Vec<u8>>, Vec<u8>) {
    let rows = vec![
        array(4, &[1, 2, 3, 4]),
        array(4, &[10, 20, 30]),
        array(4, &[5]),
    ];
    let mut ptrs: Vec<i64> = rows.iter().map(|r| r.as_ptr() as i64).collect();
    ptrs.push(0);
    let outer = array(8, &ptrs);
    (rows, outer)
}

/// Enter at the header with the interpreter's locals `[a, i, n, s, j]`:
/// `Ok(result)` when the body ran, `Err(frame)` when the entry left for
/// the interpreter, with the frame it handed back.
fn enter(
    cm: &CompiledMethod,
    locals: &[i64; 5],
) -> Result<i32, crate::deopt::ReconstructedFrame> {
    let _ = take_last_deopt();
    // SAFETY: the body was compiled by this test for exactly these local kinds, and runs on this thread against the test's own live arrays and helper table.
    let got = unsafe { cm.ir_osr_enter(5, 0, locals) }.expect("an entry at the header");
    if got == i64::MIN {
        Err(take_last_deopt().expect("a failed entry stashes the header frame"))
    } else {
        // Cast: the low 32 bits of RAX are this method's `int` result.
        Ok(got as i32)
    }
}



// ── Round 16 wave 6 (CG16-10): the fresh-frame clear, both forms, executed ──

/// Run `zeroing` as the body of a frame of `frame_size` bytes carved on a
/// private stack poisoned with `POISON`, and return that stack and the
/// index of the frame's RBP word in it. The harness: `MOV RAX, RSP; MOV
/// RSP, arg0; PUSH RAX; PUSH RBP; MOV RBP, RSP; SUB RSP, frame_size;
/// <zeroing>; MOV RSP, RBP; POP RBP; POP RSP; RET`.
fn run_zeroing_on_poisoned_stack(zeroing: &[u8], frame_size: i32) -> (Vec<u64>, usize) {
    const POISON: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    let mut stack = vec![POISON; 1024];
    // Cast: an address, aligned down to 16 as a call would leave it.
    let end = stack.as_ptr() as usize + stack.len() * 8;
    let top = end & !15;
    let mut buf = ExecutableBuffer::new(8192).expect("executable buffer");
    buf.emit(&[0x48, 0x89, 0xE0]); // MOV RAX, RSP
    if cfg!(windows) {
        buf.emit(&[0x48, 0x89, 0xCC]); // MOV RSP, RCX
    } else {
        buf.emit(&[0x48, 0x89, 0xFC]); // MOV RSP, RDI
    }
    buf.emit(&[0x50]); // PUSH RAX
    buf.emit(&[0x55]); // PUSH RBP
    buf.emit(&[0x48, 0x89, 0xE5]); // MOV RBP, RSP
    buf.emit(&[0x48, 0x81, 0xEC]); // SUB RSP, imm32
    buf.emit(&frame_size.to_le_bytes());
    buf.emit(zeroing);
    buf.emit(&[0x48, 0x89, 0xEC]); // MOV RSP, RBP
    buf.emit(&[0x5D]); // POP RBP
    buf.emit(&[0x5C]); // POP RSP
    buf.emit(&[0xC3]); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` is a complete `extern "C" fn(u64) -> u64` that runs on
    // the private stack it is handed (writing only inside it) and restores
    // RSP and RBP before returning; it calls nothing.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    // Cast: an address into `stack`.
    f(top as u64);
    let rbp_index = (top - 16 - stack.as_ptr() as usize) / 8;
    (stack, rbp_index)
}

fn assert_frame_cleared(stack: &[u64], rbp_index: usize, frame_size: i32, form: &str) {
    // Cast: a positive multiple of 8, far below `usize::MAX`.
    let words = frame_size as usize / 8;
    for (i, w) in stack[rbp_index - words..rbp_index].iter().enumerate() {
        assert_eq!(
            *w, 0,
            "{form}: frame {frame_size}, word {i} of {words} is not zero"
        );
    }
    assert_eq!(
        stack[rbp_index - words - 1],
        0xDEAD_BEEF_DEAD_BEEF,
        "{form}: frame {frame_size}, the word below the frame was written"
    );
    assert_ne!(stack[rbp_index], 0xDEAD_BEEF_DEAD_BEEF, "the saved RBP");
}

#[test]
fn r16_both_fresh_frame_clears_zero_exactly_the_frame_on_a_poisoned_stack() {
    for frame_size in [8, 16, 24, 40, 248, 256, 264, 520, 728, 1032, 2056] {
        let mut sse = ExecutableBuffer::new(4096).expect("executable buffer");
        Lowerer::emit_zero_frame_sse(&mut sse, frame_size);
        let (stack, rbp) = run_zeroing_on_poisoned_stack(sse.as_slice(), frame_size);
        assert_frame_cleared(&stack, rbp, frame_size, "sse");

        let mut lo = super::r16_codegen_tests::emitter_lowerer();
        let at = lo.buf.pos();
        lo.emit_zero_whole_frame();
        let old = lo.buf.as_slice()[at..].to_vec();
        let (stack, rbp) = run_zeroing_on_poisoned_stack(&old, frame_size);
        assert_frame_cleared(&stack, rbp, frame_size, "loop");
    }
}

/// The straight-line form has no branch; the large form's only branch is
/// its loop's `JA`.
#[test]
fn r16_a_small_frame_clear_is_straight_line() {
    let mut small = ExecutableBuffer::new(4096).expect("executable buffer");
    Lowerer::emit_zero_frame_sse(&mut small, ZERO_FRAME_SSE_STRAIGHT_BYTES);
    assert!(
        !small.as_slice().contains(&0x77),
        "no JA: {:02x?}",
        small.as_slice()
    );
    let mut large = ExecutableBuffer::new(4096).expect("executable buffer");
    Lowerer::emit_zero_frame_sse(&mut large, 1032);
    let ja = large.as_slice().iter().filter(|&&b| b == 0x77).count();
    assert_eq!(ja, 1, "one loop branch");
}
