#[allow(unused_imports)]
use super::*;

/// VM -> global-root handles of the `HandshakeCompletedListener`s of that
/// VM's sockets that died without `close()` (gc-common w16-d). Filled by
/// [`forget_ssl_security_obj_keys`] inside a collection's lock-key sweep, which has no
/// `ctx` to call `remove_global_root` with; drained by
/// [`release_dead_socket_listener_roots`] and dropped with the VM
/// ([`forget_vm_identity_keyed_rows`]; the roots die with it).
///
/// `LockLevel::Scratch` (L0): the sweep pushes under it and the drain takes
/// the VM's row out under it; `remove_global_root` runs after the guard drops.
#[allow(clippy::type_complexity)]
pub fn dead_socket_listener_roots(// addr-keyed: not a heap address -- VM identity -> global-root handles; dropped with the VM (forget_vm_identity_keyed_rows).
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<usize, Vec<usize>>> {
    static T: std::sync::OnceLock<
        // addr-keyed: the accessor's table (see above).
        cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<usize, Vec<usize>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Release the global roots [`forget_ssl_security_obj_keys`] queued for the
/// calling VM (gc-common w16-d). Called on entry to the `SSLSocket` natives a
/// program that registers listeners keeps calling: the zero-arg
/// `createSocket()`, `connect`, `add`/`removeHandshakeCompletedListener` and
/// `close`. A dead socket's listeners therefore stay reachable for at most
/// one such call after the collection that found the socket dead.
pub fn release_dead_socket_listener_roots(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    let handles = {
        let mut queued = dead_socket_listener_roots().lock();
        if queued.is_empty() {
            return;
        }
        match queued.remove(&vm) {
            Some(handles) => handles,
            None => return,
        }
    };
    for handle in handles {
        ctx.remove_global_root(handle);
    }
}

// ---------------------------------------------------------------------------
// The handshake-listener SLOT (gcd d1/e, 2026-09-27)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/native-dispatch-architecture.md`,
// option 2. JSSE keeps a socket's `HandshakeCompletedListener`s in a field of
// the socket, so a listener that captures its socket forms an ordinary cycle
// that dies with the socket. This VM kept them under STRONG global roots in
// `handshake_listeners`, and such a listener kept its socket -- and the
// socket's rows, staged trust scope and parked connection -- alive for the
// life of the VM.
//
// Every synthetic `SSLSocket` now carries one more slot, its LAST
// ([`alloc_ssl_socket`], called at all six allocation sites:
// `new13_do_create_socket`, the zero-arg `createSocket()`, the
// layered-handshake socket, `net_phase_e`'s probe-mode and connected
// `createSocket(String, int)`, and `t27_tls`'s accepted socket). The slot sits above both the natives' fixed slot maps (widths 5 and
// 6) and, on a real JDK, `java.net.Socket`'s six declared fields. It holds
// either [`SSL_SOCK_NO_LISTENERS`] or a `HandshakeCompletedListener[]`,
// replaced (never mutated) on every add and remove, so a delivery iterating
// the old array is unaffected by a listener that edits the set.
//
// Why the collectors trace it: the socket is `base + width + 1` slots wide,
// which matches no compact layout's field count (`plan_object_alloc` requires
// equality), so it is a LEGACY object, whose slots are 16-byte `Value` cells
// every collector traces by tag -- the same shape every other appended-slot
// carrier in this crate has (`alloc_tls_stream`). On a real JDK this moves the
// socket from the compact 6-field layout to the legacy one; field access by
// index round-trips identically on both (the compact store boxes a primitive
// written into a reference field, `crate::autobox`), so no native's reads
// change.
//
// A socket without the slot (a foreign receiver, a unit-test double) keeps
// the global-root side table.

/// The class every synthetic socket is an instance of.
pub const SSL_SOCKET_CLASS: &str = "javax/net/ssl/SSLSocket";

/// The listener slot's value while its socket has no listener: a marker, not
/// the slot's zero default, so that [`ssl_sock_listener_slot`] can tell the
/// slot from a field some other allocator left untouched.
pub const SSL_SOCK_NO_LISTENERS: i32 = 0x4C53_4E52;

/// The component of the listener array (never visible to Java; `Object` when
/// the interface is not loaded).
pub fn ssl_listener_component(ctx: &mut dyn NativeContext) -> ClassId {
    crate::lang_class::reflection_component_id(ctx, "javax/net/ssl/HandshakeCompletedListener")
}

/// Allocate a synthetic `javax/net/ssl/SSLSocket` whose natives use slots
/// `0..width`, plus the listener slot as its LAST slot, stamped
/// [`SSL_SOCK_NO_LISTENERS`].
///
/// The request is `max(width, base) + 1`, where `base` is the real class's
/// field count (0 for a compatibility stub): the last slot is then above every
/// slot a native or a real `java.net.Socket` field uses. The allocation funnel
/// may widen a stub further; the listener slot is still the last one.
#[track_caller]
pub fn alloc_ssl_socket(
    ctx: &mut dyn NativeContext,
    width: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // `--compatible` mints the JDK's own `SSLSocketImpl` (`crate::tls_carriers`): the private slots then
    // start above its declared fields (`sock_base`), so the request is `real + width + 1`.
    let sock = if crate::tls_carriers::socket_impl_enabled()
        && !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkJsse)
        && cratonvm_native_api::instantiable::first_instantiable(
            ctx,
            &[crate::tls_carriers::SSL_SOCKET_IMPL],
        )
        .is_some()
        && crate::appended_slot_base_for_class(ctx, crate::tls_carriers::SSL_SOCKET_IMPL) > 0
    {
        let real = crate::appended_slot_base_for_class(ctx, crate::tls_carriers::SSL_SOCKET_IMPL);
        let sock = try_alloc_concurrent_synthetic(
            ctx,
            crate::tls_carriers::SSL_SOCKET_IMPL,
            real + width + 1,
        )?;
        // The abstract carrier's slots 0..width were the declared references of `java.net.Socket`, so an
        // unwritten one read back as null and every reader's `as_int()` / `Object(Some(..))` test fell
        // through to the side table. A slot above the declared fields is untyped and defaults to
        // `Int(0)`, which `new13_resolve_tls_id` would take for stream id 0: null them.
        for slot in 0..width {
            ctx.set_field(sock, real + slot, Value::Object(None));
        }
        sock
    } else {
        let base = crate::appended_slot_base_for_class(ctx, SSL_SOCKET_CLASS);
        try_alloc_concurrent_synthetic(ctx, SSL_SOCKET_CLASS, width.max(base) + 1)?
    };
    let n = ctx.object_num_fields(sock);
    if n > width {
        ctx.set_field(sock, n - 1, Value::Int(SSL_SOCK_NO_LISTENERS));
    }
    crate::net_phase_e::re1_init_socket_locks(ctx, sock)
}

/// The index of `sock`'s listener slot, or `None` for a socket
/// [`alloc_ssl_socket`] did not build. Only a receiver of the exact (abstract)
/// class `javax/net/ssl/SSLSocket` qualifies -- natives are the only code that
/// can instantiate it -- and only when its last slot holds the marker or an
/// array.
pub fn ssl_sock_listener_slot(ctx: &dyn NativeContext, sock: ObjectRef) -> Option<usize> {
    if !ctx
        .class_name_of_id(ctx.class_id_of_object(sock))
        .as_deref()
        .is_some_and(crate::tls_carriers::is_ssl_socket_class)
    {
        return None;
    }
    let n = ctx.object_num_fields(sock);
    if n == 0 {
        return None;
    }
    match ctx.get_field(sock, n - 1) {
        Value::Int(SSL_SOCK_NO_LISTENERS) => Some(n - 1),
        Value::Object(Some(arr)) if ctx.object_is_array(arr) => Some(n - 1),
        _ => None,
    }
}

/// Drop every listener in `sock`'s slot (`close()`): the marker goes back.
pub fn ssl_sock_slot_clear(ctx: &mut dyn NativeContext, sock: ObjectRef, slot: usize) {
    ctx.set_field(sock, slot, Value::Int(SSL_SOCK_NO_LISTENERS));
}

/// The listener array `sock`'s slot holds; `None` for the marker.
pub fn ssl_sock_slot_array(ctx: &dyn NativeContext, sock: ObjectRef, slot: usize) -> Option<ObjectRef> {
    match ctx.get_field(sock, slot) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    }
}

/// The position of `listener` in the array `sock`'s slot holds, if any.
/// Identity, as the side table's weak-lock-key match was.
pub fn ssl_sock_slot_position(
    ctx: &dyn NativeContext,
    sock: ObjectRef,
    slot: usize,
    listener: ObjectRef,
) -> Option<usize> {
    let Value::Object(Some(arr)) = ctx.get_field(sock, slot) else {
        return None;
    };
    (0..ctx.array_length(arr))
        .find(|&i| matches!(ctx.get_array_element(arr, i), Value::Object(Some(l)) if l == listener))
}

/// `addHandshakeCompletedListener` on a socket with a listener slot:
/// idempotent for a listener already there (JSSE keeps a `HashSet`), otherwise
/// the slot gets a new array one longer. Can collect (the array allocation's
/// reclaim-and-retry), which is why `sock` and `listener` travel through
/// `live`; the caller must not use its own copies of either afterwards (both
/// callers return straight away).
pub fn ssl_sock_slot_add(
    ctx: &mut dyn NativeContext,
    sock: ObjectRef,
    slot: usize,
    listener: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if ssl_sock_slot_position(&*ctx, sock, slot, listener).is_some() {
        return Ok(());
    }
    let old = match ctx.get_field(sock, slot) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let len = old.map_or(0, |arr| ctx.array_length(arr));
    let component = ssl_listener_component(ctx);
    // `old` rides as the third entry only when there is one.
    let mut live = [sock, listener, old.unwrap_or(sock)];
    let grown = crate::new_ref_array_reclaiming(
        ctx,
        component,
        len + 1,
        &mut live,
        "HandshakeCompletedListener[]",
    )?;
    let [sock, listener, old_now] = live;
    if old.is_some() {
        for i in 0..len {
            let v = ctx.get_array_element(old_now, i);
            ctx.set_array_element(grown, i, v);
        }
    }
    ctx.set_array_element(grown, len, Value::Object(Some(listener)));
    ctx.set_field(sock, slot, Value::Object(Some(grown)));
    Ok(())
}

/// `removeHandshakeCompletedListener` on a socket with a listener slot.
/// `Ok(false)` when `listener` is not registered (the caller throws, as JSSE
/// does). The slot goes back to the marker when the last one leaves. Can
/// collect, like [`ssl_sock_slot_add`], with the same rule for the caller.
pub fn ssl_sock_slot_remove(
    ctx: &mut dyn NativeContext,
    sock: ObjectRef,
    slot: usize,
    listener: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let Some(pos) = ssl_sock_slot_position(&*ctx, sock, slot, listener) else {
        return Ok(false);
    };
    let Value::Object(Some(arr)) = ctx.get_field(sock, slot) else {
        return Ok(false);
    };
    let len = ctx.array_length(arr);
    if len <= 1 {
        ctx.set_field(sock, slot, Value::Int(SSL_SOCK_NO_LISTENERS));
        return Ok(true);
    }
    let component = ssl_listener_component(ctx);
    let mut live = [sock, arr];
    let shrunk = crate::new_ref_array_reclaiming(
        ctx,
        component,
        len - 1,
        &mut live,
        "HandshakeCompletedListener[]",
    )?;
    let [sock, arr] = live;
    let mut j = 0;
    for i in (0..len).filter(|&i| i != pos) {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(shrunk, j, v);
        j += 1;
    }
    ctx.set_field(sock, slot, Value::Object(Some(shrunk)));
    Ok(true)
}

/// The `endpointIdentificationAlgorithm` a caller set through
/// `SSLSocket.setSSLParameters`, by the same identity-hash key as
/// [`ssl_sock_auth_state`].
///
/// `SSLParameters` is CONFIGURATION and the JDK round-trips all of it. This
/// VM read the cipher suites out of the object and dropped everything else, so
/// `setSSLParameters(p); getSSLParameters()` lost the endpoint-identification
/// algorithm and the client-auth flags — the caller could not read back what
/// it had just set. MEASURED, `L6TlsParamSweep` row 83.
///
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0). Both acquisition sites do
/// one map operation on an `i32` key computed ABOVE the acquisition, so the
/// guard is never held across a re-entry into the VM. A raw `parking_lot`
/// lock here would be a new unordered global in the one crate whose natives
/// call back into Java; the ratchet in
/// `native-builtins/tests/lock_discipline_ratchet.rs` says so and is right.
///
/// gc-common w15-d: keyed [`ssl_sock_key`] and dropped with its socket and
/// its VM, like [`ssl_sock_auth_state`] (w27-a: by [`SslObjKey`]).
#[allow(clippy::type_complexity)]
pub fn ssl_sock_endpoint_alg(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<SslObjKey, String>> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<SslObjKey, String>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

pub fn ssl_sock_auth_update<F: FnOnce(&mut (i32, i32, i32))>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: F,
) {
    // `this` is the CURRENT address at every caller (a native's argument, or
    // a pin re-read), so its lock key can be minted from it; the row goes
    // with that key (gc-common w27-a).
    let key = ssl_sock_key(ctx, this);
    let mut table = ssl_sock_auth_state().lock();
    let entry = table.entry(key).or_insert((0, 0, 0));
    f(entry);
}

/// The `KeyManagerFactory`/`TrustManagerFactory` instances a caller has
/// successfully `init`ed, by identity hash.
///
/// Neither class has a spare slot to put the flag in — slot 1 of both is the
/// real class's `factorySpi`, which `jsse_factory_is_ours` reads and this file
/// therefore never writes (see its note) — so the state lives beside the two
/// id tables this module already keys the same way.
///
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Every acquisition takes
/// the guard after the key has been computed, does one set operation, and
/// drops it before anything re-enters Java — the same shape, and the same
/// level, as this module's two id tables.
///
/// gc-common w15-d: keyed [`FactoryIdentity`] like those id tables since
/// w10-e. It was the bare hash and never pruned, so VM B's never-`init`ed
/// factory that drew the same hash as an `init`ed factory of VM A passed the
/// `IllegalStateException` gate, and every factory the process ever `init`ed
/// kept its row. The row now goes with its factory and its VM
/// ([`forget_vm_factory_identity_rows`]). gc-common w27-a: by the factory's
/// weak lock key, so a never-`init`ed factory of the SAME VM that shares an
/// `init`ed one's hash no longer passes the gate either.
pub fn jsse_factory_initialized(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<FactoryIdentity>>
{
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<FactoryIdentity>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashSet::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// `this` must be the factory's CURRENT address (its lock key is minted from
/// it).
pub fn mark_jsse_factory_initialized(ctx: &dyn NativeContext, this: ObjectRef) {
    let key = ssl_obj_key(ctx, this);
    jsse_factory_initialized().lock().insert(key);
}

/// Refuse `getKeyManagers()`/`getTrustManagers()` on a factory nobody `init`ed.
///
/// MEASURED on HotSpot 25.0.4+7 — both are
/// `IllegalStateException: <Impl> is not initialized`, and this VM answered
/// with one manager built from whatever keystore state happened to be lying
/// around. That is the same shape as the `SSLContext` gate two files over: a
/// caller whose `init` never ran, or threw and was swallowed, got a working
/// object with nobody's configuration in it.
///
/// `impl_name` is the JDK's own class name in the message —
/// `KeyManagerFactoryImpl` / `TrustManagerFactoryImpl` — not the interface's.
pub fn require_jsse_factory_initialized(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    impl_name: &str,
) -> Result<(), MethodCallFailed> {
    if let Some(key) = existing_ssl_obj_key(ctx, this) {
        let initialized = jsse_factory_initialized().lock().contains(&key);
        if initialized {
            return Ok(());
        }
    }
    Err(cratonvm_types::error::RuntimeError::IllegalStateException {
        message: format!("{impl_name} is not initialized"),
    }
    .into())
}

