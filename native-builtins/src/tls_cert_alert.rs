// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Which TLS alert a client-side `TrustManager` rejection becomes.
//!
//! ## The defect this exists for
//!
//! CratonVM's client answered EVERY certificate rejection with `access_denied`
//! (alert 49). The verifier-time trust check turned a `TrustManager`'s
//! `CertificateException` into
//! `rustls::CertificateError::ApplicationVerificationFailure`, and rustls maps
//! that one — correctly, for what it means — to `AlertDescription::AccessDenied`.
//!
//! `access_denied` is a POLICY refusal: RFC 8446 §6.2 defines it as "the sender
//! was unable to negotiate an acceptable set of security parameters given the
//! options available". A peer that rejected a CERTIFICATE has to say so with a
//! certificate alert, and anything that distinguishes them — a log, a
//! middlebox, the peer's own error type — draws the wrong conclusion.
//!
//! Measured on `io.netty.handler.ssl.SslErrorTest` (Azure Linux, JDK 25, one
//! fork per VM, `gen-openssl-args.sh` so `OpenSsl.isAvailable()` is true):
//! HotSpot `found=72 ok=72`, CratonVM `found=72 ok=60 failed=12`. All 12 are
//! `clientProvider = JDK` client-side rejections; the server is OpenSSL and
//! reports `TLSV1_ALERT_ACCESS_DENIED`. netty's `SslErrorTest.verifyException`
//! accepts "expired", "bad", "revoked" and — for exactly this JDK-client
//! case — a blanket "unknown", so CratonVM was failing to match even the
//! escape hatch netty added to be generous.
//!
//! The POST-handshake path already had this right
//! (`reject_peer_with_fatal_alert` queues `certificate_unknown`). It was the
//! newer verifier-time path — the one that made a client rejection reach the
//! server while it is still handshaking — that lost the alert. Both now go
//! through this module, so they cannot drift apart again.
//!
//! ## Why one constant is not the fix
//!
//! `certificate_unknown` is JSSE's DEFAULT, not JSSE's rule.
//! `sun.security.ssl.CertificateMessage.getCertificateAlert` (JDK 25, verified
//! against this host's `src.zip`) branches on the `CertPathValidatorException`
//! CAUSE of the manager's exception:
//!
//! ```text
//! (no CPVE cause)                 -> certificate_unknown
//! REVOKED                         -> certificate_revoked
//! UNDETERMINED_REVOCATION_STATUS  -> certificate_unknown
//! EXPIRED                         -> certificate_expired
//! INVALID_SIGNATURE, NOT_YET_VALID-> bad_certificate
//! ALGORITHM_CONSTRAINED           -> unsupported_certificate
//!                                    (bad_certificate on TLS 1.3 when the
//!                                     message names MD5withX / SHA1withX)
//! ```
//!
//! Note `NOT_YET_VALID -> bad_certificate`: rustls's own
//! `CertificateError::NotValidYet` maps to `certificate_expired`, so JSSE and
//! rustls genuinely disagree here and the JSSE answer is the one being
//! imitated. That is why [`JsseCertAlert::certificate_error`] picks its rustls
//! variant FOR ITS ALERT rather than for its name, and why
//! `every_alert_survives_the_rustls_mapping` pins each pair.
//!
//! **Deliberately not modelled:** JSSE substitutes
//! `bad_certificate_status_response` for the two revocation reasons when OCSP
//! stapling is active on the connection (`chc.staplingActive`). CratonVM's
//! engine does not carry that bit to this point, and inventing it would mean
//! guessing; the un-stapled answer is what an un-stapled handshake gets, which
//! is every row this was measured against.

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

/// The alert JSSE raises for a `CertificateException` out of a client-side
/// `TrustManager` — a transcription of
/// `sun.security.ssl.CertificateMessage.getCertificateAlert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsseCertAlert {
    /// JSSE's default, and the answer for every exception with no
    /// `CertPathValidatorException` cause.
    CertificateUnknown,
    CertificateExpired,
    CertificateRevoked,
    BadCertificate,
    UnsupportedCertificate,
}

impl JsseCertAlert {
    /// The wire alert. This is the thing the peer sees and the thing the
    /// measurement is against.
    pub fn alert_description(self) -> rustls::AlertDescription {
        match self {
            Self::CertificateUnknown => rustls::AlertDescription::CertificateUnknown,
            Self::CertificateExpired => rustls::AlertDescription::CertificateExpired,
            Self::CertificateRevoked => rustls::AlertDescription::CertificateRevoked,
            Self::BadCertificate => rustls::AlertDescription::BadCertificate,
            Self::UnsupportedCertificate => rustls::AlertDescription::UnsupportedCertificate,
        }
    }

    /// The `rustls::CertificateError` a verifier must return to make rustls
    /// emit [`Self::alert_description`].
    ///
    /// **These variants are chosen for their ALERT, not for their meaning**, and
    /// two of them are a poor description of what happened:
    /// `BadEncoding` for a not-yet-valid or badly-signed certificate, and
    /// `InvalidPurpose` for an algorithm-constrained one. rustls offers no
    /// "send exactly this alert" channel out of a verifier, so the alert can
    /// only be reached through the variant that maps to it.
    ///
    /// `every_alert_survives_the_rustls_mapping` asserts each pair against
    /// rustls's own `From<CertificateError> for AlertDescription`, so a rustls
    /// upgrade that re-tables the mapping fails the build rather than silently
    /// changing what CratonVM puts on the wire.
    pub fn certificate_error(self, detail: String) -> rustls::CertificateError {
        match self {
            Self::CertificateUnknown => rustls::CertificateError::Other(rustls::OtherError(
                std::sync::Arc::new(TrustManagerRejection(detail)),
            )),
            Self::CertificateExpired => rustls::CertificateError::Expired,
            Self::CertificateRevoked => rustls::CertificateError::Revoked,
            Self::BadCertificate => rustls::CertificateError::BadEncoding,
            Self::UnsupportedCertificate => rustls::CertificateError::InvalidPurpose,
        }
    }
}

/// Carrier for the rejection detail on the `certificate_unknown` path, so the
/// manager's own message survives into rustls's error text instead of being
/// replaced by a bare variant name.
#[derive(Debug)]
struct TrustManagerRejection(String);

impl std::fmt::Display for TrustManagerRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TrustManager rejected the peer certificate chain: {}",
            self.0
        )
    }
}

impl std::error::Error for TrustManagerRejection {}

/// `java.security.cert.CertPathValidatorException`, internal form.
const CPVE_CLASS: &str = "java/security/cert/CertPathValidatorException";

/// `CertPathValidatorException$BasicReason`, internal form. The `Reason`
/// interface is public and an application may implement its own; only
/// `BasicReason` constants carry the meanings `getCertificateAlert` branches
/// on, so anything else must fall through to the default.
const BASIC_REASON_CLASS: &str = "java/security/cert/CertPathValidatorException$BasicReason";

/// Classify a `TrustManager`'s exception the way JSSE does.
///
/// Runs while the exception is still in hand and a live `NativeContext` is
/// available — NOT on the unwinding path, where calling Java is not allowed.
pub fn jsse_cert_alert_for(ctx: &mut dyn NativeContext, exc: ObjectRef) -> JsseCertAlert {
    let Some(cause) = cause_of(ctx, exc) else {
        return JsseCertAlert::CertificateUnknown;
    };
    if !is_cert_path_validator_exception(ctx, cause) {
        return JsseCertAlert::CertificateUnknown;
    }
    let Some(reason) = basic_reason_name(ctx, cause) else {
        return JsseCertAlert::CertificateUnknown;
    };
    match reason.as_str() {
        "REVOKED" => JsseCertAlert::CertificateRevoked,
        "EXPIRED" => JsseCertAlert::CertificateExpired,
        "INVALID_SIGNATURE" | "NOT_YET_VALID" => JsseCertAlert::BadCertificate,
        "ALGORITHM_CONSTRAINED" => algorithm_constrained_alert(ctx, exc),
        // UNDETERMINED_REVOCATION_STATUS, UNSPECIFIED, and any reason a later
        // JDK adds: JSSE leaves the default in place.
        _ => JsseCertAlert::CertificateUnknown,
    }
}

/// JSSE's TLS 1.3 carve-out: an algorithm-constrained rejection is normally
/// `unsupported_certificate`, but "Per TLSv1.3 RFC we MUST abort the handshake
/// with a `bad_certificate` alert if we reject certificate because of the
/// signature using MD5 or SHA1 algorithm" — decided, in JSSE, by upper-casing
/// the OUTER exception's message and looking for `MD5WITH` / `SHA1WITH`.
///
/// Transcribed including the protocol condition. CratonVM negotiates TLS 1.3
/// by default, so the branch is live rather than theoretical; the message test
/// is JSSE's own and is quoted here because it is a string match, not a
/// structural one, and a reader will otherwise assume it is stricter than it is.
fn algorithm_constrained_alert(ctx: &mut dyn NativeContext, exc: ObjectRef) -> JsseCertAlert {
    match message_of(ctx, exc) {
        Some(m) => {
            let up = m.to_uppercase();
            if up.contains("MD5WITH") || up.contains("SHA1WITH") {
                JsseCertAlert::BadCertificate
            } else {
                JsseCertAlert::UnsupportedCertificate
            }
        }
        // JSSE dereferences `cexc.getMessage()` here without a null check, so a
        // null message NPEs inside its own alert selection. Answering the
        // un-carved-out alert is the behaviour that method would have had.
        None => JsseCertAlert::UnsupportedCertificate,
    }
}

fn cause_of(ctx: &mut dyn NativeContext, exc: ObjectRef) -> Option<ObjectRef> {
    match ctx.invoke_virtual(exc, "getCause", "()Ljava/lang/Throwable;", &[]) {
        Ok(Some(Value::Object(Some(c)))) if c != exc => Some(c),
        _ => None,
    }
}

fn message_of(ctx: &mut dyn NativeContext, exc: ObjectRef) -> Option<String> {
    match ctx.invoke_virtual(exc, "getMessage", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// Superclass walk BY NAME, deliberately — the same reasoning as
/// `t27_tls`'s `X509ExtendedTrustManager` test: `class_id_by_name` answers
/// `None` both for "nobody has this name" and for "several loaders do", and a
/// `None` there would silently degrade to the default alert while looking like
/// a working check.
fn is_cert_path_validator_exception(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    class_chain_contains(ctx, obj, CPVE_CLASS)
}

fn class_chain_contains(ctx: &dyn NativeContext, obj: ObjectRef, wanted: &str) -> bool {
    let mut cur = Some(ctx.class_id_of_object(obj));
    while let Some(cid) = cur {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(wanted) {
            return true;
        }
        cur = ctx.superclass_of(cid);
    }
    false
}

/// `cpve.getReason()`, as a `BasicReason` constant name.
///
/// `None` when the reason is absent or is an application's own `Reason`
/// implementation, both of which JSSE leaves on the default branch.
fn basic_reason_name(ctx: &mut dyn NativeContext, cpve: ObjectRef) -> Option<String> {
    let reason = match ctx.invoke_virtual(
        cpve,
        "getReason",
        "()Ljava/security/cert/CertPathValidatorException$Reason;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(r)))) => r,
        _ => return None,
    };
    if !class_chain_contains(ctx, reason, BASIC_REASON_CLASS) {
        return None;
    }
    match ctx.invoke_virtual(reason, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The coupling this module is built on: each `JsseCertAlert` must reach
    /// its wire alert THROUGH rustls's own `CertificateError` table. Two of the
    /// variants are chosen for their alert rather than their meaning, so if a
    /// rustls upgrade re-tables the mapping this is the thing that must fail.
    #[test]
    fn every_alert_survives_the_rustls_mapping() {
        for alert in [
            JsseCertAlert::CertificateUnknown,
            JsseCertAlert::CertificateExpired,
            JsseCertAlert::CertificateRevoked,
            JsseCertAlert::BadCertificate,
            JsseCertAlert::UnsupportedCertificate,
        ] {
            let err = alert.certificate_error("detail".to_string());
            let got = rustls::AlertDescription::from(err);
            assert_eq!(
                got,
                alert.alert_description(),
                "{alert:?}: the rustls variant chosen for this alert no longer maps to it"
            );
        }
    }

    /// The defect in one line: nothing may reach `access_denied`.
    #[test]
    fn no_certificate_rejection_can_reach_access_denied() {
        for alert in [
            JsseCertAlert::CertificateUnknown,
            JsseCertAlert::CertificateExpired,
            JsseCertAlert::CertificateRevoked,
            JsseCertAlert::BadCertificate,
            JsseCertAlert::UnsupportedCertificate,
        ] {
            assert_ne!(
                alert.alert_description(),
                rustls::AlertDescription::AccessDenied,
                "{alert:?} maps to access_denied, which is a POLICY refusal — \
                 a rejected CERTIFICATE must say so with a certificate alert"
            );
        }
        // And the variant that was there before still does, so the test above
        // is not passing because `AccessDenied` became unreachable in rustls.
        assert_eq!(
            rustls::AlertDescription::from(
                rustls::CertificateError::ApplicationVerificationFailure
            ),
            rustls::AlertDescription::AccessDenied,
        );
    }

    /// The detail must survive onto the wire-visible error text, because it is
    /// the only place a `TrustManager`'s own message can still be read.
    #[test]
    fn the_certificate_unknown_arm_carries_the_managers_message() {
        let err = JsseCertAlert::CertificateUnknown
            .certificate_error("java.security.cert.CertificateExpiredException: NotAfter".into());
        let text = format!("{err}");
        assert!(
            text.contains("CertificateExpiredException") && text.contains("NotAfter"),
            "detail lost: {text}"
        );
    }
}
