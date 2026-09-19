// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.1 + WP6.2 — Java Cryptography Architecture (JCA) provider chain
//! and `MessageDigest` engine, available in real-JDK mode.
//!
//! The previous registrations under `phases_early::register_phase53_security`
//! and `lib::register_security_natives` were gated behind
//! `register_synthetic_overrides` (i.e. `--synthetic-jdk` only). Real-JDK
//! mode therefore tried to interpret JDK 25's `java.security.Security` /
//! `java.security.Provider` / `java.security.MessageDigest` bytecode — which
//! reaches into `sun.security.util.Debug`, `Provider$ServiceKey`, the
//! `spiMap` reflective bootstrap and a half-dozen other surfaces we do not
//! support yet. The result was a NoSuchAlgorithmException-like failure on
//! the first `MessageDigest.getInstance(...)` call, blocking every
//! security-touching app.
//!
//! `register_jca_natives` re-uses the proven streaming MD5/SHA-1/SHA-2
//! and `sha3` crate-backed SHA-3 implementations from the synthetic path
//! and surfaces them as native overrides on the real-JDK class names
//! (`java/security/Security`, `java/security/Provider`,
//! `java/security/MessageDigest`). Provider chain is seeded with the
//! standard JDK 25 Sun-family list — registered in the order HotSpot
//! reports — and stays mutable through `Security.addProvider` /
//! `insertProviderAt` / `removeProvider`.
//!
//! Out-of-scope (left for sibling waves):
//!   * WP6.3 — `javax.crypto.Cipher`
//!   * WP6.4 / WP6.6 — `java.security.Signature` + ASN.1
//!   * WP6.5 — BouncyCastle loading
//!   * WP6.7 — `java.security.SecureRandom`

pub mod message_digest;
pub mod provider_chain;
// WP6.3 Cipher class-init shim — landed alongside provider_chain to keep the
// JCA namespace self-contained.
pub mod cipher;
// WP6.4 + WP6.6 — KeyPairGenerator / KeyFactory / KeyPair / Public+PrivateKey.
// Without these natives, `KeyPairGenerator.getInstance("RSA")` falls through
// to the real-JDK bytecode which routes via
// `sun.security.jca.GetInstance.getServices(...)` and NPEs on a Provider
// whose `getServices()` we cannot fully materialize. Registering native
// overrides for the public surface short-circuits that bytecode path.
pub mod key_factory;
// WP6.4 — `java.security.Signature` real-JDK natives. Pairs with key_factory.
pub mod signature;
// `javax.crypto.KeyAgreement` — ECDH, by driving the real SunEC ECDH SPI.
pub mod key_agreement;
// `javax.crypto.KEM` — ML-KEM (FIPS 203) encaps/decaps, by driving the real
// SunJCE `ML_KEM_Impls` KEM SPI. Companion to the ML-DSA Signature route in
// `signature` and the PQC keygen route in `key_factory`.
pub mod kem;
// WP6.6 — `javax.security.auth.x500.X500Principal` DER + RFC 4514 round-trip.
pub mod x500;

/// The DN grammar `x500.rs` renders and parses: RDNs, AVAs and the DER string
/// type each attribute value carries.
pub mod x500_name;
// ASN.1 helper used by x500 (DER encode/decode primitives). No registrations
// of its own — exists as a compile-time module for x500.rs to depend on.
pub mod asn1;
// The `javax.net.ssl.SSLContextSpi` boundary: which SPI implementations
// CratonVM claims as its own, and how the `javax/net/ssl/SSLContext` natives
// ask. Three registration sets stand on that class (`tls.rs`, `net_phase_e.rs`,
// `phases_late/ssl_security.rs`) and last-write-wins decides which answers, so
// the decision lives here rather than in whichever one happens to win.
pub mod ssl_context_spi;

use cratonvm_native_api::NativeMethodRegistry;

/// Build a `java.security.Provider` object carrying `name`.
///
/// Every JCA engine class here is served by natives that keep their state
/// off-object, so the real `provider` field is never written and the real
/// `getProvider()` body either returns `null` or dies on `synchronized (lock)`
/// with a null `lock`. Each engine therefore has to answer `getProvider()`
/// itself — and this is the one place that knows how to shape the object, so
/// the four that need it (`MessageDigest`, `Mac`, `SecretKeyFactory`,
/// `Signature`) do not each carry their own copy of the field layout.
pub(crate) fn make_named_provider(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    name: &str,
) -> Result<cratonvm_types::ObjectRef, cratonvm_types::error::MethodCallFailed> {
    use cratonvm_types::Value;
    let p = crate::try_alloc_concurrent_synthetic(ctx, "java/security/Provider", 8)?;
    let name_s = ctx.create_string(name);
    let info = ctx.create_string(&format!("{name} provider (cratonvm)"));
    let ver_str = ctx.create_string("25");
    ctx.set_field_by_name(p, "name", Value::Object(Some(name_s)));
    ctx.set_field_by_name(p, "version", Value::Double(25.0));
    ctx.set_field_by_name(p, "versionStr", Value::Object(Some(ver_str)));
    ctx.set_field_by_name(p, "info", Value::Object(Some(info)));
    // Raw slots too: `message_digest::md_get_provider` established that both the
    // named fields and slots 0-2 are read depending on how the object is reached.
    ctx.set_field(p, 0, Value::Object(Some(name_s)));
    ctx.set_field(p, 1, Value::Double(25.0));
    ctx.set_field(p, 2, Value::Object(Some(info)));
    Ok(p)
}

/// Wire every JCA native override needed by `DigestProbe.java`,
/// `SigProbe.java`, and any other JDK 25 client that goes through
/// `Security.getProviders()`, `MessageDigest.getInstance(...)`,
/// `KeyPairGenerator.getInstance(...)`, `Signature.getInstance(...)`,
/// or `X500Principal.<init>(...)`.  Idempotent — registering twice
/// is harmless because every entry uses the same descriptor and the
/// registry is keyed by `(class, name, descriptor)`.
pub fn register_jca_natives(registry: &mut NativeMethodRegistry) {
    provider_chain::register(registry);
    message_digest::register(registry);
    // WP6.4/WP6.6: short-circuit `KeyPairGenerator.getInstance` /
    // `Signature.getInstance` / `KeyFactory.getInstance` so the JDK
    // bytecode never reaches `sun.security.jca.GetInstance.getServices`.
    key_factory::register(registry);
    signature::register(registry);
    // `javax.crypto.KeyAgreement` ECDH → SunEC ECDH SPI.
    key_agreement::register(registry);
    // `javax.crypto.KEM` ML-KEM encaps/decaps → SunJCE ML_KEM_Impls KEM SPI
    // (gated on route_pqc_to_real / skipped in real_jca_mode, like signature).
    kem::register(registry);
    // WP6.6: DER + RFC 4514 round-trip for `X500Principal`.
    x500::register(registry);
}
