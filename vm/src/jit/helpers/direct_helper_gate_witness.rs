use super::*;

/// A gate's capability field must be `classify_native`'s own answer.
///
/// The field exists so the per-call path does not run a string `match`
/// (see [`DirectHelperGate::capability`]); a hand-written copy of an
/// answer is exactly the drift
/// `a-serviceability-predicate-must-mirror-the-dispatch-it-guards` is
/// about, so the copy is checked against the source rather than against a
/// reading of it.
#[test]
pub fn every_direct_helper_gate_mirrors_classify_native() {
    for (name, gate) in ALL_DIRECT_HELPER_GATES {
        assert_eq!(
            gate.capability,
            cratonvm_native_api::capability::classify_native(
                gate.class_name(),
                gate.method_name()
            ),
            "{name} states a capability its triple does not have (or misses \
             one it does): the thin helpers bypass the funnel's \
             `check_native_dispatch_capability`, so this field is the only \
             thing standing between a classified native and a compiled \
             fast path with no gate",
        );
    }
}

/// And today every one of them is `None` -- which is the page's second
/// bullet, answered.
///
/// A SEPARATE assertion from the mirror above, on purpose. The mirror says
/// the field is honest; this says what the honest answer currently is, and
/// therefore that the capability trade the page refuses to make quietly
/// **is not being made**. `jdk/internal/misc/Unsafe` and
/// `java/lang/foreign/MemorySegment` are `RawMemory`, but no gate declared
/// here names either, and `java/nio/*` -- the Panama / `DirectByteBuffer`
/// accessors the page worried about -- is not in `classify_native`'s table
/// at all. So `check_native_dispatch_capability` is a guaranteed `Ok(())`
/// for every triple these helpers stand in front of, and admitting them
/// takes nothing off.
///
/// When this goes red, the gate it names is the one where the trade has to
/// be decided explicitly rather than inherited.
#[test]
pub fn only_the_ten_unsafe_gates_are_capability_classified() {
    // The trade the module block above says is "not being made" for
    // every OTHER gate: these eight are, deliberately, and this test is
    // what turns that from a reading of the source into an assertion.
    // `docs/feature-designs/real-jdk-mode.md`
    // is where the decision (RawMemory, checked in every mode, not
    // narrowed) was made explicit.
    let mut classified: Vec<&str> = ALL_DIRECT_HELPER_GATES
        .iter()
        .filter(|(_, gate)| gate.capability.is_some())
        .map(|(name, _)| *name)
        .collect();
    classified.sort_unstable();
    assert_eq!(
        classified,
        vec![
            "UNSAFE_CAS_INT_GATE",
            "UNSAFE_CAS_LONG_GATE",
            // nio21 wave 3, lane L4: seven more `jdk/internal/misc/Unsafe`
            // rows, `RawMemory` and checked in every mode like the rest.
            "UNSAFE_CAS_REFERENCE_GATE",
            "UNSAFE_FULL_FENCE_GATE",
            "UNSAFE_GET_BYTE_GATE",
            "UNSAFE_GET_CHAR_GATE",
            "UNSAFE_GET_INT_GATE",
            "UNSAFE_GET_INT_VOLATILE_GATE",
            "UNSAFE_GET_LONG_GATE",
            "UNSAFE_GET_LONG_VOLATILE_GATE",
            "UNSAFE_GET_REFERENCE_GATE",
            "UNSAFE_GET_REFERENCE_VOLATILE_GATE",
            "UNSAFE_GET_SHORT_GATE",
            "UNSAFE_PARK_GATE",
            "UNSAFE_PUT_BYTE_GATE",
            "UNSAFE_PUT_CHAR_GATE",
            "UNSAFE_PUT_INT_GATE",
            "UNSAFE_PUT_INT_VOLATILE_GATE",
            "UNSAFE_PUT_LONG_GATE",
            "UNSAFE_PUT_LONG_VOLATILE_GATE",
            "UNSAFE_PUT_REFERENCE_VOLATILE_GATE",
            "UNSAFE_PUT_SHORT_GATE",
            "UNSAFE_UNPARK_GATE",
        ],
        "the set of capability-classified gates changed. If a NEW one \
         joined the eight `Unsafe` accessors, confirm `jit_direct_helper_refused` \
         and `direct_native_bind_admitted` still check it unconditionally \
         of `--real-jdk` before adding it here; if one of the eight left, \
         `classify_native` narrowed and this list (and the page) should \
         say so.",
    );
    for (name, gate) in ALL_DIRECT_HELPER_GATES {
        if classified.contains(name) {
            assert_eq!(
                gate.capability,
                Some(cratonvm_native_api::CapabilityKind::RawMemory),
                "{name} is capability-classified but not as RawMemory",
            );
        }
    }
}

/// Two gates must not share a triple.
///
/// A shared triple is the `NativeCallSite` misuse its own doc names, one
/// level up: the memo is per gate, so two helpers over one gate would be
/// correct only by accident, and two gates over one triple means one of
/// them is asking about a native it does not re-implement. The
/// `ByteBuffer` pair is the near miss -- `dbb_direct_elem_addr` is shared
/// between `get` and `put`, and a single gate there would police the read
/// about `put`.
#[test]
pub fn every_direct_helper_gate_polices_its_own_triple() {
    let mut seen: Vec<((&str, &str, &str), &str)> = Vec::new();
    for (name, gate) in ALL_DIRECT_HELPER_GATES {
        let triple = gate.triple();
        if let Some((_, other)) = seen.iter().find(|(t, _)| *t == triple) {
            panic!("{name} and {other} both police {triple:?}");
        }
        seen.push((triple, name));
    }
    assert_eq!(seen.len(), ALL_DIRECT_HELPER_GATES.len());
}

/// Every declared gate is consulted, and consulted by name.
///
/// A gate nobody passes to [`jit_direct_helper_refused`] is a policy
/// question that is never asked -- the `check-orphan-instruments.sh`
/// shape, and indistinguishable at run time from a gate that always
/// admits.
#[test]
pub fn every_direct_helper_gate_is_consulted() {
    let src = crate::jit::helpers::HELPERS_SRC;
    for (name, _) in ALL_DIRECT_HELPER_GATES {
        let uses = src.matches(&format!("&{name}")).count();
        assert!(
            uses >= 1,
            "{name} is declared and never consulted: a gate that no helper \
             passes to `jit_direct_helper_refused` is a policy question \
             nobody asks",
        );
    }
}

/// The gate asks the tail's question, in the tail's terms.
///
/// The sibling of `jit_fast_native_admission_asks_the_tails_question`, and
/// a source witness for the same reason that one is: the defect is that a
/// refusal here routes the call to `vm_exec::invoke_or_native`, which then
/// runs the very native the helper refused to re-implement. The bytes that
/// came out were identical. What changed was ~3x on the loop the page
/// measures, and a counter that reported strict mode blocking something it
/// had not blocked.
#[test]
pub fn the_direct_helper_gate_asks_the_tails_question() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("fn direct_helper_refusal(")
        .expect("direct_helper_refusal must exist");
    let after = &src[start..];
    let end = after[1..]
        .find("\n/// The one sanctioned way for a compile door")
        .expect("`admit_direct_native_entry`'s doc comment must follow it")
        + 1;
    let decision = &after[..end];
    assert!(
        decision.contains("jit_fast_native_bytecode_terms("),
        "the step-3 input must come from the predicate that mirrors \
         `invoke_or_native`, or the guard refuses a Bridge the tail then \
         runs -- which is the whole defect",
    );
    assert!(
        !decision.contains("jit_fast_native_has_bytecode("),
        "the raw hierarchy probe is NOT the tail's question: \
         `invoke_or_native` computes it only for a SyntheticStub on an \
         allow-listed class",
    );
    assert!(
        decision.contains("NativeKind::SyntheticStub) => true"),
        "a fabrication must still be refused outright",
    );
    assert!(
        decision.contains("None => true"),
        "an unregistered triple must refuse: the tail runs the real bytes \
         and a VM-side body the registry never heard of cannot be audited",
    );
    assert!(
        decision.contains("thin_helper_capability_admits("),
        "a capability-classified triple must ask the capability question \
         before anything else admits it",
    );
    // ...and the per-call answer must be the funnel's own check, not a copy
    // of it: the gate a thin helper stands in for is
    // `check_native_dispatch_capability`, recording included.
    let start = src
        .find("fn thin_helper_capability_admits(")
        .expect("thin_helper_capability_admits must exist");
    let body = &src[start..start + src[start..].find("\n}\n").expect("closed item")];
    assert!(
        body.contains("check_native_dispatch_capability_for_slot("),
        "the per-call capability answer must come from the funnel's own \
         function, or a helper can admit what the funnel would deny",
    );
    assert!(
        decision.contains("jdk_only_enforce_shadow()"),
        "an armed `CRATONVM_ENFORCE_NATIVE_SHADOW` must refuse every \
         guarded helper: the dial is keyed on the RECEIVER class and a \
         gate names the implementing one, so the two can disagree, and the \
         dial's whole purpose is to price a retirement exactly",
    );
}

/// Bind time and call time are the SAME decision.
///
/// The page's third bullet asks that a site which will refuse on every
/// call should not bind at all; the converse matters just as much, and
/// both reduce to the two halves agreeing. They agree because there is one
/// function.
#[test]
pub fn the_two_halves_of_the_policy_are_one_predicate() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    for caller in [
        "fn direct_native_bind_admitted(",
        "fn direct_helper_tail_would_refuse(",
    ] {
        let start = src
            .find(caller)
            .unwrap_or_else(|| panic!("{caller} must exist"));
        let after = &src[start..];
        let end = after
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{caller} must be a closed item"));
        assert!(
            after[..end].contains("direct_helper_refusal("),
            "{caller} must route through the shared decision, or the \
             bind-time and call-time answers can differ and a bound helper \
             refuses on every call -- or, worse, the reverse",
        );
    }
}

/// A `Compatible` VM never consults the registry and never touches either
/// counter -- the property that keeps `--compatible` unmoved, which is
/// half the page's acceptance criterion.
#[test]
pub fn compatible_mode_short_circuits_before_the_gate() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert!(!crate::vm::dispatch_policy(&shared).is_jdk_only());
    let before = (
        jit_direct_helper_jdk_only_refusals(&shared),
        jit_direct_helper_jdk_only_admissions(&shared),
    );
    // JIT round 20 wave 14 (lane cjavac): the exact-`HashMap` gate answers
    // the `jdk-hash-collections` row (no registry kind, no counter), so its
    // verdict here is that row's; the counters below must still not move.
    assert_eq!(
        jit_direct_helper_refused(&shared, &HASHMAP_GET_GATE),
        hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE)
    );
    assert!(!jit_direct_helper_refused(&shared, &MD_UPDATE_BYTE_GATE));
    assert_eq!(
        (
            jit_direct_helper_jdk_only_refusals(&shared),
            jit_direct_helper_jdk_only_admissions(&shared),
        ),
        before,
        "the `Compatible` arm must not reach the counters at all",
    );
}

/// The decision table: three registered kinds, "unregistered", and a
/// capability-classified triple.
///
/// Asserted on [`direct_helper_refusal`] rather than through a strict-mode
/// `SharedVm`, because a `--real-jdk` VM cannot be constructed in-process
/// here at all -- `vm_init` refuses `jdk-only` paired with the synthetic
/// class library, and a real JDK is not a test dependency. That costs
/// nothing: both halves of the policy reach this function having already
/// established `JdkOnly` for themselves, and
/// `the_two_halves_of_the_policy_are_one_predicate` is what pins that they
/// reach it at all. The mode is the callers' term; the table is this
/// function's.
///
/// The `Bridge` row is the page. Before 2026-09-22 the answer there was
/// "refuse", the site fell through to `vm_exec::invoke_or_native`, and
/// that function ran the same `Bridge` -- so the refusal bought no policy
/// and cost a three-string registry hash per call.
#[test]
pub fn the_refusal_table_admits_a_bridge_the_tail_would_run_and_nothing_else() {
    use cratonvm_native_api::{CapabilityKind, NativeKind};

    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (class, method, descriptor) = MD_UPDATE_BYTE_GATE.triple();
    let refuse = |capability, kind| {
        direct_helper_refusal(&shared, class, method, descriptor, capability, kind)
    };

    assert!(
        !refuse(None, Some(NativeKind::Bridge)),
        "a Bridge with no concrete bytecode behind it is what the tail \
         runs, so refusing the helper only buys a slower route to the same \
         native -- the defect this page names",
    );
    assert!(
        !refuse(None, Some(NativeKind::Intrinsic)),
        "the reviewed exception may shadow bytecode",
    );
    assert!(
        refuse(None, Some(NativeKind::SyntheticStub)),
        "a fake may not be invoked under --real-jdk; the tail raises that, \
         so the helper must hand it the call",
    );
    assert!(
        refuse(None, None),
        "an unregistered triple must refuse: the tail runs the real bytes \
         and a VM-side body the registry never heard of cannot be audited",
    );
    // A classified triple asks the capability question -- the funnel's own
    // `check_native_dispatch_capability_for_slot` per call, a side-effect
    // free "could the policy deny it" at bind time -- and under the policy
    // a VM boots with (Permissive) nothing is denied. Until 2026-09-24 this
    // row asserted a blanket refusal, which left every `Unsafe` helper bound
    // nowhere; see `thin_helper_capability_admits`.
    let raw = CapabilityKind::RawMemory;
    assert!(
        !refuse(
            Some((raw, CapabilityQuestion::Bind)),
            Some(NativeKind::Bridge)
        ),
        "Permissive denies nothing, so a classified Bridge binds",
    );
    assert!(
        !refuse(
            Some((raw, CapabilityQuestion::Call(None))),
            Some(NativeKind::Bridge)
        ),
        "and each call passes the funnel's own check",
    );
}

/// Under `Enforce` the classified helpers are refused at BOTH halves unless
/// the policy grants the kind -- the gate the funnel runs, not a weaker
/// copy of it, and the same fail-closed `Scope::Any` rule
/// `enforce_admits_a_dispatch_covered_by_an_unscoped_grant` pins there.
#[test]
pub fn a_classified_helper_is_refused_exactly_when_enforce_would_deny_the_funnel() {
    use cratonvm_native_api::{
        install_capabilities, Capability, CapabilityKind, CapabilityMode, CapabilitySet,
        NativeKind, VmId,
    };

    let (class, method, descriptor) = UNSAFE_CAS_INT_GATE.triple();
    let refused = |shared: &SharedVm, q: CapabilityQuestion| {
        direct_helper_refusal(
            shared,
            class,
            method,
            descriptor,
            Some((CapabilityKind::RawMemory, q)),
            Some(NativeKind::Bridge),
        )
    };
    let with_policy = |set: CapabilitySet| {
        let mut shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let caps = std::sync::Arc::new(set);
        shared
            .natives
            .native_methods
            .set_capabilities(std::sync::Arc::clone(&caps));
        install_capabilities(caps);
        shared
    };

    let probe = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let vm = VmId::from_raw(probe.vm_identity);
    let denied = with_policy(CapabilitySet::new(vm, CapabilityMode::Enforce));
    assert!(
        refused(&denied, CapabilityQuestion::Bind),
        "Enforce, no grant: no bind"
    );
    assert!(
        refused(&denied, CapabilityQuestion::Call(None)),
        "Enforce, no grant: a bound site declines every call to the funnel, \
         which raises the denial",
    );

    let mut set = CapabilitySet::new(vm, CapabilityMode::Enforce);
    set.grant(Capability::parse_grant("raw-memory:*").expect("grant parses"));
    let granted = with_policy(set);
    assert!(
        !refused(&granted, CapabilityQuestion::Bind),
        "an unscoped grant binds"
    );
    assert!(
        !refused(&granted, CapabilityQuestion::Call(None)),
        "an unscoped grant admits"
    );

    // `Audit` never denies, so it binds; its per-call check still records.
    let audit = with_policy(CapabilitySet::new(vm, CapabilityMode::Audit));
    assert!(!refused(&audit, CapabilityQuestion::Bind));
    assert!(!refused(&audit, CapabilityQuestion::Call(None)));
}

/// nio21 wave 2 L1 (edit D of the r21 CHM-row page): the `--compatible`
/// arm refuses the segmented `ConcurrentHashMap.get` helper exactly when
/// the `jdk-concurrent-hash-map` row is in force, and no other gate.
#[test]
pub fn chm_gate_refuses_only_under_the_compat_chm_row() {
    use cratonvm_types::HotSpotAlignment;
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    let regs = &mut shared.natives.native_methods;
    regs.set_real_jdk(true);
    let off = regs
        .hotspot_alignments()
        .with(HotSpotAlignment::JdkConcurrentHashMap, false);
    regs.set_hotspot_alignments(off);
    assert!(!chm_gate_under_jdk_model(
        &shared,
        &CONCURRENT_HASHMAP_GET_GATE
    ));
    let regs = &mut shared.natives.native_methods;
    regs.set_hotspot_alignments(off.with(HotSpotAlignment::JdkConcurrentHashMap, true));
    assert!(chm_gate_under_jdk_model(
        &shared,
        &CONCURRENT_HASHMAP_GET_GATE
    ));
    assert!(!chm_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
}

/// JIT round 20 wave 14 (lane cjavac): the `--compatible` arm refuses the
/// exact-`HashMap` get/put helpers exactly when the `jdk-hash-collections`
/// row is in force over a real JDK, and no other gate; on the synthetic
/// library (no real JDK) the row is not in force and the helpers stay.
#[test]
pub fn hashmap_gates_refuse_only_under_the_compat_hash_collections_row() {
    use cratonvm_types::HotSpotAlignment;
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    assert!(!crate::vm::dispatch_policy(&shared).is_jdk_only());
    let regs = &mut shared.natives.native_methods;
    let on = regs
        .hotspot_alignments()
        .with(HotSpotAlignment::JdkHashCollections, true);
    regs.set_hotspot_alignments(on);
    regs.set_real_jdk(false);
    assert!(!hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
    let regs = &mut shared.natives.native_methods;
    regs.set_real_jdk(true);
    assert!(hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
    assert!(hashmap_gate_under_jdk_model(&shared, &HASHMAP_PUT_GATE));
    assert!(!hashmap_gate_under_jdk_model(
        &shared,
        &CONCURRENT_HASHMAP_GET_GATE
    ));
    assert!(jit_direct_helper_refused(&shared, &HASHMAP_GET_GATE));
    let regs = &mut shared.natives.native_methods;
    regs.set_hotspot_alignments(on.with(HotSpotAlignment::JdkHashCollections, false));
    assert!(!hashmap_gate_under_jdk_model(&shared, &HASHMAP_GET_GATE));
    assert!(!hashmap_gate_under_jdk_model(&shared, &HASHMAP_PUT_GATE));
}
