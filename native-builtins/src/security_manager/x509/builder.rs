use super::*;

/// Emit a DER TLV with the given tag and body.
pub fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.push(tag);
    encode_length(body.len(), &mut out);
    out.extend_from_slice(body);
    out
}

fn encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    // Emit the shortest big-endian length.
    let mut bytes = Vec::new();
    let mut v = len;
    while v > 0 {
        bytes.push((v & 0xff) as u8);
        v >>= 8;
    }
    bytes.reverse();
    out.push(0x80 | (bytes.len() as u8));
    out.extend_from_slice(&bytes);
}

pub fn sequence(body: &[u8]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, body)
}

pub fn set(body: &[u8]) -> Vec<u8> {
    tlv(TAG_SET, body)
}

pub fn oid(raw: &[u8]) -> Vec<u8> {
    tlv(TAG_OID, raw)
}

pub fn printable_string(s: &str) -> Vec<u8> {
    tlv(TAG_PRINTABLE_STRING, s.as_bytes())
}

pub fn utf8_string(s: &str) -> Vec<u8> {
    tlv(TAG_UTF8_STRING, s.as_bytes())
}

pub fn ia5_string(s: &str) -> Vec<u8> {
    tlv(TAG_IA5_STRING, s.as_bytes())
}

pub fn integer(n: u32) -> Vec<u8> {
    let mut bytes = n.to_be_bytes().to_vec();
    while bytes.len() > 1 && bytes[0] == 0 && (bytes[1] & 0x80) == 0 {
        bytes.remove(0);
    }
    tlv(TAG_INTEGER, &bytes)
}

pub fn context0(body: &[u8]) -> Vec<u8> {
    tlv(TAG_CONTEXT_0, body)
}

/// Build a minimal AlgorithmIdentifier (OID + NULL) whose OID is
/// arbitrary — the parser never looks at it, only skips it.
pub fn algorithm_identifier() -> Vec<u8> {
    // SEQUENCE { OID 1.2.840.113549.1.1.11 (sha256WithRSAEncryption), NULL }
    let oid_bytes = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
    let mut body = oid(&oid_bytes);
    // NULL: tag 0x05, length 0.
    body.extend_from_slice(&[0x05, 0x00]);
    sequence(&body)
}

pub fn validity() -> Vec<u8> {
    // SEQUENCE { UTCTime "990101000000Z", UTCTime "491231235959Z" }
    let not_before = tlv(TAG_UTC_TIME, b"990101000000Z");
    let not_after = tlv(TAG_UTC_TIME, b"491231235959Z");
    let mut body = Vec::new();
    body.extend_from_slice(&not_before);
    body.extend_from_slice(&not_after);
    sequence(&body)
}

/// Subject Public Key Info: SEQUENCE { AlgorithmIdentifier, BIT STRING }
pub fn spki() -> Vec<u8> {
    let mut body = algorithm_identifier();
    let bit_string = tlv(TAG_BIT_STRING, &[0x00]);
    body.extend_from_slice(&bit_string);
    sequence(&body)
}

/// Build an `AttributeTypeAndValue` SEQUENCE{OID, String}.
pub fn atv(oid_bytes: &[u8], value: &[u8]) -> Vec<u8> {
    let mut body = oid(oid_bytes);
    body.extend_from_slice(value);
    sequence(&body)
}

/// Single-valued RDN: SET of one ATV.
pub fn rdn(atv_bytes: &[u8]) -> Vec<u8> {
    set(atv_bytes)
}

pub fn name(rdns: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for r in rdns {
        body.extend_from_slice(r);
    }
    sequence(&body)
}

/// Build a minimal X.509 v3 certificate with the given subject DN
/// (as an RDNSequence body) and an arbitrary issuer.  Suitable for
/// feeding to `parse_cert_subject_dn`.
pub fn certificate(subject_name: &[u8]) -> Vec<u8> {
    let issuer = name(&[rdn(&atv(OID_CN, &printable_string("Issuer")))]);
    let mut tbs_body = Vec::new();
    // version [0] EXPLICIT INTEGER 2 (v3)
    tbs_body.extend_from_slice(&context0(&integer(2)));
    // serialNumber
    tbs_body.extend_from_slice(&integer(1));
    // signature AlgorithmIdentifier
    tbs_body.extend_from_slice(&algorithm_identifier());
    // issuer
    tbs_body.extend_from_slice(&issuer);
    // validity
    tbs_body.extend_from_slice(&validity());
    // subject (raw Name bytes already include the SEQUENCE wrapper)
    tbs_body.extend_from_slice(subject_name);
    // subjectPublicKeyInfo
    tbs_body.extend_from_slice(&spki());
    let tbs = sequence(&tbs_body);

    let mut cert_body = tbs;
    cert_body.extend_from_slice(&algorithm_identifier());
    let signature = tlv(TAG_BIT_STRING, &[0x00]);
    cert_body.extend_from_slice(&signature);
    sequence(&cert_body)
}

/// Wrap a single Certificate in a PKCS#7 ContentInfo/SignedData envelope.
pub fn pkcs7_signed_data(cert_der: &[u8]) -> Vec<u8> {
    // digestAlgorithms SET OF AlgorithmIdentifier — empty set is fine
    // for parsing purposes.
    let digest_algorithms = set(&[]);

    // EncapsulatedContentInfo SEQUENCE { contentType OID }
    let encap = sequence(&oid(&[
        0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01,
    ]));

    // certificates [0] IMPLICIT { Certificate }
    let certificates = tlv(TAG_CONTEXT_0, cert_der);

    // signerInfos SET — empty.
    let signer_infos = set(&[]);

    let mut signed_data_body = Vec::new();
    signed_data_body.extend_from_slice(&integer(1));
    signed_data_body.extend_from_slice(&digest_algorithms);
    signed_data_body.extend_from_slice(&encap);
    signed_data_body.extend_from_slice(&certificates);
    signed_data_body.extend_from_slice(&signer_infos);
    let signed_data = sequence(&signed_data_body);

    // Wrap in ContentInfo.
    let content_info_body = {
        let mut body = oid(OID_SIGNED_DATA);
        body.extend_from_slice(&context0(&signed_data));
        body
    };
    sequence(&content_info_body)
}
