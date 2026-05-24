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

pub mod provider_chain;
pub mod message_digest;
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
// WP6.6 — `javax.security.auth.x500.X500Principal` DER + RFC 4514 round-trip.
pub mod x500;
// ASN.1 helper used by x500 (DER encode/decode primitives). No registrations
// of its own — exists as a compile-time module for x500.rs to depend on.
pub mod asn1;

use cratonvm_native_api::NativeMethodRegistry;

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
    // WP6.6: DER + RFC 4514 round-trip for `X500Principal`.
    x500::register(registry);
}
