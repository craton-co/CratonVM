// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The fail-loud error type for the cryptographic compatibility kernels.
//!
//! ## Why this exists
//!
//! A security API that cannot do what it was asked must say so. The dangerous
//! failure mode is the *ordinary-looking* one: `Signature.sign()` handing back
//! an empty `byte[]`, `Signature.verify()` handing back `false` because the
//! **key** was rejected rather than because the **signature** did not match, or
//! a block cipher writing a zero block when the key schedule was malformed.
//! Every one of those is indistinguishable, at the call site, from a
//! legitimate negative security decision — so the caller records "not signed"
//! or "did not verify" when the truth is "we never checked".
//!
//! This crate is a **pure kernel island**: it has no [`NativeContext`], so it
//! cannot construct or throw a Java `Throwable` itself (see the crate docs in
//! `lib.rs` — Java-object marshalling lives in the `native-builtins` facade).
//! What it *can* do is refuse to answer, and name the exact JDK exception the
//! facade must raise. `CryptoFailure` is that refusal: an error carrying the
//! **internal-form class name** of the `java.security` exception the JDK
//! specification requires, plus the detail message.
//!
//! ## How the facade turns one of these into a thrown exception
//!
//! `native-builtins` already has the throw helper — see
//! `native-builtins/src/phases_early.rs:14672` (`throw_jca_exc`) and the
//! equivalent `native-builtins/src/phases_late/bouncycastle.rs:6040`
//! (`bc_gost_throw_crypto_exception`). Both do:
//!
//! ```text
//! let detail = ctx.create_string(msg);
//! match ctx.new_object_initialized(class_name, "(Ljava/lang/String;)V",
//!                                  &[Value::Object(Some(detail))]) {
//!     Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
//!     _ => RuntimeError::IllegalStateException { .. }.into(),  // still loud
//! }
//! ```
//!
//! so the mapping is mechanical:
//!
//! ```text
//! err => throw_jca_exc(ctx, err.java_class(), err.message())
//! ```
//!
//! The fallback arm matters: if the exception class itself cannot be built
//! (synthetic-JDK mode, class not on the boot path) the helper still raises an
//! unchecked exception rather than returning a value. There is deliberately no
//! arm that yields `Ok(...)`.
//!
//! ## Rule
//!
//! **A cryptographic kernel never encodes failure as output.** It returns
//! `Err(CryptoFailure)`, or — where the caller's contract genuinely admits a
//! negative answer, as `verify()` does for a real digest mismatch — it returns
//! `Ok(false)`. Those two are different things and this crate keeps them
//! different. See `docs/security/crypto-failure-contract.md`.

use std::fmt;

/// `java.security.InvalidKeyException` — the key is malformed, out of range,
/// or otherwise unusable. Thrown *instead of* returning `false`, because a
/// rejected key means no verification decision was ever made.
pub const INVALID_KEY_EXCEPTION: &str = "java/security/InvalidKeyException";

/// `java.security.SignatureException` — the signature *encoding* is malformed
/// (wrong length for the modulus, empty, un-parseable). Distinct from a
/// well-formed signature that simply does not match, which is `Ok(false)`.
pub const SIGNATURE_EXCEPTION: &str = "java/security/SignatureException";

/// `java.security.NoSuchAlgorithmException` — the requested algorithm is not
/// implemented by this build.
pub const NO_SUCH_ALGORITHM_EXCEPTION: &str = "java/security/NoSuchAlgorithmException";

/// `java.security.NoSuchProviderException` — the requested provider is not
/// registered.
pub const NO_SUCH_PROVIDER_EXCEPTION: &str = "java/security/NoSuchProviderException";

/// `java.security.InvalidAlgorithmParameterException` — the parameters (curve,
/// mode, round count, IV) are unsupported or inconsistent.
pub const INVALID_ALGORITHM_PARAMETER_EXCEPTION: &str =
    "java/security/InvalidAlgorithmParameterException";

/// `java.security.KeyStoreException`.
pub const KEY_STORE_EXCEPTION: &str = "java/security/KeyStoreException";

/// `java.security.cert.CertificateException`.
pub const CERTIFICATE_EXCEPTION: &str = "java/security/cert/CertificateException";

/// `java.security.ProviderException` — an *internal* provider failure. The JDK
/// uses this unchecked exception for "the provider was asked to do something
/// it accepted and then could not complete"; it is the correct type for an
/// invariant violation reached from inside a provider engine class.
pub const PROVIDER_EXCEPTION: &str = "java/security/ProviderException";

/// `java.lang.IllegalStateException` — the object was not initialised.
pub const ILLEGAL_STATE_EXCEPTION: &str = "java/lang/IllegalStateException";

/// `java.lang.IllegalArgumentException` — the argument is structurally wrong
/// (this is what BouncyCastle itself throws for a bad key length or an odd
/// ChaCha round count, so kernels mirroring BC use it).
pub const ILLEGAL_ARGUMENT_EXCEPTION: &str = "java/lang/IllegalArgumentException";

/// A refusal from a cryptographic kernel, naming the JDK exception the caller
/// must raise.
///
/// Deliberately **not** convertible to a plain `bool`/`Vec<u8>`: there is no
/// `unwrap_or_default()`-shaped escape hatch on this type, because that is the
/// exact bug class it exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoFailure {
    java_class: &'static str,
    message: String,
}

impl CryptoFailure {
    /// Build a failure naming `java_class` (internal form, e.g.
    /// `java/security/InvalidKeyException`).
    pub fn new(java_class: &'static str, message: impl Into<String>) -> Self {
        Self {
            java_class,
            message: message.into(),
        }
    }

    /// The internal-form class name of the exception the facade must throw.
    pub fn java_class(&self) -> &'static str {
        self.java_class
    }

    /// The detail message for the exception's `(String)` constructor.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// `InvalidKeyException` — key rejected, so no security decision was made.
    pub fn invalid_key(message: impl Into<String>) -> Self {
        Self::new(INVALID_KEY_EXCEPTION, message)
    }

    /// `SignatureException` — the signature bytes are malformed.
    pub fn malformed_signature(message: impl Into<String>) -> Self {
        Self::new(SIGNATURE_EXCEPTION, message)
    }

    /// `NoSuchAlgorithmException` — unimplemented algorithm.
    pub fn no_such_algorithm(message: impl Into<String>) -> Self {
        Self::new(NO_SUCH_ALGORITHM_EXCEPTION, message)
    }

    /// `InvalidAlgorithmParameterException` — unsupported/inconsistent params.
    pub fn invalid_parameter(message: impl Into<String>) -> Self {
        Self::new(INVALID_ALGORITHM_PARAMETER_EXCEPTION, message)
    }

    /// `IllegalStateException` — engine used before it was initialised.
    pub fn not_initialised(message: impl Into<String>) -> Self {
        Self::new(ILLEGAL_STATE_EXCEPTION, message)
    }

    /// `IllegalArgumentException` — structurally invalid argument. Used where
    /// the kernel mirrors a BouncyCastle engine that throws this itself.
    pub fn illegal_argument(message: impl Into<String>) -> Self {
        Self::new(ILLEGAL_ARGUMENT_EXCEPTION, message)
    }
}

impl fmt::Display for CryptoFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.java_class.replace('/', "."), self.message)
    }
}

impl std::error::Error for CryptoFailure {}

/// Result alias for kernels that must be able to refuse.
pub type CryptoResult<T> = Result<T, CryptoFailure>;

#[cfg(test)]
mod tests {
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
}
