//! gcd d9/d (2026-09-28): the site half of the callee-owned-arguments rule.

use super::caller_replay_to_call_is_exact as replay_exact;
use super::sp_direct_call_args_owned_by_callee;

/// The rule with its seventh term (gcd d10/f, the caller's replay) held
/// `true`, so the d9/d table below states the other six unchanged.
fn owned(
    enabled: bool,
    opcode: Option<u8>,
    invoke_kind: Option<u8>,
    sync_direct_site: bool,
    retire_cell: bool,
    pc_is_protected: bool,
) -> bool {
    sp_direct_call_args_owned_by_callee(
        enabled,
        opcode,
        invoke_kind,
        sync_direct_site,
        retire_cell,
        pc_is_protected,
        || true,
    )
}

const INVOKESTATIC: Option<u8> = Some(0xb8);
const STATIC_KIND: Option<u8> = Some(3);

/// A plain, unprotected, unsynchronized `invokestatic` with the switch on
/// is the one shape that may leave its arguments to the callee.
#[test]
fn a_plain_static_site_leaves_its_arguments_to_the_callee() {
    assert!(owned(true, INVOKESTATIC, STATIC_KIND, false, false, false));
}

/// Each term withdraws it on its own: the switch, another opcode or
/// invoke kind (an instance row handed to this arm), a caller-held
/// synchronized site, a retire-cell site, a protected pc, no info.
#[test]
fn every_term_of_the_rule_withdraws_ownership() {
    assert!(
        !owned(false, INVOKESTATIC, STATIC_KIND, false, false, false),
        "=0"
    );
    for op in [0xb6u8, 0xb7, 0xb9] {
        assert!(
            !owned(true, Some(op), STATIC_KIND, false, false, false),
            "opcode {op:#x}"
        );
    }
    assert!(
        !owned(true, None, STATIC_KIND, false, false, false),
        "no opcode"
    );
    for kind in [0u8, 1, 2, 4] {
        assert!(
            !owned(true, INVOKESTATIC, Some(kind), false, false, false),
            "kind {kind}"
        );
    }
    assert!(
        !owned(true, INVOKESTATIC, None, false, false, false),
        "no JitInvokeInfo"
    );
    assert!(
        !owned(true, INVOKESTATIC, STATIC_KIND, true, false, false),
        "sync site"
    );
    assert!(
        !owned(true, INVOKESTATIC, STATIC_KIND, false, true, false),
        "retire cell"
    );
    assert!(
        !owned(true, INVOKESTATIC, STATIC_KIND, false, false, true),
        "protected pc"
    );
}

/// gcd d9/d row 2 at the inline cache: the hit files every reference
/// argument but its receiver for the service-copy window.
#[test]
fn an_inline_cache_hit_files_every_reference_but_its_receiver() {
    use super::ic_hit_window_arg_oops as filed;
    assert_eq!(
        filed(&[true, true, false, true]),
        [false, true, false, true]
    );
    assert_eq!(filed(&[true]), [false]);
    assert!(filed(&[]).is_empty());
}

/// gcd d9/d row 4: a splice's direct call is admitted on the static
/// rule's terms, its protected pc and its replay verdict deciding as
/// there, and a virtual form keeps its copy.
#[test]
fn a_spliced_static_call_is_admitted_on_the_static_terms() {
    use super::splice_call_args_owned_by_callee as spliced;
    assert!(spliced(INVOKESTATIC, STATIC_KIND, false, || true));
    assert!(
        spliced(Some(0xb7), Some(1), false, || true),
        "invokespecial"
    );
    assert!(
        !spliced(INVOKESTATIC, STATIC_KIND, true, || true),
        "protected pc"
    );
    assert!(
        !spliced(INVOKESTATIC, STATIC_KIND, false, || false),
        "replay"
    );
    assert!(!spliced(Some(0xb6), Some(0), false, || true), "virtual");
    assert!(
        !spliced(INVOKESTATIC, None, false, || true),
        "no JitInvokeInfo"
    );
}

/// gcd d10/f (2026-09-28): a statically bound instance site (kind 1: an
/// `invokespecial`, or a private `invokevirtual` / `invokeinterface`
/// pinned to its owner) is admitted on the same terms; every virtual form
/// (kinds 0/2, whose receiver class the service reads out of the copy)
/// and every mismatched opcode/kind pair keeps the copy, and each other
/// term still withdraws it.
#[test]
fn a_statically_bound_instance_site_is_admitted_on_the_same_terms() {
    const INVOKESPECIAL: Option<u8> = Some(0xb7);
    const SPECIAL_KIND: Option<u8> = Some(1);
    for op in [0xb6u8, 0xb7, 0xb9] {
        assert!(
            owned(true, Some(op), SPECIAL_KIND, false, false, false),
            "{op:#x}"
        );
        for kind in [0u8, 2, 3, 4] {
            assert!(
                !owned(true, Some(op), Some(kind), false, false, false),
                "{op:#x}/{kind}"
            );
        }
    }
    assert!(
        !owned(true, INVOKESTATIC, SPECIAL_KIND, false, false, false),
        "static kind 1"
    );
    for op in [0x00u8, 0xb5, 0xba] {
        assert!(
            !owned(true, Some(op), SPECIAL_KIND, false, false, false),
            "{op:#x}"
        );
    }
    assert!(
        !owned(false, INVOKESPECIAL, SPECIAL_KIND, false, false, false),
        "=0"
    );
    assert!(
        !owned(true, INVOKESPECIAL, None, false, false, false),
        "no JitInvokeInfo"
    );
    assert!(
        !owned(true, INVOKESPECIAL, SPECIAL_KIND, true, false, false),
        "sync site"
    );
    assert!(
        !owned(true, INVOKESPECIAL, SPECIAL_KIND, false, true, false),
        "retire cell"
    );
    assert!(
        !owned(true, INVOKESPECIAL, SPECIAL_KIND, false, false, true),
        "protected pc"
    );
}

/// gcd d10/f (2026-09-28): the seventh term. A site whose caller cannot
/// be re-run from entry exactly keeps its copy, whatever the other six
/// say; and the term is asked LAST (it scans the bytecode), so a site
/// the other terms refuse never pays for it.
#[test]
fn a_caller_that_cannot_replay_exactly_keeps_the_copy() {
    fn plain(op: Option<u8>, kind: Option<u8>, replay: impl FnOnce() -> bool) -> bool {
        sp_direct_call_args_owned_by_callee(true, op, kind, false, false, false, replay)
    }
    for (op, kind) in [(0xb8u8, 3u8), (0xb7, 1)] {
        assert!(
            plain(Some(op), Some(kind), || true),
            "{op:#x}: exact replay"
        );
        assert!(
            !plain(Some(op), Some(kind), || false),
            "{op:#x}: inexact replay"
        );
    }
    let asked = std::cell::Cell::new(false);
    let probe = || {
        asked.set(true);
        true
    };
    let retire_cell = sp_direct_call_args_owned_by_callee(
        true,
        INVOKESTATIC,
        STATIC_KIND,
        false,
        true,
        false,
        probe,
    );
    assert!(!retire_cell);
    assert!(
        !asked.get(),
        "a retire-cell site must not scan the bytecode"
    );
}

/// The replay predicate on real shapes. Each method below is
/// `<prefix>; invokestatic #1 (the candidate call at `pc`); ireturn`.
#[test]
fn the_caller_replay_is_exact_only_before_any_side_effect() {
    // static boolean staticCaller(int n, boolean check) {
    //     return dropStatic(new long[n], check);
    // }  -- `Gcd1ArgPinSpecialProbe`: allocation only.
    let pure = [
        0x1a, // 0: iload_0
        0xbc, 0x0b, // 1: newarray long
        0x1b, // 3: iload_1
        0xb8, 0x00, 0x01, // 4: invokestatic dropStatic
        0xac, // 7: ireturn
    ];
    assert!(replay_exact(&pure, pure.len(), 4, &[]));

    // static boolean caller(int mb, boolean check) {
    //     Object[] box = new Object[1];
    //     return dropAndCheck(make(mb, box), box, check);
    // }  -- `Gcd1ArgPinProbe.caller`: `make` is an invoke before the call.
    let invoke_first = [
        0x04, // 0: iconst_1
        0xbd, 0x00, 0x02, // 1: anewarray Object
        0x4d, // 4: astore_2
        0x1a, // 5: iload_0
        0x2c, // 6: aload_2
        0xb8, 0x00, 0x03, // 7: invokestatic make
        0x2c, // 10: aload_2
        0x1b, // 11: iload_1
        0xb8, 0x00, 0x01, // 12: invokestatic dropAndCheck
        0xac, // 15: ireturn
    ];
    assert!(!replay_exact(&invoke_first, invoke_first.len(), 12, &[]));
    // ... while the FIRST call in it (`make`) has a pure prefix.
    assert!(replay_exact(&invoke_first, invoke_first.len(), 7, &[]));

    // A field store before the call: `counter++` as `getstatic; iconst_1;
    // iadd; putstatic`.
    let store_first = [
        0xb2, 0x00, 0x04, // 0: getstatic counter
        0x04, // 3: iconst_1
        0x60, // 4: iadd
        0xb3, 0x00, 0x04, // 5: putstatic counter
        0x1a, // 8: iload_0
        0xb8, 0x00, 0x01, // 9: invokestatic
        0xac, // 12: ireturn
    ];
    assert!(!replay_exact(&store_first, store_first.len(), 9, &[]));

    // A store AFTER the call, inside a loop around it: the re-run's
    // extent is closed over the loop, so an earlier iteration's store
    // counts.
    let store_in_loop = [
        0x1a, // 0: iload_0            <- loop head
        0xb8, 0x00, 0x01, // 1: invokestatic
        0xb3, 0x00, 0x04, // 4: putstatic
        0xa7, 0xff, 0xf9, // 7: goto 0
    ];
    assert!(!replay_exact(&store_in_loop, store_in_loop.len(), 1, &[]));
    // Without the back edge the store is past the call and harmless.
    let store_after = [
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic
        0xb3, 0x00, 0x04, // 4: putstatic
        0xb1, // 7: return
    ];
    assert!(replay_exact(&store_after, store_after.len(), 1, &[]));

    // A monitor held across the call.
    let locked = [
        0x2a, // 0: aload_0
        0xc2, // 1: monitorenter
        0x2a, // 2: aload_0
        0xb8, 0x00, 0x01, // 3: invokestatic
        0x2a, // 6: aload_0
        0xc3, // 7: monitorexit
        0xb1, // 8: return
    ];
    assert!(!replay_exact(&locked, locked.len(), 3, &[]));
}
