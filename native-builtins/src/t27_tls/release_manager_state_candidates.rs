#[allow(unused_imports)]
use super::*;

/// Free VM `vm`'s offered manager states that no live carrier and no factory
/// row names any more; answer how many rows went. End of
/// [`gc_sweep_tls_rows`]; locks one at a time.
pub fn release_manager_state_candidates(vm: usize) -> usize {
    let mine: Vec<(TlsOwnerKind, i32)> = {
        let mut candidates = manager_state_candidates().lock();
        if candidates.is_empty() {
            return 0;
        }
        let mine = candidates
            .iter()
            .filter(|(_, _, owner)| *owner == vm)
            .map(|(kind, id, _)| (*kind, *id))
            .collect();
        candidates.retain(|(_, _, owner)| *owner != vm);
        mine
    };
    let carried: std::collections::HashSet<(TlsOwnerKind, i32)> = {
        let owners = tls_weak_owners().lock();
        mine.iter()
            .copied()
            .filter(|(kind, id)| owners.contains_key(&(*kind, *id as u32 as u64)))
            .collect()
    };
    let mut dropped = 0usize;
    for (kind, id) in mine {
        // A live carrier offers the id again when it dies; a factory row does
        // when it is dropped or overwritten.
        if carried.contains(&(kind, id)) {
            continue;
        }
        let is_km = kind == TlsOwnerKind::KeyManagerState;
        if crate::phases_late::ssl_security::factory_rows_name_manager_id(vm, is_km, id) {
            continue;
        }
        dropped += if is_km {
            crate::x509_manager::forget_key_manager_state(vm, id)
        } else {
            crate::x509_manager::forget_trust_manager_state(vm, id)
        };
    }
    dropped
}

/// Drop VM `vm`'s rows of every `ObjectRef`-holding TLS table and slot (VM
/// teardown, from `lib.rs::forget_vm_native_root_stores`; gc-common w9-b).
/// Runs BEFORE `forget_vm_lock_keys`, because an `SSLContext` key's owner is
/// read from the lock-key registry. Rows whose owner is unknown are kept: they
/// may be another VM's. Idempotent.
pub fn forget_vm_tls_state(vm: usize) {
    forget_vm_tls_weak_owners(vm);
    session_id_order().lock().remove(&vm);
    let owned_ctx = |key: &u64| crate::lock_key_owner_vm(*key) == Some(vm);
    ctx_trust_managers_table()
        .lock()
        .retain(|k, _| !owned_ctx(k));
    ctx_key_managers_table().lock().retain(|k, _| !owned_ctx(k));
    client_session_cache()
        .lock()
        .retain(|(k, _, _), _| !owned_ctx(k));
    engine_sni_matchers_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    engine_alpn_selector_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    engine_session_table()
        .lock()
        .retain(|(k, _), _| !objref_key_owned_by(*k, vm));
    session_id_registry()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    forget_vm_huc_captures(vm);
    DEFAULT_SSL_CONTEXT.forget(vm);
    HUC_DEFAULT_FACTORY.forget(vm);
    // gc-common w11-a: the runtime server identity (a certificate chain and
    // its private key) is one per VM now, and goes with it.
    RUNTIME_TLS_IDENTITY.forget(vm);
    // gc-common w10-e: the byte-only tables keyed the same way. None holds an
    // `ObjectRef`, but every one grew by a row per `SSLContext` / engine of
    // every VM the process ever ran; the engine rows carry a whole rustls
    // connection with its buffers.
    ctx_identity_table().lock().retain(|k, _| !owned_ctx(k));
    ctx_trust_roots_table().lock().retain(|k, _| !owned_ctx(k));
    ctx_accepted_issuers_table()
        .lock()
        .retain(|k, _| !owned_ctx(k));
    ctx_jsse_identifies_table()
        .lock()
        .retain(|k, _| !owned_ctx(k));
    ctx_client_session_store_table()
        .lock()
        .retain(|k, _| !owned_ctx(k));
    ctx_server_session_store_table()
        .lock()
        .retain(|(k, _), _| !owned_ctx(k));
    // gc-common w13-a: the cached per-shape `ClientConfig`s were the one
    // `SSLContext`-keyed table teardown missed.
    ctx_client_config_table()
        .lock()
        .retain(|(k, _), _| !owned_ctx(k));
    deferred_client_auth_contexts()
        .lock()
        .retain(|k| !owned_ctx(k));
    let dead_engines: Vec<i32> = {
        let mut table = engine_table().lock();
        let ids = table
            .iter()
            .filter(|(k, _)| objref_key_owned_by(**k, vm))
            .map(|(_, id)| *id)
            .collect();
        table.retain(|k, _| !objref_key_owned_by(*k, vm));
        ids
    };
    if !dead_engines.is_empty() {
        let mut registry = engine_registry().write();
        for id in dead_engines {
            registry.remove(&id);
        }
    }
    // gc-common w11-a: the byte-only SESSION side tables are keyed the same
    // way and were not dropped at teardown either.
    session_peer_certs_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    session_local_certs_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    session_peer_endpoint_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    session_sni_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    session_invalidated_table()
        .lock()
        .retain(|k| !objref_key_owned_by(*k, vm));
    session_wire_id_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    session_last_accessed_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    negotiated_session_keys()
        .lock()
        .retain(|k| !objref_key_owned_by(*k, vm));
    sslparams_alpn_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    // gc-common w26-b: the socket tables teardown missed. (The lock-key
    // teardown that follows frees every key of the VM and drops these rows
    // too, through `forget_tls_object_keys`; this keeps the teardown of this
    // module's own tables in one place.)
    sss_mode_states()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    sss_enabled_suites_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    sss_enabled_protocols_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    sock_alpn_table()
        .lock()
        .retain(|k, _| !objref_key_owned_by(*k, vm));
    // The factory / keystore registries the TLS managers read (x509_manager,
    // keystore, phases_late::ssl_security) are torn down with the rest of the
    // row, so `forget_vm_native_root_stores` keeps one TLS entry.
    crate::x509_manager::forget_vm_trust_manager_states(vm);
    crate::keystore::forget_vm_keystores(vm);
    crate::phases_late::ssl_security::forget_vm_factory_identity_rows(vm);
    // gc-common w15-d handoff: the "not initialised" `SSLContext` marks.
    crate::jca::ssl_context_spi::forget_vm_uninitialized_contexts(vm);
}

/// Remove the rows of `map` filed under any of `keys`, walking whichever side
/// is smaller, and hand them back (so a caller can finish with them outside
/// the guard).
pub fn take_keyed_rows<V>(map: &mut HashMap<u64, V>, keys: &std::collections::HashSet<u64>) -> Vec<V> {
    if map.is_empty() {
        return Vec::new();
    }
    if map.len() <= keys.len() {
        let hit: Vec<u64> = map.keys().filter(|k| keys.contains(*k)).copied().collect();
        hit.into_iter().filter_map(|k| map.remove(&k)).collect()
    } else {
        keys.iter().filter_map(|k| map.remove(k)).collect()
    }
}

/// [`take_keyed_rows`] for a set of marks.
pub fn take_keyed_marks(
    set: &mut std::collections::HashSet<u64>,
    keys: &std::collections::HashSet<u64>,
) {
    if set.is_empty() {
        return;
    }
    if set.len() <= keys.len() {
        set.retain(|k| !keys.contains(k));
    } else {
        for k in keys {
            set.remove(k);
        }
    }
}

/// gc-common w26-b (`common-w25a-t27-tls-engine-rows-merge-live-identity-hash-colliders`):
/// drop every row this module filed under an object key
/// ([`gc_stable_objref_key`]) that the lock-key registry has just FREED --
/// its object died (`lib.rs::sweep_lock_keys`, after every collection) or its
/// VM went (`lib.rs::forget_vm_lock_keys`). A freed key is never minted
/// again, so nothing could read these rows any more; without this they would
/// stay for the life of the process. `keys` holds every weak key the sweep
/// freed, of every kind -- most name no row here.
///
/// Covers the tables the weak-owner sweep ([`gc_sweep_tls_rows`]) does not
/// (the `SSLServerSocket` / `SSLSocket` state tables, and session rows of a
/// session no constructor recorded), and is a no-op repeat for the ones it
/// does: the TLS sweep runs first at all three sites, and dropping a row
/// twice is harmless. The per-VM `HttpsURLConnection` instance captures are
/// the weak-owner sweep's alone (a freed key does not say its VM).
///
/// Every table lock is taken on its own, never nested, and never under the
/// lock-key registry's (both callers release it before calling here). Nothing
/// here reaches Java.
pub fn forget_tls_object_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let keys: std::collections::HashSet<u64> = keys.iter().map(|k| *k as u64).collect();
    // `SSLServerSocket` / `SSLSocket` rows.
    take_keyed_rows(&mut *ssl_server_socket_states().lock(), &keys);
    take_keyed_rows(&mut *sss_client_auth_states().lock(), &keys);
    take_keyed_rows(&mut *sss_mode_states().lock(), &keys);
    take_keyed_rows(&mut *sss_enabled_suites_table().lock(), &keys);
    take_keyed_rows(&mut *sss_enabled_protocols_table().lock(), &keys);
    take_keyed_rows(&mut *sock_alpn_table().lock(), &keys);
    // `SSLEngine` rows: the rustls state, then the cells it indexes.
    let engine_ids = take_keyed_rows(&mut *engine_table().lock(), &keys);
    if !engine_ids.is_empty() {
        let cells: Vec<EngineCell> = {
            let mut registry = engine_registry().write();
            engine_ids
                .iter()
                .filter_map(|id| registry.remove(id))
                .collect()
        };
        // The rustls connections go outside the registry's write guard.
        drop(cells);
    }
    {
        let mut sessions = engine_session_table().lock();
        if !sessions.is_empty() {
            if sessions.len() <= keys.len() * 3 {
                sessions.retain(|(k, _), _| !keys.contains(k));
            } else {
                for k in &keys {
                    for phase in [
                        SessionPhase::Fresh,
                        SessionPhase::Handshaking,
                        SessionPhase::Negotiated,
                    ] {
                        sessions.remove(&(*k, phase));
                    }
                }
            }
        }
    }
    take_keyed_rows(&mut *engine_alpn_selector_table().lock(), &keys);
    take_keyed_marks(&mut *alpn_selector_unrooted().lock(), &keys);
    take_keyed_rows(&mut *engine_sni_matchers_table().lock(), &keys);
    {
        // The engine no longer holds its `SSLContext`'s rows.
        let mut holders = ctx_key_holders().lock();
        if !holders.engine_ctx.is_empty() {
            let held: Vec<u64> = if holders.engine_ctx.len() <= keys.len() {
                holders
                    .engine_ctx
                    .keys()
                    .filter(|k| keys.contains(*k))
                    .copied()
                    .collect()
            } else {
                keys.iter()
                    .filter(|k| holders.engine_ctx.contains_key(*k))
                    .copied()
                    .collect()
            };
            for k in held {
                holders.release_engine(k);
            }
        }
    }
    // `SSLParameters` rows.
    take_keyed_rows(&mut *sslparams_alpn_table().lock(), &keys);
    // `SSLSession` rows.
    take_keyed_rows(&mut *session_peer_certs_table().lock(), &keys);
    take_keyed_rows(&mut *session_local_certs_table().lock(), &keys);
    take_keyed_rows(&mut *session_peer_endpoint_table().lock(), &keys);
    take_keyed_rows(&mut *session_sni_table().lock(), &keys);
    take_keyed_marks(&mut *session_invalidated_table().lock(), &keys);
    take_keyed_rows(&mut *session_wire_id_table().lock(), &keys);
    take_keyed_rows(&mut *session_last_accessed_table().lock(), &keys);
    take_keyed_marks(&mut *negotiated_session_keys().lock(), &keys);
}

/// SBR wave 4 (lane SSL): `true` when this VM runs `--real-jdk` AND the JSSE
/// family's shadows are retired (`RETIRED_SHADOW_SBR_JSSE_TRIPLES`, wave 5 lane
/// JSSE; wave 4 wrote this and left it inert), i.e. every
/// `SSLContext` / `SSLSocketFactory` / `SSLSocket` a Java caller can hold is the
/// JDK's own object and not one of this module's carriers. Keyed on the rows
/// being retired, not on the mode alone, so `CRATONVM_UNRETIRE_NATIVE_SHADOW`
/// restores the pre-wave VM with them (the kill switch's contract). The
/// retirement half is a pure function of the build and the environment, so it
/// is computed once; the mode half is per VM.
pub fn real_jsse_family(ctx: &dyn cratonvm_native_api::NativeContext) -> bool {
    static RETIRED: OnceLock<bool> = OnceLock::new();
    // `--real-jdk`, or `--compatible` with the `JdkJsse` row (SBR round 5, lane
    // compat2): the JDK's own JSSE objects over real keystores.
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkJsse)
        && *RETIRED.get_or_init(|| {
            cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
                "javax/net/ssl/SSLContext",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
            )
        })
}

/// SBR wave 4 (lane SSL): the real `sun.security.ssl.SSLContextImpl` behind a
/// REAL JSSE socket factory (`SSLSocketFactoryImpl.context`), found through
/// any wrapper factory the application put in front of it (a bounded walk of
/// object fields, arrays skipped). `None` for this module's carriers.
pub fn real_jsse_context_of_factory(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    factory: ObjectRef,
) -> Option<ObjectRef> {
    const MAX_DEPTH: usize = 4;
    const MAX_VISITED: usize = 32;
    let mut frontier = vec![factory];
    let mut visited = 0usize;
    for _ in 0..MAX_DEPTH {
        let mut next = Vec::new();
        for obj in frontier {
            if visited >= MAX_VISITED {
                return None;
            }
            visited += 1;
            if ctx.object_is_array(obj) {
                continue;
            }
            let cid = ctx.class_id_of_object(obj);
            let name = ctx.class_name_of_id(cid).unwrap_or_default();
            if name.starts_with("sun/security/ssl/SSLContextImpl") {
                return Some(obj);
            }
            let declared = ctx.class_num_total_fields(cid);
            for i in 0..declared {
                if let Value::Object(Some(child)) = ctx.get_field(obj, i) {
                    next.push(child);
                }
            }
        }
        if next.is_empty() {
            return None;
        }
        frontier = next;    }
    None
}

/// SBR wave 4 (lane SSL): file a real `SSLContextImpl`'s own trust and key
/// managers (`trustManager`, `keyManager`) in the tables `SSLContext.init`'s
/// native used to fill, so the native `HttpsURLConnection` client -- which is
/// held, and runs its own rustls connection -- verifies the peer with the
/// application's `TrustManager` exactly as it does for a carrier context.
/// JSSE's placeholder `DummyX509KeyManager` is not filed (no client identity).
pub fn attach_real_jsse_managers(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    impl_ctx: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let pin = ctx.pin_native_root(*impl_ctx);
    let result = (|| -> Result<(), MethodCallFailed> {
        for (field, iface, is_tm) in [
            ("trustManager", "javax/net/ssl/TrustManager", true),
            ("keyManager", "javax/net/ssl/KeyManager", false),
        ] {
            let owner = ctx.read_native_pin(pin, *impl_ctx);
            let Value::Object(Some(manager)) = ctx.get_field_by_name(owner, field) else {
                continue;
            };
            let mname = ctx
                .class_name_of_id(ctx.class_id_of_object(manager))
                .unwrap_or_default();
            if mname == "sun/security/ssl/DummyX509KeyManager" {
                continue;
            }
            let Some(iface_id) = ctx.class_id_by_name(iface) else {
                continue;
            };
            let manager_pin = ctx.pin_native_root(manager);
            let arr = ctx.new_ref_array(iface_id, 1);
            let manager = ctx.read_native_pin(manager_pin, manager);
            ctx.set_array_element(arr, 0, Value::Object(Some(manager)));
            let owner = ctx.read_native_pin(pin, *impl_ctx);
            if is_tm {
                attach_trust_managers_to_ctx(ctx, owner, Some(arr))?;
            } else {
                attach_key_managers_to_ctx(ctx, owner, Some(arr))?;
            }
        }
        Ok(())
    })();
    *impl_ctx = ctx.read_native_pin(pin, *impl_ctx);
    ctx.unpin_native_roots(pin);
    result
}

/// The process-wide default `SSLContext`, created and cached on first use.
///
/// This is the JDK's documented `SSLContext.getDefault()` lazy-init contract
/// ("the default context is created if it is not yet created"). It lived
/// inline in exactly one caller — `phases_late::ssl_security`'s
/// `SSLSocketFactory.getDefault()` registration — while three other natives
/// that also hand back a `javax/net/ssl/SSLSocketFactory` minted a *bare*
/// carrier instead. The layered
/// `SSLSocketFactory.createSocket(Socket,String,int,boolean)` overload reads
/// the owning context out of the carrier's field 0, so every one of those
/// bare factories threw
/// `IllegalStateException: SSLSocketFactory has no owning SSLContext`
/// instead of connecting. Converting the idiom rather than each site is what
/// keeps a future fourth caller from re-introducing it. See
/// `sslsocketfactory-getdefault-aether-resolution-regression-20260804-FIXED.md`.
pub fn default_ssl_context_or_create(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if let Some(existing) = get_runtime_default_ssl_context(vm) {
        return Ok(existing);
    }
    // gc-common w15-d handoff: the String first, rooted, then the context,
    // so the address installed in the (rooted, remapped) per-VM slot is the
    // one after the last GC point.
    let name = ctx.create_string("TLS");
    let name_pin = ctx.pin_native_root(name);
    let new_ctx = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
    let name = ctx.read_native_pin(name_pin, name);
    ctx.unpin_native_roots(name_pin);
    let new_ctx = new_ctx?;
    ctx.set_field(new_ctx, 0, Value::Object(Some(name)));
    ctx.set_field(new_ctx, 1, Value::Int(1));
    // nio21 wave 13 (L4): the default context trusts the default trust store.
    let new_ctx = scope_default_context_trust(ctx, new_ctx)?;
    set_runtime_default_ssl_context(vm, new_ctx);
    Ok(new_ctx)
}

/// Mint the object `SSLSocketFactory.getDefault()` hands back: the same
/// 1-slot synthetic carrier `SSLContext.getSocketFactory()` returns, with
/// field 0 set to the process default `SSLContext`.
///
/// `HttpsURLConnection`'s default/instance factory getters use it too — the
/// JDK documents both as defaulting to `SSLSocketFactory.getDefault()`.
pub fn default_ssl_socket_factory_obj(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // SBR wave 4 (lane SSL): under `--real-jdk` with the JSSE family retired,
    // `SSLSocketFactory.getDefault()` is real bytecode and the natives that
    // served this carrier's methods are gone, so a synthetic carrier handed
    // out here would be a receiver nothing can serve. Answer what the JDK's
    // own `HttpsURLConnection` getters answer: the real default factory.
    if real_jsse_family(&*ctx) {
        return match ctx.invoke_static(
            "javax/net/ssl/SSLSocketFactory",
            "getDefault",
            "()Ljavax/net/SocketFactory;",
            &[],
        )? {
            Some(Value::Object(Some(f))) => Ok(f),
            _ => Err(MethodCallFailed::from(
                cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: "SSLSocketFactory.getDefault() returned null".into(),
                },
            )),
        };
    }
    // Deliberately a FRESH carrier per call, not a cached singleton.
    // Measured on real JDK 21: `SSLSocketFactory.getDefault()` hands back a
    // different object each time (`SSLContextImpl.engineGetSocketFactory`
    // news up an `SSLSocketFactoryImpl` per call). Caching here was tried and
    // reverted — it diverges from the JDK, and the stability that callers do
    // observe belongs one layer up, in
    // `HttpsURLConnection.getDefaultSSLSocketFactory`, which caches its result
    // in its own static field (see that registration).
    //
    // gc-common w15-d handoff: the context is re-read from its rooted,
    // remapped per-VM slot after the factory allocation, not held across it.
    let vm = ctx.vm_identity();
    default_ssl_context_or_create(ctx)?;
    let obj = crate::tls_carriers::alloc_tls_carrier(
        ctx,
        crate::tls_carriers::SSL_SOCKET_FACTORY_PUBLIC,
        crate::tls_carriers::SSL_SOCKET_FACTORY_IMPL,
        1,
    )?;
    let ssl_ctx = get_runtime_default_ssl_context(vm).ok_or_else(|| {
        MethodCallFailed::from(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "default SSLContext vanished".into(),
        })
    })?;
    ctx.set_field(obj, 0, Value::Object(Some(ssl_ctx)));
    Ok(obj)
}

/// GC root scan for `DEFAULT_SSL_CONTEXT` -- mirrors
/// `gc_scan_tls_ctx_trust_manager_roots` exactly (see that table's doc for
/// why a raw `ObjectRef` held outside the Java heap needs this).
pub fn gc_scan_default_ssl_context_root(vm: usize, roots: &mut Vec<ObjectRef>) {
    if let Some(ctx_obj) = get_runtime_default_ssl_context(vm) {
        if !ctx_obj.as_ptr().is_null() {
            roots.push(ctx_obj);
        }
    }
}

/// Post-move remap companion to `gc_scan_default_ssl_context_root`: VM
/// `vm`'s slot only (gc-common w9-b).
pub fn gc_update_default_ssl_context_ref(vm: usize, map: &cratonvm_types::PointerMap) {
    if map.is_empty() || !DEFAULT_SSL_CONTEXT.has_row(vm) {
        return;
    }
    DEFAULT_SSL_CONTEXT.with(vm, |slot| {
        if let Some(ctx_obj) = slot.as_mut() {
            remap_tls_ref(ctx_obj, map);
        }
    });
}

pub fn register_sslengine_real(r: &mut NativeMethodRegistry) {
            register_engine_impl_natives(r);
    register_apply_parameters(r);
    register_ssl_session_real(r);
    }

/// Register `javax/net/ssl/SSLSession` accessor natives for the REAL-mode
/// session objects the VM hands out — the 8-field session from
/// `SSLEngineImpl.getSession()` (see ~line 3263) and the 4-field session from
/// `SSLServerSocket.accept()` (see ~line 1014). E42: these two numbers were
/// "7" and "3", the identical off-by-one `session_cipher_slot`'s comment
/// records having carried; the widths are and always were 8 and 4. Both
/// objects carry the bare
/// interface `javax/net/ssl/SSLSession` as their runtime class, so a virtual
/// call resolves to the abstract interface declaration (no Code) and the
/// interpreter's no-Code rescue then looks for a native registered on that
/// class name.
///
/// The *full* SSLSession accessor set lives only in the synthetic-jdk-gated
/// `phases_late::register_p68_ssl` / `tls::register_ssl_session`, which are
/// compiled OUT of the real-JDK CLI. Without a real-mode registration here,
/// Tomcat's `SecureNioChannel`/`SecureNio2Channel` buffer sizing
/// (`SSLEngine.getSession().getApplicationBufferSize()` /
/// `getPacketBufferSize()`) throws
/// `AbstractMethodError: javax/net/ssl/SSLSession.getApplicationBufferSize()I
/// has no Code attribute`, killing the NioEndpoint socket processor so the
/// HTTPS server never serves (~18 TLS/HTTP2-TLS/WebSocket-SSL test classes).
/// Side-table associating an `SSLSession` object with its peer (client)
/// certificate chain (DER, leaf first), populated by the engine's getSession().
/// The real TLS session id (ServerHello `legacy_session_id`) for a session
/// object, keyed by `gc_stable_objref_key` like the other session side-tables.
/// Populated for TLS 1.2 sessions only — see [`peek_server_hello_session_id`].
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two sites, both of which now
/// compute their `gc_stable_objref_key` before taking the guard (see there).
pub fn session_wire_id_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<u8>>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<u8>>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The bytes `SSLSession.getId()` would answer for a NEGOTIATED session, as a
/// plain `Vec<u8>` — the same derivation as that registration's body (real
/// wire id first, else the per-object `SplitMix64` pseudo-id), kept as a
/// separate copy rather than a shared call because `getId`'s closure is not a
/// free function. Callers must have already checked `session_has_negotiated`;
/// this does not re-check it, matching `getId`'s own gate placement. Used to
/// key [`session_id_registry`] so it and `getId()` can never disagree about a
/// given session's id.
pub fn session_id_bytes(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    // The seed doubles as the wire-id key: minted here, because the pseudo-id
    // must stay the same for the session's whole life (gc-common w26-b: the
    // weak lock key is, and unlike the identity hash it is never shared with
    // another live session).
    let seed = gc_stable_objref_key(ctx, this);
    let wire_id = session_wire_id_table().lock().get(&seed).cloned();
    if let Some(id) = wire_id {
        return id;
    }
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15; // gc-common w33-a: `| 1` merged keys differing in bit 0
    let mut out = Vec::with_capacity(32);
    for _ in 0..32 {
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        out.push((x & 0xff) as u8);
    }
    out
}

/// NOM-1 (`G7-1` §8, "the one that matters most" among that record's
/// nominations for `net_phase_e.rs`). `SSLSessionContext.getIds()` and
/// `getSession(byte[])` used to answer an always-empty enumeration and a
/// null lookup for every session, even a negotiated one — honest given the
/// context carrier itself was orphaned, but wrong once G7-1 §2 gave
/// `getSessionContext()` a real, populated carrier for exactly that state:
/// "the context a session points at does not contain that session" became
/// reachable. This table is the fix: keyed by the same 32 bytes `getId()`
/// reports, populated where [`negotiated_session_keys`] is (a completed
/// handshake) and erased by [`session_mark_invalidated`], so the two
/// collections stay in lockstep and `getSession(id)` after `invalidate()`
/// answers `null` exactly as MEASURED in G7-1 §1b.
///
/// Holds live `ObjectRef`s → scanned and remapped alongside
/// `engine_session_table`, see `gc_scan_tls_ctx_trust_manager_roots` /
/// `gc_update_tls_ctx_trust_manager_refs`.
///
/// Keyed by `(vm_identity, id)` since gc-common w9-b: it was keyed by the id
/// alone, so VM B's `getIds()` listed VM A's sessions and `getSession(id)`
/// handed B an object in A's heap -- and a TLS 1.2 client and server in two
/// VMs of one process negotiate the SAME wire id, so one VM's insert replaced
/// the other's.
#[allow(clippy::type_complexity)]
pub fn session_id_registry(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<(usize, Vec<u8>), ObjectRef>> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<(usize, Vec<u8>), ObjectRef>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Per-VM bound on [`session_id_registry`]: JSSE's default
/// `SSLSessionContext.getSessionCacheSize()` (gc-common w12-a,
/// `common-w11a-tls-session-objects-are-rooted-for-the-life-of-the-vm`).
///
/// The registry roots every NEGOTIATED session so that `getSession(id)` can
/// find it after its engine is gone -- which is JSSE's session cache, and
/// JSSE bounds it: `SSLSessionContextImpl` evicts past this size. Unbounded,
/// an HTTPS server rooted one `SSLSession` (and its `putValue` map) per
/// connection it ever accepted. The registry is per VM while JSSE's cache is
/// per context, so a VM with several contexts evicts sooner than HotSpot
/// would; no context-level size or timeout is modelled.
pub const SESSION_ID_REGISTRY_PER_VM: usize = 20480;

/// Insertion order of one VM's [`session_id_registry`] rows, for the bound.
/// `current` is the live sequence number of each id; a `fifo` entry whose
/// number is no longer current (the id was invalidated, or re-registered) is
/// skipped when it reaches the front.
#[derive(Default)]
pub struct SessionIdOrder {
    pub next_seq: u64,
    pub fifo: std::collections::VecDeque<(u64, Vec<u8>)>,
    pub current: HashMap<Vec<u8>, u64>,
}

/// `vm -> SessionIdOrder`. `LockLevel::Scratch`; never held together with
/// [`session_id_registry`] (the two are taken one after the other, so a racing
/// insert and invalidate of the SAME id can at worst evict one row early or
/// leave one row unevicted -- never a wrong object).
pub fn session_id_order(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<usize, SessionIdOrder>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<usize, SessionIdOrder>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Register `ses` under `id` in VM `vm`'s session cache, evicting the oldest
/// rows past `cap`. Answers how many rows were evicted.
pub fn session_id_registry_insert_bounded(vm: usize, id: Vec<u8>, ses: ObjectRef, cap: usize) -> usize {
    let evicted: Vec<Vec<u8>> = {
        let mut orders = session_id_order().lock();
        let order = orders.entry(vm).or_default();
        let seq = order.next_seq;
        order.next_seq = seq.wrapping_add(1);
        order.current.insert(id.clone(), seq);
        order.fifo.push_back((seq, id.clone()));
        let mut evicted = Vec::new();
        while order.current.len() > cap {
            let Some((s, old)) = order.fifo.pop_front() else {
                break;
            };
            if order.current.get(&old) == Some(&s) {
                order.current.remove(&old);
                evicted.push(old);
            }
        }
        // Skipped (stale) entries are otherwise dropped only when they reach
        // the front; compact once they are the majority.
        if order.fifo.len() > 2 * order.current.len() + 64 {
            let current = &order.current;
            order.fifo.retain(|(s, id)| current.get(id) == Some(s));
        }
        evicted
    };
    let mut registry = session_id_registry().lock();
    registry.insert((vm, id), ses);
    for old in &evicted {
        registry.remove(&(vm, old.clone()));
    }
    evicted.len()
}

/// Drop `id` from VM `vm`'s session cache (`invalidate()`).
pub fn session_id_registry_remove(vm: usize, id: Vec<u8>) {
    if let Some(order) = session_id_order().lock().get_mut(&vm) {
        order.current.remove(&id);
    }
    session_id_registry().lock().remove(&(vm, id));
}

/// `SSLSessionContext.getIds()`'s answer: every id VM `vm` currently has in
/// [`session_id_registry`], in no particular order — the real JDK's own
/// `getIds()` makes no ordering guarantee either.
pub fn session_id_registry_ids(vm: usize) -> Vec<Vec<u8>> {
    session_id_registry()
        .lock()
        .keys()
        .filter(|(owner, _)| *owner == vm)
        .map(|(_, id)| id.clone())
        .collect()
}

/// `SSLSessionContext.getSession(byte[])`'s lookup half, in VM `vm`. `None`
/// for both "no session was ever cached under this id" and "the id has the
/// wrong length" — the caller is expected to have already refused a `null`
/// array (MEASURED: that row is a `NullPointerException`, not a lookup miss).
pub fn session_by_id(vm: usize, id: &[u8]) -> Option<ObjectRef> {
    session_id_registry()
        .lock()
        .get(&(vm, id.to_vec()))
        .copied()
}

pub fn session_peer_certs_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The mint point of a NEW synthetic `SSLSession` object (gc-common w11-a):
/// drop every row this VM keyed under the object's identity key.
///
/// The session side tables (`session_peer_certs_table`,
/// `session_local_certs_table`, `session_peer_endpoint_table`,
/// `session_invalidated_table`, `session_wire_id_table`,
/// `session_last_accessed_table`, `negotiated_session_keys`) were keyed by
/// (VM, identity hash) until gc-common w26-b (now the session's weak lock
/// key), nothing removed a row while the VM lived, and this VM
/// hands a dead object's identity hash to a later object. The writers only
/// ever ADD a row when they have something to say -- an empty peer chain, for
/// one, records nothing -- so a fresh session whose hash matched a collected
/// one answered with the dead session's facts. The worst of them:
/// `getPeerCertificates()` on a server session whose client presented NO
/// certificate (`setWantClientAuth`) returned the certificate chain of an
/// earlier, unrelated client -- which is what a `CLIENT-CERT` authenticator
/// reads the caller's identity from. Also `isValid() == false` for a session
/// that was never invalidated, and a dead session's wire id / last-access
/// time. Called by every constructor of a synthetic session this crate owns,
/// straight after the allocation and before any row for it is written.
///
/// gc-common w12-a: also the point where the new session is recorded as the
/// weak owner of those rows ([`note_tls_weak_owner`]), so
/// [`gc_sweep_tls_rows`] drops them once the session dies instead of keeping
/// them for the life of the VM. `session` is fresh here by the contract
/// above (straight after the allocation).
pub fn forget_session_rows(ctx: &dyn NativeContext, session: ObjectRef) {
    let key = gc_stable_objref_key(ctx, session);
    forget_session_key(key);
    note_tls_weak_owner(ctx, TlsOwnerKind::Session, key, session);
}

/// Drop every session side-table row under `key`; answers how many went.
pub fn forget_session_key(key: u64) -> usize {
    let mut dropped = 0usize;
    dropped += usize::from(session_peer_certs_table().lock().remove(&key).is_some());
    dropped += usize::from(session_local_certs_table().lock().remove(&key).is_some());
    dropped += usize::from(session_peer_endpoint_table().lock().remove(&key).is_some());
    dropped += usize::from(session_sni_table().lock().remove(&key).is_some());
    dropped += usize::from(session_invalidated_table().lock().remove(&key));
    dropped += usize::from(session_wire_id_table().lock().remove(&key).is_some());
    dropped += usize::from(session_last_accessed_table().lock().remove(&key).is_some());
    dropped += usize::from(negotiated_session_keys().lock().remove(&key));
    dropped
}

/// Side-table associating an `SSLSession` object with ITS OWN (local) certificate
/// chain (DER, leaf first) — the mirror-image of `session_peer_certs_table`,
/// populated by `build_synthetic_ssl_session`. Backs `SSLSession.getLocalCertificates()`
/// (`phases_late::register_p68_ssl`'s registration, which cannot reach a
/// `t27_tls`-private table directly — see `local_certs_for_session`/
/// `record_local_cert_chain` below for the crate-visible accessors).
pub fn session_local_certs_table() -> &'static Mutex<HashMap<u64, Vec<Vec<u8>>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Crate-visible accessor for `session_local_certs_table`, used by
/// `phases_late::register_p68_ssl`'s `getLocalCertificates` registration.
pub fn local_certs_for_session(ctx: &dyn NativeContext, session: ObjectRef) -> Vec<Vec<u8>> {
    // Key before the guard, and a lookup: never mints (gc-common w26-b).
    let key = match existing_objref_key(ctx, session) {
        Some(key) => key,
        None => return Vec::new(),
    };
    session_local_certs_table()
        .lock()
        .get(&key)
        .cloned()
        .unwrap_or_default()
}

/// The crate-visible WRITER for `session_local_certs_table`, named by
/// `session_local_certs_table`'s own doc comment since that table was written —
/// and, until G51, not present in the tree at all. `grep -rn
/// 'record_local_cert_chain'` over `native-builtins/src/` returned exactly one
/// hit, the doc comment promising it. The table had ONE writer, an open-coded
/// `.lock().insert(..)` at the tail of `build_synthetic_ssl_session`, and that
/// function is reached only from the engine path.
///
/// MEASURED consequence, `RSslLiveSession` on `9ae371468` (`target-rel3`),
/// 2026-08-17 — the server side of a completed loopback handshake, through
/// `SSLServerSocket.accept()`, which never goes near
/// `build_synthetic_ssl_session`:
///
/// ```text
/// CK RSslLiveSession server.localPrincipal          = null  WANT CN=localhost
/// CK RSslLiveSession server.localPrincipal.class    = null  WANT javax.security.auth.x500.X500Principal
/// CK RSslLiveSession server.localCertificates.length = -1   WANT 1
/// ```
///
/// All three are one fact: `ssl_security`'s `getLocalCertificates` and
/// `getLocalPrincipal` both read this table (the second derives the subject
/// from the leaf of what the first returns, which is exactly HotSpot's
/// contract), and for an accepted server session it was empty.
///
/// A no-op on an empty chain, for the same reason [`record_client_peer_chain`]
/// is: a client with no configured identity legitimately has none, and
/// `getLocalCertificates()` answering `null` there is the measured HotSpot
/// answer (`client.localCertificates = null`, green today and still green).
pub fn record_local_cert_chain(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    chain_der: Vec<Vec<u8>>,
) {
    if chain_der.is_empty() {
        return;
    }
    // Key before the guard (gc-common w26-b).
    let key = gc_stable_objref_key(ctx, session);
    session_local_certs_table().lock().insert(key, chain_der);
}

/// Side-table associating an `SSLSession` object with the ENDPOINT its peer was
/// reached at — `(host, port)` — for the session shapes that have no slot to
/// carry one.
///
/// **Why a side table and not a wider session.** G44-1 §4 enumerated the
/// widening against every width-keyed reader in this file and it fails at every
/// width: at 6 `session_proto_slot`/`session_cipher_slot` SWAP, `sslsess_attrs_slot`
/// goes `None` so the whole attribute API silently no-ops, and
/// `session_has_negotiated` falls into `_ => true` — which makes the NULL
/// session valid again and takes the green `RSslNullSession` with it; at 7 the
/// swap remains and the three green `*.sessionContext.isNull` rows go red.
/// `phases_late::ssl_security::NEW13_SSL_SESS_FIELDS` now carries that table as
/// a "DO NOT WIDEN" block with a test asserting the slot map against this
/// file's two slot functions. This is the shape that costs nothing:
/// [`session_peer_certs_table`] already solves the identical problem — a fact
/// about the peer that the narrow shape has no slot for — keyed the same way,
/// on the session OBJECT.
///
/// **The port is `i32`, not `u16`.** `-1` is HotSpot's measured answer for "no
/// peer", and a table that could only hold `0..=65535` would have to spell that
/// as an absent row, which is a different statement: absent means "nobody
/// recorded an endpoint for this session", and the readers below fall THROUGH
/// an absent row to the socket registry. A recorded `-1` would be a claim.
/// Nothing writes one today; the type is what keeps the distinction available.
///
/// MEASURED, HotSpot 25.0.3+9-LTS `Microsoft-13877124`, this host, 2026-08-17
/// (`scratchpad/g51/G51Probe.java` and `G51Engine.java`, the full
/// peer-endpoint family):
///
/// ```text
///                                              getPeerHost()   getPeerPort()
///   never-connected SSLSocket                  null            -1
///   client SSLSocket dialled by hostname       localhost       the server port
///   client SSLSocket dialled by IP literal     127.0.0.1       the server port
///     ... and the SAME with an explicit SNIHostName("localhost") set:
///         SNI does NOT reach getPeerHost, in either direction
///   the SERVER's view of that handshake        127.0.0.1       the CLIENT's
///                                              (the literal,     ephemeral port
///                                               never reverse    (positive, and
///                                               resolved)        NOT the listener's)
///   SSLEngine, no peer named                   null            -1
///   SSLEngine("example.test", 8443), pre-hs    null            -1
///   SSLEngine("example.test", 8443), post-hs   example.test    8443
///   SSLEngine server side of that handshake    null            -1
///   any of the above after invalidate()        unchanged
/// ```
///
/// Two of those rows are the whole design. **The host is the one the CALLER
/// NAMED, not one derived from the peer's certificate and not one derived from
/// SNI** — the IP-literal row proves it (leaf subject `CN=localhost`, SNI
/// `localhost`, answer `127.0.0.1`). And **the server's peer is the CLIENT**,
/// so its port is an ephemeral one and comparing it to the listener's port is
/// the wrong test; `RSslLiveSession` asserts `> 0` there and `== port` on the
/// client side, and those are different questions on purpose.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both callers evaluate
/// `gc_stable_objref_key` — which calls `ctx.identity_hash_code` — into a local
/// BEFORE acquiring. It used to sit inside the lock expression, which is the
/// same shape the 2026-08-17 round hoisted out of five other tables.
pub fn session_peer_endpoint_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (String, i32)>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (String, i32)>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record the endpoint a session's peer was reached at. See
/// [`session_peer_endpoint_table`] for the measured contract and for why this
/// is a side table rather than two more fields on the session.
///
/// A no-op when there is nothing to say — an empty host AND a non-positive
/// port. That is not tidiness: the readers below FALL THROUGH an absent row to
/// `session_stream_id`, and a row of `("", -1)` would shadow a real answer the
/// socket registry could still have given.
pub fn record_session_peer_endpoint(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    host: &str,
    port: i32,
) {
    if host.is_empty() && port <= 0 {
        return;
    }
    let key = gc_stable_objref_key(ctx, session);
    session_peer_endpoint_table()
        .lock()
        .insert(key, (host.to_string(), port));
}

/// `session -> (the `server_name` host the connection carried, whether this end is the server)`.
///
/// Backs `ExtendedSSLSession.getRequestedServerNames()` (the one-element `SNIHostName` list HotSpot answers
/// on BOTH ends of a connection that carried SNI, the empty list otherwise) and
/// `getPeerSupportedSignatureAlgorithms()` (empty on a client session, the client offer on a server one).
/// A side table like its neighbours: the carrier has no spare slot.
pub fn session_sni_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (Option<String>, bool)>> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (Option<String>, bool)>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record what the connection of `session` carried as SNI and which end it is (see [`session_sni_table`]).
pub fn record_session_sni(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    sni: Option<String>,
    server_side: bool,
) {
    let key = gc_stable_objref_key(ctx, session);
    session_sni_table().lock().insert(key, (sni, server_side));
}

pub fn session_sni(ctx: &dyn NativeContext, session: ObjectRef) -> Option<(Option<String>, bool)> {
    let key = existing_objref_key(ctx, session)?;
    session_sni_table().lock().get(&key).cloned()
}

/// What `ExtendedSSLSession.getLocalSupportedSignatureAlgorithms()` answers on a default JDK 25 context: the
/// scheme names the JDK enables (measured, HotSpot 25.0.3 and 25.0.4, both ends, the default
/// `SSLContext`). A connection built over a restricted context can differ; the carrier does not model that.
pub const JDK_DEFAULT_SIGNATURE_ALGORITHMS: [&str; 16] = [
    "SHA256withECDSA",
    "SHA384withECDSA",
    "SHA512withECDSA",
    "Ed25519",
    "Ed448",
    "RSASSA-PSS",
    "RSASSA-PSS",
    "RSASSA-PSS",
    "RSASSA-PSS",
    "RSASSA-PSS",
    "RSASSA-PSS",
    "SHA256withRSA",
    "SHA384withRSA",
    "SHA512withRSA",
    "SHA1withECDSA",
    "SHA1withRSA",
];

/// The recorded endpoint for a session object, or `None` if none was recorded.
pub fn session_peer_endpoint(ctx: &dyn NativeContext, session: ObjectRef) -> Option<(String, i32)> {
    let key = existing_objref_key(ctx, session)?;
    session_peer_endpoint_table().lock().get(&key).cloned()
}

/// The remote address of an accepted rustls server stream, as
/// `(host-literal, port)`.
///
/// `rustls_server_accept_within` binds the accepted peer address as `_peer` and
/// drops it, and `TlsServerStreamEntry` — unlike its client twin, which carries
/// `peer_host`/`peer_port` — has no field for it. Rather than widen the entry
/// for one accessor, this asks the duplicate socket handle the entry already
/// keeps for exactly this class of out-of-band question (`raw`; see
/// [`TlsClientStreamEntry::raw`]).
///
/// The literal is normalised out of IPv4-mapped IPv6 form: a dual-stack
/// listener reports a loopback client as `::ffff:127.0.0.1`, and HotSpot's
/// measured answer is `127.0.0.1` (`G51Probe`, `server.peerHost`, which agrees
/// with the accepted socket's own `getInetAddress().getHostAddress()`).
/// HotSpot does NOT reverse-resolve it to a name, so neither does this.
pub fn rustls_server_peer_endpoint(rid: i32) -> Option<(String, i32)> {
    let reg = sreg().lock();
    let address = reg
        .server_streams
        .get(&rid)?
        .raw
        .as_ref()?
        .peer_addr()
        .ok()?;
    Some(socket_addr_endpoint(address))
}

/// `(host-literal, port)` for a remote socket address, in the spelling HotSpot
/// answers `SSLSession.getPeerHost()` with.
///
/// Split out of [`rustls_server_peer_endpoint`] because the only interesting
/// thing in it — the IPv4-mapped normalisation — needs no socket to test, and a
/// rule that lives inside a function requiring a live TLS peer is a rule
/// nothing checks. A dual-stack listener reports a loopback client as
/// `::ffff:127.0.0.1`; HotSpot's measured answer is `127.0.0.1`, matching the
/// accepted socket's own `getInetAddress().getHostAddress()`.
pub fn socket_addr_endpoint(address: std::net::SocketAddr) -> (String, i32) {
    let ip = match address.ip() {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => std::net::IpAddr::V6(v6),
        },
        other => other,
    };
    (ip.to_string(), address.port() as i32)
}

/// FIX (netty-https-client-trust residual): populate `session_peer_certs_table`
/// for a CLIENT-side `SSLSession` (allocated by `phases_late::new13_alloc_ssl_session`
/// for the native-tls `SSLSocketFactory.createSocket` path). Without this, the
/// client's own session object never gets an entry — the table was only ever
/// populated for SSLEngine-based (server / NIO) sessions — so any caller that
/// later queries `session.getPeerCertificates()` on the CLIENT session (e.g.
/// Spring's `DefaultSslInfo.initCertificates` when building `SslInfo` for a
/// reactive HTTPS exchange) always sees an empty chain and gets
/// `SSLPeerUnverifiedException("peer not authenticated")` even though the
/// handshake succeeded and a real peer chain was captured (and already used
/// once, to pass the TrustManager check in `new13_do_create_socket`). A no-op
/// when the chain is empty (nothing to record; the accessor's existing
/// empty-chain contract is unaffected).
pub fn record_client_peer_chain(
    ctx: &dyn NativeContext,
    session: ObjectRef,
    chain_der: Vec<Vec<u8>>,
) {
    if chain_der.is_empty() {
        return;
    }
    // Key before the guard (gc-common w26-b).
    let key = gc_stable_objref_key(ctx, session);
    session_peer_certs_table().lock().insert(key, chain_der);
}

/// The TLS STREAM ID recorded in a session's slot 2, for the widths on which
/// slot 2 *is* a stream id — and `None` on every other width.
///
/// F18. This exists because two accessors were reading slot 2 as a `servlet`
/// registry key on any shape with three or more fields, and slot 2 does not
/// mean the same thing at every width. The table is
/// [`session_has_negotiated`]'s, restated as the question these callers
/// actually ask:
///
/// | width | minted by | slot 2 | is it a stream id? |
/// |---|---|---|---|
/// | 3 | (retired — E42) | stream id | n/a, nothing mints it |
/// | 4 | `ssl_security`'s NEW-13 shape; `SSLServerSocket.accept` | stream id (or `-1`, or `HTTPS_CLIENT_SESSION_MARKER`) | **yes** |
/// | 6 | `tls.rs::init_ssl_session_fields`; `http2.rs` | `tls.rs::SES_VALID` | **no** |
/// | 8 | `build_synthetic_ssl_session` | the `isValid` flag | **no** |
///
/// **The bug this closes, which is a cross-connection one.**
/// `ssl_security::getPeerPrincipal` gated on `> NEW13_SESS_TLSID`, i.e. width
/// >= 3, so on an 8-field engine session it read the *`isValid` flag* — `0` or
/// `1` — and looked that up in `servlet::s2_tls_peer_cert_chain_der`. Ids `0`
/// and `1` are not unreachable: `s2_next_free_id` starts its counter at `1`
/// (`servlet.rs`), so id `1` is the FIRST id the socket registry hands out.
/// An engine session that happened to be valid could therefore be handed the
/// peer certificate chain of an unrelated socket. E31-1 NOMINATION 4, which
/// re-raised E22-1 NOMINATION B.
///
/// Ids below zero are rejected here rather than at the call sites: `-1` is the
/// "never connected" sentinel every minter writes, and a negative registry key
/// is meaningless in every table that consumes one.
pub fn session_stream_id(ctx: &dyn NativeContext, session: ObjectRef) -> Option<i32> {
    match session_shape_width(&*ctx, session) {
        3 | 4 => match sess_get(&*ctx, session, 2) {
            Value::Int(id) if id >= 0 => Some(id),
            _ => None,
        },
        _ => None,
    }
}

/// **The one resolver for "what certificate chain did this session's peer
/// present".** Every door that answers a question about the peer's identity
/// must come through here.
///
/// F18. `SSLSession.getPeerCertificates` (this file) and
/// `SSLSession.getPeerPrincipal` (`phases_late::ssl_security`) are registered
/// by different registrars and, until now, read *different sources* for the
/// same fact:
///
/// * `getPeerCertificates` read [`session_peer_certs_table`], keyed on the
///   session OBJECT;
/// * `getPeerPrincipal` read `servlet::s2_tls_peer_cert_chain_der(slot2)`,
///   keyed on the TLS STREAM ID.
///
/// MEASURED on HotSpot 25.0.3+9-LTS (this host,
/// `scratchpad/f18/F18SessionContract.java`, loopback `HttpsServer` +
/// `HttpsURLConnection`, three runs byte-identical): on a completed handshake
/// the two agree, and `getPeerPrincipal()` is exactly the leaf certificate's
/// subject —
/// `getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())` is
/// `true`. On CratonVM they disagreed: an HTTPS client session is registered
/// in the object table by `record_client_peer_chain` and has NO entry in the
/// socket registry, so `getPeerCertificates()` returned the chain while
/// `getPeerPrincipal()` on the SAME OBJECT, in the same call sequence, threw
/// `SSLPeerUnverifiedException`. F10-1 NOMINATION 2.
///
/// **Order is load-bearing, and it shrinks a collision surface rather than
/// widening one.** The object table is consulted FIRST. That table is the one
/// populated for HTTPS client sessions, whose slot 2 carries
/// `net_phase_e::HTTPS_CLIENT_SESSION_MARKER` — a value chosen to MISS the
/// socket registry. Trying the object table first means the common HTTPS case
/// never reaches the id lookup at all, so the marker's miss is now a
/// second-line guarantee instead of the only one. It still has to hold, and it
/// does: the marker is `0x0800_0000`, strictly below
/// `servlet::PENDING_CONNECT_SOCK_ID_BASE` (`0x1000_0000`),
/// `PENDING_LAYERED_SOCK_ID_BASE` (`0x2000_0000`) and `RUSTLS_SOCK_ID_BASE`
/// (`0x4000_0000`), and far above the monotonic `s2_next_free_id` counter that
/// starts at `1` — re-verified against `servlet.rs` for F18, not taken on
/// trust. Being below `RUSTLS_SOCK_ID_BASE` also keeps it off
/// `s2_tls_peer_cert_chain_der`'s rustls redirect.
///
/// **Not implemented by calling `getPeerCertificates` through the interpreter.**
/// That was F10-1 NOMINATION 2's suggested shape and it is the more expensive
/// one: it would build `X509CertImpl` mirrors only for `getPeerPrincipal` to
/// pull a subject string back out of them. Sharing the DER resolver gives the
/// same single-source-of-truth property — the two doors cannot see different
/// chains, because there is only one function that decides — without the round
/// trip.
pub fn peer_certs_for_session(ctx: &dyn NativeContext, session: ObjectRef) -> Vec<Vec<u8>> {
    // Key before the guard, and a lookup: never mints (gc-common w26-b).
    let recorded = existing_objref_key(ctx, session)
        .and_then(|key| session_peer_certs_table().lock().get(&key).cloned());
    if let Some(chain) = recorded {
        if !chain.is_empty() {
            return chain;
        }
    }
    if let Some(id) = session_stream_id(ctx, session) {
        if let Some(chain) = servlet::s2_tls_peer_cert_chain_der(id) {
            return chain;
        }
    }
    Vec::new()
}

/// The set of `javax/net/ssl/SSLSession` objects on which `invalidate()` has
/// been called, keyed the same GC-stable way as
/// [`session_peer_certs_table`] and `session_local_certs_table`.
///
/// F18 — **why a side table and not a field.** `SSLSession.invalidate()` had
/// no real-JDK-mode registration at all (`grep` over the whole crate returned
/// exactly one, in `tls.rs`, whose registrar is `#[cfg(feature =
/// "synthetic-jdk")]`), so in the mode `--real-jdk` runs the interface
/// declaration with no Code attribute was what ran: an `AbstractMethodError`.
/// F10-1 NOMINATION 3 split the fix in two and called the second half blocked,
/// because recording an `invalidated` bit "needs a slot this shape does not
/// have" and both F6-1 §4 and E42-1 §2 reject widening the shape again.
///
/// That premise is true and the conclusion does not follow: this file already
/// carries two per-session facts that no slot holds, in exactly this form. The
/// bit does not need to live in the object.
///
/// **Why widening would have been the worse fix, stated rather than implied.**
/// The width of a session shape is what every reader in this file uses to
/// decide what its slots MEAN (`session_cipher_slot`, `session_proto_slot`,
/// `sslsess_attrs_slot`, [`session_stream_id`], `session_has_negotiated`).
/// Widening the NEW-13 shape from 4 to 5 would move it across the `n >= 6`
/// boundary's near side and re-open the co-requisite E42-1 records as BLOCKING.
/// A side table changes no width and therefore no reader.
///
/// **This table is inert until an application calls `invalidate()`.** It is
/// empty in every process that never does, so [`session_is_valid`] answers
/// exactly what `session_has_negotiated` alone answered before F18 — which is
/// the property that makes this change safe to land without a TLS fixture.
///
/// **Growth is bounded by `invalidate()` calls, not by session mints.** F10-1
/// §8.5 records that `session_peer_certs_table` grows once per minted session
/// and is never pruned, which on the HTTPS path is once per accessor call.
/// This table is only written by the one door, so it does not inherit that
/// rate. It used to inherit the 32-bit key width (`gc_stable_objref_key` was
/// the identity hash, so a collision reported a *live* session as
/// invalidated, as the two cert tables could serve another session's chain);
/// since gc-common w26-b the key is the session's weak lock key, unique among
/// live objects, and a dead session's rows go when its key is freed.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Same hoist as
/// [`session_peer_endpoint_table`]: `gc_stable_objref_key` is evaluated into a
/// local before either acquisition, so no `NativeContext` call runs under the
/// guard.
pub fn session_invalidated_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<u64>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<u64>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashSet::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record that `invalidate()` was called on this session. Crate-visible so
/// `tls.rs`'s `--synthetic-jdk` copy writes the same state this file's
/// real-mode copy reads — one bit, one owner, both modes.
///
/// **Deliberately a no-op for a session that negotiated nothing**, and that is
/// HotSpot's behaviour rather than an optimisation. MEASURED, HotSpot
/// 25.0.3+9-LTS (`scratchpad/f18/F18SessionContract.java`, three runs
/// byte-identical), on the unconnected `SSLSocket`'s session and on the
/// pre-handshake `SSLEngine`'s alike: every accessor reads identically before
/// and after `invalidate()`, because `isValid()` was already `false` and there
/// was nothing else to invalidate. Since [`session_is_valid`] is
/// `negotiated && !invalidated`, recording the bit for a never-negotiated
/// session could not change an answer either — so the gate costs no fidelity
/// and keeps the table from growing on the one door that mints sessions
/// nobody handshaked.
pub fn session_mark_invalidated(ctx: &dyn NativeContext, session: ObjectRef) {
    if !session_has_negotiated(ctx, session) {
        return;
    }
    let key = gc_stable_objref_key(ctx, session);
    session_invalidated_table().lock().insert(key);
    // NOM-1 (G7-1). MEASURED (§1b): `getSession(id)` on the owning context
    // goes from SAME-OBJECT to `null` once the session invalidates. Only a
    // negotiated session can have an entry here at all (mirrors the guard
    // above), and removing by the id `session_id_bytes` derives is safe even
    // when there was never an entry to remove.
    let id_bytes = session_id_bytes(ctx, session);
    let vm = ctx.vm_identity();
    session_id_registry_remove(vm, id_bytes);
}

/// **The one predicate behind `SSLSession.isValid()`, in both modes.**
///
/// F18. HotSpot's `isValid()` is `SSLSessionImpl.isRejoinable()`, which is
/// `sessionId.length() != 0 && !invalidated && ...`
/// (`sun/security/ssl/SSLSessionImpl.java:788`) — TWO pieces of state. Before
/// F18 this VM modelled only the first, because nothing in the shipping mode
/// could write the second.
///
/// MEASURED, HotSpot 25.0.3+9-LTS, on a session that GENUINELY negotiated (a
/// completed loopback `HttpsURLConnection` handshake — the population F10 made
/// reachable, and the first time this contract has been measured on one rather
/// than on a null session), `scratchpad/f18/F18SessionContract.java`, three
/// runs byte-identical:
///
/// ```text
///   before invalidate()  isValid=true   id=byte[32]  cipher=TLS_AES_256_GCM_SHA384
///                        protocol=TLSv1.3  peerPrincipal=CN=localhost,...
///                        peerCerts=1  sessionContext=SSLSessionContextImpl
///   after  invalidate()  isValid=FALSE  id=byte[32]  cipher=TLS_AES_256_GCM_SHA384
///                        protocol=TLSv1.3  peerPrincipal=CN=localhost,...
///                        peerCerts=1  sessionContext=NULL
///   invalidate() twice   isValid=false            (idempotent)
///   a second, untouched connection's session      isValid=true, unaffected
/// ```
///
/// So `invalidate()` moves `isValid()` — and, a detail F10 did not report and
/// this lane measured, `getSessionContext()`, which drops to `null`. It moves
/// NOTHING else: the id keeps its 32 bytes and its exact contents, and the
/// cipher, protocol, peer principal and peer chain all survive. That is why
/// `getId()` is deliberately gated on `session_has_negotiated` ALONE and not
/// on this function — tying the id to validity would make `invalidate()` erase
/// the id, a new divergence traded for an old one. `getSessionContext()`'s
/// half of the contract is free here: this VM answers `null` in every state
/// (see its registration), so it is already on the right side of the
/// invalidated case and merely under-reports the live one.
pub fn session_is_valid(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    if !session_has_negotiated(ctx, session) {
        return false;
    }
    // A lookup: a session never invalidated need not be keyed (gc-common w26-b).
    match existing_objref_key(ctx, session) {
        Some(key) => !session_invalidated_table().lock().contains(&key),
        None => true,
    }
}

/// The JDK's own session class. Under `--compatible` the session carriers are
/// minted as instances of it (JVMS 6.5: `new` can produce it, `SSLSession` the
/// interface it replaces cannot be instantiated), with the carrier's private
/// slot map still at slots `0..width` and `width` extra slots appended ABOVE the
/// class's real fields. See [`alloc_session_carrier`].
pub const SSL_SESSION_IMPL: &str = "sun/security/ssl/SSLSessionImpl";

/// Where `this`'s private slot map starts: ABOVE every field the real class
/// declares for an `SSLSessionImpl` carrier, `0` for every other receiver.
///
/// It cannot start at 0 on the Impl: native field access is descriptor-aware
/// (`NativeContext::get_field`/`set_field` coerce to the slot's DECLARED type),
/// so a `Long` written to slot 5 (a declared `boolean`) is truncated, a
/// reference written to slot 7 (a declared `long`) is dropped, and an `Int`
/// written to a declared reference is boxed. Slots at or above the declared
/// width have no descriptor and hold what is written.
pub fn session_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let cid = ctx.class_id_of_object(this);
    if ctx.class_name_of_id(cid).as_deref() == Some(SSL_SESSION_IMPL) {
        ctx.class_num_total_fields(cid)
    } else {
        0
    }
}

/// Read private slot `slot` of the session carrier `this` (see [`session_base`]).
pub fn sess_get(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    ctx.get_field(this, session_base(ctx, this) + slot)
}

/// Write private slot `slot` of the session carrier `this` (see [`session_base`]).
pub fn sess_set(ctx: &dyn NativeContext, this: ObjectRef, slot: usize, value: Value) {
    ctx.set_field(this, session_base(ctx, this) + slot, value);
}

/// The width of `this`'s private slot map: the discriminator every session
/// reader in this crate keys its shape table on (2/3/4/6/8, see
/// [`session_has_negotiated`]).
///
/// For a carrier minted as `SSLSessionImpl` the object's slot count is
/// `real + width`, so the width is what is left after the class's declared
/// fields; for every other receiver (the interface-stamped carrier of the
/// synthetic-JDK mode, a unit-test double) it is the slot count, as before.
/// `&dyn`: it never re-enters Java.
pub fn session_shape_width(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let n = ctx.object_num_fields(this);
    let cid = ctx.class_id_of_object(this);
    if ctx.class_name_of_id(cid).as_deref() == Some(SSL_SESSION_IMPL) {
        return n.saturating_sub(ctx.class_num_total_fields(cid));
    }
    n
}

/// Mint a session carrier of private-slot width `width`.
///
/// `--compatible` hands out `sun.security.ssl.SSLSessionImpl`, as HotSpot does
/// (`session instanceof ExtendedSSLSession`, a cast, reflection); the image
/// without that class (synthetic-JDK) keeps the `SSLSession` interface stamp.
/// The allocation is `real + width` slots, so [`session_shape_width`] recovers
/// `width`; the carrier's own slots stay `0..width`, aliasing the Impl's first
/// real fields, which is harmless because the object is wider than the class
/// declares (a legacy layout, every slot a tagged cell) and every row the Impl
/// declares is a native (see [`mirror_session_rows_onto_impl`]), so no JDK body
/// ever reads those fields.
#[track_caller]
pub fn alloc_session_carrier(
    ctx: &mut dyn NativeContext,
    width: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Only where the mirrored rows exist: they are `SyntheticStub`s, which `--real-jdk` (and the
    // `jdk-jsse` row) drops, and an Impl carrier without them would run the JDK's bytecode on slots it
    // does not have.
    if !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkJsse)
    {
        if let Some((name, _)) =
            cratonvm_native_api::instantiable::first_instantiable(ctx, &[SSL_SESSION_IMPL])
        {
            let real = crate::appended_slot_base_for_class(ctx, name);
            if real > 0 {
                return try_alloc_concurrent_synthetic(ctx, name, real + width);
            }
        }
    }
    try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", width)
}

/// The registration half of [`alloc_session_carrier`]: the carrier is an
/// instance of `SSLSessionImpl`, whose real class DECLARES every session
/// method with `Code`, and native dispatch keys on the receiver's own class
/// first. So every row this crate put on the public `javax/net/ssl/SSLSession`
/// is copied onto the Impl (an Impl row that already exists is left alone), and
/// the methods the Impl declares that the public interface does not are given
/// carrier answers, so no JDK body runs on a carrier that has no `conContext`.
///
/// Runs last in `register_essential_natives_with_shims`, after every registrar
/// that writes `SSLSession` rows. Each row keeps its source row's kind; the
/// rows added here are `SyntheticStub`s and go with the carrier: the removal
/// issue is `compatible-tls-and-http-carriers-are-instances-of-abstract-classes-FIXED-20261010.md`.
pub fn mirror_session_rows_onto_impl(r: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    // Only a carrier needs these rows. Under `--real-jdk` the real JSSE stack
    // builds every `SSLSessionImpl` and no carrier is minted; there a mirrored
    // `SSLSession` row on the concrete class would shadow its real bodies
    // (m8, 2026-10-10: `NioConf19b` `--real-jdk` read `getProtocol()` "NONE"
    // and `getPeerPrincipal()` threw after a completed handshake). With the
    // switch off the carriers are not `SSLSessionImpl` instances either.
    if r.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::JdkOnly
    {
        return;
    }
            let imp = SSL_SESSION_IMPL;

    // `Object`'s identity methods: the Impl overrides them with bodies that read
    // `sessionId`.
    r.register(imp, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ctx.identity_hash_code(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(imp, "equals", "(Ljava/lang/Object;)Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let same = matches!(args.get(1), Some(Value::Object(Some(o))) if *o == this);
        Ok(Some(Value::Int(i32::from(same))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // `ExtendedSSLSession` / Impl-only accessors. The carrier negotiated through
    // rustls and recorded none of these; HotSpot answers an empty list for a
    // session that did not request SNI or staple OCSP.
    // `getRequestedServerNames`: the `SNIHostName` the connection carried (both ends), else the empty list.
    r.register(
        imp,
        "getRequestedServerNames",
        "()Ljava/util/List;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = session_sni(&*ctx, this).and_then(|(sni, _)| sni);
            match name {
                Some(host) => {
                    let text = ctx.create_string_uninterned(&host);
                    let sni = match ctx.new_object_initialized(
                        "javax/net/ssl/SNIHostName",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(text))],
                    ) {
                        Ok(Some(Value::Object(Some(o)))) => o,
                        // A name SNIHostName refuses is not one JSSE would have sent either.
                        _ => {
                            return ctx.invoke_static(
                                "java/util/Collections",
                                "emptyList",
                                "()Ljava/util/List;",
                                &[],
                            )
                        }
                    };
                    ctx.invoke_static(
                        "java/util/Collections",
                        "singletonList",
                        "(Ljava/lang/Object;)Ljava/util/List;",
                        &[Value::Object(Some(sni))],
                    )
                }
                None => ctx.invoke_static(
                    "java/util/Collections",
                    "emptyList",
                    "()Ljava/util/List;",
                    &[],
                ),
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        imp,
        "getStatusResponses",
        "()Ljava/util/List;",
        |ctx, _args| {
            ctx.invoke_static(
                "java/util/Collections",
                "emptyList",
                "()Ljava/util/List;",
                &[],
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        imp,
        "getLocalSupportedSignatureAlgorithms",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The null session (no connection recorded) lists none, as HotSpot does.
            if session_sni(&*ctx, this).is_some() {
                let arr = materialize_java_string_array(ctx, &JDK_DEFAULT_SIGNATURE_ALGORITHMS);
                return Ok(Some(Value::Object(Some(arr))));
            }
            let string_id = crate::lang_class::reflection_component_id(ctx, "java/lang/String");
            let empty = ctx.new_ref_array(string_id, 0);
            Ok(Some(Value::Object(Some(empty))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        imp,
        "getPeerSupportedSignatureAlgorithms",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A server session lists the client offer, a client session none (HotSpot, measured).
            let server_side = session_sni(&*ctx, this).is_some_and(|(_, server)| server);
            if server_side {
                let arr = materialize_java_string_array(ctx, &JDK_DEFAULT_SIGNATURE_ALGORITHMS);
                return Ok(Some(Value::Object(Some(arr))));
            }
            let string_id = crate::lang_class::reflection_component_id(ctx, "java/lang/String");
            let empty = ctx.new_ref_array(string_id, 0);
            Ok(Some(Value::Object(Some(empty))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(imp, "getTicketCreationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.invoke_virtual(this, "getCreationTime", "()J", &[])
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(imp, "isPSK", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))), cratonvm_native_api::NativeKind::SyntheticStub);
    for (name, desc) in [
        (
            "exportKeyingMaterialKey",
            "(Ljava/lang/String;Ljava/lang/String;[BI)Ljavax/crypto/SecretKey;",
        ),
        ("exportKeyingMaterialData", "(Ljava/lang/String;[BI)[B"),
        (
            "exportKeyingMaterial",
            "(Ljava/lang/String;Ljava/lang/String;[BI)Ljava/lang/Object;",
        ),
    ] {
        r.register(imp, name, desc, |_ctx, _args| {
            Err(
                cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                    message: "exportKeyingMaterial is not supported by this session".into(),
                }
                .into(),
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    }

    let rows: Vec<(String, String, NativeKind)> = r
        .dump_registrations()
        .into_iter()
        .filter(|(c, _, _, _)| *c == "javax/net/ssl/SSLSession")
        .map(|(_, m, d, k)| (m.to_string(), d.to_string(), k))
        .collect();
    // Every mirrored row is a `SyntheticStub` whatever its source row's kind: under `--real-jdk` the
    // Impl is the JDK's own object running its own bytecode, and a row that survived there (the
    // source rows are `Bridge`s) answered from carrier slots a real `SSLSessionImpl` does not have, so
    // an engine pair's sessions read as the null session and `putValue` was lost
    // (`jdk-only-engine-session-api-does-not-complete-or-store-values-FIXED-20261010.md`).
    for (method, descriptor, _kind) in rows {
        if r.resolve_id(imp, &method, &descriptor).is_some() {
            continue;
        }
        let Some(id) = r.resolve_id("javax/net/ssl/SSLSession", &method, &descriptor) else {
            continue;
        };
        let Some(callback) = r.callback_of(id) else {
            continue;
        };
        r.register(
            imp,
            &method,
            &descriptor,
            callback,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    }

/// Did the `javax/net/ssl/SSLSession` object `this` actually negotiate
/// anything?
///
/// This is the ONE predicate behind `SSLSession.isValid()` and the "does this
/// session have an id at all" test in `getId()`, because HotSpot answers both
/// from the same underlying state — measured, HotSpot 25.0.3+9-LTS
/// (`scratchpad/e12/E12SessionContract.java`, recorded in
/// `docs/feature-designs/native-dispatch-architecture.md`
/// §1):
///
/// ```text
///   nothing negotiated:  isValid() = false   getId() = byte[0]
///   after a handshake:   isValid() = true    getId() = byte[32]
/// ```
///
/// Keeping the two in one function is deliberate. E3-1's recorded lesson in
/// this same family is that a rule spread over N call sites drifts, and this
/// rule now has to hold across THREE different widths of synthetic
/// `SSLSession` — the "nothing negotiated" signal lives in a different place
/// in each, so the field count is the discriminator (the same convention
/// `getProtocol`/`getCipherSuite` already use):
///
/// | width | minted by | slot 2 holds | negotiated when |
/// |---|---|---|---|
/// | 4 | `ssl_security::new13_alloc_{,null_}ssl_session`; `SSLServerSocket.accept` in this file | `NEW13_SESS_TLSID` — the stream id | a stream id was recorded (`>= 0`); the NULL session carries `-1` |
/// | 6 | `http2.rs`'s `HttpResponse.sslSession()`; `tls.rs` | — | always — the shape is only minted after a handshake |
/// | 8 | `build_synthetic_ssl_session` in this file | the `isValid` flag | the flag is set |
///
/// E42 — **widths 2 and 3 were RETIRED, and the 4-row is the merge of what
/// used to be two rows.** `ssl_security`'s NEW-13 shape widened from 3 to 4 so
/// that `putValue` has a slot of its own (see `NEW13_SSL_SESS_FIELDS` and
/// `sslsess_attrs_slot`), and its 2-field `SSLEngine.getSession` fallback now
/// calls `new13_alloc_null_ssl_session` like everything else. Width 4 is
/// byte-identical to the accept shape — proto, cipher, streamId, attrs — so
/// this is ONE row and not a coincidence of two.
///
/// The width-4 row is the one that decides an unconnected `SSLSocket`: a
/// field-count test alone would treat every narrow shape as negotiated and
/// leave the null session answering `isValid() = true` / a 32-byte id, which
/// is the exact defect in the mode this file is live in. The `0..=3` arms are
/// kept as defensive lower bounds — nothing mints those widths any more, and
/// nothing may be allowed to answer "negotiated" if something starts.
///
/// E31: `pub` since 2026-08-13. `tls.rs` had grown its OWN width-blind
/// copies of this question (`isValid` on `javax/net/ssl/SSLSession` and again
/// on `sun/security/ssl/SSLSessionImpl`, each testing a field count and
/// answering `true` for everything narrower), which is how the null session
/// stayed valid under `--synthetic-jdk` after this predicate fixed it in the
/// default mode. There is one rule; it lives here; both files call it.
pub fn session_has_negotiated(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    match session_shape_width(&*ctx, this) {
        // No slot at all, and never minted for a negotiated session.
        0..=2 => false,
        // E42: 3 and 4 are ONE arm. `ssl_security`'s NEW-13 shape widened from
        // 3 to 4 so that `putValue` has a slot of its own instead of writing a
        // HashMap over the stream id (see `NEW13_SSL_SESS_FIELDS`), and width 4
        // is byte-identical to `SSLServerSocket.accept`'s shape below: proto,
        // cipher, streamId, attrs.
        //
        // Merging them is a no-op for the accept shape and the fix for the
        // widened one. accept writes `RUSTLS_SOCK_ID_BASE + stream_id` into
        // slot 2, which is always `>= 0`, so `_ => true`'s answer for it is
        // unchanged — and that arm's premise ("4- and 6-field shapes are only
        // minted after a handshake") is now FALSE for width 4, which is
        // precisely why it cannot stay. `ssl_security`'s null session carries
        // `Int(-1)`.
        //
        // Pinned from the other side by
        // `phases_late::ssl_security::new13_tests
        // ::the_widened_null_session_is_still_not_negotiated`, which calls this
        // function directly: if this arm regresses, that test fails rather than
        // `RSslNullSession` silently reporting a valid null session again. The
        // in-file pair `the_null_socket_session_has_no_id_and_is_not_valid` /
        // `a_session_that_negotiated_keeps_its_id_and_its_validity` covers both
        // directions at both widths without leaving this crate.
        3 | 4 => match sess_get(&*ctx, this, 2) {
            Value::Int(tls_id) => tls_id >= 0,
            // Defensive: a non-`Int` here used to mean `putValue` had
            // overwritten the stream id — the collision the widening removes.
            // Kept because this predicate is also reached from `tls.rs` and
            // from any future minter that has not been audited: an unreadable
            // slot must not silently answer "never negotiated" for a session
            // that did.
            _ => true,
        },
        n if n >= 7 => matches!(sess_get(&*ctx, this, 2), Value::Int(v) if v != 0),
        // The 6-field shape is only minted after a handshake.
        _ => true,
    }
}

/// The slot holding the negotiated CIPHER SUITE, by shape width — and its
/// twin below for the PROTOCOL. Two conventions exist and the width is what
/// separates them:
///
/// | width | minted by | slot 0 | slot 1 |
/// |---|---|---|---|
/// | 2 | `ssl_security`'s `SSLEngine.getSession` fallback | protocol | cipher |
/// | 3 | `ssl_security::new13_alloc_{,null_}ssl_session` | protocol | cipher |
/// | 4 | `SSLServerSocket.accept` in this file | protocol | cipher |
/// | 6 | `tls.rs::init_ssl_session_fields`; `http2.rs` | **cipher** | **protocol** |
/// | 8 | `build_synthetic_ssl_session` in this file | **cipher** | **protocol** |
///
/// E31: the threshold used to be `>= 7`, which put the 6-field row on the
/// wrong side of the line — `getProtocol()` on a `tls.rs` session read its
/// cipher slot and vice versa. That was inert only because the 6-field shape
/// stores `Int` indices rather than `String`s, so both accessors fell through
/// to the sentinel arm instead of returning the *other* value; a producer that
/// ever wrote real names into that shape would have swapped them. Stating the
/// rule as `>= 6` makes it true rather than harmlessly false, and changes no
/// answer today (verified against every minter in the table).
///
/// `None` for a shape too short to carry the pair at all. Both accessors used
/// to index slot 0 or 1 with no floor, so a foreign or 1-field receiver was an
/// out-of-range field read on a class the real JDK also defines — the shape
/// this directory records as a real bug rather than a miss. Returning an
/// `Option` is what makes that check unskippable at the call site.
pub fn session_cipher_slot(num_fields: usize) -> Option<usize> {
    match num_fields {
        0..=1 => None,
        n if n >= 6 => Some(0),
        _ => Some(1),
    }
}

/// See [`session_cipher_slot`] for the width table this mirrors.
pub fn session_proto_slot(num_fields: usize) -> Option<usize> {
    match num_fields {
        0..=1 => None,
        n if n >= 6 => Some(1),
        _ => Some(0),
    }
}

/// The dedicated `SSLSession` attribute-map slot for this shape, or `None` for
/// a shape that has no slot to spare.
///
/// **E31 — this replaces a bare `num_fields - 1` at five call sites, and the
/// bare form was a live slot collision.** `num_fields - 1` is the attribute
/// slot on some widths and a field that means something else on others, and
/// `putValue` wrote a `java.util.HashMap` straight over it:
///
/// | width | `num_fields - 1` is | what a `putValue` destroyed |
/// |---|---|---|
/// | 4 | a dedicated attrs slot | nothing |
/// | 6 | `tls.rs::SES_CREATION_TIME` | `getCreationTime()` returns an object reference through a `()J` descriptor |
/// | 8 | a dedicated attrs slot | nothing |
///
/// E42 — **the 2- and 3-field rows are gone, and they were the two worst.**
/// The 2-field `SSLEngine.getSession` fallback now calls
/// `ssl_security::new13_alloc_null_ssl_session`, and the 3-field NEW-13 shape
/// widened to 4 *for this reason*: on it, `num_fields - 1` was
/// `ssl_security::NEW13_SESS_TLSID`, which is also the slot
/// [`session_has_negotiated`] reads to decide whether anything was negotiated.
/// A single `putValue` on an unconnected socket's session therefore turned
/// `Int(-1)` ("nothing negotiated") into `Object(Some(map))`, which took that
/// function's defensive `_ => true` arm — so `isValid()` went back to `true`
/// and `getId()` went back to 32 fabricated bytes. **A `putValue` resurrected
/// the exact fabrication the E12/E22 lanes removed**, on the one shape whose
/// whole purpose is to represent "no handshake happened". The defensive arm is
/// what kept that from being a *new* wrong answer relative to the pre-E12
/// code; it was never a reason the collision was safe.
///
/// Note the two halves of the width-4 agreement, because either alone is a
/// silent regression: `4 => Some(3)` is only correct while
/// `ssl_security::NEW13_SESS_ATTRS == NEW13_SSL_SESS_FIELDS - 1` and
/// `!= NEW13_SESS_TLSID`, and slot 2 is only a stream id while
/// [`session_has_negotiated`]'s `3 | 4` arm reads it as one. Both are held by
/// tests rather than by this comment — this file's
/// `tests::a_widened_null_session_put_value_does_not_touch_the_stream_id`, and
/// `phases_late::ssl_security::new13_tests
/// ::the_attribute_slot_is_the_last_one_and_is_not_the_stream_id` for the
/// constants' side of the agreement.
///
/// Jetty's `SecureRequestCustomizer.retrieveSni()` calls `getValue()` then
/// `putValue()` on every SSL request, so the writer is not hypothetical — it
/// is the caller the attribute API was registered for in the first place.
///
/// Returning `None` (rather than widening the shapes) was the change that
/// could not break anything else: `putValue`/`removeValue` become no-ops and
/// `getValue`/`getValueNames` answer null/`String[0]`, which is what those
/// shapes already answer today for a session nobody has written to. The
/// divergence that remains — a `putValue` on a 6-field session does not
/// round-trip through `getValue` — is a *quiet* miss on a JSSE convenience
/// API, against a *loud* corruption of the negotiation state.
///
/// E42 closed the 3-field half of that residual by widening rather than by
/// refusing, together with the `session_has_negotiated` arm merge it required;
/// see `docs/feature-designs/native-dispatch-architecture.md`.
/// The 6-field shape is now the **only** one with no attribute slot, and it is
/// what `_ => None` is for.
pub fn sslsess_attrs_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    match session_shape_width(&*ctx, this) {
        4 => Some(3),
        n if n >= 7 => Some(n - 1),
        _ => None,
    }
}

/// Fire `SSLSessionBindingListener.valueBound`/`valueUnbound` for a value that
/// implements the interface, the way `SSLSessionImpl.putValue`/`removeValue`
/// do.
///
/// JSSE's contract is explicit: "if the object implements
/// SSLSessionBindingListener, the valueBound method is called". netty's
/// `SSLEngineTest.assertSSLSessionBindingEventValue` is a listener that
/// records the event it was handed and asserts on `event.getName()`; with no
/// callback the recorded event stayed null and the test died on
/// `NullPointerException: Cannot invoke
/// "javax.net.ssl.SSLSessionBindingEvent.getName()" because "event" is null`.
///
/// A value that is not a listener, or an event that cannot be constructed, is
/// silently skipped — the attribute store is the primary effect and must not
/// fail because of a callback.
pub fn fire_session_binding(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    name: Value,
    value: Value,
    bound: bool,
) {
    let Value::Object(Some(v)) = value else {
        return;
    };
    let Some(iface) = ctx.class_id_by_name("javax/net/ssl/SSLSessionBindingListener") else {
        return;
    };
    if !ctx.is_subclass(ctx.class_id_of_object(v), iface) {
        return;
    }
    let ses_pin = ctx.pin_native_root(session);
    let v_pin = ctx.pin_native_root(v);
    let ses_now = ctx.read_native_pin(ses_pin, session);
    let event = ctx.new_object_initialized(
        "javax/net/ssl/SSLSessionBindingEvent",
        "(Ljavax/net/ssl/SSLSession;Ljava/lang/String;)V",
        &[Value::Object(Some(ses_now)), name],
    );
    if let Ok(Some(Value::Object(Some(ev)))) = event {
        let ev_pin = ctx.pin_native_root(ev);
        let v_now = ctx.read_native_pin(v_pin, v);
        let ev_now = ctx.read_native_pin(ev_pin, ev);
        let method = if bound { "valueBound" } else { "valueUnbound" };
        let _ = ctx.invoke_virtual(
            v_now,
            method,
            "(Ljavax/net/ssl/SSLSessionBindingEvent;)V",
            &[Value::Object(Some(ev_now))],
        );
    }
    ctx.unpin_native_roots(ses_pin);
}

pub fn register_ssl_session_real(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLSession";

    // toString() — JSSE's `SSLSessionImpl.toString()` is
    // `"Session(" + creationTime + "|" + getCipherSuite() + ")"`, and it is the
    // tail of `SSLEngineImpl.toString()` / `SSLSocketImpl.toString()` as well as
    // an answer in its own right. CratonVM's session objects carry the bare
    // `javax/net/ssl/SSLSession` INTERFACE as their class, so with no
    // registration here a virtual `toString()` resolves to `Object.toString()`
    // and prints an identity hash — which is not wrong so much as useless, and
    // it is the half of `SSLEngine.toString()` a reader actually wants.
    //
    // Both fields come from this file's own accessors, so a session that cannot
    // answer one still renders rather than throwing: JSSE's own creationTime is
    // a `long` and its cipher suite is never null (it is
    // `SSL_NULL_WITH_NULL_NULL` before a handshake), so 0 and the JSSE null
    // suite are the values that make the string mean what it means.
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // gc-common w20-e: `getCreationTime()` runs Java; the second getter
        // is dispatched on the session's current address, not its entry one.
        let this_pin = ctx.pin_native_root(this);
        let created = match ctx.invoke_virtual(this, "getCreationTime", "()J", &[]) {
            Ok(Some(v)) => v.as_long().unwrap_or(0),
            _ => 0,
        };
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        let suite = match ctx.invoke_virtual(this, "getCipherSuite", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx
                .read_string(s)
                .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into()),
            _ => crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string(),
        };
        let text = format!("Session({created}|{suite})");
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getPeerCertificates() — the client certificate chain, for mTLS. Tomcat's
    // SSLAuthenticator / coyote SSLSupport reads this to authenticate the
    // client; without it a client-cert-protected resource returns HTTP 401.
    // Build real `sun.security.x509.X509CertImpl` mirrors from the captured DER
    // (same path the keystore uses). Empty chain → throw
    // SSLPeerUnverifiedException (real-JDK contract), which Tomcat treats as
    // "no client cert".
    //
    // F18: the chain now comes from `peer_certs_for_session` rather than from
    // an inline `session_peer_certs_table` lookup, so this door and
    // `ssl_security`'s `getPeerPrincipal` cannot see different chains for one
    // session — they are the same function call. See that helper for the
    // measured HotSpot contract the two have to agree on. This is a strict
    // widening for THIS door: it keeps the object-table answer it already gave
    // and gains the stream-id fallback its neighbour had, which is why the
    // change cannot make an answer that used to be a chain become empty.
    r.register(
        cls,
        "getPeerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = peer_certs_for_session(ctx, this);
            if chain.is_empty() {
                // Real-JDK contract (and this function's own doc): throw
                // SSLPeerUnverifiedException — an SSLException — NOT
                // IllegalStateException. Callers specifically catch the
                // former to mean "peer presented no certificate": e.g.
                // Spring's DefaultSslInfo.initCertificates() swallows
                // SSLPeerUnverifiedException when building SslInfo for a
                // server session without client auth; the previous
                // IllegalStateException escaped instead and failed every
                // reactive HTTPS request
                // (ServerHttpsRequestIntegrationTests::checkUri).
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLPeerUnverifiedException",
                    "peer not authenticated",
                ));
            }
            // i6-L2: `X509Certificate[]`, HotSpot's runtime type here (its
            // `SSLSessionImpl.peerCerts` is one), so the common
            // `(X509Certificate[]) session.getPeerCertificates()` holds.
            let component = crate::lang_class::reflection_component_id(
                ctx,
                "java/security/cert/X509Certificate",
            );
            let arr = ctx.new_ref_array(component, chain.len());
            // `make_x509_mirror` runs the `X509CertImpl` constructor (Java, a
            // GC point): keep the result array pinned and re-read it per store.
            let arr_pin = ctx.pin_native_root(arr);
            for (i, der) in chain.iter().enumerate() {
                let mirror = match crate::keystore::make_x509_mirror(ctx, "peer", der) {
                    Ok(mirror) => mirror,
                    Err(e) => {
                        ctx.unpin_native_roots(arr_pin);
                        return Err(e);
                    }
                };
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // KEEP THE VALUES — but the claim that used to head this block was WRONG,
    // and wrong in the confident register that stops the next reader checking.
    // It said "KEEP (correct constants…) the real JDK returns 16384 for
    // getApplicationBufferSize". Measured on HotSpot 25.0.3+9-LTS
    // (`scratchpad/e12/E12SessionContract.java`, E12-1 §1):
    //
    //     getApplicationBufferSize()  null session      = 16704
    //     getApplicationBufferSize()  after TLS 1.3     = 16676
    //     getPacketBufferSize()       every state       = 16709
    //
    // So `getPacketBufferSize` is right and `getApplicationBufferSize` is NOT
    // a constant in real JSSE at all: `SSLSessionImpl` derives it from the
    // packet size minus the negotiated suite's record expansion, which is why
    // it moves when a suite is negotiated. 16384 is the RFC 8446 §5.1
    // TLSPlaintext cap, i.e. a floor rather than the JDK's answer.
    //
    // G25 — the number is fixed now too, and the audit that comment asked for
    // was done. RE-MEASURED (`G25Probe`, HotSpot 25.0.3+9), one row per state,
    // both suites, over a real in-memory TLS 1.3 handshake:
    //
    //     never negotiated                     app = 16704   packet = 16709
    //     TLS 1.3 / TLS_AES_256_GCM_SHA384     app = 16676   packet = 16709
    //     TLS 1.3 / TLS_AES_128_GCM_SHA256     app = 16676   packet = 16709
    //
    // Two states, two answers, and no third: within TLS 1.3 the value does not
    // move with the suite, so a `negotiated?` predicate is the whole of the
    // state-dependence and no per-suite table is needed. 16384 was neither
    // value — it is the RFC 8446 §5.1 TLSPlaintext cap, a floor.
    //
    // **Why raising it is safe, which is the part the old comment could not
    // check.** `do_wrap` reads at most 16384 bytes of plaintext out of `srcs`
    // per call (`bb_read_into(ctx, *bb, &mut app_bytes, 16384)` and the
    // `app_bytes.len() >= 16384` break) whatever the caller offers, and then
    // drains only COMPLETE records that fit the destination. So a caller that
    // sizes its application buffer from this accessor and fills it hands over
    // 16704 bytes, this engine consumes 16384 of them, emits one record of at
    // most 16406 into a `getPacketBufferSize()`-sized destination, and reports
    // the true `bytesConsumed`. There is no state in which the larger number
    // makes a record that cannot fit — the livelock the old comment feared
    // needs the engine to promise a record bigger than its packet buffer, and
    // the 16384 cap is what stops it. On `unwrap`, a larger destination is
    // only ever safer.
    //
    // The under-report was not free: a caller that sizes a receive buffer from
    // this accessor and a peer that fills a genuine 16676-byte application
    // record put 292 bytes more on the wire than the buffer holds, which is a
    // `BUFFER_OVERFLOW` retry loop against a buffer the caller has already
    // been told is big enough.
    //
    // Same correction is owed to the twin in `tls.rs` — see E22-1's
    // NOMINATION, and note that the twin is `#[cfg(feature =
    // "synthetic-jdk")]`-only, so THIS copy is the live one by default.
    //
    // SHADOWING (wave 4 correction — the wave-3 note was wrong): `tls.rs
    // ::register_ssl_session` registers the same two triples, but its registrar
    // `register_tls_natives` is reached ONLY from `register_synthetic_overrides`,
    // which is `#[cfg(feature = "synthetic-jdk")]`. In the
    // DEFAULT real-JDK build THIS copy is the live one and the tls.rs pair does
    // not exist; under `--synthetic-jdk` tls.rs runs later and wins. The values
    // are identical either way — if you change one, change both.
    r.register(cls, "getApplicationBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if session_has_negotiated(ctx, this) {
            JSSE_APPLICATION_BUFFER_SIZE_NEGOTIATED
        } else {
            JSSE_APPLICATION_BUFFER_SIZE_FRESH
        })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getPacketBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(JSSE_PACKET_BUFFER_SIZE)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // getId() — Tomcat's request/auth plumbing reads the TLS session id (e.g.
    // for SSL session tracking / client-cert requests). Real JDK returns the
    // negotiated session id bytes; the abstract interface declaration has no
    // Code, so without a real-mode native this throws AbstractMethodError and
    // every HTTPS request to a protected resource fails (HTTP -1). Return a
    // stable 32-byte id derived from the session object's identity — but ONLY
    // for a session that actually negotiated one.
    //
    // E12: a session that negotiated nothing has NO id, and JSSE says so with
    // `byte[0]` rather than 32 plausible bytes. This has a named, measured
    // consumer: Tomcat's `JSSESupport.getSessionId`
    // (java/org/apache/tomcat/util/net/jsse/JSSESupport.java:171) is
    // `if (ssl_session == null || ssl_session.length == 0) return null;` — it
    // tests `length == 0` EXACTLY, so the fabricated 32 bytes are precisely
    // the value that defeats it and makes an unhandshaken session present as
    // a trackable one. Measured HotSpot 25.0.3+9-LTS, E12-1 §1/§4.
    r.register(cls, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // ONE gate, and G7 removed the second one. `session_has_negotiated` is
        // the width-aware predicate every other door in this file uses
        // (E12/E42): on the 4-field socket shapes it reads the stream id in
        // slot 2, and a session that negotiated nothing carries `-1`; on the
        // 7/8-field engine shape it reads the `isValid` flag that
        // `build_synthetic_ssl_session` writes from
        // `conn.negotiated_cipher_suite().is_some()`.
        //
        // **The second gate produced `byte[0]` in exactly one state, and that
        // is the state where the oracle says 32 bytes.** It was
        // `engine_shape && !session_is_negotiated(...)` —
        // `negotiated_session_keys` membership, which `engine_session_for`
        // writes only in the handshake-COMPLETED epoch. Enumerate what it could
        // add over the first gate:
        //
        //   * fresh engine, no `conn`      slot 2 = 0 -> gate 1 already refuses
        //   * mid-handshake, past ServerHello  slot 2 = 1, not yet in the set
        //     -> gate 1 admits, gate 2 REFUSED
        //   * completed                    slot 2 = 1, in the set -> both admit
        //
        // So the only row it decided was mid-handshake. MEASURED, HotSpot
        // 25.0.3+9-LTS, `scratchpad/g7/TlsProbe.java`, sampled inside
        // `X509ExtendedTrustManager.checkServerTrusted(chain, authType, Socket)`
        // and again inside the `SSLEngine` overload:
        //
        // ```text
        //   getId          = byte[32] 2310720247a7e8...  (and byte-identical to
        //                    the id the COMPLETED session then reported)
        //   getCipherSuite = "TLS_AES_256_GCM_SHA384"
        //   getProtocol    = "TLSv1.3"
        //   isValid        = true
        //   getSessionContext        = null
        //   getPeerCertificates      = THROWS SSLPeerUnverifiedException
        // ```
        //
        // netty's `SSLEngineTest.testSSLSessionId` — the test the second gate
        // was added for — asserts `assertEquals(0, engine.getSession().getId()
        // .length)` on a FRESHLY CREATED engine, which is the first row above,
        // and gate 1 alone already answers it: no `conn` means no negotiated
        // cipher suite means slot 2 is `0`. Dropping gate 2 cannot regress it.
        //
        // The gate itself was not deleted — it moved to `getSessionContext`,
        // which is the door the oracle shows needs exactly this discrimination
        // (`null` mid-handshake, non-null once complete).
        //
        // RESIDUAL, and it is a real one: HotSpot's handshake session IS the
        // session, so the 32 bytes read mid-handshake are the same 32 bytes the
        // completed session reports. Here `engine_session_for` caches on
        // `(engine, handshaked)`, so the two are DIFFERENT objects and the
        // pseudo-id below — seeded from `gc_stable_objref_key` — differs
        // between them. The LENGTH is now right (which is what Tomcat's
        // `JSSESupport.getSessionId` tests, `length == 0` exactly) and the
        // CONTINUITY is not. Merging the two cache entries is not a free fix:
        // the pre-handshake entry has the "nothing negotiated" sentinels frozen
        // into slots 0/1 at mint time, so reusing that object after the
        // handshake would make a completed session report
        // `SSL_NULL_WITH_NULL_NULL`. Recorded in G7-1 §4 rather than half-done.
        //
        // Hoisted into a `let` rather than chained inline: `session_has_negotiated`
        // takes `&dyn`, and this file already records (see `getPeerHost`) how
        // easily a reborrow in a compound scrutinee collides with an `&mut`
        // beside it.
        let negotiated = session_has_negotiated(ctx, this);
        if !negotiated {
            return Ok(Some(Value::Object(Some(
                ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0),
            ))));
        }
        // The REAL id, when there is one: a TLS 1.2 handshake puts a session id
        // on the wire and both engines saw it, so both answer the same bytes.
        // `SSLEngineTest.testSSLSessionId` asserts exactly that equality — a
        // per-object pseudo-id can never satisfy it. (Under TLS 1.3 nothing is
        // recorded here and the pseudo-id below stands, which is what the same
        // test's `assertFalse(Arrays.equals(...))` branch wants.)
        // Key before the guard — see the store site.
        let wire_key = gc_stable_objref_key(ctx, this);
        let wire_id = session_wire_id_table().lock().get(&wire_key).cloned();
        if let Some(id) = wire_id {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, id.len());
            for (i, b) in id.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
        // The same (minted, per-live-session) key seeds the pseudo-id, as in
        // `session_id_bytes` (gc-common w26-b).
        let seed = wire_key;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        // SplitMix64-style fill so the 32 bytes are stable per session and not
        // all-identical (some callers hash or compare the id).
        let mut x = seed ^ 0x9E37_79B9_7F4A_7C15; // gc-common w33-a: `| 1` merged keys differing in bit 0
        for i in 0..32 {
            x ^= x >> 30;
            x = x.wrapping_mul(0xbf58476d1ce4e5b9);
            x ^= x >> 27;
            ctx.set_array_element(arr, i, Value::Int((x & 0xff) as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // proto/cipher slot order differs between the shapes; the width is what
    // separates the two conventions. E31: the table (and the off-by-one this
    // comment used to carry — it named a "7-field" and a "3-field" shape that
    // are actually 8 and 4) now lives on `session_cipher_slot`, which both
    // accessors below call instead of re-deriving `>= 7` twice.
    //
    // E12: and NEVER hand back a null String. Both of these are contracted
    // non-null by JSSE, and both used to return `ctx.get_field(...)` raw — so
    // a shape whose slot was never populated (e.g. `http2.rs`'s 6-field
    // `HttpResponse.sslSession()` session, which is allocated and never
    // written) produced a null String from a method that cannot return one,
    // and the caller NPE'd on `.equals`/`.startsWith`. The sentinel is the
    // right answer there because an unpopulated slot IS "nothing negotiated";
    // see `JSSE_NULL_CIPHER_SUITE` for why it is honesty and not invention.
    r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Bound to a `let` on purpose: a closure written directly in a match
        // scrutinee is a temporary whose `&ctx` capture lives to the end of the
        // match, which collides with the `&mut ctx` the sentinel arms need.
        let raw = session_proto_slot(session_shape_width(&*ctx, this))
            .map(|slot| sess_get(&*ctx, this, slot));
        match raw {
            Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
            _ => {
                let s = ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL);
                Ok(Some(Value::Object(Some(s))))
            }
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cls,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // See `getProtocol` above for why this is a `let` and not an
            // inline match scrutinee.
            let raw = session_cipher_slot(session_shape_width(&*ctx, this))
                .map(|slot| sess_get(&*ctx, this, slot));
            match raw {
                Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
                _ => {
                    let s =
                        ctx.create_string(crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // E12: `isValid` and `getId` answer from ONE predicate, because HotSpot
    // answers them from one piece of state — see `session_has_negotiated`.
    // This used to return `Value::Int(1)` for every shape narrower than 7
    // fields on the reasoning that "the accept session was just negotiated".
    // That reasoning holds for the accept shape and NOT for the NEW-13 shape,
    // which `ssl_security::new13_resolve_socket_session` also mints — with
    // `tls_id = -1` — for a socket that was never connected. So an unconnected
    // `SSLSocket.getSession().isValid()` answered `true` where HotSpot measures
    // `false` (E12-1 §1 arm A).
    //
    // E42: those are now the SAME WIDTH (4), which is why the width alone can
    // never answer this and `session_has_negotiated` must read slot 2. The two
    // shapes are distinguished by the VALUE there — `-1` versus a real stream
    // id — and by nothing else.
    //
    // Deliberately NOT tied to stream liveness: measured (arm G), closing the
    // socket does NOT invalidate the session — it outlives its transport, for
    // resumption — so validity must not be a "is the stream still in the
    // registry" test. Reading the session's own recorded id keeps the answer
    // stable across `close()`.
    //
    // SHADOWING (wave 3): `tls.rs::register_ssl_session` registers the same
    // triple and runs later, so THAT one wins under `--synthetic-jdk`; in the
    // default real-JDK mode (which is what `--real-jdk` runs) THIS one is
    // live — confirmed against `--dump-native-registry`, not source order.
    // Its version was slot-2-only and mis-reported the 3-field accept session;
    // it has been made field-count aware to match this logic. Keep in step.
    //
    // F18 — THE SECOND HALF OF THE PREDICATE, which this VM could not model
    // until `invalidate()` had a writer in this mode. HotSpot's `isValid()` is
    // `sessionId.length() != 0 && !invalidated && ...`; `session_is_valid`
    // composes both halves and is now the one function both modes call. It
    // reduces to `session_has_negotiated` exactly when nothing has ever been
    // invalidated, so this line changes no answer in a process that never
    // calls `invalidate()`. See `session_is_valid` for the measurement on a
    // genuinely negotiated session.
    r.register(cls, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if session_is_valid(ctx, this) {
            1
        } else {
            0
        })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // invalidate() -> void
    //
    // F18 (F10-1 NOMINATION 3). This had **no registration in real-JDK mode at
    // all** — `javax.net.ssl.SSLSession` is an interface whose `invalidate()`
    // has no Code attribute, so the shipping mode threw `AbstractMethodError`.
    // It was unreachable in practice only while every session this VM handed
    // out answered `isValid() == false` anyway; F10's minter fix made completed
    // HTTPS handshakes report `isValid() == true`, so a program that checks
    // validity and then invalidates now gets here.
    //
    // Both halves of that nomination land together — the registration AND a
    // state model that makes it mean something at width 4 — because
    // `session_mark_invalidated` records the bit outside the object. See
    // `session_invalidated_table` for why that is not the widening F6-1 §4 and
    // E42-1 §2 reject, and `session_is_valid` for the measured contract.
    r.register(cls, "invalidate", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        session_mark_invalidated(ctx, this);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getPeerHost() / getPeerPort()
    //
    // F18 — neither had ANY real-JDK-mode registration. `grep -rn '"getPeerHost"'`
    // over `native-builtins/src/` returns only `tls.rs`, whose registrar
    // `register_tls_natives` is reached solely from
    // `register_synthetic_overrides` (`#[cfg(feature = "synthetic-jdk")]`), so
    // in the mode `--real-jdk` runs both were `AbstractMethodError`. E12-1's
    // residual 4 recorded these as *answering the wrong value* — it was reading
    // `build_synthetic_ssl_session`, the producer. The accessor side is worse
    // than wrong; it is absent.
    //
    // MEASURED, HotSpot 25.0.3+9-LTS (`scratchpad/f18/F18SessionContract.java`,
    // three runs byte-identical):
    //
    //   unconnected SSLSocket / pre-handshake SSLEngine   host=null   port=-1
    //   completed HTTPS client handshake                  host=localhost
    //                                                     port=<the server port>
    //   after invalidate()                                unchanged (both)
    //
    // Note HotSpot reports the host the caller ASKED for, not the certificate's
    // subject — `localhost` here, while the leaf certificate is also
    // `CN=localhost`; do not "verify" one against the other.
    //
    // Width handling, which is the trap this family keeps falling into:
    // slot 3 is the peer host on the 6- and 8-field shapes and the ATTRIBUTE
    // MAP on the 4-field one, so a slot read with no width test would return a
    // `java.util.HashMap` through a `()Ljava/lang/String;` descriptor as soon
    // as anything had called `putValue` — the exact defect E31-1 §2 records
    // against `tls.rs`'s copies. The `>= 6` threshold is
    // `session_cipher_slot`'s, deliberately the same number so there is one
    // width boundary in this file and not two.
    //
    // For the 4-field shapes the answer comes from the socket registry via
    // `session_stream_id`, which is where the host and port a client actually
    // dialled are recorded (`servlet::s2_tls_session_info`'s third and fourth
    // components). That is a real answer rather than a fallback, and it is the
    // half E12-1's residual 4 said was missing.
    r.register(cls, "getPeerHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if session_shape_width(&*ctx, this) >= 6 {
            // Only a String counts: `tls.rs::init_ssl_session_fields` seeds
            // this slot with `Int(0)`, and `build_synthetic_ssl_session` may
            // leave it unwritten.
            if let v @ Value::Object(Some(_)) = sess_get(&*ctx, this, 3) {
                return Ok(Some(v));
            }
            return Ok(Some(Value::Object(None)));
        }
        // G51: the object-keyed endpoint table, consulted BEFORE the socket
        // registry and AFTER the width branch above. See
        // `session_peer_endpoint_table` for the measured family and for why the
        // answer cannot live in a session slot on this shape.
        //
        // Order matters in one direction only. A recorded row is the endpoint
        // the caller NAMED for this exact session object; the registry lookup
        // below is keyed on a TLS stream id, and the shape that most needs an
        // answer here — the HTTPS client session — carries
        // `net_phase_e::HTTPS_CLIENT_SESSION_MARKER` in slot 2 precisely so
        // that every socket-registry lookup MISSES (see that constant: it
        // cannot be given a real stream id without leaking one registry entry
        // per request). So for that shape the fallback cannot answer by
        // design, and for the accepted server session the registry has no row
        // either — `s2_tls_session_info` reads `servlet`'s native-tls table,
        // and an accepted rustls stream lives in this file's `server_streams`.
        // Same `let`-binding discipline as the fallback below.
        let recorded = session_peer_endpoint(ctx, this)
            .map(|(host, _)| host)
            .filter(|h| !h.is_empty());
        if let Some(host) = recorded {
            let s = ctx.create_string(&host);
            return Ok(Some(Value::Object(Some(s))));
        }
        // Bound to a `let`, and NOT written as `if let Some(id) =
        // session_stream_id(ctx, this)`: the `&*ctx` reborrow in an `if let`
        // scrutinee is a temporary that lives to the end of the block under
        // Rust 2021's drop rules, which would collide with the `&mut ctx` that
        // `create_string` needs inside it. Same hazard `getProtocol` in this
        // file records against a match scrutinee.
        let host = session_stream_id(ctx, this)
            .and_then(servlet::s2_tls_session_info)
            .map(|(_, _, host, _)| host)
            .filter(|h| !h.is_empty());
        if let Some(host) = host {
            let s = ctx.create_string(&host);
            return Ok(Some(Value::Object(Some(s))));
        }
        // HotSpot's measured answer for a session that negotiated nothing.
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getPeerPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if session_shape_width(&*ctx, this) >= 6 {
            // `0` means UNWRITTEN, not "port zero". Both producers of a wide
            // shape that populate this slot write `-1` explicitly for "no peer"
            // (`build_synthetic_ssl_session` slot 4, `tls.rs
            // ::init_ssl_session_fields` `SES_PEER_PORT`), and `http2.rs`'s
            // 6-field session writes NO slot at all, so a zero here is the
            // allocator's fill. A connected peer never has port 0 — it is the
            // "any port" value a bind may ask for and never a value a peer
            // reports — so mapping it to HotSpot's measured `-1` cannot mask a
            // real answer.
            if let Value::Int(p) = sess_get(&*ctx, this, 4) {
                return Ok(Some(Value::Int(if p == 0 { -1 } else { p })));
            }
            return Ok(Some(Value::Int(-1)));
        }
        // G51 — the endpoint table, same position and same reasoning as
        // `getPeerHost` above. A recorded row whose port is non-positive is NOT
        // an answer: `record_session_peer_endpoint` accepts a host-only row
        // (nothing writes one today, but the shape is reachable), and falling
        // through to the registry there is right for the same reason an absent
        // row falls through.
        let recorded = session_peer_endpoint(ctx, this)
            .map(|(_, port)| port)
            .filter(|p| *p > 0);
        if let Some(p) = recorded {
            return Ok(Some(Value::Int(p)));
        }
        // Same `let`-binding discipline as `getPeerHost` above. This body has
        // no `&mut ctx` use inside the block today, so the hazard is latent
        // rather than live — which is exactly when it gets introduced.
        let port = session_stream_id(ctx, this)
            .and_then(servlet::s2_tls_session_info)
            .map(|(_, _, _, port)| port)
            .filter(|p| *p != 0);
        match port {
            Some(p) => Ok(Some(Value::Int(p as i32))),
            None => Ok(Some(Value::Int(-1))),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getSessionContext() -> SSLSessionContext
    //
    // F18 — registered NOWHERE in the crate, in either mode: `grep -rn
    // '"getSessionContext"'` over `native-builtins/src/` returns zero
    // registrations. Another abstract interface declaration, another
    // `AbstractMethodError`, and the last of the four `javax/net/ssl/SSLSession`
    // doors that had none (the others being `invalidate`, `getPeerHost` and
    // `getPeerPort`, all above).
    //
    // G7 — **this used to answer `null` unconditionally, and the premise the
    // constant rested on was false.** The comment it replaced said "This VM has
    // no `SSLSessionContext` — no session cache, no id-keyed lookup, no
    // timeout". SOURCE-VERIFIED, and it is not so: `net_phase_e
    // ::register_phase_e_networking` allocates a zero-field
    // `javax/net/ssl/SSLSessionContext` carrier for
    // `SSLContext.getClientSessionContext()` / `getServerSessionContext()` and
    // registers the WHOLE interface on it — `getIds()Ljava/util/Enumeration;`,
    // `getSession([B)Ljavax/net/ssl/SSLSession;`, `get/setSessionCacheSize(I)`,
    // `get/setSessionTimeout(I)`. So the object exists, it has a live method
    // surface, and applications already receive it through the other two doors.
    // Answering `null` here was not `--real-jdk` restraint; it was this door
    // disagreeing with its two siblings about whether the VM has a context.
    //
    // MEASURED, HotSpot 25.0.3+9-LTS, `scratchpad/g7/TlsProbe.java`
    // (loopback `SSLServerSocket` + a paired in-memory `SSLEngine`; see
    // `docs/known-issues/jdk-only/G7-1-*.md` §1 for the whole surface):
    //
    // ```text
    //   never-connected SSLSocket session        getSessionContext() = null
    //   fresh SSLEngine session                  getSessionContext() = null
    //   MID-HANDSHAKE (inside checkServerTrusted) getSessionContext() = null
    //   completed handshake                      = SSLSessionContextImpl
    //   resumed handshake                        = SSLSessionContextImpl
    //   HttpsURLConnection.getSSLSession().get() = SSLSessionContextImpl
    //   after invalidate()                       = null
    // ```
    //
    // Four states, and the rule behind them is exactly "is this session IN a
    // context" — a session enters one when its handshake COMPLETES and leaves
    // it when `invalidate()` evicts it (`ctx.getSession(id)` answered
    // SAME-OBJECT before the invalidate and `null` after, same probe). The
    // mid-handshake `null` is the row that matters most here: it is the one
    // state where the session already has its 32-byte id, its cipher suite and
    // `isValid() == true` and still has no context, so a gate on validity alone
    // would get it wrong.
    //
    // The two gates below are that rule, and the second one is the gate this
    // merge moved OFF `getId()`:
    //
    //   * `session_is_valid` = `session_has_negotiated && !invalidated`. It
    //     covers the never-negotiated states (slot-2 sentinel `-1`, or the
    //     engine shape's `isValid` flag `0`) and the invalidated one.
    //   * `session_is_negotiated` — the `negotiated_session_keys` membership
    //     `engine_session_for` writes ONLY in the handshake-completed epoch —
    //     covers mid-handshake. It is consulted for the 7/8-field engine shape
    //     alone, because that is the only shape whose object can be handed out
    //     while a handshake is still running (`SSLEngineImpl
    //     .getHandshakeSession` above). The socket door answers `null` for its
    //     whole handshake window, so no width-4 session is observable there.
    //
    // Both predicates already existed. `getId()` used to stack them and was
    // wrong for it — see its registration — and moving the second one here is
    // what keeps `negotiated_session_keys` load-bearing instead of leaving it a
    // write-only set.
    //
    // WHY A CARRIER AND NOT A FABRICATION. The interface's own escape hatch
    // ("This context may be unavailable in some environments, in which case
    // this method returns null" — `javax/net/ssl/SSLSession.getSessionContext`)
    // is still the right answer for the three states above that answer `null`.
    // What it does not license is claiming unavailability for a completed
    // handshake while `SSLContext.getClientSessionContext()` hands the same
    // application a context object one call away. The carrier this mints is
    // that same object, not a new shape: zero fields, `javax/net/ssl/
    // SSLSessionContext`, so every method on it lands on net_phase_e's
    // registrations rather than on an `AbstractMethodError`.
    //
    // TWO RESIDUALS, stated rather than hidden — both are net_phase_e's to
    // close and are NOMINATED in the record:
    //   1. `getIds()` answers an empty enumeration and `getSession(id)` answers
    //      `null`, where HotSpot's context lists this session and returns the
    //      SAME object for its id. A context that does not contain the session
    //      that pointed at it is a real inconsistency; it is bounded (rustls
    //      owns the cache and exposes no enumeration) and it is strictly less
    //      wrong than `null`, which fails a plain non-null check.
    //   2. `getSessionCacheSize()`/`getSessionTimeout()` read net_phase_e's
    //      `ssc_side_table` under an ORPHAN key, so they answer its default
    //      `0`/`0` where HotSpot measured `20480`/`86400`. `ssc_bind` is
    //      private to that file, so this carrier cannot be bound from here.
    r.register(
        cls,
        "getSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Hoisted, not chained: `session_is_valid` takes `&dyn` and
            // `session_is_negotiated` `&mut dyn`, and this file already records
            // (see `getPeerHost`, and `getId` below) how easily a reborrow in a
            // compound scrutinee collides with the `&mut` beside it.
            let valid = session_is_valid(ctx, this);
            let engine_shape = session_shape_width(&*ctx, this) >= 7;
            if !valid || (engine_shape && !session_is_negotiated(ctx, this)) {
                return Ok(Some(Value::Object(None)));
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSessionContext", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // creation / last-accessed time: the 8-field engine session stores a
    // millis timestamp in slot 5; the narrower shapes have no slot for one.
    //
    // E12: the narrow shapes used to answer `0`, and HotSpot never does —
    // measured (E12-1 §1), `getCreationTime()` is a real epoch value in EVERY
    // state including the null session, and `0` is exactly the value
    // application code reads as "there is no session". Answer a real epoch.
    //
    // RESIDUAL, stated rather than hidden: HotSpot's value is fixed at
    // construction, so two reads of one session agree; this one is `now` on
    // every call for the narrow shapes, so two reads disagree by the elapsed
    // millis. Making it stable needs a slot to stash it in (the NEW-13 shape
    // has none — its slot 3 is the attribute map, not a timestamp). A
    // drifting real timestamp is still strictly better than a stable
    // impossible one: age arithmetic (`now - creationTime`), which is what
    // every consumer actually does, goes from "56 years old" to "~0 ms old".
    //
    // E42 did NOT change this. The NEW-13 widening stops at 4 and this gate is
    // `> 5`, so the widened shape still answers `now` on every call. E22-1's
    // NOMINATION C asked for a widening that carried a creation-time slot and
    // this is not that widening — reaching slot 5 means width >= 6, which is
    // `tls.rs`'s cipher-first convention and would shift slots 0/1/2 under
    // every reader in this file.
    r.register(cls, "getCreationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if session_shape_width(&*ctx, this) > 5 {
            Ok(Some(sess_get(&*ctx, this, 5)))
        } else {
            Ok(Some(Value::Long(crate::epoch_millis_now())))
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getLastAccessedTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // A session reused by a later handshake was last accessed then, not
        // when it was created (`session_last_accessed_table`).
        let last = existing_objref_key(ctx, this)
            .and_then(|k| session_last_accessed_table().lock().get(&k).copied());
        if let Some(t) = last {
            return Ok(Some(Value::Long(t)));
        }
        if session_shape_width(&*ctx, this) > 5 {
            Ok(Some(sess_get(&*ctx, this, 5)))
        } else {
            Ok(Some(Value::Long(crate::epoch_millis_now())))
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getValue/putValue/removeValue/getValueNames — the JSSE session-attribute
    // API. `SSLSession` is an interface with no default body for any of these,
    // so an un-intercepted call throws AbstractMethodError. Jetty's
    // `SecureRequestCustomizer.retrieveSni()` calls `getValue()`/`putValue()`
    // on every SSL request to cache the resolved SNI host, so this previously
    // failed every HTTPS request that reached `SecureRequestCustomizer`
    // (`JettyServletWebServerFactoryTests`/`JettyReactiveWebServerFactoryTests`,
    // both 500s with this exact AbstractMethodError). Backed by a real
    // `java.util.HashMap` stored in the session's own LAST field (slot
    // `num_fields - 1`, added to both the 4-field and 8-field shapes above)
    // rather than a native side-table keyed by object address — normal GC
    // root-scanning of the (live, reachable) session object keeps arbitrary
    // attribute VALUES alive for free, avoiding the class of GC-unstable
    // objref-key bug this file's other side tables have hit.
    r.register(
        cls,
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            // MEASURED: IllegalArgumentException, SINGULAR "argument".
            if matches!(args.get(1), None | Some(Value::Object(None))) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "argument can not be null".into(),
                }
                .into());
            }
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            // E31: `sslsess_attrs_slot`, not `num_fields - 1` — see its doc.
            let slot = match sslsess_attrs_slot(ctx, this) {
                Some(s) => s,
                None => return Ok(Some(Value::Object(None))),
            };
            let map = match sess_get(&*ctx, this, slot) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke(
                "java/util/HashMap",
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(map)), name],
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cls,
        "putValue",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            if matches!(name, Value::Object(None)) || matches!(value, Value::Object(None)) {
                return Err(RuntimeError::IllegalArgumentException {
                    // MEASURED 2026-08-13 (scratchpad/orch/Three.java): JSSE
                    // VALIDATES here rather than dereferencing, so the KIND is
                    // IllegalArgumentException, not NPE -- and the text is the
                    // JDK's, not the invented "name and value must not be null".
                    // Note the PLURAL: putValue says "arguments", getValue and
                    // removeValue say "argument". One letter, two messages.
                    message: "arguments can not be null".into(),
                }
                .into());
            }
            // E31: a shape with no dedicated attribute slot must NOT fall back
            // to `num_fields - 1` — on the then-3-field NEW-13 shape that slot
            // was the stream id, and overwriting it with a HashMap flips
            // `session_has_negotiated` to `true`, which is exactly how a
            // `putValue` used to resurrect the 32-byte fabricated id and
            // `isValid() == true` on a session that never handshaked. No-op
            // instead; see `sslsess_attrs_slot`.
            //
            // E42: that shape is now 4 fields with a dedicated slot 3, so this
            // door ROUND-TRIPS for it rather than no-opping — which is the
            // whole point of the widening (Jetty's `SecureRequestCustomizer
            // .retrieveSni()` does `getValue()` then `putValue()` on every SSL
            // request). The `None` arm is now reached only by the 6-field
            // shape. Do not "simplify" it back to `num_fields - 1`: on that
            // shape the last slot is `tls.rs::SES_CREATION_TIME`.
            let slot = match sslsess_attrs_slot(ctx, this) {
                Some(s) => s,
                None => return Ok(None),
            };
            // gc-common w20-e: the session, the name and the value (the last
            // two read from `args`, a copy no collection updates) are held
            // across the map's lazy allocation, `HashMap.put` and the first
            // listener callback -- every one a GC point -- and were used raw
            // after each. Rooted here and re-read at every use.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let name_h = w19e_root_value(&mut scope, name);
            let value_h = w19e_root_value(&mut scope, value);
            let map = sslsess_attrs_map(&mut *scope, this, slot)?;
            let name_now = w19e_rooted_value(&scope, name, name_h.as_ref());
            let value_now = w19e_rooted_value(&scope, value, value_h.as_ref());
            let old = scope.invoke(
                "java/util/HashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(map)), name_now, value_now],
            )?;
            // JSSE unbinds the value being replaced before binding the new one.
            if let Some(old @ Value::Object(Some(_))) = old {
                let session = scope.get(&this_h);
                let name_now = w19e_rooted_value(&scope, name, name_h.as_ref());
                fire_session_binding(&mut *scope, session, name_now, old, false);
            }
            let session = scope.get(&this_h);
            let name_now = w19e_rooted_value(&scope, name, name_h.as_ref());
            let value_now = w19e_rooted_value(&scope, value, value_h.as_ref());
            fire_session_binding(&mut *scope, session, name_now, value_now, true);
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(cls, "removeValue", "(Ljava/lang/String;)V", |ctx, args| {
        // MEASURED: IllegalArgumentException, SINGULAR "argument".
        if matches!(args.get(1), None | Some(Value::Object(None))) {
            return Err(RuntimeError::IllegalArgumentException {
                message: "argument can not be null".into(),
            }
            .into());
        }
        let this = obj_arg(args, 0)?;
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        // E31: `sslsess_attrs_slot`, not `num_fields - 1` — see its doc.
        let slot = match sslsess_attrs_slot(ctx, this) {
            Some(s) => s,
            None => return Ok(None),
        };
        let map = match sess_get(&*ctx, this, slot) {
            Value::Object(Some(m)) => m,
            _ => return Ok(None),
        };
        // gc-common w20-e: `HashMap.remove` runs Java; the listener callback
        // gets the session and the name at their current addresses.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let name_h = w19e_root_value(&mut scope, name);
        let old = scope.invoke(
            "java/util/HashMap",
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(map)), name],
        )?;
        if let Some(old @ Value::Object(Some(_))) = old {
            let session = scope.get(&this_h);
            let name_now = w19e_rooted_value(&scope, name, name_h.as_ref());
            fire_session_binding(&mut *scope, session, name_now, old, false);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cls,
        "getValueNames",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let empty = |ctx: &mut dyn NativeContext| {
                let no_names: [&str; 0] = [];
                let arr = materialize_java_string_array(ctx, &no_names);
                Ok(Some(Value::Object(Some(arr))))
            };
            // E31: `sslsess_attrs_slot`, not `num_fields - 1` — see its doc. On the
            // then-3-field shape the old spelling read the STREAM ID as a map
            // reference; it missed (an `Int`) and answered empty, so this arm was
            // unchanged in effect and changed in correctness. E42: that shape is
            // width 4 now and slot 3 really is the map, so this door reports the
            // names a `putValue` actually stored.
            let slot = match sslsess_attrs_slot(ctx, this) {
                Some(s) => s,
                None => return empty(ctx),
            };
            let map = match sess_get(&*ctx, this, slot) {
                Value::Object(Some(m)) => m,
                _ => return empty(ctx),
            };
            let key_set = match ctx.invoke(
                "java/util/HashMap",
                "keySet",
                "()Ljava/util/Set;",
                &[Value::Object(Some(map))],
            )? {
                Some(Value::Object(Some(s))) => s,
                _ => return empty(ctx),
            };
            // `keySet()`'s runtime type is `HashMap$KeySet`, not `HashSet` — resolve
            // `toArray`'s declaring class dynamically rather than guessing a name,
            // since a wrong static class name here would use the wrong field/vtable
            // layout for the dispatch.
            let key_set_class = ctx.class_id_of_object(key_set);
            let key_set_class_name = match ctx.class_name_of_id(key_set_class) {
                Some(n) => n,
                None => return empty(ctx),
            };
            let raw_arr: ObjectRef = match ctx.invoke(
                &key_set_class_name,
                "toArray",
                "()[Ljava/lang/Object;",
                &[Value::Object(Some(key_set))],
            )? {
                Some(Value::Object(Some(arr))) => arr,
                _ => return empty(ctx),
            };
            // `Collection.toArray()` reifies as `Object[]`, not `String[]` — real
            // JDK's own `SSLSessionImpl.getValueNames()` has the same mismatch and
            // copies into a freshly-typed array rather than returning it directly.
            // Mirror that: allocate our own `String[]` (i6-L2: typed, where it
            // used to be an untyped `ClassId(0)` array) and copy each key across.
            //
            // gc-common w20-e: the keys were copied out of `raw_arr`'s address
            // from before the copy's allocation (and the component lookup,
            // which may load `String`'s class); it is rooted and re-read now.
            let len = ctx.array_length(raw_arr);
            let raw_pin = ctx.pin_native_root(raw_arr);
            let string_component =
                crate::lang_class::reflection_component_id(ctx, "java/lang/String");
            let out = ctx.new_ref_array(string_component, len);
            let keys = ctx.read_native_pin(raw_pin, raw_arr);
            ctx.unpin_native_roots(raw_pin);
            for i in 0..len {
                ctx.set_array_element(out, i, ctx.get_array_element(keys, i));
            }
            Ok(Some(Value::Object(Some(out))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

/// Lazily allocate (and cache in the session's own last field) the
/// `java.util.HashMap` backing `SSLSession.putValue`/`getValue`/etc — see the
/// doc comment on its registration in `register_ssl_session_real`.
///
/// E31: `slot` is now a PARAMETER, resolved by `sslsess_attrs_slot` at the one
/// caller that may write. It used to compute `num_fields - 1` itself, which is
/// the attribute slot on only two of five shapes — see `sslsess_attrs_slot`
/// for what the other three lost. Taking it as an argument is what makes the
/// "does this shape even have an attribute slot?" question unskippable rather
/// than something a future caller can forget to ask.
pub fn sslsess_attrs_map(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if let Value::Object(Some(map)) = sess_get(&*ctx, this, slot) {
        return Ok(map);
    }
    let this_pin = ctx.pin_native_root(this);
    let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
        Some(Value::Object(Some(m))) => m,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::OutOfMemoryError {
                message: "SSLSession: could not allocate attribute map".into(),
            }
            .into());
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    sess_set(&*ctx, this, slot, Value::Object(Some(map)));
    Ok(map)
}

/// WP5.4 — register ALPN-related natives on SSLParameters. ALPN propagation
/// from `SSLParameters` → `SSLEngineImpl` is wired in `register_apply_parameters`
/// (see `register_sslengine_real`).
pub fn register_alpn_real(r: &mut NativeMethodRegistry) {
    register_alpn_on_parameters(r);
}

#[allow(dead_code)]
pub fn _wp51_keep_symbols_live() {
    let _ = engine_negotiated_alpn_internal;
}

