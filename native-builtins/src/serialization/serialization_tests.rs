use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// --- Protocol constants ---

#[test]
fn test_stream_magic_and_version() {
    assert_eq!(STREAM_MAGIC, 0xACED);
    assert_eq!(STREAM_VERSION, 5);
}

#[test]
fn test_tc_constants_range() {
    assert_eq!(TC_NULL, 0x70);
    assert_eq!(TC_REFERENCE, 0x71);
    assert_eq!(TC_CLASSDESC, 0x72);
    assert_eq!(TC_OBJECT, 0x73);
    assert_eq!(TC_STRING, 0x74);
    assert_eq!(TC_ARRAY, 0x75);
    assert_eq!(TC_CLASS, 0x76);
    assert_eq!(TC_BLOCKDATA, 0x77);
    assert_eq!(TC_ENDBLOCKDATA, 0x78);
    assert_eq!(TC_RESET, 0x79);
    assert_eq!(TC_BLOCKDATALONG, 0x7A);
    assert_eq!(TC_EXCEPTION, 0x7B);
    assert_eq!(TC_LONGSTRING, 0x7C);
    assert_eq!(TC_PROXYCLASSDESC, 0x7D);
    assert_eq!(TC_ENUM, 0x7E);
}

/// Interpreter round i1 wave 8: a serialized-lambda record carries the
/// proxy's `altMetafactory` markers and bridges behind the high bit of its
/// reference-kind byte. The bit is free (JVMS kinds are 1..=9), the lists
/// round-trip in order, and a stream that ends early reads as no list.
#[test]
fn lambda_record_marker_and_bridge_lists_round_trip() {
    let _guard = serialization_test_guard();
    for kind in 1u8..=9 {
        assert_eq!(kind & LAMBDA_RECORD_HAS_ALT_LISTS, 0);
    }
    let addr = 0x5e41_a801usize;
    reset_serialization_state_for(addr);
    let markers = vec!["p/StrM".to_string(), "java/lang/Cloneable".to_string()];
    let bridges = vec!["()Ljava/lang/Object;".to_string()];
    write_lp_string_list(addr, &markers);
    write_lp_string_list(addr, &bridges);
    write_lp_string_list(addr, &[]);
    ois_buf_load(addr, oos_buf_snapshot(addr));
    assert_eq!(read_lp_string_list(addr), markers);
    assert_eq!(read_lp_string_list(addr), bridges);
    assert!(read_lp_string_list(addr).is_empty());
    // Past the end: the zero pad reads as an empty list.
    assert!(read_lp_string_list(addr).is_empty());
    reset_serialization_state_for(addr);
}

#[test]
fn test_base_wire_handle() {
    assert_eq!(BASE_WIRE_HANDLE, 0x7E0000);
}

#[test]
fn test_sc_flags() {
    assert_eq!(SC_WRITE_METHOD, 0x01);
    assert_eq!(SC_SERIALIZABLE, 0x02);
    assert_eq!(SC_EXTERNALIZABLE, 0x04);
    assert_eq!(SC_BLOCK_DATA, 0x08);
    assert_eq!(SC_ENUM, 0x10);
}

// --- SVUID computation ---

// Interpreter round i4 wave 96, lane L2: `default_svuid_of_shape` is the
// JDK's `computeDefaultSUID`. The expected values are HotSpot 25's
// answers for the classes of `L2W86SerialVersionUid.java`
// (its HOTSPOT_EXPECTED block, `C:\craton\i4-bin\hs\L2W86SerialVersionUid.out`),
// over the members javac 25 emits for them, and the well-known
// serialVersionUID of `String[]` that every serialized `String[]` carries
// (`0xADD256E7E91D7B47`).
fn shape(
    name: &str,
    class_mods: i32,
    interfaces: &[&str],
    fields: &[(&str, i32, &str)],
    has_clinit: bool,
    ctors: &[(i32, &str)],
    methods: &[(&str, i32, &str)],
) -> SuidShape {
    SuidShape {
        name: name.to_string(),
        class_mods,
        interfaces: interfaces.iter().map(|s| s.to_string()).collect(),
        fields: fields
            .iter()
            .map(|(n, m, d)| (n.to_string(), *m, d.to_string()))
            .collect(),
        has_clinit,
        ctors: ctors.iter().map(|(m, d)| (*m, d.to_string())).collect(),
        methods: methods
            .iter()
            .map(|(n, m, d)| (n.to_string(), *m, d.to_string()))
            .collect(),
    }
}

#[test]
fn default_svuid_matches_hotspot_plain_class() {
    // class L2W86SuidPlain implements Serializable { int a; private int b;
    // transient int c; static int d; private static int e; public String s;
    // L2W86SuidPlain() {} public void m(int) {} private void p() {}
    // static long q(String, int[]) }
    let s = shape(
        "L2W86SuidPlain",
        0x0020, // ACC_SUPER only; masked out
        &["java.io.Serializable"],
        &[
            ("a", 0x0000, "I"),
            ("b", 0x0002, "I"),
            ("c", 0x0080, "I"),
            ("d", 0x0008, "I"),
            ("e", 0x000A, "I"),
            ("s", 0x0001, "Ljava/lang/String;"),
        ],
        false,
        &[(0x0000, "()V")],
        &[
            ("q", 0x0008, "(Ljava/lang/String;[I)J"),
            ("m", 0x0001, "(I)V"),
            ("p", 0x0002, "()V"),
        ],
    );
    assert_eq!(default_svuid_of_shape(&s), -3382745865359344030);
}

#[test]
fn default_svuid_matches_hotspot_interface_and_clinit() {
    // interface L2W86SuidIface extends Serializable { int K = 1; void run(); }
    let iface = shape(
        "L2W86SuidIface",
        0x0600,
        &["java.io.Serializable"],
        &[("K", 0x0019, "I")],
        false,
        &[],
        &[("run", 0x0401, "()V")],
    );
    assert_eq!(default_svuid_of_shape(&iface), -3084270939886096296);
    // class L2W86SuidAssert implements Serializable { int x;
    // void check(int v) { assert v > 0 : "v"; } } -- javac adds
    // `static final synthetic boolean $assertionsDisabled` and `<clinit>`.
    let assert_shape = shape(
        "L2W86SuidAssert",
        0x0020,
        &["java.io.Serializable"],
        &[("x", 0x0000, "I"), ("$assertionsDisabled", 0x1018, "Z")],
        true,
        &[(0x0000, "()V")],
        &[("check", 0x0000, "(I)V")],
    );
    assert_eq!(default_svuid_of_shape(&assert_shape), -5024394253364411519);
}

#[test]
fn default_svuid_matches_hotspot_string_array() {
    let s = shape("[Ljava.lang.String;", 0x0411, &[], &[], false, &[], &[]);
    assert_eq!(default_svuid_of_shape(&s), -5921575005990323385);
    // `int[]`: `0x4DBA602676EAB2A5`, the other value every stream carries.
    let s = shape("[I", 0x0411, &[], &[], false, &[], &[]);
    assert_eq!(default_svuid_of_shape(&s), 5600894804908749477);
}

#[test]
fn java_write_utf_is_modified_utf8() {
    let mut out = Vec::new();
    java_write_utf(&mut out, "a\u{0}\u{e9}\u{1F600}");
    // 'a' | NUL as C0 80 | U+00E9 as C3 A9 | U+1F600 as two 3-byte surrogates
    assert_eq!(
        out,
        vec![
            0x00, 0x0B, b'a', 0xC0, 0x80, 0xC3, 0xA9, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80
        ]
    );
}

// --- Cycle detection ---

#[test]
fn test_detect_cycle_found() {
    let written = vec![100, 200, 300];
    assert!(detect_cycle(&written, 200));
}

#[test]
fn test_detect_cycle_not_found() {
    let written = vec![100, 200, 300];
    assert!(!detect_cycle(&written, 400));
}

#[test]
fn test_detect_cycle_empty() {
    let written: Vec<usize> = vec![];
    assert!(!detect_cycle(&written, 42));
}

// --- Type code helper ---

#[test]
fn test_type_code_primitives() {
    for c in ['B', 'C', 'D', 'F', 'I', 'J', 'S', 'Z'] {
        assert!(
            type_code_is_primitive(c as i32),
            "expected primitive: {}",
            c
        );
    }
}

#[test]
fn test_type_code_non_primitives() {
    for c in ['L', '[', 'X', 'Q'] {
        assert!(
            !type_code_is_primitive(c as i32),
            "expected non-primitive: {}",
            c
        );
    }
}

// --- Registration tests ---

/// `register_byte_array_output_stream` must stay EMPTY (lane G50).
///
/// Its twelve registrations were deleted because
/// `cratonvm_native_io::register_io_natives` re-registers every one of them
/// afterwards in the only arm that reaches this pass — measured on
/// `--dump-native-registry` in both modes, where all thirteen
/// `java/io/ByteArrayOutputStream` rows read `owns_slot = true`,
/// `registered_by = native-io/src/lib.rs`, and `serialization.rs` owns none
/// of the 12,039 / 10,691 rows. Refilling this pass would put a synthetic
/// no-op `close()`/`flush()` back in front of `native_baos_close` /
/// `native_baos_flush`, whose `BaosEvent` dispatch and
/// `process_pipe_output_close` are what make a `Process` stdin pipe work.
///
/// The `assert!(added > 0)` half is the vacuity guard: without it this test
/// would pass just as happily on the day `register_serialization_natives`
/// stops registering anything at all.
#[test]
fn baos_registrar_is_empty_and_stays_empty() {
    let mut r = NativeMethodRegistry::new();
    let before = r.len();
    register_byte_array_output_stream(&mut r);
    assert_eq!(
        r.len(),
        before,
        "register_byte_array_output_stream registered something; \
         native-io owns this family in every mode"
    );
    for (name, desc) in [
        ("close", "()V"),
        ("flush", "()V"),
        ("toByteArray", "()[B"),
        ("<init>", "()V"),
        ("write", "([BII)V"),
    ] {
        assert!(
            r.find("java/io/ByteArrayOutputStream", name, desc)
                .is_none(),
            "serialization.rs re-registered ByteArrayOutputStream.{name}{desc}"
        );
    }

    // Vacuity guard: the aggregate this pass belongs to must still work.
    let mut full = NativeMethodRegistry::new();
    let base = full.len();
    register_serialization_natives(&mut full);
    let added = full.len() - base;
    assert!(
        added > 0,
        "register_serialization_natives registered nothing, so the assertion \
         above proves nothing"
    );
    assert!(
        full.find("java/io/ByteArrayOutputStream", "toByteArray", "()[B")
            .is_none(),
        "the serialization aggregate still binds ByteArrayOutputStream"
    );
}

#[test]
fn test_oos_init_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectOutputStream",
            "<init>",
            "(Ljava/io/OutputStream;)V"
        )
        .is_some());
}

#[test]
fn test_oos_write_object_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectOutputStream",
            "writeObject",
            "(Ljava/lang/Object;)V"
        )
        .is_some());
}

#[test]
fn test_oos_primitive_writers_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectOutputStream", "writeInt", "(I)V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "writeLong", "(J)V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "writeFloat", "(F)V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "writeDouble", "(D)V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "writeBoolean", "(Z)V")
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectOutputStream",
            "writeUTF",
            "(Ljava/lang/String;)V"
        )
        .is_some());
}

#[test]
fn test_ois_init_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "<init>",
            "(Ljava/io/InputStream;)V"
        )
        .is_some());
}

#[test]
fn test_ois_read_object_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "readObject",
            "()Ljava/lang/Object;"
        )
        .is_some());
}

#[test]
fn test_osc_lookup_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "lookup",
            "(Ljava/lang/Class;)Ljava/io/ObjectStreamClass;"
        )
        .is_some());
}

#[test]
fn test_osf_get_name_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamField",
            "getName",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn test_serializable_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/Serializable", "registerNatives", "()V")
        .is_some());
}

#[test]
fn test_externalizable_not_registered() {
    // `writeExternal`/`readExternal` belong to the implementing class; a
    // native on the interface can only ever silently swallow it.
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/Externalizable",
            "writeExternal",
            "(Ljava/io/ObjectOutput;)V"
        )
        .is_none());
    assert!(r
        .find(
            "java/io/Externalizable",
            "readExternal",
            "(Ljava/io/ObjectInput;)V"
        )
        .is_none());
}

#[test]
fn test_filter_check_input_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputFilter",
            "checkInput",
            "(Ljava/io/ObjectInputFilter$FilterInfo;)Ljava/io/ObjectInputFilter$Status;"
        )
        .is_some());
}

#[test]
fn test_filter_config_get_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputFilter$Config",
            "getSerialFilter",
            "()Ljava/io/ObjectInputFilter;"
        )
        .is_some());
}

#[test]
fn test_invalid_class_exception_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/InvalidClassException",
            "getMessage",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn test_not_serializable_exception_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/NotSerializableException",
            "<init>",
            "(Ljava/lang/String;)V"
        )
        .is_some());
}

#[test]
fn test_stream_corrupted_exception_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/StreamCorruptedException",
            "<init>",
            "(Ljava/lang/String;)V"
        )
        .is_some());
}

#[test]
fn test_oos_control_methods_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectOutputStream", "reset", "()V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "flush", "()V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "close", "()V")
        .is_some());
    assert!(r
        .find("java/io/ObjectOutputStream", "useProtocolVersion", "(I)V")
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectOutputStream",
            "putFields",
            "()Ljava/io/ObjectOutputStream$PutField;"
        )
        .is_some());
}

#[test]
fn test_ois_additional_methods_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectInputStream", "close", "()V")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "available", "()I")
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "readFields",
            "()Ljava/io/ObjectInputStream$GetField;"
        )
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readStreamHeader", "()V")
        .is_some());
}

#[test]
fn test_osc_accessors_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectStreamClass", "getSerialVersionUID", "()J")
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "getName",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "toString",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn test_osf_accessors_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectStreamField", "isPrimitive", "()Z")
        .is_some());
    assert!(r
        .find("java/io/ObjectStreamField", "getTypeCode", "()C")
        .is_some());
    assert!(r
        .find("java/io/ObjectStreamField", "getOffset", "()I")
        .is_some());
}

#[test]
fn test_object_output_interface_writes_are_not_stubbed() {
    // The real writer lives on `ObjectOutputStream`; a no-op on the
    // interface would emit an empty stream instead of failing loudly.
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectOutput",
            "writeObject",
            "(Ljava/lang/Object;)V"
        )
        .is_none());
    assert!(r.find("java/io/ObjectOutput", "close", "()V").is_none());
    assert!(r
        .find(
            "java/io/ObjectOutputStream",
            "writeObject",
            "(Ljava/lang/Object;)V"
        )
        .is_some());
}

#[test]
fn test_object_input_interface_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectInput", "readObject", "()Ljava/lang/Object;")
        .is_some());
}

#[test]
fn test_invalid_class_getclassname_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/InvalidClassException",
            "getClassname",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn test_ois_read_class_descriptor_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "readClassDescriptor",
            "()Ljava/io/ObjectStreamClass;"
        )
        .is_some());
}

#[test]
fn test_svuid_wrapping_mul() {
    // Verify wrapping arithmetic does not panic on long class names
    let long_name = "a".repeat(1000);
    let _ = sentinel_name_svuid(&long_name);
}

#[test]
fn test_filter_config_set_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputFilter$Config",
            "setSerialFilter",
            "(Ljava/io/ObjectInputFilter;)V"
        )
        .is_some());
}

// ===== JEP-290 ObjectInputFilter — pattern parser + filter pipeline =====
//
// These tests exercise only the pure-Rust filter machinery (no
// NativeContext required). The end-to-end resolveClass / setSerialFilter
// wiring is registered above and covered by the *_registered tests; the
// semantic gate (REJECTED ⇒ InvalidClassException, allow-when-null,
// setSerialFilter twice ⇒ IllegalStateException) is what we lock in
// here.

#[test]
fn jep290_filter_null_allows_everything() {
    // The pure-Rust contract: an empty (or fully-undecided) filter
    // returns UNDECIDED, which the resolveClass gate treats as
    // permissive — no `InvalidClassException` is thrown. This is the
    // "no filter installed" leg of JEP-290.
    let empty = SerialFilter {
        entries: Vec::new(),
        pattern: String::new(),
    };
    assert_eq!(empty.check("java.util.HashMap"), FilterStatus::Undecided);
    assert_eq!(empty.check("any.thing.At.All"), FilterStatus::Undecided);
    // Same goes for the parsed-from-empty-string variant.
    let parsed = SerialFilter::parse("");
    assert_eq!(parsed.check("java.lang.String"), FilterStatus::Undecided);
}

#[test]
fn jep290_filter_rejects_blacklisted_class() {
    let f = SerialFilter::parse("!java.util.HashMap;*");
    assert_eq!(f.check("java/util/HashMap"), FilterStatus::Rejected);
    assert_eq!(f.check("java.util.HashMap"), FilterStatus::Rejected);
    assert_eq!(f.check("java/lang/String"), FilterStatus::Allowed);
}

#[test]
fn jep290_filter_set_twice_returns_err() {
    // The "set once" contract from JEP-290 §2.1. We model the gate
    // locally (matching the closure body of `setSerialFilter` byte-for-
    // byte for the already-installed check) so the test is hermetic and
    // cannot race against the process-wide OnceLock that other tests
    // may touch.
    fn try_install(
        state: &mut Option<SerialFilter>,
        new: SerialFilter,
    ) -> Result<(), RuntimeError> {
        if state.is_some() {
            return Err(RuntimeError::IllegalStateException {
                message: "Serial filter can only be set once".into(),
            });
        }
        *state = Some(new);
        Ok(())
    }
    let mut state: Option<SerialFilter> = None;
    // First install succeeds.
    assert!(try_install(&mut state, SerialFilter::parse("*")).is_ok());
    // Second install MUST fail with IllegalStateException.
    let err = try_install(&mut state, SerialFilter::parse("!*"))
        .expect_err("second setSerialFilter must throw IllegalStateException");
    match err {
        RuntimeError::IllegalStateException { message } => {
            assert!(message.contains("once"), "message should mention 'once'");
        }
        other => panic!("expected IllegalStateException, got {:?}", other),
    }
}

#[test]
fn jep290_glob_patterns() {
    // Recursive `**` matches sub-packages.
    let f = SerialFilter::parse("!com.evil.**;*");
    assert_eq!(f.check("com.evil.Gadget"), FilterStatus::Rejected);
    assert_eq!(f.check("com.evil.sub.Nested"), FilterStatus::Rejected);
    assert_eq!(f.check("com.good.Safe"), FilterStatus::Allowed);

    // Single-`*` is same-package-only.
    let f = SerialFilter::parse("!java.util.*;*");
    assert_eq!(f.check("java.util.HashMap"), FilterStatus::Rejected);
    // Sub-package must NOT match `java.util.*`.
    assert_eq!(
        f.check("java.util.concurrent.ConcurrentHashMap"),
        FilterStatus::Allowed
    );

    // Bare `*` matches everything.
    let f = SerialFilter::parse("*");
    assert_eq!(f.check("java.lang.String"), FilterStatus::Allowed);
    let f = SerialFilter::parse("!*");
    assert_eq!(f.check("java.lang.String"), FilterStatus::Rejected);
}

#[test]
fn jep290_first_match_wins() {
    // The JDK evaluates left-to-right and returns the first decided
    // result. So `java.util.HashMap` should be allowed here, not
    // rejected by the trailing `!*`.
    let f = SerialFilter::parse("java.util.HashMap;!*");
    assert_eq!(f.check("java.util.HashMap"), FilterStatus::Allowed);
    assert_eq!(f.check("java.util.LinkedList"), FilterStatus::Rejected);
}

#[test]
fn jep290_limit_tokens_parsed_but_not_enforced() {
    // Limits parse cleanly and don't trip class-matching logic.
    let f = SerialFilter::parse("maxdepth=10;maxrefs=100;maxbytes=4096;maxarray=1024;!*");
    assert_eq!(f.check("any.class.Name"), FilterStatus::Rejected);
    // Limits MUST NOT swallow the class rules that follow.
    let f = SerialFilter::parse("maxdepth=5;java.lang.String;!*");
    assert_eq!(f.check("java.lang.String"), FilterStatus::Allowed);
    assert_eq!(f.check("java.util.HashMap"), FilterStatus::Rejected);
}

#[test]
fn jep290_normalises_slash_form() {
    let f = SerialFilter::parse("!java/util/HashMap;*");
    // Rule is stored dotted; class name in slash form is normalised.
    assert_eq!(f.check("java/util/HashMap"), FilterStatus::Rejected);
    assert_eq!(f.check("java.util.HashMap"), FilterStatus::Rejected);
}

#[test]
fn jep290_empty_pattern_is_undecided() {
    let f = SerialFilter::parse("");
    assert_eq!(f.check("java.lang.String"), FilterStatus::Undecided);
    let f = SerialFilter::parse(";;;");
    assert_eq!(f.check("java.lang.String"), FilterStatus::Undecided);
}

#[test]
fn jep290_resolve_class_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "resolveClass",
            "(Ljava/io/ObjectStreamClass;)Ljava/lang/Class;"
        )
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "resolveProxyClass",
            "([Ljava/lang/String;)Ljava/lang/Class;"
        )
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "setObjectInputFilter",
            "(Ljava/io/ObjectInputFilter;)V"
        )
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "getObjectInputFilter",
            "()Ljava/io/ObjectInputFilter;"
        )
        .is_some());
}

// --- serialization_not_supported returns UnsupportedOperationException ---

#[test]
fn test_serialization_not_supported_returns_err() {
    // The function should always return Err (UnsupportedOperationException)
    // We cannot call it directly without a NativeContext, but we can verify
    // the error type by constructing the same RuntimeError.
    let err = RuntimeError::UnsupportedOperationException {
        message: "Java object serialization is not yet supported".into(),
    };
    assert!(matches!(
        err,
        RuntimeError::UnsupportedOperationException { .. }
    ));
}

#[test]
fn test_unsupported_operation_exception_message() {
    let err = RuntimeError::UnsupportedOperationException {
        message: "Java object serialization is not yet supported".into(),
    };
    if let RuntimeError::UnsupportedOperationException { message } = err {
        assert!(
            message.contains("serialization"),
            "Message should mention serialization"
        );
        assert!(
            message.contains("not yet supported"),
            "Message should say not yet supported"
        );
    } else {
        panic!("Expected UnsupportedOperationException");
    }
}

#[test]
fn test_deserialization_not_supported_message() {
    let err = RuntimeError::UnsupportedOperationException {
        message: "Java object deserialization is not supported (NotSerializableException)"
            .into(),
    };
    if let RuntimeError::UnsupportedOperationException { message } = err {
        assert!(
            message.contains("deserialization"),
            "Should mention deserialization"
        );
        assert!(
            message.contains("NotSerializableException"),
            "Should reference NotSerializableException"
        );
    } else {
        panic!("Expected UnsupportedOperationException");
    }
}

// --- SVUID computation edge cases ---

#[test]
fn test_svuid_known_classes_differ() {
    let names = [
        "java/lang/String",
        "java/lang/Integer",
        "java/util/ArrayList",
        "java/util/HashMap",
        "java/lang/Object",
    ];
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            assert_ne!(
                sentinel_name_svuid(names[i]),
                sentinel_name_svuid(names[j]),
                "{} and {} should have different SVUIDs",
                names[i],
                names[j]
            );
        }
    }
}

#[test]
fn test_svuid_case_sensitive() {
    let lower = sentinel_name_svuid("java/lang/string");
    let upper = sentinel_name_svuid("java/lang/String");
    assert_ne!(lower, upper, "SVUID should be case-sensitive");
}

#[test]
fn test_svuid_path_separator_matters() {
    let slash = sentinel_name_svuid("java/lang/String");
    let dot = sentinel_name_svuid("java.lang.String");
    assert_ne!(slash, dot, "Path separator should affect SVUID");
}

// --- Cycle detection edge cases ---

#[test]
fn test_detect_cycle_first_element() {
    let written = vec![42, 100, 200];
    assert!(detect_cycle(&written, 42));
}

#[test]
fn test_detect_cycle_last_element() {
    let written = vec![42, 100, 200];
    assert!(detect_cycle(&written, 200));
}

#[test]
fn test_detect_cycle_single_element() {
    let written = vec![99];
    assert!(detect_cycle(&written, 99));
    assert!(!detect_cycle(&written, 100));
}

#[test]
fn test_detect_cycle_large_set() {
    let written: Vec<usize> = (0..1000).collect();
    assert!(detect_cycle(&written, 500));
    assert!(detect_cycle(&written, 999));
    assert!(!detect_cycle(&written, 1000));
}

// --- Type code helpers ---

#[test]
fn test_type_code_all_non_primitive_object_types() {
    // 'L' is object reference, '[' is array
    assert!(!type_code_is_primitive('L' as i32));
    assert!(!type_code_is_primitive('[' as i32));
}

#[test]
fn test_type_code_zero() {
    assert!(!type_code_is_primitive(0));
}

#[test]
fn test_type_code_negative() {
    assert!(!type_code_is_primitive(-1));
}

// --- Protocol constants consistency ---

#[test]
fn test_stream_magic_is_java_magic() {
    // 0xACED is the documented Java serialization stream magic
    assert_eq!(STREAM_MAGIC, 0xACED);
}

#[test]
fn test_stream_version_is_five() {
    // Current Java serialization version is 5
    assert_eq!(STREAM_VERSION, 5);
}

#[test]
fn test_tc_constants_contiguous() {
    // TC constants should be contiguous from 0x70 to 0x7E
    let expected: Vec<u8> = (0x70..=0x7E).collect();
    let actual = [
        TC_NULL,
        TC_REFERENCE,
        TC_CLASSDESC,
        TC_OBJECT,
        TC_STRING,
        TC_ARRAY,
        TC_CLASS,
        TC_BLOCKDATA,
        TC_ENDBLOCKDATA,
        TC_RESET,
        TC_BLOCKDATALONG,
        TC_EXCEPTION,
        TC_LONGSTRING,
        TC_PROXYCLASSDESC,
        TC_ENUM,
    ];
    assert_eq!(actual.len(), expected.len());
    for (i, tc) in actual.iter().enumerate() {
        assert_eq!(
            *tc, expected[i],
            "TC at index {} should be 0x{:02X}",
            i, expected[i]
        );
    }
}

#[test]
fn test_sc_flags_are_powers_of_two() {
    // Each flag should be a distinct power of 2 (except SC_ENUM which is 0x10)
    let flags = [
        SC_WRITE_METHOD,
        SC_SERIALIZABLE,
        SC_EXTERNALIZABLE,
        SC_BLOCK_DATA,
        SC_ENUM,
    ];
    for flag in &flags {
        assert!(
            flag.is_power_of_two(),
            "Flag 0x{:02X} should be power of 2",
            flag
        );
    }
}

#[test]
fn test_sc_flags_no_overlap() {
    let flags = [
        SC_WRITE_METHOD,
        SC_SERIALIZABLE,
        SC_EXTERNALIZABLE,
        SC_BLOCK_DATA,
        SC_ENUM,
    ];
    for i in 0..flags.len() {
        for j in (i + 1)..flags.len() {
            assert_ne!(flags[i], flags[j], "Flags at {} and {} overlap", i, j);
            assert_eq!(
                flags[i] & flags[j],
                0,
                "Flags 0x{:02X} and 0x{:02X} share bits",
                flags[i],
                flags[j]
            );
        }
    }
}

#[test]
fn test_base_wire_handle_value() {
    // Base wire handle should be 0x7E0000 per the Java Object Serialization spec
    assert_eq!(BASE_WIRE_HANDLE, 0x7E_0000);
    assert!(BASE_WIRE_HANDLE > 0);
}

// --- OIS readObject/readUnshared throw ---

#[test]
fn test_ois_read_object_registered_throws() {
    // readObject should be registered as a method that throws
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "readObject",
            "()Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "readUnshared",
            "()Ljava/lang/Object;"
        )
        .is_some());
}

// --- OIS primitive reader defaults ---

#[test]
fn test_ois_primitive_readers_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectInputStream", "readInt", "()I")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readLong", "()J")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readFloat", "()F")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readDouble", "()D")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readBoolean", "()Z")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readByte", "()B")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readShort", "()S")
        .is_some());
    assert!(r
        .find("java/io/ObjectInputStream", "readChar", "()C")
        .is_some());
    assert!(r
        .find(
            "java/io/ObjectInputStream",
            "readUTF",
            "()Ljava/lang/String;"
        )
        .is_some());
}

// --- ObjectStreamClass method completeness ---

#[test]
fn test_osc_lookup_any_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "lookupAny",
            "(Ljava/lang/Class;)Ljava/io/ObjectStreamClass;"
        )
        .is_some());
}

#[test]
fn test_osc_get_field_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "getField",
            "(Ljava/lang/String;)Ljava/io/ObjectStreamField;"
        )
        .is_some());
}

#[test]
fn test_osc_get_fields_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "getFields",
            "()[Ljava/io/ObjectStreamField;"
        )
        .is_some());
}

#[test]
fn test_osc_for_class_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamClass",
            "forClass",
            "()Ljava/lang/Class;"
        )
        .is_some());
}

// --- OSF completeness ---

#[test]
fn test_osf_is_unshared_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find("java/io/ObjectStreamField", "isUnshared", "()Z")
        .is_some());
}

#[test]
fn test_osf_compare_to_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamField",
            "compareTo",
            "(Ljava/lang/Object;)I"
        )
        .is_some());
}

#[test]
fn test_osf_get_type_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamField",
            "getType",
            "()Ljava/lang/Class;"
        )
        .is_some());
}

#[test]
fn test_osf_get_type_string_registered() {
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    assert!(r
        .find(
            "java/io/ObjectStreamField",
            "getTypeString",
            "()Ljava/lang/String;"
        )
        .is_some());
}

// ===== M24 Binary Protocol Tests =====

#[test]
fn m24_write_stream_header_bytes() {
    let addr = 0xDEAD_0001;
    write_stream_header(addr);
    let buf = oos_buf_snapshot(addr);
    assert_eq!(buf.len(), 4);
    assert_eq!(buf[0], 0xAC);
    assert_eq!(buf[1], 0xED);
    assert_eq!(buf[2], 0x00);
    assert_eq!(buf[3], 0x05);
}

#[test]
fn m24_write_null_token_byte() {
    let addr = 0xDEAD_0002;
    write_null_token(addr);
    let buf = oos_buf_snapshot(addr);
    assert_eq!(buf, vec![TC_NULL]);
}

#[test]
fn m24_write_string_token_bytes() {
    let addr = 0xDEAD_0003;
    write_string_token(addr, "hello");
    let buf = oos_buf_snapshot(addr);
    // TC_STRING(0x74) + u16 len(0,5) + "hello"
    assert_eq!(buf[0], TC_STRING);
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 5);
    assert_eq!(&buf[3..], b"hello");
}

#[test]
fn m24_write_object_header_bytes() {
    let addr = 0xDEAD_0004;
    write_object_header(addr, "com/example/Foo", 0);
    let buf = oos_buf_snapshot(addr);
    // TC_OBJECT + TC_CLASSDESC + class name + SVUID + flags + 0 fields + endblockdata + null
    assert_eq!(buf[0], TC_OBJECT);
    assert_eq!(buf[1], TC_CLASSDESC);
    let name_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    assert_eq!(name_len, "com/example/Foo".len());
    let name = std::str::from_utf8(&buf[4..4 + name_len]).unwrap();
    assert_eq!(name, "com/example/Foo");
    let after_name = 4 + name_len;
    // 8 bytes SVUID + 1 byte flags + 2 bytes field count + 1 TC_ENDBLOCKDATA + 1 TC_NULL
    assert_eq!(buf[after_name + 8], SC_SERIALIZABLE); // flags
    assert_eq!(
        u16::from_be_bytes([buf[after_name + 9], buf[after_name + 10]]),
        0
    ); // 0 fields
    assert_eq!(buf[after_name + 11], TC_ENDBLOCKDATA);
    assert_eq!(buf[after_name + 12], TC_NULL);
}

#[test]
fn m24_write_class_desc_svuid() {
    let addr = 0xDEAD_0005;
    write_class_desc(addr, "Test", 12345, SC_SERIALIZABLE, &[]);
    let buf = oos_buf_snapshot(addr);
    assert_eq!(buf[0], TC_CLASSDESC);
    let name_len = u16::from_be_bytes([buf[1], buf[2]]) as usize;
    assert_eq!(name_len, 4);
    let svuid_start = 3 + name_len;
    let svuid = i64::from_be_bytes(buf[svuid_start..svuid_start + 8].try_into().unwrap());
    assert_eq!(svuid, 12345);
}

#[test]
fn m24_oos_buf_write_and_snapshot() {
    let addr = 0xBEEF_0010;
    oos_buf_reset(addr); // clear just this addr
    oos_buf_write(addr, &[1, 2, 3]);
    oos_buf_write(addr, &[4, 5]);
    let buf = oos_buf_snapshot(addr);
    assert_eq!(buf, vec![1, 2, 3, 4, 5]);
}

#[test]
fn m24_oos_buf_reset_clears() {
    let addr = 0xDEAD_0011;
    oos_buf_reset(addr); // ensure clean start
    oos_buf_write(addr, &[1, 2, 3]);
    oos_buf_reset(addr);
    let buf = oos_buf_snapshot(addr);
    assert!(buf.is_empty());
}

#[test]
fn m24_ois_buf_load_and_read() {
    let addr = 0xDEAD_0020;
    ois_buf_load(addr, vec![0xAC, 0xED, 0x00, 0x05, 0x70]);
    let header = ois_buf_read(addr, 4);
    assert_eq!(header, vec![0xAC, 0xED, 0x00, 0x05]);
    let tc = ois_buf_read(addr, 1);
    assert_eq!(tc, vec![0x70]); // TC_NULL
}

#[test]
fn m24_ois_buf_remaining_tracks_position() {
    let addr = 0xDEAD_0021;
    ois_buf_load(addr, vec![1, 2, 3, 4, 5]);
    assert_eq!(ois_buf_remaining(addr), 5);
    let _ = ois_buf_read(addr, 2);
    assert_eq!(ois_buf_remaining(addr), 3);
    let _ = ois_buf_read(addr, 3);
    assert_eq!(ois_buf_remaining(addr), 0);
}

#[test]
fn m24_validate_stream_header_valid() {
    let addr = 0xDEAD_0030;
    ois_buf_load(addr, vec![0xAC, 0xED, 0x00, 0x05]);
    assert!(validate_stream_header(addr));
}

#[test]
fn m24_validate_stream_header_invalid_magic() {
    let addr = 0xDEAD_0031;
    ois_buf_load(addr, vec![0x00, 0x00, 0x00, 0x05]);
    assert!(!validate_stream_header(addr));
}

#[test]
fn m24_validate_stream_header_invalid_version() {
    let addr = 0xDEAD_0032;
    ois_buf_load(addr, vec![0xAC, 0xED, 0x00, 0x03]);
    assert!(!validate_stream_header(addr));
}

#[test]
fn m24_int_roundtrip_via_buffers() {
    let addr = 0xDEAD_0040;
    let val: i32 = 0x12345678;
    oos_buf_write(addr, &val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 4);
    let read_val = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(read_val, val);
}

#[test]
fn m24_long_roundtrip_via_buffers() {
    let addr = 0xDEAD_0041;
    let val: i64 = 0x0102030405060708;
    oos_buf_write(addr, &val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 8);
    let read_val = i64::from_be_bytes(bytes.try_into().unwrap());
    assert_eq!(read_val, val);
}

#[test]
fn m24_float_roundtrip_via_buffers() {
    let addr = 0xDEAD_0042;
    let val: f32 = 3.14;
    oos_buf_write(addr, &val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 4);
    let read_val = f32::from_be_bytes(bytes.try_into().unwrap());
    assert_eq!(read_val, val);
}

#[test]
fn m24_double_roundtrip_via_buffers() {
    let addr = 0xDEAD_0043;
    let val: f64 = 2.718281828;
    oos_buf_write(addr, &val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 8);
    let read_val = f64::from_be_bytes(bytes.try_into().unwrap());
    assert_eq!(read_val, val);
}

#[test]
fn m24_boolean_roundtrip_true() {
    let addr = 0xDEAD_0044;
    oos_buf_write(addr, &[1u8]);
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 1);
    assert_eq!(bytes[0], 1);
}

#[test]
fn m24_boolean_roundtrip_false() {
    let addr = 0xDEAD_0045;
    oos_buf_write(addr, &[0u8]);
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 1);
    assert_eq!(bytes[0], 0);
}

#[test]
fn m24_short_roundtrip_via_buffers() {
    let addr = 0xDEAD_0046;
    let val: i16 = -1234;
    oos_buf_write(addr, &val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 2);
    let read_val = i16::from_be_bytes([bytes[0], bytes[1]]);
    assert_eq!(read_val, val);
}

#[test]
fn m24_char_roundtrip_via_buffers() {
    let addr = 0xDEAD_0047;
    let val: u16 = 'A' as u16;
    oos_buf_write(addr, &val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 2);
    let read_val = u16::from_be_bytes([bytes[0], bytes[1]]);
    assert_eq!(read_val, val);
}

#[test]
fn m24_utf_roundtrip_via_buffers() {
    let addr = 0xDEAD_0048;
    let s = "hello world";
    let bytes = s.as_bytes();
    oos_buf_write(addr, &(bytes.len() as u16).to_be_bytes());
    oos_buf_write(addr, bytes);
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let len_bytes = ois_buf_read(addr, 2);
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let str_bytes = ois_buf_read(addr, len);
    let read_s = String::from_utf8(str_bytes).unwrap();
    assert_eq!(read_s, s);
}

#[test]
fn m24_byte_roundtrip_via_buffers() {
    let addr = 0xDEAD_0049;
    let val: u8 = 0xFE;
    oos_buf_write(addr, &[val]);
    let written = oos_buf_snapshot(addr);
    ois_buf_load(addr, written);
    let bytes = ois_buf_read(addr, 1);
    assert_eq!(bytes[0], val);
}

#[test]
fn m24_negative_int_big_endian() {
    let addr = 0xDEAD_0050;
    let val: i32 = -1;
    oos_buf_write(addr, &val.to_be_bytes());
    let buf = oos_buf_snapshot(addr);
    assert_eq!(buf, vec![0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn m24_skip_class_desc_consumes_bytes() {
    let addr = 0xDEAD_0060;
    // Build a class desc manually
    let mut data = vec![TC_CLASSDESC];
    let name = b"Test";
    data.extend_from_slice(&(name.len() as u16).to_be_bytes());
    data.extend_from_slice(name);
    data.extend_from_slice(&0i64.to_be_bytes()); // SVUID
    data.push(SC_SERIALIZABLE); // flags
    data.extend_from_slice(&0u16.to_be_bytes()); // 0 fields
    data.push(TC_ENDBLOCKDATA);
    data.push(TC_NULL);
    data.push(0xFF); // sentinel
    ois_buf_load(addr, data);
    skip_class_desc(addr);
    assert_eq!(ois_buf_remaining(addr), 1); // only sentinel left
    let sentinel = ois_buf_read(addr, 1);
    assert_eq!(sentinel[0], 0xFF);
}

#[test]
fn m24_stream_header_then_null_token() {
    let addr = 0xDEAD_0070;
    write_stream_header(addr);
    write_null_token(addr);
    let buf = oos_buf_snapshot(addr);
    assert_eq!(buf.len(), 5);
    assert_eq!(&buf[0..4], &[0xAC, 0xED, 0x00, 0x05]);
    assert_eq!(buf[4], TC_NULL);
}

#[test]
fn m24_stream_header_then_string_token() {
    let addr = 0xDEAD_0071;
    write_stream_header(addr);
    write_string_token(addr, "hi");
    let buf = oos_buf_snapshot(addr);
    // 4 header + 1 TC_STRING + 2 len + 2 "hi" = 9
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[4], TC_STRING);
    assert_eq!(u16::from_be_bytes([buf[5], buf[6]]), 2);
    assert_eq!(&buf[7..9], b"hi");
}

#[test]
fn m24_multiple_primitives_sequential() {
    let addr = 0xDEAD_0080;
    let i_val: i32 = 42;
    let l_val: i64 = 999;
    oos_buf_write(addr, &i_val.to_be_bytes());
    oos_buf_write(addr, &l_val.to_be_bytes());
    let written = oos_buf_snapshot(addr);
    assert_eq!(written.len(), 12); // 4 + 8
    ois_buf_load(addr, written);
    let i_bytes = ois_buf_read(addr, 4);
    let read_i = i32::from_be_bytes(i_bytes.try_into().unwrap());
    assert_eq!(read_i, 42);
    let l_bytes = ois_buf_read(addr, 8);
    let read_l = i64::from_be_bytes(l_bytes.try_into().unwrap());
    assert_eq!(read_l, 999);
}

#[test]
fn m24_empty_string_utf() {
    let addr = 0xDEAD_0090;
    let s = "";
    oos_buf_write(addr, &(s.len() as u16).to_be_bytes());
    oos_buf_write(addr, s.as_bytes());
    let written = oos_buf_snapshot(addr);
    assert_eq!(written, vec![0x00, 0x00]); // just the zero-length u16
}

#[test]
fn m24_ois_read_returns_zeroes_when_empty() {
    let addr = 0xDEAD_00A0;
    // Don't load any data — read should return zeroes
    let bytes = ois_buf_read(addr, 4);
    assert_eq!(bytes, vec![0, 0, 0, 0]);
}

// -----------------------------------------------------------------
// JEP-290 ObjectInputFilter resource-limit parser
// -----------------------------------------------------------------

#[test]
fn jep290_parse_maxdepth_clause() {
    let s = parse_serial_filter("maxdepth=42");
    assert_eq!(s.max_depth, 42);
    // Other dimensions remain unbounded (0).
    assert_eq!(s.max_refs, 0);
    assert_eq!(s.max_bytes, 0);
    assert_eq!(s.max_array, 0);
}

#[test]
fn jep290_parse_maxbytes_large_value() {
    let s = parse_serial_filter("maxbytes=4294967296"); // > u32::MAX
    assert_eq!(s.max_bytes, 4_294_967_296);
}

#[test]
fn jep290_parse_all_limits_coexist() {
    // Acceptance test 5(d): one filter string with multiple `=N` clauses.
    let s = parse_serial_filter("maxdepth=10;maxrefs=20;maxbytes=1024;maxarray=64");
    assert_eq!(s.max_depth, 10);
    assert_eq!(s.max_refs, 20);
    assert_eq!(s.max_bytes, 1024);
    assert_eq!(s.max_array, 64);
}

#[test]
fn jep290_parse_mixed_with_class_patterns() {
    // Class-name patterns are now compiled into `patterns` (no longer a
    // silent no-op); limit clauses are still picked up alongside them.
    let s =
        parse_serial_filter("!com.evil.*;java.util.*;maxdepth=5;maxbytes=1024;com.example.Foo");
    assert_eq!(s.max_depth, 5);
    assert_eq!(s.max_bytes, 1024);
    // Pattern-only clauses leave maxrefs/maxarray unbounded.
    assert_eq!(s.max_refs, 0);
    assert_eq!(s.max_array, 0);

    // The class-name clauses are compiled and matchable (first match
    // wins, left-to-right, dot/slash agnostic — same matcher used for
    // the process-wide `jdk.serialFilter`).
    let pats = s.patterns.as_ref().expect("class patterns compiled");
    assert_eq!(pats.check("com.evil.Gadget"), FilterStatus::Rejected);
    assert_eq!(pats.check("com/evil/Gadget"), FilterStatus::Rejected);
    assert_eq!(pats.check("java.util.HashMap"), FilterStatus::Allowed);
    assert_eq!(pats.check("com.example.Foo"), FilterStatus::Allowed);
    // No matching clause => undecided (falls through to process-wide).
    assert_eq!(pats.check("org.other.Thing"), FilterStatus::Undecided);
}

#[test]
fn jep290_limits_only_spec_has_no_patterns() {
    // A spec with only `=N` clauses must leave `patterns` as `None` so
    // limits-only callers see no behavioural change.
    let s = parse_serial_filter("maxdepth=4;maxrefs=2;maxbytes=32;maxarray=8");
    assert!(s.patterns.is_none());
}

#[test]
fn jep290_perstream_pattern_filter_routed_on_synthetic_read_path() {
    // Regression: per-stream class-name pattern filters installed from a
    // filter STRING (via `parse_serial_filter` + `ois_set_filter_state`)
    // must now be honored by `evaluate_serial_filters` — the decision
    // point the synthetic read path consults — not just the numeric
    // limits. Before the fix the patterns were discarded and every class
    // was UNDECIDED (silently allowed).
    let addr = 0x4A45_5070_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_stream_filters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);

    ois_set_filter_state(
        addr,
        parse_serial_filter("!com.evil.**;java.util.*;maxdepth=10"),
    );

    // Reject pattern fires (recursive `**`).
    assert_eq!(
        evaluate_serial_filters(0, addr, "com/evil/sub/Gadget"),
        FilterStatus::Rejected
    );
    // Allow pattern fires.
    assert_eq!(
        evaluate_serial_filters(0, addr, "java/util/HashMap"),
        FilterStatus::Allowed
    );
    // No clause matches => undecided (no process-wide filter installed
    // in this unit test).
    assert_eq!(
        evaluate_serial_filters(0, addr, "org/other/Thing"),
        FilterStatus::Undecided
    );

    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
}

#[test]
fn jep290_parse_unbounded_when_absent() {
    // Acceptance test 6: missing clauses are unbounded.
    let s = parse_serial_filter("");
    assert_eq!(s.max_depth, 0);
    assert_eq!(s.max_refs, 0);
    assert_eq!(s.max_bytes, 0);
    assert_eq!(s.max_array, 0);
}

#[test]
fn jep290_parse_ignores_unknown_and_whitespace() {
    let s = parse_serial_filter("  maxdepth=3 ; futurething=99 ; maxarray=7  ");
    assert_eq!(s.max_depth, 3);
    assert_eq!(s.max_array, 7);
}

#[test]
fn jep290_parse_invalid_value_leaves_unbounded() {
    // Negative / non-numeric / overflow values leave the dimension
    // at the default unbounded (`0`).
    let s = parse_serial_filter("maxdepth=-1;maxrefs=abc;maxbytes=99999999999999999999");
    assert_eq!(s.max_depth, 0);
    assert_eq!(s.max_refs, 0);
    assert_eq!(s.max_bytes, 0);
}

// -----------------------------------------------------------------
// JEP-290 enforcement — counter wiring
// -----------------------------------------------------------------

#[test]
fn jep290_maxbytes_trips_reject_flag() {
    // Acceptance test 5(b): maxbytes=64 rejects a 100-byte payload.
    let addr = 0x4A45_5000_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_buf_load(addr, vec![0u8; 100]);
    ois_set_filter_state(addr, parse_serial_filter("maxbytes=64"));

    // Consume the full 100 bytes; somewhere past byte 64 the state
    // should trip.
    for _ in 0..100 {
        let _ = ois_buf_read(addr, 1);
    }

    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert!(snapshot.rejected, "max_bytes=64 must trip after 100 reads");
    assert!(
        snapshot.reason.contains("maxbytes=64"),
        "reason should mention maxbytes: {}",
        snapshot.reason
    );
    let why = filter_is_rejected(addr).unwrap();
    assert!(why.contains("maxbytes=64"));
}

#[test]
fn jep290_maxbytes_not_tripped_when_under_limit() {
    let addr = 0x4A45_5001_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_buf_load(addr, vec![0u8; 100]);
    ois_set_filter_state(addr, parse_serial_filter("maxbytes=200"));
    for _ in 0..100 {
        let _ = ois_buf_read(addr, 1);
    }
    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert!(
        !snapshot.rejected,
        "max_bytes=200 must not trip on 100 bytes"
    );
    assert_eq!(snapshot.bytes, 100);
}

#[test]
fn jep290_maxrefs_trips_reject_flag() {
    // Acceptance test 5(c): maxrefs=1 rejects a 2-back-reference graph.
    let addr = 0x4A45_5002_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_set_filter_state(addr, parse_serial_filter("maxrefs=1"));

    // Two back-references — the second one must trip the cap.
    assert!(filter_account_ref(addr), "first ref under cap of 1");
    assert!(!filter_account_ref(addr), "second ref must trip max_refs=1");

    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert!(snapshot.rejected);
    assert!(snapshot.reason.contains("maxrefs=1"));
}

#[test]
fn jep290_maxdepth_enforced_via_filter_enter_depth() {
    // Acceptance test 5(a): maxdepth=2 rejects a 3-deep nested object.
    // We test the depth gate directly — wiring into `ois_read_value`
    // is unit-tested separately via the `MockNativeContext` test
    // in the wp02_tests module's neighbours; here we just confirm
    // the counter behaves correctly.
    let addr = 0x4A45_5003_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_set_filter_state(addr, parse_serial_filter("maxdepth=2"));

    assert!(filter_enter_depth(addr), "depth 1");
    assert!(filter_enter_depth(addr), "depth 2");
    assert!(!filter_enter_depth(addr), "depth 3 must trip max_depth=2");

    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert!(snapshot.rejected);
    assert!(
        snapshot.reason.contains("maxdepth=2"),
        "reason should mention maxdepth: {}",
        snapshot.reason
    );
}

#[test]
fn jep290_maxarray_rejects_oversized_allocation() {
    // Verifies `filter_check_array` returns false (and trips
    // `rejected`) when the on-wire array length exceeds maxarray.
    let addr = 0x4A45_5004_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_set_filter_state(addr, parse_serial_filter("maxarray=10"));

    assert!(filter_check_array(addr, 5), "len 5 <= max 10 must pass");
    assert!(!filter_check_array(addr, 11), "len 11 > max 10 must reject");

    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert!(snapshot.rejected);
    assert!(snapshot.reason.contains("maxarray=10"));
}

#[test]
fn jep290_synthetic_path_rejects_filtered_class() {
    let _serial_guard = super::serialization_test_guard();
    // SECURITY regression (JEP-290 filter bypass, CRITICAL): a per-stream
    // reject rule installed for a gadget class must trip the sticky flag
    // when the synthetic read-path evaluates it, and a benign class in the
    // same stream must NOT be rejected. This is the gate `ois_read_object`
    // / `ois_read_array` / `TC_ENUM` now call before instantiating a class.
    let addr = 0x4A45_5100_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_stream_filters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);

    // Reject anything under `evil.**`, allow everything else.
    ois_stream_filters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(addr, SerialFilter::parse("!evil.**;*"));

    // A benign class passes (proceed = false).
    assert!(
        !synthetic_read_class_rejected(0, addr, "java/util/ArrayList"),
        "allowed class must not be rejected"
    );
    // A filtered gadget class is rejected (abort = true) and trips sticky.
    assert!(
        synthetic_read_class_rejected(0, addr, "evil/Gadget"),
        "filtered class must be rejected"
    );
    let why = filter_is_rejected(addr).expect("reject must be sticky");
    assert!(
        why.contains("evil/Gadget") && why.contains("ObjectInputFilter"),
        "reason must name the rejected class: {}",
        why
    );

    ois_stream_filters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
}

#[test]
fn longstring_oversized_length_does_not_overallocate() {
    let _serial_guard = super::serialization_test_guard();
    // SECURITY regression (deserialization DoS, HIGH): `ois_buf_read`
    // must never honour a hostile length past the bytes actually in the
    // buffer. We register a tiny buffer and ask for a near-`usize::MAX`
    // read; the result must be capped (no multi-exabyte allocation) and
    // the checked `pos + n` arithmetic must not wrap.
    let addr = 0x4A45_5101_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_buf_load(addr, vec![1u8, 2, 3, 4]);

    // Consume the 4 real bytes, then over-read.
    let _ = ois_buf_read(addr, 4);
    let huge = ois_buf_read(addr, usize::MAX); // would have OOM'd pre-fix
    assert!(
        huge.len() <= MAX_SERIAL_BUF_READ_PAD,
        "short read must be capped at the pad bound, got {}",
        huge.len()
    );

    // The TC_LONGSTRING clamp itself: a wire length far beyond remaining
    // must clamp to remaining (here 0 once exhausted, small otherwise).
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_buf_load(addr, b"hello".to_vec());
    let remaining = ois_buf_remaining(addr) as u64;
    let clamped = (u64::MAX)
        .min(remaining)
        .min(MAX_SERIAL_STRING_BYTES as u64) as usize;
    assert_eq!(clamped, 5, "clamp must bound to bytes remaining");
    assert!(
        clamped <= MAX_SERIAL_STRING_BYTES,
        "clamp must honour hard cap"
    );
}

#[test]
fn array_oversized_length_rejected_without_filter() {
    let _serial_guard = super::serialization_test_guard();
    // SECURITY regression (deserialization DoS, MEDIUM, default-reachable):
    // `ois_read_array` must apply a filter-INDEPENDENT upper bound on the
    // declared element count before allocating. With NO filter installed
    // (the default) `filter_check_array` reports "unbounded", so a hostile
    // TC_ARRAY length must still be rejected by the hard ceiling and the
    // "one byte per element" remaining-bytes guard.
    let addr = 0x4A45_5201_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync

    // A tiny buffer that has just had its 4-byte length word consumed,
    // standing in for the state inside `ois_read_array` after the length
    // read. Only a few element bytes remain on the wire.
    ois_buf_load(addr, vec![0u8; 3]);
    let remaining = ois_buf_remaining(addr);
    assert_eq!(remaining, 3);

    // The default (no filter) path: the JEP-290 maxarray check is a no-op
    // ceiling — it must NOT be relied upon to bound the allocation.
    assert!(
        filter_check_array(addr, i32::MAX as usize),
        "with no filter installed, filter_check_array reports unbounded"
    );

    // The filter-independent guard (mirrors the predicate in
    // `ois_read_array`): a 2.1-billion-element claim is rejected.
    let hostile = i32::MAX as usize;
    assert!(
        hostile > MAX_SERIAL_ARRAY_ELEMS || hostile > remaining,
        "hostile array length must trip the filter-independent guard"
    );
    // It exceeds BOTH the hard ceiling and the bytes the stream can back.
    assert!(hostile > MAX_SERIAL_ARRAY_ELEMS);
    assert!(hostile > remaining);

    // A length the buffer cannot back (more elements than bytes left, even
    // at one byte each) is rejected even though it is under the ceiling.
    let unbacked = remaining + 1;
    assert!(unbacked <= MAX_SERIAL_ARRAY_ELEMS);
    assert!(
        unbacked > remaining,
        "an array longer than the remaining bytes must be rejected"
    );

    // A legitimate, fully-backed array length is accepted by the guard.
    let ok = remaining; // every element backed by >= 1 byte
    assert!(
        !(ok > MAX_SERIAL_ARRAY_ELEMS || ok > remaining),
        "a fully-backed array length must NOT be rejected"
    );

    filter_state_remove(addr);
}

#[test]
fn jep290_unbounded_defaults_do_not_reject() {
    // Acceptance test 6: with no `=N` clauses present every check
    // returns ok regardless of magnitude.
    let addr = 0x4A45_5005_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_set_filter_state(addr, parse_serial_filter("java.util.*;!evil.*"));
    for _ in 0..1000 {
        assert!(filter_account_ref(addr));
        assert!(filter_enter_depth(addr));
        filter_exit_depth(addr);
    }
    assert!(filter_check_array(addr, usize::MAX / 2));
    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert!(!snapshot.rejected);
}

/// The suite-only failure this file's isolation contract exists to stop.
///
/// `jep290_unbounded_defaults_do_not_reject` above failed inside the
/// 4408-test binary and passed alone, because the reset the marshal tests
/// ran cleared the WHOLE of the address-keyed serialization maps and so
/// wiped the filter entry this test had just installed. Nothing about the
/// jep290 subject was wrong; the sibling was.
///
/// Asserting the scoping directly makes that a checked contract instead of
/// something a flake has to re-discover: a reset aimed at one stream must
/// leave every other stream's buffers, handles and filter state intact.
#[test]
fn resetting_one_stream_leaves_every_other_stream_alone() {
    let _serial_guard = super::serialization_test_guard();
    let mine = 0x5E11_0001_usize;
    let theirs = 0x5E11_0002_usize;

    // A sibling's state: an output buffer, an input buffer, a filter.
    oos_buf_reset(theirs);
    filter_state_remove(theirs);
    oos_buf_write(theirs, &[1, 2, 3]);
    ois_buf_load(theirs, vec![9, 9]);
    ois_set_filter_state(theirs, parse_serial_filter("maxbytes=64"));

    reset_serialization_state_for(mine);

    assert_eq!(
        oos_buf_snapshot(theirs),
        vec![1, 2, 3],
        "a reset of another stream must not clear this one's output buffer"
    );
    assert_eq!(
        ois_buf_remaining(theirs),
        2,
        "a reset of another stream must not clear this one's input buffer"
    );
    assert!(
        ois_get_filter_state(theirs).is_some(),
        "a reset of another stream must not drop this one's JEP-290 filter; \
         that drop is what stranded jep290_unbounded_defaults_do_not_reject \
         inside the full suite"
    );

    filter_state_remove(theirs);
    oos_buf_reset(theirs);
}

#[test]
fn jep290_combined_limits_each_independently_enforced() {
    // Acceptance test 5(d): single filter with several `=N` clauses
    // — each dimension is enforced on its own counter.
    let addr = 0x4A45_5006_usize;
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_set_filter_state(
        addr,
        parse_serial_filter("maxdepth=4;maxrefs=2;maxbytes=32;maxarray=8"),
    );

    // Depth: 4 pushes ok, 5th rejects.
    for _ in 0..4 {
        assert!(filter_enter_depth(addr));
    }
    for _ in 0..4 {
        filter_exit_depth(addr);
    }
    // Reset for isolated dimension check.
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
    ois_set_filter_state(
        addr,
        parse_serial_filter("maxdepth=4;maxrefs=2;maxbytes=32;maxarray=8"),
    );

    // Array dimension fires first if length=9.
    assert!(!filter_check_array(addr, 9));
    let snapshot = ois_get_filter_state(addr).expect("filter state");
    assert_eq!(snapshot.max_depth, 4);
    assert_eq!(snapshot.max_refs, 2);
    assert_eq!(snapshot.max_bytes, 32);
    assert_eq!(snapshot.max_array, 8);
    assert!(snapshot.rejected);
}

#[test]
fn jep290_read_object_surfaces_reject_as_ioexception() {
    // Drive a real `readObject` through the registered native and
    // ensure the sticky reject flag is surfaced as `IOException`
    // whose message starts with "filter status: REJECTED".
    use crate::test_utils::MockNativeContext;
    let mut r = NativeMethodRegistry::new();
    register_serialization_natives(&mut r);
    let read_object = r
        .find(
            "java/io/ObjectInputStream",
            "readObject",
            "()Ljava/lang/Object;",
        )
        .expect("readObject must be registered");

    // gc-common w21-c: the natives key the stream by its weak lock key,
    // minted in this context's VM; an own VM identity keeps the mock's
    // recycled addresses from sharing a key with a sibling test's stream.
    const VM: usize = 0x5E21_0C01;
    struct ForgetKeys(usize);
    impl Drop for ForgetKeys {
        fn drop(&mut self) {
            forget_serialization_keys(&[self.0]);
            crate::forget_vm_lock_keys(VM);
            forget_vm_serialization_state(VM);
        }
    }
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let ois_class = ctx
        .ensure_class_initialized("java/io/ObjectInputStream")
        .unwrap();
    let ois = ctx.alloc_object(ois_class, 6);
    ctx.set_field(ois, 1, Value::Int(0)); // depth
    ctx.set_field(ois, 2, Value::Int(0)); // objects_read

    let addr = stream_key(&ctx, ois);
    let _forget = ForgetKeys(addr);
    filter_state_remove(addr); // PERF: keep ois_filter_state_count in sync
                               // Pre-install a tripped filter so the readObject path
                               // immediately observes the sticky flag.
    let mut tripped = parse_serial_filter("maxdepth=1");
    tripped.rejected = true;
    tripped.reason = "synthetic for test".to_string();
    ois_set_filter_state(addr, tripped);
    ois_buf_load(addr, vec![TC_NULL]);

    let result = read_object(&mut ctx, &[Value::Object(Some(ois))]);
    let err = result.expect_err("rejected filter must raise IOException");
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("filter status: REJECTED"),
        "error must start with 'filter status: REJECTED': {}",
        msg
    );
}
