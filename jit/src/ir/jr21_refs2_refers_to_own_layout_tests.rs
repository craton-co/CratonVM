use super::*;

fn build(class_name: &'static str, referent_index: Option<u32>) -> Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name,
        method_name: "refersTo",
        descriptor: "(Ljava/lang/Object;)Z",
        num_jit_args: 2,
        return_type: b'Z',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let info_ptr = info as *const JitInvokeInfo as usize;
    // `static boolean f(R r, Object o) { return r.refersTo(o); }`
    let code = [0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xac];
    cratonvm_types::flags::with_thread_overrides(
        &[],
        || {
            let mut b = IrBuilder::new(2, 2);
            b.set_param_types(&[IrType::Ref, IrType::Ref]);
            b.set_invoke_info(HashMap::from([(2usize, (info_ptr, 2usize, b'Z'))]));
            b.set_thread_local_layout(None);
            b.set_refers_to_referent_index(referent_index);
            b.build(&code, code.len()).expect("builds")
        },
    )
}

fn ref_loads(g: &Graph) -> Vec<&Node> {
    g.nodes
        .iter()
        .filter(|n| n.op == Op::Load(MemKind::Ref))
        .collect()
}

/// The soft and phantom sites of `R21Tl3Rows` (and the weak one) expand
/// with no `ThreadLocal` layout: one volatile referent read at the invoke.
#[test]
fn jr21_refs2_expands_without_a_thread_local_layout() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    for class in [
        "java/lang/ref/SoftReference",
        "java/lang/ref/PhantomReference",
        "java/lang/ref/WeakReference",
    ] {
        let g = build(class, Some(0));
        let l = ref_loads(&g);
        assert_eq!(l.len(), 1, "{class}: one referent read");
        assert!(l[0].is_volatile_access(), "{class}");
        assert_eq!(l[0].bytecode_pc, Some(2));
        assert!(g.synthetic_field_access_pcs.contains(&2), "{class}");
    }
}

/// Neither a layout nor a referent index (`--compatible`, or the switch
/// off at the table reader): the call alone, as before.
#[test]
fn jr21_refs2_without_either_the_call_stays() {
    let g = build("java/lang/ref/SoftReference", None);
    assert!(ref_loads(&g).is_empty());
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Call { .. }))
            .count(),
        1
    );
}

unsafe extern "C" fn referent_only(_ctx: i64, out: i64, n: i64) -> i64 {
    if n != crate::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS as i64 {
        return 0;
    }
    (out as *mut u64).write(0);
    1
}

// The table reader asks the VM only with both switches on.
