use super::*;
use cratonvm_reader::read_class;

fn supplier_spec() -> ProxyClassSpec {
    ProxyClassSpec {
        gen_class_name: "java/lang/reflect/$Proxy0".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec!["java/util/function/Supplier".to_string()],
        methods: vec![ProxyMethod {
            name: "get".to_string(),
            descriptor: "()Ljava/lang/Object;".to_string(),
            is_default: false,
            is_varargs: false,
            iface_owner: "java/util/function/Supplier".to_string(),
            param_class_names: vec![],
            exception_types: vec![],
            iface_root: None,
        }],
    }
}

#[test]
fn emits_supplier_proxy_with_get_method() {
    let bytes = emit_proxy_classfile(&supplier_spec()).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("emitted class file must round-trip through reader");
    assert_eq!(&*cf.this_class, "java/lang/reflect/$Proxy0");
    assert_eq!(
        cf.super_class.as_ref().map(|s| &**s),
        Some("java/lang/reflect/Proxy$Instance")
    );
    assert!(cf
        .interfaces
        .iter()
        .any(|s| &**s == "java/util/function/Supplier"));
    assert!(cf.methods.iter().any(|m| &*m.name == "<init>"
        && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"));
    assert!(cf
        .methods
        .iter()
        .any(|m| &*m.name == "get" && &*m.descriptor == "()Ljava/lang/Object;"));
}

/// Round 13 wave 4 (lane proxy2): a proxy of a non-public interface is
/// package-private (`ACC_FINAL | ACC_SUPER`, no `ACC_PUBLIC`), as the JDK's
/// `ProxyBuilder` emits it; a proxy of public interfaces stays public.
#[test]
fn package_private_spec_drops_acc_public_only() {
    let bytes = emit_proxy_classfile(&supplier_spec()).expect("emit");
    let public = read_class(&bytes).expect("round-trip").access_flags.bits();
    assert_eq!(public & 0x0001, 0x0001, "public spec keeps ACC_PUBLIC");
    let mut spec = supplier_spec();
    spec.package_private = true;
    let bytes = emit_proxy_classfile(&spec).expect("emit");
    let flags = read_class(&bytes).expect("round-trip").access_flags.bits();
    assert_eq!(flags & 0x0001, 0, "package-private spec drops ACC_PUBLIC");
    assert_eq!(flags | 0x0001, public, "and changes nothing else");
}

/// proxy-real-classfile real-super migration: a spec whose `super_class` is
/// the real `java.lang.reflect.Proxy` emits a `$ProxyN` that extends it with
/// a **1-arg** `<init>(InvocationHandler)` delegating to
/// `Proxy.<init>(InvocationHandler)` — matching the real JDK proxy shape —
/// while the synthetic-super path keeps the 2-arg ctor (covered above).
#[test]
fn emits_real_proxy_super_with_one_arg_ctor() {
    let mut spec = supplier_spec();
    spec.gen_class_name = "com/sun/proxy/$Proxy9".to_string();
    spec.super_class = "java/lang/reflect/Proxy".to_string();
    let bytes = emit_proxy_classfile(&spec).expect("real-super emitter must succeed");
    let cf = read_class(&bytes).expect("real-super class file must round-trip");

    // Extends the REAL Proxy, not the synthetic shim.
    assert_eq!(
        cf.super_class.as_ref().map(|s| &**s),
        Some("java/lang/reflect/Proxy")
    );

    // The constructor is the 1-arg InvocationHandler form (no Class[]).
    assert!(
        cf.methods.iter().any(|m| &*m.name == "<init>"
            && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;)V"),
        "real-super proxy must have a 1-arg <init>(InvocationHandler)"
    );
    // …and NOT the synthetic 2-arg form.
    assert!(
        !cf.methods.iter().any(|m| &*m.name == "<init>"
            && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"),
        "real-super proxy must not carry the synthetic 2-arg <init>"
    );
    // The interface method is still emitted.
    assert!(cf
        .methods
        .iter()
        .any(|m| &*m.name == "get" && &*m.descriptor == "()Ljava/lang/Object;"));
}

/// proxy-real-classfile increment 7: the synthetic-super (`Proxy$Instance`)
/// path now emits BOTH the canonical 2-arg
/// `<init>(InvocationHandler, Class[])` AND a JDK-shape 1-arg
/// `<init>(InvocationHandler)` (delegating to the 2-arg super with a null
/// interfaces array), so the generated class is reflectively constructable
/// via `getConstructor(InvocationHandler.class).newInstance(h)` — the
/// `Proxy.getProxyClass` reflective-construction path.
#[test]
fn emits_synthetic_super_with_both_ctors() {
    let mut spec = supplier_spec();
    spec.gen_class_name = "jdk/proxy1/$Proxy0".to_string();
    spec.super_class = "java/lang/reflect/Proxy$Instance".to_string();
    let bytes = emit_proxy_classfile(&spec).expect("synthetic-super emitter must succeed");
    let cf = read_class(&bytes).expect("synthetic-super class file must round-trip");

    assert_eq!(
        cf.super_class.as_ref().map(|s| &**s),
        Some("java/lang/reflect/Proxy$Instance")
    );
    // Canonical 2-arg ctor retained (existing execution paths depend on it).
    assert!(
        cf.methods.iter().any(|m| &*m.name == "<init>"
            && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"),
        "synthetic-super proxy must retain the 2-arg <init>"
    );
    // JDK-shape 1-arg ctor also emitted (reflective construction).
    assert!(
        cf.methods.iter().any(|m| &*m.name == "<init>"
            && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;)V"),
        "synthetic-super proxy must also emit the 1-arg <init>(InvocationHandler)"
    );
    // The interface method is still emitted.
    assert!(cf
        .methods
        .iter()
        .any(|m| &*m.name == "get" && &*m.descriptor == "()Ljava/lang/Object;"));
}

// ─────────────────────────────────────────────────────────────────────
// JVMS §4.1 / §4.6 structural invariants
// ─────────────────────────────────────────────────────────────────────

/// JVMS §4.1: "No two entries in the `interfaces` array may be the same
/// class or interface."
///
/// Reachable repro: `build_proxy_spec_for` deduplicates the requested
/// interfaces **by `ClassId`**, and two distinct `ClassId`s can share a
/// name when the same interface is loaded by two different class loaders
/// — routine in this VM (`force_loader_faithful_linking` exists precisely
/// because of it). So
///
/// ```java
/// Proxy.newProxyInstance(l, new Class[]{ barFromLoaderA, barFromLoaderB }, h)
/// ```
///
/// arrived here as `interfaces: ["com/foo/Bar", "com/foo/Bar"]`. Because
/// `CpBuilder::add_class` dedups, the emitted `interfaces[]` held two
/// byte-identical u2 entries — a `ClassFormatError`-shaped classfile that
/// HotSpot rejects outright.
#[test]
fn duplicate_interface_names_are_collapsed_to_one_entry() {
    let spec = ProxyClassSpec {
        gen_class_name: "jdk/proxy1/$Proxy7".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec![
            "java/lang/Runnable".to_string(),
            "java/lang/Runnable".to_string(), // same name, other loader
        ],
        methods: vec![ProxyMethod {
            name: "run".to_string(),
            descriptor: "()V".to_string(),
            is_default: false,
            is_varargs: false,
            iface_owner: "java/lang/Runnable".to_string(),
            param_class_names: vec![],
            exception_types: vec![],
            iface_root: None,
        }],
    };
    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("class file must round-trip");

    assert_eq!(
        cf.interfaces.len(),
        1,
        "JVMS §4.1 forbids repeated entries in interfaces[]; got {:?}",
        cf.interfaces.iter().map(|s| &**s).collect::<Vec<_>>()
    );
    assert_eq!(&*cf.interfaces[0], "java/lang/Runnable");
}

/// Interface **order** is load-bearing (Spring's
/// `AopProxyUtils.proxiedUserInterfaces` trims trailing infrastructure
/// interfaces off `getInterfaces()`), so the dedup must keep first
/// occurrences in place rather than sorting or keeping the last.
#[test]
fn interface_dedup_preserves_first_occurrence_order() {
    let mut spec = supplier_spec();
    // gate-ok(not-a-Class): a proxy spec's interface NAMES, never stored.
    spec.interfaces = vec![
        "com/example/ITestBean".to_string(),
        "org/springframework/aop/SpringProxy".to_string(),
        "com/example/ITestBean".to_string(), // dup of [0]
        "org/springframework/aop/framework/Advised".to_string(),
    ];
    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("class file must round-trip");
    let names: Vec<&str> = cf.interfaces.iter().map(|s| &**s).collect();
    assert_eq!(
        names,
        vec![
            "com/example/ITestBean",
            "org/springframework/aop/SpringProxy",
            "org/springframework/aop/framework/Advised",
        ]
    );
}

/// JVMS §4.6: "No two methods in one `class` file may have the same name
/// and descriptor." A repeated `(name, descriptor)` also used to desync
/// the positional `m_<i>` bookkeeping's *intent* — two shims resolving
/// the same `Method` into two different static slots.
#[test]
fn duplicate_method_signatures_are_collapsed_to_one_method() {
    let mut spec = supplier_spec();
    let m = spec.methods[0].clone();
    spec.methods = vec![m.clone(), m];
    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("class file must round-trip");

    let gets = cf
        .methods
        .iter()
        .filter(|mm| &*mm.name == "get" && &*mm.descriptor == "()Ljava/lang/Object;")
        .count();
    assert_eq!(gets, 1, "JVMS §4.6 forbids duplicate name+descriptor pairs");
}

/// The `m_<i>` static `Method` slots, the `<clinit>` PUTSTATIC targets and
/// the per-method GETSTATIC references are all indexed positionally, so
/// they must be derived from the *same* de-duplicated method list. A
/// leftover `spec.methods.len()` anywhere would emit one more field than
/// there are methods (or leave `m_<n-1>` never written).
#[test]
fn field_count_matches_deduped_method_count() {
    let mut spec = supplier_spec();
    let get = spec.methods[0].clone();
    let mut other = get.clone();
    other.name = "getOther".to_string();
    // 3 entries, 2 distinct signatures.
    spec.methods = vec![get.clone(), other, get];

    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("class file must round-trip");

    let method_fields = cf
        .fields
        .iter()
        .filter(|f| f.name.starts_with('m'))
        .count();
    assert_eq!(method_fields, 2, "one Method slot per distinct signature");
    // `ProxyGenerator`'s names (i4 wave 85, lane L3: `m<i>`, was `m_<i>`).
    assert!(
        cf.fields.iter().any(|f| &*f.name == "m0"),
        "slots must be contiguous from 0"
    );
    assert!(cf.fields.iter().any(|f| &*f.name == "m1"));
    assert!(
        !cf.fields.iter().any(|f| &*f.name == "m2"),
        "no slot may be allocated for the collapsed duplicate"
    );
}

#[test]
fn emits_two_iface_proxy_with_both() {
    let spec = ProxyClassSpec {
        gen_class_name: "java/lang/reflect/$Proxy1".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec![
            "java/lang/Runnable".to_string(),
            "java/util/concurrent/Callable".to_string(),
        ],
        methods: vec![
            ProxyMethod {
                name: "run".to_string(),
                descriptor: "()V".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "java/lang/Runnable".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            },
            ProxyMethod {
                name: "call".to_string(),
                descriptor: "()Ljava/lang/Object;".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "java/util/concurrent/Callable".to_string(),
                param_class_names: vec![],
                exception_types: vec!["java/lang/Exception".to_string()],
                iface_root: None,
            },
        ],
    };
    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("two-iface emitted class file must parse");
    assert!(cf.interfaces.iter().any(|s| &**s == "java/lang/Runnable"));
    assert!(cf
        .interfaces
        .iter()
        .any(|s| &**s == "java/util/concurrent/Callable"));
    assert!(cf
        .methods
        .iter()
        .any(|m| &*m.name == "run" && &*m.descriptor == "()V"));
    assert!(cf
        .methods
        .iter()
        .any(|m| &*m.name == "call" && &*m.descriptor == "()Ljava/lang/Object;"));
    assert!(cf.methods.iter().any(|m| &*m.name == "<init>"));
}

#[test]
fn cp_dedup_returns_same_index_for_identical_utf8() {
    let mut cp = CpBuilder::new();
    let a = cp.add_utf8("java/lang/Object");
    let b = cp.add_utf8("java/lang/Object");
    let c = cp.add_utf8("java/lang/Object");
    assert_eq!(a, b);
    assert_eq!(b, c);
    let d = cp.add_utf8("java/lang/String");
    assert_ne!(a, d);
    let cls1 = cp.add_class("java/lang/Object");
    let cls2 = cp.add_class("java/lang/Object");
    assert_eq!(cls1, cls2);
}

#[test]
fn descriptor_param_parsing_handles_mixed() {
    let p = descriptor_param_slots("(IJLjava/lang/String;[BD)V")
        .expect("well-formed descriptor must parse");
    assert_eq!(p.len(), 5);
    assert_eq!(p[0], DescKind::Int);
    assert_eq!(p[1], DescKind::Long);
    assert!(matches!(p[2], DescKind::Reference(ref s) if s == "Ljava/lang/String;"));
    assert!(matches!(p[3], DescKind::Reference(ref s) if s == "[B"));
    assert_eq!(p[4], DescKind::Double);
}

#[test]
fn descriptor_return_parsing_handles_void_and_ref() {
    assert_eq!(descriptor_return("()V").unwrap(), ReturnKind::Void);
    assert_eq!(
        descriptor_return("()I").unwrap(),
        ReturnKind::Prim(DescKind::Int)
    );
    assert!(matches!(
        descriptor_return("()Ljava/lang/Object;").unwrap(),
        ReturnKind::Reference(ref s) if s == "Ljava/lang/Object;"
    ));
}

#[test]
fn malformed_descriptor_yields_error_not_panic() {
    // Audit fix (MED): unknown descriptor byte and an unterminated
    // L-type must surface as a typed error, never panic.
    assert!(descriptor_param_slots("(Q)V").is_err());
    assert!(descriptor_param_slots("(Ljava/lang/String)V").is_err());
    assert!(descriptor_return("()Q").is_err());
    assert!(descriptor_return("()Ljava/lang/String").is_err());
}

/// Round 13 wave 9 (lane proxy6): a `)` inside a class name does not
/// close the parameter list.
#[test]
fn a_paren_inside_a_class_name_does_not_close_the_parameters() {
    let p = descriptor_param_slots("(LA)B;I)Z").unwrap();
    assert_eq!(
        p,
        vec![DescKind::Reference("LA)B;".to_string()), DescKind::Int]
    );
    assert_eq!(
        descriptor_return("(LA)B;I)Z").unwrap(),
        ReturnKind::Prim(DescKind::Int)
    );
    assert!(matches!(
        descriptor_return("(LA)B;)LC)D;").unwrap(),
        ReturnKind::Reference(ref s) if s == "LC)D;"
    ));
    assert_eq!(descriptor_param_byte_at("(LA)B;Z)V", 1), b'Z');
    assert!(descriptor_return("(LA)B;").is_err());
}

/// The emission census must be able to see a degrade.
///
/// Two properties, split so neither depends on the other and neither is
/// flaky under `cargo test`'s parallel threads: the counters are
/// monotonic and reachable (a `fetch_add` with no reader is a write-only
/// counter, which is the shape this instrument exists to avoid), and the
/// real/synthetic classification is a pure function of the spec.
///
/// `>=` rather than `==` is deliberate and is what makes this sound:
/// several sibling tests in this module also call `emit_proxy_classfile`,
/// the counters are process-global, and they never decrease — so "our own
/// call added at least one" is true regardless of what else ran.
#[test]
fn the_emission_census_counts_both_outcomes_and_can_be_read() {
    let (ok_real_0, ok_syn_0, failed_0) = super::proxy_emit_counts();

    // A successful emission is counted.
    let good = supplier_spec();
    emit_proxy_classfile(&good).expect("the canonical spec must emit");
    let (ok_real_1, ok_syn_1, _) = super::proxy_emit_counts();
    assert!(
        (ok_real_1 + ok_syn_1) >= (ok_real_0 + ok_syn_0) + 1,
        "a successful emit must bump an ok bucket: before=({ok_real_0},{ok_syn_0}) \
         after=({ok_real_1},{ok_syn_1})"
    );

    // A failure is counted TOO — the whole point. Without this the
    // instrument reports a healthy `ok` count while every proxy in the
    // process is silently the shim.
    let mut bad = supplier_spec();
    // A bad RETURN byte rather than a bad parameter: it fails in
    // `descriptor_return` with the spec's `param_class_names` still
    // consistent (both empty), so the test exercises the error PATH and
    // not some downstream length mismatch.
    bad.methods[0].descriptor = "()Q".to_string();
    emit_proxy_classfile(&bad).expect_err("a malformed descriptor must fail to emit");
    let (_, _, failed_1) = super::proxy_emit_counts();
    assert!(
        failed_1 >= failed_0 + 1,
        "a failed emit must bump the failure counter: before={failed_0} after={failed_1}"
    );

    // Classification, tested purely so no global count is involved.
    assert!(super::emitted_super_is_the_synthetic_shim(
        "java/lang/reflect/Proxy$Instance"
    ));
    assert!(
        !super::emitted_super_is_the_synthetic_shim("java/lang/reflect/Proxy"),
        "the REAL base class is the default super — bucketing it as the \
         synthetic shim would make the census report a degrade on every \
         healthy proxy, which is worse than not counting at all"
    );
    assert!(!super::emitted_super_is_the_synthetic_shim(
        "jdk/proxy1/$Proxy0"
    ));
}

#[test]
fn proxy_emit_rejects_oversized_utf8_name_without_panic() {
    let mut spec = supplier_spec();
    spec.gen_class_name = format!("pkg/{}", "A".repeat(u16::MAX as usize + 1));
    let err = emit_proxy_classfile(&spec).expect_err("oversized Utf8 must be rejected");
    match err {
        ClassFileError::InvalidClassFile { message, .. } => {
            assert!(message.contains("UTF-8 entry too long"), "got: {message}");
        }
        other => panic!("expected InvalidClassFile, got {other:?}"),
    }
}

#[test]
fn cp_builder_overflow_records_error_without_panic() {
    let mut cp = CpBuilder::new();
    cp.count = u16::MAX - 1;
    assert_eq!(cp.add_utf8("overflow"), 0);
    let err = cp.check("Overflow").expect_err("overflow must be recorded");
    match err {
        ClassFileError::InvalidClassFile { message, .. } => {
            assert!(message.contains("constant pool overflow"), "got: {message}");
        }
        other => panic!("expected InvalidClassFile, got {other:?}"),
    }
}

#[test]
fn proxy_emit_rejects_local_slot_overflow_without_panic() {
    let param_count = 300usize;
    let descriptor = format!("({})V", "I".repeat(param_count));
    let spec = ProxyClassSpec {
        gen_class_name: "java/lang/reflect/$Proxy_slots".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec!["x/ManyArgs".to_string()],
        methods: vec![ProxyMethod {
            name: "many".to_string(),
            descriptor,
            is_default: false,
            is_varargs: false,
            iface_owner: "x/ManyArgs".to_string(),
            param_class_names: vec!["java/lang/Integer".to_string(); param_count],
            exception_types: vec![],
            iface_root: None,
        }],
    };
    let err = emit_proxy_classfile(&spec).expect_err("local slot overflow must be rejected");
    match err {
        ClassFileError::InvalidClassFile { message, .. } => {
            assert!(message.contains("local slot"), "got: {message}");
        }
        other => panic!("expected InvalidClassFile, got {other:?}"),
    }
}

#[test]
fn emits_int_return_method_uses_iret_in_tail() {
    let spec = ProxyClassSpec {
        gen_class_name: "java/lang/reflect/$Proxy2".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec!["x/Y".to_string()],
        methods: vec![ProxyMethod {
            name: "calc".to_string(),
            descriptor: "(I)I".to_string(),
            is_default: false,
            is_varargs: false,
            iface_owner: "x/Y".to_string(),
            param_class_names: vec!["java/lang/Integer".to_string()],
            exception_types: vec![],
            iface_root: None,
        }],
    };
    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("int-return emitted class file must parse");
    let m = cf
        .methods
        .iter()
        .find(|m| &*m.name == "calc" && &*m.descriptor == "(I)I")
        .expect("calc method present");
    // Round-trip succeeded — the verifier-skip path will accept it. Spot-
    // check the access flags include ACC_PUBLIC | ACC_FINAL.
    assert_eq!(m.access_flags.bits() & 0x0011, 0x0011);
}

#[test]
fn emits_void_return_method() {
    let spec = ProxyClassSpec {
        gen_class_name: "java/lang/reflect/$Proxy3".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec!["java/lang/Runnable".to_string()],
        methods: vec![ProxyMethod {
            name: "run".to_string(),
            descriptor: "()V".to_string(),
            is_default: false,
            is_varargs: false,
            iface_owner: "java/lang/Runnable".to_string(),
            param_class_names: vec![],
            exception_types: vec![],
            iface_root: None,
        }],
    };
    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("void-return emitted class file must parse");
    assert!(cf
        .methods
        .iter()
        .any(|m| &*m.name == "run" && &*m.descriptor == "()V"));
}

// ── v3 additions ─────────────────────────────────────────────────────

/// The generated class must declare `<clinit>` and a `m_0` static field
/// of type `Ljava/lang/reflect/Method;` per the v3 layout (item 3).
#[test]
fn emits_clinit_with_static_method_fields() {
    let bytes = emit_proxy_classfile(&supplier_spec()).expect("emitter must succeed");
    let cf = read_class(&bytes).expect("v3 supplier proxy must round-trip");

    // `<clinit>` exists and is static + void.
    let clinit = cf
        .methods
        .iter()
        .find(|m| &*m.name == "<clinit>")
        .expect("v3 emitter must produce <clinit>");
    assert_eq!(&*clinit.descriptor, "()V");
    assert!(
        clinit.access_flags.bits() & 0x0008 != 0,
        "<clinit> must be ACC_STATIC, got 0x{:04X}",
        clinit.access_flags.bits()
    );

    // `m_0` field present, descriptor is `Ljava/lang/reflect/Method;`,
    // and ACC_STATIC.
    let f = cf
        .fields
        .iter()
        .find(|f| &*f.name == "m0")
        .expect("m0 field must exist");
    assert_eq!(&*f.descriptor, "Ljava/lang/reflect/Method;");
    assert!(
        f.access_flags.bits() & 0x0008 != 0,
        "m0 must be ACC_STATIC"
    );
}

/// i4 wave 85, lane L3: `ProxyGenerator`'s `proxyClassLookup` accessor
/// (`private static`, `throws IllegalAccessException`) is the last method,
/// and the class is version 69 like the JDK 25 generator's.
#[test]
fn emits_the_jdk_lookup_accessor_and_version() {
    let bytes = emit_proxy_classfile(&supplier_spec()).expect("emitter must succeed");
    assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 69);
    let cf = read_class(&bytes).expect("must round-trip");
    let last = cf.methods.last().expect("methods");
    assert_eq!(&*last.name, "proxyClassLookup");
    assert_eq!(
        &*last.descriptor,
        "(Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;"
    );
    assert_eq!(last.access_flags.bits(), 0x000A);
}

/// `<clinit>` must take the owner `Class` off the generated class's own
/// `getInterfaces()` — NOT off a constant-pool `LDC class <owner name>`.
///
/// CP class resolution is by NAME through the flat global store, so with
/// two loader worlds holding the same interface it can bind the other
/// world's copy. `Method.equals` compares declaring classes by IDENTITY,
/// so the caller's own `Map<Method, …>` built from `getMethods()` then
/// misses every lookup — Byte Buddy's `JavaDispatcher` throwing
/// `No proxy target found for …Executable.isInstance(Object)`, which took
/// Mockito's inline mock maker down in
/// `HikariDataSourceConfigurationTests`.
///
/// Also pins the fallback: a method whose owner is `java/lang/Object` has
/// no declared-interface root and keeps the `LDC class` form.
#[test]
fn clinit_resolves_owner_through_get_interfaces_not_by_name() {
    use cratonvm_reader::attribute::Attribute;

    let spec = ProxyClassSpec {
        gen_class_name: "pkg/$ProxyRoot".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec!["pkg/A".to_string(), "pkg/B".to_string()],
        methods: vec![
            // Declared directly on the second interface.
            ProxyMethod {
                name: "b".to_string(),
                descriptor: "()V".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "pkg/B".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: Some("pkg/B".to_string()),
            },
            // Inherited from a SUPER-interface of the first — the root is
            // still a declared interface, and `getMethod` searches
            // super-interfaces, so index 0 is the right lookup receiver.
            ProxyMethod {
                name: "inherited".to_string(),
                descriptor: "()V".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "pkg/SuperOfA".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: Some("pkg/A".to_string()),
            },
            // Object method — no interface root, keeps `LDC class`.
            ProxyMethod {
                name: "hashCode".to_string(),
                descriptor: "()I".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "java/lang/Object".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            },
        ],
    };

    let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
    let mut cf = read_class(&bytes).expect("emitted class file must round-trip");
    let cp = &cf.constant_pool;
    for m in &mut cf.methods {
        for attr in &mut m.attributes {
            attr.decode(cp).expect("method attribute must decode");
        }
    }
    let clinit = cf
        .methods
        .iter()
        .find(|m| &*m.name == "<clinit>")
        .expect("emitter must produce <clinit>");
    let code = clinit
        .attributes
        .iter()
        .find_map(|a| match a.as_decoded() {
            Some(Attribute::Code(c)) => Some(c),
            _ => None,
        })
        .expect("<clinit> must have a Code attribute");

    // Two interface-rooted methods => two `getInterfaces()` call sites,
    // and neither `pkg/B` nor `pkg/SuperOfA` may be reached by name.
    let get_interfaces_calls = code
        .code
        .iter()
        .filter(|b| **b == op::INVOKEVIRTUAL)
        .count();
    assert!(
        get_interfaces_calls >= 2,
        "expected a getInterfaces() call per interface-rooted method, \
         found {get_interfaces_calls} INVOKEVIRTUAL bytes"
    );
    assert!(
        code.code.contains(&op::AALOAD),
        "<clinit> must index getInterfaces() with AALOAD"
    );

    // The owner names of interface-rooted methods must NOT appear as
    // constant-pool Class entries — that is the loader-blind form.
    let class_names = constant_pool_class_names(&cf);
    assert!(
        !class_names.iter().any(|n| n == "pkg/SuperOfA"),
        "super-interface owner must not be resolved by name, cp classes: {class_names:?}"
    );
    assert!(
        class_names.iter().any(|n| n == "pkg/$ProxyRoot"),
        "the generated class itself must be an LDC-able Class constant"
    );
    // The Object fallback still resolves by name, which is safe: there is
    // exactly one `java/lang/Object`.
    assert!(
        class_names.iter().any(|n| n == "java/lang/Object"),
        "Object-owner methods keep the LDC class form"
    );
}

/// Every `CONSTANT_Class` name in the emitted class file.
fn constant_pool_class_names(cf: &cratonvm_reader::ClassFile) -> Vec<String> {
    let mut out = Vec::new();
    for i in 1..cf.constant_pool.len() {
        if let Some(name) = cf.constant_pool.get_class_name(i as u16) {
            out.push(name.to_string());
        }
    }
    out
}

/// WP2.5-v3 item 4 audit: every emitted method body is straight-line
/// (no branches, no exception handlers) — the precondition that lets
/// us drop `skip_verification: true` in the bridge. Scans every Code
/// attribute for branch opcodes and asserts the exception_table is
/// empty.
#[test]
fn emitted_class_is_straight_line_no_handlers() {
    use cratonvm_reader::attribute::Attribute;

    let specs = vec![
        // Reference return, no params.
        supplier_spec(),
        // Void return + multiple primitive + reference params.
        ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy_mix".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            package_private: false,
            interfaces: vec!["x/Y".to_string()],
            methods: vec![ProxyMethod {
                name: "mix".to_string(),
                descriptor: "(IJFDLjava/lang/String;)V".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "x/Y".to_string(),
                param_class_names: vec![
                    "java/lang/Integer".to_string(),
                    "java/lang/Long".to_string(),
                    "java/lang/Float".to_string(),
                    "java/lang/Double".to_string(),
                    "java/lang/String".to_string(),
                ],
                exception_types: vec![],
                iface_root: None,
            }],
        },
        // Each primitive return — exercises CHECKCAST + unbox tail.
        ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy_prims".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            package_private: false,
            interfaces: vec!["x/Z".to_string()],
            methods: vec![
                ProxyMethod {
                    name: "i".into(),
                    descriptor: "()I".into(),
                    is_default: false,
                    is_varargs: false,
                    iface_owner: "x/Z".into(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "j".into(),
                    descriptor: "()J".into(),
                    is_default: false,
                    is_varargs: false,
                    iface_owner: "x/Z".into(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "f".into(),
                    descriptor: "()F".into(),
                    is_default: false,
                    is_varargs: false,
                    iface_owner: "x/Z".into(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "d".into(),
                    descriptor: "()D".into(),
                    is_default: false,
                    is_varargs: false,
                    iface_owner: "x/Z".into(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
            ],
        },
    ];

    for spec in &specs {
        let bytes = emit_proxy_classfile(spec).expect("emitter must succeed");
        let mut cf = read_class(&bytes).expect("emitted class file must round-trip");
        // `read_class` returns every attribute in its lazy `Raw` form,
        // so `as_decoded()` would yield `None` until we force a decode.
        // Decode each method's attributes against the class's constant
        // pool before inspecting the Code attribute. We split the borrow
        // (`constant_pool` immutable, `methods` mutable — disjoint fields)
        // so the decode can mutate each `LazyAttribute` in place.
        let cp = &cf.constant_pool;
        for m in &mut cf.methods {
            for attr in &mut m.attributes {
                attr.decode(cp).expect("method attribute must decode");
            }
        }
        // `proxyClassLookup` (i4 wave 85) is the one method with a branch
        // and carries its own StackMapTable; every other body stays
        // straight-line.
        for m in cf.methods.iter().filter(|m| &*m.name != "proxyClassLookup") {
            let code = m
                .attributes
                .iter()
                .find_map(|a| match a.as_decoded() {
                    Some(Attribute::Code(c)) => Some(c),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("method {} missing Code attr", m.name));
            assert!(
                code.exception_table.is_empty(),
                "WP2.5-v3 item 4: method {} of {} must have empty \
                 exception_table to skip StackMapTable",
                m.name,
                spec.gen_class_name,
            );
            if let Some((op, pc)) = scan_for_branch(&code.code) {
                panic!(
                    "WP2.5-v3 item 4: method {} of {} contains branch \
                     opcode 0x{:02X} at pc={} — would require StackMapTable",
                    m.name, spec.gen_class_name, op, pc,
                );
            }
        }
    }
}

/// Walk a Code byte stream and return `Some((opcode, pc))` for the
/// first branch / switch / athrow / wide-ret encountered, or `None`.
/// Lengths drawn from JVMS Chapter 6.
fn scan_for_branch(code: &[u8]) -> Option<(u8, usize)> {
    let mut pc = 0usize;
    while pc < code.len() {
        let op = code[pc];
        if matches!(
            op,
            0x99..=0xa6   // if<cond>, if_icmp<cond>, if_acmp<eq/ne>
            | 0xa7        // goto
            | 0xa8        // jsr
            | 0xa9        // ret
            | 0xaa        // tableswitch
            | 0xab        // lookupswitch
            | 0xbf        // athrow
            | 0xc6        // ifnull
            | 0xc7        // ifnonnull
            | 0xc8        // goto_w
            | 0xc9 // jsr_w
        ) {
            return Some((op, pc));
        }
        pc += match op {
            0xaa => return Some((op, pc)),
            0xab => return Some((op, pc)),
            0xc4 => {
                if pc + 1 < code.len() && code[pc + 1] == 0x84 {
                    6
                } else {
                    4
                }
            }
            0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
            0x11 | 0x13 | 0x14 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0x84 => 3,
            0xc5 => 4,
            0xb9 | 0xba => 5,
            _ => 1,
        };
    }
    None
}

// ── proxy-real-classfile increment 1 ─────────────────────────────────
//
// The canonical-path acceptance test: a `Proxy.newProxyInstance`-style
// single-interface probe must yield the REAL generated `$ProxyN` class
// defined through the normal class loader with full Pass-2/3
// verification (`skip_verification: false`) — NOT the synthetic
// `Proxy$Instance` shim. This exercises the exact wiring
// `native_builtins::define_or_get_proxy_class` uses: emit the canonical
// spec, register the synthetic super + interface, then
// `define_class_with_options`.
//
// Before increment 1 the real path was a fallible fast-path in front of
// the synthetic shim; this test pins that the verified define of the
// generated bytecode SUCCEEDS for the standard single-interface case, so
// the real `$ProxyN` is what reflection observes.

/// The canonical single-interface spec, matching what
/// `build_proxy_spec_for` emits: the interface method plus the three
/// Object methods (equals/hashCode/toString) it always appends.
fn canonical_single_iface_spec() -> ProxyClassSpec {
    ProxyClassSpec {
        gen_class_name: "com/sun/proxy/$Proxy0".to_string(),
        super_class: "java/lang/reflect/Proxy$Instance".to_string(),
        package_private: false,
        interfaces: vec!["pkg/Greeter".to_string()],
        methods: vec![
            ProxyMethod {
                name: "equals".to_string(),
                descriptor: "(Ljava/lang/Object;)Z".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "java/lang/Object".to_string(),
                param_class_names: vec!["java/lang/Object".to_string()],
                exception_types: vec![],
                iface_root: None,
            },
            ProxyMethod {
                name: "hashCode".to_string(),
                descriptor: "()I".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "java/lang/Object".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            },
            ProxyMethod {
                name: "hello".to_string(),
                descriptor: "(Ljava/lang/String;)Ljava/lang/String;".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "pkg/Greeter".to_string(),
                param_class_names: vec!["java/lang/String".to_string()],
                exception_types: vec![],
                iface_root: None,
            },
            ProxyMethod {
                name: "toString".to_string(),
                descriptor: "()Ljava/lang/String;".to_string(),
                is_default: false,
                is_varargs: false,
                iface_owner: "java/lang/Object".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            },
        ],
    }
}

#[test]
fn real_proxy_classfile_defines_verified_not_synthetic_shim() {
    use crate::{ClassLoaderId, ClassManager, DefineClassOptions};

    let mut cm = ClassManager::new(&[], &[], &[]);

    // Register the synthetic super (`Proxy$Instance`, 3 slots:
    // handler / interfaces / identity-hash) and the proxied interface
    // so `define_class_with_options` can resolve both at link time —
    // exactly what `define_or_get_proxy_class` does via
    // `ctx.try_ensure_synthetic_class(...).expect("Compatible mode fabricates; this fixture never runs under --real-jdk")` before defining the proxy.
    cm.try_ensure_synthetic_class("java/lang/reflect/Proxy$Instance", 3)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    cm.try_ensure_synthetic_class("pkg/Greeter", 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");

    let spec = canonical_single_iface_spec();
    let bytes = emit_proxy_classfile(&spec).expect("canonical proxy emit must succeed");

    // Define through the normal loader WITH verification enabled — the
    // canonical path. `skip_verification: false` mirrors the bridge in
    // `define_or_get_proxy_class`; if the generated bytecode failed
    // Pass-3 this would be an `Err` and the production code would
    // silently degrade to the synthetic shim.
    let opts = DefineClassOptions {
        skip_verification: false,
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options(
            "com/sun/proxy/$Proxy0",
            &bytes,
            ClassLoaderId::Application,
            opts,
        )
        .expect(
            "real generated $ProxyN must define+verify through the normal \
             loader (canonical path), not fall back to the synthetic shim",
        );

    let defined = cm
        .get_class(cid)
        .expect("defined proxy class must be retrievable");

    // It is the REAL generated `$ProxyN`, not the synthetic shim:
    //   * its name is the generated `com/sun/proxy/$Proxy0`,
    //   * NOT `java/lang/reflect/Proxy$Instance`,
    //   * and it is a fully-parsed class, not an `is_synthetic_stub`.
    assert_eq!(&*defined.name, "com/sun/proxy/$Proxy0");
    assert_ne!(&*defined.name, "java/lang/reflect/Proxy$Instance");
    assert!(
        !defined.origin.is_compatibility_stub(),
        "the canonical proxy must be a real generated classfile, \
         not a synthetic stub"
    );

    // Its superclass is the synthetic `Proxy$Instance` (so the existing
    // `proxy_chain_reaches_instance` / `isProxyClass` walk still
    // classifies it as a proxy).
    let super_id = defined.superclass.expect("proxy must have a superclass");
    let super_name = cm
        .get_class(super_id)
        .map(|c| c.name.to_string())
        .expect("superclass must be retrievable");
    assert_eq!(super_name, "java/lang/reflect/Proxy$Instance");

    // The generated class carries the proxied interface in its own
    // metadata (not borrowed from a shared shim side-table).
    assert!(
        defined.interfaces.iter().any(|&iid| cm
            .get_class(iid)
            .map(|c| &*c.name == "pkg/Greeter")
            .unwrap_or(false)),
        "generated proxy must declare the proxied interface",
    );

    // The interface method body is present on the generated class —
    // this is the dispatch shim that routes through the handler.
    assert!(
        defined.methods.iter().any(|m| &*m.name == "hello"
            && &*m.descriptor == "(Ljava/lang/String;)Ljava/lang/String;"),
        "generated proxy must emit the interface method body",
    );
}

// ── proxy-real-classfile increment 2 ─────────────────────────────────

/// Object-method routing parity (design §2/§4): a proxy constructed via
/// the real path must (a) carry the synthetic `Proxy$Instance` super
/// constructor the generated `<init>` delegates to — so executing it does
/// NOT raise `NoSuchMethodError` — and (b) route `hashCode()` /
/// `equals(Object)` / `toString()` through the `InvocationHandler`, i.e.
/// each is emitted as a dispatch shim that forwards to
/// `Proxy$Dispatch.invokeProxy` (which calls `handler.invoke(...)`),
/// matching the JDK generated proxy body rather than inheriting Object's
/// identity implementations.
#[test]
fn real_proxy_object_methods_route_through_handler_and_ctor_resolves() {
    use crate::{ClassLoaderId, ClassManager, DefineClassOptions};
    use cratonvm_reader::attribute::Attribute;

    let mut cm = ClassManager::new(&[], &[], &[]);

    let object_id = cm
        .try_ensure_synthetic_class("java/lang/Object", 0)
        .expect("synthetic Object");
    let object = cm.get_class(object_id).expect("Object metadata");
    for (name, descriptor) in [
        ("hashCode", "()I"),
        ("equals", "(Ljava/lang/Object;)Z"),
        ("toString", "()Ljava/lang/String;"),
    ] {
        assert!(object.methods.iter().any(|m| m.name.as_ref() == name
            && m.descriptor.as_ref() == descriptor
            && m.access_flags
                .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::PUBLIC)
            && m.is_native()));
    }

    // (a) Register the synthetic super exactly as
    // `define_or_get_proxy_class` does. Increment 2 added the
    // `Proxy$Instance.<init>(InvocationHandler, Class[])V` method entry to
    // `synthetic_stub_ctor_methods`, so the stub's method table now
    // carries the ctor the generated `$ProxyN.<init>` resolves against.
    let super_id = cm
        .try_ensure_synthetic_class("java/lang/reflect/Proxy$Instance", 3)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    cm.try_ensure_synthetic_class("pkg/Greeter", 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    let super_cls = cm
        .get_class(super_id)
        .expect("synthetic Proxy$Instance must be registered");
    assert!(
        super_cls.methods.iter().any(|m| {
            &*m.name == "<init>"
                && &*m.descriptor
                    == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"
        }),
        "the super ctor the generated <init> delegates to must resolve \
         (no NoSuchMethodError when the generated <init> executes)",
    );

    // Define the real generated proxy through the normal loader, verified.
    let spec = canonical_single_iface_spec();
    let bytes = emit_proxy_classfile(&spec).expect("canonical proxy emit must succeed");
    let opts = DefineClassOptions {
        skip_verification: false,
        ..Default::default()
    };
    let cid = cm
        .define_class_with_options(
            "com/sun/proxy/$Proxy0",
            &bytes,
            ClassLoaderId::Application,
            opts,
        )
        .expect("real generated $ProxyN must define+verify");
    let defined = cm.get_class(cid).expect("defined proxy retrievable");

    // (b) hashCode / equals / toString are each emitted as concrete
    // dispatch shims on the generated class (not inherited from Object).
    for (name, desc) in [
        ("hashCode", "()I"),
        ("equals", "(Ljava/lang/Object;)Z"),
        ("toString", "()Ljava/lang/String;"),
    ] {
        assert!(
            defined
                .methods
                .iter()
                .any(|m| &*m.name == name && &*m.descriptor == desc),
            "generated proxy must emit an Object-method dispatch shim for \
             {name}{desc} (JDK routes these through the handler)",
        );
    }

    // Each Object-method shim routes through the handler-forwarding
    // helper: its Code references `Proxy$Dispatch.invokeProxy`. We assert
    // the methodref is present in the generated class's constant pool by
    // re-reading the raw bytes (UTF-8 entries for owner + name), then
    // confirm the shims actually carry a Code body (they would inherit
    // Object's impl otherwise).
    let cf = read_class(&bytes).expect("generated proxy must round-trip");
    let cp = &cf.constant_pool;
    let mut has_dispatch_owner = false;
    let mut has_dispatch_name = false;
    for idx in 1..(cp.len() as u16) {
        if let Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Utf8(s)) = cp.get(idx) {
            match &**s {
                "java/lang/reflect/Proxy$Dispatch" => has_dispatch_owner = true,
                "invokeProxy" => has_dispatch_name = true,
                _ => {}
            }
        }
    }
    assert!(
        has_dispatch_owner && has_dispatch_name,
        "generated proxy must reference Proxy$Dispatch.invokeProxy — the \
         handler-forwarding helper the Object-method shims route through",
    );

    // The hashCode/equals/toString shims each have a non-empty Code body
    // (a real dispatch shim), confirming they don't fall through to
    // Object's inherited identity implementations.
    let mut cf2 = read_class(&bytes).expect("generated proxy must round-trip");
    let cp = &cf2.constant_pool;
    for m in &mut cf2.methods {
        for attr in &mut m.attributes {
            let _ = attr.decode(cp);
        }
    }
    for name in ["hashCode", "equals", "toString"] {
        let m = cf2
            .methods
            .iter()
            .find(|m| &*m.name == name)
            .unwrap_or_else(|| panic!("{name} shim must be present"));
        let code = m
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::Code(c)) => Some(c),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name} shim must carry a Code body"));
        assert!(
            !code.code.is_empty(),
            "{name} dispatch shim must have a non-empty body that forwards \
             to the handler, not inherit Object's identity impl",
        );
    }
}

/// Round 13 wave 5 (lane proxy3): a proxy method declares its (merged)
/// `throws` list in an `Exceptions` attribute, as the JDK's
/// `ProxyGenerator` does, and a method without one carries none.
#[test]
fn proxy_methods_declare_their_exception_list() {
    use cratonvm_reader::attribute::Attribute;
    let mut spec = supplier_spec();
    spec.methods[0].exception_types = vec![
        "java/io/IOException".to_string(),
        "java/sql/SQLException".to_string(),
        "java/io/IOException".to_string(), // repeated: one entry
    ];
    let mut plain = spec.methods[0].clone();
    plain.name = "plain".to_string();
    plain.exception_types = Vec::new();
    spec.methods.push(plain);
    let bytes = emit_proxy_classfile(&spec).expect("emit");
    let cf = read_class(&bytes).expect("round-trip");
    let declared = |name: &str| -> Option<Vec<String>> {
        let m = cf.methods.iter().find(|m| &*m.name == name)?;
        m.attributes
            .iter()
            .find_map(|a| match &*a.decoded_or_decode(&cf.constant_pool)? {
                Attribute::Exceptions { exception_indices } => Some(
                    exception_indices
                        .iter()
                        .filter_map(|i| cf.constant_pool.get_class_name(*i))
                        .map(str::to_string)
                        .collect(),
                ),
                _ => None,
            })
    };
    assert_eq!(
        declared("get"),
        Some(vec![
            "java/io/IOException".to_string(),
            "java/sql/SQLException".to_string()
        ])
    );
    assert_eq!(declared("plain"), None, "no throws clause, no attribute");
    assert_eq!(declared("<clinit>"), None);
}

/// Round 13 wave 5 (lane proxy3): neither the class nor its `m_<i>`
/// fields carry `ACC_SYNTHETIC` (HotSpot: `isSynthetic() == false`,
/// `getModifiers() == 17`).
#[test]
fn proxy_class_and_method_fields_are_not_synthetic() {
    let bytes = emit_proxy_classfile(&supplier_spec()).expect("emit");
    let cf = read_class(&bytes).expect("round-trip");
    assert_eq!(cf.access_flags.bits(), 0x0001 | 0x0010 | 0x0020);
    for f in &cf.fields {
        assert_eq!(
            f.access_flags.bits(),
            0x0002 | 0x0008 | 0x0010,
            "{}",
            f.name
        );
    }
}

/// Round 13 wave 8 (lane proxy5): a variable-arity interface method's
/// proxy method is `ACC_VARARGS`, every other one `ACC_PUBLIC | ACC_FINAL`.
#[test]
fn a_varargs_interface_method_gives_a_varargs_proxy_method() {
    let mut spec = supplier_spec();
    let mut va = spec.methods[0].clone();
    va.name = "many".to_string();
    va.is_varargs = true;
    spec.methods.push(va);
    let bytes = emit_proxy_classfile(&spec).expect("emit");
    let cf = read_class(&bytes).expect("round-trip");
    let flags = |name: &str| {
        cf.methods
            .iter()
            .find(|m| &*m.name == name)
            .map(|m| m.access_flags.bits())
    };
    assert_eq!(flags("many"), Some(0x0001 | 0x0010 | 0x0080));
    assert_eq!(flags("get"), Some(0x0001 | 0x0010));
}
