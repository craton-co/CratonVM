// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Which `javax.net.ssl.SSLContextSpi` implementations CratonVM claims as its
//! own — the boundary the `javax/net/ssl/SSLContext` natives are guarded on.
//!
//! ## Why this file exists
//!
//! CratonVM registers natives on `javax/net/ssl/SSLContext` because its own
//! `SSLContext.getInstance(String)` allocates a synthetic instance of that
//! exact class. Those registrations answered for **every** object of that
//! class, including a REAL `javax.net.ssl.SSLContext` that real JDK bytecode
//! built around a third party's `SSLContextSpi` — and they ignored the SPI
//! entirely. Measured on JDK 25 with
//! `SSLContext.getInstance("TLSv1.3", new BouncyCastleJsseProvider())`:
//!
//! ```text
//!                          HotSpot 25                        CratonVM (before)
//! getProtocol()            TLSv1.3                           "BCJSSE version 1.0023"
//! createSSLEngine()        o.b.jsse.provider.ProvSSLEngine_9  sun.security.ssl.SSLEngineImpl
//! ```
//!
//! `getProtocol()` was `ctx.get_field(this, 0)` — field 0 of the SYNTHETIC
//! layout is the protocol, field 0 of the real `javax.net.ssl.SSLContext` is
//! `provider`, so the provider was echoed back as a protocol name.
//! `createSSLEngine()` allocated CratonVM's own rustls-backed
//! `sun.security.ssl.SSLEngineImpl` unconditionally, so a BouncyCastle context
//! handed back a SunJSSE-named engine BC never built and that is not
//! initialised — `engine.toString()` alone threw
//! `NullPointerException: Cannot read field "conSession" because
//! "this.conContext" is null`.
//!
//! Same defect family as `native-io/src/socket_channel.rs`'s
//! `foreign_nio_receiver`: a native registered on a JDK class, correct for the
//! objects CratonVM allocates, applied to an object somebody else built.
//!
//! ## Why the obvious guard is wrong
//!
//! "If `this.contextSpi` is a real object, delegate to `SSLContext`'s own
//! `final` bytecode" is **not** safe. `jca::provider_chain::
//! seed_sunjsse_services` mirrors SunJSSE's service table into CratonVM's
//! provider map, so a real `SSLContext` whose `contextSpi` is a genuine
//! `sun.security.ssl.SSLContextImpl` is reachable today, and today it works
//! precisely BECAUSE `createSSLEngine` short-circuits to the rustls-backed
//! engine. Delegating that one to real SunJSSE bytecode would drive it into
//! JDK JSSE internals CratonVM does not implement.
//!
//! So the boundary is a DECISION, not a mechanical test: **`sun.security.ssl.*`
//! is the JSSE CratonVM claims to be; every other SPI is its own author's
//! business.** [`ADOPTED_SPI_PACKAGES`] is that decision, stated once, and
//! [`context_owner`] is the only thing that reads it.
//!
//! ## Three answers, not two
//!
//! [`SslContextOwner`] deliberately separates "no real SPI at all" from "a real
//! SPI that is ours", because the two want DIFFERENT things from
//! `getProtocol()`:
//!
//! * [`SslContextOwner::Synthetic`] — a context CratonVM allocated. Its slot 0
//!   is the protocol (or a protocol INDEX in the 12-field `tls.rs` shape), and
//!   the natives own it completely.
//! * [`SslContextOwner::Adopted`] — a real `javax.net.ssl.SSLContext` around a
//!   `sun.security.ssl.*` SPI. The CratonVM engine still answers, but the
//!   OBJECT is real, so `getProtocol()` must read the real `protocol` field
//!   rather than slot 0 (which is `provider`).
//! * [`SslContextOwner::Foreign`] — somebody else's SPI. Every method that has
//!   an `engine*` counterpart delegates to it.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

/// The `SSLContextSpi` implementations CratonVM claims as its own.
///
/// Slash-separated internal form, matched as a PREFIX. This is the whole of
/// the "which JSSE is CratonVM" decision; nothing else in the tree may
/// re-derive it.
///
/// `sun/security/ssl/` covers `SSLContextImpl` and every nested
/// protocol-specific subclass SunJSSE registers (`SSLContextImpl$TLS13Context`
/// and siblings), which is why it is a package prefix and not a class list.
pub(crate) const ADOPTED_SPI_PACKAGES: &[&str] = &["sun/security/ssl/"];

/// The real JDK's field name for the SPI an `SSLContext` was constructed with.
/// `javap -p --module java.base javax.net.ssl.SSLContext` on JDK 25:
/// `provider` (0), `contextSpi` (1), `protocol` (2).
const CONTEXT_SPI_FIELD: &str = "contextSpi";

/// The real JDK's field name for the protocol an `SSLContext` was constructed
/// with. Only read for a context that HAS a real SPI — see [`SslContextOwner`].
pub(crate) const PROTOCOL_FIELD: &str = "protocol";

/// The real JDK's field name for the `Provider` an `SSLContext` was
/// constructed with — slot 0, which every synthetic layout uses for the
/// protocol. See [`ssl_context_provider`].
pub(crate) const PROVIDER_FIELD: &str = "provider";

/// The provider CratonVM's own `SSLContext.getInstance(String)` stands in for,
/// and therefore the one a synthetic context must name.
const SYNTHETIC_CONTEXT_PROVIDER: &str = "SunJSSE";

/// `javax.net.ssl.SSLContextSpi`, in internal form.
const SPI_BASE_CLASS: &str = "javax/net/ssl/SSLContextSpi";

/// Who owns the behaviour of a given `javax/net/ssl/SSLContext` receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SslContextOwner {
    /// CratonVM allocated this object; there is no real `SSLContextSpi` in it.
    Synthetic,
    /// A real `SSLContext` around an SPI CratonVM has adopted
    /// ([`ADOPTED_SPI_PACKAGES`]). The natives still answer.
    Adopted(ObjectRef),
    /// A real `SSLContext` around a third party's SPI. Delegate.
    Foreign(ObjectRef),
}

/// Is `class_name` (internal form) an SPI CratonVM has adopted?
pub(crate) fn spi_is_adopted(class_name: &str) -> bool {
    ADOPTED_SPI_PACKAGES
        .iter()
        .any(|p| class_name.starts_with(p))
}

/// Does `obj`'s class descend from `javax.net.ssl.SSLContextSpi`?
///
/// Belt-and-braces on top of the field read in [`context_owner`]. The
/// synthetic layouts do not all agree on what lives in the slot the real
/// class calls `contextSpi`: `net_phase_e`'s 2-field shape stores an
/// `Int` there and `tls.rs`'s 12-field shape an `Int` too, but neither
/// contract is stated anywhere that a future edit would have to read. If a
/// synthetic layout ever puts an ObjectRef in that slot, this check is what
/// stops a `String` or a `KeyManager[]` from being taken for somebody's SPI.
fn is_ssl_context_spi(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(obj);
    if let Some(base) = ctx.class_id_by_name(SPI_BASE_CLASS) {
        if ctx.is_subclass(cid, base) {
            return true;
        }
        // `is_subclass` is asked about the exact class too by most contexts,
        // but do not depend on that.
        if cid == base {
            return true;
        }
    }
    // Fall back to the name walk when the base class is not loaded under that
    // exact name (or the context cannot answer `class_id_by_name`).
    let mut cur = Some(cid);
    while let Some(c) = cur {
        match ctx.class_name_arc_of_id(c) {
            Some(n) if &*n == SPI_BASE_CLASS => return true,
            _ => {}
        }
        cur = ctx.superclass_of(c);
    }
    false
}

/// Classify an `SSLContext` receiver against the boundary.
///
/// Reads `contextSpi` BY NAME, so it resolves against the real class layout
/// wherever that class is loaded, and answers `Synthetic` when the field does
/// not exist at all (synthetic-JDK mode, where there is no real
/// `javax.net.ssl.SSLContext` bytecode).
pub(crate) fn context_owner(ctx: &dyn NativeContext, this: ObjectRef) -> SslContextOwner {
    let spi = match ctx.get_field_by_name(this, CONTEXT_SPI_FIELD) {
        Value::Object(Some(o)) => o,
        // Not an object reference: a synthetic layout's own value in that slot
        // (an `Int` initialised flag in both live shapes), or a real context
        // whose SPI is genuinely null — which cannot happen, the field is
        // `final` and assigned in the only constructor.
        _ => return SslContextOwner::Synthetic,
    };
    if !is_ssl_context_spi(ctx, spi) {
        return SslContextOwner::Synthetic;
    }
    match ctx.class_name_arc_of_id(ctx.class_id_of_object(spi)) {
        Some(name) if spi_is_adopted(&name) => SslContextOwner::Adopted(spi),
        Some(_) => SslContextOwner::Foreign(spi),
        // Unknown class: keep the pre-existing behaviour rather than guess.
        None => SslContextOwner::Synthetic,
    }
}

/// The third-party SPI behind `args[0]`, if there is one.
pub(crate) fn foreign_spi(ctx: &dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    match context_owner(ctx, this) {
        SslContextOwner::Foreign(spi) => Some(spi),
        _ => None,
    }
}

/// Body shared by every foreign-SPI guard: when `args[0]` is a real
/// `SSLContext` around somebody else's SPI, run the SPI's own `engine*`
/// method — exactly what `SSLContext`'s `final` bytecode would have done —
/// and never CratonVM's state. `Some(result)` means the call was handled here.
///
/// `invoke_virtual`, not `invoke_virtual_bytecode_only`: the receiver is the
/// SPI and the method name is an `engine*` spelling, so there is no way back
/// into the native that called this and no re-entrancy to avoid. Nothing
/// allocates between reading the SPI out of its field and entering the call,
/// so the raw `ObjectRef` cannot go stale under a moving collector.
pub(crate) fn spi_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    engine_method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let spi = foreign_spi(ctx, args)?;
    Some(ctx.invoke_virtual(spi, engine_method, descriptor, &args[1..]))
}

/// `getProtocol()` for a receiver that is a REAL `javax.net.ssl.SSLContext`.
///
/// Not an `engine*` delegation — the JDK's `getProtocol()` returns its own
/// `protocol` field and never consults the SPI — but it must not read slot 0
/// either, which is `provider` on the real layout. Answers `None` for a
/// synthetic receiver, whose protocol the caller still owns.
///
/// Deliberately applies to [`SslContextOwner::Adopted`] as well as
/// [`SslContextOwner::Foreign`]: the bug is in the LAYOUT read, and a real
/// context around `sun.security.ssl.SSLContextImpl` has the real layout too.
pub(crate) fn real_context_protocol(ctx: &dyn NativeContext, args: &[Value]) -> Option<Value> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    match context_owner(ctx, this) {
        SslContextOwner::Synthetic => None,
        SslContextOwner::Adopted(_) | SslContextOwner::Foreign(_) => {
            match ctx.get_field_by_name(this, PROTOCOL_FIELD) {
                v @ Value::Object(Some(_)) => Some(v),
                _ => None,
            }
        }
    }
}

/// `getProvider()` for any `javax/net/ssl/SSLContext` receiver.
///
/// Registered rather than left to the real bytecode, for the same reason
/// [`real_context_protocol`] exists: `SSLContext.getProvider()` is
/// `return provider;`, slot 0 — and slot 0 of a CratonVM-allocated context is
/// the PROTOCOL. Measured on JDK 25 with `probes/SslContextSpiProbe.java`,
/// before this was registered:
///
/// ```text
///                    HotSpot 25          CratonVM
/// getProvider()      SunJSSE version 25  TLSv1.3
/// ```
///
/// A `String` where a `java.security.Provider` is contracted: every caller
/// that does anything with the answer beyond printing it fails on the type.
///
/// * a real context (ours or a third party's) answers its own `provider`
///   field, so `getInstance(algo, p).getProvider() == p` holds by identity;
/// * a synthetic one answers `SunJSSE`, which is the provider CratonVM's own
///   `getInstance(String)` stands in for.
pub(crate) fn ssl_context_provider(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if !matches!(context_owner(ctx, this), SslContextOwner::Synthetic) {
        return Ok(Some(ctx.get_field_by_name(this, PROVIDER_FIELD)));
    }
    match crate::jca::provider_chain::provider_object_named(ctx, SYNTHETIC_CONTEXT_PROVIDER)? {
        Some(p) => Ok(Some(Value::Object(Some(p)))),
        None => Ok(Some(Value::Object(None))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The boundary itself, with both halves pinned. A test that only asserted
    // the accept side would be satisfied by `spi_is_adopted(_) = true`.
    #[test]
    fn the_adopted_boundary_is_sun_security_ssl_and_nothing_else() {
        for ours in [
            "sun/security/ssl/SSLContextImpl",
            "sun/security/ssl/SSLContextImpl$TLSContext",
            "sun/security/ssl/SSLContextImpl$TLS13Context",
            "sun/security/ssl/SSLContextImpl$DTLS12Context",
        ] {
            assert!(spi_is_adopted(ours), "{ours} must be CratonVM's own SPI");
        }
        for theirs in [
            "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
            "org/conscrypt/OpenSSLContextImpl",
            "io/netty/handler/ssl/JdkSslContext",
            // Near-misses that must NOT be swept in by a sloppy prefix.
            "sun/security/sslx/Impostor",
            "com/example/sun/security/ssl/Impostor",
            "",
        ] {
            assert!(
                !spi_is_adopted(theirs),
                "{theirs} is somebody else's SPI and must be delegated to"
            );
        }
    }

    // The prefix is a package, not a class: SunJSSE's per-protocol contexts
    // are nested subclasses and every one of them is ours.
    #[test]
    fn the_adopted_list_is_a_package_prefix() {
        assert_eq!(ADOPTED_SPI_PACKAGES, &["sun/security/ssl/"]);
        assert!(spi_is_adopted("sun/security/ssl/anything/at/all"));
    }
}

/// `SSLContext` receivers of every shape the boundary has to tell apart,
/// shared by the classification tests below and by `registrar_tests`.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};
    use cratonvm_types::ClassId;

    /// The real JDK 25 layout of `javax.net.ssl.SSLContext`:
    /// `provider` (0), `contextSpi` (1), `protocol` (2). Verified with
    /// `javap -p --module java.base javax.net.ssl.SSLContext`.
    pub(crate) fn declare_real_ssl_context(ctx: &MockNativeContext, cid: ClassId) {
        let f = |name: &str, descriptor: &str, slot: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                f("provider", "Ljava/security/Provider;", 0),
                f("contextSpi", "Ljavax/net/ssl/SSLContextSpi;", 1),
                f("protocol", "Ljava/lang/String;", 2),
            ],
        );
    }

    /// An `SSLContextSpi` subclass named `spi_class`, allocated and returned.
    pub(crate) fn make_spi(ctx: &mut MockNativeContext, spi_class: &str) -> ObjectRef {
        let base = ctx
            .ensure_class_initialized("javax/net/ssl/SSLContextSpi")
            .expect("spi base");
        let cid = ctx.ensure_class_initialized(spi_class).expect("spi class");
        ctx.set_superclass(cid, base);
        ctx.alloc_object(cid, 1)
    }

    /// A REAL `javax.net.ssl.SSLContext` around `spi_class`, with `protocol`
    /// set to `protocol`.
    pub(crate) fn make_real_context(
        ctx: &mut MockNativeContext,
        spi_class: &str,
        protocol: &str,
    ) -> (ObjectRef, ObjectRef) {
        let spi = make_spi(ctx, spi_class);
        let cid = ctx
            .ensure_class_initialized("javax/net/ssl/SSLContext")
            .expect("context class");
        declare_real_ssl_context(ctx, cid);
        let obj = ctx.alloc_object(cid, 3);
        let provider = ctx.create_string("a Provider stand-in");
        let proto = ctx.create_string(protocol);
        ctx.set_field(obj, 0, Value::Object(Some(provider)));
        ctx.set_field(obj, 1, Value::Object(Some(spi)));
        ctx.set_field(obj, 2, Value::Object(Some(proto)));
        (obj, spi)
    }

    /// The shape `net_phase_e::register_re6_ssl_context::getInstance` builds:
    /// the real class, slot 0 = protocol String, slot 1 = an `Int` init flag.
    pub(crate) fn make_synthetic_context(ctx: &mut MockNativeContext, protocol: &str) -> ObjectRef {
        let cid = ctx
            .ensure_class_initialized("javax/net/ssl/SSLContext")
            .expect("context class");
        declare_real_ssl_context(ctx, cid);
        let obj = ctx.alloc_object(cid, 3);
        let proto = ctx.create_string(protocol);
        ctx.set_field(obj, 0, Value::Object(Some(proto)));
        ctx.set_field(obj, 1, Value::Int(0));
        obj
    }
}

#[cfg(test)]
mod owner_tests {
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
}

/// Every `javax/net/ssl/SSLContext` native that MUST route a third-party SPI
/// to that SPI, and the `engine*` method each one owes it.
///
/// The guard's coverage list, in one place, so
/// `registrar_tests::all_three_registration_sets_route_a_foreign_spi` can
/// assert it against every registration set rather than against whichever one
/// wins last-write-wins today. `getProtocol` is deliberately NOT here: the JDK
/// answers it from the context's own `protocol` field and never consults the
/// SPI, so it has its own row in that test.
#[cfg(test)]
pub(crate) const DELEGATED_SURFACE: &[(&str, &str, &str)] = &[
    (
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        "engineInit",
    ),
    (
        "createSSLEngine",
        "()Ljavax/net/ssl/SSLEngine;",
        "engineCreateSSLEngine",
    ),
    (
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        "engineCreateSSLEngine",
    ),
    (
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        "engineGetSocketFactory",
    ),
    (
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        "engineGetServerSocketFactory",
    ),
    (
        "getDefaultSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        "engineGetDefaultSSLParameters",
    ),
    (
        "getSupportedSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        "engineGetSupportedSSLParameters",
    ),
    (
        "getClientSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        "engineGetClientSessionContext",
    ),
    (
        "getServerSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        "engineGetServerSessionContext",
    ),
];

#[cfg(test)]
mod registrar_tests {
    use super::fixtures::*;
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeMethodRegistry};
    use std::sync::Mutex;

    /// `engine*` methods the delegating arm reached, most recent last.
    static DELEGATED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record(
        _ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        DELEGATED.lock().unwrap().push(method.to_string());
        Some(Ok(None))
    }

    fn take_delegated() -> Vec<String> {
        std::mem::take(&mut *DELEGATED.lock().unwrap())
    }

    fn registries() -> Vec<(&'static str, NativeMethodRegistry)> {
        let mut out = Vec::new();
        for (name, f) in [
            (
                "net_phase_e::register_re6_ssl_context",
                crate::net_phase_e::register_re6_ssl_context as fn(&mut NativeMethodRegistry),
            ),
            (
                "phases_late::ssl_security::register_p68_ssl",
                crate::phases_late::ssl_security::register_p68_ssl,
            ),
            (
                "tls::register_tls_natives",
                crate::tls::register_tls_natives,
            ),
        ] {
            let mut r = NativeMethodRegistry::new();
            f(&mut r);
            out.push((name, r));
        }
        out
    }

    /// Argument vector for `descriptor`, receiver first, everything else null
    /// or zero. Only the arity and the receiver matter to the guard.
    fn call_args(this: ObjectRef, descriptor: &str) -> Vec<Value> {
        let mut args = vec![Value::Object(Some(this))];
        let params = descriptor
            .split_once(')')
            .map(|(head, _)| head.trim_start_matches('('))
            .unwrap_or("");
        let mut chars = params.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                'L' => {
                    for c in chars.by_ref() {
                        if c == ';' {
                            break;
                        }
                    }
                    args.push(Value::Object(None));
                }
                // Element type follows on the next iteration and supplies the
                // one slot an array argument occupies.
                '[' => continue,
                'I' | 'Z' | 'B' | 'S' | 'C' => args.push(Value::Int(0)),
                'J' => args.push(Value::Long(0)),
                'F' => args.push(Value::Float(0.0)),
                'D' => args.push(Value::Double(0.0)),
                _ => {}
            }
        }
        args
    }

    /// ALL THREE registration sets, not just the one that wins today.
    ///
    /// The `SSLContext` natives are registered three times over (`tls.rs`,
    /// `net_phase_e.rs`, `phases_late/ssl_security.rs`) and boot order decides
    /// which answers. A guard on only the current winner is one re-ordering
    /// away from being dead, so this asserts the boundary holds in each of
    /// them independently.
    ///
    /// One `#[test]`, not nine: the recorder is a process-wide static (the
    /// mock's hook is a plain `fn` pointer and cannot capture), so separate
    /// tests would race under the default parallel harness.
    #[test]
    fn all_three_registration_sets_route_a_foreign_spi() {
        let _ = take_delegated();
        let mut checked = 0usize;
        for (set, r) in registries() {
            // --- MUST DELEGATE ---
            for (method, descriptor, engine_method) in DELEGATED_SURFACE {
                let f = match r.find("javax/net/ssl/SSLContext", method, descriptor) {
                    Some(f) => f,
                    // A set that does not register this method at all cannot
                    // answer for a foreign context, so there is nothing to
                    // guard. (`p68` and `tls.rs` each register a subset.)
                    None => continue,
                };
                let mut ctx = MockNativeContext::new();
                ctx.set_invoke_virtual_hook(record);
                let (obj, _) = make_real_context(
                    &mut ctx,
                    "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
                    "TLSv1.3",
                );
                let args = call_args(obj, descriptor);
                let _ = f(&mut ctx, &args);
                assert_eq!(
                    take_delegated(),
                    vec![engine_method.to_string()],
                    "{set}: SSLContext.{method}{descriptor} must reach \
                     SSLContextSpi.{engine_method} for a third-party SPI"
                );
                checked += 1;
            }

            // --- MUST NOT DELEGATE (the twin) ---
            //
            // Without this half the test would be satisfied by a guard that
            // delegates unconditionally — which is precisely the fix the page
            // rejected, because a real context around
            // `sun.security.ssl.SSLContextImpl` works today only BECAUSE the
            // natives answer for it.
            for (method, descriptor, _) in DELEGATED_SURFACE {
                let f = match r.find("javax/net/ssl/SSLContext", method, descriptor) {
                    Some(f) => f,
                    None => continue,
                };
                for spi in ["sun/security/ssl/SSLContextImpl", ""] {
                    let mut ctx = MockNativeContext::new();
                    ctx.set_invoke_virtual_hook(record);
                    let obj = if spi.is_empty() {
                        make_synthetic_context(&mut ctx, "TLSv1.3")
                    } else {
                        make_real_context(&mut ctx, spi, "TLSv1.3").0
                    };
                    let args = call_args(obj, descriptor);
                    let _ = f(&mut ctx, &args);
                    let kind = if spi.is_empty() { "a synthetic" } else { spi };
                    assert!(
                        take_delegated().is_empty(),
                        "{set}: SSLContext.{method}{descriptor} must NOT delegate \
                         for {kind} context"
                    );
                }
            }

            // --- getProtocol: the LAYOUT half, which is not a delegation ---
            for spi in [
                "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
                "sun/security/ssl/SSLContextImpl",
            ] {
                let f = match r.find(
                    "javax/net/ssl/SSLContext",
                    "getProtocol",
                    "()Ljava/lang/String;",
                ) {
                    Some(f) => f,
                    None => continue,
                };
                let mut ctx = MockNativeContext::new();
                let (obj, _) = make_real_context(&mut ctx, spi, "TLSv1.3");
                let got = match f(&mut ctx, &[Value::Object(Some(obj))]) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                    other => panic!("{set}: getProtocol answered {other:?}"),
                };
                assert_eq!(
                    got, "TLSv1.3",
                    "{set}: getProtocol on a real SSLContext read the wrong slot \
                     (slot 0 is `provider`, not `protocol`)"
                );
                checked += 1;
            }

            // --- getProvider: the same slot-0 confusion, the other way up ---
            //
            // `SSLContext.getProvider()` is `return provider;` — slot 0 on the
            // real layout, and the PROTOCOL on every synthetic one. Before
            // this was registered the real bytecode answered it and handed
            // back a `java.lang.String` where a `java.security.Provider` is
            // contracted.
            if let Some(f) = r.find(
                "javax/net/ssl/SSLContext",
                "getProvider",
                "()Ljava/security/Provider;",
            ) {
                // A REAL context answers its own `provider` field, BY IDENTITY
                // — `getInstance(algo, p).getProvider() == p`.
                let mut ctx = MockNativeContext::new();
                let (obj, _) =
                    make_real_context(&mut ctx, "sun/security/ssl/SSLContextImpl", "TLSv1.3");
                let want = match ctx.get_field(obj, 0) {
                    Value::Object(Some(o)) => o,
                    other => panic!("{set}: fixture slot 0 was {other:?}"),
                };
                match f(&mut ctx, &[Value::Object(Some(obj))]) {
                    Ok(Some(Value::Object(Some(got)))) => assert_eq!(
                        got, want,
                        "{set}: getProvider on a real SSLContext must answer its \
                         own `provider` field, by identity"
                    ),
                    other => panic!("{set}: getProvider answered {other:?}"),
                }

                // A SYNTHETIC context answers a Provider — specifically NOT
                // the protocol String sitting in its slot 0, which is what the
                // real bytecode returned.
                let mut ctx = MockNativeContext::new();
                let obj = make_synthetic_context(&mut ctx, "TLSv1.3");
                let slot0 = ctx.get_field(obj, 0);
                match f(&mut ctx, &[Value::Object(Some(obj))]) {
                    Ok(Some(Value::Object(Some(got)))) => {
                        assert_ne!(
                            Value::Object(Some(got)),
                            slot0,
                            "{set}: getProvider on a synthetic SSLContext answered \
                             its slot 0 — that is the protocol, not a Provider"
                        );
                        let cls = ctx
                            .class_name_of_id(ctx.class_id_of_object(got))
                            .unwrap_or_default();
                        assert_eq!(
                            cls, "java/security/Provider",
                            "{set}: getProvider must answer a Provider"
                        );
                    }
                    other => panic!("{set}: getProvider answered {other:?}"),
                }
                checked += 1;
            }
        }
        // A `find` that stopped resolving would silently turn every loop above
        // into a no-op and the test would still pass.
        assert!(
            checked >= 12,
            "only {checked} guarded registrations were exercised — the \
             registration lookups have drifted"
        );
    }
}

/// The `SSLContext`s this crate's `getInstance` minted and nobody has
/// `init`ed, by identity hash.
///
/// **Not a field.** The obvious place is slot 1, which both live
/// `getInstance` registrations already write `Int(0)` into and `init` writes
/// `Int(1)` into. MEASURED: the gate built on that read never fired —
/// `L6TlsParamSweep` rows 64 and 65 were unchanged by a trial binary that had
/// it. Slot 1 of a REAL `javax.net.ssl.SSLContext` is the `contextSpi`
/// REFERENCE (`javap -p`, JDK 25), and `try_alloc_concurrent_synthetic`
/// upsizes a synthetic allocation to the real class's layout in real-JDK
/// mode, so an `Int` written there is not an `Int` when it is read back. The
/// two existing writes are equally inert; they are left alone because
/// `jca::ssl_context_spi::context_owner` tests that slot for a foreign SPI
/// and this wave is not re-deciding that.
///
/// A stale entry can only be produced by a context that was minted, never
/// initialised, and then collected — and it can only refuse a LATER context
/// that reused its identity hash and was itself never initialised, which is a
/// call HotSpot refuses too. The failure mode of the opposite arrangement
/// ("initialised" set, refuse when absent) is a false refusal of every
/// context this crate did not mint, which is much worse.
fn uninitialized_contexts() -> &'static std::sync::Mutex<std::collections::HashSet<i32>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Called by every `SSLContext.getInstance` registration: the context exists
/// and is not usable yet.
pub(crate) fn mark_context_uninitialized(ctx: &dyn NativeContext, this: ObjectRef) {
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        if let Ok(mut t) = uninitialized_contexts().lock() {
            t.insert(ih);
        }
    }
}

/// Called by `init`, and by `getDefault`, whose context is pre-initialised.
pub(crate) fn mark_context_initialized(ctx: &dyn NativeContext, this: ObjectRef) {
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        if let Ok(mut t) = uninitialized_contexts().lock() {
            t.remove(&ih);
        }
    }
}

/// Refuse a factory or engine request from an `SSLContext` nobody `init`ed.
///
/// `sun.security.ssl.SSLContextImpl` guards `engineGetSocketFactory`,
/// `engineGetServerSocketFactory` and `engineCreateSSLEngine` with
/// `checkInitialized`. MEASURED on HotSpot 25.0.4+7 —
/// `SSLContext.getInstance("TLS").getSocketFactory()` is
/// `IllegalStateException: SSLContext is not initialized`, and CratonVM handed
/// back a working factory whose key and trust managers were never installed:
/// a caller whose `init()` threw and was swallowed got no signal at all.
pub(crate) fn require_context_initialized(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let ih = ctx.identity_hash_code(this);
    let uninitialized = ih != 0
        && uninitialized_contexts()
            .lock()
            .map(|t| t.contains(&ih))
            .unwrap_or(false);
    if uninitialized {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "SSLContext is not initialized".to_string(),
        }
        .into());
    }
    Ok(())
}
