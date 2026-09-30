// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T3 — Standard Library Completeness implementations
//!
//! This module provides native method registrations for:
//! - T3.8  JNDI (javax.naming) — InitialContext, DNS provider, RMI registry
//! - T3.9  StAX XMLEventReader + SchemaFactory
//! - T3.10 ScriptEngineManager (javax.script)
//! - T3.11 Internationalization extras (Locale, Charset)
//! - T3.12-T3.15 Tooling natives (javac, JShell, jpackage, javadoc)
//! - T3.1.4 ConcurrentSkipListMap subMap/headMap/tailMap (in lib.rs)
//! - T3.1.15 Flow reactive streams (in lib.rs)
//! - T3.1.16-T3.1.18 Virtual threads extras

use crate::{obj_arg, try_alloc_concurrent_synthetic};
use cratonvm_native_api::NativeContext;
use cratonvm_native_api::NativeHandleScope;
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::error::RuntimeError;
use cratonvm_types::{ObjectRef, Value};

// =============================================================================
// T3.8 — javax.naming / JNDI
// =============================================================================

/// Register JNDI natives: InitialContext, Context, Name, NamingEnumeration.
/// This is a minimal but real implementation backed by a Rust HashMap for
/// the in-memory binding store, with DNS lookups via std::net for the DNS provider.
pub(crate) fn register_t38_jndi(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ic = "javax/naming/InitialContext";

    // <init>() — creates context with empty environment
    // Fields: 0=bindings_map (HashMap-like object), 1=environment
    r.register(ic, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // bindings = HashMap synthetic (keys=0, values=1, size=2); env = null
        jndi_init_context(ctx, this, Value::Object(None))
    });

    // <init>(Hashtable) — creates context with given environment
    r.register(ic, "<init>", "(Ljava/util/Hashtable;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let env = args.get(1).copied().unwrap_or(Value::Object(None));
        jndi_init_context(ctx, this, env)
    });

    // bind(String, Object) — store binding
    r.register(
        ic,
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            jndi_put_binding(ctx, this, name, value)?;
            Ok(None)
        },
    );

    // rebind(String, Object) — replace binding
    r.register(
        ic,
        "rebind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            jndi_put_binding(ctx, this, name, value)?;
            Ok(None)
        },
    );

    // lookup(String) -> Object — retrieve binding
    r.register(
        ic,
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));

            // Check for DNS URL scheme: "dns:///hostname" or "dns://server/name"
            if let Some(Value::Object(Some(name_obj))) = args.get(1) {
                if let Some(name_str) = ctx.read_string(*name_obj) {
                    if name_str.starts_with("dns:") {
                        return jndi_dns_lookup(ctx, &name_str);
                    }
                }
            }

            let result = jndi_get_binding(ctx, this, name);
            match result {
                Value::Object(None) => Err(RuntimeError::IllegalStateException {
                    message: format!("javax.naming.NameNotFoundException: Name not found"),
                }
                .into()),
                other => Ok(Some(other)),
            }
        },
    );

    // unbind(String)
    r.register(ic, "unbind", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        jndi_remove_binding(ctx, this, name)?;
        Ok(None)
    });

    // close() — STUB-REMOVAL (wave 2): was an unconditional no-op. This
    // `InitialContext` keeps its whole binding store in its OWN fields (slot 0
    // = bindings map, slot 1 = environment), so `close()` genuinely has
    // something to release; doing nothing leaked the map (and every bound
    // object graph hanging off it) for the lifetime of the context object.
    // `Context.close()` is specified as "releases this context's resources
    // immediately" and "invoking any other method on a closed context is not
    // allowed", so dropping the store is faithful: a subsequent `lookup`
    // reports name-not-found rather than silently serving stale bindings.
    // Idempotent — closing twice just clears already-null fields.
    r.register(ic, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Object(None));
        Ok(None)
    });

    // getEnvironment() -> Hashtable
    r.register(
        ic,
        "getEnvironment",
        "()Ljava/util/Hashtable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );

    // rename(String, String)
    r.register(
        ic,
        "rename",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let old_name = args.get(1).copied().unwrap_or(Value::Object(None));
            let new_name = args.get(2).copied().unwrap_or(Value::Object(None));
            let val = jndi_get_binding(ctx, this, old_name);
            if matches!(val, Value::Object(None)) {
                return Err(RuntimeError::IllegalStateException {
                    message: "javax.naming.NameNotFoundException".to_string(),
                }
                .into());
            }
            jndi_remove_binding(ctx, this, old_name)?;
            jndi_put_binding(ctx, this, new_name, val)?;
            Ok(None)
        },
    );

    // --- RMI registry (java.rmi.registry.LocateRegistry / Registry) ---
    // Minimal: creates an in-memory registry object
    let reg = "java/rmi/registry/LocateRegistry";
    r.register(
        reg,
        "createRegistry",
        "(I)Ljava/rmi/registry/Registry;",
        |ctx, args| {
            let _port = match args.get(0) {
                Some(Value::Int(v)) => *v,
                _ => 1099,
            };
            // Create a synthetic Registry backed by a HashMap
            jndi_new_registry(ctx, _port)
        },
    );

    r.register(
        reg,
        "getRegistry",
        "(Ljava/lang/String;I)Ljava/rmi/registry/Registry;",
        |ctx, args| {
            // Return a synthetic registry pointing to the given host:port
            let port = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 1099,
            };
            jndi_new_registry(ctx, port)
        },
    );

    let ri = "java/rmi/registry/Registry";
    r.register(
        ri,
        "bind",
        "(Ljava/lang/String;Ljava/rmi/Remote;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let obj = args.get(2).copied().unwrap_or(Value::Object(None));
            jndi_put_binding(ctx, this, name, obj)?;
            Ok(None)
        },
    );

    r.register(
        ri,
        "lookup",
        "(Ljava/lang/String;)Ljava/rmi/Remote;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = jndi_get_binding(ctx, this, name);
            Ok(Some(val))
        },
    );

    r.register(
        ri,
        "rebind",
        "(Ljava/lang/String;Ljava/rmi/Remote;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let obj = args.get(2).copied().unwrap_or(Value::Object(None));
            jndi_put_binding(ctx, this, name, obj)?;
            Ok(None)
        },
    );

    r.register(ri, "unbind", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        jndi_remove_binding(ctx, this, name)?;
        Ok(None)
    });

    r.register(ri, "list", "()[Ljava/lang/String;", |ctx, args| {
        // `String[]`, the declared type (i8-L2). `String` is always loaded,
        // so the lookup does not load (a raw reference is live below).
        let string_id = crate::lang_class::reflection_component_id(ctx, "java/lang/String");
        let this = obj_arg(args, 0)?;
        // The store's key array and count; a registry without a store (or
        // without a key array) lists nothing. One fallback allocation, after
        // every read, so no reference is held across it.
        let keys = match ctx.get_field(this, 0) {
            Value::Object(Some(b)) => match (ctx.get_field(b, 0), ctx.get_field(b, 2)) {
                (Value::Object(Some(a)), Value::Int(n)) => Some((a, n as usize)),
                (Value::Object(Some(a)), _) => Some((a, 0)),
                _ => None,
            },
            _ => None,
        };
        let Some((keys_arr, size)) = keys else {
            let empty = ctx.new_ref_array(string_id, 0);
            return Ok(Some(Value::Object(Some(empty))));
        };
        // `new_array` can collect, and `keys_arr` was read out of the heap
        // before it: copy through the scope so the source address is the
        // post-allocation one.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let keys_h = scope.root(keys_arr);
        let result = scope.new_ref_array(string_id, size);
        let keys_arr = scope.get(&keys_h);
        for i in 0..size {
            let v = scope.get_array_element(keys_arr, i);
            scope.set_array_element(result, i, v);
        }
        Ok(Some(Value::Object(Some(result))))
    });
    r.set_category(__prev_cat);
}

/// Allocate a synthetic `class_name` object in the binding-store shape the
/// JNDI and `javax.script` natives share: a `cap`-slot key array in field 0, a
/// `cap`-slot value array in field 1, the count (0) in field 2.
///
/// gc-common w19-c (stale-handle triage): each of the seven builders this
/// replaces held the fresh store raw across both array allocations, and its
/// caller's own objects raw across the store's
/// `try_alloc_concurrent_synthetic`, whose class resolution can run a
/// `<clinit>` (Java). The store is rooted here and each array is stored before
/// the next allocation. The returned address is current; a caller that
/// allocates again roots it first.
fn t3_new_binding_store(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    cap: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let store = try_alloc_concurrent_synthetic(ctx, class_name, 3)?;
    let mut scope = NativeHandleScope::new(ctx);
    let store_h = scope.root(store);
    for slot in 0..2 {
        let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, cap);
        let store = scope.get(&store_h);
        scope.set_field(store, slot, Value::Object(Some(arr)));
    }
    let store = scope.get(&store_h);
    scope.set_field(store, 2, Value::Int(0));
    Ok(store)
}

/// Allocate a synthetic `class_name` object (`num_fields` slots) and store a
/// fresh `String` holding `text` in field `slot`. The object is rooted across
/// the string's allocation (gc-common w19-c); the returned address is current.
fn t3_new_with_string(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
    slot: usize,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, num_fields)?;
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(obj);
    let s = scope.create_string(text);
    let obj = scope.get(&obj_h);
    scope.set_field(obj, slot, Value::Object(Some(s)));
    Ok(obj)
}

/// Allocate a synthetic `java/util/ArrayList` (backing array in field 0, size
/// in field 1) with an empty `len`-slot backing array. The list is rooted
/// across the array's allocation (gc-common w19-c); the returned address is
/// current.
fn t3_new_array_list(
    ctx: &mut dyn NativeContext,
    len: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let mut scope = NativeHandleScope::new(ctx);
    let list_h = scope.root(list);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, len);
    let list = scope.get(&list_h);
    scope.set_field(list, 0, Value::Object(Some(arr)));
    scope.set_field(list, 1, Value::Int(0));
    Ok(list)
}

/// The two `InitialContext` constructors: a fresh binding store in field 0 and
/// `env` in field 1.
///
/// gc-common w19-c: the receiver and the environment used to be held raw
/// across the store's allocation (a `HashMap` class resolution and two
/// arrays). Both are rooted now and re-read for the stores.
fn jndi_init_context(ctx: &mut dyn NativeContext, this: ObjectRef, env: Value) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let env_h = match env {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let bindings = t3_new_binding_store(&mut *scope, "java/util/HashMap", 16)?;
    let env_now = match &env_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => env,
    };
    let this = scope.get(&this_h);
    scope.set_field(this, 0, Value::Object(Some(bindings)));
    scope.set_field(this, 1, env_now);
    Ok(None)
}

/// `LocateRegistry.createRegistry` / `getRegistry`: a synthetic `Registry`
/// with a fresh binding store in field 0 and `port` in field 1. The registry
/// is rooted across the store's allocation (gc-common w19-c).
fn jndi_new_registry(ctx: &mut dyn NativeContext, port: i32) -> MethodCallResult {
    let registry = try_alloc_concurrent_synthetic(ctx, "java/rmi/registry/Registry", 2)?;
    let mut scope = NativeHandleScope::new(ctx);
    let registry_h = scope.root(registry);
    let bindings = t3_new_binding_store(&mut *scope, "java/util/HashMap", 16)?;
    let registry = scope.get(&registry_h);
    scope.set_field(registry, 0, Value::Object(Some(bindings)));
    scope.set_field(registry, 1, Value::Int(port));
    Ok(Some(Value::Object(Some(registry))))
}

/// Put a key-value pair into the JNDI binding store.
fn jndi_put_binding(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: Value,
    value: Value,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let bindings = match ctx.get_field(this, 0) {
        Value::Object(Some(b)) => b,
        _ => return Ok(()),
    };
    let size = match ctx.get_field(bindings, 2) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let keys_arr = match ctx.get_field(bindings, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(()),
    };
    let vals_arr = match ctx.get_field(bindings, 1) {
        Value::Object(Some(a)) => a,
        _ => return Ok(()),
    };

    // PERF: decode the lookup name ONCE before scanning instead of re-decoding
    // it (ctx.read_string on the search key) inside every loop iteration. The
    // original loop string-decoded both the existing key and `name` on each
    // step → 2*n String allocations per put; this does n+1 and never re-decodes
    // the unchanged search key. Behaviour is identical: the prior inner match
    // only matched when read_string(name) was Some, so a non-string `name`
    // (search_key == None) still falls straight through to the append path
    // exactly as before (rebind of an existing key overwrites in place).
    let search_key: Option<String> = match name {
        Value::Object(Some(b)) => ctx.read_string(b),
        _ => None,
    };
    if let Some(ref sb) = search_key {
        // First matching slot wins, preserving the original forward-scan
        // "existing key overwrites" semantics.
        for i in 0..size {
            if let Value::Object(Some(a)) = ctx.get_array_element(keys_arr, i) {
                if let Some(sa) = ctx.read_string(a) {
                    if &sa == sb {
                        ctx.set_array_element(vals_arr, i, value);
                        return Ok(());
                    }
                }
            }
        }
    }

    // Grow if needed
    let cap = ctx.array_length(keys_arr);
    if size >= cap {
        let bindings = jndi_grow_and_append(ctx, bindings, keys_arr, vals_arr, size, name, value);
        ctx.set_field(bindings, 2, Value::Int((size + 1) as i32));
        return Ok(());
    }
    ctx.set_array_element(keys_arr, size, name);
    ctx.set_array_element(vals_arr, size, value);
    ctx.set_field(bindings, 2, Value::Int((size + 1) as i32));
    Ok(())
}

/// The grow path of [`jndi_put_binding`]: copy both arrays into ones twice the
/// size, publish them, append `name` / `value` at `size`, and hand back the
/// binding store's post-allocation address (the caller stores the count
/// through it).
///
/// A function of its own since gc-common w16-e, so the audit's flat scan no
/// longer pairs this path's allocations with the plain-append path's uses;
/// the body is unchanged except for the capacity, which now also covers the
/// appended slot (`cap * 2` was 0 for an empty array, and the append then
/// stored past its end).
fn jndi_grow_and_append(
    ctx: &mut dyn NativeContext,
    bindings: ObjectRef,
    keys_arr: ObjectRef,
    vals_arr: ObjectRef,
    size: usize,
    name: Value,
    value: Value,
) -> ObjectRef {
    let cap = ctx.array_length(keys_arr);
    {
        let new_cap = (cap * 2).max(size + 1);
        // GC: the grow path ALLOCATES, and everything it then touches is a Rust
        // local holding a pre-allocation address — the old array it copies from,
        // the element it stores, and the receiver it publishes into. Under a
        // moving collector those go stale; under the Generational non-moving young
        // sweep an object nothing else roots is ZEROED in place. Root them for the
        // duration of the grow and re-read each one at its use. See
        // `internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`.
        // TWO allocations here, so even `new_keys` is stale by the time
        // `new_vals` returns.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let bindings_h = scope.root(bindings);
        let keys_h = scope.root(keys_arr);
        let vals_h = scope.root(vals_arr);
        let name_h = match name {
            Value::Object(Some(o)) => Some(scope.root(o)),
            _ => None,
        };
        let value_h = match value {
            Value::Object(Some(o)) => Some(scope.root(o)),
            _ => None,
        };
        let nk = scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        let nk_h = scope.root(nk);
        let nv = scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        let nv_h = scope.root(nv);
        for i in 0..size {
            let ks = scope.get(&keys_h);
            let kv = scope.get_array_element(ks, i);
            let kd = scope.get(&nk_h);
            scope.set_array_element(kd, i, kv);
            let vs = scope.get(&vals_h);
            let vv = scope.get_array_element(vs, i);
            let vd = scope.get(&nv_h);
            scope.set_array_element(vd, i, vv);
        }
        let (b, kd, vd) = (scope.get(&bindings_h), scope.get(&nk_h), scope.get(&nv_h));
        scope.set_field(b, 0, Value::Object(Some(kd)));
        let b = scope.get(&bindings_h);
        scope.set_field(b, 1, Value::Object(Some(vd)));
        let name_now = match &name_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => name,
        };
        let value_now = match &value_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => value,
        };
        let kd = scope.get(&nk_h);
        scope.set_array_element(kd, size, name_now);
        let vd = scope.get(&nv_h);
        scope.set_array_element(vd, size, value_now);
        // The caller's count store runs after the scope closes.
        scope.get(&bindings_h)
    }
}

/// Get a value from the JNDI binding store by name.
fn jndi_get_binding(ctx: &mut dyn NativeContext, this: ObjectRef, name: Value) -> Value {
    let bindings = match ctx.get_field(this, 0) {
        Value::Object(Some(b)) => b,
        _ => return Value::Object(None),
    };
    let size = match ctx.get_field(bindings, 2) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let keys_arr = match ctx.get_field(bindings, 0) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    let vals_arr = match ctx.get_field(bindings, 1) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    // PERF: decode the lookup name once up front rather than re-decoding it on
    // every iteration (the old loop called read_string on BOTH the existing key
    // and `name` each step → 2*n String allocations per lookup). A non-string
    // `name` could never match in the old code (it required read_string(name)
    // to be Some), so returning the not-found sentinel for that case preserves
    // behaviour exactly.
    let search_key = match name {
        Value::Object(Some(b)) => match ctx.read_string(b) {
            Some(s) => s,
            None => return Value::Object(None),
        },
        _ => return Value::Object(None),
    };
    // Forward scan, comparing each existing key (decoded once) against the
    // pre-decoded search key — first match wins, identical to the original.
    for i in 0..size {
        if let Value::Object(Some(a)) = ctx.get_array_element(keys_arr, i) {
            if let Some(sa) = ctx.read_string(a) {
                if sa == search_key {
                    return ctx.get_array_element(vals_arr, i);
                }
            }
        }
    }
    Value::Object(None)
}

/// Remove a binding from the JNDI store.
fn jndi_remove_binding(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: Value,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let bindings = match ctx.get_field(this, 0) {
        Value::Object(Some(b)) => b,
        _ => return Ok(()),
    };
    let size = match ctx.get_field(bindings, 2) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let keys_arr = match ctx.get_field(bindings, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(()),
    };
    let vals_arr = match ctx.get_field(bindings, 1) {
        Value::Object(Some(a)) => a,
        _ => return Ok(()),
    };
    // PERF: decode the name to remove once instead of re-decoding it inside the
    // scan (old loop read_string'd both sides each step). A non-string `name`
    // never matched in the original (required read_string(name) = Some), so the
    // early return below is behaviour-preserving (no removal, Ok(())).
    let search_key = match name {
        Value::Object(Some(b)) => match ctx.read_string(b) {
            Some(s) => s,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    for i in 0..size {
        let existing = ctx.get_array_element(keys_arr, i);
        if let Value::Object(Some(a)) = existing {
            if let Some(sa) = ctx.read_string(a) {
                if sa == search_key {
                    // Shift left
                    for j in i..size - 1 {
                        ctx.set_array_element(keys_arr, j, ctx.get_array_element(keys_arr, j + 1));
                        ctx.set_array_element(vals_arr, j, ctx.get_array_element(vals_arr, j + 1));
                    }
                    ctx.set_array_element(keys_arr, size - 1, Value::Object(None));
                    ctx.set_array_element(vals_arr, size - 1, Value::Object(None));
                    ctx.set_field(bindings, 2, Value::Int((size - 1) as i32));
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// DNS lookup via JNDI: resolves "dns:///hostname" or "dns://server/name"
fn jndi_dns_lookup(
    ctx: &mut dyn NativeContext,
    url: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    // Parse: dns:///hostname or dns://server/hostname
    let path = url.strip_prefix("dns:").unwrap_or(url);
    let hostname = path.trim_start_matches('/');

    // Use std::net for resolution
    use std::net::ToSocketAddrs;
    let lookup = format!("{}:0", hostname);
    match lookup.to_socket_addrs() {
        Ok(addrs) => {
            let addresses: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            if addresses.is_empty() {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "javax.naming.NameNotFoundException: DNS lookup failed for {}",
                        hostname
                    ),
                }
                .into());
            }
            // Return the first address as a string
            let s = ctx.create_string(&addresses[0]);
            Ok(Some(Value::Object(Some(s))))
        }
        Err(e) => Err(RuntimeError::IllegalStateException {
            message: format!("javax.naming.NamingException: DNS resolution failed: {}", e),
        }
        .into()),
    }
}

// =============================================================================
// T3.9 — StAX XMLEventReader + SchemaFactory
// =============================================================================

/// Register StAX (Streaming API for XML) and Schema validation natives.
pub(crate) fn register_t39_stax(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- XMLInputFactory ---
    let xif = "javax/xml/stream/XMLInputFactory";
    r.register(
        xif,
        "newInstance",
        "()Ljavax/xml/stream/XMLInputFactory;",
        |ctx, _args| stax_new_factory(ctx, "javax/xml/stream/XMLInputFactory"),
    );
    r.register(
        xif,
        "newFactory",
        "()Ljavax/xml/stream/XMLInputFactory;",
        |ctx, _args| stax_new_factory(ctx, "javax/xml/stream/XMLInputFactory"),
    );

    // createXMLEventReader(InputStream) -> XMLEventReader
    r.register(
        xif,
        "createXMLEventReader",
        "(Ljava/io/InputStream;)Ljavax/xml/stream/XMLEventReader;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let is = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Read all bytes from input stream into a string
            let xml_str = stax_read_input_stream(ctx, is);
            stax_create_event_reader(ctx, &xml_str)
        },
    );

    // createXMLEventReader(Reader) -> XMLEventReader
    r.register(
        xif,
        "createXMLEventReader",
        "(Ljava/io/Reader;)Ljavax/xml/stream/XMLEventReader;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let reader = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Try to read content from the reader
            let content = match ctx.invoke_virtual(reader, "toString", "()Ljava/lang/String;", &[])
            {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            stax_create_event_reader(ctx, &content)
        },
    );

    // createXMLStreamReader(InputStream) -> XMLStreamReader
    r.register(
        xif,
        "createXMLStreamReader",
        "(Ljava/io/InputStream;)Ljavax/xml/stream/XMLStreamReader;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let is = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let xml_str = stax_read_input_stream(ctx, is);
            stax_create_stream_reader(ctx, &xml_str)
        },
    );

    // --- XMLEventReader ---
    let xer = "javax/xml/stream/XMLEventReader";
    // Fields: 0=events_array, 1=event_count, 2=current_index
    r.register(xer, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let idx = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if idx < count { 1 } else { 0 })))
    });
    r.register(
        xer,
        "nextEvent",
        "()Ljavax/xml/stream/events/XMLEvent;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = ctx.get_field(this, 1).as_int().unwrap_or(0);
            let idx = ctx.get_field(this, 2).as_int().unwrap_or(0);
            if idx >= count {
                return Err(RuntimeError::IllegalStateException {
                    message: "javax.xml.stream.XMLStreamException: No more events".to_string(),
                }
                .into());
            }
            let events = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let event = ctx.get_array_element(events, idx as usize);
            ctx.set_field(this, 2, Value::Int(idx + 1));
            Ok(Some(event))
        },
    );
    // STUB-REMOVAL (wave 2): was an unconditional no-op. `XMLEventReader.close()`
    // is specified as "frees any resources associated with this Reader" — for
    // this array-backed reader that is the materialised event array in slot 0,
    // which a no-op pinned for the object's whole lifetime (an entire parsed
    // document per reader). Release it and zero the count so a post-close
    // `hasNext()` answers false and `nextEvent()` raises the same
    // "No more events" XMLStreamException it already raises at end of input,
    // instead of continuing to serve events from a closed reader.
    r.register(xer, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(0));
        Ok(None)
    });

    // --- XMLStreamReader ---
    let xsr = "javax/xml/stream/XMLStreamReader";
    // Fields: 0=events_array, 1=event_count, 2=current_index, 3=current_event_type
    r.register(xsr, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let idx = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if idx < count { 1 } else { 0 })))
    });
    r.register(xsr, "next", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let idx = ctx.get_field(this, 2).as_int().unwrap_or(0);
        if idx >= count {
            return Err(RuntimeError::IllegalStateException {
                message: "No more events".to_string(),
            }
            .into());
        }
        let events = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let event = match ctx.get_array_element(events, idx as usize) {
            Value::Object(Some(e)) => e,
            _ => return Ok(Some(Value::Int(0))),
        };
        let event_type = ctx.get_field(event, 0).as_int().unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(idx + 1));
        ctx.set_field(this, 3, Value::Int(event_type));
        Ok(Some(Value::Int(event_type)))
    });
    r.register(xsr, "getEventType", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(xsr, "getLocalName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 2).as_int().unwrap_or(1) - 1;
        let events = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        if idx < 0 {
            return Ok(Some(Value::Object(None)));
        }
        let event = match ctx.get_array_element(events, idx as usize) {
            Value::Object(Some(e)) => e,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(event, 1))) // name field
    });
    r.register(xsr, "getText", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 2).as_int().unwrap_or(1) - 1;
        let events = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        if idx < 0 {
            return Ok(Some(Value::Object(None)));
        }
        let event = match ctx.get_array_element(events, idx as usize) {
            Value::Object(Some(e)) => e,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(event, 2))) // text field
    });
    // STUB-REMOVAL (wave 2): same reasoning as the `XMLEventReader.close()`
    // sibling above — release the materialised event array and zero the
    // counters so the reader reports end-of-input after close instead of
    // continuing to serve events (and holding the whole parsed document
    // alive).
    r.register(xsr, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(0));
        ctx.set_field(this, 3, Value::Int(0));
        Ok(None)
    });

    // --- XMLEvent types ---
    // Event object: 0=type(int), 1=name(String), 2=text(String)
    let xe = "javax/xml/stream/events/XMLEvent";
    r.register(xe, "getEventType", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(xe, "isStartElement", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let t = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if t == 1 { 1 } else { 0 }))) // START_ELEMENT = 1
    });
    r.register(xe, "isEndElement", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let t = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if t == 2 { 1 } else { 0 }))) // END_ELEMENT = 2
    });
    r.register(xe, "isCharacters", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let t = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if t == 4 { 1 } else { 0 }))) // CHARACTERS = 4
    });
    r.register(xe, "isStartDocument", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let t = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if t == 7 { 1 } else { 0 }))) // START_DOCUMENT = 7
    });
    r.register(xe, "isEndDocument", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let t = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if t == 8 { 1 } else { 0 }))) // END_DOCUMENT = 8
    });

    // --- SchemaFactory ---
    let sf = "javax/xml/validation/SchemaFactory";
    r.register(
        sf,
        "newInstance",
        "(Ljava/lang/String;)Ljavax/xml/validation/SchemaFactory;",
        |ctx, _args| stax_new_factory(ctx, "javax/xml/validation/SchemaFactory"),
    );
    // newSchema(Source) — returns a Schema object
    r.register(
        sf,
        "newSchema",
        "(Ljavax/xml/transform/Source;)Ljavax/xml/validation/Schema;",
        |ctx, _args| {
            let schema = try_alloc_concurrent_synthetic(ctx, "javax/xml/validation/Schema", 1)?;
            ctx.set_field(schema, 0, Value::Int(1)); // valid
            Ok(Some(Value::Object(Some(schema))))
        },
    );
    // newSchema() — no-arg: returns a permissive schema
    r.register(
        sf,
        "newSchema",
        "()Ljavax/xml/validation/Schema;",
        |ctx, _args| {
            let schema = try_alloc_concurrent_synthetic(ctx, "javax/xml/validation/Schema", 1)?;
            ctx.set_field(schema, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(schema))))
        },
    );

    // Schema.newValidator()
    let schema = "javax/xml/validation/Schema";
    r.register(
        schema,
        "newValidator",
        "()Ljavax/xml/validation/Validator;",
        |ctx, _args| {
            let v = try_alloc_concurrent_synthetic(ctx, "javax/xml/validation/Validator", 1)?;
            ctx.set_field(v, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(v))))
        },
    );

    // Validator.validate(Source) — performs XML well-formedness checking
    let validator = "javax/xml/validation/Validator";
    r.register(
        validator,
        "validate",
        "(Ljavax/xml/transform/Source;)V",
        |ctx, args| {
            // Extract XML content from the Source if possible and check well-formedness
            if let Some(Value::Object(Some(source))) = args.get(1) {
                // Try to get the system ID (file path/URL) or content from the source
                if let Ok(Some(Value::Object(Some(stream)))) =
                    ctx.invoke_virtual(*source, "getInputStream", "()Ljava/io/InputStream;", &[])
                {
                    let xml = stax_read_input_stream(ctx, stream);
                    if !xml.is_empty() {
                        validate_xml_well_formedness(&xml).map_err(|msg| {
                            RuntimeError::IllegalStateException {
                                message: format!("org.xml.sax.SAXParseException: {}", msg),
                            }
                        })?;
                    }
                }
            }
            Ok(None)
        },
    );

    // XMLConstants field constants
    let xc = "javax/xml/XMLConstants";
    r.register(
        xc,
        "W3C_XML_SCHEMA_NS_URI",
        "Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("http://www.w3.org/2001/XMLSchema");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.set_category(__prev_cat);
}

/// `XMLInputFactory.newInstance` / `newFactory` and `SchemaFactory.newInstance`:
/// a synthetic factory whose one slot (configuration flags) is 0. One body for
/// the three natives that each spelled it out (gc-common w19-c).
fn stax_new_factory(ctx: &mut dyn NativeContext, class_name: &str) -> MethodCallResult {
    let factory = try_alloc_concurrent_synthetic(ctx, class_name, 1)?;
    ctx.set_field(factory, 0, Value::Int(0)); // configuration flags
    Ok(Some(Value::Object(Some(factory))))
}

/// StAX event type constants (matching javax.xml.stream.XMLStreamConstants)
const STAX_START_ELEMENT: i32 = 1;
const STAX_END_ELEMENT: i32 = 2;
const STAX_CHARACTERS: i32 = 4;
const STAX_START_DOCUMENT: i32 = 7;
const STAX_END_DOCUMENT: i32 = 8;

/// Read all content from a JVM InputStream into a Rust String.
fn stax_read_input_stream(ctx: &mut dyn NativeContext, is: ObjectRef) -> String {
    // Try to read via available() + read(byte[])
    let available = match ctx.invoke_virtual(is, "available", "()I", &[]) {
        Ok(Some(Value::Int(n))) if n > 0 => n as usize,
        _ => 4096,
    };
    // `is` is held across the allocation and every `read`; `buf` across every
    // `read`. Both are pinned and re-derived per iteration.
    let is_pin = ctx.pin_native_root(is);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, available.max(4096));
    let buf_pin = ctx.pin_native_root(buf);
    let mut is = ctx.read_native_pin(is_pin, is);
    let mut buf = buf;
    let mut result = Vec::new();
    loop {
        is = ctx.read_native_pin(is_pin, is);
        buf = ctx.read_native_pin(buf_pin, buf);
        let n = match ctx.invoke_virtual(is, "read", "([B)I", &[Value::Object(Some(buf))]) {
            Ok(Some(Value::Int(n))) => n,
            _ => -1,
        };
        if n <= 0 {
            break;
        }
        // gc-common w16-e: `read` is Java, a GC point. The bytes it wrote are
        // in the buffer's CURRENT copy; they used to be read back through the
        // address taken before the call. One bulk copy instead of a boxed
        // `Value` per byte.
        buf = ctx.read_native_pin(buf_pin, buf);
        let start = result.len();
        result.resize(start + n as usize, 0);
        let copied = ctx.read_byte_array_into(buf, 0, &mut result[start..]);
        result.truncate(start + copied);
    }
    ctx.unpin_native_roots(is_pin);
    String::from_utf8(result).unwrap_or_default()
}

/// Parse XML into StAX events and create an XMLEventReader.
fn stax_create_event_reader(
    ctx: &mut dyn NativeContext,
    xml: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let events = stax_parse_events(xml);
    let event_count = events.len();
    // Everything here is a young object built next to another allocation: the
    // event array survives one element (and up to two strings) per event, and
    // each event survives the strings stored into it. Hold the array in the
    // scope and re-read it — and the element — at every store.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let events_arr_obj =
        scope.new_array(cratonvm_types::ArrayElementType::Reference, event_count + 2);
    let events_h = scope.root(events_arr_obj);

    // START_DOCUMENT event
    let start_doc: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/events/XMLEvent", 3)?;
    scope.set_field(start_doc, 0, Value::Int(STAX_START_DOCUMENT));
    let events_arr = scope.get(&events_h);
    scope.set_array_element(events_arr, 0, Value::Object(Some(start_doc)));

    for (i, ev) in events.iter().enumerate() {
        let event_obj =
            try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/events/XMLEvent", 3)?;
        let event_h = scope.root(event_obj);
        scope.set_field(event_obj, 0, Value::Int(ev.event_type));
        if let Some(ref name) = ev.name {
            let s = scope.create_string(name);
            let event_obj = scope.get(&event_h);
            scope.set_field(event_obj, 1, Value::Object(Some(s)));
        }
        if let Some(ref text) = ev.text {
            let s = scope.create_string(text);
            let event_obj = scope.get(&event_h);
            scope.set_field(event_obj, 2, Value::Object(Some(s)));
        }
        let events_arr = scope.get(&events_h);
        let event_obj = scope.get(&event_h);
        scope.set_array_element(events_arr, i + 1, Value::Object(Some(event_obj)));
    }

    // Typed: fresh, stored before the next GC point (w16-e audit triage).
    let end_doc: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/events/XMLEvent", 3)?;
    scope.set_field(end_doc, 0, Value::Int(STAX_END_DOCUMENT));
    let events_arr = scope.get(&events_h);
    scope.set_array_element(events_arr, event_count + 1, Value::Object(Some(end_doc)));

    let total = (event_count + 2) as i32;
    let reader: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/XMLEventReader", 3)?;
    let events_arr = scope.get(&events_h);
    scope.set_field(reader, 0, Value::Object(Some(events_arr)));
    scope.set_field(reader, 1, Value::Int(total));
    scope.set_field(reader, 2, Value::Int(0));
    Ok(Some(Value::Object(Some(reader))))
}

/// Create an XMLStreamReader from parsed events.
fn stax_create_stream_reader(
    ctx: &mut dyn NativeContext,
    xml: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let events = stax_parse_events(xml);
    let event_count = events.len();
    // Everything here is a young object built next to another allocation: the
    // event array survives one element (and up to two strings) per event, and
    // each event survives the strings stored into it. Hold the array in the
    // scope and re-read it — and the element — at every store.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let events_arr_obj =
        scope.new_array(cratonvm_types::ArrayElementType::Reference, event_count + 2);
    let events_h = scope.root(events_arr_obj);

    // START_DOCUMENT event
    let start_doc: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/events/XMLEvent", 3)?;
    scope.set_field(start_doc, 0, Value::Int(STAX_START_DOCUMENT));
    let events_arr = scope.get(&events_h);
    scope.set_array_element(events_arr, 0, Value::Object(Some(start_doc)));

    for (i, ev) in events.iter().enumerate() {
        let event_obj =
            try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/events/XMLEvent", 3)?;
        let event_h = scope.root(event_obj);
        scope.set_field(event_obj, 0, Value::Int(ev.event_type));
        if let Some(ref name) = ev.name {
            let s = scope.create_string(name);
            let event_obj = scope.get(&event_h);
            scope.set_field(event_obj, 1, Value::Object(Some(s)));
        }
        if let Some(ref text) = ev.text {
            let s = scope.create_string(text);
            let event_obj = scope.get(&event_h);
            scope.set_field(event_obj, 2, Value::Object(Some(s)));
        }
        let events_arr = scope.get(&events_h);
        let event_obj = scope.get(&event_h);
        scope.set_array_element(events_arr, i + 1, Value::Object(Some(event_obj)));
    }

    // Typed: fresh, stored before the next GC point (w16-e audit triage).
    let end_doc: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/events/XMLEvent", 3)?;
    scope.set_field(end_doc, 0, Value::Int(STAX_END_DOCUMENT));
    let events_arr = scope.get(&events_h);
    scope.set_array_element(events_arr, event_count + 1, Value::Object(Some(end_doc)));

    let total = (event_count + 2) as i32;
    let reader: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "javax/xml/stream/XMLStreamReader", 4)?;
    let events_arr = scope.get(&events_h);
    scope.set_field(reader, 0, Value::Object(Some(events_arr)));
    scope.set_field(reader, 1, Value::Int(total));
    scope.set_field(reader, 2, Value::Int(0));
    scope.set_field(reader, 3, Value::Int(STAX_START_DOCUMENT));
    Ok(Some(Value::Object(Some(reader))))
}

struct StaxEvent {
    event_type: i32,
    name: Option<String>,
    text: Option<String>,
}

/// Simple XML to StAX event parser. Produces START_ELEMENT, END_ELEMENT, CHARACTERS events.
fn stax_parse_events(xml: &str) -> Vec<StaxEvent> {
    let mut events = Vec::new();
    let mut pos = 0;
    let bytes = xml.as_bytes();
    let len = bytes.len();

    while pos < len {
        if bytes[pos] == b'<' {
            // Collect any text before this tag
            // Check tag type
            if pos + 1 < len && bytes[pos + 1] == b'/' {
                // End element: </tag>
                let end = xml[pos..].find('>').map(|i| pos + i).unwrap_or(len);
                let tag = xml[pos + 2..end].trim().to_string();
                events.push(StaxEvent {
                    event_type: STAX_END_ELEMENT,
                    name: Some(tag),
                    text: None,
                });
                pos = end + 1;
            } else if pos + 1 < len && bytes[pos + 1] == b'?' {
                // Processing instruction — skip
                let end = xml[pos..].find("?>").map(|i| pos + i + 2).unwrap_or(len);
                pos = end;
            } else if pos + 3 < len && &xml[pos..pos + 4] == "<!--" {
                // Comment — skip
                let end = xml[pos..].find("-->").map(|i| pos + i + 3).unwrap_or(len);
                pos = end;
            } else if pos + 8 < len && &xml[pos..pos + 9] == "<![CDATA[" {
                // CDATA section
                let end = xml[pos..].find("]]>").map(|i| pos + i).unwrap_or(len);
                let text = xml[pos + 9..end].to_string();
                events.push(StaxEvent {
                    event_type: STAX_CHARACTERS,
                    name: None,
                    text: Some(text),
                });
                pos = end + 3;
            } else {
                // Start element: <tag ...> or <tag .../>
                let end = xml[pos..].find('>').map(|i| pos + i).unwrap_or(len);
                let content = &xml[pos + 1..end];
                let self_closing = content.ends_with('/');
                let content = if self_closing {
                    &content[..content.len() - 1]
                } else {
                    content
                };
                let tag = content.split_whitespace().next().unwrap_or("").to_string();
                events.push(StaxEvent {
                    event_type: STAX_START_ELEMENT,
                    name: Some(tag.clone()),
                    text: None,
                });
                if self_closing {
                    events.push(StaxEvent {
                        event_type: STAX_END_ELEMENT,
                        name: Some(tag),
                        text: None,
                    });
                }
                pos = end + 1;
            }
        } else {
            // Text content
            let end = xml[pos..].find('<').map(|i| pos + i).unwrap_or(len);
            let text = xml[pos..end].to_string();
            if !text.trim().is_empty() {
                events.push(StaxEvent {
                    event_type: STAX_CHARACTERS,
                    name: None,
                    text: Some(text),
                });
            }
            pos = end;
        }
    }
    events
}

// =============================================================================
// T3.10 — javax.script / ScriptEngineManager
// =============================================================================

/// Register ScriptEngineManager natives. Provides engine discovery and
/// a minimal "cratonvm-eval" engine that evaluates simple numeric expressions.
pub(crate) fn register_t310_scripting(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sem = "javax/script/ScriptEngineManager";
    // Fields: 0=engines_list
    r.register(sem, "<init>", "()V", |ctx, args| {
        // gc-common w19-c: the receiver was held raw across the list's
        // allocation (an `ArrayList` class resolution and an array).
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(obj_arg(args, 0)?);
        let engines = t3_new_array_list(&mut *scope, 4)?;
        let this = scope.get(&this_h);
        scope.set_field(this, 0, Value::Object(Some(engines)));
        Ok(None)
    });

    // getEngineByName(String) -> ScriptEngine
    r.register(
        sem,
        "getEngineByName",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        |ctx, args| {
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // We provide a minimal "cratonvm-eval" and "js" engine (expression evaluator)
            if name == "cratonvm-eval"
                || name == "js"
                || name == "javascript"
                || name == "nashorn"
                || name == "graal.js"
                || name == "rhino"
            {
                return script_new_engine(ctx, &name);
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // getEngineByExtension(String) -> ScriptEngine
    r.register(
        sem,
        "getEngineByExtension",
        "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        |ctx, args| {
            let ext = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if ext == "js" {
                return script_new_engine(ctx, "javascript");
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // --- ScriptEngine ---
    let se = "javax/script/ScriptEngine";
    // eval(String) -> Object — evaluate a script expression
    r.register(
        se,
        "eval",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let script = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            // Simple expression evaluator for numeric expressions
            let result = eval_simple_expression(&script);
            match result {
                Some(n) => {
                    // Box the result as an Integer or Double
                    if n.fract() == 0.0 && n.abs() < i32::MAX as f64 {
                        let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1)?;
                        ctx.set_field(boxed, 0, Value::Int(n as i32));
                        Ok(Some(Value::Object(Some(boxed))))
                    } else {
                        let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Double", 1)?;
                        ctx.set_field(boxed, 0, Value::Double(n));
                        Ok(Some(Value::Object(Some(boxed))))
                    }
                }
                None => {
                    // Return the script as a string if not evaluable
                    let s = ctx.create_string(&script);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        },
    );

    // put(String, Object) — set a binding
    r.register(
        se,
        "put",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let bindings = match ctx.get_field(this, 0) {
                Value::Object(Some(b)) => b,
                _ => return Ok(None),
            };
            script_put_binding(ctx, bindings, key, val);
            Ok(None)
        },
    );

    // get(String) -> Object
    r.register(
        se,
        "get",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let bindings = match ctx.get_field(this, 0) {
                Value::Object(Some(b)) => b,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(script_get_binding(ctx, bindings, key)))
        },
    );

    // createBindings() -> Bindings
    r.register(
        se,
        "createBindings",
        "()Ljavax/script/Bindings;",
        |ctx, _args| {
            let bindings = t3_new_binding_store(ctx, "javax/script/SimpleBindings", 16)?;
            Ok(Some(Value::Object(Some(bindings))))
        },
    );

    // --- SimpleBindings ---
    let sb = "javax/script/SimpleBindings";
    r.register(
        sb,
        "put",
        "(Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let old = script_get_binding(ctx, this, key);
            script_put_binding(ctx, this, key, val);
            Ok(Some(old))
        },
    );
    r.register(
        sb,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            Ok(Some(script_get_binding(ctx, this, key)))
        },
    );
    r.set_category(__prev_cat);
}

/// A synthetic `ScriptEngine` named `name`: a fresh `SimpleBindings` store in
/// field 0, the name in field 1.
///
/// gc-common w19-c: `getEngineByName` / `getEngineByExtension` held the engine
/// raw across the bindings' class resolution, both arrays and the name, and
/// the bindings raw across the arrays. The engine is rooted across all of it.
fn script_new_engine(ctx: &mut dyn NativeContext, name: &str) -> MethodCallResult {
    let engine = try_alloc_concurrent_synthetic(ctx, "javax/script/ScriptEngine", 2)?;
    let mut scope = NativeHandleScope::new(ctx);
    let engine_h = scope.root(engine);
    let bindings = t3_new_binding_store(&mut *scope, "javax/script/SimpleBindings", 16)?;
    let engine = scope.get(&engine_h);
    scope.set_field(engine, 0, Value::Object(Some(bindings)));
    let name_s = scope.create_string(name);
    let engine = scope.get(&engine_h);
    scope.set_field(engine, 1, Value::Object(Some(name_s)));
    Ok(Some(Value::Object(Some(engine))))
}

/// Put key-value into SimpleBindings store.
fn script_put_binding(ctx: &mut dyn NativeContext, bindings: ObjectRef, key: Value, val: Value) {
    let size = ctx.get_field(bindings, 2).as_int().unwrap_or(0) as usize;
    let keys_arr = match ctx.get_field(bindings, 0) {
        Value::Object(Some(a)) => a,
        _ => return,
    };
    let vals_arr = match ctx.get_field(bindings, 1) {
        Value::Object(Some(a)) => a,
        _ => return,
    };
    // Check existing
    for i in 0..size {
        let ek = ctx.get_array_element(keys_arr, i);
        if let (Value::Object(Some(a)), Value::Object(Some(b))) = (ek, key) {
            if let (Some(sa), Some(sb)) = (ctx.read_string(a), ctx.read_string(b)) {
                if sa == sb {
                    ctx.set_array_element(vals_arr, i, val);
                    return;
                }
            }
        }
    }
    // Add new
    let cap = ctx.array_length(keys_arr);
    if size < cap {
        ctx.set_array_element(keys_arr, size, key);
        ctx.set_array_element(vals_arr, size, val);
        ctx.set_field(bindings, 2, Value::Int((size + 1) as i32));
    }
}

/// Get value from SimpleBindings store.
fn script_get_binding(ctx: &mut dyn NativeContext, bindings: ObjectRef, key: Value) -> Value {
    let size = ctx.get_field(bindings, 2).as_int().unwrap_or(0) as usize;
    let keys_arr = match ctx.get_field(bindings, 0) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    let vals_arr = match ctx.get_field(bindings, 1) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    for i in 0..size {
        let ek = ctx.get_array_element(keys_arr, i);
        if let (Value::Object(Some(a)), Value::Object(Some(b))) = (ek, key) {
            if let (Some(sa), Some(sb)) = (ctx.read_string(a), ctx.read_string(b)) {
                if sa == sb {
                    return ctx.get_array_element(vals_arr, i);
                }
            }
        }
    }
    Value::Object(None)
}

/// Simple numeric expression evaluator for ScriptEngine.eval().
/// Supports: integer/float literals, +, -, *, /, parentheses.
fn eval_simple_expression(expr: &str) -> Option<f64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    // Simple recursive descent parser
    let tokens = tokenize_expr(expr)?;
    let mut pos = 0;
    let result = parse_add_sub(&tokens, &mut pos)?;
    if pos == tokens.len() {
        Some(result)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
enum ExprToken {
    Num(f64),
    Op(char),
    LParen,
    RParen,
}

fn tokenize_expr(expr: &str) -> Option<Vec<ExprToken>> {
    let mut tokens = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c.is_ascii_digit() || c == '.' {
            let mut num = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() || d == '.' {
                    num.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(ExprToken::Num(num.parse().ok()?));
        } else if c == '+' || c == '-' || c == '*' || c == '/' || c == '%' {
            tokens.push(ExprToken::Op(c));
            chars.next();
        } else if c == '(' {
            tokens.push(ExprToken::LParen);
            chars.next();
        } else if c == ')' {
            tokens.push(ExprToken::RParen);
            chars.next();
        } else {
            return None; // Unknown character — not a simple expression
        }
    }
    Some(tokens)
}

fn parse_add_sub(tokens: &[ExprToken], pos: &mut usize) -> Option<f64> {
    let mut left = parse_mul_div(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            ExprToken::Op('+') => {
                *pos += 1;
                left += parse_mul_div(tokens, pos)?;
            }
            ExprToken::Op('-') => {
                *pos += 1;
                left -= parse_mul_div(tokens, pos)?;
            }
            _ => break,
        }
    }
    Some(left)
}

fn parse_mul_div(tokens: &[ExprToken], pos: &mut usize) -> Option<f64> {
    let mut left = parse_unary(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            ExprToken::Op('*') => {
                *pos += 1;
                left *= parse_unary(tokens, pos)?;
            }
            ExprToken::Op('/') => {
                *pos += 1;
                let r = parse_unary(tokens, pos)?;
                if r == 0.0 {
                    return None;
                }
                left /= r;
            }
            ExprToken::Op('%') => {
                *pos += 1;
                let r = parse_unary(tokens, pos)?;
                if r == 0.0 {
                    return None;
                }
                left %= r;
            }
            _ => break,
        }
    }
    Some(left)
}

fn parse_unary(tokens: &[ExprToken], pos: &mut usize) -> Option<f64> {
    if *pos >= tokens.len() {
        return None;
    }
    match &tokens[*pos] {
        ExprToken::Op('-') => {
            *pos += 1;
            Some(-parse_primary(tokens, pos)?)
        }
        ExprToken::Op('+') => {
            *pos += 1;
            parse_primary(tokens, pos)
        }
        _ => parse_primary(tokens, pos),
    }
}

fn parse_primary(tokens: &[ExprToken], pos: &mut usize) -> Option<f64> {
    if *pos >= tokens.len() {
        return None;
    }
    match &tokens[*pos] {
        ExprToken::Num(n) => {
            let v = *n;
            *pos += 1;
            Some(v)
        }
        ExprToken::LParen => {
            *pos += 1;
            let v = parse_add_sub(tokens, pos)?;
            if *pos < tokens.len() && matches!(tokens[*pos], ExprToken::RParen) {
                *pos += 1;
            }
            Some(v)
        }
        _ => None,
    }
}

// =============================================================================
// T3.11 — Internationalization extras
// =============================================================================

pub(crate) fn register_t311_i18n(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // `Locale.getDefault()`.
    //
    // SHADOWING NOTE: this is the SECOND registration of this key.
    // `locale_bootstrap::register` (reached from `register_essential_natives`)
    // registers it first, and `register_synthetic_overrides` — which calls this
    // function — runs after it, so under `#[cfg(feature = "synthetic-jdk")]`
    // *this* closure is the one that runs (registration is last-wins). Both
    // therefore have to resolve the locale the same way, which they now do via
    // the single `locale_bootstrap::resolve_default_locale` helper.
    //
    // What used to be here: a hand-rolled `$LANG`-then-`$LC_ALL` parse with no
    // C/POSIX mapping. On a bare Linux shell (`LANG=C`, the POSIX default, and
    // what ubuntu-latest CI gives you) that reported language `"C"`. A real JVM
    // never does: the HotSpot launcher's `java_props_md.c` maps the `C` and
    // `POSIX` locales onto English, so `Locale.getDefault().getLanguage()` is
    // `"en"` there. The precedence was inverted too (`LC_ALL` must override
    // `LANG`, not the other way round).
    let loc = "java/util/Locale";
    r.register(loc, "getDefault", "()Ljava/util/Locale;", |ctx, _args| {
        let (language, country) = crate::locale_bootstrap::resolve_default_locale(&*ctx);

        // Use the shared `locale_alloc` helper: it records the
        // language/country in the ObjectRef-keyed side table and leaves the
        // real-JDK `Locale` instance slots (baseLocale / localeExtensions)
        // untouched. Writing Strings into those typed-object slots used to
        // poison real-JDK Locale bytecode dispatch (bogus
        // `NoSuchMethodError java/lang/String.getUnicodeLocaleType`).
        let locale = crate::locale_alloc(ctx, &language, &country)?;
        Ok(Some(Value::Object(Some(locale))))
    });

    // Charset.availableCharsets() -> SortedMap
    let cs = "java/nio/charset/Charset";
    r.register(
        cs,
        "availableCharsets",
        "()Ljava/util/SortedMap;",
        |ctx, _args| {
            // Return a TreeMap with the standard charsets
            let charsets = [
                "US-ASCII",
                "ISO-8859-1",
                "UTF-8",
                "UTF-16",
                "UTF-16BE",
                "UTF-16LE",
                "UTF-32",
                "UTF-32BE",
                "UTF-32LE",
                "Shift_JIS",
                "EUC-JP",
                "ISO-2022-JP",
                "Big5",
                "EUC-KR",
                "GB2312",
                "GBK",
                "GB18030",
                "windows-1252",
                "windows-1251",
                "KOI8-R",
                "ISO-8859-2",
                "ISO-8859-15",
            ];
            // The map and the two arrays are built first and then survive
            // four allocations per charset, so all three go in the scope, and
            // the key string survives the charset allocation beside it.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let map_obj = try_alloc_concurrent_synthetic(&mut *scope, "java/util/TreeMap", 3)?;
            let map_h = scope.root(map_obj);
            let keys_obj =
                scope.new_array(cratonvm_types::ArrayElementType::Reference, charsets.len());
            let keys_h = scope.root(keys_obj);
            let vals_obj =
                scope.new_array(cratonvm_types::ArrayElementType::Reference, charsets.len());
            let vals_h = scope.root(vals_obj);
            for (i, name) in charsets.iter().enumerate() {
                let key = scope.create_string(name);
                let key_h = scope.root(key);
                let charset =
                    try_alloc_concurrent_synthetic(&mut *scope, "java/nio/charset/Charset", 2)?;
                let charset_h = scope.root(charset);
                let name_s = scope.create_string(name);
                let charset = scope.get(&charset_h);
                scope.set_field(charset, 0, Value::Object(Some(name_s)));
                scope.set_field(charset, 1, Value::Object(None)); // aliases
                let keys = scope.get(&keys_h);
                let vals = scope.get(&vals_h);
                let key = scope.get(&key_h);
                scope.set_array_element(keys, i, Value::Object(Some(key)));
                scope.set_array_element(vals, i, Value::Object(Some(charset)));
            }
            let map = scope.get(&map_h);
            let keys = scope.get(&keys_h);
            let vals = scope.get(&vals_h);
            scope.set_field(map, 0, Value::Object(Some(keys)));
            scope.set_field(map, 1, Value::Object(Some(vals)));
            scope.set_field(map, 2, Value::Int(charsets.len() as i32));
            Ok(Some(Value::Object(Some(map))))
        },
    );

    // String.getBytes(String charsetName) — extended encoding support
    // This is registered elsewhere for UTF-8/ISO-8859-1; we add Shift_JIS support
    // The encoding/decoding for exotic charsets is best-effort.
    r.set_category(__prev_cat);
}

// =============================================================================
// T3.12-T3.15 — Tooling natives
// =============================================================================

/// Register minimal tooling natives for javac, JShell, jpackage, javadoc.
/// These enable bootstrapping / booting the tools on CratonVM without implementing
/// the full tool functionality (which requires running JDK bytecode).
pub(crate) fn register_t312_tooling(r: &mut NativeMethodRegistry) {
    // SyntheticStub: javac/JavaCompiler/JShell/jpackage/javadoc natives return
    // placeholder objects and error/exit codes without performing any real
    // compilation, packaging, or doc generation ("requires full JDK
    // toolchain"). CompilationTask.call() always returns false.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // --- T3.12: javac ---
    let javac = "com/sun/tools/javac/Main";
    // compile(String[]) -> int (exit code)
    r.register(javac, "compile", "([Ljava/lang/String;)I", |ctx, args| {
        // Extract source files from args
        let files_arr = match args.get(0) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(1))), // error
        };
        let len = ctx.array_length(files_arr);
        if len == 0 {
            return Ok(Some(Value::Int(1)));
        }
        // For now, report that compilation is not supported in native mode
        // Real javac compilation requires the full JDK compiler pipeline
        let msg = ctx.create_string(
            "CratonVM: javac native compilation not yet available. Use JDK bytecode path.",
        );
        let stderr = try_alloc_concurrent_synthetic(ctx, "java/io/PrintStream", 1)?;
        ctx.set_field(stderr, 0, Value::Object(Some(msg)));
        Ok(Some(Value::Int(2))) // exit code 2 = error
    });

    // JavaCompiler via ToolProvider
    let tp = "javax/tools/ToolProvider";
    r.register(
        tp,
        "getSystemJavaCompiler",
        "()Ljavax/tools/JavaCompiler;",
        |ctx, _args| {
            // Return a compiler object with run/getTask/getStandardFileManager support
            let compiler =
                t3_new_with_string(ctx, "javax/tools/JavaCompiler", 2, 0, "cratonvm-javac")?;
            ctx.set_field(compiler, 1, Value::Int(0)); // invocation count
            Ok(Some(Value::Object(Some(compiler))))
        },
    );

    // JavaCompiler.run(InputStream, OutputStream, OutputStream, String...) -> int
    let jc = "javax/tools/JavaCompiler";
    r.register(
        jc,
        "run",
        "(Ljava/io/InputStream;Ljava/io/OutputStream;Ljava/io/OutputStream;[Ljava/lang/String;)I",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let files = match args.get(4) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(1))),
            };
            let len = ctx.array_length(files);
            if len == 0 {
                return Ok(Some(Value::Int(1))); // no files to compile
            }
            // Real javac compilation requires JDK compiler pipeline; report error code 2
            Ok(Some(Value::Int(2)))
        },
    );

    // JavaCompiler.getStandardFileManager(DiagnosticListener, Locale, Charset) -> StandardJavaFileManager
    r.register(jc, "getStandardFileManager", "(Ljavax/tools/DiagnosticListener;Ljava/util/Locale;Ljava/nio/charset/Charset;)Ljavax/tools/StandardJavaFileManager;", |ctx, _args| {
        let fm = t3_new_with_string(
            ctx,
            "javax/tools/StandardJavaFileManager",
            2,
            0,
            "cratonvm-filemanager",
        )?;
        ctx.set_field(fm, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(fm))))
    });

    // JavaCompiler.getTask(Writer, FileManager, DiagnosticListener, Iterable, Iterable, Iterable) -> CompilationTask
    r.register(jc, "getTask", "(Ljava/io/Writer;Ljavax/tools/JavaFileManager;Ljavax/tools/DiagnosticListener;Ljava/lang/Iterable;Ljava/lang/Iterable;Ljava/lang/Iterable;)Ljavax/tools/JavaCompiler$CompilationTask;", |ctx, _args| {
        let task = try_alloc_concurrent_synthetic(ctx, "javax/tools/JavaCompiler$CompilationTask", 1)?;
        ctx.set_field(task, 0, Value::Int(0)); // not yet called
        Ok(Some(Value::Object(Some(task))))
    });

    // CompilationTask.call() -> Boolean
    let ct = "javax/tools/JavaCompiler$CompilationTask";
    r.register(ct, "call", "()Ljava/lang/Boolean;", |ctx, _args| {
        // Native compilation not available — return false (compilation failed)
        let result = try_alloc_concurrent_synthetic(ctx, "java/lang/Boolean", 1)?;
        ctx.set_field(result, 0, Value::Int(0)); // false
        Ok(Some(Value::Object(Some(result))))
    });

    // StandardJavaFileManager.close() — STUB-REMOVAL (wave 2): was an
    // unconditional no-op, so a closed file manager stayed indistinguishable
    // from an open one. `JavaFileManager.close()` is specified as "a file
    // manager that has been closed will throw IllegalStateException on
    // subsequent use", and code that relies on that (a try-with-resources
    // block asserting the manager is unusable afterwards) saw no difference at
    // all. Record the closed flag in slot 1 (set to 0 at construction by
    // `getStandardFileManager` above) and enforce it below. `close()` itself is
    // idempotent, as the spec requires.
    let sfm = "javax/tools/StandardJavaFileManager";
    r.register(sfm, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // StandardJavaFileManager.getJavaFileObjectsFromStrings(Iterable) -> Iterable
    r.register(
        sfm,
        "getJavaFileObjectsFromStrings",
        "(Ljava/lang/Iterable;)Ljava/lang/Iterable;",
        |ctx, args| {
            // See `close()` above: use after close is an IllegalStateException,
            // not a silently-empty result.
            let this = obj_arg(args, 0)?;
            if ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "file manager is closed".to_string(),
                }
                .into());
            }
            // Return an empty list; file objects require JDK filesystem integration
            let list = t3_new_array_list(ctx, 0)?;
            Ok(Some(Value::Object(Some(list))))
        },
    );

    // --- T3.13: JShell ---
    let jshell = "jdk/jshell/JShell";
    r.register(jshell, "create", "()Ljdk/jshell/JShell;", |ctx, _args| {
        // Fields: 0=history_list, 1=variable_count
        // gc-common w19-c: the shell was held raw across the history list's
        // allocation (an `ArrayList` class resolution and an array).
        let shell = try_alloc_concurrent_synthetic(ctx, "jdk/jshell/JShell", 2)?;
        let mut scope = NativeHandleScope::new(ctx);
        let shell_h = scope.root(shell);
        let history = t3_new_array_list(&mut *scope, 32)?;
        let shell = scope.get(&shell_h);
        scope.set_field(shell, 0, Value::Object(Some(history)));
        scope.set_field(shell, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(shell))))
    });

    // eval(String) -> List<SnippetEvent>
    r.register(
        jshell,
        "eval",
        "(Ljava/lang/String;)Ljava/util/List;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // See `close()` below: real `JShell.eval` on a closed instance
            // throws IllegalStateException("Closed"); slot 0 (the history list)
            // is nulled by close, so its absence is the closed marker.
            if matches!(ctx.get_field(this, 0), Value::Object(None)) {
                return Err(RuntimeError::IllegalStateException {
                    message: "Closed".to_string(),
                }
                .into());
            }
            let source = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let source_trimmed = source.trim().trim_end_matches(';');
            let result_str = jshell_evaluate(source_trimmed);

            // gc-common w16-e: five allocations in a row, and each object
            // used to be stored through the address it had before the ones
            // after it (the event after both strings, the source string after
            // the value string, the event and the list after the array). All
            // three are held in one scope and re-read at every store.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            // Create a SnippetEvent
            let event = try_alloc_concurrent_synthetic(&mut *scope, "jdk/jshell/SnippetEvent", 3)?;
            let event_h = scope.root(event);
            let src = scope.create_string(&source);
            let src_h = scope.root(src);
            let val = scope.create_string(&result_str);
            let src = scope.get(&src_h);
            let event = scope.get(&event_h);
            scope.set_field(event, 0, Value::Object(Some(src))); // source
            scope.set_field(event, 1, Value::Object(Some(val))); // value
            scope.set_field(event, 2, Value::Int(0)); // status (0=VALID)

            // Wrap in a single-element list
            let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
            let list_h = scope.root(list);
            let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            let event = scope.get(&event_h);
            scope.set_array_element(arr, 0, Value::Object(Some(event)));
            let list = scope.get(&list_h);
            scope.set_field(list, 0, Value::Object(Some(arr)));
            scope.set_field(list, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(list))))
        },
    );

    // STUB-REMOVAL (wave 2): was an unconditional no-op, so a "closed" JShell
    // kept evaluating snippets and kept its whole history list reachable. Real
    // `JShell.close()` shuts the instance down and every later `eval` throws
    // IllegalStateException. Release the history (slot 0) and reset the
    // counter; `eval` above treats a null history as closed. Idempotent.
    r.register(jshell, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });

    // SnippetEvent accessors
    let se = "jdk/jshell/SnippetEvent";
    r.register(se, "value", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        se,
        "status",
        "()Ljdk/jshell/Snippet$Status;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Read the event's status code BEFORE allocating (gc-common
            // w19-c): `this` was read after the `Snippet$Status` class
            // resolution and the name's allocation.
            let code = ctx.get_field(this, 2);
            // Return a Status enum
            let status = t3_new_with_string(ctx, "jdk/jshell/Snippet$Status", 2, 0, "VALID")?;
            ctx.set_field(status, 1, code);
            Ok(Some(Value::Object(Some(status))))
        },
    );

    // --- T3.14: jpackage ---
    let jp = "jdk/jpackage/main/Main";
    r.register(jp, "execute", "([Ljava/lang/String;)I", |ctx, args| {
        let files = match args.get(0) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))), // no args = success (help mode)
        };
        let len = ctx.array_length(files);
        if len == 0 {
            return Ok(Some(Value::Int(0))); // no args = success (help mode)
        }
        // Check for --help flag
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(files, i) {
                if let Some(arg) = ctx.read_string(s) {
                    if arg == "--help" || arg == "-h" {
                        return Ok(Some(Value::Int(0)));
                    }
                }
            }
        }
        // Packaging requires the full JDK toolchain; return error 1
        Ok(Some(Value::Int(1)))
    });

    // --- T3.15: javadoc ---
    let jd = "jdk/javadoc/internal/tool/Main";
    r.register(jd, "execute", "([Ljava/lang/String;)I", |ctx, args| {
        let files = match args.get(0) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))), // no args = success (help mode)
        };
        let len = ctx.array_length(files);
        if len == 0 {
            return Ok(Some(Value::Int(0))); // no args = success (help mode)
        }
        // Check for --help flag
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(files, i) {
                if let Some(arg) = ctx.read_string(s) {
                    if arg == "--help" || arg == "-help" || arg == "-h" {
                        return Ok(Some(Value::Int(0)));
                    }
                }
            }
        }
        // Doc generation requires the full JDK toolchain; return error 1
        Ok(Some(Value::Int(1)))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// T3.1.16-T3.1.18 — Virtual threads extras (structured concurrency)
// =============================================================================

pub(crate) fn register_t31_structured_concurrency(_r: &mut NativeMethodRegistry) {
    // T16.4: StructuredTaskScope / Subtask / ShutdownOnSuccess / ShutdownOnFailure
    // natives are owned by `jdk25_concurrency::register_jdk25_concurrency_natives`
    // (see `native-builtins/src/jdk25_concurrency.rs`). This function intentionally
    // does not register competing stubs — registering them here caused the
    // canonical 8-field layout to be overridden with the earlier 2-field stubs,
    // silently breaking `close()`, `result()`, and `throwIfFailed()` contracts.
}

// =============================================================================
// XML well-formedness validation
// =============================================================================

/// Validates that an XML string is well-formed.
/// Checks: matching open/close tags, proper nesting, no unmatched brackets.
fn validate_xml_well_formedness(xml: &str) -> Result<(), String> {
    let mut tag_stack: Vec<String> = Vec::new();
    let bytes = xml.as_bytes();
    let len = bytes.len();
    let mut pos = 0;

    while pos < len {
        if bytes[pos] == b'<' {
            if pos + 1 >= len {
                return Err("Unexpected end of document after '<'".to_string());
            }

            if pos + 1 < len && bytes[pos + 1] == b'/' {
                // End tag
                let end = xml[pos..]
                    .find('>')
                    .map(|i| pos + i)
                    .ok_or_else(|| "Unclosed end tag".to_string())?;
                let tag_name = xml[pos + 2..end].trim().to_string();
                match tag_stack.pop() {
                    Some(open) if open == tag_name => {}
                    Some(open) => {
                        return Err(format!(
                            "Mismatched tags: expected </{}>, found </{}>",
                            open, tag_name
                        ));
                    }
                    None => {
                        return Err(format!(
                            "Unexpected closing tag </{}> with no matching open tag",
                            tag_name
                        ));
                    }
                }
                pos = end + 1;
            } else if pos + 1 < len && bytes[pos + 1] == b'?' {
                // Processing instruction — skip
                let end = xml[pos..]
                    .find("?>")
                    .map(|i| pos + i + 2)
                    .ok_or_else(|| "Unclosed processing instruction".to_string())?;
                pos = end;
            } else if pos + 3 < len && &xml[pos..pos + 4] == "<!--" {
                // Comment — skip
                let end = xml[pos..]
                    .find("-->")
                    .map(|i| pos + i + 3)
                    .ok_or_else(|| "Unclosed comment".to_string())?;
                pos = end;
            } else if pos + 8 < len && &xml[pos..pos + 9] == "<![CDATA[" {
                // CDATA — skip
                let end = xml[pos..]
                    .find("]]>")
                    .map(|i| pos + i + 3)
                    .ok_or_else(|| "Unclosed CDATA section".to_string())?;
                pos = end;
            } else {
                // Start tag or self-closing
                let end = xml[pos..]
                    .find('>')
                    .map(|i| pos + i)
                    .ok_or_else(|| "Unclosed start tag".to_string())?;
                let content = &xml[pos + 1..end];
                let self_closing = content.ends_with('/');
                let content = if self_closing {
                    &content[..content.len() - 1]
                } else {
                    content
                };
                let tag_name = content.split_whitespace().next().unwrap_or("").to_string();

                if tag_name.is_empty() {
                    return Err("Empty tag name".to_string());
                }

                // Validate tag name: must start with letter or underscore
                let first_char = tag_name.chars().next().unwrap();
                if !first_char.is_alphabetic() && first_char != '_' {
                    return Err(format!("Invalid tag name: '{}'", tag_name));
                }

                if !self_closing {
                    tag_stack.push(tag_name);
                }
                pos = end + 1;
            }
        } else {
            // Text content — check for stray '>' or forbidden characters
            let next_lt = xml[pos..].find('<').map(|i| pos + i).unwrap_or(len);
            let text = &xml[pos..next_lt];
            // Check for unescaped '&' not followed by valid entity
            let mut amp_pos = 0;
            while amp_pos < text.len() {
                if text.as_bytes()[amp_pos] == b'&' {
                    let rest = &text[amp_pos..];
                    let valid_entities = ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"];
                    let is_char_ref = rest.len() > 2 && rest.as_bytes()[1] == b'#';
                    let is_named = valid_entities.iter().any(|e| rest.starts_with(e));
                    if !is_named && !is_char_ref {
                        // Check if it's at least closed with semicolon (custom entity)
                        if !rest[1..].contains(';') {
                            return Err("Unescaped '&' in text content".to_string());
                        }
                    }
                }
                amp_pos += 1;
            }
            pos = next_lt;
        }
    }

    if !tag_stack.is_empty() {
        return Err(format!("Unclosed tag(s): {}", tag_stack.join(", ")));
    }

    Ok(())
}

// =============================================================================
// JShell expression evaluator
// =============================================================================

/// Evaluate a JShell snippet. Handles:
/// - Numeric arithmetic expressions: "1 + 2" → "3"
/// - String literals: "\"hello\"" → "hello"
/// - String concatenation: "\"hello\" + \" world\"" → "hello world"
/// - Boolean literals: "true" → "true", "false" → "false"
/// - Variable declarations: "int x = 5" → "5", "String s = \"hi\"" → "hi"
/// - Comparison expressions: "3 > 2" → "true"
/// - Ternary expressions: "true ? 1 : 2" → "1"
fn jshell_evaluate(source: &str) -> String {
    let s = source.trim();
    if s.is_empty() {
        return String::new();
    }

    // Boolean literals
    if s == "true" || s == "false" {
        return s.to_string();
    }

    // null literal
    if s == "null" {
        return "null".to_string();
    }

    // Variable declaration: "type name = expr"
    if let Some(expr) = try_parse_var_decl(s) {
        return jshell_evaluate(&expr);
    }

    // String concatenation: "..." + "..." (check before plain string literal)
    if let Some(result) = try_string_concat(s) {
        return result;
    }

    // String literal: "..."
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        // Verify this is a single string literal (closing quote is at the end)
        if let Some(end) = find_closing_quote(s, 1) {
            if end == s.len() - 1 {
                return unescape_java_string(&s[1..end]);
            }
        }
    }

    // Ternary: cond ? a : b
    if let Some(result) = try_ternary(s) {
        return result;
    }

    // Comparison expressions: a > b, a < b, a >= b, a <= b, a == b, a != b
    if let Some(result) = try_comparison(s) {
        return result;
    }

    // Numeric expression
    if let Some(val) = eval_simple_expression(s) {
        if val.fract() == 0.0 && val.abs() < i64::MAX as f64 {
            return format!("{}", val as i64);
        } else {
            return format!("{}", val);
        }
    }

    // Fallback: return source as-is
    s.to_string()
}

/// Unescape Java string escape sequences.
fn unescape_java_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('\'') => result.push('\''),
                Some('0') => result.push('\0'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Try to parse a variable declaration like "int x = 5" or "String s = \"hello\""
fn try_parse_var_decl(s: &str) -> Option<String> {
    // Patterns: "int x = ...", "long x = ...", "double x = ...", "float x = ...",
    //           "String x = ...", "boolean x = ...", "var x = ...", "char x = ..."
    let type_prefixes = [
        "int ", "long ", "double ", "float ", "short ", "byte ", "boolean ", "char ", "String ",
        "var ", "Object ",
    ];
    for prefix in &type_prefixes {
        if s.starts_with(prefix) {
            let rest = &s[prefix.len()..];
            // Find "= " in the rest
            if let Some(eq_pos) = rest.find('=') {
                let expr = rest[eq_pos + 1..].trim();
                return Some(expr.to_string());
            }
        }
    }
    None
}

/// Try to evaluate string concatenation: "..." + "..." + expr
fn try_string_concat(s: &str) -> Option<String> {
    // Quick check: must contain at least one string literal
    if !s.contains('"') {
        return None;
    }

    let mut result = String::new();
    let mut remaining = s.trim();
    let mut found_string = false;

    loop {
        remaining = remaining.trim();
        if remaining.is_empty() {
            break;
        }

        if remaining.starts_with('"') {
            // String literal
            let end = find_closing_quote(remaining, 1)?;
            let literal = &remaining[1..end];
            result.push_str(&unescape_java_string(literal));
            remaining = remaining[end + 1..].trim();
            found_string = true;
        } else {
            // Non-string part — try to evaluate as number
            let next_plus = find_top_level_plus(remaining);
            let part = match next_plus {
                Some(p) => &remaining[..p],
                None => remaining,
            };
            let part = part.trim();

            if let Some(val) = eval_simple_expression(part) {
                if val.fract() == 0.0 {
                    result.push_str(&format!("{}", val as i64));
                } else {
                    result.push_str(&format!("{}", val));
                }
            } else if part == "true" || part == "false" || part == "null" {
                result.push_str(part);
            } else {
                return None; // Can't evaluate this part
            }

            remaining = match next_plus {
                Some(p) => &remaining[p..],
                None => "",
            };
        }

        // Expect '+' or end
        remaining = remaining.trim();
        if remaining.starts_with('+') {
            remaining = &remaining[1..];
        } else if !remaining.is_empty() {
            return None;
        }
    }

    if found_string {
        Some(result)
    } else {
        None
    }
}

/// Find the closing quote in a string, handling escape sequences.
fn find_closing_quote(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2; // skip escaped char
        } else if bytes[i] == b'"' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// Find a '+' operator that's not inside a string literal.
fn find_top_level_plus(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'+' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// Try to evaluate a comparison expression.
fn try_comparison(s: &str) -> Option<String> {
    // Order matters: check >= and <= before > and <, check != before ==
    let ops: &[(&str, fn(f64, f64) -> bool)] = &[
        (">=", |a, b| a >= b),
        ("<=", |a, b| a <= b),
        ("!=", |a, b| (a - b).abs() > f64::EPSILON),
        ("==", |a, b| (a - b).abs() < f64::EPSILON),
        (">", |a, b| a > b),
        ("<", |a, b| a < b),
    ];
    for (op, func) in ops {
        if let Some(idx) = s.find(op) {
            let left = s[..idx].trim();
            let right = s[idx + op.len()..].trim();
            let lv = eval_simple_expression(left)?;
            let rv = eval_simple_expression(right)?;
            return Some(if func(lv, rv) { "true" } else { "false" }.to_string());
        }
    }
    None
}

/// Try to evaluate a ternary expression: "cond ? a : b"
fn try_ternary(s: &str) -> Option<String> {
    let q_pos = s.find('?')?;
    let cond = s[..q_pos].trim();
    let rest = &s[q_pos + 1..];
    let colon_pos = rest.find(':')?;
    let if_true = rest[..colon_pos].trim();
    let if_false = rest[colon_pos + 1..].trim();

    let cond_val = if cond == "true" {
        true
    } else if cond == "false" {
        false
    } else if let Some(result) = try_comparison(cond) {
        result == "true"
    } else {
        return None;
    };

    let branch = if cond_val { if_true } else { if_false };
    Some(jshell_evaluate(branch))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn test_eval_simple_expression() {
        assert_eq!(eval_simple_expression("1 + 1"), Some(2.0));
        assert_eq!(eval_simple_expression("3 * 4 + 2"), Some(14.0));
        assert_eq!(eval_simple_expression("(2 + 3) * 4"), Some(20.0));
        assert_eq!(eval_simple_expression("10 / 3"), Some(10.0 / 3.0));
        assert_eq!(eval_simple_expression("-5 + 3"), Some(-2.0));
        assert_eq!(eval_simple_expression("2.5 * 4"), Some(10.0));
        assert_eq!(eval_simple_expression(""), None);
        assert_eq!(eval_simple_expression("hello"), None);
    }

    #[test]
    fn test_stax_parse_events() {
        let xml = r#"<root><child>text</child></root>"#;
        let events = stax_parse_events(xml);
        assert_eq!(events.len(), 5); // start root, start child, text, end child, end root
        assert_eq!(events[0].event_type, STAX_START_ELEMENT);
        assert_eq!(events[0].name.as_deref(), Some("root"));
        assert_eq!(events[1].event_type, STAX_START_ELEMENT);
        assert_eq!(events[1].name.as_deref(), Some("child"));
        assert_eq!(events[2].event_type, STAX_CHARACTERS);
        assert_eq!(events[2].text.as_deref(), Some("text"));
        assert_eq!(events[3].event_type, STAX_END_ELEMENT);
        assert_eq!(events[4].event_type, STAX_END_ELEMENT);
    }

    #[test]
    fn test_stax_self_closing() {
        let xml = r#"<root><br/></root>"#;
        let events = stax_parse_events(xml);
        assert_eq!(events.len(), 4); // start root, start br, end br, end root
        assert_eq!(events[1].name.as_deref(), Some("br"));
        assert_eq!(events[2].event_type, STAX_END_ELEMENT);
    }

    #[test]
    fn test_stax_cdata() {
        let xml = r#"<root><![CDATA[some data]]></root>"#;
        let events = stax_parse_events(xml);
        assert!(events
            .iter()
            .any(|e| e.event_type == STAX_CHARACTERS && e.text.as_deref() == Some("some data")));
    }

    #[test]
    fn test_xml_well_formedness_valid() {
        assert!(validate_xml_well_formedness("<root><child>text</child></root>").is_ok());
        assert!(validate_xml_well_formedness("<br/>").is_ok());
        assert!(validate_xml_well_formedness("<?xml version=\"1.0\"?><root/>").is_ok());
        assert!(validate_xml_well_formedness("<!-- comment --><root/>").is_ok());
        assert!(validate_xml_well_formedness("<root><![CDATA[data]]></root>").is_ok());
    }

    #[test]
    fn test_xml_well_formedness_invalid() {
        assert!(validate_xml_well_formedness("<root><child></root>").is_err());
        assert!(validate_xml_well_formedness("<root>").is_err());
        assert!(validate_xml_well_formedness("</root>").is_err());
        assert!(validate_xml_well_formedness("<root><a></b></root>").is_err());
    }

    #[test]
    fn test_jshell_evaluate_arithmetic() {
        assert_eq!(jshell_evaluate("1 + 1"), "2");
        assert_eq!(jshell_evaluate("3 * 4 + 2"), "14");
        assert_eq!(jshell_evaluate("(2 + 3) * 4"), "20");
    }

    #[test]
    fn test_jshell_evaluate_strings() {
        assert_eq!(jshell_evaluate("\"hello\""), "hello");
        assert_eq!(jshell_evaluate("\"hello\" + \" world\""), "hello world");
        assert_eq!(jshell_evaluate("\"value: \" + 42"), "value: 42");
    }

    #[test]
    fn test_jshell_evaluate_var_decl() {
        assert_eq!(jshell_evaluate("int x = 5"), "5");
        assert_eq!(jshell_evaluate("String s = \"hi\""), "hi");
        assert_eq!(jshell_evaluate("double d = 3.14"), "3.14");
        assert_eq!(jshell_evaluate("var x = 10"), "10");
    }

    #[test]
    fn test_jshell_evaluate_booleans() {
        assert_eq!(jshell_evaluate("true"), "true");
        assert_eq!(jshell_evaluate("false"), "false");
        assert_eq!(jshell_evaluate("3 > 2"), "true");
        assert_eq!(jshell_evaluate("1 >= 1"), "true");
        assert_eq!(jshell_evaluate("5 < 3"), "false");
        assert_eq!(jshell_evaluate("5 != 3"), "true");
    }

    #[test]
    fn test_jshell_evaluate_ternary() {
        assert_eq!(jshell_evaluate("true ? 1 : 2"), "1");
        assert_eq!(jshell_evaluate("false ? 1 : 2"), "2");
        assert_eq!(jshell_evaluate("3 > 2 ? 10 : 20"), "10");
    }
}

// ---------------------------------------------------------------------------
// gc-common w16-e: stale references
// ---------------------------------------------------------------------------
#[cfg(test)]
mod w16e_stale_reference_tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::error::MethodCallResult;

    thread_local! {
        static READ_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// `available()` answers 3; the first `read(byte[])` MOVES the buffer
    /// (fills a fresh array and re-points the native's pin at it) and answers
    /// 3; the second answers EOF.
    fn move_buffer_on_read(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match method_name {
            "available" => Some(Ok(Some(Value::Int(3)))),
            "read" => {
                let n = READ_CALLS.with(|c| {
                    c.set(c.get() + 1);
                    c.get()
                });
                if n > 1 {
                    return Some(Ok(Some(Value::Int(-1))));
                }
                let Some(Value::Object(Some(old))) = args.first().copied() else {
                    return Some(Ok(Some(Value::Int(-1))));
                };
                let len = ctx.array_length(old);
                let moved = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
                ctx.write_byte_array_from(moved, 0, b"abc");
                ctx.remap_native_pin_addr_for_test(old.as_ptr() as usize, moved.as_ptr() as usize);
                Some(Ok(Some(Value::Int(3))))
            }
            _ => None,
        }
    }

    /// The bytes a `read` wrote are copied out of the buffer's CURRENT copy.
    /// The old loop read them back through the address taken before the
    /// call, i.e. out of the vacated copy (here: zeros).
    #[test]
    fn stax_input_is_read_back_from_the_buffer_the_read_left() {
        READ_CALLS.with(|c| c.set(0));
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(0xE16_0201);
        ctx.set_invoke_virtual_hook(move_buffer_on_read);
        let stream = ctx.fresh_object_ref();
        let text = stax_read_input_stream(&mut ctx, stream);
        assert_eq!(text, "abc");
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }

    /// An empty binding store grows to hold the appended pair. The grow path
    /// doubled the capacity, which for an empty array is 0, and then stored
    /// the pair past its end.
    #[test]
    fn an_empty_jndi_binding_store_grows_to_hold_the_first_binding() {
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(0xE16_0202);
        let cid = cratonvm_types::ClassId::new(0);
        let this = ctx.alloc_object(cid, 1);
        let bindings = ctx.alloc_object(cid, 3);
        let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
        let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
        ctx.set_field(bindings, 0, Value::Object(Some(keys)));
        ctx.set_field(bindings, 1, Value::Object(Some(vals)));
        ctx.set_field(bindings, 2, Value::Int(0));
        ctx.set_field(this, 0, Value::Object(Some(bindings)));
        let name = ctx.create_string("java:comp/env");
        let value = ctx.fresh_object_ref();

        jndi_put_binding(
            &mut ctx,
            this,
            Value::Object(Some(name)),
            Value::Object(Some(value)),
        )
        .expect("the put must succeed");

        assert_eq!(ctx.get_field(bindings, 2), Value::Int(1));
        let Value::Object(Some(keys)) = ctx.get_field(bindings, 0) else {
            panic!("the grown key array must be published");
        };
        let Value::Object(Some(vals)) = ctx.get_field(bindings, 1) else {
            panic!("the grown value array must be published");
        };
        assert!(ctx.array_length(keys) >= 1);
        assert_eq!(ctx.get_array_element(keys, 0), Value::Object(Some(name)));
        assert_eq!(ctx.get_array_element(vals, 0), Value::Object(Some(value)));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }
}

/// gc-common w19-c (stale-handle triage): the rooted builders that replaced
/// the raw ones. The mock never moves an object at an allocation or a class
/// resolution, so these check the shapes and that every root is released; the
/// relocation itself is not expressible here.
#[cfg(test)]
mod w19c_stale_handle_tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn vm_ctx(vm: usize) -> MockNativeContext {
        let ctx = mock_ctx();
        ctx.set_vm_identity(vm);
        ctx
    }

    fn object(v: MethodCallResult, what: &str) -> ObjectRef {
        match v {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("{what} answered {other:?}"),
        }
    }

    fn native(
        registry: &NativeMethodRegistry,
        class: &str,
        name: &str,
        desc: &str,
    ) -> cratonvm_native_api::NativeCallback {
        registry
            .find(class, name, desc)
            .unwrap_or_else(|| panic!("{class}.{name}{desc} should be registered"))
    }

    fn assert_store(ctx: &MockNativeContext, store: ObjectRef, cap: usize) {
        for slot in 0..2 {
            let Value::Object(Some(arr)) = ctx.get_field(store, slot) else {
                panic!("store slot {slot} must hold an array");
            };
            assert_eq!(ctx.array_length(arr), cap);
        }
        assert_eq!(ctx.get_field(store, 2), Value::Int(0));
    }

    #[test]
    fn a_binding_store_is_complete_and_releases_its_root() {
        let mut ctx = vm_ctx(0xC19_0101);
        let store = t3_new_binding_store(&mut ctx, "java/util/HashMap", 16).unwrap();
        assert_store(&ctx, store, 16);
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    /// `new InitialContext(env)`: the store and the environment land in the
    /// receiver, and a bind / lookup round trip works through them.
    #[test]
    fn an_initial_context_keeps_its_store_and_environment() {
        let mut registry = NativeMethodRegistry::new();
        register_t38_jndi(&mut registry);
        let init = native(
            &registry,
            "javax/naming/InitialContext",
            "<init>",
            "(Ljava/util/Hashtable;)V",
        );
        let bind = native(
            &registry,
            "javax/naming/InitialContext",
            "bind",
            "(Ljava/lang/String;Ljava/lang/Object;)V",
        );
        let lookup = native(
            &registry,
            "javax/naming/InitialContext",
            "lookup",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        );
        let mut ctx = vm_ctx(0xC19_0102);
        let this = ctx.fresh_object_ref();
        let env = ctx.fresh_object_ref();
        init(&mut ctx, &[Value::Object(Some(this)), Value::Object(Some(env))]).unwrap();
        assert_eq!(ctx.native_pin_count_for_test(), 0);
        let Value::Object(Some(store)) = ctx.get_field(this, 0) else {
            panic!("the context must hold its binding store");
        };
        assert_store(&ctx, store, 16);
        assert_eq!(ctx.get_field(this, 1), Value::Object(Some(env)));

        let name = ctx.create_string("jdbc/ds");
        let value = ctx.fresh_object_ref();
        bind(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(name)),
                Value::Object(Some(value)),
            ],
        )
        .unwrap();
        let again = ctx.create_string("jdbc/ds");
        let args = [Value::Object(Some(this)), Value::Object(Some(again))];
        let found = object(lookup(&mut ctx, &args), "lookup");
        assert_eq!(found, value);
    }

    /// `LocateRegistry.createRegistry`, `ScriptEngineManager.getEngineByName`,
    /// `ScriptEngineManager.<init>` and `JShell.create`: each builder's shape.
    #[test]
    fn the_registry_engine_manager_and_shell_builders_are_complete() {
        let mut registry = NativeMethodRegistry::new();
        register_t38_jndi(&mut registry);
        register_t310_scripting(&mut registry);
        register_t312_tooling(&mut registry);
        let mut ctx = vm_ctx(0xC19_0103);

        let create = native(
            &registry,
            "java/rmi/registry/LocateRegistry",
            "createRegistry",
            "(I)Ljava/rmi/registry/Registry;",
        );
        let reg = object(create(&mut ctx, &[Value::Int(1234)]), "createRegistry");
        let Value::Object(Some(store)) = ctx.get_field(reg, 0) else {
            panic!("the registry must hold its binding store");
        };
        assert_store(&ctx, store, 16);
        assert_eq!(ctx.get_field(reg, 1), Value::Int(1234));
        assert_eq!(ctx.native_pin_count_for_test(), 0);

        let by_name = native(
            &registry,
            "javax/script/ScriptEngineManager",
            "getEngineByName",
            "(Ljava/lang/String;)Ljavax/script/ScriptEngine;",
        );
        let manager = ctx.fresh_object_ref();
        let js = ctx.create_string("js");
        let args = [Value::Object(Some(manager)), Value::Object(Some(js))];
        let engine = object(by_name(&mut ctx, &args), "getEngineByName");
        let Value::Object(Some(bindings)) = ctx.get_field(engine, 0) else {
            panic!("the engine must hold its bindings");
        };
        assert_store(&ctx, bindings, 16);
        let Value::Object(Some(engine_name)) = ctx.get_field(engine, 1) else {
            panic!("the engine must hold its name");
        };
        assert_eq!(ctx.read_string(engine_name).as_deref(), Some("js"));
        assert_eq!(ctx.native_pin_count_for_test(), 0);

        let manager_init = native(&registry, "javax/script/ScriptEngineManager", "<init>", "()V");
        manager_init(&mut ctx, &[Value::Object(Some(manager))]).unwrap();
        let Value::Object(Some(engines)) = ctx.get_field(manager, 0) else {
            panic!("the manager must hold its engine list");
        };
        let Value::Object(Some(arr)) = ctx.get_field(engines, 0) else {
            panic!("the engine list must hold its array");
        };
        assert_eq!(ctx.array_length(arr), 4);
        assert_eq!(ctx.get_field(engines, 1), Value::Int(0));
        assert_eq!(ctx.native_pin_count_for_test(), 0);

        let shell_create = native(&registry, "jdk/jshell/JShell", "create", "()Ljdk/jshell/JShell;");
        let shell = object(shell_create(&mut ctx, &[]), "JShell.create");
        let Value::Object(Some(history)) = ctx.get_field(shell, 0) else {
            panic!("the shell must hold its history list");
        };
        let Value::Object(Some(arr)) = ctx.get_field(history, 0) else {
            panic!("the history list must hold its array");
        };
        assert_eq!(ctx.array_length(arr), 32);
        assert_eq!(ctx.get_field(shell, 1), Value::Int(0));
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    /// `SnippetEvent.status()` reads the event's code before it allocates.
    #[test]
    fn a_snippet_status_carries_the_events_code() {
        let mut registry = NativeMethodRegistry::new();
        register_t312_tooling(&mut registry);
        let status = native(
            &registry,
            "jdk/jshell/SnippetEvent",
            "status",
            "()Ljdk/jshell/Snippet$Status;",
        );
        let mut ctx = vm_ctx(0xC19_0104);
        let event = ctx.fresh_object_ref();
        ctx.set_field(event, 2, Value::Int(7));
        let st = object(status(&mut ctx, &[Value::Object(Some(event))]), "status");
        let Value::Object(Some(name)) = ctx.get_field(st, 0) else {
            panic!("the status must carry its name");
        };
        assert_eq!(ctx.read_string(name).as_deref(), Some("VALID"));
        assert_eq!(ctx.get_field(st, 1), Value::Int(7));
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }
}
