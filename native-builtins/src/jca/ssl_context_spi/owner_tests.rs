use super::fixtures::*;
use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

// MUST DELEGATE.
#[test]
fn a_third_party_spi_makes_the_context_foreign() {
    let mut ctx = MockNativeContext::new();
    let (obj, spi) = make_real_context(
        &mut ctx,
        "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
        "TLSv1.3",
    );
    assert_eq!(context_owner(&ctx, obj), SslContextOwner::Foreign(spi));
    assert_eq!(foreign_spi(&ctx, &[Value::Object(Some(obj))]), Some(spi));
}

// MUST NOT DELEGATE — the twin. `seed_sunjsse_services` makes a real
// context around a genuine `sun.security.ssl.SSLContextImpl` reachable,
// and that one works today precisely BECAUSE the natives answer for it.
#[test]
fn a_sun_security_ssl_spi_stays_ours() {
    let mut ctx = MockNativeContext::new();
    let (obj, spi) = make_real_context(&mut ctx, "sun/security/ssl/SSLContextImpl", "TLSv1.3");
    assert_eq!(context_owner(&ctx, obj), SslContextOwner::Adopted(spi));
    assert_eq!(foreign_spi(&ctx, &[Value::Object(Some(obj))]), None);
}

// MUST NOT DELEGATE — a context CratonVM allocated itself.
#[test]
fn a_synthetic_context_is_never_foreign() {
    let mut ctx = MockNativeContext::new();
    let obj = make_synthetic_context(&mut ctx, "TLSv1.2");
    assert_eq!(context_owner(&ctx, obj), SslContextOwner::Synthetic);
    assert_eq!(foreign_spi(&ctx, &[Value::Object(Some(obj))]), None);
}

// The layout half of the defect, separately from the delegation half:
// `getProtocol()` on a REAL context must read `protocol`, not slot 0.
// Both real kinds, because the bug is the LAYOUT, not the SPI's author.
#[test]
fn a_real_context_reports_its_protocol_field_not_slot_zero() {
    for spi_class in [
        "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
        "sun/security/ssl/SSLContextImpl",
    ] {
        let mut ctx = MockNativeContext::new();
        let (obj, _) = make_real_context(&mut ctx, spi_class, "TLSv1.3");
        let v = real_context_protocol(&ctx, &[Value::Object(Some(obj))])
            .unwrap_or_else(|| panic!("{spi_class}: must answer for a real context"));
        let s = match v {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            other => panic!("{spi_class}: getProtocol answered {other:?}"),
        };
        assert_eq!(s, "TLSv1.3");
        // And slot 0 — what the native read before — is NOT that.
        match ctx.get_field(obj, 0) {
            Value::Object(Some(o)) => assert_ne!(
                ctx.read_string(o).unwrap_or_default(),
                "TLSv1.3",
                "the test would pass for the wrong reason"
            ),
            other => panic!("slot 0 was {other:?}"),
        }
    }
}

// MUST STILL WORK: a synthetic context keeps its own protocol handling,
// so `real_context_protocol` must decline rather than answer.
#[test]
fn a_synthetic_context_keeps_its_own_protocol_path() {
    let mut ctx = MockNativeContext::new();
    let obj = make_synthetic_context(&mut ctx, "TLSv1.2");
    assert!(real_context_protocol(&ctx, &[Value::Object(Some(obj))]).is_none());
}

// An object in the `contextSpi` slot that is NOT an `SSLContextSpi` is not
// somebody's SPI. This is the guard that keeps a future synthetic layout
// change — a `String` or a `KeyManager[]` landing in slot 1 — from being
// read as a foreign provider and delegated into thin air.
#[test]
fn a_non_spi_object_in_that_slot_is_not_a_provider() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("javax/net/ssl/SSLContext")
        .expect("context class");
    declare_real_ssl_context(&ctx, cid);
    let obj = ctx.alloc_object(cid, 3);
    let impostor = ctx.create_string("not an SPI");
    ctx.set_field(obj, 1, Value::Object(Some(impostor)));
    assert_eq!(context_owner(&ctx, obj), SslContextOwner::Synthetic);
}
