// The boxing-identity tests appended at the end of this module need the
// parent's items (`box_value`, `box_value_canonical`, `Value`,
// `ObjectRef`) and a mock receiver; the ProtectionDomain tests above
// predate them and spell every path in full, so nothing was imported.
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Real JDK 21–25 `java.security.ProtectionDomain`, instance fields in
/// DECLARATION order — not the constructor's argument order, which is
/// `(CodeSource, PermissionCollection, ClassLoader, Principal[])` and is
/// how the model came to be wrong. Spelled out here so this is a claim
/// about the IMAGE that a JDK upgrade can falsify.
const REAL_PD_PREFIX: [(&str, &str); 4] = [
    ("codesource", "Ljava/security/CodeSource;"),
    ("classloader", "Ljava/lang/ClassLoader;"),
    ("principals", "[Ljava/security/Principal;"),
    ("permissions", "Ljava/security/PermissionCollection;"),
];

#[test]
fn the_model_is_the_real_declaration_order_not_the_constructor_order() {
    let model =
        cratonvm_classloading::synthetic_stub_field_model("java/security/ProtectionDomain");
    let got: Vec<(String, String)> = model
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| (f.name.to_string(), f.descriptor.to_string()))
        .collect();
    let want: Vec<(String, String)> = REAL_PD_PREFIX
        .iter()
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .collect();
    assert_eq!(
        got, want,
        "the fabricated ProtectionDomain model must be the real \
         DECLARATION order (javap -p --module java.base \
         java.security.ProtectionDomain). The constructor signature is a \
         different order and is not the layout."
    );
}

/// `populate_protection_domain_fields` must not address this class by raw
/// slot index. It used to write 0..3 in the constructor order and then the
/// same four by name, relying on the by-name pass running last to undo the
/// damage — an ordering dependency invisible at the call site.
#[test]
fn the_populator_writes_no_raw_slot_indices() {
    // The `\r` strip is load-bearing, not tidiness. `include_str!` embeds
    // the file's RAW bytes, and this repository is checked out with CRLF on
    // Windows (`core.autocrlf=true`). A `}` line is then `\r\n}\r\n`, so the
    // `"\n}\n"` terminator below matches NOTHING: the `.expect` fired and
    // this test failed with "function body must terminate" — a false
    // failure that says nothing about the invariant it polices, on every
    // Windows checkout. Normalise once so the gate asks the same question
    // on CRLF and LF. Same fix, same reason, as `jit::ir_lower`'s
    // `declared_op_variants` and `vm::runtime::env_cache`'s flags scan.
    let src = concat!(include_str!("../lang_class/mod.rs"), "\n", include_str!("../lang_class/dbg_bb_enabled.rs"), "\n", include_str!("../lang_class/wf7_synthesise_entry_class_if.rs"), "\n", include_str!("../lang_class/descriptor_to_class_mirror_via.rs"), "\n", include_str!("../lang_class/native_field_set.rs"), "\n", include_str!("../lang_class/executable_access_refusal_chain.rs"), "\n", include_str!("../lang_class/forget_lang_class_keys.rs"), "\n", include_str!("../lang_class/gc_update_annotation_proxy_refs.rs"), "\n", include_str!("../lang_class/native_class_is_annotation_present.rs"), "\n", include_str!("../lang_class/classloader_packages_synced.rs"), "\n", include_str!("../lang_class/declaring_class_loader_aware.rs")).replace("\r\n", "\n");
    let start = src
        .find("pub fn populate_protection_domain_fields")
        .expect("populate_protection_domain_fields must exist");
    let body = &src[start..];
    let end = body.find("\n}\n").expect("function body must terminate");
    let body = &body[..end];
    assert!(
        body.contains("set_field_by_name(pd, \"codesource\""),
        "the by-name writes must still be there, or this test asserts nothing"
    );
    for slot in 0..4 {
        let needle = format!("set_field(pd, {slot},");
        assert!(
            !body.contains(&needle),
            "`{needle}` addresses ProtectionDomain by raw index; resolve on \
             the receiver by name instead"
        );
    }
}

// -----------------------------------------------------------------------
// `box_value` vs `box_value_canonical` — the split, and BOTH directions.
//
// Measured on Microsoft OpenJDK 25.0.3+9 (`scratchpad/f11/BoxCallers.java`
// and `BoxCallers2.java`, which print rather than assert): `Field.get`,
// `Method.invoke`, MethodHandle return adaptation, every `VarHandle.get`
// shape, `SerializedLambda`'s captured args and a proxy's `args[]` all
// hand back the CANONICAL box, while `Array.get` hands back a FRESH one on
// both VMs. Every "fresh" row above is still `.equals`-equal to the
// canonical instance, so these tests assert on `ObjectRef` identity and
// never on the payload alone — an equality-shaped assertion cannot see
// this defect in either direction.
//
// The caches are process-global and keyed by `vm_identity()`, whose mock
// default is `0` and is therefore shared with every other test in this
// suite; entries dangle once a mock heap drops, so each test below claims
// its own identity rather than handing a stale ref to an unrelated test.
// -----------------------------------------------------------------------

fn boxed_ref(v: Value) -> ObjectRef {
    match v {
        Value::Object(Some(o)) => o,
        other => panic!("expected a boxed object, got {other:?}"),
    }
}

#[test]
fn box_value_canonical_returns_the_cached_instance_for_the_six_cached_types() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5f11);
    for (desc, v) in [
        ("I", Value::Int(7)),
        ("J", Value::Long(5)),
        ("Z", Value::Int(1)),
        ("B", Value::Int(3)),
        ("S", Value::Int(9)),
        ("C", Value::Int('a' as i32)),
    ] {
        let a = boxed_ref(box_value_canonical(&mut ctx, v, desc));
        let b = boxed_ref(box_value_canonical(&mut ctx, v, desc));
        assert_eq!(
            a, b,
            "box_value_canonical({desc}, {v:?}) must return THE canonical \
             instance — this is what makes `Field.get`/`Method.invoke` \
             agree with `X.valueOf` as they do on HotSpot"
        );
        assert_eq!(
            ctx.get_field(a, 0),
            v,
            "a canonical box that carries the wrong value passes every \
             identity row above"
        );
    }

    // Identity with the VM's own `valueOf` native, not merely with itself.
    // A private-but-self-consistent cache would satisfy the loop above and
    // still fail HotSpot's `f.get(o) == Character.valueOf('a')`.
    let via_helper = boxed_ref(box_value_canonical(&mut ctx, Value::Int('a' as i32), "C"));
    let via_native = boxed_ref(
        crate::lang_math::native_character_value_of(&mut ctx, &[Value::Int('a' as i32)])
            .unwrap()
            .unwrap(),
    );
    assert_eq!(
        via_helper, via_native,
        "the cached sibling must delegate to the registered `valueOf` \
         native, not mint a second canonical instance"
    );
}

#[test]
fn box_value_must_stay_fresh_even_for_values_that_are_in_every_cache() {
    // NEGATIVE CONTROL, and the reason this is a split rather than a
    // one-line fix. HotSpot's `Reflection::array_get` boxes with
    // `java_lang_boxing_object::create`, which never consults a cache:
    // `Array.get(new int[]{7}, 0) == Integer.valueOf(7)` is FALSE, and
    // `Array.get` twice is not even identical to itself (both measured).
    // A lane that "unifies" the pair by routing this half through the
    // caches breaks that, and nothing equality-shaped would notice.
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5f12);
    for (desc, v) in [
        ("I", Value::Int(7)),
        ("J", Value::Long(5)),
        ("Z", Value::Int(1)),
        ("B", Value::Int(3)),
        ("S", Value::Int(9)),
        ("C", Value::Int('a' as i32)),
    ] {
        let a = boxed_ref(box_value(&mut ctx, v, desc));
        let b = boxed_ref(box_value(&mut ctx, v, desc));
        assert_ne!(
            a, b,
            "box_value({desc}, {v:?}) is the FRESH half of the split and \
             must allocate — `Array.get`'s contract depends on it"
        );
        assert_eq!(ctx.get_field(a, 0), v);
    }
    assert_eq!(
        box_value(&mut ctx, Value::Int(0), "V"),
        Value::Object(None),
        "a void return has no value and boxes to null"
    );
}

#[test]
fn box_value_canonical_caches_neither_float_double_nor_out_of_bound_values() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5f13);

    // `Float`/`Double` cache NOTHING. Measured: `Float.valueOf(0f) ==
    // Float.valueOf(0f)` is false on HotSpot 25 while `.equals` is true,
    // and `Field.get`/`Method.invoke` of a `float` come back fresh even
    // though every integral sibling comes back canonical. Completing the
    // family to eight here is a regression, not a completion.
    for (desc, v) in [
        ("F", Value::Float(0.0)),
        ("F", Value::Float(1.0)),
        ("D", Value::Double(0.0)),
        ("D", Value::Double(1.0)),
    ] {
        let a = boxed_ref(box_value_canonical(&mut ctx, v, desc));
        let b = boxed_ref(box_value_canonical(&mut ctx, v, desc));
        assert_ne!(
            a, b,
            "box_value_canonical({desc}, {v:?}) must NOT be canonical — \
             there is no FloatCache and no DoubleCache"
        );
    }

    // And each cached type keeps its own uncached arm, with its own bound.
    // `Byte` is absent from this list on purpose: `ByteCache` covers all
    // 256 values and has no fresh arm at all, so no `Byte` row belongs
    // here — the four bounds in this family are genuinely different.
    for (desc, v) in [
        ("I", Value::Int(1000)),
        ("J", Value::Long(1000)),
        ("S", Value::Int(1000)),
        ("C", Value::Int(200)),
    ] {
        let a = boxed_ref(box_value_canonical(&mut ctx, v, desc));
        let b = boxed_ref(box_value_canonical(&mut ctx, v, desc));
        assert_ne!(
            a, b,
            "{desc} {v:?} is outside its cache bound; HotSpot's \
             `Field.get` is fresh there too"
        );
    }
}

#[test]
fn a_mismatched_value_variant_falls_back_instead_of_being_defaulted_to_zero() {
    // `native_long_value_of` reads `Some(Value::Long(v))` and DEFAULTS TO
    // 0 for every other variant. A raw `long` field slot can legitimately
    // present as a compact `Value::Int` — `native_wrapper_long_value`
    // exists to widen exactly that shape — so a canonical route that
    // ignored the variant would turn an identity question into a wrong
    // ANSWER: the cached `Long.valueOf(0)` returned for a field holding 5.
    // That is why the descriptor arm matches on the variant too.
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5f14);
    let obj = boxed_ref(box_value_canonical(&mut ctx, Value::Int(5), "J"));
    assert_eq!(
        ctx.get_field(obj, 0),
        Value::Int(5),
        "a (\"J\", Value::Int) pair must fall back to the verbatim \
         allocating path — never be defaulted to 0 by the cached native"
    );
    let again = boxed_ref(box_value_canonical(&mut ctx, Value::Int(5), "J"));
    assert_ne!(obj, again, "that fallback is `box_value`, which allocates");
}

#[test]
fn the_reflection_call_sites_still_take_the_cached_sibling() {
    // A source witness, because every behavioural test above exercises the
    // HELPERS and all of them would still pass if a CALL SITE were put
    // back to `box_value` — which is precisely the defect the split
    // exists to fix. The needles are assembled at runtime: spelled out as
    // literals they would match this test's own source text and assert
    // nothing (the file being searched is this file).
    //
    // The `\r` strip is load-bearing on a CRLF checkout; same reason as
    // `the_populator_writes_no_raw_slot_indices` above.
    let src = concat!(include_str!("../lang_class/mod.rs"), "\n", include_str!("../lang_class/dbg_bb_enabled.rs"), "\n", include_str!("../lang_class/wf7_synthesise_entry_class_if.rs"), "\n", include_str!("../lang_class/descriptor_to_class_mirror_via.rs"), "\n", include_str!("../lang_class/native_field_set.rs"), "\n", include_str!("../lang_class/executable_access_refusal_chain.rs"), "\n", include_str!("../lang_class/forget_lang_class_keys.rs"), "\n", include_str!("../lang_class/gc_update_annotation_proxy_refs.rs"), "\n", include_str!("../lang_class/native_class_is_annotation_present.rs"), "\n", include_str!("../lang_class/classloader_packages_synced.rs"), "\n", include_str!("../lang_class/declaring_class_loader_aware.rs")).replace("\r\n", "\n");
    let canon = format!("box_value_{}", "canonical");
    for (site, anchor) in [
        (
            "Field.get",
            format!("let result = {canon}(ctx, boxed_value, &descriptor);"),
        ),
        (
            "SerializedLambda capturedArgs",
            format!("let boxed = {canon}(ctx, raw, &tc.to_string());"),
        ),
        (
            "Method.invoke return",
            format!("let boxed = {canon}(ctx, val, &ret_desc);"),
        ),
    ] {
        assert!(
            src.contains(&anchor),
            "{site} no longer boxes through the cached sibling — on \
             HotSpot 25 that path returns the canonical instance \
             (measured); `{anchor}` is gone"
        );
    }
}
