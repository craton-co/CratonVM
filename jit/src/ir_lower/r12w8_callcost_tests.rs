    use super::*;
    use crate::ir::IrBuilder;

    fn no_helpers() -> JitRuntimeHelpers {
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the bodies here call only `guard_ok`.
        unsafe { std::mem::zeroed() }
    }

    /// The self-call stack guard's "enough stack" answer.
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }

    /// `static int fib(int n) { if (n < 2) return n; return fib(n - 1) +
    /// fib(n - 2); }` -- both invokes (pcs 10, 16) on the direct self-call
    /// route. Padded past `code_len` (21).
    const FIB: [u8; 23] = [
        0x1a, 0x05, 0xa2, 0x00, 0x05, // iload_0; iconst_2; if_icmpge +5
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic
        0x60, 0xac, // iadd; ireturn
        0x00, 0x00,
    ];

    /// `static int g(int n) { if (n <= 0) return 0; if ((n & 1) == 0) return
    /// 1 + g(n - 1); return 2 + g(n - 2); }` -- two self-calls (pcs 15, 24) on
    /// SIBLING branches, neither dominating the other; the base case returns a
    /// constant. Padded past `code_len` (30).
    const SIBLINGS: [u8; 32] = [
        0x1a, 0x9d, 0x00, 0x05, // 0: iload_0; ifgt +5 -> 6
        0x03, 0xac, // 4: iconst_0; ireturn
        0x1a, 0x04, 0x7e, 0x9a, 0x00, 0x0c, // 6: iload_0; iconst_1; iand; ifne +12 -> 21
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // 12: iload_0; iconst_1; isub; invokestatic
        0x04, 0x60, 0xac, // 18: iconst_1; iadd; ireturn
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // 21: iload_0; iconst_2; isub; invokestatic
        0x05, 0x60, 0xac, // 27: iconst_2; iadd; ireturn
        0x00, 0x00,
    ];

    fn fib_ref(n: i64) -> i64 {
        if n < 2 {
            n
        } else {
            fib_ref(n - 1) + fib_ref(n - 2)
        }
    }

    fn siblings_ref(n: i64) -> i64 {
        if n <= 0 {
            0
        } else if n & 1 == 0 {
            1 + siblings_ref(n - 1)
        } else {
            2 + siblings_ref(n - 2)
        }
    }

    fn lower_self_recursive(
        code: &[u8],
        code_len: usize,
        call_pcs: &[usize],
        edits: &[(&str, Option<&str>)],
    ) -> CompiledMethod {
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/CallCost",
            method_name: "f",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 4, // the direct self-recursive route
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        cratonvm_types::flags::with_thread_overrides(edits, || {
            let mut builder = IrBuilder::new(1, 1);
            builder.set_param_types(&[IrType::Int]);
            let mut invoke_info = HashMap::new();
            for &pc in call_pcs {
                invoke_info.insert(pc, (info as *const JitInvokeInfo as usize, 1usize, b'I'));
            }
            builder.set_invoke_info(invoke_info);
            let graph = builder.build(code, code_len).expect("IR build");
            let schedule = crate::ir_schedule::schedule(&graph);
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
            .expect("the self-recursive body lowers")
        })
    }

    fn run(cm: &CompiledMethod, n: i64) -> i64 {
        // SAFETY: an `int f(int)` body lowered by this module; its only
        // reachable helper is `guard_ok`, and it recurses at most a few dozen
        // frames deep.
        let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
        // Cast: this tier's `int` result is the low 32 bits of RAX.
        i64::from(got as i32)
    }

    fn count(code: &[u8], needle: &[u8]) -> usize {
        code.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// The `CMP <param>, imm; Jcc` the entry fast return opens with, right
    /// after the five-byte not-entrant pad, for `cm`'s parameter 0.
    fn fast_return_head(cm: &CompiledMethod, imm: i32, jcc: u8) -> Vec<u8> {
        let reg = ENTRY_ABI_REGS[usize::from(cm.needs_context())];
        let (bytes, len) = cmp_reg_imm_bytes(reg, imm, false);
        let mut head = crate::ENTRY_PATCH_PAD.to_vec();
        head.extend_from_slice(&bytes[..len]);
        head.extend_from_slice(&[0x0F, jcc]);
        head
    }

    const PUSH_RBP_MOV_RBP_RSP: [u8; 4] = [0x55, 0x48, 0x89, 0xE5];
    const SAMPLE: [u8; 4] = [0x0F, 0xB7, 0xC4, 0x3D]; // MOVZX EAX, SP; CMP EAX, imm32

#[test]
    fn sibling_self_calls_keep_both_samples() {
        let on = lower_self_recursive(&SIBLINGS, 30, &[15, 24], &[]);
        assert_eq!(
            count(on.code_bytes(), &SAMPLE),
            2,
            "neither call dominates the other, so each samples"
        );
        for n in 0..=80 {
            assert_eq!(run(&on, n), siblings_ref(n), "g({n})");
        }
    }

    #[test]
    fn the_cycle_test_sees_a_loop_and_not_a_recursion() {
        // `int sum(int n){int s=0;for(int i=0;i<n;i++)s+=i;return s;}`
        let sum = [
            0x03u8, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac,
        ];
        let graph = IrBuilder::new(1, 3)
            .build(&sum, 21)
            .expect("IR build of the loop");
        let schedule = crate::ir_schedule::schedule(&graph);
        assert!(
            schedule_block_cfg_has_cycle(&schedule),
            "the counted loop has a back edge"
        );

        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Int]);
        // The two `invokestatic`s need invoke info, or the build refuses them.
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/CallCost",
            method_name: "f",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 4,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let mut invoke_info = HashMap::new();
        for pc in [10usize, 16] {
            invoke_info.insert(pc, (info as *const JitInvokeInfo as usize, 1usize, b'I'));
        }
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(&FIB, 21).expect("IR build of fib");
        let schedule = crate::ir_schedule::schedule(&graph);
        assert!(
            !schedule_block_cfg_has_cycle(&schedule),
            "recursion is not a CFG cycle"
        );
    }

    /// `static int k(int n) { if (n < 2) return k(n + 2); return n; }`:
    /// the compare is `n < 2`, the recursion is its TRUE edge and the return
    /// its false edge, so the fast path is taken on `n >= 2`.
    #[test]
    fn the_return_edge_may_be_the_false_edge_of_a_less_than() {
        // iload_0; iconst_2; if_icmplt +5 -> 7; iload_0; ireturn;
        // 7: iload_0; iconst_2; iadd; invokestatic; ireturn
        let code = [
            0x1a, 0x05, 0xa1, 0x00, 0x05, 0x1a, 0xac, 0x1a, 0x05, 0x60, 0xb8, 0x00, 0x02, 0xac,
            0x00, 0x00,
        ];
        let cm = lower_self_recursive(&code, 14, &[10], &[]);
        let head = fast_return_head(&cm, 2, CmpOp::Ge.x64_cc());
        assert!(
            cm.code_bytes().starts_with(&head),
            "pad, CMP n, 2, JGE .fast_return: {:02x?}",
            &cm.code_bytes()[..cm.code_bytes().len().min(16)]
        );
        for n in -9..=9 {
            // k(n) climbs by 2 until n >= 2.
            let mut want = n;
            while want < 2 {
                want += 2;
            }
            assert_eq!(run(&cm, n), want, "k({n})");
        }
        // One self-call: nothing dominates it, so it keeps its sample.
        assert_eq!(count(cm.code_bytes(), &SAMPLE), 1);
    }

    /// `static int t(int n) { if (n < 2) return n + 1; return t(n - 1); }`:
    /// the base case returns a COMPUTED value, which the fast path does not
    /// reproduce, so the body keeps its ordinary entry.
    #[test]
    fn a_computed_base_case_keeps_the_prologue() {
        // iload_0; iconst_2; if_icmpge +7 -> 9; iload_0; iconst_1; iadd; ireturn;
        // 9: iload_0; iconst_1; isub; invokestatic; ireturn
        let code = [
            0x1a, 0x05, 0xa2, 0x00, 0x07, 0x1a, 0x04, 0x60, 0xac, 0x1a, 0x04, 0x64, 0xb8, 0x00,
            0x02, 0xac, 0x00, 0x00,
        ];
        let cm = lower_self_recursive(&code, 16, &[12], &[]);
        let pad = crate::ENTRY_PATCH_PAD.len();
        assert_eq!(
            &cm.code_bytes()[pad..pad + 4],
            &PUSH_RBP_MOV_RBP_RSP,
            "no fast return: the prologue follows the pad"
        );
        for n in -5..=12 {
            let want = if n < 2 { n + 1 } else { 2 };
            assert_eq!(run(&cm, n), want, "t({n})");
        }
    }

    // ── Round 14 wave 5 (lane callcost, CC5-5): the entry fold ──────────────

    /// `static int g(int n) { if (n < 2) return n; if (n < 4) return n - 1;
    /// return g(n - 1) + g(n - 2); }` -- `fib` with its two call-free
    /// activations (`n` in `{2, 3}`, what two spliced levels leave call-free)
    /// written out. Self-calls at pcs 19 and 25. Padded past `code_len` (30).
    const FIB_CALL_FREE_PAIR: [u8; 32] = [
        0x1a, 0x05, 0xa2, 0x00, 0x05, // 0: iload_0; iconst_2; if_icmpge +5 -> 7
        0x1a, 0xac, // 5: iload_0; ireturn
        0x1a, 0x07, 0xa2, 0x00, 0x07, // 7: iload_0; iconst_4; if_icmpge +7 -> 16
        0x1a, 0x04, 0x64, 0xac, // 12: iload_0; iconst_1; isub; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // 16: iload_0; iconst_1; isub; invokestatic
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // 22: iload_0; iconst_2; isub; invokestatic
        0x60, 0xac, // 28: iadd; ireturn
        0x00, 0x00,
    ];

    /// `static int h(int n) { if (n < 2) return n; if (n < 100) return n * 3;
    /// return h(n - 1); }` -- a call-free range longer than the fold's cap.
    /// Self-call at pc 20. Padded past `code_len` (24).
    const TRIPLE_BELOW_100: [u8; 26] = [
        0x1a, 0x05, 0xa2, 0x00, 0x05, // 0: iload_0; iconst_2; if_icmpge +5 -> 7
        0x1a, 0xac, // 5: iload_0; ireturn
        0x1a, 0x10, 0x64, 0xa2, 0x00, 0x07, // 7: iload_0; bipush 100; if_icmpge +7 -> 17
        0x1a, 0x06, 0x68, 0xac, // 13: iload_0; iconst_3; imul; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // 17: iload_0; iconst_1; isub; invokestatic
        0xac, // 23: ireturn
        0x00, 0x00,
    ];

    fn triple_below_100_ref(n: i64) -> i64 {
        if n < 2 {
            n
        } else if n < 100 {
            n * 3
        } else {
            297
        }
    }

    const CMOVE_RAX_R11: [u8; 4] = [0x49, 0x0F, 0x44, 0xC3];

    /// The `CMP p, hi; JLE` the entry fold emits right after the fast
    /// return's `rel32`.
    fn fold_head(cm: &CompiledMethod, hi: i32) -> Vec<u8> {
        let reg = ENTRY_ABI_REGS[usize::from(cm.needs_context())];
        let (bytes, len) = cmp_reg_imm_bytes(reg, hi, false);
        let mut head = bytes[..len].to_vec();
        head.extend_from_slice(&[0x0F, CmpOp::Le.x64_cc()]);
        head
    }




    #[test]
    fn r14w5_the_folded_int_ops_have_java_semantics() {
        let op = |o: Op, a: i32, b: Option<i32>| entry_fold_int_op(&o, a, b);
        assert_eq!(op(Op::Add, i32::MAX, Some(1)), Some(i32::MIN));
        assert_eq!(op(Op::Sub, i32::MIN, Some(1)), Some(i32::MAX));
        assert_eq!(op(Op::Mul, 0x4000_0000, Some(4)), Some(0));
        assert_eq!(op(Op::Neg, i32::MIN, None), Some(i32::MIN));
        assert_eq!(
            op(Op::Shl, 1, Some(33)),
            Some(2),
            "the distance is masked to 5 bits"
        );
        assert_eq!(op(Op::Shr, -8, Some(1)), Some(-4));
        assert_eq!(op(Op::Shr, -8, Some(-31)), Some(-4), "-31 & 31 == 1");
        assert_eq!(op(Op::UShr, -1, Some(28)), Some(15));
        assert_eq!(op(Op::And, 6, Some(3)), Some(2));
        assert_eq!(op(Op::Or, 6, Some(3)), Some(7));
        assert_eq!(op(Op::Xor, 6, Some(3)), Some(5));
        assert_eq!(op(Op::Cmp(CmpOp::Lt), -1, Some(0)), Some(1), "signed");
        assert_eq!(op(Op::Cmp(CmpOp::Ge), -1, Some(0)), Some(0));
        assert_eq!(
            op(Op::Div, 6, Some(3)),
            None,
            "a trapping op is never evaluated"
        );
        assert_eq!(op(Op::Add, 6, None), None, "wrong arity");
        assert_eq!(op(Op::Neg, 6, Some(1)), None, "wrong arity");
    }

    // ── Round 14 wave 6 (lane callcost2, CE-1): self-calls answered in the caller ──




    const CMOVL_RAX_R11: [u8; 4] = [0x49, 0x0F, 0x4C, 0xC3];



    /// `static int acc(int n, int a) { if (n <= 0) return a; return acc(n - 1,
    /// a + n); }` -- the fast return answers the OTHER parameter, so the
    /// caller answers with the call's second argument. Self-call at pc 12.
    /// Padded past `code_len` (16).
    const ACC: [u8; 18] = [
        0x1a, 0x9d, 0x00, 0x05, // 0: iload_0; ifgt +5 -> 6
        0x1b, 0xac, // 4: iload_1; ireturn
        0x1a, 0x04, 0x64, // 6: iload_0; iconst_1; isub
        0x1b, 0x1a, 0x60, // 9: iload_1; iload_0; iadd
        0xb8, 0x00, 0x02, // 12: invokestatic
        0xac, // 15: ireturn
        0x00, 0x00,
    ];

    fn acc_ref(n: i32, a: i32) -> i32 {
        let mut a = a;
        let mut n = n;
        while n > 0 {
            a = a.wrapping_add(n);
            n -= 1;
        }
        a
    }

    fn lower_acc(edits: &[(&str, Option<&str>)]) -> CompiledMethod {
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/CallCost6",
            method_name: "acc",
            descriptor: "(II)I",
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind: 4, // the direct self-recursive route
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        cratonvm_types::flags::with_thread_overrides(edits, || {
            let mut builder = IrBuilder::new(2, 2);
            builder.set_param_types(&[IrType::Int, IrType::Int]);
            let mut invoke_info = HashMap::new();
            invoke_info.insert(
                12usize,
                (info as *const JitInvokeInfo as usize, 2usize, b'I'),
            );
            builder.set_invoke_info(invoke_info);
            let graph = builder.build(&ACC, 16).expect("IR build");
            let schedule = crate::ir_schedule::schedule(&graph);
            let mut helpers = no_helpers();
            helpers.self_call_stack_guard = guard_ok as *const () as usize;
            let empty_hints: HashMap<usize, bool> = HashMap::new();
            let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
            let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
            let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
            lower_inner(
                &graph,
                &schedule,
                2,
                2,
                &helpers,
                &empty_hints,
                &[],
                None,
                &no_direct,
                &no_ic,
                &no_compact,
                &crate::ir::IrInlineFrameSites::default(),
            )
            .expect("the two-parameter body lowers")
        })
    }


    // ── Round 14 wave 7 (lane callcost3): the entry fold through self-calls ──


    #[test]
    fn r14w7_a_self_call_with_its_own_argument_or_a_growing_one_folds_nothing() {
        // `static int k(int n) { if (n < 2) return n; return k(n); }` (the
        // call at pc 8 never returns for n >= 2) and the same body calling
        // `k(n + 1)` (climbs to the wrap): neither may fold a value.
        // iload_0; iconst_2; if_icmpge +5 -> 7; iload_0; ireturn;
        // 7: iload_0; invokestatic; ireturn
        let same = [
            0x1a, 0x05, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1a, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00,
        ];
        // 7: iload_0; iconst_1; iadd; invokestatic (pc 10); ireturn
        let growing = [
            0x1a, 0x05, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1a, 0x04, 0x60, 0xb8, 0x00, 0x02, 0xac,
            0x00, 0x00,
        ];
        for (code, len, pc) in [(&same[..], 12usize, 8usize), (&growing[..], 14, 10)] {
            let cm = lower_self_recursive(code, len, &[pc], &[]);
            let fast = fast_return_head(&cm, 2, CmpOp::Lt.x64_cc());
            assert!(cm.code_bytes().starts_with(&fast));
            let at = fast.len() + 4;
            assert_eq!(
                &cm.code_bytes()[at..at + 4],
                &PUSH_RBP_MOV_RBP_RSP,
                "no fold: the prologue follows the fast return"
            );
            // Only the base case is safe to run here.
            for n in -3..=1 {
                assert_eq!(run(&cm, n), n, "k({n})");
            }
        }
    }

    // ── Round 14 wave 7 (lane callcost3, CC6-2): the parameter's register ──


    /// `MOV <gp-file reg>, [rbp - 8]`: a reload of parameter 0's prologue
    /// word into a register of the IR GP file, any of them.
    fn param0_reloads(code: &[u8]) -> usize {
        ir_gp_file()
            .iter()
            .map(|&r| {
                let rex = if r >= 8 { 0x4C } else { 0x48 };
                count(code, &[rex, 0x8B, 0x45 | ((r & 7) << 3), 0xF8])
            })
            .sum()
    }

// ── Round 13 wave 8 (lane callcost3, CC2-1): deferred cold tails ──────

    /// `static int d(int n) { if (n <= 0) return 0; return d(n - 1) + 1; }`
    /// -- one self-call (pc 9), `n` frames deep. Padded past `code_len` (15).
    const DEPTH: [u8; 17] = [
        0x1a, 0x9d, 0x00, 0x05, // 0: iload_0; ifgt +5 -> 6
        0x03, 0xac, // 4: iconst_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // 6: iload_0; iconst_1; isub; invokestatic
        0x04, 0x60, 0xac, // 12: iconst_1; iadd; ireturn
        0x00, 0x00,
    ];

    /// Deep enough to cross dozens of 64 KiB windows, so the sampled guard
    /// runs many times.
    const DEPTH_N: i64 = 20_000;


    /// [`lower_self_recursive`] with the caller's helper table.
    fn lower_self_recursive_with(
        code: &[u8],
        code_len: usize,
        call_pcs: &[usize],
        edits: &[(&str, Option<&str>)],
        helpers: JitRuntimeHelpers,
    ) -> CompiledMethod {
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/CallCost3",
            method_name: "d",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 4, // the direct self-recursive route
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        cratonvm_types::flags::with_thread_overrides(edits, || {
            let mut builder = IrBuilder::new(1, 1);
            builder.set_param_types(&[IrType::Int]);
            let mut invoke_info = HashMap::new();
            for &pc in call_pcs {
                invoke_info.insert(pc, (info as *const JitInvokeInfo as usize, 1usize, b'I'));
            }
            builder.set_invoke_info(invoke_info);
            let graph = builder.build(code, code_len).expect("IR build");
            let schedule = crate::ir_schedule::schedule(&graph);
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
            .expect("the self-recursive body lowers")
        })
    }

    /// Run `f` on a thread with a 64 MiB stack ([`DEPTH_N`] frames).
    fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(f)
            .expect("spawn the big-stack thread")
            .join()
            .expect("the big-stack thread finished")
    }

    /// The guard, counting its calls in the `AtomicUsize` the context
    /// pointer names, and answering "enough stack".
    extern "C" fn guard_counting(vm: i64, _ctor_receiver: i64) -> i64 {
        // SAFETY: the bodies lowered with this guard are called with a live
        // `AtomicUsize` as their context, which every self-call passes on.
        let calls = unsafe { &*(vm as *const std::sync::atomic::AtomicUsize) };
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        0
    }

    /// The guard, tripping (the sentinel, as `jit_self_call_stack_guard`
    /// answers with a `StackOverflowError` pending) on its SECOND call only:
    /// the first may come from the outermost frame, which has no compiled
    /// caller to take the sentinel path.
    extern "C" fn guard_trips_second(vm: i64, _ctor_receiver: i64) -> i64 {
        // SAFETY: as `guard_counting`.
        let calls = unsafe { &*(vm as *const std::sync::atomic::AtomicUsize) };
        if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 1 {
            i64::MIN
        } else {
            0
        }
    }

    /// `dispatch_threw`: an exception is pending.
    extern "C" fn threw_pending() -> i64 {
        1
    }

    /// A callee-deopt service that "resumes" the callee and answers its
    /// staged argument plus 1000, so the result shows the staging read the
    /// right word.
    extern "C" fn service_resumes(_vm: i64, _info: i64, args: i64, _num_args: i64) -> i64 {
        // SAFETY: `args` is the frame's staging block, arg 0 at its base,
        // written by the service's staging just before this call.
        let arg0 = unsafe { *(args as *const i64) };
        // Cast: an `int` argument travels as the low half of its word.
        i64::from(arg0 as i32) + 1000
    }

    /// `d(n)` on `cm`, with a fresh guard counter as the context; the raw
    /// 64-bit result (the sentinel stays visible) and the guard's call count.
    fn run_depth(cm: &CompiledMethod, n: i64) -> (i64, usize) {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        // Cast: the counter's address, passed as the context pointer.
        let ctx = &calls as *const std::sync::atomic::AtomicUsize as i64;
        // SAFETY: an `int d(int)` body lowered by this module whose helpers
        // read the context as the `AtomicUsize` above; it recurses `n` frames
        // on a stack sized for it (`on_big_stack`).
        let got = unsafe { cm.try_call_with_context(ctx, &[n]) }.expect("call");
        (got, calls.load(std::sync::atomic::Ordering::Relaxed))
    }


    /// `d(DEPTH_N)` lowered with `helpers` and the switch edits: the raw
    /// result and the guard's call count.
    fn depth_arm(edits: &[(&str, Option<&str>)], helpers: JitRuntimeHelpers) -> (i64, usize) {
        let cm = lower_self_recursive_with(&DEPTH, 15, &[9], edits, helpers);
        run_depth(&cm, DEPTH_N)
    }





    // ── Round 14 wave 3 (lane calls, CC3-1): a direct cross call's cold tail ──

    /// `static int f(int n) { return g(n) + 1; }` -- `g` (pc 1) on the direct
    /// cross-call route. Padded past `code_len` (7).
    const CROSS: [u8; 9] = [0x1a, 0xb8, 0x00, 0x02, 0x04, 0x60, 0xac, 0x00, 0x00];


    /// The direct callee `g`: `2n`, or the exception sentinel for `n == 7`.
    extern "C" fn cross_callee(n: i64) -> i64 {
        if n == 7 {
            i64::MIN
        } else {
            n.wrapping_mul(2)
        }
    }

    /// `f` lowered with `g` bound directly (no context) and `helpers`.
    fn lower_cross_call(
        edits: &[(&str, Option<&str>)],
        helpers: JitRuntimeHelpers,
    ) -> CompiledMethod {
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/CallCost14",
            method_name: "g",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        cratonvm_types::flags::with_thread_overrides(edits, || {
            let mut builder = IrBuilder::new(1, 1);
            builder.set_param_types(&[IrType::Int]);
            let mut invoke_info = HashMap::new();
            invoke_info.insert(
                1usize,
                (info as *const JitInvokeInfo as usize, 1usize, b'I'),
            );
            builder.set_invoke_info(invoke_info);
            let graph = builder.build(&CROSS, 7).expect("IR build");
            let schedule = crate::ir_schedule::schedule(&graph);
            let empty_hints: HashMap<usize, bool> = HashMap::new();
            let mut direct: HashMap<usize, (usize, bool)> = HashMap::new();
            direct.insert(1, (cross_callee as *const () as usize, false));
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
                &direct,
                &no_ic,
                &no_compact,
                &crate::ir::IrInlineFrameSites::default(),
            )
            .expect("the cross-call body lowers")
        })
    }

    extern "C" fn native_context_callee(context: i64, a: i64, b: i64, c: i64) -> i64 {
        if context != 0x12345 {
            return -1;
        }
        a + 10 * b + 100 * c
    }

    /// A native context replaces only the first ABI word. Exercise all four
    /// Win64 registers, ordinary Java operands and a copied Call info key.
    #[test]
    fn a_native_direct_call_receives_its_owned_site_context() {
        let info = JitInvokeInfo {
            class_name: "java/lang/invoke/VarHandle",
            method_name: "get",
            descriptor: "([BI)I",
            num_jit_args: 3,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let info_ptr = &info as *const JitInvokeInfo as usize;
        // Three operands, one virtual call, then return. The dummy receiver
        // is deliberately non-null; the native checks the overridden context.
        let code = [0x2a, 0x1b, 0x1c, 0xb6, 0, 1, 0xac, 0, 0];
        let mut builder = IrBuilder::new(3, 3);
        builder.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
        builder.set_invoke_info(HashMap::from([(3, (info_ptr, 3, b'I'))]));
        let mut graph = builder.build(&code, 7).expect("native call graph");
        graph.native_call_contexts.insert(info_ptr, 0x12345);
        let schedule = crate::ir_schedule::schedule(&graph);
        let direct = HashMap::from([(3, (native_context_callee as *const () as usize, true))]);
        let compiled = lower_inner(
            &graph,
            &schedule,
            3,
            3,
            &JitRuntimeHelpers::default(),
            &HashMap::new(),
            &[],
            None,
            &direct,
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("native call lowers");
        // SAFETY: this graph calls only the test's four-word C-ABI function.
        let value = unsafe { compiled.try_call_with_context(0x24680, &[1, 2, 3]) };
        assert_eq!(value.expect("native test call"), 321);
    }

    /// `f(n)`'s raw 64-bit result.
    fn run_raw(cm: &CompiledMethod, n: i64) -> i64 {
        // SAFETY: an `int f(int)` body lowered by this module whose only
        // reachable code is `cross_callee` and the test helpers above.
        unsafe { cm.try_call_with_context(0, &[n]) }.expect("call")
    }



    // ── Round 16 (lane codegen, CG16-1): the fast return through a join phi ──

    /// `static int f(int n) { return n < 2 ? n : f(n - 1) + f(n - 2); }` as
    /// javac emits it: both arms meet at ONE `ireturn` (pc 22) of a stack phi.
    /// Self-calls at pcs 12 and 18. Padded past `code_len` (23).
    const FIB_TERNARY: [u8; 25] = [
        0x1a, 0x05, 0xa2, 0x00, 0x07, // 0: iload_0; iconst_2; if_icmpge +7 -> 9
        0x1a, 0xa7, 0x00, 0x10, // 5: iload_0; goto +16 -> 22
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // 9: iload_0; iconst_1; isub; invokestatic
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // 15: iload_0; iconst_2; isub; invokestatic
        0x60, 0xac, // 21: iadd; 22: ireturn
        0x00, 0x00,
    ];



    /// The join's value is read on the fast path's OWN edge: `return n < 2 ?
    /// 7 : n;` returns the constant on the taken side and the parameter on
    /// the other, and only the edge the compare selects may be answered.
    #[test]
    fn r16_a_join_returns_the_value_of_the_edge_it_is_entered_from() {
        // 0: iload_0; iconst_2; if_icmpge +8 -> 10; 5: bipush 7; 7: goto +6 -> 13;
        // 10: iload_0; nop; nop; 13: ireturn -- no call, so no fold.
        let code = [
            0x1a, 0x05, 0xa2, 0x00, 0x08, 0x10, 0x07, 0xa7, 0x00, 0x06, 0x1a, 0x00, 0x00, 0xac,
            0x00, 0x00,
        ];
        let cm = lower_self_recursive(&code, 14, &[], &[]);
        for n in -4..=9 {
            let want = if n < 2 { 7 } else { n };
            assert_eq!(run(&cm, n), want, "f({n})");
        }
    }
