use super::*;
use crate::config::VmConfig;

/// The ABI capacity every door shares: the context pointer takes a slot.
#[test]
fn the_jit_abi_capacity_counts_the_context_pointer() {
    assert_eq!(jit_abi_max_java_params(false), JIT_ABI_MAX_JAVA_ARGS);
    assert_eq!(jit_abi_max_java_params(true), JIT_ABI_MAX_JAVA_ARGS - 1);
}

/// Argument encoding: raw bits, no NaN-box tag, `int` sign-extended.
#[test]
fn arguments_are_encoded_as_raw_jit_abi_bits() {
    assert_eq!(jit_abi_arg_bits(&Value::Int(-1)), -1i64);
    assert_eq!(jit_abi_arg_bits(&Value::Int(11)), 11);
    assert_eq!(jit_abi_arg_bits(&Value::Long(i64::MIN)), i64::MIN);
    assert_eq!(
        jit_abi_arg_bits(&Value::Float(-0.0)),
        i64::from((-0.0f32).to_bits())
    );
    assert_eq!(
        jit_abi_arg_bits(&Value::Double(f64::NAN)),
        f64::NAN.to_bits() as i64
    );
    assert_eq!(jit_abi_arg_bits(&Value::Object(None)), 0);
}

/// Interpreter round i7 wave 136, lane L2: the per-argument descriptor
/// tags the debug assertion walks (receiver first, a `long` / `double`
/// one argument, an array one `[`), and the assertion's predicate: only a
/// non-null reference in a primitive slot disagrees.
#[test]
fn the_door_argument_tags_and_the_misplaced_reference_predicate() {
    assert_eq!(
        jit_abi_arg_tags("(IJ[[Ljava/lang/String;DLjava/lang/Object;Z)V", false),
        b"LIJ[DLZ".to_vec()
    );
    assert_eq!(jit_abi_arg_tags("()I", true), Vec::<u8>::new());
    assert_eq!(jit_abi_arg_tags("([I)V", true), b"[".to_vec());
    let mut word = [0u64; 4];
    // SAFETY: a test-local word never dereferenced through the reference.
    let obj = unsafe { ObjectRef::from_raw(word.as_mut_ptr() as *mut u8) };
    assert!(!jit_abi_arg_kind_agrees(&Value::Object(Some(obj)), b'I'));
    assert!(!jit_abi_arg_kind_agrees(&Value::Object(Some(obj)), b'F'));
    assert!(jit_abi_arg_kind_agrees(&Value::Object(Some(obj)), b'L'));
    assert!(jit_abi_arg_kind_agrees(&Value::Object(Some(obj)), b'['));
    assert!(jit_abi_arg_kind_agrees(&Value::Object(None), b'I'));
    assert!(jit_abi_arg_kind_agrees(&Value::Int(3), b'J'));
    assert!(jit_abi_arg_kind_agrees(&Value::Int(3), b'L'));
}

/// Return conversion: the `int` family narrowed by the descriptor tag
/// (JVMS §6.5 `ireturn`), `void` pushes nothing, `null` stays `null`.
#[test]
fn returns_are_converted_and_narrowed_by_the_descriptor_tag() {
    assert!(matches!(jit_return_value(b'Z', 2), Some(Value::Int(0))));
    assert!(matches!(jit_return_value(b'Z', 3), Some(Value::Int(1))));
    assert!(matches!(jit_return_value(b'B', 0xff), Some(Value::Int(-1))));
    assert!(matches!(
        jit_return_value(b'C', -1),
        Some(Value::Int(0xffff))
    ));
    assert!(matches!(
        jit_return_value(b'S', 0x1_8000),
        Some(Value::Int(-32768))
    ));
    assert!(matches!(
        jit_return_value(b'I', 1 << 32),
        Some(Value::Int(0))
    ));
    assert!(matches!(
        jit_return_value(b'J', i64::MIN),
        Some(Value::Long(i64::MIN))
    ));
    assert!(matches!(
        jit_return_value(b'L', 0),
        Some(Value::Object(None))
    ));
    assert!(matches!(
        jit_return_value(b'[', 0),
        Some(Value::Object(None))
    ));
    assert!(jit_return_value(b'V', 12345).is_none());
    match jit_return_value(b'D', 1.5f64.to_bits() as i64) {
        Some(Value::Double(d)) => assert_eq!(d, 1.5),
        other => panic!("expected a double, got {other:?}"),
    }
}

/// The one tier-up stride every dispatch door now shares.
#[test]
fn tier_up_is_offered_at_the_threshold_and_then_every_stride() {
    let t = 500;
    assert!(!should_offer_tier_up(t - 1, t));
    assert!(should_offer_tier_up(t, t));
    assert!(!should_offer_tier_up(t + 1, t));
    assert!(should_offer_tier_up(t + JIT_RETRY_STRIDE, t));
    assert!(should_offer_tier_up(t + 3 * JIT_RETRY_STRIDE, t));
    assert!(!should_offer_tier_up(t + JIT_RETRY_STRIDE - 1, t));
    assert!(should_offer_tier_up(0, 0));
}

/// A panic becomes a Java exception only when its message is HEADED by the
/// exception's name; one that merely mentions it stays a VM defect.
#[test]
fn only_a_panic_headed_by_an_exception_name_maps_to_that_exception() {
    assert!(panic_names_java_exception(
        "ArrayIndexOutOfBoundsException: index 7 out of bounds for length 5",
        "ArrayIndexOutOfBoundsException"
    ));
    assert!(panic_names_java_exception(
        "NullPointerException",
        "NullPointerException"
    ));
    assert!(!panic_names_java_exception(
        "assertion failed: saw a NullPointerException state",
        "NullPointerException"
    ));
    assert!(!panic_names_java_exception(
        "StackOverflowish",
        "StackOverflowError"
    ));
    assert!(!panic_names_java_exception(
        "NullPointerExceptionHandler missing",
        "NullPointerException"
    ));
}

/// The negative-cache slot records WHICH callee and at WHICH `JitCache`
/// generation: a publication moves the generation, so the lock-free hit
/// stops matching, while the callee identity still does.
#[test]
fn a_negative_cache_stamp_carries_the_generation_it_was_verified_at() {
    let fp = callee_neg_fingerprint("a/B", "m", "()V");
    let other = callee_neg_fingerprint("a/B", "m", "(I)V");
    let at_7 = callee_neg_stamp(fp, 7);
    assert_eq!(at_7, callee_neg_stamp(fp, 7));
    assert_ne!(
        at_7,
        callee_neg_stamp(fp, 8),
        "a publication must miss the fast path"
    );
    assert!(callee_neg_names(at_7, fp));
    assert!(callee_neg_names(callee_neg_stamp(fp, 8), fp));
    assert!(!callee_neg_names(0, fp), "an empty slot names nothing");
    if (fp & !CALLEE_NEG_GEN_MASK) != (other & !CALLEE_NEG_GEN_MASK) {
        assert!(!callee_neg_names(at_7, other));
    }
}

/// The bridge's memos are per VM: a verdict recorded in one VM is not seen
/// by another (they were process-global statics keyed by `ClassId`s, which
/// are only unique within one VM).
#[test]
fn the_optimizing_osr_refusal_memo_is_per_vm() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let key = (42, 0xfeed, 7);
    note_osr_optimizing_refusal(&a, key, a.jit.jit_cache.redefine_epoch());
    if !osr_optimizing_memo_disabled() {
        assert!(osr_optimizing_already_refused(&a, key));
    }
    assert!(!osr_optimizing_already_refused(&b, key));
}
