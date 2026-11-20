// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
pub const ADOPTED_SPI_PACKAGES: &[&str] = &["sun/security/ssl/"];

/// The real JDK's field name for the SPI an `SSLContext` was constructed with.
/// `javap -p --module java.base javax.net.ssl.SSLContext` on JDK 25:
/// `provider` (0), `contextSpi` (1), `protocol` (2).
const CONTEXT_SPI_FIELD: &str = "contextSpi";

/// The real JDK's field name for the protocol an `SSLContext` was constructed
/// with. Only read for a context that HAS a real SPI — see [`SslContextOwner`].
pub const PROTOCOL_FIELD: &str = "protocol";

/// The real JDK's field name for the `Provider` an `SSLContext` was
/// constructed with — slot 0, which every synthetic layout uses for the
/// protocol. See [`ssl_context_provider`].
pub const PROVIDER_FIELD: &str = "provider";

/// The provider CratonVM's own `SSLContext.getInstance(String)` stands in for,
/// and therefore the one a synthetic context must name.
const SYNTHETIC_CONTEXT_PROVIDER: &str = "SunJSSE";

/// `javax.net.ssl.SSLContextSpi`, in internal form.
const SPI_BASE_CLASS: &str = "javax/net/ssl/SSLContextSpi";

/// Who owns the behaviour of a given `javax/net/ssl/SSLContext` receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslContextOwner {
    /// CratonVM allocated this object; there is no real `SSLContextSpi` in it.
    Synthetic,
    /// A real `SSLContext` around an SPI CratonVM has adopted
    /// ([`ADOPTED_SPI_PACKAGES`]). The natives still answer.
    Adopted(ObjectRef),
    /// A real `SSLContext` around a third party's SPI. Delegate.
    Foreign(ObjectRef),
}

/// Is `class_name` (internal form) an SPI CratonVM has adopted?
pub fn spi_is_adopted(class_name: &str) -> bool {
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
pub fn context_owner(ctx: &dyn NativeContext, this: ObjectRef) -> SslContextOwner {
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
pub fn foreign_spi(ctx: &dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
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
pub fn spi_delegate(
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
pub fn real_context_protocol(ctx: &dyn NativeContext, args: &[Value]) -> Option<Value> {
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
pub fn ssl_context_provider(
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
mod tests;

/// `SSLContext` receivers of every shape the boundary has to tell apart,
/// shared by the classification tests below and by `registrar_tests`.
#[cfg(test)]
pub mod fixtures;

#[cfg(test)]
mod owner_tests;

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
pub const DELEGATED_SURFACE: &[(&str, &str, &str)] = &[
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
mod registrar_tests;

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
/// The failure mode of the opposite arrangement ("initialised" set, refuse
/// when absent) is a false refusal of every context this crate did not mint,
/// which is much worse.
///
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Held for one set
/// operation, after the key has been produced, and dropped before anything
/// re-enters Java.
///
/// Keyed by `(vm_identity, the context's weak lock key)`.
///
/// * gc-common w15-d handoff: the VM half. Identity hashes are a per-heap
///   counter, so two VMs in one process mint the same hashes, and a bare-hash
///   row let one VM's `getDefault()` clear another's "not initialised" mark,
///   or one VM's `getInstance` refuse the other's initialised default context.
/// * gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
///   rank 21): the object half used to be the identity hash, so two LIVE
///   contexts of one VM with one hash shared a mark: an initialised context
///   was refused with "SSLContext is not initialized", or an uninitialised one
///   was served a factory with no key or trust managers installed. A dead
///   context's mark also stayed for the VM's life. A weak lock key is unique
///   per live context, and its mark goes with it
///   ([`forget_ssl_context_keys`]).
fn uninitialized_contexts(// addr-keyed: not a heap address -- (VM identity, the context's weak lock key); dropped by forget_ssl_context_keys.
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<(usize, usize)>>
{
    static T: std::sync::OnceLock<
        // addr-keyed: the accessor's table (see above).
        cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<(usize, usize)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashSet::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Called by every `SSLContext.getInstance` registration: the context exists
/// and is not usable yet.
pub fn mark_context_uninitialized(ctx: &dyn NativeContext, this: ObjectRef) {
    // Minted before the table lock, never under it.
    let key = crate::gc_stable_weak_lock_key(ctx, this)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
    uninitialized_contexts()
        .lock()
        .insert((ctx.vm_identity(), key));
}

/// Called by `init`, and by `getDefault`, whose context is pre-initialised. A
/// context that was never keyed has no mark, so nothing is minted here.
pub fn mark_context_initialized(ctx: &dyn NativeContext, this: ObjectRef) {
    if let Some(key) = crate::existing_weak_lock_key(ctx, this) {
        uninitialized_contexts()
            .lock()
            .remove(&(ctx.vm_identity(), key));
    }
}

/// VM teardown (gc-common w15-d handoff): drop VM `vm`'s rows.
pub fn forget_vm_uninitialized_contexts(vm: usize) {
    uninitialized_contexts()
        .lock()
        .retain(|(owner, _)| *owner != vm);
}

/// Drop the marks filed under `freed` weak lock keys (dead contexts, or a VM
/// going). Called by `lib.rs::{sweep_lock_keys, forget_vm_lock_keys}`.
/// gc-common w29-d.
pub fn forget_ssl_context_keys(freed: &[usize]) {
    if freed.is_empty() {
        return;
    }
    let mut marks = uninitialized_contexts().lock();
    if marks.is_empty() {
        return;
    }
    let freed: std::collections::HashSet<usize> = freed.iter().copied().collect();
    marks.retain(|(_, key)| !freed.contains(key));
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
pub fn require_context_initialized(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // A context that was never keyed was never marked (gc-common w29-d).
    let uninitialized = crate::existing_weak_lock_key(ctx, this).is_some_and(|key| {
        uninitialized_contexts()
            .lock()
            .contains(&(ctx.vm_identity(), key))
    });
    if uninitialized {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "SSLContext is not initialized".to_string(),
        }
        .into());
    }
    Ok(())
}

/// gc-common w15-d handoff: the "not initialised" marks are per VM.
#[cfg(test)]
mod w15d_uninitialized_context_tests;

/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 21): the marks are per context within one VM too.
#[cfg(test)]
mod w29d_uninitialized_context_tests;
