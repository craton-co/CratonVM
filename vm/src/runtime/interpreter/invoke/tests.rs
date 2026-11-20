use super::*;

/// Malformed descriptors are rejected, never sliced out of bounds. An
/// unterminated class name ran the index one past the end and the slice
/// panicked; a non-ASCII tag byte could split a UTF-8 sequence.
#[test]
fn split_method_descriptor_ref_rejects_malformed_input_without_panicking() {
    for bad in [
        "(Ljava/lang/String",
        "(L",
        "(IL",
        "([Ljava/lang/Object",
        "(\u{e9})V",
        "(I\u{e9}",
    ] {
        assert_eq!(
            split_method_descriptor_ref(bad),
            (Vec::new(), ""),
            "descriptor {bad:?}"
        );
    }
    // Well-formed input is unchanged.
    assert_eq!(
        split_method_descriptor_ref("(I[JLjava/lang/String;)V"),
        (vec!["I", "[J", "Ljava/lang/String;"], "V")
    );
    assert_eq!(split_method_descriptor_ref("()V"), (Vec::new(), "V"));
}

/// Round 11 wave 5 (lane rt): the return token follows the `)` that closes
/// the parameter list. A class name may contain `)`, and the first-`)` scan
/// answered `B;)Z` for the first descriptor below.
#[test]
fn descriptor_return_ref_is_not_fooled_by_a_paren_in_a_class_name() {
    assert_eq!(descriptor_return_ref("(LA)B;)Z"), "Z");
    assert_eq!(descriptor_return_ref("()LA)I;"), "LA)I;");
    assert_eq!(
        descriptor_return_ref("(I[J)Ljava/lang/String;"),
        "Ljava/lang/String;"
    );
    assert_eq!(descriptor_return_ref("()V"), "V");
    assert_eq!(descriptor_return_ref("(I"), "");
    assert_eq!(descriptor_return_ref(""), "");
}

// -----------------------------------------------------------------------
// §4 census — `try_stackless_invoke`'s exotic arms declare themselves
// uncounted
// (docs/feature-designs/real-jdk-mode.md)
// -----------------------------------------------------------------------

fn census_probe_native(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(None)
}

/// The signature-polymorphic rows must stay on the **erased** descriptor.
///
/// `MethodHandle.invoke*` and `DowncallHandle.invoke*` are registered under
/// `([Ljava/lang/Object;)Ljava/lang/Object;` while a real call site carries
/// its concrete signature — that mismatch is the whole reason those arms
/// exist, and it is also the reason the census row that loses the call is
/// the erased one. "Tidying" these rows to concrete descriptors would leave
/// the table resolving nothing and the marks silently absent, which reads
/// identically to a fixed instrument.
///
/// Duplicate-free for the same reason the JIT-side list is: a duplicate
/// would make the count assertions below pass over one triple twice.
#[test]
fn the_signature_polymorphic_rows_use_the_erased_descriptor() {
    const ERASED: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
    for (class, method, descriptor) in UNCOUNTED_STACKLESS_NATIVES {
        if matches!(
            class,
            "java/lang/invoke/MethodHandle" | "java/lang/foreign/DowncallHandle"
        ) && matches!(method, "invoke" | "invokeExact" | "invokeBasic")
        {
            assert_eq!(
                descriptor, ERASED,
                "{class}.{method} is dispatched through the erased bridge; a \
                 concrete descriptor here resolves nothing and marks nothing"
            );
        }
    }

    let mut seen: Vec<(&str, &str, &str)> = Vec::new();
    for row in UNCOUNTED_STACKLESS_NATIVES {
        assert!(
            !seen.contains(&row),
            "UNCOUNTED_STACKLESS_NATIVES lists {row:?} twice"
        );
        seen.push(row);
    }
}

/// Marking must turn exactly the exotic-arm rows into floors, leave a
/// counted row exact, and leave the tallies alone.
///
/// The counted control here is deliberately the shape this function's
/// ordinary path takes: a native reached through `resolve_step1_native`,
/// which holds the id and calls `record_invocation`. Those rows are exact
/// and must keep saying so — the point of the bit is to separate them from
/// the eleven arms that are not, not to blanket the census in doubt.
#[test]
fn marking_the_stackless_exotic_arms_turns_their_rows_into_floors() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    { let r = &mut registry; 
        for (class, method, descriptor) in UNCOUNTED_STACKLESS_NATIVES {
            r.register(class, method, descriptor, census_probe_native, cratonvm_native_api::NativeKind::Bridge);
        }
        r.register(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            census_probe_native, cratonvm_native_api::NativeKind::Bridge);
     }

    let counted = registry
        .resolve_id(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
        )
        .expect("control registered");
    let method_invoke = registry
        .resolve_id(
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .expect("registered");

    assert_eq!(registry.slots_with_incomplete_invocations(), 0);
    registry.record_invocation(counted);

    let marked = mark_stackless_exotic_natives_incomplete_in(&registry);
    assert_eq!(
        marked,
        UNCOUNTED_STACKLESS_NATIVES.len(),
        "every listed triple was registered above, so every one must resolve"
    );

    assert_eq!(
        registry.invocations_complete(method_invoke),
        Some(false),
        "reflective Method.invoke is dispatched from an arm that holds no \
         NativeMethodId, so its zero proves nothing"
    );
    assert_eq!(
        registry.invocations_complete(counted),
        Some(true),
        "the ordinary step-1 path counts, and must keep claiming to"
    );
    assert_eq!(
        registry.invocations_of_id(counted),
        Some(1),
        "marking other slots must not disturb a counted tally"
    );
    assert_eq!(
        registry.slots_with_incomplete_invocations(),
        UNCOUNTED_STACKLESS_NATIVES.len()
    );

    // Idempotent: the marker is called from five dispatch points and the
    // latch is an optimisation, not a correctness requirement.
    assert_eq!(
        mark_stackless_exotic_natives_incomplete_in(&registry),
        UNCOUNTED_STACKLESS_NATIVES.len()
    );
    assert_eq!(
        registry.slots_with_incomplete_invocations(),
        UNCOUNTED_STACKLESS_NATIVES.len()
    );
}

/// A registry without the Panama / `MethodHandle` bridges must be marked
/// with nothing rather than panic. This runs on the interpreter's hottest
/// function; an unregistered triple is an ordinary state, not an error.
#[test]
fn marking_an_empty_registry_marks_nothing_and_does_not_panic() {
    let registry = cratonvm_native_api::NativeMethodRegistry::new();
    assert_eq!(mark_stackless_exotic_natives_incomplete_in(&registry), 0);
    assert_eq!(registry.slots_with_incomplete_invocations(), 0);
}
