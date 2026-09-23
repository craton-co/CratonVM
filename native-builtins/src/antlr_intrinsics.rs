// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ANTLR runtime intrinsics (`org.antlr.v4.runtime.*`) and the Groovy parser shims built on them.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;
use cratonvm_native_api::NativeHandle;

// ---------------------------------------------------------------------------
// GC-root discipline for this module
//
// Every intrinsic below reimplements a piece of ANTLR's `ParserATNSimulator`
// machinery *in Rust*, and most of them hold references to the parser's own
// object graph across calls that allocate (`new_object`, `alloc_object`,
// `new_array`, `invoke_virtual`, `ensure_class_initialized`, …). Under the
// moving young collector any such call can relocate those objects, and a bare
// `ObjectRef` Rust local left over from before the call then points at dead
// memory. Linking one of those back into a config set silently poisons the
// closure/reach computation, whose result ANTLR memoizes as a DFA edge — so a
// single mistimed collection breaks every later prediction that reuses the
// edge (see antlr-native-roots-moving-young-hql-misparse-20260730-FIXED.md).
//
// The rule for this file, enforced by `raw_pin_api_is_not_used_in_this_module`
// in the test module at the bottom:
//
//   * NEVER call `pin_native_root` / `read_native_pin` / `unpin_native_roots`
//     here. The raw trio keeps the *same* `ObjectRef` type on both sides of a
//     collection, so reading the pre-call local instead of the re-read one
//     type-checks fine — and `unpin_native_roots(base)` truncates the pin
//     stack, silently dropping every root taken after `base` (a bug found
//     twice in this file alone).
//   * Open a `NativeHandleScope`, `root()` everything that must survive, and
//     `get()` it back immediately before each use. The scope closes itself on
//     normal return, `?`, and unwind, and nested scopes release only their own
//     handles.
//   * Iterate an `ATNConfigSet` BY INDEX out of the rooted set
//     (`antlr_config_set_at`) rather than snapshotting it into a
//     `Vec<ObjectRef>`: the snapshot is stale the moment the loop body
//     allocates, and rooting the whole snapshot to compensate makes every
//     config a GC root for the entire closure walk — on exactly the
//     allocation-heavy path that provokes the bug.
// ---------------------------------------------------------------------------

/// Root the object carried by `value`, if any, and return its handle.
///
/// Non-reference `Value`s need no root, so they yield `None` and are replayed
/// verbatim by [`antlr_read_rooted_value`].
#[inline]
fn antlr_root_value(scope: &mut NativeHandleScope<'_>, value: Value) -> Option<NativeHandle> {
    match value {
        Value::Object(Some(obj)) => Some(scope.root(obj)),
        _ => None,
    }
}

/// Re-read a `Value` rooted by [`antlr_root_value`] at its current address.
#[inline]
fn antlr_read_rooted_value(
    scope: &NativeHandleScope<'_>,
    value: Value,
    handle: Option<&NativeHandle>,
) -> Value {
    match handle {
        Some(handle) => Value::Object(Some(scope.get(handle))),
        None => value,
    }
}

#[inline]
pub(crate) fn antlr_bool(v: bool) -> Value {
    Value::Int(if v { 1 } else { 0 })
}

#[inline]
fn antlr_is_runtime_class_name(name: &str) -> bool {
    name.starts_with("org/antlr/v4/runtime/") || name.starts_with("groovyjarjarantlr4/v4/runtime/")
}

#[inline]
fn antlr_is_groovy_runtime_class_name(name: &str) -> bool {
    name.starts_with("groovyjarjarantlr4/v4/runtime/")
}

#[inline]
fn antlr_is_groovy_atn_config_name(name: &str) -> bool {
    antlr_is_groovy_runtime_class_name(name) && name.contains("/atn/ATNConfig")
}

fn antlr_fallback_field_value(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize) -> Value {
    if slot < ctx.object_num_fields(obj) {
        ctx.get_field(obj, slot)
    } else {
        Value::Object(None)
    }
}

fn antlr_groovy_packed_atn_value(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    match antlr_fallback_field_value(ctx, obj, 1) {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn antlr_groovy_atn_alt(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    antlr_groovy_packed_atn_value(ctx, obj) & GROOVY_ATN_ALT_MASK
}

fn antlr_groovy_atn_reaches(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    let packed = antlr_groovy_packed_atn_value(ctx, obj);
    let depth = (packed & GROOVY_ATN_DEPTH_MASK) >> GROOVY_ATN_DEPTH_SHIFT;
    let suppressed = if (packed & GROOVY_ATN_SUPPRESS_PRECEDENCE_FILTER) != 0 {
        ANTLR_SUPPRESS_PRECEDENCE_FILTER
    } else {
        0
    };
    depth | suppressed
}

fn antlr_set_groovy_packed_atn_alt(ctx: &mut dyn NativeContext, obj: ObjectRef, alt: i32) {
    if ctx.object_num_fields(obj) <= 1 {
        return;
    }
    let packed = antlr_groovy_packed_atn_value(ctx, obj);
    ctx.set_field(
        obj,
        1,
        Value::Int((packed & !GROOVY_ATN_ALT_MASK) | (alt & GROOVY_ATN_ALT_MASK)),
    );
}

fn antlr_set_groovy_packed_atn_reaches(ctx: &mut dyn NativeContext, obj: ObjectRef, reaches: i32) {
    if ctx.object_num_fields(obj) <= 1 {
        return;
    }
    let packed = antlr_groovy_packed_atn_value(ctx, obj);
    let depth = (reaches & !ANTLR_SUPPRESS_PRECEDENCE_FILTER).clamp(0, 127);
    let suppressed = if (reaches & ANTLR_SUPPRESS_PRECEDENCE_FILTER) != 0 {
        GROOVY_ATN_SUPPRESS_PRECEDENCE_FILTER
    } else {
        0
    };
    let next = (packed & GROOVY_ATN_ALT_MASK) | (depth << GROOVY_ATN_DEPTH_SHIFT) | suppressed;
    ctx.set_field(obj, 1, Value::Int(next));
}

fn antlr_groovy_atn_special_slot(class_name: &str, name: &str) -> Option<usize> {
    match name {
        "semanticContext"
            if class_name.ends_with("ATNConfig$SemanticContextATNConfig")
                || class_name.ends_with("ATNConfig$ActionSemanticContextATNConfig") =>
        {
            Some(3)
        }
        "lexerActionExecutor" if class_name.ends_with("ATNConfig$ActionATNConfig") => Some(3),
        "lexerActionExecutor"
            if class_name.ends_with("ATNConfig$ActionSemanticContextATNConfig") =>
        {
            Some(4)
        }
        "passedThroughNonGreedyDecision" if class_name.ends_with("ATNConfig$ActionATNConfig") => {
            Some(4)
        }
        "passedThroughNonGreedyDecision"
            if class_name.ends_with("ATNConfig$ActionSemanticContextATNConfig") =>
        {
            Some(5)
        }
        _ => None,
    }
}

fn antlr_groovy_semantic_none(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name(GROOVY_ANTLR_SEMANTIC_CONTEXT)?;
    let field_idx = ctx.static_field_index_by_name(class_id, "NONE")?;
    match ctx.get_static_field(class_id, field_idx) {
        Value::Object(obj) => obj,
        _ => None,
    }
}

#[inline]
fn antlr_class_name_matches(name: &str, simple_suffix: &str) -> bool {
    antlr_is_runtime_class_name(name) && name.ends_with(simple_suffix)
}

#[inline]
fn antlr_names_for_class_name(name: &str) -> AntlrClassNames {
    if name.starts_with("groovyjarjarantlr4/v4/runtime/") {
        GROOVY_ANTLR_NAMES
    } else {
        ANTLR_NAMES
    }
}

#[inline]
fn antlr_names_for_object(ctx: &mut dyn NativeContext, obj: ObjectRef) -> AntlrClassNames {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        .map(antlr_names_for_class_name)
        .unwrap_or(ANTLR_NAMES)
}

#[inline]
fn antlr_semantic_context_name(names: AntlrClassNames) -> &'static str {
    if names.pc == GROOVY_ANTLR_PC {
        GROOVY_ANTLR_SEMANTIC_CONTEXT
    } else {
        ANTLR_SEMANTIC_CONTEXT
    }
}

#[inline]
fn antlr_semantic_and_name(names: AntlrClassNames) -> &'static str {
    if names.pc == GROOVY_ANTLR_PC {
        GROOVY_ANTLR_SEMANTIC_AND
    } else {
        ANTLR_SEMANTIC_AND
    }
}

#[inline]
fn antlr_semantic_or_name(names: AntlrClassNames) -> &'static str {
    if names.pc == GROOVY_ANTLR_PC {
        GROOVY_ANTLR_SEMANTIC_OR
    } else {
        ANTLR_SEMANTIC_OR
    }
}

#[inline]
fn antlr_singleton_descriptor(names: AntlrClassNames) -> &'static str {
    if names.pc == GROOVY_ANTLR_PC {
        "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)V"
    } else {
        "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)V"
    }
}

#[inline]
fn antlr_array_descriptor(names: AntlrClassNames) -> &'static str {
    if names.pc == GROOVY_ANTLR_PC {
        "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)V"
    } else {
        "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)V"
    }
}

fn antlr_object_kind(ctx: &mut dyn NativeContext, obj: ObjectRef) -> AntlrPredictionContextKind {
    let mut cur = Some(ctx.class_id_of_object(obj));
    while let Some(class_id) = cur {
        if let Some(name) = ctx.class_name_of_id(class_id) {
            if antlr_class_name_matches(&name, "/atn/EmptyPredictionContext") {
                return AntlrPredictionContextKind::Empty;
            }
            if antlr_class_name_matches(&name, "/atn/ArrayPredictionContext") {
                return AntlrPredictionContextKind::Array;
            }
            if antlr_class_name_matches(&name, "/atn/SingletonPredictionContext") {
                return AntlrPredictionContextKind::Singleton;
            }
        }
        cur = ctx.superclass_of(class_id);
    }
    AntlrPredictionContextKind::Other
}

fn antlr_field_value(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    fallback_slot: usize,
) -> Value {
    if let Some(class_name) = ctx.class_name_of_id(ctx.class_id_of_object(obj)) {
        if let Some(slot) = ctx.resolve_field_index(&class_name, name) {
            return ctx.get_field(obj, slot);
        }
        if antlr_is_groovy_atn_config_name(&class_name) {
            match name {
                "alt" => return Value::Int(antlr_groovy_atn_alt(ctx, obj)),
                "reachesIntoOuterContext" => {
                    return Value::Int(antlr_groovy_atn_reaches(ctx, obj));
                }
                "semanticContext" => {
                    if let Some(slot) = antlr_groovy_atn_special_slot(&class_name, name) {
                        return antlr_fallback_field_value(ctx, obj, slot);
                    }
                    return Value::Object(antlr_groovy_semantic_none(ctx));
                }
                "lexerActionExecutor" => {
                    if let Some(slot) = antlr_groovy_atn_special_slot(&class_name, name) {
                        return antlr_fallback_field_value(ctx, obj, slot);
                    }
                    return Value::Object(None);
                }
                "passedThroughNonGreedyDecision" => {
                    if let Some(slot) = antlr_groovy_atn_special_slot(&class_name, name) {
                        return antlr_fallback_field_value(ctx, obj, slot);
                    }
                    return Value::Int(0);
                }
                _ => {}
            }
        }
        if antlr_is_runtime_class_name(&class_name) {
            return antlr_fallback_field_value(ctx, obj, fallback_slot);
        }
    }
    match ctx.get_field_by_name(obj, name) {
        Value::Object(None) => antlr_fallback_field_value(ctx, obj, fallback_slot),
        v => v,
    }
}

fn antlr_set_field_value(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    fallback_slot: usize,
    value: Value,
) {
    if let Some(class_name) = ctx.class_name_of_id(ctx.class_id_of_object(obj)) {
        if let Some(slot) = ctx.resolve_field_index(&class_name, name) {
            ctx.set_field(obj, slot, value);
            return;
        }
        if antlr_is_groovy_atn_config_name(&class_name) {
            match (name, value) {
                ("alt", Value::Int(alt)) => {
                    antlr_set_groovy_packed_atn_alt(ctx, obj, alt);
                    return;
                }
                ("reachesIntoOuterContext", Value::Int(reaches)) => {
                    antlr_set_groovy_packed_atn_reaches(ctx, obj, reaches);
                    return;
                }
                ("semanticContext", value) => {
                    if let Some(slot) = antlr_groovy_atn_special_slot(&class_name, name) {
                        if slot < ctx.object_num_fields(obj) {
                            ctx.set_field(obj, slot, value);
                        }
                    }
                    return;
                }
                ("lexerActionExecutor", value) | ("passedThroughNonGreedyDecision", value) => {
                    if let Some(slot) = antlr_groovy_atn_special_slot(&class_name, name) {
                        if slot < ctx.object_num_fields(obj) {
                            ctx.set_field(obj, slot, value);
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
    }
    if fallback_slot < ctx.object_num_fields(obj) {
        ctx.set_field(obj, fallback_slot, value);
    }
}

#[inline]
fn antlr_int_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    fallback_slot: usize,
) -> i32 {
    match antlr_field_value(ctx, obj, name, fallback_slot) {
        Value::Int(v) => v,
        _ => 0,
    }
}

#[inline]
fn antlr_ref_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    fallback_slot: usize,
) -> Option<ObjectRef> {
    match antlr_field_value(ctx, obj, name, fallback_slot) {
        Value::Object(o) => o,
        _ => None,
    }
}

#[inline]
fn antlr_prediction_context_hash(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    antlr_int_field(ctx, obj, "cachedHashCode", 1)
}

#[inline]
fn antlr_singleton_parent(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, obj, "parent", 2)
}

#[inline]
fn antlr_singleton_return_state(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    antlr_int_field(ctx, obj, "returnState", 3)
}

#[inline]
fn antlr_array_parents(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, obj, "parents", 2)
}

#[inline]
fn antlr_array_return_states(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, obj, "returnStates", 3)
}

#[inline]
fn antlr_array_return_state(ctx: &mut dyn NativeContext, obj: ObjectRef, index: usize) -> i32 {
    match antlr_array_return_states(ctx, obj)
        .map(|arr| ctx.get_array_element(arr, index))
        .unwrap_or(Value::Int(0))
    {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn antlr_prediction_context_size(ctx: &mut dyn NativeContext, obj: ObjectRef) -> usize {
    match antlr_object_kind(ctx, obj) {
        AntlrPredictionContextKind::Empty | AntlrPredictionContextKind::Singleton => 1,
        AntlrPredictionContextKind::Array => antlr_array_return_states(ctx, obj)
            .map(|arr| ctx.array_length(arr))
            .unwrap_or(0),
        AntlrPredictionContextKind::Other => 0,
    }
}

fn antlr_prediction_context_return_state(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    index: usize,
) -> i32 {
    match antlr_object_kind(ctx, obj) {
        AntlrPredictionContextKind::Empty | AntlrPredictionContextKind::Singleton => {
            antlr_singleton_return_state(ctx, obj)
        }
        AntlrPredictionContextKind::Array => antlr_array_return_state(ctx, obj, index),
        AntlrPredictionContextKind::Other => 0,
    }
}

fn antlr_prediction_context_parent(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    index: usize,
) -> Option<ObjectRef> {
    match antlr_object_kind(ctx, obj) {
        AntlrPredictionContextKind::Empty => None,
        AntlrPredictionContextKind::Singleton => antlr_singleton_parent(ctx, obj),
        AntlrPredictionContextKind::Array => {
            antlr_array_parents(ctx, obj).and_then(|arr| match ctx.get_array_element(arr, index) {
                Value::Object(o) => o,
                _ => None,
            })
        }
        AntlrPredictionContextKind::Other => None,
    }
}

fn antlr_prediction_contexts_equal(
    ctx: &mut dyn NativeContext,
    mut a: ObjectRef,
    mut b: ObjectRef,
) -> bool {
    let mut depth = 0usize;
    loop {
        if a == b {
            return true;
        }
        if depth > 8192 {
            return false;
        }
        depth += 1;

        let a_kind = antlr_object_kind(ctx, a);
        let b_kind = antlr_object_kind(ctx, b);
        if a_kind == AntlrPredictionContextKind::Empty
            || b_kind == AntlrPredictionContextKind::Empty
        {
            return false;
        }
        if antlr_prediction_context_hash(ctx, a) != antlr_prediction_context_hash(ctx, b) {
            return false;
        }
        match (a_kind, b_kind) {
            (AntlrPredictionContextKind::Singleton, AntlrPredictionContextKind::Singleton) => {
                if antlr_singleton_return_state(ctx, a) != antlr_singleton_return_state(ctx, b) {
                    return false;
                }
                match (
                    antlr_singleton_parent(ctx, a),
                    antlr_singleton_parent(ctx, b),
                ) {
                    (Some(pa), Some(pb)) => {
                        a = pa;
                        b = pb;
                    }
                    _ => return false,
                }
            }
            (AntlrPredictionContextKind::Array, AntlrPredictionContextKind::Array) => {
                return antlr_array_prediction_contexts_equal(ctx, a, b, depth);
            }
            _ => return false,
        }
    }
}

fn antlr_array_prediction_contexts_equal(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
    depth: usize,
) -> bool {
    let (a_states, b_states) = match (
        antlr_array_return_states(ctx, a),
        antlr_array_return_states(ctx, b),
    ) {
        (Some(a_states), Some(b_states)) => (a_states, b_states),
        _ => return false,
    };
    let len = ctx.array_length(a_states);
    if len != ctx.array_length(b_states) {
        return false;
    }
    for i in 0..len {
        if ctx.get_array_element(a_states, i) != ctx.get_array_element(b_states, i) {
            return false;
        }
    }

    let (a_parents, b_parents) = match (antlr_array_parents(ctx, a), antlr_array_parents(ctx, b)) {
        (Some(a_parents), Some(b_parents)) => (a_parents, b_parents),
        _ => return false,
    };
    let parent_len = ctx.array_length(a_parents);
    if parent_len != ctx.array_length(b_parents) {
        return false;
    }
    for i in 0..parent_len {
        let pa = match ctx.get_array_element(a_parents, i) {
            Value::Object(o) => o,
            _ => None,
        };
        let pb = match ctx.get_array_element(b_parents, i) {
            Value::Object(o) => o,
            _ => None,
        };
        match (pa, pb) {
            (None, None) => {}
            (Some(pa), Some(pb)) => {
                if depth > 8192 || !antlr_prediction_contexts_equal(ctx, pa, pb) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

#[inline]
fn antlr_murmur_update(mut hash: i32, value: i32) -> i32 {
    let mut k = value.wrapping_mul(-862_048_943);
    k = k.rotate_left(15);
    k = k.wrapping_mul(461_845_907);
    hash ^= k;
    hash = hash.rotate_left(13);
    hash.wrapping_mul(5).wrapping_add(-430_675_100)
}

#[inline]
fn antlr_murmur_finish(mut hash: i32, words: i32) -> i32 {
    hash ^= words.wrapping_mul(4);
    hash ^= ((hash as u32) >> 16) as i32;
    hash = hash.wrapping_mul(-2_048_144_789);
    hash ^= ((hash as u32) >> 13) as i32;
    hash = hash.wrapping_mul(-1_028_477_387);
    hash ^ (((hash as u32) >> 16) as i32)
}

#[inline]
fn antlr_murmur_object_hash(ctx: &mut dyn NativeContext, obj: Option<ObjectRef>) -> i32 {
    obj.map(|o| antlr_prediction_context_hash(ctx, o))
        .unwrap_or(0)
}

fn antlr_init_prediction_context_base(ctx: &mut dyn NativeContext, obj: ObjectRef, hash: i32) {
    let id = ANTLR_NATIVE_NODE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    antlr_set_field_value(ctx, obj, "id", 0, Value::Int(id));
    antlr_set_field_value(ctx, obj, "cachedHashCode", 1, Value::Int(hash));
}

fn native_antlr_singleton_prediction_context_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let parent = match args.get(1) {
        Some(Value::Object(parent)) => *parent,
        _ => None,
    };
    let return_state = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let hash = match parent {
        Some(parent) => {
            let parent_hash = antlr_prediction_context_hash(ctx, parent);
            antlr_murmur_finish(
                antlr_murmur_update(antlr_murmur_update(1, parent_hash), return_state),
                2,
            )
        }
        None => antlr_murmur_finish(1, 0),
    };
    antlr_init_prediction_context_base(ctx, this, hash);
    antlr_set_field_value(ctx, this, "parent", 2, Value::Object(parent));
    antlr_set_field_value(ctx, this, "returnState", 3, Value::Int(return_state));
    Ok(None)
}

fn native_antlr_empty_prediction_context_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    antlr_init_prediction_context_base(ctx, this, antlr_murmur_finish(1, 0));
    antlr_set_field_value(ctx, this, "parent", 2, Value::Object(None));
    antlr_set_field_value(
        ctx,
        this,
        "returnState",
        3,
        Value::Int(ANTLR_EMPTY_RETURN_STATE),
    );
    Ok(None)
}

fn native_antlr_array_prediction_context_init_arrays(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let parents = match args.get(1) {
        Some(Value::Object(Some(parents))) => *parents,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ArrayPredictionContext parents array is null".to_string()),
            }
            .into())
        }
    };
    let states = match args.get(2) {
        Some(Value::Object(Some(states))) => *states,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ArrayPredictionContext returnStates array is null".to_string()),
            }
            .into())
        }
    };
    let hash = antlr_calculate_array_hash(ctx, parents, states);
    antlr_init_prediction_context_base(ctx, this, hash);
    antlr_set_field_value(ctx, this, "parents", 2, Value::Object(Some(parents)));
    antlr_set_field_value(ctx, this, "returnStates", 3, Value::Object(Some(states)));
    Ok(None)
}

fn native_antlr_array_prediction_context_init_singleton(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let singleton = obj_arg(args, 1)?;
    let parent = antlr_singleton_parent(ctx, singleton);
    let return_state = antlr_singleton_return_state(ctx, singleton);
    let pc_name = match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        .map(|name| name.starts_with("groovyjarjarantlr4/v4/runtime/"))
    {
        Some(true) => GROOVY_ANTLR_PC,
        _ => ANTLR_PC,
    };
    let pc_class = ctx
        .class_id_by_name(pc_name)
        .unwrap_or_else(|| ctx.class_id_of_object(singleton));

    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let parent_h = parent.map(|parent| scope.root(parent));
    let parents_arr = scope.new_ref_array(pc_class, 1);
    let parents_h = scope.root(parents_arr);
    let states_arr = scope.new_array(cratonvm_types::ArrayElementType::Int, 1);

    // `new_array` above is the last collection point; re-read every retained
    // reference through its handle before touching the heap again.
    let this = scope.get(&this_h);
    let parents_arr = scope.get(&parents_h);
    let parent = parent_h.as_ref().map(|handle| scope.get(handle));

    scope.set_array_element(parents_arr, 0, Value::Object(parent));
    scope.set_array_element(states_arr, 0, Value::Int(return_state));
    let hash = antlr_calculate_array_hash(&mut *scope, parents_arr, states_arr);
    antlr_init_prediction_context_base(&mut *scope, this, hash);
    antlr_set_field_value(
        &mut *scope,
        this,
        "parents",
        2,
        Value::Object(Some(parents_arr)),
    );
    antlr_set_field_value(
        &mut *scope,
        this,
        "returnStates",
        3,
        Value::Object(Some(states_arr)),
    );
    Ok(None)
}

fn native_antlr_prediction_context_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(antlr_prediction_context_hash(ctx, this))))
}

fn native_antlr_prediction_context_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(antlr_bool(
        antlr_object_kind(ctx, this) == AntlrPredictionContextKind::Empty,
    )))
}

fn native_antlr_prediction_context_has_empty_path(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = antlr_prediction_context_size(ctx, this);
    let has_empty = size != 0
        && antlr_prediction_context_return_state(ctx, this, size - 1) == ANTLR_EMPTY_RETURN_STATE;
    Ok(Some(antlr_bool(has_empty)))
}

fn native_antlr_prediction_context_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(
        antlr_prediction_context_size(ctx, this) as i32
    )))
}

fn native_antlr_prediction_context_get_parent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    Ok(Some(Value::Object(antlr_prediction_context_parent(
        ctx, this, index,
    ))))
}

fn native_antlr_prediction_context_get_return_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    Ok(Some(Value::Int(antlr_prediction_context_return_state(
        ctx, this, index,
    ))))
}

fn native_antlr_prediction_context_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(antlr_bool(antlr_prediction_contexts_equal(
        ctx, this, other,
    ))))
}

fn native_antlr_prediction_context_calculate_empty_hash_code(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(antlr_murmur_finish(1, 0))))
}

fn native_antlr_prediction_context_calculate_hash_singleton(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let parent_hash = match args.first() {
        Some(Value::Object(parent)) => antlr_murmur_object_hash(ctx, *parent),
        _ => 0,
    };
    let return_state = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let hash = antlr_murmur_update(antlr_murmur_update(1, parent_hash), return_state);
    Ok(Some(Value::Int(antlr_murmur_finish(hash, 2))))
}

fn native_antlr_prediction_context_calculate_hash_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let parents = match args.first() {
        Some(Value::Object(Some(parents))) => *parents,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("PredictionContext parents array is null".to_string()),
            }
            .into())
        }
    };
    let states = match args.get(1) {
        Some(Value::Object(Some(states))) => *states,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("PredictionContext returnStates array is null".to_string()),
            }
            .into())
        }
    };
    Ok(Some(Value::Int(antlr_calculate_array_hash(
        ctx, parents, states,
    ))))
}

fn antlr_calculate_array_hash(
    ctx: &mut dyn NativeContext,
    parents: ObjectRef,
    states: ObjectRef,
) -> i32 {
    let mut hash = 1;
    let parent_len = ctx.array_length(parents);
    for i in 0..parent_len {
        let parent = match ctx.get_array_element(parents, i) {
            Value::Object(parent) => parent,
            _ => None,
        };
        hash = antlr_murmur_update(hash, antlr_murmur_object_hash(ctx, parent));
    }
    let state_len = ctx.array_length(states);
    for i in 0..state_len {
        let state = match ctx.get_array_element(states, i) {
            Value::Int(v) => v,
            _ => 0,
        };
        hash = antlr_murmur_update(hash, state);
    }
    antlr_murmur_finish(hash, (parent_len + state_len) as i32)
}

fn antlr_error(message: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

fn antlr_object_from_result(
    result: MethodCallResult,
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match result? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        other => Err(antlr_error(format!("{what} returned {other:?}"))),
    }
}

fn antlr_pc_class_id(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
) -> Result<ClassId, MethodCallFailed> {
    if let Some(class_id) = ctx.class_id_by_name(names.pc) {
        return Ok(class_id);
    }
    ctx.ensure_class_initialized(names.pc)
}

fn antlr_create_int_array(ctx: &mut dyn NativeContext, states: &[i32]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, states.len());
    for (i, state) in states.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*state));
    }
    arr
}

fn antlr_create_parent_array(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    parents: &[Option<ObjectRef>],
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let handles: Vec<_> = parents
        .iter()
        .map(|parent| parent.map(|obj| scope.root(obj)))
        .collect();
    let pc_class = antlr_pc_class_id(&mut *scope, names)?;
    let arr = scope.new_ref_array(pc_class, parents.len());
    for (i, handle) in handles.iter().enumerate() {
        let parent = handle.as_ref().map(|handle| scope.get(handle));
        scope.set_array_element(arr, i, Value::Object(parent));
    }
    Ok(arr)
}

fn antlr_create_singleton_context(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    parent: Option<ObjectRef>,
    return_state: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    if parent.is_none() && return_state == ANTLR_EMPTY_RETURN_STATE {
        return antlr_empty_instance(ctx, names);
    }

    let mut scope = NativeHandleScope::new(ctx);
    let parent_h = parent.map(|parent| scope.root(parent));
    let obj = antlr_object_from_result(
        scope.new_object(names.singleton),
        "SingletonPredictionContext",
    )?;
    // The constructor intrinsic allocates, so the freshly created receiver has
    // to be rooted too — returning the pre-`init` `obj` handed the caller a
    // dead address whenever that allocation triggered an evacuation.
    let obj_h = scope.root(obj);
    let init_args = [
        Value::Object(Some(scope.get(&obj_h))),
        Value::Object(parent_h.as_ref().map(|handle| scope.get(handle))),
        Value::Int(return_state),
    ];
    native_antlr_singleton_prediction_context_init(&mut *scope, &init_args)?;
    Ok(scope.get(&obj_h))
}

fn antlr_create_array_context(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    parents: &[Option<ObjectRef>],
    states: &[i32],
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let parents_arr = antlr_create_parent_array(&mut *scope, names, parents)?;
    let parents_h = scope.root(parents_arr);
    let states_arr = antlr_create_int_array(&mut *scope, states);
    let states_h = scope.root(states_arr);
    let obj = antlr_object_from_result(scope.new_object(names.array), "ArrayPredictionContext")?;
    // Root the receiver as well: the array constructor intrinsic allocates.
    let obj_h = scope.root(obj);
    let init_args = [
        Value::Object(Some(scope.get(&obj_h))),
        Value::Object(Some(scope.get(&parents_h))),
        Value::Object(Some(scope.get(&states_h))),
    ];
    native_antlr_array_prediction_context_init_arrays(&mut *scope, &init_args)?;
    Ok(scope.get(&obj_h))
}

fn antlr_singleton_to_array_context(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    singleton: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let parent = antlr_singleton_parent(ctx, singleton);
    let state = antlr_singleton_return_state(ctx, singleton);
    antlr_create_array_context(ctx, names, &[parent], &[state])
}

fn antlr_empty_instance(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Ok(class_id) = ctx.ensure_class_initialized(names.empty) {
        if let Some(field_idx) = ctx.static_field_index_by_name(class_id, "Instance") {
            if let Value::Object(Some(instance)) = ctx.get_static_field(class_id, field_idx) {
                return Ok(instance);
            }
        }
    }

    let mut scope = NativeHandleScope::new(ctx);
    let obj = antlr_object_from_result(scope.new_object(names.empty), "EmptyPredictionContext")?;
    let obj_h = scope.root(obj);
    let init_args = [Value::Object(Some(scope.get(&obj_h)))];
    native_antlr_empty_prediction_context_init(&mut *scope, &init_args)?;
    Ok(scope.get(&obj_h))
}

fn antlr_new_linked_hash_map(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized("java/util/LinkedHashMap", "()V", &[]) {
        Ok(Some(Value::Object(Some(map)))) => Ok(map),
        _ => {
            let mut scope = NativeHandleScope::new(ctx);
            let map = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap", 3)?;
            let map_h = scope.root(map);
            let init_args = [Value::Object(Some(scope.get(&map_h)))];
            cratonvm_native_collections::native_map_init(&mut *scope, &init_args)?;
            Ok(scope.get(&map_h))
        }
    }
}

fn antlr_double_key_map_data(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, obj, "data", 0)
}

fn antlr_double_key_map_set_data(ctx: &mut dyn NativeContext, obj: ObjectRef, data: ObjectRef) {
    antlr_set_field_value(ctx, obj, "data", 0, Value::Object(Some(data)));
}

fn antlr_double_key_map_data_or_create(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(data) = antlr_double_key_map_data(ctx, obj) {
        return Ok(data);
    }
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(obj);
    let data = antlr_new_linked_hash_map(&mut *scope)?;
    let obj = scope.get(&obj_h);
    antlr_double_key_map_set_data(&mut *scope, obj, data);
    Ok(data)
}

fn antlr_map_get(ctx: &mut dyn NativeContext, map: ObjectRef, key: Value) -> MethodCallResult {
    cratonvm_native_collections::native_map_get_pub(ctx, &[Value::Object(Some(map)), key])
}

fn antlr_map_put(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    key: Value,
    value: Value,
) -> MethodCallResult {
    cratonvm_native_collections::native_map_put_pub(ctx, &[Value::Object(Some(map)), key, value])
}

fn antlr_double_key_map_get_value(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    key1: Value,
    key2: Value,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let map_h = scope.root(map);
    let key1_h = antlr_root_value(&mut scope, key1);
    let key2_h = antlr_root_value(&mut scope, key2);

    let map = scope.get(&map_h);
    let Some(data) = antlr_double_key_map_data(&mut *scope, map) else {
        return Ok(Some(Value::Object(None)));
    };
    // Retain the data map as well: the first lookup may be the only heap path
    // to the inner map while native map code is still returning.
    let data_h = scope.root(data);
    let key1 = antlr_read_rooted_value(&scope, key1, key1_h.as_ref());
    let data = scope.get(&data_h);
    let inner = antlr_map_get(&mut *scope, data, key1)?;
    let inner = match inner {
        Some(Value::Object(Some(inner))) => inner,
        _ => return Ok(Some(Value::Object(None))),
    };
    let inner_h = scope.root(inner);
    let inner = scope.get(&inner_h);
    let key2 = antlr_read_rooted_value(&scope, key2, key2_h.as_ref());
    antlr_map_get(&mut *scope, inner, key2)
}

fn antlr_double_key_map_put_value(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    key1: Value,
    key2: Value,
    value: Value,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let map_h = scope.root(map);
    let key1_h = antlr_root_value(&mut scope, key1);
    let key2_h = antlr_root_value(&mut scope, key2);
    let value_h = antlr_root_value(&mut scope, value);

    let map = scope.get(&map_h);
    let data = antlr_double_key_map_data_or_create(&mut *scope, map)?;
    let data_h = scope.root(data);
    let data = scope.get(&data_h);
    let key1 = antlr_read_rooted_value(&scope, key1, key1_h.as_ref());
    let inner_value = antlr_map_get(&mut *scope, data, key1)?;
    let previous = match inner_value {
        Some(Value::Object(Some(inner))) => {
            let inner_h = scope.root(inner);
            let inner = scope.get(&inner_h);
            let key2 = antlr_read_rooted_value(&scope, key2, key2_h.as_ref());
            let value = antlr_read_rooted_value(&scope, value, value_h.as_ref());
            antlr_map_put(&mut *scope, inner, key2, value)?.unwrap_or(Value::Object(None))
        }
        _ => {
            let inner = antlr_new_linked_hash_map(&mut *scope)?;
            let inner_h = scope.root(inner);
            let data = scope.get(&data_h);
            let key1 = antlr_read_rooted_value(&scope, key1, key1_h.as_ref());
            let inner = scope.get(&inner_h);
            antlr_map_put(&mut *scope, data, key1, Value::Object(Some(inner)))?;
            let inner = scope.get(&inner_h);
            let key2 = antlr_read_rooted_value(&scope, key2, key2_h.as_ref());
            let value = antlr_read_rooted_value(&scope, value, value_h.as_ref());
            antlr_map_put(&mut *scope, inner, key2, value)?.unwrap_or(Value::Object(None))
        }
    };
    Ok(Some(previous))
}

fn native_antlr_double_key_map_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let data = antlr_new_linked_hash_map(&mut *scope)?;
    let this = scope.get(&this_h);
    antlr_double_key_map_set_data(&mut *scope, this, data);
    Ok(None)
}

fn native_antlr_double_key_map_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key1 = args.get(1).copied().unwrap_or(Value::Object(None));
    let key2 = args.get(2).copied().unwrap_or(Value::Object(None));
    antlr_double_key_map_get_value(ctx, this, key1, key2)
}

fn native_antlr_double_key_map_put(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key1 = args.get(1).copied().unwrap_or(Value::Object(None));
    let key2 = args.get(2).copied().unwrap_or(Value::Object(None));
    let value = args.get(3).copied().unwrap_or(Value::Object(None));
    antlr_double_key_map_put_value(ctx, this, key1, key2, value)
}

fn antlr_arraylist_field_value(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    fallback_slot: usize,
) -> Value {
    if let Some(slot) = ctx.resolve_field_index("java/util/ArrayList", name) {
        return ctx.get_field(obj, slot);
    }
    ctx.get_field(obj, fallback_slot)
}

fn antlr_arraylist_set_field_value(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    fallback_slot: usize,
    value: Value,
) {
    if let Some(slot) = ctx.resolve_field_index("java/util/ArrayList", name) {
        ctx.set_field(obj, slot, value);
        return;
    }
    ctx.set_field(obj, fallback_slot, value);
}

fn antlr_arraylist_data(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<ObjectRef> {
    match antlr_arraylist_field_value(ctx, obj, "elementData", 0) {
        Value::Object(data) => data,
        _ => None,
    }
}

fn antlr_arraylist_size(ctx: &mut dyn NativeContext, obj: ObjectRef) -> usize {
    match antlr_arraylist_field_value(ctx, obj, "size", 1) {
        Value::Int(v) if v > 0 => v as usize,
        _ => 0,
    }
}

fn antlr_arraylist_set_data(ctx: &mut dyn NativeContext, obj: ObjectRef, data: ObjectRef) {
    antlr_arraylist_set_field_value(ctx, obj, "elementData", 0, Value::Object(Some(data)));
}

fn antlr_arraylist_set_size(ctx: &mut dyn NativeContext, obj: ObjectRef, size: usize) {
    antlr_arraylist_set_field_value(ctx, obj, "size", 1, Value::Int(size as i32));
}

fn antlr_arraylist_append(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    value: Value,
) -> Result<(), MethodCallFailed> {
    let size = antlr_arraylist_size(ctx, list);
    let data = antlr_arraylist_data(ctx, list);
    let capacity = data.map(|arr| ctx.array_length(arr)).unwrap_or(0);
    if size < capacity {
        if let Some(data) = data {
            ctx.set_array_element(data, size, value);
            antlr_arraylist_set_size(ctx, list, size + 1);
        }
        return Ok(());
    }

    let new_capacity = std::cmp::max(size + 1, std::cmp::max(7, capacity + (capacity >> 1) + 1));
    let mut scope = NativeHandleScope::new(ctx);
    let list_h = scope.root(list);
    let data_h = data.map(|arr| scope.root(arr));
    let value_h = antlr_root_value(&mut scope, value);
    let new_data = scope.new_array(cratonvm_types::ArrayElementType::Reference, new_capacity);
    let list = scope.get(&list_h);
    let data = data_h.as_ref().map(|handle| scope.get(handle));
    let value = antlr_read_rooted_value(&scope, value, value_h.as_ref());
    if let Some(old_data) = data {
        for i in 0..std::cmp::min(size, scope.array_length(old_data)) {
            let item = scope.get_array_element(old_data, i);
            scope.set_array_element(new_data, i, item);
        }
    }
    scope.set_array_element(new_data, size, value);
    antlr_arraylist_set_data(&mut *scope, list, new_data);
    antlr_arraylist_set_size(&mut *scope, list, size + 1);
    Ok(())
}

fn antlr_is_java_arraylist(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some("java/util/ArrayList")
}

fn antlr_atn_state_transitions(
    ctx: &mut dyn NativeContext,
    state: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    antlr_ref_field(ctx, state, "transitions", 4).ok_or_else(|| {
        RuntimeError::NullPointerException {
            message: Some("ATNState.transitions".to_string()),
        }
        .into()
    })
}

fn native_antlr_atn_state_get_number_of_transitions(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let transitions = antlr_atn_state_transitions(ctx, this)?;
    if antlr_is_java_arraylist(ctx, transitions) {
        let size = std::cmp::min(antlr_arraylist_size(ctx, transitions), i32::MAX as usize);
        return Ok(Some(Value::Int(size as i32)));
    }

    let mut scope = NativeHandleScope::new(ctx);
    let transitions_h = scope.root(transitions);
    let transitions = scope.get(&transitions_h);
    match scope.invoke_virtual(transitions, "size", "()I", &[])? {
        Some(Value::Int(size)) if size > 0 => Ok(Some(Value::Int(size))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_antlr_atn_state_transition(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => 0,
    };
    if index < 0 {
        return Err(RuntimeError::aioobe_index_only(index).into());
    }

    let transitions = antlr_atn_state_transitions(ctx, this)?;
    if antlr_is_java_arraylist(ctx, transitions) {
        let Some(data) = antlr_arraylist_data(ctx, transitions) else {
            return Err(RuntimeError::NullPointerException {
                message: Some("ArrayList.elementData".to_string()),
            }
            .into());
        };
        let index_usize = index as usize;
        let size = antlr_arraylist_size(ctx, transitions);
        if index_usize >= size || index_usize >= ctx.array_length(data) {
            return Err(RuntimeError::aioobe_index_only(index).into());
        }
        return Ok(Some(ctx.get_array_element(data, index_usize)));
    }

    let mut scope = NativeHandleScope::new(ctx);
    let transitions_h = scope.root(transitions);
    let transitions = scope.get(&transitions_h);
    match scope.invoke_virtual(
        transitions,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Int(index)],
    )? {
        Some(Value::Object(obj)) => Ok(Some(Value::Object(obj))),
        _ => Ok(Some(Value::Object(None))),
    }
}

fn native_antlr_atn_state_only_has_epsilon_transitions(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(antlr_bool(
        antlr_int_field(ctx, this, "epsilonOnlyTransitions", 3) != 0,
    )))
}

fn antlr_atn_state_type_from_class_name(class_name: &str) -> Option<i32> {
    if !antlr_is_runtime_class_name(class_name) {
        return None;
    }
    if class_name.ends_with("/atn/BasicState") {
        Some(1)
    } else if class_name.ends_with("/atn/RuleStartState") {
        Some(2)
    } else if class_name.ends_with("/atn/BasicBlockStartState") {
        Some(3)
    } else if class_name.ends_with("/atn/PlusBlockStartState") {
        Some(4)
    } else if class_name.ends_with("/atn/StarBlockStartState") {
        Some(5)
    } else if class_name.ends_with("/atn/TokensStartState") {
        Some(6)
    } else if class_name.ends_with("/atn/RuleStopState") {
        Some(7)
    } else if class_name.ends_with("/atn/BlockEndState") {
        Some(8)
    } else if class_name.ends_with("/atn/StarLoopbackState") {
        Some(9)
    } else if class_name.ends_with("/atn/StarLoopEntryState") {
        Some(10)
    } else if class_name.ends_with("/atn/PlusLoopbackState") {
        Some(11)
    } else if class_name.ends_with("/atn/LoopEndState") {
        Some(12)
    } else {
        None
    }
}

fn native_antlr_atn_state_get_state_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    Ok(Some(Value::Int(
        antlr_atn_state_type_from_class_name(&class_name).unwrap_or(0),
    )))
}

fn antlr_atn_state_type(ctx: &mut dyn NativeContext, state: ObjectRef) -> i32 {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(state))
        .unwrap_or_default();
    antlr_atn_state_type_from_class_name(&class_name).unwrap_or(0)
}

fn antlr_atn_state_rule_index(ctx: &mut dyn NativeContext, state: ObjectRef) -> i32 {
    antlr_int_field(ctx, state, "ruleIndex", 2)
}

fn antlr_atn_state_transition_ref(
    ctx: &mut dyn NativeContext,
    state: ObjectRef,
    index: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let transitions = antlr_atn_state_transitions(ctx, state)?;
    let value = antlr_list_get(ctx, transitions, index)?;
    Ok(match value {
        Value::Object(obj) => obj,
        _ => None,
    })
}

fn antlr_transition_target(
    ctx: &mut dyn NativeContext,
    transition: ObjectRef,
) -> Option<ObjectRef> {
    antlr_ref_field(ctx, transition, "target", 0)
}

fn antlr_parser_atn_states(ctx: &mut dyn NativeContext, simulator: ObjectRef) -> Option<ObjectRef> {
    let atn = antlr_ref_field(ctx, simulator, "atn", 2)?;
    antlr_ref_field(ctx, atn, "states", 0)
}

fn antlr_parser_atn_state_by_number(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    state_number: i32,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if state_number < 0 {
        return Ok(None);
    }
    let Some(states) = antlr_parser_atn_states(ctx, simulator) else {
        return Ok(None);
    };
    match antlr_list_get(ctx, states, state_number as usize)? {
        Value::Object(obj) => Ok(obj),
        _ => Ok(None),
    }
}

fn antlr_prediction_context_is_empty(ctx: &mut dyn NativeContext, context: ObjectRef) -> bool {
    antlr_object_kind(ctx, context) == AntlrPredictionContextKind::Empty
}

fn antlr_prediction_context_has_empty_path(
    ctx: &mut dyn NativeContext,
    context: ObjectRef,
) -> bool {
    let size = antlr_prediction_context_size(ctx, context);
    size != 0
        && antlr_prediction_context_return_state(ctx, context, size - 1) == ANTLR_EMPTY_RETURN_STATE
}

fn native_antlr_parser_can_drop_loop_entry_edge(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    const ATN_STATE_BLOCK_END: i32 = 8;
    const ATN_STATE_STAR_LOOP_ENTRY: i32 = 10;

    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let Some(state) = antlr_ref_field(ctx, config, "state", 0) else {
        return Ok(Some(Value::Int(0)));
    };

    if antlr_atn_state_type(ctx, state) != ATN_STATE_STAR_LOOP_ENTRY {
        return Ok(Some(Value::Int(0)));
    }
    if antlr_int_field(ctx, state, "isPrecedenceDecision", 5) == 0 {
        return Ok(Some(Value::Int(0)));
    }

    let Some(context) = antlr_atn_config_context(ctx, config) else {
        return Ok(Some(Value::Int(0)));
    };
    if antlr_prediction_context_is_empty(ctx, context)
        || antlr_prediction_context_has_empty_path(ctx, context)
    {
        return Ok(Some(Value::Int(0)));
    }

    // Every loop below walks the ATN through `ATNState.transitions`, which can
    // dispatch into Java (`List.get`) and therefore collect. `state`,
    // `context` and `blockEndState` are compared by *identity* against results
    // read after those calls, so they must be re-read through handles rather
    // than carried as raw locals — a stale `state` here silently answered
    // "cannot drop this edge" and poisoned the memoized DFA edge.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let state_h = scope.root(state);
    let context_h = scope.root(context);

    let state_rule = antlr_atn_state_rule_index(&mut *scope, state);
    let num_contexts = antlr_prediction_context_size(&mut *scope, context);
    for index in 0..num_contexts {
        let context = scope.get(&context_h);
        let return_state_number =
            antlr_prediction_context_return_state(&mut *scope, context, index);
        let this = scope.get(&this_h);
        let Some(return_state) =
            antlr_parser_atn_state_by_number(&mut *scope, this, return_state_number)?
        else {
            return Ok(Some(Value::Int(0)));
        };
        if antlr_atn_state_rule_index(&mut *scope, return_state) != state_rule {
            return Ok(Some(Value::Int(0)));
        }
    }

    let state = scope.get(&state_h);
    let Some(first_transition) = antlr_atn_state_transition_ref(&mut *scope, state, 0)? else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(decision_start_state) = antlr_transition_target(&mut *scope, first_transition) else {
        return Ok(Some(Value::Int(0)));
    };
    let Some(block_end_state) = antlr_ref_field(&mut *scope, decision_start_state, "endState", 5)
    else {
        return Ok(Some(Value::Int(0)));
    };
    let block_end_h = scope.root(block_end_state);

    for index in 0..num_contexts {
        let context = scope.get(&context_h);
        let return_state_number =
            antlr_prediction_context_return_state(&mut *scope, context, index);
        let this = scope.get(&this_h);
        let Some(return_state) =
            antlr_parser_atn_state_by_number(&mut *scope, this, return_state_number)?
        else {
            return Ok(Some(Value::Int(0)));
        };
        let return_state_h = scope.root(return_state);
        let transition_args = [Value::Object(Some(scope.get(&return_state_h)))];
        if match native_antlr_atn_state_get_number_of_transitions(&mut *scope, &transition_args)? {
            Some(Value::Int(v)) => v,
            _ => 0,
        } != 1
        {
            return Ok(Some(Value::Int(0)));
        }

        let return_state = scope.get(&return_state_h);
        let Some(edge) = antlr_atn_state_transition_ref(&mut *scope, return_state, 0)? else {
            return Ok(Some(Value::Int(0)));
        };
        let edge_h = scope.root(edge);
        let epsilon_args = [Value::Object(Some(scope.get(&edge_h)))];
        let edge_is_epsilon = match native_antlr_transition_is_epsilon(&mut *scope, &epsilon_args)?
        {
            Some(Value::Int(v)) => v != 0,
            _ => false,
        };
        if !edge_is_epsilon {
            return Ok(Some(Value::Int(0)));
        }
        let edge = scope.get(&edge_h);
        let Some(return_state_target) = antlr_transition_target(&mut *scope, edge) else {
            return Ok(Some(Value::Int(0)));
        };
        let return_state_target_h = scope.root(return_state_target);

        let return_state = scope.get(&return_state_h);
        let return_state_target = scope.get(&return_state_target_h);
        let state = scope.get(&state_h);
        let block_end_state = scope.get(&block_end_h);
        if antlr_atn_state_type(&mut *scope, return_state) == ATN_STATE_BLOCK_END
            && return_state_target == state
        {
            continue;
        }
        if return_state == block_end_state || return_state_target == block_end_state {
            continue;
        }
        if antlr_atn_state_type(&mut *scope, return_state_target) == ATN_STATE_BLOCK_END {
            let target_args = [Value::Object(Some(scope.get(&return_state_target_h)))];
            if match native_antlr_atn_state_get_number_of_transitions(&mut *scope, &target_args)? {
                Some(Value::Int(v)) => v,
                _ => 0,
            } == 1
            {
                let return_state_target = scope.get(&return_state_target_h);
                let Some(target_edge) =
                    antlr_atn_state_transition_ref(&mut *scope, return_state_target, 0)?
                else {
                    return Ok(Some(Value::Int(0)));
                };
                let target_edge_h = scope.root(target_edge);
                let target_epsilon_args = [Value::Object(Some(scope.get(&target_edge_h)))];
                let target_edge_is_epsilon =
                    match native_antlr_transition_is_epsilon(&mut *scope, &target_epsilon_args)? {
                        Some(Value::Int(v)) => v != 0,
                        _ => false,
                    };
                let target_edge = scope.get(&target_edge_h);
                let state = scope.get(&state_h);
                if target_edge_is_epsilon
                    && antlr_transition_target(&mut *scope, target_edge) == Some(state)
                {
                    continue;
                }
            }
        }

        return Ok(Some(Value::Int(0)));
    }

    Ok(Some(Value::Int(1)))
}

fn antlr_transition_serialization_type_from_class_name(class_name: &str) -> Option<i32> {
    if !antlr_is_runtime_class_name(class_name) {
        return None;
    }
    if class_name.ends_with("/atn/EpsilonTransition") {
        Some(1)
    } else if class_name.ends_with("/atn/RangeTransition") {
        Some(2)
    } else if class_name.ends_with("/atn/RuleTransition") {
        Some(3)
    } else if class_name.ends_with("/atn/PredicateTransition") {
        Some(4)
    } else if class_name.ends_with("/atn/AtomTransition") {
        Some(5)
    } else if class_name.ends_with("/atn/ActionTransition") {
        Some(6)
    } else if class_name.ends_with("/atn/SetTransition") {
        Some(7)
    } else if class_name.ends_with("/atn/NotSetTransition") {
        Some(8)
    } else if class_name.ends_with("/atn/WildcardTransition") {
        Some(9)
    } else if class_name.ends_with("/atn/PrecedencePredicateTransition") {
        Some(10)
    } else {
        None
    }
}

fn native_antlr_transition_get_serialization_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    Ok(Some(Value::Int(
        antlr_transition_serialization_type_from_class_name(&class_name).unwrap_or(0),
    )))
}

fn antlr_transition_is_epsilon_class(class_name: &str) -> bool {
    antlr_is_runtime_class_name(class_name)
        && (class_name.ends_with("/atn/EpsilonTransition")
            || class_name.ends_with("/atn/RuleTransition")
            || class_name.ends_with("/atn/PredicateTransition")
            || class_name.ends_with("/atn/ActionTransition")
            || class_name.ends_with("/atn/PrecedencePredicateTransition"))
}

fn native_antlr_transition_is_epsilon(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    Ok(Some(antlr_bool(antlr_transition_is_epsilon_class(
        &class_name,
    ))))
}

fn antlr_interval_set_intervals(ctx: &mut dyn NativeContext, set: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, set, "intervals", 0)
}

fn antlr_interval_range(ctx: &mut dyn NativeContext, interval: ObjectRef) -> (i32, i32) {
    (
        antlr_int_field(ctx, interval, "a", 0),
        antlr_int_field(ctx, interval, "b", 1),
    )
}

fn antlr_list_size(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    if antlr_is_java_arraylist(ctx, list) {
        return Ok(antlr_arraylist_size(ctx, list));
    }

    let mut scope = NativeHandleScope::new(ctx);
    let list_h = scope.root(list);
    let list = scope.get(&list_h);
    match scope.invoke_virtual(list, "size", "()I", &[])? {
        Some(Value::Int(size)) if size > 0 => Ok(size as usize),
        _ => Ok(0),
    }
}

fn antlr_list_get(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    index: usize,
) -> Result<Value, MethodCallFailed> {
    if antlr_is_java_arraylist(ctx, list) {
        let Some(data) = antlr_arraylist_data(ctx, list) else {
            return Err(RuntimeError::NullPointerException {
                message: Some("ArrayList.elementData".to_string()),
            }
            .into());
        };
        if index >= antlr_arraylist_size(ctx, list) || index >= ctx.array_length(data) {
            return Err(RuntimeError::aioobe_index_only(index as i32).into());
        }
        return Ok(ctx.get_array_element(data, index));
    }

    let mut scope = NativeHandleScope::new(ctx);
    let list_h = scope.root(list);
    let list = scope.get(&list_h);
    let result = scope.invoke_virtual(
        list,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Int(index as i32)],
    )?;
    Ok(result.unwrap_or(Value::Object(None)))
}

fn antlr_interval_set_contains(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
    symbol: i32,
) -> Result<bool, MethodCallFailed> {
    let Some(intervals) = antlr_interval_set_intervals(ctx, set) else {
        return Err(RuntimeError::NullPointerException {
            message: Some("IntervalSet.intervals".to_string()),
        }
        .into());
    };
    // `antlr_list_size` / `antlr_list_get` fall through to `List.size()` /
    // `List.get(int)` bytecode for any non-`java/util/ArrayList` container, so
    // the interval list itself must survive the binary search.
    let mut scope = NativeHandleScope::new(ctx);
    let intervals_h = scope.root(intervals);
    let intervals = scope.get(&intervals_h);
    let size = antlr_list_size(&mut *scope, intervals)?;
    if size == 0 {
        return Ok(false);
    }
    let mut low = 0usize;
    let mut high = size - 1;
    while low <= high {
        let mid = (low + high) / 2;
        let intervals = scope.get(&intervals_h);
        let interval = match antlr_list_get(&mut *scope, intervals, mid)? {
            Value::Object(Some(interval)) => interval,
            _ => return Ok(false),
        };
        let (start, end) = antlr_interval_range(&mut *scope, interval);
        if symbol < start {
            if mid == 0 {
                break;
            }
            high = mid - 1;
        } else if symbol > end {
            low = mid + 1;
        } else {
            return Ok(true);
        }
    }
    Ok(false)
}

fn native_antlr_interval_set_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let symbol = match args.get(1) {
        Some(Value::Int(symbol)) => *symbol,
        _ => 0,
    };
    Ok(Some(antlr_bool(antlr_interval_set_contains(
        ctx, this, symbol,
    )?)))
}

// The interpreter currently loses ANTLR EPSILON during LL1 recovery lookahead.
// sync is an early-recovery hint; adaptive prediction still decides the parse.
fn native_antlr_default_error_strategy_sync(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ANTLR's inline recovery should classify EOF after a statement as a missing
// delimiter when the sole expected token is that delimiter.  The interpreter
// reaches the same recovery state but reports an InputMismatchException
// instead, which prevents clients such as Hibernate's import-script listener
// from turning the syntax error into their domain exception.  Keep the normal
// ANTLR message for every other mismatch and correct only this EOF insertion
// boundary.
fn native_antlr_default_error_strategy_report_input_mismatch(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let strategy = obj_arg(args, 0)?;
    let parser = obj_arg(args, 1)?;
    let mismatch = obj_arg(args, 2)?;
    let names = antlr_names_for_object(ctx, parser);
    let prefix = if names.pc == GROOVY_ANTLR_PC {
        "groovyjarjarantlr4/v4/runtime"
    } else {
        "org/antlr/v4/runtime"
    };
    let token_desc = format!("L{prefix}/Token;");
    let interval_set_desc = format!("L{prefix}/misc/IntervalSet;");
    let vocabulary_desc = format!("L{prefix}/Vocabulary;");
    let recognition_desc = format!("L{prefix}/RecognitionException;");

    // Seven Java calls in a row, each of which can collect, with `strategy`,
    // `parser`, `mismatch`, `offending` and `expected` all still needed at the
    // end. Root the lot once and re-read every use.
    let mut scope = NativeHandleScope::new(ctx);
    let strategy_h = scope.root(strategy);
    let parser_h = scope.root(parser);
    let mismatch_h = scope.root(mismatch);

    let mismatch = scope.get(&mismatch_h);
    let offending = match scope.invoke_virtual(
        mismatch,
        "getOffendingToken",
        &format!("(){token_desc}"),
        &[],
    )? {
        Some(Value::Object(Some(token))) => token,
        _ => return Ok(None),
    };
    let offending_h = scope.root(offending);
    let mismatch = scope.get(&mismatch_h);
    let expected = match scope.invoke_virtual(
        mismatch,
        "getExpectedTokens",
        &format!("(){interval_set_desc}"),
        &[],
    )? {
        Some(Value::Object(Some(set))) => set,
        _ => return Ok(None),
    };
    let expected_h = scope.root(expected);
    let offending = scope.get(&offending_h);
    let token_type = match scope.invoke_virtual(offending, "getType", "()I", &[])? {
        Some(Value::Int(value)) => value,
        _ => 0,
    };
    let expected = scope.get(&expected_h);
    let expected_size = match scope.invoke_virtual(expected, "size", "()I", &[])? {
        Some(Value::Int(value)) => value,
        _ => 0,
    };
    let parser = scope.get(&parser_h);
    let vocabulary = match scope.invoke_virtual(
        parser,
        "getVocabulary",
        &format!("(){vocabulary_desc}"),
        &[],
    )? {
        Some(Value::Object(Some(vocabulary))) => vocabulary,
        _ => return Ok(None),
    };
    let vocabulary_h = scope.root(vocabulary);
    let expected = scope.get(&expected_h);
    let vocabulary_args = [Value::Object(Some(scope.get(&vocabulary_h)))];
    let expected_text = match scope.invoke_virtual(
        expected,
        "toString",
        &format!("({vocabulary_desc})Ljava/lang/String;"),
        &vocabulary_args,
    )? {
        Some(Value::Object(Some(text))) => scope.read_string(text).unwrap_or_default(),
        _ => String::new(),
    };
    let strategy = scope.get(&strategy_h);
    let offending_args = [Value::Object(Some(scope.get(&offending_h)))];
    let token_text = match scope.invoke_virtual(
        strategy,
        "getTokenErrorDisplay",
        &format!("({token_desc})Ljava/lang/String;"),
        &offending_args,
    )? {
        Some(Value::Object(Some(text))) => scope.read_string(text).unwrap_or_default(),
        _ => String::new(),
    };
    let message = if token_type == -1 && expected_size == 1 {
        format!("missing {expected_text} at {token_text}")
    } else {
        format!("mismatched input {token_text} expecting {expected_text}")
    };
    let message = scope.create_string(&message);
    let message_h = scope.root(message);
    let notify_args = [
        Value::Object(Some(scope.get(&offending_h))),
        Value::Object(Some(scope.get(&message_h))),
        Value::Object(Some(scope.get(&mismatch_h))),
    ];
    let parser = scope.get(&parser_h);
    scope.invoke_virtual(
        parser,
        "notifyErrorListeners",
        &format!("({token_desc}Ljava/lang/String;{recognition_desc})V"),
        &notify_args,
    )?;
    Ok(None)
}

fn native_antlr_transition_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let symbol = match args.get(1) {
        Some(Value::Int(symbol)) => *symbol,
        _ => 0,
    };
    let min_vocab_symbol = match args.get(2) {
        Some(Value::Int(symbol)) => *symbol,
        _ => 0,
    };
    let max_vocab_symbol = match args.get(3) {
        Some(Value::Int(symbol)) => *symbol,
        _ => 0,
    };
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    let matches = if class_name.ends_with("/atn/RangeTransition") {
        let from = antlr_int_field(ctx, this, "from", 1);
        let to = antlr_int_field(ctx, this, "to", 2);
        from <= symbol && symbol <= to
    } else if class_name.ends_with("/atn/AtomTransition") {
        antlr_int_field(ctx, this, "label", 1) == symbol
    } else if class_name.ends_with("/atn/WildcardTransition") {
        min_vocab_symbol <= symbol && symbol <= max_vocab_symbol
    } else if class_name.ends_with("/atn/SetTransition")
        || class_name.ends_with("/atn/NotSetTransition")
    {
        let set = antlr_ref_field(ctx, this, "set", 1).ok_or_else(|| {
            RuntimeError::NullPointerException {
                message: Some("SetTransition.set".to_string()),
            }
        })?;
        let contained = antlr_interval_set_contains(ctx, set, symbol)?;
        if class_name.ends_with("/atn/NotSetTransition") {
            min_vocab_symbol <= symbol && symbol <= max_vocab_symbol && !contained
        } else {
            contained
        }
    } else {
        false
    };
    Ok(Some(antlr_bool(matches)))
}

fn antlr_semantic_context_hash(
    ctx: &mut dyn NativeContext,
    sem: Option<ObjectRef>,
) -> Result<i32, MethodCallFailed> {
    let Some(sem) = sem else {
        return Ok(0);
    };
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(sem))
        .unwrap_or_default();
    if antlr_class_name_matches(&class_name, "/atn/SemanticContext$Predicate") {
        let mut hash = 0;
        hash = antlr_murmur_update(hash, antlr_int_field(ctx, sem, "ruleIndex", 0));
        hash = antlr_murmur_update(hash, antlr_int_field(ctx, sem, "predIndex", 1));
        hash = antlr_murmur_update(
            hash,
            antlr_int_field(ctx, sem, "isCtxDependent", 2).signum(),
        );
        return Ok(antlr_murmur_finish(hash, 3));
    }
    if antlr_class_name_matches(&class_name, "/atn/SemanticContext$PrecedencePredicate") {
        return Ok(31i32.wrapping_add(antlr_int_field(ctx, sem, "precedence", 0)));
    }
    if antlr_class_name_matches(&class_name, "/atn/SemanticContext$AND")
        || antlr_class_name_matches(&class_name, "/atn/SemanticContext$OR")
    {
        let seed = match ctx.class_id_by_name(&class_name) {
            Some(class_id) => {
                let mut scope = NativeHandleScope::new(ctx);
                let sem_h = scope.root(sem);
                let mirror = scope.get_class_mirror(class_id);
                let seed = scope.identity_hash_code(mirror);
                let sem = scope.get(&sem_h);
                antlr_semantic_operands_hash(&mut *scope, sem, seed)?
            }
            None => ctx.identity_hash_code(sem),
        };
        return Ok(seed);
    }

    Ok(ctx.identity_hash_code(sem))
}

fn antlr_semantic_operands_hash(
    ctx: &mut dyn NativeContext,
    sem: ObjectRef,
    seed: i32,
) -> Result<i32, MethodCallFailed> {
    let Some(opnds) = antlr_ref_field(ctx, sem, "opnds", 0) else {
        return Ok(seed);
    };
    // `antlr_semantic_context_hash` mints a class mirror for AND/OR operands,
    // so the operand array has to be re-read on every iteration.
    let mut scope = NativeHandleScope::new(ctx);
    let opnds_h = scope.root(opnds);
    let mut hash = seed;
    let len = scope.array_length(opnds);
    for i in 0..len {
        let opnds = scope.get(&opnds_h);
        let operand = match scope.get_array_element(opnds, i) {
            Value::Object(obj) => obj,
            _ => None,
        };
        hash = antlr_murmur_update(hash, antlr_semantic_context_hash(&mut *scope, operand)?);
    }
    Ok(antlr_murmur_finish(hash, len as i32))
}

fn antlr_semantic_contexts_equal(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(false);
    };
    let a_name = ctx
        .class_name_of_id(ctx.class_id_of_object(a))
        .unwrap_or_default();
    let b_name = ctx
        .class_name_of_id(ctx.class_id_of_object(b))
        .unwrap_or_default();
    if a_name != b_name {
        return Ok(false);
    }
    if antlr_class_name_matches(&a_name, "/atn/SemanticContext$Predicate") {
        return Ok(antlr_int_field(ctx, a, "ruleIndex", 0)
            == antlr_int_field(ctx, b, "ruleIndex", 0)
            && antlr_int_field(ctx, a, "predIndex", 1) == antlr_int_field(ctx, b, "predIndex", 1)
            && antlr_int_field(ctx, a, "isCtxDependent", 2).signum()
                == antlr_int_field(ctx, b, "isCtxDependent", 2).signum());
    }
    if antlr_class_name_matches(&a_name, "/atn/SemanticContext$PrecedencePredicate") {
        return Ok(
            antlr_int_field(ctx, a, "precedence", 0) == antlr_int_field(ctx, b, "precedence", 0)
        );
    }
    if antlr_class_name_matches(&a_name, "/atn/SemanticContext$AND")
        || antlr_class_name_matches(&a_name, "/atn/SemanticContext$OR")
    {
        let a_ops = antlr_ref_field(ctx, a, "opnds", 0);
        let b_ops = antlr_ref_field(ctx, b, "opnds", 0);
        let (Some(a_ops), Some(b_ops)) = (a_ops, b_ops) else {
            return Ok(false);
        };
        // The recursive comparison below reaches `Object.equals` bytecode for
        // unrecognised operand shapes, so both operand arrays must be re-read
        // on every iteration.
        let mut scope = NativeHandleScope::new(ctx);
        let a_ops_h = scope.root(a_ops);
        let b_ops_h = scope.root(b_ops);
        let len = scope.array_length(a_ops);
        if len != scope.array_length(b_ops) {
            return Ok(false);
        }
        for i in 0..len {
            let a_ops = scope.get(&a_ops_h);
            let b_ops = scope.get(&b_ops_h);
            let a_op = match scope.get_array_element(a_ops, i) {
                Value::Object(obj) => obj,
                _ => None,
            };
            let b_op = match scope.get_array_element(b_ops, i) {
                Value::Object(obj) => obj,
                _ => None,
            };
            if !antlr_semantic_contexts_equal(&mut *scope, a_op, b_op)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }

    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let a = scope.get(&a_h);
    let equals_args = [Value::Object(Some(scope.get(&b_h)))];
    match scope.invoke_virtual(a, "equals", "(Ljava/lang/Object;)Z", &equals_args)? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

fn antlr_semantic_context_is_empty(ctx: &mut dyn NativeContext, sem: Option<ObjectRef>) -> bool {
    let Some(sem) = sem else {
        return false;
    };
    let Some(class_name) = ctx.class_name_of_id(ctx.class_id_of_object(sem)) else {
        return false;
    };
    if antlr_class_name_matches(&class_name, "/atn/SemanticContext$Empty") {
        return true;
    }
    antlr_is_groovy_runtime_class_name(&class_name)
        && antlr_class_name_matches(&class_name, "/atn/SemanticContext$Predicate")
        && antlr_int_field(ctx, sem, "ruleIndex", 0) == -1
        && antlr_int_field(ctx, sem, "predIndex", 1) == -1
}

fn antlr_semantic_context_is_precedence_predicate(
    ctx: &mut dyn NativeContext,
    sem: ObjectRef,
) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(sem))
        .as_deref()
        .map(|name| antlr_class_name_matches(name, "/atn/SemanticContext$PrecedencePredicate"))
        .unwrap_or(false)
}

// The operand list built while combining two SemanticContexts is a
// `Vec<NativeHandle>`, not a `Vec<ObjectRef>`: comparing, hashing and sorting
// operands all reach Java (`equals`, class mirrors), so a plain `Vec` of raw
// references would be stale by the time the operator object is allocated.
// The owning scope is threaded through instead of re-opened per helper, since
// the handles have to outlive each individual step.
fn antlr_semantic_push_unique(
    scope: &mut NativeHandleScope<'_>,
    operands: &mut Vec<NativeHandle>,
    candidate: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let candidate_h = scope.root(candidate);
    for index in 0..operands.len() {
        let existing = scope.get(&operands[index]);
        let candidate = scope.get(&candidate_h);
        if antlr_semantic_contexts_equal(&mut **scope, Some(existing), Some(candidate))? {
            return Ok(());
        }
    }
    operands.push(candidate_h);
    Ok(())
}

fn antlr_semantic_collect_operands(
    scope: &mut NativeHandleScope<'_>,
    operands: &mut Vec<NativeHandle>,
    candidate: ObjectRef,
    operator_suffix: &str,
) -> Result<(), MethodCallFailed> {
    let candidate_h = scope.root(candidate);
    let class_name = scope
        .class_name_of_id(scope.class_id_of_object(candidate))
        .unwrap_or_default();
    if antlr_class_name_matches(&class_name, operator_suffix) {
        let candidate = scope.get(&candidate_h);
        if let Some(opnds) = antlr_ref_field(&mut **scope, candidate, "opnds", 0) {
            let opnds_h = scope.root(opnds);
            let len = scope.array_length(opnds);
            for index in 0..len {
                let opnds = scope.get(&opnds_h);
                if let Value::Object(Some(operand)) = scope.get_array_element(opnds, index) {
                    antlr_semantic_push_unique(scope, operands, operand)?;
                }
            }
            return Ok(());
        }
    }
    let candidate = scope.get(&candidate_h);
    antlr_semantic_push_unique(scope, operands, candidate)
}

fn antlr_semantic_filter_precedence(
    scope: &mut NativeHandleScope<'_>,
    operands: &mut Vec<NativeHandle>,
    take_min: bool,
) -> Result<(), MethodCallFailed> {
    let mut filtered: Vec<NativeHandle> = Vec::with_capacity(operands.len());
    let mut selected: Option<(NativeHandle, i32)> = None;

    for handle in operands.drain(..) {
        let operand = scope.get(&handle);
        if antlr_semantic_context_is_precedence_predicate(&mut **scope, operand) {
            let precedence = antlr_int_field(&mut **scope, operand, "precedence", 0);
            let keep = selected
                .as_ref()
                .map(|(_, current)| {
                    if take_min {
                        precedence < *current
                    } else {
                        precedence > *current
                    }
                })
                .unwrap_or(true);
            if keep {
                selected = Some((handle, precedence));
            }
        } else {
            filtered.push(handle);
        }
    }

    if let Some((handle, _)) = selected {
        let operand = scope.get(&handle);
        antlr_semantic_push_unique(scope, &mut filtered, operand)?;
    }
    *operands = filtered;
    Ok(())
}

fn antlr_semantic_sort_operands(
    scope: &mut NativeHandleScope<'_>,
    operands: &mut Vec<NativeHandle>,
) -> Result<(), MethodCallFailed> {
    let mut keyed = Vec::with_capacity(operands.len());
    for handle in operands.drain(..) {
        let operand = scope.get(&handle);
        let class_name = scope
            .class_name_of_id(scope.class_id_of_object(operand))
            .unwrap_or_default();
        let hash = antlr_semantic_context_hash(&mut **scope, Some(operand))?;
        // `antlr_semantic_context_hash` mints a class mirror for AND/OR
        // operands and can therefore collect.
        let operand = scope.get(&handle);
        let precedence = antlr_int_field(&mut **scope, operand, "precedence", 0);
        let rule_index = antlr_int_field(&mut **scope, operand, "ruleIndex", 0);
        let pred_index = antlr_int_field(&mut **scope, operand, "predIndex", 1);
        let ctx_dependent = antlr_int_field(&mut **scope, operand, "isCtxDependent", 2).signum();
        let identity = scope.identity_hash_code(operand);
        keyed.push((
            hash,
            class_name,
            precedence,
            rule_index,
            pred_index,
            ctx_dependent,
            identity,
            handle,
        ));
    }
    keyed.sort_by(|a, b| {
        (&a.0, &a.1, &a.2, &a.3, &a.4, &a.5, &a.6).cmp(&(&b.0, &b.1, &b.2, &b.3, &b.4, &b.5, &b.6))
    });
    operands.extend(keyed.into_iter().map(|entry| entry.7));
    Ok(())
}

fn antlr_semantic_operator_object(
    scope: &mut NativeHandleScope<'_>,
    names: AntlrClassNames,
    operands: &[NativeHandle],
    is_and: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_name = if is_and {
        antlr_semantic_and_name(names)
    } else {
        antlr_semantic_or_name(names)
    };
    let operator_class = scope.ensure_class_initialized(class_name)?;
    let semantic_class = scope
        .ensure_class_initialized(antlr_semantic_context_name(names))
        .or_else(|_| scope.ensure_class_initialized(names.semantic_empty))?;
    let opnds = scope.new_ref_array(semantic_class, operands.len());
    let opnds_h = scope.root(opnds);
    let field_count = scope.class_num_total_fields(operator_class).max(1);
    let object = scope.alloc_object(operator_class, field_count);
    let object_h = scope.root(object);
    let opnds = scope.get(&opnds_h);
    for (index, handle) in operands.iter().enumerate() {
        let operand = scope.get(handle);
        scope.set_array_element(opnds, index, Value::Object(Some(operand)));
    }
    let object = scope.get(&object_h);
    let opnds = scope.get(&opnds_h);
    antlr_set_field_value(&mut **scope, object, "opnds", 0, Value::Object(Some(opnds)));
    Ok(scope.get(&object_h))
}

fn antlr_semantic_context_combine(
    ctx: &mut dyn NativeContext,
    left: ObjectRef,
    right: ObjectRef,
    is_and: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let names = antlr_names_for_object(ctx, left);
    let operator_suffix = if is_and {
        "/atn/SemanticContext$AND"
    } else {
        "/atn/SemanticContext$OR"
    };
    let mut scope = NativeHandleScope::new(ctx);
    let left_h = scope.root(left);
    let right_h = scope.root(right);
    let mut operands: Vec<NativeHandle> = Vec::with_capacity(4);
    let left = scope.get(&left_h);
    antlr_semantic_collect_operands(&mut scope, &mut operands, left, operator_suffix)?;
    let right = scope.get(&right_h);
    antlr_semantic_collect_operands(&mut scope, &mut operands, right, operator_suffix)?;
    antlr_semantic_filter_precedence(&mut scope, &mut operands, is_and)?;
    antlr_semantic_sort_operands(&mut scope, &mut operands)?;

    if operands.len() == 1 {
        return Ok(Some(scope.get(&operands[0])));
    }
    Ok(Some(antlr_semantic_operator_object(
        &mut scope, names, &operands, is_and,
    )?))
}

fn native_antlr_semantic_context_and(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let left = antlr_object_option_arg(args, 0)?;
    let right = antlr_object_option_arg(args, 1)?;
    let Some(left) = left else {
        return Ok(Some(Value::Object(right)));
    };
    if antlr_semantic_context_is_empty(ctx, Some(left)) {
        return Ok(Some(Value::Object(right)));
    }
    let Some(right) = right else {
        return Ok(Some(Value::Object(Some(left))));
    };
    if antlr_semantic_context_is_empty(ctx, Some(right)) {
        return Ok(Some(Value::Object(Some(left))));
    }
    Ok(Some(Value::Object(antlr_semantic_context_combine(
        ctx, left, right, true,
    )?)))
}

fn native_antlr_semantic_context_or(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let left = antlr_object_option_arg(args, 0)?;
    let right = antlr_object_option_arg(args, 1)?;
    let Some(left) = left else {
        return Ok(Some(Value::Object(right)));
    };
    let Some(right) = right else {
        return Ok(Some(Value::Object(Some(left))));
    };
    if antlr_semantic_context_is_empty(ctx, Some(left))
        || antlr_semantic_context_is_empty(ctx, Some(right))
    {
        let names = antlr_names_for_object(ctx, left);
        return Ok(Some(Value::Object(antlr_semantic_empty_instance(
            ctx, names,
        )?)));
    }
    Ok(Some(Value::Object(antlr_semantic_context_combine(
        ctx, left, right, false,
    )?)))
}

fn antlr_alloc_atn_config(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    ctor_args: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let arg_handles: Vec<_> = ctor_args
        .iter()
        .map(|arg| antlr_root_value(&mut scope, *arg))
        .collect();
    let config_class = scope.ensure_class_initialized(names.atn_config)?;
    let field_count = scope.class_num_total_fields(config_class).max(5);
    let config = scope.alloc_object(config_class, field_count);
    // The constructor intrinsic can initialize SemanticContext$Empty and
    // therefore collect, so the newly allocated receiver is rooted too.
    let config_h = scope.root(config);
    let mut init_args = Vec::with_capacity(ctor_args.len() + 1);
    init_args.push(Value::Object(Some(scope.get(&config_h))));
    for (arg, handle) in ctor_args.iter().copied().zip(arg_handles.iter()) {
        init_args.push(antlr_read_rooted_value(&scope, arg, handle.as_ref()));
    }
    native_antlr_atn_config_init(&mut *scope, &init_args)?;
    Ok(scope.get(&config_h))
}

fn antlr_atn_config_for_transition_target(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
    transition: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(target) = antlr_transition_target(ctx, transition) else {
        return Ok(None);
    };
    let names = antlr_names_for_object(ctx, config);
    Ok(Some(antlr_alloc_atn_config(
        ctx,
        names,
        &[Value::Object(Some(config)), Value::Object(Some(target))],
    )?))
}

fn antlr_parser_rule_transition(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
    transition: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // This path allocates the successor PredictionContext and then the
    // successor ATNConfig.  All three graph endpoints must survive both
    // allocations: in particular, `target` is read from `transition` and is
    // not necessarily reachable from the new context.  Releasing the original
    // config/transition pins before `antlr_alloc_atn_config` left these Rust
    // locals stale after a moving-young collection in compiled parser code.
    let mut scope = NativeHandleScope::new(ctx);
    let config_h = scope.root(config);
    let transition_h = scope.root(transition);

    let transition = scope.get(&transition_h);
    let Some(follow_state) = antlr_ref_field(&mut *scope, transition, "followState", 3) else {
        return Ok(None);
    };
    let config = scope.get(&config_h);
    let context = antlr_atn_config_context(&mut *scope, config);
    let return_state = antlr_int_field(&mut *scope, follow_state, "stateNumber", 1);
    let names = antlr_names_for_object(&mut *scope, config);
    let new_context = antlr_create_singleton_context(&mut *scope, names, context, return_state)?;
    let new_context_h = scope.root(new_context);

    let transition = scope.get(&transition_h);
    let Some(target) = antlr_transition_target(&mut *scope, transition) else {
        return Ok(None);
    };
    let ctor_args = [
        Value::Object(Some(scope.get(&config_h))),
        Value::Object(Some(target)),
        Value::Object(Some(scope.get(&new_context_h))),
    ];
    Ok(Some(antlr_alloc_atn_config(
        &mut *scope,
        names,
        &ctor_args,
    )?))
}

fn antlr_parser_transition_delegate_descriptor(
    names: AntlrClassNames,
    transition_name: &str,
    bool_count: usize,
) -> String {
    let prefix = if names.pc == GROOVY_ANTLR_PC {
        "groovyjarjarantlr4/v4/runtime"
    } else {
        "org/antlr/v4/runtime"
    };
    let bools = "Z".repeat(bool_count);
    format!(
        "(L{prefix}/atn/ATNConfig;L{prefix}/atn/{transition_name};{bools})L{prefix}/atn/ATNConfig;"
    )
}

fn antlr_parser_delegate_transition(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    method_name: &str,
    transition_name: &str,
    config: ObjectRef,
    transition: ObjectRef,
    bools: &[bool],
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let names = antlr_names_for_object(ctx, config);
    let descriptor =
        antlr_parser_transition_delegate_descriptor(names, transition_name, bools.len());
    // `predTransition` / `precedenceTransition` execute Java bytecode and may
    // allocate.  These three graph objects are then used to resume the native
    // closure walk, so retain GC-remapped copies for the duration of the call.
    let mut scope = NativeHandleScope::new(ctx);
    let simulator_h = scope.root(simulator);
    let config_h = scope.root(config);
    let transition_h = scope.root(transition);
    let mut args = Vec::with_capacity(2 + bools.len());
    args.push(Value::Object(Some(scope.get(&config_h))));
    args.push(Value::Object(Some(scope.get(&transition_h))));
    args.extend(bools.iter().map(|value| antlr_bool(*value)));
    let simulator = scope.get(&simulator_h);
    match scope.invoke_virtual(simulator, method_name, &descriptor, &args)? {
        Some(Value::Object(obj)) => Ok(obj),
        _ => Ok(None),
    }
}

fn native_antlr_parser_get_epsilon_target_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let transition = obj_arg(args, 2)?;
    let collect_predicates = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
    let in_context = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    let full_ctx = matches!(args.get(5), Some(Value::Int(v)) if *v != 0);
    let treat_eof_as_epsilon = matches!(args.get(6), Some(Value::Int(v)) if *v != 0);
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(transition))
        .unwrap_or_default();
    // `transitionMatches` walks an IntervalSet through `List.get(int)` and the
    // delegate branches run parser bytecode; both can collect while `config`
    // and `transition` are still needed by the branch that follows.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let config_h = scope.root(config);
    let transition_h = scope.root(transition);
    let target = match antlr_transition_serialization_type_from_class_name(&class_name) {
        Some(1) | Some(6) => {
            let config = scope.get(&config_h);
            let transition = scope.get(&transition_h);
            antlr_atn_config_for_transition_target(&mut *scope, config, transition)?
        }
        Some(2) | Some(5) | Some(7) => {
            let match_args = [
                Value::Object(Some(scope.get(&transition_h))),
                Value::Int(-1),
                Value::Int(0),
                Value::Int(1),
            ];
            let matches_eof = match native_antlr_transition_matches(&mut *scope, &match_args)? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if treat_eof_as_epsilon && matches_eof {
                let config = scope.get(&config_h);
                let transition = scope.get(&transition_h);
                antlr_atn_config_for_transition_target(&mut *scope, config, transition)?
            } else {
                None
            }
        }
        Some(3) => {
            let config = scope.get(&config_h);
            let transition = scope.get(&transition_h);
            antlr_parser_rule_transition(&mut *scope, config, transition)?
        }
        Some(4) => {
            let transition = scope.get(&transition_h);
            let is_ctx_dependent =
                antlr_int_field(&mut *scope, transition, "isCtxDependent", 3) != 0;
            let this = scope.get(&this_h);
            let config = scope.get(&config_h);
            let transition = scope.get(&transition_h);
            if collect_predicates && (!is_ctx_dependent || in_context) {
                antlr_parser_delegate_transition(
                    &mut *scope,
                    this,
                    "predTransition",
                    "PredicateTransition",
                    config,
                    transition,
                    &[collect_predicates, in_context, full_ctx],
                )?
            } else {
                antlr_atn_config_for_transition_target(&mut *scope, config, transition)?
            }
        }
        Some(10) => {
            let this = scope.get(&this_h);
            let config = scope.get(&config_h);
            let transition = scope.get(&transition_h);
            if collect_predicates && in_context {
                antlr_parser_delegate_transition(
                    &mut *scope,
                    this,
                    "precedenceTransition",
                    "PrecedencePredicateTransition",
                    config,
                    transition,
                    &[collect_predicates, in_context, full_ctx],
                )?
            } else {
                antlr_atn_config_for_transition_target(&mut *scope, config, transition)?
            }
        }
        _ => None,
    };
    Ok(Some(Value::Object(target)))
}

fn native_antlr_parser_get_epsilon_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let transition = obj_arg(args, 2)?;
    // Several branches call Java and then return to the native closure walk.
    // The scope closes on every exit path, including the one where the Java
    // call propagates an exception.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let config_h = scope.root(config);
    let transition_h = scope.root(transition);
    let rooted = [
        Value::Object(Some(scope.get(&this_h))),
        Value::Object(Some(scope.get(&config_h))),
        Value::Object(Some(scope.get(&transition_h))),
        args.get(3).cloned().unwrap_or(Value::Int(0)),
        args.get(4).cloned().unwrap_or(Value::Int(0)),
        args.get(5).cloned().unwrap_or(Value::Int(0)),
        args.get(6).cloned().unwrap_or(Value::Int(0)),
    ];
    native_antlr_parser_get_epsilon_target_impl(&mut *scope, &rooted)
}

fn antlr_parser_merge_cache(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
) -> Option<ObjectRef> {
    antlr_ref_field(ctx, simulator, "mergeCache", 5)
}

fn antlr_parser_add_config(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    configs: ObjectRef,
    config: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let merge_cache = antlr_parser_merge_cache(ctx, simulator);
    match antlr_atn_config_set_add_impl(ctx, configs, config, merge_cache)? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

fn antlr_set_add_object(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
    value: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let set_h = scope.root(set);
    let value_h = scope.root(value);
    let set = scope.get(&set_h);
    let add_args = [Value::Object(Some(scope.get(&value_h)))];
    match scope.invoke_virtual(set, "add", "(Ljava/lang/Object;)Z", &add_args)? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

fn antlr_parser_dfa(ctx: &mut dyn NativeContext, simulator: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, simulator, "_dfa", 9)
}

fn antlr_dfa_is_precedence(ctx: &mut dyn NativeContext, dfa: ObjectRef) -> bool {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(dfa))
        .unwrap_or_default();
    if let Some(slot) = ctx.resolve_field_index(&class_name, "precedenceDfa") {
        return matches!(ctx.get_field(dfa, slot), Value::Int(v) if v != 0);
    }
    match ctx.invoke_virtual(dfa, "isPrecedenceDfa", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => v != 0,
        _ => false,
    }
}

fn antlr_dfa_start_rule(ctx: &mut dyn NativeContext, dfa: ObjectRef) -> Option<i32> {
    let start = antlr_ref_field(ctx, dfa, "atnStartState", 3)?;
    Some(antlr_atn_state_rule_index(ctx, start))
}

fn antlr_transition_is_action(ctx: &mut dyn NativeContext, transition: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(transition))
        .as_deref()
        .map(|name| antlr_class_name_matches(name, "/atn/ActionTransition"))
        .unwrap_or(false)
}

fn antlr_transition_is_rule(ctx: &mut dyn NativeContext, transition: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(transition))
        .as_deref()
        .map(|name| antlr_class_name_matches(name, "/atn/RuleTransition"))
        .unwrap_or(false)
}

fn antlr_transition_is_epsilon(ctx: &mut dyn NativeContext, transition: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(transition))
        .as_deref()
        .map(antlr_transition_is_epsilon_class)
        .unwrap_or(false)
}

fn antlr_state_is_rule_stop(ctx: &mut dyn NativeContext, state: ObjectRef) -> bool {
    antlr_atn_state_type(ctx, state) == 7
}

fn antlr_parser_native_get_epsilon_target(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    config: ObjectRef,
    transition: ObjectRef,
    collect_predicates: bool,
    in_context: bool,
    full_ctx: bool,
    treat_eof_as_epsilon: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    match native_antlr_parser_get_epsilon_target(
        ctx,
        &[
            Value::Object(Some(simulator)),
            Value::Object(Some(config)),
            Value::Object(Some(transition)),
            antlr_bool(collect_predicates),
            antlr_bool(in_context),
            antlr_bool(full_ctx),
            antlr_bool(treat_eof_as_epsilon),
        ],
    )? {
        Some(Value::Object(obj)) => Ok(obj),
        _ => Ok(None),
    }
}

/// The four objects that make up one closure step's live prediction graph,
/// held as scope handles rather than raw references.
///
/// Every recursive edge of the closure walk allocates, so the walk re-reads
/// all four through [`Self::read`] after each one instead of carrying Rust
/// locals across the collection point.
struct AntlrClosureRoots {
    simulator: NativeHandle,
    config: NativeHandle,
    configs: NativeHandle,
    closure_busy: NativeHandle,
}

impl AntlrClosureRoots {
    /// Root all four in `scope`. The handles stay valid for as long as the
    /// scope (or any scope nested inside it) is open.
    fn new(
        scope: &mut NativeHandleScope<'_>,
        simulator: ObjectRef,
        config: ObjectRef,
        configs: ObjectRef,
        closure_busy: ObjectRef,
    ) -> Self {
        Self {
            simulator: scope.root(simulator),
            config: scope.root(config),
            configs: scope.root(configs),
            closure_busy: scope.root(closure_busy),
        }
    }

    /// Current addresses of all four. Nested scopes read the same per-thread
    /// slot table, so an inner scope resolves these handles just as well.
    #[inline]
    fn read(&self, scope: &NativeHandleScope<'_>) -> (ObjectRef, ObjectRef, ObjectRef, ObjectRef) {
        (
            scope.get(&self.simulator),
            scope.get(&self.config),
            scope.get(&self.configs),
            scope.get(&self.closure_busy),
        )
    }
}

fn antlr_parser_closure_checking_stop_state_unrooted(
    scope: &mut NativeHandleScope<'_>,
    roots: &AntlrClosureRoots,
    collect_predicates: bool,
    full_ctx: bool,
    depth: i32,
    treat_eof_as_epsilon: bool,
) -> MethodCallResult {
    let (mut simulator, mut config, mut configs, mut closure_busy) = roots.read(scope);
    let Some(state) = antlr_ref_field(&mut **scope, config, "state", 0) else {
        return Ok(None);
    };

    if antlr_state_is_rule_stop(&mut **scope, state) {
        let context = antlr_atn_config_context(&mut **scope, config);
        if let Some(context) = context {
            if !antlr_prediction_context_is_empty(&mut **scope, context) {
                let size = antlr_prediction_context_size(&mut **scope, context);
                for index in 0..size {
                    // A previous recursive edge can collect.  Fetch the
                    // entire graph through the handle scope before reading
                    // this slot instead of retaining raw Rust locals.
                    (simulator, config, configs, closure_busy) = roots.read(scope);
                    let Some(context) = antlr_atn_config_context(&mut **scope, config) else {
                        continue;
                    };
                    let return_state =
                        antlr_prediction_context_return_state(&mut **scope, context, index);
                    if return_state == ANTLR_EMPTY_RETURN_STATE {
                        if full_ctx {
                            let names = antlr_names_for_object(&mut **scope, config);
                            let empty = antlr_empty_instance(&mut **scope, names)?;
                            let empty_h = scope.root(empty);
                            (simulator, config, configs, closure_busy) = roots.read(scope);
                            let Some(state) = antlr_ref_field(&mut **scope, config, "state", 0)
                            else {
                                continue;
                            };
                            let ctor_args = [
                                Value::Object(Some(config)),
                                Value::Object(Some(state)),
                                Value::Object(Some(scope.get(&empty_h))),
                            ];
                            let next_config =
                                antlr_alloc_atn_config(&mut **scope, names, &ctor_args)?;
                            let next_config_h = scope.root(next_config);
                            (simulator, config, configs, closure_busy) = roots.read(scope);
                            let next_config = scope.get(&next_config_h);
                            antlr_parser_add_config(&mut **scope, simulator, configs, next_config)?;
                        } else {
                            (simulator, config, configs, closure_busy) = roots.read(scope);
                            antlr_parser_closure_impl(
                                &mut **scope,
                                simulator,
                                config,
                                configs,
                                closure_busy,
                                collect_predicates,
                                full_ctx,
                                depth,
                                treat_eof_as_epsilon,
                            )?;
                        }
                    } else {
                        (simulator, config, configs, closure_busy) = roots.read(scope);
                        let Some(return_state_obj) = antlr_parser_atn_state_by_number(
                            &mut **scope,
                            simulator,
                            return_state,
                        )?
                        else {
                            continue;
                        };
                        // State lookup may dispatch through a non-ArrayList
                        // implementation. Refresh the context before deriving
                        // constructor arguments from it.
                        let return_state_h = scope.root(return_state_obj);
                        (simulator, config, configs, closure_busy) = roots.read(scope);
                        let Some(context) = antlr_atn_config_context(&mut **scope, config) else {
                            continue;
                        };
                        let parent = antlr_prediction_context_parent(&mut **scope, context, index);
                        let semantic_context =
                            antlr_atn_config_semantic_context(&mut **scope, config);
                        let alt = antlr_atn_config_alt(&mut **scope, config);
                        let reaches = antlr_atn_config_reaches(&mut **scope, config);
                        let names = antlr_names_for_object(&mut **scope, config);
                        let ctor_args = [
                            Value::Object(Some(scope.get(&return_state_h))),
                            Value::Int(alt),
                            Value::Object(parent),
                            Value::Object(semantic_context),
                        ];
                        let next_config = antlr_alloc_atn_config(&mut **scope, names, &ctor_args)?;
                        let next_config_h = scope.root(next_config);
                        (simulator, config, configs, closure_busy) = roots.read(scope);
                        let next_config = scope.get(&next_config_h);
                        antlr_atn_config_set_reaches(&mut **scope, next_config, reaches);
                        antlr_parser_closure_checking_stop_state_impl(
                            &mut **scope,
                            simulator,
                            next_config,
                            configs,
                            closure_busy,
                            collect_predicates,
                            full_ctx,
                            depth.saturating_sub(1),
                            treat_eof_as_epsilon,
                        )?;
                    }
                }
                return Ok(None);
            }
        }

        if full_ctx {
            (simulator, config, configs, closure_busy) = roots.read(scope);
            antlr_parser_add_config(&mut **scope, simulator, configs, config)?;
            return Ok(None);
        }
    }

    (simulator, config, configs, closure_busy) = roots.read(scope);
    antlr_parser_closure_impl(
        &mut **scope,
        simulator,
        config,
        configs,
        closure_busy,
        collect_predicates,
        full_ctx,
        depth,
        treat_eof_as_epsilon,
    )
}

#[allow(clippy::too_many_arguments)]
fn antlr_parser_closure_unrooted(
    scope: &mut NativeHandleScope<'_>,
    roots: &AntlrClosureRoots,
    collect_predicates: bool,
    full_ctx: bool,
    depth: i32,
    treat_eof_as_epsilon: bool,
) -> MethodCallResult {
    let (mut simulator, mut config, mut configs, mut _closure_busy) = roots.read(scope);
    let Some(state) = antlr_ref_field(&mut **scope, config, "state", 0) else {
        return Ok(None);
    };

    if antlr_int_field(&mut **scope, state, "epsilonOnlyTransitions", 3) == 0 {
        antlr_parser_add_config(&mut **scope, simulator, configs, config)?;
        (simulator, config, configs, _closure_busy) = roots.read(scope);
    }

    // `add` can allocate.  Re-read `state` from the rooted config before
    // dereferencing its transition array.
    let Some(state) = antlr_ref_field(&mut **scope, config, "state", 0) else {
        return Ok(None);
    };
    let count_args = [Value::Object(Some(state))];
    let transition_count =
        match native_antlr_atn_state_get_number_of_transitions(&mut **scope, &count_args)? {
            Some(Value::Int(v)) if v > 0 => v as usize,
            _ => 0,
        };
    for transition_index in 0..transition_count {
        // One nested scope per edge: the per-edge handles (transition, epsilon
        // target, dfa) are released at the end of the iteration instead of
        // accumulating across the whole transition list, while the four
        // closure roots stay resolvable — nested scopes read the same
        // per-thread slot table.
        let mut scope = NativeHandleScope::new(&mut **scope);
        let (mut simulator, mut config, mut configs, mut closure_busy) = roots.read(&scope);
        if transition_index == 0 {
            let drop_args = [Value::Object(Some(simulator)), Value::Object(Some(config))];
            let can_drop =
                match native_antlr_parser_can_drop_loop_entry_edge(&mut *scope, &drop_args)? {
                    Some(Value::Int(v)) => v != 0,
                    _ => false,
                };
            if can_drop {
                continue;
            }
            (simulator, config, configs, closure_busy) = roots.read(&scope);
        }

        // The prior iteration (and the loop-entry predicate above) may have
        // allocated.  Do not carry the old raw ATNState reference across it.
        let Some(state) = antlr_ref_field(&mut *scope, config, "state", 0) else {
            continue;
        };
        let Some(transition) =
            antlr_atn_state_transition_ref(&mut *scope, state, transition_index)?
        else {
            continue;
        };
        let transition_h = scope.root(transition);
        let transition = scope.get(&transition_h);
        let continue_collecting =
            collect_predicates && !antlr_transition_is_action(&mut *scope, transition);
        // `inContext` mirrors real ANTLR's `getEpsilonTarget(config, t, collectPredicates,
        // depth == 0, fullCtx, treatEofAsEpsilon)` call in `ParserATNSimulator.closure_` —
        // it must track whether this epsilon walk is still within the rule invocation
        // closure started from (depth == 0), not merely "are we in full-context mode"
        // (`!full_ctx`, which is constant for the whole SLL closure and never reflects
        // having dipped into an outer context). Passing `!full_ctx` here made every
        // SLL-mode precedence/predicate transition look "in context" even after falling
        // off a rule with an exhausted (empty) context, wrongly attaching a precedence
        // predicate that real ANTLR would have suppressed — corrupting prediction on the
        // second+ visit to the same left-recursive loop decision within one parse.
        (simulator, config, configs, closure_busy) = roots.read(&scope);
        let transition = scope.get(&transition_h);
        let Some(next_config) = antlr_parser_native_get_epsilon_target(
            &mut *scope,
            simulator,
            config,
            transition,
            continue_collecting,
            depth == 0,
            full_ctx,
            treat_eof_as_epsilon,
        )?
        else {
            continue;
        };
        // The epsilon target itself is often a freshly allocated config.  It
        // must remain rooted while the native walk adds it to Java sets and
        // recursively resumes prediction.
        let next_config_h = scope.root(next_config);

        let mut next_depth = depth;
        // `getEpsilonTarget` can collect; reload the state through the rooted
        // config before using it below.
        (simulator, config, configs, closure_busy) = roots.read(&scope);
        let Some(state) = antlr_ref_field(&mut *scope, config, "state", 0) else {
            continue;
        };
        if antlr_state_is_rule_stop(&mut *scope, state) {
            if let Some(dfa) = antlr_parser_dfa(&mut *scope, simulator) {
                let dfa_h = scope.root(dfa);
                let dfa = scope.get(&dfa_h);
                if antlr_dfa_is_precedence(&mut *scope, dfa) {
                    (simulator, config, configs, closure_busy) = roots.read(&scope);
                    let transition = scope.get(&transition_h);
                    let next_config = scope.get(&next_config_h);
                    let dfa = scope.get(&dfa_h);
                    let outermost =
                        antlr_int_field(&mut *scope, transition, "outermostPrecedenceReturn", 1);
                    if Some(outermost) == antlr_dfa_start_rule(&mut *scope, dfa) {
                        antlr_atn_config_set_precedence_suppressed(&mut *scope, next_config);
                    }
                }
            }
            let next_config = scope.get(&next_config_h);
            let reaches = antlr_atn_config_reaches(&mut *scope, next_config).saturating_add(1);
            antlr_atn_config_set_reaches(&mut *scope, next_config, reaches);
            (simulator, config, configs, closure_busy) = roots.read(&scope);
            let next_config = scope.get(&next_config_h);
            if !antlr_set_add_object(&mut *scope, closure_busy, next_config)? {
                continue;
            }
            (simulator, config, configs, closure_busy) = roots.read(&scope);
            antlr_set_field_value(
                &mut *scope,
                configs,
                "dipsIntoOuterContext",
                6,
                Value::Int(1),
            );
            next_depth = next_depth.saturating_sub(1);
        } else {
            let transition = scope.get(&transition_h);
            if !antlr_transition_is_epsilon(&mut *scope, transition) {
                (simulator, config, configs, closure_busy) = roots.read(&scope);
                let next_config = scope.get(&next_config_h);
                if !antlr_set_add_object(&mut *scope, closure_busy, next_config)? {
                    continue;
                }
            }
        }

        let transition = scope.get(&transition_h);
        if antlr_transition_is_rule(&mut *scope, transition) && next_depth >= 0 {
            next_depth = next_depth.saturating_add(1);
        }

        (simulator, config, configs, closure_busy) = roots.read(&scope);
        let _ = config;
        let next_config = scope.get(&next_config_h);
        antlr_parser_closure_checking_stop_state_impl(
            &mut *scope,
            simulator,
            next_config,
            configs,
            closure_busy,
            continue_collecting,
            full_ctx,
            next_depth,
            treat_eof_as_epsilon,
        )?;
    }

    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn antlr_parser_closure_checking_stop_state_impl(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    config: ObjectRef,
    configs: ObjectRef,
    closure_busy: ObjectRef,
    collect_predicates: bool,
    full_ctx: bool,
    depth: i32,
    treat_eof_as_epsilon: bool,
) -> MethodCallResult {
    // Every recursive closure step may allocate or invoke Java.  The four
    // arguments form the live prediction graph, so root all of them for the
    // duration of the step rather than trusting raw ObjectRefs across a
    // moving collection.
    let mut scope = NativeHandleScope::new(ctx);
    let roots = AntlrClosureRoots::new(&mut scope, simulator, config, configs, closure_busy);
    antlr_parser_closure_checking_stop_state_unrooted(
        &mut scope,
        &roots,
        collect_predicates,
        full_ctx,
        depth,
        treat_eof_as_epsilon,
    )
}

#[allow(clippy::too_many_arguments)]
fn antlr_parser_closure_impl(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    config: ObjectRef,
    configs: ObjectRef,
    closure_busy: ObjectRef,
    collect_predicates: bool,
    full_ctx: bool,
    depth: i32,
    treat_eof_as_epsilon: bool,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let roots = AntlrClosureRoots::new(&mut scope, simulator, config, configs, closure_busy);
    antlr_parser_closure_unrooted(
        &mut scope,
        &roots,
        collect_predicates,
        full_ctx,
        depth,
        treat_eof_as_epsilon,
    )
}

fn native_antlr_parser_closure_checking_stop_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let configs = obj_arg(args, 2)?;
    let closure_busy = obj_arg(args, 3)?;
    let collect_predicates = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    let full_ctx = matches!(args.get(5), Some(Value::Int(v)) if *v != 0);
    let depth = match args.get(6) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let treat_eof_as_epsilon = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);
    antlr_parser_closure_checking_stop_state_impl(
        ctx,
        this,
        config,
        configs,
        closure_busy,
        collect_predicates,
        full_ctx,
        depth,
        treat_eof_as_epsilon,
    )
}

fn native_antlr_parser_closure(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let configs = obj_arg(args, 2)?;
    let closure_busy = obj_arg(args, 3)?;
    let collect_predicates = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    let full_ctx = matches!(args.get(5), Some(Value::Int(v)) if *v != 0);
    let depth = match args.get(6) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let treat_eof_as_epsilon = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);
    antlr_parser_closure_impl(
        ctx,
        this,
        config,
        configs,
        closure_busy,
        collect_predicates,
        full_ctx,
        depth,
        treat_eof_as_epsilon,
    )
}

fn native_antlr_parser_public_closure(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let configs = obj_arg(args, 2)?;
    let closure_busy = obj_arg(args, 3)?;
    let collect_predicates = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    let full_ctx = matches!(args.get(5), Some(Value::Int(v)) if *v != 0);
    let treat_eof_as_epsilon = matches!(args.get(6), Some(Value::Int(v)) if *v != 0);
    antlr_parser_closure_checking_stop_state_impl(
        ctx,
        this,
        config,
        configs,
        closure_busy,
        collect_predicates,
        full_ctx,
        0,
        treat_eof_as_epsilon,
    )
}

fn antlr_new_atn_config_set(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    full_ctx: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized(names.atn_config_set, "(Z)V", &[antlr_bool(full_ctx)]) {
        Ok(Some(Value::Object(Some(set)))) => Ok(set),
        _ => {
            let mut scope = NativeHandleScope::new(ctx);
            let class_id = scope.ensure_class_initialized(names.atn_config_set)?;
            let field_count = scope.class_num_total_fields(class_id).max(9);
            let set = scope.alloc_object(class_id, field_count);
            let set_h = scope.root(set);
            let configs = antlr_new_arraylist_with_capacity(&mut *scope, 7)?;
            let set = scope.get(&set_h);
            antlr_set_field_value(&mut *scope, set, "readonly", 0, Value::Int(0));
            antlr_set_field_value(&mut *scope, set, "configLookup", 1, Value::Object(None));
            antlr_set_field_value(&mut *scope, set, "configs", 2, Value::Object(Some(configs)));
            antlr_set_field_value(&mut *scope, set, "uniqueAlt", 3, Value::Int(0));
            antlr_set_field_value(&mut *scope, set, "conflictingAlts", 4, Value::Object(None));
            antlr_set_field_value(&mut *scope, set, "hasSemanticContext", 5, Value::Int(0));
            antlr_set_field_value(&mut *scope, set, "dipsIntoOuterContext", 6, Value::Int(0));
            antlr_set_field_value(&mut *scope, set, "fullCtx", 7, antlr_bool(full_ctx));
            antlr_set_field_value(&mut *scope, set, "cachedHashCode", 8, Value::Int(-1));
            Ok(scope.get(&set_h))
        }
    }
}

fn antlr_atn_config_set_size(ctx: &mut dyn NativeContext, set: ObjectRef) -> usize {
    antlr_ref_field(ctx, set, "configs", 2)
        .map(|configs| antlr_arraylist_size(ctx, configs))
        .unwrap_or(0)
}

fn antlr_atn_config_set_is_empty(ctx: &mut dyn NativeContext, set: ObjectRef) -> bool {
    antlr_atn_config_set_size(ctx, set) == 0
}

fn antlr_atn_config_set_full_ctx(ctx: &mut dyn NativeContext, set: ObjectRef) -> bool {
    antlr_int_field(ctx, set, "fullCtx", 7) != 0
}

/// Config `index` read fresh out of the `configs` list of a *rooted* set.
///
/// This is the shape the moving-young root fix is built on: re-reading by
/// index stays correct when the loop body allocates, and costs O(1) GC roots.
/// Snapshotting the set into a `Vec<ObjectRef>` is stale after the first
/// allocation, and rooting the whole snapshot to compensate makes every config
/// of every set a root for the entire closure walk — on exactly the
/// allocation-heavy path that provokes the defect.
fn antlr_config_set_at(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
    index: usize,
) -> Option<ObjectRef> {
    let configs = antlr_ref_field(ctx, set, "configs", 2)?;
    let data = antlr_arraylist_data(ctx, configs)?;
    if index >= antlr_arraylist_size(ctx, configs) || index >= ctx.array_length(data) {
        return None;
    }
    match ctx.get_array_element(data, index) {
        Value::Object(obj) => obj,
        _ => None,
    }
}

fn antlr_atn_config_set_unique_alt(ctx: &mut dyn NativeContext, set: ObjectRef) -> i32 {
    let mut unique = 0;
    for index in 0..antlr_atn_config_set_size(ctx, set) {
        let Some(config) = antlr_config_set_at(ctx, set, index) else {
            continue;
        };
        let alt = antlr_atn_config_alt(ctx, config);
        if unique == 0 {
            unique = alt;
        } else if unique != alt {
            return 0;
        }
    }
    unique
}

fn antlr_atn_config_set_has_rule_stop_state(ctx: &mut dyn NativeContext, set: ObjectRef) -> bool {
    for index in 0..antlr_atn_config_set_size(ctx, set) {
        let Some(config) = antlr_config_set_at(ctx, set, index) else {
            continue;
        };
        let is_stop = antlr_ref_field(ctx, config, "state", 0)
            .map(|state| antlr_state_is_rule_stop(ctx, state))
            .unwrap_or(false);
        if is_stop {
            return true;
        }
    }
    false
}

fn antlr_atn_config_set_all_rule_stop_states(ctx: &mut dyn NativeContext, set: ObjectRef) -> bool {
    let size = antlr_atn_config_set_size(ctx, set);
    if size == 0 {
        return false;
    }
    for index in 0..size {
        let Some(config) = antlr_config_set_at(ctx, set, index) else {
            return false;
        };
        let is_stop = antlr_ref_field(ctx, config, "state", 0)
            .map(|state| antlr_state_is_rule_stop(ctx, state))
            .unwrap_or(false);
        if !is_stop {
            return false;
        }
    }
    true
}

fn antlr_parser_atn(ctx: &mut dyn NativeContext, simulator: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, simulator, "atn", 2)
}

fn antlr_parser_merge_cache_or_create(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(cache) = antlr_parser_merge_cache(ctx, simulator) {
        return Ok(cache);
    }
    let names = antlr_names_for_object(ctx, simulator);
    let mut scope = NativeHandleScope::new(ctx);
    let simulator_h = scope.root(simulator);
    let cache = antlr_object_from_result(scope.new_object(names.double_key_map), "DoubleKeyMap")?;
    let cache_h = scope.root(cache);
    let init_args = [Value::Object(Some(scope.get(&cache_h)))];
    native_antlr_double_key_map_init(&mut *scope, &init_args)?;
    let simulator = scope.get(&simulator_h);
    let cache = scope.get(&cache_h);
    antlr_set_field_value(
        &mut *scope,
        simulator,
        "mergeCache",
        5,
        Value::Object(Some(cache)),
    );
    Ok(scope.get(&cache_h))
}

fn antlr_parser_reachable_target(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    transition: ObjectRef,
    symbol: i32,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let simulator_h = scope.root(simulator);
    let transition_h = scope.root(transition);
    let simulator = scope.get(&simulator_h);
    let max_token_type = antlr_parser_atn(&mut *scope, simulator)
        .map(|atn| antlr_int_field(&mut *scope, atn, "maxTokenType", 6))
        .unwrap_or(i32::MAX);
    let match_args = [
        Value::Object(Some(scope.get(&transition_h))),
        Value::Int(symbol),
        Value::Int(0),
        Value::Int(max_token_type),
    ];
    let matches = match native_antlr_transition_matches(&mut *scope, &match_args)? {
        Some(Value::Int(v)) => v != 0,
        _ => false,
    };
    let target = if matches {
        let transition = scope.get(&transition_h);
        antlr_transition_target(&mut *scope, transition)
    } else {
        None
    };
    Ok(target)
}

fn antlr_atn_next_tokens_contains_epsilon(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    state: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let Some(atn) = antlr_parser_atn(ctx, simulator) else {
        return Ok(false);
    };
    let names = antlr_names_for_object(ctx, state);
    let prefix = if names.pc == GROOVY_ANTLR_PC {
        "groovyjarjarantlr4/v4/runtime"
    } else {
        "org/antlr/v4/runtime"
    };
    let descriptor = format!("(L{prefix}/atn/ATNState;)L{prefix}/misc/IntervalSet;");
    let mut scope = NativeHandleScope::new(ctx);
    let atn_h = scope.root(atn);
    let state_h = scope.root(state);
    let atn = scope.get(&atn_h);
    let next_tokens_args = [Value::Object(Some(scope.get(&state_h)))];
    match scope.invoke_virtual(atn, "nextTokens", &descriptor, &next_tokens_args)? {
        Some(Value::Object(Some(intervals))) => {
            antlr_interval_set_contains(&mut *scope, intervals, -2)
        }
        _ => Ok(false),
    }
}

fn antlr_parser_rule_stop_state_for(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    state: ObjectRef,
) -> Option<ObjectRef> {
    let atn = antlr_parser_atn(ctx, simulator)?;
    let stops = antlr_ref_field(ctx, atn, "ruleToStopState", 3)?;
    let rule_index = antlr_atn_state_rule_index(ctx, state);
    if rule_index < 0 || rule_index as usize >= ctx.array_length(stops) {
        return None;
    }
    match ctx.get_array_element(stops, rule_index as usize) {
        Value::Object(obj) => obj,
        _ => None,
    }
}

fn antlr_parser_remove_all_configs_not_in_rule_stop_state(
    ctx: &mut dyn NativeContext,
    simulator: ObjectRef,
    configs: ObjectRef,
    look_to_end_of_rule: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let simulator_h = scope.root(simulator);
    let configs_h = scope.root(configs);

    let configs = scope.get(&configs_h);
    if antlr_atn_config_set_all_rule_stop_states(&mut *scope, configs) {
        return Ok(scope.get(&configs_h));
    }
    let names = antlr_names_for_object(&mut *scope, configs);
    let full_ctx = antlr_atn_config_set_full_ctx(&mut *scope, configs);
    let result = antlr_new_atn_config_set(&mut *scope, names, full_ctx)?;
    let result_h = scope.root(result);

    // Walk the source set BY INDEX out of its rooted handle. The loop body
    // allocates (ATNConfig construction, config-set add), so a `Vec<ObjectRef>`
    // snapshot would be stale, and pinning every element would make the whole
    // set a root for the duration.
    let configs = scope.get(&configs_h);
    let size = antlr_atn_config_set_size(&mut *scope, configs);
    for index in 0..size {
        let mut scope = NativeHandleScope::new(&mut *scope);
        let configs = scope.get(&configs_h);
        let Some(config) = antlr_config_set_at(&mut *scope, configs, index) else {
            continue;
        };
        let config_h = scope.root(config);
        let state = antlr_ref_field(&mut *scope, config, "state", 0);
        if state
            .map(|state| antlr_state_is_rule_stop(&mut *scope, state))
            .unwrap_or(false)
        {
            let result = scope.get(&result_h);
            let simulator = scope.get(&simulator_h);
            let merge_cache = antlr_parser_merge_cache(&mut *scope, simulator);
            let config = scope.get(&config_h);
            antlr_atn_config_set_add_impl(&mut *scope, result, config, merge_cache)?;
        } else if look_to_end_of_rule {
            if let Some(state) = state {
                let epsilon_args = [Value::Object(Some(state))];
                let epsilon_only = match native_antlr_atn_state_only_has_epsilon_transitions(
                    &mut *scope,
                    &epsilon_args,
                )? {
                    Some(Value::Int(v)) => v != 0,
                    _ => false,
                };
                let config = scope.get(&config_h);
                let state = antlr_ref_field(&mut *scope, config, "state", 0);
                let simulator = scope.get(&simulator_h);
                if epsilon_only
                    && state
                        .map(|state| {
                            antlr_atn_next_tokens_contains_epsilon(&mut *scope, simulator, state)
                        })
                        .transpose()?
                        .unwrap_or(false)
                {
                    let config = scope.get(&config_h);
                    let state = antlr_ref_field(&mut *scope, config, "state", 0);
                    let simulator = scope.get(&simulator_h);
                    if let Some(stop_state) = state.and_then(|state| {
                        antlr_parser_rule_stop_state_for(&mut *scope, simulator, state)
                    }) {
                        let ctor_args = [
                            Value::Object(Some(scope.get(&config_h))),
                            Value::Object(Some(stop_state)),
                        ];
                        let next_config = antlr_alloc_atn_config(&mut *scope, names, &ctor_args)?;
                        let next_config_h = scope.root(next_config);
                        let result = scope.get(&result_h);
                        let simulator = scope.get(&simulator_h);
                        let merge_cache = antlr_parser_merge_cache(&mut *scope, simulator);
                        let next_config = scope.get(&next_config_h);
                        antlr_atn_config_set_add_impl(
                            &mut *scope,
                            result,
                            next_config,
                            merge_cache,
                        )?;
                    }
                }
            }
        }
    }

    Ok(scope.get(&result_h))
}

fn native_antlr_parser_compute_reach_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let closure_set = obj_arg(args, 1)?;
    let token = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let full_ctx = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
    let names = antlr_names_for_object(ctx, this);

    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let closure_h = scope.root(closure_set);
    let this = scope.get(&this_h);
    let _merge_cache = antlr_parser_merge_cache_or_create(&mut *scope, this)?;
    let intermediate = antlr_new_atn_config_set(&mut *scope, names, full_ctx)?;
    let intermediate_h = scope.root(intermediate);
    // Only the rule-stop configs skipped by the first pass are retained across
    // the whole method, so only they get a handle; the rest of the closure set
    // is re-read by index.
    let mut skipped_stop_states: Vec<NativeHandle> = Vec::new();

    let closure_set = scope.get(&closure_h);
    let closure_size = antlr_atn_config_set_size(&mut *scope, closure_set);
    for index in 0..closure_size {
        let closure_set = scope.get(&closure_h);
        let Some(config) = antlr_config_set_at(&mut *scope, closure_set, index) else {
            continue;
        };
        let Some(state) = antlr_ref_field(&mut *scope, config, "state", 0) else {
            continue;
        };

        if antlr_state_is_rule_stop(&mut *scope, state) {
            if full_ctx || token == -1 {
                // Only the skipped stop-state configs outlive this loop, so
                // only they take a root in the enclosing scope.
                skipped_stop_states.push(scope.root(config));
            }
            continue;
        }

        // Everything below lives for one closure-set entry; nest a scope so
        // the per-entry roots are released as the walk advances instead of
        // accumulating one per config.
        let mut scope = NativeHandleScope::new(&mut *scope);
        let config_h = scope.root(config);
        let count_args = [Value::Object(Some(state))];
        let transition_count =
            match native_antlr_atn_state_get_number_of_transitions(&mut *scope, &count_args)? {
                Some(Value::Int(v)) if v > 0 => v as usize,
                _ => 0,
            };
        for transition_index in 0..transition_count {
            let mut scope = NativeHandleScope::new(&mut *scope);
            // Matching a previous edge can allocate.  Reacquire the state
            // from the rooted config before indexing `transitions` again.
            let config = scope.get(&config_h);
            let Some(state) = antlr_ref_field(&mut *scope, config, "state", 0) else {
                continue;
            };
            let Some(transition) =
                antlr_atn_state_transition_ref(&mut *scope, state, transition_index)?
            else {
                continue;
            };
            let this = scope.get(&this_h);
            let Some(target) = antlr_parser_reachable_target(&mut *scope, this, transition, token)?
            else {
                continue;
            };
            // `reachable_target` can invoke transition bytecode and collect.
            // Refresh the source config before passing it to the allocating
            // ATNConfig constructor.
            let ctor_args = [
                Value::Object(Some(scope.get(&config_h))),
                Value::Object(Some(target)),
            ];
            let next_config = antlr_alloc_atn_config(&mut *scope, names, &ctor_args)?;
            let next_config_h = scope.root(next_config);
            let intermediate = scope.get(&intermediate_h);
            let this = scope.get(&this_h);
            let next_config = scope.get(&next_config_h);
            antlr_parser_add_config(&mut *scope, this, intermediate, next_config)?;
        }
    }

    let intermediate = scope.get(&intermediate_h);
    let mut reach_h = None;
    if skipped_stop_states.is_empty() && token != -1 {
        if antlr_atn_config_set_size(&mut *scope, intermediate) == 1
            || antlr_atn_config_set_unique_alt(&mut *scope, intermediate) != 0
        {
            let intermediate = scope.get(&intermediate_h);
            reach_h = Some(scope.root(intermediate));
        }
    }

    if reach_h.is_none() {
        let reach_set = antlr_new_atn_config_set(&mut *scope, names, full_ctx)?;
        let reach_set_h = scope.root(reach_set);
        let closure_busy =
            cratonvm_native_collections::make_hashset_with_elements(&mut *scope, &[])?;
        let closure_busy_h = scope.root(closure_busy);
        let treat_eof_as_epsilon = token == -1;
        let intermediate = scope.get(&intermediate_h);
        let intermediate_size = antlr_atn_config_set_size(&mut *scope, intermediate);
        for index in 0..intermediate_size {
            let mut scope = NativeHandleScope::new(&mut *scope);
            let intermediate = scope.get(&intermediate_h);
            let Some(config) = antlr_config_set_at(&mut *scope, intermediate, index) else {
                continue;
            };
            let reach_set = scope.get(&reach_set_h);
            let closure_busy = scope.get(&closure_busy_h);
            let this = scope.get(&this_h);
            antlr_parser_closure_checking_stop_state_impl(
                &mut *scope,
                this,
                config,
                reach_set,
                closure_busy,
                false,
                full_ctx,
                0,
                treat_eof_as_epsilon,
            )?;
        }
        reach_h = Some(reach_set_h);
    }

    let mut reach_h = reach_h.expect("reach set should be initialized");
    if token == -1 {
        let reach = scope.get(&reach_h);
        let intermediate = scope.get(&intermediate_h);
        let look_to_end_of_rule = reach == intermediate;
        let this = scope.get(&this_h);
        let reach = antlr_parser_remove_all_configs_not_in_rule_stop_state(
            &mut *scope,
            this,
            reach,
            look_to_end_of_rule,
        )?;
        reach_h = scope.root(reach);
    }

    let reach = scope.get(&reach_h);
    if !skipped_stop_states.is_empty()
        && (!full_ctx || !antlr_atn_config_set_has_rule_stop_state(&mut *scope, reach))
    {
        for handle in &skipped_stop_states {
            let config = scope.get(handle);
            let reach = scope.get(&reach_h);
            let this = scope.get(&this_h);
            antlr_parser_add_config(&mut *scope, this, reach, config)?;
        }
    }

    let reach = scope.get(&reach_h);
    let result = if antlr_atn_config_set_is_empty(&mut *scope, reach) {
        Value::Object(None)
    } else {
        Value::Object(Some(scope.get(&reach_h)))
    };
    Ok(Some(result))
}

fn antlr_atn_config_state_number(ctx: &mut dyn NativeContext, config: ObjectRef) -> i32 {
    let Some(state) = antlr_ref_field(ctx, config, "state", 0) else {
        return -1;
    };
    antlr_int_field(ctx, state, "stateNumber", 1)
}

#[inline]
fn antlr_atn_config_alt(ctx: &mut dyn NativeContext, config: ObjectRef) -> i32 {
    antlr_int_field(ctx, config, "alt", 1)
}

#[inline]
fn antlr_atn_config_context(ctx: &mut dyn NativeContext, config: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, config, "context", 2)
}

#[inline]
fn antlr_atn_config_set_context(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
    context: Option<ObjectRef>,
) {
    antlr_set_field_value(ctx, config, "context", 2, Value::Object(context));
}

#[inline]
fn antlr_atn_config_reaches(ctx: &mut dyn NativeContext, config: ObjectRef) -> i32 {
    antlr_int_field(ctx, config, "reachesIntoOuterContext", 3)
}

#[inline]
fn antlr_atn_config_set_reaches(ctx: &mut dyn NativeContext, config: ObjectRef, value: i32) {
    antlr_set_field_value(ctx, config, "reachesIntoOuterContext", 3, Value::Int(value));
}

#[inline]
fn antlr_atn_config_outer_context_depth(ctx: &mut dyn NativeContext, config: ObjectRef) -> i32 {
    antlr_atn_config_reaches(ctx, config) & !ANTLR_SUPPRESS_PRECEDENCE_FILTER
}

#[inline]
fn antlr_atn_config_precedence_suppressed(ctx: &mut dyn NativeContext, config: ObjectRef) -> bool {
    (antlr_atn_config_reaches(ctx, config) & ANTLR_SUPPRESS_PRECEDENCE_FILTER) != 0
}

#[inline]
fn antlr_atn_config_set_precedence_suppressed(ctx: &mut dyn NativeContext, config: ObjectRef) {
    let reaches = antlr_atn_config_reaches(ctx, config) | ANTLR_SUPPRESS_PRECEDENCE_FILTER;
    antlr_atn_config_set_reaches(ctx, config, reaches);
}

#[inline]
fn antlr_atn_config_semantic_context(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
) -> Option<ObjectRef> {
    antlr_ref_field(ctx, config, "semanticContext", 4)
}

fn antlr_config_key_hash(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let mut hash = 7i32;
    hash = hash
        .wrapping_mul(31)
        .wrapping_add(antlr_atn_config_state_number(ctx, config));
    hash = hash
        .wrapping_mul(31)
        .wrapping_add(antlr_atn_config_alt(ctx, config));
    let semantic_context = antlr_atn_config_semantic_context(ctx, config);
    hash = hash
        .wrapping_mul(31)
        .wrapping_add(antlr_semantic_context_hash(ctx, semantic_context)?);
    Ok(hash)
}

fn antlr_config_key_equal(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let a_semantic = antlr_atn_config_semantic_context(ctx, a);
    let b_semantic = antlr_atn_config_semantic_context(ctx, b);
    // No handle scope needed: the only collection point is the final
    // `antlr_semantic_contexts_equal`, and `&&` short-circuits left to right,
    // so nothing here is read after it.
    Ok(
        antlr_atn_config_state_number(ctx, a) == antlr_atn_config_state_number(ctx, b)
            && antlr_atn_config_alt(ctx, a) == antlr_atn_config_alt(ctx, b)
            && antlr_semantic_contexts_equal(ctx, a_semantic, b_semantic)?,
    )
}

fn antlr_atn_configs_equal(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    // `antlr_semantic_contexts_equal` reaches `Object.equals` bytecode and can
    // collect, and BOTH configs are read again afterwards for the
    // precedence-suppressed comparison — so both need handles.
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let contexts_equal = match (
        antlr_atn_config_context(&mut *scope, a),
        antlr_atn_config_context(&mut *scope, b),
    ) {
        (Some(ac), Some(bc)) => antlr_prediction_contexts_equal(&mut *scope, ac, bc),
        (None, None) => true,
        _ => false,
    };
    let a_semantic = antlr_atn_config_semantic_context(&mut *scope, a);
    let b_semantic = antlr_atn_config_semantic_context(&mut *scope, b);
    if antlr_atn_config_state_number(&mut *scope, a)
        != antlr_atn_config_state_number(&mut *scope, b)
        || antlr_atn_config_alt(&mut *scope, a) != antlr_atn_config_alt(&mut *scope, b)
        || !contexts_equal
    {
        return Ok(false);
    }
    if !antlr_semantic_contexts_equal(&mut *scope, a_semantic, b_semantic)? {
        return Ok(false);
    }
    let a = scope.get(&a_h);
    let b = scope.get(&b_h);
    Ok(antlr_atn_config_precedence_suppressed(&mut *scope, a)
        == antlr_atn_config_precedence_suppressed(&mut *scope, b))
}

fn antlr_config_lookup_buckets(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
) -> Option<ObjectRef> {
    antlr_ref_field(ctx, lookup, "buckets", 1)
}

fn antlr_config_lookup_set_buckets(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
    buckets: ObjectRef,
) {
    antlr_set_field_value(ctx, lookup, "buckets", 1, Value::Object(Some(buckets)));
}

fn antlr_config_lookup_n(ctx: &mut dyn NativeContext, lookup: ObjectRef) -> i32 {
    antlr_int_field(ctx, lookup, "n", 2)
}

fn antlr_config_lookup_set_n(ctx: &mut dyn NativeContext, lookup: ObjectRef, n: i32) {
    antlr_set_field_value(ctx, lookup, "n", 2, Value::Int(n));
}

fn antlr_config_lookup_initial_bucket_capacity(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
) -> usize {
    match antlr_int_field(ctx, lookup, "initialBucketCapacity", 6) {
        v if v > 0 => v as usize,
        _ => 2,
    }
}

fn antlr_config_lookup_create_buckets(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
    config: ObjectRef,
) -> (ObjectRef, ObjectRef, ObjectRef) {
    let mut scope = NativeHandleScope::new(ctx);
    let lookup_h = scope.root(lookup);
    let config_h = scope.root(config);
    let buckets = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
    let lookup = scope.get(&lookup_h);
    let config = scope.get(&config_h);
    antlr_config_lookup_set_buckets(&mut *scope, lookup, buckets);
    antlr_set_field_value(&mut *scope, lookup, "threshold", 4, Value::Int(12));
    (lookup, config, buckets)
}

fn antlr_config_lookup_new_bucket(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
    buckets: ObjectRef,
    config: ObjectRef,
    bucket_index: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let names = antlr_names_for_object(ctx, config);
    let config_class = ctx
        .class_id_by_name(names.atn_config)
        .unwrap_or_else(|| ctx.class_id_of_object(config));
    let capacity = antlr_config_lookup_initial_bucket_capacity(ctx, lookup);
    let mut scope = NativeHandleScope::new(ctx);
    let lookup_h = scope.root(lookup);
    let buckets_h = scope.root(buckets);
    let config_h = scope.root(config);
    let bucket = scope.new_ref_array(config_class, capacity);
    let lookup = scope.get(&lookup_h);
    let buckets = scope.get(&buckets_h);
    let config = scope.get(&config_h);
    scope.set_array_element(bucket, 0, Value::Object(Some(config)));
    scope.set_array_element(buckets, bucket_index, Value::Object(Some(bucket)));
    let next_n = antlr_config_lookup_n(&mut *scope, lookup).saturating_add(1);
    antlr_config_lookup_set_n(&mut *scope, lookup, next_n);
    Ok(scope.get(&config_h))
}

fn antlr_config_lookup_grow_bucket(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
    buckets: ObjectRef,
    bucket: ObjectRef,
    config: ObjectRef,
    bucket_index: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let names = antlr_names_for_object(ctx, config);
    let config_class = ctx
        .class_id_by_name(names.atn_config)
        .unwrap_or_else(|| ctx.class_id_of_object(config));
    let old_len = ctx.array_length(bucket);
    let mut scope = NativeHandleScope::new(ctx);
    let lookup_h = scope.root(lookup);
    let buckets_h = scope.root(buckets);
    let bucket_h = scope.root(bucket);
    let config_h = scope.root(config);
    let new_bucket = scope.new_ref_array(config_class, std::cmp::max(1, old_len * 2));
    let lookup = scope.get(&lookup_h);
    let buckets = scope.get(&buckets_h);
    let bucket = scope.get(&bucket_h);
    let config = scope.get(&config_h);
    for i in 0..old_len {
        let value = scope.get_array_element(bucket, i);
        scope.set_array_element(new_bucket, i, value);
    }
    scope.set_array_element(new_bucket, old_len, Value::Object(Some(config)));
    scope.set_array_element(buckets, bucket_index, Value::Object(Some(new_bucket)));
    let next_n = antlr_config_lookup_n(&mut *scope, lookup).saturating_add(1);
    antlr_config_lookup_set_n(&mut *scope, lookup, next_n);
    Ok(scope.get(&config_h))
}

fn antlr_config_lookup_get_or_add(
    ctx: &mut dyn NativeContext,
    lookup: ObjectRef,
    config: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let lookup_h = scope.root(lookup);
    let config_h = scope.root(config);

    let lookup = scope.get(&lookup_h);
    let config = scope.get(&config_h);
    let buckets = match antlr_config_lookup_buckets(&mut *scope, lookup) {
        Some(buckets) if scope.array_length(buckets) != 0 => buckets,
        _ => antlr_config_lookup_create_buckets(&mut *scope, lookup, config).2,
    };
    let buckets_h = scope.root(buckets);

    let config = scope.get(&config_h);
    let hash = antlr_config_key_hash(&mut *scope, config)?;
    let lookup = scope.get(&lookup_h);
    let config = scope.get(&config_h);
    let buckets = scope.get(&buckets_h);
    let bucket_index = (hash as u32 as usize) & (scope.array_length(buckets) - 1);
    let bucket = match scope.get_array_element(buckets, bucket_index) {
        Value::Object(Some(bucket)) => bucket,
        _ => {
            return antlr_config_lookup_new_bucket(
                &mut *scope,
                lookup,
                buckets,
                config,
                bucket_index,
            )
        }
    };
    let bucket_h = scope.root(bucket);

    let len = scope.array_length(bucket);
    for i in 0..len {
        let lookup = scope.get(&lookup_h);
        let config = scope.get(&config_h);
        let bucket = scope.get(&bucket_h);
        match scope.get_array_element(bucket, i) {
            Value::Object(Some(existing)) => {
                let mut scope = NativeHandleScope::new(&mut *scope);
                let existing_h = scope.root(existing);
                let equal = antlr_config_key_equal(&mut *scope, existing, config)?;
                if equal {
                    return Ok(scope.get(&existing_h));
                }
            }
            _ => {
                scope.set_array_element(bucket, i, Value::Object(Some(config)));
                let next_n = antlr_config_lookup_n(&mut *scope, lookup).saturating_add(1);
                antlr_config_lookup_set_n(&mut *scope, lookup, next_n);
                return Ok(config);
            }
        }
    }

    let lookup = scope.get(&lookup_h);
    let config = scope.get(&config_h);
    let buckets = scope.get(&buckets_h);
    let bucket = scope.get(&bucket_h);
    antlr_config_lookup_grow_bucket(&mut *scope, lookup, buckets, bucket, config, bucket_index)
}

fn antlr_config_list_find(
    ctx: &mut dyn NativeContext,
    configs: ObjectRef,
    config: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // `antlr_config_key_equal` compares semantic contexts and can collect, so
    // the backing array, the probe config and the candidate all need handles.
    let mut scope = NativeHandleScope::new(ctx);
    let configs_h = scope.root(configs);
    let config_h = scope.root(config);
    let Some(data) = antlr_arraylist_data(&mut *scope, configs) else {
        return Ok(None);
    };
    let data_h = scope.root(data);
    let configs = scope.get(&configs_h);
    let size = std::cmp::min(
        antlr_arraylist_size(&mut *scope, configs),
        scope.array_length(data),
    );
    for i in 0..size {
        let data = scope.get(&data_h);
        if let Value::Object(Some(existing)) = scope.get_array_element(data, i) {
            let existing_h = scope.root(existing);
            let config = scope.get(&config_h);
            if antlr_config_key_equal(&mut *scope, existing, config)? {
                return Ok(Some(scope.get(&existing_h)));
            }
        }
    }
    Ok(None)
}

fn antlr_atn_config_set_add_impl(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    config: ObjectRef,
    merge_cache: Option<ObjectRef>,
) -> MethodCallResult {
    // Config lookup, context equality/merge, and ArrayList growth can all
    // allocate, so every object used after one of those sub-calls is re-read
    // through its handle.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let config_h = scope.root(config);
    let merge_cache_h = merge_cache.map(|cache| scope.root(cache));

    let this = scope.get(&this_h);
    if antlr_int_field(&mut *scope, this, "readonly", 0) != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "This set is readonly".to_string(),
        }
        .into());
    }

    let config = scope.get(&config_h);
    let semctx = antlr_atn_config_semantic_context(&mut *scope, config);
    if !antlr_semantic_context_is_empty(&mut *scope, semctx) {
        let this = scope.get(&this_h);
        antlr_set_field_value(&mut *scope, this, "hasSemanticContext", 5, Value::Int(1));
    }
    let config = scope.get(&config_h);
    if antlr_atn_config_outer_context_depth(&mut *scope, config) > 0 {
        let this = scope.get(&this_h);
        antlr_set_field_value(&mut *scope, this, "dipsIntoOuterContext", 6, Value::Int(1));
    }

    let this = scope.get(&this_h);
    let config = scope.get(&config_h);
    // `Some(_)` = an existing entry; `None` = "use `config` itself", which has
    // to be re-read afterwards because the lookup/find above can collect.
    let found = if let Some(lookup) = antlr_ref_field(&mut *scope, this, "configLookup", 1) {
        Some(antlr_config_lookup_get_or_add(&mut *scope, lookup, config)?)
    } else if let Some(configs) = antlr_ref_field(&mut *scope, this, "configs", 2) {
        antlr_config_list_find(&mut *scope, configs, config)?
    } else {
        None
    };
    let existing_h = match found {
        Some(existing) => scope.root(existing),
        None => {
            let config = scope.get(&config_h);
            scope.root(config)
        }
    };

    let this = scope.get(&this_h);
    let config = scope.get(&config_h);
    let existing = scope.get(&existing_h);
    if existing == config {
        antlr_set_field_value(&mut *scope, this, "cachedHashCode", 8, Value::Int(-1));
        let this = scope.get(&this_h);
        if let Some(configs) = antlr_ref_field(&mut *scope, this, "configs", 2) {
            let config = scope.get(&config_h);
            antlr_arraylist_append(&mut *scope, configs, Value::Object(Some(config)))?;
        }
        return Ok(Some(Value::Int(1)));
    }

    let this = scope.get(&this_h);
    let root_is_wildcard = antlr_int_field(&mut *scope, this, "fullCtx", 7) == 0;
    let existing = scope.get(&existing_h);
    let config = scope.get(&config_h);
    let existing_context = antlr_atn_config_context(&mut *scope, existing);
    let config_context = antlr_atn_config_context(&mut *scope, config);
    let merge_cache = merge_cache_h.as_ref().map(|handle| scope.get(handle));
    let merged = match (existing_context, config_context) {
        (Some(a), Some(b)) => Some(antlr_merge_contexts(
            &mut *scope,
            a,
            b,
            root_is_wildcard,
            merge_cache,
        )?),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    // `antlr_merge_contexts` allocates the merged context; root it before the
    // field writes below re-read anything else.
    let merged_h = merged.map(|merged| scope.root(merged));

    let existing = scope.get(&existing_h);
    let config = scope.get(&config_h);
    let reaches = std::cmp::max(
        antlr_atn_config_reaches(&mut *scope, existing),
        antlr_atn_config_reaches(&mut *scope, config),
    );
    antlr_atn_config_set_reaches(&mut *scope, existing, reaches);
    let existing = scope.get(&existing_h);
    let config = scope.get(&config_h);
    if antlr_atn_config_precedence_suppressed(&mut *scope, config) {
        antlr_atn_config_set_precedence_suppressed(&mut *scope, existing);
    }
    let existing = scope.get(&existing_h);
    let merged = merged_h.as_ref().map(|handle| scope.get(handle));
    antlr_atn_config_set_context(&mut *scope, existing, merged);
    Ok(Some(Value::Int(1)))
}

fn native_antlr_atn_config_set_add(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = obj_arg(args, 1)?;
    let merge_cache = match args.get(2) {
        Some(Value::Object(cache)) => *cache,
        _ => None,
    };
    antlr_atn_config_set_add_impl(ctx, this, config, merge_cache)
}

fn antlr_object_option_arg(
    args: &[Value],
    idx: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Object(obj)) => Ok(*obj),
        _ => Err(RuntimeError::IllegalArgumentException {
            message: format!("expected object argument at index {idx}"),
        }
        .into()),
    }
}

fn antlr_int_arg(args: &[Value], idx: usize) -> Result<i32, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) => Ok(*v),
        _ => Err(RuntimeError::IllegalArgumentException {
            message: format!("expected int argument at index {idx}"),
        }
        .into()),
    }
}

fn antlr_semantic_empty_instance(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if names.pc == GROOVY_ANTLR_PC {
        let class_id = ctx.ensure_class_initialized(GROOVY_ANTLR_SEMANTIC_CONTEXT)?;
        return Ok(ctx.static_field_index_by_name(class_id, "NONE").and_then(
            |field_idx| match ctx.get_static_field(class_id, field_idx) {
                Value::Object(instance) => instance,
                _ => None,
            },
        ));
    }
    let class_id = ctx.ensure_class_initialized(names.semantic_empty)?;
    Ok(ctx
        .static_field_index_by_name(class_id, "Instance")
        .and_then(
            |field_idx| match ctx.get_static_field(class_id, field_idx) {
                Value::Object(instance) => instance,
                _ => None,
            },
        ))
}

fn antlr_set_atn_config_fields(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
    state: Option<ObjectRef>,
    alt: i32,
    context: Option<ObjectRef>,
    semantic_context: Option<ObjectRef>,
    reaches: i32,
) {
    antlr_set_field_value(ctx, config, "state", 0, Value::Object(state));
    antlr_set_field_value(ctx, config, "alt", 1, Value::Int(alt));
    antlr_set_field_value(ctx, config, "context", 2, Value::Object(context));
    antlr_set_field_value(
        ctx,
        config,
        "reachesIntoOuterContext",
        3,
        Value::Int(reaches),
    );
    antlr_set_field_value(
        ctx,
        config,
        "semanticContext",
        4,
        Value::Object(semantic_context),
    );
}

fn antlr_atn_config_snapshot(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> AntlrAtnConfigSnapshot {
    AntlrAtnConfigSnapshot {
        state: antlr_ref_field(ctx, source, "state", 0),
        alt: antlr_atn_config_alt(ctx, source),
        context: antlr_atn_config_context(ctx, source),
        semantic_context: antlr_atn_config_semantic_context(ctx, source),
        reaches: antlr_atn_config_reaches(ctx, source),
    }
}

fn antlr_set_atn_config_snapshot(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
    fields: AntlrAtnConfigSnapshot,
) {
    antlr_set_atn_config_fields(
        ctx,
        config,
        fields.state,
        fields.alt,
        fields.context,
        fields.semantic_context,
        fields.reaches,
    );
}

fn antlr_object_name(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default()
}

fn antlr_is_semantic_context_arg(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    antlr_object_name(ctx, obj).contains("/atn/SemanticContext")
}

fn antlr_is_prediction_context_arg(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    antlr_object_name(ctx, obj).contains("/atn/PredictionContext")
}

fn native_antlr_atn_config_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let names = antlr_names_for_object(ctx, this);

    match args.len() {
        // ATNConfig(ATNConfig)
        2 => {
            let source = obj_arg(args, 1)?;
            let fields = antlr_atn_config_snapshot(ctx, source);
            antlr_set_atn_config_snapshot(ctx, this, fields);
        }
        // ATNConfig(ATNConfig, ATNState) or ATNConfig(ATNConfig, SemanticContext)
        3 => {
            let source = obj_arg(args, 1)?;
            let arg2 = antlr_object_option_arg(args, 2)?;
            let source_fields = antlr_atn_config_snapshot(ctx, source);
            if arg2
                .map(|obj| antlr_is_semantic_context_arg(ctx, obj))
                .unwrap_or(false)
            {
                antlr_set_atn_config_fields(
                    ctx,
                    this,
                    source_fields.state,
                    source_fields.alt,
                    source_fields.context,
                    arg2,
                    source_fields.reaches,
                );
            } else {
                antlr_set_atn_config_fields(
                    ctx,
                    this,
                    arg2,
                    source_fields.alt,
                    source_fields.context,
                    source_fields.semantic_context,
                    source_fields.reaches,
                );
            }
        }
        4 => {
            if matches!(args.get(2), Some(Value::Int(_))) {
                // ATNConfig(ATNState, int, PredictionContext)
                let state = antlr_object_option_arg(args, 1)?;
                let alt = antlr_int_arg(args, 2)?;
                let context = antlr_object_option_arg(args, 3)?;
                // `antlr_semantic_empty_instance` runs SemanticContext$Empty's
                // <clinit> and can collect, so the receiver and both reference
                // arguments have to survive it.
                let mut scope = NativeHandleScope::new(ctx);
                let this_h = scope.root(this);
                let state_h = state.map(|state| scope.root(state));
                let context_h = context.map(|context| scope.root(context));
                let semantic_context = antlr_semantic_empty_instance(&mut *scope, names)?;
                let this = scope.get(&this_h);
                let state = state_h.as_ref().map(|handle| scope.get(handle));
                let context = context_h.as_ref().map(|handle| scope.get(handle));
                antlr_set_atn_config_fields(
                    &mut *scope,
                    this,
                    state,
                    alt,
                    context,
                    semantic_context,
                    0,
                );
            } else {
                // ATNConfig(ATNConfig, ATNState, PredictionContext)
                // or ATNConfig(ATNConfig, ATNState, SemanticContext)
                let source = obj_arg(args, 1)?;
                let source_fields = antlr_atn_config_snapshot(ctx, source);
                let state = antlr_object_option_arg(args, 2)?;
                let arg3 = antlr_object_option_arg(args, 3)?;
                let arg3_is_semantic_context = arg3
                    .map(|obj| antlr_is_semantic_context_arg(ctx, obj))
                    .unwrap_or(false);
                let context = if arg3_is_semantic_context {
                    source_fields.context
                } else {
                    arg3
                };
                let semantic_context = if arg3_is_semantic_context {
                    arg3
                } else {
                    source_fields.semantic_context
                };
                antlr_set_atn_config_fields(
                    ctx,
                    this,
                    state,
                    source_fields.alt,
                    context,
                    semantic_context,
                    source_fields.reaches,
                );
            }
        }
        5 => {
            if matches!(args.get(2), Some(Value::Int(_))) {
                // ATNConfig(ATNState, int, PredictionContext, SemanticContext)
                let state = antlr_object_option_arg(args, 1)?;
                let alt = antlr_int_arg(args, 2)?;
                let context = antlr_object_option_arg(args, 3)?;
                let semantic_context = antlr_object_option_arg(args, 4)?;
                antlr_set_atn_config_fields(ctx, this, state, alt, context, semantic_context, 0);
            } else {
                // ATNConfig(ATNConfig, ATNState, PredictionContext, SemanticContext)
                let source = obj_arg(args, 1)?;
                let source_fields = antlr_atn_config_snapshot(ctx, source);
                let state = antlr_object_option_arg(args, 2)?;
                let context = antlr_object_option_arg(args, 3)?;
                let semantic_context = antlr_object_option_arg(args, 4)?;
                antlr_set_atn_config_fields(
                    ctx,
                    this,
                    state,
                    source_fields.alt,
                    context,
                    semantic_context,
                    source_fields.reaches,
                );
            }
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("unsupported ATNConfig constructor arity {}", args.len()),
            }
            .into())
        }
    }

    Ok(None)
}

fn antlr_decision_state_non_greedy(ctx: &mut dyn NativeContext, state: Option<ObjectRef>) -> bool {
    let Some(state) = state else {
        return false;
    };
    let class_name = antlr_object_name(ctx, state);
    let Some(slot) = ctx.resolve_field_index(&class_name, "nonGreedy") else {
        return false;
    };
    matches!(ctx.get_field(state, slot), Value::Int(v) if v != 0)
}

fn antlr_lexer_atn_config_set_fields(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
    lexer_action_executor: Option<ObjectRef>,
    passed_through_non_greedy_decision: bool,
) {
    antlr_set_field_value(
        ctx,
        config,
        "lexerActionExecutor",
        5,
        Value::Object(lexer_action_executor),
    );
    antlr_set_field_value(
        ctx,
        config,
        "passedThroughNonGreedyDecision",
        6,
        Value::Int(if passed_through_non_greedy_decision {
            1
        } else {
            0
        }),
    );
}

fn antlr_lexer_non_greedy_from_source(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    state: Option<ObjectRef>,
) -> bool {
    antlr_int_field(ctx, source, "passedThroughNonGreedyDecision", 6) != 0
        || antlr_decision_state_non_greedy(ctx, state)
}

fn native_antlr_lexer_atn_config_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let names = antlr_names_for_object(ctx, this);

    match args.len() {
        4 => {
            if matches!(args.get(2), Some(Value::Int(_))) {
                // LexerATNConfig(ATNState, int, PredictionContext)
                let state = antlr_object_option_arg(args, 1)?;
                let alt = antlr_int_arg(args, 2)?;
                let context = antlr_object_option_arg(args, 3)?;
                // `antlr_semantic_empty_instance` runs a <clinit> and can
                // collect; the receiver and both reference arguments have to
                // survive it.
                let mut scope = NativeHandleScope::new(ctx);
                let this_h = scope.root(this);
                let state_h = state.map(|state| scope.root(state));
                let context_h = context.map(|context| scope.root(context));
                let semantic_context = antlr_semantic_empty_instance(&mut *scope, names)?;
                let this = scope.get(&this_h);
                let state = state_h.as_ref().map(|handle| scope.get(handle));
                let context = context_h.as_ref().map(|handle| scope.get(handle));
                antlr_set_atn_config_fields(
                    &mut *scope,
                    this,
                    state,
                    alt,
                    context,
                    semantic_context,
                    0,
                );
                let this = scope.get(&this_h);
                antlr_lexer_atn_config_set_fields(&mut *scope, this, None, false);
            } else {
                // LexerATNConfig(LexerATNConfig, ATNState, LexerActionExecutor)
                // or LexerATNConfig(LexerATNConfig, ATNState, PredictionContext)
                let source = obj_arg(args, 1)?;
                let source_fields = antlr_atn_config_snapshot(ctx, source);
                let source_lexer_action_executor =
                    antlr_ref_field(ctx, source, "lexerActionExecutor", 5);
                let state = antlr_object_option_arg(args, 2)?;
                let arg3 = antlr_object_option_arg(args, 3)?;
                let arg3_is_prediction_context = arg3
                    .map(|obj| antlr_is_prediction_context_arg(ctx, obj))
                    .unwrap_or(false);
                let context = if arg3_is_prediction_context {
                    arg3
                } else {
                    source_fields.context
                };
                let lexer_action_executor = if arg3_is_prediction_context {
                    source_lexer_action_executor
                } else {
                    arg3
                };
                let passed = antlr_lexer_non_greedy_from_source(ctx, source, state);
                antlr_set_atn_config_fields(
                    ctx,
                    this,
                    state,
                    source_fields.alt,
                    context,
                    source_fields.semantic_context,
                    source_fields.reaches,
                );
                antlr_lexer_atn_config_set_fields(ctx, this, lexer_action_executor, passed);
            }
        }
        5 => {
            if matches!(args.get(2), Some(Value::Int(_))) {
                // LexerATNConfig(ATNState, int, PredictionContext, LexerActionExecutor)
                let state = antlr_object_option_arg(args, 1)?;
                let alt = antlr_int_arg(args, 2)?;
                let context = antlr_object_option_arg(args, 3)?;
                let lexer_action_executor = antlr_object_option_arg(args, 4)?;
                let mut scope = NativeHandleScope::new(ctx);
                let this_h = scope.root(this);
                let state_h = state.map(|state| scope.root(state));
                let context_h = context.map(|context| scope.root(context));
                let executor_h = lexer_action_executor.map(|obj| scope.root(obj));
                let semantic_context = antlr_semantic_empty_instance(&mut *scope, names)?;
                let this = scope.get(&this_h);
                let state = state_h.as_ref().map(|handle| scope.get(handle));
                let context = context_h.as_ref().map(|handle| scope.get(handle));
                antlr_set_atn_config_fields(
                    &mut *scope,
                    this,
                    state,
                    alt,
                    context,
                    semantic_context,
                    0,
                );
                let this = scope.get(&this_h);
                let lexer_action_executor = executor_h.as_ref().map(|handle| scope.get(handle));
                antlr_lexer_atn_config_set_fields(&mut *scope, this, lexer_action_executor, false);
            } else {
                // The current ANTLR runtime has no 4-argument copy constructor
                // on LexerATNConfig. Keep this branch explicit for descriptor
                // drift rather than silently initializing the wrong layout.
                return Err(RuntimeError::IllegalArgumentException {
                    message: "unsupported LexerATNConfig constructor shape".to_string(),
                }
                .into());
            }
        }
        3 => {
            // LexerATNConfig(LexerATNConfig, ATNState)
            let source = obj_arg(args, 1)?;
            let source_fields = antlr_atn_config_snapshot(ctx, source);
            let source_lexer_action_executor =
                antlr_ref_field(ctx, source, "lexerActionExecutor", 5);
            let state = antlr_object_option_arg(args, 2)?;
            let passed = antlr_lexer_non_greedy_from_source(ctx, source, state);
            antlr_set_atn_config_fields(
                ctx,
                this,
                state,
                source_fields.alt,
                source_fields.context,
                source_fields.semantic_context,
                source_fields.reaches,
            );
            antlr_lexer_atn_config_set_fields(ctx, this, source_lexer_action_executor, passed);
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "unsupported LexerATNConfig constructor arity {}",
                    args.len()
                ),
            }
            .into())
        }
    }

    Ok(None)
}

fn native_antlr_semantic_context_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(antlr_semantic_context_hash(
        ctx,
        Some(this),
    )?)))
}

fn native_antlr_semantic_context_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(other)) => *other,
        _ => None,
    };
    Ok(Some(antlr_bool(antlr_semantic_contexts_equal(
        ctx,
        Some(this),
        other,
    )?)))
}

fn antlr_object_hash(
    ctx: &mut dyn NativeContext,
    obj: Option<ObjectRef>,
) -> Result<i32, MethodCallFailed> {
    let Some(obj) = obj else {
        return Ok(0);
    };
    // The identity-hash fallback reads `obj` again AFTER `hashCode()` has run
    // Java, so it needs a handle.
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(obj);
    let obj = scope.get(&obj_h);
    match scope.invoke_virtual(obj, "hashCode", "()I", &[])? {
        Some(Value::Int(v)) => Ok(v),
        _ => {
            let obj = scope.get(&obj_h);
            Ok(scope.identity_hash_code(obj))
        }
    }
}

fn antlr_object_equals(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(false);
    };
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let a = scope.get(&a_h);
    let equals_args = [Value::Object(Some(scope.get(&b_h)))];
    Ok(
        match scope.invoke_virtual(a, "equals", "(Ljava/lang/Object;)Z", &equals_args)? {
            Some(Value::Int(v)) => v != 0,
            _ => false,
        },
    )
}

fn antlr_atn_config_hash(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let mut hash = 7;
    hash = antlr_murmur_update(hash, antlr_atn_config_state_number(ctx, config));
    hash = antlr_murmur_update(hash, antlr_atn_config_alt(ctx, config));
    hash = antlr_murmur_update(
        hash,
        antlr_atn_config_context(ctx, config)
            .map(|context| antlr_prediction_context_hash(ctx, context))
            .unwrap_or(0),
    );
    let semantic_context = antlr_atn_config_semantic_context(ctx, config);
    hash = antlr_murmur_update(hash, antlr_semantic_context_hash(ctx, semantic_context)?);
    Ok(antlr_murmur_finish(hash, 4))
}

fn native_antlr_atn_config_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(antlr_atn_config_hash(ctx, this)?)))
}

fn native_antlr_atn_config_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(antlr_bool(antlr_atn_configs_equal(ctx, this, other)?)))
}

fn antlr_atn_config_set_config_list_hash(
    ctx: &mut dyn NativeContext,
    configs: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    // Hashing an entry reaches `Object.hashCode` and mints class mirrors, so
    // re-read the backing array on every iteration.
    let mut scope = NativeHandleScope::new(ctx);
    let configs_h = scope.root(configs);
    let Some(data) = antlr_arraylist_data(&mut *scope, configs) else {
        return Ok(1);
    };
    let data_h = scope.root(data);
    let configs = scope.get(&configs_h);
    let size = std::cmp::min(
        antlr_arraylist_size(&mut *scope, configs),
        scope.array_length(data),
    );
    let mut hash = 1i32;
    for i in 0..size {
        let data = scope.get(&data_h);
        let item_hash = match scope.get_array_element(data, i) {
            Value::Object(Some(config)) => {
                let class_name = scope
                    .class_name_of_id(scope.class_id_of_object(config))
                    .unwrap_or_default();
                if antlr_class_name_matches(&class_name, "/atn/LexerATNConfig") {
                    antlr_lexer_atn_config_hash(&mut *scope, config)?
                } else {
                    antlr_atn_config_hash(&mut *scope, config)?
                }
            }
            _ => 0,
        };
        hash = hash.wrapping_mul(31).wrapping_add(item_hash);
    }
    Ok(hash)
}

fn antlr_atn_config_set_hash(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let cached = antlr_int_field(ctx, set, "cachedHashCode", 8);
    if antlr_int_field(ctx, set, "readonly", 0) != 0 && cached != -1 {
        return Ok(cached);
    }
    // The list hash can collect, and `set` is written afterwards.
    let mut scope = NativeHandleScope::new(ctx);
    let set_h = scope.root(set);
    let hash = match antlr_ref_field(&mut *scope, set, "configs", 2) {
        Some(configs) => antlr_atn_config_set_config_list_hash(&mut *scope, configs)?,
        None => 1,
    };
    let set = scope.get(&set_h);
    if antlr_int_field(&mut *scope, set, "readonly", 0) != 0 {
        antlr_set_field_value(&mut *scope, set, "cachedHashCode", 8, Value::Int(hash));
    }
    Ok(hash)
}

fn native_antlr_atn_config_set_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(antlr_atn_config_set_hash(ctx, this)?)))
}

fn antlr_atn_config_set_configs_equal(
    ctx: &mut dyn NativeContext,
    a_configs: ObjectRef,
    b_configs: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let a_len = antlr_arraylist_size(ctx, a_configs);
    let b_len = antlr_arraylist_size(ctx, b_configs);
    if a_len != b_len {
        return Ok(false);
    }
    // Comparing two configs reaches Java equality and can collect, so both
    // backing arrays are re-read on every iteration.
    let mut scope = NativeHandleScope::new(ctx);
    let (Some(a_data), Some(b_data)) = (
        antlr_arraylist_data(&mut *scope, a_configs),
        antlr_arraylist_data(&mut *scope, b_configs),
    ) else {
        return Ok(a_len == 0);
    };
    let a_data_h = scope.root(a_data);
    let b_data_h = scope.root(b_data);
    let len = std::cmp::min(
        a_len,
        std::cmp::min(scope.array_length(a_data), scope.array_length(b_data)),
    );
    if len != a_len {
        return Ok(false);
    }
    for i in 0..len {
        let a_data = scope.get(&a_data_h);
        let b_data = scope.get(&b_data_h);
        let a = match scope.get_array_element(a_data, i) {
            Value::Object(o) => o,
            _ => None,
        };
        let b = match scope.get_array_element(b_data, i) {
            Value::Object(o) => o,
            _ => None,
        };
        let equal = match (a, b) {
            (Some(a), Some(b)) => {
                let class_name = scope
                    .class_name_of_id(scope.class_id_of_object(a))
                    .unwrap_or_default();
                if antlr_class_name_matches(&class_name, "/atn/LexerATNConfig") {
                    antlr_lexer_atn_configs_equal(&mut *scope, a, b)?
                } else {
                    antlr_atn_configs_equal(&mut *scope, a, b)?
                }
            }
            (None, None) => true,
            _ => false,
        };
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

fn antlr_atn_config_set_equals(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let b_name = ctx
        .class_name_of_id(ctx.class_id_of_object(b))
        .unwrap_or_default();
    if !antlr_class_name_matches(&b_name, "/atn/ATNConfigSet")
        && !antlr_class_name_matches(&b_name, "/atn/OrderedATNConfigSet")
    {
        return Ok(false);
    }
    // Both sets are read again after the element-wise comparison, which can
    // collect.
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let (Some(a_configs), Some(b_configs)) = (
        antlr_ref_field(&mut *scope, a, "configs", 2),
        antlr_ref_field(&mut *scope, b, "configs", 2),
    ) else {
        return Ok(false);
    };
    if !antlr_atn_config_set_configs_equal(&mut *scope, a_configs, b_configs)? {
        return Ok(false);
    }
    let a = scope.get(&a_h);
    let b = scope.get(&b_h);
    Ok(antlr_int_field(&mut *scope, a, "fullCtx", 7)
        == antlr_int_field(&mut *scope, b, "fullCtx", 7)
        && antlr_int_field(&mut *scope, a, "uniqueAlt", 3)
            == antlr_int_field(&mut *scope, b, "uniqueAlt", 3)
        && antlr_ref_field(&mut *scope, a, "conflictingAlts", 4)
            == antlr_ref_field(&mut *scope, b, "conflictingAlts", 4)
        && antlr_int_field(&mut *scope, a, "hasSemanticContext", 5)
            == antlr_int_field(&mut *scope, b, "hasSemanticContext", 5)
        && antlr_int_field(&mut *scope, a, "dipsIntoOuterContext", 6)
            == antlr_int_field(&mut *scope, b, "dipsIntoOuterContext", 6))
}

fn native_antlr_atn_config_set_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(antlr_bool(antlr_atn_config_set_equals(
        ctx, this, other,
    )?)))
}

fn antlr_lexer_atn_config_hash(
    ctx: &mut dyn NativeContext,
    config: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    // Unlike the plain ATNConfig hash, `config` is read again *after* the
    // semantic-context hash (which can mint a class mirror), so it needs a
    // handle.
    let mut scope = NativeHandleScope::new(ctx);
    let config_h = scope.root(config);
    let mut hash = 7;
    hash = antlr_murmur_update(hash, antlr_atn_config_state_number(&mut *scope, config));
    hash = antlr_murmur_update(hash, antlr_atn_config_alt(&mut *scope, config));
    hash = antlr_murmur_update(
        hash,
        antlr_atn_config_context(&mut *scope, config)
            .map(|context| antlr_prediction_context_hash(&mut *scope, context))
            .unwrap_or(0),
    );
    let semantic_context = antlr_atn_config_semantic_context(&mut *scope, config);
    hash = antlr_murmur_update(
        hash,
        antlr_semantic_context_hash(&mut *scope, semantic_context)?,
    );
    let config = scope.get(&config_h);
    hash = antlr_murmur_update(
        hash,
        antlr_int_field(&mut *scope, config, "passedThroughNonGreedyDecision", 6).signum(),
    );
    let lexer_action_executor = antlr_ref_field(&mut *scope, config, "lexerActionExecutor", 5);
    hash = antlr_murmur_update(hash, antlr_object_hash(&mut *scope, lexer_action_executor)?);
    Ok(antlr_murmur_finish(hash, 6))
}

fn antlr_lexer_atn_configs_equal(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let b_name = ctx
        .class_name_of_id(ctx.class_id_of_object(b))
        .unwrap_or_default();
    if !antlr_class_name_matches(&b_name, "/atn/LexerATNConfig") {
        return Ok(false);
    }
    // Both configs are needed after the executor comparison, which runs
    // `Object.equals` bytecode and can collect.
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    if antlr_int_field(&mut *scope, a, "passedThroughNonGreedyDecision", 6).signum()
        != antlr_int_field(&mut *scope, b, "passedThroughNonGreedyDecision", 6).signum()
    {
        return Ok(false);
    }
    let a_lexer_action_executor = antlr_ref_field(&mut *scope, a, "lexerActionExecutor", 5);
    let b_lexer_action_executor = antlr_ref_field(&mut *scope, b, "lexerActionExecutor", 5);
    if !antlr_object_equals(
        &mut *scope,
        a_lexer_action_executor,
        b_lexer_action_executor,
    )? {
        return Ok(false);
    }
    let a = scope.get(&a_h);
    let b = scope.get(&b_h);
    antlr_atn_configs_equal(&mut *scope, a, b)
}

fn native_antlr_lexer_atn_config_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(antlr_lexer_atn_config_hash(ctx, this)?)))
}

fn native_antlr_lexer_atn_config_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(antlr_bool(antlr_lexer_atn_configs_equal(
        ctx, this, other,
    )?)))
}

fn antlr_dfa_state_configs(ctx: &mut dyn NativeContext, state: ObjectRef) -> Option<ObjectRef> {
    antlr_ref_field(ctx, state, "configs", 1)
}

fn native_antlr_dfa_state_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let configs_hash = match antlr_dfa_state_configs(ctx, this) {
        Some(configs) => antlr_atn_config_set_hash(ctx, configs)?,
        None => 0,
    };
    let hash = antlr_murmur_finish(antlr_murmur_update(7, configs_hash), 1);
    Ok(Some(Value::Int(hash)))
}

fn native_antlr_dfa_state_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other_name = ctx
        .class_name_of_id(ctx.class_id_of_object(other))
        .unwrap_or_default();
    if !antlr_class_name_matches(&other_name, "/dfa/DFAState") {
        return Ok(Some(Value::Int(0)));
    }
    let equal = match (
        antlr_dfa_state_configs(ctx, this),
        antlr_dfa_state_configs(ctx, other),
    ) {
        (Some(a), Some(b)) => antlr_atn_config_set_equals(ctx, a, b)?,
        (None, None) => true,
        _ => false,
    };
    Ok(Some(antlr_bool(equal)))
}

fn antlr_contexts_equal_opt(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => antlr_prediction_contexts_equal(ctx, a, b),
        (None, None) => true,
        _ => false,
    }
}

fn antlr_add_unique_alt(alts: &mut Vec<i32>, alt: i32) {
    if alt >= 0 && !alts.contains(&alt) {
        alts.push(alt);
    }
}

fn antlr_prediction_mode_conflicting_alt_groups(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
) -> Vec<Vec<i32>> {
    let mut groups: Vec<AntlrAltSubsetGroup> = Vec::new();
    for index in 0..antlr_atn_config_set_size(ctx, set) {
        let Some(config) = antlr_config_set_at(ctx, set, index) else {
            continue;
        };
        let state_number = antlr_atn_config_state_number(ctx, config);
        let context = antlr_atn_config_context(ctx, config);
        let alt = antlr_atn_config_alt(ctx, config);
        if let Some(group) = groups.iter_mut().find(|group| {
            group.state_number == state_number
                && antlr_contexts_equal_opt(ctx, group.context, context)
        }) {
            antlr_add_unique_alt(&mut group.alts, alt);
        } else {
            let mut alts = Vec::with_capacity(2);
            antlr_add_unique_alt(&mut alts, alt);
            groups.push(AntlrAltSubsetGroup {
                state_number,
                context,
                alts,
            });
        }
    }
    groups.into_iter().map(|group| group.alts).collect()
}

fn antlr_new_arraylist_with_capacity(
    ctx: &mut dyn NativeContext,
    capacity: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
    let list_h = scope.root(list);
    let data = scope.new_array(cratonvm_types::ArrayElementType::Reference, capacity.max(1));
    let list = scope.get(&list_h);
    antlr_arraylist_set_data(&mut *scope, list, data);
    antlr_arraylist_set_size(&mut *scope, list, 0);
    Ok(scope.get(&list_h))
}

fn antlr_new_bitset_with_alts(
    ctx: &mut dyn NativeContext,
    alts: &[i32],
) -> Result<ObjectRef, MethodCallFailed> {
    let max_alt = alts
        .iter()
        .copied()
        .filter(|alt| *alt >= 0)
        .max()
        .unwrap_or(0) as usize;
    let word_count = (max_alt / 64) + 1;
    let mut scope = NativeHandleScope::new(ctx);
    let bitset = try_alloc_concurrent_synthetic(&mut *scope, "java/util/BitSet", 3)?;
    let bitset_h = scope.root(bitset);
    let words = scope.new_array(cratonvm_types::ArrayElementType::Long, word_count);
    let bitset = scope.get(&bitset_h);
    scope.set_field(bitset, 0, Value::Object(Some(words)));
    scope.set_field(bitset, 1, Value::Int(0));
    scope.set_field(bitset, 2, Value::Int(0));
    let mut words_in_use = 0usize;
    for alt in alts.iter().copied().filter(|alt| *alt >= 0) {
        let bit = alt as usize;
        let word_idx = bit / 64;
        words_in_use = words_in_use.max(word_idx + 1);
        let mask = 1i64 << (bit % 64);
        let word = match scope.get_array_element(words, word_idx) {
            Value::Long(v) => v,
            _ => 0,
        };
        scope.set_array_element(words, word_idx, Value::Long(word | mask));
    }
    let words_in_use = std::cmp::min(words_in_use, i32::MAX as usize) as i32;
    let bitset = scope.get(&bitset_h);
    scope.set_field(bitset, 1, Value::Int(words_in_use));
    Ok(scope.get(&bitset_h))
}

fn native_antlr_prediction_mode_get_conflicting_alt_subsets(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let set = obj_arg(args, 0)?;
    let alt_groups = antlr_prediction_mode_conflicting_alt_groups(ctx, set);
    let mut scope = NativeHandleScope::new(ctx);
    let result = antlr_new_arraylist_with_capacity(&mut *scope, alt_groups.len())?;
    let result_h = scope.root(result);
    for alts in alt_groups {
        let bitset = antlr_new_bitset_with_alts(&mut *scope, &alts)?;
        let bitset_h = scope.root(bitset);
        let result = scope.get(&result_h);
        let bitset = scope.get(&bitset_h);
        antlr_arraylist_append(&mut *scope, result, Value::Object(Some(bitset)))?;
    }
    Ok(Some(Value::Object(Some(scope.get(&result_h)))))
}

fn native_antlr_prediction_mode_has_state_associated_with_one_alt(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let set = obj_arg(args, 0)?;
    let mut state_to_alts: Vec<(i32, Vec<i32>)> = Vec::new();
    for index in 0..antlr_atn_config_set_size(ctx, set) {
        let Some(config) = antlr_config_set_at(ctx, set, index) else {
            continue;
        };
        let state_number = antlr_atn_config_state_number(ctx, config);
        let alt = antlr_atn_config_alt(ctx, config);
        if let Some((_, alts)) = state_to_alts
            .iter_mut()
            .find(|(candidate_state, _)| *candidate_state == state_number)
        {
            antlr_add_unique_alt(alts, alt);
        } else {
            let mut alts = Vec::with_capacity(2);
            antlr_add_unique_alt(&mut alts, alt);
            state_to_alts.push((state_number, alts));
        }
    }
    Ok(Some(antlr_bool(
        state_to_alts.iter().any(|(_, alts)| alts.len() == 1),
    )))
}

fn antlr_merge_cache_get(
    ctx: &mut dyn NativeContext,
    cache: Option<ObjectRef>,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(cache) = cache else {
        return Ok(None);
    };
    match antlr_double_key_map_get_value(
        ctx,
        cache,
        Value::Object(Some(a)),
        Value::Object(Some(b)),
    )? {
        Some(Value::Object(previous)) => Ok(previous),
        _ => Ok(None),
    }
}

fn antlr_merge_cache_put(
    ctx: &mut dyn NativeContext,
    cache: Option<ObjectRef>,
    a: ObjectRef,
    b: ObjectRef,
    value: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let Some(cache) = cache else {
        return Ok(());
    };
    antlr_double_key_map_put_value(
        ctx,
        cache,
        Value::Object(Some(a)),
        Value::Object(Some(b)),
        Value::Object(Some(value)),
    )?;
    Ok(())
}

#[inline]
fn antlr_is_singleton_like(kind: AntlrPredictionContextKind) -> bool {
    matches!(
        kind,
        AntlrPredictionContextKind::Singleton | AntlrPredictionContextKind::Empty
    )
}

fn antlr_merge_root_contexts(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    a: ObjectRef,
    b: ObjectRef,
    root_is_wildcard: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let a_empty = antlr_object_kind(ctx, a) == AntlrPredictionContextKind::Empty;
    let b_empty = antlr_object_kind(ctx, b) == AntlrPredictionContextKind::Empty;
    if root_is_wildcard {
        if a_empty || b_empty {
            return Ok(Some(antlr_empty_instance(ctx, names)?));
        }
        return Ok(None);
    }

    if a_empty && b_empty {
        return Ok(Some(antlr_empty_instance(ctx, names)?));
    }
    if a_empty {
        let b_parent = antlr_singleton_parent(ctx, b);
        let b_state = antlr_singleton_return_state(ctx, b);
        return Ok(Some(antlr_create_array_context(
            ctx,
            names,
            &[b_parent, None],
            &[b_state, ANTLR_EMPTY_RETURN_STATE],
        )?));
    }
    if b_empty {
        let a_parent = antlr_singleton_parent(ctx, a);
        let a_state = antlr_singleton_return_state(ctx, a);
        return Ok(Some(antlr_create_array_context(
            ctx,
            names,
            &[a_parent, None],
            &[a_state, ANTLR_EMPTY_RETURN_STATE],
        )?));
    }
    Ok(None)
}

fn antlr_merge_contexts(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
    root_is_wildcard: bool,
    merge_cache: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let cache_h = merge_cache.map(|cache| scope.root(cache));

    let a = scope.get(&a_h);
    let b = scope.get(&b_h);
    if a == b || antlr_prediction_contexts_equal(&mut *scope, a, b) {
        return Ok(scope.get(&a_h));
    }

    let names = antlr_names_for_object(&mut *scope, a);
    let a_kind = antlr_object_kind(&mut *scope, a);
    let b_kind = antlr_object_kind(&mut *scope, b);
    if antlr_is_singleton_like(a_kind) && antlr_is_singleton_like(b_kind) {
        let a = scope.get(&a_h);
        let b = scope.get(&b_h);
        let merge_cache = cache_h.as_ref().map(|handle| scope.get(handle));
        return antlr_merge_singletons(&mut *scope, names, a, b, root_is_wildcard, merge_cache);
    }

    if root_is_wildcard {
        if a_kind == AntlrPredictionContextKind::Empty {
            return Ok(scope.get(&a_h));
        }
        if b_kind == AntlrPredictionContextKind::Empty {
            return Ok(scope.get(&b_h));
        }
    }

    let converted_a_h = if antlr_is_singleton_like(a_kind) {
        let a = scope.get(&a_h);
        let converted = antlr_singleton_to_array_context(&mut *scope, names, a)?;
        scope.root(converted)
    } else {
        let a = scope.get(&a_h);
        scope.root(a)
    };

    let converted_b_h = if antlr_is_singleton_like(b_kind) {
        let b = scope.get(&b_h);
        let converted = antlr_singleton_to_array_context(&mut *scope, names, b)?;
        scope.root(converted)
    } else {
        let b = scope.get(&b_h);
        scope.root(b)
    };

    let converted_a = scope.get(&converted_a_h);
    let converted_b = scope.get(&converted_b_h);
    let merge_cache = cache_h.as_ref().map(|handle| scope.get(handle));
    antlr_merge_arrays(
        &mut *scope,
        names,
        converted_a,
        converted_b,
        root_is_wildcard,
        merge_cache,
    )
}

fn antlr_merge_singletons(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    a: ObjectRef,
    b: ObjectRef,
    root_is_wildcard: bool,
    merge_cache: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let cache_h = merge_cache.map(|cache| scope.root(cache));
    // Re-read all three at every step: the merge cache is a Java map, so both
    // its lookups and its stores run bytecode and can collect.
    macro_rules! roots {
        () => {{
            let a = scope.get(&a_h);
            let b = scope.get(&b_h);
            let cache = cache_h.as_ref().map(|handle| scope.get(handle));
            (a, b, cache)
        }};
    }

    let (a, b, merge_cache) = roots!();
    if let Some(previous) = antlr_merge_cache_get(&mut *scope, merge_cache, a, b)? {
        return Ok(previous);
    }
    let (a, b, merge_cache) = roots!();
    if let Some(previous) = antlr_merge_cache_get(&mut *scope, merge_cache, b, a)? {
        return Ok(previous);
    }

    let (a, b, _) = roots!();
    if let Some(root_merge) = antlr_merge_root_contexts(&mut *scope, names, a, b, root_is_wildcard)?
    {
        let root_merge_h = scope.root(root_merge);
        let (a, b, merge_cache) = roots!();
        let root_merge = scope.get(&root_merge_h);
        antlr_merge_cache_put(&mut *scope, merge_cache, a, b, root_merge)?;
        return Ok(scope.get(&root_merge_h));
    }

    let (a, b, merge_cache) = roots!();
    let a_state = antlr_singleton_return_state(&mut *scope, a);
    let b_state = antlr_singleton_return_state(&mut *scope, b);
    let a_parent = antlr_singleton_parent(&mut *scope, a);
    let b_parent = antlr_singleton_parent(&mut *scope, b);

    if a_state == b_state {
        let parent = match (a_parent, b_parent) {
            (Some(pa), Some(pb)) => Some(antlr_merge_contexts(
                &mut *scope,
                pa,
                pb,
                root_is_wildcard,
                merge_cache,
            )?),
            (None, None) => None,
            (Some(pa), None) => Some(pa),
            (None, Some(pb)) => Some(pb),
        };
        let parent_h = parent.map(|parent| scope.root(parent));
        let (a, b, _) = roots!();
        let parent = parent_h.as_ref().map(|handle| scope.get(handle));
        let a_parent = antlr_singleton_parent(&mut *scope, a);
        let b_parent = antlr_singleton_parent(&mut *scope, b);
        if parent == a_parent {
            return Ok(scope.get(&a_h));
        }
        if parent == b_parent {
            return Ok(scope.get(&b_h));
        }
        let merged = antlr_create_singleton_context(&mut *scope, names, parent, a_state)?;
        let merged_h = scope.root(merged);
        let (a, b, merge_cache) = roots!();
        let merged = scope.get(&merged_h);
        antlr_merge_cache_put(&mut *scope, merge_cache, a, b, merged)?;
        return Ok(scope.get(&merged_h));
    }

    let single_parent = if a == b {
        a_parent
    } else {
        match (a_parent, b_parent) {
            (Some(pa), Some(pb)) if antlr_prediction_contexts_equal(&mut *scope, pa, pb) => {
                Some(pa)
            }
            _ => None,
        }
    };

    if let Some(parent) = single_parent {
        let mut states = [a_state, b_state];
        if a_state > b_state {
            states = [b_state, a_state];
        }
        let merged =
            antlr_create_array_context(&mut *scope, names, &[Some(parent), Some(parent)], &states)?;
        let merged_h = scope.root(merged);
        let (a, b, merge_cache) = roots!();
        let merged = scope.get(&merged_h);
        antlr_merge_cache_put(&mut *scope, merge_cache, a, b, merged)?;
        return Ok(scope.get(&merged_h));
    }

    let mut states = [a_state, b_state];
    let mut parents = [a_parent, b_parent];
    if a_state > b_state {
        states = [b_state, a_state];
        parents = [b_parent, a_parent];
    }
    let merged = antlr_create_array_context(&mut *scope, names, &parents, &states)?;
    let merged_h = scope.root(merged);
    let (a, b, merge_cache) = roots!();
    let merged = scope.get(&merged_h);
    antlr_merge_cache_put(&mut *scope, merge_cache, a, b, merged)?;
    Ok(scope.get(&merged_h))
}

fn antlr_array_states(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<i32> {
    let Some(states) = antlr_array_return_states(ctx, obj) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(ctx.array_length(states));
    for i in 0..ctx.array_length(states) {
        out.push(match ctx.get_array_element(states, i) {
            Value::Int(v) => v,
            _ => 0,
        });
    }
    out
}

fn antlr_array_parent_vec(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<Option<ObjectRef>> {
    let Some(parents) = antlr_array_parents(ctx, obj) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(ctx.array_length(parents));
    for i in 0..ctx.array_length(parents) {
        out.push(match ctx.get_array_element(parents, i) {
            Value::Object(parent) => parent,
            _ => None,
        });
    }
    out
}

fn antlr_combine_common_parents(ctx: &mut dyn NativeContext, parents: &mut [Option<ObjectRef>]) {
    for i in 0..parents.len() {
        let Some(parent) = parents[i] else {
            continue;
        };
        for j in 0..i {
            if let Some(existing) = parents[j] {
                if parent == existing || antlr_prediction_contexts_equal(ctx, parent, existing) {
                    parents[i] = Some(existing);
                    break;
                }
            }
        }
    }
}

fn antlr_merge_arrays(
    ctx: &mut dyn NativeContext,
    names: AntlrClassNames,
    a: ObjectRef,
    b: ObjectRef,
    root_is_wildcard: bool,
    merge_cache: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let a_h = scope.root(a);
    let b_h = scope.root(b);
    let cache_h = merge_cache.map(|cache| scope.root(cache));
    macro_rules! roots {
        () => {{
            let a = scope.get(&a_h);
            let b = scope.get(&b_h);
            let cache = cache_h.as_ref().map(|handle| scope.get(handle));
            (a, b, cache)
        }};
    }

    let (a, b, merge_cache) = roots!();
    if let Some(previous) = antlr_merge_cache_get(&mut *scope, merge_cache, a, b)? {
        return Ok(previous);
    }
    let (a, b, merge_cache) = roots!();
    if let Some(previous) = antlr_merge_cache_get(&mut *scope, merge_cache, b, a)? {
        return Ok(previous);
    }

    let (a, b, _) = roots!();
    let a_states = antlr_array_states(&mut *scope, a);
    let b_states = antlr_array_states(&mut *scope, b);
    // Both parent arrays are walked while the merge recursion allocates, so
    // every element is a rooted handle rather than a raw reference.
    let a_parents: Vec<Option<NativeHandle>> = antlr_array_parent_vec(&mut *scope, a)
        .into_iter()
        .map(|parent| parent.map(|parent| scope.root(parent)))
        .collect();
    let b_parents: Vec<Option<NativeHandle>> = antlr_array_parent_vec(&mut *scope, b)
        .into_iter()
        .map(|parent| parent.map(|parent| scope.root(parent)))
        .collect();
    let mut merged_states = Vec::with_capacity(a_states.len() + b_states.len());
    let mut merged_parents: Vec<Option<NativeHandle>> =
        Vec::with_capacity(a_states.len() + b_states.len());

    let mut i = 0usize;
    let mut j = 0usize;
    while i < a_states.len() && j < b_states.len() {
        let a_parent = a_parents
            .get(i)
            .and_then(|handle| handle.as_ref())
            .map(|handle| scope.get(handle));
        let b_parent = b_parents
            .get(j)
            .and_then(|handle| handle.as_ref())
            .map(|handle| scope.get(handle));
        if a_states[i] == b_states[j] {
            let payload = a_states[i];
            let both_empty =
                payload == ANTLR_EMPTY_RETURN_STATE && a_parent.is_none() && b_parent.is_none();
            let same_parent = match (a_parent, b_parent) {
                (Some(pa), Some(pb)) => {
                    pa == pb || antlr_prediction_contexts_equal(&mut *scope, pa, pb)
                }
                _ => false,
            };
            if both_empty || same_parent {
                merged_parents.push(a_parent.map(|parent| scope.root(parent)));
                merged_states.push(payload);
            } else {
                let merged_parent = match (a_parent, b_parent) {
                    (Some(pa), Some(pb)) => {
                        // Re-read the cache immediately before the recursive
                        // merge: an earlier iteration's merge may have moved it.
                        let (_, _, merge_cache) = roots!();
                        Some(antlr_merge_contexts(
                            &mut *scope,
                            pa,
                            pb,
                            root_is_wildcard,
                            merge_cache,
                        )?)
                    }
                    (Some(pa), None) => Some(pa),
                    (None, Some(pb)) => Some(pb),
                    (None, None) => None,
                };
                merged_parents.push(merged_parent.map(|parent| scope.root(parent)));
                merged_states.push(payload);
            }
            i += 1;
            j += 1;
        } else if a_states[i] < b_states[j] {
            merged_parents.push(a_parent.map(|parent| scope.root(parent)));
            merged_states.push(a_states[i]);
            i += 1;
        } else {
            merged_parents.push(b_parent.map(|parent| scope.root(parent)));
            merged_states.push(b_states[j]);
            j += 1;
        }
    }

    while i < a_states.len() {
        let parent = a_parents
            .get(i)
            .and_then(|handle| handle.as_ref())
            .map(|handle| scope.get(handle));
        merged_parents.push(parent.map(|parent| scope.root(parent)));
        merged_states.push(a_states[i]);
        i += 1;
    }
    while j < b_states.len() {
        let parent = b_parents
            .get(j)
            .and_then(|handle| handle.as_ref())
            .map(|handle| scope.get(handle));
        merged_parents.push(parent.map(|parent| scope.root(parent)));
        merged_states.push(b_states[j]);
        j += 1;
    }

    if merged_parents.len() == 1 && merged_parents.len() < a_states.len() + b_states.len() {
        let parent = merged_parents[0].as_ref().map(|handle| scope.get(handle));
        let merged = antlr_create_singleton_context(&mut *scope, names, parent, merged_states[0])?;
        let merged_h = scope.root(merged);
        let (a, b, merge_cache) = roots!();
        let merged = scope.get(&merged_h);
        antlr_merge_cache_put(&mut *scope, merge_cache, a, b, merged)?;
        return Ok(scope.get(&merged_h));
    }

    let mut merged_parent_values: Vec<_> = merged_parents
        .iter()
        .map(|handle| handle.as_ref().map(|handle| scope.get(handle)))
        .collect();
    antlr_combine_common_parents(&mut *scope, &mut merged_parent_values);
    let merged =
        antlr_create_array_context(&mut *scope, names, &merged_parent_values, &merged_states)?;
    let merged_h = scope.root(merged);
    let (a, b, merge_cache) = roots!();
    let merged = scope.get(&merged_h);
    if antlr_prediction_contexts_equal(&mut *scope, merged, a) {
        antlr_merge_cache_put(&mut *scope, merge_cache, a, b, a)?;
        return Ok(scope.get(&a_h));
    }
    let (a, b, merge_cache) = roots!();
    let merged = scope.get(&merged_h);
    if antlr_prediction_contexts_equal(&mut *scope, merged, b) {
        antlr_merge_cache_put(&mut *scope, merge_cache, a, b, b)?;
        return Ok(scope.get(&b_h));
    }
    let (a, b, merge_cache) = roots!();
    let merged = scope.get(&merged_h);
    antlr_merge_cache_put(&mut *scope, merge_cache, a, b, merged)?;
    Ok(scope.get(&merged_h))
}

fn native_antlr_prediction_context_merge(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let root_is_wildcard = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    let merge_cache = match args.get(3) {
        Some(Value::Object(cache)) => *cache,
        _ => None,
    };
    Ok(Some(Value::Object(Some(antlr_merge_contexts(
        ctx,
        a,
        b,
        root_is_wildcard,
        merge_cache,
    )?))))
}

fn native_antlr_prediction_context_merge_singletons(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let names = antlr_names_for_object(ctx, a);
    let root_is_wildcard = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    let merge_cache = match args.get(3) {
        Some(Value::Object(cache)) => *cache,
        _ => None,
    };
    Ok(Some(Value::Object(Some(antlr_merge_singletons(
        ctx,
        names,
        a,
        b,
        root_is_wildcard,
        merge_cache,
    )?))))
}

fn native_antlr_prediction_context_merge_root(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let names = antlr_names_for_object(ctx, a);
    let root_is_wildcard = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    Ok(Some(Value::Object(antlr_merge_root_contexts(
        ctx,
        names,
        a,
        b,
        root_is_wildcard,
    )?)))
}

fn native_antlr_prediction_context_merge_arrays(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = obj_arg(args, 0)?;
    let b = obj_arg(args, 1)?;
    let names = antlr_names_for_object(ctx, a);
    let root_is_wildcard = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    let merge_cache = match args.get(3) {
        Some(Value::Object(cache)) => *cache,
        _ => None,
    };
    Ok(Some(Value::Object(Some(antlr_merge_arrays(
        ctx,
        names,
        a,
        b,
        root_is_wildcard,
        merge_cache,
    )?))))
}

fn antlr_common_token_value(ctx: &dyn NativeContext, this: ObjectRef, field: &str) -> Value {
    ctx.get_field_by_name(this, field)
}

fn native_antlr_common_token_get_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = match antlr_common_token_value(ctx, this, field) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(v)))
}

fn native_antlr_common_token_get_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "type")
}

fn native_antlr_common_token_get_line(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "line")
}

fn native_antlr_common_token_get_char_position_in_line(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "charPositionInLine")
}

fn native_antlr_common_token_get_channel(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "channel")
}

fn native_antlr_common_token_get_start_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "start")
}

fn native_antlr_common_token_get_stop_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "stop")
}

fn native_antlr_common_token_get_token_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_get_int(ctx, args, "index")
}

fn native_antlr_common_token_set_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field_by_name(this, field, Value::Int(antlr_int_arg(args, 1)?));
    Ok(None)
}

fn native_antlr_common_token_set_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "type")
}

fn native_antlr_common_token_set_line(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "line")
}

fn native_antlr_common_token_set_char_position_in_line(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "charPositionInLine")
}

fn native_antlr_common_token_set_channel(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "channel")
}

fn native_antlr_common_token_set_start_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "start")
}

fn native_antlr_common_token_set_stop_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "stop")
}

fn native_antlr_common_token_set_token_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_antlr_common_token_set_int(ctx, args, "index")
}

fn native_antlr_common_token_set_text(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let text = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field_by_name(this, "text", text);
    Ok(None)
}

fn antlr_common_token_pair_field(ctx: &dyn NativeContext, token: ObjectRef, field: &str) -> Value {
    match ctx.get_field_by_name(token, "source") {
        Value::Object(Some(source)) => ctx.get_field_by_name(source, field),
        _ => Value::Object(None),
    }
}

fn native_antlr_common_token_get_token_source(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(antlr_common_token_pair_field(ctx, this, "a")))
}

fn native_antlr_common_token_get_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(antlr_common_token_pair_field(ctx, this, "b")))
}

fn native_antlr_common_token_get_text(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Object(Some(text)) = ctx.get_field_by_name(this, "text") {
        return Ok(Some(Value::Object(Some(text))));
    }
    // `this` is read again after `CharStream.size()`, and `input` after
    // `Interval.of` — both run Java and can collect.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let input = match antlr_common_token_pair_field(&*scope, this, "b") {
        Value::Object(Some(input)) => input,
        _ => return Ok(Some(Value::Object(None))),
    };
    let input_h = scope.root(input);
    let size = match scope.invoke_virtual(input, "size", "()I", &[])? {
        Some(Value::Int(size)) => size,
        _ => 0,
    };
    let this = scope.get(&this_h);
    let start = match scope.get_field_by_name(this, "start") {
        Value::Int(v) => v,
        _ => -1,
    };
    let stop = match scope.get_field_by_name(this, "stop") {
        Value::Int(v) => v,
        _ => -1,
    };
    if start >= size || stop >= size {
        return Ok(Some(Value::Object(Some(scope.create_string("<EOF>")))));
    }
    let interval = match scope.invoke(
        "org/antlr/v4/runtime/misc/Interval",
        "of",
        "(II)Lorg/antlr/v4/runtime/misc/Interval;",
        &[Value::Int(start), Value::Int(stop)],
    )? {
        Some(Value::Object(Some(interval))) => interval,
        _ => return Ok(Some(Value::Object(None))),
    };
    let interval_h = scope.root(interval);
    let input = scope.get(&input_h);
    let get_text_args = [Value::Object(Some(scope.get(&interval_h)))];
    scope.invoke_virtual(
        input,
        "getText",
        "(Lorg/antlr/v4/runtime/misc/Interval;)Ljava/lang/String;",
        &get_text_args,
    )
}

fn antlr_copy_text_if_requested(
    ctx: &mut dyn NativeContext,
    factory: ObjectRef,
    source: Value,
    text: Value,
    start: i32,
    stop: i32,
) -> Result<Value, MethodCallFailed> {
    if !matches!(text, Value::Object(None)) {
        return Ok(text);
    }
    if !matches!(
        ctx.get_field_by_name(factory, "copyText"),
        Value::Int(v) if v != 0
    ) {
        return Ok(text);
    }
    let source = match source {
        Value::Object(Some(source)) => source,
        _ => return Ok(text),
    };
    let input = match ctx.get_field_by_name(source, "b") {
        Value::Object(Some(input)) => input,
        _ => return Ok(text),
    };
    // `Interval.of` allocates; `input` is the receiver of the call after it.
    let mut scope = NativeHandleScope::new(ctx);
    let input_h = scope.root(input);
    let interval = match scope.invoke(
        "org/antlr/v4/runtime/misc/Interval",
        "of",
        "(II)Lorg/antlr/v4/runtime/misc/Interval;",
        &[Value::Int(start), Value::Int(stop)],
    )? {
        Some(Value::Object(Some(interval))) => interval,
        _ => return Ok(text),
    };
    let interval_h = scope.root(interval);
    let input = scope.get(&input_h);
    let get_text_args = [Value::Object(Some(scope.get(&interval_h)))];
    match scope.invoke_virtual(
        input,
        "getText",
        "(Lorg/antlr/v4/runtime/misc/Interval;)Ljava/lang/String;",
        &get_text_args,
    )? {
        Some(v) => Ok(v),
        None => Ok(text),
    }
}

fn antlr_alloc_common_token(
    ctx: &mut dyn NativeContext,
    source: Value,
    token_type: i32,
    text: Value,
    channel: i32,
    start: i32,
    stop: i32,
    line: i32,
    char_position: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // `source` and `text` come from the caller and are stored into the token
    // AFTER `ensure_class_initialized` + `alloc_object`, both of which can
    // collect — so they are rooted, not carried as raw `Value`s.
    let mut scope = NativeHandleScope::new(ctx);
    let source_h = antlr_root_value(&mut scope, source);
    let text_h = antlr_root_value(&mut scope, text);
    let class_id = match scope.ensure_class_initialized(ANTLR_COMMON_TOKEN) {
        Ok(id) => id,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3). `org.antlr`'s
        // `CommonToken` is a dependency class, not a JDK one, so under
        // `--jdk-only` fabricating a stand-in for it is exactly the
        // substitution contract §5 refuses.
        Err(_) => crate::util_concurrent_ext::refused_class(&mut *scope, ANTLR_COMMON_TOKEN, 9)?,
    };
    let field_count = scope.class_num_total_fields(class_id).max(9);
    let token = scope.alloc_object(class_id, field_count);
    let token_h = scope.root(token);
    let source = antlr_read_rooted_value(&scope, source, source_h.as_ref());
    let text = antlr_read_rooted_value(&scope, text, text_h.as_ref());
    let token = scope.get(&token_h);
    scope.set_field_by_name(token, "charPositionInLine", Value::Int(char_position));
    scope.set_field_by_name(token, "channel", Value::Int(channel));
    scope.set_field_by_name(token, "index", Value::Int(-1));
    scope.set_field_by_name(token, "source", source);
    scope.set_field_by_name(token, "type", Value::Int(token_type));
    scope.set_field_by_name(token, "start", Value::Int(start));
    scope.set_field_by_name(token, "stop", Value::Int(stop));
    scope.set_field_by_name(token, "line", Value::Int(line));
    scope.set_field_by_name(token, "text", text);
    Ok(scope.get(&token_h))
}

fn native_antlr_common_token_factory_create_full(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let factory = obj_arg(args, 0)?;
    let source = args.get(1).copied().unwrap_or(Value::Object(None));
    let token_type = antlr_int_arg(args, 2)?;
    let text = args.get(3).copied().unwrap_or(Value::Object(None));
    let channel = antlr_int_arg(args, 4)?;
    let start = antlr_int_arg(args, 5)?;
    let stop = antlr_int_arg(args, 6)?;
    let line = antlr_int_arg(args, 7)?;
    let char_position = antlr_int_arg(args, 8)?;
    // `antlr_copy_text_if_requested` calls into Java (`Interval.of`,
    // `CharStream.getText`) and can collect, but `source` is stored into the
    // token afterwards.
    let mut scope = NativeHandleScope::new(ctx);
    let source_h = antlr_root_value(&mut scope, source);
    let text = antlr_copy_text_if_requested(&mut *scope, factory, source, text, start, stop)?;
    let source = antlr_read_rooted_value(&scope, source, source_h.as_ref());
    let token = antlr_alloc_common_token(
        &mut *scope,
        source,
        token_type,
        text,
        channel,
        start,
        stop,
        line,
        char_position,
    )?;
    Ok(Some(Value::Object(Some(token))))
}

fn native_antlr_common_token_factory_create_text(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let token_type = antlr_int_arg(args, 1)?;
    let text = args.get(2).copied().unwrap_or(Value::Object(None));
    // The caller's `text` has to survive CommonToken's <clinit>.
    let mut scope = NativeHandleScope::new(ctx);
    let text_h = antlr_root_value(&mut scope, text);
    let empty_source = match scope.ensure_class_initialized(ANTLR_COMMON_TOKEN) {
        Ok(class_id) => match scope.static_field_index_by_name(class_id, "EMPTY_SOURCE") {
            Some(index) => scope.get_static_field(class_id, index),
            None => Value::Object(None),
        },
        Err(_) => Value::Object(None),
    };
    let text = antlr_read_rooted_value(&scope, text, text_h.as_ref());
    let token =
        antlr_alloc_common_token(&mut *scope, empty_source, token_type, text, 0, 0, 0, 0, -1)?;
    Ok(Some(Value::Object(Some(token))))
}

pub(crate) fn register_antlr_token_intrinsics(registry: &mut NativeMethodRegistry) {
    const CREATE_FULL_COMMON_TOKEN: &str = "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/CommonToken;";
    const CREATE_FULL_TOKEN: &str =
        "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/Token;";
    registry.register(
        ANTLR_COMMON_TOKEN_FACTORY,
        "create",
        CREATE_FULL_COMMON_TOKEN,
        native_antlr_common_token_factory_create_full,
    );
    registry.register(
        ANTLR_COMMON_TOKEN_FACTORY,
        "create",
        CREATE_FULL_TOKEN,
        native_antlr_common_token_factory_create_full,
    );
    registry.register(
        ANTLR_COMMON_TOKEN_FACTORY,
        "create",
        "(ILjava/lang/String;)Lorg/antlr/v4/runtime/CommonToken;",
        native_antlr_common_token_factory_create_text,
    );
    registry.register(
        ANTLR_COMMON_TOKEN_FACTORY,
        "create",
        "(ILjava/lang/String;)Lorg/antlr/v4/runtime/Token;",
        native_antlr_common_token_factory_create_text,
    );

    for (name, desc, func) in [
        (
            "getType",
            "()I",
            native_antlr_common_token_get_type as NativeCallback,
        ),
        ("getLine", "()I", native_antlr_common_token_get_line),
        (
            "getCharPositionInLine",
            "()I",
            native_antlr_common_token_get_char_position_in_line,
        ),
        ("getChannel", "()I", native_antlr_common_token_get_channel),
        (
            "getStartIndex",
            "()I",
            native_antlr_common_token_get_start_index,
        ),
        (
            "getStopIndex",
            "()I",
            native_antlr_common_token_get_stop_index,
        ),
        (
            "getTokenIndex",
            "()I",
            native_antlr_common_token_get_token_index,
        ),
    ] {
        registry.register(ANTLR_COMMON_TOKEN, name, desc, func);
    }
    for (name, desc, func) in [
        (
            "setType",
            "(I)V",
            native_antlr_common_token_set_type as NativeCallback,
        ),
        ("setLine", "(I)V", native_antlr_common_token_set_line),
        (
            "setCharPositionInLine",
            "(I)V",
            native_antlr_common_token_set_char_position_in_line,
        ),
        ("setChannel", "(I)V", native_antlr_common_token_set_channel),
        (
            "setStartIndex",
            "(I)V",
            native_antlr_common_token_set_start_index,
        ),
        (
            "setStopIndex",
            "(I)V",
            native_antlr_common_token_set_stop_index,
        ),
        (
            "setTokenIndex",
            "(I)V",
            native_antlr_common_token_set_token_index,
        ),
    ] {
        registry.register(ANTLR_COMMON_TOKEN, name, desc, func);
    }
    registry.register(
        ANTLR_COMMON_TOKEN,
        "getText",
        "()Ljava/lang/String;",
        native_antlr_common_token_get_text,
    );
    registry.register(
        ANTLR_COMMON_TOKEN,
        "setText",
        "(Ljava/lang/String;)V",
        native_antlr_common_token_set_text,
    );
    registry.register(
        ANTLR_COMMON_TOKEN,
        "getTokenSource",
        "()Lorg/antlr/v4/runtime/TokenSource;",
        native_antlr_common_token_get_token_source,
    );
    registry.register(
        ANTLR_COMMON_TOKEN,
        "getInputStream",
        "()Lorg/antlr/v4/runtime/CharStream;",
        native_antlr_common_token_get_input_stream,
    );
}

pub(crate) fn register_antlr_prediction_context_intrinsics(registry: &mut NativeMethodRegistry) {
    for class_name in [ANTLR_DOUBLE_KEY_MAP, GROOVY_ANTLR_DOUBLE_KEY_MAP] {
        registry.register(
            class_name,
            "<init>",
            "()V",
            native_antlr_double_key_map_init,
        );
        registry.register(
            class_name,
            "get",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_antlr_double_key_map_get,
        );
        registry.register(
            class_name,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_antlr_double_key_map_put,
        );
    }

    registry.register(
        ANTLR_ATN_STATE,
        "getNumberOfTransitions",
        "()I",
        native_antlr_atn_state_get_number_of_transitions,
    );
    registry.register(
        ANTLR_ATN_STATE,
        "onlyHasEpsilonTransitions",
        "()Z",
        native_antlr_atn_state_only_has_epsilon_transitions,
    );
    registry.register(
        ANTLR_ATN_STATE,
        "transition",
        "(I)Lorg/antlr/v4/runtime/atn/Transition;",
        native_antlr_atn_state_transition,
    );
    registry.register(
        GROOVY_ANTLR_ATN_STATE,
        "getNumberOfTransitions",
        "()I",
        native_antlr_atn_state_get_number_of_transitions,
    );
    registry.register(
        GROOVY_ANTLR_ATN_STATE,
        "onlyHasEpsilonTransitions",
        "()Z",
        native_antlr_atn_state_only_has_epsilon_transitions,
    );
    registry.register(
        GROOVY_ANTLR_ATN_STATE,
        "transition",
        "(I)Lgroovyjarjarantlr4/v4/runtime/atn/Transition;",
        native_antlr_atn_state_transition,
    );

    for prefix in ["org/antlr/v4/runtime", "groovyjarjarantlr4/v4/runtime"] {
        let interval_set = format!("{prefix}/misc/IntervalSet");
        registry.register(
            &interval_set,
            "contains",
            "(I)Z",
            native_antlr_interval_set_contains,
        );
        let parser_atn_simulator = format!("{prefix}/atn/ParserATNSimulator");
        let atn_config_desc = format!("L{prefix}/atn/ATNConfig;");
        registry.register(
            &parser_atn_simulator,
            "canDropLoopEntryEdgeInLeftRecursiveRule",
            &format!("({atn_config_desc})Z"),
            native_antlr_parser_can_drop_loop_entry_edge,
        );
        let transition_desc = format!("L{prefix}/atn/Transition;");
        registry.register(
            &parser_atn_simulator,
            "getEpsilonTarget",
            &format!("({atn_config_desc}{transition_desc}ZZZZ){atn_config_desc}"),
            native_antlr_parser_get_epsilon_target,
        );
        let atn_config_set_desc = format!("L{prefix}/atn/ATNConfigSet;");
        registry.register(
            &parser_atn_simulator,
            "computeReachSet",
            &format!("({atn_config_set_desc}IZ){atn_config_set_desc}"),
            native_antlr_parser_compute_reach_set,
        );
        let closure_desc = format!("({atn_config_desc}{atn_config_set_desc}Ljava/util/Set;ZZIZ)V");
        let public_closure_desc =
            format!("({atn_config_desc}{atn_config_set_desc}Ljava/util/Set;ZZZ)V");
        registry.register(
            &parser_atn_simulator,
            "closure",
            &public_closure_desc,
            native_antlr_parser_public_closure,
        );
        registry.register(
            &parser_atn_simulator,
            "closureCheckingStopState",
            &closure_desc,
            native_antlr_parser_closure_checking_stop_state,
        );
        registry.register(
            &parser_atn_simulator,
            "closure_",
            &closure_desc,
            native_antlr_parser_closure,
        );
        let default_error_strategy = format!("{prefix}/DefaultErrorStrategy");
        let parser_desc = format!("L{prefix}/Parser;");
        registry.register(
            &default_error_strategy,
            "sync",
            &format!("({parser_desc})V"),
            native_antlr_default_error_strategy_sync,
        );
        let mismatch_desc = format!("L{prefix}/InputMismatchException;");
        registry.register(
            &default_error_strategy,
            "reportInputMismatch",
            &format!("({parser_desc}{mismatch_desc})V"),
            native_antlr_default_error_strategy_report_input_mismatch,
        );
        let semantic_context = format!("{prefix}/atn/SemanticContext");
        let semantic_context_desc = format!("L{prefix}/atn/SemanticContext;");
        let semantic_combine_desc =
            format!("({semantic_context_desc}{semantic_context_desc}){semantic_context_desc}");
        registry.register(
            &semantic_context,
            "and",
            &semantic_combine_desc,
            native_antlr_semantic_context_and,
        );
        registry.register(
            &semantic_context,
            "or",
            &semantic_combine_desc,
            native_antlr_semantic_context_or,
        );

        for simple_name in [
            "BasicState",
            "RuleStartState",
            "BasicBlockStartState",
            "PlusBlockStartState",
            "StarBlockStartState",
            "TokensStartState",
            "RuleStopState",
            "BlockEndState",
            "StarLoopbackState",
            "StarLoopEntryState",
            "PlusLoopbackState",
            "LoopEndState",
        ] {
            let class_name = format!("{prefix}/atn/{simple_name}");
            registry.register(
                &class_name,
                "getStateType",
                "()I",
                native_antlr_atn_state_get_state_type,
            );
        }

        let transition_base = format!("{prefix}/atn/Transition");
        registry.register(
            &transition_base,
            "isEpsilon",
            "()Z",
            native_antlr_transition_is_epsilon,
        );

        for simple_name in [
            "EpsilonTransition",
            "RangeTransition",
            "RuleTransition",
            "PredicateTransition",
            "AtomTransition",
            "ActionTransition",
            "SetTransition",
            "NotSetTransition",
            "WildcardTransition",
            "PrecedencePredicateTransition",
        ] {
            let class_name = format!("{prefix}/atn/{simple_name}");
            registry.register(
                &class_name,
                "getSerializationType",
                "()I",
                native_antlr_transition_get_serialization_type,
            );
            registry.register(
                &class_name,
                "isEpsilon",
                "()Z",
                native_antlr_transition_is_epsilon,
            );
            registry.register(
                &class_name,
                "matches",
                "(III)Z",
                native_antlr_transition_matches,
            );
        }
    }

    registry.register(
        ANTLR_ATN_CONFIG_SET,
        "add",
        "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z",
        native_antlr_atn_config_set_add,
    );
    registry.register(
        ANTLR_ATN_CONFIG_SET,
        "add",
        "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/misc/DoubleKeyMap;)Z",
        native_antlr_atn_config_set_add,
    );
    registry.register(
        GROOVY_ANTLR_ATN_CONFIG_SET,
        "add",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z",
        native_antlr_atn_config_set_add,
    );
    registry.register(
        GROOVY_ANTLR_ATN_CONFIG_SET,
        "add",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Z",
        native_antlr_atn_config_set_add,
    );
    for class_name in [
        ANTLR_ATN_CONFIG_SET,
        GROOVY_ANTLR_ATN_CONFIG_SET,
        "org/antlr/v4/runtime/atn/OrderedATNConfigSet",
        "groovyjarjarantlr4/v4/runtime/atn/OrderedATNConfigSet",
    ] {
        registry.register(
            class_name,
            "hashCode",
            "()I",
            native_antlr_atn_config_set_hash_code,
        );
        registry.register(
            class_name,
            "equals",
            "(Ljava/lang/Object;)Z",
            native_antlr_atn_config_set_equals,
        );
    }
    for prefix in ["org/antlr/v4/runtime", "groovyjarjarantlr4/v4/runtime"] {
        let atn_config = format!("{prefix}/atn/ATNConfig");
        let atn_state = format!("L{prefix}/atn/ATNState;");
        let prediction_context = format!("L{prefix}/atn/PredictionContext;");
        let semantic_context = format!("L{prefix}/atn/SemanticContext;");
        let atn_config_desc = format!("L{prefix}/atn/ATNConfig;");
        for descriptor in [
            format!("({atn_config_desc})V"),
            format!("({atn_config_desc}{atn_state})V"),
            format!("({atn_config_desc}{atn_state}{prediction_context})V"),
            format!("({atn_config_desc}{atn_state}{semantic_context})V"),
            format!("({atn_config_desc}{atn_state}{prediction_context}{semantic_context})V"),
        ] {
            registry.register(
                &atn_config,
                "<init>",
                &descriptor,
                native_antlr_atn_config_init,
            );
        }
    }
    for class_name in [ANTLR_ATN_CONFIG, GROOVY_ANTLR_ATN_CONFIG] {
        registry.register(
            class_name,
            "hashCode",
            "()I",
            native_antlr_atn_config_hash_code,
        );
        registry.register(
            class_name,
            "equals",
            "(Ljava/lang/Object;)Z",
            native_antlr_atn_config_equals,
        );
    }
    registry.register(
        ANTLR_ATN_CONFIG,
        "equals",
        "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z",
        native_antlr_atn_config_equals,
    );
    registry.register(
        GROOVY_ANTLR_ATN_CONFIG,
        "equals",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z",
        native_antlr_atn_config_equals,
    );
    for (class_name, typed_descriptor) in [
        (
            "org/antlr/v4/runtime/atn/LexerATNConfig",
            "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z",
        ),
        (
            "groovyjarjarantlr4/v4/runtime/atn/LexerATNConfig",
            "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z",
        ),
    ] {
        registry.register(
            class_name,
            "hashCode",
            "()I",
            native_antlr_lexer_atn_config_hash_code,
        );
        registry.register(
            class_name,
            "equals",
            "(Ljava/lang/Object;)Z",
            native_antlr_lexer_atn_config_equals,
        );
        registry.register(
            class_name,
            "equals",
            typed_descriptor,
            native_antlr_lexer_atn_config_equals,
        );
    }
    for class_name in [
        "org/antlr/v4/runtime/dfa/DFAState",
        "groovyjarjarantlr4/v4/runtime/dfa/DFAState",
    ] {
        registry.register(
            class_name,
            "hashCode",
            "()I",
            native_antlr_dfa_state_hash_code,
        );
        registry.register(
            class_name,
            "equals",
            "(Ljava/lang/Object;)Z",
            native_antlr_dfa_state_equals,
        );
    }
    for prefix in ["org/antlr/v4/runtime", "groovyjarjarantlr4/v4/runtime"] {
        for simple_name in ["Predicate", "PrecedencePredicate", "AND", "OR"] {
            let class_name = format!("{prefix}/atn/SemanticContext${simple_name}");
            registry.register(
                &class_name,
                "hashCode",
                "()I",
                native_antlr_semantic_context_hash_code,
            );
            registry.register(
                &class_name,
                "equals",
                "(Ljava/lang/Object;)Z",
                native_antlr_semantic_context_equals,
            );
        }
    }

    registry.register(
        ANTLR_PREDICTION_MODE,
        "getConflictingAltSubsets",
        "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;",
        native_antlr_prediction_mode_get_conflicting_alt_subsets,
    );
    registry.register(
        ANTLR_PREDICTION_MODE,
        "hasStateAssociatedWithOneAlt",
        "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Z",
        native_antlr_prediction_mode_has_state_associated_with_one_alt,
    );
    registry.register(
        GROOVY_ANTLR_PREDICTION_MODE,
        "getConflictingAltSubsets",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;",
        native_antlr_prediction_mode_get_conflicting_alt_subsets,
    );
    registry.register(
        GROOVY_ANTLR_PREDICTION_MODE,
        "hasStateAssociatedWithOneAlt",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;)Z",
        native_antlr_prediction_mode_has_state_associated_with_one_alt,
    );

    let context_classes = [
        ANTLR_PC,
        ANTLR_SINGLETON_PC,
        ANTLR_EMPTY_PC,
        ANTLR_ARRAY_PC,
        GROOVY_ANTLR_PC,
        GROOVY_ANTLR_SINGLETON_PC,
        GROOVY_ANTLR_EMPTY_PC,
        GROOVY_ANTLR_ARRAY_PC,
    ];
    for class_name in context_classes {
        registry.register(
            class_name,
            "hashCode",
            "()I",
            native_antlr_prediction_context_hash_code,
        );
        registry.register(
            class_name,
            "isEmpty",
            "()Z",
            native_antlr_prediction_context_is_empty,
        );
        registry.register(
            class_name,
            "hasEmptyPath",
            "()Z",
            native_antlr_prediction_context_has_empty_path,
        );
    }

    for class_name in [
        ANTLR_SINGLETON_PC,
        ANTLR_EMPTY_PC,
        ANTLR_ARRAY_PC,
        GROOVY_ANTLR_SINGLETON_PC,
        GROOVY_ANTLR_EMPTY_PC,
        GROOVY_ANTLR_ARRAY_PC,
    ] {
        registry.register(
            class_name,
            "size",
            "()I",
            native_antlr_prediction_context_size,
        );
        registry.register(
            class_name,
            "getReturnState",
            "(I)I",
            native_antlr_prediction_context_get_return_state,
        );
        registry.register(
            class_name,
            "equals",
            "(Ljava/lang/Object;)Z",
            native_antlr_prediction_context_equals,
        );
    }
    registry.register(
        ANTLR_SINGLETON_PC,
        "<init>",
        "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)V",
        native_antlr_singleton_prediction_context_init,
    );
    registry.register(
        GROOVY_ANTLR_SINGLETON_PC,
        "<init>",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)V",
        native_antlr_singleton_prediction_context_init,
    );
    registry.register(
        ANTLR_EMPTY_PC,
        "<init>",
        "()V",
        native_antlr_empty_prediction_context_init,
    );
    registry.register(
        GROOVY_ANTLR_EMPTY_PC,
        "<init>",
        "()V",
        native_antlr_empty_prediction_context_init,
    );
    registry.register(
        ANTLR_ARRAY_PC,
        "<init>",
        "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)V",
        native_antlr_array_prediction_context_init_arrays,
    );
    registry.register(
        GROOVY_ANTLR_ARRAY_PC,
        "<init>",
        "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)V",
        native_antlr_array_prediction_context_init_arrays,
    );
    registry.register(
        ANTLR_ARRAY_PC,
        "<init>",
        "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;)V",
        native_antlr_array_prediction_context_init_singleton,
    );
    registry.register(
        GROOVY_ANTLR_ARRAY_PC,
        "<init>",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;)V",
        native_antlr_array_prediction_context_init_singleton,
    );

    for class_name in [ANTLR_SINGLETON_PC, ANTLR_EMPTY_PC, ANTLR_ARRAY_PC] {
        registry.register(
            class_name,
            "getParent",
            "(I)Lorg/antlr/v4/runtime/atn/PredictionContext;",
            native_antlr_prediction_context_get_parent,
        );
    }
    for class_name in [
        GROOVY_ANTLR_SINGLETON_PC,
        GROOVY_ANTLR_EMPTY_PC,
        GROOVY_ANTLR_ARRAY_PC,
    ] {
        registry.register(
            class_name,
            "getParent",
            "(I)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;",
            native_antlr_prediction_context_get_parent,
        );
    }

    registry.register(
        ANTLR_PC,
        "calculateEmptyHashCode",
        "()I",
        native_antlr_prediction_context_calculate_empty_hash_code,
    );
    registry.register(
        ANTLR_PC,
        "calculateHashCode",
        "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)I",
        native_antlr_prediction_context_calculate_hash_singleton,
    );
    registry.register(
        ANTLR_PC,
        "calculateHashCode",
        "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)I",
        native_antlr_prediction_context_calculate_hash_array,
    );
    registry.register(
        ANTLR_PC,
        "merge",
        "(Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/PredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge,
    );
    registry.register(
        ANTLR_PC,
        "mergeSingletons",
        "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge_singletons,
    );
    registry.register(
        ANTLR_PC,
        "mergeRoot",
        "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Z)Lorg/antlr/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge_root,
    );
    registry.register(
        ANTLR_PC,
        "mergeArrays",
        "(Lorg/antlr/v4/runtime/atn/ArrayPredictionContext;Lorg/antlr/v4/runtime/atn/ArrayPredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge_arrays,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "calculateEmptyHashCode",
        "()I",
        native_antlr_prediction_context_calculate_empty_hash_code,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "calculateHashCode",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)I",
        native_antlr_prediction_context_calculate_hash_singleton,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "calculateHashCode",
        "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)I",
        native_antlr_prediction_context_calculate_hash_array,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "merge",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "mergeSingletons",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge_singletons,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "mergeRoot",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Z)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge_root,
    );
    registry.register(
        GROOVY_ANTLR_PC,
        "mergeArrays",
        "(Lgroovyjarjarantlr4/v4/runtime/atn/ArrayPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/ArrayPredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;",
        native_antlr_prediction_context_merge_arrays,
    );
}

#[cfg(test)]
mod antlr_prediction_context_tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ArrayElementType;

    /// Guard: this module must never go back to the raw pin trio.
    ///
    /// `pin_native_root` / `read_native_pin` / `unpin_native_roots` keep the
    /// same `ObjectRef` type on both sides of a collection, so reading the
    /// pre-call local instead of the re-read one type-checks fine — and
    /// `unpin_native_roots(base)` truncates the pin stack, silently dropping
    /// every root taken after `base`. Both mistakes were made in this file and
    /// both produced the nondeterministic HQL misparse recorded in
    /// `antlr-native-roots-moving-young-hql-misparse-20260730-FIXED.md`.
    /// `NativeHandleScope` + `NativeHandle` are the supported replacement:
    /// a handle is an opaque slot id, not a heap address, and the scope closes
    /// itself on return, `?` and unwind.
    ///
    /// A source-level check rather than a behavioural one, because the defect
    /// only shows up when a collection lands inside a specific window — which
    /// is exactly what no unit test can schedule.
    #[test]
    fn raw_pin_api_is_not_used_in_this_module() {
        // Assembled at runtime so this test does not match its own source.
        let banned: Vec<String> = [
            ["pin_native", "_root("],
            ["read_native", "_pin("],
            ["unpin_native", "_roots("],
        ]
        .iter()
        .map(|parts| parts.concat())
        .collect();
        let source = include_str!("antlr_intrinsics.rs");
        let offenders: Vec<(usize, &str)> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                // Skip the doc comments (here and at the top of the file) that
                // have to name the banned functions in order to ban them.
                let code = line.trim_start();
                !code.starts_with("//") && !code.starts_with("///")
            })
            .filter(|(_, line)| banned.iter().any(|needle| line.contains(needle.as_str())))
            .map(|(index, line)| (index + 1, line.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "antlr_intrinsics.rs must root objects with NativeHandleScope, not the raw pin API; \
             found {} offending line(s): {:?}",
            offenders.len(),
            offenders
        );
    }

    fn install_classes(ctx: &mut MockNativeContext) -> (ClassId, ClassId, ClassId, ClassId) {
        let pc = ctx.ensure_class_initialized(ANTLR_PC).unwrap();
        let singleton = ctx.ensure_class_initialized(ANTLR_SINGLETON_PC).unwrap();
        let empty = ctx.ensure_class_initialized(ANTLR_EMPTY_PC).unwrap();
        let array = ctx.ensure_class_initialized(ANTLR_ARRAY_PC).unwrap();
        ctx.set_superclass(singleton, pc);
        ctx.set_superclass(empty, singleton);
        ctx.set_superclass(array, pc);
        (pc, singleton, empty, array)
    }

    fn make_empty(ctx: &mut MockNativeContext, empty_class: ClassId) -> ObjectRef {
        let obj = ctx.alloc_object(empty_class, 4);
        ctx.set_field(obj, 1, Value::Int(antlr_murmur_finish(1, 0)));
        ctx.set_field(obj, 2, Value::Object(None));
        ctx.set_field(obj, 3, Value::Int(ANTLR_EMPTY_RETURN_STATE));
        obj
    }

    fn make_singleton(
        ctx: &mut MockNativeContext,
        singleton_class: ClassId,
        parent: ObjectRef,
        return_state: i32,
        hash: i32,
    ) -> ObjectRef {
        let obj = ctx.alloc_object(singleton_class, 4);
        ctx.set_field(obj, 1, Value::Int(hash));
        ctx.set_field(obj, 2, Value::Object(Some(parent)));
        ctx.set_field(obj, 3, Value::Int(return_state));
        obj
    }

    fn make_array(
        ctx: &mut MockNativeContext,
        array_class: ClassId,
        pc_class: ClassId,
        parents: &[ObjectRef],
        states: &[i32],
        hash: i32,
    ) -> ObjectRef {
        let parents_arr = ctx.new_ref_array(pc_class, parents.len());
        for (i, parent) in parents.iter().enumerate() {
            ctx.set_array_element(parents_arr, i, Value::Object(Some(*parent)));
        }
        let states_arr = ctx.new_array(ArrayElementType::Int, states.len());
        for (i, state) in states.iter().enumerate() {
            ctx.set_array_element(states_arr, i, Value::Int(*state));
        }
        let obj = ctx.alloc_object(array_class, 4);
        ctx.set_field(obj, 1, Value::Int(hash));
        ctx.set_field(obj, 2, Value::Object(Some(parents_arr)));
        ctx.set_field(obj, 3, Value::Object(Some(states_arr)));
        obj
    }

    fn make_ref_array(ctx: &mut MockNativeContext, values: &[Value], capacity: usize) -> ObjectRef {
        let arr = ctx.new_array(
            ArrayElementType::Reference,
            std::cmp::max(values.len(), capacity),
        );
        for (i, value) in values.iter().enumerate() {
            ctx.set_array_element(arr, i, *value);
        }
        arr
    }

    fn make_arraylist(ctx: &mut MockNativeContext, values: &[Value], capacity: usize) -> ObjectRef {
        let arraylist_class = ctx.ensure_class_initialized("java/util/ArrayList").unwrap();
        let data = make_ref_array(ctx, values, capacity);
        let list = ctx.alloc_object(arraylist_class, 2);
        ctx.set_field(list, 0, Value::Object(Some(data)));
        ctx.set_field(list, 1, Value::Int(values.len() as i32));
        list
    }

    fn install_atn_config_classes(ctx: &mut MockNativeContext) -> (ClassId, ClassId, ClassId) {
        let config = ctx.ensure_class_initialized(ANTLR_ATN_CONFIG).unwrap();
        let config_set = ctx.ensure_class_initialized(ANTLR_ATN_CONFIG_SET).unwrap();
        let semantic_empty = ctx.ensure_class_initialized(ANTLR_SEMANTIC_EMPTY).unwrap();
        let _hash_set = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/ATNConfigSet$ConfigHashSet")
            .unwrap();
        (config, config_set, semantic_empty)
    }

    fn make_semantic_empty(
        ctx: &mut MockNativeContext,
        semantic_empty_class: ClassId,
    ) -> ObjectRef {
        ctx.alloc_object(semantic_empty_class, 0)
    }

    fn make_atn_state(ctx: &mut MockNativeContext, state_number: i32) -> ObjectRef {
        let state_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/BasicState")
            .unwrap();
        let state = ctx.alloc_object(state_class, 2);
        ctx.set_field(state, 1, Value::Int(state_number));
        state
    }

    fn make_atn_config(
        ctx: &mut MockNativeContext,
        config_class: ClassId,
        state: ObjectRef,
        alt: i32,
        context: ObjectRef,
        reaches: i32,
        semantic_context: ObjectRef,
    ) -> ObjectRef {
        let config = ctx.alloc_object(config_class, 5);
        ctx.set_field(config, 0, Value::Object(Some(state)));
        ctx.set_field(config, 1, Value::Int(alt));
        ctx.set_field(config, 2, Value::Object(Some(context)));
        ctx.set_field(config, 3, Value::Int(reaches));
        ctx.set_field(config, 4, Value::Object(Some(semantic_context)));
        config
    }

    fn make_config_lookup(ctx: &mut MockNativeContext) -> ObjectRef {
        let lookup_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/ATNConfigSet$ConfigHashSet")
            .unwrap();
        let buckets = ctx.new_array(ArrayElementType::Reference, 16);
        let lookup = ctx.alloc_object(lookup_class, 7);
        ctx.set_field(lookup, 1, Value::Object(Some(buckets)));
        ctx.set_field(lookup, 2, Value::Int(0));
        ctx.set_field(lookup, 3, Value::Int(1));
        ctx.set_field(lookup, 4, Value::Int(12));
        ctx.set_field(lookup, 5, Value::Int(16));
        ctx.set_field(lookup, 6, Value::Int(2));
        lookup
    }

    fn make_config_set(
        ctx: &mut MockNativeContext,
        config_set_class: ClassId,
        full_ctx: bool,
    ) -> (ObjectRef, ObjectRef, ObjectRef) {
        let lookup = make_config_lookup(ctx);
        let configs = make_arraylist(ctx, &[], 7);
        let set = ctx.alloc_object(config_set_class, 9);
        ctx.set_field(set, 0, Value::Int(0));
        ctx.set_field(set, 1, Value::Object(Some(lookup)));
        ctx.set_field(set, 2, Value::Object(Some(configs)));
        ctx.set_field(set, 7, Value::Int(if full_ctx { 1 } else { 0 }));
        ctx.set_field(set, 8, Value::Int(-1));
        (set, lookup, configs)
    }

    fn arraylist_values(ctx: &mut MockNativeContext, list: ObjectRef) -> Vec<Value> {
        let data = antlr_arraylist_data(ctx, list).unwrap();
        let size = antlr_arraylist_size(ctx, list);
        (0..size).map(|i| ctx.get_array_element(data, i)).collect()
    }

    fn config_lookup_contains(
        ctx: &mut MockNativeContext,
        lookup: ObjectRef,
        config: ObjectRef,
    ) -> bool {
        let buckets = antlr_config_lookup_buckets(ctx, lookup).unwrap();
        for i in 0..ctx.array_length(buckets) {
            let Value::Object(Some(bucket)) = ctx.get_array_element(buckets, i) else {
                continue;
            };
            for j in 0..ctx.array_length(bucket) {
                if ctx.get_array_element(bucket, j) == Value::Object(Some(config)) {
                    return true;
                }
            }
        }
        false
    }

    fn bitset_contains(ctx: &mut MockNativeContext, bitset: ObjectRef, bit: usize) -> bool {
        let words = match ctx.get_field(bitset, 0) {
            Value::Object(Some(words)) => words,
            _ => return false,
        };
        let word_idx = bit / 64;
        if word_idx >= ctx.array_length(words) {
            return false;
        }
        let word = match ctx.get_array_element(words, word_idx) {
            Value::Long(v) => v,
            _ => 0,
        };
        (word & (1i64 << (bit % 64))) != 0
    }

    fn bitset_cardinality(ctx: &mut MockNativeContext, bitset: ObjectRef) -> i32 {
        let words = match ctx.get_field(bitset, 0) {
            Value::Object(Some(words)) => words,
            _ => return 0,
        };
        let words_in_use = match ctx.get_field(bitset, 1) {
            Value::Int(v) if v > 0 => v as usize,
            _ => 0,
        };
        assert!(
            words_in_use <= ctx.array_length(words),
            "BitSet wordsInUse must not exceed backing words length"
        );
        let mut count = 0u32;
        for i in 0..words_in_use {
            if let Value::Long(word) = ctx.get_array_element(words, i) {
                count += (word as u64).count_ones();
            }
        }
        count as i32
    }

    #[test]
    fn antlr_double_key_map_insert_releases_temporary_native_pins() {
        let mut ctx = mock_ctx();
        let map = ctx.fresh_object_ref();
        let data = try_alloc_concurrent_synthetic(&mut ctx, "java/util/HashMap", 3).unwrap();
        cratonvm_native_collections::native_map_init(&mut ctx, &[Value::Object(Some(data))])
            .unwrap();
        ctx.set_field(map, 0, Value::Object(Some(data)));
        let key1 = ctx.fresh_object_ref();
        let key2 = ctx.fresh_object_ref();
        let value = ctx.fresh_object_ref();

        assert_eq!(ctx.native_pin_count_for_test(), 0);
        assert_eq!(
            antlr_double_key_map_put_value(
                &mut ctx,
                map,
                Value::Object(Some(key1)),
                Value::Object(Some(key2)),
                Value::Object(Some(value)),
            )
            .unwrap(),
            Some(Value::Object(None))
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    #[test]
    fn antlr_atn_state_accessors_read_arraylist_transitions() {
        let mut ctx = mock_ctx();
        let atn_state_class = ctx.ensure_class_initialized(ANTLR_ATN_STATE).unwrap();
        let transition_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/Transition")
            .unwrap();
        let first = ctx.alloc_object(transition_class, 0);
        let second = ctx.alloc_object(transition_class, 0);
        let transitions = make_arraylist(
            &mut ctx,
            &[Value::Object(Some(first)), Value::Object(Some(second))],
            4,
        );
        let state = ctx.alloc_object(atn_state_class, 6);
        ctx.set_field(state, 3, Value::Int(1));
        ctx.set_field(state, 4, Value::Object(Some(transitions)));

        assert_eq!(
            native_antlr_atn_state_get_number_of_transitions(
                &mut ctx,
                &[Value::Object(Some(state))]
            )
            .unwrap(),
            Some(Value::Int(2))
        );
        assert_eq!(
            native_antlr_atn_state_transition(
                &mut ctx,
                &[Value::Object(Some(state)), Value::Int(1)]
            )
            .unwrap(),
            Some(Value::Object(Some(second)))
        );
        assert_eq!(
            native_antlr_atn_state_only_has_epsilon_transitions(
                &mut ctx,
                &[Value::Object(Some(state))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
    }

    fn make_interval_set(ctx: &mut MockNativeContext, ranges: &[(i32, i32)]) -> ObjectRef {
        let interval_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/misc/Interval")
            .unwrap();
        let mut intervals = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            let interval = ctx.alloc_object(interval_class, 2);
            ctx.set_field(interval, 0, Value::Int(*start));
            ctx.set_field(interval, 1, Value::Int(*end));
            intervals.push(Value::Object(Some(interval)));
        }
        let interval_set_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/misc/IntervalSet")
            .unwrap();
        let interval_list = make_arraylist(ctx, &intervals, ranges.len());
        let set = ctx.alloc_object(interval_set_class, 2);
        ctx.set_field(set, 0, Value::Object(Some(interval_list)));
        set
    }

    #[test]
    fn antlr_state_transition_and_interval_predicates_are_native() {
        let mut ctx = mock_ctx();
        let rule_stop_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/RuleStopState")
            .unwrap();
        let rule_stop = ctx.alloc_object(rule_stop_class, 6);
        assert_eq!(
            native_antlr_atn_state_get_state_type(&mut ctx, &[Value::Object(Some(rule_stop))])
                .unwrap(),
            Some(Value::Int(7))
        );

        let set = make_interval_set(&mut ctx, &[(1, 3), (10, 12)]);
        assert_eq!(
            native_antlr_interval_set_contains(
                &mut ctx,
                &[Value::Object(Some(set)), Value::Int(2)]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_antlr_interval_set_contains(
                &mut ctx,
                &[Value::Object(Some(set)), Value::Int(7)]
            )
            .unwrap(),
            Some(Value::Int(0))
        );

        let epsilon_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/EpsilonTransition")
            .unwrap();
        let epsilon = ctx.alloc_object(epsilon_class, 2);
        assert_eq!(
            native_antlr_transition_get_serialization_type(
                &mut ctx,
                &[Value::Object(Some(epsilon))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_antlr_transition_is_epsilon(&mut ctx, &[Value::Object(Some(epsilon))]).unwrap(),
            Some(Value::Int(1))
        );

        let range_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/RangeTransition")
            .unwrap();
        let range = ctx.alloc_object(range_class, 3);
        ctx.set_field(range, 1, Value::Int(5));
        ctx.set_field(range, 2, Value::Int(9));
        assert_eq!(
            native_antlr_transition_matches(
                &mut ctx,
                &[
                    Value::Object(Some(range)),
                    Value::Int(6),
                    Value::Int(0),
                    Value::Int(20),
                ],
            )
            .unwrap(),
            Some(Value::Int(1))
        );

        let not_set_class = ctx
            .ensure_class_initialized("org/antlr/v4/runtime/atn/NotSetTransition")
            .unwrap();
        let not_set = ctx.alloc_object(not_set_class, 2);
        ctx.set_field(not_set, 1, Value::Object(Some(set)));
        assert_eq!(
            native_antlr_transition_matches(
                &mut ctx,
                &[
                    Value::Object(Some(not_set)),
                    Value::Int(7),
                    Value::Int(0),
                    Value::Int(20),
                ],
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_antlr_transition_matches(
                &mut ctx,
                &[
                    Value::Object(Some(not_set)),
                    Value::Int(2),
                    Value::Int(0),
                    Value::Int(20),
                ],
            )
            .unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn antlr_empty_prediction_context_equals_identity_only() {
        let mut ctx = mock_ctx();
        let (_, _, empty_class, _) = install_classes(&mut ctx);
        let a = make_empty(&mut ctx, empty_class);
        let b = make_empty(&mut ctx, empty_class);

        assert_eq!(
            native_antlr_prediction_context_equals(
                &mut ctx,
                &[Value::Object(Some(a)), Value::Object(Some(a))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_antlr_prediction_context_equals(
                &mut ctx,
                &[Value::Object(Some(a)), Value::Object(Some(b))]
            )
            .unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn antlr_singleton_prediction_context_equals_structurally() {
        let mut ctx = mock_ctx();
        let (_, singleton_class, empty_class, _) = install_classes(&mut ctx);
        let empty = make_empty(&mut ctx, empty_class);
        let a = make_singleton(&mut ctx, singleton_class, empty, 42, 1234);
        let b = make_singleton(&mut ctx, singleton_class, empty, 42, 1234);
        let c = make_singleton(&mut ctx, singleton_class, empty, 43, 1234);

        assert_eq!(
            native_antlr_prediction_context_equals(
                &mut ctx,
                &[Value::Object(Some(a)), Value::Object(Some(b))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_antlr_prediction_context_equals(
                &mut ctx,
                &[Value::Object(Some(a)), Value::Object(Some(c))]
            )
            .unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn antlr_array_prediction_context_equals_arrays() {
        let mut ctx = mock_ctx();
        let (pc_class, singleton_class, empty_class, array_class) = install_classes(&mut ctx);
        let empty = make_empty(&mut ctx, empty_class);
        let parent = make_singleton(&mut ctx, singleton_class, empty, 7, 77);
        let a = make_array(&mut ctx, array_class, pc_class, &[parent], &[1, 2], 555);
        let b = make_array(&mut ctx, array_class, pc_class, &[parent], &[1, 2], 555);
        let c = make_array(&mut ctx, array_class, pc_class, &[parent], &[1, 3], 555);

        assert_eq!(
            native_antlr_prediction_context_equals(
                &mut ctx,
                &[Value::Object(Some(a)), Value::Object(Some(b))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_antlr_prediction_context_equals(
                &mut ctx,
                &[Value::Object(Some(a)), Value::Object(Some(c))]
            )
            .unwrap(),
            Some(Value::Int(0))
        );
    }

    #[test]
    fn antlr_merge_singletons_with_shared_parent_makes_sorted_array() {
        let mut ctx = mock_ctx();
        let _ = install_classes(&mut ctx);
        let empty = antlr_empty_instance(&mut ctx, ANTLR_NAMES).unwrap();
        let a = antlr_create_singleton_context(&mut ctx, ANTLR_NAMES, Some(empty), 20).unwrap();
        let b = antlr_create_singleton_context(&mut ctx, ANTLR_NAMES, Some(empty), 10).unwrap();

        let merged = antlr_merge_contexts(&mut ctx, a, b, false, None).unwrap();
        assert_eq!(
            antlr_object_kind(&mut ctx, merged),
            AntlrPredictionContextKind::Array
        );
        let states = antlr_array_states(&mut ctx, merged);
        assert_eq!(states, vec![10, 20]);
        let parents = antlr_array_parent_vec(&mut ctx, merged);
        assert_eq!(parents, vec![Some(empty), Some(empty)]);
    }

    #[test]
    fn antlr_merge_arrays_preserves_sorted_states_and_shared_parents() {
        let mut ctx = mock_ctx();
        let _ = install_classes(&mut ctx);
        let empty = antlr_empty_instance(&mut ctx, ANTLR_NAMES).unwrap();
        let a = antlr_create_array_context(
            &mut ctx,
            ANTLR_NAMES,
            &[Some(empty), Some(empty)],
            &[10, 30],
        )
        .unwrap();
        let b = antlr_create_array_context(
            &mut ctx,
            ANTLR_NAMES,
            &[Some(empty), Some(empty)],
            &[20, 30],
        )
        .unwrap();

        let merged = antlr_merge_contexts(&mut ctx, a, b, false, None).unwrap();
        assert_eq!(antlr_array_states(&mut ctx, merged), vec![10, 20, 30]);
        assert_eq!(
            antlr_array_parent_vec(&mut ctx, merged),
            vec![Some(empty), Some(empty), Some(empty)]
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    #[test]
    fn antlr_merge_root_full_context_preserves_empty_path() {
        let mut ctx = mock_ctx();
        let _ = install_classes(&mut ctx);
        let empty = antlr_empty_instance(&mut ctx, ANTLR_NAMES).unwrap();
        let parent = antlr_create_singleton_context(&mut ctx, ANTLR_NAMES, Some(empty), 7).unwrap();
        let node = antlr_create_singleton_context(&mut ctx, ANTLR_NAMES, Some(parent), 42).unwrap();

        let merged = antlr_merge_contexts(&mut ctx, empty, node, false, None).unwrap();
        assert_eq!(
            antlr_object_kind(&mut ctx, merged),
            AntlrPredictionContextKind::Array
        );
        assert_eq!(
            antlr_array_states(&mut ctx, merged),
            vec![42, ANTLR_EMPTY_RETURN_STATE]
        );
        assert_eq!(
            antlr_array_parent_vec(&mut ctx, merged),
            vec![Some(parent), None]
        );
    }

    #[test]
    fn antlr_atn_config_set_add_inserts_config_and_lookup_entry() {
        let mut ctx = mock_ctx();
        let (_, _, empty_class, _) = install_classes(&mut ctx);
        let (config_class, config_set_class, semantic_empty_class) =
            install_atn_config_classes(&mut ctx);
        let (set, lookup, configs) = make_config_set(&mut ctx, config_set_class, true);
        let state = make_atn_state(&mut ctx, 7);
        let context = make_empty(&mut ctx, empty_class);
        let semantic = make_semantic_empty(&mut ctx, semantic_empty_class);
        let config = make_atn_config(&mut ctx, config_class, state, 2, context, 0, semantic);

        assert_eq!(
            native_antlr_atn_config_set_add(
                &mut ctx,
                &[
                    Value::Object(Some(set)),
                    Value::Object(Some(config)),
                    Value::Object(None),
                ],
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            arraylist_values(&mut ctx, configs),
            vec![Value::Object(Some(config))]
        );
        assert!(config_lookup_contains(&mut ctx, lookup, config));
        assert_eq!(antlr_config_lookup_n(&mut ctx, lookup), 1);
    }

    #[test]
    fn antlr_atn_config_set_add_merges_equal_key_contexts() {
        let mut ctx = mock_ctx();
        let _ = install_classes(&mut ctx);
        let (config_class, config_set_class, semantic_empty_class) =
            install_atn_config_classes(&mut ctx);
        let (set, lookup, configs) = make_config_set(&mut ctx, config_set_class, true);
        let state = make_atn_state(&mut ctx, 11);
        let semantic = make_semantic_empty(&mut ctx, semantic_empty_class);
        let empty = antlr_empty_instance(&mut ctx, ANTLR_NAMES).unwrap();
        let context_a =
            antlr_create_singleton_context(&mut ctx, ANTLR_NAMES, Some(empty), 10).unwrap();
        let context_b =
            antlr_create_singleton_context(&mut ctx, ANTLR_NAMES, Some(empty), 20).unwrap();
        let config_a = make_atn_config(&mut ctx, config_class, state, 3, context_a, 1, semantic);
        let config_b = make_atn_config(
            &mut ctx,
            config_class,
            state,
            3,
            context_b,
            2 | ANTLR_SUPPRESS_PRECEDENCE_FILTER,
            semantic,
        );

        native_antlr_atn_config_set_add(
            &mut ctx,
            &[
                Value::Object(Some(set)),
                Value::Object(Some(config_a)),
                Value::Object(None),
            ],
        )
        .unwrap();
        native_antlr_atn_config_set_add(
            &mut ctx,
            &[
                Value::Object(Some(set)),
                Value::Object(Some(config_b)),
                Value::Object(None),
            ],
        )
        .unwrap();

        assert_eq!(
            arraylist_values(&mut ctx, configs),
            vec![Value::Object(Some(config_a))]
        );
        assert!(config_lookup_contains(&mut ctx, lookup, config_a));
        assert_eq!(antlr_config_lookup_n(&mut ctx, lookup), 1);
        assert_eq!(
            antlr_atn_config_reaches(&mut ctx, config_a) & ANTLR_SUPPRESS_PRECEDENCE_FILTER,
            ANTLR_SUPPRESS_PRECEDENCE_FILTER
        );
        let merged = antlr_atn_config_context(&mut ctx, config_a).unwrap();
        assert_eq!(
            antlr_object_kind(&mut ctx, merged),
            AntlrPredictionContextKind::Array
        );
        assert_eq!(antlr_array_states(&mut ctx, merged), vec![10, 20]);
    }

    #[test]
    fn antlr_prediction_mode_get_conflicting_alt_subsets_groups_by_state_and_context() {
        let mut ctx = mock_ctx();
        let (_, _, empty_class, _) = install_classes(&mut ctx);
        let (config_class, config_set_class, semantic_empty_class) =
            install_atn_config_classes(&mut ctx);
        let (set, _, configs) = make_config_set(&mut ctx, config_set_class, true);
        let state_a = make_atn_state(&mut ctx, 21);
        let state_b = make_atn_state(&mut ctx, 22);
        let context = make_empty(&mut ctx, empty_class);
        let semantic = make_semantic_empty(&mut ctx, semantic_empty_class);
        let config_a = make_atn_config(&mut ctx, config_class, state_a, 1, context, 0, semantic);
        let config_b = make_atn_config(&mut ctx, config_class, state_a, 2, context, 0, semantic);
        let config_c = make_atn_config(&mut ctx, config_class, state_b, 3, context, 0, semantic);
        for config in [config_a, config_b, config_c] {
            antlr_arraylist_append(&mut ctx, configs, Value::Object(Some(config))).unwrap();
        }

        let result = native_antlr_prediction_mode_get_conflicting_alt_subsets(
            &mut ctx,
            &[Value::Object(Some(set))],
        )
        .unwrap();
        let list = match result {
            Some(Value::Object(Some(list))) => list,
            other => panic!("expected ArrayList result, got {other:?}"),
        };
        let groups = arraylist_values(&mut ctx, list);
        assert_eq!(groups.len(), 2);
        let group0 = match groups[0] {
            Value::Object(Some(bitset)) => bitset,
            other => panic!("expected BitSet, got {other:?}"),
        };
        let group1 = match groups[1] {
            Value::Object(Some(bitset)) => bitset,
            other => panic!("expected BitSet, got {other:?}"),
        };
        assert_eq!(bitset_cardinality(&mut ctx, group0), 2);
        assert!(bitset_contains(&mut ctx, group0, 1));
        assert!(bitset_contains(&mut ctx, group0, 2));
        assert_eq!(bitset_cardinality(&mut ctx, group1), 1);
        assert!(bitset_contains(&mut ctx, group1, 3));
    }

    #[test]
    fn antlr_prediction_mode_has_state_associated_with_one_alt_tracks_distinct_alts() {
        let mut ctx = mock_ctx();
        let (_, _, empty_class, _) = install_classes(&mut ctx);
        let (config_class, config_set_class, semantic_empty_class) =
            install_atn_config_classes(&mut ctx);
        let (set, _, configs) = make_config_set(&mut ctx, config_set_class, true);
        let state_a = make_atn_state(&mut ctx, 31);
        let context = make_empty(&mut ctx, empty_class);
        let semantic = make_semantic_empty(&mut ctx, semantic_empty_class);
        let config_a = make_atn_config(&mut ctx, config_class, state_a, 1, context, 0, semantic);
        let config_b = make_atn_config(&mut ctx, config_class, state_a, 2, context, 0, semantic);
        for config in [config_a, config_b] {
            antlr_arraylist_append(&mut ctx, configs, Value::Object(Some(config))).unwrap();
        }

        assert_eq!(
            native_antlr_prediction_mode_has_state_associated_with_one_alt(
                &mut ctx,
                &[Value::Object(Some(set))]
            )
            .unwrap(),
            Some(Value::Int(0))
        );

        let state_b = make_atn_state(&mut ctx, 32);
        let config_c = make_atn_config(&mut ctx, config_class, state_b, 3, context, 0, semantic);
        antlr_arraylist_append(&mut ctx, configs, Value::Object(Some(config_c))).unwrap();

        assert_eq!(
            native_antlr_prediction_mode_has_state_associated_with_one_alt(
                &mut ctx,
                &[Value::Object(Some(set))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
    }

    #[test]
    fn antlr_prediction_context_intrinsics_are_registered() {
        let mut registry = NativeMethodRegistry::new();
        register_antlr_prediction_context_intrinsics(&mut registry);
        register_antlr_token_intrinsics(&mut registry);

        assert!(registry.find(ANTLR_PC, "hashCode", "()I").is_some());
        assert!(registry
            .find(
                ANTLR_SINGLETON_PC,
                "getParent",
                "(I)Lorg/antlr/v4/runtime/atn/PredictionContext;",
            )
            .is_some());
        assert!(registry
            .find(
                GROOVY_ANTLR_SINGLETON_PC,
                "getParent",
                "(I)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_PC,
                "merge",
                "(Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/PredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_DOUBLE_KEY_MAP,
                "get",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            )
            .is_some());
        assert!(registry
            .find(ANTLR_ATN_STATE, "getNumberOfTransitions", "()I")
            .is_some());
        assert!(registry
            .find(ANTLR_ATN_STATE, "onlyHasEpsilonTransitions", "()Z")
            .is_some());
        assert!(registry
            .find(
                ANTLR_ATN_STATE,
                "transition",
                "(I)Lorg/antlr/v4/runtime/atn/Transition;",
            )
            .is_some());
        assert!(registry
            .find("org/antlr/v4/runtime/atn/BasicState", "getStateType", "()I",)
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/EpsilonTransition",
                "getSerializationType",
                "()I",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/RangeTransition",
                "matches",
                "(III)Z",
            )
            .is_some());
        assert!(registry
            .find("org/antlr/v4/runtime/misc/IntervalSet", "contains", "(I)Z")
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/ParserATNSimulator",
                "canDropLoopEntryEdgeInLeftRecursiveRule",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/DefaultErrorStrategy",
                "sync",
                "(Lorg/antlr/v4/runtime/Parser;)V",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/ParserATNSimulator",
                "getEpsilonTarget",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/Transition;ZZZZ)Lorg/antlr/v4/runtime/atn/ATNConfig;",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/ParserATNSimulator",
                "computeReachSet",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;IZ)Lorg/antlr/v4/runtime/atn/ATNConfigSet;",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/ParserATNSimulator",
                "closureCheckingStopState",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/ParserATNSimulator",
                "closure",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZZ)V",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/ParserATNSimulator",
                "closure_",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_ATN_CONFIG_SET,
                "add",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/misc/DoubleKeyMap;)Z",
            )
            .is_some());
        assert!(registry
            .find(ANTLR_ATN_CONFIG_SET, "hashCode", "()I")
            .is_some());
        assert!(registry
            .find(
                ANTLR_ATN_CONFIG,
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;)V",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_ATN_CONFIG,
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/PredictionContext;)V",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_ATN_CONFIG,
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/SemanticContext;)V",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_ATN_CONFIG,
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)V",
            )
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/OrderedATNConfigSet",
                "equals",
                "(Ljava/lang/Object;)Z",
            )
            .is_some());
        assert!(registry
            .find("org/antlr/v4/runtime/atn/LexerATNConfig", "hashCode", "()I")
            .is_some());
        assert!(registry
            .find("org/antlr/v4/runtime/dfa/DFAState", "hashCode", "()I")
            .is_some());
        assert!(registry
            .find(
                "org/antlr/v4/runtime/atn/SemanticContext$Predicate",
                "equals",
                "(Ljava/lang/Object;)Z",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_SEMANTIC_CONTEXT,
                "and",
                "(Lorg/antlr/v4/runtime/atn/SemanticContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)Lorg/antlr/v4/runtime/atn/SemanticContext;",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_PREDICTION_MODE,
                "getConflictingAltSubsets",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_PREDICTION_MODE,
                "hasStateAssociatedWithOneAlt",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Z",
            )
            .is_some());
        assert!(registry
            .find(
                ANTLR_COMMON_TOKEN_FACTORY,
                "create",
                "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/Token;",
            )
            .is_some());
        assert!(registry
            .find(ANTLR_COMMON_TOKEN, "getType", "()I")
            .is_some());
    }

    #[test]
    fn hibernate_models_annotation_intrinsics_are_registered_for_descriptors() {
        let mut registry = NativeMethodRegistry::new();
        register_hibernate_models_intrinsics(&mut registry);

        let target = "org/hibernate/models/internal/AnnotationTargetSupport";
        let descriptor = "org/hibernate/models/internal/OrmAnnotationDescriptor";
        for owner in [target, descriptor] {
            assert!(registry
                .find(
                    owner,
                    "getDirectAnnotationUsage",
                    "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
                )
                .is_some());
            assert!(registry
                .find(
                    owner,
                    "locateAnnotationUsage",
                    "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
                )
                .is_some());
        }
        assert!(registry
            .find(
                HIBERNATE_ANNOTATION_USAGE_HELPER,
                "getUsage",
                "(Ljava/lang/Class;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;",
            )
            .is_some());
        assert!(registry
            .find(
                HIBERNATE_ANNOTATION_DESCRIPTOR_REGISTRY_STANDARD,
                "getDescriptor",
                "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
            )
            .is_some());
        assert!(registry
            .find(HIBERNATE_ASSOCIATION_KEY, "hashCode", "()I")
            .is_some());
        assert!(registry
            .find(HIBERNATE_ASSOCIATION_KEY, "equals", "(Ljava/lang/Object;)Z",)
            .is_some());
        assert!(registry
            .find(HIBERNATE_NAVIGABLE_PATH, "equals", "(Ljava/lang/Object;)Z")
            .is_some());
        assert!(registry
            .find(
                HIBERNATE_NAVIGABLE_PATH,
                "getParent",
                "()Lorg/hibernate/spi/NavigablePath;"
            )
            .is_some());
        assert!(registry
            .find(
                HIBERNATE_NAVIGABLE_PATH,
                "getRealParent",
                "()Lorg/hibernate/spi/NavigablePath;"
            )
            .is_some());
        assert!(registry
            .find(
                HIBERNATE_IMMUTABLE_ATTRIBUTE_MAPPING_LIST,
                "indexedForEach",
                "(Lorg/hibernate/internal/util/IndexedConsumer;)V",
            )
            .is_some());
        assert!(registry
            .find(
                HIBERNATE_BASIC_VALUED_MODEL_PART,
                "forEachSelectable",
                "(Lorg/hibernate/metamodel/mapping/SelectableConsumer;)I",
            )
            .is_some());
        assert!(registry
            .find(
                HIBERNATE_BASIC_VALUED_MODEL_PART,
                "forEachSelectable",
                "(ILorg/hibernate/metamodel/mapping/SelectableConsumer;)I",
            )
            .is_some());
    }

    #[test]
    fn bytebuddy_method_list_intrinsics_are_registered_for_descriptors() {
        let mut registry = NativeMethodRegistry::new();
        register_bytebuddy_method_token_intrinsics(&mut registry);

        assert!(registry
            .find(BYTEBUDDY_METHOD_TYPE_TOKEN, "hashCode", "()I")
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_METHOD_DESCRIPTION_TYPE_SUBSTITUTING,
                "<init>",
                "(Lnet/bytebuddy/description/type/TypeDescription$Generic;Lnet/bytebuddy/description/method/MethodDescription;Lnet/bytebuddy/description/type/TypeDescription$Generic$Visitor;)V",
            )
            .is_some());
        assert!(registry
            .find(BYTEBUDDY_METHOD_LIST_TYPE_SUBSTITUTING, "size", "()I",)
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_METHOD_LIST_TYPE_SUBSTITUTING,
                "get",
                "(I)Ljava/lang/Object;",
            )
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_METHOD_LIST_FOR_LOADED_METHODS,
                "get",
                "(I)Lnet/bytebuddy/description/method/MethodDescription$InDefinedShape;",
            )
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_METHOD_LIST_FOR_TOKENS,
                "get",
                "(I)Lnet/bytebuddy/description/method/MethodDescription$InDefinedShape;",
            )
            .is_some());
        assert!(registry
            .find(BYTEBUDDY_FIELD_LIST_FOR_TOKENS, "size", "()I")
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_FIELD_LIST_FOR_LOADED_FIELDS,
                "get",
                "(I)Lnet/bytebuddy/description/field/FieldDescription$InDefinedShape;",
            )
            .is_some());
        assert!(registry
            .find(BYTEBUDDY_TYPE_LIST_EXPLICIT, "size", "()I")
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_TYPE_LIST_GENERIC_EXPLICIT,
                "get",
                "(I)Lnet/bytebuddy/description/type/TypeDescription$Generic;",
            )
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_METHOD_GRAPH_FOR_JAVA_METHOD_TOKEN,
                "equals",
                "(Ljava/lang/Object;)Z",
            )
            .is_some());
        assert!(registry
            .find(BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY, "hashCode", "()I")
            .is_some());
        assert!(registry
            .find(
                BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY,
                "equals",
                "(Ljava/lang/Object;)Z",
            )
            .is_some());
    }

    /// `MockMethodAdvice.isOverridden` is a genuine semantic bridge and stays
    /// registered unconditionally.
    #[test]
    fn mockito_is_overridden_intrinsic_is_always_registered() {
        let mut registry = NativeMethodRegistry::new();
        register_mockito_debugging_intrinsics(&mut registry);

        assert!(registry
            .find(
                MOCKITO_MOCK_METHOD_ADVICE,
                "isOverridden",
                "(Ljava/lang/Object;Ljava/lang/reflect/Method;)Z",
            )
            .is_some());
    }

    /// The `Location` / `MemberAccessor` *selector* overrides forced Mockito
    /// onto its fallback implementations on every run, diverging from HotSpot
    /// (`LocationImpl` / `InstrumentationMemberAccessor`) and erasing the call
    /// site from every Mockito diagnostic. They must NOT be registered unless
    /// `CRATONVM_MOCKITO_LEGACY_SELECTORS` opts back in — which this test
    /// process does not, since the flag is latched from the environment.
    #[test]
    fn mockito_selector_overrides_are_off_by_default() {
        if cratonvm_types::flags::mockito_legacy_selectors() {
            return; // explicit opt-in in this environment; nothing to assert
        }
        let mut registry = NativeMethodRegistry::new();
        register_mockito_debugging_intrinsics(&mut registry);

        assert!(registry
            .find(
                MOCKITO_LOCATION_FACTORY,
                "create",
                "()Lorg/mockito/invocation/Location;",
            )
            .is_none());
        assert!(registry
            .find(
                MOCKITO_LOCATION_FACTORY,
                "create",
                "(Z)Lorg/mockito/invocation/Location;",
            )
            .is_none());
        assert!(registry
            .find(
                MOCKITO_LOCATION_FACTORY_DEFAULT,
                "create",
                "(Z)Lorg/mockito/invocation/Location;",
            )
            .is_none());
        assert!(registry
            .find(
                MOCKITO_MODULE_MEMBER_ACCESSOR,
                "delegate",
                "()Lorg/mockito/plugins/MemberAccessor;",
            )
            .is_none());
    }
}
