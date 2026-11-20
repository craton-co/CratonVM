use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

const VM_A: usize = 0xC34C_0A01;
const VM_B: usize = 0xC34C_0A02;
const VM_C: usize = 0xC34C_0A03;
const VM_D: usize = 0xC34C_0A04;
const VM_E: usize = 0xC34C_0A05;
const VM_F: usize = 0xC34C_0A06;
const VM_G: usize = 0xC34C_0A07;

/// SEQUENCE { INTEGER 0, SEQUENCE { OID rsaEncryption, NULL },
/// OCTET STRING 4 } -- a well-formed, if tiny, PKCS#8 `PrivateKeyInfo`.
const PLAIN: &[u8] = &[
    0x30, 0x18, 0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d,
    0x01, 0x01, 0x01, 0x05, 0x00, 0x04, 0x04, 0xde, 0xad, 0xbe, 0xef,
];

/// Forgets only the VMs its own test used (w24 lesson pp).
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::t27_tls::forget_vm_tls_state(vm);
            forget_vm_keystores(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn entry(id: i32, alias: &str) -> Option<KeyStoreEntry> {
    with_store(id, |s| s.entries.get(alias).cloned()).flatten()
}

fn byte_array(c: &mut crate::test_utils::MockNativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = c.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        c.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    arr
}

// -- section 1: the slot tier ------------------------------------------

/// The decision over plain values: a declared slot 4 is never the id
/// slot, whatever it holds; an undeclared one is, unless it holds a live
/// reference.
#[test]
fn the_slot_verdict_refuses_every_declared_slot() {
    // The synthetic `java.security.KeyStore`: five slots, none declared
    // (synthetic JDK) or four declared (real JDK, `max(5, 4)`).
    assert!(store_id_slot_verdict(5, 0, Value::Int(0)));
    assert!(store_id_slot_verdict(5, 4, Value::Int(7)));
    // `KeyStoreDelegator`: seven declared, slot 4 is `type` (String).
    assert!(!store_id_slot_verdict(7, 7, Value::Object(None)));
    assert!(!store_id_slot_verdict(7, 7, Value::Int(0)));
    // A real class whose slot 4 is an `int` field: still not the id slot,
    // or its value would be read back as a store id.
    assert!(!store_id_slot_verdict(9, 9, Value::Int(77)));
    // No slot 4 at all (`JavaKeyStore$JKS` has one field).
    assert!(!store_id_slot_verdict(1, 1, Value::Int(0)));
    assert!(!store_id_slot_verdict(4, 0, Value::Int(0)));
    // Layout unknown (the trait default reports 0 declared): a live
    // reference in the slot is certainly not an id. Never dereferenced;
    // 8-byte aligned for `from_raw`'s debug assertion.
    let r = unsafe { ObjectRef::from_raw(0x34C0_0010usize as *mut u8) };
    assert!(!store_id_slot_verdict(7, 0, Value::Object(Some(r))));
}

/// The same, through a mock that models declared layouts: a real
/// `KeyStoreDelegator`-shaped receiver is refused, the synthetic layout is
/// accepted, and an `int` field at slot 4 is not read back as a store id.
#[test]
fn a_declared_slot_four_is_neither_written_nor_read_as_the_store_id() {
    use cratonvm_native_api::test_mock::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let delegator = ctx.declare_class(
        "sun/security/util/KeyStoreDelegator",
        &[
            ("primaryType", "Ljava/lang/String;"),
            ("secondaryType", "Ljava/lang/String;"),
            ("primaryKeyStore", "Ljava/lang/Class;"),
            ("secondaryKeyStore", "Ljava/lang/Class;"),
            ("type", "Ljava/lang/String;"),
            ("keystore", "Ljava/security/KeyStoreSpi;"),
            ("compatModeEnabled", "Z"),
        ],
    );
    let ks = ctx.alloc_object(delegator, 7);
    assert!(
        !store_id_slot_is_synthetic(&ctx, ks),
        "slot 4 of a KeyStoreDelegator is its `type` field; the store id was \
         written into it, nulling it on every engineLoad"
    );

    let int_slot = ctx.declare_class(
        "p/IntAtSlotFour",
        &[("a", "I"), ("b", "I"), ("c", "I"), ("d", "I"), ("e", "I")],
    );
    let obj = ctx.alloc_object(int_slot, 5);
    ctx.set_field(obj, FIELD_STORE_ID, Value::Int(77));
    assert!(!store_id_slot_is_synthetic(&ctx, obj));
    assert_eq!(
        get_store_id(&mut ctx, obj),
        0,
        "a real int field at slot 4 must not be read back as a store id"
    );

    // The real-JDK `java.security.KeyStore` shape `tls.rs` allocates:
    // four declared fields, five slots (`max(5, 4)`).
    let real_ks = ctx.declare_class(
        "java/security/KeyStore",
        &[
            ("keyStoreSpi", "Ljava/security/KeyStoreSpi;"),
            ("type", "Ljava/lang/String;"),
            ("provider", "Ljava/security/Provider;"),
            ("initialized", "Z"),
        ],
    );
    let synthetic = ctx.alloc_object(real_ks, 5);
    ctx.set_field(synthetic, FIELD_STORE_ID, Value::Int(0));
    assert!(store_id_slot_is_synthetic(&ctx, synthetic));
    ctx.set_field(synthetic, FIELD_STORE_ID, Value::Int(12));
    assert_eq!(
        get_store_id(&mut ctx, synthetic),
        12,
        "the synthetic slot still answers"
    );
}

/// `set_store_id` leaves a reference in slot 4 alone and the id is still
/// found (identity tier); on the synthetic layout it still writes the slot.
#[test]
fn set_store_id_writes_only_the_synthetic_slot() {
    let _teardown = Teardown(&[VM_A]);
    let mut c = ctx_in(VM_A);

    let real = c.alloc_object(ClassId::new(0), 7);
    let referent = c.fresh_object_ref();
    c.set_field(real, FIELD_STORE_ID, Value::Object(Some(referent)));
    let id = keystore_register_in_vm(VM_A, LoadedKeyStore::default());
    set_store_id(&mut c, real, id);
    assert_eq!(
        c.get_field(real, FIELD_STORE_ID),
        Value::Object(Some(referent)),
        "set_store_id overwrote a reference field with the store id"
    );
    assert_eq!(
        get_store_id(&mut c, real),
        id,
        "found through the identity tier"
    );

    let synthetic = c.alloc_object(ClassId::new(0), 5);
    c.set_field(synthetic, FIELD_STORE_ID, Value::Int(0));
    let id2 = keystore_register_in_vm(VM_A, LoadedKeyStore::default());
    set_store_id(&mut c, synthetic, id2);
    assert_eq!(c.get_field(synthetic, FIELD_STORE_ID), Value::Int(id2));
    assert_eq!(get_store_id(&mut c, synthetic), id2);
}

// -- section 2: the three delegator methods ----------------------------

/// Every class the engine surface is registered on gets the three, as
/// `Bridge` (never an inherited `SyntheticStub`, AGENTS.md).
#[test]
fn the_three_delegator_methods_are_bridges_on_every_engine_class() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    register_keystore_real(&mut registry);
    let classes = [
        PKCS12_FQN,
        JKS_FQN,
        JKS_INNER_JKS_FQN,
        JKS_INNER_DUAL_FQN,
        PKCS12_INNER_DUAL_FQN,
        JKS_INNER_CASE_EXACT_FQN,
        DKS_FQN,
        JCEKS_FQN,
    ];
    let triples = [
        ("engineGetAttributes", "(Ljava/lang/String;)Ljava/util/Set;"),
        (
            "engineEntryInstanceOf",
            "(Ljava/lang/String;Ljava/lang/Class;)Z",
        ),
        (
            "engineSetKeyEntry",
            "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V",
        ),
    ];
    for fqn in classes {
        for (name, desc) in triples {
            // SBR wave 4 (lane SSL): the keystore SPI rows in
            // `RETIRED_SHADOW_SBR_SSL_TRIPLES` are re-tagged at
            // registration. Retired, the whole engine surface is real
            // bytecode over a store the real constructor built, so the
            // delegator's `keystore` field is not null any more; the rest
            // (DKS, JCEKS, and `engineGetAttributes`, which JDK 17 does not
            // declare) must stay Bridges.
            let want = if cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
                fqn, name, desc,
            ) {
                cratonvm_native_api::NativeKind::SyntheticStub
            } else {
                cratonvm_native_api::NativeKind::Bridge
            };
            assert_eq!(
                registry.kind_of(fqn, name, desc),
                Some(want),
                "{fqn}.{name}{desc} must be a registered {want:?}: an unregistered \
                 Bridge lets the real KeyStoreDelegator body dereference the null \
                 `keystore` field of a native-backed store"
            );
        }
    }
}

/// HotSpot 25.0.3's answers, both rules (see `entry_is_instance_of`).
#[test]
fn entry_instance_of_follows_both_jdk_rules() {
    const T: &str = "java/security/KeyStore$TrustedCertificateEntry";
    const P: &str = "java/security/KeyStore$PrivateKeyEntry";
    const S: &str = "java/security/KeyStore$SecretKeyEntry";
    const E: &str = "java/security/KeyStore$Entry";
    let cert = EntryKind::TrustedCert {
        cert_der: vec![0x30, 0x00],
    };
    let key_with_chain = EntryKind::PrivateKey {
        key_der: PLAIN.to_vec(),
        chain: vec![vec![0x30, 0x00]],
        protected: None,
    };
    let key_no_chain = EntryKind::PrivateKey {
        key_der: PLAIN.to_vec(),
        chain: Vec::new(),
        protected: None,
    };
    let secret = EntryKind::SecretKey {
        key_bytes: vec![1; 16],
        algorithm: "AES".into(),
    };
    let row =
        |k: &EntryKind, pkcs12: bool| [T, P, S, E].map(|w| entry_is_instance_of(k, w, pkcs12));
    for pkcs12 in [false, true] {
        assert_eq!(row(&cert, pkcs12), [true, false, false, false]);
        assert_eq!(row(&key_with_chain, pkcs12), [false, true, false, false]);
        assert_eq!(row(&secret, pkcs12), [false, false, true, false]);
    }
    // `setKeyEntry(a, byte[], null)`: JKS/JCEKS call it a SecretKeyEntry
    // (key entry, no certificate), PKCS12 a PrivateKeyEntry.
    assert_eq!(row(&key_no_chain, false), [false, false, true, false]);
    assert_eq!(row(&key_no_chain, true), [false, true, false, false]);
}

/// The whole native, with a real `Class` argument resolved through the
/// mirror.
#[test]
fn engine_entry_instance_of_answers_from_the_side_table() {
    let _teardown = Teardown(&[VM_B]);
    let mut c = ctx_in(VM_B);
    let ks = c.fresh_object_ref();
    let id = keystore_register_in_vm(VM_B, LoadedKeyStore::default());
    set_store_id(&mut c, ks, id);
    assert!(keystore_set_cert_entry(id, "c", vec![0x30, 0x00]));

    let trusted_cid = c
        .ensure_class_initialized("java/security/KeyStore$TrustedCertificateEntry")
        .expect("mock class");
    let private_cid = c
        .ensure_class_initialized("java/security/KeyStore$PrivateKeyEntry")
        .expect("mock class");
    let trusted = c.fresh_object_ref();
    c.set_field(trusted, 0, Value::Int(trusted_cid.as_u32() as i32));
    let private = c.fresh_object_ref();
    c.set_field(private, 0, Value::Int(private_cid.as_u32() as i32));
    let alias = c.create_string("C");

    let args = |mirror: ObjectRef| {
        [
            Value::Object(Some(ks)),
            Value::Object(Some(alias)),
            Value::Object(Some(mirror)),
        ]
    };
    assert_eq!(
        engine_entry_instance_of(&mut c, &args(trusted)).expect("no throw"),
        Some(Value::Int(1)),
        "the alias is case-folded, as the JDK's own SPIs fold it"
    );
    assert_eq!(
        engine_entry_instance_of(&mut c, &args(private)).expect("no throw"),
        Some(Value::Int(0))
    );
    assert!(
        engine_entry_instance_of(
            &mut c,
            &[
                Value::Object(Some(ks)),
                Value::Object(None),
                Value::Object(Some(trusted)),
            ],
        )
        .is_err(),
        "a null alias is NullPointerException"
    );
}

/// `setKeyEntry(String, byte[], Certificate[])` stores the envelope as the
/// entry's own, the right password opens it, a non-envelope is refused
/// with KeyStoreException and a null key with NPE.
#[test]
fn a_protected_key_entry_is_stored_as_its_envelope() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = ctx_in(VM_C);
    let ks = c.fresh_object_ref();
    let id = keystore_register_in_vm(VM_C, LoadedKeyStore::default());
    set_store_id(&mut c, ks, id);

    let env = jks_protect_key(PLAIN, b"kp").expect("OS entropy in the test environment");
    let key_arr = byte_array(&mut c, &env);
    let alias = c.create_string("Env");
    engine_set_protected_key_entry(
        &mut c,
        &[
            Value::Object(Some(ks)),
            Value::Object(Some(alias)),
            Value::Object(Some(key_arr)),
            Value::Object(None),
        ],
    )
    .expect("an EncryptedPrivateKeyInfo is accepted");
    let stored = entry(id, "env").expect("the entry is in the side table");
    assert!(stored.creation_time_ms > 0, "an API-set entry is dated now");
    match &stored.kind {
        EntryKind::PrivateKey {
            key_der,
            chain,
            protected,
        } => {
            assert_eq!(key_der, &env);
            assert!(chain.is_empty());
            assert_eq!(protected.as_deref(), Some(env.as_slice()));
        }
        other => panic!("stored as {other:?}"),
    }
    // `engineGetKey`'s unlock with the right password yields the key and
    // keeps the envelope for the next wrong-password check.
    keystore_unlock_private_keys(VM_C, id, b"kp");
    match entry(id, "env").map(|e| e.kind) {
        Some(EntryKind::PrivateKey {
            key_der, protected, ..
        }) => {
            assert_eq!(key_der, PLAIN);
            assert_eq!(protected.as_deref(), Some(env.as_slice()));
        }
        other => panic!("after unlock: {other:?}"),
    }

    let junk_arr = byte_array(&mut c, &[1, 2, 3]);
    let junk_alias = c.create_string("junk");
    assert!(
        engine_set_protected_key_entry(
            &mut c,
            &[
                Value::Object(Some(ks)),
                Value::Object(Some(junk_alias)),
                Value::Object(Some(junk_arr)),
                Value::Object(None),
            ],
        )
        .is_err(),
        "JKS refuses bytes that are not an EncryptedPrivateKeyInfo"
    );
    assert!(entry(id, "junk").is_none());

    let no_key_alias = c.create_string("nokey");
    assert!(
        engine_set_protected_key_entry(
            &mut c,
            &[
                Value::Object(Some(ks)),
                Value::Object(Some(no_key_alias)),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .is_err(),
        "a null key is NullPointerException"
    );
    assert!(entry(id, "nokey").is_none());
}

/// JCEKS accepts any bytes, as `JceKeyStore` does; no password then opens
/// them, so `getKey` refuses rather than handing the bytes back as a key.
#[test]
fn jceks_accepts_any_protected_bytes() {
    let _teardown = Teardown(&[VM_D]);
    let mut c = ctx_in(VM_D);
    let cid = c.ensure_class_initialized(JCEKS_FQN).expect("mock class");
    let ks = c.alloc_object(cid, 1);
    let id = keystore_register_in_vm(VM_D, LoadedKeyStore::default());
    set_store_id(&mut c, ks, id);
    let arr = byte_array(&mut c, &[1, 2, 3]);
    let alias = c.create_string("raw");
    engine_set_protected_key_entry(
        &mut c,
        &[
            Value::Object(Some(ks)),
            Value::Object(Some(alias)),
            Value::Object(Some(arr)),
            Value::Object(None),
        ],
    )
    .expect("JCEKS does not validate");
    match entry(id, "raw").map(|e| e.kind) {
        Some(EntryKind::PrivateKey { protected, .. }) => {
            let env = protected.expect("kept as the entry's envelope");
            assert_eq!(recover_key_any_scheme(&env, b"anything"), None);
        }
        other => panic!("stored as {other:?}"),
    }
}

/// SunPKCS12's attribute pairs, against the values HotSpot 25.0.3 printed.
#[test]
fn pkcs12_attributes_match_sunpkcs12s_shape() {
    let cert = KeyStoreEntry {
        alias: "c".into(),
        creation_time_ms: 1,
        kind: EntryKind::TrustedCert {
            cert_der: vec![0x30, 0x00],
        },
    };
    let mut got = pkcs12_entry_attributes(&cert);
    got.sort();
    assert_eq!(
        got,
        vec![
            (PKCS9_FRIENDLY_NAME_OID, "c".to_string()),
            (PKCS12_TRUSTED_KEY_USAGE_OID, "2.5.29.37.0".to_string()),
        ]
    );
    let key = KeyStoreEntry {
        alias: "k".into(),
        creation_time_ms: 1_790_407_363_089,
        kind: EntryKind::PrivateKey {
            key_der: PLAIN.to_vec(),
            chain: Vec::new(),
            protected: None,
        },
    };
    let mut got = pkcs12_entry_attributes(&key);
    got.sort();
    assert_eq!(
        got,
        vec![
            (PKCS9_FRIENDLY_NAME_OID, "k".to_string()),
            (
                PKCS9_LOCAL_KEY_ID_OID,
                "54:69:6d:65:20:31:37:39:30:34:30:37:33:36:33:30:38:39".to_string()
            ),
        ],
        "HotSpot's localKeyID for an entry dated 1790407363089 is the UTF-8 of \
         \"Time 1790407363089\""
    );
    let unnamed = KeyStoreEntry {
        alias: String::new(),
        ..cert
    };
    assert!(
        pkcs12_entry_attributes(&unnamed)
            .iter()
            .all(|(oid, _)| *oid != PKCS9_FRIENDLY_NAME_OID),
        "an empty value would throw in the PKCS12Attribute constructor"
    );
}

/// A null alias is NPE before anything is built.
#[test]
fn get_attributes_with_a_null_alias_throws() {
    let _teardown = Teardown(&[VM_E]);
    let mut c = ctx_in(VM_E);
    let ks = c.fresh_object_ref();
    assert!(
        engine_get_attributes(&mut c, &[Value::Object(Some(ks)), Value::Object(None)]).is_err()
    );
}

/// Discovery: the envelope a `getKey` opens stays the entry's own, so a
/// later WRONG password is still refused and a re-store keeps the entry
/// password (HotSpot 25.0.3, all three types).
#[test]
fn an_opened_envelope_still_guards_the_entry() {
    let _teardown = Teardown(&[VM_G]);
    let mut c = ctx_in(VM_G);
    let env = jks_protect_key(PLAIN, b"kp").expect("OS entropy in the test environment");
    let mut store = LoadedKeyStore::default();
    store.entries.insert(
        "k".to_string(),
        KeyStoreEntry {
            alias: "k".to_string(),
            creation_time_ms: 0,
            // As `load_jks` leaves a key the STORE password did not open.
            kind: EntryKind::PrivateKey {
                key_der: env.clone(),
                chain: Vec::new(),
                protected: None,
            },
        },
    );
    let id = keystore_register_in_vm(VM_G, store);
    let ks = c.fresh_object_ref();
    set_store_id(&mut c, ks, id);

    keystore_unlock_private_keys(VM_G, id, b"kp");
    match entry(id, "k").map(|e| e.kind) {
        Some(EntryKind::PrivateKey {
            key_der, protected, ..
        }) => {
            assert_eq!(key_der, PLAIN, "the right password opens the key");
            assert_eq!(
                protected.as_deref(),
                Some(env.as_slice()),
                "the opened envelope must be kept: it is the only thing a later \
                 getKey can check its password against"
            );
        }
        other => panic!("after unlock: {other:?}"),
    }

    // `getKey("k", "wrong")` after the correct one: refused.
    let alias = c.create_string("k");
    let wrong = c.new_array(ArrayElementType::Char, 5);
    for (i, ch) in "wrong".chars().enumerate() {
        c.set_array_element(wrong, i, Value::Int(ch as i32));
    }
    assert!(
        engine_get_key(
            &mut c,
            &[
                Value::Object(Some(ks)),
                Value::Object(Some(alias)),
                Value::Object(Some(wrong)),
            ],
        )
        .is_err(),
        "a wrong password after a right one was accepted: nothing was left to check it"
    );

    // A JKS re-store keeps the entry password, not the store's.
    let snapshot = keystore_lookup(id).expect("registered");
    let back = load_jks(&write_jks(&snapshot, b"pw"), b"pw").expect("reload");
    match &back.entries.get("k").expect("alias kept").kind {
        EntryKind::PrivateKey { key_der, .. } => {
            assert!(
                is_jks_encrypted_private_key(key_der),
                "the store password opened it"
            );
            assert_eq!(
                recover_key_any_scheme(key_der, b"kp").as_deref(),
                Some(PLAIN)
            );
        }
        other => panic!("came back as {other:?}"),
    }
    // A PKCS#12 write does not carry the JKS envelope into the file.
    let back =
        load_pkcs12(&write_pkcs12(&snapshot, b"pw").expect("write"), b"pw").expect("reload");
    match &back.entries.get("k").expect("alias kept").kind {
        EntryKind::PrivateKey { key_der, .. } => assert_eq!(key_der.as_slice(), PLAIN),
        other => panic!("came back as {other:?}"),
    }
}

/// Discovery: the three API setters date the entry now, not the epoch.
#[test]
fn api_set_entries_are_dated_now() {
    let _teardown = Teardown(&[VM_F]);
    let id = keystore_register_in_vm(VM_F, LoadedKeyStore::default());
    assert!(keystore_set_cert_entry(id, "c", vec![0x30, 0x00]));
    assert!(keystore_set_key_entry(
        id,
        "k",
        PLAIN.to_vec(),
        Vec::new(),
        None
    ));
    assert!(keystore_set_secret_key_entry(id, "s", vec![1; 16], "AES"));
    for alias in ["c", "k", "s"] {
        let e = entry(id, alias).expect("set");
        assert!(
            e.creation_time_ms > 1_600_000_000_000,
            "{alias}: getCreationDate of a just-set entry was the epoch"
        );
    }
}
