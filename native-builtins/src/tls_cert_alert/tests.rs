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
fn cause_of_self_causal_throwable_returns_none_after_gc_relocation() {
    use crate::test_utils::MockNativeContext;
    use std::cell::Cell;

    thread_local! {
        static SELF_CAUSE_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }

    fn hook(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<cratonvm_types::error::MethodCallResult> {
        if method_name != "getCause" {
            return None;
        }
        let (old, new) = SELF_CAUSE_MOVE.with(|m| m.get());
        ctx.remap_native_pin_addr_for_test(old, new);
        let moved = unsafe {
            ObjectRef::from_raw(new as *mut u8)
        };
        Some(Ok(Some(Value::Object(Some(moved)))))
    }

    let mut ctx = MockNativeContext::new();
    let exc_old = ctx.fresh_object_ref();
    let exc_new = ctx.fresh_object_ref();
    SELF_CAUSE_MOVE.with(|m| m.set((exc_old.as_ptr() as usize, exc_new.as_ptr() as usize)));
    ctx.set_invoke_virtual_hook(hook);

    let got = cause_of(&mut ctx, exc_old);
    assert_eq!(
        got, None,
        "self-causal throwable (getCause() == this) must return None even when GC moves `this` during getCause()"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "all native pins must be released");
}

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
