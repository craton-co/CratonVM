use super::*;
use crate::classloading::resolution::{MethodHandle, MethodHandleKind};

fn mk_handle(kind: MethodHandleKind) -> MethodHandle {
    MethodHandle {
        kind,
        class_name: std::sync::Arc::from("com/example/Foo"),
        member_name: std::sync::Arc::from("bar"),
        descriptor: std::sync::Arc::from("()V"),
    }
}

#[test]
fn impl_kind_classification_covers_every_reference_kind() {
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeStatic)),
        ImplKind::Static
    );
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeVirtual)),
        ImplKind::Virtual
    );
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeInterface)),
        ImplKind::Virtual
    );
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeSpecial)),
        ImplKind::Special
    );
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::NewInvokeSpecial)),
        ImplKind::Constructor
    );
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::GetField)),
        ImplKind::Getter
    );
    assert_eq!(
        ImplKind::from_method_handle(&mk_handle(MethodHandleKind::PutStatic)),
        ImplKind::Setter
    );
}

/// A lookup that knows nothing but what a test says.
struct NoLookup(Option<u16>);

impl LambdaCodeLookup for NoLookup {
    fn method_flags(&self, _: &str, _: &str, _: &str) -> Option<u16> {
        self.0
    }

    fn is_interface(&self, _: &str) -> Option<bool> {
        Some(false)
    }
}

/// A call site of host `K54Cp` whose implementation is static
/// `K54Cp.<member><descriptor>`.
fn k54_site(
    interface: &str,
    sam: &str,
    sam_desc: &str,
    inst: &str,
    member: &str,
    descriptor: &str,
    captures: Vec<char>,
    serial: bool,
) -> LambdaCallSite {
    LambdaCallSite {
        functional_interface_id: None,
        functional_interface: Arc::from(interface),
        sam_method_name: Arc::from(sam),
        sam_descriptor: Arc::from(sam_desc),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("K54Cp"),
            member_name: Arc::from(member),
            descriptor: Arc::from(descriptor),
        },
        instantiated_descriptor: Arc::from(inst),
        capture_types: captures,
        proxy_class_id: ClassId::new(0x8000_0002),
        serializable_flag: serial,
    }
}

/// The spun class of `site` with `interfaces`, with no class store.
fn spun(site: &LambdaCallSite, serial: LambdaProxySerial, interfaces: &[&str]) -> SpunClass {
    lambda_proxy_spun_class(
        site,
        "K54Cp",
        &lambda_proxy_methods(site, &[], serial),
        interfaces,
        ClassId::new(0x8000_0002),
        &NoLookup(None),
    )
    .expect("modelled")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn utf8(s: &str) -> PoolEntry {
    PoolEntry::Utf8(s.to_string())
}

const HIDDEN: &str = "K54Cp$$Lambda+0x0000000080000002";

/// The i54-L1 retirement: the spun class's constant pool and code, entry
/// for entry and byte for byte as HotSpot 25.0.3 holds them (MEASURED
/// with `GetConstantPool` / `GetBytecodes`,
/// `tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.java`; the
/// hidden suffix is the VM's own id). Wave 57 measured the lengths and
/// first opcodes and left every pool operand 0.
#[test]
fn a_lambda_proxys_spun_class_is_the_metafactorys() {
    use PoolEntry::*;
    // `Runnable plain = K54Cp::hit;`
    let plain = k54_site(
        "java/lang/Runnable",
        "run",
        "()V",
        "()V",
        "hit",
        "()V",
        vec![],
        false,
    );
    let class = spun(&plain, LambdaProxySerial::None, &["java/lang/Runnable"]);
    assert_eq!(
        class.pool,
        vec![
            utf8("K54Cp$$Lambda"),
            Class(18),
            utf8("<init>"),
            utf8("()V"),
            utf8("java/lang/Object"),
            Class(5),
            NameAndType(3, 4),
            Methodref(6, 7),
            utf8("run"),
            utf8("K54Cp"),
            Class(10),
            utf8("hit"),
            NameAndType(12, 4),
            Methodref(11, 13),
            utf8("java/lang/Runnable"),
            Class(15),
            utf8("Code"),
            utf8(HIDDEN),
        ]
    );
    assert_eq!(
        class.codes.iter().map(|c| hex(c)).collect::<Vec<_>>(),
        vec!["2ab70008b1", "b8000eb1"]
    );
    assert_eq!(class.invoke_at, vec![None, Some(0)]);
    assert_eq!(class.constant_pool().0, 19);
    // `Supplier<String> capturing = () -> text + number;` (a `String`
    // and a `long`).
    let capturing = k54_site(
        "java/util/function/Supplier",
        "get",
        "()Ljava/lang/Object;",
        "()Ljava/lang/String;",
        "lambda$main$0",
        "(Ljava/lang/String;J)Ljava/lang/String;",
        vec!['L', 'J'],
        false,
    );
    let class = spun(
        &capturing,
        LambdaProxySerial::None,
        &["java/util/function/Supplier"],
    );
    assert_eq!(
        class.pool,
        vec![
            utf8("K54Cp$$Lambda"),
            Class(29),
            utf8("arg$1"),
            utf8("Ljava/lang/String;"),
            utf8("arg$2"),
            utf8("J"),
            utf8("<init>"),
            utf8("(Ljava/lang/String;J)V"),
            utf8("java/lang/Object"),
            Class(9),
            utf8("()V"),
            NameAndType(7, 11),
            Methodref(10, 12),
            NameAndType(3, 4),
            Fieldref(2, 14),
            NameAndType(5, 6),
            Fieldref(2, 16),
            utf8("get"),
            utf8("()Ljava/lang/Object;"),
            utf8("K54Cp"),
            Class(20),
            utf8("lambda$main$0"),
            utf8("(Ljava/lang/String;J)Ljava/lang/String;"),
            NameAndType(22, 23),
            Methodref(21, 24),
            utf8("java/util/function/Supplier"),
            Class(26),
            utf8("Code"),
            utf8(HIDDEN),
        ]
    );
    assert_eq!(
        class.codes.iter().map(|c| hex(c)).collect::<Vec<_>>(),
        vec!["2ab7000d2a2bb5000f2a20b50011b1", "2ab4000f2ab40011b80019b0"]
    );
    // `Function<Integer, String> unboxing = K54Cp::twice;` over
    // `twice(int)`: `checkcast Integer; invokevirtual Number.intValue`.
    let unboxing = k54_site(
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Integer;)Ljava/lang/String;",
        "twice",
        "(I)Ljava/lang/String;",
        vec![],
        false,
    );
    let class = spun(
        &unboxing,
        LambdaProxySerial::None,
        &["java/util/function/Function"],
    );
    assert_eq!(
        class.codes[1],
        vec![0x2b, 0xc0, 0, 12, 0xb6, 0, 18, 0xb8, 0, 24, 0xb0]
    );
    assert_eq!(class.pool[12], utf8("java/lang/Number"));
    assert_eq!(class.pool[14], utf8("intValue"));
    assert_eq!(class.pool.len(), 28);
    assert_eq!(class.invoke_at[1], Some(7));
    // `(Runnable & Serializable) K54Cp::hit`: `writeReplace`.
    let serializable = k54_site(
        "java/lang/Runnable",
        "run",
        "()V",
        "()V",
        "hit",
        "()V",
        vec![],
        true,
    );
    let class = spun(
        &serializable,
        LambdaProxySerial::Friendly,
        &["java/lang/Runnable", "java/io/Serializable"],
    );
    assert_eq!(
        hex(&class.codes[2]),
        "bb001259120b1214121512161217121812191216121603bd0006b7001cb0"
    );
    assert_eq!(
        &class.pool[19..24],
        &[String(19), String(9), String(4), Integer(6), String(10)]
    );
    assert_eq!(
        &class.pool[28..],
        &[
            Class(19),
            utf8("java/io/Serializable"),
            Class(30),
            utf8("Code"),
            utf8(HIDDEN)
        ]
    );
    assert_eq!(class.invoke_at, vec![None, Some(0), None]);
    // The hostile pair: the `Exceptions` attribute's class before the
    // code, its name after `Code` (by the generator's rules, not
    // measured: javac makes no such lambda).
    let class = spun(&plain, LambdaProxySerial::Hostile, &["java/lang/Runnable"]);
    let codes: Vec<std::string::String> = class.codes.iter().map(|c| hex(c)).collect();
    assert_eq!(codes[2], codes[3]);
    assert_eq!(codes[2].len(), 20);
    assert_eq!(class.pool[class.pool.len() - 2], utf8("Exceptions"));
}

/// Interpreter round i2 wave 56, lane L1: the methods of the class
/// `InnerClassLambdaMetafactory` spins, in its order and with the facts
/// HotSpot 25 answers for them (measured, `L1W56JvmtiLambdaMethods`).
#[test]
fn a_lambda_proxys_methods_are_the_spun_classs() {
    let site = |kind: MethodHandleKind, owner: &str, descriptor: &str, captures: Vec<char>| {
        LambdaCallSite {
            functional_interface_id: None,
            functional_interface: Arc::from("java/util/function/Function"),
            sam_method_name: Arc::from("apply"),
            sam_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
            impl_handle: MethodHandle {
                kind,
                class_name: Arc::from(owner),
                member_name: Arc::from("m"),
                descriptor: Arc::from(descriptor),
            },
            instantiated_descriptor: Arc::from("(Ljava/lang/Integer;)Ljava/lang/String;"),
            capture_types: captures,
            proxy_class_id: ClassId::new(0x8000_0001),
            serializable_flag: false,
        }
    };
    let shape = |methods: Vec<LambdaProxyMethod>| -> Vec<(String, u16, u16)> {
        methods
            .into_iter()
            .map(|m| {
                (
                    format!("{}{}", m.name, m.descriptor),
                    m.access_flags,
                    m.args_size,
                )
            })
            .collect()
    };
    // A static body capturing a String and a long (measured: `<init>`
    // 0x2 with 4 slots, `apply` 0x1 with 2).
    let capturing = site(
        MethodHandleKind::InvokeStatic,
        "p/Host",
        "(Ljava/lang/String;JLjava/lang/Integer;)Ljava/lang/String;",
        vec!['L', 'J'],
    );
    assert_eq!(
        shape(lambda_proxy_methods(
            &capturing,
            &[],
            LambdaProxySerial::None
        )),
        vec![
            ("<init>(Ljava/lang/String;J)V".to_string(), 0x2, 4),
            (
                "apply(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                0x1,
                2
            ),
        ]
    );
    // A bound receiver is typed by the method's class; a bridge and the
    // serialization methods follow the functional method.
    let bound = site(
        MethodHandleKind::InvokeVirtual,
        "p/Recv",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        vec!['L'],
    );
    let bridges: Vec<Arc<str>> = vec![Arc::from("(Ljava/lang/String;)Ljava/lang/Object;")];
    assert_eq!(
        shape(lambda_proxy_methods(
            &bound,
            &bridges,
            LambdaProxySerial::Friendly
        )),
        vec![
            ("<init>(Lp/Recv;)V".to_string(), 0x2, 2),
            (
                "apply(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                0x1,
                2
            ),
            (
                "apply(Ljava/lang/String;)Ljava/lang/Object;".to_string(),
                0x41,
                2
            ),
            ("writeReplace()Ljava/lang/Object;".to_string(), 0x12, 1),
        ]
    );
    // A capture whose type disagrees with the body's parameter is spelled
    // by its kind; the hostile pair for an unflagged serializable one.
    let odd = site(
        MethodHandleKind::InvokeStatic,
        "p/Host",
        "(I)Ljava/lang/Object;",
        vec!['L', 'D'],
    );
    assert_eq!(
        shape(lambda_proxy_methods(&odd, &[], LambdaProxySerial::Hostile)),
        vec![
            ("<init>(Ljava/lang/Object;D)V".to_string(), 0x2, 4),
            (
                "apply(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                0x1,
                2
            ),
            (
                "writeObject(Ljava/io/ObjectOutputStream;)V".to_string(),
                0x12,
                2
            ),
            (
                "readObject(Ljava/io/ObjectInputStream;)V".to_string(),
                0x12,
                2
            ),
        ]
    );
}

#[test]
fn is_instance_predicate_matches_spec() {
    assert!(ImplKind::Virtual.is_instance());
    assert!(ImplKind::Special.is_instance());
    assert!(ImplKind::Getter.is_instance());
    assert!(ImplKind::Setter.is_instance());
    assert!(!ImplKind::Static.is_instance());
    assert!(!ImplKind::Constructor.is_instance());
}

/// Guards the JDK's serializability rule for lambda proxies
/// (`SharedVm::lambda_proxy_serializability`).
///
/// Before 2026-08-01 every lambda proxy was treated as `Serializable`:
/// `getDeclaredMethods()` reported a synthetic `writeReplace()` on all of
/// them and the `instanceof` fast path answered `true` for
/// `java.io.Serializable` unconditionally, so a plain
/// `Supplier<String> s = () -> "x"` looked serializable where real HotSpot
/// throws ClassCastException on `(Serializable) s`.
///
/// This asserts the two arms a bare fixture can decide without a loaded
/// class hierarchy: the recorded `FLAG_SERIALIZABLE` alone must drive
/// `ByFlag` vs `NotSerializable`, and a `ClassId` that is not a registered
/// proxy at all must come back `NotSerializable` rather than defaulting to
/// serializable. (`ByInheritance` needs a real `Serializable`-extending
/// interface loaded, so the `LambdaSerProbe` differential probe against
/// jdk-25 covers that arm.)
///
/// NB: the sibling lambda-proxy tests in `vm/src/vm.rs` live behind
/// `#[cfg(all(test, feature = "synthetic-jdk"))]` and do NOT run by
/// default -- this one is deliberately here so it actually executes.
#[test]
fn lambda_proxy_serializability_follows_the_recorded_flag() {
    use crate::classloading::resolution::LambdaCallSite;
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_native_api::LambdaSerializability;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    let register = |flag: bool| {
        let id = shared.alloc_lambda_proxy_id();
        shared.classes.lambda_proxies.write().insert(
            id,
            std::sync::Arc::new(LambdaCallSite {
                functional_interface_id: None,
                // `Runnable` does not extend `Serializable`, so the
                // inheritance half of the rule is false either way and the
                // recorded flag is the only thing under test.
                functional_interface: std::sync::Arc::from("java/lang/Runnable"),
                sam_method_name: std::sync::Arc::from("run"),
                sam_descriptor: std::sync::Arc::from("()V"),
                impl_handle: mk_handle(MethodHandleKind::InvokeStatic),
                instantiated_descriptor: std::sync::Arc::from("()V"),
                capture_types: vec![],
                proxy_class_id: id,
                serializable_flag: flag,
            }),
        );
        id
    };

    let plain = register(false);
    let ser = register(true);

    assert_eq!(
        shared.lambda_proxy_serializability(plain),
        LambdaSerializability::NotSerializable,
        "a plain metafactory lambda over a non-Serializable interface must not \
         be serializable"
    );
    assert_eq!(
        shared.lambda_proxy_serializability(ser),
        LambdaSerializability::ByFlag,
        "FLAG_SERIALIZABLE must make the lambda serializable AND earn it the \
         added java.io.Serializable marker interface"
    );
    assert_eq!(
        shared.lambda_proxy_serializability(ClassId::new(1)),
        LambdaSerializability::NotSerializable,
        "a ClassId that is not a registered lambda proxy must not be reported \
         serializable"
    );
}
