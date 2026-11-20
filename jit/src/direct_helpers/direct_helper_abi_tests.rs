use super::*;

/// Two rows naming the same slot would make [`direct_helper_sig`] answer
/// with whichever came first, so an assertion at a call site could be
/// silently checking the wrong helper's arity.
#[test]
fn every_signature_row_names_a_distinct_slot() {
    let mut seen: Vec<&str> = Vec::new();
    for sig in DIRECT_HELPER_FN_SIGS {
        assert!(
            !seen.contains(&sig.field),
            "two `direct_helper_fn_slots!` rows name the slot `{}`",
            sig.field,
        );
        seen.push(sig.field);
    }
}

/// The six hand-written slots are the ones with no stack-argument path
/// anywhere, so the set is worth naming rather than only counting: a
/// seventh member would be a helper the emitter calls with registers it
/// never loads, and the census assertion alone cannot say which one is new.
/// (`field_site_cp` joined in interpreter round i4 wave 92, lane L2.)
#[test]
fn the_hand_written_call_path_has_exactly_the_six_known_members() {
    let mut hand: Vec<&str> = DIRECT_HELPER_FN_SIGS
        .iter()
        .filter(|s| matches!(s.path, DirectHelperCallPath::HandWritten))
        .map(|s| s.field)
        .collect();
    hand.sort_unstable();
    assert_eq!(
        hand,
        [
            "arm_savebase_watch",
            "disarm_savebase_watch",
            "field_site_cp",
            "monitor_enter",
            "monitor_exit",
            "reflect_method_invoke_sentinel",
            "sb_grow",
        ],
        "the set of direct helpers reached by a hand-written call sequence \
         changed. Every member must fit Win64's four integer argument \
         registers, because that route has no shadow-space stack-argument \
         marshalling at all.",
    );
}

/// The nine slots that overflow the Win64 register file must ALL be on the
/// marshalled path. The count assertion beside the table would be
/// satisfied by trading a marshalled overflow for a hand-written one; this
/// is the check that would not be.
#[test]
fn every_slot_that_overflows_the_register_file_is_marshalled() {
    for sig in DIRECT_HELPER_FN_SIGS {
        if sig.win64_stack_args() > 0 {
            assert!(
                matches!(sig.path, DirectHelperCallPath::JavaArgMarshalled),
                "`{}` takes {} arguments but is not reached through \
                 JitDirectCall's `emit_stack_arg_setup`",
                sig.field,
                sig.arity,
            );
        }
    }
}

/// Every slot reached through the `JitDirectCall` sequence returns a word.
///
/// That sequence (`x64/op_invoke.rs`, both the invokestatic and the
/// instance arm) ends in `emit_post_invoke_exception_check(ret_type)`,
/// which compares RAX against `i64::MIN` for a `V` site too. A `-> ()`
/// helper leaves RAX undefined there, and one whose cold arm dispatches a
/// native that throws loses the exception: `varhandle_write` did exactly
/// that, and a `ClassCastException` from the native escaped the compiled
/// caller's own `catch`
/// (r11w7-orch-jit-varhandle-native-exception-escapes-the-local-handler).
#[test]
fn every_java_arg_marshalled_slot_returns_a_word() {
    for sig in DIRECT_HELPER_FN_SIGS {
        if matches!(sig.path, DirectHelperCallPath::JavaArgMarshalled) {
            assert!(
                sig.returns_value,
                "`{}` is reached through JitDirectCall but is declared `-> ()`; \
                 the post-call sentinel compare would read an undefined RAX",
                sig.field,
            );
        }
    }
    assert!(direct_helper_sig_of("varhandle_write").returns_value);
    assert!(direct_helper_sig_of("reachability_fence").returns_value);
}

/// `varhandle_cas` is the slot the review nominated for the Win64
/// stack-argument census, so pin what the answer actually is: five words,
/// one over the register file, and legal precisely because it is NOT on
/// the hand-written route. Moving it there without adding marshalling
/// would pass an uninitialised fifth argument on Windows.
#[test]
fn varhandle_cas_overflows_win64_and_is_marshalled_not_hand_written() {
    let sig = direct_helper_sig_of("varhandle_cas");
    assert_eq!(sig.arity, 5);
    assert_eq!(sig.win64_stack_args(), 1);
    assert_eq!(sig.sysv_stack_args(), 0);
    assert_eq!(sig.path, DirectHelperCallPath::JavaArgMarshalled);
}

/// The `ScopedMemoryAccess` write accessors take seven words and so spill
/// on SysV too — the only slots in either helper table that do. Nothing
/// said so anywhere before this.
#[test]
fn the_scoped_memory_write_accessors_spill_on_both_abis() {
    for field in [
        "scoped_memory_put_int",
        "scoped_memory_put_short",
        "scoped_memory_put_long",
        "scoped_memory_put_char",
    ] {
        let sig = direct_helper_sig_of(field);
        assert_eq!(sig.arity, 7, "{field}");
        assert_eq!(sig.sysv_stack_args(), 1, "{field}");
        assert_eq!(sig.win64_stack_args(), 3, "{field}");
    }
}

/// `monitor_exit` returns `1`, not the object — which is why a shared
/// enter/exit emitter must not store RAX back. Same declared shape as
/// `monitor_enter`, which is what makes sharing the emitter legal.
#[test]
fn the_two_monitor_helpers_share_one_declared_shape() {
    let enter = direct_helper_sig_of("monitor_enter");
    let exit = direct_helper_sig_of("monitor_exit");
    assert_eq!(
        (enter.arity, enter.returns_value, enter.path),
        (exit.arity, exit.returns_value, exit.path),
        "`runtime_lowering::emit_monitor_stub` emits ONE sequence for both; \
         a divergence here makes one of the two call sites wrong",
    );
}

/// An unknown field name must fail the build, not quietly assert nothing.
/// The `const fn` half of that cannot be tested (it would not compile), so
/// this pins the `None` arm it is built on.
#[test]
fn an_unknown_slot_name_resolves_to_nothing() {
    assert!(direct_helper_sig("monitor_entre").is_none());
    assert!(direct_helper_sig("").is_none());
    assert!(direct_helper_sig("monitor_enter").is_some());
}

/// The compiler-only predicates are deliberately absent from the signature
/// table: their arguments are a `u32` and a `bool`, which the
/// machine-word rule would (correctly) reject. Their convention is pinned
/// by the `extern "C"` aliases the transmute sites name instead.
#[test]
fn the_compiler_only_predicates_are_not_baked_call_targets() {
    for field in [
        "buffer_session_served_class",
        "nio_byte_element_served_class",
        "static_base_resolver",
        "class_initialized",
        "well_known_class_id",
    ] {
        assert!(
            direct_helper_sig(field).is_none(),
            "{field} must not be described as a baked callable",
        );
    }
}

/// `integer_class_id` answers only with BOTH the resolver and the VM
/// context wired, passes the `Integer` selector, and never answers class
/// id 0 (`java/lang/Object`) or a value outside `u32`: each of those would
/// be a receiver guard that some non-`Integer` object passes.
#[test]
fn the_integer_class_id_needs_the_resolver_the_vm_and_a_real_id() {
    unsafe extern "C" fn answers_42(ctx: i64, which: i64) -> i64 {
        if ctx == 0x1000 && which == WELL_KNOWN_CLASS_INTEGER {
            42
        } else {
            0
        }
    }
    unsafe extern "C" fn answers_zero(_ctx: i64, _which: i64) -> i64 {
        0
    }
    unsafe extern "C" fn answers_too_wide(_ctx: i64, _which: i64) -> i64 {
        i64::from(u32::MAX) + 1
    }
    let wired = DirectHelperTable {
        well_known_class_id: answers_42 as *const () as usize,
        static_base_resolver_ctx: 0x1000,
        ..DirectHelperTable::EMPTY
    };
    assert_eq!(wired.integer_class_id(), Some(42));
    let no_vm = DirectHelperTable {
        static_base_resolver_ctx: 0,
        ..wired
    };
    assert_eq!(no_vm.integer_class_id(), None);
    let no_resolver = DirectHelperTable {
        well_known_class_id: 0,
        ..wired
    };
    assert_eq!(no_resolver.integer_class_id(), None);
    let zero = DirectHelperTable {
        well_known_class_id: answers_zero as *const () as usize,
        ..wired
    };
    assert_eq!(zero.integer_class_id(), None);
    let too_wide = DirectHelperTable {
        well_known_class_id: answers_too_wide as *const () as usize,
        ..wired
    };
    assert_eq!(too_wide.integer_class_id(), None);
    assert_eq!(DirectHelperTable::EMPTY.integer_class_id(), None);
}

/// Round 11 wave 14: `string_hash_field` needs both answers, decodes the
/// field selector's `index + 1`, and treats `0` (unknown) as no answer —
/// field 0 is reachable only as `1`.
#[test]
fn the_string_hash_field_needs_the_class_and_a_decoded_index() {
    unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
        match (ctx, which) {
            (0x1000, WELL_KNOWN_CLASS_STRING) => 7,
            (0x1000, WELL_KNOWN_STRING_HASH_FIELD) => 3,
            (0x2000, WELL_KNOWN_CLASS_STRING) => 7,
            (0x2000, WELL_KNOWN_STRING_HASH_FIELD) => 0,
            (0x3000, WELL_KNOWN_STRING_HASH_FIELD) => 1,
            _ => 0,
        }
    }
    let wired = DirectHelperTable {
        well_known_class_id: answers as *const () as usize,
        static_base_resolver_ctx: 0x1000,
        ..DirectHelperTable::EMPTY
    };
    assert_eq!(wired.string_hash_field(), Some((7, 2)));
    assert_eq!(wired.integer_class_id(), None, "the selectors stay apart");
    let no_field = DirectHelperTable {
        static_base_resolver_ctx: 0x2000,
        ..wired
    };
    assert_eq!(no_field.string_hash_field(), None);
    let no_class = DirectHelperTable {
        static_base_resolver_ctx: 0x3000,
        ..wired
    };
    assert_eq!(no_class.string_hash_field(), None);
    assert_eq!(DirectHelperTable::EMPTY.string_hash_field(), None);
}
