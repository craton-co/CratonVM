use super::*;
use cratonvm_native_api::test_mock::MockNativeContext;
// The mock implements these through the trait, so the trait has to be in
// scope for `alloc_object` / `set_field` / `class_id_by_name` to resolve.
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

/// Slot 2 of the receiver is a `java.lang.Class`, exactly as it is on a
/// `KeyStoreDelegator`. The unwrap must refuse it and answer the receiver.
#[test]
fn a_slot_two_that_is_not_a_keystore_spi_is_not_followed() {
    let mut ctx = MockNativeContext::new();
    let spi_cid = ctx.declare_class("java/security/KeyStoreSpi", &[]);
    let delegator_cid = ctx.declare_class(
        "sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12",
        &[
            ("primaryType", "Ljava/lang/String;"),
            ("secondaryType", "Ljava/lang/String;"),
            ("primaryKeyStore", "Ljava/lang/Class;"),
        ],
    );
    let class_mirror_cid = ctx.declare_class("java/lang/Class", &[]);
    assert_ne!(
        spi_cid, class_mirror_cid,
        "the mock handed the same ClassId to two names, so this test could not tell \
         a Class mirror from a KeyStoreSpi and would pass on the broken tree"
    );

    let mirror = ctx.alloc_object(class_mirror_cid, 0);
    let delegator = ctx.alloc_object(delegator_cid, 8);
    ctx.set_field(delegator, 2, Value::Object(Some(mirror)));

    let got = unwrap_keystore_spi(&mut ctx, delegator);
    assert_eq!(
        got, delegator,
        "unwrap_keystore_spi followed slot 2 to a java.lang.Class. That is how \
         KeyStore.setEntry came to store its entries against the store id of a \
         process-global class mirror: setEntry returned normally, threw nothing, and \
         size()/aliases()/getKey() all answered as if it had never been called."
    );
}

/// The same fallback, with a slot 2 that IS a `KeyStoreSpi`. It must still
/// be followed — otherwise the narrowing above has broken the real
/// `java.security.KeyStore` wrapper it was never aimed at.
#[test]
fn a_slot_two_that_is_a_keystore_spi_is_still_followed() {
    let mut ctx = MockNativeContext::new();
    let spi_cid = ctx.declare_class("java/security/KeyStoreSpi", &[]);
    let wrapper_cid = ctx.declare_class(
        "java/security/KeyStore",
        &[
            ("type", "Ljava/lang/String;"),
            ("provider", "Ljava/security/Provider;"),
            ("keyStoreSpi", "Ljava/security/KeyStoreSpi;"),
        ],
    );

    let spi = ctx.alloc_object(spi_cid, 1);
    let wrapper = ctx.alloc_object(wrapper_cid, 4);
    // By INDEX only: the by-name tier is what a real wrapper answers, and
    // this test is about the tier underneath it.
    ctx.set_field(wrapper, 2, Value::Object(Some(spi)));

    let got = unwrap_keystore_spi(&mut ctx, wrapper);
    assert_eq!(
        got, spi,
        "the slot-2 fallback stopped following a genuine KeyStoreSpi. That tier is the \
         only one that resolves a synthetic KeyStore mirror, so breaking it drops every \
         staged keystore identity -- the KeyManagerFactory/TrustManagerFactory defect \
         `keystore_id_from_object` documents."
    );
}

/// When `java/security/KeyStoreSpi` cannot be resolved at all, the check
/// cannot adjudicate and the historical blind behaviour is kept.
///
/// Written down as a test because it is a decision, not an accident: an
/// image with no such class (a `--synthetic-jdk` mirror) must not have its
/// unwrap silently changed by a guard that has nothing to compare against.
#[test]
fn an_unresolvable_keystore_spi_class_keeps_the_old_behaviour() {
    let mut ctx = MockNativeContext::new();
    // Deliberately NOT declaring `java/security/KeyStoreSpi`.
    let carrier_cid = ctx.declare_class("cratonvm/internal/ks/Carrier", &[]);
    let inner_cid = ctx.declare_class("cratonvm/internal/ks/Inner", &[]);
    let inner = ctx.alloc_object(inner_cid, 1);
    let carrier = ctx.alloc_object(carrier_cid, 6);
    ctx.set_field(carrier, 2, Value::Object(Some(inner)));

    assert!(
        ctx.class_id_by_name("java/security/KeyStoreSpi").is_none(),
        "this test's premise is that the class is unresolvable; the mock resolved it, \
         so the branch under test was never reached"
    );
    assert_eq!(
        unwrap_keystore_spi(&mut ctx, carrier),
        inner,
        "with no KeyStoreSpi to compare against, the fallback must behave as it always \
         did rather than start refusing on an image that cannot answer the question"
    );
}
