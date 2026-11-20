// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

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
//! Page: `docs/feature-designs/native-dispatch-architecture.md`.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::ObjectRef;

use crate::{appended_slot_base_for_class, try_alloc_concurrent_synthetic};

pub const SSL_SOCKET_IMPL: &str = "sun/security/ssl/SSLSocketImpl";
pub const SSL_SERVER_SOCKET_IMPL: &str = "sun/security/ssl/SSLServerSocketImpl";
pub const SSL_SOCKET_FACTORY_IMPL: &str = "sun/security/ssl/SSLSocketFactoryImpl";
pub const SSL_SERVER_SOCKET_FACTORY_IMPL: &str =
    "sun/security/ssl/SSLServerSocketFactoryImpl";

pub const SSL_SOCKET_PUBLIC: &str = "javax/net/ssl/SSLSocket";
pub const SSL_SERVER_SOCKET_PUBLIC: &str = "javax/net/ssl/SSLServerSocket";
pub const SSL_SOCKET_FACTORY_PUBLIC: &str = "javax/net/ssl/SSLSocketFactory";
pub const SSL_SERVER_SOCKET_FACTORY_PUBLIC: &str = "javax/net/ssl/SSLServerSocketFactory";

/// Is the carrier minted as the Impl class (the default)?
pub fn socket_impl_enabled() -> bool {
    true
}

/// Mint a carrier of private-slot width `width`: the Impl when the image has it and the switch is on,
/// the public (abstract) class otherwise. The allocation is `real + width` slots so the private slots sit
/// above every declared field (`carrier_base`).
#[track_caller]
pub fn alloc_tls_carrier(
    ctx: &mut dyn NativeContext,
    public: &str,
    imp: &str,
    width: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Only where the mirrored rows exist (`SyntheticStub`s, dropped under `--real-jdk` / `jdk-jsse`).
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
pub fn carrier_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
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
pub fn is_factory_carrier(ctx: &dyn NativeContext, factory: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(factory);
    matches!(
        ctx.class_name_of_id(cid).as_deref(),
        Some(SSL_SOCKET_FACTORY_IMPL | SSL_SERVER_SOCKET_FACTORY_IMPL)
    ) && ctx.object_num_fields(factory) > ctx.class_num_total_fields(cid)
}

/// Is `name` the public class or the Impl of the socket carrier?
pub fn is_ssl_socket_class(name: &str) -> bool {
    name == SSL_SOCKET_PUBLIC || name == SSL_SOCKET_IMPL
}

mod tables;
#[allow(unused_imports)]
use tables::*;
#[macro_use]
mod fwd;
#[allow(unused_imports)]
use fwd::*;

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
            r.register(imp, method, descriptor, callback, NativeKind::Intrinsic);
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
    r.register(
        imp,
        "getPeerHost",
        "()Ljava/lang/String;",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(Some(Value::Object(None)));
            };
            Ok(Some(peer_host(ctx, this)))
        },
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "getPeerPort",
        "()I",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(Some(Value::Int(-1)));
            };
            Ok(Some(Value::Int(peer_port(ctx, this))))
        },
        NativeKind::Intrinsic,
    );
    // `SSLSocketImpl.toString()`: `SSLSocket[hostname=H, port=P, Session(T|SUITE)]`.
    r.register(
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
                        _ => "null".to_string(),                    }                }
                _ => "null".to_string(),
            };
            let text = format!("SSLSocket[hostname={host}, port={port}, {session}]");
            Ok(Some(Value::Object(Some(
                ctx.create_string_uninterned(&text),
            ))))
        },
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "setHost",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "doneConnect",
        "()V",
        |_ctx, _args| Ok(None),
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "closeNotify",
        "(Z)V",
        |_ctx, _args| Ok(None),
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "shutdown",
        "()V",
        |ctx, args| ctx.invoke_special(SSL_SOCKET_PUBLIC, "close", "()V", args),
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "isLayered",
        "()Z",
        |_ctx, _args| Ok(Some(cratonvm_types::Value::Int(0))),
        NativeKind::Intrinsic,
    );
    r.register(
        imp,
        "useDelegatedTask",
        "()Z",
        |_ctx, _args| Ok(Some(cratonvm_types::Value::Int(0))),
        NativeKind::Intrinsic,
    );
}

/// The registration half of the socket / factory carriers. Copies the rows the old carrier reached onto the
/// Impls, then forwards every other declared method to the public class (`forward_unserved_rows`). Runs last
/// in `register_essential_natives_with_shims`; the mirrored rows are `SyntheticStub`s, so `--real-jdk`
/// (where the Impls are the JDK's own objects) drops them, and they go with the carrier.
pub fn mirror_socket_family_rows_onto_impls(r: &mut NativeMethodRegistry) {
    if r.aligned(cratonvm_types::HotSpotAlignment::JdkJsse) {
        return;
    }
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
    }

#[cfg(test)]
mod tests;
