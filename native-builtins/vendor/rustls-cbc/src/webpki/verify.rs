use alloc::vec::Vec;
use core::fmt;

use pki_types::{
    CertificateDer, ServerName, SignatureVerificationAlgorithm, SubjectPublicKeyInfoDer, UnixTime,
};

use super::anchors::RootCertStore;
use super::pki_error;
use crate::enums::SignatureScheme;
use crate::error::{Error, PeerMisbehaved};
use crate::verify::{DigitallySignedStruct, HandshakeSignatureValid};

/// Verify that the end-entity certificate `end_entity` is a valid server cert
/// and chains to at least one of the trust anchors in the `roots` [RootCertStore].
///
/// This function is primarily useful when building a custom certificate verifier. It
/// performs **no revocation checking**. Implementers must handle this themselves,
/// along with checking that the server certificate is valid for the subject name
/// being used (see [`verify_server_name`]).
///
/// `intermediates` contains all certificates other than `end_entity` that
/// were sent as part of the server's `Certificate` message. It is in the
/// same order that the server sent them and may be empty.
#[allow(dead_code)]
pub fn verify_server_cert_signed_by_trust_anchor(
    cert: &ParsedCertificate<'_>,
    roots: &RootCertStore,
    intermediates: &[CertificateDer<'_>],
    now: UnixTime,
    supported_algs: &[&dyn SignatureVerificationAlgorithm],
) -> Result<(), Error> {
    verify_server_cert_signed_by_trust_anchor_impl(
        cert,
        roots,
        intermediates,
        None, // No revocation checking supported with this API.
        now,
        supported_algs,
    )
}

/// Verify that the `end_entity` has an alternative name matching the `server_name`.
///
/// Note: this only verifies the name and should be used in conjunction with more verification
/// like [verify_server_cert_signed_by_trust_anchor]
pub fn verify_server_name(
    cert: &ParsedCertificate<'_>,
    server_name: &ServerName<'_>,
) -> Result<(), Error> {
    cert.0
        .verify_is_valid_for_subject_name(server_name)
        .map_err(pki_error)
}

/// Describes which `webpki` signature verification algorithms are supported and
/// how they map to TLS [`SignatureScheme`]s.
#[derive(Clone, Copy)]
#[allow(unreachable_pub)]
pub struct WebPkiSupportedAlgorithms {
    /// A list of all supported signature verification algorithms.
    ///
    /// Used for verifying certificate chains.
    ///
    /// The order of this list is not significant.
    pub all: &'static [&'static dyn SignatureVerificationAlgorithm],

    /// A mapping from TLS `SignatureScheme`s to matching webpki signature verification algorithms.
    ///
    /// This is one (`SignatureScheme`) to many ([`SignatureVerificationAlgorithm`]) because
    /// (depending on the protocol version) there is not necessary a 1-to-1 mapping.
    ///
    /// For TLS1.2, all `SignatureVerificationAlgorithm`s are tried in sequence.
    ///
    /// For TLS1.3, only the first is tried.
    ///
    /// The supported schemes in this mapping is communicated to the peer and the order is significant.
    /// The first mapping is our highest preference.
    pub mapping: &'static [(
        SignatureScheme,
        &'static [&'static dyn SignatureVerificationAlgorithm],
    )],
}

impl WebPkiSupportedAlgorithms {
    /// Return all the `scheme` items in `mapping`, maintaining order.
    pub fn supported_schemes(&self) -> Vec<SignatureScheme> {
        self.mapping
            .iter()
            .map(|item| item.0)
            .collect()
    }

    /// Return the first item in `mapping` that matches `scheme`.
    fn convert_scheme(
        &self,
        scheme: SignatureScheme,
    ) -> Result<&[&'static dyn SignatureVerificationAlgorithm], Error> {
        self.mapping
            .iter()
            .filter_map(|item| if item.0 == scheme { Some(item.1) } else { None })
            .next()
            .ok_or_else(|| PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme.into())
    }

    /// Return `true` if all cryptography is FIPS-approved.
    pub fn fips(&self) -> bool {
        self.all.iter().all(|alg| alg.fips())
            && self
                .mapping
                .iter()
                .all(|item| item.1.iter().all(|alg| alg.fips()))
    }
}

impl fmt::Debug for WebPkiSupportedAlgorithms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WebPkiSupportedAlgorithms {{ all: [ .. ], mapping: ")?;
        f.debug_list()
            .entries(self.mapping.iter().map(|item| item.0))
            .finish()?;
        write!(f, " }}")
    }
}

/// Wrapper around internal representation of a parsed certificate.
///
/// This is used in order to avoid parsing twice when specifying custom verification
pub struct ParsedCertificate<'a>(pub(crate) webpki::EndEntityCert<'a>);

impl ParsedCertificate<'_> {
    /// Get the parsed certificate's SubjectPublicKeyInfo (SPKI)
    pub fn subject_public_key_info(&self) -> SubjectPublicKeyInfoDer<'static> {
        self.0.subject_public_key_info()
    }
}

impl<'a> TryFrom<&'a CertificateDer<'a>> for ParsedCertificate<'a> {
    type Error = Error;
    fn try_from(value: &'a CertificateDer<'a>) -> Result<Self, Self::Error> {
        webpki::EndEntityCert::try_from(value)
            .map_err(pki_error)
            .map(ParsedCertificate)
    }
}

/// Verify a message signature using the `cert` public key and any supported scheme.
///
/// This function verifies the `dss` signature over `message` using the subject public key from
/// `cert`. Since TLS 1.2 doesn't provide enough information to map the `dss.scheme` into a single
/// [`SignatureVerificationAlgorithm`], this function will map to several candidates and try each in
/// succession until one succeeds or we exhaust all candidates.
///
/// See [WebPkiSupportedAlgorithms::mapping] for more information.
pub fn verify_tls12_signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    supported_schemes: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, Error> {
    let possible_algs = supported_schemes.convert_scheme(dss.scheme)?;
    let cert = webpki::EndEntityCert::try_from(cert).map_err(pki_error)?;

    let mut error = None;
    for alg in possible_algs {
        match cert.verify_signature(*alg, message, dss.signature()) {
            Err(err @ webpki::Error::UnsupportedSignatureAlgorithmForPublicKeyContext(_)) => {
                error = Some(err);
                continue;
            }
            Err(e) => return Err(pki_error(e)),
            Ok(()) => return Ok(HandshakeSignatureValid::assertion()),
        }
    }

    #[allow(deprecated)] // The `unwrap_or()` should be statically unreachable
    Err(pki_error(error.unwrap_or(
        webpki::Error::UnsupportedSignatureAlgorithmForPublicKey,
    )))
}

/// Verify a message signature using the `cert` public key and the first TLS 1.3 compatible
/// supported scheme.
///
/// This function verifies the `dss` signature over `message` using the subject public key from
/// `cert`. Unlike [verify_tls12_signature], this function only tries the first matching scheme. See
/// [WebPkiSupportedAlgorithms::mapping] for more information.
pub fn verify_tls13_signature(
    msg: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    supported_schemes: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, Error> {
    if !dss.scheme.supported_in_tls13() {
        return Err(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme.into());
    }

    let alg = supported_schemes.convert_scheme(dss.scheme)?[0];

    let cert = webpki::EndEntityCert::try_from(cert).map_err(pki_error)?;

    cert.verify_signature(alg, msg, dss.signature())
        .map_err(pki_error)
        .map(|_| HandshakeSignatureValid::assertion())
}

/// CratonVM addition — the peer's SubjectPublicKeyInfo, parsed WITHOUT
/// webpki's v3-only rule.
///
/// Why this exists: `EndEntityCert::try_from` runs `version3()`, so a v1
/// X.509 certificate cannot be parsed at all — not even to read its public
/// key. That rule is a PATH-BUILDING policy, and webpki itself says so by
/// exempting one position: `anchor_from_trusted_cert` catches
/// `UnsupportedCertVersion` and re-parses with a v1-capable parser, on the
/// reasoning that a v1 certificate carries no extensions and so no embedded
/// name constraints to worry about.
///
/// Extracting a public key to check a handshake signature is the same kind of
/// position. The JDK has no v1 restriction anywhere, and its `SSLEngine`
/// completes handshakes against servers whose end-entity certificate is v1 —
/// netty's own `mutual_auth_server.p12` / `localhost_server.pem` fixtures are
/// exactly that, and every connection to them died here with
/// `InvalidCertificate(Other(UnsupportedCertVersion))` even when the trust
/// decision had been delegated to a Java `TrustManager` and no path building
/// was being asked for at all.
///
/// This deliberately does NOT relax path building: `verify_server_cert` /
/// `verify_client_cert` still refuse a v1 certificate in a CA position, which
/// is correct under RFC 5280 (a v1 CA has no `basicConstraints`, so accepting
/// one would let any leaf sign for any other) and is what netty's
/// `mutual_auth_invalid_client.p12` fixture exists to exercise.
fn spki_lenient(cert: &CertificateDer<'_>) -> Result<SubjectPublicKeyInfoDer<'static>, Error> {
    match webpki::EndEntityCert::try_from(cert) {
        Ok(parsed) => Ok(parsed.subject_public_key_info()),
        Err(e) => match webpki::anchor_from_trusted_cert(cert) {
            // `TrustAnchor::subject_public_key_info` is the VALUE of the
            // `subjectPublicKeyInfo` field — webpki stores it without the outer
            // SEQUENCE header — whereas `SubjectPublicKeyInfoDer` (and
            // `RawPublicKeyEntity`, which parses it) wants the complete DER
            // element. Re-wrap it, or the raw-key path fails with `BadEncoding`
            // and the v1 tolerance buys nothing.
            Ok(anchor) => Ok(SubjectPublicKeyInfoDer::from(der_sequence(
                anchor.subject_public_key_info.as_ref(),
            ))),
            Err(_) => Err(pki_error(e)),
        },
    }
}

/// Wrap `value` in a DER `SEQUENCE` (tag 0x30) header.
fn der_sequence(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 6);
    out.push(0x30);
    let len = value.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len() - 1);
        let significant = &bytes[first..];
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
    out.extend_from_slice(value);
    out
}

/// As [`verify_tls12_signature`], but tolerant of a v1 end-entity certificate.
/// See [`spki_lenient`] for why that tolerance is correct here and nowhere
/// else. A CratonVM addition.
pub fn verify_tls12_signature_lenient(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    supported_schemes: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, Error> {
    match verify_tls12_signature(message, cert, dss, supported_schemes) {
        Err(Error::InvalidCertificate(_)) => {}
        other => return other,
    }
    let spki = spki_lenient(cert)?;
    let raw_key = webpki::RawPublicKeyEntity::try_from(&spki).map_err(pki_error)?;
    let possible_algs = supported_schemes.convert_scheme(dss.scheme)?;
    let mut error = None;
    for alg in possible_algs {
        match raw_key.verify_signature(*alg, message, dss.signature()) {
            Err(err @ webpki::Error::UnsupportedSignatureAlgorithmForPublicKeyContext(_)) => {
                error = Some(err);
                continue;
            }
            Err(e) => return Err(pki_error(e)),
            Ok(()) => return Ok(HandshakeSignatureValid::assertion()),
        }
    }
    #[allow(deprecated)] // The `unwrap_or()` should be statically unreachable
    Err(pki_error(error.unwrap_or(
        webpki::Error::UnsupportedSignatureAlgorithmForPublicKey,
    )))
}

/// As [`verify_tls13_signature`], but tolerant of a v1 end-entity certificate.
/// See [`spki_lenient`]. A CratonVM addition.
pub fn verify_tls13_signature_lenient(
    msg: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    supported_schemes: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, Error> {
    match verify_tls13_signature(msg, cert, dss, supported_schemes) {
        Err(Error::InvalidCertificate(_)) => {}
        other => return other,
    }
    let spki = spki_lenient(cert)?;
    verify_tls13_signature_with_raw_key(msg, &spki, dss, supported_schemes)
}

/// Verify a message signature using a raw public key and the first TLS 1.3 compatible
/// supported scheme.
pub fn verify_tls13_signature_with_raw_key(
    msg: &[u8],
    spki: &SubjectPublicKeyInfoDer<'_>,
    dss: &DigitallySignedStruct,
    supported_schemes: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, Error> {
    if !dss.scheme.supported_in_tls13() {
        return Err(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme.into());
    }

    let raw_key = webpki::RawPublicKeyEntity::try_from(spki).map_err(pki_error)?;
    let alg = supported_schemes.convert_scheme(dss.scheme)?[0];

    raw_key
        .verify_signature(alg, msg, dss.signature())
        .map_err(pki_error)
        .map(|_| HandshakeSignatureValid::assertion())
}

/// Verify that the end-entity certificate `end_entity` is a valid server cert
/// and chains to at least one of the trust anchors in the `roots` [RootCertStore].
///
/// `intermediates` contains all certificates other than `end_entity` that
/// were sent as part of the server's `Certificate` message. It is in the
/// same order that the server sent them and may be empty.
///
/// `revocation` controls how revocation checking is performed, if at all.
///
/// This function exists to be used by [`verify_server_cert_signed_by_trust_anchor`],
/// and differs only in providing a `Option<webpki::RevocationOptions>` argument. We
/// can't include this argument in `verify_server_cert_signed_by_trust_anchor` because
/// it will leak the webpki types into Rustls' public API.
pub(crate) fn verify_server_cert_signed_by_trust_anchor_impl(
    cert: &ParsedCertificate<'_>,
    roots: &RootCertStore,
    intermediates: &[CertificateDer<'_>],
    revocation: Option<webpki::RevocationOptions<'_>>,
    now: UnixTime,
    supported_algs: &[&dyn SignatureVerificationAlgorithm],
) -> Result<(), Error> {
    let result = cert.0.verify_for_usage(
        supported_algs,
        &roots.roots,
        intermediates,
        now,
        webpki::KeyUsage::server_auth(),
        revocation,
        None,
    );
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(pki_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::format;

    use super::*;

    #[test]
    fn certificate_debug() {
        assert_eq!(
            "CertificateDer(0x6162)",
            format!("{:?}", CertificateDer::from(b"ab".to_vec()))
        );
    }

    #[cfg(feature = "ring")]
    #[test]
    fn webpki_supported_algorithms_is_debug() {
        assert_eq!(
            "WebPkiSupportedAlgorithms { all: [ .. ], mapping: [ECDSA_NISTP384_SHA384, ECDSA_NISTP256_SHA256, ED25519, RSA_PSS_SHA512, RSA_PSS_SHA384, RSA_PSS_SHA256, RSA_PKCS1_SHA512, RSA_PKCS1_SHA384, RSA_PKCS1_SHA256] }",
            format!(
                "{:?}",
                crate::crypto::ring::default_provider().signature_verification_algorithms
            )
        );
    }
}
