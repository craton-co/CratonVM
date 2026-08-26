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

use crate::{try_alloc_concurrent_synthetic, obj_arg};
use cratonvm_native_api::NativeContext;
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::RuntimeError;
use cratonvm_types::{ObjectRef, Value};
use cratonvm_types::error::MethodCallFailed;

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
        // bindings = HashMap synthetic (keys=0, values=1, size=2)
        let bindings = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
        let keys_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let vals_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        ctx.set_field(bindings, 0, Value::Object(Some(keys_arr)));
        ctx.set_field(bindings, 1, Value::Object(Some(vals_arr)));
        ctx.set_field(bindings, 2, Value::Int(0));
        ctx.set_field(this, 0, Value::Object(Some(bindings)));
        ctx.set_field(this, 1, Value::Object(None)); // env
        Ok(None)
    });

    // <init>(Hashtable) — creates context with given environment
    r.register(ic, "<init>", "(Ljava/util/Hashtable;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let env = args.get(1).copied().unwrap_or(Value::Object(None));
        let bindings = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
        let keys_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let vals_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        ctx.set_field(bindings, 0, Value::Object(Some(keys_arr)));
        ctx.set_field(bindings, 1, Value::Object(Some(vals_arr)));
        ctx.set_field(bindings, 2, Value::Int(0));
        ctx.set_field(this, 0, Value::Object(Some(bindings)));
        ctx.set_field(this, 1, env);
        Ok(None)
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
            let registry = try_alloc_concurrent_synthetic(ctx, "java/rmi/registry/Registry", 2)?;
            let keys_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let vals_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let bindings = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            ctx.set_field(bindings, 0, Value::Object(Some(keys_arr)));
            ctx.set_field(bindings, 1, Value::Object(Some(vals_arr)));
            ctx.set_field(bindings, 2, Value::Int(0));
            ctx.set_field(registry, 0, Value::Object(Some(bindings)));
            ctx.set_field(registry, 1, Value::Int(_port));
            Ok(Some(Value::Object(Some(registry))))
        },
    );

    r.register(
        reg,
        "getRegistry",
        "(Ljava/lang/String;I)Ljava/rmi/registry/Registry;",
        |ctx, args| {
            // Return a synthetic registry pointing to the given host:port
            let registry = try_alloc_concurrent_synthetic(ctx, "java/rmi/registry/Registry", 2)?;
            let bindings = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            ctx.set_field(bindings, 0, Value::Object(Some(keys)));
            ctx.set_field(bindings, 1, Value::Object(Some(vals)));
            ctx.set_field(bindings, 2, Value::Int(0));
            ctx.set_field(registry, 0, Value::Object(Some(bindings)));
            let port = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 1099,
            };
            ctx.set_field(registry, 1, Value::Int(port));
            Ok(Some(Value::Object(Some(registry))))
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
        let this = obj_arg(args, 0)?;
        let bindings = match ctx.get_field(this, 0) {
            Value::Object(Some(b)) => b,
            _ => {
                let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                return Ok(Some(Value::Object(Some(empty))));
            }
        };
        let size = match ctx.get_field(bindings, 2) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        let keys_arr = match ctx.get_field(bindings, 0) {
            Value::Object(Some(a)) => a,
            _ => {
                let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                return Ok(Some(Value::Object(Some(empty))));
            }
        };
        let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
        for i in 0..size {
            ctx.set_array_element(result, i, ctx.get_array_element(keys_arr, i));
        }
        Ok(Some(Value::Object(Some(result))))
    });
    r.set_category(__prev_cat);
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
        let new_cap = cap * 2;
        let new_keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        let new_vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        for i in 0..size {
            ctx.set_array_element(new_keys, i, ctx.get_array_element(keys_arr, i));
            ctx.set_array_element(new_vals, i, ctx.get_array_element(vals_arr, i));
        }
        ctx.set_field(bindings, 0, Value::Object(Some(new_keys)));
        ctx.set_field(bindings, 1, Value::Object(Some(new_vals)));
        ctx.set_array_element(new_keys, size, name);
        ctx.set_array_element(new_vals, size, value);
    } else {
        ctx.set_array_element(keys_arr, size, name);
        ctx.set_array_element(vals_arr, size, value);
    }
    ctx.set_field(bindings, 2, Value::Int((size + 1) as i32));
    Ok(())
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
        |ctx, _args| {
            let factory = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/XMLInputFactory", 1)?;
            ctx.set_field(factory, 0, Value::Int(0)); // configuration flags
            Ok(Some(Value::Object(Some(factory))))
        },
    );
    r.register(
        xif,
        "newFactory",
        "()Ljavax/xml/stream/XMLInputFactory;",
        |ctx, _args| {
            let factory = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/XMLInputFactory", 1)?;
            ctx.set_field(factory, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(factory))))
        },
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
        |ctx, _args| {
            let factory = try_alloc_concurrent_synthetic(ctx, "javax/xml/validation/SchemaFactory", 1)?;
            ctx.set_field(factory, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(factory))))
        },
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
        for i in 0..n as usize {
            match ctx.get_array_element(buf, i) {
                Value::Int(b) => result.push(b as u8),
                _ => {}
            }
        }
    }
    String::from_utf8(result).unwrap_or_default()
}

/// Parse XML into StAX events and create an XMLEventReader.
fn stax_create_event_reader(
    ctx: &mut dyn NativeContext,
    xml: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let events = stax_parse_events(xml);
    let event_count = events.len();
    let events_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, event_count + 2);

    // START_DOCUMENT event
    let start_doc = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/events/XMLEvent", 3)?;
    ctx.set_field(start_doc, 0, Value::Int(STAX_START_DOCUMENT));
    ctx.set_array_element(events_arr, 0, Value::Object(Some(start_doc)));

    for (i, ev) in events.iter().enumerate() {
        let event_obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/events/XMLEvent", 3)?;
        ctx.set_field(event_obj, 0, Value::Int(ev.event_type));
        if let Some(ref name) = ev.name {
            let s = ctx.create_string(name);
            ctx.set_field(event_obj, 1, Value::Object(Some(s)));
        }
        if let Some(ref text) = ev.text {
            let s = ctx.create_string(text);
            ctx.set_field(event_obj, 2, Value::Object(Some(s)));
        }
        ctx.set_array_element(events_arr, i + 1, Value::Object(Some(event_obj)));
    }

    // END_DOCUMENT event
    let end_doc = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/events/XMLEvent", 3)?;
    ctx.set_field(end_doc, 0, Value::Int(STAX_END_DOCUMENT));
    ctx.set_array_element(events_arr, event_count + 1, Value::Object(Some(end_doc)));

    let total = (event_count + 2) as i32;
    let reader = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/XMLEventReader", 3)?;
    ctx.set_field(reader, 0, Value::Object(Some(events_arr)));
    ctx.set_field(reader, 1, Value::Int(total));
    ctx.set_field(reader, 2, Value::Int(0));
    Ok(Some(Value::Object(Some(reader))))
}

/// Create an XMLStreamReader from parsed events.
fn stax_create_stream_reader(
    ctx: &mut dyn NativeContext,
    xml: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let events = stax_parse_events(xml);
    let event_count = events.len();
    let events_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, event_count + 2);

    let start_doc = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/events/XMLEvent", 3)?;
    ctx.set_field(start_doc, 0, Value::Int(STAX_START_DOCUMENT));
    ctx.set_array_element(events_arr, 0, Value::Object(Some(start_doc)));

    for (i, ev) in events.iter().enumerate() {
        let event_obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/events/XMLEvent", 3)?;
        ctx.set_field(event_obj, 0, Value::Int(ev.event_type));
        if let Some(ref name) = ev.name {
            let s = ctx.create_string(name);
            ctx.set_field(event_obj, 1, Value::Object(Some(s)));
        }
        if let Some(ref text) = ev.text {
            let s = ctx.create_string(text);
            ctx.set_field(event_obj, 2, Value::Object(Some(s)));
        }
        ctx.set_array_element(events_arr, i + 1, Value::Object(Some(event_obj)));
    }

    let end_doc = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/events/XMLEvent", 3)?;
    ctx.set_field(end_doc, 0, Value::Int(STAX_END_DOCUMENT));
    ctx.set_array_element(events_arr, event_count + 1, Value::Object(Some(end_doc)));

    let total = (event_count + 2) as i32;
    let reader = try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/XMLStreamReader", 4)?;
    ctx.set_field(reader, 0, Value::Object(Some(events_arr)));
    ctx.set_field(reader, 1, Value::Int(total));
    ctx.set_field(reader, 2, Value::Int(0));
    ctx.set_field(reader, 3, Value::Int(STAX_START_DOCUMENT));
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
        let this = obj_arg(args, 0)?;
        let engines = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_field(engines, 0, Value::Object(Some(arr)));
        ctx.set_field(engines, 1, Value::Int(0));
        ctx.set_field(this, 0, Value::Object(Some(engines)));
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
                let engine = try_alloc_concurrent_synthetic(ctx, "javax/script/ScriptEngine", 2)?;
                // Fields: 0=bindings_map, 1=engine_name
                let bindings = try_alloc_concurrent_synthetic(ctx, "javax/script/SimpleBindings", 3)?;
                let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
                let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
                ctx.set_field(bindings, 0, Value::Object(Some(keys)));
                ctx.set_field(bindings, 1, Value::Object(Some(vals)));
                ctx.set_field(bindings, 2, Value::Int(0));
                ctx.set_field(engine, 0, Value::Object(Some(bindings)));
                let name_s = ctx.create_string(&name);
                ctx.set_field(engine, 1, Value::Object(Some(name_s)));
                return Ok(Some(Value::Object(Some(engine))));
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
                let engine = try_alloc_concurrent_synthetic(ctx, "javax/script/ScriptEngine", 2)?;
                let bindings = try_alloc_concurrent_synthetic(ctx, "javax/script/SimpleBindings", 3)?;
                let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
                let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
                ctx.set_field(bindings, 0, Value::Object(Some(keys)));
                ctx.set_field(bindings, 1, Value::Object(Some(vals)));
                ctx.set_field(bindings, 2, Value::Int(0));
                ctx.set_field(engine, 0, Value::Object(Some(bindings)));
                let name_s = ctx.create_string("javascript");
                ctx.set_field(engine, 1, Value::Object(Some(name_s)));
                return Ok(Some(Value::Object(Some(engine))));
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
            let bindings = try_alloc_concurrent_synthetic(ctx, "javax/script/SimpleBindings", 3)?;
            let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            ctx.set_field(bindings, 0, Value::Object(Some(keys)));
            ctx.set_field(bindings, 1, Value::Object(Some(vals)));
            ctx.set_field(bindings, 2, Value::Int(0));
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
            let map = try_alloc_concurrent_synthetic(ctx, "java/util/TreeMap", 3)?;
            let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, charsets.len());
            let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, charsets.len());
            for (i, name) in charsets.iter().enumerate() {
                let key = ctx.create_string(name);
                let charset = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 2)?;
                let name_s = ctx.create_string(name);
                ctx.set_field(charset, 0, Value::Object(Some(name_s)));
                ctx.set_field(charset, 1, Value::Object(None)); // aliases
                ctx.set_array_element(keys, i, Value::Object(Some(key)));
                ctx.set_array_element(vals, i, Value::Object(Some(charset)));
            }
            ctx.set_field(map, 0, Value::Object(Some(keys)));
            ctx.set_field(map, 1, Value::Object(Some(vals)));
            ctx.set_field(map, 2, Value::Int(charsets.len() as i32));
            Ok(Some(Value::Object(Some(map))))
        },
    );

    // String.getBytes(String charsetName) — extended encoding support
    // This is registered elsewhere for UTF-8/ISO-8859-1; we add Shift_JIS support
    // The encoding/decoding for exotic charsets is best-effort.
    r.set_category(__prev_cat);
    ()
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
            let compiler = try_alloc_concurrent_synthetic(ctx, "javax/tools/JavaCompiler", 2)?;
            let name = ctx.create_string("cratonvm-javac");
            ctx.set_field(compiler, 0, Value::Object(Some(name)));
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
        let fm = try_alloc_concurrent_synthetic(ctx, "javax/tools/StandardJavaFileManager", 2)?;
        let name = ctx.create_string("cratonvm-filemanager");
        ctx.set_field(fm, 0, Value::Object(Some(name)));
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
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            ctx.set_field(list, 0, Value::Object(Some(arr)));
            ctx.set_field(list, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(list))))
        },
    );

    // --- T3.13: JShell ---
    let jshell = "jdk/jshell/JShell";
    r.register(jshell, "create", "()Ljdk/jshell/JShell;", |ctx, _args| {
        // Fields: 0=history_list, 1=variable_count
        let shell = try_alloc_concurrent_synthetic(ctx, "jdk/jshell/JShell", 2)?;
        let history = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 32);
        ctx.set_field(history, 0, Value::Object(Some(arr)));
        ctx.set_field(history, 1, Value::Int(0));
        ctx.set_field(shell, 0, Value::Object(Some(history)));
        ctx.set_field(shell, 1, Value::Int(0));
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

            // Create a SnippetEvent
            let event = try_alloc_concurrent_synthetic(ctx, "jdk/jshell/SnippetEvent", 3)?;
            let src = ctx.create_string(&source);
            let val = ctx.create_string(&result_str);
            ctx.set_field(event, 0, Value::Object(Some(src))); // source
            ctx.set_field(event, 1, Value::Object(Some(val))); // value
            ctx.set_field(event, 2, Value::Int(0)); // status (0=VALID)

            // Wrap in a single-element list
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(event)));
            ctx.set_field(list, 0, Value::Object(Some(arr)));
            ctx.set_field(list, 1, Value::Int(1));
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
            // Return a Status enum
            let status = try_alloc_concurrent_synthetic(ctx, "jdk/jshell/Snippet$Status", 2)?;
            let name = ctx.create_string("VALID");
            ctx.set_field(status, 0, Value::Object(Some(name)));
            ctx.set_field(status, 1, ctx.get_field(this, 2));
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

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
