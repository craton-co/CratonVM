use super::*;
use crate::x64::{ARG_REGS, RAX, RCX};

/// A leaf `extern "C" fn(i64, i64) -> i64` whose body sees its first
/// argument in RAX and its second in RCX, and returns RAX. Emitted after
/// the prologue `Compiler::new` writes, then flipped to RX and executed —
/// the same shape as `the_inline_g1_barrier_filter_...` in `x64/tests.rs`.
#[cfg(target_arch = "x86_64")]
struct Leaf {
    compiler: Compiler,
    entry: usize,
}

#[cfg(target_arch = "x86_64")]
impl Leaf {
    fn new(body: impl FnOnce(&mut Compiler)) -> Leaf {
        let mut compiler = super::super::emit::tests::test_compiler();
        let entry = compiler.buf.pos();
        // Win64: ARG_REGS[0] is RCX, so read it before RCX is overwritten.
        compiler.emit_mov_r64_r64(RAX, ARG_REGS[0]);
        compiler.emit_mov_r64_r64(RCX, ARG_REGS[1]);
        body(&mut compiler);
        compiler.emit_ret();
        assert!(!compiler.buf.overflowed(), "the leaf must fit and encode");
        crate::platform::make_executable(
            compiler.buf.as_ptr() as *mut u8,
            compiler.buf.capacity(),
        )
        .expect("the test buffer must be flippable to RX");
        Leaf { compiler, entry }
    }

    fn call(&self, a: i64, b: i64) -> i64 {
        // SAFETY: `entry` is an offset inside the live, RX buffer owned by
        // `self.compiler`; the body touches only caller-saved registers
        // (RAX/RCX/RDX, XMM0/XMM1) and no memory, and returns in RAX.
        let f: extern "C" fn(i64, i64) -> i64 =
            unsafe { std::mem::transmute(self.compiler.buf.as_ptr().add(self.entry)) };
        f(a, b)
    }
}

/// `emit_safe_idiv` against Rust's wrapping semantics, which are Java's:
/// `MIN / -1 == MIN`, `MIN % -1 == 0`, and every other divisor of -1 is a
/// negation — now taken without an IDIV at all.
#[cfg(target_arch = "x86_64")]
#[test]
fn safe_idiv_matches_java_semantics_for_every_divisor_class() {
    let div32 = Leaf::new(|c| c.emit_safe_idiv(0, false, false));
    let rem32 = Leaf::new(|c| c.emit_safe_idiv(0, false, true));
    let div64 = Leaf::new(|c| c.emit_safe_idiv(0, true, false));
    let rem64 = Leaf::new(|c| c.emit_safe_idiv(0, true, true));

    let ints = [i32::MIN, i32::MIN + 1, -7, -1, 1, 3, 7, i32::MAX];
    for &a in ints.iter() {
        for &b in ints.iter() {
            let (a64, b64) = (i64::from(a), i64::from(b));
            assert_eq!(
                div32.call(a64, b64),
                i64::from(a.wrapping_div(b)),
                "{a} / {b}"
            );
            assert_eq!(
                rem32.call(a64, b64),
                i64::from(a.wrapping_rem(b)),
                "{a} % {b}"
            );
        }
    }
    let longs = [i64::MIN, i64::MIN + 1, -7, -1, 1, 3, 7, i64::MAX, 1 << 40];
    for &a in longs.iter() {
        for &b in longs.iter() {
            assert_eq!(div64.call(a, b), a.wrapping_div(b), "{a} / {b}");
            assert_eq!(rem64.call(a, b), a.wrapping_rem(b), "{a} % {b}");
        }
    }
}

/// `CVTT*` + `emit_fp_to_int_nan_fixup` against Rust's `as` casts, which
/// saturate and send NaN to 0 exactly as JVMS §2.8.3 requires.
#[cfg(target_arch = "x86_64")]
#[test]
fn fp_to_int_fixup_matches_java_semantics() {
    // The source arrives as raw bits in RAX; MOVQ/MOVD it into XMM0.
    let d2i = Leaf::new(|c| {
        c.emit_movq_xmm_from_gpr(0, RAX);
        c.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]); // CVTTSD2SI EAX, XMM0
        c.emit_fp_to_int_nan_fixup(true, false);
        c.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
    });
    let d2l = Leaf::new(|c| {
        c.emit_movq_xmm_from_gpr(0, RAX);
        c.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]); // CVTTSD2SI RAX, XMM0
        c.emit_fp_to_int_nan_fixup(true, true);
    });
    let f2i = Leaf::new(|c| {
        c.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
        c.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]); // CVTTSS2SI EAX, XMM0
        c.emit_fp_to_int_nan_fixup(false, false);
        c.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
    });
    let f2l = Leaf::new(|c| {
        c.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
        c.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]); // CVTTSS2SI RAX, XMM0
        c.emit_fp_to_int_nan_fixup(false, true);
    });

    let doubles = [
        f64::NAN,
        -f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        1.5,
        -1.5,
        2147483647.0,
        2147483647.9,
        2147483648.0,
        -2147483648.0,
        -2147483648.9,
        -2147483649.0,
        9.2e18,
        9.3e18,
        -9.223372036854775808e18,
        -9.3e18,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
    ];
    for &x in doubles.iter() {
        // Cast: raw IEEE bits into the argument register.
        let bits = x.to_bits() as i64;
        assert_eq!(d2i.call(bits, 0), i64::from(x as i32), "d2i({x:e})");
        assert_eq!(d2l.call(bits, 0), x as i64, "d2l({x:e})");
        // Cast: f64 -> f32 rounding is the point of this sample set.
        let f = x as f32;
        let fbits = i64::from(f.to_bits());
        assert_eq!(f2i.call(fbits, 0), i64::from(f as i32), "f2i({f:e})");
        assert_eq!(f2l.call(fbits, 0), f as i64, "f2l({f:e})");
    }
}

/// JIT round 21 wave 3 (lane x64emit): `Float.floatToRawIntBits`'s int
/// result is held sign-extended, from a GPR/frame operand and from an XMM
/// one alike, and whatever sits above the float's 32 bits never reaches
/// it. `sext = false` is the `CRATONVM_JIT_X64_RAW_INT_BITS_SEXT=0` arm:
/// the old zero-extended word, kept so the control is pinned too.
#[cfg(target_arch = "x86_64")]
#[test]
fn float_raw_int_bits_result_is_a_sign_extended_int() {
    // The float's bits arrive in RCX (the leaf's second argument).
    let gpr = |sext: bool| {
        Leaf::new(move |c| c.emit_float_raw_int_bits_to_rax(StackSlot::Scratch(RCX), sext))
    };
    let xmm = |sext: bool| {
        Leaf::new(move |c| {
            c.emit_movq_xmm_from_gpr(3, RCX); // the whole word, junk above the float
            c.emit_float_raw_int_bits_to_rax(StackSlot::Xmm(3), sext);
        })
    };
    let (gpr_on, gpr_off, xmm_on, xmm_off) = (gpr(true), gpr(false), xmm(true), xmm(false));
    let floats = [-1.0f32, 1.0, -0.0, 0.0, f32::NAN, -f32::NAN, f32::MIN, f32::MAX];
    for &f in floats.iter() {
        let bits = f.to_bits();
        // Cast: the int Java's `floatToRawIntBits` answers.
        let java = i64::from(bits as i32);
        // A high half of junk, as a register or an XMM lane may carry.
        let word = (0x5A5A_5A5A_i64 << 32) | i64::from(bits);
        assert_eq!(gpr_on.call(0, word), java, "gpr floatToRawIntBits({f:e})");
        assert_eq!(xmm_on.call(0, word), java, "xmm floatToRawIntBits({f:e})");
        assert_eq!(gpr_off.call(0, word), i64::from(bits), "gpr =0 ({f:e})");
        assert_eq!(xmm_off.call(0, word), i64::from(bits), "xmm =0 ({f:e})");
    }
}

/// The flush a popped-operand FP op runs must not relocate a live XMM0
/// value INTO the scratch register an operand it has not read yet is in.
///
/// `[Xmm(0)=A, Xmm(2)=B]`: popping B frees XMM2, and a bare
/// `flush_xmm0_slots` then moves A there — the exact register B is read
/// from next. `flush_xmm0_slots_keeping(&[B])` must pick another home.
#[test]
fn flushing_xmm0_after_a_pop_never_reuses_the_popped_operands_register() {
    let mut c = super::super::emit::tests::test_compiler();
    let b_reg = SCRATCH_XMMS[0];
    c.stack_push(StackSlot::Xmm(0), false);
    c.stack_push(StackSlot::Xmm(b_reg), false);
    // As if `flush_xmm0_slots` had allocated it for B earlier.
    c.scratch_xmm_in_use |= 1;
    let b = c.pop_stack();
    assert!(matches!(b, StackSlot::Xmm(r) if r == b_reg));
    assert_eq!(c.scratch_xmm_in_use & 1, 0, "pop_stack frees B's register");
    c.flush_xmm0_slots_keeping(&[b]);
    match c.stack[0] {
        StackSlot::Xmm(r) => {
            assert_ne!(r, b_reg, "A was relocated into B's register");
            assert_ne!(r, 0, "A must have left XMM0");
        }
        StackSlot::Frame(_) => {}
        other => panic!("A relocated to an unexpected home: {other:?}"),
    }
    assert_eq!(
        c.scratch_xmm_in_use & 1,
        0,
        "the hold on B's register is released once the flush returns"
    );
}

/// Every scratch XMM busy, so the flush spills the live XMM0 value to a
/// FRAME word: that word must not be one a just-popped operand still
/// occupies (`pop_stack` handed both back to the cursor).
#[test]
fn flushing_xmm0_to_the_frame_never_reuses_a_popped_operands_word() {
    let mut c = super::super::emit::tests::test_compiler();
    c.base_spill_offset = 8;
    c.next_spill_offset = 8;
    c.spill_limit_offset = 8 + 16 * 8;
    c.stack.clear();
    c.stack_oop_marks.clear();
    c.stack_push(StackSlot::Xmm(0), false);
    // Cast: at most six scratch XMMs, so the mask fits a u8.
    c.scratch_xmm_in_use = ((1u16 << SCRATCH_XMMS.len()) - 1) as u8;
    let Some(StackSlot::Frame(off_a)) = c.push_stack() else {
        panic!("the test frame has room for the left operand");
    };
    let Some(StackSlot::Frame(off_b)) = c.push_stack() else {
        panic!("the test frame has room for the right operand");
    };
    let b = c.pop_stack();
    let a = c.pop_stack();
    assert_eq!(
        c.next_spill_offset,
        off_a.min(off_b),
        "precondition: the two pops released both operand words"
    );
    c.flush_xmm0_slots_keeping(&[a, b]);
    match c.stack[0] {
        StackSlot::Frame(off) => {
            assert_ne!(off, off_a, "XMM0 was spilled over the left operand");
            assert_ne!(off, off_b, "XMM0 was spilled over the right operand");
        }
        other => panic!("with no scratch XMM free XMM0 must go to the frame: {other:?}"),
    }
}

/// The same shape executed: `P` parked in XMM0 below two frame-homed
/// double operands with every scratch XMM taken, then `dmul`. The answer
/// is `a * b`, not `P * b` (the w5b binary's `FpNestProbe` answer).
#[cfg(target_arch = "x86_64")]
#[test]
fn a_double_binop_with_no_scratch_xmm_reads_its_own_frame_operands() {
    let parked = 3.0f64;
    for op in [0x58u8, 0x59, 0x5C, 0x5E] {
        let leaf = Leaf::new(|c| {
            open_frame(c, false);
            c.emit_mov_r64_r64(R10, RAX); // a
                                          // Cast: raw IEEE bits into an immediate.
            c.emit_mov_imm64(RAX, parked.to_bits() as i64);
            c.emit_movq_xmm_from_gpr(0, RAX);
            c.stack_push(StackSlot::Xmm(0), false);
            // Cast: at most six scratch XMMs, so the mask fits a u8.
            c.scratch_xmm_in_use = ((1u16 << SCRATCH_XMMS.len()) - 1) as u8;
            place(c, Home::Frame, R10);
            place(c, Home::Frame, RCX);
            c.emit_double_binop(op);
            c.pop_to_rax();
            let _ = c.pop_stack(); // the parked value
            let _ = c.pop_stack(); // the sentinel
            assert!(c.stack.is_empty(), "the model must be balanced");
            c.buf.emit_byte(0xC9); // LEAVE
        });
        for (a, b) in [(5.0f64, 7.0f64), (-1.5, 0.25), (1e300, 1e-300)] {
            let want = match op {
                0x58 => a + b,
                0x59 => a * b,
                0x5C => a - b,
                _ => a / b,
            };
            // Cast: raw IEEE bits through the integer argument registers.
            let got = leaf.call(a.to_bits() as i64, b.to_bits() as i64);
            // Cast: the result's raw bits back to a double.
            assert_eq!(f64::from_bits(got as u64), want, "op {op:#x} over {a}, {b}");
        }
    }
}

/// The power-of-two and magic-number division/remainder sequences against
/// Rust's `/` and `%` (truncating, remainder takes the dividend's sign —
/// JVMS `idiv`/`irem`/`ldiv`/`lrem`). Executed, one leaf per divisor.
#[cfg(target_arch = "x86_64")]
#[test]
fn constant_division_sequences_match_java_semantics() {
    let longs: [i64; 12] = [
        0,
        1,
        -1,
        5,
        -5,
        7,
        -8,
        1 << 40,
        -(1 << 40) - 3,
        i64::MAX,
        i64::MIN,
        i64::MIN + 1,
    ];
    for d in [1i64, 2, 4, 8, 1 << 20, 1 << 31] {
        let div = Leaf::new(|c| c.emit_ldiv_pow2(d));
        let rem = Leaf::new(|c| c.emit_lrem_pow2(d));
        for &n in longs.iter() {
            assert_eq!(div.call(n, 0), n / d, "{n} / {d}");
            assert_eq!(rem.call(n, 0), n % d, "{n} % {d}");
        }
    }
    for d in [3i64, 7, 10, 1000, 7919] {
        let (magic, shift) = Compiler::magic_signed_div64(d);
        let div = Leaf::new(|c| c.emit_ldiv_magic64(magic, shift));
        let rem = Leaf::new(|c| c.emit_lrem_magic64(magic, shift, d));
        for &n in longs.iter() {
            assert_eq!(div.call(n, 0), n / d, "{n} / {d}");
            assert_eq!(rem.call(n, 0), n % d, "{n} % {d}");
        }
    }
    let ints: [i32; 10] = [0, 1, -1, 5, -5, 7, -8, i32::MAX, i32::MIN, i32::MIN + 1];
    // (Not 1: `emit_idiv_pow2(1)` is a no-op on an already-canonical RAX.)
    for d in [2i32, 4, 16, 1 << 30] {
        let div = Leaf::new(|c| c.emit_idiv_pow2(d));
        let rem = Leaf::new(|c| c.emit_irem_pow2(d));
        for &n in ints.iter() {
            // Dirty upper half: the 32-bit sequences must read EAX only.
            let arg = i64::from(n) ^ (0x5A5A_5A5A_i64 << 32);
            assert_eq!(div.call(arg, 0), i64::from(n / d), "{n} / {d}");
            assert_eq!(rem.call(arg, 0), i64::from(n % d), "{n} % {d}");
        }
    }
    // r11 x64gen: the 32-bit MAGIC sequences had no executed test, and
    // since `ldc`/`ldc_w` int constants reach `try_const_arith_peephole`
    // the divisor range is the whole positive `int`, up to `i32::MAX`
    // (`x % 1_000_000_007`), not just what fits a `sipush`.
    for d in [
        3i32,
        7,
        10,
        641,
        1000,
        100_000,
        1_000_000_007,
        i32::MAX - 1,
        i32::MAX,
    ] {
        let (magic, shift) = Compiler::magic_signed_div32(d);
        let div = Leaf::new(|c| c.emit_idiv_magic(magic, shift));
        let rem = Leaf::new(|c| c.emit_irem_magic(magic, shift, d));
        for &n in ints.iter() {
            let arg = i64::from(n) ^ (0x5A5A_5A5A_i64 << 32);
            assert_eq!(div.call(arg, 0), i64::from(n / d), "{n} / {d}");
            assert_eq!(rem.call(arg, 0), i64::from(n % d), "{n} % {d}");
        }
    }
}

/// Round 11 wave 3 (x64gen): `emit_div_by_const` for every sign and
/// magnitude class of constant divisor, executed against Rust's wrapping
/// `/` and `%` (Java's), including `MIN / -1` and the two divisors whose
/// magic numbers the pre-fix loop test got wrong, `2^30 + 1` and
/// `2^62 + 1` (`(1 << 30) / 1073741825` came out 1).
#[cfg(target_arch = "x86_64")]
#[test]
fn constant_divisors_of_every_sign_reduce_without_idiv() {
    let ints: [i32; 12] = [
        0,
        1,
        -1,
        5,
        -5,
        7,
        -8,
        1 << 30,
        -(1 << 30),
        i32::MAX,
        i32::MIN,
        i32::MIN + 1,
    ];
    let int_divisors: [i32; 18] = [
        1,
        -1,
        2,
        -2,
        3,
        -3,
        7,
        -7,
        8,
        -8,
        31,
        -31,
        1 << 30,
        -(1 << 30),
        (1 << 30) + 1,
        -((1 << 30) + 1),
        i32::MAX,
        -i32::MAX,
    ];
    for d in int_divisors {
        let div = Leaf::new(|c| assert!(c.emit_div_by_const(i64::from(d), false, false)));
        let rem = Leaf::new(|c| assert!(c.emit_div_by_const(i64::from(d), false, true)));
        for &n in ints.iter() {
            // Dirty upper half: the 32-bit sequences must read EAX only.
            let arg = i64::from(n) ^ (0x5A5A_5A5A_i64 << 32);
            assert_eq!(div.call(arg, 0), i64::from(n.wrapping_div(d)), "{n} / {d}");
            assert_eq!(rem.call(arg, 0), i64::from(n.wrapping_rem(d)), "{n} % {d}");
        }
    }
    let longs: [i64; 12] = [
        0,
        1,
        -1,
        5,
        -5,
        1 << 40,
        1 << 62,
        -(1 << 62),
        -(1 << 40) - 3,
        i64::MAX,
        i64::MIN,
        i64::MIN + 1,
    ];
    let long_divisors: [i64; 18] = [
        1,
        -1,
        2,
        -2,
        10,
        -10,
        1 << 31,
        -(1 << 31),
        1 << 40,
        -(1 << 40),
        1_000_000_007,
        -1_000_000_007,
        (1 << 62) + 1,
        -((1 << 62) + 1),
        (1 << 30) + 1,
        -3,
        i64::MAX,
        -i64::MAX,
    ];
    for d in long_divisors {
        let div = Leaf::new(|c| assert!(c.emit_div_by_const(d, true, false)));
        let rem = Leaf::new(|c| assert!(c.emit_div_by_const(d, true, true)));
        for &n in longs.iter() {
            assert_eq!(div.call(n, 0), n.wrapping_div(d), "{n} / {d}");
            assert_eq!(rem.call(n, 0), n.wrapping_rem(d), "{n} % {d}");
        }
    }
    // No sequence for zero or for the width's MIN_VALUE, and an `int`
    // divisor outside the `int` range is not an `int` divisor.
    assert!(!Compiler::const_divisor_is_reducible(0, false));
    assert!(!Compiler::const_divisor_is_reducible(0, true));
    assert!(!Compiler::const_divisor_is_reducible(
        i64::from(i32::MIN),
        false
    ));
    assert!(!Compiler::const_divisor_is_reducible(i64::MIN, true));
    assert!(!Compiler::const_divisor_is_reducible(1 << 31, false));
    // The magic numbers themselves, for the two edge divisors: Hacker's
    // Delight's (M, s), not the early-exit (4, 0) both used to return.
    assert_eq!(
        Compiler::magic_signed_div32((1 << 30) + 1),
        (0x7FFF_FFFF, 29)
    );
    assert_eq!(Compiler::magic_signed_div64((1 << 62) + 1), (i64::MAX, 61));
}

// -------------------------------------------------------------------
// Operand-stack integer ALU lowering (round 9 wave 5, lane spstack5)
// -------------------------------------------------------------------
//
// Executed: every helper is driven through the real operand-stack model
// with its operands homed in each place the walk can leave them — a
// scratch register, a register standing in for a local's home, a frame
// word (whose slot mirror is live when it was stored last) — with the
// pure-kernel operand cache on and off, and the answer is compared with
// Rust's wrapping arithmetic, which is Java's. A value parked UNDER the
// operands (`SENTINEL` in R11) must come out untouched: the in-place
// lowering may only overwrite registers it popped.

use super::super::operand_stack::operand_fold_enabled as fold_on;

/// Where a test operand lives. `Reg10` stands in for a callee-saved local
/// home: R10 is caller-saved on both ABIs, so a leaf may clobber it, and
/// the model does not care which register a `CalleeSaved` entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    Scratch8,
    Scratch9,
    Reg10,
    Frame,
}

const HOMES: [Home; 4] = [Home::Scratch8, Home::Scratch9, Home::Reg10, Home::Frame];

/// What R11 holds under the operands for the whole body.
const SENTINEL: i64 = 0x5EED_0000_1234_5678;

const OPS: [GprAlu; 6] = [
    GprAlu::Add,
    GprAlu::Sub,
    GprAlu::And,
    GprAlu::Or,
    GprAlu::Xor,
    GprAlu::Imul,
];

/// `push rbp; mov rbp, rsp; sub rsp, 0x100`, and a spill window of 16
/// words ([rbp-8] ..= [rbp-0x80]) for `push_stack`, inside that frame.
#[cfg(target_arch = "x86_64")]
fn open_frame(c: &mut Compiler, cache: bool) {
    c.buf.emit(&[
        0x55, 0x48, 0x89, 0xE5, 0x48, 0x81, 0xEC, 0x00, 0x01, 0x00, 0x00,
    ]);
    c.base_spill_offset = 8;
    c.next_spill_offset = 8;
    c.spill_limit_offset = 8 + 16 * 8;
    c.stack.clear();
    c.stack_oop_marks.clear();
    c.kernel_operand_cache = cache;
    c.emit_mov_imm64(R11, SENTINEL);
    c.stack_push(StackSlot::CalleeSaved(R11), false);
}

/// Pop the result (RAX) and the sentinel (RCX), fold them into one
/// answer (`result ^ sentinel`), and tear the frame down.
#[cfg(target_arch = "x86_64")]
fn close_frame(c: &mut Compiler) {
    c.pop_to_rax();
    c.pop_to_rcx();
    c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
    assert!(c.stack.is_empty(), "the model must be balanced");
    c.buf.emit_byte(0xC9); // LEAVE
}

/// Put the value in `src` on the operand stack, homed at `home`.
#[cfg(target_arch = "x86_64")]
fn place(c: &mut Compiler, home: Home, src: u8) {
    match home {
        Home::Scratch8 => {
            c.emit_mov_r64_r64(R8, src);
            c.stack_push(StackSlot::Scratch(R8), false);
        }
        Home::Scratch9 => {
            c.emit_mov_r64_r64(R9, src);
            c.stack_push(StackSlot::Scratch(R9), false);
        }
        Home::Reg10 => {
            c.emit_mov_r64_r64(R10, src);
            c.stack_push(StackSlot::CalleeSaved(R10), false);
        }
        Home::Frame => {
            let Some(StackSlot::Frame(off)) = c.push_stack() else {
                panic!("the test frame has room for every operand");
            };
            c.emit_store_local(off, src);
        }
    }
}

/// Java's answer: wrapping 32-bit for an int op (sign-extended, as the
/// backend holds ints), wrapping 64-bit for a long op.
fn alu_ref(op: GprAlu, wide: bool, a: i64, b: i64) -> i64 {
    if wide {
        match op {
            GprAlu::Add => a.wrapping_add(b),
            GprAlu::Sub => a.wrapping_sub(b),
            GprAlu::And => a & b,
            GprAlu::Or => a | b,
            GprAlu::Xor => a ^ b,
            GprAlu::Imul => a.wrapping_mul(b),
        }
    } else {
        // Cast: truncation to the int operand is the point.
        let (x, y) = (a as i32, b as i32);
        i64::from(match op {
            GprAlu::Add => x.wrapping_add(y),
            GprAlu::Sub => x.wrapping_sub(y),
            GprAlu::And => x & y,
            GprAlu::Or => x | y,
            GprAlu::Xor => x ^ y,
            GprAlu::Imul => x.wrapping_mul(y),
        })
    }
}

const INTS: [i64; 8] = [
    0,
    1,
    -1,
    7,
    -8,
    0x1234_5678,
    i32::MAX as i64,
    i32::MIN as i64,
];
const LONGS: [i64; 8] = [
    0,
    1,
    -1,
    1 << 40,
    -(1 << 33) + 5,
    i64::MAX,
    i64::MIN,
    0x0123_4567_89AB_CDEF,
];

/// `emit_gpr_binop` over every pair of operand homes, both widths, with
/// the operand cache on and off. `(Reg10, Reg10)` is `iload x; iload x`:
/// two entries naming one register, the same value.
#[cfg(target_arch = "x86_64")]
#[test]
fn gpr_binop_reads_every_operand_home_and_matches_java() {
    for op in OPS {
        for wide in [false, true] {
            for cache in [false, true] {
                for a_home in HOMES {
                    for b_home in HOMES {
                        let same_reg = a_home == b_home && a_home != Home::Frame;
                        if same_reg && a_home != Home::Reg10 {
                            // Two entries in ONE scratch register is not a
                            // state the walk creates (`dup` gives the copy
                            // its own home).
                            continue;
                        }
                        let leaf = Leaf::new(|c| {
                            open_frame(c, cache);
                            place(c, a_home, RAX);
                            if same_reg {
                                let top = c.peek_stack();
                                c.stack_push(top, false);
                            } else {
                                place(c, b_home, RCX);
                            }
                            assert!(!c.emit_gpr_binop(op, wide, None), "no store to fuse");
                            close_frame(c);
                        });
                        let vals = if wide { LONGS } else { INTS };
                        for &a in vals.iter() {
                            for &b in vals.iter() {
                                let b = if same_reg { a } else { b };
                                assert_eq!(
                                    leaf.call(a, b) ^ SENTINEL,
                                    alu_ref(op, wide, a, b),
                                    "{op:?} wide={wide} cache={cache} a@{a_home:?}={a} b@{b_home:?}={b}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `x = x OP y` with `x` register-homed: the binop writes the home and the
/// store is consumed — in place for the left operand always, for the right
/// one when the op commutes, and (wave 7) for the right operand of a `sub`
/// through a work register and a move. With a copy of `x`'s OLD value
/// still on the stack (`x + (x = x OP y)`-shaped), it must NOT fuse, and
/// the copy must still read the old value.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_store_to_the_left_operands_home_is_fused_only_when_nothing_else_reads_it() {
    for op in OPS {
        for wide in [false, true] {
            for cache in [false, true] {
                for y_home in [Home::Scratch8, Home::Frame] {
                    for local_is_right in [false, true] {
                        for old_copy_below in [false, true] {
                            let mut fused = None;
                            let leaf = Leaf::new(|c| {
                                open_frame(c, cache);
                                // x lives in R10, its home.
                                c.emit_mov_r64_r64(R10, RAX);
                                if old_copy_below {
                                    c.stack_push(StackSlot::CalleeSaved(R10), false);
                                }
                                if local_is_right {
                                    place(c, y_home, RCX);
                                    c.stack_push(StackSlot::CalleeSaved(R10), false);                                    } else {
                                    c.stack_push(StackSlot::CalleeSaved(R10), false);
                                    place(c, y_home, RCX);                                    }
                                let f = c.emit_gpr_binop(op, wide, Some(R10));
                                if f {
                                    // What `istore x` would have left: the
                                    // new value in the home. Read it back.
                                    c.emit_mov_r64_r64(RAX, R10);
                                } else {
                                    c.pop_to_rax();
                                }
                                if old_copy_below {
                                    // RDX = the copy of x's OLD value.
                                    let old = c.pop_stack();
                                    c.load_slot_to_reg(RDX, old);
                                }
                                c.pop_to_rcx(); // the sentinel
                                c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
                                if old_copy_below {
                                    // Fold the old copy in too: rax ^= rdx.
                                    c.emit_alu_reg_src(
                                        GprAlu::Xor,
                                        RAX,
                                        AluSrc::Reg(RDX),
                                        true,
                                    );
                                }
                                assert!(c.stack.is_empty());
                                c.buf.emit_byte(0xC9); // LEAVE
                                fused = Some(f);
                            });
                            let fused = fused.expect("the body ran");
                            // Wave 7: the right operand of a `sub` is
                            // fused too (computed elsewhere, then moved
                            // into the home) -- still never with a copy.
                            let expect_fused = fold_on() && !old_copy_below;
                            assert_eq!(
                                fused, expect_fused,
                                "{op:?} wide={wide} right={local_is_right} copy={old_copy_below}"
                            );
                            let vals = if wide { LONGS } else { INTS };
                            for &x in vals.iter() {
                                for &y in vals.iter() {
                                    let want = if local_is_right {
                                        alu_ref(op, wide, y, x)
                                    } else {
                                        alu_ref(op, wide, x, y)
                                    };
                                    let want = if old_copy_below { want ^ x } else { want };
                                    assert_eq!(
                                        leaf.call(x, y) ^ SENTINEL,
                                        want,
                                        "{op:?} wide={wide} cache={cache} y@{y_home:?} \
                                         right={local_is_right} copy={old_copy_below} x={x} y={y}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Wave 7: `y = a OP b` with `y` register-homed (RDX here) and NEITHER
/// operand: the result is computed where 2/3 of `emit_gpr_binop` put it
/// and moved into the home (`movsxd home, r32` for an int), the store
/// consumed. The home starts with a stale value whose upper half is
/// dirty, so a 32-bit write to it would be caught.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_store_to_a_home_that_is_not_an_operand_takes_the_result_directly() {
    for op in OPS {
        for wide in [false, true] {
            for cache in [false, true] {
                for a_home in HOMES {
                    for b_home in HOMES {
                        if a_home == b_home && a_home != Home::Frame {
                            continue;
                        }
                        let mut fused = None;
                        let leaf = Leaf::new(|c| {
                            open_frame(c, cache);
                            c.emit_mov_imm64(RDX, 0x7777_0000_8000_0001);
                            place(c, a_home, RAX);
                            place(c, b_home, RCX);
                            let f = c.emit_gpr_binop(op, wide, Some(RDX));
                            if f {
                                c.emit_mov_r64_r64(RAX, RDX);
                            } else {
                                c.pop_to_rax();
                            }
                            c.pop_to_rcx(); // the sentinel
                            c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
                            assert!(c.stack.is_empty());
                            c.buf.emit_byte(0xC9); // LEAVE
                            fused = Some(f);
                        });
                        assert_eq!(
                            fused.expect("the body ran"),
                            fold_on(),
                            "{op:?} wide={wide} cache={cache} a@{a_home:?} b@{b_home:?}"
                        );
                        let vals = if wide { LONGS } else { INTS };
                        for &a in vals.iter() {
                            for &b in vals.iter() {
                                assert_eq!(
                                    leaf.call(a, b) ^ SENTINEL,
                                    alu_ref(op, wide, a, b),
                                    "{op:?} wide={wide} cache={cache} \
                                     a@{a_home:?}={a} b@{b_home:?}={b}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `i2l` / `l2i` (`emit_sext32_top`) from every home. The input's upper
/// half is dirty on purpose: only the low 32 bits may be read.
#[cfg(target_arch = "x86_64")]
#[test]
fn sign_extension_reads_only_the_low_half_from_every_home() {
    for cache in [false, true] {
        for home in HOMES {
            let leaf = Leaf::new(|c| {
                open_frame(c, cache);
                place(c, home, RAX);
                c.emit_sext32_top();
                close_frame(c);
            });
            for &v in LONGS.iter().chain(INTS.iter()) {
                // Cast: truncation to the low half is the point.
                let want = i64::from(v as i32);
                assert_eq!(
                    leaf.call(v, 0) ^ SENTINEL,
                    want,
                    "cache={cache} {home:?} {v:#x}"
                );
            }
        }
    }
}

/// The constant arms of `try_const_arith_peephole` (int) and
/// `try_const_arith_peephole_long`, on an operand in each home: in place
/// on a scratch operand, the historical RAX encodings otherwise.
#[cfg(target_arch = "x86_64")]
#[test]
fn constant_alu_arms_compute_in_place_and_match_java() {
    let alu_consts = [0i32, 1, -1, 5, 127, 128, -129, 0x7FFF_FFFF, i32::MIN];
    for cache in [false, true] {
        for home in HOMES {
            let mut cases: Vec<(u8, i32)> = Vec::new();
            for o in [0x60u8, 0x64, 0x7e, 0x80, 0x82] {
                for k in alu_consts {
                    cases.push((o, k));
                }
            }
            for o in [0x78u8, 0x7a, 0x7c] {
                for k in [0i32, 1, 5, 31] {
                    cases.push((o, k));
                }
            }
            // imul: the three-operand form (wave 7) and, for 3/16/-1, the
            // strength-reduced RAX forms it leaves alone.
            for k in [31i32, -7, 6, 1000, 0x10001, i32::MIN, 3, 16, -1] {
                cases.push((0x68, k));
            }
            for (opcode, k) in cases {
                let leaf = Leaf::new(|c| {
                    open_frame(c, cache);
                    c.pc_to_native = vec![0; 2];
                    place(c, home, RAX);
                    assert!(c.try_const_arith_peephole(k, 0, &[opcode], 1, &[false]));
                    close_frame(c);
                });
                for &v in INTS.iter() {
                    // Cast: the int operand.
                    let x = v as i32;
                    let want = i64::from(match opcode {
                        0x60 => x.wrapping_add(k),
                        0x64 => x.wrapping_sub(k),
                        0x7e => x & k,
                        0x80 => x | k,
                        0x82 => x ^ k,
                        0x68 => x.wrapping_mul(k),
                        0x78 => x.wrapping_shl(k as u32), // Cast: 0..=31
                        0x7a => x.wrapping_shr(k as u32), // Cast: 0..=31
                        // Cast: logical shift on the unsigned bits.
                        _ => ((x as u32) >> (k as u32)) as i32,
                    });
                    assert_eq!(
                        leaf.call(v, 0) ^ SENTINEL,
                        want,
                        "op={opcode:#x} k={k} cache={cache} {home:?} x={x}"
                    );
                }
            }
            let mut long_cases: Vec<(u8, i64)> = Vec::new();
            for o in [0x61u8, 0x65] {
                for k in [0i64, 1, -1, 1000, -129, 0x7FFF_FFFF] {
                    long_cases.push((o, k));
                }
            }
            // lshl / lshr / lushr by an int constant (wave 7): the count's
            // low six bits, a zero count leaving the value as it is.
            for opcode in [0x79u8, 0x7b, 0x7d] {
                for k in [0i32, 1, 3, 31, 32, 63, 64, -1, 100] {
                    let leaf = Leaf::new(|c| {
                        open_frame(c, cache);
                        c.pc_to_native = vec![0; 2];
                        place(c, home, RAX);
                        assert_eq!(
                            c.try_const_arith_peephole(k, 0, &[opcode], 1, &[false]),
                            fold_on()
                        );
                        // Under the kill switch nothing is fused and the
                        // operand itself is the answer (checked below).
                        close_frame(c);
                    });
                    // Cast: the count's low six bits, as JVMS lshl/lshr/lushr.
                    let n = (k & 0x3f) as u32;
                    for &v in LONGS.iter() {
                        let want = if !fold_on() {
                            v
                        } else {
                            match opcode {
                                0x79 => v.wrapping_shl(n),
                                0x7b => v.wrapping_shr(n),
                                // Cast: logical shift on the unsigned bits.
                                _ => ((v as u64) >> n) as i64,
                            }
                        };
                        assert_eq!(
                            leaf.call(v, 0) ^ SENTINEL,
                            want,
                            "op={opcode:#x} k={k} cache={cache} {home:?} v={v:#x}"
                        );
                    }
                }
            }
            for (opcode, k) in long_cases {
                let leaf = Leaf::new(|c| {
                    open_frame(c, cache);
                    c.pc_to_native = vec![0; 2];
                    place(c, home, RAX);
                    assert!(c.try_const_arith_peephole_long(k, 0, &[opcode], 1, &[false]));
                    close_frame(c);
                });
                for &v in LONGS.iter() {
                    let want = if opcode == 0x61 {
                        v.wrapping_add(k)
                    } else {
                        v.wrapping_sub(k)
                    };
                    assert_eq!(
                        leaf.call(v, 0) ^ SENTINEL,
                        want,
                        "op={opcode:#x} k={k} cache={cache} {home:?} v={v}"
                    );
                }
            }
        }
    }
}

/// The shapes the page asked for, byte for byte, with the cache on:
/// `s += x` (s in R13, x cached in R8) is ONE `add r13, r8`, and an int
/// add of a cached operand and a local home is `add r8d, r13d;
/// movsxd r8, r8d` with the result left in R8 — no RAX/RCX round trip.
#[test]
fn the_scalar_loop_shapes_emit_no_round_trip() {
    if !fold_on() {
        return; // the kill switch is set in this process
    }
    let emit = |f: &dyn Fn(&mut Compiler)| {
        let mut c = super::super::emit::tests::test_compiler();
        c.kernel_operand_cache = true;
        c.stack.clear();
        c.stack_oop_marks.clear();
        let start = c.buf.pos();
        f(&mut c);
        (c.buf.as_slice()[start..].to_vec(), c.stack.clone())
    };
    // lload s (R13); <x in R8>; ladd; lstore s
    let (bytes, stack) = emit(&|c: &mut Compiler| {
        c.stack_push(StackSlot::CalleeSaved(R13), false);
        c.stack_push(StackSlot::Scratch(R8), false);
        assert!(c.emit_gpr_binop(GprAlu::Add, true, Some(R13)));
    });
    assert_eq!(bytes, vec![0x4D, 0x03, 0xE8], "add r13, r8");
    assert!(stack.is_empty());
    // <x in R8>; iload s (R13); iadd  -> in place in R8
    let (bytes, stack) = emit(&|c: &mut Compiler| {
        c.stack_push(StackSlot::Scratch(R8), false);
        c.stack_push(StackSlot::CalleeSaved(R13), false);
        assert!(!c.emit_gpr_binop(GprAlu::Add, false, None));
    });
    assert_eq!(
        bytes,
        vec![0x45, 0x03, 0xC5, 0x4D, 0x63, 0xC0],
        "add r8d, r13d; movsxd r8, r8d"
    );
    assert!(matches!(stack.as_slice(), [StackSlot::Scratch(r)] if *r == R8));
    // i2l of a cached operand: one movsxd in place.
    let (bytes, stack) = emit(&|c: &mut Compiler| {
        c.stack_push(StackSlot::Scratch(R9), false);
        c.emit_sext32_top();
    });
    assert_eq!(bytes, vec![0x4D, 0x63, 0xC9], "movsxd r9, r9d");
    assert!(matches!(stack.as_slice(), [StackSlot::Scratch(r)] if *r == R9));
    // A frame operand folds into the instruction as a memory operand.
    let (bytes, _) = emit(&|c: &mut Compiler| {
        c.stack_push(StackSlot::CalleeSaved(R12), false);
        c.stack_push(StackSlot::Frame(0x30), false);
        c.slot_mirror = None;
        assert!(!c.emit_gpr_binop(GprAlu::Sub, true, None));
    });
    // mov r8, r12; sub r8, [rbp-0x30]
    assert_eq!(bytes, vec![0x4D, 0x8B, 0xC4, 0x4C, 0x2B, 0x45, 0xD0]);
}

/// Wave 7: `skip_redundant_i2l` consumes the `i2l` after a producer only
/// where nothing else can arrive at it, and never emits anything.
#[test]
fn a_redundant_i2l_is_consumed_only_where_nothing_else_arrives() {
    if !fold_on() {
        return; // the kill switch is set in this process
    }
    let run = |code: &[u8], targets: &[bool], push: bool, eager: Option<usize>| {
        let mut c = super::super::emit::tests::test_compiler();
        c.stack.clear();
        c.stack_oop_marks.clear();
        c.pc_to_native = vec![-1; 4];
        c.deopt_eager_bci = eager;
        if push {
            c.stack_push(StackSlot::Scratch(R8), false);
        }
        let start = c.buf.pos();
        let next = c.skip_redundant_i2l(code, code.len(), 1, targets);
        let native = c.pc_to_native[1];
        // Cast: a test buffer offset, far below i32::MAX.
        (next, c.buf.pos() - start, native == c.buf.pos() as i32)
    };
    // iaload; i2l; ladd
    let code = [0x2eu8, 0x85, 0x61];
    let none = [false; 3];
    assert_eq!(run(&code, &none, true, None), (2, 0, true), "consumed");
    let target = [false, true, false];
    assert_eq!(run(&code, &target, true, None).0, 1, "a branch target");
    assert_eq!(
        run(&[0x2e, 0x00, 0x61], &none, true, None).0,
        1,
        "not an i2l"
    );
    assert_eq!(run(&code, &none, false, None).0, 1, "nothing pushed");
    assert_eq!(
        run(&code, &none, true, Some(1)).0,
        1,
        "a debug deopt trigger"
    );
    assert_eq!(run(&code[..1], &none, true, None).0, 1, "past the end");
}

/// `emit_affine_fold` replaces a balanced `iload x ... istore x` run, so
/// it must honour what the `istore` did: an entry pushed EARLIER by
/// `iload x` (a `CalleeSaved` copy of the local's register) still reads
/// the OLD value after the fold writes the register.
#[cfg(target_arch = "x86_64")]
#[test]
fn the_affine_fold_keeps_an_earlier_copy_of_the_local() {
    let leaf = Leaf::new(|c| {
        open_frame(c, false);
        c.local_assignments = vec![Some(R10)];
        c.emit_mov_r64_r64(R10, RAX); // x = arg0
        c.stack_push(StackSlot::CalleeSaved(R10), false); // iload x (earlier)
        c.emit_affine_fold(0, 3, 5); // x = x * 3 + 5
        let old = c.pop_stack();
        c.load_slot_to_reg(RDX, old);
        c.emit_mov_r64_r64(RAX, R10);
        c.pop_to_rcx(); // the sentinel
        c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
        c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RDX), true);
        assert!(c.stack.is_empty());
        c.buf.emit_byte(0xC9); // LEAVE
    });
    for &x in INTS.iter() {
        // Cast: the int local.
        let xi = x as i32;
        let new = i64::from(xi.wrapping_mul(3).wrapping_add(5));
        assert_eq!(leaf.call(x, 0) ^ SENTINEL, new ^ x, "x={x}");
    }
}

// -------------------------------------------------------------------
// Conversions read their operand in place (round 9 wave 6, x64misc6)
// -------------------------------------------------------------------

/// `i2f` `i2d` `l2f` `l2d` through `emit_int_to_fp_xmm0`, from every
/// integer home, against Rust's `as` (round-to-nearest, as `CVTSI2Sx`
/// under the default MXCSR). The int forms get a dirty upper half: only
/// the low 32 bits may be read.
#[cfg(target_arch = "x86_64")]
#[test]
fn int_to_fp_conversions_read_every_operand_home() {
    for cache in [false, true] {
        for home in HOMES {
            for (prefix, wide) in [(0xF3u8, false), (0xF2, false), (0xF3, true), (0xF2, true)] {
                let leaf = Leaf::new(|c| {
                    open_frame(c, cache);
                    place(c, home, RAX);
                    c.emit_int_to_fp_xmm0(prefix, wide);
                    c.stack_push(StackSlot::Xmm(0), false);
                    close_frame(c);
                });
                let vals = if wide { LONGS } else { INTS };
                for &v in vals.iter() {
                    let arg = if wide { v } else { v ^ (0x5A5A_5A5A_i64 << 32) };
                    let got = leaf.call(arg, 0) ^ SENTINEL;
                    // Cast: truncation to the int operand is the point.
                    let vi = v as i32;
                    let what = format!(
                        "prefix={prefix:#x} wide={wide} cache={cache} home={home:?} v={v}"
                    );
                    match (prefix, wide) {
                        // Cast: a float result is the low 32 bits.
                        (0xF3, false) => {
                            assert_eq!(got as u32, (vi as f32).to_bits(), "{what}")
                        }
                        // Cast: the double's raw bits.
                        (0xF2, false) => {
                            assert_eq!(got as u64, f64::from(vi).to_bits(), "{what}")
                        }
                        // Cast: a float result is the low 32 bits; `as` rounds to nearest.
                        (0xF3, true) => assert_eq!(got as u32, (v as f32).to_bits(), "{what}"),
                        // Cast: the double's raw bits; `as` rounds to nearest.
                        _ => assert_eq!(got as u64, (v as f64).to_bits(), "{what}"),
                    }
                }
            }
        }
    }
}

/// Where a floating-point test operand lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FpHome {
    /// XMM0 itself (a `dmul` result).
    Xmm0,
    /// The first scratch XMM.
    XmmScratch,
    /// A frame word.
    Frame,
    /// R8, the operand cache's first register.
    Scratch8,
}

/// `f2i` `f2l` `f2d` `d2i` `d2l` `d2f` through the real `walk_arith`
/// arms, from every home, with and without an unrelated double parked in
/// XMM0 under the operand. Checks the answer against Rust's `as` (which
/// saturates and sends NaN to 0, JVMS §2.8.3) and, in a second leaf, that
/// the parked value survived the conversion's use of XMM0.
#[cfg(target_arch = "x86_64")]
#[test]
fn fp_conversions_read_every_operand_home_and_keep_a_parked_xmm0() {
    let parked = -12.75f64;
    let doubles = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        1.5,
        -2.5,
        2147483648.0,
        -2147483649.0,
        9.3e18,
        -9.3e18,
        123456.789,
    ];
    for op in [0x8bu8, 0x8c, 0x8d, 0x8e, 0x8f, 0x90] {
        let src_is_double = matches!(op, 0x8e..=0x90);
        for home in [
            FpHome::Xmm0,
            FpHome::XmmScratch,
            FpHome::Frame,
            FpHome::Scratch8,
        ] {
            for with_parked in [false, true] {
                if with_parked && home == FpHome::Xmm0 {
                    // Two entries naming XMM0 would be one value.
                    continue;
                }
                let build = |return_parked: bool| {
                    Leaf::new(|c| {
                        open_frame(c, true);
                        c.emit_mov_r64_r64(R10, RAX); // the operand's bits
                        if with_parked {
                            // Cast: raw IEEE bits into an immediate.
                            c.emit_mov_imm64(RAX, parked.to_bits() as i64);
                            c.emit_movq_xmm_from_gpr(0, RAX);
                            c.stack_push(StackSlot::Xmm(0), false);
                        }
                        match home {
                            FpHome::Xmm0 => {
                                c.emit_movq_xmm_from_gpr(0, R10);
                                c.stack_push(StackSlot::Xmm(0), false);
                            }
                            FpHome::XmmScratch => {
                                c.emit_movq_xmm_from_gpr(SCRATCH_XMMS[0], R10);
                                c.scratch_xmm_in_use |= 1;
                                c.stack_push(StackSlot::Xmm(SCRATCH_XMMS[0]), false);
                            }
                            FpHome::Frame => place(c, Home::Frame, R10),
                            FpHome::Scratch8 => place(c, Home::Scratch8, R10),
                        }
                        let mut dead = false;
                        let step = c.walk_arith(&[op], 1, op, 0, &mut dead, &[false]);
                        assert!(
                            matches!(step, super::super::bytecode_walk::WalkStep::Next(1)),
                            "op {op:#x} lowers"
                        );
                        if return_parked {
                            c.pop_to_rcx(); // the result, discarded
                            c.pop_to_rax(); // the parked double
                        } else {
                            c.pop_to_rax(); // the result
                            if with_parked {
                                let _ = c.pop_stack();
                            }
                        }
                        let _ = c.pop_stack(); // the sentinel
                        assert!(c.stack.is_empty(), "the model must be balanced");
                        c.buf.emit_byte(0xC9); // LEAVE
                    })
                };
                let result_leaf = build(false);
                let parked_leaf = if with_parked { Some(build(true)) } else { None };
                for &x in doubles.iter() {
                    // Cast: f64 -> f32 rounding is part of the sample set.
                    let f = x as f32;
                    // Cast: raw IEEE bits through the integer argument register.
                    let arg = if src_is_double {
                        x.to_bits() as i64
                    } else {
                        i64::from(f.to_bits())
                    };
                    let got = result_leaf.call(arg, 0);
                    let what = format!("op={op:#x} home={home:?} parked={with_parked} x={x:e}");
                    match op {
                        0x8b => assert_eq!(got, i64::from(f as i32), "{what}"),
                        0x8c => assert_eq!(got, f as i64, "{what}"),
                        0x8d => {
                            // Cast: the double result's raw bits.
                            let d = f64::from_bits(got as u64);
                            let want = f64::from(f);
                            assert!(d == want || (d.is_nan() && want.is_nan()), "{what}: {d}");
                        }
                        0x8e => assert_eq!(got, i64::from(x as i32), "{what}"),
                        0x8f => assert_eq!(got, x as i64, "{what}"),
                        _ => {
                            // Cast: a float result is the low 32 bits.
                            let r = f32::from_bits(got as u32);
                            assert!(r == f || (r.is_nan() && f.is_nan()), "{what}: {r}");
                        }
                    }
                    if let Some(leaf) = &parked_leaf {
                        // Cast: the parked double's raw bits.
                        let p = f64::from_bits(leaf.call(arg, 0) as u64);
                        assert_eq!(p, parked, "{what}: the parked XMM0 value was clobbered");
                    }
                }
            }
        }
    }
}

/// The min/max peephole's edge table: a count per named pc (saturating at
/// 2), switch targets and handler entries included, `None` on a
/// subroutine.
#[test]
fn incoming_edge_counts_counts_branches_switch_targets_and_handlers() {
    // (a < b) ? a : b — the if_icmpge names 9 once, the goto names 10 once.
    let canonical = [
        0x1au8, 0x1b, 0xa2, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x1b, 0xac,
    ];
    let counts = incoming_edge_counts(&canonical, canonical.len(), std::iter::empty())
        .expect("no subroutine");
    assert_eq!(counts.len(), canonical.len());
    assert_eq!((counts[9], counts[10]), (1, 1));
    assert_eq!(counts.iter().map(|&c| usize::from(c)).sum::<usize>(), 2);

    // The same, plus a handler entry at 9: two "edges" name it now.
    let with_handler = incoming_edge_counts(&canonical, canonical.len(), [9]).expect("ok");
    assert_eq!(with_handler[9], 2);

    // A lookupswitch whose default and one case both name 28, and whose
    // other case names 29 (plus two handler entries at 28): 28 saturates at
    // 2 (never wraps), 29 is 1.
    let mut switch = vec![0x1au8, 0xab, 0x00, 0x00];
    switch.extend_from_slice(&27i32.to_be_bytes()); // default -> 28
    switch.extend_from_slice(&2i32.to_be_bytes()); // npairs
    switch.extend_from_slice(&0i32.to_be_bytes());
    switch.extend_from_slice(&27i32.to_be_bytes()); // 0 -> 28
    switch.extend_from_slice(&1i32.to_be_bytes());
    switch.extend_from_slice(&28i32.to_be_bytes()); // 1 -> 29
    switch.extend_from_slice(&[0x03, 0xac]); // 28: iconst_0 ; 29: ireturn
    let counts = incoming_edge_counts(&switch, switch.len(), [28, 28]).expect("strict switch");
    assert_eq!((counts[28], counts[29]), (2, 1));

    // jsr: successors not statically known -> refuse.
    let jsr = [0xa8u8, 0x00, 0x03, 0xb1];
    assert!(incoming_edge_counts(&jsr, jsr.len(), std::iter::empty()).is_none());
}
