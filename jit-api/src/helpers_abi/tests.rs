use super::*;
use crate::FieldKind;
use core::mem::offset_of;

type H = JitRuntimeHelpers;

/// Every descriptor offset, compared against an independently written
/// `offset_of!` probe for the same field. The table itself is built with
/// `offset_of!`, so this additionally pins the *order* of the rows: probe
/// `i` is written out by hand in ABI order, and a reordered table row
/// would land on the wrong probe.
#[test]
fn helper_fields_offsets_match_offset_of() {
    let probes: [(&str, usize); H::NUM_FIELDS] = [
        ("newarray", offset_of!(H, newarray)),
        ("new_object", offset_of!(H, new_object)),
        ("anewarray_object", offset_of!(H, anewarray_object)),
        ("baload", offset_of!(H, baload)),
        ("bastore", offset_of!(H, bastore)),
        ("iaload", offset_of!(H, iaload)),
        ("iastore", offset_of!(H, iastore)),
        ("aaload", offset_of!(H, aaload)),
        ("aastore", offset_of!(H, aastore)),
        ("multianewarray_2d", offset_of!(H, multianewarray_2d)),
        ("arraylength", offset_of!(H, arraylength)),
        ("getfield", offset_of!(H, getfield)),
        ("putfield_int", offset_of!(H, putfield_int)),
        ("putfield_long", offset_of!(H, putfield_long)),
        ("putfield_float", offset_of!(H, putfield_float)),
        ("putfield_double", offset_of!(H, putfield_double)),
        ("putfield_object", offset_of!(H, putfield_object)),
        ("getstatic", offset_of!(H, getstatic)),
        ("putstatic_int", offset_of!(H, putstatic_int)),
        ("putstatic_long", offset_of!(H, putstatic_long)),
        ("putstatic_float", offset_of!(H, putstatic_float)),
        ("putstatic_double", offset_of!(H, putstatic_double)),
        ("putstatic_object", offset_of!(H, putstatic_object)),
        ("checkcast", offset_of!(H, checkcast)),
        ("instanceof_check", offset_of!(H, instanceof_check)),
        ("throw_aioobe", offset_of!(H, throw_aioobe)),
        ("throw_arithmetic", offset_of!(H, throw_arithmetic)),
        ("invoke_dispatch", offset_of!(H, invoke_dispatch)),
        ("invoke_virtual_mic", offset_of!(H, invoke_virtual_mic)),
        ("lambda_int_to_double", offset_of!(H, lambda_int_to_double)),
        ("write_barrier", offset_of!(H, write_barrier)),
        (
            "satb_pre_write_barrier",
            offset_of!(H, satb_pre_write_barrier),
        ),
        ("uncommon_trap", offset_of!(H, uncommon_trap)),
        ("math_fma_double", offset_of!(H, math_fma_double)),
        ("math_fma_float", offset_of!(H, math_fma_float)),
        (
            "tlab_cursor_offset_in_thread",
            offset_of!(H, tlab_cursor_offset_in_thread),
        ),
        (
            "tlab_end_offset_in_thread",
            offset_of!(H, tlab_end_offset_in_thread),
        ),
        (
            "class_id_offset_in_obj",
            offset_of!(H, class_id_offset_in_obj),
        ),
        ("get_current_thread", offset_of!(H, get_current_thread)),
        ("tlab_post_init", offset_of!(H, tlab_post_init)),
        ("frame_record", offset_of!(H, frame_record)),
        (
            "shadow_stack_offset_in_thread",
            offset_of!(H, shadow_stack_offset_in_thread),
        ),
        ("throw_exception", offset_of!(H, throw_exception)),
        ("jit_npe_with_action", offset_of!(H, jit_npe_with_action)),
        ("dispatch_threw", offset_of!(H, dispatch_threw)),
        ("jit_frem", offset_of!(H, jit_frem)),
        ("jit_drem", offset_of!(H, jit_drem)),
        (
            "self_call_stack_guard",
            offset_of!(H, self_call_stack_guard),
        ),
        ("region_bounds_addr", offset_of!(H, region_bounds_addr)),
        (
            "native_stack_floor_fn",
            offset_of!(H, native_stack_floor_fn),
        ),
        ("ldc_string", offset_of!(H, ldc_string)),
        ("safepoint_flag_addr", offset_of!(H, safepoint_flag_addr)),
        ("safepoint_slow_path", offset_of!(H, safepoint_slow_path)),
        ("jit_card_table_addr", offset_of!(H, jit_card_table_addr)),
        ("jit_card_old_base", offset_of!(H, jit_card_old_base)),
        ("jit_card_old_end", offset_of!(H, jit_card_old_end)),
        ("set_throw_bci", offset_of!(H, set_throw_bci)),
        ("service_callee_deopt", offset_of!(H, service_callee_deopt)),
        ("new_object_cp", offset_of!(H, new_object_cp)),
        ("anewarray_object_cp", offset_of!(H, anewarray_object_cp)),
        ("monitor_enter", offset_of!(H, monitor_enter)),
        ("monitor_exit", offset_of!(H, monitor_exit)),
        ("ldc_class_cp", offset_of!(H, ldc_class_cp)),
        ("aastore_type_check", offset_of!(H, aastore_type_check)),
        ("read_bounds_addr", offset_of!(H, read_bounds_addr)),
        ("local_handler_lookup", offset_of!(H, local_handler_lookup)),
        ("ldc_string_cp", offset_of!(H, ldc_string_cp)),
        ("ffm_segment_get", offset_of!(H, ffm_segment_get)),
        ("ffm_segment_set", offset_of!(H, ffm_segment_set)),
        ("ref_store_pre_gate", offset_of!(H, ref_store_pre_gate)),
        ("ref_store_post_gate", offset_of!(H, ref_store_post_gate)),
        (
            "ref_store_post_young_floor",
            offset_of!(H, ref_store_post_young_floor),
        ),
        ("g1_barrier_addr", offset_of!(H, g1_barrier_addr)),
        (
            "g1_post_write_barrier",
            offset_of!(H, g1_post_write_barrier),
        ),
        (
            "ref_store_post_skip_mask",
            offset_of!(H, ref_store_post_skip_mask),
        ),
        ("zgc_note_tlab_object", offset_of!(H, zgc_note_tlab_object)),
        ("multianewarray_n", offset_of!(H, multianewarray_n)),
        (
            "monitor_block_offset_in_thread",
            offset_of!(H, monitor_block_offset_in_thread),
        ),
        (
            "jit_stack_floor_offset_in_thread",
            offset_of!(H, jit_stack_floor_offset_in_thread),
        ),
    ];

    assert_eq!(HELPER_FIELDS.len(), probes.len());
    for (i, (name, offset)) in probes.iter().enumerate() {
        let desc = &HELPER_FIELDS[i];
        assert_eq!(
            desc.name, *name,
            "HELPER_FIELDS row {} is `{}`, expected `{}` — the ABI order \
             changed",
            i, desc.name, name,
        );
        assert_eq!(
            desc.offset, *offset,
            "HELPER_FIELDS offset for `{}` is {}, offset_of! says {}",
            name, desc.offset, offset,
        );
        assert_eq!(
            desc.offset,
            i * HELPER_FIELD_STRIDE,
            "field `{}` is not at its sequential JIT ABI offset",
            name,
        );
    }
}

/// The literal size and alignment the JIT's baked offsets assume. Written
/// as bare numbers on purpose: an accidental layout change must fail here
/// loudly rather than be absorbed by a computed expression.
#[test]
fn helper_table_size_and_align_are_the_literal_abi_numbers() {
    assert_eq!(core::mem::size_of::<H>(), 632);
    assert_eq!(core::mem::align_of::<H>(), 8);
    assert_eq!(JIT_HELPERS_ABI_SIZE, 632);
    assert_eq!(JIT_HELPERS_ABI_ALIGN, 8);
    assert_eq!(HELPER_FIELD_STRIDE, 8);
    assert_eq!(NUM_HELPER_FIELDS, 79);
    assert_eq!(H::NUM_FIELDS, 79);
    // v10's three appended slots are gate ADDRESSES, not call targets, so
    // the callable-slot count stood still while the table grew. v11 (F-08)
    // appended one of each: `g1_barrier_addr` is a table address and
    // `g1_post_write_barrier` IS a call target, so the callable count moves
    // by exactly one. That divergence is the point of counting them apart.
    // v12 appends `ref_store_post_skip_mask`, a Constant (a VALUE this
    // time, not an address), so the callable count stands still again.
    // v13 appends `zgc_note_tlab_object`, an optional call target.
    // v14 appends `multianewarray_n`, a REQUIRED call target, so both the
    // callable count and the required count move by one.
    // v15 appends `monitor_block_offset_in_thread`, an Offset, so neither
    // callable count moves.
    // v16 appends `jit_stack_floor_offset_in_thread`, an Offset too.
    assert_eq!(H::NUM_HELPER_FN_FIELDS, 62);
    assert_eq!(JIT_HELPERS_ABI_VERSION, 16);
}

/// The golden table is the only name→offset binding in the crate written
/// as literals. Everything else derives from the struct and therefore
/// follows a reorder instead of catching it.
#[test]
fn golden_offsets_are_the_struct_offsets() {
    assert_eq!(GOLDEN_HELPER_OFFSETS.len(), H::NUM_FIELDS);
    for (i, (name, offset)) in GOLDEN_HELPER_OFFSETS.iter().enumerate() {
        let desc = helper_field(name)
            .unwrap_or_else(|| panic!("golden table names an unknown slot `{}`", name));
        assert_eq!(
            desc.offset, *offset,
            "`{}` moved: golden offset {}, struct offset {}",
            name, offset, desc.offset,
        );
        assert_eq!(
            HELPER_FIELDS[i].name, *name,
            "golden row {} is `{}`, HELPER_FIELDS row {} is `{}`",
            i, name, i, HELPER_FIELDS[i].name,
        );
    }
    // The last golden offset plus one stride is the whole table.
    let (last_name, last_offset) = GOLDEN_HELPER_OFFSETS[H::NUM_FIELDS - 1];
    assert_eq!(last_name, "jit_stack_floor_offset_in_thread");
    assert_eq!(last_offset + HELPER_FIELD_STRIDE, JIT_HELPERS_ABI_SIZE);
}

#[test]
fn abi_revision_ledger_names_the_current_table() {
    let last = *ABI_REVISIONS.last().expect("the ledger is never empty");
    assert_eq!(last.version, JIT_HELPERS_ABI_VERSION);
    assert_eq!(last.num_fields, NUM_HELPER_FIELDS);
    assert_eq!(last.size, core::mem::size_of::<H>());
    assert_eq!(
        last,
        HelperAbiRevision {
            version: 16,
            num_fields: 79,
            size: 632,
        },
    );
    // Append-only history: each revision strictly grows the table.
    for pair in ABI_REVISIONS.windows(2) {
        assert!(pair[1].version == pair[0].version + 1);
        assert!(pair[1].num_fields > pair[0].num_fields);
    }
}

/// The signature table is derived from the `HelperFn*` aliases, so this
/// pins the shapes the emitter's hand-written call sites assume.
#[test]
fn helper_fn_sigs_record_the_real_c_signatures() {
    let by_name = |n: &str| {
        *HELPER_FN_SIGS
            .iter()
            .find(|s| s.field == n)
            .unwrap_or_else(|| panic!("no signature row for `{}`", n))
    };

    assert_eq!(HELPER_FN_SIGS.len(), H::NUM_HELPER_FN_FIELDS);

    // The only helper that outruns Win64's four integer argument
    // registers. Its call site writes args 5 and 6 to [RSP+32]/[RSP+40].
    let mic = by_name("invoke_virtual_mic");
    assert_eq!((mic.arity, mic.float_args), (6, 0));
    assert_eq!(mic.win64_stack_args(), 2);
    assert_eq!(mic.int_args(), 6);
    assert!(mic.returns_value && !mic.returns_float);
    assert_eq!(HELPERS_NEEDING_WIN64_STACK_ARGS, 1);

    // All-float helpers: operands and result in XMM.
    let fma = by_name("math_fma_double");
    assert_eq!((fma.arity, fma.float_args), (3, 3));
    assert!(fma.returns_value && fma.returns_float);
    let frem = by_name("jit_frem");
    assert_eq!((frem.arity, frem.float_args), (2, 2));
    assert!(frem.returns_float);

    // `-> ()`. A call site that treats one of these as returning a value
    // reads whatever the callee left in RAX.
    for void_helper in [
        "jit_npe_with_action",
        "set_throw_bci",
        "write_barrier",
        "frame_record",
    ] {
        assert!(
            !by_name(void_helper).returns_value,
            "`{}` is declared `-> ()`",
            void_helper,
        );
    }

    // Zero-argument helpers.
    for nullary in [
        "throw_arithmetic",
        "dispatch_threw",
        "get_current_thread",
        "native_stack_floor_fn",
    ] {
        assert_eq!(
            by_name(nullary).arity,
            0,
            "`{}` takes no arguments",
            nullary
        );
    }
    // (flag_addr, body_id) since interpreter round i1 wave 17.
    assert_eq!(by_name("safepoint_slow_path").arity, 2);
    // The loop-exit verdict (interpreter round i1 wave 12): an OSR body's
    // back-edge poll reads RAX after the call.
    let poll = by_name("safepoint_slow_path");
    assert!(poll.returns_value && !poll.returns_float);

    // The mechanical accessor-naming rule, including the field that
    // already ends in `_fn`.
    for sig in HELPER_FN_SIGS {
        assert_eq!(
            sig.accessor,
            format!("{}_fn", sig.field),
            "accessor naming rule broken for `{}`",
            sig.field,
        );
        assert!(accessor_name_matches_field(sig.accessor, sig.field));
        assert!(sig.float_args == 0 || sig.float_args == sig.arity);
    }
    assert_eq!(
        by_name("native_stack_floor_fn").accessor,
        "native_stack_floor_fn_fn",
    );
}

#[test]
fn every_accessor_is_none_on_a_zeroed_table() {
    let h = H::default();
    let probe = h.helper_fn_accessor_probe();
    assert_eq!(probe.len(), H::NUM_HELPER_FN_FIELDS);
    for (name, value) in probe.iter() {
        assert!(
            value.is_none(),
            "`{}_fn` returned Some for an unwired (zero) slot — a null \
             helper could be CALLed",
            name,
        );
    }
}

/// Wire exactly one callable slot and require that exactly one accessor
/// sees it, and that it is the accessor named for that slot.
///
/// This is what catches two swapped rows in `helper_fn_slots!`: swapping
/// the fields of two rows leaves every count, offset, census and size
/// check satisfied while handing each helper the other's C signature.
#[test]
fn accessor_reads_only_its_own_slot() {
    unsafe extern "C" fn marker() {}
    let addr = marker as usize;
    assert_ne!(addr, 0);

    let fn_slots: Vec<&HelperFieldDesc> = HELPER_FIELDS
        .iter()
        .filter(|d| d.kind == HelperKind::Function)
        .collect();
    assert_eq!(fn_slots.len(), H::NUM_HELPER_FN_FIELDS);

    for target in fn_slots {
        let mut h = H::default();
        let idx = target.offset / HELPER_FIELD_STRIDE;
        // SAFETY: `idx < NUM_FIELDS` because `offset` came from
        // `offset_of!` on this struct and the layout is exactly
        // `[usize; NUM_FIELDS]` (see `as_words`). The write is in bounds,
        // correctly typed and correctly aligned. `addr` is the address of
        // a real function, so no accessor fabricates an invalid function
        // pointer; none of them are called.
        unsafe { *(&mut h as *mut H as *mut usize).add(idx) = addr };

        let probe = h.helper_fn_accessor_probe();
        let wired: Vec<&'static str> = probe
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(
            wired,
            vec![target.name],
            "wiring only `{}` should light up only its own accessor",
            target.name,
        );
        let (_, value) = probe
            .iter()
            .find(|(n, _)| *n == target.name)
            .expect("the probe covers every callable slot");
        assert_eq!(
            *value,
            Some(addr),
            "`{}_fn` returned a different address than the slot holds",
            target.name,
        );
    }
}

#[test]
fn typed_helper_addr_yields_the_declared_functions_address() {
    unsafe extern "C" fn stub_arraylength(_array_ptr: i64) -> i64 {
        3
    }
    // Compiles only because `stub_arraylength` really has
    // `HelperFnArraylength`'s shape.
    let addr = crate::typed_helper_addr!(HelperFnArraylength, stub_arraylength);
    assert_eq!(addr, stub_arraylength as usize);

    let mut h = H::default();
    h.arraylength = addr;
    let f = h.arraylength_fn().expect("wired slot must be Some");
    // SAFETY: the slot holds `stub_arraylength`, which matches
    // `HelperFnArraylength` exactly and dereferences none of its arguments.
    assert_eq!(unsafe { f(0) }, 3);
}

/// The census the const assertions pin, restated as values so a failure
/// says which class drifted rather than only that one did.
#[test]
fn slot_census_matches_the_pinned_counts() {
    let functions = HELPER_FIELDS
        .iter()
        .filter(|d| d.kind == HelperKind::Function)
        .count();
    let offsets = HELPER_FIELDS
        .iter()
        .filter(|d| d.kind == HelperKind::Offset)
        .count();
    let constants = HELPER_FIELDS
        .iter()
        .filter(|d| d.kind == HelperKind::Constant)
        .count();
    let required = HELPER_FIELDS.iter().filter(|d| d.required).count();
    assert_eq!(functions, 62, "callable slots");
    // v15's `monitor_block_offset_in_thread` is a displacement: 4 -> 5.
    // v16's `jit_stack_floor_offset_in_thread` is one too: 5 -> 6.
    assert_eq!(offsets, 6, "displacement slots");
    // v10 appended three Constants (gate ADDRESSES, not call targets) and
    // v11 (F-08) appended one more Constant — `g1_barrier_addr`, the
    // geometry table's address — plus one optional Function,
    // `g1_post_write_barrier`. So the callable count moved by exactly one
    // across the two revisions and the baked-address count by four, which
    // is the divergence counting them apart exists to show.
    // v12 appends one more Constant, `ref_store_post_skip_mask` — a baked
    // VALUE rather than an address, which is why it is a Constant and not
    // an OptionalPtr: nothing about it is range-checkable as a pointer.
    assert_eq!(constants, 11, "baked-address slots");
    assert_eq!(required, 44, "required slots");
    // v13's `zgc_note_tlab_object` is an optional call target.
    assert_eq!(functions - required, 18, "optional callable slots");
    assert_eq!(functions + offsets + constants, H::NUM_FIELDS);
}

/// The descriptor table and the crate root's `helper_fields!` list are two
/// independent transcriptions of the same struct. Neither may drift.
#[test]
fn helper_fields_agree_with_crate_root() {
    let h = H::default();
    let root = h.all_fields();
    assert_eq!(root.len(), HELPER_FIELDS.len());
    for (i, entry) in root.iter().enumerate() {
        let desc = &HELPER_FIELDS[i];
        assert_eq!(desc.name, entry.name, "field name drift at index {}", i);
        match entry.kind {
            FieldKind::RequiredPtr => {
                assert_eq!(desc.kind, HelperKind::Function, "{}", desc.name);
                assert!(desc.required, "{} should be required", desc.name);
            }
            FieldKind::OptionalPtr => {
                assert_eq!(desc.kind, HelperKind::Function, "{}", desc.name);
                assert!(!desc.required, "{} should be optional", desc.name);
            }
            FieldKind::Offset => {
                assert!(
                    desc.kind == HelperKind::Offset || desc.kind == HelperKind::Constant,
                    "{} should be a non-callable slot",
                    desc.name,
                );
                assert!(!desc.required, "{} should not be required", desc.name);
            }
        }
    }
}

#[test]
fn validate_with_rejects_a_zeroed_table_naming_the_first_gap() {
    let h = H::default();
    // `newarray` is slot 0 and required, so it is the first miss.
    assert_eq!(
        h.validate_with(JIT_HELPERS_ABI_VERSION),
        Err(HelperAbiError::MissingRequired("newarray")),
    );
    assert_eq!(
        h.validate_abi(),
        Err(HelperAbiError::MissingRequired("newarray"))
    );
    // The message must name the field.
    let msg = HelperAbiError::MissingRequired("newarray").to_string();
    assert!(msg.contains("newarray"), "unhelpful message: {}", msg);
}

#[test]
fn validate_with_reports_the_next_gap_once_the_first_is_wired() {
    let mut h = H::default();
    h.newarray = 0x1000;
    assert_eq!(
        h.validate_with(JIT_HELPERS_ABI_VERSION),
        Err(HelperAbiError::MissingRequired("new_object")),
    );
}

#[test]
fn validate_with_rejects_a_foreign_abi_version() {
    // The version check runs before the field scan, so even a fully wired
    // table is rejected.
    let h = fully_wired();
    assert_eq!(
        h.validate_with(JIT_HELPERS_ABI_VERSION + 1),
        Err(HelperAbiError::VersionMismatch {
            expected: JIT_HELPERS_ABI_VERSION,
            found: JIT_HELPERS_ABI_VERSION + 1,
        }),
    );
}

#[test]
fn validate_with_accepts_a_wired_table_with_zero_optionals() {
    // Every optional slot and every offset stays 0: that is the documented
    // "not wired" state and must not be an error.
    let h = fully_wired();
    assert_eq!(h.validate_with(JIT_HELPERS_ABI_VERSION), Ok(()));
}

#[test]
fn validate_with_rejects_a_pointer_stored_in_an_offset_slot() {
    let mut h = fully_wired();
    h.tlab_cursor_offset_in_thread = 0x7fff_1234_5678;
    assert_eq!(
        h.validate_with(JIT_HELPERS_ABI_VERSION),
        Err(HelperAbiError::ImplausibleOffset {
            name: "tlab_cursor_offset_in_thread",
            value: 0x7fff_1234_5678,
        }),
    );
}

#[test]
fn accessors_return_none_for_zeroed_slots() {
    let h = H::default();
    assert!(h.newarray_fn().is_none());
    assert!(h.invoke_virtual_mic_fn().is_none());
    assert!(h.math_fma_double_fn().is_none());
    assert!(h.get_current_thread_fn().is_none());
    assert!(h.frame_record_fn().is_none());
    assert!(h.ldc_string_fn().is_none());
    assert!(h.native_stack_floor_fn_fn().is_none());
    assert!(h.service_callee_deopt_fn().is_none());
    assert!(h.safepoint_slow_path_fn().is_none());
}

/// A wired slot round-trips: the accessor hands back a pointer whose
/// address is the one that was stored, with no offset confusion.
#[test]
fn accessors_round_trip_a_wired_slot() {
    unsafe extern "C" fn stub_arraylength(_array_ptr: i64) -> i64 {
        7
    }
    unsafe extern "C" fn stub_drem(a: f64, b: f64) -> f64 {
        a % b
    }

    let mut h = H::default();
    h.arraylength = stub_arraylength as *const () as usize;
    h.jit_drem = stub_drem as *const () as usize;

    let f = h.arraylength_fn().expect("wired slot must be Some");
    assert_eq!(f as usize, h.arraylength);
    // SAFETY: the slot holds `stub_arraylength`, which matches
    // `HelperFnArraylength` exactly and dereferences none of its arguments.
    assert_eq!(unsafe { f(0) }, 7);

    let g = h.jit_drem_fn().expect("wired slot must be Some");
    assert_eq!(g as usize, h.jit_drem);
    // SAFETY: as above — `stub_drem` matches `HelperFnJitDrem` and is pure.
    assert_eq!(unsafe { g(5.0, 3.0) }, 2.0);

    // Neighbouring slots stay unwired: the accessors read their own field.
    assert!(h.aastore_fn().is_none());
    assert!(h.jit_frem_fn().is_none());
}

#[test]
fn word_at_reads_the_slot_the_descriptor_names() {
    let mut h = H::default();
    h.set_throw_bci = 0xdead_beef;
    let desc = helper_field("set_throw_bci").expect("described slot");
    assert_eq!(h.word_at(desc), 0xdead_beef);
    assert_eq!(h.as_words()[desc.offset / HELPER_FIELD_STRIDE], 0xdead_beef);
    assert_eq!(helper_field_offset("set_throw_bci"), Some(desc.offset));
    assert!(helper_field("no_such_helper").is_none());
}

#[test]
fn as_words_matches_the_struct_fields() {
    let mut h = H::default();
    h.newarray = 1;
    // The LAST field, whatever it currently is — `jit_stack_floor_offset_
    // in_thread` since v16 appended the native-stack floor word's
    // displacement after v15's inline thin-lock block displacement.
    h.jit_stack_floor_offset_in_thread = 2;
    let w = h.as_words();
    assert_eq!(w[0], 1, "first slot");
    assert_eq!(w[H::NUM_FIELDS - 1], 2, "last slot");
    assert_eq!(w.len(), 79);
}

/// Build a table with every *required* slot non-zero and every optional
/// slot left at the documented `0`.
fn fully_wired() -> H {
    let mut h = H::default();
    for (i, desc) in HELPER_FIELDS.iter().enumerate() {
        if desc.required {
            // SAFETY: `i < NUM_HELPER_FIELDS` and the struct's layout is
            // exactly `[usize; NUM_HELPER_FIELDS]` (see `as_words`), so the
            // write is in bounds, correctly typed and correctly aligned.
            // The pointer is re-derived from a fresh `&mut h` on every
            // iteration, so no stale provenance is carried across writes.
            // The value is a dummy non-zero address that is never called.
            unsafe { *(&mut h as *mut H as *mut usize).add(i) = 0x1000 + i };
        }
    }
    h
}
