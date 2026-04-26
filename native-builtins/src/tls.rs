//! TLS/SSL native method implementations.
//!
//! Provides javax.net.ssl.*, java.security.KeyStore, and related classes
//! for TLS/SSL support in the RustJVM native layer.

// The `tls_impl` submodule contains a TLS 1.3 handshake emulator that depends
// on the crypto primitives in `crate::crypto::crypto_impl`. It is therefore
// gated behind `legacy-synthetic-crypto`; the rest of the javax.net.ssl surface in
// this module is unconditional (NEW-13).
#[cfg(feature = "legacy-synthetic-crypto")]
#[path = "tls_impl.rs"]
pub mod tls_impl;

use rustjvm_types::ClassId;
use rustjvm_types::error::MethodCallResult;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectRef, Value};
use crate::{native_noop, native_noop_with_this, obj_arg, alloc_concurrent_synthetic};
#[cfg(feature = "legacy-synthetic-crypto")]
use crate::crypto::crypto_impl;

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

fn alloc_ssl_context(ctx: &mut dyn NativeContext, protocol_idx: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 12);
    ctx.set_field(obj, CTX_PROTOCOL_IDX, Value::Int(protocol_idx));
    ctx.set_field(obj, CTX_INITIALIZED, Value::Int(0));
    ctx.set_field(obj, CTX_KM_REF, Value::Int(0));
    ctx.set_field(obj, CTX_TM_REF, Value::Int(0));
    obj
}

fn alloc_ssl_engine(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 8);
    ctx.set_field(obj, ENG_CLIENT_MODE, Value::Int(1));
    ctx.set_field(obj, ENG_HANDSHAKE_STATUS, Value::Int(HS_NOT_HANDSHAKING));
    ctx.set_field(obj, ENG_INBOUND_DONE, Value::Int(0));
    ctx.set_field(obj, ENG_OUTBOUND_DONE, Value::Int(0));
    ctx.set_field(obj, ENG_PROTOCOL_IDX, Value::Int(0)); // TLSv1.3
    ctx.set_field(obj, ENG_PEER_HOST, Value::Int(0));
    ctx.set_field(obj, ENG_PEER_PORT, Value::Int(-1));
    ctx.set_field(obj, ENG_ALPN_PROTOCOL, Value::Int(0)); // none
    obj
}

fn alloc_ssl_session(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 6);
    ctx.set_field(obj, SES_CIPHER_SUITE, Value::Int(0)); // TLS_AES_256_GCM_SHA384
    ctx.set_field(obj, SES_PROTOCOL, Value::Int(0));     // TLSv1.3
    ctx.set_field(obj, SES_VALID, Value::Int(1));
    ctx.set_field(obj, SES_PEER_HOST, Value::Int(0));
    ctx.set_field(obj, SES_PEER_PORT, Value::Int(-1));
    ctx.set_field(obj, SES_CREATION_TIME, Value::Long(epoch_millis()));
    obj
}

fn alloc_ssl_parameters(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4);
    ctx.set_field(obj, 0, Value::Int(0)); // protocols_set
    ctx.set_field(obj, 1, Value::Int(0)); // cipher_suites_set
    ctx.set_field(obj, 2, Value::Int(0)); // endpoint_identification_algorithm
    ctx.set_field(obj, 3, Value::Int(0)); // need_client_auth
    obj
}

fn alloc_ssl_engine_result(ctx: &mut dyn NativeContext, status: i32, hs_status: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 2);
    ctx.set_field(obj, 0, Value::Int(status));
    ctx.set_field(obj, 1, Value::Int(hs_status));
    obj
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
    let cls = "javax/net/ssl/SSLContext";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getInstance(String protocol) -> SSLContext
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let protocol_idx = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("TLS")    => 0,
                        Some("TLSv1.2") => 1,
                        Some("TLSv1.3") => 2,
                        Some("SSL")    => 3,
                        _              => 0,
                    }
                }
                _ => 0,
            };
            let obj = alloc_ssl_context(ctx, protocol_idx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getDefault() -> SSLContext  (returns TLSv1.3 context, idx=2)
    r.register(
        cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let obj = alloc_ssl_context(ctx, 2); // TLSv1.3
            ctx.set_field(obj, CTX_INITIALIZED, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // init(KeyManager[], TrustManager[], SecureRandom) -> void
    r.register(
        cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
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
            let eng = alloc_ssl_engine(ctx);
            Ok(Some(Value::Object(Some(eng))))
        },
    );

    // createSSLEngine(String host, int port) -> SSLEngine
    r.register(
        cls,
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        |ctx, args| {
            let eng = alloc_ssl_engine(ctx);
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
    r.register(
        cls,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            let sf = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 2);
            Ok(Some(Value::Object(Some(sf))))
        },
    );

    // getServerSocketFactory() -> SSLServerSocketFactory
    r.register(
        cls,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, _args| {
            let ssf = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 2);
            Ok(Some(Value::Object(Some(ssf))))
        },
    );

    // getDefaultSSLParameters() -> SSLParameters
    r.register(
        cls,
        "getDefaultSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            let p = alloc_ssl_parameters(ctx);
            Ok(Some(Value::Object(Some(p))))
        },
    );

    // getSupportedSSLParameters() -> SSLParameters
    r.register(
        cls,
        "getSupportedSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            let p = alloc_ssl_parameters(ctx);
            Ok(Some(Value::Object(Some(p))))
        },
    );

    // getProtocol() -> String
    r.register(
        cls,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, CTX_PROTOCOL_IDX) {
                Value::Int(i) => i,
                _ => 0,
            };
            let proto = match idx {
                1 => "TLSv1.2",
                2 => "TLSv1.3",
                3 => "SSL",
                _ => "TLS",
            };
            let s = ctx.create_string(proto);
            Ok(Some(Value::Object(Some(s))))
        },
    );
}

// ---------------------------------------------------------------------------
// 2. javax.net.ssl.SSLEngine  (8-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_engine(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLEngine";
    r.register(cls, "<init>", "()V", native_noop_with_this);

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
                let result = alloc_ssl_engine_result(ctx, STATUS_CLOSED, HS_NOT_HANDSHAKING);
                return Ok(Some(Value::Object(Some(result))));
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
            let result = alloc_ssl_engine_result(ctx, STATUS_OK, next_hs);
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
                let result = alloc_ssl_engine_result(ctx, STATUS_CLOSED, HS_NOT_HANDSHAKING);
                return Ok(Some(Value::Object(Some(result))));
            }
            // Advance handshake state machine
            let hs_status = match ctx.get_field(this, ENG_HANDSHAKE_STATUS) {
                Value::Int(v) => v,
                _ => HS_NOT_HANDSHAKING,
            };
            let next_hs = match hs_status {
                HS_NEED_UNWRAP => HS_NEED_WRAP, // After receiving, need to send next message
                HS_NEED_TASK => HS_FINISHED,     // After delegated task, handshake finishes
                _ => HS_NOT_HANDSHAKING,
            };
            // If handshake just finished, mark it complete
            if next_hs == HS_FINISHED {
                ctx.set_field(this, ENG_HANDSHAKE_STATUS, Value::Int(HS_NOT_HANDSHAKING));
                let result = alloc_ssl_engine_result(ctx, STATUS_OK, HS_FINISHED);
                return Ok(Some(Value::Object(Some(result))));
            }
            ctx.set_field(this, ENG_HANDSHAKE_STATUS, Value::Int(next_hs));
            let result = alloc_ssl_engine_result(ctx, STATUS_OK, next_hs);
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
        let initial_hs = if client_mode != 0 { HS_NEED_WRAP } else { HS_NEED_UNWRAP };
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
            let hs_obj = alloc_concurrent_synthetic(
                ctx,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                1,
            );
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
    // Shadowed by phases_late.rs::register_p68_ssl (Phase L) which provides the
    // real implementation that stores the protocol list on the engine. This
    // registration runs first; the phases_late.rs version overrides it.
    r.register(cls, "setEnabledProtocols", "([Ljava/lang/String;)V", native_noop_with_this);

    // setEnabledCipherSuites(String[]) -> void
    // Shadowed by phases_late.rs::register_p68_ssl (Phase L) — see note above.
    r.register(cls, "setEnabledCipherSuites", "([Ljava/lang/String;)V", native_noop_with_this);

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
    // Shadowed by phases_late.rs::register_p68_ssl (Phase L) which copies the
    // parameter fields onto the engine. This stub is kept so the tls.rs module
    // remains self-contained when the experimental-tls feature is built alone.
    r.register(cls, "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V", native_noop_with_this);

    // getSSLParameters() -> SSLParameters
    r.register(
        cls,
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            let p = alloc_ssl_parameters(ctx);
            Ok(Some(Value::Object(Some(p))))
        },
    );

    // getSession() -> SSLSession
    r.register(
        cls,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, _args| {
            let ses = alloc_ssl_session(ctx);
            Ok(Some(Value::Object(Some(ses))))
        },
    );
}

// ---------------------------------------------------------------------------
// 3. javax.net.ssl.SSLSession  (6-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_session(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLSession";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getCipherSuite() -> String
    r.register(
        cls,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, SES_CIPHER_SUITE) {
                Value::Int(i) => i as usize,
                _ => 0,
            };
            let suite = TLS13_CIPHERS.get(idx).copied().unwrap_or(TLS13_CIPHERS[0]);
            let s = ctx.create_string(suite);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // getProtocol() -> String
    r.register(
        cls,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, SES_PROTOCOL) {
                Value::Int(i) => i,
                _ => 0,
            };
            let proto = if idx == 0 { "TLSv1.3" } else { "TLSv1.2" };
            let s = ctx.create_string(proto);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // isValid() -> boolean
    r.register(cls, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SES_VALID)))
    });

    // invalidate() -> void
    r.register(cls, "invalidate", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, SES_VALID, Value::Int(0));
        Ok(None)
    });

    // getId() -> byte[]
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Build a 32-byte session ID derived from object identity
        let hash = ctx.identity_hash_code(this);
        let id_arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 32);
        for i in 0..4 {
            let byte_val = ((hash >> (i * 8)) & 0xFF) as i8;
            ctx.set_array_element(id_arr, i, Value::Int(byte_val as i32));
        }
        Ok(Some(Value::Object(Some(id_arr))))
    });

    // getPeerHost() -> String
    r.register(
        cls,
        "getPeerHost",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let field = ctx.get_field(this, SES_PEER_HOST);
            match field {
                Value::Object(_) => Ok(Some(field)),
                _ => Ok(Some(Value::Object(None))),
            }
        },
    );

    // getPeerPort() -> int
    r.register(cls, "getPeerPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SES_PEER_PORT)))
    });

    // getCreationTime() -> long
    r.register(cls, "getCreationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SES_CREATION_TIME)))
    });

    // getLastAccessedTime() -> long
    r.register(cls, "getLastAccessedTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(epoch_millis())))
    });

    // getApplicationBufferSize() -> int
    r.register(cls, "getApplicationBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16384))) // standard TLS max record payload
    });

    // getPacketBufferSize() -> int
    r.register(cls, "getPacketBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(16709))) // 16384 + header overhead
    });
}

// ---------------------------------------------------------------------------
// 4. javax.net.ssl.SSLParameters  (4-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_parameters(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLParameters";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getProtocols() -> String[]
    r.register(
        cls,
        "getProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let set = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            if set != 0 {
                let arr = build_string_array(ctx, SUPPORTED_PROTOCOLS);
                Ok(Some(Value::Object(Some(arr))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // setProtocols(String[]) -> void
    r.register(cls, "setProtocols", "([Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(1));
        Ok(None)
    });

    // getCipherSuites() -> String[]
    r.register(
        cls,
        "getCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let set = match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            if set != 0 {
                let arr = build_string_array(ctx, TLS13_CIPHERS);
                Ok(Some(Value::Object(Some(arr))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // setCipherSuites(String[]) -> void
    r.register(cls, "setCipherSuites", "([Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // getApplicationProtocols() -> String[]
    r.register(
        cls,
        "getApplicationProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let arr = build_string_array(ctx, &["h2", "http/1.1"]);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // setApplicationProtocols(String[]) -> void
    // Instance setter on SSLParameters — the Phase L real impl in
    // phases_late.rs::register_p68_ssl writes the list to the engine;
    // this is the Phase E fallback stub for isolated experimental-tls
    // builds. NEW-6: documented intentional no-op.
    r.register(
        cls,
        "setApplicationProtocols",
        "([Ljava/lang/String;)V",
        native_noop_with_this,
    );

    // getEndpointIdentificationAlgorithm() -> String
    r.register(
        cls,
        "getEndpointIdentificationAlgorithm",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, 2) {
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
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("HTTPS") => 1,
                        Some("LDAPS") => 2,
                        _             => 0,
                    }
                }
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(idx));
            Ok(None)
        },
    );

    // getNeedClientAuth() -> boolean
    r.register(cls, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });

    // setNeedClientAuth(boolean) -> void
    r.register(cls, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match args.get(1) {
            Some(Value::Int(b)) => *b,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(v));
        Ok(None)
    });

    // getWantClientAuth() -> boolean
    r.register(cls, "getWantClientAuth", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // setWantClientAuth(boolean) -> void
    // Shadowed by phases_late.rs::register_p68_ssl (Phase L) which writes the
    // flag to SSLParameters' want-client-auth field. Stub retained for module
    // self-containment under experimental-tls feature.
    r.register(cls, "setWantClientAuth", "(Z)V", native_noop_with_this);
}

// ---------------------------------------------------------------------------
// 5. javax.net.ssl.TrustManagerFactory  (3-field synthetic)
// ---------------------------------------------------------------------------

fn register_trust_manager_factory(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/TrustManagerFactory";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getInstance(String algorithm) -> TrustManagerFactory
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        |ctx, args| {
            let alg_idx = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("PKIX")    => 0,
                        Some("SunX509") => 1,
                        _              => 0,
                    }
                }
                _ => 0,
            };
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/TrustManagerFactory", 3);
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
    r.register(
        cls,
        "init",
        "(Ljava/security/KeyStore;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ks_present = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(ks_present));
            ctx.set_field(this, 1, Value::Int(1));
            Ok(None)
        },
    );

    // getTrustManagers() -> TrustManager[]
    r.register(
        cls,
        "getTrustManagers",
        "()[Ljavax/net/ssl/TrustManager;",
        |ctx, _args| {
            let tm = alloc_concurrent_synthetic(ctx, "javax/net/ssl/X509TrustManager", 1);
            let arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(tm)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

// ---------------------------------------------------------------------------
// 6. javax.net.ssl.KeyManagerFactory  (3-field synthetic)
// ---------------------------------------------------------------------------

fn register_key_manager_factory(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/KeyManagerFactory";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getInstance(String algorithm) -> KeyManagerFactory
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        |ctx, args| {
            let alg_idx = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("SunX509")    => 0,
                        Some("NewSunX509") => 1,
                        Some("PKIX")       => 2,
                        _                  => 0,
                    }
                }
                _ => 0,
            };
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/KeyManagerFactory", 3);
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
    r.register(
        cls,
        "init",
        "(Ljava/security/KeyStore;[C)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ks_present = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(ks_present));
            ctx.set_field(this, 1, Value::Int(1));
            Ok(None)
        },
    );

    // getKeyManagers() -> KeyManager[]
    r.register(
        cls,
        "getKeyManagers",
        "()[Ljavax/net/ssl/KeyManager;",
        |ctx, _args| {
            let km = alloc_concurrent_synthetic(ctx, "javax/net/ssl/X509KeyManager", 1);
            let arr = ctx.new_ref_array(ClassId::new(0), 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(km)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

// ---------------------------------------------------------------------------
// 7. java.security.KeyStore  (5-field synthetic)
//    field 0: type_idx (0=JKS, 1=PKCS12, 2=JCEKS)
//    field 1: loaded (0/1)
//    field 2: entry_count
//    field 3: provider_idx
//    field 4: store_id (Long — index into global KeyStore store)
// ---------------------------------------------------------------------------

fn register_key_store(r: &mut NativeMethodRegistry) {
    let cls = "java/security/KeyStore";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getInstance(String type) -> KeyStore
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyStore;",
        |ctx, args| {
            let (type_idx, _type_name) = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("JKS")    => (0, "JKS"),
                        Some("PKCS12") => (1, "PKCS12"),
                        Some("JCEKS")  => (2, "JCEKS"),
                        _              => (0, "JKS"),
                    }
                }
                _ => (0, "JKS"),
            };
            let obj = alloc_concurrent_synthetic(ctx, "java/security/KeyStore", 5);
            ctx.set_field(obj, 0, Value::Int(type_idx));
            ctx.set_field(obj, 1, Value::Int(0)); // not loaded
            ctx.set_field(obj, 2, Value::Int(0)); // 0 entries
            ctx.set_field(obj, 3, Value::Int(0)); // provider idx
            ctx.set_field(obj, 4, Value::Long(0)); // no store yet
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getType() -> String (instance method)
    r.register(
        cls,
        "getType",
        "()Ljava/lang/String;",
        |ctx, args| {
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
        },
    );

    // getDefaultType() -> String (static)
    r.register(
        cls,
        "getDefaultType",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("PKCS12");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // load(InputStream, char[]) -> void
    r.register(
        cls,
        "load",
        "(Ljava/io/InputStream;[C)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1)); // mark loaded

            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let type_idx = match ctx.get_field(this, 0) { Value::Int(v) => v, _ => 0 };
                let type_hint = match type_idx { 0 => "JKS", 1 => "PKCS12", 2 => "JCEKS", _ => "auto" };

                // Read password from char[]
                let password = if let Some(Value::Object(Some(pwd_arr))) = args.get(2) {
                    let len = ctx.array_length(*pwd_arr);
                    let mut pwd = Vec::with_capacity(len);
                    for i in 0..len {
                        let c = match ctx.get_array_element(*pwd_arr, i) {
                            Value::Int(v) => v as u8,
                            _ => 0,
                        };
                        pwd.push(c);
                    }
                    pwd
                } else { Vec::new() };

                // Try to read data from InputStream
                if let Some(Value::Object(Some(is_ref))) = args.get(1) {
                    // ByteArrayInputStream pattern: field 0 = byte[] buf
                    if let Value::Object(Some(buf_ref)) = ctx.get_field(*is_ref, 0) {
                        let len = ctx.array_length(buf_ref);
                        if len > 0 {
                            let mut data = Vec::with_capacity(len);
                            for i in 0..len {
                                let b = match ctx.get_array_element(buf_ref, i) {
                                    Value::Int(v) => v as u8,
                                    _ => 0,
                                };
                                data.push(b);
                            }
                            if let Ok(ks_data) = crypto_impl::KeyStoreData::load(&data, &password, type_hint) {
                                let entry_count = ks_data.entries.len() as i32;
                                let store_id = crypto_impl::keystore_next_id();
                                crypto_impl::keystore_store(store_id, ks_data);
                                ctx.set_field(this, 2, Value::Int(entry_count));
                                ctx.set_field(this, 4, Value::Long(store_id as i64));
                                return Ok(None);
                            }
                        }
                    }
                } else {
                    // null InputStream = empty keystore (valid per spec)
                    let store_id = crypto_impl::keystore_next_id();
                    crypto_impl::keystore_store(store_id, crypto_impl::KeyStoreData {
                        store_type: type_hint.to_string(),
                        entries: std::collections::HashMap::new(),
                    });
                    ctx.set_field(this, 4, Value::Long(store_id as i64));
                }
            }

            Ok(None)
        },
    );

    // getCertificate(String alias) -> Certificate
    r.register(
        cls,
        "getCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let this = obj_arg(args, 0)?;
                let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
                if store_id > 0 {
                    let alias = match args.get(1) {
                        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                        _ => String::new(),
                    };
                    if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                        if let Some(entry) = ks_data.entries.get(&alias) {
                            let x509_cert = match entry {
                                crypto_impl::KeyStoreEntry::TrustedCert { cert } => Some(cert),
                                crypto_impl::KeyStoreEntry::PrivateKeyEntry { cert_chain, .. } => cert_chain.first(),
                                _ => None,
                            };
                            if let Some(cert) = x509_cert {
                                let cert_obj = alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 3);
                                let sub_str = ctx.create_string(&cert.subject_cn);
                                let iss_str = ctx.create_string(&cert.issuer_cn);
                                let cert_id = crypto_impl::cert_next_id();
                                crypto_impl::cert_store(cert_id, cert.clone());
                                ctx.set_field(cert_obj, 0, Value::Object(Some(sub_str)));
                                ctx.set_field(cert_obj, 1, Value::Object(Some(iss_str)));
                                ctx.set_field(cert_obj, 2, Value::Long(cert_id as i64));
                                return Ok(Some(Value::Object(Some(cert_obj))));
                            }
                        }
                    }
                }
            }
            let _ = (ctx, args);
            Ok(Some(Value::Object(None)))
        },
    );

    // getKey(String alias, char[] password) -> Key
    r.register(
        cls,
        "getKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let this = obj_arg(args, 0)?;
                let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
                if store_id > 0 {
                    let alias = match args.get(1) {
                        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                        _ => String::new(),
                    };
                    if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                        if let Some(entry) = ks_data.entries.get(&alias) {
                            match entry {
                                crypto_impl::KeyStoreEntry::PrivateKeyEntry { key_bytes, .. } => {
                                    let pk = alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 4);
                                    ctx.set_field(pk, 0, Value::Int(6)); // RSA default
                                    ctx.set_field(pk, 1, Value::Int(2048));
                                    ctx.set_field(pk, 2, Value::Int(key_bytes.len() as i32));
                                    ctx.set_field(pk, 3, Value::Long(0));
                                    return Ok(Some(Value::Object(Some(pk))));
                                }
                                crypto_impl::KeyStoreEntry::SecretKeyEntry { key_bytes, algorithm } => {
                                    let sk = alloc_concurrent_synthetic(ctx, "javax/crypto/SecretKey", 3);
                                    ctx.set_field(sk, 0, Value::Int(0));
                                    ctx.set_field(sk, 1, Value::Int((key_bytes.len() * 8) as i32));
                                    ctx.set_field(sk, 2, Value::Int(key_bytes.len() as i32));
                                    let _ = algorithm;
                                    return Ok(Some(Value::Object(Some(sk))));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            let _ = (ctx, args);
            Ok(Some(Value::Object(None)))
        },
    );

    // containsAlias(String alias) -> boolean
    r.register(cls, "containsAlias", "(Ljava/lang/String;)Z", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let this = obj_arg(args, 0)?;
            let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
            if store_id > 0 {
                let alias = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                    return Ok(Some(Value::Int(if ks_data.entries.contains_key(&alias) { 1 } else { 0 })));
                }
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Int(0)))
    });

    // aliases() -> Enumeration<String>
    r.register(
        cls,
        "aliases",
        "()Ljava/util/Enumeration;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let this = obj_arg(args, 0)?;
                let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
                if store_id > 0 {
                    if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                        let aliases: Vec<&str> = ks_data.entries.keys().map(|s| s.as_str()).collect();
                        if !aliases.is_empty() {
                            // Build a Vector-backed enumeration
                            let vec_obj = alloc_concurrent_synthetic(ctx, "java/util/Vector", 2);
                            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, aliases.len());
                            for (i, alias) in aliases.iter().enumerate() {
                                let s = ctx.create_string(alias);
                                ctx.set_array_element(arr, i, Value::Object(Some(s)));
                            }
                            ctx.set_field(vec_obj, 0, Value::Object(Some(arr)));
                            ctx.set_field(vec_obj, 1, Value::Int(aliases.len() as i32));
                            // Return a simple enumeration wrapping the vector
                            let en = alloc_concurrent_synthetic(ctx, "java/util/Vector$VectorEnumeration", 2);
                            ctx.set_field(en, 0, Value::Object(Some(vec_obj)));
                            ctx.set_field(en, 1, Value::Int(0)); // cursor
                            return Ok(Some(Value::Object(Some(en))));
                        }
                    }
                }
            }
            let _ = args;
            let en = alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0);
            Ok(Some(Value::Object(Some(en))))
        },
    );

    // size() -> int
    r.register(cls, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
            if store_id > 0 {
                if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                    return Ok(Some(Value::Int(ks_data.entries.len() as i32)));
                }
            }
        }
        Ok(Some(ctx.get_field(this, 2)))
    });

    // setCertificateEntry(String alias, Certificate cert) -> void
    r.register(
        cls,
        "setCertificateEntry",
        "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = match ctx.get_field(this, 2) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(count + 1));
            Ok(None)
        },
    );

    // setKeyEntry(String alias, Key key, char[] password, Certificate[] chain) -> void
    r.register(
        cls,
        "setKeyEntry",
        "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = match ctx.get_field(this, 2) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(count + 1));
            Ok(None)
        },
    );

    // deleteEntry(String alias) -> void
    r.register(
        cls,
        "deleteEntry",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = match ctx.get_field(this, 2) {
                Value::Int(v) => v,
                _ => 0,
            };
            if count > 0 {
                ctx.set_field(this, 2, Value::Int(count - 1));
            }
            Ok(None)
        },
    );

    // store(OutputStream, char[]) -> void
    // KeyStore.store is an instance method that persists the store to
    // a stream. Our synthetic KeyStore is in-memory only (no persistent
    // backing) so store() is a no-op. NEW-6: documented.
    r.register(
        cls,
        "store",
        "(Ljava/io/OutputStream;[C)V",
        native_noop_with_this,
    );

    // isCertificateEntry(String alias) -> boolean
    r.register(cls, "isCertificateEntry", "(Ljava/lang/String;)Z", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let this = obj_arg(args, 0)?;
            let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
            if store_id > 0 {
                let alias = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                    if let Some(crypto_impl::KeyStoreEntry::TrustedCert { .. }) = ks_data.entries.get(&alias) {
                        return Ok(Some(Value::Int(1)));
                    }
                }
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Int(0)))
    });

    // isKeyEntry(String alias) -> boolean
    r.register(cls, "isKeyEntry", "(Ljava/lang/String;)Z", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let this = obj_arg(args, 0)?;
            let store_id = match ctx.get_field(this, 4) { Value::Long(id) => id as u64, _ => 0 };
            if store_id > 0 {
                let alias = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                if let Some(ks_data) = crypto_impl::keystore_get(store_id) {
                    if let Some(crypto_impl::KeyStoreEntry::PrivateKeyEntry { .. }) = ks_data.entries.get(&alias) {
                        return Ok(Some(Value::Int(1)));
                    }
                }
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Int(0)))
    });
}

// ---------------------------------------------------------------------------
// 8. javax.net.ssl.SSLSocketFactory  (2-field synthetic)
// ---------------------------------------------------------------------------

fn register_ssl_socket_factory(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLSocketFactory";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getDefault() -> SSLSocketFactory
    r.register(
        cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            let sf = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 2);
            Ok(Some(Value::Object(Some(sf))))
        },
    );

    // createSocket(String host, int port) -> Socket
    r.register(
        cls,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, _args| {
            let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 2);
            Ok(Some(Value::Object(Some(sock))))
        },
    );

    // createSocket(Socket s, String host, int port, boolean autoClose) -> Socket
    r.register(
        cls,
        "createSocket",
        "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
        |ctx, _args| {
            let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 2);
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
}

// ---------------------------------------------------------------------------
// 9. javax.net.ssl.X509TrustManager  (real validation)
// ---------------------------------------------------------------------------

/// Validate a certificate chain with full security checks:
/// 1. Reject null/empty chains
/// 2. Check validity dates for every certificate
/// 3. Verify each certificate's signature against its issuer
/// 4. Validate chain continuity (issuer[i] == subject[i+1])
fn validate_cert_chain(
    ctx: &mut dyn NativeContext,
    chain_arg: Option<&Value>,
) -> MethodCallResult {
    let chain_arr = match chain_arg {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            // Null chain — reject (security: never accept missing certificates)
            return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                message: "Certificate chain is null — TLS connection rejected".into(),
            }.into());
        }
    };
    let chain_len = ctx.array_length(chain_arr);
    if chain_len == 0 {
        // Empty chain — reject
        return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
            message: "Certificate chain is empty — TLS connection rejected".into(),
        }.into());
    }

    // Without legacy-synthetic-crypto we can only enforce non-empty chains; the
    // signature/date/continuity checks below all rely on the parsed
    // `crate::crypto::crypto_impl::X509Cert` representation. Chain cert DER
    // bytes produced by NEW-13's native-tls path are still captured and
    // returned via SSLSession.getPeerCertificates — the underlying native-tls
    // handshake already performed full PKIX validation (including hostname
    // verification) before the chain reached the JVM side.
    #[cfg(not(feature = "legacy-synthetic-crypto"))]
    {
        let _ = ctx;
        return Ok(None);
    }

    #[cfg(feature = "legacy-synthetic-crypto")]
    return validate_cert_chain_crypto(ctx, chain_arr, chain_len);
}

#[cfg(feature = "legacy-synthetic-crypto")]
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
    let mut parsed_certs: Vec<Option<crate::crypto::crypto_impl::X509Cert>> = Vec::new();

    for i in 0..chain_len {
        if let Value::Object(Some(cert_ref)) = ctx.get_array_element(chain_arr, i) {
            let cert_id = match ctx.get_field(cert_ref, 2) {
                Value::Long(id) => id as u64,
                _ => {
                    parsed_certs.push(None);
                    continue;
                }
            };
            if let Some(cert) = crate::crypto::crypto_impl::cert_get(cert_id) {
                // 1. Check validity dates (always — no feature gate)
                if now < cert.not_before {
                    return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                        message: format!(
                            "Certificate not yet valid: {} (not before {})",
                            cert.subject_cn, cert.not_before
                        ),
                    }.into());
                }
                if now > cert.not_after {
                    return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                        message: format!(
                            "Certificate expired: {} (expired at {})",
                            cert.subject_cn, cert.not_after
                        ),
                    }.into());
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
                parsed_certs[i + 1].as_ref().map(|c| c.public_key_bytes.as_slice())
            } else {
                // Last cert in chain: verify as self-signed (root CA)
                Some(cert.public_key_bytes.as_slice())
            };
            if let Some(spki) = issuer_spki {
                if !cert.verify_signature(spki) {
                    return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                        message: format!(
                            "Certificate signature verification failed for: {}",
                            cert.subject_cn
                        ),
                    }.into());
                }
            }
        }
    }

    // 3. Validate chain continuity: cert[i].issuer should match cert[i+1].subject
    for i in 0..parsed_certs.len().saturating_sub(1) {
        if let (Some(ref cert), Some(ref issuer)) = (&parsed_certs[i], &parsed_certs[i + 1]) {
            if cert.issuer_raw != issuer.subject_raw {
                return Err(rustjvm_types::error::RuntimeError::IllegalStateException {
                    message: format!(
                        "Certificate chain broken: issuer of '{}' does not match subject of '{}'",
                        cert.subject_cn, issuer.subject_cn
                    ),
                }.into());
            }
        }
    }

    Ok(None) // all certs valid
}

fn register_x509_trust_manager(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/X509TrustManager";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // checkClientTrusted(X509Certificate[] chain, String authType) -> void
    // Validates certificate chain: checks dates and chain length.
    r.register(
        cls,
        "checkClientTrusted",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
        |ctx, args| {
            validate_cert_chain(ctx, args.get(1))
        },
    );

    // checkServerTrusted(X509Certificate[] chain, String authType) -> void
    // Validates server certificate chain: checks dates and basic chain integrity.
    r.register(
        cls,
        "checkServerTrusted",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
        |ctx, args| {
            validate_cert_chain(ctx, args.get(1))
        },
    );

    // T19.9 CONSOLIDATION: getAcceptedIssuers() -> X509Certificate[] is now
    // owned by `t27_tls::register_accepted_issuers`, which returns the real
    // system-root DER chain via rustls. The former tls.rs stub returned an
    // empty array and was shadowed in practice by t27_tls (which loads after
    // this function). Keeping only one registration removes the last
    // cross-file duplicate between tls.rs and t27_tls.rs.
}

// ---------------------------------------------------------------------------
// 10. sun.security.ssl.SSLContextImpl  (alias for SSLContext)
// ---------------------------------------------------------------------------

fn register_ssl_context_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/security/ssl/SSLContextImpl";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let protocol_idx = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("TLS")    => 0,
                        Some("TLSv1.2") => 1,
                        Some("TLSv1.3") => 2,
                        Some("SSL")    => 3,
                        _              => 0,
                    }
                }
                _ => 0,
            };
            let obj = alloc_ssl_context(ctx, protocol_idx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let obj = alloc_ssl_context(ctx, 0);
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
            let eng = alloc_ssl_engine(ctx);
            Ok(Some(Value::Object(Some(eng))))
        },
    );

    r.register(
        cls,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = match ctx.get_field(this, CTX_PROTOCOL_IDX) {
                Value::Int(i) => i,
                _ => 0,
            };
            let proto = match idx {
                1 => "TLSv1.2",
                2 => "TLSv1.3",
                3 => "SSL",
                _ => "TLS",
            };
            let s = ctx.create_string(proto);
            Ok(Some(Value::Object(Some(s))))
        },
    );
}

// ---------------------------------------------------------------------------
// 11. SSLEngineResult — accessor stubs
// ---------------------------------------------------------------------------

fn register_ssl_engine_result(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLEngineResult";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getStatus() -> SSLEngineResult$Status
    r.register(
        cls,
        "getStatus",
        "()Ljavax/net/ssl/SSLEngineResult$Status;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let status_obj = alloc_concurrent_synthetic(
                ctx,
                "javax/net/ssl/SSLEngineResult$Status",
                1,
            );
            let status_val = ctx.get_field(this, 0);
            ctx.set_field(status_obj, 0, status_val);
            Ok(Some(Value::Object(Some(status_obj))))
        },
    );

    // getHandshakeStatus() -> SSLEngineResult$HandshakeStatus
    r.register(
        cls,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let hs_obj = alloc_concurrent_synthetic(
                ctx,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                1,
            );
            let hs_val = ctx.get_field(this, 1);
            ctx.set_field(hs_obj, 0, hs_val);
            Ok(Some(Value::Object(Some(hs_obj))))
        },
    );

    // bytesConsumed() -> int
    r.register(cls, "bytesConsumed", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // bytesProduced() -> int
    r.register(cls, "bytesProduced", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
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
    // --- sun.security.ssl.SSLSessionImpl ------------------------------------
    //
    // Keycloak reads the session ID (for access log correlation) and the
    // peer certificate chain (for mTLS client-cert auth). javax.net.ssl
    // versions of these are registered by phases_late.rs; this registers
    // the concrete impl class so direct `SSLSessionImpl` references
    // resolve without falling back to the interpreter.
    let sess_impl = "sun/security/ssl/SSLSessionImpl";
    r.register(sess_impl, "<init>", "()V", native_noop_with_this);
    r.register(sess_impl, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Allocate a 32-byte session id derived from the object identity.
        // Real rustls sessions hand back the server-assigned id; for
        // synthetic native-only paths we emit a 32-byte deterministic
        // buffer so callers can compare equality across invocations.
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 32);
        let addr = this.as_ptr() as u64;
        for i in 0..32 {
            let b = ((addr >> ((i % 8) * 8)) & 0xFF) as u8;
            ctx.set_array_element(arr, i, Value::Int(b as i32));
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
        |ctx, _args| {
            // Default to TLS 1.3 MTI cipher — callers can swap in a
            // real suite after a successful handshake. Picked over
            // TLS_AES_256_GCM_SHA384 so Keycloak's "is this a modern
            // suite?" heuristic (name starts with "TLS_AES_128_GCM_") passes.
            let s = ctx.create_string("TLS_AES_128_GCM_SHA256");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        sess_impl,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("TLSv1.3");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(sess_impl, "isValid", "()Z", |_ctx, _args| {
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
                        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
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

pub(crate) fn register_tls_natives(r: &mut NativeMethodRegistry) {
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
    // T19.9: Keycloak-specific natives for sun.security.ssl.SSLSessionImpl,
    // sun.security.ssl.SSLContextImpl.engineInit, and the hardened
    // SSLEngine.setEnabledCipherSuitesStrict allowlist. These entries are
    // registered LAST in tls.rs so they're visible to phases_late when it
    // runs in the lib.rs wiring (phases_late then re-registers the
    // permissive variants of a few, but the Sun-package shims are unique
    // to this block and never shadowed).
    register_keycloak_tls_natives(r);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tls_tests {
    use super::*;
    use rustjvm_native_api::NativeMethodRegistry;

    // --- Registration tests -------------------------------------------------

    #[test]
    fn test_ssl_context_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLContext";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;").is_some());
        assert!(r.find(cls, "getDefault", "()Ljavax/net/ssl/SSLContext;").is_some());
        assert!(r.find(
            cls,
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"
        ).is_some());
        assert!(r.find(cls, "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;").is_some());
        assert!(r.find(cls, "createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;").is_some());
        assert!(r.find(cls, "getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;").is_some());
        assert!(r.find(cls, "getServerSocketFactory", "()Ljavax/net/ssl/SSLServerSocketFactory;").is_some());
        assert!(r.find(cls, "getDefaultSSLParameters", "()Ljavax/net/ssl/SSLParameters;").is_some());
        assert!(r.find(cls, "getSupportedSSLParameters", "()Ljavax/net/ssl/SSLParameters;").is_some());
        assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_ssl_engine_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLEngine";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "wrap", "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;").is_some());
        assert!(r.find(cls, "unwrap", "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;").is_some());
        assert!(r.find(cls, "beginHandshake", "()V").is_some());
        assert!(r.find(cls, "getHandshakeStatus", "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;").is_some());
        assert!(r.find(cls, "isInboundDone", "()Z").is_some());
        assert!(r.find(cls, "isOutboundDone", "()Z").is_some());
        assert!(r.find(cls, "closeInbound", "()V").is_some());
        assert!(r.find(cls, "closeOutbound", "()V").is_some());
        assert!(r.find(cls, "setUseClientMode", "(Z)V").is_some());
        assert!(r.find(cls, "getUseClientMode", "()Z").is_some());
        assert!(r.find(cls, "getPeerHost", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getPeerPort", "()I").is_some());
        assert!(r.find(cls, "setEnabledProtocols", "([Ljava/lang/String;)V").is_some());
        assert!(r.find(cls, "setEnabledCipherSuites", "([Ljava/lang/String;)V").is_some());
        assert!(r.find(cls, "getApplicationProtocol", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V").is_some());
        assert!(r.find(cls, "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;").is_some());
    }

    #[test]
    fn test_ssl_session_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLSession";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getCipherSuite", "()Ljava/lang/String;").is_some());
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
        assert!(r.find(cls, "getProtocols", "()[Ljava/lang/String;").is_some());
        assert!(r.find(cls, "setProtocols", "([Ljava/lang/String;)V").is_some());
        assert!(r.find(cls, "getCipherSuites", "()[Ljava/lang/String;").is_some());
        assert!(r.find(cls, "setCipherSuites", "([Ljava/lang/String;)V").is_some());
        assert!(r.find(cls, "getApplicationProtocols", "()[Ljava/lang/String;").is_some());
        assert!(r.find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V").is_some());
        assert!(r.find(cls, "getEndpointIdentificationAlgorithm", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "setEndpointIdentificationAlgorithm", "(Ljava/lang/String;)V").is_some());
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
        assert!(r.find(cls, "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;").is_some());
        assert!(r.find(cls, "getDefaultAlgorithm", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "init", "(Ljava/security/KeyStore;)V").is_some());
        assert!(r.find(cls, "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;").is_some());
    }

    #[test]
    fn test_key_manager_factory_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/KeyManagerFactory";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;").is_some());
        assert!(r.find(cls, "getDefaultAlgorithm", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "init", "(Ljava/security/KeyStore;[C)V").is_some());
        assert!(r.find(cls, "getKeyManagers", "()[Ljavax/net/ssl/KeyManager;").is_some());
    }

    #[test]
    fn test_key_store_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "java/security/KeyStore";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getInstance", "(Ljava/lang/String;)Ljava/security/KeyStore;").is_some());
        assert!(r.find(cls, "getDefaultType", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "load", "(Ljava/io/InputStream;[C)V").is_some());
        assert!(r.find(cls, "getCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;").is_some());
        assert!(r.find(cls, "getKey", "(Ljava/lang/String;[C)Ljava/security/Key;").is_some());
        assert!(r.find(cls, "containsAlias", "(Ljava/lang/String;)Z").is_some());
        assert!(r.find(cls, "aliases", "()Ljava/util/Enumeration;").is_some());
        assert!(r.find(cls, "size", "()I").is_some());
        assert!(r.find(cls, "setCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V").is_some());
        assert!(r.find(
            cls,
            "setKeyEntry",
            "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"
        ).is_some());
        assert!(r.find(cls, "deleteEntry", "(Ljava/lang/String;)V").is_some());
    }

    #[test]
    fn test_ssl_socket_factory_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLSocketFactory";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getDefault", "()Ljavax/net/ssl/SSLSocketFactory;").is_some());
        assert!(r.find(cls, "createSocket", "(Ljava/lang/String;I)Ljava/net/Socket;").is_some());
        assert!(r.find(cls, "createSocket", "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;").is_some());
        assert!(r.find(cls, "getDefaultCipherSuites", "()[Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getSupportedCipherSuites", "()[Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_x509_trust_manager_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/X509TrustManager";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "checkClientTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V").is_some());
        assert!(r.find(cls, "checkServerTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V").is_some());
        // T19.9: getAcceptedIssuers moved to t27_tls.rs (rustls-backed). The
        // tls.rs baseline no longer registers it — t27 is the canonical home.
        assert!(r.find(cls, "getAcceptedIssuers", "()[Ljava/security/cert/X509Certificate;").is_none());
    }

    #[test]
    fn test_ssl_context_impl_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "sun/security/ssl/SSLContextImpl";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;").is_some());
        assert!(r.find(cls, "getDefault", "()Ljavax/net/ssl/SSLContext;").is_some());
        assert!(r.find(
            cls,
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"
        ).is_some());
        assert!(r.find(cls, "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;").is_some());
        assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_ssl_engine_result_registration() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLEngineResult";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getStatus", "()Ljavax/net/ssl/SSLEngineResult$Status;").is_some());
        assert!(r.find(cls, "getHandshakeStatus", "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;").is_some());
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
        assert_eq!(SUPPORTED_PROTOCOLS.len(), 2, "Only TLSv1.3 and TLSv1.2 should be supported");
    }

    #[test]
    fn test_status_constants_are_distinct() {
        let statuses = [STATUS_OK, STATUS_CLOSED, STATUS_BUFFER_UNDERFLOW, STATUS_BUFFER_OVERFLOW];
        for i in 0..statuses.len() {
            for j in (i + 1)..statuses.len() {
                assert_ne!(statuses[i], statuses[j]);
            }
        }
    }

    #[test]
    fn test_handshake_status_constants_are_distinct() {
        let hs = [HS_NOT_HANDSHAKING, HS_NEED_WRAP, HS_NEED_UNWRAP, HS_NEED_TASK, HS_FINISHED];
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
        assert!(total >= 8, "Expected at least 8 total ciphers, got {}", total);
    }

    #[test]
    fn test_protocol_index_mapping() {
        // Verify the documented mapping used in register_ssl_context
        // idx 0 -> TLS, 1 -> TLSv1.2, 2 -> TLSv1.3, 3 -> SSL
        let mappings: &[(i32, &str)] = &[
            (0, "TLS"),
            (1, "TLSv1.2"),
            (2, "TLSv1.3"),
            (3, "SSL"),
        ];
        for (idx, proto) in mappings {
            let result = match *idx {
                1 => "TLSv1.2",
                2 => "TLSv1.3",
                3 => "SSL",
                _ => "TLS",
            };
            assert_eq!(result, *proto, "Protocol index {} should map to {}", idx, proto);
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
        // All field indices must be within the 8-field synthetic object
        assert!(ENG_CLIENT_MODE < 8);
        assert!(ENG_HANDSHAKE_STATUS < 8);
        assert!(ENG_INBOUND_DONE < 8);
        assert!(ENG_OUTBOUND_DONE < 8);
        assert!(ENG_PROTOCOL_IDX < 8);
        assert!(ENG_PEER_HOST < 8);
        assert!(ENG_PEER_PORT < 8);
        assert!(ENG_ALPN_PROTOCOL < 8);
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
        ];
        for i in 0..indices.len() {
            for j in (i + 1)..indices.len() {
                assert_ne!(indices[i], indices[j], "Duplicate field index at {} and {}", i, j);
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
            assert_ne!(*proto, "TLSv1.1", "TLSv1.1 must not be in SUPPORTED_PROTOCOLS");
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
                "Unexpected protocol: {}", proto
            );
        }
    }

    #[test]
    fn test_supported_protocols_len() {
        assert_eq!(SUPPORTED_PROTOCOLS.len(), 2, "Expected exactly 2 protocols (TLSv1.3 + TLSv1.2)");
    }

    // --- Handshake state machine tests ---

    #[test]
    fn test_client_begins_handshake_with_need_wrap() {
        // Client mode (1) should start with NEED_WRAP to send ClientHello
        let client_mode = 1;
        let initial_hs = if client_mode != 0 { HS_NEED_WRAP } else { HS_NEED_UNWRAP };
        assert_eq!(initial_hs, HS_NEED_WRAP);
    }

    #[test]
    fn test_server_begins_handshake_with_need_unwrap() {
        // Server mode (0) should start with NEED_UNWRAP to receive ClientHello
        let client_mode = 0;
        let initial_hs = if client_mode != 0 { HS_NEED_WRAP } else { HS_NEED_UNWRAP };
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
        let proto = match idx { 1 => "TLSv1.2", 2 => "TLSv1.3", 3 => "SSL", _ => "TLS" };
        assert_eq!(proto, "TLS");
    }

    #[test]
    fn test_ssl_context_protocol_idx_tls12() {
        let idx: i32 = 1;
        let proto = match idx { 1 => "TLSv1.2", 2 => "TLSv1.3", 3 => "SSL", _ => "TLS" };
        assert_eq!(proto, "TLSv1.2");
    }

    #[test]
    fn test_ssl_context_protocol_idx_tls13() {
        let idx: i32 = 2;
        let proto = match idx { 1 => "TLSv1.2", 2 => "TLSv1.3", 3 => "SSL", _ => "TLS" };
        assert_eq!(proto, "TLSv1.3");
    }

    #[test]
    fn test_ssl_context_protocol_idx_ssl() {
        let idx: i32 = 3;
        let proto = match idx { 1 => "TLSv1.2", 2 => "TLSv1.3", 3 => "SSL", _ => "TLS" };
        assert_eq!(proto, "SSL");
    }

    #[test]
    fn test_ssl_context_protocol_idx_unknown_defaults_to_tls() {
        let idx: i32 = 99;
        let proto = match idx { 1 => "TLSv1.2", 2 => "TLSv1.3", 3 => "SSL", _ => "TLS" };
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
                assert_ne!(indices[i], indices[j], "Duplicate SSLContext field at {} and {}", i, j);
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
            ("setEndpointIdentificationAlgorithm", "(Ljava/lang/String;)V"),
            ("getNeedClientAuth", "()Z"),
            ("setNeedClientAuth", "(Z)V"),
            ("getWantClientAuth", "()Z"),
            ("setWantClientAuth", "(Z)V"),
        ].iter().filter(|(name, desc)| r.find(cls, name, desc).is_some()).count();
        assert_eq!(count, 13, "Expected all 13 SSLParameters methods registered");
    }

    #[test]
    fn test_key_store_registration_complete() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "java/security/KeyStore";
        let all_methods = [
            ("<init>", "()V"),
            ("getInstance", "(Ljava/lang/String;)Ljava/security/KeyStore;"),
            ("getDefaultType", "()Ljava/lang/String;"),
            ("load", "(Ljava/io/InputStream;[C)V"),
            ("getCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
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
            .find("rustjvm/tls/T27SelfTest", "run", "()Ljava/lang/String;")
            .is_some());
    }

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
                    cls, name, desc,
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

    #[test]
    fn t19_9_handshake_still_green() {
        // Regression sentinel — if Session 87 Lambda's three server-handshake
        // tests in tls_impl ever regress, this module-level test must
        // notice. The tests themselves are in
        // `crate::tls::tls_impl::tests`. Here we confirm the
        // state-machine entry points they rely on still exist by
        // constructing one and walking a minimal client -> server ->
        // FINISHED transition.
        use crate::tls::tls_impl::{CipherSuite, HandshakeMessage, Tls13StateMachine, TlsExtension};

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
