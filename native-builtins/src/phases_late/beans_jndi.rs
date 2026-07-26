// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.beans`, `java.util.prefs` and `javax.naming` natives: PropertyChange support, Introspector, Preferences, JNDI stubs.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// java.util.prefs.Preferences — 2-field synthetic (data=0 HashMap, name=1 String)
// =============================================================================

pub(crate) fn p72_alloc_prefs(ctx: &mut dyn NativeContext) -> ObjectRef {
    let prefs = alloc_concurrent_synthetic(ctx, "java/util/prefs/Preferences", 2);
    // Pin across the map/string allocs below — a moving young GC there would
    // relocate them (native stale-local family).
    let prefs_pin = ctx.pin_native_root(prefs);
    let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
    let map_pin = ctx.pin_native_root(map);
    cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
    let prefs = ctx.read_native_pin(prefs_pin, prefs);
    let map = ctx.read_native_pin(map_pin, map);
    ctx.set_field(prefs, 0, Value::Object(Some(map)));
    let name_str = ctx.create_string("");
    let prefs = ctx.read_native_pin(prefs_pin, prefs);
    ctx.set_field(prefs, 1, Value::Object(Some(name_str)));
    ctx.unpin_native_roots(prefs_pin);
    prefs
}

pub(crate) fn p72_prefs_map(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    match ctx.get_field(this, 0) {
        Value::Object(Some(m)) => m,
        _ => {
            // Pin across the map alloc/init below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
            let map_pin = ctx.pin_native_root(map);
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
            let this = ctx.read_native_pin(this_pin, this);
            let map = ctx.read_native_pin(map_pin, map);
            ctx.set_field(this, 0, Value::Object(Some(map)));
            ctx.unpin_native_roots(this_pin);
            map
        }
    }
}

pub(crate) fn register_p72_preferences(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pref = "java/util/prefs/Preferences";
    let abs_pref = "java/util/prefs/AbstractPreferences";

    // Static factory methods
    for cls in [pref, abs_pref] {
        r.register(
            cls,
            "userRoot",
            "()Ljava/util/prefs/Preferences;",
            |ctx, _args| {
                let p = p72_alloc_prefs(ctx);
                Ok(Some(Value::Object(Some(p))))
            },
        );
        r.register(
            cls,
            "systemRoot",
            "()Ljava/util/prefs/Preferences;",
            |ctx, _args| {
                let p = p72_alloc_prefs(ctx);
                Ok(Some(Value::Object(Some(p))))
            },
        );
        r.register(
            cls,
            "userNodeForPackage",
            "(Ljava/lang/Class;)Ljava/util/prefs/Preferences;",
            |ctx, _args| {
                let p = p72_alloc_prefs(ctx);
                Ok(Some(Value::Object(Some(p))))
            },
        );
        r.register(
            cls,
            "systemNodeForPackage",
            "(Ljava/lang/Class;)Ljava/util/prefs/Preferences;",
            |ctx, _args| {
                let p = p72_alloc_prefs(ctx);
                Ok(Some(Value::Object(Some(p))))
            },
        );
        r.register(cls, "<init>", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Pin across the map/string allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
            let map_pin = ctx.pin_native_root(map);
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
            let this = ctx.read_native_pin(this_pin, this);
            let map = ctx.read_native_pin(map_pin, map);
            ctx.set_field(this, 0, Value::Object(Some(map)));
            let name_str = ctx.create_string("");
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, 1, Value::Object(Some(name_str)));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        });
        r.register(
            cls,
            "node",
            "(Ljava/lang/String;)Ljava/util/prefs/Preferences;",
            |ctx, args| {
                let name_val = args.get(1).copied().unwrap_or(Value::Object(None));
                // Pin across the prefs alloc below — a moving young GC there
                // would relocate it (native stale-local family).
                let name_pin = pinned_object_value(ctx, name_val);
                let p = p72_alloc_prefs(ctx);
                let name_val = read_pinned_object_value(ctx, name_pin, name_val);
                ctx.set_field(p, 1, name_val);
                if let Some((h, _)) = name_pin {
                    ctx.unpin_native_roots(h);
                }
                Ok(Some(Value::Object(Some(p))))
            },
        );
        r.register(
            cls,
            "get",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let def = args.get(2).copied().unwrap_or(Value::Object(None));
                let map = p72_prefs_map(ctx, this);
                let result = cratonvm_native_collections::native_map_get_pub(
                    ctx,
                    &[Value::Object(Some(map)), key],
                )?;
                match result {
                    Some(Value::Object(Some(_))) => Ok(result),
                    _ => Ok(Some(def)),
                }
            },
        );
        r.register(
            cls,
            "put",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let val = args.get(2).copied().unwrap_or(Value::Object(None));
                let map = p72_prefs_map(ctx, this);
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[Value::Object(Some(map)), key, val],
                )?;
                Ok(None)
            },
        );
        r.register(cls, "getInt", "(Ljava/lang/String;I)I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let def = args.get(2).copied().unwrap_or(Value::Int(0));
            let map = p72_prefs_map(ctx, this);
            let result = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )?;
            let val = match result {
                Some(Value::Object(Some(s))) => {
                    let txt = ctx.read_string(s).unwrap_or_default();
                    txt.parse::<i32>().map(Value::Int).unwrap_or(def)
                }
                _ => def,
            };
            Ok(Some(val))
        });
        r.register(cls, "putInt", "(Ljava/lang/String;I)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let v = match args.get(2) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            let map = p72_prefs_map(ctx, this);
            let sv = ctx.create_string(&v.to_string());
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[Value::Object(Some(map)), key, Value::Object(Some(sv))],
            )?;
            Ok(None)
        });
        r.register(cls, "getBoolean", "(Ljava/lang/String;Z)Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let def = args.get(2).copied().unwrap_or(Value::Int(0));
            let map = p72_prefs_map(ctx, this);
            let result = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )?;
            let val = match result {
                Some(Value::Object(Some(s))) => {
                    let txt = ctx.read_string(s).unwrap_or_default();
                    Value::Int(if txt.eq_ignore_ascii_case("true") {
                        1
                    } else {
                        0
                    })
                }
                _ => def,
            };
            Ok(Some(val))
        });
        r.register(cls, "putBoolean", "(Ljava/lang/String;Z)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let v = match args.get(2) {
                Some(Value::Int(i)) => *i != 0,
                _ => false,
            };
            let map = p72_prefs_map(ctx, this);
            let sv = ctx.create_string(if v { "true" } else { "false" });
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[Value::Object(Some(map)), key, Value::Object(Some(sv))],
            )?;
            Ok(None)
        });
        r.register(cls, "getLong", "(Ljava/lang/String;J)J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let def = args.get(2).copied().unwrap_or(Value::Long(0));
            let map = p72_prefs_map(ctx, this);
            let result = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )?;
            let val = match result {
                Some(Value::Object(Some(s))) => {
                    let txt = ctx.read_string(s).unwrap_or_default();
                    txt.parse::<i64>().map(Value::Long).unwrap_or(def)
                }
                _ => def,
            };
            Ok(Some(val))
        });
        r.register(cls, "putLong", "(Ljava/lang/String;J)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let v = match args.get(2) {
                Some(Value::Long(l)) => *l,
                _ => 0,
            };
            let map = p72_prefs_map(ctx, this);
            let sv = ctx.create_string(&v.to_string());
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[Value::Object(Some(map)), key, Value::Object(Some(sv))],
            )?;
            Ok(None)
        });
        r.register(cls, "getDouble", "(Ljava/lang/String;D)D", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let def = args.get(2).copied().unwrap_or(Value::Double(0.0));
            let map = p72_prefs_map(ctx, this);
            let result = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )?;
            let val = match result {
                Some(Value::Object(Some(s))) => {
                    let txt = ctx.read_string(s).unwrap_or_default();
                    txt.parse::<f64>().map(Value::Double).unwrap_or(def)
                }
                _ => def,
            };
            Ok(Some(val))
        });
        r.register(cls, "putDouble", "(Ljava/lang/String;D)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let v = match args.get(2) {
                Some(Value::Double(d)) => *d,
                _ => 0.0,
            };
            let map = p72_prefs_map(ctx, this);
            let sv = ctx.create_string(&v.to_string());
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[Value::Object(Some(map)), key, Value::Object(Some(sv))],
            )?;
            Ok(None)
        });
        r.register(cls, "getFloat", "(Ljava/lang/String;F)F", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let def = args.get(2).copied().unwrap_or(Value::Float(0.0));
            let map = p72_prefs_map(ctx, this);
            let result = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )?;
            let val = match result {
                Some(Value::Object(Some(s))) => {
                    let txt = ctx.read_string(s).unwrap_or_default();
                    txt.parse::<f32>().map(Value::Float).unwrap_or(def)
                }
                _ => def,
            };
            Ok(Some(val))
        });
        r.register(cls, "putFloat", "(Ljava/lang/String;F)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let v = match args.get(2) {
                Some(Value::Float(f)) => *f,
                _ => 0.0,
            };
            let map = p72_prefs_map(ctx, this);
            let sv = ctx.create_string(&v.to_string());
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[Value::Object(Some(map)), key, Value::Object(Some(sv))],
            )?;
            Ok(None)
        });
        r.register(cls, "remove", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let map = p72_prefs_map(ctx, this);
            cratonvm_native_collections::native_map_remove_pub(
                ctx,
                &[Value::Object(Some(map)), key],
            )?;
            Ok(None)
        });
        r.register(cls, "clear", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = p72_prefs_map(ctx, this);
            cratonvm_native_collections::native_map_clear_pub(ctx, &[Value::Object(Some(map))])?;
            Ok(None)
        });
        r.register(cls, "sync", "()V", |_ctx, _args| {
            // Preferences.sync() forces backing-store synchronization. Our impl uses an
            // in-memory HashMap (no disk backing), so there's nothing to sync.
            Ok(None)
        });
        r.register(cls, "flush", "()V", |_ctx, _args| {
            // Same — no backing store to flush.
            Ok(None)
        });
        r.register(cls, "keys", "()[Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = p72_prefs_map(ctx, this);
            let keys_result = cratonvm_native_collections::native_map_key_set_pub(
                ctx,
                &[Value::Object(Some(map))],
            )?;
            Ok(Some(keys_result.unwrap_or(Value::Object(None))))
        });
        r.register(cls, "name", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        });
        r.register(cls, "absolutePath", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        });
        r.register(
            cls,
            "parent",
            "()Ljava/util/prefs/Preferences;",
            |_ctx, _args| Ok(Some(Value::Object(None))),
        );
        r.register(cls, "nodeExists", "(Ljava/lang/String;)Z", |_ctx, _args| {
            Ok(Some(Value::Int(0)))
        });
        r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name_val = ctx.get_field(this, 1);
            let name = if let Value::Object(Some(s)) = name_val {
                ctx.read_string(s).unwrap_or_default()
            } else {
                String::new()
            };
            let s = ctx.create_string(&format!("Preferences[{}]", name));
            Ok(Some(Value::Object(Some(s))))
        });
    }
    r.set_category(__prev_cat);
}

// =============================================================================
// java.beans — PropertyChangeEvent, PropertyChangeSupport, VetoableChange, Introspector
// =============================================================================

/// Box a primitive int as java.lang.Integer (1-field synthetic with int value at field 0).
pub(crate) fn pcs_box_int(ctx: &mut dyn NativeContext, v: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1);
    ctx.set_field(obj, 0, Value::Int(v));
    obj
}

/// Box a primitive bool as java.lang.Boolean (1-field synthetic with int 0/1 at field 0).
pub(crate) fn pcs_box_bool(ctx: &mut dyn NativeContext, v: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/lang/Boolean", 1);
    ctx.set_field(obj, 0, Value::Int(if v != 0 { 1 } else { 0 }));
    obj
}

/// Compare two Value variants for equality. Used by PropertyChangeSupport to short-circuit
/// no-change events. For Object refs, compares wrapper field 0 if both have ≥1 field.
pub(crate) fn pcs_values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(x)), Value::Object(Some(y))) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        _ => false,
    }
}

/// Dispatch a PropertyChangeEvent to all listeners stored in PCS field 1 (ArrayList).
pub(crate) fn pcs_dispatch(ctx: &mut dyn NativeContext, pcs_this: ObjectRef, event: ObjectRef) {
    let listeners = match ctx.get_field(pcs_this, 1) {
        Value::Object(Some(l)) => l,
        _ => return,
    };
    // Pin across the listener callbacks below — a moving young GC there would
    // relocate `listeners`/`event` (native stale-local family).
    let listeners_pin = ctx.pin_native_root(listeners);
    let event_pin = ctx.pin_native_root(event);
    // Get listener count
    let size =
        match cratonvm_native_collections::native_al_size(ctx, &[Value::Object(Some(listeners))]) {
            Ok(Some(Value::Int(n))) => n,
            _ => 0,
        };
    for i in 0..size {
        let listeners = ctx.read_native_pin(listeners_pin, listeners);
        let listener_val = match cratonvm_native_collections::native_al_get(
            ctx,
            &[Value::Object(Some(listeners)), Value::Int(i)],
        ) {
            Ok(Some(v)) => v,
            _ => continue,
        };
        if let Value::Object(Some(listener)) = listener_val {
            let event = ctx.read_native_pin(event_pin, event);
            // Best-effort dispatch — ignore individual listener errors so all listeners get notified.
            let _ = ctx.invoke_virtual(
                listener,
                "propertyChange",
                "(Ljava/beans/PropertyChangeEvent;)V",
                &[Value::Object(Some(event))],
            );
        }
    }
    ctx.unpin_native_roots(listeners_pin);
}

pub(crate) fn register_p72_beans(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // PropertyChangeEvent = 4-field (source=0, propertyName=1, oldValue=2, newValue=3)
    let pce = "java/beans/PropertyChangeEvent";
    r.register(
        pce,
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 2, args.get(3).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 3, args.get(4).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(pce, "getSource", "()Ljava/lang/Object;", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 0)))
    });
    r.register(
        pce,
        "getPropertyName",
        "()Ljava/lang/String;",
        |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, 1))),
    );
    r.register(pce, "getOldValue", "()Ljava/lang/Object;", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 2)))
    });
    r.register(pce, "getNewValue", "()Ljava/lang/Object;", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 3)))
    });
    r.register(pce, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let prop = if let Value::Object(Some(s)) = ctx.get_field(this, 1) {
            ctx.read_string(s).unwrap_or_default()
        } else {
            String::new()
        };
        let s = ctx.create_string(&format!("PropertyChangeEvent[{}]", prop));
        Ok(Some(Value::Object(Some(s))))
    });

    // PropertyChangeSupport = 2-field (source=0, listeners=1 ArrayList)
    let pcs = "java/beans/PropertyChangeSupport";
    r.register(pcs, "<init>", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Pin across the list alloc/init below — a moving young GC there would
        // relocate `this` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let lst = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
        let lst_pin = ctx.pin_native_root(lst);
        cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(lst))]).ok();
        let this = ctx.read_native_pin(this_pin, this);
        let lst = ctx.read_native_pin(lst_pin, lst);
        ctx.set_field(this, 1, Value::Object(Some(lst)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(
        pcs,
        "addPropertyChangeListener",
        "(Ljava/beans/PropertyChangeListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
                cratonvm_native_collections::native_al_add(
                    ctx,
                    &[Value::Object(Some(lst)), listener],
                )
                .ok();
            }
            Ok(None)
        },
    );
    r.register(
        pcs,
        "addPropertyChangeListener",
        "(Ljava/lang/String;Ljava/beans/PropertyChangeListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = args.get(2).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
                cratonvm_native_collections::native_al_add(
                    ctx,
                    &[Value::Object(Some(lst)), listener],
                )
                .ok();
            }
            Ok(None)
        },
    );
    r.register(
        pcs,
        "removePropertyChangeListener",
        "(Ljava/beans/PropertyChangeListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
                cratonvm_native_collections::native_al_remove_obj(
                    ctx,
                    &[Value::Object(Some(lst)), listener],
                )
                .ok();
            }
            Ok(None)
        },
    );
    r.register(
        pcs,
        "removePropertyChangeListener",
        "(Ljava/lang/String;Ljava/beans/PropertyChangeListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // We don't track per-property listener subsets, so just remove from the main list.
            let listener = args.get(2).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
                cratonvm_native_collections::native_al_remove_obj(
                    ctx,
                    &[Value::Object(Some(lst)), listener],
                )
                .ok();
            }
            Ok(None)
        },
    );
    r.register(
        pcs,
        "getPropertyChangeListeners",
        "()[Ljava/beans/PropertyChangeListener;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
                cratonvm_native_collections::native_al_to_array(ctx, &[Value::Object(Some(lst))])
            } else {
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                Ok(Some(Value::Object(Some(arr))))
            }
        },
    );
    r.register(
        pcs,
        "getPropertyChangeListeners",
        "(Ljava/lang/String;)[Ljava/beans/PropertyChangeListener;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        pcs,
        "firePropertyChange",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prop_name = args.get(1).copied().unwrap_or(Value::Object(None));
            let old_val = args.get(2).copied().unwrap_or(Value::Object(None));
            let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
            // Short-circuit: skip dispatch when oldValue == newValue (matches JDK semantics).
            if pcs_values_equal(&old_val, &new_val) {
                return Ok(None);
            }
            // Build a PropertyChangeEvent (4-field: source, propertyName, oldValue, newValue)
            // Pin across the event alloc below — a moving young GC there would
            // relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let prop_pin = pinned_object_value(ctx, prop_name);
            let old_pin = pinned_object_value(ctx, old_val);
            let new_pin = pinned_object_value(ctx, new_val);
            let source = ctx.get_field(this, 0);
            let source_pin = pinned_object_value(ctx, source);
            let event = alloc_concurrent_synthetic(ctx, "java/beans/PropertyChangeEvent", 4);
            ctx.set_field(event, 0, read_pinned_object_value(ctx, source_pin, source));
            ctx.set_field(event, 1, read_pinned_object_value(ctx, prop_pin, prop_name));
            ctx.set_field(event, 2, read_pinned_object_value(ctx, old_pin, old_val));
            ctx.set_field(event, 3, read_pinned_object_value(ctx, new_pin, new_val));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            pcs_dispatch(ctx, this, event);
            Ok(None)
        },
    );
    r.register(
        pcs,
        "firePropertyChange",
        "(Ljava/lang/String;ILjava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prop_name = args.get(1).copied().unwrap_or(Value::Object(None));
            let old_int = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
            // Pin across the box/event allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let prop_pin = pinned_object_value(ctx, prop_name);
            let new_pin = pinned_object_value(ctx, new_val);
            let old_box = pcs_box_int(ctx, old_int);
            let old_box_pin = ctx.pin_native_root(old_box);
            let new_val = read_pinned_object_value(ctx, new_pin, new_val);
            if pcs_values_equal(&Value::Object(Some(old_box)), &new_val) {
                ctx.unpin_native_roots(this_pin);
                return Ok(None);
            }
            let this_cur = ctx.read_native_pin(this_pin, this);
            let source = ctx.get_field(this_cur, 0);
            let source_pin = pinned_object_value(ctx, source);
            let event = alloc_concurrent_synthetic(ctx, "java/beans/PropertyChangeEvent", 4);
            ctx.set_field(event, 0, read_pinned_object_value(ctx, source_pin, source));
            ctx.set_field(event, 1, read_pinned_object_value(ctx, prop_pin, prop_name));
            let old_box = ctx.read_native_pin(old_box_pin, old_box);
            ctx.set_field(event, 2, Value::Object(Some(old_box)));
            ctx.set_field(event, 3, read_pinned_object_value(ctx, new_pin, new_val));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            pcs_dispatch(ctx, this, event);
            Ok(None)
        },
    );
    r.register(
        pcs,
        "firePropertyChange",
        "(Ljava/lang/String;ZZ)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prop_name = args.get(1).copied().unwrap_or(Value::Object(None));
            let old_b = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let new_b = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            if old_b == new_b {
                return Ok(None);
            }
            // Pin across the box/event allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let prop_pin = pinned_object_value(ctx, prop_name);
            let old_box = pcs_box_bool(ctx, old_b);
            let old_box_pin = ctx.pin_native_root(old_box);
            let new_box = pcs_box_bool(ctx, new_b);
            let new_box_pin = ctx.pin_native_root(new_box);
            let this_cur = ctx.read_native_pin(this_pin, this);
            let source = ctx.get_field(this_cur, 0);
            let source_pin = pinned_object_value(ctx, source);
            let event = alloc_concurrent_synthetic(ctx, "java/beans/PropertyChangeEvent", 4);
            ctx.set_field(event, 0, read_pinned_object_value(ctx, source_pin, source));
            ctx.set_field(event, 1, read_pinned_object_value(ctx, prop_pin, prop_name));
            let old_box = ctx.read_native_pin(old_box_pin, old_box);
            let new_box = ctx.read_native_pin(new_box_pin, new_box);
            ctx.set_field(event, 2, Value::Object(Some(old_box)));
            ctx.set_field(event, 3, Value::Object(Some(new_box)));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            pcs_dispatch(ctx, this, event);
            Ok(None)
        },
    );
    r.register(
        pcs,
        "firePropertyChange",
        "(Ljava/lang/String;II)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prop_name = args.get(1).copied().unwrap_or(Value::Object(None));
            let old_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let new_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            if old_i == new_i {
                return Ok(None);
            }
            // Pin across the box/event allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let prop_pin = pinned_object_value(ctx, prop_name);
            let old_box = pcs_box_int(ctx, old_i);
            let old_box_pin = ctx.pin_native_root(old_box);
            let new_box = pcs_box_int(ctx, new_i);
            let new_box_pin = ctx.pin_native_root(new_box);
            let this_cur = ctx.read_native_pin(this_pin, this);
            let source = ctx.get_field(this_cur, 0);
            let source_pin = pinned_object_value(ctx, source);
            let event = alloc_concurrent_synthetic(ctx, "java/beans/PropertyChangeEvent", 4);
            ctx.set_field(event, 0, read_pinned_object_value(ctx, source_pin, source));
            ctx.set_field(event, 1, read_pinned_object_value(ctx, prop_pin, prop_name));
            let old_box = ctx.read_native_pin(old_box_pin, old_box);
            let new_box = ctx.read_native_pin(new_box_pin, new_box);
            ctx.set_field(event, 2, Value::Object(Some(old_box)));
            ctx.set_field(event, 3, Value::Object(Some(new_box)));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            pcs_dispatch(ctx, this, event);
            Ok(None)
        },
    );
    r.register(
        pcs,
        "firePropertyChange",
        "(Ljava/beans/PropertyChangeEvent;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let event = match args.get(1) {
                Some(Value::Object(Some(e))) => *e,
                _ => return Ok(None),
            };
            // Check oldValue/newValue equality via the event's fields
            let old_v = ctx.get_field(event, 2);
            let new_v = ctx.get_field(event, 3);
            if pcs_values_equal(&old_v, &new_v) {
                return Ok(None);
            }
            pcs_dispatch(ctx, this, event);
            Ok(None)
        },
    );
    r.register(pcs, "hasListeners", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
            match ctx.get_field(lst, 1) {
                Value::Int(n) => n > 0,
                _ => false,
            }
        } else {
            false
        };
        Ok(Some(Value::Int(if has { 1 } else { 0 })))
    });

    // PropertyChangeListener interface
    r.register(
        "java/beans/PropertyChangeListener",
        "propertyChange",
        "(Ljava/beans/PropertyChangeEvent;)V",
        native_noop_with_this,
    );

    // VetoableChangeListener interface
    r.register(
        "java/beans/VetoableChangeListener",
        "vetoableChange",
        "(Ljava/beans/PropertyChangeEvent;)V",
        native_noop_with_this,
    );

    // VetoableChangeSupport = 2-field (source=0, listeners=1)
    let vcs = "java/beans/VetoableChangeSupport";
    r.register(vcs, "<init>", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Pin across the list alloc/init below — a moving young GC there would
        // relocate `this` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let lst = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
        let lst_pin = ctx.pin_native_root(lst);
        cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(lst))]).ok();
        let this = ctx.read_native_pin(this_pin, this);
        let lst = ctx.read_native_pin(lst_pin, lst);
        ctx.set_field(this, 1, Value::Object(Some(lst)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(
        vcs,
        "addVetoableChangeListener",
        "(Ljava/beans/VetoableChangeListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
                cratonvm_native_collections::native_al_add(
                    ctx,
                    &[Value::Object(Some(lst)), listener],
                )
                .ok();
            }
            Ok(None)
        },
    );
    r.register(
        vcs,
        "removeVetoableChangeListener",
        "(Ljava/beans/VetoableChangeListener;)V",
        native_noop_with_this,
    );
    r.register(
        vcs,
        "fireVetoableChange",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        native_noop_with_this,
    );
    r.register(
        vcs,
        "fireVetoableChange",
        "(Ljava/lang/String;II)V",
        native_noop_with_this,
    );
    r.register(
        vcs,
        "fireVetoableChange",
        "(Ljava/lang/String;ZZ)V",
        native_noop_with_this,
    );
    r.register(vcs, "hasListeners", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = if let Value::Object(Some(lst)) = ctx.get_field(this, 1) {
            match ctx.get_field(lst, 1) {
                Value::Int(n) => n > 0,
                _ => false,
            }
        } else {
            false
        };
        Ok(Some(Value::Int(if has { 1 } else { 0 })))
    });

    // Introspector
    let intro = "java/beans/Introspector";
    r.register(
        intro,
        "getBeanInfo",
        "(Ljava/lang/Class;)Ljava/beans/BeanInfo;",
        introspector_get_bean_info,
    );
    r.register(
        intro,
        "getBeanInfo",
        "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/beans/BeanInfo;",
        introspector_get_bean_info,
    );
    r.register(intro, "flushCaches", "()V", |_ctx, _args| {
        // Introspector caches BeanInfo per Class. Our implementation doesn't cache
        // anything — each call walks the class freshly — so there's nothing to flush.
        Ok(None)
    });
    r.register(
        intro,
        "flushFromCaches",
        "(Ljava/lang/Class;)V",
        |_ctx, _args| {
            // Same — no cache to flush.
            Ok(None)
        },
    );
    r.register(
        intro,
        "decapitalize",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let s_obj = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let s = ctx.read_string(s_obj).unwrap_or_default();
            let chars_vec: Vec<char> = s.chars().collect();
            let result = if chars_vec.is_empty() {
                s
            } else if chars_vec.len() >= 2
                && chars_vec[0].is_uppercase()
                && chars_vec[1].is_uppercase()
            {
                // Java spec: if first two chars are uppercase, don't change (e.g. "URL" stays "URL")
                s
            } else {
                let mut out = chars_vec[0].to_lowercase().to_string();
                for &c in &chars_vec[1..] {
                    out.push(c);
                }
                out
            };
            let rs = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(rs))))
        },
    );

    // BeanInfo = 2-field (propertyDescriptors=0 PD[], beanDescriptor=1)
    let bi = "java/beans/BeanInfo";
    r.register(
        bi,
        "getPropertyDescriptors",
        "()[Ljava/beans/PropertyDescriptor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pd = ctx.get_field(this, 0);
            match pd {
                Value::Object(Some(_)) => Ok(Some(pd)),
                _ => {
                    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
                    Ok(Some(Value::Object(Some(arr))))
                }
            }
        },
    );
    // getBeanDescriptor: slot 2 of our synthetic BeanInfo holds a real
    // java.beans.BeanDescriptor (built by `introspector_get_bean_info`).
    // Spring's CachedIntrospectionResults calls
    // `beanInfo.getBeanDescriptor().getBeanClass()`, so returning null here
    // NPEs (BeanWrapperTests.replaceWrappedInstance).
    r.register(
        bi,
        "getBeanDescriptor",
        "()Ljava/beans/BeanDescriptor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    // SPB.11: Spring's ExtendedBeanInfo constructor calls
    // `delegate.getMethodDescriptors()` and then `findCandidateWriteMethods`
    // (looking for set*-prefixed candidates) → `handleCandidateWriteMethod`
    // which calls `pd.setWriteMethod(method)` on the matching
    // SimplePropertyDescriptor. Returning an empty array here causes Spring
    // to never wire setters into its SimplePropertyDescriptor instances, so
    // `pd.getWriteMethod()` later returns null — surfaces as
    // `NotWritablePropertyException` on `metadataReaderFactory` etc.
    //
    // Slot 1 of our BeanInfo holds an Object[] of MethodDescriptor mirrors
    // (populated by `introspector_get_bean_info`). Each MethodDescriptor is
    // a 1-slot synthetic with the underlying `java.lang.reflect.Method` at
    // slot 0; the matching `MethodDescriptor.getMethod()` native is
    // registered below.
    r.register(
        bi,
        "getMethodDescriptors",
        "()[Ljava/beans/MethodDescriptor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mds = ctx.get_field(this, 1);
            match mds {
                Value::Object(Some(_)) => Ok(Some(mds)),
                _ => {
                    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
                    Ok(Some(Value::Object(Some(arr))))
                }
            }
        },
    );
    // MethodDescriptor.getMethod() is left to the real JDK bytecode:
    // `introspector_get_bean_info` now builds genuine
    // `java.beans.MethodDescriptor` objects via their JDK constructor, so the
    // wrapped Method resolves through the real `methodRef`.
    r.register(
        bi,
        "getEventSetDescriptors",
        "()[Ljava/beans/EventSetDescriptor;",
        |ctx, _args| {
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    // The synthetic BeanInfo objects above are intentionally stamped with the
    // BeanInfo interface so these native entries are their complete public
    // contract.  Leaving the remaining abstract interface members unresolved
    // makes perfectly ordinary callers (including Introspector's superclass
    // merge) dispatch to the abstract declaration and fail with
    // AbstractMethodError.
    r.register(bi, "getDefaultPropertyIndex", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(-1)))
    });
    r.register(bi, "getDefaultEventIndex", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(-1)))
    });
    r.register(
        bi,
        "getAdditionalBeanInfo",
        "()[Ljava/beans/BeanInfo;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(bi, "getIcon", "(I)Ljava/awt/Image;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    });

    // FeatureDescriptor.getName(): JDK declares `private String name` on
    // FeatureDescriptor. Resolving it via `get_field_by_name("name")` is robust
    // for both real JDK PropertyDescriptors (the JDK ctor calls setName) and
    // Spring subclasses (whose super-ctor calls setName), and agrees with
    // reflective `Field.get`. Keep this override so Spring's
    // ExtendedBeanInfo$PropertyDescriptorComparator never sees a null name.
    //
    // Spring's `ExtendedBeanInfoFactory`/`SimpleBeanInfoFactory.getBeanInfo`
    // are intentionally NOT overridden anymore. `introspector_get_bean_info`
    // now returns a BeanInfo whose PropertyDescriptors are real JDK PDs and
    // whose `getMethodDescriptors()` exposes *all* public methods, so Spring's
    // real `ExtendedBeanInfo` runs correctly on top of it: it discovers
    // non-standard write methods (static / non-void-returning setters, indexed
    // 2-arg setters) that java.beans.Introspector itself ignores. Short-
    // circuiting those factories to the plain delegate broke
    // CachedIntrospectionResultsTests.shouldUseExtendedBeanInfoWhenApplicable
    // and BeanWrapperTests.cornerSpr10115/cornerSpr13837 (non-standard setters
    // never became writable properties).

    // Register on both FeatureDescriptor (where getName is declared) AND
    // PropertyDescriptor (the typical invokevirtual call-site class). The
    // VM's hierarchy-walk doesn't always reach our parent-class native, so
    // duplicate the registration on the subclass call-site for safety.
    r.register(
        "java/beans/FeatureDescriptor",
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "name")))
        },
    );

    // PropertyDescriptor = 4-field
    //   field 0: name (String)
    //   field 1: readMethod (java.lang.reflect.Method or null)
    //   field 2: writeMethod (java.lang.reflect.Method or null)
    //   field 3: propertyType (java.lang.Class or null)
    //
    // Spring's BeanWrapperImpl checks `pd.getWriteMethod() != null` to decide
    // whether a property is writable. Returning null here surfaces as
    // `NotWritablePropertyException` even when the bean has a real setter, so
    // we materialise real `java.lang.reflect.Method` mirrors below in
    // `introspector_get_bean_info` and just hand them back here.
    let pd = "java/beans/PropertyDescriptor";
    // getName resolves the FeatureDescriptor `name` field by name (set by the
    // JDK PropertyDescriptor ctor / setName), robust across subclasses.
    r.register(pd, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    });

    // SPB.11: Spring's `GenericTypeAwarePropertyDescriptor` (built by
    // `CachedIntrospectionResults.buildGenericTypeAwarePropertyDescriptor`)
    // stores its read/write methods in its own `readMethod`/`writeMethod`
    // fields. We've observed that `getfield` on those fields returns null
    // even when `Field.get(...)` reflectively returns the right Method —
    // a field-slot mismatch in our class-layout computation for subclass
    // fields. Force getReadMethod/getWriteMethod to consult the dynamic
    // `readMethod`/`writeMethod` field via name-resolved lookup (which
    // matches the slot that the ctor's `putfield` wrote to). Without this,
    // `BeanWrapperImpl.isWritableProperty("metadataReaderFactory")` returns
    // false on Spring Boot demo and surfaces as `NotWritablePropertyException`.
    let gtapd = "org/springframework/beans/GenericTypeAwarePropertyDescriptor";
    r.register(
        gtapd,
        "getReadMethod",
        "()Ljava/lang/reflect/Method;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "readMethod")))
        },
    );
    r.register(
        gtapd,
        "getWriteMethod",
        "()Ljava/lang/reflect/Method;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "writeMethod")))
        },
    );
    r.register(
        gtapd,
        "getPropertyType",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pt = ctx.get_field_by_name(this, "propertyType");
            if matches!(pt, Value::Object(Some(_))) {
                return Ok(Some(pt));
            }
            // Fallback: derive from write method's parameter type, then read
            // method's return type (matches JDK findPropertyType behaviour).
            let wm = ctx.get_field_by_name(this, "writeMethod");
            if let Value::Object(Some(m)) = wm {
                // We can't easily call Method.getParameterTypes() here without
                // recursion machinery, so leave null — the read path below will
                // catch the common cases.
                let _ = m;
            }
            Ok(Some(pt))
        },
    );
    // NOTE: `java.beans.PropertyDescriptor.getReadMethod/getWriteMethod/
    // getPropertyType` are deliberately NOT overridden. `introspector_get_bean_info`
    // builds genuine JDK PropertyDescriptors (via the JDK ctor), so the real
    // bytecode resolves those via the JDK `readMethodRef`/`writeMethodRef`/
    // `propertyTypeRef`. Overriding them with synthetic overlay slots returned
    // null on the real PD objects that callers construct directly (Spring's
    // SimplePropertyDescriptor / IndexedPropertyDescriptor), which broke
    // ExtendedBeanInfoTests. The GTAPD overrides above remain because Spring's
    // GenericTypeAwarePropertyDescriptor stores read/write in its own fields.
    let _ = pd;
    r.set_category(__prev_cat);
}

/// Real Introspector.getBeanInfo() — discovers properties via getter/setter naming conventions.
/// Returns a synthetic BeanInfo (slot 0 = PropertyDescriptor[], slot 1 =
/// MethodDescriptor[], slot 2 = BeanDescriptor) whose descriptors are GENUINE
/// java.beans objects built via their JDK constructors.
///
/// Walks the target class plus its superclasses so that inherited setters
/// (e.g. `ConfigurationClassPostProcessor.setMetadataReaderFactory`, declared
/// on a superclass) are visible — Spring's `BeanWrapperImpl.setPropertyValue`
/// requires `pd.getWriteMethod() != null` to consider a property writable.
pub(crate) fn introspector_get_bean_info(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let trace = false;
    let class_mirror = match args.first() {
        Some(Value::Object(Some(c))) => *c,
        other => {
            if trace {
                eprintln!("BI-TRACE: arg0 unexpected: {:?}", other);
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    // Pin across the mirror/descriptor construction below — a moving young GC
    // there would relocate `class_mirror` (native stale-local family). The pin
    // stays live until the enclosing native returns (the VM truncates the pin
    // vec at native exit), so the `?`-free early returns below need no unpin.
    let class_mirror_pin = ctx.pin_native_root(class_mirror);

    // The argument is a `Class` *mirror*, so `class_id_of_object` returns
    // `java/lang/Class` itself. We need the represented class — use
    // `mirror_class_id`, which consults the VM's mirror→ClassId table.
    let class_id = match crate::lang_class::mirror_class_id(ctx, class_mirror) {
        Some(c) => c,
        None => {
            if trace {
                eprintln!("BI-TRACE: mirror_class_id returned None");
            }
            // Empty BeanInfo is safer than null (matches JDK behaviour for
            // classes with no introspectable bean properties). 3 slots so the
            // getMethodDescriptors (slot 1) / getBeanDescriptor (slot 2) natives
            // read in-bounds.
            let pd_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            // Pin across the sibling allocs below — a moving young GC there
            // would relocate the fresh arrays (native stale-local family).
            let pd_pin = ctx.pin_native_root(pd_arr);
            let md_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            let md_pin = ctx.pin_native_root(md_arr);
            let bean_info = alloc_concurrent_synthetic(ctx, "java/beans/BeanInfo", 3);
            let pd_arr = ctx.read_native_pin(pd_pin, pd_arr);
            let md_arr = ctx.read_native_pin(md_pin, md_arr);
            ctx.set_field(bean_info, 0, Value::Object(Some(pd_arr)));
            ctx.set_field(bean_info, 1, Value::Object(Some(md_arr)));
            ctx.unpin_native_roots(class_mirror_pin);
            return Ok(Some(Value::Object(Some(bean_info))));
        }
    };
    if trace {
        let cn = ctx.class_name_of_id(class_id).unwrap_or_default();
        eprintln!("BI-TRACE: class_id resolved -> {}", cn);
    }

    // Discover properties from getters/setters across the class + superclasses,
    // replicating jakarta.el.BeanSupportStandalone — which itself mirrors the
    // JDK java.beans.Introspector property-merge rules that the Tomcat suite
    // (TestBeanSupport) asserts:
    //   * read method = getXxx()/isXxx(); a boolean isXxx() locks out getXxx();
    //   * among overloaded setXxx(T) the write method is chosen by walking the
    //     assignable chain, seeded by the getter's return type (or, with no
    //     getter, by the lexicographically-smallest parameter type name);
    //   * property type = read method return type, else the chosen setter's
    //     parameter type.
    // Convert a field/param descriptor to Class.getName() form for sorting.
    fn desc_to_binary_name(desc: &str) -> String {
        match desc.chars().next() {
            Some('L') => desc[1..desc.len().saturating_sub(1)].replace('/', "."),
            Some('[') => desc.replace('/', "."),
            Some('Z') => "boolean".to_string(),
            Some('B') => "byte".to_string(),
            Some('C') => "char".to_string(),
            Some('S') => "short".to_string(),
            Some('I') => "int".to_string(),
            Some('J') => "long".to_string(),
            Some('F') => "float".to_string(),
            Some('D') => "double".to_string(),
            _ => desc.to_string(),
        }
    }
    // Every mirror accumulated during discovery is produced by an allocating
    // call and read again only after MANY later allocating calls (the rest of
    // the discovery scan, then the descriptor-build tail), so each ref is
    // stored together with its native-pin handle — `(pin_handle, ref)` — and
    // re-read from the pin at the point of use (native stale-local family).
    #[derive(Default)]
    struct PropAcc {
        name: String,
        read_method: Option<(usize, ObjectRef)>,
        read_ret_mirror: Option<(usize, ObjectRef)>,
        read_ret_desc: Option<String>,
        read_ret_cid: Option<cratonvm_types::ClassId>,
        uses_is: bool,
        // (method_mirror, param_desc, param_mirror, param_class_id)
        write_methods: Vec<(
            (usize, ObjectRef),
            String,
            (usize, ObjectRef),
            Option<cratonvm_types::ClassId>,
        )>,
        // indexed read method `getXxx(int) -> T` (first one wins).
        indexed_read: Option<(usize, ObjectRef)>,
        // void-returning indexed setters `setXxx(int, E)` — (mirror, E desc).
        indexed_write_candidates: Vec<((usize, ObjectRef), String)>,
    }
    fn prop_idx(props: &mut Vec<PropAcc>, name: &str) -> usize {
        match props.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                props.push(PropAcc {
                    name: name.to_string(),
                    ..Default::default()
                });
                props.len() - 1
            }
        }
    }
    let mut props: Vec<PropAcc> = Vec::new();

    // Scan order: the class + its superclass chain first (so concrete overrides
    // win the dedup), then ALL transitively-implemented interfaces — so interface
    // DEFAULT methods (e.g. a `default String getValueC()`) are discovered as
    // bean properties. This mirrors java.beans.Introspector, which works off
    // Class.getMethods() (all public methods incl. inherited interface methods);
    // the old code walked only `superclass_of` and missed default-method props
    // (jakarta.el.TestBeanELResolver.testGetDefaultValue: property `valueC`).
    // G2-fix parity (see `collect_public_methods` in lang_class.rs): every
    // interface's constant-pool `super_class` entry points at
    // `java/lang/Object` per JVMS, but interfaces do NOT semantically inherit
    // from Object. Walking `superclass_of` unconditionally here pulled
    // Object's declared methods (including `getClass`) into the scan for any
    // interface target, which then synthesized a spurious "class" property
    // below on top of the *explicit* one — HotSpot's Introspector does not
    // add "class" (or any Object member) when introspecting a bare interface
    // (PropertyDescriptorUtilsPropertyResolutionTests.
    // determineBasicPropertiesWithUnresolvedGenericsInInterface). Only walk
    // the superclass chain for non-interface classes.
    let mut scan_cids: Vec<cratonvm_types::ClassId> = Vec::new();
    let mut sc = Some(class_id);
    while let Some(cid) = sc {
        scan_cids.push(cid);
        sc = if ctx.is_interface_class(cid) {
            None
        } else {
            ctx.superclass_of(cid)
        };
    }
    {
        let mut seen_if: std::collections::HashSet<cratonvm_types::ClassId> =
            std::collections::HashSet::new();
        let mut queue: Vec<cratonvm_types::ClassId> = scan_cids
            .iter()
            .flat_map(|&cid| ctx.class_interfaces(cid))
            .collect();
        let mut qi = 0;
        while qi < queue.len() {
            let icid = queue[qi];
            qi += 1;
            if !seen_if.insert(icid) {
                continue;
            }
            scan_cids.push(icid);
            for sup in ctx.class_interfaces(icid) {
                queue.push(sup);
            }
        }
    }
    let mut seen_method_keys: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    // ALL public methods (instance + static, excluding constructors), used to
    // build getMethodDescriptors() — java.beans BeanInfo exposes every
    // Class.getMethods() entry, and Spring's ExtendedBeanInfo scans them for
    // non-standard write methods.
    let mut all_method_mirrors: Vec<(usize, ObjectRef)> = Vec::new();
    for cid in scan_cids {
        // Resolve the mirror for this declaring class so the Method mirror
        // points at the class that actually declares the method.
        let declaring_mirror = ctx.get_class_mirror(cid);
        // Pin across the per-method mirror construction below — every
        // `build_method_mirror` / descriptor-mirror call allocates, and a
        // moving young GC there would relocate `declaring_mirror` between
        // methods (native stale-local family). Re-read before each use.
        let declaring_mirror_pin = ctx.pin_native_root(declaring_mirror);

        let methods = ctx.declared_methods(cid);
        for method in &methods {
            let name = &method.name;
            let desc = &method.descriptor;
            // Dedupe across inheritance: subclass override wins.
            let key = (name.clone(), desc.clone());
            if !seen_method_keys.insert(key) {
                continue;
            }
            // Collect every public, non-constructor method for the
            // MethodDescriptor[] (mirrors Class.getMethods()).
            if method.access_flags & 0x0001 != 0 && !name.starts_with('<') {
                let declaring_mirror = ctx.read_native_pin(declaring_mirror_pin, declaring_mirror);
                let mm_all = crate::jmx_openmbean::build_method_mirror(
                    ctx,
                    declaring_mirror,
                    name,
                    desc,
                    method.access_flags,
                );
                let mm_all_pin = ctx.pin_native_root(mm_all);
                all_method_mirrors.push((mm_all_pin, mm_all));
            }
            // JavaBeans properties come from PUBLIC INSTANCE methods only
            // (java.beans uses Class.getMethods(), which is public-only — a
            // non-public getFoo()/setFoo() is NOT a bean property).
            if method.access_flags & 0x0008 != 0 || method.access_flags & 0x0001 == 0 {
                continue;
            }

            // getter: getXxx() -> T (T != void) or isXxx() -> boolean.
            let is_get = name.starts_with("get")
                && name.len() > 3
                && desc.starts_with("()")
                && !desc.ends_with(")V");
            let is_is = name.starts_with("is") && name.len() > 2 && desc == "()Z";
            if is_get || is_is {
                let prop_start = if is_get { 3 } else { 2 };
                let prop_name = decapitalize(&name[prop_start..]);
                let ret_desc = desc
                    .split(')')
                    .nth(1)
                    .unwrap_or("Ljava/lang/Object;")
                    .to_string();
                let ret_mirror =
                    crate::jmx_openmbean::type_descriptor_to_class_mirror_pub(ctx, &ret_desc);
                let ret_mirror_pin = ctx.pin_native_root(ret_mirror);
                let ret_cid = crate::lang_class::mirror_class_id(ctx, ret_mirror);
                let declaring_mirror = ctx.read_native_pin(declaring_mirror_pin, declaring_mirror);
                let mm = crate::jmx_openmbean::build_method_mirror(
                    ctx,
                    declaring_mirror,
                    name,
                    desc,
                    method.access_flags,
                );
                let mm_pin = ctx.pin_native_root(mm);
                let ret_mirror = ctx.read_native_pin(ret_mirror_pin, ret_mirror);
                let idx = prop_idx(&mut props, &prop_name);
                let p = &mut props[idx];
                if is_is {
                    // A boolean isXxx() always wins and locks out plain getters.
                    p.read_method = Some((mm_pin, mm));
                    p.read_ret_mirror = Some((ret_mirror_pin, ret_mirror));
                    p.read_ret_desc = Some(ret_desc);
                    p.read_ret_cid = ret_cid;
                    p.uses_is = true;
                } else if !p.uses_is && p.read_method.is_none() {
                    // First plain getter (subclass is walked first) wins.
                    p.read_method = Some((mm_pin, mm));
                    p.read_ret_mirror = Some((ret_mirror_pin, ret_mirror));
                    p.read_ret_desc = Some(ret_desc);
                    p.read_ret_cid = ret_cid;
                }
            }

            // indexed getter: getXxx(int) -> T (T != void).
            if name.starts_with("get") && name.len() > 3 {
                let (params, ret) = crate::jmx_openmbean::parse_method_descriptor_pub(desc);
                if params.len() == 1 && params[0] == "I" && ret != "V" {
                    let prop_name = decapitalize(&name[3..]);
                    let declaring_mirror =
                        ctx.read_native_pin(declaring_mirror_pin, declaring_mirror);
                    let mm = crate::jmx_openmbean::build_method_mirror(
                        ctx,
                        declaring_mirror,
                        name,
                        desc,
                        method.access_flags,
                    );
                    let mm_pin = ctx.pin_native_root(mm);
                    let idx = prop_idx(&mut props, &prop_name);
                    if props[idx].indexed_read.is_none() {
                        props[idx].indexed_read = Some((mm_pin, mm));
                    }
                }
            }

            // setter: setXxx(T) -> void (1 param) = standard write;
            //         setXxx(int, E) -> void (2 params) = indexed write.
            if name.starts_with("set") && name.len() > 3 && desc.ends_with(")V") {
                let (params, _ret) = crate::jmx_openmbean::parse_method_descriptor_pub(desc);
                if params.len() == 1 {
                    let prop_name = decapitalize(&name[3..]);
                    let param_mirror =
                        crate::jmx_openmbean::type_descriptor_to_class_mirror_pub(ctx, &params[0]);
                    let param_mirror_pin = ctx.pin_native_root(param_mirror);
                    let param_cid = crate::lang_class::mirror_class_id(ctx, param_mirror);
                    let declaring_mirror =
                        ctx.read_native_pin(declaring_mirror_pin, declaring_mirror);
                    let mm = crate::jmx_openmbean::build_method_mirror(
                        ctx,
                        declaring_mirror,
                        name,
                        desc,
                        method.access_flags,
                    );
                    let mm_pin = ctx.pin_native_root(mm);
                    let param_mirror = ctx.read_native_pin(param_mirror_pin, param_mirror);
                    let idx = prop_idx(&mut props, &prop_name);
                    props[idx].write_methods.push((
                        (mm_pin, mm),
                        params[0].clone(),
                        (param_mirror_pin, param_mirror),
                        param_cid,
                    ));
                } else if params.len() == 2 && params[0] == "I" {
                    // Standard (void-returning) indexed setter.
                    let prop_name = decapitalize(&name[3..]);
                    let declaring_mirror =
                        ctx.read_native_pin(declaring_mirror_pin, declaring_mirror);
                    let mm = crate::jmx_openmbean::build_method_mirror(
                        ctx,
                        declaring_mirror,
                        name,
                        desc,
                        method.access_flags,
                    );
                    let mm_pin = ctx.pin_native_root(mm);
                    let idx = prop_idx(&mut props, &prop_name);
                    props[idx]
                        .indexed_write_candidates
                        .push(((mm_pin, mm), params[1].clone()));
                }
            }
        }
    }

    // Resolve each accumulated property into the
    // (name, readMethod, writeMethod, propertyType, indexedReadMethod,
    // indexedWriteMethod) tuple the descriptor builder below expects.
    // Each Option carries a `(pin_handle, ref)` pair — see `PropAcc` above.
    // NOTE: this resolve loop performs no allocating ctx calls, so reading
    // the raw refs stored in `props` here (they are only copied, never
    // dereferenced) is safe; every heap read happens in the descriptor-build
    // tail below via `read_native_pin`.
    let mut properties: Vec<(
        String,
        Option<(usize, ObjectRef)>,
        Option<(usize, ObjectRef)>,
        Option<(usize, ObjectRef)>,
        Option<(usize, ObjectRef)>,
        Option<(usize, ObjectRef)>,
    )> = Vec::with_capacity(props.len());
    for p in &mut props {
        let mut write_method: Option<(usize, ObjectRef)> = None;
        let mut write_param_mirror: Option<(usize, ObjectRef)> = None;
        if !p.write_methods.is_empty() {
            // Seed type: getter return type, else smallest parameter type name.
            let (mut type_desc, mut type_cid) = if p.read_method.is_some() {
                (p.read_ret_desc.clone().unwrap_or_default(), p.read_ret_cid)
            } else {
                p.write_methods
                    .sort_by(|a, b| desc_to_binary_name(&a.1).cmp(&desc_to_binary_name(&b.1)));
                (p.write_methods[0].1.clone(), p.write_methods[0].3)
            };
            for (mm, pdesc, pmirror, pcid) in &p.write_methods {
                let assignable = if type_desc == *pdesc {
                    true
                } else if let (Some(t), Some(c)) = (type_cid, *pcid) {
                    c == t || ctx.is_subclass(c, t)
                } else {
                    false
                };
                if assignable {
                    type_desc = pdesc.clone();
                    type_cid = *pcid;
                    write_method = Some(*mm);
                    write_param_mirror = Some(*pmirror);
                }
            }
        }
        // Property type: read method return type, else chosen setter param type.
        let type_mirror = if p.read_method.is_some() {
            p.read_ret_mirror
        } else {
            write_param_mirror
        };
        // Standard indexed write method = first void-returning setXxx(int, E).
        let indexed_write = p.indexed_write_candidates.first().map(|(m, _)| *m);
        properties.push((
            p.name.clone(),
            p.read_method,
            write_method,
            type_mirror,
            p.indexed_read,
            indexed_write,
        ));
    }

    // Always include the synthetic "class" property (java.beans includes it
    // because every Object has `getClass()`). Spring's reflection caches key
    // off PD presence, so omitting it can mislead callers. EXCEPT for
    // interfaces: `getClass()` is inherited from Object, not the interface
    // itself, and HotSpot's Introspector does not surface it (or any other
    // Object member) when introspecting a bare interface type — see the
    // `is_interface_class` gate on `scan_cids` above for the matching
    // rationale.
    if !ctx.is_interface_class(class_id) && !properties.iter().any(|(n, ..)| n == "class") {
        let class_class_mirror = match ctx.ensure_class_initialized("java/lang/Class") {
            Ok(cid) => ctx.get_class_mirror(cid),
            // Re-read from the pin: the discovery scan above (and the failed
            // load here) allocated, so the raw `class_mirror` is stale
            // (native stale-local family).
            Err(_) => ctx.read_native_pin(class_mirror_pin, class_mirror),
        };
        let class_class_mirror_pin = ctx.pin_native_root(class_class_mirror);
        let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
        let getter = crate::jmx_openmbean::build_method_mirror(
            ctx,
            class_mirror,
            "getClass",
            "()Ljava/lang/Class;",
            0x0001, /* ACC_PUBLIC */
        );
        let getter_pin = ctx.pin_native_root(getter);
        let class_class_mirror = ctx.read_native_pin(class_class_mirror_pin, class_class_mirror);
        properties.push((
            "class".to_string(),
            Some((getter_pin, getter)),
            None,
            Some((class_class_mirror_pin, class_class_mirror)),
            None,
            None,
        ));
    }

    // java.beans returns PropertyDescriptors sorted by name (String natural
    // order). Spring's ExtendedBeanInfo keeps them in a TreeSet keyed by the
    // same order, so `ExtendedBeanInfoTests.propertyDescriptorOrderIsEqual`
    // requires our plain BeanInfo to be sorted identically. Property names are
    // ASCII identifiers, so Rust's str ordering matches Java's char ordering.
    properties.sort_by(|a, b| a.0.cmp(&b.0));

    // Build PropertyDescriptor[] as GENUINE java.beans.PropertyDescriptor
    // objects, constructed via the JDK ctor
    // `PropertyDescriptor(String, Method, Method)`. The ctor populates the real
    // `readMethodRef`/`writeMethodRef`/`propertyTypeRef`, so the unmodified JDK
    // bytecode for `getReadMethod`/`getWriteMethod`/`getPropertyType` works.
    //
    // Earlier revisions allocated PD objects with synthetic *overlay* slots and
    // intercepted the getters to read them. That was incompatible with the real
    // PD objects callers construct directly (Spring's SimplePropertyDescriptor /
    // GenericTypeAwarePropertyDescriptor, and java.beans.IndexedPropertyDescriptor):
    // the overlay getters read out-of-range slots on those and returned null,
    // breaking ExtendedBeanInfoTests.
    let pd_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), properties.len());
    // Pin across the per-property ctor invokes below — a moving young GC there
    // would relocate the fresh array (native stale-local family).
    let pd_arr_pin = ctx.pin_native_root(pd_arr);
    for (i, (prop_name, getter, setter, _type_mirror, idx_read, idx_write)) in
        properties.iter().enumerate()
    {
        let name_str = ctx.create_string(prop_name);
        let name_pin = ctx.pin_native_root(name_str);
        // Re-read every pinned discovery-phase mirror to its current address —
        // the `create_string` above and the previous iterations' ctor invokes
        // may have moved them (native stale-local family).
        let getter_cur = getter.map(|(h, o)| ctx.read_native_pin(h, o));
        let setter_cur = setter.map(|(h, o)| ctx.read_native_pin(h, o));
        let idx_read_cur = idx_read.map(|(h, o)| ctx.read_native_pin(h, o));
        let idx_write_cur = idx_write.map(|(h, o)| ctx.read_native_pin(h, o));
        // A property with any indexed accessor becomes a java.beans
        // IndexedPropertyDescriptor (so `pd instanceof IndexedPropertyDescriptor`
        // holds and getIndexedReadMethod/getIndexedWriteMethod work). The JDK
        // 5-arg ctor validates index/type consistency itself.
        let pd = if idx_read.is_some() || idx_write.is_some() {
            match ctx.new_object_initialized(
                "java/beans/IndexedPropertyDescriptor",
                "(Ljava/lang/String;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;)V",
                &[
                    Value::Object(Some(name_str)),
                    Value::Object(getter_cur),
                    Value::Object(setter_cur),
                    Value::Object(idx_read_cur),
                    Value::Object(idx_write_cur),
                ],
            ) {
                Ok(Some(Value::Object(Some(p)))) => Some(p),
                // The ctor can throw IntrospectionException on an inconsistent
                // indexed/array type pairing; retry with just the indexed read
                // (java.beans favours the read method) before giving up.
                _ => {
                    let name_str = ctx.read_native_pin(name_pin, name_str);
                    let idx_read_cur = idx_read.map(|(h, o)| ctx.read_native_pin(h, o));
                    match ctx.new_object_initialized(
                        "java/beans/IndexedPropertyDescriptor",
                        "(Ljava/lang/String;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;)V",
                        &[
                            Value::Object(Some(name_str)),
                            Value::Object(None),
                            Value::Object(None),
                            Value::Object(idx_read_cur),
                            Value::Object(None),
                        ],
                    ) {
                        Ok(Some(Value::Object(Some(p)))) => Some(p),
                        _ => None,
                    }
                }
            }
        } else {
            None
        };
        let pd = match pd {
            Some(p) => p,
            None => {
                let name_str = ctx.read_native_pin(name_pin, name_str);
                // Re-read again — the failed ctor invokes above (indexed
                // case) may have moved the mirrors since the reads at the
                // top of this iteration.
                let getter_cur = getter.map(|(h, o)| ctx.read_native_pin(h, o));
                let setter_cur = setter.map(|(h, o)| ctx.read_native_pin(h, o));
                build_property_descriptor(ctx, name_str, getter_cur, setter_cur)
            }
        };
        let pd_arr = ctx.read_native_pin(pd_arr_pin, pd_arr);
        ctx.set_array_element(pd_arr, i, Value::Object(Some(pd)));
        ctx.unpin_native_roots(name_pin);
    }

    // Build MethodDescriptor[] as genuine java.beans.MethodDescriptor objects
    // over ALL public methods (java.beans BeanInfo exposes every
    // Class.getMethods() entry). Spring's ExtendedBeanInfo.findCandidateWriteMethods
    // scans these to discover non-standard write methods (static setters,
    // non-void-returning setters, 2-arg indexed setters) that
    // java.beans.Introspector itself ignores.
    let md_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), all_method_mirrors.len());
    // Pin across the per-method ctor invokes below — a moving young GC there
    // would relocate the fresh array (native stale-local family).
    let md_arr_pin = ctx.pin_native_root(md_arr);
    for (i, &(m_pin, m)) in all_method_mirrors.iter().enumerate() {
        // Re-read the pinned mirror to its current address — the discovery
        // scan and the previous iterations' ctor invokes may have moved it
        // (native stale-local family).
        let m = ctx.read_native_pin(m_pin, m);
        let md = match ctx.new_object_initialized(
            "java/beans/MethodDescriptor",
            "(Ljava/lang/reflect/Method;)V",
            &[Value::Object(Some(m))],
        ) {
            Ok(Some(Value::Object(Some(d)))) => d,
            _ => {
                let md = alloc_concurrent_synthetic(ctx, "java/beans/MethodDescriptor", 1);
                // Re-read — the failed ctor invoke + the alloc above may have
                // moved the mirror again.
                let m = ctx.read_native_pin(m_pin, m);
                ctx.set_field_by_name(md, "method", Value::Object(Some(m)));
                ctx.set_field(md, 0, Value::Object(Some(m)));
                md
            }
        };
        let md_arr = ctx.read_native_pin(md_arr_pin, md_arr);
        ctx.set_array_element(md_arr, i, Value::Object(Some(md)));
    }

    // Build a real java.beans.BeanDescriptor so BeanInfo.getBeanDescriptor() is
    // non-null (Spring's CachedIntrospectionResults calls
    // `beanInfo.getBeanDescriptor().getBeanClass()`).
    let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
    let bean_descriptor = match ctx.new_object_initialized(
        "java/beans/BeanDescriptor",
        "(Ljava/lang/Class;)V",
        &[Value::Object(Some(class_mirror))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Object(None),
    };
    let bd_pin = pinned_object_value(ctx, bean_descriptor);

    // Build BeanInfo: slot 0 = PD[], slot 1 = MethodDescriptor[],
    // slot 2 = BeanDescriptor.
    let bean_info = alloc_concurrent_synthetic(ctx, "java/beans/BeanInfo", 3);
    let pd_arr = ctx.read_native_pin(pd_arr_pin, pd_arr);
    let md_arr = ctx.read_native_pin(md_arr_pin, md_arr);
    let bean_descriptor = read_pinned_object_value(ctx, bd_pin, bean_descriptor);
    ctx.set_field(bean_info, 0, Value::Object(Some(pd_arr)));
    ctx.set_field(bean_info, 1, Value::Object(Some(md_arr)));
    ctx.set_field(bean_info, 2, bean_descriptor);

    ctx.unpin_native_roots(class_mirror_pin);
    Ok(Some(Value::Object(Some(bean_info))))
}

/// Build a genuine `java.beans.PropertyDescriptor` from a name + read/write
/// Method mirrors, matching java.beans' lenient construction.
///
/// The public `PropertyDescriptor(String, Method, Method)` ctor rejects a
/// read/write pair whose property types are not *exactly* equal (e.g. a
/// `Number getFoo()` paired with `void setFoo(Integer)` — see HotSpot, which
/// also throws from that ctor). java.beans.Introspector itself does NOT use
/// that ctor; it sets the methods individually, and `setWriteMethod` stores the
/// write-method reference *before* it validates the type, so the write method
/// survives the resulting IntrospectionException. We reproduce that: build a
/// read-only PD, then call `setWriteMethod` and ignore a thrown
/// IntrospectionException (the reference is already stored). The exception is
/// fully contained by the `invoke` boundary — getReadMethod/getWriteMethod then
/// return both methods, exactly like HotSpot.
pub(crate) fn build_property_descriptor(
    ctx: &mut dyn NativeContext,
    name_str: ObjectRef,
    read: Option<ObjectRef>,
    write: Option<ObjectRef>,
) -> ObjectRef {
    // Pin the inputs across the ctor/setter invokes below — each invoke can
    // run a moving young GC that relocates them (native stale-local family).
    // Entry contract: callers pass refs that are current at call time.
    let name_pin = ctx.pin_native_root(name_str);
    let read_pin = read.map(|o| ctx.pin_native_root(o));
    let write_pin = write.map(|o| ctx.pin_native_root(o));
    // First try the strict 3-arg ctor; it succeeds for the common (matching or
    // read-only/write-only) cases and yields the correct propertyType.
    if let Ok(Some(Value::Object(Some(p)))) = ctx.new_object_initialized(
        "java/beans/PropertyDescriptor",
        "(Ljava/lang/String;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;)V",
        &[
            Value::Object(Some(name_str)),
            Value::Object(read),
            Value::Object(write),
        ],
    ) {
        ctx.unpin_native_roots(name_pin);
        return p;
    }
    // Strict ctor rejected the pair (type mismatch). Build a read-only PD, then
    // attach the write method leniently (ref stored before validation throws).
    // Re-read the inputs first — the failed ctor invoke above may have moved
    // them.
    let name_str = ctx.read_native_pin(name_pin, name_str);
    let read = match (read_pin, read) {
        (Some(h), Some(o)) => Some(ctx.read_native_pin(h, o)),
        _ => read,
    };
    let pd = match ctx.new_object_initialized(
        "java/beans/PropertyDescriptor",
        "(Ljava/lang/String;Ljava/lang/reflect/Method;Ljava/lang/reflect/Method;)V",
        &[
            Value::Object(Some(name_str)),
            Value::Object(read),
            Value::Object(None),
        ],
    ) {
        Ok(Some(Value::Object(Some(p)))) => Some(p),
        _ => None,
    };
    if let (Some(pd), Some(_)) = (pd, write) {
        let pd_pin = ctx.pin_native_root(pd);
        let write = match (write_pin, write) {
            (Some(h), Some(o)) => Some(ctx.read_native_pin(h, o)),
            _ => write,
        };
        // This is the package-private helper used by the JDK Introspector.
        // The public setter validates read/write type equality *before*
        // retaining the Method, while Introspector deliberately preserves its
        // selected covariant setter.  Calling the helper keeps that same
        // descriptor contract for overloaded JavaBeans properties.
        let _ = ctx.invoke(
            "java/beans/PropertyDescriptor",
            "setWriteMethod0",
            "(Ljava/lang/reflect/Method;)V",
            &[Value::Object(Some(pd)), Value::Object(write)],
        );
        // Re-read — the invoke above may have moved the descriptor we return.
        let pd = ctx.read_native_pin(pd_pin, pd);
        ctx.unpin_native_roots(name_pin);
        return pd;
    }
    if let Some(pd) = pd {
        ctx.unpin_native_roots(name_pin);
        return pd;
    }
    // Last resort (class load failure): a bare object with the name field set
    // keeps callers from NPEing on getName().
    let cid = ctx
        .ensure_class_initialized("java/beans/PropertyDescriptor")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let n = ctx.class_num_total_fields(cid);
    let o = ctx.alloc_object(cid, n);
    let name_str = ctx.read_native_pin(name_pin, name_str);
    ctx.set_field_by_name(o, "name", Value::Object(Some(name_str)));
    ctx.unpin_native_roots(name_pin);
    o
}

/// JavaBeans decapitalize: "FooBar" -> "fooBar", "URL" -> "URL" (first two uppercase stay)
pub(crate) fn decapitalize(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() >= 2 && chars[0].is_uppercase() && chars[1].is_uppercase() {
        return s.to_string();
    }
    let mut out = chars[0].to_lowercase().to_string();
    for &c in &chars[1..] {
        out.push(c);
    }
    out
}

// =============================================================================
// javax.naming — JNDI stubs
// =============================================================================

pub(crate) fn register_p72_naming(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // InitialContext = 2-field (bindings=0 HashMap, env=1 HashMap)
    let ic = "javax/naming/InitialContext";
    r.register(ic, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Pin across the map allocs/inits below — a moving young GC there
        // would relocate them (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let bindings = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
        let bindings_pin = ctx.pin_native_root(bindings);
        cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(bindings))]).ok();
        let this = ctx.read_native_pin(this_pin, this);
        let bindings = ctx.read_native_pin(bindings_pin, bindings);
        ctx.set_field(this, 0, Value::Object(Some(bindings)));
        let env = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
        let env_pin = ctx.pin_native_root(env);
        cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(env))]).ok();
        let this = ctx.read_native_pin(this_pin, this);
        let env = ctx.read_native_pin(env_pin, env);
        ctx.set_field(this, 1, Value::Object(Some(env)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(ic, "<init>", "(Ljava/util/Hashtable;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let env_arg = args.get(1).copied().unwrap_or(Value::Object(None));
        // Pin across the map alloc/init below — a moving young GC there would
        // relocate them (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let env_pin = pinned_object_value(ctx, env_arg);
        let bindings = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
        let bindings_pin = ctx.pin_native_root(bindings);
        cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(bindings))]).ok();
        let this = ctx.read_native_pin(this_pin, this);
        let bindings = ctx.read_native_pin(bindings_pin, bindings);
        ctx.set_field(this, 0, Value::Object(Some(bindings)));
        let env_arg = read_pinned_object_value(ctx, env_pin, env_arg);
        ctx.set_field(this, 1, env_arg);
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(
        ic,
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(bindings)) = ctx.get_field(this, 0) {
                let result = cratonvm_native_collections::native_map_get_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), key],
                )?;
                Ok(Some(result.unwrap_or(Value::Object(None))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
    r.register(
        ic,
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(bindings)) = ctx.get_field(this, 0) {
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), key, val],
                )?;
            }
            Ok(None)
        },
    );
    r.register(
        ic,
        "rebind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(bindings)) = ctx.get_field(this, 0) {
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), key, val],
                )?;
            }
            Ok(None)
        },
    );
    r.register(ic, "unbind", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        if let Value::Object(Some(bindings)) = ctx.get_field(this, 0) {
            cratonvm_native_collections::native_map_remove_pub(
                ctx,
                &[Value::Object(Some(bindings)), key],
            )?;
        }
        Ok(None)
    });
    r.register(
        ic,
        "list",
        "(Ljava/lang/String;)Ljavax/naming/NamingEnumeration;",
        |ctx, _args| {
            let lst = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(lst))]).ok();
            Ok(Some(Value::Object(Some(lst))))
        },
    );
    r.register(ic, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Clear bindings on close
        // Pin across the map alloc/init below — a moving young GC there would
        // relocate them (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let empty = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
        let empty_pin = ctx.pin_native_root(empty);
        cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(empty))]).ok();
        let this = ctx.read_native_pin(this_pin, this);
        let empty = ctx.read_native_pin(empty_pin, empty);
        ctx.set_field(this, 0, Value::Object(Some(empty)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(
        ic,
        "getEnvironment",
        "()Ljava/util/Hashtable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(
        ic,
        "rename",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let old_name = args.get(1).copied().unwrap_or(Value::Object(None));
            let new_name = args.get(2).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(bindings)) = ctx.get_field(this, 0) {
                // Pin across the chained map ops below (their hashCode/equals
                // callbacks can allocate) — a moving young GC there would
                // relocate them (native stale-local family).
                let bindings_pin = ctx.pin_native_root(bindings);
                let old_pin = pinned_object_value(ctx, old_name);
                let new_pin = pinned_object_value(ctx, new_name);
                // Get the value under old name
                let val = cratonvm_native_collections::native_map_get_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), old_name],
                )?
                .unwrap_or(Value::Object(None));
                let val_pin = pinned_object_value(ctx, val);
                // Remove old, put new
                let bindings = ctx.read_native_pin(bindings_pin, bindings);
                let old_name = read_pinned_object_value(ctx, old_pin, old_name);
                cratonvm_native_collections::native_map_remove_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), old_name],
                )?;
                let bindings = ctx.read_native_pin(bindings_pin, bindings);
                let new_name = read_pinned_object_value(ctx, new_pin, new_name);
                let val = read_pinned_object_value(ctx, val_pin, val);
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), new_name, val],
                )?;
                ctx.unpin_native_roots(bindings_pin);
            }
            Ok(None)
        },
    );
    r.register(
        ic,
        "createSubcontext",
        "(Ljava/lang/String;)Ljavax/naming/Context;",
        |ctx, _args| {
            let sub = alloc_concurrent_synthetic(ctx, "javax/naming/InitialContext", 2);
            // Pin across the map alloc/init below — a moving young GC there
            // would relocate them (native stale-local family).
            let sub_pin = ctx.pin_native_root(sub);
            let b2 = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
            let b2_pin = ctx.pin_native_root(b2);
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(b2))]).ok();
            let sub = ctx.read_native_pin(sub_pin, sub);
            let b2 = ctx.read_native_pin(b2_pin, b2);
            ctx.set_field(sub, 0, Value::Object(Some(b2)));
            ctx.unpin_native_roots(sub_pin);
            Ok(Some(Value::Object(Some(sub))))
        },
    );
    r.register(
        ic,
        "destroySubcontext",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            // Remove the sub-context binding
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(bindings)) = ctx.get_field(this, 0) {
                cratonvm_native_collections::native_map_remove_pub(
                    ctx,
                    &[Value::Object(Some(bindings)), key],
                )?;
            }
            Ok(None)
        },
    );

    // NamingException
    for cls in [
        "javax/naming/NamingException",
        "javax/naming/NameNotFoundException",
        "javax/naming/NameAlreadyBoundException",
        "javax/naming/NoInitialContextException",
    ] {
        r.register(cls, "<init>", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.first().copied() {
                let msg = args.get(1).copied().unwrap_or(Value::Object(None));
                if ctx.object_num_fields(this) > 0 {
                    ctx.set_field(this, 0, msg);
                }
            }
            Ok(None)
        });
        r.register(cls, "getMessage", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) > 0 {
                Ok(Some(ctx.get_field(this, 0)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        });
    }

    // Context interface stub (javax/naming/Context)
    let ctx_iface = "javax/naming/Context";
    r.register(
        ctx_iface,
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        ctx_iface,
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_noop_with_this,
    );
    r.register(
        ctx_iface,
        "rebind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_noop_with_this,
    );
    // Note: Context is an interface — these are fallback defaults. Real implementations
    // are on javax/naming/InitialContext which will be dispatched via virtual lookup first.
    r.register(
        ctx_iface,
        "unbind",
        "(Ljava/lang/String;)V",
        |_ctx, _args| {
            // Default interface implementation — concrete classes override.
            Ok(None)
        },
    );
    r.register(ctx_iface, "close", "()V", |_ctx, _args| {
        // Default interface implementation — concrete classes override.
        Ok(None)
    });
    r.set_category(__prev_cat);
}
