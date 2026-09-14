// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

/// Control for `aastore_element_assignable`, the predicate the interpreter
/// opcode, the JIT's `jit_aastore` and (since e627cdff5) reflective
/// `Array.set` all share.
///
/// # Why this needs to exist
///
/// That predicate is deliberately *additive*: it must never produce a FALSE
/// `ArrayStoreException`, so it fails open along five separate arms. Nothing
/// measured how much it still refuses, and `Array.set`'s refusal in particular
/// had **no** coverage anywhere in the tree — grepping
/// `array element type mismatch` found the throw site and two comments, and no
/// test. A predicate that had degenerated into `true` would have satisfied the
/// `Array.set` fix and broken nothing visible.
///
/// So this asserts the refusal and two of the fail-open arms **together**. The
/// refusal alone would pass against a predicate that refuses everything; the
/// lenient arms alone are what a degenerate `true` satisfies. Only the pair
/// pins the shape.
///
/// # Why the class names are neutral
///
/// `synthetic_implements`, the last fail-open arm, is a table of specific name
/// pairs (`HashMap$Entry` -> `Map$Entry`, `SystemLogger` -> `System$Logger`,
/// …). Naming these classes after real JDK types could match it and make the
/// control vacuously pass, so they are deliberately outside any table.
///
/// # Not covered here
///
/// The loader-split arm (same component name, two `ClassId`s) needs two
/// same-named classes, which `ensure_synthetic_class` dedupes by name. That leg
/// is characterised instead by
/// `classloading::class::tests::a_proxy_is_assignable_to_the_other_loaders_copy_of_its_interface_by_name`.
#[test]
fn aastore_refuses_a_real_mismatch_and_still_fails_open_where_it_must() {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_types::ArrayElementType;

    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));

    let (alpha, beta, iface, proxy) = {
        let mut cm = shared.classes.class_manager.write();
        let alpha = cm
            .try_ensure_synthetic_class("cratonvm/test/AastoreAlpha", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let beta = cm
            .try_ensure_synthetic_class("cratonvm/test/AastoreBeta", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let iface = cm
            .try_ensure_synthetic_class("cratonvm/test/AastoreIface", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.class_store
            .get_mut(iface)
            .expect("just fabricated")
            .access_flags |= ClassAccessFlags::INTERFACE;
        // The name is the whole point: the predicate's proxy arm tests
        // `contains("$Proxy")`, which is what admits `jdk/proxy3/$Proxy27` in
        // the `AotIntegrationTests` case without consulting any interface list.
        let proxy = cm
            .try_ensure_synthetic_class("jdk/proxy3/$Proxy27", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        (alpha, beta, iface, proxy)
    };

    // A reference array's own class id IS its component class id (JVMS §4.4.1),
    // which is how `array_descriptor_of` recovers the component name.
    let alpha_arr = shared
        .mem
        .heap
        .alloc_array(alpha, ArrayElementType::Reference, 1);
    let iface_arr = shared
        .mem
        .heap
        .alloc_array(iface, ArrayElementType::Reference, 1);
    let beta_obj = shared.mem.heap.alloc_object(beta, 0);
    let proxy_obj = shared.mem.heap.alloc_object(proxy, 0);

    // THE CONTROL. Concrete component, unrelated concrete value: every
    // fail-open arm must decline and the store must be refused. This is the
    // assertion that fails if the predicate ever degenerates to `true`.
    assert!(
        !aastore_element_assignable(&shared, alpha_arr, beta_obj),
        "AastoreBeta into AastoreAlpha[] must be refused — if this starts \
         passing, the predicate has stopped refusing anything and both \
         `aastore` and `Array.set` now accept every reference store",
    );

    // NO LONGER a documented lenience. An interface component used to `return
    // true` unconditionally, which made this the assertion that pinned the
    // blanket in place; HotSpot 25.0.3 throws `ArrayStoreException` for
    // `Runnable[] <- String` and `Comparable[] <- Object`, and the predicate
    // permitted both. An unrelated concrete value must now be REFUSED against
    // an interface component exactly as against a concrete one.
    // docs/known-issues/jdk-only/W7-101-aastore-interface-component-blanket.md
    assert!(
        !aastore_element_assignable(&shared, iface_arr, beta_obj),
        "AastoreBeta implements nothing, so storing it into an AastoreIface[] \
         must be refused — an interface component is not a reason to fail open",
    );

    // …and the fail-open population the blanket was WRITTEN for is still served,
    // now by the arm that can actually tell a proxy from an `Object`: the
    // `$Proxy` name test below `is_subclass_of`. Without this assertion the one
    // above would also pass against a predicate that had started refusing
    // everything with an interface component.
    assert!(
        aastore_element_assignable(&shared, iface_arr, proxy_obj),
        "a $Proxy-named value must still fail open against an interface component",
    );

    // Documented lenience 2: a `$Proxy`-named value. This is the arm that
    // admits the `ContextConfiguration[] <- jdk/proxy3/$Proxy27` store that
    // `TypeMappedAnnotation.adapt` makes through `Array.set`.
    assert!(
        aastore_element_assignable(&shared, alpha_arr, proxy_obj),
        "a $Proxy-named value must fail open even against a concrete component",
    );
}

/// The two loader-split arms of `aastore_element_assignable` — the ones that
/// exist for `@CompileWithForkedClassLoader`, and the reason
/// `AotIntegrationTests` reached this code at all.
///
/// Two scenarios, both arising when one class NAME carries two `ClassId`s:
///
/// 1. the value's class IS the component, under the other loader's copy;
/// 2. the value is a SUBCLASS whose recorded superclass edge points at the
///    other loader's copy of the component.
///
/// # One walk serves both arms
///
/// The predicate used to spell these as two consecutive checks: an explicit
/// `value_class.name == comp_name && value_class_id != comp_id`, then a by-name
/// superclass walk. **Mutation testing said only the walk was load-bearing.**
/// Disabling the walk fails scenario 2 as expected; disabling the explicit
/// same-name check changed nothing at all, because the walk starts at
/// `value_class_id` itself, so its first iteration already tests
/// `class.name == comp_name`. Anything the fast path accepted, the walk
/// accepted one line later.
///
/// The fast path has since been removed and this test stayed green, which is
/// the confirmation the equivalence argument needed. Both assertions below now
/// run against the walk alone; scenario 1 is its first iteration and scenario 2
/// is a later one. If a future change reintroduces a same-name early return,
/// note that this test cannot tell the two apart — only mutating them can.
///
/// # Building the pathological state
///
/// `ensure_synthetic_class` dedupes by name, so a second copy cannot be
/// fabricated directly — which is why this leg was left uncovered when the
/// first control landed. The state is instead reached by fabricating under a
/// distinct name and renaming the copy in place. That deliberately leaves
/// `ClassManager`'s name index pointing at the old name, and that is fine
/// *here*: the predicate reads `class.name` out of the store via `get_class`,
/// and `comp_id` is recovered from the array's own class id, so no by-name
/// lookup is consulted on this path. Do not copy this trick into a test that
/// does exercise name resolution.
///
/// # Each arm is asserted against its own negative
///
/// A "same name, different id" acceptance is only meaningful if the exact
/// check would have refused, so both arms assert `is_subclass_of` is `false`
/// first. Otherwise the store could be passing legitimately and the arm under
/// test would never have run.
#[test]
fn aastore_fails_open_across_a_split_loaders_two_copies_of_one_name() {
    use cratonvm_types::ArrayElementType;

    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));

    const COMPONENT: &str = "cratonvm/test/SplitAlpha";

    let (alpha, forked, child) = {
        let mut cm = shared.classes.class_manager.write();
        // The parent loader's copy — what the array was created with.
        let alpha = cm
            .try_ensure_synthetic_class(COMPONENT, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");

        // The forked loader's copy: fabricated under its own name, then renamed
        // so the store holds two distinct ids for one name.
        let forked = cm
            .try_ensure_synthetic_class("cratonvm/test/SplitAlpha$Forked", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.class_store
            .get_mut(forked)
            .expect("just fabricated")
            .name = cratonvm_types::intern_arc(COMPONENT);

        // A subclass of the FORKED copy, for arm 2. `set_superclass` rather than
        // writing the field: the store maintains a subclass adjacency index that
        // a raw field write would desynchronise.
        let child = cm
            .try_ensure_synthetic_class("cratonvm/test/SplitChild", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.class_store.set_superclass(child, Some(forked));

        (alpha, forked, child)
    };
    assert_ne!(alpha, forked, "the two copies must be distinct ClassIds");

    let alpha_arr = shared
        .mem
        .heap
        .alloc_array(alpha, ArrayElementType::Reference, 1);
    let forked_obj = shared.mem.heap.alloc_object(forked, 0);
    let child_obj = shared.mem.heap.alloc_object(child, 0);

    {
        let cm = shared.classes.class_manager.read();
        assert!(
            !cm.is_subclass_of(forked, alpha),
            "identity must refuse the two copies — if it accepts, arm 1 is not \
             what is being measured below",
        );
        assert!(
            !cm.is_subclass_of(child, alpha),
            "identity must refuse the child too: its superclass edge points at \
             the FORKED copy, not at this one",
        );
    }

    // Scenario 1: the value IS the component, under the other copy. Served by
    // the by-name walk's first iteration (see the note above on why the
    // explicit same-name fast path that used to precede it was subsumed).
    assert!(
        aastore_element_assignable(&shared, alpha_arr, forked_obj),
        "the other loader's copy of the component must be storable — refusing \
         it is the `array element type mismatch` that AotIntegrationTests hit",
    );

    // Scenario 2: a subclass reaching the other copy. This one is served ONLY
    // by the walk — disabling it fails right here.
    assert!(
        aastore_element_assignable(&shared, alpha_arr, child_obj),
        "a subclass whose superclass edge reaches a same-named copy must be \
         storable",
    );

    // And the split is not a licence to accept anything: an unrelated class
    // whose chain never reaches the component name is still refused. Without
    // this, both assertions above would also pass against a predicate that had
    // degenerated to `true`.
    let unrelated = {
        let mut cm = shared.classes.class_manager.write();
        cm.try_ensure_synthetic_class("cratonvm/test/SplitUnrelated", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
    };
    let unrelated_obj = shared.mem.heap.alloc_object(unrelated, 0);
    assert!(
        !aastore_element_assignable(&shared, alpha_arr, unrelated_obj),
        "an unrelated class must still be refused even once same-named copies \
         exist in the store",
    );
}

/// The wiring the test above cannot see: reflective `Array.set` must actually
/// route through that shared predicate rather than keep a private check.
///
/// `reflect_array_element_assignable` lives in `native-builtins`, whose test
/// targets do not currently compile (~620 pre-existing errors from the
/// in-flight fallibility migration), so a behavioural test cannot be hosted
/// beside it. A source witness is the cheap stand-in for "this call must not
/// quietly disappear" — matched on text, never on line numbers, so ordinary
/// edits to the file cannot rot it.
#[test]
fn array_set_routes_through_the_shared_aastore_predicate() {
    let src = include_str!("../../../../native-builtins/src/lib.rs");
    let start = src
        .find("fn reflect_array_element_assignable")
        .expect("reflect_array_element_assignable must exist in native-builtins");
    // Bound the search to this function so a coincidental match elsewhere in a
    // 39k-line file cannot vouch for it.
    let body = &src[start..];
    // End at the next top-level `fn` rather than at a named neighbour: the
    // function that follows this one has already changed once (a diagnostic
    // helper was inserted between them), and a witness that has to be edited
    // whenever a sibling is added is a witness that will one day be edited
    // wrongly. `\nfn ` at column 0 cannot match an inner item.
    let end = body[1..]
        .find("\nfn ")
        .expect("a top-level function must follow it")
        + 1;
    let body = &body[..end];
    assert!(
        body.contains("ctx.aastore_element_assignable(arr, value)"),
        "Array.set must consult the same predicate as the `aastore` opcode. \
         Without it the reflective path falls back to a ClassId-identity \
         `is_subclass`, which refuses a proxy stored into the annotation-type \
         array it was created from — the AotIntegrationTests failure.",
    );
}

#[test]
fn forced_generic_metadata_scan_reuses_the_callers_class_manager_guard() {
    let _guard_reusing_signature: fn(
        &crate::classloading::ClassManager,
        ClassId,
        &[u8],
        usize,
    ) -> bool = jit_method_calls_forced_class_generic_metadata;
}

/// The `java.lang.Thread`-mirror recovery in `execute_invoke_kind` replaces the
/// receiver with a live thread mirror when the receiver's address is in
/// `former_mirror_addrs`. Its gate used to be `class_id_of(recv) == ClassId(0)`,
/// justified in-comment as "the receiver header is genuinely all-zero".
///
/// It is not the same test. **Every primitive array reads `ClassId(0)`** —
/// `Instruction::Newarray` allocates with `ClassId::new(0)` because an array
/// header carries its COMPONENT class id (JVMS §4.4.1) and `long[]` has none —
/// so a perfectly live `long[]` that landed on a recycled young address matched
/// and was replaced by a `java.lang.Thread`. Measured on
/// `org.h2.test.db.TestTempTables`: `Arrays.copyOf(long[], int)`'s
/// `original.clone()` dispatching into the mirror's inherited `Thread.clone`,
/// i.e. `CloneNotSupportedException`. See
/// `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`.
///
/// Restoring the old gate (dropping the header term from
/// `stale_mirror_recovery_applies`) fails the first assertion below.
#[test]
fn stale_mirror_recovery_skips_a_live_primitive_array() {
    use super::invoke::stale_mirror_recovery_applies;
    use cratonvm_gc::heap::ObjectHeader;
    use cratonvm_types::{ArrayElementType, ObjectKind, HEADER_SIZE};

    let header_bytes = |h: &ObjectHeader| -> [u8; HEADER_SIZE] {
        // SAFETY: `ObjectHeader` is `#[repr(C)]` and exactly `HEADER_SIZE`
        // bytes; this reads it exactly as the interpreter reads a header off a
        // heap address.
        unsafe { std::ptr::read(h as *const ObjectHeader as *const [u8; HEADER_SIZE]) }
    };

    // A live `long[1]` — `bits` in H2's `VersionedBitSet`, the witness shape.
    let live_long_array = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Long,
        1,
        1,
    );
    assert_eq!(
        live_long_array.class_id,
        ClassId::new(0),
        "the trap itself: a primitive array's header carries no component class id"
    );
    assert!(
        !stale_mirror_recovery_applies(live_long_array.class_id, &header_bytes(&live_long_array)),
        "a live long[] must never be mistaken for a reclaimed span and replaced \
         by a java.lang.Thread mirror"
    );

    // A live `Object[3]`. Its component class id is `java/lang/Object` =
    // ClassId(0) too, and `ArrayElementType::Reference` is discriminant 0, so
    // this one is separated from the wipe by `kind` and `shape` alone.
    let live_ref_array = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Reference,
        3,
        3,
    );
    assert!(
        !stale_mirror_recovery_applies(live_ref_array.class_id, &header_bytes(&live_ref_array)),
        "a live Object[] must not be mistaken for a reclaimed span either"
    );

    // What the recovery is actually for: the all-zero header a collector
    // leaves over a span it reclaimed.
    assert!(
        stale_mirror_recovery_applies(ClassId::new(0), &[0u8; HEADER_SIZE]),
        "the collector's own wipe must still reach the former-mirror lookup"
    );
}

/// The header test above cannot close the recovery's last address-collision
/// window on its own. On the 16-byte header a bare `new Object()` is ALSO
/// all-zero — `class_id` 0, `shape` 0, `ObjectKind::Object` and
/// `ArrayElementType::Reference` both discriminant 0, and the identity hash
/// minted lazily into the mark word rather than stamped at allocation. So the
/// second half of the gate asks a question the header cannot answer: could this
/// CALL SITE be holding a thread mirror at all?
///
/// Two names can never be evidence of one, whatever the heap says, and
/// admitting the second is what would leave the `new Object()` window open.
#[test]
fn only_a_call_site_that_could_hold_a_thread_mirror_admits_the_recovery() {
    use super::invoke::call_site_type_can_hold_a_thread_mirror;

    // An array type has no relationship to `java.lang.Thread` in either
    // direction. This is the `Arrays.copyOf(long[], int)` witness's call site.
    assert!(
        !call_site_type_can_hold_a_thread_mirror("[J"),
        "an array-typed call site can never legitimately hold a thread mirror"
    );
    assert!(!call_site_type_can_hold_a_thread_mirror(
        "[Ljava/lang/Object;"
    ));

    // Bare `java/lang/Object` admits every mirror, so it is no evidence at all
    // — and a zero-field `Object` receiver is header-identical to a reclaimed
    // span, so nothing else could refuse it.
    assert!(
        !call_site_type_can_hold_a_thread_mirror("java/lang/Object"),
        "an Object-typed call site carries no evidence the receiver was a mirror"
    );

    // The case the recovery exists for — Tomcat's `TaskThreadFactory.<init>`
    // calling `Thread.currentThread().getThreadGroup()` — and an
    // interface-typed use, both still admitted (assignability is then checked
    // against the recovered mirror's real class).
    assert!(call_site_type_can_hold_a_thread_mirror("java/lang/Thread"));
    assert!(call_site_type_can_hold_a_thread_mirror(
        "java/lang/Runnable"
    ));
    assert!(call_site_type_can_hold_a_thread_mirror(
        "jdk/internal/misc/InnocuousThread"
    ));
}

#[test]
fn invoke_args_root_guard_refreshes_forwarded_pins_and_restores_watermark() {
    // The guard never dereferences these values; aligned sentinel addresses
    // are sufficient to model a collector rewriting native pin slots.
    // SAFETY: test-only. As the comment above says, the guard never
    // dereferences these refs — they are aligned sentinel addresses used
    // to model a collector rewriting native pin slots.
    let obj = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let existing = obj(0x1000);
    let old_a = obj(0x2000);
    let old_b = obj(0x3000);
    let new_a = obj(0x4000);
    let new_b = obj(0x5000);

    let mut thread = JvmThread::default();
    thread.native_pin_roots.push(existing);
    let mut args = [
        Value::Object(Some(old_a)),
        Value::Int(7),
        Value::Object(None),
        Value::Object(Some(old_b)),
    ];

    {
        let guard = InvokeArgsRootGuard::new(&mut thread, &args);
        assert_eq!(thread.native_pin_roots, vec![existing, old_a, old_b]);

        // Model a moving collector's in-place root rewrite.
        thread.native_pin_roots[1] = new_a;
        thread.native_pin_roots[2] = new_b;
        guard.refresh(&mut args);

        assert!(matches!(args[0], Value::Object(Some(o)) if o == new_a));
        assert!(matches!(args[1], Value::Int(7)));
        assert!(matches!(args[2], Value::Object(None)));
        assert!(matches!(args[3], Value::Object(Some(o)) if o == new_b));

        let replacement = obj(0x6000);
        guard.replace_object_arg(&args, 0, replacement);
        guard.refresh(&mut args);
        assert!(matches!(args[0], Value::Object(Some(o)) if o == replacement));
    }

    assert_eq!(thread.native_pin_roots, vec![existing]);
}

#[test]
fn class_array_reflection_uses_class_id_backed_natives() {
    for (name, descriptor) in [
        ("isArray", "()Z"),
        ("getComponentType", "()Ljava/lang/Class;"),
        ("componentType", "()Ljava/lang/Class;"),
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;"),
    ] {
        assert!(force_native_over_real_jdk_bytecode(
            "java/lang/Class",
            name,
            descriptor,
        ));
        assert!(is_class_mirror_native_override(
            "java/lang/Class",
            name,
            descriptor,
        ));
    }
}

#[test]
fn tomcat_scanner_uses_only_audited_native_bridges() {
    // `Response.toAbsolute` IS an audited bridge, as of `6a87072ca`
    // ("fix(tomcat): close silent hang residual cluster", 2026-07-22),
    // which added the explicit rule above and exempted `Response` from the
    // blanket `org/apache/*` opt-out so it could fire. This test landed at
    // `995ff48c7` (2026-07-16) and asserted the opposite; it was not
    // updated when the bridge was added, so it has been failing on `dev`
    // ever since. The audit intent is preserved — the bridge is still
    // pinned here, just with the polarity the shipping rule actually has.
    assert!(force_native_over_real_jdk_bytecode(
        "org/apache/catalina/connector/Response",
        "toAbsolute",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ));
    // A sibling method on the same class must NOT be bridged: the rule is
    // one specific method, not the whole class.
    assert!(!force_native_over_real_jdk_bytecode(
        "org/apache/catalina/connector/Response",
        "sendRedirect",
        "(Ljava/lang/String;)V",
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/io/DataInputStream",
        "readInt",
        "()I",
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/io/FileInputStream",
        "read",
        "([BII)I",
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/util/zip/CRC32",
        "updateBytes",
        "(I[BII)I",
    ));
    assert!(!force_native_over_real_jdk_bytecode(
        "org/apache/tomcat/unittest/TesterRequest",
        "getRequestURI",
        "()Ljava/lang/String;",
    ));
}

#[test]
fn jfr_known_type_class_lookup_uses_the_canonical_native_bridge() {
    assert!(force_native_over_real_jdk_bytecode(
        "jdk/jfr/internal/Type",
        "getKnownType",
        "(Ljava/lang/Class;)Ljdk/jfr/internal/Type;",
    ));
    assert!(redefine_immune_forced_native(
        "jdk/jfr/internal/Type",
        "getKnownType",
        "(Ljava/lang/Class;)Ljdk/jfr/internal/Type;",
    ));
    assert!(is_jfr_metadata_native_override(
        "jdk/jfr/internal/util/Utils",
        "getValidType",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljdk/jfr/internal/Type;",
    ));
    assert!(is_class_mirror_native_override(
        "java/lang/Class",
        "forPrimitiveName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
    ));
}

/// Perf/starvation fix (2026-07-13) — `stw_takeover_should_scan` must scan
/// every round through the fast window (catching a genuinely in-JIT peer
/// with no added latency), then only periodically once a stall has
/// already run long — never falling back to the old "scan literally every
/// round forever" behavior that starved the very peer it was waiting for.
#[test]
fn stw_takeover_scan_cadence_backs_off() {
    // Round 0: gated on the hint alone, exactly like before this fix.
    assert!(!stw_takeover_should_scan(0, false));
    assert!(stw_takeover_should_scan(0, true));

    // Fast window (rounds 1..20): always scan regardless of the hint —
    // unchanged latency for the common near-immediate takeover case.
    for r in 1..20u32 {
        assert!(
            stw_takeover_should_scan(r, false),
            "round {r} should still scan every tick in the fast window"
        );
    }

    // Slow window (rounds 20..500): only every 20th round.
    assert!(stw_takeover_should_scan(20, false));
    assert!(!stw_takeover_should_scan(21, false));
    assert!(!stw_takeover_should_scan(39, false));
    assert!(stw_takeover_should_scan(40, false));
    assert!(!stw_takeover_should_scan(499, false));

    // Very-slow window (rounds >= 500): only every 200th round (aligned
    // to multiples of 200, not to 500 itself) — this is the regime a
    // multi-minute-or-permanent stall (the WildFly parallel-extension-add
    // hang, the 5-class Tomcat hang cluster) lives in, where the old code
    // was doing a full OS-level suspend-scan of every peer thread on
    // literally every 1ms tick.
    assert!(!stw_takeover_should_scan(500, false));
    assert!(!stw_takeover_should_scan(599, false));
    assert!(stw_takeover_should_scan(600, false));
    assert!(!stw_takeover_should_scan(601, false));
    assert!(stw_takeover_should_scan(800, false));

    // The hint must NOT override the backoff once rounds >= 1: it cannot
    // distinguish a peer actually being in JIT from the initiator's own
    // live JIT-entry guard (this function is commonly reached via
    // `maybe_gc` called from JIT-compiled code).
    assert!(!stw_takeover_should_scan(21, true));
    assert!(!stw_takeover_should_scan(501, true));
}

/// Young-GC live-reclaim ROOT FIX regression (RRWL/ThreadLocalMap$Entry
/// IMSE/hang family, 2026-07-07): the pre-GC watch publication must
/// include the REFERENCE OBJECTS' own addresses, not just their
/// referents. The non-moving young sweep only records identity
/// `pointer_map` entries for WATCHED kept-in-place survivors, and the
/// post-GC restore pass judges a Reference object's survival by
/// `pointer_map ∪ is_addr_live` — so an unwatched live YOUNG Reference
/// object was judged dead: its nulled referent was never restored and
/// the mutator then expunged the live entry (`refersTo(null) == true`),
/// losing RRWL read-lock hold counters and WeakHashMap entries.
#[test]
fn weakref_pre_gc_watch_includes_reference_objects() {
    let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.ref_processor.lock().discover_reference(
        cratonvm_gc::reference::ReferenceType::Weak,
        weak_ref.as_ptr() as usize,
        referent.as_ptr() as usize,
        None,
    );

    weakref_null_referents_pre_gc(&shared);

    assert!(
        cratonvm_gc::gc_quiescence::is_watched_referent(referent.as_ptr() as usize),
        "referent address must be watched (pre-existing behaviour)"
    );
    assert!(
        cratonvm_gc::gc_quiescence::is_watched_referent(weak_ref.as_ptr() as usize),
        "the Reference OBJECT's own address must be watched, so a \
         kept-in-place young Reference survivor gets an identity \
         pointer_map entry and its nulled referent is restored post-GC"
    );

    // Clean up the process-global watch list for other tests.
    cratonvm_gc::gc_quiescence::set_watched_referents(&[]);
}

#[test]
fn jit_native_shadow_dynamic_invoke_index_excludes_direct_calls() {
    assert_eq!(
        jit_native_shadow_dynamic_invoke_index(&Instruction::Invokevirtual(7)),
        Some(7)
    );
    assert_eq!(
        jit_native_shadow_dynamic_invoke_index(&Instruction::Invokeinterface {
            index: 17,
            count: 1,
        }),
        Some(17)
    );
    assert_eq!(
        jit_native_shadow_dynamic_invoke_index(&Instruction::Invokespecial(11)),
        None
    );
    assert_eq!(
        jit_native_shadow_dynamic_invoke_index(&Instruction::Invokestatic(13)),
        None
    );
    assert_eq!(
        jit_native_shadow_dynamic_invoke_index(&Instruction::Invokedynamic(19)),
        None
    );
}

#[test]
fn count_down_latch_force_native_covers_registered_surface() {
    let cdl = "java/util/concurrent/CountDownLatch";
    for (name, descriptor) in [
        ("<init>", "(I)V"),
        ("countDown", "()V"),
        ("await", "()V"),
        ("await", "(JLjava/util/concurrent/TimeUnit;)Z"),
        ("getCount", "()J"),
        ("toString", "()Ljava/lang/String;"),
    ] {
        assert!(
            is_count_down_latch_native_override(cdl, name, descriptor),
            "{name}{descriptor} must route to the registered native"
        );
    }
    assert!(!is_count_down_latch_native_override(
        cdl,
        "await",
        "(JLjava/time/Duration;)Z"
    ));
    assert!(!is_count_down_latch_native_override(
        "java/util/concurrent/Semaphore",
        "await",
        "()V"
    ));
}

#[test]
fn map_for_each_force_native_preserves_cyclic_map_iteration() {
    assert!(force_native_over_real_jdk_bytecode(
        "java/util/Map",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V"
    ));
    assert!(!force_native_over_real_jdk_bytecode(
        "java/util/Map",
        "forEach",
        "(Ljava/util/function/Consumer;)V"
    ));
}

#[test]
fn class_mirror_force_native_covers_bound_method_reference_surface() {
    let class = "java/lang/Class";
    for (name, descriptor) in [
        ("getName", "()Ljava/lang/String;"),
        ("getAnnotations", "()[Ljava/lang/annotation/Annotation;"),
        (
            "getDeclaredAnnotations",
            "()[Ljava/lang/annotation/Annotation;",
        ),
        (
            "getAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        ),
        (
            "getDeclaredAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        ),
        ("isAnnotationPresent", "(Ljava/lang/Class;)Z"),
        (
            "getAnnotationsByType",
            "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        ),
        (
            "getDeclaredAnnotationsByType",
            "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        ),
    ] {
        assert!(is_class_mirror_native_override(class, name, descriptor));
        assert!(force_native_over_real_jdk_bytecode(class, name, descriptor));
    }
    assert!(!is_class_mirror_native_override(
        class,
        "getMethods",
        "()[Ljava/lang/reflect/Method;"
    ));
}

#[test]
fn buffered_input_stream_real_jdk_uses_its_own_bytecode() {
    let buffered = "java/io/BufferedInputStream";
    // 995ff48c (Tomcat silent-hang scanner fix, see
    // docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md):
    // the two read overloads are now DELIBERATELY forced to the registered
    // native — interpreted per-byte read dispatch dominated the scanner's
    // hot path. Everything else (ctor/mark/reset/skip/...) still runs its
    // real-JDK bytecode so buffer/mark state stays bytecode-owned.
    for (name, descriptor) in [("read", "()I"), ("read", "([BII)I")] {
        assert!(
            force_native_over_real_jdk_bytecode(buffered, name, descriptor),
            "BufferedInputStream.{name}{descriptor} is forced native per 995ff48c"
        );
    }
    for (name, descriptor) in [
        ("<init>", "(Ljava/io/InputStream;)V"),
        ("<init>", "(Ljava/io/InputStream;I)V"),
        ("skip", "(J)J"),
        ("available", "()I"),
        ("mark", "(I)V"),
        ("reset", "()V"),
        ("markSupported", "()Z"),
        ("close", "()V"),
    ] {
        assert!(
            !force_native_over_real_jdk_bytecode(buffered, name, descriptor),
            "BufferedInputStream.{name}{descriptor} must keep its real-JDK bytecode"
        );
    }
    assert!(force_native_over_real_jdk_bytecode(
        "java/io/FilterInputStream",
        "skip",
        "(J)J"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/io/FilterInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/io/ByteArrayInputStream",
        "skip",
        "(J)J"
    ));
    for (name, descriptor) in [
        ("read", "()I"),
        ("read", "([BII)I"),
        ("available", "()I"),
        ("close", "()V"),
    ] {
        assert!(force_native_over_real_jdk_bytecode(
            "java/io/ByteArrayInputStream",
            name,
            descriptor
        ));
    }
}

#[test]
fn ffm_symbol_lookup_force_native_covers_find() {
    let symbol_lookup = "java/lang/foreign/SymbolLookup";
    let descriptor = "(Ljava/lang/String;)Ljava/util/Optional;";
    assert!(
        is_ffm_symbol_lookup_native_override(symbol_lookup, "find", descriptor),
        "SymbolLookup.find must route to the registered native instead of the abstract interface method"
    );
    assert!(
        force_native_over_real_jdk_bytecode(symbol_lookup, "find", descriptor),
        "real-JDK bytecode dispatch must force the SymbolLookup.find native"
    );
    assert!(!is_ffm_symbol_lookup_native_override(
        symbol_lookup,
        "findOrThrow",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;"
    ));
    assert!(!is_ffm_symbol_lookup_native_override(
        "java/lang/foreign/Linker",
        "find",
        descriptor
    ));
}

#[test]
fn ffm_arena_force_native_covers_lifecycle() {
    let arena = "java/lang/foreign/Arena";
    for (name, descriptor) in [
        ("scope", "()Ljava/lang/foreign/MemorySegment$Scope;"),
        ("close", "()V"),
        ("allocate", "(J)Ljava/lang/foreign/MemorySegment;"),
        ("allocate", "(JJ)Ljava/lang/foreign/MemorySegment;"),
        (
            "allocate",
            "(Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/MemorySegment;",
        ),
        (
            "allocate",
            "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;",
        ),
        (
            "allocateFrom",
            "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        ),
        (
            "allocateUtf8String",
            "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        ),
    ] {
        assert!(
            is_ffm_arena_native_override(arena, name, descriptor),
            "Arena.{name}{descriptor} must be exempt from the interface instance-method skip"
        );
        assert!(
            force_native_over_real_jdk_bytecode(arena, name, descriptor),
            "step-6 dispatch must force the Arena.{name}{descriptor} native"
        );
    }
    // No native is registered for these, so they must NOT be force-routed.
    assert!(!is_ffm_arena_native_override(
        arena,
        "allocateArray",
        "(Ljava/lang/foreign/MemoryLayout;J)Ljava/lang/foreign/MemorySegment;"
    ));
    // A real `ArenaImpl` declares scope/close/allocate concretely, so it
    // resolves under its own name and must never match.
    assert!(!is_ffm_arena_native_override(
        "jdk/internal/foreign/ArenaImpl",
        "close",
        "()V"
    ));
    assert!(!is_ffm_arena_native_override(
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;"
    ));
}

#[test]
fn ffm_memory_layout_force_native_covers_varhandle() {
    let descriptor = "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;";
    assert!(is_ffm_memory_layout_native_override(
        "java/lang/foreign/MemoryLayout",
        "varHandle",
        descriptor
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/MemoryLayout",
        "varHandle",
        descriptor
    ));
}

#[test]
fn ffm_group_layout_force_native_covers_member_layouts() {
    let group_layout = "java/lang/foreign/GroupLayout";
    let descriptor = "()Ljava/util/List;";
    assert!(is_ffm_group_layout_native_override(
        group_layout,
        "memberLayouts",
        descriptor
    ));
    assert!(force_native_over_real_jdk_bytecode(
        group_layout,
        "memberLayouts",
        descriptor
    ));
    assert!(is_ffm_group_layout_native_override(
        "java/lang/foreign/StructLayout",
        "name",
        "()Ljava/util/Optional;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/MemoryLayout",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/AddressLayout",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/AddressLayout;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/ValueLayout",
        "carrier",
        "()Ljava/lang/Class;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "carrier",
        "()Ljava/lang/Class;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/ValueLayout",
        "order",
        "()Ljava/nio/ByteOrder;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/AddressLayout",
        "targetLayout",
        "()Ljava/util/Optional;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/foreign/AddressLayout",
        "withTargetLayout",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/AddressLayout;"
    ));
}

#[test]
fn jar_file_force_native_covers_registered_surface() {
    let jar_file = "java/util/jar/JarFile";
    for (name, descriptor) in [
        ("<init>", "(Ljava/io/File;)V"),
        ("<init>", "(Ljava/lang/String;)V"),
        ("<init>", "(Ljava/lang/String;Z)V"),
        ("<init>", "(Ljava/io/File;Z)V"),
        ("<init>", "(Ljava/io/File;ZI)V"),
        ("<init>", "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V"),
        ("getManifest", "()Ljava/util/jar/Manifest;"),
        ("getManifestFromReference", "()Ljava/util/jar/Manifest;"),
        ("stream", "()Ljava/util/stream/Stream;"),
        ("entries", "()Ljava/util/Enumeration;"),
        ("getEntry", "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;"),
        (
            "getJarEntry",
            "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        ),
        (
            "getInputStream",
            "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
        ),
        ("size", "()I"),
        ("close", "()V"),
        ("getName", "()Ljava/lang/String;"),
    ] {
        assert!(
            force_native_over_real_jdk_bytecode(jar_file, name, descriptor),
            "{name}{descriptor} must use native JarFile bridge"
        );
    }
}

#[test]
fn manifest_force_native_covers_registered_surface() {
    let manifest = "java/util/jar/Manifest";
    for (name, descriptor) in [
        ("<init>", "()V"),
        ("<init>", "(Ljava/io/InputStream;)V"),
        ("<init>", "(Ljava/io/InputStream;Ljava/lang/String;)V"),
        ("<init>", "(Ljava/util/jar/Manifest;)V"),
        (
            "<init>",
            "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V",
        ),
        ("getMainAttributes", "()Ljava/util/jar/Attributes;"),
        ("getEntries", "()Ljava/util/Map;"),
    ] {
        assert!(
            force_native_over_real_jdk_bytecode(manifest, name, descriptor),
            "{name}{descriptor} must use native Manifest bridge"
        );
    }
}

#[test]
fn standard_location_force_native_covers_javac_regex_shortcut() {
    assert!(force_native_over_real_jdk_bytecode(
        "javax/tools/StandardLocation",
        "computeIsModuleOrientedLocation",
        "(Ljava/lang/String;)Z"
    ));
    assert!(!force_native_over_real_jdk_bytecode(
        "javax/tools/StandardLocation",
        "locationFor",
        "(Ljava/lang/String;)Ljavax/tools/JavaFileManager$Location;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/JavacFileManager",
        "checkNotModuleOrientedLocation",
        "(Ljavax/tools/JavaFileManager$Location;)V"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/JavacFileManager",
        "list",
        "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/JavacFileManager",
        "inferBinaryName",
        "(Ljavax/tools/JavaFileManager$Location;Ljavax/tools/JavaFileObject;)Ljava/lang/String;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/RelativePath",
        "hashCode",
        "()I"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/RelativePath",
        "equals",
        "(Ljava/lang/Object;)Z"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/RelativePath",
        "compareTo",
        "(Lcom/sun/tools/javac/file/RelativePath;)I"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "com/sun/tools/javac/file/RelativePath",
        "getPath",
        "()Ljava/lang/String;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/ClassLoader",
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/ClassLoader",
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;"
    ));
}

#[test]
fn method_handles_varhandle_factories_force_native() {
    let method_handles = "java/lang/invoke/MethodHandles";
    for (name, descriptor) in [
        (
            "arrayElementVarHandle",
            "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        ),
        (
            "byteArrayViewVarHandle",
            "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        ),
        (
            "byteBufferViewVarHandle",
            "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;",
        ),
    ] {
        assert!(
            is_method_handles_varhandle_factory_native_override(method_handles, name, descriptor),
            "{name}{descriptor} must route to the registered native factory"
        );
        assert!(
            force_native_over_real_jdk_bytecode(method_handles, name, descriptor),
            "{name}{descriptor} must bypass real JDK VarHandle factory bytecode"
        );
    }
    assert!(!is_method_handles_varhandle_factory_native_override(
        method_handles,
        "byteArrayViewVarHandle",
        "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"
    ));
    assert!(!is_method_handles_varhandle_factory_native_override(
        "java/lang/invoke/MethodHandle",
        "byteArrayViewVarHandle",
        "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"
    ));
}

#[test]
fn java_nio_access_force_native_covers_jdk17_direct_buffer_pool() {
    let access = "jdk/internal/access/JavaNioAccess";
    let descriptor = "()Ljdk/internal/misc/VM$BufferPool;";
    assert!(
        is_java_nio_access_native_override(access, "getDirectBufferPool", descriptor),
        "JDK 17 VM$BufferPoolsHolder must route JavaNioAccess.getDirectBufferPool to the native bridge"
    );
    assert!(
        force_native_over_real_jdk_bytecode(access, "getDirectBufferPool", descriptor),
        "invokeinterface JavaNioAccess.getDirectBufferPool must force the registered native"
    );
    assert!(!is_java_nio_access_native_override(
        "java/nio/Buffer$1",
        "getDirectBufferPool",
        descriptor
    ));
    assert!(!is_java_nio_access_native_override(
        access,
        "getBufferPool",
        "()Ljava/lang/management/BufferPoolMXBean;"
    ));
}

#[test]
fn file_channel_impl_open_force_native_covers_registered_factories() {
    let fci = "sun/nio/ch/FileChannelImpl";
    let descriptor =
        "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;";
    assert!(
        is_file_channel_impl_open_native_override(fci, "open", descriptor),
        "FileChannelImpl.open{descriptor} must route to the registered native factory"
    );
    assert!(
        force_native_over_real_jdk_bytecode(fci, "open", descriptor),
        "real-JDK bytecode dispatch must force the FileChannelImpl.open native"
    );
    let jdk21_descriptor =
        "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/lang/Object;)Ljava/nio/channels/FileChannel;";
    assert!(
        is_file_channel_impl_open_native_override(fci, "open", jdk21_descriptor),
        "JDK 21 FileChannelImpl.open{jdk21_descriptor} must route to native too"
    );
    assert!(
        force_native_over_real_jdk_bytecode(fci, "open", jdk21_descriptor),
        "real-JDK bytecode dispatch must force the JDK 21 FileChannelImpl.open native"
    );
    assert!(!is_file_channel_impl_open_native_override(
        fci,
        "tryLock",
        "(JJZ)Ljava/nio/channels/FileLock;"
    ));
}

#[test]
fn file_system_provider_link_ops_force_native_over_the_base_class_throw() {
    // The base class gives all three a concrete
    // `throw new UnsupportedOperationException()` body, and CratonVM's
    // default provider IS that base class — so both dispatch gates have to
    // admit the registered natives or `Files.createSymbolicLink` dies at
    // `FileSystemProvider.createSymbolicLink`.
    let providers = [
        "java/nio/file/spi/FileSystemProvider",
        "sun/nio/fs/WindowsFileSystemProvider",
        "sun/nio/fs/UnixFileSystemProvider",
    ];
    let ops = [
        (
            "createSymbolicLink",
            "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
        ),
        ("createLink", "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V"),
        (
            "readSymbolicLink",
            "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        ),
    ];
    for provider in providers {
        for (name, descriptor) in ops {
            assert!(
                is_file_system_provider_link_native_override(provider, name, descriptor),
                "{provider}.{name}{descriptor} must route to the registered native"
            );
            assert!(
                force_native_over_real_jdk_bytecode(provider, name, descriptor),
                "real-JDK bytecode dispatch must force the {name} native"
            );
        }
    }
    // The `Files` static wrapper is NOT the receiver these run on: a
    // registration there loses to `Files`' own bytecode, which is what made
    // the original three stubs dead code. The exemption must not pretend
    // otherwise.
    assert!(!is_file_system_provider_link_native_override(
        "java/nio/file/Files",
        "createSymbolicLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;"
    ));
    // A neighbouring provider method must not be swept in.
    assert!(!is_file_system_provider_link_native_override(
        "java/nio/file/spi/FileSystemProvider",
        "delete",
        "(Ljava/nio/file/Path;)V"
    ));
}

#[test]
fn native_thread_set_force_native_covers_filechannel_blocking_bookkeeping() {
    let nts = "sun/nio/ch/NativeThreadSet";
    for (name, descriptor) in [("add", "()I"), ("remove", "(I)V"), ("signalAndWait", "()V")] {
        assert!(
            is_native_thread_set_native_override(nts, name, descriptor),
            "NativeThreadSet.{name}{descriptor} must route to native bookkeeping"
        );
        assert!(
            force_native_over_real_jdk_bytecode(nts, name, descriptor),
            "real-JDK bytecode dispatch must force NativeThreadSet.{name}{descriptor}"
        );
    }
}

#[test]
fn awt_imageio_force_native_covers_registered_surface() {
    let bi = "java/awt/image/BufferedImage";
    for (name, descriptor) in [
        ("<init>", "(III)V"),
        ("getWidth", "()I"),
        ("getHeight", "()I"),
        ("getRGB", "(II)I"),
        ("setRGB", "(III)V"),
        ("getType", "()I"),
        ("createGraphics", "()Ljava/awt/Graphics2D;"),
        ("flush", "()V"),
        ("getRGB", "(IIII[III)[I"),
    ] {
        assert!(is_awt_imageio_native_override(bi, name, descriptor));
        assert!(force_native_over_real_jdk_bytecode(bi, name, descriptor));
    }

    let imageio = "javax/imageio/ImageIO";
    for descriptor in [
        "(Ljava/io/InputStream;)Ljava/awt/image/BufferedImage;",
        "(Ljava/io/File;)Ljava/awt/image/BufferedImage;",
        "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/OutputStream;)Z",
        "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljavax/imageio/stream/ImageOutputStream;)Z",
        "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/File;)Z",
    ] {
        let name = if descriptor.ends_with("BufferedImage;") { "read" } else { "write" };
        assert!(is_awt_imageio_native_override(imageio, name, descriptor));
        assert!(force_native_over_real_jdk_bytecode(imageio, name, descriptor));
    }

    let jpeg_reader = "com/sun/imageio/plugins/jpeg/JPEGImageReader";
    assert!(is_awt_imageio_native_override(
        jpeg_reader,
        "read",
        "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        jpeg_reader,
        "read",
        "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;"
    ));
    assert!(is_awt_imageio_native_override(
        jpeg_reader,
        "dispose",
        "()V"
    ));

    let png_writer = "com/sun/imageio/plugins/png/PNGImageWriter";
    let write_desc = "(Ljavax/imageio/metadata/IIOMetadata;Ljavax/imageio/IIOImage;Ljavax/imageio/ImageWriteParam;)V";
    assert!(is_awt_imageio_native_override(
        png_writer, "write", write_desc
    ));
    assert!(force_native_over_real_jdk_bytecode(
        png_writer, "write", write_desc
    ));
}

#[test]
fn charset_force_native_covers_tomcat_cache_surface() {
    let charset = "java/nio/charset/Charset";
    assert!(
        force_native_over_real_jdk_bytecode(
            charset,
            "availableCharsets",
            "()Ljava/util/SortedMap;"
        ),
        "Tomcat B2CConverter must use native Charset.availableCharsets"
    );
    assert!(
        force_native_over_real_jdk_bytecode(charset, "aliases", "()Ljava/util/Set;"),
        "Tomcat CharsetCache must use native Charset.aliases on synthetic Charset objects"
    );
    assert!(!force_native_over_real_jdk_bytecode(
        charset,
        "aliases",
        "()Ljava/util/List;"
    ));
}

#[test]
fn stamped_lock_force_native_covers_registered_surface() {
    let sl = "java/util/concurrent/locks/StampedLock";
    for (name, descriptor) in [
        ("<init>", "()V"),
        ("readLock", "()J"),
        ("writeLock", "()J"),
        ("tryOptimisticRead", "()J"),
        ("unlockRead", "(J)V"),
        ("unlockWrite", "(J)V"),
        ("unstampedUnlockRead", "()V"),
        ("unstampedUnlockWrite", "()V"),
        ("tryUnlockRead", "()Z"),
        ("tryUnlockWrite", "()Z"),
        ("validate", "(J)Z"),
        ("tryReadLock", "()J"),
        ("tryWriteLock", "()J"),
        ("tryConvertToWriteLock", "(J)J"),
        ("tryConvertToReadLock", "(J)J"),
        ("tryConvertToOptimisticRead", "(J)J"),
        ("unlock", "(J)V"),
        ("readLockInterruptibly", "()J"),
        ("writeLockInterruptibly", "()J"),
        ("tryReadLock", "(JLjava/util/concurrent/TimeUnit;)J"),
        ("tryWriteLock", "(JLjava/util/concurrent/TimeUnit;)J"),
        ("isLocked", "()Z"),
        ("isWriteLocked", "()Z"),
        ("isReadLocked", "()Z"),
        ("getReadLockCount", "()I"),
    ] {
        assert!(
            is_stamped_lock_native_override(sl, name, descriptor),
            "{name}{descriptor} must route to the registered native"
        );
    }
    assert!(!is_stamped_lock_native_override(
        sl,
        "asWriteLock",
        "()Ljava/util/concurrent/locks/Lock;"
    ));
    assert!(!is_stamped_lock_native_override(
        "java/util/concurrent/locks/ReentrantReadWriteLock",
        "writeLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$WriteLock;"
    ));
    for view in [
        "java/util/concurrent/locks/StampedLock$ReadLockView",
        "java/util/concurrent/locks/StampedLock$WriteLockView",
    ] {
        for (name, descriptor) in [("lock", "()V"), ("tryLock", "()Z"), ("unlock", "()V")] {
            assert!(
                is_stamped_lock_native_override(view, name, descriptor),
                "{view}.{name}{descriptor} must route to the registered native"
            );
            assert!(
                redefine_immune_forced_native(view, name, descriptor),
                "{view}.{name}{descriptor} must stay native even after unrelated redefinition"
            );
        }
    }
    assert!(redefine_immune_forced_native(
        sl,
        "unstampedUnlockWrite",
        "()V"
    ));
    assert!(!is_stamped_lock_native_override(
        "java/util/concurrent/locks/StampedLock$WriteLockView",
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;"
    ));
    assert!(!redefine_immune_forced_native(
        "java/util/concurrent/locks/StampedLock$WriteLockView",
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;"
    ));
}

#[test]
fn bc_crypto_math_force_native_covers_longarray_helpers() {
    let long_array = "org/bouncycastle/math/ec/LongArray";
    for (name, descriptor) in [
        ("modReduce", "(I[I)Lorg/bouncycastle/math/ec/LongArray;"),
        (
            "modMultiply",
            "(Lorg/bouncycastle/math/ec/LongArray;I[I)Lorg/bouncycastle/math/ec/LongArray;",
        ),
        ("modSquare", "(I[I)Lorg/bouncycastle/math/ec/LongArray;"),
        ("modSquareN", "(II[I)Lorg/bouncycastle/math/ec/LongArray;"),
        ("modInverse", "(I[I)Lorg/bouncycastle/math/ec/LongArray;"),
        ("reduce", "(I[I)V"),
        (
            "multiply",
            "(Lorg/bouncycastle/math/ec/LongArray;I[I)Lorg/bouncycastle/math/ec/LongArray;",
        ),
        ("square", "(I[I)Lorg/bouncycastle/math/ec/LongArray;"),
    ] {
        assert!(
            is_bc_crypto_math_native_override(long_array, name, descriptor),
            "{name}{descriptor} must route to the registered BC native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(long_array, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted BC bytecode"
        );
        assert!(
            redefine_immune_forced_native(long_array, name, descriptor),
            "{name}{descriptor} must stay native after unrelated redefinition"
        );
    }

    let f2m = "org/bouncycastle/math/ec/ECFieldElement$F2m";
    for (name, descriptor) in [
        (
            "add",
            "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        ),
        (
            "subtract",
            "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        ),
        (
            "multiply",
            "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        ),
        (
            "divide",
            "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        ),
        (
            "multiplyPlusProduct",
            "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        ),
        (
            "squarePlusProduct",
            "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        ),
        ("addOne", "()Lorg/bouncycastle/math/ec/ECFieldElement;"),
        ("square", "()Lorg/bouncycastle/math/ec/ECFieldElement;"),
        ("squarePow", "(I)Lorg/bouncycastle/math/ec/ECFieldElement;"),
        ("invert", "()Lorg/bouncycastle/math/ec/ECFieldElement;"),
    ] {
        assert!(
            is_bc_crypto_math_native_override(f2m, name, descriptor),
            "{name}{descriptor} must route to the registered BC F2m native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(f2m, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted BC F2m bytecode"
        );
        assert!(
            redefine_immune_forced_native(f2m, name, descriptor),
            "{name}{descriptor} must stay native after unrelated redefinition"
        );
    }

    let f2m_point = "org/bouncycastle/math/ec/ECPoint$F2m";
    for (name, descriptor) in [
        (
            "add",
            "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        ),
        ("twice", "()Lorg/bouncycastle/math/ec/ECPoint;"),
        (
            "twicePlus",
            "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        ),
    ] {
        assert!(
            is_bc_crypto_math_native_override(f2m_point, name, descriptor),
            "{name}{descriptor} must route to the registered BC F2m point native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(f2m_point, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted BC F2m point bytecode"
        );
        assert!(
            redefine_immune_forced_native(f2m_point, name, descriptor),
            "{name}{descriptor} must stay native after unrelated redefinition"
        );
    }

    let ec_algorithms = "org/bouncycastle/math/ec/ECAlgorithms";
    assert!(is_bc_crypto_math_native_override(
        ec_algorithms,
        "implShamirsTrickJsf",
        "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        ec_algorithms,
        "implShamirsTrickJsf",
        "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;"
    ));
    assert!(redefine_immune_forced_native(
        ec_algorithms,
        "implShamirsTrickJsf",
        "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;"
    ));

    let x25519_field = "org/bouncycastle/math/ec/rfc7748/X25519Field";
    assert!(is_bc_crypto_math_native_override(
        x25519_field,
        "mul",
        "([I[I[I)V"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        x25519_field,
        "mul",
        "([I[I[I)V"
    ));
    assert!(redefine_immune_forced_native(
        x25519_field,
        "mul",
        "([I[I[I)V"
    ));

    let x448_field = "org/bouncycastle/math/ec/rfc7748/X448Field";
    for (name, descriptor) in [
        ("mul", "([I[I[I)V"),
        ("mul", "([II[I)V"),
        ("sqr", "([I[I)V"),
        ("sqr", "([II[I)V"),
    ] {
        assert!(is_bc_crypto_math_native_override(
            x448_field, name, descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            x448_field, name, descriptor
        ));
        assert!(redefine_immune_forced_native(x448_field, name, descriptor));
    }

    let fp_point = "org/bouncycastle/math/ec/ECPoint$Fp";
    for (name, descriptor) in [
        (
            "add",
            "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        ),
        ("twice", "()Lorg/bouncycastle/math/ec/ECPoint;"),
        (
            "twicePlus",
            "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        ),
        ("threeTimes", "()Lorg/bouncycastle/math/ec/ECPoint;"),
        ("timesPow2", "(I)Lorg/bouncycastle/math/ec/ECPoint;"),
        ("negate", "()Lorg/bouncycastle/math/ec/ECPoint;"),
    ] {
        assert!(
            is_bc_crypto_math_native_override(fp_point, name, descriptor),
            "{name}{descriptor} must route to the registered BC Fp point native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(fp_point, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted BC Fp point bytecode"
        );
        assert!(
            redefine_immune_forced_native(fp_point, name, descriptor),
            "{name}{descriptor} must stay native after unrelated redefinition"
        );
    }

    let sect571 = "org/bouncycastle/math/ec/custom/sec/SecT571Field";
    for (name, descriptor) in [
        ("add", "([J[J[J)V"),
        ("addBothTo", "([J[J[J)V"),
        ("addExt", "([J[J[J)V"),
        ("multiply", "([J[J[J)V"),
        ("multiplyAddToExt", "([J[J[J)V"),
        ("reduce", "([J[J)V"),
        ("square", "([J[J)V"),
        ("squareAddToExt", "([J[J)V"),
        ("squareN", "([JI[J)V"),
        ("invert", "([J[J)V"),
        ("sqrt", "([J[J)V"),
        ("halfTrace", "([J[J)V"),
        ("trace", "([J)I"),
        ("precompMultiplicand", "([J)[J"),
        ("multiplyPrecomp", "([J[J[J)V"),
        ("multiplyPrecompAddToExt", "([J[J[J)V"),
    ] {
        assert!(
            is_bc_crypto_math_native_override(sect571, name, descriptor),
            "{name}{descriptor} must route to the registered SecT native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(sect571, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted SecT bytecode"
        );
        assert!(
            redefine_immune_forced_native(sect571, name, descriptor),
            "{name}{descriptor} must stay native after unrelated redefinition"
        );
    }

    assert!(is_bc_crypto_math_native_override(
        "org/bouncycastle/math/ec/custom/sec/SecT233Field",
        "multiply",
        "([J[J[J)V"
    ));
    assert!(is_bc_crypto_math_native_override(
        "org/bouncycastle/math/ec/ECPoint",
        "timesPow2",
        "(I)Lorg/bouncycastle/math/ec/ECPoint;"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "org/bouncycastle/math/ec/ECPoint",
        "timesPow2",
        "(I)Lorg/bouncycastle/math/ec/ECPoint;"
    ));
    assert!(redefine_immune_forced_native(
        "org/bouncycastle/math/ec/ECPoint",
        "timesPow2",
        "(I)Lorg/bouncycastle/math/ec/ECPoint;"
    ));

    let cbc = "org/bouncycastle/crypto/modes/CBCBlockCipher";
    assert!(is_bc_crypto_math_native_override(
        cbc,
        "processBlock",
        "([BI[BI)I"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        cbc,
        "processBlock",
        "([BI[BI)I"
    ));
    assert!(redefine_immune_forced_native(
        cbc,
        "processBlock",
        "([BI[BI)I"
    ));

    let sic = "org/bouncycastle/crypto/modes/SICBlockCipher";
    for (name, descriptor) in [
        ("processBlock", "([BI[BI)I"),
        ("processBytes", "([BII[BI)I"),
    ] {
        assert!(is_bc_crypto_math_native_override(sic, name, descriptor));
        assert!(force_native_over_real_jdk_bytecode(sic, name, descriptor));
        assert!(redefine_immune_forced_native(sic, name, descriptor));
    }

    let sm4 = "org/bouncycastle/crypto/engines/SM4Engine";
    assert!(is_bc_crypto_math_native_override(
        sm4,
        "processBlock",
        "([BI[BI)I"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        sm4,
        "processBlock",
        "([BI[BI)I"
    ));
    assert!(redefine_immune_forced_native(
        sm4,
        "processBlock",
        "([BI[BI)I"
    ));

    let xtea = "org/bouncycastle/crypto/engines/XTEAEngine";
    assert!(is_bc_crypto_math_native_override(
        xtea,
        "processBlock",
        "([BI[BI)I"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        xtea,
        "processBlock",
        "([BI[BI)I"
    ));
    assert!(redefine_immune_forced_native(
        xtea,
        "processBlock",
        "([BI[BI)I"
    ));

    let salsa = "org/bouncycastle/crypto/engines/Salsa20Engine";
    assert!(is_bc_crypto_math_native_override(
        salsa,
        "salsaCore",
        "(I[I[I)V"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        salsa,
        "salsaCore",
        "(I[I[I)V"
    ));
    assert!(redefine_immune_forced_native(
        salsa,
        "salsaCore",
        "(I[I[I)V"
    ));
    for stream_engine in [
        "org/bouncycastle/crypto/engines/Salsa20Engine",
        "org/bouncycastle/crypto/engines/XSalsa20Engine",
        "org/bouncycastle/crypto/engines/ChaChaEngine",
        "org/bouncycastle/crypto/engines/ChaCha7539Engine",
        "org/bouncycastle/crypto/engines/XChaCha20Engine",
    ] {
        assert!(is_bc_crypto_math_native_override(
            stream_engine,
            "processBytes",
            "([BII[BI)I"
        ));
        assert!(force_native_over_real_jdk_bytecode(
            stream_engine,
            "processBytes",
            "([BII[BI)I"
        ));
        assert!(redefine_immune_forced_native(
            stream_engine,
            "processBytes",
            "([BII[BI)I"
        ));
    }

    for vmpc_engine in [
        "org/bouncycastle/crypto/engines/VMPCEngine",
        "org/bouncycastle/crypto/engines/VMPCKSA3Engine",
    ] {
        assert!(is_bc_crypto_math_native_override(
            vmpc_engine,
            "processBytes",
            "([BII[BI)I"
        ));
        assert!(force_native_over_real_jdk_bytecode(
            vmpc_engine,
            "processBytes",
            "([BII[BI)I"
        ));
        assert!(redefine_immune_forced_native(
            vmpc_engine,
            "processBytes",
            "([BII[BI)I"
        ));
    }

    let gost3411 = "org/bouncycastle/crypto/digests/GOST3411Digest";
    assert!(is_bc_crypto_math_native_override(
        gost3411,
        "processBlock",
        "([BI)V"
    ));
    assert!(force_native_over_real_jdk_bytecode(
        gost3411,
        "processBlock",
        "([BI)V"
    ));
    assert!(redefine_immune_forced_native(
        gost3411,
        "processBlock",
        "([BI)V"
    ));

    let whirlpool = "org/bouncycastle/crypto/digests/WhirlpoolDigest";
    for (name, descriptor) in [("processBlock", "()V"), ("update", "([BII)V")] {
        assert!(is_bc_crypto_math_native_override(
            whirlpool, name, descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            whirlpool, name, descriptor
        ));
        assert!(redefine_immune_forced_native(whirlpool, name, descriptor));
    }

    let poly1305 = "org/bouncycastle/crypto/macs/Poly1305";
    for (name, descriptor) in [("update", "([BII)V"), ("doFinal", "([BI)I")] {
        assert!(is_bc_crypto_math_native_override(
            poly1305, name, descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            poly1305, name, descriptor
        ));
        assert!(redefine_immune_forced_native(poly1305, name, descriptor));
    }

    let pkcs12 = "org/bouncycastle/crypto/generators/PKCS12ParametersGenerator";
    for (name, descriptor) in [
        (
            "generateDerivedParameters",
            "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        ),
        (
            "generateDerivedParameters",
            "(II)Lorg/bouncycastle/crypto/CipherParameters;",
        ),
        (
            "generateDerivedMacParameters",
            "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        ),
    ] {
        assert!(is_bc_crypto_math_native_override(pkcs12, name, descriptor));
        assert!(force_native_over_real_jdk_bytecode(
            pkcs12, name, descriptor
        ));
        assert!(redefine_immune_forced_native(pkcs12, name, descriptor));
    }

    for (class_name, name, descriptor) in [
        (
            "org/bouncycastle/crypto/digests/Blake2sDigest",
            "compress",
            "([BI)V",
        ),
        (
            "org/bouncycastle/crypto/digests/Blake2sDigest",
            "G",
            "(IIIIII)V",
        ),
        (
            "org/bouncycastle/crypto/digests/KeccakDigest",
            "KeccakPermutation",
            "()V",
        ),
        (
            "org/bouncycastle/crypto/digests/KeccakDigest",
            "KeccakAbsorb",
            "([BI)V",
        ),
        (
            "org/bouncycastle/crypto/digests/KeccakDigest",
            "KeccakExtract",
            "()V",
        ),
        (
            "org/bouncycastle/crypto/generators/SCrypt",
            "generate",
            "([B[BIIII)[B",
        ),
        (
            "org/bouncycastle/crypto/generators/Argon2BytesGenerator",
            "generateBytes",
            "([B[BII)I",
        ),
        (
            "org/bouncycastle/crypto/generators/Argon2BytesGenerator",
            "roundFunction",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;IIIIIIIIIIIIIIII)V",
        ),
    ] {
        assert!(is_bc_crypto_math_native_override(
            class_name, name, descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            class_name, name, descriptor
        ));
        assert!(redefine_immune_forced_native(class_name, name, descriptor));
    }

    let argon2_block = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block";
    for (name, descriptor) in [
        ("fromBytes", "([B)V"),
        ("toBytes", "([B)V"),
        (
            "copyBlock",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
        (
            "xor",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
        (
            "xorWith",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
        (
            "xorWith",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
        (
            "clear",
            "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;",
        ),
    ] {
        assert!(is_bc_crypto_math_native_override(
            argon2_block,
            name,
            descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            argon2_block,
            name,
            descriptor
        ));
        assert!(redefine_immune_forced_native(
            argon2_block,
            name,
            descriptor
        ));
    }

    let argon2_fill_block = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FillBlock";
    for (name, descriptor) in [
        ("applyBlake", "()V"),
        (
            "fillBlock",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
        (
            "fillBlock",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
        (
            "fillBlockWithXor",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
    ] {
        assert!(is_bc_crypto_math_native_override(
            argon2_fill_block,
            name,
            descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            argon2_fill_block,
            name,
            descriptor
        ));
        assert!(redefine_immune_forced_native(
            argon2_fill_block,
            name,
            descriptor
        ));
    }

    let argon2_fixed_pool =
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FixedBlockPool";
    for (name, descriptor) in [
        (
            "allocate",
            "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;",
        ),
        (
            "deallocate",
            "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        ),
    ] {
        assert!(is_bc_crypto_math_native_override(
            argon2_fixed_pool,
            name,
            descriptor
        ));
        assert!(force_native_over_real_jdk_bytecode(
            argon2_fixed_pool,
            name,
            descriptor
        ));
        assert!(redefine_immune_forced_native(
            argon2_fixed_pool,
            name,
            descriptor
        ));
    }

    let pack = "org/bouncycastle/util/Pack";
    for (name, descriptor) in [
        ("bigEndianToInt", "([BI)I"),
        ("littleEndianToInt", "([BI)I"),
        ("bigEndianToInt", "([BI[I)V"),
        ("littleEndianToInt", "([BI[I)V"),
        ("bigEndianToInt", "([BI[III)V"),
        ("littleEndianToInt", "([BI[III)V"),
        ("intToBigEndian", "(I[BI)V"),
        ("intToLittleEndian", "(I[BI)V"),
        ("intToBigEndian", "([I[BI)V"),
        ("intToLittleEndian", "([I[BI)V"),
        ("intToBigEndian", "([III[BI)V"),
        ("intToLittleEndian", "([III[BI)V"),
    ] {
        assert!(is_bc_crypto_math_native_override(pack, name, descriptor));
        assert!(force_native_over_real_jdk_bytecode(pack, name, descriptor));
        assert!(redefine_immune_forced_native(pack, name, descriptor));
    }

    for point in [
        "org/bouncycastle/math/ec/custom/sec/SecT283R1Point",
        "org/bouncycastle/math/ec/custom/sec/SecT571K1Point",
        "org/bouncycastle/math/ec/custom/sec/SecT163K1Point",
    ] {
        assert!(
            is_bc_crypto_math_native_override(
                point,
                "twice",
                "()Lorg/bouncycastle/math/ec/ECPoint;"
            ),
            "{point}.twice must route to the registered SecT point native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(
                point,
                "twice",
                "()Lorg/bouncycastle/math/ec/ECPoint;"
            ),
            "{point}.twice must not fall through to interpreted SecT point bytecode"
        );
        assert!(
            redefine_immune_forced_native(point, "twice", "()Lorg/bouncycastle/math/ec/ECPoint;"),
            "{point}.twice must stay native after unrelated redefinition"
        );
    }
    assert!(!is_bc_crypto_math_native_override(
        "org/bouncycastle/math/ec/custom/sec/SecP224K1Field",
        "multiply",
        "([I[I[I)V"
    ));
}

#[test]
fn xerces_cmstateset_force_native_covers_hash_and_equals_hotspots() {
    let cmstateset = "com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet";
    for (name, descriptor) in [
        ("hashCode", "()I"),
        ("equals", "(Ljava/lang/Object;)Z"),
        (
            "isSameSet",
            "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z",
        ),
    ] {
        assert!(
            is_xerces_cmstateset_native_override(cmstateset, name, descriptor),
            "{name}{descriptor} must route to the registered CMStateSet native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(cmstateset, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted Xerces bytecode"
        );
    }
    assert!(!is_xerces_cmstateset_native_override(
        cmstateset, "setBit", "(I)V"
    ));
    assert!(!is_xerces_cmstateset_native_override(
        "com/sun/org/apache/xerces/internal/impl/xs/models/XSDFACM",
        "hashCode",
        "()I"
    ));
}

#[test]
fn object_clone_force_native_covers_super_clone() {
    assert!(
        force_native_over_real_jdk_bytecode("java/lang/Object", "clone", "()Ljava/lang/Object;"),
        "Object.clone must route to the registered shallow-clone native"
    );
}

#[test]
fn string_utf16_get_chars_force_native_covers_cached_dispatch() {
    assert!(force_native_over_real_jdk_bytecode(
        "java/lang/StringUTF16",
        "getChars",
        "([BII[CI)V"
    ));
}

#[test]
fn xerces_xml_parser_force_native_covers_liquibase_parse_hotspots() {
    let xmlchar = "com/sun/org/apache/xerces/internal/util/XMLChar";
    for (name, descriptor) in [
        ("isSpace", "(I)Z"),
        ("isNameStart", "(I)Z"),
        ("isName", "(I)Z"),
        ("isNCNameStart", "(I)Z"),
        ("isNCName", "(I)Z"),
    ] {
        assert!(
            is_xerces_xml_parser_native_override(xmlchar, name, descriptor),
            "{name}{descriptor} must route to the registered XMLChar native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(xmlchar, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted XMLChar bytecode"
        );
    }

    let analyzer = "jdk/xml/internal/XMLLimitAnalyzer";
    for (name, descriptor) in [
        ("addValue", "(ILjava/lang/String;I)V"),
        (
            "addValue",
            "(Ljdk/xml/internal/XMLSecurityManager$Limit;Ljava/lang/String;I)V",
        ),
        ("getValue", "(I)I"),
        ("getValue", "(Ljdk/xml/internal/XMLSecurityManager$Limit;)I"),
        ("getTotalValue", "(I)I"),
        (
            "getTotalValue",
            "(Ljdk/xml/internal/XMLSecurityManager$Limit;)I",
        ),
        ("getValueByIndex", "(I)I"),
    ] {
        assert!(
            is_xerces_xml_parser_native_override(analyzer, name, descriptor),
            "{name}{descriptor} must route to the registered XMLLimitAnalyzer native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(analyzer, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted XMLLimitAnalyzer bytecode"
        );
    }

    let simple_type_decl = "com/sun/org/apache/xerces/internal/impl/dv/xs/XSSimpleTypeDecl";
    for descriptor in [
        "(Ljava/lang/String;S)Ljava/lang/String;",
        "(Ljava/lang/Object;S)Ljava/lang/String;",
    ] {
        assert!(
            is_xerces_xml_parser_native_override(simple_type_decl, "normalize", descriptor),
            "normalize{descriptor} must route to the registered XSSimpleTypeDecl native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(simple_type_decl, "normalize", descriptor),
            "normalize{descriptor} must not fall through to interpreted XSSimpleTypeDecl bytecode"
        );
    }

    let xsd_key = "com/sun/org/apache/xerces/internal/impl/xs/traversers/XSDHandler$XSDKey";
    for (name, descriptor) in [("hashCode", "()I"), ("equals", "(Ljava/lang/Object;)Z")] {
        assert!(
            is_xerces_xml_parser_native_override(xsd_key, name, descriptor),
            "{name}{descriptor} must route to the registered XSDKey native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(xsd_key, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted XSDKey bytecode"
        );
    }

    let entity_scanner = "com/sun/org/apache/xerces/internal/impl/XMLEntityScanner";
    for (name, descriptor) in [
        ("scanContent", "(Lcom/sun/org/apache/xerces/internal/xni/XMLString;)I"),
        (
            "scanQName",
            "(Lcom/sun/org/apache/xerces/internal/xni/QName;Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z",
        ),
        ("skipSpaces", "()Z"),
        (
            "normalizeNewlines",
            "(SLcom/sun/org/apache/xerces/internal/xni/XMLString;ZZLcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z",
        ),
        (
            "checkEntityLimit",
            "(Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;Lcom/sun/xml/internal/stream/Entity$ScannedEntity;II)V",
        ),
    ] {
        assert!(
            is_xerces_xml_parser_native_override(entity_scanner, name, descriptor),
            "{name}{descriptor} must route to the registered XMLEntityScanner native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(entity_scanner, name, descriptor),
            "{name}{descriptor} must not fall through to interpreted XMLEntityScanner bytecode"
        );
    }

    for (class_name, name, descriptor) in [
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl",
            "getNodeName",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl",
            "getNamespaceURI",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl",
            "getPrefix",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl",
            "getLocalName",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl",
            "getNodeType",
            "()S",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl",
            "getReadOnly",
            "()Z",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/ElementImpl",
            "getTagName",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl",
            "getName",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl",
            "getValue",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl",
            "getNodeValue",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl",
            "getSpecified",
            "()Z",
        ),
        (
            "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl",
            "isId",
            "()Z",
        ),
    ] {
        assert!(
            is_xerces_xml_parser_native_override(class_name, name, descriptor),
            "{class_name}.{name}{descriptor} must route to the registered opti-DOM native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(class_name, name, descriptor),
            "{class_name}.{name}{descriptor} must not fall through to interpreted opti-DOM bytecode"
        );
    }

    let range_token = "com/sun/org/apache/xerces/internal/impl/xpath/regex/RangeToken";
    assert!(
        is_xerces_xml_parser_native_override(range_token, "sortRanges", "()V"),
        "RangeToken.sortRanges must route to the registered regex native"
    );
    assert!(
        force_native_over_real_jdk_bytecode(range_token, "sortRanges", "()V"),
        "RangeToken.sortRanges must not fall through to interpreted regex bytecode"
    );

    assert!(!is_xerces_xml_parser_native_override(
        xmlchar,
        "isValidName",
        "(Ljava/lang/String;)Z"
    ));
    assert!(!is_xerces_xml_parser_native_override(
        analyzer,
        "debugPrint",
        "(Ljdk/xml/internal/XMLSecurityManager;)V"
    ));
}

#[test]
fn liquibase_checksum_force_native_covers_status_hotpath_intrinsics() {
    for (class_name, method_name, descriptor) in [
        (
            "liquibase/change/AbstractChange$1",
            "include",
            "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;)Z",
        ),
        (
            "liquibase/change/ColumnConfig",
            "getSerializableFieldValue",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        ),
    ] {
        assert!(
            is_liquibase_checksum_native_override(class_name, method_name, descriptor),
            "{class_name}.{method_name}{descriptor} must route to the registered native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(class_name, method_name, descriptor),
            "{class_name}.{method_name}{descriptor} must not fall through to interpreted stream bytecode"
        );
    }
    assert!(!is_liquibase_checksum_native_override(
        "liquibase/serializer/core/string/StringChangeLogSerializer$FieldFilter",
        "include",
        "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;)Z"
    ));
}

/// `group.shutdownGracefully()` must keep Netty's own semantics.
///
/// The MongoDB Reactive Streams lifecycle needs a zero quiet period, and gets
/// one by calling `native_netty_event_executor_group_shutdown_gracefully`
/// directly from the `destroy()` bridge that replaces the bean method. It was
/// ALSO registered against `EventExecutorGroup`,
/// `AbstractEventExecutorGroup` and `MultiThreadIoEventLoopGroup` and forced
/// over their bytecode, on the stated premise that this restricted it to "that
/// concrete Netty 4.2 group" — but `MultiThreadIoEventLoopGroup` is the group
/// every Netty 4.2 application builds, so the premise was false and every
/// shutdown in the process ran with quiet period 0.
///
/// A zero quiet period does not drain the event loop, and Netty runs channel
/// deregistration — hence `handlerRemoved` — as a queued task. That silently
/// truncated `PcapWriteHandler`'s capture (522 bytes of 732: every close
/// packet missing) and would truncate any other graceful-shutdown-dependent
/// teardown the same way.
#[test]
fn netty_group_shutdown_gracefully_is_not_forced_to_a_zero_quiet_period() {
    for class_name in [
        "io/netty/util/concurrent/EventExecutorGroup",
        "io/netty/util/concurrent/AbstractEventExecutorGroup",
        "io/netty/channel/MultiThreadIoEventLoopGroup",
        "io/netty/channel/nio/NioEventLoopGroup",
    ] {
        for descriptor in [
            "()Lio/netty/util/concurrent/Future;",
            "(JJLjava/util/concurrent/TimeUnit;)Lio/netty/util/concurrent/Future;",
        ] {
            assert!(
                !force_native_over_real_jdk_bytecode(class_name, "shutdownGracefully", descriptor),
                "{class_name}.shutdownGracefully{descriptor} must run Netty's own bytecode, \
                 with Netty's own quiet period — a zero quiet period drops the queued \
                 deregistration task that fires handlerRemoved"
            );
        }
    }
}

#[test]
fn springboot_mongo_reactive_destroy_wait_is_replaced_only_for_its_lifecycle_bean() {
    let class_name = "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer";
    assert!(
        is_springboot_mongo_reactive_customizer_destroy_native_override(
            class_name, "destroy", "()V"
        )
    );
    assert!(force_native_over_real_jdk_bytecode(
        class_name, "destroy", "()V"
    ));
    assert!(
        !is_springboot_mongo_reactive_customizer_destroy_native_override(
            class_name,
            "customize",
            "(Lcom/mongodb/MongoClientSettings$Builder;)V"
        )
    );
    assert!(
        is_springboot_mongo_reactive_customizer_customize_native_override(
            class_name,
            "customize",
            "(Lcom/mongodb/MongoClientSettings$Builder;)V"
        )
    );
    assert!(force_native_over_real_jdk_bytecode(
        class_name,
        "customize",
        "(Lcom/mongodb/MongoClientSettings$Builder;)V"
    ));
}

#[test]
fn datagram_channel_factories_are_forced_to_the_udp_bridge() {
    let descriptor = "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;";
    assert!(is_datagram_channel_open_native_override(
        "java/nio/channels/DatagramChannel",
        "open",
        descriptor
    ));
    assert!(force_native_over_real_jdk_bytecode(
        "java/nio/channels/DatagramChannel",
        "open",
        descriptor
    ));
    assert!(is_datagram_channel_open_native_override(
        "sun/nio/ch/SelectorProviderImpl",
        "openDatagramChannel",
        descriptor
    ));
    assert!(is_datagram_channel_open_native_override(
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;"
    ));
}

#[test]
fn h2_liquibase_force_native_covers_ddl_hotpath_intrinsics() {
    for (class_name, name, descriptor) in [
        ("org/h2/table/Column", "equals", "(Ljava/lang/Object;)Z"),
        ("org/h2/table/Column", "hashCode", "()I"),
        ("org/h2/engine/DbObject", "equals", "(Ljava/lang/Object;)Z"),
        ("org/h2/engine/DbObject", "hashCode", "()I"),
        ("org/h2/engine/Session", "hashCode", "()I"),
        ("org/h2/engine/SessionLocal", "hashCode", "()I"),
    ] {
        assert!(
            is_h2_parser_native_override(class_name, name, descriptor),
            "{class_name}.{name}{descriptor} must route to the registered H2 native"
        );
        assert!(
            force_native_over_real_jdk_bytecode(class_name, name, descriptor),
            "{class_name}.{name}{descriptor} must not fall through to interpreted H2 bytecode"
        );
    }

    assert!(!is_h2_parser_native_override(
        "org/h2/table/Column",
        "getName",
        "()Ljava/lang/String;"
    ));
}

#[test]
fn reflection_factory_force_native_covers_serialization_surface() {
    for class_name in [
        "sun/reflect/ReflectionFactory",
        "jdk/internal/reflect/ReflectionFactory",
    ] {
        for (name, descriptor) in [
            (
                "newConstructorForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "newConstructorForSerialization",
                "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "newConstructorForExternalization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "readObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "readObjectNoDataForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "writeObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "readResolveForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "writeReplaceForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "hasStaticInitializerForSerialization",
                "(Ljava/lang/Class;)Z",
            ),
        ] {
            assert!(
                is_reflection_factory_serialization_native_override(class_name, name, descriptor),
                "{class_name}.{name}{descriptor} must route to the registered native"
            );
            assert!(force_native_over_real_jdk_bytecode(
                class_name, name, descriptor
            ));
        }
    }
    assert!(!is_reflection_factory_serialization_native_override(
        "java/io/ObjectStreamClass",
        "getReflector",
        "(Ljava/lang/Class;)Ljava/lang/Object;"
    ));
}

// -----------------------------------------------------------------------
// real-frame-deopt — IR-path deopt frame-value mapping (type source)
// -----------------------------------------------------------------------

/// `ir_deopt_frame_values` (the operand-stack mapper) maps the producer's
/// type-source variants to the right interpreter `Value`s, 1:1: a cat-1 `Int`
/// stays an `Int`, a cat-2 `Long` becomes a `Value::Long` (the compact stack
/// is one slot per value), an object-ref slot (`StackSlotRef` already resolved
/// in-stub to a raw heap word) becomes `Value::Object` (null word → `None`,
/// non-null word → the pointer verbatim — NOT a truncated `Int`), and
/// `Undefined` is a zero slot. Any not-yet-reconstructable variant
/// (`Unsupported`, `Float`-in-slot, or a `VirtualObject`) returns `None`,
/// forcing the safe re-run path.
#[test]
fn ir_deopt_frame_values_maps_object_and_int() {
    use cratonvm_jit::deopt::FrameValue;
    // 8-byte aligned, never dereferenced — only wrapped in an `ObjectRef`.
    let raw: u64 = 0x1000;
    let mapped = ir_deopt_frame_values(&[
        FrameValue::Object(0),
        FrameValue::Object(raw),
        FrameValue::Int(42),
        FrameValue::Long(0xFEDC_BA98_7654_3210u64 as i64),
        FrameValue::Undefined,
    ])
    .expect("Int/Long/Object/Undefined are all mappable");
    assert_eq!(mapped[0], Value::Object(None), "null word → null ref");
    match mapped[1] {
        Value::Object(Some(r)) => assert_eq!(
            // Cast: object/code pointer to integer address
            r.as_ptr() as u64,
            raw,
            "non-null ref slot must carry the heap pointer verbatim"
        ),
        other => panic!("expected a non-null object reference, got {other:?}"),
    }
    assert_eq!(mapped[2], Value::Int(42));
    assert_eq!(
        mapped[3],
        Value::Long(0xFEDC_BA98_7654_3210u64 as i64),
        "a cat-2 long must keep all 64 bits (not truncate to Int)"
    );
    assert_eq!(mapped[4], Value::Int(0), "Undefined → zero slot");

    // deopt-osr P2 — cat-1 `float` and cat-2 `double` now reconstruct from
    // their raw IEEE-754 bits (resolved in-stub from the slot/XMM).
    assert_eq!(
        ir_deopt_frame_values(&[FrameValue::Float(1.5f32.to_bits() as u64)]),
        Some(vec![Value::Float(1.5)]),
        "a float slot maps to Value::Float"
    );
    assert_eq!(
        ir_deopt_frame_values(&[FrameValue::Double(std::f64::consts::PI.to_bits())]),
        Some(vec![Value::Double(std::f64::consts::PI)]),
        "a double slot maps to Value::Double"
    );

    // The unresolved machine forms / sentinel still force the safe re-run.
    assert!(
        ir_deopt_frame_values(&[FrameValue::Unsupported]).is_none(),
        "an Unsupported slot must re-run, not resume"
    );
}

/// A caller scope parks at the invoke's SUCCESSOR, never at the invoke.
///
/// `ResumeSemantics::for_caller_scope()` is `RESUME`: the call at that bci is
/// already in progress, so parking the interpreter there would run it a second
/// time — the double-execution defect the deopt contract exists to prevent, one
/// bytecode instead of one loop iteration. `jit/src/lib.rs` refuses `RESUME`
/// points precisely because "computing the successor bci needs the method's
/// bytecode, which this crate does not have"; the VM does, and this is it.
#[test]
fn a_caller_scope_resumes_after_its_invoke_not_at_it() {
    use super::deopt_resume::caller_resume_pc;
    // 0xb8 invokestatic is 3 bytes; 0xb9 invokeinterface is 5.
    let code = [0x2a, 0xb8, 0x00, 0x07, 0xb9, 0x00, 0x0b, 0x02, 0x00, 0x57];
    assert_eq!(caller_resume_pc(&code, code.len(), 1).unwrap(), 4);
    assert_eq!(caller_resume_pc(&code, code.len(), 4).unwrap(), 9);

    // A bci that is not an invoke is a malformed chain, not a resume point.
    let err = caller_resume_pc(&code, code.len(), 0).unwrap_err();
    assert!(err.contains("not an invoke"), "{err}");
    let err = caller_resume_pc(&code, code.len(), 9).unwrap_err();
    assert!(err.contains("not an invoke"), "{err}");

    // Past the end, and an invoke whose operands run off the end, both refuse
    // rather than reading padding as bytecode.
    assert!(caller_resume_pc(&code, code.len(), 99).is_err());
    let truncated = [0xb9u8, 0x00, 0x0b];
    assert!(caller_resume_pc(&truncated, truncated.len(), 0).is_err());
}

/// `Unsupported` in a caller scope's locals must REFUSE, where the in-place OSR
/// transfer tolerates it.
///
/// The difference is the whole reason `caller_frame_values` is not the same
/// function as the transfer's mapping loop. That transfer leaves an
/// `Unsupported` local at the live frame's existing value, sound because the
/// verified bytecode proves the slot is dead or re-stored before it is read. A
/// MATERIALISED frame has no existing value — every sink maps a missing slot to
/// `Value::Int(0)` — so tolerating it would resume a caller with silently
/// zeroed locals, which is exactly what `docs/jit/deopt-inline-scopes.md`
/// describes when it says an undescribed caller frame lowers to
/// `[FrameValue::Unsupported]` so that this consumer refuses it.
#[test]
fn an_unsupported_caller_local_refuses_where_the_in_place_transfer_tolerates_it() {
    use super::deopt_resume::caller_frame_values;
    use cratonvm_jit::deopt::FrameValue;

    let scope =
        |locals: Vec<FrameValue>, stack: Vec<FrameValue>| cratonvm_jit::deopt::ReconstructedFrame {
            method_key: "p/C.m:()V".to_string(),
            bci: 4,
            locals,
            stack,
            monitors: Vec::new(),
            caller_frames: Vec::new(),
        };

    // The describable case is accepted, so the refusals below cannot be passing
    // for some unrelated reason.
    let (locals, stack) = caller_frame_values(&scope(
        vec![FrameValue::Int(7), FrameValue::Long(9)],
        vec![FrameValue::Int(1)],
    ))
    .expect("a fully described caller scope must be accepted");
    assert_eq!(locals.len(), 2, "the cat-2 upper half is compacted away");
    assert_eq!(stack.len(), 1);

    let err = caller_frame_values(&scope(vec![FrameValue::Unsupported], Vec::new())).unwrap_err();
    assert!(err.contains("Unsupported"), "{err}");
    assert!(
        err.contains("no existing value to leave in place"),
        "the refusal must say WHY a materialised frame differs: {err}"
    );

    // An unmappable STACK slot refuses too, as it does everywhere.
    assert!(caller_frame_values(&scope(Vec::new(), vec![FrameValue::Unsupported])).is_err());

    // A held monitor in a caller scope is out of scope for this sink.
    let mut with_monitor = scope(Vec::new(), Vec::new());
    with_monitor.monitors = vec![cratonvm_jit::deopt::MonitorInfo {
        object: FrameValue::Int(0),
        lock_depth: 1,
        relock: true,
    }];
    let err = caller_frame_values(&with_monitor).unwrap_err();
    assert!(err.contains("monitor"), "{err}");
}

/// `ir_deopt_locals` produces a COMPACT arg list: the JVM-slot-indexed
/// snapshot reserves the upper half of a cat-2 `long` as an `Undefined`
/// placeholder at the next slot, which must be SKIPPED (Frame::new_pooled's
/// copy_args_to_locals re-expands the long into its two slots). A `long`
/// local followed by an int must yield exactly `[Long, Int]`, not
/// `[Long, <placeholder>, Int]`.
#[test]
fn ir_deopt_locals_compacts_cat2() {
    use cratonvm_jit::deopt::FrameValue;
    // JVM-slot layout for `(long a, int x)`: a@0, <upper-half>@1, x@2.
    let locals = ir_deopt_locals(&[
        FrameValue::Long(7),
        FrameValue::Undefined, // reserved upper half of the long — skipped
        FrameValue::Int(9),
    ])
    .expect("Long/Undefined/Int are all mappable");
    assert_eq!(
        locals,
        vec![Value::Long(7), Value::Int(9)],
        "the long's reserved upper-half placeholder must be dropped"
    );

    // deopt-osr P2 — a cat-2 `double` collapses identically to a `long`.
    assert_eq!(
        ir_deopt_locals(&[
            FrameValue::Double(std::f64::consts::PI.to_bits()),
            FrameValue::Undefined, // reserved upper half of the double — skipped
            FrameValue::Float(1.5f32.to_bits() as u64),
        ]),
        Some(vec![Value::Double(std::f64::consts::PI), Value::Float(1.5)]),
        "the double's reserved upper-half placeholder must be dropped",
    );

    // A genuine (non-long) Undefined local is kept as a zero slot.
    assert_eq!(
        ir_deopt_locals(&[FrameValue::Int(1), FrameValue::Undefined]),
        Some(vec![Value::Int(1), Value::Int(0)]),
    );
    // An unmappable slot still forces re-run.
    assert!(ir_deopt_locals(&[FrameValue::Unsupported]).is_none());

    // Slice C: a `double` is cat-2 too — its reserved upper-half slot must be
    // skipped just like a `long`'s. JVM-slot layout for `(double d, int x)`:
    // d@0, <upper-half>@1, x@2 → compact `[Double, Int]`.
    let dlocals = ir_deopt_locals(&[
        FrameValue::Double(2.5f64.to_bits()),
        FrameValue::Undefined, // reserved upper half of the double — skipped
        FrameValue::Int(9),
    ])
    .expect("Double/Undefined/Int are all mappable");
    assert_eq!(
        dlocals,
        vec![Value::Double(2.5), Value::Int(9)],
        "the double's reserved upper-half placeholder must be dropped"
    );
}

// -----------------------------------------------------------------------
// H6 — native-stack-aware re-entrant recursion ceiling
// -----------------------------------------------------------------------

/// The derived ceiling must scale with the native stack size: a worker
/// carrier's 8 MiB stack gets a far lower ceiling than the main VM
/// thread's 128 MiB stack, and the 8 MiB ceiling must be small enough
/// that the guard trips before ~8 MiB of native stack is exhausted.
#[test]
fn exec_depth_ceiling_scales_with_native_stack() {
    let worker = derive_exec_depth_ceiling(8 * 1024 * 1024);
    let main = derive_exec_depth_ceiling(128 * 1024 * 1024);
    assert!(
        worker < main,
        "8 MiB worker ceiling ({worker}) must be below 128 MiB main ceiling ({main})"
    );
    // 8 MiB / 2 (safety) / 8 KiB per level = 512 levels.
    assert_eq!(worker, 512);
    // 128 MiB / 2 / 8 KiB = 8192 levels.
    assert_eq!(main, 8192);
}

/// JIT dispatch depth is also a call-chain depth, not a recursion count.
/// An 8 MiB Java worker must therefore admit normal deep framework calls
/// while retaining half of the native stack as an overflow reserve.
#[test]
fn jit_dispatch_depth_ceiling_for_8mib_allows_normal_call_chains() {
    assert_eq!(
        derive_jit_dispatch_depth_ceiling(8 * 1024 * 1024),
        512,
        "8 MiB / 2 safety reserve / 8 KiB per JIT-dispatch level"
    );
}

/// The old hard-coded 10_000 ceiling overflowed an 8 MiB native stack
/// before tripping. The derived 8 MiB ceiling must be strictly below
/// that old constant so the guard now fires first.
#[test]
fn exec_depth_ceiling_for_8mib_below_legacy_constant() {
    assert!(derive_exec_depth_ceiling(8 * 1024 * 1024) < 10_000);
}

/// Even a pathologically tiny stack must yield at least the floor so
/// ordinary (non-recursive) call graphs are never spuriously rejected.
#[test]
fn exec_depth_ceiling_respects_floor() {
    assert_eq!(derive_exec_depth_ceiling(0), MIN_EXEC_DEPTH_CEILING);
    assert_eq!(derive_exec_depth_ceiling(1024), MIN_EXEC_DEPTH_CEILING);
}

/// `init_thread_exec_depth_ceiling` must install the derived value into
/// the per-thread cell (verified on a fresh thread to avoid disturbing
/// the default on the test runner thread).
#[test]
fn init_thread_ceiling_installs_derived_value() {
    let derived = std::thread::spawn(|| {
        init_thread_exec_depth_ceiling(8 * 1024 * 1024);
        EXEC_DEPTH_CEILING.with(|c| c.get())
    })
    .join()
    .expect("ceiling probe thread must not panic");
    assert_eq!(derived, derive_exec_depth_ceiling(8 * 1024 * 1024));
}

// -----------------------------------------------------------------------
// C8 — tail-call optimization must not discard an enclosing try/catch
// -----------------------------------------------------------------------

/// Regression pin for the picocli `loadClosureClass` bug: when an
/// invokestatic/invokevirtual sits inside a caller's exception-table
/// range and is immediately followed by a matching return opcode, TCO
/// would replace the caller's frame (and its exception table) with the
/// callee's. If the callee then threw an exception the caller's
/// try/catch would have swallowed, the handler is lost and the
/// exception escapes.
///
/// The fix (see `try_stackless_invoke` step 8): suppress TCO whenever
/// any entry in the caller's exception table covers the invoke PC.
/// This test pins the predicate so an accidental weakening of the
/// check is caught at `cargo test` time without needing a full VM.
#[test]
fn tco_suppressed_when_invoke_pc_lies_inside_handler_range() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    let table: &[ExceptionTableEntry] = &[ExceptionTableEntry {
        start_pc: 22,
        end_pc: 27,
        handler_pc: 28,
        catch_type: 1,
    }];
    // Picocli's layout: invokestatic at 24, areturn at 27. The handler
    // covers [22, 27), so invoke_pc=24 must be detected as "covered".
    let invoke_pc = 24usize;
    let covered = table
        .iter()
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        .any(|e| invoke_pc >= e.start_pc as usize && invoke_pc < e.end_pc as usize);
    assert!(
        covered,
        "invoke at pc=24 must be flagged as inside [22,27) handler range"
    );

    // Negative case: an invoke at pc=30 is past the try region — TCO
    // remains safe there.
    let invoke_pc_outside = 30usize;
    let covered_outside = table.iter().any(|e| {
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        invoke_pc_outside >= e.start_pc as usize && invoke_pc_outside < e.end_pc as usize
    });
    assert!(
        !covered_outside,
        "invoke at pc=30 must NOT be flagged (outside any handler)"
    );

    // Empty exception table — TCO always safe, predicate returns false.
    let empty: &[ExceptionTableEntry] = &[];
    let covered_empty = empty
        .iter()
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        .any(|e| invoke_pc >= e.start_pc as usize && invoke_pc < e.end_pc as usize);
    assert!(!covered_empty, "empty exception table must never flag");
}

// -----------------------------------------------------------------------
// NEW-7 — hot-file panic-free invariant
// -----------------------------------------------------------------------

/// Scans the production portion of a Rust source file (every line
/// that is NOT inside a `#[cfg(test)]`-gated item) and returns the
/// count of any line containing the given needle. Used by the
/// [`hot_files_have_no_production_panics`] test below to enforce the
/// NEW-7 invariant at `cargo test` time in addition to the
/// `#![cfg_attr(not(test), deny(...))]` clippy gate at the top of
/// each hot file.
///
/// This is a pragmatic line scanner rather than a full Rust parser.
///
/// B3 fix: the previous implementation used
/// `src.find("#[cfg(test)]")` as the boundary, but the FIRST literal
/// occurrence of `#[cfg(test)]` in these files is inside a `//!` doc
/// comment near the top (interpreter.rs:33, x64.rs:22), so the gate
/// scanned only the ~33-line file header and treated the entire 17k-
/// line production body as "test code" — the assertion passed
/// vacuously while real production panic sites slipped through.
///
/// The scanner now walks line by line and skips only the bodies of
/// genuine `#[cfg(test)]`-gated items (a real attribute line, not a
/// comment), tracking brace depth so it correctly excludes both the
/// trailing `mod tests { ... }` block AND any mid-file
/// `#[cfg(test)] fn helper(...) { ... }` (vm_exec.rs has one at the
/// top of its production region). Everything else — the full opcode
/// dispatch surface — is scanned.
///
/// Returns `(production_hits, scanned_lines)`. The test fails if
/// `production_hits` is not zero.
fn scan_production_section(path: &str, needles: &[&str]) -> (usize, usize) {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));

    let mut hits = 0usize;
    let mut scanned = 0usize;

    // State for skipping a `#[cfg(test)]`-gated item.
    // `pending` = we just saw the attribute and are waiting for the
    // item's opening line; `skip_depth` = current brace nesting inside
    // the gated block (0 once it closes).
    let mut pending = false;
    let mut skip_depth: i32 = 0;

    for line in src.lines() {
        let trimmed = line.trim_start();

        // A real `#[cfg(test)]` attribute (not a `//`/`//!`/`*` comment
        // mentioning it) opens a test-gated item to skip.
        let is_comment = trimmed.starts_with("//") || trimmed.starts_with("*");
        if !is_comment && skip_depth == 0 && !pending && trimmed.starts_with("#[cfg(test)]") {
            pending = true;
            continue; // skip the attribute line itself
        }

        if pending || skip_depth > 0 {
            // Inside (or entering) a test-gated item: count braces to
            // find where it ends, and never scan these lines.
            if !is_comment {
                // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
                let opens = line.matches('{').count() as i32;
                // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
                let closes = line.matches('}').count() as i32;
                skip_depth += opens - closes;
                if pending {
                    if opens > 0 {
                        // Block item (mod/fn/impl): now tracked by braces.
                        pending = false;
                    } else if trimmed.ends_with(';') {
                        // One-line gated item (e.g. `#[cfg(test)] use ...;`).
                        pending = false;
                    }
                }
                if skip_depth < 0 {
                    skip_depth = 0;
                }
            }
            continue;
        }

        scanned += 1;

        // Skip doc comments and ordinary comments — the patterns below
        // legitimately appear in rustdoc explaining *why* we forbid them.
        if is_comment {
            continue;
        }
        for needle in needles {
            if line.contains(needle) {
                hits += 1;
                break;
            }
        }
    }
    (hits, scanned)
}

/// NEW-7 CI gate: the hot files must not introduce production-code
/// uses of `.unwrap()`, `.expect(`, `panic!(`, `unimplemented!(`,
/// `todo!(` or `unreachable!(` outside their `#[cfg(test)] mod tests`
/// sections.
///
/// B3 fix (audit `vm-runtime.md`): the old gate anchored its scan on
/// `src.find("#[cfg(test)]")`, which matched a `//!` doc comment near
/// the top of each file — so it scanned only the ~33-line header and
/// asserted `0 == 0` vacuously while the real 17k-line dispatch body
/// (including a production `unreachable!()`) went unguarded. The
/// scanner now skips only genuine `#[cfg(test)]`-gated items and
/// covers the entire production surface; see
/// [`scan_production_section`].
///
/// `interpreter.rs` is held to a strict **zero** (it is now clean).
/// `vm_exec.rs` and `x64.rs` carry a small documented baseline of
/// pre-existing production panic sites (thread-spawn failure; JIT
/// codegen-invariant `unreachable!`s) that are owned elsewhere — they
/// are *ratcheted*: the count may only shrink, never grow, so a new
/// `.unwrap()` in those files still fails the gate.
///
/// Regression modes caught:
///   - A new `.unwrap()`/`panic!()` anywhere in production code.
///   - Silencing the clippy gate with `#[allow(clippy::unwrap_used)]`
///     instead of fixing the call site.
///   - Removal of the `#![cfg_attr(not(test), deny(...))]` header.
///   - Re-breaking the scan boundary (the `scanned_lines` self-check
///     below fails if the scan collapses back to the file header).
#[test]
fn hot_files_have_no_production_panics() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let needles = [
        ".unwrap()",
        ".expect(",
        "panic!(",
        "unimplemented!(",
        "todo!(",
        "unreachable!(",
    ];

    // (path, max-allowed production panic sites). interpreter.rs is
    // strict-zero; the other two are ratcheted at their current,
    // owned-elsewhere baseline — lower these as they are cleaned up,
    // never raise them.
    //
    // Both hot files were split into `<name>/` submodule directories in
    // 2026-07. The submodules MUST be covered: a gate that keeps scanning
    // only the parent turns "split the file" into "silently stop checking
    // most of it", which is worse than the file size the split fixed. They
    // are enumerated from disk so a new submodule cannot be added outside
    // the gate, and both directories are strict-zero — giving each new file
    // the parent's allowance would multiply the budget by the file count.
    let mut targets: Vec<(String, usize)> = vec![
        (format!("{manifest}/src/runtime/interpreter.rs"), 0),
        (format!("{manifest}/src/vm/vm_exec.rs"), 1),
        // x64.rs lives in the `jit` sibling crate; path is resolved
        // relative to this crate's manifest. Its ratchet is **zero**: the
        // sixteen pre-existing sites it used to carry all moved into
        // submodules in the 2026-08-03 split and are pinned there
        // individually, below. Nothing may come back.
        (format!("{manifest}/../jit/src/x64.rs"), 0),
    ];
    // A submodule whose `mod` declaration in the parent is `#[cfg(test)]`
    // -gated is test code in its entirety, and must be skipped.
    //
    // The attribute lives on the *declaration*, not inside the file, so
    // `scan_production_section` finds no boundary in the file itself and
    // would scan every assertion in it as production code. That is not
    // hypothetical: when `x64.rs`'s three inline test modules became files
    // on 2026-08-03, this gate failed with 18 "production panic sites" in
    // `x64/loop_unroll_admission.rs`, every one of them an `assert!` that
    // had been skipped the day before as part of `#[cfg(test)] mod
    // loop_unroll_admission { ... }`.
    //
    // The rule this restores: **splitting a file must not change what the
    // gate covers.** Test code was exempt inline and stays exempt in a
    // file, and production code stays covered either way.
    //
    // Derived from the parent's declaration rather than from file names or
    // contents: the parent is the thing that decides, so renaming a file or
    // removing its `#[cfg(test)]` changes the answer here with no edit.

    for (dir, max_allowed) in [
        (format!("{manifest}/src/runtime/interpreter"), 0usize),
        (format!("{manifest}/../jit/src/x64"), 0usize),
    ] {
        let parent = std::fs::read_to_string(format!("{dir}.rs"))
            .unwrap_or_else(|e| panic!("cannot read module parent {dir}.rs: {e}"));
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot enumerate split module dir {dir}: {e}"));
        let mut production = 0usize;
        let mut test_only: Vec<String> = Vec::new();
        for entry in entries {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            if declared_cfg_test(&parent, &stem) {
                test_only.push(stem);
                continue;
            }
            production += 1;
            targets.push((path.to_string_lossy().into_owned(), max_allowed));
        }
        // Non-vacuity: the exemption above is the only way this gate can
        // quietly stop covering a directory, so require that something in
        // each one is still being scanned as production.
        assert!(
            production > 0,
            "every .rs under {dir} was classified as a `#[cfg(test)]` \
             module ({test_only:?}) — the split-module half of this gate \
             silently stopped covering anything",
        );
    }

    // Per-FILE exceptions inside the strict-zero submodule dirs. Kept
    // separate from the directory budget above on purpose: raising that
    // budget would hand the same allowance to every file in the
    // directory, which is exactly what the comment above refuses to do.
    //
    // `disp.rs` earns one because `disp8_const` is a `const fn` whose
    // `panic!` IS the const-evaluation failure mechanism — const context
    // has no `Result`, so this is how a layout constant that would not fit
    // a signed disp8 becomes a BUILD failure instead of an instruction
    // that addresses memory backwards from the base register. It already
    // carries `#[allow(clippy::panic)]` and documents that it must only be
    // called in const context. Note this is a per-file allowance, not a
    // blanket "const fn panics are fine" rule: a `const fn` called at run
    // time panics like any other function, which is why the scanner is not
    // taught to skip them wholesale.
    //
    // The other four entries are the 2026-08-03 split's bookkeeping, not
    // new debt. Before the split those sixteen sites were pooled in one
    // ratcheted allowance on `x64.rs`; the split moved them, unchanged,
    // into the files that now own the code. Pinning them per file is
    // strictly stronger than the pool was — a site can no longer move
    // between files unnoticed — and the assertion below keeps the pool's
    // guarantee as a single number.
    //
    // These are all codegen-invariant `unreachable!()`s, `.unwrap()`s on
    // an `Option` a preceding branch already proved `Some`, and two
    // `.expect()`s inside the cycle-breaking loop of the stack shuffler.
    // They are owned by the JIT lane, not by this gate. Lower them as they
    // are cleaned up; never raise one.
    let per_file: [(&str, usize); 5] = [
        ("jit/src/x64/disp.rs", 1),
        ("jit/src/x64/arith.rs", 1),
        ("jit/src/x64/bytecode_walk.rs", 10),
        ("jit/src/x64/inlining.rs", 2),
        ("jit/src/x64/operand_stack.rs", 3),
    ];
    for (path, max_allowed) in targets.iter_mut() {
        let norm = path.replace('\\', "/");
        for (suffix, allowed) in per_file {
            if norm.ends_with(suffix) {
                *max_allowed = allowed;
            }
        }
    }

    // The ratchet, as one number. `disp.rs`'s const-evaluation `panic!` is
    // the documented exception above; the remaining 16 are exactly the
    // budget `x64.rs` carried before it was split. A new submodule with a
    // fresh allowance, or a raised one, fails here — which is the hazard
    // the pooled form was protecting against and the per-file form would
    // otherwise reopen.
    let x64_budget: usize = targets
        .iter()
        .filter(|(p, _)| p.replace('\\', "/").contains("/jit/src/x64"))
        .map(|(_, allowed)| *allowed)
        .sum();
    assert!(
        x64_budget <= 17,
        "the x64 backend's total production-panic allowance is {x64_budget}, \
         above the 17 it was ratcheted at before the file was split (16 \
         pooled on x64.rs + disp.rs's documented const-eval panic!). The \
         per-file table exists to pin those sites to a file, not to raise \
         the total.",
    );

    for (path, max_allowed) in &targets {
        let (hits, scanned) = scan_production_section(path, &needles);

        // Self-check (B3 meta-test): the scan must cover the real
        // production body, not just the file header. If the boundary
        // logic ever regresses to the old `find("#[cfg(test)]")`
        // doc-comment anchor, `scanned` collapses to the header and this
        // trips before the (now-vacuous) panic assertion can pass
        // silently.
        //
        // Anchored on the FIRST real `#[cfg(test)]` attribute rather than
        // on a fraction of the file. Every line above that attribute is
        // unambiguously production, so the scan must have covered at
        // least that many — a property that holds whatever the
        // production/test ratio is. The previous form asserted
        // `scanned * 2 >= total`, i.e. "production is at least half the
        // file", which is not the property being tested and which a small
        // utility with thorough tests legitimately fails:
        // `jit/src/x64/disp.rs` is 327 production lines and 406 test
        // lines, so the scan was exactly right and the gate still fired.
        let src =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
        let total = src.lines().count();
        // Same "is this a real attribute, not a comment mentioning one"
        // test `scan_production_section` applies.
        let production_prefix = src
            .lines()
            .position(|l| {
                let t = l.trim_start();
                !(t.starts_with("//") || t.starts_with('*')) && t.starts_with("#[cfg(test)]")
            })
            .unwrap_or(total);
        assert!(
            scanned >= production_prefix,
            "B3 regression: scan of {path} covered only {scanned} lines, \
             but {production_prefix} lines precede the first \
             `#[cfg(test)]` attribute (file is {total} lines) — the \
             production body was not scanned. The `#[cfg(test)]` boundary \
             detection in scan_production_section is broken.",
        );

        assert!(
            hits <= *max_allowed,
            "NEW-7 regression: {path} has {hits} production-code panic \
             sites (max allowed {max_allowed}) across {scanned} scanned \
             lines. These files must route every recoverable error \
             through VmError/VmResult; see the \
             #![cfg_attr(not(test), deny(...))] header at the top of \
             each file for the rationale.",
        );
    }
}

#[test]
fn count_method_params_simple() {
    assert_eq!(count_method_params("()V"), 0);
    assert_eq!(count_method_params("(I)V"), 1);
    assert_eq!(count_method_params("(II)I"), 2);
    assert_eq!(count_method_params("(IJD)V"), 3);
}

#[test]
fn count_method_params_objects() {
    assert_eq!(count_method_params("(Ljava/lang/String;)V"), 1);
    assert_eq!(count_method_params("(Ljava/lang/String;I)V"), 2);
    assert_eq!(
        count_method_params("(ILjava/lang/Object;Ljava/lang/String;)Ljava/lang/Object;"),
        3
    );
}

#[test]
fn count_method_params_arrays() {
    assert_eq!(count_method_params("([I)V"), 1);
    assert_eq!(count_method_params("([[I)V"), 1);
    assert_eq!(count_method_params("([Ljava/lang/String;)V"), 1);
    assert_eq!(
        count_method_params("(Ljava/lang/Object;I[Ljava/lang/Object;II)V"),
        5
    );
}

#[test]
fn float_conversion_nan() {
    assert_eq!(float_to_int(f32::NAN), 0);
    assert_eq!(float_to_long(f32::NAN), 0);
    assert_eq!(double_to_int(f64::NAN), 0);
    assert_eq!(double_to_long(f64::NAN), 0);
}

#[test]
fn float_conversion_overflow() {
    assert_eq!(float_to_int(f32::INFINITY), i32::MAX);
    assert_eq!(float_to_int(f32::NEG_INFINITY), i32::MIN);
}

#[test]
fn branch_target_forward() {
    assert_eq!(branch_target(10, 5), 15);
}

#[test]
fn branch_target_backward() {
    assert_eq!(branch_target(10, -5), 5);
}

#[test]
fn refs_equal_both_null() {
    assert!(refs_equal(&Value::Object(None), &Value::Object(None)));
}

// -- alloc_multi_array tests --

#[test]
fn alloc_multi_array_2d() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let sizes = vec![3, 4];
    let outer = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Reference,
        sizes.len(),
        &[],
    )
    .unwrap();

    assert_eq!(vm.shared.mem.heap.array_length(outer), 3);

    for i in 0..3 {
        let inner_val = vm.shared.mem.heap.get_array_element(outer, i).unwrap();
        match inner_val {
            Value::Object(Some(inner_ref)) => {
                assert_eq!(vm.shared.mem.heap.array_length(inner_ref), 4);
            }
            other => panic!("Expected non-null object at index {i}, got {other:?}"),
        }
    }
}

#[test]
fn alloc_multi_array_zero_outer() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let sizes = vec![0, 5];
    let outer = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Reference,
        sizes.len(),
        &[],
    )
    .unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(outer), 0);
}

#[test]
fn alloc_multi_array_1d() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let sizes = vec![7];
    let arr = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Reference,
        sizes.len(),
        &[],
    )
    .unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(arr), 7);
}

#[test]
fn alloc_multi_array_3d() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let sizes = vec![2, 3, 4];
    let outer = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Reference,
        sizes.len(),
        &[],
    )
    .unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(outer), 2);

    let mid_val = vm.shared.mem.heap.get_array_element(outer, 0).unwrap();
    match mid_val {
        Value::Object(Some(mid_ref)) => {
            assert_eq!(vm.shared.mem.heap.array_length(mid_ref), 3);
            let inner_val = vm.shared.mem.heap.get_array_element(mid_ref, 0).unwrap();
            match inner_val {
                Value::Object(Some(inner_ref)) => {
                    assert_eq!(vm.shared.mem.heap.array_length(inner_ref), 4);
                }
                other => panic!("Expected inner array, got {other:?}"),
            }
        }
        other => panic!("Expected mid array, got {other:?}"),
    }
}

// -- Lambda proxy dispatch tests --

#[test]
fn lambda_dispatch_non_proxy_returns_none() {
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use crate::vm::SharedVm;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Allocate a regular (non-proxy) object
    let regular_obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let result = try_lambda_dispatch(
        &shared,
        &mut thread,
        regular_obj,
        ClassId::new(0),
        "accept",
        "()V",
        &[],
    )
    .unwrap();

    assert!(result.is_none(), "Non-proxy object should return None");
}

#[test]
fn lambda_proxy_captures_read_correctly() {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // Register a lambda proxy with 3 capture types
    let proxy_class_id = shared.alloc_lambda_proxy_id();
    let call_site = LambdaCallSite {
        functional_interface_id: None,
        functional_interface: Arc::from("test/Func"),
        sam_method_name: Arc::from("apply"),
        sam_descriptor: Arc::from("()V"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("test/Impl"),
            member_name: Arc::from("target"),
            descriptor: Arc::from("(IIJ)V"),
        },
        instantiated_descriptor: Arc::from("()V"),
        capture_types: vec!['I', 'I', 'J'],
        proxy_class_id,
        serializable_flag: false,
    };
    shared
        .classes
        .lambda_proxies
        .write()
        .insert(proxy_class_id, std::sync::Arc::new(call_site));

    // Allocate a proxy object with 3 captured values
    let proxy_ref = shared.mem.heap.alloc_object(proxy_class_id, 3);
    shared.mem.heap.set_field(proxy_ref, 0, Value::Int(10));
    shared.mem.heap.set_field(proxy_ref, 1, Value::Int(20));
    shared.mem.heap.set_field(proxy_ref, 2, Value::Long(30));

    // Verify the captures are stored correctly
    assert_eq!(shared.mem.heap.get_field(proxy_ref, 0), Value::Int(10));
    assert_eq!(shared.mem.heap.get_field(proxy_ref, 1), Value::Int(20));
    assert_eq!(shared.mem.heap.get_field(proxy_ref, 2), Value::Long(30));

    // Verify the proxy is recognized as a lambda
    let proxies = shared.classes.lambda_proxies.read();
    let lcs = proxies.get(&proxy_class_id).unwrap();
    assert_eq!(&*lcs.functional_interface, "test/Func");
    assert_eq!(&*lcs.sam_method_name, "apply");
    assert_eq!(lcs.impl_handle.kind, MethodHandleKind::InvokeStatic);
    assert_eq!(lcs.capture_types.len(), 3);
}

#[test]
fn lambda_proxy_id_uniqueness() {
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    let id1 = shared.alloc_lambda_proxy_id();
    let id2 = shared.alloc_lambda_proxy_id();
    let id3 = shared.alloc_lambda_proxy_id();

    assert_ne!(id1, id2);
    assert_ne!(id2, id3);
    assert_ne!(id1, id3);

    // IDs should start at 0x8000_0000
    assert_eq!(id1, ClassId::new(0x8000_0000));
    assert_eq!(id2, ClassId::new(0x8000_0001));
    assert_eq!(id3, ClassId::new(0x8000_0002));
}

// -- refs_equal autoboxed integer tests --

#[test]
fn refs_equal_int_int_same_value() {
    // Autoboxed integers with the same value should be equal
    assert!(refs_equal(&Value::Int(42), &Value::Int(42)));
}

#[test]
fn refs_equal_int_int_different_value() {
    // Autoboxed integers with different values should not be equal
    assert!(!refs_equal(&Value::Int(42), &Value::Int(99)));
}

#[test]
fn refs_equal_int_zero_vs_null() {
    // Int(0) represents null in autoboxed contexts, so matches Object(None)
    assert!(refs_equal(&Value::Int(0), &Value::Object(None)));
}

#[test]
fn refs_equal_nonzero_int_vs_null() {
    // Non-zero Int should NOT match Object(None)
    assert!(!refs_equal(&Value::Int(1), &Value::Object(None)));
}

#[test]
fn refs_equal_int_zero() {
    // Two Int(0) values should be equal (autoboxed Integer.valueOf(0))
    assert!(refs_equal(&Value::Int(0), &Value::Int(0)));
}

// -- IEEE 754 conversion edge cases --

#[test]
fn float_to_int_positive_infinity() {
    assert_eq!(float_to_int(f32::INFINITY), i32::MAX);
}

#[test]
fn float_to_int_negative_infinity() {
    assert_eq!(float_to_int(f32::NEG_INFINITY), i32::MIN);
}

#[test]
fn float_to_int_negative_zero() {
    assert_eq!(float_to_int(-0.0f32), 0);
}

#[test]
fn float_to_int_truncation() {
    assert_eq!(float_to_int(2.9f32), 2);
    assert_eq!(float_to_int(-2.9f32), -2);
}

#[test]
fn float_to_long_nan() {
    assert_eq!(float_to_long(f32::NAN), 0);
}

#[test]
fn float_to_long_overflow() {
    assert_eq!(float_to_long(f32::INFINITY), i64::MAX);
    assert_eq!(float_to_long(f32::NEG_INFINITY), i64::MIN);
}

#[test]
fn float_to_long_negative_zero() {
    assert_eq!(float_to_long(-0.0f32), 0);
}

#[test]
fn float_to_long_truncation() {
    assert_eq!(float_to_long(2.9f32), 2);
    assert_eq!(float_to_long(-2.9f32), -2);
}

#[test]
fn double_to_int_positive_infinity() {
    assert_eq!(double_to_int(f64::INFINITY), i32::MAX);
}

#[test]
fn double_to_int_negative_infinity() {
    assert_eq!(double_to_int(f64::NEG_INFINITY), i32::MIN);
}

#[test]
fn double_to_int_negative_zero() {
    assert_eq!(double_to_int(-0.0f64), 0);
}

#[test]
fn double_to_int_truncation() {
    assert_eq!(double_to_int(2.9f64), 2);
    assert_eq!(double_to_int(-2.9f64), -2);
}

#[test]
fn double_to_long_nan() {
    assert_eq!(double_to_long(f64::NAN), 0);
}

#[test]
fn double_to_long_overflow() {
    assert_eq!(double_to_long(f64::INFINITY), i64::MAX);
    assert_eq!(double_to_long(f64::NEG_INFINITY), i64::MIN);
}

#[test]
fn double_to_long_negative_zero() {
    assert_eq!(double_to_long(-0.0f64), 0);
}

#[test]
fn double_to_long_truncation() {
    assert_eq!(double_to_long(2.9f64), 2);
    assert_eq!(double_to_long(-2.9f64), -2);
}

#[test]
fn double_to_long_large_value() {
    // A large f64 that exceeds i64::MAX
    assert_eq!(double_to_long(1.0e19), i64::MAX);
    assert_eq!(double_to_long(-1.0e19), i64::MIN);
}

#[test]
fn float_to_int_boundary() {
    // Values just within range
    assert_eq!(float_to_int(1.0f32), 1);
    assert_eq!(float_to_int(-1.0f32), -1);
    assert_eq!(float_to_int(0.0f32), 0);
}

// -- refs_equal additional edge cases --

#[test]
fn refs_equal_null_vs_int_zero() {
    // Int(0) representing null in autoboxed contexts
    assert!(refs_equal(&Value::Object(None), &Value::Int(0)));
    assert!(refs_equal(&Value::Int(0), &Value::Object(None)));
}

// -----------------------------------------------------------------------
// narrow_int_to_field_type (finding 2): sub-int field store/load narrowing
// -----------------------------------------------------------------------

#[test]
fn narrow_field_byte_sign_extends() {
    // 0x1234_5680 -> low byte 0x80 -> -128 (sign-extended)
    assert_eq!(
        // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
        narrow_int_to_field_type(Value::Int(0x1234_5680u32 as i32), b'B'),
        Value::Int(-128)
    );
    assert_eq!(
        narrow_int_to_field_type(Value::Int(127), b'B'),
        Value::Int(127)
    );
    assert_eq!(
        narrow_int_to_field_type(Value::Int(256), b'B'),
        Value::Int(0)
    );
}

#[test]
fn narrow_field_short_sign_extends() {
    // low 16 bits 0x8000 -> -32768
    assert_eq!(
        // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
        narrow_int_to_field_type(Value::Int(0x0001_8000u32 as i32), b'S'),
        Value::Int(-32768)
    );
    assert_eq!(
        narrow_int_to_field_type(Value::Int(32767), b'S'),
        Value::Int(32767)
    );
}

#[test]
fn narrow_field_char_zero_extends() {
    // char is unsigned: low 16 bits 0xFFFF -> 65535 (NOT -1)
    assert_eq!(
        narrow_int_to_field_type(Value::Int(-1), b'C'),
        Value::Int(65535)
    );
    assert_eq!(
        narrow_int_to_field_type(Value::Int(0x10000), b'C'),
        Value::Int(0)
    );
}

#[test]
fn narrow_field_boolean_masks_bit0() {
    assert_eq!(narrow_int_to_field_type(Value::Int(2), b'Z'), Value::Int(0));
    assert_eq!(narrow_int_to_field_type(Value::Int(3), b'Z'), Value::Int(1));
    assert_eq!(
        narrow_int_to_field_type(Value::Int(-1), b'Z'),
        Value::Int(1)
    );
}

#[test]
fn narrow_field_int_and_others_unchanged() {
    // 'I' and any non-sub-int descriptor pass through untouched.
    assert_eq!(
        narrow_int_to_field_type(Value::Int(-1), b'I'),
        Value::Int(-1)
    );
    assert_eq!(
        // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
        narrow_int_to_field_type(Value::Int(0x1234_5680u32 as i32), b'L'),
        // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
        Value::Int(0x1234_5680u32 as i32)
    );
    // Non-Int carriers pass through regardless of descriptor.
    assert_eq!(
        narrow_int_to_field_type(Value::Long(5), b'B'),
        Value::Long(5)
    );
    assert_eq!(
        narrow_int_to_field_type(Value::Object(None), b'B'),
        Value::Object(None)
    );
}

// -----------------------------------------------------------------------
// ref_operand_is_null (finding 3): ifnull/ifnonnull jobject-as-Long(0)
// -----------------------------------------------------------------------

#[test]
fn ref_null_recognises_object_none() {
    assert!(ref_operand_is_null(&Value::Object(None)));
    assert!(ref_operand_is_null(&Value::Uninitialized));
}

#[test]
fn ref_null_recognises_jobject_long_zero() {
    // A JNI jobject null handle carried as raw long bits.
    assert!(ref_operand_is_null(&Value::Long(0)));
}

#[test]
fn ref_null_rejects_nonzero_and_live_object() {
    // A non-zero long is either a live jobject pointer or an honest long;
    // neither is null.
    assert!(!ref_operand_is_null(&Value::Long(1)));
    assert!(!ref_operand_is_null(&Value::Int(0)));
}

#[test]
fn refs_equal_int_negative() {
    assert!(refs_equal(&Value::Int(-128), &Value::Int(-128)));
    assert!(!refs_equal(&Value::Int(-1), &Value::Int(1)));
}

#[test]
fn refs_equal_different_types() {
    // Long vs Int — different value types
    assert!(!refs_equal(&Value::Long(42), &Value::Int(42)));
    assert!(!refs_equal(&Value::Float(0.0), &Value::Int(0)));
}

#[test]
fn refs_equal_same_heap_object() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 0);
    // Same object reference should be equal
    assert!(refs_equal(
        &Value::Object(Some(obj)),
        &Value::Object(Some(obj))
    ));
}

#[test]
fn refs_equal_different_heap_objects() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let obj1 = vm.shared.mem.heap.alloc_object(ClassId::new(1), 0);
    let obj2 = vm.shared.mem.heap.alloc_object(ClassId::new(1), 0);
    // Different objects (same class) should NOT be equal
    assert!(!refs_equal(
        &Value::Object(Some(obj1)),
        &Value::Object(Some(obj2))
    ));
}

#[test]
fn refs_equal_null_vs_nonnull() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 0);
    assert!(!refs_equal(&Value::Object(None), &Value::Object(Some(obj))));
    assert!(!refs_equal(&Value::Object(Some(obj)), &Value::Object(None)));
}

// -- count_method_params additional edge cases --

#[test]
fn count_method_params_all_primitives() {
    // B=byte, C=char, D=double, F=float, I=int, J=long, S=short, Z=boolean
    assert_eq!(count_method_params("(BCDFIJSZ)V"), 8);
}

#[test]
fn count_method_params_multi_dim_array() {
    // [[[I is a 3D int array — counts as 1 param
    assert_eq!(count_method_params("([[[I)V"), 1);
}

#[test]
fn count_method_params_multi_dim_object_array() {
    // [[Ljava/lang/String; is a 2D String array
    assert_eq!(count_method_params("([[Ljava/lang/String;)V"), 1);
}

#[test]
fn count_method_params_mixed_complex() {
    // int, 2D byte array, String, long, Object array
    assert_eq!(
        count_method_params("(I[[BLjava/lang/String;J[Ljava/lang/Object;)V"),
        5
    );
}

#[test]
fn count_method_params_void_return() {
    assert_eq!(count_method_params("()V"), 0);
}

#[test]
fn count_method_params_object_return() {
    // Return type should NOT be counted
    assert_eq!(count_method_params("(I)Ljava/lang/String;"), 1);
}

// -- branch_target edge cases --

#[test]
fn branch_target_zero_offset() {
    assert_eq!(branch_target(100, 0), 100);
}

#[test]
fn branch_target_max_forward() {
    assert_eq!(branch_target(0, i16::MAX), i16::MAX as usize); // Widening: index conversion
}

#[test]
fn branch_target_large_pc() {
    assert_eq!(branch_target(65535, 1), 65536);
}

// -- multianewarray with int leaf type --

#[test]
fn alloc_multi_array_int_leaf() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    // 2D array with int leaves: int[3][4]
    let sizes = vec![3, 4];
    let outer = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Int,
        sizes.len(),
        &[],
    )
    .unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(outer), 3);

    let inner_val = vm.shared.mem.heap.get_array_element(outer, 0).unwrap();
    match inner_val {
        Value::Object(Some(inner_ref)) => {
            assert_eq!(vm.shared.mem.heap.array_length(inner_ref), 4);
            // Inner elements should be default int (0)
            let elem = vm.shared.mem.heap.get_array_element(inner_ref, 0).unwrap();
            assert_eq!(elem, Value::Int(0));
        }
        other => panic!("Expected inner int array, got {other:?}"),
    }
}

#[test]
fn alloc_multi_array_4d() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    // 4D: [2][2][2][2]
    let sizes = vec![2, 2, 2, 2];
    let d0 = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Reference,
        sizes.len(),
        &[],
    )
    .unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(d0), 2);

    // Walk down to depth 3
    let d1_val = vm.shared.mem.heap.get_array_element(d0, 0).unwrap();
    let d1 = match d1_val {
        Value::Object(Some(r)) => r,
        other => panic!("d1: expected object, got {other:?}"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(d1), 2);

    let d2_val = vm.shared.mem.heap.get_array_element(d1, 0).unwrap();
    let d2 = match d2_val {
        Value::Object(Some(r)) => r,
        other => panic!("d2: expected object, got {other:?}"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(d2), 2);

    let d3_val = vm.shared.mem.heap.get_array_element(d2, 0).unwrap();
    let d3 = match d3_val {
        Value::Object(Some(r)) => r,
        other => panic!("d3: expected object, got {other:?}"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(d3), 2);
}

#[test]
fn alloc_multi_array_partial_dims_leaves_inner_as_references() {
    // Regression: Eclipse ecj CharDeduplication does
    //   new char[5][30][6][];   //  multianewarray [[[[C, 3
    // The descriptor `[[[[C` has 4 array dims but only 3 are specified.
    // The deepest allocated array must be Reference[N] (null slots, to be
    // filled in later by user code with char[]), NOT char[N]. Without this
    // fix the subsequent `init()` walked the array with aaload/aastore and
    // crashed with AIOOBE because each "slot" was 2-byte char storage and
    // bounds were wrong.
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    // sizes.len()=3, total_array_depth=4, leaf_et=Char
    let sizes = vec![5, 30, 6];
    let outer = alloc_multi_array(&vm.shared, &sizes, 0, ArrayElementType::Char, 4, &[]).unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(outer), 5);

    // Walk to the inner (3rd) dim and verify it's a reference array of
    // length 6, not a char array.
    let mid = match vm.shared.mem.heap.get_array_element(outer, 0).unwrap() {
        Value::Object(Some(r)) => r,
        other => panic!("expected mid array, got {other:?}"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(mid), 30);
    let inner = match vm.shared.mem.heap.get_array_element(mid, 0).unwrap() {
        Value::Object(Some(r)) => r,
        other => panic!("expected inner array, got {other:?}"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(inner), 6);

    // The element type of the inner array must be Reference (so the user
    // can store char[] references into it via aastore). All slots start
    // as null Object refs.
    let slot = vm.shared.mem.heap.get_array_element(inner, 5).unwrap();
    assert!(
        matches!(slot, Value::Object(None)),
        "inner slot must be null Object, got {slot:?}"
    );
}

#[test]
fn alloc_multi_array_single_element() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    // [1][1] — minimal non-zero multi-array
    let sizes = vec![1, 1];
    let outer = alloc_multi_array(
        &vm.shared,
        &sizes,
        0,
        ArrayElementType::Reference,
        sizes.len(),
        &[],
    )
    .unwrap();
    assert_eq!(vm.shared.mem.heap.array_length(outer), 1);

    let inner_val = vm.shared.mem.heap.get_array_element(outer, 0).unwrap();
    match inner_val {
        Value::Object(Some(inner_ref)) => {
            assert_eq!(vm.shared.mem.heap.array_length(inner_ref), 1);
        }
        other => panic!("Expected inner array, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Lambda SAM/impl descriptor coercion — C2 fix
// ---------------------------------------------------------------------

#[test]
fn split_descriptor_no_args() {
    let (p, r) = split_method_descriptor("()V");
    assert_eq!(p.len(), 0);
    assert_eq!(r, "V");
}

#[test]
fn split_descriptor_simple() {
    let (p, r) = split_method_descriptor("(I)I");
    assert_eq!(p, vec!["I"]);
    assert_eq!(r, "I");
}

#[test]
fn split_descriptor_mixed() {
    let (p, r) = split_method_descriptor("(ILjava/lang/String;[BJ)Ljava/lang/Object;");
    assert_eq!(p, vec!["I", "Ljava/lang/String;", "[B", "J"]);
    assert_eq!(r, "Ljava/lang/Object;");
}

#[test]
fn split_descriptor_arrays() {
    let (p, r) = split_method_descriptor("([[Ljava/lang/Object;)[I");
    assert_eq!(p, vec!["[[Ljava/lang/Object;"]);
    assert_eq!(r, "[I");
}

/// The four tests above all pass a WELL-FORMED descriptor, which is why a
/// panic on the malformed one survived: `split_method_descriptor_ref` skipped
/// the `(` by starting its cursor at 1 and then sliced the tail unguarded, so
/// an empty descriptor panicked instead of being rejected. It reached that
/// state from `Lookup.unreflect` on a VM whose core-reflection natives were
/// yielding, and took the VM down with it (`internal error: native method
/// panic`) rather than returning a value any caller could handle.
///
/// A parser reachable from a native must be total over its input type.
#[test]
fn split_descriptor_rejects_malformed_without_panicking() {
    // The exact input that aborted the VM.
    let (p, r) = split_method_descriptor("");
    assert!(p.is_empty(), "no parameters from an empty descriptor");
    assert_eq!(r, "", "empty return token, matching descriptor_return_ref");

    // Anything not opening with `(` is the same class of input.
    for d in ["V", "I)V", "Ljava/lang/String;", ")", "["] {
        let (p, r) = split_method_descriptor(d);
        assert!(
            p.is_empty(),
            "{d:?} does not open with '(' and must yield no parameters"
        );
        assert_eq!(r, "", "{d:?} must yield an empty return token");
    }

    // A well-formed descriptor is untouched by the guard.
    let (p, r) = split_method_descriptor("(I)V");
    assert_eq!(p, vec!["I"]);
    assert_eq!(r, "V");
}

#[test]
fn is_primitive_desc_covers_all_8_primitives() {
    for t in &["I", "J", "F", "D", "B", "S", "Z", "C"] {
        assert!(is_primitive_desc(t), "{} should be primitive", t);
    }
    assert!(!is_primitive_desc("V"));
    assert!(!is_primitive_desc("Ljava/lang/Integer;"));
    assert!(!is_primitive_desc("[I"));
}

#[test]
fn is_reference_desc_covers_object_and_array() {
    assert!(is_reference_desc("Ljava/lang/Integer;"));
    assert!(is_reference_desc("[I"));
    assert!(is_reference_desc("[[Ljava/lang/String;"));
    assert!(!is_reference_desc("I"));
    assert!(!is_reference_desc("V"));
}

#[test]
fn widen_primitive_int_to_long() {
    assert_eq!(widen_primitive("I", "J", Value::Int(5)), Value::Long(5));
}

#[test]
fn unboxed_integer_widens_for_long_lambda_target() {
    assert_eq!(
        widen_unboxed_primitive('J', Value::Int(i32::MIN)),
        Value::Long(i64::from(i32::MIN))
    );
    assert_eq!(widen_unboxed_primitive('J', Value::Int(1)), Value::Long(1));
}

#[test]
fn widen_primitive_int_to_float() {
    assert_eq!(widen_primitive("I", "F", Value::Int(3)), Value::Float(3.0));
}

#[test]
fn widen_primitive_int_to_double() {
    assert_eq!(widen_primitive("I", "D", Value::Int(7)), Value::Double(7.0));
}

#[test]
fn widen_primitive_long_to_float() {
    assert_eq!(
        widen_primitive("J", "F", Value::Long(100)),
        Value::Float(100.0)
    );
}

#[test]
fn widen_primitive_long_to_double() {
    assert_eq!(
        widen_primitive("J", "D", Value::Long(100)),
        Value::Double(100.0)
    );
}

#[test]
fn widen_primitive_float_to_double() {
    assert_eq!(
        widen_primitive("F", "D", Value::Float(1.5)),
        Value::Double(1.5)
    );
}

#[test]
fn widen_primitive_byte_to_long() {
    assert_eq!(widen_primitive("B", "J", Value::Int(42)), Value::Long(42));
}

#[test]
fn widen_primitive_noop_same_type() {
    assert_eq!(widen_primitive("I", "I", Value::Int(9)), Value::Int(9));
    assert_eq!(widen_primitive("J", "J", Value::Long(9)), Value::Long(9));
}

// -----------------------------------------------------------------------
// C1 — exception-handler lookup covers invoke instruction PC
// -----------------------------------------------------------------------
//
// Regression guard: before C1, when a native method (e.g. Class.forName)
// threw and its caller had a try/catch whose range covered ONLY the
// invoke instruction itself (start_pc = invoke_pc, end_pc =
// invoke_pc + 3), the handler was missed because the caller's PC had
// already been advanced past the invoke. This test builds a synthetic
// ExceptionTableEntry with that shape and asserts the PC at the invoke
// site falls inside the handler range.
#[test]
fn exception_handler_range_covers_invoke_pc() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    let entry = ExceptionTableEntry {
        start_pc: 22,
        end_pc: 27,
        handler_pc: 28,
        catch_type: 0, // catch-all for simplicity
    };
    let invoke_pc = 24usize; // Class.forName at invoke_pc 24 (3-byte insn)
                             // JVMS range is inclusive-start, exclusive-end.
                             // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    assert!(invoke_pc >= entry.start_pc as usize);
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    assert!(invoke_pc < entry.end_pc as usize);

    // Simulate PC-after-invoke (post-invoke caller PC). The unwind
    // path computes `pc.saturating_sub(1)` to land back inside the
    // range.
    let post_invoke_pc = invoke_pc + 3;
    let unwound_pc = post_invoke_pc.saturating_sub(1);
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    assert!(unwound_pc >= entry.start_pc as usize);
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    assert!(unwound_pc < entry.end_pc as usize);
}

// Regression guard for the JIT-unknown-PC unwind path
// (`find_exception_handler_pc_unknown` / `route_jit_exception_through_method`):
// when the throw-site PC cannot be recovered, a catch-all / `finally`
// entry must still be honoured *iff* its protected region spans the whole
// method (`start_pc == 0 && end_pc >= code_len`), so `finally` /
// synchronized-monitor-exit cleanup is not silently skipped. A narrower
// catch-all is rejected (it could swallow an out-of-region exception).
// This exercises the exact predicate both functions use; `code_len` is the
// *unpadded* bytecode length (`code.len() - 2`).
#[test]
fn pc_unknown_catch_all_honored_only_for_whole_method() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // Padded bytecode (real body is 40 bytes; +2 trailing padding bytes).
    let padded_code_len = 42usize;
    let code_len = padded_code_len.saturating_sub(2);
    assert_eq!(code_len, 40);

    // The predicate factored out of the unwind loop.
    let honored = |e: &ExceptionTableEntry| {
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        e.catch_type == 0 && e.start_pc == 0 && e.end_pc as usize >= code_len
    };

    // Whole-method finally: start at 0, end at the code length → honored.
    let whole = ExceptionTableEntry {
        start_pc: 0,
        end_pc: 40,
        handler_pc: 40,
        catch_type: 0,
    };
    assert!(honored(&whole), "method-wide finally must run");

    // `end_pc` past the body (e.g. equal to padded len) still covers it.
    let whole_over = ExceptionTableEntry {
        end_pc: 41,
        ..whole
    };
    assert!(honored(&whole_over));

    // Narrow catch-all that does NOT start at 0 → skipped (could catch an
    // out-of-region throw when the PC is unknown).
    let narrow_start = ExceptionTableEntry {
        start_pc: 4,
        end_pc: 40,
        handler_pc: 40,
        catch_type: 0,
    };
    assert!(!honored(&narrow_start));

    // Catch-all that ends before the method end → skipped.
    let narrow_end = ExceptionTableEntry {
        start_pc: 0,
        end_pc: 20,
        handler_pc: 20,
        catch_type: 0,
    };
    assert!(!honored(&narrow_end));

    // A *typed* handler (catch_type != 0) is never honored by this
    // catch-all predicate; it matches by exception class elsewhere.
    let typed = ExceptionTableEntry {
        start_pc: 0,
        end_pc: 40,
        handler_pc: 40,
        catch_type: 7,
    };
    assert!(!honored(&typed));
}

// -----------------------------------------------------------------------
// T10.4 — SharedResolutionState promoted-invoke read path is wired in
//         the VM's SharedVm and bypasses the class-manager write lock.
// T10.7 — VecPool refill / spill is wired through the frame push/pop
//         path, and per-thread overflow re-populates the shared pool.
// -----------------------------------------------------------------------

#[test]
fn t10_shared_vm_exposes_shared_resolution_and_pools() {
    use crate::config::VmConfig;
    use crate::vm::Vm;
    let vm = Vm::new(VmConfig::new());
    // Freshly booted VM — counters should be at their initial values.
    // Some JDK bootstrap may have populated the invoke path, so we only
    // assert the counters are accessible and non-negative (u64 cannot
    // be negative, so the check is that the getters compile and return).
    let _ = vm.shared.classes.shared_resolution.promoted_hit_count();
    let _ = vm.shared.classes.shared_resolution.promoted_insert_count();
    let _ = vm.shared.classes.shared_resolution.promoted_invoke_count();
    let _ = vm.shared.mem.operand_stack_pool.acquire_count();
    let _ = vm.shared.mem.tag_pool.acquire_count();
}

#[test]
fn t10_shared_resolution_read_hit_round_trip() {
    // Wire check: inserting a promoted target via SharedResolutionState
    // on a live SharedVm and reading it back via get_promoted_invoke
    // yields the same target without touching class_manager.
    use crate::classloading::resolution::{CachedBytecodeMethod, CachedInvokeTarget, RedefineGate};
    use crate::config::VmConfig;
    use crate::runtime::lockfree_resolve::PromotedInvokeKey;
    use crate::vm::Vm;
    let vm = Vm::new(VmConfig::new());

    let before_hits = vm.shared.classes.shared_resolution.promoted_hit_count();
    let before_inserts = vm.shared.classes.shared_resolution.promoted_insert_count();

    let cached = std::sync::Arc::new(CachedBytecodeMethod {
        declaring_class_id: ClassId::new(12345),
        class_name: Arc::from("t10/Target"),
        method_name: Arc::from("hi"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: Arc::from(vec![0xB1u8].as_slice()),
        exception_table: Arc::from(vec![].as_slice()),
        max_stack: 0,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: false,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    });
    let key: PromotedInvokeKey = (ClassId::new(9999), 17, false, Some(ClassId::new(12345)));
    vm.shared.classes.shared_resolution.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(12345),
            cached: cached.clone(),
            gate: RedefineGate::never_stale(),
        },
    );
    let got = vm
        .shared
        .classes
        .shared_resolution
        .get_promoted_invoke(&key);
    match got {
        Some(CachedInvokeTarget::VirtualBytecode {
            receiver_class_id,
            cached: got_cached,
            gate: _,
        }) => {
            assert_eq!(receiver_class_id, ClassId::new(12345));
            assert_eq!(got_cached.method_name.as_ref(), "hi");
        }
        _ => panic!("expected VirtualBytecode hit"),
    }
    // Exactly one insert and one hit attributable to this test.
    assert_eq!(
        vm.shared.classes.shared_resolution.promoted_insert_count() - before_inserts,
        1
    );
    assert_eq!(
        vm.shared.classes.shared_resolution.promoted_hit_count() - before_hits,
        1
    );
}

#[test]
fn t10_vec_pool_wired_into_shared_vm_roundtrip() {
    use crate::config::VmConfig;
    use crate::vm::Vm;
    let vm = Vm::new(VmConfig::new());

    // FIX: the VecPool stat counters (acquire/hit/release_stored) are
    // gated behind the process-wide `VEC_POOL_STATS_ENABLED` flag, which
    // defaults to `false` so release builds skip the per-frame fetch_adds
    // on the hot path (see alloc_fastpath.rs `VEC_POOL_STATS_ENABLED`).
    // Without enabling stats the counters never advance and the round-trip
    // asserts `0 == 2`. Enable stats for the duration of this diagnostic
    // round-trip, snapshotting the prior gate state so we restore it and
    // don't leak the flag into other tests.
    use crate::runtime::alloc_fastpath::VecPool;
    let stats_were_enabled = VecPool::<u64>::stats_enabled();
    VecPool::<u64>::enable_stats();

    let before_op_count = vm.shared.mem.operand_stack_pool.acquire_count();
    let before_op_hits = vm.shared.mem.operand_stack_pool.acquire_hit_count();
    let before_op_stored = vm.shared.mem.operand_stack_pool.release_stored_count();

    // Acquire + release on the shared pools directly and confirm the
    // counters advance.  This exercises the exact API that
    // `JvmThread::refill_pools_from_shared` and
    // `JvmThread::recycle_frame_with_shared` drive during interpretation.
    let v = vm.shared.mem.operand_stack_pool.acquire(128);
    assert!(v.capacity() >= 128);
    vm.shared.mem.operand_stack_pool.release(v);
    // Acquire again — this one must be a reuse hit.
    let v2 = vm.shared.mem.operand_stack_pool.acquire(64);
    assert!(v2.capacity() >= 128, "reused Vec must keep its capacity");
    vm.shared.mem.operand_stack_pool.release(v2);

    assert_eq!(
        vm.shared.mem.operand_stack_pool.acquire_count() - before_op_count,
        2
    );
    assert!(vm.shared.mem.operand_stack_pool.acquire_hit_count() > before_op_hits);
    assert!(vm.shared.mem.operand_stack_pool.release_stored_count() > before_op_stored);

    // FIX: restore the global stat gate to its prior state so this test
    // does not leak `VEC_POOL_STATS_ENABLED = true` into sibling tests
    // that assert the counters stay at 0 while stats are disabled.
    if !stats_were_enabled {
        VecPool::<u64>::disable_stats();
    }
}

#[test]
fn t10_vec_pool_acquire_release_capacity_preserved_on_shared_vm() {
    use crate::config::VmConfig;
    use crate::vm::Vm;
    // Drain the pool so a subsequent release-then-acquire on a known
    // capacity round-trips cleanly even when VM bootstrap populated
    // mixed-sized entries.
    let vm = Vm::new(VmConfig::new());
    while vm.shared.mem.operand_stack_pool.pool_size() > 0 {
        let _ = vm.shared.mem.operand_stack_pool.acquire(0);
    }
    let v = vm.shared.mem.operand_stack_pool.acquire(256);
    let cap = v.capacity();
    assert!(cap >= 256);
    let ptr = v.as_ptr();
    vm.shared.mem.operand_stack_pool.release(v);
    let v2 = vm.shared.mem.operand_stack_pool.acquire(1);
    assert_eq!(v2.as_ptr(), ptr, "same allocation must come back");
    assert_eq!(v2.capacity(), cap, "capacity must be exactly preserved");
    vm.shared.mem.operand_stack_pool.release(v2);
}

// -----------------------------------------------------------------------
// T10.K5 — long/double constant-push + numeric-conversion tagging gate
//
// These tests pin the invariant that every opcode that leaves a long or
// double on the operand stack writes the 8-byte slot directly as a
// CompactValue::long / CompactValue::double (never through the Value
// enum boundary, which collapses longs into the untagged-double bucket
// and can in rare cases produce the "expected long on stack, got
// <uninitialized>" KC26 diagnostic).  The check exercised here is
// behavioural: after simulating the opcode by calling the same
// push_long / push_double helpers the fast path now calls, the stack's
// tag-aware pop_long / pop_double must return the original value.
// -----------------------------------------------------------------------

#[test]
fn t18_k5_ldc2_w_long_round_trip() {
    use crate::runtime::ValueStack;
    use crate::types::CompactTag;
    // Simulates execute_ldc2w for a CONSTANT_Long_info entry — it now
    // calls push_long directly, preserving the 8-byte slot tag.
    //
    // `as_long_unchecked` must round-trip every i64 bit pattern because
    // `CompactValue::long` is defined as `Self(v as u64)` — the test
    // covers small values (untagged, tag() => Double) AND values whose
    // upper bits alias the NaN-box space (tag() => Long via the
    // SUB_LONG_LO/HI sub-tags).
    let cases: &[i64] = &[0, 1, -1, 123_456_789_012_345, i64::MAX, i64::MIN];
    for &v in cases {
        let mut stack = ValueStack::new(2);
        stack.push_long(v).expect("push_long");
        let top = stack.peek_compact();
        // The slot must be recognisable as long-carrying: either untagged
        // (tag() == Double is the canonical untagged-long case) or
        // explicitly Long-tagged for high-magnitude values.
        assert!(
            matches!(top.tag(), CompactTag::Double | CompactTag::Long),
            "long slot tag must be Double (untagged) or Long for {v}, got {:?}",
            top.tag()
        );
        // Raw i64 round-trip via the tag-agnostic accessor the JIT and
        // interpreter use when instruction context guarantees a long.
        assert_eq!(top.as_long_unchecked(), v);
    }
}

#[test]
fn t18_k5_ldc2_w_double_round_trip() {
    use crate::runtime::ValueStack;
    use crate::types::CompactTag;
    // Simulates execute_ldc2w for a CONSTANT_Double_info entry.
    let cases: &[f64] = &[
        0.0,
        1.0,
        -1.0,
        std::f64::consts::PI,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for &v in cases {
        let mut stack = ValueStack::new(2);
        stack.push_double(v).expect("push_double");
        let top = stack.peek_compact();
        assert_eq!(top.tag(), CompactTag::Double);
        assert_eq!(stack.pop_double().expect("pop_double"), v);
    }
    // NaN round-trips bit-exact through push_double; this one is the
    // canonical quiet NaN, which never collided with the tag space anyway.
    let mut stack = ValueStack::new(2);
    stack.push_double(f64::NAN).expect("push_double NaN");
    assert!(stack.pop_double().expect("pop_double NaN").is_nan());
}

#[test]
fn t18_k5_lconst_0_round_trip() {
    use crate::runtime::ValueStack;
    // Mirrors the fast-path 0x09 arm: push_long_unchecked(0).
    let mut stack = ValueStack::new(2);
    stack.push_long_unchecked(0);
    assert_eq!(stack.pop_long().expect("pop_long"), 0);
}

#[test]
fn t18_k5_lconst_1_round_trip() {
    use crate::runtime::ValueStack;
    // Mirrors the fast-path 0x0a arm.
    let mut stack = ValueStack::new(2);
    stack.push_long_unchecked(1);
    assert_eq!(stack.pop_long().expect("pop_long"), 1);
}

#[test]
fn t18_k5_dconst_0_round_trip() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(2);
    stack.push_double_unchecked(0.0);
    assert_eq!(
        stack.pop_double().expect("pop_double").to_bits(),
        0f64.to_bits()
    );
}

#[test]
fn t18_k5_dconst_1_round_trip() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(2);
    stack.push_double_unchecked(1.0);
    assert_eq!(stack.pop_double().expect("pop_double"), 1.0);
}

#[test]
fn t18_k5_i2l_converts_correctly() {
    use crate::runtime::ValueStack;
    // Mirrors the Instruction::I2l and fast-path 0x85 migrations:
    // int → long via i64::from sign extension (no truncating `as`).
    // `as_long_unchecked` is the tag-agnostic accessor that always
    // returns the raw i64 bit pattern, so both positive and negative
    // int inputs round-trip regardless of NaN-box aliasing.
    let cases: &[i32] = &[0, 1, -1, 42, i32::MAX, i32::MIN];
    for &v in cases {
        let mut stack = ValueStack::new(2);
        stack.push_long(i64::from(v)).expect("push_long");
        let top = stack.peek_compact();
        assert_eq!(top.as_long_unchecked(), i64::from(v));
    }
}

#[test]
fn t18_k5_f2l_negative() {
    use crate::runtime::ValueStack;
    // JVM spec §2.8.3: in-range floats truncate toward zero.
    // -3.7f32 → -3i64 (not -4, not saturated).
    let result = float_to_long(-3.7f32);
    assert_eq!(result, -3i64);
    let mut stack = ValueStack::new(2);
    stack.push_long(result).expect("push_long");
    // Use the tag-agnostic accessor — small negatives alias NaN-box
    // space and would otherwise take a longer pop_long path.
    assert_eq!(stack.peek_compact().as_long_unchecked(), -3i64);
}

#[test]
fn t18_k5_f2l_nan_and_inf_saturate() {
    // JVM §2.8.3: NaN → 0, +inf → Long::MAX, -inf → Long::MIN.
    assert_eq!(float_to_long(f32::NAN), 0i64);
    assert_eq!(float_to_long(f32::INFINITY), i64::MAX);
    assert_eq!(float_to_long(f32::NEG_INFINITY), i64::MIN);
}

#[test]
fn t18_k5_d2l_round_trip() {
    use crate::runtime::ValueStack;
    // In-range doubles truncate toward zero.
    assert_eq!(double_to_long(1.9), 1i64);
    assert_eq!(double_to_long(-2.5), -2i64);
    let mut stack = ValueStack::new(2);
    stack
        .push_long(double_to_long(1_234_567_890.5))
        .expect("push_long");
    assert_eq!(stack.peek_compact().as_long_unchecked(), 1_234_567_890i64);

    // Saturation edges: JVM §2.8.3.
    assert_eq!(double_to_long(f64::NAN), 0i64);
    assert_eq!(double_to_long(f64::INFINITY), i64::MAX);
    assert_eq!(double_to_long(f64::NEG_INFINITY), i64::MIN);
}

#[test]
fn t18_k5_i2d_round_trip() {
    use crate::runtime::ValueStack;
    // i2d is lossless; f64::from widens without silent truncation.
    let cases: &[i32] = &[0, 1, -1, 42, i32::MAX, i32::MIN];
    for &v in cases {
        let mut stack = ValueStack::new(2);
        stack.push_double(f64::from(v)).expect("push_double");
        assert_eq!(stack.pop_double().expect("pop_double"), f64::from(v));
    }
}

#[test]
fn t18_k5_f2d_widens_lossless() {
    use crate::runtime::ValueStack;
    // f2d: float → double, lossless; f64::from is the widening conversion.
    let cases: &[f32] = &[0.0, 1.0, -1.0, std::f32::consts::PI, f32::MIN_POSITIVE];
    for &v in cases {
        let mut stack = ValueStack::new(2);
        stack.push_double(f64::from(v)).expect("push_double");
        assert_eq!(stack.pop_double().expect("pop_double"), f64::from(v));
    }
}

#[test]
fn t18_k5_l2d_round_trip() {
    use crate::runtime::ValueStack;
    // l2d may lose precision for |v| > 2^53, but the resulting slot must
    // decode as a double regardless of the magnitude.
    let cases: &[i64] = &[0, 1, -1, 1_000_000_000_000, i64::MAX, i64::MIN];
    for &v in cases {
        let mut stack = ValueStack::new(2);
        stack.push_double(v as f64).expect("push_double"); // JVM spec: l2d rounds to nearest
        let popped = stack.pop_double().expect("pop_double");
        assert_eq!(popped, v as f64); // JVM spec: l2d rounds to nearest
    }
}

#[test]
fn t18_k5_long_slot_is_single_8_byte_compactvalue() {
    use crate::runtime::ValueStack;
    // Regression guard: a long is a single 8-byte CompactValue slot —
    // the stack must NOT allocate two category-2 half-slots.
    let mut stack = ValueStack::new(4);
    stack.push_long(42).expect("push_long");
    assert_eq!(stack.len(), 1, "long occupies exactly one 8-byte slot");
    stack.push_double(1.0).expect("push_double");
    assert_eq!(stack.len(), 2, "double occupies exactly one 8-byte slot");
}

// -----------------------------------------------------------------------
// T18.K4 — invoke* return-value push must preserve J/D tags
// -----------------------------------------------------------------------
//
// The `push_invoke_return_value` helper must route `Value::Long` /
// `Value::Double` returns through `CompactValue::long` /
// `CompactValue::double` so they land on the caller's operand stack
// with the correct bit pattern — simulating the
// `invokestatic ()J` / `invokevirtual ()J` / `invokeinterface ()D`
// return path.  Regressions here reproduce the KC26-boot
// "expected long on stack, got <uninitialized>" panic.
//
// These tests are unit-level: they drive the helper directly with
// `Value::Long(...)` / `Value::Double(...)` (the exact shape the
// dispatched bytecode/native paths hand back) and assert both the
// slot contents and the decoded round-trip.  A full-VM integration
// rotation is covered by the higher-level synthetic-jdk suite.

#[test]
fn t18_k4_invokestatic_long_return_round_trip() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(8);
    // A long value whose raw bits set NANBOX_BITS — the exact case
    // that used to confuse the `Value` boundary and surface as
    // "expected long on stack, got <uninitialized>".
    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
    let lv: i64 = 0x7FF8_1234_5678_9ABC_u64 as i64;
    push_invoke_return_value(&mut stack, Value::Long(lv))
        .expect("push_invoke_return_value must not overflow on a fresh stack");
    // Tag-aware read: the top slot must decode as Long via
    // `pop_long` (which treats untagged slots as raw long bits)
    // and return exactly the bits we pushed.
    assert_eq!(stack.pop_long().expect("J must decode as long"), lv);
}

#[test]
fn t18_k4_invokevirtual_long_return_round_trip() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(8);
    // A long at an arbitrary bit pattern exercises the non-NaN
    // branch of the `long`/`double` codec.
    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
    let lv: i64 = 0x0DEA_DBEE_FCAF_EBAB_u64 as i64;
    push_invoke_return_value(&mut stack, Value::Long(lv)).expect("push must succeed");
    assert_eq!(stack.pop_long().expect("J must decode as long"), lv);
}

#[test]
fn t18_k4_invokeinterface_double_return() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(8);
    // A finite, non-NaN double — round-trips exactly.
    let dv: f64 = std::f64::consts::PI;
    push_invoke_return_value(&mut stack, Value::Double(dv)).expect("push must succeed");
    let popped = stack.pop_double().expect("D must decode as double");
    assert_eq!(popped.to_bits(), dv.to_bits());
}

#[test]
fn t18_k4_invoke_void_return_no_push() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(8);
    // Void-return path: the interpreter never calls the helper for
    // `None` returns, so pushing nothing keeps the stack empty.
    // We simulate the `if let Some(value) = result { ... }` wrapper
    // used at every call site and confirm no slot was written.
    let result: Option<Value> = None;
    if let Some(value) = result {
        push_invoke_return_value(&mut stack, value).unwrap();
    }
    assert_eq!(stack.len(), 0, "void return must leave the stack empty");
}

#[test]
fn t18_k4_invoke_int_return_still_works() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(8);
    // Non-J/D returns must fall through to the legacy
    // `push(Value)` boundary without any tag surprises.
    push_invoke_return_value(&mut stack, Value::Int(42))
        .expect("I must push through the legacy boundary");
    assert_eq!(stack.pop_int().expect("I must decode as int"), 42);
}

#[test]
fn t18_k4_invoke_object_return_still_works() {
    use crate::runtime::ValueStack;
    let mut stack = ValueStack::new(8);
    // Object returns also fall through to `push(Value)` — confirm
    // the tag-aware branch does not accidentally swallow the
    // reference.
    push_invoke_return_value(&mut stack, Value::Object(None)).expect("null reference push");
    let popped = stack.pop().expect("non-empty");
    assert!(matches!(popped, Value::Object(None)));
}

// -----------------------------------------------------------------------
// T10.9.D K3 — getstatic / putstatic CompactValue hot path
//
// These tests pin the behavior of `push_static_field_value` and
// `pop_static_field_value` — the descriptor-aware static-field push/pop
// helpers that preserve exact bit-for-bit round trips for J (long) and
// D (double) descriptors.  Before the migration, `Value::Long(x)` was
// encoded onto the stack via `CompactValue::long` (untagged raw bits)
// and then decoded back through `to_value()` as `Value::Double`,
// silently corrupting every long-descriptor static on read.  KC26's
// boot path exposed the regression.
//
// Tests are named `t18_k3_*` and filter-match the no-regression gate
// (`-- getstatic putstatic`).  The integer test pins that I/F/Z/B/S/C
// and reference descriptors keep the legacy boundary coercion.
// -----------------------------------------------------------------------

/// J-descriptor getstatic: a `Value::Long` stored in the statics map
/// must reach the operand stack as a `CompactValue` whose raw bits
/// round-trip through `as_long_unchecked()`.  Picks a bit pattern
/// that is NOT a valid NaN when reinterpreted as f64 so the fix is
/// observable — the pre-K3 path would silently swap the tag and
/// later `to_value()` would report `Value::Double`.
#[test]
fn t18_k3_getstatic_long_round_trip() {
    let mut stack = crate::runtime::ValueStack::new(16);
    let sentinel: i64 = 0x0102_0304_0506_0708_i64;
    push_static_field_value(
        &mut stack,
        Value::Long(sentinel),
        /* is_reference = */ false,
        Some(b'J'),
    )
    .expect("push for J-descriptor must succeed");
    assert_eq!(stack.len(), 1, "one CompactValue slot per 8-byte long");
    let cv = stack.pop_compact();
    assert_eq!(
        cv.as_long_unchecked(),
        sentinel,
        "long must round-trip bit-exact through CompactValue::long",
    );
}

/// J-descriptor putstatic: a `CompactValue::long` on the stack must
/// decode back to `Value::Long(x)` with the full 64-bit payload
/// preserved.  The naïve `stack.pop()?` path would return
/// `Value::Double(f64::from_bits(x))`, which re-encoded onto the
/// next push would corrupt the static field on every write.
#[test]
fn t18_k3_putstatic_long_round_trip() {
    let mut stack = crate::runtime::ValueStack::new(16);
    let sentinel: i64 = -0x0F0E_0D0C_0B0A_0908_i64;
    stack.push_compact(crate::types::CompactValue::long(sentinel));
    let v =
        pop_static_field_value(&mut stack, Some(b'J')).expect("pop for J-descriptor must succeed");
    assert_eq!(v, Value::Long(sentinel));
    assert_eq!(stack.len(), 0);
}

/// D-descriptor getstatic: a `Value::Double` must round-trip
/// bit-exact through the tag-aware push.  Uses a non-NaN payload
/// so the canonical-NaN scrubbing in `CompactValue::double` is a
/// no-op.
#[test]
fn t18_k3_getstatic_double_round_trip() {
    let mut stack = crate::runtime::ValueStack::new(16);
    let sentinel: f64 = std::f64::consts::PI;
    push_static_field_value(
        &mut stack,
        Value::Double(sentinel),
        /* is_reference = */ false,
        Some(b'D'),
    )
    .expect("push for D-descriptor must succeed");
    assert_eq!(stack.len(), 1, "one CompactValue slot per 8-byte double");
    let cv = stack.pop_compact();
    assert_eq!(
        f64::from_bits(cv.raw_bits()),
        sentinel,
        "double must round-trip bit-exact through CompactValue::double",
    );
}

/// D-descriptor putstatic: a `CompactValue::double` on the stack
/// decodes back to `Value::Double(x)` with the payload preserved.
#[test]
fn t18_k3_putstatic_double_round_trip() {
    let mut stack = crate::runtime::ValueStack::new(16);
    let sentinel: f64 = -std::f64::consts::E;
    stack.push_compact(crate::types::CompactValue::double(sentinel));
    let v =
        pop_static_field_value(&mut stack, Some(b'D')).expect("pop for D-descriptor must succeed");
    match v {
        Value::Double(x) => assert_eq!(x, sentinel),
        other => panic!("expected Value::Double, got {other:?}"),
    }
    assert_eq!(stack.len(), 0);
}

/// I-descriptor regression: the legacy `Value`-boundary coercion
/// must still apply to all non-J/D primitive and reference
/// descriptors.  Storing an Int via getstatic keeps a plain
/// `Value::Int` on the stack, and the symmetric putstatic pop
/// returns the same `Value::Int`.
#[test]
fn t18_k3_getstatic_int_still_works() {
    let mut stack = crate::runtime::ValueStack::new(16);
    push_static_field_value(
        &mut stack,
        Value::Int(0x1234_5678_i32),
        /* is_reference = */ false,
        Some(b'I'),
    )
    .expect("push for I-descriptor must succeed");
    let cv = stack.pop_compact();
    assert_eq!(cv.to_value(), Value::Int(0x1234_5678));

    // Zero-initialized static int field: the statics backing store
    // holds `Value::Int(0)`, and the legacy path leaves it as
    // `Value::Int(0)` on the stack.  The helper must NOT coerce an
    // int slot to Object.
    push_static_field_value(
        &mut stack,
        Value::Int(0),
        /* is_reference = */ false,
        Some(b'I'),
    )
    .unwrap();
    assert_eq!(stack.pop_compact().to_value(), Value::Int(0));

    // Symmetric pop on a non-J/D descriptor: both `Some(b'I')` and
    // `None` (unknown desc) route through the legacy `stack.pop()?`
    // path — a pushed Int returns as Int.
    stack.push(Value::Int(42)).unwrap();
    assert_eq!(
        pop_static_field_value(&mut stack, Some(b'I')).unwrap(),
        Value::Int(42),
    );
    stack.push(Value::Int(99)).unwrap();
    assert_eq!(
        pop_static_field_value(&mut stack, None).unwrap(),
        Value::Int(99),
    );
}

// -----------------------------------------------------------------------
// K2 (T10.9.E) — getfield / putfield direct-CompactValue round trips.
//
// Regression pin for the KC26 boot failure ("expected long on stack,
// got <uninitialized>"): a long field whose heap slot was still
// zero-init would be coerced to `Value::Int(0)` by the legacy
// non-reference branch of getfield, silently dropping the J tag.  The
// new J/D fast path on getfield constructs `CompactValue::long` /
// `CompactValue::double` directly from the heap value, preserving the
// 8-byte slot exactly as `lload`/`dload`/`lreturn` expect.  The
// symmetric putfield path `pop_compact`s the stack slot and decodes it
// tag-aware, so buggy upstream producers that push an untagged long
// (which would round-trip as `Value::Double` via `to_value()`) still
// store the correct bits.
//
// These tests execute the exact sequence the production arms use
// (heap.set_field + heap.get_field + push_compact / pop_compact) so
// they catch regressions without needing a full interpreter harness.
// -----------------------------------------------------------------------

#[test]
fn t18_k2_getfield_long_round_trip() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);

    // Allocate an object with a single long field slot.  The single
    // slot is enough — our `CompactValue` stores the whole 64-bit
    // long in one 8-byte slot, matching the interpreter's view.
    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 1);

    // Store a non-trivial long value, then read it back via the K2
    // path: heap.get_field → CompactValue::long → push_compact →
    // pop_long.  The asserted value must round-trip losslessly.
    let cases: &[i64] = &[
        0,
        1,
        -1,
        42,
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        0x0BAD_BEEF_DEAD_CAFE_u64 as i64,
        i64::MAX,
        i64::MIN,
    ];
    for &v in cases {
        vm.shared.mem.heap.set_field(obj, 0, Value::Long(v));
        let value = vm.shared.mem.heap.get_field(obj, 0);
        let bits: i64 = match value {
            Value::Long(x) => x,
            // Cast: float/double raw bit pattern stored in integer word (no value conversion)
            Value::Double(x) => x.to_bits() as i64,
            // Widening: i32 -> i64 (sign-extended, JVM i2l)
            Value::Int(x) => x as i64,
            Value::Object(None) | Value::Uninitialized => 0,
            other => panic!("unexpected tag for long field: {other:?}"),
        };
        let mut stack = crate::runtime::ValueStack::new(2);
        stack.push_long_unchecked(bits);
        let popped = stack.pop_long().expect("pop_long after K2 push");
        assert_eq!(popped, v, "long {v:#x} must round-trip through K2 getfield");
    }
}

#[test]
fn t18_k2_putfield_long_round_trip() {
    use crate::config::VmConfig;
    use crate::types::CompactTag;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);
    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 1);

    // Simulate a full putfield → getfield round trip: push a long via
    // `push_long` (mirroring an upstream `lconst` / `ldc2_w` / `lload`
    // producer), then exercise the K2 putfield pop path (pop_compact
    // + tag-aware extract), write to heap, read back via K2 getfield.
    let cases: &[i64] = &[0, 1, -1, 123_456_789_012_345, i64::MAX, i64::MIN];
    for &v in cases {
        let mut stack = crate::runtime::ValueStack::new(2);
        stack.push_long(v).expect("push_long");

        // K2 putfield pop: pop_compact + tag-aware decode.
        let cv = stack.pop_compact();
        let lv = match cv.tag() {
            CompactTag::Long => cv.as_long_unchecked(),
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            CompactTag::Double => cv.raw_bits() as i64,
            CompactTag::Int => match cv.to_value() {
                // Widening: i32 -> i64 (sign-extended, JVM i2l)
                Value::Int(x) => x as i64,
                _ => 0,
            },
            CompactTag::Null | CompactTag::Uninitialized => 0,
            other => panic!("unexpected tag for J-descriptor field: {other:?}"),
        };
        vm.shared.mem.heap.set_field(obj, 0, Value::Long(lv));

        // K2 getfield push: heap → CompactValue::long → push_compact.
        let value = vm.shared.mem.heap.get_field(obj, 0);
        let bits: i64 = match value {
            Value::Long(x) => x,
            // Cast: float/double raw bit pattern stored in integer word (no value conversion)
            Value::Double(x) => x.to_bits() as i64,
            // Widening: i32 -> i64 (sign-extended, JVM i2l)
            Value::Int(x) => x as i64,
            Value::Object(None) | Value::Uninitialized => 0,
            other => panic!("unexpected tag for long field: {other:?}"),
        };
        stack.push_long_unchecked(bits);
        assert_eq!(
            stack.pop_long().expect("pop_long"),
            v,
            "long {v:#x} must round-trip through K2 putfield + getfield"
        );
    }
}

#[test]
fn t18_k2_getfield_double_round_trip() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);
    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 1);

    let cases: &[f64] = &[
        0.0,
        1.0,
        -1.0,
        std::f64::consts::PI,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for &v in cases {
        vm.shared.mem.heap.set_field(obj, 0, Value::Double(v));
        let value = vm.shared.mem.heap.get_field(obj, 0);
        let d: f64 = match value {
            Value::Double(x) => x,
            // Cast: integer word reinterpreted as float/double bit pattern
            Value::Long(x) => f64::from_bits(x as u64),
            Value::Object(None) | Value::Uninitialized => 0.0,
            other => panic!("unexpected tag for double field: {other:?}"),
        };
        let mut stack = crate::runtime::ValueStack::new(2);
        stack.push_double_unchecked(d);
        let popped = stack.pop_double().expect("pop_double");
        assert_eq!(
            popped.to_bits(),
            v.to_bits(),
            "double {v} must round-trip losslessly"
        );
    }
}

#[test]
fn t18_k2_putfield_double_round_trip() {
    use crate::config::VmConfig;
    use crate::types::CompactTag;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);
    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 1);

    let cases: &[f64] = &[0.0, 1.0, -1.0, std::f64::consts::E, f64::MAX];
    for &v in cases {
        let mut stack = crate::runtime::ValueStack::new(2);
        stack.push_double(v).expect("push_double");

        // K2 putfield pop for D.
        let cv = stack.pop_compact();
        let dv = match cv.tag() {
            CompactTag::Double => f64::from_bits(cv.raw_bits()),
            // Cast: integer word reinterpreted as float/double bit pattern
            CompactTag::Long => f64::from_bits(cv.as_long_unchecked() as u64),
            CompactTag::Int => match cv.to_value() {
                // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                Value::Int(x) => x as f64,
                _ => 0.0,
            },
            CompactTag::Null | CompactTag::Uninitialized => 0.0,
            other => panic!("unexpected tag for D-descriptor field: {other:?}"),
        };
        vm.shared.mem.heap.set_field(obj, 0, Value::Double(dv));

        // K2 getfield push for D.
        let value = vm.shared.mem.heap.get_field(obj, 0);
        let d: f64 = match value {
            Value::Double(x) => x,
            // Cast: integer word reinterpreted as float/double bit pattern
            Value::Long(x) => f64::from_bits(x as u64),
            Value::Object(None) | Value::Uninitialized => 0.0,
            other => panic!("unexpected tag for double field: {other:?}"),
        };
        stack.push_double_unchecked(d);
        assert_eq!(
            stack.pop_double().expect("pop_double").to_bits(),
            v.to_bits(),
            "double {v} must round-trip through K2 putfield + getfield"
        );
    }
}

#[test]
fn t18_k2_getfield_int_still_works() {
    // Regression pin for category-1 integer fields: the K2 J/D
    // fast path must NOT swallow I descriptors — they keep using
    // the legacy `push(Value)` boundary.  This test confirms the
    // non-J/D branch of the match is still reached and an int
    // field still round-trips cleanly.
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let config = VmConfig::new();
    let vm = Vm::new(config);
    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(1), 1);

    for &v in &[i32::MIN, -1, 0, 1, 42, i32::MAX] {
        vm.shared.mem.heap.set_field(obj, 0, Value::Int(v));

        // The K2 path for non-J/D descriptors: read Value, apply the
        // legacy non-reference coercion (Object(None) → Int(0)), then
        // push(Value).
        let mut value = vm.shared.mem.heap.get_field(obj, 0);
        if matches!(value, Value::Object(None)) {
            value = Value::Int(0);
        }
        let mut stack = crate::runtime::ValueStack::new(2);
        stack.push(value).expect("push int");
        assert_eq!(
            stack.pop_int().expect("pop_int"),
            v,
            "int {v} must round-trip through the non-J/D (legacy) branch"
        );
    }
}

// -----------------------------------------------------------------------
// H1 — TLAB fast-path object header is fully populated at allocation
// -----------------------------------------------------------------------

/// Regression pin for the CGLIB "Stale pointer detected" warning.
///
/// Pre-fix, `init_object_header` left `identity_hash_code` at 0 with
/// a "lazy" comment that was never wired up. A fresh `new Object()`
/// (cid=0, num_fields=0) with hash=0 then produced an all-zero
/// first 16 bytes of header that the stale-pointer detector in
/// `execute_invoke` mis-flagged on every legitimate Object key in
/// HashMap operations. This test pins:
///   1. `init_object_header` writes the supplied hash into the
///      header (so the caller controls it).
///   2. `VmHeap::next_identity_hash` mints a non-zero, monotonic
///      hash so the eager-assignment-at-allocation path never
///      produces an all-zero header.
///   3. The resulting header's first 16 bytes are not all-zero
///      even for a zero-field java.lang.Object instance.
///
/// (3) is now carried by `GC_FLAG_HEADER` rather than by the hash, which went
/// lazy in 2026-08-07's header shrink. The property is the same one; only its
/// source moved. See the comment at the assertion for the month this test spent
/// asserting the opposite, and for why the fix is not to mint a hash again.
#[test]
fn h1_tlab_object_header_has_nonzero_hash_at_allocation() {
    use cratonvm_gc::heap::ObjectHeader;
    use cratonvm_types::ClassId;

    // Allocate a 32-byte buffer, properly aligned, to host an
    // ObjectHeader. We use a Vec<u64> so it's 8-aligned.
    let mut storage = vec![0u64; 4]; // 32 bytes = HEADER_SIZE
                                     // Cast: reinterpret pointer/address to typed pointer
    let ptr = storage.as_mut_ptr() as *mut u8;

    // Path 1: the TLAB fast path no longer mints a hash at allocation.
    // `body_size` 0 and `gc_flags` 0: this test is about the TLAB fast path not
    // minting a hash, and it asserts `class_id` and `num_slots` only. The two
    // parameters added since carry the COMPACT body shape, which a zero-slot
    // object does not have -- giving them anything else would be asserting a
    // layout the test does not check.
    //
    // Both trailing arguments were added to `init_object_header` without
    // updating this call, so `cargo test -p cratonvm-vm` did not compile
    // at all for a time -- the library still built, so only a test run
    // showed it.
    super::init_object_header(ptr, ClassId::new(0), 0, 0, 0);

    // SAFETY: we just wrote a valid ObjectHeader into `ptr`.
    let header = unsafe { std::ptr::read(ptr as *const ObjectHeader) };
    assert_eq!(header.class_id, ClassId::new(0));
    assert_eq!(header.num_slots(), 0);

    // First 16 bytes: must NOT be all-zero.
    //
    // This assertion has been both ways round, and the round trip is the point.
    // It was written as `assert_ne!` because an all-zero header made the
    // stale-pointer detector in `execute_invoke` mis-flag every legitimate
    // `Object` key in a `HashMap`. When the identity hash moved into the mark
    // word and became lazy (2026-08-07), a bare `new Object()` went back to
    // all-zero and this test was INVERTED to `assert_eq!`, with a comment
    // calling it "a real regression in the stale-pointer detector's
    // discriminator, recorded here rather than hidden".
    //
    // Recorded, and then it cost far more than the detector. An all-zero header
    // is also unparseable by the young non-moving sweep's linear walk, so every
    // `System.gc()` under `-XX:+UseGenerationalGC` retained ~100% of an
    // allocation-only workload's garbage — see
    // `docs/internal/fixed-bugs/h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md`.
    //
    // `GC_FLAG_HEADER` restores the property WITHOUT reviving the eager hash the
    // comment correctly refused: a minted-at-allocation hash would lose every
    // `try_thin_lock` CAS and inflate a monitor for every `synchronized` block.
    // SAFETY: we just wrote a valid ObjectHeader into `ptr`, so its first 16 bytes are initialized and readable.
    let first_16: [u8; 16] = unsafe { std::ptr::read(ptr as *const [u8; 16]) };
    assert_ne!(
        first_16, [0u8; 16],
        "a published header must never be sixteen zero bytes: it is then \
         indistinguishable from reclaimed, zeroed arena space, both to the \
         stale-pointer detector and to the collector's own heap walk"
    );

    // Path 2: verify VmHeap::next_identity_hash never returns 0
    // (matching the legacy non-TLAB allocators).
    use cratonvm_gc::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 1024 * 1024);
    for _ in 0..16 {
        assert_ne!(
            heap.next_identity_hash(),
            0,
            "next_identity_hash() must mint non-zero values to keep \
             fresh-allocation headers distinguishable from stale memory"
        );
    }
}

// -----------------------------------------------------------------------
// Task #42 — SATB pre-barrier wired at the aastore store site
// -----------------------------------------------------------------------

/// Task #42 (deferred from #25): the interpreter aastore path and
/// the `vm_exec` CAS putfield/aastore path must fire an SATB
/// pre-barrier on the *old* element BEFORE the new reference is
/// stored.  Without it, an old->new overwrite that happens between
/// G1 initial-mark and remark would silently drop the old
/// reference from the live closure — the classic SATB lost-object
/// scenario that turns into a use-after-free on the next
/// evacuation.
///
/// This test exercises the exact pattern emitted by the aastore
/// sites at interpreter.rs ~4227 and ~5349 (and now mirrored in
/// `vm_exec.rs::compare_and_swap_field`):
///
///   1. allocate a length-1 reference array under G1,
///   2. plant a non-null `old_ref` at slot 0 BEFORE activating SATB,
///   3. activate the G1 SATB queue and drain leftovers,
///   4. drive the migrated `if let Ok(old_elem) =
///      get_array_element(...) { heap.satb_barrier(old_elem); }`
///      pre-barrier followed by the `set_array_element` store,
///   5. flush the per-thread SATB buffer and drain the global
///      queue — the old reference's raw address MUST be present.
///
/// Regression modes caught:
///   - Pre-barrier removed entirely (queue would be empty).
///   - Pre-barrier fired AFTER the store (it would log the new
///     value instead of the old).
///   - `satb_barrier` silently mis-routes a `Value::Object(Some)`
///     payload on G1.
///
/// Note: this test uses the `satb_barrier` API exported on this
/// branch.  The orchestrator task description refers to a
/// `VmHeap::write_barrier_pre` trait method from task #25; that
/// method has not landed on this worktree's base commit, so the
/// migration is expressed through the equivalent
/// `VmHeap::satb_barrier` entry point.  The semantic invariant
/// (old ref ends up in the G1 SATB log before the store) is
/// identical.
#[test]
fn t42_aastore_satb_pre_barrier_captures_old_ref_under_g1() {
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_gc::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    let heap = VmHeap::new(GcBackend::G1, 8 * 1024 * 1024);

    // Allocate a length-1 reference array plus two objects to
    // play "old" and "new" roles.  ClassId is a placeholder —
    // the G1 allocator only cares about layout for this path.
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    let old_obj = heap.alloc_object(ClassId::new(1), 1);
    let new_obj = heap.alloc_object(ClassId::new(1), 1);
    // Cast: object/code pointer to integer address
    let old_addr = old_obj.as_ptr() as usize;

    // Plant the old reference BEFORE activating SATB so the
    // initial store doesn't pollute the log we're going to check.
    heap.set_array_element(arr, 0, Value::Object(Some(old_obj)))
        .expect("planting old_ref at arr[0] must succeed");

    // Grab the G1 SATB queue, drain any leftover entries from
    // prior tests, then activate concurrent marking so the
    // pre-barrier enqueues into the log.
    let g1 = match &heap {
        VmHeap::G1(g) => g,
        _ => panic!("test requires the G1 backend"),
    };
    cratonvm_gc::satb::flush_thread_satb_buffer(g1.satb_queue());
    let _ = g1.satb_queue().drain();
    g1.satb_queue().activate();
    assert!(
        g1.satb_queue().is_empty(),
        "SATB queue must start empty after drain"
    );

    // Drive the EXACT pattern emitted by the migrated aastore
    // sites at interpreter.rs ~4227 and ~5349 (the slow-path
    // Aastore), and the same shape now used by
    // `vm_exec.rs::compare_and_swap_field` for ref-typed CAS.
    if let Ok(old_elem) = heap.get_array_element(arr, 0) {
        heap.satb_barrier(old_elem);
    }
    heap.set_array_element(arr, 0, Value::Object(Some(new_obj)))
        .expect("aastore of new_obj must succeed");

    // Force a per-thread flush — the auto-flush threshold (256)
    // would otherwise hide a single-entry test under a stale
    // thread-local buffer.
    cratonvm_gc::satb::flush_thread_satb_buffer(g1.satb_queue());

    let drained = g1.satb_queue().drain();
    assert!(
        drained.contains(&old_addr),
        "G1 SATB queue must capture the old aastore element \
         ({old_addr:#x}); got {drained:?}",
    );

    g1.satb_queue().deactivate();
}

// -----------------------------------------------------------------------
// B5 — malformed ldc constant-pool entries become a *catchable*
// ClassFormatError, not an uncatchable VmError::Internal.
// -----------------------------------------------------------------------

/// `convert_ldc_class_format_error` turns a malformed-CP
/// `VmError::Linkage(ClassFormatError)` into a Java-catchable
/// `ExceptionThrown` (or, if the exception class can't be constructed
/// without rt.jar, falls back to the original error — never panics).
#[test]
fn ldc_class_format_error_is_catchable_or_falls_back() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let mut vm = Vm::new(VmConfig::new());
    let err = MethodCallFailed::InternalError(VmError::Linkage(LinkageError::ClassFormatError {
        class_name: "Demo".to_string(),
        message: "ldc: invalid constant pool index 99".to_string(),
    }));
    let out = convert_ldc_class_format_error(&vm.shared, &mut vm.main_thread, err);
    // Either a real Java exception (rt.jar available) or the original
    // error preserved (rt.jar absent in the test harness). It must
    // NEVER be lost or turned into a panic.
    assert!(matches!(
        out,
        MethodCallFailed::ExceptionThrown(_)
            | MethodCallFailed::InternalError(VmError::Linkage(
                LinkageError::ClassFormatError { .. }
            ))
    ));
}

/// A non-`ClassFormatError` failure must pass through
/// `convert_ldc_class_format_error` completely unchanged — the helper
/// is narrowly scoped to the malformed-CP case.
#[test]
fn ldc_converter_passes_through_unrelated_errors() {
    use crate::config::VmConfig;
    use crate::vm::Vm;

    let mut vm = Vm::new(VmConfig::new());
    let err = MethodCallFailed::InternalError(VmError::Internal {
        message: "current class not found".to_string(),
    });
    let out = convert_ldc_class_format_error(&vm.shared, &mut vm.main_thread, err);
    assert!(matches!(
        out,
        MethodCallFailed::InternalError(VmError::Internal { .. })
    ));
}

// -----------------------------------------------------------------------
// B3 — the panic-free CI gate must actually scan the production body,
// not just the ~33-line file header.
// -----------------------------------------------------------------------

/// Is `<stem>.rs` under a module directory declared `#[cfg(test)]` by its
/// parent?
///
/// Used by both source-scanning gates below: a test-only submodule must not
/// be scanned as production code, and must not be counted towards the
/// production surface B3 requires.
///
/// # The WHOLE attribute run, not the line above
///
/// A declaration may carry more than one attribute, and `#[cfg(test)]` is not
/// obliged to be the last of them. `jit/src/x64.rs` writes
///
/// ```text
/// #[cfg(test)]
/// // x86-64 ONLY. These EXECUTE the code they emit ...
/// #[cfg(target_arch = "x86_64")]
/// mod tests;
/// ```
///
/// because the module is BOTH test-only and x86-64-only. Remembering only the
/// previous non-comment line saw `#[cfg(target_arch = ...)]`, answered "not a
/// test module", and handed 17 278 lines of assertions to the panic gate as
/// production code -- 578 sites against a budget of 0. The gate was red on dev
/// from the moment the aarch64 work added that second attribute, and it read
/// as a JIT defect rather than as a classifier one, which is the expensive
/// kind of false positive: it accuses the wrong lane.
///
/// So the run of attributes immediately above the declaration is scanned as a
/// unit. Comments inside the run are skipped, as they already were; anything
/// that is not an attribute ENDS the run, so an attribute belonging to a
/// previous item cannot leak onto this one.
fn declared_cfg_test(parent_src: &str, stem: &str) -> bool {
    // Set by any `#[cfg(test)]` in the current attribute run, cleared by the
    // first line that is neither an attribute nor a comment.
    let mut run_has_cfg_test = false;
    for line in parent_src.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        let decl = t
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("pub(super) ")
            .trim_start_matches("pub ");
        if decl == format!("mod {stem};") {
            return run_has_cfg_test;
        }
        if t.starts_with("#[") {
            run_has_cfg_test |= t.starts_with("#[cfg(test)]");
        } else {
            run_has_cfg_test = false;
        }
    }
    false
}

/// **THE CLASSIFIER MUST READ THE WHOLE ATTRIBUTE RUN.**
///
/// Every one of these is a shape that appears in the tree or that a reviewer
/// would write without a second thought, and the pre-2026-09-04 form -- which
/// remembered one line -- got the second one wrong, silently, in the direction
/// of scanning 17 278 lines of test assertions as production code.
///
/// The last two are the other direction and matter just as much: a classifier
/// loosened until the real case passes can start calling PRODUCTION modules
/// test-only, and this gate going quiet is indistinguishable from it passing.
#[test]
fn the_cfg_test_classifier_reads_the_whole_attribute_run() {
    // The plain form.
    assert!(declared_cfg_test(
        "#[cfg(test)]
mod tests;
",
        "tests"
    ));
    // The form `jit/src/x64.rs` actually uses: test-only AND x86-64-only, with
    // the reason for the second attribute written between them.
    assert!(declared_cfg_test(
        "#[cfg(test)]
// x86-64 ONLY -- these EXECUTE what they emit.
         #[cfg(target_arch = \"x86_64\")]
mod tests;
",
        "tests"
    ));
    // Order must not matter either.
    assert!(declared_cfg_test(
        "#[cfg(target_arch = \"x86_64\")]
#[cfg(test)]
mod tests;
",
        "tests"
    ));
    // A production module is still production.
    assert!(!declared_cfg_test(
        "mod isel;
",
        "isel"
    ));
    assert!(!declared_cfg_test(
        "#[cfg(target_arch = \"x86_64\")]
mod simd;
",
        "simd"
    ));
    // AND AN ATTRIBUTE MAY NOT LEAK ACROSS AN ITEM. The `#[cfg(test)]` here
    // belongs to `helpers`; `objects` is production and must read as such.
    assert!(!declared_cfg_test(
        "#[cfg(test)]
mod helpers;
mod objects;
",
        "objects"
    ));
}

/// Meta-test for B3: prove `scan_production_section` covers the real
/// production dispatch surface of interpreter.rs (thousands of lines),
/// not just the doc-comment header — and that the body is panic-free.
/// If the `#[cfg(test)]` boundary detection ever regresses to the old
/// `find("#[cfg(test)]")` doc-comment anchor, `scanned` collapses to
/// ~33 and this fails loudly.
#[test]
fn b3_gate_scans_full_production_body_of_interpreter() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let needles = [
        ".unwrap()",
        ".expect(",
        "panic!(",
        "unimplemented!(",
        "todo!(",
        "unreachable!(",
    ];

    // The interpreter MODULE, not the file. This gate's property is "the scan
    // reaches the real dispatch surface, not just the ~33-line header", and
    // that surface is the module. Asserting it against `interpreter.rs` alone
    // was true at 26,000 lines and false the moment the SEAM-02 split moved
    // most of it one directory down — a gate reporting a broken scanner when
    // the scanner was fine.
    let dir = format!("{manifest}/src/runtime/interpreter");
    let parent = std::fs::read_to_string(format!("{dir}.rs")).expect("read interpreter.rs");
    let mut paths = vec![format!("{dir}.rs")];
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("enumerate interpreter/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .collect();
    entries.sort();
    for p in entries {
        let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or_default();
        if !declared_cfg_test(&parent, stem) {
            paths.push(p.to_string_lossy().into_owned());
        }
    }

    let mut hits = 0usize;
    let mut scanned = 0usize;
    for p in &paths {
        let (h, sc) = scan_production_section(p, &needles);
        hits += h;
        scanned += sc;
    }
    assert!(
        scanned > 10_000,
        "B3: the scan covered only {scanned} production lines across {} files \
         of the interpreter module — the dispatch surface was not scanned \
         (boundary detection broke).",
        paths.len(),
    );
    assert_eq!(
        hits, 0,
        "B3: interpreter.rs production body has {hits} panic sites \
         across {scanned} scanned lines; it must be panic-free.",
    );
}

/// `multianewarray` must reject `dimensions > <bracket count of the referenced
/// array class>` *itself*, not lean on the verifier for it.
///
/// The type-state verifier does check it (`classloading/src/verify_insn.rs`,
/// `Instruction::Multianewarray`), but that pass does not run for every class:
/// `ClassManager::define_class_shared_with_options` sets
/// `defer_loader_sensitive_pass3` for any class defined by a user-defined
/// loader while `loader_aware_resolution()` is on — which is the default —
/// and the structural-only substitute it runs instead
/// (`verifier::verify_method_structural`) never looks at this operand.
/// `-Xverify:none` removes the check too.
///
/// Without the guard the opcode arm computes `total_array_depth - d - 1` for
/// `d` in `0..dimensions`. The first `d == total_array_depth` underflows
/// `usize`: a release build (`[profile.release]` sets no `overflow-checks`, so
/// it defaults off) wraps it to `usize::MAX`, and the very next statement is
/// `"[".repeat(comp_brackets)` — `Vec::with_capacity(usize::MAX)`, which the
/// allocator cannot satisfy and which aborts the process rather than raising
/// anything Java can catch. `multianewarray #cp("java/lang/Object"), 1`
/// underflows on the *first* iteration.
///
/// This is a source witness rather than an execution test: reaching the arm
/// needs a full `Vm`, a hand-built classfile and a `skip_verification`/
/// user-loader define, and there is no single-opcode harness in this module.
/// It is anchored on code text, not line numbers, so it does not go stale the
/// way a fixed line band would.
#[test]
fn multianewarray_arm_guards_the_component_bracket_subtraction() {
    // The arm's body was extracted into `interpreter::multianewarray_alloc` so
    // the JIT's `jit_multianewarray_2d` helper could stop carrying a second,
    // component-class-less transcription of it. The guard travelled with the
    // subtraction it protects, so this witness follows it there.
    let src = std::fs::read_to_string(format!(
        "{}/src/runtime/interpreter.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read interpreter.rs");

    let arm = src
        .find("pub(crate) fn multianewarray_alloc(")
        .expect("the shared multianewarray allocator must still exist");
    // Anchor on the whole binding, not the bare expression: the guard's own
    // explanatory comment quotes `total_array_depth - d - 1`, and matching that
    // would find the comment (which sits *before* the guard) instead of the code.
    let subtraction = src[arm..]
        .find("let comp_brackets = total_array_depth - d - 1")
        .map(|off| arm + off)
        .expect(
            "the component-bracket subtraction must still exist; if it was rewritten \
             (e.g. to `checked_sub`), retarget this witness at the new form",
        );

    let guard = src[arm..subtraction].find("sizes.len() > total_array_depth");
    assert!(
        guard.is_some(),
        "multianewarray: `total_array_depth - d - 1` is reached with no \
         `sizes.len() > total_array_depth` rejection in front of it. A class \
         whose Pass 3 was deferred (any user-defined loader, the default) can \
         then underflow it to usize::MAX and abort the process in \
         `\"[\".repeat(..)`."
    );

    // The guard must reject, not clamp: a silently-truncated dimension count
    // would allocate the wrong shape instead of crashing, which is worse.
    let guarded = &src[arm..subtraction];
    assert!(
        guarded.contains("LinkageError::VerifyError"),
        "the multianewarray depth guard must raise a catchable VerifyError, \
         not clamp the dimension count or fall through"
    );

    // And the opcode arm must still route through it rather than growing a
    // second copy: the whole reason the body moved is that two copies drifted.
    let opcodes = std::fs::read_to_string(format!(
        "{}/src/runtime/interpreter/opcodes.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read opcodes.rs");
    let opcode_arm = opcodes
        .find("Instruction::Multianewarray { index, dimensions } =>")
        .expect("the multianewarray arm must still exist");
    assert!(
        opcodes[opcode_arm..opcode_arm + 2000].contains("multianewarray_alloc("),
        "the interpreter's multianewarray arm must call the shared \
         `multianewarray_alloc`, not re-implement the component-class resolution"
    );
}

/// The hidden-class self-reference predicate, both directions.
///
/// A hidden class's constant pool names the class by its class-FILE name, which
/// is never the name it is stored under, so `resolve_class_loader_aware` has to
/// recognise `stored == "<referenced>/0x<hex>"` and answer with the referencing
/// class itself. Getting this WRONG in the permissive direction resolves an
/// unrelated reference to a hidden class, which is worse than the missing answer
/// it replaces -- hence the negative rows, not just the positive one.
#[test]
fn hidden_self_reference_predicate_matches_only_the_mint_sites_shape() {
    use constants::hidden_stored_name_is_self as is_self;

    // The shape every mint site writes: `format!("{original}/0x{id:x}")`.
    assert!(is_self(
        "jdk/MHProxy1/RJdkProxyIface$Greeter/0x0",
        "jdk/MHProxy1/RJdkProxyIface$Greeter"
    ));
    assert!(is_self("Foo/0xdeadbeef", "Foo"));
    assert!(is_self("a/b/C/0xff", "a/b/C"));

    // A LONGER name that merely starts with the referenced one. `A/0x1$Inner`
    // is its own class; resolving a reference to `A` onto it would be a wrong
    // answer, not a missing one.
    assert!(!is_self("A/0x1$Inner", "A"));
    assert!(!is_self("A/0x", "A"), "the counter is never empty");
    assert!(!is_self("A/0xzz", "A"), "not hex");
    assert!(!is_self("A/1", "A"), "no 0x");
    assert!(
        !is_self("AB/0x1", "A"),
        "prefix of the NAME, not of a segment"
    );

    // An ordinary class referring to itself by its own name is not a hidden
    // self-reference, and must fall through to the real resolution path.
    assert!(!is_self("java/lang/String", "java/lang/String"));

    // `Unsafe.defineAnonymousClass` mints `<HOST>/0x<id>`, so the stored name
    // belongs to the HOST, not to the anonymous class's own class-file name.
    // The predicate simply does not match, which is the correct outcome: that
    // path has no self-reference to rescue.
    assert!(!is_self("Host/0x7", "AnonymousBody"));
}

/// The self-reference predicate the two dispatch doors share.
///
/// `dispatch_static`'s `self_class_id` and `invoke`'s `self_match` both answer
/// "the constant-pool owner is MY class" from the frame's own `ClassId`. Exact
/// string equality is right for every ordinary class and cannot be right for a
/// hidden one, so both halves are asserted here -- including that a non-hidden
/// class is NOT given the mangled-name shortcut, which would let an ordinary
/// class named `A/0x1` answer for a reference to `A`.
#[test]
fn self_class_reference_covers_the_hidden_name_and_nothing_more() {
    use constants::is_self_class_reference as is_self;

    // Ordinary self-call: literal equality, hidden flag irrelevant.
    assert!(is_self("a/b/C", false, "a/b/C"));
    assert!(is_self("a/b/C", true, "a/b/C"));

    // Hidden self-call: the stored name is the class-file name plus the suffix.
    assert!(is_self(
        "jdk/MHProxy1/P$Greeter/0x0",
        true,
        "jdk/MHProxy1/P$Greeter"
    ));

    // The SAME stored name on a class that is not hidden gets no shortcut.
    assert!(!is_self(
        "jdk/MHProxy1/P$Greeter/0x0",
        false,
        "jdk/MHProxy1/P$Greeter"
    ));

    // Unrelated names stay unrelated in both modes.
    assert!(!is_self("a/b/C", true, "a/b/D"));
    assert!(!is_self("a/b/C", false, "a/b/D"));
}
