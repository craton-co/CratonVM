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
    let source = concat!(
        include_str!("../antlr_intrinsics.rs"),
        "\n",
        include_str!("rooted_value.rs")
    );
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
    (word & (1i64 << (bit % 64))) != 0    }

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
