// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 8, lane `arraylist8` — the inline `instanceof`
//! final-class miss for a USER class (`NOTES-w5-typecheck5.md` cross-lane
//! request 2).
//!
//! Before this, the inline definite miss (`x instanceof T` answered `0` inline
//! for a plain object of any other class) fired only for the fixed list of
//! final BOOT classes (`instanceof_miss_is_final_boot_class`). A compile door
//! that proves its resolved target `final` now interns the site through
//! `intern_typecheck_target_with_finality(.., true)`, and
//! `typecheck_target_is_final_class` admits it too. What this executes:
//!
//! * a site interned FINAL answers a miss inline, keeps the exact-match hit
//!   inline, and still sends the id-range (lambda proxy / autobox) and array
//!   receivers to the helper;
//! * a site for the SAME name and id interned NOT final gets a different
//!   pointer and keeps sending the miss to the helper;
//! * a final verdict without a resolved id is ignored.
//!
//! One `#[test]` in its own binary, like the typecheck5 test it mirrors, so the
//! process-wide duplicate-class-name latch (which also routes the miss to the
//! helper) is down unless something in this binary raised it.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

static HITS: AtomicUsize = AtomicUsize::new(0);

/// Marker `jit_instanceof`: counts, and answers `1` — the answer the helper's
/// by-name fallback would give a duplicate `String`, and one the inline `0`
/// stub can never produce.
unsafe extern "C" fn marker_instanceof(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
    HITS.fetch_add(1, Ordering::SeqCst);
    1
}

/// Stub helpers, as `r9_invoke_int_results_and_self_tail_call.rs` builds them,
/// with `instanceof_check` wired to the marker.
fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w8 arraylist8 test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    let throw_bci = record_throw_bci as *const () as usize;
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    let deopt_unserviceable = deopt_unserviceable_stub as *const () as usize;
    JitRuntimeHelpers {
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_card_table_addr: 0,
        jit_card_old_base: 0,
        jit_card_old_end: 0,
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: marker_instanceof as *const () as usize,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        get_current_thread: 0,
        tlab_post_init: 0,
        frame_record: 0,
        shadow_stack_offset_in_thread: 0,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

/// A header-only stand-in for a heap object: the class id at offset 0
/// (`class_id_offset_in_obj` above) and the KIND_TAGS byte, all else zero.
fn fake_header(class_id: u32, kind_tags: u8) -> Box<[u64; 8]> {
    let kind_off = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    assert!(
        (4..64).contains(&kind_off),
        "the fake header assumes KIND_TAGS lies past the 4-byte class id and inside 64 bytes"
    );
    let mut words = Box::new([0u64; 8]);
    let base = words.as_mut_ptr() as *mut u8; // Cast: byte view of the header words
                                              // SAFETY: `base` addresses 64 owned bytes and both writes are in range.
    unsafe {
        std::ptr::copy_nonoverlapping(class_id.to_le_bytes().as_ptr(), base, 4);
        *base.add(kind_off) = kind_tags;
    }
    words
}

/// Compile `int f(Object o) { return o instanceof <site>; }` for an already
/// interned site pointer, through the door that reaches the inline typecheck
/// guard: a non-empty `method_key` and an oop-marked parameter.
fn instanceof_method(
    helpers: &JitRuntimeHelpers,
    site: (*const u8, usize),
) -> cratonvm_jit::CompiledMethod {
    let (name_ptr, name_len) = site;
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0xac, // 4: ireturn
        0, 0,
    ];
    compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        5,
        1,
        1,
        false,
        Vec::new(),                         // multianewarray_info
        Vec::new(),                         // field_info
        vec![(1usize, name_ptr, name_len)], // typecheck_info
        Vec::new(),                         // static_field_info
        Vec::new(),                         // new_info
        Vec::new(),                         // new_deferred_info
        Vec::new(),                         // anewarray_info
        Vec::new(),                         // anewarray_deferred_info
        Vec::new(),                         // invoke_info
        Vec::new(),                         // direct_calls
        Vec::new(),                         // mic_slots
        Vec::new(),                         // pic_slots
        Vec::new(),                         // ldc_info
        Vec::new(),                         // ldc_string_info
        Vec::new(),                         // ldc_class_info
        Vec::new(),                         // ldc2w_info
        Default::default(),                 // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        helpers,
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,           // string_layout
        &[],
        0,
        0b1,                            // param_oop_mask: the parameter is a reference
        Vec::new(),                     // compact_field_info
        "R9w8.f:(Ljava/lang/Object;)I", // non-empty => trusted-oop eligible
        None,                           // despec
        Vec::new(),                     // indy_info
        None,                           // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .expect("instanceof must compile")
}

/// Run the compiled method on `obj` and return (answer, helper calls made).
fn run(compiled: &cratonvm_jit::CompiledMethod, obj: &[u64; 8]) -> (i64, usize) {
    let before = HITS.load(Ordering::SeqCst);
    // SAFETY: JIT code compiled by this test from valid bytecode; the argument
    // is a live, aligned fake header the guard only reads, and the only helper
    // it can call is the marker above.
    let got = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    (got, HITS.load(Ordering::SeqCst) - before)
}

#[test]
fn a_final_user_class_target_answers_the_miss_inline_and_a_non_final_one_does_not() {
    if !cratonvm_jit::x64::checkcast_inline_enabled() {
        return; // `CRATONVM_JIT_CHECKCAST_INLINE=0`: nothing inline to assert.
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INSTANCEOF_FINAL_MISS") {
        return; // explicitly opted out: the miss always calls the helper.
    }
    if cratonvm_types::duplicate_class_name_seen() {
        return; // the latch routes every miss to the helper; nothing to tell apart
    }
    const NAME: &str = "com/example/r9w8/FinalUserClass";
    const TARGET: u32 = 0x0006_2201;
    let helpers = helpers();

    let final_site = cratonvm_jit::intern_typecheck_target_with_finality(NAME, Some(TARGET), true);
    let plain_site = cratonvm_jit::intern_typecheck_target(NAME, Some(TARGET));
    assert_ne!(
        final_site.0, plain_site.0,
        "a final verdict must intern its own pointer, so a door that knows          nothing about finality is never handed one"
    );
    assert!(cratonvm_jit::typecheck_target_is_final_class(final_site.0));
    assert!(!cratonvm_jit::typecheck_target_is_final_class(plain_site.0));
    assert_eq!(
        cratonvm_jit::typecheck_target_for_site(final_site.0),
        Some(TARGET)
    );
    assert_eq!(
        cratonvm_jit::typecheck_target_for_site(plain_site.0),
        Some(TARGET)
    );
    // No resolved id: there is no class to be final.
    let unresolved = cratonvm_jit::intern_typecheck_target_with_finality(NAME, None, true);
    assert!(!cratonvm_jit::typecheck_target_is_final_class(unresolved.0));

    let exact = fake_header(TARGET, 0);
    let other = fake_header(TARGET + 1, 0);
    let proxy = fake_header(0x8000_0001, 0); // lambda-proxy / autobox id range
    let array = fake_header(TARGET, 0x01);

    let fin = instanceof_method(&helpers, final_site);
    assert_eq!(
        run(&fin, &other),
        (0, 0),
        "a final user class's miss is answered inline"
    );
    assert_eq!(run(&fin, &exact), (1, 0), "an exact match is inline");
    assert_eq!(
        run(&fin, &proxy),
        (1, 1),
        "an id >= 0x8000_0000 asks the helper"
    );
    assert_eq!(run(&fin, &array), (1, 1), "an array header asks the helper");

    let plain = instanceof_method(&helpers, plain_site);
    assert_eq!(
        run(&plain, &other),
        (1, 1),
        "a site not proved final must keep asking the helper on a miss"
    );
    assert_eq!(
        run(&plain, &exact),
        (1, 0),
        "its exact match is still inline"
    );
}
