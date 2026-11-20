use super::*;
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

fn site(
    kind: MethodHandleKind,
    owner: &str,
    impl_desc: &str,
    sam: &str,
    instantiated: &str,
    captures: &[char],
) -> LambdaCallSite {
    LambdaCallSite {
        functional_interface: Arc::from("p/F"),
        functional_interface_id: None,
        sam_method_name: Arc::from("apply"),
        sam_descriptor: Arc::from(sam),
        impl_handle: MethodHandle {
            kind,
            class_name: Arc::from(owner),
            member_name: Arc::from("m"),
            descriptor: Arc::from(impl_desc),
        },
        instantiated_descriptor: Arc::from(instantiated),
        capture_types: captures.to_vec(),
        proxy_class_id: ClassId::new(0x8000_0001),
        serializable_flag: false,
    }
}

/// The bcis HotSpot 25.0.3 reports for the shapes of
/// `L3W50LambdaProxyFrames.java`.
#[test]
fn the_forwarding_call_bci_follows_the_spun_body() {
    use MethodHandleKind as K;
    // x -> ..., IntUnaryOperator: iload_1.
    let plain = site(K::InvokeStatic, "p/O", "(I)I", "(I)I", "(I)I", &[]);
    assert_eq!(lambda_forwarding_call_bci(&plain), Some(1));
    // One captured int: aload_0, getfield, iload_1.
    let captured = site(K::InvokeStatic, "p/O", "(II)I", "(I)I", "(I)I", &['I']);
    assert_eq!(lambda_forwarding_call_bci(&captured), Some(5));
    // A bound receiver (`self::m`): the receiver is the capture.
    let bound = site(K::InvokeVirtual, "p/O", "(I)I", "(I)I", "(I)I", &['L']);
    assert_eq!(lambda_forwarding_call_bci(&bound), Some(5));
    // `Made::new` as a Supplier: new, dup.
    let ctor = site(
        K::NewInvokeSpecial,
        "p/Made",
        "()V",
        "()Ljava/lang/Object;",
        "()Lp/Made;",
        &[],
    );
    assert_eq!(lambda_forwarding_call_bci(&ctor), Some(4));
    // Runnable: nothing before the call.
    let run = site(K::InvokeStatic, "p/O", "()V", "()V", "()V", &[]);
    assert_eq!(lambda_forwarding_call_bci(&run), Some(0));
    // `String::length` as a ToIntFunction<String>: aload_1, checkcast.
    let unbound = site(
        K::InvokeVirtual,
        "java/lang/String",
        "()I",
        "(Ljava/lang/Object;)I",
        "(Ljava/lang/String;)I",
        &[],
    );
    assert_eq!(lambda_forwarding_call_bci(&unbound), Some(4));
    // A `Function<Integer, ...>` into an `int` parameter: aload_1,
    // checkcast Integer, invokevirtual intValue.
    let unboxed = site(
        K::InvokeStatic,
        "p/O",
        "(I)Ljava/lang/Object;",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Integer;)Ljava/lang/Object;",
        &[],
    );
    assert_eq!(lambda_forwarding_call_bci(&unboxed), Some(7));
    // A malformed descriptor lists nothing.
    let bad = site(K::InvokeStatic, "p/O", "(Q)I", "(I)I", "(I)I", &[]);
    assert_eq!(lambda_forwarding_call_bci(&bad), None);
}

fn entry(method: &str) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from("p/C"),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    }
}

#[test]
fn insertions_merge_by_index_with_the_first_list_innermost() {
    let a = vec![(1, vec![entry("a1")]), (3, vec![entry("a3")])];
    let b = vec![
        (0, vec![entry("b0")]),
        (3, vec![entry("b3")]),
        (5, vec![entry("b5")]),
    ];
    let merged = merge_standin_insertions(a, b);
    let shape: Vec<(usize, Vec<String>)> = merged
        .iter()
        .map(|(at, es)| (*at, es.iter().map(|e| e.method_name.to_string()).collect()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (0, vec!["b0".to_string()]),
            (1, vec!["a1".to_string()]),
            (3, vec!["a3".to_string(), "b3".to_string()]),
            (5, vec!["b5".to_string()]),
        ]
    );
}

#[test]
fn a_site_implements_only_its_exact_method() {
    let s = site(
        MethodHandleKind::InvokeStatic,
        "p/C",
        "(I)I",
        "(I)I",
        "(I)I",
        &[],
    );
    let mut e = entry("m");
    assert!(!lambda_site_implements(&s, &e), "no descriptor: no match");
    e.method_descriptor = Some(Arc::from("(I)I"));
    assert!(lambda_site_implements(&s, &e));
    e.method_descriptor = Some(Arc::from("(J)I"));
    assert!(!lambda_site_implements(&s, &e));
}

/// Interpreter round i5 wave 102, lane L3: a static or constructor
/// reference's proxy stands below its class's `<clinit>`; a receiver
/// call's does not (its class is initialized already).
#[test]
fn a_static_or_constructor_site_stands_below_its_class_initializer() {
    let clinit = entry("<clinit>");
    for kind in [MethodHandleKind::InvokeStatic, MethodHandleKind::NewInvokeSpecial] {
        let s = site(kind, "p/C", "(I)I", "(I)I", "(I)I", &[]);
        assert!(lambda_site_initializes(&s, &clinit));
        assert!(lambda_site_stands_below(&s, &clinit));
        let other = site(kind, "p/D", "(I)I", "(I)I", "(I)I", &[]);
        assert!(!lambda_site_initializes(&other, &clinit));
    }
    let virt = site(MethodHandleKind::InvokeVirtual, "p/C", "(I)I", "(I)I", "(I)I", &[]);
    assert!(!lambda_site_initializes(&virt, &clinit));
}
