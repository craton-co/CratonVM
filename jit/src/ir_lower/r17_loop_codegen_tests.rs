    use super::*;
    use crate::ir::IrBuilder;

    /// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
    const SUM_LOOP: [u8; 23] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];

    /// `int f(int n) { double s = 0; for (int i = 0; i < n; i++) s += i + 1.0; return (int) s; }`
    /// -- an FP accumulator carried across the back edge, which the poll's
    /// slow path must reload from its home after the call.
    const FP_LOOP: [u8; 27] = [
        0x0e, 0x48, // 0: dconst_0; dstore_1
        0x03, 0x3e, // 2: iconst_0; istore_3
        0x1d, 0x1a, 0xa2, 0x00, 0x10, // 4: iload_3; iload_0; if_icmpge +16 -> 22
        0x27, 0x1d, 0x87, 0x0f, 0x63, 0x63,
        0x48, // 9: dload_1; iload_3; i2d; dconst_1; dadd; dadd; dstore_1
        0x84, 0x03, 0x01, // 16: iinc 3, 1
        0xa7, 0xff, 0xf1, // 19: goto -15 -> 4
        0x27, 0x8e, 0xac, // 22: dload_1; d2i; ireturn
        0, 0,
    ];

    fn sum_ref(n: i64) -> i32 {
        (0..n.max(0)).fold(0i32, |s, i| s.wrapping_add(i as i32))
    }

    fn fp_ref(n: i64) -> i32 {
        let s = (0..n.max(0)).fold(0f64, |s, i| s + (i as f64 + 1.0));
        // Cast: `d2i` saturates, as `as` does.
        s as i32
    }

    /// The slow path: counts its calls in the word after the flag (the
    /// helper's first argument is the flag's address) and clobbers every
    /// caller-saved XMM register, so an FP value the outlined block failed to
    /// reload would read back as a NaN.
    extern "C" fn counting_poll(flag_addr: i64, _body_id: i64) -> i64 {
        // SAFETY: `flag_addr` is the leaked two-word block `lower_polling`
        // baked into the body; its second word is the counter.
        unsafe {
            let words = flag_addr as *mut u64;
            *words.add(1) += 1;
            std::arch::asm!(
                "pcmpeqd xmm0, xmm0",
                "pcmpeqd xmm1, xmm1",
                "pcmpeqd xmm2, xmm2",
                "pcmpeqd xmm3, xmm3",
                "pcmpeqd xmm4, xmm4",
                "pcmpeqd xmm5, xmm5",
                out("xmm0") _,
                out("xmm1") _,
                out("xmm2") _,
                out("xmm3") _,
                out("xmm4") _,
                out("xmm5") _,
            );
        }
        0
    }

    /// Lower `code` with a live poll word (`flag` set: every poll takes its
    /// slow path) and return the body and the poll block.
    fn lower_polling(
        code: &[u8],
        code_len: usize,
        max_locals: usize,
        flag: u8,
        edits: &[(&str, Option<&str>)],
    ) -> (CompiledMethod, &'static mut [u64; 2]) {
        cratonvm_types::flags::with_thread_overrides(edits, || {
            let mut builder = IrBuilder::new(1, max_locals);
            builder.set_param_types(&[IrType::Int]);
            let mut graph = builder.build(code, code_len).expect("IR build");
            crate::ir_optimize::optimize(&mut graph);
            let schedule = crate::ir_schedule::schedule(&graph);
            // A leaked block (no `static`: `process_global_statics_ratchet`):
            // byte 0 is the poll flag, word 1 the slow path's call count.
            let words: &'static mut [u64; 2] = Box::leak(Box::new([u64::from(flag), 0]));
            // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; only the poll is wired.
            let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            helpers.safepoint_flag_addr = words.as_ptr() as usize; // Cast: baked flag address
            helpers.safepoint_slow_path = counting_poll as *const () as usize; // Cast: helper address
            let cm = lower_inner(
                &graph,
                &schedule,
                1,
                max_locals,
                &helpers,
                &HashMap::new(),
                &[],
                None,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &crate::ir::IrInlineFrameSites::default(),
            )
            .expect("the loop lowers");
            (cm, words)
        })
    }

    fn run(cm: &CompiledMethod, n: i64) -> i32 {
        // SAFETY: an `int f(int)` loop body with no call but the poll's.
        let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
        // Cast: this tier's `int` result is the low 32 bits of RAX.
        got as i32
    }


    /// `TEST BYTE [rip+d], 0xFF; JZ rel32` -- the inline poll's skip -- and
    /// `...; JNZ rel32`, the outlined one's entry.
    fn poll_jcc_counts(code: &[u8]) -> (usize, usize) {
        let (mut jz, mut jnz) = (0, 0);
        for w in code.windows(9) {
            if w[0] == 0xF6 && w[1] == 0x05 && w[6] == 0xFF && w[7] == 0x0F {
                match w[8] {
                    0x84 => jz += 1,
                    0x85 => jnz += 1,
                    _ => {}
                }
            }
        }
        (jz, jnz)
    }


    /// `int f(int x) { int a = x * 31 + 7; int b = (a ^ 0x55) + 3; return a ^ (b + 1000); }`
    /// -- straight-line arithmetic whose every constant folds into its reader.
    const STRAIGHT: [u8; 25] = [
        0x1a, 0x10, 0x1f, 0x68, 0x10, 0x07, 0x60, 0x3c, // a = x * 31 + 7
        0x1b, 0x10, 0x55, 0x82, 0x06, 0x60, 0x3d, // b = (a ^ 0x55) + 3
        0x1b, 0x1c, 0x11, 0x03, 0xe8, 0x60, 0x82, 0xac, // return a ^ (b + 1000)
        0, 0,
    ];

    fn straight_ref(x: i64) -> i32 {
        // Cast: the fixture's `int` parameter is the low 32 bits.
        let a = (x as i32).wrapping_mul(31).wrapping_add(7);
        let b = (a ^ 0x55).wrapping_add(3);
        a ^ b.wrapping_add(1000)
    }


    /// `MOV qword [rbp + disp8|disp32], imm32` -- a constant's home store.
    fn imm_home_stores(code: &[u8]) -> usize {
        code.windows(3)
            .filter(|w| w[0] == 0x48 && w[1] == 0xC7 && (w[2] == 0x45 || w[2] == 0x85))
            .count()
    }


    /// `int f(int x) { return x * 63 + x * 9 ^ x * 3; }` -- multipliers the
    /// pass leaves alone (no constant addend), which must answer as before.
    const MULS: [u8; 16] = [
        0x1a, 0x10, 0x3f, 0x68, // x * 63
        0x1a, 0x10, 0x09, 0x68, 0x60, // + x * 9
        0x1a, 0x06, 0x68, 0x82, // ^ x * 3
        0xac, 0, 0,
    ];

    fn muls_ref(x: i64) -> i32 {
        // Cast: the fixture's `int` parameter is the low 32 bits.
        let x = x as i32;
        x.wrapping_mul(63).wrapping_add(x.wrapping_mul(9)) ^ x.wrapping_mul(3)
    }

    /// `IMUL EAX/RAX, r, imm8|imm32` (`6B` / `69` with a register-direct
    /// ModRM whose `reg` is RAX, `C0..=C7`: every form `emit_imul_imm` and
    /// `imul_rax_reg_imm_bytes` emit) -- what the rewrite removes.
    ///
    /// The opcode byte ALONE is not a count of instructions: these bodies
    /// carry the entry poll, which bakes the leaked poll block's heap address
    /// (`MOV r64, imm64`) and its RIP displacement from the code buffer. Both
    /// differ per lowering and per run, and a `0x69` / `0x6B` byte in either
    /// could make `reduced` and `plain` count equal on an unlucky run.
    fn imul_imms(code: &[u8]) -> usize {
        code.windows(2)
            .filter(|w| (w[0] == 0x6B || w[0] == 0x69) && (w[1] & 0xF8) == 0xC0)
            .count()
    }


    /// `int f(int x) { return 31 * x + x; }` -- the constant FIRST, javac's order.
    const CONST_FIRST: [u8; 9] = [0x10, 0x1f, 0x1a, 0x68, 0x1a, 0x60, 0xac, 0, 0];

    fn const_first_ref(x: i64) -> i32 {
        // Cast: the fixture's `int` parameter is the low 32 bits.
        let x = x as i32;
        x.wrapping_mul(31).wrapping_add(x)
    }

    /// `int f(int n) { int h = 0; for (int i = 0; i < n; i++) h = 31 * h + i; return h; }`
    /// -- `heapGet`'s recurrence with `h` resident.
    const HASH_LOOP: [u8; 26] = [
        0x03, 0x3c, 0x03, 0x3d, // h = 0; i = 0
        0x1c, 0x1a, 0xa2, 0x00, 0x10, // 4: i < n, else -> 22
        0x10, 0x1f, 0x1b, 0x68, 0x1c, 0x60, 0x3c, // h = 31 * h + i
        0x84, 0x02, 0x01, // i++
        0xa7, 0xff, 0xf1, // goto 4
        0x1b, 0xac, // 22: return h
        0, 0,
    ];

    fn hash_loop_ref(n: i64) -> i32 {
        // Cast: the fixture's `int` parameter is the low 32 bits.
        let n = n as i32;
        let mut h = 0i32;
        let mut i = 0i32;
        while i < n {
            h = h.wrapping_mul(31).wrapping_add(i);
            i += 1;
        }
        h
    }

    /// Register-register `IMUL r32/r64, r/m` (`0F AF /r`, mod 11): the form
    /// the constant-first arm used to reach through RCX.
    fn imul_reg_regs(code: &[u8]) -> usize {
        code.windows(3)
            .filter(|w| w[0] == 0x0F && w[1] == 0xAF && w[2] >= 0xC0)
            .count()
    }

    /// JIT r23 wave 11 (lane L7, `CRATONVM_JIT_IR_IMUL_CONST_FIRST`): `31 * x`
    /// multiplies by the immediate -- from RAX in a straight line, from `h`'s
    /// register in the loop -- with no register-register `IMUL` left, `=0`
    /// restores it, and every answer is the multiply's, at the wrap edges too.
/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i * 3 ^ (i >>> 2); return s; }`
    /// -- `R16CodegenLoops.intLoop`.
    const INT_LOOP: [u8; 29] = [
        0x03, 0x3c, 0x03, 0x3d, // s = 0; i = 0
        0x1c, 0x1a, 0xa2, 0x00, 0x13, // 4: i < n, else -> 25
        0x1b, 0x1c, 0x06, 0x68, 0x1c, 0x05, 0x7c, 0x82, 0x60, 0x3c, // s += i * 3 ^ (i >>> 2)
        0x84, 0x02, 0x01, // i++
        0xa7, 0xff, 0xee, // goto 4
        0x1b, 0xac, // 25: return s
        0, 0,
    ];

    fn int_loop_ref(n: i64) -> i32 {
        // Cast: the fixture's `int` parameter is the low 32 bits.
        let n = n as i32;
        let mut s = 0i32;
        let mut i = 0i32;
        while i < n {
            s = s.wrapping_add(i.wrapping_mul(3) ^ ((i as u32) >> 2) as i32);
            i += 1;
        }
        s
    }


