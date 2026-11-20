use super::*;
use crate::deopt::{take_last_deopt, DespecRegistry};
use crate::ir::IrBuilder;
use crate::{ir_optimize, ir_schedule};
use cratonvm_types::flags::with_thread_overrides;

const POISON: u64 = 0x5EED_F00D_5EED_F00D;
const FRAME_SIZES: [i32; 12] = [8, 16, 24, 40, 136, 248, 256, 264, 520, 728, 1032, 2056];


/// The AVX arms run only where the clear may use AVX; elsewhere the
/// switch is off by construction (`ir_zero_fresh_frames_avx_enabled`).
fn wide_forms() -> Vec<bool> {
    if fresh_frame_clear_avx_supported() {
        vec![false, true]
    } else {
        vec![false]
    }
}


/// The masks every frame size is cleared under.
fn masks(words: usize) -> Vec<(&'static str, Vec<bool>)> {
    let window_lo = words / 4;
    vec![
        ("none", vec![false; words]),
        (
            "a 16-word window",
            (0..words)
                .map(|w| w >= window_lo && w < window_lo + 16)
                .collect(),
        ),
        ("every third word", (0..words).map(|w| w % 3 == 1).collect()),
        ("alternate words", (0..words).map(|w| w % 2 == 0).collect()),
        ("the top word", (0..words).map(|w| w == 0).collect()),
        (
            "the bottom word",
            (0..words).map(|w| w + 1 == words).collect(),
        ),
        (
            "all but the middle word",
            (0..words).map(|w| w != words / 2).collect(),
        ),
        ("every word", vec![true; words]),
    ]
}


/// With nothing skipped and 16-byte stores, the word clear IS the CG16-10
/// clear, byte for byte -- which is what lets the prologue keep calling
/// `emit_zero_frame_sse` with every lane-frames switch off.
#[test]
fn r16w7_an_unskipped_sse_word_clear_is_the_cg16_10_clear_byte_for_byte() {
    for frame_size in FRAME_SIZES {
        let mut old = ExecutableBuffer::new(16384).expect("executable buffer");
        Lowerer::emit_zero_frame_sse(&mut old, frame_size);
        let mut new = ExecutableBuffer::new(16384).expect("executable buffer");
        Lowerer::emit_zero_frame_words(&mut new, frame_size, &[], false);
        assert_eq!(new.as_slice(), old.as_slice(), "frame {frame_size}");
    }
}

/// The AVX form brackets its stores with `VPXOR` (VEX.128) and
/// `VZEROUPPER`, and carries no legacy-SSE store (no `F3 0F 7F` /
/// `66 0F D6` opcode run) that would run with a dirty upper state.
#[test]
fn r16w7_the_avx_clear_is_vex_from_vpxor_to_vzeroupper() {
    for frame_size in FRAME_SIZES {
        let mut buf = ExecutableBuffer::new(16384).expect("executable buffer");
        Lowerer::emit_zero_frame_words(&mut buf, frame_size, &[], true);
        let code = buf.as_slice();
        assert!(
            code.starts_with(&[0xC5, 0xD1, 0xEF, 0xED]),
            "VPXOR first: {code:02x?}"
        );
        assert!(
            code.ends_with(&[0xC5, 0xF8, 0x77]),
            "VZEROUPPER last: {code:02x?}"
        );
        let legacy = code
            .windows(3)
            .any(|w| w == [0xF3, 0x0F, 0x7F] || w == [0x66, 0x0F, 0xD6]);
        assert!(
            !legacy,
            "frame {frame_size}: a legacy-SSE store in {code:02x?}"
        );
    }
}



/// On a pre-zeroed frame the prologue's per-word zero store is elided
/// only for a word the clear zeroed; a word the clear left out keeps it.
#[test]
fn r16w7_a_word_the_clear_left_out_keeps_its_zero_store() {
    let mut lo = super::r16_codegen_tests::emitter_lowerer();
    lo.frame_prezeroed = true;
    lo.prologue_zero_stretch = true;
    lo.fresh_clear_skip = vec![true, false];
    let at = lo.buf.pos();
    lo.emit_zero_frame_slot(16);
    assert_eq!(
        lo.buf.pos(),
        at,
        "[rbp - 16] was cleared: the store is elided"
    );
    lo.emit_zero_frame_slot(8);
    assert!(
        lo.buf.pos() > at,
        "[rbp - 8] was left out: the store is emitted"
    );
}

// ── Executed bodies: fib (prologue) and rowsum (OSR entry) ──

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the bodies here call only `guard_ok`.
    unsafe { std::mem::zeroed() }
}

/// The self-call stack guard's "enough stack" answer.
extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
    0
}

/// `static int fib(int n) { if (n < 2) return n; return fib(n - 1) +
/// fib(n - 2); }`, both invokes (pcs 10, 16) direct self-calls. Padded
/// past `code_len` (21).
const FIB: [u8; 23] = [
    0x1a, 0x05, 0xa2, 0x00, 0x05, // iload_0; iconst_2; if_icmpge +5
    0x1a, 0xac, // iload_0; ireturn
    0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic
    0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic
    0x60, 0xac, // iadd; ireturn
    0x00, 0x00,
];

fn fib_ref(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fib_ref(n - 1) + fib_ref(n - 2)
    }
}

pub(super) fn lower_fib(edits: &[(&str, Option<&str>)]) -> CompiledMethod {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/Frames",
        method_name: "fib",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 4, // the direct self-recursive route
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    with_thread_overrides(edits, || {
        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Int]);
        let mut invoke_info = HashMap::new();
        for pc in [10usize, 16] {
            invoke_info.insert(pc, (info as *const JitInvokeInfo as usize, 1usize, b'I'));
        }
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(&FIB, 21).expect("IR build");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.self_call_stack_guard = guard_ok as *const () as usize;
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            1,
            1,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("fib lowers")
    })
}

pub(super) fn run_fib(cm: &CompiledMethod, n: i64) -> i64 {
    // SAFETY: an `int f(int)` body lowered by this module; its only
    // reachable helper is `guard_ok`, and it recurses a few dozen frames
    // deep at most.
    let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
    // Cast: this tier's `int` result is the low 32 bits of RAX.
    i64::from(got as i32)
}


/// `static int rowsum(int[][] a, int i, int n)` (the r16 matrix lane's
/// OSR shape): locals `a, i, n, s, j`, the loop header at bci 5.
const ROWSUM: [u8; 30] = [
    0x03, 0x3e, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1c, 0xa2, 0x00, 0x12, 0x1d, 0x2a, 0x1b, 0x32,
    0x15, 0x04, 0x2e, 0x60, 0x3e, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xee, 0x1d, 0xac, 0, 0,
];


/// The four rows `[1,2,3,4]`, `[10,20,30]`, `[5]`, `null`.
fn rows() -> (Vec<Vec<u8>>, Vec<u8>) {
    let rows = vec![
        crate::tests::synthetic_array(4, &[1, 2, 3, 4]),
        crate::tests::synthetic_array(4, &[10, 20, 30]),
        crate::tests::synthetic_array(4, &[5]),
    ];
    let mut ptrs: Vec<i64> = rows.iter().map(|r| r.as_ptr() as i64).collect();
    ptrs.push(0);
    let outer = crate::tests::synthetic_array(8, &ptrs);
    (rows, outer)
}

fn enter(cm: &CompiledMethod, locals: &[i64; 5]) -> Option<i32> {
    let _ = take_last_deopt();
    // SAFETY: the body was compiled by this test for exactly these local kinds, and runs on this thread against the test's own live arrays and helper table.
    let got = unsafe { cm.ir_osr_enter(5, 0, locals) }.expect("an entry at the header");
    // Cast: the low 32 bits of RAX are this method's `int` result.
    (got != i64::MIN).then_some(got as i32)
}

