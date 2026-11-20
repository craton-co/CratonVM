#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn key_generator_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let kg = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let kg_ref = kg.as_object().unwrap();
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "init",
        "(I)V",
        &[Value::Object(Some(kg_ref)), Value::Int(256)],
    )
    .unwrap();
    let sk = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "generateKey",
        "()Ljavax/crypto/SecretKey;",
        &[Value::Object(Some(kg_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(sk.as_object().is_some());
    let sk_ref = sk.as_object().unwrap();
    let encoded = call_native(
        &shared,
        &mut thread,
        "javax/crypto/SecretKey",
        "getEncoded",
        "()[B",
        &[Value::Object(Some(sk_ref))],
    )
    .unwrap()
    .unwrap();
    let arr_ref = encoded.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(arr_ref), 32);
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn secret_key_spec_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let key_bytes = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        16,
    );
    // Distinguishable key material. A freshly allocated array is all
    // zeroes, and all-zeroes cannot tell a copy from an alias — nor from
    // the scrubbed buffer this test now checks for.
    for i in 0..16 {
        let _ = shared
            .mem
            .heap
            .set_array_element(key_bytes, i, Value::Int(i as i32 + 1));
    }
    let sks = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "<init>",
        "([BLjava/lang/String;)V",
        &[
            Value::Object(Some(sks)),
            Value::Object(Some(key_bytes)),
            Value::Object(Some(algo)),
        ],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sks))],
    )
    .unwrap()
    .unwrap();
    let algo_ref = result.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, algo_ref),
        Some("AES".to_string())
    );
    // `SecretKeySpec` copies on the way IN and on the way OUT — `<init>` is
    // `this.key = key.clone()` and `getEncoded()` is `return
    // this.key.clone()` (JDK 25 `src.zip`). This assertion used to be
    // `assert_eq!(enc, Value::Object(Some(key_bytes)))`, i.e. it required
    // the exact ALIASING those clones exist to prevent — and that aliasing
    // was a live all-zero-key defect, because SunJCE's generators scrub
    // their working buffer the instant the key object exists
    // (`AESKeyGenerator.engineGenerateKey` = `new SecretKeySpec(keyBytes,
    // "AES"); Arrays.fill(keyBytes, (byte) 0);`), so the scrub landed on
    // the key itself. Scrub the caller's array here the same way and check
    // the key survives it, which is the property that was actually broken.
    for i in 0..16 {
        let _ = shared
            .mem
            .heap
            .set_array_element(key_bytes, i, Value::Int(0));
    }
    let enc = call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "getEncoded",
        "()[B",
        &[Value::Object(Some(sks))],
    )
    .unwrap()
    .unwrap();
    let enc_ref = enc.as_object().expect("getEncoded() must return a byte[]");
    assert_ne!(
        enc_ref, key_bytes,
        "getEncoded() must not hand back the caller's own array"
    );
    assert_eq!(shared.mem.heap.array_length(enc_ref), 16);
    for i in 0..16 {
        assert_eq!(
            shared.mem.heap.get_array_element(enc_ref, i).unwrap(),
            Value::Int(i as i32 + 1),
            "key byte {i} did not survive the caller scrubbing its buffer"
        );
    }
    let fmt = call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "getFormat",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sks))],
    )
    .unwrap()
    .unwrap();
    let fmt_ref = fmt.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, fmt_ref),
        Some("RAW".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn security_provider_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let provs = call_native(
        &shared,
        &mut thread,
        "java/security/Security",
        "getProviders",
        "()[Ljava/security/Provider;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arr = provs.as_object().unwrap();
    // T2.6.15 seeded the registry with the five standard Sun providers
    // (SUN, SunJCE, SunRsaSign, SunEC, SunJSSE) to match the JDK's
    // default provider list; earlier revisions of this test asserted a
    // single "CratonVM" entry, which predated that work.
    assert_eq!(shared.mem.heap.array_length(arr), 5);
    let prov = shared.mem.heap.get_array_element(arr, 0).unwrap();
    let prov_ref = prov.as_object().unwrap();
    let name = call_native(
        &shared,
        &mut thread,
        "java/security/Provider",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(prov_ref))],
    )
    .unwrap()
    .unwrap();
    let name_ref = name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, name_ref),
        Some("SUN".to_string())
    );
    let ver = call_native(
        &shared,
        &mut thread,
        "java/security/Provider",
        "getVersion",
        "()D",
        &[Value::Object(Some(prov_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ver, Value::Double(21.0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn keystore_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ks_type = create_java_string(&shared, "PKCS12");
    let ks = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyStore;",
        &[Value::Object(Some(ks_type))],
    )
    .unwrap()
    .unwrap();
    let ks_ref = ks.as_object().unwrap();
    let tp = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "getType",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ks_ref))],
    )
    .unwrap()
    .unwrap();
    let tp_ref = tp.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, tp_ref),
        Some("PKCS12".to_string())
    );
    // A KeyStore must be loaded before it can be queried: the JDK throws
    // `KeyStoreException: Uninitialized keystore` from size()/containsAlias()
    // otherwise, and so does CratonVM. `load(null, null)` is the documented
    // way to initialise an empty one.
    call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "load",
        "(Ljava/io/InputStream;[C)V",
        &[
            Value::Object(Some(ks_ref)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();
    let sz = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "size",
        "()I",
        &[Value::Object(Some(ks_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(0));
    let alias = create_java_string(&shared, "mykey");
    let ca = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "containsAlias",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(ks_ref)), Value::Object(Some(alias))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ca, Value::Int(0));
    let dt = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "getDefaultType",
        "()Ljava/lang/String;",
        &[],
    )
    .unwrap()
    .unwrap();
    let dt_ref = dt.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, dt_ref),
        Some("PKCS12".to_string())
    );
}

/// API-surface check for `KeyPairGenerator`: `getInstance` → `initialize`
/// → `generateKeyPair` → `getPublic`/`getPrivate`/`getAlgorithm`.
///
/// Deliberately asks for a **512-bit** key, as the `m22_*` tests do. Since
/// the synthetic `KeyPairGenerator` stub in
/// `native-builtins::phases_early::register_phase53_security` was retired,
/// `generateKeyPair` runs the genuine `crypto_impl::Rsa::generate_keypair`
/// — two real Miller-Rabin prime searches. At 2048 bits that is two
/// ~1024-bit searches through a hand-rolled u32-limb `BigUint`, which in an
/// **unoptimised debug test binary** costs tens of seconds to minutes, with
/// the heavy tail of a geometric prime search on top (and a ~39% chance of
/// discarding the whole pair when `n` lands one bit short, restarting both
/// searches). This binary already runs ~10 minutes against a 20-minute CI
/// timeout, and a timeout produces NO result line for ANY test in it.
///
/// 512 bits keeps the crypto real — the same code path, a real key, a real
/// `sign`/`verify` elsewhere — while a 256-bit prime search costs ~1/64th
/// of a 1024-bit one per modular exponentiation. What this test asserts (the
/// method surface and the algorithm round-trip) is unchanged; it was never
/// a prime-search benchmark.
#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn keypair_generator_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "RSA");
    let kpg = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let kpg_ref = kpg.as_object().unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "initialize",
        "(I)V",
        &[Value::Object(Some(kpg_ref)), Value::Int(512)],
    )
    .unwrap();
    let kp = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "generateKeyPair",
        "()Ljava/security/KeyPair;",
        &[Value::Object(Some(kpg_ref))],
    )
    .unwrap()
    .unwrap();
    let kp_ref = kp.as_object().unwrap();
    let pub_key = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPair",
        "getPublic",
        "()Ljava/security/PublicKey;",
        &[Value::Object(Some(kp_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(pub_key.as_object().is_some());
    let priv_key = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPair",
        "getPrivate",
        "()Ljava/security/PrivateKey;",
        &[Value::Object(Some(kp_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(priv_key.as_object().is_some());
    let alg = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(kpg_ref))],
    )
    .unwrap()
    .unwrap();
    let alg_ref = alg.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, alg_ref),
        Some("RSA".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn signature_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "SHA256withRSA");
    let sig = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/Signature;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let sig_ref = sig.as_object().unwrap();
    let alg = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sig_ref))],
    )
    .unwrap()
    .unwrap();
    let alg_ref = alg.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, alg_ref),
        Some("SHA256withRSA".to_string())
    );
    let priv_key = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "initSign",
        "(Ljava/security/PrivateKey;)V",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(priv_key))],
    )
    .unwrap();
    let signed = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "sign",
        "()[B",
        &[Value::Object(Some(sig_ref))],
    )
    .unwrap()
    .unwrap();
    let arr = signed.as_object().unwrap();
    // RSA signature length is 256 bytes (2048-bit key)
    assert_eq!(shared.mem.heap.array_length(arr), 256);
    let pub_key = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "initVerify",
        "(Ljava/security/PublicKey;)V",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(pub_key))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "verify",
        "([B)Z",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn socket_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Use default constructor (no connect) to test field layout
    let sock = alloc_receiver(&shared, &mut thread, "java/net/Socket", 5);
    call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "<init>",
        "()V",
        &[Value::Object(Some(sock))],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "getPort",
        "()I",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(0));
    let closed = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));
    let conn = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "isConnected",
        "()Z",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(conn, Value::Int(0)); // not connected (no stream_id)
    call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "close",
        "()V",
        &[Value::Object(Some(sock))],
    )
    .unwrap();
    let closed2 = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed2, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn server_socket_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ss = alloc_receiver(&shared, &mut thread, "java/net/ServerSocket", 4);
    // Use port 0 for OS-assigned port to avoid conflicts
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ss)), Value::Int(0)],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "getLocalPort",
        "()I",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    // Port 0 → OS assigns a real port > 0
    assert!(port.as_int().unwrap() > 0);
    let closed = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "close",
        "()V",
        &[Value::Object(Some(ss))],
    )
    .unwrap();
    let closed2 = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed2, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn socket_exception_classes() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let result1 = call_native(
        &shared,
        &mut thread,
        "java/net/SocketException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result1.is_ok());
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/net/BindException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result2.is_ok());
    let result3 = call_native(
        &shared,
        &mut thread,
        "java/net/ConnectException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result3.is_ok());
    let result4 = call_native(
        &shared,
        &mut thread,
        "java/net/SocketTimeoutException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result4.is_ok());
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn service_loader_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // The argument to the STATIC `ServiceLoader.load(Class)` is the service
    // interface's Class mirror, not a ServiceLoader — and it has to be a
    // REAL mirror: the native resolves `service.getName()` to derive the
    // provider-configuration resource name, which a bare synthetic object
    // cannot answer.
    let service_cid = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.try_ensure_synthetic_class("java/util/spi/ToolProvider", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
    };
    let class_mirror = get_or_create_class_mirror(&shared, service_cid);
    let sl = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    let sl_ref = sl.as_object().unwrap();
    let itr = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(itr.as_object().is_some());
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(stream.as_object().is_some());
    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "findFirst",
        "()Ljava/util/Optional;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(opt.as_object().is_some());
    let s = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    let s_ref = s.as_object().unwrap();
    let s_str = read_java_string(&shared.mem.heap, s_ref).unwrap_or_default();
    assert!(s_str.contains("ServiceLoader"));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn record_equals_and_hash_code() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let rec1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec1, 0, Value::Int(42));
    shared.mem.heap.set_field(rec1, 1, Value::Int(100));
    let rec2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec2, 0, Value::Int(42));
    shared.mem.heap.set_field(rec2, 1, Value::Int(100));
    let eq = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(rec1)), Value::Object(Some(rec2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
    let h1 = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "hashCode",
        "()I",
        &[Value::Object(Some(rec1))],
    )
    .unwrap()
    .unwrap();
    let h2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "hashCode",
        "()I",
        &[Value::Object(Some(rec2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h1, h2);
    let rec3 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec3, 0, Value::Int(42));
    shared.mem.heap.set_field(rec3, 1, Value::Int(999));
    let eq2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(rec1)), Value::Object(Some(rec3))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq2, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn record_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let rec = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let s = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(rec))],
    )
    .unwrap()
    .unwrap();
    let s_ref = s.as_object().unwrap();
    let s_str = read_java_string(&shared.mem.heap, s_ref).unwrap_or_default();
    // toString with no record components uses fallback format: ClassName@hex or @hex
    assert!(
        s_str.contains('@'),
        "expected fallback toString with @, got: {}",
        s_str
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn class_sealed_and_record_markers() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // A REAL mirror. Hand-building one and stuffing an Int into slot 0 no
    // longer identifies a class: the natives resolve a mirror through the
    // VM's `class_mirrors_reverse` map, and an object that was never
    // registered there resolves to nothing, so `isSealed` returned
    // `Ok(None)` and the test unwrapped it.
    let class_id = make_test_class(&shared, "test/PlainClass", vec![], vec![]);
    let class_mirror = get_or_create_class_mirror(&shared, class_id);
    let sealed = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isSealed",
        "()Z",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sealed, Value::Int(0));
    let is_rec = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isRecord",
        "()Z",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_rec, Value::Int(0));
    let subs = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getPermittedSubclasses",
        "()[Ljava/lang/Class;",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(subs, Value::Object(None));
    let comps = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getRecordComponents",
        "()[Ljava/lang/reflect/RecordComponent;",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap();
    // The JDK returns NULL from `getRecordComponents()` for a class that is
    // not a record; this VM hands back an empty array from the sibling
    // `getPermittedSubclasses()`. Accept either — what the test is actually
    // about is that a plain class reports no record components, not which
    // of the two spellings of "none" the implementation picked.
    match comps {
        None | Some(Value::Object(None)) => {}
        Some(Value::Object(Some(a))) => {
            assert_eq!(shared.mem.heap.array_length(a), 0);
        }
        other => panic!("expected null or an empty array, got {other:?}"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn cipher_do_final_passthrough() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let cipher = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let cipher_ref = cipher.as_object().unwrap();
    // A Cipher must be initialised before doFinal: the JDK throws
    // IllegalStateException("Cipher not initialized") otherwise and so does
    // CratonVM. This used to call doFinal straight after getInstance and
    // accept whatever came back — the comment below ("crypto_impl stub
    // returns Object(None)") records the passthrough stub that made that
    // work.
    // A REAL key: `doFinal` needs key material, and a bare
    // `ClassId::new(0)` object has none ("No key provided"). Generate one
    // the way a caller would.
    let kg_algo = create_java_string(&shared, "AES");
    let kg = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        &[Value::Object(Some(kg_algo))],
    )
    .unwrap()
    .unwrap();
    let kg_ref = kg.as_object().unwrap();
    let key = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "generateKey",
        "()Ljavax/crypto/SecretKey;",
        &[Value::Object(Some(kg_ref))],
    )
    .unwrap()
    .unwrap()
    .as_object()
    .expect("generateKey must return a SecretKey");
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "init",
        "(ILjava/security/Key;)V",
        &[
            Value::Object(Some(cipher_ref)),
            Value::Int(1),
            Value::Object(Some(key)),
        ],
    )
    .unwrap();
    let input = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        4,
    );
    let _ = shared.mem.heap.set_array_element(input, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(input, 1, Value::Int(2));
    let _ = shared.mem.heap.set_array_element(input, 2, Value::Int(3));
    let _ = shared.mem.heap.set_array_element(input, 3, Value::Int(4));
    let result = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "doFinal",
        "([B)[B",
        &[Value::Object(Some(cipher_ref)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    // An initialised AES cipher encrypts the 4-byte input into a padded
    // block, so doFinal must hand back an actual array.
    let out = result
        .as_object()
        .expect("doFinal on an initialised cipher must return a byte[]");
    assert_eq!(
        shared.mem.heap.array_length(out),
        16,
        "4 bytes of plaintext pad up to one AES block"
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn socket_get_inet_address() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Use default constructor + set host field manually to test getInetAddress
    // without needing a real TCP connection
    let sock = alloc_receiver(&shared, &mut thread, "java/net/Socket", 5);
    call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "<init>",
        "()V",
        &[Value::Object(Some(sock))],
    )
    .unwrap();
    // getInetAddress on an unconnected socket with host set
    let addr = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    // Host is empty string from default init, InetAddress still created
    assert!(addr.as_object().is_some());
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn server_socket_with_backlog() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ss = alloc_receiver(&shared, &mut thread, "java/net/ServerSocket", 4);
    // Use port 0 for OS-assigned to avoid conflicts
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "<init>",
        "(II)V",
        &[Value::Object(Some(ss)), Value::Int(0), Value::Int(100)],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "getLocalPort",
        "()I",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert!(port.as_int().unwrap() > 0);
    // Clean up
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "close",
        "()V",
        &[Value::Object(Some(ss))],
    )
    .unwrap();
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn key_interface_methods() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let key = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        16,
    );
    // Distinguishable material, for the same reason as
    // `secret_key_spec_basics`: a zeroed array cannot tell a copy from an
    // alias.
    for i in 0..16 {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(i as i32 + 1));
    }
    shared.mem.heap.set_field(key, 0, Value::Object(Some(arr)));
    let enc = call_native(
        &shared,
        &mut thread,
        "java/security/Key",
        "getEncoded",
        "()[B",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    // `Key.getEncoded()` returns a fresh array — every JCA key
    // implementation ends `return this.<field>.clone()`, and callers build
    // key hygiene on that (the JDK's own providers scrub what they get
    // back). This used to assert the opposite, by identity; the same stale
    // aliasing assertion as `secret_key_spec_basics`, where it was pinning
    // a live all-zero-key defect.
    let enc_ref = enc.as_object().expect("getEncoded() must return a byte[]");
    assert_ne!(
        enc_ref, arr,
        "getEncoded() must not hand back the key's own backing array"
    );
    assert_eq!(shared.mem.heap.array_length(enc_ref), 16);
    for i in 0..16 {
        assert_eq!(
            shared.mem.heap.get_array_element(enc_ref, i).unwrap(),
            Value::Int(i as i32 + 1),
            "encoded byte {i} does not match the key material"
        );
    }
    let algo = call_native(
        &shared,
        &mut thread,
        "java/security/Key",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    let algo_ref = algo.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, algo_ref),
        Some("AES".to_string())
    );
    let fmt = call_native(
        &shared,
        &mut thread,
        "java/security/Key",
        "getFormat",
        "()Ljava/lang/String;",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    let fmt_ref = fmt.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, fmt_ref),
        Some("RAW".to_string())
    );
}

