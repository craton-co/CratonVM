// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! **This module was never wired in, and that is why it had a defect to find.**
//! Until 2026-09-12 `deny_plaintext_fallback` had no caller outside its own
//! test module: the nine plaintext base registrations in `phases_early` did
//! not call it, so the sentence below described an intention rather than a
//! mechanism. The overload it was written to catch —
//! `SSLServerSocketFactory.createServerSocket()`, the one shape nothing in
//! the corpus used — went on returning a plaintext `java.net.ServerSocket`
//! for as long as the module existed. It now has a real bridge (see
//! [`BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS`]) AND the nine call sites,
//! so the net catches the NEXT one.
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
pub const SSL_SOCKET_FACTORY: &str = "javax/net/ssl/SSLSocketFactory";
/// `javax.net.ssl.SSLServerSocketFactory` — server side.
pub const SSL_SERVER_SOCKET_FACTORY: &str = "javax/net/ssl/SSLServerSocketFactory";

/// Which plaintext base surface a guard is protecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsFactoryKind {
    /// `javax.net.SocketFactory.createSocket(..)`.
    Socket,
    /// `javax.net.ServerSocketFactory.createServerSocket(..)`.
    ServerSocket,
}

impl TlsFactoryKind {
    /// The JSSE subclass whose instances must never reach the plaintext base.
    pub fn tls_class(self) -> &'static str {
        match self {
            TlsFactoryKind::Socket => SSL_SOCKET_FACTORY,
            TlsFactoryKind::ServerSocket => SSL_SERVER_SOCKET_FACTORY,
        }
    }

    /// The plaintext base class carrying the registration being guarded.
    pub fn base_class(self) -> &'static str {
        match self {
            TlsFactoryKind::Socket => "javax/net/SocketFactory",
            TlsFactoryKind::ServerSocket => "javax/net/ServerSocketFactory",
        }
    }

    /// Descriptors declared TLS-bridged for this surface. See the allowlists
    /// below; used only to sharpen the refusal message, never to permit a
    /// call.
    pub fn bridged_overloads(self) -> &'static [&'static str] {
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
pub const BRIDGED_SSL_SOCKET_FACTORY_OVERLOADS: &[&str] = &[
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
/// TLS bridge (`t27_tls::register_sslserversocket`).
///
/// The first three bind a real TLS listener. The no-arg one does not, and it
/// was in [`UNBRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS`] until 2026-09-12
/// for that reason — but "no listener" and "no SSLServerSocket" are different
/// things, and the JDK's own
/// `SSLServerSocketFactoryImpl.createServerSocket()` is
/// `new SSLServerSocketImpl(context)`: an UNBOUND socket whose parameters can
/// be set and read before anything binds. That is what the bridge returns
/// now, so the two rows of `L6TlsParamSweep` that died on
/// `class java.net.ServerSocket cannot be cast to class
/// javax.net.ssl.SSLServerSocket` pass, and the caller who did NOT cast stops
/// getting a plaintext listener from a factory whose name says TLS.
///
/// `ServerSocket.bind(SocketAddress)` on that socket is refused with a named
/// `SocketException` rather than brought up: `create_ssl_server_socket`
/// resolves its TLS identity from the FACTORY it was called on and a socket
/// keeps no rooted reference to one.
pub const BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS: &[&str] = &[
    "()Ljava/net/ServerSocket;",
    "(I)Ljava/net/ServerSocket;",
    "(II)Ljava/net/ServerSocket;",
    "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
];

/// `SSLServerSocketFactory.createServerSocket` descriptors this VM knowingly
/// does **not** implement, and therefore refuses.
///
/// **Empty since 2026-09-12**, when the no-arg overload got its bridge. The
/// constant stays because the guard it feeds is the mechanism that keeps a
/// future overload from silently degrading to the plaintext base-class
/// registration — which is what the no-arg one did, unnoticed in the field,
/// because nothing in the test corpus used the connect-later shape on the
/// server side.
pub const UNBRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS: &[&str] = &[];

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
pub fn throw_tls_exc(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "javax/net/ssl/SSLException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
        || {
            RuntimeError::IOException {
                message: msg.to_string(),
            }
            .into()
        },
    )
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
pub fn receiver_is_tls_factory(
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
pub fn deny_plaintext_fallback(
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
mod tests;
