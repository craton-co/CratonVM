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
