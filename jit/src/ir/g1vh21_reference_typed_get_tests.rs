use super::*;

/// `static Object f(Reference r) { return r.get(); }`.
const GET: [u8; 5] = [0x2a, 0xb6, 0x00, 0x01, 0xb0];
const WEAK: u32 = 11;
const SOFT: u32 = 12;

fn layout(weak_class: u32, soft_class: u32) -> ReferenceGetIrLayout {
    ReferenceGetIrLayout {
        weak_class,
        soft_class,
        referent_index: 0,
        weak_referent_row: 17,
        soft_referent_row: 25,
        timestamp_index: 3,
        timestamp_row: 33,
        clock_index: 1,
        clock_volatile: false,
    }
}

fn build(class_name: &'static str, l: Option<ReferenceGetIrLayout>) -> Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name,
        method_name: "get",
        descriptor: "()Ljava/lang/Object;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let info_ptr = info as *const JitInvokeInfo as usize;
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_invoke_info(HashMap::from([(1usize, (info_ptr, 1usize, b'L'))]));
    b.set_reference_get_layout(l);
    b.build(&GET, GET.len()).expect("builds")
}

fn class_tests(g: &Graph) -> Vec<u32> {
    g.nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::ExactClassIs { class_id } => Some(class_id),
            _ => None,
        })
        .collect()
}

fn calls(g: &Graph) -> usize {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .count()
}

fn referent_loads(g: &Graph) -> usize {
    g.nodes
        .iter()
        .filter(|n| n.op == Op::Load(MemKind::Ref))
        .count()
}

fn static_loads(g: &Graph) -> usize {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::LoadStatic { .. }))
        .count()
}

/// A `Reference`-typed site: one exact test of the `WeakReference` id, the
/// referent read (volatile, a keep-alive-gated referent pc), the call kept
/// on the miss edge, no soft-body state (`timestamp`, `clock`).
#[test]
fn g1vh21_a_reference_typed_site_expands_the_weak_body_only() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    let g = build("java/lang/ref/Reference", Some(layout(WEAK, SOFT)));
    assert_eq!(class_tests(&g), vec![WEAK], "exactly WeakReference");
    assert_eq!(calls(&g), 1, "the call stays on the miss edge");
    assert_eq!(referent_loads(&g), 1, "the referent, and no timestamp read");
    assert_eq!(static_loads(&g), 0, "no SoftReference.clock");
    assert!(g.referent_load_pcs.contains(&1), "the keep-alive-gated pc");
    let load = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Load(MemKind::Ref))
        .expect("the referent load");
    assert!(load.is_volatile_access());
    assert_eq!(load.bytecode_pc, Some(1));
}

/// The same site with a layout that admits only `SoftReference` keeps the
/// call alone, and a `SoftReference`-typed site still takes the soft body
/// (the `Reference` arm does not steal it).
#[test]
fn g1vh21_the_reference_arm_needs_the_weak_class_and_leaves_the_typed_arms_alone() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    let soft_only = build("java/lang/ref/Reference", Some(layout(0, SOFT)));
    assert!(class_tests(&soft_only).is_empty());
    assert_eq!(calls(&soft_only), 1);
    assert_eq!(referent_loads(&soft_only), 0);
    let none = build("java/lang/ref/Reference", None);
    assert!(class_tests(&none).is_empty(), "no layout: the call alone");
    assert_eq!(calls(&none), 1);
    let soft = build("java/lang/ref/SoftReference", Some(layout(WEAK, SOFT)));
    assert_eq!(class_tests(&soft), vec![SOFT]);
    assert_eq!(static_loads(&soft), 1, "the soft body reads the clock");
    let weak = build("java/lang/ref/WeakReference", Some(layout(WEAK, SOFT)));
    assert_eq!(class_tests(&weak), vec![WEAK]);
    // Another class's `get()` is never expanded.
    let other = build("java/lang/ref/PhantomReference", Some(layout(WEAK, SOFT)));
    assert!(class_tests(&other).is_empty());
    assert_eq!(calls(&other), 1);
}
