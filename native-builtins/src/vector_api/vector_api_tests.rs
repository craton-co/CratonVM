use super::*;
use cratonvm_native_api::test_mock::MockNativeContext;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_vector_api_natives(&mut r);
    r
}

// --- VectorSpeciesConfig tests ---

#[test]
fn test_species_config_int_256() {
    let cfg = VectorSpeciesConfig::new(ELEM_INT, 256);
    assert_eq!(cfg.element_type, ELEM_INT);
    assert_eq!(cfg.bit_size, 256);
    assert_eq!(cfg.lane_count, 8);
}

#[test]
fn test_species_config_long_256() {
    let cfg = VectorSpeciesConfig::new(ELEM_LONG, 256);
    assert_eq!(cfg.lane_count, 4);
}

#[test]
fn test_species_config_float_128() {
    let cfg = VectorSpeciesConfig::new(ELEM_FLOAT, 128);
    assert_eq!(cfg.lane_count, 4);
}

#[test]
fn test_species_config_double_512() {
    let cfg = VectorSpeciesConfig::new(ELEM_DOUBLE, 512);
    assert_eq!(cfg.lane_count, 8);
}

#[test]
fn test_species_config_byte_64() {
    let cfg = VectorSpeciesConfig::new(ELEM_BYTE, 64);
    assert_eq!(cfg.lane_count, 8);
}

#[test]
fn test_species_config_short_128() {
    let cfg = VectorSpeciesConfig::new(ELEM_SHORT, 128);
    assert_eq!(cfg.lane_count, 8);
}

#[test]
fn test_species_config_unknown_type() {
    let cfg = VectorSpeciesConfig::new(255, 256);
    assert_eq!(cfg.lane_count, 0);
}

#[test]
fn test_element_size_bits_all_types() {
    assert_eq!(element_size_bits(ELEM_BYTE), 8);
    assert_eq!(element_size_bits(ELEM_SHORT), 16);
    assert_eq!(element_size_bits(ELEM_INT), 32);
    assert_eq!(element_size_bits(ELEM_LONG), 64);
    assert_eq!(element_size_bits(ELEM_FLOAT), 32);
    assert_eq!(element_size_bits(ELEM_DOUBLE), 64);
    assert_eq!(element_size_bits(99), 0);
}

#[test]
fn test_get_species_config_valid_indices() {
    let c0 = get_species_config(0);
    assert_eq!(c0.element_type, ELEM_INT);
    assert_eq!(c0.bit_size, 64);
    assert_eq!(c0.lane_count, 2);

    let c2 = get_species_config(2);
    assert_eq!(c2.element_type, ELEM_INT);
    assert_eq!(c2.bit_size, 256);
    assert_eq!(c2.lane_count, 8);
}

#[test]
fn test_get_species_config_fallback() {
    let cfg = get_species_config(999);
    assert_eq!(cfg.element_type, ELEM_INT);
    assert_eq!(cfg.bit_size, 256);
}

#[test]
fn test_lane_count_from_species() {
    assert_eq!(lane_count_from_species(0), 2); // 64-bit Int = 2 lanes
    assert_eq!(lane_count_from_species(2), 8); // 256-bit Int = 8 lanes
    assert_eq!(lane_count_from_species(3), 16); // 512-bit Int = 16 lanes
}

// --- VectorOperators tests ---

#[test]
fn test_op_name_known() {
    assert_eq!(op_name(OP_NEG), "NEG");
    assert_eq!(op_name(OP_ADD), "ADD");
    assert_eq!(op_name(OP_MUL), "MUL");
    assert_eq!(op_name(OP_FMA), "FMA");
    assert_eq!(op_name(CMP_EQ), "EQ");
    assert_eq!(op_name(CMP_GE), "GE");
}

#[test]
fn test_op_name_unknown() {
    assert_eq!(op_name(999), "UNKNOWN");
}

#[test]
fn test_is_associative_true() {
    assert!(is_associative(OP_ADD));
    assert!(is_associative(OP_MUL));
    assert!(is_associative(OP_AND));
    assert!(is_associative(OP_OR));
    assert!(is_associative(OP_XOR));
    assert!(is_associative(OP_MIN));
    assert!(is_associative(OP_MAX));
}

#[test]
fn test_is_associative_false() {
    assert!(!is_associative(OP_SUB));
    assert!(!is_associative(OP_DIV));
    assert!(!is_associative(OP_NEG));
    assert!(!is_associative(OP_ABS));
    assert!(!is_associative(CMP_EQ));
}

// --- Registration tests ---

#[test]
fn test_vector_support_jdk25_natives_registered() {
    let r = make_registry();
    assert!(r.find(VECTOR_SUPPORT, "registerNatives", "()I").is_some());
    assert!(r
        .find(VECTOR_SUPPORT, "getCPUFeatures", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(VECTOR_SUPPORT, "getMaxLaneCount", "(Ljava/lang/Class;)I")
        .is_some());
}

#[test]
fn test_float_vector_mul_reduce_uses_real_lanes() {
    let mut ctx = MockNativeContext::new();
    let species = alloc_species(&mut ctx, 10).unwrap();
    let lhs = ctx.new_array(ArrayElementType::Float, 8);
    let rhs = ctx.new_array(ArrayElementType::Float, 8);
    for i in 0..8 {
        ctx.set_array_element(lhs, i, Value::Float((i + 1) as f32));
        ctx.set_array_element(rhs, i, Value::Float(0.5));
    }

    let lhs_vec = fv_from_array(
        &mut ctx,
        &[
            Value::Object(Some(species)),
            Value::Object(Some(lhs)),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    let rhs_vec = fv_from_array(
        &mut ctx,
        &[
            Value::Object(Some(species)),
            Value::Object(Some(rhs)),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    let product = fv_mul(&mut ctx, &[lhs_vec, rhs_vec]).unwrap().unwrap();
    let reduced = fv_reduce_lanes(&mut ctx, &[product, Value::Int(RED_ADD)])
        .unwrap()
        .unwrap();

    assert_eq!(reduced.as_float(), Some(18.0));
}

#[test]
fn test_vs_of_int_registered() {
    let r = make_registry();
    assert!(r
        .find(VS, "ofInt", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
}

#[test]
fn test_vs_of_long_registered() {
    let r = make_registry();
    assert!(r
        .find(VS, "ofLong", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
}

#[test]
fn test_vs_of_float_registered() {
    let r = make_registry();
    assert!(r
        .find(VS, "ofFloat", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
}

#[test]
fn test_vs_of_double_registered() {
    let r = make_registry();
    assert!(r
        .find(VS, "ofDouble", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
}

#[test]
fn test_vs_length_registered() {
    let r = make_registry();
    assert!(r.find(VS, "length", "()I").is_some());
}

#[test]
fn test_vs_vector_bit_size_registered() {
    let r = make_registry();
    assert!(r.find(VS, "vectorBitSize", "()I").is_some());
}

#[test]
fn test_iv_zero_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "zero",
            "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/IntVector;"
        )
        .is_some());
}

#[test]
fn test_iv_broadcast_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "broadcast",
            "(Ljdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/IntVector;"
        )
        .is_some());
}

#[test]
fn test_iv_add_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "add",
            "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;"
        )
        .is_some());
}

#[test]
fn test_iv_sub_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "sub",
            "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;"
        )
        .is_some());
}

#[test]
fn test_iv_mul_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "mul",
            "(Ljdk/incubator/vector/IntVector;)Ljdk/incubator/vector/IntVector;"
        )
        .is_some());
}

#[test]
fn test_iv_reduce_lanes_registered() {
    let r = make_registry();
    assert!(r.find(IV, "reduceLanes", "(I)I").is_some());
}

#[test]
fn test_iv_lane_registered() {
    let r = make_registry();
    assert!(r.find(IV, "lane", "(I)I").is_some());
}

#[test]
fn test_iv_with_lane_registered() {
    let r = make_registry();
    assert!(r
        .find(IV, "withLane", "(II)Ljdk/incubator/vector/IntVector;")
        .is_some());
}

#[test]
fn test_iv_blend_registered() {
    let r = make_registry();
    assert!(r.find(IV, "blend", "(Ljdk/incubator/vector/IntVector;Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/IntVector;").is_some());
}

#[test]
fn test_iv_compare_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "compare",
            "(ILjdk/incubator/vector/IntVector;)Ljdk/incubator/vector/VectorMask;"
        )
        .is_some());
}

#[test]
fn test_iv_rearrange_registered() {
    let r = make_registry();
    assert!(r
        .find(
            IV,
            "rearrange",
            "(Ljdk/incubator/vector/VectorShuffle;)Ljdk/incubator/vector/IntVector;"
        )
        .is_some());
}

#[test]
fn test_lv_zero_registered() {
    let r = make_registry();
    assert!(r
        .find(
            LV,
            "zero",
            "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/LongVector;"
        )
        .is_some());
}

#[test]
fn test_lv_add_registered() {
    let r = make_registry();
    assert!(r
        .find(
            LV,
            "add",
            "(Ljdk/incubator/vector/LongVector;)Ljdk/incubator/vector/LongVector;"
        )
        .is_some());
}

#[test]
fn test_lv_reduce_lanes_registered() {
    let r = make_registry();
    assert!(r.find(LV, "reduceLanes", "(I)J").is_some());
}

#[test]
fn test_fv_zero_registered() {
    let r = make_registry();
    assert!(r
        .find(
            FV,
            "zero",
            "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/FloatVector;"
        )
        .is_some());
}

#[test]
fn test_fv_sqrt_registered() {
    let r = make_registry();
    assert!(r
        .find(FV, "sqrt", "()Ljdk/incubator/vector/FloatVector;")
        .is_some());
}

#[test]
fn test_fv_fma_registered() {
    let r = make_registry();
    assert!(r.find(FV, "fma", "(Ljdk/incubator/vector/FloatVector;Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;").is_some());
}

#[test]
fn test_fv_min_max_registered() {
    let r = make_registry();
    assert!(r
        .find(
            FV,
            "min",
            "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;"
        )
        .is_some());
    assert!(r
        .find(
            FV,
            "max",
            "(Ljdk/incubator/vector/FloatVector;)Ljdk/incubator/vector/FloatVector;"
        )
        .is_some());
}

#[test]
fn test_dv_zero_registered() {
    let r = make_registry();
    assert!(r
        .find(
            DV,
            "zero",
            "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/DoubleVector;"
        )
        .is_some());
}

#[test]
fn test_dv_fma_registered() {
    let r = make_registry();
    assert!(r.find(DV, "fma", "(Ljdk/incubator/vector/DoubleVector;Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;").is_some());
}

#[test]
fn test_dv_min_max_registered() {
    let r = make_registry();
    assert!(r
        .find(
            DV,
            "min",
            "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;"
        )
        .is_some());
    assert!(r
        .find(
            DV,
            "max",
            "(Ljdk/incubator/vector/DoubleVector;)Ljdk/incubator/vector/DoubleVector;"
        )
        .is_some());
}

#[test]
fn test_vm_all_true_registered() {
    let r = make_registry();
    assert!(r
        .find(VM, "allTrue", "(I)Ljdk/incubator/vector/VectorMask;")
        .is_some());
}

#[test]
fn test_vm_all_false_registered() {
    let r = make_registry();
    assert!(r
        .find(VM, "allFalse", "(I)Ljdk/incubator/vector/VectorMask;")
        .is_some());
}

#[test]
fn test_vm_and_registered() {
    let r = make_registry();
    assert!(r
        .find(
            VM,
            "and",
            "(Ljdk/incubator/vector/VectorMask;)Ljdk/incubator/vector/VectorMask;"
        )
        .is_some());
}

#[test]
fn test_vm_not_registered() {
    let r = make_registry();
    assert!(r
        .find(VM, "not", "()Ljdk/incubator/vector/VectorMask;")
        .is_some());
}

#[test]
fn test_vsh_iota_registered() {
    let r = make_registry();
    assert!(r
        .find(VSH, "iota", "(I)Ljdk/incubator/vector/VectorShuffle;")
        .is_some());
}

#[test]
fn test_vsh_lane_source_registered() {
    let r = make_registry();
    assert!(r.find(VSH, "laneSource", "(I)I").is_some());
}

#[test]
fn test_vo_op_name_registered() {
    let r = make_registry();
    assert!(r.find(VO, "opName", "(I)Ljava/lang/String;").is_some());
}

#[test]
fn test_vo_is_associative_registered() {
    let r = make_registry();
    assert!(r.find(VO, "isAssociative", "(I)Z").is_some());
}

// --- Species predefined index tests ---

#[test]
fn test_species_long_512_lanes() {
    let cfg = get_species_config(7); // SPECIES_512 Long
    assert_eq!(cfg.element_type, ELEM_LONG);
    assert_eq!(cfg.bit_size, 512);
    assert_eq!(cfg.lane_count, 8);
}

#[test]
fn test_species_float_64_lanes() {
    let cfg = get_species_config(8); // SPECIES_64 Float
    assert_eq!(cfg.element_type, ELEM_FLOAT);
    assert_eq!(cfg.bit_size, 64);
    assert_eq!(cfg.lane_count, 2);
}

#[test]
fn test_species_double_128_lanes() {
    let cfg = get_species_config(13); // SPECIES_128 Double
    assert_eq!(cfg.element_type, ELEM_DOUBLE);
    assert_eq!(cfg.bit_size, 128);
    assert_eq!(cfg.lane_count, 2);
}

#[test]
fn test_species_byte_256_lanes() {
    let cfg = get_species_config(18); // SPECIES_256 Byte
    assert_eq!(cfg.element_type, ELEM_BYTE);
    assert_eq!(cfg.bit_size, 256);
    assert_eq!(cfg.lane_count, 32);
}

#[test]
fn test_species_short_512_lanes() {
    let cfg = get_species_config(23); // SPECIES_512 Short
    assert_eq!(cfg.element_type, ELEM_SHORT);
    assert_eq!(cfg.bit_size, 512);
    assert_eq!(cfg.lane_count, 32);
}

// --- Total registration count ---

#[test]
fn test_total_registration_count() {
    let r = make_registry();
    // Verify a sampling of all categories are present
    // VectorSpecies: 8, IntVector: 25, LongVector: 16, FloatVector: 18,
    // DoubleVector: 18, VectorMask: 9, VectorShuffle: 4, VectorOperators: 2
    // Total = 100
    assert!(r.find(VS, "elementSize", "()I").is_some());
    assert!(r.find(IV, "toArray", "()[I").is_some());
    assert!(r.find(IV, "intoArray", "([II)V").is_some());
    assert!(r
        .find(IV, "species", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
    assert!(r.find(IV, "length", "()I").is_some());
    assert!(r
        .find(
            IV,
            "reinterpretAsLongs",
            "()Ljdk/incubator/vector/LongVector;"
        )
        .is_some());
    assert!(r
        .find(
            LV,
            "broadcast",
            "(Ljdk/incubator/vector/VectorSpecies;J)Ljdk/incubator/vector/LongVector;"
        )
        .is_some());
    assert!(r
        .find(
            FV,
            "broadcast",
            "(Ljdk/incubator/vector/VectorSpecies;F)Ljdk/incubator/vector/FloatVector;"
        )
        .is_some());
    assert!(r
        .find(
            DV,
            "broadcast",
            "(Ljdk/incubator/vector/VectorSpecies;D)Ljdk/incubator/vector/DoubleVector;"
        )
        .is_some());
    assert!(r
        .find(VM, "fromValues", "(II)Ljdk/incubator/vector/VectorMask;")
        .is_some());
    assert!(r
        .find(
            VSH,
            "fromValues",
            "(II)Ljdk/incubator/vector/VectorShuffle;"
        )
        .is_some());
}
