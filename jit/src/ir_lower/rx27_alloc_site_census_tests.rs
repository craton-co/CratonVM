use super::*;

/// A ZGC `new` of a post-init no-op class that took the inline bump.
fn zgc_noop() -> NewSiteFacts {
    NewSiteFacts {
        inline_tlab: true,
        proven_initialized: true,
        bump_emitted: true,
        post_init_noop: true,
        spliced: false,
        skip_post_init: false,
        announce: true,
        inline_start_bit: true,
    }
}

#[test]
fn a_stub_route_is_named_before_any_bump_choice() {
    let f = NewSiteFacts {
        inline_tlab: false,
        proven_initialized: false,
        bump_emitted: false,
        ..zgc_noop()
    };
    assert_eq!(new_site_call_reason(f), "inline-tlab-off");
    let f = NewSiteFacts {
        inline_tlab: true,
        ..f
    };
    assert_eq!(new_site_call_reason(f), "class-not-proven-initialised");
    let f = NewSiteFacts {
        proven_initialized: true,
        ..f
    };
    assert_eq!(new_site_call_reason(f), "bump-declined");
}

#[test]
fn a_zgc_noop_class_calls_only_when_the_start_bit_store_is_off() {
    assert_eq!(new_site_call_reason(zgc_noop()), "none");
    let off = NewSiteFacts {
        inline_start_bit: false,
        ..zgc_noop()
    };
    assert_eq!(new_site_call_reason(off), "zgc-inline-start-bit-off");
}


#[test]
fn a_chunk_parsing_collector_skips_post_init_or_names_the_kill_switch() {
    // Generational / G1: no announce; a no-op class skips the call.
    let skip = NewSiteFacts {
        announce: false,
        skip_post_init: true,
        inline_start_bit: false,
        ..zgc_noop()
    };
    assert_eq!(new_site_call_reason(skip), "none");
    // `CRATONVM_JIT_IR_TLAB_SKIP_POST_INIT=0` (or `..._ZGC_ANNOUNCE=0`).
    let kept = NewSiteFacts {
        skip_post_init: false,
        ..skip
    };
    assert_eq!(new_site_call_reason(kept), "post-init-elision-off");
}

#[test]
fn an_array_site_names_the_any_gc_switch_only_for_a_reference_array_off_zgc() {
    let zgc_ref = NewArraySiteFacts {
        offered: true,
        reference: true,
        registration: true,
        anewarray_any_gc: false,
        bump_emitted: true,
        inline_start_bit: true,
    };
    // On ZGC the reference bump is offered whatever the ANY_GC switch says.
    assert_eq!(newarray_site_call_reason(zgc_ref), "none");
    let gen_ref = NewArraySiteFacts {
        offered: false,
        registration: false,
        inline_start_bit: false,
        ..zgc_ref
    };
    assert_eq!(newarray_site_call_reason(gen_ref), "anewarray-any-gc-off");
    let gen_prim = NewArraySiteFacts {
        reference: false,
        ..gen_ref
    };
    assert_eq!(newarray_site_call_reason(gen_prim), "inline-array-off");
    let declined = NewArraySiteFacts {
        offered: true,
        bump_emitted: false,
        ..zgc_ref
    };
    assert_eq!(newarray_site_call_reason(declined), "bump-declined");
    let no_store = NewArraySiteFacts {
        inline_start_bit: false,
        ..zgc_ref
    };
    assert_eq!(
        newarray_site_call_reason(no_store),
        "zgc-inline-start-bit-off"
    );
}
