//! `--compatible`: the JSSE socket and factory carriers are instances of the JDK's own
//! `sun.security.ssl` classes, as they are on HotSpot.
//!
//! `SSLSocket` / `SSLServerSocket` / `SSLSocketFactory` / `SSLServerSocketFactory` are abstract, so a
//! carrier stamped with one of them fails `instanceof SSLSocketImpl`, a cast to the Impl and reflection
//! over the Impl's declared fields (JVMS 6.5: `new` cannot produce them). This is the registration half of
//! minting the real classes, the same recipe as `t27_tls::alloc_session_carrier`:
//!
//! * the carrier's private slots start ABOVE the Impl's declared fields (`carrier_base`, read and written
//!   through `sock_field` / `sock_set_field` in `phases_late/ssl_security.rs`): native field access is
//!   descriptor-aware, so slots 0..4 aliasing typed Impl fields would be coerced;
//! * every method the Impl (and `BaseSSLSocketImpl`) declares with `Code` needs a row on the Impl, because
//!   dispatch keys on the receiver's own class first. [`mirror_socket_family_rows_onto_impls`] copies the
//!   row the old (abstract) carrier would have reached, walking the same chain: the public class, then the
//!   `java.net` / `javax.net` ancestor whose rows served it through the superclass walk.
//!
//! Kill switch: `CRATONVM_JSSE_SOCKET_IMPL=0` (token `jsse-socket-impl`).
//! Page: `docs/internal/fixed-bugs/compatible-tls-and-http-carriers-are-instances-of-abstract-classes-FIXED-20261010.md`.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::ObjectRef;

use crate::{appended_slot_base_for_class, try_alloc_concurrent_synthetic};

pub(crate) const SSL_SOCKET_IMPL: &str = "sun/security/ssl/SSLSocketImpl";
pub(crate) const SSL_SERVER_SOCKET_IMPL: &str = "sun/security/ssl/SSLServerSocketImpl";
pub(crate) const SSL_SOCKET_FACTORY_IMPL: &str = "sun/security/ssl/SSLSocketFactoryImpl";
pub(crate) const SSL_SERVER_SOCKET_FACTORY_IMPL: &str =
    "sun/security/ssl/SSLServerSocketFactoryImpl";

pub(crate) const SSL_SOCKET_PUBLIC: &str = "javax/net/ssl/SSLSocket";
pub(crate) const SSL_SERVER_SOCKET_PUBLIC: &str = "javax/net/ssl/SSLServerSocket";
pub(crate) const SSL_SOCKET_FACTORY_PUBLIC: &str = "javax/net/ssl/SSLSocketFactory";
pub(crate) const SSL_SERVER_SOCKET_FACTORY_PUBLIC: &str = "javax/net/ssl/SSLServerSocketFactory";

/// Is the carrier minted as the Impl class (the default)?
pub(crate) fn socket_impl_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JSSE_SOCKET_IMPL")
}

/// Mint a carrier of private-slot width `width`: the Impl when the image has it and the switch is on,
/// the public (abstract) class otherwise. The allocation is `real + width` slots so the private slots sit
/// above every declared field (`carrier_base`).
#[track_caller]
pub(crate) fn alloc_tls_carrier(
    ctx: &mut dyn NativeContext,
    public: &str,
    imp: &str,
    width: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Only where the mirrored rows exist (`SyntheticStub`s, dropped under `--jdk-only` / `jdk-jsse`).
    if socket_impl_enabled() && !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkJsse) {
        if let Some((name, _)) = cratonvm_native_api::instantiable::first_instantiable(ctx, &[imp])
        {
            let real = appended_slot_base_for_class(ctx, name);
            if real > 0 {
                let obj = try_alloc_concurrent_synthetic(ctx, name, real + width)?;
                // See `alloc_ssl_socket`: an unwritten private slot reads back as `Int(0)`, not null.
                for slot in 0..width {
                    ctx.set_field(obj, real + slot, cratonvm_types::Value::Object(None));
                }
                return Ok(obj);
            }
        }
    }
    try_alloc_concurrent_synthetic(ctx, public, width)
}

/// Where `this`'s private slot map starts: above the declared fields of an Impl carrier, `0` for every
/// other receiver (the abstract-stamped carrier, a unit-test double).
pub(crate) fn carrier_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let cid = ctx.class_id_of_object(this);
    match ctx.class_name_of_id(cid).as_deref() {
        Some(
            SSL_SOCKET_IMPL
            | SSL_SERVER_SOCKET_IMPL
            | SSL_SOCKET_FACTORY_IMPL
            | SSL_SERVER_SOCKET_FACTORY_IMPL,
        ) => ctx.class_num_total_fields(cid),
        _ => 0,
    }
}

/// Is `factory` one of OUR factory carriers: an `SSLSocketFactoryImpl` / `SSLServerSocketFactoryImpl` object
/// the VM minted (it carries private slots above the class's declared fields), as opposed to one the JDK's
/// own bytecode built? A carrier has no overriding Java bytecode to probe, which is what the old check
/// "its class is the abstract public factory" meant.
pub(crate) fn is_factory_carrier(ctx: &dyn NativeContext, factory: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(factory);
    matches!(
        ctx.class_name_of_id(cid).as_deref(),
        Some(SSL_SOCKET_FACTORY_IMPL | SSL_SERVER_SOCKET_FACTORY_IMPL)
    ) && ctx.object_num_fields(factory) > ctx.class_num_total_fields(cid)
}

/// Is `name` the public class or the Impl of the socket carrier?
pub(crate) fn is_ssl_socket_class(name: &str) -> bool {
    name == SSL_SOCKET_PUBLIC || name == SSL_SOCKET_IMPL
}

include!("tls_carriers_tables.rs");
include!("tls_carriers_fwd.rs");

/// Copy onto `imp` the row each declared method of the old carrier reached.
///
/// `chain` is the public class(es) whose own rows the old carrier ran: the first with a row for the triple
/// wins. A row already on `imp` is kept. Rows on the `java.net` / `javax.net` ancestors are deliberately NOT
/// copied (a later registrar may still add one, `register_io_natives` runs after this): the declared methods
/// with no public-class row get a forwarder instead (`forward_unserved_rows`), which resolves at call time.
/// Returns the triples with no public row.
fn mirror_declared(
    r: &mut NativeMethodRegistry,
    imp: &str,
    chain: &[&str],
    declared: &[(&str, &str)],
) -> Vec<(String, String)> {
    let mut missing = Vec::new();
    for (method, descriptor) in declared {
        if r.resolve_id(imp, method, descriptor).is_some() {
            continue;
        }
        let mut done = false;
        for class in chain {
            let Some(id) = r.resolve_id(class, method, descriptor) else {
                continue;
            };
            let Some(callback) = r.callback_of(id) else {
                continue;
            };
            r.register_with_kind(imp, method, descriptor, callback, NativeKind::SyntheticStub);
            done = true;
            break;
        }
        if !done {
            missing.push((method.to_string(), descriptor.to_string()));
        }
    }
    missing
}

/// Rows for the methods only the Impl classes declare (no public-class counterpart): the carrier has no
/// JDK state, so each answers from what the carrier does have.
fn register_impl_only_rows(r: &mut NativeMethodRegistry) {
    use cratonvm_types::Value;
    let imp = SSL_SOCKET_IMPL;
    // The peer, from the raw slots the `new13_*` sockets write, else the side table a socket from
    // `createSocket(String, int)` keeps (`net_phase_e`).
    fn peer_host(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
        match crate::phases_late::ssl_security::sock_field(
            &*ctx,
            this,
            crate::phases_late::ssl_security::NEW13_SOCK_HOST,
        ) {
            v @ Value::Object(Some(_)) => v,
            _ => {
                let host = crate::net_phase_e::sock_get(&*ctx, this).host;
                if host.is_empty() {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ctx.create_string_uninterned(&host)))
                }
            }
        }
    }
    fn peer_port(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
        match crate::phases_late::ssl_security::sock_field(
            &*ctx,
            this,
            crate::phases_late::ssl_security::NEW13_SOCK_PORT,
        ) {
            Value::Int(p) if p > 0 => p,
            _ => crate::net_phase_e::sock_get(&*ctx, this).port,
        }
    }
    r.register_with_kind(
        imp,
        "getPeerHost",
        "()Ljava/lang/String;",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(Some(Value::Object(None)));
            };
            Ok(Some(peer_host(ctx, this)))
        },
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "getPeerPort",
        "()I",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(Some(Value::Int(-1)));
            };
            Ok(Some(Value::Int(peer_port(ctx, this))))
        },
        NativeKind::SyntheticStub,
    );
    // `SSLSocketImpl.toString()`: `SSLSocket[hostname=H, port=P, Session(T|SUITE)]`.
    r.register_with_kind(
        imp,
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(Some(Value::Object(None)));
            };
            let host = match peer_host(ctx, this) {
                Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_default(),
                _ => "null".to_string(),
            };
            let port = peer_port(ctx, this);
            let session = match ctx.invoke_virtual(
                this,
                "getSession",
                "()Ljavax/net/ssl/SSLSession;",
                &[],
            )? {
                Some(Value::Object(Some(s))) => {
                    match ctx.invoke_virtual(s, "toString", "()Ljava/lang/String;", &[])? {
                        Some(Value::Object(Some(t))) => ctx.read_string(t).unwrap_or_default(),
                        _ => "null".to_string(),
                    }
                }
                _ => "null".to_string(),
            };
            let text = format!("SSLSocket[hostname={host}, port={port}, {session}]");
            Ok(Some(Value::Object(Some(
                ctx.create_string_uninterned(&text),
            ))))
        },
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "setHost",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "doneConnect",
        "()V",
        |_ctx, _args| Ok(None),
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "closeNotify",
        "(Z)V",
        |_ctx, _args| Ok(None),
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "shutdown",
        "()V",
        |ctx, args| ctx.invoke_special(SSL_SOCKET_PUBLIC, "close", "()V", args),
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "isLayered",
        "()Z",
        |_ctx, _args| Ok(Some(cratonvm_types::Value::Int(0))),
        NativeKind::SyntheticStub,
    );
    r.register_with_kind(
        imp,
        "useDelegatedTask",
        "()Z",
        |_ctx, _args| Ok(Some(cratonvm_types::Value::Int(0))),
        NativeKind::SyntheticStub,
    );
}

/// The registration half of the socket / factory carriers. Copies the rows the old carrier reached onto the
/// Impls, then forwards every other declared method to the public class (`forward_unserved_rows`). Runs last
/// in `register_essential_natives_with_shims`; the mirrored rows are `SyntheticStub`s, so `--jdk-only`
/// (where the Impls are the JDK's own objects) drops them, and they go with the carrier.
pub(crate) fn mirror_socket_family_rows_onto_impls(r: &mut NativeMethodRegistry) {
    let prev = r.current_category();
    r.set_category(NativeKind::SyntheticStub);
    let sock_chain = [SSL_SOCKET_PUBLIC];
    for table in [SSL_SOCKET_IMPL_METHODS, BASE_SSL_SOCKET_IMPL_METHODS] {
        mirror_declared(r, SSL_SOCKET_IMPL, &sock_chain, table);
    }
    mirror_declared(
        r,
        SSL_SERVER_SOCKET_IMPL,
        &[SSL_SERVER_SOCKET_PUBLIC],
        SSL_SERVER_SOCKET_IMPL_METHODS,
    );
    mirror_declared(
        r,
        SSL_SOCKET_FACTORY_IMPL,
        &[SSL_SOCKET_FACTORY_PUBLIC],
        SSL_SOCKET_FACTORY_IMPL_METHODS,
    );
    mirror_declared(
        r,
        SSL_SERVER_SOCKET_FACTORY_IMPL,
        &[SSL_SERVER_SOCKET_FACTORY_PUBLIC],
        SSL_SERVER_SOCKET_FACTORY_IMPL_METHODS,
    );
    register_impl_only_rows(r);
    forward_unserved_rows(r);
    r.set_category(prev);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every method the Impl classes declare with `Code` has a row on the Impl: a copy of the row the old
    /// (abstract) carrier reached, or a forwarder to the public class. A method without one runs the
    /// Impl's own bytecode on a carrier that has no `conContext`.
    #[test]
    fn every_declared_impl_method_has_a_row() {
        let mut r = NativeMethodRegistry::new();
        crate::register_essential_natives(&mut r);
        let mut missing = Vec::new();
        for (imp, tables) in [
            (
                SSL_SOCKET_IMPL,
                vec![SSL_SOCKET_IMPL_METHODS, BASE_SSL_SOCKET_IMPL_METHODS],
            ),
            (SSL_SERVER_SOCKET_IMPL, vec![SSL_SERVER_SOCKET_IMPL_METHODS]),
            (
                SSL_SOCKET_FACTORY_IMPL,
                vec![SSL_SOCKET_FACTORY_IMPL_METHODS],
            ),
            (
                SSL_SERVER_SOCKET_FACTORY_IMPL,
                vec![SSL_SERVER_SOCKET_FACTORY_IMPL_METHODS],
            ),
        ] {
            for table in tables {
                for (m, d) in table {
                    if r.resolve_id(imp, m, d).is_none() {
                        missing.push(format!("{imp}.{m}{d}"));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "declared methods with no row: {missing:#?}"
        );
    }

    /// Every row the module adds is a `SyntheticStub`, so `--jdk-only` drops it and the JDK's own object
    /// runs its own bytecode.
    #[test]
    fn mirrored_rows_are_synthetic_stubs() {
        let mut r = NativeMethodRegistry::new();
        crate::register_essential_natives(&mut r);
        for (c, m, d, k) in r.dump_registrations() {
            if matches!(
                c,
                SSL_SOCKET_IMPL
                    | SSL_SERVER_SOCKET_IMPL
                    | SSL_SOCKET_FACTORY_IMPL
                    | SSL_SERVER_SOCKET_FACTORY_IMPL
            ) {
                assert_eq!(k, NativeKind::SyntheticStub, "{c}.{m}{d}");
            }
        }
    }
}
