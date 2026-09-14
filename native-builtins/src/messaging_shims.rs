// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Messaging shims: embedded ActiveMQ, Spring messaging/STOMP and the Reactor Netty transport bridges.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

pub(crate) fn native_message_bytes_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.get_field_by_name(this, "type").as_int().unwrap_or(0) {
        0 | 1 => Ok(Some(ctx.get_field_by_name(this, "strValue"))),
        2 => {
            // ByteChunk.toString() can execute overridable bytecode and move
            // MessageBytes; reload its receiver before caching the result.
            let this_pin = ctx.pin_native_root(this);
            let byte_string = match ctx.get_field_by_name(this, "byteC") {
                Value::Object(Some(byte_c)) => {
                    match ctx.invoke_virtual(byte_c, "toString", "()Ljava/lang/String;", &[]) {
                        Ok(result) => result,
                        Err(error) => {
                            ctx.unpin_native_roots(this_pin);
                            return Err(error);
                        }
                    }
                }
                _ => None,
            };
            let this = ctx.read_native_pin(this_pin, this);
            if let Some(value @ Value::Object(_)) = byte_string {
                ctx.set_field_by_name(this, "strValue", value);
                ctx.unpin_native_roots(this_pin);
                Ok(Some(value))
            } else {
                ctx.unpin_native_roots(this_pin);
                Ok(Some(Value::Object(None)))
            }
        }
        3 => {
            // create_string can collect, so retain this receiver while
            // producing the cached String and refresh it before the write.
            let this_pin = ctx.pin_native_root(this);
            let str_obj = match ctx.get_field_by_name(this, "charC") {
                Value::Object(Some(char_c)) => {
                    if let Some((buff, start, end)) = char_chunk_parts(ctx, char_c) {
                        let mut units = Vec::with_capacity(end.saturating_sub(start));
                        for i in start..end {
                            units.push(ctx.get_array_element(buff, i).as_int().unwrap_or(0) as u16);
                        }
                        Some(ctx.create_string(&String::from_utf16_lossy(&units)))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "strValue", Value::Object(str_obj));
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(str_obj)))
        }
        _ => Ok(Some(ctx.get_field_by_name(this, "strValue"))),
    }
}

pub(crate) fn native_message_bytes_to_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let this_key = ctx.identity_hash_code(this);
    let ty = ctx.get_field_by_name(this, "type").as_int().unwrap_or(0);

    if ty == 0 {
        message_bytes_to_chars_cache()
            .lock()
            .unwrap()
            .remove(&this_key);
        if let Value::Object(Some(char_c)) = ctx.get_field_by_name(this, "charC") {
            let _ = ctx.invoke_virtual(char_c, "recycle", "()V", &[])?;
        }
        return Ok(None);
    }
    if ty == 3 {
        return Ok(None);
    }
    if ty == 2 {
        let _ = ctx.invoke_virtual(this, "toString", "()Ljava/lang/String;", &[])?;
    } else if ty != 1 {
        return Ok(None);
    }

    let str_obj = match ctx.get_field_by_name(this, "strValue") {
        Value::Object(Some(o)) => o,
        _ => {
            message_bytes_to_chars_cache()
                .lock()
                .unwrap()
                .remove(&this_key);
            return Ok(None);
        }
    };
    let str_key = ctx.identity_hash_code(str_obj);
    let cached = message_bytes_to_chars_cache()
        .lock()
        .unwrap()
        .get(&this_key)
        .copied();
    if let Some((cached_str_key, cached_len)) = cached {
        if cached_str_key == str_key {
            if let Value::Object(Some(char_c)) = ctx.get_field_by_name(this, "charC") {
                if let Some((_, start, end)) = char_chunk_parts(ctx, char_c) {
                    if end.saturating_sub(start) == cached_len {
                        return Ok(None);
                    }
                }
            }
        }
    }

    let Some(s) = ctx.read_string(str_obj) else {
        message_bytes_to_chars_cache()
            .lock()
            .unwrap()
            .remove(&this_key);
        return Ok(None);
    };
    let char_c = match ctx.get_field_by_name(this, "charC") {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    set_char_chunk_from_units(ctx, char_c, &units);
    message_bytes_to_chars_cache()
        .lock()
        .unwrap()
        .insert(this_key, (str_key, units.len()));
    Ok(None)
}

pub(crate) fn native_netty_mpsc_offer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    netty_queue_drain_retired(ctx);
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let elem = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem_key = ctx.identity_hash_code(elem);
    let q_class = if netty_queue_dbg_enabled() {
        Some(netty_queue_obj_class(ctx, this))
    } else {
        None
    };
    let elem_class = if netty_queue_dbg_enabled() {
        Some(netty_queue_obj_class(ctx, elem))
    } else {
        None
    };
    let entry = netty_queue_entry(ctx, elem);
    let key = ctx.identity_hash_code(this);
    let len = {
        let mut store = netty_jctools_queue_store().lock().unwrap();
        let q = store.entry(key).or_default();
        q.push_back(entry);
        q.len()
    };
    if netty_queue_dbg_enabled() {
        eprintln!(
            "[NETTYQ] offer q={key} qcls={} elem={elem_key} ecls={} len={len} thread={}",
            q_class.unwrap_or_default(),
            elem_class.unwrap_or_default(),
            netty_queue_thread_name()
        );
    }
    std::thread::yield_now();
    Ok(Some(Value::Int(1)))
}

fn native_netty_mpsc_test(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let first = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let second = match args.get(2).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let first = netty_queue_entry(ctx, first);
    let second = netty_queue_entry(ctx, second);
    let key = ctx.identity_hash_code(this);
    let mut store = netty_jctools_queue_store().lock().unwrap();
    let q = store.entry(key).or_default();
    q.push_back(first);
    q.push_back(second);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_netty_mpsc_poll(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    netty_queue_drain_retired(ctx);
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let key = ctx.identity_hash_code(this);
    let (entry, len_after) = {
        let mut store = netty_jctools_queue_store().lock().unwrap();
        if let Some(q) = store.get_mut(&key) {
            let entry = q.pop_front();
            (entry, q.len())
        } else {
            (None, 0)
        }
    };
    let elem = entry.and_then(|e| {
        let obj = netty_queue_resolve(ctx, e);
        if netty_queue_dbg_enabled() {
            let (elem_key, elem_class) = obj
                .map(|o| (ctx.identity_hash_code(o), netty_queue_obj_class(ctx, o)))
                .unwrap_or((0, "null".to_string()));
            eprintln!(
                "[NETTYQ] poll q={key} elem={elem_key} ecls={elem_class} len={len_after} root={} thread={}",
                e.root,
                netty_queue_thread_name()
            );
        }
        netty_queue_retire(e);
        obj
    });
    Ok(Some(Value::Object(elem)))
}

pub(crate) fn native_netty_mpsc_peek(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let key = ctx.identity_hash_code(this);
    let entry = {
        let store = netty_jctools_queue_store().lock().unwrap();
        store.get(&key).and_then(|q| q.front().copied())
    };
    let elem = entry.and_then(|e| netty_queue_resolve(ctx, e));
    Ok(Some(Value::Object(elem)))
}

pub(crate) fn native_netty_mpsc_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let key = ctx.identity_hash_code(this);
    let store = netty_jctools_queue_store().lock().unwrap();
    Ok(Some(Value::Int(
        store.get(&key).map(|q| q.len()).unwrap_or(0) as i32,
    )))
}

pub(crate) fn native_netty_mpsc_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(1)));
    };
    let key = ctx.identity_hash_code(this);
    let store = netty_jctools_queue_store().lock().unwrap();
    let empty = store.get(&key).map(|q| q.is_empty()).unwrap_or(true);
    Ok(Some(Value::Int(if empty { 1 } else { 0 })))
}

pub(crate) fn native_netty_mpsc_clear(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let key = ctx.identity_hash_code(this);
        let drained = {
            let mut store = netty_jctools_queue_store().lock().unwrap();
            store
                .get_mut(&key)
                .map(|q| q.drain(..).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        for entry in drained {
            netty_queue_release(ctx, entry);
        }
    }
    Ok(None)
}

fn native_netty_mpsc_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let target = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = ctx.identity_hash_code(this);
    let removed = {
        let mut store = netty_jctools_queue_store().lock().unwrap();
        let Some(q) = store.get_mut(&key) else {
            return Ok(Some(Value::Int(0)));
        };
        let mut found = None;
        for (idx, entry) in q.iter().copied().enumerate() {
            if netty_queue_resolve(ctx, entry) == Some(target) {
                found = Some(idx);
                break;
            }
        }
        found.and_then(|pos| q.remove(pos))
    };
    if let Some(entry) = removed {
        netty_queue_release(ctx, entry);
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

fn native_reactor_mono_just(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let value = args.first().copied().unwrap_or(Value::Object(None));
    // Reactor's Mono.just rejects null via Objects.requireNonNull in MonoJust's
    // constructor. Preserve that behavior by letting the real constructor run.
    let mono = ctx.new_object_initialized(
        "reactor/core/publisher/MonoJust",
        "(Ljava/lang/Object;)V",
        &[value],
    )?;
    Ok(mono)
}

pub(crate) fn register_reactor_core_bridges(registry: &mut NativeMethodRegistry) {
    registry.register(
        "reactor/core/publisher/Mono",
        "just",
        "(Ljava/lang/Object;)Lreactor/core/publisher/Mono;",
        native_reactor_mono_just,
    );
}

fn native_activemq_abstract_subscription_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let broker = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let context = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let info = match args.get(3).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let broker_pin = ctx.pin_native_root(broker);
    let context_pin = ctx.pin_native_root(context);
    let info_pin = ctx.pin_native_root(info);

    let destinations =
        match ctx.new_object_initialized("java/util/concurrent/CopyOnWriteArrayList", "()V", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            Ok(_) => {
                ctx.unpin_native_roots(this_pin);
                return Ok(None);
            }
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
    let destinations_pin = ctx.pin_native_root(destinations);

    let prefetch_extension = match ctx.new_object_initialized(
        "java/util/concurrent/atomic/AtomicInteger",
        "(I)V",
        &[Value::Int(0)],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let prefetch_pin = ctx.pin_native_root(prefetch_extension);

    let stats = match ctx.new_object_initialized(
        "org/apache/activemq/broker/region/SubscriptionStatistics",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let stats_pin = ctx.pin_native_root(stats);

    let info_now = ctx.read_native_pin(info_pin, info);
    let destination = match ctx.invoke_virtual(
        info_now,
        "getDestination",
        "()Lorg/apache/activemq/command/ActiveMQDestination;",
        &[],
    ) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let destination_pin = match destination {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let destination = match destination_pin {
        Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
        None => Value::Object(None),
    };

    let destination_filter = match ctx.invoke(
        "org/apache/activemq/filter/DestinationFilter",
        "parseFilter",
        "(Lorg/apache/activemq/command/ActiveMQDestination;)Lorg/apache/activemq/filter/DestinationFilter;",
        &[destination],
    ) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let destination_filter_pin = match destination_filter {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    let info_now = ctx.read_native_pin(info_pin, info);
    let selector = match ctx.invoke_virtual(info_now, "getSelector", "()Ljava/lang/String;", &[]) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let selector_pin = match selector {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    // `info_now` was derived from `info_pin` before the `getSelector()` call
    // above, which can collect -- re-derive rather than reuse the stale copy.
    let info_now = ctx.read_native_pin(info_pin, info);
    let no_local = match ctx.invoke_virtual(info_now, "isNoLocal", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v != 0,
        Ok(_) => false,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let additional = match ctx.invoke_virtual(
        info_now,
        "getAdditionalPredicate",
        "()Lorg/apache/activemq/filter/BooleanExpression;",
        &[],
    ) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let additional_pin = match additional {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    let selector_expression = if matches!(selector, Value::Object(Some(_)))
        || no_local
        || matches!(additional, Value::Object(Some(_)))
    {
        let info_now = ctx.read_native_pin(info_pin, info);
        match ctx.invoke(
            "org/apache/activemq/broker/region/AbstractSubscription",
            "parseSelector",
            "(Lorg/apache/activemq/command/ConsumerInfo;)Lorg/apache/activemq/filter/BooleanExpression;",
            &[Value::Object(Some(info_now))],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    } else {
        Value::Object(None)
    };
    let selector_expression_pin = match selector_expression {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    let this = ctx.read_native_pin(this_pin, this);
    let broker = ctx.read_native_pin(broker_pin, broker);
    let context = ctx.read_native_pin(context_pin, context);
    let info = ctx.read_native_pin(info_pin, info);
    let destinations = ctx.read_native_pin(destinations_pin, destinations);
    let prefetch_extension = ctx.read_native_pin(prefetch_pin, prefetch_extension);
    let stats = ctx.read_native_pin(stats_pin, stats);
    let destination_filter = match destination_filter_pin {
        Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
        None => Value::Object(None),
    };
    let selector_expression = match selector_expression_pin {
        Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
        None => Value::Object(None),
    };

    let abstract_subscription = "org/apache/activemq/broker/region/AbstractSubscription";
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "broker",
        Value::Object(Some(broker)),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "context",
        Value::Object(Some(context)),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "info",
        Value::Object(Some(info)),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "destinationFilter",
        destination_filter,
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "destinations",
        Value::Object(Some(destinations)),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "prefetchExtension",
        Value::Object(Some(prefetch_extension)),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "usePrefetchExtension",
        Value::Int(1),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "selectorExpression",
        selector_expression,
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "objectName",
        Value::Object(None),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "cursorMemoryHighWaterMark",
        Value::Int(70),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "slowConsumer",
        Value::Int(0),
    );
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "lastAckTime",
        Value::Long(now_ms),
    );
    set_declared_field(
        ctx,
        this,
        abstract_subscription,
        "subscriptionStatistics",
        Value::Object(Some(stats)),
    );

    let _ = selector_pin;
    let _ = additional_pin;
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_activemq_topic_subscription_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let broker = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let context = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let info = match args.get(3).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let usage_manager = match args.get(4).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let broker_pin = ctx.pin_native_root(broker);
    let context_pin = ctx.pin_native_root(context);
    let info_pin = ctx.pin_native_root(info);
    let usage_pin = ctx.pin_native_root(usage_manager);

    if let Err(e) = native_activemq_abstract_subscription_init(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(broker)),
            Value::Object(Some(context)),
            Value::Object(Some(info)),
        ],
    ) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }

    macro_rules! new_obj {
        ($class:expr, $desc:expr, $args:expr) => {
            match ctx.new_object_initialized($class, $desc, $args) {
                Ok(Some(Value::Object(Some(o)))) => o,
                Ok(_) => {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(None);
                }
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        };
    }

    let eviction = new_obj!(
        "org/apache/activemq/broker/region/policy/OldestMessageEvictionStrategy",
        "()V",
        &[]
    );
    let eviction_pin = ctx.pin_native_root(eviction);
    let discarded = new_obj!(
        "java/util/concurrent/atomic/AtomicInteger",
        "(I)V",
        &[Value::Int(0)]
    );
    let discarded_pin = ctx.pin_native_root(discarded);
    let matched_list_mutex = new_obj!("java/lang/Object", "()V", &[]);
    let matched_list_mutex_pin = ctx.pin_native_root(matched_list_mutex);
    let dispatch_lock = new_obj!("java/lang/Object", "()V", &[]);
    let dispatch_lock_pin = ctx.pin_native_root(dispatch_lock);
    let dispatched = new_obj!("java/util/ArrayList", "()V", &[]);
    let dispatched_pin = ctx.pin_native_root(dispatched);
    let current_dispatched_count = new_obj!(
        "java/util/concurrent/atomic/AtomicInteger",
        "(I)V",
        &[Value::Int(0)]
    );
    let current_dispatched_count_pin = ctx.pin_native_root(current_dispatched_count);
    let matched = new_obj!(
        "org/apache/activemq/broker/region/cursors/VMPendingMessageCursor",
        "(Z)V",
        &[Value::Int(0)]
    );
    let matched_pin = ctx.pin_native_root(matched);

    let broker_now = ctx.read_native_pin(broker_pin, broker);
    let scheduler = ctx
        .invoke_virtual(
            broker_now,
            "getScheduler",
            "()Lorg/apache/activemq/thread/Scheduler;",
            &[],
        )
        .ok()
        .flatten()
        .unwrap_or(Value::Object(None));
    let scheduler_pin = match scheduler {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    let this = ctx.read_native_pin(this_pin, this);
    let usage_manager = ctx.read_native_pin(usage_pin, usage_manager);
    let eviction = ctx.read_native_pin(eviction_pin, eviction);
    let discarded = ctx.read_native_pin(discarded_pin, discarded);
    let matched_list_mutex = ctx.read_native_pin(matched_list_mutex_pin, matched_list_mutex);
    let dispatch_lock = ctx.read_native_pin(dispatch_lock_pin, dispatch_lock);
    let dispatched = ctx.read_native_pin(dispatched_pin, dispatched);
    let current_dispatched_count =
        ctx.read_native_pin(current_dispatched_count_pin, current_dispatched_count);
    let matched = ctx.read_native_pin(matched_pin, matched);
    let scheduler = match scheduler_pin {
        Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
        None => Value::Object(None),
    };

    let topic = "org/apache/activemq/broker/region/TopicSubscription";
    set_declared_field(ctx, this, topic, "singleDestination", Value::Int(1));
    set_declared_field(ctx, this, topic, "destination", Value::Object(None));
    set_declared_field(ctx, this, topic, "scheduler", scheduler);
    set_declared_field(ctx, this, topic, "maximumPendingMessages", Value::Int(-1));
    set_declared_field(
        ctx,
        this,
        topic,
        "messageEvictionStrategy",
        Value::Object(Some(eviction)),
    );
    set_declared_field(
        ctx,
        this,
        topic,
        "discarded",
        Value::Object(Some(discarded)),
    );
    set_declared_field(
        ctx,
        this,
        topic,
        "matchedListMutex",
        Value::Object(Some(matched_list_mutex)),
    );
    set_declared_field(ctx, this, topic, "memoryUsageHighWaterMark", Value::Int(95));
    set_declared_field(ctx, this, topic, "maxProducersToAudit", Value::Int(1024));
    set_declared_field(ctx, this, topic, "maxAuditDepth", Value::Int(1000));
    set_declared_field(ctx, this, topic, "enableAudit", Value::Int(0));
    set_declared_field(ctx, this, topic, "audit", Value::Object(None));
    set_declared_field(ctx, this, topic, "active", Value::Int(0));
    set_declared_field(ctx, this, topic, "discarding", Value::Int(0));
    set_declared_field(
        ctx,
        this,
        topic,
        "useTopicSubscriptionInflightStats",
        Value::Int(1),
    );
    set_declared_field(
        ctx,
        this,
        topic,
        "dispatchLock",
        Value::Object(Some(dispatch_lock)),
    );
    set_declared_field(
        ctx,
        this,
        topic,
        "dispatched",
        Value::Object(Some(dispatched)),
    );
    set_declared_field(
        ctx,
        this,
        topic,
        "currentDispatchedCount",
        Value::Object(Some(current_dispatched_count)),
    );
    set_declared_field(
        ctx,
        this,
        topic,
        "usageManager",
        Value::Object(Some(usage_manager)),
    );
    set_declared_field(ctx, this, topic, "matched", Value::Object(Some(matched)));

    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_activemq_stomp_subscription_on_message_dispatch(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let dispatch = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let dispatch_pin = ctx.pin_native_root(dispatch);

    let message = match ctx.invoke_virtual(
        dispatch,
        "getMessage",
        "()Lorg/apache/activemq/command/Message;",
        &[],
    ) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let message = match message {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    let message_pin = ctx.pin_native_root(message);

    let this = ctx.read_native_pin(this_pin, this);
    let protocol_converter = match ctx.get_field_by_name(this, "protocolConverter") {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    let protocol_pin = ctx.pin_native_root(protocol_converter);

    let is_auto_ack = match ctx.invoke_virtual(this, "isAutoAck", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v != 0,
        Ok(_) => true,
        Err(_) => true,
    };

    if is_auto_ack {
        let dispatch = ctx.read_native_pin(dispatch_pin, dispatch);
        let ack = match ctx.new_object_initialized(
            "org/apache/activemq/command/MessageAck",
            "(Lorg/apache/activemq/command/MessageDispatch;BI)V",
            &[Value::Object(Some(dispatch)), Value::Int(2), Value::Int(1)],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            Ok(_) => {
                ctx.unpin_native_roots(this_pin);
                return Ok(None);
            }
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let ack_pin = ctx.pin_native_root(ack);
        let protocol_converter = ctx.read_native_pin(protocol_pin, protocol_converter);
        if let Ok(Some(Value::Object(Some(transport)))) = ctx.invoke_virtual(
            protocol_converter,
            "getStompTransport",
            "()Lorg/apache/activemq/transport/stomp/StompTransport;",
            &[],
        ) {
            let ack = ctx.read_native_pin(ack_pin, ack);
            let send_result = ctx.invoke_virtual(
                transport,
                "sendToActiveMQ",
                "(Lorg/apache/activemq/command/Command;)V",
                &[Value::Object(Some(ack))],
            );
            if let Err(e) = send_result {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }

    let protocol_converter = ctx.read_native_pin(protocol_pin, protocol_converter);
    let message = ctx.read_native_pin(message_pin, message);
    let frame = match ctx.invoke_virtual(
        protocol_converter,
        "convertMessage",
        "(Lorg/apache/activemq/command/ActiveMQMessage;Z)Lorg/apache/activemq/transport/stomp/StompFrame;",
        &[Value::Object(Some(message)), Value::Int(0)],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let frame_pin = ctx.pin_native_root(frame);

    let action = ctx.create_string("MESSAGE");
    let action_pin = ctx.pin_native_root(action);
    let frame = ctx.read_native_pin(frame_pin, frame);
    let action = ctx.read_native_pin(action_pin, action);
    if let Err(e) = ctx.invoke_virtual(
        frame,
        "setAction",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(action))],
    ) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }

    let this = ctx.read_native_pin(this_pin, this);
    let subscription_id = ctx.get_field_by_name(this, "subscriptionId");
    let subscription_pin = match subscription_id {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    if let Some((sub_pin, sub_fallback)) = subscription_pin {
        let frame = ctx.read_native_pin(frame_pin, frame);
        if let Ok(Some(Value::Object(Some(headers)))) =
            ctx.invoke_virtual(frame, "getHeaders", "()Ljava/util/Map;", &[])
        {
            let key = ctx.create_string("subscription");
            let key_pin = ctx.pin_native_root(key);
            let key = ctx.read_native_pin(key_pin, key);
            let subscription_id = ctx.read_native_pin(sub_pin, sub_fallback);
            let put_result = ctx.invoke_virtual(
                headers,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[
                    Value::Object(Some(key)),
                    Value::Object(Some(subscription_id)),
                ],
            );
            if let Err(e) = put_result {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }

    let protocol_converter = ctx.read_native_pin(protocol_pin, protocol_converter);
    let frame = ctx.read_native_pin(frame_pin, frame);
    match ctx.invoke_virtual(
        protocol_converter,
        "getStompTransport",
        "()Lorg/apache/activemq/transport/stomp/StompTransport;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(transport)))) => {
            if let Err(e) = ctx.invoke_virtual(
                transport,
                "sendToStomp",
                "(Lorg/apache/activemq/transport/stomp/StompFrame;)V",
                &[Value::Object(Some(frame))],
            ) {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
        Ok(_) => {}
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }

    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_spring_stomp_encoder_encode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let message = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Ok(Some(Value::Object(Some(spring_bytes_to_java_array(
                ctx,
                &[],
            )))))
        }
    };
    let headers = match ctx.invoke_virtual(
        message,
        "getHeaders",
        "()Lorg/springframework/messaging/MessageHeaders;",
        &[],
    )? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Ok(Some(Value::Object(Some(spring_bytes_to_java_array(
                ctx,
                &[],
            )))))
        }
    };
    let command_value = spring_map_get_key(ctx, headers, "stompCommand")?;
    let command = match command_value {
        Value::Object(Some(cmd)) => {
            match ctx.invoke_virtual(cmd, "name", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name).unwrap_or_default(),
                _ => spring_value_to_string(ctx, Value::Object(Some(cmd))).unwrap_or_default(),
            }
        }
        _ => String::new(),
    };
    if command.is_empty() {
        return Ok(Some(Value::Object(Some(spring_bytes_to_java_array(
            ctx,
            &[],
        )))));
    }

    let native_headers = match spring_map_get_key(ctx, headers, "nativeHeaders")? {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let destination = match spring_native_header_first(ctx, native_headers, "destination") {
        Some(v) => Some(v),
        None => spring_map_get_key(ctx, headers, "simpDestination")
            .ok()
            .and_then(|v| spring_value_to_string(ctx, v)),
    };
    let subscription_id = match spring_native_header_first(ctx, native_headers, "id") {
        Some(v) => Some(v),
        None => spring_map_get_key(ctx, headers, "simpSubscriptionId")
            .ok()
            .and_then(|v| spring_value_to_string(ctx, v)),
    };
    let receipt = spring_native_header_first(ctx, native_headers, "receipt");
    let accept_version = spring_native_header_first(ctx, native_headers, "accept-version")
        .unwrap_or_else(|| "1.1,1.2".to_string());
    let heartbeat = spring_native_header_first(ctx, native_headers, "heart-beat")
        .unwrap_or_else(|| "10000,10000".to_string());
    let host = spring_native_header_first(ctx, native_headers, "host");
    let login = spring_native_header_first(ctx, native_headers, "login");
    let passcode = spring_native_header_first(ctx, native_headers, "passcode");
    let ack = spring_native_header_first(ctx, native_headers, "ack");
    let content_type = match spring_native_header_first(ctx, native_headers, "content-type") {
        Some(v) => Some(v),
        None => spring_map_get_key(ctx, headers, "contentType")
            .ok()
            .and_then(|v| spring_value_to_string(ctx, v)),
    };

    let payload = ctx
        .invoke_virtual(message, "getPayload", "()Ljava/lang/Object;", &[])?
        .unwrap_or(Value::Object(None));
    let payload = spring_payload_bytes(ctx, payload);

    let mut frame = Vec::new();
    frame.extend_from_slice(command.as_bytes());
    frame.push(b'\n');
    match command.as_str() {
        "CONNECT" | "STOMP" => {
            frame.extend_from_slice(format!("heart-beat:{}\n", heartbeat).as_bytes());
            frame.extend_from_slice(format!("accept-version:{}\n", accept_version).as_bytes());
            if let Some(v) = host {
                frame.extend_from_slice(format!("host:{}\n", v).as_bytes());
            }
            if let Some(v) = login {
                frame.extend_from_slice(format!("login:{}\n", v).as_bytes());
            }
            if let Some(v) = passcode {
                frame.extend_from_slice(format!("passcode:{}\n", v).as_bytes());
            }
        }
        "SUBSCRIBE" => {
            if let Some(v) = subscription_id {
                frame.extend_from_slice(format!("id:{}\n", v).as_bytes());
            }
            if let Some(v) = destination {
                frame.extend_from_slice(format!("destination:{}\n", v).as_bytes());
            }
            if let Some(v) = ack {
                frame.extend_from_slice(format!("ack:{}\n", v).as_bytes());
            }
            if let Some(v) = receipt {
                frame.extend_from_slice(format!("receipt:{}\n", v).as_bytes());
            }
        }
        "SEND" => {
            if let Some(v) = destination {
                frame.extend_from_slice(format!("destination:{}\n", v).as_bytes());
            }
            if let Some(v) = content_type {
                frame.extend_from_slice(format!("content-type:{}\n", v).as_bytes());
            }
            frame.extend_from_slice(format!("content-length:{}\n", payload.len()).as_bytes());
            if let Some(v) = receipt {
                frame.extend_from_slice(format!("receipt:{}\n", v).as_bytes());
            }
        }
        "DISCONNECT" => {
            if let Some(v) = receipt {
                frame.extend_from_slice(format!("receipt:{}\n", v).as_bytes());
            }
        }
        _ => {
            if let Some(v) = destination {
                frame.extend_from_slice(format!("destination:{}\n", v).as_bytes());
            }
            if let Some(v) = receipt {
                frame.extend_from_slice(format!("receipt:{}\n", v).as_bytes());
            }
        }
    }
    frame.push(b'\n');
    frame.extend_from_slice(&payload);
    frame.push(0);

    Ok(Some(Value::Object(Some(spring_bytes_to_java_array(
        ctx, &frame,
    )))))
}

fn native_spring_reactor_netty_tcp_connection_send_async(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let message = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };

    let this_pin = ctx.pin_native_root(this);
    let message_pin = ctx.pin_native_root(message);
    let this = ctx.read_native_pin(this_pin, this);

    let outbound = match ctx.get_field_by_name(this, "outbound") {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let outbound_pin = ctx.pin_native_root(outbound);
    let codec = match ctx.get_field_by_name(this, "codec") {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let codec_pin = ctx.pin_native_root(codec);

    let allocator = match ctx.invoke_virtual(
        outbound,
        "alloc",
        "()Lio/netty/buffer/ByteBufAllocator;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let allocator_pin = ctx.pin_native_root(allocator);

    let allocator = ctx.read_native_pin(allocator_pin, allocator);
    let byte_buf = match ctx.invoke_virtual(allocator, "buffer", "()Lio/netty/buffer/ByteBuf;", &[])
    {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let byte_buf_pin = ctx.pin_native_root(byte_buf);

    let codec = ctx.read_native_pin(codec_pin, codec);
    let message = ctx.read_native_pin(message_pin, message);
    let byte_buf = ctx.read_native_pin(byte_buf_pin, byte_buf);
    let mut direct_written = false;
    if let Value::Object(Some(encoder)) = ctx.get_field_by_name(codec, "encoder") {
        let encoder_pin = ctx.pin_native_root(encoder);
        let encoder = ctx.read_native_pin(encoder_pin, encoder);
        let encoded = match native_spring_stomp_encoder_encode(
            ctx,
            &[Value::Object(Some(encoder)), Value::Object(Some(message))],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        if let Value::Object(Some(bytes)) = encoded {
            let bytes_pin = ctx.pin_native_root(bytes);
            let bytes = ctx.read_native_pin(bytes_pin, bytes);
            let len = ctx.array_length(bytes);
            if len >= 8 {
                let mut prefix = [0u8; 8];
                for i in 0..8 {
                    prefix[i] = match ctx.get_array_element(bytes, i) {
                        Value::Int(v) => v as u8,
                        _ => 0,
                    };
                }
                if &prefix == b"CONNECT\n" {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
            let outbound_for_direct = ctx.read_native_pin(outbound_pin, outbound);
            if let Ok(Some(Value::Object(Some(channel)))) = ctx.invoke_virtual(
                outbound_for_direct,
                "channel",
                "()Lio/netty/channel/Channel;",
                &[],
            ) {
                let channel_pin = ctx.pin_native_root(channel);
                let channel = ctx.read_native_pin(channel_pin, channel);
                let selectable = match ctx.invoke_virtual(
                    channel,
                    "javaChannel",
                    "()Ljava/nio/channels/SelectableChannel;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(o)))) => Some(o),
                    _ => match ctx.get_field_by_name(channel, "ch") {
                        Value::Object(Some(o)) => Some(o),
                        _ => None,
                    },
                };
                if let Some(socket_channel) = selectable {
                    let socket_pin = ctx.pin_native_root(socket_channel);
                    let bytes = ctx.read_native_pin(bytes_pin, bytes);
                    if let Ok(Some(Value::Object(Some(buffer)))) = ctx.invoke(
                        "java/nio/ByteBuffer",
                        "wrap",
                        "([B)Ljava/nio/ByteBuffer;",
                        &[Value::Object(Some(bytes))],
                    ) {
                        let buffer_pin = ctx.pin_native_root(buffer);
                        let mut wrote_all = false;
                        for _ in 0..1024 {
                            let buffer = ctx.read_native_pin(buffer_pin, buffer);
                            let has_remaining =
                                match ctx.invoke_virtual(buffer, "hasRemaining", "()Z", &[]) {
                                    Ok(Some(Value::Int(v))) => v != 0,
                                    Ok(_) => false,
                                    Err(e) => {
                                        ctx.unpin_native_roots(this_pin);
                                        return Err(e);
                                    }
                                };
                            if !has_remaining {
                                wrote_all = true;
                                break;
                            }
                            let socket_channel = ctx.read_native_pin(socket_pin, socket_channel);
                            let buffer = ctx.read_native_pin(buffer_pin, buffer);
                            match ctx.invoke_virtual(
                                socket_channel,
                                "write",
                                "(Ljava/nio/ByteBuffer;)I",
                                &[Value::Object(Some(buffer))],
                            ) {
                                Ok(Some(Value::Int(n))) if n > 0 => {}
                                Ok(_) => std::thread::yield_now(),
                                Err(e) => {
                                    ctx.unpin_native_roots(this_pin);
                                    return Err(e);
                                }
                            }
                        }
                        if wrote_all {
                            let byte_buf = ctx.read_native_pin(byte_buf_pin, byte_buf);
                            let _ = ctx.invoke_virtual(byte_buf, "release", "()Z", &[]);
                            direct_written = true;
                        }
                    }
                }
            }
            if !direct_written {
                for i in 0..len {
                    let b = match ctx.get_array_element(bytes, i) {
                        Value::Int(v) => v & 0xff,
                        _ => 0,
                    };
                    let byte_buf = ctx.read_native_pin(byte_buf_pin, byte_buf);
                    if let Err(e) = ctx.invoke_virtual(
                        byte_buf,
                        "writeByte",
                        "(I)Lio/netty/buffer/ByteBuf;",
                        &[Value::Int(b)],
                    ) {
                        ctx.unpin_native_roots(this_pin);
                        return Err(e);
                    }
                }
            }
        }
    } else if let Err(e) = ctx.invoke_virtual(
        codec,
        "encode",
        "(Lorg/springframework/messaging/Message;Lio/netty/buffer/ByteBuf;)V",
        &[Value::Object(Some(message)), Value::Object(Some(byte_buf))],
    ) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }

    let byte_buf = ctx.read_native_pin(byte_buf_pin, byte_buf);
    let outbound = ctx.read_native_pin(outbound_pin, outbound);
    if !direct_written {
        match ctx.invoke_virtual(outbound, "channel", "()Lio/netty/channel/Channel;", &[]) {
            Ok(Some(Value::Object(Some(channel)))) => {
                let write_result = ctx.invoke_virtual(
                    channel,
                    "writeAndFlush",
                    "(Ljava/lang/Object;)Lio/netty/channel/ChannelFuture;",
                    &[Value::Object(Some(byte_buf))],
                );
                if let Err(e) = write_result {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
            Ok(_) => {}
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }

    let future = match ctx.invoke(
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(None)],
    ) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(_) => {
            match ctx.new_object_initialized("java/util/concurrent/CompletableFuture", "()V", &[]) {
                Ok(Some(Value::Object(Some(f)))) => {
                    let _ = ctx.invoke_virtual(
                        f,
                        "complete",
                        "(Ljava/lang/Object;)Z",
                        &[Value::Object(None)],
                    );
                    Value::Object(Some(f))
                }
                _ => Value::Object(None),
            }
        }
    };

    ctx.unpin_native_roots(this_pin);
    Ok(Some(future))
}

fn native_spring_default_stomp_session_execute(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let message = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let message_pin = ctx.pin_native_root(message);
    let this = ctx.read_native_pin(this_pin, this);
    let connection = match ctx.get_field_by_name(this, "connection") {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    let connection_pin = ctx.pin_native_root(connection);
    let connection = ctx.read_native_pin(connection_pin, connection);
    let message = ctx.read_native_pin(message_pin, message);
    let result = ctx.invoke_virtual(
        connection,
        "sendAsync",
        "(Lorg/springframework/messaging/Message;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(message))],
    );
    ctx.unpin_native_roots(this_pin);
    result.map(|_| None)
}

fn native_activemq_abstract_pending_message_cursor_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let prioritized = match args.get(1).copied() {
        Some(Value::Int(v)) => v != 0,
        _ => false,
    };
    let class_name = "org/apache/activemq/broker/region/cursors/AbstractPendingMessageCursor";
    set_declared_field(
        ctx,
        this,
        class_name,
        "memoryUsageHighWaterMark",
        Value::Int(70),
    );
    set_declared_field(ctx, this, class_name, "maxBatchSize", Value::Int(200));
    set_declared_field(ctx, this, class_name, "systemUsage", Value::Object(None));
    set_declared_field(ctx, this, class_name, "maxProducersToAudit", Value::Int(64));
    set_declared_field(ctx, this, class_name, "maxAuditDepth", Value::Int(10000));
    set_declared_field(ctx, this, class_name, "enableAudit", Value::Int(1));
    set_declared_field(ctx, this, class_name, "audit", Value::Object(None));
    set_declared_field(ctx, this, class_name, "useCache", Value::Int(1));
    set_declared_field(ctx, this, class_name, "cacheEnabled", Value::Int(1));
    set_declared_field(ctx, this, class_name, "started", Value::Int(0));
    set_declared_field(ctx, this, class_name, "last", Value::Object(None));
    set_declared_field(
        ctx,
        this,
        class_name,
        "prioritizedMessages",
        Value::Int(if prioritized { 1 } else { 0 }),
    );
    Ok(None)
}

fn native_activemq_ordered_pending_list_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let this_pin = ctx.pin_native_root(this);

    let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let map_pin = ctx.pin_native_root(map);
    let name = ctx.create_string("messageSize");
    let name_pin = ctx.pin_native_root(name);
    let desc = ctx.create_string("The size in bytes of the pending messages");
    let desc_pin = ctx.pin_native_root(desc);
    let name = ctx.read_native_pin(name_pin, name);
    let desc = ctx.read_native_pin(desc_pin, desc);
    let message_size = match ctx.new_object_initialized(
        "org/apache/activemq/management/SizeStatisticImpl",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[Value::Object(Some(name)), Value::Object(Some(desc))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let message_size_pin = ctx.pin_native_root(message_size);
    let message_size = ctx.read_native_pin(message_size_pin, message_size);
    if let Err(e) = ctx.invoke_virtual(message_size, "setEnabled", "(Z)V", &[Value::Int(1)]) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }

    let map = ctx.read_native_pin(map_pin, map);
    let message_size = ctx.read_native_pin(message_size_pin, message_size);
    let helper = match ctx.new_object_initialized(
        "org/apache/activemq/broker/region/cursors/PendingMessageHelper",
        "(Ljava/util/Map;Lorg/apache/activemq/management/SizeStatisticImpl;)V",
        &[Value::Object(Some(map)), Value::Object(Some(message_size))],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let helper_pin = ctx.pin_native_root(helper);

    let this = ctx.read_native_pin(this_pin, this);
    let map = ctx.read_native_pin(map_pin, map);
    let message_size = ctx.read_native_pin(message_size_pin, message_size);
    let helper = ctx.read_native_pin(helper_pin, helper);
    let class_name = "org/apache/activemq/broker/region/cursors/OrderedPendingList";
    set_declared_field(ctx, this, class_name, "root", Value::Object(None));
    set_declared_field(ctx, this, class_name, "tail", Value::Object(None));
    set_declared_field(ctx, this, class_name, "map", Value::Object(Some(map)));
    set_declared_field(
        ctx,
        this,
        class_name,
        "messageSize",
        Value::Object(Some(message_size)),
    );
    set_declared_field(
        ctx,
        this,
        class_name,
        "pendingMessageHelper",
        Value::Object(Some(helper)),
    );

    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_activemq_vm_pending_message_cursor_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let prioritized = args.get(1).copied().unwrap_or(Value::Int(0));
    let this_pin = ctx.pin_native_root(this);

    if let Err(e) = native_activemq_abstract_pending_message_cursor_init(
        ctx,
        &[Value::Object(Some(this)), prioritized],
    ) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }

    let list = match ctx.new_object_initialized(
        "org/apache/activemq/broker/region/cursors/OrderedPendingList",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let this = ctx.read_native_pin(this_pin, this);
    let list = ctx.read_native_pin(list_pin, list);
    let class_name = "org/apache/activemq/broker/region/cursors/VMPendingMessageCursor";
    set_declared_field(ctx, this, class_name, "list", Value::Object(Some(list)));
    set_declared_field(ctx, this, class_name, "iter", Value::Object(None));

    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_netty_pipeline_touch(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

fn native_activemq_mutex_transport_oneway(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let command = args.get(1).copied().unwrap_or(Value::Object(None));
    let next = match ctx.get_field_by_name(this, "next") {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    ctx.invoke_virtual(next, "oneway", "(Ljava/lang/Object;)V", &[command])?;
    Ok(None)
}

fn native_activemq_mutex_transport_on_command(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let command = args.get(1).copied().unwrap_or(Value::Object(None));
    let listener = match ctx.get_field_by_name(this, "transportListener") {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    ctx.invoke_virtual(listener, "onCommand", "(Ljava/lang/Object;)V", &[command])?;
    Ok(None)
}

fn native_activemq_message_get_object_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let map = match ctx.get_field_by_name(this, "properties") {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[name],
    )
}

fn native_activemq_message_get_string_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let value = native_activemq_message_get_object_property(ctx, args)?;
    match value {
        Some(Value::Object(Some(o))) => {
            ctx.invoke_virtual(o, "toString", "()Ljava/lang/String;", &[])
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

fn native_activemq_protocol_converter_on_active_mq_command(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let command = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let command_pin = ctx.pin_native_root(command);

    let is_response = match ctx.invoke_virtual(command, "isResponse", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v != 0,
        Ok(_) => false,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if is_response {
        let command = ctx.read_native_pin(command_pin, command);
        let correlation = match ctx.invoke_virtual(command, "getCorrelationId", "()I", &[]) {
            Ok(Some(Value::Int(v))) => v,
            Ok(_) => 0,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let key = match ctx.invoke(
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;",
            &[Value::Int(correlation)],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let key_pin = match key {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let this = ctx.read_native_pin(this_pin, this);
        if let Value::Object(Some(handlers)) = ctx.get_field_by_name(this, "resposeHandlers") {
            let key = match key_pin {
                Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
                None => Value::Object(None),
            };
            let handler = match ctx.invoke_virtual(
                handlers,
                "remove",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            ) {
                Ok(v) => v.unwrap_or(Value::Object(None)),
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            if let Value::Object(Some(handler)) = handler {
                let handler_pin = ctx.pin_native_root(handler);
                let this = ctx.read_native_pin(this_pin, this);
                let command = ctx.read_native_pin(command_pin, command);
                let handler = ctx.read_native_pin(handler_pin, handler);
                if let Err(e) = ctx.invoke_virtual(
                    handler,
                    "onResponse",
                    "(Lorg/apache/activemq/transport/stomp/ProtocolConverter;Lorg/apache/activemq/command/Response;)V",
                    &[Value::Object(Some(this)), Value::Object(Some(command))],
                ) {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        }
        ctx.unpin_native_roots(this_pin);
        return Ok(None);
    }

    let command = ctx.read_native_pin(command_pin, command);
    let is_dispatch = match ctx.invoke_virtual(command, "isMessageDispatch", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v != 0,
        Ok(_) => false,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if is_dispatch {
        let consumer_id = match ctx.invoke_virtual(
            command,
            "getConsumerId",
            "()Lorg/apache/activemq/command/ConsumerId;",
            &[],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let consumer_pin = match consumer_id {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        let this = ctx.read_native_pin(this_pin, this);
        if let Value::Object(Some(map)) = ctx.get_field_by_name(this, "subscriptionsByConsumerId") {
            let consumer_id = match consumer_pin {
                Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
                None => Value::Object(None),
            };
            let subscription = match ctx.invoke_virtual(
                map,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[consumer_id],
            ) {
                Ok(v) => v.unwrap_or(Value::Object(None)),
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            if let Value::Object(Some(subscription)) = subscription {
                let sub_pin = ctx.pin_native_root(subscription);
                let command = ctx.read_native_pin(command_pin, command);
                let subscription = ctx.read_native_pin(sub_pin, subscription);
                if let Err(e) = native_activemq_stomp_subscription_on_message_dispatch(
                    ctx,
                    &[
                        Value::Object(Some(subscription)),
                        Value::Object(Some(command)),
                    ],
                ) {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        }
    }

    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn activemq_topic_add_consumer_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn native_activemq_topic_region_add_consumer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let context = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let info = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };

    let _guard = activemq_topic_add_consumer_lock().lock().unwrap();
    ctx.invoke_special(
        "org/apache/activemq/broker/region/AbstractRegion",
        "addConsumer",
        "(Lorg/apache/activemq/broker/ConnectionContext;Lorg/apache/activemq/command/ConsumerInfo;)Lorg/apache/activemq/broker/region/Subscription;",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(context)),
            Value::Object(Some(info)),
        ],
    )
}

fn native_activemq_topic_subscription_init_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    let topic = "org/apache/activemq/broker/region/TopicSubscription";
    let abstract_sub = "org/apache/activemq/broker/region/AbstractSubscription";
    let abstract_cursor = "org/apache/activemq/broker/region/cursors/AbstractPendingMessageCursor";

    let matched = ctx.get_field_by_name(this, "matched");
    if let Value::Object(Some(cursor)) = matched {
        let usage = ctx.get_field_by_name(this, "usageManager");
        set_declared_field(ctx, cursor, abstract_cursor, "systemUsage", usage);
        let high_water = ctx.get_field_by_name(this, "cursorMemoryHighWaterMark");
        set_declared_field(
            ctx,
            cursor,
            abstract_cursor,
            "memoryUsageHighWaterMark",
            high_water,
        );
        set_declared_field(ctx, cursor, abstract_cursor, "started", Value::Int(1));
    }
    set_declared_field(ctx, this, topic, "audit", Value::Object(None));
    set_declared_field(ctx, this, topic, "active", Value::Int(1));
    // Keep AbstractSubscription's defaults explicit; the bytecode init path
    // only mutates TopicSubscription.active plus cursor state for this test.
    let _ = abstract_sub;
    Ok(None)
}

fn native_activemq_topic_region_create_subscription(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let context = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let info = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };

    let durable = match ctx.invoke_virtual(info, "isDurable", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v != 0,
        Ok(_) => false,
        Err(e) => return Err(e),
    };
    if durable {
        return Ok(Some(Value::Object(None)));
    }

    let this_pin = ctx.pin_native_root(this);
    let context_pin = ctx.pin_native_root(context);
    let info_pin = ctx.pin_native_root(info);
    let this = ctx.read_native_pin(this_pin, this);
    let broker = match ctx.get_field_by_name(this, "broker") {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let broker_pin = ctx.pin_native_root(broker);
    let usage = match ctx.get_field_by_name(this, "usageManager") {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let usage_pin = ctx.pin_native_root(usage);

    let broker = ctx.read_native_pin(broker_pin, broker);
    let context = ctx.read_native_pin(context_pin, context);
    let info = ctx.read_native_pin(info_pin, info);
    let usage = ctx.read_native_pin(usage_pin, usage);
    let subscription = match ctx.new_object_initialized(
        "org/apache/activemq/broker/region/TopicSubscription",
        "(Lorg/apache/activemq/broker/Broker;Lorg/apache/activemq/broker/ConnectionContext;Lorg/apache/activemq/command/ConsumerInfo;Lorg/apache/activemq/usage/SystemUsage;)V",
        &[
            Value::Object(Some(broker)),
            Value::Object(Some(context)),
            Value::Object(Some(info)),
            Value::Object(Some(usage)),
        ],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let sub_pin = ctx.pin_native_root(subscription);
    let subscription = ctx.read_native_pin(sub_pin, subscription);
    if let Err(e) =
        native_activemq_topic_subscription_init_method(ctx, &[Value::Object(Some(subscription))])
    {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let subscription = ctx.read_native_pin(sub_pin, subscription);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(subscription))))
}

pub(crate) fn register_spring_messaging_bridges(registry: &mut NativeMethodRegistry) {
    for descriptor in [
        "([BIILjava/lang/foreign/MemorySegment;)V",
        "([BIILjdk/internal/access/foreign/MemorySegmentProxy;)V",
    ] {
        registry.register(
            "java/nio/HeapByteBuffer",
            "<init>",
            descriptor,
            native_heap_byte_buffer_init_array_offset_len,
        );
    }
    registry.register(
        "java/nio/ByteBuffer",
        "wrap",
        "([B)Ljava/nio/ByteBuffer;",
        native_byte_buffer_wrap_bytes,
    );
    registry.register(
        "java/nio/ByteBuffer",
        "wrap",
        "([BII)Ljava/nio/ByteBuffer;",
        native_byte_buffer_wrap_bytes_offset_len,
    );
    registry.register(
        "io/netty/util/ReferenceCountUtil",
        "touch",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_return_first_object_arg,
    );
    registry.register(
        "io/netty/util/ReferenceCountUtil",
        "touch",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_return_first_object_arg,
    );
    registry.register(
        "io/netty/channel/DefaultChannelPipeline",
        "touch",
        "(Ljava/lang/Object;Lio/netty/channel/AbstractChannelHandlerContext;)Ljava/lang/Object;",
        native_netty_pipeline_touch,
    );
    registry.register(
        "org/apache/activemq/command/ActiveMQMessage",
        "getObjectProperty",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        native_activemq_message_get_object_property,
    );
    registry.register(
        "org/apache/activemq/command/ActiveMQMessage",
        "getStringProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_activemq_message_get_string_property,
    );
    registry.register(
        "org/apache/activemq/transport/MutexTransport",
        "onCommand",
        "(Ljava/lang/Object;)V",
        native_activemq_mutex_transport_on_command,
    );
    registry.register(
        "org/apache/activemq/transport/MutexTransport",
        "oneway",
        "(Ljava/lang/Object;)V",
        native_activemq_mutex_transport_oneway,
    );
    registry.register(
        "org/apache/activemq/broker/region/TopicRegion",
        "addConsumer",
        "(Lorg/apache/activemq/broker/ConnectionContext;Lorg/apache/activemq/command/ConsumerInfo;)Lorg/apache/activemq/broker/region/Subscription;",
        native_activemq_topic_region_add_consumer,
    );
    registry.register(
        "org/apache/activemq/broker/region/TopicSubscription",
        "init",
        "()V",
        native_activemq_topic_subscription_init_method,
    );
    registry.register(
        "org/apache/activemq/broker/region/TopicRegion",
        "createSubscription",
        "(Lorg/apache/activemq/broker/ConnectionContext;Lorg/apache/activemq/command/ConsumerInfo;)Lorg/apache/activemq/broker/region/Subscription;",
        native_activemq_topic_region_create_subscription,
    );
    registry.register(
        "org/apache/activemq/broker/region/cursors/AbstractPendingMessageCursor",
        "<init>",
        "(Z)V",
        native_activemq_abstract_pending_message_cursor_init,
    );
    registry.register(
        "org/apache/activemq/broker/region/cursors/OrderedPendingList",
        "<init>",
        "()V",
        native_activemq_ordered_pending_list_init,
    );
    registry.register(
        "org/apache/activemq/broker/region/cursors/VMPendingMessageCursor",
        "<init>",
        "(Z)V",
        native_activemq_vm_pending_message_cursor_init,
    );
    registry.register(
        "org/springframework/messaging/tcp/reactor/ReactorNettyTcpConnection",
        "sendAsync",
        "(Lorg/springframework/messaging/Message;)Ljava/util/concurrent/CompletableFuture;",
        native_spring_reactor_netty_tcp_connection_send_async,
    );
    registry.register(
        "org/apache/activemq/broker/region/TopicSubscription",
        "<init>",
        "(Lorg/apache/activemq/broker/Broker;Lorg/apache/activemq/broker/ConnectionContext;Lorg/apache/activemq/command/ConsumerInfo;Lorg/apache/activemq/usage/SystemUsage;)V",
        native_activemq_topic_subscription_init,
    );
    registry.register(
        "org/apache/activemq/broker/region/AbstractSubscription",
        "<init>",
        "(Lorg/apache/activemq/broker/Broker;Lorg/apache/activemq/broker/ConnectionContext;Lorg/apache/activemq/command/ConsumerInfo;)V",
        native_activemq_abstract_subscription_init,
    );
    // KEEP (deliberate, load-bearing constant) — audited 2026-07-27.
    // `isFull()` is the broker's dispatch gate. Its real body is a predicate
    // over `active`, `prefetchExtension` and `dispatched` — and the synthetic
    // `<init>` immediately above (`native_activemq_topic_subscription_init`)
    // parks `active = false` and never populates `prefetchExtension` at all,
    // so running the real bytecode here reports a permanently-full (or
    // null-dereferencing) subscription and the broker never dispatches a
    // single message. Reporting "not full" is what keeps dispatch alive.
    //
    // Known consequence, deliberately accepted: prefetch backpressure never
    // engages, so a consumer that stops acking will not throttle the broker.
    // Fixing this properly means teaching that `<init>` to build a real
    // `prefetchExtension` and flip `active`, at which point this registration
    // should be deleted rather than reimplemented.
    registry.register(
        "org/apache/activemq/broker/region/TopicSubscription",
        "isFull",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
}

/// Shut a Netty group down with a ZERO quiet period.
///
/// **Call this directly, from a bridge that owns the group.** It is deliberately
/// NOT registered against `EventExecutorGroup.shutdownGracefully()`: the only
/// caller is `native_springboot_mongo_reactive_customizer_destroy`, whose group
/// this VM created itself (in the matching `customize` bridge, with a
/// `cratonvm-mongo-reactive` daemon thread factory) and whose monitor callback
/// keeps re-enqueuing work so the ordinary two-second quiet period never
/// becomes quiet.
///
/// A registration would apply it to every Netty group in the process, because
/// `MultiThreadIoEventLoopGroup` — the class check below — is the ordinary
/// Netty 4.2 group, not a Mongo-specific one. That is what it used to do, and
/// a zero quiet period does not drain: Netty runs channel deregistration, and
/// therefore `handlerRemoved`, as a queued event-loop task. See
/// `netty_group_shutdown_gracefully_is_not_forced_to_a_zero_quiet_period`.
pub(crate) fn native_netty_event_executor_group_shutdown_gracefully(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
    if class_name.as_deref() != Some("io/netty/channel/MultiThreadIoEventLoopGroup") {
        return ctx.invoke_virtual_bytecode_only(
            this,
            "shutdownGracefully",
            "()Lio/netty/util/concurrent/Future;",
            &[],
        );
    }

    let time_unit = ctx.ensure_class_initialized("java/util/concurrent/TimeUnit")?;
    let milliseconds_field = ctx
        .static_field_index_by_name(time_unit, "MILLISECONDS")
        .ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "java/util/concurrent/TimeUnit.MILLISECONDS static field not found"
                    .to_string(),
            })
        })?;
    let milliseconds = ctx.get_static_field(time_unit, milliseconds_field);
    ctx.invoke_virtual_bytecode_only(
        this,
        "shutdownGracefully",
        "(JJLjava/util/concurrent/TimeUnit;)Lio/netty/util/concurrent/Future;",
        &[Value::Long(0), Value::Long(0), milliseconds],
    )
}

/// Netty's `netty-tcnative` uses the same APR-style JNI as Tomcat but under
/// `io.netty.internal.tcnative`. Windows host DLLs + libffi dispatch AV
/// (`0xC0000005`); `vm_exec` skips symbol lookup for this package — register
/// minimal stubs so `Library.initialize` / `SSL` static constants can load.
// JDK-ONLY-CLASSIFY: stub — stated for the whole registrar, not adjudicated
// per row. Every one of these was among the 200 registrations the real boot
// made with NO category scope over them, which `--dump-native-registry`
// could not report until `current_category` became an `Option`: the old
// `category_chosen` flag was set by the first `set_category` in boot and
// never cleared, so everything after it claimed to have been chosen.
// `SyntheticStub` is the kind these carried before and after — verified by
// a census A/B — and it is the right one on the merits: `io.netty.internal.tcnative` is a third-party JNI
// binding whose host DLL this VM refuses to load (see the fn doc above) —
// the registrations exist to keep `Library.<clinit>` from throwing.
pub(crate) fn register_netty_internal_tcnative_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let lib = "io/netty/internal/tcnative/Library";

    registry.register(lib, "version", "(I)I", |_ctx, args| {
        let what = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let v = match what {
            0x01 => 2,
            0x02 => 0,
            0x03 => 0,
            0x04 => 0,
            0x11 => 1,
            0x12 => 7,
            0x13 => 0,
            0x14 => 0,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });
    // KEEP (deliberate, load-bearing constants). `has`/`initialize0` must
    // both answer positively or `Library.<clinit>` throws and every class in
    // the io.netty.internal.tcnative package becomes unloadable — which is
    // the crash this whole registrar exists to avoid (see the fn doc above).
    registry.register(lib, "has", "(I)Z", |_ctx, _args| Ok(Some(Value::Int(1))));
    registry.register(lib, "initialize0", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    registry.register(
        lib,
        "aprVersionString",
        "()Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.create_string("1.7.0-cratonvm-netty-stub"),
            ))))
        },
    );

    const NETTY_STATIC_INT_NATIVES: &[&str] = &[
        "sslOpCipherServerPreference",
        "sslOpNoSSLv2",
        "sslOpNoSSLv3",
        "sslOpNoTLSv1",
        "sslOpNoTLSv11",
        "sslOpNoTLSv12",
        "sslOpNoTicket",
        "sslOpNoCompression",
        "sslSessCacheOff",
        "sslSessCacheServer",
        "sslStConnect",
        "sslStAccept",
        "sslModeEnablePartialWrite",
        "sslModeAcceptMovingWriteBuffer",
        "sslModeReleaseBuffers",
        "sslSendShutdown",
        "sslReceivedShutdown",
        "sslErrorNone",
        "sslErrorSSL",
        "sslErrorWantRead",
        "sslErrorWantWrite",
        "sslErrorWantX509Lookup",
        "sslErrorSyscall",
        "sslErrorZeroReturn",
        "sslErrorWantConnect",
        "sslErrorWantAccept",
        "x509CheckFlagAlwaysCheckSubject",
        "x509CheckFlagDisableWildCards",
        "x509CheckFlagNoPartialWildCards",
        "x509CheckFlagMultiLabelWildCards",
        "x509vOK",
        "x509vErrUnspecified",
        "x509vErrUnableToGetIssuerCert",
        "x509vErrUnableToGetCrl",
        "x509vErrUnableToDecryptCertSignature",
        "x509vErrUnableToDecryptCrlSignature",
        "x509vErrUnableToDecodeIssuerPublicKey",
        "x509vErrCertSignatureFailure",
        "x509vErrCrlSignatureFailure",
        "x509vErrCertNotYetValid",
        "x509vErrCertHasExpired",
        "x509vErrCrlNotYetValid",
        "x509vErrCrlHasExpired",
        "x509vErrErrorInCertNotBeforeField",
        "x509vErrErrorInCertNotAfterField",
        "x509vErrErrorInCrlLastUpdateField",
        "x509vErrErrorInCrlNextUpdateField",
        "x509vErrOutOfMem",
        "x509vErrDepthZeroSelfSignedCert",
        "x509vErrSelfSignedCertInChain",
        "x509vErrUnableToGetIssuerCertLocally",
        "x509vErrUnableToVerifyLeafSignature",
        "x509vErrCertChainTooLong",
        "x509vErrCertRevoked",
        "x509vErrInvalidCa",
        "x509vErrPathLengthExceeded",
        "x509vErrInvalidPurpose",
        "x509vErrCertUntrusted",
        "x509vErrCertRejected",
        "x509vErrSubjectIssuerMismatch",
        "x509vErrAkidSkidMismatch",
        "x509vErrAkidIssuerSerialMismatch",
        "x509vErrKeyUsageNoCertSign",
        "x509vErrUnableToGetCrlIssuer",
        "x509vErrUnhandledCriticalExtension",
        "x509vErrKeyUsageNoCrlSign",
        "x509vErrUnhandledCriticalCrlExtension",
        "x509vErrInvalidNonCa",
        "x509vErrProxyPathLengthExceeded",
        "x509vErrKeyUsageNoDigitalSignature",
        "x509vErrProxyCertificatesNotAllowed",
        "x509vErrInvalidExtension",
        "x509vErrInvalidPolicyExtension",
        "x509vErrNoExplicitPolicy",
        "x509vErrDifferntCrlScope",
        "x509vErrUnsupportedExtensionFeature",
        "x509vErrUnnestedResource",
        "x509vErrPermittedViolation",
        "x509vErrExcludedViolation",
        "x509vErrSubtreeMinMax",
        "x509vErrApplicationVerification",
        "x509vErrUnsupportedConstraintType",
        "x509vErrUnsupportedConstraintSyntax",
        "x509vErrUnsupportedNameSyntax",
        "x509vErrCrlPathValidationError",
        "x509vErrPathLoop",
        "x509vErrSuiteBInvalidVersion",
        "x509vErrSuiteBInvalidAlgorithm",
        "x509vErrSuiteBInvalidCurve",
        "x509vErrSuiteBInvalidSignatureAlgorithm",
        "x509vErrSuiteBLosNotAllowed",
        "x509vErrSuiteBCannotSignP384WithP256",
        "x509vErrHostnameMismatch",
        "x509vErrEmailMismatch",
        "x509vErrIpAddressMismatch",
        "x509vErrDaneNoMatch",
    ];
    // `NativeStaticallyReferencedJniMethods` is how tcnative imports the
    // OpenSSL `SSL_OP_*` / `SSL_ERROR_*` / `X509_V_ERR_*` numeric constants.
    // Those are fixed integers in OpenSSL's own headers, so the true values
    // are knowable without linking OpenSSL — `register_netty_static_int_const`
    // publishes them.
    //
    // Every name used to answer 0. That made every `SSL_ERROR_*` compare equal
    // to `SSL_ERROR_NONE` (a failed handshake reading as success), collapsed
    // the mutually exclusive `SSL_OP_NO_<protocol>` bits onto each other, and
    // made every `X509_V_ERR_*` compare equal to `X509_V_OK`.
    let nsm = "io/netty/internal/tcnative/NativeStaticallyReferencedJniMethods";
    for name in NETTY_STATIC_INT_NATIVES {
        register_netty_static_int_const(registry, nsm, name);
    }

    let ssl = "io/netty/internal/tcnative/SSL";
    // KEEP (deliberate constants): `initialize` returns OpenSSL's success
    // code (0), and `version` reports the OpenSSL 1.1.1g version number that
    // matches the "OpenSSL cratonvm-stub" string below — tcnative gates
    // feature detection on that number, so it has to name a plausible
    // release. Nothing behind these two actually links OpenSSL.
    registry.register(ssl, "initialize", "(Ljava/lang/String;)I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    registry.register(ssl, "version", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0x1010107f)))
    });
    registry.register(
        ssl,
        "versionString",
        "()Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.create_string("OpenSSL cratonvm-stub"),
            ))))
        },
    );
    registry.set_category(__prev_cat);
}

/// Register one `NativeStaticallyReferencedJniMethods.<name>()I` accessor with
/// the value OpenSSL's headers give it.
///
/// Kept as an explicit match so each name maps to a `fn` pointer — the registry
/// does not accept capturing closures. Sources: `openssl/ssl.h` (`SSL_OP_*`,
/// `SSL_SESS_CACHE_*`, `SSL_ST_*`, `SSL_MODE_*`, `SSL_*_SHUTDOWN`,
/// `SSL_ERROR_*`), `openssl/x509v3.h` (`X509_CHECK_FLAG_*`) and
/// `openssl/x509_vfy.h` (`X509_V_*`), OpenSSL 1.1.1 — the release
/// `SSL.version()` below reports.
fn register_netty_static_int_const(r: &mut NativeMethodRegistry, class: &str, name: &str) {
    let cb: cratonvm_native_api::NativeCallback = match name {
        // -- SSL_OP_* option bits (ssl.h). SSLv2 support was removed in
        //    OpenSSL 1.1.0, which is why SSL_OP_NO_SSLv2 is genuinely 0.
        "sslOpCipherServerPreference" => |_c, _a| Ok(Some(Value::Int(0x0040_0000))),
        "sslOpNoSSLv2" => |_c, _a| Ok(Some(Value::Int(0x0000_0000))),
        "sslOpNoSSLv3" => |_c, _a| Ok(Some(Value::Int(0x0200_0000))),
        "sslOpNoTLSv1" => |_c, _a| Ok(Some(Value::Int(0x0400_0000))),
        "sslOpNoTLSv11" => |_c, _a| Ok(Some(Value::Int(0x1000_0000))),
        "sslOpNoTLSv12" => |_c, _a| Ok(Some(Value::Int(0x0800_0000))),
        "sslOpNoTicket" => |_c, _a| Ok(Some(Value::Int(0x0000_4000))),
        "sslOpNoCompression" => |_c, _a| Ok(Some(Value::Int(0x0002_0000))),
        // -- SSL_SESS_CACHE_* (ssl.h)
        "sslSessCacheOff" => |_c, _a| Ok(Some(Value::Int(0x0000))),
        "sslSessCacheServer" => |_c, _a| Ok(Some(Value::Int(0x0002))),
        // -- SSL_ST_* handshake-side bits (ssl.h)
        "sslStConnect" => |_c, _a| Ok(Some(Value::Int(0x1000))),
        "sslStAccept" => |_c, _a| Ok(Some(Value::Int(0x2000))),
        // -- SSL_MODE_* (ssl.h)
        "sslModeEnablePartialWrite" => |_c, _a| Ok(Some(Value::Int(0x0000_0001))),
        "sslModeAcceptMovingWriteBuffer" => |_c, _a| Ok(Some(Value::Int(0x0000_0002))),
        "sslModeReleaseBuffers" => |_c, _a| Ok(Some(Value::Int(0x0000_0010))),
        // -- SSL_SENT_SHUTDOWN / SSL_RECEIVED_SHUTDOWN (ssl.h)
        "sslSendShutdown" => |_c, _a| Ok(Some(Value::Int(1))),
        "sslReceivedShutdown" => |_c, _a| Ok(Some(Value::Int(2))),
        // -- SSL_ERROR_* (ssl.h). These must be distinct or a failed handshake
        //    reads as SSL_ERROR_NONE.
        "sslErrorNone" => |_c, _a| Ok(Some(Value::Int(0))),
        "sslErrorSSL" => |_c, _a| Ok(Some(Value::Int(1))),
        "sslErrorWantRead" => |_c, _a| Ok(Some(Value::Int(2))),
        "sslErrorWantWrite" => |_c, _a| Ok(Some(Value::Int(3))),
        "sslErrorWantX509Lookup" => |_c, _a| Ok(Some(Value::Int(4))),
        "sslErrorSyscall" => |_c, _a| Ok(Some(Value::Int(5))),
        "sslErrorZeroReturn" => |_c, _a| Ok(Some(Value::Int(6))),
        "sslErrorWantConnect" => |_c, _a| Ok(Some(Value::Int(7))),
        "sslErrorWantAccept" => |_c, _a| Ok(Some(Value::Int(8))),
        // -- X509_CHECK_FLAG_* (x509v3.h)
        "x509CheckFlagAlwaysCheckSubject" => |_c, _a| Ok(Some(Value::Int(0x1))),
        "x509CheckFlagDisableWildCards" => |_c, _a| Ok(Some(Value::Int(0x2))),
        "x509CheckFlagNoPartialWildCards" => |_c, _a| Ok(Some(Value::Int(0x4))),
        "x509CheckFlagMultiLabelWildCards" => |_c, _a| Ok(Some(Value::Int(0x8))),
        // -- X509_V_OK / X509_V_ERR_* (x509_vfy.h), in header order 0..65.
        "x509vOK" => |_c, _a| Ok(Some(Value::Int(0))),
        "x509vErrUnspecified" => |_c, _a| Ok(Some(Value::Int(1))),
        "x509vErrUnableToGetIssuerCert" => |_c, _a| Ok(Some(Value::Int(2))),
        "x509vErrUnableToGetCrl" => |_c, _a| Ok(Some(Value::Int(3))),
        "x509vErrUnableToDecryptCertSignature" => |_c, _a| Ok(Some(Value::Int(4))),
        "x509vErrUnableToDecryptCrlSignature" => |_c, _a| Ok(Some(Value::Int(5))),
        "x509vErrUnableToDecodeIssuerPublicKey" => |_c, _a| Ok(Some(Value::Int(6))),
        "x509vErrCertSignatureFailure" => |_c, _a| Ok(Some(Value::Int(7))),
        "x509vErrCrlSignatureFailure" => |_c, _a| Ok(Some(Value::Int(8))),
        "x509vErrCertNotYetValid" => |_c, _a| Ok(Some(Value::Int(9))),
        "x509vErrCertHasExpired" => |_c, _a| Ok(Some(Value::Int(10))),
        "x509vErrCrlNotYetValid" => |_c, _a| Ok(Some(Value::Int(11))),
        "x509vErrCrlHasExpired" => |_c, _a| Ok(Some(Value::Int(12))),
        "x509vErrErrorInCertNotBeforeField" => |_c, _a| Ok(Some(Value::Int(13))),
        "x509vErrErrorInCertNotAfterField" => |_c, _a| Ok(Some(Value::Int(14))),
        "x509vErrErrorInCrlLastUpdateField" => |_c, _a| Ok(Some(Value::Int(15))),
        "x509vErrErrorInCrlNextUpdateField" => |_c, _a| Ok(Some(Value::Int(16))),
        "x509vErrOutOfMem" => |_c, _a| Ok(Some(Value::Int(17))),
        "x509vErrDepthZeroSelfSignedCert" => |_c, _a| Ok(Some(Value::Int(18))),
        "x509vErrSelfSignedCertInChain" => |_c, _a| Ok(Some(Value::Int(19))),
        "x509vErrUnableToGetIssuerCertLocally" => |_c, _a| Ok(Some(Value::Int(20))),
        "x509vErrUnableToVerifyLeafSignature" => |_c, _a| Ok(Some(Value::Int(21))),
        "x509vErrCertChainTooLong" => |_c, _a| Ok(Some(Value::Int(22))),
        "x509vErrCertRevoked" => |_c, _a| Ok(Some(Value::Int(23))),
        "x509vErrInvalidCa" => |_c, _a| Ok(Some(Value::Int(24))),
        "x509vErrPathLengthExceeded" => |_c, _a| Ok(Some(Value::Int(25))),
        "x509vErrInvalidPurpose" => |_c, _a| Ok(Some(Value::Int(26))),
        "x509vErrCertUntrusted" => |_c, _a| Ok(Some(Value::Int(27))),
        "x509vErrCertRejected" => |_c, _a| Ok(Some(Value::Int(28))),
        "x509vErrSubjectIssuerMismatch" => |_c, _a| Ok(Some(Value::Int(29))),
        "x509vErrAkidSkidMismatch" => |_c, _a| Ok(Some(Value::Int(30))),
        "x509vErrAkidIssuerSerialMismatch" => |_c, _a| Ok(Some(Value::Int(31))),
        "x509vErrKeyUsageNoCertSign" => |_c, _a| Ok(Some(Value::Int(32))),
        "x509vErrUnableToGetCrlIssuer" => |_c, _a| Ok(Some(Value::Int(33))),
        "x509vErrUnhandledCriticalExtension" => |_c, _a| Ok(Some(Value::Int(34))),
        "x509vErrKeyUsageNoCrlSign" => |_c, _a| Ok(Some(Value::Int(35))),
        "x509vErrUnhandledCriticalCrlExtension" => |_c, _a| Ok(Some(Value::Int(36))),
        "x509vErrInvalidNonCa" => |_c, _a| Ok(Some(Value::Int(37))),
        "x509vErrProxyPathLengthExceeded" => |_c, _a| Ok(Some(Value::Int(38))),
        "x509vErrKeyUsageNoDigitalSignature" => |_c, _a| Ok(Some(Value::Int(39))),
        "x509vErrProxyCertificatesNotAllowed" => |_c, _a| Ok(Some(Value::Int(40))),
        "x509vErrInvalidExtension" => |_c, _a| Ok(Some(Value::Int(41))),
        "x509vErrInvalidPolicyExtension" => |_c, _a| Ok(Some(Value::Int(42))),
        "x509vErrNoExplicitPolicy" => |_c, _a| Ok(Some(Value::Int(43))),
        // tcnative's own spelling of X509_V_ERR_DIFFERENT_CRL_SCOPE.
        "x509vErrDifferntCrlScope" => |_c, _a| Ok(Some(Value::Int(44))),
        "x509vErrUnsupportedExtensionFeature" => |_c, _a| Ok(Some(Value::Int(45))),
        "x509vErrUnnestedResource" => |_c, _a| Ok(Some(Value::Int(46))),
        "x509vErrPermittedViolation" => |_c, _a| Ok(Some(Value::Int(47))),
        "x509vErrExcludedViolation" => |_c, _a| Ok(Some(Value::Int(48))),
        "x509vErrSubtreeMinMax" => |_c, _a| Ok(Some(Value::Int(49))),
        "x509vErrApplicationVerification" => |_c, _a| Ok(Some(Value::Int(50))),
        "x509vErrUnsupportedConstraintType" => |_c, _a| Ok(Some(Value::Int(51))),
        "x509vErrUnsupportedConstraintSyntax" => |_c, _a| Ok(Some(Value::Int(52))),
        "x509vErrUnsupportedNameSyntax" => |_c, _a| Ok(Some(Value::Int(53))),
        "x509vErrCrlPathValidationError" => |_c, _a| Ok(Some(Value::Int(54))),
        "x509vErrPathLoop" => |_c, _a| Ok(Some(Value::Int(55))),
        "x509vErrSuiteBInvalidVersion" => |_c, _a| Ok(Some(Value::Int(56))),
        "x509vErrSuiteBInvalidAlgorithm" => |_c, _a| Ok(Some(Value::Int(57))),
        "x509vErrSuiteBInvalidCurve" => |_c, _a| Ok(Some(Value::Int(58))),
        "x509vErrSuiteBInvalidSignatureAlgorithm" => |_c, _a| Ok(Some(Value::Int(59))),
        "x509vErrSuiteBLosNotAllowed" => |_c, _a| Ok(Some(Value::Int(60))),
        "x509vErrSuiteBCannotSignP384WithP256" => |_c, _a| Ok(Some(Value::Int(61))),
        "x509vErrHostnameMismatch" => |_c, _a| Ok(Some(Value::Int(62))),
        "x509vErrEmailMismatch" => |_c, _a| Ok(Some(Value::Int(63))),
        "x509vErrIpAddressMismatch" => |_c, _a| Ok(Some(Value::Int(64))),
        "x509vErrDaneNoMatch" => |_c, _a| Ok(Some(Value::Int(65))),
        // Unknown name: X509_V_ERR_APPLICATION_VERIFICATION is OpenSSL's
        // catch-all "verification failed for an unnamed reason".
        _ => |_c, _a| Ok(Some(Value::Int(50))),
    };
    r.register(class, name, "()I", cb);
}
