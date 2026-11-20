use super::*;

/// Every slot has its OWN monomorphisation.
///
/// The slot is the only thing that tells one of these helpers from another
/// — it picks the return kind the read is checked against and the synthetic
/// call site the cold arm dispatches through. Two slots sharing an address
/// would not fail to compile and would not fail loudly: a `getAcquire`
/// site returning `J` would run `get`'s `Z` body, read the right field and
/// return the wrong width.
#[test]
pub fn every_slot_is_its_own_distinct_function() {
    let addrs = varhandle_read_direct_fns();
    // addr-keyed: not a heap address -- test-local helper function addresses.
    let unique: std::collections::HashSet<usize> = addrs.iter().copied().collect();
    assert_eq!(
        unique.len(),
        cratonvm_jit::VARHANDLE_READ_SLOTS,
        "slot monomorphisations collapsed onto {} addresses",
        unique.len(),
    );
    assert!(addrs.iter().all(|a| *a != 0));
    for (slot, addr) in addrs.iter().enumerate() {
        assert_eq!(varhandle_read_direct_fn(slot), *addr, "slot {slot}");
    }
}

/// The synthetic call site at a slot describes the SAME (mode, return kind)
/// pair `cratonvm_jit::varhandle_read_helper_slot` decodes that slot as.
///
/// This is the seam between the two crates: the JIT picks a slot from the
/// site's name and descriptor, and the cold arm hands the generic
/// dispatcher this table's entry for that slot. If they disagree, a
/// declined `getAcquire` read is dispatched as a `get` — the same value on
/// this VM, but the wrong method name in every exception and census the
/// dispatch produces, and a real divergence the day the two modes stop
/// sharing a callback.
#[test]
pub fn each_synthetic_call_site_matches_its_slot() {
    for (slot, info) in VARHANDLE_READ_INFOS.iter().enumerate() {
        let mode = cratonvm_jit::varhandle_read_slot_mode(slot);
        assert_eq!(info.class_name, "java/lang/invoke/VarHandle");
        assert_eq!(info.method_name, cratonvm_jit::VARHANDLE_READ_MODES[mode]);
        assert_eq!(
            info.return_type,
            cratonvm_jit::varhandle_read_slot_return(slot)
        );
        assert_eq!(info.invoke_kind, 0, "a read mode is an invokevirtual site");
        // The receiver (the handle) plus one coordinate.
        assert_eq!(info.num_jit_args, 2);
        // And the round trip. The two REFERENCE slots share one descriptor
        // by construction — `REF_STRICT`'s is erased, which is the whole
        // reason `varhandle_read_direct_impl` reproduces W6-1 for it
        // instead of leaving it to the descriptor — so the round trip is
        // asserted to land on the `REF_OBJECT` slot of the SAME mode
        // rather than on `slot` itself.
        let round_trip =
            cratonvm_jit::varhandle_read_helper_slot(info.method_name, info.descriptor);
        let expected = if cratonvm_jit::varhandle_read_slot_is_reference(slot) {
            mode * cratonvm_jit::VARHANDLE_READ_KINDS
                + cratonvm_jit::VARHANDLE_READ_KIND_REF_OBJECT
        } else {
            slot
        };
        assert_eq!(round_trip, Some(expected), "slot {slot}");
    }
}

/// What each erased descriptor is allowed to be, per kind.
///
/// * a PRIMITIVE slot's substitution is unobservable, because the
///   descriptor's only readers — the argument decode,
///   `unbox_poly_return_checked`'s return-type rules and
///   `coerce_native_return` — all see the identical return char;
/// * a `REF_OBJECT` slot's is not a substitution at all: a site of that
///   kind declares exactly `(L…;)Ljava/lang/Object;`, and W6-1 cannot fire
///   at an `Object` return anyway;
/// * a `REF_STRICT` slot's IS erased, and that is exactly why
///   `varhandle_strict_reference_return_check` exists. This row is the one
///   that says the erasure is deliberate rather than an oversight — the
///   pairing that must not be broken silently is "erased descriptor here,
///   W6-1 reproduced there".
///
/// An `[` or `V` return in this table would be a slot the recogniser can
/// never produce, so it is asserted absent rather than handled.
#[test]
pub fn every_synthetic_call_site_carries_its_kinds_erasure() {
    for (slot, info) in VARHANDLE_READ_INFOS.iter().enumerate() {
        assert!(info.descriptor.starts_with("(Ljava/lang/Object;)"));
        assert!(!matches!(info.return_type, b'[' | b'V'));
        let kind = cratonvm_jit::varhandle_read_slot_kind(slot);
        if kind == cratonvm_jit::VARHANDLE_READ_KIND_REF_OBJECT
            || kind == cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT
        {
            assert_eq!(info.return_type, b'L', "slot {slot}");
            assert_eq!(info.descriptor, "(Ljava/lang/Object;)Ljava/lang/Object;");
        } else {
            assert!(
                !matches!(info.return_type, b'L'),
                "{} returns {}",
                info.descriptor,
                info.return_type as char,
            );
        }
    }
    // The pairing, stated as an assertion rather than only in prose: the
    // strict kind is the only one whose descriptor lies, and the file that
    // holds this table also holds the check that makes the lie harmless.
    // A source witness, for the reason `source-witness-tests-read-the-working-tree`
    // gives: `CARGO_MANIFEST_DIR` is `<repo>/vm`, so this reads the working
    // tree and an unbuilt edit still fails here. What it pins is that the
    // erased descriptor above is CALLED-OUT-TO rather than merely declared:
    // deleting the call and keeping the function would leave the strict
    // slot silently unchecked.
    // `vm/src/jit/helpers.rs` is split into `vm/src/jit/helpers/*.rs`;
    // this reads their working-tree concatenation.
    let text = crate::jit::helpers::read_helpers_src_from_disk();
    assert!(
        text.contains("varhandle_strict_reference_return_check(vm, slot, bits)"),
        "REF_STRICT carries an erased descriptor with nothing reproducing W6-1",
    );
}
