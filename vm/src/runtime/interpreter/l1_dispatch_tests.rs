use super::*;
use crate::threading::jvm_thread::ThreadId;

fn run_with(
    name: &str,
    desc: &str,
    code: Vec<u8>,
    max_stack: u16,
    max_locals: u16,
    setup: impl FnOnce(&mut Frame),
) -> MethodCallResult {
    // Beyond the type-map store's addressable id range, so no verifier
    // maps can exist for it and the per-method fast-path gate admits it.
    run_in_class(
        ClassId::new(0x00F1_0001),
        name,
        desc,
        code,
        max_stack,
        max_locals,
        setup,
    )
}

fn run_in_class(
    class_id: ClassId,
    name: &str,
    desc: &str,
    code: Vec<u8>,
    max_stack: u16,
    max_locals: u16,
    setup: impl FnOnce(&mut Frame),
) -> MethodCallResult {
    // Heap-pinned like every other test that builds one: nothing may move
    // the VM after construction.
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    run_on(
        &shared, class_id, name, desc, code, max_stack, max_locals, setup,
    )
}

/// [`run_in_class`] on a caller-built VM, for a test that arms VM state
/// (a debugger request, a channel) before the method runs.
#[allow(clippy::too_many_arguments)]
fn run_on(
    shared: &SharedVm,
    class_id: ClassId,
    name: &str,
    desc: &str,
    code: Vec<u8>,
    max_stack: u16,
    max_locals: u16,
    setup: impl FnOnce(&mut Frame),
) -> MethodCallResult {
    let mut thread = JvmThread::new(ThreadId(0), "l1-dispatch-test");
    let mut frame = Frame::new(
        class_id,
        "cratonvm/test/L1Dispatch".to_string(),
        name.to_string(),
        desc.to_string(),
        None,
        code,
        Vec::new(),
        max_stack,
        max_locals,
        &[],
    );
    setup(&mut frame);
    execute_prebuilt_frame(shared, &mut thread, frame)
}

fn run(name: &str, desc: &str, code: Vec<u8>, max_stack: u16, max_locals: u16) -> Value {
    match run_with(name, desc, code, max_stack, max_locals, |_| {}) {
        Ok(Some(v)) => v,
        other => panic!("{name}: expected a value return, got {other:?}"),
    }
}

fn int(name: &str, code: Vec<u8>, max_stack: u16, max_locals: u16) -> i32 {
    match run(name, "()I", code, max_stack, max_locals) {
        Value::Int(v) => v,
        other => panic!("{name}: expected an int, got {other:?}"),
    }
}

fn long(name: &str, code: Vec<u8>) -> i64 {
    match run(name, "()J", code, 4, 0) {
        Value::Long(v) => v,
        other => panic!("{name}: expected a long, got {other:?}"),
    }
}

/// A double whose NaN payload collides with the SUB_INT tag space, held in
/// a local WITHOUT the `LKIND_DOUBLE` mark (`set_local_compact` never
/// writes it). `dload_0` must hand it to `dreturn` bit for bit — the same
/// answer the wide `dload 0` spelling always gave.
#[test]
fn dload_n_is_bit_exact_like_wide_dload() {
    const BITS: u64 = 0xFFFC_0000_0000_0005;
    for (name, code) in [
        ("dloadShort", vec![0x26, 0xaf]),      // dload_0; dreturn
        ("dloadWide", vec![0x18, 0x00, 0xaf]), // dload 0; dreturn
    ] {
        let r = run_with(name, "()D", code, 2, 2, |f| {
            f.set_local_compact(0, CompactValue::from_bits(BITS));
        });
        match r {
            Ok(Some(Value::Double(d))) => {
                assert_eq!(d.to_bits(), BITS, "{name} must not reinterpret the payload")
            }
            other => panic!("{name}: expected a double, got {other:?}"),
        }
    }
}

/// JVMS idiv/irem: `MIN / -1` overflows to MIN and `MIN % -1` is 0, with no
/// exception; int and long shift distances are masked to 5 and 6 bits.
#[test]
fn integer_division_and_shift_edges() {
    // iconst_1; bipush 31; ishl  => Integer.MIN_VALUE
    let min = [0x04, 0x10, 31, 0x78];
    let mut idiv = min.to_vec();
    idiv.extend([0x02, 0x6c, 0xac]); // iconst_m1; idiv; ireturn
    assert_eq!(int("idivMinByMinusOne", idiv, 2, 0), i32::MIN);
    let mut irem = min.to_vec();
    irem.extend([0x02, 0x70, 0xac]); // iconst_m1; irem; ireturn
    assert_eq!(int("iremMinByMinusOne", irem, 2, 0), 0);
    // iconst_1; bipush 33; ishl => 1 << (33 & 31)
    assert_eq!(int("ishlMasked", vec![0x04, 0x10, 33, 0x78, 0xac], 2, 0), 2);
    // lconst_1; bipush 65; lshl => 1L << (65 & 63)
    assert_eq!(long("lshlMasked", vec![0x0a, 0x10, 65, 0x79, 0xad]), 2);
    // lconst_1; lconst_0; lcmp
    assert_eq!(int("lcmpGreater", vec![0x0a, 0x09, 0x94, 0xac], 4, 0), 1);
    // (1L << 32) + 1; l2i => 1
    let l2i = vec![0x0a, 0x10, 32, 0x79, 0x0a, 0x61, 0x88, 0xac];
    assert_eq!(int("l2iTruncates", l2i, 4, 0), 1);
}

/// fcmpl/dcmpl answer -1 on NaN, fcmpg/dcmpg answer 1; f2i of NaN is 0 and
/// d2l / d2i saturate at the infinities (JVMS §2.8.3).
#[test]
fn float_compare_and_convert_edges() {
    // fconst_0; fconst_0; fdiv => NaN
    let fnan = [0x0b, 0x0b, 0x6e];
    let mut fcmpl = fnan.to_vec();
    fcmpl.extend([0x0c, 0x95, 0xac]); // fconst_1; fcmpl; ireturn
    assert_eq!(int("fcmplNaN", fcmpl, 2, 0), -1);
    let mut fcmpg = fnan.to_vec();
    fcmpg.extend([0x0c, 0x96, 0xac]);
    assert_eq!(int("fcmpgNaN", fcmpg, 2, 0), 1);
    let mut f2i = fnan.to_vec();
    f2i.extend([0x8b, 0xac]);
    assert_eq!(int("f2iNaN", f2i, 2, 0), 0);
    // dconst_0; dconst_0; ddiv => NaN
    let dnan = [0x0e, 0x0e, 0x6f];
    let mut dcmpl = dnan.to_vec();
    dcmpl.extend([0x0f, 0x97, 0xac]);
    assert_eq!(int("dcmplNaN", dcmpl, 4, 0), -1);
    let mut dcmpg = dnan.to_vec();
    dcmpg.extend([0x0f, 0x98, 0xac]);
    assert_eq!(int("dcmpgNaN", dcmpg, 4, 0), 1);
    // dconst_1; dconst_0; ddiv => +inf; d2l
    assert_eq!(
        long("d2lPosInf", vec![0x0f, 0x0e, 0x6f, 0x8f, 0xad]),
        i64::MAX
    );
    // dconst_1; dneg; dconst_0; ddiv => -inf; d2i
    let d2i = vec![0x0f, 0x77, 0x0e, 0x6f, 0x8e, 0xac];
    assert_eq!(int("d2iNegInf", d2i, 4, 0), i32::MIN);
}

/// JVMS §6.5 `ireturn` narrows to a `boolean`/`byte`/`char`/`short`
/// declared return type, as the baseline JIT does; `int` is untouched.
#[test]
fn ireturn_narrows_to_the_declared_return_type() {
    let ret = |name: &str, desc: &str, code: Vec<u8>| match run(name, desc, code, 2, 0) {
        Value::Int(v) => v,
        other => panic!("{name}: expected an int, got {other:?}"),
    };
    // iconst_2; ireturn
    assert_eq!(ret("narrowZ", "()Z", vec![0x05, 0xac]), 0);
    assert_eq!(ret("narrowI", "()I", vec![0x05, 0xac]), 2);
    // sipush 384; ireturn => (byte) 384 == -128
    assert_eq!(ret("narrowB", "()B", vec![0x11, 0x01, 0x80, 0xac]), -128);
    // iconst_m1; ireturn => (char) -1 == 65535
    assert_eq!(ret("narrowC", "()C", vec![0x02, 0xac]), 65535);
    // iconst_1; bipush 16; ishl; iconst_m1; iadd => 65535; (short) == -1
    let s = vec![0x04, 0x10, 16, 0x78, 0x02, 0x60, 0xac];
    assert_eq!(ret("narrowS", "()S", s), -1);
    assert_eq!(narrow_ireturn(3, b'Z'), 1);
    assert_eq!(narrow_ireturn_value(Value::Long(7), b'Z'), Value::Long(7));
}

/// The pure saturation helpers the conversion arms share with the decoded
/// path.
#[test]
fn conversion_helpers_follow_jvms() {
    assert_eq!(float_to_int(f32::NAN), 0);
    assert_eq!(float_to_int(f32::INFINITY), i32::MAX);
    assert_eq!(float_to_int(f32::NEG_INFINITY), i32::MIN);
    assert_eq!(float_to_int(-1.9), -1);
    assert_eq!(float_to_long(f32::NAN), 0);
    assert_eq!(float_to_long(1e30), i64::MAX);
    assert_eq!(double_to_int(f64::NAN), 0);
    assert_eq!(double_to_int(2147483647.9), i32::MAX);
    assert_eq!(double_to_int(-2147483648.9), i32::MIN);
    assert_eq!(double_to_long(f64::NAN), 0);
    assert_eq!(double_to_long(-1e300), i64::MIN);
    assert_eq!(double_to_long(9.2e18), 9_200_000_000_000_000_000);
}

/// `swap`, `dup`, the `f*` local arms and wide `istore`/`iload` now copy raw
/// slots instead of decoding to `Value`; the answers must not move.
#[test]
fn slot_copy_arms_keep_their_answers() {
    // iconst_1; iconst_2; swap; isub => 2 - 1
    assert_eq!(int("swapInts", vec![0x04, 0x05, 0x5f, 0x64, 0xac], 2, 0), 1);
    // iconst_5; dup; iadd
    assert_eq!(int("dupInt", vec![0x08, 0x59, 0x60, 0xac], 2, 0), 10);
    // fconst_2; fstore_1; fload_1; fload_1; fmul; freturn
    let fl = vec![0x0d, 0x44, 0x23, 0x23, 0x6a, 0xae];
    match run("floatLocals", "()F", fl, 2, 2) {
        Value::Float(f) => assert_eq!(f, 4.0),
        other => panic!("floatLocals: expected a float, got {other:?}"),
    }
    // bipush 7; istore 1; iload 1; ireturn (explicit-index forms)
    assert_eq!(
        int("wideIndexInt", vec![0x10, 7, 0x36, 1, 0x15, 1, 0xac], 1, 2),
        7
    );
}

/// The `iload; iload; iadd`, `iload; iload; if_icmplt` (with its back
/// edge) and `iload; iconst_1; iadd; istore` fusions, in one counted loop:
/// `int s = 0; for (int i = 0; i < 10; i++) s += i; return s;`, plus a
/// branch-free increment.
#[test]
fn superinstruction_fusions_compute_the_unfused_answer() {
    let sum = vec![
        0x03, 0x3b, // 0: iconst_0; istore_0        i = 0
        0x03, 0x3c, // 2: iconst_0; istore_1        s = 0
        0x10, 10, 0x3d, // 4: bipush 10; istore_2   n = 10
        0x1b, 0x1a, 0x60, 0x3c, // 7: iload_1; iload_0; iadd; istore_1
        0x84, 0, 1, // 11: iinc 0 1
        0x1a, 0x1c, 0xa1, 0xff, 0xf7, // 14: iload_0; iload_2; if_icmplt -9 (-> 7)
        0x1b, 0xac, // 19: iload_1; ireturn
    ];
    assert_eq!(int("fusedCountedLoop", sum, 2, 3), 45);
    // iconst_0; istore_0; iload_0; iconst_1; iadd; istore_0; iload_0; ireturn
    let inc = vec![0x03, 0x3b, 0x1a, 0x04, 0x60, 0x3b, 0x1a, 0xac];
    assert_eq!(int("fusedIncrement", inc, 2, 1), 1);
}

// ── Interpreter round i1, wave 2 (lane L1) ──────────────────────────

/// `<prologue>; <group>; if_icmp<op> +5; iconst_0; ireturn; iconst_1;
/// ireturn`: answers 1 iff the fused (or unfused) branch was taken.
fn branch_answer(name: &str, prologue: &[u8], group: &[u8], op: u8, max_locals: u16) -> i32 {
    let mut code = prologue.to_vec();
    code.extend_from_slice(group);
    code.extend_from_slice(&[op, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac]);
    int(name, code, 2, max_locals)
}

/// Every `if_icmp<cond>` fused behind every second-operand shape the
/// fusion admits — `iload_N`, `iload n`, `bipush`, `sipush`, `iconst_*` —
/// and behind an `iload n` first operand, against the plain predicate.
#[test]
fn compare_and_branch_fusions_match_every_condition() {
    let ops: [(u8, fn(i32, i32) -> bool); 6] = [
        (0x9f, |a: i32, b: i32| a == b),
        (0xa0, |a: i32, b: i32| a != b),
        (0xa1, |a: i32, b: i32| a < b),
        (0xa2, |a: i32, b: i32| a >= b),
        (0xa3, |a: i32, b: i32| a > b),
        (0xa4, |a: i32, b: i32| a <= b),
    ];
    let pairs: [(i8, i8); 4] = [(-5, 7), (7, -5), (3, 3), (-128, 127)];
    for (op, pred) in ops {
        for (a, b) in pairs {
            let (ab, bb) = (a as u8, b as u8); // Cast: test operand bytes
            let want = pred(i32::from(a), i32::from(b)) as i32;
            // iload_0; iload_1
            let p = [0x10, ab, 0x3b, 0x10, bb, 0x3c];
            assert_eq!(branch_answer("iloadIload", &p, &[0x1a, 0x1b], op, 2), want);
            // iload_0; bipush b
            let p = [0x10, ab, 0x3b];
            assert_eq!(
                branch_answer("iloadBipush", &p, &[0x1a, 0x10, bb], op, 1),
                want
            );
            // iload 4; iload 5
            let p = [0x10, ab, 0x36, 4, 0x10, bb, 0x36, 5];
            let g = [0x15, 4, 0x15, 5];
            assert_eq!(branch_answer("iloadNIloadN", &p, &g, op, 6), want);
            // iload 4; iload_1
            let p = [0x10, ab, 0x36, 4, 0x10, bb, 0x3c];
            assert_eq!(
                branch_answer("iloadNIload1", &p, &[0x15, 4, 0x1b], op, 5),
                want
            );
        }
        for (a, b) in [(-300i32, 300i32), (1000, 1000), (1000, -300)] {
            // iload_0; sipush b (a set through sipush too)
            let [ah, al] = (a as i16).to_be_bytes(); // Cast: test operand bytes
            let [bh, bl] = (b as i16).to_be_bytes(); // Cast: test operand bytes
            let p = [0x11, ah, al, 0x3b];
            let got = branch_answer("iloadSipush", &p, &[0x1a, 0x11, bh, bl], op, 1);
            assert_eq!(got, pred(a, b) as i32, "sipush op=0x{op:x} a={a} b={b}");
        }
        for k in -1i32..=5 {
            // iload_0; iconst_k, with local 0 = 2
            let konst = (0x03 + k) as u8; // Cast: iconst_m1..iconst_5 opcode
            let got = branch_answer("iloadIconst", &[0x05, 0x3b], &[0x1a, konst], op, 1);
            assert_eq!(got, pred(2, k) as i32, "iconst op=0x{op:x} k={k}");
        }
    }
}

/// javac's top-tested counted loop, unchanged from what `javac` emits for
/// `int s = 0; for (int i = 0; i < n; i++) s += i; return s;`: the
/// `iload; iload; if_icmpge` test fuses, as does the `iinc; goto` step,
/// and the `goto` is the back edge. Then the same loop with its counter
/// and sum in locals 4 and 5 and a `bipush` bound.
#[test]
fn javac_shaped_counted_loops_fuse_and_keep_their_answer() {
    let low = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    assert_eq!(int("javacLoopLowLocals", low, 2, 3), 45);
    let high = vec![
        0x03, 0x36, 4, // 0: iconst_0; istore 4          i = 0
        0x03, 0x36, 5, // 3: iconst_0; istore 5          s = 0
        0x15, 4, 0x10, 10, 0xa2, 0x00, 16, // 6: iload 4; bipush 10; if_icmpge +16 (-> 26)
        0x15, 5, 0x15, 4, 0x60, 0x36, 5, // 13: iload 5; iload 4; iadd; istore 5
        0x84, 4, 1, // 20: iinc 4 1
        0xa7, 0xff, 0xef, // 23: goto -17 (-> 6)
        0x15, 5, 0xac, // 26: iload 5; ireturn
    ];
    assert_eq!(int("javacLoopHighLocals", high, 2, 6), 45);
}

/// A branch that lands INSIDE a fusable group runs the rest of it unfused:
/// the `goto` enters `iload_0; iload_1; if_icmpge` at its `iload_1`, with
/// the first operand already on the stack.
#[test]
fn branch_into_a_fused_group_runs_it_unfused() {
    let code = vec![
        0x05, 0x3c, // 0: iconst_2; istore_1
        0x06, // 2: iconst_3
        0xa7, 0x00, 0x04, // 3: goto +4 (-> 7)
        0x1a, // 6: iload_0 (the group's first bytecode, skipped)
        0x1b, // 7: iload_1
        0xa2, 0x00, 0x05, // 8: if_icmpge +5 (-> 13): 3 >= 2
        0x03, 0xac, // 11: iconst_0; ireturn
        0x04, 0xac, // 13: iconst_1; ireturn
    ];
    assert_eq!(int("midGroupTarget", code, 2, 2), 1);
}

/// `wide iinc` on the fast path: a 16-bit constant each way, and a local
/// past 255 whose load and store stay on the decoded `wide` forms.
#[test]
fn wide_iinc_takes_sixteen_bit_constants_and_indices() {
    let code = vec![
        0x03, 0x3b, // iconst_0; istore_0
        0xc4, 0x84, 0x00, 0x00, 0x03, 0xe8, // wide iinc 0 +1000
        0xc4, 0x84, 0x00, 0x00, 0xff, 0x9c, // wide iinc 0 -100
        0x1a, 0xac, // iload_0; ireturn
    ];
    assert_eq!(int("wideIincConstant", code, 1, 1), 900);
    let code = vec![
        0x03, 0xc4, 0x36, 0x01, 0x2c, // iconst_0; wide istore 300
        0xc4, 0x84, 0x01, 0x2c, 0x00, 0x07, // wide iinc 300 7
        0xc4, 0x15, 0x01, 0x2c, // wide iload 300
        0xac, // ireturn
    ];
    assert_eq!(int("wideIincIndex", code, 1, 301), 7);
}

/// `astore n` / `aload n` move a null reference verbatim, and
/// `ifnull` / `ifnonnull` read it as null through the compact test.
#[test]
fn explicit_index_reference_locals_and_null_tests() {
    for (op, want) in [(0xc6u8, 1), (0xc7u8, 0)] {
        let code = vec![
            0x01, 0x3a, 4, // 0: aconst_null; astore 4
            0x19, 4, // 3: aload 4
            op, 0x00, 0x05, // 5: ifnull / ifnonnull +5 (-> 10)
            0x03, 0xac, // 8: iconst_0; ireturn
            0x04, 0xac, // 10: iconst_1; ireturn
        ];
        assert_eq!(int("nullLocal", code, 1, 5), want, "opcode 0x{op:x}");
    }
    // if_acmpeq / if_acmpne on two nulls, decided on the raw slots.
    for (op, want) in [(0xa5u8, 1), (0xa6u8, 0)] {
        let code = vec![
            0x01, 0x01, // 0: aconst_null; aconst_null
            op, 0x00, 0x05, // 2: if_acmpeq / if_acmpne +5 (-> 7)
            0x03, 0xac, // 5: iconst_0; ireturn
            0x04, 0xac, // 7: iconst_1; ireturn
        ];
        assert_eq!(int("acmpNulls", code, 2, 0), want, "opcode 0x{op:x}");
    }
    let mut mixed = crate::runtime::ValueStack::new(2);
    mixed.push_compact(CompactValue::null());
    mixed.push_int_unchecked(0);
    assert_eq!(
        acmp_on_raw_slots(&mixed),
        None,
        "a non-reference slot keeps the `Value` path"
    );
}

/// The per-method admission: a method whose verifier maps carry a
/// `FastPathVeto` runs on the bounds-checked decoded path, so a
/// `max_stack` overrun is an error return and not a panic inside an
/// unchecked push; a method with no maps keeps the fast path.
#[test]
fn a_vetoed_method_runs_on_the_decoded_path() {
    // Inside the type-map store's addressable range (< 2^22) and far
    // above any id a unit-test VM mints.
    let vetoed = ClassId::new(0x003F_F001);
    let maps = cratonvm_classloading::MethodTypeMapsBuilder::new(0, 1).finish(false);
    assert!(!maps.safe_for_fast_path(), "an incomplete walk is a veto");
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    shared.classes.type_maps.publish(
        vetoed,
        cratonvm_classloading::ClassTypeMaps::new(vec![(
            Arc::from("vetoed"),
            Arc::from("()I"),
            Some(maps),
        )]),
    );
    // 40 x iconst_1; ireturn — forty pushes against max_stack 1. The
    // overrun must exceed the frame's operand-stack padding
    // (`max(max_stack, 16) + 8` slots, frame.rs) or it is absorbed and the
    // method legitimately returns.
    let mut code = vec![0x04u8; 40];
    code.push(0xac);
    let frame_in = |cid: ClassId| {
        Frame::new(
            cid,
            "cratonvm/test/L1Dispatch".to_string(),
            "vetoed".to_string(),
            "()I".to_string(),
            None,
            code.clone(),
            Vec::new(),
            1,
            0,
            &[],
        )
    };
    let probe = frame_in(vetoed);
    assert!(
        !frame_fast_path_admitted(&shared, &probe),
        "the veto closes the fast path"
    );
    assert!(
        !frame_fast_path_admitted(&shared, &probe),
        "and the memo keeps the answer"
    );
    let unmapped = frame_in(ClassId::new(0x00F1_0001));
    assert!(
        frame_fast_path_admitted(&shared, &unmapped),
        "no maps: unchanged"
    );

    match run_on(&shared, vetoed, "vetoed", "()I", code.clone(), 1, 0, |_| {}) {
        Ok(v) => panic!("an overrunning method must not return normally: {v:?}"),
        Err(e) => {
            let text = format!("{e:?}");
            assert!(
                !text.contains("_unchecked") && !text.contains("index out of bounds"),
                "the overrun reached an unchecked push: {text}"
            );
        }
    }
}

/// Wave 6: a method with no verifier maps is admitted to the fast path
/// only when every local its code touches lies inside its frame, short
/// forms included — `iload_3` in a one-slot frame used to index the
/// locals `Vec` out of bounds (the short forms carry no `max_locals`
/// guard) and now runs on the decoded path. Well-formed code, and a jump
/// into an operand byte, are the two other sides of the proof.
#[test]
fn an_unproven_method_whose_locals_overrun_runs_on_the_decoded_path() {
    let unmapped = ClassId::new(0x00F1_0001);
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // Distinct names: the padded code (and so the memo key) is shared per
    // `(class, name, descriptor, body)`.
    let admitted = |name: &str, code: &[u8], max_locals: u16| {
        let frame = Frame::new(
            unmapped,
            "cratonvm/test/L1Dispatch".to_string(),
            name.to_string(),
            "()I".to_string(),
            None,
            code.to_vec(),
            Vec::new(),
            2,
            max_locals,
            &[],
        );
        frame_fast_path_admitted(&shared, &frame)
    };
    let iload_3 = [0x1du8, 0xac]; // iload_3; ireturn
    assert!(
        !admitted("i6LocalsOverrun", &iload_3, 1),
        "iload_3 in one slot"
    );
    assert!(
        admitted("i6LocalsFit", &iload_3, 4),
        "iload_3 in four slots"
    );
    // lconst_0; lstore_3; iconst_0; ireturn — the long takes slots 3 and 4.
    let lstore_3 = [0x09u8, 0x42, 0x03, 0xac];
    assert!(
        !admitted("i6Cat2Overrun", &lstore_3, 4),
        "lstore_3 in four slots"
    );
    assert!(
        admitted("i6Cat2Fit", &lstore_3, 5),
        "lstore_3 in five slots"
    );
    // bipush 0x1d; goto -1: the jump lands on the operand, an `iload_3`.
    let into_operand = [0x10u8, 0x1d, 0xa7, 0xff, 0xff];
    assert!(
        !admitted("i6IntoOperand", &into_operand, 4),
        "jump into an operand"
    );

    // And it runs: the decoded `iload` reads an out-of-range local as
    // uninitialized instead of panicking.
    let _ = run_with("i6LocalsOverrunRuns", "()I", iload_3.to_vec(), 2, 1, |_| {});
}

// ── Interpreter round i1, wave 3 (lane L1) ──────────────────────────

/// An `Object`-tagged slot whose payload never crossed an `ObjectRef`
/// constructor in this process: `to_value` decodes it as a `long`. Built
/// from raw bits (sub-tag `SUB_OBJECT` = 2 at bit 47, the NaN-box prefix
/// taken from `CompactValue::null()`), so building it records nothing.
/// Each caller passes its own address: the first `ObjectRef` built from
/// one records it for the rest of the process.
fn unrecorded_ref(addr: u64) -> CompactValue {
    let prefix = CompactValue::null().raw_bits() & !(0x7u64 << 47);
    let cv = CompactValue::from_bits(prefix | (2u64 << 47) | addr);
    assert_eq!(
        cv.tag(),
        CompactTag::Object,
        "test precondition: object tag"
    );
    assert_eq!(cv.as_object_ptr(), Some(addr), "test precondition: payload");
    assert!(
        matches!(cv.to_value(), Value::Long(_)),
        "test precondition: {addr:#x} must be unrecorded"
    );
    cv
}

/// A fresh VM in which the per-method fast-path gate is closed for
/// `class_id`'s `name`+`desc`, so the method runs on the decoded path (see
/// `a_vetoed_method_runs_on_the_decoded_path`). The maps go into THAT VM's
/// type-map store: a `ClassId` means nothing in another VM.
fn vetoed_vm(class_id: ClassId, name: &str, desc: &str) -> Arc<SharedVm> {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let maps = cratonvm_classloading::MethodTypeMapsBuilder::new(0, 1).finish(false);
    // Asserted: the store is first-writer-wins per class, so a second
    // veto for the same id in one VM would silently publish nothing.
    assert!(shared.classes.type_maps.publish(
        class_id,
        cratonvm_classloading::ClassTypeMaps::new(vec![(
            Arc::from(name),
            Arc::from(desc),
            Some(maps),
        )]),
    ));
    shared
}

fn returned_ref(name: &str, r: MethodCallResult) -> u64 {
    match r {
        Ok(Some(Value::Object(Some(o)))) => o.as_ptr() as u64, // Cast: address compare
        other => panic!("{name}: expected a reference return, got {other:?}"),
    }
}

/// `areturn`, `aload n`, `astore_<n>` and the decoded `Aload` / `Astore` /
/// `Areturn` arms pass a reference with no recorded provenance through as
/// that reference. `areturn` (and, on the decoded path, all three) used to
/// decode it context-free as a `long` and return null.
#[test]
fn reference_arms_keep_an_unrecorded_reference() {
    let desc = "()Ljava/lang/Object;";
    let cases: [(&str, Vec<u8>, u64); 3] = [
        // aload_0; areturn
        ("areturnShort", vec![0x2a, 0xb0], 0x0000_5A5A_0001_0000),
        // aload 4; areturn
        ("areturnWide", vec![0x19, 4, 0xb0], 0x0000_5A5A_0002_0000),
        // aload_0; astore_1; aload_1; areturn
        (
            "areturnViaLocal",
            vec![0x2a, 0x4c, 0x2b, 0xb0],
            0x0000_5A5A_0003_0000,
        ),
    ];
    for (name, code, addr) in cases {
        let cv = unrecorded_ref(addr);
        let r = run_with(name, desc, code, 2, 5, |f| {
            f.set_local_compact(0, cv);
            f.set_local_compact(4, cv);
        });
        assert_eq!(returned_ref(name, r), addr, "{name}");
    }
    // The decoded path: aload_0; astore 4; aload 4; areturn.
    let decoded = ClassId::new(0x003F_F002);
    let shared = vetoed_vm(decoded, "decodedRefs", desc);
    let addr = 0x0000_5A5A_0004_0000;
    let cv = unrecorded_ref(addr);
    let code = vec![0x2a, 0x3a, 4, 0x19, 4, 0xb0];
    let r = run_on(&shared, decoded, "decodedRefs", desc, code, 2, 5, |f| {
        f.set_local_compact(0, cv);
    });
    assert_eq!(returned_ref("decodedRefs", r), addr);
}

/// The operand decode the reference arms share: a plain object slot is a
/// reference, a KIND_LONG-marked slot with the same bits is not (it keeps
/// the validated coerce), and null stays null.
#[test]
fn reference_operand_decode_honours_the_kind_mark() {
    let addr = 0x0000_5A5A_0005_0000;
    let cv = unrecorded_ref(addr);
    assert!(
        ref_slot_object(cv, crate::runtime::ValueStack::KIND_MARK_LONG).is_none(),
        "a long-marked slot is not a reference"
    );
    let r = ref_slot_object(cv, 0).expect("a plain object slot is a reference");
    assert_eq!(r.as_ptr() as u64, addr); // Cast: address compare
    assert!(ref_slot_object(CompactValue::null(), 0).is_none());
    assert_eq!(
        stack_slot_value(
            CompactValue::long(7),
            crate::runtime::ValueStack::KIND_MARK_LONG
        ),
        Value::Long(7)
    );
    let mut stack = crate::runtime::ValueStack::new(2);
    assert!(
        plain_ref_on_top(&stack).is_none(),
        "empty stack: no operand"
    );
    stack.push_int_unchecked(0);
    assert!(
        plain_ref_on_top(&stack).is_none(),
        "an int is not a reference"
    );
}

/// An int-tagged zero in a reference local (the "native returned 0 for
/// null" shape) reads as null through every spelling of `aload` and is
/// stored as null through every spelling of `astore`. `aload_3` used to
/// push it as an int, which `ifnull` calls non-null, while `aload 3` read
/// null.
#[test]
fn an_int_zero_reference_local_is_null_in_every_spelling() {
    let tail = [0xc6u8, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac]; // ifnull +5; 0; 1
    let mut short = vec![0x2d]; // aload_3
    short.extend_from_slice(&tail);
    let mut wide = vec![0x19, 3]; // aload 3
    wide.extend_from_slice(&tail);
    for (name, code) in [("int0Short", short), ("int0Wide", wide)] {
        let r = run_with(name, "()I", code, 1, 4, |f| {
            f.set_local_compact(3, CompactValue::int(0));
        });
        assert!(
            matches!(r, Ok(Some(Value::Int(1)))),
            "{name}: expected null, got {r:?}"
        );
    }
    // iconst_0; astore_1 / astore 4; aload_1 / aload 4; ifnull
    let mut via_short = vec![0x03, 0x4c, 0x2b];
    via_short.extend_from_slice(&tail);
    let mut via_wide = vec![0x03, 0x3a, 4, 0x19, 4];
    via_wide.extend_from_slice(&tail);
    assert_eq!(int("int0StoreShort", via_short, 1, 5), 1);
    assert_eq!(int("int0StoreWide", via_wide, 1, 5), 1);
}

/// `newarray` on the raw-bytecode arm and on the decoded arm runs one
/// body (`op_newarray`), and `athrow` of null fails the same way on both
/// (`op_athrow`) — never with a normal return.
#[test]
fn newarray_and_athrow_share_one_body_on_both_paths() {
    // bipush 5; newarray int; arraylength; ireturn
    let code = vec![0x10, 5, 0xbc, 10, 0xbe, 0xac];
    assert_eq!(int("newarrayFast", code.clone(), 1, 0), 5);
    let decoded = ClassId::new(0x003F_F003);
    let shared = vetoed_vm(decoded, "newarrayDecoded", "()I");
    let r = run_on(
        &shared,
        decoded,
        "newarrayDecoded",
        "()I",
        code,
        1,
        0,
        |_| {},
    );
    assert!(matches!(r, Ok(Some(Value::Int(5)))), "decoded: {r:?}");
    // aconst_null; athrow
    let throw_null = vec![0x01, 0xbf];
    let fast = run_with("athrowNull", "()V", throw_null.clone(), 1, 0, |_| {});
    assert!(fast.is_err(), "athrow of null must not return: {fast:?}");
    // A second VM: in the old process-wide store this second veto for the
    // same class id was dropped (first writer wins), so this half of the
    // test ran on the fast path.
    let shared = vetoed_vm(decoded, "athrowDecoded", "()V");
    let slow = run_on(
        &shared,
        decoded,
        "athrowDecoded",
        "()V",
        throw_null,
        1,
        0,
        |_| {},
    );
    assert!(
        slow.is_err(),
        "decoded athrow of null must not return: {slow:?}"
    );
}

/// A JDWP breakpoint on the `if_icmpge` INSIDE javac's fusable loop test
/// (`iload_2; iload_0; if_icmpge`) fires once per execution of that pc —
/// eleven times for a ten-trip loop — because an armed breakpoint turns
/// fusion off; and the loop's answer does not move.
#[cfg(feature = "experimental-debug")]
#[test]
fn a_breakpoint_inside_a_fused_group_fires_every_time() {
    use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let cid = ClassId::new(0x00F1_0003);
    let method_id = crate::debug::jdwp_method_id("bpLoop", "()I");
    let _ = shared.debug.debug_state.lock().events.set_event_request(
        EventKind::Breakpoint,
        SuspendPolicy::None,
        vec![EventModifier::LocationOnly {
            class_id: u64::from(cid.as_u32()),
            method_id,
            offset: 9,
        }],
    );
    crate::debug::publish_debugger_gates(&shared, &shared.debug.debug_state.lock());
    let (tx, rx) = std::sync::mpsc::channel();
    *shared.debug.debug_event_tx.lock().unwrap() = Some(tx);
    let code = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    // Wave 7: the gate is per method — the same loop in a method with no
    // breakpoint runs without the suspend point and keeps its answer; the
    // breakpoint's own method still reports.
    assert!(!shared
        .debug
        .debugger_gates
        .concerns_method(cid.as_u32(), "bpLoopOther", "()I"));
    let r = run_on(
        &shared,
        cid,
        "bpLoopOther",
        "()I",
        code.clone(),
        2,
        3,
        |_| {},
    );
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "unconcerned: {r:?}");
    assert_eq!(rx.try_iter().count(), 0, "no event outside the method");
    let r = run_on(&shared, cid, "bpLoop", "()I", code, 2, 3, |_| {});
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    let hits: Vec<_> = rx.try_iter().collect();
    assert_eq!(hits.len(), 11, "one event per execution of pc 9: {hits:?}");
    assert!(hits
        .iter()
        .all(|e| e.kind == EventKind::Breakpoint && e.offset == 9 && e.method_id == method_id));
}

// ── Interpreter round i1, wave 4 (lane L1) ──────────────────────────

/// Run a caller (`areturn` at pc 0) over a callee (`aload_0; areturn`,
/// local 0 = a reference to `addr`) in one dispatch loop, with a JVMTI
/// `MethodExit` listener recording, per exit, whether the returned
/// reference sat in a GC root while the callback ran: the parent's
/// operand-stack top (a stackless return) or `native_pin_roots` (the
/// outermost return, and the decoded `areturn`). Returns the records and
/// the reference the loop returned. Runs on `shared` (see [`vetoed_vm`]).
fn method_exit_root_census(
    shared: Arc<SharedVm>,
    callee_class: ClassId,
    callee: &str,
    addr: u64,
) -> (Vec<bool>, u64) {
    use crate::runtime::jvmti::{self, EventCallbacks, EventMode, JvmtiEventKind};
    let _lock = jvmti::jvmti_registry_test_lock();
    let vm = shared.vm_identity;
    jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
    let desc = "()Ljava/lang/Object;";
    let frame = |cid: ClassId, name: &str, code: Vec<u8>| {
        Frame::new(
            cid,
            "cratonvm/test/L1Dispatch".to_string(),
            name.to_string(),
            desc.to_string(),
            None,
            code,
            Vec::new(),
            1,
            1,
            &[],
        )
    };
    let mut thread = JvmThread::new(ThreadId(0), "l1-return-roots");
    thread
        .frames
        .push(frame(ClassId::new(0x00F1_0004), "rootCaller", vec![0xb0]));
    let mut callee_frame = frame(callee_class, callee, vec![0x2a, 0xb0]);
    callee_frame.set_local_compact(0, unrecorded_ref(addr));
    thread.frames.push(callee_frame);
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&records);
    // Test-only: the callback reads the thread through its address. The
    // loop holds only shared borrows of it while `MethodExit` runs.
    let tp: *mut JvmThread = &mut thread;
    let tp_addr = tp as usize; // Cast: address carried into the callback
    let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
    mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodExit, None)
        .expect("enable MethodExit");
    mgr.set_event_callbacks(EventCallbacks {
        method_exit: Some(Box::new(move |_, _, _, _| {
            // SAFETY: `tp_addr` is the live test thread; see above.
            let t = unsafe { &*(tp_addr as *const JvmThread) }; // Cast: see above
            let n = t.frames.len();
            let in_parent = n >= 2
                && plain_ref_on_top(&t.frames[n - 2].stack)
                    .is_some_and(|r| r.as_ptr() as u64 == addr); // Cast: address compare
            let pinned = t.native_pin_roots.iter().any(|r| r.as_ptr() as u64 == addr); // Cast: address compare
            seen.lock().unwrap().push(in_parent || pinned);
        })),
        ..Default::default()
    })
    .expect("install callbacks");
    // SAFETY: `tp` is `&mut thread`; nothing else writes through it.
    let r = execute_frame_from_index(&shared, unsafe { &mut *tp }, 0);
    // The callback holds the thread's address: switch it off before the
    // thread goes out of scope.
    let _ =
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodExit, None);
    jvmti::forget_vm_jvmti_state(vm);
    let got = returned_ref(callee, r);
    let recs = records.lock().unwrap().clone();
    (recs, got)
}

/// A returned reference is in a GC root while JVMTI `MethodExit` runs —
/// on the fast path's stackless and outermost returns and on the decoded
/// `areturn` — and still comes back intact. It used to live only in a
/// Rust local across that callback (and across `FramePop` and the
/// synchronized-monitor release), so a collection an agent triggered
/// there left the caller holding a stale address.
#[test]
fn a_returned_reference_is_rooted_while_method_exit_runs() {
    let addr = 0x0000_5A5A_0006_0000;
    let plain = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let (recs, got) =
        method_exit_root_census(plain, ClassId::new(0x00F1_0005), "rootCallee", addr);
    assert_eq!(got, addr);
    assert_eq!(recs, vec![true, true], "callee exit, then caller exit");
    // The callee on the decoded path (its gate vetoed).
    let decoded = ClassId::new(0x003F_F004);
    let shared = vetoed_vm(decoded, "rootCalleeDecoded", "()Ljava/lang/Object;");
    let addr = 0x0000_5A5A_0007_0000;
    let (recs, got) = method_exit_root_census(shared, decoded, "rootCalleeDecoded", addr);
    assert_eq!(got, addr);
    assert_eq!(
        recs,
        vec![true, true],
        "decoded callee exit, then caller exit"
    );
}

/// `pop_root_frames_keeping_result` pops orphans and the root frame, and
/// hands back a reference result unchanged (no `FramePop` listener: no
/// pin taken, none leaked).
#[test]
fn popping_the_root_frames_keeps_the_result() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "l1-root-pop");
    for name in ["base", "root", "orphan"] {
        thread.frames.push(Frame::new(
            ClassId::new(0x00F1_0006),
            "cratonvm/test/L1Dispatch".to_string(),
            name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            Vec::new(),
            1,
            0,
            &[],
        ));
    }
    let addr = 0x0000_5A5A_0008_0000;
    let r = ref_local_object(unrecorded_ref(addr)).expect("a plausible reference");
    let mut result: MethodCallResult = Ok(Some(Value::Object(Some(r))));
    let pins = thread.native_pin_roots.len();
    pop_root_frames_keeping_result(&shared, &mut thread, 1, &mut result);
    assert_eq!(thread.frames.len(), 1, "the orphan and the root are popped");
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin left behind");
    assert_eq!(returned_ref("rootPop", result), addr);
}

/// Wave 4 fusions keep the unfused answers: `iload_X; iload_Y; iadd;
/// istore_Z` (into another local, and back into an operand), and
/// `aload_N; arraylength` — whose null receiver must still throw from the
/// `arraylength`, never return.
#[test]
fn wave4_fusions_keep_the_unfused_answers() {
    // bipush 3; istore_0; bipush 4; istore_1;
    // iload_0; iload_1; iadd; istore_2; iload_2; ireturn
    let code = vec![
        0x10, 3, 0x3b, 0x10, 4, 0x3c, 0x1a, 0x1b, 0x60, 0x3d, 0x1c, 0xac,
    ];
    assert_eq!(int("iaddIntoLocal", code, 2, 3), 7);
    // ... iload_0; iload_1; iadd; istore_0 (into an operand local)
    let code = vec![
        0x10, 3, 0x3b, 0x10, 4, 0x3c, 0x1a, 0x1b, 0x60, 0x3b, 0x1a, 0xac,
    ];
    assert_eq!(int("iaddIntoOperand", code, 2, 2), 7);
    // ... iload_0; iload_1; iadd; ireturn (no store: the sum is pushed)
    let code = vec![0x10, 3, 0x3b, 0x10, 4, 0x3c, 0x1a, 0x1b, 0x60, 0xac];
    assert_eq!(int("iaddPushed", code, 2, 2), 7);
    // bipush 5; newarray int; astore_1; aload_1; arraylength; ireturn
    let code = vec![0x10, 5, 0xbc, 10, 0x4c, 0x2b, 0xbe, 0xac];
    assert_eq!(int("aloadArraylength", code, 1, 2), 5);
    // aconst_null; astore_1; aload_1; arraylength; ireturn
    let code = vec![0x01, 0x4c, 0x2b, 0xbe, 0xac];
    let r = run_with("aloadArraylengthNull", "()I", code, 1, 2, |_| {});
    assert!(r.is_err(), "arraylength of null must throw: {r:?}");
}

/// The opcode-pair census counts a dispatch only as the fall-through
/// successor of the previous one in the same activation: not across a
/// frame switch, not after a taken branch.
#[test]
fn the_opcode_pair_census_counts_fall_through_pairs_only() {
    use cratonvm_reader::quickened::opcode_pair_census_top;
    let count = |a: u8, b: u8| {
        opcode_pair_census_top(256 * 256)
            .into_iter()
            .find(|r| r.0 == a && r.1 == b)
            .map_or(0, |r| r.2)
    };
    let code = [0u8; 4];
    let p = code.as_ptr();
    // monitorenter (1 byte) at pc 10, monitorexit at pc 11: a pair.
    let c = note_opcode_pair(OpcodePairCursor::NONE, 0, p, 10, 0xc2);
    let c = note_opcode_pair(c, 0, p, 11, 0xc3);
    // monitorexit then monitorenter, but in another frame: no pair.
    let c = note_opcode_pair(c, 1, p, 12, 0xc2);
    // ... and at a pc that is not the successor (a taken branch): none.
    let _ = note_opcode_pair(c, 1, p, 40, 0xc3);
    assert_eq!(count(0xc2, 0xc3), 1, "the one fall-through pair");
    assert_eq!(count(0xc3, 0xc2), 0, "a frame switch breaks the pair");
}

/// A `SUSPEND_EVENT_THREAD` breakpoint parks the thread before the
/// bytecode runs, with its frames and locals published to the JDWP
/// state, until the debugger resumes it; the bytecode then runs once and
/// the method's answer is unchanged. A request on an overload with the
/// same name and bytecode index does not match (method ids hash the
/// descriptor).
#[cfg(feature = "experimental-debug")]
#[test]
fn a_suspending_breakpoint_parks_the_thread_until_resumed() {
    use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
    use std::time::{Duration, Instant};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let cid = ClassId::new(0x00F1_0007);
    let method_id = crate::debug::jdwp_method_id("bpPark", "()I");
    let overload_id = crate::debug::jdwp_method_id("bpPark", "()J");
    assert_ne!(method_id, overload_id, "overloads have distinct ids");
    let at = |mid: u64| {
        vec![EventModifier::LocationOnly {
            class_id: u64::from(cid.as_u32()),
            method_id: mid,
            offset: 22,
        }]
    };
    let req = {
        let mut ds = shared.debug.debug_state.lock();
        let _ = ds.events.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            at(overload_id),
        );
        ds.events.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::EventThread,
            at(method_id),
        )
    };
    crate::debug::publish_debugger_gates(&shared, &shared.debug.debug_state.lock());
    let (tx, rx) = std::sync::mpsc::channel();
    *shared.debug.debug_event_tx.lock().unwrap() = Some(tx);
    let code = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    let (r, observed) = std::thread::scope(|s| {
        // The debugger: wait for the stop, read the published state,
        // resume. A missed stop still resumes at the deadline, so a
        // failure cannot hang the test.
        let resumer = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                {
                    let mut ds = shared.debug.debug_state.lock();
                    if ds.is_thread_suspended(0) {
                        let top = ds
                            .thread_frames
                            .get(&0)
                            .and_then(|f| f.first())
                            .map(|e| (e.frame_id, e.method_id, e.offset));
                        let sum = ds
                            .get_frame_locals(crate::debug::ids::ThreadId(0), 0)
                            .and_then(|l| l.get(1))
                            .and_then(|v| v.as_int());
                        ds.resume_thread(0);
                        return Some((top, sum));
                    }
                    if Instant::now() > deadline {
                        ds.resume_all();
                        return None;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let r = run_on(&shared, cid, "bpPark", "()I", code, 2, 3, |_| {});
        (r, resumer.join().expect("the resumer thread"))
    });
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    assert_eq!(
        observed,
        Some((Some((0, method_id, 22)), Some(45))),
        "the thread stopped at pc 22 with s = 45 published"
    );
    let hits: Vec<_> = rx.try_iter().collect();
    assert_eq!(
        hits.len(),
        1,
        "one stop, and the resumed bytecode is not re-reported"
    );
    assert_eq!(hits[0].request_id, req);
    assert_eq!(hits[0].suspend_policy, SuspendPolicy::EventThread);
    assert!(
        shared.debug.debug_state.lock().thread_frames.is_empty(),
        "the snapshot is withdrawn on resume"
    );
}

/// Wave 8: the JDWP server's work (invocations, heap commands) runs on
/// the thread parked at a suspend point (`debug::run_on_parked_thread`):
/// on the thread named, above its parked frame, with the suspend point
/// suppressed, able to allocate and export what it allocated; it is
/// refused for a thread that is not parked. The parked frame is intact
/// afterwards (the method's answer is unchanged) and the thread leaves
/// the parked set when resumed.
#[cfg(feature = "experimental-debug")]
#[test]
fn debugger_work_runs_on_the_parked_thread() {
    use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
    use crate::debug::ParkedError;
    use std::time::{Duration, Instant};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let cid = ClassId::new(0x00F1_0009);
    let method_id = crate::debug::jdwp_method_id("bpWork", "()I");
    {
        let mut ds = shared.debug.debug_state.lock();
        let _ = ds.events.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::EventThread,
            vec![EventModifier::LocationOnly {
                class_id: u64::from(cid.as_u32()),
                method_id,
                offset: 22,
            }],
        );
    }
    crate::debug::publish_debugger_gates(&shared, &shared.debug.debug_state.lock());
    let code = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    let (r, observed) = std::thread::scope(|s| {
        // The debugger: wait for the park, hand the thread work, resume.
        // A missed park still resumes at the deadline.
        let debugger = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if shared.debug.debug_state.lock().is_parked(0) {
                    break;
                }
                if Instant::now() > deadline {
                    shared.debug.debug_state.lock().resume_all();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let not_parked = crate::debug::run_on_parked_thread(&shared, Some(7), |_, _| ());
            let seen =
                crate::debug::run_on_parked_thread(&shared, Some(0), |shared, thread| {
                    let suppressed = crate::debug::debugger_hooks_suppressed();
                    let array = gc_alloc_array(
                        shared,
                        thread,
                        ClassId::new(0),
                        crate::memory::heap::ArrayElementType::Int,
                        8,
                    )
                    .ok();
                    let id = array.map(|a| {
                        let mut ds = shared.debug.debug_state.lock();
                        crate::debug::export_object(
                            shared,
                            &mut ds,
                            a,
                            crate::debug::ids::ObjectExport::Sent,
                        )
                    });
                    (thread.thread_id.0, thread.frames.len(), suppressed, id)
                });
            let any = crate::debug::run_on_parked_thread(&shared, None, |_, thread| {
                thread.thread_id.0
            });
            shared.debug.debug_state.lock().resume_thread(0);
            Some((not_parked.err(), seen.ok(), any.ok()))
        });
        let r = run_on(&shared, cid, "bpWork", "()I", code, 2, 3, |_| {});
        (r, debugger.join().expect("the debugger thread"))
    });
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    let Some((not_parked, seen, any)) = observed else {
        panic!("the thread never parked");
    };
    assert_eq!(not_parked, Some(ParkedError::NotParked));
    let Some((tid, depth, suppressed, id)) = seen else {
        panic!("the parked thread never answered");
    };
    assert_eq!(
        (tid, depth),
        (0, 1),
        "on the parked thread, above its frame"
    );
    assert!(suppressed, "no suspend point inside the debugger's work");
    if let Some(id) = id {
        let ds = shared.debug.debug_state.lock();
        assert!(
            crate::debug::object_for_id(&shared, &ds, id).is_some(),
            "what the parked thread allocated and exported resolves"
        );
    }
    assert_eq!(any, Some(0), "any parked thread");
    assert!(
        !shared.debug.debug_state.lock().is_parked(0),
        "left on resume"
    );
    assert!(!crate::debug::debugger_hooks_suppressed());
}

/// Wave 9: a breakpoint inside a debugger invocation is reported and
/// stops the invoking thread again (it used to be dropped). The thread,
/// parked at a breakpoint in `bpOuter`, is asked to run `bpInner`, which
/// holds a `SUSPEND_EVENT_THREAD` breakpoint: the event follows the outer
/// one, the published stack grows by `bpInner`'s frame, a resume lets it
/// return, and the invocation answers with the thread suspended again
/// and the outer stop's frames published — before the answer, so a
/// debugger acting on it finds them.
#[cfg(feature = "experimental-debug")]
#[test]
fn a_breakpoint_inside_a_debugger_invocation_stops_the_thread() {
    use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
    use std::time::{Duration, Instant};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let sh: &SharedVm = &shared;
    let cid = ClassId::new(0x00F1_000B);
    let outer_id = crate::debug::jdwp_method_id("bpOuter", "()I");
    let inner_id = crate::debug::jdwp_method_id("bpInner", "()I");
    {
        let mut ds = sh.debug.debug_state.lock();
        for (method_id, offset) in [(outer_id, 22u64), (inner_id, 2u64)] {
            let _ = ds.events.set_event_request(
                EventKind::Breakpoint,
                SuspendPolicy::EventThread,
                vec![EventModifier::LocationOnly {
                    class_id: u64::from(cid.as_u32()),
                    method_id,
                    offset,
                }],
            );
        }
    }
    crate::debug::publish_debugger_gates(sh, &sh.debug.debug_state.lock());
    let (tx, rx) = std::sync::mpsc::channel();
    *sh.debug.debug_event_tx.lock().unwrap() = Some(tx);
    let outer = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    // bpInner: bipush 7; iconst_2; imul; ireturn. The breakpoint is on
    // the iconst_2 (pc 2).
    let inner = vec![0x10, 7, 0x05, 0x68, 0xac];
    // Thread 0 as the debugger sees it: suspended, and its published
    // frame count and top location.
    let top = |sh: &SharedVm| {
        let ds = sh.debug.debug_state.lock();
        (
            ds.is_thread_suspended(0),
            ds.thread_frames
                .get(&0)
                .map(|f| (f.len(), f.first().map(|e| (e.method_id, e.offset)))),
        )
    };
    let (r, (invoked, nested)) = std::thread::scope(|s| {
        // Waits for the outer stop and invokes `bpInner` on the thread.
        let debugger = s.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while top(sh) != (true, Some((1, Some((outer_id, 22))))) {
                if Instant::now() > deadline {
                    sh.debug.debug_state.lock().resume_all();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let answer = crate::debug::run_invocation_on_parked_thread(
                sh,
                0,
                false,
                move |sh, thread| {
                    let frame = Frame::new(
                        cid,
                        "cratonvm/test/L1Dispatch".to_string(),
                        "bpInner".to_string(),
                        "()I".to_string(),
                        None,
                        inner,
                        Vec::new(),
                        2,
                        0,
                        &[],
                    );
                    match execute_prebuilt_frame(sh, thread, frame) {
                        Ok(Some(Value::Int(v))) => Some(v),
                        _ => None,
                    }
                },
            );
            let after = top(sh);
            sh.debug.debug_state.lock().resume_thread(0);
            Some((answer, after))
        });
        // Waits for the stop inside the invocation and resumes it.
        let resumer = s.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let now = top(sh);
                if now.0 && matches!(now.1, Some((2, Some((m, 2)))) if m == inner_id) {
                    sh.debug.debug_state.lock().resume_thread(0);
                    return Some(now);
                }
                if Instant::now() > deadline {
                    sh.debug.debug_state.lock().resume_all();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let r = run_on(sh, cid, "bpOuter", "()I", outer, 2, 3, |_| {});
        let invoked = debugger.join().expect("the debugger thread");
        let nested = resumer.join().expect("the resumer thread");
        (r, (invoked, nested))
    });
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    assert_eq!(
        nested,
        Some((true, Some((2, Some((inner_id, 2)))))),
        "stopped inside the invocation, its frame on top"
    );
    assert_eq!(
        invoked,
        Some((Ok(Some(14)), (true, Some((1, Some((outer_id, 22))))))),
        "the invocation answered; suspended again at the outer stop"
    );
    let hits: Vec<_> = rx
        .try_iter()
        .map(|e| (e.method_id, e.offset, e.suspend_policy))
        .collect();
    assert_eq!(
        hits,
        vec![
            (outer_id, 22, SuspendPolicy::EventThread),
            (inner_id, 2, SuspendPolicy::EventThread),
        ]
    );
    assert!(sh.debug.debug_state.lock().thread_frames.is_empty());
}

/// Wave 9: `StackFrame.SetValues` writes a parked frame's local on its
/// own thread (it answered `NOT_IMPLEMENTED`; before wave 8, success
/// without a write): the summing loop, stopped before `iload_1; ireturn`,
/// returns the value the debugger set, and `GetValues` reads it back
/// first. Every check comes before any write: an unknown thread, a frame
/// id the snapshot does not have, a slot out of range, a kind the slot
/// does not hold, and — with a `LocalVariableTable` entry — the
/// variable's declared kind.
#[cfg(feature = "experimental-debug")]
#[test]
fn stack_frame_set_values_writes_the_parked_frame() {
    use crate::debug::commands::{
        CMD_SF_GET_VALUES, CMD_SF_SET_VALUES, CS_STACK_FRAME, ERR_INVALID_FRAMEID,
        ERR_INVALID_SLOT, ERR_INVALID_THREAD, ERR_TYPE_MISMATCH,
    };
    use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
    use crate::debug::inspect::run_heap_command;
    use crate::debug::protocol::PayloadWriter;
    use std::time::{Duration, Instant};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let sh: &SharedVm = &shared;
    let cid = ClassId::new(0x00F1_000C);
    let method_id = crate::debug::jdwp_method_id("bpSet", "()I");
    {
        let mut ds = sh.debug.debug_state.lock();
        let _ = ds.events.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::EventThread,
            vec![EventModifier::LocationOnly {
                class_id: u64::from(cid.as_u32()),
                method_id,
                offset: 22,
            }],
        );
        // `i` (slot 2) declared a reference: the table's kind wins over
        // the int the slot holds. `s` (slot 1) is declared too
        // (interpreter round i4 wave 85, lane L1): a method with a table
        // answers `INVALID_SLOT` for a slot no entry covers, in
        // `GetValues` since wave 42 (`commands::local_read_refusal`, as
        // HotSpot's `check_slot_type_lvt`), so the read-back below
        // needs the entry.
        ds.method_variables.insert(
            (u64::from(cid.as_u32()), method_id),
            vec![
                crate::debug::VariableInfo {
                    code_index: 0,
                    name: "s".to_string(),
                    signature: "I".to_string(),
                    length: 24,
                    slot: 1,
                },
                crate::debug::VariableInfo {
                    code_index: 0,
                    name: "i".to_string(),
                    signature: "Ljava/lang/Object;".to_string(),
                    length: 24,
                    slot: 2,
                },
            ],
        );
    }
    crate::debug::publish_debugger_gates(sh, &sh.debug.debug_state.lock());
    let code = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    let set = |thread: u64, frame: u64, slot: u32, tagged: &[u8]| {
        let mut pw = PayloadWriter::new();
        pw.put_u64_be(thread);
        pw.put_u64_be(frame);
        pw.put_u32_be(1);
        pw.put_u32_be(slot);
        pw.put_bytes(tagged);
        run_heap_command(sh, CS_STACK_FRAME, CMD_SF_SET_VALUES, &pw.into_bytes()).error_code
    };
    let int = |v: i32| {
        let mut t = vec![b'I'];
        t.extend_from_slice(&v.to_be_bytes());
        t
    };
    let long_one = [b'J', 0, 0, 0, 0, 0, 0, 0, 1];
    let (r, observed) = std::thread::scope(|s| {
        let debugger = s.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !sh.debug.debug_state.lock().is_parked(0) {
                if Instant::now() > deadline {
                    sh.debug.debug_state.lock().resume_all();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let codes = [
                set(7, 0, 1, &int(100)),
                set(0, 5, 1, &int(100)),
                set(0, 0, 9, &int(100)),
                set(0, 0, 1, &long_one),
                set(0, 0, 2, &int(3)),
                set(0, 0, 1, &int(100)),
            ];
            let mut get = PayloadWriter::new();
            get.put_u64_be(0);
            get.put_u64_be(0);
            get.put_u32_be(1);
            get.put_u32_be(1);
            get.put_u8(b'I');
            let read =
                run_heap_command(sh, CS_STACK_FRAME, CMD_SF_GET_VALUES, &get.into_bytes());
            sh.debug.debug_state.lock().resume_thread(0);
            Some((codes, read.data))
        });
        let r = run_on(sh, cid, "bpSet", "()I", code, 2, 3, |_| {});
        (r, debugger.join().expect("the debugger thread"))
    });
    assert!(
        matches!(r, Ok(Some(Value::Int(100)))),
        "the method returns the value the debugger set: {r:?}"
    );
    let Some((codes, read)) = observed else {
        panic!("the thread never parked");
    };
    assert_eq!(
        codes,
        [
            ERR_INVALID_THREAD,
            ERR_INVALID_FRAMEID,
            ERR_INVALID_SLOT,
            ERR_TYPE_MISMATCH,
            ERR_TYPE_MISMATCH,
            0,
        ]
    );
    assert_eq!(
        read,
        vec![0, 0, 0, 1, b'I', 0, 0, 0, 100],
        "GetValues sees it"
    );
    assert!(sh.debug.debug_state.lock().thread_frames.is_empty());
}

/// Wave 8: the OSR and first-call gates stand down for a method the JDWP
/// requests concern, not for every method while any breakpoint is set.
#[cfg(feature = "experimental-debug")]
#[test]
fn the_osr_and_first_call_gates_stand_down_per_method() {
    use crate::debug::events::{EventKind, EventModifier, SuspendPolicy};
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
        return; // a JVMTI listener already keeps everything interpreted
    }
    let cid = ClassId::new(0x00F1_000A);
    assert!(!jvmti_requires_interpreter_for_method(
        &shared, cid, "hot", "()V"
    ));
    let mut ds = crate::debug::DebugState::new();
    let _ = ds.events.set_event_request(
        EventKind::Breakpoint,
        SuspendPolicy::None,
        vec![EventModifier::LocationOnly {
            class_id: u64::from(cid.as_u32()),
            method_id: crate::debug::jdwp_method_id("hot", "()V"),
            offset: 0,
        }],
    );
    crate::debug::publish_debugger_gates(&shared, &ds);
    assert!(jvmti_requires_interpreter_for_method(
        &shared, cid, "hot", "()V"
    ));
    assert!(!jvmti_requires_interpreter_for_method(
        &shared, cid, "cold", "()V"
    ));
    ds.suspend_all();
    crate::debug::publish_debugger_gates(&shared, &ds);
    assert!(
        jvmti_requires_interpreter_for_method(&shared, cid, "cold", "()V"),
        "a suspension concerns every method"
    );
}

// ── Interpreter round i1, wave 5 (lane L1) ──────────────────────────

/// Every raw-bytecode arm's error goes through
/// `classify_fastpath_invoke_error`, which settles a thrown exception the
/// way the decoded path does: `native_pending_return` is drained, a live
/// thrown object wins, and the rooted copy stands in only when the thrown
/// address is no longer an object. A `RuntimeError` is converted (and
/// settled) at the loop top, so the classifier leaves the slot alone.
#[test]
fn the_fast_path_classifier_settles_every_thrown_exception() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "l1-settle");
    let thrown = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let leftover = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    // Cast: address probe
    if shared
        .mem
        .heap
        .is_object_address(thrown.as_ptr() as usize)
        .is_none()
    {
        return; // a backend that cannot answer the liveness probe
    }
    thread.native_pending_return = Some(leftover);
    let live = classify_fastpath_invoke_error(
        &shared,
        &mut thread,
        MethodCallFailed::ExceptionThrown(thrown),
    );
    assert!(
        matches!(live, FastPathInvokeError::Java(e) if e.as_ptr() == thrown.as_ptr()),
        "a live throwable is the one routed"
    );
    assert!(
        thread.native_pending_return.is_none(),
        "the leftover is drained"
    );
    let stale =
        ref_local_object(unrecorded_ref(0x0000_5A5A_0009_0000)).expect("a plausible reference");
    thread.native_pending_return = Some(leftover);
    let moved = classify_fastpath_invoke_error(
        &shared,
        &mut thread,
        MethodCallFailed::ExceptionThrown(stale),
    );
    assert!(
        matches!(moved, FastPathInvokeError::Java(e) if e.as_ptr() == leftover.as_ptr()),
        "a thrown address that is no object falls back to the rooted copy"
    );
    assert!(thread.native_pending_return.is_none());
    thread.native_pending_return = Some(leftover);
    let runtime = classify_fastpath_invoke_error(
        &shared,
        &mut thread,
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::StackOverflowError)),
    );
    assert!(matches!(runtime, FastPathInvokeError::Runtime(_)));
    assert_eq!(
        thread.native_pending_return.map(|r| r.as_ptr()),
        Some(leftover.as_ptr()),
        "the loop top settles a runtime error once it is a throwable"
    );
}

/// End to end: a `RuntimeError` a fast-path arm raises (`newarray` of a
/// negative size) is thrown from the loop top with `native_pending_return`
/// drained, as the decoded path's conversion drains it. It used to be left
/// rooted past the throw.
#[test]
fn a_fast_path_runtime_throw_drains_native_pending_return() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "l1-drain");
    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NegativeArraySizeException",
        None,
    )
    .is_err()
    {
        return; // stripped VM: the throwable cannot be built
    }
    thread.native_pending_return = Some(shared.mem.heap.alloc_object(ClassId::new(0), 0));
    // iconst_m1; newarray int; arraylength; ireturn
    let frame = Frame::new(
        ClassId::new(0x00F1_0008),
        "cratonvm/test/L1Dispatch".to_string(),
        "negativeArray".to_string(),
        "()I".to_string(),
        None,
        vec![0x02, 0xbc, 10, 0xbe, 0xac],
        Vec::new(),
        1,
        0,
        &[],
    );
    let r = execute_prebuilt_frame(&shared, &mut thread, frame);
    assert!(
        matches!(r, Err(MethodCallFailed::ExceptionThrown(_))),
        "NegativeArraySizeException expected: {r:?}"
    );
    assert!(
        thread.native_pending_return.is_none(),
        "the throw drained the leftover"
    );
}

/// javac's array-loop test `iload i; aload a; arraylength; if_icmp<cond>`
/// fuses as a compare-and-branch whose second operand is the array's
/// length, for every condition and in the `iload n` / `aload n`
/// spellings, and answers what the unfused bytecodes answer.
#[test]
fn array_length_compare_and_branch_fuses_for_every_condition() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let branch = |name: &str, prologue: &[u8], group: &[u8], op: u8, max_locals: u16| {
        let mut code = prologue.to_vec();
        code.extend_from_slice(group);
        code.extend_from_slice(&[op, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac]);
        match run_on(&shared, ClassId::new(0x00F1_0001), name, "()I", code, 2, max_locals, |_| {}) {
            Ok(Some(Value::Int(value))) => value,
            other => panic!("{name}: expected an int, got {other:?}"),
        }
    };
    let ops: [(u8, fn(i32, i32) -> bool); 6] = [
        (0x9f, |a: i32, b: i32| a == b),
        (0xa0, |a: i32, b: i32| a != b),
        (0xa1, |a: i32, b: i32| a < b),
        (0xa2, |a: i32, b: i32| a >= b),
        (0xa3, |a: i32, b: i32| a > b),
        (0xa4, |a: i32, b: i32| a <= b),
    ];
    for (op, pred) in ops {
        for len in [0u8, 3, 7] {
            for i in [-1i8, 0, 3, 7, 9] {
                let want = pred(i32::from(i), i32::from(len)) as i32;
                let ib = i as u8; // Cast: test operand bytes
                                  // bipush len; newarray int; astore_1; bipush i; istore_0
                let p = [0x10, len, 0xbc, 10, 0x4c, 0x10, ib, 0x3b];
                let got = branch("iloadAloadLength", &p, &[0x1a, 0x2b, 0xbe], op, 2);
                assert_eq!(got, want, "short op=0x{op:x} i={i} len={len}");
                // bipush len; newarray int; astore 5; bipush i; istore 4
                let p = [0x10, len, 0xbc, 10, 0x3a, 5, 0x10, ib, 0x36, 4];
                let g = [0x15, 4, 0x19, 5, 0xbe];
                let got = branch("iloadNAloadNLength", &p, &g, op, 6);
                assert_eq!(got, want, "wide op=0x{op:x} i={i} len={len}");
            }
        }
    }
}

/// javac's `for (int i = 0; i < a.length; i++) s += i;`, over low and
/// high locals, fuses its length test and keeps its answer; the
/// `aload n; arraylength` pair fuses too; and a null array still throws
/// from the `arraylength` — never a branch, never a return.
#[test]
fn javac_array_loops_fuse_their_length_test() {
    let low = vec![
        0x10, 10, 0xbc, 10, 0x4b, // 0: bipush 10; newarray int; astore_0
        0x03, 0x3c, // 5: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 7: iconst_0; istore_2           i = 0
        0x1c, 0x2a, 0xbe, 0xa2, 0x00,
        13, // 9: iload_2; aload_0; arraylength; if_icmpge +13
        0x1b, 0x1c, 0x60, 0x3c, // 15: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 19: iinc 2 1
        0xa7, 0xff, 0xf3, // 22: goto -13 (-> 9)
        0x1b, 0xac, // 25: iload_1; ireturn
    ];
    assert_eq!(int("arrayLoopLowLocals", low, 2, 3), 45);
    let high = vec![
        0x10, 10, 0xbc, 10, 0x3a, 4, // 0: bipush 10; newarray int; astore 4
        0x03, 0x36, 6, // 6: iconst_0; istore 6          s = 0
        0x03, 0x36, 5, // 9: iconst_0; istore 5          i = 0
        0x15, 5, 0x19, 4, 0xbe, 0xa2, 0x00,
        16, // 12: iload 5; aload 4; arraylength; if_icmpge +16
        0x15, 6, 0x15, 5, 0x60, 0x36, 6, // 20: iload 6; iload 5; iadd; istore 6
        0x84, 5, 1, // 27: iinc 5 1
        0xa7, 0xff, 0xee, // 30: goto -18 (-> 12)
        0x15, 6, 0xac, // 33: iload 6; ireturn
    ];
    assert_eq!(int("arrayLoopHighLocals", high, 2, 7), 45);
    // bipush 7; newarray int; astore 4; aload 4; arraylength; ireturn
    let pair = vec![0x10, 7, 0xbc, 10, 0x3a, 4, 0x19, 4, 0xbe, 0xac];
    assert_eq!(int("aloadNArraylength", pair, 1, 5), 7);
    let nulls: [(&str, Vec<u8>, u16); 3] = [
        // iconst_0; istore_0; aconst_null; astore_1;
        // iload_0; aload_1; arraylength; if_icmpge +5; 0; 1
        (
            "nullLengthLow",
            vec![
                0x03, 0x3b, 0x01, 0x4c, 0x1a, 0x2b, 0xbe, 0xa2, 0, 5, 0x03, 0xac, 0x04, 0xac,
            ],
            2,
        ),
        // the same over locals 4 and 5
        (
            "nullLengthHigh",
            vec![
                0x03, 0x36, 4, 0x01, 0x3a, 5, 0x15, 4, 0x19, 5, 0xbe, 0xa2, 0, 5, 0x03, 0xac,
                0x04, 0xac,
            ],
            6,
        ),
        // aconst_null; astore 4; aload 4; arraylength; ireturn
        ("nullPairHigh", vec![0x01, 0x3a, 4, 0x19, 4, 0xbe, 0xac], 5),
    ];
    for (name, code, locals) in nulls {
        let r = run_with(name, "()I", code, 2, locals, |_| {});
        assert!(r.is_err(), "{name}: arraylength of null must throw: {r:?}");
    }
    // The local reader declines every non-array shape.
    assert_eq!(
        field_fast::arraylength_of_local(CompactValue::null(), false),
        None
    );
    assert_eq!(
        field_fast::arraylength_of_local(CompactValue::int(5), false),
        None
    );
}

/// Wave 24 (L7): `aload_N; iload_M; <x>aload` (javac's `a[i]`) fuses and
/// keeps the unfused answers -- an int loop sum, a long element -- and a
/// null array, an out-of-range and a negative index still throw from the
/// `*aload` (the fused group declines and pushes nothing).
#[test]
fn array_element_reads_fuse_and_keep_their_answers() {
    let sum = vec![
        0x10, 10, 0xbc, 10, 0x4b, // 0: bipush 10; newarray int; astore_0
        0x03, 0x3d, // 5: iconst_0; istore_2               i = 0
        0x1c, 0x2a, 0xbe, 0xa2, 0x00,
        13, // 7: iload_2; aload_0; arraylength; if_icmpge 23
        0x2a, 0x1c, 0x1c, 0x4f, // 13: aload_0; iload_2; iload_2; iastore   a[i] = i
        0x84, 2, 1, // 17: iinc 2 1
        0xa7, 0xff, 0xf3, // 20: goto 7
        0x03, 0x3c, // 23: iconst_0; istore_1              s = 0
        0x03, 0x3d, // 25: iconst_0; istore_2              i = 0
        0x1c, 0x2a, 0xbe, 0xa2, 0x00,
        15, // 27: iload_2; aload_0; arraylength; if_icmpge 45
        0x1b, 0x2a, 0x1c, 0x2e, 0x60,
        0x3c, // 33: iload_1; aload_0; iload_2; iaload; iadd; istore_1
        0x84, 2, 1, // 39: iinc 2 1
        0xa7, 0xff, 0xf1, // 42: goto 27
        0x1b, 0xac, // 45: iload_1; ireturn
    ];
    assert_eq!(int("fusedIntElementSum", sum, 4, 3), 45);
    // iconst_2; newarray long; astore_0; aload_0; iconst_1; lconst_1;
    // lastore; iconst_1; istore_1; aload_0; iload_1; laload; lreturn
    let long_elem = vec![
        0x05, 0xbc, 11, 0x4b, 0x2a, 0x04, 0x0a, 0x50, 0x04, 0x3c, 0x2a, 0x1b, 0x2f, 0xad,
    ];
    match run_with("fusedLongElement", "()J", long_elem, 5, 2, |_| {}) {
        Ok(Some(Value::Long(v))) => assert_eq!(v, 1),
        other => panic!("fusedLongElement: expected 1L, got {other:?}"),
    }
    let throwing: [(&str, Vec<u8>); 3] = [
        // aconst_null; astore_0; iconst_0; istore_1; aload_0; iload_1; iaload; ireturn
        (
            "nullArray",
            vec![0x01, 0x4b, 0x03, 0x3c, 0x2a, 0x1b, 0x2e, 0xac],
        ),
        // iconst_1; newarray int; astore_0; iconst_5; istore_1; aload_0; iload_1; iaload; ireturn
        (
            "indexTooLarge",
            vec![0x04, 0xbc, 10, 0x4b, 0x08, 0x3c, 0x2a, 0x1b, 0x2e, 0xac],
        ),
        // iconst_1; newarray int; astore_0; iconst_m1; istore_1; aload_0; iload_1; iaload; ireturn
        (
            "negativeIndex",
            vec![0x04, 0xbc, 10, 0x4b, 0x02, 0x3c, 0x2a, 0x1b, 0x2e, 0xac],
        ),
    ];
    for (name, code) in throwing {
        let r = run_with(name, "()I", code, 2, 2, |_| {});
        assert!(r.is_err(), "{name}: the iaload must throw: {r:?}");
    }
}

/// Wave 25 (L7): `iload{_N, n}; <x>aload` with the array already on the
/// operand stack (the inner dimension of `m[i][k]`, `this.a[i]`) fuses and
/// keeps the unfused answers; a null array, a negative and an
/// out-of-range index still throw from the `*aload`.
#[test]
fn stacked_array_element_reads_fuse_and_keep_their_answers() {
    // bipush 10; newarray int; astore_0; aload_0; iconst_3; bipush 42;
    // iastore; iconst_3; istore_1; aload_0; dup; pop; iload_1; iaload;
    // ireturn -- `dup; pop` keeps the wave-24 `aload_N; iload_M` group
    // from matching, so the array reaches the `iload_1` on the stack.
    let short_index = vec![
        0x10, 10, 0xbc, 10, 0x4b, 0x2a, 0x06, 0x10, 42, 0x4f, 0x06, 0x3c, 0x2a, 0x59, 0x57,
        0x1b, 0x2e, 0xac,
    ];
    assert_eq!(int("stackedShortIndex", short_index, 4, 2), 42);
    // ... the same with the index in local 4: `iconst_3; istore 4;
    // aload_0; iload 4; iaload; ireturn`.
    let wide_index = vec![
        0x10, 10, 0xbc, 10, 0x4b, 0x2a, 0x06, 0x10, 42, 0x4f, 0x06, 0x36, 4, 0x2a, 0x15, 4,
        0x2e, 0xac,
    ];
    assert_eq!(int("stackedWideIndex", wide_index, 4, 5), 42);
    // iconst_2; newarray long; astore_0; aload_0; iconst_1; lconst_1;
    // lastore; iconst_1; istore_1; aload_0; dup; pop; iload_1; laload; lreturn
    let long_elem = vec![
        0x05, 0xbc, 11, 0x4b, 0x2a, 0x04, 0x0a, 0x50, 0x04, 0x3c, 0x2a, 0x59, 0x57, 0x1b, 0x2f,
        0xad,
    ];
    match run_with("stackedLongElement", "()J", long_elem, 5, 2, |_| {}) {
        Ok(Some(Value::Long(v))) => assert_eq!(v, 1),
        other => panic!("stackedLongElement: expected 1L, got {other:?}"),
    }
    let throwing: [(&str, Vec<u8>); 3] = [
        // aconst_null; iconst_0; istore_1; iload_1; iaload; ireturn
        ("stackedNull", vec![0x01, 0x03, 0x3c, 0x1b, 0x2e, 0xac]),
        // bipush 10; newarray int; iconst_m1; istore 4; iload 4; iaload; ireturn
        (
            "stackedNegative",
            vec![0x10, 10, 0xbc, 10, 0x02, 0x36, 4, 0x15, 4, 0x2e, 0xac],
        ),
        // iconst_1; newarray int; iconst_5; istore_1; iload_1; iaload; ireturn
        (
            "stackedTooLarge",
            vec![0x04, 0xbc, 10, 0x08, 0x3c, 0x1b, 0x2e, 0xac],
        ),
    ];
    for (name, code) in throwing {
        let r = run_with(name, "()I", code, 3, 5, |_| {});
        assert!(r.is_err(), "{name}: the iaload must throw: {r:?}");
    }
}

/// The suspend point delivers JDWP single steps (one `SingleStep` per
/// bytecode for a MIN step, even inside a fusable group) and applies the
/// request modifiers: a breakpoint with `Count(4)` reports only its
/// fourth hit, and one restricted to another thread never reports.
#[cfg(feature = "experimental-debug")]
#[test]
fn single_steps_and_breakpoint_modifiers_are_delivered() {
    use crate::debug::events::{EventKind, EventModifier, StepDepth, StepSize, SuspendPolicy};
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let cid = ClassId::new(0x00F1_0009);
    let method_id = crate::debug::jdwp_method_id("stepLoop", "()I");
    let at_9 = EventModifier::LocationOnly {
        class_id: u64::from(cid.as_u32()),
        method_id,
        offset: 9,
    };
    let (counted, _other_thread) = {
        let mut ds = shared.debug.debug_state.lock();
        let counted = ds.events.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![at_9.clone(), EventModifier::Count(4)],
        );
        let other = ds.events.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![at_9, EventModifier::ThreadOnly { thread_id: 77 }],
        );
        (counted, other)
    };
    crate::debug::publish_debugger_gates(&shared, &shared.debug.debug_state.lock());
    let (tx, rx) = std::sync::mpsc::channel();
    *shared.debug.debug_event_tx.lock().unwrap() = Some(tx);
    // The ten-trip javac loop of `a_breakpoint_inside_a_fused_group_fires_every_time`.
    let code = vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0      n = 10
        0x03, 0x3c, // 3: iconst_0; istore_1           s = 0
        0x03, 0x3d, // 5: iconst_0; istore_2           i = 0
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ];
    let r = run_on(&shared, cid, "stepLoop", "()I", code.clone(), 2, 3, |_| {});
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    let hits: Vec<_> = rx.try_iter().collect();
    assert_eq!(hits.len(), 1, "Count(4) reports once: {hits:?}");
    assert_eq!(hits[0].request_id, counted);

    // A MIN step request on this thread: one event per executed bytecode.
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.events.clear_all();
        ds.events.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![EventModifier::Step {
                thread_id: 0,
                size: StepSize::Min,
                depth: StepDepth::Into,
            }],
        );
        // A step concerns every method (wave 7), `stepStraight` included.
        crate::debug::publish_debugger_gates(&shared, &ds);
    }
    // iconst_1; iconst_2; iadd; ireturn
    let straight = vec![0x04, 0x05, 0x60, 0xac];
    let r = run_on(&shared, cid, "stepStraight", "()I", straight, 2, 0, |_| {});
    assert!(matches!(r, Ok(Some(Value::Int(3)))), "answer: {r:?}");
    let steps: Vec<u64> = rx
        .try_iter()
        .filter(|e| e.kind == EventKind::SingleStep)
        .map(|e| e.offset)
        .collect();
    assert_eq!(steps, vec![0, 1, 2, 3], "one step per bytecode");
    // The ten-trip loop steps through every bytecode it executes, the
    // fusable groups included (fusion is off while stepping): 6 set-up
    // bytecodes, 9 per trip, then the exiting test (3) and the return (2).
    let r = run_on(&shared, cid, "stepLoop", "()I", code, 2, 3, |_| {});
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    let loop_steps = rx
        .try_iter()
        .filter(|e| e.kind == EventKind::SingleStep)
        .count();
    assert_eq!(loop_steps, 6 + 9 * 10 + 3 + 2, "bytecodes executed");
}

// ── Interpreter round i1, wave 7 (lane L1) ──────────────────────────

/// A map-less method's operand-stack depth is proven (and memoized) in
/// the admission walk when it fits `max_stack`; one that declares too
/// small a `max_stack` is still admitted (its locals fit) but not proven,
/// and a method whose code names a member it cannot resolve is not proven
/// either.
#[test]
fn map_less_stack_depth_is_proven_in_the_admission_walk() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // Beyond the type-map store's addressable ids: no maps, no class.
    let cid = ClassId::new(0x00F1_000A);
    let frame = |name: &str, code: Vec<u8>, max_stack: u16| {
        Frame::new(
            cid,
            "cratonvm/test/L1Dispatch".to_string(),
            name.to_string(),
            "()I".to_string(),
            None,
            code,
            Vec::new(),
            max_stack,
            1,
            &[],
        )
    };
    // iconst_1; iconst_2; iadd; ireturn — two words deep.
    let sum = vec![0x04, 0x05, 0x60, 0xac];
    let fits = frame("stackFits", sum.clone(), 2);
    assert!(frame_fast_path_admitted(&shared, &fits));
    assert!(frame_stack_depth_proven(&shared, &fits));
    assert!(frame_stack_depth_proven(&shared, &fits), "memoized");
    let short = frame("stackShort", sum, 1);
    assert!(frame_fast_path_admitted(&shared, &short), "locals fit");
    assert!(!frame_stack_depth_proven(&shared, &short));
    // getstatic #1 of a class the store does not have; ireturn.
    let member = frame("stackMember", vec![0xb2, 0, 1, 0xac], 4);
    assert!(frame_fast_path_admitted(&shared, &member));
    assert!(!frame_stack_depth_proven(&shared, &member));
}
