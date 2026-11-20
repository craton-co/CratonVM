use super::test_fixtures::*;
use super::*;
use cratonvm_native_api::NativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ObjectRef;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// A nested `set_active_native_context` must RESTORE the outer window, not
/// clear it.
///
/// There are two publishers now — `http_url_connection::perform` on the
/// client path and `x509_manager::do_check_trusted` on the server path —
/// and on a client connection that validates a chain they nest. With the
/// old clear-to-`None` drop, the inner guard's return would leave the outer
/// frame with no window, and every later `with_active_native_context` there
/// would answer `None`. For `JavaKeyManagerResolver::resolve` that is not a
/// crash, it is "no client certificate" — a silent wrong answer, on the one
/// path whose whole job is to produce one.
///
/// Asserted through `with_active_native_context`, the real reader, rather
/// than by inspecting the thread-local: that is what every consumer
/// actually calls.
#[test]
fn a_nested_active_native_context_restores_the_outer_one() {
    use crate::test_utils::MockNativeContext;
    let mut outer = MockNativeContext::new();
    let mut inner = MockNativeContext::new();

    assert!(
        super::with_active_native_context(|_| ()).is_none(),
        "no window should be published before the first guard"
    );
    let outer_guard = super::set_active_native_context(&mut outer);
    let outer_ptr = ACTIVE_TLS_NATIVE_CTX.with(|c| c.get());
    assert!(outer_ptr.is_some(), "the outer guard must publish a window");
    {
        let _inner_guard = super::set_active_native_context(&mut inner);
        let inner_ptr = ACTIVE_TLS_NATIVE_CTX.with(|c| c.get());
        assert!(inner_ptr.is_some());
        assert!(
            !std::ptr::addr_eq(inner_ptr.unwrap(), outer_ptr.unwrap()),
            "the inner guard must publish ITS context while it is alive"
        );
    }
    assert!(
        super::with_active_native_context(|_| ()).is_some(),
        "the inner guard cleared the window instead of restoring it — the \
         enclosing frame is now running with no published context, and every \
         `with_active_native_context` in it silently answers None"
    );
    assert!(
        std::ptr::addr_eq(
            ACTIVE_TLS_NATIVE_CTX.with(|c| c.get()).unwrap(),
            outer_ptr.unwrap()
        ),
        "the restored window must be the OUTER context, not some other one"
    );
    drop(outer_guard);
    assert!(
        super::with_active_native_context(|_| ()).is_none(),
        "the outermost guard must still clear the window on the way out"
    );
}

/// A server session cache is per `SSLContext` **and per client-auth
/// policy** — a connection asking for a client certificate must not be
/// able to resume one that did not.
///
/// Resuming across that boundary replays a handshake that sent no
/// `CertificateRequest`, so the client is never asked and the request is
/// silently cancelled. That is what defeated `wants_deferred_client_auth`
/// and produced `TestClientCert`'s whole failing set: the second engine
/// really did offer client auth (`engine_begin request=true`, measured)
/// and `JavaKeyManagerResolver::resolve` was still called zero times.
///
/// Asserted on `Arc::ptr_eq`, which is the property that matters — two
/// distinct caches, not merely two lookups. The same-policy case is
/// asserted too, because a partition that never shares would silently
/// disable resumption altogether and still pass a difference-only check.
#[test]
fn a_server_session_cache_is_partitioned_by_client_auth_policy() {
    let key = 0x5eed_0000_0000_0001u64;
    let without = super::ctx_server_session_store(key, false);
    let with = super::ctx_server_session_store(key, true);
    assert!(
        !Arc::ptr_eq(&without, &with),
        "a client-auth connection shares the no-client-auth session cache, so it \
         can resume a session that carries no client certificate — the resumption \
         then cancels the CertificateRequest and the peer is never asked"
    );
    assert!(
        Arc::ptr_eq(&without, &super::ctx_server_session_store(key, false)),
        "same context, same policy must share ONE cache — otherwise this \
         partition has disabled server-side resumption instead of scoping it"
    );
    assert!(
        Arc::ptr_eq(&with, &super::ctx_server_session_store(key, true)),
        "same context, same policy must share ONE cache (client-auth side)"
    );
}

/// The exact PKCS#8 key CratonVM lifts out of Spring Boot's
/// `spring-boot-ldap` test keystore
/// (`.../ldap/autoconfigure/embedded/test.jks`, alias `mykey`, 335 bytes),
/// captured with `CRATONVM_DBG=tls-hs` on 2026-08-11. Truncated to the
/// header — the algorithm OID is all this test reads.
const LDAP_TEST_JKS_DSA_KEY_PREFIX: &[u8] = &[
    0x30, 0x82, 0x01, 0x4b, 0x02, 0x01, 0x00, 0x30, 0x82, 0x01, 0x2c, 0x06, 0x07, 0x2a, 0x86,
    0x48, 0xce, 0x38, 0x04, 0x01, 0x30, 0x82, 0x01, 0x1f, 0x02, 0x81, 0x00,
];

/// `EmbeddedLdapAutoConfigurationTests.whenSslBundleIsConfiguredLdapsListenerIsConfigured`
/// fails on Windows with rustls's generic "failed to parse private key as
/// RSA, ECDSA, or EdDSA", which says nothing about why. It is a DSA key, and
/// naming that is what separates "broken keystore" from "no TLS backend on
/// this platform speaks DHE_DSS".
#[test]
fn ldap_test_keystore_key_is_reported_as_dsa() {
    assert_eq!(
        pkcs8_algorithm_name(LDAP_TEST_JKS_DSA_KEY_PREFIX),
        Some("DSA")
    );
}

#[test]
fn pkcs8_algorithm_name_reads_the_algorithm_oid() {
    // Minimal PKCS#8 prefixes: SEQUENCE { INTEGER 0, SEQUENCE { OID .. } }
    let rsa = [
        0x30u8, 0x10, 0x02, 0x01, 0x00, 0x30, 0x0b, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
        0x0d, 0x01, 0x01, 0x01,
    ];
    let ec = [
        0x30u8, 0x0e, 0x02, 0x01, 0x00, 0x30, 0x09, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d,
        0x02, 0x01,
    ];
    let ed = [
        0x30u8, 0x0a, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70,
    ];
    assert_eq!(pkcs8_algorithm_name(&rsa), Some("RSA"));
    assert_eq!(pkcs8_algorithm_name(&ec), Some("EC"));
    assert_eq!(pkcs8_algorithm_name(&ed), Some("Ed25519"));
    // Not a key at all, and a truncated one: both must decline rather than
    // name an algorithm the bytes do not carry.
    assert_eq!(pkcs8_algorithm_name(b"not der"), None);
    assert_eq!(pkcs8_algorithm_name(&rsa[..6]), None);
}

/// Guard so tests that mutate VM 0's `RUNTIME_TLS_IDENTITY` slot don't
/// race with each other. Each test acquires the lock for its duration;
/// the previous slot value is restored on drop. VM 0 because these tests
/// drive natives through `mock_ctx()`, whose `vm_identity()` is 0 (the
/// slot is per VM since gc-common w11-a).
static IDENTITY_TEST_LOCK: StdMutex<()> = StdMutex::new(());
const IDENTITY_TEST_VM: usize = 0;

#[test]
fn der_identity_to_pem_preserves_private_key_encoding() {
    // Minimal DER envelopes are sufficient for the label sniffer: its
    // decision only depends on the outer sequence, version, and next tag.
    let pkcs8 = [0x30, 0x07, 0x02, 0x01, 0x00, 0x30, 0x02, 0x06, 0x00];
    let pkcs1_rsa = [0x30, 0x08, 0x02, 0x01, 0x00, 0x02, 0x03, 0x01, 0x02, 0x03];
    let sec1_ec = [0x30, 0x05, 0x02, 0x01, 0x00, 0x04, 0x00];

    assert!(der_identity_to_pem(&pkcs8, &[])
        .1
        .starts_with("-----BEGIN PRIVATE KEY-----"));
    assert!(der_identity_to_pem(&pkcs1_rsa, &[])
        .1
        .starts_with("-----BEGIN RSA PRIVATE KEY-----"));
    assert!(der_identity_to_pem(&sec1_ec, &[])
        .1
        .starts_with("-----BEGIN EC PRIVATE KEY-----"));
}

/// RAII helper: stash a runtime TLS identity for the lifetime of a
/// test, then restore whatever was there before. Acquires the
/// `IDENTITY_TEST_LOCK` so concurrent tests are serialized.
struct IdentityGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<super::RuntimeTlsIdentity>,
}

impl IdentityGuard {
    fn install(identity: super::RuntimeTlsIdentity) -> Self {
        // `lock()` can fail only if the mutex is poisoned by a panicking
        // test; recover the guard so we still serialize.
        let lock = IDENTITY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = super::runtime_tls_identity(IDENTITY_TEST_VM);
        super::set_runtime_tls_identity(IDENTITY_TEST_VM, Some(identity));
        IdentityGuard { _lock: lock, prev }
    }

    fn install_none() -> Self {
        let lock = IDENTITY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = super::runtime_tls_identity(IDENTITY_TEST_VM);
        super::set_runtime_tls_identity(IDENTITY_TEST_VM, None);
        IdentityGuard { _lock: lock, prev }
    }
}

impl Drop for IdentityGuard {
    fn drop(&mut self) {
        super::set_runtime_tls_identity(IDENTITY_TEST_VM, self.prev.take());
    }
}

/// Install the test fixtures (server cert+key + test CA) as the
/// runtime TLS identity for the duration of a test.
fn install_test_identity() -> IdentityGuard {
    IdentityGuard::install(super::RuntimeTlsIdentity {
        cert_pem: SERVER_CRT_PEM.to_string(),
        key_pem: SERVER_KEY_PEM.to_string(),
        client_ca_pem: Some(CA_CRT_PEM.to_string()),
    })
}

fn fake_object_ref(tag: usize) -> ObjectRef {
    let ptr = (0x1000_0000usize + tag * 0x1000) as *mut u8;
    unsafe { ObjectRef::from_raw(ptr) }
}

// -------------------------------------------------------------------
// bb_view buffer-shape resolution (Reactor-Netty SSLEngine
// BUFFER_UNDERFLOW fix, 2026-07-07). The mock maps real-JDK
// java.nio.Buffer field names for "java/nio/*ByteBuffer" classes:
// mark@0, position@1, limit@2, capacity@3, address@4, hb@5, offset@6.
// -------------------------------------------------------------------

/// Real-JDK HeapByteBuffer shape with a nonzero arrayOffset (a sliced
/// or duplicated view, e.g. a Netty pooled heap buffer): logical index
/// 0 lives at `hb[offset]`, so reads must add the offset.
#[test]
fn bb_view_heap_named_respects_array_offset() {
    let mut ctx = crate::test_utils::mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    for i in 0..16 {
        ctx.set_array_element(arr, i, Value::Int(i as i32));
    }
    let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/HeapByteBuffer", 8).unwrap();
    ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(bb, "position", Value::Int(1));
    ctx.set_field_by_name(bb, "limit", Value::Int(4));
    ctx.set_field_by_name(bb, "capacity", Value::Int(6));
    ctx.set_field_by_name(bb, "offset", Value::Int(10));

    let mut out = Vec::new();
    let n = bb_read_into(&mut ctx, bb, &mut out, 64);
    assert_eq!(n, 3);
    assert_eq!(
        out,
        vec![11, 12, 13],
        "must read hb[offset+pos..offset+lim]"
    );
    assert_eq!(
        ctx.get_field_by_name(bb, "position").as_int(),
        Some(4),
        "position must advance to limit"
    );

    // Write path honors the offset too.
    ctx.set_field_by_name(bb, "position", Value::Int(0));
    let put = bb_write_from(&mut ctx, bb, &[0x7f, 0x7e]);
    assert_eq!(put, 2);
    assert_eq!(ctx.get_array_element(arr, 10).as_int(), Some(0x7f));
    assert_eq!(ctx.get_array_element(arr, 11).as_int(), Some(0x7e));
}

/// Real-JDK DirectByteBuffer shape (Reactor-Netty's default): `hb` is
/// null and the bytes live in native memory at the `address` field.
/// Reads and writes must go through native memory.
#[test]
fn bb_view_direct_named_reads_and_writes_native_memory() {
    let mut ctx = crate::test_utils::mock_ctx();
    let mut native: Vec<u8> = (0u8..32).collect();
    let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/DirectByteBuffer", 8).unwrap();
    ctx.set_field_by_name(
        bb,
        "address",
        Value::Long(native.as_mut_ptr() as usize as i64),
    );
    ctx.set_field_by_name(bb, "position", Value::Int(2));
    ctx.set_field_by_name(bb, "limit", Value::Int(7));
    ctx.set_field_by_name(bb, "capacity", Value::Int(32));

    let mut out = Vec::new();
    let n = bb_read_into(&mut ctx, bb, &mut out, 64);
    assert_eq!(n, 5);
    assert_eq!(out, vec![2, 3, 4, 5, 6]);
    assert_eq!(
        ctx.get_field_by_name(bb, "position").as_int(),
        Some(7),
        "position must advance to limit"
    );

    // Write path: fill [7, 9) through the buffer.
    ctx.set_field_by_name(bb, "limit", Value::Int(32));
    let put = bb_write_from(&mut ctx, bb, &[0xAA, 0xBB]);
    assert_eq!(put, 2);
    assert_eq!(native[7], 0xAA);
    assert_eq!(native[8], 0xBB);
    assert_eq!(ctx.get_field_by_name(bb, "position").as_int(), Some(9));
}

/// Direct-backing accesses are clamped to capacity: a limit (or record
/// end) past the allocation is truncated, never read out of bounds.
#[test]
fn bb_view_direct_clamps_to_capacity() {
    let mut ctx = crate::test_utils::mock_ctx();
    let mut native: Vec<u8> = (10u8..18).collect(); // 8 bytes
    let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/DirectByteBuffer", 8).unwrap();
    ctx.set_field_by_name(
        bb,
        "address",
        Value::Long(native.as_mut_ptr() as usize as i64),
    );
    ctx.set_field_by_name(bb, "position", Value::Int(0));
    ctx.set_field_by_name(bb, "limit", Value::Int(64)); // lies past cap
    ctx.set_field_by_name(bb, "capacity", Value::Int(8));

    let v = bb_view(&mut ctx, bb);
    assert_eq!(v.lim, 8, "limit must clamp to capacity");
    let bytes = bb_bytes_range(&mut ctx, &v, 0, 100);
    assert_eq!(bytes.len(), 8, "range reads clamp to capacity");
    assert_eq!(bytes[0], 10);
    assert_eq!(bytes[7], 17);
    assert!(bb_get_byte(&mut ctx, &v, 8).is_none());
    // Writes clamp as well.
    assert_eq!(bb_put_bytes(&mut ctx, &v, 6, &[1, 2, 3, 4]), 2);
    assert_eq!(native[6], 1);
    assert_eq!(native[7], 2);
}

/// The pre-fix synthetic heap layout `[0]=array,[1]=pos,[2]=limit,
/// [3]=cap` must keep resolving (synthetic-jdk mode engines).
#[test]
fn bb_view_synthetic_heap_slot_fallback_still_resolves() {
    let mut ctx = crate::test_utils::mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    for i in 0..8 {
        ctx.set_array_element(arr, i, Value::Int((40 + i) as i32));
    }
    // A class OUTSIDE the mock's java/nio/*ByteBuffer named-field map,
    // so only slot-indexed reads can resolve it.
    let bb = try_alloc_concurrent_synthetic(&mut ctx, "javax/net/ssl/SyntheticBuf", 4).unwrap();
    ctx.set_field(bb, 0, Value::Object(Some(arr)));
    ctx.set_field(bb, 1, Value::Int(1)); // pos
    ctx.set_field(bb, 2, Value::Int(3)); // limit
    ctx.set_field(bb, 3, Value::Int(8)); // cap

    let mut out = Vec::new();
    let n = bb_read_into(&mut ctx, bb, &mut out, 64);
    assert_eq!(n, 2);
    assert_eq!(out, vec![41, 42]);
    assert_eq!(
        ctx.get_field(bb, 1).as_int(),
        Some(3),
        "slot-1 pos advanced"
    );
}

/// A heap-backed buffer whose `capacity` claims more room than its backing
/// array actually has must not report bytes it did not move.
///
/// The engine's heap arms used to copy one element at a time and let
/// `get_array_element`/`set_array_element`'s own guards drop whatever fell
/// past the array end — while still returning the full requested count. The
/// read side then padded the tail with zeros and handed them to rustls as
/// plaintext; the write side reported `data.len()` bytes produced into a
/// buffer that had only taken some of them. Both are silent corruption in
/// exactly the direction a caller cannot detect. The bulk intrinsics that
/// replaced those loops bounds-check the whole range up front, so the count
/// is now the truth. This test injects the overrun the old code hid.
#[test]
fn bb_heap_arms_report_only_the_bytes_that_fit() {
    let mut ctx = crate::test_utils::mock_ctx();
    // 4-byte array, but the buffer's metadata claims 16 bytes of capacity.
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    for i in 0..4 {
        ctx.set_array_element(arr, i, Value::Int((10 + i) as i32));
    }
    let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/nio/HeapByteBuffer", 8).unwrap();
    ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(bb, "position", Value::Int(0));
    ctx.set_field_by_name(bb, "limit", Value::Int(16));
    ctx.set_field_by_name(bb, "capacity", Value::Int(16));
    ctx.set_field_by_name(bb, "offset", Value::Int(0));

    let mut out = Vec::new();
    let n = bb_read_into(&mut ctx, bb, &mut out, 64);
    assert_eq!(n, 4, "only the 4 real bytes exist; 16 was the buffer's lie");
    assert_eq!(out, vec![10, 11, 12, 13], "no zero padding past the array");
    assert_eq!(
        ctx.get_field_by_name(bb, "position").as_int(),
        Some(4),
        "position advances by what was actually read, not by the claim"
    );

    // Write side: 8 bytes offered into 4 bytes of real room.
    ctx.set_field_by_name(bb, "position", Value::Int(0));
    let put = bb_write_from(&mut ctx, bb, &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(put, 4, "must report the 4 that landed, not all 8");
    assert_eq!(ctx.get_array_element(arr, 3).as_int(), Some(4));
    assert_eq!(
        ctx.get_field_by_name(bb, "position").as_int(),
        Some(4),
        "position advances by the bytes stored"
    );
}

/// A shape we cannot resolve must move zero bytes (and not panic).
#[test]
fn bb_view_unresolved_moves_zero_bytes() {
    let mut ctx = crate::test_utils::mock_ctx();
    let bb = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 3).unwrap();
    let mut out = Vec::new();
    assert_eq!(bb_read_into(&mut ctx, bb, &mut out, 64), 0);
    assert!(out.is_empty());
    assert_eq!(bb_write_from(&mut ctx, bb, &[1, 2, 3]), 0);
    let v = bb_view(&mut ctx, bb);
    assert!(matches!(v.backing, BbBacking::Unresolved));
}

#[test]
fn scoped_trust_roots_attach_to_one_ssl_context() {
    let ca_der = parse_cert_chain_pem(CA_CRT_PEM).unwrap()[0]
        .as_ref()
        .to_vec();
    set_selected_context_trust_roots(None);

    let ctx = fake_object_ref(1);
    let mut mock_ctx = crate::test_utils::mock_ctx();
    set_pending_tm_trust_roots(vec![ca_der.clone()]);
    // A supplied TrustManager[] that resolves no roots of its own keeps
    // the staged-roots fallback.
    attach_pending_identity_to_ctx(&mut mock_ctx, ctx, None, None, false, true).unwrap();

    assert!(ctx_identity(&mut mock_ctx, ctx).unwrap().is_none());
    let selected = selected_context_trust_roots().expect("context trust roots selected");
    assert_eq!(selected.root_ders, vec![ca_der]);
    let root_store = root_store_for_trust_roots(Some(&selected));
    assert_eq!(root_store.roots.len(), 1);

    let other_ctx = fake_object_ref(2);
    assert!(ctx_identity(&mut mock_ctx, other_ctx).unwrap().is_none());
    assert!(selected_context_trust_roots().is_none());
}

/// nio21 wave 13 (L4): JSSE's received-alert text, `(name) Received fatal
/// alert: name`, with its snake_case alert names.
#[test]
fn received_alert_message_is_jsse_wording() {
    use rustls::AlertDescription as A;
    assert_eq!(
        jsse_received_alert_message(A::InternalError),
        "(internal_error) Received fatal alert: internal_error"
    );
    assert_eq!(
        jsse_received_alert_message(A::UnknownCA),
        "(unknown_ca) Received fatal alert: unknown_ca"
    );
    assert_eq!(
        jsse_received_alert_message(A::UnrecognisedName),
        "(unrecognized_name) Received fatal alert: unrecognized_name"
    );
    assert_eq!(
        jsse_received_alert_message(A::CertificateUnknown),
        "(certificate_unknown) Received fatal alert: certificate_unknown"
    );
}

#[test]
fn trust_manager_unhandled_carries_the_internal_error_alert_prefix() {
    assert!(TRUST_MANAGER_UNHANDLED.starts_with(rustls::INTERNAL_ERROR_VERIFIER_PREFIX));
}

/// nio20 wave 5 (L5): `SSLContext.init(kms, null, sr)` is SunJSSE's
/// DEFAULT trust manager, never the roots a preceding `KeyStore.load`
/// staged on this thread; the staged roots are drained, and a previous
/// init's roots on the same context do not survive.
#[test]
fn null_trust_managers_do_not_inherit_staged_roots() {
    let ca_der = parse_cert_chain_pem(CA_CRT_PEM).unwrap()[0]
        .as_ref()
        .to_vec();
    set_selected_context_trust_roots(None);
    let ctx = fake_object_ref(0x7e05);
    let mut mock_ctx = crate::test_utils::mock_ctx();

    set_pending_tm_trust_roots(vec![ca_der.clone()]);
    attach_pending_identity_to_ctx(&mut mock_ctx, ctx, None, None, false, true).unwrap();
    assert!(ctx_identity(&mut mock_ctx, ctx).unwrap().is_none());
    assert!(selected_context_trust_roots().is_some());

    set_pending_tm_trust_roots(vec![ca_der]);
    attach_pending_identity_to_ctx(&mut mock_ctx, ctx, None, None, false, false).unwrap();
    assert!(
        take_pending_tm_trust_roots().is_none(),
        "staged roots drained"
    );
    assert!(ctx_identity(&mut mock_ctx, ctx).unwrap().is_none());
    assert!(
        selected_context_trust_roots().is_none(),
        "no named trust store: the default (platform) roots, not the staged ones"
    );
}

#[test]
fn legacy_extra_trust_registration_does_not_expand_global_roots() {
    let ca_der = parse_cert_chain_pem(CA_CRT_PEM).unwrap()[0]
        .as_ref()
        .to_vec();
    set_selected_context_trust_roots(None);

    add_extra_trust_root_der(ca_der);

    assert!(selected_context_trust_roots().is_none());
    assert!(trust_roots_pem(None).is_empty());
}

#[test]
fn pem_parsers_preserve_key_forms_and_reject_malformed_blocks() {
    for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        let pem =
            format!("{CA_CRT_PEM}\n-----BEGIN {label}-----\nAQID\n-----END {label}-----\n");
        let key = parse_private_key_pem(&pem).expect("all supported key encodings");
        assert_eq!(key.secret_der(), &[1, 2, 3]);
        assert!(matches!(
            (&key, label),
            (PrivateKeyDer::Pkcs8(_), "PRIVATE KEY")
                | (PrivateKeyDer::Pkcs1(_), "RSA PRIVATE KEY")
                | (PrivateKeyDer::Sec1(_), "EC PRIVATE KEY")
        ));
        assert_eq!(parse_cert_chain_pem(&pem).unwrap().len(), 1);
    }
    assert_eq!(
        parse_cert_chain_pem("").unwrap_err(),
        "no CERTIFICATE blocks found in PEM"
    );
    assert_eq!(
        parse_private_key_pem(CA_CRT_PEM).unwrap_err(),
        "no private key block found in PEM"
    );
    assert!(
        parse_cert_chain_pem("-----BEGIN CERTIFICATE-----\n!\n-----END CERTIFICATE-----")
            .is_err()
    );
    assert!(
        parse_private_key_pem("-----BEGIN PRIVATE KEY-----\n!\n-----END PRIVATE KEY-----")
            .is_err()
    );
}

#[test]
fn t27_parses_embedded_certs() {
    let chain = parse_cert_chain_pem(CA_CRT_PEM).expect("CA parses");
    assert_eq!(chain.len(), 1, "CA is a single cert");
    let _ = parse_cert_chain_pem(SERVER_CRT_PEM).expect("server cert parses");
    let _ = parse_private_key_pem(SERVER_KEY_PEM).expect("server key parses");
    let _ = parse_private_key_pem(CLIENT_KEY_PEM).expect("client key parses");
}

#[test]
fn t27_loopback_self_test() {
    // T2.7.16 / T2.7.17 — full in-process handshake + app-data exchange.
    let msg = run_loopback_self_test(SERVER_CRT_PEM, SERVER_KEY_PEM, Some(CA_CRT_PEM))
        .expect("self-test succeeds");
    assert!(msg.starts_with("OK "), "unexpected result: {}", msg);
    assert!(msg.contains("proto=TLSv1.3"));
    assert!(msg.contains("alpn=h2"));
    // The cipher substring was never asserted here, which is why the
    // rustls spelling survived in this string. It is now the ONLY
    // behavioural exercise of `suite_to_java_cipher_name` that needs no
    // network: a real in-process TLS 1.3 handshake, whose name must be
    // the one HotSpot reports. Measured on this host, HotSpot 25.0.3+9-LTS
    // answers `TLS_AES_256_GCM_SHA384` for a live TLS 1.3 session and lists
    // ZERO supported suites beginning `TLS13_`.
    assert!(
        msg.contains("cipher=TLS_"),
        "cipher must carry JSSE's spelling: {}",
        msg
    );
    assert!(
        !msg.contains("cipher=TLS13_"),
        "cipher is still in rustls's TLS 1.3 spelling, which JSSE never \
         produces -- `suite_to_java_cipher_name` was bypassed: {}",
        msg
    );
}

/// The family guard for the rustls-vs-JSSE cipher spelling.
///
/// The helper `http_url_connection::jsse_cipher_suite_name` existed, was
/// unit-tested in both directions, and had exactly ONE of the eight
/// producers of a rustls suite name as a caller — the other seven lived in
/// this file and each re-spelled the name by hand. Testing the FUNCTION is
/// what let that happen: nothing asserted that anyone CALLS it. This
/// asserts the call, by shape, over the working tree.
///
/// ZERO occurrences of the raw `{:?}`-on-a-suite idiom may exist in this
/// file. The translation is `suite_to_java_cipher_name`, which takes a
/// `rustls::CipherSuite` rather than a `Debug` string, so every producer —
/// including `engine_take_pending_trust_check`, whose value never reaches
/// Java — goes through it and the old two-exception carve-out is gone. A
/// new producer, or a rewrite of an adapted call site back to the raw
/// idiom, fails here.
///
/// Reads the working tree rather than `include_str!` — this repository is
/// edited from both Windows and Linux, so `\r` is normalised.
#[test]
fn the_only_rustls_suite_spelling_left_is_the_adapters_own() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("t27_tls.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("t27_tls.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();

    // Assembled from fragments so this test's own source does not contain
    // the needle it searches for.
    let needle = format!("{}{}{}", "format!(\"{:", "?}\", cs.", "suite())");

    let mut found: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        // Prose mentions the idiom in doc comments; only code counts.
        if t.starts_with("//") {
            continue;
        }
        if !line.contains(&needle) {
            continue;
        }
        let owner = lines[..=i]
            .iter()
            .rev()
            .find_map(|l| {
                let l = l.trim_start();
                for p in ["pub fn ", "pub fn ", "fn "] {
                    if let Some(rest) = l.strip_prefix(p) {
                        return Some(
                            rest.split(['(', '<', ' ']).next().unwrap_or("").to_string(),
                        );
                    }
                }
                None
            })
            .unwrap_or_else(|| format!("<no enclosing fn, line {}>", i + 1));
        found.push(owner);
    }
    found.sort();
    found.dedup();

    // Was two (`negotiated_suite_name`, the adapter, and
    // `engine_take_pending_trust_check`, whose value never reaches Java).
    // Both are now on the typed `suite_to_java_cipher_name`, which takes a
    // `rustls::CipherSuite` and cannot be handed a `Debug` string at all —
    // so the raw idiom must appear NOWHERE in this file. A producer that
    // re-introduces it fails here, exactly as before, on a stricter rule.
    let expected: Vec<String> = Vec::new();
    assert_eq!(
        found, expected,
        "the raw rustls suite spelling must appear NOWHERE in this file. \
         Found it in: {found:?}. Every producer feeds \
         `SSLSession.getCipherSuite()`, which is contracted to return the \
         IANA/JSSE name -- HotSpot 25 lists ZERO suites spelled `TLS13_`. \
         Use `suite_to_java_cipher_name`, which takes the suite itself."
    );
}

/// T2.7-SEC-1 — release-config server with no keystore must refuse to
/// start with an `IllegalStateException`, not panic and not hang. This
/// is the regression test for the HIGH-severity finding "embedded TLS
/// keys in release binary": once the embedded keys are gated behind
/// `cfg(test)`, the only way to start a server is via an explicitly
/// installed runtime identity, and the absence of one must surface as
/// a clean Java exception.
#[test]
fn t27_sec_no_keystore_raises_illegal_state() {
    let _guard = IdentityGuard::install_none();
    let mut r = NativeMethodRegistry::new();
    super::register_sslserversocket(&mut r);
    let handler = r
        .find(
            "javax/net/ssl/SSLServerSocketFactory",
            "createServerSocket",
            "(I)Ljava/net/ServerSocket;",
        )
        .expect("createServerSocket(I) registered");

    let mut ctx = crate::test_utils::mock_ctx();
    // Two args: the (synthetic) receiver and the port (0 = ephemeral).
    let args = [Value::Object(None), Value::Int(0)];
    let result = handler(&mut ctx, &args);
    let err = result.expect_err("must fail without a runtime identity");
    // The convention in this crate is to surface `RuntimeError` via
    // `Into<MethodCallFailed>`. We re-stringify the wrapped error and
    // assert on both the exception class and the message — that keeps
    // the test resilient to any future re-shaping of `MethodCallFailed`
    // while still proving the correct exception type reaches the JVM.
    let s = format!("{:?}", err);
    assert!(
        s.contains("IllegalStateException"),
        "expected IllegalStateException, got: {}",
        s
    );
    assert!(
        s.contains("No TLS key/cert configured"),
        "unexpected ISE message in: {}",
        s
    );
    assert!(
        s.contains("javax.net.ssl.keyStore"),
        "ISE message should mention javax.net.ssl.keyStore: {}",
        s
    );
}

/// T2.7-SEC-2 — companion to the above: the JVM-callable self-test
/// must also refuse (returning a structured `"ERR: ..."` string)
/// rather than reaching for any embedded test key when no keystore is
/// configured.
#[test]
fn t27_sec_self_test_with_no_identity_returns_err_string() {
    let _guard = IdentityGuard::install_none();
    let mut r = NativeMethodRegistry::new();
    super::register_self_test(&mut r);
    let handler = r
        .find("cratonvm/tls/T27SelfTest", "run", "()Ljava/lang/String;")
        .expect("self-test registered");
    let mut ctx = crate::test_utils::mock_ctx();
    let v = handler(&mut ctx, &[]).expect("native returns Ok");
    // Pull the Rust String back out so we can pattern-match. The mock
    // context's `create_string` stores the source in a heap slot that
    // we can read back via `string_to_rust`.
    let s_obj = match v {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected String object, got {:?}", other),
    };
    let s = ctx
        .read_string(s_obj)
        .expect("self-test result is a String");
    assert!(
        s.starts_with("ERR: "),
        "self-test should report an error when no keystore is configured: {}",
        s
    );
    assert!(
        s.contains("No TLS key/cert configured"),
        "self-test error should mention the missing keystore: {}",
        s
    );
}

#[test]
fn t27_mtls_loopback() {
    // T2.7.12 / T2.7.18 — require client cert, verify the handshake
    // only succeeds when the client presents one signed by our CA.
    let server_config = build_server_config_single_cert(
        SERVER_CRT_PEM,
        SERVER_KEY_PEM,
        &["h2"],
        true,
        Some(CA_CRT_PEM),
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let scfg = server_config.clone();
    let s = std::thread::spawn(move || -> Result<(), String> {
        let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
        let conn = ServerConnection::new(scfg).map_err(|e| e.to_string())?;
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .map_err(|e| e.to_string())?;
                stream
                    .conn
                    .process_new_packets()
                    .map_err(|e| e.to_string())?;
            }
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .map_err(|e| e.to_string())?;
            }
        }
        let mut buf = [0u8; 2];
        stream.read(&mut buf).map_err(|e| e.to_string())?;
        stream.write_all(b"ok").map_err(|e| e.to_string())?;
        stream.conn.send_close_notify();
        let _ = stream.flush();
        Ok(())
    });

    let mut roots = RootCertStore::empty();
    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
        roots.add(c).unwrap();
    }
    let client_config =
        build_client_config(roots, &["h2"], Some((CLIENT_CRT_PEM, CLIENT_KEY_PEM))).unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let sni = ServerName::try_from("localhost".to_string()).unwrap();
    let conn = ClientConnection::new(client_config, sni).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);
    while stream.conn.is_handshaking() {
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut EintrIo::new(&mut stream.sock))
                .unwrap();
        }
        if stream.conn.wants_read() {
            stream
                .conn
                .read_tls(&mut EintrIo::new(&mut stream.sock))
                .unwrap();
            stream.conn.process_new_packets().unwrap();
        }
    }
    stream.write_all(b"hi").unwrap();
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"ok");
    s.join().unwrap().unwrap();
}

#[test]
fn t27_sni_dispatch() {
    // T2.7.10 / T2.7.19 — multi-tenant: two cert/key pairs keyed by
    // hostname. A client requesting "foo.test" must receive server1's
    // cert; a client requesting "bar.test" must receive server2's. We
    // inspect the server-side leaf cert to confirm dispatch.
    let config = build_server_config_sni(
        &[
            ("foo.test", SERVER1_CRT_PEM, SERVER1_KEY_PEM),
            ("bar.test", SERVER2_CRT_PEM, SERVER2_KEY_PEM),
        ],
        &[],
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let scfg = config.clone();
    let s = std::thread::spawn(move || -> Result<(), String> {
        for _ in 0..2 {
            let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
            let conn = ServerConnection::new(scfg.clone()).map_err(|e| e.to_string())?;
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_read() {
                    stream
                        .conn
                        .read_tls(&mut EintrIo::new(&mut stream.sock))
                        .map_err(|e| e.to_string())?;
                    stream
                        .conn
                        .process_new_packets()
                        .map_err(|e| e.to_string())?;
                }
                if stream.conn.wants_write() {
                    stream
                        .conn
                        .write_tls(&mut EintrIo::new(&mut stream.sock))
                        .map_err(|e| e.to_string())?;
                }
            }
            stream.conn.send_close_notify();
            let _ = stream.flush();            }
        Ok(())
    });

    // Two clients, each asserting they receive the expected leaf cert.
    let mut roots = RootCertStore::empty();
    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
        roots.add(c).unwrap();
    }
    let cc = build_client_config(roots, &[], None).unwrap();

    let check = |sni_name: &str, expect_cn: &str| {
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let sni = ServerName::try_from(sni_name.to_string()).unwrap();
        let conn = ClientConnection::new(cc.clone(), sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
                stream.conn.process_new_packets().unwrap();
            }
        }
        let certs = stream.conn.peer_certificates().expect("peer cert present");
        let leaf = &certs[0];
        // Cheap contains-check against the embedded CN byte substring:
        // server1 cert has `CN=foo.test`, server2 has `CN=bar.test`.
        let bytes = leaf.as_ref();
        let needle = expect_cn.as_bytes();
        let found = bytes.windows(needle.len()).any(|w| w == needle);
        assert!(
            found,
            "SNI {} did not get leaf containing {}",
            sni_name, expect_cn
        );
        stream.conn.send_close_notify();
        let _ = stream.flush();
    };
    check("foo.test", "foo.test");
    check("bar.test", "bar.test");
    s.join().unwrap().unwrap();
}

#[test]
fn t27_trust_store_loads() {
    // T2.7.4 — native trust store must be loadable on the host, even if
    // empty. This test asserts the call does not error out; the returned
    // root count is platform-dependent.
    let _ = load_native_root_store();
    let _ = accepted_issuer_ders();
}

/// T2.7.16 — connect to https://www.google.com/ via rustls using real
/// system roots, send a minimal HTTP/1.1 GET, and verify we receive an
/// HTTP response with a 2xx or 3xx status. Requires network; marked
/// `#[ignore]` so the default `cargo test` run stays offline.
#[test]
#[ignore]
fn t27_google_com_https() {
    let roots = load_native_root_store().expect("system roots load");
    // Only offer http/1.1 — we send a plaintext HTTP/1.1 request, and
    // offering h2 would cause the server to speak HTTP/2 binary framing.
    let config = build_client_config(roots, &["http/1.1"], None).expect("client config");
    let tcp = TcpStream::connect("www.google.com:443").expect("TCP connect");
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    tcp.set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    let sni = ServerName::try_from("www.google.com".to_string()).unwrap();
    let conn = ClientConnection::new(config, sni).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);
    // Drive handshake.
    while stream.conn.is_handshaking() {
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut EintrIo::new(&mut stream.sock))
                .unwrap();
        }
        if stream.conn.wants_read() {
            stream
                .conn
                .read_tls(&mut EintrIo::new(&mut stream.sock))
                .unwrap();
            stream.conn.process_new_packets().unwrap();
        }
    }
    // Verify negotiated protocol is TLS 1.2 or 1.3.
    let proto = stream.conn.protocol_version().expect("protocol version");
    assert!(
        proto == rustls::ProtocolVersion::TLSv1_3 || proto == rustls::ProtocolVersion::TLSv1_2,
        "unexpected protocol: {:?}",
        proto,
    );
    // Send a minimal HTTP/1.1 GET.
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: www.google.com\r\nConnection: close\r\n\r\n")
        .unwrap();
    // Read the status line.
    let mut response = vec![0u8; 4096];
    let n = stream.read(&mut response).expect("read");
    assert!(n > 0, "zero bytes read");
    let header = String::from_utf8_lossy(&response[..n.min(256)]);
    assert!(
        header.starts_with("HTTP/1.1 2") || header.starts_with("HTTP/1.1 3"),
        "unexpected status line: {}",
        &header[..header.len().min(80)],
    );
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

/// T2.7.13 — session resumption: two sequential handshakes to the same
/// server must resume, demonstrating that rustls's TLS 1.3 ticket cache is
/// retained by the shared client configuration.
#[test]
fn t27_session_resumption() {
    let server_config =
        build_server_config_single_cert(SERVER_CRT_PEM, SERVER_KEY_PEM, &[], false, None)
            .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let scfg = server_config.clone();

    // Server thread accepts 2 connections.
    let s = std::thread::spawn(move || -> Result<(), String> {
        for _ in 0..2 {
            let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
            let conn = ServerConnection::new(scfg.clone()).map_err(|e| e.to_string())?;
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_read() {
                    stream
                        .conn
                        .read_tls(&mut EintrIo::new(&mut stream.sock))
                        .map_err(|e| e.to_string())?;
                    stream
                        .conn
                        .process_new_packets()
                        .map_err(|e| e.to_string())?;
                }
                if stream.conn.wants_write() {
                    stream
                        .conn
                        .write_tls(&mut EintrIo::new(&mut stream.sock))
                        .map_err(|e| e.to_string())?;
                }
            }
            stream.conn.send_close_notify();
            let _ = stream.flush();            }
        Ok(())
    });

    let mut roots = RootCertStore::empty();
    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
        roots.add(c).unwrap();
    }
    let config = build_client_config(roots, &[], None).unwrap();

    // First connection.
    {
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let sni = ServerName::try_from("localhost".to_string()).unwrap();
        let conn = ClientConnection::new(config.clone(), sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
                stream.conn.process_new_packets().unwrap();
            }
        }
        assert!(
            stream.conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
            "first handshake should be TLSv1.3"
        );
        // The server sends TLS 1.3's NewSessionTicket after the handshake.
        // Drain through its close_notify so rustls stores that ticket on
        // the shared ClientConfig before the next connection is created.
        let mut eof = [0u8; 1];
        assert_eq!(
            stream.read(&mut eof).unwrap(),
            0,
            "first connection should close after delivering its ticket"
        );
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }
    // Second connection must use the ticket received on the first.
    {
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let sni = ServerName::try_from("localhost".to_string()).unwrap();
        let conn = ClientConnection::new(config.clone(), sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .unwrap();
                stream.conn.process_new_packets().unwrap();
            }
        }
        assert!(
            stream.conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
            "second (resumed) handshake should be TLSv1.3"
        );
        assert_eq!(
            stream.conn.handshake_kind(),
            Some(rustls::HandshakeKind::Resumed),
            "second handshake must use the ticket from the first connection"
        );
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }
    s.join().unwrap().unwrap();
}

// -------------------------------------------------------------------------
// WP5.1 / WP5.4 engine + ALPN registration smoke tests
// -------------------------------------------------------------------------

#[test]
fn wp51_register_sslengine_real_registers_impl_methods() {
    let mut r = NativeMethodRegistry::new();
    super::register_sslengine_real(&mut r);
    let cls = "sun/security/ssl/SSLEngineImpl";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "setUseClientMode", "(Z)V").is_some());
    assert!(r.find(cls, "getUseClientMode", "()Z").is_some());
    assert!(r.find(cls, "setNeedClientAuth", "(Z)V").is_some());
    assert!(r.find(cls, "setWantClientAuth", "(Z)V").is_some());
    assert!(r.find(cls, "beginHandshake", "()V").is_some());
    // JSSE's delegated-task contract. `getDelegatedTask` was unregistered
    // until 2026-08-16, so a caller that followed the NEED_TASK this
    // engine can report had no way to satisfy it. See `DelegatedTask`.
    assert!(r
        .find(cls, "getDelegatedTask", "()Ljava/lang/Runnable;")
        .is_some());
    assert!(r.find("java/lang/Runnable", "run", "()V").is_some());
    assert!(r
        .find(
            cls,
            "wrap",
            "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "wrap",
            "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "wrap",
            "([Ljava/nio/ByteBuffer;IILjava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "unwrap",
            "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "unwrap",
            "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "unwrap",
            "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;II)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getHandshakeStatus",
            "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"
        )
        .is_some());
    assert!(r.find(cls, "closeOutbound", "()V").is_some());
    assert!(r.find(cls, "closeInbound", "()V").is_some());
    assert!(r.find(cls, "isInboundDone", "()Z").is_some());
    assert!(r.find(cls, "isOutboundDone", "()Z").is_some());
    assert!(r
        .find(cls, "getSession", "()Ljavax/net/ssl/SSLSession;")
        .is_some());
    assert!(r
        .find(cls, "getApplicationProtocol", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "setEnabledProtocols", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getEnabledProtocols", "()[Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setEnabledCipherSuites", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getEnabledCipherSuites", "()[Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
        .is_some());
    assert!(r
        .find(cls, "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
        .is_some());

    // TC0622: the SSLSession accessors must also be registered in the
    // real-mode path (they previously lived only in the synthetic-jdk-gated
    // register_p68_ssl, so SSLEngine.getSession().getApplicationBufferSize()
    // threw AbstractMethodError and killed Tomcat's NioEndpoint processor).
    let sess = "javax/net/ssl/SSLSession";
    assert!(r.find(sess, "getApplicationBufferSize", "()I").is_some());
    assert!(r.find(sess, "getPacketBufferSize", "()I").is_some());
    assert!(r
        .find(sess, "getProtocol", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(sess, "getCipherSuite", "()Ljava/lang/String;")
        .is_some());
    assert!(r.find(sess, "isValid", "()Z").is_some());
}

#[test]
fn wp54_register_alpn_real_registers_parameters_alpn() {
    let mut r = NativeMethodRegistry::new();
    super::register_alpn_real(&mut r);
    let cls = "javax/net/ssl/SSLParameters";
    assert!(r
        .find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getApplicationProtocols", "()[Ljava/lang/String;")
        .is_some());
}

#[test]
fn a_delegated_task_is_owed_once_and_handed_over_once() {
    // The state machine behind JSSE's NEED_TASK contract. See
    // `DelegatedTask` for what each transition is load-bearing for.
    let id = super::engine_alloc_id();
    super::engine_insert(id, super::EngineState::default());

    // First need: deferred, and a Runnable is now owed.
    assert_eq!(super::engine_begin_or_defer(id), Ok(true));
    super::with_engine(id, |s| {
        assert_eq!(s.delegated_task, super::DelegatedTask::Owed);
        assert!(s.task_unclaimed);
    });

    // Collected exactly once — netty's in-line `runDelegatedTasks` loop
    // terminates on the second, null answer and would otherwise spin.
    assert!(super::claim_delegated_task(id));
    assert!(!super::claim_delegated_task(id));
    super::with_engine(id, |s| {
        assert_eq!(s.delegated_task, super::DelegatedTask::HandedOut)
    });

    // Handed out and not yet run: the engine makes no progress, however
    // many times it is asked. This refusal is the whole feature — it is
    // what `test{Client,Server}HandshakeTimeoutBecauseExecutorNotExecute`
    // measure, and it has to survive a retry loop.
    assert_eq!(super::engine_begin_or_defer(id), Ok(true));
    assert_eq!(super::engine_begin_or_defer(id), Ok(true));

    super::engine_registry().write().remove(&id);
}

#[test]
fn a_task_done_inline_is_still_handed_over_when_the_caller_asks() {
    // Rule 2 on `DelegatedTask`: netty's `SslTasksRunner.run()` returns
    // WITHOUT calling `runComplete()` when `getDelegatedTask()` answers
    // null, so `SslHandler` stays in STATE_PROCESS_TASK — where `decode()`
    // and `flush()` are both no-ops — and the connection is wedged for
    // good. Measured: `getDelegatedTask id=3 hand_out=false` on the
    // executor thread, immediately after the event-loop thread had done
    // the work inline, was `testHandshakeWithExecutorJDK`'s failure.
    //
    // A promise made must therefore be answered even when the work was
    // meanwhile done by somebody else.
    let id = super::engine_alloc_id();
    let mut st = super::EngineState::default();
    // Exactly what the `Owed` arm of `engine_begin_or_defer` leaves
    // behind when it self-heals: the work is done, the promise is not.
    st.delegated_task = super::DelegatedTask::None;
    st.delegated_task_armed = true;
    st.task_unclaimed = true;
    super::engine_insert(id, st);

    assert!(
        super::claim_delegated_task(id),
        "a caller told NEED_TASK must never be answered with null"
    );
    assert!(!super::claim_delegated_task(id));

    super::engine_registry().write().remove(&id);
}

#[test]
fn wp51_engine_id_alloc_is_unique() {
    let a = super::engine_alloc_id();
    let b = super::engine_alloc_id();
    assert_ne!(a, b);
    assert_ne!(a, 0);
    assert_ne!(b, 0);
}

#[test]
fn wp51_engine_negotiated_alpn_unknown_returns_none() {
    // A fresh id (well above any previously allocated) should resolve to
    // None — the registry never sees this handle.
    let alpn = super::engine_negotiated_alpn_internal(i32::MAX - 1);
    assert!(alpn.is_none());
}

#[test]
fn wp51_engine_state_default_starts_as_client() {
    let s = super::EngineState::default();
    assert!(s.is_client);
    assert!(!s.closed_inbound);
    assert!(!s.closed_outbound);
    assert!(s.alpn_protocols.is_empty());
    assert!(s.enabled_protocols.contains(&"TLSv1.3".to_string()));
    assert!(s.enabled_protocols.contains(&"TLSv1.2".to_string()));
}

#[test]
fn wp51_handshake_status_no_conn_is_not_handshaking() {
    let s = super::EngineState::default();
    assert_eq!(super::handshake_status_of(&s), super::HS_NOT_HANDSHAKING_R);
}

#[test]
fn wp51_default_engine_client_config_with_alpn() {
    // Smoke: building a default client config with ALPN should succeed even
    // when native trust roots are unavailable (returns empty store).
    let alpn: Vec<Vec<u8>> = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let cfg = super::default_engine_client_config(&alpn);
    assert!(cfg.is_ok());
    let cfg = cfg.unwrap();
    assert_eq!(cfg.alpn_protocols, alpn);
}

#[test]
fn wp51_default_engine_server_config_uses_runtime_identity() {
    // Server config builds from the runtime-configured TLS identity
    // (here the test fixtures, installed via `install_test_identity`)
    // plus the ALPN list. Asserts that the gating of the embedded
    // keys behind `cfg(test)` did not break the engine code path.
    let _guard = install_test_identity();
    let alpn: Vec<Vec<u8>> = vec![b"h2".to_vec()];
    let cfg = super::default_engine_server_config(IDENTITY_TEST_VM, &alpn, false);
    assert!(
        cfg.is_ok(),
        "expected Ok with identity installed: {:?}",
        cfg.err()
    );
    let cfg = cfg.unwrap();
    assert_eq!(cfg.alpn_protocols, alpn);
}

#[test]
fn wp51_default_engine_server_config_no_identity_is_err() {
    // Companion to the above: with no runtime identity installed,
    // the engine-side default config builder must fail cleanly rather
    // than reaching for any embedded key.
    let _guard = IdentityGuard::install_none();
    let alpn: Vec<Vec<u8>> = vec![b"h2".to_vec()];
    let cfg = super::default_engine_server_config(IDENTITY_TEST_VM, &alpn, false);
    let err = cfg.err().expect("must fail without runtime identity");
    assert!(
        err.contains("No TLS key/cert configured"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn wp51_engine_close_outbound_sets_state() {
    // Round-trip: alloc id, mark closed, observe via with_engine.
    let id = super::engine_alloc_id();
    super::engine_insert(id, super::EngineState::default());
    super::with_engine(id, |s| {
        s.closed_outbound = true;
    });
    let v = super::with_engine(id, |s| s.closed_outbound).unwrap_or(false);
    assert!(v);
    // Cleanup so the registry doesn't grow forever between tests.
    super::engine_registry().write().remove(&id);
}

#[test]
fn wp54_engine_alpn_round_trip_via_set() {
    let id = super::engine_alloc_id();
    super::engine_insert(id, super::EngineState::default());
    super::with_engine(id, |s| {
        s.alpn_protocols = vec![b"h2".to_vec()];
        s.negotiated_alpn = Some("h2".to_string());
    });
    assert_eq!(
        super::engine_negotiated_alpn_internal(id),
        Some("h2".to_string())
    );
    super::engine_registry().write().remove(&id);
}

#[test]
fn wp51_loopback_handshake_via_engine_state() {
    // End-to-end: drive a handshake purely through EngineState pumps,
    // wrapping/unwrapping records between a client and server engine.
    // Asserts that rustls's wrap output is consumed by unwrap and that
    // both sides reach is_handshaking() == false.
    let mut client = super::EngineState::default();
    let mut server = super::EngineState::default();
    client.is_client = true;
    client.peer_host = Some("localhost".to_string());
    client.alpn_protocols = vec![b"h2".to_vec()];
    client.client_config = Some(
        super::build_client_config(
            {
                let mut roots = RootCertStore::empty();
                for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                    roots.add(c).unwrap();
                }
                roots
            },
            &["h2"],
            None,
        )
        .unwrap(),
    );
    server.is_client = false;
    server.alpn_protocols = vec![b"h2".to_vec()];
    server.server_config = Some(
        super::build_server_config_single_cert(
            SERVER_CRT_PEM,
            SERVER_KEY_PEM,
            &["h2"],
            false,
            None,
        )
        .unwrap(),
    );

    super::engine_begin(&mut client).expect("client begin");
    super::engine_begin(&mut server).expect("server begin");

    // 32 round-trips is plenty for TLS 1.3 (typically ~3 flights).
    for _ in 0..32 {
        // Client wrap
        let (_c_in, _c_out) = super::engine_wrap_pump(&mut client, &[], 65536);
        let buf = std::mem::take(&mut client.outbound);
        if !buf.is_empty() {
            let _ = super::engine_unwrap_pump(&mut server, &buf);
        }
        // Server wrap
        let (_s_in, _s_out) = super::engine_wrap_pump(&mut server, &[], 65536);
        let buf2 = std::mem::take(&mut server.outbound);
        if !buf2.is_empty() {
            let _ = super::engine_unwrap_pump(&mut client, &buf2);
        }
        super::engine_capture_negotiation(&mut client);
        super::engine_capture_negotiation(&mut server);
        let c_done = client
            .conn
            .as_ref()
            .map(|c| !c.is_handshaking())
            .unwrap_or(false);
        let s_done = server
            .conn
            .as_ref()
            .map(|c| !c.is_handshaking())
            .unwrap_or(false);
        if c_done && s_done {
            break;
        }
    }
    let c_done = client
        .conn
        .as_ref()
        .map(|c| !c.is_handshaking())
        .unwrap_or(false);
    let s_done = server
        .conn
        .as_ref()
        .map(|c| !c.is_handshaking())
        .unwrap_or(false);
    assert!(c_done, "client handshake should complete");
    assert!(s_done, "server handshake should complete");
    assert_eq!(client.negotiated_alpn.as_deref(), Some("h2"));
    assert_eq!(server.negotiated_alpn.as_deref(), Some("h2"));
}

/// `ConnCheckout` must put the connection back on EVERY exit, including the
/// early `return Err(...)` the record loop takes on a `process_new_packets`
/// failure. A missed restore does not fail a test — it leaves `conn == None`
/// for the life of that engine, so every later `wrap`/`unwrap` silently does
/// nothing and the connection just stops.
#[test]
fn a_checked_out_connection_is_restored_on_every_exit() {
    let id = super::engine_alloc_id();
    {
        let mut st = super::EngineState::default();
        st.is_client = true;
        st.peer_host = Some("localhost".to_string());
        st.client_config = Some(
            super::build_client_config(
                {
                    let mut roots = RootCertStore::empty();
                    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                        roots.add(c).unwrap();
                    }
                    roots
                },
                &["h2"],
                None,
            )
            .unwrap(),
        );
        super::engine_begin(&mut st).expect("begin");
        assert!(st.conn.is_some());
        super::engine_insert(id, st);
    }

    // Normal scope exit.
    {
        let checkout = super::ConnCheckout::take(id);
        assert!(checkout.conn.is_some(), "the connection must come out");
        assert!(
            super::with_engine(id, |s| s.conn.is_none() && s.conn_checked_out).unwrap(),
            "while on loan the engine must report `conn_checked_out`, not just an absent conn"
        );
        // `engine_begin` must refuse to build a rival connection in this
        // window — the restore below would silently discard it.
        super::with_engine(id, |s| {
            super::engine_begin(s).expect("begin during checkout");
            assert!(
                s.conn.is_none(),
                "engine_begin built a second connection while one was on loan"
            );
        });
    }
    assert!(
        super::with_engine(id, |s| s.conn.is_some() && !s.conn_checked_out).unwrap(),
        "the connection must be back after a normal scope exit"
    );

    // Early-return exit, the shape the record loop's `return Err(...)` takes.
    fn bails_out(id: i32) -> Result<(), ()> {
        let _checkout = super::ConnCheckout::take(id);
        Err(())
    }
    assert!(bails_out(id).is_err());
    assert!(
        super::with_engine(id, |s| s.conn.is_some() && !s.conn_checked_out).unwrap(),
        "the connection must be back after an early return"
    );

    // A `?`-style exit out of a nested scope, and a panic-driven unwind.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _checkout = super::ConnCheckout::take(id);
        panic!("simulated fault inside the record loop");
    }));
    assert!(unwound.is_err());
    assert!(
        super::with_engine(id, |s| s.conn.is_some() && !s.conn_checked_out).unwrap(),
        "the connection must be back after an unwind"
    );

    super::engine_registry().write().remove(&id);
}

/// A `ServerCertVerifier` that refuses every chain, the way a Java
/// `X509TrustManager` throwing `CertificateException` would if its verdict
/// reached rustls at verification time instead of after the handshake.
#[derive(Debug)]
struct AlwaysRejectVerifier {
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
    /// `false` makes this the ACCEPTING control arm — the verdict the
    /// deferred design effectively gives rustls today (it always accepts at
    /// verification time and consults Java afterwards). The control is what
    /// keeps the assertions below from being vacuous.
    reject: bool,
}

impl rustls::client::danger::ServerCertVerifier for AlwaysRejectVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if !self.reject {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        ))
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature_lenient(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature_lenient(message, cert, dss, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// **The destination for the deferred-trust-check refactor, proven before the
/// refactor.**
///
/// `testHandshakeFailureOnlyFireExceptionOnce` (`SslHandlerTest:1546`) asserts
/// the SERVER's handshake future fails when the CLIENT's `TrustManager`
/// rejects the chain. Today it cannot: the trust check is armed by
/// `engine_take_pending_trust_check`, which fires only once
/// `!conn.is_handshaking()` -- by construction AFTER the client's `Finished`
/// has gone out. The server therefore completes a valid TLS 1.3 handshake,
/// netty runs `setHandshakeSuccess()`, and the alert arriving a moment later
/// cannot fail an already-completed promise.
///
/// This asserts the property the current design cannot deliver and a
/// verifier-time verdict can: with the rejection raised INSIDE
/// `verify_server_cert`, the server receives an alert it can DECRYPT while
/// `is_handshaking()` is still true -- i.e. under handshake keys, not under
/// the application keys the cheap shortcut would need. (That shortcut was
/// measured and does not work: discarding rustls's queued flight before
/// queueing the alert desynchronises the TLS 1.3 key schedule and the server
/// reads `DecryptError` instead of the alert.)
///
/// If this ever starts failing, section B of
/// the openssl-key-material-and-engine-residuals write-up (now retired)
/// has lost its destination and the plan needs rethinking before any more of
/// it is built.
/// Drive a client/server `EngineState` pair whose client verifier either
/// rejects or accepts, and report what the SERVER observed:
/// `(saw_error_while_handshaking, error_text, still_handshaking_at_end)`.
fn drive_pair_with_client_verifier(reject: bool) -> (bool, String, bool) {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let verifier: std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier> =
        std::sync::Arc::new(AlwaysRejectVerifier {
            algorithms: provider.signature_verification_algorithms.clone(),
            reject,
        });
    let mut client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    client_config.alpn_protocols = vec![b"h2".to_vec()];

    let mut client = super::EngineState::default();
    client.is_client = true;
    client.peer_host = Some("localhost".to_string());
    client.alpn_protocols = vec![b"h2".to_vec()];
    client.client_config = Some(std::sync::Arc::new(client_config));

    let mut server = super::EngineState::default();
    server.is_client = false;
    server.alpn_protocols = vec![b"h2".to_vec()];
    server.server_config = Some(
        super::build_server_config_single_cert(
            SERVER_CRT_PEM,
            SERVER_KEY_PEM,
            &["h2"],
            false,
            None,
        )
        .unwrap(),
    );

    super::engine_begin(&mut client).expect("client begin");
    super::engine_begin(&mut server).expect("server begin");

    // The client's rejection surfaces out of its own `unwrap` of the server
    // flight; what matters is what the SERVER can then read.
    let mut server_saw_alert_while_handshaking = false;
    let mut server_error: Option<String> = None;
    for _ in 0..32 {
        let _ = super::engine_wrap_pump(&mut client, &[], 65536);
        let to_server = std::mem::take(&mut client.outbound);
        if !to_server.is_empty() {
            let was_handshaking = server
                .conn
                .as_ref()
                .map(|c| c.is_handshaking())
                .unwrap_or(false);
            match super::engine_unwrap_pump(&mut server, &to_server) {
                Ok(_) => {}
                Err(e) => {
                    // The alert decrypted and rustls reported it. That is the
                    // whole property: it arrived under handshake keys, so the
                    // server learns of the failure before it could complete.
                    if was_handshaking {
                        server_saw_alert_while_handshaking = true;
                    }
                    server_error = Some(format!("{e:?}"));
                    break;
                }
            }
        }
        let _ = super::engine_wrap_pump(&mut server, &[], 65536);
        let to_client = std::mem::take(&mut server.outbound);
        if !to_client.is_empty() {
            // The client's own unwrap is where its verifier runs and where it
            // raises; that error is not what this test is about.
            let _ = super::engine_unwrap_pump(&mut client, &to_client);
        }
    }

    let still_handshaking = server
        .conn
        .as_ref()
        .map(|c| c.is_handshaking())
        .unwrap_or(false);
    (
        server_saw_alert_while_handshaking,
        server_error.unwrap_or_default(),
        still_handshaking,
    )
}

#[test]
fn a_verifier_time_rejection_reaches_the_server_while_it_is_still_handshaking() {
    // CONTROL first: the same driver with an ACCEPTING verifier -- which is
    // what the deferred design gives rustls today -- must NOT produce any of
    // the three signals. Without this arm the assertions below would pass on
    // a driver that simply never completed a handshake at all.
    let (accept_saw, accept_err, accept_handshaking) = drive_pair_with_client_verifier(false);
    assert!(
        !accept_saw,
        "control: an accepted chain must not make the server see an error, got {accept_err}"
    );
    assert!(
        !accept_handshaking,
        "control: with the chain accepted the server must COMPLETE its handshake -- \
         if it does not, the driver is broken and the reject arm proves nothing"
    );

    let (saw, err, still_handshaking) = drive_pair_with_client_verifier(true);
    assert!(
        saw,
        "the server must see the client rejection while still handshaking; server_error={err}"
    );
    // And it must NOT be a decrypt failure: a `DecryptError` here would mean
    // the alert went out under the wrong keys, which is exactly what the
    // discard-the-queued-flight shortcut produced.
    assert!(
        !err.contains("DecryptError"),
        "the alert must be decryptable under handshake keys, got {err}"
    );
    assert!(
        still_handshaking,
        "the server must still be handshaking, not completed"
    );
}

/// Drive a client/server `EngineState` pair through a complete loopback
/// handshake (same shape as `wp51_loopback_handshake_via_engine_state`) and
/// return them, with the client's captured peer chain populated.
fn handshaked_pair() -> (super::EngineState, super::EngineState) {
    let mut client = super::EngineState::default();
    let mut server = super::EngineState::default();
    client.is_client = true;
    client.peer_host = Some("localhost".to_string());
    client.client_config = Some(
        super::build_client_config(
            {
                let mut roots = RootCertStore::empty();
                for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                    roots.add(c).unwrap();
                }
                roots
            },
            &[],
            None,
        )
        .unwrap(),
    );
    server.is_client = false;
    server.server_config = Some(
        super::build_server_config_single_cert(
            SERVER_CRT_PEM,
            SERVER_KEY_PEM,
            &[],
            false,
            None,
        )
        .unwrap(),
    );
    super::engine_begin(&mut client).expect("client begin");
    super::engine_begin(&mut server).expect("server begin");
    for _ in 0..32 {
        let _ = super::engine_wrap_pump(&mut client, &[], 65536);
        let buf = std::mem::take(&mut client.outbound);
        if !buf.is_empty() {
            let _ = super::engine_unwrap_pump(&mut server, &buf);
        }
        let _ = super::engine_wrap_pump(&mut server, &[], 65536);
        let buf2 = std::mem::take(&mut server.outbound);
        if !buf2.is_empty() {
            let _ = super::engine_unwrap_pump(&mut client, &buf2);
        }
        super::engine_capture_negotiation(&mut client);
        super::engine_capture_negotiation(&mut server);
        let done = |s: &super::EngineState| {
            s.conn
                .as_ref()
                .map(|c| !c.is_handshaking())
                .unwrap_or(false)
        };
        if done(&client) && done(&server) {
            break;
        }
    }
    (client, server)
}

#[test]
fn endpoint_alg_only_https_and_ldaps_verify_identity() {
    assert!(super::endpoint_alg_verifies_identity("HTTPS"));
    assert!(super::endpoint_alg_verifies_identity("https"));
    assert!(super::endpoint_alg_verifies_identity("LDAPS"));
    // Not an identification algorithm JSSE knows: treat as "no check"
    // rather than inventing one, so we never reject what JSSE accepts.
    assert!(!super::endpoint_alg_verifies_identity(""));
    assert!(!super::endpoint_alg_verifies_identity("NONE"));
}

/// The engine id these trust-check tests pass through.
///
/// `handshaked_pair()` builds `EngineState`s directly instead of
/// registering engines, so there is no registry id to quote — and
/// `engine_take_pending_trust_check` only copies the value into
/// `PendingTrustCheck::engine_id`, so any stable value serves.
const TEST_ENGINE_ID: i32 = 0;

/// REGRESSION (`TestSecurity2018.testCVE_2018_8034`): a client engine
/// configured with `setEndpointIdentificationAlgorithm("HTTPS")` must
/// refuse a certificate that does not name the host it dialled — even
/// though the chain itself is perfectly trusted and even though the
/// application's own `TrustManager` accepts everything.
#[test]
fn endpoint_identity_is_pending_and_rejects_a_mismatched_host() {
    let (mut client, _server) = handshaked_pair();
    assert!(
        !client.peer_cert_chain_der.is_empty(),
        "client must have captured the server chain"
    );

    // 1. No algorithm configured and no TrustManager: nothing pending —
    //    exactly as cheap as before this fix.
    client.trust_check_done = false;
    client.endpoint_id_alg = None;
    assert!(super::engine_take_pending_trust_check(TEST_ENGINE_ID, &mut client).is_none());

    // 2. HTTPS configured: a pending check appears even with no
    //    TrustManager attached, because JSSE's own default manager is what
    //    performs identification.
    client.trust_check_done = false;
    client.endpoint_id_alg = Some("HTTPS".to_string());
    client.peer_host = Some("localhost".to_string());
    let pending = super::engine_take_pending_trust_check(TEST_ENGINE_ID, &mut client)
        .expect("identity check pending");
    assert!(pending.trust_ctx_key.is_none());
    // The engine id is carried through so the deferred half
    // (`engine_run_trust_check`, which runs after the registry lock is
    // dropped) can find its engine again. Nothing asserted it, which is
    // why adding the parameter broke three call sites and no test.
    assert_eq!(pending.engine_id, TEST_ENGINE_ID);
    assert_eq!(
        pending.endpoint_identity,
        Some(("HTTPS".to_string(), "localhost".to_string()))
    );
    // The host the engine actually dialled matches the leaf: accepted.
    assert!(
        crate::x509_manager::check_endpoint_identity(&pending.peer_chain_der, "localhost")
            .is_ok()
    );

    // 3. The CVE shape: same trusted chain, different host. The in-tree
    //    test leaf names localhost/foo.test/bar.test/127.0.0.1, so a host
    //    outside that set must be refused.
    assert!(
        crate::x509_manager::check_endpoint_identity(&pending.peer_chain_der, "evil.test")
            .is_err(),
        "a certificate that does not name the dialled host must be refused"
    );

    // 4. A server engine never runs client-side identification, whatever
    //    is configured on it.
    client.trust_check_done = false;
    client.is_client = false;
    let pending = super::engine_take_pending_trust_check(TEST_ENGINE_ID, &mut client);
    assert!(
        pending.is_none(),
        "server engines do not identify endpoints"
    );
}

/// REGRESSION (`NettyReactiveWebServerFactoryTests.whenSslBundleIsUpdatedThenSslIsReloaded`):
/// which TrustManager is in force decides WHO identifies the endpoint, and
/// getting that wrong rejects a connection a real JDK accepts.
///
/// The three cases are the three branches of `SSLContextImpl.chooseTrustManager`,
/// and they must not collapse into each other: an empty array is JSSE's own
/// default manager (identifies), a plain `X509TrustManager` is wrapped by
/// `AbstractTrustManagerWrapper` (JSSE identifies AFTER it — the
/// CVE-2018-8034 case Tomcat's `TesterSupport.TrustAllCerts` exercises),
/// and an `X509ExtendedTrustManager` is used as-is (JSSE adds nothing, and
/// Netty's `X509TrustManagerWrapper` is one).
#[test]
fn an_extended_trust_manager_owns_endpoint_identification() {
    let mut ctx = crate::test_utils::mock_ctx();
    let extended = ctx
        .ensure_class_initialized("javax/net/ssl/X509ExtendedTrustManager")
        .expect("mock class");
    let tm_iface = ctx
        .ensure_class_initialized("javax/net/ssl/X509TrustManager")
        .expect("mock class");
    ctx.set_superclass(extended, tm_iface);

    // 1. No application TrustManager: JSSE's own default identifies.
    assert!(
        super::jsse_owns_endpoint_identification(&mut ctx, &[]),
        "with no application manager, JSSE's default X509TrustManagerImpl identifies"
    );

    // 2. A PLAIN X509TrustManager — the accept-everything test shape.
    //    JSSE wraps it and still identifies, so this must stay strict or
    //    `TestSecurity2018.testCVE_2018_8034` regresses.
    let plain_cls = ctx
        .ensure_class_initialized("org/example/TrustAllCerts")
        .expect("mock class");
    let plain = ctx.alloc_object(plain_cls, 1);
    assert!(
        super::jsse_owns_endpoint_identification(&mut ctx, &[plain]),
        "a plain X509TrustManager is wrapped by JSSE, which then identifies"
    );

    // 3. An X509ExtendedTrustManager subclass — Netty's
    //    `X509TrustManagerWrapper`. JSSE uses it as-is and adds no check.
    let netty_cls = ctx
        .ensure_class_initialized("io/netty/handler/ssl/util/X509TrustManagerWrapper")
        .expect("mock class");
    ctx.set_superclass(netty_cls, extended);
    let netty = ctx.alloc_object(netty_cls, 1);
    assert!(
        !super::jsse_owns_endpoint_identification(&mut ctx, &[netty]),
        "an X509ExtendedTrustManager owns identification; JSSE adds no check of its own"
    );

    // 3b. The class itself, not only a subclass.
    let direct = ctx.alloc_object(extended, 1);
    assert!(!super::jsse_owns_endpoint_identification(
        &mut ctx,
        &[direct]
    ));

    // 4. `chooseTrustManager` takes the FIRST manager, so a plain one ahead
    //    of an extended one still means JSSE identifies.
    let plain2 = ctx.alloc_object(plain_cls, 1);
    assert!(
        super::jsse_owns_endpoint_identification(&mut ctx, &[plain2, netty]),
        "the FIRST manager decides, matching SSLContextImpl.chooseTrustManager"
    );

    // 5. JSSE's OWN X509TrustManagerImpl is an X509ExtendedTrustManager
    //    too, but its `checkServerTrusted` is served natively here and
    //    performs no identification — so this VM must, or nobody does
    //    (`testClientHostnameValidationFail`). This is the case
    //    `SslContextBuilder.trustManager(File)` produces.
    let jsse_cls = ctx
        .ensure_class_initialized("sun/security/ssl/X509TrustManagerImpl")
        .expect("mock class");
    ctx.set_superclass(jsse_cls, extended);
    let jsse = ctx.alloc_object(jsse_cls, 1);
    assert!(
        super::jsse_owns_endpoint_identification(&mut ctx, &[jsse]),
        "JSSE's own trust manager does not identify on this VM, so this VM must"
    );
}

#[test]
fn wrap_consumes_no_app_data_before_finished_is_reported() {
    // REGRESSION (websocket-jsse-ssl-bytes-consumed-during-write): rustls
    // reports `is_handshaking() == false` one flight BEFORE the engine has
    // told the caller the handshake ended. `handshake_status_of` keeps
    // answering NEED_WRAP through that window on purpose (the TLS 1.2
    // server-flight fix), so a caller that correctly obeys NEED_WRAP calls
    // `wrap(src, dst)` while, from its point of view, it is still
    // handshaking -- and JSSE's contract says such a wrap consumes NOTHING
    // from `src`.
    //
    // Gating app-data consumption on `!is_handshaking()` alone made the
    // engine treat `src` as application data in that window. Tomcat's
    // WebSocket client passes a 16921-byte STATIC
    // `AsyncChannelWrapperSecure.DUMMY`, so the engine drained 16384 bytes
    // of zeros out of it, ENCRYPTED them onto the wire mid-upgrade, and
    // reported bytesConsumed=16384 -- tripping
    // `AsyncChannelWrapperSecure.checkResult`'s "Bytes were consumed from
    // the input during a write" and killing every wss:// connect. DUMMY is
    // never rewound, so the damage leaked into later connections too.
    let mut client = super::EngineState::default();
    let mut server = super::EngineState::default();
    client.is_client = true;
    client.peer_host = Some("localhost".to_string());
    client.client_config = Some(
        super::build_client_config(
            {
                let mut roots = RootCertStore::empty();
                for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                    roots.add(c).unwrap();
                }
                roots
            },
            &[],
            None,
        )
        .unwrap(),
    );
    server.is_client = false;
    server.server_config = Some(
        super::build_server_config_single_cert(
            SERVER_CRT_PEM,
            SERVER_KEY_PEM,
            &[],
            false,
            None,
        )
        .unwrap(),
    );
    super::engine_begin(&mut client).expect("client begin");
    super::engine_begin(&mut server).expect("server begin");

    for _ in 0..32 {
        let _ = super::engine_wrap_pump(&mut client, &[], 65536);
        let buf = std::mem::take(&mut client.outbound);
        if !buf.is_empty() {
            let _ = super::engine_unwrap_pump(&mut server, &buf);
        }
        let _ = super::engine_wrap_pump(&mut server, &[], 65536);
        let buf2 = std::mem::take(&mut server.outbound);
        if !buf2.is_empty() {
            let _ = super::engine_unwrap_pump(&mut client, &buf2);
        }
        if !client.conn.as_ref().unwrap().is_handshaking() {
            break;
        }
    }

    // The exact window: rustls is done, the caller has NOT been told.
    // Nothing in the pumps sets `handshake_finished_reported`; only
    // `do_wrap`/`do_unwrap` do, when they hand a FINISHED result back.
    assert!(
        !client.conn.as_ref().unwrap().is_handshaking(),
        "client handshake did not complete"
    );
    assert!(
        !client.handshake_finished_reported,
        "precondition: FINISHED has not been reported to the caller yet"
    );

    let dummy = vec![0u8; 16921];
    let (consumed, _) = super::engine_wrap_pump(&mut client, &dummy, 65536);
    assert_eq!(
        consumed, 0,
        "wrap() consumed the caller's buffer before reporting FINISHED; \
         Tomcat's AsyncChannelWrapperSecure asserts bytesConsumed == 0 for \
         every handshake-time wrap"
    );

    // Once FINISHED has been reported, an ordinary application write must
    // still work -- the fix must not wedge the post-handshake path.
    client.handshake_finished_reported = true;
    let (consumed_after, _) = super::engine_wrap_pump(&mut client, &dummy, 65536);
    assert!(
        consumed_after > 0,
        "application data must be consumed once the handshake is reported finished"
    );
}

// -----------------------------------------------------------------------
// E12/E22 — the "nothing was negotiated" session, in the LIVE registrar
// -----------------------------------------------------------------------
//
// `phases_late::ssl_security` already has tests of this shape. They pass,
// and they measured code that is DEAD: `--dump-native-registry` shows THIS
// file's `getProtocol`/`getCipherSuite`/`getId`/`isValid` owning the
// `javax/net/ssl/SSLSession` slots in the default real-JDK mode, which is
// the mode `--real-jdk` runs. Duplicating the coverage here is the point:
// a green test over an overwritten registration is worse than no test,
// because it reads as proof.

fn session_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    super::register_ssl_session_real(&mut r);
    r
}

/// DOOR 1 of `regression-suite/src/RSslNullSession.java`, without a VM:
/// the session `ssl_security::new13_alloc_null_ssl_session` mints for an
/// `SSLSocket` that was never connected (`tls_id = -1`).
///
/// `getId` and `isValid` are the two this file decides and the two the
/// nominated patch would have missed — a field-count-only test treats
/// every shape narrower than 7 as "negotiated", which is exactly this one.
///
/// E42 — **run at BOTH widths, and 4 is the one that ships.**
/// `NEW13_SSL_SESS_FIELDS` went 3 -> 4 so `putValue` would stop writing
/// over slot 2, which moved the live null session onto
/// `session_has_negotiated`'s `_ => true` arm. Width 3 stays here because
/// the merged `3 | 4` arm still answers it defensively and a regression
/// dropping either half would otherwise be invisible from this crate;
/// `ssl_security::new13_tests` holds the same line from the other side.
#[test]
fn the_null_socket_session_has_no_id_and_is_not_valid() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");

    // [0]=proto [1]=cipher [2]=tls_id, tls_id = -1 => never connected.
    // Width 4 adds [3]=attrs, which is the whole reason it is 4.
    for width in [3usize, 4] {
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), width);
        let p = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL);
        let c = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
        ctx.set_field(sess, 0, Value::Object(Some(p)));
        ctx.set_field(sess, 1, Value::Object(Some(c)));
        ctx.set_field(sess, 2, Value::Int(-1));
        if width > 3 {
            ctx.set_field(sess, 3, Value::Object(None));
        }
        let this = &[Value::Object(Some(sess))];

        match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(a)))) => assert_eq!(
                ctx.array_length(a),
                0,
                "width {width}: HotSpot answers byte[0] for a session that \
                 negotiated nothing; Tomcat's JSSESupport.java:171 tests \
                 `length == 0` exactly"
            ),
            other => panic!("getId must return an array, got {other:?}"),
        }

        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(0)),
            "width {width}: HotSpot 25.0.3+9-LTS, unconnected SSLSocket: \
             isValid() = false"
        );
    }
}

/// MUTATION CHECK for the test above. Without it, `getId` could return
/// `byte[0]` and `isValid` `false` unconditionally and the previous test
/// would still pass — measuring one branch and calling it coverage.
///
/// The 32-byte id is itself a CratonVM stand-in (this VM does not surface
/// the real negotiated session id); what is pinned here is only that a
/// session which DID negotiate is not dragged into the null-session
/// branch by this change.
#[test]
fn a_session_that_negotiated_keeps_its_id_and_its_validity() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    // 3-field connect shape (retired by E42; the merged arm still answers
    // it) with a real stream id.
    let live = ctx.alloc_object(cratonvm_types::ClassId::new(0), 3);
    ctx.set_field(live, 2, Value::Int(0)); // id 0 is a VALID stream id
                                           // E42 — the 4-field accept/NEW-13 shape, which is what the tree mints
                                           // TODAY. Two rows, because `>= 0` is the boundary and `0` is on it:
                                           // `SSLServerSocket.accept` writes `RUSTLS_SOCK_ID_BASE + stream_id`, so
                                           // its ids are large, while `invalidate()`/`close()` bugs in this family
                                           // have historically produced exactly `Int(0)`. Both must read as
                                           // negotiated, which is what makes the merged arm a NO-OP for the accept
                                           // shape rather than a change to it.
    let live4 = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(live4, 2, Value::Int(0));
    ctx.set_field(live4, 3, Value::Object(None));
    let live4_accept = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(
        live4_accept,
        2,
        Value::Int(crate::servlet::RUSTLS_SOCK_ID_BASE + 3),
    );
    ctx.set_field(live4_accept, 3, Value::Object(None));
    // 8-field engine shape, slot 2 = the isValid flag, set.
    let engine_ok = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(engine_ok, 2, Value::Int(1));
    // 8-field engine shape BEFORE a handshake — the DOOR 2 state.
    let engine_pre = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(engine_pre, 2, Value::Int(0));

    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");

    for (what, sess, want_len, want_valid) in [
        ("3-field, stream id 0", live, 32usize, 1i32),
        ("4-field, stream id 0", live4, 32, 1),
        ("4-field, accept-shaped offset id", live4_accept, 32, 1),
        ("8-field, negotiated", engine_ok, 32, 1),
        ("8-field, pre-handshake", engine_pre, 0, 0),
    ] {
        let this = &[Value::Object(Some(sess))];
        match get_id(&mut ctx, this) {
            Ok(Some(Value::Object(Some(a)))) => {
                assert_eq!(ctx.array_length(a), want_len, "getId: {what}")
            }
            other => panic!("getId must return an array for {what}, got {other:?}"),
        }
        assert_eq!(
            is_valid(&mut ctx, this).unwrap(),
            Some(Value::Int(want_valid)),
            "isValid: {what}"
        );
    }
}

/// `getProtocol`/`getCipherSuite` are contracted non-null by JSSE, and
/// both used to hand back `ctx.get_field(..)` raw. `http2.rs` mints a
/// 6-field session and never writes a slot of it, so that shape produced a
/// null String from a method that cannot return one.
#[test]
fn an_unpopulated_session_shape_answers_the_sentinel_not_a_null_string() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
    let this = &[Value::Object(Some(sess))];

    let proto = r
        .find(
            "javax/net/ssl/SSLSession",
            "getProtocol",
            "()Ljava/lang/String;",
        )
        .expect("getProtocol registered");
    match proto(&mut ctx, this) {
        Ok(Some(Value::Object(Some(s)))) => assert_eq!(
            ctx.read_string(s).as_deref(),
            Some(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL)
        ),
        other => panic!("getProtocol must never return a null String, got {other:?}"),
    }

    let cipher = r
        .find(
            "javax/net/ssl/SSLSession",
            "getCipherSuite",
            "()Ljava/lang/String;",
        )
        .expect("getCipherSuite registered");
    match cipher(&mut ctx, this) {
        Ok(Some(Value::Object(Some(s)))) => assert_eq!(
            ctx.read_string(s).as_deref(),
            Some(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE)
        ),
        other => panic!("getCipherSuite must never return a null String, got {other:?}"),
    }
}

/// §2's argument, mechanised on THIS file's list rather than argued: the
/// sentinel is safe to return only because it can never be the outcome of
/// a handshake. `SUPPORTED_CIPHER_SUITE_NAMES` is what this VM claims to
/// support through `getSupportedCipherSuites`, so if the sentinel ever
/// appears in it the reason for returning the sentinel is gone.
#[test]
fn the_sentinel_is_never_offerable_and_the_fabrication_always_was() {
    assert!(
        !SUPPORTED_CIPHER_SUITE_NAMES
            .contains(&crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE),
        "the null-session sentinel must never be an offerable suite — that \
         unofferability is the whole reason it is not a fabrication"
    );
    assert!(
        SUPPORTED_CIPHER_SUITE_NAMES.contains(&"TLS_AES_256_GCM_SHA384"),
        "the literal this lane REMOVED is offerable, which is exactly why no \
         caller could tell it from a real negotiation"
    );
}

/// E31 — THE SLOT COLLISION, as a test rather than as a nomination.
///
/// `putValue` used to write its `java.util.HashMap` into
/// `num_fields - 1`, which on the then-3-field null-session shape was the
/// STREAM ID. That is the slot `session_has_negotiated` reads, so a single
/// `putValue` turned `Int(-1)` into an object reference, took the
/// predicate's defensive `_ => true` arm, and handed the session back its
/// 32-byte fabricated id and `isValid() == true`.
///
/// This is the whole defect in one assertion: **do the writes, then re-ask
/// the two accessors E12/E22 fixed.** Jetty's
/// `SecureRequestCustomizer.retrieveSni()` is the real caller.
#[test]
fn a_put_value_cannot_resurrect_the_null_sessions_id_or_validity() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    // The DOOR 1 shape: [0]=proto [1]=cipher [2]=tls_id = -1.
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 3);
    ctx.set_field(sess, 2, Value::Int(-1));

    let name = ctx.create_string("org.eclipse.jetty.sni.host");
    let value = ctx.create_string("example.test");
    let put = r
        .find(
            "javax/net/ssl/SSLSession",
            "putValue",
            "(Ljava/lang/String;Ljava/lang/Object;)V",
        )
        .expect("putValue registered");
    put(
        &mut ctx,
        &[
            Value::Object(Some(sess)),
            Value::Object(Some(name)),
            Value::Object(Some(value)),
        ],
    )
    .expect("putValue must not fail");

    assert_eq!(
        ctx.get_field(sess, 2),
        Value::Int(-1),
        "putValue must not overwrite NEW13_SESS_TLSID on a shape with no \
         attribute slot — that slot is what says 'nothing was negotiated'"
    );

    let this = &[Value::Object(Some(sess))];
    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(a)))) => assert_eq!(
            ctx.array_length(a),
            0,
            "a putValue must not give an unhandshaked session an id"
        ),
        other => panic!("getId must return an array, got {other:?}"),
    }
    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");
    assert_eq!(
        is_valid(&mut ctx, this).unwrap(),
        Some(Value::Int(0)),
        "a putValue must not make an unhandshaked session valid"
    );
}

/// E42 — THE SAME QUESTION AT THE WIDTH THAT SHIPS.
///
/// The test above pins the retired 3-field shape, where the fix was to
/// REFUSE (`sslsess_attrs_slot` -> `None`). `NEW13_SSL_SESS_FIELDS` is 4
/// now, so the live null session takes the *other* branch: slot 3 is a real
/// attribute slot, the write happens, and the thing that must not move is
/// slot 2. That makes this the executable form of the whole E42 bargain —
/// the API works AND the stream id survives it — and it is a different
/// assertion from the width-3 one, not a rename of it.
///
/// The three follow-up assertions are the measured consequences that were
/// reported when the widening landed without `session_has_negotiated`'s arm
/// merge: a `putValue` flipped `isValid()` back to `true` and `getId()`
/// back to 32 fabricated bytes through an unrelated API. Note they would
/// pass here for the WRONG reason if the arm merge were reverted — slot 2
/// still reads `Int(-1)` — which is why
/// `the_null_socket_session_has_no_id_and_is_not_valid` runs at width 4
/// too. That test is the one that fails on a reverted arm; this one is the
/// one that fails if the attribute slot regresses onto slot 2.
#[test]
fn a_widened_null_session_put_value_does_not_touch_the_stream_id() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    // The width-4 DOOR 1 shape: [0]=proto [1]=cipher [2]=tls_id = -1,
    // [3]=attrs.
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(sess, 2, Value::Int(-1));
    ctx.set_field(sess, 3, Value::Object(None));

    let name = ctx.create_string("org.eclipse.jetty.sni.host");
    let value = ctx.create_string("example.test");
    let put = r
        .find(
            "javax/net/ssl/SSLSession",
            "putValue",
            "(Ljava/lang/String;Ljava/lang/Object;)V",
        )
        .expect("putValue registered");
    put(
        &mut ctx,
        &[
            Value::Object(Some(sess)),
            Value::Object(Some(name)),
            Value::Object(Some(value)),
        ],
    )
    .expect("putValue must not fail");

    assert!(
        matches!(ctx.get_field(sess, 3), Value::Object(Some(_))),
        "slot 3 IS the attribute slot at NEW13_SSL_SESS_FIELDS = 4 — the \
         widening exists so this write has somewhere to land; if it is \
         None, `sslsess_attrs_slot`'s `4 => Some(3)` row has regressed"
    );
    assert_eq!(
        ctx.get_field(sess, 2),
        Value::Int(-1),
        "and it must NOT have landed on the stream id. `num_fields - 1` is \
         3 here as well, so a reader who 'simplifies' `sslsess_attrs_slot` \
         back to that spelling passes this — the guard is the 6-field row, \
         not this one"
    );

    let this = &[Value::Object(Some(sess))];
    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(a)))) => assert_eq!(
            ctx.array_length(a),
            0,
            "a putValue must not give an unhandshaked session an id"
        ),
        other => panic!("getId must return an array, got {other:?}"),
    }
    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");
    assert_eq!(
        is_valid(&mut ctx, this).unwrap(),
        Some(Value::Int(0)),
        "a putValue must not make an unhandshaked session valid"
    );
}

// -----------------------------------------------------------------------
// F18 — the four doors that had no real-mode registration, the one bit
// behind `isValid()`, and the width table that stops a session reading
// another connection's certificate chain
// -----------------------------------------------------------------------

/// **A registration census, not a behaviour test.** Each of these four was
/// absent from real-JDK mode entirely, and the failure mode of an absent
/// registration on `javax/net/ssl/SSLSession` is not a wrong answer — it is
/// `AbstractMethodError`, because the interface declaration carries no Code
/// attribute. A body cannot be tested until it exists, so this asserts that
/// it exists.
///
/// `invalidate` is F10-1 NOMINATION 3; `getPeerHost`/`getPeerPort` are the
/// accessor half of E12-1's residual 4 (which recorded the *producer* and
/// did not notice the accessor was missing); `getSessionContext` had zero
/// registrations anywhere in the crate, in either mode.
#[test]
fn the_four_unregistered_real_mode_session_doors_are_registered() {
    let r = session_registry();
    let cls = "javax/net/ssl/SSLSession";
    for (name, desc) in [
        ("invalidate", "()V"),
        ("getPeerHost", "()Ljava/lang/String;"),
        ("getPeerPort", "()I"),
        ("getSessionContext", "()Ljavax/net/ssl/SSLSessionContext;"),
    ] {
        assert!(
            r.find(cls, name, desc).is_some(),
            "{cls}.{name}{desc} has no real-JDK-mode registration. It is an \
             abstract interface declaration with no Code attribute, so an \
             un-intercepted call is an AbstractMethodError in the mode \
             --real-jdk runs — not a wrong value, a thrown Error. \
             See docs/known-issues/jdk-only/F18-1-*.md."
        );
    }
}

/// **The marker-collision invariant, mechanised in the file that now
/// depends on it.**
///
/// `net_phase_e::HTTPS_CLIENT_SESSION_MARKER` is written into slot 2 of
/// every HTTPS client session so `session_has_negotiated` reads it as
/// "negotiated". `peer_certs_for_session` also reads slot 2 — as a
/// `servlet` registry KEY. The two uses are only compatible while the
/// marker cannot name a real socket: if it ever could, one connection
/// would be handed another connection's peer certificate chain, which is a
/// far worse failure than the `isValid()` one F10 fixed.
///
/// F10-1 §4.1 argued this from the constants. This asserts it, so that
/// moving any one of the four numbers fails a build instead of a comment.
#[test]
fn the_https_session_marker_can_never_name_a_real_socket() {
    use crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER as M;
    assert!(
        M > 0,
        "the marker must be >= 0 or `session_has_negotiated` reads it as \
         'never negotiated' — the defect F10-1 fixed"
    );
    for (name, base) in [
        (
            "PENDING_CONNECT_SOCK_ID_BASE",
            crate::servlet::PENDING_CONNECT_SOCK_ID_BASE,
        ),
        (
            "PENDING_LAYERED_SOCK_ID_BASE",
            crate::servlet::PENDING_LAYERED_SOCK_ID_BASE,
        ),
        ("RUSTLS_SOCK_ID_BASE", crate::servlet::RUSTLS_SOCK_ID_BASE),
    ] {
        assert!(
            M < base,
            "HTTPS_CLIENT_SESSION_MARKER ({M:#x}) must stay strictly below \
             servlet::{name} ({base:#x}), or an HTTPS session's slot 2 \
             names a real socket id and `peer_certs_for_session` hands it \
             THAT socket's peer certificate chain"
        );
    }
}

/// The width table behind `session_stream_id`, which is the fix for the
/// cross-connection read E31-1 NOMINATION 4 (re-raising E22-1 NOMINATION B)
/// recorded and nobody had landed.
///
/// The old test was `> NEW13_SESS_TLSID` — "three or more fields" — so on
/// the 8-field engine session slot 2, the `isValid` FLAG, was looked up in
/// the socket registry. `servlet::s2_next_free_id` starts its counter at
/// `1`, so a valid engine session asked the registry for the chain of the
/// FIRST socket the process ever opened.
#[test]
fn slot_two_is_only_a_stream_id_on_the_widths_where_it_is_one() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();

    // Width 4 — the NEW-13 shape and `SSLServerSocket.accept`'s shape.
    // Slot 2 IS a stream id here, and this is the ONLY width that ships.
    let four = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(four, 2, Value::Int(7));
    assert_eq!(super::session_stream_id(&ctx, four), Some(7));

    // ... and `-1`, every minter's "never connected" sentinel, is not one.
    ctx.set_field(four, 2, Value::Int(-1));
    assert_eq!(super::session_stream_id(&ctx, four), None);

    // Width 8 — the engine session. Slot 2 is the `isValid` flag. Both of
    // its values are plausible registry keys, and BOTH must be refused.
    for flag in [0, 1] {
        let eight = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        ctx.set_field(eight, 2, Value::Int(flag));
        assert_eq!(
            super::session_stream_id(&ctx, eight),
            None,
            "slot 2 of the 8-field engine shape is the isValid flag, not a \
             stream id; reading it as one looks up socket id {flag} and \
             `s2_next_free_id` hands out id 1 first"
        );
    }

    // Width 6 — `tls.rs`'s shape, where slot 2 is `tls.rs::SES_VALID`.
    let six = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
    ctx.set_field(six, 2, Value::Int(1));
    assert_eq!(super::session_stream_id(&ctx, six), None);
}

/// `invalidate()` moves `isValid()` — the whole point of registering it —
/// and moves NOTHING else. MEASURED, HotSpot 25.0.3+9-LTS, on a session
/// that genuinely negotiated (`scratchpad/f18/F18SessionContract.java`,
/// three runs byte-identical): after `invalidate()` the id keeps its 32
/// bytes *and its exact contents*, and the cipher and protocol survive.
///
/// The id half is the assertion that matters, because it is the one a
/// plausible "simplification" breaks: gating `getId` on `isValid` instead
/// of on `session_has_negotiated` would make `invalidate()` erase the id,
/// trading a new divergence for the fixed one.
#[test]
fn invalidate_moves_is_valid_and_nothing_else() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    // A width-4 session that DID negotiate: slot 2 is a real stream id.
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(sess, 2, Value::Int(11));
    ctx.set_field(sess, 3, Value::Object(None));
    let this = &[Value::Object(Some(sess))];

    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");
    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    let invalidate = r
        .find("javax/net/ssl/SSLSession", "invalidate", "()V")
        .expect("invalidate registered");

    assert_eq!(
        is_valid(&mut ctx, this).unwrap(),
        Some(Value::Int(1)),
        "a session carrying stream id 11 negotiated, and nothing has \
         invalidated it yet"
    );
    let id_before = match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(a)))) => (0..ctx.array_length(a))
            .map(|i| ctx.get_array_element(a, i))
            .collect::<Vec<_>>(),
        other => panic!("getId must return an array, got {other:?}"),
    };
    assert_eq!(id_before.len(), 32, "a negotiated session has a 32-byte id");

    invalidate(&mut ctx, this).expect("invalidate must not fail");

    assert_eq!(
        is_valid(&mut ctx, this).unwrap(),
        Some(Value::Int(0)),
        "invalidate() must move isValid() to false — at THIS width. Before \
         F18 the only `invalidate` in the crate was `tls.rs`'s, gated \
         `> SES_CREATION_TIME`, so it no-opped here and the session stayed \
         valid after being invalidated"
    );
    let id_after = match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(a)))) => (0..ctx.array_length(a))
            .map(|i| ctx.get_array_element(a, i))
            .collect::<Vec<_>>(),
        other => panic!("getId must return an array, got {other:?}"),
    };
    assert_eq!(
        id_before, id_after,
        "MEASURED on HotSpot: invalidate() leaves the session id byte-for-byte \
         intact. `getId` is gated on `session_has_negotiated` ALONE and must \
         not be 'simplified' onto `session_is_valid`"
    );

    // Idempotent, as measured.
    invalidate(&mut ctx, this).expect("second invalidate must not fail");
    assert_eq!(is_valid(&mut ctx, this).unwrap(), Some(Value::Int(0)));
}

/// MUTATION CHECK for the test above, in both directions the invalidated
/// bit could be wrong.
///
/// Without this, `session_is_valid` could answer `false` for everything —
/// or `session_mark_invalidated` could mark every session in the process —
/// and `invalidate_moves_is_valid_and_nothing_else` would still pass,
/// because it only ever looks at one object.
#[test]
fn invalidating_one_session_does_not_invalidate_another() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    let a = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(a, 2, Value::Int(21));
    let b = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(b, 2, Value::Int(22));

    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");
    let invalidate = r
        .find("javax/net/ssl/SSLSession", "invalidate", "()V")
        .expect("invalidate registered");

    invalidate(&mut ctx, &[Value::Object(Some(a))]).expect("invalidate");
    assert_eq!(
        is_valid(&mut ctx, &[Value::Object(Some(a))]).unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        is_valid(&mut ctx, &[Value::Object(Some(b))]).unwrap(),
        Some(Value::Int(1)),
        "the invalidated bit is per SESSION. HotSpot: invalidating one \
         connection's session leaves a second, untouched connection's \
         session valid (MEASURED, F18SessionContract ARM D)"
    );
}

/// The null session is where `invalidate()` must do NOTHING, and this
/// pins that the newly-registered door did not become a way to *change*
/// it. MEASURED on HotSpot: on an unconnected `SSLSocket`'s session and a
/// pre-handshake `SSLEngine`'s alike, every accessor reads identically
/// before and after `invalidate()`.
///
/// This is also the cheapest regression test for the whole F18 change
/// against the failure mode F10-1 §7 names: if any edit here loosened
/// `session_has_negotiated`, the null session would report `isValid()` and
/// a 32-byte id again — the fabrication E12/E22/E31/E42 removed.
#[test]
fn invalidate_changes_nothing_on_a_session_that_negotiated_nothing() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(sess, 2, Value::Int(-1));
    ctx.set_field(sess, 3, Value::Object(None));
    let this = &[Value::Object(Some(sess))];

    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");
    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    let invalidate = r
        .find("javax/net/ssl/SSLSession", "invalidate", "()V")
        .expect("invalidate registered");

    assert_eq!(is_valid(&mut ctx, this).unwrap(), Some(Value::Int(0)));
    let len_before = match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
        other => panic!("getId must return an array, got {other:?}"),
    };
    assert_eq!(len_before, 0);

    invalidate(&mut ctx, this).expect("invalidate must not fail");

    assert_eq!(
        is_valid(&mut ctx, this).unwrap(),
        Some(Value::Int(0)),
        "still false — there was nothing to invalidate"
    );
    let len_after = match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
        other => panic!("getId must return an array, got {other:?}"),
    };
    assert_eq!(
        len_after, 0,
        "and still byte[0]. An invalidate() that gave the null session an \
         id would be the E42-era slot-2 corruption with a new writer"
    );
}

/// `getSessionContext()` answers `null` for a session that is in no
/// context, and a real carrier for one that is.
///
/// **G7 rewrote this test with the registration it pins.** It used to
/// assert `null` for BOTH a negotiated and a never-negotiated shape, and
/// said so deliberately: the interface's own contract permits `null`
/// ("This context may be unavailable in some environments…" —
/// `javax/net/ssl/SSLSession.getSessionContext`) and this VM was described
/// as having no session cache. The second half of that was false —
/// `net_phase_e` allocates a `javax/net/ssl/SSLSessionContext` carrier for
/// `SSLContext.get{Client,Server}SessionContext()` and registers its whole
/// six-method surface — so the door was under-reporting against its own
/// siblings, not against a capability the VM lacks.
///
/// The tempting wrong fix is still a fabricated context, and the tempting
/// wrong REFUSAL is still a blanket `null`; the arms below pin the boundary
/// between them at the measured place: negotiated AND not invalidated.
/// MEASURED, HotSpot 25.0.3+9-LTS, `scratchpad/g7/TlsProbe.java`.
#[test]
fn get_session_context_answers_null_rather_than_fabricating_one() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    let get_ctx = r
        .find(
            "javax/net/ssl/SSLSession",
            "getSessionContext",
            "()Ljavax/net/ssl/SSLSessionContext;",
        )
        .expect("getSessionContext registered");
    let invalidate = r
        .find("javax/net/ssl/SSLSession", "invalidate", "()V")
        .expect("invalidate registered");

    // ARM 1 — width-4 socket shape, slot 2 = -1: nothing was negotiated.
    let never = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(never, 2, Value::Int(-1));
    assert_eq!(
        get_ctx(&mut ctx, &[Value::Object(Some(never))]).unwrap(),
        Some(Value::Object(None)),
        "a session that negotiated nothing is in no context — HotSpot \
         answers null on an unconnected socket's session and on a \
         pre-handshake engine's alike"
    );

    // ARM 2 — width-4 socket shape with a real stream id: negotiated and
    // not invalidated, so it IS in a context. This is the row the old
    // unconditional `null` got wrong, and the row netty's
    // `SSLEngineTest.testSessionAfterHandshake0` asserts non-null on.
    let live = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(live, 2, Value::Int(5));
    match get_ctx(&mut ctx, &[Value::Object(Some(live))]).unwrap() {
        Some(Value::Object(Some(_))) => {}
        other => panic!(
            "a negotiated, valid session must answer a real \
             javax/net/ssl/SSLSessionContext carrier — the same zero-field \
             object net_phase_e hands out from \
             SSLContext.getClientSessionContext(), whose six methods it \
             registers. Got {other:?}"
        ),
    }

    // ARM 3 — and `invalidate()` takes it back out. MEASURED: the second
    // thing invalidate() moves, after isValid().
    invalidate(&mut ctx, &[Value::Object(Some(live))]).expect("invalidate must not fail");
    assert_eq!(
        get_ctx(&mut ctx, &[Value::Object(Some(live))]).unwrap(),
        Some(Value::Object(None)),
        "invalidate() evicts the session from its context; HotSpot's \
         ctx.getSession(id) answers null afterwards where it answered \
         SAME-OBJECT before"
    );
}

/// The engine shape's three states, which the socket shape cannot express:
/// a fresh engine, a handshake in flight, and a completed one. This is the
/// gate G7 moved off `getId()` and onto this door, so it is tested here and
/// its absence is tested next door.
///
/// MEASURED, HotSpot 25.0.3+9-LTS (`scratchpad/g7/TlsProbe.java`), sampling
/// `SSLEngine.getHandshakeSession()` from inside
/// `X509ExtendedTrustManager.checkServerTrusted(chain, authType, SSLEngine)`:
/// mid-handshake the session already answers a 32-byte id, a real cipher
/// suite and `isValid() == true`, and `getSessionContext()` is still `null`.
/// A gate on validity alone cannot produce that row.
#[test]
fn a_handshake_still_in_flight_has_no_session_context_yet() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    let get_ctx = r
        .find(
            "javax/net/ssl/SSLSession",
            "getSessionContext",
            "()Ljavax/net/ssl/SSLSessionContext;",
        )
        .expect("getSessionContext registered");

    // Fresh engine: `build_synthetic_ssl_session` writes slot 2 = 0 because
    // `conn.negotiated_cipher_suite()` is None.
    let fresh = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(fresh, 2, Value::Int(0));
    assert_eq!(
        get_ctx(&mut ctx, &[Value::Object(Some(fresh))]).unwrap(),
        Some(Value::Object(None))
    );

    // Mid-handshake: a suite HAS been negotiated (slot 2 = 1) but
    // `engine_session_for` has not recorded this object as belonging to a
    // completed epoch.
    let mid = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(mid, 2, Value::Int(1));
    assert_eq!(
        get_ctx(&mut ctx, &[Value::Object(Some(mid))]).unwrap(),
        Some(Value::Object(None)),
        "mid-handshake the session is valid and has an id, and HotSpot \
         still answers null here"
    );

    // Completed: the key `engine_session_for` inserts is present.
    let done = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(done, 2, Value::Int(1));
    let key = super::gc_stable_objref_key(&ctx, done);
    super::negotiated_session_keys().lock().insert(key);
    let answer = get_ctx(&mut ctx, &[Value::Object(Some(done))]).unwrap();
    // Removed before asserting: `negotiated_session_keys` is a process
    // global and `MockNativeContext` restarts its pointer sequence per
    // instance, so a leaked key can be re-derived by another test's object.
    super::negotiated_session_keys().lock().remove(&key);
    match answer {
        Some(Value::Object(Some(_))) => {}
        other => panic!("a completed engine session must answer a real context. Got {other:?}"),
    }
}

/// The other half of the same move: `getId()` must NOT consult
/// `negotiated_session_keys` any more.
///
/// MEASURED: mid-handshake HotSpot answers `byte[32]`, byte-identical to
/// the id the completed session then reports. The old second gate answered
/// `byte[0]` in exactly that state, and Tomcat's `JSSESupport.getSessionId`
/// tests `length == 0` exactly, so `byte[0]` there presents a live
/// handshake as an untrackable session.
///
/// The fresh-engine row is the one netty's `SSLEngineTest.testSSLSessionId`
/// asserts (`assertEquals(0, engine.getSession().getId().length)`), and it
/// is answered by `session_has_negotiated` alone.
#[test]
fn get_id_answers_thirty_two_bytes_while_the_handshake_is_still_running() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");

    // Fresh engine — no suite negotiated. Still byte[0].
    let fresh = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(fresh, 2, Value::Int(0));
    let len = match get_id(&mut ctx, &[Value::Object(Some(fresh))]) {
        Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
        other => panic!("getId must return an array, got {other:?}"),
    };
    assert_eq!(
        len, 0,
        "a freshly created engine's session has no id — netty's \
         SSLEngineTest.testSSLSessionId asserts exactly this"
    );

    // Mid-handshake — a suite has been negotiated, the epoch key is absent.
    let mid = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(mid, 2, Value::Int(1));
    assert!(
        !super::negotiated_session_keys()
            .lock()
            .contains(&super::gc_stable_objref_key(&ctx, mid)),
        "this arm is only meaningful while the epoch key is absent"
    );
    let len = match get_id(&mut ctx, &[Value::Object(Some(mid))]) {
        Ok(Some(Value::Object(Some(arr)))) => ctx.array_length(arr),
        other => panic!("getId must return an array, got {other:?}"),
    };
    assert_eq!(
        len, 32,
        "mid-handshake HotSpot answers a 32-byte id. If this reads 0, the \
         `engine_shape && !session_is_negotiated(..)` gate has come back to \
         getId — it belongs on getSessionContext, which is the door whose \
         oracle answer actually changes across that boundary. See \
         docs/known-issues/jdk-only/G7-1-*.md §3."
    );
}

/// `getPeerHost`/`getPeerPort` must not read slot 3 and 4 on a shape where
/// they are not the host and port.
///
/// This is the E31-1 §2 defect, which was live in `tls.rs`'s copies: on the
/// width-4 shape slot 3 is the ATTRIBUTE MAP, so a width-blind
/// `getPeerHost` returns a `java.util.HashMap` through a
/// `()Ljava/lang/String;` descriptor as soon as anything has called
/// `putValue` — and Jetty's `SecureRequestCustomizer.retrieveSni()` does,
/// on every SSL request. The new registrations use `session_cipher_slot`'s
/// `>= 6` boundary so this file has one width line and not two.
///
/// HotSpot's measured answers for a session that negotiated nothing are
/// `null` and `-1`, which is what the width-4 shape with no registry entry
/// falls through to here.
#[test]
fn peer_host_and_port_do_not_read_the_attribute_slot() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    // Width 4 with a populated attribute slot — the Jetty state.
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(sess, 2, Value::Int(-1));
    let map = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(sess, 3, Value::Object(Some(map)));
    let this = &[Value::Object(Some(sess))];

    let host = r
        .find(
            "javax/net/ssl/SSLSession",
            "getPeerHost",
            "()Ljava/lang/String;",
        )
        .expect("getPeerHost registered");
    let port = r
        .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
        .expect("getPeerPort registered");

    assert_eq!(
        host(&mut ctx, this).unwrap(),
        Some(Value::Object(None)),
        "slot 3 is the ATTRIBUTE MAP at width 4, not the peer host. \
         Returning it would hand a java.util.HashMap back through a \
         ()Ljava/lang/String; descriptor"
    );
    assert_eq!(
        port(&mut ctx, this).unwrap(),
        Some(Value::Int(-1)),
        "HotSpot's measured answer for a session that negotiated nothing"
    );
}

/// MUTATION CHECK for the test above: without it, both accessors could
/// return the sentinel for EVERY shape and still pass. The 6-/8-field
/// shapes DO carry a peer host at slot 3 and a port at slot 4, and must
/// report them.
#[test]
fn peer_host_and_port_are_read_on_the_shapes_that_carry_them() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    let host = r
        .find(
            "javax/net/ssl/SSLSession",
            "getPeerHost",
            "()Ljava/lang/String;",
        )
        .expect("getPeerHost registered");
    let port = r
        .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
        .expect("getPeerPort registered");

    for width in [6, 8] {
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), width);
        let h = ctx.create_string("example.test");
        ctx.set_field(sess, 3, Value::Object(Some(h)));
        ctx.set_field(sess, 4, Value::Int(8443));
        let this = &[Value::Object(Some(sess))];
        assert_eq!(
            host(&mut ctx, this).unwrap(),
            Some(Value::Object(Some(h))),
            "slot 3 IS the peer host at width {width}"
        );
        assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(8443)));
    }
}

/// `tls.rs::init_ssl_session_fields` seeds the peer-host slot with
/// `Int(0)`, not a String reference. A `getPeerHost` that returned the raw
/// slot would hand an `Int` back through a `()Ljava/lang/String;`
/// descriptor — the same descriptor violation as the attribute-map case,
/// from the opposite direction.
///
/// The port half is the same defect with a quieter symptom: `http2.rs`'s
/// 6-field session writes NO slot, so slot 4 is the allocator's zero fill,
/// and `0` returned through `()I` is a *plausible* value HotSpot never
/// produces — the shape this directory keeps recording as worse than a
/// loud one.
#[test]
fn an_unwritten_peer_host_slot_is_null_and_an_unwritten_port_is_minus_one() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
    ctx.set_field(sess, 3, Value::Int(0));
    ctx.set_field(sess, 4, Value::Int(0));
    let this = &[Value::Object(Some(sess))];
    let host = r
        .find(
            "javax/net/ssl/SSLSession",
            "getPeerHost",
            "()Ljava/lang/String;",
        )
        .expect("getPeerHost registered");
    let port = r
        .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
        .expect("getPeerPort registered");
    assert_eq!(host(&mut ctx, this).unwrap(), Some(Value::Object(None)));
    assert_eq!(
        port(&mut ctx, this).unwrap(),
        Some(Value::Int(-1)),
        "HotSpot's measured answer when there is no peer. A connected peer \
         never reports port 0, so mapping the zero fill to -1 cannot mask \
         a real port"
    );
}

/// G51 — the peer endpoint on the shape that has no slot for one.
///
/// MEASURED, `RSslLiveSession` on `9ae371468` (`target-rel3`), 2026-08-17:
///
/// ```text
/// CK RSslLiveSession client.peerHost                    = null   WANT localhost
/// CK RSslLiveSession client.peerPort.isServerPort       = false  WANT true
/// CK RSslLiveSession attrs.shadow.peerHost              = null   WANT localhost
/// CK RSslLiveSession attrs.shadow.peerPort.isServerPort = false  WANT true
/// CK RSslLiveSession server.peerPort.isPositive         = false  WANT true
/// ```
///
/// The trap is armed in the same breath: slot 3 carries a populated
/// ATTRIBUTE MAP, which is the state E31-1 §2 records — a width-blind read
/// hands a `java.util.HashMap` back through a `()Ljava/lang/String;`
/// descriptor. The recorded endpoint must be answered from the SIDE TABLE
/// and the attribute slot must stay untouched, which is the whole reason
/// G44-1 §4 rejected widening the session shape instead.
#[test]
fn a_recorded_peer_endpoint_answers_the_shape_that_has_no_slot_for_one() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();

    // The width-4 HTTPS/accept shape, with the attribute map populated.
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(
        sess,
        2,
        Value::Int(crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER),
    );
    let map = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(sess, 3, Value::Object(Some(map)));
    let this = &[Value::Object(Some(sess))];

    let host = r
        .find(
            "javax/net/ssl/SSLSession",
            "getPeerHost",
            "()Ljava/lang/String;",
        )
        .expect("getPeerHost registered");
    let port = r
        .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
        .expect("getPeerPort registered");

    // MUTATION GUARD, and the before-state: with nothing recorded the two
    // accessors must still answer HotSpot's never-negotiated pair, because
    // that is what `RSslNullSession` (89 checks, green) asserts.
    assert_eq!(host(&mut ctx, this).unwrap(), Some(Value::Object(None)));
    assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(-1)));

    super::record_session_peer_endpoint(&ctx, sess, "localhost", 45123);
    match host(&mut ctx, this).unwrap() {
        Some(Value::Object(Some(s))) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some("localhost"))
        }
        other => panic!("getPeerHost must answer the recorded host, got {other:?}"),
    }
    assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(45123)));
    assert_eq!(
        ctx.get_field(sess, 3),
        Value::Object(Some(map)),
        "the attribute slot is not the peer host and must not be disturbed \
         — E31-1 §2, and the reason G44-1 §4 refuses to widen this shape"
    );
}

/// The recorded endpoint must NOT outrank the slots on the shapes that
/// genuinely carry a peer host and port.
///
/// The width branch runs first, deliberately: the 6- and 8-field shapes are
/// written by their own minters and a side-table row for one of them would
/// be a second source of truth for a fact the object already states. This
/// is the mutation check on the ORDER of the two lookups — a reader that
/// consulted the table first would pass every other test in this file.
#[test]
fn a_recorded_endpoint_does_not_shadow_the_slots_that_carry_one() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    let host = r
        .find(
            "javax/net/ssl/SSLSession",
            "getPeerHost",
            "()Ljava/lang/String;",
        )
        .expect("getPeerHost registered");
    let port = r
        .find("javax/net/ssl/SSLSession", "getPeerPort", "()I")
        .expect("getPeerPort registered");

    for width in [6, 8] {
        let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), width);
        let h = ctx.create_string("example.test");
        ctx.set_field(sess, 3, Value::Object(Some(h)));
        ctx.set_field(sess, 4, Value::Int(8443));
        super::record_session_peer_endpoint(&ctx, sess, "wrong.test", 1);
        let this = &[Value::Object(Some(sess))];
        assert_eq!(
            host(&mut ctx, this).unwrap(),
            Some(Value::Object(Some(h))),
            "slot 3 wins at width {width}"
        );
        assert_eq!(port(&mut ctx, this).unwrap(), Some(Value::Int(8443)));
    }
}

/// "Nothing to say" must stay ABSENT, not become a recorded blank.
///
/// An absent row means "nobody recorded an endpoint", and both readers fall
/// THROUGH it to `session_stream_id` and the socket registry — which is the
/// only answer the `SSLSocketFactory.createSocket` client shape has. A row
/// of `("", -1)` would shadow that. HotSpot's never-connected pair
/// (`null`, `-1`) is measured in `session_peer_endpoint_table`'s table.
#[test]
fn an_empty_endpoint_is_not_recorded_at_all() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    super::record_session_peer_endpoint(&ctx, sess, "", -1);
    assert!(
        super::session_peer_endpoint(&ctx, sess).is_none(),
        "an empty host and a non-positive port say nothing, and a row that \
         says nothing shadows the socket-registry fallback"
    );
    super::record_session_peer_endpoint(&ctx, sess, "", 4711);
    assert_eq!(
        super::session_peer_endpoint(&ctx, sess),
        Some((String::new(), 4711)),
        "a port with no host is still an answer for getPeerPort"
    );
}

/// G51 — the SERVER session's own certificate chain.
///
/// MEASURED, `RSslLiveSession` on `9ae371468`: `server.localPrincipal =
/// null WANT CN=localhost`, `server.localPrincipal.class = null`, and
/// `server.localCertificates.length = -1 WANT 1`. All three read
/// `session_local_certs_table`, whose only writer before G51 was an
/// open-coded insert at the tail of `build_synthetic_ssl_session` — a
/// function `SSLServerSocket.accept()` never reaches.
///
/// The empty-chain contract is the other half and is NOT a tidiness rule:
/// `client.localCertificates = null` and `client.localPrincipal = null` are
/// measured GREEN rows for a client with no configured identity, and
/// `ssl_security`'s two readers answer `null` on an empty chain. A writer
/// that recorded an empty vector would turn those into `Certificate[0]`.
#[test]
fn the_local_chain_writer_records_a_chain_and_declines_an_empty_one() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    super::record_local_cert_chain(&ctx, sess, vec![]);
    assert!(
        super::local_certs_for_session(&ctx, sess).is_empty(),
        "an empty chain must leave the table empty: getLocalCertificates() \
         answers null there, which is the measured client-side row"
    );
    super::record_local_cert_chain(&ctx, sess, vec![vec![0x30, 0x82, 0x03]]);
    assert_eq!(
        super::local_certs_for_session(&ctx, sess),
        vec![vec![0x30u8, 0x82, 0x03]],
        "the accepted server session's local chain is what \
         getLocalCertificates() returns and what getLocalPrincipal() takes \
         its subject from — one fact, three rows"
    );
}

/// A dual-stack listener reports a loopback client as `::ffff:127.0.0.1`.
///
/// MEASURED on HotSpot (`scratchpad/g51/G51Probe.java`): the server-side
/// `getPeerHost()` is `127.0.0.1`, equal to the accepted socket's own
/// `getInetAddress().getHostAddress()`, and it is never reverse-resolved to
/// a name. A real IPv6 peer keeps its own spelling.
#[test]
fn an_ipv4_mapped_peer_address_is_reported_in_ipv4_spelling() {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    let mapped = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped(), 53114));
    assert_eq!(
        super::socket_addr_endpoint(mapped),
        ("127.0.0.1".to_string(), 53114)
    );
    let v4 = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 53114));
    assert_eq!(
        super::socket_addr_endpoint(v4),
        ("127.0.0.1".to_string(), 53114)
    );
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 53114));
    assert_eq!(
        super::socket_addr_endpoint(v6),
        ("::1".to_string(), 53114),
        "a genuine IPv6 peer is not an IPv4-mapped one and keeps its \
         spelling"
    );
}

/// The two peer-identity doors must see ONE chain.
///
/// `t27_tls::getPeerCertificates` and `ssl_security::getPeerPrincipal` are
/// registered by different registrars and used to read different sources —
/// the object-keyed table and the socket registry. HotSpot's contract makes
/// disagreement impossible by construction: MEASURED, the principal IS the
/// leaf certificate's subject
/// (`getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())`).
///
/// This pins the half a unit test can reach without a TLS peer: that the
/// chain an HTTPS client session was given via `record_client_peer_chain`
/// is visible through `peer_certs_for_session`, which is now the only
/// function either door consults. The width-4 HTTPS session carries
/// `HTTPS_CLIENT_SESSION_MARKER` in slot 2 and has NO socket-registry
/// entry, so before F18 the principal door found nothing here.
#[test]
fn the_object_keyed_chain_is_visible_to_the_one_resolver() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(
        sess,
        2,
        Value::Int(crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER),
    );
    assert!(
        super::peer_certs_for_session(&ctx, sess).is_empty(),
        "nothing recorded yet"
    );
    super::record_client_peer_chain(&ctx, sess, vec![vec![0x30, 0x82, 0x01]]);
    assert_eq!(
        super::peer_certs_for_session(&ctx, sess),
        vec![vec![0x30u8, 0x82, 0x01]],
        "the HTTPS client session's chain lives in the OBJECT-keyed table; \
         its slot 2 marker deliberately misses the socket registry, so a \
         resolver that consulted only the registry — which is what \
         getPeerPrincipal did — throws SSLPeerUnverifiedException on a \
         completed handshake. F10-1 NOMINATION 2"
    );
}

/// MUTATION CHECK for the test above: without it, `peer_certs_for_session`
/// could return a non-empty chain for anything and still pass. A session
/// nobody recorded a chain for must resolve to empty, because that is what
/// makes `getPeerCertificates`/`getPeerPrincipal` throw
/// `SSLPeerUnverifiedException` — the refusal `RSslNullSession` asserts.
#[test]
fn a_session_with_no_recorded_chain_resolves_to_empty() {
    use crate::test_utils::MockNativeContext;
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(sess, 2, Value::Int(-1));
    assert!(super::peer_certs_for_session(&ctx, sess).is_empty());
}

/// MUTATION CHECK for `a_widened_null_session_put_value_does_not_touch_the_stream_id`.
/// Without it, `putValue` could no-op
/// for EVERY shape and the collision test would still pass — the classic
/// "measured the refusal, called it coverage" shape. The 8-field engine
/// session has a dedicated attribute slot and must still round-trip.
#[test]
fn a_shape_with_a_real_attribute_slot_still_round_trips() {
    use crate::test_utils::MockNativeContext;
    let r = session_registry();
    let mut ctx = MockNativeContext::new();
    // 8-field engine shape: slot 2 = isValid flag, slot 7 = attrs.
    let sess = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(sess, 2, Value::Int(1));

    let name = ctx.create_string("k");
    let value = ctx.create_string("v");
    let put = r
        .find(
            "javax/net/ssl/SSLSession",
            "putValue",
            "(Ljava/lang/String;Ljava/lang/Object;)V",
        )
        .expect("putValue registered");
    put(
        &mut ctx,
        &[
            Value::Object(Some(sess)),
            Value::Object(Some(name)),
            Value::Object(Some(value)),
        ],
    )
    .expect("putValue must not fail");
    assert!(
        matches!(ctx.get_field(sess, 7), Value::Object(Some(_))),
        "slot 7 IS the attribute slot on the 8-field shape — putValue must \
         still store there, or `sslsess_attrs_slot` has disabled the API"
    );
    assert_eq!(
        ctx.get_field(sess, 2),
        Value::Int(1),
        "and it must not have touched the isValid flag"
    );
}

/// The width->slot rules, pinned as a table so a future shape has to
/// declare which convention it uses instead of inheriting one silently.
/// The 6-field row is the one that was wrong (`>= 7`): `tls.rs`'s and
/// `http2.rs`'s sessions are cipher-first like the 8-field engine shape,
/// not protocol-first like the narrow ones.
#[test]
fn the_session_slot_conventions_are_split_at_six_fields_not_seven() {
    for n in [0usize, 1] {
        assert_eq!(session_cipher_slot(n), None, "width {n} carries neither");
        assert_eq!(session_proto_slot(n), None, "width {n} carries neither");
    }
    for n in [2usize, 3, 4] {
        assert_eq!(
            session_proto_slot(n),
            Some(0),
            "width {n} is protocol-first"
        );
        assert_eq!(
            session_cipher_slot(n),
            Some(1),
            "width {n} is protocol-first"
        );
    }
    for n in [6usize, 8] {
        assert_eq!(session_cipher_slot(n), Some(0), "width {n} is cipher-first");
        assert_eq!(session_proto_slot(n), Some(1), "width {n} is cipher-first");
    }
}

// =====================================================================
// G25 - the int that was written into a reference slot.
// Record: docs/feature-designs/native-dispatch-architecture.md
//         G25-1-the-int-written-into-a-reference-slot-20260817.md
// =====================================================================

fn g25_state(bind_address: &str, bind_display: &str) -> SslServerSocketState {
    SslServerSocketState {
        listener_id: 7,
        local_port: 60553,
        closed: 0,
        bound: 1,
        bind_address: bind_address.to_string(),
        bind_display: bind_display.to_string(),
    }
}

/// The whole point of the change: nothing in `SSLServerSocket`'s lifecycle
/// is stored in an object slot any more, because none of the six slots
/// `java.net.ServerSocket` declares means what this file wanted to put
/// there. Slot 0 is `impl` - a REFERENCE - and an `Int` written there was
/// never readable, which is why `getImpl()` was null and every inherited
/// method NPE'd.
///
/// The test reads this file's own source, because the defect is a WRITE
/// that compiles, registers and silently does nothing: there is no runtime
/// observation of it from inside this crate, only the absence of the call.
#[test]
fn the_ssl_server_socket_lifecycle_is_never_written_into_an_object_slot() {
    let source = concat!(include_str!("../t27_tls/mod.rs"), "\n", include_str!("../t27_tls/runtime_tls_identity.rs"), "\n", include_str!("../t27_tls/cipher_provider_for.rs"), "\n", include_str!("../t27_tls/register_self_test.rs"), "\n", include_str!("../t27_tls/engine_session_for.rs"), "\n", include_str!("../t27_tls/release_manager_state_candidates.rs"));
    // Assembled from fragments so this test's own source does not contain
    // the needles it searches for - the same trick, and the same reason, as
    // `the_only_rustls_suite_spelling_left_is_the_adapters_own`.
    let write = format!("{}{}", "ctx.set_", "field(");
    for (receiver, slot) in [
        ("obj", "SSS_LISTENER_ID"),
        ("obj", "SSS_LOCAL_PORT"),
        ("obj", "SSS_CLOSED"),
        ("this", "SSS_LISTENER_ID"),
        ("this", "SSS_CLOSED"),
    ] {
        let needle = format!("{write}{receiver}, {slot}");
        assert!(
            !source.contains(&needle),
            "`{needle}` is back. Slots 0/1/2/3 of a real \
             javax.net.ssl.SSLServerSocket are impl (a REFERENCE) / created / \
             bound / closed; `ssl_server_socket_states` is the authority."
        );
    }
}

/// `isBound` / `getInetAddress` / `getLocalSocketAddress` / `toString` are
/// ONE contract. `java.net.ServerSocket.toString()` short-circuits to the
/// constant `"ServerSocket[unbound]"` while `isBound()` is false and
/// otherwise dereferences `impl`, so registering `isBound` without
/// `toString` walks the inherited body into a null `impl` and converts a
/// row that AGREED with HotSpot into an NPE. The mirror of
/// `net_phase_e`'s
/// `ssl_server_socket_bound_identity_rows_are_not_registered_piecemeal`,
/// from the side that now owns them.
#[test]
fn sss_bind_identity_rows_move_together() {
    let mut r = NativeMethodRegistry::new();
    super::register_sslserversocket(&mut r);
    let rows = [
        ("isBound", "()Z"),
        ("getInetAddress", "()Ljava/net/InetAddress;"),
        ("getLocalSocketAddress", "()Ljava/net/SocketAddress;"),
        ("toString", "()Ljava/lang/String;"),
    ];
    let mut present = 0usize;
    for (method, descriptor) in rows {
        if r.find("javax/net/ssl/SSLServerSocket", method, descriptor)
            .is_some()
        {
            present += 1;
        }
    }
    assert_eq!(
        present,
        rows.len(),
        "{present} of {} bind-identity rows registered; they are one contract",
        rows.len()
    );
}

/// The three rows measured as `AbstractMethodError` (`G16Sweep`), and the
/// setters that make their getters read-backs rather than constants.
#[test]
fn sss_abstract_method_error_rows_are_registered_with_their_setters() {
    let mut r = NativeMethodRegistry::new();
    super::register_sslserversocket(&mut r);
    for (method, descriptor) in [
        ("getEnabledCipherSuites", "()[Ljava/lang/String;"),
        ("getSupportedCipherSuites", "()[Ljava/lang/String;"),
        ("setEnabledCipherSuites", "([Ljava/lang/String;)V"),
        ("getUseClientMode", "()Z"),
        ("setUseClientMode", "(Z)V"),
        ("getEnableSessionCreation", "()Z"),
        ("setEnableSessionCreation", "(Z)V"),
    ] {
        assert!(
            r.find("javax/net/ssl/SSLServerSocket", method, descriptor)
                .is_some(),
            "SSLServerSocket.{method}{descriptor} is abstract with no Code \
             attribute; without a native it raises AbstractMethodError"
        );
    }
}

/// The other half of `net_phase_e`'s
/// `ssl_server_socket_option_registrar_leaves_the_t27_owned_names_alone`,
/// asserted from this side. `register_t27_natives` runs AFTER
/// `register_phase_e_networking` (lib.rs 18731 vs 18688) and `register()`
/// is last-write-wins with no unregister API, so a name added here that
/// RE.6b already owns does not conflict - it silently deletes RE.6b's
/// body, and with it the delegate that makes `setSoTimeout` work at all.
/// `accept()` now ASKS one of those nine through `invoke_virtual`, so
/// shadowing `getSoTimeout` here would also make the accept timeout read
/// its own answer.
#[test]
fn t27_does_not_shadow_the_re6b_option_surface() {
    let mut r = NativeMethodRegistry::new();
    super::register_sslserversocket(&mut r);
    for (method, descriptor) in [
        ("getSoTimeout", "()I"),
        ("setSoTimeout", "(I)V"),
        ("getReuseAddress", "()Z"),
        ("setReuseAddress", "(Z)V"),
        ("getReceiveBufferSize", "()I"),
        ("setReceiveBufferSize", "(I)V"),
        ("supportedOptions", "()Ljava/util/Set;"),
        ("getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
        (
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/ServerSocket;",
        ),
    ] {
        assert!(
            r.find("javax/net/ssl/SSLServerSocket", method, descriptor)
                .is_none(),
            "SSLServerSocket.{method}{descriptor} belongs to net_phase_e's \
             RE.6b; registering it here runs later and kills that body"
        );
    }
}

/// Both renderings, transcribed from HotSpot 25.0.3+9 (`G25Probe`).
#[test]
fn sss_to_string_matches_the_oracle() {
    assert_eq!(
        sss_to_string(Some(&g25_state("127.0.0.1", "/127.0.0.1"))),
        "[SSL: ServerSocket[addr=/127.0.0.1,localport=60553]]"
    );
    assert_eq!(
        sss_to_string(Some(&g25_state(SSS_WILDCARD_BIND, SSS_WILDCARD_DISPLAY))),
        "[SSL: ServerSocket[addr=0.0.0.0/0.0.0.0,localport=60553]]"
    );
    assert_eq!(sss_to_string(None), "ServerSocket[unbound]");
}

/// MEASURED: `close()` moves `isClosed()` and NOTHING else. Every bind row
/// answers after close exactly what it answered before.
#[test]
fn closing_a_server_socket_does_not_unbind_it() {
    let open = g25_state("127.0.0.1", "/127.0.0.1");
    let closed = sss_closed_state(open.clone());
    assert_eq!(closed.closed, 1);
    assert_eq!(closed.listener_id, -1, "the listener is gone");
    assert_eq!(closed.local_port, open.local_port);
    assert_eq!(closed.bind_address, open.bind_address);
    assert_eq!(closed.bind_display, open.bind_display);
    assert_eq!(sss_to_string(Some(&closed)), sss_to_string(Some(&open)));
}

/// A socket with no record is not a socket that is merely unconfigured:
/// it is one this file cannot accept on, and the miss answers say so.
#[test]
fn an_unknown_server_socket_reads_closed_and_unbound() {
    let miss = ssl_server_socket_state_miss();
    assert_eq!(miss.closed, 1);
    assert_eq!(miss.listener_id, -1);
    assert_eq!(miss.local_port, 0);
    assert_eq!(sss_to_string(None), "ServerSocket[unbound]");
}

/// MEASURED (`G25Probe`) on an `SSLServerSocket` nobody has configured.
#[test]
fn sss_mode_defaults_are_the_measured_hotspot_values() {
    assert_eq!(SSS_MODE_DEFAULT.0, 0, "getUseClientMode on a server socket");
    assert_eq!(SSS_MODE_DEFAULT.1, 1, "getEnableSessionCreation");
}

/// NOM-6. MEASURED, HotSpot 25.0.3+9 (`G25Probe`): the packet size is a
/// constant in every state, the application size is not - 16704 before a
/// handshake and 16676 after a TLS 1.3 one, under both AES-128-GCM and
/// AES-256-GCM. 16384 (the old answer) is neither; it is the RFC 8446
/// section 5.1 TLSPlaintext cap.
#[test]
fn the_session_buffer_sizes_are_two_states_not_one_constant() {
    assert_eq!(JSSE_PACKET_BUFFER_SIZE, 16709);
    assert_eq!(JSSE_APPLICATION_BUFFER_SIZE_FRESH, 16704);
    assert_eq!(JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED, 16676);
    assert_eq!(
        JSSE_APPLICATION_BUFFER_SIZE_FRESH,
        JSSE_PACKET_BUFFER_SIZE - 5,
        "the fresh value is the packet size less the 5-byte record header"
    );
    assert!(
        JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED < JSSE_APPLICATION_BUFFER_SIZE_FRESH,
        "negotiating a suite costs record expansion, so the app buffer shrinks"
    );
    assert_ne!(
        JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED, 16384,
        "16384 is the RFC 8446 plaintext cap, not the JDK's answer"
    );
    // The audit that made raising this safe: `do_wrap` never consumes more
    // than 16384 bytes of plaintext per call whatever the caller offers, so
    // one record can never exceed the packet buffer.
    let source = concat!(include_str!("../t27_tls/mod.rs"), "\n", include_str!("../t27_tls/runtime_tls_identity.rs"), "\n", include_str!("../t27_tls/cipher_provider_for.rs"), "\n", include_str!("../t27_tls/register_self_test.rs"), "\n", include_str!("../t27_tls/engine_session_for.rs"), "\n", include_str!("../t27_tls/release_manager_state_candidates.rs"));
    // Fragments again: a literal here would match ITSELF and the assertion
    // would hold whether or not `do_wrap` still caps.
    let cap = format!("{}{}", "bb_read_into(ctx, *bb, &mut app_bytes, ", "16384)");
    assert!(
        source.contains(&cap),
        "do_wrap's 16384 plaintext cap is what bounds one record below \
         getPacketBufferSize(); without it the larger app buffer can \
         livelock a BUFFER_OVERFLOW retry loop"
    );
}
