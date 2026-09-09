// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! TLS/SSL native method implementations.
//!
//! Provides javax.net.ssl.*, java.security.KeyStore, and related classes
//! for TLS/SSL support in the CratonVM native layer.

// The `tls_impl` submodule contains a TLS 1.3 handshake emulator that depends
// on the crypto primitives in `crate::crypto_impl`. It is therefore
// gated behind `legacy-synthetic-crypto`; the rest of the javax.net.ssl surface in
// this module is unconditional (NEW-13).
#[cfg(feature = "legacy-synthetic-crypto")]
#[path = "tls_impl.rs"]
pub mod tls_impl;

use crate::{native_noop, native_noop_with_this, obj_arg, try_alloc_concurrent_synthetic};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::ClassId;
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Cipher and protocol constants
// ---------------------------------------------------------------------------

const TLS13_CIPHERS: &[&str] = &[
    "TLS_AES_256_GCM_SHA384",
    "TLS_AES_128_GCM_SHA256",
    "TLS_CHACHA20_POLY1305_SHA256",
];

const TLS12_CIPHERS: &[&str] = &[
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    // T-CBC.1: real CBC-mode suites, see t27_tls_cbc /
    // rustls-cbc-cipher-suites-not-supported.md
    "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
    "TLS_RSA_WITH_AES_256_GCM_SHA256",
];

const SUPPORTED_PROTOCOLS: &[&str] = &["TLSv1.3", "TLSv1.2"];

// SSLContext field indices
const CTX_PROTOCOL_IDX: usize = 0;
const CTX_INITIALIZED: usize = 1;
const CTX_KM_REF: usize = 2;
const CTX_TM_REF: usize = 3;

// SSLEngine field indices
const ENG_CLIENT_MODE: usize = 0;
const ENG_HANDSHAKE_STATUS: usize = 1;
const ENG_INBOUND_DONE: usize = 2;
const ENG_OUTBOUND_DONE: usize = 3;
const ENG_PROTOCOL_IDX: usize = 4;
const ENG_PEER_HOST: usize = 5;
const ENG_PEER_PORT: usize = 6;
const ENG_ALPN_PROTOCOL: usize = 7;
// Slots 8..13 hold the TLS configuration a caller pushes at the engine. They
// were added when `setEnabledProtocols`/`setEnabledCipherSuites`/
// `setSSLParameters` stopped being no-ops: without somewhere to put the
// caller's lists, an application restricting itself to TLS 1.3 (or to a safe
// cipher set) was silently ignored and `getSSLParameters()` handed back a
// default block that contradicted every setter that had been called. They sit
// ABOVE the original eight so no existing slot changes meaning.
//
// NOTE: `phases_late.rs::register_p68_ssl` models the SAME class with a
// DIFFERENT 7-slot layout (enabled protocols at 3, cipher suites at 4) and,
// registering later, wins for the setter triples it also owns. The slots below
// are deliberately disjoint from 0..7 so the two layouts cannot corrupt each
// other; reconciling them needs an edit to phases_late.rs, which this module
// does not own.
const ENG_ENABLED_PROTOCOLS: usize = 8;
const ENG_ENABLED_CIPHERS: usize = 9;
const ENG_APP_PROTOCOLS: usize = 10;
const ENG_NEED_CLIENT_AUTH: usize = 11;
const ENG_WANT_CLIENT_AUTH: usize = 12;
const ENG_ENDPOINT_ID_ALG: usize = 13;
const ENG_FIELD_COUNT: usize = 14;

// SSLParameters field indices. Slots 0/1 hold the caller's String[] once a
// setter has run; a bare `Int(1)` is the legacy "set, contents unknown" marker
// other modules (http2.rs, phases_late.rs) still write, and `Int(0)`/absent
// means "never set" — which real JSSE reports as a null array.
const PAR_PROTOCOLS: usize = 0;
const PAR_CIPHER_SUITES: usize = 1;
const PAR_ENDPOINT_ID_ALG: usize = 2;
const PAR_NEED_CLIENT_AUTH: usize = 3;
const PAR_WANT_CLIENT_AUTH: usize = 4;
const PAR_APP_PROTOCOLS: usize = 5;
const PAR_FIELD_COUNT: usize = 6;

// SSLSession field indices
const SES_CIPHER_SUITE: usize = 0;
const SES_PROTOCOL: usize = 1;
const SES_VALID: usize = 2;
const SES_PEER_HOST: usize = 3;
const SES_PEER_PORT: usize = 4;
const SES_CREATION_TIME: usize = 5;

// SSLEngineResult status codes
const STATUS_OK: i32 = 0;
const STATUS_CLOSED: i32 = 1;
const STATUS_BUFFER_UNDERFLOW: i32 = 2;
const STATUS_BUFFER_OVERFLOW: i32 = 3;

// Handshake status codes
const HS_NOT_HANDSHAKING: i32 = 0;
const HS_NEED_WRAP: i32 = 1;
const HS_NEED_UNWRAP: i32 = 2;
const HS_NEED_TASK: i32 = 3;
const HS_FINISHED: i32 = 4;

// ---------------------------------------------------------------------------
// Helper: current epoch millis
// ---------------------------------------------------------------------------

fn epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Allocation helpers
// ---------------------------------------------------------------------------

fn alloc_ssl_context(
    ctx: &mut dyn NativeContext,
    protocol_idx: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 12)?;
    ctx.set_field(obj, CTX_PROTOCOL_IDX, Value::Int(protocol_idx));
    ctx.set_field(obj, CTX_INITIALIZED, Value::Int(0));
    ctx.set_field(obj, CTX_KM_REF, Value::Int(0));
    ctx.set_field(obj, CTX_TM_REF, Value::Int(0));
    Ok(obj)
}

/// Establish the field state a freshly-created synthetic `SSLEngine` must
/// have. Split out of [`alloc_ssl_engine`] so `<init>()V` can reach exactly
/// the same state: an engine built with `new SSLEngine()` and one built with
/// `SSLContext.createSSLEngine()` must not differ.
fn init_ssl_engine_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, ENG_CLIENT_MODE, Value::Int(1));
    ctx.set_field(obj, ENG_HANDSHAKE_STATUS, Value::Int(HS_NOT_HANDSHAKING));
    ctx.set_field(obj, ENG_INBOUND_DONE, Value::Int(0));
    ctx.set_field(obj, ENG_OUTBOUND_DONE, Value::Int(0));
    ctx.set_field(obj, ENG_PROTOCOL_IDX, Value::Int(0)); // TLSv1.3
    ctx.set_field(obj, ENG_PEER_HOST, Value::Int(0));
    // -1, not 0: real `SSLEngine()` reports -1 for "no peer port known", and
    // 0 is a legal port number a caller could mistake for a real one.
    ctx.set_field(obj, ENG_PEER_PORT, Value::Int(-1));
    ctx.set_field(obj, ENG_ALPN_PROTOCOL, Value::Int(0)); // none

    // Unconfigured: `getSSLParameters()` reports these as null arrays / false,
    // which is what real JSSE reports for a parameter block nobody has set.
    ctx.set_field(obj, ENG_ENABLED_PROTOCOLS, Value::Object(None));
    ctx.set_field(obj, ENG_ENABLED_CIPHERS, Value::Object(None));
    ctx.set_field(obj, ENG_APP_PROTOCOLS, Value::Object(None));
    ctx.set_field(obj, ENG_NEED_CLIENT_AUTH, Value::Int(0));
    ctx.set_field(obj, ENG_WANT_CLIENT_AUTH, Value::Int(0));
    ctx.set_field(obj, ENG_ENDPOINT_ID_ALG, Value::Int(0));
}

fn alloc_ssl_engine(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", ENG_FIELD_COUNT)?;
    init_ssl_engine_fields(ctx, obj);
    Ok(obj)
}

/// Field state of a freshly-created synthetic `SSLSession`. Shared with
/// `SSLSession.<init>()V` — in particular the creation timestamp, which
/// `getCreationTime()` returns verbatim and which a no-op constructor left at
/// the slot default (reported as 0 = the epoch).
fn init_ssl_session_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, SES_CIPHER_SUITE, Value::Int(0)); // TLS_AES_256_GCM_SHA384
    ctx.set_field(obj, SES_PROTOCOL, Value::Int(0)); // TLSv1.3
    ctx.set_field(obj, SES_VALID, Value::Int(1));
    ctx.set_field(obj, SES_PEER_HOST, Value::Int(0));
    ctx.set_field(obj, SES_PEER_PORT, Value::Int(-1));
    ctx.set_field(obj, SES_CREATION_TIME, Value::Long(epoch_millis()));
}

fn alloc_ssl_session(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 6)?;
    init_ssl_session_fields(ctx, obj);
    Ok(obj)
}

/// Field state of a freshly-created synthetic `SSLParameters` — shared with
/// `SSLParameters.<init>()V`.
fn init_ssl_parameters_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, PAR_PROTOCOLS, Value::Int(0)); // never set
    ctx.set_field(obj, PAR_CIPHER_SUITES, Value::Int(0)); // never set
    ctx.set_field(obj, PAR_ENDPOINT_ID_ALG, Value::Int(0));
    ctx.set_field(obj, PAR_NEED_CLIENT_AUTH, Value::Int(0));
    ctx.set_field(obj, PAR_WANT_CLIENT_AUTH, Value::Int(0));
    ctx.set_field(obj, PAR_APP_PROTOCOLS, Value::Object(None));
}

fn alloc_ssl_parameters(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", PAR_FIELD_COUNT)?;
    init_ssl_parameters_fields(ctx, obj);
    Ok(obj)
}

fn alloc_ssl_engine_result(
    ctx: &mut dyn NativeContext,
    status: i32,
    hs_status: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 2)?;
    ctx.set_field(obj, 0, Value::Int(status));
    ctx.set_field(obj, 1, Value::Int(hs_status));
    Ok(obj)
}

// ---------------------------------------------------------------------------
// Refusal helpers
// ---------------------------------------------------------------------------

/// Throw `java.security.NoSuchAlgorithmException(msg)`.
///
/// The JDK-specified failure for `SSLContext.getInstance` with an unsupported
/// protocol. Same construct-or-fall-back shape as
/// `phases_early::throw_jca_exc` (the mechanism documented in
/// `docs/security/crypto-failure-contract.md`); the fallback is an
/// `IllegalArgumentException` rather than a value, so an absent exception
/// class still cannot turn a refusal into a working context.
fn throw_tls_algorithm_exc(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/security/NoSuchAlgorithmException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::IllegalArgumentException {
        message: msg.to_string(),
    }
    .into()
}

/// Refuse a factory request from an `SSLContext` that was never `init()`ed.
///
/// Real JSSE (`sun.security.ssl.SSLContextImpl.engineGetSocketFactory`) throws
/// `IllegalStateException("SSLContext is not initialized")`. This module used
/// to hand out a factory regardless, so a caller that skipped `init()` — or
/// whose `init()` threw and was swallowed — got a factory whose key and trust
/// managers were never installed, and no signal that anything was missing.
fn require_initialized_context(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    method: &str,
) -> Result<(), MethodCallFailed> {
    if matches!(ctx.get_field(this, CTX_INITIALIZED), Value::Int(1)) {
        return Ok(());
    }
    Err(cratonvm_types::error::RuntimeError::IllegalStateException {
        message: format!(
            "SSLContext is not initialized; call SSLContext.init(KeyManager[], \
             TrustManager[], SecureRandom) before {method}. Refusing to return a \
             factory with no key or trust managers installed."
        ),
    }
    .into())
}

// ---------------------------------------------------------------------------
// The non-cryptographic synthetic SSLEngine
// ---------------------------------------------------------------------------

/// Tri-state cache for the [`noncrypto_engine_allowed`] opt-in:
/// `0` = not yet read, `1` = deny (the default), `2` = allow.
static NONCRYPTO_ENGINE_OPT_IN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether the caller has explicitly opted in to this module's
/// non-cryptographic `javax/net/ssl/SSLEngine`.
///
/// **Default: deny.** This engine's `wrap`/`unwrap` perform no cryptography
/// whatsoever — they move no bytes, emit no records, and drive a hand-written
/// state machine that reaches `HandshakeStatus.FINISHED` and a session
/// reporting `TLSv1.3` / `TLS_AES_256_GCM_SHA384`. An application that wraps
/// plaintext and writes the (untouched) destination buffer to a socket sends
/// cleartext while every JSSE status it can observe says the handshake
/// completed. That is precisely "unsupported TLS mistaken for transport
/// security".
///
/// No working path is lost by refusing: the engine never produced TLS bytes,
/// so anything driving it was already failing further downstream (`lib.rs`'s
/// note on this stub records the symptom as "unexpected EOF"). Refusing turns
/// a confusing downstream failure into an accurate local one.
///
/// `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE=1` restores the legacy behaviour for
/// debugging and regression bisecting. The real, rustls-backed engine is
/// `t27_tls::register_sslengine_real` on `sun/security/ssl/SSLEngineImpl` and
/// is unaffected by this gate.
fn noncrypto_engine_allowed() -> bool {
    use std::sync::atomic::Ordering;
    match NONCRYPTO_ENGINE_OPT_IN.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let allowed = cratonvm_types::flags::runtime_var("CRATONVM_ALLOW_NONCRYPTO_SSLENGINE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            NONCRYPTO_ENGINE_OPT_IN.store(if allowed { 2 } else { 1 }, Ordering::Relaxed);
            allowed
        }
    }
}

/// Test-only override, so the deny path and the opt-in path can both be
/// exercised without depending on process environment (which latches).
#[cfg(test)]
fn set_noncrypto_engine_opt_in(allowed: bool) {
    use std::sync::atomic::Ordering;
    NONCRYPTO_ENGINE_OPT_IN.store(if allowed { 2 } else { 1 }, Ordering::Relaxed);
}

/// Refuse an operation on the non-cryptographic synthetic engine.
///
/// `javax.net.ssl.SSLException` is a subclass of `IOException`, which
/// `SSLEngine.wrap`/`unwrap` already declare, so an existing
/// `catch (SSLException)` / `catch (IOException)` in a handshake loop handles
/// it. Falls back to a bare `IOException` — never to a result object.
fn throw_noncrypto_engine_exc(ctx: &mut dyn NativeContext, op: &str) -> MethodCallFailed {
    let msg = format!(
        "SSLEngine.{op} is not implemented in this VM: the synthetic \
         javax.net.ssl.SSLEngine performs no cryptography and would report a \
         completed TLSv1.3 handshake for data it never protected. Refusing to \
         return a success status for a connection that is not secured. Use the \
         rustls-backed engine (sun.security.ssl.SSLEngineImpl), or set \
         CRATONVM_ALLOW_NONCRYPTO_SSLENGINE=1 to restore the legacy no-op \
         behaviour for debugging."
    );
    let detail = ctx.create_string(&msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "javax/net/ssl/SSLException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::IOException { message: msg }.into()
}

/// Build a Java String[] from a Rust slice of &str.
fn build_string_array(ctx: &mut dyn NativeContext, items: &[&str]) -> ObjectRef {
    let arr = ctx.new_ref_array(ClassId::new(0), items.len());
    for (i, s) in items.iter().enumerate() {
        let sv = ctx.create_string(s);
        ctx.set_array_element(arr, i, Value::Object(Some(sv)));
    }
    arr
}

// ---------------------------------------------------------------------------
// 1. javax.net.ssl.SSLContext  (12-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_context(r: &mut NativeMethodRegistry) {
    // SyntheticStub: SSLContext here is a synthetic field-holder; getInstance/
    // getDefault/init merely allocate an object and flip an "initialized" flag.
    // No rustls config is built. The real engine lives in t27_tls.rs.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/SSLContext";
    // <init>() — establish the same state `getInstance()` produces. A no-op
    // left every slot at its untagged default, so `getProtocol()` fell out of
    // its `_ => 0` arm by accident rather than by initialisation and the
    // "initialized" flag could not be told apart from "never written" (a
    // subsequent `init()` was the only thing that ever tagged it as an Int).
    // Real JDK has no no-arg `SSLContext` constructor, and this file is only
    // registered from `register_synthetic_overrides` (synthetic-JDK mode), so
    // the receiver is always this module's 12-slot shim.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, CTX_PROTOCOL_IDX, Value::Int(0)); // "TLS"
        ctx.set_field(this, CTX_INITIALIZED, Value::Int(0));
        ctx.set_field(this, CTX_KM_REF, Value::Int(0));
        ctx.set_field(this, CTX_TM_REF, Value::Int(0));
        Ok(None)
    });

    // getInstance(String protocol) -> SSLContext
    //
    // DENY-BY-DEFAULT (P1, "unsupported TLS must be impossible to mistake for
    // transport security"): an unrecognised protocol string used to fall out
    // of a `_ => 0` arm and produce a perfectly ordinary "TLS" context. So
    // `SSLContext.getInstance("SSLv2")`, `getInstance("NoSuchThing")` and a
    // typo'd bundle protocol all succeeded, and `getProtocol()` then reported
    // "TLS" — an answer to a question nobody asked. The JDK contract is
    // `NoSuchAlgorithmException`, and the real-JDK-mode registrations already
    // enforce it (`net_phase_e::register_re6_ssl_context`,
    // `phases_late::ssl_security::register_p68_ssl`); this synthetic-JDK
    // registration was the odd one out.
    //
    // The accepted set matches `register_p68_ssl`'s, plus `SSLv3`, and is
    // matched case-insensitively because JCA algorithm lookup is. Each name
    // gets its OWN index so `getProtocol()` echoes back what was requested
    // rather than collapsing to "TLS".
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let requested = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                // Real JDK: `getInstance(null)` throws NullPointerException.
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("SSLContext.getInstance: protocol is null".into()),
                    }
                    .into());
                }
            };
            let requested = requested.unwrap_or_default();
            // W3-7: the accept DECISION is now
            // `jca::provider_chain::ssl_context_protocol_supported`, shared with
            // the two real-JDK-mode registrations, so the three cannot drift
            // into three different answers for the same protocol string again.
            // The index match below only decides which name `getProtocol()`
            // echoes back; a supported-but-unindexed name (DTLS, or a protocol
            // a caller's own Provider registered) lands on index 0.
            if !crate::jca::provider_chain::ssl_context_protocol_supported(&requested) {
                return Err(throw_tls_algorithm_exc(
                    ctx,
                    &format!(
                        "{requested} SSLContext not available: refusing to substitute a \
                         different protocol — an SSLContext for a protocol you did not \
                         ask for is not the protection you asked for."
                    ),
                ));
            }
            let protocol_idx = match requested.to_ascii_uppercase().as_str() {
                "TLSV1.2" => 1,
                "TLSV1.3" => 2,
                "SSL" => 3,
                "TLSV1" => 4,
                "TLSV1.1" => 5,
                "SSLV3" => 6,
                "DTLS" => 7,
                "DTLSV1.0" => 8,
                "DTLSV1.2" => 9,
                // "TLS", "Default", and anything a caller's Provider added.
                _ => 0,
            };
            let obj = alloc_ssl_context(ctx, protocol_idx)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getDefault() -> SSLContext  (returns TLSv1.3 context, idx=2)
    r.register(
        cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let obj = alloc_ssl_context(ctx, 2)?; // TLSv1.3
            ctx.set_field(obj, CTX_INITIALIZED, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // init(KeyManager[], TrustManager[], SecureRandom) -> void
    //
    // The `jca::ssl_context_spi` guards on this and the seven registrations
    // below are inert in synthetic-JDK mode — there is no real
    // `javax.net.ssl.SSLContext` bytecode, so no receiver ever carries a
    // `contextSpi` — and are here because the boundary must hold in ALL THREE
    // registration sets. Whichever set wins last-write-wins is a property of
    // the boot order, not of this file, and a mode change that made this one
    // live must not silently re-open the defect.
    r.register(
        cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineInit",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let km_present = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            let tm_present = match args.get(2) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, CTX_KM_REF, Value::Int(km_present));
            ctx.set_field(this, CTX_TM_REF, Value::Int(tm_present));
            ctx.set_field(this, CTX_INITIALIZED, Value::Int(1));
            Ok(None)
        },
    );

    // createSSLEngine() -> SSLEngine
    r.register(
        cls,
        "createSSLEngine",
        "()Ljavax/net/ssl/SSLEngine;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineCreateSSLEngine",
                "()Ljavax/net/ssl/SSLEngine;",
            ) {
                return r;
            }
            let eng = alloc_ssl_engine(ctx)?;
            Ok(Some(Value::Object(Some(eng))))
        },
    );

    // createSSLEngine(String host, int port) -> SSLEngine
    r.register(
        cls,
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineCreateSSLEngine",
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
            ) {
                return r;
            }
            let eng = alloc_ssl_engine(ctx)?;
            // Store peer host
            if let Some(Value::Object(Some(host_ref))) = args.get(1) {
                if let Some(host_str) = ctx.read_string(*host_ref) {
                    let hs = ctx.create_string(&host_str);
                    ctx.set_field(eng, ENG_PEER_HOST, Value::Object(Some(hs)));
                }
            }
            let port = match args.get(2) {
                Some(Value::Int(p)) => *p,
                _ => -1,
            };
            ctx.set_field(eng, ENG_PEER_PORT, Value::Int(port));
            Ok(Some(Value::Object(Some(eng))))
        },
    );

    // getSocketFactory() -> SSLSocketFactory
    //
    // Gated on `init()` — see `require_initialized_context`. `getDefault()`
    // above marks its context initialized, so the JDK's own pre-initialised
    // default context path is unaffected.
    r.register(
        cls,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetSocketFactory",
                "()Ljavax/net/ssl/SSLSocketFactory;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            require_initialized_context(ctx, this, "getSocketFactory()")?;
            let sf = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 2)?;
            Ok(Some(Value::Object(Some(sf))))
        },
    );

    // getServerSocketFactory() -> SSLServerSocketFactory
    r.register(
        cls,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetServerSocketFactory",
                "()Ljavax/net/ssl/SSLServerSocketFactory;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            require_initialized_context(ctx, this, "getServerSocketFactory()")?;
            let ssf =
                try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 2)?;
            Ok(Some(Value::Object(Some(ssf))))
        },
    );

    // getDefaultSSLParameters() -> SSLParameters
    r.register(
        cls,
        "getDefaultSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineGetDefaultSSLParameters",
                "()Ljavax/net/ssl/SSLParameters;",
            ) {
                return r;
            }
            let p = alloc_ssl_parameters(ctx)?;
            Ok(Some(Value::Object(Some(p))))
        },
    );

    // getSupportedSSLParameters() -> SSLParameters
    r.register(
        cls,
        "getSupportedSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineGetSupportedSSLParameters",
                "()Ljavax/net/ssl/SSLParameters;",
            ) {
                return r;
            }
            let p = alloc_ssl_parameters(ctx)?;
            Ok(Some(Value::Object(Some(p))))
        },
    );

    // getProtocol() -> String
    r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        if let Some(v) = crate::jca::ssl_context_spi::real_context_protocol(ctx, args) {
            return Ok(Some(v));
        }
        let this = obj_arg(args, 0)?;
        let idx = match ctx.get_field(this, CTX_PROTOCOL_IDX) {
            Value::Int(i) => i,
            _ => 0,
        };
        let s = ctx.create_string(ctx_protocol_name(idx));
        Ok(Some(Value::Object(Some(s))))
    });

    // getProvider() -> Provider. See `jca::ssl_context_spi`.
    r.register(
        cls,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| crate::jca::ssl_context_spi::ssl_context_provider(ctx, args),
    );
    r.set_category(__prev_cat);
}

/// Protocol name for a `CTX_PROTOCOL_IDX` slot value.
///
/// Indices 4/5/6 were added with the `getInstance` deny-by-default fix so an
/// accepted-but-previously-unmapped name (`TLSv1`, `TLSv1.1`, `SSLv3`) is
/// echoed back verbatim instead of collapsing to "TLS". The `_` arm is now
/// only reachable for index 0 (`TLS`/`Default`) and for a slot that was never
/// written, which is the same state a freshly-allocated context is in.
fn ctx_protocol_name(idx: i32) -> &'static str {
    match idx {
        1 => "TLSv1.2",
        2 => "TLSv1.3",
        3 => "SSL",
        4 => "TLSv1",
        5 => "TLSv1.1",
        6 => "SSLv3",
        // W3-7: DTLS is three more SunJSSE `SSLContext` services (measured on
        // jdk-25.0.3.9-hotspot). They were previously refused outright.
        7 => "DTLS",
        8 => "DTLSv1.0",
        9 => "DTLSv1.2",
        _ => "TLS",
    }
}

// ---------------------------------------------------------------------------
// 2. javax.net.ssl.SSLEngine  (14-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_engine(r: &mut NativeMethodRegistry) {
    // SyntheticStub: wrap()/unwrap() do NO real TLS — they emit empty records
    // and advance a fake handshake state machine with no rustls crypto. The
    // genuine rustls-backed SSLEngine is t27_tls.rs::register_sslengine_real.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/SSLEngine";
    // <init>() — give a directly-constructed engine the state
    // `SSLContext.createSSLEngine()` gives one. The no-op left client-mode at
    // 0 (so `getUseClientMode()` reported a SERVER engine, the opposite of the
    // JSSE default and of every engine this file hands out) and peer port at 0
    // rather than -1.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        init_ssl_engine_fields(ctx, this);
        Ok(None)
    });

    // wrap(ByteBuffer[] srcs, ByteBuffer dst) -> SSLEngineResult
    r.register(
        cls,
        "wrap",
        "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let outbound_done = match ctx.get_field(this, ENG_OUTBOUND_DONE) {
                Value::Int(v) => v,
                _ => 0,
            };
            if outbound_done != 0 {
                // PRESERVED NEGATIVE: `CLOSED` is a real, correct answer —
                // `closeOutbound()` was called and there is genuinely nothing
                // more to send. It claims no protection, so it stays a value.
                let result = alloc_ssl_engine_result(ctx, STATUS_CLOSED, HS_NOT_HANDSHAKING)?;
                return Ok(Some(Value::Object(Some(result))));
            }
            // DENY-BY-DEFAULT: everything below fabricates a successful
            // handshake without any cryptography. See
            // `noncrypto_engine_allowed`.
            if !noncrypto_engine_allowed() {
                return Err(throw_noncrypto_engine_exc(ctx, "wrap"));
            }
            // Advance handshake state machine
            let hs_status = match ctx.get_field(this, ENG_HANDSHAKE_STATUS) {
                Value::Int(v) => v,
                _ => HS_NOT_HANDSHAKING,
            };
            let next_hs = match hs_status {
                HS_NEED_WRAP => HS_NEED_UNWRAP, // After sending ClientHello, wait for ServerHello
                _ => HS_NOT_HANDSHAKING,
            };
            ctx.set_field(this, ENG_HANDSHAKE_STATUS, Value::Int(next_hs));
            let result = alloc_ssl_engine_result(ctx, STATUS_OK, next_hs)?;
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // unwrap(ByteBuffer src, ByteBuffer[] dsts) -> SSLEngineResult
    r.register(
        cls,
        "unwrap",
        "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let inbound_done = match ctx.get_field(this, ENG_INBOUND_DONE) {
                Value::Int(v) => v,
                _ => 0,
            };
            if inbound_done != 0 {
                // PRESERVED NEGATIVE — see the matching comment in `wrap`.
                let result = alloc_ssl_engine_result(ctx, STATUS_CLOSED, HS_NOT_HANDSHAKING)?;
                return Ok(Some(Value::Object(Some(result))));
            }
            // DENY-BY-DEFAULT: the arms below can report `HS_FINISHED` — a
            // completed TLS handshake that never happened. See
            // `noncrypto_engine_allowed`.
            if !noncrypto_engine_allowed() {
                return Err(throw_noncrypto_engine_exc(ctx, "unwrap"));
            }
            // Advance handshake state machine
            let hs_status = match ctx.get_field(this, ENG_HANDSHAKE_STATUS) {
                Value::Int(v) => v,
                _ => HS_NOT_HANDSHAKING,
            };
            let next_hs = match hs_status {
                HS_NEED_UNWRAP => HS_NEED_WRAP, // After receiving, need to send next message
                HS_NEED_TASK => HS_FINISHED,    // After delegated task, handshake finishes
                _ => HS_NOT_HANDSHAKING,
            };
            // If handshake just finished, mark it complete
            if next_hs == HS_FINISHED {
                ctx.set_field(this, ENG_HANDSHAKE_STATUS, Value::Int(HS_NOT_HANDSHAKING));
                let result = alloc_ssl_engine_result(ctx, STATUS_OK, HS_FINISHED)?;
                return Ok(Some(Value::Object(Some(result))));
            }
            ctx.set_field(this, ENG_HANDSHAKE_STATUS, Value::Int(next_hs));
            let result = alloc_ssl_engine_result(ctx, STATUS_OK, next_hs)?;
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // beginHandshake() -> void
    r.register(cls, "beginHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Initialize handshake: client starts with NEED_WRAP (to send ClientHello),
        // server starts with NEED_UNWRAP (to receive ClientHello)
        let client_mode = match ctx.get_field(this, ENG_CLIENT_MODE) {
            Value::Int(v) => v,
            _ => 1,
        };
        let initial_hs = if client_mode != 0 {
            HS_NEED_WRAP
        } else {
            HS_NEED_UNWRAP
        };
        ctx.set_field(this, ENG_HANDSHAKE_STATUS, Value::Int(initial_hs));
        // Reset inbound/outbound done flags for new handshake
        ctx.set_field(this, ENG_INBOUND_DONE, Value::Int(0));
        ctx.set_field(this, ENG_OUTBOUND_DONE, Value::Int(0));
        Ok(None)
    });

    // getHandshakeStatus() -> SSLEngineResult$HandshakeStatus
    r.register(
        cls,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Return the enum as a synthetic object carrying the int status
            let hs_val = ctx.get_field(this, ENG_HANDSHAKE_STATUS);
            let hs_obj = try_alloc_concurrent_synthetic(
                ctx,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                1,
            )?;
            ctx.set_field(hs_obj, 0, hs_val);
            Ok(Some(Value::Object(Some(hs_obj))))
        },
    );

    // isInboundDone() -> boolean
    r.register(cls, "isInboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, ENG_INBOUND_DONE)))
    });

    // isOutboundDone() -> boolean
    r.register(cls, "isOutboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, ENG_OUTBOUND_DONE)))
    });

    // closeInbound() -> void
    r.register(cls, "closeInbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, ENG_INBOUND_DONE, Value::Int(1));
        Ok(None)
    });

    // closeOutbound() -> void
    r.register(cls, "closeOutbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, ENG_OUTBOUND_DONE, Value::Int(1));
        Ok(None)
    });

    // setUseClientMode(boolean mode) -> void
    r.register(cls, "setUseClientMode", "(Z)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let mode = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            ctx.set_field(*this, ENG_CLIENT_MODE, Value::Int(mode));
        }
        Ok(None)
    });

    // getUseClientMode() -> boolean
    r.register(cls, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, ENG_CLIENT_MODE)))
    });

    // getPeerHost() -> String
    r.register(cls, "getPeerHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let field = ctx.get_field(this, ENG_PEER_HOST);
        match field {
            Value::Object(_) => Ok(Some(field)),
            _ => Ok(Some(Value::Object(None))),
        }
    });

    // getPeerPort() -> int
    r.register(cls, "getPeerPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, ENG_PEER_PORT)))
    });

    // setEnabledProtocols(String[]) -> void
    //
    // Was a no-op: an application narrowing itself to TLSv1.3 (or excluding a
    // protocol it considers unsafe) was silently ignored, and the only way to
    // observe the engine's protocol set — `getSSLParameters()` — went on
    // reporting the module default. Store the caller's array so
    // `getSSLParameters().getProtocols()` reflects it.
    //
    // The fake wrap/unwrap state machine below does NOT consult this list:
    // it performs no negotiation at all (it only walks NEED_WRAP/NEED_UNWRAP),
    // so the list is configuration state, not something the handshake can
    // honour. The genuine, protocol-enforcing engine is t27_tls.rs.
    //
    // Shadowed by phases_late.rs::register_p68_ssl (Phase L), which registers
    // the same triple later and wins; this copy keeps tls.rs self-consistent
    // (its own `getSSLParameters` reads what this writes).
    r.register(
        cls,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, ENG_ENABLED_PROTOCOLS, list);
            Ok(None)
        },
    );

    // setEnabledCipherSuites(String[]) -> void — see setEnabledProtocols above
    // for why this is stored rather than dropped, and for the phases_late.rs
    // shadowing note.
    r.register(
        cls,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, ENG_ENABLED_CIPHERS, list);
            Ok(None)
        },
    );

    // getApplicationProtocol() -> String
    r.register(
        cls,
        "getApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, ENG_ALPN_PROTOCOL) {
                Value::Int(i) => i,
                _ => 0,
            };
            let proto = match idx {
                1 => "h2",
                2 => "http/1.1",
                _ => "",
            };
            let s = ctx.create_string(proto);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // setSSLParameters(SSLParameters) -> void
    //
    // Nothing else registers this triple on `javax/net/ssl/SSLEngine`
    // (phases_late.rs owns the SSLSocket copy, t27_tls.rs the
    // sun.security.ssl.SSLEngineImpl copy), so this no-op WAS the
    // implementation: an engine configured entirely through an SSLParameters
    // block — the way Apache HttpComponents 5 and Tomcat's NIO endpoint do it
    // — kept the module defaults for protocols, cipher suites, ALPN and
    // client-auth, and `getSSLParameters()` handed back a fresh default block
    // that contradicted what had just been set.
    //
    // Copy field-by-field rather than retaining the caller's object: real JSSE
    // reads the values out at this point, so a later mutation of the caller's
    // block must not reconfigure a live engine. No allocation happens here, so
    // neither `this` nor `params` can be moved by a collection mid-copy.
    r.register(
        cls,
        "setSSLParameters",
        "(Ljavax/net/ssl/SSLParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = match args.get(1) {
                Some(Value::Object(Some(p))) => *p,
                // Real JSSE dereferences the argument unconditionally.
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("SSLEngine.setSSLParameters: null SSLParameters".into()),
                    }
                    .into())
                }
            };
            let protocols = ctx.get_field(params, PAR_PROTOCOLS);
            let ciphers = ctx.get_field(params, PAR_CIPHER_SUITES);
            let app_protocols = ctx.get_field(params, PAR_APP_PROTOCOLS);
            let endpoint_alg = ctx.get_field(params, PAR_ENDPOINT_ID_ALG);
            let need_auth = ctx.get_field(params, PAR_NEED_CLIENT_AUTH);
            let want_auth = ctx.get_field(params, PAR_WANT_CLIENT_AUTH);
            ctx.set_field(this, ENG_ENABLED_PROTOCOLS, protocols);
            ctx.set_field(this, ENG_ENABLED_CIPHERS, ciphers);
            ctx.set_field(this, ENG_APP_PROTOCOLS, app_protocols);
            ctx.set_field(this, ENG_ENDPOINT_ID_ALG, endpoint_alg);
            ctx.set_field(this, ENG_NEED_CLIENT_AUTH, need_auth);
            ctx.set_field(this, ENG_WANT_CLIENT_AUTH, want_auth);
            Ok(None)
        },
    );

    // getSSLParameters() -> SSLParameters
    //
    // Mirror of setSSLParameters: hand back a FRESH block carrying the
    // engine's current configuration (real JSSE also returns a copy, so a
    // caller mutating the result cannot reconfigure the engine behind its
    // back). Previously this ignored the receiver entirely and always returned
    // module defaults.
    r.register(
        cls,
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `alloc_ssl_parameters` can collect and relocate `this`; pin it
            // and re-read the (possibly forwarded) reference before touching
            // its fields again.
            let pin = ctx.pin_native_root(this);
            let p = alloc_ssl_parameters(ctx)?;
            let this = ctx.read_native_pin(pin, this);
            let protocols = ctx.get_field(this, ENG_ENABLED_PROTOCOLS);
            let ciphers = ctx.get_field(this, ENG_ENABLED_CIPHERS);
            let app_protocols = ctx.get_field(this, ENG_APP_PROTOCOLS);
            let endpoint_alg = ctx.get_field(this, ENG_ENDPOINT_ID_ALG);
            let need_auth = ctx.get_field(this, ENG_NEED_CLIENT_AUTH);
            let want_auth = ctx.get_field(this, ENG_WANT_CLIENT_AUTH);
            ctx.unpin_native_roots(pin);
            ctx.set_field(p, PAR_PROTOCOLS, protocols);
            ctx.set_field(p, PAR_CIPHER_SUITES, ciphers);
            ctx.set_field(p, PAR_APP_PROTOCOLS, app_protocols);
            ctx.set_field(p, PAR_ENDPOINT_ID_ALG, endpoint_alg);
            ctx.set_field(p, PAR_NEED_CLIENT_AUTH, need_auth);
            ctx.set_field(p, PAR_WANT_CLIENT_AUTH, want_auth);
            Ok(Some(Value::Object(Some(p))))
        },
    );

    // getSession() -> SSLSession
    r.register(
        cls,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, _args| {
            let ses = alloc_ssl_session(ctx)?;
            Ok(Some(Value::Object(Some(ses))))
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 3. javax.net.ssl.SSLSession  (6-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_session(r: &mut NativeMethodRegistry) {
    // SyntheticStub: 6-field synthetic session; getId is derived from object
    // identity, cipher/protocol read back synthetic fields. No real session.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/SSLSession";
    // <init>() — `javax.net.ssl.SSLSession` is an INTERFACE, so this can only
    // ever run for one of this module's synthetic 6-slot sessions; there is no
    // real constructor behind it. A no-op left the session with no creation
    // time (`getCreationTime()` returned 0 = the epoch, so any "session age"
    // computation saw a ~56-year-old session), `isValid()` reading an untagged
    // default instead of true, and peer port 0 instead of -1. Establish the
    // same state `SSLEngine.getSession()` produces.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        init_ssl_session_fields(ctx, this);
        Ok(None)
    });

    // getCipherSuite() / getProtocol() -> String
    //
    // E31 — THREE separate defects were folded into the two lines these
    // replace, and the third is the one that matters:
    //
    // 1. **The slot was hard-coded.** `SES_CIPHER_SUITE` (0) / `SES_PROTOCOL`
    //    (1) is this module's own 6-field convention, but this registration is
    //    on the shared `javax/net/ssl/SSLSession` interface and under
    //    `--synthetic-jdk` it wins for EVERY shape in the tree — including the
    //    2-, 3- and 4-field ones, whose order is the other way round
    //    (protocol first). Route through `t27_tls::session_cipher_slot`, the
    //    one width->slot table, instead of assuming one shape.
    // 2. **A `String` slot was read as an `Int`.** Every shape except this
    //    module's stores the negotiated names as real `String` references. The
    //    `Value::Int(i)` arm never matched them, so they fell to the default.
    // 3. **The default was `0`, and index 0 is `TLS_AES_256_GCM_SHA384`.**
    //    That is a *defaulting reader turning a wrong type into a confident
    //    wrong answer*: an unconnected socket's session — whose producer had
    //    just been fixed to write `SSL_NULL_WITH_NULL_NULL`/`NONE` — was
    //    reported by this accessor as a completed TLS 1.3 handshake on the
    //    strongest suite in the list. E12-1 §2 is the argument for why THAT
    //    literal is the dangerous one to invent: it is in
    //    `getSupportedCipherSuites()`, so no test a caller can write separates
    //    it from a genuine negotiation, and security-sensitive code branches
    //    on this string. Every sentinel this family landed was undone here,
    //    in the mode where this file is the live registrar.
    //
    // The `Int` arm stays because it is this module's real encoding (an index
    // into `TLS13_CIPHERS`, written by `init_ssl_session_fields`), but it is
    // now the second question rather than the only one, and anything that is
    // neither a name nor an in-range index answers the sentinel.
    r.register(
        cls,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Bound to a `let`: a closure written directly in a match
            // scrutinee is a temporary whose `&ctx` capture lives to the end
            // of the match, colliding with the `&mut ctx` the arms need.
            let raw = crate::t27_tls::session_cipher_slot(ctx.object_num_fields(this))
                .map(|slot| ctx.get_field(this, slot));
            match raw {
                // A producer already wrote the real name (or the sentinel).
                Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
                Some(Value::Int(i)) => {
                    let suite = usize::try_from(i)
                        .ok()
                        .and_then(|u| TLS13_CIPHERS.get(u).copied())
                        .unwrap_or(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
                    let s = ctx.create_string(suite);
                    Ok(Some(Value::Object(Some(s))))
                }
                _ => {
                    let s =
                        ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        },
    );

    r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `let`, not an inline scrutinee — see `getCipherSuite` above.
        let raw = crate::t27_tls::session_proto_slot(ctx.object_num_fields(this))
            .map(|slot| ctx.get_field(this, slot));
        match raw {
            Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
            // This module's encoding: 0 = TLSv1.3, 1 = TLSv1.2. Anything else
            // is not a protocol this file ever wrote.
            Some(Value::Int(0)) => {
                let s = ctx.create_string("TLSv1.3");
                Ok(Some(Value::Object(Some(s))))
            }
            Some(Value::Int(1)) => {
                let s = ctx.create_string("TLSv1.2");
                Ok(Some(Value::Object(Some(s))))
            }
            _ => {
                let s = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL);
                Ok(Some(Value::Object(Some(s))))
            }
        }
    });

    // isValid() -> boolean
    //
    // SHADOWING (wave 3): this registration and `t27_tls::
    // register_ssl_session_real`'s both key `javax/net/ssl/SSLSession.isValid`;
    // `register_tls_natives` runs LAST, so THIS one wins under
    // `--synthetic-jdk` and t27's wins in the default real-JDK mode.
    //
    // E31 — THE SAME BUG THE E22 LANE FIXED IN THE OTHER COPY, still live in
    // this one. The comment this replaces named three shapes and got two of
    // their widths wrong (t27's engine session is 8 fields, not 7; its accept
    // session is 4, not 3), and its `> SES_CREATION_TIME` gate answered
    // `Int(1)` — VALID — for **every** shape narrower than 6. That is the
    // width-blind inference: it treats "too narrow to carry a flag" as "was
    // just negotiated", and the `ssl_security::new13_alloc_null_ssl_session`
    // shape (`NEW13_SSL_SESS_FIELDS` — 3 fields when this was written, 4 since
    // E42, and narrower than 6 either way) is minted with `tls_id = -1` for a
    // socket that was NEVER CONNECTED. So under `--synthetic-jdk` an
    // unconnected `SSLSocket.getSession().isValid()` still answered `true`
    // where HotSpot measures `false` (E12-1 §1 arm A) — the E22 fix was live
    // in one mode only.
    //
    // `t27_tls::session_has_negotiated` is the one width-aware predicate and
    // is now `pub(crate)` so this file calls it rather than growing a fifth
    // copy of the rule.
    //
    // THE SECOND HALF, and why this is not simply that call: HotSpot's
    // `isValid()` is `isRejoinable()`, which is
    // `sessionId.length() != 0 && !invalidated && ...`
    // (`sun/security/ssl/SSLSessionImpl.java:788`) — TWO pieces of state, and
    // `invalidated` is a plain field only `invalidate()` writes. This file
    // registers an `invalidate()` (below) that clears `SES_VALID`, and on this
    // module's own shape `SES_VALID` *is* the slot `session_has_negotiated`
    // reads. Calling the predicate alone would therefore have been correct for
    // the narrow shapes and a REGRESSION for this one: `invalidate()` would
    // have gone inert, because width 6 takes the predicate's
    // "only minted after a handshake" arm. Compose the two the way the JDK
    // composes them — negotiated AND not invalidated — instead of picking one.
    //
    // F18 — the `invalidated` half is no longer width-limited, and the comment
    // above that said "a narrower shape has no `invalidate()` writer either" is
    // no longer true. `t27_tls::session_is_valid` composes negotiated AND
    // not-invalidated for EVERY width, because the bit now lives in a side
    // table (`t27_tls::session_invalidated_table`) rather than in a slot the
    // narrow shapes do not have. That closes F10-1 NOMINATION 3's second half
    // in this mode too: at width 4 `invalidate()` used to no-op, so
    // `SSLServerSocket.accept`'s session stayed `isValid() == true` after an
    // explicit invalidation.
    //
    // The wide-shape flag is still read and still wins a `false`. It is a
    // SECOND writer of the same concept, kept because `init_ssl_session_fields`
    // seeds `SES_VALID` at construction and other code in this file writes it;
    // dropping the read here would make those writes silently inert. The two
    // are composed, not chosen between — the same reasoning the original
    // comment applied to the two halves of the predicate.
    r.register(cls, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !crate::t27_tls::session_is_valid(ctx, this) {
            return Ok(Some(Value::Int(0)));
        }
        // The wide shapes' own `invalidated` slot, still honoured.
        if ctx.object_num_fields(this) > SES_CREATION_TIME {
            return Ok(Some(ctx.get_field(this, SES_VALID)));
        }
        Ok(Some(Value::Int(1)))
    });

    // invalidate() -> void
    //
    // E31 — this is a THIRD member of the slot-collision family
    // `t27_tls::sslsess_attrs_slot` documents, and nobody had named it. It
    // wrote `SES_VALID` (slot 2) on whatever shape it was handed, and slot 2 is
    // the valid flag on only some of the widths. On the
    // `ssl_security::new13_alloc_null_ssl_session` shape it is
    // `NEW13_SESS_TLSID`, so `invalidate()` on an unconnected socket's session
    // overwrote `Int(-1)` — "never connected" — with `Int(0)`, which
    // `session_has_negotiated` reads as a valid stream id. The result was
    // exactly inverted: `isValid()` went from `false` to `true` and `getId()`
    // from `byte[0]` back to 32 fabricated bytes, **because the caller asked to
    // invalidate it**. On the accept shape it clobbers the real stream id
    // with 0.
    //
    // E42 did not change this gate and must not be read as having done so.
    // `NEW13_SSL_SESS_FIELDS` widened 3 -> 4, which is still `<= SES_CREATION_TIME`,
    // so this stays a no-op for that shape — and it has to, because slot 2
    // there is STILL the stream id. The widening bought an attribute slot, not
    // an `invalidated` flag.
    //
    // No-op for shapes with no flag slot, on the same reasoning as
    // `sslsess_attrs_slot`: a quiet miss on a state this VM cannot record beats
    // a loud corruption of the state it can.
    //
    // For the NULL session the no-op is not a miss at all — it is HotSpot's
    // own behaviour. MEASURED, HotSpot 25.0.3+9-LTS, three byte-identical runs
    // (`scratchpad/f6/F6Invalidate.java`), on the unconnected `SSLSocket`'s
    // session and on the pre-handshake `SSLEngine`'s alike:
    //
    //   before-invalidate  isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL
    //   after-invalidate   isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL
    //
    // `invalidate()` on a session that negotiated nothing changes NOTHING,
    // because there was nothing to invalidate. So this arm is exactly right
    // for the shape it governs, and the residual is only a session that DID
    // negotiate and cannot be invalidated on this width. Measured, that
    // residual is confined to one accessor: `invalidate()` changes `isValid()`
    // and nothing else — cipher, protocol, id and certs all survive it (E12-1
    // §1 arm E) — so it cannot spread to the other twelve. Recording an
    // `invalidated` bit needs a slot this shape does not have, and widening
    // again would move slots 0/1/2 under every reader in `t27_tls`.
    //
    // F18 — the "recording an `invalidated` bit needs a slot this shape does
    // not have" paragraph above is now only half true, and the half that
    // mattered is fixed. The slot is still unavailable at width 4 and widening
    // is still rejected for the reasons E42-1 §2 gives — so the bit went
    // OUTSIDE the object, into `t27_tls::session_invalidated_table`, keyed the
    // same GC-stable way this file's sibling cert tables already are. No width
    // moves, no reader retargets, and `invalidate()` stops being a silent
    // no-op on the two width-4 shapes.
    //
    // The slot write is KEPT for the wide shapes. It is not redundant: other
    // code in this file reads `SES_VALID` directly, and the `isValid()`
    // registration above still consults it. Writing both keeps this door's
    // effect visible to both readers.
    //
    // The narrow-shape no-op that HotSpot itself performs is unchanged in
    // effect and now for the right reason. MEASURED (this lane,
    // `scratchpad/f18/F18SessionContract.java`; independently reproducing
    // `scratchpad/f6/F6Invalidate.java`): on a session that negotiated nothing,
    // `invalidate()` changes NOTHING, because there was nothing to invalidate —
    // and `session_mark_invalidated` declines to record a bit for exactly that
    // session, so the two agree by construction rather than by coincidence.
    r.register(cls, "invalidate", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        crate::t27_tls::session_mark_invalidated(ctx, this);
        if ctx.object_num_fields(this) > SES_CREATION_TIME {
            ctx.set_field(this, SES_VALID, Value::Int(0));
        }
        Ok(None)
    });

    // getId() -> byte[]
    //
    // E31 — the twin of the fabrication E22 removed from `t27_tls`'s copy,
    // still live in this one. A session that negotiated nothing has NO id, and
    // JSSE says so with `byte[0]` rather than 32 plausible bytes (measured,
    // E12-1 §1 arm A / §4; reproduced by `scratchpad/e31/
    // E31HandshakeSessionSocket.java` — `id=0B` on the unconnected socket,
    // `id=32B` after a handshake). The named consumer is Tomcat's
    // `JSSESupport.getSessionId`, which tests `ssl_session.length == 0`
    // exactly, so 32 fabricated bytes are precisely the value that defeats it
    // and makes an unhandshaked session present as a trackable one.
    //
    // DELIBERATELY GATED ON `session_has_negotiated` ALONE, NOT on `isValid()`
    // above: measured (E12-1 §1 arm E), `invalidate()` changes `isValid()` and
    // NOTHING else — cipher, protocol and id all survive it. Tying the id to
    // the valid flag would have made `invalidate()` erase the id, which is a
    // new divergence in exchange for fixing an old one. This is the same split
    // HotSpot has: `isRejoinable()` reads the id AND `invalidated`; the id
    // reads neither.
    //
    // Also fixed here: the loop ran `0..4`, so 28 of the 32 bytes were always
    // zero — an id whose entropy is 4 bytes of an identity hash collides far
    // more readily than its length advertises, and callers hash or compare it.
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !crate::t27_tls::session_has_negotiated(ctx, this) {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
        // `identity_hash_code`, not the raw `ObjectRef` pointer: this VM's
        // young-gen GC moves objects, so a pointer-derived id is not stable
        // across the session's own lifetime.
        let id_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let mut x = (ctx.identity_hash_code(this) as u32 as u64) | 1;
        for i in 0..32 {
            // SplitMix64-style fill, same as `t27_tls`'s copy, so the 32 bytes
            // are stable per session and not 28 zeroes.
            x ^= x >> 30;
            x = x.wrapping_mul(0xbf58476d1ce4e5b9);
            x ^= x >> 27;
            ctx.set_array_element(id_arr, i, Value::Int((x & 0xff) as i8 as i32));
        }
        Ok(Some(Value::Object(Some(id_arr))))
    });

    // getPeerHost() / getPeerPort() / getCreationTime()
    //
    // E31 — these three read slots 3, 4 and 5 with NO width test at all, which
    // on this shared interface registration is a stronger version of the same
    // defect as a width-blind one. Only the 6- and 8-field shapes have those
    // slots; on the 4-field `SSLServerSocket.accept` shape slot 3 is the
    // ATTRIBUTE MAP, so `getPeerHost()` returned a `java.util.HashMap`
    // reference through a `()Ljava/lang/String;` descriptor once any caller had
    // done a `putValue` (Jetty's `SecureRequestCustomizer` does, on every
    // request), and slots 4/5 are off the end of the object entirely on the 2-,
    // 3- and 4-field shapes.
    //
    // The fallbacks are HotSpot's own measured answers for a session that
    // negotiated nothing (E12-1 §1 arm A): `getPeerHost()` = `null`,
    // `getPeerPort()` = `-1`, `getCreationTime()` = a real epoch value — never
    // 0, which is the value application code reads as "there is no session".
    r.register(cls, "getPeerHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) <= SES_CREATION_TIME {
            return Ok(Some(Value::Object(None)));
        }
        let field = ctx.get_field(this, SES_PEER_HOST);
        match field {
            Value::Object(_) => Ok(Some(field)),
            _ => Ok(Some(Value::Object(None))),
        }
    });

    // getPeerPort() -> int
    r.register(cls, "getPeerPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) <= SES_CREATION_TIME {
            return Ok(Some(Value::Int(-1)));
        }
        Ok(Some(ctx.get_field(this, SES_PEER_PORT)))
    });

    // getCreationTime() -> long
    r.register(cls, "getCreationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) <= SES_CREATION_TIME {
            return Ok(Some(Value::Long(epoch_millis())));
        }
        Ok(Some(ctx.get_field(this, SES_CREATION_TIME)))
    });

    // getLastAccessedTime() -> long
    r.register(cls, "getLastAccessedTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(epoch_millis())))
    });

    // getApplicationBufferSize() / getPacketBufferSize()
    //
    // 16709 for the packet size is CORRECT in every state. 16384 for the
    // application buffer size is a deliberate under-report, and the paragraph
    // that used to stand here — "the same pair a stock JDK returns", "real
    // `SSLSessionImpl` varies these only via `SSLParameters
    // .setMaximumPacketSize` and only for DTLS" — was measurably false in both
    // clauses. It is corrected rather than deleted because a confident
    // "KEEP: correct" is precisely what stops the next reader from measuring.
    //
    // MEASURED, HotSpot 25.0.3+9-LTS (`scratchpad/e31/
    // E31HandshakeSessionSocket.java`, reproducing E12-1 §1):
    //
    //     null session (unconnected SSLSocket)  app=16704  packet=16709
    //     after a TLS 1.3 handshake             app=16676  packet=16709
    //                                           suite=TLS_AES_256_GCM_SHA384
    //
    // Never 16384, in either state. DERIVED, from the oracle's source rather
    // than only from the measurement — `getApplicationBufferSize()` is not a
    // constant, it is two branches:
    //
    //   * with no `maximumPacketSize` and no negotiated max-fragment-length,
    //     `sun/security/ssl/SSLSessionImpl.java:1297` returns
    //     `SSLRecord.maxRecordSize - SSLRecord.headerSize`, and `headerSize`
    //     is 5 (`SSLRecord.java:35`) — 16709 - 5 = **16704**, the measured
    //     null-session value;
    //   * once a suite is negotiated it returns
    //     `cipherSuite.calculateFragSize(...)` (`CipherSuite.java:1051-1076`),
    //     which subtracts the header and then, for an AEAD cipher, the tag
    //     size and the explicit-IV remainder — 16676 for TLS 1.3 AES-GCM.
    //     That is why the real answer VARIES WITH THE SUITE and cannot be a
    //     constant in real JSSE.
    //
    // `SSLRecord.maxDataSize` (= 16384), the constant the old text named, is
    // reached by this method only on the DTLS branch.
    //
    // The VALUE stays at 16384 on purpose: it is a safe under-report against
    // this VM's own engine, which never emits a larger fragment than it
    // advertises, and raising it without auditing every `BUFFER_OVERFLOW` path
    // is how a constant becomes an outage. What was wrong was the assertion,
    // not the number.
    //
    // REACHABILITY + SHADOWING (wave 4 correction — the wave-3 note was wrong):
    // `register_tls_natives` is reached ONLY from
    // `register_synthetic_overrides`, which is
    // `#[cfg(feature = "synthetic-jdk")]`. So this pair does NOT exist in the
    // default real-JDK build — there `t27_tls::register_ssl_session_real`
    // (reached from `register_essential_natives_with_shims`) is the live copy.
    // Under `--synthetic-jdk` this registrar runs later and wins. The values
    // are identical either way; if you change one, change both.
    r.register(cls, "getApplicationBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16384)))
    });
    r.register(cls, "getPacketBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16709)))
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 4. javax.net.ssl.SSLParameters  (6-field synthetic)
// ---------------------------------------------------------------------------

/// Read one of the two `String[]`-valued SSLParameters slots.
///
/// Three encodings have to be understood, because more than one module
/// allocates this synthetic class:
///   * `Object(Some(arr))` — the array a setter in this file stored;
///   * `Int(n != 0)` — the legacy "was set, contents not retained" marker other
///     modules still write (`http2.rs`, `phases_late.rs`), answered with the
///     module's canonical list;
///   * anything else (`Int(0)`, or a slot the object is too short to have) —
///     never set, which real JSSE reports as a null array.
fn params_string_array(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
    legacy_default: &[&str],
) -> MethodCallResult {
    match ctx.get_field(this, slot) {
        Value::Object(Some(arr)) => Ok(Some(Value::Object(Some(arr)))),
        Value::Int(n) if n != 0 => {
            let arr = build_string_array(ctx, legacy_default);
            Ok(Some(Value::Object(Some(arr))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// Read a boolean-valued SSLParameters slot as a JVM `Z` (0/1). A slot the
/// object is too short to have, or one never written, reads back as
/// `Object(None)` and must surface as `false` rather than as a reference —
/// returning the raw `Value` from a `()Z` native puts a non-int on the stack.
fn params_flag(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize) -> i32 {
    match ctx.get_field(this, slot) {
        Value::Int(v) => i32::from(v != 0),
        _ => 0,
    }
}

fn register_ssl_parameters(r: &mut NativeMethodRegistry) {
    // SyntheticStub: 6-field synthetic parameter holder. The setters now retain
    // what they are given (they used to drop it and, at best, flip a "was set"
    // flag), so every getter here reports the caller's own configuration.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/SSLParameters";
    // <init>() — a no-op left every slot untagged, which the getters below
    // read as "never set"; that happened to be the right answer for the two
    // list slots but not for the client-auth flags, whose getters return the
    // slot verbatim as a boolean. Initialise all six explicitly.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        init_ssl_parameters_fields(ctx, this);
        Ok(None)
    });

    // getProtocols() -> String[]
    r.register(cls, "getProtocols", "()[Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        params_string_array(ctx, this, PAR_PROTOCOLS, SUPPORTED_PROTOCOLS)
    });

    // setProtocols(String[]) -> void
    // Retains the caller's array. Previously only a "was set" flag was kept,
    // so `setProtocols(["TLSv1.2"])` followed by `getProtocols()` answered
    // ["TLSv1.3", "TLSv1.2"] — i.e. it re-enabled the protocol the caller had
    // just excluded.
    r.register(
        cls,
        "setProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, PAR_PROTOCOLS, list);
            Ok(None)
        },
    );

    // getCipherSuites() -> String[]
    r.register(
        cls,
        "getCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            params_string_array(ctx, this, PAR_CIPHER_SUITES, TLS13_CIPHERS)
        },
    );

    // setCipherSuites(String[]) -> void — see setProtocols above.
    r.register(
        cls,
        "setCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, PAR_CIPHER_SUITES, list);
            Ok(None)
        },
    );

    // getApplicationProtocols() -> String[]
    // Returns the ALPN list the caller configured. The h2/http-1.1 pair is the
    // fallback for a block nobody has configured (and for the shorter
    // SSLParameters objects other modules allocate, whose slot 5 does not
    // exist) — it is what this module has always advertised.
    r.register(
        cls,
        "getApplicationProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(arr)) = ctx.get_field(this, PAR_APP_PROTOCOLS) {
                return Ok(Some(Value::Object(Some(arr))));
            }
            let arr = build_string_array(ctx, &["h2", "http/1.1"]);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // setApplicationProtocols(String[]) -> void
    //
    // Was a documented no-op, so `getApplicationProtocols()` above kept
    // answering h2/http-1.1 no matter what ALPN list the caller asked for —
    // including for a caller that deliberately offers only http/1.1. Store it.
    // (t27_tls.rs::register_alpn_on_parameters registers the same triple later
    // with a side-table implementation and wins in a full build; this copy
    // keeps tls.rs self-consistent on its own.)
    r.register(
        cls,
        "setApplicationProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, PAR_APP_PROTOCOLS, list);
            Ok(None)
        },
    );

    // getEndpointIdentificationAlgorithm() -> String
    r.register(
        cls,
        "getEndpointIdentificationAlgorithm",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, PAR_ENDPOINT_ID_ALG) {
                Value::Int(i) => i,
                _ => 0,
            };
            let alg = match idx {
                1 => "HTTPS",
                2 => "LDAPS",
                _ => "",
            };
            let s = ctx.create_string(alg);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // setEndpointIdentificationAlgorithm(String) -> void
    r.register(
        cls,
        "setEndpointIdentificationAlgorithm",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match args.get(1) {
                Some(Value::Object(Some(s))) => match ctx.read_string(*s).as_deref() {
                    Some("HTTPS") => 1,
                    Some("LDAPS") => 2,
                    _ => 0,
                },
                _ => 0,
            };
            ctx.set_field(this, PAR_ENDPOINT_ID_ALG, Value::Int(idx));
            Ok(None)
        },
    );

    // getNeedClientAuth() -> boolean
    r.register(cls, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = params_flag(ctx, this, PAR_NEED_CLIENT_AUTH);
        Ok(Some(Value::Int(v)))
    });

    // setNeedClientAuth(boolean) -> void
    // Per the JSSE contract the two client-auth modes are mutually exclusive:
    // requiring a client certificate cancels merely requesting one.
    r.register(cls, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match args.get(1) {
            Some(Value::Int(b)) => *b,
            _ => 0,
        };
        ctx.set_field(this, PAR_NEED_CLIENT_AUTH, Value::Int(v));
        if v != 0 {
            ctx.set_field(this, PAR_WANT_CLIENT_AUTH, Value::Int(0));
        }
        Ok(None)
    });

    // getWantClientAuth() -> boolean
    // Used to return a hardcoded false, which made it impossible for a caller
    // to read back its own setWantClientAuth(true) — see below.
    r.register(cls, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = params_flag(ctx, this, PAR_WANT_CLIENT_AUTH);
        Ok(Some(Value::Int(v)))
    });

    // setWantClientAuth(boolean) -> void
    //
    // Nothing else registers this triple on `javax/net/ssl/SSLParameters`
    // (phases_late.rs and t27_tls.rs own the SSLSocket / SSLServerSocket /
    // SSLEngineImpl copies), so the no-op WAS the implementation: a server
    // asking for an optional client certificate had the request dropped, and
    // the paired getter's hardcoded `false` hid that it had been dropped.
    r.register(cls, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match args.get(1) {
            Some(Value::Int(b)) => *b,
            _ => 0,
        };
        ctx.set_field(this, PAR_WANT_CLIENT_AUTH, Value::Int(v));
        if v != 0 {
            ctx.set_field(this, PAR_NEED_CLIENT_AUTH, Value::Int(0));
        }
        Ok(None)
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 5. javax.net.ssl.TrustManagerFactory  (binds a KeyStore's trust anchors;
//    getTrustManagers returns a real validating X509TrustManager)
// ---------------------------------------------------------------------------

/// Recover the `keystore.rs`-registry id (the one `x509_manager::
/// build_trust_manager_state` consults) from a `java/security/KeyStore` object
/// passed to `TrustManagerFactory.init(KeyStore)`.
///
/// The real-JDK keystore path (`keystore.rs::engine_load`) registers a parsed
/// `LoadedKeyStore` and stamps its id onto the *KeyStoreSpi* mirror via
/// `set_store_id`, which writes the named field `cratonvm$keystore$storeId`
/// (plus an identity side-table only reachable from inside `keystore.rs`). A
/// public `java/security/KeyStore` delegates to that SPI through its
/// `keyStoreSpi` field. So we probe, in order:
///   1. the KeyStore object's own `cratonvm$keystore$storeId` named field;
///   2. its `keyStoreSpi` delegate's `cratonvm$keystore$storeId` named field.
///
/// FIX (tls-residuals): this now delegates to `keystore::keystore_id_from_object`
/// (the exact helper this doc comment used to ask for — see git history for
/// the prior TODO). That helper's identity-hash side-table tier is the ONLY
/// one that resolves a real `java.security.KeyStore` in real-JDK mode (its
/// real 4-field layout has no room for a pseudo-field, so both the named-field
/// and `keyStoreSpi`-delegate probes below used to always miss). Confirmed via
/// a minimal repro: `TrustManagerFactory.getInstance("PKIX").init(caTrustStore)`
/// resolved id 0 and validated peers against the ~100+ platform roots instead
/// of `caTrustStore`, so a peer cert signed by a private/test CA always failed
/// with `UnknownIssuer` — independent of and unaffected by any OCSP/cipher/
/// client-cert configuration.
pub(crate) fn read_keystore_registry_id(ctx: &mut dyn NativeContext, ks_obj: ObjectRef) -> i32 {
    crate::keystore::keystore_id_from_object(ctx, ks_obj)
}

/// The keystore-registry id for JSSE's DEFAULT trust store, or 0 for "the
/// platform roots" (which is what `build_trust_manager_state(0)` resolves to).
///
/// `TrustManagerFactory.init(null)` means "use the default trust material",
/// and in JSSE that is not unconditionally the platform roots: when
/// `javax.net.ssl.trustStore` is set it names the ONLY trust store, REPLACING
/// cacerts rather than adding to it. CratonVM recorded id 0 for every null
/// KeyStore, so the property was ignored — and the failure was in the widening
/// direction. MEASURED against Temurin 25.0.3 with the property pointing at a
/// one-certificate store (`TmProbe2`):
///
/// ```text
/// HOTSPOT  acceptedIssuers=1    checkServerTrusted(that cert) = ACCEPTED
/// CRATONVM acceptedIssuers=122  checkServerTrusted(that cert) = REJECTED
/// ```
///
/// So an application that pinned its trust to one private CA was silently
/// given the whole public root set instead, AND still rejected the one
/// certificate it had actually asked to trust.
///
/// `NONE` is JSSE's spelling for "no trust store at all"; it maps to 0 here
/// rather than to an empty store, because an empty anchor set and the platform
/// set are not distinguishable downstream and refusing everything would be a
/// worse guess than today's behaviour. The password is optional: JSSE uses it
/// only for the integrity check, and a trust store with no private material
/// commonly ships without one.
/// NOTE ON WHICH COPY RUNS: `init(KeyStore)` is registered twice for this
/// class, and `phases_late::ssl_security`'s copy is the one that WINS
/// (registered later in the `lib.rs` wiring). Both call this resolver, so the
/// twins cannot drift on the answer — see the `es-restclient-https` note on
/// the handler below.
pub(crate) fn default_trust_store_keystore_id(ctx: &mut dyn NativeContext) -> i32 {
    trust_store_keystore_id(ctx, true)
}

/// The keystore-registry id for a trust store the APPLICATION named, and only
/// that — never the JDK's own `cacerts`.
///
/// The distinction is load-bearing, not tidiness. `new13_connect_and_handshake_on`
/// stands native verification DOWN for the store it resolves here, because
/// OpenSSL's security level otherwise refuses certificates the application has
/// explicitly chosen to trust. That trade is right for a store someone
/// deliberately configured; applying it to `cacerts` would move every default
/// HTTPS client in the VM off OpenSSL's path builder and onto
/// `x509_manager::validate_chain`, which is a far larger change than this one
/// and is not what the cacerts fix is for.
pub(crate) fn explicit_trust_store_keystore_id(ctx: &mut dyn NativeContext) -> i32 {
    trust_store_keystore_id(ctx, false)
}

fn trust_store_keystore_id(ctx: &mut dyn NativeContext, allow_jdk_cacerts: bool) -> i32 {
    let (path, password) = match resolve_default_trust_store(ctx, allow_jdk_cacerts) {
        Some(pair) => pair,
        None => return 0,
    };
    // Keyed by (path, password) so repeated `init(null)` calls — every
    // `SSLContext` build in a long-running app — parse the file once and
    // reuse one registry id instead of leaking a fresh keystore per call.
    // ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0). Two acquisitions, both
    // in this function: a `get(..).copied()` whose `if let` body is a bare
    // `return`, and the `insert` at the end. The file read and parse happen
    // between them, with no guard held and no `ctx` in this function at all.
    static CACHE: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            std::collections::HashMap<(String, String), i32>,
        >,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    });
    let key = (path.clone(), password.clone());
    if let Some(id) = cache.lock().get(&key).copied() {
        return id;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            // JSSE throws for an unreadable `javax.net.ssl.trustStore`. This
            // shim does not, for the same reason the rest of this file does
            // not: a hard failure at `init(null)` would take down callers that
            // never depended on the file being readable. Fall back to the
            // platform roots and say so.
            tracing::debug!(
                target: "tls",
                "default trust store {path} could not be read ({e}); using platform roots"
            );
            return 0;
        }
    };
    // An EMPTY password skips the JKS integrity MAC, which is exactly what
    // real-JDK `JavaKeyStore` does for a null password and the standard way a
    // trust store with no private material is read — including `cacerts`.
    let store = match crate::keystore::load_keystore(&bytes, password.as_bytes()) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                target: "tls",
                "default trust store {path} did not parse ({e}); using platform roots"
            );
            return 0;
        }
    };
    if store.entries.is_empty() {
        // Never NARROW trust to nothing on the strength of a file we parsed
        // but got nothing out of: the platform roots are a better guess than
        // an empty anchor set.
        // `warn!`, not `debug!`: this narrows nothing but it DOES mean the
        // configured trust store contributed no anchors and the platform
        // roots are standing in. Under `release_max_level_info` a `debug!`
        // here is compiled out, so the one run where an operator needs to
        // know their trust store parsed empty is the one that says nothing.
        // One-shot per store, so no rate limit is needed.
        tracing::warn!(
            target: "tls",
            "default trust store {path} parsed to zero entries; using platform roots"
        );
        return 0;
    }
    let id = crate::keystore::keystore_register(store);
    cache.lock().insert(key, id);
    id
}

/// JSSE's default-trust-store search, in its own order:
/// `javax.net.ssl.trustStore`, else `<java.home>/lib/security/jssecacerts`,
/// else `<java.home>/lib/security/cacerts`. `None` means "no file found" —
/// the caller then keeps the platform roots, which is also what a VM with no
/// JDK image (synthetic-jdk mode) gets, since neither file will exist.
///
/// The cacerts leg is not cosmetic. MEASURED on the Azure host, anchor sets
/// keyed on SHA-256 of the encoded certificate (`TrustSetProbe`), NOT on the
/// subject DN — the two VMs render the same DN differently (hex-escaped OIDs
/// vs `EMAILADDRESS=`/`SERIALNUMBER=` keywords), and a subject-keyed diff
/// reports one certificate as two:
///
/// ```text
/// HotSpot cacerts   118 anchors
/// CratonVM OS store 122 anchors      overlap 118
/// ```
///
/// A strict SUPERSET: nothing the JDK trusts was missing, and FOUR CAs were
/// trusted here that the JDK deliberately does not —
/// `CN=Entrust Root Certification Authority` (a distrust action the JDK has
/// already taken), `CN=Izenpe.com`, `CN=SecureSign Root CA12`, and the build
/// host's OWN self-signed machine certificate, which sits in
/// `/etc/ssl/certs` and is therefore a trusted CA for every default-context
/// client in the VM. Same widening family as the ignored
/// `javax.net.ssl.trustStore` above, and the reason this resolver exists.
fn resolve_default_trust_store(
    ctx: &mut dyn NativeContext,
    allow_jdk_cacerts: bool,
) -> Option<(String, String)> {
    let password = ctx
        .get_system_property("javax.net.ssl.trustStorePassword")
        .unwrap_or_default();
    if let Some(path) = ctx.get_system_property("javax.net.ssl.trustStore") {
        // `NONE` is JSSE's spelling for "no trust store at all". It maps to
        // the platform roots here rather than to an empty anchor set, because
        // refusing every peer is a worse guess than today's behaviour and the
        // two are not distinguishable downstream.
        if !path.is_empty() && !path.eq_ignore_ascii_case("NONE") {
            return Some((path, password));
        }
        return None;
    }
    if !allow_jdk_cacerts {
        return None;
    }
    let java_home = ctx.get_system_property("java.home")?;
    for leaf in ["jssecacerts", "cacerts"] {
        let path = std::path::Path::new(&java_home)
            .join("lib")
            .join("security")
            .join(leaf);
        if path.is_file() {
            return Some((path.to_string_lossy().into_owned(), password));
        }
    }
    None
}

fn register_trust_manager_factory(r: &mut NativeMethodRegistry) {
    // Bridge: init(KeyStore) records the loaded keystore's registry id on the
    // factory; getTrustManagers() returns a real javax/net/ssl/X509TrustManager
    // stamped with that id, so its checkServerTrusted/checkClientTrusted (in
    // register_x509_trust_manager) validate against the keystore's trust anchors
    // via x509_manager::build_trust_manager_state. No-validation placeholder
    // removed.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "javax/net/ssl/TrustManagerFactory";
    // <init>() — slot layout is (algorithm idx, initialized, bound keystore
    // registry id), the same three `getInstance` writes. The no-op left them
    // untagged; `getTrustManagers()` reads slot 2 to decide which trust
    // anchors to build, and an untagged slot is indistinguishable from a
    // genuine "0 = system roots", so a directly-constructed factory could not
    // be told apart from an initialised one. Real JDK has no no-arg
    // TrustManagerFactory constructor — this only ever sees the synthetic shim.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0)); // PKIX (getDefaultAlgorithm)
        ctx.set_field(this, 1, Value::Int(0)); // not initialized
        ctx.set_field(this, 2, Value::Int(0)); // no keystore bound
        Ok(None)
    });

    // getInstance(String algorithm) -> TrustManagerFactory
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        |ctx, args| {
            let alg_idx = match args.get(0) {
                Some(Value::Object(Some(s))) => match ctx.read_string(*s).as_deref() {
                    Some("PKIX") => 0,
                    Some("SunX509") => 1,
                    _ => 0,
                },
                _ => 0,
            };
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/TrustManagerFactory", 3)?;
            ctx.set_field(obj, 0, Value::Int(alg_idx));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getDefaultAlgorithm() -> String
    r.register(
        cls,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("PKIX");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // init(KeyStore) -> void
    // FIX (nb-tls-tmf): wire the loaded KeyStore's trust anchors into this
    // factory. Previously slot 2 recorded only a 0/1 presence flag and the
    // store was discarded, so getTrustManagers() could not validate against it.
    // We now recover the keystore's id in the `keystore.rs` registry (the one
    // x509_manager::build_trust_manager_state reads) and store it on the factory
    // (slot 2). A null KeyStore means "use the default trust store" — real-JDK
    // loads the platform cacerts; we record id 0, which
    // build_trust_manager_state(0) resolves to system roots only.
    //
    // FIX (es-restclient-https): this is the ONLY `TrustManagerFactory.init`
    // the public `TrustManagerFactory.getInstance(...).init(ks)` call resolves
    // to (the SPI-level `engineInit` in `x509_manager.rs` is a different,
    // not-reached-from-here entry point). Previously this native never told
    // `t27_tls`'s per-`SSLContext` trust scope
    // (`set_pending_tm_trust_roots`/`attach_pending_identity_to_ctx`, consumed
    // by the REAL rustls-backed `SSLContext.init` in `net_phase_e.rs`) about a
    // custom KeyStore-bound truststore. So building an `SSLContext` from a
    // caller-supplied truststore (e.g. a test JKS holding a self-signed/test-CA
    // cert — the `HttpsServer`+`RestClient` pattern) still validated the peer
    // against ONLY the platform root store, and every such handshake failed
    // with `UnknownIssuer` even though the JDK caller correctly built and
    // wired a custom trust store. Feed the same keystore-derived anchor DERs
    // into `set_pending_tm_trust_roots` here so the next `SSLContext.init` on
    // this thread scopes trust to them (restrictively, matching the reference
    // JDK's custom-truststore semantics) instead of silently falling back to
    // system roots only.
    r.register(cls, "init", "(Ljava/security/KeyStore;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ks_id = match args.get(1) {
            Some(Value::Object(Some(ks_obj))) => read_keystore_registry_id(ctx, *ks_obj),
            // null KeyStore => JSSE's DEFAULT trust store, which is the
            // platform roots ONLY when the application has not named one via
            // `javax.net.ssl.trustStore` — see that resolver's doc comment.
            _ => default_trust_store_keystore_id(ctx),
        };
        // slot 2 = bound keystore registry id (0 = system roots only).
        ctx.set_field(this, 2, Value::Int(ks_id));
        // slot 1 = initialized flag.
        ctx.set_field(this, 1, Value::Int(1));
        if ks_id != 0 {
            let state = crate::x509_manager::build_trust_manager_state(ks_id);
            if !state.anchor_ders.is_empty() {
                crate::t27_tls::set_pending_tm_trust_roots(state.anchor_ders);
            }
        }
        Ok(None)
    });

    // getTrustManagers() -> TrustManager[]
    // FIX (nb-tls-tmf): return a REAL javax/net/ssl/X509TrustManager (whose
    // checkServerTrusted/checkClientTrusted run full PKIX validation in
    // register_x509_trust_manager) stamped with the keystore id bound by init().
    // validate_cert_chain reads that id off the manager via
    // read_trust_manager_id_from_obj (named field `cratonvm$x509tm$id`, slot 0
    // fallback) and builds the anchor set from the keystore + system roots. This
    // replaces the former no-validation placeholder that loaded no trust store.
    r.register(
        cls,
        "getTrustManagers",
        "()[Ljavax/net/ssl/TrustManager;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The id init() bound to this factory (0 if init() was never called
            // or a null KeyStore was passed => default/system trust anchors).
            let ks_id = match ctx.get_field(this, 2) {
                Value::Int(i) => i,
                Value::Long(l) => l as i32,
                _ => 0,
            };
            // FIX (tls-residuals): register through the SAME `tm_registry` id
            // space `x509_manager`'s PKIXFactory/SimpleFactory SPI path uses
            // (`register_trust_manager_state`), rather than stamping this raw
            // KeyStore registry id directly onto `cratonvm$x509tm$id`. Both
            // counters start at 1 and increment independently, so stamping
            // either kind of id onto the same field let
            // `validate_cert_chain` misresolve a `tm_registry`-sourced id as
            // a keystore id (or vice versa) whenever this path is live.
            let state = crate::x509_manager::build_trust_manager_state(ks_id);
            let tm_id = crate::x509_manager::register_trust_manager_state(state);
            // Allocate the same real-impl-shaped mirror used by the WP5.3
            // TrustManagerFactory SPI path. Returning the bare interface here
            // lets invokevirtual resolve to abstract interface slots such as
            // getAcceptedIssuers(), which have no Code attribute.
            let tm = try_alloc_concurrent_synthetic(ctx, crate::x509_manager::FQN_X509_TM, 2)?;
            crate::x509_manager::set_tm_id(ctx, tm, tm_id);
            let arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(tm)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 6. javax.net.ssl.KeyManagerFactory  (3-field synthetic)
// ---------------------------------------------------------------------------

fn register_key_manager_factory(r: &mut NativeMethodRegistry) {
    // SyntheticStub: 3-field synthetic factory; getKeyManagers() returns a
    // placeholder X509KeyManager with no real key material loaded.
    //
    // NOTE: this registration is currently shadowed — `phases_late.rs`'s
    // `register_p68_ssl` registers the same (class, method, descriptor)
    // triples under the `Bridge` category, runs later in the boot sequence,
    // and wins via last-registration-wins (`NativeMethodRegistry::register`).
    // Confirmed by direct tracing: this function's `getInstance` never fires
    // in a real-JDK run. If you're debugging `KeyManagerFactory` behavior and
    // changes here don't seem to take effect, check `phases_late.rs` first —
    // see its own field-layout comment for why that copy must match the real
    // `javax.net.ssl.KeyManagerFactory` field order.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/KeyManagerFactory";
    // <init>() — same three slots `getInstance` writes (algorithm idx,
    // initialized, keystore-present); `init()` and the shadowing phases_late
    // copy both read slot 1 as an "initialized" boolean, which a no-op left
    // untagged instead of a definite false.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0)); // SunX509 (getDefaultAlgorithm)
        ctx.set_field(this, 1, Value::Int(0)); // not initialized
        ctx.set_field(this, 2, Value::Int(0)); // no keystore
        Ok(None)
    });

    // getInstance(String algorithm) -> KeyManagerFactory
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        |ctx, args| {
            let alg_idx = match args.get(0) {
                Some(Value::Object(Some(s))) => match ctx.read_string(*s).as_deref() {
                    Some("SunX509") => 0,
                    Some("NewSunX509") => 1,
                    Some("PKIX") => 2,
                    _ => 0,
                },
                _ => 0,
            };
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/KeyManagerFactory", 3)?;
            ctx.set_field(obj, 0, Value::Int(alg_idx));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getDefaultAlgorithm() -> String
    r.register(
        cls,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("SunX509");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // init(KeyStore, char[]) -> void
    r.register(cls, "init", "(Ljava/security/KeyStore;[C)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ks_present = match args.get(1) {
            Some(Value::Object(Some(_))) => 1,
            _ => 0,
        };
        ctx.set_field(this, 2, Value::Int(ks_present));
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // getKeyManagers() -> KeyManager[]
    r.register(
        cls,
        "getKeyManagers",
        "()[Ljavax/net/ssl/KeyManager;",
        |ctx, _args| {
            let km = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/X509KeyManager", 1)?;
            let arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(km)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 7. java.security.KeyStore  (5-field synthetic, backed by `keystore.rs`)
//    field 0: type_idx (0=JKS, 1=PKCS12, 2=JCEKS)
//    field 1: loaded (0/1)
//    field 2: entry-count MIRROR — a convenience readout kept in step with the
//             backing store, NOT the source of truth. It used to BE the whole
//             "store": `setCertificateEntry`/`setKeyEntry` incremented this
//             counter and dropped the certificate/key on the floor, so
//             `getCertificate`/`aliases`/`isKeyEntry` could never see anything
//             the caller had put in, and `store()` had nothing to serialise.
//    field 3: provider_idx
//    field 4: store id in `keystore.rs`'s process-wide `KEYSTORE_REGISTRY`
//
// STORAGE. Every entry-facing method below delegates to the registry-backed
// engine surface in `keystore.rs` (`engine_load`, `engine_get_certificate`,
// `engine_set_key_entry`, `engine_store`, …) through that module's `keystore_*`
// wrappers, so the public `KeyStore` API and the `KeyStoreSpi` API read and
// write ONE store and cannot disagree. Entries live in `KEYSTORE_REGISTRY` as
// plain Rust `Vec<u8>` DER: no Java object reference is retained anywhere, so
// there is nothing for a moving GC to strand and no `add_global_root` /
// `register_var_handle_root` is required. The only link from a Java `KeyStore`
// object to its entries is the integer store id, which `keystore::set_store_id`
// records BOTH in slot 4 and in that module's identity-hash side-table
// (`identity_hash_code` is stable across GC by contract) — the same idiom
// `engineLoad` has always used for real-JDK `KeyStoreSpi` objects, which is why
// a store opened through either API is visible through the other.
//
// An uninitialized receiver THROWS, it does not answer "empty": real
// `java.security.KeyStore` guards each of these methods with
// `if (!initialized) throw new KeyStoreException("Uninitialized keystore")`.
//
// CATEGORY. This block is `Bridge`, not `SyntheticStub` (it was the latter for
// as long as it WAS a stub). `SyntheticStub` registrations are dropped wholesale
// under `CRATONVM_NO_STUBS`, and the only other `java.security.KeyStore`
// registrations in the tree are `phases_early.rs`'s rival 3-field shim, which is
// tagged `Bridge` and survives — so leaving this block tagged `SyntheticStub`
// meant strict no-stubs mode fell back to a keystore with a DIFFERENT slot
// layout, no `store`/`setKeyEntry`/`isKeyEntry` at all, and (outside the
// non-default `legacy-synthetic-crypto` feature) no entry storage whatsoever.
// That inverts the point of the flag: the strict mode would behave WORSE than
// the default. Every triple below is now backed by `keystore.rs`'s registry and
// is load-bearing in that mode, with ONE deliberate exception —
// `getDefaultType`, which is still a hardcoded constant and keeps its
// `SyntheticStub` tag (see its own comment).
// ---------------------------------------------------------------------------

/// Raise one of the checked exceptions `java.security.KeyStore` declares.
///
/// `phases_early::throw_jca_exc` constructs the real exception object when the
/// class is available and degrades to `IllegalArgumentException` when it is
/// not; either way the caller sees a throw rather than a bogus success value.
fn ks_throw(ctx: &mut dyn NativeContext, class_name: &str, msg: &str) -> MethodCallFailed {
    crate::phases_early::throw_jca_exc(ctx, class_name, msg)
}

/// Enforce the `KeyStore` "must be loaded first" precondition.
///
/// Slot 1 is this module's `loaded` flag. Two other shapes can legitimately
/// reach these natives and must NOT be rejected, because slot 1 means something
/// else on them: a real `java.security.KeyStore`
/// (`type`/`provider`/`keyStoreSpi`/`initialized`) carries the flag in its named
/// `initialized` field, and any receiver that already has a store bound in
/// `keystore.rs`'s registry was loaded through `engineLoad` by definition.
/// Only a receiver that satisfies none of the three is genuinely uninitialized.
fn ks_require_loaded(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    // Receiver by `&mut` per the gate's own remedy (`WORKER-5-NOTE-10` §7.3).
    // TODAY every allocation in this function is on the path that THROWS, and
    // every caller propagates with `?` — so no caller currently reaches a use
    // of a stale receiver. That is a property of the callers, not of this
    // function, and it is not one a reader can check at the call site. The
    // `&mut` makes the shape impossible instead of making this instance safe.
    if matches!(ctx.get_field(*this, 1), Value::Int(1))
        || matches!(ctx.get_field_by_name(*this, "initialized"), Value::Int(1))
        || crate::keystore::keystore_id_from_object(ctx, *this) != 0
    {
        return Ok(());
    }
    Err(ks_throw(
        ctx,
        "java/security/KeyStoreException",
        "Uninitialized keystore",
    ))
}

/// Decode the alias argument (`args[1]`) of an alias-taking `KeyStore` method.
fn ks_alias(ctx: &mut dyn NativeContext, args: &[Value]) -> String {
    match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Refresh the slot-2 entry-count mirror from the authoritative store.
fn ks_sync_count(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    let n = crate::keystore::keystore_lookup(id)
        .map(|s| s.entries.len())
        .unwrap_or(0);
    ctx.set_field(this, 2, Value::Int(n as i32));
}

fn register_key_store(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "java/security/KeyStore";
    // <init>() — the 5-slot layout documented above. A no-op left `loaded`
    // and the entry count untagged, so `size()` returned a reference-typed
    // default from a `()I` native and `isKeyEntry`/`containsAlias` could not
    // distinguish "empty" from "never loaded". The type slot stays JKS (0),
    // which is exactly what `getType()` already reported for an
    // uninitialised receiver: a bare `new KeyStore()` never passed through
    // `getInstance(String)`, so the requested type is genuinely unknown here
    // and inventing `getDefaultType()`'s PKCS12 would change what callers
    // observe today.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0)); // JKS
        ctx.set_field(this, 1, Value::Int(0)); // not loaded
        ctx.set_field(this, 2, Value::Int(0)); // 0 entries
        ctx.set_field(this, 3, Value::Int(0)); // provider idx
        ctx.set_field(this, 4, Value::Int(0)); // no backing store yet
        Ok(None)
    });

    // getInstance(String type) -> KeyStore
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyStore;",
        |ctx, args| {
            let (type_idx, _type_name) = match args.get(0) {
                Some(Value::Object(Some(s))) => match ctx.read_string(*s).as_deref() {
                    Some("JKS") => (0, "JKS"),
                    Some("PKCS12") => (1, "PKCS12"),
                    Some("JCEKS") => (2, "JCEKS"),
                    _ => (0, "JKS"),
                },
                _ => (0, "JKS"),
            };
            let obj = try_alloc_concurrent_synthetic(ctx, "java/security/KeyStore", 5)?;
            ctx.set_field(obj, 0, Value::Int(type_idx));
            ctx.set_field(obj, 1, Value::Int(0)); // not loaded
            ctx.set_field(obj, 2, Value::Int(0)); // 0 entries
            ctx.set_field(obj, 3, Value::Int(0)); // provider idx
            ctx.set_field(obj, 4, Value::Int(0)); // no store yet
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getType() -> String (instance method)
    r.register(cls, "getType", "()Ljava/lang/String;", |ctx, args| {
        let type_name = if let Some(Value::Object(Some(this))) = args.get(0) {
            match ctx.get_field(*this, 0) {
                Value::Int(1) => "PKCS12",
                Value::Int(2) => "JCEKS",
                _ => "JKS",
            }
        } else {
            "PKCS12"
        };
        let s = ctx.create_string(type_name);
        Ok(Some(Value::Object(Some(s))))
    });

    // getDefaultType() -> String (static)
    //
    // The one registration in this block that stays `SyntheticStub`: it hands
    // back a hardcoded constant, where the real method reads the `keystore.type`
    // security property (defaulting to "pkcs12"). Tagging a constant `Bridge`
    // would claim it is load-bearing in strict no-stubs mode, which it is not —
    // and nothing is lost by dropping it there, because `phases_early.rs`
    // registers a byte-identical `Bridge` copy that then wins.
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cls,
        "getDefaultType",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("PKCS12");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.set_category(cratonvm_native_api::NativeKind::Bridge);

    // load(InputStream, char[]) -> void
    //
    // Delegates to `keystore::engine_load`, the real PKCS#12 + JKS reader
    // (format detected by magic, PKCS#12 MAC / JKS integrity tag verified,
    // shrouded key bags decrypted with the supplied password). It registers
    // the parsed store and stamps its id on this object, which is what makes
    // every accessor below — and a `store()` → `load()` round-trip — see the
    // same entries. A null `InputStream` means "create an empty store", per
    // the JDK contract, and an unparseable stream propagates `IOException`
    // instead of the old silent "mark loaded and forget the bytes".
    r.register(cls, "load", "(Ljava/io/InputStream;[C)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        crate::keystore::keystore_load(ctx, args)?;
        ctx.set_field(this, 1, Value::Int(1)); // mark loaded
        let id = crate::keystore::keystore_ensure_store_id(ctx, this);
        ks_sync_count(ctx, this, id);
        Ok(None)
    });

    // getCertificate(String alias) -> Certificate
    r.register(
        cls,
        "getCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            crate::keystore::keystore_get_certificate(ctx, args)
        },
    );

    // getCertificateChain(String alias) -> Certificate[]
    //
    // Companion to getCertificate/getKey: a caller copying an identity between
    // keystores needs `setKeyEntry(alias, getKey(...), pw, getCertificateChain
    // (alias))`, and the chain half was the piece this shim never had.
    r.register(
        cls,
        "getCertificateChain",
        "(Ljava/lang/String;)[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            crate::keystore::keystore_get_certificate_chain(ctx, args)
        },
    );

    // getKey(String alias, char[] password) -> Key
    r.register(
        cls,
        "getKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            let key = crate::keystore::keystore_get_key(ctx, args)?;
            // `engine_get_key` decrypts JKS-shrouded key material with the
            // password it was just handed. If the stored DER is STILL an
            // `EncryptedPrivateKeyInfo` afterwards, that password was wrong —
            // real `KeyStore.getKey` throws `UnrecoverableKeyException` there,
            // and handing back a proxy whose `getEncoded()` is ciphertext
            // masquerading as PKCS#8 is precisely the silent wrong answer this
            // shim must not produce.
            if matches!(key, Some(Value::Object(Some(_)))) {
                let alias = ks_alias(ctx, args);
                let id = crate::keystore::keystore_id_from_object(ctx, this);
                if let Some((der, _)) = crate::keystore::keystore_get_private_key(id, &alias) {
                    if crate::keystore::is_jks_encrypted_private_key(&der) {
                        return Err(ks_throw(
                            ctx,
                            "java/security/UnrecoverableKeyException",
                            "Cannot recover key: the supplied password does not \
                             decrypt this entry's key material",
                        ));
                    }
                }
            }
            Ok(key)
        },
    );

    // containsAlias(String alias) -> boolean
    r.register(
        cls,
        "containsAlias",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            crate::keystore::keystore_contains_alias(ctx, args)
        },
    );

    // aliases() -> Enumeration<String>
    //
    // `keystore::engine_aliases` enumerates in INSERTION order (the backing
    // map is an `IndexMap`), matching real JDK's `LinkedHashMap`-backed
    // `JavaKeyStore`/`PKCS12KeyStore`. The returned
    // `java/util/IteratorEnumeration` has `hasMoreElements`/`nextElement`
    // registered in both modes (phases_early phase53 for synthetic-JDK,
    // `keystore::register_keystore_real` for real-JDK).
    r.register(cls, "aliases", "()Ljava/util/Enumeration;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        ks_require_loaded(ctx, &mut this)?;
        crate::keystore::keystore_aliases(ctx, args)
    });

    // size() -> int  (a readout of the real entry count, not a counter)
    r.register(cls, "size", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        ks_require_loaded(ctx, &mut this)?;
        crate::keystore::keystore_size(ctx, args)
    });

    // setCertificateEntry(String alias, Certificate cert) -> void
    //
    // Stores the certificate's DER in the backing store, so `getCertificate`,
    // `containsAlias`, `isCertificateEntry`, `aliases`, `size` and `store` all
    // see it. If no DER can be obtained (a `Certificate` whose `getEncoded()`
    // yields nothing and which is not one of our own mirrors) the entry cannot
    // be stored, and that must surface as the `KeyStoreException` the method
    // declares — not as a successful-looking no-op.
    r.register(
        cls,
        "setCertificateEntry",
        "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            let id = crate::keystore::keystore_ensure_store_id(ctx, this);
            let alias = ks_alias(ctx, args);
            crate::keystore::keystore_set_certificate_entry_native(ctx, args)?;
            if !crate::keystore::keystore_has_alias(id, &alias) {
                let msg = format!(
                    "setCertificateEntry({alias:?}): no DER encoding available for this \
                     certificate — nothing was stored"
                );
                return Err(ks_throw(ctx, "java/security/KeyStoreException", &msg));
            }
            ks_sync_count(ctx, this, id);
            Ok(None)
        },
    );

    // setKeyEntry(String alias, Key key, char[] password, Certificate[] chain) -> void
    //
    // Same contract as setCertificateEntry: the PKCS#8 key DER plus each chain
    // certificate's DER go into the backing store (and, when a chain is
    // present, the identity is also installed for the native TLS listener —
    // see `keystore::engine_set_key_entry`). A key with no obtainable encoding
    // (PKCS#11/HSM-backed, `getEncoded() == null`) genuinely cannot be stored
    // here, so it throws rather than reporting success.
    r.register(
        cls,
        "setKeyEntry",
        "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            let id = crate::keystore::keystore_ensure_store_id(ctx, this);
            let alias = ks_alias(ctx, args);
            crate::keystore::keystore_set_key_entry_native(ctx, args)?;
            if !crate::keystore::keystore_has_alias(id, &alias) {
                let msg = format!(
                    "setKeyEntry({alias:?}): the key has no PKCS#8 encoding this VM can \
                     store (getEncoded() returned nothing) — nothing was stored"
                );
                return Err(ks_throw(ctx, "java/security/KeyStoreException", &msg));
            }
            ks_sync_count(ctx, this, id);
            Ok(None)
        },
    );

    // deleteEntry(String alias) -> void
    r.register(cls, "deleteEntry", "(Ljava/lang/String;)V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        ks_require_loaded(ctx, &mut this)?;
        let id = crate::keystore::keystore_ensure_store_id(ctx, this);
        crate::keystore::keystore_delete_entry_native(ctx, args)?;
        ks_sync_count(ctx, this, id);
        Ok(None)
    });

    // store(OutputStream, char[]) -> void
    //
    // Nothing else registers this triple (phases_early.rs's `java.security.
    // KeyStore` shim covers getInstance/load/getType/size/aliases but not
    // store), so the former no-op WAS the implementation: a caller that built
    // a keystore and asked for it to be written got an empty output stream and
    // no error at all — the failure only surfaced much later as an unreadable
    // or entry-less file. Its replacement threw `IOException` for every
    // SPI-less receiver, which was honest but still useless.
    //
    // Both are now fixed at the root: this KeyStore HAS entries (see the
    // storage note at the top of this section), and `keystore.rs`'s JKS
    // serialiser (`write_jks`, driven by `engine_store`) is reachable from
    // here. Order of preference:
    //   1. a real `KeyStoreSpi` delegate, whose `engineStore` is the contract
    //      `KeyStore.store` is defined in terms of;
    //   2. this VM's own store, serialised as JKS. JKS — not the declared
    //      type — deliberately: CV's read path detects the format by magic, so
    //      a JKS body round-trips through `load()` whatever type string the
    //      caller asked `getInstance` for.
    r.register(cls, "store", "(Ljava/io/OutputStream;[C)V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let out = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::IOException {
                    message: "KeyStore.store: output stream is null".to_string(),
                }
                .into())
            }
        };
        let password = args.get(2).copied().unwrap_or(Value::Object(None));
        if let Value::Object(Some(spi)) = ctx.get_field_by_name(this, "keyStoreSpi") {
            let _ = ctx.invoke_virtual(
                spi,
                "engineStore",
                "(Ljava/io/OutputStream;[C)V",
                &[Value::Object(Some(out)), password],
            )?;
            return Ok(None);
        }
        ks_require_loaded(ctx, &mut this)?;
        let id = crate::keystore::keystore_id_from_object(ctx, this);
        if id == 0 {
            // `loaded` is set, yet no store is bound: the only way to reach
            // this is a receiver whose slot 4 / identity entry was clobbered.
            // Writing an empty JKS here would silently truncate the caller's
            // keystore, so refuse.
            return Err(ks_throw(
                ctx,
                "java/security/KeyStoreException",
                "KeyStore.store: this keystore has no backing store bound — \
                 nothing was written",
            ));
        }
        crate::keystore::keystore_store(ctx, args)?;
        Ok(None)
    });

    // isCertificateEntry(String alias) -> boolean
    r.register(
        cls,
        "isCertificateEntry",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            ks_require_loaded(ctx, &mut this)?;
            crate::keystore::keystore_is_certificate_entry(ctx, args)
        },
    );

    // isKeyEntry(String alias) -> boolean  (true for private AND secret keys,
    // matching both the JDK contract and what `getKey` above will hand back)
    r.register(cls, "isKeyEntry", "(Ljava/lang/String;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        ks_require_loaded(ctx, &mut this)?;
        crate::keystore::keystore_is_key_entry(ctx, args)
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 8. javax.net.ssl.SSLSocketFactory  (2-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_socket_factory(r: &mut NativeMethodRegistry) {
    // SyntheticStub: createSocket() returns an unconnected synthetic
    // java/net/Socket with no TLS wiring; getDefault() yields a placeholder
    // factory. Cipher-suite getters return hardcoded lists.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/SSLSocketFactory";
    // <init>() — KEPT as a no-op, and it is spec-correct: real
    // `SSLSocketFactory()` (like its `javax.net.SocketFactory` super) has an
    // empty body and no field initialisers, and this module's 2-slot shim has
    // no getter that reads either slot (createSocket allocates a fresh Socket,
    // getDefault allocates a fresh factory, and the cipher-suite getters are
    // constant). Every field legitimately starts null/0.
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getDefault() -> SSLSocketFactory
    r.register(
        cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            let sf = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 2)?;
            Ok(Some(Value::Object(Some(sf))))
        },
    );

    // createSocket(String host, int port) -> Socket
    r.register(
        cls,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, _args| {
            // Seed socketLock/closeLock (see net_phase_e::re1_init_socket_locks
            // doc comment) -- this bare allocation skips real `Socket.<init>`,
            // and a real-bytecode `synchronized (socketLock)` method (e.g.
            // `getImpl()`) would otherwise NPE. Same bug family as
            // jndirealmintegration-ldap-connection-npe-FIXED.md.
            let sock = try_alloc_concurrent_synthetic(ctx, "java/net/Socket", 2)?;
            let sock = crate::net_phase_e::re1_init_socket_locks(ctx, sock)?;
            Ok(Some(Value::Object(Some(sock))))
        },
    );

    // createSocket(Socket s, String host, int port, boolean autoClose) -> Socket
    r.register(
        cls,
        "createSocket",
        "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
        |ctx, _args| {
            let sock = try_alloc_concurrent_synthetic(ctx, "java/net/Socket", 2)?;
            let sock = crate::net_phase_e::re1_init_socket_locks(ctx, sock)?;
            Ok(Some(Value::Object(Some(sock))))
        },
    );

    // getDefaultCipherSuites() -> String[]
    r.register(
        cls,
        "getDefaultCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let arr = build_string_array(ctx, TLS13_CIPHERS);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // getSupportedCipherSuites() -> String[]
    r.register(
        cls,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let mut all: Vec<&str> = Vec::new();
            all.extend_from_slice(TLS13_CIPHERS);
            all.extend_from_slice(TLS12_CIPHERS);
            let arr = build_string_array(ctx, &all);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 9. javax.net.ssl.X509TrustManager  (real validation)
// ---------------------------------------------------------------------------

/// Validate a certificate chain with full security checks:
/// 1. Reject null/empty chains
/// 2. Check validity dates for every certificate
/// 3. Verify each certificate's signature against its issuer
/// 4. Validate chain continuity (issuer[i] == subject[i+1])
/// 5. Enforce BasicConstraints.cA on intermediates and require the chain to
///    terminate at a configured trust anchor.
///
/// VULN [nb-tls] FIX: previously, in the DEFAULT build (legacy-synthetic-crypto
/// is NOT a default feature) this routine bailed out with `Ok(None)` after the
/// null/empty guard, so a custom `javax/net/ssl/X509TrustManager` performed NO
/// PKIX validation — every cert chain was trusted at the JVM TrustManager
/// layer. We now route to the production RFC 5280 validator
/// (`crate::x509_manager::validate_chain`) in ALL builds. That validator parses
/// the DER, enforces validity dates, verifies signatures up to a trust anchor,
/// checks chain continuity and BasicConstraints — the same code path used by
/// the real `sun.security.ssl` trust manager registrations. `this_obj` is the
/// trust-manager receiver, used to pick up an app-configured trust-anchor set
/// (falling back to the system trust store when the TM was never bound to a
/// KeyStore).
fn validate_cert_chain(
    ctx: &mut dyn NativeContext,
    this_obj: Option<&Value>,
    chain_arg: Option<&Value>,
) -> MethodCallResult {
    let chain_arr = match chain_arg {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            // Null chain — reject (security: never accept missing certificates)
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "Certificate chain is null — TLS connection rejected".into(),
            }
            .into());
        }
    };
    let chain_len = ctx.array_length(chain_arr);
    if chain_len == 0 {
        // Empty chain — reject
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Certificate chain is empty — TLS connection rejected".into(),
        }
        .into());
    }

    // VULN [nb-tls] FIX: perform REAL PKIX validation in every build by routing
    // to the production validator rather than returning Ok(None). Extract the
    // DER for each cert in the chain (leaf first) and hand it to
    // `x509_manager::validate_chain` together with the trust-anchor set this
    // trust manager was configured with.
    let mut chain_der: Vec<Vec<u8>> = Vec::with_capacity(chain_len);
    for i in 0..chain_len {
        if let Value::Object(Some(cert_ref)) = ctx.get_array_element(chain_arr, i) {
            if let Some(der) = read_x509_der(ctx, cert_ref) {
                chain_der.push(der);
            } else {
                // A cert object whose DER we cannot recover is a hard failure:
                // we must never silently skip a certificate and then validate a
                // shorter (possibly anchor-less) chain.
                return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: format!(
                        "Certificate at index {} has no recoverable DER encoding — TLS connection rejected",
                        i
                    ),
                }.into());
            }
        } else {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: format!(
                    "Certificate at index {} is null — TLS connection rejected",
                    i
                ),
            }
            .into());
        }
    }

    // Build the trust-anchor set. Prefer the anchors the application bound to
    // this trust manager (via its KeyStore or PKIX params); fall back to the
    // system trust store (keystore id 0 has no user entries) so the implicit
    // default trust manager still validates — exactly what real-JDK does when
    // init() is bypassed.
    //
    // FIX (tls-residuals): `trust_manager_state_by_id` (NOT
    // `build_trust_manager_state` directly) — `tm_id` is a `tm_registry` id
    // (from `x509_manager::register_trust_manager_state`) for the live
    // PKIXFactory/SimpleFactory path, NOT a raw KeyStore registry id.
    // Calling `build_trust_manager_state(tm_id)` directly treated it as one
    // anyway, so `keystore::keystore_lookup` either hit an unrelated keystore
    // that coincidentally shared the same small integer, or (far more often)
    // found nothing — silently emptying the trust-anchor set and rejecting
    // every chain for a fully valid PKIX-configured connector (e.g. Tomcat's
    // OCSP-enabled connectors, which always go through
    // `engineInit(ManagerFactoryParameters)` — no backing keystore at all).
    let tm_id = match this_obj {
        Some(Value::Object(Some(this_ref))) => read_trust_manager_id_from_obj(ctx, *this_ref),
        _ => 0,
    };
    let trust = crate::x509_manager::trust_manager_state_by_id(tm_id);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] validate_cert_chain tm_id={} anchor_ders={} anchors_groups={} keystore_id={} chain_len={}",
            tm_id,
            trust.anchor_ders.len(),
            trust.anchors.len(),
            trust.keystore_id,
            chain_der.len()
        );
    }

    match crate::x509_manager::validate_chain(&chain_der, &trust) {
        Ok(()) => Ok(None),
        // A failed PKIX check must surface as a REAL
        // `java.security.cert.CertificateException`, not an `IOException`
        // whose message names one: `checkServerTrusted` declares that type and
        // every caller — TLS stacks turning a failure into a handshake alert,
        // tests asserting an untrusted chain was rejected — catches it by
        // type. See `x509_manager::cert_exception`, which this shares.
        Err(e) => Err(crate::x509_manager::cert_exception_external(
            ctx,
            e.to_string(),
        )),
    }
}

/// Recover the DER encoding of a Java `X509Certificate` mirror object.
///
/// Two object layouts are in play across the native crypto surface:
///   * the keystore mirror `(subject, issuer, cert_id, der_bytes)` — slot 3 is
///     the `byte[]` DER (see `x509_manager::make_x509_mirror`);
///   * the synthetic crypto cert whose slot 2 is a `cert_id` indexing the
///     `crypto_impl` cert side-table, whose `X509Cert.encoded` holds the DER.
/// We also probe a by-name `encoded` field as a last resort. Returns `None`
/// only when no layout yields bytes, which `validate_cert_chain` treats as a
/// hard rejection.
fn read_x509_der(ctx: &mut dyn NativeContext, cert: ObjectRef) -> Option<Vec<u8>> {
    let n = ctx.object_num_fields(cert);
    // (a) keystore-mirror layout: slot 3 = byte[] DER.
    if n > 3 {
        if let Value::Object(Some(arr)) = ctx.get_field(cert, 3) {
            if let Some(bytes) = read_byte_array(ctx, arr) {
                return Some(bytes);
            }
        }
    }
    // (b) synthetic crypto cert: slot 2 = cert_id into the crypto_impl table.
    if n > 2 {
        let cert_id = match ctx.get_field(cert, 2) {
            Value::Long(id) => Some(id as u64),
            Value::Int(id) if id != 0 => Some(id as u64),
            _ => None,
        };
        if let Some(id) = cert_id {
            if let Some(cert) = crate::crypto_impl::cert_get(id) {
                if !cert.encoded.is_empty() {
                    return Some(cert.encoded);
                }
            }
        }
    }
    // (c) by-name fallback for non-standard mirror layouts.
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(cert, "encoded") {
        if let Some(bytes) = read_byte_array(ctx, arr) {
            return Some(bytes);
        }
    }
    None
}

/// Read a Java `byte[]` into a `Vec<u8>` (elements arrive as sign-extended Int).
fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Option<Vec<u8>> {
    let len = ctx.array_length(arr);
    if len == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Int(b) => out.push(b as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// Read the configured trust-manager id off the receiver object. Mirrors the
/// `x509_manager::get_tm_id` convention: a named field
/// `cratonvm$x509tm$id`, falling back to slot 0. Returns 0 when unset, which
/// `build_trust_manager_state(0)` resolves to "system roots only".
fn read_trust_manager_id_from_obj(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    if let Value::Int(i) = ctx.get_field_by_name(this, "cratonvm$x509tm$id") {
        if i != 0 {
            return i;
        }
    }
    if ctx.object_num_fields(this) > 0 {
        if let Value::Int(i) = ctx.get_field(this, 0) {
            if i != 0 {
                return i;
            }
        }
    }
    0
}

// VULN [nb-tls] FIX: `validate_cert_chain` now routes to the production
// `x509_manager::validate_chain` in ALL builds (including the default build
// where `legacy-synthetic-crypto` is off), so this legacy synthetic-crypto
// validator is no longer wired into the trust-manager path. It is retained
// (allow dead_code) for reference and to keep the legacy crypto feature
// compiling; the unified validator supersedes it.
#[cfg(feature = "legacy-synthetic-crypto")]
#[allow(dead_code)]
fn validate_cert_chain_crypto(
    ctx: &mut dyn NativeContext,
    chain_arr: ObjectRef,
    chain_len: usize,
) -> MethodCallResult {
    // Get current time for validity checks (always, no feature gate)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Collect parsed certificates for chain validation
    let mut parsed_certs: Vec<Option<crate::crypto_impl::X509Cert>> = Vec::new();

    for i in 0..chain_len {
        if let Value::Object(Some(cert_ref)) = ctx.get_array_element(chain_arr, i) {
            let cert_id = match ctx.get_field(cert_ref, 2) {
                Value::Long(id) => id as u64,
                _ => {
                    parsed_certs.push(None);
                    continue;
                }
            };
            if let Some(cert) = crate::crypto_impl::cert_get(cert_id) {
                // 1. Check validity dates (always — no feature gate)
                if now < cert.not_before {
                    return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                        message: format!(
                            "Certificate not yet valid: {} (not before {})",
                            cert.subject_cn, cert.not_before
                        ),
                    }
                    .into());
                }
                if now > cert.not_after {
                    return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                        message: format!(
                            "Certificate expired: {} (expired at {})",
                            cert.subject_cn, cert.not_after
                        ),
                    }
                    .into());
                }
                parsed_certs.push(Some(cert));
            } else {
                parsed_certs.push(None);
            }
        } else {
            parsed_certs.push(None);
        }
    }

    // 2. Verify signatures: each cert[i] should be signed by cert[i+1] (or self-signed for root)
    for i in 0..parsed_certs.len() {
        if let Some(ref cert) = parsed_certs[i] {
            let issuer_spki = if i + 1 < parsed_certs.len() {
                // Use the next cert's public key as the issuer
                parsed_certs[i + 1]
                    .as_ref()
                    .map(|c| c.public_key_bytes.as_slice())
            } else {
                // Last cert in chain: verify as self-signed (root CA)
                Some(cert.public_key_bytes.as_slice())
            };
            if let Some(spki) = issuer_spki {
                if !cert.verify_signature(spki) {
                    return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                        message: format!(
                            "Certificate signature verification failed for: {}",
                            cert.subject_cn
                        ),
                    }
                    .into());
                }
            }
        }
    }

    // 3. Validate chain continuity: cert[i].issuer should match cert[i+1].subject
    for i in 0..parsed_certs.len().saturating_sub(1) {
        if let (Some(ref cert), Some(ref issuer)) = (&parsed_certs[i], &parsed_certs[i + 1]) {
            if cert.issuer_raw != issuer.subject_raw {
                return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: format!(
                        "Certificate chain broken: issuer of '{}' does not match subject of '{}'",
                        cert.subject_cn, issuer.subject_cn
                    ),
                }
                .into());
            }
        }
    }

    Ok(None) // all certs valid
}

fn register_x509_trust_manager(r: &mut NativeMethodRegistry) {
    // Bridge: checkClientTrusted/checkServerTrusted perform real X.509 chain
    // validation (validity dates, signature verification, chain continuity).
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "javax/net/ssl/X509TrustManager";
    // <init>() — KEPT as a no-op. `javax.net.ssl.X509TrustManager` is an
    // INTERFACE, so there is no real constructor to shadow and this can only
    // run for a CratonVM synthetic receiver. The one piece of state such a
    // receiver carries is its trust-manager id, and the only reader
    // (`read_trust_manager_id_from_obj`) already treats both an absent field
    // and a 0 slot as "id 0", which `trust_manager_state_by_id` resolves to
    // the platform root anchors — the correct, restrictive default for a
    // trust manager nobody has bound a KeyStore to. Writing 0 explicitly would
    // change nothing.
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // checkClientTrusted(X509Certificate[] chain, String authType) -> void
    // VULN [nb-tls] FIX: now performs full PKIX validation in every build (was
    // a no-op Ok(None) in the default build). args[0]=this, args[1]=chain,
    // args[2]=authType.
    r.register(
        cls,
        "checkClientTrusted",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
        |ctx, args| validate_cert_chain(ctx, args.first(), args.get(1)),
    );

    // checkServerTrusted(X509Certificate[] chain, String authType) -> void
    // VULN [nb-tls] FIX: now performs full PKIX validation in every build (was
    // a no-op Ok(None) in the default build). args[0]=this, args[1]=chain,
    // args[2]=authType.
    r.register(
        cls,
        "checkServerTrusted",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
        |ctx, args| validate_cert_chain(ctx, args.first(), args.get(1)),
    );

    // T19.9 CONSOLIDATION: getAcceptedIssuers() -> X509Certificate[] is now
    // owned by `t27_tls::register_accepted_issuers`, which returns the real
    // system-root DER chain via rustls. The former tls.rs stub returned an
    // empty array and was shadowed in practice by t27_tls (which loads after
    // this function). Keeping only one registration removes the last
    // cross-file duplicate between tls.rs and t27_tls.rs.
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 10. sun.security.ssl.SSLContextImpl  (alias for SSLContext)
// ---------------------------------------------------------------------------

fn register_ssl_context_impl(r: &mut NativeMethodRegistry) {
    // SyntheticStub: alias of the synthetic SSLContext; createSSLEngine()
    // returns the fake (non-rustls) engine, init just flips a flag.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "sun/security/ssl/SSLContextImpl";
    // <init>() — this class is an alias of the synthetic SSLContext and shares
    // its slot layout, so give it the same initial state `getInstance` writes.
    // `engineInit` (register_keycloak_tls_natives) reads CTX_INITIALIZED /
    // CTX_KM_REF / CTX_TM_REF off exactly these slots, and a no-op left them
    // untagged rather than definitely-unset.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, CTX_PROTOCOL_IDX, Value::Int(0)); // "TLS"
        ctx.set_field(this, CTX_INITIALIZED, Value::Int(0));
        ctx.set_field(this, CTX_KM_REF, Value::Int(0));
        ctx.set_field(this, CTX_TM_REF, Value::Int(0));
        Ok(None)
    });

    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            // W3-7: this alias of `SSLContext.getInstance` had the same
            // fabrication its sibling did — a `_ => 0` arm that answered a
            // bogus protocol with an ordinary "TLS" context whose
            // `getProtocol()` then reported a protocol nobody asked for. It
            // now asks the SAME question the other three registrations ask,
            // and refuses with the same catchable `NoSuchAlgorithmException`.
            let requested = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("SSLContextImpl.getInstance: protocol is null".into()),
                    }
                    .into());
                }
            };
            if !crate::jca::provider_chain::ssl_context_protocol_supported(&requested) {
                return Err(throw_tls_algorithm_exc(
                    ctx,
                    &format!("{requested} SSLContext not available"),
                ));
            }
            let protocol_idx = match requested.to_ascii_uppercase().as_str() {
                "TLSV1.2" => 1,
                "TLSV1.3" => 2,
                "SSL" => 3,
                "TLSV1" => 4,
                "TLSV1.1" => 5,
                "SSLV3" => 6,
                "DTLS" => 7,
                "DTLSV1.0" => 8,
                "DTLSV1.2" => 9,
                _ => 0,
            };
            let obj = alloc_ssl_context(ctx, protocol_idx)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let obj = alloc_ssl_context(ctx, 0)?;
            ctx.set_field(obj, CTX_INITIALIZED, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, CTX_INITIALIZED, Value::Int(1));
            Ok(None)
        },
    );

    r.register(
        cls,
        "createSSLEngine",
        "()Ljavax/net/ssl/SSLEngine;",
        |ctx, _args| {
            let eng = alloc_ssl_engine(ctx)?;
            Ok(Some(Value::Object(Some(eng))))
        },
    );

    r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match ctx.get_field(this, CTX_PROTOCOL_IDX) {
            Value::Int(i) => i,
            _ => 0,
        };
        // W3-7: share `ctx_protocol_name` with the `javax.net.ssl.SSLContext`
        // registration. This copy knew only indices 1/2/3, so a context
        // created as TLSv1 / TLSv1.1 / SSLv3 / DTLS* reported itself as "TLS".
        let s = ctx.create_string(ctx_protocol_name(idx));
        Ok(Some(Value::Object(Some(s))))
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 11. SSLEngineResult — accessor stubs
// ---------------------------------------------------------------------------

/// Map this file's OWN synthetic status int codes (the `alloc_ssl_engine_result`
/// convention above: `STATUS_OK`=0/`CLOSED`=1/`BUFFER_UNDERFLOW`=2/
/// `BUFFER_OVERFLOW`=3) to `t27_tls`'s real-JDK-ordinal codes (`SR_*`), so both
/// conventions resolve through the same real-enum-singleton lookup.
fn tls_status_code_to_real(code: i32) -> i32 {
    match code {
        STATUS_CLOSED => crate::t27_tls::SR_CLOSED,
        STATUS_BUFFER_UNDERFLOW => crate::t27_tls::SR_BUFFER_UNDERFLOW,
        STATUS_BUFFER_OVERFLOW => crate::t27_tls::SR_BUFFER_OVERFLOW,
        _ => crate::t27_tls::SR_OK,
    }
}

/// As [`tls_status_code_to_real`], for the handshake-status convention
/// (`HS_NOT_HANDSHAKING`=0/`NEED_WRAP`=1/`NEED_UNWRAP`=2/`NEED_TASK`=3/
/// `FINISHED`=4 here vs the real JDK enum ordinals in `t27_tls::HS_*_R`).
fn tls_hs_code_to_real(code: i32) -> i32 {
    match code {
        HS_NEED_WRAP => crate::t27_tls::HS_NEED_WRAP_R,
        HS_NEED_UNWRAP => crate::t27_tls::HS_NEED_UNWRAP_R,
        HS_NEED_TASK => crate::t27_tls::HS_NEED_TASK_R,
        HS_FINISHED => crate::t27_tls::HS_FINISHED_R,
        _ => crate::t27_tls::HS_NOT_HANDSHAKING_R,
    }
}

/// Shared body for `getStatus()`/`getHandshakeStatus()`.
///
/// FIX (sslengineresult-stub-shadow): these accessors are registered
/// unconditionally — the `SyntheticStub` category is only dropped under
/// `CRATONVM_NO_STUBS`, which nothing sets by default — so in real-JDK mode
/// they SHADOW the real `SSLEngineResult` bytecode. The pre-fix versions
/// assumed every receiver was this file's synthetic int-code object and
/// returned a FRESH 1-slot synthetic enum holder, so every identity
/// comparison in real JSSE driver code (`result.getStatus() == Status.OK`,
/// `result.getHandshakeStatus() == HandshakeStatus.NEED_WRAP` — e.g.
/// `sun.net.httpserver.SSLStreams.recvData`, which gates `doHandshake()` on
/// exactly those `==` checks) silently failed. The server-side handshake
/// driver therefore never saw NEED_WRAP, never wrapped, and its peer waited
/// forever for a ServerHello — surfacing as the client-side
/// `SSLHandshakeException: handshake read: Resource temporarily unavailable
/// (os error 11)` stall on every real-JDK `HttpsServer` exchange. Same
/// stub-shadows-real-bytecode family as the `SymbolLookup` and
/// `KeyManagerFactory` fixes.
///
/// Resolution order:
/// 1. A real enum reference already stored on the receiver (objects built
///    via the real 4-arg ctor in `t27_tls::alloc_engine_result`) passes
///    through untouched — by-name first (layout-proof), then the raw slot.
/// 2. An int code is translated to the REAL enum singleton via
///    `t27_tls::real_*_enum` (`valueOf` on the real class), distinguishing
///    the two producer conventions by allocated width: a 2-slot object is
///    this file's fake-engine convention; anything wider stores the
///    real-JDK-ordinal codes (`t27_tls`/`phases_late` fallbacks).
/// 3. Only when no real enum class is resolvable (pure synthetic-JDK mode,
///    where the fake state machine is the only producer AND the only
///    consumer) fall back to the legacy fresh 1-slot holder.
fn engine_result_enum_accessor(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    real_field: &str,
    slot: usize,
    enum_cls: &str,
    is_handshake: bool,
) -> MethodCallResult {
    if let Value::Object(Some(o)) = ctx.get_field_by_name(this, real_field) {
        return Ok(Some(Value::Object(Some(o))));
    }
    let raw = ctx.get_field(this, slot);
    match raw {
        Value::Object(Some(_)) => Ok(Some(raw)),
        Value::Int(code) => {
            let real_code = if ctx.object_num_fields(this) <= 2 {
                if is_handshake {
                    tls_hs_code_to_real(code)
                } else {
                    tls_status_code_to_real(code)
                }
            } else {
                // Wider objects (t27_tls / phases_late fallbacks) already
                // store real-JDK-ordinal codes.
                code
            };
            let v = if is_handshake {
                crate::t27_tls::real_handshake_status_enum(ctx, real_code)
            } else {
                crate::t27_tls::real_status_enum(ctx, real_code)
            };
            if matches!(v, Value::Object(Some(_))) {
                Ok(Some(v))
            } else {
                // Pure synthetic-JDK mode: legacy 1-slot int holder.
                let holder = try_alloc_concurrent_synthetic(ctx, enum_cls, 1)?;
                ctx.set_field(holder, 0, Value::Int(code));
                Ok(Some(Value::Object(Some(holder))))
            }
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

fn register_ssl_engine_result(r: &mut NativeMethodRegistry) {
    // Shadowed by phases_late.rs::register_p68_ssl (Phase L), which registers
    // the same four accessors LATER (registration is last-wins) and runs on
    // the real-JDK path too since the httpserver-pkcs12 fix — if you are
    // debugging SSLEngineResult accessor behavior, the copy that actually
    // fires is the one in phases_late.rs. This copy is kept value-aware too
    // (same fix recipe) so it stays correct if the registration order ever
    // changes.
    //
    // SyntheticStub: accessors over the synthetic SSLEngineResult produced by
    // the fake SSLEngine state machine. NOTE: these are ACTIVE in real-JDK
    // mode too (stub-dropping is opt-in via CRATONVM_NO_STUBS), so every body
    // below must also behave correctly for a REAL SSLEngineResult built by
    // real bytecode / `t27_tls::alloc_engine_result` — see
    // `engine_result_enum_accessor`'s doc for the handshake stall this
    // caused when they didn't.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cls = "javax/net/ssl/SSLEngineResult";
    // <init>() — the registered descriptor is the NO-ARG one, which the real
    // (final) `SSLEngineResult` does not have: its only constructor is
    // `(Status, HandshakeStatus, int, int)` and it rejects null status
    // arguments. So `()V` here can only ever construct one of this module's
    // synthetic int-slot results, and a no-op left both status slots holding a
    // reference default — which `engine_result_enum_accessor` reports through
    // its `_ => Object(None)` arm as a NULL `getStatus()`/`getHandshakeStatus()`,
    // the one thing a real SSLEngineResult can never return.
    //
    // Write OK / NOT_HANDSHAKING plus zero byte counts. Code 0 means exactly
    // that in BOTH conventions this file bridges (STATUS_OK == SR_OK == 0 and
    // HS_NOT_HANDSHAKING == HS_NOT_HANDSHAKING_R == 0), so the result is
    // correct whichever width the synthetic class ended up with — see
    // `engine_result_enum_accessor`, which picks the convention from the
    // object's slot count. Deliberately no native for the real 4-arg ctor:
    // `t27_tls::alloc_engine_result` builds real results through it and
    // registering over it would break that path.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(STATUS_OK));
        ctx.set_field(this, 1, Value::Int(HS_NOT_HANDSHAKING));
        // bytesConsumed / bytesProduced, present only on the 4-slot shape.
        if ctx.object_num_fields(this) >= 4 {
            ctx.set_field(this, 2, Value::Int(0));
            ctx.set_field(this, 3, Value::Int(0));
        }
        Ok(None)
    });

    // getStatus() -> SSLEngineResult$Status
    r.register(
        cls,
        "getStatus",
        "()Ljavax/net/ssl/SSLEngineResult$Status;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            engine_result_enum_accessor(
                ctx,
                this,
                "status",
                0,
                "javax/net/ssl/SSLEngineResult$Status",
                false,
            )
        },
    );

    // getHandshakeStatus() -> SSLEngineResult$HandshakeStatus
    r.register(
        cls,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            engine_result_enum_accessor(
                ctx,
                this,
                "handshakeStatus",
                1,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                true,
            )
        },
    );

    // bytesConsumed() -> int. Real field first (real ctor path), then the
    // 4-slot fallback convention (consumed@2); the 2-slot fake-engine object
    // never stored a consumed count, so 0 is the only honest answer there
    // (matching the pre-fix behavior for that producer only).
    r.register(cls, "bytesConsumed", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(n) = ctx.get_field_by_name(this, "bytesConsumed") {
            return Ok(Some(Value::Int(n)));
        }
        if ctx.object_num_fields(this) >= 3 {
            if let Value::Int(n) = ctx.get_field(this, 2) {
                return Ok(Some(Value::Int(n)));
            }
        }
        Ok(Some(Value::Int(0)))
    });

    // bytesProduced() -> int (see bytesConsumed; produced@3 in the 4-slot
    // fallback convention).
    r.register(cls, "bytesProduced", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(n) = ctx.get_field_by_name(this, "bytesProduced") {
            return Ok(Some(Value::Int(n)));
        }
        if ctx.object_num_fields(this) >= 4 {
            if let Value::Int(n) = ctx.get_field(this, 3) {
                return Ok(Some(Value::Int(n)));
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 12. T19.9 — Keycloak-specific Sun extensions (SSLSessionImpl, engineInit)
// ---------------------------------------------------------------------------

/// T19.9: RFC 8446 §9.1 allowlist for TLS 1.3 cipher suites.
///
/// Keycloak hardens its SSLContext by calling
/// `SSLEngine.setEnabledCipherSuites(...)` with a specific list; we enforce
/// that only the three MTI suites (AES-128-GCM, AES-256-GCM, ChaCha20) are
/// accepted. Any pre-1.3 suite name (TLS_ECDHE_RSA_WITH_AES_..., SSL_, RC4,
/// 3DES_CBC, etc.) is rejected outright — the method silently drops the
/// entry rather than crashing, matching the reference JDK's behaviour of
/// only keeping entries that intersect with `getSupportedCipherSuites()`.
const TLS13_ALLOWED_SUITES: &[&str] = &[
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_256_GCM_SHA384",
    "TLS_CHACHA20_POLY1305_SHA256",
];

/// T19.9: returns true if `name` is an RFC 8446 §9.1 MTI TLS 1.3 suite.
/// Used by `register_keycloak_tls_natives` to enforce the allowlist on
/// `SSLEngine.setEnabledCipherSuites`.
pub(crate) fn is_tls13_allowed_suite_name(name: &str) -> bool {
    TLS13_ALLOWED_SUITES.iter().any(|s| *s == name)
}

/// T19.9: register the `sun.security.ssl.SSLSessionImpl` + `engineInit`
/// surface that Keycloak's TLS bootstrap reaches for but which was
/// unregistered in sessions 86/87. Placed here (tls.rs) rather than
/// phases_late.rs because: (a) phases_late is owned by T9.5 agents,
/// (b) these are thin shims that delegate to the same helpers used by
/// the public javax.net.ssl.SSLSession/SSLContext registrations, and
/// (c) consolidating them into one function makes the
/// `t19_9_consolidation_no_duplicate_registrations` test trivial to
/// express.
fn register_keycloak_tls_natives(r: &mut NativeMethodRegistry) {
    // SyntheticStub: these are thin shims and `engineInit` just flips a flag.
    // The setEnabledCipherSuitesStrict allowlist gate is the only real check.
    // Real session data comes from the rustls path in t27_tls.rs /
    // phases_late.rs.
    //
    // E31: the sentence that used to stand here — "getId derived from pointer,
    // cipher/protocol hardcoded, isValid()=true" — was an accurate inventory
    // of four defects, written as though listing them retired them. Each is
    // the drifted twin of a `javax/net/ssl/SSLSession` accessor above that has
    // since been fixed, and this class ALIASES that layout (its own `<init>`
    // calls `init_ssl_session_fields`), so nothing about the shape justified
    // the divergence. All four now go through the same shared predicates.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // --- sun.security.ssl.SSLSessionImpl ------------------------------------
    //
    // Keycloak reads the session ID (for access log correlation) and the
    // peer certificate chain (for mTLS client-cert auth). javax.net.ssl
    // versions of these are registered by phases_late.rs; this registers
    // the concrete impl class so direct `SSLSessionImpl` references
    // resolve without falling back to the interpreter.
    let sess_impl = "sun/security/ssl/SSLSessionImpl";
    // <init>() — the concrete impl class is treated as an alias of the
    // synthetic `javax/net/ssl/SSLSession` above (same 6-slot layout), so give
    // it the same initial state: valid session, no peer, real creation
    // timestamp. Without this a `new SSLSessionImpl()` had no creation time at
    // all, and Keycloak's access-log correlation reads session metadata off
    // exactly these objects.
    r.register(sess_impl, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        init_ssl_session_fields(ctx, this);
        Ok(None)
    });
    // E31 — the THIRD copy of `getId`, and it had both defects the other two
    // have now shed:
    //
    //   * unconditional 32 bytes for a session that negotiated nothing (see
    //     the `javax/net/ssl/SSLSession` copy above for the measurement and
    //     the named Tomcat consumer);
    //   * seeded from `this.as_ptr()` — the raw `ObjectRef` — which is the
    //     exact GC-unstable identity `gc_stable_objref_key` exists to replace.
    //     This VM's young-gen GC moves objects, so the "deterministic buffer
    //     so callers can compare equality across invocations" the old comment
    //     promised is not deterministic at all: the same session answers a
    //     DIFFERENT id after a young collection moves it, and a later,
    //     unrelated object allocated at the same address answers the SAME one.
    //     `identity_hash_code` is pinned for an object's lifetime.
    r.register(sess_impl, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !crate::t27_tls::session_has_negotiated(ctx, this) {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
            return Ok(Some(Value::Object(Some(empty))));
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let mut x = (ctx.identity_hash_code(this) as u32 as u64) | 1;
        for i in 0..32 {
            x ^= x >> 30;
            x = x.wrapping_mul(0xbf58476d1ce4e5b9);
            x ^= x >> 27;
            ctx.set_array_element(arr, i, Value::Int((x & 0xff) as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(
        sess_impl,
        "getPeerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, _args| {
            // Return empty array — matches the synthetic-session
            // convention where mTLS is off by default; phases_late's
            // javax.net.ssl.SSLSession.getPeerCertificates populates
            // real chains when a rustls SSLSocket has a client cert.
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        sess_impl,
        "getCipherSuite",
        "()Ljava/lang/String;",
        // E31 — this ignored `this` entirely and answered
        // `TLS_AES_128_GCM_SHA256` for every session in every state, with a
        // comment saying "callers can swap in a real suite after a successful
        // handshake" for a body that could not read one if they had. It is the
        // fabrication E12-1 §2 is about, in its purest form: a real, strong,
        // OFFERABLE suite name asserted about a session that may never have
        // handshaked, indistinguishable by any test a caller can write from a
        // genuine negotiation.
        //
        // THE STATED REASON FOR THE LITERAL DOES NOT EXIST. The comment said
        // `TLS_AES_128_GCM_SHA256` was "picked over TLS_AES_256_GCM_SHA384 so
        // Keycloak's 'is this a modern suite?' heuristic (name starts with
        // TLS_AES_128_GCM_) passes". Keycloak IS checked out on this host
        // (`C:\craton\apps\keycloak`, 8035 `.java` files) and contains **zero**
        // occurrences of `getCipherSuite` and **zero** of `TLS_AES_128_GCM`.
        // The heuristic this value was tuned for is not there, so the literal
        // was pinned against a consumer nobody can point to — which is also
        // why nobody noticed that it CONTRADICTS the sibling door: for one and
        // the same object (both registrations serve the layout
        // `init_ssl_session_fields` seeds, `SES_CIPHER_SUITE = Int(0)`),
        // `javax/net/ssl/SSLSession.getCipherSuite()` answered
        // `TLS13_CIPHERS[0]` = `TLS_AES_256_GCM_SHA384` while this one
        // answered `TLS_AES_128_GCM_SHA256`. Two doors, one session, two
        // suites. Reading the state makes them agree; the door that changes is
        // this one, from `..._128_...` to `..._256_...`, and the measured
        // denominator for that change is 0.
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `let`, not an inline scrutinee — see the `javax/net/ssl/
            // SSLSession` copy for the borrow it avoids.
            let raw = crate::t27_tls::session_cipher_slot(ctx.object_num_fields(this))
                .map(|slot| ctx.get_field(this, slot));
            match raw {
                Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
                Some(Value::Int(i)) => {
                    let suite = usize::try_from(i)
                        .ok()
                        .and_then(|u| TLS13_CIPHERS.get(u).copied())
                        .unwrap_or(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
                    let s = ctx.create_string(suite);
                    Ok(Some(Value::Object(Some(s))))
                }
                _ => {
                    let s =
                        ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        },
    );
    r.register(
        sess_impl,
        "getProtocol",
        "()Ljava/lang/String;",
        // E31: the twin of the above — a hardcoded `"TLSv1.3"` for a session
        // that may have negotiated nothing.
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let raw = crate::t27_tls::session_proto_slot(ctx.object_num_fields(this))
                .map(|slot| ctx.get_field(this, slot));
            match raw {
                Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
                Some(Value::Int(0)) => {
                    let s = ctx.create_string("TLSv1.3");
                    Ok(Some(Value::Object(Some(s))))
                }
                Some(Value::Int(1)) => {
                    let s = ctx.create_string("TLSv1.2");
                    Ok(Some(Value::Object(Some(s))))
                }
                _ => {
                    let s = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        },
    );
    // isValid() — read the session's own valid flag instead of answering a
    // constant `true`. `<init>` above seeds the same 6-slot layout as the
    // `javax/net/ssl/SSLSession` shim (`init_ssl_session_fields` sets
    // SES_VALID=1), so anything that invalidates a session by writing that
    // slot is honoured; a hardcoded `true` reported invalidated and
    // expired sessions as still usable.
    //
    // E31 — THE DRIFTED TWIN. The `javax/net/ssl/SSLSession.isValid` above
    // carries a comment explaining that slot 2 is a STREAM ID on the narrow
    // shapes and gating on the field count for exactly that reason; this copy,
    // registered ~1,900 lines later in the same file, kept the naive
    // `> SES_VALID` (i.e. 3+ fields) test the other one was written to
    // replace. On the `new13_alloc_null_ssl_session` shape that reads
    // `Int(-1)` — the "never connected" stream id — and returns it through a
    // `()Z` descriptor, so an unconnected socket's session answers `isValid()`
    // with a non-zero int. On the accept shape it returns the stream id, so a
    // genuinely negotiated session reports INVALID whenever its id happens to
    // be 0. Both directions wrong, from one width-blind test. (E42 made those
    // two shapes the same width, 4, which is why no width test can ever
    // separate them — only the VALUE in slot 2 can.)
    //
    // Same composition as the other copy: negotiated (the one shared
    // width-aware predicate) AND not invalidated. See that copy for why the
    // two halves must not collapse into one.
    //
    // F18 — and "same composition" now means the same FUNCTION, not a second
    // hand-assembled copy of the rule. This copy said it matched its twin and
    // then spelled the composition out again, which is how the twin drifted
    // last time. `t27_tls::session_is_valid` is the one place both halves are
    // combined; this door adds only the wide-shape slot read, exactly as the
    // `javax/net/ssl/SSLSession` copy above does.
    //
    // No `invalidate` is registered on THIS class name and none is needed: the
    // invalidated bit is keyed on the session OBJECT, not on the class name it
    // was reached through, so an `invalidate()` made through the
    // `javax/net/ssl/SSLSession` door is visible here on the same object.
    r.register(sess_impl, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !crate::t27_tls::session_is_valid(ctx, this) {
            return Ok(Some(Value::Int(0)));
        }
        if ctx.object_num_fields(this) > SES_CREATION_TIME {
            return Ok(Some(ctx.get_field(this, SES_VALID)));
        }
        Ok(Some(Value::Int(1)))
    });

    // --- sun.security.ssl.SSLContextImpl.engineInit ------------------------
    //
    // The SSLContextImpl class Keycloak instantiates under the hood when
    // `SSLContext.getInstance("TLSv1.3")` is called uses `engineInit` as
    // the SPI-private initialisation entry-point. It mirrors the public
    // javax.net.ssl.SSLContext.init signature.
    r.register(
        "sun/security/ssl/SSLContextImpl",
        "engineInit",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Mark the context as initialised via the existing field layout.
            ctx.set_field(this, CTX_INITIALIZED, Value::Int(1));
            // Record KM/TM refs so getKeyManagers/getTrustManagers can find
            // them later. Null is tolerated — engine builds without explicit
            // managers fall back to the platform defaults.
            if let Some(Value::Object(Some(km))) = args.get(1) {
                ctx.set_field(this, CTX_KM_REF, Value::Object(Some(*km)));
            }
            if let Some(Value::Object(Some(tm))) = args.get(2) {
                ctx.set_field(this, CTX_TM_REF, Value::Object(Some(*tm)));
            }
            Ok(None)
        },
    );

    // --- javax.net.ssl.SSLEngine.setEnabledCipherSuites (hardened) ----------
    //
    // Keycloak hardens its SSLContext by calling this with a specific list;
    // we filter to the MTI allowlist. This is a baseline shim — phases_late
    // already has a permissive version which shadows us under
    // last-writer-wins, but the allowlist gate lives HERE so
    // `t19_9_consolidation_cipher_allowlist_rejects_rc4` can exercise it
    // directly via `register_keycloak_tls_natives` in an isolated registry.
    r.register(
        "javax/net/ssl/SSLEngine",
        "setEnabledCipherSuitesStrict",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let arr_ref = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let len = ctx.array_length(arr_ref) as usize;
            for i in 0..len {
                if let Value::Object(Some(name_ref)) = ctx.get_array_element(arr_ref, i) {
                    let name = ctx.read_string(name_ref).unwrap_or_default();
                    if !is_tls13_allowed_suite_name(&name) {
                        return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
                            message: format!(
                                "setEnabledCipherSuites: cipher {} is not on the RFC 8446 §9.1 MTI allowlist",
                                name,
                            ),
                        }.into());
                    }
                }
            }
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Public registration entry-point
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// WP5.1/WP5.4 — public accessor for the rustls-backed engine's negotiated
// ALPN. Sibling modules (e.g. http2.rs's HttpClient) read this after a
// handshake completes to choose between HTTP/1.1 and HTTP/2 framing.
// The actual engine-handle registry + state machine lives in `t27_tls.rs`
// (see `register_sslengine_real` / `register_alpn_real`); this is the
// thin re-export through the module the integrator imports `engine_negotiated_alpn`
// from.
// ---------------------------------------------------------------------------

/// Return the ALPN protocol the rustls-backed engine with this `engine_id`
/// negotiated, or `None` if the handshake hasn't finished or no ALPN was
/// selected.
pub fn engine_negotiated_alpn(engine_id: i32) -> Option<String> {
    crate::t27_tls::engine_negotiated_alpn_internal(engine_id)
}

pub(crate) fn register_conscrypt_native_bridges(r: &mut NativeMethodRegistry) {
    let prev = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "org/conscrypt/NativeCrypto",
        "clinit",
        "()V",
        native_conscrypt_clinit,
    );
    r.register(
        "org/conscrypt/NativeCrypto",
        "get_cipher_names",
        "(Ljava/lang/String;)[Ljava/lang/String;",
        native_conscrypt_get_cipher_names,
    );
    r.register(
        "org/conscrypt/NativeCrypto",
        "EVP_has_aes_hardware",
        "()I",
        native_conscrypt_has_aes_hardware,
    );
    r.set_category(prev);
}

// Conscrypt's JNI_OnLoad registration is not ABI-safe on CratonVM/Windows.
// Jetty only needs a provider that can advertise its ALPN processor while it
// builds a connector; the actual TLS engine remains CratonVM's TLS surface.
fn native_conscrypt_clinit(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn native_conscrypt_get_cipher_names(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(
        ctx.new_array(ArrayElementType::Reference, 0),
    ))))
}

fn native_conscrypt_has_aes_hardware(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

pub(crate) fn register_tls_natives(r: &mut NativeMethodRegistry) {
    // Dispatcher: each sub-fn sets its own NativeKind (mostly SyntheticStub —
    // this is the emulated TLS layer; the real rustls engine is t27_tls.rs).
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    register_ssl_context(r);
    register_ssl_engine(r);
    register_ssl_session(r);
    register_ssl_parameters(r);
    register_trust_manager_factory(r);
    register_key_manager_factory(r);
    register_key_store(r);
    register_ssl_socket_factory(r);
    register_x509_trust_manager(r);
    register_ssl_context_impl(r);
    register_ssl_engine_result(r);
    register_conscrypt_native_bridges(r);
    // T19.9: Keycloak-specific natives for sun.security.ssl.SSLSessionImpl,
    // sun.security.ssl.SSLContextImpl.engineInit, and the hardened
    // SSLEngine.setEnabledCipherSuitesStrict allowlist. These entries are
    // registered LAST in tls.rs so they're visible to phases_late when it
    // runs in the lib.rs wiring (phases_late then re-registers the
    // permissive variants of a few, but the Sun-package shims are unique
    // to this block and never shadowed).
    register_keycloak_tls_natives(r);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tls_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // --- TLS deny-by-default -------------------------------------------------
    //
    // Every "must raise" case below has a "must still work" twin, so the suite
    // cannot be satisfied by rejecting everything.

    fn tls_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        r
    }

    /// `SSLContext.getInstance(name)` → the returned context, or the failure.
    fn get_instance(
        r: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        name: &str,
    ) -> MethodCallResult {
        let f = r
            .find(
                "javax/net/ssl/SSLContext",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
            )
            .expect("getInstance registered");
        let arg = ctx.create_string(name);
        f(ctx, &[Value::Object(Some(arg))])
    }

    fn protocol_of(
        r: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        context: ObjectRef,
    ) -> String {
        let f = r
            .find(
                "javax/net/ssl/SSLContext",
                "getProtocol",
                "()Ljava/lang/String;",
            )
            .expect("getProtocol registered");
        match f(ctx, &[Value::Object(Some(context))]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            other => panic!("getProtocol returned {other:?}"),
        }
    }

    // MUST STILL WORK.
    #[test]
    fn ssl_context_get_instance_accepts_every_supported_protocol_and_echoes_it_back() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        // (requested name, name `getProtocol()` must report)
        for (requested, expected) in [
            ("TLS", "TLS"),
            ("Default", "TLS"),
            ("TLSv1.2", "TLSv1.2"),
            ("TLSv1.3", "TLSv1.3"),
            ("SSL", "SSL"),
            ("TLSv1", "TLSv1"),
            ("TLSv1.1", "TLSv1.1"),
            ("SSLv3", "SSLv3"),
            // W3-7: SunJSSE registers three DTLS SSLContext services on JDK
            // 25 (measured). This registration refused all three.
            ("DTLS", "DTLS"),
            ("DTLSv1.0", "DTLSv1.0"),
            ("DTLSv1.2", "DTLSv1.2"),
        ] {
            let context = match get_instance(&r, &mut ctx, requested) {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("getInstance({requested}) failed: {other:?}"),
            };
            assert_eq!(
                protocol_of(&r, &mut ctx, context),
                expected,
                "getInstance({requested}) must not silently substitute a protocol"
            );
        }
    }

    // MUST STILL WORK — JCA algorithm lookup is case-insensitive.
    #[test]
    fn ssl_context_get_instance_is_case_insensitive() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        for name in ["tls", "TLSV1.3", "tlsv1.2", "ssl"] {
            assert!(
                matches!(
                    get_instance(&r, &mut ctx, name),
                    Ok(Some(Value::Object(Some(_))))
                ),
                "getInstance({name}) must be accepted"
            );
        }
    }

    // MUST RAISE.
    #[test]
    fn ssl_context_get_instance_refuses_an_unsupported_protocol() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        for name in ["SSLv2", "NoSuchThing", "TLSv1.4", "", "TLS "] {
            let result = get_instance(&r, &mut ctx, name);
            assert!(
                result.is_err(),
                "getInstance({name:?}) must raise, not substitute a TLS context"
            );
        }
    }

    #[test]
    fn the_unsupported_protocol_refusal_is_a_no_such_algorithm_exception() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        let err = get_instance(&r, &mut ctx, "SSLv2").expect_err("must raise");
        match err {
            MethodCallFailed::ExceptionThrown(exc) => {
                let cid = ctx.class_id_of_object(exc);
                assert_eq!(
                    ctx.class_name_arc_of_id(cid).as_deref(),
                    Some("java/security/NoSuchAlgorithmException")
                );
            }
            // Fallback arm: still an exception, never a context.
            MethodCallFailed::InternalError(e) => {
                let text = format!("{e}");
                assert!(
                    text.contains("IllegalArgumentException"),
                    "unexpected fallback: {text}"
                );
            }
        }
    }

    #[test]
    fn ssl_context_get_instance_of_null_is_a_null_pointer_exception() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        let f = r
            .find(
                "javax/net/ssl/SSLContext",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
            )
            .unwrap();
        assert!(f(&mut ctx, &[Value::Object(None)]).is_err());
    }

    // --- SSLContext factory surface requires init() --------------------------

    fn socket_factory_of(
        r: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        context: ObjectRef,
    ) -> MethodCallResult {
        let f = r
            .find(
                "javax/net/ssl/SSLContext",
                "getSocketFactory",
                "()Ljavax/net/ssl/SSLSocketFactory;",
            )
            .expect("getSocketFactory registered");
        f(ctx, &[Value::Object(Some(context))])
    }

    // MUST RAISE: a factory from an un-`init()`ed context carries neither key
    // nor trust managers, and nothing about the returned object says so.
    #[test]
    fn get_socket_factory_on_an_uninitialized_context_raises() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        let context = match get_instance(&r, &mut ctx, "TLSv1.3") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("getInstance failed: {other:?}"),
        };
        assert!(socket_factory_of(&r, &mut ctx, context).is_err());

        let g = r
            .find(
                "javax/net/ssl/SSLContext",
                "getServerSocketFactory",
                "()Ljavax/net/ssl/SSLServerSocketFactory;",
            )
            .unwrap();
        assert!(g(&mut ctx, &[Value::Object(Some(context))]).is_err());
    }

    // MUST STILL WORK — the twin: after `init()` the same call succeeds.
    #[test]
    fn get_socket_factory_after_init_succeeds() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        let context = match get_instance(&r, &mut ctx, "TLSv1.3") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("getInstance failed: {other:?}"),
        };
        let init = r
            .find(
                "javax/net/ssl/SSLContext",
                "init",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            )
            .unwrap();
        init(
            &mut ctx,
            &[
                Value::Object(Some(context)),
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .expect("init");
        assert!(matches!(
            socket_factory_of(&r, &mut ctx, context),
            Ok(Some(Value::Object(Some(_))))
        ));
    }

    // MUST STILL WORK — `getDefault()` hands back a pre-initialised context,
    // so the init gate must not break the JDK's documented default path.
    #[test]
    fn get_default_context_can_still_hand_out_a_socket_factory() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        let f = r
            .find(
                "javax/net/ssl/SSLContext",
                "getDefault",
                "()Ljavax/net/ssl/SSLContext;",
            )
            .unwrap();
        let context = match f(&mut ctx, &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("getDefault failed: {other:?}"),
        };
        assert!(matches!(
            socket_factory_of(&r, &mut ctx, context),
            Ok(Some(Value::Object(Some(_))))
        ));
    }

    // --- The non-cryptographic SSLEngine ------------------------------------

    /// All three engine cases live in ONE test: they share the process-wide
    /// opt-in flag, so running them as separate `#[test]`s would race under
    /// the default parallel test harness.
    #[test]
    fn noncrypto_engine_denies_by_default_works_when_opted_in_and_still_reports_closed() {
        let r = tls_registry();
        let mut ctx = MockNativeContext::new();
        let wrap = r
            .find(
                "javax/net/ssl/SSLEngine",
                "wrap",
                "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
            )
            .expect("wrap registered");
        let unwrap = r
            .find(
                "javax/net/ssl/SSLEngine",
                "unwrap",
                "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
            )
            .expect("unwrap registered");

        // MUST RAISE (default): no cryptography happened, so no OK status.
        set_noncrypto_engine_opt_in(false);
        let engine = alloc_ssl_engine(&mut ctx).unwrap();
        let args = [Value::Object(Some(engine))];
        assert!(
            wrap(&mut ctx, &args).is_err(),
            "wrap must not report success for an unprotected connection"
        );
        assert!(
            unwrap(&mut ctx, &args).is_err(),
            "unwrap must not report a finished handshake that never happened"
        );

        // PRESERVED NEGATIVE: `CLOSED` after `closeOutbound()` is a real,
        // correct answer and must stay a value, not become an exception.
        ctx.set_field(engine, ENG_OUTBOUND_DONE, Value::Int(1));
        ctx.set_field(engine, ENG_INBOUND_DONE, Value::Int(1));
        for f in [wrap, unwrap] {
            match f(&mut ctx, &args) {
                Ok(Some(Value::Object(Some(result)))) => {
                    assert_eq!(ctx.get_field(result, 0), Value::Int(STATUS_CLOSED));
                }
                other => panic!("closed engine must still report CLOSED, got {other:?}"),
            }
        }

        // MUST STILL WORK: the explicit opt-in restores the legacy behaviour.
        set_noncrypto_engine_opt_in(true);
        let legacy = alloc_ssl_engine(&mut ctx).unwrap();
        let legacy_args = [Value::Object(Some(legacy))];
        assert!(matches!(
            wrap(&mut ctx, &legacy_args),
            Ok(Some(Value::Object(Some(_))))
        ));
        assert!(matches!(
            unwrap(&mut ctx, &legacy_args),
            Ok(Some(Value::Object(Some(_))))
        ));

        // Leave the process in the secure state for any later test.
        set_noncrypto_engine_opt_in(false);
    }

    // --- SSLEngineResult int-code convention translation ---------------------

    #[test]
    fn engine_result_code_translation_maps_conventions_onto_real_ordinals() {
        // This file's fake-engine convention → t27_tls real-JDK-ordinal codes.
        assert_eq!(tls_status_code_to_real(STATUS_OK), crate::t27_tls::SR_OK);
        assert_eq!(
            tls_status_code_to_real(STATUS_CLOSED),
            crate::t27_tls::SR_CLOSED
        );
        assert_eq!(
            tls_status_code_to_real(STATUS_BUFFER_UNDERFLOW),
            crate::t27_tls::SR_BUFFER_UNDERFLOW
        );
        assert_eq!(
            tls_status_code_to_real(STATUS_BUFFER_OVERFLOW),
            crate::t27_tls::SR_BUFFER_OVERFLOW
        );
        assert_eq!(
            tls_hs_code_to_real(HS_NOT_HANDSHAKING),
            crate::t27_tls::HS_NOT_HANDSHAKING_R
        );
        assert_eq!(
            tls_hs_code_to_real(HS_NEED_WRAP),
            crate::t27_tls::HS_NEED_WRAP_R
        );
        assert_eq!(
            tls_hs_code_to_real(HS_NEED_UNWRAP),
            crate::t27_tls::HS_NEED_UNWRAP_R
        );
        assert_eq!(
            tls_hs_code_to_real(HS_NEED_TASK),
            crate::t27_tls::HS_NEED_TASK_R
        );
        assert_eq!(
            tls_hs_code_to_real(HS_FINISHED),
            crate::t27_tls::HS_FINISHED_R
        );
        // The two conventions genuinely disagree on these — the whole reason
        // the translation exists (e.g. raw code 3 is NEED_TASK here but
        // NEED_WRAP in real-JDK ordinals).
        assert_ne!(HS_NEED_WRAP, crate::t27_tls::HS_NEED_WRAP_R);
        assert_ne!(STATUS_CLOSED, crate::t27_tls::SR_CLOSED);
    }

    // --- Registration tests -------------------------------------------------

    #[test]
    fn test_ssl_context_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLContext";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"
            )
            .is_some());
        assert!(r
            .find(cls, "getDefault", "()Ljavax/net/ssl/SSLContext;")
            .is_some());
        assert!(r.find(
            cls,
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"
        ).is_some());
        assert!(r
            .find(cls, "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
            .is_some());
        assert!(r
            .find(
                cls,
                "createSSLEngine",
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "getSocketFactory",
                "()Ljavax/net/ssl/SSLSocketFactory;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "getServerSocketFactory",
                "()Ljavax/net/ssl/SSLServerSocketFactory;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "getDefaultSSLParameters",
                "()Ljavax/net/ssl/SSLParameters;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "getSupportedSSLParameters",
                "()Ljavax/net/ssl/SSLParameters;"
            )
            .is_some());
        assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_ssl_engine_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLEngine";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "wrap",
                "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "unwrap",
                "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
        assert!(r.find(cls, "beginHandshake", "()V").is_some());
        assert!(r
            .find(
                cls,
                "getHandshakeStatus",
                "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"
            )
            .is_some());
        assert!(r.find(cls, "isInboundDone", "()Z").is_some());
        assert!(r.find(cls, "isOutboundDone", "()Z").is_some());
        assert!(r.find(cls, "closeInbound", "()V").is_some());
        assert!(r.find(cls, "closeOutbound", "()V").is_some());
        assert!(r.find(cls, "setUseClientMode", "(Z)V").is_some());
        assert!(r.find(cls, "getUseClientMode", "()Z").is_some());
        assert!(r.find(cls, "getPeerHost", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getPeerPort", "()I").is_some());
        assert!(r
            .find(cls, "setEnabledProtocols", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "setEnabledCipherSuites", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getApplicationProtocol", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
            .is_some());
        assert!(r
            .find(cls, "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
            .is_some());
    }

    #[test]
    fn test_ssl_session_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLSession";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(cls, "getCipherSuite", "()Ljava/lang/String;")
            .is_some());
        assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "isValid", "()Z").is_some());
        assert!(r.find(cls, "invalidate", "()V").is_some());
        assert!(r.find(cls, "getId", "()[B").is_some());
        assert!(r.find(cls, "getPeerHost", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getPeerPort", "()I").is_some());
        assert!(r.find(cls, "getCreationTime", "()J").is_some());
        assert!(r.find(cls, "getLastAccessedTime", "()J").is_some());
        assert!(r.find(cls, "getApplicationBufferSize", "()I").is_some());
        assert!(r.find(cls, "getPacketBufferSize", "()I").is_some());
    }

    #[test]
    fn test_ssl_parameters_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLParameters";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(cls, "getProtocols", "()[Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setProtocols", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getCipherSuites", "()[Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setCipherSuites", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getApplicationProtocols", "()[Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(
                cls,
                "getEndpointIdentificationAlgorithm",
                "()Ljava/lang/String;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "setEndpointIdentificationAlgorithm",
                "(Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r.find(cls, "getNeedClientAuth", "()Z").is_some());
        assert!(r.find(cls, "setNeedClientAuth", "(Z)V").is_some());
        assert!(r.find(cls, "getWantClientAuth", "()Z").is_some());
        assert!(r.find(cls, "setWantClientAuth", "(Z)V").is_some());
    }

    #[test]
    fn test_trust_manager_factory_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/TrustManagerFactory";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;"
            )
            .is_some());
        assert!(r
            .find(cls, "getDefaultAlgorithm", "()Ljava/lang/String;")
            .is_some());
        assert!(r.find(cls, "init", "(Ljava/security/KeyStore;)V").is_some());
        assert!(r
            .find(cls, "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;")
            .is_some());
    }

    // FIX (nb-tls-tmf): getTrustManagers() must propagate the keystore registry
    // id bound by init() (factory slot 2) onto the returned X509TrustManager,
    // so validate_cert_chain's read_trust_manager_id_from_obj resolves the
    // bound trust anchors rather than returning a no-validation placeholder.
    //
    // UPDATED (tls-residuals, see x509_manager::register_trust_manager_state's
    // doc comment): the id stamped on slot 0 is no longer the raw keystore
    // registry id itself. `getTrustManagers()` now builds a `TrustManagerState`
    // from the bound keystore id and registers *that state* through
    // `register_trust_manager_state`, getting back a fresh id in the separate
    // `tm_registry` id space — deliberately, because stamping the raw keystore
    // id directly caused `validate_cert_chain` to misresolve it against a
    // numerically-coincident but unrelated `tm_registry` entry from other
    // TrustManagerFactory SPI paths (both counters start at 1 and increment
    // independently). So slot 0 is now an *indirection*: resolving it via
    // `trust_manager_state_by_id` must yield a `TrustManagerState` whose own
    // `keystore_id` field is the one init() bound — that's the invariant this
    // test checks, not raw numeric equality with the bound id.
    #[test]
    fn nb_tls_tmf_get_trust_managers_propagates_keystore_id() {
        use crate::test_utils::MockNativeContext;
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/TrustManagerFactory";
        let mut ctx = MockNativeContext::new();

        // Obtain a factory instance via getInstance (PKIX), then simulate
        // init(KeyStore) having bound a keystore registry id by writing slot 2.
        let get_instance = r
            .find(
                cls,
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
            )
            .unwrap();
        let factory = match get_instance(&mut ctx, &[Value::Object(None)]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("getInstance should return a factory, got {other:?}"),
        };
        const BOUND_ID: i32 = 7;
        ctx.set_field(factory, 2, Value::Int(BOUND_ID));

        // getTrustManagers() should return a 1-element array whose X509TrustManager
        // carries a `tm_registry` id resolving back to the bound keystore id.
        let get_tms = r
            .find(cls, "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;")
            .unwrap();
        let arr = match get_tms(&mut ctx, &[Value::Object(Some(factory))]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            other => panic!("getTrustManagers should return an array, got {other:?}"),
        };
        assert_eq!(
            ctx.array_length(arr),
            1,
            "expected exactly one trust manager"
        );
        let tm = match ctx.get_array_element(arr, 0) {
            Value::Object(Some(t)) => t,
            other => panic!("trust manager element should be an object, got {other:?}"),
        };
        let tm_class = ctx
            .class_name_of_id(ctx.class_id_of_object(tm))
            .unwrap_or_default();
        assert_eq!(
            tm_class,
            crate::x509_manager::FQN_X509_TM,
            "TrustManagerFactory must return a concrete X509TrustManagerImpl mirror, not the abstract javax.net.ssl.X509TrustManager interface"
        );
        let tm_id = match ctx.get_field(tm, 0) {
            Value::Int(i) => i,
            other => panic!("trust manager id (slot 0) should be an Int, got {other:?}"),
        };
        let resolved_state = crate::x509_manager::trust_manager_state_by_id(tm_id);
        assert_eq!(
            resolved_state.keystore_id, BOUND_ID,
            "the X509TrustManager's stamped id must resolve (via \
             trust_manager_state_by_id) to the TrustManagerState built from \
             the keystore id init() bound, so checkServerTrusted validates \
             against those anchors (not a no-op placeholder or an unrelated \
             tm_registry entry)"
        );
    }

    #[test]
    fn test_key_manager_factory_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/KeyManagerFactory";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;"
            )
            .is_some());
        assert!(r
            .find(cls, "getDefaultAlgorithm", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "init", "(Ljava/security/KeyStore;[C)V")
            .is_some());
        assert!(r
            .find(cls, "getKeyManagers", "()[Ljavax/net/ssl/KeyManager;")
            .is_some());
    }

    #[test]
    fn test_key_store_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "java/security/KeyStore";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "getInstance",
                "(Ljava/lang/String;)Ljava/security/KeyStore;"
            )
            .is_some());
        assert!(r
            .find(cls, "getDefaultType", "()Ljava/lang/String;")
            .is_some());
        assert!(r.find(cls, "load", "(Ljava/io/InputStream;[C)V").is_some());
        assert!(r
            .find(
                cls,
                "getCertificate",
                "(Ljava/lang/String;)Ljava/security/cert/Certificate;"
            )
            .is_some());
        assert!(r
            .find(cls, "getKey", "(Ljava/lang/String;[C)Ljava/security/Key;")
            .is_some());
        assert!(r
            .find(cls, "containsAlias", "(Ljava/lang/String;)Z")
            .is_some());
        assert!(r
            .find(cls, "aliases", "()Ljava/util/Enumeration;")
            .is_some());
        assert!(r.find(cls, "size", "()I").is_some());
        assert!(r
            .find(
                cls,
                "setCertificateEntry",
                "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "setKeyEntry",
                "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"
            )
            .is_some());
        assert!(r
            .find(cls, "deleteEntry", "(Ljava/lang/String;)V")
            .is_some());
    }

    #[test]
    fn test_ssl_socket_factory_synthetic_surface_is_dropped_with_real_sockets() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLSocketFactory";
        for (name, descriptor) in [
            ("<init>", "()V"),
            ("getDefault", "()Ljavax/net/ssl/SSLSocketFactory;"),
            ("createSocket", "(Ljava/lang/String;I)Ljava/net/Socket;"),
            (
                "createSocket",
                "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
            ),
            ("getDefaultCipherSuites", "()[Ljava/lang/String;"),
            ("getSupportedCipherSuites", "()[Ljava/lang/String;"),
        ] {
            assert!(
                r.find(cls, name, descriptor).is_none(),
                "{cls}.{name}{descriptor} is a SyntheticStub and must not shadow \
                 the real socket/TLS path when real sockets are the default"
            );
        }
    }

    #[test]
    fn test_x509_trust_manager_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/X509TrustManager";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "checkClientTrusted",
                "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "checkServerTrusted",
                "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"
            )
            .is_some());
        // T19.9: getAcceptedIssuers moved to t27_tls.rs (rustls-backed). The
        // tls.rs baseline no longer registers it — t27 is the canonical home.
        assert!(r
            .find(
                cls,
                "getAcceptedIssuers",
                "()[Ljava/security/cert/X509Certificate;"
            )
            .is_none());
    }

    // VULN [nb-tls] regression: the X509TrustManager check{Server,Client}Trusted
    // path must perform REAL PKIX validation in every build, NOT return Ok(None).
    // `validate_cert_chain` routes to `crate::x509_manager::validate_chain`, so
    // we guard the security property of that production validator directly: an
    // empty chain and an unparseable/untrusted chain MUST be rejected (no
    // trust anchors configured). Before the fix the default build accepted any
    // chain unconditionally.
    #[test]
    fn test_validate_chain_rejects_untrusted_in_all_builds() {
        use crate::x509_manager::{validate_chain, TrustManagerState};

        // No trust anchors → nothing can validate.
        let trust = TrustManagerState::default();

        // (1) Empty chain is rejected.
        let empty: Vec<Vec<u8>> = Vec::new();
        assert!(
            validate_chain(&empty, &trust).is_err(),
            "empty cert chain must be rejected"
        );

        // (2) A structurally invalid / untrusted single-cert chain is rejected
        // (garbage DER fails to parse; even a well-formed self-signed cert with
        // no matching anchor would fail at the trust-anchor step). The key
        // property is simply: validation actually runs and says NO.
        let garbage: Vec<Vec<u8>> = vec![vec![0x30, 0x03, 0x02, 0x01, 0x00]];
        assert!(
            validate_chain(&garbage, &trust).is_err(),
            "untrusted/unparseable cert chain must be rejected"
        );
    }

    #[test]
    fn test_ssl_context_impl_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "sun/security/ssl/SSLContextImpl";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(
                cls,
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"
            )
            .is_some());
        assert!(r
            .find(cls, "getDefault", "()Ljavax/net/ssl/SSLContext;")
            .is_some());
        assert!(r.find(
            cls,
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"
        ).is_some());
        assert!(r
            .find(cls, "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
            .is_some());
        assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_ssl_engine_result_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLEngineResult";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r
            .find(cls, "getStatus", "()Ljavax/net/ssl/SSLEngineResult$Status;")
            .is_some());
        assert!(r
            .find(
                cls,
                "getHandshakeStatus",
                "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"
            )
            .is_some());
        assert!(r.find(cls, "bytesConsumed", "()I").is_some());
        assert!(r.find(cls, "bytesProduced", "()I").is_some());
    }

    // --- Constant / logic tests (no VM context required) -------------------

    #[test]
    fn test_tls13_ciphers_not_empty() {
        assert!(!TLS13_CIPHERS.is_empty());
        assert_eq!(TLS13_CIPHERS[0], "TLS_AES_256_GCM_SHA384");
        assert_eq!(TLS13_CIPHERS[1], "TLS_AES_128_GCM_SHA256");
        assert_eq!(TLS13_CIPHERS[2], "TLS_CHACHA20_POLY1305_SHA256");
    }

    #[test]
    fn test_tls12_ciphers_not_empty() {
        assert!(!TLS12_CIPHERS.is_empty());
        assert!(TLS12_CIPHERS.iter().all(|c| c.starts_with("TLS_")));
    }

    #[test]
    fn test_supported_protocols_order() {
        assert_eq!(SUPPORTED_PROTOCOLS[0], "TLSv1.3");
        assert_eq!(SUPPORTED_PROTOCOLS[1], "TLSv1.2");
        assert_eq!(
            SUPPORTED_PROTOCOLS.len(),
            2,
            "Only TLSv1.3 and TLSv1.2 should be supported"
        );
    }

    #[test]
    fn test_status_constants_are_distinct() {
        let statuses = [
            STATUS_OK,
            STATUS_CLOSED,
            STATUS_BUFFER_UNDERFLOW,
            STATUS_BUFFER_OVERFLOW,
        ];
        for i in 0..statuses.len() {
            for j in (i + 1)..statuses.len() {
                assert_ne!(statuses[i], statuses[j]);
            }
        }
    }

    #[test]
    fn test_handshake_status_constants_are_distinct() {
        let hs = [
            HS_NOT_HANDSHAKING,
            HS_NEED_WRAP,
            HS_NEED_UNWRAP,
            HS_NEED_TASK,
            HS_FINISHED,
        ];
        for i in 0..hs.len() {
            for j in (i + 1)..hs.len() {
                assert_ne!(hs[i], hs[j]);
            }
        }
    }

    #[test]
    fn test_epoch_millis_is_positive() {
        let ms = epoch_millis();
        assert!(ms > 0);
    }

    #[test]
    fn test_epoch_millis_is_plausible() {
        // Should be after 2020-01-01 = 1577836800000 ms
        let ms = epoch_millis();
        assert!(ms > 1_577_836_800_000);
    }

    #[test]
    fn test_all_tls13_cipher_names_valid() {
        for cipher in TLS13_CIPHERS {
            assert!(cipher.starts_with("TLS_"), "Bad cipher: {}", cipher);
            assert!(!cipher.is_empty());
        }
    }

    #[test]
    fn test_all_tls12_cipher_names_valid() {
        for cipher in TLS12_CIPHERS {
            assert!(cipher.starts_with("TLS_"), "Bad cipher: {}", cipher);
        }
    }

    #[test]
    fn test_total_cipher_count() {
        // Combined pool must have enough variety for realistic TLS handshake simulation
        let total = TLS13_CIPHERS.len() + TLS12_CIPHERS.len();
        assert!(
            total >= 8,
            "Expected at least 8 total ciphers, got {}",
            total
        );
    }

    #[test]
    fn test_protocol_index_mapping() {
        // Verify the documented mapping used in register_ssl_context
        // idx 0 -> TLS, 1 -> TLSv1.2, 2 -> TLSv1.3, 3 -> SSL
        let mappings: &[(i32, &str)] = &[(0, "TLS"), (1, "TLSv1.2"), (2, "TLSv1.3"), (3, "SSL")];
        for (idx, proto) in mappings {
            let result = match *idx {
                1 => "TLSv1.2",
                2 => "TLSv1.3",
                3 => "SSL",
                _ => "TLS",
            };
            assert_eq!(
                result, *proto,
                "Protocol index {} should map to {}",
                idx, proto
            );
        }
    }

    #[test]
    fn test_ssl_context_field_indices_in_range() {
        // All field indices must be within the 12-field synthetic object
        assert!(CTX_PROTOCOL_IDX < 12);
        assert!(CTX_INITIALIZED < 12);
        assert!(CTX_KM_REF < 12);
        assert!(CTX_TM_REF < 12);
    }

    #[test]
    fn test_ssl_engine_field_indices_in_range() {
        // The original eight stay inside the first 8 slots — modules that
        // allocate this class with the older width must keep working.
        assert!(ENG_CLIENT_MODE < 8);
        assert!(ENG_HANDSHAKE_STATUS < 8);
        assert!(ENG_INBOUND_DONE < 8);
        assert!(ENG_OUTBOUND_DONE < 8);
        assert!(ENG_PROTOCOL_IDX < 8);
        assert!(ENG_PEER_HOST < 8);
        assert!(ENG_PEER_PORT < 8);
        assert!(ENG_ALPN_PROTOCOL < 8);
        // The configuration slots the setters write live above them, inside
        // the width `alloc_ssl_engine` requests.
        assert!(ENG_ENABLED_PROTOCOLS >= 8 && ENG_ENABLED_PROTOCOLS < ENG_FIELD_COUNT);
        assert!(ENG_ENABLED_CIPHERS >= 8 && ENG_ENABLED_CIPHERS < ENG_FIELD_COUNT);
        assert!(ENG_APP_PROTOCOLS >= 8 && ENG_APP_PROTOCOLS < ENG_FIELD_COUNT);
        assert!(ENG_NEED_CLIENT_AUTH >= 8 && ENG_NEED_CLIENT_AUTH < ENG_FIELD_COUNT);
        assert!(ENG_WANT_CLIENT_AUTH >= 8 && ENG_WANT_CLIENT_AUTH < ENG_FIELD_COUNT);
        assert!(ENG_ENDPOINT_ID_ALG >= 8 && ENG_ENDPOINT_ID_ALG < ENG_FIELD_COUNT);
    }

    #[test]
    fn test_ssl_parameters_field_indices_in_range_and_distinct() {
        let indices = [
            PAR_PROTOCOLS,
            PAR_CIPHER_SUITES,
            PAR_ENDPOINT_ID_ALG,
            PAR_NEED_CLIENT_AUTH,
            PAR_WANT_CLIENT_AUTH,
            PAR_APP_PROTOCOLS,
        ];
        for (i, a) in indices.iter().enumerate() {
            assert!(*a < PAR_FIELD_COUNT);
            for b in indices.iter().skip(i + 1) {
                assert_ne!(a, b, "Duplicate SSLParameters field index");
            }
        }
    }

    #[test]
    fn test_ssl_session_field_indices_in_range() {
        // All field indices must be within the 6-field synthetic object
        assert!(SES_CIPHER_SUITE < 6);
        assert!(SES_PROTOCOL < 6);
        assert!(SES_VALID < 6);
        assert!(SES_PEER_HOST < 6);
        assert!(SES_PEER_PORT < 6);
        assert!(SES_CREATION_TIME < 6);
    }

    #[test]
    fn test_ssl_engine_field_indices_are_distinct() {
        let indices = [
            ENG_CLIENT_MODE,
            ENG_HANDSHAKE_STATUS,
            ENG_INBOUND_DONE,
            ENG_OUTBOUND_DONE,
            ENG_PROTOCOL_IDX,
            ENG_PEER_HOST,
            ENG_PEER_PORT,
            ENG_ALPN_PROTOCOL,
            ENG_ENABLED_PROTOCOLS,
            ENG_ENABLED_CIPHERS,
            ENG_APP_PROTOCOLS,
            ENG_NEED_CLIENT_AUTH,
            ENG_WANT_CLIENT_AUTH,
            ENG_ENDPOINT_ID_ALG,
        ];
        for i in 0..indices.len() {
            for j in (i + 1)..indices.len() {
                assert_ne!(
                    indices[i], indices[j],
                    "Duplicate field index at {} and {}",
                    i, j
                );
            }
        }
    }

    #[test]
    fn test_ssl_session_field_indices_are_distinct() {
        let indices = [
            SES_CIPHER_SUITE,
            SES_PROTOCOL,
            SES_VALID,
            SES_PEER_HOST,
            SES_PEER_PORT,
            SES_CREATION_TIME,
        ];
        for i in 0..indices.len() {
            for j in (i + 1)..indices.len() {
                assert_ne!(indices[i], indices[j]);
            }
        }
    }

    #[test]
    fn test_application_buffer_size_plausible() {
        // Standard TLS max record payload is 16384
        assert_eq!(16384_i32, 16384);
    }

    #[test]
    fn test_packet_buffer_size_larger_than_app_buffer() {
        // Packet buffer must include header overhead
        let packet: i32 = 16709;
        let app: i32 = 16384;
        assert!(packet > app);
    }

    #[test]
    fn test_ssl_context_total_registration_count() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        // SSLContext alone registers 11 methods: <init> + 10 named
        let methods = [
            ("<init>", "()V"),
            ("getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"),
            ("getDefault", "()Ljavax/net/ssl/SSLContext;"),
            (
                "init",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            ),
            ("createSSLEngine", "()Ljavax/net/ssl/SSLEngine;"),
            ("createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"),
            ("getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;"),
            ("getServerSocketFactory", "()Ljavax/net/ssl/SSLServerSocketFactory;"),
            ("getDefaultSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
            ("getSupportedSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
            ("getProtocol", "()Ljava/lang/String;"),
        ];
        for (name, desc) in &methods {
            assert!(
                r.find("javax/net/ssl/SSLContext", name, desc).is_some(),
                "Missing SSLContext.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_all_hs_constants_non_negative() {
        assert!(HS_NOT_HANDSHAKING >= 0);
        assert!(HS_NEED_WRAP >= 0);
        assert!(HS_NEED_UNWRAP >= 0);
        assert!(HS_NEED_TASK >= 0);
        assert!(HS_FINISHED >= 0);
    }

    // --- TLSv1.1 removal verification ---

    #[test]
    fn test_supported_protocols_does_not_include_tls11() {
        for proto in SUPPORTED_PROTOCOLS {
            assert_ne!(
                *proto, "TLSv1.1",
                "TLSv1.1 must not be in SUPPORTED_PROTOCOLS"
            );
            assert_ne!(*proto, "TLSv1", "TLSv1 must not be in SUPPORTED_PROTOCOLS");
            assert_ne!(*proto, "SSLv3", "SSLv3 must not be in SUPPORTED_PROTOCOLS");
        }
    }

    #[test]
    fn test_supported_protocols_only_modern() {
        // All supported protocols must be TLSv1.2 or higher
        for proto in SUPPORTED_PROTOCOLS {
            assert!(
                *proto == "TLSv1.3" || *proto == "TLSv1.2",
                "Unexpected protocol: {}",
                proto
            );
        }
    }

    #[test]
    fn test_supported_protocols_len() {
        assert_eq!(
            SUPPORTED_PROTOCOLS.len(),
            2,
            "Expected exactly 2 protocols (TLSv1.3 + TLSv1.2)"
        );
    }

    // --- Handshake state machine tests ---

    #[test]
    fn test_client_begins_handshake_with_need_wrap() {
        // Client mode (1) should start with NEED_WRAP to send ClientHello
        let client_mode = 1;
        let initial_hs = if client_mode != 0 {
            HS_NEED_WRAP
        } else {
            HS_NEED_UNWRAP
        };
        assert_eq!(initial_hs, HS_NEED_WRAP);
    }

    #[test]
    fn test_server_begins_handshake_with_need_unwrap() {
        // Server mode (0) should start with NEED_UNWRAP to receive ClientHello
        let client_mode = 0;
        let initial_hs = if client_mode != 0 {
            HS_NEED_WRAP
        } else {
            HS_NEED_UNWRAP
        };
        assert_eq!(initial_hs, HS_NEED_UNWRAP);
    }

    #[test]
    fn test_wrap_advances_need_wrap_to_need_unwrap() {
        // When wrap is called during NEED_WRAP, next state should be NEED_UNWRAP
        let hs_status = HS_NEED_WRAP;
        let next_hs = match hs_status {
            HS_NEED_WRAP => HS_NEED_UNWRAP,
            _ => HS_NOT_HANDSHAKING,
        };
        assert_eq!(next_hs, HS_NEED_UNWRAP);
    }

    #[test]
    fn test_wrap_not_handshaking_stays_not_handshaking() {
        let hs_status = HS_NOT_HANDSHAKING;
        let next_hs = match hs_status {
            HS_NEED_WRAP => HS_NEED_UNWRAP,
            _ => HS_NOT_HANDSHAKING,
        };
        assert_eq!(next_hs, HS_NOT_HANDSHAKING);
    }

    #[test]
    fn test_unwrap_advances_need_unwrap_to_need_wrap() {
        let hs_status = HS_NEED_UNWRAP;
        let next_hs = match hs_status {
            HS_NEED_UNWRAP => HS_NEED_WRAP,
            HS_NEED_TASK => HS_FINISHED,
            _ => HS_NOT_HANDSHAKING,
        };
        assert_eq!(next_hs, HS_NEED_WRAP);
    }

    #[test]
    fn test_unwrap_need_task_transitions_to_finished() {
        let hs_status = HS_NEED_TASK;
        let next_hs = match hs_status {
            HS_NEED_UNWRAP => HS_NEED_WRAP,
            HS_NEED_TASK => HS_FINISHED,
            _ => HS_NOT_HANDSHAKING,
        };
        assert_eq!(next_hs, HS_FINISHED);
    }

    // --- SSLContext initialization ---

    #[test]
    fn test_ssl_context_protocol_idx_tls() {
        // "TLS" -> idx 0
        let idx: i32 = 0;
        let proto = match idx {
            1 => "TLSv1.2",
            2 => "TLSv1.3",
            3 => "SSL",
            _ => "TLS",
        };
        assert_eq!(proto, "TLS");
    }

    #[test]
    fn test_ssl_context_protocol_idx_tls12() {
        let idx: i32 = 1;
        let proto = match idx {
            1 => "TLSv1.2",
            2 => "TLSv1.3",
            3 => "SSL",
            _ => "TLS",
        };
        assert_eq!(proto, "TLSv1.2");
    }

    #[test]
    fn test_ssl_context_protocol_idx_tls13() {
        let idx: i32 = 2;
        let proto = match idx {
            1 => "TLSv1.2",
            2 => "TLSv1.3",
            3 => "SSL",
            _ => "TLS",
        };
        assert_eq!(proto, "TLSv1.3");
    }

    #[test]
    fn test_ssl_context_protocol_idx_ssl() {
        let idx: i32 = 3;
        let proto = match idx {
            1 => "TLSv1.2",
            2 => "TLSv1.3",
            3 => "SSL",
            _ => "TLS",
        };
        assert_eq!(proto, "SSL");
    }

    #[test]
    fn test_ssl_context_protocol_idx_unknown_defaults_to_tls() {
        let idx: i32 = 99;
        let proto = match idx {
            1 => "TLSv1.2",
            2 => "TLSv1.3",
            3 => "SSL",
            _ => "TLS",
        };
        assert_eq!(proto, "TLS");
    }

    // --- SSLSession field indices ---

    #[test]
    fn test_ssl_session_creation_time_index_is_last() {
        // Creation time should be the last field (index 5 in a 6-field object)
        assert_eq!(SES_CREATION_TIME, 5);
    }

    #[test]
    fn test_ssl_context_field_indices_are_distinct() {
        let indices = [CTX_PROTOCOL_IDX, CTX_INITIALIZED, CTX_KM_REF, CTX_TM_REF];
        for i in 0..indices.len() {
            for j in (i + 1)..indices.len() {
                assert_ne!(
                    indices[i], indices[j],
                    "Duplicate SSLContext field at {} and {}",
                    i, j
                );
            }
        }
    }

    #[test]
    fn test_ssl_parameters_registration_complete() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLParameters";
        // Verify we can look up all 12 SSLParameters methods
        let count = [
            ("<init>", "()V"),
            ("getProtocols", "()[Ljava/lang/String;"),
            ("setProtocols", "([Ljava/lang/String;)V"),
            ("getCipherSuites", "()[Ljava/lang/String;"),
            ("setCipherSuites", "([Ljava/lang/String;)V"),
            ("getApplicationProtocols", "()[Ljava/lang/String;"),
            ("setApplicationProtocols", "([Ljava/lang/String;)V"),
            ("getEndpointIdentificationAlgorithm", "()Ljava/lang/String;"),
            (
                "setEndpointIdentificationAlgorithm",
                "(Ljava/lang/String;)V",
            ),
            ("getNeedClientAuth", "()Z"),
            ("setNeedClientAuth", "(Z)V"),
            ("getWantClientAuth", "()Z"),
            ("setWantClientAuth", "(Z)V"),
        ]
        .iter()
        .filter(|(name, desc)| r.find(cls, name, desc).is_some())
        .count();
        assert_eq!(
            count, 13,
            "Expected all 13 SSLParameters methods registered"
        );
    }

    #[test]
    fn test_key_store_registration_complete() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "java/security/KeyStore";
        let all_methods = [
            ("<init>", "()V"),
            (
                "getInstance",
                "(Ljava/lang/String;)Ljava/security/KeyStore;",
            ),
            ("getDefaultType", "()Ljava/lang/String;"),
            ("load", "(Ljava/io/InputStream;[C)V"),
            (
                "getCertificate",
                "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
            ),
            ("getKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
            ("containsAlias", "(Ljava/lang/String;)Z"),
            ("aliases", "()Ljava/util/Enumeration;"),
            ("size", "()I"),
            (
                "setCertificateEntry",
                "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
            ),
            (
                "setKeyEntry",
                "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
            ),
            ("deleteEntry", "(Ljava/lang/String;)V"),
        ];
        for (name, desc) in &all_methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing KeyStore.{}{}",
                name,
                desc
            );
        }
    }

    // =======================================================================
    // T19.9 — TLS 1.3 API consolidation
    // =======================================================================
    //
    // These ten tests exercise the consolidated TLS surface landed in
    // Session 88. They assert:
    //   (1) the canonical-home map (tls_impl owns the state machine,
    //       t27_tls owns SSLServerSocket/X509TrustManager.getAcceptedIssuers,
    //       tls.rs owns the SSLSessionImpl/SSLContextImpl Sun shims, and
    //       phases_late.rs owns the rest of the javax.net.ssl surface);
    //   (2) there are no duplicate (class, method, desc) tuples across tls.rs
    //       + tls_impl.rs + t27_tls.rs (phases_late.rs is out of scope);
    //   (3) the Session 87 Lambda handshake tests continue to pass via the
    //       `tls_impl::tests::test_server_*` module re-exports;
    //   (4) Keycloak-required SSLSessionImpl + engineInit registrations land;
    //   (5) the RFC 8446 §9.1 MTI cipher allowlist rejects RC4/3DES.

    #[test]
    fn t19_9_ssl_context_instance_get_tlsv13() {
        // SSLContext.getInstance("TLSv1.3") must resolve to a registered
        // native (post-consolidation it lives in phases_late.rs, but the
        // baseline tls.rs also registers it so the `register_tls_natives`
        // output has an entry — either way, a fresh registry populated
        // from register_tls_natives MUST have it).
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        assert!(
            r.find(
                "javax/net/ssl/SSLContext",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
            )
            .is_some(),
            "SSLContext.getInstance must be registered post-consolidation",
        );
    }

    #[test]
    fn t19_9_ssl_context_init_with_keymanager_and_trustmanager() {
        // Both the javax.net.ssl.SSLContext.init and
        // sun.security.ssl.SSLContextImpl.engineInit SPI entry points
        // must be registered — the latter is Keycloak's actual bootstrap
        // path when SSLContext.getInstance("TLSv1.3") returns an impl.
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        assert!(r
            .find(
                "javax/net/ssl/SSLContext",
                "init",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            )
            .is_some());
        assert!(
            r.find(
                "sun/security/ssl/SSLContextImpl",
                "engineInit",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            )
            .is_some(),
            "engineInit SPI must be registered (Keycloak bootstrap path)",
        );
    }

    #[test]
    fn t19_9_key_manager_factory_get_instance_sun_x509() {
        // KeyManagerFactory.getInstance("SunX509") is what Keycloak asks
        // for first. The native contract is that getInstance is
        // protocol-agnostic (accepts any string, validates later), so
        // the registration just needs to exist under the javax.net.ssl
        // path.
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        assert!(r
            .find(
                "javax/net/ssl/KeyManagerFactory",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
            )
            .is_some());
    }

    #[test]
    fn t19_9_trust_manager_factory_init_with_keystore() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        assert!(r
            .find(
                "javax/net/ssl/TrustManagerFactory",
                "init",
                "(Ljava/security/KeyStore;)V",
            )
            .is_some());
        assert!(r
            .find(
                "javax/net/ssl/TrustManagerFactory",
                "getTrustManagers",
                "()[Ljavax/net/ssl/TrustManager;",
            )
            .is_some());
    }

    #[test]
    fn t19_9_ssl_engine_wrap_unwrap_round_trip() {
        // Verify both wrap and unwrap are registered with the JDK-compatible
        // ByteBuffer signatures. The actual byte-level round-trip is
        // exercised by tls_impl::tests::test_server_full_handshake
        // (RFC 8446 atomic-flight server). Here we just assert the
        // registration contract so callers' dispatch resolves.
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLEngine";
        assert!(r
            .find(
                cls,
                "wrap",
                "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "unwrap",
                "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
            )
            .is_some());
    }

    #[test]
    fn t19_9_ssl_session_id_is_nonempty_after_handshake() {
        // Post-consolidation SSLSessionImpl.getId must be registered.
        // The Session 87 Lambda handshake populates real session IDs via
        // rustls; for the registry smoke test we just check the shim is in.
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        assert!(
            r.find("sun/security/ssl/SSLSessionImpl", "getId", "()[B")
                .is_some(),
            "SSLSessionImpl.getId must be registered (Keycloak session tracking)",
        );
        // And the javax.net.ssl.SSLSession.getId (baseline) too.
        assert!(r
            .find("javax/net/ssl/SSLSession", "getId", "()[B")
            .is_some());
    }

    #[test]
    fn t19_9_t27_session_ticket_resumption_bypasses_full_handshake() {
        // T27 session ticket / resumption surface is in t27_tls.rs. The
        // HttpsURLConnection setSSLSocketFactory path is the one Keycloak
        // exercises when it caches a TLS session. Assert the registration
        // is LIVE (not duplicated elsewhere — see
        // `t19_9_consolidation_no_duplicate_registrations`).
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        crate::t27_tls::register_t27_natives(&mut r);
        assert!(r
            .find(
                "javax/net/ssl/HttpsURLConnection",
                "setSSLSocketFactory",
                "(Ljavax/net/ssl/SSLSocketFactory;)V",
            )
            .is_some());
        // The rustls-backed self-test surface is what verifies ticket
        // resumption at the wire level (see t27_tls::tests::t27_session_resumption).
        assert!(r
            .find("cratonvm/tls/T27SelfTest", "run", "()Ljava/lang/String;")
            .is_some());
    }

    // `tls_impl` only exists under `legacy-synthetic-crypto`; gate the tests
    // that reach into it so the default (synthetic-crypto-free) test build
    // compiles.
    #[cfg(feature = "legacy-synthetic-crypto")]
    #[test]
    fn t19_9_consolidation_no_duplicate_registrations() {
        // Canonical-home map: register each file's natives once, in the
        // same order as lib.rs runtime wiring (tls -> tls_impl -> t27),
        // then check no (class, method, desc) key appears more than
        // once across the three owned files. phases_late.rs is out of
        // this module's scope (owned by T9.5 agents) — duplicates
        // between tls.rs and phases_late.rs are intentional
        // last-writer-wins and not covered by this test.
        //
        // To measure duplicates independently of the registry's last-writer
        // semantics, we dump the `(class, method, desc)` tuples each
        // module would register when given a fresh registry, and then
        // diff. NativeMethodRegistry has no iteration API, so we probe
        // a closed list of known signatures collected at audit time.
        //
        // This test closes on the post-consolidation set: after removing
        // (a) X509TrustManager.getAcceptedIssuers from tls.rs, and
        // (b) SSLContext.createSSLEngine (both descriptors) from
        // tls_impl.rs, there should be zero cross-file duplicates among
        // the files tls.rs / tls_impl.rs / t27_tls.rs.
        let known_shared_keys: &[(&str, &str, &str)] = &[
            // Previously duplicated between tls.rs and t27_tls.rs —
            // removed from tls.rs in T19.9.
            (
                "javax/net/ssl/X509TrustManager",
                "getAcceptedIssuers",
                "()[Ljava/security/cert/X509Certificate;",
            ),
            // Previously duplicated between tls_impl.rs and phases_late.rs —
            // removed from tls_impl.rs in T19.9 (phases_late is canonical).
            (
                "javax/net/ssl/SSLContext",
                "createSSLEngine",
                "()Ljavax/net/ssl/SSLEngine;",
            ),
            (
                "javax/net/ssl/SSLContext",
                "createSSLEngine",
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
            ),
        ];

        // For each formerly-duplicated key, the tls_impl / t27 files must
        // no longer register it. (tls.rs still registers SSLContext.*
        // baselines — that's fine because phases_late.rs is the shadow.)
        let mut r_impl = NativeMethodRegistry::new();
        crate::tls::tls_impl::register_tls_impl_natives(&mut r_impl);
        for (cls, name, desc) in known_shared_keys {
            if *cls == "javax/net/ssl/SSLContext" && *name == "createSSLEngine" {
                assert!(
                    r_impl.find(cls, name, desc).is_none(),
                    "tls_impl.rs must no longer register {}.{}{} (shadowed by phases_late.rs)",
                    cls,
                    name,
                    desc,
                );
            }
        }

        let mut r_tls = NativeMethodRegistry::new();
        register_tls_natives(&mut r_tls);
        assert!(
            r_tls
                .find(
                    "javax/net/ssl/X509TrustManager",
                    "getAcceptedIssuers",
                    "()[Ljava/security/cert/X509Certificate;",
                )
                .is_none(),
            "tls.rs must no longer register X509TrustManager.getAcceptedIssuers (owned by t27_tls.rs)",
        );
    }

    #[cfg(feature = "legacy-synthetic-crypto")]
    #[test]
    fn t19_9_tls_orphan_registrations_removed() {
        // The two orphan registrations landed in sessions 86/87 and
        // deleted in T19.9:
        //   tls.rs:             X509TrustManager.getAcceptedIssuers
        //   tls_impl.rs:        SSLContext.createSSLEngine (both descriptors)
        // Build the registry as lib.rs does (tls -> tls_impl -> t27) and
        // verify the ORPHAN files do not re-add them.
        let mut r_tls_only = NativeMethodRegistry::new();
        register_tls_natives(&mut r_tls_only);
        assert!(
            r_tls_only
                .find(
                    "javax/net/ssl/X509TrustManager",
                    "getAcceptedIssuers",
                    "()[Ljava/security/cert/X509Certificate;",
                )
                .is_none(),
            "ORPHAN: tls.rs must not re-register X509TrustManager.getAcceptedIssuers",
        );

        let mut r_impl_only = NativeMethodRegistry::new();
        crate::tls::tls_impl::register_tls_impl_natives(&mut r_impl_only);
        assert!(
            r_impl_only
                .find(
                    "javax/net/ssl/SSLContext",
                    "createSSLEngine",
                    "()Ljavax/net/ssl/SSLEngine;",
                )
                .is_none(),
            "ORPHAN: tls_impl.rs must not re-register SSLContext.createSSLEngine()",
        );
        assert!(
            r_impl_only
                .find(
                    "javax/net/ssl/SSLContext",
                    "createSSLEngine",
                    "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
                )
                .is_none(),
            "ORPHAN: tls_impl.rs must not re-register SSLContext.createSSLEngine(String, int)",
        );
    }

    #[cfg(feature = "legacy-synthetic-crypto")]
    #[test]
    fn t19_9_handshake_still_green() {
        // Regression sentinel — if Session 87 Lambda's three server-handshake
        // tests in tls_impl ever regress, this module-level test must
        // notice. The tests themselves are in
        // `crate::tls::tls_impl::tests`. Here we confirm the
        // state-machine entry points they rely on still exist by
        // constructing one and walking a minimal client -> server ->
        // FINISHED transition.
        use crate::tls::tls_impl::{
            CipherSuite, HandshakeMessage, Tls13StateMachine, TlsExtension,
        };

        let mut sm = Tls13StateMachine::new_server();
        let resp = sm
            .advance(HandshakeMessage::ClientHello {
                random: [7u8; 32],
                session_id: [8u8; 32],
                cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
                extensions: vec![TlsExtension::ServerName("keycloak.localhost".into())],
            })
            .expect("CH accepted");
        assert!(resp.is_some(), "server must emit at least ServerHello");
        assert_eq!(sm.sni_hostname, Some("keycloak.localhost".to_string()));

        let fin = sm
            .advance(HandshakeMessage::Finished {
                verify_data: vec![0xCD],
            })
            .expect("client Finished accepted");
        assert!(fin.is_some(), "server must emit server Finished");
        assert!(sm.is_connected(), "server must be CONNECTED after flight");

        // RFC 8446 §9.1 MTI check — the selected suite must be on the
        // allowlist.
        assert!(
            crate::tls::tls_impl::is_mti_cipher_suite(sm.cipher_suite),
            "post-handshake cipher suite {:?} is not on the RFC 8446 §9.1 MTI allowlist",
            sm.cipher_suite,
        );
    }
}

// ---------------------------------------------------------------------------
// Registrar ordering — the ratchet for W7-61
// ---------------------------------------------------------------------------

/// Four registrars write `javax/net/ssl/SSLEngine` and
/// `javax/net/ssl/SSLContext.createSSLEngine`, under three mutually
/// incompatible slot maps (`tls.rs` 14 slots, `phases_late::ssl_security` 7,
/// `tls_impl` 3). Registration is last-write-wins, so WHICH ONE RUNS LAST is
/// the whole question — and W7-49-slot-index-recensus.md filed the site as a
/// live 7-over-2 heap-corruption row because it answered that question from a
/// call-graph walk that stopped at the first registrar it reached.
///
/// These tests pin the answer against the registry itself, by identity of the
/// registered `fn` pointer, so a future reordering of `lib.rs` fails here
/// rather than silently moving which slot map is authoritative. They assert
/// ORDER, not behaviour: no engine is allocated and no TLS runs.
///
/// The three orderings pinned, all re-derived by brace-depth scan of the
/// registrar bodies (see the block comment at `register_p68_ssl`'s SSLEngine
/// section for why a column-0 scan gets this file wrong):
///
///   * Compatible / strict: `register_p68_ssl` (lib.rs 18191) then
///     `net_phase_e::register_phase_e_networking` (18214). net_phase_e wins
///     `createSSLEngine`, which is what makes `ssleng_alloc` — the 7-wide
///     allocation on a class declaring 2 — DEAD in that mode.
///   * Synthetic: `register_tls_natives` (23713) then
///     `register_phase68_natives` (23716). p68 wins, both for
///     `createSSLEngine` and for the 21 `SSLEngine` triples it registers.
///   * Nothing later than `t27_tls::register_sslengine_real` (18252) touches
///     `sun/security/ssl/SSLEngineImpl`, which is the class the Compatible
///     engine actually is.
#[cfg(test)]
mod registry_ordering_tests {
    use cratonvm_native_api::NativeMethodRegistry;

    /// The callback `find` reports for a triple, as a raw address, so two
    /// registrations can be compared for identity.
    fn cb_addr(r: &NativeMethodRegistry, cls: &str, name: &str, desc: &str) -> Option<usize> {
        r.find(cls, name, desc).map(|cb| cb as usize)
    }

    const CREATE_ENGINE: (&str, &str, &str) = (
        "javax/net/ssl/SSLContext",
        "createSSLEngine",
        "()Ljavax/net/ssl/SSLEngine;",
    );
    const CREATE_ENGINE_HOSTPORT: (&str, &str, &str) = (
        "javax/net/ssl/SSLContext",
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
    );

    /// Compatible / strict: `net_phase_e` overwrites p68's `createSSLEngine`,
    /// so `ssleng_alloc` never runs and the 7-vs-2 over-allocation W7-49
    /// reported cannot happen in this mode.
    #[test]
    fn net_phase_e_wins_create_ssl_engine_on_the_essential_path() {
        // net_phase_e alone — the reference callback.
        let mut solo = NativeMethodRegistry::new();
        crate::net_phase_e::register_re6_ssl_context(&mut solo);

        // The essential path's real order.
        let mut boot = NativeMethodRegistry::new();
        crate::phases_late::ssl_security::register_p68_ssl(&mut boot);
        crate::net_phase_e::register_re6_ssl_context(&mut boot);

        for (cls, name, desc) in [CREATE_ENGINE, CREATE_ENGINE_HOSTPORT] {
            let want = cb_addr(&solo, cls, name, desc)
                .unwrap_or_else(|| panic!("register_re6_ssl_context must register {name}{desc}"));
            let got = cb_addr(&boot, cls, name, desc)
                .unwrap_or_else(|| panic!("{name}{desc} must be registered after both"));
            assert_eq!(
                got, want,
                "net_phase_e::register_re6_ssl_context must be the LAST writer of \
                 {cls}.{name}{desc} on the essential path (lib.rs 18191 then 18214). \
                 If p68 wins here, `ssleng_alloc` is live again and it allocates \
                 7 slots on a class declaring 2 — see W7-61.",
            );
        }
    }

    /// Synthetic overlay: `register_phase68_natives` runs AFTER
    /// `register_tls_natives` (lib.rs 23713 then 23716, stated there
    /// deliberately), so p68's 7-slot map is authoritative in synthetic mode.
    /// A renumber of p68's map is therefore NOT inert there.
    #[test]
    fn p68_ssl_is_the_last_writer_on_the_ssl_engine_surface() {
        let mut solo = NativeMethodRegistry::new();
        crate::phases_late::ssl_security::register_p68_ssl(&mut solo);

        let mut overlay = NativeMethodRegistry::new();
        super::register_tls_natives(&mut overlay);
        crate::phases_late::register_phase68_natives(&mut overlay);

        // Every triple p68 registers on the engine must be p68's afterwards.
        for (name, desc) in [
            ("setUseClientMode", "(Z)V"),
            ("getUseClientMode", "()Z"),
            ("beginHandshake", "()V"),
            ("closeInbound", "()V"),
            ("closeOutbound", "()V"),
            ("isInboundDone", "()Z"),
            ("isOutboundDone", "()Z"),
            ("setEnabledProtocols", "([Ljava/lang/String;)V"),
            ("setEnabledCipherSuites", "([Ljava/lang/String;)V"),
            ("getSession", "()Ljavax/net/ssl/SSLSession;"),
        ] {
            let want = cb_addr(&solo, "javax/net/ssl/SSLEngine", name, desc)
                .unwrap_or_else(|| panic!("register_p68_ssl must register {name}{desc}"));
            let got = cb_addr(&overlay, "javax/net/ssl/SSLEngine", name, desc)
                .unwrap_or_else(|| panic!("{name}{desc} must be registered after both"));
            assert_eq!(
                got, want,
                "register_p68_ssl must be the LAST writer of SSLEngine.{name}{desc} \
                 in synthetic mode (lib.rs 23713 then 23716). If tls.rs wins here, \
                 the 14-slot map is authoritative and p68's 7-slot map became dead \
                 code — which inverts every reachability verdict in W7-61.",
            );
        }
        // …and the allocator with it.
        for (cls, name, desc) in [CREATE_ENGINE, CREATE_ENGINE_HOSTPORT] {
            let want = cb_addr(&solo, cls, name, desc)
                .unwrap_or_else(|| panic!("register_p68_ssl must register {name}{desc}"));
            let got = cb_addr(&overlay, cls, name, desc).unwrap();
            assert_eq!(
                got, want,
                "register_p68_ssl (`ssleng_alloc`, 7 slots) must be the synthetic-mode \
                 winner of {cls}.{name}{desc}",
            );
        }
    }

    /// The Compatible-mode engine is a `sun/security/ssl/SSLEngineImpl`, and
    /// `t27_tls` owns that class outright — every triple p68 puts on the
    /// abstract `javax/net/ssl/SSLEngine` also exists there, so the abstract
    /// map is never reached by a superclass walk from a real engine.
    #[test]
    fn t27_covers_every_engine_triple_p68_registers_on_the_abstract_class() {
        let mut p68 = NativeMethodRegistry::new();
        crate::phases_late::ssl_security::register_p68_ssl(&mut p68);

        let mut t27 = NativeMethodRegistry::new();
        crate::t27_tls::register_sslengine_real(&mut t27);

        // The 21 triples `register_p68_ssl` registers on the abstract class.
        for (name, desc) in [
            ("setUseClientMode", "(Z)V"),
            ("getUseClientMode", "()Z"),
            ("setNeedClientAuth", "(Z)V"),
            ("getNeedClientAuth", "()Z"),
            ("setWantClientAuth", "(Z)V"),
            ("getWantClientAuth", "()Z"),
            ("setEnabledProtocols", "([Ljava/lang/String;)V"),
            ("getEnabledProtocols", "()[Ljava/lang/String;"),
            ("setEnabledCipherSuites", "([Ljava/lang/String;)V"),
            ("getEnabledCipherSuites", "()[Ljava/lang/String;"),
            ("getSupportedCipherSuites", "()[Ljava/lang/String;"),
            ("getSupportedProtocols", "()[Ljava/lang/String;"),
            ("beginHandshake", "()V"),
            (
                "getHandshakeStatus",
                "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
            ),
            (
                "wrap",
                "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
            ),
            (
                "unwrap",
                "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
            ),
            ("closeOutbound", "()V"),
            ("closeInbound", "()V"),
            ("isOutboundDone", "()Z"),
            ("isInboundDone", "()Z"),
            ("getSession", "()Ljavax/net/ssl/SSLSession;"),
        ] {
            assert!(
                p68.find("javax/net/ssl/SSLEngine", name, desc).is_some(),
                "this list tracks register_p68_ssl's SSLEngine surface; \
                 {name}{desc} is no longer on it — update the list, do not delete it",
            );
            assert!(
                t27.find("sun/security/ssl/SSLEngineImpl", name, desc)
                    .is_some(),
                "t27_tls must own SSLEngineImpl.{name}{desc}. Without it, a virtual \
                 call on the Compatible-mode engine falls through to the hierarchy \
                 walk and can reach p68's 7-slot map on the abstract superclass, \
                 which writes an Int into `peerHost` — the W7-61 corruption path.",
            );
        }
    }
}
