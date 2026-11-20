use super::builder::*;
use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn subject_cn(name_text: &str) -> Vec<u8> {
    name(&[rdn(&atv(OID_CN, &printable_string(name_text)))])
}

#[test]
fn parse_bare_cert_cn_only() {
    let cert = certificate(&subject_cn("Acme Corp"));
    let dn = parse_cert_subject_dn(&cert).unwrap();
    assert_eq!(dn, "CN=Acme Corp");
}

#[test]
fn parse_multi_rdn_reverse_order() {
    // Subject C=US, O=Acme, CN=Widget — renders most-specific first.
    let rdns = vec![
        rdn(&atv(OID_C, &printable_string("US"))),
        rdn(&atv(OID_O, &printable_string("Acme"))),
        rdn(&atv(OID_CN, &printable_string("Widget"))),
    ];
    let cert = certificate(&name(&rdns));
    let dn = parse_cert_subject_dn(&cert).unwrap();
    assert_eq!(dn, "CN=Widget, O=Acme, C=US");
}

#[test]
fn parse_utf8_value() {
    let subj = name(&[rdn(&atv(OID_CN, &utf8_string("Ümlaut GmbH")))]);
    let cert = certificate(&subj);
    let dn = parse_cert_subject_dn(&cert).unwrap();
    assert_eq!(dn, "CN=Ümlaut GmbH");
}

#[test]
fn parse_unknown_oid_renders_dotted() {
    // Random synthetic OID 1.2.3.4.5 = 0x2a 0x03 0x04 0x05.
    let unknown_oid = [0x2a, 0x03, 0x04, 0x05];
    let subj = name(&[rdn(&atv(&unknown_oid, &printable_string("x")))]);
    let cert = certificate(&subj);
    let dn = parse_cert_subject_dn(&cert).unwrap();
    assert!(dn.starts_with("OID.1.2.3.4.5="), "got {dn}");
}

#[test]
fn parse_pkcs7_wrapped() {
    let cert = certificate(&subject_cn("Acme Corp"));
    let p7 = pkcs7_signed_data(&cert);
    let dn = parse_signer_dn(&p7).unwrap();
    assert_eq!(dn, "CN=Acme Corp");
}

#[test]
fn rfc4514_escapes_special_chars() {
    let subj = name(&[rdn(&atv(OID_CN, &utf8_string("Acme, Inc.")))]);
    let cert = certificate(&subj);
    let dn = parse_cert_subject_dn(&cert).unwrap();
    // Comma must be backslash-escaped inside the value.
    assert_eq!(dn, "CN=Acme\\, Inc.");
}

#[test]
fn rfc4514_escapes_leading_space_and_hash() {
    let subj = name(&[rdn(&atv(OID_CN, &utf8_string(" Acme")))]);
    let cert = certificate(&subj);
    let dn = parse_cert_subject_dn(&cert).unwrap();
    assert_eq!(dn, "CN=\\ Acme");

    let subj2 = name(&[rdn(&atv(OID_CN, &utf8_string("#Acme")))]);
    let cert2 = certificate(&subj2);
    let dn2 = parse_cert_subject_dn(&cert2).unwrap();
    assert_eq!(dn2, "CN=\\#Acme");
}

#[test]
fn malformed_input_returns_err_not_panic() {
    assert!(parse_signer_dn(b"").is_err());
    assert!(parse_signer_dn(&[0x30, 0x82, 0xff, 0xff]).is_err());
    assert!(parse_signer_dn(&[0xff; 256]).is_err());
    // Truncated TLV.
    assert!(parse_signer_dn(&[0x30, 0x05, 0x02]).is_err());
}

#[test]
fn oversize_input_rejected() {
    let big = vec![0u8; MAX_INPUT_SIZE + 1];
    assert_eq!(parse_signer_dn(&big).unwrap_err(), X509Error::TooLarge);
}

#[test]
fn indefinite_length_rejected() {
    // BER indefinite length (first byte 0x80) is illegal in DER.
    let bad = vec![0x30, 0x80, 0x00, 0x00];
    assert_eq!(parse_signer_dn(&bad).unwrap_err(), X509Error::NotPkcs7);
}

#[test]
fn cert_without_signed_data_oid_rejected() {
    let cert = certificate(&subject_cn("Acme"));
    // Feed a bare Certificate (no PKCS#7 wrapper) — parse_signer_dn
    // must reject it.
    let err = parse_signer_dn(&cert).unwrap_err();
    assert_eq!(err, X509Error::NotPkcs7);
}

#[test]
fn multi_valued_rdn_joined_with_plus() {
    // SET { ATV1, ATV2 }
    let mut set_body = Vec::new();
    set_body.extend_from_slice(&atv(OID_CN, &printable_string("Foo")));
    set_body.extend_from_slice(&atv(OID_O, &printable_string("Bar")));
    let r = set(&set_body);
    let subj = name(&[r]);
    let cert = certificate(&subj);
    let dn = parse_cert_subject_dn(&cert).unwrap();
    // DER SET is sorted so ordering is not stable, but both must
    // be present joined with '+'.
    assert!(dn.contains("CN=Foo"));
    assert!(dn.contains("O=Bar"));
    assert!(dn.contains("+"));
}

#[test]
fn email_and_ou_render() {
    let rdns = vec![
        rdn(&atv(OID_CN, &printable_string("John"))),
        rdn(&atv(OID_OU, &printable_string("Eng"))),
        rdn(&atv(OID_EMAIL, &ia5_string("john@acme.com"))),
    ];
    let cert = certificate(&name(&rdns));
    let dn = parse_cert_subject_dn(&cert).unwrap();
    assert!(dn.contains("CN=John"), "got {dn}");
    assert!(dn.contains("OU=Eng"), "got {dn}");
    assert!(dn.contains("EMAILADDRESS=john@acme.com"), "got {dn}");
}
