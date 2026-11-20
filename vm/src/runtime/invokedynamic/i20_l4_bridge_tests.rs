//! Round i1 wave 20, lane L4: the compiled concat bridge runs frame-free
//! and answers what the synthetic-frame arm answered; a compiled lambda
//! site reads its zero-capture singleton from the row's slot.

use super::*;
use crate::threading::jvm_thread::ThreadId;

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn concat_site(recipe: &str, constants: &[&str], descriptor: &str) -> JitStringConcatSite {
    JitStringConcatSite {
        kind: JIT_INDY_SITE_CONCAT,
        constant_word: std::sync::atomic::AtomicUsize::new(0),
        recipe: Arc::from(units(recipe).as_slice()),
        constant_args: constants
            .iter()
            .map(|c| Arc::from(units(c).as_slice()))
            .collect(),
        target_descriptor: Arc::from(descriptor),
        arg_types: parse_descriptor_args(descriptor).into_boxed_slice(),
        frame_class_name: Arc::from(crate::runtime::stackwalker::JIT_INDY_BRIDGE_FRAME_CLASS),
        frame_method_name: Arc::from("concat"),
        frame_method_descriptor: Arc::from("()Ljava/lang/String;"),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
        latin1_fast: false,
        tlab_result: false,
        utf16_fast: false,
        inline_plan: std::sync::OnceLock::new(),
        deopt_on_overflow: false,
    }
}

/// `I`, `J` and `L` operands (a `String` and a null) plus a recipe
/// constant: the frame-free bridge renders exactly what the frame arm
/// renders, and leaves the thread's frame stack as it found it.
#[test]
fn the_frame_free_concat_bridge_matches_the_frame_arm() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "i20-l4-concat");
    thread.frames.push(Frame::new(
        ClassId::new(0),
        "Test".into(),
        "caller".into(),
        "()V".into(),
        None,
        vec![],
        vec![],
        0,
        0,
        &[],
    ));
    let site = concat_site(
        "<\x01|\x01|\x01|\x01|\x02>",
        &["K"],
        "(IJLjava/lang/String;Ljava/lang/Object;)Ljava/lang/String;",
    );
    let s = create_java_string(&shared, "s");
    let long = 1_i64 << 40;
    let args: [i64; 4] = [-5, long, s.as_ptr() as i64, 0];
    let site_ptr = &site as *const JitStringConcatSite as usize;
    let depth = thread.frames.len();
    // SAFETY: `site_ptr` points at `site`, live for the call; `args` holds
    // four descriptor-typed slots, the object slot a live `String`.
    let free = unsafe {
        execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 4)
    };
    assert_eq!(thread.frames.len(), depth, "no synthetic frame is left");
    let Ok(Some(free)) = free else {
        panic!("the frame-free concat did not answer");
    };
    let mut values: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
    values.push(Value::Int(-5));
    values.push(Value::Long(long));
    values.push(Value::Object(Some(s)));
    values.push(Value::Object(None));
    let Ok(Some(framed)) =
        execute_jit_string_concat_on_frame(&shared, &mut thread, &site, values)
    else {
        panic!("the frame arm did not answer");
    };
    assert_eq!(thread.frames.len(), depth);
    let want = units("<-5|1099511627776|s|null|K>");
    assert_eq!(
        read_java_string_units(&shared.mem.heap, free).as_deref(),
        Some(want.as_slice())
    );
    assert_eq!(
        read_java_string_units(&shared.mem.heap, framed).as_deref(),
        Some(want.as_slice())
    );
    assert!(
        free.as_ptr() != framed.as_ptr(),
        "each concat result is a fresh String"
    );
}

/// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): a fused
/// `StringBuilder` chain's site, built from its tag-only recipe, renders
/// what the builder would (`String` / `null` / `char` / `int` / `long` /
/// `boolean` / `float` / `double`,
/// literal constants, a UTF16 unit), and a recipe that disagrees with its
/// descriptor or constants, or names an operand type the chain never has,
/// gets no site.
#[test]
fn a_fused_builder_chain_site_renders_the_builder_text() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "sbfuse-concat");
    let s = create_java_string(&shared, "s");
    let cases: [(Vec<u16>, Vec<Vec<u16>>, &str, Vec<i64>, Vec<u16>); 6] = [
        (
            vec![TAG_CONST_UNIT, TAG_ARG_UNIT, TAG_CONST_UNIT, TAG_ARG_UNIT],
            vec![units("k"), units("-")],
            "(II)Ljava/lang/String;",
            vec![5, -35],
            units("k5--35"),
        ),
        (
            vec![
                TAG_ARG_UNIT,
                TAG_ARG_UNIT,
                TAG_ARG_UNIT,
                TAG_ARG_UNIT,
                TAG_CONST_UNIT,
            ],
            vec![units("!")],
            "(Ljava/lang/String;CJLjava/lang/String;)Ljava/lang/String;",
            vec![s.as_ptr() as i64, 0x3a9, i64::MIN, 0],
            {
                let mut u = units("s");
                u.push(0x3a9);
                u.extend(units("-9223372036854775808null!"));
                u
            },
        ),
        (
            vec![TAG_CONST_UNIT, TAG_ARG_UNIT],
            vec![vec![1, 2]],
            "(I)Ljava/lang/String;",
            vec![i64::from(i32::MIN)],
            {
                let mut u = vec![1u16, 2];
                u.extend(units("-2147483648"));
                u
            },
        ),
        (
            vec![TAG_CONST_UNIT, TAG_ARG_UNIT],
            vec![units("pre:")],
            "(Z)Ljava/lang/String;",
            vec![1],
            units("pre:true"),
        ),
        // sbf21 (SBFUSE21-5): `append(double)` / `append(float)` render
        // `Double.toString` / `Float.toString` (derived from the JDK
        // sources, which `StringBuilder.append(D/F)` share).
        (
            vec![TAG_CONST_UNIT, TAG_ARG_UNIT],
            vec![units("d=")],
            "(D)Ljava/lang/String;",
            // Cast: the operand slot carries the double's bits.
            vec![1.5f64.to_bits() as i64],
            units("d=1.5"),
        ),
        (
            vec![TAG_ARG_UNIT, TAG_ARG_UNIT],
            vec![],
            "(FD)Ljava/lang/String;",
            vec![
                i64::from((-0.0f32).to_bits()),
                // Cast: the operand slot carries the double's bits.
                f64::NAN.to_bits() as i64,
            ],
            units("-0.0NaN"),
        ),
    ];
    for (recipe, constants, descriptor, args, want) in cases {
        let site_ptr = make_jit_string_concat_site_from_recipe(&recipe, &constants, descriptor, false)
            .expect("a well-formed chain recipe gets a site");
        // SAFETY: `site_ptr` is the live site just built; `args` holds one
        // descriptor-typed slot per operand, the object slots a live
        // `String` or null.
        let r = unsafe {
            execute_jit_string_concat_raw(
                &shared,
                &mut thread,
                site_ptr,
                args.as_ptr(),
                args.len(),
            )
        };
        let Ok(Some(r)) = r else {
            panic!("the fused chain's concat did not answer");
        };
        assert_eq!(read_java_string_units(&shared.mem.heap, r), Some(want));
        // SAFETY: built by `Box::into_raw` above and never shared.
        drop(unsafe { Box::from_raw(site_ptr as *mut JitStringConcatSite) });
    }
    // Two operand tags, one operand; a constant tag with no constant; a
    // `short` operand; a literal unit in the recipe; a `void`
    // result.
    for (recipe, descriptor) in [
        (vec![TAG_ARG_UNIT, TAG_ARG_UNIT], "(I)Ljava/lang/String;"),
        (vec![TAG_CONST_UNIT], "()Ljava/lang/String;"),
        (vec![TAG_ARG_UNIT], "(S)Ljava/lang/String;"),
        (vec![TAG_ARG_UNIT, 0x41], "(I)Ljava/lang/String;"),
        (vec![TAG_ARG_UNIT, TAG_ARG_UNIT], "(II)V"),
    ] {
        assert_eq!(
            make_jit_string_concat_site_from_recipe(&recipe, &[], descriptor, false),
            None,
            "{descriptor}"
        );
    }
}

/// CONCAT5-4 (`CRATONVM_JIT_CONCAT_TLAB_RESULT`): a site whose general
/// renderer builds its Latin-1 result from the TLAB answers the same text
/// as one that does not, for a Latin-1 result and for a UTF16 one (which
/// keeps the shared-heap arm), and every answer is a fresh String.
#[test]
fn the_tlab_result_arm_renders_what_the_heap_arm_renders() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "str6-concat-tlab");
    for (recipe, want) in [("x=\x01;\u{e9}", "x=7;\u{e9}"), ("\u{3a9}\x01", "\u{3a9}7")] {
        let heap_site = concat_site(recipe, &[], "(I)Ljava/lang/String;");
        let mut tlab_site = concat_site(recipe, &[], "(I)Ljava/lang/String;");
        tlab_site.tlab_result = true;
        let args: [i64; 1] = [7];
        let mut got = Vec::new();
        for site in [&heap_site, &tlab_site] {
            let site_ptr = site as *const JitStringConcatSite as usize;
            // SAFETY: `site_ptr` points at a live site; one `I` slot.
            let r = unsafe {
                execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 1)
            };
            let Ok(Some(r)) = r else {
                panic!("the concat did not answer");
            };
            assert_eq!(
                read_java_string_units(&shared.mem.heap, r),
                Some(units(want))
            );
            got.push(r);
        }
        assert!(got[0].as_ptr() != got[1].as_ptr(), "fresh Strings");
    }
}

/// A `long` whose bits collide with the operand stack's NaN tag space
/// (`execute_string_concat`'s note) is rendered from its descriptor type,
/// and a zero-operand site and an arity mismatch keep their answers.
#[test]
fn the_frame_free_concat_bridge_types_operands_by_descriptor() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "i20-l4-concat-j");
    let site = concat_site("\x01", &[], "(J)Ljava/lang/String;");
    let collide: i64 = -1_125_899_906_842_623;
    let args: [i64; 1] = [collide];
    let site_ptr = &site as *const JitStringConcatSite as usize;
    // SAFETY: as above, one `J` slot.
    let got = unsafe {
        execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 1)
    };
    let Ok(Some(got)) = got else {
        panic!("the concat did not answer");
    };
    assert_eq!(
        read_java_string_units(&shared.mem.heap, got),
        Some(units("-1125899906842623"))
    );
    // Arity mismatch: the historical `Ok(None)`.
    // SAFETY: the count disagrees with the site, so nothing is read.
    let mismatch = unsafe {
        execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 0)
    };
    assert!(matches!(mismatch, Ok(None)));
    let empty = concat_site("ab", &[], "()Ljava/lang/String;");
    let empty_ptr = &empty as *const JitStringConcatSite as usize;
    // SAFETY: zero operands and a null argument block, which the bridge
    // admits for exactly that case.
    let got = unsafe {
        execute_jit_string_concat_raw(&shared, &mut thread, empty_ptr, std::ptr::null(), 0)
    };
    let Ok(Some(got)) = got else {
        panic!("a zero-operand concat did not answer");
    };
    assert_eq!(
        read_java_string_units(&shared.mem.heap, got),
        Some(units("ab"))
    );
}

fn lambda_site(class_id: ClassId, singleton_site: (u64, usize)) -> JitIndyGenericSite {
    JitIndyGenericSite {
        kind: JIT_INDY_SITE_GENERIC,
        constant_word: std::sync::atomic::AtomicUsize::new(0),
        constant_lambda: false,
        capture_fast: false,
        capture_memo: std::sync::atomic::AtomicU64::new(0),
        class_id,
        cp_index: 4,
        target_descriptor: Arc::from("()Ljava/lang/Runnable;"),
        arg_types: Arc::from(Vec::<u8>::new().as_slice()),
        frame_class_name: Arc::from(crate::runtime::stackwalker::JIT_INDY_BRIDGE_FRAME_CLASS),
        frame_method_name: Arc::from(JIT_INDY_FRAME_METHOD),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
        frame_max_stack: 1,
        return_type: b'L',
        singleton_site,
        singleton_slot: std::sync::OnceLock::new(),
        switch_fast: false,
        record_fast: false,
        generic_linked: false,
        linked_direct: false,
        direct: IndyDirectState::default(),
        owner_method: std::sync::OnceLock::new(),
        constant_recompile_requested: std::sync::atomic::AtomicBool::new(false),
    }
}

/// A compiled zero-capture site learns its row's slot from the table and
/// answers from it afterwards -- the minted instance, for its own VM and
/// proxy class only -- and stops answering once the row is dropped.
#[test]
fn a_compiled_lambda_site_answers_its_singleton_from_the_learnt_slot() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "i20-l4-slot");
    let proxy = shared.alloc_lambda_proxy_id();
    let key = lambda_singleton_site_for("i20l4", "()V", 3);
    let site = lambda_site(ClassId::new(0x7_2004), key);
    assert_eq!(
        site.singleton_memo(&shared, proxy),
        None,
        "nothing learnt yet"
    );
    let minted = allocate_lambda_proxy_from_values(&shared, &mut thread, proxy, &mut [], key)
        .expect("an empty heap has room for the singleton");
    site.remember_singleton_slot(&shared, proxy);
    assert_eq!(site.singleton_memo(&shared, proxy), Some(minted));
    let other = shared.alloc_lambda_proxy_id();
    assert_eq!(
        site.singleton_memo(&shared, other),
        None,
        "a memo answers only for the proxy class it learnt"
    );
    let again = allocate_lambda_proxy_from_values(&shared, &mut thread, proxy, &mut [], key)
        .expect("the table answers");
    assert_eq!(again, minted, "the table and the slot agree");
    forget_vm_generic_indy_sites(shared.vm_identity);
    assert_eq!(
        site.singleton_memo(&shared, proxy),
        None,
        "a dropped row's slot is retired"
    );
}

/// JIT round 16 (lane indy, `CRATONVM_JIT_INDY_SWITCH_FAST`): a linked
/// `typeSwitch` site answers from this thread's entry with no synthetic
/// frame, the same index and the same error the frame form gives, and
/// hands the entry's `Arc` back.
#[test]
fn r16_a_switch_fast_site_answers_without_a_frame() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "r16-indy-switch");
    let class_id = ClassId::new(0x7_1601);
    let mut site = lambda_site(class_id, (0, 0));
    site.target_descriptor = Arc::from("(Ljava/lang/Object;I)I");
    site.arg_types = Arc::from(&b"LI"[..]);
    site.frame_max_stack = 3;
    site.return_type = b'I';
    site.switch_fast = true;
    let linked = Arc::new(ResolvedCallSite::TypeSwitch { labels: Vec::new() });
    thread.indy_sites.put(
        class_id,
        4,
        IndySiteCache::epochs_now(),
        CachedIndySite::Other(Some(Arc::clone(&linked))),
    );
    if !matches!(
        thread.indy_sites.get(class_id, 4),
        Some(CachedIndySite::Other(Some(_)))
    ) {
        return; // The epochs moved under the put.
    }
    let site_ptr = &site as *const JitIndyGenericSite as usize;
    let null_target = [0i64, 0i64];
    // SAFETY: a live generic site and a two-word argument block.
    let got = unsafe {
        execute_jit_indy_generic_raw(&shared, &mut thread, site_ptr, null_target.as_ptr(), 2)
    };
    assert!(matches!(got, Ok((-1, None))), "a null target answers -1");
    assert!(thread.frames.is_empty(), "no synthetic frame was pushed");
    assert_eq!(Arc::strong_count(&linked), 2, "the loan went back");
    // An out-of-range restart index raises, as the frame form does.
    let bad_restart = [0i64, 5i64];
    // SAFETY: as above.
    let got = unsafe {
        execute_jit_indy_generic_raw(&shared, &mut thread, site_ptr, bad_restart.as_ptr(), 2)
    };
    assert!(got.is_err(), "restart 5 of 0 labels is out of bounds");
    assert!(thread.frames.is_empty());
}
