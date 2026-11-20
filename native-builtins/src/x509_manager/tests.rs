//! Hermetic tests built against a hand-rolled X.509 fixture. We never
//! shell out to `openssl` — every cert byte here is produced by the
//! `mk_cert` builder below so the suite passes on any host.
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;

/// gc-common w9-b (`handoff-w9b-x509-key-manager-state-per-vm.md`): a
/// `KeyManagerState`'s live keys are rooted, remapped and forgotten only by
/// the VM that published it.
#[test]
fn w9b_key_manager_states_belong_to_their_vm() {
    const VM_A: usize = 0x9B5E_0A01;
    const VM_B: usize = 0x9B5E_0A02;
    // Negative ids: `next_km_id` only hands out positive ones.
    const ID_A: i32 = -0x9B01;
    const ID_B: i32 = -0x9B02;
    let (addr_a, addr_b) = (0x9B5E_1000usize, 0x9B5E_2000usize);
    // SAFETY: never dereferenced; only the address is compared.
    let key = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let state = |vm: usize, a: usize| {
        let mut s = KeyManagerState {
            vm,
            ..Default::default()
        };
        s.aliases_to_live_key.insert("k".into(), key(a));
        s
    };
    km_registry().write().insert(ID_A, state(VM_A, addr_a));
    km_registry().write().insert(ID_B, state(VM_B, addr_b));
    let scan = |vm: usize| {
        let mut roots = Vec::new();
        gc_scan_key_manager_roots(vm, &mut roots);
        roots
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect::<Vec<usize>>()
    };
    assert!(scan(VM_A).contains(&addr_a) && !scan(VM_A).contains(&addr_b));
    assert!(scan(VM_B).contains(&addr_b) && !scan(VM_B).contains(&addr_a));

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(addr_a, addr_a + 0x100);
    gc_update_key_manager_refs(VM_B, &map);
    let live_a = |id: i32| {
        km_registry()
            .read()
            .get(&id)
            .map(|s| s.aliases_to_live_key["k"].as_ptr() as usize)
    };
    assert_eq!(
        live_a(ID_A),
        Some(addr_a),
        "B's map must not rewrite A's key"
    );

    forget_vm_key_manager_states(VM_A);
    assert!(km_registry().read().get(&ID_A).is_none());
    assert!(km_registry().read().get(&ID_B).is_some());
    forget_vm_key_manager_states(VM_B);
    assert!(km_registry().read().get(&ID_B).is_none());
}

/// gc-common w10-e: the manager / factory / keystore rows a VM registers
/// are keyed by that VM where they are keyed by identity hash, and are
/// dropped by that VM's teardown and no other's.
#[test]
fn w10e_manager_factory_and_keystore_rows_belong_to_their_vm() {
    const VM_A: usize = 0xE10C_0001;
    const VM_B: usize = 0xE10C_0002;
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            for vm in [VM_A, VM_B] {
                forget_vm_trust_manager_states(vm);
                forget_vm_key_manager_states(vm);
                crate::keystore::forget_vm_keystores(vm);
                crate::phases_late::ssl_security::forget_vm_factory_identity_rows(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }
    let _teardown = Teardown;
    // One object address -- so one identity hash -- in both VMs (gc-common
    // w27-b: the rows are keyed by weak lock keys, which are per VM).
    // SAFETY: never dereferenced; the mock hashes the address.
    let obj = unsafe { ObjectRef::from_raw(0x0E10_C100usize as *mut u8) };
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    file_manager_id_row(&a, km_id_by_identity(), obj, 71);
    assert_eq!(
        lookup_manager_id_row(&b, km_id_by_identity(), obj),
        None,
        "B's KeyManager must not resolve A's id"
    );
    file_manager_id_row(&a, tm_id_by_identity(), obj, 72);
    file_manager_id_row(&b, tm_id_by_identity(), obj, 75);
    let kmf = crate::phases_late::ssl_security::kmf_keystore_id_by_identity;
    let tmf = crate::phases_late::ssl_security::tmf_tm_id_by_identity;
    // gc-common w27-a: the `ssl_security` factory rows are keyed `(VM, weak
    // lock key)`; any number stands in.
    let fk: u64 = 0x0E10_C100;
    kmf().lock().insert((VM_A, fk), 73);
    kmf().lock().insert((VM_B, fk), 74);
    tmf().lock().insert((VM_B, fk), 75);

    let tm_a = register_trust_manager_state_in_vm(VM_A, TrustManagerState::default());
    let tm_b = register_trust_manager_state_in_vm(VM_B, TrustManagerState::default());
    let ks_a = crate::keystore::keystore_register_in_vm(VM_A, Default::default());
    let ks_b = crate::keystore::keystore_register_in_vm(VM_B, Default::default());

    forget_vm_trust_manager_states(VM_A);
    forget_vm_key_manager_states(VM_A);
    crate::keystore::forget_vm_keystores(VM_A);
    crate::phases_late::ssl_security::forget_vm_factory_identity_rows(VM_A);

    assert!(tm_registry().read().get(&tm_a).is_none());
    assert_eq!(tm_registry().read().get(&tm_b).map(|s| s.vm), Some(VM_B));
    assert_eq!(lookup_manager_id_row(&a, km_id_by_identity(), obj), None);
    assert_eq!(lookup_manager_id_row(&a, tm_id_by_identity(), obj), None);
    assert_eq!(
        lookup_manager_id_row(&b, tm_id_by_identity(), obj),
        Some(75)
    );
    assert!(crate::keystore::keystore_lookup(ks_a).is_none());
    assert!(crate::keystore::keystore_lookup(ks_b).is_some());
    assert_eq!(kmf().lock().get(&(VM_A, fk)), None);
    assert_eq!(kmf().lock().get(&(VM_B, fk)), Some(&74));
    assert_eq!(tmf().lock().get(&(VM_B, fk)), Some(&75));
}

/// Tiny DER builder — same shape as `keystore.rs` but kept private so the
/// two modules don't drift on test fixtures.
fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    encode_length(body.len(), &mut out);
    out.extend_from_slice(body);
    out
}

fn encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= 0xFF {
        out.push(0x81);
        out.push(len as u8);
    } else if len <= 0xFFFF {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push(len as u8);
    } else {
        out.push(0x83);
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push(len as u8);
    }
}

fn der_oid(body: &[u8]) -> Vec<u8> {
    der_tlv(TAG_OID, body)
}

fn der_seq(body: Vec<u8>) -> Vec<u8> {
    der_tlv(TAG_SEQUENCE, &body)
}

fn der_set(body: Vec<u8>) -> Vec<u8> {
    der_tlv(TAG_SET, &body)
}

fn der_int(value: u8) -> Vec<u8> {
    der_tlv(TAG_INTEGER, &[value])
}

fn der_utctime(s: &str) -> Vec<u8> {
    der_tlv(TAG_UTC_TIME, s.as_bytes())
}

fn der_bit_string(unused: u8, body: &[u8]) -> Vec<u8> {
    let mut full = Vec::with_capacity(body.len() + 1);
    full.push(unused);
    full.extend_from_slice(body);
    der_tlv(TAG_BIT_STRING, &full)
}

fn der_octet(body: &[u8]) -> Vec<u8> {
    der_tlv(TAG_OCTET_STRING, body)
}

fn der_bool(b: bool) -> Vec<u8> {
    der_tlv(TAG_BOOLEAN, &[if b { 0xff } else { 0x00 }])
}

fn der_context_explicit(num: u8, body: &[u8]) -> Vec<u8> {
    let tag = 0xa0 | (num & 0x0f);
    der_tlv(tag, body)
}

/// Build a v3 cert with the supplied extensions.  This is *not* a valid
/// signed cert (the signature is a fixed dummy) but exercises every
/// field of our parser.  The KU bit-string convention here is "high bit
/// first" — unused bits are the trailing `8 - keep` bits of the last
/// byte.
struct CertSpec {
    not_before_utc: &'static str,
    not_after_utc: &'static str,
    subject_cn: &'static str,
    issuer_cn: &'static str,
    spki_alg: &'static [u8],
    key_usage_bits: Option<u16>,
    ext_key_usages: &'static [&'static [u8]],
    basic_constraints_ca: Option<bool>,
    /// dNSName SubjectAltName entries to embed (empty = no SAN ext).
    subject_alt_dns: &'static [&'static str],
}

impl Default for CertSpec {
    fn default() -> Self {
        CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf.example.com",
            issuer_cn: "Acme CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        }
    }
}

/// Encode a SubjectAltName extension value containing the given dNSName
/// entries: SEQUENCE OF GeneralName, each `[2] IMPLICIT IA5String`.
fn san_dns_value(names: &[&str]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for n in names {
        body.extend_from_slice(&der_tlv(SAN_TAG_DNS_NAME, n.as_bytes()));
    }
    der_seq(body)
}

fn name_with_cn(cn: &str) -> Vec<u8> {
    // SEQUENCE { SET { SEQUENCE { OID 2.5.4.3, PrintableString } } }
    let cn_oid = der_oid(&[0x55, 0x04, 0x03]);
    let cn_val = der_tlv(0x13, cn.as_bytes());
    let atv = der_seq([cn_oid, cn_val].concat());
    let rdn = der_set(atv);
    der_seq(rdn)
}

fn spki(alg_oid: &[u8]) -> Vec<u8> {
    // SubjectPublicKeyInfo ::= SEQUENCE { AlgorithmIdentifier, BIT STRING }
    let alg = der_seq([der_oid(alg_oid), der_tlv(TAG_NULL, &[])].concat());
    let bs = der_bit_string(0, &[0x00, 0x01]);
    der_seq([alg, bs].concat())
}

fn extension(oid: &[u8], critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut parts: Vec<u8> = Vec::new();
    parts.extend_from_slice(&der_oid(oid));
    if critical {
        parts.extend_from_slice(&der_bool(true));
    }
    parts.extend_from_slice(&der_octet(&value));
    der_seq(parts)
}

fn ku_bitstring(bits: u16) -> Vec<u8> {
    // Pack the lowest 9 bits into a BIT STRING per RFC 5280: bit 0 (MSB)
    // = digitalSignature, bit 8 = decipherOnly.  Find the highest bit
    // position that is set so we can choose body length + unused-bits
    // count correctly (per DER, the unused-bits count must let the
    // receiver recover *exactly* the bits that were set by the issuer).
    let mut value: u16 = 0;
    let mut highest_set: i32 = -1;
    for pos in 0..9 {
        if (bits >> pos) & 1 == 1 {
            value |= 1 << (15 - pos);
            if pos as i32 > highest_set {
                highest_set = pos as i32;
            }
        }
    }
    if highest_set < 0 {
        return der_bit_string(7, &[0x00]);
    }
    let needed_bits = (highest_set + 1) as usize;
    // Round up to whole bytes.
    let body_len = (needed_bits + 7) / 8;
    let unused = (body_len * 8 - needed_bits) as u8;
    let bytes = [(value >> 8) as u8, value as u8];
    let body = &bytes[..body_len];
    der_bit_string(unused, body)
}

fn eku_seq(oids: &[&[u8]]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for o in oids {
        body.extend_from_slice(&der_oid(o));
    }
    der_seq(body)
}

fn bc_seq(ca: bool) -> Vec<u8> {
    if ca {
        der_seq(der_bool(true))
    } else {
        der_seq(Vec::new())
    }
}

fn mk_cert(spec: &CertSpec) -> Vec<u8> {
    let mut tbs: Vec<u8> = Vec::new();
    // version [0] EXPLICIT INTEGER 2 (i.e. v3)
    tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
    // serial
    tbs.extend_from_slice(&der_int(1));
    // signature alg
    tbs.extend_from_slice(&der_seq(
        [
            der_oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
            der_tlv(TAG_NULL, &[]),
        ]
        .concat(),
    ));
    // issuer
    tbs.extend_from_slice(&name_with_cn(spec.issuer_cn));
    // validity
    tbs.extend_from_slice(&der_seq(
        [
            der_utctime(spec.not_before_utc),
            der_utctime(spec.not_after_utc),
        ]
        .concat(),
    ));
    // subject
    tbs.extend_from_slice(&name_with_cn(spec.subject_cn));
    // SPKI
    tbs.extend_from_slice(&spki(spec.spki_alg));
    // extensions
    let mut exts: Vec<u8> = Vec::new();
    if let Some(bits) = spec.key_usage_bits {
        exts.extend_from_slice(&extension(OID_EXT_KEY_USAGE, true, ku_bitstring(bits)));
    }
    if !spec.ext_key_usages.is_empty() {
        exts.extend_from_slice(&extension(
            OID_EXT_EXTENDED_KEY_USAGE,
            false,
            eku_seq(spec.ext_key_usages),
        ));
    }
    if let Some(ca) = spec.basic_constraints_ca {
        exts.extend_from_slice(&extension(OID_EXT_BASIC_CONSTRAINTS, true, bc_seq(ca)));
    }
    if !spec.subject_alt_dns.is_empty() {
        exts.extend_from_slice(&extension(
            OID_EXT_SUBJECT_ALT_NAME,
            false,
            san_dns_value(spec.subject_alt_dns),
        ));
    }
    if !exts.is_empty() {
        tbs.extend_from_slice(&der_context_explicit(3, &der_seq(exts)));
    }

    // Build outer cert: SEQUENCE { tbs, sigAlg, signature }
    let outer_sig_alg = der_seq(
        [
            der_oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
            der_tlv(TAG_NULL, &[]),
        ]
        .concat(),
    );
    let dummy_sig = der_bit_string(0, &[0xde, 0xad, 0xbe, 0xef]);
    der_seq([der_seq(tbs), outer_sig_alg, dummy_sig].concat())
}

#[test]
fn parse_minimal_v3_cert() {
    let cert = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "leaf.example.com",
        issuer_cn: "Acme CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let p = parse_certificate(&cert).expect("parse");
    assert_eq!(p.spki_algorithm_oid, OID_RSA);
    assert_eq!(p.basic_constraints_ca, Some(false));
    let ku = p.key_usage.expect("ku");
    assert!(ku & KU_DIGITAL_SIGNATURE != 0);
    assert!(ku & KU_KEY_ENCIPHERMENT != 0);
    assert_eq!(p.ext_key_usage.len(), 1);
    assert_eq!(p.ext_key_usage[0], OID_KP_SERVER_AUTH);
}

#[test]
fn classify_key_type_recognises_known_algorithms() {
    assert_eq!(classify_key_type(OID_RSA), "RSA");
    assert_eq!(classify_key_type(OID_EC), "EC");
    assert_eq!(classify_key_type(OID_DSA), "DSA");
    assert_eq!(classify_key_type(OID_ED25519), "Ed25519");
    assert_eq!(classify_key_type(&[0x01, 0x02]), "UNKNOWN");
}

#[test]
fn server_cert_prefers_eku_serverauth_when_eku_present() {
    let cert_ok = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "ok.example",
        issuer_cn: "CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let cert_bad = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "bad.example",
        issuer_cn: "CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_CLIENT_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    assert!(is_server_cert(&parse_certificate(&cert_ok).unwrap()));
    assert!(!is_server_cert(&parse_certificate(&cert_bad).unwrap()));
}

#[test]
fn client_cert_prefers_eku_clientauth_when_eku_present() {
    let cert_ok = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "client.example",
        issuer_cn: "CA",
        spki_alg: OID_EC,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_CLIENT_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let cert_bad = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "wrong.example",
        issuer_cn: "CA",
        spki_alg: OID_EC,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    assert!(is_client_cert(&parse_certificate(&cert_ok).unwrap()));
    assert!(!is_client_cert(&parse_certificate(&cert_bad).unwrap()));
}

#[test]
fn cert_without_eku_is_preferred_for_both_roles() {
    let cert = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "any.example",
        issuer_cn: "CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
        ext_key_usages: &[],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let p = parse_certificate(&cert).unwrap();
    assert!(is_server_cert(&p));
    assert!(is_client_cert(&p));
}

/// An EKU that names only `serverAuth` must still yield a CLIENT alias.
///
/// `SunX509KeyManagerImpl` — the default `KeyManagerFactory` algorithm —
/// filters aliases on the key algorithm and the peer's issuer list only, so
/// on JDK 25 this exact certificate answers `chooseClientAlias(RSA)=key`.
/// Treating [`is_client_cert`] as an eligibility test instead of a
/// preference made `getClientAliases(RSA)` answer `[]` and
/// `chooseClientAlias(RSA)` answer `null`, so netty's OPENSSL client sent no
/// certificate at all and a `ClientAuth.REQUIRE` server closed the
/// handshake with `SSLV3_ALERT_HANDSHAKE_FAILURE` — 14 of the 17 residual
/// rows of `JdkDelegatingPrivateKeyMethodTest`, whose fixture builds
/// `.setKeyUsage(true, digitalSignature).addExtendedKeyUsageServerAuth()`
/// and then uses that one cert on BOTH sides.
#[test]
fn an_eku_mismatch_orders_an_alias_last_but_never_drops_it() {
    let server_only = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "serveronly.example",
        issuer_cn: "serveronly.example",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let both = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "both.example",
        issuer_cn: "both.example",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH, OID_KP_CLIENT_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });

    // A store holding ONLY the serverAuth-marked cert: the client list must
    // still name it. This is the shape the netty fixture builds.
    let mut only = crate::keystore::LoadedKeyStore::default();
    only.entries.insert(
        "key".to_string(),
        crate::keystore::KeyStoreEntry {
            alias: "key".to_string(),
            creation_time_ms: 0,
            kind: crate::keystore::EntryKind::PrivateKey {
                key_der: vec![0x30, 0x00],
                chain: vec![server_only.clone()],
                protected: None,
            },
        },
    );
    let st = build_key_manager_state(crate::keystore::keystore_register(only));
    assert_eq!(
        st.client_aliases_by_key_type.get("RSA").map(Vec::as_slice),
        Some(&["key".to_string()][..]),
        "a serverAuth-only cert must still be offered as a client alias"
    );
    assert_eq!(
        st.server_aliases_by_key_type.get("RSA").map(Vec::as_slice),
        Some(&["key".to_string()][..])
    );

    // With BOTH in one store the properly-marked one must come first, so a
    // caller taking `.first()` still prefers it.
    let mut two = crate::keystore::LoadedKeyStore::default();
    for (alias, der) in [("aserver", &server_only), ("zboth", &both)] {
        two.entries.insert(
            alias.to_string(),
            crate::keystore::KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: 0,
                kind: crate::keystore::EntryKind::PrivateKey {
                    key_der: vec![0x30, 0x00],
                    chain: vec![der.clone()],
                    protected: None,
                },
            },
        );
    }
    let st2 = build_key_manager_state(crate::keystore::keystore_register(two));
    let clients = st2
        .client_aliases_by_key_type
        .get("RSA")
        .expect("both aliases are RSA");
    assert_eq!(
        clients.first().map(String::as_str),
        Some("zboth"),
        "the clientAuth-marked alias must be preferred: got {clients:?}"
    );
    assert_eq!(
        clients.len(),
        2,
        "neither alias may be dropped: {clients:?}"
    );
}

#[test]
fn validate_chain_rejects_expired_leaf() {
    let leaf = mk_cert(&CertSpec {
        not_before_utc: "990101000000Z",
        not_after_utc: "000101000000Z", // expired in 2000
        subject_cn: "expired",
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let anchor_der = mk_cert(&CertSpec {
        not_before_utc: "990101000000Z",
        not_after_utc: "490101000000Z",
        subject_cn: "Anchor",
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_KEY_CERT_SIGN),
        ext_key_usages: &[],
        basic_constraints_ca: Some(true),
        subject_alt_dns: &[],
    });
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, anchor_der);
    let chain = vec![leaf];
    let result = validate_chain(&chain, &trust);
    match result {
        Err(TrustError::Expired { .. }) => {}
        other => panic!("expected Expired, got {:?}", other),
    }
}

#[test]
fn validate_chain_accepts_an_expired_certificate_that_is_the_anchor_itself() {
    // netty's `testMutualAuthDiffCerts` shape: a self-signed certificate
    // that expired long ago, installed as the ONLY trust anchor and then
    // presented by the peer as its own identity. RFC 5280 §6.1 validates
    // the certificates on the path against the anchor; the anchor is not
    // on the path, so its validity period is not one of the inputs. JSSE
    // completes this handshake.
    let expired_anchor = mk_cert(&CertSpec {
        not_before_utc: "990101000000Z",
        not_after_utc: "000101000000Z", // expired in 2000
        subject_cn: "self-signed-and-trusted",
        issuer_cn: "self-signed-and-trusted",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, expired_anchor.clone());
    validate_chain(&[expired_anchor.clone()], &trust)
        .expect("a presented certificate that IS the anchor is not path-validated");

    // The exemption is by full DER, not by subject: a DIFFERENT expired
    // certificate with the same subject is still expired.
    let impostor = mk_cert(&CertSpec {
        not_before_utc: "990101000000Z",
        not_after_utc: "000101000000Z",
        subject_cn: "self-signed-and-trusted",
        issuer_cn: "self-signed-and-trusted",
        spki_alg: OID_EC, // different SPKI => different DER
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    match validate_chain(&[impostor], &trust) {
        Err(TrustError::Expired { .. }) => {}
        other => panic!(
            "a same-subject impostor must still be Expired, got {:?}",
            other
        ),
    }
}

#[test]
fn validate_chain_rejects_when_no_trust_anchor() {
    let leaf = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "leaf",
        issuer_cn: "Stranger",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let trust = TrustManagerState::default();
    let result = validate_chain(&[leaf], &trust);
    match result {
        Err(TrustError::NoTrustAnchor) => {}
        other => panic!("expected NoTrustAnchor, got {:?}", other),
    }
}

#[test]
fn validate_chain_passes_self_signed_in_trust() {
    let cert = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "Anchor",
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[],
        basic_constraints_ca: Some(true),
        subject_alt_dns: &[],
    });
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, cert.clone());
    validate_chain(&[cert], &trust).expect("should validate");
}

#[test]
fn validate_chain_rejects_broken_continuity() {
    let leaf = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "leaf",
        issuer_cn: "Real CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let intermediate = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "Different CA", // mismatch — not the issuer of leaf
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_KEY_CERT_SIGN),
        ext_key_usages: &[],
        basic_constraints_ca: Some(true),
        subject_alt_dns: &[],
    });
    let anchor = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "Anchor",
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_KEY_CERT_SIGN),
        ext_key_usages: &[],
        basic_constraints_ca: Some(true),
        subject_alt_dns: &[],
    });
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, anchor);
    let result = validate_chain(&[leaf, intermediate], &trust);
    match result {
        Err(TrustError::BrokenChain { .. }) => {}
        other => panic!("expected BrokenChain, got {:?}", other),
    }
}

#[test]
fn validate_chain_rejects_intermediate_without_ca_flag() {
    let leaf = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "leaf",
        issuer_cn: "Bogus Intermediate",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let bogus = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "Bogus Intermediate",
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[],
        basic_constraints_ca: Some(false), // explicitly NOT a CA
        subject_alt_dns: &[],
    });
    let anchor = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "Anchor",
        issuer_cn: "Anchor",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_KEY_CERT_SIGN),
        ext_key_usages: &[],
        basic_constraints_ca: Some(true),
        subject_alt_dns: &[],
    });
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, anchor);
    match validate_chain(&[leaf, bogus], &trust) {
        Err(TrustError::NotCa { .. }) => {}
        other => panic!("expected NotCa, got {:?}", other),
    }
}

#[test]
fn rustls_native_certs_loads() {
    // Sanity check: the platform call returns *something*. Even on a
    // stripped-down container this should yield either certs or
    // documented errors, never a panic.
    let result = rustls_native_certs::load_native_certs();
    let _ = result.certs.len();
    let _ = result.errors.len();
}

#[test]
fn build_trust_manager_state_includes_system_roots() {
    // No user keystore, just system anchors. We expect a non-zero number
    // of accepted issuers on every supported platform — but we do not
    // assert > 0 because some sandboxed CI environments strip the OS
    // trust store. We assert no panic and that the structure is
    // internally consistent: anchor_ders.len() >= anchors.len().
    let s = build_trust_manager_state(0);
    assert!(s.anchor_ders.len() >= s.anchors.len());
    assert_eq!(s.keystore_id, 0);
}

#[test]
fn build_trust_manager_state_custom_store_is_restrictive() {
    let custom_anchor = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "Custom Only Root",
        issuer_cn: "Custom Only Root",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_KEY_CERT_SIGN),
        ext_key_usages: &[],
        basic_constraints_ca: Some(true),
        subject_alt_dns: &[],
    });
    let store = keystore::LoadedKeyStore {
        entries: [(
            "custom-root".to_string(),
            keystore::KeyStoreEntry {
                alias: "custom-root".to_string(),
                creation_time_ms: 0,
                kind: keystore::EntryKind::TrustedCert {
                    cert_der: custom_anchor.clone(),
                },
            },
        )]
        .into_iter()
        .collect(),
    };
    let id = keystore::keystore_register(store);
    let state = build_trust_manager_state(id);
    assert_eq!(state.keystore_id, id);
    assert_eq!(state.anchor_ders, vec![custom_anchor]);
    assert_eq!(state.anchors.len(), 1);
}

#[test]
fn key_usage_bit_extraction_is_correct() {
    let cert = mk_cert(&CertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "x",
        issuer_cn: "y",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_AGREEMENT),
        ext_key_usages: &[],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &[],
    });
    let ku = parse_certificate(&cert).unwrap().key_usage.unwrap();
    assert_eq!(ku & KU_DIGITAL_SIGNATURE, KU_DIGITAL_SIGNATURE);
    assert_eq!(ku & KU_KEY_AGREEMENT, KU_KEY_AGREEMENT);
    assert_eq!(ku & KU_KEY_ENCIPHERMENT, 0);
}

#[test]
fn classify_key_type_from_pkcs8_for_rsa() {
    // Minimal PKCS#8: SEQUENCE { INTEGER 0, AlgorithmIdentifier{rsa,
    // NULL}, OCTET STRING ... }. We only need the OID match.
    let pkcs8 = der_seq(
        [
            der_int(0),
            der_seq([der_oid(OID_RSA), der_tlv(TAG_NULL, &[])].concat()),
            der_octet(&[0u8; 4]),
        ]
        .concat(),
    );
    assert_eq!(classify_key_type_from_pkcs8(&pkcs8), "RSA");

    let pkcs8_ec = der_seq(
        [
            der_int(0),
            der_seq([der_oid(OID_EC), der_tlv(TAG_NULL, &[])].concat()),
            der_octet(&[0u8; 4]),
        ]
        .concat(),
    );
    assert_eq!(classify_key_type_from_pkcs8(&pkcs8_ec), "EC");
}

// ====================================================================
// Real-signature chain-verification tests (RSA-SHA256 and ECDSA-P256).
//
// These build a self-consistent toy CA in-memory: anchor + leaf are
// signed with a real generated keypair, then validate_chain re-verifies
// them through the same primitives. No openssl / no fixtures.
// ====================================================================

use crate::crypto_impl::{
    Ecdsa, EcdsaPrivateKey, EcdsaPublicKey, Rsa, RsaPrivateKey, RsaPublicKey, Sha256,
};

/// Variant of `CertSpec` that takes a *real* SPKI DER (already encoded
/// by `Rsa::public_key_to_der` / `Ecdsa::public_key_to_der`) instead of
/// a fake algorithm-OID-only SPKI. We also accept the issuer's signing
/// algorithm (the cert's `signatureAlgorithm`) so RSA and ECDSA chains
/// share the same builder.
struct SignedCertSpec<'a> {
    not_before_utc: &'static str,
    not_after_utc: &'static str,
    subject_cn: &'static str,
    issuer_cn: &'static str,
    spki_der: &'a [u8],
    sig_alg_oid: &'static [u8],
    key_usage_bits: Option<u16>,
    ext_key_usages: &'static [&'static [u8]],
    basic_constraints_ca: Option<bool>,
}

/// Build the *tbsCertificate* DER for a SignedCertSpec. Used both to
/// produce the bytes the issuer signs and (after the signature is
/// computed) to assemble the final SEQUENCE { tbs, sigAlg, sigValue }.
fn build_tbs(spec: &SignedCertSpec) -> Vec<u8> {
    build_tbs_with_alg(spec, &sig_alg_seq(spec.sig_alg_oid))
}

/// `build_tbs` with the whole `AlgorithmIdentifier` supplied, for the
/// algorithms whose parameters are not ASN.1 NULL (RSASSA-PSS).
fn build_tbs_with_alg(spec: &SignedCertSpec, sig_alg_der: &[u8]) -> Vec<u8> {
    let mut tbs: Vec<u8> = Vec::new();
    // version [0] EXPLICIT INTEGER 2 (v3)
    tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
    // serial
    tbs.extend_from_slice(&der_int(1));
    // signature alg (this MUST match the outer sigAlg byte-for-byte;
    // RFC 5280 §4.1.1.2 requires it)
    let sig_alg_seq = sig_alg_der.to_vec();
    tbs.extend_from_slice(&sig_alg_seq);
    // issuer
    tbs.extend_from_slice(&name_with_cn(spec.issuer_cn));
    // validity
    tbs.extend_from_slice(&der_seq(
        [
            der_utctime(spec.not_before_utc),
            der_utctime(spec.not_after_utc),
        ]
        .concat(),
    ));
    // subject
    tbs.extend_from_slice(&name_with_cn(spec.subject_cn));
    // SPKI — already DER-encoded by the keypair serializer
    tbs.extend_from_slice(spec.spki_der);
    // extensions
    let mut exts: Vec<u8> = Vec::new();
    if let Some(bits) = spec.key_usage_bits {
        exts.extend_from_slice(&extension(OID_EXT_KEY_USAGE, true, ku_bitstring(bits)));
    }
    if !spec.ext_key_usages.is_empty() {
        exts.extend_from_slice(&extension(
            OID_EXT_EXTENDED_KEY_USAGE,
            false,
            eku_seq(spec.ext_key_usages),
        ));
    }
    if let Some(ca) = spec.basic_constraints_ca {
        exts.extend_from_slice(&extension(OID_EXT_BASIC_CONSTRAINTS, true, bc_seq(ca)));
    }
    if !exts.is_empty() {
        tbs.extend_from_slice(&der_context_explicit(3, &der_seq(exts)));
    }
    der_seq(tbs)
}

/// Encrypted sigAlg DER (SEQUENCE { OID, NULL }) for use in the outer
/// SEQUENCE.
fn sig_alg_seq(oid: &[u8]) -> Vec<u8> {
    der_seq([der_oid(oid), der_tlv(TAG_NULL, &[])].concat())
}

/// Assemble Certificate ::= SEQUENCE { tbs, sigAlgorithm, signatureValue }
/// once `tbs` and `signature` bytes are known.
fn assemble_cert(tbs: &[u8], sig_alg_oid: &[u8], signature: &[u8]) -> Vec<u8> {
    assemble_cert_with_alg(tbs, &sig_alg_seq(sig_alg_oid), signature)
}

fn assemble_cert_with_alg(tbs: &[u8], sig_alg_der: &[u8], signature: &[u8]) -> Vec<u8> {
    let outer_sig_alg = sig_alg_der.to_vec();
    let sig_bs = der_bit_string(0, signature);
    let mut outer = Vec::with_capacity(tbs.len() + outer_sig_alg.len() + sig_bs.len());
    outer.extend_from_slice(tbs);
    outer.extend_from_slice(&outer_sig_alg);
    outer.extend_from_slice(&sig_bs);
    der_seq(outer)
}

/// Sign-and-bundle a cert with an RSA issuer key.
fn mk_rsa_signed_cert(spec: &SignedCertSpec, issuer_sk: &RsaPrivateKey) -> Vec<u8> {
    let tbs = build_tbs(spec);
    let signature = Rsa::sign_sha256(issuer_sk, &tbs);
    assemble_cert(&tbs, spec.sig_alg_oid, &signature)
}

/// Sign-and-bundle a cert with an ECDSA P-256 issuer key.
fn mk_ecdsa_signed_cert(spec: &SignedCertSpec, issuer_sk: &EcdsaPrivateKey) -> Vec<u8> {
    let tbs = build_tbs(spec);
    let digest = Sha256::digest(&tbs);
    let signature = Ecdsa::sign_with_digest(issuer_sk, &digest);
    assemble_cert(&tbs, spec.sig_alg_oid, &signature)
}

/// Lazily cached RSA-1024 root keypair so the test suite doesn't pay
/// for `generate_keypair` more than once. Keypair generation can take
/// several seconds on debug builds — sharing it across the three RSA
/// tests keeps the suite snappy without weakening coverage (each test
/// still drives its own validate_chain end-to-end).
fn shared_rsa_root() -> &'static (RsaPublicKey, RsaPrivateKey) {
    use std::sync::OnceLock;
    static CELL: OnceLock<(RsaPublicKey, RsaPrivateKey)> = OnceLock::new();
    CELL.get_or_init(|| Rsa::generate_keypair(1024))
}

/// Lazily cached ECDSA P-256 root keypair (parallel rationale to
/// `shared_rsa_root`).
fn shared_ecdsa_root() -> &'static (EcdsaPublicKey, EcdsaPrivateKey) {
    use std::sync::OnceLock;
    static CELL: OnceLock<(EcdsaPublicKey, EcdsaPrivateKey)> = OnceLock::new();
    CELL.get_or_init(|| Ecdsa::generate_keypair())
}

#[test]
fn validate_chain_real_rsa_sha256_signature_passes() {
    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);

    // Build a self-signed root (anchor) with REAL RSA SPKI and a real
    // sha256WithRSAEncryption signature over its TBS.
    let root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Real RSA Root",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        root_sk,
    );

    // Build a leaf signed by the same root. (Sharing the keypair is a
    // shortcut — the leaf's *own* SPKI is irrelevant to the verifier
    // because we never re-sign anything from the leaf in this test.)
    let leaf = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf.example.com",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        },
        root_sk,
    );

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());
    validate_chain(&[leaf, root], &trust).expect("real RSA chain must validate");
}

/// A CROSS-SIGNED intermediate: the same subject and the same key
/// certified by two different roots, both supplied so that either root can
/// be the trust anchor. Only one of them is on the path to the configured
/// anchor; the other is an alternative, not a break — and this was refused
/// as `BrokenChain` until path building landed.
/// `io.netty.pkitesting.CertificateBuilderTest
/// .authenticatingCrossSignedCertificate` is the real-world case.
#[test]
fn validate_chain_builds_a_path_past_a_cross_signed_intermediate() {
    let (trusted_pk, trusted_sk) = shared_rsa_root();
    let trusted_spki = Rsa::public_key_to_der(trusted_pk);
    let (other_pk, other_sk) = Rsa::generate_keypair(1024);
    let other_spki = Rsa::public_key_to_der(&other_pk);
    let (issuer_pk, issuer_sk) = Rsa::generate_keypair(1024);
    let issuer_spki = Rsa::public_key_to_der(&issuer_pk);

    let ca = |subject: &'static str, issuer: &'static str, spki: &[u8], sk: &RsaPrivateKey| {
        mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: subject,
                issuer_cn: issuer,
                spki_der: spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            sk,
        )
    };

    let trusted_root = ca("Trusted Root", "Trusted Root", &trusted_spki, trusted_sk);
    // Same subject, same key, two different issuing roots.
    let issuer_by_trusted = ca("Cross Issuer", "Trusted Root", &issuer_spki, trusted_sk);
    let issuer_by_other = ca("Cross Issuer", "Other Root", &issuer_spki, &other_sk);
    let leaf = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf.example.com",
            issuer_cn: "Cross Issuer",
            spki_der: &other_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        },
        &issuer_sk,
    );

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, trusted_root);

    // Presented with the intermediate that does NOT lead to the anchor
    // first — the order netty's cross-signing test produces.
    let r = validate_chain(
        &[
            leaf.clone(),
            issuer_by_other.clone(),
            issuer_by_trusted.clone(),
        ],
        &trust,
    );
    assert!(r.is_ok(), "cross-signed path must validate, got {r:?}");

    // Already a valid path: must still validate, by the untouched
    // presented-order route.
    let r = validate_chain(
        &[leaf.clone(), issuer_by_trusted, issuer_by_other.clone()],
        &trust,
    );
    assert!(
        r.is_ok(),
        "already-ordered path must still validate, got {r:?}"
    );

    // Path building must NOT rescue a set with no path to the anchor:
    // dropping the untrusted intermediate leaves a leaf with no issuer.
    let r = validate_chain(&[leaf, issuer_by_other], &trust);
    assert!(
        r.is_err(),
        "no path to the anchor must stay rejected, got {r:?}"
    );
}

#[test]
fn validate_chain_rejects_same_subject_fake_root() {
    let (trusted_pk, trusted_sk) = shared_rsa_root();
    let trusted_spki = Rsa::public_key_to_der(trusted_pk);
    let (fake_pk, fake_sk) = Rsa::generate_keypair(1024);
    let fake_spki = Rsa::public_key_to_der(&fake_pk);

    let trusted_root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Collision Root",
            issuer_cn: "Collision Root",
            spki_der: &trusted_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        trusted_sk,
    );
    let fake_root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Collision Root",
            issuer_cn: "Collision Root",
            spki_der: &fake_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        &fake_sk,
    );
    let leaf = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf.example.com",
            issuer_cn: "Collision Root",
            spki_der: &fake_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        },
        &fake_sk,
    );

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, trusted_root);
    match validate_chain(&[leaf, fake_root], &trust) {
        Err(TrustError::BadSignature { at }) => assert_eq!(at, 1),
        other => panic!("expected fake root signature failure, got {:?}", other),
    }
}

#[test]
fn validate_chain_accepts_actual_anchor_when_subject_collides() {
    let (trusted_pk, trusted_sk) = shared_rsa_root();
    let trusted_spki = Rsa::public_key_to_der(trusted_pk);
    let (other_pk, other_sk) = Rsa::generate_keypair(1024);
    let other_spki = Rsa::public_key_to_der(&other_pk);

    let trusted_root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Collision Root",
            issuer_cn: "Collision Root",
            spki_der: &trusted_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        trusted_sk,
    );
    let other_root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Collision Root",
            issuer_cn: "Collision Root",
            spki_der: &other_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        &other_sk,
    );
    let leaf = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf.example.com",
            issuer_cn: "Collision Root",
            spki_der: &trusted_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        },
        trusted_sk,
    );

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, trusted_root.clone());
    insert_anchor(&mut trust, other_root);
    let subject = parse_certificate(&trusted_root).unwrap().subject_der;
    assert_eq!(
        trust.anchors.get(&subject).map(|anchors| anchors.len()),
        Some(2)
    );

    validate_chain(&[leaf, trusted_root], &trust)
        .expect("actual stored anchor must not be hidden by same-subject roots");
}

#[test]
fn validate_chain_real_rsa_tampered_signature_is_bad_signature() {
    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);

    let root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Real RSA Root",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        root_sk,
    );

    // Build a leaf, then *flip a byte in the signature bit-string* so
    // PKCS#1 v1.5 padding-decode will produce the wrong EMSA envelope.
    let mut leaf = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "tampered.example",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        },
        root_sk,
    );
    // Flip the very last byte of the cert — that's inside the signature
    // BIT STRING. (We don't shift any DER length headers, so the cert
    // still parses; only the cryptographic check should fail.)
    let last_idx = leaf.len() - 1;
    leaf[last_idx] ^= 0x01;

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());
    match validate_chain(&[leaf, root], &trust) {
        Err(TrustError::BadSignature { at }) => {
            assert_eq!(at, 0, "leaf is at index 0");
        }
        other => panic!("expected BadSignature, got {:?}", other),
    }
}

#[test]
fn validate_chain_real_ecdsa_p256_sha256_signature_passes() {
    let (root_pk, root_sk) = shared_ecdsa_root();
    let root_spki = Ecdsa::public_key_to_der(root_pk);

    let root = mk_ecdsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Real EC Root",
            issuer_cn: "Real EC Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_ECDSA_SHA256,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        root_sk,
    );

    let leaf = mk_ecdsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "ec-leaf.example.com",
            issuer_cn: "Real EC Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_ECDSA_SHA256,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        },
        root_sk,
    );

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());
    validate_chain(&[leaf, root], &trust).expect("real ECDSA chain must validate");
}

#[test]
fn validate_chain_unknown_signature_oid_reports_not_implemented() {
    // Build a chain where the leaf claims id-Ed25519 — an OID the
    // verifier knows by name but does not implement (no EdDSA primitive
    // at this layer). The anchor is RSA-SHA256-signed so the chain
    // reaches Step 6 cleanly; the rejection comes from the leaf's OID
    // dispatch, and must be structural (NotImplemented) rather than
    // cryptographic, so callers can decide to delegate instead.
    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);

    let root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Real RSA Root",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        root_sk,
    );

    // The *signature bytes* don't matter; we only need the outer sigAlg
    // OID to route to NotImplemented before the cryptographic verifier.
    let tbs = build_tbs(&SignedCertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "ed25519-leaf",
        issuer_cn: "Real RSA Root",
        spki_der: &root_spki,
        sig_alg_oid: OID_SIG_ED25519,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
    });
    let fake_sig = Rsa::sign_sha256(root_sk, &tbs);
    let leaf = assemble_cert(&tbs, OID_SIG_ED25519, &fake_sig);

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());
    match validate_chain(&[leaf, root], &trust) {
        Err(TrustError::NotImplemented { at, oid }) => {
            assert_eq!(at, 0);
            assert_eq!(oid, OID_SIG_ED25519.to_vec());
        }
        other => panic!("expected NotImplemented for Ed25519, got {:?}", other),
    }
}

/// `AlgorithmIdentifier` for RSASSA-PSS with SHA-256, MGF1-SHA-256 and an
/// explicit salt length — the shape netty's `rsapss-ca-cert.cert` and the
/// two `rsaValidation*.p12` fixtures carry.
fn pss_sha256_alg_id(salt_len: u8) -> Vec<u8> {
    const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    let sha256 = der_seq([der_oid(OID_SHA256), der_tlv(TAG_NULL, &[])].concat());
    let mgf = der_seq([der_oid(OID_MGF1), sha256.clone()].concat());
    let params = der_seq(
        [
            der_tlv(0xa0, &sha256),
            der_tlv(0xa1, &mgf),
            der_tlv(0xa2, &der_int(salt_len)),
        ]
        .concat(),
    );
    der_seq([der_oid(OID_SIG_RSA_PSS), params].concat())
}

#[test]
fn validate_chain_rsa_pss_uses_the_params_salt_length_not_hlen() {
    use crate::crypto_impl::{rsa_sign_pss_ex, PssHash};

    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);

    let root = mk_rsa_signed_cert(
        &SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Real RSA Root",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_SHA256_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
        },
        root_sk,
    );

    let leaf_spec = SignedCertSpec {
        not_before_utc: "200101000000Z",
        not_after_utc: "300101000000Z",
        subject_cn: "pss-leaf",
        issuer_cn: "Real RSA Root",
        spki_der: &root_spki,
        sig_alg_oid: OID_SIG_RSA_PSS,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
    };
    // saltLength 20 with a SHA-256 digest: RFC 4055's default, and what
    // the netty fixtures use. The salt length is NOT hLen.
    let alg = pss_sha256_alg_id(20);
    let tbs = build_tbs_with_alg(&leaf_spec, &alg);

    let sig20 = rsa_sign_pss_ex(root_sk, PssHash::Sha256, PssHash::Sha256, 20, &tbs);
    let leaf = assemble_cert_with_alg(&tbs, &alg, &sig20);

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());
    validate_chain(&[leaf, root.clone()], &trust)
        .expect("PSS chain with saltLength=20 must validate");

    // The regression this pins: a verifier that assumes salt == hLen
    // accepts THIS signature and rejects the one above. Both directions
    // are checked, so neither assumption can be reintroduced silently.
    let sig32 = rsa_sign_pss_ex(root_sk, PssHash::Sha256, PssHash::Sha256, 32, &tbs);
    let leaf_wrong_salt = assemble_cert_with_alg(&tbs, &alg, &sig32);
    match validate_chain(&[leaf_wrong_salt, root], &trust) {
        Err(TrustError::BadSignature { at }) => assert_eq!(at, 0),
        other => panic!("saltLength mismatch must be BadSignature, got {:?}", other),
    }
}

#[test]
fn parse_rsa_pss_params_defaults_and_explicit_fields() {
    use crate::crypto_impl::PssHash;

    // Absent parameters => RFC 4055 defaults, which are NOT internally
    // consistent: SHA-1 with a 20-byte salt.
    let d = parse_rsa_pss_params(&[]).expect("absent params are the defaults");
    assert_eq!(d.hash, PssHash::Sha1);
    assert_eq!(d.mgf_hash, PssHash::Sha1);
    assert_eq!(d.salt_len, 20);
    // An explicit NULL means the same thing.
    assert_eq!(parse_rsa_pss_params(&der_tlv(TAG_NULL, &[])), Some(d));

    // The netty shape: SHA-256 / MGF1-SHA-256 / salt 20.
    let alg = pss_sha256_alg_id(20);
    let inner = read_tlv_tagged(&alg, TAG_SEQUENCE).unwrap();
    let oid = read_tlv_tagged(inner.content, TAG_OID).unwrap();
    let p = parse_rsa_pss_params(oid.rest).expect("netty-shaped params parse");
    assert_eq!(p.hash, PssHash::Sha256);
    assert_eq!(p.mgf_hash, PssHash::Sha256);
    assert_eq!(p.salt_len, 20);

    // A digest we do not implement is a refusal, not a silent default.
    const OID_SHA3_256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x08];
    let bad = der_seq(der_tlv(
        0xa0,
        &der_seq([der_oid(OID_SHA3_256), der_tlv(TAG_NULL, &[])].concat()),
    ));
    assert_eq!(parse_rsa_pss_params(&bad), None);
}

// ====================================================================
// Endpoint identification (hostname verification) tests.
// ====================================================================

fn leaf_with_san(cn: &'static str, dns: &'static [&'static str]) -> ParsedCert {
    let der = mk_cert(&CertSpec {
        subject_cn: cn,
        issuer_cn: "CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: dns,
        ..Default::default()
    });
    parse_certificate(&der).expect("parse")
}

#[test]
fn san_dns_names_are_parsed_lowercased() {
    let leaf = leaf_with_san("ignored", &["WWW.Example.COM", "api.example.com"]);
    assert_eq!(
        leaf.san_dns_names,
        vec!["www.example.com", "api.example.com"]
    );
}

#[test]
fn subject_cn_is_extracted_lowercased() {
    let leaf = leaf_with_san("Leaf.Example.Com", &[]);
    assert_eq!(leaf.subject_cn.as_deref(), Some("leaf.example.com"));
}

#[test]
fn verify_hostname_exact_san_match() {
    let leaf = leaf_with_san("cn.example.com", &["host.example.com"]);
    assert_eq!(verify_hostname(&leaf, "host.example.com"), Ok(()));
    assert_eq!(verify_hostname(&leaf, "HOST.example.com"), Ok(()));
}

#[test]
fn verify_hostname_rejects_wrong_host_with_san() {
    let leaf = leaf_with_san("cn.example.com", &["host.example.com"]);
    match verify_hostname(&leaf, "evil.example.com") {
        Err(HostnameError::NoMatch { .. }) => {}
        other => panic!("expected NoMatch, got {:?}", other),
    }
}

#[test]
fn verify_hostname_wildcard_matches_one_label() {
    let leaf = leaf_with_san("cn", &["*.example.com"]);
    assert_eq!(verify_hostname(&leaf, "a.example.com"), Ok(()));
    // Wildcard must NOT span a dot.
    assert!(verify_hostname(&leaf, "a.b.example.com").is_err());
    // Wildcard must NOT match the bare parent domain.
    assert!(verify_hostname(&leaf, "example.com").is_err());
}

#[test]
fn verify_hostname_rejects_overbroad_wildcard() {
    // `*.com` leaves only one label to the right — must be refused.
    assert!(!dns_name_matches("*.com", "example.com"));
    assert!(!dns_name_matches("*", "example"));
    // Wildcard outside the left-most label is invalid.
    assert!(!dns_name_matches("a.*.com", "a.b.com"));
    // Partial-label wildcards are not RFC 6125 wildcards (we only accept
    // a whole `*.` left label), so they fall through to literal compare.
    assert!(!dns_name_matches("f*o.example.com", "foo.example.com"));
}

#[test]
fn verify_hostname_cn_fallback_only_without_san_dns() {
    // No SAN dNSName → CN is consulted.
    let cn_only = leaf_with_san("host.example.com", &[]);
    assert_eq!(verify_hostname(&cn_only, "host.example.com"), Ok(()));

    // SAN dNSName present but non-matching → CN must NOT rescue it
    // (RFC 6125 §6.4.4: presence of SAN forbids CN fallback).
    let san_present = leaf_with_san("host.example.com", &["other.example.com"]);
    match verify_hostname(&san_present, "host.example.com") {
        Err(HostnameError::NoMatch { .. }) => {}
        other => panic!("expected NoMatch (CN must not rescue), got {:?}", other),
    }
}

#[test]
fn verify_hostname_empty_host_is_rejected() {
    let leaf = leaf_with_san("host.example.com", &["host.example.com"]);
    assert_eq!(verify_hostname(&leaf, ""), Err(HostnameError::EmptyHost));
    assert_eq!(verify_hostname(&leaf, "   "), Err(HostnameError::EmptyHost));
}

#[test]
fn verify_hostname_no_identity_fails_closed() {
    // No SAN, no CN — nothing to match against.
    let leaf = leaf_with_san("", &[]);
    // An empty CN string is still "present" but cannot match a real host;
    // either NoIdentity or NoMatch is acceptable, never Ok.
    assert!(verify_hostname(&leaf, "host.example.com").is_err());
}

#[test]
fn host_is_ip_literal_classifies_correctly() {
    assert!(host_is_ip_literal("127.0.0.1"));
    assert!(host_is_ip_literal("10.0.0.255"));
    assert!(host_is_ip_literal("::1"));
    assert!(host_is_ip_literal("fe80::1"));
    assert!(!host_is_ip_literal("example.com"));
    assert!(!host_is_ip_literal("256.0.0.1")); // out of range
    assert!(!host_is_ip_literal("1.2.3")); // too few labels
}

#[test]
fn check_endpoint_identity_drives_leaf() {
    let der = mk_cert(&CertSpec {
        subject_cn: "cn.example.com",
        issuer_cn: "CA",
        spki_alg: OID_RSA,
        key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
        ext_key_usages: &[OID_KP_SERVER_AUTH],
        basic_constraints_ca: Some(false),
        subject_alt_dns: &["leaf.example.com"],
        ..Default::default()
    });
    assert_eq!(
        check_endpoint_identity(&[der.clone()], "leaf.example.com"),
        Ok(())
    );
    assert!(check_endpoint_identity(&[der], "wrong.example.com").is_err());
    // Empty chain has no leaf identity.
    assert!(check_endpoint_identity(&[], "leaf.example.com").is_err());
}

// ====================================================================
// RFC 5280 §4.2.1.10 name-constraints tests.
// ====================================================================

/// Build a `Name` SEQUENCE with one CN-RDN per supplied label, in order.
fn name_with_rdns(cns: &[&str]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for cn in cns {
        let cn_oid = der_oid(&[0x55, 0x04, 0x03]);
        let cn_val = der_tlv(0x13, cn.as_bytes());
        let atv = der_seq([cn_oid, cn_val].concat());
        body.extend_from_slice(&der_set(atv));
    }
    der_seq(body)
}

/// Encode a `NameConstraints` extension (critical) from `(GeneralName tag,
/// base bytes)` entries. `der_tlv(tag, base)` is the GeneralName; each is
/// wrapped in a one-element `GeneralSubtree` SEQUENCE.
fn nc_extension(permitted: &[(u8, &[u8])], excluded: &[(u8, &[u8])]) -> Vec<u8> {
    fn subtrees(entries: &[(u8, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (tag, base) in entries {
            let gn = der_tlv(*tag, base);
            body.extend_from_slice(&der_seq(gn)); // GeneralSubtree ::= SEQ { base }
        }
        body
    }
    let mut seq_body = Vec::new();
    if !permitted.is_empty() {
        seq_body.extend_from_slice(&der_tlv(0xa0, &subtrees(permitted))); // [0]
    }
    if !excluded.is_empty() {
        seq_body.extend_from_slice(&der_tlv(0xa1, &subtrees(excluded))); // [1]
    }
    extension(OID_EXT_NAME_CONSTRAINTS, true, der_seq(seq_body))
}

/// Build + RSA-sign a v3 cert carrying BasicConstraints plus an arbitrary
/// set of pre-encoded extensions. Keeps the name-constraints tests from
/// having to widen the shared `SignedCertSpec`.
fn mk_rsa_cert_with_exts(
    subject_cn: &str,
    issuer_cn: &str,
    spki_der: &[u8],
    is_ca: bool,
    extra_exts: &[u8],
    issuer_sk: &RsaPrivateKey,
) -> Vec<u8> {
    let mut tbs: Vec<u8> = Vec::new();
    tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
    tbs.extend_from_slice(&der_int(1));
    tbs.extend_from_slice(&sig_alg_seq(OID_SIG_SHA256_RSA));
    tbs.extend_from_slice(&name_with_cn(issuer_cn));
    tbs.extend_from_slice(&der_seq(
        [der_utctime("200101000000Z"), der_utctime("490101000000Z")].concat(),
    ));
    tbs.extend_from_slice(&name_with_cn(subject_cn));
    tbs.extend_from_slice(spki_der);
    let mut exts: Vec<u8> = Vec::new();
    exts.extend_from_slice(&extension(OID_EXT_BASIC_CONSTRAINTS, true, bc_seq(is_ca)));
    exts.extend_from_slice(extra_exts);
    tbs.extend_from_slice(&der_context_explicit(3, &der_seq(exts)));
    let tbs = der_seq(tbs);
    let sig = Rsa::sign_sha256(issuer_sk, &tbs);
    assemble_cert(&tbs, OID_SIG_SHA256_RSA, &sig)
}

#[test]
fn dns_constraint_matching_follows_rfc5280() {
    // Bare constraint: matches itself and any subdomain, label-aligned.
    assert!(dns_constraint_matches("example.com", "example.com"));
    assert!(dns_constraint_matches("example.com", "www.example.com"));
    assert!(dns_constraint_matches("example.com", "a.b.example.com"));
    assert!(!dns_constraint_matches("example.com", "notexample.com"));
    assert!(!dns_constraint_matches(
        "example.com",
        "example.com.evil.com"
    ));
    assert!(!dns_constraint_matches("example.com", "com"));
    // Empty constraint matches everything.
    assert!(dns_constraint_matches("", "anything.test"));
    // Leading-dot constraint: strict subdomains only.
    assert!(dns_constraint_matches(".example.com", "www.example.com"));
    assert!(!dns_constraint_matches(".example.com", "example.com"));
}

#[test]
fn email_constraint_matching_follows_rfc5280() {
    // Full mailbox: exact match.
    assert!(email_constraint_matches(
        "ann@example.com",
        "ann@example.com"
    ));
    assert!(!email_constraint_matches(
        "ann@example.com",
        "bob@example.com"
    ));
    // Host constraint: any mailbox at that host, not a subdomain.
    assert!(email_constraint_matches("example.com", "ann@example.com"));
    assert!(!email_constraint_matches(
        "example.com",
        "ann@sub.example.com"
    ));
    // Leading-dot: subdomains only.
    assert!(email_constraint_matches(
        ".example.com",
        "ann@sub.example.com"
    ));
    assert!(!email_constraint_matches(".example.com", "ann@example.com"));
    // Malformed presented address (no host).
    assert!(!email_constraint_matches("example.com", "no-at-sign"));
}

#[test]
fn ip_constraint_matching_uses_cidr_mask() {
    // 192.168.0.0/16 = addr 192.168.0.0, mask 255.255.0.0.
    let v4 = [192u8, 168, 0, 0, 255, 255, 0, 0];
    assert!(ip_constraint_matches(&v4, &[192, 168, 1, 5]));
    assert!(ip_constraint_matches(&v4, &[192, 168, 255, 255]));
    assert!(!ip_constraint_matches(&v4, &[10, 0, 0, 1]));
    assert!(!ip_constraint_matches(&v4, &[192, 169, 0, 1]));
    // Family mismatch: v4 constraint vs 16-byte address.
    assert!(!ip_constraint_matches(&v4, &[0u8; 16]));
    // IPv6 ::/0 (all-zero mask) matches anything v6.
    let v6_any = [0u8; 32];
    assert!(ip_constraint_matches(&v6_any, &[1u8; 16]));
}

#[test]
fn uri_host_extraction() {
    assert_eq!(
        uri_host("https://host.example.com/path"),
        Some("host.example.com".into())
    );
    assert_eq!(
        uri_host("http://user@h.example.com:8443/x"),
        Some("h.example.com".into())
    );
    assert_eq!(
        uri_host("https://[2001:db8::1]:443/"),
        Some("2001:db8::1".into())
    );
    assert_eq!(
        uri_host("HTTPS://Host.Example.COM"),
        Some("host.example.com".into())
    );
    assert_eq!(uri_host(""), None);
}

#[test]
fn dir_name_prefix_matching() {
    let base = name_with_rdns(&["Acme"]);
    let leaf = name_with_rdns(&["Acme", "leaf"]);
    let other = name_with_rdns(&["Other"]);
    // Constraint is an initial RDN prefix of the presented DN.
    assert!(dir_name_within(&base, &leaf));
    assert!(dir_name_within(&base, &base));
    // Different first RDN → not within.
    assert!(!dir_name_within(&other, &leaf));
    // Constraint longer than presented → not within.
    assert!(!dir_name_within(&leaf, &base));
}

#[test]
fn parse_name_constraints_round_trips() {
    let ext = nc_extension(
        &[(SAN_TAG_DNS_NAME, b"example.com")],
        &[(SAN_TAG_DNS_NAME, b"bad.example.com")],
    );
    // `ext` is a full Extension SEQUENCE; pull the OCTET STRING value out.
    // Extension ::= SEQ { OID, BOOL critical, OCTET STRING value }.
    let seq = read_tlv_tagged(&ext, TAG_SEQUENCE).unwrap();
    let oid = read_tlv_tagged(seq.content, TAG_OID).unwrap();
    let after_oid = oid.rest;
    let crit = read_tlv(after_oid).unwrap();
    let octet = read_tlv_tagged(crit.rest, TAG_OCTET_STRING).unwrap();
    let nc = parse_name_constraints(octet.content).expect("parse NC");
    assert_eq!(nc.permitted.dns, vec!["example.com".to_string()]);
    assert_eq!(nc.excluded.dns, vec!["bad.example.com".to_string()]);
}

#[test]
fn validate_chain_enforces_permitted_dns_subtree() {
    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);

    // Root permits only the example.com dNSName subtree.
    let nc = nc_extension(&[(SAN_TAG_DNS_NAME, b"example.com")], &[]);
    let root = mk_rsa_cert_with_exts("NC Root", "NC Root", &root_spki, true, &nc, root_sk);

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());

    // Leaf inside the permitted subtree validates.
    let san_ok = extension(
        OID_EXT_SUBJECT_ALT_NAME,
        false,
        san_dns_value(&["host.example.com"]),
    );
    let leaf_ok = mk_rsa_cert_with_exts(
        "host.example.com",
        "NC Root",
        &root_spki,
        false,
        &san_ok,
        root_sk,
    );
    validate_chain(&[leaf_ok, root.clone()], &trust)
        .expect("leaf within permitted subtree must validate");

    // Leaf outside the permitted subtree is rejected at index 0.
    let san_bad = extension(
        OID_EXT_SUBJECT_ALT_NAME,
        false,
        san_dns_value(&["host.evil.com"]),
    );
    let leaf_bad = mk_rsa_cert_with_exts(
        "host.evil.com",
        "NC Root",
        &root_spki,
        false,
        &san_bad,
        root_sk,
    );
    match validate_chain(&[leaf_bad, root.clone()], &trust) {
        Err(TrustError::NameConstraintViolation { at, .. }) => assert_eq!(at, 0),
        other => panic!("expected NameConstraintViolation, got {:?}", other),
    }
}

#[test]
fn validate_chain_enforces_excluded_dns_subtree() {
    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);

    // Root excludes the evil.example.com subtree (permits everything else).
    let nc = nc_extension(&[], &[(SAN_TAG_DNS_NAME, b"evil.example.com")]);
    let root = mk_rsa_cert_with_exts("X Root", "X Root", &root_spki, true, &nc, root_sk);

    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());

    // A name outside the excluded subtree is fine.
    let san_ok = extension(
        OID_EXT_SUBJECT_ALT_NAME,
        false,
        san_dns_value(&["host.good.com"]),
    );
    let leaf_ok = mk_rsa_cert_with_exts(
        "host.good.com",
        "X Root",
        &root_spki,
        false,
        &san_ok,
        root_sk,
    );
    validate_chain(&[leaf_ok, root.clone()], &trust).expect("non-excluded leaf must validate");

    // A name inside the excluded subtree is rejected.
    let san_bad = extension(
        OID_EXT_SUBJECT_ALT_NAME,
        false,
        san_dns_value(&["www.evil.example.com"]),
    );
    let leaf_bad = mk_rsa_cert_with_exts(
        "www.evil.example.com",
        "X Root",
        &root_spki,
        false,
        &san_bad,
        root_sk,
    );
    match validate_chain(&[leaf_bad, root], &trust) {
        Err(TrustError::NameConstraintViolation { .. }) => {}
        other => panic!("expected NameConstraintViolation, got {:?}", other),
    }
}

#[test]
fn validate_chain_without_name_constraints_is_unaffected() {
    // A chain whose CA carries NO NameConstraints extension must still
    // validate names of every kind (regression guard for the new step).
    let (root_pk, root_sk) = shared_rsa_root();
    let root_spki = Rsa::public_key_to_der(root_pk);
    let root =
        mk_rsa_cert_with_exts("Plain Root", "Plain Root", &root_spki, true, &[], root_sk);
    let san = extension(
        OID_EXT_SUBJECT_ALT_NAME,
        false,
        san_dns_value(&["anything.example"]),
    );
    let leaf = mk_rsa_cert_with_exts(
        "anything.example",
        "Plain Root",
        &root_spki,
        false,
        &san,
        root_sk,
    );
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, root.clone());
    validate_chain(&[leaf, root], &trust).expect("unconstrained chain must validate");
}

// -------------------------------------------------------------------
// OCSP (RFC 6960) — DER encode/decode round-trip and signature check.
// Network I/O (`ocsp_http_post`/`check_ocsp`) is exercised by the real
// Tomcat OCSP suites (TestSecurity2017Ocsp, TestOcspEnabled,
// TestOcspSoftFail*) rather than here; these tests cover the pure
// ASN.1 + crypto primitives in isolation.
// -------------------------------------------------------------------

fn sample_cert_id() -> CertId {
    CertId {
        issuer_name_hash: [0x11; 20],
        issuer_key_hash: [0x22; 20],
        serial_der: vec![0x10, 0x03],
    }
}

#[test]
fn build_ocsp_request_is_well_formed_der() {
    let req = build_ocsp_request(&sample_cert_id());
    // OCSPRequest ::= SEQUENCE { tbsRequest TBSRequest }
    let outer = read_tlv_tagged(&req, TAG_SEQUENCE).expect("outer SEQUENCE");
    let tbs = read_tlv_tagged(outer.content, TAG_SEQUENCE).expect("tbsRequest SEQUENCE");
    let request_list =
        read_tlv_tagged(tbs.content, TAG_SEQUENCE).expect("requestList SEQUENCE");
    let request =
        read_tlv_tagged(request_list.content, TAG_SEQUENCE).expect("Request SEQUENCE");
    let cert_id = read_tlv_tagged(request.content, TAG_SEQUENCE).expect("CertID SEQUENCE");
    let hash_alg =
        read_tlv_tagged(cert_id.content, TAG_SEQUENCE).expect("hashAlgorithm SEQUENCE");
    let oid = read_tlv_tagged(hash_alg.content, TAG_OID).expect("hashAlgorithm OID");
    assert_eq!(
        oid.content, OID_SHA1,
        "CertID must use SHA-1 per RFC 6960 convention"
    );
    // `oid.rest` is the hashAlgorithm's NULL parameter, still inside the
    // hashAlgorithm SEQUENCE; issuerNameHash is `hash_alg.rest` (CertID's
    // next sibling field, after the whole hashAlgorithm SEQUENCE).
    let name_hash = read_tlv_tagged(hash_alg.rest, TAG_OCTET_STRING).expect("issuerNameHash");
    assert_eq!(name_hash.content, &[0x11; 20]);
    let key_hash = read_tlv_tagged(name_hash.rest, TAG_OCTET_STRING).expect("issuerKeyHash");
    assert_eq!(key_hash.content, &[0x22; 20]);
    let serial = read_tlv_tagged(key_hash.rest, TAG_INTEGER).expect("serialNumber");
    assert_eq!(serial.content, &[0x10, 0x03]);
}

#[test]
fn sha1_matches_known_vector() {
    // RFC 3174 test vector: SHA-1("abc") = a9993e364706816aba3e25717850c26c9cd0d89
    let digest = sha1(b"abc");
    assert_eq!(
        digest,
        [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
        ]
    );
}

/// Build a minimal, well-formed `BasicOCSPResponse` DER wrapped in an
/// `OCSPResponse` (`responseStatus = successful`), signed directly by
/// `issuer_sk` (no delegated responder cert) over a single
/// `SingleResponse` for `serial_der` with the given `status_tag`.
fn build_test_ocsp_response(
    issuer_sk: &RsaPrivateKey,
    serial_der: &[u8],
    status_tag: u8,
) -> Vec<u8> {
    // responderID: byName [1] EXPLICIT Name -- content doesn't matter for
    // these tests, use an empty SEQUENCE (valid, if unusual, RDNSequence).
    let responder_id = der_tlv_encode(0xa1, &der_seq_encode(Vec::new()));
    let produced_at = der_tlv_encode(TAG_GENERALIZED_TIME, b"20200101000000Z");

    let cert_id = der_seq_encode({
        let mut h = der_seq_encode({
            let mut hh = der_oid_encode(OID_SHA1);
            hh.extend_from_slice(&der_null_encode());
            hh
        });
        h.extend_from_slice(&der_octet_encode(&[0x11; 20]));
        h.extend_from_slice(&der_octet_encode(&[0x22; 20]));
        h.extend_from_slice(&der_integer_from_content(serial_der));
        h
    });
    // certStatus: good=0x80 (primitive NULL), revoked=0xa1 (constructed,
    // needs a RevokedInfo body), unknown=0x82 (primitive NULL).
    let cert_status = match status_tag {
        0x80 => vec![0x80, 0x00],
        TAG_CONTEXT_1_CONSTRUCTED => {
            // RevokedInfo ::= SEQUENCE { revocationTime GeneralizedTime }
            der_tlv_encode(
                TAG_CONTEXT_1_CONSTRUCTED,
                &der_tlv_encode(TAG_GENERALIZED_TIME, b"20200101000000Z"),
            )
        }
        TAG_CONTEXT_2_PRIMITIVE => vec![0x82, 0x00],
        _ => panic!("unsupported status_tag in test helper"),
    };
    // SingleResponse ::= SEQUENCE { certID, certStatus, thisUpdate,
    //   [nextUpdate], [singleExtensions] } -- `produced_at`'s DER shape
    // (a GeneralizedTime TLV) is reused verbatim for thisUpdate.
    let single_response = der_seq_encode({
        let mut v = cert_id;
        v.extend_from_slice(&cert_status);
        v.extend_from_slice(&produced_at);
        v
    });

    let responses = der_seq_encode(single_response);
    let response_data = der_seq_encode({
        let mut v = responder_id;
        v.extend_from_slice(&produced_at);
        v.extend_from_slice(&responses);
        v
    });

    let sig_alg = der_seq_encode({
        let mut v = der_oid_encode(OID_SIG_SHA256_RSA);
        v.extend_from_slice(&der_null_encode());
        v
    });
    let signature = Rsa::sign_sha256(issuer_sk, &response_data);
    let sig_bitstring = der_tlv_encode(TAG_BIT_STRING, &{
        let mut v = vec![0u8];
        v.extend_from_slice(&signature);
        v
    });

    let basic_response = der_seq_encode({
        let mut v = response_data;
        v.extend_from_slice(&sig_alg);
        v.extend_from_slice(&sig_bitstring);
        v
    });

    let response_bytes = der_seq_encode({
        let mut v = der_oid_encode(OID_OCSP_BASIC_RESPONSE);
        v.extend_from_slice(&der_octet_encode(&basic_response));
        v
    });

    der_seq_encode({
        let mut v = vec![TAG_ENUMERATED, 0x01, 0x00]; // responseStatus = successful(0)
        v.extend_from_slice(&der_tlv_encode(TAG_CONTEXT_0, &response_bytes));
        v
    })
}

#[test]
fn ocsp_response_good_status_parses_and_verifies() {
    let (_pk, sk) = shared_rsa_root();
    let serial = vec![0x10, 0x03];
    let der = build_test_ocsp_response(sk, &serial, 0x80);
    let parsed = parse_ocsp_response(&der).expect("parse should succeed");
    assert_eq!(parsed.responses.len(), 1);
    assert_eq!(parsed.responses[0].status, OcspStatusTag::Good);
    assert_eq!(parsed.responses[0].cert_id_serial, serial);

    let root_spki = Rsa::public_key_to_der(&shared_rsa_root().0);
    verify_ocsp_response_signature(&parsed, &root_spki, None)
        .expect("signature must verify against the real issuer key");
}

#[test]
fn ocsp_response_revoked_status_parses() {
    let (_pk, sk) = shared_rsa_root();
    let serial = vec![0x10, 0x03];
    let der = build_test_ocsp_response(sk, &serial, TAG_CONTEXT_1_CONSTRUCTED);
    let parsed = parse_ocsp_response(&der).expect("parse should succeed");
    assert_eq!(parsed.responses[0].status, OcspStatusTag::Revoked);
}

#[test]
fn ocsp_response_unknown_status_parses() {
    let (_pk, sk) = shared_rsa_root();
    let serial = vec![0x10, 0x03];
    let der = build_test_ocsp_response(sk, &serial, TAG_CONTEXT_2_PRIMITIVE);
    let parsed = parse_ocsp_response(&der).expect("parse should succeed");
    assert_eq!(parsed.responses[0].status, OcspStatusTag::Unknown);
}

#[test]
fn ocsp_response_signature_rejected_against_wrong_key() {
    let (_pk, sk) = shared_rsa_root();
    let serial = vec![0x10, 0x03];
    let der = build_test_ocsp_response(sk, &serial, 0x80);
    let parsed = parse_ocsp_response(&der).expect("parse should succeed");

    // A different keypair's SPKI must NOT validate this response's signature.
    let (other_pk, _other_sk) = Rsa::generate_keypair(1024);
    let wrong_spki = Rsa::public_key_to_der(&other_pk);
    let err = verify_ocsp_response_signature(&parsed, &wrong_spki, None)
        .expect_err("signature must NOT verify against an unrelated key");
    assert!(
        err.contains("does not verify"),
        "unexpected error text: {err}"
    );
}

#[test]
fn ocsp_response_try_later_status_is_rejected() {
    // responseStatus = tryLater(3), no responseBytes at all — matches
    // TesterOcspResponderServlet's TRY_LATER fixed-response shape.
    let der = der_seq_encode(vec![TAG_ENUMERATED, 0x01, 0x03]);
    let err = parse_ocsp_response(&der).expect_err("tryLater must not parse as successful");
    assert!(err.contains("tryLater"), "unexpected error text: {err}");
}
