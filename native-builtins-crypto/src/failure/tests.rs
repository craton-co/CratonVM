use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn constructors_carry_the_jdk_specified_class() {
    assert_eq!(
        CryptoFailure::invalid_key("bad").java_class(),
        "java/security/InvalidKeyException"
    );
    assert_eq!(
        CryptoFailure::malformed_signature("bad").java_class(),
        "java/security/SignatureException"
    );
    assert_eq!(
        CryptoFailure::no_such_algorithm("bad").java_class(),
        "java/security/NoSuchAlgorithmException"
    );
    assert_eq!(
        CryptoFailure::invalid_parameter("bad").java_class(),
        "java/security/InvalidAlgorithmParameterException"
    );
    assert_eq!(
        CryptoFailure::not_initialised("bad").java_class(),
        "java/lang/IllegalStateException"
    );
    assert_eq!(
        CryptoFailure::illegal_argument("bad").java_class(),
        "java/lang/IllegalArgumentException"
    );
}

/// Every advertised class name must be in JVM internal form (slashes, no
/// `L`/`;` wrapper) — that is what `new_object_initialized` expects. A
/// dotted name here would make the facade's construction fail and silently
/// degrade to the fallback exception.
#[test]
fn every_class_constant_is_internal_form() {
    for c in [
        INVALID_KEY_EXCEPTION,
        SIGNATURE_EXCEPTION,
        NO_SUCH_ALGORITHM_EXCEPTION,
        NO_SUCH_PROVIDER_EXCEPTION,
        INVALID_ALGORITHM_PARAMETER_EXCEPTION,
        KEY_STORE_EXCEPTION,
        CERTIFICATE_EXCEPTION,
        PROVIDER_EXCEPTION,
        ILLEGAL_STATE_EXCEPTION,
        ILLEGAL_ARGUMENT_EXCEPTION,
    ] {
        assert!(!c.contains('.'), "{c} is not internal form");
        assert!(c.contains('/'), "{c} is not internal form");
        assert!(c.ends_with("Exception"), "{c} is not an exception class");
    }
}

#[test]
fn display_uses_the_dotted_java_name() {
    let e = CryptoFailure::invalid_key("modulus is zero");
    assert_eq!(
        e.to_string(),
        "java.security.InvalidKeyException: modulus is zero"
    );
    assert_eq!(e.message(), "modulus is zero");
}
