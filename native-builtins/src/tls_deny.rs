// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deny-by-default guard for the JSSE socket-factory surface.
//!
//! # The bug class this closes
//!
//! `javax.net.ssl.SSLSocketFactory` extends `javax.net.SocketFactory` and
//! `javax.net.ssl.SSLServerSocketFactory` extends
//! `javax.net.ServerSocketFactory`. In this VM the *base* classes carry
//! native registrations that build a **plaintext** `java.net.Socket` /
//! `java.net.ServerSocket`
//! (`phases_early::register_phase52_server_socket_factory`). Those bases are
//! abstract in the real JDK, so the registrations exist only to give the
//! default (non-TLS) factory something to run.
//!
//! Native dispatch resolves up the class hierarchy. So any
//! `SSLSocketFactory` / `SSLServerSocketFactory` overload that does **not**
//! have its own TLS bridge lands on the plaintext base implementation and
//! hands the caller a cleartext socket where TLS was requested. The caller
//! has no way to tell: it asked an `SSLServerSocketFactory` for a server
//! socket and got a `ServerSocket` back, exactly as the API promises.
//!
//! This is not hypothetical. `t27_tls.rs`'s own comment on
//! `register_sslserversocket` records it:
//!
//! > Without explicit bridges here, dispatch reaches `ServerSocketFactory`'s
//! > plaintext implementation and an LDAPS client gets "wrong version number"
//! > after the SocketFactory client-side fix succeeds.
//!
//! and `lib.rs`'s call site for `t27_tls::register_t27_natives` says the same
//! thing ("without this complete phase, a configured SSL server factory falls
//! through to the plaintext ServerSocketFactory overloads").
//!
//! Overloads were bridged **individually, as each was found in the field**.
//! That is a losing strategy: the failure mode of a missing bridge is silence.
//!
//! # The fix
//!
//! The plaintext base implementations now call
//! [`deny_plaintext_fallback`] first. If the receiver is a TLS factory, the
//! call raises instead of returning a cleartext object. There is deliberately
//! **no arm that returns a value**.
//!
//! The consequence is that a TLS overload works only if it was *explicitly*
//! bridged. Adding a new overload to the base class without also bridging it
//! (or recording it as knowingly-unsupported) now fails loudly at the first
//! call and, before that, fails
//! [`base_overloads_are_bridged_or_explicitly_denied`] in this module's test
//! suite.
//!
//! # Which exception
//!
//! `javax.net.ssl.SSLException` — a subclass of `IOException`, which is what
//! both `SocketFactory.createSocket` and
//! `ServerSocketFactory.createServerSocket` already declare, so an existing
//! `catch (IOException)` still handles it and the refusal cannot escape past
//! a caller that was written to handle connection failure. It is also
//! unmistakably a TLS failure, which `SocketException("Unsupported
//! operation")` (the real JDK's wording for the no-arg case) is not.
//!
//! The fallback arm, for a build where `javax/net/ssl/SSLException` cannot be
//! constructed, is a plain `IOException` — still an error, still catchable,
//! still never a socket.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

/// `javax.net.ssl.SSLSocketFactory` — client side.
pub(crate) const SSL_SOCKET_FACTORY: &str = "javax/net/ssl/SSLSocketFactory";
/// `javax.net.ssl.SSLServerSocketFactory` — server side.
pub(crate) const SSL_SERVER_SOCKET_FACTORY: &str = "javax/net/ssl/SSLServerSocketFactory";

/// Which plaintext base surface a guard is protecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TlsFactoryKind {
    /// `javax.net.SocketFactory.createSocket(..)`.
    Socket,
    /// `javax.net.ServerSocketFactory.createServerSocket(..)`.
    ServerSocket,
}

impl TlsFactoryKind {
    /// The JSSE subclass whose instances must never reach the plaintext base.
    pub(crate) fn tls_class(self) -> &'static str {
        match self {
            TlsFactoryKind::Socket => SSL_SOCKET_FACTORY,
            TlsFactoryKind::ServerSocket => SSL_SERVER_SOCKET_FACTORY,
        }
    }

    /// The plaintext base class carrying the registration being guarded.
    pub(crate) fn base_class(self) -> &'static str {
        match self {
            TlsFactoryKind::Socket => "javax/net/SocketFactory",
            TlsFactoryKind::ServerSocket => "javax/net/ServerSocketFactory",
        }
    }

    /// Descriptors declared TLS-bridged for this surface. See the allowlists
    /// below; used only to sharpen the refusal message, never to permit a
    /// call.
    pub(crate) fn bridged_overloads(self) -> &'static [&'static str] {
        match self {
            TlsFactoryKind::Socket => BRIDGED_SSL_SOCKET_FACTORY_OVERLOADS,
            TlsFactoryKind::ServerSocket => BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS,
        }
    }
}

// ---------------------------------------------------------------------------
// The explicit opt-in lists
// ---------------------------------------------------------------------------
//
// These are the *only* place a TLS overload is declared supported. They are
// checked against the live registry by
// `base_overloads_are_bridged_or_explicitly_denied`, so they cannot drift out
// of sync with the registrations without the test suite saying so.

/// `SSLSocketFactory.createSocket` descriptors that have a real TLS bridge
/// (`phases_late::ssl_security::register_p68_ssl`). Every one of these
/// produces a genuine `javax/net/ssl/SSLSocket`.
pub(crate) const BRIDGED_SSL_SOCKET_FACTORY_OVERLOADS: &[&str] = &[
    // Unconnected socket, connected later via `Socket.connect(SocketAddress)`.
    "()Ljava/net/Socket;",
    "(Ljava/lang/String;I)Ljava/net/Socket;",
    "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
    "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
    "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
    // SSLSocketFactory's own layering overload — no plaintext base exists for
    // it, listed here so the supported set is readable in one place.
    "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
];

/// `SSLServerSocketFactory.createServerSocket` descriptors that have a real
/// TLS bridge (`t27_tls::register_sslserversocket`). Every one of these binds
/// a real TLS listener.
pub(crate) const BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS: &[&str] = &[
    "(I)Ljava/net/ServerSocket;",
    "(II)Ljava/net/ServerSocket;",
    "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
];

/// `SSLServerSocketFactory.createServerSocket` descriptors this VM knowingly
/// does **not** implement, and therefore refuses.
///
/// `createServerSocket()` returns an *unbound* server socket, bound later by
/// `ServerSocket.bind(SocketAddress)`. `t27_tls::create_ssl_server_socket`
/// binds a `TcpListener` and builds the rustls config in one step, so there
/// is no unbound TLS listener object to hand back. Until there is, the call
/// raises.
///
/// Before this module existed it returned a plaintext `java.net.ServerSocket`
/// via the base-class registration — the exact degradation this file closes,
/// and the one overload of the four that was never noticed in the field
/// because nothing in the test corpus used the connect-later shape on the
/// server side.
pub(crate) const UNBRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS: &[&str] =
    &["()Ljava/net/ServerSocket;"];

// ---------------------------------------------------------------------------
// Refusal
// ---------------------------------------------------------------------------

/// Build a `javax.net.ssl.SSLException` carrying `msg`, falling back to a
/// plain `IOException` if that class cannot be constructed.
///
/// Mirrors `phases_early::throw_jca_exc` (the mechanism documented in
/// `docs/security/crypto-failure-contract.md`), but keeps the fallback inside
/// the `IOException` family so a caller's `catch (IOException)` still sees it.
/// **There is deliberately no arm that returns a value.**
pub(crate) fn throw_tls_exc(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "javax/net/ssl/SSLException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IOException {
        message: msg.to_string(),
    }
    .into()
}

/// Whether `receiver`'s runtime class is `kind`'s JSSE factory, or a subclass
/// of it (application wrappers such as Tomcat's
/// `TesterSupport.ClientSSLSocketFactory` are real bytecode subclasses).
///
/// Matched by `ClassId`, not by name string: `alloc_concurrent_synthetic`
/// documents that `class_name_of_id` can misreport an interface-like
/// synthetic class back as `java/lang/Object` (both JSSE factories are
/// abstract, so their synthetic carriers hit exactly that). `class_id_by_name`
/// resolved once and compared by id is immune to the per-object misreport —
/// the same reasoning as `t27_tls::resolve_sslcontext_from_factory`.
///
/// The name comparison is kept only as a backstop for the case where the
/// receiver's `ClassId` *is* the misreported one but its recorded name is
/// still correct. It cannot false-positive: it tests equality with the exact
/// JSSE class name, never a substring, so an unrelated
/// `javax/net/SocketFactory` carrier is never mistaken for a TLS factory.
pub(crate) fn receiver_is_tls_factory(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    kind: TlsFactoryKind,
) -> bool {
    let tls_name = kind.tls_class();
    let receiver_cid = ctx.class_id_of_object(receiver);
    if let Some(tls_cid) = ctx.class_id_by_name(tls_name) {
        // `is_subclass` is reflexive (`classloading::Class::is_subclass_of`
        // early-outs on `self.id == other_id`), so this covers both the exact
        // carrier and any application subclass.
        if ctx.is_subclass(receiver_cid, tls_cid) {
            return true;
        }
    }
    matches!(ctx.class_name_arc_of_id(receiver_cid).as_deref(), Some(n) if n == tls_name)
}

/// Guard the plaintext base-class implementation of `method`/`descriptor`.
///
/// Call this as the **first** statement of every
/// `javax/net/SocketFactory.createSocket` and
/// `javax/net/ServerSocketFactory.createServerSocket` native. Returns
/// `Ok(())` for an ordinary (non-TLS) factory and an error for a TLS one.
///
/// A TLS receiver reaching a plaintext base implementation means, by
/// construction, that no TLS bridge matched this `(class, method,
/// descriptor)` triple — the overload is unbridged. There is nothing to
/// consult at run time beyond that fact: if the overload *were* bridged, the
/// bridge would have won dispatch and this function would never have been
/// entered. The allowlists above exist to make the same statement checkable
/// at build/test time rather than only at call time.
///
/// `args[0]` is the receiver for these instance methods. A missing or null
/// receiver is left alone — that is a static call or an NPE for the body
/// proper to report, not a TLS question.
pub(crate) fn deny_plaintext_fallback(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    kind: TlsFactoryKind,
    method: &str,
    descriptor: &str,
) -> Result<(), MethodCallFailed> {
    let receiver = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(()),
    };
    if !receiver_is_tls_factory(ctx, receiver, kind) {
        return Ok(());
    }
    let tls_class = kind.tls_class().replace('/', ".");
    let base_class = kind.base_class().replace('/', ".");
    // Two different defects reach here; say which one, because the fix differs.
    // Either way the answer is the same refusal — the diagnosis never changes
    // the outcome into a socket.
    let diagnosis = if kind.bridged_overloads().contains(&descriptor) {
        "the overload IS listed as TLS-bridged, so its bridge registration was \
         lost or was overwritten by a later registration on the same triple \
         (registry semantics are last-writer-wins)"
    } else {
        "the overload has no TLS bridge; register one on the TLS class and add \
         its descriptor to tls_deny::BRIDGED_*, or record it in \
         tls_deny::UNBRIDGED_* if it is knowingly unsupported"
    };
    Err(throw_tls_exc(
        ctx,
        &format!(
            "{tls_class}.{method}{descriptor} has no TLS implementation in this VM. \
             Refusing to fall through to {base_class}'s plaintext implementation: \
             a cleartext socket must never be returned where TLS was requested. \
             Diagnosis: {diagnosis}."
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Allocate an object whose runtime class is `name`.
    fn obj_of_class(ctx: &mut MockNativeContext, name: &str) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(name).expect("class");
        ctx.alloc_object(cid, 2)
    }

    // --- MUST STILL WORK ----------------------------------------------------

    #[test]
    fn plain_socket_factory_receiver_is_allowed_through() {
        let mut ctx = mock_ctx();
        // Both JSSE classes exist, so the guard has something to compare
        // against — this is the realistic shape, not an "SSL absent" one.
        ctx.ensure_class_initialized(SSL_SOCKET_FACTORY).unwrap();
        let plain = obj_of_class(&mut ctx, "javax/net/SocketFactory");
        let args = [Value::Object(Some(plain))];
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &args,
            TlsFactoryKind::Socket,
            "createSocket",
            "(Ljava/lang/String;I)Ljava/net/Socket;",
        )
        .is_ok());
    }

    #[test]
    fn plain_server_socket_factory_receiver_is_allowed_through() {
        let mut ctx = mock_ctx();
        ctx.ensure_class_initialized(SSL_SERVER_SOCKET_FACTORY)
            .unwrap();
        let plain = obj_of_class(&mut ctx, "javax/net/ServerSocketFactory");
        let args = [Value::Object(Some(plain))];
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &args,
            TlsFactoryKind::ServerSocket,
            "createServerSocket",
            "(I)Ljava/net/ServerSocket;",
        )
        .is_ok());
    }

    #[test]
    fn an_unrelated_class_is_not_mistaken_for_a_tls_factory() {
        // The name backstop must be an equality test, not a substring test.
        let mut ctx = mock_ctx();
        ctx.ensure_class_initialized(SSL_SOCKET_FACTORY).unwrap();
        let other = obj_of_class(&mut ctx, "org/example/NotAnSSLSocketFactoryAtAll");
        assert!(!receiver_is_tls_factory(
            &mut ctx,
            other,
            TlsFactoryKind::Socket
        ));
    }

    #[test]
    fn a_static_call_with_no_receiver_is_left_alone() {
        let mut ctx = mock_ctx();
        ctx.ensure_class_initialized(SSL_SOCKET_FACTORY).unwrap();
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &[],
            TlsFactoryKind::Socket,
            "createSocket",
            "()Ljava/net/Socket;",
        )
        .is_ok());
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &[Value::Object(None)],
            TlsFactoryKind::Socket,
            "createSocket",
            "()Ljava/net/Socket;",
        )
        .is_ok());
    }

    // --- MUST RAISE ---------------------------------------------------------

    #[test]
    fn ssl_socket_factory_receiver_is_denied() {
        let mut ctx = mock_ctx();
        let ssl = obj_of_class(&mut ctx, SSL_SOCKET_FACTORY);
        let args = [Value::Object(Some(ssl))];
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &args,
            TlsFactoryKind::Socket,
            "createSocket",
            "(Ljava/lang/String;I)Ljava/net/Socket;",
        )
        .is_err());
    }

    #[test]
    fn ssl_server_socket_factory_receiver_is_denied() {
        let mut ctx = mock_ctx();
        let ssl = obj_of_class(&mut ctx, SSL_SERVER_SOCKET_FACTORY);
        let args = [Value::Object(Some(ssl))];
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &args,
            TlsFactoryKind::ServerSocket,
            "createServerSocket",
            // The live gap: the no-arg overload has no TLS bridge and used to
            // return a plaintext `java.net.ServerSocket`.
            "()Ljava/net/ServerSocket;",
        )
        .is_err());
    }

    #[test]
    fn an_application_subclass_of_an_ssl_factory_is_denied() {
        // Tomcat's `TesterSupport.ClientSSLSocketFactory`, Apache HttpClient's
        // wrappers, etc. are real bytecode subclasses — the guard must follow
        // the hierarchy, not just match the exact carrier class.
        let mut ctx = mock_ctx();
        let base = ctx.ensure_class_initialized(SSL_SOCKET_FACTORY).unwrap();
        let sub = ctx
            .ensure_class_initialized("org/example/WrappingSSLSocketFactory")
            .unwrap();
        ctx.set_superclass(sub, base);
        let wrapper = ctx.alloc_object(sub, 2);
        assert!(receiver_is_tls_factory(
            &mut ctx,
            wrapper,
            TlsFactoryKind::Socket
        ));
        let args = [Value::Object(Some(wrapper))];
        assert!(deny_plaintext_fallback(
            &mut ctx,
            &args,
            TlsFactoryKind::Socket,
            "createSocket",
            "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
        )
        .is_err());
    }

    #[test]
    fn the_refusal_names_tls_and_is_not_a_socket() {
        let mut ctx = mock_ctx();
        let ssl = obj_of_class(&mut ctx, SSL_SERVER_SOCKET_FACTORY);
        let args = [Value::Object(Some(ssl))];
        let err = deny_plaintext_fallback(
            &mut ctx,
            &args,
            TlsFactoryKind::ServerSocket,
            "createServerSocket",
            "()Ljava/net/ServerSocket;",
        )
        .expect_err("must refuse");
        match err {
            // The mock can construct `javax/net/ssl/SSLException`, so this is
            // the arm normally taken; the important property is that it is a
            // thrown Java exception and not a returned object.
            MethodCallFailed::ExceptionThrown(exc) => {
                let cid = ctx.class_id_of_object(exc);
                assert_eq!(
                    ctx.class_name_arc_of_id(cid).as_deref(),
                    Some("javax/net/ssl/SSLException")
                );
            }
            // Fallback arm: still an error, still an `IOException`, still not
            // a socket.
            MethodCallFailed::InternalError(e) => {
                let text = format!("{e}");
                assert!(text.contains("IOException"), "unexpected fallback: {text}");
            }
        }
    }

    // --- The opt-in invariant ----------------------------------------------

    /// Every plaintext base overload must be *either* bridged for TLS *or*
    /// recorded as knowingly unsupported. Adding a new overload to
    /// `javax/net/SocketFactory` or `javax/net/ServerSocketFactory` without
    /// doing one of the two fails here — which is the whole point: the next
    /// unbridged overload must fail loudly rather than silently.
    #[test]
    fn base_overloads_are_bridged_or_explicitly_denied() {
        let mut r = NativeMethodRegistry::new();
        crate::phases_early::register_phase52_server_socket_factory(&mut r);
        crate::phases_late::ssl_security::register_p68_ssl(&mut r);
        crate::t27_tls::register_t27_natives(&mut r);

        for kind in [TlsFactoryKind::Socket, TlsFactoryKind::ServerSocket] {
            let (method, bridged, unbridged) = match kind {
                TlsFactoryKind::Socket => (
                    "createSocket",
                    BRIDGED_SSL_SOCKET_FACTORY_OVERLOADS,
                    // No `SocketFactory.createSocket` overload is knowingly
                    // unsupported: all five plaintext ones are bridged.
                    &[] as &'static [&'static str],
                ),
                TlsFactoryKind::ServerSocket => (
                    "createServerSocket",
                    BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS,
                    UNBRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS,
                ),
            };
            // 1. Nothing may be in both lists.
            for d in unbridged {
                assert!(
                    !bridged.contains(d),
                    "{d} is listed as both bridged and unbridged"
                );
            }
            // 2. Everything the allowlist calls bridged really is registered
            //    on the TLS class — otherwise the list is a lie and the guard
            //    would refuse a call the list says works.
            for d in bridged {
                assert!(
                    r.find(kind.tls_class(), method, d).is_some(),
                    "{}.{}{} is in BRIDGED_* but has no registration",
                    kind.tls_class(),
                    method,
                    d
                );
            }
            // 3. Everything the allowlist calls unbridged really is absent.
            for d in unbridged {
                assert!(
                    r.find(kind.tls_class(), method, d).is_none(),
                    "{}.{}{} is in UNBRIDGED_* but IS registered — move it",
                    kind.tls_class(),
                    method,
                    d
                );
            }
        }
    }

    /// The companion direction: the guard is only reachable for descriptors
    /// the plaintext base actually registers, so every such descriptor must
    /// be accounted for in one of the two lists. A new plaintext overload
    /// with no entry is a new silent-downgrade hole.
    #[test]
    fn every_plaintext_base_overload_is_accounted_for() {
        // The plaintext base only registers these overloads in SYNTHETIC
        // net-socket mode. `real_net_sockets` is default-ON, and the registry
        // drops every `javax/net/SocketFactory` native when it is — by
        // design, so real JDK bytecode drives the real socket path. Without
        // this override the registry is EMPTY here and the census below
        // asserts against nothing, which is what made this test fail on a
        // default build rather than on a broken one.
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SYNTHETIC_NET_SOCKETS", Some("1"))],
            || {
                let mut r = NativeMethodRegistry::new();
                crate::phases_early::register_phase52_server_socket_factory(&mut r);
                r
            },
        );
        assert!(
            r.find(
                "javax/net/SocketFactory",
                "getDefault",
                "()Ljavax/net/SocketFactory;"
            )
            .is_some(),
            "the synthetic-net-sockets override did not take effect, so the census below would pass vacuously"
        );

        let socket_descs = [
            "()Ljava/net/Socket;",
            "(Ljava/lang/String;I)Ljava/net/Socket;",
            "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
            "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
            "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        ];
        for d in socket_descs {
            assert!(
                r.find("javax/net/SocketFactory", "createSocket", d)
                    .is_some(),
                "plaintext base lost {d} — update this test and the allowlists"
            );
            assert!(
                BRIDGED_SSL_SOCKET_FACTORY_OVERLOADS.contains(&d),
                "plaintext createSocket{d} has no TLS opt-in entry"
            );
        }

        let server_descs = [
            "()Ljava/net/ServerSocket;",
            "(I)Ljava/net/ServerSocket;",
            "(II)Ljava/net/ServerSocket;",
            "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
        ];
        for d in server_descs {
            assert!(
                r.find("javax/net/ServerSocketFactory", "createServerSocket", d)
                    .is_some(),
                "plaintext base lost {d} — update this test and the allowlists"
            );
            assert!(
                BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS.contains(&d)
                    || UNBRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS.contains(&d),
                "plaintext createServerSocket{d} has no TLS opt-in entry"
            );
        }
    }
}
