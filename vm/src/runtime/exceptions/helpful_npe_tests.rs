use super::helpful_npe::{self, CpRef, CpResolver, Producer};
use super::{linkage_throwable, nsfe_message, nsme_message, LinkageError};
use std::collections::HashMap;

/// Map-backed resolver: cp-index -> CpRef, mirroring what the live
/// constant pool would hand back. No VM / rt.jar required.
///
/// `locals` mimics a resolved `LocalVariableTable`: slot -> source name.
/// When a slot is present the resolver returns that real name (as the
/// live `CpPoolResolver` does from the `LocalVariableTable` attribute);
/// when absent the analysis falls back to the synthetic `<localN>` form.
/// `is_static` mirrors the trapping method's access flag (slot-0 naming).
struct MockResolver {
    fields: HashMap<u16, CpRef>,
    methods: HashMap<u16, CpRef>,
    locals: HashMap<u16, String>,
    is_static: bool,
    descriptor: Option<&'static str>,
    /// `invokedynamic` call-site descriptors by CP index (wave 22).
    indy: HashMap<u16, String>,
    /// Exception-handler bcis of the method under analysis (wave 22).
    handlers: Vec<usize>,
}

impl MockResolver {
    fn new(fields: HashMap<u16, CpRef>, methods: HashMap<u16, CpRef>) -> Self {
        MockResolver {
            fields,
            methods,
            locals: HashMap::new(),
            is_static: false,
            descriptor: None,
            indy: HashMap::new(),
            handlers: Vec::new(),
        }
    }
    fn new_static(fields: HashMap<u16, CpRef>, methods: HashMap<u16, CpRef>) -> Self {
        let mut r = Self::new(fields, methods);
        r.is_static = true;
        r
    }
}

impl CpResolver for MockResolver {
    fn field_ref(&self, i: u16) -> Option<CpRef> {
        self.fields.get(&i).cloned()
    }
    fn method_ref(&self, i: u16) -> Option<CpRef> {
        self.methods.get(&i).cloned()
    }
    fn local_name(&self, slot: u16, _bci: usize) -> Option<String> {
        self.locals.get(&slot).cloned()
    }
    fn is_static_method(&self) -> bool {
        self.is_static
    }
    fn method_descriptor(&self) -> Option<&str> {
        self.descriptor
    }
    fn invokedynamic_descriptor(&self, i: u16) -> Option<String> {
        self.indy.get(&i).cloned()
    }
    fn handler_pcs(&self) -> Vec<usize> {
        self.handlers.clone()
    }
}

/// The inline `text` of a reconstructed producer (ignoring the
/// `is_invoke` flag), for terse `Some("…")` assertions.
fn text(p: &Option<Producer>) -> Option<&str> {
    p.as_ref().map(|x| x.text.as_str())
}

// Opcode bytes (JVMS §6).
const ALOAD_0: u8 = 0x2a;
const ALOAD_1: u8 = 0x2b;
const GETFIELD: u8 = 0xb4;
const GETSTATIC: u8 = 0xb2;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const CHECKCAST: u8 = 0xc0;
const AALOAD: u8 = 0x32;
const ICONST_2: u8 = 0x05;

fn u16_be(x: u16) -> [u8; 2] {
    x.to_be_bytes()
}

fn method(owner: &str, name: &str, desc: &str) -> CpRef {
    CpRef::Method {
        owner_internal: owner.to_string(),
        name: name.to_string(),
        descriptor: desc.to_string(),
    }
}
fn field(owner: &str, name: &str) -> CpRef {
    CpRef::Field {
        owner_internal: owner.to_string(),
        name: name.to_string(),
    }
}

/// External-name formatting (used for parameter rendering) matches the
/// JEP 358 dotted form, including array spelling.
#[test]
fn class_external_formats() {
    assert_eq!(
        helpful_npe::class_external("java/lang/String"),
        "java.lang.String"
    );
    assert_eq!(helpful_npe::class_external("[I"), "int[]");
    assert_eq!(
        helpful_npe::class_external("[Ljava/lang/Object;"),
        "java.lang.Object[]"
    );
}

/// `NoSuchMethodError`'s message, pinned to the JDK 25 output of
/// `apps/nsme_probe` (compile against a class, run against one with the
/// methods removed). Every character matters: the quotes are part of the
/// message, the return type leads, params are `, `-separated, and arrays
/// use source spelling rather than descriptor form.
#[test]
fn nsme_message_matches_hotspot() {
    // 'Lib Lib.widen(boolean)'
    assert_eq!(
        nsme_message("Lib", "widen", "(Z)LLib;"),
        "'Lib Lib.widen(boolean)'"
    );
    // 'long Lib.calc(int, java.lang.String[], double[][])'
    assert_eq!(
        nsme_message("Lib", "calc", "(I[Ljava/lang/String;[[D)J"),
        "'long Lib.calc(int, java.lang.String[], double[][])'"
    );
    // 'void Lib.plain()' — void return, and an empty parameter list must
    // not leave a stray separator.
    assert_eq!(nsme_message("Lib", "plain", "()V"), "'void Lib.plain()'");
}

/// `NoSuchFieldError`'s message, HotSpot's JDK 21+ sentence
/// (`LinkResolver::resolve_field`): dotted class, then the fieldref type in
/// source spelling and the name, quoted.
#[test]
fn nsfe_message_matches_hotspot() {
    assert_eq!(
        nsfe_message("p/C", "f", "I"),
        "Class p.C does not have member field 'int f'"
    );
    assert_eq!(
        nsfe_message("p/C", "names", "[Ljava/lang/String;"),
        "Class p.C does not have member field 'java.lang.String[] names'"
    );
    assert_eq!(
        nsfe_message("p/C$In", "o", "Ljava/lang/Object;"),
        "Class p.C$In does not have member field 'java.lang.Object o'"
    );
    assert_eq!(
        nsfe_message("C", "g", "[[J"),
        "Class C does not have member field 'long[][] g'"
    );
    // No descriptor in hand: no invented type.
    assert_eq!(
        nsfe_message("C", "g", ""),
        "Class C does not have member field 'g'"
    );
    // And `linkage_throwable` routes the variant through it.
    let (class, msg) = linkage_throwable(&LinkageError::NoSuchFieldError {
        class_name: "p/C".into(),
        field_name: "f".into(),
        field_descriptor: "Z".into(),
    });
    assert_eq!(class, "java/lang/NoSuchFieldError");
    assert_eq!(msg, "Class p.C does not have member field 'boolean f'");
}

/// The verifier's final-override refusal is HotSpot's
/// `IncompatibleClassChangeError`, its text verbatim (no prefix).
#[test]
fn final_override_refusal_is_an_unprefixed_icce() {
    let hs = "class p.C overrides final method java.lang.Object.getClass()Ljava/lang/Class;";
    let (class, msg) =
        linkage_throwable(&LinkageError::IncompatibleClassChangeError { message: hs.into() });
    assert_eq!(class, "java/lang/IncompatibleClassChangeError");
    assert_eq!(msg, hs);
}

/// The internal owner name is dotted, and the message still contains the
/// fully-qualified `Class.method(` substring Spring Boot's
/// `NoSuchMethodFailureAnalyzer` looks for — the property the previous
/// (raw-descriptor) spelling was written to satisfy, which the HotSpot
/// spelling satisfies too. This is what keeps
/// `NoSuchMethodFailureAnalyzerTests` green across the change.
#[test]
fn nsme_message_keeps_the_dotted_class_and_method_prefix() {
    let msg = nsme_message(
        "org/springframework/data/r2dbc/mapping/R2dbcMappingContext",
        "setForceQuote",
        "(Z)V",
    );
    assert!(
        msg.contains(
            "org.springframework.data.r2dbc.mapping.R2dbcMappingContext.setForceQuote("
        ),
        "analyzer-visible prefix missing from {msg}"
    );
}

/// The real Linux `spring-boot-data-redis` failure this spelling was fixed
/// for: a fixture compiled against a newer Jedis than the jar on its
/// classpath. HotSpot reports exactly this string.
#[test]
fn nsme_message_matches_hotspot_for_the_jedis_fixture_skew() {
    assert_eq!(
        nsme_message(
            "redis/clients/jedis/DefaultJedisClientConfig$Builder",
            "autoNegotiateProtocol",
            "(Z)Lredis/clients/jedis/DefaultJedisClientConfig$Builder;"
        ),
        "'redis.clients.jedis.DefaultJedisClientConfig$Builder \
         redis.clients.jedis.DefaultJedisClientConfig$Builder.autoNegotiateProtocol(boolean)'"
    );
}

/// The invoke **owner** rendering (verified against JDK 25): descriptor
/// form for arrays (`[I`, not `int[]`), dotted otherwise, and only
/// `String`/`Object` shortened.
#[test]
fn render_owner_matches_hotspot() {
    assert_eq!(helpful_npe::render_owner("java/lang/String"), "String");
    assert_eq!(helpful_npe::render_owner("java/lang/Object"), "Object");
    assert_eq!(
        helpful_npe::render_owner("java/lang/Integer"),
        "java.lang.Integer"
    );
    assert_eq!(
        helpful_npe::render_owner("java/util/List"),
        "java.util.List"
    );
    assert_eq!(helpful_npe::render_owner("NpeProbe$Node"), "NpeProbe$Node");
    // Array owners keep descriptor form (clone() on an array).
    assert_eq!(helpful_npe::render_owner("[I"), "[I");
    assert_eq!(
        helpful_npe::render_owner("[Ljava/lang/String;"),
        "[Ljava.lang.String;"
    );
}

/// The action half names owner + method + parameter types, with
/// `String`/`Object` shortened in *both* the owner and the params
/// (matches `java.lang.String.length` → `String.length`, and
/// `(Object)`/`(String)` params shortened too).
#[test]
fn action_invoke_shape() {
    assert_eq!(
        helpful_npe::action_invoke("java/lang/String", "length", "()I"),
        "Cannot invoke \"String.length()\""
    );
    assert_eq!(
        helpful_npe::action_invoke("java/util/List", "get", "(I)Ljava/lang/Object;"),
        "Cannot invoke \"java.util.List.get(int)\""
    );
    // String/Object params shortened; CharSequence stays qualified.
    assert_eq!(
        helpful_npe::action_invoke(
            "java/util/Map",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        "Cannot invoke \"java.util.Map.put(Object, Object)\""
    );
    assert_eq!(
        helpful_npe::action_invoke(
            "java/lang/String",
            "contains",
            "(Ljava/lang/CharSequence;)Z"
        ),
        "Cannot invoke \"String.contains(java.lang.CharSequence)\""
    );
    // Array param uses external form (Object[]), array owner stays [I.
    assert_eq!(
        helpful_npe::action_invoke("[I", "clone", "()Ljava/lang/Object;"),
        "Cannot invoke \"[I.clone()\""
    );
    assert_eq!(
        helpful_npe::action_invoke(
            "java/util/List",
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;"
        ),
        "Cannot invoke \"java.util.List.toArray(Object[])\""
    );
}

/// INVOKE case with a `this.next`-style getfield receiver:
/// bytecode `aload_0; getfield #1 (Node.next); invokevirtual #2
/// (Node.value())` with `next` null must yield the full HotSpot shape.
#[test]
fn invoke_receiver_is_field_of_this() {
    // aload_0; getfield #1; invokevirtual #2
    let mut code = vec![ALOAD_0, GETFIELD];
    code.extend_from_slice(&u16_be(1));
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut fields = HashMap::new();
    fields.insert(1u16, field("Node", "next"));
    let mut methods = HashMap::new();
    methods.insert(2u16, method("Node", "value", "()I"));
    let resolver = MockResolver::new(fields, methods);

    let action = helpful_npe::action_invoke("Node", "value", "()I");
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("this.next"));
    let msg = helpful_npe::combine(&action, expr.as_ref());
    assert_eq!(
        msg,
        "Cannot invoke \"Node.value()\" because \"this.next\" is null"
    );
}

/// GETFIELD-receiver chain where the *base* receiver is a local param
/// (`aload_1`): `aload_1; getfield #1 (Box.contents);
/// invokevirtual #2 (String.length())`.
#[test]
fn invoke_receiver_is_field_of_local() {
    let mut code = vec![ALOAD_1, GETFIELD];
    code.extend_from_slice(&u16_be(1));
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut fields = HashMap::new();
    fields.insert(1u16, field("Box", "contents"));
    let mut methods = HashMap::new();
    methods.insert(2u16, method("java/lang/String", "length", "()I"));
    let resolver = MockResolver::new(fields, methods);

    let action = helpful_npe::action_invoke("java/lang/String", "length", "()I");
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    let msg = helpful_npe::combine(&action, expr.as_ref());
    assert_eq!(
        msg,
        "Cannot invoke \"String.length()\" because \"<local1>.contents\" is null"
    );
}

/// Direct `aload_1` receiver (no field deref): the null expression is just
/// the synthetic local spelling.
#[test]
fn invoke_receiver_is_bare_local() {
    let code = vec![ALOAD_1, INVOKEVIRTUAL, 0x00, 0x02];
    let invoke_bci = 1;
    let mut methods = HashMap::new();
    methods.insert(
        2u16,
        method("java/lang/String", "trim", "()Ljava/lang/String;"),
    );
    let resolver = MockResolver::new(HashMap::new(), methods);
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
}

/// GETSTATIC receiver: `getstatic #1 (Sys.out); invokevirtual #2`.
#[test]
fn invoke_receiver_is_static_field() {
    let mut code = vec![GETSTATIC];
    code.extend_from_slice(&u16_be(1));
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut fields = HashMap::new();
    fields.insert(1u16, field("java/lang/System", "out"));
    let mut methods = HashMap::new();
    methods.insert(2u16, method("java/io/PrintStream", "println", "()V"));
    let resolver = MockResolver::new(fields, methods);
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("java.lang.System.out"));
}

/// In a **static** method, slot 0 is an ordinary local — HotSpot prints
/// `<local0>`, not `this`. `aload_0; arraylength`.
#[test]
fn static_method_slot0_is_local0() {
    let code = vec![ALOAD_0, ARRAYLENGTH];
    let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    let expr = helpful_npe::null_expr_at_depth(&code, 1, 0, &resolver);
    assert_eq!(text(&expr), Some("<local0>"));

    // The same bytecode in an instance method names slot 0 `this`.
    let instance = MockResolver::new(HashMap::new(), HashMap::new());
    let expr2 = helpful_npe::null_expr_at_depth(&code, 1, 0, &instance);
    assert_eq!(text(&expr2), Some("this"));
}

/// With the method descriptor known, an unwritten slot in the parameter
/// area is `<parameterN>` (HotSpot `print_local_var`): 1-based, a `long`
/// counting once, `this` not counted. A slot the method stores to anywhere
/// is `<localN>` again.
#[test]
fn unwritten_parameter_slots_are_named_parameter_n() {
    let code = vec![ALOAD_0, ARRAYLENGTH];
    let mut r = MockResolver::new_static(HashMap::new(), HashMap::new());
    r.descriptor = Some("([I)V");
    let expr = helpful_npe::null_expr_at_depth(&code, 1, 0, &r);
    assert_eq!(text(&expr), Some("<parameter1>"));

    // Instance method `(JLjava/lang/Object;)V`: slots 1-2 are the long,
    // slot 3 is the second parameter. `aload_3; arraylength`.
    let code3 = vec![0x2d, ARRAYLENGTH];
    let mut inst = MockResolver::new(HashMap::new(), HashMap::new());
    inst.descriptor = Some("(JLjava/lang/Object;)V");
    let expr3 = helpful_npe::null_expr_at_depth(&code3, 1, 0, &inst);
    assert_eq!(text(&expr3), Some("<parameter2>"));

    // Past the parameter area: still `<localN>`. `aload 4; arraylength`.
    let code4 = vec![0x19, 0x04, ARRAYLENGTH];
    let expr4 = helpful_npe::null_expr_at_depth(&code4, 2, 0, &inst);
    assert_eq!(text(&expr4), Some("<local4>"));

    // A parameter slot the method writes is no longer a parameter:
    // `aconst_null; astore_0; aload_0; arraylength` in the static method.
    let written = vec![0x01, 0x4b, ALOAD_0, ARRAYLENGTH];
    let expr5 = helpful_npe::null_expr_at_depth(&written, 3, 0, &r);
    assert_eq!(text(&expr5), Some("<local0>"));
}

/// A store between the producer and the trapping use must not defeat the
/// analysis — the store-free hand-written tests missed this, but every real
/// `Type x = null; … x.deref()` emits `astore` before the `aload`.
/// `aconst_null; astore_0; aload_0; getfield #1 (Node.x)`.
#[test]
fn store_then_load_reconstructs_local() {
    const ACONST_NULL: u8 = 0x01;
    const ASTORE_0: u8 = 0x4b;
    let mut code = vec![ACONST_NULL, ASTORE_0, ALOAD_0, GETFIELD];
    code.extend_from_slice(&u16_be(1));
    let trap_bci = 3;
    let mut fields = HashMap::new();
    fields.insert(1u16, field("Node", "x"));
    let resolver = MockResolver::new_static(fields, HashMap::new());
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local0>"));
}

/// A null **invoke result** used directly as a receiver renders as
/// `the return value of "Owner.m()"` (note: not quoted as a whole).
/// `invokestatic #1 (P.getNull()); invokevirtual #2`. (The parameter-list
/// rendering inside the "return value of" form is covered end-to-end by the
/// `Edge3` integration probe — `getNullArg(int, String)` — which needs the
/// real arg-pushing bytecode a hand-written synthetic can't easily supply.)
#[test]
fn return_value_producer() {
    let mut code = vec![INVOKESTATIC];
    code.extend_from_slice(&u16_be(1));
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut methods = HashMap::new();
    methods.insert(1u16, method("P", "getNull", "()Ljava/lang/String;"));
    methods.insert(2u16, method("java/lang/String", "length", "()I"));
    let resolver = MockResolver::new(HashMap::new(), methods);

    let action = helpful_npe::action_invoke("java/lang/String", "length", "()I");
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert!(expr.as_ref().unwrap().is_invoke);
    assert_eq!(text(&expr), Some("P.getNull()"));
    assert_eq!(
        helpful_npe::combine(&action, expr.as_ref()),
        "Cannot invoke \"String.length()\" because the return value of \"P.getNull()\" is null"
    );
}

/// A non-null invoke result whose **field** is null renders the invoke
/// inline (`Owner.m().field`), not as "the return value of".
/// `invokestatic #1 (P.getN()); getfield #4 (P.next); invokevirtual #2`.
#[test]
fn nested_return_value_subreceiver() {
    let mut code = vec![INVOKESTATIC];
    code.extend_from_slice(&u16_be(1));
    code.push(GETFIELD);
    code.extend_from_slice(&u16_be(4));
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut fields = HashMap::new();
    fields.insert(4u16, field("P", "next"));
    let mut methods = HashMap::new();
    methods.insert(1u16, method("P", "getN", "()LP;"));
    methods.insert(
        2u16,
        method("java/lang/Object", "toString", "()Ljava/lang/String;"),
    );
    let resolver = MockResolver::new(fields, methods);

    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert!(!expr.as_ref().unwrap().is_invoke);
    assert_eq!(text(&expr), Some("P.getN().next"));
}

/// A `checkcast` is transparent: `((String) o)` is named after `o`.
/// `aload_1; checkcast #3; invokevirtual #2`.
#[test]
fn checkcast_is_transparent() {
    let mut code = vec![ALOAD_1, CHECKCAST];
    code.extend_from_slice(&u16_be(3));
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut methods = HashMap::new();
    methods.insert(2u16, method("java/lang/String", "length", "()I"));
    let resolver = MockResolver::new(HashMap::new(), methods);
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
}

/// `aaload` reconstructs the index sub-expression: `getstatic #1 (P.sarr);
/// iconst_2; aaload; invokevirtual #2` → array element `P.sarr[2]`.
#[test]
fn aaload_index_reconstruction() {
    let mut code = vec![GETSTATIC];
    code.extend_from_slice(&u16_be(1));
    code.push(ICONST_2);
    code.push(AALOAD);
    let invoke_bci = code.len();
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));

    let mut fields = HashMap::new();
    fields.insert(1u16, field("P", "sarr"));
    let mut methods = HashMap::new();
    methods.insert(2u16, method("java/lang/String", "length", "()I"));
    let resolver = MockResolver::new(fields, methods);
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("P.sarr[2]"));
}

// -- Increment 2: action halves + new-opcode expression shapes ----------

// Additional opcode bytes (JVMS §6) for the increment-2 tests.
const ALOAD: u8 = 0x19; // aload <index> (wide-index local load)
const IASTORE: u8 = 0x4f;
const ARRAYLENGTH: u8 = 0xbe;
const MONITORENTER: u8 = 0xc2;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;

/// The increment-2 action halves match the exact HotSpot wording. Note
/// `baload`/`bastore` (byte *and* boolean arrays share the opcode) spell
/// the element type "byte/boolean".
#[test]
fn increment2_action_shapes() {
    use helpful_npe::ArrayElemKind;
    assert_eq!(
        helpful_npe::action_read_field("x"),
        "Cannot read field \"x\""
    );
    assert_eq!(
        helpful_npe::action_assign_field("count"),
        "Cannot assign field \"count\""
    );
    assert_eq!(
        helpful_npe::action_array_length(),
        "Cannot read the array length"
    );
    assert_eq!(
        helpful_npe::action_array_load(ArrayElemKind::Int),
        "Cannot load from int array"
    );
    assert_eq!(
        helpful_npe::action_array_store(ArrayElemKind::Object),
        "Cannot store to object array"
    );
    assert_eq!(
        helpful_npe::action_array_store(ArrayElemKind::Byte),
        "Cannot store to byte/boolean array"
    );
    assert_eq!(
        helpful_npe::action_array_load(ArrayElemKind::Boolean),
        "Cannot load from byte/boolean array"
    );
    assert_eq!(
        helpful_npe::action_monitor(),
        "Cannot enter synchronized block"
    );
    assert_eq!(helpful_npe::action_throw(), "Cannot throw exception");
}

/// `combine_opt` emits the action-only string (no fabricated `because`)
/// when the expression can't be classified, and the full clause otherwise.
#[test]
fn combine_opt_action_only_when_unknown() {
    let action = helpful_npe::action_array_length();
    assert_eq!(
        helpful_npe::combine_opt(&action, None),
        "Cannot read the array length"
    );
    // A real reconstructed expression yields the full clause.
    let code = vec![ALOAD_1, ARRAYLENGTH];
    let resolver = MockResolver::new(HashMap::new(), HashMap::new());
    let expr = helpful_npe::null_expr_at_depth(&code, 1, 0, &resolver);
    assert_eq!(
        helpful_npe::combine_opt(&action, expr.as_ref()),
        "Cannot read the array length because \"<local1>\" is null"
    );
}

/// getfield-read on a null `this.next`: `aload_0; getfield #1 (Node.next);
/// getfield #2 (Node.value)`. The *second* getfield (at `trap_bci`) reads
/// a field of the null `this.next`, so the null operand is the receiver at
/// depth 0.
#[test]
fn getfield_read_receiver_is_field_of_this() {
    // aload_0; getfield #1; getfield #2
    let mut code = vec![ALOAD_0, GETFIELD];
    code.extend_from_slice(&u16_be(1));
    let trap_bci = code.len();
    code.push(GETFIELD);
    code.extend_from_slice(&u16_be(2));

    let mut fields = HashMap::new();
    fields.insert(1u16, field("Node", "next"));
    fields.insert(2u16, field("Node", "value"));
    let resolver = MockResolver::new(fields, HashMap::new());

    // The trapping getfield reads field "value"; its receiver (depth 0)
    // is `this.next`.
    let action = helpful_npe::action_read_field("value");
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("this.next"));
    let msg = helpful_npe::combine_opt(&action, expr.as_ref());
    assert_eq!(
        msg,
        "Cannot read field \"value\" because \"this.next\" is null"
    );
}

/// arraylength of a null local array: `aload_1; arraylength`. The array is
/// the top-of-stack operand (depth 0).
#[test]
fn arraylength_of_local() {
    let code = vec![ALOAD_1, ARRAYLENGTH];
    let trap_bci = 1;
    let resolver = MockResolver::new(HashMap::new(), HashMap::new());
    let action = helpful_npe::action_array_length();
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
    assert_eq!(
        helpful_npe::combine_opt(&action, expr.as_ref()),
        "Cannot read the array length because \"<local1>\" is null"
    );
}

/// array-store into a null local int[]: `aload_1; iconst_0; iconst_1;
/// iastore`. Source stack at the iastore is `[arrayref, index, value]`, so
/// the null array is at depth 2.
#[test]
fn array_store_to_local() {
    let mut code = vec![ALOAD_1, ICONST_0, ICONST_1];
    let trap_bci = code.len();
    code.push(IASTORE);
    let resolver = MockResolver::new(HashMap::new(), HashMap::new());
    let action = helpful_npe::action_array_store(helpful_npe::ArrayElemKind::Int);
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 2, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
    assert_eq!(
        helpful_npe::combine_opt(&action, expr.as_ref()),
        "Cannot store to int array because \"<local1>\" is null"
    );
}

/// monitorenter on a null local: `aload_1; monitorenter`. The monitor
/// object is the top-of-stack operand (depth 0).
#[test]
fn monitorenter_of_local() {
    let code = vec![ALOAD_1, MONITORENTER];
    let trap_bci = 1;
    let resolver = MockResolver::new(HashMap::new(), HashMap::new());
    let action = helpful_npe::action_monitor();
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
    assert_eq!(
        helpful_npe::combine_opt(&action, expr.as_ref()),
        "Cannot enter synchronized block because \"<local1>\" is null"
    );
}

/// When a `LocalVariableTable` resolves slot 3 to its real source name
/// `items`, the analysis renders that name in place of `<local3>`.
/// `aload 3; arraylength`.
#[test]
fn lvt_name_resolves_when_present() {
    let code = vec![ALOAD, 3u8, ARRAYLENGTH];
    let trap_bci = 2;
    let mut resolver = MockResolver::new(HashMap::new(), HashMap::new());
    resolver.locals.insert(3u16, "items".to_string());
    let action = helpful_npe::action_array_length();
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("items"));
    assert_eq!(
        helpful_npe::combine_opt(&action, expr.as_ref()),
        "Cannot read the array length because \"items\" is null"
    );

    // Without the LVT entry the same bytecode falls back to <local3>.
    let bare = MockResolver::new(HashMap::new(), HashMap::new());
    let expr_bare = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &bare);
    assert_eq!(text(&expr_bare), Some("<local3>"));
}

/// Step 6 (partial): the JIT-NPE action codes map to the exact HotSpot
/// `BytecodeUtils` action-only strings, and an unrecognised / NONE code
/// yields no message (so the caller emits an unmessaged NPE).
#[test]
fn jit_action_messages_match_hotspot() {
    use helpful_npe::jit_action::*;
    assert_eq!(
        helpful_npe::jit_action_message(ARRAY_LENGTH).as_deref(),
        Some("Cannot read the array length")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_INT).as_deref(),
        Some("Cannot load from int array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_OBJECT).as_deref(),
        Some("Cannot load from object array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_BYTE).as_deref(),
        Some("Cannot load from byte/boolean array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_INT).as_deref(),
        Some("Cannot store to int array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_OBJECT).as_deref(),
        Some("Cannot store to object array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_BYTE).as_deref(),
        Some("Cannot store to byte/boolean array")
    );
    // Inline-codegen path extension (JEP-358 follow-up): the remaining
    // primitive element kinds the inline null-check stubs now name.
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_LONG).as_deref(),
        Some("Cannot load from long array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_FLOAT).as_deref(),
        Some("Cannot load from float array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_DOUBLE).as_deref(),
        Some("Cannot load from double array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_CHAR).as_deref(),
        Some("Cannot load from char array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ALOAD_SHORT).as_deref(),
        Some("Cannot load from short array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_LONG).as_deref(),
        Some("Cannot store to long array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_FLOAT).as_deref(),
        Some("Cannot store to float array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_DOUBLE).as_deref(),
        Some("Cannot store to double array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_CHAR).as_deref(),
        Some("Cannot store to char array")
    );
    assert_eq!(
        helpful_npe::jit_action_message(ASTORE_SHORT).as_deref(),
        Some("Cannot store to short array")
    );
    assert_eq!(helpful_npe::jit_action_message(NONE), None);
    assert_eq!(helpful_npe::jit_action_message(250), None);
}

// -- Merge-point blocks (DIV-001..003 divergence-log closure) ----------
//
// A basic block whose leader is a control-flow *join* starts with operand
// entries its predecessors pushed. `simulate_to` cannot see them, and used
// to bail on the resulting underflow — costing the `because "<expr>"` clause
// for every `T x = cond ? a : b; x.deref()`, the single most common shape in
// real code. It now treats an underflow as "this operand predates the
// block", which keeps top-relative indices exact. Expectations below are the
// verbatim JDK 25 `getExtendedNPEMessage` output for the equivalent source
// (the `DivNpe2` differential probe).

const ILOAD_0: u8 = 0x1a;
const IFEQ: u8 = 0x99;
const GOTO: u8 = 0xa7;
const ACONST_NULL: u8 = 0x01;
const ASTORE_1: u8 = 0x4c;
const ALOAD_2: u8 = 0x2c;
const IADD: u8 = 0x60;
const POP: u8 = 0x57;

/// `Node n = flag ? null : other; n.value` — the trapping `getfield` sits in
/// the ternary's merge block, whose leader (`astore_1`) pops a value pushed
/// by both predecessor blocks.
#[test]
fn ternary_merge_block_still_names_the_local() {
    // 0: iload_0
    // 1: ifeq   -> 8
    // 4: aconst_null
    // 5: goto   -> 9
    // 8: aload_2
    // 9: astore_1        <- merge-point block leader
    // 10: aload_1
    // 11: getfield #1    <- trap
    let mut code = vec![ILOAD_0, IFEQ];
    code.extend_from_slice(&u16_be(7)); // relative to bci 1 -> 8
    code.push(ACONST_NULL);
    code.push(GOTO);
    code.extend_from_slice(&u16_be(4)); // relative to bci 5 -> 9
    code.push(ALOAD_2);
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    let trap_bci = code.len();
    code.push(GETFIELD);
    code.extend_from_slice(&u16_be(1));

    let mut fields = HashMap::new();
    fields.insert(1u16, field("Node", "value"));
    let mut resolver = MockResolver::new_static(fields, HashMap::new());
    resolver.locals.insert(1, "n".to_string());

    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("n"));
    assert_eq!(
        helpful_npe::combine_opt(&helpful_npe::action_read_field("value"), expr.as_ref()),
        "Cannot read field \"value\" because \"n\" is null"
    );
}

/// `a[i + 1]` in a merge block: the index sub-expression puts a modelled
/// `iadd` in the prefix, and the array operand is still reachable at depth 1.
/// Before arithmetic was modelled the `iadd` alone abandoned the walk.
#[test]
fn arithmetic_prefix_does_not_abandon_the_walk() {
    // 0: astore_1   <- merge-point leader, pops a predecessor value
    // 1: aload_1
    // 2: iload_0
    // 3: iconst_1
    // 4: iadd
    // 5: iaload     <- trap; stack is [arr, idx]
    const IALOAD: u8 = 0x2e;
    let code = vec![ASTORE_1, ALOAD_1, ILOAD_0, ICONST_1, IADD, IALOAD];
    let trap_bci = 5;

    let mut resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    resolver.locals.insert(1, "a".to_string());

    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 1, &resolver);
    assert_eq!(text(&expr), Some("a"));
    assert_eq!(
        helpful_npe::combine_opt(
            &helpful_npe::action_array_load(helpful_npe::ArrayElemKind::Int),
            expr.as_ref()
        ),
        "Cannot load from int array because \"a\" is null"
    );
}

/// Tolerating the underflow must not *invent* an expression: an operand that
/// genuinely came from a predecessor block stays unknown, so the message
/// falls back to HotSpot's action-only shape rather than naming the wrong
/// value.
#[test]
fn operand_from_a_predecessor_block_stays_unnamed() {
    // 0: astore_1   (consumes the merge value)
    // 1: pop        (consumes a second predecessor value)
    // 2: arraylength  <- trap on a third, which predates the block entirely
    let code = vec![ASTORE_1, POP, ARRAYLENGTH];
    let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    let expr = helpful_npe::null_expr_at_depth(&code, 2, 0, &resolver);
    assert_eq!(text(&expr), None);
    assert_eq!(
        helpful_npe::combine_opt(&helpful_npe::action_array_length(), expr.as_ref()),
        "Cannot read the array length"
    );
}

// -- Whole-method analysis (interpreter round i1 wave 22, lane L7) ------
//
// Each shape below lost its `because` clause (or named the wrong local)
// under the old basic-block walk; the expectations are HotSpot 25's, from
// `L7W22HelpfulNpeFlow.java` and the same shapes
// compiled from source.

const INVOKEDYNAMIC: u8 = 0xba;
const ASTORE_0_OP: u8 = 0x4b;
const ASTORE_2: u8 = 0x4d;
const ILOAD_1: u8 = 0x1b;
const ALOAD_3: u8 = 0x2d;
const IALOAD_OP: u8 = 0x2e;
const NEWARRAY: u8 = 0xbc;
const LCONST_1: u8 = 0x0a;
const POP2: u8 = 0x58;
const DUP_X1: u8 = 0x5a;
const SWAP: u8 = 0x5f;
const ARETURN: u8 = 0xb0;
const RETURN: u8 = 0xb1;

/// An array store earlier in the block no longer abandons the walk:
/// `iconst_2; newarray int; astore_0; aload_0; iconst_0; iconst_1;
/// iastore; aconst_null; astore_1; aload_1; arraylength`.
#[test]
fn an_array_store_before_the_trap_keeps_the_clause() {
    let code = vec![
        ICONST_2,
        NEWARRAY,
        10,
        ASTORE_0_OP,
        ALOAD_0,
        ICONST_0,
        ICONST_1,
        IASTORE,
        ACONST_NULL,
        ASTORE_1,
        ALOAD_1,
        ARRAYLENGTH,
    ];
    let trap_bci = code.len() - 1;
    let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
}

/// A string concatenation (`invokedynamic`) sized by its call-site
/// descriptor: `aconst_null; astore_1; iload_0; invokedynamic #7
/// (I)String; astore_2; aload_1; arraylength`.
#[test]
fn an_invokedynamic_before_the_trap_is_sized_by_its_descriptor() {
    let mut code = vec![ACONST_NULL, ASTORE_1, ILOAD_0, INVOKEDYNAMIC];
    code.extend_from_slice(&u16_be(7));
    code.extend_from_slice(&[0, 0]);
    code.extend_from_slice(&[ASTORE_2, ALOAD_1]);
    let trap_bci = code.len();
    code.push(ARRAYLENGTH);
    let mut resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    resolver.indy.insert(7, "(I)Ljava/lang/String;".to_string());
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<local1>"));
    // Without the descriptor the path stops at the call site: no clause,
    // never a guessed one.
    let blind = MockResolver::new_static(HashMap::new(), HashMap::new());
    assert_eq!(
        text(&helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &blind)),
        None
    );
}

/// The shuffles move producers with their values, and `pop2` of a
/// `long` pops ONE entry.
#[test]
fn swap_dup_x1_and_pop2_are_modelled() {
    let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    // aload_1; aload_2; swap; pop; arraylength -> local 2 is on top.
    let code = vec![ALOAD_1, ALOAD_2, SWAP, POP, ARRAYLENGTH];
    assert_eq!(
        text(&helpful_npe::null_expr_at_depth(&code, 4, 0, &resolver)),
        Some("<local2>")
    );
    // aload_1; aload_2; dup_x1; pop; pop; arraylength -> the copy of
    // local 2 that dup_x1 put underneath.
    let code = vec![ALOAD_1, ALOAD_2, DUP_X1, POP, POP, ARRAYLENGTH];
    assert_eq!(
        text(&helpful_npe::null_expr_at_depth(&code, 5, 0, &resolver)),
        Some("<local2>")
    );
    // aload_1; lconst_1; pop2; arraylength
    let code = vec![ALOAD_1, LCONST_1, POP2, ARRAYLENGTH];
    assert_eq!(
        text(&helpful_npe::null_expr_at_depth(&code, 3, 0, &resolver)),
        Some("<local1>")
    );
}

/// `n.take(b ? 1 : 2)`: the receiver is pushed before the branch, and both
/// predecessors of the merge agree on it.
/// `0: aload_0; 1: iload_1; 2: ifeq +7; 5: iconst_1; 6: goto +4;
///  9: iconst_2; 10: invokevirtual #2`.
#[test]
fn a_receiver_pushed_before_a_conditional_argument_is_named() {
    let mut code = vec![ALOAD_0, ILOAD_1, IFEQ];
    code.extend_from_slice(&u16_be(7)); // bci 2 -> 9
    code.push(ICONST_1); // 5
    code.push(GOTO); // 6
    code.extend_from_slice(&u16_be(4)); // bci 6 -> 10
    code.push(ICONST_2); // 9
    let invoke_bci = code.len(); // 10
    code.push(INVOKEVIRTUAL);
    code.extend_from_slice(&u16_be(2));
    let mut methods = HashMap::new();
    methods.insert(2u16, method("Node", "take", "(I)I"));
    let mut resolver = MockResolver::new_static(HashMap::new(), methods);
    resolver.descriptor = Some("(LNode;Z)V");
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, 1, &resolver);
    assert_eq!(text(&expr), Some("<parameter1>"));
    // The argument itself came from two different constants: unnamed.
    assert_eq!(
        text(&helpful_npe::null_expr_at_depth(
            &code, invoke_bci, 0, &resolver
        )),
        None
    );
}

/// `s = s.trim()` on a null parameter: the store comes AFTER the trap, so
/// the slot is still a parameter there (HotSpot asks the paths reaching
/// the trap, not the whole method).
/// `aload_0; invokevirtual #2; astore_0; aload_0; areturn`.
#[test]
fn a_parameter_written_only_after_the_trap_is_still_a_parameter() {
    let mut code = vec![ALOAD_0, INVOKEVIRTUAL];
    code.extend_from_slice(&u16_be(2));
    code.extend_from_slice(&[ASTORE_0_OP, ALOAD_0, ARETURN]);
    let mut methods = HashMap::new();
    methods.insert(
        2u16,
        method("java/lang/String", "trim", "()Ljava/lang/String;"),
    );
    let mut resolver = MockResolver::new_static(HashMap::new(), methods);
    resolver.descriptor = Some("(Ljava/lang/String;)Ljava/lang/String;");
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, 1, 0, &resolver);
    assert_eq!(text(&expr), Some("<parameter1>"));
}

/// A handler starts with no written locals: a parameter the protected
/// range reassigned is still `<parameterN>` in the catch block.
/// `0: aload_0; 1: invokevirtual #2; 4: astore_0; 5: return;
///  6: astore_1 (handler); 7: aload_0; 8: arraylength`.
#[test]
fn a_handler_does_not_inherit_the_protected_ranges_writes() {
    let mut code = vec![ALOAD_0, INVOKEVIRTUAL];
    code.extend_from_slice(&u16_be(2));
    code.extend_from_slice(&[ASTORE_0_OP, RETURN, ASTORE_1, ALOAD_0, ARRAYLENGTH]);
    let mut methods = HashMap::new();
    methods.insert(
        2u16,
        method("java/lang/String", "trim", "()Ljava/lang/String;"),
    );
    let mut resolver = MockResolver::new_static(HashMap::new(), methods);
    resolver.descriptor = Some("([I)V");
    resolver.handlers = vec![6];
    let expr = helpful_npe::null_expr_at_depth(&code, 8, 0, &resolver);
    assert_eq!(text(&expr), Some("<parameter1>"));
}

/// `aaload` with an undescribable array prints `<array>`, and `iaload`
/// describes an index: `(c ? p : q)[idx[0]]` as
/// `0: iload_0; 1: ifeq +7; 4: aload_1; 5: goto +4; 8: aload_2;
///  9: aload_3; 10: iconst_0; 11: iaload; 12: aaload; 13: arraylength`.
#[test]
fn array_and_index_rendering_follow_hotspot() {
    let mut code = vec![ILOAD_0, IFEQ];
    code.extend_from_slice(&u16_be(7)); // bci 1 -> 8
    code.push(ALOAD_1); // 4
    code.push(GOTO); // 5
    code.extend_from_slice(&u16_be(4)); // bci 5 -> 9
    code.push(ALOAD_2); // 8
    code.extend_from_slice(&[ALOAD_3, ICONST_0, IALOAD_OP, AALOAD]);
    let trap_bci = code.len();
    code.push(ARRAYLENGTH);
    let resolver = MockResolver::new_static(HashMap::new(), HashMap::new());
    let expr = helpful_npe::null_expr_at_depth(&code, trap_bci, 0, &resolver);
    assert_eq!(text(&expr), Some("<array>[<local3>[0]]"));
}

/// The gated wrapper follows the VM's own
/// `-XX:±ShowCodeDetailsInExceptionMessages` (interpreter round i4 wave
/// 84, lane L2): no message when it is off, HotSpot's action text when it
/// is on. The process-wide override and `CRATONVM_HELPFUL_NPE_OPCODES`
/// outrank the configuration, so each arm is asserted only when the
/// effective answer is the configuration's (other tests set the override).
#[test]
fn jit_npe_message_gated_follows_the_vms_flag() {
    for on in [false, true] {
        let mut config = crate::config::VmConfig::default();
        config.show_code_details_in_exception_messages = on;
        let message =
            helpful_npe::jit_npe_message_gated(&config, helpful_npe::jit_action::ALOAD_INT);
        if crate::runtime::env_cache::show_code_details(&config) {
            assert_eq!(message.as_deref(), Some("Cannot load from int array"));
        } else {
            assert_eq!(message, None);
        }
    }
}
